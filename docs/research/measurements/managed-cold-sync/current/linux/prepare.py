#!/usr/bin/env python3
"""Export the frozen API corpus with narrowly checked portability deltas."""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import posixpath
import re
import subprocess
import sys

REF = "0aad84439f2b6231a0028713f40bf74d4bf7646c"
POLICY = "b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe"


def record(body, edited=False):
    return {"text": body.decode(), "sha256": hashlib.sha256(body).hexdigest(),
            "bytes": len(body), "private_test_edits": edited}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--candidate", action="store_true")
    parser.add_argument("--test-portability", action="store_true")
    parser.add_argument("--darwin-ordinary", action="store_true")
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("frozen_prepare", here.parent / "prepare.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    version = helper.version("frozen-0aad-ordinary", REF, True, b"")
    suffix = ('\n#[cfg(all(test, target_vendor = "apple"))]\n'
              '#[path = "../../../docs/research/measurements/managed-cold-sync/current/native_case.rs"]\n'
              'mod cold_sync_current;\n')
    client = version["files"]["crates/api/src/client.rs"]["text"]
    if not client.endswith(suffix):
        raise ValueError("private registration anchor changed")
    original = client[:-len(suffix)].encode()
    if original != helper.git(REF, "crates/api/src/client.rs"):
        raise ValueError("frozen client bytes changed")
    version["files"]["crates/api/src/client.rs"] = record(original)
    del version["files"]["docs/research/measurements/managed-cold-sync/current/native_case.rs"]
    version["has_allocation_refusal"] = False
    # taken.rs includes this existing leaf through concat!(CARGO_MANIFEST_DIR),
    # rather than a #[path] attribute. The first Linux export missed it; retain
    # that actual missing-source compile failure separately from product lint.
    for leaf in ["repro.rs", "reader-tests.rs"]:
        name = "docs/research/measurements/managed-proc-main/" + leaf
        version["files"][name] = record(helper.git(REF, name))
    # Existing ordinary Linux tests import their existing test-only allocator;
    # ignored coordinator fault cases stay ignored. No hook or test is invented.
    pending = [name for name in version["files"] if name.endswith(".rs")]
    seen = set()
    while pending:
        name = pending.pop()
        if name in seen:
            continue
        seen.add(name)
        text = version["files"][name]["text"]
        for path in re.findall(r'#\[path\s*=\s*"([^"]+)"\]', text):
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(name), path))
            if resolved not in version["files"]:
                version["files"][resolved] = record(helper.git(REF, resolved))
                pending.append(resolved)
    version["files"][".cargo/config.toml"] = record(helper.git(REF, ".cargo/config.toml"))
    for name in ["AGENTS.md", "CLAUDE.md", "docs/development-rules.md", "docs/development-evidence.md"]:
        version["files"][name] = record(helper.git(POLICY, name))
    # Assert all actual crate sources, manifests and imported test leaves remain
    # exact Git bytes; only the closure's root workspace/lock membership differs.
    for name, value in version["files"].items():
        if name.startswith("crates/") or name.endswith(".rs"):
            if value["text"].encode() != helper.git(REF, name):
                raise ValueError("non-exact frozen source: " + name)
    # The sole coordinator-approved product delta after the actual GNU dead-code
    # error; keep every other frozen crate/source byte exact.
    if args.candidate:
        name = "crates/api/src/sync.rs"
        original = helper.git(REF, name)
        repository = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
        candidate = (repository / name).read_bytes()
        expected = original.replace(b"\nimpl Failure {", b'\n#[cfg(target_vendor = "apple")]\nimpl Failure {', 1)
        if candidate != expected:
            raise ValueError("candidate differs beyond the approved native-helper cfg")
        version["files"][name] = record(candidate, True)
        version["product_delta"] = "one Apple cfg on existing native-only Failure constructor impl"
    if args.test_portability:
        name = "crates/api/src/client.rs"
        original = helper.git(REF, name)
        repository = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
        candidate = (repository / name).read_bytes()
        anchor = (b"                code: libc::ESRCH,\n"
                  b"            }\n"
                  b"        }) if pid == identity.pid && expected_started == identity.started),")
        replacement = (b"                code: libc::ESRCH,\n"
                       b"            } | crate::ProcessFailure::Os {\n"
                       b"                operation: crate::ProcessOperation::OpenStat,\n"
                       b"                code: libc::ENOENT,\n"
                       b"            }\n"
                       b"        }) if pid == identity.pid && expected_started == identity.started),")
        expected = original.replace(anchor, replacement, 1)
        if expected == original or candidate != expected:
            raise ValueError("client differs beyond the demonstrated post-reap test assertion")
        version["files"][name] = record(candidate, True)
        version["test_delta"] = "only first post-reap attach assertion accepts exact OpenStat/ENOENT with PID/start guards retained"
    if args.darwin_ordinary:
        name = "crates/api/src/client.rs"
        body = version["files"][name]["text"].encode()
        body += (b'\n#[cfg(all(test, target_vendor = "apple"))]\n'
                 b'#[path = "../../../docs/research/measurements/managed-cold-sync/current/concurrent_case.rs"]\n'
                 b'mod concurrent_sync;\n')
        version["files"][name] = record(body, True)
        name = "docs/research/measurements/managed-cold-sync/current/concurrent_case.rs"
        version["files"][name] = record(helper.git(REF, name))
        version["label"] = "darwin-ordinary-portability"
    package = {"versions": [version]} if args.darwin_ordinary else {"version": version}
    packed = gzip.compress(json.dumps(package, separators=(",", ":")).encode())
    (args.out / "input.b64").write_text(base64.b64encode(packed).decode())
    driver = (here / ("darwin_run.py" if args.darwin_ordinary else "run.py")).read_bytes()
    (args.out / "driver.b64").write_text(base64.b64encode(driver).decode())
    identities = {"revision": REF, "production_revision": "cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0",
                  "policy_revision": POLICY,
                  "candidate": args.candidate,
                  "test_portability": args.test_portability,
                  "darwin_ordinary": args.darwin_ordinary,
                  "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
                  "packed_sha256": hashlib.sha256(packed).hexdigest(),
                  "driver_sha256": hashlib.sha256(driver).hexdigest(),
                  "files": {name: {k: v for k, v in value.items() if k != "text"}
                            for name, value in version["files"].items()}}
    if args.darwin_ordinary:
        for name in ["binding_run", "concurrent_run"]:
            body = (here.parent / (name + ".py")).read_bytes()
            (args.out / (name + ".b64")).write_text(base64.b64encode(body).decode())
            identities[name + "_sha256"] = hashlib.sha256(body).hexdigest()
    (args.out / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
    print(json.dumps({k: v for k, v in identities.items() if k != "files"}))


if __name__ == "__main__":
    main()
