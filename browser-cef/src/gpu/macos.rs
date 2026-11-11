use std::cell::Cell;
use std::ptr::NonNull;

use cef::{AcceleratedPaintInfo, ColorType, PaintElementType, Rect};
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_surface::{
    IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use waterui_graphics::gpu::FrameOutput;
use waterui_graphics::gpu::interop::metal::import_texture;
use wgpu_external_frame::io_surface::{PackedFormat, PackedIoSurfaceFrame};

use super::pool::PoolSlot;
use super::publisher::{FramePublisher, FrameSlot, SLOT_USAGE, SlotAllocator, submit_copy};
use crate::{AcceleratedFrameSink, CefPopupRect};

// # Safety
//
// The `unsafe` in this file rests on one fact stated by
// [`AcceleratedFrameSink::import`]: CEF hands the sink a frame whose
// `IOSurface` is valid for the duration of that call and returns it to its pool
// immediately afterwards. Everything here therefore either runs inside that
// call, or operates on the surface this module retained during it. Sites that
// depend on something else say so.

/// `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCVPixelFormatType_32RGBA`.
const PIXEL_FORMAT_RGBA: u32 = u32::from_be_bytes(*b"RGBA");

struct CefIoSurface {
    /// The retained surface, at the allocated extent it must be imported as.
    surface: PackedIoSurfaceFrame,
    /// The part of it that actually holds the page.
    ///
    /// Chromium may allocate the shared image with alignment padding, so this
    /// is not always the coded size. Presenting the coded texture edge to edge
    /// stretched the page and sampled the padding gutter.
    visible_width: u32,
    visible_height: u32,
}

impl CefIoSurface {
    /// # Safety
    ///
    /// `frame.shared_texture_io_surface` must be the surface of a live
    /// accelerated paint callback, so that retaining it happens while it is
    /// still valid.
    unsafe fn retain(frame: &AcceleratedPaintInfo) -> Self {
        let size = &frame.extra.coded_size;
        let width = u32::try_from(size.width).expect("CEF IOSurface width must be positive");
        let height = u32::try_from(size.height).expect("CEF IOSurface height must be positive");
        let visible = &frame.extra.visible_rect;
        // Clamped to the allocation: a visible rect larger than the coded size
        // would be a CEF bug, and copying past the end of the surface is not the
        // way to find out.
        let visible_width = u32::try_from(visible.width).unwrap_or(width).min(width);
        let visible_height = u32::try_from(visible.height).unwrap_or(height).min(height);
        let pointer = NonNull::new(frame.shared_texture_io_surface)
            .expect("CEF accelerated paint returned a null IOSurface");
        let format = if frame.format == ColorType::BGRA_8888 {
            PackedFormat::Bgra8
        } else if frame.format == ColorType::RGBA_8888 {
            PackedFormat::Rgba8
        } else {
            panic!("CEF returned unsupported macOS accelerated color format")
        };
        // SAFETY: the caller contract makes `pointer` a live `IOSurface`;
        // retaining it here is what keeps it valid after CEF reclaims the
        // frame. The frame reads the allocated extent from the surface itself,
        // which is the `coded_size` CEF reported for it.
        let surface = unsafe { PackedIoSurfaceFrame::retain(pointer, format) };
        Self {
            surface,
            visible_width,
            visible_height,
        }
    }
}

/// A pool slot backed by an `IOSurface` of its own, so the frame it carries
/// stays promotable to a system compositor plane.
struct IoSurfaceSlot {
    surface: CFRetained<IOSurfaceRef>,
    texture: wgpu::Texture,
    extent: (u32, u32),
    format: wgpu::TextureFormat,
}

impl PoolSlot for IoSurfaceSlot {
    fn extent(&self) -> (u32, u32) {
        self.extent
    }

    fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// A compositor plane keeps displaying the surface after the engine has
    /// released the frame, and `IOSurfaceIsInUse` is how the system says so.
    fn in_system_use(&self) -> bool {
        self.surface.is_in_use()
    }
}

impl FrameSlot for IoSurfaceSlot {
    fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

struct IoSurfaceAllocator {
    device: wgpu::Device,
}

impl SlotAllocator for IoSurfaceAllocator {
    type Slot = IoSurfaceSlot;

    fn allocate(&self, extent: (u32, u32), format: wgpu::TextureFormat) -> IoSurfaceSlot {
        let (pixel_format, metal_format) = match format {
            wgpu::TextureFormat::Bgra8Unorm => (PIXEL_FORMAT_BGRA, MTLPixelFormat::BGRA8Unorm),
            wgpu::TextureFormat::Rgba8Unorm => (PIXEL_FORMAT_RGBA, MTLPixelFormat::RGBA8Unorm),
            format => panic!("CEF produced {format:?}, which has no IOSurface slot"),
        };
        let surface = create_surface(extent, pixel_format);
        let raw = autoreleasepool(|_| {
            // SAFETY: the handle is only borrowed to read the raw `MTLDevice`, and
            // it is not kept past this closure.
            let hal_device = unsafe {
                self.device
                    .as_hal::<wgpu::hal::api::Metal>()
                    .expect("IOSurface slots require a Metal device")
            };
            let raw_device = hal_device.raw_device();
            let descriptor = MTLTextureDescriptor::new();
            // SAFETY: plain setters on a descriptor this scope just allocated and
            // owns.
            unsafe {
                descriptor.setWidth(usize::try_from(extent.0).expect("slot width fits usize"));
                descriptor.setHeight(usize::try_from(extent.1).expect("slot height fits usize"));
            }
            descriptor.setTextureType(MTLTextureType::Type2D);
            descriptor.setPixelFormat(metal_format);
            descriptor.setUsage(MTLTextureUsage::ShaderRead);
            // A texture aliasing an `IOSurface` is `Shared` where memory is
            // unified and `Managed` on a discrete GPU.
            descriptor.setStorageMode(if raw_device.hasUnifiedMemory() {
                MTLStorageMode::Shared
            } else {
                MTLStorageMode::Managed
            });
            raw_device
                .newTextureWithDescriptor_iosurface_plane(&descriptor, &surface, 0)
                .unwrap_or_else(|| {
                    panic!("Metal rejected an IOSurface slot of {extent:?} as {format:?}")
                })
        });
        // SAFETY: `raw` was just created on the `MTLDevice` this `wgpu` device
        // wraps, with the pixel format `format` names. Its `ShaderRead` usage
        // is all `TEXTURE_BINDING` needs, and the blits `COPY_DST` and
        // `COPY_SRC` stand for need no `MTLTextureUsage`. The slot owns the
        // texture, and a frame sampling it holds the pool's claim on the slot,
        // so it is not written while a frame that can be in flight shows it.
        let texture = unsafe { import_texture(&self.device, raw, format, SLOT_USAGE) };
        IoSurfaceSlot {
            surface,
            texture,
            extent,
            format,
        }
    }
}

/// Allocates a non-planar `IOSurface` of 4-byte pixels.
fn create_surface((width, height): (u32, u32), pixel_format: u32) -> CFRetained<IOSurfaceRef> {
    let number = |value: u32| CFNumber::new_i64(i64::from(value));
    let (width, height, bytes_per_element) = (number(width), number(height), number(4));
    let pixel_format = CFNumber::new_i64(i64::from(pixel_format));
    // SAFETY: IOSurface defines these keys as immutable `CFString`s that live for
    // the whole process.
    let keys = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfacePixelFormat,
        ]
    };
    let properties = CFDictionary::<CFString, CFNumber>::from_slices(
        &keys,
        &[&width, &height, &bytes_per_element, &pixel_format],
    );
    // SAFETY: `properties` is a live dictionary of the keys and value types
    // `IOSurfaceCreate` documents.
    unsafe { IOSurfaceRef::new(properties.as_opaque()) }.expect("IOSurfaceCreate failed")
}

pub(super) struct MacFrameSink {
    device: wgpu::Device,
    queue: wgpu::Queue,
    publisher: FramePublisher<IoSurfaceAllocator>,
    imported_size: Cell<(u32, u32)>,
}

impl AcceleratedFrameSink for MacFrameSink {
    fn import(
        &self,
        element: PaintElementType,
        _dirty_rects: &[Rect],
        frame: &AcceleratedPaintInfo,
    ) {
        // SAFETY: `import` is the accelerated paint callback, so `frame` names a
        // surface that is valid for exactly this call.
        let surface = unsafe { CefIoSurface::retain(frame) };
        let size = (surface.surface.width(), surface.surface.height());
        if self.imported_size.replace(size) != size {
            tracing::debug!(
                width = size.0,
                height = size.1,
                format = ?surface.surface.format(),
                "Importing a resized accelerated CEF IOSurface"
            );
        }
        let source = surface.surface.import(&self.device);
        let visible = (surface.visible_width, surface.visible_height);
        self.publisher.paint(
            element,
            visible,
            surface.surface.format().texture_format(),
            |destination| submit_copy(&self.device, &self.queue, &source, destination, visible),
        );
    }

    fn set_popup_rect(&self, rect: Option<CefPopupRect>) {
        self.publisher.set_popup_rect(rect);
    }
}

pub(super) fn make_frame_sink(output: FrameOutput) -> MacFrameSink {
    assert_eq!(
        output.shared_device().adapter.get_info().backend,
        wgpu::Backend::Metal,
        "CEF IOSurface composition requires WaterUI's Metal backend"
    );
    tracing::debug!("Installed the accelerated CEF frame sink");
    let device = output.device().clone();
    let queue = output.queue().clone();
    MacFrameSink {
        publisher: FramePublisher::new(
            output,
            IoSurfaceAllocator {
                device: device.clone(),
            },
        ),
        device,
        queue,
        imported_size: Cell::new((0, 0)),
    }
}
