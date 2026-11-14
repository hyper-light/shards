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
