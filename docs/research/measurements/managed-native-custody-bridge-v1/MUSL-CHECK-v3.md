# musl compile of the composed init tree (GREEN-v3), runner 451397

Tree: r269 scope root-custody-report-green-v3-1794430198083/execution/source (restored GREEN-v3;
FROZEN-SOURCE b0c64aed…, run.rs aae543a1…), its private offline CARGO_HOME, separate target-musl.
- `cargo check --locked --offline -p shards-init --all-targets --target x86_64-unknown-linux-musl -j2`: exit 0
- same for `aarch64-unknown-linux-musl`: exit 0
  (warnings only: the known unconsumed input/RAW/owner_input dead code)
- `cargo clippy ... --target x86_64-unknown-linux-musl -- -D warnings`: exit 101, 15 bin + 11 bin-test
  errors (same counts as the GNU r8/r182/r269 strict baseline: unconsumed input/RAW APIs).
r87/r182/r269 themselves compiled GNU only; this closes the musl compile gap for this tree.
