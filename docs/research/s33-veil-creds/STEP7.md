# Step 33, step 7, slice (a): HTTP/2's layer for the injector (frames, HPACK, flow control, the request rewrite)

Kiln goal "#74 step 7 slice (a)" (plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 7 of the goal's plan
01a1d2b88be57304b972761e2a5c6781; research 01a1d2b88be57304bf6b081fe46832e0: f10, f22, f41, f42, f58, f67). Role:
implementer for ROOT (g121). Policy read at origin/main ff08eca25: AGENTS.md sha256 f66d304c…, docs/development-rules.md
df2acf47…, docs/development-evidence.md 61613a24…, and /tmp/g121-shot/brief-common.md. Worktree `s33-h2a`, branch
`p/s33-h2a-v1`. Main moved to 66e8fcb74 while this slice was built (step 10 part B, D251, the VM process's Mac to
shards-ipc); the commits below are on 66e8fcb74, this slice's changes to lib.rs and inject.rs applied to main's. The
design and its decisions are D242's "Built: step 7's first slice"; this file is the light-test record.

## Change

- `crates/net/src/hpack.rs` (new): RFC 7541. `Field` (name, value wiped as it drops, `Indexing`: how it was sent),
  `Decoder` (its peer's table, this side's SETTINGS_HEADER_TABLE_SIZE as its limit; a block decoded whole, its fields
  dropped past its caller's bound, `Decoded::TooLarge`), `Encoder` (its own table up to the peer's limit and this
  side's other hop's; size updates at a block's start, the least first), integers (§5.1), strings and Huffman (§5.2,
  Appendix B), the static table (Appendix A). Typed `Error`s, no panic path.
- `crates/net/src/http2.rs` (new): RFC 9113. `frame`, `Head`, `Settings` (§6.5.2), `Hop` (each side's windows, the
  peer's SETTINGS and WINDOW_UPDATEs, `send_data`, `received`, `released`, `preface`, and the hop's own `Decoder` and
  `Encoder`), `Block` (HEADERS and CONTINUATION assembled), `headers` (a block split for the peer's frame size),
  `request` (RFC 9113 §8.2–8.3's checks, `credential::decide`, `Request::{Go, Misdirected, Malformed}`), and
  `Go::encode` (each value in its placeholder's place, every credential field never indexed). Typed `Error` with its
  `Scope` (connection or stream) and `Code` (§7).
- `crates/net/src/inject.rs`: `token_in` and `authority_host` taken out of `token` and `request_host`, so that the
  HTTP/1.1 path and HTTP/2's read a token and a host alike (R12).
- `crates/net/src/lib.rs`: `pub mod hpack; pub mod http2;`.
- `scripts/step7-h2-survey.sh` and its output `evidence/step7/h2-survey.txt`: the implementations considered (R02).
- `scripts/step7-hpack-tables.py`: HPACK's tables from RFC 7541's text (`evidence/standards/rfc/rfc7541.txt`).

## Decisions (D242 records each with its alternatives and evidence)

- Our own frame codec and HPACK; no crate added. `h2` and `loona-hpack` lose a field's never-indexed flag, which RFC
  7541 §7.1.3 holds an intermediary to; `hpack` is unmaintained and panics on input; `httlib-hpack` is unmaintained
  since 2020 and slices unchecked input; `h2` also adds 9 crates new to the lockfile (itself and tokio among them), chosen limits
  (R03) and an async endpoint where the injector is sans-I/O (evidence/step7/h2-survey.txt).
- Each hop its own HPACK contexts and windows; a request's block decoded, filled and re-encoded, never patched.
- Every credential field, filled or not, re-encoded never indexed, its value raw (not Huffman-coded).
- Windows granted are what this side holds (its caller's, from tcp.rs' `TO_HOST`/`TO_GUEST`), in force once acked;
  credit returned once min(a frame, half the window) is owed (RFC 9113 §6.9.1, RFC 1122 §4.2.3.3).
- A Huffman code is used where it is no longer than the octets, as RFC 7541's own examples are; held to C.4 and C.6.

## Tests

One host: adas-mac-studio.local (hostname Adas-Mac-Studio-3.local), macOS arm64, cargo 1.98.1, through the carrier
`.claude/worktrees/s33-h2a-carrier` (`run.sh`, `check.sh`: `CARGO_TARGET_DIR=~/g121-tmp/s33-h2a/target`,
`SHARDS_HELPERS=skip`, `CARGO_INCREMENTAL=0`; each step's whole output in `~/g121-tmp/s33-h2a/step-<name>.log`, read
with `grep -a`), kiln run r828: `bash run.sh p/s33-h2a-red test; bash run.sh p/s33-h2a-v1 test clippy-macos
clippy-linux clippy-windows`.

- **Red, at 8c7db1498** (the tests, the layer's entry points stubbed): `cargo test -p shards-net --lib -- hpack::
  http2:: inject::`: 11 failed, 15 passed, 2 ignored, 99 filtered out. Failed: `http2::tests::`
  `a_placeholder_in_a_header_block_comes_out_filled_and_never_indexed` and `an_ungranted_hosts_block_passes_as_it_came`
  (`Malformed("not built yet")`), `each_hops_dynamic_table_stays_its_own_across_blocks` (nothing decoded),
  `flow_control_holds_a_sender_past_its_peers_window` (no SETTINGS ACK),
  `a_peer_past_this_sides_window_is_refused_and_credit_comes_back_as_its_data_goes` (no preface),
  `a_block_for_a_host_not_the_sni_is_misdirected`; `hpack::tests::` `rfc_7541s_examples_decode_and_encode_as_written`,
  `a_field_sent_never_indexed_stays_never_indexed_and_out_of_every_table`, `integers_are_rfc_7541s_examples`,
  `malformed_blocks_are_refused`, `a_block_past_its_bound_is_decoded_whole_and_its_fields_dropped`. Passed: inject.rs'
  13, and hpack's `the_tables_are_rfc_7541s` (data) and `a_fields_value_never_prints`. Ignored: step 10 part B's
  measurement (`inject::ab`).
- **Green, at b1e41f7d9**: the same command, 26 passed, 0 failed, 2 ignored. `cargo clippy -p shards-net --all-targets
  -- -D warnings` clean for aarch64-apple-darwin, x86_64-unknown-linux-gnu (`CC_x86_64_unknown_linux_gnu=scripts/
  zig-cc`, the pinned zig, as scripts/lint drives it) and x86_64-pc-windows-msvc (`cargo xwin clippy`), each log
  saying `Checking shards-net` and `Finished`.
- **Before them**, while the code was built (kiln runs r798 and two after it on adalundhe-macbook-pro, which has
  neither zig nor xwin, so the record is the studio's): a test's local shadowing its helper, two clippy lints
  (`is_multiple_of`, `as_chunks`), the tables' test reading `'('`'s ASCII form as its number, and C.6.2's `307`,
  which RFC 7541's encoder writes in Huffman's 3 octets where this one first wrote its 3 octets raw (now Huffman where
  no longer); each fixed before the commits above.
- **R10**: the red and green above are the unit tests' own. The plan's mutation (Authorization indexed) is owed to
  ROOT below, mutation runs being ROOT's under brief-common.

## Slice (b): wiring it into the injector's connections (for the next agent)

1. **ALPN, decided by the guest's offer (f41).** Keep the guest's ClientHello held (rustls `Acceptor`, as now) and
   open the host's TLS offering what the guest offered of `h2` and `http/1.1` (`ClientConnection::new_with_alpn`, rustls 0.23.46
   client_conn.rs:741);
   once the host's handshake is done, accept the guest with the protocol the host chose (`guest_config` taking it),
   so each connection's two hops speak one protocol and the injector offers only what it implements. A guest offering
   no ALPN, or neither, is HTTP/1.1. Its cost is none past today's: a request already waits for the host's TLS.
2. **An HTTP/2 flow state (R04)** beside `Ended`'s HTTP/1.1 states: the guest's preface (§3.4) and SETTINGS read; this
   side's `Hop::preface` on each hop, `Holds` derived from the connection's windows (`TO_HOST` for the guest's hop's
   DATA, `TO_GUEST` for the host's); the client preface and a SETTINGS with ENABLE_PUSH 0 to the host.
3. **The frame loop on each hop.** SETTINGS (`Hop::settings`, its ACK queued), WINDOW_UPDATE (`window_update`, then held
   DATA flushed), PING answered on its own hop (§6.7), GOAWAY carried to the other hop with its last stream, RST_STREAM
   carried by the stream's mapping, PRIORITY passed over (§5.3.2 deprecates it), PUSH_PROMISE a PROTOCOL_ERROR (push
   off), unknown types ignored (§5.5), HEADERS and CONTINUATION through `Block` (any other frame between: `continued`'s
   PROTOCOL_ERROR), DATA through `received`, the other hop's `send_data` as its windows allow, and `released` as it
   leaves (its padding released at once).
4. **Streams.** Each stream's state as §5.1's (idle, open, half-closed, closed) in an enum; host stream ids given in
   the order requests reach the host, as §5.1.1 needs them rising and a request parked on a lease must not hold back
   another; the host's SETTINGS_MAX_CONCURRENT_STREAMS bounding how many are open, the rest waiting.
5. **Requests.** `http2::request` on each block: `Misdirected` answered `:status 421` with END_STREAM on that stream
   alone, encoded by the guest's hop's encoder; `Malformed` a RST_STREAM PROTOCOL_ERROR; `Go`: each of `references()`
   asked of `Leases::ask` with the connection's waiter, the stream parked while one is asked and resumed by
   `Leases::resumed`; then `Go::encode` with `leases.held` and `headers` at the host's frame size, each fill audited
   (`Did::Filled`); a refused lease answered 503 on that stream; `inject::mints` 403. A sign-in's token host (step 8)
   as the HTTP/1.1 path does it: `identity` asked, a token POST and its response read whole per stream.
6. **Responses.** The host's blocks decoded by the host's hop (`Block::decode`), each value filled into the stream's
   request masked in its fields and its DATA (inject.rs' `Mask`, one per stream, across DATA frames), re-encoded by
   the guest's hop's encoder; trailers alike.
7. **Bounds.** A block's `most` is what the connection holds less what it already holds (`Conn::held`, within
   `TO_HOST`), as an HTTP/1.1 head's; a decoded list past it answered 431 on its stream (§10.5.1), the block decoded.
   `http2::Error`'s `Scope::Connection` a GOAWAY with its code (the other hop closed after its streams end),
   `Scope::Stream` a RST_STREAM.
8. **Tests (the plan's light integration test).** A rustls guest offering `h2` and a local HTTP/2 server over TLS
   trusting a test root (written over `http2.rs` and `hpack.rs` in the test, as no h2 crate is in the tree): a granted
   stream's request arriving with the value and its Authorization never indexed, its echo masked; a concurrent
   stream and a large DATA body under a small window unaffected; a `:authority` not the SNI answered 421 while
   another stream on the connection completes; a guest offering `h2` to a host speaking `http/1.1` alone served
   HTTP/1.1. tcp.rs' handoff unchanged.

## Owed

- Slice (b), above.
- ROOT: CI's matrix on main; a mutation that indexes Authorization (`perl -0pi -e 's/field\.indexing =
  Indexing::Never;/{}/' crates/net/src/http2.rs`, then `cargo test -p shards-net --lib http2::tests::
  a_placeholder_in_a_header_block_comes_out_filled_and_never_indexed`, which must fail), mutation runs being
  ROOT's under brief-common; the real-VM runs of step 10 once slice (b) lands.
