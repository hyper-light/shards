# Credentials adversarial review

POLICY_READ — role: **adversarial reviewer**, area: **creds**. Coordinator for the overall Shards review: ROOT, Kiln agent `01a148884a4770fdaf0111f3f792a248`. This reviewer coordinated two read-only source subreviews; both acknowledged the full brief and policy with the hashes below. No implementation role was exercised.

Head: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148` (also this checkout's `origin/main`). Base: `79caf2d74`. Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-creds`.

| Policy file read in full | SHA-256 |
| --- | --- |
| `AGENTS.md` | `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41` |
| `docs/development-rules.md` | `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3` |
| `docs/development-evidence.md` | `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63` |
| `CLAUDE.md` | `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010` |

Read `/tmp/g121-shot/review/brief.md` in full before any other inspection. Review method: `git diff 79caf2d74...HEAD` and scoped history, line-numbered source inspection, D242/D184 and STEP3/4/7/8 evidence inspection, relevant existing tests, and primary RFC text. **All findings below are source-confirmed traces, not live reproductions.** The binding brief prohibits builds, tests, daemons and VMs, so none was run. Test sketches are proposals for ROOT, not executed tests. `T`, `R`, and `P` below are symbolic synthetic fixture values, never actual credentials.

Priority mapping used here: P0 = critical disclosure; P1 = high; P3 = medium. No P2, P4 or P5 findings. Counts: **4 critical, 2 high, 2 medium, 0 low**. The two documented residuals are explicitly identified; they are included because the requested claim is that no credential value reaches the VM, and the brief excludes only the known chunked *request* token-path issue.

## Findings, highest severity first

### creds-01 — P0 / critical — Sign-in capture forwards tokens repeated in response headers

**Location:** `crates/net/src/inject.rs:2235` (HTTP/1.1), `:3481` (HTTP/2); `crates/net/src/signin.rs:416`.

**Defect:** Capture rewrites the JSON token body but forwards response header values without checking them against the captured tokens.

**Failure scenario:** A granted sign-in's token endpoint returns a valid identity-coded response with `{"access_token":"T","refresh_token":"R"}` and `X-Echo: T` (or `Set-Cookie: access_token=T`). The orchestrator accepts capture and supplies placeholders. The guest receives placeholders in the JSON and the complete real `T` in the header, available to its TLS client/raw socket and logs. This requires a token host response containing such a header; no claim is made that either real provider currently emits it.

**Evidence / trace:** `capture_tokens` (`inject.rs:1497`) calls `signin::read` on the **body only**. Its duplicate-token guard (`signin.rs:537`) inspects other JSON strings, not the head. H1 `response_head` retains the head at `inject.rs:2285`; after capture, `captured` does:

```rust
let (sign_in, written) = capture_written(&body, &tokens, self.sign_in, run)?;
let framed = signin::frame(&head, written.len()).map_err(Refusal::SignIn)?;
writer.write_all(&framed).and_then(|()| writer.write_all(&written))
```

`signin::frame` only replaces Content-Length/Transfer-Encoding (`signin.rs:421–429`). H2 initially applies the ordinary request-value mask to response fields (`inject.rs:3821`), but a token-host request injects no token, so that mask contains no newly captured `T`. It retains those fields (`:3845`), then `captured` calls `give(gid, fields, &written)` (`:3483`); `give` only changes Content-Length (`:3498–3510`). D242's capture claim (`architecture.md:27649` vicinity, step 8 second slice) says no real tokens reach the guest afterward; this trace contradicts it.

**Fix direction:** Before publishing any captured response, check/sanitize all retained response fields and trailers against all captured token values, including refresh and ID tokens; reject an unhandled representation. Apply the same rule on H1/H2.

**Regression sketch:** Extend the existing H1/H2 capture benches with a JSON token response and a header repeating each token. After broker capture succeeds, collect the guest's entire plaintext response; assert no `T`/`R`/real ID token occurs in headers or body. Include a field that is retained through `frame`/`give`, rather than a framing field.

**Confidence:** High for both source paths; live TLS and actual-provider behavior unverified.

### creds-02 — P0 / critical — A chunked response can carry the complete credential in an unmasked extension

**Location:** `crates/net/src/inject.rs:877`, `:2142`, `:1349`.

**Defect:** Chunk-size lines, including extension values, are forwarded into guest plaintext while deliberately bypassing credential matching.

**Failure scenario:** A normal injected request reaches its granted host with live leased value `T`. That host answers:

```text
HTTP/1.1 200 OK\r\n
Transfer-Encoding: chunked\r\n
\r\n
1;echo=T\r\n
x\r\n
0\r\n
\r\n
```

Choose a token-safe synthetic `T`; quoted-string extensions also permit other token characters. The guest receives the complete raw `T` before HTTP decoding. No expired lease, compression or failed grant is needed. This is a chunked **response** disclosure, distinct from the excluded unread chunked token-path POST.

**Evidence / trace:** `chunk_size` (`:935`) validates the size and tolerates semicolon extensions. `walk` emits the whole size/extension line as `Piece::Frame(line)` (`:877`). Response forwarding sends `Piece::Frame` through `mask.push(b, false, ...)` (`:2142`). Matching is skipped for it:

```rust
if !data || longest == 0 {
    continue;
}
```

The bytes nevertheless enter `Mask.queue` and are released to `guest.writer()`. Trailers are `Piece::Data` and do not share this specific bypass. [RFC 9112 §7.1.1](https://www.rfc-editor.org/rfc/rfc9112.html#section-7.1.1) permits extension values as tokens or quoted strings; ignoring their semantics does not prevent their bytes entering VM memory. D242 describes skipping framing but does not resolve its confidentiality consequence.

**Fix direction:** Forward canonical chunk-size lines without extensions, or sanitize extensions through a separate match context while keeping body matching across chunks. Merely matching all framing as body would lose the existing cross-chunk matching guarantee.

**Regression sketch:** In the HTTPS echo fixture, return a body with `T` only in a chunk extension. Collect raw guest plaintext and assert `T` is absent. Split the size/extension line across TLS reads. Preserve a separate cross-chunk body-token regression.

**Confidence:** High by complete source trace; not run.

### creds-03 — P0 / critical — Lease expiry or rotation disables masking of responses already in flight

**Location:** `crates/net/src/inject.rs:1309`, `:1339`; `crates/net/src/lib.rs:1299`, `:2883`.

**Defect:** An in-flight response remembers a credential reference, then looks up its current lease instead of the value actually sent; expiry or refresh makes echoes of the sent value pass unmasked.

**Failure scenario:** Send a request filled with `T`. The granted host delays its echo until after the lease ends, then returns `T` in a plain body/header. `held(reference, now)` is now `None`, so the guest receives `T`. Alternatively, while that response waits, refresh the sign-in on another flow: `follow(Changed::Refreshed)` forgets the held access lease. An echo of the old `T` now passes, even before its previous deadline. Reacquiring a rotated value `T2` masks `T2` rather than the `T` already sent.

**Evidence / trace:** H1 records only references at `inject.rs:2098–2110`; H2 does likewise at `:3609–3618`. `Mask::values` calls `leases.held(reference, run.now)?` (`:1309`). When no value is held, `push` resets matching and releases buffered data (`:1337–1349`, `:1390–1403`). `Leases::held` excludes lapsed leases (`lease.rs:298`), `lapse` removes them (`:307`), and the loop invokes lapse (`lib.rs:2883`) without closing affected response streams. Refresh invokes `forget` (`lib.rs:1299`), also without ending their responses. Both H1 and H2 use this same mask.

D242 **explicitly acknowledges post-expiry echoes as an unmasked residual** (`architecture.md:27460–27464`). This is not newly discovered documentation: it is a confirmed exception to the requested unconditional security claim, with refresh/rotation supplying a second trigger.

**Fix direction:** Fail closed for in-flight streams when their masking lease/value is invalidated, or design a per-exchange masking lifetime that preserves confidentiality without using an expired lease to authorize new requests. Do not simply retain all expired credentials indefinitely.

**Regression sketch:** Send with synthetic `T`, withhold the response, expire the lease or call the real refresh-follow path, then return `T`. For H1 and H2 assert that the stream is refused/reset or its full guest plaintext contains no `T`. Repeat with a newly leased `T2` and an old-`T` response.

**Confidence:** High; explicitly confirmed by both code and design record, no live reproduction.

### creds-04 — P0 / critical — Compressed echoes bypass the raw-byte response mask

**Location:** `crates/net/src/inject.rs:1992`, `:2141`, `:3939`.

**Defect:** Ordinary credential-bearing API responses are forwarded with content coding intact, while masking only searches the encoded bytes for the unencoded credential.

**Failure scenario:** The guest requests gzip and sends placeholder `P`; the injector fills real `T` to its granted host. The host returns a gzip-compressed JSON/body echo of the request's Authorization value. Select an encoding whose wire bytes contain no contiguous `T`. The raw matcher makes no replacement, and the guest's normal HTTP decompressor reconstructs the complete credential in VM memory. The same applies to HTTP/2 DATA. This requires a host response echo; it is not evidence either current provider exposes such an endpoint.

**Evidence / trace:** `identity` (`inject.rs:1101`) is applied only to sign-in token-host requests (`:1977–1979`, H2 `:3264` vicinity). Generic requests preserve Accept-Encoding, and generic response handling calls no `signin::readable` content-coding guard. H1 passes body bytes directly to `Mask::push` (`:2141`), H2 does the same for DATA (`:3939`); neither decompresses them. Mask matching compares those bytes directly with `leases.held` (`:1309`, `:1363`).

D242 **explicitly lists compressed response/WebSocket echoes as residuals** (`architecture.md:27460–27462`). Their presence prevents treating the current implementation as an absolute no-value-in-VM boundary.

**Fix direction:** Define and enforce the supported response representations for injected exchanges. Request identity and reject unsupported content coding before forwarding, or decode/mask/re-encode under appropriate resource accounting. Apply equivalent enforcement after upgrades; changing Accept-Encoding alone cannot guarantee compliant upstream behavior.

**Regression sketch:** Existing echo fixtures return `gzip(JSON(T))` with Content-Encoding gzip; first assert the compressed fixture does not literally contain `T`. Collect and decompress the guest response, and assert `T` never becomes available (or the injector refuses before forwarding). Cover H1 and H2.

**Confidence:** High for source behavior; no decompression/live-provider test run.

### creds-05 — P1 / high — Lease duration restarts at each delivery hop, authorizing use after the lessor's deadline

**Location:** `crates/net/src/lease.rs:266`; supporting `crates/shards/src/api_run.rs:1353`, `crates/api/src/image/session.rs:1375`.

**Defect:** A remaining lifetime computed against the broker's deadline is turned into a fresh full lifetime on receipt, ignoring time already spent queued or delivering it.

**Failure scenario:** At `t0`, the lessor's `Leased.until` is `t0 + 1s`. The SDK calculates `lives = 1s`, then its answer is held by scheduling or channel backpressure until `t0 + 2s`. The network process accepts it at that late time and holds/injects `T` until `t0 + 3s`, despite the original lease having ended. Additional run-host-to-network delay adds another discrepancy. A fast normal-path measurement cannot establish a security bound on those delays.

**Evidence / trace:** `session::settle` computes `leased.until.checked_duration_since(now)` and serializes only `lives` (`session.rs:1375–1390`). The run host stores `until: now.checked_add(Duration::from_nanos(lives.get()))` (`api_run.rs:1353`) and relays that duration. `Leases::answered` repeats the same addition at receipt (`lease.rs:266`) without any original expiry or transit-age field. `held`/`ask` subsequently consult the inflated local expiry. D242's values-only-while-the-lease-lives claim does not hold under delayed delivery. The protocol's deliberate use of relative lives does not bound or remove this extension.

**Fix direction:** Carry an expiry/age representation that cannot extend authority across local hops, or conservatively debit all elapsed time from an agreed request/answer reference. Handle cross-clock uncertainty explicitly rather than granting a fresh lifetime on each receive.

**Regression sketch:** Obtain a lease ending at `t0 + 1s`, encode its answer at `t0`, and deliver it at `t0 + 2s` using injected Instants through the actual relay. Assert `held(reference, late_now)` is `None` and a pending request sends no upstream head. Current source creates a new live lease instead.

**Confidence:** High for the deadline extension; no scheduling/backpressure reproduction run.

### creds-06 — P1 / high — Repeated retries of one pending lease accumulate unbounded duplicate waiters

**Location:** `crates/net/src/lease.rs:219`; supporting `crates/net/src/inject.rs:2839`, `:3555`.

**Defect:** Every retry of an already-pending lease appends the same flow id to its waiting vector, with no uniqueness or per-flow cleanup.

**Failure scenario:** A guest opens one H2 stream containing a granted placeholder while its broker remains slow/unanswered. It then sends valid PING frames one input turn at a time and reads their ACKs. Each turn retries the parked stream and appends the same waiter. TCP and protocol queues drain while the host process retains an ever-growing waiter vector. When the broker finally answers, each duplicate becomes another `Resumed` with a cloned reference, causing additional allocation before downstream deduplication.

**Evidence / trace:** H2 `serve` iterates all streams and calls `go` on every turn (`inject.rs:2839–2844`). PING is accepted (`:3079`). `go` calls `run.ask` for each parked reference (`:3554–3559`), reaching:

```rust
Some(Lease::Asked { waiting, .. }) => {
    waiting.push(waiter);
    Asked::Waiting
}
```

`resume` maps every vector entry into a newly allocated result (`lease.rs:283–288`). `Stack::resume_flows` deduplicates only after those allocations (`lib.rs:2589–2594`). The waiting vector is not included in `Conn::held`/H2 byte-window accounting. This violates the bounded-state/resource claim; no invented timeout is needed to fix it.

**Fix direction:** Keep a unique set of flow waiters for each outstanding reference and unregister ended flows. Streams of one flow already share its resumption, so repeated entries carry no additional information.

**Regression sketch:** Repeatedly call `ask(reference, same_waiter, now)` while pending, answer once, and require exactly one resumed entry. Then park a real H2 stream and send many PING turns while measuring the pending waiter count; it must remain one.

**Confidence:** High for linear retained growth; exhaustion limits and runtime measurements unverified.

**Status (2026-11-13):** fixed in 1b21eb21a403024baae2a7b815b8b1f5ac987255 (red `lease::tests::creds_06_a_pending_lease_resumes_each_flow_once` at 6eb3ad718). Merged-tree lease tests: 12 passed; shards-net all-target clippy and policy check passed on macOS arm64. See `docs/research/measurements/creds-fixes-v1/README.md` for revisions, retained output and coordinator work.

### creds-07 — P3 / medium — The CA's constraints authorize descendants outside exact host grants

**Location:** `crates/net/src/ca.rs:93`, `:116`.

**Defect:** Exact credential grant names are encoded as DNS subtrees, and `Ca::leaf` signs arbitrary names, so the CA can issue a trusted certificate for an ungranted descendant.

**Failure scenario:** Construct a CA for only `api.example.test`, then call `ca.leaf("child.api.example.test")`. It signs the child with no grant check. Its permitted subtree admits that child even though no credential grant names it. This disproves the CA test's claim that only granted hosts can verify. **No guest-accessible signing exploit is established:** the current injector has an exact SNI allow-list before calling `leaf` (`inject.rs:437–444`).

**Evidence:** `permitted_subtrees` is `hosts.map(GeneralSubtree::DnsName)` and `excluded_subtrees` is empty (`ca.rs:93–98`); `leaf` accepts any supplied name (`:116–123`). [RFC 5280 §4.2.1.10](https://www.rfc-editor.org/rfc/rfc5280.html#section-4.2.1.10) defines DNS constraints to include names formed by adding labels to the left. The existing negative cases (`ca.rs:165` vicinity) test unrelated/ancestor/suffix-confusion names but omit descendants. The code's own `Ca::new` comment acknowledges subtree semantics.

**Fix direction:** Enforce the exact granted name set inside the signing API, and narrow certificate constraints where supported. Do not claim DNS subtree constraints alone implement the exact-host authorization contract.

**Regression sketch:** Extend `its_leaves_hold_for_granted_hosts_alone` with a descendant and require signing refusal (and verifier refusal if exact-host constraints are advertised). Evaluate IP SAN constraints separately before claiming all other name forms are denied.

**Confidence:** High for the scope gap under RFC semantics; actual TLS verifier execution unrun. Medium reflects the existing outer exact-SNI guard.

### creds-08 — P3 / medium — A sign-in POST with Expect: 100-continue stalls before its headers can reach the origin

**Location:** `crates/net/src/inject.rs:1874`, `:2032`.

**Defect:** The whole-body sign-in path waits for the body before forwarding headers but neither acknowledges nor rejects a 100-continue expectation.

**Failure scenario:** On a sign-in token host, send a POST to the profile token/revoke path with a nonzero Content-Length within TO_HOST, Content-Type, and `Expect: 100-continue`; send only headers and wait for a provisional/final response. `outside` waits for the absent body; the origin has not received the request head and cannot send 100. The client stalls until it sends the body on a fallback timer or times out. Ordinary injected POSTs do not necessarily have this issue because their headers are forwarded before their bodies.

**Evidence / trace:** `read_head` selects `Whole` for the token/revoke POST (`inject.rs:1982–1990`). `go` invokes `outside` before any upstream head write (`:2032` versus `:2086`); `outside` returns Waiting when `pending.len() < n` (`:1874–1875`), parking the request again (`:2034–2037`). No implementation branch in crates/net handles Expect/100-continue. [RFC 9110 §10.1.1](https://www.rfc-editor.org/rfc/rfc9110.html#section-10.1.1) requires an origin receiving such headers to give an immediate final or Continue response, without first waiting for content. Its optional client fallback means the result is not an infinite hang for every client.

**Fix direction:** Explicitly handle the expectation in the whole-body path: emit a local 100 after admission or a typed final refusal before waiting. Preserve the rule that refresh/revoke bodies cannot be sent upstream before classification.

**Regression sketch:** Supply the headers alone to the token-host H1 bench; require a provisional or final response before supplying body bytes. Then send the body and check that refresh/revoke still stays outside the provider connection.

**Confidence:** High for the stalled transition; no real-client timeout observation. Not the excluded chunked request bug.

## Checked and found sound

These are source-inspection conclusions, not passing runtime results.

- Credential references and placeholders are distinct from leased values on the launch/lease control paths. Lease/sign-in payload copies are wiped after relay; late answers after whole-run release are handled without sending new requests. This does not establish complete allocator/disk erasure.
- The exact SNI gate precedes leaf issuance. H1 rejects duplicate/missing Host and direct SNI/host mismatch; H2 rejects duplicate authority/Host and authority-versus-Host mismatch before comparing against SNI. No off-grant send was demonstrated through those checked branches. H1 absolute-form authority semantics and hostile origin routing were inspected but not validated against real server implementations.
- H1 refuses TE+CL, conflicting lengths, bare CR/LF and invalid chunk-size controls; pipelined request heads are held until the prior response ends. The response-extension finding is specifically outside those guards.
- H2 unfinished field blocks require an immediate CONTINUATION on their same stream (`http2.rs:636`); interruption is a connection error. Each hop has its own HPACK context. Filled Authorization/x-api-key fields are encoded Never Indexed (`http2.rs:850`, `hpack.rs:322`), including avoiding full-value indexed lookup for that mode.
- H1 and H2 refuse a broker-denied lease before serializing the upstream request head (`inject.rs:2056`, `:3559`); no refused-lease-but-send path was found.
- Supported guest ALPN offers are filtered, forwarded upstream, and the upstream-selected protocol determines the guest's served protocol. This source review found no ALPN path that directly forwards a filled value outside the injector.
- CA key generation remains in the network process, before confinement; this source path does not write the CA private key to disk. The per-VM/process construction and public-certificate trust setup were inspected; no key persistence defect was proven.
- D184's helper isolation/challenge rules were read; relevant helper production changes in the review range are explanatory comments rather than a new credential-routing implementation. No new D184 finding established.
- Credential staging is deliberately documented as **native sign-in delegation** (`crates/api/src/image.rs:13`). `api_image.rs:186–231` explicitly expects synthetic staged values in guest stdout/stderr. This is not evidence of leased injector values silently taking that path. G18 (`managed-runtime-plan.md:208`, `:488–493`) still asks for an explicit mode; `ImageSpec` has no typed credential-mode field. The local gate evidence says the person chose Veil's placeholder shape, but this read-only review cannot establish the external goal #28's closure or whether a mixed-mode request is supposed to be rejected. That uncertainty is not promoted to a separate proven bug.
- New H2/HPACK numeric bounds inspected cite protocol/format requirements; no independent new fixed timeout/retry value was established in these paths. Reused TCP window constants predate the injector changes; this is not an exhaustive R03 literal audit of the repository.

## Not covered

- No builds, tests, `scripts/check-development-policy`, daemons, VMs, Docker, E2E, fault/chaos/stress, profiling, benchmarking or live TLS verification were run, per the binding brief. No tracked source files were changed; only this requested report was written outside the worktree. Test sketches above must be executed by ROOT before claiming live reproduction or validation.
- No real credentials, vendor sign-in, external broker store, keychain, provider CLI or provider endpoint was accessed. No finding claims the real Claude/OpenAI services currently return the synthetic malicious echo layouts.
- No empirical disk/snapshot/log leak scan, allocator lifetime/wipe verification, fd cleanup or child-process death campaign. Ordinary native staged credentials are guest-readable by documented design; a global no-credential-anywhere statement therefore needs an explicit mode/scope in addition to fixing injector leaks.
- Linux glibc/musl, Windows amd64/arm64 and macOS arm64 execution remain unverified. Intel macOS is unsupported. No portability passing result is claimed.
- PM records and linked STEP evidence were used to distinguish prior claims/owed validation; comparative performance, cold-start behavior and the 4–5x target were not assessed or claimed.
- Broader H2 stream/window exhaustion, every custom JSON edge case, arbitrary origin normalization, redirects and cross-host connection reuse were not exhaustively exercised. Candidate issues without a complete source trace were excluded.
- The known unread chunked H1 token-path POST was deliberately excluded as instructed. No fixes, commits, pushes or PRs were made.

R01–R16 accounting: R01/R04/R05/R06/R11/R12/R13 informed scoped code/state/ownership inspection, with gaps above; R02/R03 used existing design/evidence plus primary RFC semantics, not new architecture; R07 style-only observations were excluded; R08/R14 performance work was forbidden/unrun; R09/R10 live reproduction was forbidden and remains owed; R15 review/readback boundaries were maintained; R16 no builds/storage reclamation were performed. A receipt or source trace is not a substitute for the unrun coordinator gates.
