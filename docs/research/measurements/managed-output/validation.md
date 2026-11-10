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
