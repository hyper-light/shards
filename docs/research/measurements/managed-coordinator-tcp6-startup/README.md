# Coordinator: IPv6 effects, actual startup OOM and cold stderr

POLICY_READ coordinator: f5cec365d2e435afae5e6f613c92d9d3759ef60f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root owns these actual guests, mutations, allocation probes and evidence review.
This record closes no full managed-runtime phase or complete platform gate.

## Native IPv6 interception and meaningful mutations

Root r293 at combined5b7d1bf57990e0ec7aef64601dbe3dee6e4f692c passes ten fresh KVM
guests: local-loopback, local-self, local-to-peer, forward and forward-relay-absent
on IPv6, then the same five IPv4 regressions. All ten actual case exits are0.
Raw commands/source/artifact/config/inputs, actual bytes/tuples/namespace IDs,
counts, joins and outcomes are in linux/managed-intercept-5b7d-both-families-v1.
This is the same experimental kernel/owned synthetic topology, not a product pin,
real forwarding relay or complete route mediation. Addresses/flags derive from
the pinned primary contracts in managed-network-intercept/TCP6.md.

The guarded absent-listener result is NoNewBackendEffectBeforeDeadline: baseline
backend1/1, no second arrival before the original absolute deadline and joined
workers. It is distinct from prompt refusal. Behavior after the window remains
explicitly Unverified. No timeout/error is generally accepted as containment.

Root r337 at the same5b7d source validates relevant independent IPv6 mutations:

| Change | Actual effect | Actual cargo result |
| --- | --- | --- |
| Remove only OUTPUT marking for local-to-peer (r336) | Backend accepts2/receipts2, relay0, joinedtrue; actual direct backend reply. |101 |
| Remove only OUTPUT marking for ::1 (r336) | Still intercepted: backend baseline1, relay1. This is an observed auxiliary-rule non-necessity, not a successful guard-removal proof. |0 |
| Remove only OUTPUT marking for own address (r337) | Still intercepted: backend baseline1, relay1. Same scope limit. |0 |
| Remove the selected PREROUTING interception bundle (TPROXY rule plus its following same-selector DROP) for ::1 | Actual backend2/2, relay0, joinedtrue. |101 |
| Remove that selected bundle for own address | Actual backend2/2, relay0, joinedtrue. |101 |
| Remove ONLY following fallback DROP for forward-relay-absent; TPROXY, mark, route/defaults stay | Actual backend2/2/clientSuccessBackend/relay0, joinedtrue. |101 |
| Restore source byte-identically | All five IPv6 cases pass again, with their respective strict or bounded predicates. |0 each |

r336 failed its campaign assumption that OUTPUT-mark removal must bypass every
local origin. The runtime test correctly still intercepted loopback. No product
fix or test weakening was inferred from that difference. Its raw failure and
source restoration remain in mutations-v2. r337 tests the actual same-namespace
interception bundle separately; the original meaningful local-to-peer result is
retained. Initial mutations-v1 failed setup before edits/cases because a source
substring also matched a pure test; its driver1 and restoration are retained.
The v3 driver always restores the exact bytes and attempts closing green cases,
even after recording a mutation error. All original deadlines are unchanged.

Raw: linux/managed-intercept-5b7d-tcp6-mutations-v{1,2,3}. IPv6 extensions,
fragments, mapped/link-local/scope IDs, UDP/QUIC, received FDs/Unix sockets, policy
replacement, mid-flow relay death, hard aggregate accounting and production
installer integration remain unverified. No performance advantage is claimed.

## Actual VM construction under persistent allocation refusal

Current-main-only candidateab94467ac63b4357306383c07b0d1c5a9170c5e8 combines the
kernel-argv backport and typed StartError. Root r341 exercises actual public
vm::start and real KVM machine construction with the retained experimental kernel,
a256MiB/one-vCPU config and overlong command line. A TEST-ONLY hook enables
same-thread allocator refusal at the real command-line bridge after normal
construction setup; refusal stays active through typed error propagation and
cleanup until start returns. The guest is not run: this is real construction
and failed-start cleanup, not a successful boot or whole CLI-reporting proof.

- Fixed: typed DoesNotFit returns with actual fields what=command line,
  at131072, len2071, both cold-host and repeated-host attempts.
- Meaningful mutation: change only the real bridge back to constructing a
  Legacy String. The actual start aborts on a12-byte allocation, SIGABRT/cargo101.
- Restore typed bridge: both actual attempts return again. Final test-only
  instrumentation is removed byte-identically; ordinary composer4/0 and typed
  fields/output1/0 pass on the restored production source.

The first r339 test returned the typed error correctly but failed a simplistic
FD-count assertion4→5. Root then identified the actual retained object rather
than declaring it a leak or warming it away. The v2 trace shows only fd3=/dev/kvm
added on the cold path, then unchanged on the repeated path. Existing KVM owns
one checked process-wide handle in kvm()'s OnceLock (hv/kvm/mod.rs104–114); no
per-machine anon_inode descriptor remains. The observer excludes only its own
actual /proc/PID/fd directory object. This checks descriptor identities, not
complete RAM/kernel-charge return or every cleanup resource. The cold process
handle's lifetime/cost remains explicitly counted, not hidden from the baseline.

Raw: linux/root-main-start-oom-ab944-v{1,2}, including actual instrumentation,
production mutation, source restoration, commands and output. It is a same-thread
Rust-global refusal probe, not native libc allocation injection, every error-path
proof, or WHP/HVF validation. Legacy startup String producers remain unproven.
Public vm::start and vm::run error types change; external explicitly-String
callers require migration. Main adoption and complete seven-target/VM gates are
still pending; only the candidate branch contains these new pieces.

## Open defect: cold macOS diagnostic output

Root r338 on arm64 macOS copies the exact existing vm_run::report function and
allocator helper from5b7d, with a static Display message. Separate processes show
ordinary output0; cold fail_after0 aborts SIGABRT/-6 on64 bytes; warmed diagnostic
control0. Warming is NEVER a remedy. This independently demonstrates unsafe cold
stdio presentation even without constructing an owned diagnostic String. It does
not pretend to be BootError or a full VM constructor/CLI reproduction.

Raw: macos/root-managed-cold-stderr-probe-v1, source/build/binary hashes, exact
commands, outputs and exits. A TEST-ONLY C-stream-lock candidate is being studied;
no production adapter is selected. A private C lock does not automatically share
Rust's stderr lock. FILE lifetime, native lock failure observability, mixed-writer
serialization, I/O/formatter failures, closed/replaced descriptors, recursion,
blocking and costs require proof. Do not claim the typed-startup fix closes this
separate reporting defect. MR-OOM-MAC-1 owned-client Mutex is also still open.

## Integrity, policy and remaining gates

transfer.json identifies both immutable archives and verified SHA256/byte counts.
392 Linux +22 Mac regular raw records were transferred; executables and object
artifacts stay on their owners with hashes recorded. Extraction rejects absolute/
parent paths, links and executable magic. Originals and old failed evidence remain
preserved; no source, credentials or active compiler cache was deleted.

R01/R04/R06/R07/R13: real typed propagation and explicit outcomes, original failed
assertions retained; no whole-path panic-free claim. R02/R03/R05/R12: pinned
family/native contracts and actual necessity observations, no new product constant,
cache or installed binary, host root or crypto weakening. R09/R10/R15: actual
forbidden backend bytes, independently removed guards, exact restoration and root
role. R08/R14: no latency/throughput/competitive or complete resource claim from
functional elapsed times. R11: exact Fedora KVM +native Mac scope only; other
supported targets/hypervisors and closing gates owed, Windows ARM paused. R16:
private admitted resources and inactive read-only reflink basis; Mac>=30GiB.
Full managed-runtime budgets, mandatory routes, brokerage/adapters/credentials
and generation-safe restore remain required, incomplete work.
