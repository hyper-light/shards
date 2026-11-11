#!/usr/bin/env python3
"""Emit an unapplied Reserved-disconnect proposal; never changes native.rs."""
import difflib
import hashlib
import json
from pathlib import Path
import subprocess


here = Path(__file__).parent
leaf = here.parent / "native.rs"
original = leaf.read_text()
assert hashlib.sha256(original.encode()).hexdigest() == "228bb9e67e1e699d969a71de817c5a9c6e461ee18e07d98aa070029b07b59e4e"
proposed = original


def once(before, after):
    global proposed
    assert proposed.count(before) == 1, before
    proposed = proposed.replace(before, after)


once("    ReservedClose,\n}", "    ReservedClose,\n    ReservedDisconnect,\n}")
arm = """        Case::ReservedDisconnect => {
            require(wire.sent == 8, "disconnect must follow exactly the first8 complete requests")?;
            wire.trace.event(format!(
                "disconnect_last_wire_reply={reserved:?}; later individual lease stage/cursors remain unobserved"
            ))?;
            return Ok(());
        }
"""
boundary = """    }
    require(
        wire.control(node, fh, scope, reserve)? == reserved,"""
once(boundary, arm + boundary)

observation = """                let result = match case {
                    Case::ReservedDisconnect if result.is_ok() && cleanup.is_ok() => result.and((|| {
                        // The answerer is already joined; readiness releases its own
                        // state lock before trace/file I/O. It reports accounting,
                        // never a synthesized individual Closed reply or live cursor.
                        let ready = server.audit_readiness()
                            .map_err(|e| io::Error::other(format!("post-disconnect readiness: {e:?}")))?;
                        wire.trace.event(format!("disconnect_after_answer_readiness={ready:?}"))?;
                        require(
                            ready == wire::ExportStatus::Ready {
                                remaining: 0,
                                objects: u32::try_from(corpus.scopes.len()).map_err(io::Error::other)?,
                            },
                            "disconnect changed the still-owned aggregate accounting",
                        )?;
                        // The reference is our original create-exclusive raw held
                        // snapshot, read with the existing bounded regular-file helper.
                        // It is not a replacement object/ledger/cache or a new grant.
                        let before = [
                            corpus_bytes(&wire.trace.root.join("reserved.member-index-0.bin"))?,
                            corpus_bytes(&wire.trace.root.join("reserved.member-index-1.bin"))?,
                        ];
                        let after = snapshot(&files, wire.trace, "disconnect-after-answer", corpus.budget)?;
                        require(after == before, "disconnect changed held bytes or retained extent")?;
                        let [first, second] = &after;
                        require(
                            sum(amount(first.len())?, amount(second.len())?)? == corpus.budget,
                            "disconnect changed the aggregate retained length",
                        )?;
                        wire.trace.event("disconnect_individual_lease_stage_and_live_cursors=Unobserved; last wire Reserved receipt and held byte fields retained; no post-disconnect protocol reply or refund inferred")
                    })()),
                    Case::ReservedDisconnect => {
                        let _ = wire.trace.event("disconnect_post_join_observations=UnreachedOrUnknown; prior sequence/answer/FD failure retained");
                        result
                    }
                    Case::Committed | Case::ReservedClose => result,
                };
"""
once("                result.and(cleanup)", observation + "                result.and(cleanup)")
test = """
#[test]
#[ignore = "SOURCE PROPOSAL ONLY: requires ROOT source/build/actor authorization"]
fn reserved_client_disconnect_retains_owned_extent() {
    let outcome = run(Case::ReservedDisconnect);
    assert!(outcome.is_ok(), "actual native Reserved disconnect fixture: {outcome:?}");
}
"""
proposed += test

# The entire predecessor is recovered, not merely a count of assertions. The
# proposed third case consists solely of insertions around the existing path.
recovered = proposed.removesuffix(test).replace(arm, "").replace(observation, "")
recovered = recovered.replace("    ReservedClose,\n    ReservedDisconnect,\n}", "    ReservedClose,\n}")
assert recovered == original
for begin, end in [("fn reserved_close_sequence(", "\nfn owned_case("),
                   ("fn snapshot(", "\nfn sequence(")]:
    a = original[original.index(begin):original.index(end)]
    b = proposed[proposed.index(begin):proposed.index(end)]
    assert a == b

path = "docs/research/measurements/managed-audit-wire-adversarial/native.rs"
patch = "".join(difflib.unified_diff(original.splitlines(True), proposed.splitlines(True), fromfile="a/" + path, tofile="b/" + path))
for name, data in [("native.proposed.rs", proposed), ("native-caller.unformatted.patch", patch)]:
    with (here / name).open("x") as file:
        file.write(data)
paths = ["crates/vmm/src/devices/virtio/fs/mod.rs", "crates/vmm/src/devices/virtio/fs/server.rs",
         "crates/vmm/src/devices/virtio/fs/audit.rs", "crates/abi/src/run/audit.rs",
         "crates/ipc/src/unix.rs", "crates/vmm/src/platform/mod.rs"]
pins = []
for source_path in paths:
    raw = subprocess.check_output(["git", "show", "a7185733c4bfb1cc18d8018b96ac1790b29ecb24:" + source_path])
    frozen = subprocess.check_output(["git", "show", "ba78d4ba775e7d83b7fea9a5989e3ca6b9857a2a:" + source_path])
    assert raw == frozen, source_path
    pins.append({"path": source_path, "sha256": hashlib.sha256(raw).hexdigest(),
                 "bytes": len(raw), "equal_frozen_ba78": True})
receipt = {"status": "SOURCE_PROPOSAL_UNAPPLIED_NO_BUILD_NO_ACTOR", "base": "a7185733c4bfb1cc18d8018b96ac1790b29ecb24",
           "actual_leaf_sha256": hashlib.sha256(original.encode()).hexdigest(),
           "proposed_unformatted_sha256": hashlib.sha256(proposed.encode()).hexdigest(),
           "unformatted_patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
           "entire_previous_source_recovered_by_removing_insertions": True,
           "previous_24_and_19_bodies_assertions_helpers_cleanup_lines_exact": True,
           "selected_request_count": 8, "explicit_Close_Release_Commit_WRITE_DESTROY_after_reserve": 0,
           "new_threads_wrappers_queues_caches_managers_production_changes": 0,
           "expected_answer": "Ok(()) via read_any_into frame-header EOF; actual future result must be recorded",
           "expected_public_accounting": "Ready remaining0 and original scope count while Server remains owned",
           "last_actual_wire_reply": "Reserved/None from request8 with actual validated lease/cursors",
           "post_disconnect_individual_lease_stage_and_live_cursors": "Unobserved; no fabricated Closed or refund",
           "post_Server_drop_accounting": "Unobservable; owned file/root cleanup observed separately",
           "source_pins": pins,
           "new_builds": 0, "new_native_cases": 0, "new_target_or_cache": False}
with (here / "source-scope.unformatted.json").open("x") as file:
    json.dump(receipt, file, indent=2)
    file.write("\n")
assert leaf.read_text() == original
print(json.dumps(receipt, indent=2))
