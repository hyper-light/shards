# Actual migration result

The ordinary `code_mode_runs_a_program_with_the_callers_grants` test **passed0**
in actual KVM r86: one passed, zero failed, zero ignored,137 filtered. No outer
timeout or signal. Native duration3.56s is an operational run fact, not a
performance comparison. Production remained exact
`0219ab2eb2466ee3e9b08064434dc9efd70053d5`; tested source changes are signed
checkpoint `f408e66ea941e2405903db9ac0701f20849682ea`'s three frozen test files.
No assertion was removed, test skipped, transport duplicated or product output
rewriting restored.

The root's actual earlier3b red remains the baseline: its original kernel-ready
predicate passed, then the first prefixed-main-stderr expectation failed. This
new case validates all outcomes on0219; it is not a production before/after fix
or a new independent reproduction of that earlier revision.
At closing, main33df95b0's retained identity, exit101 and log were read through
Git; log SHA256 `72805b3e49b1b12cd829ac9e2810df5ee3344a77d632f2a0be80f922cc9e512c`.
No root runtime cache was accessed to obtain it.

## Actual effects and cleanup

The three original programs/grants and expectation literals are unchanged.
Bracketed expectation labels now select their checked member/channel, rather
than demand rewritten main-stderr bytes. The actual records contain:

- Agent A: own identity/peer rights, successful send id1, denied send to agent C.
- Agent B: independent actual inbox request from agent A, id1, exact text
  `from agent a`.
- Agent C: all five globals undefined; TypeError boom, SyntaxError invalid
  property, RangeError stack exhaustion;300ms timeout; a different session's
  cancellation leaves the finite worker's `finished` result; its own session
  returns code-32800 cancellation; abandonment reports the client connection drop.

All original exact/prefix assertions passed on actual canonical fd1 data.
The denied C send is the original sender refusal assertion, not a new kernel
packet-absence witness at C. The different cancellation session belongs to the
same tenant, not another principal.

Six generation1 authenticated subscriptions produced these receipt cuts:

| Member/channel | Payload bytes | Wire ACK enqueued | ABI state |
| --- | ---: | ---: | --- |
| 0/agent a fd1 | 390 | 685 | Open offset390 |
| 0/agent a fd2 | 0 | 23 | Open offset0 |
| 1/agent b fd1 | 324 | 571 | Open offset324 |
| 1/agent b fd2 | 0 | 23 | Open offset0 |
| 2/agent c fd1 | 1151 | 1526 | Open offset1151 |
| 2/agent c fd2 | 0 | 23 | Open offset0 |

Wire records are actual File/QUIC/ABI effects, including Open records for empty
stderr. No canonical End or future-zero stderr is inferred. These envelopes
authenticate inherited tenant/channel bytes, not semantic tool/PID provenance.
Actual controlled-client RPC reports remain distinct from supervisor audit facts.

The independent guest kernel snapshot showed exactly3 confined-ready actors,
3 servers,3 sandbox helpers and0 program workers, with main caught mask0x640.
Its owner SIGUSR1 succeeded; actual main wait returned exit0. Explicit removal
succeeded and the private metadata path was absent. Before removal, all six
metadata records were aborted/tail unknown with accepted=consumed equal to
the listed wire offsets. Program completion is not actor EOF: these actors
remain live until main/run ownership ends.

Owner daemon stop succeeded and all six observer joins returned no errors.
The post-case owned cwd/executable/argv kernel process snapshot matched no
remaining processes. No host FD count was collected in this migration; do not
carry the separate broker case's8→8 value into it. Full population/thread history,
wait statuses and all kernel objects/charges returned remain unverified.

## Source and native artifacts

`move-check.json` passes: identical moved bodies after only pub(super)/final-EOF
normalization; identical parent Fixture/payload/tests suffix; identical build.rs
bytes outside the named function. The block-local external module and narrow
visibility compiled on pinned Rust1.98.1 in a metadata-only language probe; that
probe has no producer, VM, network or tool effects.

r83 normal full carried VM/net/init/server and both integration test consumers
compiled0 in10m02s. r85 both-consumer Clippy0 in1m45s; actual static guest build0
in41.01s; pinned kernel SHA verified; original metadata parser pure test1/0/0.
No interruption or failed native attempt occurred in this batch.
The source hashes are unchanged from f408 and independently checked by the
native caller against the compilation inputs. Actual selected artifact SHA256:

| Artifact | Bytes | SHA256 |
| --- | ---: | --- |
| Legacy build test | 103342224 | `cbc81f550374b02d74c912a2da8b404723f49b796147ec1654800b96f4b18c2f` |
| Managed-output test | 60381768 | `a5e25dec6a39242cb1eb8004bf1b9ab21631544c7c757d72c0981935d1c41049` |
| Main CLI | 270585008 | `fb299fb871dda253d48e25497e582f2e6b4b7170068ed3f30b9245a6193eb8bd` |
| Static testguest | 3673760 | `09d1ed31a6893f5b6500f4020b2233dc4f187f85973ad9b3d5a317e537f20c5b` |
| Pinned guest kernel | 27758808 | `48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7` |
| Actual runtime-cache VM | 30934048 | `8f617ed7bb881673a57cb3eca2f7310d49410a24c6d41699784c87cf352646dc` |
| Actual runtime-cache network | 7908800 | `01a8a7cc4a9717bbfe132278cfc80286092d42f8d85aadbf60f9eea743bf12fe` |

The artifact manifest includes both build/Clippy output contexts; not every
listed file is claimed used. The runtime cache matches the normal carried
helpers above. The caller pins the actual static guest and kernel and removes
a VM-binary override. Its SHA256 is
`1efede7fd1481a3df2753b36021fa0df328e00785309c5572c3eca36f6d140f1`.

[`raw-linux.tar.gz`](raw-linux.tar.gz),35049bytes, SHA256
`cfe54c29e246da56a55e573833ef735c697ba1100f42efc7a2ba985eb33e3cbf`,
retains112 regular members: all111 manifest sizes/hashes and safe/unique paths
verified locally. It includes original source patch, language probe, toolchain,
raw build/lint/pure logs, source/binary inputs, raw canonical/payload bytes,
metadata, actual CLI/process/result/cleanup records. `raw-manifest.json` indexes
original bytes; `decoded-selection.json` provides a readable structured subset.
Submitted build/preparation/native/retention commands remain alongside it.

Host: Fedora Linux6.19.10-300.fc44.x86_64 GNU, Rust1.98.1 `48a229cea`,
zig0.16.0; actual static musl guest on KVM. New owned scope is
`~/native-code-mode-capture-0219-v1` on Fedora8fadc. All source/target/Cargo/Zig/
XDG/Shards caches were private from the first build; all actors/checks completed.
Its target was2.7GiB, source606MiB, Cargo151MiB, Zig local21MiB, evidence38MiB
including runtime helper binaries. No original source/evidence/cache was deleted.
Old3ead/7bca and Windows/terminal references/artifacts remain preserved.

Fresh guard admitted2CPU/4GiB/24GiB; actual r86 admission reported29 idle cores
and47GB free. Admission is not a hard cgroup/peak measurement. One actual case,
original120s work plus at most15s cleanup, outer240s+one15s reap/provider300s;
normal compilation<=3600s and preparation within the remaining original budget.
No polling/retry/cache workaround, profiling, benchmark, stress or sweep ran.

## Remaining coordinator work

R01 test assertions only, unchanged production and scoped Clippy0; universal
panic freedom is not proved. R02/R03 primary alternatives/inherited/test bounds
are documented. R04/R05 actual typed state/transport/parser and explicit work/
cleanup, one authoritative observer, no duplicate state owner; frozen helper
waits disclosed. R06/R07/R13 exact focused move and one consumer migration.
R08/R14 no comparisons/profiles;4–5× target remains root-owned and unproven.
R09/R10 actual root legacy red and this source-specific real green/raw effects
retained without inventing a production fix or guard mutation. R11/R12 only GNU
Linux amd64 KVM/musl guest validated here, shared implementation retained; the
new combined seven-target/HVF/WHP/other-libc/arch gates remain root-owed.
R15 signed scoped implementation and one actual case; root audits/integrates/
reviews/mutates/final-merges. R16 guarded owned paths/data preserved, no heavy
local build, protected root cache, paused WinARM, privilege/security/crypto change,
force/reset/stash/destructive Git or main merge.

Root still owes combined revision validation and broader management work:
resource/population/kernel accounting, failure/pressure/concurrent/cross-principal
campaigns and meaningful guard mutations, required durable pre-effect audit,
per-execution provenance/native adapters and complete compatibility/performance.
Passing this migrated test does not close P7 or the managed plan by implication.
