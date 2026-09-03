//! Bundled WPE `WebKit` engine used by `WaterUI`'s standard Linux `WebView`.
//!
//! The Rust crate never links the host's WPE or `WebKit` libraries. `water`
//! stages an exact runtime next to the application and this crate loads its
//! narrow, versioned `libwaterui_wpe` ABI.
//!
//! WPE is a Linux engine — the runtime polls dma-buf fences through `poll(2)`
//! and passes dma-buf file descriptors — so this crate is empty elsewhere,
//! the same way `waterui-gtk` is.
//!
//! A frame arrives in whichever platform buffer WPE rendered into, which
//! [`BrowserFrame`] is the choice between. A display with a DRM render node
//! renders into a dma-buf, which crosses as a
//! [`wgpu_external_frame::dma_buf::DmaBufFrame`] and imports into a texture with
//! no copy — importing a dma-buf is a general problem, so it lives in a crate
//! that knows nothing about WPE or `WaterUI`. A display without one — a
//! container, a virtual machine, a hosted CI runner — renders into shared
//! memory, which crosses as an [`ShmFrame`] and is uploaded into the same
//! texture. Neither is a fallback for the other: WPE Platform picks the kind
//! from what the host can do. What is WPE's either way is the buffer lease
//! ([`WpeFrameLease`]) and the compositing view built on top
//! ([`BrowserGpuView`]).
//!
//! # Testing against the real engine
//!
//! `tests/real_engine.rs` drives an actual WPE `WebKit` runtime — navigation,
//! history, and the `waterui` bridge in both directions. Running it needs a
//! staged runtime, so it sits behind the `real-engine` feature and its module
//! documentation carries the commands. `.github/workflows/browser-wpe.yml` runs
//! it on the paths it guards; nothing else in CI does.

#[cfg(all(feature = "webview", target_os = "linux"))]
mod abi;
#[cfg(target_os = "linux")]
mod frame;
#[cfg(target_os = "linux")]
mod gpu;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod input;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod install;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod lease;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod page;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod pump;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod runtime;
#[cfg(all(feature = "webview", target_os = "linux"))]
mod webview;

#[cfg(target_os = "linux")]
pub use frame::{BrowserFrame, ShmFormat, ShmFrame};
#[cfg(target_os = "linux")]
pub use gpu::{BrowserFrameSource, BrowserGpuView};
#[cfg(all(target_os = "linux", feature = "webview"))]
pub use gpu::{WpeGpuView, gpu_view_with_input};
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use input::{WpeInputGpuView, WpeSurfaceInput};
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use install::install;
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use lease::WpeFrameLease;
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use page::{PointerButton, WpePage};
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use pump::{PumpDeadline, WpePollFd, WpeReadiness};
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use runtime::{WPE_WEBKIT_VERSION, WpeRuntime, WpeRuntimePaths};
#[cfg(all(feature = "webview", target_os = "linux"))]
pub use webview::{WpeController, WpeWebViewHandle};
