# Native startup custody: typed member refusals keep the host's answer

ROOT coordinator 01a148884a4770fdaf0111f3f792a248. POLICY_READ main
bff3015182ad754274f1048ce393ac12cb6cd91f, rules
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3
(`POLICY_READ.json`; resume receipt `POLICY_READ-resume-1.json`). Owned: this
directory and its ignored isolated candidates. No active peer file and no main
production file changed. This is ROOT composition onto the runtime owner's
unadopted custody v10d source (manifest 9904e4bb, owner 01a1c99e), not an
adoption of v10d.

## Finding

v9b (manifest f2fdbf3f, archive 2261860c) turned a failed clone3, an
unrepresentable PID and a failed `Launch::parent` into `StartFailure::Setup`
strings (`starting <label>: <error>`, `<label>: the kernel returned a PID out of
range`, `<label>: retaining its owner: <error>`). `run.rs` answered each with
`SYSTEM_ERR` text and status `NOT_RUN` (125), then EXIT, the layer handshake and
power-off. v10d replaces them with typed `Clone`, `Pid` and `OwnerParent` (and
adds `StartupState`, `StartupAllocation`, `MetadataAllocation`) but leaves
`domain_start_failed` byte-identical, so its catch-all routes every new variant
to `UnreportedDomain`: a guest stderr line and immediate power-off, which the host
sees as VM loss (Unknown) instead of a definite not-run refusal.

## Live reproduction (R09)

r14, scope `root-custody-report-red-1794423907582735000`, source manifest
ffa7e771 (v10d + the reporting match moved verbatim into
`report_startup_failure` + two new tests, `REPORT-EXTRACTION.patch`). Whole init
check 0, test ELF b7626f96 (76763184 B) frozen and listed before cases. Both new
cases failed at the reporting assertion: `left: None`, `right: Some(125)`, with
the UnreportedDomain stderr line (`member 7 clone returned -1:
PermissionDenied, OS Some(1)`; `member 9: member parent association State;
rejected group held true`). Unit-level reproduction through the real
`domain_start_failed`/`report_startup_failure` and the real
`PendingMember::parent` guard; no VM reproduction is claimed.

ROOT then found a weakness in its own r14 case 1: it checked the rejected group
by descriptor number (`fcntl`), which a reused number can falsely satisfy. RED-v2
and GREEN observe the socket object instead: its peer reads `WouldBlock` while the
group is owned and EOF once it is released (`RED-V2-ON-RED.patch`, tests only).

## Change

`ROOT-RUN-ON-v10d.patch` (v10d run.rs c5b8d0d8, 277040 B -> d1a383f8, 281586 B;
`git apply --check`/apply 0 on a fresh copy of the pinned preimage):

- the existing borrowed reporting match moves into `report_startup_failure`,
  called at the same shutdown boundary, responses unchanged;
- `StartupFailure::RefusedDomain(StartFailure)` holds `Clone`, `Pid` and
  `OwnerParent`; reporting renders it through the existing fallible
  `run::render_error`, sends `SYSTEM_ERR` and answers `NOT_RUN`. The typed
  original, including a rejected parent group `File`, stays owned by `outcome`
  until power-off; presentation only borrows it;
- two tests in `run::startup_custody_tests`.

Alternatives (R02/R04/R05/R13). Keeping VM loss changes a definite pre-workload
refusal into Unknown, with no recorded rationale (the runtime record itself lists
it for compatibility review). Converting back to `Setup(String)` restores the
text but drops the typed failure and its rejected group inside the conversion,
before member shutdown; the custody design keeps a rejected transfer owned by the
real startup error, and the mutant below shows the tests catch that. A flag or
classification method on `StartFailure` would duplicate the variant in a second
place; callers match variants instead. This restores existing protocol behavior
at the existing seam: no new wire kind, interface, constant or state machine
(`NOT_RUN` keeps its cited docker/cli `toStatusError` derivation), so no broader
external research applied.

## Result (r87, all expectations matched)

`run-green.py` fb34731d, package `source-green-delta.zlib` 9d613a88 (533184 B,
carries GREEN, RED-v2 and MUTANT run.rs byte-exactly), source manifest 7e42d3ca
(4789 entries, Cargo.lock 7ee94a5e = the private init cohort's lock, not main
1ecb). Runner 451397, Linux 7.2.9-2-cachyos x86_64, UID 1000, CPUs 0-1,
rustc 1.98.1 (48a229cea), `--locked --offline -j2`, RLIMIT_AS 2 GiB per child,
ONE 600 s budget from the first write; 64.2 s used. Every stage's direct child
waited, FD 9 -> 9.

| stage | result |
|---|---|
| policy, consumer rustfmt --check, whole init all-target check | 0 |
| strict clippy -D warnings | 101, 17 headings IDENTICAL to r8 (15 bin + 11 bin-test dead code on unconsumed input/RAW/owner_input APIs): unchanged known red, not a pass |
| GREEN d1a383f8 -> ELF 60a21c30 | both cases pass |
| RED-v2 b784b38e -> ELF 8eb2f985 | both fail: `typed member refusal must answer SYSTEM_ERR with NOT_RUN`, `left: None`, `right: Some(125)` |
| MUTANT bec3bab8 -> ELF 5bc51b7d | case 0 passes; case 1 fails: `rejected group was released before its startup failure`, `left: Ok(0)` |

Each ELF was frozen and listed before its cases and unchanged after them. The
source was restored to GREEN and all 4789 hashes re-verified; the read-only
template (4790 files) and registry 3528 / git 1802 caches are unchanged.
Records: `RESULTS-r87.json`, raw `raw-r87.tar.xz` 448772 B b1f373a8 (87 payloads
+ manifest 1d0ea286, every payload re-hashed on the Mac).

r8 (same runner, immutable v10d 9904 with ROOT's compiler-only driver, zero
cases): check 0, policy/format 0, strict 101 with the same 17 headings as v9b's
raw; v10b's `large_enum_variant` at managed.rs:293 is gone and v10d adds no
diagnostic. r8 and r14 raw: `raw-r8-r14.tar.xz` 446696 B 1181daa0 (31 + 46
payloads and both manifests verified), summary `RESULTS-r8-r14.json`.

## R01–R16

- R01: the new arms use no unwrap, indexing or unchecked arithmetic;
  `render_error` sizes with checked addition and `try_reserve_exact` and yields
  empty text on failure; the send result is ignored as the baseline's is. Tests
  may unwrap.
- R02/R03: no architectural decision or new literal (see Change).
- R04: one typed variant; the transition is refusal -> SYSTEM_ERR -> EXIT ->
  layer handshake -> shutdown -> power-off, the baseline's path; ownership until
  power-off is stated where it lives.
- R05: no wait or cache is touched; reporting is the existing synchronous send.
- R06/R07/R13: one arm in each of two matches; one comment states the custody
  invariant and the predecessor behavior.
- R08/R14: no performance, allocation or copy claim. The refusal path renders one
  String as v9b's `format!` did; not measured.
- R09/R10: red r14 and RED-v2, green r87, two meaningful mutants (guard absent =
  RED-v2; custody dropped = MUTANT), raw retained and verified.
- R11/R12: only x86_64-unknown-linux-gnu was checked. Production shards-init is
  static musl on x86_64 and aarch64: both unverified. No platform code.
- R15: ROOT integration composition and light unit validation only; no VM, actor,
  helper, native child, cgroup or population evidence; the full shards-init suite
  was not run (two named cases only). Held audit graph/five untouched.
- R16: runner scopes retained: `root-custody-v10d-check-1794423492919687000`,
  `root-custody-report-red-1794423907582735000`,
  `root-custody-report-green-1794424717422` and the two raw-recovery dirs.

## Open

- `StartupState`, `StartupAllocation` and `MetadataAllocation` still end in VM
  loss. v9b had no SYSTEM_ERR answer for them (their allocations were infallible),
  so reporting them is a new public-failure decision for the runtime owner and
  ROOT, not a restoration.
- The typed text names the member number and the error kind/errno; v9b's named
  the domain label. The label is not carried by the typed failure.
- v10d stays unadopted and unsigned runtime source; this delta must compose with
  it in the combined graph (main has no native/custody stack yet).
- Strict 101 (unconsumed input/RAW/owner_input) remains owed by those consumers.
