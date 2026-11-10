#!/usr/bin/env python3
"""One bounded, owned native Direction case; source-linked Rust owns the sockets."""

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
    "turn-pressure": "managed_network::intercept::shared::native_turn_tests::native_direction_turn_retains_suffix_under_native_pressure",
    "turn-pending-cancel": "managed_network::intercept::shared::native_turn_tests::native_direction_turn_pending_cancel_retains_suffix",
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
    parser.add_argument("--case", choices=FILTERS, default="turn-pressure")
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
    prefill = [r for r in rows if r.get("type") == "NativeTurnPrefill"]
    snapshots = [r for r in rows if r.get("type") == "NativeTurnSnapshot"]
    source = [r for r in snapshots if r["stage"] == "SourceReceived"]
    blocked = [r for r in snapshots if r["stage"] == "BlockedSend"]
    recovery = [r for r in snapshots if r["stage"] == "Recovery"]
    cleanup = [r for r in rows if r.get("type") == "NativeTurnCleanup"]
    receipts = {r["role"]: r for r in rows if r.get("type") == "NativeTurnBytes"}
    request = payload(env_inputs["SHARDS_TCP_REQUEST_HEX"])
    envelope = int(env_inputs["SHARDS_TURN_PREFILL_BYTES"])
    seed = payload(env_inputs["SHARDS_TURN_PREFILL_SEED_HEX"])
    full_prefill = (seed * ((envelope + len(seed) - 1) // len(seed)))[:envelope]
    accepted = prefill[0]["accepted"] if len(prefill) == 1 else 0
    prefix = full_prefill[:accepted]
    state = ("half", "read", "written", "pending", "sent", "extra_consumed", "suffix_hex")
    terminal_name = "CancelBeforeRecovery" if args.case == "turn-pending-cancel" else "Forward"
    outcome["checks"] = {
        "one_actual_test": "1 passed; 0 failed;" in raw and "running 1 test" in raw,
        "child_success": outcome.get("status") == 0 and outcome["pidfd_exit_ready"] and not outcome["timeout"],
        "actual_prefill_send_would_block": len(prefill) == 1 and prefill[0]["would_block"] and prefill[0]["error_kind"] == "Some(WouldBlock)" and 0 < accepted <= envelope,
        "independent_prefill_receipt": all(k in receipts for k in ("PrefillAccepted", "SourceAccepted", "PrefillDrained")) and bytes.fromhex(receipts["PrefillAccepted"]["hex"]) == bytes.fromhex(receipts["PrefillDrained"]["hex"]) == prefix,
        "pending_from_actual_source_recv": len(source) == 1 and receipts.get("SourceAccepted", {}).get("hex") == request.hex() and source[0]["turn_result"] == "Ok(true)" and source[0]["half"] == "Reading" and 0 < source[0]["pending"] == source[0]["read"] <= len(request) and source[0]["written"] == source[0]["sent"] == source[0]["extra_consumed"] == 0 and bytes.fromhex(source[0]["suffix_hex"]) == request[:source[0]["read"]],
        "one_real_block_preserves_suffix_cursors": len(source) == len(blocked) == 1 and blocked[0]["turn_result"] == "Ok(false)" and all(source[0][k] == blocked[0][k] for k in state) and blocked[0]["write_would_block"] == source[0]["write_would_block"] + 1 and blocked[0]["would_block"] == source[0]["would_block"] + 1,
        "owned_child_reaped": outcome["reaped"] and not outcome.get("forced_owned_cleanup", False),
        "parent_fd_return": outcome["fd_after"] == before,
        "child_fd_return_original_deadline_no_extra_workers": len(cleanup) == 1 and cleanup[0]["terminal"] == terminal_name and cleanup[0]["fd_before"] == cleanup[0]["fd_after"] and cleanup[0]["original_remaining_ns"] > 0 and cleanup[0]["additional_workers"] == 0 and cleanup[0]["result_ok"] and cleanup[0]["failure_kind"] == "None",
        "binary_unchanged": outcome["binary_after_sha256"] == initial_hash,
    }
    if args.case == "turn-pressure":
        terminal = [r for r in rows if r.get("type") == "NativeTurnTerminal"]
        outcome["checks"].update({
            "positive_recovery_without_repeated_would_block": len(blocked) == 1 and len(recovery) > 0 and any(r["written"] > 0 for r in recovery) and all(r["turn_result"] == "Ok(true)" and r["write_would_block"] == blocked[0]["write_would_block"] and r["would_block"] == blocked[0]["would_block"] and r["read"] == r["written"] + r["pending"] for r in recovery),
            "exact_destination_prefix_plus_payload": "DestinationReceived" in receipts and bytes.fromhex(receipts["DestinationReceived"]["hex"]) == prefix + request,
            "actual_eof_and_payload_only_counters": len(terminal) == 1 and terminal[0]["eof_read"] == 0 and terminal[0]["prefill_bytes"] == accepted and terminal[0]["relayed_bytes"] == terminal[0]["read"] == terminal[0]["written"] == len(request) and terminal[0]["pending"] == terminal[0]["extra_consumed"] == 0 and terminal[0]["half"] == "WriteClosed(Eof)",
        })
    else:
        signal = [r for r in rows if r.get("type") == "NativeTurnCancelSignal"]
        cancelled = [r for r in snapshots if r["stage"] == "PendingCancel"]
        terminal = [r for r in rows if r.get("type") == "NativeTurnCancelTerminal"]
        outcome["checks"].update({
            "same_actual_eventfd_signal8_before_attempt": len(signal) == 1 and signal[0]["bytes"] == 8 and signal[0]["value"] == 1 and signal[0]["same_cancel_fd"] and signal[0]["original_remaining_ns"] > 0,
            "signal_turn_probe_cleanup_order": len(signal) == len(cancelled) == len(terminal) == len(cleanup) == 1 and "CancelledBackendProbe" in receipts and rows.index(signal[0]) < rows.index(cancelled[0]) < rows.index(receipts["CancelledBackendProbe"]) < rows.index(terminal[0]) < rows.index(cleanup[0]),
            "typed_cancelled_retains_actual_suffix_and_counters": len(blocked) == len(cancelled) == 1 and cancelled[0]["turn_result"] == "Err(Cancelled)" and all(cancelled[0][k] == blocked[0][k] for k in state + ("would_block", "write_would_block")) and len(recovery) == 0,
            "independent_nonempty_probe_eof_no_payload": "CancelledBackendProbe" in receipts and receipts["CancelledBackendProbe"]["bytes"] == 0 and receipts["CancelledBackendProbe"]["hex"] == "" and len(terminal) == 1 and terminal[0]["eof_read"] == 0 and terminal[0]["eof_error_kind"] == "None",
            "cancel_terminal_preserves_all_actual_counters": len(signal) == len(terminal) == len(blocked) == 1 and terminal[0]["typed_cancelled"] and terminal[0]["source_accepted"] == len(request) and terminal[0]["prefill_bytes"] == accepted and terminal[0]["read"] == blocked[0]["read"] and terminal[0]["pending"] == blocked[0]["pending"] and terminal[0]["half"] == blocked[0]["half"] and terminal[0]["written"] == terminal[0]["sent"] == terminal[0]["extra_consumed"] == 0 and all(terminal[0][k] == signal[0][k + "_before"] for k in ("would_block", "write_would_block", "partial_writes")),
        })
    outcome["passed"] = all(outcome["checks"].values()) and "launcher_error" not in outcome
    (args.record / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    print(json.dumps(outcome), flush=True)
    return 0 if outcome["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
