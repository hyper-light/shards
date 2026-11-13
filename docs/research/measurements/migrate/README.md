# Moves between machines: harnesses and their records (D176 parts 6 and 7)

Roadmap items 7a (a microVM moved saved) and 7b (moved live). Two harnesses live here:

- `two-hosts.sh`: moves between two machines, or two homes of one, end to end on real
  microVMs, with n, p50, p90, p99 and max of each move (its header says how to run each
  subcommand; PM M460, M462, M497 and M499 are its runs).
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

Owed (Windows x86-64 with WHP; the coordinator's): the WHP partition probe behind
`shards-vm cpu` there (`hv::whp::host_cpu_id`), and the refusal end to end between two
Windows hosts of different CPUs, none here:

    shards move vm NAME CONTEXT    # from one WHP host to another of another CPU: refused at
                                   # `ready`, "this machine's CPU is not the one NAME was
                                   # saved on", NAME going on where it was
