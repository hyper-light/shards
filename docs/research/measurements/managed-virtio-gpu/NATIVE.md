# Native follow-up to118978b03

This extends the first checkpoint's current native evidence. It does not replace
its frozen red/green record or close the GPU-host matrix. Production source is
unchanged from signed `118978b03c0f27f17a995bcecb1d7be3dedb3221`:
GPU protocol SHA256 `56bee5a5515c8059b2efd46dc026723b81d28421fbcd06b0392a64844362f486`,
GPU mod.rs SHA256 `8858bc80655d9f52436690be2bea3d40d94820d8abe64438282dbf6539d6d1a3`.
Exact product Cargo.lock SHA256 remains
`1ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133`.

Implementer/coordinator, policy receipt and R01–R16 accounting are in README.md.
All executions retain the user envelope2CPU/2GiB/4GiB/600s/-j2, and the Mac
30GiB floor plus `/tmp/g121-mac.lock`. No VM launch, fault-injection campaign,
mutation, benchmark, profiling, driver install or global system change occurred.

## Actual unit results

| Host/command (after `light-check.py LABEL`) | Result and raw |
| --- | --- |
| Fedora44, Linux6.19.10-300.fc44.x86_64, glibc2.43: `cargo test --locked -j2 -p shards-vmm --lib devices::virtio::gpu:: -- --nocapture --test-threads=1` |14 passed,0 failed,0 ignored; `raw/linux-gnu-framing` |
| Same host: `cargo test --locked -j2 -p shards-vmm --target x86_64-unknown-linux-musl --lib devices::virtio::gpu:: -- --nocapture --test-threads=1` |14 passed,0 failed,0 ignored; `raw/linux-musl-framing` |
| Mac ARM64 native unit/worker corpus and strict Clippy |13 passed/0 ignored and Clippy0, as the first checkpoint records |

Both Linux corpora include the native descriptor-backed `mmap` window test,
which verifies two-way memory bytes and inaccessible memory after unmap. They
use no hypervisor and link no Venus renderer. A static musl unit binary run on
Fedora does **not** establish an Alpine-host dynamic-musl GPU helper or driver.
These are native unit results, not actual GPU VM or renderer compatibility.

Linux runner `8fadc1ec213332e16cb0d915a3fb913f`, private checkout/target:
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-virtio-gpu-kiln-d1ae1dd089/target/managed-virtio-gpu-light`.
It is separate from ROOT's Fedora checkout/target. Raw stdout/stderr/receipts/diff
were transported with per-file SHA256 verification. The protocol snapshot was
reconstructed only after proving its bytes equal the remote receipt's SHA256.

## Read-only native metadata

These probes request device/driver/API metadata. They are neither Vulkan compute
workloads nor proofs that a confined GPU VM, whitelist or renderer works. The
pre-existing `vulkaninfo` executable's internal object/queue creation was not
instrumented; the harness's original scope wording about queues was intent,
not a measured absence of those internal calls. No GPU command workload was
requested. Tool, loader, helper, SDK/header and source hashes below are in raw.
Driver version strings are observations; no driver-library binary hash is
claimed where it was not collected.

- Mac runner `e30189ea29182644fe5a4b2e06c7dac2`: native
  `MTLCopyAllDevices`, Apple M5 Max, unified memory, macOS26.4.1 build25E253.
  `metal-readonly.m` creates no command queue/buffer or Metal memory allocation;
  it enumerates device names/registry IDs and properties. Native helper compiled
  with Apple clang21.0.0, exact helper source/binary hashes retained, Metal SDK
  header SHA256 `d1ef1bb5bc85faeeec9bc85f49124a453a8cd33b1182b0a71a41ba0dc4ebbb89`.
  `raw/metal-native-readonly` retains the original `xcrun --show-sdk-version`
  failure. `raw/metal-sdk-provenance` uses the actual compiler dry-run's sysroot
  and SDKSettings.json (SDK26.4), and succeeds. No Xcode/SDK setting changed.
  No Vulkan/MoltenVK native or guest inference.
- Fedora runner above: `/usr/bin/vulkaninfo --summary` exits0, Vulkan instance
  1.4.341. Intel Graphics RPL-S (`8086:a788`, revision04), i915, and NVIDIA
  GeForce RTX4070 Laptop/NVK AD106 (`10de:2820`, revisiona1), nouveau; both Mesa
  26.0.3, device API1.4.335. llvmpipe is explicitly a CPU renderer and is not
  counted as GPU hardware parity. The dzn ICD skip, missing display and display
  extension warnings are retained in `raw/linux-native-readonly`. Tool SHA256
  `cdfe167b6bd87e1845adfb4f6685d54e432fd6abf50833f8dd34ffedb289987f`.
- Jupiter runner `f766f09294f2e17e3b8a631ffd4e6a4b`: Windows amd64 build26200,
  AMD Radeon8060S, Windows driver32.0.22032.6002, Vulkan driverInfo25.20.32.06
  (LLPC), device API1.4.329. `vulkaninfo --summary` exits0. System32 loader
  1.4.321.1, SHA256 `37853ec7133bb12b56ea2e18be7ccb6e6da07a3b1deee2e5ecd1154e5db30453`;
  tool SHA256 `bf80053d2a7390857b3b5dbf790ad7f98cbf7b831a1671247710876e66f7c794`.
  `raw/windows-readonly` retains exact tool output, PowerShell progress XML,
  parsed fields and separate actual Vulkan stdout/stderr. The registry-layer
  manifest warning remains present. This is outside the product AppContainer
  and proves no confinement, imported-blob memory or VM behavior.

The Windows initial Python3 and python commands were Store aliases, exit49
before any Python probe. No interpreter was installed and no setting changed.
The first PowerShell wrapper treated native stderr's loader warning as an
exception (`ErrorActionPreference=Stop`), exit1, before producing its structured
result; `raw/windows-wrapper-error` preserves it. The corrected wrapper uses
separate asynchronous stdout/stderr readers and the same explicit45s probe
budget, preserving the warning and actual native exit0. `tool-attempts.json`
retains these and the transport/tool-discovery failures. Raw streams are never
rewritten into a passing result.

## Current compatibility boundary

Current native unit proof is Mac ARM64 and Linux GNU/musl amd64. Current metadata
proof is M5 Max Metal, Intel/NVK Linux Vulkan and AMD Windows Vulkan. Actual
current confined renderer/VM, GPU capability enforcement, multi-context resource
lifetime, hostile sizes/fences, old/new snapshot, pressure/control/cleanup and
comparative performance remain ROOT-owed. Linux ARM64 and Windows ARM64 GPU hosts
are unverified. The Windows ARM VM stays paused. adacachy2's proprietary NVIDIA
host remains unavailable; NVK metadata does not replace that host's gates.

The source-confirmed Windows ARM GPU packaging omission and actual physical
selector/whitelist requirement are coordinated with existing Hex L2
`01a1c7676c7c76cb998cb12303110488` and Hex ROOT
`01a1aa77778471339bea3beff90d6bbd`. Both roots must approve the exact path split;
there is no transfer or broad edit inferred from a peer proposal. I proposed
retaining protocol/window/device resource/fence core, with Hex L2 owning physical
identity/selection and virgl filtering/ACK/Windows ARM renderer work. No such
shared production edits have been made by this lane. Both API and VMM already
depend on `shards-abi`; that existing seam is a source-grounded input for ROOT's
type choice, not approval to create a new crate or edit ABI.

New Windows ARM primary research: pinned Vulkan-Headers1.3.252 calling convention,
Meson1.11.2 cross-build host-machine distinction and Microsoft ARM64 ABI source
at36738035d38d4d2ab474a3f4681366fa341a6f6c, with content hashes in
`windows-arm-sources.json` and `windows-arm-abi-source.json`. No Rust ABI defect,
working crossbuild or native ARM GPU result is inferred from reading them.

## Operations and remaining work

All lane build/test/API jobs ended at this checkpoint; no queued job is reported
running. Local target remains dedicated; the native Metal helper is a frozen
identified artifact, not permission to delete it. No source/attachments/credentials
or other lane's target/evidence was deleted. One subsequent Kiln sync reported
files_sent55/removed8/bytes2660546 without enumerating removed paths, within the
lane's private reach checkout only. All expected named GPU raw payloads are
verified locally; no known evidence was lost. No path-specific deletion was
requested. Further remote runs used `sync:false` and preserved remote-only data.
The unenumerated eight removals remain an explicit transport uncertainty.

ROOT still owes evidence review, all audits/profiling/benchmarks, guard-removal,
GPU VM/fault/chaos/pressure/matrix sweeps and main integration. A proposed bounded
GPU request-ENOMEM native unit is held for ROOT classification/approval; no
allocator fault was injected and no source-only allocation fix was made. Existing
production allocation/panic risks therefore remain unresolved. No parent/kernel
entitlement, tenant isolation, whole GPU parity, layer completion or4–5x claim.

Mechanical policy check and harness in-memory syntax check exit0. The complete
native-evidence staged whitespace check exits2 on authentic unit stdout EOF
blank lines and retained Windows CRLF/PowerShell output. Raw bytes remain
unchanged; full output is native-staged-check.stdout. This is not a complete
whitespace pass. Production protocol bytes remain identical to the tested
118978b03 checkpoint.

## New GPU core checkpoint05015e3fa

CONTEXT-FENCES.md now supersedes the earlier source-only context inquiry with
actual native component red/green. New current bytes pass Mac14/0/0, GNU15/0/0
and musl15/0/0, strict VMM lib/tests Clippy0. Both Linux runs print actual used
heads[0,2,4,6] before the new fence signal and[0,2,4,6,8] after it. Full native
renderer/GPUVM/old-new snapshot/physical-enforcement/performance gates remain
unrun; no extra fault, mutation or benchmark authorization is inferred.
