//! # Safety
//!
//! Loading the bridge means `dlopen` plus symbol lookup, so the `unsafe` here is
//! unavoidable: the library must actually export the symbols at the signatures
//! declared in `abi`, which is the contract between this crate and the bridge it
//! is built against. The module stays mapped for the life of the process — see
//! [`RuntimeApi`] — so every resolved pointer stays valid for as long as anything
//! can call it.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_uint};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;

use crate::abi::{ABI_VERSION, WaterWpePollFd, WaterWpeReadiness, WaterWpeRuntime, WpeApi};
use crate::pump::{PumpDeadline, WpePollFd, WpeReadiness};

/// How many descriptors a readiness report is asked for before it has to ask
/// again with a larger buffer.
///
/// A WPE main context watches its own wakeup descriptor plus a handful per web
/// and network process, so this is sized to answer in one call and grows from
/// the count the bridge reports when it does not.
const INITIAL_DESCRIPTOR_CAPACITY: usize = 16;

/// Exact WPE `WebKit` line used by the bundled runtime.
pub const WPE_WEBKIT_VERSION: &str = "2.52.5";

#[derive(Deserialize)]
struct RuntimeIdentity {
    engine: String,
    version: String,
    platform: String,
    architecture: String,
}

/// Resolved locations inside one staged WPE runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpeRuntimePaths {
    root: PathBuf,
}

impl WpeRuntimePaths {
    /// Creates paths for an explicitly staged WPE runtime root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolves the runtime staged next to the executable by `water`.
    ///
    /// # Panics
    ///
    /// Panics when the current executable path cannot be resolved.
    #[must_use]
    pub fn packaged() -> Self {
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("failed to resolve executable for WPE: {error}"));
        let executable_dir = executable
            .parent()
            .expect("WPE executable path must have a parent directory");
        Self::new(executable_dir.join("waterui-browser/wpe"))
    }

    /// Returns the runtime root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn bridge(&self) -> PathBuf {
        self.root.join("lib/libwaterui_wpe.so")
    }

    fn validate(&self) {
        let bridge = self.bridge();
        assert!(
            bridge.is_file(),
            "bundled WPE bridge is missing at {}. Package or run the app through `water` so WPE WebKit {WPE_WEBKIT_VERSION} is staged",
            bridge.display()
        );
        let manifest = self.root.join("runtime.json");
        assert!(
            manifest.is_file(),
            "bundled WPE runtime manifest is missing at {}",
            manifest.display()
        );
        let identity: RuntimeIdentity =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap_or_else(|error| {
                panic!(
                    "failed to read bundled WPE runtime manifest {}: {error}",
                    manifest.display()
                )
            }))
            .unwrap_or_else(|error| {
                panic!(
                    "invalid bundled WPE runtime manifest {}: {error}",
                    manifest.display()
                )
            });
        assert_eq!(identity.engine, "wpe", "bundled runtime engine mismatch");
        assert_eq!(
            identity.version, WPE_WEBKIT_VERSION,
            "bundled WPE WebKit version mismatch"
        );
        assert_eq!(
            identity.platform,
            std::env::consts::OS,
            "bundled WPE platform mismatch"
        );
        assert_eq!(
            identity.architecture,
            std::env::consts::ARCH,
            "bundled WPE architecture mismatch"
        );
        let licenses = self.root.join("licenses");
        assert!(
            licenses.is_dir(),
            "bundled WPE license directory is missing at {}",
            licenses.display()
        );
        for (tool, package) in [
            ("/usr/bin/bwrap", "bubblewrap >= 0.3.1"),
            ("/usr/bin/xdg-dbus-proxy", "xdg-dbus-proxy"),
        ] {
            assert!(
                Path::new(tool).is_file(),
                "bundled WPE sandbox requires {package} at {tool}"
            );
        }
    }
}

pub struct RuntimeApi {
    pub api: WpeApi,
    /// The bridge, mapped for the life of the process.
    ///
    /// It is deliberately never closed. `dlclose` on this module is unsound
    /// twice over: the bridge registers `GObject` types — `WaterView`,
    /// `WaterToplevel`, `WaterDisplay` — and `GLib`'s type system keeps the
    /// class and instance initializers it was handed forever, so unmapping the
    /// code they live in leaves the type system pointing into nothing; and WPE
    /// `WebKit` runs worker threads that no teardown call joins — `GStreamer`'s,
    /// WTF's, the vblank monitor — so unloading the engine out from under them
    /// stops whichever one was running. The smoke reported the second of those
    /// as a SIGSEGV at an unmapped address on a thread that was still running
    /// while the main thread dropped this handle.
    ///
    /// Dropping a `WpeRuntime` is still fine: the engine's threads keep running
    /// against code that is still mapped, and the process reclaims all of it on
    /// exit.
    _library: ManuallyDrop<libloading::Library>,
}

impl RuntimeApi {
    fn load(paths: &WpeRuntimePaths) -> Arc<Self> {
        paths.validate();
        let bridge = paths.bridge();
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        let library = unsafe { libloading::Library::new(&bridge) }.unwrap_or_else(|error| {
            panic!(
                "failed to load bundled WPE bridge {}: {error}",
                bridge.display()
            )
        });
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        let api = unsafe { WpeApi::load(&library) };
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        let version = unsafe { (api.abi_version)() };
        assert_eq!(
            version, ABI_VERSION,
            "bundled WPE bridge ABI mismatch: runtime={version}, WaterUI={ABI_VERSION}"
        );
        Arc::new(Self {
            api,
            _library: ManuallyDrop::new(library),
        })
    }
}

// SAFETY: the bridge ABI explicitly permits calling the frame-completion
// functions from wgpu's completion thread, and the module stays mapped for the
// life of the process, so the function pointers stay valid.
unsafe impl Send for RuntimeApi {}
// SAFETY: see the `Send` impl — the API holds only function pointers into a module
// that stays mapped, with no interior mutability.
unsafe impl Sync for RuntimeApi {}

struct RuntimeInner {
    api: Arc<RuntimeApi>,
    raw: NonNull<WaterWpeRuntime>,
    /// Whether [`WpeRuntime::start_message_pump`] already spawned this runtime's
    /// pump task. Every controller that resolves this runtime asks for it, and
    /// one main context needs exactly one loop driving it.
    pump_running: Cell<bool>,
}

impl Drop for RuntimeInner {
    /// Frees the runtime, and says so on both sides of the call.
    ///
    /// `water_wpe_runtime_free` drains the main context before it releases the
    /// display, so the two events bracket every queued frame completion that
    /// had not run yet.
    fn drop(&mut self) {
        tracing::debug!("freeing the WPE runtime");
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        unsafe { (self.api.api.runtime_free)(self.raw.as_ptr()) };
        tracing::debug!("freed the WPE runtime");
    }
}

/// Main-thread WPE `WebKit` runtime.
#[derive(Clone)]
pub struct WpeRuntime {
    inner: Rc<RuntimeInner>,
}

impl core::fmt::Debug for WpeRuntime {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("WpeRuntime").finish_non_exhaustive()
    }
}

impl WpeRuntime {
    /// Loads and initializes the exact staged WPE runtime.
    ///
    /// One runtime is live per process at a time. WPE `WebKit` is a process-wide
    /// singleton — its web context, its `GLib` types and its display all belong to
    /// one runtime on the thread that owns its main context — so a second one is
    /// refused by the bridge rather than left to abort somewhere inside the
    /// engine. Dropping a [`WpeRuntime`] releases the claim; work that needs two
    /// belongs in two processes, which is why the real-engine tests run one test
    /// per process.
    ///
    /// # Panics
    ///
    /// Panics when the runtime is missing, invalid, ABI-incompatible, already
    /// live in this process, or fails to initialize.
    #[must_use]
    pub fn initialize(paths: &WpeRuntimePaths) -> Self {
        let api = RuntimeApi::load(paths);
        let mut error = std::ptr::null_mut::<c_char>();
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        let raw = unsafe { (api.api.runtime_new)(&raw mut error) };
        let raw = NonNull::new(raw).unwrap_or_else(|| {
            let message = take_error(&api, error);
            panic!("failed to initialize bundled WPE WebKit {WPE_WEBKIT_VERSION}: {message}")
        });
        assert!(
            error.is_null(),
            "WPE runtime initialized while also returning an error"
        );
        Self {
            inner: Rc::new(RuntimeInner {
                api,
                raw,
                pump_running: Cell::new(false),
            }),
        }
    }

    /// Dispatches every currently-ready GLib/WebKit task without blocking.
    ///
    /// Backends call this from their native event loop before rendering.
    #[must_use]
    pub fn iteration(&self) -> bool {
        // SAFETY: symbol resolved from the bridge library kept mapped by this
        // runtime; see the module safety note.
        unsafe { (self.inner.api.api.runtime_iteration)(self.inner.raw.as_ptr()) }
    }

    /// Reports what the runtime's `GLib` main context is waiting for.
    ///
    /// Nothing is dispatched: this asks every source when it next wants to run,
    /// so a host can schedule [`Self::iteration`] against the engine's own
    /// deadline and descriptors instead of an interval it invented.
    ///
    /// # Panics
    ///
    /// Panics when the bridge reports more descriptors than a `usize` holds.
    #[must_use]
    pub fn readiness(&self) -> WpeReadiness {
        let mut report = WaterWpeReadiness::default();
        let mut descriptors = vec![WaterWpePollFd::default(); INITIAL_DESCRIPTOR_CAPACITY];
        loop {
            let capacity = c_uint::try_from(descriptors.len())
                .expect("the WPE descriptor buffer is sized by this crate");
            // SAFETY: symbol resolved from the bridge library kept mapped by
            // this runtime; see the module safety note. The buffer is `capacity`
            // entries long, which is what the bridge is told it may write.
            let needed = unsafe {
                (self.inner.api.api.runtime_readiness)(
                    self.inner.raw.as_ptr(),
                    &raw mut report,
                    descriptors.as_mut_ptr(),
                    capacity,
                )
            };
            let needed = usize::try_from(needed)
                .expect("the WPE bridge reported more descriptors than a usize holds");
            if needed <= descriptors.len() {
                descriptors.truncate(needed);
                break;
            }
            // The bridge wrote nothing and answered with the size it needs; ask
            // again with a buffer that large, as `g_main_context_query` is used.
            descriptors.resize(needed, WaterWpePollFd::default());
        }
        WpeReadiness::new(
            report.ready,
            report.timeout_ms,
            descriptors
                .into_iter()
                .map(|descriptor| WpePollFd::new(descriptor.fd, descriptor.events))
                .collect(),
        )
    }

    /// Dispatches everything WPE has ready and reports when it wants the loop
    /// back.
    ///
    /// # Panics
    ///
    /// Panics when the bridge reports more descriptors than a `usize` holds.
    #[must_use]
    pub fn pump(&self) -> PumpDeadline {
        while self.iteration() {}
        self.readiness().deadline()
    }

    /// Drives this runtime's main loop from `WaterUI`'s UI executor.
    ///
    /// WPE's engine work runs on the thread that created the runtime — the UI
    /// thread — and it has to keep running whether or not anything is being
    /// drawn. A page only produces frames while its main context is iterated,
    /// and a renderer that skips idle frames stops drawing exactly when a page
    /// has nothing new to show, so a pump that lives inside rendering deadlocks
    /// the two against each other: no frame, no pump, no frame. Background
    /// timers, network completions and DOM mutations then wait for an unrelated
    /// event — a moved pointer — to force a frame. This task is therefore
    /// independent of every surface, and a page that wakes up asks for a redraw
    /// through the frame sink's waker.
    ///
    /// The loop is paced by `GLib` itself: [`Self::pump`] returns the instant the
    /// main context asked to be iterated at, and the task sleeps exactly that
    /// long. Calling this more than once for one runtime is a no-op, so every
    /// controller that resolves a runtime may ask for it.
    ///
    /// The task holds no strong reference to the runtime and ends when the last
    /// one is dropped.
    pub fn start_message_pump(&self) {
        if self.inner.pump_running.replace(true) {
            return;
        }
        let runtime: Weak<RuntimeInner> = Rc::downgrade(&self.inner);
        executor_core::spawn_local(async move {
            loop {
                let Some(inner) = runtime.upgrade() else {
                    return;
                };
                // The strong reference must not be held across the await, or
                // this task would keep WPE alive for the life of the process.
                let deadline = Self { inner }.pump().instant();
                futures_timer::Delay::new(deadline.saturating_duration_since(Instant::now())).await;
            }
        })
        .detach();
    }

    pub(super) fn raw(&self) -> NonNull<WaterWpeRuntime> {
        self.inner.raw
    }

    pub(super) fn api(&self) -> &Arc<RuntimeApi> {
        &self.inner.api
    }
}

pub fn take_error(api: &RuntimeApi, error: *mut c_char) -> String {
    assert!(!error.is_null(), "WPE failed without an error message");
    // SAFETY: symbol resolved from the bridge library kept mapped by this runtime;
    // see the module safety note.
    let message = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: symbol resolved from the bridge library kept mapped by this runtime;
    // see the module safety note.
    unsafe { (api.api.string_free)(error) };
    message
}
