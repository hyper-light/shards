# runtime-image-boot-v1: evidence (implementer record)

D231, [decision record](../../runtime-image-boot-decision-v1.md). The implementer's light
checks, then the real-VM runs ROOT assigned after main's CI failed (2026-11-12, below);
nothing here is a performance claim (R14 is ROOT's).

## Identity

- Policy: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, docs/development-rules.md
  sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3
  ([receipt](POLICY_READ.json)); role implementer; coordinator ROOT
  01a148884a4770fdaf0111f3f792a248.
- Branch `p/runtime-image-boot-v1` from `origin/p/integrate-engine-20261111` 82083f27,
  with the integration branch's ring window fix 032b63b1c merged at ROOT's word (75c36864f).
- Checks ran on kiln runners `kiln-a094a12d.attlocal.net` (8fadc1ec…, Linux x86_64, under
  `systemd-run --user --scope -p MemoryMax=24G`, `-j4`, no amphora-bench running) and
  `adacachy` (451397fc…, Linux x86_64, `MemoryMax=16G`, `-j4`) when the first stopped
  answering its sync. The shards crate was checked with `SHARDS_HELPERS=skip` and
  placeholder `SHARDS_INIT_BINARY`/`SHARDS_SERVER_BINARY` files: what it embeds is not built
  or run by these checks.

## Light checks (Linux x86_64 gnu)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean after one module-order fix (main.rs) |
| `cargo clippy -p shards-abi -p shards-api -p shards-ipc -p shards-init -p shards-testguest --all-targets -- -D warnings` | clean |
| `cargo clippy -p shards --all-targets -- -D warnings` (lib, bin, tests incl. `tests/api_image.rs`) | clean |
| `cargo test -p shards-abi --lib` | 37 passed (9 new: `run::members::tests`) |
| `cargo test -p shards-api --lib image` | 9 passed (new: `image::tests`, `image::wire::tests`) |
| `cargo test -p shards-ipc --lib every_message_kind` | passed (`MEMBER_ENDED` 87 unique) |
| `cargo test -p shards-init -- staged stats_scope` | 7 passed (new: `staged::tests`, `stats_scope_tests`) |
| `cargo test -p shards --bin shards api_run` | 4 passed (new: `api_run::tests`) |
| `cargo test -p shards-api --lib` (whole) | 58 passed, 3 failed in the pre-existing `taken::linux` allocation tests; attribution in "Open" below |

Not run by this implementer (owed to ROOT, §8 of the record): the seven-target lint
(`scripts/lint`), any guest build for musl, every real-VM test, any Windows or macOS build.

## Mutations (R10): each guard removed, its test fails for that reason

Script: `mutate.py` beside the gate (applies one change, runs the one test, restores the
file and touches it so cargo rebuilds), adacachy, after the gate above.

| Mutant | Guard removed | Test | Result |
|---|---|---|---|
| M1 | abi `variable_ok`: `SHARDS_DOMAIN` refused | `run::members::tests::variables_are_checked` | killed |
| M2 | abi `path_ok`: `..` component refused | `run::members::tests::paths_beneath_the_home_alone` | killed |
| M3 | init `staged::write`: `O_NOFOLLOW` on a directory | `staged::tests::links_in_the_way_are_not_followed` | killed |
| M4 | init `staged::check`: a member that runs nothing refused | `staged::tests::staging_into_no_running_member_is_refused` | killed |
| M5 | init `stats_cgroup`: a joiner's exec reads its own cgroup alone | `stats_scope_tests` | killed |
| M6 | api wire `Reader::end`: trailing bytes refused | `image::wire::tests::a_cut_or_padded_request_is_refused` | killed |
| M7 | api `request`: a variable's key holding `=` refused | `image::tests::what_the_guest_refuses_is_refused_first` | killed |
| M8 | run host `kept`: a grant the image does not give refused | `api_run::tests::grants_narrow_and_never_widen` | killed |
| M9 | run host `ports_granted`: no ingress receiver, no command, refused | `api_run::tests::ports_need_a_receiver_without_a_command` | killed |

These are unit-level guards. The forbidden effects they stand for (a widened grant reaching
the network process, a staged file landing outside a member's home, a member's credential
reaching another) are owed as real-VM observations (§8 of the record).

## R01–R16 accounting (implementer's part)

- R01: no unwrap/expect/panic/indexing in production code (clippy's denials clean); checked
  arithmetic on untrusted sizes (wire counts bounded by the bytes left, `checked_add` for
  frame lengths); poison-tolerant locks; threads by `Builder`.
- R02/R03: decision record with sources and alternatives; no new runtime literal (bounds are
  `run::MAX_PAYLOAD`, `run::CHUNK`, `run::TOKEN`, Linux's `PATH_MAX`/`NAME_MAX`, the
  permission bits); every wait on a fact.
- R04: states and refusals typed (`ImageRefusal`, `wire::Cause`, `RunEnd`, `MemberEnd`,
  `StreamEnd`); unknown tags and versions refused; `Secret` never in Debug.
- R05/R13: one new interface (`Images`/`ImageGuest`) at the existing api seam; a simulator
  for it is owed.
- R09: the three defects of 2026-11-12 (below) were each reproduced live before their fixes.
  The three `taken::linux` failures are not attributed until reproduced (below).
- R11/R12: the run host on Linux, macOS and Windows, its serving one implementation over
  `shards_ipc::Stream`; Windows arm64 unbuilt and unrun (above); the run host's launch is a
  third copy of the builder's and the daemon's (owed, record §3.5).
- R14/R15/R16: no profiling or benchmarks run; one kiln agent on jupiter, at ROOT's word,
  for the Windows host's build and its WHP runs; kiln's headroom guard froze the first
  Linux run on adacachy and was not bypassed; runner caches left in place.

## Real VMs, 2026-11-12: main's CI failures, their causes and fixes (R09, R10)

`crates/shards/tests/api_image.rs` had never run before main's CI ran it. Every run below
is `cargo test --profile test-release -p shards --test api_image`, serial
(`-- --test-threads 1`) except on CI.

| Run | Host | Revision | Result |
|---|---|---|---|
| CI 43230466376 | GitHub x86_64 KVM (gnu, musl jobs) | 8f3cd5b74 (main) | 4 boots FAILED about 2 s after the test guest built; the share test hung until the jobs' 56 and 60 minute limits: its run host in `do_wait` on its share process, whose door it still held (the job's hang dump) |
| CI 43275175563, job 131382706564 | GitHub x86_64 KVM | 74cb24706 (boots bounded; a Boot refusal quotes the run host's last words) | 5 boots refused in 58 s, each `the VM ended before it took its run` after `its guest is ready`; no hang |
| jupiter (Windows 11 26200, WHP), the D231 Windows lane | x86_64 WHP | e1be4007c + the request fix reverted by hand (log retention 0, 0) + the VM's report of a refused request | RED: `stop_ends_the_run_at_once` refused in 2.43 s, quoting the VM's `shards: a request whose log keeps nothing`; `its_request_is_one_a_warm_vm_takes` FAILED `LogRetention { size: 0, files: 0 }` |
| same | same | the fix restored | GREEN: both pass |
| jupiter | x86_64 WHP | e1be4007c (request fixed) | 5 passed; `stats_read_the_run_and_a_member` FAILED `no answer within 120s` |
| adalundhe-macbook-pro (M5 Max, macOS 26.5.2) | arm64 HVF | 686514aa0 | 5 passed; stats FAILED as on WHP |
| jupiter | x86_64 WHP | e1be4007c + the link lock fix + the VM's report | 6 passed |
| adalundhe-macbook-pro | arm64 HVF | f6051bc93 | 6 passed, 4.12 s |
| kiln-a094 (Fedora 44, kernel 6.19.10, i9-14900HX) | x86_64 KVM | 5333c2747, main after the Linux landing | 6 passed, 49.98 s |
| jupiter, a stale tree by mistake | x86_64 WHP | e1be4007c, without the link lock fix | `stats_read_the_run_and_a_member` FAILED `no answer within 120s` again: the lock's second live red |
| adalundhe-macbook-pro | arm64 HVF | 731b5e54f, the Windows host (one implementation over `shards_ipc::Stream`) | 6 passed, 20.71 s; `api_run` unit 9/9 |
| kiln-a094 | x86_64 KVM | 731b5e54f | 6 passed, 4.28 s; `api_run` unit 9/9; clippy (`-p shards -p shards-api -p shards-ipc --all-targets -D warnings`) clean |
| jupiter (a fresh clone) | x86_64 WHP | 731b5e54f | 6 passed, 147.32 s with its test guest's build; `api_run` unit 9/9; clippy (`--target x86_64-pc-windows-msvc`, the same three crates) clean |

Owed: arm64 KVM (no arm64 Linux host here has KVM; boot.rs's nested path), and Windows
arm64: unbuilt, as jupiter has no C compiler for it (aws-lc-sys needs clang-cl: Build Tools'
`VC/Tools/Llvm` holds clang-format and clang-tidy alone, and neither ARM64 `cl.exe` nor ARM64
libraries are installed), and no host here runs WHP on arm64 (record §4).

Causes, each reproduced live before its fix:
1. **A request a warm VM refuses** (686514aa0). The run host's `RunHead` kept `RunHead`'s
   defaults, log retention 0 and 0; a warm VM refuses a request whose log keeps nothing
   (warm.rs `head`), so every boot ended at `TAKEN`. The VM said nothing of it before its
   link closed (its other failures it says there, vm_run.rs), and the run host killed it at
   once, so no refusal said why until the VM reported it there too. Fix: `run_request`
   keeps a daemon run's retention with no log (`DEFAULT_LOGS`), held by
   `its_request_is_one_a_warm_vm_takes` to the VM's own check (`RunHead::keeps_its_log`).
2. **A lock held for the run's life** (f6051bc93). `serve`'s follower took its clone of the
   link in a `match` scrutinee, whose guard lives to the match's end: the link's mutex was
   held through `follow`, and a stats exec's send waited on it until the run ended. The
   Windows lane found it with labelled steps on WHP; HVF failed the same way.
3. **A share process waited for with its door held** (86f3c87b2), CI's hang: reaped now as
   a network process is, within `netproc::GRACE` (686514aa0), after its door is let go of.

## Open

- `taken::linux` (crates/api, pre-existing tests from
  docs/research/measurements/managed-proc-main/reader-tests.rs) failed 3 of 61 in a whole
  `shards-api --lib` run on adacachy, a host under load from other work. Their refusal hook
  is thread-local, so another test's allocations cannot trip it, but they compare
  `/proc/self/fd` counts before and after, which another test thread's descriptors change.
  The reproduction (alone, serially, and on the base revision) is recorded in the report.

## Review (plan step 2, goal 52's step 4)

Reviewer: goal 52's agent (role implementer), 2026-11-13, at origin/main 039f5019c. Policy at that revision:
AGENTS.md f66d304c…6266db41, docs/development-rules.md df2acf47…38368fb63, docs/development-evidence.md
61613a24…68fb63. It reviews the lane's 16 commits on main: 7c69cfd46 405a2ddd7 8fb70a6f7 d1f21886d f6f9a008d
4ef075fd4 31b3f106b 86f3c87b2 74cb24706 686514aa0 f6051bc93 72317ade0 e5830745e f62150ef6, and the Windows host
731b5e54f and 53277586c.

Method: the production code of the files the lane wrote, read against R01-R16, with targeted reads. That's api_run.rs
1-2914, image.rs 1-497, image/session.rs 1-1120, image/wire.rs 1-2006, abi run.rs 1-1001 and run/members.rs 1-576,
and init staged.rs 1-203; their test modules follow. It also covers the 431 lines the commits added to shared files
(init run, domains and managed; ipc; vm_run, warm, spec, daemon, main and cli/mod; testguest). Scans looked for what
clippy's panic denials don't catch: `assert!`, panicking std calls, arithmetic, casts, literals, clocks, sleeps and
caches.

### Findings

- **R01: no defect.**
  - Every frame read (api_run.rs `records`, wire.rs `read_frame`) goes through `shards_abi::run::parse_header`, which
    refuses a length past `MAX_PAYLOAD` before `try_reserve_exact`.
  - The one unchecked subtraction (`feed.room -= n`) follows its bound check.
  - `swap_remove` and `insert` take indexes from `position` and `binary_search`.
  - The casts widen, or encode enum tags and small records.
  - Every `assert!` is in a test module.
- **R03: none new.** The values are protocol tags and ports, Linux's `PATH_MAX`/`NAME_MAX` and exec bound
  (`MAX_PAYLOAD`), the permission bits, and the existing `CHUNK` reused as the client's read-ahead. The daemon's
  `DEFAULT_LOGS` is `disk::log_retention`'s.
  - Waits are the caller's timeout (`Duration::MAX` for none) or the request's `ready_by`.
  - A clock out of range is refused `Boot` (this host's) and a budget past a u64 `Request` (the caller's), as each
    is.
- **R04: typed end to end by cause, no defect.**
  - The run host's `No(Cause, _)` (Request 22 sites, Image 12, Widening 5, Boot 13) crosses as `wire::Refused`, and
    the client maps it one to one to `ImageRefusal` (session.rs 740-754).
  - Hex, its one caller outside tests, maps each variant to a typed refusal of its own (hex-runtime
    `guest/image.rs` 181-226, Hex 65ef204f), carrying the text as its `why` too.
  - Within `Boot` the part (network, share, launch, the VM's last words) is text. No caller tells those apart, and
    none models retry, so a finer type would be an interface without a caller (R05). It's recorded as a design
    question, not a defect.
- **R05: no defect.** No sleep, poll or cache in the lane's production code. The client's waits are a condvar
  bounded by the caller's own timeout (session.rs 274-295), and the run host's are facts (`READY`, `TAKEN`, exits,
  channel ends).
- **R12: the third launch copy, the gap evidenced (not folded here).** Three functions start a VM process with its
  network process:

  | Launch | Unix | Windows |
  |---|---|---|
  | daemon.rs `start` | 8382-8535 | 8544-8728 |
  | build/builder.rs `launch` | 569-645 | 653-740 |
  | api_run.rs `launch` | 1125-1238 | 1240-1384 |

  That's about 345 lines on Unix and 418 on Windows doing one job.
  - The daemon's and the run host's Unix launches share one descriptor layout: the link at 3, macOS's grants at 4,
    the ring's `VM_FDS` and `VM_RELEASE_FD`, with `--grants 4`, `--net` and `--net-release`.
    - The daemon adds its spawner group (D183), `--layer`/`--layer-cloned` and the GPU's binary.
    - The run host adds its share processes.
  - The builder's layout differs: grants at 3, `--attach` at 4, its `SAID_TO` pipe, no release descriptor.
  - On Windows all three make the VM suspended in an AppContainer, give handles by name and start the network
    process on it.
  - The seam is one launch, for example `netproc::launch_vm` taking a descriptor plan (link, grants, attach, said-to,
    network side) and the spawner group, used by the daemon and the run host first (one layout), then the builder.
  - It isn't folded here because no unit test reaches any of the three. A fold is proven only by every backend's
    real runs of each caller: api_image.rs; a daemon run (the cold start of every run, D26); the builder's
    (tests/build.rs). Those are HVF, KVM x86_64, KVM arm64 (L1) and WHP x86_64, with R14's cold-start costs before
    and after. Its Windows half compiles on no host this step may use. Owed to ROOT as its own slice.
- **`taken::linux` (the record's Open item): not a D231 defect.**
  - The three failing tests are taken::linux's descriptor-counting reader tests. 037866828 (Hex vendor patch 0030)
    runs each one alone.
  - Its message names this failure ("`cargo test -p shards-api` failed on the integration base"), with red, green and
    guard removal in 30 runs each.
  - It is on main, and step 1 reviewed it. It is in none of the lane's tested revisions: base 82083f27, 75c36864f and
    4ef075fd4 lack it (`git merge-base --is-ancestor`). 31b3f106b, which wrote the note, has it.
- **CI's api_image failures at d35c4cf79 (run 43303460342): each is the Windows host's absence.**
  - Six tests failed: `a_published_ingress_port_reaches_its_member`, `a_share_at_a_granted_volume_reaches_the_harness`,
    `staged_credentials_reach_the_harness_and_its_output_and_end_come_back`, `stats_read_the_run_and_a_member`,
    `stop_ends_the_run_at_once` and `what_the_image_does_not_grant_is_refused_before_boot`.
  - All six failed on the x86_64 Windows jobs alone (windows-2025-vs2026 and windows-2022). Five failed on
    `Unsupported { what: "the run host", why: "an orchestrator's run on Windows … is owed (D231)" }`. The sixth's
    `ImageRefusal::Image` match failed, consistent with that same refusal coming first.
  - d35c4cf79 doesn't contain 731b5e54f, the Windows host, which ran 6 of 6 on jupiter (above). None is left
    unexplained by it. A run on a main that has it is owed (below).
- **p/runtime-image-boot-win: superseded, and its local branch deleted.**
  - Every feature of its two WIP commits (e1be4007c, 25868ba5f) is on main.
  - Their `LogRetention::keeps` landed as `RunHead::keeps_its_log` (ipc lib.rs 2096, warm.rs 405, 686514aa0).
  - `origin/p/runtime-image-boot-win` remains on GitHub. Deleting it is a push, ROOT's.

No code defect was found, so this review lands no code.

### Light gates on main (this review)

These ran on adas-mac-studio.local (macOS 26.2 arm64, rustc 1.98.1) at main 039f5019c, as kiln run r231, with
`SHARDS_HELPERS=skip` and placeholder init and server binaries.

| Command | Result |
|---|---|
| `cargo test -p shards-abi --lib run::` | 33 passed |
| `cargo test -p shards-api --lib image` | 35 passed |
| `cargo test -p shards-ipc --lib every_message_kind` | 1 passed |
| `cargo test -p shards --bin shards api_run` | 17 passed |
| `cargo clippy -p shards-abi -p shards-api -p shards-ipc -p shards --all-targets -- -D warnings` | clean |

Not run here: shards-init's tests (a guest crate, whose macOS build is a stub), and Linux and Windows (owed below).

### R01-R16 accounting (review)

- R01, R03, R04, R05: as above.
- R02: the decision record's sources stand.
- R06: api_run.rs's `hosted` module is about 2,200 lines. The other two launch copies would go with the R12 fold.
- R07: comments state sources and invariants.
- R09 and R10: no defect is claimed, so there's no fix to pair. The two open items are explained by evidence already
  on main (037866828; the jupiter run at 731b5e54f).
- R11: Windows arm64 is unbuilt and unrun, and KVM arm64 is unrun (record §4). Both are unverified.
- R12: the gap above, owed.
- R13: no redundant layer found besides R12's.
- R14 and R15: no measurement; implementer scope.
- R16: none.

### Owed to ROOT (exact runs)

1. `cargo test --profile test-release -p shards --test api_image -- --test-threads 1` at the landed main, on HVF
   (adalundhe-macbook-pro, through docs/research/measurements/macos-sched/launchd-run.sh), KVM x86_64 (adacachy2),
   KVM arm64 (boot.rs's L1 path, `SHARDS_L1_TESTS=api_image`) and WHP x86_64 (jupiter). On WHP that's the run that
   confirms d35c4cf79's six are gone.
2. nesting.rs's image tests (`cargo test --profile test-release -p shards --test nesting`) on the same backends.
3. The light gates this review couldn't run on its one host:
   - `cargo test -p shards-init -- staged stats_scope` and the guest crates' musl clippy (Linux);
   - the lane's unit tests and clippy on Linux x86_64 and Windows x86_64;
   - `scripts/lint`.
4. R12's fold, as its own slice, with the runs above, a daemon run, the builder's tests and R14's cold start before
   and after.
5. Unverified, no host: WHP arm64.
