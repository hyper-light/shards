# The nesting grant (D234): evidence record

This record follows [docs/development-evidence.md](../../../development-evidence.md) for step 38 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d.

The decision is [D234](../../../design/architecture.md). Its research, with every finding graded, is [nesting-grant-decision-v1](../../nesting-grant-decision-v1.md). The policy receipt is [POLICY_READ.json](POLICY_READ.json).

Each item names its command, revision, log and outcome as it is done. **Owed** marks what is not done yet, and nothing owed is passed.

## Identity

- Task: plan step 38. It is planned as kiln plan 01a1cda5426f73cc810ea93e3bd008cd and researched as kiln research 01a1cda5426f73cc86b87e7749163666.
- Branch: `p/s38-nested-grants-v1`, from origin/p/integrate-engine-20261111 032b63b1c, merged with origin/main c4ae59ee at c63b3e944.
- Policy: origin/main bff30151. The rules' sha256 is df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
- Role: implementer for Shards ROOT (01a148884a4770fdaf0111f3f792a248).
- Owned paths: as in POLICY_READ.json.
- Targets: the seven of resources/supported-targets.txt. Guest code is Linux only, on amd64 and arm64.
- Step 7's close (2026-11-13) was done by ROOT's goal agent for kiln goal 428667b64437861496673271bfc92009, as implementer. It is planned as kiln plan 01a1d2b7b91672289d81f74e93d7494e and researched as 01a1d2b7b91775ca9c61ae09eda39b53. Its branch, `p/s38-docs-v1`, comes from origin/main 1145bd3f2. The policy at that revision is unchanged from bff30151: AGENTS.md f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41, the rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, the checklist 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63.

## Decision (R02, R03, R05, R12)

D234 and its record:
- the alternatives and why each was not chosen;
- every source at a pinned revision;
- every literal's derivation.

No measurement backs the decision itself. The two inferences that only a live run shows are owed to the real-VM test:
- the boxes' pivot_root under shards-init's root (f28);
- the workload's filter bounding nested device programs (f20).

## Wait and cache review (R05)

The grant adds one wait: the readiness state machine's wait on the caller's deadline. It waits on the deadline the caller set and on the workload's own READY=1 notification, and it adds no probe and no cache.

Reviewed in the code as built (steps 3, 4 and 6):
- init waits on the deadline with a timerfd on CLOCK_BOOTTIME, and on the notify socket, in its own poll loop (crates/init/src/readiness.rs): no sleep, no probe;
- the daemon and the run host wait on the VM's own messages (ipc kind 90, the image wire's READY), never on a clock of their own;
- shards-api's `wait_ready` blocks on each answer's arrival and returns as the readiness moves, with no polling (step 6's session tests).

No cache is added.

## Live example (R09, R10)

The real-VM test `crates/shards/tests/nesting.rs` (plan step 5) runs runc as a stand-in nested runtime under the grant, and without it, through `shards run vm` and through shards-api.

**Red on CI's KVM x86_64.** CI's x86_64 runners have /dev/kvm. The two main runs read that reached the test ran it red, the same way in each of three jobs: 3 passed, 6 failed, 1 ignored. They were 83f0549ad (run 43303186680, job 131479475824, gnu) and d35c4cf79 (run 43303460342, jobs 131480418603, gnu, and 131480418606, musl). The logs are [step7/](step7/), and [step7/ci-runs.txt](step7/ci-runs.txt) has the six other main runs read, which timed out before reaching it.

The symptoms, and their cause in the test itself, not in the grant (D234, "The real-VM test's first runs"):
- runc said "rootless container requires user namespaces"; a mount was refused EPERM; the subtree root's own files were refused EACCES. The test's images ran as the served image's user `app`, which holds no capability across its execve (capabilities(7)). runc v1.5.2 is rootless at any non-zero euid.
- The shards-api case ended at once (`Ended(Exited(128))`): its command lacked its program, and shards-api's command is the whole argv.

The same runs showed three things green on KVM x86_64: the leaf (`workload cgroup /init`), READY reaching the host, and AF_VSOCK still refused. macOS CI runs none of it, every case returning at "Hypervisor.framework is unavailable".

**Fixed, green unverified.** 231bbdf6d gives every image the test builds `USER root`, and names the shards-api case's program. Its compile and clippy pass on x86_64 Linux (the table's step 7 close), and CI on main builds the other targets. No VM has run it since. ROOT runs it, or reads CI's next main run that reaches it, on:
- KVM x86_64;
- nested arm64 KVM;
- HVF with its GPU;
- WHP.

Each guard's mutation is listed with the test.

## State and code (R01, R04, R06, R07, R13)

As built (each step's row below has the commands and logs):
- The readiness transitions (D234's table) are typed and tested:
  - init's readiness states and their datagrams (step 4);
  - abi's `Ready` encode and decode;
  - the daemon's events from them (step 3);
  - shards-api's `wait_ready` (step 6).
- The refusals are typed through to their words:
  - the agentfile crate's `NestingRefused` and `nesting_beside` (step 2);
  - abi's shared words, which daemon and init both use (steps 3 and 4);
  - shards-api's `ImageRefusal` mapping (step 6).
- The panic review is the workspace's own deny lints (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing` and the rest). Clippy with `-D warnings` exits 0 on every touched crate on Linux x86_64, and on x86_64 and aarch64 musl for init, abi and the test guest.

## Adversarial results (R15)

Owed to ROOT's campaign. nesting.rs covers the first, second, third, fifth and sixth items below (cases (c), the exec test, (b), the deadline through shards-api, and (f)). init's unit tests and mutants cover the datagrams (step 4, M3 and M3b), which nesting.rs's `ready` probe does not send. Only a real run is evidence, and CI's red runs (above) show none of them: run as `app`, each case failed at or before its check:
- a crafted image declaring NESTING beside an agent;
- a joiner into a granted microVM;
- a box writing outside the subtree;
- datagrams to the notify socket from outside the workload, and oversize ones;
- a missed deadline;
- the workload's death taking its boxes.

## Performance (R08, R14)

Owed to ROOT: a granted run's start against an ungranted one (docs/research/measurements/build-ab), with n, p50, p90, p99 and max, host, OS and revision. That includes the parse of the grant's filter at each start, which step 3 left unmeasured.

## Portability (R11, R12)

- The guest side (crates/init) is Linux, on amd64 and arm64, and runs on KVM, HVF and WHP.
- The daemon's checks are shared code for every host.
- Built and checked on Linux x86_64 for every touched crate. init, abi and the test guest are also checked on aarch64 musl. nesting.rs compiles for x86_64 (gnu, adacachy2) and aarch64 (musl, cross-built on this Mac).
- The test's fix (231bbdf6d) compiles and lints clean for x86_64-unknown-linux-gnu, natively on adacachy2. That is the one host the person's rule of 2026-11-13 gives a change's tests; CI on main builds the matrix. It changes two strings of the test and no code of any host or guest.
- Before that rule reached this lane, two aarch64-unknown-linux-musl attempts on adalundhe-macbook-pro failed on that runner's tooling, not on the change, and were not rerun. Their logs are kept in step7/:
  - the first found no cargo on the runner's PATH;
  - the second's dev-profile link of the `shards` binary failed (rust-lld, exit 1, its message not kept), and its 1.98.1 toolchain had no clippy.
- Owed to ROOT: `scripts/lint` on the seven targets, the real-VM runs listed above, and the daemon's filter tests on arm64. Their arm64 syscall numbers are checked against the kernel's headers, but they have not run there.
- Shared, not copied (R12): the refusal words, which daemon and init take from abi and the build takes from the agentfile crate; and api-run's decision, which is the daemon's own `nesting()`.

## Operations (R15, R16)

- Commits are signed with gits, of named paths. Each step lands on main by fast-forward push once its tests pass, under the person's rule of 2026-11-12 (subagents merge their own work to main), which supersedes AGENTS.md's no-self-merge for this work.
- No build or test ran on this Mac, which is below its 30 GiB floor.
- Light tests run on kiln Linux runners within POLICY_READ.json's envelope, or over ssh to adacachy where kiln's runners refused runs.
- On 2026-11-12, kiln's headroom guard on adacachy froze builds (`cgroup.freeze=1`) under that host's load: step 6's lane and this record's own step 5 gate. Each was stopped and moved to adacachy2 through `kiln run`, and the guard was never bypassed (step6/logs/guard-adacachy.txt).
- Every copy and target directory these runs left on adacachy and adacachy2 was removed after its run (R16). One cleanup's `pkill -f` matched its own shell's command line, and was redone in separate commands.
- Step 7's close ran no VM, pushed no branch and started no CI: CI on main is ROOT's (the person, 2026-11-13). Its gates went through `kiln run` to adacachy2 and adalundhe-macbook-pro, never adacachy. Its harness removes its own directory as it ends.

| Step | Commit | Checks | Outcome |
|---|---|---|---|
| 1. D234, the record, this skeleton, the literal audit | 534a1401a to c026df6a2 (origin/main merged at c63b3e944, d59e0705b and fdfd28a87; D234 landed on main at d59e0705b) | **R03, measured:** `python3 docs/research/measurements/s38-nested-grants/literal-audit.py` at c026df6a2. It covers the 22 literals D234's grant adds, every backticked token of its Decided and Wire text, and the identifiers marker 7, `run::kind` 49 and ipc kind 90 against their tables. Its own mutation check, on copies of the files: an unmutated control passes; deleting PIPE_BUF's bullet fails (UNDERIVED PIPE_BUF); planting `quota=<n>` in Wire fails (UNPLANNED); taking marker 7 fails (TAKEN, next free 8). **Policy:** `python3 scripts/check-development-policy`, with the audit, at c026df6a2: (a) through kiln run on runner cec37f60 (macOS arm64), fetched from origin; (b) on adacachy (Linux x86_64, Python 3.14.8) over ssh from `git archive c026df6a2`. kiln's Linux runners refused runs from this machine after kiln's restart: 451397fc and 00aad8da "did not answer in 15s", 8fadc1ec "didn't answer hand … in 8914 ms", and "has no room here now: it isn't started". Earlier, at e57f32a4e, the policy check alone passed on adacachy over ssh | audit: planned 22, underived or unplanned 0; markers 7, 49 and 90 free; exit 0, on both. Policy: "Development policy checks passed; evidence/coordinator review is still required.", exit 0, on both. **Through kiln run on Linux** (the step's own test): `kiln run --on 451397fc… --no-sync --stdin` (the files by `git archive c026df6a2` on stdin; harness [kiln-linux-check.sh](kiln-linux-check.sh), raw [kiln-linux-check.log](kiln-linux-check.log)) ran on adacachy (Linux x86_64) at 2026-11-12T07:59Z, after adacachy2's runner restarted mid-run: policy_exit=0; audit planned 22, underived or unplanned 0, audit_exit=0. kiln's CLI then exited 1 with "refused: 451397fc… sent no receipt of it", a receipt kiln wants after the output, not the command's status. **Again, with a clean exit:** the harness, now requiring kiln's own exit status 0, ran on kiln-a094a12d (8fadc1ec…, Linux x86_64) at c026df6a2 ([kiln-linux-check-clean.log](kiln-linux-check-clean.log)): rc 0, policy_exit=0, audit planned 22, underived or unplanned 0, audit_exit=0 |
| 4 (coordinator's follow-up). init refuses the grant with `--pid host`, as D234 says; D234's literals gain step 4's (the socket's mode 0666, READY's 9-byte payload, the `notify` entry and tag `N`, NR_OPEN_MAX 2147483584) and the on-time boundary | 6f752173d | on adacachy (Linux 7.2.9-2-cachyos, rustc 1.98.1) under systemd-run MemoryMax=16G CPUQuota=400%, -j4, from `git archive 6f752173d`: `cargo test -p shards-init -p shards-abi --target x86_64-unknown-linux-musl`; clippy for that target with `-D warnings`; `cargo fmt --check`. Then the mutant that drops the `pid=host` arm of `undoes_nesting`, against `run::nesting_tests::the_grant_is_refused_with_the_microvms_cgroup_or_pid_namespace`. The literal audit: 26 planned | abi 49 passed and 2 passed; init 196 passed, 0 failed, 1 ignored (frames_cost, a measurement); clippy exit 0; fmt exit 0; the mutant fails its test (exit 101); the copy removed. Audit: 26 planned, 0 underived or unplanned; marker 7 and run kind 49 held by the grant's own WORKLOAD_READY and READY, ipc kind 90 free |
| 4 (step 4's lane). init applies D234 (b3001d90a to 16c72cc32), then a real-kernel light test of D234's cgroup claims (6b2c72a3a, main b51253c18) | 16c72cc32, b51253c18 | on adacachy (Linux 7.2.9-2-cachyos, cgroup2 nsdelegate): `cargo test -p shards-init -p shards-abi --target x86_64-unknown-linux-musl -j4`; clippy `-D warnings`, all targets, x86_64 and aarch64 musl; fmt `--check`; the mutants M1 to M6, each against its named test ([step4/mutants-586c018fb](step4/mutants-586c018fb)); and `run::nesting_tests::on_a_real_kernel_the_granted_command_sits_in_its_leaf_and_its_root_is_inits` through init's own isolate and setup code in a delegated systemd scope ([step4/real-kernel.sh](step4/real-kernel.sh); no VM, no daemon). Logs: [step4/](step4/) | at b51253c18: abi 49 and 2 passed; init 197 passed, 1 ignored, 0 failed; clippy exit 0 on both targets; fmt exit 0. Every mutant fails its named test. On the real kernel: the hierarchy is writable; the standby sits in the leaf with the namespace root empty; `+memory +pids` enabled beside the leaf; a box's limits set; the root's own memory.max and pids.max refused EPERM (nsdelegate) and unchanged (268435456, 64); an exec refused the root (EBUSY) and put in the leaf. Its two mutants (standby left in the root; exec sent to the root) fail, and restored, it passes. Then 40edb79da, init's re-check reading NESTING as step 2's `spec()` writes it (merged as 5cef4a441), gated the same way: init 197 passed, 0 failed, 1 ignored, with `domains::tests::init_refuses_the_grant_unless_the_image_as_built_declares_it_alone` ok; clippy exit 0 on both targets; fmt exit 0 ([step4/test-40edb79da.log](step4/test-40edb79da.log), copied from the lane's transcript, as it kept no log file). These runs went to adacachy over ssh, not through kiln run, because kiln's Linux runners refused runs that night |
| 1 (R03, independent review). A read-only reviewer was given D234, the policy and the sources, not the author's audit. It built its own literal list and checked each derivation ([r03-review.md](r03-review.md)) | 20f37abee reviewed; fixes in this commit | The reviewer's verdicts, against its own list and the sources: 25 hold, 7 weak, 2 missing; 7 literals omitted from the list; 5 derivations wrong or unsupported. Every point is answered in D234 (r03-review.md's Response). literal-audit.py after: 31 planned, 0 underived or unplanned, exit 0 |
| 2 (step 2's lane). The Agentfile declares NESTING, and the build checks it (e871b1cf8; landed as 00fefebe9) | d59e0705b (baseline), 00fefebe9 | on adacachy (rustc 1.98.1, -j4), from a clean checkout of the landed commit: `cargo test -p shards-agentfile -j4 --no-fail-fast`; `cargo check -p shards --tests` (SHARDS_HELPERS=skip); `cargo clippy -p shards-agentfile --all-targets -D warnings`; `cargo fmt --all --check`; literal-audit.py. Then five text-removal mutants in agentfile.rs ([step2/mutate.py](step2/mutate.py), [step2/mutants.out](step2/mutants.out)). Logs: [step2/](step2/) | baseline: lib 43, agentfile 27, commit 1, gateway 3, oracle 12. At 00fefebe9: lib 43, agentfile 32 (5 new), commit 1, gateway 3, oracle 12/12 unchanged; exit 0. check, clippy and fmt exit 0; audit 31 planned, 0 underived. Each mutant (check()'s beside-domain and duplicate refusals, nesting()'s two, the argument refusal) fails its named test; restored, all green. Owed: a real `shards build` of a NESTING Agentfile and its run (step 5, ROOT); seven-target lint; `cargo test -p shards` beyond the compile check |
| 3 (step 3's lane). The daemon gives the grant only as both sides say, and composes it (3ab1eab5d, 9c886608d, 5ba5f377a; evidence 8f3cd5b74) | 5ba5f377a, landed as 8f3cd5b74 | on adacachy (Linux x86_64, rustc 1.98.1) over ssh, from `git archive 5ba5f377a`, -j4 in a 16 GiB, 4-CPU scope: `cargo test -p shards-ipc`, `cargo test -p shards --bin shards` (the package has no lib target), `cargo test -p shards-vm-process --bin shards-vm`, each again with `--profile test-release`; `cargo clippy -p shards -p shards-ipc -p shards-vm-process --all-targets -- -D warnings`; `cargo fmt --all --check`; literal-audit.py; scripts/check-development-policy; 17 text-removal mutants ([step3/s38-mutants.tsv](step3/s38-mutants.tsv)); the ungranted run's setup entries against bbd908197's ([step3/golden-bbd908197-x86_64.txt](step3/golden-bbd908197-x86_64.txt)). Logs: [step3/](step3/) | ipc 54 passed, 5 ignored; shards 697 passed, 13 ignored in debug (build::policy's tests skipped there, as their thread overflows its stack in the debug profile, before this change) and 720 passed, 0 failed, 14 ignored with test-release; vm-process 44; clippy, fmt, audit and policy exit 0. Each of the 17 mutants fails its named test (exit 101), and the named tests pass again restored: consent, declared, beside, joiner, joined, tracked, cgroupns-host, pid-host, ready-by-ungranted, pivot-root, mount-filter, caps-replaced, grant-caps, grant-filter, ungranted-golden, events, relay. The ungranted golden is byte-identical. Owed: `gpus=` (no run carries a D232 selection); its filter tests on arm64 |
| 6 (step 6's lane). The grant through D231's image boot: shards-api's `nesting` and `ready_by`, `shards api-run`, `wait_ready` (93ce2e26a, 0d63d6bf2; evidence db2ab36e9, merged as 8a857b267, 7ad28fab1) | 0d63d6bf2 and 8a857b267, landed as 7ad28fab1 | on adacachy2 (Linux 7.3.3 x86_64, rustc 1.98.1) through `kiln run --no-sync --stdin`, -j4 in a 16 GiB, 4-CPU scope, at both revisions: `cargo test -p shards-api`; `cargo test -p shards --bin shards --profile test-release api_run`; `cargo clippy -p shards-api -p shards --all-targets -- -D warnings`; `cargo fmt --all --check`; literal-audit.py; 8 mutants ([step6/mutants.tsv](step6/mutants.tsv)). Logs: [step6/](step6/) | shards-api lib 93 passed, 9 ignored, and tests/sim.rs 16 passed, 2 ignored; api_run 8 passed; clippy, fmt and audit exit 0. Each of the 8 mutants fails its named test: consent passthrough, ready_by passthrough, the budget's conversion, its overflow, Widening typing, READY's relay, the client's READY, a wait without the grant. Refusals: undeclared as `Widening`, a crafted image as `Image`, an overflow as `Spec`. Deviation: no image simulator exists (crates/api/src/sim.rs serves the raw `Shards` trait), so unit tests hold the consent matrix. Owed: `gpu_devices` on the image spec; the real-VM run through api-run |
| 5 (coordinator). The real-VM test: crates/shards/tests/nesting.rs and the test guest's nesting.rs (4ad27afb0, merged with main as 398dfcc68), and case (d)'s deadline through shards-api (2800336cb) | d9882b146 (a tree), 398dfcc68, 2800336cb | x86_64, from `git archive` with SHARDS_HELPERS=skip ([step5/remote-x86_64.sh](step5/remote-x86_64.sh)): `cargo test -p shards --test nesting --no-run`; `cargo clippy -p shards --test nesting -- -D warnings`; `cargo clippy -p shards-testguest --target x86_64-unknown-linux-musl --all-targets -- -D warnings`; `cargo build -p shards-testguest --profile guest --target x86_64-unknown-linux-musl`. These ran on adacachy (Linux 7.2.9) over ssh at tree d9882b146, and on adacachy2 (Linux 7.3.3) through `kiln run` at 398dfcc68 and 2800336cb. adacachy's headroom guard froze the run at tree 6759ab41 for two hours; it was stopped and moved, never bypassed. aarch64, on this Mac, cross-built through zig and rust-lld as common::linux_tests builds: clippy of the test guest and of the test for aarch64-unknown-linux-musl, and `cargo test -p shards --profile test-release --no-run --target aarch64-unknown-linux-musl --test nesting`, at d9882b146's files, 398dfcc68 and 2800336cb. runc 1.5.2's pin: its two binaries' SHA-256s against the release's runc.sha256sum, their signatures against runc.keyring at the tag, and `file` ([step5/runc-1.5.2/verify.log](step5/runc-1.5.2/verify.log)). Logs: [step5/](step5/) | every check exits 0, on both architectures, at each revision. The two binaries match their pinned hashes, and each carries a good signature by C2428CD75720FACDCF76B6EA17DE5ECB75A1100E, a key of runc.keyring. Both are static-pie. A finding: runc fails at any keyring error but ENOSYS (libcontainer/standard_init_linux.go at v1.5.2), and the grant leaves keyctl refused, so the stand-in runs with `--no-new-keyring`; a nested runtime of Hex's meets the same refusal. Not run: the test itself on any hypervisor, and its guards' mutants, which its header lists (ROOT) |
| 7 (coordinator). D234 built and owed; AGENTFILE_ARCH.md §9.3; the README's words for asking for the grant; runtime-image-boot-decision-v1.md §3.4; step 6's literals in D234 and the audit; this record's R01 to R16 | 9ae42d55f | `python3 scripts/check-development-policy` and `python3 docs/research/measurements/s38-nested-grants/literal-audit.py` on adacachy2 (Linux 7.3.3 x86_64, Python 3.14.9), through `kiln run --no-sync --stdin` from `git archive 9ae42d55f` of their files ([step7/policy-and-audit.log](step7/policy-and-audit.log)). The audit's new check was mutated on copies first: the image wire READY's bullet removed, then READY moved off 23 | policy: "Development policy checks passed", exit 0; audit: 35 planned, 0 underived or unplanned, every identifier held by the grant's own constant, exit 0; kiln's exit 0. The mutants: UNDERIVED to_client::READY = 23, then TAKEN image wire kind 23, each exit 1 |
| 7 (close, ROOT's goal agent, 2026-11-13). CI's red runs of nesting.rs read and kept. The test's two defects fixed: its images run as root, and its shards-api case names its program. D234, AGENTFILE_ARCH.md §11's NESTING row and the README's words brought to main | 231bbdf6d (the fix), then this record's commit | CI's logs, read with `gh api`: [step7/ci-runs.txt](step7/ci-runs.txt) and each run's nesting.rs part. No CI was started, watched or rerun for this. The fix's light gate, [step7/gate.sh](step7/gate.sh) and [step7/gate-remote.sh](step7/gate-remote.sh), sends `git archive 231bbdf6d` through `kiln run --no-sync --stdin` and runs `cargo test -p shards --test nesting --no-run`, `cargo clippy -p shards --test nesting -- -D warnings` (SHARDS_HELPERS=skip) and `cargo fmt --all --check`. x86_64-unknown-linux-gnu ran on adacachy2 (Linux 7.3.3, rustc 1.98.1, zig 0.16.0; [log](step7/gate-x86_64-unknown-linux-gnu-231bbdf6d.log)). Two aarch64-unknown-linux-musl attempts, cross-built on adalundhe-macbook-pro (macOS arm64) through zig and rust-lld, came before the person's one-host rule of 2026-11-13 and are kept, not rerun ([attempt 1](step7/gate-aarch64-unknown-linux-musl-231bbdf6d-attempt1-no-cargo-on-path.log), [attempt 2](step7/gate-aarch64-unknown-linux-musl-231bbdf6d-attempt2-dev-profile-link-no-clippy.log)). x86_64 ran gate-remote.sh before its PATH line and failure count were added. Then `python3 scripts/check-development-policy` and literal-audit.py on this Mac, the policy's own light check, at this record's commit ([step7/policy-and-audit.out](step7/policy-and-audit.out)) | CI: red, 3 passed, 6 failed, 1 ignored, alike in three jobs at two revisions; macOS CI skips every case. x86_64: no-run exit 0, clippy exit 0, fmt exit 0. aarch64: attempt 1, exit 127 (no cargo); attempt 2, no-run exit 101 (the `shards` binary's link) and clippy exit 1 (no clippy component), fmt exit 0. Policy and audit: as policy-and-audit.out says. Not run: the test itself, on any hypervisor, and its guards' mutants (ROOT) |

## R01 to R16

Each rule, what shows it for this work, and what remains. Nothing unrun is passed here.

| Rule | Proof | Result | Gap |
|---|---|---|---|
| R01 Production never panics | The workspace's deny lints, through `cargo clippy -D warnings`, on each touched crate (rows 2 to 6), and on the test's fix on x86_64 Linux (step 7's close) | exit 0 at each step | `scripts/lint` on the seven targets (ROOT) |
| R02 Research and data behind decisions | D234 and [nesting-grant-decision-v1](../../nesting-grant-decision-v1.md): 39 findings, each graded from its sources | decided 2026-11-11, reviewed against its research | none |
| R03 No arbitrary constants | D234's Literals; [literal-audit.py](literal-audit.py) with its mutation checks; the independent review ([r03-review.md](r03-review.md)) | 35 planned, 0 underived or unplanned; marker 7, run kind 49, ipc kind 90 and image wire kind 23 held by the grant's constants alone; every review point answered | none |
| R04 Explicit state machines | D234's readiness table; abi's `Ready`; init's readiness tests and mutants M3 and M3b (row 4); the daemon's events (row 3); `wait_ready` (row 6); CI's KVM x86_64 runs (step 7's close), where READY reached the host as its event | every transition typed and tested; starting to ready shown in a real guest | every other transition in a real guest, the deadline's through shards-api included (ROOT) |
| R05 Deliberate waits and caches | The wait review above | one wait, on the caller's deadline and the workload's READY=1; no poll, no cache | none |
| R06 Control complexity | The grant extends existing seams: setup words, run kinds, ipc kinds, the image wire's kinds | no new mechanism beside them | none |
| R07 Comments explain what code cannot | Each new item's comment cites D234 and its finding | reviewed in each step's diff | none |
| R08 Compete on measured results | none measured | none | R14's build-ab (ROOT) |
| R09 Reproduce live | D234's cgroup claims on a real kernel through init's own code (row 4's real-kernel test); the real-VM test's red, reproduced live by CI's KVM x86_64 runs at 83f0549ad and d35c4cf79, alike in three jobs ([step7/](step7/)) | as D234 says, with its two mutants failing; the red's cause found in the test itself (runc v1.5.2's rootless rule, capabilities(7), api-run's whole argv) and fixed at 231bbdf6d | the fix's green, and the real-VM runs on the other hypervisors (ROOT) |
| R10 Durable evidence | Every run's command, revision, host and log, kept in step2/ to step7/ beside this record, CI's red runs' logs among them | kept | none |
| R11 Supported platforms | Linux x86_64 for every crate; aarch64 musl for init, abi, the test guest and nesting.rs; the test's fix on x86_64 gnu (its two aarch64 attempts failed on a runner's tooling, and CI on main builds aarch64); CI's KVM x86_64 runs of the test, red, and macOS CI's skip of it | compiled and tested as each row says; the real-VM test red on KVM x86_64 until its fix, then unrun | the seven targets' lint; the fix's green on KVM x86_64; nested arm64 KVM, HVF with its GPU, and WHP runs; arm64 filter tests (ROOT) |
| R12 Share implementations | One set of words for the build, the daemon and init; api-run decides through the daemon's own `nesting()`; nesting.rs is built on tests/common | shared | api-run's VM launch remains D231's own owed R12 gap |
| R13 Simple and concise | Each step's diff, kept to its seam | reviewed | none |
| R14 Profile allocations and faults | none measured | none | a granted run's start against an ungranted one, with the filter's parse per start (ROOT) |
| R15 Roles, adversarial validation, merges | Each lane's policy readback, signed `gits` commits of named paths, fast-forward landings under the person's rule of 2026-11-12; text-removal mutants: 5 (step 2), 10 and the real-kernel test (step 4), 17 (step 3), 8 (step 6). Step 7's close ran no VM, pushed no branch and started no CI (the person, 2026-11-13) | each mutant fails its named test | ROOT's real E2E, E2E mutation and adversarial campaign |
| R16 Recover runner space | Each copy and target on adacachy and adacachy2 removed after its run; step 7's close's on adacachy2 and adalundhe-macbook-pro removed by its harness as it ends | none left (checked after each; the harness prints each removal) | none |
