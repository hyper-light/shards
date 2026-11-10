# Scoped light check, kiln r5

Base main6bf5b15d; Fedora x86_64, Linux6.19.10-300.fc44;
rustc1.99.1 (58cf8f9c3), LLVM23.1.1.

Exact tested source SHA-256:

```
c5c11283a4e8e1875364143ffc5e111758d14318b1d23ebde2f684a6e23acdcd  crates/remote/src/managed.rs
71cad56ff86043a0f6d38062b090e7f1118f53af5c22133354d51467492bda5d  crates/remote/src/net.rs
4643140b1b52f09b06f633f2f6985a7d9c05c0cc811bae5a4b6071338b0fc02f  crates/remote/src/tls.rs
```

```
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
cargo clippy -j4 -p shards-remote --all-targets -- -D warnings: exit0
```

Remote logs are ~/managed-output-units.log, ~/managed-output-clippy.log and
~/managed-output-light-source.log on runner8fadc1ec213332e16cb0d915a3fb913f.
No real-VM, benchmark, heavy adversarial, mutation or other-target pass is claimed.

## Scoped daemon and typed-outcome checkpoint

Final scoped run: Fedora kiln r24, native x86_64 Linux6.19.10, rustc1.99.1,
`SHARDS_HELPERS=skip`, `-j4`, lane-owned
`CARGO_TARGET_DIR=$HOME/managed-output-endpoint-target` (6.6GiB at completion).
No local Mac compilation or VM runs in this lane. Policy/diff/rustfmt checks pass.

- `cargo test --locked -j4 -p shards-remote --lib`:19 passed.
- `cargo test --locked -j4 -p shards --bin shards daemon::managed::tests`:5 passed.
- `cargo test --locked -j4 -p shards --bin shards agentfile::tests`:3 passed,
  including the file-backed EROFS active-member and symlink-resolution case.
- `cargo clippy --locked -j4 -p shards-remote --lib -- -D warnings`:exit0.
- `cargo clippy --locked -j4 -p shards --bin shards -- -D warnings`:exit0.
- New `climb::tests::a_byte_quantum_climbs_on_consumed_full_data_inside_its_ceiling`
  passed in isolated run r19 on the unchanged Quantum source below. It was not a
  live throughput benchmark. The earlier registry all-target clippy is an earlier
  source gate; final all-target/seven-platform checks remain owed.

Exact locally matched SHA-256 source hashes from
`~/managed-output-typed-source.log`:

```text
eeb45ad4e8a7fd2fcaaff293bede87b2adc99e37cd5fa9f3d036da2d7f6d852f  crates/remote/src/managed.rs
e3cf92dbff95d0302b96199d170ade037361585bf425a766a226ce09d8f0c678  crates/remote/src/net.rs
4643140b1b52f09b06f633f2f6985a7d9c05c0cc811bae5a4b6071338b0fc02f  crates/remote/src/tls.rs
524751057d4466b382c5fca303e2487f1c6898d2a3532b19722fc6e21ae8a44e  crates/shards/src/daemon/managed.rs
24394f3a1b8cc10a9ac957c3a2f24904e82c3e1f0d2277ee23182af950548365  crates/shards/src/agentfile.rs
f226adb9e9dd06e783edea88f9aad85f3ee8630347928ac66798aa0220716f69  crates/registry/src/climb.rs
```

Retained Fedora logs: `~/managed-{output,daemon,plan}-typed-{units,clippy}.log`
where applicable, `~/managed-daemon-typed-check.log`,
`~/managed-quantum-final-units.log`, and the source log above. Run r21 initially
found six unnecessary-lazy-evaluation lints introduced by typed enum constructors;
r24 uses direct `ok_or` and passes. Shared-target run r17 had a check/unit artifact
inconsistency (`RunId` compiled in check but missing in unit dependency); no final
pass is based on that shared cache. r19's pre-typed lint was stopped when the typed
slice superseded it. All lane jobs have ended; no server or worker remains running.

This validates scoped component behavior, not mixed real-VM fairness, managed ALPN
native delivery, hard parent/kernel attribution, callback/disconnect races,
revocation, root crypto/route audits, chaos/mutations, full platforms or4–5× results.
The suspected submit publication/wake race remains the coordinator's live
reproduction task; this lane did not patch it on inference.
