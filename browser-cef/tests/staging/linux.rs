//! The flat runtime CEF resolves beside the executable on Linux.
//!
//! `CefRuntimePaths::packaged` takes the executable's own directory as the
//! runtime root here, so a test running from `target/debug/deps` has no runtime
//! at all: `libcef.so`, `icudtl.dat` and `locales/` live in the distribution
//! `cef-dll-sys` downloaded, and nothing puts them next to the test binary.
//! This module stages the same flat layout `water package` produces — every
//! runtime file from the distribution, the runtime directories, and the
//! manifest under `waterui-browser/cef` — around a hard link to this
//! executable, and the checks re-run from there.

use std::path::PathBuf;

use super::staged::{
    EXECUTABLE, clone_tree, create_directory, distribution, executable, hard_link, link,
    runtime_identity, write,
};

/// The runtime library `CefRuntimePaths::library` resolves beside the
/// executable, and the one member of the distribution this staging cannot do
/// without.
const LIBRARY: &str = "libcef.so";

/// Where the manifest `CefRuntimePaths::manifest` reads sits, relative to the
/// executable.
const MANIFEST: &str = "waterui-browser/cef/runtime.json";

/// Distribution files that belong to building against CEF rather than running
/// it. `water package` stages every other file flat, and so does this.
const NON_RUNTIME_FILES: [&str; 3] = ["archive.json", "CMakeLists.txt", "CREDITS.html"];

/// Distribution directories that are part of the runtime. The rest — `include`,
/// `cmake`, `libcef_dll` — are the sources for building against CEF and have no
/// place beside a running executable.
const RUNTIME_DIRECTORIES: [&str; 2] = ["locales", "swiftshader"];

/// The staged runtime directory, which is also the directory the checks run
/// from.
fn root() -> PathBuf {
    super::workspace().join("runtime")
}

/// Whether this process is already running from the staged runtime.
///
/// The condition is the one CEF actually cares about — `packaged` resolves the
/// runtime from the directory the process was executed with — rather than a
/// flag this test hands itself.
pub fn running_staged() -> bool {
    executable().parent() == Some(root().as_path())
}

/// Stages the runtime around a fresh link to this executable and returns the
/// executable to run.
///
/// # Panics
///
/// Panics when the runtime cannot be written.
pub fn stage() -> PathBuf {
    let root = root();
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear the previously staged runtime");
    }
    create_directory(&root);

    let distribution = distribution(LIBRARY);
    let entries = std::fs::read_dir(&distribution)
        .unwrap_or_else(|error| panic!("read {}: {error}", distribution.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|error| panic!("read {}: {error}", distribution.display()));
        let source = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if source.is_dir() {
            if RUNTIME_DIRECTORIES.contains(&name.as_ref()) {
                clone_tree(&source, &root.join(name.as_ref()));
            }
            continue;
        }
        if NON_RUNTIME_FILES.contains(&name.as_ref()) {
            continue;
        }
        hard_link(&source, &root.join(name.as_ref()));
    }

    let manifest = root.join(MANIFEST);
    create_directory(
        manifest
            .parent()
            .expect("the staged manifest path has a parent directory"),
    );
    write(&manifest, &runtime_identity());

    let staged_executable = root.join(EXECUTABLE);
    link(&staged_executable);
    staged_executable
}
