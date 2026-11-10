# Typed command-line startup refusal

Source checkpoint: `ef3ba744fbd696bc98f5d0b0e3d1acee834a4e6c`, based on
`6276f895469a9a20e78e82cdbf4b6d2fd99ce8f0`. This is the approved five-file
follow-up to the isolated main backport, not its adoption or a whole-startup R01
claim. Policy: implementer, full canonical `f5cec365d2e435afae5e6f613c92d9d3759ef60f`
AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff read; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
The coordinator owns all audits, research selection, profiling, benchmarks, real
VMs, fault/mutation campaigns, sweeps and final matrix/merge gates. This lane ran
only scoped units, format/policy and affected-package compilation/lint.

## Actual defect and scope

Coordinator r289 on `ba287c7c93995c9de6bae08b9ecd6d5c51f921ca` called the real
composer with the real `BootError`, then the exact build call-site conversion
`.map_err(|e| e.to_string())` under thread-local allocator refusal. It first
observed `DoesNotFit { what: "command line", at: 131072, len: 2065 }`, then a
12-byte allocation failure and SIGABRT; cargo returned101. **No full VM
constructor was run in that probe.** Original source hashes were restored and
the four ordinary composer units passed. The fourteen root-supplied artifacts
are copied byte-for-byte under [root-red-r289](reporting-oom-light/root-red-r289/)
from `~/managed-cmdline-report-oom-ba28-v1`; the original remains untouched.
The original command and temporary patch are retained, not installed in product
source. That patch uses a coordinator-only allocation fixture from its original
combined snapshot; it is not a standalone current-main test.

The older [main-backport record](main-backport.md) is historical: its unchanged
outer String bridge was subsequently demonstrated unsafe by r289. Its earlier
passes do not establish safety of reporting under allocation refusal.

## Decision and compatibility

`vm::StartError` has exactly the two actual startup representations:
`CommandLine(BootError)` and `Legacy(String)`. The x86 build maps only the
composer error to `CommandLine`; other errors move their existing owned Strings.
`Display` delegates directly, with no intermediate owned formatting. Its
`Error::source()` delegates transparently to the wrapped error's source, rather
than returning an error whose text Display already renders. The current
`DoesNotFit` has no lower source; its actual typed cause and fields remain
accessible through the public `CommandLine` variant.

The coordinator selected the official
[Rust Error contract](https://doc.rust-lang.org/std/error/trait.Error.html#error-source)
and [Firecracker v1.13.0 builder](https://github.com/firecracker-microvm/firecracker/blob/v1.13.0/src/vmm/src/builder.rs)
comparison before approval. The implementer read the selected Error contract
(served version1.99.1, `58cf8f9c3`) to check transparent reporting; its documented
rule avoids rendering the same error through both Display and source. The pinned
1.98.1 documentation URL was inaccessible through the web tool; actual code was
compiled with1.98.1. Firecracker's typed builder enum is a comparison, but its
KernelCmdline String conversion is not copied. No new research selection,
dependency, `Box`, trait framework, alternate start API or static/sentinel message
was introduced. Keeping a String-returning wrapper would preserve the reproduced
allocation; boxing would introduce allocation at that boundary; copying BootError
into a second field representation would discard its existing type unnecessarily.

**Public source compatibility changes:** `vm::start` and `vm::run` now return
`StartError`, not `String`. External callers with explicit String expectations
must migrate; conversion back to String inside the startup path is not supported
by this fix. Repository direct-start callers are private `vm_run::start` and
seven test calls: hotplug120/259/338, boot315, lifecycle105, confined358, acg36.
Those tests already render through Display. The public `vm::run` wrapper's sole
repository caller, lifecycle158, uses `unwrap` and needs no edit. The original
proposal missed the unqualified `start(cfg)?` inside that wrapper: r72 caught the
compile error, the coordinator approved its return-type change, and the red is
retained. No source-history rewrite or String bridge was used to conceal it.

Runtime `start` and the private CLI `start`, `serve_workload` start callback and
`supervise` carry the enum to the existing `report(impl Display)` boundary.
The local failure helper accepts the two actual Display inputs. VMM
`restore`/`restore_from`, architecture restore, private `restore_vm`, arm64
machine build and other public VM APIs keep their signatures. Three CLI restore
call sites move existing errors into `Legacy` to share supervision. The
unsupported-backend `start` matches the public return type. `BootError` is
already public and built on all targets, so the enum needs no per-platform variant.
The existing CLI prefix/newline and direct-start failure/workload `NOT_RUN` exit
branches are unchanged. No startup error is re-stringified later in the migrated
chain. Existing unrelated String creation remains outside this fix and unproven.

## Light validation and retained intermediate failures

Linux runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora kernel6.19.10-300.fc44
amd64, Rust1.98.1 (`48a229cea`), LLVM22.1.8. Explicit admitted envelope for every
run:4CPU/1GiB/2GiB; no lower resource retry or tool provisioning. Private clone:
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/main-cmdline-error-type-7beed95f98`;
private writable target `~/main-cmdline-error-type-target`; external evidence
`~/main-cmdline-error-type-evidence`. The prior own `main-kernel-argv-target` was
only a read-only reflink source. Source mtimes were touched before checks to avoid
inheriting prior-path artifacts. Final five-file before/after hashes match the
signed checkpoint; runs compiled its dirty6276 snapshot, not a falsely reported
already-committed ef3 identity.

| Actual run | Retained result |
| --- | --- |
| r72 / raw/v1 | policy0/format0; first GNU compile101: missed public run String return and test-only non-const array length E0435. No unit executed. |
| r73 / raw/v2 | GNU4+1 and musl4+1 units; three affected strict checks0. Superseded: first-draft Error returned the already-rendered BootError as source. These passes are not evidence for the corrected contract. |
| r74 / raw/v3 | Final source: GNU composer4/0 +typed1/0, musl composer4/0 +typed1/0; strict GNU/musl/MSVC VMM+VM-process all-target clippy0; policy0/format0. |

The focused unit invokes the real composer at the protocol capacity, checks the
actual typed fields, checks no duplicate source, and renders exact existing bytes
into a caller-owned fixed byte slice. It does **not** inject allocation failure,
invoke the full build/start/CLI constructor or prove stderr initialization safe.
Commands, raw logs/exits, source patches and hashes are under
[reporting-oom-light](reporting-oom-light/). Final target `du` is2,932,108KiB
logical; prior reflink source was1,431,448KiB. These are not unique/physical
charges or a hard disk-budget enforcement proof. All own jobs ended; no caches,
frozen artifacts or other agents were removed or interrupted. Mac free before
implementation was38,985,172KiB; no local cargo build ran.

The two exact final GNU/musl VMM test executables were subsequently preserved
and SHA256-verified through reflink copies outside the compiler target, under
`~/main-cmdline-error-type-evidence/frozen-binaries/`. Their original/frozen paths,
sizes and hashes are in [artifact-manifest.json](reporting-oom-light/artifact-manifest.json).
The private target and clone are inactive/frozen after r74; no own job still
writes them and no further sync/build is planned. This is ownership/job evidence,
not a global process audit. No full VM-process or CLI executable was produced.

## Owed coordinator gates

The exact original red invocation is in `root-red-r289/probe.command.json`.
Root must adapt its owned allocation fixture to this typed bridge and actual
caller/report paths, retain exact source/patch/commands, observe return/rendering
under refusal, independently reintroduce the old owned-String conversion to
observe the relevant abort, restore byte-identically and rerun. No such result
is claimed here. Cold std::stderr, earlier allocations, teardown and unrelated
legacy errors remain unproven; never warm them or infer a whole R01 guarantee.

Repeat ordinary focused checks in a private target:

```sh
: "${CARGO_TARGET_DIR:?private coordinator target}"
cargo test --locked -j4 -p shards-vmm --lib --target x86_64-unknown-linux-gnu \
  vm::cmdline::tests:: -- --nocapture --test-threads=1
cargo test --locked -j4 -p shards-vmm --lib --target x86_64-unknown-linux-gnu \
  vm::tests::a_command_line_start_error_preserves_the_cause_and_message \
  -- --exact --nocapture --test-threads=1
scripts/check-development-policy
scripts/lint
```

The exact light musl/MSVC commands are retained in raw/v3. All seven-target
combined gates, external/API caller compatibility, ordinary/oversized startup
reporting and cleanup, current-main real KVM/HVF/WHP argv proof and meaningful
allocation/mutation/resource measurements remain coordinator-owned. Four other
targets (macOS arm64, Windows arm64, Linux arm64 GNU/musl) and actual VM behavior
are unrun here. AMD64 MSVC is compilation only, not native Windows execution.
WinARM stays paused; Intel Mac is unsupported. No performance ratio is claimed.

## R01–R16 accounting

| Rule | This checkpoint / remaining proof |
| --- | --- |
| R01 | Remove this observed String allocation; direct typed propagation/Display. Unit/lint passes only; exact OOM/report/cleanup and cold stderr remain root-owned and unproven. |
| R02 | Coordinator-selected official contract/established builder comparison, reviewed concrete migration and actual r289 red; no broader choice delegated. |
| R03 | No production bound, timer, allowance or numeric policy added; unit capacity derives from existing x86 CMDLINE_MAX. |
| R04 | Retain actual BootError/fields through control and render only at report; Legacy moves existing messages. |
| R05 | One existing startup seam with two real representations; no waits, readiness changes, caches, duplicate authority or framework. Existing startup waits/caches not audited by this lane; root owns that review. |
| R06 | Two enum branches, one local presentation helper, five production paths; no broader startup redesign. |
| R07 | Type comment explains the staged ownership/type boundary; no unsafe operation added. |
| R08 | No benchmark or4–5x claim; root owns comparisons. |
| R09 | Root r289 actual SIGABRT at the exact conversion precedes implementation. No full-constructor reproduction inferred. |
| R10 | Original root red/restoration plus all three intermediate/final light records retained; new exact-caller/mutation evidence owed. |
| R11 | Focused GNU/musl native units and GNU/musl/MSVC compilation only; remaining seven-target/actual host coverage explicitly owed. |
| R12 | Existing hv/arch/startup/report seams; one shared enum, no installed binary, host-root, crypto or platform-policy change. |
| R13 | Two necessary public error-return changes, unchanged restore APIs, no parallel start or stringifying fallback. |
| R14 | No profiling/allocation-count/resource-return claim; du is logical only, not physical attribution. |
| R15 | Implementer light-only, signed named five-file checkpoint/required trailer; no children, main merge, root cache or other-worker interruption. |
| R16 | Private admitted runner/cache, source/evidence/frozen binaries preserved, Mac floor observed; no cleanup or local build. |
