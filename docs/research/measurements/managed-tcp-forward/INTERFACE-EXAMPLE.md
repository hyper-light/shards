# TCP forwarding: interface check, without a relay implementation

Base: `eb5654d6f5356cacaca536ff51f14a632fa7e801`. This example checks
existing signatures against a private copy of the actual guest module. It
neither implements a pump nor invokes an I/O operation. The proposed adapter
semantics below still need ROOT review and actual native/guest evidence.

## Checked surface

The following Rust block is appended to the **private check copy** of
`crates/testguest/src/managed_network.rs`. No repository Rust file changes.
The function only assigns function pointers; there is no call to it and no
socket, namespace, thread, cancellation or VM test runs in this check.

```rust
mod tcp_interface_check {
    use super::*;

    enum Attempt<T> {
        Ready(T),
        WouldBlock,
        NativeError(io::Error),
    }

    fn signatures() {
        type AttemptFn = fn() -> io::Result<Attempt<usize>>;
        let _: fn(&Deadline, RawFd, i16, AttemptFn) -> io::Result<Attempt<usize>> =
            Deadline::io::<Attempt<usize>>;
        let _: fn(&Deadline) -> io::Result<Duration> = Deadline::left;
        let _: fn(i16) -> io::Result<()> = Deadline::cancellation_event;
        let _: fn(&TcpStream, bool) -> io::Result<()> = TcpStream::set_nonblocking;
        let _: fn(&TcpStream) -> io::Result<Option<io::Error>> = TcpStream::take_error;
        let _: fn(&TcpStream) -> io::Result<SocketAddr> = TcpStream::peer_addr;
        let _: fn(&TcpStream) -> io::Result<SocketAddr> = TcpStream::local_addr;
        let _: fn(&TcpStream, Shutdown) -> io::Result<()> = TcpStream::shutdown;
        let _: fn(&mut TcpStream, &mut [u8]) -> io::Result<usize> =
            <TcpStream as Read>::read;
        let _: fn(&mut TcpStream, &[u8]) -> io::Result<usize> =
            <TcpStream as Write>::write;
        let _: fn(OwnedFd) -> TcpStream = TcpStream::from;
        let _: fn(SocketAddr, i32, SocketMode) -> io::Result<OwnedFd> = bound;
        let _: unsafe extern "C" fn(
            libc::c_int,
            *const libc::sockaddr,
            libc::socklen_t,
        ) -> libc::c_int = libc::connect;
        let _: unsafe extern "C" fn(
            *mut libc::pollfd,
            libc::nfds_t,
            libc::c_int,
        ) -> libc::c_int = libc::poll;
    }
}
```

This establishes whether `Deadline::io` can return a typed native-attempt result
without a new admission API, and whether the required native operations exist
at this exact source/toolchain. It does not establish the proposed control flow,
error mapping, absence of spin, forwarding behavior or cleanup.

## Proposed use of those signatures — not compiled behavior

For an actual nonempty read/write attempt, the candidate closure returns
`Ok(Attempt::Ready(n))` for the actual native count,
`Ok(Attempt::WouldBlock)` only for an actual native `WouldBlock`, and
`Ok(Attempt::NativeError(error))` for other native failures. An actual read/write
`Interrupted` remains an outer error so existing `Deadline::io` rechecks original
expiry and pending cancellation before retry. The inner error distinguishes a
native socket abort from the existing admission seam's cancellation outcome.
No operation is fabricated to produce `WouldBlock`.

The owner can then wait for both streams and its cancellation descriptor together,
instead of entering `Deadline::io`'s single-stream `WouldBlock` wait. A native
`poll` attempt uses the same admission seam, derives its timeout from `left()`
**inside each attempt**, and checks original expiry followed by
`cancellation_event` after returning. The timeout is the remaining protocol
deadline, with the existing millisecond conversion, not a periodic wake interval.
The two stream entries and cancellation entry follow the topology; they are not
a chosen queue depth. A disabled stream entry uses the native negative-FD
convention, since `events = 0` still reports HUP/ERR/NVAL.

Connect has separate proposed states. One nonblocking native `connect` returns
Connected, `EINPROGRESS`/ConnectPending, or its actual error; readiness then needs
`take_error` and a known peer, as the existing packet-stack caller does. The
example does not provide a connect adapter. In particular, do not feed connect's
`EINTR` into a generic reconnect/retry: retain that attempt's uncertainty and end
the owned attempt without replaying it. No automatic redial or address fallback.

`Deadline::io`'s already-pending cancellation check remains non-consuming.
Expiry wins when expiry and cancellation are observed together. Its extra
readiness observation is a syscall per admitted attempt; ROOT must measure the
cost. Cancellation arriving after that check can still race with real I/O.
Neither this interface nor an original-deadline check makes external effects
atomic or rolls back a previously accepted byte prefix.

## Caller envelope example

The future harness supplies numeric fields explicitly; there are no fallback
production sizes, wait durations, port numbers or flow limits in this packet.

| Supplied value | Check before fixture effects |
| --- | --- |
| One original operation duration | Nonzero and `Instant::checked_add`; pass the same absolute `at` into all owned workers and waits. |
| Outer host timeout / boot envelope | Finite, supplied separately; expiry is recorded separately from guest operation expiry. |
| Request, greeting and final-tail byte strings | Exact lengths, bounded hex decoding and checked response-length addition; bytes are opaque. |
| Pending-to-backend and pending-to-client storage | Nonzero for exercised directions; fallible reservation before namespace/listener/actor setup; account actual capacities without increasing caller authorization. |
| Flow envelope | Exactly the one scripted flow this experiment implements; refuse any other count rather than silently broadening it. Aggregate production admission remains open. |
| Observation/control byte envelope | Checked separately from payload slots; reserve records and finite private handshakes before actor release. |
| Mark/table/rule priority | Existing checked caller fixture inputs; no new exemption or security grant. |
| Kernel, effective config, CLI/init/testguest/helper paths and hashes | Match the exact combined source and caller-supplied artifacts before execution; no kernel pin/profile change. |

Pending payload bounds are separate from total script bytes: a larger finite
script may pass through smaller slots under backpressure. Byte counters use
checked arithmetic. A full pending buffer disables that source read until actual
destination writes free capacity; it does not grow, discard bytes, admit another
flow or block the opposite direction and cancellation. Kernel socket buffers,
allocator rounding and induced parent work still require admission/accounting
evidence; a payload-length comparison alone supplies none of those proofs.

## Check record

Actual private GNU and musl `cargo check --lib` results are both **0**, and
`scripts/check-development-policy` is **0**. Source, commands, complete small
logs and hashes are retained in [SOURCE-IDENTITIES.json](SOURCE-IDENTITIES.json).
The 1,597-byte Rust block above has SHA256
`2a3afa5e3f9e0ef32a01e1dde8b9d447511b6e64d935270624ce8da49b6afa7b`.

Private runner: Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`;
`/home/adalundhe/relay-tcp-proposal-eb565-light-v2`. Source/target/cargo-home
are new private paths, with `sync:false`, one-CPU affinity, 1GiB address-space
limit and 2GiB admitted disk. Actual private allocation at completion was
25,477,120 bytes. No ROOT source/cache was written; no LOCAL target/cache exists.

The first private copy under `...-light-v1` failed both checks101 with E0583:
my external `#[path]` wrapper resolved `mod intercept` from the wrong directory.
Its source/logs/results remain preserved. Version2 corrects only the checker
crate's root/module layout; no original guest source or interface assertion was
fixed. This was a check-scaffolding failure, not a Shards runtime failure.

To reconstruct the check, copy the frozen guest `managed_network.rs` and its
submodule tree, plus the existing devcgroup source, to a new private source
directory. Append exactly the Rust block to that parent module. Put a private
crate root beside `managed_network.rs` containing only `#![allow(dead_code)]`
and `mod managed_network;`; point the separate minimal check package's `[lib]`
at that root. Its only dependencies are exact libc0.2.189 and the copied
devcgroup source; its minimal devcgroup manifest points at the original copied
`lib.rs`. The exact private manifests and lockfile hashes are in the identity
record. The policy script runs over unchanged copied repository policy,
Cargo.toml, target list and CI/lint files, independently of that minimal package.

The actual check commands, from the private `check` directory, are:

```sh
rustup run 1.98.1 cargo check --lib --target x86_64-unknown-linux-gnu -j 1
rustup run 1.98.1 cargo check --lib --target x86_64-unknown-linux-musl -j 1
```

`CARGO_HOME`, `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS=1`, `CARGO_INCREMENTAL=0`
and `NUM_JOBS=1` name that private scope. The exact compiler is
rustc1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8;
Cargo1.98.1 `797e8a9bc`. These commands generate check metadata, without linking
or executing a forwarding/test binary. This is not the normal host/helper
workspace build, its full lint/matrix, or native syscall behavior/cleanup proof.
