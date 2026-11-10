# Native managed-output comparison: sources and constraints

Policy loaded at `c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`;
implementer `/root/resource_performance`, coordinator `/root`. This is a private
measurement experiment, not a production architecture decision.

The measured product source starts at `dd169dce` (endpoint `b6866be5` plus the
activation-cache removal). The existing hyper-quic dependency is pinned to
`f116ac276d2d53296de24c2a43837baf66ed1cbd`. The standalone fixture lock records
its full dependency graph; comparison runs use the repository lock.

## Alternatives inspected

1. Current endpoint: two independently authenticated QUIC listeners, drivers and
   connections per active member, one stdout and one stderr channel. Each retains
   one unacknowledged application record and checks owner/run/generation/member/
   channel/cursor. Its existing `Service`, `Producer` and TLS constructors are the
   reference behavior, not a claim of low cost.
2. One existing Driver/listener/connection carrying independent bidirectional
   channel streams. RFC 9000 §§4/4.1 require both per-stream and aggregate connection
   credit. Per-stream credit alone cannot reserve sibling progress; the aggregate
   receive/send allowances must cover the admitted sum. Transport packet ACKs
   acknowledge processing/enqueueing rather than application consumption (§13.1),
   so both variants retain Shards' explicit application ACK. Cancellation uses
   STOP_SENDING/RESET_STREAM for a channel and the independent owner stop flag for
   the whole driver (§3.5). See [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000.html).
3. Established Quinn `quinn-0.11.9`: one endpoint can host multiple connections on
   one UDP socket; its transport API separately exposes stream and connection
   windows, send-window retention and stream fairness. The private fixture reuses
   Shards' current fork rather than adding Quinn's async runtime. See the pinned
   [endpoint](https://github.com/quinn-rs/quinn/blob/quinn-0.11.9/quinn/src/endpoint.rs)
   and [transport](https://github.com/quinn-rs/quinn/blob/quinn-0.11.9/quinn-proto/src/config/transport.rs)
   sources. These are compatibility/design references, not measured competitors.
4. Established quic-go `v0.54.1`: connection-scoped stream maps create independent
   per-stream flow controllers and handle stop/reset transitions. No Go runtime,
   new protocol or foreign crypto is introduced into Shards. See pinned
   [streams_map.go](https://github.com/quic-go/quic-go/blob/v0.54.1/streams_map.go).

## Scope of the throwaway adapter

The in-crate test hook loads `in_crate.rs`, which loads the measurement source.
`src/shared.rs` directly uses the current private `Service`, `input`, `transport`,
`Producer` and slot state; it does not copy their protocol/security implementation.
It dispatches the initial existing Subscribe by Scope, then routes that stream to
its existing Service. Per-channel retained allocations and application ACK rules
are unchanged. The shared connection's transport receive/send windows and command/
datagram bounds derive from the sum of the fixture's admitted channel allowances.
No measurement changes a grant or selects a production constant.

**Known non-equivalence:** malformed/stale/duplicate subscriptions and Service
refusals still close the shared connection. The endpoint reference closes only
that channel's independent connection. Thus this adapter can measure the common
valid-owner record/ACK/stall/owner-stop path, but cannot select a production
architecture or certify equivalent hostile-input/failure containment. It remains
throwaway pending a researched per-stream typed-refusal interface and live proof.
No shared-listener performance advantage would waive that gap.

## Resource method

The complete server, publisher, subscriber and control threads are in the measured
process. Phase snapshots include process RSS/PSS/private mappings, task and FD
counts, getrusage CPU and minor/major faults. Driver socket buffer allowances are
reported separately from actual socket accounting. Linux SO_MEMINFO is read for
each unique socket inode: duplicate FDs do not multiply kernel charges. Its nine
fields follow the [Linux v6.19 sock_diag uapi](https://github.com/torvalds/linux/blob/v6.19/include/uapi/linux/sock_diag.h)
and [SO_MEMINFO implementation](https://github.com/torvalds/linux/blob/v6.19/net/core/sock.c);
fields are retained individually rather than added despite overlapping semantics.
Global sockstat, load/CPU pressure and accessible cgroup statistics are context,
not claimed exclusive tenant attribution. Missing memory-controller files remain
an attribution gap. Socket structure/slab costs beyond SO_MEMINFO remain unmeasured.

The optional System counting allocator observes Rust allocations, reallocations,
frees, requested bytes and retained requested bytes. TLS C allocations and actual
allocator arena retention are not covered by it; RSS/PSS cover their combined
resident effect. It does not count copies. Instrumented and plain runs are kept
separate, and libtest overhead is explicit. Phase resource inspection/output occurs
after the phase's allocator snapshot; snapshots can affect later allocator reuse,
so retained-memory differences are not a leak-freedom proof.

No production optimization or 4–5×/zero-allocation/leak-free claim follows from
the sources, API shape or the first successful TLS run.
