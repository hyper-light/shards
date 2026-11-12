# Hex's GPU patches 0002 and 0007: each Venus context bound to its VM's selected GPUs

**What is ported (Hex's 0002, shards commits de0a0e6e and 0b6a4b40 on Hex's
hex/physical-gpu-0342).** `run --kernel … --gpu-device ID` (repeatable, implies `--gpu`) gives
the VM process exact GPU identities (shards-abi's `PhysicalGpu`); the renderer resolves them to
the host's live Vulkan device UUIDs before any guest context or host-visible window exists
(`vm::gpu_selection`, crates/vmm/src/devices/virtio/gpu/selection.rs: missing, ambiguous,
aliased or foreign identities refused) and binds every Venus context of the VM to exactly those
devices, through virglrenderer's physical-device filter and the selection's transport to its
render server (resources/gpu/physical-{linux,krunkit}.patch and
physical-transport-{linux,krunkit}.patch, which scripts/gpu-libs applies to the pinned
renderer) and a small identity bridge (resources/gpu/native-identity.c) built beside the
renderer's libraries and linked into the GPU's VM process. The snapshot format becomes 29,
keeping the selection, and still reads 28. Hex's research on the filter and transport: its
docs/hex-venus-physical-filter (worktree adversarial-relay) and shards'
docs/research/measurements/managed-virtio-gpu/DESTROY-CONTRACT.md, which verified these patch
bytes (selected-de0a) against Hex's historical 6219.

**Adapted.** `virgl.rs` is Hex's state after 0002 with this branch's held driver (Hex's 0008,
applied to that state with its one type, `Vec<UuidText>` for `Vec<SelectedGpu>`, as of 0002).
The `--gpu-device` usage line Hex inserted inside `--gpu`'s paragraph is its own paragraph after
it. Hex's 0007 is folded in: without it the Linux bridge library is a thin archive and the GPU's
VM process does not link (below).

**Not ported, for ROOT.** (1) Windows arm64 GPU packaging: Hex's 0002 adds
`aarch64-pc-windows-msvc` to crates/shards/build.rs `gpu_target` and teaches scripts/gpu-libs to
build for Windows arm64 (target detection, a clang-cl cross file, two more wheel hashes). ROOT's
GPU lane holds exactly these spans pending ROOT's agreement and crossbuild research
(managed-virtio-gpu/HANDOFF.md), and Hex's own record checked the Windows renderer patches for
composition only, never compiled (hex-venus-physical-filter/RESULTS.md); no Windows arm64 GPU
build or host has run it. (2) Moving gpu-libs' scratch from `mktemp -d` into
`$XDG_CACHE_HOME/shards/gpu-libs`: no reason is recorded (de0a0e6e has no body).

**Hex's 0007, red and green (R09/R10).** `gpu-libs-archives.sh` on adacachy (451397fc),
2026-11-11, meson 1.11.2 from gpu-libs' pinned wheels:

| Build | libshards_gpu_selection.a | shards-vmm's GPU tests link |
|---|---|---|
| g0007-red: 0002's gpu-libs (meson's own archive copied) | `!<thin>`, 272 bytes | exit 101: `rust-lld: … libshards_gpu_selection.a: could not get the buffer for a child of the archive: 'libshards_gpu_selection.a.p/native-identity.c.o': No such file or directory` |
| g0007-green: the object archived whole (`ar rcsD`), as the render server's are | `!<arch>`, 75,556 bytes | exit 0 |

**Gates.** adacachy, 2026-11-11, the tree at this commit with 0010–0015 and 0021 (gate.sh g0002,
g0002b): fmt; clippy -D warnings for shards-api/abi, shards-vmm, shards-vmm with `gpu`, and
shards (all targets); tests: shards-vmm's lib 455 passed, the GPU device's 16, shards-abi's 31,
shards-api's 63 (one earlier run failed only the zero-patience contract flake 0030's record
names). With green's libraries, on the RTX 5060 Ti (driver 615.78.08): the held-driver test,
`the_renderer_offers_venus_and_holds_its_blobs` and the four `vm::gpu_selection` tests pass.

**Owed to ROOT.** A GPU VM booted with `--gpu-device` on each GPU host class (Linux NVIDIA and
others, macOS Metal, Windows x86_64): that its Venus contexts render on the selected GPU and on no
other, refuse a wrong identity, and restore a format-29 snapshot.
