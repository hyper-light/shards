# G22 borrowed connector: pre-fix evidence and implementation proposal

Current candidate, results and remaining checks: [CANDIDATE.md](CANDIDATE.md).
The pre-implementation record below describes signed `9a29652d`: live deadline
failures reproduced before production connector/allocation changes.
This branch starts at combined `cbe3d027`. Coordinator owns
profiling, benchmarks, audits, heavy E2E, platform sweeps and final integration;
this implementer owns `crates/api`, the necessary focused `afunix` adapter/tests,
and this directory. No frozen RPK artifacts are changed.

Policy receipt: canonical g121-i7 `c921a0b12b06d495b354907162e991376edffd41`,
R01–R16 rules SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
AGENTS, full rules/evidence, CLAUDE, managed plan (G22) and current handoff read;
role implementer. The latest user instruction limits this lane to implementation
and light tests; older performance-lane authorization does not supersede it.

## Exact native red

Reference-only Hex patch SHA256 is in [reference.sha256](reference.sha256).
`old_patch_handshake.rs` is extracted from that applied patch's handshake, changing
only its function visibility for this private test module. `red.rs` exercises its
borrowed zero mapping and overflowing deadline, plus the existing production
handshake's positive-budget dribble. It is not an alternate production transport.

Source `09e1a482e7f333e9abb4e88bafbe7cdbda6a7155`, kiln r171, Fedora runner
`8fadc1ec213332e16cb0d915a3fb913f`, Linux6.19.10 x86_64, native AF_UNIX.
Assigned envelope: 4CPU/8GiB/10GiB; private source
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-borrowed-c5f6175fcc`,
private target `~/managed-borrowed-target`, evidence `~/managed-borrowed-evidence`.
The target's `.rustc_info.json` and repo-context `rustc -Vv` confirm Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. A later home-directory
query selected the unrelated default1.99.1; it is not r171's compiler identity.
The r171 executable SHA256 is
`008eb57ca43a91bcb9ca7acb638ffa9300c699b14dbb7fea112c06c31f7d7293`.

Exact commands, from the private synced source:

```sh
export CARGO_TARGET_DIR="$HOME/managed-borrowed-target"
git rev-parse HEAD > "$HOME/managed-borrowed-evidence/red-fixed-source.txt"
cargo test --locked -j4 --profile test-release -p shards-api --lib --no-run \
  > "$HOME/managed-borrowed-evidence/red-fixed-build.log" 2>&1
timeout 20 cargo test --locked -j4 --profile test-release -p shards-api --lib \
  borrowed_repro -- --test-threads=1 --nocapture \
  > "$HOME/managed-borrowed-evidence/red-fixed.log" 2>&1
result=$?
printf '%s\n' "$result" > "$HOME/managed-borrowed-evidence/red-fixed.exit"
test "$result" -eq 101
```

The wrapper expects cargo101 because these are pre-fix assertions of the required
contract. All three tests actually failed, none skipped, in0.91s:

| Case | Actual effect | Required behavior |
| --- | --- | --- |
| Reference zero patience | Fake muxer received exact `CONNECT 1234\n`; worker still awaited withheld answer after200ms | Expired before I/O |
| Reference `Duration::MAX` | Same actual request and withheld wait; `Instant::checked_add` overflow became no deadline | Typed overflow refusal before dial |
| Existing positive200ms handshake | Dribbled valid `OK 7\n` accepted at501.503476ms | One finite total deadline across connect/write/read |

These are one-shot correctness examples, not timing distributions or performance
claims. The200ms input reuses the existing conformance `UNSERVED_PATIENCE`; each
dribble byte is deliberately delayed by half that input, with no state polling.
The outer20s watchdog bounds fixture failure only, never production behavior.
Zero/overflow workers are explicitly released after observation and both fixture
threads joined. No Guest, VM, provider, credentials or lifetime owner was created.
FD/task-return measurement has not yet been run and is not inferred from joins.

[Raw logs](r171/red-fixed.log), [build](r171/red-fixed-build.log),
[source](r171/red-fixed-source.txt), [exit](r171/red-fixed.exit) retained.
The earlier r170 source `fbbdb107` did **not** compile: extraction omitted the
reference file. Its [build failure](r171/red-build.log) and
[source](r171/red-source.txt) are retained, not counted as a live reproduction.
The two const-temporary warnings in r171 were subsequently fixed in the harness;
that edit has not yet been tested in the recorded r171 binary.
The three historical red tests are now explicitly ignored by default, so they do
not make the ordinary corpus permanently red. Replay this updated harness with
`borrowed_repro -- --ignored --test-threads=1 --nocapture`; the retained r171 source
predates that ignore annotation and its three tests ran without `--ignored`.

## Allocation alternatives before production redesign

The proposed public borrowed API is non-owning: checked finite deadline, held
process identity, shared handshake, post-handshake held-process check, returned
stream. It never constructs Guest, owns a process lifetime, spawns a watcher,
signals/stops, exports or copies a VM. Owned Guest's documented zero/MAX semantics
remain intentional (D215); the borrowed contract differs explicitly.

Current Socket allocates two std Arcs for shared timeouts/nonblocking state, then
the connector/clone boxes Socket infallibly. Making only the outer Box fallible
does not establish R01. A reserve followed by infallible Arc/Box allocation also
does not establish it: the second allocation can still fail.

| Alternative | Primary source and consequence | Scope/decision |
| --- | --- | --- |
| Stable std Arc/Box `try_new` | [Arc](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.try_new), [Box](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.try_new): allocator_api remains nightly in1.99.1 | Reject toolchain/API change for this slice |
| fallible_collections stable FallibleArc | [0.5.2 source](https://docs.rs/fallible_collections/0.5.2/src/fallible_collections/arc.rs.html) warns its stable implementation can panic: fallible Box is converted through ordinary `Arc::from` | Reject incomplete fallibility |
| fallacy-arc0.1.1 | [published crate](https://docs.rs/fallacy-arc/0.1.1/fallacy_arc/) source `src/lib.rs` unconditionally enables `allocator_api` (verified from the official static.crates.io source archive) | Reject nightly-only implementation |
| Triomphe0.1.17 | [pinned Arc source](https://raw.githubusercontent.com/Manishearth/triomphe/v0.1.17/src/arc.rs) uses checked layout and null-check for `try_new`; [manifest](https://raw.githubusercontent.com/Manishearth/triomphe/v0.1.17/Cargo.toml) MSRV1.85, no mandatory dependencies with default features disabled; Servo-derived, no Weak support | Candidate one private shared state for Socket's existing three atomics; enable only std (its no_std abort fallback uses panics); no workspace Arc migration |
| allocator-api2 Box0.2.21 | Already in Cargo.lock; pinned local registry `stable/boxed.rs` `try_new` delegates `try_new_in(Global)`, `stable/alloc/global.rs` calls the global alloc and checks null | Candidate direct dependency for Box only; no shared Arc available there |
| Fixed-Socket global allocation | [Rust Box safety/docs](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.from_raw) explicitly permit global `alloc(Layout::new::<T>())`, write initialized T, then Box::from_raw; null must return typed error | Smaller Box-only candidate: private Socket-specific helper, no generic allocation framework or guessed Arc representation |
| Bespoke refcount/Arc layout reconstruction | Would duplicate mature synchronization and depend on hidden layout | Rejected by coordinator; not implemented |

Triomphe's ordinary Clone retains an abort branch above `isize::MAX` references.
That precondition is **not** waived as unrealistic. Before adoption, the candidate
must prove private Socket sharing cannot reach it, including forgotten returned
boxes and concurrent clone temporaries, or return another safe design. Only this
module would hold the Arc, clone only after successful OS socket duplication and
fallible returned-Socket allocation, never expose an Arc clone/into_raw API, and
never detach an Arc count from the corresponding owned socket/box. No capacity or
refcount cutoff is proposed. A numerical process-handle assumption is not a proof;
[Microsoft](https://learn.microsoft.com/en-us/windows/win32/sysinfo/handle-limitations)
does not specify a universal fixed handle limit.

The fixed-Socket Box path must use its exact nonzero Sized layout, test allocation
failure, write exactly once, and transfer ownership once to std Box for normal
drop/deallocation. The allocator-api2 conversion alternative uses the same Global
allocation/layout, but adds a direct dependency merely to hide that small helper.
No allocation redesign has been adopted at this checkpoint.

The error path must stay nonallocating: a typed BorrowError and narrowly deepened
handshake/identity causes, not newly formatted strings. A legacy String Refusal
bridge needs a typed allocation outcome if fallible rendering fails, rather than
allocating a fallback. [`io::Error::new/other`](https://doc.rust-lang.org/std/io/struct.Error.html#method.new)
allocate even for static-string payloads; `ErrorKind::OutOfMemory.into()` has a
nonallocating representation. No new OOM error boxing is allowed.

The Linux identity query needs removal of path/error `format!` allocations, but
does **not** need a copied read framework: [Rust1.98.1 fs::read_to_string](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/fs.rs)
already uses `try_reserve_exact`, and the [shared read loop](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/io/mod.rs)
uses fallible growth. This is source evidence, not an injected-OOM passing test.
Unix must reuse taken::Held; Windows needs only query+synchronize held-handle RAII.
The current Windows afunix local clone converts WSAPROTOCOL_INFOW to Vec; a narrow
stack-based local duplication path is a candidate, preserving external share APIs.

## Pre-implementation contract and owed checks (9a29652d)

One absolute finite deadline covers borrowed connect, request write and all reply
bytes; expiration/overflow is typed and zero is rejected before held-handle/dial
I/O. Stack request/reply bounds derive from protocol words and decimal u32 bounds,
not measurements. Handshake failure retains phase, retry class and OS cause as
types. Shared owned-client adapters preserve zero/MAX and existing stream clone,
timeout, nonblocking, half-close and handle behavior. No new cache or periodic wait.

Production implementation, green fake-muxer cases, independent guard removals,
identity mismatch/dead/exit-during-handshake, malformed reply, byte/half-close and
clone behavior, deterministic allocation-failure return/cleanup, complete light
API corpus, focused afunix tests, fmt/check/clippy and policy script are **owed**.
Coordinator additionally owns seven-target/real platform gates, native repeated
Windows handle counts, actual VM borrowed attach/lifetime/drop behavior and all
performance/parent+kernel accounting. No leak-free, zero-allocation, speedup or
managed-runtime-completion claim is made.

R01: production unchanged; listed implicit allocation hazards unresolved.
R02: primary alternatives above; adoption pending review and actual injected OOM.
R03: test inputs/outer watchdog explicit; production bounds proposed from protocol.
R04: proposed finite deadline and typed phases; production states not added yet.
R05: reuse held identity/stream/handshake, no cache/watcher; existing per-byte wait
renewal actually reproduced. R06/R07: test-only focused hook/reference provenance.
R08/R14: no profiling or comparative claim; coordinator-owned work owed.
R09: three native actual failures retained. R10: raw source/build/result plus
repeatable fixture committed; green/mutations owed. R11: native Linuxamd64 only,
all other targets unverified. R12: shared seam with minimal OS handle adapter
proposed, not implemented. R13: no manager/framework introduced. R15: isolated
signed checkpoint, no main merge, no children/heavy campaigns. R16: private target,
no unrelated cleanup; Mac builds not used.
