# Local memory and task ceilings through one Reader

This checkpoint extends the reviewed memory-limit Reader with local `pids.max`
reading. Both real getters return `Limit::{Finite(u64), Unbounded}` or existing
`Failure`, through one private parser. **Neither getter is wired into Snapshot
or export here. This is not exported-metric completion.** ROOT owns that wiring
and actual combined caller validation. A configured cap is not a guaranteed
entitlement, reservation, effective ancestor limit or available capacity.

Parent: signed/pushed `02272096c758a2df31691112a4c1b8b83b8c601c`, same branch
`p/managed-otel-memory-limit-v1`; no rebase of prior proofs. Current fetched
origin/main policy revision is `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`, rules
SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Implementer `01a1c5be192972d8b05dc7c207cc6fca`, coordinator
`01a148884a4770fdaf0111f3f792a248`. FULL current AGENTS/R01–R16/evidence/global
handoff read, with previously fully read CLAUDE/plan verified byte-identical.
[Readback](records/policy-read.json) records exact hashes and owned paths.
Only Reader type/helpers, existing Reader tests, and this new directory changed.
Snapshot, otlp.rs, run, daemon and all ROOT fields remain unchanged.

## Primary contract and implementation choice

The existing OTel research pins Linux **v6.18.48**, commit
`5bbb9c9f8f808710e2123f2b30f0d61d7d698f52`. Its
[PID controller documentation](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst#pid)
defines task/TID counting, including threads, and non-root `pids.max`. Lowering a
limit or migrating tasks can leave current usage above it; fork/clone admission
checks still apply. The observed local file is therefore neither a process-leader
count nor a guarantee that another task can start.

In pinned [kernel/cgroup/pids.c](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/kernel/cgroup/pids.c#L156),
`pids_try_charge` checks the hierarchy, and `pids_can_fork` charges one task.
[`pids_max_show`](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/kernel/cgroup/pids.c#L311)
renders a finite decimal or exact `max`. The numeric internal sentinel and the
write-side accepted range are kernel implementation details, not new Reader
sentinels. Full-u64 fixture values establish parser width; this kernel does not
accept or emit every u64 as a finite PID limit. The Reader does not write limits.

The shared finite/max grammar is also prescribed by the kernel's
[interface conventions](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst#conventions).
Primary source URLs, retrieved byte sizes/hashes and inspected sections are in
[source identities](records/research/primary-source-identities.json). The two
new semantic pieces are the fixed `pids.max` control name and its task meaning;
`max`, unsigned parsing, token bounds and page allocation were already present.

Rather than duplicate the existing finite/max fold, `MemoryLimit` becomes
`Limit`, and `memory_limit` / `task_limit` select their fixed names through
private `limit(&mut self, &File, &'static CStr)`. The public getters accept no
caller-supplied pathname. This uses the existing actual-page tokenizer, checked
number conversion and held-directory openat/O_NOFOLLOW ownership. No generic
public parser, alternate cgroup library, adapter, cache, polling or dependency
is introduced. Kernel PID-specific write-range validation is intentionally not
substituted for the unsigned observation grammar.

| Boundary for either actual getter | Result |
| --- | --- |
| One unsigned decimal | `Finite(value)`, with distinct finite zero and full u64 parser width. |
| Exact lowercase `max` | `Unbounded`, never finite zero or a cached/default value. |
| Empty or whitespace-only contents | Existing `Failure::Missing`. |
| Malformed or extra scalar tokens | Existing `Failure::Invalid`. |
| Numeric or selected-token overflow | Existing `Failure::Overflow`. |
| Missing file / fixed-name symlink | Actual Linux `Failure::Io(ENOENT)` / `Failure::Io(ELOOP)`. |

The two reads, usage reads and timestamp remain separate non-atomic operations.
`Unbounded` describes only this file; an ancestor may impose a finite cap. No
parent walk, remaining-capacity arithmetic, reservation or policy change occurs.

## Exact light result

Actual private runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora x86_64,
kernel `6.19.10-300.fc44.x86_64`, glibc2.43. NEW owned root
`/home/adalundhe/managed-otel-limits-next-022-light-v1/{source,target,cargo-home,tmp,out}`.
Normal headroom admission accepted **2CPU / 2GiB memory / 4GiB disk / <=600s / j2**,
sync:false. These are requested/admitted bounds, not measured peaks or a new
kernel-enforcement claim. Original source/targets/raw records were preserved.

The exact unchanged [original proof driver](https://github.com/hyper-light/shards/blob/02272096c758a2df31691112a4c1b8b83b8c601c/docs/measurements/managed-otel-limits/run-light.py),
test-only manifests, wrapper and lock from 02272096c were reused. The source
closure includes the actual Reader, unchanged codec and actual testdir source;
there is no second maintained implementation. A normal module wrapper is
generated only beside the Reader in the private source copy. No wrapper is
created under the repository's production directory.

Actual **format0 / format-check0 / native light build0 / 15 Reader tests passed /
0 failed / 0 ignored / strict all-target light Clippy0**. Ten codec tests in the
binary were explicitly filtered, not passed. The four earlier limit corpus tests
now exercise both real getters; one additional case exercises different values
in two simultaneous fixed names/held groups and recovery after an invalid task
limit. Finite/zero/full-width/max, empty/missing, malformed/duplicates/overflow,
symlinks, actual page crossings and shared buffer identity are covered.
The private TMPDIR had no remaining files/symlinks. Buffer identity is not an
allocation counter; that cleanup witness is not an FD/leak/kernel-population proof.

Actual rustc1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`, Cargo1.98.1
`797e8a9bc`, rustfmt1.9.0-stable and Clippy0.1.98. The compiler is an observed
tool, not an enforced repository pin; rust-version is an MSRV. Root max_width110
format config was included. [Result](records/light-v1/result.json) records exact
argv/environment/source/binary identities; [libtest output](records/light-v1/reader-tests.stdout)
and the full raw archive are retained without normalizing diagnostic text.

| Compiled identity | Bytes | SHA256 |
| --- | ---: | --- |
| metrics.rs | 10207 | `f023a1aaa04597c3a9bf4094557a6e6545b670769421f12e27571ff2450673f9` |
| metrics/tests.rs | 13345 | `00bdb503602f042dc2a50b1eed7d980d38ed0f94709d53fe36d1109fbea3e244` |
| Unchanged otlp.rs | 16634 | `0bb86b2f172e12b8053cf30333ccbabd1a6963a315836a639c79de5a4458f28c` |
| Unchanged proof Cargo.lock | 496 | `3c76154b7e5d87f0518803c46299b0187006bc9e9e020f7f6e9facefaa5dab73` |
| Native test executable | 7587576 | `124962faa6288d00f069c0fa0966eba846045d63553d4172c800438a21699def` |
| Raw archive | 12274 | `c05e6c27c0bf91069054fb10fd8ae05217fc255868fd7f8b718a8d4362243246` |

Executable: private root's
`target/debug/deps/shards_managed_otel_limits_light-3c25f2e21fff10a4`, hashed
before execution and after Clippy with equal identities. Pre-format source
closure13 files/24449B SHA256
`021eeb60b07497bc688287259b27cbb08891e41836e3d34cc7fd12e480b85903`;
formatter only adjusted owned test layout, whose compiled bytes were copied back
after identity verification. Source/launch/result/raw/member manifest and exact
pre/post format identities are under [records/light-v1](records/light-v1/).

Reproduce by extracting that retained source closure into a **new private** root's
`source/`, with verified regular member hashes, then run
`python3 source/docs/measurements/managed-otel-limits/run-light.py` from its private
parent under the same compile admission. The literal bootstrap is in launch.json.
It runs build `cargo test --lib --no-run --message-format=json -j2`, then the one
exact binary with `metrics::tests:: --test-threads=1`, then
`cargo clippy --locked --all-targets -j2 -- -D warnings`, using the recorded
manifest and compiler. No repeat/broad tests were added after this passing gate.

Raw transfer used a distinct honest **read-only1CPU/256MiB/1GiB<=600s** request,
with no compilation/test. All36 unique safe regular archive members and35
manifest-covered originals were verified locally; the manifest excludes itself.
This follow-up had no failed build/test attempt. The original prior fmt-before-
compile failure, its two refused headroom reads and later corrected harness
remain unchanged in [the preceding record](../managed-otel-limits/README.md).
That failure is not relabelled as a production defect or a passing run.

The full staged `git diff --cached --check` returned2 for a new blank line at
EOF in the byte-preserved `records/light-v1/reader-tests.stdout`, line20. This
raw libtest output is intentionally not normalized. The separate source/Markdown
whitespace check passed0; the full-check warning is retained in
[the whitespace record](records/whitespace-check.json), not reported as a pass.

## R01–R16 and ROOT obligations

| Rule | Scoped result and remaining boundary |
| --- | --- |
| R01 | Existing fallible buffer allocation, checked numeric/token paths and RAII descriptors reused; new getters/fold add no unchecked index/unwrap/assert/allocation API. Strict light Clippy0 is not whole-caller OOM proof. |
| R02 | Exact pinned PID source/docs and existing memory grammar researched; two real requested getters justify the one private helper. Native file tests are not real kernel-controller validation. |
| R03 | Fixed pids.max name derives from kernel interface; no new runtime numeric bound/threshold/timeout. Existing max/u64/page/token grammar retained. |
| R04 | Explicit finite/unbounded/Failure states, no zero/default entitlement or process-leader interpretation. |
| R05 | One Reader/private parser, no waits/cache/poll/alternate owner or unused ledger. Actual pending caller wiring remains ROOT's. |
| R06 | One bounded scalar fold with two fixed getters and an explicit small test corpus. |
| R07 | Getter comment preserves TID meaning; evidence records local/hierarchical/width limits. |
| R08 | No performance or comparative claim. |
| R09 | New required observation, no alleged production defect. Prior actual harness failure remains retained separately. |
| R10 | Exact source/commands/toolchain/binary/format/result/raw and native boundary/cleanup evidence retained; no actual-controller mutation/effect proof claimed. |
| R11 | Actual GNU x86_64 light only. Full init GNU/musl/architecture/host gates unrun here and owed ROOT; macOS ARM64 only, paused WinARM untouched. |
| R12 | Existing shared Linux guest Reader seam; no per-host copy, platform API, installed binary or production dependency. |
| R13 | Shared typed representation/private helper replaces duplication; existing proof runner/manifests reused unchanged. |
| R14 | No allocation/fault/CPU/timing/profile/benchmark run. Automatic Cargo/libtest elapsed text is raw diagnostics, not performance evidence. |
| R15 | Implementer-only admitted private light and distinct read scopes; no children/goals/audit/heavy/VM/campaign/main merge. Signed named-path gits checkpoint with required attribution. |
| R16 | No local build/cache or protected-path write/deletion. Prior evidence/source/targets and current private artifacts preserved; no agent cache reclamation. |

ROOT owes actual Snapshot/codec/caller/export wiring with omission/diagnostics,
signed OTLP numeric bounds and faithful task units; full carried init/combined
GNU+musl/target builds; real held-controller/identity/generation/cleanup and
applicable guard/restoration gates; official decoder/Collector and any profiles.
The public proof module makes the pending APIs reachable, so this strict light
Clippy result does not assert that unwired real init has no dead-code warning.
Full G25/P0–P13 and owner/parent guarantees remain open. This checkpoint does not
change admission, D59, telemetry cadence or authority.
