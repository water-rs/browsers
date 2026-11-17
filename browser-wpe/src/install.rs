//! Where an application links WPE `WebKit` into its own views.
//!
//! Which browser engine draws a `WebView` is the application's choice, not the
//! renderer's: an app that wants WPE depends on this crate, prepares the
//! controller with [`install`] in its composition root and scopes
//! [`WpeWebView`] over its views. The renderer stays engine-agnostic — it draws
//! the `ExternalFrameView` the hook returns exactly like any other — and a
//! build that never asks for WPE links none of it.

use waterui_core::accessibility::{AccessibilityRole, default_role};
use waterui_core::view::hook::Hook;
use waterui_core::{Environment, Metadata, Retain, View};
use waterui_webview::{WebView, WebViewController};

use crate::{WpeController, WpeWebViewHandle, gpu_view_with_input};

/// Prepares the environment for WPE's [`WebView`] realization.
///
/// It supplies a [`WebViewController`] backed by the `water`-staged runtime, unless the
/// application already installed one of its own. The runtime itself is not
/// loaded until the first page opens. [`WpeWebView`] draws the pages that
/// controller opens.
///
/// Call it from the application's composition root, and scope the hook over the
/// views that hold web views:
///
/// ```ignore
/// pub fn app(mut env: Environment) -> App<impl Windows> {
///     waterui_browser_wpe::install(&mut env);
///     App::new(|| root().hooks(waterui_browser_wpe::WpeWebView), env)
/// }
/// ```
pub fn install(env: &mut Environment) {
    // The controller is a default, not an override: an application or test that
    // opened its pages with a controller of its own keeps it.
    if env.get::<WebViewController>().is_none() {
        env.insert(WebViewController::new(WpeController::packaged()));
    }
}

/// WPE's realization of the standard [`WebView`]: every page in its scope
/// drawn into a GPU surface.
///
/// # Panics
///
/// Realizing a page another engine opened panics: the `WebViewController` in
/// the environment is not WPE's.
#[derive(Debug, Clone, Copy, Default)]
pub struct WpeWebView;

impl Hook for WpeWebView {
    type Target = WebView;

    fn apply(&self, webview: WebView, env: &Environment) -> Result<impl View, WebView> {
        let page = webview
            .handle()
            .downcast_ref::<WpeWebViewHandle>()
            .expect(
                "the WPE WebView realization was handed a page from another engine; \
                 the WebViewController in this environment is not WPE's",
            )
            .page()
            .clone();
        // The WPE view takes its own input: it reports `wants_input_events`, so
        // a renderer routes what lands on this layer straight into this crate's
        // adapter and owns no `WPEPlatform` semantics.
        let surface = gpu_view_with_input(page);
        // The semantic `WebView` owns the subscriptions that drive
        // `can_go_back` / `can_go_forward`, the event signal, and the URL and
        // user-agent bindings. Dropping it here would leave a live page whose
        // reactive state had gone dead.
        // The page is drawn into a texture the host tree cannot see into, so
        // the surface publishes the accessibility node itself.
        Ok(Metadata::new(
            default_role(env, surface, AccessibilityRole::Group),
            Retain::new(webview),
        ))
    }
}
