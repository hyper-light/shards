# Private managed capture: implementation checkpoint

This is an experimental G01/G03/P3 source checkpoint, not completion of P3 or the
managed-runtime plan. Implementer `/root/managed_capture`; coordinator `/root`
reviews and integrates the combined capture, lifecycle and endpoint revisions.

Policy read after resume: canonical `c921a0b1`, full rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
The implementation's dependency HEAD is
`0ef05cabc0e97d553f36b1ee52ba0cd469316290`; the commit containing this record
identifies the checked source. Tests below ran with that HEAD plus exactly this
commit's production diff. Documentation/log collection followed the checks.

Owned source: `init::{capture,domains,run,frames,main}`, the shared managed ABI,
`shards::{workload,workload::capture,workload::hellos,warm,vm_run}` and the existing
VMM served-port/platform readiness seams. Endpoint/TLS/admission service and
lifecycle policy have separate implementers. No subagents or main/integration
merges were performed by this lane.

## Live red and scoped checks

The coordinator's real KVM red is recorded in [wire.md](wire.md): fixture
`cdc463b4`, Fedora runner `8fadc`, kiln r47, 0 passed/1 failed, exit 101. Main
stderr contained member prefixes, merged fd1/fd2, added newlines and UTF-8
replacement. The coordinator retains its actual log. The timing-diagnostic
fixture correction is explicit; this checkpoint has no fixed real-VM result yet.

Native host: Darwin 25.4.0, arm64, XNU 12377.101.15, Rust 1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8. Mac checks used
`nice -n 19 lockf /tmp/g121-mac.lock`, cargo `-j 4`; storage remained above 30 GiB.

| Check | Command after the scheduling prefix | Result/evidence |
| --- | --- | --- |
| Native host units | `cargo test -p shards-vm-process --bin shards-vm -j 4 -- --test-threads=1` | 25/25, [raw log](core-host-unit-3.log) |
| Native host lint | `cargo clippy -p shards-vm-process --all-targets -j 4 -- -D warnings` | clean, [raw log](core-host-clippy-1.log) |
| Linux compile/lint | `cargo clippy -p shards-init -p shards-vm-process --target x86_64-unknown-linux-gnu --all-targets -j 4 -- -D warnings` | clean, [raw log](core-linux-clippy-1.log) |
| Actual Linux pipes | `cargo test -p shards-init capture::tests -j 4 -- --test-threads=1` | Fedora `8fadc`, kiln r8: 3/3, 16.63 s build; kiln retains the command/output |

The actual Linux tests are
`native_pipes_preserve_binary_channels_offsets_and_actual_eof`,
`reserved_control_orders_abort_then_explicit_end_without_output_credit` and
`sparse_sibling_and_stderr_credit_do_not_depend_on_noisy_stdout_ack`.
These are native pipe/socket tests, not VM E2E or performance measurements.

A targeted host unit exposed an actual lifetime error in the new acceptance path:
the global listener state retained its wake writer after consumer disposal, so
joining its worker did not finish. [The interrupted run](core-host-unit-1.log)
is retained. Explicit listener close now transitions to Closed and releases its
wake/candidate registry; [the exact corrected test](core-hello-unit-2.log) and the
full 25-test run pass. No heavy deadlock campaign or mutation is implied by this
unit result. No active build/test jobs remain at checkpoint time.

## Representation, ownership and transitions

The [wire record](wire.md) specifies checked numeric member/channel/offset values.
The run connection authenticates scope; this slice does not claim restore-generation
completion or exact descendant attribution. Member payload is opaque and cannot
choose a trusted abort, label, principal or other channel. Stable declared IDs
remain distinct from runnable-vector scheduling positions and preserve files-only
declarations.

The native child receives separate fd1/fd2 write ends. Trusted init owns both
nonblocking read ends and closes surplus descriptors before exec. Canonical data
never enters `prefixed_lines`, the main stdcopy stream, or the lossy Logger.
Actual EOF produces End; a native read error produces Aborted. Owner disposal and
quarantine close remaining pipe endpoints and report unknown uncaptured tail,
not successful EOF or zero loss.

Credit states are Inactive/Waiting/Granted/InFlight/Closed per member/channel.
Guest connection states are Hello/Running, with explicit Idle/Sending progress.
A runnable round rotates separately from sparse wire identity; stdout and stderr
have independent issued credit. Allocations use actual readable bytes and issued
capacity, not the setup protocol ceiling. The guest keeps one framed record;
the slim VM receives one actual-sized record, then the daemon receives/moves it.
Kernel pipe/VSOCK/IPC/QUIC costs also exist and require combined accounting.

The dedicated connection reuses EXEC_PORT, with its exact existing HELLO header
and 16-byte token. Registration precedes actor release. Admission derives from
registered expected exec/capture/resume tokens; no unsolicited queue or guessed
handshake timeout was added. A callback only attempts nonblocking handoff into an
existing cell and wake notification. Full rejects/closes the newcomer without
replacing an unread candidate. The worker owns candidates across readiness
snapshots, reads available HELLO bytes before pressure eviction, and releases
unknown/duplicate/stale candidates. Its fixed parser is `run::HEADER + run::TOKEN`.
At most one active and one incoming candidate per expected token accounts for
two transient descriptors, not two slots of new authority.

Abort control uses the independent duplex SIGNAL connection. Before release it
reserves one encoded abort per active member plus one control-End header, derived
as `active_count * (run::HEADER + Abort::LEN) + run::HEADER`, checked for overflow.
The trusted member hook coalesces each abort. Normal shutdown flushes explicit
control End; EOF without it is an error. Main EXIT does not wait for subscriber
ACK. Observer disconnect never owns the VM lifetime. Private bulk bytes use the
new IPC pair, not the shared main Link/TO_DAEMON mutex; only the small trusted
IPC86 abort uses existing daemon control.

## Wait/cache review and decision basis

The sources/alternatives are linked in [wire.md](wire.md) and the existing buffered
relay research. Separate inherited pipes preserve raw channel bytes; a dedicated
credited connection avoids blocking main output/control on a subscriber. This
uses the existing transport/framing/platform implementations rather than an extra
installed binary, TLS implementation or independent OS-specific manager.

Readiness uses existing poll/WSAPoll and wake sockets. The bounded credit/data
handoff uses a condition-variable notification, releasing its lock before waiting;
no lock spans bulk I/O. There is no new periodic status probe or clock cutoff.
The HELLO registry is the authoritative owned admission state, invalidated when
its owner consumes/cancels/closes it. Per-flow stream state validates protocol
progress rather than caching actor facts. Guest scratch capacity may be reused
within the same channel while credit permits; it is released on channel switch,
shrinking credit, abort or terminal completion. Its measured benefit and actual
resident/page-fault cost remain unverified; this is not a performance claim.
The endpoint lane's experimental host active-plan classifier still needs root's
review against a single authoritative trusted-init prelaunch activation seam.

## Rule-by-rule evidence and remaining work

| Rule | Scoped evidence / unresolved work |
| --- | --- |
| R01 | Checked frame arithmetic/indexing, fallible new bulk/state reservations, handled synchronization poison, clippy clean. Allocation/FD fault injection and a full implicit-panic audit remain assigned combined work. |
| R02 | Parent-reviewed architecture and primary-source alternatives in wire/relay research; actual baseline red. No new architectural choice is inferred from these light checks. |
| R03 | Wire tags/widths, two POSIX output channels, fixed HELLO widths and control/candidate bounds derive from actual admitted entities. Runtime credit comes from caller admission; no guessed buffer/port/timeout. Combined kernel/resource attribution remains open. |
| R04 | Explicit credit, connection, owner, stream and control states; typed ABI records/refusals. Some existing outer String/io presentation boundaries remain; typed internal capture/resource classification is the next checkpoint. |
| R05 | Event-driven waits and authoritative HELLO registry described above. Buffer reuse benefit and host duplicated active-plan classification are explicitly unverified, not waived. |
| R06 | Parsing, scheduling, credit, transport and control are isolated in focused existing-consumer modules. Coordinator review of complexity/composition remains required. |
| R07 | Added comments state safety, provenance and non-obvious ownership; no file-wide explanatory rewrite. |
| R08 | No comparative speed/resource claim. The 4–5x goal remains entirely unmeasured. |
| R09 | Coordinator's live KVM G01 red plus targeted actual host lifetime/pipe failure evidence. No unrelated source inference is called a reproduced defect. |
| R10 | Fixture, wire notes, failed/corrected logs and this source checkpoint retained. Fixed live green, guard-removal mutations and production traces remain owed. |
| R11 | Native arm64 macOS, actual Linux amd64 pipe tests and Linux amd64 cross-lint listed; Windows, arm64 Linux/musl and all real VM combinations remain unverified. Intel Mac unsupported. |
| R12 | Shared VMM served-port and readiness adapters, existing EXEC/SIGNAL/IPC paths; remote/TLS stays outside slim VM. No grants/crypto/root/single-binary change. |
| R13 | Capture stays private to its real consumers; no unused broad managed API or manifest expansion. Full P3 is not claimed. |
| R14 | No profiling/benchmarking performed by this lane. Copies/retention/kernel costs enumerated for assigned combined measurements. |
| R15 | Signed scoped source returned for coordinator composition; no self-main/integration merge. Recurring assigned test/quality/performance lanes remain active under root. |
| R16 | Above Mac floor, no active build at return; no source/data/evidence deletion. The retained two-file composition patch and stash are preserved. |

Dependency composition retained lifecycle population/quarantine proof, optional
event descriptors and settled-versus-ended semantics alongside both channel drain
states. Backup stash `c235fc967f8168bba468cf9ac8b57fd8a1c25cbc` and explicit
`/Users/adalundhe/g121-capture-c235-dependency.patch` are retained, not dropped.
Patch SHA-256: `544b2fb8fa888f1eda703113f12a1af9dd9a9a4920013d8adaf049c890af08d4`.

Next source checkpoint composes lifecycle `df47bc06`/`213c61f5`, retains typed
quiesce refusal and uses its held-cgroup `QuarantineCause::Capture` for affected
tenant allocation/preparation failure. The present checkpoint can still escalate
some capture preparation/resource errors to the run; it must not be represented
as the final tenant-local failure contract.

Required combined work: endpoint composition; live exact-byte/no-newline/invalid
UTF-8 and independent EOF; mixed runnable/files-only compatibility; sibling and
channel progress under absent/slow/dropped observers; trusted-control death and
layer-save ordering; flooded/stalled HELLOs and descriptor return; actual tenant
takeover attempts; fault/chaos/mutation campaigns; restore/move generation and
cursor semantics; all seven targets and native Windows; resource/performance
comparisons. No G01–G24, P3, product or platform-parity completion is claimed.
