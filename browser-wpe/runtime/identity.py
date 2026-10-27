#!/usr/bin/env python3
"""Prints the identity of the packaged WPE runtime this checkout builds.

The identity names the published runtime: `wpe-real-engine.yml` publishes the
archives for a checkout to the `wpe-runtime-<version>-<identity>` release, and
`waterui-browser-wpe` declares the same value as
`package.metadata.waterui.wpe-runtime.identity`, which is how a consumer that
resolved the package finds the runtime built for it.

It is a digest of every tracked input that decides what the archive contains:
the build and packaging scripts and the source pin under `runtime/`, the C
bridge under `native/`, the licenses the packager copies in, the action that
selects the compiler, and the workflow that provisions the build host. The
crate manifest is not an input — it carries the identity — and the WebKit
version it pins is checked against `runtime/source.toml`, which is one.

`--check` also fails unless the crate declares exactly this identity, so a
change to any input that does not update the declaration is caught before it
can name a runtime nobody built.
"""

import argparse
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys

INPUTS = (
    "browser-wpe/runtime",
    "browser-wpe/native",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    ".github/actions/llvm-apt-source",
    ".github/workflows/wpe-real-engine.yml",
)
CRATE = "waterui-browser-wpe"
LENGTH = 16


def git(root: pathlib.Path, *arguments: str) -> bytes:
    return subprocess.run(
        ("git", "-C", str(root), *arguments), check=True, stdout=subprocess.PIPE
    ).stdout


def identity(root: pathlib.Path) -> str:
    """Path, kind and content of each input file as the working tree holds it,
    so an edit counts before it is staged; files git ignores do not count."""
    listed = git(
        root, "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *INPUTS
    )
    digest = hashlib.sha256()
    for name in sorted({entry for entry in listed.split(b"\0") if entry}):
        path = root / os.fsdecode(name)
        if not os.path.lexists(path):
            continue
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            kind, content = b"l", os.fsencode(os.readlink(path))
        else:
            kind, content = (b"x" if mode & stat.S_IXUSR else b"f"), path.read_bytes()
        digest.update(name + b"\0" + kind + b"\0")
        digest.update(len(content).to_bytes(8, "big") + content)
    return digest.hexdigest()[:LENGTH]


def declared(root: pathlib.Path) -> str:
    metadata = json.loads(
        subprocess.run(
            (
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
                str(root / "browser-wpe/Cargo.toml"),
            ),
            check=True,
            stdout=subprocess.PIPE,
        ).stdout
    )
    package = next(entry for entry in metadata["packages"] if entry["name"] == CRATE)
    return package["metadata"]["waterui"]["wpe-runtime"]["identity"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help=f"fail unless {CRATE} declares this identity",
    )
    arguments = parser.parse_args()
    root = pathlib.Path(git(pathlib.Path(__file__).parent, "rev-parse", "--show-toplevel").decode().strip())
    computed = identity(root)
    if arguments.check:
        expected = declared(root)
        if expected != computed:
            sys.exit(
                f"{CRATE} declares WPE runtime identity {expected}, but its inputs "
                f"hash to {computed}: set `identity = \"{computed}\"` under "
                "[package.metadata.waterui.wpe-runtime] in browser-wpe/Cargo.toml"
            )
    print(computed)


if __name__ == "__main__":
    main()
