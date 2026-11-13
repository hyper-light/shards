# guest-cpu-time-v1: review (plan step 2, goal 52's step 6)

Reviewer: goal 52's agent (role implementer), 2026-11-13, at origin/main 308895ef4. Policy at that revision:
AGENTS.md f66d304c…6266db41, docs/development-rules.md df2acf47…38368fb63, docs/development-evidence.md
61613a24…68fb63. Commits reviewed: 221c73774 6396a78b8 08ce60e4f 81f633ba0 dad5db63a e4b43764a dcaae37b4, with the
lane's record (README.md, gates.md).

## Findings

- **R04: the record's own gap, cured (goal 52's step 6 slice, image channel v14).**
  - Today each backend's typed `hv::Error` becomes a `String` at runtime.rs `guest_clock` (main 308895ef4, lines
    1670-1673), and stays text through the registry, the ` unread WHY` text form, `GUEST_TIMED`, the daemon's JSON
    and D231's image wire. A WHP failure arrives as `WHvGetVirtualProcessorCounters failed: E_ACCESSDENIED` (the
    lane's own test fixture), from which no HRESULT can be read back.
  - Now one type, `shards_abi::guest_time::Unread` (shared by the VMM, shards-api and shards, R12), carries the
    cause each backend meets, with its code:
    - `Gone`;
    - `HvfExit` and `HvfExecTime` (an hv_return_t);
    - `WhpCounters` (an HRESULT, x86_64 and arm64);
    - `ThreadStat` (an errno, or none for a line not parsed).
  - Each backend's `GuestClock::read` returns it, built where the cause is known, never from text.
  - `guest_clock()` is infallible: every backend's always returned `Ok`, so the "no clock" path is gone (R13).
  - Its text form is `CAUSE [CODE]` (`Unread::write`/`parse`, which refuses anything else). It runs through the
    registry, `GuestTimes::encode`/`decode`, the VM process's answer, the run host and D231's image wire (VERSION
    14, refused as `WireError::Unread`).
  - The daemon's stats JSON gives `"unread": { "cause", "code" (where given), "why" }`, so readers match cause and
    code; `why` is for showing.
  - Hex vendors that wire: its lead was given the landing SHA, and Hex's own code reads no `unread`.
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

## The R04 slice's checks

On adas-mac-studio.local (macOS 26.2 arm64, rustc 1.98.1), at main 1d4016720 with the slice applied, with
`SHARDS_HELPERS=skip` and placeholder init and server binaries:

| Command | Result |
|---|---|
| `cargo test -p shards-abi --lib guest_time` | 3 passed (each cause reads back; text not written so is refused; its words) |
| `cargo test -p shards-vmm --lib guest_time` | 11 passed, 1 ignored (a VM's) |
| `cargo test -p shards-api --lib image::wire` | 16 passed (a vCPU's typed unread round-trips; a code signed `+` is refused `Unread`) |
| `cargo test -p shards --bin shards engine::watch` | 2 passed (the JSON's typed `unread`) |
| `cargo clippy -p shards-abi -p shards-vmm -p shards-api -p shards --all-targets -- -D warnings` | clean |
| `cargo clippy -p shards-vmm --all-targets --target aarch64-unknown-linux-musl -- -D warnings` (zig CC) | clean: KVM's clock |
| `cargo xwin clippy -p shards-vmm --all-targets --target x86_64-pc-windows-msvc -- -D warnings` | clean: WHP's clock |
| `cargo xwin clippy -p shards-vmm --all-targets --target aarch64-pc-windows-msvc -- -D warnings` | clean: WHP arm64's clock |

Red, then green, then guard removal (R09, R10):
- Red: on main, a failure's code is words alone (the fixture above).
- Green: the typed round trips above.
- M1: `Unread::write` drops its code. `each_cause_reads_back_as_itself` fails.
- M2: the wire takes words it doesn't parse as `Gone`. `a_runs_microvm_reads_back_as_its_host_said_it` fails,
  `right: Err(Unread)`.

## R01-R16 accounting (review)

- R01, R05: as above.
- R03: the record's derivations stand. `STAT_LINE_MOST` is a page, seq_file's first buffer.
- R04: cured above.
- R06, R07, R13: small functions; the comments carry sources and orderings.
- R09, R10: the R04 slice's red, green and two mutations, above.
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
4. At the R04 slice's revision: the runs above, which now also show a typed `unread` where a read fails, plus
   `scripts/lint` and the Linux and Windows unit tests.
