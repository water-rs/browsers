#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::cell::RefCell;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::rc::Rc;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use num_traits::ToPrimitive as _;
use waterui_graphics::gpu::ExternalFrameView;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use waterui_graphics::gpu::{ExternalFrameSource, FrameOutput, PresentedSize};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use crate::AcceleratedFrameSink;
use crate::CefPageHandle;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use crate::input::CefSurfaceInput;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod pool;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod publisher;
#[cfg(target_os = "windows")]
mod windows;

/// The output the host most recently started the source with, shared between
/// the source and the UI bridge. Both live on the UI thread.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[derive(Default)]
struct StartedOutput {
    output: Option<FrameOutput>,
    /// Whether `output` is new since the bridge last installed a sink on it.
    fresh: bool,
}

/// The [`ExternalFrameSource`] of one CEF page.
///
/// It only records the output the host starts it with, again after a device
/// loss. The page lives on the UI thread, so everything that touches it is in
/// [`CefUiBridge`], and the frames themselves are published from the sink the
/// bridge installs.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct CefFrameSource {
    started: Rc<RefCell<StartedOutput>>,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl ExternalFrameSource for CefFrameSource {
    fn start(&mut self, output: FrameOutput) {
        *self.started.borrow_mut() = StartedOutput {
            output: Some(output),
            fresh: true,
        };
    }
}

/// The UI-thread half of a CEF view: it installs the page's frame sink on
/// every output the host starts the source with, keeps Chromium's logical
/// viewport in step with the size the host presents at, and asks Chromium for
/// the next compositor frame.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct CefUiBridge<S: AcceleratedFrameSink> {
    page: CefPageHandle,
    started: Rc<RefCell<StartedOutput>>,
    make_sink: fn(FrameOutput) -> S,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl<S: AcceleratedFrameSink> CefUiBridge<S> {
    /// Runs one UI-side browser frame; call once per host pass.
    ///
    /// # Panics
    ///
    /// Panics when the logical viewport does not fit a `u32`.
    fn frame(&self) {
        let (output, fresh) = {
            let mut started = self.started.borrow_mut();
            (started.output.clone(), std::mem::take(&mut started.fresh))
        };
        // The host starts the source when it builds the layer, before its
        // first pass runs this hook; until then there is nothing to pump.
        let Some(output) = output else {
            return;
        };
        if fresh {
            self.page.set_frame_sink((self.make_sink)(output.clone()));
        }
        if let Some(PresentedSize { pixels, scale }) = output.presented_size() {
            let logical = |pixels: u32| {
                (f64::from(pixels) / scale)
                    .round()
                    .max(1.0)
                    .to_u32()
                    .expect("CEF logical size exceeds u32")
            };
            self.page.set_viewport(
                logical(pixels.width()),
                logical(pixels.height()),
                scale.to_f32().expect("CEF device scale exceeds f32"),
            );
        } else {
            // The host reports its size during its first pass, after this hook;
            // another pass lets the viewport follow it.
            output.request_redraw();
        }
        // On Linux and Windows the host paces the compositor itself; asking
        // for the next pass here keeps `page.request_frame()` pumping. macOS
        // relies on Chromium's own pacing instead, and `external_begin_frame_enabled`
        // is off there — `request_frame` does not exist; paints arrive on
        // vsync and damage at `windowless_frame_rate` and wake the host
        // through the output.
        #[cfg(not(target_os = "macos"))]
        {
            self.page.request_frame();
            output.request_redraw();
        }
    }
}

/// The view of one CEF page, shown through the platform's frame sink.
///
/// No pump here. Chromium's message loop belongs to
/// `CefRuntime::start_message_pump`, which Chromium itself paces; running
/// `do_message_loop_work` inside the host pass put whatever the browser had
/// queued — parsing, script, compositing — on the main thread inside one
/// frame's budget, which is what tripped the stall probe every few seconds on
/// an idle page. The UI hook installs the sink and requests the next
/// compositor frame and nothing else.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn external_view<S: AcceleratedFrameSink>(
    page: CefPageHandle,
    make_sink: fn(FrameOutput) -> S,
) -> ExternalFrameView {
    let started = Rc::new(RefCell::new(StartedOutput::default()));
    let bridge = CefUiBridge {
        page,
        started: Rc::clone(&started),
        make_sink,
    };
    ExternalFrameView::new(CefFrameSource { started }).on_frame(move || bridge.frame())
}

/// Creates the target-specific external-frame view for one visible CEF page.
#[cfg(target_os = "linux")]
#[must_use]
pub fn gpu_view(page: CefPageHandle) -> ExternalFrameView {
    external_view(page, linux::make_frame_sink)
}

/// Creates the target-specific external-frame view for one visible CEF page.
#[cfg(target_os = "macos")]
#[must_use]
pub fn gpu_view(page: CefPageHandle) -> ExternalFrameView {
    external_view(page, macos::make_frame_sink)
}

/// Creates the target-specific external-frame view for one visible CEF page.
#[cfg(target_os = "windows")]
#[must_use]
pub fn gpu_view(page: CefPageHandle) -> ExternalFrameView {
    external_view(page, windows::make_frame_sink)
}

/// Creates the view for one visible CEF page, wired to take its own input.
///
/// The view answers
/// [`wants_input_events`](ExternalFrameView::wants_input_events), so a backend
/// that routes surface input to GPU views needs nothing CEF-specific: the
/// pointer, keyboard, scroll and composition events landing on this layer
/// reach Chromium through [`CefSurfaceInput`]. A backend whose input arrives
/// somewhere else entirely — GTK delivers it to the `GtkGLArea`'s event
/// controllers — uses [`gpu_view`] and owns a [`CefSurfaceInput`] beside it
/// instead.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[must_use]
pub fn gpu_view_with_input(page: CefPageHandle) -> ExternalFrameView {
    let input = RefCell::new(CefSurfaceInput::new(page.clone()));
    gpu_view(page).on_input(move |event| input.borrow_mut().handle(event))
}
