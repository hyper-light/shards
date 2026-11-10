#!/usr/bin/env python3
"""One finite ordinary GNU corpus, scoped lint, and musl compile gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import subprocess
import tempfile
import time


def sha(data):
    return hashlib.sha256(data).hexdigest()


def write_new(path, body):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(body)


def command(argv, cwd, env, prefix, deadline):
    row = {"argv": argv, "cwd": str(cwd), "deadline_monotonic": deadline,
           "timed_out": False, "cleanup": []}
    row["controller_fds_before"] = len(os.listdir("/proc/self/fd"))
    with prefix.with_suffix(".stdout").open("xb") as out, prefix.with_suffix(".stderr").open("xb") as err:
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                 stdout=out, stderr=err, start_new_session=True)
        row["pid"] = child.pid
        try:
            row["exit"] = child.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            row["timed_out"] = True
            # Timeout retains the still-owned unreaped child/session identity.
            # This is only this command's process group, never a host-wide sweep.
            try:
                os.killpg(child.pid, signal.SIGKILL)
                row["cleanup"].append("owned command group SIGKILL sent")
            except OSError as error:
                row["cleanup"].append({"kill_errno": error.errno})
            try:
                row["exit"] = child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                row["exit"] = None
                row["cleanup"].append("owned command reap unverified")
    row["controller_fds_after"] = len(os.listdir("/proc/self/fd"))
    row["stdout_sha256"] = sha(prefix.with_suffix(".stdout").read_bytes())
    row["stderr_sha256"] = sha(prefix.with_suffix(".stderr").read_bytes())
    write_new(prefix.with_suffix(".json"), (json.dumps(row, indent=2) + "\n").encode())
    return row


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--seconds", type=int, required=True)
    args = parser.parse_args()
    if args.seconds <= 0:
        parser.error("finite positive scope required")
    deadline = time.monotonic() + args.seconds
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    available = sorted(os.sched_getaffinity(0))
    if len(available) < 2:
        raise RuntimeError("two admitted CPUs required")
    os.sched_setaffinity(0, available[:2])
    version = json.loads(args.bundle.read_text())["version"]
    source = root / "source"
    source.mkdir()
    for name, value in version["files"].items():
        path = Path(name)
        body = value["text"].encode()
        if path.is_absolute() or ".." in path.parts or sha(body) != value["sha256"]:
            raise ValueError("source identity/path mismatch")
        write_new(source / path, body)
    raw = root / "raw"
    raw.mkdir()
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("DYLD_", "CARGO_", "SHARDS_", "SCCACHE_"))
           and k not in ["RUSTFLAGS", "RUSTDOCFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                         "RUSTC", "RUSTDOC", "RUSTUP_TOOLCHAIN"]}
    # TempDir adds shards-tests/PID/case names. Keep this newly owned parent
    # short enough for the unchanged native Unix-socket test paths.
    temporary = Path(tempfile.mkdtemp(prefix="cs-", dir="/tmp"))
    env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_TARGET_DIR=str(root / "target"),
               CARGO_BUILD_JOBS="2", CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS="1",
               RUSTFLAGS="-Cllvm-args=-threads=1", TMPDIR=str(temporary))
    cargo = ["cargo", "+1.98.1"]
    common = ["--locked", "-j2", "--profile", "test-release"]
    corpus = ["-p", "shards-api", "--lib", "--test", "sim"]
    gnu = ["--target", "x86_64-unknown-linux-gnu"]
    row = {"revision": version["revision"], "source": str(source), "deadline_monotonic": deadline,
           "cpu_affinity": sorted(os.sched_getaffinity(0)),
           "memory_disk_admission_is_not_enforced_quota": True,
           "temporary": {"path": str(temporary), "created_new": True, "mode": oct(temporary.stat().st_mode & 0o777)},
           "source_identities": {name: {k: v for k, v in value.items() if k != "text"}
                                 for name, value in version["files"].items()},
           "environment": {key: env[key] for key in ["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS",
                                                    "CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS", "RUSTFLAGS", "TMPDIR"]},
           "commands": []}
    stages = [
        ("gnu-compile", cargo + ["test"] + common + gnu + corpus + ["--no-run", "--message-format=json"]),
        ("gnu-list", cargo + ["test"] + common + gnu + corpus + ["--", "--list"]),
        # Existing reader tests compare process-wide /proc/self/fd snapshots.
        # Serialize the ordinary corpus without changing any case/assertion;
        # compile parallelism remains the caller's requested j2.
        ("gnu-lib", cargo + ["test"] + common + gnu + ["-p", "shards-api", "--lib", "--", "--test-threads=1", "--nocapture"]),
        ("gnu-sim", cargo + ["test"] + common + gnu + ["-p", "shards-api", "--test", "sim", "--", "--test-threads=1", "--nocapture"]),
        ("gnu-clippy", cargo + ["clippy"] + common + gnu + ["-p", "shards-api", "-p", "shards-afunix",
                                                         "--all-targets", "--", "-D", "warnings"]),
        ("musl-compile", cargo + ["test"] + common + ["--target", "x86_64-unknown-linux-musl"]
                         + corpus + ["--no-run", "--message-format=json"]),
    ]
    for name, argv in stages:
        if time.monotonic() >= deadline:
            row["commands"].append({"stage": name, "unrun": "original gate deadline exhausted"})
            continue
        result = command(argv, source, env, raw / name, deadline)
        result["stage"] = name
        row["commands"].append(result)
        write_new(raw / (name + "-result.json"), (json.dumps(result, indent=2) + "\n").encode())
        if name in ["gnu-lib", "gnu-sim"]:
            text = (raw / (name + ".stdout")).read_text(errors="replace")
            row.setdefault("ordinary", {})[name] = {
                "summaries": re.findall(r"test result: ([^\n]+)", text),
                "ignored_lines": [line for line in text.splitlines() if " ... ignored" in line],
                "skip_lines": [line for line in text.splitlines() if "SKIP" in line]}
    binaries = []
    frozen = root / "frozen"
    frozen.mkdir()
    for name in ["gnu-compile", "musl-compile"]:
        stdout = raw / (name + ".stdout")
        if not stdout.exists():
            continue
        for line in stdout.read_text(errors="replace").splitlines():
            if not line.startswith("{"):
                continue
            item = json.loads(line)
            if item.get("reason") == "compiler-artifact" and item.get("executable"):
                path = Path(item["executable"])
                data = path.read_bytes()
                destination = frozen / (name + "-" + path.name)
                write_new(destination, data)
                destination.chmod(0o500)
                binaries.append({"path": str(path), "frozen": str(destination), "sha256": sha(data)})
    row["binaries"] = binaries
    row["temporary"]["remaining_paths"] = sorted(str(p.relative_to(temporary)) for p in temporary.rglob("*"))
    row["temporary"]["preserved"] = True
    write_new(root / "summary.json", (json.dumps(row, indent=2) + "\n").encode())
    print(json.dumps({"root": str(root), "commands": [{k: v for k, v in r.items()
                                                      if k in ["stage", "exit", "timed_out", "unrun"]}
                                                     for r in row["commands"]],
                      "ordinary": row.get("ordinary"), "temporary": row["temporary"],
                      "binaries": binaries}))
    if any(r.get("exit") != 0 or r.get("timed_out") for r in row["commands"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
