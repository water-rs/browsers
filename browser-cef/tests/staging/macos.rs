//! The macOS application bundle CEF refuses to start without.
//!
//! `cef_initialize` traps inside the framework when the browser process is not
//! bundled, and its child processes are launched from helper applications
//! Chromium locates relative to the outer bundle, so a plain
//! `target/debug/deps/real_engine-*` can never initialize CEF. This module
//! stages the same layout `water package` produces — main bundle, staged
//! framework, runtime manifest and the five helper applications — and the test
//! binary re-executes itself from it.

use std::path::PathBuf;

use askama::Template;

use super::staged::{
    EXECUTABLE, clone_tree, create_directory, distribution, executable, link, runtime_identity,
    write,
};

const BUNDLE_IDENTIFIER: &str = "dev.waterui.browser-cef.real-engine";

const FRAMEWORK: &str = "Chromium Embedded Framework.framework";

/// The helper variants Chromium chooses between by child process type, as
/// `(name suffix, bundle identifier suffix)`.
///
/// All five exist for the same reason they do in a packaged application: the
/// GPU, renderer and alerts processes are launched from their own bundles, and
/// a missing one is a launch failure rather than a fallback.
const HELPER_VARIANTS: [(&str, &str); 5] = [
    ("", ""),
    (" (Alerts)", ".alerts"),
    (" (GPU)", ".gpu"),
    (" (Plugin)", ".plugin"),
    (" (Renderer)", ".renderer"),
];

#[derive(Template)]
#[template(path = "macos/CefHelperInfo.plist.tpl", escape = "none")]
struct HelperInfoPlist<'a> {
    bundle_identifier: &'a str,
    helper_name: &'a str,
    product_name: &'a str,
}

/// Whether this process is already running as the bundled executable.
///
/// The condition is the one CEF actually cares about — an executable inside
/// `Contents/MacOS` is what makes `NSBundle` report a bundle — rather than a
/// flag this test hands itself.
pub fn running_staged() -> bool {
    executable()
        .parent()
        .is_some_and(|directory| directory.ends_with("Contents/MacOS"))
}

/// Stages the bundle around a fresh copy of this executable and returns the
/// executable to run.
///
/// # Panics
///
/// Panics when the bundle cannot be written.
pub fn stage() -> PathBuf {
    let application = super::workspace().join(format!("{EXECUTABLE}.app"));
    if application.exists() {
        std::fs::remove_dir_all(&application).expect("clear the previously staged bundle");
    }
    let contents = application.join("Contents");
    let frameworks = contents.join("Frameworks");
    let staged_executable = contents.join("MacOS").join(EXECUTABLE);

    create_directory(&frameworks);
    create_directory(&contents.join("MacOS"));
    create_directory(&contents.join("Resources/waterui-browser/cef"));
    write(&contents.join("Info.plist"), include_str!("Info.plist"));
    write(&contents.join("PkgInfo"), "APPL????");
    link(&staged_executable);

    // The framework has to live inside the bundle for real. Chromium's child
    // processes load it *after* entering the seatbelt sandbox, which grants them
    // the bundle's own paths and nothing else, so a symlink out to the build
    // directory fails with `file system sandbox blocked open()` in every helper.
    // Hard links give the bundle real paths without copying 322 MB.
    clone_tree(
        &distribution(FRAMEWORK).join(FRAMEWORK),
        &frameworks.join(FRAMEWORK),
    );

    write(
        &contents.join("Resources/waterui-browser/cef/runtime.json"),
        &runtime_identity(),
    );

    for (name_suffix, identifier_suffix) in HELPER_VARIANTS {
        let helper_name = format!("{EXECUTABLE} Helper{name_suffix}");
        let helper_contents = frameworks
            .join(format!("{helper_name}.app"))
            .join("Contents");
        create_directory(&helper_contents.join("MacOS"));
        link(&helper_contents.join("MacOS").join(&helper_name));
        write(&helper_contents.join("PkgInfo"), "APPL????");
        write(
            &helper_contents.join("Info.plist"),
            &HelperInfoPlist {
                bundle_identifier: &format!("{BUNDLE_IDENTIFIER}.helper{identifier_suffix}"),
                helper_name: &helper_name,
                product_name: EXECUTABLE,
            }
            .render()
            .expect("render the CEF helper Info.plist"),
        );
    }

    staged_executable
}
