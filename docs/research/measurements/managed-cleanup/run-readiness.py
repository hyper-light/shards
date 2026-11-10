"""Focused Unix host-fixture proof; never starts a VM or touches guest cleanup."""
import argparse
import difflib
import hashlib
import json
import os
import pathlib
import platform
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--patience-ms", type=int, required=True)
parser.add_argument("--source-ref", required=True)
parser.add_argument("--evidence", type=pathlib.Path, required=True)
args = parser.parse_args()
if os.name != "posix" or args.patience_ms <= 0 or not os.environ.get("CARGO_TARGET_DIR"):
    raise SystemExit("requires Unix, positive caller patience and an owned CARGO_TARGET_DIR")
args.evidence.mkdir(parents=True, exist_ok=False)
root = pathlib.Path(__file__).resolve().parents[4]
source = root / "crates/shards/tests/managed_cleanup.rs"
guest = root / "crates/testguest/src/cleanup.rs"
original = source.read_bytes()
identity = {"source_ref": args.source_ref, "source_sha256": hashlib.sha256(original).hexdigest(),
            "guest_sha256": hashlib.sha256(guest.read_bytes()).hexdigest(),
            "cwd": str(root), "platform": platform.platform(), "host": platform.node(),
            "rustc": subprocess.check_output(["rustc", "-Vv"], cwd=root, text=True),
            "target_dir": os.environ["CARGO_TARGET_DIR"], "patience_ms": args.patience_ms}
(args.evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
env = dict(os.environ, SHARDS_HELPERS="skip", CLEANUP_CLIENT_PATIENCE_MS=str(args.patience_ms))
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
