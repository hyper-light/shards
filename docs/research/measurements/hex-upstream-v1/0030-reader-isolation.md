# Hex patch 0030: the reader tests that count /proc/self/fd run each alone

**Defect.** Three `taken::linux` tests in `docs/research/measurements/managed-proc-main/reader-tests.rs`
(included by `crates/api/src/taken.rs`) assert that a call leaves `/proc/self/fd`'s count unchanged.
That count is the whole test process's, and libtest runs the crate's other tests on other threads,
which open and close descriptors of their own meanwhile: `cargo test -p shards-api` fails on the
integration base (82083f27, and 9f3c91da, which does not touch these files).

**Fix (Hex's 0030, adapted).** Each of the three becomes a test that runs its body, an ignored
`*_alone` test, by itself in a child test process (`current_exe --exact … --ignored
--test-threads=1`) and asserts it passed. Hex moved the fixture into the crate
(`crates/api/src/taken/reader_tests.rs`); here it stays at its evidence path, which
`managed-proc-main/run-outer-oom.sh` hashes and touches, and changes in place. Hex's other half of
0030, restoring `Refusal::Process`'s `source()`, undoes Hex's 0011, which this port does not
carry as Hex had it (see the source-chain record).

**Method.** `reader-isolation.sh reader-0030 30` on kiln-a094a12d (8fadc1ec, x86_64 Fedora,
rustc 1.98.1), 2026-11-11, base `9f3c91da` (the committed fixture), MemoryMax=24G, -j4. It builds
shards-api's lib test binary three ways and runs each 30 times, the whole lib at libtest's default
threads:

| Build | Fixture | Runs failed | `held_find_…` | `public_attach_…` | `wrong_start_…` |
|---|---|---|---|---|---|
| red | as committed (sha256 7e95254b…) | 30 of 30 | 22 | 23 | 30 |
| green | each test alone in a child (085bbd9c…) | 10 of 30, none of these | 0 | 0 | 0 |
| mutation | green, each wrapper calling its body in-process (70ce7eb1…) | 30 of 30 | 23 | 24 | 30 |

Red's assertions, run 1: `held_find_…` 53 vs 47 descriptors, `public_attach_…` 53 vs 46,
`wrong_start_…` 47 vs 48 (`reader-tests.rs:115`, `:158`, `:237`).

Every green failure (10 of 30) was `client::tests::attach_takes_back_a_guest_whose_handle_was_lost`,
the race Hex's 0031 is about (its record: `0031-take-back-race.md`). The mutation's runs also failed
once in `client::borrowed_tests::contract::zero_patience_attempts_one_request_and_does_not_wait_for_accept_or_reply`
(`managed-borrowed-nowait/contract.rs:40`: the refused zero-patience socket's end still open,
`WouldBlock` where its EOF was expected): a third flake, no Hex patch's, reported to ROOT, not
changed here.

**Raw.** On the runner: `~/g121-tmp/hex-upstream-v1/archive/reader-0030-raw.tgz` (sha256
e09cc921eea65019d1764286a9f60419e2b06d7997a90475d47bb0e4f8ddc2fe: identity, the three fixtures,
the failure lists, one log of each kind) and `reader-0030-full.tgz` (8b399f7cd7f0422c7564add445a82cae035c59748c24693cb10286ebabe60eb0:
every run's log and the three test binaries).
