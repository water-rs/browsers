use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use num_traits::ToPrimitive as _;
use waterui_graphics::gpu::interop::vulkan::{
    self, DmaBuf, DmaBufPlane, FrameSource, QueueFamily, Wait,
};
use waterui_graphics::gpu::interop::{ExternalFrame, FrameColor, RgbAlpha};
use waterui_graphics::gpu::{ExternalFrameSource, ExternalFrameView, FrameOutput, PresentedSize};
use wgpu_external_frame::dma_buf::DmaBufFrame;

use crate::frame::{BrowserFrame, ShmFrame};

#[cfg(feature = "webview")]
use crate::WpePage;
#[cfg(feature = "webview")]
use crate::input::{WpeInputGpuView, WpeSurfaceInput};

/// `VK_IMAGE_LAYOUT_GENERAL`: the layout WPE leaves a DMA-BUF image in. It is
/// the layout `wgpu-external-frame`'s own importer acquires the image from
/// (`record_acquire`'s `old_layout`), which is the contract WPE's frames were
/// written against.
const IMAGE_LAYOUT_GENERAL: u32 = 1;

/// How a frame of `force_opaque` pixels composes: the `X` formats carry no
/// alpha, and every other WPE format holds premultiplied alpha.
const fn alpha(force_opaque: bool) -> RgbAlpha {
    if force_opaque {
        RgbAlpha::Opaque
    } else {
        RgbAlpha::Premultiplied
    }
}

/// Hands the bridge a shared-memory slot back to its pool.
struct ShmRelease(Arc<AtomicBool>);

impl ShmRelease {
    fn release(self) {
        self.0.store(true, Ordering::Release);
    }
}

struct ShmSlot {
    texture: wgpu::Texture,
    extent: (u32, u32),
    format: wgpu::TextureFormat,
    free: Arc<AtomicBool>,
}

/// The textures shared-memory frames are uploaded into and presented from. A
/// slot is reused only once the host has released the frame it carried.
#[derive(Default)]
struct ShmPool {
    slots: Vec<ShmSlot>,
}

impl ShmPool {
    /// A texture nobody else holds, of `extent` and `format`, and the release
    /// that frees it again. A free slot of another size is dropped here, so a
    /// resize leaves nothing behind.
    fn acquire(
        &mut self,
        device: &wgpu::Device,
        extent: (u32, u32),
        format: wgpu::TextureFormat,
    ) -> (wgpu::Texture, ShmRelease) {
        self.slots.retain(|slot| {
            !slot.free.load(Ordering::Acquire) || (slot.extent == extent && slot.format == format)
        });
        let index = if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.free.load(Ordering::Acquire))
        {
            index
        } else {
            self.slots.push(ShmSlot {
                texture: device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("waterui_wpe_shm_slot"),
                    size: wgpu::Extent3d {
                        width: extent.0,
                        height: extent.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                }),
                extent,
                format,
                free: Arc::new(AtomicBool::new(true)),
            });
            self.slots.len() - 1
        };
        let slot = &self.slots[index];
        slot.free.store(false, Ordering::Release);
        (slot.texture.clone(), ShmRelease(Arc::clone(&slot.free)))
    }
}

/// The device-bound half of the view: what one [`FrameOutput`]'s frames are
/// imported or uploaded with. A new output, after a device loss, gets a new
/// sink and the old one goes with its device.
struct FrameSink {
    output: FrameOutput,
    vulkan: vulkan::Device,
    shm: ShmPool,
}

impl FrameSink {
    /// # Panics
    ///
    /// Panics unless the host's device is Vulkan: it is the one backend a
    /// DMA-BUF is imported on.
    fn new(output: FrameOutput) -> Self {
        let vulkan = vulkan::Device::new(output.shared_device())
            .unwrap_or_else(|error| panic!("WPE frames need a Vulkan host device: {error}"));
        Self {
            output,
            vulkan,
            shm: ShmPool::default(),
        }
    }

    fn present(&mut self, frame: BrowserFrame) {
        let frame = match frame {
            BrowserFrame::DmaBuf(dma_buf) => self.import_dma_buf(dma_buf),
            BrowserFrame::Shm(shm) => self.upload_shm(shm),
        };
        if self.output.present(frame).is_err() {
            tracing::debug!("The WPE frame output was retired; dropping the frame");
        }
    }

    /// Imports the DMA-BUF in place: no copy, and no draw. The engine waits on
    /// the page's rendering fence on the GPU, and WPE gets the buffer back once
    /// the engine will not read it again.
    fn import_dma_buf(&self, mut frame: DmaBufFrame) -> ExternalFrame {
        assert_eq!(
            frame.visible_size(),
            (frame.width, frame.height),
            "WPE hands over exactly the picture, which the in-place import presents whole"
        );
        assert_eq!(
            frame.planes.len(),
            1,
            "a WPE DMA-BUF frame carries one packed plane"
        );
        let plane = frame
            .planes
            .pop()
            .expect("the plane count was just checked");
        let descriptor = DmaBuf {
            fourcc: frame.format.fourcc(),
            modifier: frame.modifier,
            size: (frame.width, frame.height),
            planes: vec![DmaBufPlane {
                memory: 0,
                offset: plane.offset,
                stride: plane.stride,
            }],
            memory: vec![plane.fd],
            layout: IMAGE_LAYOUT_GENERAL,
            producer_family: QueueFamily::External,
            sync: frame.rendering_fence.take().map(|fd| Wait::SyncFd { fd }),
            release: None,
            color: FrameColor::SRGB,
            alpha: alpha(frame.format.force_opaque()),
        };
        let imported = self
            .vulkan
            .import(FrameSource::DmaBuf(Box::new(descriptor)))
            .unwrap_or_else(|error| panic!("failed to import a WPE DMA-BUF frame: {error}"));
        frame.presented();
        ExternalFrame::native(imported)
            .expect("an imported WPE DMA-BUF satisfies the external-frame contract")
            .on_release(move || frame.release(None))
    }

    /// Uploads the mapping into a pooled texture and gives the lease back at
    /// once: [`wgpu::Queue::write_texture`] copies out of the mapping before it
    /// returns, which is the whole reason a shared-memory frame needs neither a
    /// fence nor the deferred release its DMA-BUF sibling takes.
    fn upload_shm(&mut self, mut frame: ShmFrame) -> ExternalFrame {
        let format = frame.format();
        let extent = (frame.width(), frame.height());
        let (texture, release) =
            self.shm
                .acquire(self.output.device(), extent, format.texture_format());
        self.output.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            frame.pixels(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.stride()),
                rows_per_image: Some(frame.height()),
            },
            wgpu::Extent3d {
                width: extent.0,
                height: extent.1,
                depth_or_array_layers: 1,
            },
        );
        frame.presented();
        frame.release();
        ExternalFrame::rgb(texture, alpha(format.force_opaque()), FrameColor::SRGB)
            .expect("an uploaded WPE frame satisfies the external-frame contract")
            .on_release(move || release.release())
    }
}

/// Source of Linux browser frames for GPU-only DMA-BUF composition.
pub trait BrowserFrameSource: 'static {
    /// Drains engine work that is ready on the current thread.
    fn pump(&self);
    /// Updates the browser viewport.
    fn resize(&self, width: u32, height: u32, scale: f64);
    /// Installs the host redraw callback.
    fn set_frame_waker(&self, waker: Rc<dyn Fn()>);
    /// Takes the newest available frame.
    fn take_frame(&self) -> Option<BrowserFrame>;
}

#[cfg(feature = "webview")]
impl BrowserFrameSource for WpePage {
    fn pump(&self) {
        Self::pump(self);
    }

    fn resize(&self, width: u32, height: u32, scale: f64) {
        Self::resize(self, width, height, scale);
    }

    fn set_frame_waker(&self, waker: Rc<dyn Fn()>) {
        Self::set_frame_waker(self, move || waker());
    }

    fn take_frame(&self) -> Option<BrowserFrame> {
        Self::take_frame(self)
    }
}

/// The output the host most recently started the source with, shared between
/// the source and the UI bridge. Both live on the UI thread.
#[derive(Default)]
struct StartedOutput {
    output: Option<FrameOutput>,
    /// Whether `output` is new since the bridge last built its sink on it.
    fresh: bool,
}

/// The [`ExternalFrameSource`] of a browser view: it only records the output
/// the host starts it with, again after a device loss. The frame source is
/// confined to the UI thread, so everything that touches it lives in
/// [`BrowserUiBridge`].
struct BrowserExternalSource {
    started: Rc<RefCell<StartedOutput>>,
}

impl ExternalFrameSource for BrowserExternalSource {
    fn start(&mut self, output: FrameOutput) {
        *self.started.borrow_mut() = StartedOutput {
            output: Some(output),
            fresh: true,
        };
    }
}

/// The UI-thread half of a browser view: drives the frame source and presents
/// its frames on the host's output.
///
/// `WpePage` and any other [`BrowserFrameSource`] are confined to the UI
/// thread, so everything that touches the source lives here.
struct BrowserUiBridge<S> {
    source: S,
    started: Rc<RefCell<StartedOutput>>,
    sink: Option<FrameSink>,
}

impl<S: BrowserFrameSource> BrowserUiBridge<S> {
    /// Runs one UI-side browser frame; call once per host pass.
    ///
    /// # Panics
    ///
    /// Panics when the logical viewport does not fit a `u32`.
    fn frame(&mut self) {
        let (output, fresh) = {
            let mut started = self.started.borrow_mut();
            (started.output.clone(), std::mem::take(&mut started.fresh))
        };
        // The host starts the source when it builds the layer, before its first
        // pass runs this hook; until then there is nothing to pump.
        let Some(output) = output else {
            return;
        };
        if fresh {
            self.sink = Some(FrameSink::new(output.clone()));
            let redraw = output.clone();
            self.source
                .set_frame_waker(Rc::new(move || redraw.request_redraw()));
        }
        self.source.pump();
        if let Some(PresentedSize { pixels, scale }) = output.presented_size() {
            let logical = |pixels: u32| {
                (f64::from(pixels) / scale)
                    .round()
                    .max(1.0)
                    .to_u32()
                    .expect("WPE logical size exceeds u32")
            };
            self.source
                .resize(logical(pixels.width()), logical(pixels.height()), scale);
        } else {
            // The host reports its size during its first pass, after this hook;
            // another pass lets the viewport follow it.
            output.request_redraw();
        }
        let sink = self
            .sink
            .as_mut()
            .expect("the sink is built with the first output");
        while let Some(next) = self.source.take_frame() {
            sink.present(next);
        }
    }
}

/// External-frame view that presents a Linux browser's frame stream without a
/// copy or a draw.
///
/// The source stays on the UI thread — [`BrowserFrameSource`] implementations
/// are browser engine objects — so the produced [`ExternalFrameView`] carries a
/// frame hook that drives it and publishes what it takes.
pub struct BrowserGpuView<S> {
    source: S,
}

/// WPE-specialized DMA-BUF GPU view.
#[cfg(feature = "webview")]
pub type WpeGpuView = BrowserGpuView<WpePage>;

impl<S> core::fmt::Debug for BrowserGpuView<S> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("WpeGpuView").finish_non_exhaustive()
    }
}

impl<S: BrowserFrameSource> BrowserGpuView<S> {
    /// Creates a view over `source`'s frame stream.
    ///
    /// The viewport follows the size the host presents at, so nothing has to
    /// publish the device scale separately.
    #[must_use]
    pub const fn new(source: S) -> Self {
        Self { source }
    }

    /// Returns the frame source.
    #[must_use]
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// The [`ExternalFrameSource`] and the UI-side bridge hook, for hosts that
    /// drive the render loop by hand instead of through a view.
    ///
    /// The hook is [`ExternalFrameView::on_frame`]'s callback lifted out: call
    /// it once per host pass before the host drains the frames, exactly as a
    /// hosted view's `frame()` does.
    pub fn into_parts(self) -> (impl ExternalFrameSource, impl Fn() + 'static) {
        let started = Rc::new(RefCell::new(StartedOutput::default()));
        let source = BrowserExternalSource {
            started: Rc::clone(&started),
        };
        let bridge = RefCell::new(BrowserUiBridge {
            source: self.source,
            started,
            sink: None,
        });
        (source, move || bridge.borrow_mut().frame())
    }

    /// Builds the [`ExternalFrameView`] that presents the frame stream.
    #[must_use]
    pub fn into_view(self) -> ExternalFrameView {
        let (source, pump) = self.into_parts();
        ExternalFrameView::new(source).on_frame(pump)
    }
}

/// Creates the view for one visible WPE page, wired to take its own input.
///
/// The view answers
/// [`wants_input_events`](ExternalFrameView::wants_input_events), so a backend
/// that routes surface input to GPU views needs nothing WPE-specific: the
/// pointer, keyboard, scroll and composition events landing on this layer
/// reach `WPEPlatform` through
/// [`WpeSurfaceInput`](crate::WpeSurfaceInput). A backend whose input arrives
/// somewhere else entirely — GTK delivers it to the `GtkGLArea`'s event
/// controllers — builds a [`BrowserGpuView`] and owns a `WpeSurfaceInput`
/// beside it instead.
#[cfg(feature = "webview")]
#[must_use]
pub fn gpu_view_with_input(page: WpePage) -> ExternalFrameView {
    WpeInputGpuView::new(
        BrowserGpuView::new(page.clone()).into_view(),
        WpeSurfaceInput::new(page),
    )
    .into_view()
}
