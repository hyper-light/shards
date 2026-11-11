# Native startup custody: typed member refusals keep the host's answer

ROOT coordinator 01a148884a4770fdaf0111f3f792a248. POLICY_READ main
bff3015182ad754274f1048ce393ac12cb6cd91f, rules
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3
(`POLICY_READ.json`; resume receipt `POLICY_READ-resume-1.json`). Owned: this
directory and its ignored isolated candidates. No active peer file and no main
production file changed. This is ROOT composition onto the runtime owner's
unadopted custody v10d source (manifest 9904e4bb, owner 01a1c99e), not an
adoption of v10d. The final delta is `ROOT-RUN-ON-v10d-v2.patch`.

## Finding

v9b (manifest f2fdbf3f, archive 2261860c) turned a failed clone3, an
unrepresentable PID and a failed `Launch::parent` into `StartFailure::Setup`
strings (`starting <label>: <error>`, `<label>: the kernel returned a PID out of
range`, `<label>: retaining its owner: <error>`). `run.rs` answered each with
`SYSTEM_ERR` text and status `NOT_RUN` (125), then EXIT, the layer handshake and
power-off. v10d replaces them with typed `Clone`, `Pid` and `OwnerParent` and adds
the fallible-custody variants `StartupState`, `StartupAllocation` and
`MetadataAllocation`, but leaves `domain_start_failed` byte-identical. Its
catch-all therefore routes all six to `UnreportedDomain`: a guest stderr line and
immediate power-off, which the host sees as VM loss (Unknown) instead of a
definite start refusal.

## Live reproduction (R09)

r14, scope `root-custody-report-red-1794423907582735000`, source manifest
ffa7e771 (v10d + the reporting match moved verbatim into
`report_startup_failure`, `REPORT-EXTRACTION.patch`, + two new tests). Whole init
check 0, test ELF b7626f96 frozen and listed before cases. Both cases failed at the
reporting assertion (`left: None`, `right: Some(125)`) with the UnreportedDomain
stderr lines. Unit-level reproduction through the real `domain_start_failed`,
`report_startup_failure` and `PendingMember::parent` guard; no VM reproduction is
claimed.

ROOT then found a weakness in its own r14 case 1: it checked the rejected group by
descriptor number (`fcntl`), which a reused number can falsely satisfy. All later
runs observe the socket object instead: its peer reads `WouldBlock` while the
group is owned and EOF once it is released.

## Decision

v1 (`ROOT-RUN-ON-v10d.patch`, r87) restored only the three failures that had a v9b
answer and asked the runtime owner about the other three.
`DECISION-RUNTIME-OWNER.json` records the verbatim reply: VM loss for
`StartupState`, `StartupAllocation` and `MetadataAllocation` was an unfinished
presentation bridge, not policy; route them to `RefusedDomain` too. `NOT_RUN`
means the run failed to start, never that no member was born or that cleanup
finished. The numeric member is intentional (stable identity, no allocation
during failure); a domain label may only supplement presentation through an
existing trusted borrow, which v2 does not add.

ROOT checked the post-clone case the owner named: v10d domains.rs:2137 returns
`StartupState` after a successful clone when the staged group is absent. The born
child stays in `PendingMember` custody and `run.rs` shuts pending and started
members down before any report, so routing changes only the host's answer. The
other sites (domains.rs:1580, 1855, 1858, 1870, 2047) precede that member's clone.

Alternatives (R02/R04/R05/R13). Keeping VM loss turns a definite start refusal
into Unknown. Converting back to `Setup(String)` drops the typed failure and its
rejected group inside the conversion, before member shutdown; the mutant shows the
tests catch that. A flag or classification method on `StartFailure` would repeat
the variant in a second place; callers match variants. This completes existing
protocol behavior at the existing seam: no new wire kind, interface, constant or
state machine (`NOT_RUN` keeps its cited docker/cli `toStatusError` derivation),
so no broader external research applied.

## Change

`ROOT-RUN-ON-v10d-v2.patch` (8779 B, a5160382; v10d run.rs c5b8d0d8, 277040 B ->
dff63046, 282189 B; `git apply --check`/apply 0 on a fresh copy of the pinned
preimage, postimage hash exact):

- the existing borrowed reporting match moves into `report_startup_failure`,
  called at the same shutdown boundary, responses unchanged;
- `StartupFailure::RefusedDomain(StartFailure)` holds the six new custody
  variants; reporting renders through the existing fallible `run::render_error`,
  sends `SYSTEM_ERR` and answers `NOT_RUN`. The typed original, including a
  rejected parent group `File`, stays owned by `outcome` until power-off;
  presentation only borrows it. Pre-existing variants keep their v9b routing;
- `run::startup_custody_tests`: `member_start_refusals_report_system_error_and_not_run`
  (all five non-parent refusals, frame text and status, and an unchanged
  `MissingNativeOwner` sending nothing) and
  `rejected_parent_group_survives_system_error_until_owner_drop`.

`GREEN-V2-ON-V1.patch` is the v1 -> v2 step; `MUTANT-V2-ON-GREEN-V2.patch` is the
mutant.

## Result (r182, all expectations matched)

`run-green-v2.py` 3b33f3a2, package `source-green-v2-delta.zlib` 373406de
(640766 B, all four run.rs variants byte-exact), source manifest 93855c9e
(4789 entries, Cargo.lock 7ee94a5e = the private init cohort's lock, not main
1ecb). Runner 451397, Linux 7.2.9-2-cachyos x86_64, UID 1000, CPUs 0-1, rustc
1.98.1 (48a229cea), `--locked --offline -j2`, RLIMIT_AS 2 GiB per child, ONE 600 s
budget from the first write; 181.1 s used. All 22 stages: direct child waited,
FD 9 -> 9, no timeout.

| stage | result |
|---|---|
| policy, consumer rustfmt --check, whole init all-target check | 0 |
| strict clippy -D warnings | 101, 17 headings IDENTICAL to r8 (15 bin + 11 bin-test dead code on unconsumed input/RAW/owner_input APIs): unchanged known red, not a pass |
| GREEN-v2 dff63046 -> ELF 9ac35aeb | both cases pass |
| RED-v10d 737a39ad (v10d routing) -> ELF e30a6029 | case 0 `left: [None, None, None, None, None]`; case 1 `left: None` |
| RED-v1 767f819d (r87 routing) -> ELF a56f5e28 | case 0 `left: [Some(125), Some(125), None, None, None]`; case 1 passes |
| MUTANT-v2 33870450 -> ELF 590fa4d5 | case 0 passes; case 1 `rejected group was released before its startup failure`, `left: Ok(0)` |

Each ELF was frozen and listed before its cases and unchanged after them. All four
variants carry byte-identical tests; RED-v10d's production equals r14's. The
source was restored to GREEN-v2 and all 4789 hashes re-verified; the read-only
template (4790 files) and registry 3528 / git 1802 caches are unchanged. Records:
`RESULTS-r182.json`, raw `raw-r182.tar.xz` 449712 B 0d2b57fd (105 payloads +
manifest 63f55383, every payload re-hashed on the Mac).

History: r87 (v1, `RESULTS-r87.json`, raw b1f373a8) passed GREEN 2/2, RED-v2 2/2
red and its mutant. r8 compiled the immutable v10d 9904 (check 0, strict 101 with
the same 17 headings as v9b, zero cases); r8 and r14 raw `raw-r8-r14.tar.xz`
1181daa0, summary `RESULTS-r8-r14.json`.

## R01–R16

- R01: the new arms use no unwrap, indexing or unchecked arithmetic;
  `render_error` sizes with checked addition and `try_reserve_exact` and yields
  empty text on failure; `send` writes a stack header and the payload without
  allocating; its result is ignored as the baseline's is. Tests may unwrap.
  Pre-existing observation, unchanged and not reproduced: the shared terminal
  `asked_to_save` allocates `vec![0u8; CHUNK]` infallibly, and allocation refusals
  now reach it after `SYSTEM_ERR` and EXIT(125) were sent.
- R02/R03: no architectural decision or new literal (see Decision).
- R04: one typed variant; refusal -> member shutdown -> SYSTEM_ERR -> EXIT(125)
  -> layer handshake -> power-off, the baseline Setup path; ownership until
  power-off is stated where it lives.
- R05: no wait or cache is touched; reporting is the existing synchronous send.
- R06/R07/R13: one arm in each of two matches; one comment states the custody
  invariant and what NOT_RUN does not imply.
- R08/R14: no performance, allocation or copy claim; the refusal path renders one
  String as v9b's `format!` did; not measured.
- R09/R10: red r14, RED-v10d and RED-v1, green r182, two meaningful mutants
  (routing absent = RED-v10d/RED-v1; custody dropped = MUTANT-v2), raw retained
  and verified.
- R11/R12: only x86_64-unknown-linux-gnu was checked. Production shards-init is
  static musl on x86_64 and aarch64: both unverified. No platform code.
- R15: ROOT integration composition and light unit validation only; no VM, actor,
  helper, native child, cgroup or population evidence; the full shards-init suite
  was not run (two named cases only). Held audit graph/five untouched.
- R16: runner scopes retained: `root-custody-v10d-check-1794423492919687000`,
  `root-custody-report-red-1794423907582735000`,
  `root-custody-report-green-1794424717422`,
  `root-custody-report-green-v2-1794425344162` and three raw-recovery dirs.

## Open

- v10d stays unadopted and unsigned runtime source; this delta must compose with
  it in the combined graph (main has no native/custody stack yet).
- Strict 101 (unconsumed input/RAW/owner_input) remains owed by those consumers.
- musl, aarch64, the full shards-init suite and real-VM startup failures are
  unverified; the `asked_to_save` allocation is recorded, not fixed.
