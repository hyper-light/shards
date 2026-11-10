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

Native Linux r194 at clean `88c8b5fd2f33d92c4d4d05a64b2c36ad3a22b3ba`
completed under one admitted 4 CPU / 8 GiB / 20 GiB envelope (30 of 32 cores idle
at dispatch). Runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora Linux
`6.19.10-300.fc44.x86_64`, pinned Rust 1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8. Private synced source
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-borrowed-c5f6175fcc`;
new private target `~/managed-borrowed-held-target`. Earlier targets/binaries
were untouched. Exact invocation was
`bash docs/research/measurements/managed-borrowed/run-held-output-light.sh`.

- Baseline causal test: 1 passed, writer held/alive/unreaped at refusal.
- Exactly one marker-recognition removal: 1 failed, exit101. It observed
  `refusal_before_writer_release=false writer_held_alive=true writer_unreaped=true`
  and the final causal assertion received `Timeout`. The test explicitly released
  the writer and joined workers before that assertion. No production mutation
  was retained.
- Byte-exact source restoration and actual recompile: causal test 1 passed.
- Ordinary GNU API library: 49 passed / 0 failed / 9 ignored (including two new
  private child roles); simulator integration: 14 passed / 0 failed / 2 ignored.
  Real-VM test remains ignored/unrun. No prior fault campaign was rerun.
- Strict GNU API/afunix all-target clippy: exit0. GNU afunix's Windows-only code
  is not native Windows coverage.

Baseline/restored API binary:
`~/managed-borrowed-held-target/x86_64-unknown-linux-gnu/test-release/deps/shards_api-780ac76e90e497e3`,
SHA256 `88940a52601f332b66d16ee0b27cc8468067b7c2a71a2b0103df8a5a566a4b55`.
Mutation binary SHA256
`39bec7e41d4b2e5d2ae7550857a0d7808e701851e598b10b4686eb9256b045d8`.
All three selected compiler artifacts reported `fresh:false`; immutable copies
are retained privately under `~/managed-borrowed-evidence/held-output-v1/*.bin`.
Raw logs/commands/source manifest/patch/status/build JSON are in
[held-output-v1](held-output-v1/); archive SHA256
`f4d8c4b296a3d9cbbaec0056d6c8452275a2a14e848a6a765c9671fb986865a3`.
The first local extraction attempt used Python's unavailable `filter` argument
and failed before extracting; the subsequent extraction validated every regular
file/directory path and archive SHA. This was evidence-transfer setup, not a test.

The subsequent exact source `902f29a77c1dc8c919882a7b922bae6382bc1d0c`
moves explicit actual child-status checks and the cleanup print before the
causal assertion, so a mutation failure also logs release/reap/join outcomes.
Root approved one focused repeat without a whole corpus rerun. Native Linux
r198, same runner/toolchain/envelope (29 of 32 cores idle), completed exit0:
baseline1/0, one marker-removal1red/101, byte-exact restored1/0, strict
API/afunix clippy0. All three builds again reported `fresh:false`. The actual
red log says:

```text
refusal_before_writer_release=false writer_held_alive=true writer_unreaped=true
writer_explicitly_released=true writer_exit=Some(0) vm_reaped_exit=Some(7) all_workers_joined=true
```

Only then did the causal assertion report `Timeout`. The baseline and restored
logs report refusal before release, with the same actual exit statuses and
completed joins. The two children and four workers completed in the failing
mutation too. This proves this fixture detects the relevant blocked-refusal
ordering; it is not a general resource/leak/performance claim.

Command:
`HELD_OUTPUT_FOCUSED_ONLY=1 bash docs/research/measurements/managed-borrowed/run-held-output-light.sh`.
Tracked source was exact902; status lists only previously retrieved untracked
`held-output-v1` evidence. Raw [held-output-v2](held-output-v2/) archive SHA256
`2735929b8f73d3a1aab112094a55c42fbdaa7ff57da726f58774b9bd3e0b3731`.
The same private binary path now has baseline/restored SHA256
`610acf10c3a1e38aa75c0d89b82126b041130ab4a0c20294acc3e53e587968af`;
marker-removal SHA256
`c69f5e52ec484c325c184cb21d864abda350dd2cac77513488616002b2a46e14`.
All three immutable private copies remain under
`~/managed-borrowed-evidence/held-output-v2/*.bin`. No production or causal
condition changed; the full ordinary corpus was not rerun for evidence print
ordering. The prior GNU timing red remains independently retained.

Owed coordinator runs: focused native macOS arm64/musl coverage of this new
fixture, any combined API/VM/matrix gates after integration, and review of the
ordinary-corpus source88 versus evidence-order-only source902 distinction.
Windows does not compile this Unix-only replacement (matching the old test's
scope); its previous native borrowed results retain their original identities.
No new profiles, heavy campaigns, native Windows repetitions or VM runs occurred.
The non-UTF8 process-name suspicion is still unreproduced and deferred.

R01–R16 account: this is test-only (R01/R04/R06/R07/R13), with primary EOF and
endpoint evidence and no new runtime constants/architecture (R02/R03/R05/R12),
baseline red retained (R09/R10), no performance claim (R08/R14), Unix-only
placement matching the old test (R11), scoped light implementation/no children
or main merge (R15), new private target and preserved prior artifacts (R16).
Root still owns combined/matrix/heavy checks. The full Mac library OOM abort
remains red; no warming, skip or production remedy is part of this change.
