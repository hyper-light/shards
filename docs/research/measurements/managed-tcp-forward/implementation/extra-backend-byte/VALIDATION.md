# Actual backend extra-byte guard proof

Source signed5a4b676aa4d4f83f29fa88e6512fb3edb5ce2e62, policy read b57/df2acf
as recorded in [README](README.md). Implementer plus assigned adversarial relay
tester; ROOT owns integration/matrix, audits and profiles/benchmarks. The single
new142-line cfg(test) block can be removed to recover exact1be fixture bytes.
All six other selected source files, including engine1f32faae… and shared pump
083bce5e…, remain byte-identical. [source-identities.json](source-identities.json)
records complete source and driver identities. The old db4 pressure batch remains
19/20 with its separate unchanged passing repeat; this campaign changes none of it.

## Executed campaign

Private Fedora runner8fadc1ec213332e16cb0d915a3fb913f, scope
`~/relay-tcp-extra-backend-f6a-v1`, r153 COMPLETE0. Kernel6.19.10, Rust1.98.1;
full identities in [environment.json](environment.json). Fresh admission requested
2CPU/2GiB/4GiB, compiler/native process AS2GiB, two-CPU affinity and -j2,
outer600s, case30s/parent45s. These are explicit admission/process/caller limits;
physical and kernel resource caps/return remain unverified.

| Phase | Actual result |
| --- | --- |
| Mechanical policy and changed-fixture rustfmt |0 each |
| GNU/musl exact managed-network projection check and strict Clippy, including tests/current lint flags |0 each; source projection, not full normal testguest/host |
| New backend boundary, normal GNU and musl |2 native0 |
| Only response ByteLimit removed, private static-musl copy |Native101/driver1; actual independent client extra58 and EOF |
| `finally` source restoration |Exact fixture d076019b…, all seven hashes restored, tracked git diff0/empty |
| Restored new boundary, ordinary server-first, ordinary backend-FIN-first, client boundary and held-receipt cancellation, each GNU/musl |10 native0 |

Total12 positive native0 and one expected actual-effect mutant101. There were
no retries, pressure-input changes, new polling hooks, VM or held campaigns.
The new case's ordinary filter is
`managed_network::intercept::tcp_forward::native_tests::native_tcp_extra_backend_byte_is_not_forwarded`.
The exact [native-campaign.sh](commands/native-campaign.sh) retains every command,
caller input, native case name, expected negative and bounds. Its existing paths
refuse overwrite; any future authorized rerun requires a new private scope and
explicit immutable5a4 checkout if the branch tip has advanced.

## Independent byte and terminal witnesses

Both baseline libcs record backend request13 exact bytes and real request EOF.
The completed backend Worker returns the accepted socket still write-open,
after sending the14-byte greeting+tail. The main client actually receives those
14 bytes before releasing extra58 from that held backend socket. The backend
then records15 locally accepted bytes and its real write half-close.

Strict source returns typed `ExtraData{leg:Backend,offset:14}`. To-client
read/written14, pending0, extra-consumed1; actual client response14 exact bytes,
real EOF and no extra. To-backend read/written13, `WriteClosed(Eof)`, pending0,
extra-consumed0. Its earlier external request/response effects are retained;
nothing was rolled back. The original absolute deadline still had positive
remaining time, recorded as exact nanoseconds in [CHECKS.json](CHECKS.json).

The private mutation changes only the response entry passed by `run_accepted`
to the same pump: `[ByteLimit::Bytes(request), ByteLimit::PeerEnd]`. No shared
pump byte is edited. The original small patch is in the raw archive.

The mutant client independently records exact14-byte response prefix plus
`extra_hex:"58"`, then an actual EOF from the second finite probe. The first
unexpected-byte error remains `InvalidInput`; no error is cleared by observing
that EOF. The flow itself records `BothHalvesClosed`, no fault, to-client
read/written15 and pending0/extra-consumed0. Backend request13/EOF and accepted
write15 remain. This is a real extra-byte effect, not a missing alert.
All joins, child descriptor facts and original remaining time were printed
before the strict test failed at `client_result.unwrap()` with
`unexpected extra TCP endpoint byte` (native101). The driver result remains
`passed:false`; the outer campaign explicitly requires that negative plus the
independent effect and cleanup checks.

`finally` restores fixture SHA256
`d076019b672558ef53a8a8168f9b4989a08205972501cd6de62ed857e0c03e66`.
All seven before/after hashes match and tracked diff has status0/zero bytes.
Only the declared untracked source projection root remains. Restored native
binaries match the baseline binaries exactly, before the10 fresh controls:

| Artifact | SHA256 |
| --- | --- |
| Baseline/restored GNU | `bdbe09bf69777abc5c75e74074b77d49679d6a777f7e4f6102fbe92221ec3d97` |
| Baseline/restored static-musl | `5a906289796410b073a0058e7732fc3c09f8dd9da9594840f9c0b0a7bbfc3534` |
| Retained static-musl mutant | `64387d7f8b76def07bc7aba2490e444eecdca5d9faffec535416893f963b9da0` |

All25 phase children have actual held-pidfd exit readiness and are reaped;
none required forced cleanup. Outer callerFD9→9. Each of the13 native parents
records FD4→4, held pidfd exit/reaping, no timeout and exact before/after binary
hash. Boundary child FD4→4 and worker joins are recorded before assertions,
including the mutant. Ordinary controls retain their native descriptor-return
assertions and actual stdout; held-receipt cancel retains typed cancellation
and positive original time. This does not prove whole-system/kernel return.

## Durable raw record

[raw-linux.tar.gz](raw-linux.tar.gz),54704 bytes, SHA256
`52a4742b042f7b198d6fa215cb83a67ab96a75541c74e22f70b08f4b56c7e2f2`.
All117 safe regular members were verified locally;116 original files match
[raw-manifest.json](raw-manifest.json) by exact length/hash. It contains raw
stdout/stderr, observed bytes, inputs/results, check/lint/build logs, original
private mutation patch/source, exact restoration, source-after receipt,
projection/config/locks and toolchain identity. No original failure, newline,
encoding or byte receipt was normalized. [CHECKS.json](CHECKS.json) keeps the
compact record and all explicit forbidden-effect checks.

Source, target, private Cargo home and artifacts remain untouched under the
owned remote scope; frozen baseline/mutant/restored binaries are outside target.
No local compilation or target/cache was created. No ROOT path, paused WinARM,
main branch or held campaign was changed. Root integration remains separate.

## R01–R16 account

| Rule | Evidence and remaining limit |
| --- | --- |
| R01 | Only cfg(test) assertions/unwraps added; GNU/musl strict current lints0. Production implicit-failure safety is not established by lint. |
| R02 | Implements the exact reviewed f6 NEXT proposal using existing primary-backed TCP/Deadline/Worker seams; no new architecture or production choice. |
| R03 | Same explicit caller byte/time/slot values. No production number, capacity, timeout or grant changes. |
| R04 | Existing typed ExtraData versus ordinary terminal states, request/response/EOF/extra facts and cancellation semantics remain explicit. |
| R05 | Existing readiness, Worker and original absolute Deadline; no manager, polling interval, cache or synthetic WouldBlock. |
| R06 | One focused142-line cfgtest addition plus one decoder case. The existing fixture file grows; no broader refactor or audit is claimed. |
| R07 | Added comments explain held ownership, ordering and preservation of the first error; safety comments unchanged. |
| R08 | No comparative performance result or4–5× claim. |
| R09 | This is a guard fixture, not a guessed production fix. Its exact guard removal produces the actual independent forbidden byte before strict failure. |
| R10 | Exact source/mutant/restoration/binaries, commands, raw failure/effects and controls are durable here; original pressure failure retained separately. |
| R11 | Actual native Linux x86_64 GNU/musl only. Guest seccomp/VSOCK/full seven-target and other hardware remain unrun here; macOS ARM64 only. |
| R12 | Same shared pump/Deadline and existing Worker/Endpoint; no duplicate runtime implementation or installed helper. |
| R13 | New finite case only; unchanged production paths, existing strict fixtures and host registration. |
| R14 | No profiling/benchmark/audit; parent/kernel charges, page-fault/allocation costs and leak-freedom remain ROOT work/unverified. |
| R15 | Exact bounded owned native campaign with held child handles, independent effect observers and expected negative. Signed checkpoints; no children, goals or main merge. |
| R16 | Fresh private admitted paths, no headroom bypass/cache warming/deletion. Original source/evidence/frozen artifacts and all protected scopes preserved. |

Pressure hooks/turn-seam options remain proposal-only. ROOT still owes current
carried KVM VSOCK, complete caller/helper and matrix gates. Native TCP proof
does not establish interception, mandatory internal/IPv6/UDP/FD route coverage,
hard producer/parent/kernel budgets or management P0–P13/G25 completion.
