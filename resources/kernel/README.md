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
- **Patches:** `patches/*.patch`, applied in order with no fuzz. Each says what it fixes
  and why:
  - `0001`: an arm64 guest's first exec from a DAX-mapped file oopsed in `fs/dax.c`.
- **The toolchain:** `builder.env` pins a Debian image by its multi-architecture index
  digest, and installs its compiler, linker and tools from Debian's snapshot archive at a
  fixed time. So every build of the same inputs gets the same toolchain, and a new
  toolchain is a new input.

## Building

CI builds each architecture twice, on separate native runners, in the pinned builder
(`scripts/build-kernel-in-builder.sh`). It publishes a prerelease named
`kernel-<version>-<hash of every input above>` only if both builds made the same bytes.
The release holds:

- the kernel: a `vmlinux-*` ELF for x86_64, an `Image-*` for aarch64;
- its final `.config`;
- the toolchain's exact versions;
- `SHA256SUMS`.

A tag published before must hold the bytes its inputs build now, or the workflow fails
(.github/workflows/kernel.yml). Tests pin one release asset by URL and SHA-256.

To build locally, with Docker on a Linux host of the kernel's architecture:

```sh
scripts/build-kernel-in-builder.sh "$(uname -m)" out
```

`scripts/build-kernel.sh "$(uname -m)" out` builds with the host's own toolchain instead.

Build metadata (timestamp, user, host, version) is fixed, and nothing in the configuration
generates keys. So the same inputs and toolchain produce the same bytes, which CI checks on
every build. Before the builder was pinned, two independent CI runs built byte-identical
x86_64 kernels (releases kernel-6.18.48-0c3e7e3279fd and kernel-6.18.48-1bff175d35cb,
whose inputs differ only for aarch64): SHA-256 136a182b…c6e181. The first release built in
the pinned builder, kernel-6.18.48-296d2de54137, passed the check on both architectures.

## Licences

The two `firecracker-*.config` files come from Firecracker, which is licensed under the
Apache License 2.0. Its NOTICE reads:

> Firecracker
> Copyright 2017-2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
> SPDX-License-Identifier: Apache-2.0

Linux is licensed under GPL-2.0; its source is the kernel.org tarball named above.
