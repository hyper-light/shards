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
2026-11-13T02:43Z, tree 9281f24ca; log `~/g121-tmp/cold-migrate/s1m-gate.log` there.

| step | result |
|---|---|
| `cargo test -p shards-vmm --lib arch::` | 45 passed |
| `cargo test -p shards --bin shards -- daemon::moves cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed` | 10 passed |
| M1 | exit 101: `a_microvm_of_another_hypervisor_is_refused_before_anything_moves` (moves.rs:2021, `None` where `Some(Hypervisor { name: "web", hypervisor: "kvm", here: Some("hvf") })` was due) and `a_move_of_another_hypervisors_microvm_is_refused_typed` (daemon.rs:14926, status 0 and what this machine lacked, `{"image":true,"files":[],"unpacks":["zstd"]}`, where status 1 was due) failed |
| M2 | exit 101: `a_move_of_another_hypervisors_microvm_is_refused_typed` (daemon.rs:14927, nothing typed on stdout: "EOF while parsing a value") failed |
| M3 | exit 101: `a_refusal_is_said_from_its_type_and_else_in_the_steps_words` (cli/moves.rs:597, "anything", the step's words, where "a microVM named web is here already" was due) failed |
