#!/usr/bin/env python3

"""Runs a command with cores enabled, and reads the stacks out of any it leaves.

`cargo test` and `cargo nextest run` hand a failing test's status back without a
word about where it died: a test binary that takes a signal is reported as one,
and its output was captured by a harness that is no longer running. This wraps
the invocation so a crash still comes with every thread's stack, using the same
reporter the runtime smoke uses.
"""

import signal
import subprocess
import sys
import time

import crash_report


def main() -> None:
    command = sys.argv[1:]
    if not command:
        raise SystemExit("usage: run-with-crash-report.py <command> [argument ...]")
    # The limit is raised here rather than per child, because the process that
    # dies is usually a grandchild: cargo's test binary, not cargo.
    crash_report.allow_core_dumps()
    started = time.time()
    completed = subprocess.run(command, check=False)
    status = completed.returncode
    if status < 0:
        # `subprocess` reports a signal death as its negative number; the shell
        # convention is 128 plus the signal, which keeps 139 and 134 readable as
        # what they are in a CI log.
        died_from = signal.Signals(-status)
        print(f"{command[0]} died from {died_from.name}", flush=True)
        crash_report.report_since(started)
        status = 128 - status
    elif status != 0:
        crash_report.report_since(started)
    raise SystemExit(status)


if __name__ == "__main__":
    main()
