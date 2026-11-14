//! Publishing CEF's frames to the host.
//!
//! CEF hands a shared texture to `OnAcceleratedPaint` and takes it back when
//! the callback returns, so each paint is copied exactly once, into a texture
//! of a [`SlotPool`] the host then samples in place. That texture is the
//! presented frame as is: nothing draws it a second time.
//!
//! A popup is a second shared texture CEF paints at a rectangle of the view.
//! While one is shown the latest view copy stays claimed, and every presented
//! frame is a fresh slot holding that copy with the latest popup copy over it,
//! so a popup-only paint recomposes without a new view paint.

use std::cell::RefCell;

use cef::PaintElementType;
use num_traits::ToPrimitive as _;
use waterui_graphics::gpu::FrameOutput;
use waterui_graphics::gpu::interop::{ExternalFrame, FrameColor, RgbAlpha};

use super::pool::{Claim, PoolSlot, SlotPool};
use crate::CefPopupRect;

/// A pool slot the publisher can copy into, sample from and present.
pub(super) trait FrameSlot: PoolSlot {
    /// The slot's texture: `COPY_DST | COPY_SRC | TEXTURE_BINDING`.
    fn texture(&self) -> &wgpu::Texture;
}

/// How a platform allocates the slots of its pool.
pub(super) trait SlotAllocator {
    type Slot: FrameSlot;

    /// A new slot of `extent` and `format`.
    fn allocate(&self, extent: (u32, u32), format: wgpu::TextureFormat) -> Self::Slot;
}

/// The usage of every texture CEF pixels are copied into or composed from.
pub(super) const SLOT_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::COPY_DST
    .union(wgpu::TextureUsages::COPY_SRC)
    .union(wgpu::TextureUsages::TEXTURE_BINDING);

/// A plain `wgpu` texture slot, for the platforms whose frames are not
/// compositor-promotable.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) struct PlainSlot {
    texture: wgpu::Texture,
    extent: (u32, u32),
    format: wgpu::TextureFormat,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl PoolSlot for PlainSlot {
    fn extent(&self) -> (u32, u32) {
        self.extent
    }

    fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    fn in_system_use(&self) -> bool {
        false
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl FrameSlot for PlainSlot {
    fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

/// Allocates [`PlainSlot`]s on the host's device.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) struct PlainAllocator {
    pub(super) device: wgpu::Device,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl SlotAllocator for PlainAllocator {
    type Slot = PlainSlot;

    fn allocate(&self, extent: (u32, u32), format: wgpu::TextureFormat) -> PlainSlot {
        PlainSlot {
            texture: create_texture(&self.device, "waterui_cef_frame_slot", extent, format),
            extent,
            format,
        }
    }
}

fn create_texture(
    device: &wgpu::Device,
    label: &'static str,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: SLOT_USAGE,
        view_formats: &[],
    })
}

/// Copies `extent` of `source` into `destination` and submits the copy.
///
/// There is deliberately no wait: this runs inside CEF's `OnAcceleratedPaint`,
/// dispatched from the message pump on the `WaterUI` main thread, so blocking on
/// a GPU fence would stall the whole UI loop once per browser frame. Work on
/// one queue completes in order, so whatever samples `destination` later is
/// already ordered after this copy, and wgpu keeps `source` alive until the
/// submission that references it has finished.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) fn submit_copy(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    source: &wgpu::Texture,
    destination: &wgpu::Texture,
    extent: (u32, u32),
) {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("waterui_cef_frame_copy"),
    });
    encoder.copy_texture_to_texture(
        source.as_image_copy(),
        destination.as_image_copy(),
        extent3d(extent),
    );
    queue.submit([encoder.finish()]);
}

const fn extent3d((width, height): (u32, u32)) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

/// The latest view paint, held in a pool slot.
struct ViewCopy {
    texture: wgpu::Texture,
    extent: (u32, u32),
    format: wgpu::TextureFormat,
    claim: Claim,
}

/// The latest popup paint.
struct PopupCopy {
    texture: wgpu::Texture,
    extent: (u32, u32),
    format: wgpu::TextureFormat,
}

struct Scene<S> {
    pool: SlotPool<S>,
    view: Option<ViewCopy>,
    popup: Option<PopupCopy>,
    popup_rect: Option<CefPopupRect>,
}

/// The part of a popup copy that lands inside a view copy, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopupRegion {
    /// Where the region starts in the popup copy.
    source: (u32, u32),
    /// Where the region starts in the view copy.
    destination: (u32, u32),
    extent: (u32, u32),
}

/// Where `popup`, a copy of `rect`'s widget painted at `scale` pixels per
/// view unit, lands in a view copy of `view` pixels, clipped to it; `None`
/// when it lies entirely outside.
fn clip_popup(
    rect: CefPopupRect,
    scale: f64,
    popup: (u32, u32),
    view: (u32, u32),
) -> Option<PopupRegion> {
    let origin = |axis: i32| {
        (f64::from(axis) * scale)
            .round()
            .to_i64()
            .expect("CEF popup origin exceeds i64")
    };
    let axis = |origin: i64, popup: u32, view: u32| {
        let destination = origin.max(0);
        let source = destination - origin;
        let extent = (i64::from(popup) - source).min(i64::from(view) - destination);
        (extent > 0).then(|| {
            (
                u32::try_from(source).expect("clipped popup source fits u32"),
                u32::try_from(destination).expect("clipped popup destination fits u32"),
                u32::try_from(extent).expect("clipped popup extent fits u32"),
            )
        })
    };
    let (source_x, destination_x, width) = axis(origin(rect.x), popup.0, view.0)?;
    let (source_y, destination_y, height) = axis(origin(rect.y), popup.1, view.1)?;
    Some(PopupRegion {
        source: (source_x, source_y),
        destination: (destination_x, destination_y),
        extent: (width, height),
    })
}

/// Copies CEF's paints into pooled textures and presents them on one
/// [`FrameOutput`].
pub(super) struct FramePublisher<A: SlotAllocator> {
    output: FrameOutput,
    allocator: A,
    scene: RefCell<Scene<A::Slot>>,
}

impl<A: SlotAllocator> FramePublisher<A> {
    pub(super) fn new(output: FrameOutput, allocator: A) -> Self {
        Self {
            output,
            allocator,
            scene: RefCell::new(Scene {
                pool: SlotPool::default(),
                view: None,
                popup: None,
                popup_rect: None,
            }),
        }
    }

    /// Takes one paint of `element`: `copy` fills the texture it is given
    /// with the paint's `extent` of pixels, submitting the copy itself.
    ///
    /// # Panics
    ///
    /// Panics for a paint element CEF does not define a surface for.
    pub(super) fn paint(
        &self,
        element: PaintElementType,
        extent: (u32, u32),
        format: wgpu::TextureFormat,
        copy: impl FnOnce(&wgpu::Texture),
    ) {
        match element {
            PaintElementType::VIEW => self.paint_view(extent, format, copy),
            PaintElementType::POPUP => self.paint_popup(extent, format, copy),
            element => panic!("CEF returned unsupported paint element {element:?}"),
        }
    }

    /// Updates the popup widget rectangle, in view units; `None` when the
    /// popup is hidden.
    pub(super) fn set_popup_rect(&self, rect: Option<CefPopupRect>) {
        let mut scene = self.scene.borrow_mut();
        let was_shown = scene.popup_rect.is_some();
        scene.popup_rect = rect;
        if rect.is_none() {
            scene.popup = None;
        }
        // A rectangle with no paint behind it changes nothing visible yet.
        if scene.popup.is_some() || (was_shown && rect.is_none()) {
            self.present(&mut scene);
        }
    }

    fn paint_view(
        &self,
        extent: (u32, u32),
        format: wgpu::TextureFormat,
        copy: impl FnOnce(&wgpu::Texture),
    ) {
        let mut scene = self.scene.borrow_mut();
        let (slot, claim) = scene
            .pool
            .acquire(extent, format, || self.allocator.allocate(extent, format));
        let texture = slot.texture().clone();
        copy(&texture);
        scene.view = Some(ViewCopy {
            texture,
            extent,
            format,
            claim,
        });
        self.present(&mut scene);
    }

    fn paint_popup(
        &self,
        extent: (u32, u32),
        format: wgpu::TextureFormat,
        copy: impl FnOnce(&wgpu::Texture),
    ) {
        let mut scene = self.scene.borrow_mut();
        let reusable = scene
            .popup
            .take_if(|popup| popup.extent == extent && popup.format == format);
        let popup = reusable.unwrap_or_else(|| PopupCopy {
            texture: create_texture(self.output.device(), "waterui_cef_popup", extent, format),
            extent,
            format,
        });
        copy(&popup.texture);
        scene.popup = Some(popup);
        self.present(&mut scene);
    }

    /// Presents the scene: the view copy as is, or composed with the popup.
    fn present(&self, scene: &mut Scene<A::Slot>) {
        let Scene {
            pool,
            view,
            popup,
            popup_rect,
        } = scene;
        let Some(view) = view.as_ref() else {
            return;
        };
        let (Some(rect), Some(popup)) = (*popup_rect, popup.as_ref()) else {
            self.publish(&view.texture, view.claim.clone());
            return;
        };
        let scale = self
            .output
            .presented_size()
            .expect("CEF paints a popup only after the host has sized the view")
            .scale;
        let (slot, claim) = pool.acquire(view.extent, view.format, || {
            self.allocator.allocate(view.extent, view.format)
        });
        let target = slot.texture().clone();
        let mut encoder =
            self.output
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("waterui_cef_popup_compose"),
                });
        encoder.copy_texture_to_texture(
            view.texture.as_image_copy(),
            target.as_image_copy(),
            extent3d(view.extent),
        );
        if let Some(region) = clip_popup(rect, scale, popup.extent, view.extent) {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    origin: wgpu::Origin3d {
                        x: region.source.0,
                        y: region.source.1,
                        z: 0,
                    },
                    ..popup.texture.as_image_copy()
                },
                wgpu::TexelCopyTextureInfo {
                    origin: wgpu::Origin3d {
                        x: region.destination.0,
                        y: region.destination.1,
                        z: 0,
                    },
                    ..target.as_image_copy()
                },
                extent3d(region.extent),
            );
        }
        self.output.queue().submit([encoder.finish()]);
        self.publish(&target, claim);
    }

    /// Hands `texture` to the host, which releases `claim` when it is done
    /// with the frame.
    fn publish(&self, texture: &wgpu::Texture, claim: Claim) {
        let frame = ExternalFrame::rgb(texture.clone(), RgbAlpha::Premultiplied, FrameColor::SRGB)
            .expect("a CEF frame slot satisfies the external-frame contract")
            .on_release(move || drop(claim));
        if self.output.present(frame).is_err() {
            tracing::debug!("The CEF frame output was retired; dropping the frame");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PopupRegion, clip_popup};
    use crate::CefPopupRect;

    const VIEW: (u32, u32) = (800, 600);

    fn rect(x: i32, y: i32) -> CefPopupRect {
        CefPopupRect {
            x,
            y,
            width: 100,
            height: 50,
        }
    }

    #[test]
    fn a_popup_inside_the_view_is_placed_at_its_scaled_origin() {
        assert_eq!(
            clip_popup(rect(10, 20), 2.0, (200, 100), VIEW),
            Some(PopupRegion {
                source: (0, 0),
                destination: (20, 40),
                extent: (200, 100),
            })
        );
    }

    #[test]
    fn a_popup_past_the_far_edges_is_clipped() {
        assert_eq!(
            clip_popup(rect(350, 290), 2.0, (200, 100), VIEW),
            Some(PopupRegion {
                source: (0, 0),
                destination: (700, 580),
                extent: (100, 20),
            })
        );
    }

    #[test]
    fn a_popup_past_the_near_edges_loses_its_leading_part() {
        assert_eq!(
            clip_popup(rect(-25, -10), 2.0, (200, 100), VIEW),
            Some(PopupRegion {
                source: (50, 20),
                destination: (0, 0),
                extent: (150, 80),
            })
        );
    }

    #[test]
    fn a_popup_wholly_outside_the_view_lands_nowhere() {
        assert_eq!(clip_popup(rect(400, 0), 2.0, (200, 100), VIEW), None);
        assert_eq!(clip_popup(rect(0, 300), 2.0, (200, 100), VIEW), None);
        assert_eq!(clip_popup(rect(-100, 0), 2.0, (200, 100), VIEW), None);
        assert_eq!(clip_popup(rect(0, -50), 2.0, (200, 100), VIEW), None);
    }

    #[test]
    fn a_fractional_scale_rounds_the_origin_to_the_nearest_pixel() {
        assert_eq!(
            clip_popup(rect(11, 7), 1.5, (150, 75), VIEW).map(|region| region.destination),
            Some((17, 11)),
            "16.5 rounds away from zero, 10.5 likewise"
        );
    }
}
