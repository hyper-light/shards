"""ROOT-owned finite guest-only cgroup permission observation; no host chown."""
from pathlib import Path
import hashlib
import json
import os
import resource
import select
import shutil
import subprocess
import sys
import time

root = Path.home() / "root-native-cgroup-birth-v2"
root.mkdir(exist_ok=False)
deadline = time.monotonic() + 600
source = root / "guest.c"
source.write_bytes(bytes.fromhex(sys.argv[1]))
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
source_sha = sha(source)
old = Path.home() / "root-managed-events-de5e4fbe9c5c-v1"
kernel = old / "target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
assert sha(kernel) == "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
cli = old / "target/test-release/shards"
vm = old / "target/helpers/x86_64-unknown-linux-musl/test-release/shards-vm"
assert os.getuid() != 0 and os.access("/dev/kvm", os.R_OK | os.W_OK)
assert os.sched_getaffinity(0)
os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:2])
resource.setrlimit(resource.RLIMIT_AS, (2 * 1024**3, 2 * 1024**3))
for name in ["docker", "home", "tmp", "cache"]:
    (root / name).mkdir()
env = os.environ.copy()
assert "LD_PRELOAD" not in env
env.pop("LD_LIBRARY_PATH", None)
env.update(DOCKER_CONFIG=str(root / "docker"), SHARDS_HOME=str(root / "home"),
           TMPDIR=str(root / "tmp"), XDG_CACHE_HOME=str(root / "cache"),
           SHARDS_VM_BINARY=str(vm))
identity = {"role": "coordinator", "policy_revision": "06ae067c0beda71c98a8c6f9633df9feb71fc11b",
            "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
            "uid": os.getuid(), "uname": list(os.uname()), "envelope": "normal 2 CPU / 2 GiB / 4 GiB / 600 s",
            "physical_kernel_caps": "Unverified", "affinity": sorted(os.sched_getaffinity(0)),
            "source_sha256": source_sha,
            "artifacts": {n: {"path": str(p), "bytes": p.stat().st_size, "sha256": sha(p)}
                          for n, p in [("kernel", kernel), ("cli", cli), ("vm", vm)]}}
(root / "IDENTITY.json").write_text(json.dumps(identity, indent=2) + "\n")

def run(name, argv, seconds):
    remaining = deadline - time.monotonic()
    assert remaining > seconds, (name, remaining)
    before = len(list(Path("/proc/self/fd").iterdir()))
    began = time.monotonic()
    with (root / (name + ".stdout")).open("xb") as out, (root / (name + ".stderr")).open("xb") as err:
        p = subprocess.Popen(argv, cwd=root, env=env, stdout=out, stderr=err)
        pidfd = os.pidfd_open(p.pid)
        poll = select.poll()
        poll.register(pidfd, select.POLLIN)
        ready = bool(poll.poll(int(min(seconds, remaining) * 1000)))
        if not ready:
            p.kill()
        code = p.wait()
        os.close(pidfd)
    try:
        again = os.waitpid(p.pid, os.WNOHANG)
    except ChildProcessError:
        again = "ECHILD"
    receipt = {"argv": argv, "pid": p.pid, "exit_code": code, "timeout": not ready,
               "elapsed_seconds": time.monotonic() - began, "pidfd_exit_ready": ready,
               "direct_child_reaped": p.returncode is not None, "second_wait": again,
               "fds_before": before, "fds_after": len(list(Path("/proc/self/fd").iterdir())),
               "whole_population_kernel_return": "Unknown"}
    (root / (name + ".json")).write_text(json.dumps(receipt, indent=2) + "\n")
    assert ready and code == 0 and before == receipt["fds_after"], receipt

run("gcc-version", ["gcc", "--version"], 15)
run("compile", ["gcc", "-std=gnu11", "-static", "-O2", "-Wall", "-Wextra", "-Werror",
                str(source), "-o", str(root / "guest-init")], 180)
guest = root / "guest-init"
frozen = root / "frozen-init"
shutil.copy2(guest, frozen)
(root / "BEFORE-VM.json").write_text(json.dumps({"source": sha(source), "guest": sha(guest),
    "frozen_guest": sha(frozen), "bytes": guest.stat().st_size}, indent=2) + "\n")
run("guest", [str(cli), "run", "--kernel", str(kernel), "--init", str(frozen),
              "--cpus", "1", "--memory", "256", "--cmdline", "console=ttyS0 panic=-1"], 120)
output = (root / "guest.stdout").read_text(errors="replace")
assert "BIRTH-FAIL" not in output, output
cases = [json.loads(line.split("BIRTH-CASE ", 1)[1]) for line in output.splitlines() if "BIRTH-CASE " in line]
assert len(cases) == 5 and [c["errno"] for c in cases] == [13, 13, 0, 13, 13], cases
assert output.count("BIRTH-PASS ") == 1
assert sha(source) == source_sha and sha(guest) == sha(frozen)
(root / "RESULTS.json").write_text(json.dumps({"cases": cases, "source_sha256": source_sha,
    "guest_sha256": sha(guest), "kernel_sha256": sha(kernel), "host_permission_changes": 0,
    "actor_isolation": "Unverified", "production_helper": "Unrun",
    "remaining_seconds": deadline - time.monotonic()}, indent=2) + "\n")
print(json.dumps({"result": "five_guest_cases_pass", "root": str(root), "cases": cases,
                  "actor_isolation": "Unverified"}), flush=True)
