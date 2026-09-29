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
<p align="center"><em>MicroVMs for agents, designed for home or at scale.</em></p>

Give an agent a shell and it will install packages, start servers, run builds and, sooner or
later, break something. Shards runs your agents in microVMs: small virtual machines with their
own Linux kernel, so nothing inside one reaches your computer.

You build a microVM like a Docker image, and it is the OS your agents run on. Many agents share
one microVM, each isolated like a container, with its own networks, files, devices and
permissions. Because they inherit the microVM's OS instead of bringing their own images,
shards needs no container runtime. Its own runtime takes containerd's place and speaks Docker
and Compose, and a whole microVM, agents and all, snapshots and restores in microseconds.
Your agents can:

- Boot in under 200 µs
- Share a microVM's OS, each isolated like a container
- Get a microVM to themselves when they shouldn't share one
- Reach only the networks, files and devices you give them
- Use the Docker commands and Compose files you already have
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
> with Firecracker's CI build of Linux 6.18 as `vmlinux`.

> [!IMPORTANT]
> Shards has no release yet. Today it boots Linux microVMs on Apple silicon Macs and x86_64
> Linux, and snapshots and restores them on the Mac. Building microVMs like Docker images,
> the runtime that runs agents inside them, per-agent isolation, the Docker-compatible commands
> and GPU support are still being built. See [Where things stand](#where-things-stand).

## Install

Build from source with Rust 1.98 (pinned by the toolchain file):

```sh
git clone https://github.com/hyper-light/shards && cd shards
cargo build --release -p shards
codesign -s - -f --entitlements resources/hvf.entitlements target/release/shardsd   # macOS only
cp target/release/shards target/release/shardsd ~/.local/bin/
```

shards is two programs: `shards`, the command you type, and `shardsd`, which runs the
machines. Keep them in the same directory.

- **macOS**: 15 or later, on Apple silicon. The `codesign` line lets `shardsd` use
  Hypervisor.framework. It is an ad-hoc signature, so you need no developer account. Without
  it, machines fail to start with `HV_DENIED`.
- **Linux**: x86_64, with access to `/dev/kvm` (usually the `kvm` group). Add
  `--target x86_64-unknown-linux-musl` for a static binary.
- **Elsewhere** (arm64 Linux, Intel Macs, Windows): shards builds, but can't run machines yet.

## Quickstart

You need a kernel and a program to run as PID 1. Download shards' kernel, built
reproducibly by CI ([resources/kernel](resources/kernel/README.md)):

```sh
arch=$(uname -m | sed s/arm64/aarch64/)
file=$([ $arch = x86_64 ] && echo vmlinux || echo Image)-6.18.48-$arch
curl -fLo vmlinux https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-1bff175d35cb/$file
shasum -a 256 vmlinux
```

| Architecture | SHA-256 |
|---|---|
| aarch64 (arm64) | `ed7fb50d27b59e29e9e6c9f57f02c4bb82f8c3f5ecd51bd8083741f77597913b` |
| x86_64 | `136a182b7013fa32d852a7f227b91f6c113d9ad9dbe7a9b9d4baac7153ddd59c` |

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

| Feature | Today (2026-09-29) |
|---|---|
| Boot Linux on Apple silicon Macs | Works |
| Boot Linux on x86_64 Linux | Works, tested in CI on every push |
| Snapshot and restore | Works on macOS; Linux is next |
| arm64 Linux, Intel Macs, Windows | Builds, but can't run machines yet |
| Connect host programs to programs in a running machine (vsock) | Works |
| Pull images from Docker Hub and other registries, as `docker pull` does | Works: `shards pull`. Your `docker login` credentials and `certs.d` certificates work as they are |
| Run a command in an image, as `docker run` does | Works: `shards run IMAGE`, with the image's entrypoint, command, environment, directory, user and stdin (`-i`), in the background (`-d`), named (`--name`) or removed when done (`--rm`). On the Mac, repeated runs of an image are served from copies of its booted microVM that a background service restores ahead of time: about 5 ms from start to exit |
| Build microVMs like Docker images | In progress: image layers become bootable images |
| Run many agents on a microVM's OS, each isolated like a container | Planned |
| Networks, files, devices and permissions per agent and per microVM | Planned |
| Docker's commands and Compose files | In progress: `run`, `ps`, `wait`, `logs`, `stop`, `kill` and `rm` take `docker`'s flags and answer with its words, its `--help` included. A flag shards can't serve yet says so. `build`, `exec`, the rest and Compose are planned |
| GPUs | Planned |

The target is a usable machine within 5 ms of the request, using less memory than
Firecracker. The plan and its evidence are in
[docs/design/architecture.md](docs/design/architecture.md).

## Commands

| Command | What it does |
|---|---|
| `shards pull [-q] IMAGE` | Pull `IMAGE` as `docker pull` does, for this machine's architecture. Every layer is checked against its digests before it is kept |
| `shards run [OPTIONS] IMAGE [COMMAND] [ARG...]` | Run a command in a new microVM booted into `IMAGE`, as `docker run` runs it in a new container, with `-d`, `-e`, `-h`, `-i`, `-u`, `-w`, `--entrypoint`, `--name`, `--pull` and `--rm`. `IMAGE` is pulled first if it isn't here. It boots the guest `shards guest use` chose, or `SHARDS_KERNEL` and `SHARDS_INIT` |
| `shards ps [-a] [-q] [-n N] [-l] [--no-trunc]` | List containers, as `docker ps` does: each run is one, until `shards rm` or `--rm` removes it |
| `shards wait CONTAINER...` | Wait for containers to stop, and print their exit codes |
| `shards logs [-f] [-t] [-n N] CONTAINER` | Print what a container wrote, stdout to stdout and stderr to stderr |
| `shards stop [-t SECONDS] [-s SIGNAL] CONTAINER...` | Stop containers: the signal (SIGTERM), then SIGKILL after 10 s |
| `shards kill [-s SIGNAL] CONTAINER...` | Send containers a signal (SIGKILL) |
| `shards rm [-f] CONTAINER...` | Remove stopped containers; with `-f`, running ones too |
| `shards daemon stop` | Stop the background service (`shardsd daemon`) that `shards run` starts on its own. It keeps `SHARDS_POOL` microVMs ready for each image you run (default 2), and exits after `SHARDS_DAEMON_IDLE` seconds without a run (default 900). Stopping it stops the containers running, as Docker's does |
| `shards guest use --kernel FILE --init FILE` | Choose the kernel and shards-init that `shards run` boots. With them chosen, the first run of an image saves a copy of its booted microVM, and later runs start from that copy. `shards guest` shows the choice |
| `shards vm run --kernel FILE [options]` | Boot a new machine |
| `shards vm restore DIR [--hold]` | Start a copy of the machine saved in `DIR`. `--hold` preloads it and waits for a line on stdin |
| `shards vm restore DIR [--hold] [-e …] [-w …] [-u …] -- COMMAND [ARG...]` | Run `COMMAND` in a copy of a template saved by `--rootfs` with `--snapshot-dir` |
| `shards version` · `shards help` | |

| Option | What it does |
|---|---|
| `--init FILE` | The static Linux binary to run as PID 1 |
| `--initrd FILE` | An initramfs, instead of `--init` |
| `--cmdline TEXT` | The kernel command line (default: `console=ttyS0 earlycon panic=-1`) |
| `--cpus N` · `--memory MIB` | Size (default: 1 CPU, 256 MiB) |
| `--disk FILE[:ro]` | A disk. Repeat for more |
| `--vsock PATH` | A vsock device. Host programs connect to the Unix socket `PATH` and send `CONNECT <port>`; the guest reaches host port P at `PATH_P`. A restored copy needs its own `PATH` |
| `--rootfs FILE -- COMMAND [ARG...]` | Boot into the EROFS image `FILE` and run `COMMAND` there, as `docker run` would. Its output and exit status are shards'. `--init` must be shards-init |
| `--rootfs FILE --snapshot-dir DIR` | Boot into `FILE` and save a template to `DIR` once the image is mounted, for `vm restore DIR -- COMMAND` |
| `-e` · `-w` · `-u` · `--hostname` · `-i` | With `--rootfs`: as for `docker run` |
| `--snapshot-dir DIR` | Save the machine to `DIR` when it asks, then exit. `--snapshot-then resume` keeps it running. Works with `restore` too |
| `--no-console` | Hide the console |

Exit codes: 0 for shutdown or snapshot, 1 for an error, 2 for bad usage, 3 when the guest
reboots. With `--rootfs`, the command's own, or 125–127 as for `docker run`. `SHARDS_LOG=debug` shows what shards is doing.

Pulled images, the chosen guest and saved microVMs are kept in `SHARDS_HOME`, if you set it;
otherwise in `shards` in your data directory (`~/Library/Application Support` on macOS, `~/.local/share` on Linux,
`%LOCALAPPDATA%` on Windows).

## Performance

Measured on an Apple M5 Max with macOS 26.4.1 at `e8ec2e6`: 100 runs each, 1 CPU, 256 MiB.
Details are in [docs/benchmarks.md](docs/benchmarks.md).

| Start | p50 | p99 |
|---|---:|---:|
| Restore, preloaded with `--hold` | **149 µs** | 197 µs |
| Restore, in a new process | 794 µs | 1.7 ms |
| Cold boot, to PID 1 | 21.2 ms | 22.2 ms |

Running a command in an image you have run before takes **3.4 ms** at p50 and 3.9 ms at
p99, start to exit (`shards run IMAGE exit 0`), where a boot takes 33.5 ms. That is 300
runs over 10 saved copies, on a Mac running other VMs. About a third of it is starting
the `shards` process, and another third is the command itself, inside the microVM.

Linux itself takes 18.6 ms of a cold boot. That is why shards restores snapshots, and why a
leaner kernel is coming.

Peak memory is 12.5 MiB for a restored machine, 16.4 MiB once it has run a command, and
59.8 MiB for a booted one, guest memory included.

Against Firecracker v1.17.0 on one host (GitHub's x86_64 runner), with the same kernel and
guest, 1 CPU and 128 MiB, 30 interleaved runs each:

| | shards | Firecracker |
|---|---:|---:|
| Boot to ready, p50 | **143 ms** | 147 ms |
| Boot to ready, p99 | **162 ms** | 228 ms |
| VMM memory | **2.4 MiB** | 4.5 MiB |

The guest kernel dominates boot there, under nested virtualization. Snapshot restores are
next.

## How it works

- **Two walls.** Agents in one microVM are kept apart like containers. The microVM keeps
  all of them away from your computer and from other microVMs: it has its own kernel and runs
  in its own process.
- **One OS per microVM.** Agents run on their microVM's OS instead of each bringing an image,
  so a snapshot holds one OS however many agents run on it.
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

Shards learned a great deal from [Firecracker], and its guest kernel starts from Firecracker's
microVM kernel configuration.

## License

MIT — © 2026 Hyperlight. See [LICENSE](LICENSE).

[Firecracker]: https://github.com/firecracker-microvm/firecracker
