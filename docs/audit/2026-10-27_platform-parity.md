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

Since 2026-10-30 the script checks each host, Linux, macOS and Windows, where it checked
Unix against Windows; leaves out imports, which do no work of their own, and the guest's
crates; and stands an item with its twin, the item that does its part on the hosts it
misses under a name of its own, or with why those hosts need none, from
`scripts/audit/platform-parity.tsv`, whose every twin it checks is there. It exits 1 while
any item stands alone. Of the 404 items its first run found, 352 stand with their twins; the
52 left are rows 10 and 11's. Two more were fixed as found: `shards search`'s page and the
styled failures of `search` and `login` on a Windows colour terminal, which gates of "the
daemon, Unix's alone" had kept off, and the ghost files a crash left in a Windows home
(D176, D197), which no daemon start removed there.

Since 2026-11-01 it reads a cfg's any(), all() and not() of `unix`, `windows` and
`target_os`, `test` holding on no host, where it read nine forms alone and took an item of
any other as every host's: 25 items stood alone unseen, the groups shards keeps (D183), the
grants' modules and SPIFFE's client among them. These, and the 20 the GPUs' work brought
(D201, D202), are each a host's own mechanism with its twin, or with why the other hosts
need none, but for rows 12 and 13's. And it passes over a test's item to that item's end,
where it read no more of a file past its first `#[cfg(test)]`: a test's `use` hid
platform/mod.rs below its line 11, and daemon.rs below 622 of its 8,400. 31 more items
stood alone unseen, each a host's own mechanism (platform-parity.tsv), one of them row
11's: main.rs's `working_set`, which macOS needs not, its template's run recording.

Since 2026-11-04 (branch integrate at ff0cf486, with D183's slices 2 and 3, D38's bind
owners, D99's part four, D179, D188's builders, D190, D191, D208, D212, D214 and D176's
parts 6 and 7 merged) the script found 19 items alone, 11 of them new, the 8 others row 9's
routes. What each misses on macOS is a host's own mechanism, or none needed, with its twin
(platform-parity.tsv): D214's `plug_vfio`, `plug_vfio_files`, `plug_vfio_with` and
`VfioFiles` stand with macOS's refusal as row 9's routes do; D191's `run_promptly` and
`PROMPT_SLICE_NS` with macOS's real-time policy; and D188's `build::outside`, a budgeted
build's writes made outside its budget, which Linux's memory controller alone needs, as
macOS's groups and Windows' jobs charge no file's dirty pages to a budget, stands with none
needed on either. The 19 items it finds now are Windows' alone, each of an open row, and
stand alone until their row is closed: row 9's 12 (VFIO's routes, module and plugs), row
14's 4 (`daemon::moves`' attributes) and row 15's 3 (`run_promptly`, `PROMPT_SLICE_NS`, and
`macos_thread`, whose line had said Linux and Windows needed no policy before Linux's was
measured). A stub of the same name hides a gap from the script, as a twin it cannot tell
from one: the messages of the merged code that name a host found row 17's
(`daemon::devices`' `plug_share` on Windows, "not plugged into a running microVM on Windows
yet"), since built: one `plug_share` for every host, which differs only in how the share's
stream reaches the VM.

The Windows refusal probe does not implement DDA. A refusal is a counterpart only where
the host forbids assignment; Windows Server hosts that allow it still need a passthrough
implementation and a real hardware E2E. The PCI routing twins below therefore cover macOS
alone, so the script keeps Windows' missing passthrough visible (row 9).

## The gaps

| # | What | Hosts without it | Evidence |
|---|---|---|---|
| 1 | The daemon, and all it serves. That is `shards run IMAGE`, containers and their every command (create, start, stop, rm, ps, logs, exec, attach, cp, top, stats, update, wait, kill, inspect), images run, volumes and binds, networks, published ports, log drivers, names, warm pools, the Engine API, compose, events, and the CLI's client, listings, terminal UI, `cp`, `save` and `show` | closed: Windows amd64 (D195); arm64 has no backend (row 5) | main.rs:93-96 "the daemon needs Unix sockets, which shards does not support on this platform yet"; cli/mod.rs:592 "container commands need the daemon"; main.rs:17-66 `containers`, `daemon`, `local_store`, `logging`, `names`, `networks`, `segments`, `share`, `volumes`, `working_set` are `cfg(unix)`; cli/mod.rs:15-29 `client`, `cp`, `listing`, `look`, `screens`, `show` likewise; architecture.md:4282 "No daemon of shards' runs on Windows yet", :9753 "No published ports yet", :7897 "etwlogs ... waits for shards' daemon on Windows" |
| 2a | Snapshots and restores: `--snapshot-dir`, `shards restore`, templates (D25), warm pools (D26) | closed: Windows amd64 (D196) | D13 "no snapshots yet (D140)"; grant_answer.rs:406 "a microVM on Windows takes no snapshot yet"; grant.rs:97; vmm platform/windows.rs:559-630 (a snapshot's files, directories, prefetch and working sets "not ... on Windows") |
| 2b | The same | closed: Linux arm64 (D154) | D13 "no snapshots yet (D154)" |
| 3 | Host directories shared into a microVM (volumes and binds, D38: the share process and its FUSE server) and their DAX windows (D189) | closed: Windows (D197, D207) | virtio/fs/mod.rs: `server`, `host`, `headroom`, `window` are `cfg(unix)`; window_none.rs and vm/mod.rs:764 "a DAX window is not built on Windows yet (D189)" (slates-d7 holds the WHP window, its step 6) |
| 4 | Memory a microVM gives back: free page reporting (D151), budgets (D148), the host's pressure watched and given back to (D188) for runs and builders | closed: Windows (D151, D148, D188) | platform/pressure.rs Linux's and macOS's alone; build/mod.rs:48 `give` `cfg(unix)`; D188 "The daemon has no other platform yet" |
| 5 | A hypervisor backend | open: Windows arm64, in progress (the plan's step 10) on whp-arm64, not yet merged: its real-VM suite on a native arm64 Windows host last seen at 81 of 84 test binaries passing, three failing at their commands' deadlines (89f52499) | hv/mod.rs on integrate: `whp` is "Windows on x86_64"; D13 |
| 6 | A GPU given a microVM (D144, Venus) | Windows arm64, which has no backend (row 5); closed: Linux (D201, and musl's hosts and Intel's GPUs, D208), Windows x86_64 (D202); open: the GPU desktop's run of D208's build (its RTX 5060 Ti on NVIDIA's driver, its Radeon 610M) waits for its owner's go, and arm64 Linux is built and linted, untested on a GPU (none here) | vm/x86_64.rs and vm/aarch64.rs "a GPU is given on macOS and Linux hosts alone so far (D144, D201)"; user decision 2026-10-13 "--gpus Linux YES" |
| 7 | `--cgroup-parent` (D183) | closed: macOS and Windows (D183's slice 3) | D183 "Refused before anything starts" |
| 8 | Log drivers bound to one host: `journald` (Linux's systemd journal), `etwlogs` (Docker's on Windows) | closed: etwlogs on Windows (D198), and macOS's own log (D199) and etwlogs as journald's counterparts; journald is Linux's own | logging.rs:129, :633, daemon/ship.rs:810; architecture.md:7897 |
| 9 | A host's PCI function passed through (VFIO, D154), at a run's start or plugged into it as it runs (D214) | open: Windows hosts that permit DDA still have no passthrough implementation or real hardware E2E (the plan's step 13, which needs a Windows Server host that permits it). macOS's refusal is its API's limit (PM M356, M403); the Windows hosts tested here refuse with their own evidence (PM M401, M402) | vmm devices/pci/mod.rs:7 `vfio` Linux's; vm::passthrough_refused reports the implementation gap when WHP accepts AllowDeviceAssignment; platform-parity.py (2026-11-04): runtime.rs's `plug_vfio`, `plug_vfio_files`, `plug_vfio_with`, vm/mod.rs's `VfioFiles` beside the routes and `vfio` |
| 10 | A machine that serves other machines' clients through a context (D180's machine side): its listener, enrollments and invitations, a client's build contexts kept in step, its secrets, SSH agents and outputs relayed, Docker's tools' engine sockets served | closed: Windows (D180's part 3k) | platform-parity.py (2026-10-30): synced.rs (27 items), context.rs (12: `enroll`, `open_invitations`, `take_terminal`, the cidfile's, `copying`, `step_there`), build/ssh.rs (4: `AGENT_FDS`, `take_agents`, `asked_of`), agent.rs (`sign_step`, `BUNDLE_MOST`), daemon.rs `mod remote` ("Unix's alone as yet"), daemon/relay.rs `fds` |
| 11 | A restore's working set recorded and mapped ahead, the pages its guest writes copied first (REAP, D29) | closed: Windows (D196) | platform-parity.py (2026-10-30): platform/unix.rs's `populate_writable`, `FaultAroundOff` and `uffd`, whose Windows twins are stubs ("a restore on Windows does not record yet", vm::WORKING_SETS, D196), and cli/mod.rs's `working_set_writer` |
| 12 | Keyless signing's SPIFFE provider: a JWT-SVID from the agent's Workload API (cosign's `spiffe`) | closed: Windows (D126) | platform-parity.py (2026-11-01): spiffe.rs's `request`, `svids`, `spiffe_id` and `check` `any(unix, test)`; `jwt_svid` on Windows refuses ("the SPIFFE Workload API's named pipe: give the token itself with --identity-token"). cosign v3.1.3 dials `unix://` and the path alone (pkg/providers/spiffe), which go-spiffe v2.7.0 refuses on Windows, where it dials `npipe:` and `tcp:` (workloadapi/addr_windows.go) |
| 13 | The GPU's VM process given what its drivers reach, as confine.rs gives Linux's (D201) | closed: Windows, its spawn on a window station and desktop of its own (D202, `shards_ipc::Spawn::graphics`) | platform-parity.py (2026-11-01): confine.rs's `GPU_IOCTL_TYPES`, `GPU_SYSCALLS`, `gpu_paths` and `library_paths` |
| 14 | A microVM moved to another machine (D176 part 6) with its volumes' guest owners, which a volume's server keeps in its files' extended attributes (D38; on Windows NTFS's, as WSL keeps them, D197) | open: Windows, whose move carries no attribute (`xattrs::list` and `set` there say none), so a volume moved from or to a Windows machine leaves its guest's owners behind | platform-parity.py (2026-11-04): daemon/moves.rs's `NO_ATTR` and `sys`, macOS's and Linux's, alone; moves.rs "Windows keeps a volume's owners in NTFS's own extended attributes (D197), which this move does not carry yet" |
| 15 | The policy a vCPU thread wakes at on a busy host (D191) | open: Windows, unmeasured: its vCPU threads run as they are, its hold's governor alone at TIME_CRITICAL (PM M294) | platform-parity.py (2026-11-04): platform/unix.rs's `run_promptly` and `PROMPT_SLICE_NS` (Linux's shortest slice, PM M440), platform/mod.rs's `macos_thread` (macOS's real-time policy, M8, M10); D191's Next, "Windows' vCPUs on a contended host: unmeasured" |
| 16 | A microVM moved live (D176 part 7) | open: arm64 Linux, whose dirty rings need the ITS's bitmap beside them (`KVM_CAP_DIRTY_LOG_RING_WITH_BITMAP`), and Windows, whose dirty page tracking (`WHvQueryGpaRangeDirtyBitmap`) is not asked yet: each refuses a live move, saying so, and moves one saved. A saved move on either, built, has not run there yet | D176's Not yet; moves.rs, "this host's microVMs move only saved, not live"; tests/moved.rs asserts the refusal off macOS and Linux x86_64 |
| 17 | Devices plugged into a running microVM through the CLI (D214: `shards plug`, `unplug`, `grow`) | open, built: Windows, whose daemon now plugs a volume, its share process's stream (a duplex pipe the daemon makes, D197) handed to the VM as a run's are (D207) and taken there as a `PipeStream` (vm_run.rs `device::SHARE`); tests/plug.rs builds and runs on every host (booted where WHP restores no template), and tests/hotplug.rs serves its shared directory over a pipe on Windows. Neither has run on WHP yet (CI's windows-2022 and windows-2025, jupiter) | daemon/devices.rs `plug_share`, every host's; tests/plug.rs no longer `cfg(unix)`; architecture.md D214's Verified and Not yet |
| 18 | A run's writable layer restarted over its kept file, not packed (D190 slice 7, PM M443) | open: Windows and macOS, built and held by unit tests, not yet run on their real microVMs. Windows now goes over the kept file as Unix does: each shared view replaces a placeholder (VirtualAlloc2, MapViewOfFile3's MEM_REPLACE_PLACEHOLDER), and the swap unmaps the template's view back to it (UnmapViewOfFile2's MEM_PRESERVE_PLACEHOLDER) and places the container's file's there, unmapped from WHP's partition and mapped back around it; the daemon no longer packs there. To close: the two D190 E2E tests (`a_stopped_microvms_next_run_goes_on_over_its_kept_file`, `a_restored_microvm_goes_on_over_a_stopped_containers_kept_file`, now built for Windows) passing on Windows and a Mac, and Windows' restarts measured against packing. A restart works on every host; this is its cost past 5 MiB written | D190's "On Windows, a view in place of a view" and Next; `platform::map_file_shared_at`, `a_view_goes_in_place_of_another_at_its_address` |

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
11. Row 10, a Windows machine serving other machines' clients through a context.
12. Row 11, working sets on Windows: a restore's pages recorded and mapped ahead.
13. Row 12, the SPIFFE Workload API reached on Windows by its named pipe, as SPIRE's agent
    serves it there.
14. Row 13, with gpu-windows, after win-pools lands.
15. Row 14, a volume's guest owners moved to and from a Windows machine: each file's
    attributes as the guest sees them on both sides, through its share server's own reading
    of them (the owner's attribute on Unix, WSL's metadata on Windows), so that a move between
    hosts of either kind keeps them.
16. Row 15, Windows' vCPU threads on a busy host measured as Linux's were (PM M440), and a
    policy taken where they wait as Linux's did.
17. Row 17, built on p/winplug: tests/plug.rs and tests/hotplug.rs run on CI's
    windows-2022 and windows-2025 and on jupiter, and their numbers recorded in D214.
18. Row 16, live moves on arm64 KVM (dirty rings with the ITS's bitmap) and WHP
    (WHvQueryGpaRangeDirtyBitmap); saved moves run on both.
19. Row 18, restarts over the kept file (built on Windows with placeholder views) run on
    Windows' and a Mac's real microVMs, and measured.
20. Row 9 on a Windows Server host that permits DDA (step 13), and row 5 (step 10).

## Closed

- Row 1, the daemon on Windows and all it serves (the plan's steps 1, 2 and 8): the home's
  request slots as named pipes and the daemon's spawner (D195), every container command and
  the CLI's client, images' save, load and import, the Engine API on its named pipe, events,
  compose and every log driver. Tested with real VMs on CI's windows-2022 and windows-2025:
  tests/daemon.rs 37 of 37 (CI 40618994939 at 61a07442), and containers.rs 132 of 132,
  lifecycle.rs 1, run.rs 6, images.rs 11, engine.rs 27 (Docker's own CLI on the Engine API's
  pipe) and compose.rs 3 (the real compose plugin), every log driver among them (CI
  40862958912 at 624ba89b); on jupiter, the same counts, and shards_ipc's slots 58 of 58 with
  23 mutants caught.
- Row 2a, snapshots, restores, templates and warm pools on Windows amd64 (steps 3 and 4,
  D196): a WHP partition saved and restored, D25's templates and D26's warm pools served by
  the Windows daemon. Tested on jupiter (Windows 10.0.26200): snapshot.rs 10 of 10, restore.rs
  2 of 2, warm.rs 6 of 6; on CI's windows-2025 snapshot.rs 10, restore.rs 2 and the vmm's 233
  (CI 40519967428, 40527550618), warm.rs 6 (40684427069), tests/daemon.rs's pool and template
  tests among its 37 (CI 40618994939). windows-2022's WHP sets no APIC page back and so
  restores nothing, which its tests say (PM M345). Its cost: PM M345.
- Row 7, `--cgroup-parent` on macOS and Windows (step 12, D183's slice 3): groups shards keeps,
  held to their `memory.high`, `memory.max` and `cpu.max`; on Windows each group's VM processes
  in a job of its own. Tested with real VMs: a_microvm_under_a_group_is_held_to_its_budget on
  macOS (PM M355), a_groups_job_holds_its_microvms_to_its_memory_max on CI's windows-2022 and
  windows-2025 (CI 40624202863), and a_groups_microvms_are_held_to_its_cpu_max_together on
  macOS, jupiter and both CI runners (03943f0a).
- Row 8, log drivers bound to one host (step 8): etwlogs, Docker's on Windows, built (D198),
  its real-time ETW session's lines word for word on both CI runners (CI 40862958912,
  `a_runs_log_is_sent_to_etw_as_its_options_say`); journald's counterparts, macOS's own log
  (`oslog`, D199, `a_runs_log_is_sent_to_macos_own_log_as_its_options_say` with `log show`)
  and etwlogs on Windows, journald itself refused off Linux as Linux's own.
- Row 6, Linux: a microVM given its host's GPUs through Venus, rendered by the host's own
  Vulkan driver in the GPU's VM process (D201), tested by tests/gpu.rs on the GPU desktop's
  NVIDIA GeForce RTX 5060 Ti and Radeon 610M (PM M381, M382, M384); on musl hosts too, the
  GPU's VM process linked against the host's musl, and on Intel's GPUs, tested by
  tests/gpu.rs on fedora's UHD Graphics with glibc and in Alpine (D208, PM M422).
- Row 6, Windows: the same on an x86_64 Windows host, the krunkit fork's Venus in the GPU's
  VM process, its blobs of the renderer's own pages mapped by WHP, the process in its
  AppContainer on a window station and desktop of its own (D202), tested by tests/gpu.rs on
  jupiter's Radeon 8060S (PM M383, M385). arm64 Windows waits for row 5.

- Row 2b, arm64 Linux: snapshots and restores, cold and warm, templates and warm pools on
  KVM (D154), tested by the snapshot tests themselves on arm64 Linux in a guest with EL2
  on an M3 or later Mac (boot.rs, `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`),
  where all 78 passed (PM M346).
- Row 3, host directories shared into a microVM on Windows and their DAX windows (the
  plan's step 6): the share process's FUSE server over NTFS, a guest's files kept there as
  WSL keeps them (D197's slices 1 and 2); the share process and the daemon's shares on
  Windows, each share's stream a pipe, and a Windows host's bind sources its drives' paths
  (slice 3, D207); and shards' own shares' DAX windows on WHP, each range a view of the
  share process's section of its file, as long as the file's bytes there and zeros past
  them, followed as the file grows and let go of before its guest cuts it, the window's
  head committed as its guest first touches each 2 MiB block (slice 4, PM M412, M413).
  Tested with real VMs on CI's windows-2022 and windows-2025 runners and on jupiter:
  containers.rs's volume, bind and DAX tests (`run_mounts_binds_volumes_and_tmpfs`,
  `a_volumes_data_outlives_the_microvm_that_wrote_it`,
  `a_volumes_large_files_go_through_its_dax_window`, `..._by_default`,
  `a_volumes_files_are_mapped_from_dax_smallest_bytes`,
  `a_volumes_long_dax_window_takes_its_guest_no_mapping_for_each_range` among them),
  saved.rs's `a_saved_microvm_reads_on_through_its_dax_window` and
  `a_saved_microvm_writes_on_through_its_volume`, engine.rs and compose.rs: CI 40916747277
  at 652aae3b for slice 3, and 40961442989 at 4b6e135f and 41004323062 at 1cdaef68 for
  slice 4; on jupiter containers.rs 141 of 141, engine.rs 27, compose.rs 3, saved.rs 13.
- Row 4, memory given back on Windows (the plan's step 7): free page reporting under WHP, a
  cold boot's RAM decommitted and committed again (PM M371), a restore's given back through
  views of its template's data and memory of its own between them (M375, M378); the
  system's low memory notification watched as the daemon's and a build's pressure
  (`platform::MemoryPressure`, D188), each signal a word, mutation-checked on jupiter;
  budgets, a group shards keeps holding its microVMs (D183's slice 3); and give-backs for
  runs and builders, on jupiter: a joiner's 512 MiB read given back at its group's budget,
  its VM process from 642 to 651 MiB to 261 to 266
  (`what_a_joiner_reads_of_its_image_is_given_back_at_its_groups_budget`, PM M391), and at
  its host's word, the daemon told through `SHARDS_PRESSURE`, its VM process back to within
  4 MiB of idle 1.1 to 1.2 s after the word
  (`what_a_microvm_holds_cold_is_given_back_at_its_hosts_word`, on every host), and a
  builder's 253 MiB of a 256 MiB file at its build's word, its VM process from 352,088 KiB
  to 96,220 (`a_builder_gives_back_what_it_holds_cold_at_its_builds_word`), with
  `a_runs_memory_freed_goes_back_to_the_host` passing there too; and what a volume's reads
  hold, through D197's FUSE server over NTFS, at its group's budget, its microVMs from 764
  to 768 MiB to 272 (`a_microvm_under_a_group_is_held_to_its_budget`).
- Row 12, keyless signing's SPIFFE provider on Windows (D126): SPIFFE's Workload API
  reached by its named pipe, SPIRE's agent's (`\\.\pipe\spire-agent\public\api`) or the
  one `SPIFFE_ENDPOINT_SOCKET` names (`npipe:NAME`, as go-spiffe reads it there, or the
  pipe's path), opened anonymous to its server as go-winio dials one, and found there
  without a connection taken; cosign v3.1.3 reaches none there. Tested on CI's
  windows-2022 and windows-2025 (CI 41284231177 at 6b17d663): spiffe.rs's
  `a_jwt_svid_is_asked_of_the_workload_api_on_its_named_pipe` and
  `a_named_pipe_is_named_as_go_spiffe_names_it`, beside its socket's twin on Linux and
  macOS. platform-parity.py finds none of the row's items alone.
- Row 10, a Windows machine serving other machines' clients through a context (D180's part
  3k): its listener, enrollments and invitations, as a Unix machine's; a client's command
  started through the daemon's spawner with its streams, signals' pipe, a build's secrets,
  SSH agents' pipes and a `--cidfile`'s pipe given by number, in a pseudoconsole of its own
  that starts at its terminal's cursor (PSEUDOCONSOLE_INHERIT_CURSOR, its question answered
  and kept from the client), so that no command clears its client's screen; a build's
  context copy kept as a Linux tree on NTFS, as WSL keeps one (crates/ntfs,
  `apply_layer_lx`), and read back as the client's by the build's walk and the manifest
  the client compares with, times compared to NTFS's 100 ns; a build's output to a
  directory asked for as its archive; and Docker's tools through a context, a Windows
  client's engine a named pipe. Tested with real VMs in tests/context.rs, whole: on CI's
  windows-2022 and windows-2025 (budget-e2e 41273324343 at 7452404e), and on jupiter
  (Windows 11 Pro 10.0.26200, 19 of 19 at 341450a4's Rust, Docker's 29.9.1 client among
  them, built by windows-binaries.yml as kiln's guard there holds builds); a Linux
  client's tree on a Windows machine by archive's tests/lx.rs and synced's
  `a_linux_clients_tree_is_its_own_in_a_windows_copy` on both runners (budget-e2e
  41273319640 and 41262337649). platform-parity.py finds none of the row's items alone.
- Row 11, a Windows restore's working set (D196): what its guest touched read back as its VM
  process's own working set (QueryWorkingSetEx), a page its file's read and a private one a
  copy its guest wrote, recorded by a template's first warm restore and by `shards restore`
  through the CLI that started it; and mapped ahead by the next, before its guest runs, in two
  calls of WHP's populate, its written pages for writing, which copies each in the host's
  kernel, and the rest for reading; its memory kept cached by a view of it held where its
  generation is held, as Windows drops a sparse file's pages once no process maps them (PM
  M421). With it against without, a cold restore at p50 / p99: on jupiter, held as the
  daemon holds a template, 15.1 / 17.0 ms against 17.5 / 22.4, and a warm one 2.49 / 2.54
  against 6.79 / 10.58; on CI's windows-2025, held, 20.4 / 23.1 against 36.0 / 52.9 (PM
  M415, M421, n=100 each). Tested with real VMs on CI's windows-2025 (41176475317 at
  54f7c0fb, which fails rather than skips a VM test it cannot run): snapshot.rs's
  `a_restore_maps_ahead_the_working_set_the_restore_before_it_recorded` and images.rs's
  `repeat_runs_restore_a_template_of_the_image`; the restore bench's arms with and without,
  held and not, there and on jupiter; and windows_process.rs's and hv::whp's tests of what is
  mapped, on CI's Windows runners, the populate's on jupiter too (M415). platform-parity.py
  finds none of the row's items alone.
- Row 13, the GPU's VM process on Windows given what its driver reaches, as confine.rs gives
  Linux's (D201): its AppContainer and every mitigation kept but Win32k's, on a window
  station and desktop of its own (D202, `shards_ipc::Spawn::graphics`, PM M385), the
  GPU's Station its twin of confine.rs's GPU rules (platform-parity.tsv). Tested with real
  VMs on jupiter's Radeon 8060S: tests/gpu.rs at gpu-windows' e39a9218, a run's and a build
  step's `vulkaninfo` finding `Virtio-GPU Venus` through Venus (196.6 s); CI 41331454598
  green on every target there, its Windows runners building, linting and testing the
  renderer. Landed on gpu (e39a9218) and dev (bc0b245b). platform-parity.py finds no item
  alone, of any row.

### Since 2026-11-04 (integrate)

What the merges into integrate brought, host by host, each in D13's table:

- Row 7, wider: `--cgroup-parent` on Linux without a user's manager, a cgroup delegated to
  the user (D183's slice 2, `a_microvm_is_made_in_the_delegated_cgroup_its_cgroup_parent_names`
  on Fedora 44), and on Windows the microVM's network process made in its group's job beside
  its VM process (`a_groups_microvm_is_made_in_its_job_its_network_process_too`), so a group
  holds a microVM's whole host footprint there as a slice's scope does on Linux.
- Rows 3 and 4, wider, each written once over every host's share server and budget: a
  bind's host-made files its run's user's (D38, `a_binds_host_files_are_its_runs_users`,
  NTFS's WSL metadata the owner's attribute on Windows); a budgeted build's own writes made
  outside its budget on Linux, which alone charges them to it (D188, PM M442), its twin
  none needed on macOS and Windows (platform-parity.tsv); an export over its base's clone
  where the store's file system clones (APFS, Btrfs, XFS), written whole where it does not
  (ext4, NTFS: `build::host::clone_file` makes no clone on Windows, as ReFS's block clone is
  not asked).
- IPv6 past the microVM (D99's part four), `--platform` for one platform's microVM (D179),
  the build cache collected as its user's shares say (D212), and D191's limits and hold:
  the guest's, the network process's and the daemon's, written once for every host. D191's
  vCPU wake policy is each host's (row 15).
- A run's writable layer (D190): ext4 over pmem and its freed blocks given back on every
  host (FALLOC_FL_PUNCH_HOLE, F_PUNCHHOLE, FSCTL_SET_ZERO_DATA); restarts over the kept
  file on Linux, built on macOS and Windows (placeholder views) and run on neither's real
  microVMs yet (row 18).
- GPUs on every Linux host, musl's and Intel's (D208, PM M422; row 6).
- Devices plugged in (D214) on Hypervisor.framework, KVM x86_64 and, nested, arm64 KVM,
  through the CLI; a host's function on Linux alone, the others refusing as at a start
  (row 9); the CLI's path on Windows built, not yet run (row 17).
- Moves (D176 parts 6 and 7): saved on every backend, live on KVM x86_64 (PM M461) and
  Hypervisor.framework; open rows 14 and 16.

CI on the merged whole has not passed yet: integrate's CI 41959673660 (b5113c13) failed,
and ff0cf486, which merges the builders, VFIO plugs and moves, has no run yet. The plan's
step 14 closes once rows 5, 9 and 14 to 18 are closed and a run of CI on the closing
commit passes on every target, Windows' E2E included.
