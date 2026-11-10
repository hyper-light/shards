#!/usr/bin/env python3
"""ROOT-only native FD-admission launcher; importing it performs no native work."""

import argparse
from dataclasses import dataclass
from pathlib import PurePosixPath
import sys


U64_MAX = (1 << 64) - 1
INT_MAX = (1 << 31) - 1
HEADER_BYTES = 4 * 4  # Frozen Linux UAPI: four 32-bit scalar fields.


class InputFault(ValueError):
    pass


def positive(text, maximum=U64_MAX):
    if not text or not text.isascii() or not text.isdecimal():
        raise argparse.ArgumentTypeError("positive ASCII decimal required")
    # Bound the input before int(), including Python's decimal-conversion limit.
    significant = text.lstrip("0")
    bound = str(maximum)
    if not significant or len(significant) > len(bound):
        raise argparse.ArgumentTypeError("zero or integer overflow")
    if len(significant) == len(bound) and significant > bound:
        raise argparse.ArgumentTypeError("integer overflow")
    return int(significant)


def absolute_path(text):
    if not text.startswith("/") or "\0" in text or any(
        part in (".", "..") for part in text.split("/")
    ):
        raise argparse.ArgumentTypeError("absolute path without NUL/dot components required")
    path = PurePosixPath(text)
    if not path.name:
        raise argparse.ArgumentTypeError("root is not an artifact path")
    return path


def sha256_hex(text):
    if len(text) != 256 // 4 or any(c not in "0123456789abcdefABCDEF" for c in text):
        raise argparse.ArgumentTypeError("exact SHA-256 hexadecimal required")
    return text.lower()


def add_u64(left, right):
    if not (0 <= left <= U64_MAX and 0 <= right <= U64_MAX) or left > U64_MAX - right:
        raise InputFault("absolute monotonic deadline overflow")
    return left + right


def remaining_ns(deadline, current):
    if not (0 <= current < deadline <= U64_MAX):
        raise InputFault("original absolute deadline expired or unrepresentable")
    return deadline - current


def poll_millis(nanoseconds):
    if not (0 < nanoseconds <= U64_MAX):
        raise InputFault("nonzero finite poll duration required")
    # Physical ns/ms conversion; the only clamp is poll(2)'s signed ABI bound.
    milliseconds = (nanoseconds + 1_000_000 - 1) // 1_000_000
    return min(milliseconds, INT_MAX)


def outer_deadline(entered, timeout, native_deadline):
    outer = add_u64(entered, timeout)
    if not entered < native_deadline < outer:
        raise InputFault("future original native deadline and finite outer cleanup window required")
    return outer


@dataclass(frozen=True)
class Config:
    binary: PurePosixPath
    binary_sha256: str
    child_helper: PurePosixPath
    child_helper_sha256: str
    record_dir: PurePosixPath
    deadline_monotonic_ns: int
    writes: int
    read_bytes: int
    nofile_soft: int
    outer_timeout_ns: int
    max_log_bytes: int
    expect_operation: str
    expect_errno: int
    expect_exit: int

    @classmethod
    def parse(cls, argv):
        parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
        for flag in ("binary", "child-helper", "record-dir"):
            parser.add_argument("--" + flag, type=absolute_path, required=True)
        for flag in ("binary-sha256", "child-helper-sha256"):
            parser.add_argument("--" + flag, type=sha256_hex, required=True)
        for flag in ("deadline-monotonic-ns", "writes", "read-bytes", "nofile-soft",
                     "outer-timeout-ns", "max-log-bytes", "expect-errno", "expect-exit"):
            parser.add_argument("--" + flag, type=positive, required=True)
        parser.add_argument("--expect-operation", required=True, choices=(
            "create_new_file", "inotify_init1", "independent_open"))
        config = cls(**vars(parser.parse_args(argv)))
        if config.read_bytes < HEADER_BYTES or config.read_bytes > sys.maxsize:
            parser.error("read envelope must fit one UAPI header and isize::MAX")
        if config.max_log_bytes > sys.maxsize or config.writes > sys.maxsize * 2 + 1:
            parser.error("output/work envelope is not representable by the frozen binary")
        if config.expect_errno > INT_MAX or config.expect_exit > (1 << 8) - 1:
            parser.error("expected errno/normal exit outside native ABI")
        if config.binary == config.child_helper:
            parser.error("native fixture and limit helper must be distinct artifacts")
        # No filesystem query: callers must supply fresh private artifact paths.
        for executable in (config.binary, config.child_helper):
            if executable == config.record_dir or config.record_dir in executable.parents:
                parser.error("executables must be outside the new record directory")
        return config


def native_main(config, entered_ns):
    # All native/file operations are ROOT-only and below this explicit entrypoint.
    import errno
    import hashlib
    import json
    import os
    from pathlib import Path
    import re
    import resource
    import select
    import signal
    import stat
    import subprocess
    import time

    class HarnessFault(Exception):
        def __init__(self, kind):
            self.kind = kind
            super().__init__(kind)

    def now():
        return time.clock_gettime_ns(time.CLOCK_MONOTONIC)

    outer_end = outer_deadline(entered_ns, config.outer_timeout_ns, config.deadline_monotonic_ns)

    def before(deadline):
        try:
            return remaining_ns(deadline, now())
        except InputFault as error:
            raise HarnessFault("Deadline") from error

    def snapshot():
        observer = os.open("/proc/self/fd", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        rows, transient = {}, []
        try:
            for name in os.listdir(observer):
                fd = int(name)
                if fd == observer:
                    continue
                try:
                    item = os.fstat(fd)
                    rows[str(fd)] = {"dev": item.st_dev, "ino": item.st_ino,
                                     "mode": item.st_mode,
                                     "target": os.readlink("/proc/self/fd/" + name)}
                except OSError as error:
                    if error.errno not in (errno.EBADF, errno.ENOENT):
                        raise
                    transient.append(fd)
        finally:
            os.close(observer)
        return {"fds": rows, "observer_fd_closed": observer,
                "closed_listdir_duplicates_excluded": transient}

    def executable(path, expected):
        before(outer_end)
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as source:
            first = os.fstat(source.fileno())
            if not stat.S_ISREG(first.st_mode) or first.st_mode & (stat.S_ISUID | stat.S_ISGID):
                raise HarnessFault("ExecutableKind")
            if source.read(4) != b"\x7fELF":
                raise HarnessFault("NativeELFRequired")
            source.seek(0)
            actual = hashlib.file_digest(source, "sha256").hexdigest()
            last = os.fstat(source.fileno())
        before(outer_end)
        if actual != expected or (first.st_dev, first.st_ino, first.st_size, first.st_mtime_ns) != (
            last.st_dev, last.st_ino, last.st_size, last.st_mtime_ns
        ):
            raise HarnessFault("ExecutableIdentity")
        return {"path": str(path), "sha256": actual, "dev": first.st_dev,
                "ino": first.st_ino, "bytes": first.st_size, "mode": first.st_mode}

    def write_all(destination, data):
        offset = 0
        while offset < len(data):
            before(outer_end)
            count = destination.write(data[offset:])
            if not count:
                raise HarnessFault("ReportWriteZero")
            offset += count

    def reap_if_ready(child, pidfd, deadline):
        waiter = select.poll()
        waiter.register(pidfd, select.POLLIN)
        while True:
            # At expiry one immediate completion check is permitted; no zero-time
            # retry loop or new cleanup deadline is introduced.
            left = max(0, deadline - now())
            events = waiter.poll(poll_millis(left) if left else 0)
            for _, flags in events:
                if flags & (select.POLLERR | select.POLLNVAL):
                    raise HarnessFault("ExitReadinessError")
                if flags & (select.POLLIN | select.POLLHUP):
                    return child.wait()  # Sole reaper, only after held exit readiness.
            if now() >= deadline:
                return None

    def capture(child, pidfd, raw, buffer, record):
        poller = select.poll()
        streams = {child.stdout.fileno(): ("stdout", child.stdout),
                   child.stderr.fileno(): ("stderr", child.stderr)}
        for fd in streams:
            os.set_blocking(fd, False)
            poller.register(fd, select.POLLIN)
        poller.register(pidfd, select.POLLIN)
        record["parent_control_fds"] = {"pidfd": pidfd,
                                        **{name: fd for fd, (name, _) in streams.items()}}
        record["parent_during_capture"] = snapshot()
        total = 0
        view = memoryview(buffer)
        while streams or child.returncode is None:
            # Original native deadline bounds actor execution. The ONE outer
            # deadline includes drain/cleanup; no timeout is restarted.
            end = config.deadline_monotonic_ns if child.returncode is None else outer_end
            events = poller.poll(poll_millis(before(end)))
            before(end)
            for fd, flags in events:
                if flags & (select.POLLNVAL | select.POLLERR):
                    raise HarnessFault("CaptureReadinessError")
                if fd == pidfd:
                    if flags & (select.POLLIN | select.POLLHUP):
                        record["held_child_exit_ready"] = True
                        record["actual_child_exit"] = child.wait()
                        poller.unregister(pidfd)
                    continue
                if fd not in streams:
                    raise HarnessFault("UnknownCaptureDescriptor")
                available = config.max_log_bytes - total
                if available <= 0:
                    raise HarnessFault("OutputEnvelope")
                try:
                    count = os.readv(fd, [view[:available]])
                except BlockingIOError:
                    continue
                except InterruptedError:
                    continue
                name, stream = streams[fd]
                if count == 0:
                    poller.unregister(fd)
                    stream.close()
                    del streams[fd]
                    record["stream_eof"].append(name)
                else:
                    write_all(raw[name], view[:count])
                    total += count
                    record["captured_bytes"][name] += count
                    if total >= config.max_log_bytes:
                        raise HarnessFault("OutputEnvelope")
        record["capture_complete"] = True

    def created_file(path, expected_bytes, buffer):
        before(outer_end)
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb", buffering=0) as observed:
            identity = os.fstat(observed.fileno())
            if not stat.S_ISREG(identity.st_mode) or identity.st_size != expected_bytes:
                raise HarnessFault("NativeFileShape")
            left = expected_bytes
            view = memoryview(buffer)
            while left:
                before(outer_end)
                count = observed.readinto(view[:min(left, len(buffer))])
                if not count or any(byte != 1 for byte in view[:count]):
                    raise HarnessFault("IndependentFileBytes")
                left -= count
            if observed.read(1):
                raise HarnessFault("IndependentFileExtraBytes")
        return {"dev": identity.st_dev, "ino": identity.st_ino,
                "bytes": identity.st_size, "independent_readback": True}

    record = {"schema": "managed-memory-notify-failure-v1", "phase": "Preflight",
              "arguments": {key: str(value) if isinstance(value, PurePosixPath) else value
                            for key, value in vars(config).items()},
              "entered_monotonic_ns": entered_ns, "outer_deadline_monotonic_ns": outer_end,
              "native_deadline_monotonic_ns": config.deadline_monotonic_ns,
              "actual_child_exit": None, "held_child_exit_ready": False,
              "capture_complete": False, "captured_bytes": {"stdout": 0, "stderr": 0},
              "stream_eof": [], "native_passed": False, "expected_failure_case": False,
              "cleanup_errors": [], "cleanup_incomplete": False,
              "parent_limits_modified": False, "paths_deleted": False}
    child, pidfd, raw, directory, buffer = None, None, {}, None, None
    baseline, parent_limits = None, None
    sentinel = b"owned native-notification FD-test sentinel\n"
    try:
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise HarnessFault("LinuxPidfdRequired")
        if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
            raise HarnessFault("SoleReaperPrecondition")
        before(config.deadline_monotonic_ns)
        before(outer_end)
        parent_limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        record["parent_nofile_before"] = parent_limits
        if parent_limits[0] != resource.RLIM_INFINITY and config.nofile_soft > parent_limits[0]:
            raise HarnessFault("ChildLimitIncreaseRefused")
        baseline = snapshot()
        record["parent_before"] = baseline
        record["binary"] = executable(config.binary, config.binary_sha256)
        record["child_helper"] = executable(config.child_helper, config.child_helper_sha256)
        buffer = bytearray(config.max_log_bytes)  # Provision before workload release.
        candidate = Path(config.record_dir)
        candidate.mkdir(mode=0o700)  # Fails on every pre-existing final path.
        directory = candidate
        with (directory / "sentinel").open("xb", buffering=0) as owner:
            write_all(owner, sentinel)
        for name in ("stdout", "stderr"):
            raw[name] = (directory / (name + ".raw")).open("xb", buffering=0)
        command = [str(config.child_helper), str(config.nofile_soft), str(config.binary),
                   str(directory / "native"), str(config.deadline_monotonic_ns),
                   str(config.writes), str(config.read_bytes)]
        record["helper_argv"] = command
        before(config.deadline_monotonic_ns)
        record["phase"] = "Launching"
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, close_fds=True, bufsize=0,
                                 env={"LC_ALL": "C"})
        record["owned_child_pid"] = child.pid
        # No poll/wait/reaper has run. Default SIGCHLD + sole Popen owner keeps
        # even an already exited child unreaped across this exact ROOT-owned seam.
        pidfd = os.pidfd_open(child.pid, 0)
        record["owned_child_pidfd"] = pidfd
        record["phase"] = "Capturing"
        capture(child, pidfd, raw, buffer, record)
        record["phase"] = "Reaped"
    except (HarnessFault, InputFault, OSError, MemoryError, ValueError) as error:
        record["launcher_error"] = {"kind": getattr(error, "kind", type(error).__name__),
                                    "errno": getattr(error, "errno", None), "detail": str(error)}
    finally:
        if child is not None and child.returncode is None:
            try:
                if pidfd is not None:
                    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                else:
                    # Only the still-unreaped sole Popen child; no arbitrary PID API.
                    child.kill()
                record["owned_child_kill_requested"] = True
            except ProcessLookupError:
                record["kill_already_exited"] = True
            except OSError as error:
                record["cleanup_errors"].append({"operation": "kill_owned_child", "errno": error.errno})
            try:
                if child.returncode is not None:
                    record["actual_child_exit"] = child.returncode
                elif pidfd is not None:
                    record["actual_child_exit"] = reap_if_ready(child, pidfd, outer_end)
                    record["held_child_exit_ready"] = child.returncode is not None
                else:
                    # A failed pidfd acquisition cannot justify an unbounded wait.
                    # One nonblocking owned-child reap attempt is not a polling loop.
                    waited, status = os.waitpid(child.pid, os.WNOHANG)
                    if waited == child.pid:
                        child.returncode = os.waitstatus_to_exitcode(status)
                        record["actual_child_exit"] = child.returncode
                if child.returncode is None:
                    record["cleanup_incomplete"] = True
            except (OSError, HarnessFault) as error:
                record["cleanup_incomplete"] = True
                record["cleanup_errors"].append({"operation": "reap_owned_child", "detail": str(error)})
        if child is not None:
            for stream in (child.stdout, child.stderr):
                if stream is not None and not stream.closed:
                    try:
                        stream.close()
                    except OSError as error:
                        record["cleanup_errors"].append({"operation": "close_capture", "errno": error.errno})
        if pidfd is not None:
            try:
                os.close(pidfd)
            except OSError as error:
                record["cleanup_errors"].append({"operation": "close_pidfd", "errno": error.errno})
        for output in raw.values():
            try:
                output.close()
            except OSError as error:
                record["cleanup_errors"].append({"operation": "close_raw_record", "errno": error.errno})

    try:
        if directory is not None:
            record["raw_file_bytes"] = {
                name: (directory / (name + ".raw")).stat().st_size
                for name in raw
            }
            with (directory / "sentinel").open("rb") as observed:
                record["sentinel_unchanged"] = observed.read(len(sentinel) + 1) == sentinel
            target = directory / "native"
            record["native_directory_exists"] = target.is_dir()
            record["native_file_exists"] = (target / "watched").exists()
            if record["capture_complete"] and "launcher_error" not in record:
                stdout = (directory / "stdout.raw").read_bytes()
                lines = stdout.splitlines()
                expected = ('terminal result=Err(Io { operation: "' + config.expect_operation
                            + '", errno: Some(' + str(config.expect_errno)
                            + ') }) cleanup=Ok(()) passed=false').encode()
                terminals = [line for line in lines if line.startswith(b"terminal result=")]
                record["native_terminal_matches"] = terminals == [expected]
                record["native_passed"] = child.returncode == 0 and terminals == [
                    b"terminal result=Ok(()) cleanup=Ok(()) passed=true"]
                stdio = [re.fullmatch(rb"launcher_child phase=stdio pid=(\d+) observer_fd=(\d+) "
                                     rb"observer_closed=true application_fds=(\d+)", line)
                         for line in lines]
                limits = [re.fullmatch(rb"launcher_child phase=limit pid=(\d+) before_soft=(\d+) "
                                      rb"before_hard=(\d+) after_soft=(\d+) after_hard=(\d+)", line)
                          for line in lines]
                stdio, limits = [m for m in stdio if m], [m for m in limits if m]
                observed_stdio = tuple(map(int, stdio[0].groups())) if len(stdio) == 1 else ()
                observed_limit = tuple(map(int, limits[0].groups())) if len(limits) == 1 else ()
                inherited = tuple(U64_MAX if n == resource.RLIM_INFINITY else n for n in parent_limits)
                record["helper_stdio_matches"] = bool(observed_stdio) and (
                    observed_stdio[0] == child.pid and observed_stdio[2] == 3)
                record["helper_limit_matches"] = observed_limit == (
                    child.pid, *inherited, config.nofile_soft, inherited[1])
                record["helper_reported_limit"] = observed_limit
                record["helper_error_present"] = any(
                    line.startswith(b"launcher_child phase=error") for line in lines)
                # The error logger writes to stderr; retain/check that stream too.
                stderr = (directory / "stderr.raw").read_bytes()
                record["helper_error_present"] |= b"launcher_child phase=error" in stderr
                if config.expect_operation == "create_new_file":
                    record["independent_created_paths_match"] = target.is_dir() and not (
                        target / "watched").exists()
                else:
                    expected_bytes = config.writes if config.expect_operation == "independent_open" else 0
                    record["independent_native_file"] = created_file(
                        target / "watched", expected_bytes, buffer)
                    record["independent_created_paths_match"] = True
        if baseline is not None:
            record["parent_after"] = snapshot()
            record["parent_fds_returned"] = baseline["fds"] == record["parent_after"]["fds"]
        if parent_limits is not None:
            record["parent_nofile_after"] = resource.getrlimit(resource.RLIMIT_NOFILE)
            record["parent_nofile_unchanged"] = tuple(record["parent_nofile_after"]) == parent_limits
        checks = ("capture_complete", "held_child_exit_ready", "native_terminal_matches",
                  "helper_stdio_matches", "helper_limit_matches", "sentinel_unchanged",
                  "independent_created_paths_match", "parent_fds_returned", "parent_nofile_unchanged")
        record["expected_failure_case"] = (all(record.get(key, False) for key in checks)
            and record["actual_child_exit"] == config.expect_exit and not record["native_passed"]
            and not record.get("helper_error_present", True) and not record["cleanup_errors"]
            and not record["cleanup_incomplete"] and "launcher_error" not in record)
    except (OSError, HarnessFault, MemoryError, ValueError) as error:
        record["assertion_error"] = {"kind": type(error).__name__, "detail": str(error),
                                     "errno": getattr(error, "errno", None)}
        record["expected_failure_case"] = False
    record["phase"] = "ExpectedNativeFailure" if record["expected_failure_case"] else "NotEstablished"
    if directory is not None:
        try:
            with (directory / "result.json").open("x") as result:
                observations = dict(record)
                observations["case_assertions_match_before_result_close"] = observations.pop(
                    "expected_failure_case")
                observations["phase"] = "BeforeResultWriterClose"
                observations["record_stage"] = "BeforeResultWriterClose; final verdict: launcher stdout/exit"
                json.dump(observations, result, indent=2)
                result.write("\n")
            # Report-writing opens/closes its own fd after the first capture census.
            record["parent_fds_after_result_close"] = snapshot()
            record["result_writer_closed_fd_return"] = (
                baseline is not None and baseline["fds"] == record["parent_fds_after_result_close"]["fds"])
            if not record["result_writer_closed_fd_return"]:
                record["expected_failure_case"] = False
        except (OSError, MemoryError) as error:
            record["result_error"] = {"detail": str(error), "errno": getattr(error, "errno", None)}
            record["expected_failure_case"] = False
    record["record_stage"] = "FinalLauncherRecord"
    record["phase"] = "ExpectedNativeFailure" if record["expected_failure_case"] else "NotEstablished"
    try:
        sys.stdout.write(json.dumps(record) + "\n")
        sys.stdout.flush()
    except (OSError, MemoryError):
        return 1
    return 0 if record["expected_failure_case"] and record.get("result_writer_closed_fd_return") else 1


if __name__ == "__main__":
    import json
    import time
    if sys.platform != "linux":
        sys.stderr.write('{"launcher_error":"UnsupportedPlatform","native_started":false}\n')
        raise SystemExit(1)
    entered = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    config = Config.parse(sys.argv[1:])
    try:
        exit_code = native_main(config, entered)
    except InputFault as error:
        sys.stderr.write(json.dumps({"launcher_error": "InputArithmetic", "detail": str(error),
                                     "native_started": False, "expected_failure_case": False}) + "\n")
        exit_code = 1
    raise SystemExit(exit_code)
