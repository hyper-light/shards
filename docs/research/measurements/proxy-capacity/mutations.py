#!/usr/bin/env python3
"""Independently remove proxy guards, retain each actual failure, then restore."""
import difflib
import hashlib
import os
import pathlib
import subprocess

source = pathlib.Path("crates/shards/src/build/proxy/server.rs")
original = source.read_text()
evidence = pathlib.Path(os.environ["PROXY_EVIDENCE_DIR"])
evidence.mkdir(parents=True, exist_ok=True)
environment = dict(os.environ, SHARDS_HELPERS="skip", SHARDS_PROXY_SAMPLES="1", SHARDS_PROXY_SAMPLE_MS="10")
environment.pop("SHARDS_PROXY_REQUIRE_IDLE", None)
cases = [
    (
        "completion-removed",
        "        if full && removed {\n            self.notify_capacity();\n        }",
        "        if full && removed {\n            // Mutation: omit the completion publication.\n        }",
        "a_full_proxy_refills_on_worker_completion_and_stops_with_a_queued_socket",
        "Timeout",
    ),
    (
        "listener-always-armed",
        "wait_readable(if full { &controls } else { &all }, None)",
        "wait_readable(if full && false { &controls } else { &all }, None)",
        "proxy_capacity_wait_costs",
        "questions.asks.try_recv()",
    ),
    (
        "identity-wrap",
        "id.checked_add(1).ok_or(Admission::IdentityExhausted)?",
        "id.wrapping_add(1)",
        "exhausted_proxy_connection_identity_grants_no_connection",
        "exhausted connection identity was not refused",
    ),
    (
        "notification-failure-not-closed",
        '                shards_vmm::debug!("the build\'s proxy capacity: {e}");\n                self.end();\n                return;',
        '                shards_vmm::debug!("the build\'s proxy capacity: {e}");\n                // Mutation: return without closing owned connections.\n                return;',
        "a_broken_completion_path_closes_admission_and_owned_connections",
        "assertion `left == right` failed",
    ),
]
try:
    for name, old, new, test, expected in cases:
        assert original.count(old) == 1, (name, "source context changed")
        changed = original.replace(old, new, 1)
        patch = "".join(difflib.unified_diff(original.splitlines(True), changed.splitlines(True), fromfile="a/" + str(source), tofile="b/" + str(source)))
        (evidence / (name + ".patch")).write_text(patch)
        source.write_text(changed)
        (evidence / (name + ".source")).write_text(hashlib.sha256(source.read_bytes()).hexdigest() + "  " + str(source) + "\n")
        command = ["cargo", "test", "-j", "4", "--profile", "test-release", "-p", "shards", "--bin", "shards", "build::proxy::server::tests::" + test, "--", "--exact", "--nocapture", "--test-threads=1"]
        if test == "proxy_capacity_wait_costs":
            command.append("--ignored")
        print(name + ": " + " ".join(command), flush=True)
        with (evidence / (name + ".log")).open("w") as log:
            result = subprocess.run(command, env=environment, stdout=log, stderr=subprocess.STDOUT)
        (evidence / (name + ".exit")).write_text(str(result.returncode) + "\n")
        output = (evidence / (name + ".log")).read_text()
        print(output[-4000:], flush=True)
        assert result.returncode == 101 and expected in output and "0 passed; 1 failed" in output, (name, "mutation did not fail at its guard")
        source.write_text(original)
finally:
    source.write_text(original)

command = ["cargo", "test", "-j", "4", "--profile", "test-release", "-p", "shards", "--bin", "shards", "build::proxy::server::tests", "--", "--nocapture", "--test-threads=1"]
print("restored: " + " ".join(command), flush=True)
with (evidence / "mutations-restored.log").open("w") as log:
    result = subprocess.run(command, env=environment, stdout=log, stderr=subprocess.STDOUT)
(evidence / "mutations-restored.exit").write_text(str(result.returncode) + "\n")
print((evidence / "mutations-restored.log").read_text()[-6000:], flush=True)
assert result.returncode == 0, "restored source failed"
