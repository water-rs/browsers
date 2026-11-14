//! The flat runtime CEF resolves beside the executable on Linux and Windows.
//!
//! `CefRuntimePaths::packaged` takes the executable's own directory as the
//! runtime root on both, so a test running from `target/debug/deps` has no
//! runtime at all. The runtime library — `libcef.so` on Linux, `libcef.dll` on
//! Windows — together with `icudtl.dat` and `locales/` lives in the
//! distribution `cef-dll-sys` downloaded, and the copy `cef-dll-sys` makes of
//! those files as it builds lands in the target directory, one level *above*
//! the `deps` directory cargo runs a test binary from. Neither platform looks
//! there.
//!
//! This module stages the same flat layout `water package` produces — every
//! runtime file from the distribution, the runtime directories, and the
//! manifest under `waterui-browser/cef` — around a hard link to this
//! executable, and the checks re-run from there.

use std::path::PathBuf;

use super::staged::{
    EXECUTABLE, clone_tree, create_directory, distribution, executable, hard_link, link,
    runtime_identity, write,
};

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

/// The runtime library `CefRuntimePaths::library` resolves beside the
/// executable, and the one member of the distribution this staging cannot do
/// without.
///
/// CEF names it `libcef` on both platforms and only the dynamic library suffix
/// differs, which is exactly how `CefRuntimePaths::library` resolves it.
fn library() -> String {
    format!("libcef{}", std::env::consts::DLL_SUFFIX)
}

/// The staged executable's file name.
///
/// Windows runs a file by its extension, so the link has to carry the `.exe`
/// the executable cargo built carries; Linux adds nothing. A macOS bundle
/// names its executable from `CFBundleExecutable` instead, which is why the
/// suffix belongs to this layout rather than to the shared name.
fn executable_name() -> String {
    format!("{EXECUTABLE}{}", std::env::consts::EXE_SUFFIX)
}

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

    let distribution = distribution(&library());
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

    let staged_executable = root.join(executable_name());
    link(&staged_executable);
    staged_executable
}
