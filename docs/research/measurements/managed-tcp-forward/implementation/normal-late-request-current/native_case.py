#!/usr/bin/env python3
"""One bounded native TCP case; existing source-linked Workers own the sockets."""

import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import select
import signal
import subprocess
import time


FILTERS = {
    "late-request": "managed_network::intercept::tcp_forward::native_tests::native_tcp_late_request_suffix_after_backend_eof",
    "forward-server-first": "managed_network::intercept::tcp_forward::native_tests::native_tcp_pump_both_half_closes",
}

PREFIX = "managed-tcp-forward "


def identity(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--record", type=Path, required=True)
    parser.add_argument("--case", choices=FILTERS, default="forward-server-first")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    inputs = json.loads(args.inputs.read_text())
    timeout_ms = inputs["parent_timeout_ms"]
    env_inputs = inputs["env"]
    if not isinstance(timeout_ms, int) or timeout_ms <= 0:
        raise ValueError("positive caller parent_timeout_ms required")
    if int(env_inputs["SHARDS_NETWORK_CASE_MS"]) >= timeout_ms:
        raise ValueError("parent must retain time to observe/reap child after its deadline")
    if "SHARDS_NATIVE_RELEASE_FD" in env_inputs:
        raise ValueError("launcher owns the release descriptor")
    args.record.mkdir(exist_ok=False, parents=True)
    (args.record / "inputs.json").write_bytes(args.inputs.read_bytes())
    before = len(os.listdir("/proc/self/fd"))
    initial_hash = identity(binary)
    child = None
    pidfd = None
    reader = None
    writer = None
    outcome = {"binary": str(binary), "binary_before_sha256": initial_hash,
               "filter": FILTERS[args.case], "fd_before": before,
               "kernel_return": "Unknown", "pidfd_exit_ready": False,
               "reaped": False, "timeout": False}
    command = [str(binary), "--exact", FILTERS[args.case], "--ignored", "--nocapture", "--test-threads=1"]
    try:
        reader, writer = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
        env = os.environ.copy()
        for name in ("SHARDS_NATIVE_WRITE_PRESSURE", "SHARDS_NATIVE_SNDBUF_BYTES", "SHARDS_NATIVE_RCVBUF_BYTES"):
            env.pop(name, None)
        env.update(env_inputs)
        env["SHARDS_NATIVE_RELEASE_FD"] = str(reader)
        end = time.monotonic() + timeout_ms / 1000
        with (args.record / "stdout").open("wb") as stdout, (args.record / "stderr").open("wb") as stderr:
            child = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr,
                                     pass_fds=(reader,))
            os.close(reader)
            reader = None
            # Held while our child is unreaped, before its private release.
            pidfd = os.pidfd_open(child.pid, 0)
            outcome["pid"] = child.pid
            if os.write(writer, b"R") != 1:
                raise RuntimeError("private release was not written")
            os.close(writer)
            writer = None
            poller = select.poll()
            poller.register(pidfd, select.POLLIN)
            events = poller.poll(max(0, int((end - time.monotonic()) * 1000)))
            outcome["pidfd_events"] = events
            outcome["pidfd_exit_ready"] = any(fd == pidfd and flags & select.POLLIN for fd, flags in events)
            if not outcome["pidfd_exit_ready"]:
                outcome["timeout"] = True
                raise TimeoutError("owned child did not become exit-ready within caller bound")
            outcome["status"] = child.wait(timeout=max(0, end - time.monotonic()))
            outcome["reaped"] = True
    except Exception as error:
        outcome["launcher_error"] = {"type": type(error).__name__, "detail": str(error)}
    finally:
        if child is not None and child.returncode is None:
            # Only the unreaped child we started; no process-name/PID scan.
            if pidfd is not None:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
            else:
                child.kill()
            outcome["status"] = child.wait()
            outcome["reaped"] = True
            outcome["forced_owned_cleanup"] = True
        for fd in (reader, writer, pidfd):
            if fd is not None:
                os.close(fd)
    outcome["fd_after"] = len(os.listdir("/proc/self/fd"))
    outcome["binary_after_sha256"] = identity(binary)
    raw = (args.record / "stdout").read_text(errors="replace")
    rows = [json.loads(line.split(PREFIX, 1)[1]) for line in raw.splitlines() if PREFIX in line]
    (args.record / "observed.json").write_text(json.dumps(rows, indent=2) + "\n")
    def one(kind, role=None):
        found = [row for row in rows if row.get("type") == kind and (role is None or row.get("role") == role)]
        return found[0] if len(found) == 1 else {}

    def endpoint(row, expected, written, read_end, write_end):
        return (bool(row) and row.get("read_hex") == expected.hex() and row.get("read") == len(expected)
                and row.get("written") == written and row.get("read_end") == read_end
                and row.get("write_end") == write_end and row.get("extra_hex") is None
                and row.get("ok") is True and row.get("error_kind") == "None")

    request = bytes.fromhex(env_inputs["SHARDS_TCP_REQUEST_HEX"])
    greeting = bytes.fromhex(env_inputs["SHARDS_TCP_GREETING_HEX"])
    tail = bytes.fromhex(env_inputs["SHARDS_TCP_TAIL_HEX"])
    response = greeting + tail
    flow = one("NativeFlow")
    joined = one("NativeJoined")
    client = one("Endpoint", "NativeClient")
    backend = one("Endpoint", "NativeBackend")
    up, down = flow.get("to_backend", {}), flow.get("to_client", {})
    outcome["checks"] = {
        "one_actual_test": "1 passed; 0 failed;" in raw and "running 1 test" in raw,
        "native0": outcome.get("status") == 0 and outcome["pidfd_exit_ready"] and not outcome["timeout"],
        "pidfd_exit_ready_direct_reaped_no_forced_cleanup": outcome["reaped"] and outcome["pidfd_exit_ready"] and not outcome.get("forced_owned_cleanup", False),
        "parent_fd_return": outcome["fd_after"] == before,
        "binary_unchanged": outcome["binary_after_sha256"] == initial_hash,
        "existing_workers_joined": joined.get("relay_joined") is True and joined.get("backend_joined") is True,
        "actual_native_connected": flow.get("connect") == "Connected",
        "authorized_fixed_inputs": request.hex() == "5245515545535400ff0d0a0102" and greeting.hex() == "53455256455200ff" and tail.hex() == "5441494c0d0a" and "SHARDS_TCP_EXTRA_HEX" not in env_inputs and env_inputs["SHARDS_TCP_UP_BYTES"] == "3" and env_inputs["SHARDS_TCP_DOWN_BYTES"] == "5" and env_inputs["SHARDS_NETWORK_CASE_MS"] == "30000" and timeout_ms == 45000,
    }
    if args.case == "late-request":
        prefix = one("Endpoint", "BackendPrefix")
        closed = one("Endpoint", "BackendResponseClosed")
        before_suffix = one("Endpoint", "ClientBeforeSuffix")
        after_suffix = one("Endpoint", "ClientAfterSuffix")
        cleanup = one("NativeBoundaryCleanup")
        def precedes(left, right):
            return bool(left) and bool(right) and rows.index(left) < rows.index(right)
        outcome["checks"].update({
            "independent_backend_prefix3_before_response_close": endpoint(prefix, request[:3], len(greeting), "Open", "Open") and endpoint(closed, request[:3], len(response), "Open", "Closed"),
            "actual_client_response14_EOF_while_write_open_after_prefix3": endpoint(before_suffix, response, 3, "Closed", "Open"),
            "actual_suffix10_accepted_after_response_EOF": endpoint(after_suffix, response, len(request), "Closed", "Open"),
            "actual_late_client_FIN": endpoint(client, response, len(request), "Closed", "Closed"),
            "independent_backend_full13_and_EOF": endpoint(backend, request, len(response), "Closed", "Closed"),
            "typed_both_halves_closed_no_fault": flow.get("end") == "BothHalvesClosed" and flow.get("fault") is None and flow.get("abort") == {"kind": "NotRequired"},
            "exact_progress_and_no_extra_or_pending": all(p.get("half") == "WriteClosed" and p.get("source_end") == "Eof" and p.get("pending") == p.get("extra_consumed") == 0 for p in (up, down)) and up.get("read") == up.get("written") == len(request) and down.get("read") == down.get("written") == len(response),
            "actual_causal_receipts_before_strict_assertions": all(precedes(a, b) for a, b in [(prefix, closed), (closed, backend), (prefix, before_suffix), (before_suffix, after_suffix), (after_suffix, client), (client, flow), (backend, flow), (flow, joined), (joined, cleanup)]),
            "native_fd_return_original_deadline_positive": bool(cleanup) and cleanup.get("fd_before") == cleanup.get("fd_after") and joined.get("original_remaining_ns", 0) > 0,
        })
    else:
        cleaned = re.findall(r"NATIVE_TCP_EXACT scenario=ServerFirst request=(\d+) response=(\d+) fd_before=(\d+) fd_after=(\d+) kernel_return=Unknown", raw)
        outcome["checks"].update({
            "ordinary_exact_client_prefix_eof": endpoint(client, response, len(request), "Closed", "Closed"),
            "ordinary_exact_backend_prefix_eof": endpoint(backend, request, len(response), "Closed", "Closed"),
            "ordinary_both_halves_closed": flow.get("end") == "BothHalvesClosed" and flow.get("fault") is None and flow.get("abort") == {"kind": "NotRequired"},
            "ordinary_progress_facts": all(p.get("half") == "WriteClosed" and p.get("source_end") == "Eof" and p.get("pending") == p.get("extra_consumed") == 0 for p in (up, down)) and up.get("read") == up.get("written") == len(request) and down.get("read") == down.get("written") == len(response),
            "ordinary_native_fd_return": len(cleaned) == 1 and tuple(map(int, cleaned[0][:2])) == (len(request), len(response)) and cleaned[0][2] == cleaned[0][3],
        })
    outcome["kernel_population_return"] = "Unknown"
    outcome["physical_kernel_caps"] = "Unverified"
    outcome["passed"] = all(outcome["checks"].values()) and "launcher_error" not in outcome
    (args.record / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    print(json.dumps(outcome), flush=True)
    return 0 if outcome["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
