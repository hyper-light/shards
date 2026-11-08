# Shipping the guest: where VM runtimes get their kernel and guest init

Research note, 2026-09-29. Evidence only; nothing here is a final design decision.

It informs the next step of phase 3, "shipping the kernel and shards-init" [shards: docs/design/architecture.md:1017], and D25's guest record. Today `shards run IMAGE` fails until the user brings both: "no guest to boot: `shards guest use --kernel FILE --init FILE`, or --kernel and --init" [shards: crates/shards/src/run.rs:49-51]. The goal is a `shards run IMAGE` that works on first use, and:

- needs no root, on every target of the 8-target matrix [shards: CLAUDE.md:3, 15];
- verifies the kernel and shards-init before they are used;
- always pairs a host with the init it was built with, since `shards-abi` has no version negotiation (§2.11);
- works offline once set up, and can be set up without a network.

Pinned sources (citation paths are relative to each repository; abbreviations as listed):

| Tag | Source |
|---|---|
| `cz` | apple/containerization 0.47.0, `bc994b88df46` |
| `ctr` | apple/container 1.5.0, `d265d669ecae` |
| `libkrunfw` | containers/libkrunfw v5.6.2, `f14ac64ca8a0` (the repository now redirects to github.com/libkrun/libkrunfw). `wf/` = `.github/workflows/` |
| `libkrun` | containers/libkrun v1.19.6, `227b2de6ed32` (now github.com/libkrun/libkrun). `lib.rs` = `src/libkrun/src/lib.rs`, `blob/` = `src/init_blob/`, `fs/` = `src/devices/src/virtio/fs/`, `builder.rs` = `src/vmm/src/builder.rs` |
| `krunvm` | containers/krunvm v0.2.7, `d51d82ae415b` |
| `muvm` | AsahiLinux/muvm muvm-0.6.0, `04d9b95f13ea`. `m/` = `crates/muvm/src/` |
| `krun-tap` | the Homebrew tap krunvm's README names (slp/homebrew-krun, now libkrun/homebrew-krun), `ec25e458347e` |
| `lima` | lima-vm/lima v2.2.0, `de0816ea4bdc`. `DL` = `pkg/downloader/downloader.go`, `GA.sh` = `pkg/cidata/cidata.TEMPLATE.d/boot.Linux/25-guestagent-base.sh`, `U26` = `templates/_images/ubuntu-26.04.yaml`, `internals.md` = `website/content/en/docs/dev/internals.md` |
| `colima` | abiosoft/colima v0.10.3, `00f6c297e92a`. `dl/` = `util/downloader/`, `limautil/` = `environment/vm/lima/limautil/` |
| `podman` | containers/podman v6.1.3, `85b994955e0b`. `M/` = `pkg/machine/`, `oci/` = `pkg/machine/ocipull/`, `D/` = `docs/source/markdown/`, `V/` = `vendor/go.podman.io/` (common v0.69.2, image/v5 v5.41.2, storage v1.64.1) |
| `kata` | kata-containers/kata-containers 4.2.0, `c7351e797eff`. `BK` = `tools/packaging/kernel/build-kernel.sh`, `KDB` = `tools/packaging/kata-deploy/local-build/kata-deploy-binaries.sh`, `OSB/` = `tools/osbuilder/`, `RT/` = `src/runtime/` (the Go shim), `RS/` = `src/runtime-rs/crates/`, `PROTO/` = `src/libs/protocols/protos/` |
| `fc` | firecracker-microvm/firecracker v1.17.0, `95f868c8e345` |
| `ch` | cloud-hypervisor/cloud-hypervisor v53.0, `9ed824d6d08d` |
| `linuxkit` | linuxkit/linuxkit v1.8.2, `43200ea63425`. `cli/` = `src/cmd/linuxkit/`; `ggcr/` = its vendored go-containerregistry v0.20.3 |
| `ddocs` | docker/docs at `de3bdf51fc36` (main, 2026-09-29). `DD/` = `content/manuals/desktop/`, rendered under https://docs.docker.com/desktop/ |
| `dd` | Docker Desktop 4.66.1 (build 222799, arm64), installed on this Mac and inspected read-only. `LK/` = `Contents/Resources/linuxkit/`. Observations of one build, not documented behavior |
| `gvisor` | google/gvisor release-20260921.0, `26f3455a4cb9`. `gb/` = `runsc/gvisorbinaries/`, `ug/` = `g3doc/user_guide/` (https://gvisor.dev/docs/user_guide/install/) |
| `gv13718` | google/gvisor issue #13718, "Make gVisor installation process support sidecar binaries", opened 2026-07-15, https://github.com/google/gvisor/issues/13718 |
| `go-digest` / `go` | opencontainers/go-digest v1.0.0 `algorithm.go` / Go 1.26.1 `src/os/file.go` |
| `cargo` | rust-lang/cargo `797e8a9bc`, the cargo of the Rust 1.98.0 that shards pins. `C/` = `src/doc/src/commands/`, `R/` = `src/doc/src/reference/` (rendered at https://doc.rust-lang.org/cargo/) |
| `rustup` | rust-lang/rustup 1.29.1: `doc/user-guide/src/overrides.md`, `src/config.rs`, `src/toolchain.rs`, `CHANGELOG.md` |
| `rustc` | rust-lang/rust 1.98.0: `src/doc/rustc/src/platform-support.md`, `src/doc/rustc/src/codegen-options/index.md` |
| `xdg` | XDG Base Directory Specification 0.8, https://specifications.freedesktop.org/basedir-spec/latest/ |
| `assets` | release pages (`/releases/expanded_assets/TAG`), GitHub's release metadata, asset sizes by HTTP `Content-Length`, registry manifests fetched anonymously (ghcr.io `apple/containerization/vminit:0.47.0`, quay.io `podman/machine-os:6.1`, Docker Hub `linuxkit/kernel:6.6.71` and `linuxkit/init`), the Firecracker CI and gVisor release bucket listings, Homebrew bottles and crates.io; all read 2026-09-29 |
| `shards` | this repository at `09a7226`; the lines cited are unchanged through `9b26624`. `PM Mn` = docs/research/platform-measurements.md; `S1`–`S4` = the four parts of PM M36 (§2.12) |

Why these versions:

- Each project's newest release tag on 2026-09-29, from `git ls-remote`. docker/docs has no releases, so its `main` of that day.
- `container` 1.5.0 depends on Containerization at exactly 0.47.0, the version read here [ctr: Package.swift:26, 58].
- Rust 1.98.0 is what `rust-toolchain.toml` pins [shards: rust-toolchain.toml:2]; `cargo --version` names `797e8a9bc`, and rustup 1.29.1 is the local rustup.

Identity checks:

- `container`'s pinned kernel digest equals the SHA-256 GitHub reports for `kata-static-3.32.0-arm64.tar.zst` [assets] (derived by comparison).
- Our downloads matched their pins: libkrunfw's prebuilt tarball and bottle against Homebrew's SHA-256s, Lima's release files against its `SHA256SUMS`, Firecracker's x86_64 tarball against its `.sha256.txt`, and gVisor's aarch64 tarball against its `.sha512` [assets].

Markers:

- **(derived)**: our arithmetic or comparison on cited facts.
- **(inference)**: reasoning from code or docs, not stated there.
- **(measured)**: our own measurement (§2.12, PM M36).
- **UNVERIFIED**: no acceptable source found.

## 1. Scope

For each runtime:

- **Q1.** Where the guest kernel comes from, and how it is built.
- **Q2.** Where the guest init or agent comes from, and how it reaches the guest.
- **Q3.** How both are verified: hash pins, signatures.
- **Q4.** Where they are cached or installed, and whether that needs root.
- **Q5.** How host and guest stay compatible across upgrades.
- **Q6.** What works offline, and how big the artifacts are.

Then:

- **Q7.** What Rust's tools allow for building shards-init into a host binary: `cargo install`, rustup, build scripts (§2.10).
- **Q8.** Implications for shards (§3), and what needs our own measurement (§4).

## 2. Findings

### 2.1 At a glance

| Runtime | Kernel | Guest init or agent | Integrity | Kept in | Host–guest coupling |
|---|---|---|---|---|---|
| Apple `container` 1.5.0 | Kata's release kernel, downloaded after a prompt | `vminitd`, an OCI image tagged with the framework's version | kernel: archive SHA-256 compiled in; init: by tag, blob digests | `~/Library/Application Support/com.apple.container`; binaries in `/usr/local` (admin) | exact pin of the framework version; no run-time check |
| libkrun 1.19.6 + libkrunfw 5.6.2 | inside a shared library, `libkrunfw.so.5` | C init built by a build script, `include_bytes!` into libkrun | none upstream; Homebrew pins SHA-256s | system library directories | init: the same file; kernel: the soname's major only |
| Lima 2.2.0 | the distribution image's own | `lima-guestagent` beside `limactl`, copied in at every boot | image SHA-256s pinned in templates; unpinned fallbacks | downloads in the user cache directory | shipped together; replaced when its SHA-256 differs |
| Colima 0.10.3 | its images' own | the runtime inside its images | SHA-512 per image compiled in | downloads in the user cache directory | new images with each release |
| Podman machine 6.1.3 | the image's own (Fedora CoreOS) | the Podman service in the image | OCI digests; signatures only with a host policy | XDG data directory, macOS too | image tag = client `major.minor`; mismatch "unsupported" |
| Kata 4.2.0 | kernel.org, `vmlinux-<version>-<config version>` | Rust `kata-agent` baked into the guest image | kernel source vs kernel.org sums; no release checksums | `/opt/kata` (root) | one tarball; versions readable, never compared |
| Firecracker v1.17.0, Cloud Hypervisor v53.0 | brought by the user | none | CI only (Cloud Hypervisor: URL + SHA-1) | the user's choice | none |
| Docker Desktop 4.66.1 | in the signed app bundle | a root image in the same bundle | the app's code signature | inside `Docker.app` | one app release |
| gVisor 20260921.0 | the Sentry, a helper binary beside `runsc` (inside it until 2026-07) | the gofer: `runsc` re-executing itself | signed apt repository, or a same-origin `.sha512` | beside `runsc` | build label enforced in release builds |

Sources are in the sections below.

### 2.2 Apple's Containerization and `container`

The closest design to shards: one lightweight VM per container on macOS, with its own init as PID 1.

**What runs in the VM**

- Each container gets its own lightweight VM. `vminitd` is its first process and serves a gRPC API over vsock, through which the host sets the VM up and starts processes [cz: README.md:25-32].
- Two backends share that contract: Virtualization.framework on macOS, and cloud-hypervisor with KVM on Linux [cz: README.md:34-46].

**Kernel: not shipped, but recommended, downloaded and pinned by digest**

- **The framework brings no kernel.**
  - It ships a kernel configuration and a containerized build [cz: README.md:69-82; kernel/README.md:1-24].
  - It "allows user provided kernels but tests functionality starting with kernel version `6.14.9`" [cz: README.md:84-86].
  - A prebuilt kernel needs VIRTIO built in. The README points at Kata's `vmlinux.container` [cz: README.md:88-92].
- **Its own downloads.**
  - The kernel build fetches linux-6.18.5 from cdn.kernel.org, checks no hash, and compiles inside a `container` VM [cz: kernel/Makefile:15, 50-64].
  - The developer target `fetch-default-kernel` downloads Kata 3.17.0's arm64 static tarball, checks no hash, and copies `vmlinux.container` out [cz: Makefile:67, 422-434].
  - The same Makefile pins its cloud-hypervisor and runc downloads by SHA-256 and deletes a mismatch [cz: Makefile:68-81, 436-466].
- **The `container` CLI's default kernel** is one file in Kata's 3.32.0 arm64 release tarball [ctr: Sources/ContainerPersistence/ContainerSystemConfig.swift:167-171]:
  - `binaryPath`: `opt/kata/share/kata-containers/vmlinux-6.18.35-197-debug`;
  - `url`: `https://github.com/kata-containers/kata-containers/releases/download/3.32.0/kata-static-3.32.0-arm64.tar.zst`;
  - `digest`: `sha256:8736c054…e4b5`, the tarball's.
  - A `url` other than the default without a `digest` fails to load [ctr: ContainerSystemConfig.swift:205-214; docs/container-system-config.md:100-108].
  - The tarball is 696,573,576 bytes [assets]. So the first install downloads about 664 MiB to keep one kernel (derived).
  - Its own test fixture avoids "downloading the real ~570MB kata-static release tarball in every test" [ctr: Sources/ContainerTestSupport/KernelFixture.swift:25-32].
- **When.**
  - `container system start` installs a kernel only if no default kernel exists.
  - It asks `Install the recommended default kernel from [<url>]? [Y/n]`, unless `--enable-kernel-install` or `--disable-kernel-install` answers [ctr: Sources/ContainerCommands/System/SystemStart.swift:53-57, 157-163, 177-202].
  - `container system kernel set --recommended` does it later [ctr: Sources/ContainerCommands/System/Kernel/KernelSet.swift:44-45, 59-70].
- **How** [ctr: Sources/Services/ContainerAPIService/Server/Kernel/KernelService.swift]:
  1. A remote archive needs an expected digest; a local one does not [ctr: KernelService.swift:119-131].
  2. The archive is downloaded into a new temporary directory [ctr: KernelService.swift:133-155].
  3. Its SHA-256 is compared with the digest. A mismatch fails with `kernel archive digest mismatch`. Only `sha256` is accepted [ctr: KernelService.swift:160-168, 184-221].
  4. The one member is extracted, following a symlink member such as `vmlinux.container` [ctr: KernelService.swift:279-302].
  5. The file is copied into `<app root>/kernels/` under its own name, and the symlink `default.kernel-<arch>` is pointed at it [ctr: KernelService.swift:38-42, 66-85, 223-251].
  6. The archive is deleted [ctr: KernelService.swift:179-181].
- **Where.** The app root is `~/Library/Application Support/com.apple.container`, or `CONTAINER_APP_ROOT` [ctr: Sources/ContainerPlugin/ApplicationRoot.swift:21-44].
- **Other routes.** `--binary PATH` installs a local kernel file. `--tar PATH|URL --binary MEMBER` installs a member of an archive, and needs `--digest` for a URL [ctr: KernelSet.swift:35-51, 77-123].
- **Upgrades.** `system start` checks only that some default kernel exists [ctr: SystemStart.swift:157-159, 217-224]. So a kernel installed by an older release stays in use after an upgrade that changes the recommended one (inference).

**Init: `vminitd`, an OCI image named by the framework's version**

- **Build.**
  - vminitd and vmexec are Swift programs, built for `<arch>-swift-linux-musl` with Swift's Static Linux SDK. The SDK download is pinned by checksum [cz: vminitd/Makefile:21-49, 88-98].
  - On a Mac they are built inside a Linux container, not cross-compiled on the host [cz: Makefile:369-384; README.md:103-115].
- **Packaging.**
  - `build-initfs.sh` stages `sbin/vminitd`, `sbin/vmexec`, `sbin/runc` only when asked, and empty directories, into an ext4 image and a tar [cz: scripts/build-initfs.sh:76-83, 109-112].
  - `make init-image` turns the tar into the single-layer OCI image `vminit:latest` [cz: Makefile:355-367; Sources/Containerization/Image/InitImage.swift:43-84].
- **Publishing.**
  - Release CI builds vminitd for aarch64 only ("vminitd (PID 1) must be an aarch64 binary") and pushes `ghcr.io/apple/containerization/vminit:${VERSION}` [cz: .github/workflows/containerization-build-template.yml:62-71, 139-148].
  - `vminit:0.47.0` is an index with one `linux/arm64/v8` manifest, whose one gzip layer is 69,253,615 bytes [assets].
- **Coupling by an exact pin.**
  - `container` depends on Containerization `exact: "0.47.0"` and compiles that version into `CZ_VERSION` [ctr: Package.swift:26, 58, 595-600].
  - Its default init image is `ghcr.io/apple/containerization/vminit:<CZ_VERSION>` [ctr: ContainerSystemConfig.swift:147-153; Sources/CVersion/Version.c:27-29].
  - So the host library and vminitd come from one release, unless the user sets `[vminit] image` [ctr: docs/container-system-config.md:125-129] (derived).
  - Nothing checks it at run time. The gRPC service has no version call (derived: no "version" in `SandboxContext.proto`). vminitd only logs its commit, tag and build time [cz: vminitd/Sources/vminitd/Application.swift:38-39, 65-74].
  - vminitd's build stamps `date -u` into the binary [cz: vminitd/Makefile:17-19; vminitd/Package.swift:23-25, 44-46], so two builds of one commit differ (inference).
- **Integrity.**
  - The init image is named by tag, not by digest (derived from the default above).
  - A pull checks each blob against its descriptor's digest [cz: Sources/Containerization/Image/ImageStore/ImageStore+Import.swift:169-176, 188-200]. What binds the tag to a manifest is the registry (inference).
- **When it is fetched.**
  - `system start` pulls it if absent, and only logs a failure [ctr: SystemStart.swift:153-155, 166-175].
  - Creating a container fetches it again if needed, pulling when it is missing [ctr: Sources/Services/ContainerAPIService/Server/Containers/ContainersService.swift:1101-1107; Sources/Services/ContainerAPIService/Client/ClientImage.swift:354-378].
- **How it boots.**
  - The image is unpacked once per manifest digest into a 512 MiB ext4 snapshot, then mounted read-only as the VM's root [ctr: Sources/Services/ContainerImagesService/Server/SnapshotStore.swift:38-51, 81-87, 195-204; ContainersService.swift:1101-1107].
  - The framework's own convenience initializer reuses an existing `initfs.ext4` whatever reference it is given [cz: Sources/Containerization/ContainerManager.swift:112-127; Sources/Containerization/Image/Unpacker/EXT4Unpacker.swift:150-156]. There, a stale init survives a change of reference (inference).

**Install and offline**

- **The installer.** The signed package (118,045,087 bytes [assets]) puts files under `/usr/local` and asks for an administrator password [ctr: README.md:22-26]. It holds the binaries, plugin configurations and scripts: no kernel and no init image [ctr: Makefile:127-158, 160-173].
- **Data compatibility.** App data is forward-compatible within a major version [ctr: README.md:109].
- **Offline** (inference):
  - a Mac that has run `system start` once needs no network to run the images it has;
  - a new one can install a kernel from a local archive or file (above), and the init image with `container image load` of a saved tar [ctr: docs/command-reference.md:634-647].

### 2.3 libkrun and libkrunfw, krunvm, muvm

**libkrunfw: the kernel as a shared library**

- "a library bundling a Linux kernel in a dynamic library". libkrun leaves "to the linker the work of mapping the sections into the process" [libkrunfw: README.md:3-5].
- **Build.**
  - Linux 6.12.109 from cdn.kernel.org [libkrunfw: Makefile:1-3], with 36 patches [libkrunfw: Makefile:5, 142-144]. Two of them: an orderly reboot when PID 1 dies, and socket impersonation over vsock (TSI) [libkrunfw: patches/0001-krunfw-Don-t-panic-when-init-dies.patch:4-10; patches/0011-Transparent-Socket-Impersonation-implementation.patch:4-19].
  - One config per architecture and variant [libkrunfw: Makefile:145-154]. Build metadata is fixed [libkrunfw: Makefile:10-14, 161, 164].
- **From kernel to library.**
  - `bin2cbundle.py` writes the kernel as a C array aligned to 64 KiB ("64k covers 4k/16k/64k Linux kernels") [libkrunfw: bin2cbundle.py:6-12].
  - The API is two functions: `krunfw_get_kernel(&load_addr, &entry_addr, &size)`, and `krunfw_get_version()`, which returns the ABI version [libkrunfw: bin2cbundle.py:104-120].
  - The file is `libkrunfw.so.5.6.2` with soname `libkrunfw.so.5`, or `libkrunfw.5.dylib` on macOS [libkrunfw: Makefile:8-9, 74-87, 203-211].
- **On macOS.** By default the kernel is built in a krunvm VM [libkrunfw: Makefile:21-27, 167-171; build_on_krunvm_fedora.sh:15-47]. Releases also carry `libkrunfw-prebuilt-aarch64.tgz` with the generated `kernel.c`, so a Mac only compiles C [libkrunfw: wf/publish-release.yml:43-56]. That `kernel.c` is 95,806,207 bytes [assets]. Homebrew builds from it [krun-tap: Formula/libkrunfw.rb:4-5, 17-20].
- **Integrity.**
  - The Makefile fetches the kernel tarball with plain `curl` and checks no hash [libkrunfw: Makefile:138-140].
  - Releases carry no checksum files or signatures [assets].
  - Homebrew pins the tarball and each bottle by SHA-256 [krun-tap: Formula/libkrunfw.rb:4-12].
- **Install.** `sudo make install`, into `/usr/local` by default [libkrunfw: README.md:22-26; Makefile:89-95, 213-222].
- **Licence.** Binary distributions must be "accompanied by the source code of the Linux kernel bundled in the binary" [libkrunfw: README.md:126-136].
- **Sizes.** The library unpacked is 21,825,208 bytes (x86_64) and 24,839,784 (aarch64). Release tarballs are 8.7–13.1 MB [assets].

**libkrun: loading the kernel, embedding the init**

- **The kernel, by soname alone.**
  - libkrun dlopens `libkrunfw.so.5`, or `libkrunfw.5.dylib`, lazily, and looks up `krunfw_get_kernel` [libkrun: lib.rs:77-85, 116-150].
  - It never calls `krunfw_get_version` (derived: grep). So any library with that name is accepted, whatever kernel it carries (inference).
  - Missing: `Couldn't find or load libkrunfw.so.5`, and `-ENOENT` [libkrun: lib.rs:2900-2913].
  - `krun_set_kernel` takes a kernel file instead [libkrun: include/libkrun.h:869-892].
  - On x86_64 the library's own pages become guest memory; on aarch64 the bytes are copied [libkrun: builder.rs:1303-1364].
- **The init, built by a build script and embedded.**
  - The `krun-init-blob` crate's build script compiles `init/init.c` statically, with `$CC_LINUX`, `$CC` or `cc`. `KRUN_INIT_BINARY_PATH` names a prebuilt init instead [libkrun: blob/build.rs:5-67].
  - The crate is one line: `pub static INIT_BINARY: &[u8] = include_bytes!(env!("KRUN_INIT_BINARY_PATH"));` [libkrun: blob/src/lib.rs:1].
  - On macOS the Makefile cross-compiles it with Apple clang, lld and a Debian bookworm sysroot that it downloads from deb.debian.org with `curl`, unchecked [libkrun: Makefile:67-69, 99-121, 159-185]. On Linux it needs static glibc [libkrun: README.md:110-116].
  - libkrun's own tests build a Rust guest for `aarch64-unknown-linux-musl`, linked through environment variables such as `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER`, not a config file [libkrun: tests/run.sh:25, 33-52, 60].
- **How the init reaches the guest.**
  - It is a virtual, one-shot file, `/init.krun`, on the guest's virtio-fs root, backed by the library's static bytes. The command line says `init=/init.krun` [libkrun: lib.rs:90-114, 2941-2942; fs/virtual_entry.rs:8-25].
  - Nothing of it is written to the host's disk (derived).
- **Coupling.** Configuration travels by kernel command line, environment and a JSON file, with no version in it [libkrun: lib.rs:217-247; blob/init/init.c:52, 1503-1566]. None is needed: the VMM and the init are one file (derived).
- **Sizes.** In Homebrew's arm64 bottle, `libkrun.1.19.6.dylib` is 5,732,416 bytes. The static glibc init inside it is 778,088 [assets].

**krunvm and muvm**

- **krunvm** ships no kernel or init. It links libkrun, which loads libkrunfw [krunvm: src/bindings.rs:6-24; src/commands/start.rs:192-197, 309]. It installs from a Homebrew tap that depends on buildah and libkrun [krunvm: README.md:26-31; krun-tap: Formula/krunvm.rb:14-19].
- **muvm** shares the host's `/` as the guest root, and runs the host's installed `muvm-guest` inside [muvm: m/bin/muvm.rs:252-258, 395, 432-458].
  - Its launch messages carry no version field [muvm: m/utils/launch.rs:7-49].
  - The design needs a Linux host of the guest's architecture (inference).

### 2.4 Lima and Colima

**Lima: kernel and images**

- **The kernel is the image's.** Lima boots a distribution's cloud image (inference). Under `vmType: vz`, the default on macOS 13.5 and later, it boots through Virtualization.framework's EFI loader unless the instance has a `kernel` file [lima: templates/default.yaml:8-12; pkg/driver/vz/vm_darwin.go:883-925].
- **Pins.**
  - Image entries have `location`, `arch` and an optional `digest` [lima: templates/default.yaml:19-37].
  - The default Ubuntu 26.04 template lists dated release URLs pinned by SHA-256, then digest-less fallbacks to the moving `release/` URL, because "release-yyyyMMdd will be removed after several months" [lima: U26:1-11, 28-37].
  - Maintainers regenerate the pins from Ubuntu's published sums [lima: hack/update-template-ubuntu.sh:112-133, 220-226]. Templates ship with Lima [lima: pkg/templatestore/templatestore.go:26-43], so each release pins its own images (derived).
  - The containerd bundle's URL and SHA-256 are compiled into `limactl` with `go:embed` [lima: pkg/limayaml/containerd.yaml:1-11; pkg/limayaml/defaults.go:61-68].

**Lima: the downloader**

- **Verified while writing.**
  - sha256, sha384 or sha512 [go-digest: algorithm.go:33-40, 51-53].
  - The body is hashed as it is written. A mismatch fails with `expected digest …, got …` [lima: DL:929-960].
  - Only a verified file is renamed into place, and its digest is stored beside it as `<algo>.digest` [lima: DL:415-419, 921-927, 962-969].
- **Cached files are not hashed again:** "the actual digest of the `data` file is not computed". The expected digest is compared with the stored one [lima: DL:127-135, 283-286].
- **Fallback past a failure.** `limactl` tries the image entries in order and keeps the first that downloads [lima: pkg/instance/start.go:118-153]. So a digest mismatch on the pinned image falls through to the unpinned one (derived).
- **Cache.**
  - `os.UserCacheDir()/lima/download/by-url-sha256/<SHA-256 of the URL>/`, holding `url`, `data`, `time`, `type` and `<algo>.digest` [lima: DL:83-93, 601-622; internals.md:136-154].
  - That is `~/Library/Caches` on macOS, `$XDG_CACHE_HOME` or `~/.cache` on other Unix [go: src/os/file.go:504-542].
  - `LIMA_HOME` is `~/.lima`, not Application Support, because macOS socket paths are limited to 104 bytes [lima: pkg/limatype/dirnames/dirnames.go:18-42].
- **Offline.**
  - A cached pinned image needs no network.
  - A cached digest-less one survives a failed HEAD request: "using cached image" [lima: DL:283-292].
  - An uncached one fails [lima: pkg/instance/start.go:119-127].

**Lima: the guest agent**

- **What ships.**
  - `lima-guestagent` is a static Go binary, built for each Linux architecture [lima: Makefile:357-426].
  - The macOS arm64 release holds `share/lima/lima-guestagent.Linux-aarch64.gz`, 7,275,764 bytes (19,660,962 unpacked), beside a 32,669,616-byte `limactl`. Agents for other guest architectures are a separate tarball [assets].
  - `limactl` finds the agent relative to its own path [lima: pkg/usrlocal/usrlocal.go:22-42, 99-157].
- **Delivered at every start.**
  - The host agent regenerates the cidata ISO, with the agent in it [lima: pkg/hostagent/hostagent.go:189-204; pkg/cidata/cidata.go:396-398, 461-486].
  - A boot script compares SHA-256s and reinstalls the agent when they differ [lima: GA.sh:20-28, 59-83].
- **Coupling.**
  - The gRPC `GuestService` has no version call [lima: pkg/guestagent/api/guestservice.proto:8-19] (derived).
  - Each instance records the Lima version that created it [lima: pkg/instance/create.go:81]. A template may demand a `minimumLimaVersion` [lima: pkg/limayaml/validate.go:40-48].
  - CI tests an upgrade from v0.15.1 [lima: hack/test-upgrade.sh:61-109].
- **Sizes.** `lima-2.2.0-Darwin-arm64.tar.gz` is 37,586,365 bytes; the default arm64 Ubuntu image 941,132,800 [assets].

**Colima**

- **Images.** Colima drives Lima with its own Ubuntu 24.04 images, one per runtime and architecture, with the container runtime inside [colima: embedded/images/images.txt:1-8; environment/vm/lima/yaml.go:24-58].
- **Pins in the binary.** Each image's URL and SHA-512 are compiled in [colima: embedded/images/images_sha.sh:17-23; embedded/embed.go:7-8; limautil/image.go:20-24, 101-139].
- **Verification.**
  - The whole file is hashed after download. A mismatch is renamed `.invalid` [colima: dl/download.go:117-145; dl/sha.go:31-71].
  - A cached file is trusted on `stat`, and handed to Lima with its digest cleared, "so lima does not re-validate" [colima: dl/download.go:89-100; limautil/image.go:26-40, 61-93].
- **Cache.** `$COLIMA_CACHE_HOME`, else `$XDG_CACHE_HOME/colima`, else `os.UserCacheDir()/colima` [colima: config/files.go:101-117].
- **Coupling.** Colima needs Lima v0.18.0 or later, checked with `limactl info` [colima: core/core.go:16, 45-83]. The Docker client is installed separately [colima: README.md:81], and no client–daemon check was found (derived by grep).
- **Sizes.** The arm64 images are 209,768,031 bytes (`none`) to 446,992,920 (`containerd`) [assets].

### 2.5 Podman machine

- **The OS image.**
  - "a custom Fedora CoreOS based image pushed to quay.io/podman/machine-os". WSL uses a custom Fedora image [podman: D/podman-machine-init.1.md.in:27-29].
  - On macOS, applehv and libkrun boot it through EFI [podman: M/applehv/stubber.go:46; M/libkrun/stubber.go:33], so the kernel is the image's (inference).
- **No agent of Podman's own.** Ignition enables `podman.socket`, and a unit that reports `Ready` over vsock [podman: M/ignition/ignition.go:166-171, 356-366; M/ignition/ready.go:18-43]. Fedora CoreOS's auto-updater is turned off [podman: M/ignition/ignition.go:172-179].
- **Tagged with the client's version.**
  - An untagged reference gets Podman's own `major.minor` (`6.1`) as its tag, so that "endpoints in containers.conf" need no change per release [podman: oci/ociartifact.go:95-112; version/rawversion/version.go:7].
  - From the index, Podman picks the disk whose `disktype` annotation, architecture and OS match [podman: oci/source.go:45-111].
- **Integrity.**
  - containers/image checks each blob's digest as it streams [podman: V/image/v5/copy/digesting_reader.go:20-62].
  - Signatures: the host's `policy.json` if there is one, else `insecureAcceptAnything` [podman: oci/pull.go:45-73]. The macOS installer does not install a policy ("Leaving for future considerations") [podman: contrib/pkginstaller/Makefile:59-61].
  - `--image` URLs and local files are taken with no checksum [podman: M/stdpull/url.go:82-125; M/stdpull/local.go:23-30].
- **Cache.**
  - `<XDG data>/containers/podman/machine/<provider>/cache`, on macOS too. Files are named by the disk manifest's digest, and a hit is a `stat`. Older files are deleted after a pull [podman: M/env/dir.go:40-106; oci/ociartifact.go:158-225].
  - Each machine gets its own decompressed copy [podman: M/shim/host.go:153-173].
- **Coupling.**
  - "A configuration where the Podman host and machine mismatch are unsupported" [podman: D/podman-machine-init.1.md.in:31-47].
  - `podman machine os upgrade` refuses a client older than the machine; otherwise it moves the machine to the client's `major.minor` [podman: M/os/ostree.go:71-150].
  - The remote client refuses a server API older than 4.0.0 [podman: pkg/bindings/connection.go:377-412].
- **Offline.** With the default image, `podman machine init` reads the registry's index before it looks at the cache [podman: oci/ociartifact.go:150-165, 227-271]. So it needs the network even when the disk is cached (derived; not run). A local `--image` needs none.
- **Sizes.** The macOS arm64 installer is 76,397,043 bytes. The `machine-os:6.1` aarch64 applehv disk is 937,980,979 bytes, zstd-compressed [assets].

### 2.6 Kata Containers

- **Kernel.**
  - versions.yaml pins a kernel.org version, `v6.18.35`, and no checksum [kata: versions.yaml:205-211].
  - Kata's configs and patches have their own counter, `kata_config_version`, 202, which any change must bump [kata: tools/packaging/kernel/kata_config_version:1; tools/packaging/kernel/README.md:95-97].
  - The installed file is `vmlinux-<version>-<config version>`, behind a stable `vmlinux.container` symlink [kata: BK:591-592, 618-622], in `/opt/kata/share/kata-containers/` [kata: KDB:23, 1319; BK:17, 576]. For 4.2.0 that is `vmlinux-6.18.35-202` (derived).
  - The source tarball is checked against kernel.org's `sha256sums.asc`, whose PGP signature is not checked [kata: BK:174-193, 209-212] (derived: no `gpg` in the script).
- **Agent.**
  - Rust, built for musl by default [kata: utils.mk:133-134], and installed as `/usr/bin/kata-agent` in the guest root [kata: OSB/rootfs-builder/rootfs.sh:785-844].
  - In the release's initrd the agent is PID 1 (`AGENT_INIT=yes`), except on s390x. Every guest image gets the agent tarball of the same build [kata: KDB:1104-1112].
- **Install.**
  - One `kata-static-<version>-<arch>.tar.zst` that "uses an /opt/kata/ prefix", installed with `sudo tar -xvf … -C /` [kata: docs/installation.md:216-269].
  - The configurations name `kernel = "@KERNELPATH@"` and `image = "@IMAGEPATH@"`, filled in at build time [kata: RT/config/configuration-qemu.toml.in:15-17; RT/Makefile:959-962].
- **Integrity.**
  - The release has no SHA256SUMS and no signature for its tarballs. The upload is a bare `gh release upload` [kata: tools/packaging/release/release.sh:167-179; assets].
  - Per-sandbox annotations can pass a SHA-512 for a custom kernel or image [kata: RT/virtcontainers/pkg/annotations/annotations.go:54-58]. The Go runtime hashes the whole file at each sandbox creation and refuses a mismatch. The default assets are not hashed [kata: RT/virtcontainers/types/asset.go:134-211; RT/virtcontainers/sandbox.go:547-570].
  - Confidential configurations bake the guest image's dm-verity root hash into the shim's configuration at build time [kata: tools/packaging/static-build/shim-v2/build.sh:40-100; KDB:819-833].
- **Coupling.**
  - ttrpc on vsock port 1024 [kata: src/libs/kata-types/src/config/default.rs:26].
  - The agent reports `agent_version` and `grpc_version`, `0.0.1` [kata: PROTO/health.proto:27-35; src/agent/Makefile:17-18, 85-86].
  - Neither runtime compares them: the Go shim only calls `Check`, and runtime-rs logs the version [kata: RT/virtcontainers/kata_agent.go:843-846; RS/runtimes/virt_container/src/health_check.rs:64-83] (derived by grep).
  - Coupling is by packaging: one tarball, whose images carry the same build's agent [kata: KDB:1111-1112; docs/installation.md:246-248]. Across majors, 1.x guest assets "will **not** work" with 2.x [kata: docs/Upgrading.md:106-121].
- **Sizes.** `kata-static-4.2.0-arm64.tar.zst` is 653,631,272 bytes; amd64 996,647,537 [assets].

### 2.7 Firecracker and Cloud Hypervisor: the user brings the guest

- **Firecracker ships binaries only.**
  - The API boots "a given kernel image, root file system, and boot arguments" [fc: README.md:121-122].
  - Releases hold static musl binaries in a tgz (7,464,385 bytes on x86_64) with a `.sha256.txt` beside it [fc: tools/release.sh:125, 162-188; tools/gh_release.py:49-63; assets]. `SHA256SUMS.sig` is left out, "since we aren't making those keys public" [fc: tools/gh_release.py:22-28].
- **Its kernels.**
  - getting-started takes the newest dated prefix of the CI bucket, and the highest kernel version in it, and downloads with `wget`, with no hash [fc: docs/getting-started.md:94-126].
  - CI kernels come from `resources/rebuild.sh`, which builds the newest matching Amazon Linux tag at run time [fc: resources/rebuild.sh:98-189] (derived: not pinned).
  - The policy: "Firecracker is tightly coupled with the guest and host kernels"; guest kernels 5.10, 6.1 and 6.18 [fc: docs/kernel-policy.md:1-15, 32-55].
  - It recommends an uncompressed `vmlinux`: "A `bzImage` is compressed and is decompressed by the guest at boot, which costs additional boot time and guest memory" [fc: docs/rootfs-and-kernel-setup.md:5-16].
  - Its CI 6.18.48 kernels are 27,872,576 bytes (x86_64) and 19,466,752 (aarch64) [assets].
- **No guest agent.** Snapshots carry a format version of their own [fc: docs/snapshotting/versioning.md:52-54].
- **Cloud Hypervisor ships binaries only**, 7,062,256 bytes for `cloud-hypervisor-static`, with no kernel, firmware or checksums [ch: .github/workflows/release.yaml:21-28, 63-95; assets].
  - Users bring a firmware, or a kernel: "any recent kernel will suffice" with the required options [ch: README.md:112-146, 196-219].
  - Its CI pins every workload by URL and SHA-1 [ch: scripts/test_assets.yaml:1-26, 58-76]. `fetch_workloads.py` verifies, deletes a mismatch, and reuses a verified file without the network [ch: scripts/fetch_workloads.py:168-218].

### 2.8 Docker Desktop and LinuxKit

**LinuxKit**

- **What it builds.** A YAML names OCI images for the kernel, init and services. "All components are downloaded at build time … The image is self-contained and immutable" [linuxkit: docs/yaml.md:9-14].
- **Kernel.**
  - The kernel image holds a `kernel` file plus a `kernel.tar` of modules [linuxkit: docs/yaml.md:54-62].
  - The default config pins `linuxkit/kernel:6.6.71` by tag [linuxkit: linuxkit.yml:1-3].
  - Kernel images are built from kernel.org sources, after verifying kernel.org's signed checksums and the tarball's signature [linuxkit: kernel/Dockerfile:50-67].
- **Init.** `linuxkit/init`, `runc` and `containerd` images are unpacked into the root [linuxkit: docs/yaml.md:74-80]. They are pinned by tags that are git tree hashes of their sources [linuxkit: linuxkit.yml:4-8; docs/packages.md:276, 316, 440].
- **Integrity.**
  - Pulls check each blob's digest (go-containerregistry's `verify.ReadCloser`) [linuxkit: ggcr/pkg/v1/remote/fetcher.go:268-276; ggcr/internal/verify/verify.go:73-82].
  - The docs promise optional Docker Content Trust [linuxkit: docs/yaml.md:15]. At v1.8.2 no code reads the `trust` section (derived by grep).
- **Cache.** `~/.linuxkit/cache`, in OCI image-layout form, one architecture per image [linuxkit: docs/image-cache.md:22-32]. A cached image means "no network activity at all" [linuxkit: cli/cache/write.go:31-41].
- **Coupling.** The pin set, fixed at build time (inference).
- **Sizes.** `linuxkit/kernel:6.6.71` for arm64 is one gzip layer of 203,706,808 bytes, which also carries the kernel's sources and headers [assets; linuxkit: kernel/Dockerfile:163-193].

**Docker Desktop**

- **LinuxKit.** Docker Desktop "uses LinuxKit to provide an embedded, invisible virtual machine" [linuxkit: ADOPTERS.md:7].
- **Docker owns the guest.** "Because Docker controls the kernel and the OS inside the VM, Docker can roll these out to all users immediately" [ddocs: DD/troubleshoot-and-support/faqs/linuxfaqs.md:23-25].
  - Release notes list the kernel as a component; 4.93.0 has Linux v7.0.14 [ddocs: DD/release-notes.md:27-38].
  - Updates are app updates, checked automatically by default [ddocs: DD/troubleshoot-and-support/faqs/releases.md:9-17].
- **VMMs.** Docker's own VMM from 4.86, libkrun before it. The page contradicts itself about which versions used libkrun [ddocs: DD/features/vmm.md:20-22, 41-44].
- **Observed in the installed 4.66.1** [dd]:
  - `LK/kernel`: 36,630,536 bytes, an uncompressed arm64 `Image`, 6.12.76-linuxkit;
  - `LK/desktop.img`: 653,074,432 bytes, EROFS;
  - `LK/libkrun.dylib`.
  - The app's Developer ID signature seals both files by SHA-256 [dd: Contents/_CodeSignature/CodeResources:683-688, 739-744] (derived).
  - So the kernel, root image and VMM change together with the signed app (inference), and booting downloads nothing (inference).

### 2.9 gVisor

- **One release, one set of binaries.** The Sentry, gVisor's application kernel, and the gofer come from runsc's release. The gofer is runsc re-executing `/proc/self/exe` [gvisor: runsc/container/container.go:1539, 1556, 1562; runsc/specutils/specutils.go:95-97].
- **Embedded until 2026-07.**
  - Everything shipped inside `runsc` [gvisor: ug/install.md:86-89].
  - Each helper was flate-compressed with `go:embed`, and extracted to a temporary file or a memfd at each use [gvisor: tools/embeddedbinary/embeddedbinary_template.go:36-37, 63-125].
  - They split it out. Embedding "makes the gVisor binary size quite large (most of the `runsc` binary size comes from embedded binary). Extraction at runtime costs CPU and I/O, and is too expensive to work when on the sandbox startup hot path" [gv13718].
  - Helpers now live in `gvisor-bin/` beside runsc [gvisor: gb/gvisorbinaries.go:70-87, 290-304]. The Sentry boots from `gvisor-bin/gvisor_sentry`; by default runsc refuses to fall back to its embedded copy [gvisor: runsc/sandbox/sandbox.go:974-982; runsc/config/flags.go:122; runsc/config/config.go:1494-1505].
- **Coupling, checked.** runsc passes its build label to every helper in `GVISOR_ENFORCE_RELEASE`. A helper whose label differs panics: "all gVisor binaries must come from the same release" [gvisor: gb/gvisorbinaries.go:25-51, 122-178]. Release builds enforce it by default [gvisor: runsc/config/flags.go:121; runsc/config/config.go:1452-1455].
- **Integrity.**
  - A signed apt repository, or a `.sha512` fetched from the same bucket and checked with `sha512sum -c` [gvisor: ug/install.md:21-35, 45-57].
  - GitHub releases carry `SHA256SUMS` and `SHA512SUMS` [gvisor: ug/install.md:196-202].
  - A checksum from the same origin catches corruption, not a compromised origin (inference).
- **Sizes.** `gvisor.tar.zstd` is 129,280,755 bytes (x86_64) and 120,312,665 (aarch64). For aarch64, `runsc` is 102,620,704 bytes and `gvisor_sentry` 49,180,874 [assets].

### 2.10 Building a guest binary into a Rust host binary

**What `cargo install` reads** [cargo: C/cargo-install.md]

- **Where it installs.** `--root`, `CARGO_INSTALL_ROOT`, `install.root`, `CARGO_HOME`, then `$HOME/.cargo`, into its `bin/` [cargo: C/cargo-install.md:20-26].
- **Where it builds.** crates.io and `--git` sources build in a temporary target directory [cargo: C/cargo-install.md:55-59].
- **The lockfile** is ignored unless `--locked` [cargo: C/cargo-install.md:61-75].
- **Configuration.** "This command operates on system or user level, not project level. This means that the local configuration discovery is ignored. Instead, the configuration discovery begins at `$CARGO_HOME/config.toml`. If the package is installed with `--path $PATH`, the local configuration will be used, beginning discovery at `$PATH/.cargo/config.toml`." [cargo: C/cargo-install.md:77-83].
  - Discovery otherwise walks up from the current directory [cargo: R/config.md:7-23].
- **Workspace-root settings.** Profiles and `[patch]` are read only from the workspace root's manifest [cargo: R/profiles.md:21-23; R/overriding-dependencies.md:294-296]. shards' `guest` profile and its `[patch.crates-io]` for AWS-LC live there [shards: Cargo.toml:57-60, 69-74]. So a package published alone would lose both (inference).
- **No post-install step** (derived: none among its options). On macOS, `shards-vm` is then left without the hypervisor entitlement, and fails with `HV_DENIED` [shards: CLAUDE.md:37] (inference).

**What rustup reads**

- **Which toolchain.** `+toolchain`, then `RUSTUP_TOOLCHAIN`, a directory override, a `rust-toolchain.toml` found by walking up from the current directory, then the default [rustup: doc/user-guide/src/overrides.md:3-20]. So the file is found from where cargo runs, not from the package being built (derived).
- **Targets.** The file's `targets` are platforms to install [rustup: overrides.md:167-173]. When the file's toolchain is active, rustup installs it with them, or adds the ones an installed toolchain lacks [rustup: src/config.rs:792-836, 838-893]. That needs auto-install, the default since 1.28.1; `RUSTUP_AUTO_INSTALL=0` turns it off [rustup: CHANGELOG.md:458-465; src/config.rs:555-579].
- **Nested builds.** rustup sets `RUSTUP_TOOLCHAIN` for the tool it runs [rustup: src/toolchain.rs:167-183]. A cargo started by a build script therefore inherits the outer build's toolchain, which outranks `rust-toolchain.toml` (derived).

**Build scripts and cross-target artifacts**

- **Outputs.** Build scripts write into `OUT_DIR` and "should not modify any files outside of that directory" [cargo: R/build-scripts.md:83-96]. They can fail the build with a message, `cargo::error=MESSAGE` (Rust 1.84 and later) [cargo: R/build-scripts.md:345-356].
- **Artifact dependencies**, cargo's own way to depend on another package's binary built for another target, are `-Z bindeps` [cargo: R/unstable.md:994-1070]. Unstable features "are only available on the nightly channel" [cargo: R/unstable.md:3-7]. shards pins stable 1.98.0 [shards: rust-toolchain.toml:2].
- **Linking musl without a C toolchain.**
  - `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl` are tier 2 with host tools, on musl 1.2.5 [rustc: platform-support.md:84-93, 113].
  - `-C link-self-contained` uses "libraries and objects shipped with Rust" [rustc: codegen-options/index.md:247-257].
  - shards links them with `rust-lld`, named in `.cargo/config.toml` [shards: .cargo/config.toml:21-27].

**What happens (measured, S4 [PM M36])**

A workspace laid out like shards' was built and installed six ways on a Mac whose default toolchain (1.94.1) has no musl targets. Its host package's build script builds a guest for musl and embeds it with `include_bytes!`.

- Inside the checkout, `cargo build` and `cargo install --path host` both applied the toolchain file and `.cargo/config.toml`, and the nested musl build succeeded (A, B).
- From elsewhere, `cargo install --path <repo>/host` applied the config but not the toolchain file (C). `cargo install --git` applied neither (D). Both nested builds failed with `error[E0463]: can't find crate for std`.
- In both, the build script saw `RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin`. So the nested build used the outer toolchain, as the rustup facts above predict.
- With `RUSTUP_TOOLCHAIN=1.98.0`, the `--git` install succeeded (E). Its nested `cargo build` runs in the checkout and reads the config there. Forcing the musl linker to `cc` failed it (F): Apple's clang cannot link a Linux ELF.

### 2.11 shards today

- **Boot.** A run boots `--kernel` and `--init` (or `SHARDS_KERNEL` and `SHARDS_INIT`), or the recorded guest, and fails with neither [shards: crates/shards/src/run.rs:45-53].
- **The guest store.**
  - `shards guest use` copies both files into `$SHARDS_HOME/guest` as `sha256-<hex>`, hashing while it copies, and renames only complete files into place [shards: crates/shards/src/guest.rs:145-181].
  - It records the pair in `current`, written to a temporary file, synced and renamed [shards: guest.rs:127-143].
- **Templates** are named by the SHA-256 of the snapshot format, the kernel's and init's digests, the root filesystem, the CPUs, the memory and the command line [shards: crates/shards/src/run.rs:100-118; docs/design/architecture.md:584-592].
- **The home** is per-user: `SHARDS_HOME`, or `shards` in `~/Library/Application Support`, `$XDG_DATA_HOME` or `~/.local/share`, or `%LOCALAPPDATA%` [shards: crates/ipc/src/lib.rs:11-33].
- **Tests**:
  - pin the kernel by URL and SHA-256, from the release `kernel-6.18.48-1bff175d35cb` [shards: crates/shards/tests/common/mod.rs:33-38, 71-88];
  - cache it in `target/artifacts`, verifying a download before renaming it [shards: tests/common/mod.rs:104-124];
  - build shards-init with cargo for `<ARCH>-unknown-linux-musl`, profile `guest`, target directory `target/guest`, through the rustup proxy with `DYLD_*` removed [shards: tests/common/mod.rs:136-162].
- **The kernel build.**
  - Its source tarball is pinned by the SHA-256 in kernel.org's signed `sha256sums.asc` [shards: scripts/build-kernel.sh:12-15, 36-38].
  - It is reproducible: two CI runs built byte-identical x86_64 kernels [shards: resources/kernel/README.md:31-34].
  - CI publishes a prerelease named by a hash of the inputs, with `SHA256SUMS` [shards: .github/workflows/kernel.yml:58-71].
- **The contract.**
  - `shards-abi` is compiled into both sides: "Anything both sides must agree on lives here, so the two can never drift apart" [shards: crates/abi/src/lib.rs:1-2].
  - It holds the control page's layout and markers [shards: crates/abi/src/lib.rs:10-52] and the run protocol's frames and `Spec` [shards: crates/abi/src/run.rs:11-46, 60-140].
  - It has no version field. Its one compatibility provision: a spec without a terminal ends early, "so an init from before terminals still reads the spec" [shards: crates/abi/src/run.rs:101-104].
- **How init reaches the guest.** At boot the VMM reads the init file and wraps it in an in-memory initramfs, `/init` plus `/dev/console` [shards: crates/vmm/src/vm/aarch64.rs:291-297; crates/vmm/src/initramfs.rs:80-89]. A restore reads neither kernel nor init: it resumes a template (D25), and `vm restore` takes neither [shards: CLAUDE.md:34] (derived).
- **The toolchain.** `rust-toolchain.toml` pins 1.98.0 with both musl targets [shards: rust-toolchain.toml:1-6]. CI's platform jobs install those targets explicitly [shards: .github/workflows/ci.yml:51-56]. The guest's architecture is the host's [shards: CLAUDE.md:40].
- **Builds are told apart** by the file identities of `shardsd` and the `shards-vm` beside it [shards: crates/ipc/src/unix.rs:125-139].

### 2.12 Our measurements

PM M36 records the method and numbers, from this Mac (Apple M5 Max, macOS 26.4.1, Rust 1.98.0), one build each. Its harness for S4 is `docs/research/measurements/cargo-install-guest/`.

- **S1. The kernel.** 18,883,072 bytes for aarch64 and 27,708,976 for x86_64. The aarch64 kernel compresses to 8,350,871 bytes with gzip -9, 7,022,164 with zstd -19, and 6,104,196 with xz -9e.
- **S2. shards-init.** 428,912 bytes for aarch64 and 481,792 for x86_64, each built in 3.2–3.7 s. Two target directories gave the same bytes.
- **S3. The host binaries.** `shardsd` is 5,547,896 bytes, `shards-vm` 957,032 and `shards` 598,904.
- **S4. What `cargo install` honors** (§2.10).

## 3. Implications for shards (ranked)

Ranked by how much each decides whether `shards run IMAGE` works on first use, correctly and safely. The designs marked (inference) are options, not decisions.

1. **shards-init ships inside `shardsd`, built by a build script, and the daemon keeps it in the guest store.**
   - *Why embed:*
     - the host and init share `shards-abi` with no version field (§2.11), so a mismatched pair is caught nowhere (derived);
     - every runtime here with a private host–guest protocol takes both halves from one release:
       - libkrun in one library file [libkrun: blob/src/lib.rs:1];
       - Lima by delivering its own agent at each boot [lima: GA.sh:20-28];
       - Kata in one tarball [kata: KDB:1111-1112];
       - Docker Desktop in one signed bundle [dd];
       - gVisor beside its binary, with a label check [gvisor: gb/gvisorbinaries.go:152-178];
       - Apple downloads its init separately, but names it by the framework's exact version [ctr: ContainerSystemConfig.swift:147-153].
     - None negotiates versions at run time, and only gVisor checks (derived from §2.2–2.9).
     - Embedding makes the pair exact by construction, with no network, no second artifact and no two-stage release (inference).
   - *Against downloading a prebuilt init:*
     - It would spare the build its nested musl build, and `cargo install --git` would then work without the musl target (S4 D) (inference).
     - But the first run would need the network, and each upgrade would fetch again, as Apple's does: `system start` and container creation pull the version-named init when it is missing [ctr: SystemStart.swift:153-155; ContainersService.swift:1101-1107].
     - An exact pin needs the init published for every host build before the host is built (inference).
     - Development builds still need a local init. Apple's falls back to a locally built `vminit:latest` when no version is compiled in [ctr: ContainerSystemConfig.swift:147-153; Sources/CVersion/include/Version.h:17-19].
   - *How* (inference, after libkrun's `krun-init-blob` [libkrun: blob/build.rs:5-67]):
     - a build script in `crates/shards` runs the test helper's command [shards: crates/shards/tests/common/mod.rs:136-162] for `<CARGO_CFG_TARGET_ARCH>-unknown-linux-musl`, with a target directory of its own;
     - it names the linker itself (`CARGO_TARGET_<TRIPLE>_LINKER=rust-lld`), rather than rely on config discovery, which the outer build of `cargo install --git` skips (S4 D–F);
     - an environment variable can name a prebuilt init instead, as `KRUN_INIT_BINARY_PATH` does [libkrun: blob/build.rs:54-67], so CI builds shards-init once per architecture;
     - when the musl standard library is missing (S4 C–D), it fails with `cargo::error=` naming `rustup target add <triple>` [cargo: R/build-scripts.md:345-356], which keeps the build script panic-free;
     - only `shardsd` holds the bytes. The client must stay thin [shards: docs/design/architecture.md:60; PM M23], and VM processes got a binary of their own because `shardsd`'s extra code cost each VM [PM M34]. VMs need a file, which the daemon can give them (inference);
     - the daemon writes the bytes into `$SHARDS_HOME/guest/sha256-<hex>` once, as `keep` does [shards: crates/shards/src/guest.rs:145-181]. Boots pass that path to `shards-vm`, as today (§2.11).
   - *gVisor's main reason for leaving does not apply.* gVisor stopped embedding for size, and because extraction "is too expensive to work when on the sandbox startup hot path" [gv13718]. shards would extract once per build, not per start, and restores read no init at all (§2.11) (inference).
   - *Cost* (S2, S3): 428,912 bytes on this Mac's 5,547,896-byte `shardsd`, 7.7% (derived); the x86_64 init is 481,792 bytes. A clean guest build took 3.2–3.7 s.
2. **The kernel: pinned in `shardsd` by URL, size and SHA-256, downloaded on first need, verified before it is kept.**
   - *The shape* is the tests' `Artifact { name, url, sha256 }` [shards: crates/shards/tests/common/mod.rs:33-38, 71-88], moved into `shardsd` per architecture.
     - Apple's `container` 1.5.0 pins its kernel the same way [ctr: ContainerSystemConfig.swift:167-171].
     - Lima and Colima compile their download pins into their binaries [lima: pkg/limayaml/defaults.go:61-68; colima: embedded/embed.go:7-8].
   - *Fetch the bare kernel:* 18,883,072 bytes (aarch64) or 27,708,976 (x86_64) (S1). Apple's route downloads 696,573,576 bytes to keep one kernel (§2.2).
   - *Verify while streaming, rename only on a match, and fail closed.*
     - Lima and Apple verify before the rename [lima: DL:921-969; ctr: KernelService.swift:160-168]. `keep` already hashes as it copies [shards: guest.rs:145-181].
     - Lima's fallback from a pinned image to an unpinned URL is the counter-example [lima: pkg/instance/start.go:118-153; U26:28-37].
   - *After that, trust the content name.* Lima, Colima and Podman never hash a cached file again [lima: DL:127-135; colima: dl/download.go:89-100; podman: oci/ociartifact.go:158-190]. Kata hashes a custom asset at every sandbox creation [kata: RT/virtcontainers/types/asset.go:134-165]. Hashing 19–28 MB costs a boot something (E2); restores do not read the kernel (§2.11).
   - *Why not embed the kernel:*
     - raw, the aarch64 kernel is 3.4 times this Mac's `shardsd`, and the x86_64 one is 1.5 times larger again; compressed, it is still 6.1–8.4 MB, plus a decompressor (S1, S3) (derived);
     - it changes with `resources/kernel`, not with every host build [shards: .github/workflows/kernel.yml:6-12];
     - Docker Desktop, which ships the kernel inside the app, ships an app of hundreds of MB [dd];
     - gVisor moved its kernel out of `runsc`, for size among other reasons [gv13718].
   - *Fetch it beside the pull.* The first `shards run` of an image already pulls and boots (D24, D25). Fetching the kernel while the image downloads would cost only the slower of the two (inference; E1).
   - *Its source:* shards' own reproducible release assets, with `SHA256SUMS` [shards: resources/kernel/README.md:20-34; .github/workflows/kernel.yml:58-71]. They are prereleases today [shards: .github/workflows/kernel.yml:70]. A URL compiled into shipped binaries needs a release that stays (inference).
   - *A mirror* would be a configured URL with the digest still pinned; Apple refuses a custom URL without a digest [ctr: ContainerSystemConfig.swift:205-214] (inference).
3. **A default guest: with nothing recorded, runs boot the pinned kernel and the embedded init, and `shards guest use` becomes an override.**
   - Today a run with no record fails [shards: crates/shards/src/run.rs:45-53].
   - Templates are named by the kernel's and init's digests [shards: crates/shards/src/run.rs:100-118]. An upgrade that changes either names new templates, and never restores an old one against a new init (derived).
   - An upgrade that leaves init's bytes unchanged keeps its templates, if init builds are reproducible (S2; E5).
   - `shards guest` could say where the guest in use comes from: default, recorded or given (inference).
   - Overrides elsewhere: Apple's `kernel set --binary` and `--tar` [ctr: KernelSet.swift:35-51], Kata's per-sandbox asset annotations [kata: RT/virtcontainers/pkg/annotations/annotations.go:54-58], and libkrun's `krun_set_kernel` [libkrun: include/libkrun.h:869-892].
4. **Offline: nothing to fetch after first use, and a documented way to set up without a network.**
   - With the init embedded and the kernel in the store, runs need no network (derived).
   - A first run offline should fail before it boots, naming the URL, the digest and `shards guest use --kernel FILE --init FILE` (inference). Apple (`--tar PATH`, `--binary PATH`) and Lima (local files) offer the same [ctr: KernelSet.swift:77-123; lima: DL:224-234].
   - Check the store before the network. Podman asks the registry even when its disk is cached [podman: oci/ociartifact.go:150-165]; Lima and Colima start from cache [lima: DL:283-289; colima: environment/vm/lima/disk.go:174-178].
   - A prefetch command, say `shards guest pull`, would let a machine be prepared ahead (inference).
5. **Rootless paths: both files stay in `$SHARDS_HOME/guest`.**
   - The home is per-user [shards: crates/ipc/src/lib.rs:11-33], so nothing needs root (derived).
   - Others need root:
     - Apple's installer asks for an administrator password [ctr: README.md:22-26];
     - libkrunfw installs with `sudo make install` [libkrunfw: README.md:22-26];
     - Kata extracts into `/opt/kata` with `sudo` [kata: docs/installation.md:239-243].
   - *Data, not cache.*
     - Lima and Colima download into the user's cache directory [lima: DL:83-93; colima: config/files.go:101-117], which XDG defines as "non-essential data files" [xdg].
     - shards' templates depend on the kernel by digest. Keeping it beside them in the data directory spares a cache cleaner from breaking them (inference).
6. **An identity check for guests that are not the default one.**
   - With the embedded init the pair is exact. With `shards guest use --init` or `SHARDS_INIT` it is whatever the user gave (derived).
   - gVisor sends its build label, and each helper refuses a mismatch in release builds [gvisor: gb/gvisorbinaries.go:25-51, 152-178]. Podman compares versions when it upgrades a machine [podman: M/os/ostree.go:71-83]. Apple, libkrun, Lima and Kata check no version at run time (§2.2–2.6).
   - For shards, an ABI number in `shards-abi`, reported by init before `SPEC` or read from the control page, would turn a misparsed frame into a clear error (inference).
7. **Installing from source: `cargo install --locked --path crates/shards`, run inside the checkout.**
   - Measured (S4):
     - inside the checkout, both the toolchain file, with its musl targets, and `.cargo/config.toml` apply;
     - with `--path` from elsewhere, the config applies but the toolchain file does not;
     - with `--git`, neither applies to the outer build, and without the musl standard library the nested build fails.
   - `--locked`, because the lockfile is otherwise ignored [cargo: C/cargo-install.md:61-75].
   - Publishing to crates.io would need the `guest` profile and AWS-LC's `[patch]` inside the package, since both are read only from the workspace root [cargo: R/profiles.md:21-23; R/overriding-dependencies.md:294-296] (inference).
   - On macOS, `cargo install` leaves `shards-vm` without the hypervisor entitlement [shards: CLAUDE.md:37] (inference). A from-source install needs a signing step like `scripts/hvf-run`'s [shards: scripts/hvf-run:1-7].
8. **Release artifacts: signed per-target binaries with the init inside, checksums beside them, and a durable kernel release.**
   - What others publish:
     - Lima: `SHA256SUMS` and build-provenance attestations [lima: .github/workflows/release.yml:96-101, 127-130];
     - Firecracker: a `.sha256.txt` per tarball [fc: tools/gh_release.py:49-63];
     - Podman: a notarized installer [podman: contrib/pkginstaller/Makefile:69-73];
     - Docker: an app signature that seals the kernel and root image [dd];
     - Kata: no checksums [kata: tools/packaging/release/release.sh:167-179].
   - A checksum from the file's own origin catches corruption, not a compromised origin (inference). A digest compiled into a signed binary covers both (inference).
9. **Reproducibility makes the pins checkable.**
   - The kernel builds reproducibly [shards: resources/kernel/README.md:31-34].
   - shards-init gave the same bytes from two target directories on one Mac (S2). Across machines and checkout paths: E5.
   - By contrast, Apple's vminitd stamps its build time [cz: vminitd/Makefile:17-19], while libkrunfw fixes its build metadata [libkrunfw: Makefile:10-14].
10. **The kernel's licence travels with it.**
    - A binary kernel carries the obligation to provide its source. libkrunfw says so for its bundled kernel [libkrunfw: README.md:126-136].
    - shards' kernel notes already name the source tarball [shards: resources/kernel/README.md:36-45].
    - A download from shards' releases distributes the kernel just as embedding would (inference).

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| Working on first use vs working offline | The kernel has to arrive once. Apple, Lima, Colima and Podman all download at first setup (§2.2–2.5) |
| Exact host–guest coupling vs small VM processes | An init inside `shards-vm` would grow the binary every VM process maps, which M34 trimmed [PM M34]; so it goes in `shardsd` and reaches VMs as a file |
| `cargo install` convenience vs a guest built at build time | `--git` and out-of-tree `--path` builds use the user's default toolchain, which may lack the musl target (S4 C–D) |
| Embedding the kernel vs binary size and rebuilds | An 18.9–27.7 MB kernel against a 5.5 MB `shardsd` (S1, S3) |
| Rootless vs a system-wide signed install | Apple's installer needs an administrator [ctr: README.md:22-26]; shards' home is per-user [shards: crates/ipc/src/lib.rs:11-33] |
| Verifying on every use vs boot latency | Kata hashes custom assets at each sandbox creation [kata: RT/virtcontainers/types/asset.go:134-165]; Lima, Colima and Podman trust content names (§2.4–2.5) |

## 4. Open questions needing our own measurement

- **E1. The first run's latency with a kernel download.**
  - `shards run alpine true` on an empty `SHARDS_HOME`, end to end.
  - Split it into daemon start, image pull, kernel download and verification, boot and template save.
  - Fetch the kernel serially and in parallel with the pull.
  - macOS/HVF and Linux/KVM; n, p50, p90, p99 and max, with host, OS, revision and the network described.
- **E2. The cost of verifying.** SHA-256 of the kernel (18.9 and 27.7 MB) and of init (0.43–0.48 MB): at each boot, at each daemon start, or only on entry to the store. p50 and p99 on each host, to decide when to hash again.
- **E3. `shardsd` with the init inside.**
  - Size and `.rodata` growth, daemon start time, and RSS.
  - On macOS, the first-launch assessment of the larger signed binary [shards: crates/shards/tests/common/mod.rs:176-181].
- **E4. Build cost.**
  - Clean and incremental builds of `shardsd` with the nested init build.
  - `scripts/lint` over the 8 targets.
  - Whether one shared guest target directory, as `target/guest` is for tests, avoids building the init once per host target.
  - Whether a build under `RUSTUP_AUTO_INSTALL=0` with the targets missing fails as cleanly as S4 C.
- **E5. Reproducibility of shards-init across machines.** Two hosts, different checkout paths, and both architectures as build hosts. Same SHA-256 or not; if not, what differs (paths, `--remap-path-prefix`).
- **E6. Offline behavior.** The first run with no network and an empty store: error text, exit status, time to fail. Also the kernel stored but the registry unreachable.
- **E7. A compressed kernel asset.** Download time saved against decompression time, with S1's sizes: gzip 8.4 MB, zstd 7.0 MB and xz 6.1 MB for 18.9 MB.
- **E8. Upgrades.**
  - After `shardsd` and `shards-vm` are replaced, confirm that no warm VM or template of the old init serves the new daemon.
  - Measure the first run of each image after the upgrade, which boots.

## 5. References

**Source code** (paths and lines cited inline)

- apple/containerization 0.47.0 (`bc994b88df46`) and apple/container 1.5.0 (`d265d669ecae`).
- containers/libkrunfw v5.6.2, containers/libkrun v1.19.6, containers/krunvm v0.2.7, AsahiLinux/muvm muvm-0.6.0, and the Homebrew tap libkrun/homebrew-krun at `ec25e458347e`.
- lima-vm/lima v2.2.0, abiosoft/colima v0.10.3, containers/podman v6.1.3 (with its vendored common, image and storage), opencontainers/go-digest v1.0.0.
- kata-containers/kata-containers 4.2.0, firecracker-microvm/firecracker v1.17.0, cloud-hypervisor/cloud-hypervisor v53.0.
- linuxkit/linuxkit v1.8.2 (with its vendored go-containerregistry v0.20.3), google/gvisor release-20260921.0.
- rust-lang/cargo `797e8a9bc`, rust-lang/rustup 1.29.1, rust-lang/rust 1.98.0 (the rustc book).
- Go 1.26.1 `src/os/file.go`, from the local toolchain.

**Official documentation and specifications**

- Docker Desktop's documentation, from docker/docs at `de3bdf51fc36` (https://docs.docker.com/desktop/).
- gVisor's installation guide (https://gvisor.dev/docs/user_guide/install/), from its repository.
- The Cargo Book and the rustup book, from their repositories (https://doc.rust-lang.org/cargo/, https://rust-lang.github.io/rustup/).
- XDG Base Directory Specification 0.8.

**Release artifacts and manifests** [assets], read 2026-09-29: the release asset lists and sizes above; the ghcr.io, quay.io and Docker Hub manifests named in the tag table; Homebrew's libkrun and libkrunfw bottles; gVisor's release bucket; Firecracker's CI bucket listing. The installed Docker Desktop 4.66.1 bundle [dd].

**Issues:** google/gvisor #13718 [gv13718].

**Our notes:** D24, D25, D26 (architecture.md); PM M23, M34, M36 (platform-measurements.md), whose S1–S4 are summarized in §2.12.
