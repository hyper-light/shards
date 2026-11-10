#!/usr/bin/env python3
"""One bounded, owned native TCP case; source-linked Rust owns the sockets."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import time


FILTERS = {
    "both-half-closes": "managed_network::intercept::tcp_forward::native_tests::native_tcp_pump_both_half_closes",
    "receipt-cancel": "managed_network::intercept::tcp_forward::native_tests::native_tcp_cancel_after_backend_receipt",
    "reset": "managed_network::intercept::tcp_forward::native_tests::native_tcp_reset_matches_direct_peer",
    "client-reset": "managed_network::intercept::tcp_forward::native_tests::native_tcp_client_reset_matches_direct_peer",
}
PREFIX = "managed-tcp-forward "


def identity(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def payload(word):
    if not word.startswith("repeat:"):
        return bytes.fromhex(word)
    _, size, seed = word.split(":", 2)
    size = int(size)
    seed = bytes.fromhex(seed)
    if size < 0 or not seed:
        raise ValueError("nonnegative repeated size and nonempty hex seed required")
    count, tail = divmod(size, len(seed))
    return seed * count + seed[:tail]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--record", type=Path, required=True)
    parser.add_argument("--case", choices=FILTERS, default="both-half-closes")
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
    endpoints = [row for row in rows if row.get("type") == "Endpoint"]
    flows = [row for row in rows if row.get("type") == "NativeFlow"]
    backend = [row for row in endpoints if row["role"] == "NativeBackend"]
    client = [row for row in endpoints if row["role"] == "NativeClient"]
    joined = [row for row in rows if row.get("type") == "NativeJoined"]
    response = payload(env_inputs["SHARDS_TCP_GREETING_HEX"])
    if args.case == "both-half-closes":
        response += payload(env_inputs["SHARDS_TCP_TAIL_HEX"])
    outcome["checks"] = {
        "one_actual_test": "1 passed; 0 failed;" in raw and "running 1 test" in raw,
        "child_success": outcome.get("status") == 0 and outcome["pidfd_exit_ready"] and not outcome["timeout"],
        "backend_bytes": len(backend) == 1 and backend[0]["ok"] and bytes.fromhex(backend[0]["read_hex"]) == payload(env_inputs["SHARDS_TCP_REQUEST_HEX"]),
        "client_bytes": len(client) == 1 and client[0]["ok"] and bytes.fromhex(client[0]["read_hex"]) == response,
        "both_endpoint_halves": len(endpoints) == 2 and all(row["read_end"] == "Closed" and row["write_end"] == "Closed" and row["extra_hex"] is None for row in endpoints),
        "both_flow_halves": len(flows) == 1 and all(flows[0][key]["half"] == "WriteClosed" and flows[0][key]["source_end"] == "Eof" and flows[0][key]["pending"] == 0 and flows[0][key]["extra_consumed"] == 0 for key in ("to_backend", "to_client")),
        "owned_child_reaped": outcome["reaped"],
        "parent_fd_return": outcome["fd_after"] == before,
        "binary_unchanged": outcome["binary_after_sha256"] == initial_hash,
    }
    if args.case == "receipt-cancel":
        outcome["checks"]["both_endpoint_halves"] = len(backend) == len(client) == 1 and backend[0]["read_end"] == "Closed" and backend[0]["write_end"] == "Open" and client[0]["read_end"] == client[0]["write_end"] == "Closed"
        outcome["checks"]["both_flow_halves"] = len(flows) == 1 and flows[0]["fault"] == {"kind": "Cancelled"} and flows[0]["to_backend"]["half"] == "WriteClosed" and flows[0]["to_backend"]["source_end"] == "Eof" and flows[0]["to_client"]["half"] == "Reading" and flows[0]["to_client"]["pending"] == 0
        outcome["checks"]["joined_with_original_time_remaining"] = len(joined) == 1 and joined[0]["relay_joined"] and joined[0]["backend_joined"] and joined[0]["original_remaining_ns"] > 0
    if args.case == "reset":
        reset = [row for row in rows if row.get("type") == "NativeReset"]
        cleanup = [row for row in rows if row.get("type") == "NativeResetCleanup"]
        backend = [row for row in endpoints if row["role"] == "ResetBackend"]
        client = [row for row in endpoints if row["role"] == "ResetClient"]
        outcome["checks"]["backend_bytes"] = len(backend) == 2 and all(row["ok"] and bytes.fromhex(row["read_hex"]) == payload(env_inputs["SHARDS_TCP_REQUEST_HEX"]) for row in backend)
        outcome["checks"]["client_bytes"] = len(client) == 2 and all(bytes.fromhex(row["read_hex"]) == payload(env_inputs["SHARDS_TCP_GREETING_HEX"]) for row in client)
        outcome["checks"]["both_endpoint_halves"] = len(reset) == 2 and all(row["client_error_kind"] == "Some(ConnectionReset)" for row in reset)
        outcome["checks"]["both_flow_halves"] = len(flows) == 1 and flows[0]["fault"]["kind"] == "Native" and flows[0]["fault"]["error_kind"] == "ConnectionReset"
        outcome["checks"]["all_owned_descriptor_returns"] = len(cleanup) == 2 and all(row["fd_before"] == row["fd_after"] for row in cleanup)
    if args.case == "client-reset":
        reset = [row for row in rows if row.get("type") == "NativeClientReset"]
        cleanup = [row for row in rows if row.get("type") == "NativeResetCleanup"]
        backend = [row for row in endpoints if row["role"] == "ClientResetBackend"]
        client = [row for row in endpoints if row["role"] == "ClientResetClient"]
        outcome["checks"]["backend_bytes"] = len(backend) == 2 and all(row["ok"] and bytes.fromhex(row["read_hex"]) == payload(env_inputs["SHARDS_TCP_REQUEST_HEX"]) for row in backend)
        outcome["checks"]["client_bytes"] = len(client) == 2 and all(row["ok"] and bytes.fromhex(row["read_hex"]) == payload(env_inputs["SHARDS_TCP_GREETING_HEX"]) for row in client)
        outcome["checks"]["both_endpoint_halves"] = len(reset) == 2 and all(row["backend_error_kind"] == "Some(ConnectionReset)" for row in reset)
        outcome["checks"]["both_flow_halves"] = len(flows) == 1 and flows[0]["fault"]["kind"] == "Native" and flows[0]["fault"]["leg"] == "Client" and flows[0]["fault"]["error_kind"] == "ConnectionReset" and flows[0]["abort"] == {"kind": "Requested", "leg": "Backend"}
        outcome["checks"]["all_owned_descriptor_returns"] = len(cleanup) == 2 and all(row["fd_before"] == row["fd_after"] for row in cleanup)
    if env_inputs.get("SHARDS_NATIVE_WRITE_PRESSURE") == "required":
        outcome["checks"]["actual_short_send"] = len(flows) == 1 and flows[0]["to_backend"]["partial_writes"] > 0
        outcome["checks"]["actual_send_would_block"] = len(flows) == 1 and flows[0]["to_backend"]["write_would_block"] > 0
    outcome["passed"] = all(outcome["checks"].values()) and "launcher_error" not in outcome
    (args.record / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    print(json.dumps(outcome), flush=True)
    return 0 if outcome["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
