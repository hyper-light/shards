# Guest-memory access checks

Evidence for D29 (docs/design/architecture.md): host threads reach guest memory only
through an `Access`, one at a time, so none of their accesses race, whatever addresses a
guest gives its devices (audit A01). Results are platform-measurements.md M44.

## `check.sh`

Runs ThreadSanitizer over the memory and virtqueue unit tests, and Miri over those that
map only anonymous memory, with a pinned nightly (rust-src and miri components). Among
them:

- `memory::tests::host_threads_take_turns`: eight threads write and read back the same
  unaligned bytes;
- `queue::tests::queues_laid_over_each_other_work_on_two_threads`: two threads work two
  queues whose rings lie on each other's descriptors and rings, so each thread's
  atomic index stores land on bytes the other reads and writes whole or in words.

**Negative control.** `git apply negative-control.patch` gives every access a lock of
its own, which excludes nothing; then both tools must report data races, and did (M44).
`git apply -R` restores the source.

## `save-ab/`

`GuestMemory::save` at two revisions, in alternating fresh processes: `run.py OLD_REV`
builds the harness in a temporary worktree of OLD_REV and in the working tree, each
against its own `crates/vmm`, and reports n, p50, p90, p99 and max per arm and the
paired median difference with a bootstrap 95% interval.
