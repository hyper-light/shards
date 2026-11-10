#!/usr/bin/env python3
"""ROOT-executed managed_output cost pairs. Implementer runs pure tests only."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import time


TEST = "cost::owner_output_resources_at_explicit_member_count"
ARTIFACTS = ("fixture", "cli", "testguest", "kernel", "init")
ROLES = ("observer_parent", "daemon", "vm")
PHASES = (
    "before_fixture",
    "after_start_and_watch_acquisition",
    "owner_ready_before_recording",
    "after_proc_recording",
    "after_observer_completion",
    "after_shutdown_watch_release_and_fd_sample",
)
CUMULATIVE = (
    "alloc_successes", "alloc_failures", "zeroed_successes", "zeroed_failures",
    "realloc_successes", "realloc_failures", "dealloc_calls",
    "allocation_requested_bytes", "allocation_success_bytes", "realloc_requested_bytes",
    "realloc_grown_bytes", "realloc_shrunk_bytes", "deallocated_bytes",
)
GAUGES = ("live_requested_bytes", "peak_recorded_live_requested_bytes")
PUBLIC_ENV = {"PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ"}
RECORDS = (
    "inputs", "ready", "snapshots", "control", "teardown", "exit-watches.ready",
    "terminal", "allocation-phases", "allocation-final",
)


class Refusal(Exception):
    def __init__(self, reason):
        self.reason = reason
        super().__init__(reason)


def require(condition, reason):
    if not condition:
        raise Refusal(reason)


def integer(value, minimum=0):
    return type(value) is int and value >= minimum


def digest(value):
    require(isinstance(value, str), "MalformedDigest")
    raw = value.removeprefix("sha256:")
    require(re.fullmatch(r"[0-9a-f]{64}", raw) is not None, "MalformedDigest")
    return raw


def file_digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "DuplicateJsonKey")
        result[key] = value
    return result


def load_json(path):
    def invalid_constant(_):
        raise Refusal("NonFiniteJsonNumber")
    with Path(path).open(encoding="utf-8") as stream:
        return json.load(stream, object_pairs_hook=unique_object, parse_constant=invalid_constant)


def write_json(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")


def absolute(value):
    require(isinstance(value, str) and Path(value).is_absolute(), "AbsolutePathRequired")
    return Path(value)


def validate_manifest(value):
    require(isinstance(value, dict), "ManifestObjectRequired")
    require(value.get("schema") == "managed-output-cost-pair-manifest-v1", "UnsupportedManifest")
    require(isinstance(value.get("variant"), str) and re.fullmatch(r"[A-Za-z0-9_-]+", value["variant"]), "MalformedVariant")
    source = value.get("source", {})
    require(isinstance(source, dict), "SourceObjectRequired")
    for field in ("revision", "tree"):
        require(isinstance(source.get(field), str) and re.fullmatch(r"[0-9a-f]{40}", source[field]), "ExactGitIdentityRequired")
    absolute(source.get("directory"))
    require(isinstance(value.get("artifacts"), dict) and set(value["artifacts"]) == {*ARTIFACTS, "build_record", "workload_record"}, "ExactArtifactRolesRequired")
    for artifact in (*ARTIFACTS, "build_record", "workload_record"):
        item = value.get("artifacts", {}).get(artifact, {})
        require(isinstance(item, dict), "ArtifactObjectRequired")
        absolute(item.get("path"))
        digest(item.get("sha256"))
    guest = absolute(value.get("test_guests")) / "guest" / "shards-testguest"
    require(guest == Path(value["artifacts"]["testguest"]["path"]), "PrebuiltTestGuestRequired")
    env = value.get("environment")
    require(isinstance(env, dict) and set(env) <= PUBLIC_ENV, "OnlyExplicitPublicEnvironmentAllowed")
    require(all(isinstance(v, str) for v in env.values()), "EnvironmentStringsRequired")
    require(bool(env.get("PATH")) and bool(env.get("HOME")), "ExplicitPathAndHomeRequired")
    return value


def validate_pair(a, b):
    require(a["variant"] != b["variant"], "DistinctVariantsRequired")
    require(a["environment"] == b["environment"], "DifferentRuntimeEnvironment")
    # The reviewed workload descriptor can account for build-coupled ABI
    # differences in guest binaries. A manifest is not proof of equivalence.
    for role in ("kernel", "workload_record"):
        require(digest(a["artifacts"][role]["sha256"]) == digest(b["artifacts"][role]["sha256"]), "DifferentWorkloadOrKernel")


def verify_manifest_files(manifest):
    for item in manifest["artifacts"].values():
        require(file_digest(item["path"]) == digest(item["sha256"]), "ArtifactDigestMismatch")


def verify_source(manifest, timeout_seconds):
    source = manifest["source"]
    def git(*args):
        result = subprocess.run(["git", "-C", source["directory"], *args], capture_output=True, text=True, timeout=timeout_seconds, check=False)
        require(result.returncode == 0, "SourceQueryOrTrackedStateFailed")
        return result.stdout.strip()
    found = {"revision": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}")}
    require(found == {k: source[k] for k in found}, "SourceIdentityMismatch")
    git("diff", "--quiet", "HEAD", "--")
    return found | {"tracked_state": "Clean", "untracked_and_build_history": "Unverified; root reviews supplied build record"}


def schedule(n):
    require(integer(n, 1), "PositiveCallerPairCountRequired")
    for pair in range(n):
        # Two variants and opposite order on successive pairs, not a chosen
        # performance sample count. Odd n has one extra AB order, reported.
        for position, variant in enumerate(("A", "B") if pair % 2 == 0 else ("B", "A")):
            yield pair, position, variant


def identity(value):
    require(isinstance(value, (list, tuple)) and len(value) == 2, "MalformedProcessIdentity")
    require(integer(value[0], 1) and integer(value[1]), "MalformedProcessIdentity")
    return tuple(value)


def held_identity(value):
    return identity([value.get("pid"), value.get("expected_started")])


def observed_count(value):
    require(isinstance(value, dict) and value.get("state") == "Observed" and integer(value.get("count")), "FdCountUnavailable")
    return value["count"]


def validate_case(manifest, members, deadline_ms, data):
    inputs = data["inputs"]
    require(inputs.get("source") == manifest["source"]["revision"], "ExecutedSourceMismatchOrMissing")
    require(integer(inputs.get("members")) and integer(inputs.get("phase_deadline_ms"), 1) and inputs["members"] == members and inputs["phase_deadline_ms"] == deadline_ms, "ExecutedWorkloadMismatch")
    for role in ARTIFACTS:
        require(digest(inputs.get(role, {}).get("sha256")) == digest(manifest["artifacts"][role]["sha256"]), "ExecutedArtifactMismatch")
        absolute(inputs[role].get("path"))
    ready = data["ready"]
    state = "NoManagedMembers" if members == 0 else "OwnerChannelsReady"
    require(ready.get("state") == state and integer(ready.get("endpoints")) and ready["endpoints"] == 2 * members, "ReadinessContractMismatch")
    require(isinstance(ready.get("run"), str) and re.fullmatch(r"[0-9a-fA-F]+", ready["run"]), "MissingRunIdentity")
    snapshots = data["snapshots"]
    require(snapshots.get("checkpoint") == ready, "DifferentReadinessCheckpoint")
    identities = {}
    for role in ROLES:
        sample = snapshots.get(role, {})
        expected = identity([sample.get("pid"), sample.get("expected_start")])
        require(sample.get("identity_state") == "MatchedBeforeAndAfter", "IdentityUnverified")
        require(identity(sample.get("before_identity")) == expected == identity(sample.get("after_identity")), "DifferentSampleIdentity")
        identities[role] = expected
    require(len(set(identities.values())) == len(ROLES), "RolesShareIdentity")
    control = data["control"]
    require(control.get("state") == "MainProgressAfterObserverCancel" and integer(control.get("members")) and integer(control.get("exact_payload_channels")) and control["members"] == members and control["exact_payload_channels"] == 2 * members, "ControlProgressUnverified")
    teardown = data["teardown"]
    require(teardown.get("protocol_completed") is True and teardown.get("owner_stop_and_observer_join") == {"Ok": None}, "ProtocolOrOwnedCleanupFailed")
    require(teardown.get("cleanup_phase_deadline_ms") == deadline_ms, "CleanupDeadlineMismatch")
    exits = teardown.get("host_process_exit", {})
    require(exits.get("both_valid_exit_events") is True, "ExitEventsUnverified")
    for role in ("daemon", "vm"):
        held = data["exit-watches.ready"].get(role, {})
        require(held.get("state") == "Watching" and held.get("readiness_identity", {}).get("state") == "Matched", "HeldReadinessUnverified")
        require(held_identity(held.get("identity", {})) == identities[role], "HeldReadinessIdentityMismatch")
        end = exits.get(role, {})
        require(end.get("state") == "ExitReadyObservedAfterOwnerStopWithinCleanupDeadline", "ExitDeadlineOrIdentityFailed")
        require(held_identity(end.get("identity", {})) == identities[role], "ExitIdentityMismatch")
    fds = teardown.get("exit_observation_fds", {})
    baseline = observed_count(fds.get("baseline"))
    held = fds.get("owned_observation_fds")
    # Existing Linux proof owns one epoll and (proc directory + pidfd) per role.
    require(held == 1 + 2 * len(("daemon", "vm")), "OwnedExitObjectsMismatch")
    require(observed_count(fds.get("with_exit_objects")) == baseline + held and observed_count(fds.get("after_release")) == baseline and fds.get("exact_delta_and_return") is True, "ExactFdReturnFailed")
    terminal = data["terminal"]
    if members == 0:
        require(terminal.get("state") == "NoManagedMembers" and integer(terminal.get("managed_channels")) and terminal["managed_channels"] == 0, "ZeroMemberTerminalMismatch")
    else:
        channels = terminal.get("channels")
        require(terminal.get("state") == "OwnerChannelsAborted" and isinstance(channels, list) and len(channels) == 2 * members, "TerminalChannelsMismatch")
        expected = ((member, channel) for member in range(1, members + 1) for channel in (1, 2))
        for key, channel in zip(expected, channels, strict=True):
            require(integer(channel.get("member"), 1) and integer(channel.get("channel"), 1), "TerminalMemberMappingMismatch")
            require((channel.get("member"), channel.get("channel")) == key, "TerminalMemberMappingMismatch")
            accepted, consumed = channel.get("accepted"), channel.get("consumed")
            require(integer(accepted) and integer(consumed) and accepted >= consumed, "TerminalCursorMismatch")
            require((accepted > consumed) if key == (1, 1) else (accepted == consumed), "TerminalConsumptionMismatch")
            require(channel.get("tail") == "unknown", "TerminalContractMismatch")
    return identities


def unknown(reason):
    return {"state": "Unknown", "reason": reason}


def counter(value):
    require(isinstance(value, dict), "MissingAllocationCounter")
    if value.get("state") == "Unknown":
        return unknown(value.get("reason", "CounterUnknown"))
    require(value.get("state") == "Known" and integer(value.get("value")) and value["value"] < (1 << 64) - 1, "MalformedAllocationCounter")
    return {"state": "Known", "value": value["value"]}


def counter_difference(before, after):
    if before["state"] != "Known" or after["state"] != "Known":
        return unknown("CounterUnknownAtBoundary")
    if after["value"] < before["value"]:
        return unknown("CumulativeCounterDecreased")
    return {"state": "Known", "value": after["value"] - before["value"]}


def allocation_metrics(data):
    phases = data["allocation-phases"]
    for phase in PHASES:
        sample = phases.get(phase, {})
        require(sample.get("state") == "Observed" and sample.get("semantics") == "ConcurrentNonAtomic", "AllocationPhaseMissingOrAtomicClaim")
    final = data["allocation-final"]
    require(final.get("phase") == "after_terminal_records_and_fixture_drop", "AllocationFinalPhaseMismatch")
    sample = final.get("snapshot", {})
    require(sample.get("state") == "Observed" and sample.get("semantics") == "ConcurrentNonAtomic", "AllocationFinalMissingOrAtomicClaim")
    result = {}
    before = phases["before_fixture"]
    for name in CUMULATIVE:
        first = counter(before.get(name))
        for phase in ("owner_ready_before_recording", "after_shutdown_watch_release_and_fd_sample"):
            result[f"observer_rust.{phase}.{name}.since_before_fixture"] = counter_difference(first, counter(phases[phase].get(name)))
    for name in GAUGES:
        for phase in ("owner_ready_before_recording", "after_shutdown_watch_release_and_fd_sample"):
            result[f"observer_rust.{phase}.{name}.absolute"] = counter(phases[phase].get(name))
        result[f"observer_rust.after_fixture_drop.{name}.absolute"] = counter(sample.get(name))
    # Native deltas are optional coverage, never silently replaced by zero.
    native = data.get("native-fault-phases", {}).get("start_to_ready", {})
    for role in ROLES:
        value = native.get(role, {})
        native_identity = value.get("identity")
        expected_identity = [data["snapshots"][role]["pid"], data["snapshots"][role]["expected_start"]]
        if value.get("state") == "ConditionalDifference" and integer(value.get("value")) and value.get("source") == "LinuxMinfltPlusMajfltViaPlatformUsage" and value.get("native_bits") == 64 and native_identity == expected_identity:
            result[f"native_faults.{role}.start_to_ready"] = value | {"state": "Conditional"}
        else:
            result[f"native_faults.{role}.start_to_ready"] = unknown(value.get("reason", "NativeFaultPhaseUnavailable"))
    return result


def distribution(values, requested):
    require(integer(requested, 1) and len(values) <= requested, "InvalidDistributionCoverage")
    known = [item["value"] for item in values if item.get("state") in ("Known", "Conditional")]
    result = {"n_requested": requested, "n": len(known), "n_unknown": len(values) - len(known), "n_missing": requested - len(values), "n_conditional": sum(item.get("state") == "Conditional" for item in values), "unknowns": [item for item in values if item.get("state") == "Unknown"], "method": "inverse empirical CDF/type1: one-based rank ceil(p*n); descriptive only, no confidence interval"}
    if known:
        ordered = sorted(known)
        result.update({f"p{p}": ordered[(p * len(ordered) + 99) // 100 - 1] for p in (50, 90, 99)})
        result["max"] = ordered[-1]
    result["coverage"] = "Incomplete" if result["n_unknown"] or result["n_missing"] else ("Conditional" if result["n_conditional"] else "KnownSamples")
    return result


def summarize(samples, n):
    require(integer(n, 1), "PositiveCallerPairCountRequired")
    seen = set()
    for sample in samples:
        key = (sample["pair"], sample["variant"])
        require(integer(key[0]) and key[0] < n and key[1] in ("A", "B") and key not in seen, "DuplicateOrInvalidSample")
        seen.add(key)
    keys = sorted({key for sample in samples for key in sample.get("metrics", {})})
    output = {"pairs_requested": n, "samples_requested": 2 * n, "validated_samples": len(samples), "schedule": "AB,BA alternating; serial; no warmup or automatic retry", "order_pairs_requested": {"AB": (n + 1) // 2, "BA": n // 2}, "limitations": "same-N fixture, non-atomic phases, observer Rust only; native faults conditional; parent/kernel/foreign C/process-tree and instrumentation overhead remain separately root-accounted", "variants": {}, "paired_B_minus_A": {}}
    for variant in ("A", "B"):
        rows = [sample for sample in samples if sample["variant"] == variant]
        output["variants"][variant] = {key: distribution([row["metrics"].get(key, unknown("MetricMissing")) for row in rows], n) for key in keys}
    by_pair = {(sample["pair"], sample["variant"]): sample for sample in samples}
    for key in keys:
        differences = []
        for pair in range(n):
            left, right = by_pair.get((pair, "A")), by_pair.get((pair, "B"))
            if left is None or right is None:
                continue
            a, b = left["metrics"].get(key, unknown("MetricMissing")), right["metrics"].get(key, unknown("MetricMissing"))
            if a["state"] == "Unknown" or b["state"] == "Unknown":
                differences.append(unknown("PairedMetricUnknown"))
            else:
                differences.append({"state": "Conditional" if "Conditional" in (a["state"], b["state"]) else "Known", "value": b["value"] - a["value"]})
        output["paired_B_minus_A"][key] = distribution(differences, n)
    return output


def read_case(path):
    data = {name: load_json(path / f"{name}.json") for name in RECORDS}
    native = path / "native-fault-phases.json"
    if native.exists():
        data["native-fault-phases"] = load_json(native)
    return data


def hash_records(path):
    rows = []
    for item in sorted(path.rglob("*")):
        require(not item.is_symlink(), "RecordSymlinkRefused")
        if item.is_file():
            rows.append({"path": str(item.relative_to(path)), "bytes": item.stat().st_size, "sha256": file_digest(item)})
    return rows


def run_case(manifest, workload, directory):
    directory.mkdir()
    records = directory / "fixture-records"  # Fixture creates this NEW path.
    env = manifest["environment"] | {"SHARDS_REQUIRE_VMS": "1", "SHARDS_TEST_KERNEL": manifest["artifacts"]["kernel"]["path"], "SHARDS_TEST_GUESTS": manifest["test_guests"], "SHARDS_OUTPUT_COST_MEMBERS": str(workload.members), "SHARDS_OUTPUT_COST_RECORDS": str(records), "SHARDS_OUTPUT_COST_DEADLINE_MS": str(workload.phase_deadline_ms)}
    command = [manifest["artifacts"]["fixture"]["path"], "--exact", TEST, "--ignored", "--nocapture", "--test-threads=1"]
    write_json(directory / "command.json", {"command": command, "environment": env, "cwd": manifest["source"]["directory"], "case_timeout_ms": workload.case_timeout_ms, "timeout_scope": "each external Git command and fixture child individually; synchronous file IO is not interruptibly bounded"})
    started = time.perf_counter_ns()
    verdict = {"status": "NotStarted", "cleanup": "NotStarted"}
    stage = "Preflight"
    try:
        write_json(directory / "source-before.json", verify_source(manifest, workload.case_timeout_ms / 1000))
        verify_manifest_files(manifest)
        with (directory / "stdout.log").open("xb") as stdout, (directory / "stderr.log").open("xb") as stderr:
            child_start = time.perf_counter_ns()
            verdict["status"] = "Running"
            verdict["cleanup"] = "UnverifiedAfterLaunchAttempt"
            stage = "Fixture"
            result = subprocess.run(command, cwd=manifest["source"]["directory"], env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, timeout=workload.case_timeout_ms / 1000, check=False)
            child_ns = time.perf_counter_ns() - child_start
        verdict.update({"child_exit": result.returncode, "child_elapsed_ns": child_ns, "cleanup": "Unverified"})
        stage = "Postflight"
        require(result.returncode == 0, "FixtureFailed")
        data = read_case(records)
        verdict["identities"] = validate_case(manifest, workload.members, workload.phase_deadline_ms, data)
        verdict["cleanup"] = "OwnedStopJoinExitReadinessAndExactFdReturn"
        write_json(directory / "source-after.json", verify_source(manifest, workload.case_timeout_ms / 1000))
        verify_manifest_files(manifest)
        verdict["metrics"] = allocation_metrics(data) | {"fixture_child_elapsed_ns": {"state": "Known", "value": child_ns}}
        verdict["status"] = "Validated"
    except (Refusal, OSError, ValueError, KeyError, TypeError, AttributeError, OverflowError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        verdict.update({"status": "Refused", "stage": stage, "reason": error.reason if isinstance(error, Refusal) else type(error).__name__, "continue": False})
        if isinstance(error, subprocess.TimeoutExpired) and stage == "Fixture":
            verdict["cleanup"] = "UnknownAfterOuterTimeout; root owns reconciliation; no subsequent case"
        if isinstance(error, KeyboardInterrupt):
            verdict["cleanup"] = "UnknownAfterInterrupt; root owns reconciliation; no subsequent case"
    finally:
        if verdict["status"] not in ("Validated", "Refused"):
            verdict.update({"status": "Refused", "stage": stage, "reason": "UnexpectedIncompleteCase", "continue": False})
        verdict["case_through_validation_ns"] = time.perf_counter_ns() - started
        write_json(directory / "verdict.json", verdict)
        write_json(directory / "raw-manifest.json", hash_records(directory))
    return verdict


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("a", "b", "records"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    for name in ("n", "members", "phase-deadline-ms", "case-timeout-ms"):
        parser.add_argument(f"--{name}", required=True, type=int)
    args = parser.parse_args(argv)
    require(sys.platform == "linux", "ExistingCostCallerIsLinuxOnly")
    require(integer(args.n, 1) and integer(args.members) and args.members < (1 << 32) - 1, "CallerCountNotRepresentable")
    require(integer(args.phase_deadline_ms, 1) and args.phase_deadline_ms <= (1 << 64) - 1, "CallerDeadlineNotRepresentable")
    require(integer(args.case_timeout_ms, 1) and math.isfinite(args.case_timeout_ms / 1000), "CallerOuterTimeoutNotFinite")
    manifests = {"A": validate_manifest(load_json(args.a)), "B": validate_manifest(load_json(args.b))}
    validate_pair(manifests["A"], manifests["B"])
    absolute(str(args.records))
    args.records.mkdir()  # Never reuse or overwrite records; no automatic resume.
    write_json(args.records / "request.json", {"n": args.n, "members": args.members, "phase_deadline_ms": args.phase_deadline_ms, "case_timeout_ms": args.case_timeout_ms, "python": sys.version, "platform": platform.platform(), "clock": vars(time.get_clock_info("perf_counter")), "manifest_inputs": {key: {"path": str(path), "sha256": file_digest(path)} for key, path in (("A", args.a), ("B", args.b))}})
    for variant, manifest in manifests.items():
        write_json(args.records / f"manifest-{variant}.json", manifest)
        original = args.a if variant == "A" else args.b
        copied = args.records / f"manifest-{variant}.original.json"
        shutil.copyfile(original, copied)
        require(load_json(copied) == manifest, "ManifestChangedDuringPreflight")
    samples = []
    with (args.records / "samples.jsonl").open("x", encoding="utf-8") as stream:
        for pair, position, variant in schedule(args.n):
            verdict = run_case(manifests[variant], args, args.records / f"pair-{pair}-{position}-{variant}")
            row = {"pair": pair, "position": position, "variant": variant, "verdict": verdict}
            stream.write(json.dumps(row, allow_nan=False) + "\n")
            stream.flush()
            os.fsync(stream.fileno())
            if verdict["status"] == "Validated":
                samples.append({"pair": pair, "variant": variant, "metrics": verdict["metrics"]})
            summary = summarize(samples, args.n)
            write_json(args.records / f"summary-after-{pair}-{position}.json", summary)
            if verdict["status"] != "Validated":
                write_json(args.records / "stopped.json", {"state": "Stopped; no next case", "pair": pair, "variant": variant, "reason": verdict.get("reason"), "cleanup": verdict.get("cleanup"), "partial_summary": summary})
                return 1
    write_json(args.records / "summary.json", summarize(samples, args.n))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refusal, OSError, ValueError, KeyError, TypeError, OverflowError) as error:
        print(json.dumps({"state": "Refused", "reason": error.reason if isinstance(error, Refusal) else type(error).__name__, "actual_campaign_complete": False}), file=sys.stderr)
        raise SystemExit(2)
