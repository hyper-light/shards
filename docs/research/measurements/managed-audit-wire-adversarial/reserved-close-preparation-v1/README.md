# Exact Reserved→Closed caller proposal — UNAPPLIED

The [19-frame contract](../NEXT-RESERVED-CLOSE-PROPOSAL.md) now has a concrete
[native-caller.patch](native-caller.patch), SHA256
`87f101726e01beb0654600bfc8b7f0d826bb1462fdc9a8e6bfd497c155831820`.
Its base is signed33ea5a816; proposed leaf SHA256 is
`066d59449e3019a02e8822ada398b8beebad6dbf790600a657fad34fc16b6ab0`.
The actual native.rs is still byte-exact executed75427cca. No patch was applied,
new build/target/source package allocated or actor started. ROOT review/GO is
required before applying/building/running this distinct case.

The only proposed source file is the same cfg(test) native leaf. A private
two-variant `Case` selects either the original complete commit sequence or this
new early Close sequence AFTER the shared first8 requests/reservation checks.
It is passed through the same run/owned_case/sequence functions. The original
24-frame body and assertions remain exact apart from selector forwarding; the
preparer programmatically reconstructs and compares its statements. Wire,
frame/parser, corpus loading, snapshot helpers and cleanup predicates/order are
unchanged. The original test remains present and uses Case::Committed. This is
source preservation, not a fresh runtime gate for the proposed changed caller.

The new helper sends exactly7 controls after Reserve: Close, duplicate Close,
Inspect, same Reserve, CommitIntent, CommitTerminal, final Inspect. Each matches
the whole actual typed Reply, retaining lease/maxima/offsets/accepted0/remaining0
and native/failure fields. All whole-file snapshots equal the held post-Reserve
snapshot. Member3 Reserve/Identify and two releases finish exact19 requests,
with typed Exhausted, unchanged neighbor/retained extents and no refund. No WRITE
is introduced. The new exact ignored filter would be
`managed_audit_wire_adversarial::reserved_close_retains_extent_and_refuses_reopening`.
No production constructor/ABI/writer/parser/transport or include change is needed.

## Actual static preparation and remaining gates

`python3 prepare-reserved-close-patch.py` returned0, emitting create-exclusive
patch/source/readback files while verifying that the actual frozen leaf did not
change. `git apply --check <native-caller.patch>` returned0, without applying it.
`python3 scripts/check-development-policy` returned0.

The attempted canonical rustfmt under `/tmp/g121-mac.lock` did NOT execute the
formatter: lockf returned75, `already locked` (actual result/stderr retained here).
No retry or bypass. Therefore syntax/format/compilation/Clippy for this proposal
are UNVERIFIED, and actor count remains0. There is no new native campaign deadline
or entitlement. Intended later bounds remain those in the contract, but only a
fresh ROOT-authorized normally admitted private scope can start them.

Full staged `git diff --check` returns2 on FIVE literal single-space context
lines in the exact unapplied patch (4,13,19,48,148). Those original patch bytes
are preserved. Excluding only that named patch artifact returns0; this does not
establish canonical Rust formatting or compilation. The separately retained
proposed Rust file contains no whitespace-check finding.

This is source preparation under policy current0c598386/rulesdf2, implementer /
assigned adversarial. R01/R04/R06/R07/R13: test-only typed full-reply/snapshot
predicates, unchanged real cleanup; R02/R03/R05/R12: same reviewed API/corpus and
one existing path, no production literals or speculative architecture; R09/R10:
no new live defect/pass claim, original24-frame source/raw preserved; R08/R14:
no profile/benchmark/cost claim; R11: all native/matrix/VM results for this new
source remain unrun; R15: exact unexecuted diff for ROOT review, no children/goals/
main merge; R16: the local30GiB floor check passed before the lock attempt; actual
free bytes are in rustfmt.result.json. No data/cache deletion, protected scopes
and old FIN untouched. Lock refusal is preserved.
Full management closure and ROOT's integration/authority/storage-fault/cost gates
remain open.
