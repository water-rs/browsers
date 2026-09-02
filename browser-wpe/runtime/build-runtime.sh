#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <output-directory>" >&2
    exit 2
fi

# Everything this script reads is resolved from where the script itself is,
# rather than from a layout named inside it: the runtime directory is the one it
# lives in, and the crate is that directory's parent.
runtime_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_directory="$(cd "$runtime_directory/.." && pwd)"
# The two WaterUI licences the packaged runtime ships are the repository's own,
# so git is asked about this script's directory rather than about whichever one
# the caller happened to be standing in.
repo_root="$(git -C "$runtime_directory" rev-parse --show-toplevel)"
crate="waterui-browser-wpe"
manifest="$crate_directory/Cargo.toml"
source_configuration="$runtime_directory/source.toml"
output_directory="$(mkdir -p "$1" && cd "$1" && pwd)"

configuration_value() {
    local key="$1"
    sed -n "s/^${key} = \"\\([^\"]*\\)\"$/\\1/p" "$source_configuration"
}

version="$(configuration_value version)"
source_url="$(configuration_value source_url)"
source_sha256="$(configuration_value source_sha256)"
glib_dependencies_url="$(configuration_value glib_dependencies_url)"
glib_dependencies_sha256="$(configuration_value glib_dependencies_sha256)"
released="$(configuration_value released)"
maximum_glibc="$(configuration_value maximum_glibc)"
minimum_gcc="$(configuration_value minimum_gcc)"
smoke_timeout_seconds="$(configuration_value smoke_timeout_seconds)"

if [[ -z "$version" || -z "$source_url" || -z "$source_sha256" || -z "$glib_dependencies_url" || -z "$glib_dependencies_sha256" || -z "$released" || -z "$maximum_glibc" || -z "$minimum_gcc" || -z "$smoke_timeout_seconds" ]]; then
    echo "invalid WPE runtime source configuration" >&2
    exit 1
fi

# The pin every consumer reads is the crate's own metadata, so that the `water`
# CLI can take it from the published crate instead of keeping a copy of its own.
# It is read through `cargo metadata` because it lives in a TOML file, and a
# regex over TOML answers wrongly the first time the file is reformatted. The
# same answer carries the target directory the smoke example is built into,
# which is cargo's to decide and not this script's to assume.
crate_metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest")"
# `cargo metadata` answers for the whole workspace, so the crate is picked out by
# the same name its example is built from below.
pinned_version="$(printf '%s' "$crate_metadata" | python3 -c 'import json, sys; print(next(package for package in json.load(sys.stdin)["packages"] if package["name"] == sys.argv[1])["metadata"]["waterui"]["wpe-runtime"]["version"])' "$crate")"
target_directory="$(printf '%s' "$crate_metadata" | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"

if [[ "$pinned_version" != "$version" ]]; then
    echo "WPE runtime source version $version does not match the $crate pin $pinned_version" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64)
        architecture="x86_64"
        ;;
    aarch64)
        architecture="aarch64"
        ;;
    *)
        echo "WPE runtime artifacts require native x86_64 or aarch64 Linux" >&2
        exit 1
        ;;
esac

c_compiler="${CC:-cc}"
cxx_compiler="${CXX:-c++}"
command -v "$c_compiler" >/dev/null || {
    echo "WPE runtime C compiler is unavailable: $c_compiler" >&2
    exit 1
}
command -v "$cxx_compiler" >/dev/null || {
    echo "WPE runtime C++ compiler is unavailable: $cxx_compiler" >&2
    exit 1
}
compiler_banner="$($c_compiler --version | head -n 1)"
if [[ "$compiler_banner" != *gcc* && "$compiler_banner" != *GCC* ]]; then
    echo "WPE runtime requires GCC $minimum_gcc or newer; $c_compiler is $compiler_banner" >&2
    exit 1
fi
actual_gcc="$($c_compiler -dumpfullversion -dumpversion)"
oldest_gcc="$(printf '%s\n%s\n' "$actual_gcc" "$minimum_gcc" | sort -V | head -n 1)"
if [[ "$oldest_gcc" != "$minimum_gcc" ]]; then
    echo "WPE WebKit $version requires GCC $minimum_gcc or newer; $c_compiler reports $actual_gcc" >&2
    exit 1
fi

actual_glibc="$(getconf GNU_LIBC_VERSION | awk '{print $2}')"
highest_glibc="$(printf '%s\n%s\n' "$actual_glibc" "$maximum_glibc" | sort -V | tail -n 1)"
if [[ "$highest_glibc" != "$maximum_glibc" ]]; then
    echo "build host glibc $actual_glibc exceeds artifact floor $maximum_glibc" >&2
    exit 1
fi

# Compiling WPE WebKit takes hours, and nothing about it depends on this
# crate's own sources. A caller that can keep the unpacked release and the
# installed prefix between runs — CI restores them from a cache keyed on the
# pin and this script — names that directory in
# `WATERUI_WPE_WORK_DIRECTORY`, and the phases below reuse whatever of it is
# already there. Unset, the build happens in a temporary directory that goes
# away with the script, which is what a one-off invocation wants.
if [[ -n "${WATERUI_WPE_WORK_DIRECTORY:-}" ]]; then
    work_directory="$(mkdir -p "$WATERUI_WPE_WORK_DIRECTORY" && cd "$WATERUI_WPE_WORK_DIRECTORY" && pwd)"
else
    work_directory="$(mktemp -d)"
    trap 'rm -rf "$work_directory"' EXIT
fi
archive="$work_directory/wpewebkit.tar.xz"
source_directory="$work_directory/wpewebkit-$version"
build_directory="$work_directory/build"
prefix="$work_directory/runtime"
source_identity="$work_directory/webkit-source-identity"

# Ninja and CMake both decide what to redo from file timestamps, so a source
# tree that is already the pinned release is left untouched: re-extracting it
# would restamp every file. The identity written beside it is the digest the
# tree was unpacked from, so changing the pin still replaces it.
if [[ "$(cat "$source_identity" 2>/dev/null)" != "$source_sha256" ]]; then
    rm -rf "$source_directory" "$source_identity"
    curl --fail --location --retry 3 --output "$archive" "$source_url"
    printf '%s  %s\n' "$source_sha256" "$archive" | sha256sum --check
    tar -xJf "$archive" -C "$work_directory"
    rm -f "$archive"
    printf '%s\n' "$source_sha256" > "$source_identity"
fi

# `Tools/wpe/dependencies/apt` sources this file and the release tarball does
# not ship it, so upstream's own installer exits before installing anything.
# Restore it from the tag the tarball was cut from rather than keeping a second,
# hand-copied dependency list in this repository.
glib_dependencies="$source_directory/Tools/glib/dependencies/apt"
if [[ ! -f "$glib_dependencies" ]]; then
    mkdir -p "$(dirname "$glib_dependencies")"
    curl --fail --location --retry 3 --output "$glib_dependencies" "$glib_dependencies_url"
    printf '%s  %s\n' "$glib_dependencies_sha256" "$glib_dependencies" | sha256sum --check
    chmod +x "$glib_dependencies"
fi

# Upstream's list mixes developer tooling in with the build dependencies, and
# two entries make the whole apt transaction unsatisfiable on a current CI
# image:
#   * `git-svn` pins `git (< 1:2.34.1-.)`, while GitHub's runner images install
#     git from a PPA — 2.55 against 22.04's 2.34. It is tooling for the SVN
#     workflow WebKit has long since left behind; nothing in this build reads
#     it. apt cannot install it, and one unusable package aborts everything
#     else with it.
#   * `libgstreamer1.0-dev` needs `libunwind-dev`, which apt does not pull in
#     on its own here.
# Drop the first from the list and install the second alongside our own
# additions, so the installer fails only for reasons that actually matter.
sed -i '/git-svn/d' \
    "$glib_dependencies" \
    "$source_directory/Tools/wpe/dependencies/apt"

# Before upstream's installer, not after: it is `libgstreamer1.0-dev` inside
# that installer's own list that needs this, so satisfying it afterwards is too
# late — the installer has already aborted the transaction.
sudo apt-get install -y --no-install-recommends libunwind-dev

sudo "$source_directory/Tools/wpe/install-dependencies"
sudo apt-get install -y --no-install-recommends \
    bubblewrap \
    cmake \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    ninja-build \
    patchelf \
    pax-utils \
    xdg-dbus-proxy

# Three options default on and cannot be satisfied on the host this artifact
# must be built on. All three are named here so the packaged runtime contains
# the same thing everywhere it is built, rather than whatever the build host's
# archive happened to carry.
#
#   * `USE_LIBBACKTRACE`: no Debian or Ubuntu release packages libbacktrace, so
#     configuring fails on every apt-based host. It only symbolizes WebKit's own
#     crash logs, which a shipped runtime does not print.
#   * `USE_JPEGXL`: `libjxl-dev` does not exist in Ubuntu 22.04, and 22.04 is
#     not a choice — `source.toml` pins the artifact's glibc floor at 2.35 and
#     this script refuses to package against a newer one, so every runtime a
#     user downloads is built where libjxl cannot be installed. Upstream treats
#     the package as optional in `Tools/glib/dependencies/apt`
#     (`$(aptIfExists libjxl-dev)`) while `Source/cmake/OptionsWPE.cmake` errors
#     out without it, which is how a missing optional package became a build
#     failure. JPEG XL decoding is what the runtime gives up; `libavif-dev` is
#     in 22.04, so AVIF is unaffected.
#   * `ENABLE_WPE_PLATFORM_DRM`: the KMS platform scans WPE's output out to a
#     physical display, and this runtime never does that. `native/waterui_wpe.c`
#     builds its display on `wpe_display_headless_new()` and exports every frame
#     as a DMA-BUF for `src/gpu.rs` to import into wgpu, so the only platform
#     backend it needs is `ENABLE_WPE_PLATFORM_HEADLESS`, which is a separate
#     option and stays on. Turning the KMS platform off drops
#     `Source/WebKit/WPEPlatform/wpe/drm` from the build, and with it the only
#     unguarded uses of `drmModeCreateDumbBuffer` / `drmModeDestroyDumbBuffer`
#     in the tree — libdrm 2.4.114 functions that 22.04's 2.4.113 does not
#     declare. `Source/cmake/OptionsWPE.cmake` probes for exactly those two
#     symbols, but `WPEScreenDRM.cpp` calls them without testing the result, so
#     the KMS platform simply does not compile against 2.4.113. The other call
#     site of a 2.4.114 symbol, `WebKitProtocolHandler.cpp`, is properly
#     `HAVE()`-guarded and is unaffected. `USE_GBM` and `USE_LIBDRM` stay on —
#     `ENABLE_GPU_PROCESS` requires them and the headless platform allocates its
#     buffers through GBM — so the DMA-BUF path is untouched.
webkit_options=(
    -DPORT=WPE
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX="$prefix"
    -DCMAKE_INSTALL_LIBDIR=lib
    -DCMAKE_INSTALL_LIBEXECDIR=libexec
    -DCMAKE_C_COMPILER="$c_compiler"
    -DCMAKE_CXX_COMPILER="$cxx_compiler"
    -DBWRAP_EXECUTABLE=/usr/bin/bwrap
    -DDBUS_PROXY_EXECUTABLE=/usr/bin/xdg-dbus-proxy
    -DENABLE_API_TESTS=OFF
    -DENABLE_BUBBLEWRAP_SANDBOX=ON
    -DENABLE_DOCUMENTATION=OFF
    -DENABLE_INTROSPECTION=OFF
    -DENABLE_JOURNALD_LOG=OFF
    -DENABLE_LAYOUT_TESTS=OFF
    -DENABLE_MINIBROWSER=OFF
    -DENABLE_WPE_LEGACY_API=OFF
    -DENABLE_WPE_PLATFORM=ON
    -DENABLE_WPE_PLATFORM_DRM=OFF
    -DUSE_JPEGXL=OFF
    -DUSE_LIBBACKTRACE=OFF
)

# What the installed prefix contains is decided by the release it was built
# from, the options above and the compiler that read them, so those three are
# what the prefix is stamped with. A work directory carried over from an
# earlier run whose stamp still matches already holds this exact build, and the
# hours go to whatever comes after it instead — the bridge, the packaging and
# the smoke run, which is where a change to this crate lands.
webkit_identity="$(printf '%s\n' "$source_sha256" "$actual_gcc" "${webkit_options[@]}" | sha256sum | cut -d ' ' -f 1)"
webkit_installed="$work_directory/webkit-build-identity"
installed_snapshot="$work_directory/install"
if [[ "$(cat "$webkit_installed" 2>/dev/null)" != "$webkit_identity" ]]; then
    rm -rf "$prefix" "$installed_snapshot" "$webkit_installed"
    cmake -S "$source_directory" -B "$build_directory" -G Ninja "${webkit_options[@]}"
    cmake --build "$build_directory" --parallel "${WATERUI_WPE_BUILD_JOBS:-$(nproc)}"
    cmake --install "$build_directory"
    # The object tree is nine thousand compilations and tens of gigabytes, and
    # nothing past this point reads it: the bridge compiles against the
    # installed prefix and the packaging copies out of it. Dropping it here is
    # what makes the work directory small enough to keep between runs.
    rm -rf "$build_directory"
    cp -a "$prefix" "$installed_snapshot"
    # Written last, so a stamped work directory is one that holds the whole of
    # this phase's output and not some interrupted part of it.
    printf '%s\n' "$webkit_identity" > "$webkit_installed"
fi

# Packaging stages into the prefix: it copies every system library the runtime
# links into it, rewrites the RPATH of every binary in it and writes the
# licences and metadata that the smoke run and the archive are then read from.
# The tree it is handed therefore has to be the installer's own output, which
# is what the snapshot beside it holds — a second run against a prefix the
# first one already packaged collides with its own copies. Restoring it to the
# same path is what keeps it honest: whatever the installer recorded in those
# binaries still resolves to where it was recorded.
rm -rf "$prefix"
cp -a "$installed_snapshot" "$prefix"

bridge_build_directory="$work_directory/bridge-build"
PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig" \
cmake \
    -S "$crate_directory/native" \
    -B "$bridge_build_directory" \
    -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCMAKE_C_COMPILER="$c_compiler" \
    -DCMAKE_CXX_COMPILER="$cxx_compiler" \
    -DCMAKE_PREFIX_PATH="$prefix"
cmake --build "$bridge_build_directory"
cmake --install "$bridge_build_directory"

python3 "$runtime_directory/package-runtime.py" \
    --architecture "$architecture" \
    --maximum-glibc "$maximum_glibc" \
    --output "$output_directory" \
    --prefix "$prefix" \
    --released "$released" \
    --repository "$repo_root" \
    --source "$source_directory" \
    --version "$version"

cargo build \
    --manifest-path "$manifest" \
    --package "$crate" \
    --example runtime_smoke \
    --features runtime-smoke
smoke_binary="$target_directory/debug/examples/runtime_smoke"
archive="$output_directory/waterui-wpe-$version-linux-$architecture.zip"
snapshot="$output_directory/wpe-smoke-$version-linux-$architecture.png"
metrics="$output_directory/wpe-smoke-$version-linux-$architecture.json"
python3 "$runtime_directory/run-smoke.py" \
    --archive "$archive" \
    --binary "$smoke_binary" \
    --metrics "$metrics" \
    --runtime "$prefix" \
    --snapshot "$snapshot" \
    --timeout-seconds "$smoke_timeout_seconds"
