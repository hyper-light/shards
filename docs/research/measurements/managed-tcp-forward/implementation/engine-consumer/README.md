# Shared production init engine consumer checkpoint

Policy cbd392/df2acf, unchanged current main33df; implementer + assigned
adversarial relay tester. `Standby::start` → `engine::start/serve/relay` now
calls the single `crates/init/src/relay.rs` pump. Testguest imports that exact
same source. The previous scoped second `io::copy` thread is removed from the
production relay; its existing per-connection serving thread remains. This is
a compiled production consumer candidate. Actual before/after Unix/VSOCK
compatibility and ROOT's integration approval are still required.

The actual `relay(UnixStream, &[u8; TOKEN])` caller signature is unchanged.
Before dialing or HELLO, it fallibly maps one minimum OS page per direction
through `Slots::minimum_mapped()`. No growth, cache, copied default queue depth
or aggregate hard-budget guarantee is introduced. It uses the existing
`run::dial(ENGINE_PORT, true)` and identical header/token bytes in a fixed ABI-sized
stack array. Nothing logs token bytes or changes authentication/grants. The call
is `pump([conn.as_fd(), host.as_fd()], slots, [ByteLimit::PeerEnd;2],
Control::ProcessOwned, EndPolicy::EnginePeerEnd, &mut report)`. The real engine
currently has process/peer lifetime rather than a supplied per-flow timer/cancel
object; this names that contract without inventing a deadline or disabled
`Control::Until` cancellation. Bounded TCP still requires its actual cancel FD.

The host-EOF stop is typed separately from actual Unix-source EOF. The existing
final Unix `shutdown(Both)` remains at this caller. Neither the source model nor
compilation proves native half-close/reset/error compatibility. The actual old
client-FIN-first KVM response/EOF baseline is retained in [the preceding record](../reset/README.md).
The approved one-case normal/no-AS pilot was consumed there; a separate old
host-FIN-first/late-write method extension is pending. No further VM has been
launched and no speculative half-close correction was made in this checkpoint.

r100, private `~/relay-tcp-engine-consumer-c080-light-v1`, retained two distinct
failures. Normal musl guest LTO reached LLVM then aborted under the compiler
harness's1GiB address-space limit. That is a build-harness outcome, with no
runtime actor. Strict normal init Clippy101 separately identified the shared
path module's unused alternate-caller items and one nested `if`.

The two path-import sites now explicitly expect only Rust `dead_code`, with a
reason naming the other actual consumer. Testguest's expectation applies only
outside cfg(test). No Clippy, panic, unsafe or other lint is relaxed. This is the
known caller-subset consequence of the approved path-shared source, avoiding a
new dependency/framework or duplicate pump. Rust's [primary diagnostic contract](https://doc.rust-lang.org/reference/attributes/diagnostics.html#the-expect-attribute)
also diagnoses an unfulfilled expectation. The nested cancellation-FD check is
collapsed without changing its condition, fault or precedence.

r107, NEW `~/relay-tcp-engine-consumer-2586-light-v2`, normal static-musl
`cargo check -p shards-init`0 and strict `cargo clippy -p shards-init --bin
shards-init --target x86_64-unknown-linux-musl -- -D warnings`0 under fresh
1CPU/1GiB/2GiB. r108, NEW `~/relay-tcp-engine-consumer-2586-guest-v1`, full
normal static-musl guest-profile build0 and the same strict Clippy0. Its guarded
4CPU/8GiB/20GiB compile scope reuses the8GiB compiler AS envelope that actually
succeeded in r61; it is unrelated to VM sparse RAM/DAX reservations or product
resource policy. No profile/optimization/TLS setting is changed.

Frozen normal init binary outside target:
`~/relay-tcp-engine-consumer-2586-guest-v1/artifacts/shards-init`,2099624 bytes,
SHA256af0e18735cf7b76f0a3196ed17ddfbfda045000279e967d777b96c7102684162.
`binary-identity.json`, the three exact commands, source identities and
before/check/guest records retain the build chain. Raw logs are losslessly
compressed with original size/hash mappings in `raw-storage.json`. Existing
roots refuse overwrite; reproduction needs NEW owned paths, exact source-packet
variables and fresh headroom admission. Frozen old baseline sources/binaries
and all failures remain preserved.

R01/R04/R06/R07/R13: actual normal caller build/strict lint, fixed ABI storage,
fallible mapping, typed control/terminal/fault state, one shared source. R02/R03/
R05/R12: [RUNTIME.md](../RUNTIME.md) pins the real callers, Linux Unix/VSOCK,
Rust copy and mapping alternatives; OS page and ABI protocol facts derive the
sizes, no learned value grants authority, no polling timer/cache/second pump.
R09/R10/R15: approved missing shared consumer implemented; actual compiler
failures/corrections retained. Runtime compatibility/mutation/control evidence
for these final source bytes is still owed and no before/after behavior pass
is inferred from compilation or earlier shared source. R08/R14: ROOT profiles
allocations/mappings/faults/copies/CPU/latency and comparative targets; unmeasured.
R11: normal amd64 static-musl caller only; full testguest, seven targets and
supported real-VM combinations remain unverified. R16: private admitted remote
scopes, no local build/target/cache, protected root/paused WinARM write or evidence/
source/final-binary deletion. Hard parent/kernel budgets and missing mandatory
internal/IPv6/UDP/Unix-FD route coverage remain open; this caller does not grant
or mediate those routes.
