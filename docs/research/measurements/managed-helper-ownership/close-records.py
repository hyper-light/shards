#!/usr/bin/env python3
"""Read the actual mutation effect, restore fresh processes, preserve raw records."""
from pathlib import Path
import base64
import hashlib
import io
import json
import os
import shutil
import signal
import subprocess
import tarfile

p = Path.home() / "managed-helper-ownership-cbd-before-v1"
os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
owned = []
for process in Path("/proc").iterdir():
    if not process.name.isdigit():
        continue
    try:
        executable = os.readlink(process / "exe")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
    if executable.startswith(str(p) + "/"):
        owned.append({"pid": int(process.name), "executable": executable})
observation = {"owned_binary_processes": owned, "kernel": list(os.uname()),
               "limit": "One executable-path snapshot after runner reavailability; no escaped-descendant/kernel charge proof."}
(p / "post-restart-processes.json").write_text(json.dumps(observation, indent=2) + "\n")
mutation = json.loads((p / "mutation-death-v1/death.json").read_text())
summary = {"mutation_effect": mutation, "restore": {}, "post_restart": observation}
expected = (mutation.get("effects", {}).get("after_helper_exit", {}).get("kind") == "D"
            and not mutation.get("passed", True) and not owned)
if expected:
    dest = p / "restore-v1"
    dest.mkdir()
    shutil.copyfile(p / "after-musl-v1/observe.py", dest / "observe.py")
    binary = p / "after-musl-v1/target/x86_64-unknown-linux-musl/release/managed-helper-ownership"
    source = p / "after-musl-v1/source/src/sandbox.rs"
    assert hashlib.sha256(source.read_bytes()).hexdigest() == "3d4acf45f97e023f0d789f4dd8492f2c140ffd6c11e81f8045e32ff45b19ba2e"
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == "13438cc87ef8d5e5b2ab4f1b383e6464a818d1ec822432e25425fcfe28e237b5"
    for mode in ["death", "control-error"]:
        command = ["python3", str(dest / "observe.py"), str(binary), str(dest), mode, "3"]
        (dest / (mode + ".command.json")).write_text(json.dumps(command) + "\n")
        timed_out = False
        with (dest / (mode + ".log")).open("wb") as log:
            child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                     start_new_session=True)
            try:
                code = child.wait(timeout=40)
            except subprocess.TimeoutExpired:
                timed_out = True
                # Own unreaped group leader pins this finite fixture group identity.
                os.killpg(child.pid, signal.SIGKILL)
                code = child.wait(timeout=10)
        facts = dest / (mode + ".json")
        summary["restore"][mode] = {
            "exit": code, "timeout": timed_out,
            "facts": json.loads(facts.read_text()) if facts.exists() else {"unknown": "no fixture record"}}
else:
    summary["restore"] = {"unrun": "Mutation lacked DONE or owned binary population remained; dependent restore held"}
(p / "closing-results.json").write_text(json.dumps(summary, indent=2) + "\n")

associations = {}
for name in ["before-v3", "before-musl-v4", "after-musl-v1", "mutation-death-v1"]:
    scope = p / name
    found = []
    for line in (scope / "build.log").read_text().splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get("reason") == "compiler-artifact" and item.get("executable"):
            binary = Path(item["executable"])
            assert item["manifest_path"] == str(scope / "source/Cargo.toml")
            found.append({"path": str(binary), "manifest": item["manifest_path"],
                          "fresh": item["fresh"], "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                          "bytes": binary.stat().st_size})
    assert found and all(not item["fresh"] for item in found), found
    associations[name] = found
(p / "artifact-associations.json").write_text(json.dumps(associations, indent=2) + "\n")

selected = [item for item in sorted(p.iterdir()) if item.is_file()
            and item.suffix in [".log", ".json", ".txt", ".py"]]
for name in ["before-v3", "diagnostic-v1", "before-musl-v4", "after-musl-v1",
             "mutation-death-v1", "restore-v1"]:
    scope = p / name
    if not scope.exists():
        continue
    selected += [item for item in sorted(scope.iterdir()) if item.is_file()
                 and item.suffix in [".log", ".json", ".txt", ".py"]]
    for relative in ["source/source-hashes.json", "source/Cargo.toml", "source/Cargo.lock",
                     "source/src/sandbox.rs", "source/src/main.rs", "source/src/server.rs",
                     "source/src/lib.rs", "source/clippy.toml"]:
        item = scope / relative
        if item.is_file():
            selected.append(item)
members = {str(item.relative_to(p)): {"bytes": item.stat().st_size,
           "sha256": hashlib.sha256(item.read_bytes()).hexdigest()} for item in selected}
archive = p / "closing-records-v1.tar.gz"
with tarfile.open(archive, "w:gz") as tar:
    for item in selected:
        tar.add(item, arcname=str(item.relative_to(p)), recursive=False)
    data = json.dumps(members, indent=2).encode() + b"\n"
    info = tarfile.TarInfo("record-members.json")
    info.size = len(data)
    tar.addfile(info, io.BytesIO(data))
data = archive.read_bytes()
print(json.dumps({"archive": str(archive), "bytes": len(data),
                  "sha256": hashlib.sha256(data).hexdigest(), "members": len(members) + 1,
                  "summary": summary, "associations": associations,
                  "base64": base64.b64encode(data).decode()}))
