# Four test-helper expect lint corrections

Root's actual normal-helper managed_output caller compiled0 at composition
`cfc3f2b17a0397f044d93ec5cd3b77ffc7886599` (r472). Root then reported actual
supported-target `scripts/lint` at `a5b428551c80ebcc0ee268f0ca22bf8359a729b4`
failed101 in GNU managed_output (r485): exactly four `clippy::expect_used`
violations in this fixture. Original raw remains root-owned, unchanged at
Macb24 `~/root-managed-seven-target-a5b4-v2/lint.log`. This record preserves the
coordinator's actual red; the agent did not rerun a full matrix to reproduce it.

Signed SOURCE fix `ed403164fa1965b1af913a289cae94bb73a9a9c9` is a direct child
of evidence `b9c29de54ede5c7fcdd2d9066350854fbddf2d68` on
`p/managed-output-cost`. The **only** source path changed is `fixture.rs`:

| Original check | Same condition after correction |
| --- | --- |
| `members.checked_add(1).expect(...)` | Assert that the checked result is Some. |
| Checked channel count multiplication's `expect(...)` | Assert that the checked result is Some. |
| Checked absolute phase deadline's `expect(...)` | Assert that the checked result is Some. |
| Metadata parse's `expect(...)` | Surrounding test style `unwrap_or_else(|| panic!(...))`. |

All four original failure messages are retained. No success value from the first
three expressions was used before or after. Metadata still must parse before its
same run/owner/server/scoped-endpoint assertions. No default, limit, fallback,
validator, deadline, architecture, production source, dependency or lint setting
changed. Tests may panic under R01; no production exception is introduced.

Local formatter, shell syntax and policy checks0. Native Fedora GNU amd64 r240,
one admitted private4CPU/1GiB/2GiB request, compiled only the existing standalone
synthetic parser with `rustc --test -D warnings`: **3 passed, 0 failed/ignored**.
No real fixture, VM, proc/resource observation, profile, audit or matrix ran.
The pure parser source is unchanged and its executable hash remains
`04e3723eeef9a09401951406e7451add67ed3994ac34de2ad873ed02b20d11f1`, retained at
`~/managed-output-cost-target/parser-tests-expect-fix`. These tests **do not**
compile or lint `fixture.rs` through its real caller and cannot close r485.

[Commands/exits/raw](records/expect-fix-native), [job](records/expect-fix-job.json),
[transfer](records/expect-fix-transfer.json), [manifest](records/expect-fix-manifest.json),
[verification](records/expect-fix-verification.json) and
[local checks](records/expect-fix-local-checks.json) retain the light scope.
Original archive1575B SHA256
`9fd8272698652d54013426e30e59a271ae1d9550b5953b6c713644e136d233d9`;
14 original raw files and 5 actual-source hashes verified against source Git
blobs. Native checkout status was clean; compiler pinned Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, Linux6.19.10-300.fc44 amd64.
The original libtest EOF blank is retained rather than sanitized.

Owed root gates: actual changed-revision caller/strict Clippy, full supported
matrix, existing real-output regressions, explicit N0/N1/N2/finite-count pilots,
and every resource/compatibility/adoption obligation in [README](README.md).
Root's earlier caller compile green is revision-specific, not a new-revision
green. No allocation/performance/leak-free or main-readiness claim follows.

Renewed POLICY_READ implementer: FULL canonical g121-main AGENTS/R01–R16,
development-evidence/CLAUDE/managed plan and updated handoff; main revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Ownership only four fixture.rs sites and local evidence. All R01–R16 accounting
and limits in README remain: R02/R03/R05/R12 broader decisions are inapplicable
to a test-style-only correction; R04/R06/R07/R13 preserve actual guards/state;
R09/R10 use the reported live red and exact retained source/light evidence;
R08/R14/R15 central costs/audits/E2E/matrix/adoption/merges remain root-only;
R11 unrun targets stay unverified; R16 private paths only, no root cache writes,
cleanup or local heavy build. No children/main merge or paused Windows ARM work.
