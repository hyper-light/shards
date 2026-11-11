# Getter cleanup checkpoint

Removed exactly the five getters unused by the actual runtime owner:
Prepared job/directory, Published job, and Retained job/directory. Tests inspect
the same private fields or Holding variants. `Published::directory` remains.
No stored field, ownership state, syscall, cleanup path, constant or runtime
caller was changed. No fake use, suppression or replacement interface was added.
`API.md` is the exact updated handoff for ROOT to forward to runtime owner
`01a1c99e7b717018b81686624876ad5d`.

Before `f1dc920bf54cc1d2bb86025f77f5658b9485f83bd26b87febf7a418df80405cb`;
after `7878ad24229335ce3f2fa6ef9c06cd1e3f03d8837d62a56dcb1cefec833424e3`.
The exact delta is `UNUSED-GETTERS.patch`, SHA-256
`034fcf05cd9678027ec54c4c0aff8867b98009e4d81ca275802503c4bb3dd835`.
Old commit `25d9da41ffe580879a7d8d6753d25dc14944450a`, all earlier source freezes,
raw evidence, packages, targets and frozen binaries remain intact.

The actual owner v1 compile report is retained in `actual-consumer-before/`.
Its three unused-getter warning groups identify all five methods; that actual
compile also failed for independent caller/type errors. Its early leaf was
`9119e887...`, so this is not a claim that deleting getters alone repairs the
whole graph. No actual owner graph was rebuilt or modified by this implementer.

New normally admitted GNU compile-only scope:
`/home/adalundhe/native-execution-getter-7878-compile-v1`, runner
`451397fcc585f7f7cb9aaf00e42064bb`, two CPUs, requested 2 GiB memory/4 GiB disk,
600-second absolute batch and cargo j2. Build and Clippy exited 0; targeted local
format and policy checks exited 0. All **26 Rust projection unused warnings**
remain visible. Clippy denies all Clippy lints, undocumented unsafe and the
repository's panic-related lints; it is not a full-graph `-D warnings` claim.
Runtime cases **0**. The prior seven passes remain attached only to their prior
source and were not replayed for getter removal.

`PACKAGE.tar.gz`: 25,919 bytes, SHA-256
`267327de705a74c3504b595ff6d38e134edd3a3ba4cf0551aa421ff12828f599`.
Source manifest SHA-256:
`c3191ee0f11ed7f7be70e091d48ffebb2391f2c0cabcbe6f2f12d90825de07e9`.
Compiled **unexecuted** test binary: 8,320,616 bytes, SHA-256
`2b9bdc3100676ec12515337b5e600c0e6e298dacaa844ae8b0ff55bf8348802e`.
Remote original sources, target and binary are retained.

`raw-compile.tar.gz`: 8,236 bytes, SHA-256
`38831482f6a80abc61c8fd03594b8ccaffa5105a46c2744b796a60d1274efb9b`.
All 14 payloads plus the original manifest were verified; full readable logs
are retained in `raw-compile/`, including all projection warnings. The source
manifest was checked before and after compile. `COMPILE-RECEIPT.json` retains
the full stage commands/results. In a fresh admitted private Linux scope,
extract the exact package and run `python3 run.py`; it has no runtime-test stage.

Policy/role receipt is `POLICY_READ.json`: current origin/main
`3830327b6c8d7ac22452a5e32ffa32799263132c`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`,
implementer, same owned leaf/session directory. Full current policy, evidence,
handoff and updated authority decision were read before this batch. No child,
local build, cgroup write, VM, profile, heavy run, main merge or peer source edit.

R01/R03/R04/R07/R12: runtime bodies, state and literals are unchanged; panic/
unsafe lints remain denied. R02/R05/R06/R13: the actual consumer and its retained
compiler diagnostics justify deleting unused interface; no new architecture.
R09/R10: exact actual before diagnostics and source-specific compile after logs
are retained; no legacy kernel defect or whole-graph green is invented.
R08/R14: no performance/profile claim. R11: GNU amd64 compile-only; seven targets
and real VM/kernel behavior remain unverified. R15/R16: scoped light checks,
fresh normal admission, unchanged old evidence and no cleanup/bypass. The full
R01–R16 account and unchanged kernel/ownership limits remain in `../README.md`.

Actual current-owner/admission/NotBorn or matching reap plus kernel empty,
audit/filter/clone/pidfd/ACK, platform/VM and cost gates remain ROOT/runtime-owned.
The shared parent v2 refactor is unchanged. This checkpoint supplies the smaller
API and necessary typed compilation proof, not the remaining integration result.
