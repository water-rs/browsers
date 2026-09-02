//! Builds the platform CEF application bridge.

use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=native/macos_application.mm");
    let cef_runtime_enabled = std::env::var_os("CARGO_FEATURE_CEF_RUNTIME").is_some();
    let compile_only_enabled = std::env::var_os("CARGO_FEATURE_COMPILE_ONLY").is_some();
    // The two features describe incompatible builds, and the earliest thing that
    // can say so is this script: it runs before the crate is compiled, and it is
    // the step whose behaviour they disagree about. Cargo fails the build on
    // `cargo::error` without a panic's noise around the message.
    if cef_runtime_enabled && compile_only_enabled {
        println!(
            "cargo::error=the `cef-runtime` and `compile-only` features are mutually exclusive: \
             `compile-only` builds against CEF's documentation stubs and stages no binary \
             distribution, while `cef-runtime` links and loads one. `cef-runtime` is a default \
             feature, so select the compile-only build with `--no-default-features --features \
             compile-only` rather than adding `compile-only` on top of the defaults, and never \
             with `--all-features`."
        );
        return;
    }
    if !cef_runtime_enabled {
        return;
    }

    // The real-engine tests drive the very distribution `cef-dll-sys` downloaded
    // and this crate linked against, so its path is compiled in rather than
    // configured by hand: a runtime that does not match what was linked is not a
    // thing a test should be able to be pointed at.
    println!(
        "cargo::rustc-env=WATERUI_CEF_DISTRIBUTION={}",
        cef_directory().display()
    );

    // macOS is the only platform with something to build here. Windows used to
    // compile a `CefScopedSandboxInfo` bridge and link `cef_sandbox.lib`, and
    // CEF 150 ships that library in no windows64 distribution: the sandbox now
    // lives inside CEF's own `bootstrap.exe`, which builds it for a client DLL
    // instead of letting an executable create one (water-rs/browsers#17).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_macos_application();
    }
}

fn cef_directory() -> PathBuf {
    PathBuf::from(
        std::env::var("DEP_CEF_DLL_WRAPPER_CEF_DIR")
            .expect("cef-dll-sys did not expose its CEF distribution directory"),
    )
}

fn build_macos_application() {
    let cef_directory = cef_directory();
    cc::Build::new()
        .cpp(true)
        .file("native/macos_application.mm")
        .flag("-isystem")
        .flag(cef_directory)
        .flag_if_supported("-std=c++20")
        .warnings_into_errors(true)
        .compile("waterui_cef_macos_application");
    println!("cargo::rustc-link-lib=framework=AppKit");
}
