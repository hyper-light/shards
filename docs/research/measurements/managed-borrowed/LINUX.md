# Exact current Linux GNU/musl light checks and recovered evidence

Kiln r192 completed1 at exact clean
`eaedd0b0fe39662b9216a286d83e0e984c3cf2d3` (production53535ab6), with no production
changes. One Linux4CPU/8GiB/20GiB admission on
`8fadc1ec213332e16cb0d915a3fb913f` was accepted. It used only the owned synced source
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-borrowed-c5f6175fcc`
and `~/managed-borrowed-target`; root capture/RPK sources and targets were untouched.
The driver archived previous evidence first, touched owned source/harness inputs,
and selected separate explicit GNU/musl target directories. Compiler artifact
records show `fresh:false` for every selected actual executable.

Environment: Fedora Linux6.19.10 amd64, Rust1.98.1 `48a229cea`, LLVM22.1.8.
Both requested standard libraries were already installed; no install was attempted.
[Source](linux-eaedd0b0/source.txt), [clean status](linux-eaedd0b0/status.txt),
[source hashes](linux-eaedd0b0/source.sha256), [toolchain](linux-eaedd0b0/rustc.txt),
[installed targets](linux-eaedd0b0/installed-targets.txt).

| Native target | Actual result |
| --- | --- |
| x86_64 GNU | Build0; API units48 pass/0fail/7ignored; integration14 pass/1fail/2ignored; strict API+afunix clippy0; corpus exit101 |
| x86_64 musl | Build0; API units48 pass/0fail/7ignored; integration15 pass/0fail/2ignored; strict API+afunix clippy0; corpus0 |

`shards-afunix` is cfg(windows), so its Linux test target executes0 tests; this is
not Windows adapter coverage. The existing real-VM measurement is ignored on
both runs; no VM was started. The ignored historical reds, child roles and
measurements remain explicit in each log. No six fault mutations were rerun.

## GNU failure preserved, not classified as noise

`client::a_child_holding_shards_output_holds_no_refusal_back` failed at
`crates/api/tests/sim.rs:431`, assertion `took < REFUSED_AFTER_END.get()`:
`refused 1.348517ms after shards' end`. The preceding refusal match accepted
`Refusal::Boot` containing the expected `no kernel at kernel`. The actual threshold
was not printed. No scheduler/load classification, bound increase, rerun or fix
was made. Root received the exact case/assertion/log before any further action.
Root's subsequent source review identifies a single-host Linux299.915µs recorded
maximum behind this learned bound. This timing assertion after correct refusal
does not causally prove the inherited writer blocked refusal. A controlled
writer-held/event-based fixture proposal is separate next work; no threshold is
enlarged/relearned and the original14/1 result stays RED.

[GNU corpus raw log](linux-eaedd0b0/gnu-corpus.log),
[GNU native exit](linux-eaedd0b0/gnu-corpus.exit),
[musl corpus](linux-eaedd0b0/musl-corpus.log).
The successful borrowed payload case records Linux FD/task counts `(9,2)→(9,2)`;
that one assertion is not repeated leak/resource accounting. Musl success does not
turn the GNU corpus green, and neither result clears the Mac full-library RED.

## Exact binaries and replay

Selected actual compiler outputs and SHA256 values are in
[GNU binaries](linux-eaedd0b0/gnu-binaries.json) and
[musl binaries](linux-eaedd0b0/musl-binaries.json). Principal binaries:

| Artifact | SHA256 |
| --- | --- |
| GNU API units `shards_api-780ac76e90e497e3` |0aa48c4747cf3e4cf960829bee149745e6d52da8a61f276983384d74848ccb06 |
| GNU integration `sim-71a7738039f7ec17` |eff208e8f75ca9aa45c84b4e43a3c786bbf0748d1056fa49e79e60cfa58a6ebc |
| musl API units `shards_api-16640ff8e275a4d5` |3db770ef70456f9d411af47556db43c0a8c95b6e87312568f5a362ffce0fee67 |
| musl integration `sim-4f2b8fcd145a490b` |ba33ec4738232a5ec1d195c8ca75f35aaf5bb7a77901b9c6a3bb8418b8a5032f |

Paths are under `~/managed-borrowed-target/<target>/test-release/deps/`.
Each build/test/clippy command has its own `.command`, `.log` and `.exit` record.
[The exact driver](run-linux-light.sh) is retained. Its ordinary commands are:

```sh
export CARGO_TARGET_DIR="$HOME/managed-borrowed-target"
touch crates/api/src/*.rs crates/afunix/src/lib.rs docs/research/measurements/managed-borrowed/*.rs
cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix -- --test-threads=1 --nocapture
cargo clippy --locked -j4 --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix --all-targets -- -D warnings
# Same two commands with x86_64-unknown-linux-musl for the installed native musl target.
```

The complete raw archive SHA256 is
`fb969b03eeaa3d9249b066e4fc9c763f0dca9b746200de2adc98b8f5ddee6e76`:
[raw archive](linux-eaedd0b0/raw-archive.tgz). This is native amd64 GNU/musl
execution and cfg compilation, not the seven-target or real-VM gate.

## Previously owed raw records recovered without rerun

[linux-native-history](linux-native-history/) now durably retains:

- r176 `preimplementation/`: source9a29652d, units33/0/6ignored, strict clippy0.
- r184 `ac8c2834/`: units45/0/7ignored, integration15/0/2ignored, strict clippy0.
- r185 `guards/`: all six exact mutation patches, source hashes, actual forbidden
  CONNECT/returned-stream/late-OK/allocator-abort logs, each restored test and
  restored corpus47/0/7ignored, clippy0 and actual binary hash.
- r186 `queued-timeout-red*`: source389e823e and actual WouldBlock failure despite
  queued bytes. Later535 correction has separate current unit/stream proof.
- Earlier extraction/lock/compile/clippy setup failures and the r171 original reds,
  retained without being presented as successful or latest-source gates.

Original archive SHA256
`95643e65f7e33eb6109d6c1cb9562b28e663070f1db148ce29d723dd6936f9e0`:
[history archive](linux-native-history/raw-archive.tgz). Those historical results
remain attributed to their own sources; retrieval did not rerun them.

## Policy, migration and owed work

Dispatch readback: canonical c921a0b1 / rules SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d` after full
AGENTS/R01–R16/evidence/CLAUDE/plan/handoff read. While r192 was accepted, root
published current main `f5cec365d2e435afae5e6f613c92d9d3759ef60f`, SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Implementer read its full AGENTS/rules/evidence and renewed receipt before further
edits. The accepted job was preserved and source was not rebased solely for policy.
Root owns all audits/profiles/benchmarks/heavyE2E/chaos/stress/sweeps; this lane ran
only assigned ordinary unit/light integration/cfg checks and evidence retrieval.

[D215 migration note](D215-MIGRATION.md) is owned evidence for root to compose into
architecture text; this batch does not edit the architecture document.

R01: targeted allocation/error checks pass here; Mac owned-OOM64B remains real RED.
R02/R03/R05/R12: no production decision, constant, wait/cache or platform seam
change. R04/R06/R07/R13: production source unchanged, migration semantics explicit.
R08/R14: no performance/comparative claim or profiling. R09: GNU failure recorded
before fix/rerun; separate non-UTF8 comm suspicion is not reproduced or changed
in this checkpoint. R10: exact raw/new/historical evidence retained. R11: native
amd64 GNU/musl only, no matrix/VM substitute. R15: one accepted unchanged-envelope
light dispatch, no children/main merge; original and renewed policy receipts
preserved. R16: private paths, no unrelated cleanup, preserved evidence.

Remaining root work: investigate GNU refusal-bound failure; preserve/resolve full
Mac owned-OOM RED without warming; Windowsarm64/Linuxarm64 and exact combined
seven-target/real-VM checks; repeated native handle/resource gates; adoption and
architecture composition. No ready-main or whole-library panic-free claim.
