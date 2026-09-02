//! Native Linux validation for the staged WPE runtime and GPU DMA-BUF path.

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::{Cell, RefCell};
    use std::mem::ManuallyDrop;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use tracing_subscriber::EnvFilter;
    use waterui_browser_wpe::{
        BrowserFrame, BrowserFrameSource, BrowserGpuView, WPE_WEBKIT_VERSION, WpePage, WpeRuntime,
        WpeRuntimePaths,
    };
    use waterui_core::Environment;
    use waterui_graphics::gpu_surface::{GpuSurface, OffscreenRenderConfig, OffscreenSize};
    use waterui_graphics::shared_context::GpuRuntime;
    use waterui_webview::{BackendEvent, WebViewEvent};

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;

    struct SmokeFrameSource {
        frame: RefCell<Option<BrowserFrame>>,
    }

    impl BrowserFrameSource for SmokeFrameSource {
        fn pump(&self) {}

        fn resize(&self, _width: u32, _height: u32, _scale: f64) {}

        fn set_frame_waker(&self, _waker: Rc<dyn Fn()>) {}

        fn take_frame(&self) -> Option<BrowserFrame> {
            self.frame.borrow_mut().take()
        }
    }

    /// Which platform buffer WPE rendered into, for the run's own record: the
    /// kind is the host's to decide, so a run that changes kind is a change in
    /// what was actually exercised.
    const fn frame_kind(frame: &BrowserFrame) -> &'static str {
        match frame {
            BrowserFrame::DmaBuf(_) => "dma-buf",
            BrowserFrame::Shm(_) => "shared memory",
        }
    }

    /// Composites one frame offscreen and writes it out.
    ///
    /// The surface owns the view, which owns the frame's lease, and
    /// `render_offscreen` consumes the surface — so the frame is handed back to
    /// the engine before this returns.
    fn snapshot(gpu_runtime: &GpuRuntime, frame: BrowserFrame, output_path: &std::ffi::OsStr) {
        let source = SmokeFrameSource {
            frame: RefCell::new(Some(frame)),
        };
        let surface = GpuSurface::new(BrowserGpuView::new(source));
        let config = OffscreenRenderConfig::new(
            OffscreenSize::try_from_pixels(WIDTH, HEIGHT)
                .expect("WPE smoke viewport must be non-zero"),
        )
        .format(wgpu::TextureFormat::Rgba8Unorm);
        let rendered = pollster::block_on(surface.render_offscreen(
            gpu_runtime,
            config,
            &mut Environment::new(),
        ))
        .unwrap_or_else(|error| panic!("WPE smoke offscreen render failed: {error}"));
        rendered
            .save_png(output_path)
            .unwrap_or_else(|error| panic!("WPE smoke snapshot write failed: {error}"));
    }

    pub fn run() {
        // The crate's own teardown events are `debug`, and this run is where
        // the order they happen in is the thing being watched.
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info,waterui_browser_wpe=debug")),
            )
            .init();
        let mut arguments = std::env::args_os().skip(1);
        let runtime_root = arguments
            .next()
            .expect("runtime_smoke requires a staged WPE runtime root");
        let output_path = arguments
            .next()
            .expect("runtime_smoke requires an output PNG path");
        let timeout_seconds = arguments
            .next()
            .expect("runtime_smoke requires a timeout in seconds")
            .to_string_lossy()
            .parse::<u64>()
            .expect("runtime_smoke timeout must be an integer");
        assert!(
            arguments.next().is_none(),
            "runtime_smoke received unexpected arguments"
        );

        let gpu_runtime = pollster::block_on(GpuRuntime::new())
            .unwrap_or_else(|error| panic!("WPE smoke GPU runtime creation failed: {error}"));
        let paths = WpeRuntimePaths::new(runtime_root);
        let runtime = WpeRuntime::initialize(&paths);
        let page = WpePage::new(runtime);
        let loaded = Rc::new(Cell::new(false));
        let load_error = Rc::new(RefCell::new(None::<String>));
        // The guard has to outlive the pump loop below: dropping it
        // unsubscribes, and the smoke run would then wait for a `Loaded` it can
        // no longer observe until it times out.
        let load_watcher = page.watch({
            let loaded = Rc::clone(&loaded);
            let load_error = Rc::clone(&load_error);
            move |event| match event {
                BackendEvent::Event(WebViewEvent::Loaded) => loaded.set(true),
                BackendEvent::Event(WebViewEvent::Error(error)) => {
                    load_error.replace(Some(format!("{error:?}")));
                }
                _ => {}
            }
        });
        let frame_ready = Rc::new(Cell::new(false));
        page.set_frame_waker({
            let frame_ready = Rc::clone(&frame_ready);
            move || frame_ready.set(true)
        });
        page.resize(WIDTH, HEIGHT, 1.0);
        let document =
            include_str!("runtime_smoke.html").replace("{{WPE_VERSION}}", WPE_WEBKIT_VERSION);
        let document = base64::engine::general_purpose::STANDARD.encode(document);
        page.load_uri(&format!("data:text/html;base64,{document}"));

        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        while !loaded.get() || !frame_ready.get() {
            page.pump();
            if let Some(error) = load_error.borrow().as_deref() {
                panic!("WPE smoke page load failed: {error}");
            }
            assert!(
                Instant::now() < deadline,
                "WPE smoke timed out before the page loaded and submitted a frame"
            );
            std::thread::yield_now();
        }
        let frame = page
            .take_frame()
            .expect("WPE signalled a frame without retaining it");
        tracing::info!(
            kind = frame_kind(&frame),
            width = WIDTH,
            height = HEIGHT,
            "WPE submitted a frame"
        );
        while !frame.is_render_ready() {
            page.pump();
            assert!(
                Instant::now() < deadline,
                "WPE smoke timed out waiting for the rendering fence"
            );
            std::thread::yield_now();
        }

        snapshot(&gpu_runtime, frame, &output_path);

        // Teardown, in the order the compiler would have dropped these anyway,
        // said out loud. The engine's own threads keep running throughout, so
        // when something dies here the log has to show how far the sequence had
        // got.
        // `render_offscreen` consumed the surface, and with it the view and the
        // frame it had leased, so the compositing side is already down by here.
        tracing::info!("snapshot written; tearing down");
        drop(frame_ready);
        drop(load_watcher);
        drop(load_error);
        drop(loaded);
        tracing::info!("dropping the page, which owns the runtime");
        drop(page);
        drop(paths);
        // The GPU runtime is deliberately not dropped, and this is the same
        // contract the engine gets: some libraries must not be unloaded from a
        // running process. Dropping it drops the wgpu `Instance`, and
        // wgpu-hal's GLES and Vulkan backends hold `libloading` handles for
        // `libEGL` and `libvulkan`; closing those unloads Mesa's software
        // drivers — llvmpipe and the LLVM inside it, which is what a runner
        // with no GPU renders through — and those register `atexit`
        // destructors that have to still be mapped when the process exits.
        // The core this smoke captured is precisely that: `exit()` calling
        // into an address that no library owns any more.
        tracing::info!("pinning the GPU runtime for the life of the process");
        let _gpu_runtime = ManuallyDrop::new(gpu_runtime);
        tracing::info!("smoke teardown complete");
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("runtime_smoke is supported only on Linux");
}
