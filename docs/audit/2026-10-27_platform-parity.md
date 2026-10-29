# Platform parity (2026-10-27)

The user's criterion: every feature of shards built for Windows, macOS and Linux, on amd64
and arm64, macOS on x86_64 aside (Intel Macs are out, 2026-10-13). This audit finds where the
existing work falls short of it.

## How found

- `scripts/audit/platform-parity.py crates` lists each item a source file compiles for some
  hosts alone, a `#[cfg(...)]` naming hosts before a fn, mod, struct, enum, impl, const,
  static, type, use or trait, with no item of its name compiled for the other hosts in that
  file. It found 494 such items, test code left out: 278 `unix`, 86 Linux's, 70 Windows', 46
  macOS's, 14 others.
- What a user is refused on one host: each message of the host code that names a host
  (`grep` of `on Windows`, `on macOS`, `on Linux`, `not ... yet` in crates/shards, vmm,
  vm-process, net-process, cmdline, ipc).
- D13's status table, and each D-entry's notes of a host it leaves out.

The guest's crates, `init`, `testguest` and `server` (the in-VM server), are Linux's by
nature: each runs inside a microVM's Linux guest, on every host. Their `target_os = "linux"`
items are not gaps.

## The gaps

| # | What | Hosts without it | Evidence |
|---|---|---|---|
| 1 | The daemon, and all it serves. That is `shards run IMAGE`, containers and their every command (create, start, stop, rm, ps, logs, exec, attach, cp, top, stats, update, wait, kill, inspect), images run, volumes and binds, networks, published ports, log drivers, names, warm pools, the Engine API, compose, events, and the CLI's client, listings, terminal UI, `cp`, `save` and `show` | Windows (amd64; arm64 has no backend, row 5) | main.rs:93-96 "the daemon needs Unix sockets, which shards does not support on this platform yet"; cli/mod.rs:592 "container commands need the daemon"; main.rs:17-66 `containers`, `daemon`, `local_store`, `logging`, `names`, `networks`, `segments`, `share`, `volumes`, `working_set` are `cfg(unix)`; cli/mod.rs:15-29 `client`, `cp`, `listing`, `look`, `screens`, `show` likewise; architecture.md:4282 "No daemon of shards' runs on Windows yet", :9753 "No published ports yet", :7897 "etwlogs ... waits for shards' daemon on Windows" |
| 2a | Snapshots and restores: `--snapshot-dir`, `shards restore`, templates (D25), warm pools (D26) | Windows amd64 (D140) | D13 "no snapshots yet (D140)"; grant_answer.rs:406 "a microVM on Windows takes no snapshot yet"; grant.rs:97; vmm platform/windows.rs:559-630 (a snapshot's files, directories, prefetch and working sets "not ... on Windows") |
| 2b | The same | Linux arm64 (D154) | D13 "no snapshots yet (D154)" |
| 3 | Host directories shared into a microVM (volumes and binds, D38: the share process and its FUSE server) and their DAX windows (D189) | Windows | virtio/fs/mod.rs: `server`, `host`, `headroom`, `window` are `cfg(unix)`; window_none.rs and vm/mod.rs:764 "a DAX window is not built on Windows yet (D189)" (slates-d7 holds the WHP window, its step 6) |
| 4 | Memory a microVM gives back: free page reporting (D151), budgets (D148), the host's pressure watched and given back to (D188) for runs and builders | Windows | platform/pressure.rs Linux's and macOS's alone; build/mod.rs:48 `give` `cfg(unix)`; D188 "The daemon has no other platform yet" |
| 5 | A hypervisor backend | Windows arm64 | hv/mod.rs: `whp` is "Windows on x86_64"; D13 "planned" |
| 6 | A GPU given a microVM (D144, Venus) | Linux, Windows | vm/x86_64.rs:378 and vm/aarch64.rs:425 "a GPU is given on macOS hosts alone so far (D144)"; user decision 2026-10-13 "--gpus Linux YES" |
| 7 | `--cgroup-parent` (D183) | macOS (refused: "no host resource group a process can be placed in"), Windows (refused until D183's slice 3, a job object) | D183 "Refused before anything starts" |
| 8 | Log drivers bound to one host: `journald` (Linux's systemd journal), `etwlogs` (Docker's on Windows) | journald on macOS and Windows; etwlogs nowhere yet | logging.rs:129, :633, daemon/ship.rs:810; architecture.md:7897 |
| 9 | A host's PCI function passed through (VFIO, D154) | macOS, Windows | vmm devices/pci/mod.rs:7 `vfio` Linux's |

The rest of the script's 494 items fall within these rows: `workload.rs` (95), `synced.rs`
(29), `vm_run.rs` (18), `context.rs` (14) and `cli/*` items are the daemon's and its client's
(row 1); `pressure.rs`, `warm.rs` and `confine.rs` items are rows 2 and 4, or each host's own
mechanism with its twin under another name (`platform::FileWatch`, the grants' transports).
Each lone item is confirmed or closed as its row is worked.

## Order

1. Row 1, the daemon on Windows, with its client, built on what Windows already runs:
   `run --kernel`'s confined VM process (D141), its network process (D142, D143), builds
   (D33), AF_UNIX (`shards_afunix`) and the console (Part 2b).
2. Row 2a, Windows snapshots, then templates and warm pools on it.
3. Row 3, host directories on Windows, with slates-d7's WHP DAX window.
4. Row 4, memory given back on Windows: WHP's discard of reported pages, a Windows memory
   pressure watcher, budgets, and give-backs for runs and builders.
5. Row 2b, arm64 Linux snapshots (D154's test path: an arm64 Linux guest with EL2 on an M3
   or later Mac).
6. Row 5, WHP on arm64. Its real-VM tests need an arm64 Windows host with the hypervisor
   platform.
7. Row 6, GPUs on Linux (the GPU desktop, its NVIDIA driver not yet installed), then on
   Windows.
8. Row 7, `--cgroup-parent` on Windows (job objects) and on macOS: groups shards keeps
   itself, held to their budgets by D188's give-backs.
9. Row 8, `etwlogs` on Windows, and journald's counterparts on macOS and Windows.
10. Row 9, a function passed through where a host allows one.

## Closed

- Row 2b, arm64 Linux: snapshots and restores, cold and warm, templates and warm pools on
  KVM (D154), tested by the snapshot tests themselves on arm64 Linux in a guest with EL2
  on an M3 or later Mac (boot.rs, `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`),
  where all 78 passed (PM M346).
- Row 4, memory given back on Windows (the plan's step 7): free page reporting under WHP, a
  cold boot's RAM decommitted and committed again (PM M371), a restore's given back through
  views of its template's data and memory of its own between them (M375, M378); the
  system's low memory notification watched as the daemon's and a build's pressure
  (`platform::MemoryPressure`, D188), each signal a word, mutation-checked on jupiter;
  budgets, a group shards keeps holding its microVMs (D183's slice 3); and give-backs for
  runs and builders, on jupiter: a joiner's 512 MiB read given back at its group's budget,
  its VM process from 642 to 651 MiB to 261 to 266
  (`what_a_joiner_reads_of_its_image_is_given_back_at_its_groups_budget`, PM M391), and a
  builder's 253 MiB of a 256 MiB file at its build's word, its VM process from 352,088 KiB
  to 96,220 (`a_builder_gives_back_what_it_holds_cold_at_its_builds_word`), with
  `a_runs_memory_freed_goes_back_to_the_host` passing there too. What a volume's reads hold
  is given back the same way once row 3 shares host directories on Windows.
