# Corrected exact post-stop fixture observations

The two ordinary client attach tests pass at signed harness/source checkpoint
`eb034c5710ce5e493872f9e0463cb10a39b10ad4`. Test-only source commits are
`82a6b7fdaaf18aca288dda10f743f845453fcd7d` (matching Exited) and
`4cb5081311affa38e17e6e15299557f8e90df424` (actual ReadStat+ESRCH).
No production or conformance change follows73c85d039 in these corrections.

The rationale is the preserved [root r345 and focused r231 red record](../typed-attach-red/README.md):
root's full corpus independently observed matching Exited after owned stop/wait;
the first narrow correction's focused run then actually observed ReadStat+ESRCH
at that lookup window. Root approved each exact alternative after its actual red.
Neither was called noise or a new production failure.

Both assertions still require the original PID and expected start. Accepted
results after that owned stop/wait are only Exited or Os with exactly ESRCH at
PidfdOpen, RegisterExit or the newly observed ReadStat. CapacityUnavailable,
UnrepresentedIo, other operations and other codes remain rejected. No blanket
Process allowance, retry/probe, timeout inflation, cache or helper framework.

Native Fedora GNU amd64 r234 requested unchanged4CPU/1GiB/2GiB once, admitted with
27/32 idle. Invocation:

```sh
bash docs/research/measurements/managed-proc-main/run-typed-attach.sh attach-v2
```

Actual result: cargo0/wrapper0, **2passed/0failed/0ignored,45filtered**:
`client::tests::attach_takes_back_a_guest_whose_client_crashed` and
`client::tests::attach_takes_back_a_guest_whose_handle_was_lost`.
Only those two tests ran. No full corpus, additional libc/platform, mutation,
profiling or VM campaign was run. Full ordinary composition rerun is root-owned
and remains owed; earlier73 GNU/musl/cross-clippy results are not presented as
this new test revision's whole-corpus gate.

The launcher preserves each phase, forces actual selected source recompilation,
requires Cargo `fresh:false`, and retains exact command/build+test output, source
status/hashes and pinned toolchain. Selected executable SHA256
`b850a5253b84319708bf886fbe1f0fd39584a726b3d8be935b8685ad1f2d22c5`;
immutable copy `~/managed-proc-typed-evidence/attach-v2/test.bin` in the existing
private target/evidence scope. Original [raw archive](original-raw.tgz) SHA256
`7f70695c644331d932a9cbb457253e0fd0e1747d2b94302ab99b9374097f8251`,2866B.
[Raw extraction](raw/) preserves the actual result; selected input hashes match
the committed source. No root/frozen6db target or cache was modified.

Current full f5/d634 policy/implementer receipt and R01–R16 account remain in
[TYPED.md](../../TYPED.md). This is the authorized tiny fixture-correctness
followup, not a new error or authority policy. The ordinary test result contains
its existing stop/wait/echo assertions; no new global resource/lifetime claim is
inferred. Historic errors, raw red records, Mac owned synchronization RED and all
other owed compatibility/interrupt/cost/combined gates remain explicit.
