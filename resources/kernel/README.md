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
- **Our changes:** `shards.config`, merged on top, then the architecture's own
  `shards-<arch>.config` where there is one (`shards-aarch64.config`: a 1,000 Hz tick, PM
  M66); no option is named by both. Each line says why. The build fails if
  any of them does not survive `make olddefconfig`.
- **The `kvm` variant:** `kvm.config`, merged last, for a guest that is itself a KVM host,
  on arm64 booted at EL2 (`run --kernel --nested`, D132), and for a guest that takes a PCI
  function it was given through VFIO, its interrupts among it, as the tests of passing a
  device through do (D154): `Image-<version>-aarch64-kvm` and `vmlinux-<version>-x86_64-kvm`.
  The kernel every other guest boots stays without KVM's 800 KiB of text, which saves or
  spends no boot or restore time (PM M192).
- **The `hyperv` variant:** `hyperv.config`, merged last, for the guests of a Windows host,
  which runs them in Windows Hypervisor Platform partitions (D140):
  `vmlinux-<version>-x86_64-hyperv`. Their partitions offer Hyper-V's clocks, and Linux
  keeps time by them only with CONFIG_HYPERV (PM M197, M198); a KVM or HVF guest would
  carry its code and use none of it.
- **The `gpu` variant:** `gpu.config`, merged last, for a guest given a GPU (D144): DRM's
  virtio-gpu driver without modesetting, Vulkan through Venus on its render node,
  `Image-<version>-aarch64-gpu`. Every other guest's kernel stays without DRM: a GPU costs
  nothing where it is not used.
- **Patches:** `patches/*.patch`, applied in order with no fuzz. Each says what it fixes
  and why:
  - `0001`: an arm64 guest's first exec from a DAX-mapped file oopsed in `fs/dax.c`.
  - `0002`: an exec's process event named the process by its PIDs alone, so a listener
    raced the process for its command line in /proc, and a short-lived one had often
    ended by then (PM M176). The event now carries the command line the exec laid out,
    for the record of a run's processes (D123).
  - `0003`: a host blob of a GPU is mapped into its guest in the host's pages, 16 KiB on
    Apple silicon, so its place in the device's region must be whole host pages: Linux
    7.2's VIRTIO_GPU_F_BLOB_ALIGNMENT, backported, with a host blob's size rounded up to
    the alignment where upstream refuses it, so that userspace that aligns nothing gets
    whole pages too (D144).
  - `0004`: a guest given memory as it needs it (virtio-mem) reported only what was
    plugged so far, and programs sized themselves by it; `vm.pluggable_kbytes`, which init
    sets, adds what may still be plugged to sysinfo(2), /proc/meminfo and the overcommit
    checks.
  - `0005`: a guest saved within two seconds of registering its page reporting device
    carried the first pass into every copy restored from it; `page_reporting.flush`
    reports what waits at once, as init writes it before a save.
  - `0006`: memory a page reporting pass holds is free memory: counted so in sysinfo(2)
    and /proc/meminfo, and waited for by an allocation that would otherwise fail.
  - `0007`: FUSE dropped a DAX file's entries at each open and as its attributes showed
    its size changed, before unmapping the processes that mapped them, and since 6.15's
    counting of DAX pages by their mappings a 2 MiB entry still mapped split with its
    counts wrong: processes that faulted there got SIGBUS (PM M274). A DAX file's entries
    are left to its layouts' own breaking.
  - `0008`: a DAX window's page structures, a 64th of the window, came out of the guest's
    memory as it probed the device (PM M269), which a guest given its memory as it needs
    it boots with little of. Where the device gives a region of its own right before the
    window (shared memory region 0xfe), they go there, as a pmem namespace keeps its own
    (D189, PM M300).
  - `0009`: a store through a DAX window into a page the host's page cache does not hold
    waits for the host to read the page in, where a FUSE_WRITE of the whole page reads
    nothing (PM M283, M299). Where the device keeps a record of which of the window's pages
    its host holds (shared memory region 0xfd), a write of whole pages sends the host's
    whole pages it does not hold (FUSE_INIT's map_alignment) as FUSE_WRITEs, mapping or
    upgrading each range it meets first so that the record is taken as the write comes
    (D189, PM M301).
  - `0010`: arm64 maps memory it adds in pages where rodata=full or KFENCE asks, so that a
    page's permissions in the linear map can change alone; a file system DAX device's
    memory (a pmem namespace, a virtio-fs DAX window) is never given out by the page
    allocator, nor its permissions changed, and its page tables took a 512th of it from
    the guest's memory (PM M300). It is mapped in blocks (D189).
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
