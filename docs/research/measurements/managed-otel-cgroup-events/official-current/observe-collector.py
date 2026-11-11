#!/usr/bin/env python3
"""Observe the one actual child of the unchanged committed Collector fixture."""
import errno
import json
import os
from pathlib import Path
import runpy
import select
import subprocess
import sys


def main():
    harness, collector, observer = map(Path, sys.argv[1:4])
    forwarded = sys.argv[4:]
    original = subprocess.Popen
    children = []
    before = sorted(os.listdir("/proc/self/fd"))

    class CollectorChild(original):
        def __init__(self, argv, *args, **kwargs):
            if children or Path(argv[0]).resolve() != collector.resolve():
                raise RuntimeError("only the fixed owned Collector child is admitted")
            super().__init__(argv, *args, **kwargs)
            # This direct child has no other waiter. Early exit remains a zombie
            # until this owner waits, so its PID cannot be reused before pidfd_open.
            self.owned_pidfd = os.pidfd_open(self.pid, 0)
            self.fact = {"pid": self.pid, "argv": argv, "pidfd_opened": True,
                         "pidfd_closed_before_harness_fd_check": False, "waits": []}
            children.append(self)

        def wait(self, *args, **kwargs):
            try:
                status = super().wait(*args, **kwargs)
            except subprocess.TimeoutExpired:
                self.fact["waits"].append({"timeout": True})
                raise
            if self.owned_pidfd is not None:
                ready = bool(select.select([self.owned_pidfd], [], [], 0)[0])
                try:
                    os.waitid(os.P_PIDFD, self.owned_pidfd, os.WEXITED | os.WNOHANG)
                    reaped = False
                except ChildProcessError as error:
                    reaped = error.errno == errno.ECHILD
                self.fact["waits"].append({"returncode": status, "pidfd_exit_ready": ready,
                                           "direct_child_reaped": reaped})
                os.close(self.owned_pidfd)
                self.owned_pidfd = None
                self.fact["pidfd_closed_before_harness_fd_check"] = True
            return status

    code = 1
    failure = None
    subprocess.Popen = CollectorChild
    try:
        sys.argv = [str(harness)] + forwarded
        runpy.run_path(str(harness), run_name="__main__")
        code = 0
    except SystemExit as result:
        code = result.code if isinstance(result.code, int) else 1
    except Exception as error:
        failure = {"type": type(error).__name__, "message": str(error)}
    finally:
        subprocess.Popen = original
        for child in children:
            if child.owned_pidfd is not None:
                child.fact["cleanup_unresolved"] = True
                os.close(child.owned_pidfd)
                child.owned_pidfd = None
        after = sorted(os.listdir("/proc/self/fd"))
        result = {"harness_exit": code, "failure": failure, "children": [p.fact for p in children],
                  "fd_before": before, "fd_after": after, "fd_sets_include_enumeration": True,
                  "assertions_and_harness_source_unchanged": True}
        observer.write_text(json.dumps(result, indent=2) + "\n")
    return code


if __name__ == "__main__":
    sys.exit(main())
