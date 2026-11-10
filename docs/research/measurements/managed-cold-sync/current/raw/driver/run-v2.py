#!/usr/bin/env python3
"""Bounded Mac correctness runner; receives exact named Git source bytes only."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import resource
import select
import signal
import subprocess
import time

SELECTOR = "client::cold_sync_current::owned_connection_cold_sync_refusal_current"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def write_new(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(data)


def command(argv, cwd, env, prefix, timeout):
    with prefix.with_suffix(".stdout").open("xb") as out, prefix.with_suffix(".stderr").open("xb") as err:
        result = subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                stdout=out, stderr=err, timeout=timeout)
    return {"argv": argv, "cwd": str(cwd), "exit": result.returncode,
            "stdout": str(prefix.with_suffix(".stdout")), "stderr": str(prefix.with_suffix(".stderr"))}


def case(binary, directory, seconds, trace, env):
    directory.mkdir()
    token = os.urandom(24).hex()
    # This process never changes session/group; the fresh test session owns it.
    write_new(directory / "standin", ("#!/bin/sh\nexec /bin/sleep " + str(seconds + 1) + "\n").encode())
    (directory / "standin").chmod(0o700)
    deadline = time.monotonic() + seconds
    case_env = dict(env, SHARDS_COLD_CASE_DIR=str(directory), SHARDS_COLD_CASE_MS=str(seconds * 1000),
                    SHARDS_COLD_OWNER=token)
    if trace:
        case_env["SHARDS_COLD_TRACE"] = "1"
    row = {"trace": trace, "deadline_monotonic": deadline, "seconds": seconds,
           "argv": [str(binary), SELECTOR, "--exact", "--test-threads=1", "--nocapture"],
           "standin_exit_observed": False, "test_exit_observed": False, "timed_out": False,
           "cleanup": [], "child_reaping_after_abort": "unknown; outer is not its parent"}
    output = bytearray()
    ready = None
    with (directory / "stderr").open("xb") as stderr:
        process = subprocess.Popen(row["argv"], env=case_env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=stderr, start_new_session=True)
        row["test_pid"] = process.pid
        queue = select.kqueue()
        try:
            row["test_pgid"] = os.getpgid(process.pid)
            if row["test_pgid"] != process.pid:
                raise RuntimeError("test did not own its fresh session")
            os.set_blocking(process.stdout.fileno(), False)
            queue.control([
                select.kevent(process.pid, filter=select.KQ_FILTER_PROC,
                              flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT),
                select.kevent(process.stdout.fileno(), filter=select.KQ_FILTER_READ,
                              flags=select.KQ_EV_ADD),
            ], 0, 0)
            while not row["test_exit_observed"] and time.monotonic() < deadline:
                events = queue.control(None, 3, max(0, deadline - time.monotonic()))
                for event in events:
                    if event.flags & select.KQ_EV_ERROR:
                        raise OSError(event.data, "kevent observer error")
                    if event.filter == select.KQ_FILTER_PROC and event.fflags & select.KQ_NOTE_EXIT:
                        if event.ident == process.pid:
                            row["test_exit_observed"] = True
                        elif ready and event.ident == ready[0]:
                            row["standin_exit_observed"] = True
                    elif event.filter == select.KQ_FILTER_READ:
                        chunk = os.read(process.stdout.fileno(), 4096)
                        output.extend(chunk)
                        if not chunk:
                            queue.control([select.kevent(process.stdout.fileno(), filter=select.KQ_FILTER_READ,
                                          flags=select.KQ_EV_DELETE)], 0, 0)
                        if len(output) > 65536:
                            raise RuntimeError("control output exceeds explicit fixture envelope")
                        if ready is None and b"COLD_READY " in output:
                            line = bytes(output).split(b"COLD_READY ", 1)[1]
                            if b"\n" not in line:
                                continue
                            pid, path = line.split(b"\n", 1)[0].decode().split(" ", 1)
                            ready = (int(pid), Path(path))
                            row["standin_pid"] = ready[0]
                            row["vsock"] = str(ready[1])
                            queue.control([select.kevent(ready[0], filter=select.KQ_FILTER_PROC,
                                          flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                                          fflags=select.KQ_NOTE_EXIT)], 0, 0)
                            row["observer_registered_before_release"] = True
                            process.stdin.write(b"R")
                            process.stdin.flush()
                            process.stdin.close()
            row["timed_out"] = not row["test_exit_observed"]
        except Exception as error:
            row["driver_error"] = repr(error)
        finally:
            # No poll()/wait()/communicate() has reaped the test. Its PID still
            # anchors the session identity while we end only that owned group.
            try:
                os.killpg(process.pid, signal.SIGKILL)
                row["cleanup"].append({"kill_owned_group": "sent"})
            except ProcessLookupError:
                row["cleanup"].append({"kill_owned_group": "ESRCH"})
            except OSError as error:
                row["cleanup"].append({"kill_owned_group_errno": error.errno})
            if ready and not row["standin_exit_observed"]:
                cleanup_deadline = time.monotonic() + seconds
                while time.monotonic() < cleanup_deadline and not row["standin_exit_observed"]:
                    for event in queue.control(None, 3, max(0, cleanup_deadline - time.monotonic())):
                        if event.filter == select.KQ_FILTER_READ:
                            queue.control([select.kevent(process.stdout.fileno(), filter=select.KQ_FILTER_READ,
                                          flags=select.KQ_EV_DELETE)], 0, 0)
                        if event.flags & select.KQ_EV_ERROR:
                            row["cleanup"].append({"observer_errno": event.data})
                        elif event.filter == select.KQ_FILTER_PROC and event.fflags & select.KQ_NOTE_EXIT and event.ident == ready[0]:
                            row["standin_exit_observed"] = True
            row["exit"] = process.wait(timeout=seconds)
            while True:
                try:
                    chunk = os.read(process.stdout.fileno(), 4096)
                except BlockingIOError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
            process.stdout.close()
            if not process.stdin.closed:
                process.stdin.close()
            queue.close()
    write_new(directory / "stdout", bytes(output))
    if ready:
        socket = ready[1]
        marker = socket.parent / "cold-sync-owned"
        if marker.exists() and marker.read_bytes() == token.encode():
            for item in [socket, marker]:
                try:
                    item.unlink()
                    row["cleanup"].append({"owned_unlink": str(item)})
                except FileNotFoundError:
                    pass
            try:
                socket.parent.rmdir()
                row["cleanup"].append({"owned_rmdir": str(socket.parent)})
            except OSError as error:
                row["cleanup"].append({"owned_rmdir_errno": error.errno})
        elif socket.parent.exists():
            row["cleanup"].append({"unremoved_directory": str(socket.parent), "reason": "ownership marker absent"})
    row["stdout_sha256"] = sha(bytes(output))
    row["stderr_sha256"] = sha((directory / "stderr").read_bytes())
    write_new(directory / "result.json", (json.dumps(row, indent=2) + "\n").encode())
    return row


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--case-seconds", type=int, required=True)
    parser.add_argument("--compile-seconds", type=int, required=True)
    args = parser.parse_args()
    if args.case_seconds <= 0 or args.compile_seconds <= 0:
        parser.error("positive finite bounds required")
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    # Kiln admits 1GiB; preserve any tighter inherited hard data limit.
    soft, hard = resource.getrlimit(resource.RLIMIT_DATA)
    data_limit = min(1073741824, hard) if hard != resource.RLIM_INFINITY else 1073741824
    resource.setrlimit(resource.RLIMIT_DATA, (data_limit, hard))
    soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    file_limit = min(2147483648, hard) if hard != resource.RLIM_INFINITY else 2147483648
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, hard))
    bundle = json.loads(args.bundle.read_text())
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_BUILD_JOBS="1",
               CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS="1", RUSTFLAGS="-Cllvm-args=-threads=1 --check-cfg=cfg(has_allocation_refusal)")
    results = []
    for version in bundle["versions"]:
        source = root / version["label"]
        source.mkdir()
        for name, record in version["files"].items():
            path = Path(name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("unsafe closure path")
            body = record["text"].encode()
            if sha(body) != record["sha256"]:
                raise ValueError("source identity mismatch")
            write_new(source / path, body)
        version_env = dict(env, CARGO_TARGET_DIR=str(source / "target"))
        if version["has_allocation_refusal"]:
            version_env["RUSTFLAGS"] += " --cfg has_allocation_refusal"
        row = {"label": version["label"], "revision": version["revision"], "source": str(source),
               "file_identities": {name: {k: v for k, v in rec.items() if k != "text"} for name, rec in version["files"].items()},
               "environment": {key: version_env[key] for key in ["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS", "RUSTFLAGS"]}}
        argv = ["cargo", "+1.98.1", "test", "--locked", "-j1", "--profile", "test-release", "-p", "shards-api", "--lib", "--no-run", "--message-format=json"]
        row["compile"] = command(argv, source, version_env, source / "compile", args.compile_seconds)
        if row["compile"]["exit"] == 0:
            artifacts = [json.loads(line) for line in (source / "compile.stdout").read_text().splitlines() if line.startswith("{")]
            binaries = [Path(item["executable"]) for item in artifacts if item.get("reason") == "compiler-artifact" and item.get("executable") and item.get("target", {}).get("name") == "shards_api"]
            if len(binaries) != 1:
                raise RuntimeError("expected one actual API library test artifact")
            binary = binaries[0]
            row["binary"] = {"path": str(binary), "sha256": sha(binary.read_bytes())}
            row["selection"] = command([str(binary), SELECTOR, "--list", "--exact"], source, version_env, source / "selection", args.case_seconds)
            selected = (source / "selection.stdout").read_text()
            if row["selection"]["exit"] != 0 or selected.count(SELECTOR + ": test") != 1 or "1 test," not in selected:
                raise RuntimeError("missing exact single selected test")
            row["cases"] = [case(binary, source / name, args.case_seconds, trace, version_env)
                            for name, trace in [("unmodified-red", False), ("attribution", True)]]
        results.append(row)
        write_new(source / "summary.json", (json.dumps(row, indent=2) + "\n").encode())
    write_new(root / "results.json", (json.dumps(results, indent=2) + "\n").encode())
    print(json.dumps({"root": str(root), "results": [{"label": r["label"], "compile": r["compile"]["exit"], "binary": r.get("binary"), "cases": r.get("cases")} for r in results]}))


if __name__ == "__main__":
    main()
