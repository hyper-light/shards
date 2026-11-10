# Native partial I/O, held-backend cancellation and byte-integrity mutation

Follow-up to signed6b8d0279230df5e8d18326abccf9f132272d0879. Policy remains
cbd392/df2acf; implementer + assigned adversarial relay tester. ROOT owns
integration, audits, profiles/benchmarks and main. New main0e024 was acknowledged
without rebasing active/frozen sources. The two separately rejected helper
campaigns remain held and are not exercised by this TCP work.

## Actual finite cases

| Private scope / run | Actual outcome |
| --- | --- |
| `~/relay-tcp-forward-cbd3-light-v6`, r64 | GNU/musl library and cfg(test) builds0; four fresh native pressure cases0 |
| `~/relay-tcp-forward-cbd3-light-v7`, r66 | GNU/musl library and cfg(test) builds0; two fresh held-backend receipt/cancel cases0 |
| `~/relay-tcp-byte-mutation-v1`, r67 | One intentional byte-corruption strict101 with actual wrong backend bytes; exact restoration; three fresh restored controls0 |
| `~/relay-tcp-deadline-native-v1` | Existing Deadline/parser class11pass per libc,22 total; same frozen first-checkpoint binaries |

Each scope requested fresh Linux1CPU/1GiB/2GiB admission. Runtime children used
the explicit30000ms original operation deadline and45000ms parent bound. These
are finite fixture envelopes; no production timeout/capacity is selected.

Pressure cases supplied a1MiB binary request,131080-byte response, fixed64KiB
slots, and owned socket send/receive option4096. Kernel readbacks were8192;
these are actual native option facts, not hard aggregate kernel accounting.
Both half-close orders passed per libc. Independent backend/client bytes were
exact; both directions reached `WriteClosed(Eof)`, pending0/extra0. Actual
upstream short sends ranged1–32, actual upstream send-WouldBlock5–89. These
event counts establish exercised correctness paths, not a performance result.
The new `write_would_block` counter distinguishes actual send refusal from read
readiness waits; it does not schedule retries or grant capacity. Its cost remains
ROOT's profiling work. Queue changes are cfg(test), supplied by each case.

The receipt/cancel case first reads the independent backend's exact13-byte
request and EOF. Its worker returns with the actual accepted socket still held
and its write half open; helper completion therefore cannot stand in for EOF.
The client already received the actual8-byte greeting. The relay remains pending
for the unproduced tail; owner cancellation then returns typed `Cancelled`, not
expiry, with positive original time remaining. Both workers join. Request13
read/sent and greeting8 read/sent remain recorded, upstream `WriteClosed(Eof)`,
downstream `Reading`, pending0. This explicitly retains already released bytes;
there is no rollback or no-effect claim. All positive native child FD checks and
parent FD checks are4→4, with held pidfd exit readiness/reaping and unchanged
binary hashes. Kernel resource return remains `Unknown`.

The mutation flips only the first actually consumed client byte before its
actual upstream send. The independent backend receives13 bytes whose first
byte is53 rather than52; it records the wrong bytes before strict failure101.
Both actual joins, parent FD4→4 and pidfd exit/reap are retained. The failing
child does not reach its final FD assertion, so that particular child FD-return
assertion is unverified. This is an intentional integrity defect in a private
copy, not a newly reproduced production bug or a missing-alert result.

`finally` restores shared source SHA256
`ab2ac5712e25dcd7210bdde287f126eda3c8b96ed2101c93c2055629d6fe5951`.
All copied source-file hash differences are empty, and the named-file
`git diff --no-index` is0. This is a source projection and makes no empty-HEAD-
diff claim. Frozen mutant binary SHA256
`9a43485875d95324cb78ec13805de644b30be7074f338384ceceaf2087db7307`
is retained. Restored binary SHA256
`02ffb4874cde8da71770c5fcb1e3bbfb4b552e00f47863353236da9fc0465fa2`
then passes server-first, backend-FIN-first and receipt-cancel in fresh processes.

## Reproduction and retained evidence

[CHECKS.json](CHECKS.json) retains exact input/source/binary/log identities,
native queue readbacks, parsed pressure facts, expected payload seeds and actual
independent payload hashes. The full megabyte pressure stdout/observed bytes
remain at the exact private paths and SHA256 values listed there; they have not
been deleted or replaced by the compact record. Small cancellation/mutation raw
stdout/stderr, exact inputs and observations are copied alongside CHECKS.
The Deadline/parser logs and result are under `deadline-pure`.

`commands` contains the exact submitted Python command bodies. Their existing
paths deliberately refuse overwrite; reproduction uses a new private path and
fresh Kiln admission. The projection commands consume the caller-supplied gzip
JSON source records in `TCP_SOURCE_PACKET_A/B/C`; each record carries path/text/
SHA256. The exact compiled files are listed in CHECKS; the minimal manifests are
the preceding [harness.json](../native-v1/harness.json). No provider/crypto/lint
exception or syscall allowlist change was made. These checks allow unused guest
fixture code in the minimal root and are not the full normal caller clippy gate.

## Production consumer and remaining work

There is still one shared pump in `crates/init/src/relay.rs`; testguest imports
that source. `init::engine::relay` is the actual production Unix/VSOCK consumer.
The added `EngineHalfClose` fixture uses the existing engine socket and records
client-FIN-first/host-FIN-first response EOF plus one actual late-write result.
It compiles in the GNU/musl projection but has **no actual Unix/VSOCK result**
at this checkpoint. Normal old-engine host/helpers and static guest build r61
is active in `~/relay-tcp-engine-compat-6b8-v1`; it preserves the old engine body
and composes host registration only in a new private test root. The shared host
parent is unchanged. Production engine wiring stays uncommitted until the old
behavior is measured; current candidate equivalence is not assumed.

Native failure records now render typed fault/progress and actual join receipts
before asserting, so byte corruption/cancellation preserves its witnesses.
Native reset/extra-data cases, KVM interception and old/new Unix/VSOCK behavior,
further guard mutations and normal guest/helper/clippy/matrix gates remain next
work. Mandatory route/resource/generation obligations remain open.

R01/R04/R06/R07/R13: fixed buffers, checked arithmetic, typed errors, explicit
half/worker/terminal ownership and failure witnesses; normal production caller
validation still pending. R02/R03/R05/R12: c584 and RUNTIME primary source study,
one pump, no cache/timer/new manager/copy threads; all new numeric inputs here are
explicit finite test envelopes/native readbacks. R08/R14: no speed, allocation,
fault/leak or comparative claim. R09/R10/R15: actual independent byte effects,
meaningful mutation/restoration and limited cleanup with raw retained records;
no claim to close unrun gates. R11: native Linux amd64 GNU/musl only, other
targets/VM combinations unverified, macOS ARM64 only, paused WinARM untouched.
R16: no local builds/targets/full worktrees, no deletion of source/evidence or
protected ROOT scope, all remote checks freshly admitted. This is a progress
checkpoint; production extraction/wiring and ROOT review/integration remain owed.
