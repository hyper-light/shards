# guest-cpu-time-v1: review (plan step 2, goal 52's step 6)

Reviewer: goal 52's agent (role implementer), 2026-11-13, at origin/main 308895ef4. Policy at that revision:
AGENTS.md f66d304c…6266db41, docs/development-rules.md df2acf47…38368fb63, docs/development-evidence.md
61613a24…68fb63. Commits reviewed: 221c73774 6396a78b8 08ce60e4f 81f633ba0 dad5db63a e4b43764a dcaae37b4, with the
lane's record (README.md, gates.md).

## Findings

- **R04: the record's own gap stands, and its fix is a slice of its own.** Each backend's `GuestClock::read`
  returns a typed `hv::Error`:
  - HVF: `Call(sys::Error { op, code })`;
  - KVM: `Call { op, error }`, its errno already text;
  - WHP: `Call { op, hr }`.

  crates/vmm/src/vm/runtime.rs `guest_clock` (lines 1670-1673) makes each a `String`. From there it is text
  everywhere: `guest_time::Read`, `Clock.read`, `VcpuTime.unread`, the ` unread WHY` text form, the VM process's
  `GUEST_TIMED` answer, the daemon's stats JSON, the run host's `guest_times` (api_run.rs) and D231's image wire
  (api image/wire.rs `VcpuTime`, `guest: Result<GuestTimes, String>`). Typing it end to end changes that wire (its
  VERSION, which Hex vendors). The brief, given to a child of this goal, is /tmp/s2/steps/step-6-r04-child.md. Its
  landing is recorded here when it lands.
- **The record's item 7 is fixed.** c17348d8b ("has_vm_flag lives in the CLI, its only caller: the VM process
  compiles spec.rs and found it dead") is on main.
- **R01: no defect.**
  - `mach_ns` saturates its multiply, takes a zero denominator as no scaling and clamps its `u128` to `u64`
    (platform/unix.rs 2773-2792).
  - `whp_guest_ns` saturates (hv/mod.rs 128-130).
  - WHP's `WHvGetVirtualProcessorCounters` call has its SAFETY comment, with the counters' size given.
  - HVF's reader and its exit path are atomics, plus a condvar taken only while a reader waits.
- **R05: no per-exit lock.**
  - HVF's exit stores the exec time and bumps `stints`. It takes `left` only when `waiting` is nonzero
    (hvf/mod.rs 681-698).
  - The registry's lock is taken by join, leave and sampling alone.
  - The cost left: as vCPUs leave together, each takes one final read under that lock, serialized (on KVM a /proc
    open and read each). That's the record's item 8, whose measurement is owed. Reading before taking the lock and
    folding under it would shorten the hold, but measure first (R14).

## What ran where

On adalundhe-macbook-pro (macOS 26.5.2 arm64, rustc 1.98.1), natively, at main 308895ef4 (kiln runs r249 and the
one after), with `SHARDS_HELPERS=skip` and placeholder init and server binaries:

| Command | Result |
|---|---|
| `cargo test -p shards-vmm --lib -- mach_ns guest_time stat` | 25 passed, 1 ignored: the guest_time registry's 9, the stat line's, WHP's guest ns |
| `cargo test -p shards-vmm --lib -- mach_time a_threads_clock` | 2 passed (`mach_time_is_counted_in_nanoseconds`, `a_threads_clock_counts_what_it_runs`) |
| `cargo test -p shards-ipc --lib every_message_kind` | 1 passed |
| `cargo clippy -p shards-vmm -p shards -p shards-vm-process --all-targets -- -D warnings` | clean (3 min 33 s, cold) |

The ignored test, `hv::hvf::tests::a_vcpus_guest_time_is_read_from_another_thread_as_it_spins`, makes a VM, so it's
ROOT's. This closes the record's item 6, the native macOS checks.

## R01-R16 accounting (review)

- R01, R05: as above.
- R03: the record's derivations stand. `STAT_LINE_MOST` is a page, seq_file's first buffer.
- R04: the gap above; its slice is in progress.
- R06, R07, R13: small functions; the comments carry sources and orderings.
- R09, R10: no defect is claimed here. The R04 slice brings its own red, green and mutation.
- R11: WHP arm64 is unverified (no host), and KVM arm64 is through L1.
- R12: one registry and wire; each backend's clock is the smallest adapter.
- R14, R15: no measurement.
- R16: none.

## Owed to ROOT (exact runs)

1. `cargo test --profile test-release -p shards --test guest_time -- --nocapture` on HVF (adalundhe-macbook-pro,
   through docs/research/measurements/macos-sched/launchd-run.sh), KVM x86_64 (adacachy2), KVM arm64 (boot.rs's L1
   path, `SHARDS_L1_TESTS=guest_time`) and WHP x86_64 (jupiter). WHP arm64 is unverified, for want of a host.
2. The HVF ignored test, alone:
   `cargo test --profile test-release -p shards-vmm --lib -- --ignored --exact hv::hvf::tests::a_vcpus_guest_time_is_read_from_another_thread_as_it_spins --nocapture`.
3. R14: the per-exit cost of `hv_vcpu_get_exec_time` on an exit-heavy workload, before and after, and a machine's
   teardown with many vCPUs (the record's items 5 and 8).
4. After the R04 slice lands: the same real-VM runs at its revision, the Linux and Windows light gates, and
   `scripts/lint`.
