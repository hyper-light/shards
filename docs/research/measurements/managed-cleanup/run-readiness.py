"""Focused Unix host-fixture proof; never starts a VM or touches guest cleanup."""
import argparse
import difflib
import hashlib
import json
import os
import pathlib
import platform
import selectors
import subprocess
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--patience-ms", type=int, required=True)
parser.add_argument("--source-ref", required=True)
parser.add_argument("--evidence", type=pathlib.Path, required=True)
parser.add_argument("--helper-only", action="store_true", help="light shared-helper protocol controls only; no Client build/mutation/VM")
args = parser.parse_args()
if os.name != "posix" or args.patience_ms <= 0 or not os.environ.get("CARGO_TARGET_DIR"):
    raise SystemExit("requires Unix, positive caller patience and an owned CARGO_TARGET_DIR")
args.evidence.mkdir(parents=True, exist_ok=False)
args.evidence = args.evidence.resolve()
root = pathlib.Path(__file__).resolve().parents[4]
source = root / "crates/shards/tests/managed_cleanup.rs"
guest = root / "crates/testguest/src/cleanup.rs"
original = source.read_bytes()
child_source = pathlib.Path(__file__).with_name("readiness-child.rs")
child_artifact = args.evidence / "readiness-child"
identity = {"source_ref": args.source_ref, "source_sha256": hashlib.sha256(original).hexdigest(),
            "guest_sha256": hashlib.sha256(guest.read_bytes()).hexdigest(),
            "child_sha256": hashlib.sha256(child_source.read_bytes()).hexdigest(),
            "cwd": str(root), "platform": platform.platform(), "host": platform.node(),
            "rustc": subprocess.check_output(["rustc", "+1.98.1", "-Vv"], cwd=root, text=True),
            "target_dir": os.environ["CARGO_TARGET_DIR"], "patience_ms": args.patience_ms,
            "helper_only": args.helper_only, "child_artifact": str(child_artifact)}
(args.evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
env = dict(os.environ, SHARDS_HELPERS="skip", CLEANUP_CLIENT_PATIENCE_MS=str(args.patience_ms),
           CLEANUP_CLIENT_CHILD=str(child_artifact), RUSTUP_TOOLCHAIN="1.98.1")
env = {key: value for key, value in env.items() if not key.startswith("DYLD_")}
base = ["cargo", "test", "--locked", "-j4", "--profile", "test-release", "-p", "shards", "--test", "managed_cleanup"]
all_native = base + ["native::", "--", "--ignored", "--test-threads=1", "--nocapture"]
mutation = base + ["native::dropping_ready_client_reaps_without_releasing_the_child", "--", "--ignored", "--exact", "--test-threads=1", "--nocapture"]


def run(name, command, expected):
    (args.evidence / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
    with (args.evidence / (name + ".log")).open("w") as output:
        code = subprocess.call(command, cwd=root, env=env, stdout=output, stderr=subprocess.STDOUT)
    (args.evidence / (name + ".exit")).write_text(str(code) + "\n")
    print(name, "exit", code, flush=True)
    if code != expected:
        raise RuntimeError(f"{name}: expected {expected}, got {code}")


if sys.platform == "darwin":
    space = os.statvfs(args.evidence)
    if space.f_bavail * space.f_frsize < 30 * 1024**3:
        raise SystemExit("held: Mac below required30GiB floor")
run("child-compile", ["rustc", "+1.98.1", "--edition=2024", "-Dwarnings", "-Ccodegen-units=4", str(child_source), "-o", str(child_artifact)], 0)
identity["child_artifact_sha256"] = hashlib.sha256(child_artifact.read_bytes()).hexdigest()
(args.evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")


def protocol(mode, release, expected_exit, prefix, expected_output):
    command = [str(child_artifact), mode, "stdin"]
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env) as child:
        try:
            before = b""
            if expected_exit != 7:
                deadline = time.monotonic() + args.patience_ms / 1000
                os.set_blocking(child.stdout.fileno(), False)
                with selectors.DefaultSelector() as ready:
                    ready.register(child.stdout, selectors.EVENT_READ)
                    while len(before) < len(prefix):
                        if not ready.select(max(0, deadline - time.monotonic())):
                            raise RuntimeError(f"{mode}: missing readiness prefix")
                        got = os.read(child.stdout.fileno(), len(prefix) - len(before))
                        if not got:
                            raise RuntimeError(f"{mode}: early stdout EOF")
                        before += got
                    if before != prefix:
                        raise RuntimeError(f"{mode}: different prefix {before!r}")
                    # One bounded event wait proves withheld stdin has not released it.
                    if ready.select(args.patience_ms / 1000):
                        raise RuntimeError(f"{mode}: output/EOF before stdin release")
                os.set_blocking(child.stdout.fileno(), True)
            rest, error = child.communicate(input=release, timeout=args.patience_ms / 1000)
            record = dict(command=command, pid=child.pid, exit=child.returncode,
                          stdout_hex=(before + rest).hex(), stderr_hex=error.hex(), release_hex=release.hex())
            (args.evidence / f"child-{mode}-{release.hex() or 'eof'}.json").write_text(json.dumps(record, indent=2) + "\n")
            if child.returncode != expected_exit or before + rest != expected_output:
                raise RuntimeError(f"{mode}: different exit or exact protocol bytes")
            if error != f"CLEANUP_CHILD_PID {child.pid}\n".encode():
                raise RuntimeError(f"{mode}: missing exact owned-child identity")
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()


for case in [("early", b"r", 7, b"", b""),
             ("silent", b"r", 0, b"", b"released\n"),
             ("partial", b"r", 0, b"rea", b"reareleased\n"),
             ("normal", b"r", 0, b"ready\n", b"ready\nreleased\n"),
             ("normal", b"x", 8, b"ready\n", b"ready\n"),
             ("normal", b"", 8, b"ready\n", b"ready\n")]:
    protocol(*case)
(args.evidence / "child-protocol.exit").write_text("0\n")
print("shared native helper protocol6/6; exact bytes/exits and owned child wait", flush=True)
if args.helper_only:
    raise SystemExit(0)


# A kiln sync may restore an older source mtime over a newer compiler artifact.
# Force a real package rebuild before proof and after restoring a source mutation.
os.utime(source, None)
run("green", all_native, 0)
if "5 passed; 0 failed" not in (args.evidence / "green.log").read_text():
    raise RuntimeError("the five requested native cases did not all run")
needle = '''            if let Err(error) = self.child.wait() {
                let _ = writeln!(io::stderr(), "cleanup client reap failed: {error}");
            }
'''
text = original.decode()
if text.count(needle) != 1:
    raise RuntimeError("expected one exact owned-child reap guard")
changed = text.replace(needle, "")
(args.evidence / "no-drop-wait.patch").write_text("".join(difflib.unified_diff(text.splitlines(True), changed.splitlines(True), fromfile=str(source), tofile=str(source))))
try:
    source.write_text(changed)
    os.utime(source, None)
    run("mutation", mutation, 101)
    if "controlled child needed explicit rescue" not in (args.evidence / "mutation.log").read_text():
        raise RuntimeError("mutation failed for an unrelated reason")
finally:
    source.write_bytes(original)
    os.utime(source, None)
    restored = hashlib.sha256(source.read_bytes()).hexdigest()
    (args.evidence / "restored.sha256").write_text(restored + "\n")
    if restored != identity["source_sha256"]:
        raise RuntimeError("source restoration differs")
run("restored", all_native, 0)
run("clippy", ["cargo", "clippy", "--locked", "-j4", "-p", "shards", "--test", "managed_cleanup", "--", "-D", "warnings"], 0)
run("windows-typecheck", ["python3", str(pathlib.Path(__file__).with_name("windows-check.py")), "--target", "x86_64-pc-windows-msvc", "--evidence", str(args.evidence / "windows")], 0)
