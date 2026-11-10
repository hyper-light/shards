#!/usr/bin/env python3
"""Run only the admission module's private-file/pure tests in a new Linux scope."""

import base64
import gzip
import hashlib
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main(given):
    home = pathlib.Path.home()
    tag = given["tag"]
    assert os.uname().sysname == "Linux" and os.geteuid() != 0
    assert shutil.disk_usage(home).free >= 2 * 1024**3
    cargo, rustup = shutil.which("cargo"), shutil.which("rustup")
    assert cargo and rustup
    target_args = []
    if "target" in given:
        installed = subprocess.check_output(
            [rustup, "target", "list", "--installed", "--toolchain", "1.98.1"], text=True
        ).splitlines()
        assert given["target"] in installed, "requested target not installed; no toolchain mutation/fallback"
        target_args = ["--target", given["target"]]
    source = home / ("managed-resource-admission-source-" + tag)
    target = home / ("managed-resource-admission-target-" + tag)
    records = home / ("managed-resource-admission-light-" + tag)
    assert all(not p.exists() for p in (source, target, records))
    for path in (source, target, records):
        path.mkdir()
    raw = base64.b64decode(given["archive_base64"], validate=True)
    assert hashlib.sha256(raw).hexdigest() == given["archive_sha256"]
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as archive:
        for member in archive:
            relative = pathlib.PurePosixPath(member.name)
            assert member.isfile() and not relative.is_absolute()
            assert ".." not in relative.parts
            data = archive.extractfile(member).read()
            path = source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    (records / "source.json").write_text(json.dumps(given["source"], indent=2) + "\n")
    (records / "scope.json").write_text(json.dumps({
        "runner": given["runner"], "admission": "1 CPU/1 GiB RAM/2 GiB disk; serial; sync:false",
        "source": str(source), "target": str(target), "records": str(records),
        "executed": "actual module's targeted pure/private ordinary-file unit tests only",
        "test_filter": given.get("test_filter", "admission::tests::"),
        "expected_unit_count": given.get("expected_unit_count"),
        "target": given.get("target", "observed rustc host default"),
        "unrun": "actual run_group/Parent admission, kernel controllers/cgroups, full init caller, helper/VM/resource attribution/profile/benchmark/mutation/matrix",
        "harness_allowance": "dead_code only for unused production entrypoints in the isolated import; production panic/unsafe lints remain enabled",
    }, indent=2) + "\n")
    environment = os.environ.copy()
    for key in list(environment):
        if key.startswith("DYLD_"):
            del environment[key]
    environment.update({
        "CARGO_HOME": str(target / "cargo-home"), "CARGO_TARGET_DIR": str(target / "target"),
        "CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0", "RUSTFLAGS": "-C debuginfo=0 -C codegen-units=1",
        "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "TMPDIR": str(target / "tmp"),
    })
    (target / "tmp").mkdir()
    rustc = pathlib.Path(subprocess.check_output([rustup, "which", "--toolchain", "1.98.1", "rustc"], text=True).strip())
    environment["RUSTC"] = str(rustc)
    (records / "environment.json").write_text(json.dumps({k: environment[k] for k in [
        "CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "RUSTFLAGS", "RUSTC", "TMPDIR"
    ]}, indent=2) + "\n")
    (records / "tools.json").write_text(json.dumps({
        "python": {"path": sys.executable, "version": sys.version, "sha256": digest(pathlib.Path(sys.executable))},
        "rustc": {"path": str(rustc), "sha256": digest(rustc)}, "cargo_proxy": cargo,
    }, indent=2) + "\n")
    for label, args in [("rustc", [str(rustc), "-Vv"]), ("cargo", [cargo, "+1.98.1", "-Vv"])]:
        (records / (label + ".txt")).write_bytes(subprocess.check_output(args, env=environment))

    def run(label, argv):
        (records / (label + ".command.json")).write_text(json.dumps({
            "argv": argv, "cwd": str(source), "timeout_seconds": given["command_timeout_seconds"]
        }, indent=2) + "\n")
        try:
            result = subprocess.run(argv, cwd=source, env=environment, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=given["command_timeout_seconds"])
            output, code = result.stdout, result.returncode
        except subprocess.TimeoutExpired as error:
            output, code = error.stdout or b"", "Timeout"
        (records / (label + ".log")).write_bytes(output)
        (records / (label + ".exit")).write_text(str(code) + "\n")
        return code, output

    outcomes = {}
    outcomes["build"], output = run("build", [cargo, "+1.98.1", "test", "--lib", "--no-run", "--message-format=json", *target_args, "-j", "1"])
    binary = None
    if outcomes["build"] == 0:
        artifacts = []
        for line in output.splitlines():
            try:
                item = json.loads(line)
            except (ValueError, UnicodeDecodeError):
                continue
            if item.get("reason") == "compiler-artifact" and item.get("executable") and item.get("profile", {}).get("test"):
                artifacts.append(pathlib.Path(item["executable"]))
        assert len(artifacts) == 1
        binary = artifacts[0]
        identity = {"path": str(binary), "bytes": binary.stat().st_size, "sha256": digest(binary)}
        (records / "binary.json").write_text(json.dumps(identity, indent=2) + "\n")
        outcomes["units"], unit_output = run("units", [str(binary), given.get("test_filter", "admission::tests::"), "--test-threads=1", "--nocapture"])
        if "expected_unit_count" in given and outcomes["units"] == 0:
            marker = ("test result: ok. " + str(given["expected_unit_count"]) +
                      " passed; 0 failed; 0 ignored; 0 measured; 0 filtered out")
            if marker.encode() not in unit_output:
                outcomes["unit_count"] = "Unexpected unit coverage"
                (records / "unit_count.log").write_bytes(unit_output)
        outcomes["clippy"], _ = run("clippy", [cargo, "+1.98.1", "clippy", "--all-targets", *target_args, "-j", "1", "--", "-D", "warnings"])
        assert digest(binary) == identity["sha256"]
    (records / "outcomes.json").write_text(json.dumps(outcomes, indent=2) + "\n")
    for name in ("Cargo.lock",):
        if (source / name).exists():
            shutil.copyfile(source / name, records / name)
    manifest = [{"path": p.name, "bytes": p.stat().st_size, "sha256": digest(p)}
                for p in sorted(records.iterdir()) if p.is_file()]
    (records / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"outcomes": outcomes, "records": str(records), "source": str(source),
                      "binary": None if binary is None else identity, "raw_files": len(manifest) + 1}))
    if any(code != 0 for code in outcomes.values()):
        for label in outcomes:
            if outcomes[label] != 0:
                print(label + ":\n" + (records / (label + ".log")).read_text(errors="replace")[-7000:])


if __name__ == "__main__":
    main(json.loads(gzip.decompress(base64.b64decode(sys.argv[1], validate=True))))
