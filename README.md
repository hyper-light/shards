<p align="center">
  <a href="docs/assets/brand/shards-fleet-preview.png">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/shards-fleet-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="docs/assets/brand/shards-fleet-light.svg">
      <img src="docs/assets/brand/shards-fleet-light.svg" alt="Shards logo: three faceted shards floating apart at different heights" width="84" height="90">
    </picture>
  </a>
</p>

<h1 align="center">shards</h1>
<p align="center"><em>Rootless microVMs for agents, built and run like containers.</em></p>

Give an agent a shell and it will install packages, start servers, run builds and, sooner or
later, break something. Shards gives your agents a microVM to do that in. It has its own Linux
kernel, so nothing inside it reaches your computer.

One microVM runs many agents. Shards' own runtime sits where containerd would. It speaks
Docker and Compose but runs no containers, so a microVM full of agents snapshots and restores
in microseconds. You build microVMs like Docker images. Your agents can:

- Boot in under 200 µs
- Share a microVM, or each get their own
- Use the Docker commands and Compose files you already have
- Reach only the network, devices and files you allow
- Use GPUs
- Break anything without touching your computer

Shards is one binary. It never needs root.

```console
$ time shards vm run --kernel vmlinux --init shards-init --cmdline "console=ttyS0 quiet"
shards-init: pid 1 running at uptime 0.012768s
[    0.015804] reboot: Power down

real	0m0.028s
user	0m0.019s
sys	0m0.007s

$ time shards vm restore snap
SHARDS-TEST PASS
[    0.016121] reboot: Power down

real	0m0.007s
user	0m0.002s
sys	0m0.003s
```

Booting Linux, running a program and shutting down took 28 ms. Restoring a saved machine
took 7 ms, most of it process start and exit. The saved machine runs the test program from
[Snapshots](#snapshots).

> [!NOTE]
> Output captured on an Apple M5 Max with macOS 26.4.1, from a release build at `e8ec2e6`,
> with the pinned Linux 6.18 kernel as `vmlinux`.

> [!IMPORTANT]
> Shards has no release yet. Today it boots Linux microVMs on Apple silicon Macs and x86_64
> Linux, and snapshots and restores them on the Mac. The image builder, Docker-compatible
> commands, in-VM runtime, isolation controls and GPU support are still being built. See
> [Where things stand](#where-things-stand).

## Install

Build from source with Rust 1.98 (pinned by the toolchain file):

```sh
git clone https://github.com/hyper-light/shards && cd shards
cargo build --release -p shards
codesign -s - -f --entitlements resources/hvf.entitlements target/release/shards   # macOS only
cp target/release/shards ~/.local/bin/
```

- **macOS**: 15 or later, on Apple silicon. The `codesign` line lets shards use
  Hypervisor.framework. It is an ad-hoc signature, so you need no developer account. Without
  it, machines fail to start with `HV_DENIED`.
- **Linux**: x86_64, with access to `/dev/kvm` (usually the `kvm` group). Add
  `--target x86_64-unknown-linux-musl` for a static binary.
- **Elsewhere** (arm64 Linux, Intel Macs, Windows): shards builds, but can't run machines yet.

## Quickstart

You need a kernel and a program to run as PID 1. Until shards ships its own kernel, use
Firecracker's:

```sh
arch=$(uname -m | sed s/arm64/aarch64/)
curl -fLo vmlinux https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260923-6f82ac4cf331-0/$arch/vmlinux-6.18.48
shasum -a 256 vmlinux
```

| Architecture | SHA-256 |
|---|---|
| aarch64 (arm64) | `a80108af80d9549b357ea7e00bd5c12f80686869541d135a8a67f6fe1ec3451e` |
| x86_64 | `9204218e8bcca6ac23848d74f45df2eb19d7f31e8277840a7d145a0df8b078d2` |

Build `shards-init`, a tiny PID 1 that reports Linux's boot time and powers off:

```sh
cargo build -p shards-init --profile guest --target $arch-unknown-linux-musl
cp target/$arch-unknown-linux-musl/guest/shards-init .
```

Boot it:

```console
$ shards vm run --kernel vmlinux --init shards-init --cpus 4 --memory 512 --cmdline "console=ttyS0 quiet"
shards-init: pid 1 running at uptime 0.014162s
[    0.018563] reboot: Power down
```

The console is your terminal, and `Ctrl-A x` stops the machine. Drop `quiet` to see the kernel
log. `--init` takes any static Linux binary, `--initrd` a whole initramfs, and
`--disk FILE[:ro]` adds `/dev/vda`, `/dev/vdb` and so on. The defaults are 1 CPU and 256 MiB.

## Snapshots

Set a machine up once, save it, and start copies. A program inside the machine decides when
to save. The test program does it right away:

```sh
cargo build -p shards-testguest --profile guest --target $arch-unknown-linux-musl
cp target/$arch-unknown-linux-musl/guest/shards-testguest .
```

```console
$ shards vm run --kernel vmlinux --init shards-testguest --snapshot-dir snap \
    --cmdline "console=ttyS0 quiet shards_test=resume"
$ du -h snap/*
 43M	snap/memory
 20K	snap/state
```

Empty memory isn't stored, so 256 MiB of RAM takes 43 MiB on disk. `shards vm restore snap`
starts a copy exactly where the original asked to be saved, as in the example at the top.

- **Copies are cheap.** They share the snapshot's memory until they write to it.
- **Copies don't collide.** Linux reseeds its randomness in each copy as it wakes, so keys
  and IDs differ between copies. Values a program drew before the snapshot don't.
- **Clocks keep running** from the moment of the snapshot. Wall-clock time lags until
  something in the machine sets it.

`--hold` preloads a copy and waits. A line on stdin starts it in 150 µs:

```console
$ (sleep 1; echo) | shards vm restore snap --hold
shards-ready
SHARDS-TEST PASS
[    0.016274] reboot: Power down
```

> [!NOTE]
> Snapshots work on macOS. Linux is next.

## Where things stand

| Feature | Today (2026-09-28) |
|---|---|
| Boot Linux on Apple silicon Macs | Works |
| Boot Linux on x86_64 Linux | Works, tested in CI on every push |
| Snapshot and restore | Works on macOS; Linux is next |
| arm64 Linux, Intel Macs, Windows | Builds, but can't run machines yet |
| Run commands in a running machine | Next |
| Build machines like Docker images | Planned |
| Docker's commands (`run`, `build`, `ps`, `exec` and the rest) | Planned. Today: `shards vm run` and `shards vm restore` |
| Many agents per machine, with Docker and Compose | Planned |
| Network, device and permission controls per agent and per machine | Planned |
| GPUs | Planned |

The target is a usable machine within 5 ms of the request, using less memory than
Firecracker. The plan and its evidence are in
[docs/design/architecture.md](docs/design/architecture.md).

## Commands

| Command | What it does |
|---|---|
| `shards vm run --kernel FILE [options]` | Boot a new machine |
| `shards vm restore DIR [--hold]` | Start a copy of the machine saved in `DIR`. `--hold` preloads it and waits for a line on stdin |
| `shards version` · `shards help` | |

| Option | What it does |
|---|---|
| `--init FILE` | The static Linux binary to run as PID 1 |
| `--initrd FILE` | An initramfs, instead of `--init` |
| `--cmdline TEXT` | The kernel command line (default: `console=ttyS0 earlycon panic=-1`) |
| `--cpus N` · `--memory MIB` | Size (default: 1 CPU, 256 MiB) |
| `--disk FILE[:ro]` | A disk. Repeat for more |
| `--snapshot-dir DIR` | Save the machine to `DIR` when it asks, then exit. `--snapshot-then resume` keeps it running. Works with `restore` too |
| `--no-console` | Hide the console |

Exit codes: 0 for shutdown or snapshot, 1 for an error, 2 for bad usage, 3 when the guest
reboots. `SHARDS_LOG=debug` shows what shards is doing.

## Performance

Measured on an Apple M5 Max with macOS 26.4.1 at `e8ec2e6`: 100 runs each, 1 CPU, 256 MiB.
Details are in [docs/benchmarks.md](docs/benchmarks.md).

| Start | p50 | p99 |
|---|---:|---:|
| Restore, preloaded with `--hold` | **149 µs** | 197 µs |
| Restore, in a new process | 794 µs | 1.7 ms |
| Cold boot, to PID 1 | 21.2 ms | 22.2 ms |

Linux itself takes 18.6 ms of a cold boot. That is why shards restores snapshots, and why a
leaner kernel is coming.

Peak memory is 12.5 MiB for a restored machine and 59.3 MiB for a booted one, guest memory
included.

Firecracker's published bounds are 125 ms to `/sbin/init` and 5 MiB of VMM overhead, for
1 CPU and 128 MiB. A same-host comparison is in progress.

## How it works

- **Isolation.** Each machine is a VM with its own kernel, in its own process.
- **Speed.** Shards boots once, snapshots, and restores copies. A preloaded copy only has to
  start running.
- **Density.** Copies share snapshot memory until they write to it.
- **No root.** Shards uses `/dev/kvm` on Linux and Hypervisor.framework on macOS.
- **No crashes.** Bad input gets an error message. Lints forbid code that can panic, and CI
  enforces them.
- **Portability.** One codebase covers Linux, macOS and Windows on x86_64 and arm64, and CI
  builds all eight targets.

The design and the evidence behind each decision are in
[docs/design/architecture.md](docs/design/architecture.md).

## Documentation

| Doc | What's in it |
|---|---|
| [Architecture](docs/design/architecture.md) | The design, each decision with its evidence, and the plan |
| [Benchmarks](docs/benchmarks.md) | Every measurement, with its command, machine and revision |
| [Research](docs/research/) | The papers, specifications and measurements behind the design |
| [Brand](docs/assets/brand/README.md) | The logo |

## Contributing / development

```sh
cargo build -p shards
cargo test --workspace --release       # boots real machines
cargo clippy --workspace --all-targets --target <triple> -- -D warnings
cargo bench -p shards --bench boot     # and --bench restore
```

The tests need what shards needs (see [Install](#install)). They fetch the kernel and build
the guest programs themselves. Hosts that can't run machines print `SKIP:`. The project rules
are in [CLAUDE.md](CLAUDE.md).

## Acknowledgements

Shards learned a great deal from [Firecracker], and its tests boot Firecracker's CI kernels.

## License

MIT — © 2026 Hyperlight. See [LICENSE](LICENSE).

[Firecracker]: https://github.com/firecracker-microvm/firecracker
