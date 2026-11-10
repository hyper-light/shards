# Independent main backport of KARGV-1

Main-based source: `b8b8e897e3e056c6ba14c9401c38f4f232b5a0b9`, directly on
`origin/main` `f5cec365d2e435afae5e6f613c92d9d3759ef60f`. Branch
`p/main-kernel-argv` contains no managed/API/lifecycle/P4 ancestors. The reviewed
implementation is `06b800822fe0cfaa7619b9d9d0f216710bbb8c05`; only its three owned
VMM paths were applied. Main's x86 call-site/layout/BootError preconditions match
the reviewed parent. The helper and complete x86 source are byte-identical to06b;
`vm/mod.rs` adds only its two cfg-gated module-wiring lines. Unrelated combined
ServedPort/Admission code was not copied. No behavior was redesigned.

POLICY_READ implementer: f5cec365d2e435afae5e6f613c92d9d3759ef60f; full rules
SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Current AGENTS/R01–R16/evidence/CLAUDE/full managed plan/handoff were read before
implementation. Root owns all audits, profiling, benchmarks, heavy real E2E,
chaos/sweeps and final main gates/merge. No children, main merge, local Mac build,
production policy/crypto change or other agent interruption occurred.

## Defect and provenance

Before this backport, main appends required x86 machine options to the whole
command line. Linux's first recognized no-value `--` ends kernel argument parsing,
so appended options become init arguments. Root r173 actually observed
`["/init","managed-network","capabilities","reboot=k","pci=off"]` and the probe
failed. The raw red and pinned primary-source review are copied unchanged from06b
in [README.md](README.md), [red-r173.log](red-r173.log) and
[sources.json](sources.json). That README and its original native-v1/manifest are
**historical**, including their old policy receipt and then-pending gates; this
record describes the main backport's current scope/results.

The coordinator subsequently retained combined4d3f real-guest green in r182
(capability+12 socket boots), and independent original-append mutation/red plus
byte-identical restoration/green in r185. Exact raw commands/source/identities are
on combined under `docs/research/measurements/managed-central-vm/`, including
`managed-kernel-argv-4d3f-mutation-v1`. Those are causal historical evidence for
this reviewed fix, **not a passing new-main VM result**. No experimental kernel
pin, network engine, guest fixture or managed runtime source is imported here.

The helper preserves the raw init tail and existing required-option precedence.
No-delimiter ordering/spacing stays the same. Its bounded stack storage, quote
state, NUL/overflow refusal and existing typed BootError are unchanged from06b.
The Linux6.18 grammar/protocol and rust-vmm alternative references remain exactly
as reviewed. No cache/wait/manager/dependency is introduced. Quiet/boot_into
follow-ups remain outside this backport.

## New-main light results

All results below used exact source b8b8e897e, Fedora44 amd64, Linux6.19.10-300.fc44,
Rust1.98.1(48a229cea), LLVM22.1.8, private clone/outputs and admitted
4CPU/1GiB/2GiB. Installed tools/targets were used; no xwin/toolchain provisioning
or additional campaign was started. Raw commands/exits/compiler identities/source
hashes are in [main-backport-light](main-backport-light/), covered by SHA256SUMS.

| Run | Actual result and limitation |
| --- | --- |
| r68 historical native-helper driver on Linux | compile0, four pure helper units pass. Its test adapter substitutes the actual layout declarations and DoesNotFit variant; it does not compile the complete VMM/ABI. |
| r68 post-test tooling | overall driver127 because Fedora lacks `shasum`. Original empty artifacts.sha256 and failure metadata are retained. Separate artifacts-python.sha256 records actual wrapper/binary hashes; the driver result is not relabeled0. |
| r69 real VMM GNU | four focused vm::cmdline tests pass; strict lib+tests clippy0 on x86_64-unknown-linux-gnu. |
| r69 real VMM musl | four focused vm::cmdline tests pass; strict lib+tests clippy0 on x86_64-unknown-linux-musl. |
| r69 MSVC | strict lib+tests clippy0 on installed x86_64-pc-windows-msvc; no Windows execution/WHP proof. |

r69 was already started when root requested no expansion beyond existing-tool
checks. A stop request found it had already completed0, so no running build or
agent was actually interrupted. No further compilation, provisioning or repeat
ran. These are targeted pure-unit/compile checks, not a full supported matrix,
VM proof, performance result or allocation/fault profile.

Current source SHA256:

- cmdline.rs: `6878c49291097f2775f8d1148f0ca93964be1107bc855f632f4622f157480814`
- vm/mod.rs: `03706a266876559b8aa21d71245fd39ca40d1512c63a07946c21b3852559c5ab`
- x86_64.rs: `d8cc19ef10b03faf269133f64354187ea515890d9e3468785d0d2483602cb5e7`

The 16 historical evidence files remain byte-identical to06b; the29 new raw files
are separate. Original native binaries stay outside Git. The source checkpoint
and evidence commits use signed named-path gits and the required attribution;
signed headers/trailers are checked, not a certificate trust chain.

## Exact owed coordinator gates

After composing the independent main fixes, root runs the current-main matrix
once through `scripts/lint` in its authorized matrix environment. It also repeats
the focused commands against that composed source if it changed their inputs:

```sh
cargo test --locked -p shards-vmm --lib --target x86_64-unknown-linux-gnu -j4 vm::cmdline::tests -- --nocapture
cargo test --locked -p shards-vmm --lib --target x86_64-unknown-linux-musl -j4 vm::cmdline::tests -- --nocapture
cargo clippy --locked -p shards-vmm --lib --tests --target x86_64-unknown-linux-gnu -j4 -- -D warnings
cargo clippy --locked -p shards-vmm --lib --tests --target x86_64-unknown-linux-musl -j4 -- -D warnings
cargo clippy --locked -p shards-vmm --lib --tests --target x86_64-pc-windows-msvc -j4 -- -D warnings
```

The managed_network test source is absent on main and must not be cited as a
runnable main test. Root can reuse its retained argv-sensitive init executable
as a **test artifact**, without importing that source into the shipping backport.
In a private coordinator VM validation clone/target with real carried helpers,
retain its absolute path/hash/source identity, the pinned experimental kernel /
config identities, private home, exact CLI source/binary and all output. The
existing public CLI invocation corresponding to the historical capability probe
is below; MAIN_ARGV_INIT must name that retained compatible observer, not main's
unrelated stock testguest stub. This command has not been run on main here.
The fixture asks1vCPU/256MiB and120s are the prior coordinator's explicit experiment
inputs, not production policy constants.

```sh
: "${MAIN_ARGV_INIT:?absolute retained argv-sensitive init executable}"
: "${MAIN_ARGV_HOME:?private absolute validation home}"
: "${CARGO_TARGET_DIR:?private coordinator build target}"
mkdir -p "$MAIN_ARGV_HOME/docker"
env -u SHARDS_HELPERS CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo build --locked -j4 --profile test-release -p shards
env SHARDS_HOME="$MAIN_ARGV_HOME" DOCKER_CONFIG="$MAIN_ARGV_HOME/docker" \
  timeout 120 "$CARGO_TARGET_DIR/test-release/shards" run \
  --kernel "$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy" \
  --init "$MAIN_ARGV_INIT" --memory 256 --cpus 1 \
  --cmdline 'console=ttyS0 quiet panic=-1 shards_test=managed-network -- managed-network capabilities'
```

Root first records actual unchanged-main failure if required, then corrected
actual init argv/green, independently restores the original append behavior at
the x86 call site and observes the corresponding extra init words/probe failure,
then byte-identically restores/touches/rebuilds and reruns green. Use a private
validation tree, never the active root source/cache or main branch. A compiler
failure or lost alert is not the causal mutation result. The retained r185 patch
is the reviewed original-append mutation template. Actual current-main KVM,
WHP/HVF/arm64 compatibility, full seven-target gate and induced parent/kernel
resources/performance remain root-owned and unrun in this lane. Windows ARM VM
stays paused; macOS support is arm64 only.

## R01–R16 accounting

| Rule | Main backport proof / gap |
| --- | --- |
| R01 | Reviewed checked math/ranges/typed refusal unchanged; real GNU/musl unit and three-target strict clippy pass. No whole-library panic proof. |
| R02 | Existing exact reviewed Linux/parser/protocol/alternative sources and root causal red/green/mutation; no architectural redesign. |
| R03 | Same2048 x86 protocol bound/grammar/option precedence; no new runtime constant or learned allowance. |
| R04 | Existing quote states and typed BootError unchanged; outer build String rendering unchanged. |
| R05 | Underlying composition correction; no wait/cache/duplicate authority introduced. |
| R06 | One existing private bounded helper/call site; no unrelated main APIs imported. |
| R07 | Reviewed comments and source citations preserved byte-for-byte. |
| R08 | No comparative/performance or4–5x achievement claim. |
| R09 | Historical root actual argv symptom/independent mutation; unchanged-main append matches inspected causal source. New-main actual proof still owed root. |
| R10 | Separate current-source raw commands/exits/hashes, historical bytes preserved; current-main real VM/mutation still owed. |
| R11 | Focused amd64 GNU/musl and MSVC compile results above only; seven-target/hardware matrix unrun here. Intel Mac unsupported, WinARM paused. |
| R12 | Existing x86 boot seam shared by KVM/WHP; no installed binary, host root, crypto or policy change. |
| R13 | Only3 production paths plus owned evidence; no managed/API/lifecycle/P4 ancestor merge. |
| R14 | No profile/allocation/fault/parent+kernel cost claim; all root-owned. |
| R15 | Implementer light-only scope; root all audits/heavy/merges. Named-path signed checkpoint/push, no children or main merge. |
| R16 | Private admitted4CPU1GiB2GiB jobs, no provisioning/repeated lower asks/local Mac build/cleanup. Managed-limits source/evidence and root clone/cache untouched. |
