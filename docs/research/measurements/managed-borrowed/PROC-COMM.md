# Linux borrowed identity and raw process-name bytes

Implementer resource_performance, coordinator root. New isolated branch
`p/managed-borrowed-comm` from `f7604be6`; owns taken.rs, focused Linux borrowed
fixture and this evidence only. Policy full-read receipt: canonical g121-main
AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff,
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns all audit/profile/benchmark/heavy E2E/VM/chaos/sweeps. No subagents,
root-owned targets, main merge, production Mutex/lifecycle changes or credentials.

## Reproduction before production edit

Native Fedora r202 at exact clean test-only
`2dc12b51832daad0e653e95ec1e674f48a3710a8` built successfully, then the ordinary
regression failed exit101. Envelope4 CPU/8 GiB/20 GiB, accepted once with27/32
idle cores. It used new private `~/managed-borrowed-comm-target` and private
evidence `~/managed-borrowed-evidence/proc-comm-baseline`. Pinned Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`/LLVM22.1.8; Fedora
Linux6.19.10 amd64. Command:
`bash docs/research/measurements/managed-borrowed/run-proc-comm-light.sh baseline`.

One owned child of the test binary changes only `/proc/self/comm`, the process
leader's name even from a libtest worker. PR_SET_NAME in that worker would test
the wrong task. Parent obtains its numeric start and Held before the change;
child then uses bytes `6f 77 6e 20 ff 29 20 29 20 fe`, including invalid UTF8,
spaces and two right parentheses. Its raw `/proc/PID/stat` contains those bytes.
The real finite `connect_borrowed` then returns:

```text
actual_CONNECT=false
Process { cause: Io { phase: Start, source: InvalidData("stream did not contain valid UTF-8") } }
own_comm_restored=true start_unchanged=true child_exit=Some(0) workers_joined=true
```

The final assertion fails only after explicit name restoration, unchanged start
check, held liveness check, owned-child reap and worker joins. Muxer readiness
and pre-I/O refusal use a single readiness wait, not repeated accept probes or a
sleep. The original regression was run before any production edit.
Exact raw [proc-comm-baseline](proc-comm-baseline/); archive SHA256
`968ee576d92afebafaa36c1b2754a70993f7da005f858de505d4d51408e8b315`.
Actual freshly compiled binary SHA256
`1da5b8c628f86fd2e6323e44f5fbe8a56770c618a197d1e5b15109b7d44d99aa`;
compiler artifact `fresh:false`; immutable binary remains private as `test.bin`.

Ranked causes after the live red: whole-record UTF8 conversion predicts this
exact Start refusal; wrong-thread mutation is falsified by raw process stat;
PID/start replacement or child death is falsified by held checks and restored
same start; a handshake/muxer failure is inconsistent with Start/noCONNECT.

## Primary semantics and least-change candidate

[Linuxv6.18 fs/proc/array.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/proc/array.c)
calls proc_task_name with escape=false for stat and emits its comm bytes raw.
It computes start time from boot time and emits field22 as unsigned decimal.
[proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html)
specifies the parentheses and field numbering; there is no UTF8 requirement for
comm. [proc_pid_comm(5)](https://man7.org/linux/man-pages/man5/proc_pid_comm.5.html)
documents same-process thread-group naming through procfs and TASK_COMM_LEN16
including NUL; the test name fits that bound.

The approved narrow candidate reads bytes, selects the last right parenthesis
and the20th subsequent whitespace field as before, then validates UTF8 only on
that decimal field and uses the existing u64 parse. The small private field
helper allows malformed/missing/overflow tests without introducing a file-reader
or parser framework. No lossy conversion, new dependency, buffer cap, authority,
deadline, process-handle or ownership change. Whole-String parsing rejects legal
kernel names; lossy conversion is unnecessary; a bespoke reader/cap would broaden
the scope and require independent allocation/bound evidence.

Allocation qualification is **pending**, not an assertion that std fs::read is
universally fallible. [Pinned Rust1.98.1 fs.rs](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/fs.rs)
uses try_with_capacity, but its
[io default_read_to_end](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/io/mod.rs)
initial small probe uses Vec::extend_from_slice when metadata length is zero,
as procfs stat reports. That source observation requires an isolated actual OOM
control before claiming returning allocation failure. The same primitive underlies
the previous read_to_string; its invalid-UTF8 error uses the static INVALID_UTF8
error, so allocating error construction is not established by this repro.
No custom-reader remedy or whole-library panic-free claim is adopted.

## Actual candidate results and allocation blocker

Root explicitly authorized a sequential red→local candidate→new-source focused
job, one unchanged4CPU8GiB20GiB request per concrete source; this does not license
repeated headroom polling or lowering an envelope. Native r204 at exact
`eee20469c7589ca71688eadab25396a00ae2ddbe` (production encoding change6a7fcd6a)
was accepted once with30/32 idle cores, same runner/toolchain/private target.
Invocation: `bash docs/research/measurements/managed-borrowed/run-proc-comm-followup.sh`.

- Real own-child finite borrowed connect passed1/0: actual CONNECT1234, OK reply
  and exact payload `k`; original name restored, same start, held alive before
  exit, child exit0 and all worker joins.
- Valid and malformed/missing/invalid-UTF8/overflow decimal field cases passed2/0.
  The malformed outcomes were checked under allocation refusal and returned
  InvalidData. These test the real private field selection helper.
- Independent old-UTF8-read mutation failed1/101: exact original Start/InvalidData
  and noCONNECT; explicit name restoration/same start/child0/all joins still
  completed before the assertion. Byte-exact taken.rs restoration and actual
  recompile passed the real borrowed test1/0 again.
- Strict GNU API/afunix all-target clippy passed0.
- **Isolated OOM control RED:** the first expression,
  `allocation::fail_after(0, || std::fs::read("/proc/self/stat"))`, aborted with
  `memory allocation of 32 bytes failed`, exit134/SIGABRT. The later started/path
  and valid-field-under-OOM assertions in that same test were NOT reached.
  Source proof of the small-probe allocation is corroborated by the direct
  primitive control; this is not a new production caller stack trace.
  Entire r204 exits1 and the encoding candidate is not R01-complete/adoptable.

The full ordinary corpus was not rerun after the known isolated abort. There is
no skipped-OOM passing-corpus claim. The failing test remains ordinary/unignored.
No extra allocator campaign, profile, custom reader, capacity guess, warmed state
or production remedy for the allocation blocker was introduced. `ulimit -c0`
applied only to the test shell; Bash still printed its signal description.

Fixed/restored binary SHA256
`9afe76665e9fe5034797eef37182c90520fb70075d516c8fe0ae294867c5b00b`;
mutation SHA256
`65e758f6257093a44b4fff92eb53d09633c3188a8d5dbd048e53bda44bfc3ad9`.
All selected compiler artifacts `fresh:false`. Exact binaries remain private
in `~/managed-borrowed-evidence/proc-comm-{fixed,utf8-removal,restored}/test.bin`.
Raw commands/logs/manifests/build JSON/restoration/patch/admission are retained:

| Raw directory | Archive SHA256 |
| --- | --- |
| [proc-comm-fixed](proc-comm-fixed/) | `7eb73c3d850169d173d7db26864ce6534c0b296a009270be5740c8c0b0deb428` |
| [proc-comm-utf8-removal](proc-comm-utf8-removal/) | `68233753279ef0f477f04a0e388fbd3c617930d9a1dce4b4dd6ad49882ddb49e` |
| [proc-comm-restored](proc-comm-restored/) | `d85c77057f29e57b6d9cc6184db0476dc72485194067baf0841d15497d959e42` |
| [proc-comm-followup](proc-comm-followup/) | `72c12cca336e163b7920316dab2526b7c8616953fbccb86faf1ee26fedd0eecb` |

Owed: reviewed returning-allocation solution for the actual initial-probe abort,
then reached path/valid-parser OOM assertions and whole ordinary GNU/musl corpus,
Linux arm64 libc checks, coordinator combined/matrix/VM review. The reproduction
and allocation controls do not validate every process-name/host/version/resource
case, and no general allocation-free/leak-free assertion is made.

## R01–R16 account

R01: actual initial-probe allocation abort blocks completion; no hidden fallback
formatting. R02/R03:
primary raw-comm/decimal-field semantics; existing field22/19 index unchanged.
R04/R05: existing held identity/error phases and original finite deadline remain;
no new production wait/cache/state owner. R06/R07/R13: one private field helper,
byte iteration and rationale comment. R08/R14: no performance claim or agent
profiling. R09/R10: actual pre-fix red and fixed/mutation/raw above. R11/R12:
Linux-only procfs adapter; GNU/musl/arm64 and combined matrix owed. R15: scoped
light units only, root review/integration and heavy validation owed. R16: new
private worktree/target/evidence, historical artifacts untouched. The earlier
GNU timing red and full Mac library OOM abort remain preserved at their named
sources; this task does not fix/warm/skip either.
