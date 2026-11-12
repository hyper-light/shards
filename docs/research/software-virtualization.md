# Software virtualization: shards' own isolation where the host gives none

Status: research, opened 2026-11-11. Nothing here is decided until its D-entry is written.
Policy: `docs/development-rules.md` SHA-256 `df2acf47cf92996e…`; this record is the
coordinator's (R02, R10). Measurements: `measurements/softvirt-traps/`, PM M516 onward.

## The goal

Shards runs microVMs only where the host hands it hardware virtualization: `/dev/kvm` on
Linux, Hypervisor.framework on macOS, the Windows Hypervisor Platform on Windows
(`crates/vmm/src/hv/mod.rs`, one backend a target). Many hosts hand it none: cloud VMs
without nested virtualization (EC2's AMD and Graviton instances, most of its Intel ones;
GitHub's arm64 runners), containers and pods without `/dev/kvm`, Windows without Hyper-V,
macOS VMs on an M1 or M2 or before macOS 15. There shards starts no microVM at all.

Ada's goal (2026-11-11): shards supplies the isolation itself there, on all seven targets,
with guarantees modelled on AWS Nitro Sandboxes' (isolation, attestation, memory), in
software. Her decisions, the same day:

1. **Memory hidden from the host** is required only where the CPU provides it (SEV-SNP's
   VMPLs, TDX partitioning, Arm CCA), and comes later. Everywhere else, isolation of a
   guest from the host and from other guests, with attestation, is enough.
2. **Rootless and one binary, with no exceptions.** No kernel module (PVM's route), no
   signed driver, nothing installed beside `shards`. "Do the hard work and keep both."

Kept unless she says otherwise: the guest runs a real Linux kernel (shards' pinned one,
paravirtualized as needed), so the in-VM runtime keeps namespaces, Landlock, seccomp,
cgroups and eBPF device filters, and images' programs run unmodified.

## What Nitro guarantees, and what software can

Nitro's terms are from the EC2 user guide's Nitro Sandboxes pages, Firecracker v1.18's
`docs/nitro-sandboxes/` (branch `firecracker-v1.18`, PR #6408, 2026-11-09) and the
whitepaper "The Security Design of the AWS Nitro System" (2024-02-15).

| Nitro | In software here | How, or the limit |
|---|---|---|
| A sandbox cannot reach the parent's memory or another sandbox's | yes | The guest runs deprivileged; every trap and every mapping of guest memory goes through shards; the processes holding guest code are confined (seccomp + Landlock on Linux, App Sandbox on macOS, AppContainer on Windows). What must be trusted: shards' isolation core and the host kernel, where Nitro's confidential mode trusts neither the parent nor its kernel. |
| Confidential mode: the parent cannot read the sandbox's memory | no, by decision 1 | The most privileged software of a machine reads all of its memory. Linux's `memfd_secret` takes pages out of the kernel's direct map, but it counts against `RLIMIT_MEMLOCK` (`mm/secretmem.c`, `mlock_future_ok`): 8 MiB for a user on Fedora 44, so it can hold keys, not guest memory (PM M516's probe). |
| Attestation: the NSM measures the image into PCRs and signs documents | yes, rooted elsewhere | Per-sandbox measurement registers and signed documents, through a guest device modelled on the NSM; the signing key bound to the host's TPM measured boot (NitroTPM, Azure and GCP vTPMs, a PC's TPM) or a SEV-SNP or TDX report. Open: rootless TPM access (Linux's `/dev/tpmrm0` belongs to group `tss`), and what macOS lets a third party verify. |
| Memory donated whole, zeroed, never shared between instances | yes, and more | Guest pages zeroed before use, never shared between tenants, kept out of core dumps; snapshots, copy-on-write within one tenant's template, save, move and elastic memory all stay, since every guest page is in shards' processes. Nitro Sandboxes have none of those (no snapshots, pause, migration, balloon or virtio-mem). |
| A core runs one sandbox or the parent, never both hyperthreads; state flushed between | Linux only | Core scheduling (`PR_SCHED_CORE`, commit 7ac592aa35a6, 2021) needs only ptrace access to one's own processes and worked unprivileged on Fedora 44 (PM M516). macOS and Windows give a user process nothing like it that we know of: to confirm, then to record as a parity gap. |
| Vetted isolation core: the Nitro Isolation Engine, proven in Isabelle/HOL | possible | The part that enforces isolation (trap dispatch, the map of guest memory, the code checker) kept small enough to prove: RockSalt (PLDI 2012) proved an x86 code checker, VeriWasm (NDSS 2021) checks compiled Wasm's isolation; Firecracker proves parts of its virtqueue code with Kani (`docs/formal-verification.md`). |

## What a host gives a process without hardware virtualization

A guest's kernel cannot run privileged without VT-x, AMD-V or EL2. Shards owns that kernel, so
it can be paravirtualized; the programs inside cannot be changed. What decides the design
is how the host lets an unprivileged program catch another's system calls and faults:

- **Linux** traps them for any process, without root. A seccomp filter's
  `SECCOMP_RET_TRAP` raises SIGSYS in the calling thread; `SECCOMP_RET_USER_NOTIF` (with
  synchronous wake-up since commit 48a1084a8b74, 2023) hands the call to another process;
  ptrace's `PTRACE_SYSEMU` (arm64's since commit fd3866381be2, 2019) stops at each call.
  Faults arrive as signals, or for user-mode faults through userfaultfd's
  `UFFD_USER_MODE_ONLY`, which Fedora 44 allows a user where it refuses the rest (PM M516).
  Guest memory is a memfd mapped into each guest address space at the guest's offsets.
  - gVisor's systrap platform runs its users' code so, with a reimplemented kernel
    (gvisor.dev's platform guide; default since its 2023-04-28 release post). The guide
    reports it often faster than KVM inside a nested VM.
  - User-mode Linux runs a real Linux kernel as a process, its processes as children:
    with seccomp and futex handoffs since commits dac494bf54f7 and 406d17c6c370 (Benjamin
    Berg, 2025-06-02), SMP since 1e4ee5135d81 (Tiwei Bie, 2025-10-27). x86 hosts only:
    `arch/x86/um` is its only subarchitecture (Linux 7.2-rc4, 1590cf032971).
- **macOS** has no syscall filter. A guest's `svc` would enter XNU with registers it chose,
  so guest code must never execute one: it reaches the hypervisor through `brk` (each `svc`
  rewritten, which arm64's fixed four-byte instructions make checkable page by page) or a
  fault, raised as a signal in its own process or as a Mach exception to another
  (`posix_spawnattr_setexceptionports_np`). Each costs 3.4 to 5.7 us on an M3 Ultra, 50
  to 80 times a native system call (PM M517). Mapping guest memory into another process
  needs its task port: `task_for_pid` is refused, but a child can send its parent its own,
  with or without the hardened runtime, and the parent then maps a 16 KiB page into it in
  0.5 us (M517; untested under App Sandbox). Translated code needs `MAP_JIT`
  (`com.apple.security.cs.allow-jit` under the hardened runtime), whose per-thread write
  protection flips in 83 ns.
- **Windows** has no syscall filter for user processes either, and x86_64's variable-length
  instructions defeat checking a code page for `syscall`: guest code must be translated
  there; arm64 Windows could check pages as macOS can. To measure: vectored exception
  handlers in-process, debug events across processes, placeholder views mapped into
  another process (`VirtualAlloc2`, `MapViewOfFile3`), AppContainer.

## The approaches

1. **Native execution, host traps** (Linux): guest programs run at full speed in confined
   host processes; each guest system call and fault is a trap to shards. Cost: one round
   trip per call, which hardware virtualization never pays (a guest's system call stays
   in the guest). PM M516 measures each way of making it.
2. **Paravirtualized kernel with a host module** (PVM, SOSP 2023; out of tree at
   `virt-pvm/linux`; shipped by Tencent's CubeSandbox): excluded by decision 2.
3. **A reimplemented kernel** (gVisor's Sentry; Microsoft's LiteBox, a Rust library OS,
   v0.1.0 2026-10-07): excluded by the real-kernel default; their trap paths still inform 1.
4. **Binary translation with software fault isolation**: the guest's code rewritten so it
   reaches only guest memory and enters shards instead of the host kernel. VMware's
   translator (Adams and Agesen, ASPLOS 2006) ran its monitor in ring 0 through a driver,
   which decision 2 rules out; QEMU's TCG translates without privilege on every host. A
   guest system call becomes a call, with no host trap; every memory access pays for its
   translation. The only route on macOS and Windows that needs neither root nor a
   hypervisor.
5. **Checked native code** (arm64 hosts): guest code pages checked before they may run
   (no `svc`, `hvc`, `smc` or privileged register writes; `svc` rewritten to `brk`), run
   natively, with traps as exceptions (NaCl's ARM sandbox: Sehr et al., USENIX Security
   2010). Guest JITs that write and run the same pages pay a check per change.

## Questions, each with what settles it

1. What each trap costs, per host and mechanism: PM M516 (Linux), M517 (macOS), Windows
   next. Hardware virtualization pays none of these for a guest's system calls.
2. How many host mappings a guest's memory needs: a page each where guest pages are not
   contiguous, against `vm.max_map_count` (65530 by default: `DEFAULT_MAX_MAP_COUNT`,
   `include/linux/mm.h`; Fedora 44 sets 1048576, PM M516). Measure a guest kernel's real layouts; large folios and 2 MiB units
   cut it 512 times where pages are contiguous.
3. Guest JITs (V8, the JVM) under checked or translated code: how often real agent
   workloads make code writable and executable again.
4. macOS: a child's task port without root (PM M517), and under App Sandbox.
5. Windows: exception and debug-event round trips, views into another process,
   AppContainer.
6. The isolation core: what each approach must trust, and its size.
7. Attestation roots per host, without root: TPM access on Linux and Windows, macOS's.
8. Baselines (R08), on the same hosts: gVisor systrap, User-mode Linux (x86_64), QEMU TCG,
   nested KVM where offered, Nitro Sandboxes under Firecracker v1.18 (needs EC2 access).
