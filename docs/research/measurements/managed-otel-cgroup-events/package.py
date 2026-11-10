#!/usr/bin/env python3
"""Freeze only the assigned standalone sources; never build locally."""

import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tarfile


def main():
    root = pathlib.Path("docs/research/measurements/managed-otel-cgroup-events")
    records = root / "records" / (sys.argv[1] if len(sys.argv) > 1 else "light-v1")
    records.mkdir(parents=True, exist_ok=True)
    paths = ["crates/init/src/metrics.rs", "crates/init/src/metrics/events.rs",
             "crates/init/src/metrics/otlp.rs", "crates/init/src/metrics/tests.rs"]
    for area, names in [
        ("managed-otel-cgroup-events", ["Cargo.toml", "Cargo.lock", "lib.rs", "testdir/Cargo.toml", "run-light.py"]),
        ("managed-otel-encoder", ["Cargo.toml", "Cargo.lock", "fixture.rs", "check.py"]),
        ("managed-otel-rust-reference", ["Cargo.toml", "Cargo.lock", "src/main.rs", "src/reference.rs", "tests/equivalence.rs", "check.py"]),
        ("managed-otel/profile", ["Cargo.toml", "Cargo.lock", "src/main.rs", "src/allocations.rs"]),
    ]:
        paths.extend("docs/research/measurements/" + area + "/" + name for name in names)
    files, identities = {}, {}

    def add(name, data, association):
        files[name] = data
        identities[name] = {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(), "source": association}

    for name in paths:
        path = pathlib.Path(name)
        if path.is_file():
            add(name, path.read_bytes(), "assigned worktree source")
        else:
            # Current profile lock was not in this frozen branch. Read its exact
            # current-main Git object; do not create/overwrite a worktree lock.
            revision = "origin/main" if name.endswith("profile/Cargo.lock") else "HEAD"
            add(name, subprocess.check_output(["git", "show", revision + ":" + name]), revision + " Git blob")
    for name in ["Cargo.lock", "crates/testdir/src/lib.rs", "rustfmt.toml", "clippy.toml",
                 "AGENTS.md", "docs/development-rules.md", "docs/development-evidence.md",
                 "CLAUDE.md", "docs/design/managed-runtime-plan.md"]:
        add(name, subprocess.check_output(["git", "show", "origin/main:" + name]), "origin/main0342 Git blob")
    manifest = {
        "base_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "policy_main": subprocess.check_output(["git", "rev-parse", "origin/main"], text=True).strip(),
        "source_files": identities, "native_invocations": 0, "profile_invocations": 0,
        "scope": "GNU/musl pure Reader+codec and official-reference equivalence; profile CHECK only",
        "new_private_root": sys.argv[2] if len(sys.argv) > 2 else "/home/adalundhe/managed-otel-cgroup-events-ed9-light-v1",
        "envelope": {"cpus": 2, "memory_bytes": 2 * 1024**3, "disk_bytes": 4 * 1024**3, "timeout_s": 600, "cargo_jobs": 2},
    }
    files["source-manifest.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as packed:
        for name, data in sorted(files.items()):
            member = tarfile.TarInfo(name)
            member.size, member.mode, member.mtime = len(data), 0o600, 0
            packed.addfile(member, io.BytesIO(data))
    archive = records / "source.tar.gz"
    data = buffer.getvalue()
    with archive.open("xb") as stream:
        stream.write(data)
    summary = {"archive": {"path": str(archive), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()},
               "original_source_payloads": len(identities), "embedded_manifests": 1, "manifest": manifest}
    with (records / "source-package.json").open("x") as stream:
        stream.write(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "manifest"}, indent=2))


if __name__ == "__main__":
    main()
