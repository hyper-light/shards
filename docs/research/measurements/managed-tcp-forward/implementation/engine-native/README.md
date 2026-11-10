# Native Unix engine EOF compatibility correction

Policy cbd392/df2acf, unchanged current main33df; implementer + assigned
adversarial relay tester. This follows the compiled [production consumer](../engine-consumer/README.md)
at3bbdd6105. The actual `init::engine::relay` and testguest retain one shared
pump. No main merge or overall managed-runtime completion is claimed.

## Actual old-body comparison and red

[original_body.rs](original_body.rs) is the exact605-byte post-HELLO copy body
extracted from cbd392's `init/src/engine.rs`, full source SHA256e2e4e6e6.
[baseline-identity.json](baseline-identity.json) records both hashes. The adapter
only supplies owned UnixStream/File endpoints instead of dialing/authenticating;
the remaining body bytes are unchanged and verified after formatting. It is a
frozen measurement baseline. Production has one shared implementation.

The finite test uses owned unnamed Unix socketpairs, fixed caller request13,
greeting8/tail6/late58, actual nonblocking endpoint reads/writes, original
30000ms endpoint deadline and45000ms parent bound. It compares both original
and shared paths in each test. The shared path uses the actual engine's mapped
slots, ProcessOwned lifetime, EnginePeerEnd policy and caller error cleanup.
Its parent owns a held pidfd before release. It exercises Unix semantics without
VSOCK, HELLO/token validation or VM/routing coverage.

r113, private `~/relay-tcp-forward-cbd3-light-v12`, compiles GNU/musl0. Actual
static-musl client-FIN-first passes0. Host-FIN-first then returns strict101:
after the same complete14-byte response and native EOF, the original body gives
`BrokenPipe`/errno32 for the one late write, while the shared3bbd candidate
accepts1 byte locally (client written14 versus13). Both independently observed
backend prefixes remain13, extra byte absent; this is local socket acceptance.
Each path joins, returns FD4→4 and retains about29.999s of the original deadline.
The parent observes exit readiness/reap/FD4→4. Both observations precede the
strict mismatch assertion. GNU was compiled but not executed after that first
musl red. The earlier actual KVM client-FIN-first pass remains a separate proof.

## Existing seam and correction

Previously the pump published client EOF through write-only shutdown, then its
caller shut down both Unix halves after the pump returned. The measured late
write entered that gap. `Direction::turn` now receives the existing EndPolicy.
Only `(EnginePeerEnd, Backend, actual Eof)` selects native SHUT_RDWR when the
response tail is drained to Client. Independent TCP still uses SHUT_WR. The
engine caller's full-shutdown fallback remains on errors; successful EOF already
ended both Unix halves. No extra success syscall, state owner, callback, periodic
probe, timer, cache, framework, grant or deadline is introduced. Typed PeerEnded
versus source EOF and already consumed/written prefixes remain intact.

The pinned [Linux6.18.48 Unix implementation](https://github.com/gregkh/linux/blob/v6.18.48/net/unix/af_unix.c),
blob52b17331e9d5124c52f108c2fc25c89ea2aa47d4, `unix_shutdown`, maps the selected
mode to separate shutdown states and applies the peer states before waking it.
This supports the existing full-shutdown operation at EOF publication. It does
not establish atomic external commitment or rollback. Keeping shutdown in the
later caller has the actual red above; a new callback or two separate shutdown
operations would add interface/work without supplying a needed behavior.
Actual host kernel is Fedora44/Linux6.19.10; the6.18.48 source is the planned
guest-kernel primary, not a claim this native run booted that guest.

r116, NEW private light-v13, compiles GNU/musl0 and runs four exact tests0:
both close orders per libc, each with original and shared paths. Host-FIN-first
now yields actual BrokenPipe32 on both paths with client written13, backend13,
complete14-byte response/EOF, up PeerEnded/down Eof, joins and FD/pidfd/hash return.

r117, NEW `~/relay-engine-unix-eof-mutation-v1`, changes **only** SHUT_RDWR to
SHUT_WR in that new selection. Strict101 again records original BrokenPipe32
versus shared Accepted1, actual independent13/14-byte receipts and all owned
joins/FD returns before asserting. `finally` restores shared SHA256
083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27;
all source hash differences are empty and named-file diff0. Frozen mutant
198eeeb0c96ec63c41fb8c9f8c8d5f36e8e410128f39a16a9b2f4855509d4406 and
restored222483025120546df6f4a84bd344a5a892fefa8dc70c77a140733c951b0ac7d7
remain outside target. Restored Unix both orders and ordinary TCP both orders0.

Four other r117 controls initially received BackendFinFirst despite requiring
ServerFirst and strictly failed **before opening sockets**. Their logs/results
remain unchanged HARNESS outcomes. A NEW `resupplied-controls-v1` record supplies
ServerFirst to those exact unchanged source/binary tests: extra-byte, backend
reset, client reset and receipt-cancel all actually pass0. Eight distinct restored
controls therefore have passing executions across the original and resupplied
records; r117's original overall exit1 is retained, never rewritten as0.

r118 normal static-musl init guest-profile build and strict Clippy pass0. Frozen
corrected init `~/relay-tcp-engine-consumer-3bbd-guest-v2/artifacts/shards-init`,
2099624 bytes, SHA256eadd299957630177bf310783a5f107902171ce103fcd03390e01d68df4d62e57.
The build uses the previously successful r61 remote compile envelope, unchanged
guest profile and kernel/security settings. The old AS-limited build failures and
normal KVM harness outcomes remain retained in their earlier records.

The final fixture source also has GNU/musl projection Clippy0 with exact workspace
lint flags and the unchanged repository clippy.toml. Earlier projection runs
retained write-with-newline and misplaced safety-comment diagnostics; their
corrections preserve output bytes and unsafe obligations, with no lint allowance.
r133 then rebuilds the final source and passes16 actual tests0: Unix both orders,
TCP both orders, extra byte, receipt cancellation and resets from both origins,
per libc. Each retains actual bytes/errors, FD returns, held pidfd exit/reap and
unchanged binary identity. This projection is separate from full testguest lint.
Final fixture SHA2560e346df4a8e0885aa754e96d0eecfa7730bc75343007bd5aad734fe6773f5efa;
frozen native musl e2eb1fe659e1a9845041001b2102cec886f9827052fb0d1c0df4e4612ae15bd1,
GNU d251660f2ae1838b5b0b7d68610ab9d3bd6cd9275b9a2710aef1aac834a505fa.

r114 separately completes the earlier3bbd **pre-correction** normal host/carried
helper build and full static-musl testguest build0. Its source/target is preserved
at `~/relay-tcp-engine-after-3bbd-v1`; frozen host test, main binary and guest are
outside target in `artifacts`. It supplies no runtime pass or final corrected
caller gate. The actual original old engine6b8 baseline remains independently
retained; neither source/target has been rebased or overwritten.

## Identities, reproduction and limits

[CHECKS.json](CHECKS.json), exact command bodies and [raw-linux.tar.gz](raw-linux.tar.gz)
retain before/after/mutation/resupplied/normal identities and all original logs.
Archive85013 bytes, SHA25643d9f44a8b7f5227cd2fbe853c4715f093de784e63fd9a1bc9ebcee28e8e0829;
152 regular safe-path members,151 original size/hash entries plus MANIFEST.json,
all locally verified. [raw-manifest.json](raw-manifest.json) preserves the manifest.
Source bytes for both candidate phases are retained inside it. No whitespace,
CRLF or libtest EOF normalization occurs. Commands require their recorded source
packet variables, NEW private paths and fresh admission; existing roots refuse
overwrite. Rust1.98.1/libc0.2.189; native scopes1CPU/1GiB/2GiB, one test thread.

[FINAL-CHECKS.json](FINAL-CHECKS.json) and [raw-final-linux.tar.gz](raw-final-linux.tar.gz)
retain the two projection lint failures, final strict lint/16 native cases and
the separate pre-correction full build. Archive42224 bytes,
SHA256b7b7fca1a625317d60acdad3fbbb756ad426ea650b19710387c89e95ed123cad,
120 regular safe-path members/119 original hashes plus MANIFEST, all locally
verified. [raw-final-manifest.json](raw-final-manifest.json) preserves that map;
the final fixture bytes and native/full-build frozen binary identities are inside.

R01/R04/R06/R07/R13: actual static-musl normal init build/strict lint, borrowed
FD safety, existing typed end/cause/error state, focused source diff; assertions
and frozen copy threads occur only in the baseline/test. R02/R03/R05/R12: pinned
Unix/VSOCK/Rust sources in [RUNTIME.md](../RUNTIME.md), actual differential,
existing policy seam and caller OS/ABI facts; no guessed allowance or new manager.
R09/R10/R15: actual native red before correction, green, exact guard removal
and restoration, independent byte/local-write/limited cleanup witnesses, harness
failures retained. R08/R14: no profiling, comparative performance or allocation/
kernel-cost claim; ROOT measures. R11: amd64 native GNU/musl and normal musl
init only; full testguest, matrix, actual VSOCK and before/after KVM remain owed.
The separately requested old host-FIN-first normal/no-AS method extension is
pending; no second VM was launched. R16: no local build/target/cache, no protected
root cache/source or paused WinARM write, frozen evidence/source/binaries retained.
Physical/kernel resource return and hard caps remain unverified. Full routes,
producer charges and overall managed-runtime closure remain open.
