# Exact light results

Source/test base: `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, branch
`p/native-otel-reader-tests-v1`, new worktree `managed-otel-reader-tests`.
Reader is ROOT's frozen uncommitted `8f35e059...`, codec is signed `11d1c04d...` /
`9aa16e1d...`, test leaf is `5925769c...`; full digests and limits are in README.
This is a normal unit-test checkpoint, not an adversarial or collector campaign.

## Native Linux light gate: r100, exit 0

Runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora-2,
Linux `6.19.10-300.fc44.x86_64`, native `x86_64-unknown-linux-gnu`.
Rust `1.98.1` (`48a229ceaefd4985c50990b14116b6d856af0985`), LLVM 22.1.8,
Cargo 1.98.1. Runtime `getconf PAGESIZE` returned 4096; the fixture queries that
fact itself using sysconf and does not embed 4096.

Exact caller:

```sh
RUSTUP_TOOLCHAIN=1.98.1 python3 "$HOME/native-managed-otel-reader-33df-v2/run-light.py" \
  "$HOME/native-managed-otel-reader-33df-v2"
```

Kiln admission was `linux/amd64 cpus=2 mem=1g disk=2g`, `sync:false`,
`reuse:false`, outer timeout 300 seconds. At upload the guard reported 24 idle
cores and 46 GB free memory; actual filesystem free bytes were 1,915,830,706,176.
These are headroom observations, not resource peak measurements. All source,
Cargo home, target and temp/cache outputs are in the owned NEW scope above.
Protected root worktrees/caches, paused WinARM and old proof worktrees were not used.

| Command | Actual result | Raw |
| --- | --- | --- |
| rustc -Vv / cargo -V / uname -a / getconf PAGESIZE | all 0 | `raw/00.log`–`03.log` |
| cargo fmt --check | 0, empty log | `raw/04.log` |
| cargo test --lib reader_tests:: -- --test-threads=1 | **10 passed, 0 failed, 0 ignored, 10 filtered out** | `raw/05.log` |
| cargo clippy --lib --tests -- -D warnings | 0 | `raw/06.log` |

The ten filtered tests belong to the real encoder; they were compiled but not
run here. Encoder results from its separate task are not reused as this gate.
The 24 actual snapshots returned their asserted typed outcomes, preserved scope,
and each fixture asserted directory removal. Final TMPDIR listing contained only
the empty `tmp/shards-tests` parent. All nine copied source/config hashes remained
unchanged. No deadline fired or cleanup wait was needed. File close results,
FD/handle counts, allocations, peaks, leak freedom and full kernel resource return
were not measured.

The operational caller completed in 2.344346485 seconds; test command wall time
was 1.618312218 seconds including compile and the libtest result printed 0.01s.
These are execution receipts, not benchmark data or performance claims.
The final recursive sum of owned regular-file sizes was 36,926,876 bytes; it is
not allocated disk usage, a process/kernel memory measure or a quota proof.

Frozen actual test binary retained remotely:

`target/debug/deps/managed_otel_reader_light-aa7737090c42b055`, 8,496,288 bytes,
SHA-256 `b822bb96858eac8ee41eaef5987526cee55c931f60e55bc1a547748b1134c513`.

## Retained raw source and output

`raw-storage/fedora-r100.tar.gz`, 26,792 bytes, SHA-256
`c7f33e3eed4580ff7833deea5518f95cfa342245f363677725599ecc2f7cce76`.
All 13 unique regular members passed safe-path checks; all 12 original entries
in `evidence-manifest.json` passed exact size/SHA-256 checks after download.
This includes unchanged raw command outputs, summary, source map, Cargo.lock,
actual caller and frozen input archive. No raw output was reformatted.

The nested `inputs.tar.gz` is 19,904 bytes, SHA-256
`4ee9efcc7bd2d8084166bba2e6ebb99f45125760eaa15d39790361f62044958c`.
It retains the actual reader/codec/test/helper/config bytes and minimal wrapper.
Reader source is copied byte-for-byte, not an alternative implementation.
The unchanged source map preserves the intermediate local snapshot paths;
the original ROOT origin was captured by the first input archive below.

## Earlier actual red and admission holds

The first attempt on `451397fcc585f7f7cb9aaf00e42064bb` used the same source/test
bytes but omitted canonical `rustfmt.toml` in my private harness. Its
`cargo fmt --check` returned 1 and stopped before compile/test/Clippy. Default
formatter width caused the diffs; this is a packaging mistake, not a reproduced
production parser defect. Copying canonical config fixed the next check while
all production/test source hashes remained unchanged.

Its frozen input is `raw-storage/adacachy-v1-inputs.tar.gz`, 19,801 bytes, SHA-256
`29994449f59b6d2cd73cade0c703d409bcdb81a7de85e4b5a854b1ba670b9a7f`.
Original raw remains in
`451397...:~/native-managed-otel-reader-33df-v1/raw/`; the actual observed
`04.log` SHA-256 is
`dab84718f4b649f83a5318a72ae6f8858f7eb4538451ca17682c429c876c641b`.
That remote raw has not been copied into this checkpoint: subsequent admission
reported 0 idle cores and was respected. The original raw/source is preserved.
The separate partial `~/native-managed-otel-reader-33df-v2` on that runner contains
only the first 12,000-byte upload part; no check ran there. The successful v2 is
on Fedora and is a distinct owned directory on that machine.

Local light checks: test leaf rustfmt under `lockf`, Python AST parse of both
caller scripts, `scripts/check-development-policy` and whitespace checks passed.
Mac filesystem free space was about 208 GiB at the final receipt; no local Cargo
build, deletion or heavy job ran.

## Owed coordinator work

ROOT must compose the test registration with its final reader and selected codec,
then validate the normal guest/init caller, full supported targets, real held
cgroup/controller effects and real collector decoding. Independent audits,
profiles/allocations/faults, equivalent baseline comparisons and full managed-plan
gates remain ROOT-owned and unrun here. The regular-file pass does not imply any
of those gates or a live-kernel defect fix.
