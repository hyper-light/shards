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

## Seeded by the OS

`.cargo/config.toml` sets `AWS_LC_SYS_NO_JITTER_ENTROPY=1`. AWS-LC then seeds its DRBG
from the OS CSPRNG (`CRYPTO_sysrand`: `CCRandomGenerateBytes`, getrandom or `urandom`,
`BCryptGenRandom`), as BoringSSL and ring do. RDRAND or RNDR supply its personalization
string where the CPU has one (`crypto/fipsmodule/rand/entropy/entropy_sources.c`). AWS-LC
picks this configuration itself on Linux when `/dev/sysgenid` says a VM snapshot can
clone the process (`crypto/ube/vm_ube_detect.c`).

Its default seeds from CPU jitter entropy, which exists for FIPS's two-source rule; ours
is the non-FIPS build. That default costs every new process 17 ms before its first random
bytes, against 7 µs from the OS (platform-measurements.md M19).

## Updating

Recheck these upstream items with each update:
- Whether jitter entropy stops being the default, which AWS-LC's maintainers have
  suggested.
- aws/aws-lc-rs#1241: TLS 1.3 AES-GCM sealing from several input slices (open).
- aws/aws-lc-rs#1165: `Clone` for `aead::LessSafeKey`, which ring has. It waits on an
  AWS-LC context copy (open).
- aws/aws-lc-rs#1192: reading back an ML-DSA key pair's seed (open). It matters once
  shards stores ML-DSA keys.
- aws/aws-lc-rs#738: an HPKE API (RFC 9180; open since 2025-03). It matters for
  Encrypted Client Hello, which rustls can also do with its own HPKE.
- aws/aws-lc-rs#617: JWE, and AES-192-GCM, which it lacks (open since 2024-11). Registry
  tokens are signed (JWS) and opaque to us, so it matters only if shards issues its own.
- aws/aws-lc-rs#935: GCC 15's `-Werror=unterminated-string-initialization` breaks
  aws-lc-fips-sys, whose validated sources cannot take the fix.
  - Our non-FIPS copy has the fix (aws/aws-lc 78c2583). CI's Alpine jobs compile it with
    GCC 15.2, with no such warning.
  - It matters again only if we build FIPS.

Then:

1. Run `scripts/vendor-crate NAME VERSION` for each crate, then `cargo update -p NAME`.
2. Update the table above and D19.
3. Lint every target (`scripts/lint`) and run the tests.
