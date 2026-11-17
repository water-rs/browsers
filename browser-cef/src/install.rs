//! Where an application links CEF into its own views.
//!
//! Which browser engine draws a `WebView` is the application's choice, not the
//! renderer's: an app that wants Chromium depends on this crate, prepares the
//! runtime with [`install`] in its composition root and scopes [`CefWebView`]
//! over its views. The renderer stays engine-agnostic — it draws the `GpuView`
//! the hook returns exactly like any other — and a build that never asks for
//! CEF links none of it.

use waterui_core::accessibility::{AccessibilityRole, default_role};
use waterui_core::view::hook::Hook;
use waterui_core::{Environment, Metadata, Retain, View};

use crate::{CefRuntime, CefRuntimeConfiguration};

/// Returns the process-owned CEF runtime, starting one if the environment has
/// none.
///
/// Spawns nothing: an install runs while the application is still being built,
/// before a backend has installed the local executor. The message pump starts
/// with the first page the runtime creates.
fn ensure_runtime(env: &mut Environment) -> CefRuntime {
    env.get::<CefRuntime>().cloned().unwrap_or_else(|| {
        let runtime = CefRuntime::initialize(CefRuntimeConfiguration::packaged());
        env.insert(runtime.clone());
        runtime
    })
}

/// Prepares the environment for CEF's [`WebView`](waterui_webview::WebView)
/// realization.
///
/// It starts (or reuses) the packaged CEF runtime and supplies a
/// [`WebViewController`](waterui_webview::WebViewController) unless the
/// application already installed one of its own. [`CefWebView`] draws the pages
/// that controller opens.
///
/// Call it from the application's composition root, and scope the hook over the
/// views that hold web views:
///
/// ```ignore
/// pub fn app(mut env: Environment) -> App {
///     waterui_browser_cef::install(&mut env);
///     App::new(|| root().hooks(waterui_browser_cef::CefWebView), env)
/// }
/// ```
///
/// Installing spawns no task, so it is safe to call before the backend has
/// installed the local executor. The runtime's message pump starts when the
/// first page is created.
#[cfg(feature = "webview")]
pub fn install(env: &mut Environment) {
    use waterui_webview::WebViewController;

    let runtime = ensure_runtime(env);
    // The controller is a default, not an override: an application or test that
    // opened its pages with a controller of its own keeps it.
    if env.get::<WebViewController>().is_none() {
        env.insert(runtime.webview_controller());
    }
}

/// CEF's realization of the standard [`WebView`](waterui_webview::WebView):
/// every page in its scope drawn into a GPU surface.
///
/// # Panics
///
/// Realizing a page another engine opened panics: the `WebViewController` in
/// the environment is not CEF's.
#[cfg(feature = "webview")]
#[derive(Debug, Clone, Copy, Default)]
pub struct CefWebView;

#[cfg(feature = "webview")]
impl Hook for CefWebView {
    type Target = waterui_webview::WebView;

    fn apply(
        &self,
        webview: waterui_webview::WebView,
        env: &Environment,
    ) -> Result<impl View, waterui_webview::WebView> {
        let page = webview
            .handle()
            .downcast_ref::<crate::CefWebViewHandle>()
            .expect(
                "the CEF WebView realization was handed a page from another engine; \
                 the WebViewController in this environment is not CEF's",
            )
            .page()
            .clone();
        // The CEF view takes its own input: it reports `wants_input_events`,
        // so a renderer routes what lands on this layer straight into this
        // crate's adapter and owns no Chromium semantics.
        let surface = crate::gpu_view_with_input(page);
        // The semantic `WebView` owns the subscriptions that drive
        // `can_go_back` / `can_go_forward`, the event signal, and the URL and
        // user-agent bindings. Dropping it here would leave a live page whose
        // reactive state had gone dead.
        // The page is drawn into a texture the host tree cannot see into, so
        // the view publishes the accessibility node itself.
        Ok(Metadata::new(
            default_role(env, surface, AccessibilityRole::Group),
            Retain::new(webview),
        ))
    }
}

/// Prepares the environment for CEF's
/// [`ChromiumView`](waterui_chromium::ChromiumView) realization.
///
/// It starts the runtime and supplies a `ChromiumController` unless the
/// application installed one. [`CefChromium`] draws its pages.
///
/// The full Chromium component is separate from the standard `WebView` on
/// purpose, and so is its install: an application that wants both calls both,
/// and they share one runtime and one message pump.
///
/// Like [`install`], it spawns no task: the pump starts when the runtime
/// creates its first page, whichever component opens it.
#[cfg(feature = "chromium")]
pub fn install_chromium(env: &mut Environment) {
    use waterui_chromium::ChromiumController;
    use waterui_core::State;

    let runtime = ensure_runtime(env);
    if env.get::<State<ChromiumController>>().is_none() {
        env.insert(State(runtime.chromium_controller()));
    }
}

/// CEF's realization of [`ChromiumView`](waterui_chromium::ChromiumView):
/// every visible page in its scope drawn into a GPU surface.
///
/// # Panics
///
/// Realizing a headless page, or one another runtime opened, panics.
#[cfg(feature = "chromium")]
#[derive(Debug, Clone, Copy, Default)]
pub struct CefChromium;

#[cfg(feature = "chromium")]
impl Hook for CefChromium {
    type Target = waterui_chromium::ChromiumView;

    fn apply(
        &self,
        chromium: waterui_chromium::ChromiumView,
        env: &Environment,
    ) -> Result<impl View, waterui_chromium::ChromiumView> {
        assert_eq!(
            chromium.page().mode(),
            waterui_chromium::PageMode::Visible,
            "a headless Chromium page cannot be rendered as a ChromiumView"
        );
        let page = chromium
            .page()
            .handle()
            .downcast_ref::<crate::CefPageHandle>()
            .expect(
                "the Chromium realization was handed a page from another runtime; \
                 the ChromiumController in this environment is not CEF's",
            )
            .clone();
        let surface = crate::gpu_view_with_input(page);
        // The page is drawn into a texture the host tree cannot see into, so
        // the view publishes the accessibility node itself.
        Ok(Metadata::new(
            default_role(env, surface, AccessibilityRole::Group),
            Retain::new(chromium),
        ))
    }
}
