//! The runtime layout the CEF real-engine checks have to run from.
//!
//! Chromium launches its GPU, network and renderer processes by re-executing
//! this same test binary with a `--type=` argument on every platform, so
//! [`is_child_process`] is what tells one of those children from the browser
//! process.
//!
//! Neither macOS nor Linux can initialize CEF from the bare
//! `target/debug/deps/real_engine-*` cargo builds, and they cannot for
//! different reasons: `cef_initialize` traps inside the framework when the
//! macOS browser process is not bundled, while on Linux
//! `CefRuntimePaths::packaged` takes the executable's own directory as the
//! runtime root, and nothing puts `libcef.so`, `icudtl.dat` or `locales/`
//! there. Both answers are the layout `water package` produces for that
//! platform — [`macos`] stages an application bundle, [`linux`] a flat runtime
//! directory — and both re-run the checks from a hard link to this executable
//! placed inside it.
//!
//! The staged files are hard links rather than copies: the distribution is
//! hundreds of megabytes, and a link is indistinguishable from a copy to
//! `NSBundle`, to the dynamic loader and to Chromium, all of which resolve
//! their paths from the path the process was executed with.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{running_staged, stage};
#[cfg(target_os = "macos")]
pub use macos::{running_staged, stage};

/// Whether this process was launched by Chromium as one of its child processes.
pub fn is_child_process() -> bool {
    std::env::args().any(|argument| argument.starts_with("--type="))
}

/// Everything this test writes, inside the build directory cargo already owns.
pub fn workspace() -> PathBuf {
    PathBuf::from(env!("OUT_DIR")).join("real-engine")
}

/// What the two staging platforms share.
///
/// Both hard-link this executable and pieces of the CEF distribution into a
/// directory of their own and write the manifest `CefRuntimePaths::validate`
/// reads; only the layout around those files differs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod staged {
    use std::path::{Path, PathBuf};

    use serde::Serialize;

    /// The name every staged executable derives from.
    ///
    /// Chromium computes a macOS helper's path from the main executable's file
    /// name, so this has to match `CFBundleExecutable` in `Info.plist`.
    pub const EXECUTABLE: &str = "waterui-cef-real-engine";

    /// The manifest `CefRuntimePaths::validate` reads to confirm the staged
    /// runtime is the one this build links against.
    #[derive(Serialize)]
    struct RuntimeIdentity<'a> {
        schema_version: u32,
        engine: &'a str,
        version: String,
        platform: &'a str,
        architecture: &'a str,
    }

    /// The CEF distribution `cef-dll-sys` downloaded and this crate linked
    /// against.
    ///
    /// `member` is what the calling platform stages out of it, and its absence
    /// means the build directory the distribution came from was removed after
    /// this test was linked.
    ///
    /// # Panics
    ///
    /// Panics when the compiled-in distribution is not on disk.
    pub fn distribution(member: &str) -> PathBuf {
        let distribution = PathBuf::from(env!("WATERUI_CEF_DISTRIBUTION"));
        let staged = distribution.join(member);
        assert!(
            staged.exists(),
            "the CEF distribution this test linked against is gone: {} does not exist. It is \
             downloaded by `cef-dll-sys` into the build directory, so rebuild with `cargo test -p \
             waterui-browser-cef --features real-engine --test real_engine` rather than running a \
             stale binary.",
            staged.display()
        );
        distribution
    }

    /// The manifest body identifying the runtime this build links against.
    ///
    /// # Panics
    ///
    /// Panics when the identity cannot be serialized.
    pub fn runtime_identity() -> String {
        let identity = RuntimeIdentity {
            schema_version: 1,
            engine: "cef",
            version: format!(
                "{}.{}.{}",
                cef::sys::CEF_VERSION_MAJOR,
                cef::sys::CEF_VERSION_MINOR,
                cef::sys::CEF_VERSION_PATCH
            ),
            platform: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
        };
        serde_json::to_string_pretty(&identity).expect("serialize the staged runtime identity")
    }

    /// This test executable, as the path it was executed with.
    ///
    /// # Panics
    ///
    /// Panics when the executable path cannot be resolved.
    pub fn executable() -> PathBuf {
        std::env::current_exe().expect("resolve this test executable")
    }

    /// # Panics
    ///
    /// Panics when the directory cannot be created.
    pub fn create_directory(path: &Path) {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    }

    /// # Panics
    ///
    /// Panics when the file cannot be written.
    pub fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }

    /// # Panics
    ///
    /// Panics when the link cannot be created.
    pub fn hard_link(from: &Path, to: &Path) {
        std::fs::hard_link(from, to).unwrap_or_else(|error| {
            panic!("link {} into {}: {error}", from.display(), to.display())
        });
    }

    /// Hard-links this executable to `path`, which is how a staged layout runs
    /// the very binary cargo built without copying it.
    pub fn link(path: &Path) {
        hard_link(&executable(), path);
    }

    /// Reproduces a directory tree as real directories and hard-linked files.
    ///
    /// # Panics
    ///
    /// Panics when the tree cannot be read or reproduced.
    pub fn clone_tree(from: &Path, to: &Path) {
        create_directory(to);
        let entries = std::fs::read_dir(from)
            .unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
            let source = entry.path();
            let destination = to.join(entry.file_name());
            let kind = entry
                .file_type()
                .unwrap_or_else(|error| panic!("stat {}: {error}", source.display()));
            if kind.is_dir() {
                clone_tree(&source, &destination);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&source)
                    .unwrap_or_else(|error| panic!("read link {}: {error}", source.display()));
                std::os::unix::fs::symlink(target, &destination)
                    .unwrap_or_else(|error| panic!("link {}: {error}", destination.display()));
            } else {
                hard_link(&source, &destination);
            }
        }
    }
}
