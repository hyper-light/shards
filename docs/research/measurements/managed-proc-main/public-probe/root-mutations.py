"""Coordinator-only public caller mutations, in an exclusively owned Linux checkout."""
from pathlib import Path
import difflib
import hashlib
import json
import os
import shutil
import subprocess
import sys
import traceback

record = Path(sys.argv[1]).resolve()
target = Path(os.environ["CARGO_TARGET_DIR"]).resolve()
record.mkdir(exist_ok=False)
shutil.copyfile(__file__, record / "driver.py")
probe = Path("docs/research/measurements/managed-proc-main/public-probe")
client = Path("crates/api/src/client.rs")
taken = Path("crates/api/src/taken.rs")
original = {path: path.read_bytes() for path in (client, taken)}
results = []
errors = []


def run(name, command):
    (record / (name + ".command.json")).write_text(json.dumps(command) + "\n")
    with (record / (name + ".log")).open("wb") as log:
        rc = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
    (record / (name + ".exit")).write_text(str(rc) + "\n")
    results.append({"case": name, "exit": rc})
    print(name, rc, flush=True)
    return rc


def build(name):
    # Recompile the actual dependency and public consumer after every mutation.
    for path in (*original, probe / "src/main.rs"):
        path.touch()
    assert run(name + "-build", ["cargo", "build", "--manifest-path", str(probe / "Cargo.toml"),
                               "--offline", "--locked", "--release", "-j4",
                               "--message-format=json-render-diagnostics"]) == 0
    rows = []
    for line in (record / (name + "-build.log")).read_text().splitlines():
        if line.startswith("{"):
            row = json.loads(line)
            if row.get("reason") == "compiler-artifact" and row.get("executable"):
                rows.append(row)
    assert len(rows) == 1 and rows[0]["fresh"] is False, rows
    binary = Path(rows[0]["executable"])
    assert target in binary.parents
    frozen = record / (name + ".bin")
    shutil.copyfile(binary, frozen)
    frozen.chmod(0o700)
    (record / (name + ".binary.json")).write_text(json.dumps({
        "compiled_executable": str(binary), "frozen": str(frozen), "fresh": False,
        "sha256": hashlib.sha256(frozen.read_bytes()).hexdigest()}, indent=2) + "\n")
    return frozen


def change(name, path, old, new):
    source = original[path].decode()
    assert source.count(old) == 1, (name, path)
    changed = source.replace(old, new)
    path.write_text(changed)
    (record / (name + ".patch")).write_text("".join(difflib.unified_diff(
        source.splitlines(True), changed.splitlines(True), fromfile=str(path), tofile=str(path))))


def restore():
    for path, data in original.items():
        path.write_bytes(data)
        path.touch()
        assert path.read_bytes() == data


def aborts(name, binary):
    rc = run(name, [str(binary), "oom"])
    log = (record / (name + ".log")).read_text(errors="replace")
    assert rc == -6 and "memory allocation of" in log and "CapacityUnavailable fds_before=" not in log, (rc, log)


(record / "identity.txt").write_text(str(Path.cwd()) + "\n" + subprocess.check_output(
    ["git", "rev-parse", "HEAD"], text=True) + subprocess.check_output(["rustc", "-Vv"], text=True)
    + subprocess.check_output(["uname", "-a"], text=True))
(record / "initial-status.txt").write_bytes(subprocess.check_output(["git", "status", "--short"]))
try:
    fixed = build("fixed")
    for mode in ("oom", "raw-comm"):
        assert run("fixed-" + mode, [str(fixed), mode]) == 0

    change("string-cause", client,
           "crate::taken::Held::find(identity.pid, identity.started).map_err(|failure| Refusal::Process {\n"
           "            pid: identity.pid,\n            expected_started: identity.started,\n"
           "            failure,\n        })?;",
           "crate::taken::Held::find(identity.pid, identity.started).map_err(|failure| Refusal::Gone {\n"
           "            pid: identity.pid, why: failure.to_string(),\n        })?;")
    aborts("string-cause-oom", build("string-cause"))
    restore()

    change("whole-record-utf8", taken,
           "    fn start_field(stat: &[u8]) -> Result<u64, ProcessFailure> {",
           "    fn start_field(stat: &[u8]) -> Result<u64, ProcessFailure> {\n"
           "        let stat = std::str::from_utf8(stat)\n"
           "            .map_err(|_| ProcessFailure::InvalidStartRecord)?.as_bytes();")
    binary = build("whole-record-utf8")
    assert run("whole-record-utf8-raw-comm", [str(binary), "raw-comm"]) == 1
    log = (record / "whole-record-utf8-raw-comm.log").read_text()
    assert "InvalidStartRecord" in log and "unexpected public refusal" in log, log
    restore()

    change("infallible-reserve", taken,
           "            bytes\n                .try_reserve(1)\n"
           "                .map_err(|_| ProcessFailure::CapacityUnavailable)?;",
           "            bytes.reserve(1);")
    aborts("infallible-reserve-oom", build("infallible-reserve"))
except BaseException:
    errors.append(traceback.format_exc())
    (record / "campaign-error.txt").write_text(errors[-1])
    print(errors[-1], flush=True)
finally:
    restore()
    (record / "restored-source.sha256.json").write_text(json.dumps({
        str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in original}, indent=2) + "\n")
    (record / "restored.diff").write_bytes(subprocess.check_output(
        ["git", "diff", "--", *(str(path) for path in original)]))

try:
    restored = build("restored")
    for mode in ("oom", "raw-comm"):
        assert run("restored-" + mode, [str(restored), mode]) == 0
    for name, args in (("ordinary-lib", ["--lib"]), ("ordinary-sim", ["--test", "sim"])):
        assert run(name, ["cargo", "test", "--offline", "--locked", "-p", "shards-api", "-j4",
                          *args, "--", "--test-threads=1"]) == 0
except BaseException:
    errors.append(traceback.format_exc())
    (record / "closing-error.txt").write_text(errors[-1])
    print(errors[-1], flush=True)

(record / "results.json").write_text(json.dumps(results, indent=2) + "\n")
(record / "driver.exit").write_text("1\n" if errors else "0\n")
raise SystemExit(1 if errors else 0)
