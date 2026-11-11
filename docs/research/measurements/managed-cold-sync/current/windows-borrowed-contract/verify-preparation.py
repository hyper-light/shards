#!/usr/bin/env python3
"""Verify only the proposed source boundary; never compile or launch the fixture."""
import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path

BASE = "ca3ab10c151aa5539e757f1cc76029746ccbb561"
CLIENT = "crates/api/src/client.rs"
FIXTURE = "crates/api/src/client/windows_borrowed.rs"
HOOK = (
    "    #[cfg(all(test, windows))]\n"
    "    windows_borrowed_contract::before_identity(began, wait);\n"
)
MODULE = (
    '#[cfg(all(test, windows))]\n#[path = "client/windows_borrowed.rs"]\n'
    "mod windows_borrowed_contract;\n\n"
)


def git(*arguments):
    return subprocess.check_output(["git", *arguments])


def digest(body):
    return {"bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()}


def record(repo):
    before = git("show", f"{BASE}:{CLIENT}")
    after = (repo / CLIENT).read_bytes()
    text = after.decode()
    assert text.count(HOOK) == 1 and text.count(MODULE) == 1
    stripped = text.replace(HOOK, "", 1).replace(MODULE, "", 1).encode()
    assert stripped == before, "client changed outside the two exact cfg(test,windows) blocks"
    assert git("diff", "33df95b08ad8f95bddeaa1c48acf0c64386e0eac", BASE, "--", CLIENT) == b""
    unchanged = {}
    for name in ["crates/api/src/stream.rs", "crates/api/src/lib.rs", "crates/afunix/src/lib.rs"]:
        body = git("show", f"{BASE}:{name}")
        # Sparse-absent leaves are read from Git; no checkout or guessed substitute.
        path = repo / name
        if path.exists():
            assert path.read_bytes() == body, name
        unchanged[name] = digest(body)
    fixture = (repo / FIXTURE).read_bytes()
    selectors = re.findall(r"^fn (\w+)\(\) \{", fixture.decode(), re.M)
    assert len(selectors) == 9 and "held_child" in selectors, selectors
    assert fixture.decode().count("#[test]") == 9
    assert fixture.decode().count("#[ignore = ") == 9
    old_wrapper = git(
        "show",
        "ec494ad24905e2f39fe05f77a9513a8fd41df5e9:"
        "docs/research/measurements/managed-cold-sync/current/windows/run.ps1",
    )
    return {
        "base_revision": BASE,
        "policy_rules_sha256": hashlib.sha256(git("show", f"{BASE}:docs/development-rules.md")).hexdigest(),
        "role": "implementer; source preparation only",
        "current_main_client_identical_to_33df": True,
        "client_before": digest(before),
        "client_after": digest(after),
        "exact_test_hook_and_module_lines": 6,
        "client_after_removing_only_these_blocks_equals_base": True,
        "non_test_behavior": "source boundary checked, not compiled or executed",
        "unchanged_existing_seams": unchanged,
        "fixture": {"path": FIXTURE, **digest(fixture)},
        "source_derived_ignored_selectors": [f"client::windows_borrowed_contract::{s}" for s in selectors],
        "actual_libtest_selection_ignore_pass_fail_counts": "UNRUN",
        "root_execution_wrapper_reference": {
            "revision": "ec494ad24905e2f39fe05f77a9513a8fd41df5e9",
            "path": "docs/research/measurements/managed-cold-sync/current/windows/run.ps1",
            **digest(old_wrapper),
            "reuse_limit": "native Process/raw-capture/original Stopwatch ownership; ROOT reviews held-child observer before execution",
        },
        "prior_proof_ref_preserved": git("rev-parse", "p/managed-cold-sync-owned-v1").decode().strip(),
        "native_compile_lint_actor_execution": "UNRUN; ROOT gate not yet assigned",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[6]
    result = record(repo)
    text = json.dumps(result, indent=2) + "\n"
    if args.out:
        with args.out.open("x") as output:
            output.write(text)
    else:
        print(text, end="")
