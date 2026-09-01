//! What the CEF real-engine checks need of the process that runs them.
//!
//! Chromium launches its GPU, network and renderer processes by re-executing
//! this same test binary with a `--type=` argument on every platform, so
//! [`is_child_process`] is what tells one of those children from the browser
//! process.
//!
//! macOS asks for more than that, and [`macos`] is where the answer lives:
//! `cef_initialize` traps inside the framework when the browser process is not
//! bundled, and the child processes are launched from helper applications
//! Chromium locates relative to the outer bundle, so a plain
//! `target/debug/deps/real_engine-*` can never initialize CEF there. Linux and
//! Windows have no bundle — Chromium re-executes this binary directly and
//! `CefRuntimePaths::packaged` resolves the runtime beside it — so there is
//! nothing to stage.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{running_bundled, stage};

/// Whether this process was launched by Chromium as one of its child processes.
pub fn is_child_process() -> bool {
    std::env::args().any(|argument| argument.starts_with("--type="))
}

/// Everything this test writes, inside the build directory cargo already owns.
pub fn workspace() -> PathBuf {
    PathBuf::from(env!("OUT_DIR")).join("real-engine")
}
