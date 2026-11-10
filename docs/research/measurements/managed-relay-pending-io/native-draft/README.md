# Ignored native cancellation witnesses: source draft for ROOT review

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Branch
`p/managed-relay-native-witness-draft` starts at signed proposal
`6b346e2298ca350ca2955a9f7cb57f0a8a2304fa`, independent of the prepared
notification branches. All earlier refs/evidence remain preserved. Full canonical
policy/plan/handoff readback loaded; five canonical files unchanged69dbc→main4bface,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242,
policy filesf5cec365. POLICY_READ and proposal/draft scope sent to ROOT before edits.

This is a reviewable SOURCE DRAFT under approved bounded owned-fixture work.
The earlier [proposal](../NATIVE-FAILURE-PROPOSAL.md) placed a pre-source hold;
preparing the reversible ignored draft makes its review concrete. It does not
adopt source on main or execute native cases. ROOT retains review/adoption,
ALL actual descriptor/socket/VM/failure/mutation campaigns, audits, profiles,
combined gates and main merges. No children/goal expansion/parallel dispatch.

## Three ignored tests, four actual witnesses still unrun

Only a Linux-only module inside existing udp::tests was added. The original
non-test UDP prefix, whole Deadline parent, forwarding source, sock_addr and
host cancel fixture are exact6b bytes; [source-invariants.json](source-invariants.json)
retains hashes. No new CLI/worker/manager/adapter/grant/real-path cancel=None.

Every native test requires explicit positive SHARDS_CANCEL_CASE_MS. Existing
Deadline::new rejects zero/overflow; no default, interval, sleep or grace. The
positive/HUP/ERR attempts retain their original setup deadline. The expiry phase
is a separate deliberately already-expired input created at Instant::now,
reusing the same owned cancellation FD. It changes no positive phase deadline
and does not wait for a chosen duration. No hard wall-time bound on pipe2/setup
or whole-test completion is claimed; ROOT supplies its outer execution envelope.

| Ignored test / phase | Native setup | Required actual result |
| --- | --- | --- |
| pending_token_and_expiry_precede_operation_without_consumption: Pending | Existing eventfd+notify, Deadline same original at / Some(ownedFD) | ConnectionAborted, operation counter0, existing cancellation_observed still consumes the retained token afterward |
| Same test: ExpiredAndPending | Re-notify same FD; explicit already-expired Deadline / Some(sameFD) | TimedOut, counter0, token retained for existing consumer |
| owned_reader_hup_refuses_operation | Own nonblocking/CLOEXEC pipe, drop only its owned writer | BrokenPipe before operation, counter0 |
| owned_writer_err_refuses_operation | Own same pipe type, drop only its owned reader | Other before operation, counter0 |

The operation closure only increments an in-process witness and returns Ok.
It performs no socket/pipe I/O or external commit. Actual result kind, operation
attempts and token/peer facts are written BEFORE assertions, retaining a red's
observed admission rather than only an assertion. This does not replace ROOT's
actual pre-send backend receipt/mutation evidence. The pipe inputs exercise the
native failure classifier; they are not claims that a held eventfd naturally
produces the same HUP/ERR conditions.

pipe2's two successful outputs immediately enter OwnedFd without an intervening
fallible step. No closed descriptor-number reuse, cotenant/process access, new
thread, signal handler or SIGPIPE policy. The test never reads/writes the pipe;
only existing Deadline readiness observes the remaining valid owned endpoint.
FDs remain scoped to the native test and drop normally/on unwind; actual resource
counts and every OS-failure cleanup path remain ROOT-unverified. NVAL native
execution, actual EINTR signals and cancel-after-check race remain unverified.
Existing pure flag/Interrupted tests are not recast as those native cases.

## Basis and alternatives

[Linux v6.18 pipe_poll](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/pipe.c)
and [poll(2)](https://man7.org/linux/man-pages/man2/poll.2.html) provide the
HUP/ERR source basis. Prior researchf6 is **B/79**, exact source spans found;
it supports selecting owned-pipe inputs, not a runtime passing result.
[pipe2's pair/flag contract](https://man7.org/linux/man-pages/man2/pipe.2.html)
and the same kernel source justify the test-only two-endpoint RAII adapter;
researchf8 is **B/76**, both quotes found (full record in research-f8.json). Existing
[eventfd counter contract](https://man7.org/linux/man-pages/man2/eventfd.2.html)
underlies the retained-token consumer witness. Every actual native result is owed.

Pure masks alone do not validate native delivery; extra sockets/threads/timers/
manager or racing invalid FD numbers would enlarge scope. The chosen source
uses the existing Deadline/eventfd consumer and two small test-only helpers
(shared caller-budget parsing and fallible owned pipe creation). Two pipe slots,
one notification and zero admitted attempts derive from ABI/declared invariants,
not production capacity thresholds. No installed artifact/security/trust/crypto
or host setting changes.

## Actual implementer light checks

Own r23, Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1,
NEW private `~/managed-relay-native-witness-draft-light-v1/{source,guest/target}`,
Kiln1CPU/1GiB/2GiB sync:false/jobs1. Policy/owned rustfmt0, GNU37 passed and
musl37 passed, THREE NATIVE TESTS IGNORED IN EACH; both scoped production panic/
unsafe lints0. Those lints are the non-test production subset, not actual native
behavior or proof of all test/OS error paths. All native bodies compiled.

[light](light/) retains24 raw text members, commands/logs/results, Cargo lock/
scaffolds, source manifests and two compiled test-binary identities. All24 member
hashes/five final source+wrapper paths verified locally. In-memory text transfer
8554B SHA256 dd33c50d5dc0c2703e6152fc929fee7c082ad9e1ece0e70a7425aad9f3f56fcd;
no executable/local archive transfer. Remote sources/targets/binaries preserved.
No runtime/test source changed after checks. No native test, socket/VM, campaign,
mutation/profile/benchmark ran in this implementer lane.

No active LOCAL target/cache; no local cargo/build/compile/largecopy/deletion.
Mac floor was restored by ROOT; this lane still used admitted private remote
checks. ROOT clones/targets/caches, frozen artifacts and paused Windows ARM were
untouched. Any source adoption still needs ROOT source-matching artifact/matrix
review; source hashing/ABI identity may change even for cfg(test) additions.

## ROOT-only next gates

[ROOT-CASES.json](ROOT-CASES.json) lists exact registered filters. ROOT reviews
this draft, selects the actual retained source-linked native test binary (or
rebuilds matching source) and supplies positive caller budget/outer timeout.
Run one exact ignored filter with --nocapture/--test-threads=1, retaining source,
artifact/toolchain/kernel/environment, raw outcome/record and actual FD/cleanup
facts. No native result or FD-leak claim is implied by default ignored status.
ROOT owns mutations/restore, combined supported-target gates and native hardware.
All prior strict real-guest before/after/mutation controls remain separate;
no atomic cancellation/commit, race closure, performance or lane completion claim.

## R01–R16 record

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | Production unchanged; asserts/expect are test-only. Unsafe pipe ownership documented, compiled only; actual error/cleanup proof unrun. |
| R02 | Existing proposal, pinned primary contracts and alternatives above; native claims remain unverified. |
| R03 | ABI pair/one token/zero attempts, required caller budget and controlled expired input; no production literal/default. |
| R04 | Actual typed kind/counter/retained-token records precede assertions; expiry/error not permission/Cancelled. Native outcomes still owed. |
| R05 | Existing Deadline/eventfd consumer; no manager/cache/periodic probe/new timer/thread. ROOT audits actual waits. |
| R06 | Three finite ignored tests/four phases with two small test-only helpers; no general framework. ROOT review pending. |
| R07 | Comments state ownership, expired-input separation and ABI facts. |
| R08 | No4–5x/performance result; ROOT measurements owed. |
| R09 | No new defect or production fix. Earlier cd5 actual red/green/mutation remains ROOT evidence for that revision. |
| R10 | Exact source/light/raw/binary identities and ROOT filter list retained; actual native/mutation/cleanup proofs unrun. |
| R11 | GNU/musl amd64 pure/default suites only; native ignored, full matrix/other hardware unverified. macOS arm64 only, Windows ARM paused. |
| R12 | Existing native seam/test-only owned pipe, no production adapter/extra installed artifact/root/grant/security/trust/crypto changes. |
| R13 | Non-test and old fixture bytes unchanged; counter-only operation is labelled, not external effect proof. |
| R14 | No implementer audit/profile/benchmark; syscall/FD/kernel/cost distributions unmeasured. |
| R15 | Draft signed source/light checkpoint, ROOT review/adoption/actual validation/main merge. Persistent goal stays active, no children/auto dispatch. |
| R16 | Same worktree/index, NEW private admitted remote proof, no local heavy work/deletion; protected evidence/artifacts/paused VM retained. |
