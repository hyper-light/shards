#!/usr/bin/env python3
"""Interleave preserved release binaries on the same native socket workload."""
import hashlib
import os
import pathlib
import subprocess
import time

evidence = pathlib.Path(os.environ["PROXY_EVIDENCE_DIR"])
environment = dict(os.environ, SHARDS_PROXY_SAMPLES="1", SHARDS_PROXY_SAMPLE_MS="100")
environment.pop("SHARDS_PROXY_REQUIRE_IDLE", None)
test = "build::proxy::server::tests::proxy_capacity_wait_costs"
with (evidence / "paired.environment").open("w") as record:
    record.write(subprocess.check_output(["uname", "-a"], text=True))
    record.write(subprocess.check_output(["getconf", "CLK_TCK"], text=True))
    for path in ["/proc/loadavg", "/proc/pressure/cpu", "/proc/sys/kernel/sched_schedstats"]:
        record.write(path + "\n")
        record.write(pathlib.Path(path).read_text())
    record.write(f"started_ns={time.time_ns()}\n")
with (evidence / "paired.binaries").open("w") as identity:
    for name in ["baseline", "green"]:
        binary = evidence / (name + "-native")
        identity.write(hashlib.sha256(binary.read_bytes()).hexdigest() + "  " + str(binary) + "\n")
for name in ["baseline", "green"]:
    (evidence / ("paired-" + name + ".log")).write_text("")
with (evidence / "paired.order").open("w") as order:
    for pair in range(20):
        for name in (["baseline", "green"] if pair % 2 == 0 else ["green", "baseline"]):
            command = [str(evidence / (name + "-native")), test, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
            result = subprocess.run(command, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            order.write(f"pair={pair} variant={name} exit={result.returncode}\n")
            order.flush()
            with (evidence / ("paired-" + name + ".log")).open("a") as log:
                log.write(f"pair={pair} variant={name}\n" + result.stdout)
            assert result.returncode == 0 and "1 passed; 0 failed" in result.stdout, (pair, name, result.stdout)
print("paired: 20 baseline and 20 changed runs passed", flush=True)
environment.update(SHARDS_PROXY_SAMPLES="200", SHARDS_PROXY_SAMPLE_MS="1")
command = [str(evidence / "green-native"), test, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
with (evidence / "stress.log").open("w") as log:
    result = subprocess.run(command, env=environment, stdout=log, stderr=subprocess.STDOUT)
(evidence / "stress.exit").write_text(str(result.returncode) + "\n")
assert result.returncode == 0, ("stress", result.returncode)
print("stress: 200 capacity/refill/stop/descriptor-return sessions passed", flush=True)
with (evidence / "paired.environment").open("a") as record:
    record.write(f"ended_ns={time.time_ns()}\n")
    record.write(pathlib.Path("/proc/loadavg").read_text())
    record.write(pathlib.Path("/proc/pressure/cpu").read_text())
