# Image formats, rootfs delivery, and the in-VM image store

Research input for shards, dated 2026-09-28. Pinned sources:

- Linux v7.2-rc4 tree at `/Users/adalundhe/Projects/linux` (1590cf03). Linux paths below are relative to that tree.
- Firecracker edb60617, libkrun 1f5dd028, go-microvm 7e148d85.
- OCI image-spec ca68a05f, erofs-utils v1.9.4 (f36cadb5), composefs ec2573a0, squashfs-tools db038ef2 (4.7.6 docs).
- BuildKit 3bcbbc94, containerd 04f9be90, NVIDIA open-gpu-kernel-modules 61dcc937, VIRTIO 1.3 CSD01.

Citation conventions:

- `[Paper §x, Fig. y]` cites a paper section, figure or table.
- `[repo:path:lines]` cites source or documentation lines.
- `[calc]` marks arithmetic on cited inputs.
- **UNVERIFIED** marks a claim with no acceptable source.

## 1. Scope

- **Q1.** What container startup needs from images: sizes, layer counts, redundancy, bytes read at start, and lazy-pull results.
- **Q2.** How EROFS, squashfs, composefs, dm-verity and fs-verity compare on performance, memory, integrity, determinism, and rootless generation from OCI tars without extraction.
- **Q3.** Delivery into VMs (virtio-blk, virtio-fs ± DAX, virtio-pmem + DAX): double caching, cross-VM sharing, arm64/HVF alignment.
- **Q4.** How the rootless in-VM engine stores images: overlayfs in user namespaces, idmapped mounts, and mapping OCI whiteouts to overlay.
- **Q5.** Which OCI image-spec details affect correctness.
- **Q6.** Recommendations (§3), with measurement gaps (§4).

## 2. Findings

### 2.1 What startup needs from images (Q1)

**Most pulled bytes are never read.**
- HelloBench (57 images): pulling takes 76% of start time, yet only 6.4% of the pulled data is read. That is 27 MB on average, against an average uncompressed image 15× larger [Harter16 Abstract, §4.1 Figs. 5–6, §4.2 Figs. 8–9].
- Alibaba Function Compute (712,295 cold starts): more than 50% and 60% of cold starts, in the two regions studied, spend at least 80% and 72% of startup pulling images [FaaSNet21 §2.2, Fig. 3b].
- Three popular containers touch less than 1% of their files (1–39% of bytes) at startup [Starlight22 §3].

**Image sizes and layer counts.**
- Docker Hub, all latest public images as of May 2017 (47 TB compressed, 167 TB uncompressed): median image 94 MB uncompressed/17 MB compressed, p90 1.3 GB/0.48 GB; half of images have <8 layers, p90 <18, max 120; about half of layers are under 4 MB; median layer compression ratio 2.6 [Zhao19 Abstract, §I, §IV Figs. 3, 9, 10].
- IBM registry traces (75 days, 38 M requests): 65% of layers are under 1 MB and 80% under 10 MB [Anwar18 §1, §4.6 obs. 2, Fig. 6a].

**Layer depth costs lookups and copy-ups.**
- Over half of HelloBench bytes sit at layer depth 9 or more, with a maximum depth of 28 [Harter16 §4.3 Fig. 12].
- AUFS open latency rises with depth. Copy-up cost scales with file size: appending one byte to a 1 GB lower file took more than 20 s [Harter16 §4.3 Fig. 11].

**Redundancy is high, but layer sharing does not capture it.**
- About 3% of files are unique. In 90% of images, more than 99.4% of files also appear in other images [Zhao19 Abstract, §V Fig. 26b].
- Deduplicating decompressed layers yields 2.1× (jdupes) to 4× (VDO). Compressed tarballs dedupe at 1× [DupHunter20 §3.2 Table 1].
- Global 4 KB block dedup (2.8×) beats per-image gzip (2.3–2.7×) [Harter16 §4.1 Fig. 7].
- Layer reuse captures only 3% of the duplication across 21 popular images. Minor version updates inflate transfers 1.23–10.54× [Starlight22 §3.2, Table 1].
- AWS Lambda deterministically flattens each image into one ext4 filesystem, split into 512 KiB content-addressed chunks [Brooker23 §2]. About 80% of new uploads contain zero unique chunks; the rest average 4.3% unique (median 2.5%). This reduces storage by up to 23× [Brooker23 §3, Fig. 5].

**Reuse.** On the second run of an image, 99% of reads could be served from the first run's cache [Harter16 §4.4 Fig. 14].

**Lazy loading.**
- Slacker: 5× faster deployment cycle and 20× faster development cycle. Its run phase is 17% slower [Harter16 Abstract, §6.1].
- DADI (per-layer block diffs under ext4, lz4 seekable "ZFile"): cold-started 10,000 containers on 1,000 hosts in 4 s; warm starts 15–25% faster than overlayfs on NVMe; trace-based prefetch closes 95% of the cold–warm gap; batch cold start flat at 0.7 s [DADI20 Abstract, §5.2 Figs. 14–17].
- Starlight is 3.0× faster than the prior state of the art. Per-file lazy pulling (eStargz) degrades with round-trip time and, for postgres at ≥150 ms, is slower than plain containerd [Starlight22 Abstract, §3 Fig. 1].
- Lambda: a median 67% of chunks come from the worker cache and 32% from the AZ cache (550 µs vs 36 ms median from S3) [Brooker23 §4, Fig. 7]. Chunks stay uncompressed for random-access latency and to avoid a compression side channel [§3.2]. Its FUSE-file→virtio-blk path caused scheduling jitter and is being replaced by userfaultfd+mmap [§5.2].

**Consequence.** None of the reviewed sources reports a cold pull completing in milliseconds. A ≤5 ms start therefore assumes the image is already resident on the host and already converted.

### 2.2 Read-only image filesystems (Q2)

**Table 1. Read-only formats compared.**

| | EROFS | squashfs | composefs | dm-verity | fs-verity |
|---|---|---|---|---|---|
| Model | 32 B or 64 B inodes; block-aligned data; optional per-inode LZ4/LZMA/DEFLATE/zstd; chunk dedup; external blob devices; block-backed, or file-backed since v6.12 [erofs.rst:47-89; commit ce63cb62] | Compressed; 128 KiB default block, 1 MiB max [squashfs.rst:9-12] | EROFS metadata tree plus overlayfs `redirect`/`metacopy` xattrs pointing into a content-addressed store [composefs README.md:35-49] | Block-level Merkle tree [verity.rst:177-193] | Per-file Merkle tree; ext4, f2fs and btrfs only [fsverity.rst:13-27] |
| Read path | 16 MB random read issues 26 MB of I/O [EROFS19 Table 1] | 16 MB random read issues 165 MB (128 KiB blocks); a 16 MB stride read issues 204 MB [EROFS19 §2.2, Table 1] | Extra overlay indirection; no peer-reviewed data | Hashes each block on first read into the page cache; `check_at_most_once` is optional [verity.rst:142-152, 186-190] | Verifies on every page-in [fsverity.rst:57-59] |
| Memory / DAX | Decompression memory +4.9% over ext4, against +39.6–61.6% for squashfs [EROFS19 §5.3 Fig. 10]. FSDAX works on uncompressed inodes [erofs.rst:85-86]. Page-cache sharing across identical inodes is experimental and excludes DAX [fs/erofs/Kconfig:189-195; fs/erofs/super.c:613-616] | No DAX [dax.rst:26] | Backing files shared in the page cache [README.md:70-76] | Cannot do DAX: only the linear, stripe, log-writes and writecache targets implement `direct_access` [drivers/md/] | — |
| Integrity | None native | None native | fs-verity digest per backing file, plus an image digest [README.md:78-95; overlayfs.rst:481-526] | Root hash; can be set up at boot with `dm-mod.create=` [dm-init.rst:1-40] | Per-file digest; the doc says "dm-verity should still be used on read-only filesystems" [fsverity.rst:61-64] |
| Determinism | `-T`, `--mkfs-time`, `-U` [mkfs.erofs.1:160-175, 345-347]; directory entries sorted by format [erofs.rst:250-254] | Reproducible by default; honours `SOURCE_DATE_EPOCH` [USAGE-SQFSTAR.md:248, 494-500] | Reproducible [README.md:135-138] | — | — |
| Built from OCI tar without root | `mkfs.erofs --tar=f\|i\|headerball`, plus `--aufs` [mkfs.erofs.1:193-195, 423-437] | `sqfstar` reads tar, including PAX headers and SCHILY/LIBARCHIVE xattrs [USAGE-SQFSTAR.md:21-31]; no whiteout handling in its source (grep) | `mkcomposefs` needs no privileges [mkcomposefs.md:110-116] | Hashing runs in userspace | Enabled by a Linux filesystem ioctl, so Linux hosts only |
| Mountable in guest without init-namespace CAP_SYS_ADMIN | No [fs/erofs/super.c:902] | No [fs/squashfs/super.c:690] | No (its EROFS metadata mount needs privilege) | No | — |

**1. Evidence gap.** [EROFS19] evaluates only compressed Android partitions: boot time −5.0% on low-end and −2.3% on high-end phones versus ext4 (§5.7). No peer-reviewed evaluation of uncompressed EROFS, DAX, or container workloads was found.

**2. Block size.**
- EROFS rejects blocks larger than PAGE_SIZE [super.c:272-275].
- It silently disables DAX unless the block size equals PAGE_SIZE [super.c:678-681; dax.rst:22-23].
- `mkfs.erofs` defaults to the host's page size [mkfs.erofs.1:35-38]. On the dev host that is 16 KiB (`pagesize` = 16384, measured), which 4 KiB-page guests cannot mount.

**3. Tar-index mode.** It keeps the original tar bytes, so a guest can recompute the DiffID. It needs 512-byte blocks [containerd erofs.md:330-356], which rules out DAX [super.c:678-681].

**4. Opaque-directory trap under `userxattr`.** `mkfs.erofs --aufs` writes whiteouts as char 0/0 and marks opaque directories with `trusted.overlay.opaque` [erofs-utils lib/tar.c:1151-1161; lib/xattr.c:66-76, 597]. An overlay mounted with `userxattr` reads only `user.overlay.*` [fs/overlayfs/util.c:872-886; overlayfs.h:188-191]. The opaque markers are ignored, so deleted lower-layer contents reappear.

**5. Extracting onto a macOS host is not faithful.** APFS is case-insensitive by default on macOS and normalization-insensitive in both variants [Apple APFS FAQ]; `chown` to other users and `mknod` need the super-user [MacOSX.sdk man2/chown.2:81, mknod.2:77]. go-microvm extracts to a host directory, keeps ownership in `user.containers.override_stat` xattrs and skips device nodes and FIFOs [go-microvm docs/MACOS.md:78-87, image/pull.go:729-732; libkrun src/devices/src/virtio/fs/macos/passthrough.rs:37, 1071-1082]. A streaming tar→image writer avoids all three problems, because names, owners and devices exist only as image metadata.

**6. ext4 is also possible.** `mke2fs -d` accepts a tarball since e2fsprogs 1.47.1 [e2fsprogs RelNotes v1.47.1:11-13; mke2fs.8.in:182-186]. Lambda, however, needed a modified serial ext4 writer to get deterministic output [Brooker23 §2].

### 2.3 Delivering images into VMs (Q3)

**Table 2. Delivery devices compared.**

| | virtio-blk (read-only) | virtio-fs | virtio-fs with DAX window | virtio-pmem with EROFS `dax=always` |
|---|---|---|---|---|
| Guest page cache | Yes; duplicates the host page cache [RunD22 §3.1] | Yes | Bypassed [RunD22 §3.1; VIRTIO §5.11.6.4] | None; host page cache only [VIRTIO §5.19; dax.rst:10-15] |
| Cross-VM sharing | Host cache only; each guest's copy is private | Host cache | Host pages mapped into guests | Every VM maps the same host file pages. Firecracker example: two 128 MB VMs sharing a 100 MB pmem use at most 356 MB [fc docs/pmem.md:297-305] |
| Guest metadata RAM | — | — | `struct page` for the window only [fs/fuse/virtio_fs.c:1110-1132] | `struct page` for the **whole region**, allocated from guest RAM [drivers/nvdimm/pmem.c:525-531; fs/Kconfig:70-71] |
| Per-access cost | One virtqueue round trip per miss | One FUSE request | One FUSE_SETUPMAPPING per 2 MiB [fs/fuse/dax.c:20-22]; the window can run out [VIRTIO §5.11.6.4] | First touch takes a stage-2 fault [fc docs/pmem.md:265-277] |
| Host attack surface | Block I/O only; Firecracker and Lambda chose it for security [Agache20 §3.1; Brooker23 §2] | Host filesystem server; a larger attack surface than block [DADI20 §2.3] | Same as virtio-fs | File mmap plus a flush request |
| macOS host | Works | The server must emulate Linux semantics on APFS (§2.2 point 5) | libkrun maps `MAP_SHARED` file windows with `hv_vm_map` [passthrough.rs:2701-2780; src/hvf/src/lib.rs:312-331] | Same `hv_vm_map` mechanism; needs page-aligned (16 KiB) address and size [Hypervisor.framework hv_vm.h:44-53] |
| Guest mount without privilege | No (EROFS) | No; `virtiofs` lacks `FS_USERNS_MOUNT` [virtio_fs.c:1782-1788] | No | No |
| Writes | Best of the three in RunD's tests [RunD22 §3.1 Fig. 4] | Poor, and each container needs a daemon that burns CPU [RunD22 §3.1] | — | A flush triggers `msync` over the whole file [fc docs/pmem.md:160-165] |

**Double caching and clones.**
- Per-container block devices defeat host page-cache sharing even for identical base layers. DADI proposes a shared block pool plus DAX [DADI20 §6]; Slacker patched the loop driver with clone bitmaps so reads of unmodified blocks go to the base [Harter16 §5.4 Fig. 18].
- RunD serves the read-only layer over virtio-fs and a reflinked, volatile writable image over virtio-blk, with overlay in the guest. Rootfs preparation drops from 207 ms to 0.2 ms at 200 concurrent starts [RunD22 §4.2 Fig. 8].

**DAX has a guest memory cost that scales with image size [calc].**
- fsdax needs a `struct page` for every page of the region. Raw virtio-pmem has no altmap, so they come from guest RAM, at 56–96 B each [include/linux/mm.h:157-159]. At 64 B that is 16 MiB per GiB of mapped image with 4 KiB guest pages, or 4 MiB/GiB with 16 KiB pages.
- DAX therefore saves guest memory over virtio-blk only when the guest touches more than 1.56% (4 KiB pages) or 0.39% (16 KiB pages) of the image. Slacker's average read fraction at startup is 6.4% [Harter16 §4.1].
- Probe initialises every struct page one by one [mm/mm_init.c:1104-1149], so boot time grows linearly with region size (E1). Firecracker's own measurement (method unstated): a 128 MB VM booted from pmem has ~120 MB RSS without DAX and ~96 MB with DAX, "similar to virtio-block" [fc docs/pmem.md:293-295].

**Alignment on arm64 (and x86).**
- **(a) 2 MiB is Linux's rule, not the spec's.** Device memory is hot-added in 2 MiB subsections, and `check_pfn_span` rejects an unaligned start or size [mm/memory_hotplug.c:319-331; include/linux/mmzone.h:1968-1969; mm/memremap.c:20-32]. The subsection size does not depend on the page size. Firecracker pads regions to 2 MiB with private anonymous memory, and writes to that padding are lost [fc src/vmm/src/devices/virtio/pmem/device.rs:197-245, 296-298; docs/pmem.md:106-115]. The VIRTIO pmem section itself states no alignment [VIRTIO §5.19].
- **(b) HVF.** `hv_vm_map` takes host-page-aligned (16 KiB) ranges [hv_vm.h:44-53]. 2 MiB alignment satisfies this.
- **(c) EROFS block size must equal the guest PAGE_SIZE** (§2.2 point 2).
- **(d) PMD-size DAX mappings** need `FS_DAX_PMD` (THP) [fs/Kconfig:90-95]. An arm64 PMD is 2 MiB, 32 MiB or 512 MiB for 4K, 16K or 64K pages [arch/arm64/include/asm/pgtable-hwdef.h:10-13, 47, 55; calc]. EROFS only guarantees block alignment of data [erofs.rst:24-25, 164].
- **(e) arm64 DAX is allowed.** DAX is refused only on CPUs with aliasing data caches, and only 32-bit ARM selects that option [drivers/dax/super.c:555-561; arch/arm/Kconfig:10; include/linux/cacheinfo.h:157-158]; that is the case dax.rst:288-289 warns about. arm64 needs `ARM64_PMEM` [arch/arm64/Kconfig:1922-1932; fc docs/pmem.md:43-44].

**Sharing pages across tenants is a side channel.** Flush+Reload recovered 96.7% of GnuPG key bits across VMs [Yarom14 Abstract], and the attack class works on ARM without privileges [Lipp16 Abstract]. VIRTIO recommends per-device backing memory unless workload risk is low, and forbids trim/discard eviction of shared regions [VIRTIO §5.19.8–§5.19.9]; it calls the virtio-fs DAX window "a likely target" [VIRTIO §5.11.6.5]. Firecracker advises against sharing pmem backing files across VMs [fc docs/pmem.md:152-158].

**GPU interaction.** The kernel refuses `FOLL_LONGTERM` pins on fsdax mappings with EOPNOTSUPP [mm/gup.c:1213-1214], and NVIDIA's open driver requests `FOLL_LONGTERM` when locking user pages on x86 [nvidia kernel-open/nvidia/os-mlock.c:216-253]. DMA-registered buffers must therefore not be mmaps of DAX files. Which CUDA APIs take this path is **UNVERIFIED**.

**Pre-faulting.** `KVM_PRE_FAULT_MEMORY` is enabled only on x86 in v7.2-rc4 [Documentation/virt/kvm/api.rst:6473-6524; arch/x86/kvm/Kconfig:48]. Hypervisor.framework exposes only map, unmap and protect [hv_vm.h:44-72].

### 2.4 The in-VM store under rootless constraints (Q4)

**Unprivileged overlay mounts exist.**
- Overlay can be mounted in a user namespace since v5.11 (commit 459c7c56 "ovl: unprivieged mounts"; `FS_USERNS_MOUNT` [fs/overlayfs/super.c:1576]). BuildKit's rootless overlay snapshotter requires kernel 5.11 or later [buildkit docs/rootless.md:6-8].
- An unprivileged process can create 0/0 whiteout devices since v5.8 (commit a3c751a5; [fs/namei.c:5109-5117]).

**`userxattr` changes the semantics.**
- It moves all overlay xattrs to `user.overlay.*` [overlayfs.rst:864-869; util.c:872-886].
- It forces `redirect_dir=nofollow` and `metacopy=off`, and explicit requests for either fail [fs/overlayfs/params.c:988-1007]. Consequences: `chown`/`chmod` of a lower file copies its full data [overlayfs.rst:382-391]; renaming a lower directory returns EXDEV [overlayfs.rst:201-208]; copy-up breaks hardlinks without `index` [overlayfs.rst:626-631].
- Data-only lower layers (the composefs model) are still allowed with `userxattr` [overlayfs.rst:446-451]. Without it, verity and data-only layers need init-namespace CAP_SYS_ADMIN [params.c:1014-1031].

**Which filesystems a user namespace can mount.** Not EROFS, squashfs or virtiofs (above). Of the filesystems usable for image storage, only overlay, FUSE, tmpfs and ramfs are `FS_USERNS_MOUNT` [super.c:1576; fs/fuse/inode.c:2004; mm/shmem.c:5278; fs/ramfs/inode.c:322]. tmpfs supports `user.*` xattrs, so it can serve as a `userxattr` upper [mm/shmem.c:4287-4298]. The kernel docs expect superblock creation to stay with privileged users in the initial namespace, with idmapped mounts covering the rest [idmappings.rst:629-633].

**File ownership.**
- Owners not mapped into the user namespace show up as the overflow ID, and capabilities do not apply to such inodes [idmappings.rst:80-81; kernel/capability.c:464-481]. A root-owned 0600 file in an unshifted layer is therefore unreadable by container root through overlay's stashed-credential check [overlayfs.rst:309-335].
- Idmapped mounts fix this, but need CAP_SYS_ADMIN in the superblock's user namespace and a detached mount from `open_tree(OPEN_TREE_CLONE)` [fs/namespace.c:4799-4838; mount_setattr(2), Linux 5.12]. EROFS, squashfs and tmpfs all support them [fs/erofs/super.c:902; fs/squashfs/super.c:690; mm/shmem.c:5278].
- The alternative is to pre-shift owners when building the image [mkfs.erofs.1:299-304, 439-445]. IDs of 65536 or above force 64-byte extended inodes [erofs.rst:52-60].

**Layer plumbing.**
- ≤500 lowers [params.h:20]; `lowerdir+`/`datadir+` since v6.8 and layers passed as fds since v6.13 [overlayfs.rst:370-377, 453-478]. Stacking depth is ≤2 [include/linux/fs.h:297; overlayfs super.c:1203-1204], and file-backed EROFS refuses stacked backing filesystems [erofs super.c:630-641].
- Overlay `mmap` of a lower file maps the real file, so DAX survives overlay [fs/overlayfs/file.c:468-476; fs/backing-file.c:329-348]. `volatile` skips all syncs and refuses reuse after a crash [overlayfs.rst:836-861]. `st_dev`/`st_ino` are non-uniform across layers without `xino` [overlayfs.rst:39-77].

**Export hazard.** Copy-up writes `overlay.origin` onto upper directories and single-link files, even without `index` [fs/overlayfs/copy_up.c:688-689, 955-961]. A diff exporter must strip `user.overlay.*` xattrs and translate whiteouts, or they leak into OCI layers.

**Table 3. OCI layer entries mapped to overlay lower representation.**

| OCI entry [image-spec layer.md] | Overlay lower representation |
|---|---|
| `dir/.wh.X` (244-252) | `dir/X` as a char 0/0 device [overlayfs.rst:148-153]. Alternatively, a zero-size file with `overlay.whiteout`, with parent `overlay.opaque="x"` [overlayfs.rst:159-164, 583-591] |
| `dir/.wh..wh..opq` (276-315) | `overlay.opaque="y"` on `dir`, in the xattr namespace the mount uses [overlayfs.rst:155-157] |
| `.wh.X` plus a new `X/` in the same layer | A whiteout only hides lower layers (251-252), and opaque is applied before the directory is recreated (336). So emit `X/` as opaque, not as a whiteout |
| Hardlink whose target is outside the layer (71-73) | Cannot be represented; flatten or reject |
| `.wh.` with an empty name (248), or duplicate paths (21-22) | Reject |

### 2.5 OCI correctness hazards (Q5)

**Media types.**
- Layers are `…layer.v1.tar`, `+gzip` or `+zstd`. The non-distributable variants are deprecated, but implementations are expected to support existing images that use them [media-types.md:10-19; layer.md:349-361].
- Docker's `rootfs.diff.tar.gzip` is interchangeable with OCI gzip [media-types.md:60-64].
- An unknown `mediaType` in an index must not raise an error [image-index.md:50].

**Layer application.**
- Layers are applied, not simply extracted. Directory attributes are replaced; any other existing path is unlinked and recreated [layer.md:228-242].
- Manifest layers are ordered base first, and the result must equal applying them to an empty directory [manifest.md:68-73].
- Producers should emit explicit whiteouts, but consumers must accept both whiteout forms [layer.md:315]. Whiteouts should come before their siblings [layer.md:274].

**Tar entries.**
- Entries include sockets, device nodes and FIFOs.
- Attributes include uid/gid (names are ignored), mode, mtime and xattrs. Sparse files should not be used [layer.md:36-62].
- The spec does not name the PAX xattr keys. Both the SCHILY and LIBARCHIVE forms occur in practice [USAGE-SQFSTAR.md:29-31].

**Identity.**
- The DiffID is the digest of the uncompressed tar. A ChainID identifies an applied stack of layers [config.md:26-66].
- Content from untrusted sources should be verified against its descriptor digest [descriptor.md:30, 109].
- Manifest layer digests cover the blob as stored (usually compressed), which is distinct from the DiffID [config.md:31]. An EROFS conversion cannot regenerate that blob, so keeping `push`/`save` digests stable means keeping the original blobs.

**Platform selection.**
- Matching uses `architecture`, `os` and `variant` (arm64 variants are `v8`, `v8.1`, …). When several entries match, the first should be used [image-index.md:52-89, 104-116].
- BuildKit attestation manifests appear in indexes as platform `unknown/unknown` and must be skipped [buildkit docs/attestations/attestation-storage.md:100-105].

**Runtime conversion.**
- A non-numeric `Config.User` must be resolved from inside the image; if it cannot be, the converter must error [conversion.md:75-86].
- `Config.Volumes` should become mounts outside the rootfs [conversion.md:100-103].

## 3. Implications for shards (ranked)

### R1. Convert images on the host with an in-repo streaming tar→EROFS writer

Never extract onto the host filesystem.

- **What:** stream each layer (gzip/zstd/uncompressed) straight into EROFS. By default, resolve whiteouts into one flattened image per ChainID; an optional per-layer mode maps them per Table 3 using `user.overlay.*`. Resolve hardlinks across the stack; write device nodes, both PAX xattr forms and pre-shifted owners as metadata; block size = guest PAGE_SIZE; uncompressed, block-aligned data; fixed timestamps/UUID. Keep original blobs for `push`/`save`.
- **Why:** §2.2 points 2–5 and §2.5. Deterministic flattening underpins Lambda's 23× dedup [Brooker23 §2–3], and flattening removes layer-depth lookup cost [Harter16 §4.3 Fig. 11].
- **Benefit:** rootless on both hosts; faithful Linux metadata on APFS; byte-identical images on every host, usable as cache keys.
- **Cost/risk:** we own a filesystem writer whose bugs reach the guest kernel's parser (fuzz against `fsck.erofs`). liberofs is mostly GPL-2.0+ OR MIT [erofs-utils COPYING:1-15], so vendoring under our MIT licence needs a per-file audit.
- **Constraints:** runs at pull time, off the 5 ms path; adds no guest memory; enables DAX (R2).

### R2. Deliver the base rootfs, and small or medium images, as virtio-pmem with EROFS `dax=always`

Use virtio-blk as the fallback for large images. Do not deliver images over virtio-fs.

- **Base rootfs (engine, init):** one pmem region shared by every VM in a trust domain, booted with `root=/dev/pmem0 rootfstype=erofs rootflags=dax` [fc docs/pmem.md:28-30], so no userspace mount is needed.
- **Images:** one flattened image per device, 2 MiB aligned and padded. Use DAX when the expected touched fraction exceeds `sizeof(struct page)/PAGE_SIZE` [calc, §2.3]; otherwise use read-only virtio-blk with host buffered I/O. On arm64, prefer 16 KiB guest pages (4× less memmap).
- **Sharing:** within one tenant only [VIRTIO §5.19.9; Yarom14; Lipp16]; cross-tenant VMs get private copies.
- **Benefit:** no guest page cache; one host copy per image; O(1) mount; file data stays out of VM memory snapshots, since DAX pages are not guest RAM [dax.rst:10-15] and Firecracker restores pmem from the backing file [fc docs/pmem.md:254-261].
- **Risk:** memmap RAM and probe time grow with image size (E1); first-touch faults (E2); HVF sharing unverified (E3); no in-guest dm-verity, which cannot do DAX.
- **GPU:** never register DAX mmaps for DMA (§2.3). Ship driver userspace as a shared read-only image per driver version; NVIDIA regenerates CDI specs whenever the installed driver changes [NVIDIA CDI docs].

### R3. Privileged mounting happens once, before the engine starts

The engine itself stays rootless.

- **Flow:** guest init (platform code, running before any tenant code) mounts the EROFS devices, or the kernel mounts root. Where needed it creates idmapped detached mounts; pre-shifting (R1) usually avoids this. It passes mount fds to the engine, which mounts overlay in its user namespace with `userxattr,volatile`: lower = flattened image by fd (v6.13+ [overlayfs.rst:464-478]), upper = tmpfs.
- **Conflict:** a literal "no uid 0 anywhere in the guest" cannot mount block filesystems (§2.4). The only fully unprivileged path is FUSE (`erofsfuse` [erofs-utils man/erofsfuse.1]), at unmeasured cost (E6).
- **Runtime pulls:** the host pulls and converts, then hot-plugs a pmem/blk device (Firecracker has developer-preview PCI hot-plug for these [fc CHANGELOG.md:211-220]); init mounts it and hands the fd over. One shared store stays on the host.
- **Keep the guest mount table small:** namespace clone rate collapses as mounts grow, and namespace cleanup waits tens of ms for RCU [SOCK18 §2.1–2.2 Figs. 2–3].

### R4. Writable layer: tmpfs upper with `volatile` by default

Give large writers a per-VM virtio-blk scratch disk (ext4, with discard).

- **Evidence:** RunD's volatile writable device [RunD22 §4.2]; tmpfs `user.*` xattrs [shmem.c:4287-4298]; virtio-blk DISCARD/WRITE_ZEROES to return space [VIRTIO §5.2.3].
- **Risks:** tmpfs pages count as guest RAM; rootless copy-up has no metacopy, so `chown -R` copies data (E8, E11). Never put an upper or a file-backed EROFS image on overlay, because stacking depth is limited.

### R5. `docker build` runs in a builder microVM with the same engine

On macOS, builds must execute in a Linux VM anyway; the host only ingests the results.

- **Snapshots** are the overlay upper directories themselves. On commit, stream the upper once into (a) an OCI tar diff (char 0/0 → `.wh.X`, opaque → `.wh..wh..opq`, `user.overlay.*` stripped) and (b) an EROFS image, with no second extraction.
- **Compression:** keep local layers uncompressed (`…layer.v1.tar`) and compress with zstd only on push. BuildKit exposes `compression=uncompressed|gzip|estargz|zstd`, `force-compression` and `rewrite-timestamp` [buildkit README.md:286-295]. Slacker was fast partly because layer data was never compressed or decompressed [Harter16 §5.2].
- **Risk:** `chmod`/`chown`-heavy Dockerfiles pay full copy-ups; DADI observed copy-ups triggered by `chmod` [DADI20 §5.5].

### R6. Integrity: verify on the host, and trust the device inside the guest

- Verify blob digests at pull time [descriptor.md:30, 109]; the host VMM already controls guest memory, so guest-side re-verification adds cost without shrinking the TCB.
- Key each EROFS artifact by ChainID, format parameters and writer version.
- fs-verity needs ext4, f2fs or btrfs, so it is unavailable on APFS [fsverity.rst:13-15]. dm-verity cannot back DAX (Table 1).
- If a guest-side check is ever required, use dm-verity only on the virtio-blk path, created at boot with `dm-mod.create=` [dm-init.rst:1-40].

## 4. Open questions needing our own measurement

- **E1. pmem probe cost.** Sweep region size (64 MiB–8 GiB) × guest page size (4K/16K) on KVM-x86, KVM-arm64 and HVF. Record `nr_memmap_pages` from /proc/vmstat before and after probe [mm/vmstat.c:1031-1035], the `memmap_init_zone_device` "initialised N pages in X ms" debug line [mm/mm_init.c:1147], and boot-to-mount time. Fit memmap bytes and probe time as functions of size and page size.
- **E2. First-touch latency.** Trace container starts (`python -c 'import numpy'`, `node -e 0`, a CUDA smoke test) on DAX pmem; count stage-2 faults and time-to-ready. Variants: host `MAP_POPULATE`/`madvise(WILLNEED)`, `KVM_PRE_FAULT_MEMORY` (x86), 16 KiB HVF granules.
- **E3. HVF page sharing.** `hv_vm_map` one EROFS file into N VMs. Check with `footprint`/`vmmap` whether host memory grows once or N times, whether pages are wired, and coherency after a host-side write.
- **E4. Device break-even.** For N = 1…100 VMs of one image, measure host+guest memory and start latency for virtio-blk (buffered, O_DIRECT), virtio-fs+DAX and pmem+DAX. Validate the §2.3 inequality with measured touched fractions (Slacker's 6.4% dates from 2016).
- **E5. Converter.** Convert the top-100 Docker Hub images plus large ML images with R1, `mkfs.erofs --tar`, `sqfstar` and `mke2fs -d`: wall time, output size, and bit-identity across runs and across macOS/Linux hosts.
- **E6. Rootless overlay path.** Overlay mount time with 1/8/20/50 lowers (fds vs paths); open() latency vs depth (overlay analogue of [Harter16 Fig. 11]); flattened vs per-layer images; erofsfuse vs kernel EROFS.
- **E7. Ownership.** Root-owned 0600 files, setuid binaries, in-container `chown`, `docker cp`: compare pre-shifted, idmapped and unshifted images, plus the extended-inode size cost of shifting.
- **E8. Writable layer.** `pip install`, `npm ci`, `docker build` on tmpfs upper vs ext4 on virtio-blk (sparse; buffered vs O_DIRECT) vs ext4-DAX on memfd-backed pmem: memory, runtime, and reclaim after deletion.
- **E9. Large GPU images.** Block-trace bytes read at startup of CUDA/PyTorch images; choose full host prefetch or demand paging. Lambda moved demand paging to userfaultfd+mmap [Brooker23 §5.2]; on HVF the equivalent would be VMM handling of faults on unmapped IPA (**UNVERIFIED** as workable).
- **E10. Side channels on Apple silicon.** Attempt Flush+Reload between guests sharing pmem pages under HVF; the result decides whether per-tenant sharing is sufficient.
- **E11. Copy-up without metacopy.** Time `chown -R` over a 50k-file lower directory under `userxattr` vs a privileged `metacopy=on` baseline.

## 5. References

### Peer-reviewed papers

- **[Harter16]** T. Harter, B. Salmon, R. Liu, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. "Slacker: Fast Distribution with Lazy Docker Containers." USENIX FAST 2016. https://www.usenix.org/system/files/conference/fast16/fast16-papers-harter.pdf
- **[Anwar18]** A. Anwar, M. Mohamed, V. Tarasov, M. Littley, L. Rupprecht, Y. Cheng, N. Zhao, D. Skourtis, A. S. Warke, H. Ludwig, D. Hildebrand, A. R. Butt. "Improving Docker Registry Design Based on Production Workload Analysis." USENIX FAST 2018. https://www.usenix.org/system/files/conference/fast18/fast18-anwar.pdf
- **[Zhao19]** N. Zhao, V. Tarasov, H. Albahar, A. Anwar, L. Rupprecht, D. Skourtis, A. S. Warke, M. Mohamed, A. R. Butt. "Large-Scale Analysis of the Docker Hub Dataset." IEEE CLUSTER 2019, doi:10.1109/CLUSTER.2019.8891000. Accepted manuscript read at https://par.nsf.gov/servlets/purl/10167826
- **[DupHunter20]** N. Zhao, H. Albahar, S. Abraham, K. Chen, V. Tarasov, D. Skourtis, L. Rupprecht, A. Anwar, A. R. Butt. "DupHunter: Flexible High-Performance Deduplication for Docker Registries." USENIX ATC 2020. https://www.usenix.org/system/files/atc20-zhao.pdf
- **[DADI20]** H. Li, Y. Yuan, R. Du, K. Ma, L. Liu, W. Hsu. "DADI: Block-Level Image Service for Agile and Elastic Application Deployment." USENIX ATC 2020. https://www.usenix.org/system/files/atc20-li-huiba.pdf
- **[Starlight22]** J. L. Chen, D. Liaqat, M. Gabel, E. de Lara. "Starlight: Fast Container Provisioning on the Edge and over the WAN." USENIX NSDI 2022. https://www.usenix.org/system/files/nsdi22-paper-chen_jun_lin.pdf
- **[EROFS19]** X. Gao, M. Dong, X. Miao, W. Du, C. Yu, H. Chen. "EROFS: A Compression-friendly Readonly File System for Resource-scarce Devices." USENIX ATC 2019. https://www.usenix.org/system/files/atc19-gao.pdf
- **[RunD22]** Z. Li, J. Cheng, Q. Chen, E. Guan, Z. Bian, Y. Tao, B. Zha, Q. Wang, W. Han, M. Guo. "RunD: A Lightweight Secure Container Runtime for High-density Deployment and High-concurrency Startup in Serverless Computing." USENIX ATC 2022. https://www.usenix.org/system/files/atc22-li-zijun-rund.pdf
- **[Brooker23]** M. Brooker, M. Danilov, C. Greenwood, P. Piwonka. "On-demand Container Loading in AWS Lambda." USENIX ATC 2023. https://www.usenix.org/system/files/atc23-brooker.pdf
- **[Agache20]** A. Agache, M. Brooker, A. Florescu, A. Iordache, A. Liguori, R. Neugebauer, P. Piwonka, D.-M. Popa. "Firecracker: Lightweight Virtualization for Serverless Applications." USENIX NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- **[FaaSNet21]** A. Wang, S. Chang, H. Tian, H. Wang, H. Yang, H. Li, R. Du, Y. Cheng. "FaaSNet: Scalable and Fast Provisioning of Custom Serverless Container Runtimes at Alibaba Cloud Function Compute." USENIX ATC 2021. https://www.usenix.org/system/files/atc21-wang-ao.pdf
- **[SOCK18]** E. Oakes, L. Yang, D. Zhou, K. Houck, T. Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. "SOCK: Rapid Task Provisioning with Serverless-Optimized Containers." USENIX ATC 2018. https://www.usenix.org/system/files/conference/atc18/atc18-oakes.pdf
- **[Yarom14]** Y. Yarom, K. Falkner. "FLUSH+RELOAD: A High Resolution, Low Noise, L3 Cache Side-Channel Attack." USENIX Security 2014. https://www.usenix.org/system/files/conference/usenixsecurity14/sec14-paper-yarom.pdf
- **[Lipp16]** M. Lipp, D. Gruss, R. Spreitzer, C. Maurice, S. Mangard. "ARMageddon: Cache Attacks on Mobile Devices." USENIX Security 2016. https://www.usenix.org/system/files/conference/usenixsecurity16/sec16_paper_lipp.pdf

### Specifications

- **[VIRTIO]** OASIS. "Virtual I/O Device (VIRTIO) Version 1.3," Committee Specification Draft 01, 6 Oct 2023: §2.10, §5.2.3, §5.11.6.4–5.11.6.5, §5.19. https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html
- **[image-spec]** OCI Image Format Specification, https://github.com/opencontainers/image-spec at ca68a05f (2026-09-17): layer.md, manifest.md, image-index.md, config.md, conversion.md, media-types.md, descriptor.md.

### Linux kernel (v7.2-rc4, `/Users/adalundhe/Projects/linux`, commit 1590cf03)

- Documentation: `Documentation/filesystems/{overlayfs,erofs,squashfs,dax,fsverity,idmappings,virtiofs,tmpfs}.rst`, `Documentation/admin-guide/device-mapper/{verity,dm-init}.rst`, `Documentation/admin-guide/mm/ksm.rst`, `Documentation/virt/kvm/api.rst`.
- Source: the `fs/`, `mm/`, `drivers/`, `include/` and `arch/` files cited inline.
- Commits, with first-release tags verified through GitHub's compare API against torvalds/linux:
  - 459c7c565ac3 "ovl: unprivieged mounts" (v5.11-rc1).
  - a3c751a50fe6 "vfs: allow unprivileged whiteout creation" (v5.8-rc1).
  - ce63cb62d794 "erofs: support unencoded inodes for fileio" (v6.12-rc1).

### Man pages

- mount_setattr(2), Linux man-pages: https://man7.org/linux/man-pages/man2/mount_setattr.2.html
- macOS chown(2) and mknod(2): `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/share/man/man2/{chown,mknod}.2`

### Vendor documentation

- Apple, "Apple File System Guide — FAQ." https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/FAQ/FAQ.html
- Apple, Hypervisor.framework `hv_vm.h` (MacOSX 26.5 SDK): `/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/Frameworks/Hypervisor.framework/Headers/hv_vm.h`
- NVIDIA, "Support for Container Device Interface," NVIDIA Container Toolkit docs. https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/cdi-support.html

### Project source and documentation

- **Firecracker** (`/Users/adalundhe/Projects/firecracker`, edb60617): docs/pmem.md, docs/block.md, CHANGELOG.md, src/vmm/src/devices/virtio/pmem/device.rs.
- **libkrun** (`/Users/adalundhe/Projects/libkrun`, 1f5dd028): include/libkrun.h, src/devices/src/virtio/fs/macos/passthrough.rs, src/hvf/src/lib.rs.
- **go-microvm** (`/Users/adalundhe/Projects/go-microvm`, 7e148d85): docs/MACOS.md, image/pull.go.
- **erofs-utils** v1.9.4 (git.kernel.org/pub/scm/linux/kernel/git/xiang/erofs-utils.git, f36cadb5): man/mkfs.erofs.1, man/erofsfuse.1, lib/tar.c, lib/xattr.c, COPYING.
- **composefs** (github.com/containers/composefs, ec2573a0): README.md, man/mkcomposefs.md.
- **squashfs-tools** (github.com/plougher/squashfs-tools, db038ef2): Documentation/4.7.6/USAGE-SQFSTAR.md.
- **e2fsprogs** (git.kernel.org/pub/scm/fs/ext2/e2fsprogs.git, master): misc/mke2fs.8.in, doc/RelNotes/v1.47.1.txt.
- **BuildKit** (github.com/moby/buildkit, 3bcbbc94): README.md, docs/rootless.md, docs/attestations/attestation-storage.md.
- **containerd** (github.com/containerd/containerd, 04f9be90): docs/snapshotters/erofs.md.
- **NVIDIA open-gpu-kernel-modules** (61dcc937): kernel-open/nvidia/os-mlock.c.
