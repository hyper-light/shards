# R10 harness relocation and separate current-path light check

This is the narrow relocation after signed research
`a41ab4a2c4f65360016263ef30aaa517d4388180`, on the same owned branch
`p/managed-otel-memory-limit-v1`. The original SAME28-file directory moved:

`docs/measurements/managed-otel-limits` →
`docs/research/measurements/managed-otel-limits`.

The latter is R10's harness location. No production Reader, tests, Snapshot,
codec, run, daemon, field, dependency or resource policy changed. ROOT retains
review/adoption/wiring, actual whole-caller gates and main merge; typed Reader
availability is still not exported-limit metric completion.

POLICY_READ: implementer `01a1c5be192972d8b05dc7c207cc6fca`, coordinator
`01a148884a4770fdaf0111f3f792a248`; current origin/main
`b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe`, FULL AGENTS/R01–R16/evidence/
current handoff and its required combined0ae closing record read. Current b57
plan fully read; CLAUDE unchanged since the full read. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only this relocation, its live paths/required document links and separate
relocation evidence are owned. No children/goals/profiles/audits/VM/main merge.

## Historical source association, unchanged bytes

Before `gits mv`, every one of28 files was checked byte-equal to a41. The
[source association](records/relocation/source-association.json) records each
original and relocated pathname, length/SHA256 and role. **All21 pre-existing
historical records are byte-identical**, including raw archives, source archives,
commands, launch/results, guard refusals, original formatter failure and timing
text. Their contents keep the original source/execution paths. No old result
is claimed to have executed from the new path.

Only four original files changed beyond their Git pathname:

- Cargo.toml: one additional `../` for the private crate-root include.
- testdir/Cargo.toml: one additional `../` for the actual testdir source include.
- run-light.py: live manifest/source leaf gains `research/`.
- README.md: repaired live OTel-report link and an explicit historical/relocation note.

The later owned `managed-otel-limits-next/README.md` repairs only two links:
its historical original driver now names immutable022 source, and its preceding
record link follows this directory. Its historical command/source/result text
and every raw record remain unchanged. Old paths in README/HANDOFF or archives
are intentional source-history association, not remaining live manifest paths.
Historical source archives can still be replayed with their original commands
inside a new private extraction of those archives; that would reproduce their
historical source, not the relocated current source.

Manifest roles were read from the actual embedded manifests, which exclude
themselves. An outside inventory may also list the manifest row:

| Historical archive | Original payloads | Manifest | Total inventory members |
| --- | ---: | ---: | ---: |
| First fmt-before-compile failure | 19 | 1 | 20 |
| Original first passing memory Reader | 35 | 1 | 36 |
| Later shared memory/PID Reader | 35 | 1 | 36 |

There are no36-original-payload claims. The association verifies unique safe
relative regular members and each manifest-covered payload's length/hash.

## New current-source path validation

Actual runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora GNU x86_64,
kernel6.19.10-300.fc44.x86_64/glibc2.43. NEW private root
`/home/adalundhe/managed-otel-limits-relocate-a41-light-v1/{source,target,cargo-home,tmp,out}`.
Normal admitted/requested envelope **2CPU / 2GiB memory / 4GiB disk / <=600s / j2**,
sync:false, unchanged. Admission is not a measured peak or an independently
proved kernel resource limit. No local build/target/cache or ROOT path was used.

Actual format0, format-check0, native light build0, **15 Reader tests passed /
0 failed / 0 ignored**, strict all-target light Clippy0. Ten unrelated codec
tests were filtered, not passed. This current-path attempt had no failed test or
compile; historical failure/guard distinctions above stay unchanged. The one
private TMPDIR had no remaining files/symlinks. No FD/process/kernel-population
or whole-operation leak claim follows from that regular-file cleanup witness.

Actual rustc1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`, Cargo1.98.1
`797e8a9bc`, rustfmt1.9.0-stable/Clippy0.1.98. These are observed tools, not an
enforced toolchain pin. Existing rustfmt max_width110 was included. Exact argv,
environment, tools, source/format/binary outcomes are in
[result.json](records/relocation/light-v1/result.json).

| Current source/artifact | Bytes | SHA256 |
| --- | ---: | --- |
| Unchanged actual Reader | 10207 | `f023a1aaa04597c3a9bf4094557a6e6545b670769421f12e27571ff2450673f9` |
| Unchanged actual tests | 13345 | `00bdb503602f042dc2a50b1eed7d980d38ed0f94709d53fe36d1109fbea3e244` |
| Live relocated driver | 7160 | `76bd0d53345e37c5316ec5e8878f997eaaf7c148cfd3e2f6012cc6c7dacac959` |
| New source closure13 files | 24456 | `44cd64e08556c4cdd77f1c3a822eb22e5a55c8e0a85fdcd9cae121a5da86e3bd` |
| Actual current-path native binary | 7587608 | `1a39f99b646dcf2803e02d87900ded56f7faa2d91e1ce7631fc1cefc82785382` |
| New raw archive | 12295 | `19a30f64d6d2f0b97cba834a66d695d89fabaaaf82c158016e23caf3dfe162ef` |

Binary: private root's
`target/debug/deps/shards_managed_otel_limits_light-3c25f2e21fff10a4`, hashed before
execution and after Clippy with equal identities. New raw has **35 original
payloads +1 manifest =36 inventory members**; all members/roles/hashes are in
[archive.json](records/relocation/light-v1/archive.json). Separate read-only
1CPU/256MiB/1GiB<=600s transfer0 performed no compilation or test.

To reproduce this **new source**, extract its new retained source.tar.gz into a
fresh private root's `source/`, verifying its package's safe regular members and
hashes; from that private root run:

```text
python3 source/docs/research/measurements/managed-otel-limits/run-light.py
```

The literal [launch](records/relocation/light-v1/launch.json) runs only this driver
under the stated admission. Current Cargo commands use
`--manifest-path docs/research/measurements/managed-otel-limits/Cargo.toml`:
build `test --lib --no-run --message-format=json -j2`, the one exact test binary
with `metrics::tests:: --test-threads=1`, and
`clippy --locked --all-targets -j2 -- -D warnings`. No broad/repeated tests were
added after the passing path check. Both private source copies and all old/new
targets/binaries/raw evidence remain preserved; no cache reclamation occurred.

## R01–R16 and remaining work

| Rules | Proof / boundary |
| --- | --- |
| R01/R04/R06/R07/R13 | Only proof-harness path correction and historical/live document distinction; no production state, panic/allocation path or parser change. Actual normal light format/build/corpus/strict Clippy pass. |
| R02/R03/R05/R12 | Direct directory-depth derivation and explicit R10 instruction; no new runtime literal, polling/cache, abstraction, dependency or platform implementation. |
| R08/R09/R14 | No performance claim, profile or new production-defect assertion. Original failures/timings preserved, not relabelled; new compile/test checks only relocated path correctness. |
| R10 | Actual Git rename, before/after source association, byte-equal historical records, manifest roles and distinct new source/commands/binary/raw identities. No old path rerun claimed. |
| R11 | GNU x86_64 light only; full carried init/targets/platform/kernel/Collector checks remain ROOT-owned and unrun here. macOS ARM64 only; paused WinARM untouched. |
| R15/R16 | Implementer-only normal admitted private scopes, no children/goals/audit/VM/heavy/main merge or ROOT cache writes/deletion. Signed named-path gits checkpoint with required attribution; originals preserved. |

Small development-policy/whitespace checks accompany the signed return. ROOT
still owes review/integration and actual pending limit-export decision/wiring,
whole-caller GNU/musl/target/identity/controller/cleanup/consumer/guard gates and
all measurements. The historical README/HANDOFF are not the current queue:
this relocation check is finished, and the lane remains assigned for ROOT's next
finite implementation. Full P0–P13/G25/parent guarantees remain open.
