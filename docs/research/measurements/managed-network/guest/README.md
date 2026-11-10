# Prepared direct-guest network fixture (coordinator execution owed)

This checkpoint prepares real-guest capability observations and native socket
pairs. **No VM, socket campaign, mutation, interception experiment or performance
measurement was run by the implementer.** P4/G04–G07 and the mediation architecture
remain incomplete. The native engine comes from signed `f17884fc`; an actual musl compile failure
then required two coordinator-approved ancillary ABI-width adaptations. Its socket
algorithm/state remains unchanged; direct-guest dispatch and the host fixture are new.

`POLICY_READ`: canonical `c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`, role implementer,
`/root/constants_research`. Full R01–R16, evidence, AGENTS, CLAUDE, plan/handoff read.
The newest explicit bootstrap reserves audits, profiling, benchmarks, heavy real
E2E, chaos and sweeps to the coordinator. This lane does only scoped implementation
and light parse/compile/clippy checks; no nested agents or main merge.
The coordinator also identified separate failure-path debt in cleanup checkpoint
fa1b5d356: raw Child ownership and blocking BufRead readiness lack a bounded failure
guard/RAII cleanup. That is fixture debt, not a product bug; no cleanup source is
changed here and root holds it out of combined pending a separate fix/check.

## Source and dispatch

Sparse branch/worktree `p/managed-network-guest` / `managed-network-guest`, based on
pushed combined `5d231e1b36ad37f9de7451bce1207a4fc859153b`. No cleanup-branch merge.
The existing native checkpoint's module and measurement records are composed by
named paths. Their older policy receipts/results are historical evidence, not
current policy or a new pass. Imported native source before ABI adaptation SHA-256:
`fcab6063aa76bace6b644369e6ae306ea8e3d1e897316e9ab146c822cf374ac5`.

New code is [the host fixture](../../../../../crates/shards/tests/managed_network.rs),
its testguest module/native dispatch composition and one Linux test arm. Direct
PID1 boot retains the existing Linux setup/report/poweroff path and reuses its
existing `loopback_up`. Non-PID1 native invocation retains `testguest managed-network
...`. Existing managed lifecycle/reap modules and workload arms are preserved.
Production networking/kernel pins and kernel configs are unchanged. The only
engine-source difference from f178 is [ancillary-abi.patch](ancillary-abi.patch):
checked destination-field conversion and checked common-width ancillary comparison.
Final module SHA-256: `8cfc9417e256d10fe0e8a36c384f5ba2eca913846a65b8b5b476ccbcaa2ddac2`.

The host passes native probe argv after the kernel's `--`. [Linux v6.18
set_init_arg](https://raw.githubusercontent.com/torvalds/linux/v6.18/init/main.c)
forwards those words to init; [next_arg](https://raw.githubusercontent.com/torvalds/linux/v6.18/lib/cmdline.c)
handles double-quoted empty words. This invokes no shell. The pure parser test
checks generated empty-payload argv and missing config evidence. Actual guest argv
and runtime behavior still require the coordinator run. Pinned source hashes are
in [sources.json](sources.json).

## Three distinct observations

| Record | What it establishes | What remains unverified |
| --- | --- | --- |
| Supplied config file, SHA-256, provenance, named values including `n`/`absent` | Exact bytes supplied as the build-output evidence | Association with the booted kernel is coordinator-supplied, not independently attested; `m` is not proof a module is loaded |
| Guest `capabilities` kernel release/netns and transparent-bind accepted/refused with errno | Actual runtime syscall outcome when executed | Successful IP_TRANSPARENT is not proof an nft socket/TPROXY hook exists |
| Two native endpoint `Matched` records plus exact receive hex, EOF/half-close events | The tested native pair's payload/socket semantics when executed | No interception, grant mediation, original-route attribution or bypass closure is installed/proved |

The capability test accepts an explicit refusal as a collected observation; it
never turns that refusal into a capability-present claim. It requires no `Matched`
record and leaves `nft_hook=unverified`. The guest does not read or pretend to
observe the host's supplied config; it prints `config=unverified`. The host records
supplied evidence separately. Socket tests are separate and cannot pass merely
because capability collection succeeded. Missing native IPv6/socket support is an
actual failing socket result to retain, never a reason to relax assertions/grants.

## Prepared cases and records

Two real-VM tests are ignored in ordinary suites and must be explicitly selected
by the coordinator. The first boots capability collection. The second prepares
12 direct boots: IPv4/IPv6 × TCP/UDP/connected UDP × binary/empty payloads. The
existing pair implementation binds before starting its peer; the OS assigns the
port. It uses eventfd cancellation/readiness and a caller-provided absolute socket
deadline, not periodic `/proc`/state probes. Binary inputs include NUL, invalid UTF8
and newline bytes. TCP requires both WriteClosed and PeerClosed events for both
endpoints; the server waits for client EOF before replying. Empty UDP remains a
zero-length datagram, not stream EOF.

The host records each actual kernel/config/guest hash, supplied config provenance,
command line, status and stdout/stderr, with the configured budgets. OCI-style
`sha256:` prefixes come from the existing shared digest helper. The coordinator
script also retains source HEAD/status/source SHA-256 and an exact copy of the
supplied effective config. It refuses to reuse an existing evidence directory.
No production-pin fallback is present: `SHARDS_KERNEL` must be an absolute existing
experimental file. No RUN/VOLUME install, shell actor, new manager or networking
implementation is added.

## Supplied experimental artifacts

The coordinator reported rootless pinned-Debian build r122 succeeded for combined
5d231e1b plus the two-symbol experiment. The implementer checked only these two
read-only artifact byte hashes; neither was booted here. Full metadata is in
[coordinator-artifacts.json](coordinator-artifacts.json):

| Artifact under Fedora `$HOME/managed-kernel-tproxy-5d23-v1/out/` | SHA-256 |
| --- | --- |
| `vmlinux-6.18.48-x86_64-managed-tproxy` | `97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae` |
| `config-6.18.48-x86_64-managed-tproxy` | `fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d` |

These are supplied effective-build artifacts, distinct from the preserved
`../experimental-tproxy.config` input fragment. Their hashes do not establish
runtime availability or interception. Root retains the build source/config/logs.

## Owed coordinator command

Use a coordinator-owned private checkout/target and the reviewed fixture revision.
The unchanged last assigned real-VM envelope is **4 CPU / 8 GiB RAM / 20 GiB disk**;
obtain admission before execution and never lower the ask to bypass the guard.
Select positive guest CPU/memory and boot/socket budgets within that envelope;
there are deliberately no fixture defaults copied from a past measurement.

```sh
export CARGO_TARGET_DIR="$HOME/managed-network-guest-coordinator-target"
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_EFFECTIVE_CONFIG="$HOME/managed-kernel-tproxy-5d23-v1/out/config-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_CONFIG_PROVENANCE='coordinator r122; combined5d231e1b plus two-symbol overlay; rootless pinned Debian build'
export SHARDS_NETWORK_RECORDS="$HOME/managed-network-guest-reviewed-tproxy-v1"
: "${SHARDS_NETWORK_CPUS:?coordinator chooses positive vCPUs within admission}"
: "${SHARDS_NETWORK_MEMORY_MIB:?coordinator chooses guest memory within admission}"
: "${SHARDS_NETWORK_BOOT_MS:?coordinator chooses direct-boot liveness budget}"
: "${SHARDS_NETWORK_CASE_MS:?coordinator chooses socket liveness budget}"
export SHARDS_NETWORK_CPUS SHARDS_NETWORK_MEMORY_MIB SHARDS_NETWORK_BOOT_MS SHARDS_NETWORK_CASE_MS
bash docs/research/measurements/managed-network/guest/run.sh
```

This command runs the capability boot and 12 native pair boots. Root owns actual
success/refusal collection, exact reviewed-source combined gates, guard mutations,
resource cleanup/profiling and all further P4 route/interception work. No VM result
or source-independent effective-config verification is supplied by this checkpoint.

## Actual portability blocker and correction

The old native source passed GNU compilation but failed actual
`x86_64-unknown-linux-musl` checking at msg_controllen assignment and cmsg_len
comparison (r64, E0308). [libc0.2.189 musl bindings](https://raw.githubusercontent.com/rust-lang/libc/ef0906e20828777175f65caa7e681a0ce33c559a/src/new/musl/sys/socket.rs)
use socklen_t there; [GNU bindings](https://raw.githubusercontent.com/rust-lang/libc/ef0906e20828777175f65caa7e681a0ce33c559a/src/unix/linux_like/linux/gnu/mod.rs)
use size_t. The coordinator approved only the two ABI adaptations after this red.
No socket algorithm/state was redesigned. Conversion failures retain explicit
InvalidInput errors. Scoped useless_conversion allowances preserve one shared
checked expression where GNU's conversion is an identity; no panic lint was relaxed.

The first host clippy run also caught println in a non-test helper; it now uses
fallible output. The first ABI clippy run caught a displaced SAFETY comment; it was
moved next to CMSG_LEN. These development failures are preserved and are not live
network or policy reproductions.

## Light validation and remaining coverage

Actual light gates: r64 final host parser **1 passed, 0 failed, 3 filtered out**;
strict host-fixture clippy **exit0**; corrected testguest strict clippy on both
`x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` **exit0** (final r65 command).
The musl command compiles/checks the real guest code; it is not an executed guest
or a static-link/boot pass. [light-results.tar.gz](light-results.tar.gz) retains 27
raw logs/status/source/environment/binary records, including all earlier failed
checks; [light-manifest.json](light-manifest.json) hashes them. Final source hashes
in `abi-checked/source.sha256` match the checkpoint. The pure parser executable
identity is recorded; no guest-run identity is fabricated.

Private Linux clone `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-guest-839819c94b`,
private target `~/managed-network-guest-target`, evidence `~/managed-network-guest-light`.
Assigned admission stayed4CPU/8GiB/20GiB, cargo -j4. The target was read-only
reflink-copied from this lane's inactive cleanup target; no other active cache was
modified. No lane jobs remain active. No Mac compiler/test ran.

The implementer selected only `fixture_cmdline_and_config_preserve_empty_and_missing_evidence`
for its host unit test. It performs no socket, process-launch or VM operation.
Compile/clippy commands are scoped to the host test and testguest. Actual results,
source hashes and compiler logs are retained beside this note; no real-VM command
was executed by this lane. The native engine's older native results remain in the
composed checkpoint and are not a new guest pass.

Seven-target builds/lints, actual HVF/WHP and other KVM hosts, guest runtime argv,
effective nft hook availability, loopback/self/wildcard interception, internal/
external routes, admission/accounting, mutations and performance remain unrun.
The supplied kernel here is amd64 only; macOS support is arm64 only and the paused
Windows ARM VM was untouched. Payload matching alone cannot close P4.

| Rule | Proof / remaining limit |
| --- | --- |
| R01 | Test-only dispatch/fixture and reviewed checked ABI adapters. Guest dispatch propagates failure; no production panic/allocation safety certification. |
| R02 | Existing reviewed native checkpoint, actual musl compile red and pinned libc/kernel argv API references; runtime/interception decisions remain with root evidence. |
| R03 | No new product limits/default budgets. All resource/deadline inputs explicit; binary/empty patterns are synthetic fixture inputs. |
| R04 | Supplied config, runtime capabilities and payload completion are distinct records; existing native states/errors retained with explicit ABI conversion refusal. |
| R05 | Existing direct-boot, setup/report/poweroff and pair seams; no new socket engine/cache/manager. |
| R06 | One direct dispatch arm and focused host test; existing loopback setup reused. |
| R07 | Comments explain PID1 dispatch priority, kernel argv quoting and evidence distinctions. |
| R08 | No comparative measurements or achieved 4–5× claim. |
| R09 | No product defect/fix claim; all runtime reproductions owed to coordinator. |
| R10 | Runnable explicit command, source/artifact hashes, retained light compiler logs and clear unrun cases. Runtime/mutation artifacts are owed. |
| R11 | Only recorded light host/guest compile scope; seven-target/actual-host matrix unverified. |
| R12 | Shared helpers and existing testguest; no installed deliverable, host root, grant or crypto change. |
| R13 | Compose existing implementation; new behavior limited to direct test dispatch and evidence/assertions. |
| R14 | No profiling/allocation/performance conclusions; coordinator owns those runs. |
| R15 | Current role receipt/ownership, scoped light checks only, signed named paths, no children/main merges; root owns all VM/campaign execution. |
| R16 | Initial 29GiB Mac hold honored; sparse checkout only after actual recovery to ~44GiB. Private target/clone; inactive own target read-only reflink; source/evidence preserved; no guard bypass/local compilation. |
