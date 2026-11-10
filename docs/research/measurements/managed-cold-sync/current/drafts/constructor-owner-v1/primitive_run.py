#!/usr/bin/env python3
"""Run only the assigned native-backend correctness/mutation in a NEW scope."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess


def digest(body):
    return hashlib.sha256(body).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--readonly-deps", type=Path, required=True)
    parser.add_argument("--case-ms", type=int, required=True)
    parser.add_argument("--command-seconds", type=int, required=True)
    args = parser.parse_args()
    if args.case_ms <= 0 or args.command_seconds <= 0:
        parser.error("positive finite case/command bounds required")
    if shutil.disk_usage(Path.home()).free < 32 * 1024**3:
        raise RuntimeError("below32GiB before2GiB admission")
    root = args.out.resolve()
    root.mkdir(mode=0o700)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    files = json.loads(args.input.read_text())
    if set(files) != {"sync.rs", "primitive_case.rs", "allocation.rs"}:
        raise ValueError("unexpected source closure")
    for name, body in files.items():
        with (root / name).open("x") as stream:
            stream.write(body)
    libs = list(args.readonly_deps.glob("liblibc-*.rlib"))
    if len(libs) != 1:
        raise ValueError("one exact readonly libc artifact required")
    rustc = subprocess.check_output(["rustup", "which", "--toolchain", "1.98.1", "rustc"], text=True).strip()
    record = {"root": str(root), "source_hashes": {name: digest(body.encode()) for name, body in files.items()},
              "libc": {"path": str(libs[0]), "sha256": digest(libs[0].read_bytes())}, "commands": [],
              "scope": "standalone native backend only; no API integration, profiling or native errno injection",
              "admission": "1CPU/1GiB/2GiB; no enforced Darwin RSS/data/aggregate disk quota claim"}
    env = {name: value for name, value in os.environ.items() if not name.startswith("DYLD_")}
    env["SHARDS_SYNC_CASE_MS"] = str(args.case_ms)

    def run(name, argv):
        result = subprocess.run(argv, cwd=root, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, timeout=args.command_seconds)
        (root / (name + ".stdout")).write_text(result.stdout)
        (root / (name + ".stderr")).write_text(result.stderr)
        record["commands"].append({"name": name, "argv": argv, "exit": result.returncode,
                                   "stdout": result.stdout, "stderr": result.stderr})
        return result

    def build(name):
        argv = [rustc, "--edition=2024", "--test", "-Copt-level=1", "-Ccodegen-units=1",
                "-Cllvm-args=-threads=1", "--check-cfg", "cfg(test)", "--check-cfg", "cfg(send_guard)",
                "--extern", "libc=" + str(libs[0]), "-L", "dependency=" + str(args.readonly_deps),
                str(root / "primitive_case.rs"), "-o", str(root / name)]
        result = run("compile-" + name, argv)
        if result.returncode != 0:
            raise RuntimeError(name + " did not compile")
        record.setdefault("binaries", {})[name] = digest((root / name).read_bytes())
        return argv

    original = (root / "sync.rs").read_bytes()
    try:
        run("toolchain", [rustc, "-vV"])
        positive = build("baseline")
        result = run("baseline", [str(root / "baseline"), "--test-threads=1", "--nocapture"])
        if result.returncode != 0 or "3 passed; 0 failed" not in result.stdout:
            raise RuntimeError("initial three-case native gate failed")
        needle = "let code = unsafe { libc::pthread_cond_broadcast(self.raw.get()) };"
        body = original.decode()
        if body.count(needle) != 1:
            raise RuntimeError("notification mutation location is ambiguous")
        mutant = body.replace(needle, "let code = 0; // MUTATION: omit native notification only.")
        (root / "mutation-sync.rs").write_text(mutant)
        (root / "sync.rs").write_text(mutant)
        record["mutation"] = {"removed": needle, "replacement": "let code = 0;",
                              "sha256": digest(mutant.encode()), "original_sha256": digest(original)}
        try:
            build("notify-mutant")
            result = run("notify-mutant", [str(root / "notify-mutant"),
                         "cold_pair_wait_notify_and_timeout_under_refusal", "--exact", "--test-threads=1", "--nocapture"])
            record["mutation_detected"] = (result.returncode == 101 and "running 1 test" in result.stdout
                                           and "TimedOut" in result.stderr and "FAILED" in result.stdout)
            if not record["mutation_detected"]:
                raise RuntimeError("notification mutation did not fail for expected timeout")
        finally:
            (root / "sync.rs").write_bytes(original)
            record["source_restored_exact"] = digest((root / "sync.rs").read_bytes()) == digest(original)
        build("restored")
        result = run("restored", [str(root / "restored"), "--test-threads=1", "--nocapture"])
        if result.returncode != 0 or "3 passed; 0 failed" not in result.stdout:
            raise RuntimeError("restored native gate failed")
        negative = positive[:-1] + [str(root / "negative-guard-send"), "--cfg", "send_guard"]
        result = run("negative-guard-send", negative)
        record["negative_expected"] = result.returncode == 1 and "E0277" in result.stderr
        if not record["negative_expected"]:
            raise RuntimeError("Guard was not rejected as non-Send")
        record["status"] = "native-backend-only-baseline-mutation-restored"
    except Exception as error:
        record["status"] = "failed"
        record["driver_error"] = repr(error)
    finally:
        (root / "record.json").write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record))
    if record["status"] == "failed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
