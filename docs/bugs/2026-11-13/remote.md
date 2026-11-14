# Remote adversarial review

POLICY_READ: head `2bc5f78f9e48d09ddbf4e848f9960133f6d19148` (origin/main agreed at review start); base `79caf2d74`; role **assigned adversarial reviewer**, area **remote**, coordinating this read-only review. Owned scope: crates/remote, context/CLI routing, enrollment/certificates, Windows D180 and established-connection liveness. Binding `/tmp/g121-shot/review/brief.md` read first and in full.

Policy SHA-256:
- AGENTS.md: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`
- docs/development-rules.md: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`
- docs/development-evidence.md: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`
- CLAUDE.md: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`

Three scoped read-only reviewers returned matching POLICY_READ receipts (TLS/enrollment, Windows contexts, framing/cleanup). Coordinator independently re-read the decisive paths. No source changes, builds, tests, policy scripts, daemons, VMs or Docker were run. All results below are **source proofs, not live reproductions**; proposed regression/mutation checks remain ROOT's. No real secret was read or included.

Ranking requested by Ada: **P0 critical, P1 high, P3 medium, P5 low**; unused intermediate ranks have no findings here. Counts: **3 critical/P0, 2 high/P1, 3 medium/P3, 0 low/P5**. Both the goal-stack diff and current head were reviewed. Older defects are explicitly marked; they are not attributed as new regressions.

## Findings, highest severity first

### remote-01 — P0 / critical — Untrusted CLI prefixes bypass owner-only authorization

**Location:** `crates/shards/src/daemon/remote.rs:41`, `:109`, `:128`; `crates/shards/src/context.rs:918`, `:3727`.

**Defect:** Authorization checks the original first wire argument, but the child CLI later removes leading context flags and executes an owner-only command with the daemon owner's filesystem authority.

**Concrete scenario:** Alice is enrolled with no host-directory/device grants. A custom authenticated client sends Request.args `["--context", "default", "client", "grant", "alice", "--dir", "C:\\"]` to a Windows host (use `/` on Unix). `owners()` rewrites grammar and matches only `said.first()`: `--context` is not forbidden. `client_grants::refuse_words` likewise sees no recognized command after that prefix (`client_grants.rs:507`). `Machine::start` spawns the unchanged argv. `cli::main` calls `context::chosen_in`, whose `args.drain(..2)` strips the prefix, then dispatches `client grant alice --dir C:\\`. `context::admin` / `grant_client` do not reject SHARDS_CLIENT and directly call `grants.add` and `save`. Alice has granted herself the host drive. `--context=default` and `-c default` work similarly. An explicit other context also outranks command_env's SHARDS_CONTEXT=default and can forward into the owner's other context using its identity.

**Additional ingress needing the same audit:** `\0sync build KEY` accepts client-supplied inner argv (`remote.rs:769`, `:818`, `:895`) and runs it through `Building::run` without `owners` or `refuse_words`. After a valid empty context header/layer, inner argv `client grant alice --dir /` reaches the same admin handler without requiring a context prefix. Fixing only the prefix leaves this ingress.

**Evidence/contract:** `owners`: `let first = said.first()`; `chosen_in`: `args.drain(..2)` / `args.remove(0)`; `grant_client`: `grants.add(&asked)?; ... save(...)`. D180 reserves client/context/invitation/listener administration to the machine owner; D228 gives clients no host directory/device authority until the owner grants it. Raw wire arguments are not restricted to arguments the trusted CLI ordinarily emits.

**Refutation checks:** command_env sets SHARDS_CLIENT last, but admin never consults it. HERE only sets an environment default, which explicit flags supersede. File-operation refusal has the same prefix assumption. No later grant operation reauthorizes the caller.

**Confidence:** high, source-proven; no exploitation run. **Provenance:** prefix/inner-command primitives existed at base; goal-stack Windows serving and D228 grants expose/amplify their current isolation impact.

**Fix direction:** authorize the exact canonical operation executed; reject remote global context overrides, strictly type/allowlist sync build operations, and independently enforce the owner boundary in administration handlers. Do not trust CLI-generated NUL markers as authentication.

**Test:** live disposable enrolled client sends all three prefixes and the sync carrier; each receives typed refusal, grants/context files remain unchanged, and a different configured daemon sees no request. Removing a guard must produce the forbidden grant/routing effect.

### remote-02 — P0 / critical — Windows drive-relative Dockerfile names escape the copy

**Location:** `crates/shards/src/synced.rs:613`, `:627`, `:690`.

**Defect:** A build filename with a Windows drive prefix passes the single-name guard and replaces the directory in PathBuf::join, allowing a remote client to overwrite files outside its synced directory.

**Concrete scenario:** Listening Windows daemon's home/current directory is `C:\...\home` (`daemon.rs:1831`). Enrolled Alice has no directory grants. Send Request.args `["\0sync", "build", KEY, "build", "\0context"]`, KEY = 64 lowercase hex digits. Supply stdin magic `shards context 3\n`, u32BE file count 1, filename `C:context`, valid mode/ordinary timestamp, length/body naming another context. The guard rejects slash, backslash, NUL, `.` and `..`, but accepts `C:context`. On Windows a prefixed path replaces the base in PathBuf::push/join; `dockerfile.join("C:context")` becomes drive-relative `C:context`. `File::create` truncates the daemon home's `context` file. `C:identity` similarly targets the identity file. The escaped write precedes remaining secrets/archive decoding, so truncating the request afterward does not undo it.

**Evidence/contract:** receive parses network filename and calls `write_file(&dockerfile.join(&name), ...)`; write_file uses plain `File::create`. D180/D228 allow no ungranted host-file writes. Rust PathBuf Windows prefix semantics are significant here; a name with no slash is not necessarily a relative child name.

**Refutation checks:** the 64-hex outer key constrains only the copy directory. Windows context tar's `apply_layer_lx` protection is a different path and is not used for these separately framed Dockerfiles. Timestamp or later parse errors happen after the write. This needs no symlink or `..`.

**Confidence:** high source proof, Windows execution unverified. **Provenance:** filename guard existed on Unix; the goal stack made this receiver reachable on a serving Windows host.

**Fix direction:** require one host-valid Normal path component, reject drive/root/prefix/ADS/reserved aliases, and open relative to the intended directory with reparse protections; use the existing LX-name representation if Linux colon-containing names must be retained.

**Test:** on Windows, valid authenticated build framing with `C:context`, alternate prefixes/ADS names and an ordinary control filename; assert refusal happens before any outside file is opened and home markers remain byte-for-byte unchanged.

### remote-03 — P0 / critical — Removal does not revoke existing Engine authority or cached enrollment identities

**Location:** `crates/shards/src/daemon/engine.rs:584`; related `daemon/remote.rs:96`, `:509`; `crates/remote/src/server.rs:440`.

**Defect:** Long-lived remote state caches a client name instead of revalidating the authenticated enrollment for each new operation, allowing revoked clients to continue using authority.

**Concrete scenario A (no name reuse):** Enrolled K opens `\0engine` and completes one HTTP/1.1 request while keeping the stream open. `Machine::start` checks membership once, `engine_step` creates the API connection, and `engine.rs:551` stores only client NAME in thread-local state. Owner completes `remove client NAME` (`context.rs:3661`), removing enrollment/grants/synced files but invalidating no connection. On the same stream K sends a *new* `GET /containers/KNOWN_ID/json`. `engine_connection` loops over heads and calls engine_call with no enrollment check; route `engine.rs:727` invokes `engine_inspect`, which resolves the record and returns 200/full inspection (`engine/containers.rs:369`). Removal has not ended access.

**Concrete scenario B (replacement identity):** Keep K1's native QUIC connection open as NAME; remove NAME; enroll K2 under NAME and grant K2 a directory. K1 opens a new Exec stream on its old connection. `server.rs:440` passes only cached NAME into Host::start, and `remote.rs:96` checks only `.any(|c| c.name == client)`. It now passes because K2 exists; K1 executes with SHARDS_CLIENT=NAME and replacement grants, without proving possession of K2.

**Evidence/contract:** D180 (`architecture.md:5388`) promises enrollment checked for each command, removal ending access at once. The API loop is explicitly persistent. Removing grants does not restrict direct inspect/list requests. No connection invalidation occurs in the removal path. The existing removal test opens a fresh Engine connection (`tests/context.rs:880`), so it misses reuse.

**Refutation checks:** these are new requests after completed removal, not continuation of an admitted in-flight command. TLS pinning remains cryptographically correct for K1 but does not reauthorize that key against current enrollment. Closing fresh connections after removal does not revoke retained ones.

**Confidence:** high source proof. **Provenance:** older admission model retained at head; goal-stack D228 named grants increase the name-reuse consequence.

**Fix direction:** carry authenticated key plus enrollment generation into all remote/Engine operations, revalidate on each operation, invalidate removed generations' channels, and define ongoing-operation behavior explicitly.

**Test:** retain HTTP keep-alive through removal and issue fresh inspect/kill; assert no document or side effect. Separately retain native K1 across remove/re-enroll K2 under the same name; K1's fresh Exec must be refused while K2 works.

### remote-04 — P1 / high — An untrusted build timestamp aborts the daemon

**Location:** `crates/shards/src/synced.rs:617`, `:701`; `Cargo.toml:106`, `:134`.

**Defect:** A remotely supplied timestamp reaches unchecked SystemTime addition on a daemon thread, panicking with panic=abort.

**Concrete scenario:** An enrolled client sends the valid `\0sync build` carrier above, legal filename `Dockerfile`, empty body, mtime seconds `i64::MAX` and nanoseconds `u32::MAX`. The receiver reads these integers directly. `write_file` computes `UNIX_EPOCH + Duration::new(secs, mtime.1)`. Nanosecond normalization puts the duration beyond i64::MAX seconds; SystemTime cannot represent it and Add panics (Windows has a much smaller representable positive range too). The context-step thread runs inside the listening daemon (`daemon/remote.rs:745`, `:773`). Both dev/release use panic=abort, ending the daemon and disconnecting other clients. No complete tar is needed because this happens before tar parsing.

**Evidence/contract:** unchecked `+` at `synced.rs:701`; R01 prohibits implicit panic paths from untrusted sizes/values. A Result-returning closure does not catch a panic.

**Refutation checks:** set_modified's error return cannot help because argument evaluation panics before the call. Negative timestamps being mapped to zero does not constrain positive maxima. An unwinding unit-test profile would understate the deployed blast radius.

**Confidence:** high source proof. **Provenance:** arithmetic existed on Unix; serving Windows became reachable in this goal stack.

**Fix direction:** validate subsecond range and use checked SystemTime construction; return a typed malformed/unrepresentable timestamp.

**Test:** authenticated malformed timestamp against a disposable release daemon must refuse and leave a second command working. Include nanosecond and host timestamp boundaries; mutation restoring unchecked Add must terminate the fixture for this reason.

### remote-05 — P1 / high — Expanded client secrets are persisted in ordinary logs

**Location:** `crates/shards/src/daemon/remote.rs:177`; also `:132`, `:142`, `:160`.

**Defect:** Remote command logging prints full argument values after client-side environment/file expansion, creating an unintended persistent secret store.

**Concrete scenario:** Client uses synthetic `REVIEW_SECRET=sentinel` in its environment or env file and runs `shards --context gpu run -e REVIEW_SECRET IMAGE true` (or `--env-file`). `context.rs:2378` expands bare names; `:2383` expands env-file lines into literal `--env=KEY=VALUE`. forward encodes these words in Request. Machine::start logs `format!("client {client}: shards {}", args.join(" "))`. daemon::log writes the line to stderr, captured in daemon.log. The guest need not print any value. Sync build args and refused requests have corresponding full-argument logs.

**Evidence/contract:** direct data flow from here_values through Request into args.join; brief explicitly hunts secrets in logs; R04 protects private tokens. Argument values and guest command arguments may contain credentials.

**Refutation checks:** login stays client-local and is not the example. Explicit build secret bytes use pipes and are not asserted to leak here. Owner control of a guest does not justify recording its secrets in ordinary diagnostic logs.

**Confidence:** high source proof. **Provenance:** run/sync logging existed at base; new refusal/policy branches extend the pattern.

**Fix direction:** log operation identity and safe metadata rather than raw argv; apply consistent redaction to every success/refusal/step branch.

**Test:** synthetic inherited env, env-file, exec env and build-arg markers must reach their intended operation and remain absent from captured daemon.log, including refusal paths.

**Status (2026-11-13):** fixed in 68b43042c111dbd7696e16fa5b8020993de9d274 (red `daemon::remote::tests::remote_requests_never_log_argument_values`; 0 passed/1 failed before, 5 module tests passed after; shards all-target clippy passed, macOS arm64). [Evidence](../../research/measurements/remote-fixes-v1/README.md). Real context/guest validation remains owed to ROOT.

### remote-06 — P3 / medium — Unauthenticated datagrams count as peer progress

**Location:** `crates/remote/src/net.rs:1683`; consumers `:704`, `:774`.

**Defect:** Driver::receive updates Liveness::heard before the QUIC connection validates the datagram, so rejected traffic suppresses stall reporting and satisfies reclaim's liveness check.

**Concrete scenario:** A known/warm connection has an outstanding send; its true peer vanishes. An observer knows its visible destination CID and periodically sends syntactically valid short-header datagrams with that CID but bogus encrypted bytes, more frequently than the learned window. Endpoint routes the CID to ConnectionEvent; line 1683 unconditionally updates heard. `handle_event` subsequently discards traffic from an invalid source or failing authentication, but never rolls heard back. `stalled` uses the latest heard, so no whole window of silence is detected and RanOut::Stalled is not emitted. At a bounded owner endpoint, such packets after each newcomer's PING also make `answered_ask` true, keeping a vanished holder's place and refusing its replacement. Server-side injection needs no valid packet authentication; client-side injection is constrained by the connected UDP source filter.

**Evidence:** inspected the exact pinned hyper-quic `e0613f524b0df198cc61dc1e82b33c22d604cf7b` source in the existing Cargo dependency cache, without modification. `crates/hyper-quic/src/endpoint.rs:320–385` only partially decodes/routes, and ConnectionIndex::get (`:1817`) returns a handle from CID alone. `connection/mod.rs:1759–1794` can discard an unexpected source; packet_result (`:3764–3782`) does AEAD authentication later and silently drops failures. Shards calls heard before any of these checks. RFC 9000 §10.1 successful packet processing is the relevant liveness principle; D180's whole-window/no-progress rule must describe the actual authenticated peer.

**Refutation checks:** transport drops the payload, so this is not command forgery. Its UDP receive counter also increments before authentication and is not a safe replacement. Handshake-progress counters already protect unfinished-handshake reclaim, but established-holder progress still uses unverified heard. This attack does not require decrypting a CID, which is visible in QUIC headers.

**Confidence:** high for source behavior; attack rate/network manifestation untested. **Provenance:** Liveness and reclaim are goal-stack additions.

**Fix direction:** advance heard only from a transport indication of a newly accepted authenticated packet (including legitimate ACK-only packets), never header routing or raw UDP receipt.

**Test:** warm real connection through a disposable raw-packet relay; stop true peer, inject bogus CID-matching packets, assert exactly one Stalled and no Answered; a later newcomer must reclaim the place. Authentic ACK-only progress must still preserve a live holder.

### remote-07 — P3 / medium — Relay setup errors lose child cleanup/reaping ownership

**Location:** `crates/remote/src/server.rs:751`; related `:733`, `:773`, `:674`.

**Defect:** Fallible setup after destructuring Running drops hangup/wait callbacks without executing them, leaving an already-started command unowned on descriptor/thread exhaustion.

**Concrete scenario:** A Unix remote command with no output is successfully spawned (`daemon/remote.rs:281–305`). The listener reaches its descriptor limit before relay's `sender.try_clone()` at line 751, which duplicates a wake socket. The `?` drops Running's wait/hangup closures. `Server::started` receives Err, but its stream only receives hangup in the success arm (`:694`); ending the stream cannot terminate/reap the child. A no-output sleeping command can outlive closed stdio; after exit it still needs its spawner wait/reap request. Thread creation failures have the same ownership hole.

**Evidence/contract:** `Running` has no cleanup Drop; `shards_ipc::Child` Unix docs (`crates/ipc/src/unix.rs:1323`) state waiting reaps, dropping does not. R04/R10 require cleanup on errors; D180 promises command hangup and reaping when the stream ends.

**Refutation checks:** start_apart cleans up an undeliverable Started event, but this event was delivered and fails later. The missing-stream branch hangs up only an Ok relay. This proof is Unix; incidental Windows signal-pipe effects are not asserted.

**Confidence:** high source proof. **Provenance:** early-return ownership defect existed at base and survives the new asynchronous start handling.

**Fix direction:** retain a cleanup guard until all workers and the sole wait/reap obligation are successfully transferred; every setup failure must terminate and arrange exactly one reap.

**Test:** inject sender-clone/thread-start failure at every relay stage, assert hangup and wait exactly once; then bounded disposable real-child exhaustion fixture must observe process exit and reaping.

### remote-08 — P3 / medium — Chosen invitation timeout and frame size still violate R03

**Location:** `crates/remote/src/invite.rs:21`; `crates/remote/src/frame.rs:13`.

**Defect:** Production still chooses a 3600-second invitation expiry and 4 MiB frame cap without the protocol-bound or runtime-derived justification required by the binding brief/R03.

**Concrete behavior:** an unused otherwise-valid invitation redeemed after 3600 seconds is refused regardless of the actual cold-start/transfer path; the comment only says "as focal's do" and "long enough". A payload of 4 MiB+1 is refused by put/peek despite the u32 wire length being representable; the cited Linux ARG_MAX 2 MiB neither derives the multiplier nor establishes a portable bound (Linux's aggregate argument limit also depends on RLIMIT_STACK).

**Evidence/contract:** explicit constants and their comments; D180 repeats these choices but its later values-derived inventory does not derive MAX_FRAME. Brief says upstream defaults/other products' numbers are not enough and every chosen timeout/size is a finding. This finding follows that explicit policy; it does not claim bounded tokens are unbounded or ask to remove safe admission bounds.

**Refutation checks:** SHA256/Ed25519 lengths and RFC 9002's cold RTT/PTO have genuine derivations and are excluded. Input queue=1 has a documented wakeup rationale. These two particular values lack that derivation; a fuller literal audit was not completed.

**Confidence:** high on policy mismatch; no measured runtime harm claimed. **Provenance:** constants existed at base and remain at head under the current binding policy.

**Fix direction:** make invitation expiry an explicit owner-authorized security policy (never lengthen authority via learning); derive frame admission from the actual wire/operation/resource contract and document cold-start/adversarial behavior. Establish any exceptional invariant with primary evidence rather than a borrowed value.

**Test/evidence:** retain boundary tests for authorized expiry and admitted payload size, plus the policy/measurement record deriving each boundary. Merely asserting the current literal is not evidence of its derivation.

## Checked and found sound

Source inspection only, not passing runtime validation:

- Every production Shards QUIC provider found routes through remote/tls.rs's provider, whose sole suite is TLS13_AES_256_GCM_SHA384 (`:138`). Both client/server enable TLS13 only (`:268`, `:319`); both disable early data (`:273`, `:325`). Context, managed input/output and agent-control callers share it. In-VM TLS also narrows its separate stream-TLS provider.
- Suite tests assert both endpoints and negative AES128/ChaCha-only offers in both role directions; control tests assert both ends too. Tests were read, not run. QUIC Initial packets' specified AES128 protection is not an alternative negotiated TLS application suite.
- Identity is explicit RFC 7250 raw Ed25519 public keys, so DNS SAN, certificate chain and X509 expiry checks are not missing X509 validation in these paths. Pinned verifier checks full SPKI SHA256 and encoding, and verifies TLS13 transcript signatures; mandatory client authentication proves key possession before command eligibility.
- Invitation redemption checks expiry and constant-time HMAC-SHA256 over the TLS exporter before redeem/enroll. Atomic invitation removal permits one successful concurrent redemption. Stranger Exec is refused before Request decoding; async identity judgment completes before command frames are read. Debug implementations redact identity/invitation private material.
- Liveness Stage::Waiting -> RanOut -> Stalled is explicit; Stalled is told once, keeps the connection open and drops its deadline; ACK progress emits Answered once and raises the learned window. No local idle timeout/periodic keepalive remains between two current Shards peers. Peer-negotiated idle timeout still bounds older peers. The positive state logic does not cure the unauthenticated input issue above.
- Framing validates tags and bounded lengths, uses checked frame offsets/slices, retains partial writes and propagates per-chunk credits. Request decoding restricts environment names and rejects trailing/malformed fields.
- Normal PolicyBuilder keeps eval/test and input files on the client, and uses dedicated typed platform/resolve/fetch steps remotely. Policy step environment cannot be supplied directly by a client.
- Normal copy-vm paths drive source/destination files on the client and exchange archive streams with the chosen machine. Windows engine relay converts private pipe records to npipe URI and reloads the named context under lock. Normal child routing sets SHARDS_CONTEXT=default, though malicious wire flags/inner commands bypass that assumption as remote-01 documents.
- Synced outer keys restrict to 64 lowercase hex digits; Windows tar application uses the existing LX tree implementation. This does not protect the separately framed Dockerfile names.

## Not covered

- No live reproductions, red/green/mutation runs, tests/builds/lint/check-development-policy, captures, workloads or profiling: forbidden by the binding brief. Findings need ROOT's isolated live validation before an R09/R10 fix. No claim that R01–R16 or goal #15/#21/#31 is fully validated.
- No actual Windows listener/client pair, WHP guest, Windows buildx policy/copy-vm/Docker CLI, cross-machine link, KVM/HVF E2E or comparative baseline was run.
- TLS resumption/ticket-cache internals were not comprehensively reviewed; no resumption-specific suite regression was found. Configuration has no suite-widening route, but resumption behavior is not reported passed.
- Downstream filesystem/archive NTFS/reparse races and managed/control subprotocols were not exhaustively reviewed. No exhaustive constants/allocation/resource audit.
- Unbounded default Driver::take_in drain (`net.rs:1504–1531`) can defer commands/timers while datagrams remain continually available; sustained-flood starvation feasibility was not measured and is not counted as a proven finding.
- Initially suspected ordinary repeat-join failure was refuted: client::join closes its temporary connection, and later exec authenticates anew. Omitted as an independent defect.

Supporting lane notes (same source revision, source-only): `/tmp/g121-shot/review/remote-context-notes.md`, `remote-tls-notes.md`, `remote-protocol-notes.md`.

Kiln delivery: first agents_send to ROOT `01a148884a4770fdaf0111f3f792a248` was rejected with HTTP 429 backpressure (`pending=3`, `authority=2`); retry pending. Immutable retry key: `remote-review-2bc5f78f9-final-v1`.

Final delivery status: three publication attempts (two steer retries using the same immutable key, then queue mode with key `remote-review-2bc5f78f9-final-queue-v1`) were rejected by Kiln with HTTP 429 `backpressure`, `pending=3`, `authority=2`. None returned an acceptance receipt; notification to ROOT is **unconfirmed/not delivered via agents_send**. The complete ranked result remains available at the report path above. No message or queue was deleted, replaced or forced to bypass this refusal.
