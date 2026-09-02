#!/usr/bin/env python3

import argparse
import json
import os
import pathlib
import resource
import shutil
import subprocess
import time

# Stopping at the GLib critical rather than at whatever it later corrupts is the
# difference between a stack that names a defect and a bare `-11`: a critical
# from the runtime is a defect either way, so the smoke does not let one pass.
SMOKE_G_DEBUG = "fatal-criticals"


def process_tree(root: int) -> set[int]:
    discovered = set()
    pending = [root]
    while pending:
        process = pending.pop()
        if process in discovered:
            continue
        discovered.add(process)
        children = pathlib.Path(f"/proc/{process}/task/{process}/children")
        try:
            pending.extend(int(value) for value in children.read_text().split())
        except FileNotFoundError:
            pass
    return discovered


def resident_bytes(process: int) -> int:
    status = pathlib.Path(f"/proc/{process}/status")
    try:
        for line in status.read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except FileNotFoundError:
        pass
    return 0


def directory_bytes(root: pathlib.Path) -> int:
    return sum(
        path.stat().st_size
        for path in root.rglob("*")
        if path.is_file() and not path.is_symlink()
    )


def smoke_environment() -> dict[str, str]:
    environment = dict(os.environ)
    environment.setdefault("G_DEBUG", SMOKE_G_DEBUG)
    return environment


def allow_core_dumps() -> None:
    """Lets the smoke leave a core behind.

    A crash that only happens at full speed cannot be caught by running the
    thing again under a debugger — the debugger is the reason it stops
    happening. A core is the same evidence taken without touching the timing.
    """
    _, hard = resource.getrlimit(resource.RLIMIT_CORE)
    resource.setrlimit(resource.RLIMIT_CORE, (hard, hard))


def core_for(pid: int, executable: pathlib.Path) -> pathlib.Path | None:
    """Where this kernel put the core for `pid`, if it put one anywhere.

    The location is the kernel's to decide, so it is read from the kernel
    rather than assumed: a pattern beginning with `|` hands the core to a
    handler and leaves nothing on disk, and a pattern using specifiers beyond
    `%p` and `%e` is one this does not claim to resolve.
    """
    pattern = pathlib.Path("/proc/sys/kernel/core_pattern").read_text().strip()
    if pattern.startswith("|"):
        print(f"cores go to a handler ({pattern}); none is on disk", flush=True)
        return None
    name = pattern.replace("%p", str(pid)).replace("%e", executable.name)
    if "%" in name:
        print(f"unsupported core pattern {pattern}", flush=True)
        return None
    core = pathlib.Path(name)
    if not core.is_absolute():
        core = pathlib.Path.cwd() / core
    if not core.is_file():
        print(f"no core at {core}", flush=True)
        return None
    return core


def report_crash(command: list[str], environment: dict[str, str], pid: int) -> None:
    """Prints the stacks of the smoke that just died.

    The core is the first choice, because it is the crash that actually
    happened: it records the run that was not being watched. Running the smoke
    again under gdb is the fallback for when no core was written, and it is a
    weaker one — a race that only loses at full speed can win under a debugger,
    and then the rerun exits cleanly and says nothing.
    """
    debugger = shutil.which("gdb")
    if debugger is None:
        print("no gdb on PATH: the smoke crash has no stack to report", flush=True)
        return
    executable = pathlib.Path(command[0])
    core = core_for(pid, executable)
    if core is not None:
        print(f"the smoke died on a signal; reading {core}", flush=True)
        subprocess.run(
            [
                debugger,
                "-batch",
                "-nx",
                "-ex",
                "thread apply all bt",
                str(executable),
                str(core),
            ],
            check=False,
        )
        return
    print(
        "the smoke died on a signal and left no core; running it again under gdb, "
        "which may not reproduce a race",
        flush=True,
    )
    subprocess.run(
        [
            debugger,
            "-batch",
            "-nx",
            "-ex",
            "run",
            "-ex",
            "thread apply all bt full",
            "--args",
            *command,
        ],
        env=environment,
        check=False,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=pathlib.Path)
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--metrics", required=True, type=pathlib.Path)
    parser.add_argument("--runtime", required=True, type=pathlib.Path)
    parser.add_argument("--snapshot", required=True, type=pathlib.Path)
    parser.add_argument("--timeout-seconds", required=True, type=int)
    arguments = parser.parse_args()

    identity = json.loads((arguments.runtime / "runtime.json").read_text())
    started = time.monotonic_ns()
    command = [
        str(arguments.binary),
        str(arguments.runtime),
        str(arguments.snapshot),
        str(arguments.timeout_seconds),
    ]
    environment = smoke_environment()
    process = subprocess.Popen(command, env=environment, preexec_fn=allow_core_dumps)
    peak_resident_bytes = 0
    peak_process_count = 0
    while process.poll() is None:
        processes = process_tree(process.pid)
        peak_resident_bytes = max(
            peak_resident_bytes,
            sum(resident_bytes(child) for child in processes),
        )
        peak_process_count = max(peak_process_count, len(processes))
        try:
            process.wait(timeout=0.01)
        except subprocess.TimeoutExpired:
            pass
    if process.returncode != 0:
        if process.returncode < 0:
            report_crash(command, environment, process.pid)
        raise RuntimeError(
            f"WPE runtime smoke process exited with {process.returncode}"
        )
    if not arguments.snapshot.is_file():
        raise RuntimeError("WPE runtime smoke did not produce its GPU snapshot")

    metrics = {
        "schema_version": 1,
        "engine": identity["engine"],
        "version": identity["version"],
        "platform": identity["platform"],
        "architecture": identity["architecture"],
        "archive_bytes": arguments.archive.stat().st_size,
        "uncompressed_runtime_bytes": directory_bytes(arguments.runtime),
        "peak_process_tree_rss_bytes": peak_resident_bytes,
        "peak_process_count": peak_process_count,
        "duration_milliseconds": (time.monotonic_ns() - started) // 1_000_000,
        "snapshot": arguments.snapshot.name,
    }
    arguments.metrics.write_text(
        json.dumps(metrics, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
