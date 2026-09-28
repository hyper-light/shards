# Guest kernel

shards builds its own guest kernel. Firecracker's CI kernel lacks EROFS, which images need
(docs/research/image-storage.md R1-R2), and our own build is where boot-time tuning will go.

## Inputs

- **Source:** Linux 6.18.48 from kernel.org, pinned in `scripts/build-kernel.sh` by the
  SHA-256 in kernel.org's `sha256sums.asc`. That file's signature was verified on
  2026-09-28 against the Kernel.org checksum autosigner key
  (B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1, from kernel.org's pgpkeys repository).
- **Base configuration:** `firecracker-x86_64-6.18.config` and
  `firecracker-aarch64-6.18.config`, Firecracker's microVM CI configs at commit
  6f82ac4cf331, unmodified. Crypto self-tests stay on (`CONFIG_CRYPTO_SELFTESTS=y`).
- **Our changes:** `shards.config`, merged on top. Each line says why. The build fails if
  any of them does not survive `make olddefconfig`.

## Building

CI builds on each architecture's native runner and publishes a prerelease named
`kernel-<version>-<hash of the inputs>` with the kernel (`vmlinux-*` ELF for x86_64,
`Image-*` for aarch64), its final `.config`, the toolchain versions and `SHA256SUMS`
(.github/workflows/kernel.yml). Tests pin one release asset by URL and SHA-256.

To build locally on Linux:

```sh
scripts/build-kernel.sh "$(uname -m)" out
```

Build metadata (timestamp, user, host, version) is fixed and nothing in the configuration
generates keys, so the same inputs and toolchain produce the same bytes. Two independent CI
runs (releases kernel-6.18.48-0c3e7e3279fd and kernel-6.18.48-1bff175d35cb, whose inputs
differ only for aarch64) built byte-identical x86_64 kernels: SHA-256 136a182b…c6e181.

## Licences

The two `firecracker-*.config` files come from Firecracker, which is licensed under the
Apache License 2.0. Its NOTICE reads:

> Firecracker
> Copyright 2017-2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
> SPDX-License-Identifier: Apache-2.0

Linux is licensed under GPL-2.0; its source is the kernel.org tarball named above.
