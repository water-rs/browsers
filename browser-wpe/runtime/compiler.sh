# shellcheck shell=bash
# Sourced by build-webkit.sh and build-runtime.sh: both compile against the
# same toolchain, so the gate that picks it lives here once.
#
# select_wpe_compiler <minimum_clang> <webkit_version> sets c_compiler,
# cxx_compiler and gcc_install_dir, or exits with the reason it cannot.
# The three are the caller's, read after this returns.
# shellcheck disable=SC2034
select_wpe_compiler() {
    local minimum_clang="$1" version="$2" compiler compiler_banner actual_clang oldest_clang
    c_compiler="${CC:-cc}"
    cxx_compiler="${CXX:-c++}"
    for compiler in "$c_compiler" "$cxx_compiler"; do
        command -v "$compiler" >/dev/null || {
            echo "WPE runtime compiler is unavailable: $compiler" >&2
            exit 1
        }
        compiler_banner="$("$compiler" --version | head -n 1)"
        if [[ "$compiler_banner" != *clang* && "$compiler_banner" != *Clang* ]]; then
            echo "WPE runtime requires Clang $minimum_clang or newer; $compiler is $compiler_banner" >&2
            exit 1
        fi
    done
    # Clang prints its full version for -dumpversion and has no
    # -dumpfullversion before LLVM 19.
    actual_clang="$("$c_compiler" -dumpversion)"
    oldest_clang="$(printf '%s\n%s\n' "$actual_clang" "$minimum_clang" | sort -V | head -n 1)"
    if [[ "$oldest_clang" != "$minimum_clang" ]]; then
        echo "WPE WebKit $version requires Clang $minimum_clang or newer; $c_compiler reports $actual_clang" >&2
        exit 1
    fi

    # Clang keeps WebKit on libstdc++, and the libstdc++ the packaged runtime
    # ships is whichever one Clang picks — it links the newest GCC installation
    # it finds under /usr/lib/gcc, which is a property of the host and not of
    # CC. `package-runtime.py` bundles the resolved libstdc++.so.6 into the
    # artifact, so that choice is the artifact's C++ runtime floor. The floor is
    # GCC 12's and stays it: the installation is named explicitly through
    # --gcc-install-dir rather than left to whichever GCC the next runner image
    # happens to carry.
    command -v gcc-12 >/dev/null || {
        echo "WPE runtime compiles against GCC 12's libstdc++ and gcc-12 is unavailable" >&2
        exit 1
    }
    gcc_install_dir="$(dirname "$(gcc-12 -print-libgcc-file-name)")"
}
