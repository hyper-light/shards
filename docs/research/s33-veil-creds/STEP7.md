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

## Owed (slice (a))

- Slice (b): built, below.
- ROOT: CI's matrix on main; a mutation that indexes Authorization (`perl -0pi -e 's/field\.indexing =
  Indexing::Never;/{}/' crates/net/src/http2.rs`, then `cargo test -p shards-net --lib http2::tests::
  a_placeholder_in_a_header_block_comes_out_filled_and_never_indexed`, which must fail), mutation runs being
  ROOT's under brief-common.

# Slice (b), built: HTTP/2 in the injector's connections

Kiln goal "#74 step 7 slice (b)". Role: implementer for ROOT (g121). Policy read at origin/main a70aa132b: AGENTS.md
sha256 f66d304c…, docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…, and
/tmp/g121-shot/brief-common.md. Worktree `s33-h2b`, branch `p/s33-h2b-v1`, on main a70aa132b. Its design and
decisions are D242's "Built: step 7's second slice"; this is the light-test record.

## Change

- `crates/net/src/inject.rs`:
  - `Opening` holds the guest's ClientHello while the host's TLS is made offering what the guest offered of `h2` and
    `http/1.1`; `Flow::open` then accepts the guest speaking the host's choice.
  - `H2` is the HTTP/2 flow: `Phase` for each hop, `StreamState` (§5.1), `Upward` (parked, sent, answered), `Last`
    and `Down`, the frame loop on each hop, requests through `http2::request`, responses masked and re-encoded, and
    its bounds.
  - `Ended::new` now takes the TLS `Opening` made; `guest_config` takes the protocol chosen.
  - A ClientHello with no protocol or no suite in common is refused at once.
- `crates/net/src/http2.rs`:
  - The remaining frame types and error codes, and SETTINGS_MAX_CONCURRENT_STREAMS.
  - `End`: a client's preface, with SETTINGS_ENABLE_PUSH 0.
  - `data` (DATA's padding taken off), `rst_stream`, `goaway`, `ping_ack`, `word` and `Block::held`; `connection`
    and `MOST_STREAM` made public.
- Tests in inject.rs:
  - An HTTP/2 end over http2.rs and hpack.rs (`Peer`), which holds its peer to its windows and gives credit back.
  - A host speaking HTTP/2 alone over TLS (`h2_host`), and three tests.

## Tests

One host: adas-mac-studio.local (Adas-Mac-Studio-3.local), macOS arm64, cargo 1.98.1, through the carrier
`.claude/worktrees/s33-h2b-carrier` (`run.sh` clones the repo into `~/g121-tmp/s33-h2b`, fetches the bundle, and runs
`check.sh`; `CARGO_TARGET_DIR=~/g121-tmp/s33-h2b/target`, `SHARDS_HELPERS=skip`, `CARGO_INCREMENTAL=0`). The command
is `cargo test -p shards-net --lib -- hpack:: http2:: inject::`.

- **Red, at 6ad258c96** (the tests and http2.rs' helpers; the injector still offering HTTP/1.1 alone), kiln run r903:
  2 failed, 27 passed, 2 ignored.
  - `an_h2_guests_granted_stream_arrives_filled_never_indexed_and_its_echo_comes_back_masked` and
    `an_h2_stream_for_another_host_is_answered_421_while_another_completes` failed: the guest was served HTTP/1.1, and
    its HTTP/2 end read the response as a frame past SETTINGS_MAX_FRAME_SIZE.
  - `a_guest_offering_h2_to_an_http_1_1_host_is_served_http_1_1` passed, as HTTP/1.1 alone already served it; it
    guards that path.
- **On the way** (r903 and the runs after it, each fixed before green):
  - `a_guest_offering_no_aes_256_gcm_is_refused` timed out: a guest was accepted only after the host's handshake,
    which that test never gives. The fix refuses a ClientHello with no suite in common at once.
  - The 1 MiB stream came back as `502 "the host's side failed: received plaintext buffer full"`: `H2::host_bytes`
    processed every record before taking any plaintext, past rustls's own bound. Its plaintext is now taken record
    by record. A close_notify is acted on only after the frames before it are read, and a host that ends lets
    responses that came whole finish.
- **Green, at the wiring's commit**: 29 passed, 0 failed, 2 ignored (step 10 part B's measurement, `inject::ab`).
- **Clippy**: `cargo clippy -p shards-net --all-targets -- -D warnings` on aarch64-apple-darwin,
  x86_64-unknown-linux-gnu (`CC_x86_64_unknown_linux_gnu=scripts/zig-cc`) and x86_64-pc-windows-msvc (`cargo xwin
  clippy`).

## Guard-removal mutations (ROOT's to run; each must fail the test named)

Run each on the green tree with `cargo test -p shards-net --lib -- inject::tests::<test>`, then `git checkout --
crates/net/src`. Touch the file after restoring it.

1. Credential fields never indexed. Fails `an_h2_guests_granted_stream_arrives_filled_never_indexed_and_its_echo_comes_back_masked` ("the credential was not sent never indexed"):
   `perl -0pi -e 's/field\.indexing = Indexing::Never;/{}/' crates/net/src/http2.rs`
2. A response's fields masked. Fails the same test ("the echo in a field was not masked"):
   `perl -0pi -e 's/f\.value = masked\(&mut s\.mask, &f\.value, run\);/let _ = &mut s.mask;/' crates/net/src/inject.rs`
3. DATA masked. Fails the same test ("the echo in DATA was not masked"):
   `perl -0pi -e 's/let _ = mask\.push\(data, true, run, &mut out\);/let _ = out(data);/' crates/net/src/inject.rs`
4. 421 on its stream alone. Fails `an_h2_stream_for_another_host_is_answered_421_while_another_completes`:
   `perl -0pi -e 's/self\.answer\(gid, Refusal::Misdirected \{ sni, host \}\);/let _ = (sni, host); return Err(http2::connection(Code::Protocol, "mutant"));/' crates/net/src/inject.rs`
5. ALPN from the host's choice. Fails both HTTP/2 tests:
   `perl -0pi -e 's/None => host\.alpn_protocol\(\) == Some\(H2\),/None => false,/' crates/net/src/inject.rs`
6. Plaintext taken per record. Fails the first HTTP/2 test with the 502 seen above:
   `perl -0pi -e 's/if plain\(&mut self\.host, &mut self\.from_host\) \{\s*break;\s*\}//' crates/net/src/inject.rs`
7. A ClientHello with no suite in common refused at once. Fails `a_guest_offering_no_aes_256_gcm_is_refused` after its 10 s deadline (expect an unused-variable warning):
   `perl -0pi -e 's/if offered\.is_empty\(\) \|\| !suite \{/if offered.is_empty() {/' crates/net/src/inject.rs`

## Owed

- ~~A sign-in's token host over HTTP/2~~: built (#74 steps 7 and 8, 3ad5e9d3a): a token host is offered what its
  guest offered, and step 8's token handling runs per stream. Its record is STEP8.md's "A sign-in's token host over
  HTTP/2".
- ~~503 (a refused lease)~~: covered, and its livelock fixed, below ("A refused lease answered 503").
- Not covered by a light test here: 403 (`mints`), 431, a host's GOAWAY and RST_STREAM carried,
  and SETTINGS_MAX_CONCURRENT_STREAMS holding a stream back. Each is a code path in `H2`, typed and bounded, and none
  is exercised yet.
- ROOT:
  - CI's 8-target matrix on main.
  - The mutations above.
  - The real-VM runs: an api run with a credential grant toward a host that speaks HTTP/2 (a harness whose client
    offers `h2`, or `curl --http2` in the guest), on KVM, HVF and WHP, under step 10's leak scan, with the audit lines
    of each fill.

# A refused lease answered 503, on its stream or connection

Kiln goal "#74 credentials" (plan step 33), step 7. Role: implementer for ROOT (g121). Policy read at origin/main
7491a957e: AGENTS.md sha256 f66d304c…, docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…,
and /tmp/g121-shot/brief-common.md. Worktree `s33-lease-refused`, branch `p/s33-lease-refused-v1`, on main 503c96677.
Its decision is D242's "Built: a refused lease answered on what asked it, step 7".

## Defect

A refused lease reached `Leases` as a `Resumed` with an `Err` outcome. lib.rs' `resume_flows` kept the waiter alone,
`Leases::answered` had dropped the lease, and the resumed flow asked again: `H2::go` (HTTP/2) and `Ended::go`
(HTTP/1.1) alike, a new ask each time the broker refused, never answered 503. Found by the HTTP/2 coverage lane (kiln
run r963 ran past kiln's hour); its red test is e418b65c3, carried into this slice's red commit.

## Change

- `lease::refused`: the refusals among `Leases::resumed` of the asks one flow waited on, by reference.
- `inject::Run::resumed`: the run a resumed flow is served with, those refusals in its `refused`. `resume_flows` and
  the test benches' `Bench::resume` both build it.
- `inject::Run::ask`: a reference refused in this resume answered with its refusal, else `Leases::ask`. `Ended::go` and
  `H2::go` call it, and answer its `Asked::Refused` as before: `Unleased`, 503.
- Nothing is kept past the resume: a later request asks anew. No timer, no constant.

## Tests

One host: adas-mac-studio.local (Adas-Mac-Studio-3.local), macOS arm64, through the carrier /tmp/s33lr-carrier
(`run.sh` clones the repo blobless into `~/g121-tmp/s33-lease-refused`, fetches the bundle;
`CARGO_TARGET_DIR=~/g121-tmp/s33-lease-refused/target`, `CARGO_INCREMENTAL=0`). The command is
`cargo test -p shards-net --profile test-release --lib -- inject:: lease:: signin::`.

- **Red, at 853456f78**, kiln run r1122: 58 passed, 2 failed, 2 ignored.
  - `an_h2_stream_whose_lease_is_refused_is_answered_503_while_another_completes`: "a refused lease was asked again:
    its stream was never answered".
  - `a_request_whose_lease_is_refused_is_answered_503_and_a_later_one_asks_anew`: "a refused lease was asked again:
    its request was never answered". The HTTP/1.1 path had the same livelock.
- **Green, at 310cbf3b7** (main fe06f5ab6 merged), kiln run r1139: 61 passed, 0 failed, 2 ignored.
- **On the way**: r1122's green did not build (ab.rs, included by path, named no `refused`); r1138 failed
  `a_resume_carries_each_flows_refusals_to_it` alone, its helper `the_ask` holding one ask where the test made two (the
  test fixed in 310cbf3b7; both 503 tests passed there).
- **Clippy** `-p shards-net --all-targets -- -D warnings`: aarch64-apple-darwin, x86_64-unknown-linux-gnu
  (`CC_x86_64_unknown_linux_gnu=scripts/zig-cc`) and x86_64-pc-windows-msvc (`cargo xwin clippy`), clean.

## Guard-removal mutations (ROOT's to run; each must fail the tests named)

Run each on the green tree with `cargo test -p shards-net --profile test-release --lib -- inject:: lease::`, then
`git checkout -- crates/net/src` and touch the file.

1. The refusal not answered from the resume. Fails both 503 tests ("a refused lease was asked again"):
   `perl -0pi -e 's/if let Some\(\(_, why\)\) = self\.refused\.iter\(\)\.find\(\|\(r, _\)\| r == reference\) \{/if let Some((_, why)) = self.refused.iter().find(|_| false) {/' crates/net/src/inject.rs`
2. The resume carrying no refusal. Fails both 503 tests and `a_resume_carries_each_flows_refusals_to_it`:
   `perl -0pi -e 's/for r in resumed\.iter\(\)\.filter\(\|r\| r\.waiter == waiter\)/for r in resumed.iter().filter(|_| false)/' crates/net/src/lease.rs`
3. `Run::resumed` dropping what `lease::refused` gave (the `resume_flows` path). Fails both 503 tests:
   `perl -0pi -e 's/refused: lease::refused\(resumed, waiter\),/refused: { let _ = resumed; Vec::new() },/' crates/net/src/inject.rs`
4. A refresh asked outside left parked to go upward (the test waits' red, STEP8.md "The injector's test waits"). Fails
   `an_h2_refresh_is_answered_outside_on_its_stream_with_placeholders` and
   `a_refused_h2_refresh_is_answered_on_its_stream_while_another_completes` at once, "the refresh reached the host"
   (run r1206, the whole lib in 1.26 s; before, a hang of over an hour):
   `perl -0pi -e 's/s\.up = Upward::Refreshing;/s.up = Upward::Parked(go);/' crates/net/src/inject.rs`

## Owed

- ROOT: CI's 8-target matrix on main; the mutations above; a real-VM run whose broker refuses a granted lease (an api
  run on KVM, HVF and WHP), its guest answered 503 once, over HTTP/1.1 and HTTP/2.

# Real-VM tests of steps 7 and 8, written for ROOT to run

Kiln goal "#74 credentials" (plan step 33), steps 7 and 8. Role: implementer for ROOT (g121). Policy read at origin/main
70eba153d: AGENTS.md sha256 f66d304c…, docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…, and
/tmp/g121-shot/brief-common.md. Worktree `s33-vm-tests`, branch `p/s33-vm-tests-v1`. Written, compiled and linted; not
run (real-VM runs are ROOT's).

## Tests (`crates/shards/tests/api_image.rs`)

- `an_h2_request_to_a_granted_host_is_filled_never_indexed_and_its_echo_comes_back_masked`: a grant toward the host
  itself; the harness sends its placeholder over HTTP/2 as a bearer token, then as `x-api-key` with a body, to the
  test's host speaking `h2`. The host sees the value, each field never indexed (RFC 7541 §7.1.3); the echoes, in a
  field and in DATA, come back masked; the value is in neither the harness's output nor the run's home; every lease
  is released by the run's end.
- `a_refused_lease_is_answered_503_over_http_1_1_and_h2_and_a_later_request_asks_again`: the lessor refuses its first
  two asks. The first request (HTTP/1.1) and the second (an h2 stream) are each answered 503, and neither reaches
  the host. The third (h2) asks anew, three asks in all, and is filled and echoed masked.
- `an_h2_sign_in_gives_the_harness_placeholders_and_its_refresh_is_answered_outside`: Codex's sign-in, with the
  test's host playing `auth.openai.com`. The exchange over h2 reaches the host, which is asked `identity`. Its
  tokens are captured by the lessor, and the harness holds the capture's placeholders alone. The refresh never
  reaches the host: the lessor answers it, again with placeholders. The access token's use is then filled with the
  refreshed token toward the host itself, never indexed, and its echo is masked. No token reaches the harness's
  output or the run's home.
- The harness is the test guest's new `tls-ask` mode (`crates/testguest/src/tls_ask.rs`): HTTP/2 or HTTP/1.1 per
  request over TLS trusting the run's bundle, HPACK through shards-net's `hpack.rs` included by path. The host is
  the test's own, on this host's loopback (`tls_host`: h2 and HTTP/1.1 under a test CA, HPACK through
  `shards_net::hpack`); the run reaches it as `host.docker.internal`. Waits end on the harness's own end (its
  stdout read to its close, `assert_ended`); the host's records are read after it, with no deadline of their own.
- No vendor is asked. The sign-in's guest dials the test's host and names `auth.openai.com` in its SNI alone.
  The injector picks a flow by its SNI (inject.rs `hello`) and reaches the address the guest dialed, and its
  upstream TLS verifies that name against the test's root.

## Where they run

- **Linux (KVM)**: the run host's environment carries `SSL_CERT_FILE` naming the test's CA. Its network process
  inherits it, and its verifier reads it as it starts (rustls-platform-verifier 0.7.1 on Linux loads roots through
  rustls-native-certs 0.8, which takes `SSL_CERT_FILE` over the system's bundle).
- **macOS (HVF) and Windows (WHP)**: each test prints `SKIP:` and returns. The injector's upstream side is the OS's
  verifier (shards-verify: trustd, CryptoAPI), and production builds `Upstream::new(Vec::new())`
  (`crates/net/src/lib.rs` `Prepared::new`), so nothing a test sets adds a root. These runs stay **unverified**
  there until a run's injector can be given a test's root. That seam is a D242 decision, not taken here.

## Checks (adas-mac-studio.local, macOS arm64, through the carrier /tmp/s33vm-carrier; `SHARDS_HELPERS=skip`, `CARGO_INCREMENTAL=0`)

- kiln run r1238 at a4bc7f212:
  - `cargo fmt --all --check` clean.
  - `cargo test -p shards --profile test-release --test api_image --no-run` built.
  - `cargo clippy -p shards-testguest --all-targets --target {aarch64,x86_64}-unknown-linux-musl -- -D warnings`
    (zig) clean.
  - `cargo clippy -p shards --test api_image -- -D warnings` refused three lints (two `type_complexity`, one
    `panic_in_result_fn`) on aarch64-apple-darwin, x86_64-unknown-linux-gnu (zig) and x86_64-pc-windows-msvc
    (xwin). They were fixed in the next commit.
- kiln run r1247 at 29c8d321d: fmt, and clippy of `shards --test api_image` on the same three targets, clean (exit 0 each).

## Owed (ROOT)

- On a Linux KVM host (x86_64; and arm64 through the L1 path):
  `cargo test -p shards --profile test-release --test api_image -- --exact --test-threads=1
  an_h2_request_to_a_granted_host_is_filled_never_indexed_and_its_echo_comes_back_masked
  a_refused_lease_is_answered_503_over_http_1_1_and_h2_and_a_later_request_asks_again
  an_h2_sign_in_gives_the_harness_placeholders_and_its_refresh_is_answered_outside
  staged_credentials_reach_the_harness_and_its_output_and_end_come_back` (the last because its home scan now
  goes through `holding`).
- HVF and WHP: the same command prints SKIP for the three. A decision on giving a run's injector a test's root
  (D242) is needed before they can run there.
- Under step 10's leak scan, and the guard-removal mutations listed above, run against these real-VM tests.
