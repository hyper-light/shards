# Vendored crates

shards builds these crates from the copies here, not from crates.io. The workspace
`Cargo.toml` points `[patch.crates-io]` at them. They are the TLS provider for registry
pulls: AWS-LC, the one C dependency (docs/design/architecture.md D19).

Each directory is the crate exactly as crates.io publishes it. Nothing here is edited.

- `scripts/vendor-crate NAME VERSION` unpacks a crate once its `.crate` matches the
  checksum in crates.io's index.
- `scripts/vendor-crate --check`, run by CI, verifies that every directory still matches
  its published `.crate`, file for file.

| Crate | Version | `.crate` SHA-256 | Source |
|---|---|---|---|
| aws-lc-sys | 0.45.0 | `9bff6c3b54fad79a2e60b8102caf565819711497c1f5f092f49508e2f5c31b27` | [aws/aws-lc-rs](https://github.com/aws/aws-lc-rs) `7943223c99d909bc399bdf1b856821bb04f1f3c5`, `aws-lc-sys/` |
| aws-lc-rs | 1.18.1 | `b281d307588d634de920874890732659e2e7672f72b5e10e81badc1a8a83621e` | [aws/aws-lc-rs](https://github.com/aws/aws-lc-rs) `22e629d5c46276497a24ee3e575be4315940e7cb`, `aws-lc-rs/` |

The source commits come from each crate's `.cargo_vcs_info.json`. The licenses are in
each directory: ISC, Apache-2.0, MIT and BSD-3-Clause parts, as each `Cargo.toml` lists.

## Built from source only

aws-lc-sys ships prebuilt NASM objects for Windows x64 (`builder/prebuilt-nasm/`), and
uses them when NASM is missing. `.cargo/config.toml` sets `AWS_LC_SYS_PREBUILT_NASM=0`,
so they are never used: Windows x64 builds assemble AWS-LC with NASM, and fail without
it. CI installs the official NASM 3.02 build, pinned by hash.

## Updating

1. Run `scripts/vendor-crate NAME VERSION` for each crate, then `cargo update -p NAME`.
2. Update the table above and D19.
3. Lint every target (`scripts/lint`) and run the tests.
