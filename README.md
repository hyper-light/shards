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
later, break something. Shards gives your agents a machine to do that in: a microVM with its
own Linux kernel, so nothing that happens inside it reaches your computer.

One machine holds as many agents as you like. Inside it, shards runs a runtime of its own in
the place containerd has on a Docker host. It takes Docker's commands and Compose files, so
your agents and the services they depend on run side by side, just as they would under
Docker. Underneath it is built differently, with no containerd, runc or containers, so that a
machine full of running agents can be saved and started again in well under a millisecond.
You describe and build these machines like Docker images. Your agents can:

- Start in a machine that is already set up, in well under a millisecond
- Work side by side in one machine, with the Docker commands and Compose files you already
  have
- Get a machine to themselves when they shouldn't share one
- Reach only the network, devices and files you allow, per agent and per machine
- Use a GPU when a job needs one
- Break anything without it reaching your computer

Shards is one binary, and it never needs root.

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

The first command boots Linux in a new machine, runs a program in it and shuts it down, in
28 ms from start to finish. The second starts a copy of a machine that was saved while it
was running, instead of booting a new one. That took 7 ms, and most of it was the `shards`
process itself starting and exiting. The saved machine runs the test program you will meet
in [Set up once, start many](#set-up-once-start-many).

> [!NOTE]
> The console output in this README was captured on an Apple M5 Max with macOS 26.4.1, from a
> release build at `e8ec2e6`, in a directory holding the pinned Linux 6.18 kernel as
> `vmlinux`.

> [!IMPORTANT]
> Shards has no release yet. Today you can boot Linux machines on a Mac with Apple silicon
> or on x86_64 Linux, and on the Mac, save a running machine and start copies of it. Building
> machines like Docker images, the Docker-compatible commands, the runtime that runs your
> agents inside each machine, per-agent network and device controls, and GPU support are still
> being built.
> [Where things stand](#where-things-stand) goes through each one.

## Install

Build it from source. You need Rust 1.98, which the toolchain file pins, so `rustup` picks it
up:

```sh
git clone https://github.com/hyper-light/shards && cd shards
cargo build --release -p shards
codesign -s - -f --entitlements resources/hvf.entitlements target/release/shards   # macOS only
cp target/release/shards ~/.local/bin/   # or anywhere on your PATH
shards --help
```

On a Mac, you need macOS 15 or later on Apple silicon. macOS lets only signed programs use
its virtualization, and the `codesign` line does that signing. It is an ad-hoc signature, so
you need no Apple developer account. If you skip it, shards stops with `HV_DENIED` when you
start a machine.

On Linux, you need an x86_64 machine and access to `/dev/kvm`, which usually means being in
the `kvm` group. If you don't have it, shards tells you. Build with
`--target x86_64-unknown-linux-musl` for a fully static binary that runs on any
distribution.

On arm64 Linux, Intel Macs and Windows, shards builds but can't start machines yet, and
`shards vm run` says so.

## Quickstart

A machine needs a Linux kernel and a first program to run. Shards doesn't ship its own kernel
yet, so borrow the one Firecracker tests with. Download it for your architecture, and check
that it is the same kernel shards' tests use:

```sh
arch=$(uname -m | sed s/arm64/aarch64/)
curl -fLo vmlinux https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260923-6f82ac4cf331-0/$arch/vmlinux-6.18.48
shasum -a 256 vmlinux
```

| Architecture | SHA-256 |
|---|---|
| aarch64 (arm64) | `a80108af80d9549b357ea7e00bd5c12f80686869541d135a8a67f6fe1ec3451e` |
| x86_64 | `9204218e8bcca6ac23848d74f45df2eb19d7f31e8277840a7d145a0df8b078d2` |

For the first program, use `shards-init`, a small program that comes with shards. It reports
how long Linux took to start, then shuts the machine down. Build it for Linux. Rust's own
linker does that, so you don't need a cross-compiler:

```sh
cargo build -p shards-init --profile guest --target $arch-unknown-linux-musl
cp target/$arch-unknown-linux-musl/guest/shards-init .
```

Then start a machine with 4 CPUs and 512 MiB of memory:

```console
$ shards vm run --kernel vmlinux --init shards-init --cpus 4 --memory 512 --cmdline "console=ttyS0 quiet"
shards-init: pid 1 running at uptime 0.014162s
[    0.018563] reboot: Power down
```

The machine's console is your terminal. What the machine prints shows up here, what you type
goes to it, and `Ctrl-A` then `x` stops it. Leave out `quiet` to watch Linux boot.

Any static Linux program can be the first program. Pass it with `--init`, or bring a whole
initramfs with `--initrd`. `--disk FILE` gives the machine a disk (`FILE:ro` for read-only),
which it sees as `/dev/vda`, the next one as `/dev/vdb`, and so on. Machines get 1 CPU and
256 MiB unless you ask for more.

## Set up once, start many

Booting Linux, installing what your agents need and starting their services all take time.
With shards you do that once. Save the machine when it's ready, and whenever you need another
one, start a copy of it that is already set up.

For now, the machine decides when it is ready: a program inside it asks shards to save it.
The test program in this repository asks as soon as it starts, so you can try the whole flow
with it. Build it the same way as `shards-init`:

```sh
cargo build -p shards-testguest --profile guest --target $arch-unknown-linux-musl
cp target/$arch-unknown-linux-musl/guest/shards-testguest .
```

Run it with `--snapshot-dir`. When it asks, shards saves the machine to that directory and
exits:

```console
$ shards vm run --kernel vmlinux --init shards-testguest --snapshot-dir snap \
    --cmdline "console=ttyS0 quiet shards_test=resume"
$ du -h snap/*
 43M	snap/memory
 20K	snap/state
```

The machine had 256 MiB of memory. Empty memory isn't stored, so the snapshot takes 43 MiB.

`shards vm restore snap` starts a copy, as in the example at the top. The copy picks up
exactly where the original asked to be saved, and the test program prints `PASS` and shuts
down. Start as many copies as you like:

- **Copies are cheap.** They share the snapshot's memory and use more only as they change it.
- **Copies don't collide.** Linux in each copy gets fresh randomness the moment it wakes up,
  so the keys, tokens and IDs it hands out after that differ from copy to copy. The tests
  check this. Anything a program drew before the snapshot is, of course, the same in every
  copy.
- **Clocks keep running.** Time inside a copy carries on from the moment the original was
  saved. The date and time of day are the exception for now. They are behind by however long
  the snapshot waited, until something in the machine sets them.

When every millisecond counts, get a copy ready before you need it. With `--hold`, shards does
all the loading up front, prints `shards-ready` and waits. The copy starts when a line
arrives on shards' input, and is running about 150 µs later:

```console
$ (sleep 1; echo) | shards vm restore snap --hold
shards-ready
SHARDS-TEST PASS
[    0.016274] reboot: Power down
```

> [!NOTE]
> Saving and restoring machines works on macOS today. Linux is next.

## Where things stand

| You want to | Today (2026-09-28) |
|---|---|
| Boot a Linux machine on a Mac with Apple silicon | Works, with as many CPUs as the Mac allows, disks and a console |
| Boot a Linux machine on x86_64 Linux | Works, with up to 254 CPUs (the borrowed kernel uses 64 of them), disks and a console. CI boots machines on every push, from both glibc and static musl builds |
| Save a running machine and start copies of it | Works on macOS; Linux is next |
| Run shards on arm64 Linux, an Intel Mac or Windows | Builds and passes CI, but can't start machines yet |
| Run commands in a running machine and talk to it from the host | Next |
| Describe and build machines like Docker images | Planned |
| Use Docker's commands (`run`, `build`, `ps`, `exec` and the rest) with shards | Planned. Today there are `shards vm run` and `shards vm restore` |
| Run agents side by side in one machine, with Docker's commands and Compose files | Planned |
| Choose each agent's and each machine's network, devices and permissions | Planned |
| Give a machine a GPU | Planned |

The goal is a machine you can use within 5 ms of asking for it, boot included, using less
memory than a Firecracker machine. The plan, and the evidence behind each design choice, is in
[docs/design/architecture.md](docs/design/architecture.md).

## Commands

| Command | What it does |
|---|---|
| `shards vm run --kernel FILE [options]` | Boot a new machine |
| `shards vm restore DIR [--hold]` | Start a copy of the machine saved in `DIR`. `--hold` gets it ready, then waits for a line on stdin |
| `shards version` · `shards help` | |

| Option | What it does |
|---|---|
| `--init FILE` | The static Linux program the machine runs first |
| `--initrd FILE` | An initramfs to boot with, instead of `--init` |
| `--cmdline TEXT` | The kernel command line (default: `console=ttyS0 earlycon panic=-1`) |
| `--cpus N` · `--memory MIB` | The machine's size (default: 1 CPU, 256 MiB) |
| `--disk FILE[:ro]` | A disk, read-only with `:ro`. Repeat it for more disks |
| `--snapshot-dir DIR` | When the machine asks, save it to `DIR` and exit. Add `--snapshot-then resume` to keep it running. Works with `restore` too |
| `--no-console` | Don't show the machine's console |

`shards` exits with 0 when the machine shuts down or is saved, 3 when it reboots, 1 when
something goes wrong, and 2 when the command line is wrong. Set `SHARDS_LOG=debug` to see what
shards is doing.

## Performance

Measured 2026-09-28 at `e8ec2e6` on an Apple M5 Max (18 cores, 128 GB, macOS 26.4.1, rustc
1.98.0), 100 runs each, with 1 CPU and 256 MiB, while other work used about 1.5 cores. The
commands and every earlier run are in [docs/benchmarks.md](docs/benchmarks.md).

### How fast do I get a machine?

| Starting a machine from | Until it's running | p50 | p99 |
|---|---|---:|---:|
| A copy prepared with `--hold` | after the request | **149 µs** | 197 µs |
| A snapshot, with `shards vm restore` | after the command starts | 794 µs | 1.7 ms |
| Nothing, by booting Linux with `shards vm run` | after the command starts, until its first program runs | 21.2 ms | 22.2 ms |

Most of a boot is Linux starting up: 18.6 of those 21 ms. That is why shards starts machines
from snapshots, and why it will build a leaner kernel of its own. Set `SHARDS_TIMING=1` and
shards prints the same timings for your own runs.

### How much memory does a machine use?

A copy started from a snapshot peaked at 12.5 MiB, and a freshly booted machine at 59.3 MiB.
Both count everything the process touched, including the machine's own memory, so shards'
own share is smaller. Measuring that share on its own comes with the Firecracker comparison.

### How does it compare with Firecracker?

Firecracker promises to reach `/sbin/init` within 125 ms of its start call, with at most
5 MiB of memory overhead, for a machine with 1 CPU and 128 MiB. Those are its published
limits on its own hardware, not measurements next to shards. The side-by-side comparison will
run on one Linux host, once shards can save and restore machines there.

## How it works

- **Why nothing an agent does reaches you.** Every machine is a real virtual machine with its
  own Linux kernel, run by its own `shards` process. A container on your computer runs on
  your kernel; a shards machine runs its own. A crash, a runaway process or an exploit stays
  inside its machine, and agents that must share nothing get a machine each.
- **Why a machine is ready in microseconds.** Booting Linux takes about 20 ms, so shards
  boots once, saves the machine, and starts copies of it. A copy prepared with `--hold` has
  already loaded everything, so starting it only means letting it run.
- **Why copies are cheap.** Copies read the snapshot's memory in place instead of each
  loading their own, and get private memory only for what they change.
- **Why it never needs root.** Shards uses the virtualization your OS already offers to
  ordinary users: `/dev/kvm` on Linux, Hypervisor.framework on macOS.
- **Why bad input gets an error, not a crash.** Shards checks everything it reads, from
  command-line flags to snapshot files, and tells you what's wrong instead of crashing. Its
  lints reject code that could crash, and CI runs them on every push.
- **Why it runs where you do.** It is one codebase for Linux, macOS and Windows on x86_64 and
  arm64. CI builds all eight combinations on every push, and boots real machines on the ones
  that can run them.

The design, with the research and measurements behind each decision, is in
[docs/design/architecture.md](docs/design/architecture.md).

## Documentation

| Doc | What's in it |
|---|---|
| [Architecture](docs/design/architecture.md) | How shards is designed, each decision with its evidence, and the plan |
| [Benchmarks](docs/benchmarks.md) | Every measurement, with its command, machine and revision |
| [Research](docs/research/) | The papers, specifications and measurements the design rests on |
| [Brand](docs/assets/brand/README.md) | The logo and how it is drawn |

## Contributing / development

```sh
cargo build -p shards
cargo test --workspace --release       # includes tests that boot real machines
cargo clippy --workspace --all-targets --target <triple> -- -D warnings
cargo bench -p shards --bench boot     # and --bench restore
```

The tests boot real machines on your computer, so they need what `shards` needs (see
[Install](#install)). They download the kernel and build the test programs themselves, and on
macOS they sign every binary they run. On a computer that can't run machines, those tests
print `SKIP:` and pass. The project's rules are in [CLAUDE.md](CLAUDE.md).

## Acknowledgements

Shards learned a great deal from [Firecracker], and its tests boot Firecracker's CI kernels.

## License

MIT — © 2026 Hyperlight. See [LICENSE](LICENSE).

[Firecracker]: https://github.com/firecracker-microvm/firecracker
