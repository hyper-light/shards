# Network review fixes, 2026-11-13

Role: implementer, area network, ROOT coordinator. Base
`9efde72524dd626f3ff84e6b9891852d7c810a9b`, branch `p/fix-network-v1`.
Read full fix/common briefs and origin/main's policies before edits.
POLICY_READ SHA-256 receipts:

| Policy | SHA-256 |
| --- | --- |
| AGENTS.md | f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 |
| docs/development-rules.md | df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 |
| docs/development-evidence.md | 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 |
| CLAUDE.md | 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010 |

Scope/order: network-01, 04, 02, 03, 05–11, then clearly derived pieces of 12.
No subagents, local builds, E2E, VM runs, benchmarking or cross-target fleet checks.
The review report retains its original review-time assertions; subsequent statuses
record implementation outcomes separately.

## network-01: received segments must overlap the receive window

The actual `Conn::on_segment` plus its connected host socket is the agreed regression
seam. Red commit `a6ca2403ebb4f79ce8acb8b11aebb8f13ab0d452`, fix commit
`50716d9465c42d9cbabbdd8e725134ba6f80bffe`. The direct deterministic red reproduced
the reported state transition; no speculative alternate-cause investigation or
temporary instrumentation was needed for this small protocol check.

Source: [RFC 9293 §3.10.7.4](https://www.rfc-editor.org/rfc/rfc9293.html#section-3.10.7.4),
table 6 and its fifth/eighth checks. The receive sequence must overlap the advertised
window before ACK/FIN processing; established segments require ACK. A FIN retains
its original position after duplicate bytes are trimmed. Existing exact-sequence
RST/challenge handling remains separate. Window positions use wrapping TCP sequence
arithmetic; zero-window/zero-length ACKs retain their protocol allowance. Length
conversion/addition is checked, and subtraction is guarded by nonzero length.

No chosen timing, capacity, cache, new state representation or new public interface
is introduced. The focused addition uses the existing connection state owner and
socket lifecycle. Existing numeric TCP tuning is network-12, not changed here.

Validation: `adas-mac-studio.local`, Darwin arm64, Rust 1.98.1, one cargo at a time,
`CARGO_INCREMENTAL=0`, jobs 4, debug info disabled, own cold target at
`~/g121-tmp/network-fix-v1/target`. Carrier contains a git bundle, not the big local
worktree; remote source is an independent GitHub clone. Kiln runs:

- r4: carrier setup failure (bundle advertised HEAD rather than the named branch);
  no tests ran. Corrected the bundle ref; not counted as a red reproduction.
- r5: `cargo test -p shards-net stale_fins_do_not_end_the_host_stream` — one test
  failed with `FIN 0 flags 1 was accepted`, exit 101. [Retained output](red01.log).
- r6: `cargo test -p shards-net tcp::tests` — 20 passed, zero failed;
  `cargo clippy -p shards-net --all-targets -- -D warnings` — exit 0.
  Includes stale FINs with/without ACK, valid FIN after duplicate payload, actual
  host-stream continuity, existing retransmission/window/reset/injector tests.
  [Clippy completion](green01-clippy.log).

Merged main `b8da5eca33a57c7b6231725135b7adc8c29ea23f` into
`aaa4f87b4ca360e6675c97948dc35edc602ff687`; r7 rechecked source
`0f5f80816` with 20 TCP tests and clippy passing ([output](merged01.log)). The first
ff push was rejected because main advanced; this is not a landing. The next merged
source is rechecked before retrying. Signing receipts require exactly one gpgsig
on each commit/merge.

### Policy/evidence accounting for network-01

| Rules | Evidence / remaining gap |
| --- | --- |
| R01/R04 | Checked length conversion/addition, guarded subtraction, sequence-space wrapping; existing connection states and refusal/challenge behavior. No production assertion or panic added. |
| R02/R03 | RFC table 6 and ACK/FIN processing define the algorithm; no picked bound/wait/floor introduced. |
| R05/R06/R07/R13 | One connection owns the check; no cache/wait change; small explicit predicate and early refusal, comments cite the protocol obligation. |
| R08/R14 | No performance claim or measurements; profiling and comparative costs remain ROOT's. |
| R09/R10 | Real unit-test red/green at the existing socket seam, revisions/commands/output retained. Heavy live campaigns not run. |
| R11/R12 | Shared protocol code, focused macOS arm64 result only; Linux glibc/musl, Windows amd64/arm64 and real KVM/HVF/WHP validation owed to ROOT/CI. |
| R15/R16 | Exact scoped branch and remote owned source/target; signed commits; no local cargo or new agents. Remote target cleanup after landing, recorded below. |

ROOT owes real-VM known-tuple FIN injection with both blind sequence values, packet
capture proving no premature upstream EOF, and normal close/retransmission on
KVM/HVF/WHP. Existing relevant baseline: `cargo test -p shards --test boot net`;
add the adversarial FIN packet fixture at this socket/VM boundary before claiming
full trust-domain isolation coverage. No such VM campaign was run here.

Landing: `53d5cea99f51798678662143162b04144f67c44a` ff-pushed main after merging
main `3b3a9e7fe`; r9 rechecked that exact source with 20 TCP tests and clippy
passing ([output](landing01.log)). The previous pushes were refused when other
lanes advanced main; no force/rewrite occurred. r10 removed this slice's owned
remote target, exit 0. ROOT notification was rejected by Kiln inbox backpressure
(`pending:4, authority:3`), so acceptance is not claimed.

## network-04: conntrack zone retirement

**Owed to ROOT; not fixed or live-confirmed.** Its decisive red is a kernel NAT
binding being reused, not merely observing u16 arithmetic. The fix brief reserves
real-VM/kernel campaigns to ROOT. The chooser still wraps its zone to 1, and
`program_chosen` sets that zone before conntrack. D122 records why existing tuples
preserve old NAT bindings. No elapsed-time assumption establishes retirement.

Exact starting command on a disposable guest:
`cargo test -p shards --test build a_udp_flow_goes_to_its_ports_next_holder -- --exact --nocapture`.
First extend that test: retain a zone-1 tuple to A, move the allocator to
`u16::MAX` under a test-only fixture, hand the port to B, resend the same tuple
while its old kernel entry is alive, and witness actual A/B receipt. The existing
one-handoff test alone cannot reproduce wrap. A full unforced cycle is ROOT's
bounded campaign, not this implementer's unit scope.

Proposed fail-closed design: a fresh network namespace starts with unused zones;
allocate each once in that namespace, then return typed
`ConntrackZonesExhausted` at the u16 kernel identity bound. The owner must close
admission for the affected port and receive that terminal refusal. Chooser exit
alone is insufficient: nft rules/conntrack outlive its socket. The existing
control has no such terminal notification, so a local checked-add patch is not a
complete fix. No timer, copied 120-second quarantine or counter reset is valid.

Alternative: authoritative kernel deletion/DESTROY observation before reuse,
through ctnetlink, with stale packets fenced before retirement. D122's pinned
guest lacks ctnetlink, requiring a kernel/config change plus request/ack and
event-loss handling. Kernel sources to verify before that broader decision:
`net/netfilter/nf_conntrack_netlink.c`, `nf_conntrack_core.c`, `nf_nat_core.c` and
`include/uapi/linux/netfilter/nfnetlink_conntrack.h`. ROOT owes those R02/R04/R05
ownership/kernel proofs and R09/R10 real reproduction. No blind fix or passing
campaign is claimed.

## network-02: native seccomp on both Linux architectures

Code fix `33d5f8af715ea94021b65a7c8dc793f65a2339a6` removes the x86-only
startup/compile/test guards. Native syscall identities come from libc. The sole
legacy call `epoll_wait` remains x86_64-only; arm64's native ABI provides
`epoll_pwait`. Sources: Linux `include/uapi/asm-generic/unistd.h`,
`arch/x86/entry/syscalls/syscall_64.tbl`, and the existing
`shards-syscall-filter` native AUDIT_ARCH dispatch. No x32/compat normalization,
argument-width change, filesystem grant or arbitrary value is introduced.

The platform reviewer (Kiln agent 01a1d7f2542d723485bfdee7ecd51d8e) confirmed
their platform-05–12 lane does not touch VM confinement or syscall-filter, and
emphasized native numbers, exact architecture dispatch and pointer limitations.
Claude owns VM-process platform-01/02/04; this slice changes only net helper
startup and `net/src/confine.rs`, not VM confinement or syscall-filter.

The assigned Linux-only runner is adacachy2 (Linux x86_64, ample free disk/memory,
nice -n 10). Native ARM red cannot execute on that host. The fix brief expressly
allows an unambiguous source-path fix when real/native validation is owed: old
arm64 always skipped the call. No passing amd64 test is labelled an arm64 red.

Kiln r12 at that fix: `cargo test -p shards-net confine::tests`, then
`cargo clippy -p shards-net -p shards-net-process --all-targets -- -D warnings`,
exit 0 ([full output](green02.log)). Own cold target, incremental disabled,
one cargo at a time. Existing forked test admits used socket/epoll operations
then proves an excluded openat terminates the child with the filter's refusal.
Landlock tests remain distinct. At this initial landing, native arm64 GNU/musl
and carried-helper VM startup were unverified. The GNU follow-up below closes
the native unit-test gap; musl and carried-helper VM startup remain owed, including:
`cargo test -p shards-net confine::tests::the_filter_ends_a_process_that_makes_what_none_makes -- --exact --nocapture`.
Run it on native arm64 Linux, then the normal carried network-helper VM smoke.

R01/R04/R06/R07/R13: same fallible install path and minimal cfg seam; failures
remain io::Result before serving frames. R02/R03/R05/R12: no new tuning,
wait/cache or abstraction, native protocol identities reused. R08/R14: no
performance claim; ROOT owns audit/profiling. R09/R10: r12 supplies only actual
amd64 shared-path evidence; native ARM evidence follows. R11/R15/R16: one
assigned host, no local cargo/VM/fleet checks; signed landing and owned target
cleanup are required, not inferred from this receipt.

### Native ARM GNU follow-up requested by ROOT

Policy re-read at main `994e7715d3677594a323ae58db1037f084890431`, then
`a33644dbef6c967cbe16c98d64723f80d889600b`; hashes above unchanged. Role remains
network implementer. ROOT explicitly assigned Docker Desktop arm64 Linux unit
validation on a permitted Mac. Platform fixes `a729003e9` are included: net uses
`shards-syscall-filter::compile` for its initial default-deny allowlist. The VM's
later default-allow TSYNC restriction layer is a separate operation in that same
compiler crate. No second compiler, ABI normalization or syscall-number copy was
introduced; libc supplies each target's native numbers and the shared compiler
rejects a different AUDIT_ARCH (including AUDIT_ARCH_AARCH64 on arm64).

The regression calls the startup syscall boundary in a forked child, keeps it
alive with a pipe, and reads its actual `/proc/<pid>/status`. It checks both mode
2 and one additional `Seccomp_filters` entry relative to the parent, so an outer
container filter cannot mask an omitted install. The child owns only its pipe
ends; the parent closes the release pipe and reaps it before asserting. Mode 2
is the Linux seccomp protocol identity, not a tuning value. Failure still returns
through the existing io::Result before guest frames are served. No new runtime
wait, cache, bound or allocation policy is added.

Runner: `adas-mac-studio-3.local`, Docker Desktop, native Linux aarch64 GNU,
Rust 1.98.1 (`rust:1.98.1`, local image a8a5f0a1e5fe). Docker's outer filter was
explicitly disabled for the red witness; no host privilege or user daemon was
changed. Own source/target at `~/g121-tmp/network-arm02-v1`, incremental disabled,
four jobs, one cargo at a time. Commands are preserved in
[native-arm02.sh](native-arm02.sh), runnable from a source checkout on a permitted
Mac. The carrier fetched a named-branch git bundle into an independent clone.

Follow-up Kiln runs (IDs belong to the ARM follow-up connection):

- r3 setup failed because the bundle was not yet present; no test verdict.
  A subsequent setup retry was stopped during toolchain preparation and is not
  counted as reproduction. The corrected red owns both pipe-end lifetimes.
- r7, `bash arm02.sh red`, source `46e739f131ca3b93e584d49dc37de03d419b3ca0`:
  `cargo test -p shards-net-process startup_installs_a_native_seccomp_filter`
  failed, exit 101: child Seccomp was 0, expected 2. This restores the original
  x86-only startup gate and is the relevant guard-removal red.
  [Raw output](arm02-red.log).
- r9, `bash arm02.sh green`, source `53e89b2b8843eb39dd2fc00b732d525aeca256c6`:
  that startup test passed (1); then
  `cargo test -p shards-net confine::tests::the_filter_ends_a_process_that_makes_what_none_makes`
  passed (1). Clippy setup then failed because static.rust-lang.org DNS failed;
  the overall run exited 1 and is not reported as a complete green.
  [Raw output](arm02-green-tests.log).
- r11, `bash arm02.sh lint`, same source: toolchain download retry succeeded;
  `cargo clippy -p shards-net -p shards-net-process --all-targets -- -D warnings`
  passed, exit 0. [Raw output](arm02-green-clippy.log).

R01/R04/R06/R07/R13: fallible install, same startup order, explicit child lifetime
in the test, no production panic introduced. R02/R03/R05/R12: Linux ABI identities
and existing shared compiler; no chosen policy constants, cache or new architecture.
R09/R10: actual native red/green and relevant gate-removal witness above. R11:
GNU arm64 unit execution verified; arm64 musl, carried-helper VM traffic and seven
target CI remain ROOT/CI work. R08/R14: no performance claim, benchmark or profile.
R15/R16: scoped remote tests only, no delegates/local cargo; own containers removed
by traps, owned targets removed after landing. Earlier amd64 target cleanup exited
0 (ARM follow-up r10). Final merge/check and ARM target cleanup recorded below.

Some early automatic merge messages in the first slices omitted the requested
coauthor trailer; those landed histories are not rewritten. Later commits and
merges carry it. This is a recorded attribution gap, not a waived requirement.

Native ARM merged-source gate: Kiln r12 could not dispatch before a server timeout;
r14's carrier sync was stopped without a test verdict. A read-only check found
no owned container and the source still at 53e89b2b8. Pushed the signed branch,
then used the prepared clone directly, avoiding the stalled carrier sync.
Kiln r16 checked exact merge `e7822b6b75574ec1b5c5f811ee4f7a359ee3de90`
(main a33644dbe) on Linux 6.12.65-linuxkit aarch64 / Rust 1.98.1:
`NETWORK_CHECK_ROOT=$HOME/g121-tmp/network-arm02-v1 bash docs/research/measurements/network-fixes-v1/native-arm02.sh green`.
Startup test 1 passed, forbidden-syscall test 1 passed, both crates' clippy passed,
overall exit 0. This is the focused tested merge for amended landing contention.

Native ARM follow-up landed at `e37f837e03dba5c4c8f70ffccac379072df9ae19`,
ff-pushed from f7a3edfbc. The first push was refused; new main touched net/inject
and syscall-filter, so amended contention used warm native ARM
`cargo check -p shards-net -p shards-net-process --tests`, Kiln r17, exit 0
([output](arm02-landing-check.log)). No merge conflict occurred; the focused
passed merge remains e7822b6b7. Every follow-up commit/merge has one gpgsig.

## network-03: DNS authority lifetime

Red `5a3d27784` exercises a real connected UDP resolver socket and `Stack::on_udp`:
a TTL-60 answer A permits a new TCP flow; a subsequent answer B should replace A,
but A remains allowed. Kiln r19 on Mac3 Docker Linux aarch64 / Rust1.98.1 failed
exactly `the replaced DNS answer still grants a new flow`, exit101.
[Output](red03.log). Code fix `e2237fb29` changes both UDP and TCP learning;
the subsequent import cleanup and TTL-bound test are recorded by their commits.

Sources: [RFC2181 §§5.2,5.4,8](https://www.rfc-editor.org/rfc/rfc2181.html),
[RFC1035 §§3.2.1,4.2.2](https://www.rfc-editor.org/rfc/rfc1035.html).
The authoritative input is the resolver answer received on the existing connected
transport. Alternatives: keeping a union is contradicted by RRset replacement;
lookup again at connect risks connecting to an answer different from the one
checked; SNI/HTTP assertions do not cover arbitrary granted TCP (D236 alternatives).
Keeping one current expiring answer at this existing owner is the narrow change.
No extra cache/index or independent manager is introduced; this state is necessary
to hold an IP connect to a name grant, not a performance cache.

Keys are normalized granted name plus A/AAAA question type. Bound: at most those
two protocol types per distinct granted name, each parsed from one u16-length DNS
message. Replacing an answer frees the old address vector; the earliest expiry
joins the existing readiness deadline, and timers drop expired answers even when
no further DNS query arrives. The grant's current name/port is checked at admission,
so retained state cannot preserve a removed port grant; NET_POLICY clears learning.
The monotonic deadline uses the shortest received address/CNAME TTL. A high TTL sign
bit means zero (RFC2181§8); no TTL floor, chosen capacity or guessed timeout exists.
Clock overflow and failed vector reservation return io::Error and admit no new
learned authority. Cold start has none; an unanswered name grants no IP flow.

TTL-zero answers retain no authority for a later IP connect. This is a deliberate
fail-closed compatibility limitation: the current stream has no identity binding
that separate connect to the DNS transaction. A future transaction-aware connect
would need an explicit authority token, consumption/revocation semantics and R02
research; silently caching zero TTL is not a substitute. Already established TCP
flows retain their opening grant and close normally; expiry affects new flows.
This slice does not claim to fix network-11's owner/class/CNAME/status validation.

Initial green attempt r20: 2 DNS-authority tests, 1 grant-port test, 1 answer-parser
test and 2 TCP DNS tests passed; clippy failed on the now-unused HashSet import.
[Output](green03-first.log). Removed that import and added parser assertions for
TTL0/1, sign-bit-zero semantics and lowest-TTL selection. The focused merged-source
gate below is required before landing. Harness: [native-dns03.sh](native-dns03.sh).
Same permitted runner, one cargo, incremental0/jobs4, own cold target recreated
after network-02's cleanup; no local cargo or VM tests.

R01/R04: checked clock/length/allocation, current/expired/replaced answer states,
existing wire-policy denial on failed authority admission; production assertions
not added. R02/R03/R05/R12: protocol-derived lifetime/space and existing state owner,
no picked controls or new architecture. R06/R07/R13: one learning operation shared
by both transport callers, expiry joins the existing timer. R09/R10: actual UDP
red; deterministic expiry/history/zero-TTL and TCP/parser regression evidence above.
R08/R14: no profile, benchmark, resource ratio or performance claim; ROOT owns those.
R11/R15/R16: Linux arm64 GNU light checks only; CI other targets, musl and real VMs
unverified, targets removed after landing. Signed commits and merge evidence below.

ROOT owes: `cargo test -p shards --test containers a_networks_egress_is_a_grant_of_its_destinations_alone -- --exact --nocapture`
and `cargo test -p shards --test build a_remote_mcp_server_is_a_grant_of_that_server_alone -- --exact --nocapture`.
Extend with rotating TTL0/TTL1 UDP and TCP answers and fixed live socket count,
witness expired/replaced IP refusal, valid current IP success, established TCP
continuity and host memory returning after expiry. These heavy VM/resource and
adversarial runs were not executed here.

Merged-source green: `309dd3ead659eed817f81183fc9fd73686e551c5`, includes main
31e51b4b2. Kiln r21, same Linux arm64 GNU runner/toolchain: `native-dns03.sh green`
passed 2 authority tests, 1 grant-port test, 2 answer/TTL tests, 2 TCP DNS tests
(7 total), and both touched crates' clippy, overall exit0.
[Full output](green03-merged.log). Policy checker and diff whitespace check passed
locally; no local build/test ran. This is the focused tested merge for contention.

Network-03 landed at `4ce420fd2d5b78c56da35634d25eff798acf81da`, ff-pushed
from c6dd46525. Focused tests/clippy remain on 309dd3ead; contention merges
e0c788174 and 4ce420fd2 changed net injector inputs and each passed warm native
ARM `cargo check -p shards-net -p shards-dns --tests`, r22/r23, exit0
([first](check03-e0c78817.log), [landing](check03-landing.log)). No conflict
occurred. No force push. This slice's owned target cleanup is recorded next.

## network-05-07: guest-kernel and API reproductions

**Owed to ROOT, not fixed/refuted/live-confirmed.** The report's proposed reds
require actual guest eth0/route programming or distinct member namespaces with
kernel socket diagnostics and nft DNAT. Fix/common briefs reserve these VM runs
to ROOT. Source-only observations, synthetic diagnostic byte strings and seeing
an omitted NET_ADDRESS are not claimed as IPv6 receipt/holder reproductions.
Current source at main 4ce420fd2 still has the reviewed paths. No new tuning value,
guest kernel patch, speculative broad refactor or passing VM result is introduced.

### network-05

Starting command:
`cargo test -p shards --test api_image a_published_ingress_port_reaches_its_member -- --exact --nocapture`.
Extend its actual ImageShards/Hex launch with build.rs's
`agents_reach_past_the_microvm_by_ipv6_what_their_networks_grant` IPv6-only
upstream fixture. Run fresh and restored; require the granted IPv6 address/port,
refuse its ungranted address/port, and witness the command cannot borrow agents'
egress. The current ingress fixture alone is not the red.

Design: share the home's engine-ID-derived ULA /48 and reserved last /64 from
`daemon/networks.rs::{ula_base,host_ipv6}` with API-run. One physical uplink value
must drive both guest `uplink6=guest/64,gateway` and network-process NET_ADDRESS,
including bridge IPv4 identity. API plan must keep it through fresh/template and
restored launch, and tell_net must await its matching acknowledgement before the
run starts. Keep existing narrowed NET_POLICY, publication and credential ordering;
keep `confine-eth0` and member gates. Do not silently enable IPv6 for every command
or substitute a chosen address prefix. Changes to home-ID creation need typed
failure and concurrent first-writer handling, not a second independently chosen ID.
ROOT owes the paired-state unit fixture plus real guest route/receipt red before
claiming this design implemented.

### network-06

Starting command:
`cargo test -p shards --test build a_port_comes_in_to_the_member_listening_on_it -- --exact --nocapture`.
Extend to one unmapped 7000–7001 EXPOSE, web only7000 and api only7001, then close
one listener. Repeat TCP/UDP; observe each actual member payload. Existing
single-port ownership cannot establish this failure.

Design: partition each contested exposure at individual protocol-port identities,
preserving unmapped and explicitly mapped destinations. Share allocation is bounded
by u16 port space times admitted candidates, not a chosen cache size; checked/fallible
allocation must refuse the image/start rather than partially install. Selection and
holder generation are per port. `links::program_chosen` must not interpret a newly
split unmapped port as a range mapping to the old low endpoint. Its fallback ingress
rules must exclude all contested intervals, not only original exact tuples. A missing
holder drops that port. Holder close/rejoin must leave the other port alone. Kernel
programming remains one atomic batch; network-04's zone-retirement gap stays separate.
ROOT owes actual nft rules and TCP/UDP receipt witnesses, including range boundaries.

### network-07

Use the same build command, with web `[::]:7000` IPV6_V6ONLY=1 first, api IPv4
wildcard second and an IPv4 client; require api receipt. Then make web dual-stack
and exercise the intended first-listener rule for TCP and UDP. Keep the original
member socket ownership and guest kernel version in evidence.

Primary sources inspected: Linux v6.12
[`inet_diag.c`](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/inet_diag.c)
and [`inet_diag.h`](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/inet_diag.h).
`INET_DIAG_SKV6ONLY` is an attribute after fixed inet_diag_msg, beyond the request's
8-bit idiag_ext range; do not invent a request-bit value or read fixed source bytes
as the option. Parse aligned netlink attributes with checked bounds, validate the
one-byte value and family, and refuse unknown/missing acceptance rather than guessing
false. Verify the pinned guest's emitted attributes for TCP/UDP and non-admin
observability before selecting an implementation. Preserve dual-stack and mapped
IPv4 behavior only where kernel evidence establishes it. ROOT owes that R02/live
version proof and explicit typed observation/refusal handling.

R09/R10 decisive reds remain unrun as required by the role boundary; R02/R04/R05
state/resource/compatibility requirements above are designs, not compliance results.
D99/D122 and finding statuses retain these gaps. No platform or performance result
is inferred from this documentation. Network-03 target cleanup r24 exited0; all
owned ARM test containers are --rm, and no run remains active.

## network-03: native macOS readiness regression

ROOT reported two failures on macOS arm64 at main6389b6e09, first-address admission
at lib.rs4076, before the replacement assertion. Urgent follow-up takes priority;
network08 is held unlanded on p/fix-network-v1 at74ea2d725, and its green run r26
failed GitHub DNS before any cargo. This fix is isolated on
p/fix-network-03-macos-v1, based91cba0180. Full policy/brief readback was repeated;
policy hashes above unchanged, implementer role, owned net test and these records.
The amended brief now stops self-landing after two lost ff-push races.

Native red: Mac3 (adas-mac-studio-3.local), Darwin arm64, Rust1.98.1, own cold
~/g121-tmp/network-dns03-macos-v1/target, incremental0/jobs4. At exact main
6389b6e09671c7a89288fbb2224b1648b5e4dd16, Kiln r27:
`cargo test -p shards-net --profile test-release --lib dns_answers_replace_the_names_previous_authority`
failed first-answer admission, exit101 ([raw output](mac03-red.log)). This is
not the historical replacement red: no first answer had yet been processed.
Earlier Linux arm64 GNU runs passed this test in the test profile; they did not
establish macOS behavior or an optimized-profile platform comparison.

Hypotheses: an immediate nonblocking receive can precede loopback delivery;
otherwise parser/admission or registration would still fail after readiness. The
production dispatcher already waits for the UDP token to be readable before
calling on_udp. The test called on_udp immediately after send_to, which is no
readiness witness. [POSIX sendto](https://pubs.opengroup.org/onlinepubs/9699919799/functions/sendto.html)
does not promise message delivery from successful output. No XNU implementation
mechanism is assumed from the failure alone.

Fix68968478b waits on the actual stack poller, dispatching only its readable UDP
token, for each answer, with the existing test's five-second failure bound. There
is no sleep or platform cfg, and both new-flow admission and stale-address denial
assertions remain. Production code and DNS authority semantics are unchanged.
No public interface, cache, production constant, state machine or architecture
is added. Native green, mutation and merged-source outcomes follow below.

R01/R04/R06/R07/R13: test-only assertions and the same readiness transition as the
real dispatcher; bounded failure if the token never arrives. R02/R03/R05/R12:
existing cross-platform poller and POSIX socket contract, no production policy
choice or copied platform workaround. R09/R10: exact native red and retained
revision/commands, pending green below. R11: native macOS check required explicitly
by ROOT; no skipped platform reported passed. R08/R14: no performance/resource
claim. R15/R16: one permitted Mac, no local cargo/delegation/VM runs; owned target
cleanup after landing. ROOT inbox rejected urgent status with429backpressure,
so delivery is not claimed. r28 was not dispatched before Kiln server timeout;
read-only r29 found old6389 source and no own cargo, so it is no test verdict.

Native merged gate, r30: exact merge18e5e2aa85ae583ccbc9cbcbe75ecbc1b9e91eb2
(main02d3fe33b), Darwin25.2.0 arm64 / Rust1.98.1. Restoring historical-answer
retention with the corrected readiness test fails at its intended final assertion
`the replaced DNS answer still grants a new flow` (test-release, exit101).
The harness requires that message/status and restores the source before continuing.
It therefore rejects an unrelated first-answer or compile failure as mutation proof.
The restored source passes 2 DNS authority tests and 1 grant-port test in
test-release, then shards-net all-targets clippy, overall exit0.
[Raw mutation/green output](mac03-mutation-green.log),
[exact owned-clone harness](native-mac03-guard.sh).
There is no diff in net/lib.rs or dns/lib.rs between6389 and the fix branch's91cba
main base; the correction changes only test synchronization in those files.
Thus the native failure was the test's missing readiness transition; no production
DNS change is needed. An optimized Linux/native-macOS equivalence claim is not made:
the earlier Linux pass used the test profile. No additional broad suite ran.

Urgent macOS correction landed main6df152edcdf3c731d0db450e09f0a916687fc38c
(code68968478b). Tests/clippy ran on merge18e5e2aa8; final6df merge brings only
disjoint daemon/migration inputs from main6a52ef516, so pushed immediately under
the contention rule. Native target cleanup r31 exited0. ROOT notification was
rejected by Kiln429backpressure; acceptance is not claimed. Network08 resumed
only after that landing, on p/fix-network-v1, preserving its unlanded work.

## network-08: independent DNS relay clients

Red7aa3c4a34, Kiln r25, Mac3 Docker native Linux6.12.65-linuxkit aarch64 /
Rust1.98.1: `cargo test -p shards-init dnsrelay::tests::an_open_tcp_client_and_a_lost_udp_query_leave_other_clients_progress`
failed after an open TCP client blocked the second client's UDP query
([raw output](red08.log)). Ordinary loopback sockets, no guest namespace/VM/daemon.
The first TCP query was answered before the other client started, so the baseline
failure is connection ownership, not an unready test listener.

Code74ea2d725 moves TCP connections off the UDP/accept loop into independently
owned workers. Shared Flight state, extracted from agentdns, rewrites UDP IDs and
restores their original IDs/peers on receipt; outstanding queries expire at their
actual deadline. Both resolvers use this state owner and its descriptor-budget
observation; no parallel copy of the UDP tracking implementation is introduced.
The agent resolver's one-second expiry probe is removed: both wait for readiness
or their earliest pending deadline, with poll's millisecond ceiling.

Sources: [RFC7766 §§6.2.1–6.2.4](https://www.rfc-editor.org/rfc/rfc7766.html)
for persistent/concurrent connections and overload close; RFC1035's 16-bit ID
and two-byte message-length fields. Alternative: a second independent UDP tracker
would duplicate agentdns's ownership; a new fully asynchronous TCP framer/connector
would replace an existing working per-connection path. Sharing tracking and the
existing worker style fixes the observed cross-client ownership with a smaller
protocol change. No comparative performance claim is made for that choice.

Bounds/state: complete 16-bit ID space, equal shares by current asker count,
no reuse of an outstanding ID, checked reservation/deadline/count, typed
WouldBlock/AlreadyExists/io errors on admission. Vacancy search is finite even
when all IDs are held. Answer/expiry releases each link count; zero-count keys go.
Per-worker socket budget is three: client, a current UDP query, and an already
retained TCP fallback. RLIMIT_NOFILE minus observed open descriptors determines
capacity once fixed sockets exist, with no chosen concurrency constant. A private
permit counts the worker until return or failed spawn; exhaustion closes that
accepted connection, and allocation failure forwards no untracked UDP question.
Agent TCP's two-socket budget uses the same observation; failed observation now
admits zero, rather than pretending no descriptors are open.

The existing five-second WAIT is retained, not justified as protocol law; it is
network12's owed timing design. FD-derived admission does not establish a memory
cost bound for all threads; ROOT still owes actual stack/heap/parent/kernel cost
and resource-envelope validation. The old per-connection blocking framer retains
its per-I/O timeout behavior, but cannot monopolize UDP or other workers. VM/process
end terminates all workers; a failed poll returns to the relay child's _exit.
No complete exhaustion, fairness, slow-read or cleanup campaign is claimed.

First green dispatch r26 failed GitHub DNS before cargo, then work was held for
the urgent native-macOS correction. Merged6df main into4b7e93f10e9e0dd5ae7b6803004a232ccc1995a4;
Kiln r32 on the same Linux arm64 GNU runner ran 2 dnsrelay tests and 2 agentdns
tests (4 passed), then `cargo clippy -p shards-init --all-targets -- -D warnings`,
exit0 ([raw output](green08-merged.log)). Progress test includes open TCP, a
partial next prefix, a deliberately lost UDP query, and independent UDP/TCP
answers before the first connection closes. Full-ID-space test admits all65,536,
refuses another, releases/reuses an ID, then expires all questions. Existing
per-asker fairness and DNS refusal-format tests pass. Own one-cargo target,
incremental0/jobs4; [exact green runner harness](native-dns08.sh).

R01/R04: fallible admission, finite IDs, RAII worker permit and explicit answer/
expiry/refusal transitions. R02/R03/R05/R12: cited protocol bounds, observed FD
capacity, one shared owner; no new chosen limit or periodic expiry probe. The
inherited WAIT and unmeasured thread memory envelope remain explicit R03/R14 work.
R06/R07/R13: existing TCP framer and connection lifecycle reused, no new public
wire protocol. R09/R10: actual socket red/green and source/log harness retained;
no separate guard mutation campaign run. R08/R14: no benchmark/profile/resource
ratio or low-memory guarantee. R11/R15/R16: Linux arm64 GNU light checks only;
amd64/musl, real guest boots and failures are ROOT/CI's. Cleanup after landing below.

ROOT owes `cargo test -p shards --test containers network -- --nocapture`, with
this fixture added at actual127.0.0.11: retain/drip one TCP client while other UDP
and TCP clients resolve a live peer, withhold one upstream UDP reply, and verify
progress before that connection closes. Add failed spawn, FD/memory pressure,
relay death and guest end witnesses under ROOT's campaign bounds. Existing serial
DNS latency M247 is not such evidence. No such VM/cost campaign ran here.
