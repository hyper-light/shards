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

- Three binaries, found beside each other. `shards` is the command: it links only std, `shards_ipc` and `shards_cmdline`, reads `run` and the container commands (`ps`, `wait`, `logs`, `stop`, `kill`, `rm`) itself and asks the daemon for the rest, serves `daemon stop`, execs `shards-vm` for `vm`, and `shardsd` for everything else. `shardsd` is the daemon, pulls and the guest. `shards-vm` runs one microVM per process, for `shards vm` and the daemon's templates and warm VMs; it links the VMM and nothing of `shardsd`'s (every VM process relocates its whole binary, M34), and on macOS it is the one that needs the hypervisor entitlement. Subcommands today: `shards vm run --kernel … [--init …] [--disk PATH[:ro]]… [--vsock PATH] [--snapshot-dir DIR]` and `shards vm restore DIR [--hold] [--vsock PATH]`. With `--rootfs IMAGE` (and `--init` = shards-init), `vm run … -- CMD` runs a command in an image as `docker run` does, and `--snapshot-dir` saves a template that `vm restore DIR -- CMD` runs commands from. `shards pull` and `shards run IMAGE` work as Docker's do; after `shards guest use --kernel … --init …`, repeated runs of an image restore its template (D25).
- `shards run` is a client of a per-`SHARDS_HOME` daemon that the first run starts and that serves runs from pools of warm VMs (D26), and keeps each run as a container (D27). `SHARDS_KERNEL` and `SHARDS_INIT` make a run boot those instead. `shards daemon stop` ends it and its runs; a daemon of another build does the same for the client that finds it. Its log is `daemon.log` in the home, and its socket `daemon.sock`, which processes reach relative to the home as their working directory.
- `cargo test --workspace --release` runs unit and E2E tests. E2E downloads a pinned kernel into `target/artifacts`.
  - On macOS, `scripts/hvf-run` (the cargo runner) ad-hoc signs each binary with `resources/hvf.entitlements`. Unsigned binaries fail with `HV_DENIED`. E2E tests run the copies `common::shards()`, `common::shardsd()` and `common::shards_vm()` place side by side in `target/e2e/`.
- Guest binaries are static musl, linked by `rust-lld`, so no cross toolchain is needed:
  `cargo build -p shards-init --profile guest --target <arch>-unknown-linux-musl`
  - The guest arch is the host arch.
  - Lint them with the same `--target`, because host builds compile only their stub.
- Lint every matrix target before pushing: `scripts/lint`, or `scripts/lint <triple>…`. `.github/workflows/ci.yml` lists the triples.
  - aws-lc-sys compiles C for each target, so this needs zig, LLVM (clang-cl, llvm-lib), NASM and cargo-xwin: `brew install zig llvm nasm` and `cargo install cargo-xwin`.
  - The first Windows lint downloads Microsoft's CRT and SDK.
- A process passing a socket by `SCM_RIGHTS` keeps its own descriptor until the receiver says it has it: XNU's collector flushes a socket in flight that no process holds (M24; the daemon's `TAKEN`).
- Compare two builds' run latency with `docs/research/measurements/build-ab/ab.py`, both restoring one template: restore costs differ from template to template by tens of µs (PM M29).
- VM tests print `SKIP:` and return where the host cannot run VMs (`vm::check_host`). `this_host_has_its_hypervisor_backend` pins which hosts must have a backend.
- When tests invoke cargo, call the rustup proxy on `PATH` with `DYLD_*` removed. Otherwise `rust-lld` cannot load `libLLVM`.
- `../linux` sits on case-insensitive APFS, which corrupts files whose names differ only by case. Build kernels inside a Linux VM.

## Where things are

- `docs/design/architecture.md`: decisions D1–Dn with evidence, the start-path budget, and the phased plan.
- `docs/research/`: literature reviews, plus `platform-measurements.md` for Hypervisor.framework ground truth.
- `crates/shards`: `shardsd` (`src/main.rs`), the thin `shards` command (`src/bin/shards`), `shards-vm` (`src/bin/shards-vm`, which includes the VM side's modules by path), and the real-VM E2E tests.
- `crates/vmm`: the VMM library.
- `crates/image`: layers, EROFS images, OCI documents and the image store.
- `crates/registry`: pulling from registries. It is the only crate with C (AWS-LC, vendored in `vendor/`, see `vendor/README.md`).
- `crates/init`: guest PID 1.
- `crates/testguest`: PID 1 of test VMs. Its library holds the data patterns the host tests share.
- `crates/abi`: constants shared by the VMM and the guest.
- `crates/cmdline`: command lines as the Docker CLI reads them, and its texts.
  - A command's flags are in `commands.rs`; the ones it serves are listed again in `scripts/docker-cli/oracle_test.go` (`served`). After changing either, run `scripts/docker-cli/generate`, which regenerates the tables and the real docker/cli's golden answers that the tests match byte for byte.
- `crates/ipc`: what the CLI, the daemon and warm VMs say to each other (`lib.rs`), and on Unix the transport, messages with file descriptors, plus `spawn`, which gives a child only the descriptors named for it (`unix.rs`).
