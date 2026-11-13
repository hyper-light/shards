# vm-launch-seam-v1: one VM launch for the daemon, the builder and the run host (R12)

Goal 52's plan step 10 (kiln plan 01a1d2c5867975b7ba71815aa3a9cb9e, goal part
01a1d3a178e876e79065cd55b39f4d9d): the fold step 4's review left owed
(runtime-image-boot-v1/README.md, "Review (plan step 2)", R12). D231 points here.

## Identity

- Role: implementer (goal 52's step-10 child, started by ROOT). Light checks only; every real-VM
  run and measurement is ROOT's (below).
- Policy, read in full at origin/main ba9879a66 and unchanged at the bases below: AGENTS.md
  `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`, docs/development-rules.md
  `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`,
  docs/development-evidence.md `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- Source: the fold, a55868e4e, made on main 1acbf2cb9 (ROOT's cgroup fix a853a8138 included and
  kept); merged with main ef0bec4de in 1d1d992b2, where the seam takes up D217 (`netproc::reap`
  follows a network process by the home's record of ends, PM M546); then merged with main
  f5c0c8b04 to land, where D253's shared argv (goal 75's `run::restore_args` and its kin) names
  the link `launch::LINK`.
- Owned paths: crates/shards/src/launch.rs (new); the launch code of daemon.rs `start`,
  api_run.rs `launch` and build/builder.rs `launch`/`Builder::start`; the link constant's uses in
  daemon/refill.rs and daemon/snapshots.rs; main.rs's `mod launch`; D231's sentences on the
  launch; this record.
- Host: adas-mac-studio.local (macOS arm64, Darwin, rustc 1.98.1), through kiln run with a
  carrier (this change as a patch over its base, in a blobless clone of its own), target
  directory ~/g121-tmp/s2-step10-target, `SHARDS_HELPERS=skip`, placeholder init and server.

## The three copies, before

At main 1acbf2cb9 (from each function's doc comment to its closing brace):

| Launch | Unix | Windows |
|---|---|---|
| daemon.rs `start` | 8405-8562 | 8564-8755 |
| api_run.rs `launch` | 1232-1338 | 1340-1491 |
| build/builder.rs `launch` (and its `Launched`, 553-563) | 565-645 | 647-740 |

Each made a VM process and its network process; they differed in descriptors, words, cleanup
and error texts. At 1d1d992b2 against main ef0bec4de, 779 lines of production code go and 684
come (launch.rs's 554 before its tests, and 130 in the callers): 95 fewer. The seam's tests are
343 lines.

## The seam: crates/shards/src/launch.rs

- `launch::run(Plan) -> (Launched, Stream)`: a run's VM, the daemon's (cold boots, warm VMs,
  restores) or the run host's. The seam makes its link, given at `LINK` (3 on Unix, the
  handle `daemon` on Windows), and returns this process's end of it.
- `launch::builder(Plan, attach, said_to) -> Launched`: a build's builder VM, with its join
  disk's socket (`--attach`, D134) and its console report's pipe (`--said-to`, D12).
- `Plan`: the VM's command line, its command first (`run`, `restore`); its binary (`--gpu` in
  its command picks the GPU's build, D144); its network (policy, MAC, bridge, credential grants,
  D31, D242); the home's record of ends (`netproc::reap`, D217); the group it is made under (a
  spawner on Unix, a job on Windows, D183); and on macOS who answers its grants:
  `Grants::Answered` (a thread of the launch's, as the run host and the builder did) or
  `Grants::Kept` (handed back, as the daemon's watch answers a warm VM's with its template's
  directory and judges its start by them, PM M166).
- `Launched`: the VM process, its network process with this process's control of it, and on
  macOS the kept grants.
- Unix: what can fail before any process is made first (the binary, the grants pair, the null
  device), then the network process (`netproc::start`), then the VM (`spawn_in_group`). Where the
  VM does not spawn, its side of the ring is let go of and the network process reaped, followed
  to its end by the home's record (`netproc::reap`). Where the grants thread cannot start, the VM
  is killed and waited for, then the network process reaped (`Launched::end`).
- Windows: the binary, the network prepared (no process), the AppContainer and the grants'
  pipes; the VM made suspended (`spawn_suspended`, `grants_at` 1); the network process started
  on it; the VM resumed, its AppContainer held by its value (`Child::hold`); its grants answered
  on a thread. Every process it starts is a confined one, ended as its value drops (its Job's
  last handle, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), so an early return ends what was started.
- `handed` (one per OS) is each kind's layout: where each end goes and the words that name it.

## Each caller's descriptors and words (the tests' expected values are the old ones)

Unix, at 1acbf2cb9 and now:

| VM | Descriptors | Words |
|---|---|---|
| a run's (daemon and run host, one layout) | null 0, stderr 1 and 2, link 3, grants 4 (macOS), ring 5, 6, 7 (`netproc::VM_FDS`), release 8 (`VM_RELEASE_FD`) | `--grants 4` (macOS), `--net 5,6,7,MAC`, `--net-release 8` |
| a builder's | null 0, stderr 1 and 2, grants 3 (macOS), attach 4, ring 5, 6, 7, said-to 9 | `--grants 3` (macOS), `--net 5,6,7,MAC`, `--said-to 9`, `--attach 4` |

Windows, by name, each in its old order:

| VM | Handles given | Sockets shared | Words |
|---|---|---|---|
| a run's | ask, answer, daemon, null, net-region, net-wake-me | net-wake-peer, net-release | `--net MAC` |
| a builder's | ask, answer, net-region, net-wake-me, said-to | attach, net-wake-peer | `--net MAC`, `--said-to said-to`, `--attach attach` |

Without a network a run's VM has no ring, release or `--net`; Linux has no grants. The words go
right after the VM's command, where the daemon (all its words) and the builder (`--grants`) put
them. The run host appended its words to its command, and the builder put `--net`, `--said-to`
and `--attach` among its own. The meaning is the same: the VM's parsers read each flag by its
name until `--` (vm_run.rs `Common::option`, the run and restore loops), and right after the
command is before any `--`. On Windows `--grants` stays first (`grants_at` 1), which `--net`
needs, as it takes the handles `--grants` named (vm_run.rs `net_host`).

## What else the fold changes

1. **Cleanup, one way for the three.** This is the step's acceptance: a spawn that fails after
   the network process started leaves nothing running. Before, on Unix, each copy cleaned up
   differently, as the source shows:
   - The daemon reaped the network process where the spawn failed. A GPU run's `vm_gpu()?`
     after the network process started returned without reaping it.
   - The run host reaped it with the VM's side still held, so it went only at the reaper's
     kill. Its share process, started after the network process, was not ended where the VM
     did not spawn. `start_shares`'s and `helpers::vm()`'s failures after the network process
     started returned without reaping it.
   - The builder killed the network process at once where the spawn failed. Its grants pair's,
     /dev/null's and `helpers::vm()`'s failures after the network process started returned
     without reaping it, and a grants thread that did not start left the VM running.
   These are the old copies' paths as their source reads; none was reproduced live (R09), and
   none is claimed as a defect found. Now each path is as the seam's section above says. The run
   host starts its share process before the launch and ends it (`Shares::end`) where the launch
   fails.
2. **A GPU's VM from the run host on Windows** was made with `graphics: false` with the GPU's
   binary (api_run.rs's `vm_binary`, from gpus=). It is now made as the daemon's and the
   builder's always were: `graphics` wherever its command says `--gpu`. MoltenVK's
   `MVK_CONFIG_LOG_LEVEL` (1, its errors-only level) now goes on Unix alone, as the daemon and
   the builder set it; MoltenVK runs on macOS.
3. **The builder's AppContainer** is held by its VM process's value (`Child::hold`), as the
   daemon's and the run host's are, instead of a field of `Builder`. It goes as the VM's value
   drops, after its Job has ended it. `Builder::net` is the launch's `Option`; the builder
   drops its control, as before, since a build publishes no port.
4. **Names.** daemon.rs's `VM_LINK` and api_run.rs's `LINK` are `launch::LINK`. The builder's
   `ATTACH` and `SAID_TO` are the seam's layout. The daemon's two `start`s are one, with cfg
   alone for the group and macOS's grants.
5. **Error texts** read `starting the VM (BINARY): ...`, `the VM's link: ...`, `the builder's
   grants: ...`, `answering the builder: ...`. No test or caller matches the old texts
   (searched across crates/ and docs/).

## Light checks

On adas-mac-studio.local (macOS arm64, rustc 1.98.1), through kiln run, one cargo at a time:

| Command | Tree | Result |
|---|---|---|
| `cargo test -p shards --bin shards -- launch:: api_run build::builder` | the fold on 1acbf2cb9 (r554) | 30 passed: launch 4, api_run 22, builder 4 |
| the same test, the spawn's failure path reaping no network process (mutation) | same | `a_vm_that_does_not_start_leaves_its_network_process_reaped` red: "the network process 86159 is left: Some(0)", a zombie still found by its pid |
| the same, a run's VM not handed its release (mutation) | same | `a_runs_vm_is_handed_its_ends_where_the_daemon_and_the_run_host_handed_them` red: its descriptors lack (release, 8) |
| restored | same | 4 of 4 green |
| `cargo clippy -p shards --all-targets -- -D warnings` | same | clean after one fix: `type_complexity` on `program`'s result, now the `Env` alias |
| the same with `--target aarch64-unknown-linux-musl`, zig as scripts/lint sets it (r586) | same | clean |
| `cargo xwin clippy -p shards --all-targets --target x86_64-pc-windows-msvc -- -D warnings` (r586, stopped there) | same | 2 errors: this seam's Windows test (a closure's lifetimes, fixed with nested fns) and the base's api_run.rs `lane` (E0282, fixed on main since) |
| `cargo fmt -p shards -- --check` | merged with ef0bec4de, 1d1d992b2 (r602) | clean |
| `cargo test -p shards --bin shards -- launch:: api_run build::builder` | same | 30 passed |
| `cargo clippy -p shards --all-targets -- -D warnings`, for aarch64-apple-darwin and aarch64-unknown-linux-musl, and through `cargo xwin` for x86_64-pc-windows-msvc and aarch64-pc-windows-msvc | same | clean, all four |
| the reap-removal mutation again, on the merged reap (`netproc::reap(&process, record)`) | same | red: "the network process 6180 is left: Some(2)"; green restored |

The tests and lints after the landing merge with f5c0c8b04 are in its commit message.

Not run here, and why:
- The Windows unit test (`launch::tests::each_vm_is_handed_by_name_what_its_launch_handed_it`,
  `cfg(windows)`): compiled by the Windows lints; its run is owed (below).
- Linux's unit tests: the Linux layout's numbers are checked on macOS, the tests passing no
  grants as Linux makes none; the Linux spawn and reap paths run in CI's unit tests and are owed
  here (below).
- The x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu and x86_64-unknown-linux-musl lints:
  the step asked for one Linux target, and CI lints all seven.

## R01-R16 accounting

- R01: no `unwrap`, `expect`, indexing or unchecked arithmetic in production code. The ring's
  three numbers are a fixed array destructured. A raw socket goes `as u64`, as netproc's do.
  Thread starts are `thread::Builder::spawn`, their failures handled.
- R02: the decision's sources are the three copies at 1acbf2cb9, vm_run.rs's parsers, shards_ipc's
  spawn semantics (`spawn_in_group`, `spawn_suspended`'s `grants_at`, a Windows child's Job ended
  as its value drops, a Unix child that does not kill on drop) and netproc's `start`, `prepare`
  and `reap`. Alternatives weighed:
  - Each caller's own list of (descriptor, number, word): rejected. A number and the word naming
    it would sit in two places per caller, and could collide with the seam's fixed ones.
  - A `Spawner` trait: rejected under R05, as it has one real implementation. The stand-in test
    gives the private `launch_with` its two process makers instead.
  - Words at the end of the command (the run host's place): rejected. A `--` would end the
    VM's flags before them, and Windows' `--grants` must come before `--net`.
  - The seam answering the daemon's macOS grants too: rejected. The daemon's watch answers them
    with a pool's template directory and judges a warm VM's start by them (PM M166), so they are
    `Grants::Kept`.
- R03: no new value. The descriptor numbers and names are the old layouts (netproc's
  `VM_FDS` and `VM_RELEASE_FD`, the builder's 3, 4 and 9, `LINK`). `MVK_CONFIG_LOG_LEVEL` 1 is
  MoltenVK's errors-only level, as the daemon and the builder set it. Reaping is D217's learned
  slice (netproc's).
- R04: no new state machine. `Kind` (Run, Builder) and `Grants` (Answered, Kept) are typed
  variants. Errors stay text (`Cause::Boot` in the run host), as step 4's review decided that no
  caller tells a launch's parts apart.
- R05: no new wait, poll or cache. The reap is netproc's completion wait. The seam replaces three
  copies.
- R06: one launch and one layout function per OS. The daemon's `start` lost its Windows twin.
- R07: comments give each layout's source, the order's reasons and the Job semantics.
- R08: no comparative claim.
- R09: no defect claimed. The old copies' cleanup paths are described from their source (above).
- R10: the stand-in test is red with the reap removed and green with it; the layout test is red
  with the release dropped.
- R11: written for macOS, Linux and Windows on both arches; tested on macOS arm64 alone. Linux
  and Windows unit tests and every real-VM run are owed (below). WHP arm64 is unverified, for
  want of a host.
- R12: the three copies are gone.
- R13: 112 fewer production lines; no layer added beyond the seam.
- R14: no measurement (implementer). R14's cold start, before and after, is owed (below).
- R15: implementer scope: no E2E, VM, benchmark or CI run.
- R16: the target and clone directories are removed after landing.

## Owed to ROOT (exact runs)

At the landed main. Use `test-release`, as CLAUDE.md says, and an empty `DOCKER_CONFIG` for
manual runs.

1. **The run host** (`crates/shards/tests/api_image.rs`, all of it):
   `cargo test --profile test-release -p shards --test api_image -- --test-threads 1`
   - HVF arm64 (adalundhe-macbook-pro, outside the session's QoS clamp):
     `docs/research/measurements/macos-sched/launchd-run.sh /tmp/s10-api cargo test --profile test-release -p shards --test api_image -- --test-threads 1`
   - KVM x86_64 (adacachy2): the command above.
   - KVM arm64 (boot.rs's L1 path, an M3 or later Mac, through launchd-run.sh):
     `SHARDS_L1_TESTS=api_image docs/research/measurements/macos-sched/launchd-run.sh /tmp/s10-l1-api cargo test --profile test-release -p shards --test boot -- --exact snapshot_tests_pass_on_a_kvm_host_of_this_architecture`
   - WHP x86_64 (jupiter): the command above.
2. **A daemon run.** These daemon.rs tests cover the cold boot with a pool of none, a pool's
   warm VMs, a restored microVM with its writable layer, signals, the daemon's stop and its end:
   `cargo test --profile test-release -p shards --test daemon -- --test-threads 1 a_pool_of_none_serves_every_run warm_vms_are_given_their_templates_root_filesystem a_restored_microvm_goes_on_over_a_stopped_containers_kept_file signals_reach_a_warm_run stop_ends_runs_and_waiting_vms a_dead_daemons_waiting_vms_end errors_reach_the_client`
   - `runs_under_a_slice_start_after_it_was_stopped` (D183's spawner group, Linux only) on
     adacachy2.
   - The network's release, through containers.rs:
     `cargo test --profile test-release -p shards --test containers -- --test-threads 1 published_ports_reach_the_guest published_ports_are_free_at_once_while_the_daemon_makes_vms published_udp_ports_carry_datagrams_both_ways`
   - HVF arm64 through launchd-run.sh, KVM x86_64 on adacachy2, WHP x86_64 on jupiter as in 1.
     KVM arm64 through L1:
     `SHARDS_L1_TESTS=daemon:a_pool_of_none_serves_every_run,daemon:warm_vms_are_given_their_templates_root_filesystem,daemon:a_restored_microvm_goes_on_over_a_stopped_containers_kept_file,containers:published_ports_reach_the_guest`
     with the boot.rs command of 1.
3. **The builder** (`crates/shards/tests/build.rs`):
   `cargo test --profile test-release -p shards --test build -- --test-threads 1`
   - HVF, KVM x86_64 and WHP x86_64 as in 1. On WHP, also
     `cargo test --profile test-release -p shards --test build_windows -- --test-threads 1`.
   - KVM arm64 through L1:
     `SHARDS_L1_TESTS=build:run_steps_run_in_a_builder_microvm,build:a_killed_build_takes_its_builder_with_it`
     with the boot.rs command of 1.
4. **R14: cold start (no warm VM) before and after**, from the caller's view, as n, p50, p90,
   p99 and max. Use docs/research/measurements/cold-start/cold.py: a pool of none
   (`SHARDS_POOL=0`), each run restoring its own microVM from the image's template. The two
   builds are arms, their runs interleaved:
   `docs/research/measurements/cold-start/cold.py AFTER/shards alpine 200 /tmp/s10-cold --seed SEED --arm before=BEFORE/shards --log none -- true`
   - SEED is a home that already holds `alpine`, so that neither arm pulls from Docker Hub.
   - BEFORE is main 1acbf2cb9 built with `--profile release`; AFTER is the landed main, built
     the same.
   - On macOS, run it through launchd-run.sh. Run it on HVF arm64, KVM x86_64 (adacachy2) and
     WHP x86_64 (jupiter, where Python is there).
   - The builder's and the run host's launches are on the same seam. Their cold starts, if
     wanted: `shards build --no-cache` of a one-step Dockerfile, and api_image's
     `a_run_keeping_nothing_the_image_grants_boots_and_its_harness_runs`, each timed n times
     per arm.
5. **Unit tests elsewhere:** the Windows test on jupiter,
   `cargo test -p shards --bin shards -- launch::`, and the same on Linux x86_64 (adacachy2),
   where the Unix spawn and reap paths run without grants.
6. **Unverified, no host:** WHP arm64.

## Findings outside this step's scope

- `Builder::start`: where the builder does not boot after its launch (its boot patience passes,
  or it ends as it starts), the VM is killed, but the network process the launch returned is
  dropped unreaped. On Unix it goes as its doorbell hangs up, and stays a zombie of the process
  that ran the build until that process exits. This is unchanged by the fold, seen in source
  and not reproduced.
- The daemon's `start`: where its watcher thread cannot start, the network process goes into
  the dropped closure unreaped. This too is unchanged by the fold, seen in source and not
  reproduced.
