# Moves between machines: harnesses and their records (D176 parts 6 and 7)

Roadmap items 7a (a microVM moved saved) and 7b (moved live). Two harnesses live here:

- `two-hosts.sh`: moves between two machines, or two homes of one, end to end on real
  microVMs, with n, p50, p90, p99 and max of each move (its header says how to run each
  subcommand; PM M460, M462, M497 and M499 are its runs). Its `e2e` runs tests/moved.rs's
  tests of another machine, an invitation each; the run sheet below (step 5) gives each
  pair of machines' commands.
- `light-gate.sh`: the unit tests of a move's checks, then each guard's removal, which must
  fail them. No microVM boots. Each roadmap-7a slice that adds a guard adds its mutant here.
  Since 2026-11-13 an implementer runs its unit tests alone, on one host; the mutants are
  the coordinator's to run. Slice 1's ran once, below, before that rule.

## The light gate

Run in a copy of the tree (rsync, no `.git`; the copy's `.gate-rev` names the revision it
is of), at nice 10, each step's command and exit logged:

    rsync -a --delete --exclude .git --exclude /target CHECKOUT/ COPY/
    git -C CHECKOUT rev-parse HEAD > COPY/.gate-rev
    CARGO_TARGET_DIR=TARGET bash COPY/docs/research/measurements/migrate/light-gate.sh COPY LOG

A unit step passes with exit 0. A mutant passes the gate when its step exits non-zero with
its named test failing on its assertion: the guard it removed is what that test holds. The
script puts each mutated file back with a fresh mtime, so that cargo rebuilds it.

## Slice 1: a move's refusals typed, another hypervisor's microVM refused before anything moves

Commit a9cf3b94c, merged with main 8035c5812 as 9281f24ca. The guards (D176 part 6, "Refused
before anything moves"):

- M1, `platform_refused` (daemon/moves.rs): a plan of another hypervisor taken.
- M2, `move_step`'s `ready` (daemon/moves.rs): the refusal said in words alone, its type not
  on stdout.
- M3, `unready` (cli/moves.rs): the mover reads the step's words, never its type.

### Linux x86_64, adacachy

AMD Ryzen 9 9950X3D2 (32 threads), CachyOS, Linux 7.2.9-2-cachyos, KVM; rustc 1.98.1;
2026-11-13T02:43Z, tree 9281f24ca; log [logs/s1m-gate.log](logs/s1m-gate.log).

| step | result |
|---|---|
| `cargo test -p shards-vmm --lib arch::` | 45 passed |
| `cargo test -p shards --bin shards -- daemon::moves cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed` | 10 passed |
| M1 | exit 101: `a_microvm_of_another_hypervisor_is_refused_before_anything_moves` (moves.rs:2021, `None` where `Some(Hypervisor { name: "web", hypervisor: "kvm", here: Some("hvf") })` was due) and `a_move_of_another_hypervisors_microvm_is_refused_typed` (daemon.rs:14926, status 0 and what this machine lacked, `{"image":true,"files":[],"unpacks":["zstd"]}`, where status 1 was due) failed |
| M2 | exit 101: `a_move_of_another_hypervisors_microvm_is_refused_typed` (daemon.rs:14927, nothing typed on stdout: "EOF while parsing a value") failed |
| M3 | exit 101: `a_refusal_is_said_from_its_type_and_else_in_the_steps_words` (cli/moves.rs:597, "anything", the step's words, where "a microVM named web is here already" was due) failed |

Landed as merged with main again, 4a79f7f93 (2026-11-13T03:07Z,
[logs/s1m3.log](logs/s1m3.log)): the two unit steps as above, 45 and 10 passed, and
`cargo clippy -p shards -p shards-vmm --all-targets -- -D warnings` clean.

## Step 2: the x86-64 CPU baseline on KVM (D245)

The guards (D245; mutants M4-M10 of `light-gate.sh`):

- M4, `hv::kvm::x86_64` `Vm::new`: a VM not told its baseline (`cpuid::within`).
- M5, `cpuid::meet`: every bit either machine has, not both.
- M6, `cpuid::meet`: a meet of two vendors' CPUs.
- M7, `cpuid::FEATURES`: XSAVE's state components not among the features.
- M8, `cpuid::decode`: a baseline's text of any number of leaves.
- M9, daemon/moves.rs `unmasked`: an x86-64 guest told a baseline still checked by Linux's
  names.
- M10, daemon/moves.rs `baseline_refused`: a baseline taken where this machine's KVM shows
  less of it.

At the predecessor's tree (adacachy, Linux x86_64, KVM, from 2026-11-13T02:05Z, tree a9cf3b94c and its uncommitted edits,
[logs/s2-gate.log](logs/s2-gate.log)): `cargo test -p shards-vmm --lib -- arch::
hv::kvm::x86_64::tests::a_vm_told_a_baseline_gives_its_vcpus_no_more` 48 passed (the KVM
test makes a VM with no guest), the move's tests 11 passed, `--test moved --no-run`
compiled, and each of M1-M10 failed its named test.


Its own checks, at 2192b9dac (one host, adas-mac-studio.local: Apple M4 Max, macOS 26.2,
rustc 1.98.1, 2026-11-13T04:07Z; its tree carried there as a git bundle onto a clone of
main): `cargo fmt --all --check` clean; `cargo test -p shards-vmm --lib -- arch::` 48 passed,
`a_baseline_holds_supervisor_state_amx_and_kvms_own_features` among them (red at 0993723ee,
whose sample of KVM's features already had the one it moves); the move's tests 10 passed;
`cargo test -p shards --test moved --no-run` compiled; `cargo clippy -p shards -p shards-vmm
--all-targets -- -D warnings` clean natively and with `--target x86_64-unknown-linux-gnu`
(zig as scripts/lint sets it), where it first refused `assemble`'s eight arguments.

Owed (x86-64 Linux with KVM; the coordinator's):

    cargo test -p shards-vmm --lib -- arch:: hv::kvm::x86_64::tests::a_vm_told_a_baseline_gives_its_vcpus_no_more
    cargo test -p shards --bin shards -- daemon::moves cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed
    cargo test -p shards --test moved -- an_x86_cpu_baseline_is_all_its_guest_finds_and_goes_with_it

The first two hold `a_vm_told_a_baseline_gives_its_vcpus_no_more` (a KVM VM, no guest),
`a_baseline_masks_every_feature_an_x86_guest_finds` and
`a_machine_short_of_a_baseline_refuses_it_by_each_register`, built on x86-64 alone; the third
is the E2E on two homes of a KVM x86-64 machine.

## Step 3: a WHP microVM's CPU checked before anything moves (D245)

`identity_refused` (daemon/moves.rs): a microVM saved on another CPU refused where WHP on
x86-64 restores a snapshot only on its own; `move step cpu` refusing a baseline there; the
plan's `cpu_identity`, read back empty from an earlier build's plan. Held by
`a_microvm_saved_on_another_cpu_is_refused_where_a_restore_takes_only_its_own` and the
refusal's round trip (`a_refusal_is_typed_on_the_wire_and_says_what_it_said`).

At 0551545cc (one host, adas-mac-studio.local: Apple M4 Max, macOS 26.2, rustc 1.98.1,
2026-11-13T04:23Z): `cargo fmt --all --check` clean; `cargo test -p shards-vmm --lib --
arch::` 48 passed; `cargo test -p shards --bin shards -- daemon::moves cli::moves
a_move_of_another_hypervisors_microvm_is_refused_typed` 11 passed; `cargo clippy -p shards
-p shards-vmm --all-targets -- -D warnings` clean natively and, through cargo-xwin as
scripts/lint runs it, with `--target x86_64-pc-windows-msvc`, which compiles the WHP side
(`hv::whp::host_cpu_id`, `vm::host_cpu_id` on WHP).

Its mutant is M11 of `light-gate.sh` (added with step 6), the coordinator's to run.

Owed (Windows x86-64 with WHP; the coordinator's): the WHP partition probe behind
`shards-vm cpu` there (`hv::whp::host_cpu_id`), and the refusal end to end between two
Windows hosts of different CPUs, none here:

    shards move vm NAME CONTEXT    # from one WHP host to another of another CPU: refused at
                                   # `ready`, "this machine's CPU is not the one NAME was
                                   # saved on", NAME going on where it was

## Step 4: a moved microVM's published ports checked before anything moves (D176 part 6)

`ports_refused` (daemon/moves.rs): each port the plan says its run publishes bound here as
a start binds it (`publish::bind`), let go at once; one that cannot be bound refused typed
(`Refused::PortTaken`). Held by `a_move_whose_published_port_is_taken_here_is_refused_typed`
(daemon.rs: a port a test listener holds refused, a range with a free port taken, the port
taken once let go) and the refusal's round trip.

At 3030a5fab (one host, adas-mac-studio.local: Apple M4 Max, macOS 26.2, rustc 1.98.1,
2026-11-13T04:50Z): `cargo test -p shards --bin shards -- daemon::moves cli::moves
a_move_of_another_hypervisors_microvm_is_refused_typed
a_move_whose_published_port_is_taken_here_is_refused_typed` 12 passed; `cargo clippy -p
shards --all-targets -- -D warnings` clean (it first refused the refusal's size inside
`Unready`, now boxed).

Its mutant is M12 of `light-gate.sh` (added with step 6), the coordinator's to run.

Owed (the coordinator's): the E2E on two homes of each OS,

    cargo test -p shards --test moved -- a_moved_microvms_published_port_answers_there

## Step 6: a moved run's key made anew where it lands; a managed run refused before anything moves (D176 part 6, D238)

ROOT's decision (7a row 11c), in D176 part 6's "Its key, made anew where it lands and never
carried": the move's side of it on step 11's per-run key (17ed8a2e5, `shards.control=true`).
`Plan::key` names the run's key where it ran (`managed::vm_key`, the `vm` line of its managed
metadata, agreed with goal 58's agent); the machine moved to keeps it as the run's
`key-replaced` (a fingerprint alone, none carried from an earlier move); `move step key NAME
[ASKED]` (`Daemon::move_key`) answers `{"key", "replaces"}` and refuses typed a lease asked
under the replaced key (`key_refused`, `Refused::KeyReplaced { name, asked, current }`,
whose fields goal 63's lease refusal takes); `shards move vm` prints the name, then
`{"name", "context", "key", "replaces"}` where either is known. A run with a managed scope
(`managed::has_scope`) is refused typed at its source's `save` and `plan` steps
(`Refused::Managed`), before anything of it is saved, as no restore takes a managed scope
yet; the source's refusals now go typed as `ready`'s do. Held by
`a_lease_under_the_key_a_move_replaced_is_refused_and_the_run_names_its_own` (moves.rs),
`a_managed_runs_move_and_a_lease_under_its_replaced_key_are_refused_typed` (daemon.rs: the
daemon's own `save` and `key` steps) and the refusals' round trip. Its mutants: M13
(`unmovable`) and M14 (`key_refused`), the coordinator's to run.

Found in review before its run, fixed at 3834de09c: the plan's `baseline` field took the
containers' lock as a temporary, which lives to the end of its statement (the Rust
Reference, "Destructors", temporary scopes), and the new `key` field after it locked the
same std `Mutex` on the same thread, which never returns (`std::sync::Mutex::lock`). `plan`
serves the `save`, `live` and `export` steps and `move_plan` the `plan` step: every move
would have hung there. The run's directory is now read in a statement of its own. No unit
test reaches `plan`, which reads a real snapshot; the moves E2E below does. The run before,
at 1c315812b, did not compile (E0308: `move_plan`'s not-running error, not an `Unready`).

At 625169a6e, main 9bdd4a7ec merged (one host, adas-mac-studio.local: Apple M4 Max, macOS
26.2, rustc 1.98.1, 2026-11-13T05:33Z): `cargo test -p shards --bin shards -- daemon::moves
cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed
a_move_whose_published_port_is_taken_here_is_refused_typed
a_managed_runs_move_and_a_lease_under_its_replaced_key_are_refused_typed` 14 passed; `cargo
clippy -p shards --all-targets -- -D warnings` clean.

Owed (the coordinator's): M11-M14 (`light-gate.sh`); a move end to end, which reaches
`plan` (saved and live, two homes of each OS),

    cargo test -p shards --test moved -- a_running_microvm_moves_to_another_machine_and_goes_on_there a_running_microvm_moves_live_and_goes_on_there

and the key's own E2E once a restore takes a managed scope (D238): a run with
`--annotation shards.control=true` moved, the answer's `key` the `vm` line there, its
`replaces` the one here, a lease asked under `replaces` refused.

## Step 5: the two-host E2E, and its run sheet

tests/moved.rs's tests of another machine each join the machine whose invitation
`SHARDS_MOVE_INVITE` names (`another_machine`) with a home of their own, so each needs an
invitation of its own. two-hosts.sh `e2e` runs each alone (`PEER_TESTS`), and `local` runs
each against the other home of one machine. Each checks there what a move carries
(D176 part 6):

| test | what it checks across two hosts |
|---|---|
| `a_microvm_moves_to_another_host_saved_and_live` | a counter in a volume, moved saved, counts on there each count once; a guest stamping 16 MiB, moved live, loses no page's last write (from before step 5) |
| `a_volumes_guest_owners_move_with_it` | a volume's owners, modes, attribute, symlink, device node and names, as its guest sees them (from before step 5) |
| `a_microvm_moved_to_another_host_keeps_its_cpu_image_files_and_connections` | told the two machines' CPU baseline, its guest's CPUs the same there; its image there by its digest, loaded there; the snapshot it moved as alone, and no more files sent than it has; an exec there, and a stop's SIGTERM ending it inside its timeout; a run with no key answered its name alone |
| `a_volume_shared_here_moves_to_another_host_with_one_and_the_other_goes_on_here` | a volume two microVMs share moves with one, the other's file in it as it was; the other's move refused there by the volume's name, it appending on here, each count once on both machines |
| `a_moved_microvms_published_port_answers_on_another_host` | its guest's published port answers there, at the port that machine bound, at its address |
| `a_move_another_host_cannot_take_is_refused_before_anything_moves` | refused at `ready`, typed, nothing sent, nothing there, going on here: by another architecture or hypervisor; else by a CPU short of its baseline (`told_past_every_cpu`). WHP x86-64 tells no baseline (D245): between two WHP x86-64 machines it says SKIP |

Its key (D176 part 6, "Its key"): no run a move takes today has one, since a run with a
managed scope is refused before anything moves (`Refused::Managed`, step 6). These tests
hold a keyless run's answer to its name alone. A key made anew there and named in the
answer waits for a restore that takes a managed scope (D238), and its E2E with it.

`the_harness_runs_every_test_of_another_machine` holds two-hosts.sh's `PEER_TESTS` to the
tests that call `another_machine`, and this sheet to naming each. No microVM boots. Its
mutant is M15 of `light-gate.sh`, the coordinator's to run.

Checked at d07c49d09, step 5 merged with main 24d64a414, on one host (adas-mac-studio.local:
Apple M4 Max, macOS 26.2, rustc 1.98.1, 2026-11-13T06:23Z; the tree carried there as a git
bundle onto a clone of main, `SHARDS_HELPERS=skip` and empty stand-ins for the guest
binaries, as `light-gate.sh` runs): `cargo test -p shards --test moved
the_harness_runs_every_test_of_another_machine` built the test binary, every test of
moved.rs in it, and passed (1 passed, 17 filtered out); `cargo clippy -p shards
--all-targets -- -D warnings` clean. Before it, two-hosts.sh's `invite` and `e2e` were run
here with stand-ins for `shards` and `cargo`: each test got the next invitation, a test that
failed left the rest running and the exit 1, and too few invitations were refused naming how
many to make. None of the tests of another machine has run on a microVM: the run sheet
below is ROOT's.

### The run sheet (ROOT's)

Every machine is at one revision R of main. Build before any invitation, since each lapses
in an hour. `$S` is a machine's checkout and `$H` its
`docs/research/measurements/migrate/two-hosts.sh`:

    git -C "$S" checkout --detach R
    cargo build --locked -p shards --release                            # each machine
    cargo test -p shards --profile test-release --test moved --no-run   # the machine moved from

For a pair, A is the machine moved to and B the machine moved from:

    A, kept running (a background run):  SHARDS=$S/target/release/shards ADDR=<A's address:port> HOLD=10800 $H listen
    A:  SHARDS=$S/target/release/shards COUNT=6 $H invite      # its last 6 lines, an invitation each
    B:  INVITES="<A's 6 lines>" $H e2e 2>&1 | tee e2e.log     # each test alone, an invitation each
    A:  SHARDS=$S/target/release/shards KEEP_LOG=<a file> $H stop

Passing: B's `e2e` exits 0. Each test says `test NAME ... ok` and `test result: ok. 1
passed`, with no `SKIP:` line but where the table below says. A test that fails is said,
and the rest still run. Keep `e2e.log` and A's daemon log, with each host's OS, CPU and the
revision, for the PM entry.

| pair | A, moved to | B, moved from | B's `e2e` | passing |
|---|---|---|---|---|
| KVM x86-64 | adacachy, `ADDR=192.168.1.90:3000` (PM M497: the one UDP port its firewall took from the LAN that nothing held) | adacachy2 | `INVITES=<6>` | all six. The refusal test says `peer cannot take it: … which refused<pid>'s CPU baseline names`. The published-port test connects to adacachy at the port it bound: give `SHARDS_MOVE_PORT`, a TCP port adacachy's firewall takes from adacachy2. PM M497 records no such port, so without one, that test failing with "takes no TCP from this one there" is the firewall's, and it stays owed |
| KVM x86-64, back | adacachy2 | adacachy | as above | as above. adacachy2 needs a UDP port its firewall takes from the LAN, and PM M497 found none free there: owed until one is, which needs the person (root) |
| HVF arm64 | adalundhe-macbook-pro (M5 Max), `ADDR` its default route's address | adas-mac-studio.local (M4 Max) | `INVITES=<6> BASELINE=1` | all six. `BASELINE=1` tells the baseline the two Macs share to the microVMs of the tests that read `SHARDS_MOVE_BASELINE`, and the keeps test tells it either way. The refusal test says `… which refused<pid>'s CPU baseline names`. The published-port test needs A's application firewall to take the TCP |
| WHP x86-64 to KVM | adacachy, `ADDR=192.168.1.90:3000` | jupiter | `COUNT=1` on A, then `FILTER=a_move_another_host_cannot_take_is_refused_before_anything_moves INVITES=<1>`, no `BASELINE` (WHP x86-64 tells none) | `peer cannot take it: refused<pid> ran on the Windows Hypervisor Platform, and this machine runs microVMs on KVM`, then `refused<pid> goes on here` |
| two homes, each OS | (`local`) | | `SHARDS=$S/target/release/shards UNCLAMP=0 $H local` on adacachy, adas-mac-studio.local and jupiter | exit 0: every test of moved.rs between two homes, then the six against the listening home, the refusal test refused by a CPU short of its baseline but on jupiter, where it says SKIP. `UNCLAMP=0` keeps `move` in the run, since kiln ends a launchd job a run starts (PM M499), so its times are a clamped session's, not a measurement |

Linux arm64 has no two KVM hosts here, and its nested L1 guest runs moved.rs's tests between
two homes (boot.rs, `SHARDS_L1_TESTS=moved`), where the tests of another machine say SKIP.
Windows arm64 waits on the parity plan's row 5 host (step 10).

## 7a against R01-R16

Roadmap item 7a is steps 1 to 6 above, with D176 part 6 and D245. "Owed" is the
coordinator's.

| rule | evidence | gap |
|---|---|---|
| R01 | The move's production code returns errors (daemon/moves.rs, cli/moves.rs, `arch::cpu`, `cpuid`, the KVM and WHP baselines); `cargo clippy -D warnings` clean at each landing (above), with the workspace's panic lints denied. Step 5 is test code and the harness | lints are one check: a review of implicit panic paths, owed |
| R02 | The plan's research (01a1d2b8c23575b8bc08d008c9883f70, findings f1-f19); D245's alternatives (Firecracker's templates, libvirt's `hypervisor-cpu-baseline`, Hyper-V's processor compatibility, KVM's api.rst, cloud-hypervisor v54.1's check); D176's (CRIU, Docker's checkpoint) | WHP x86-64's baseline: its deciding runs (D245) |
| R03 | D245's literals (`cpuid::FEATURES` from the SDM, the APM and KVM's cpuid.rst; `decode`'s bound derived from it); the stream's bounds from Linux's limits.h and xattr.h; `MAX_PLAN` a frame's `MAX_FRAME`; a connection's 100 s from RFC 1122 §4.2.3.5 (D176, "Over a slow link") | `running_there` (cli/moves.rs) asks up to 12 times, 5 s apart, with no derivation recorded: found in this review, not changed (outside step 5) |
| R04 | `Refused` typed on the wire (`save`, `plan`, `ready`, `key`) and matched by the mover (`unready`); `CpuShort`; `Unready`; the move's answer's keys (`Keys`) | none known |
| R05 | Waits: the stream's reads and writes block on the link, with no deadline (D176); `running_there` probes after a stream whose restore said nothing. Caches: `host_cpu_id`, this host's CPU as its hypervisor shows a guest, kept for the daemon's life | not reviewed as R05 asks (measured cost, invalidation, a notification instead of `running_there`'s probes): owed |
| R06, R07, R13 | each refusal a function of its own (`platform_refused`, `baseline_refused`, `identity_refused`, `ports_refused`, `key_refused`, `unmovable`) | a review, owed |
| R08 | PM M497: on a gigabit link a cold move ran nowhere 0.91 s at p50, 1.12 at p90 and 9.89 at most (the fresh home's first); a live one 0.21 s at p50 | no competitor's migration measured beside it (QEMU's, cloud-hypervisor's): owed |
| R09 | PM M460 and M462 reproduced live the refusals between Macs and the slow link's ended moves before their changes | steps 3 and 4 (a WHP move to another CPU, a host port taken there, each failing after the whole stream) were found in the source, not reproduced live: owed, between two WHP hosts of different CPUs (none here) and with a port held on A |
| R10 | `light-gate.sh` (mutants M1-M15), `logs/`, PM M460, M462, M497 and M499 | M11-M15 not yet run; step 5's tests not yet run: owed |
| R11 | below: what ran and what is owed for each platform | the run sheet |
| R12 | one move for every OS (cli/moves.rs, daemon/moves.rs, the context's QUIC); `arch::cpu` for each architecture; `restores_on_its_own_cpu_alone` the one WHP x86-64 adapter | none known |
| R14 | not profiled (allocations, page faults, the parent's and kernel's costs) | owed |
| R15 | implementers ran unit tests and clippy on one host; the coordinator runs E2E, mutants and the lint matrix | the run sheet, M11-M15, `scripts/lint` across the matrix (CI on main) |
| R16 | builds in `~/g121-tmp/cold-migrate-mac` on adas-mac-studio.local (a clone and its target, regenerable) | none |

What ran and what is owed, by platform:

- **Linux x86-64, KVM.** Between two homes: tests/moved.rs whole, 9 of 9 at abcea5f5 (PM
  M497). Between two machines, adacachy2 to adacachy: 20 cold and 20 live moves, and the
  two tests of another machine of then (PM M497). Owed: the six of the run sheet each way,
  and `an_x86_cpu_baseline_is_all_its_guest_finds_and_goes_with_it` (D245).
- **Linux arm64, KVM (nested).** Between two homes: tests/moved.rs whole in the L1 guest
  (parity audit row 16, 3efe4190). Between two machines: none here.
- **macOS arm64, Hypervisor.framework.** Between two homes: 20 cold and 20 live moves (PM
  M462), the baseline test (PM M497). Between two Macs: refused as designed where one lacks a
  feature of the other, 38 of 40 at `ready` (PM M460, M462). Owed: the six with a baseline,
  M4 Max to M5 Max.
- **Windows x86-64, WHP.** Between two homes: tests/moved.rs on CI's windows-2025 and on
  jupiter (parity audit rows 14, 16). Owed: WHP to KVM refused (the run sheet); WHP to a WHP
  host of another CPU refused at `ready` (D245; no such pair here).
- **Windows arm64.** Waits on the parity plan's row 5 host.
