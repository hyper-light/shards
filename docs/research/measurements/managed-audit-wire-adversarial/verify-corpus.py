#!/usr/bin/env python3
"""Validate original audit corpus evidence without executing/reencoding a codec."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import sys
import tarfile

ARCHIVE = "7b14ede379a4679e4bb3269c40928dda6139358b3fb6342fc4767a4c6404ad46"
MANIFEST = "804f3b66b34565517f88caf43a1b48ad0102eaede0e15348b548df0a9eaa6698"
SOURCE = "6b86176f7354c4b9655b1a70c5671b1f04068f66"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def safe(name):
    p = PurePosixPath(name)
    assert name and not p.is_absolute() and str(p) == name
    assert ".." not in p.parts and "\\" not in name
    return name


def check(original):
    archive = (original / "native-records-v1.tar.gz").read_bytes()
    manifest_bytes = (original / "corpus-manifest.json").read_bytes()
    assert digest(archive) == ARCHIVE
    assert digest(manifest_bytes) == MANIFEST
    corpus = json.loads(manifest_bytes)
    assert corpus["source_revision"] == SOURCE
    with tarfile.open(original / "native-records-v1.tar.gz") as tar:
        members = tar.getmembers()
        names = [safe(m.name) for m in members]
        assert len(names) == len(set(names)) == 390
        assert all(m.isfile() and not m.issym() and not m.islnk() for m in members)
        data = {m.name: tar.extractfile(m).read() for m in members}
    rows = json.loads(data["manifest.json"])
    assert len(rows) == 389
    indexed = set()
    for row in rows:
        name = safe(row["path"])
        assert name not in indexed
        indexed.add(name)
        assert len(data[name]) == row["bytes"] and digest(data[name]) == row["sha256"]
    assert indexed == set(data) - {"manifest.json"}
    assert data["corpus-manifest.json"] == manifest_bytes
    indexed = set()
    for row in corpus["files"]:
        name = safe(row["path"])
        assert name not in indexed
        indexed.add(name)
        raw = data["corpus/" + name]
        assert len(raw) == row["bytes"] and digest(raw) == row["sha256"]
        actual = original / "data" / name
        assert actual.is_file() and not actual.is_symlink() and actual.read_bytes() == raw
    assert len(indexed) == 364
    assert indexed == {n.removeprefix("corpus/") for n in data if n.startswith("corpus/")}
    assert indexed == {p.relative_to(original / "data").as_posix() for p in (original / "data").rglob("*") if p.is_file()}
    inputs = json.loads(data["corpus/inputs.json"])
    assert inputs["fixture_only"] is True and inputs["schema"] == 1 and inputs["usize_bits"] == 64
    assert inputs["members"] == [1, 3]
    assert (inputs["owner_byte"], inputs["run_byte"], inputs["generation"], inputs["policy_byte"]) == (11, 12, 13, 14)
    assert (inputs["journal_header_bytes"], inputs["slot_overhead"]) == (118, 40)
    selected = []
    for member, peer in [(1, 3), (3, 1)]:
        cases = [r for r in inputs["cases"] if r["member"] == member and r["operation"] == "Send"]
        assert len(cases) == 1
        case = cases[0]
        prefix = f"member-{member}-Send"
        assert case["prefix"] == prefix and case["request"] == 1
        intent = data[f"corpus/{prefix}.intent"]
        terminal = data[f"corpus/{prefix}.terminal"]
        i = json.loads(intent)
        b = json.loads(terminal)["receipt"]["broker"]
        assert (i["event"], i["principal"], i["peer"], i["operation"], i["request"]) == ("intent", f"fixture-member-{member}", f"fixture-member-{peer}", "Send", 1)
        assert i["packet_limit"] == case["packet_limit"] and i["object"] is None
        assert (b["principal"], b["peer"], b["operation"], b["request"]) == (i["principal"], i["peer"], "send", 1)
        assert b["outcome"] == "completed" and b["effect"] == {"kind": "kernel_accepted", "bytes": case["packet_limit"]}
        assert b["audit"] == {"kind": "intent_synced"}
        shapes = [n for n in indexed if n.startswith(prefix + ".terminal-shape-")]
        numbers = sorted(int(n.removeprefix(prefix + ".terminal-shape-")) for n in shapes)
        assert numbers == list(range(case["shape_count"]))
        lengths = []
        for number in numbers:
            raw = data[f"corpus/{prefix}.terminal-shape-{number}"]
            shape = json.loads(raw)["receipt"]["broker"]
            assert (shape["principal"], shape["peer"], shape["operation"], shape["request"]) == (i["principal"], i["peer"], "send", 1)
            lengths.append(len(raw))
        imax = len(data[f"corpus/{prefix}.intent-max"])
        tmax = max(lengths)
        assert (imax, tmax, lengths.index(tmax)) == (case["intent_max"], case["terminal_max"], case["terminal_max_shape"])
        assert (len(intent), len(terminal)) == (case["intent_bytes"], case["terminal_bytes"])
        assert 0 < len(intent) <= imax and 0 < len(terminal) <= tmax
        assert len(intent) < imax or len(terminal) < tmax
        assert data[f"corpus/{prefix}.native-test-journal"] == intent + terminal
        assert data[f"corpus/{prefix}.short-journal"] == b""
        # Scope bytes stay opaque here: exact producer-source/manifest identity
        # is checked; the one native fixture uses the real shared ABI decoder.
        scope = data[f"corpus/member-{member}.scope"]
        assert len(scope) == 109
        selected.append({"member": member, "operation": "Send", "broker_request": 1,
                         "scope_sha256": digest(scope), "intent_max": imax, "terminal_max": tmax,
                         "intent_bytes": len(intent), "terminal_bytes": len(terminal),
                         "shape_count": len(shapes), "intent_sha256": digest(intent),
                         "terminal_sha256": digest(terminal)})
    budget = len(selected) * inputs["journal_header_bytes"] + inputs["slot_overhead"] + selected[0]["intent_max"] + selected[0]["terminal_max"]
    return {"archive_sha256": ARCHIVE, "manifest_sha256": MANIFEST, "source_revision": SOURCE,
            "archive_payloads": len(rows), "archive_total_members": len(data),
            "corpus_payloads": len(indexed), "all_unique_safe_regular_hashes_verified": True,
            "selected": selected, "aggregate_B": budget, "audit_lease_call": 0,
            "fuse_unique_correlations": "1..24", "codec_reemitted": False,
            "scope_typed_decode": "existing ABI before effects in the single native case",
            "synthetic_effects_are_not_live_peer_facts": True}


if __name__ == "__main__":
    receipt = check(Path(sys.argv[1]))
    with Path(sys.argv[2]).open("x") as file:
        json.dump(receipt, file, indent=2)
        file.write("\n")
    print(json.dumps(receipt))
