# Implementer validation checkpoint

Policy/readback and the per-rule account are in README.md. Base revision is
`6bf5b15da904931b984ecd25662a8b8c571268bf`; final source identity is its signed
commit plus the file hash recorded by light-check.sh on the next run.

Completed locally on macOS arm64 with Rust1.98.1:

- `rustfmt --edition 2024 --check crates/testguest/src/managed_network.rs crates/testguest/src/main.rs`: exit0.
- `sh -n docs/research/measurements/managed-network/light-check.sh`: exit0.
- `python3 scripts/check-development-policy`: exit0.
- `git diff --check`: exit0.

Initial lightweight Linux native check, kiln r1 on runner
`8fadc1ec213332e16cb0d915a3fb913f` (Fedora), Rust1.98.1:

- Standalone module compile with libc0.2.191: success.
- Three then-existing parser/hex/config tests:3passed/0failed/0ignored.
- Clippy: exit101. `chunks_exact(2)` triggered chunks_exact_to_as_chunks;
  wrapper lacked the repository's clippy.toml, so test unwraps also failed.
  No socket or VM ran. This was not the final source or pinned-libc gate.
- Corrected source uses as_chunks, wrapper copies the actual test lint policy,
  and libc now pins the Cargo.lock version (0.2.189 here). Added zero/overflow
  deadline tests. Borrowed config-path parsing avoids another allocation.
- Corrected rerun was refused by kiln: runner had0idle cores, below the requested
  4CPU/4GiB/1GiB. No resources were lowered to evade the guard. The corrected
  five-test/clippy check is **owed**, not passed.

Root still owes the corrected light check, full testguest Linux gnu/musl builds
on both architectures, supported seven-target lint and every live VM/network,
interception/mutation, failure, leak, fairness and cost result in README.md.
No kernel has been built, no interception installed, no real-domain socket
experiment run and no production mechanism selected by this implementer.

The r1 temporary generated crate/cache was removed by its own exit trap; source
and this evidence remain. No jobs are left running in this lane.
