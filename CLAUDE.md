# shards

An all-in-one, rootless microVM platform for agents:

- our own VMM (Hypervisor.framework on macOS, KVM on Linux)
- microVMs specified and built like Docker images
- a Docker drop-in CLI
- inside every VM, our own runtime in containerd's place: many agents per VM, Docker- and Compose-compatible at the interface, but no containerd, runc or containers underneath (a different implementation, built for snapshot-speed starts)

Targets: request → usable in **under 5 ms, boot included**, and less memory per VM than Firecracker.

## Rules

- **Platform- and architecture-agnostic.**
  - shards runs on Linux (glibc and musl), macOS and Windows, on x86_64/amd64 and aarch64/arm64: the 8-target matrix CI lints and tests.
  - Hypervisor specifics live behind `hv` (KVM, Hypervisor.framework, WHP), guest-architecture specifics behind `arch`, and OS specifics behind `platform`. Everything else is written once.
  - Configs, builds, optimizations and tests cover every architecture. Use OCI platform names (`amd64`, `arm64`) wherever users see platforms.
- **Panic-free production code.** Every fallible step returns an error, and the caller handles it.
  - Use `?`, `ok_or`, `.get()`, checked arithmetic on untrusted sizes, `thread::Builder::spawn`, `env::args_os`, and poison-tolerant locks.
  - Write console output with `let _ = writeln!(...)`.
  - Tests may unwrap and assert.
  - Workspace clippy lints enforce this. `cargo clippy --workspace --all-targets` stays clean.
- **Evidence behind every decision.** Cite peer-reviewed papers, specs, official docs, source code, or our own measurements. `docs/design/architecture.md` records each decision next to its evidence.
  - When evidence is missing, measure it.
  - Commit the harness under `docs/research/measurements/`, and record method and numbers in `docs/research/platform-measurements.md`.
- **Real VMs in tests.** E2E tests boot real guests on the host hypervisor.
  - Every performance claim comes from a committed benchmark.
  - Benchmarks report n, p50, p90, p99 and max, with host, OS and revision.
- **Lean code.** Write only what the current milestone needs. Match the surrounding style.
- **Commit and push** after each tested milestone, on `dev`.

## Commands and gotchas

- One binary, `shards`, with subcommands. Today: `shards vm run --kernel … [--init …] [--disk PATH[:ro]]… [--vsock PATH] [--snapshot-dir DIR]` and `shards vm restore DIR [--hold] [--vsock PATH]`. With `--rootfs IMAGE` (and `--init` = shards-init), `vm run … -- CMD` runs a command in an image as `docker run` does, and `--snapshot-dir` saves a template that `vm restore DIR -- CMD` runs commands from.
- `cargo test --workspace --release` runs unit and E2E tests. E2E downloads a pinned kernel into `target/artifacts`.
  - On macOS, `scripts/hvf-run` (the cargo runner) ad-hoc signs each binary with `resources/hvf.entitlements`. Unsigned binaries fail with `HV_DENIED`.
- Guest binaries are static musl, linked by `rust-lld`, so no cross toolchain is needed:
  `cargo build -p shards-init --profile guest --target <arch>-unknown-linux-musl`
  - The guest arch is the host arch.
  - Lint them with the same `--target`, because host builds compile only their stub.
- Lint every matrix target before pushing: `cargo clippy --workspace --all-targets --target <triple> -- -D warnings`. `.github/workflows/ci.yml` lists the triples.
- VM tests print `SKIP:` and return where the host cannot run VMs (`vm::check_host`). `this_host_has_its_hypervisor_backend` pins which hosts must have a backend.
- When tests invoke cargo, call the rustup proxy on `PATH` with `DYLD_*` removed. Otherwise `rust-lld` cannot load `libLLVM`.
- `../linux` sits on case-insensitive APFS, which corrupts files whose names differ only by case. Build kernels inside a Linux VM.

## Where things are

- `docs/design/architecture.md`: decisions D1–Dn with evidence, the start-path budget, and the phased plan.
- `docs/research/`: literature reviews, plus `platform-measurements.md` for Hypervisor.framework ground truth.
- `crates/shards`: the CLI, and the real-VM E2E tests.
- `crates/vmm`: the VMM library.
- `crates/init`: guest PID 1.
- `crates/testguest`: PID 1 of test VMs. Its library holds the data patterns the host tests share.
- `crates/abi`: constants shared by the VMM and the guest.
