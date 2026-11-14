# Runs review fixes, 2026-11-13

Implementer: Codex, p/fix-runs-v1. Main coordinator: ROOT. Policy at base
9efde72524dd626f3ff84e6b9891852d7c810a9b: AGENTS SHA256
f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41;
rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
evidence 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63;
CLAUDE 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010.
Read both /tmp/g121-shot/review/fix-brief.md and /tmp/g121-shot/brief-common.md.
No children. All cargo commands run through kiln on adas-mac-studio.local,
macOS arm64, in ~/g121-tmp/fix-runs-v1/source, with CARGO_INCREMENTAL=0,
CARGO_TARGET_DIR=../target (absolute), SHARDS_HELPERS=skip, one cargo at a time.
Local work is edits/format/git only. The tiny carrier lives at
/tmp/g121-shot/fix-runs-carrier; its patch is applied to the recorded revision.

## runs-01

Source: base above plus the server owner-metadata patch in the fixing commit.
`cargo test -j 4 -p shards-vmm --lib owner_metadata_failures_refuse_creation_and_lookup`
went red in kiln r4: injected owner-write EIO was acknowledged 0, expected -5.
[Red log](runs-01-red.log). The same test passed in r7 after correction;
`cargo test -j 4 -p shards-vmm --lib devices::virtio::fs::server::tests` passed
49 tests and `cargo clippy -j 4 -p shards-vmm --all-targets -- -D warnings`
passed. [Green log](runs-01-green.log). An intermediate module check (r6)
exposed symlink ELOOP and pinned-unlinked ENOENT regressions; the correction
keeps native special nodes root-owned and reads pinned metadata through its fd.
The final module gate includes these preexisting regressions and passes them.
No guard-removal mutation or live VM was run.

Owner syscall fault injection is test-only, thread-local and restored by a guard.
Production uses existing typed FUSE errno outcomes; absent metadata alone permits
run-owner fallback. Missing/failed opens, corrupt metadata and failed owner writes
do not silently grant ownership. Creation failure removes the new file/directory;
cleanup failure propagates its errno. No new runtime numbers, cache, wait or
platform adapter. Existing platform xattr calls carry the same test hooks.

R01/R04/R06/R07/R13: scoped source review and focused tests/clippy above; no new
production panics or string parsing. R02/R03/R05/R12: existing FUSE/xattr boundary,
no broader architecture, learned values or speculative manager introduced. R09/R10:
actual unit red/green retained. R08/R14: no performance claim or local profiling;
ROOT owns it. R11: one macOS arm64 unit host; Linux/Windows CI and guest effects
unverified. R15: signed ff land after main merge; no main checkout or force push.
R16: own target directory removed after the lane's final check, no shared cache
or user data removed. The narrow brief prohibits local tests and broad gates.

Owed ROOT: `cargo test -p shards --test volumes a_binds_host_files_are_its_runs_users`
on supported real-VM hosts, plus a real bind xattr-failure permission/cleanup
campaign; seven-target CI and `scripts/check-development-policy`. Existing live
case does not replace the fault campaign. No performance/portability pass inferred.
