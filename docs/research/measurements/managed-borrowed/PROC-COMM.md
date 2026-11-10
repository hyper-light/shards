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

## Validation status and R01–R16

Repro is actual red; encoding candidate and focused valid/malformed/allocation
checks are not yet run. The approved next light job preserves one4CPU8GiB20GiB
request per new concrete source, one old-UTF8-read mutation and byte-exact
restoration. A headroom refusal will be retained without lowering/retrying.

R01: allocation qualification pending, no hidden fallback formatting. R02/R03:
primary raw-comm/decimal-field semantics; existing field22/19 index unchanged.
R04/R05: existing held identity/error phases and original finite deadline remain;
no new production wait/cache/state owner. R06/R07/R13: one private field helper,
byte iteration and rationale comment. R08/R14: no performance claim or agent
profiling. R09/R10: actual pre-fix red/raw above, fixed/mutation pending. R11/R12:
Linux-only procfs adapter; GNU/musl/arm64 and combined matrix owed. R15: scoped
light units only, root review/integration and heavy validation owed. R16: new
private worktree/target/evidence, historical artifacts untouched. The earlier
GNU timing red and full Mac library OOM abort remain preserved at their named
sources; this task does not fix/warm/skip either.
