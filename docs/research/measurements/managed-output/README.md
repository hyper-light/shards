# Managed owner-output service checkpoint

Implementer `/root/managed_output_endpoint`, based on main `6bf5b15d`.
POLICY_READ: R01–R16 at policy revision `b4918cd8`, SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
The coordinator owns audits, real-VM/adversarial campaigns and all measurements.

This checkpoint implements a transport component, not complete G02/G11/P3.
The follow-on daemon registration, scoped credit, admission and learning are described in [admission.md](admission.md); combined live gates remain open.

## Interface and ownership

`shards_remote::managed::Endpoint::bind(identity, owner, scope, admitted)` returns
an already bound endpoint, one `Producer` and owner `Control`. The daemon will run
`Endpoint::run` on its owned thread. `Admitted` is an explicit caller reservation
contract: record, transport, handshake and datagram allowances plus patience. It
neither reserves resources itself nor proves opaque library/kernel accounting.

The default UDP endpoint binds loopback on an OS-selected port and rejects80/443.
It uses the existing hyper-quic/hyper-tls/Ed25519 implementation, pins the allowed
client fingerprint during TLS verification and uses `shards-managed-output/1`
ALPN, separate from remote command execution. It never starts a CLI or child.

The producer accepts one checked whole record by owned Vec. No subscriber means
no data credit. `capacity` is nonblocking; `wait_capacity` is confined to a private
data worker; `on_credit` wakes that worker after state locks are released.
`submit` returns the owned record on Full/Closed/Invalid and checks allocation
capacity, not merely slice length. Captured content is omitted from error Debug.

The wire reuses remote framing: tag byte, u32BE payload length. Subscribe is Exec
with distinct RunId, u64 generation, u32 member, channel and u64 next offset. Data is Stdout with u64
start offset then one unchanged managed record. Consumed is Stdin with u64 end
offset. These tags do not invoke the remote CLI server. ACKs only clear an entirely
sent record at its exact contiguous end; transport/socket ACKs do not return credit.

An observer disconnect retains that one unacknowledged record at its original
cursor and blocks the producer. It does not own or stop the VM. Owner `finish`
marks normal ending after app consumption; `abort`/Control drop revokes the service,
wakes blocked producer and stops the driver independently of its data handoff.
Zero persistent retention is implemented; multi-observer fanout is not promised.

Explicit states: subscription Listening→Connecting→Subscribed→Listening/Closed;
slot Waiting→Ready→Record→AwaitingAck→Ready/Finished, with Aborted terminal;
owner ending Open→Finishing/Aborted. Disconnect resets only send position and
subscription, never consumed offset or VM ownership. Generation is caller-minted;
restore/move must mint a new one, which this component does not yet implement.

## Shared seams and alternatives

The existing remote command Host/Running relay has fixed1MiB pending buffers,
64KiB reader buffers,4 stdin messages, default100 streams and unbounded driver
commands. Those are not treated as admitted managed-output capacity. Instead the
same Driver gains an optional bounded entry point, typed full handoff, fixed-size
control reads, explicit connection/datagram allowance, bounded receive turns and
an independent stop flag/wake. The legacy command path keeps its public protocol.

Pinned hyper-quic `f116ac276d2d53296de24c2a43837baf66ed1cbd` provides explicit
send/receive/stream windows, crypto-buffer size and incoming limits. Its
`send_window` documentation warns that slices can retain larger allocations;
retaining only a short Vec length would not bound its capacity. Connection
admission is also checked before accepting, because max_incoming bounds the
pre-accept Incoming object, not all live connections.

Primary sources and established comparison:

- [RFC9000 §§4,4.6](https://www.rfc-editor.org/rfc/rfc9000.html#section-4): stream/connection flow control and concurrency.
- [RFC9001 §§4.4,4.6](https://www.rfc-editor.org/rfc/rfc9001.html#section-4.4): authenticated peer and disabled early data; no custom cryptography.
- [Pinned QUIC transport](https://github.com/hyper-light/hyper-raft/blob/f116ac276d2d53296de24c2a43837baf66ed1cbd/crates/hyper-quic/src/config/transport.rs): explicit windows, retained allocation warning.
- [Pinned QUIC server config](https://github.com/hyper-light/hyper-raft/blob/f116ac276d2d53296de24c2a43837baf66ed1cbd/crates/hyper-quic/src/config/mod.rs): Incoming limits are not a live-connection cap.
- [Envoy v1.36.1 flow control](https://github.com/envoyproxy/envoy/blob/v1.36.1/source/docs/flow_control.md): propagate backpressure; independent blockage reasons. Its soft watermarks/defaults were not copied.
- Existing [relay research](../../managed-runtime-buffered-relays.md) compares Envoy/HAProxy/QUIC and records the remaining hard-accounting questions.

## Light validation

Fedora x86_64 Linux6.19.10, rustc1.99.1/LLVM23.1.1, kiln lane run r5:

```sh
CARGO_TARGET_DIR="$HOME/bp-mirror-g121/target" cargo test -j4 -p shards-remote --lib
CARGO_TARGET_DIR="$HOME/bp-mirror-g121/target" cargo clippy -j4 -p shards-remote --all-targets -- -D warnings
```

18 passed,0 failed (8 new parser/state cases); clippy exit0. The tests cover every
control split, malformed lengths/kinds, exact consumed offset, no early credit,
disconnect ownership/cursor retention, finish after app ACK and Debug content
minimization. Logs remain `~/managed-output-{units,clippy}.log`; exact tested hashes
are in `~/managed-output-light-source.log` and validation.md. Initial compile caught
an encode-loop mutable-borrow error; the encoder was corrected and rerun.

## Evidence account and coordinator work owed

- R01/R04/R06/R07/R13: checked protocol arithmetic, fallible owned vector growth,
  poison-tolerant locks and explicit states; coordinator review of implicit library
  allocation paths and affected legacy behavior still owed.
- R02/R03/R05/R12: primary alternatives above, shared native transport and the D180
  literals table. No new transport implementation, shell, host root or crypto.
- R08/R14: no cost/4–5×/allocation-free/leak-free claim. All CPU/allocation/copy/
  fault/kernel/thread/FD/latency/fairness comparison work is coordinator-owned.
- R09/R10: this is a new component. Existing capture red and fixed-green/mutation
  evidence belong to root; no old defect is claimed reproduced here.
- R11: Linux GNU compile/unit/clippy only. Other six targets and real KVM/HVF/WHP
  combinations remain unverified; paused Windows ARM VM untouched.
- R15/R16: scoped implementation/light tests only, no children/self-merges/cache
  deletion. Local heavyweight rebuild avoided because Mac disk headroom is small.

Still owed: combined daemon/private-IPC and active-plan validation; owner-only native
TLS+real-VM byte delivery; unauthorized/cross-scope/generation and replay guards;
forced partial I/O/slow consumer/reconnect/owner death; queue-full/ENOMEM/EMFILE/
handshake flood/socket blockage; deadlock/livelock/cleanup; meaningful guard-removal
mutations; host/kernel accounting and comparative profiling; full seven-target gate.
These are not passing merely because the component's unit tests passed.
