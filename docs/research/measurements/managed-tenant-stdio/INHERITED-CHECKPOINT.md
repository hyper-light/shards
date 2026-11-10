# Inherited writer case A: source and compile-only checkpoint

Historical daaed7e01 source/hash record. The subsequent
[envelope checkpoint](ENVELOPE-CHECKPOINT.md) adds effect-free host/guest byte
calculation checks and the later ROOT r696 normal/stdio receipt. A's actual
owned-child and cleanup gates remain ROOT-owed.

IMPLEMENTER `01a1c5d28cea74a5b0241200190d6c54`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Canonical full policy/plan/handoff re-read at
`1d0588b28f839fd0fec049e4bcb00e261e28c3aa`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242` unchanged.
ROOT's later main `eb5654d6f` composition instruction is acknowledged. This is
fixture source, not a reproduced product correction, actual VM pass or lane closure.

Branch `p/managed-tenant-inherited` starts at the frozen B checkpoint
`33508dc14267cc51dbabec958c4987494a0e5a1a`. B's host/native bodies remain byte
identical except the required None task argument at its existing start call; the
new child module declaration is outside its bodies. The strict terminal function
is byte-identical. Neither B branch nor its signed checkpoint is amended.
Crash875 and reconnect0f0b remain separate preserved candidates.

## Contract, inputs and ownership

The original [proposal](PROPOSAL.md) preceded new helper mechanisms. The concrete
parent/child sequence and retained-Unknown cleanup were sent to ROOT with stable
key `tenant-inherited-native-shape-v2` before this source implementation.
Literal modes are:

```text
managed-capture TOKEN inherit-stdio PORT WORK_MS CLEANUP_MS CHALLENGE_BYTES REPORT_BYTES
managed-capture TOKEN inherited-child PORT WORK_MS CHALLENGE_BYTES REPORT_BYTES
```

The parent validates positive finite port/work/cleanup/challenge values, checked
deadlines and the report envelope before capture/spawn. It emits its controlled
prefix, fresh-execs exactly one owned Child using std::Command, piped child-read
stdin and explicitly inherited stdout/stderr. No shell, raw fork, pre_exec or
new thread/manager. It opens its pidfd only after spawn, so that new parent handle
is not inherited by the child. The private stdin pipe carries one release byte
only; it conveys no parent management handle.

Parent state is Spawned → HeldPidfd → StdoutClosed → StdioClosed → Released →
ExitReady → Reaped(actual Child.wait status). After each own close, F_GETFD must
return EBADF before advancing. Only then does the parent write the private byte
and promptly close its writer. No parent stdio is used afterward. The parent
retains sole Child wait ownership; successful status is required for its exit0.
No next child can be admitted, even on failure.

The child validates its local finite envelope, waits on the private byte, closes
and verifies its own stdin slot before binding its explicitly declared listener,
then emits `TOKEN-inherited-ready` on both inherited capture writers. The host
requires those bytes after the parent prefix. Only after independent main and
new sibling fd1/fd2 progress does the host send its fresh supplied challenge.
The child echoes the exact tagged u32-length frame, closes that connection,
emits `TOKEN-inherited-after-release` on both inherited writers, flushes and
returns0. Its local deadline cannot extend the parent's original work deadline:
the parent still owns the earlier authoritative timeout and cleanup.

The parent work and cleanup deadlines are fixed before effects. Acquisition,
close, release, readiness and wait failures close the private writer and attempt
kill of only this owned unreaped Child, then held-pidfd readiness and actual wait
under the original cleanup deadline. Kill success/refusal is not reaping proof.
If cleanup cannot establish reaping, the parent retains Child/fd/error state
and waits only for existing outer owner stop; it does not detach/drop ownership,
renew a deadline or claim cleanup. This bounds the cleanup attempt, not an
independent guarantee that the outer owner has already terminated the population.
No core fault, signals-to-other-tenants, host setting or privilege/profile change.

Native failures present the fixture stage as an exit code when return is possible.
Typed io::Error and cleanup status are retained internally; they are not exported
after closed stdio. The exact remote cause, raw parent wait status and uncertain
cleanup therefore remain Unknown to the host unless ROOT obtains independent
actual held-lifecycle evidence. An output End cannot supply that missing proof.

The host new leaf reuses B's supplied explicit ports/deadlines/challenge/record
inputs and adds positive u32 `SHARDS_TENANT_STDIO_TASKS`. It recomputes the larger
capture/control bound before starting a Fixture. A's control exchange lives in
its own leaf to preserve ROOT's frozen B body; it uses the same existing finite
socket IO semantics, with its different causal milestone recorded accurately.
No second lifecycle owner or product network/observer implementation is created.

The tiny approved parent extension keeps optional task and publication inputs
distinct: `agent_tasks: Option<u32>`, `publish_ports: Option<&[u16]>`. Actual
`agentfile.rs::domain` accepts numeric processes only as positive u32; source was
rechecked at lines365–369. A passes the caller's Some(tasks); existing default,
normal and B callers pass None. No task number is picked. Publication continues
to use actual two caller ports, checked repeated NETWORK ingress flags and exact
EXPOSE/CONNECT grants with existing loopback OS-assigned `-p` host publication.
No counter/header/module/global-allocator or cost-fixture parent edits.

Host acceptance requires exact parent+ready bytes; new sibling fd1/fd2 and main
progress before child release; exact child echo and suffix; then canonical End,
ACK and actual transport EOF on both actor channels. It uses the unchanged three
read goals and sole final sibling ACK+FIN. Raw chunks, commands, metadata and
teardown outcomes precede assertions. It then attempts existing owned stop,
inspect/remove, owner shutdown and observer joins under a fixed cleanup deadline,
and invokes the unchanged strict ended/complete versus aborted/unknown function.
ROOT's normal r668 metadata red and refactor-owned correction remain separate.

ROOT subsequently reported actual r692 clean-main26ba, raw
`~/root-managed-terminal-before-26ba-v1`: source-matching host/carried-helper build0;
normal and B each test101 ONLY at that strict final-metadata defect. B's two
canonical End/ACK/EOF, later UUID16 challenge in an exact21B frame+peerEOF,
visible live actor and new sibling fd1/fd2/main progress succeeded. Protocol,
remove, owner join and observer FD7→7 cleanup succeeded for both. This is ROOT's
actual B protocol/lifetime-separation evidence, not an A run or a full B pass.
The unchanged terminal assertions remain mandatory while refactor composes its
narrow correction from this exact baseline. No privilege/profile was widened.

The report bound is the sum of three controlled fd1/fd2 payload pairs plus one
tag1/u32-length challenge request and echo. It is a fixture byte bound, not hard
aggregate resource accounting. Record files, host copies, child/parent state,
pidfds, pipes, socket buffers and kernel charges require ROOT's actual measures.

## Primary contracts and alternatives

Sources were re-read on2026-11-10, not executed:

- [Linux pipe7, man-pages6.19](https://man7.org/linux/man-pages/man7/pipe.7.html):
  EOF depends on closure of every referring writer; a pipe is a byte stream.
- [Rust1.99.1 Stdio](https://doc.rust-lang.org/std/process/struct.Stdio.html):
  inherited corresponding writers and one private stdin pipe at the existing
  std::Command seam. No unsafe post-fork Rust/pre_exec is needed.
- [Rust1.99.1 Child](https://doc.rust-lang.org/std/process/struct.Child.html):
  dropping Child does not stop or reap it; actual wait returns its exit status.
- [pidfd_open, man-pages6.19](https://man7.org/linux/man-pages/man2/pidfd_open.2.html):
  CLOEXEC pidfd readiness precedes actual wait. Sole wait ownership and ordinary
  SIGCHLD handling are prerequisites, not a PID-reuse proof from a numeric ID alone.
- [F_GETFD](https://man7.org/linux/man-pages/man2/F_GETFD.2const.html),
  [fcntl](https://man7.org/linux/man-pages/man2/fcntl.2.html), and
  [close](https://man7.org/linux/man-pages/man2/close.2.html): self-slot flag query
  and EBADF closure witness; no retry of a failed close that could hit a reused FD.

The chosen existing std Child+pidfd/private-byte mechanism is already the bounded
owned-child mechanism ROOT approved for crash875. A adds no crash behavior.
Retaining a duplicate parent capture writer for a later wait-status report would
invalidate this last-writer fixture. Passing a management socket/FD or introducing
a second report manager is unnecessary; missing exported status stays explicit
and ROOT owes its independent actual witness. Source contracts are not live data.

## Exact light results

Fedora `8fadc1ec213332e16cb0d915a3fb913f`, r32, NEW private
`~/managed-tenant-inherited-light-01a1c5d2-v3/{source,target}`, admitted through
Kiln `linux/amd64,cpus=1,mem=1g,disk=2g`, Cargo jobs1/offline. Headroom admission
is not hard aggregate accounting. Rust/cargo1.99.1, Fedora44/kernel6.19.10.
Source436KiB/target84,592KiB; no local target/cache or Cargo invocation.

Final GNU/musl `check --tests` and `clippy --tests -- -D warnings` all0, with
panic-related production lints explicitly denied for the exact extracted native
helper module. Formatter and policy checker0. Exact leaf/native/ABI and actual
task/Agentfile/publication/run blocks were compiled with inert surrounding host
integration stubs. No helper, pure fixture function, test body, socket or VM was
executed. The actual complete dispatcher, full host/helper build and every effect
remain unverified. [Raw v3](inherited-raw/v3.json) retains logs, compiler/kernel,
lockfile, checked hashes and exact compiled parent blocks; [command](inherited-kiln-v3.command)
retains transfer SHA. [Prepare script](prepare-inherited-light.py) reconstructs
the narrow source package. Final source hashes: [manifest](inherited-source.sha256).
Extracted helper SHA256
`c3640e8e240ebd919b2358f110f2de397bdbab29c7a662b7e125417aa9d2105f`;
new leaf SHA256 `9cf2a4c73643d23021d2e6d6eb1ff3f404645332ac803ca59313a5271771fbf8`.

[v1/r28](inherited-raw/v1.json) is failed compile101 because the compile-only
wrapper placed an inner deny attribute after a use. It executed no fixture code.
The corrected [v2/r30](inherited-raw/v2.json) passes all four checks; v3 additionally
checks own closed stdio slots and is the final checked source. Earlier evidence
does not substitute for the final source. B body and strict-terminal identity were
checked after adopting only the recorded formatter output.

## ROOT gates and repeatable invocation

ROOT keeps parent tenant_exit module-header wiring and rebuilds source-matching
host plus all carried ABI/helpers. Use B's explicit SHARDS_TENANT_STDIO variables
from [HOST-CHECKPOINT.md](HOST-CHECKPOINT.md), plus the caller's positive TASKS,
and a new absolute records directory. Replace B's exact test selector with:

```sh
SHARDS_TENANT_STDIO_TASKS="$caller_positive_agent_tasks" \
cargo test -p shards --test managed_output --profile test-release \
  tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure \
  -- --ignored --exact --nocapture --test-threads=1
```

Set compile-time SOURCE_REVISION during the actual source build; retain exact
kernel/init/testguest/host hashes and commands. No default timing/task/byte budget
is selected here. The directory, tasks, port, representable fixed deadlines and
combined report envelope must validate before starting a managed fixture.

ROOT owes actual source-matching compatibility and full B regression, controlled
parent close+private handoff ordering, child inherited writer bytes and last-writer
End/EOF, causally later sibling/main bytes, actual parent/child statuses/reaping,
owned cleanup/resource return and strict metadata. Decisive negative candidates
include removing ONLY parent close while retaining self-EBADF check; wrong/missing
handoff; altered child suffix; early child exit; insufficient task/report inputs;
and acquisition/release/cleanup refusal. Restore exact source after each actual
guard run. Never widen seccomp/grants to convert a refusal into a pass. ROOT alone
runs these helpers, actual sockets/VMs, mutations, audits, profiles and full matrix.

## R01–R16 accounting

| Rule | Scoped result and remaining gap |
| --- | --- |
| R01 | No product edit. Checked framing/deadlines/conversions; native helper panic-denial clippy0. Tests may assert; no universal implicit-panic proof. |
| R02 | Primary contracts above and exact grammar/callers; existing std Child/pidfd versus unsafe fork/extra manager. All actual compatibility/effects ROOT-owed. |
| R03 | Explicit caller ports/tasks/work/cleanup/challenge/report inputs. Frame tag1/u32 length, three literal payload pairs and fixture exit-stage enum derive the bound; no runtime constant chosen. |
| R04 | Explicit parent states, child/error stages, real wait, finite cleanup and retained Unknown until outer stop. Remote exact cause/status uncertainty stays explicit. |
| R05 | Existing Child/pidfd/readiness/owner endpoint/publication seams. No caches, polling of state, extra manager or owner transfer. B frozen; A's focused control exchange stays local. |
| R06 | New own leaf/helper functions and tiny approved task+AGENT line; exact scoped compile/clippy0. ROOT combined audit owed. |
| R07 | Unsafe/self-FD, wait ownership, closed-stdio and Unknown retention obligations documented. |
| R08 | No baseline/ratio/4–5× claim; ROOT performance comparison owed. |
| R09 | New fixture, no speculative product correction. Previous actual normal failures retain their own ROOT records. A actual result unrun. |
| R10 | Exact source, failed wrapper, final logs/recipe and owed mutations retained. Actual before/after/effects/cleanup not claimed. |
| R11 | GNU/musl amd64 compile only; other targets/real KVM/HVF/WHP unverified. macOS ARM64 only; paused Windows ARM untouched. |
| R12 | Existing shared native Linux fixture and host seams, no new OS adapter, installed helper or privilege change. |
| R13 | One Child/control exchange, explicit finite envelopes, retained authority on Unknown; no second process manager. |
| R14 | No profile/benchmark/allocation/leak claim. Parent/kernel/pipe/socket/copy costs and distributions ROOT-owed. |
| R15 | No child sessions/automatic dispatch/main merge/audits/actual helpers/VM/socket runs. Signed own checkpoint only; ROOT actual review/merge. |
| R16 | Reused own worktree, small snapshots; NEW private remote sources/targets only. No local Cargo/build, protected target/cache/paused VM mutation or deletion. |

Both stdio cases remain source candidates until ROOT's actual gates; reconnect's
separate initial-cursor seam and the overall managed-runtime plan remain open.
