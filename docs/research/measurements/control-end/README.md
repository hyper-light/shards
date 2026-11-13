# control-end: where agent control terminates

Plan step 10 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): end a microVM's control in the guest,
each microVM a QUIC peer of its own, or at a host endpoint that carries each stream on to its
microVM over vsock. This is the harness that measures both. ROOT runs the measurements and
writes the PM entry and the D-entry (R14). This file says what the harness does, every knob, and
the command for each host.

## The arms

| arm | where TLS ends | the path a byte takes from the controller | the microVM |
|---|---|---|---|
| `g` | in the guest: `control-end guest quic`, a QUIC endpoint with its own key made in the guest | controller → host UDP → the run's network process (shards-net) → virtio-net → guest UDP 4433 | a daemon run of an image holding the guest program, under shards-init, its port published on 127.0.0.1 (`-p 127.0.0.1::4433/udp`) |
| `g0` | none: arm g's baseline | — | the same runs, the program idle (`guest idle`), nothing published |
| `h` | at the host: `control-end endpoint`, one process and one key for every microVM | controller → host UDP → endpoint → AF_UNIX → the VM process's vsock device (`CONNECT 5000`) → guest vsock 5000 | booted directly (`shards run --kernel K --init GUEST --vsock S`, D215's vehicle, the one Hex boots by today), the guest program as PID 1 serving vsock |
| `h0` | none: arm h's baseline | — | the same microVMs, PID 1 idle |

Every endpoint is D180's QUIC: shards-remote's driver (`net::Driver`), its server and client
contexts (`tls::server`, `tls::client`), its raw Ed25519 identities and its transport (100 s idle,
a keep-alive every 2.5 s). Its provider is hyper-quic's node provider (aws-lc-rs, the hybrid
ML-KEM key-exchange groups) narrowed to TLS13_AES_256_GCM_SHA384, the person's 2026-11-11
decision; tls.rs's own tests hold the negotiated suite. Each side pins the other's key: the
controller pins the endpoint's SHA-256, and the endpoint takes only the controller's.

The two arms need two vehicles. Only a daemon run gets a network process and published ports
(arm g). Only a direct boot gets a vsock socket the host can `CONNECT` to: a daemon run's vsock
is its daemon's, and Docker's default seccomp profile, which shards-init applies to a run's
command, refuses `socket(AF_VSOCK)` (crates/seccomp oracle.rs). So each arm is compared to
its own baseline on its own vehicle. What a VM costs for its vehicle (shards-init and a
template restore, or a cold direct boot) is subtracted from each arm.

Each arm keeps the controller side the same. Each microVM gets one QUIC connection, on a driver
and thread of its own, as a fleet of separate controllers would. The guest program is one
static musl binary in every arm. In arm h it is the stand-in for shards-init's vsock relay,
which a production arm h would use. Its first chunk is the stream's 13-byte header, so both
arms' guests read the same bytes. The host endpoint is the stand-in for the daemon's managed
owner endpoint: one driver thread for every connection, and two threads per stream for its
vsock leg. Each direction holds one 64 KiB chunk in flight (one vsock packet,
`VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`, as managed-engine-relay-throughput-v1 derived).

## Method

Each round runs every arm asked for, rotating the order round by round (interleaved A/B), on
microVMs booted for that arm and ended after it. For each arm the harness samples every
process Shards runs for it: the daemon, its spawner and the runs' VM and network processes for
g/g0; each `shards run --kernel` process and its children, plus the endpoint, for h/h0. It
samples them at these points:

| point | when |
|---|---|
| S0 | every microVM has said it is ready (`control-end ready` / `control-end idle`), nothing connected |
| S1 | one idle window (`--idle-s`) later |
| S2 | every microVM's controller connection made at once, each with its first round trip back (g, h) |
| S3 | one idle window later, connections held |
| S4 | after the round trips |
| S5 | after the transfers |
| R1 | after a second connection to every microVM from new controller connections (the first ones closed) |

What it reports, per arm. The pooled samples are reported as n, p50, p90, p99 and max. Per-run
values have one sample per arm run, so n is `--rounds`.

- **Memory per VM, idle**: the sum over the arm's processes at S3 (g, h) or S1 (g0, h0), divided
  by `--vms`. Linux counts PSS (`smaps_rollup`, falling back to VmRSS where unreadable); macOS
  counts physical footprint (`proc_pid_rusage` `ri_phys_footprint`, as M49's fleet measurement
  reads it); Windows counts the working set. Guest RAM the guest touched is in its VM
  process's count. The summary's last table gives g − g0 and h − h0: what ending control in
  each place costs a VM. `memory per VM, nothing connected` (S1) separates the endpoint's
  standing cost from what connections add.
- **Handshake CPU per VM, at density**: every microVM's connection is made at once. The arm's
  processes' CPU over the window, minus what the same processes use in the same window idle
  (the S0→S1 rate), divided by `--vms`, by class: `vm` (VM processes, guest time included),
  `net`, `endpoint`, `shards` (daemon, spawner, launchers), `all`. Linux reads each process's
  CPU-time clock (`clock_getcpuclockid`, counting KVM guest time). macOS reads `proc_pid_rusage`
  user+system time, Mach time converted. Windows reads `GetProcessTimes`. All three count ended
  threads too. `first` is each microVM's first connection, cold code and keys included.
  `reconnect` is the second, which a restarted controller makes.
- **Handshake, client side**: each connection's handshake wall time (connect to the driver's
  `Connected`), the controller thread's CPU over it (its thread CPU clock), and connect to
  first round trip back. In arm h, that round trip includes the endpoint's vsock `CONNECT`.
- **Guest handshake CPU, its process** (g): the guest program's own CPU from serving to its first
  connection's handshake (`getrusage`). It leaves out guest-kernel and host-side work, which
  `host vm` and `host net` hold.
- **Round trip**: `--pings` round trips of `--ping-bytes` on an open stream. Every connection
  pings at once, one round trip outstanding each, so `--vms 1` gives the uncontended round trip.
  Timed on the controller's driver thread, from handing the bytes to QUIC to having them all back.
- **Throughput**: `--bulk-rounds` uploads, then downloads, of `--bulk-bytes` each on
  `--bulk-vms` connections at once. Each stream's MiB/s is timed from its first byte to the
  guest's 8-byte acknowledgment (upload) or to the stream's end (download). All streams together
  are timed from the ask to the last answer.
- **Idle CPU per VM**: the arm's processes' CPU over S0→S1 (nothing connected) and S2→S3
  (connections held, D180's keep-alives every 2.5 s), as a percentage of one CPU, per VM.
- **Guest program memory**: the guest program's RssAnon and PSS as the guest counts them (`STATS`).

Load: each arm's start and end record the host's load averages (Unix), or on Windows its CPUs'
busy share over the arm. `--spin N` adds N busy threads for the whole run, a reproducible
loaded condition.

## Knobs

| option | default | what |
|---|---|---|
| `--shards PATH` | (needed) | the `shards` under test, built from the checkout this harness is in |
| `--guest PATH` | (needed) | this crate built for `<arch>-unknown-linux-musl` (`--profile guest`) |
| `--out DIR` | (needed) | records, summary, logs, the home (`DIR/home`), image archive, sockets; a fresh one per run |
| `--arms LIST` | `g0,g,h0,h` | which arms, comma-separated; the order rotates by round |
| `--vms N` | 8 | microVMs per arm: the density |
| `--rounds N` | 3 | rounds of every arm |
| `--cpus N` | 1 | each microVM's vCPUs |
| `--memory-mib N` | 512 | each microVM's memory |
| `--idle-s N` | 10 | each idle window |
| `--pings N` | 100 | round trips per connection |
| `--ping-bytes N` | 64 | bytes each way per round trip |
| `--bulk-bytes N` | 16777216 | bytes per transfer |
| `--bulk-vms N` | 1 | connections transferring at once |
| `--bulk-rounds N` | 3 | uploads and downloads per connection per arm |
| `--ready-s N` | 120 | how long a microVM may take to say it is ready |
| `--spin N` | 0 | busy threads for the whole run |
| `--kernel PATH` | the home's pinned kernel | arm h's kernel; by default the one `shards version` pins, as the daemon stored it in `DIR/home/guest` on its first run, so both arms boot one kernel |
| `--sock-dir DIR` | `DIR/s` | the VMs' vsock sockets; their paths must fit a socket address (103 bytes) |
| `--harness-rev REV` | `unknown` | this harness's revision, for the record |

The daemon runs in the foreground as the harness's child (`shards daemon`, `SHARDS_POOL=0`, so
each run's microVM is its own), with a docker config of its own and no local image store, as
the E2E tests run it.

## Outputs

- `DIR/summary.txt` (also printed): host, OS, kernel, CPU, the `shards` and harness revisions,
  the knobs, then a table of every metric per arm, then each arm's cost over its baseline.
- `DIR/records.jsonl`: one JSON object per line, kinds `host`, `arm` (start and end, with load
  and every per-run value), `vm`, `process` (every sampled process at every point: pid, name,
  class, CPU ns, memory, alternate memory, or `unread` where its counts could not be read),
  `handshake`, `rtt` (every sample), `bulk`, `stats` (the guest's line), `failed`.
- `DIR/logs/`: the daemon's output, each directly booted VM's console, the endpoint's stderr.

## Build

From a checkout of the revision under test. The harness is a workspace of its own and builds
against that checkout's crates and vendored aws-lc. Build `shards` from the same checkout:
`cargo build --profile test-release -p shards --bin shards`. On macOS its VM process is signed
by build.rs.

```sh
cd docs/research/measurements/control-end
root=$(cd ../../../.. && pwd)
cargo build --release                       # the host side: target/release/control-end
arch=$(uname -m | sed s/arm64/aarch64/)     # the guest's arch is the host's
triple=$arch-unknown-linux-musl; var=$(echo $triple | tr - _)
env CC_$var=$root/scripts/zig-cc AR_$var=$root/scripts/zig-ar ZIG_TARGET=$arch-linux-musl \
  cargo build --profile guest --target $triple   # target/$triple/guest/control-end
```

The guest build needs zig (`scripts/install-zig`), as Shards' own guest binaries do (CLAUDE.md).

## Run

Each command takes `--vms`, `--rounds` and the other knobs as the campaign asks. `REV` is
`git rev-parse HEAD` of the checkout.

**Linux, KVM (adacachy)**, in a directory of its own (`~/g121-tmp/s10-*`):

```sh
out=~/g121-tmp/s10-run-$(date +%s)
target/release/control-end run --shards $root/target/test-release/shards \
  --guest target/x86_64-unknown-linux-musl/guest/control-end --out $out \
  --vms 32 --rounds 5 --harness-rev REV
```

**macOS, HVF (adas-mac-studio.local-2)**. Run it as a launchd job of the user's session,
because an agent's session clamps every thread to utility QoS (PM M317). The daemon and the
VMs the harness starts inherit the job's QoS:

```sh
out=$HOME/s10-run-$(date +%s); mkdir -p $out
$root/docs/research/measurements/macos-sched/launchd-run.sh $out/launchd \
  $PWD/target/release/control-end run --shards $root/target/test-release/shards \
  --guest $PWD/target/aarch64-unknown-linux-musl/guest/control-end --out $out \
  --vms 32 --rounds 5 --harness-rev REV
cat $out/summary.txt    # the job's own output is in $out/launchd.log
```

**Windows, WHP (jupiter)**, in kiln's bash. Put the tools a shards build needs there (zig 0.16,
NASM) and `/c/Users/corph/.cargo/bin` first on `PATH`, and keep `CARGO_TARGET_DIR` outside the
synced checkout. The C compiler for the guest is the zig script run by `sh`, as the E2E tests
run it (crates/shards/tests/common `guest_cargo`):

```sh
export CARGO_TARGET_DIR=~/s10-cargo
cargo build --release
env CC_x86_64_unknown_linux_musl="sh $root/scripts/zig-cc" AR_x86_64_unknown_linux_musl="sh $root/scripts/zig-ar" \
  ZIG_TARGET=x86_64-linux-musl cargo build --profile guest --target x86_64-unknown-linux-musl
w() { cygpath -w "$1"; }    # the harness takes Windows paths there
out=$(w ~)\\s10-run-$(date +%s)
"$CARGO_TARGET_DIR/release/control-end.exe" run --shards "$(w SHARDS_EXE)" \
  --guest "$(w $CARGO_TARGET_DIR/x86_64-unknown-linux-musl/guest/control-end)" --out "$out" \
  --vms 32 --rounds 5 --harness-rev REV
```

Here `SHARDS_EXE` is a `shards.exe` built from the same revision for jupiter. Arm h's endpoint
reaches each VM's vsock socket over AF_UNIX (`shards-afunix`), as the E2E tests do there.

## Tests and lint

`cargo test` runs the light tests:

- the header, fingerprint, record and percentile codecs, and the image's ustar checksums;
- `loopback.rs`, both arms' protocol paths on loopback with no microVM. Arm g's endpoint is
  served in-process. Arm h's endpoint relays to a stand-in VM socket that answers `CONNECT` as
  the VMM's muxer does and serves each stream with the guest's own `answer`. Each does connect,
  3 round trips, a 1 MiB upload and download, and the guest's counts. A controller of another
  key is refused.

Lint: `scripts/lint.sh [TARGET...]` from a Mac, as the workspace's scripts/lint: `cargo clippy
--all-targets -- -D warnings` under the workspace's lints (no unwrap, expect, panic, indexing or
printing) for each of resources/supported-targets.txt, macOS natively, Linux through zig,
Windows through cargo-xwin, then `cargo test`.

## Record

- Functional smoke, not a measurement (no numbers are recorded from it): adacachy (Linux
  7.2.9-2-cachyos, AMD Ryzen 9 9950X3D2, KVM). The harness was built from 1e1d306's clean
  tree; later commits change only this file and scripts/. It ran `shards` test-release at
  87bf86b31 with `--vms 2 --rounds 1 --idle-s 2 --pings 20 --bulk-bytes 4194304
  --bulk-rounds 1`. Every phase of every arm ran: 4 first and 4 reconnect handshakes, every
  round trip, transfer and guest count. There were 0 failures and 0 unreadable processes, and
  every process ended with the run.
- Lint and light tests at 3bbbf33 on adas-mac-studio.local-2 (`scripts/lint.sh`, zig 0.16.0,
  cargo-xwin): clippy passes on all 7 supported targets, and `cargo test` passes 8 of 8 on
  macOS arm64. On adacachy, at 1e1d306, it passes 8 of 8 on Linux x86_64.
- What the smoke found and fixed: quinn-proto reports a peer-opened stream's first bytes only
  through `Opened`, so both endpoints read in `opened`, as Shards' own server does; a daemon
  run's command is PID 1 of its own PID namespace, so the guest program takes PID 1's role only
  when its kernel's command line names a mode (`control_end=`).
- The measurements, ROOT's, are PM M540: HVF r411 on adas-mac-studio.local and KVM on
  adacachy, 32 microVMs and 5 rounds each, the harness at 66113379c. ROOT's summaries are in
  `results/` as ROOT wrote them: `hvf-r411-summary.md` (p50s, and the round trip's p99),
  `kvm-adacachy-summary-table.md` (n, p50, p90, p99 and max) and `kvm-adacachy-head.txt` (the
  host and the knobs). The raw `summary.txt` and `records.jsonl` are still on those hosts.
  WHP's run is owed. D238 records the decision.

## Limits

- The arm-h guest program and endpoint stand in for shards-init's relay and the daemon's
  managed owner endpoint. Their thread handoffs (the driver to a vsock writer, a vsock reader to
  the driver's command channel) are the relay's cost here, not necessarily the one that
  endpoint has.
- One QUIC connection per microVM in both arms. An arm-h endpoint could carry every microVM
  on one connection and pay one handshake for all. Its cost is the per-connection one measured
  here, times one.
- Whether each host lets the harness read every VM process's counts is checked per process.
  One it may not read is recorded as `unread` and left out of every sum. That has not been
  tried on Windows, whose VM processes run in AppContainers (D141).
- The harness compiles and lints for each host it runs on. Real-VM runs on HVF and WHP are
  ROOT's.
