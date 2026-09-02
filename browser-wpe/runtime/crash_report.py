#!/usr/bin/env python3

"""Stacks for a child that died on a signal, read from the core it left.

Both halves of the WPE runtime checks need this — the smoke at the end of
`build-runtime.sh` and the real-engine test binaries — and neither can get the
same answer by running the thing again under a debugger: the failures that reach
here are races, and a debugger is exactly the thing that perturbs them away.

Where cores land is the kernel's decision, so it is read back from
`kernel.core_pattern` rather than assumed. A pattern that pipes to a handler
leaves nothing on disk, and that is reported rather than passed over in silence.
"""

import pathlib
import resource
import shutil
import subprocess

CORE_PATTERN = pathlib.Path("/proc/sys/kernel/core_pattern")


def allow_core_dumps() -> None:
    """Raises this process's core limit to its ceiling; children inherit it."""
    _, hard = resource.getrlimit(resource.RLIMIT_CORE)
    resource.setrlimit(resource.RLIMIT_CORE, (hard, hard))


def _pattern() -> str | None:
    try:
        pattern = CORE_PATTERN.read_text().strip()
    except OSError as error:
        print(f"cannot read {CORE_PATTERN}: {error}", flush=True)
        return None
    if pattern.startswith("|"):
        print(f"cores go to a handler ({pattern}); none is on disk", flush=True)
        return None
    return pattern


def _executable_of(core: pathlib.Path) -> pathlib.Path | None:
    """The binary the core came from, when the pattern's `%E` recorded it.

    `%E` is the executable's path with every `/` written as `!`, so the name
    carries back the one thing gdb needs and the core does not name.
    """
    encoded = core.name.find("!")
    if encoded == -1:
        return None
    end = core.name.rfind(".")
    if end <= encoded:
        return None
    return pathlib.Path(core.name[encoded:end].replace("!", "/"))


def cores_since(started: float) -> list[pathlib.Path]:
    """Every core written since `started`, oldest first."""
    pattern = _pattern()
    if pattern is None:
        return []
    template = pathlib.Path(pattern)
    directory = template.parent if pattern.startswith("/") else pathlib.Path.cwd()
    prefix = template.name.split("%")[0]
    if not prefix:
        print(f"core pattern {pattern} names no file to look for", flush=True)
        return []
    cores = [
        core
        for core in directory.glob(f"{prefix}*")
        if core.is_file() and core.stat().st_mtime >= started
    ]
    return sorted(cores, key=lambda core: core.stat().st_mtime)


def print_stacks(core: pathlib.Path, executable: pathlib.Path | None = None) -> None:
    """Prints every thread's stack from `core`, and what was still mapped.

    A frame in no library — `?? ()` at a bare address — means the code was
    unloaded, which is only readable next to the list of mappings.
    """
    debugger = shutil.which("gdb")
    if debugger is None:
        print(f"no gdb on PATH: {core} cannot be read", flush=True)
        return
    binary = executable or _executable_of(core)
    print(f"reading {core}" + (f" for {binary}" if binary else ""), flush=True)
    command = [
        debugger,
        "-batch",
        "-nx",
        "-ex",
        "thread apply all bt",
        "-ex",
        "info sharedlibrary",
        "-ex",
        "info proc mappings",
    ]
    if binary is not None:
        command.append(str(binary))
    command += ["--core", str(core)]
    subprocess.run(command, check=False)


def report_since(started: float) -> int:
    """Prints the stacks of every core written since `started`."""
    cores = cores_since(started)
    if not cores:
        print("no core was written for this crash", flush=True)
    for core in cores:
        print_stacks(core)
    return len(cores)
