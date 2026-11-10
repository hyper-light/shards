# Causal held-output regression

Implementer: resource_performance; coordinator: root. Policy read after resume:
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, full canonical
`g121-main/AGENTS.md`, R01–R16 and evidence checklist; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
CLAUDE, managed plan and current handoff also read. Scope is the one client test,
its old integration-test replacement, and this evidence. Production behavior is
unchanged from `53535ab61225905acfa98f12dabdabb75138c808`.

## Reproduced problem and boundary

The unchanged-source [Linux record](LINUX.md) at `eaedd0b0` retains the ordinary
GNU integration result 14 passed / 1 failed / 2 ignored. Its failing
`client::a_child_holding_shards_output_holds_no_refusal_back` already received the
correct `Refusal::Boot` containing `no kernel at kernel`; only its subsequent
`REFUSED_AFTER_END` comparison failed (`1.348517ms`). This is a test assertion
failure, not proof the inherited writer blocked the refusal and not a claim of
scheduler noise. The exact source, compiler artifacts and raw failure remain at
`f77ddc52`; no rerun or threshold change rewrites that result.

The contract is causal: refusal arrives while another live process still holds
the captured output pipe's write endpoint. A smaller recorded latency maximum
does not establish that ordering. This replacement therefore removes the sole
`REFUSED_AFTER_END` learned record and moves this one ordinary, unignored
regression to a client-unit module. It calls the existing private `drain`,
`Exit::of`, `Exit::watch`, and `ShardsGuest::connect`. The two watcher spawn bodies
are wrapped in owned test JoinHandles because production `start_threads`
deliberately detaches its handles. There is no production hook or API change.

Two directly owned Child objects run private ignored roles of the same test
executable. The fake VM waits for explicit stdin release before diagnosing and
exiting 7. The writer reports readiness and the identity of its stdout endpoint
on its separate stderr, then holds stdout until its own explicit stdin release.
The parent samples the original write endpoint before duplicating it. It checks
the writer through existing `taken::Held(pid,start)` before and after observing
refusal, while retaining the child and the release pipe. Only after that
observation does it release and reap the writer and join every test worker.
Cleanup closes controls, kills through the two owned Child objects, reaps and
joins even on assertion failure. No naked-PID signal, orphan sleep, periodic
probe, new learned cache, production timeout or latency assertion is introduced.
One existing `conformance::PATIENCE` absolute outer deadline bounds event waits.

## Primary evidence and alternatives

[Linux pipe(7), man-pages 6.18](https://man7.org/linux/man-pages/man7/pipe.7.html)
specifies EOF only after all write-end descriptors are closed. Explicit writer
ownership therefore directly establishes the missing condition; sleeping for an
estimated duration or comparing two timestamps does not.

[Apple XNU xnu-12377.101.15 sys_pipe.c](https://raw.githubusercontent.com/apple-oss-distributions/xnu/xnu-12377.101.15/bsd/kern/sys_pipe.c)
creates distinct read/write endpoint structures and `pipe_stat` hashes the
specific endpoint into `st_ino`. Comparing the child's writer against the
reader's inode would be an incorrect cross-platform premise. This fixture
instead compares the child stdout's fstat device/inode against the still-held
original **writer**, whose known duplicate was assigned to that child. Reader
association follows from the same `io::pipe` constructor. These samples are not
treated as persistent identities after close or reap.

Using only a public boot stand-in would lose the original writer descriptor and
the detached watcher joins. Extracting a new production control API solely for
this test would broaden the seam. The approved private unit placement reuses
the actual watchers and preserves ordinary regression coverage without either.

## Validation and remaining work

Initial fixture source: targeted live result pending. The approved light scope
is one native focused causal test, one temporary marker-recognition guard
removal which must fail because refusal waits for writer release, byte-exact
restoration/recompile, and focused ordinary regression/clippy. Raw commands,
source/binary hashes and actual outcomes will be appended at the evidence
checkpoint. The prior GNU timing red is retained independently.

R01–R16 account: this is test-only (R01/R04/R06/R07/R13), with primary EOF and
endpoint evidence and no new runtime constants/architecture (R02/R03/R05/R12),
baseline red retained (R09/R10), no performance claim (R08/R14), Unix-only
placement matching the old test (R11), scoped light implementation/no children
or main merge (R15), new private target and preserved prior artifacts (R16).
Root still owns combined/matrix/heavy checks. The full Mac library OOM abort
remains red; no warming, skip or production remedy is part of this change.
