#!/usr/bin/env python3
"""Check the real init target from a private source subset; never execute init."""

import base64
import hashlib
import io
import json
import os
import pathlib
import shutil
import subprocess
import tarfile


def main():
    count = int(os.environ["ADMISSION_INPUT_PARTS"])
    packed = b"".join(base64.b64decode(os.environ["ADMISSION_INPUT_PART_" + str(i)], validate=True)
                      for i in range(count))
    identity = json.loads(os.environ["ADMISSION_INPUT_IDENTITY"])
    assert hashlib.sha256(packed).hexdigest() == identity["archive"]["sha256"]
    assert len(packed) == identity["archive"]["bytes"]
    home = pathlib.Path.home()
    assert os.uname().sysname == "Linux" and os.geteuid() != 0
    assert shutil.disk_usage(home).free >= 2 * 1024**3
    tag = identity["tag"]
    source, target, records = [home / ("managed-resource-admission-" + name + "-" + tag)
                               for name in ("source", "target", "light")]
    assert all(not p.exists() for p in (source, target, records))
    for p in (source, target, records):
        p.mkdir()
    (records / "input.json").write_text(json.dumps(identity, indent=2) + "\n")
    with tarfile.open(fileobj=io.BytesIO(packed), mode="r:gz") as archive:
        for member in archive:
            relative = pathlib.PurePosixPath(member.name)
            assert member.isfile() and not relative.is_absolute() and ".." not in relative.parts
            path = source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(archive.extractfile(member).read())
    for e in identity["file_hashes"]:
        data = (source / e["path"]).read_bytes()
        assert len(data) == e["bytes"] and hashlib.sha256(data).hexdigest() == e["sha256"]
    environment = {k: v for k, v in os.environ.items()
                   if not k.startswith("DYLD_") and not k.startswith("ADMISSION_INPUT_")}
    environment.update({
        "CARGO_HOME": str(target / "cargo-home"), "CARGO_TARGET_DIR": str(target / "target"),
        "CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0", "RUSTFLAGS": "-C debuginfo=0 -C codegen-units=1",
        "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "TMPDIR": str(target / "tmp"),
    })
    (target / "tmp").mkdir()
    cargo, rustup = shutil.which("cargo"), shutil.which("rustup")
    assert cargo and rustup
    rustc = pathlib.Path(subprocess.check_output([rustup, "which", "--toolchain", "1.98.1", "rustc"], text=True).strip())
    environment["RUSTC"] = str(rustc)
    for name, args in [("rustc", [str(rustc), "-Vv"]), ("cargo", [cargo, "+1.98.1", "-Vv"])]:
        (records / (name + ".txt")).write_bytes(subprocess.check_output(args, env=environment))
    (records / "scope.json").write_text(json.dumps({
        "admission": "1CPU/1GiB RAM/2GiB disk; serial sync:false; new private paths",
        "source": str(source), "target": str(target), "records": str(records),
        "compiled": "real shards-init binary target, cargo check only",
        "unrun": "link/installed binary, helper build, init execution, kernel/controller/VM/profile/guarantee or comparative gate",
        "source_adaptation": identity["private_manifest_adaptation"],
        "environment": {k: environment[k] for k in ["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "RUSTFLAGS", "RUSTC", "TMPDIR"]},
        "rustc_sha256": hashlib.sha256(rustc.read_bytes()).hexdigest(),
    }, indent=2) + "\n")
    command = [cargo, "+1.98.1", "check", "-p", "shards-init", "--bin", "shards-init", "-j", "1"]
    (records / "check.command.json").write_text(json.dumps({"argv": command, "cwd": str(source), "timeout_seconds": 240}, indent=2) + "\n")
    try:
        result = subprocess.run(command, cwd=source, env=environment, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=240)
        code, output = result.returncode, result.stdout
    except subprocess.TimeoutExpired as error:
        code, output = "Timeout", error.stdout or b""
    (records / "check.exit").write_text(str(code) + "\n")
    (records / "check.log").write_bytes(output)
    if (source / "Cargo.lock").exists():
        shutil.copyfile(source / "Cargo.lock", records / "Cargo.lock")
    for e in identity["file_hashes"]:
        assert hashlib.sha256((source / e["path"]).read_bytes()).hexdigest() == e["sha256"] or e["path"] == "Cargo.lock"
    manifest = [{"path": p.name, "bytes": p.stat().st_size, "sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
                for p in sorted(records.iterdir()) if p.is_file()]
    (records / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"check_exit": code, "records": str(records), "binary": None,
                      "target": "real shards-init binary check only", "raw_files": len(manifest) + 1}))
    print(output.decode(errors="replace")[-6000:])


if __name__ == "__main__":
    main()
