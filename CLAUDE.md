# shards

An all-in-one, rootless microVM platform for agents:

- our own VMM (Hypervisor.framework on macOS, KVM on Linux)
- microVMs specified and built like Docker images
- a Docker drop-in CLI
- inside every VM, our own runtime in containerd's place: many agents per VM, Docker- and Compose-compatible at the interface, but no containerd, runc or containers underneath (a different implementation, built for snapshot-speed starts)

Targets: request → usable in **under 5 ms, boot included**, and less memory per VM than Firecracker.

## Rules

- **One binary.** `shards` is the only file to install and run: every command, the daemon included, one CLI. Never ship, install or document anything beside it. The VM process and the network process are separate *processes* (per-VM memory, PM M34; macOS App Sandbox, D30) but not separate deliverables: `shards` carries them and writes them out once per build (D36).
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

- One binary, `shards` (D36). `src/main.rs` is its root: the command line (`src/cli/`) reads `run`, the container and image commands as the Docker CLI reads them and asks the daemon for them, serves `daemon stop`, reads shards' own grammar (`shards ACTION THING`, `cmdline/src/grammar.rs`), and becomes the VM process for `run --kernel` and `restore`; every other command is `main.rs`'s `shardsd()`, in-process: the daemon (which `shards run daemon` runs as), builds, the guest, the grants broker (`shards grants`). It links no framework that loads at launch: Security, CoreFoundation (`crates/apple`) and Hypervisor.framework (`vmm` `hv::hvf::ffi`) are bound when first called, which keeps a launch at 2.5 ms (PM M113). The VM process (`crates/vm-process`, binary `shards-vm`, which includes the VM side's modules from `crates/shards/src` by path) and the network process (`crates/net-process`, `shards-net`) are built by `crates/shards/build.rs` with a nested cargo into `target/helpers`, embedded, and written out by `src/helpers.rs` to `~/Library/Caches/shards/helpers/<digest>/` (or `$XDG_CACHE_HOME/shards/…`) the first time a build needs them. The VM process links the VMM and nothing of the daemon's (every VM maps its whole binary, M34), and on macOS it alone runs in App Sandbox with the hypervisor entitlement: it opens only what its spawner grants it over `--grants FD` (the daemon for its VMs, a `shards grants` broker that `shards run --kernel` and `shards restore` start; D30, M71). `SHARDS_VM_BINARY` (an absolute path) names a VM binary to start instead, for VMM work and tests. Booting a kernel directly: `shards run --kernel … [--init …] [--disk PATH[:ro]]… [--vsock PATH] [--snapshot-dir DIR]` and resuming a snapshot: `shards restore DIR [--hold] [--vsock PATH]`. With `--rootfs IMAGE` (and `--init` = shards-init), `run --kernel … -- CMD` runs a command in an image as `docker run` does, and `--snapshot-dir` saves a template that `restore DIR -- CMD` runs commands from. `shards pull`, `shards push` and `shards run IMAGE` work as Docker's do, and `shards build` as `docker build` does (D33). Runs boot shards' pinned kernel (`src/kernel.rs`, downloaded on first need) and the shards-init `shards` carries, or the guest `shards configure guest --kernel … --init …` records (D28), and repeated runs of an image restore its template (D25).
- `shards run` is a client of a per-`SHARDS_HOME` daemon that the first run starts and that serves runs from pools of warm VMs (D26), and keeps each run as a container (D27). `SHARDS_KERNEL` and `SHARDS_INIT` make a run boot those instead. `shards stop daemon` ends it and its runs; a daemon of another build does the same for the client that finds it. Its log is `daemon.log` in the home, and its socket `daemon.sock`, which processes reach relative to the home as their working directory.
- `cargo test --workspace --release` runs unit and E2E tests. E2E downloads a pinned kernel into `target/artifacts`.
  - On macOS, `crates/shards/build.rs` ad-hoc signs the carried VM process with `resources/vm.entitlements` and `-o runtime` (App Sandbox, the hypervisor, Hardened Runtime) before embedding it, and `scripts/hvf-run` (the cargo runner) signs test binaries that create VMs with `resources/hvf.entitlements`. Unsigned VM processes fail with `HV_DENIED`; one outside App Sandbox starts no VM. E2E tests run the copy of `shards` that `common::shards()` places in `target/e2e/`; `common::shards_vm()` is the carried VM process (`SHARDS_HELPERS_CARRIED`).
- `crates/shards/build.rs` builds shards-init for `shards` to embed, with a nested cargo for the host arch's musl target (`SHARDS_INIT_BINARY`, an absolute path, names a prebuilt one), and the VM and network processes (`SHARDS_HELPERS_DIR` names prebuilt ones; `SHARDS_HELPERS=skip`, which `scripts/lint` sets, carries none, for check builds of other targets).
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
- `crates/shards`: the one binary (`src/main.rs`, the command line in `src/cli/`, the helpers it carries in `src/helpers.rs`), and the real-VM E2E tests. `crates/vm-process` and `crates/net-process`: the VM and network processes it carries. `crates/apple`: Apple's frameworks, bound when first used. `crates/tui`: how shards looks on a colour terminal (hyperlight's design).
- `crates/vmm`: the VMM library.
- `crates/image`: layers, EROFS images, OCI documents and the image store.
- `crates/registry`: pulling from registries. It is the only crate with C (AWS-LC, vendored in `vendor/`, see `vendor/README.md`).
- `crates/init`: guest PID 1.
- `crates/testguest`: PID 1 of test VMs. Its library holds the data patterns the host tests share.
- `crates/abi`: constants shared by the VMM and the guest.
- `crates/cmdline`: command lines as the Docker CLI reads them, and its texts.
  - A command's flags are in `commands.rs`; the ones it serves are listed again in `scripts/docker-cli/oracle_test.go` (`served`). After changing either, run `scripts/docker-cli/generate`, which regenerates the tables and the real docker/cli's golden answers that the tests match byte for byte.
  - `build` is buildx v0.37.1's, as `docker build` runs the plugin: its served flags are listed again in `scripts/buildx/oracle_test.go`, and `scripts/buildx/generate` regenerates buildx's answers (`tests/buildx.json`).
- `crates/dockerfile`: Dockerfiles as BuildKit reads and plans them, for `shards build`: the parser, the shell-like lexer, typed instructions, and the plan (Dockerfile2LLB's LLB graph, image config and build checks).
  - Held byte for byte to BuildKit's own code by `tests/oracle.rs`, against what `scripts/dockerfile/generate` (pinned to moby/buildkit dockerfile/1.27.1) records BuildKit making of `testdata/corpus` (plans of `corpus/plan`, with `images.json`'s fake base images), `lex-cases.json`, `configs.json` and `sizes.json`; rerun it after changing any. Each deliberate difference is in `testdata/deviations.json`, with its reason.
- `crates/build`: what `shards build` does to files: BuildKit's file actions on in-memory snapshots that change as Linux would (`vfs.rs`), fsutil's copy, and the layers BuildKit's overlay differ writes.
  - Held byte for byte to BuildKit by `tests/oracle.rs`, against what `scripts/build/generate` records BuildKit's own backend and differ making of `testdata/ops.json`, run as root on overlayfs in a privileged Linux container (Docker), and by `tests/context.rs`, against fsutil's own walk of `testdata/contexts.json`'s tree on this host. Rerun it after changing the cases.
- `crates/template`: Go's text/template with the Docker CLI's functions, for `--format`; held to Go by `scripts/template/generate`.
- `crates/archive`: tar archives as moby/go-archive makes and unpacks them, for `export` and `cp`, in the guest and on the host; held to go-archive by `scripts/archive/generate`.
- `shards top` lays a microVM's processes out as procps-ng 4.0.2's `ps` (`crates/shards/src/daemon/top.rs`), from shards-init's `/proc` dump; held to real procps by `scripts/top/generate`.
- `crates/ipc`: what the CLI, the daemon and warm VMs say to each other (`lib.rs`), and on Unix the transport, messages with file descriptors, plus `spawn`, which gives a child only the descriptors named for it (`unix.rs`).
