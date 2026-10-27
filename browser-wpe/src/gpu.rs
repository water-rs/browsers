use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use num_traits::ToPrimitive as _;
use waterui_graphics::gpu::{Context, Frame, GpuContent, GpuContentView, RedrawHandle};
use wgpu_external_frame::dma_buf::{DmaBufFrame, DmaBufImporter};

/// The uniform block the blit pipeline binds at `@group(0) @binding(2)`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FrameOptions {
    force_opaque: u32,
    /// WGSL rounds a uniform struct up to a multiple of 16 bytes. The shader
    /// declares the flag as a `vec4<u32>` and reads only its `x` lane, so the
    /// block is exactly 16 bytes; these are the three lanes it never reads.
    _padding: [u32; 3],
}

impl FrameOptions {
    /// The size the uniform buffer is allocated with and the pipeline layout
    /// declares as the binding's minimum.
    const SIZE: NonZeroU64 = match NonZeroU64::new(size_of::<Self>() as u64) {
        Some(size) => size,
        None => panic!("the blit's uniform block has fields"),
    };
}

use crate::frame::{BrowserFrame, ShmFrame};

#[cfg(feature = "webview")]
use crate::WpePage;
#[cfg(feature = "webview")]
use crate::input::{WpeInputGpuView, WpeSurfaceInput};

struct SourceTexture {
    size: (u32, u32),
    format: wgpu::TextureFormat,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

struct GpuState {
    importer: DmaBufImporter,
    target_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    options: wgpu::Buffer,
    source: Option<SourceTexture>,
}

struct Mailbox {
    /// Browser frames the UI side drained from the source and the render side
    /// has not yet consumed.
    frames: VecDeque<BrowserFrame>,
    /// The physical size and device-pixel ratio of the last rendered frame,
    /// published to the UI side so it can keep the browser's logical viewport
    /// in step. `(0, 0, _)` marks "no frame yet".
    viewport: (u32, u32, f64),
    /// The render thread's redraw handle, published once at setup so the UI
    /// side can hand it to the frame source as its waker.
    redraw: Option<RedrawHandle>,
}

/// Crosses the UI/render boundary both ways between [`BrowserUiBridge`] and
/// [`BrowserGpuContent`].
struct BrowserShared {
    state: Mutex<Mailbox>,
}

impl BrowserShared {
    const fn new() -> Self {
        Self {
            state: Mutex::new(Mailbox {
                frames: VecDeque::new(),
                viewport: (0, 0, 1.0),
                redraw: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.state.lock().expect("WPE frame mailbox poisoned")
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

/// The render-side half of a DMA-BUF browser view: owns the GPU state and the
/// newest pending frame, all `Send`.
struct BrowserGpuContent {
    shared: Arc<BrowserShared>,
    gpu: Option<GpuState>,
    pending_frame: Option<BrowserFrame>,
}

impl GpuContent for BrowserGpuContent {
    fn setup(&mut self, context: &Context<'_>) {
        self.shared.lock().redraw = Some(context.redraw.clone());
        self.gpu = Some(create_gpu_state(context));
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        {
            let mut state = self.shared.lock();
            state.viewport = (frame.width, frame.height, f64::from(frame.scale));
            if self.pending_frame.is_none() {
                self.pending_frame = state.frames.pop_front();
            }
        }
        let Some(pending) = self.pending_frame.as_ref() else {
            clear_target(frame);
            frame.request_redraw();
            return;
        };
        if !pending.is_render_ready() {
            frame.request_redraw();
            return;
        }

        let incoming = self
            .pending_frame
            .take()
            .expect("ready WPE frame must remain pending");
        let gpu = self
            .gpu
            .as_mut()
            .expect("WPE GPU content rendered before setup");
        render_browser_frame(gpu, incoming, frame);
        let next = self.shared.lock().frames.pop_front();
        if let Some(next) = next {
            self.pending_frame = Some(next);
            frame.request_redraw();
        }
    }
}

/// The UI-thread half of a DMA-BUF browser view: drives the frame source and
/// hands its frames to the render side.
///
/// `WpePage` and any other [`BrowserFrameSource`] are confined to the UI
/// thread, so everything that touches the source lives here and only DMA-BUF
/// frames cross.
struct BrowserUiBridge<S> {
    source: S,
    shared: Arc<BrowserShared>,
    waker_installed: bool,
}

impl<S: BrowserFrameSource> BrowserUiBridge<S> {
    /// Runs one UI-side browser frame; call once per presented frame.
    ///
    /// # Panics
    ///
    /// Panics when the logical viewport does not fit a `u32`.
    fn frame(&mut self) {
        if !self.waker_installed {
            let redraw = self.shared.lock().redraw.take();
            if let Some(redraw) = redraw {
                self.source
                    .set_frame_waker(Rc::new(move || redraw.request_redraw()));
                self.waker_installed = true;
            }
        }
        self.source.pump();
        let (width, height, scale) = self.shared.lock().viewport;
        if width > 0 && height > 0 {
            let logical_width = (f64::from(width) / scale)
                .round()
                .max(1.0)
                .to_u32()
                .expect("WPE logical width exceeds u32");
            let logical_height = (f64::from(height) / scale)
                .round()
                .max(1.0)
                .to_u32()
                .expect("WPE logical height exceeds u32");
            self.source.resize(logical_width, logical_height, scale);
        }
        while let Some(next) = self.source.take_frame() {
            self.shared.lock().frames.push_back(next);
        }
    }
}

/// GPU view that composites a Linux browser DMA-BUF stream without CPU
/// readback.
///
/// The source stays on the UI thread — [`BrowserFrameSource`] implementations
/// are browser engine objects — so the produced [`GpuContentView`] carries a
/// frame hook that drives it; only the frames themselves cross to the render
/// side.
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
    /// The device scale comes from the frame the host draws — see
    /// [`Frame::scale`] — so nothing has to publish it separately.
    #[must_use]
    pub const fn new(source: S) -> Self {
        Self { source }
    }

    /// Returns the frame source.
    #[must_use]
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// The render-side [`GpuContent`] and the UI-side bridge hook, for hosts
    /// that drive the render loop by hand instead of through a surface.
    ///
    /// The hook is `GpuContentView`'s `on_frame` callback lifted out: call it
    /// once per host frame before rendering, exactly as a hosted view's
    /// `frame()` does.
    pub fn into_parts(self) -> (impl GpuContent, impl Fn() + 'static) {
        let shared = Arc::new(BrowserShared::new());
        let content = BrowserGpuContent {
            shared: Arc::clone(&shared),
            gpu: None,
            pending_frame: None,
        };
        let bridge = RefCell::new(BrowserUiBridge {
            source: self.source,
            shared,
            waker_installed: false,
        });
        (content, move || bridge.borrow_mut().frame())
    }

    /// Builds the [`GpuContentView`] that composites the frame stream.
    #[must_use]
    pub fn into_view(self) -> GpuContentView {
        let (content, pump) = self.into_parts();
        GpuContentView::new(content).on_frame(pump)
    }
}

/// Creates the presenter for one visible WPE page, wired to take its own
/// input.
///
/// The view answers
/// [`wants_input_events`](GpuContentView::wants_input_events), so a backend
/// that routes surface input to GPU views needs nothing WPE-specific: the
/// pointer, keyboard, scroll and composition events landing on this layer
/// reach `WPEPlatform` through
/// [`WpeSurfaceInput`](crate::WpeSurfaceInput). A backend whose input arrives
/// somewhere else entirely — GTK delivers it to the `GtkGLArea`'s event
/// controllers — builds a [`BrowserGpuView`] and owns a `WpeSurfaceInput`
/// beside it instead.
#[cfg(feature = "webview")]
#[must_use]
pub fn gpu_view_with_input(page: WpePage) -> GpuContentView {
    WpeInputGpuView::new(
        BrowserGpuView::new(page.clone()).into_view(),
        WpeSurfaceInput::new(page),
    )
    .into_view()
}

fn render_browser_frame(gpu: &mut GpuState, incoming: BrowserFrame, frame: &Frame<'_>) {
    assert_eq!(
        frame.format, gpu.target_format,
        "WPE target format changed after setup"
    );
    match incoming {
        BrowserFrame::DmaBuf(dma_buf) => render_dma_buf_frame(gpu, dma_buf, frame),
        BrowserFrame::Shm(shm) => render_shm_frame(gpu, shm, frame),
    }
}

fn render_dma_buf_frame(gpu: &mut GpuState, mut incoming: DmaBufFrame, frame: &Frame<'_>) {
    ensure_source_texture(
        gpu,
        frame.device,
        incoming.width,
        incoming.height,
        incoming.format.texture_format(),
    );
    let bind_group = create_source_bind_group(
        gpu,
        incoming.format.force_opaque(),
        frame.device,
        frame.queue,
    );
    let source = gpu
        .source
        .as_ref()
        .expect("WPE source texture must exist before import");
    let mut import = gpu.importer.copy_into(&mut incoming, &source.texture);
    incoming.presented();
    encode_browser_blit(gpu, &bind_group, frame, &mut import.encoder);
    // The Vulkan import records its queue-family acquire/copy/release into
    // `command_buffers`; they go ahead of the blit encoder in one submission
    // (the GLES path leaves it empty and has already run its copy).
    frame.queue.submit(
        import
            .command_buffers
            .into_iter()
            .chain([import.encoder.finish()]),
    );
    let guard = import.guard;
    frame.queue.on_submitted_work_done(move || {
        drop(guard);
        incoming.release(None);
    });
}

/// Uploads a shared-memory frame into the same source texture the imported
/// dma-bufs land in, so the blit below sees one texture contract either way.
///
/// The lease is returned as soon as the upload has been staged:
/// [`wgpu::Queue::write_texture`] copies out of the mapping before it returns,
/// which is the whole reason a shared-memory frame needs neither a fence nor
/// the deferred release its dma-buf sibling takes.
fn render_shm_frame(gpu: &mut GpuState, mut incoming: ShmFrame, frame: &Frame<'_>) {
    let format = incoming.format();
    ensure_source_texture(
        gpu,
        frame.device,
        incoming.width(),
        incoming.height(),
        format.texture_format(),
    );
    let bind_group =
        create_source_bind_group(gpu, format.force_opaque(), frame.device, frame.queue);
    let source = gpu
        .source
        .as_ref()
        .expect("WPE source texture must exist before upload");
    frame.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        incoming.pixels(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(incoming.stride()),
            rows_per_image: Some(incoming.height()),
        },
        wgpu::Extent3d {
            width: incoming.width(),
            height: incoming.height(),
            depth_or_array_layers: 1,
        },
    );
    incoming.presented();
    let mut encoder = frame
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("waterui_wpe_shm_encoder"),
        });
    encode_browser_blit(gpu, &bind_group, frame, &mut encoder);
    frame.queue.submit([encoder.finish()]);
    incoming.release();
}

fn create_source_bind_group(
    gpu: &GpuState,
    force_opaque: bool,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> wgpu::BindGroup {
    let source = gpu
        .source
        .as_ref()
        .expect("WPE source texture must exist after allocation");
    let options = FrameOptions {
        force_opaque: u32::from(force_opaque),
        _padding: [0; 3],
    };
    queue.write_buffer(&gpu.options, 0, bytemuck::bytes_of(&options));
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("waterui_wpe_bind_group"),
        layout: &gpu.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&source.view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: gpu.options.as_entire_binding(),
            },
        ],
    })
}

fn encode_browser_blit(
    gpu: &GpuState,
    bind_group: &wgpu::BindGroup,
    frame: &Frame<'_>,
    encoder: &mut wgpu::CommandEncoder,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("waterui_wpe_blit"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: frame.view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(&gpu.pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..6, 0..1);
}

fn create_gpu_state(context: &Context<'_>) -> GpuState {
    let importer = DmaBufImporter::new(context.device, context.queue, context.adapter);
    let shader = context
        .device
        .create_shader_module(wgpu::include_wgsl!("wpe_blit.wgsl"));
    let bind_group_layout =
        context
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("waterui_wpe_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: Some(FrameOptions::SIZE),
                        },
                        count: None,
                    },
                ],
            });
    let pipeline_layout = context
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("waterui_wpe_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
    let pipeline = context
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("waterui_wpe_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: context.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
    GpuState {
        importer,
        target_format: context.format,
        pipeline,
        bind_group_layout,
        sampler: context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("waterui_wpe_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        }),
        options: context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("waterui_wpe_options"),
            size: FrameOptions::SIZE.get(),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }),
        source: None,
    }
}

fn ensure_source_texture(
    gpu: &mut GpuState,
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) {
    if gpu
        .source
        .as_ref()
        .is_some_and(|source| source.size == (width, height) && source.format == format)
    {
        return;
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("waterui_wpe_source"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    gpu.source = Some(SourceTexture {
        size: (width, height),
        format,
        texture,
        view,
    });
}

fn clear_target(frame: &Frame<'_>) {
    let mut encoder = frame
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("waterui_wpe_empty_encoder"),
        });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("waterui_wpe_empty"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame.view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    frame.queue.submit([encoder.finish()]);
}

#[cfg(test)]
mod tests {
    use super::FrameOptions;
    use naga::{AddressSpace, ResourceBinding, TypeInner};

    /// The pipeline's uniform block, as the shader declares it.
    ///
    /// wgpu is not an oracle for this: it checks the bound buffer against the
    /// pipeline layout, which [`super::create_gpu_state`] writes from
    /// [`FrameOptions`], so a shader that disagrees with both is accepted and
    /// only a real draw on a real device shows it. Reflecting the WGSL with the
    /// same front end wgpu parses it with asks the question the run-time check
    /// cannot, and needs no adapter to answer it.
    #[test]
    fn shader_uniform_block_matches_the_host_struct() {
        let module = naga::front::wgsl::parse_str(include_str!("wpe_blit.wgsl"))
            .expect("the blit shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("the blit shader validates");

        let options = module
            .global_variables
            .iter()
            .map(|(_, variable)| variable)
            .find(|variable| {
                variable.space == AddressSpace::Uniform
                    && variable.binding
                        == Some(ResourceBinding {
                            group: 0,
                            binding: 2,
                        })
            })
            .expect("the blit shader binds a uniform block at group 0 binding 2");
        let TypeInner::Struct { members, span } = &module.types[options.ty].inner else {
            panic!("the blit shader's uniform block is a struct");
        };

        assert_eq!(
            u64::from(*span),
            FrameOptions::SIZE.get(),
            "the shader's uniform block and FrameOptions are different sizes"
        );
        let force_opaque = members
            .iter()
            .find(|member| member.name.as_deref() == Some("force_opaque"))
            .expect("the blit shader's uniform block has a force_opaque member");
        assert_eq!(
            u64::from(force_opaque.offset),
            core::mem::offset_of!(FrameOptions, force_opaque) as u64,
            "force_opaque sits at a different offset in the shader than in FrameOptions"
        );
    }
}
