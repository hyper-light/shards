#!/usr/bin/env python3
"""Emit an UNAPPLIED cfg(test) proposal; never modifies the frozen native leaf."""
import difflib
import hashlib
import json
from pathlib import Path


root = Path(__file__).parent
leaf = root / "native.rs"
original = leaf.read_text()
assert hashlib.sha256(original.encode()).hexdigest() == "75427cca006a347f7b6a02ef88203e3b947db3d5f74157192001d1dc097e8a00"
proposed = original


def replace_once(before, after):
    global proposed
    assert proposed.count(before) == 1, before
    proposed = proposed.replace(before, after)


replace_once("type Snapshot = [Vec<u8>; 2];\n", """type Snapshot = [Vec<u8>; 2];

#[derive(Clone, Copy)]
enum Case {
    Committed,
    ReservedClose,
}
""")
replace_once(
    "fn sequence(wire: &mut Wire<'_>, corpus: &Corpus, files: &[File; 2], initial: &Snapshot) -> Result<()> {",
    "fn sequence(wire: &mut Wire<'_>, corpus: &Corpus, files: &[File; 2], initial: &Snapshot, case: Case) -> Result<()> {",
)
old_boundary = """        "actual immutable slot differs",
    )?;
    require(
        wire.control(node, fh, scope, reserve)? == reserved,"""
new_boundary = """        "actual immutable slot differs",
    )?;
    match case {
        Case::Committed => {}
        Case::ReservedClose => {
            return reserved_close_sequence(
                wire,
                corpus,
                files,
                (node, fh, other_node, other_fh),
                reserved,
                &current,
            );
        }
    }
    require(
        wire.control(node, fh, scope, reserve)? == reserved,"""
replace_once(old_boundary, new_boundary)

helper = """fn reserved_close_sequence(
    wire: &mut Wire<'_>,
    corpus: &Corpus,
    files: &[File; 2],
    handles: (u64, u64, u64, u64),
    reserved: wire::Reply,
    retained: &Snapshot,
) -> Result<()> {
    let (node, fh, other_node, other_fh) = handles;
    let [scope, other] = corpus.scopes;
    let [data, neighbor] = &corpus.records;
    let closed = wire::Reply {
        stage: wire::Stage::Closed,
        ..reserved
    };
    let reserve = wire::Request::Reserve {
        call: AUDIT_CALL,
        intent: data.intent_max,
        terminal: data.terminal_max,
    };
    // These are seven selected ordinary controls, not recovery retries. No
    // append occurs: every result must retain the exact post-Reserve objects.
    for (name, request, cause) in [
        ("reserved-close", wire::Request::Close { call: AUDIT_CALL }, wire::Cause::None),
        ("reserved-close-duplicate", wire::Request::Close { call: AUDIT_CALL }, wire::Cause::None),
        ("reserved-close-inspect", wire::Request::Inspect { call: AUDIT_CALL }, wire::Cause::None),
        ("closed-reserve-refused", reserve, wire::Cause::Closed),
        ("closed-intent-refused", wire::Request::CommitIntent { call: AUDIT_CALL }, wire::Cause::Closed),
        ("closed-terminal-refused", wire::Request::CommitTerminal { call: AUDIT_CALL }, wire::Cause::Closed),
        ("closed-final-inspect", wire::Request::Inspect { call: AUDIT_CALL }, wire::Cause::None),
    ] {
        require(
            wire.control(node, fh, scope, request)? == wire::Reply { cause, ..closed },
            format!("actual {name} state/cause/cursors/accepted/credit differ"),
        )?;
        require(
            snapshot(files, wire.trace, name, corpus.budget)? == *retained,
            format!("actual {name} changed the retained slot or neighbor"),
        )?;
    }
    require(wire.sent == 15, "did not execute exact seven post-Reserve controls")?;
    empty(
        wire.control(
            other_node,
            other_fh,
            other,
            wire::Request::Reserve {
                call: AUDIT_CALL,
                intent: neighbor.intent_max,
                terminal: neighbor.terminal_max,
            },
        )?,
        0,
        wire::Cause::Exhausted,
    )?;
    require(
        snapshot(files, wire.trace, "closed-neighbor-refused", corpus.budget)? == *retained,
        "closed member refunded extents or neighbor refusal changed files",
    )?;
    empty(
        wire.control(other_node, other_fh, other, wire::Request::Identify)?,
        0,
        wire::Cause::None,
    )?;
    require(
        snapshot(files, wire.trace, "closed-neighbor-identified", corpus.budget)? == *retained,
        "neighbor identification changed files",
    )?;
    wire.release(node, fh)?;
    wire.release(other_node, other_fh)?;
    require(wire.sent == 19, "did not execute exact19 requests")?;
    require(
        snapshot(files, wire.trace, "closed-reserved-released", corpus.budget)? == *retained,
        "RELEASE changed the retained uncommitted slot",
    )?;
    let [first, second] = retained;
    require(
        sum(amount(first.len())?, amount(second.len())?)? == corpus.budget,
        "actual retained lengths differ from the aggregate B after early Close",
    )
}

"""
replace_once(
    "fn owned_case(corpus: &Corpus, root: &Path, trace: &mut Trace, deadline: Instant) -> Result<()> {",
    helper + "fn owned_case(corpus: &Corpus, root: &Path, trace: &mut Trace, deadline: Instant, case: Case) -> Result<()> {",
)
replace_once("let result = sequence(&mut wire, corpus, &files, &initial);", "let result = sequence(&mut wire, corpus, &files, &initial, case);")
replace_once("fn run() -> Result<()> {", "fn run(case: Case) -> Result<()> {")
replace_once("let result = owned_case(&corpus, &directory, &mut trace, deadline);", "let result = owned_case(&corpus, &directory, &mut trace, deadline, case);")
replace_once("let outcome = run();", "let outcome = run(Case::Committed);")
proposed += """
#[test]
#[ignore = "requires separate ROOT source and one-case dispatch approval"]
fn reserved_close_retains_extent_and_refuses_reopening() {
    let outcome = run(Case::ReservedClose);
    assert!(outcome.is_ok(), "actual native reserved Close fixture: {outcome:?}");
}
"""

# Removing just the new selector/scaffold must recover every original positive
# sequence statement; cleanup and byte predicates themselves are not rewritten.
original_sequence = original[original.index("fn sequence("):original.index("\nfn owned_case(")]
proposed_sequence = proposed[proposed.index("fn sequence("):proposed.index("\nfn reserved_close_sequence(")]
recovered = proposed_sequence.replace(
    "initial: &Snapshot, case: Case)", "initial: &Snapshot)",
).replace(new_boundary, old_boundary)
assert recovered == original_sequence
for name, end in [("fn snapshot(", "\nfn changed("), ("impl Wire<'_>", "\nfn empty(")]:
    assert original[original.index(name):original.index(end)] == proposed[proposed.index(name):proposed.index(end)]

output = root / "reserved-close-preparation-v1"
output.mkdir()
path = "docs/research/measurements/managed-audit-wire-adversarial/native.rs"
patch = "".join(difflib.unified_diff(original.splitlines(True), proposed.splitlines(True), fromfile="a/" + path, tofile="b/" + path))
with (output / "native-caller.patch").open("x") as file:
    file.write(patch)
with (output / "native.proposed.rs").open("x") as file:
    file.write(proposed)
receipt = {"status": "UNAPPLIED_SOURCE_PROPOSAL_ONLY", "base": "33ea5a816ae1d4f9656762d2acedc98ec00ccc34",
           "frozen_leaf_sha256": hashlib.sha256(original.encode()).hexdigest(),
           "proposed_leaf_sha256": hashlib.sha256(proposed.encode()).hexdigest(),
           "patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
           "original_committed_sequence_statements_exact": True,
           "wire_parser_frame_and_snapshot_helpers_exact": True,
           "cleanup_predicates_and_order_unchanged": True,
           "new_selected_requests": 19, "new_builds": 0, "new_actors": 0,
           "required_before_apply_build_actors": "ROOT review/GO; new normally admitted scope/deadline/dispatch"}
with (output / "source-scope.json").open("x") as file:
    json.dump(receipt, file, indent=2)
    file.write("\n")
assert leaf.read_text() == original
print(json.dumps(receipt, indent=2))
