# Platform parity (2026-10-27)

The user's criterion: every feature of shards built for Windows, macOS and Linux, on amd64
and arm64, macOS on x86_64 aside (Intel Macs are out, 2026-10-13). This audit finds where the
existing work falls short of it.

**Closed 2026-11-13** (the plan's step 14): no code gap remains. What stays unrun is hardware
this project lacks and ROOT's runs, listed under [Closed](#closed-2026-11-13).

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
measured). On p/winmove row 14's 4 are gone, moves.rs's attributes read and made through the share
server's own calls on every host (`server::guest`), the script finding 15. A stub of the
same name hides a gap from the script, as a twin it cannot tell
from one: the messages of the merged code that name a host found row 17's
(`daemon::devices`' `plug_share` on Windows, "not plugged into a running microVM on Windows
yet"), since built: one `plug_share` for every host, which differs only in how the share's
stream reaches the VM.

Since 2026-11-05 (branch p/internal-local at b8270370, everything of integrate and gpu at
90de623f merged, CI 42134267973 green on every target) the script finds 12 items alone, all
Windows' and all row 9's: vm/aarch64.rs:272 `route`, :305 `IrqfdRoute`, :313 its `impl`;
vm/x86_64.rs:259 `route`, :282 `IrqfdRoute`, :288 its `impl`; vm/mod.rs:1221 `VfioFiles`,
:1361 `route`; vm/runtime.rs:115 `plug_vfio`, :123 `plug_vfio_files`, :132
`plug_vfio_with`; devices/pci/mod.rs:9 `mod vfio`. Each is Linux's VFIO, twinned with macOS's
refusal (its API assigns no device, PM M403) and with none on Windows, whose refusal
(`vm::passthrough_refused`) is no twin where a host permits Discrete Device Assignment: they
stand alone until row 9 is built on such a host. Row 15's items stand with Windows'
candidate (`windows_thread::raise_current`), a mechanism of its own whose measurement keeps
the row open. One stub of a shared name the script cannot tell from a twin remains, row
16's: hv/whp/mod.rs's `watch_writes`, which refuses a live move, and its `written` and
`unwatch_writes`, which do nothing.

A refusal is a counterpart only where the host's API cannot do the item's work; where a
host could, its item stands alone until it is built there.

Since 2026-11-07 (branch p/step10-final at b8c98e7f, with D154's step 13, d3029698: a host's
function assigned where Windows' hypervisor assigns one, by WHP's virtual PCI devices) the
script finds 5 items alone, all Windows' and all row 9's. Seven of the 12 now stand with a
Windows twin (platform-parity.tsv):

- devices/pci/mod.rs's `mod vfio`, Linux's function given the VM's kernel as it boots, with
  `mod assigned` (hv/whp/vpci.rs's allocation and device, devices/pci/assigned.rs's
  function on the guest's bus), WHP's x86_64 backend's.
- vm/x86_64.rs's `route`, `IrqfdRoute` and its `impl`, and vm/mod.rs's `route`: a
  passed-through function's MSIs routed by KVM's irqfds, with vpci.rs's `map_interrupt`,
  each MSI the guest programs mapped by WHP to the vCPUs it names
  (WHvGetInterruptTargetVpSet, WHvMapVpciDeviceInterrupt).
- vm/mod.rs's `VfioFiles` and runtime.rs's `plug_vfio_files`: a function the daemon opened
  for a confined VM process to plug in for a run's workload, behind the guest's IOMMU (their
  one caller, vm_run.rs's `function`, plugs with `iommu` set, as the daemon's
  `plug_function` asks), with `vm::passthrough_refused`, on Windows as on macOS a limit of
  the host's API: WHP remaps an assigned function's DMA to all of the partition's memory as
  it makes the device (WHvCreateVpciDevice's remarks) and gives the guest no I/O address
  space of its own, which a guest's VFIO needs to give a process a function
  (`WHP_NO_GUEST_IOMMU`, D154).

The 5 that stood alone were real gaps, and since branch p/whp-assign-gaps the script finds
none:

- vm/aarch64.rs:275 `route`, :308 `IrqfdRoute`, :316 its `impl`: Windows arm64 assigned no
  host function (`WHP_ARM64_NO_ASSIGNMENT`, now gone). Built: Microsoft Learn gives every
  virtual PCI function "x64, Arm64" (each function's page, 2026-06-08), and the SDK
  10.0.26100 declares them and their types outside its `_AMD64_`/`_ARM64_` branches
  (WinHvPlatform.h, WinHvPlatformDefs.h), so WHP's arm64 backend includes hv/whp/vpci.rs
  by path, its partition allowed device assignment before setup (`Vm::assigning`), the VM
  made before the layout where it is given a function (`vm::assigned_functions`, shared
  with x86_64). Its MSIs are mapped by vpci.rs's `map_interrupt` as x86_64's are, their
  target the SPI the GICv2m frame the VMM keeps names, at VP 0 alone: on Arm64
  WHvMapVpciDeviceInterrupt refuses more than one processor (its page), and
  WHvGetInterruptTargetVpSet is the header's x64's alone. The tsv stands the three with it.
- vm/runtime.rs:117 `plug_vfio`, :134 `plug_vfio_with`: a function plugged into a running
  microVM outside its IOMMU, the VM's kernel driving it (D214's VMM path,
  tests/hotplug.rs's `a_hosts_function_is_plugged_into_a_running_microvm`). Refused off
  Linux by runtime.rs's own `plug_vfio` (`vm::plug_refused`), on Windows where its
  hypervisor assigns devices with Microsoft's documented limit (`WHP_NO_PLUG`): a partition
  is allowed device assignment before its setup alone (E_INVALIDARG after, PM M401);
  Microsoft documents Discrete Device Assignment for a VM given its device before it starts
  and giving it back once stopped ("Deploy graphics devices by using Discrete Device
  Assignment", 2026-01-30); and WHP's pages say nothing of a virtual PCI device made in a
  partition that runs, nor does OpenVMM make one (ba105dd7). A refusal on the vendor's
  documentation, not a measured API limit: should a Windows Server host that assigns
  devices show WHvCreateVpciDevice making one in a running partition allowed assignment,
  the item stands alone again until it is built.

No stub of a shared name hides a gap the script cannot tell: hv/whp's `watch_writes`,
`written` and `unwatch_writes` now track the guest's writes (WHvMapGpaRangeFlagTrackDirtyPages,
WHvQueryGpaRangeDirtyBitmap), as `whp_arm64`'s do (row 16).

## The gaps

| # | What | Hosts without it | Evidence |
|---|---|---|---|
| 1 | The daemon, and all it serves. That is `shards run IMAGE`, containers and their every command (create, start, stop, rm, ps, logs, exec, attach, cp, top, stats, update, wait, kill, inspect), images run, volumes and binds, networks, published ports, log drivers, names, warm pools, the Engine API, compose, events, and the CLI's client, listings, terminal UI, `cp`, `save` and `show` | closed: Windows amd64 (D195); arm64 awaits row 5's qualification | main.rs:93-96 "the daemon needs Unix sockets, which shards does not support on this platform yet"; cli/mod.rs:592 "container commands need the daemon"; main.rs:17-66 `containers`, `daemon`, `local_store`, `logging`, `names`, `networks`, `segments`, `share`, `volumes`, `working_set` are `cfg(unix)`; cli/mod.rs:15-29 `client`, `cp`, `listing`, `look`, `screens`, `show` likewise; architecture.md:4282 "No daemon of shards' runs on Windows yet", :9753 "No published ports yet", :7897 "etwlogs ... waits for shards' daemon on Windows" |
| 2a | Snapshots and restores: `--snapshot-dir`, `shards restore`, templates (D25), warm pools (D26) | closed: Windows amd64 (D196) | D13 "no snapshots yet (D140)"; grant_answer.rs:406 "a microVM on Windows takes no snapshot yet"; grant.rs:97; vmm platform/windows.rs:559-630 (a snapshot's files, directories, prefetch and working sets "not ... on Windows") |
| 2b | The same | closed: Linux arm64 (D154) | D13 "no snapshots yet (D154)" |
| 3 | Host directories shared into a microVM (volumes and binds, D38: the share process and its FUSE server) and their DAX windows (D189) | closed: Windows (D197, D207) | virtio/fs/mod.rs: `server`, `host`, `headroom`, `window` are `cfg(unix)`; window_none.rs and vm/mod.rs:764 "a DAX window is not built on Windows yet (D189)" (slates-d7 holds the WHP window, its step 6) |
| 4 | Memory a microVM gives back: free page reporting (D151), budgets (D148), the host's pressure watched and given back to (D188) for runs and builders | closed: Windows (D151, D148, D188) | platform/pressure.rs Linux's and macOS's alone; build/mod.rs:48 `give` `cfg(unix)`; D188 "The daemon has no other platform yet" |
| 5 | A hypervisor backend | Windows arm64: built and merged into the integration (D209, `hv::whp` on aarch64 is `whp_arm64`), full-suite qualification open. Its real-VM suite on the nested arm64 Windows 11 host (FullReal04, 142 test binaries, whp-arm64's 2026-11-04 build) failed 10, nearly every case a microVM still running its start at a wall-clock deadline (PM M424; 81 of the first 84 had passed, 89f52499); and both 64-vCPU storms of tests/snapshot.rs, slow on 4 emulated CPUs, not losing interrupts, their watchdog's account holding the console (PM M427). The waits that follow their microVMs (D209, M424) and the storm watchdog's one-line stall (852b3def) are not yet rerun there, nor any of it on a native arm64 Windows host. CI's windows-11-arm runs its unit tests, its VM tests saying SKIP (no hypervisor platform, PM M172) | Windows 11 arm64 from Microsoft's ISO, nested on this Mac under QEMU/TCG and TF-A: real WHP boots, core snapshot restores, working sets, and busy restores at 2 and 8 vCPUs pass (M414); the first full run there was aborted after 80 of 141 binaries, with four failed. hv/mod.rs: `whp` is "Windows on x86_64 and arm64"; D13 |
| 6 | A GPU given a microVM (D144, Venus) | Windows arm64 (row 5's qualification pending); closed: Linux (D201, and musl's hosts and Intel's GPUs, D208), Windows x86_64 (D202); the GPU desktop's RTX 5060 Ti on NVIDIA's driver 620.70.06 and its Radeon 610M pass tests/gpu.rs again at cf36cf42, 3 of 3 (D208); open: arm64 Linux is built and linted, untested on a GPU (none here) | vm/x86_64.rs and vm/aarch64.rs "a GPU is given on macOS and Linux hosts alone so far (D144, D201)"; user decision 2026-10-13 "--gpus Linux YES" |
| 7 | `--cgroup-parent` (D183) | closed: macOS and Windows (D183's slice 3) | D183 "Refused before anything starts" |
| 8 | Log drivers bound to one host: `journald` (Linux's systemd journal), `etwlogs` (Docker's on Windows) | closed: etwlogs on Windows (D198), and macOS's own log (D199) and etwlogs as journald's counterparts; journald is Linux's own | logging.rs:129, :633, daemon/ship.rs:810; architecture.md:7897 |
| 9 | A host's PCI function passed through (VFIO, D154), at a run's start or plugged into it as it runs (D214) | open: Windows gives the VM's kernel a function as it boots where its hypervisor assigns devices (DDA through WHP's virtual PCI devices, D154 step 13, d3029698: hv/whp/vpci.rs, devices/pci/assigned.rs), on x86_64 and, since p/whp-assign-gaps, arm64 by the same code (vpci.rs included by path; its MSIs the SPIs of the GICv2m frame the VMM keeps, at VP 0, as WHP aims an MSI at one vCPU on Arm64), its E2E (boot.rs `a_hosts_pci_function_is_assigned_where_its_hypervisor_assigns_one`, now built for both) waiting on a Windows Server host that assigns one; a workload's function, at its start or plugged in behind the guest's IOMMU, is refused there with WHP's own limit, no I/O address space of the guest's (`WHP_NO_GUEST_IOMMU`), and one plugged into a running microVM outside its IOMMU (D214's VMM path) with Microsoft's documented one, a function given a VM as it starts alone (`WHP_NO_PLUG`: AllowDeviceAssignment before setup alone, PM M401; DDA documented for a VM off; WHP's pages silent on a running partition), untested where a hypervisor assigns devices. Unmeasured on arm64: where WHP delivers the SPI (GICD_IROUTER or VP 0) and the stage-2 attributes of a BAR's pages. x86_64 Linux's VFIO is untested on hardware, no host here having a function to spare. macOS's refusal is its APIs' limit, Virtualization's one passthrough a USB device's (D154, PM M356, M403); the Windows hosts tested here refuse with their own evidence (PM M401, M402) | vmm devices/pci/mod.rs `vfio` Linux's, `assigned` WHP's; platform-parity.py (2026-11-07, b8c98e7f): 5 items alone, vm/aarch64.rs's `route`, `IrqfdRoute` and its `impl` (Windows arm64), runtime.rs's `plug_vfio` and `plug_vfio_with` (a plug outside the IOMMU); on p/whp-assign-gaps 0: aarch64.rs's three stand with vpci.rs's `map_interrupt` on Windows arm64, `plug_vfio` and `plug_vfio_with` with runtime.rs's `plug_vfio` off Linux; `vfio`, x86_64's routes, vm/mod.rs's `route`, `VfioFiles` and `plug_vfio_files` stand with WHP's twins or its API's limit (How found, 2026-11-07) |
| 10 | A machine that serves other machines' clients through a context (D180's machine side): its listener, enrollments and invitations, a client's build contexts kept in step, its secrets, SSH agents and outputs relayed, Docker's tools' engine sockets served | closed: Windows (D180's part 3k) | platform-parity.py (2026-10-30): synced.rs (27 items), context.rs (12: `enroll`, `open_invitations`, `take_terminal`, the cidfile's, `copying`, `step_there`), build/ssh.rs (4: `AGENT_FDS`, `take_agents`, `asked_of`), agent.rs (`sign_step`, `BUNDLE_MOST`), daemon.rs `mod remote` ("Unix's alone as yet"), daemon/relay.rs `fds` |
| 11 | A restore's working set recorded and mapped ahead, the pages its guest writes copied first (REAP, D29) | closed: Windows (D196) | platform-parity.py (2026-10-30): platform/unix.rs's `populate_writable`, `FaultAroundOff` and `uffd`, whose Windows twins are stubs ("a restore on Windows does not record yet", vm::WORKING_SETS, D196), and cli/mod.rs's `working_set_writer` |
| 12 | Keyless signing's SPIFFE provider: a JWT-SVID from the agent's Workload API (cosign's `spiffe`) | closed: Windows (D126) | platform-parity.py (2026-11-01): spiffe.rs's `request`, `svids`, `spiffe_id` and `check` `any(unix, test)`; `jwt_svid` on Windows refuses ("the SPIFFE Workload API's named pipe: give the token itself with --identity-token"). cosign v3.1.3 dials `unix://` and the path alone (pkg/providers/spiffe), which go-spiffe v2.7.0 refuses on Windows, where it dials `npipe:` and `tcp:` (workloadapi/addr_windows.go) |
| 13 | The GPU's VM process given what its drivers reach, as confine.rs gives Linux's (D201) | closed: Windows, its spawn on a window station and desktop of its own (D202, `shards_ipc::Spawn::graphics`) | platform-parity.py (2026-11-01): confine.rs's `GPU_IOCTL_TYPES`, `GPU_SYSCALLS`, `gpu_paths` and `library_paths` |
| 14 | A microVM moved to another machine (D176 part 6) with its volumes' guest owners, which a volume's server keeps in its files' extended attributes (D38; on Windows NTFS's, as WSL keeps them, D197) | closed: Windows (D176 part 6), each file moved as its guest sees it, read and made through the share server's own calls on both sides (`server::guest`, stream version 2), so its owner, mode, kind, device number and attributes, and a name no Windows name holds, cross any pair of hosts | was: daemon/moves.rs's `NO_ATTR` and `sys`, macOS's and Linux's, alone, Windows' `xattrs::list` and `set` saying none; platform-parity.py (p/winmove) finds no item of moves.rs. Closed by CI 42070454222 at 90de623f and 42134267973 at b8270370: tests/moved.rs 9 of 9 on windows-2025 with every VM test required to run (`SHARDS_REQUIRE_VMS`, no SKIP), `a_volumes_guest_owners_move_with_it` and the saved moves among them, and `server::guest`'s unit tests (`a_guests_names_are_the_hosts_and_back`, `what_is_set_reads_back`) on every target |
| 15 | The policy a vCPU thread wakes at on a busy host (D191) | closed: Windows (PM M495), its vCPU threads and device workers at THREAD_PRIORITY_ABOVE_NORMAL, back to normal while one uses more than half of its CPU (`platform::windows_thread`, its governor's credit carried across its periods, e88ff240), the hold's governor at TIME_CRITICAL (PM M294): measured as Linux's slice was (`cpu-elastic/phases.py` against `winnone.patch`, n 100 a side, interleaved), on CI's windows-2025 with 4 busy loops on its 4 CPUs a `--cpus 1` start's guest part 8.20 and 10.13 ms at p50 where 217.09 and 336.51 without (`windows-wake.yml`, run 42435846541), the same raised from outside (`winslice.py`, 8.73 where 351.90); on jupiter neither moved a start past its spread. Measuring it found a held daemon's microVMs given every host CPU on Windows (`available_parallelism` reads no affinity mask there): fixed, `resources::host_cpus` counts the process's mask (e2820ce9) | platform-parity.py (2026-11-04): platform/unix.rs's `run_promptly` and `PROMPT_SLICE_NS` (Linux's shortest slice, PM M440), platform/mod.rs's `macos_thread` (macOS's real-time policy, M8, M10); D191's Windows, now measured; 2026-11-08: resources.rs's `affinity_cpus`, Windows', with `host_cpus` (platform-parity.tsv) |
| 16 | A microVM moved live (D176 part 7) | closed: arm64 Linux and Windows x86_64; Windows arm64 with row 5. arm64 Linux reads each writable slot's dirty bitmap (`KVM_GET_DIRTY_LOG`, which takes the ITS's writes too), and Windows WHP's dirty page tracking (`WHvQueryGpaRangeDirtyBitmap`, its ranges mapped again with `WHvMapGpaRangeFlagTrackDirtyPages` as the watch begins). tests/moved.rs whole, 9 of 9, live and saved moves both (`a_running_microvm_moves_live_and_goes_on_there`, `a_microvm_writing_its_memory_moves_live_with_none_of_it_lost`, `a_microvm_moved_saved_keeps_its_cpu_image_files_and_connections` among them), every VM test required to run: on arm64 KVM in the nested L1 guest on an M3 Ultra Mac Studio (boot.rs `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`, `SHARDS_L1_TESTS=moved`, at 3efe4190, 31.4 s, 2026-11-08), on jupiter (Windows 11 10.0.26200, e2820ce9, 163.1 s, 2026-11-08), and on CI's windows-2025 (CI 42329593544 at 51a709e9; windows-2022 restores no snapshot, PM M345). Windows arm64's backend watches its guest's writes as x86_64's does (built, D209); its moves, saved and live, wait on row 5's host (step 10). A move between two machines is the moves' own work (D176) | was: hv/kvm `Dirty::watch` ("keeps no dirty rings"), hv/whp `Vm::watch_writes` ("move only saved, not live"); tests/moved.rs `live_moves_here` |
| 17 | Devices plugged into a running microVM through the CLI (D214: `shards plug`, `unplug`, `grow`) | closed: Windows, whose daemon plugs a volume, its share process's stream (a duplex pipe the daemon makes, D197) handed to the VM as a run's are (D207) and taken there as a `PipeStream` (vm_run.rs `device::SHARE`) | daemon/devices.rs `plug_share`, every host's; tests/plug.rs no longer `cfg(unix)`. Closed by CI 42070454222 at 90de623f and 42134267973 at b8270370: tests/plug.rs's `devices_are_plugged_into_a_run_and_its_disk_grows` (4.9 to 6.6 s) and tests/hotplug.rs's `devices_are_plugged_into_a_running_microvm_and_its_disks_grow` (1.0 to 1.5 s) on windows-2025 (the run restored from its template) and windows-2022 (booted, PM M345), every VM test required to run; architecture.md D214's Verified. On jupiter too (e2820ce9, 2026-11-08): plug.rs's in 4.06 s, hotplug.rs's in 0.69 s |
| 18 | A run's writable layer restarted over its kept file, not packed (D190 slice 7, PM M443) | closed: macOS and Windows (PM M496). Both D190 E2E tests pass on a Mac's real microVMs (an M3 Ultra, macOS 26.4, Hypervisor.framework: `a_stopped_microvms_next_run_goes_on_over_its_kept_file` and `a_restored_microvm_goes_on_over_a_stopped_containers_kept_file`, 3 of 3 and 4 of 4 at 8a13e492, 2026-11-07, and 3 of 3 each at 5ae6a197, going over from 7 MiB, 2026-11-08), on jupiter (e2820ce9) and on CI's Windows runners. Windows goes over the kept file as Unix does: each shared view replaces a placeholder (VirtualAlloc2, MapViewOfFile3's MEM_REPLACE_PLACEHOLDER), and the swap unmaps the template's view back to it (UnmapViewOfFile2's MEM_PRESERVE_PLACEHOLDER) and places the container's file's there, unmapped from WHP's partition and mapped back around it; a kept layer's freed blocks go back in NTFS's whole sparse units (b2c7a727). Measured against packing (`layer-kept/ab.py`, `pack.patch` against `over.patch`, interleaved): macOS's restarts cross at 6.8 to 7.9 MiB (three runs, n 20 and 40; 5.3 in a fourth at load 19 to 23, within its spread), Windows' at 7.95 and 7.73 (jupiter, n 20, two runs), Linux's at 5.08 (M443), going over costing an empty layer's restart 7.5 to 8.7 ms more than packing on macOS and 233 on Windows; so no host's crossing is another's: each restart's way is learned on the host from its own restarts (`layers::Restarts`, D190), these tables holding the estimator (`a_restart_goes_the_way_this_hosts_restarts_say_is_quicker`) | D190's "On Windows, a view in place of a view"; `platform::map_file_shared_at`, `a_view_goes_in_place_of_another_at_its_address`; on Windows' real microVMs, CI 42070454222 at 90de623f, 42134267973 at b8270370 and 42329593544 at 51a709e9: both tests on windows-2025 and windows-2022 |

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
6. Row 5, WHP on arm64. Its host is Microsoft's Windows 11 arm64 ISO nested on this Mac
   (the user's decision, 2026-10-30). Windows' hypervisor and WHP run under QEMU/TCG with
   TF-A (M414). Core boots, snapshots and working sets pass; the full real-VM suite must
   still pass before this row is closed.
7. Row 6, GPUs on Linux (done: the GPU desktop at cf36cf42, D208), then on Windows.
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
    hosts of either kind keeps them. Built on p/winmove (`server::guest`); done (Closed,
    below).
16. Row 15, Windows' vCPU threads on a busy host measured as Linux's were (PM M440), and a
    policy taken where they wait as Linux's did: the candidate is built (D191), its harness
    committed; its measurement on jupiter decides it.
17. Row 17, built on p/winplug: tests/plug.rs and tests/hotplug.rs run on CI's
    windows-2022 and windows-2025, done (Closed, below); on jupiter, and their steps' numbers
    recorded in D214, still to do.
18. Row 16, live moves on arm64 KVM (each slot's dirty bitmap) and WHP
    (WHvQueryGpaRangeDirtyBitmap), built: run on each; saved moves run on both.
19. Row 18, restarts over the kept file (built on Windows with placeholder views) run on
    a Mac's real microVMs (Windows' done on CI), and measured on both against packing.
20. Row 9's E2E on a Windows Server host that permits DDA (step 13), and row 5 (step 10).
21. Row 9's two unbuilt parts: a function assigned on Windows arm64, and a function plugged
    into a running microVM outside its IOMMU on Windows (the script's last 5 items).

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
  tests/gpu.rs on fedora's UHD Graphics with glibc and in Alpine (D208, PM M425); both
  hosts again at cf36cf42, the GPU desktop on NVIDIA's 620.70.06 (D208).
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
- Row 14, a microVM moved with its volumes' guest owners to and from a Windows machine
  (D176 part 6): each file read and made as its guest sees it through the share server's
  own calls on both sides (`server::guest`, stream version 2). Tested with real VMs on CI's
  windows-2025, every VM test required to run there (`SHARDS_REQUIRE_VMS`, no SKIP):
  tests/moved.rs 9 of 9 between two homes, `a_volumes_guest_owners_move_with_it`,
  `a_microvm_moved_saved_keeps_its_cpu_image_files_and_connections` and
  `a_volume_shared_here_moves_with_one_and_the_other_goes_on_here` among them, and
  `server::guest`'s unit tests on every target (CI 42070454222 at 90de623f, 42134267973 at
  b8270370). Landed on gpu and dev (94838b88). A move between a Windows and a Unix machine
  has no CI job yet: migrate-e2e.yml's pairs are KVM's and HVF's.
- Row 17, devices plugged into a running microVM through the CLI on Windows (D214): a disk
  plugged, grown, unplugged and plugged again, and a host directory through its share
  process's duplex pipe. Tested with real VMs on CI's windows-2025 (the run restored from its
  image's template) and windows-2022 (booted, PM M345), every VM test required to run:
  tests/plug.rs's `devices_are_plugged_into_a_run_and_its_disk_grows` in 4.9 to 6.6 s and
  tests/hotplug.rs's `devices_are_plugged_into_a_running_microvm_and_its_disks_grow` in 1.0
  to 1.5 s (CI 42070454222 at 90de623f, 42134267973 at b8270370). Not yet run on jupiter,
  nor its steps timed on WHP.

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
- GPUs on every Linux host, musl's and Intel's (D208, PM M425; row 6).
- Devices plugged in (D214) on Hypervisor.framework, KVM x86_64 and, nested, arm64 KVM,
  through the CLI; a host's function on Linux alone, the others refusing as at a start
  (row 9); the CLI's path on Windows built, not yet run (row 17).
- Moves (D176 parts 6 and 7): saved on every backend, live on KVM x86_64 (PM M461) and
  Hypervisor.framework; open rows 14 and 16.

### CI's lanes (2026-11-05)

What every target's CI runs (.github/workflows/ci.yml's `native` and `musl` jobs), read from
the workflow and from the logs of CI 42134267973 at b8270370, all 17 jobs green:

- x86_64 Windows, on windows-2025 and windows-2022: the whole workspace's tests,
  `cargo test --workspace --no-fail-fast`, with `SHARDS_REQUIRE_VMS=1`, so a VM test that
  cannot run fails rather than skips (tests/common `cannot_run_vms`). windows-2025's log has
  no SKIP; windows-2022's has 58, each a snapshot's restore, which its WHP cannot do (it sets
  no APIC page back, PM M345, `cannot_snapshot`), restores running on windows-2025. Every
  test ignored there is a measurement run by its own harness.
- arm64 Windows: built, linted and tested on windows-11-arm (`test: true`, whp-arm64's
  lane, merged with its backend): its unit tests and every VM test's SKIP, limits from its
  first two runs (24.6 and 26.2 minutes, PM M423). That runner, the only hosted arm64
  Windows, has the hypervisor platform disabled, and enabling it needs a restart the runner cannot take: WHvCreatePartition
  0xC0351000 before and after (PM M172, whp-probe.yml run 38097837105; again in
  whp-passthrough.yml's run 41363714971, PM M402). Its VM tests can run on a host of its own
  alone: the nested Windows 11 arm64 of step 10 and a native arm64 Windows host (row 5).
- macOS arm64: the whole workspace's tests, every VM test saying SKIP (431 in its log):
  macos-15 is an M1 VM without Hypervisor.framework (`kern.hv_support`), as migrate-e2e.yml
  says too. A Mac's real-VM suite runs on this project's Macs.

Test files compiled for some hosts alone, which no Windows lane runs. Brought to every host
on p/parity-close: tests/guest.rs (the default guest fetched, checked and refused, D28);
tests/isolation.rs's vsock, engine-token and local MCP tests (its App Sandbox test stays
macOS's); tests/login.rs's `logins_are_checked_kept_used_and_forgotten`; vmm's
tests/fs_dir_handles.rs (its root opened through `platform::open_dir`); archive's
tests/adversarial.rs's bounded sparse map (its deep names stay Unix's: on Windows each
implied parent is made by a walk from the root, unmeasured at 200,000 components), and
tests/oracle.rs's `read_as_go` and `rebases_match_their_recorded_answers` (its trees' cases
stay Unix's, go-archive's own archives of a tree differing on Windows by design). Unix
mechanisms with their Windows twin, or none needed:

- shards' tests/budget.rs (Linux's memory controller under FUSE reads) and containers.rs's
  `a_microvm_held_to_a_budget_gives_back_what_it_holds_cold` and
  `..._reads_a_large_file_through_its_window_to_its_end`: Windows' groups' tests,
  `a_groups_job_holds_its_microvms_to_its_memory_max`,
  `a_microvm_under_a_group_is_held_to_its_budget` and
  `what_a_joiner_reads_of_its_image_is_given_back_at_its_groups_budget` (containers.rs).
- daemon.rs's `a_background_run_stops_to_read_its_terminal` (SIGTTIN, Unix's job control),
  `a_daemon_raises_its_descriptor_limit` and `a_daemon_out_of_descriptors_waits_for_room`
  (RLIMIT_NOFILE), and vmm's tests/fs_dir_budget.rs (a share under a descriptor limit set
  by setrlimit): Windows has no per-process descriptor limit to raise or meet.
  `runs_under_a_slice_start_after_it_was_stopped` (a systemd slice):
  `a_groups_microvm_is_made_in_its_job_its_network_process_too` (containers.rs).
- images.rs's `a_templates_saver_leaves_nothing_the_daemon_writes_through` and
  `a_restore_reaches_nothing_its_saver_left_in_its_template`: warm.rs's
  `a_template_saver_takes_a_run_only_out_of_the_templates_reach` (D196).
- slates_fs.rs's `a_microvm_maps_a_slates_mount_file_through_its_own_dax_window`: its
  `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`.
- context_client.rs's `a_file_not_written_whole_here_is_kept_by_no_exit` (RLIMIT_FSIZE).
- fleet.rs and commit.rs: measurements, every test of them ignored.

Brought to Windows since, each passing on CI's windows-2025 with every VM test required to
run (CI 42329593544 at 51a709e9, all 17 jobs green; its windows-2025 log has no SKIP,
windows-2022's 58, each a snapshot's restore, PM M345):

- tests/build.rs, gated test by test, not whole (11709e8b; 1af3ef44, 6de15f92): 123 passed
  and 4 ignored there. Four stay Unix's, each with its reason in the test: the FIFO
  give-back (build_windows.rs's test of the name its twin, over a named pipe), the
  systemd-scope memory test (`a_build_held_short_of_memory_has_its_builder_give_back`,
  Linux's at run), ADD over SSH (sshd and ssh-agent as the user's own processes, which
  Windows' OpenSSH runs as the machine's services), and the reproducible-build oracle (its
  cases' files carry Unix modes). referrers.rs's
  `policies_hold_an_agent_to_its_signature` and `an_agents_sbom_is_a_scanned_referrer`
  ungated with it.
- tests/buildx.rs, merged with row 5's backend (cdd2c031).
- login.rs's `a_credential_helper_that_never_answers_holds_only_what_needs_its_answer`
  (9ea74392, the tests' helper program its helper); daemon.rs's
  `an_engine_stream_ends_with_its_client` (6e371693, over engine.pipe); context_client.rs's
  `a_client_ended_before_its_machine_answers_leaves_no_file` (2719a807, its console's
  event); boot.rs's `a_guest_grows_and_gives_back_its_memory` (24e1f3e0, 45fcd6b7's
  `platform::held`) and its two DAX window tests (01c00152).
- archive's tests/changes.rs (d48432c6) and remote's tests/loopback.rs (afbdfdd1).
- The Windows network process's AppContainer: confined.rs's
  `the_network_process_is_confined` (e2436e01).

Still Unix's alone, not yet brought to Windows (open):

- build's tests/context.rs: ungated whole (p/tests-windows-last), its oracle held on
  Windows to testdata/contexts-answers-windows.json, fsutil's own walk on Windows that
  `scripts/build/generate --contexts` records there (the tree's FIFO left out, which
  Windows keeps in no directory; a file's other names sent as links, shards' D180
  difference from stat_windows.go's fsutil), and its races held to Windows' refusal by
  sharing; closed once those answers are recorded and CI's Windows runs pass it. Its
  `a_symlink_put_in_a_files_place_is_not_followed` runs on Windows too
  (p/win-context-nofollow): Windows' `open` (host.rs) opens through no link
  (`shards_ntfs::open_unfollowed`), refusing one ERROR_STOPPED_ON_SYMLINK as Unix's
  O_NOFOLLOW refuses one ELOOP, and `a_reparse_point_that_is_no_link_is_taken_as_its_bytes`
  holds a WOF-compressed file to its bytes; closed once CI's Windows runs pass them. A
  directory of the context put in place as a link after the walk, which the open of a
  file below it followed on every platform (O_NOFOLLOW held the last name only), is
  refused (p/dir-swap-races): host.rs `Beneath` opens each directory below the root in
  the one above it, never through a link (openat(2) O_NOFOLLOW|O_DIRECTORY on Linux and
  macOS; on Windows NtCreateFile relative to its parent's handle,
  `shards_ntfs::open_unfollowed_at`), and macOS's whole-directory clone goes through
  clonefileat(2) in its parent's descriptor; `a_directory_a_link_replaced_since_the_walk_is_not_followed`
  holds it, a symlink's on Unix, a junction's and a symlink's on Windows; closed once
  CI's runs on every host pass it.
- archive's Windows unpack (sys/windows.rs): it walked paths, each directory checked and
  then used, so a directory a local process swapped for a link in between was followed
  out of the root. It now walks as Go's os.Root walks on Windows (go1.26.1
  os/root_windows.go, internal/syscall/windows/at_windows.go) and as unix.rs walks: each
  name opened relative to its directory's handle as itself (FILE_OPEN_REPARSE_POINT), a
  link spliced in from its own handle's reparse data, and every operation on the last
  name made in its directory's handle (`shards_ntfs::create_at`, `hard_link_at`,
  `host_symlink_at` after Go's Symlinkat, `delete` after Go's Deleteat, `names`), so a
  read-only file is removed as Go's RemoveAll removes one and a final link of `read_dir`
  is refused as Unix's O_NOFOLLOW refuses it. sys/windows.rs's and sys/unix.rs's
  `a_directory_swapped_for_a_link_mid_unpack_leads_nowhere_outside` swap the directory
  between the walk's open of it and the file's, and `a_link_within_the_root_is_followed_inside_it`
  holds links inside the root to go-archive's splicing; closed once CI's Windows runs
  pass them and tests/oracle.rs's Windows cases.
- the git crate's `a_kept_git_dir_is_one_git_reads_as_its_own` (tests/real_git.rs):
  ungated (p/tests-windows-last), its origin's symlink put in git's index, its checkout's an
  NTFS symlink or, where this user may make none, the file git checks one out as with
  `core.symlinks` false; Windows' git finds only `run.sh`'s mode changed under the kept
  `filemode = true`, and nothing with `core.fileMode` false. Closed once CI's Windows runs
  pass it.

## Where step 14 stands

CI on the merged whole passes: CI 42070454222 at 90de623f (integrate, landed on gpu and dev
94838b88), 42134267973 at b8270370 (p/internal-local), 42329593544 at 51a709e9
(p/step10-final) and 42429865659 at e2820ce9 (p/parity-step14, over p/integrate-1107)
green on every target, both x86_64 Windows runners' VM tests among them, every test binary
of the workspace run with `SHARDS_REQUIRE_VMS=1` (ci.yml's `native` job excludes none), the
two Windows test gaps above among them (build's tests/context.rs, ungated, its answers on
Windows recorded in testdata/contexts-answers-windows.json; git's kept git dir).

Since 2026-11-08 (p/parity-step14): rows 15, 16 and 18 are closed, and row 17's tests ran
on jupiter too.

- Row 15: Windows' wake policy measured (PM M495) and kept; measuring it found and fixed a
  held daemon's microVMs given every CPU of a Windows host (`resources::host_cpus`,
  `affinity_cpus`, which the script stands with `host_cpus`, platform-parity.tsv).
- Row 16: tests/moved.rs whole, live and saved, on arm64 KVM (nested) and on jupiter.
- Row 18: both D190 E2E tests on a Mac's real microVMs; macOS's and Windows' restarts
  measured against packing (PM M496), their crossings each host's own: a restart's way
  learned on the host from its own restarts (`layers::Restarts`).

platform-parity.py over crates/ finds 0 items alone (2026-11-08, p/parity-step14), once
platform-parity.tsv stands the 7 that integrate's merges brought with their reasons:
block.rs's `Through` and its four (D188's writer outside a build's budget, needed where a
cgroup charges page cache, Linux's; Windows' job limits commit alone) and warm.rs's
`RUN_DESCRIPTORS` and `reserve_descriptors` (Linux's RCU wait as a shared descriptor table
grows; macOS's and Windows' hand-over not measured for it).

Open, each waiting on what this step cannot give it:

- Row 5, WHP on arm64: its full real-VM suite on the nested arm64 Windows 11 host, the plan's
  step 10, the coordinator's, in progress; with it Windows arm64's cells of rows 1, 6, 9 and
  16. windows-11-arm runs no VM test, its image having no hypervisor platform (PM M172).
- Row 6, GPUs: the GPU desktop's run of D208's build passed at cf36cf42 (its RTX 5060 Ti
  and Radeon 610M, tests/gpu.rs 3 of 3); arm64 Linux's GPU is untested, no arm64 Linux host
  here having one, and Windows arm64's waits on row 5.
- Row 9, a host's PCI function: its E2E on a Windows Server host that permits Discrete
  Device Assignment (the plan's step 13), x86_64 and arm64; x86_64 Linux's VFIO on hardware
  with a function to spare.

## Closed (2026-11-13)

platform-parity.py over crates/ found 58 items alone on main (2026-11-13), with five tsv lines
naming relay.rs's `relay_polled`, gone since fb1f28d42. Each now stands with its twin, or with
its hosts' reason none is needed (platform-parity.tsv, fe6dc59b7):
- D235's host edits: inotify, FSEvents and ReadDirectoryChangesW;
- each vCPU's guest time: KVM's /proc stat, HVF's exec time and WHP's counters;
- D12's console report;
- the managed GPU report and its lifeline (a Windows Job ends its processes as its owner goes,
  D233);
- shares given a VM, a client's share told apart, a refused client's reserve, an awaited
  input and children's inherited descriptors;
- a guest taken back.

The script now leaves out a file its parent declares a test module. It fails on a tsv line
whose item is no longer in its file, so a stale line can hide no later item of that name.
Main's merges since brought one item more: client.rs's `system_root`, Windows' alone because
Winsock names its providers under %SystemRoot%. Its line is 8db4c3755.

One of the 58 was a real gap: `attach` of a guest its client lost (D215) was refused on
Windows. Yet the guest's process, the `shards` that holds its VM in a Job (D141), outlives
the client, and Windows' API can hold it.
- Built in 43670dc18: held by a handle of its own with query, synchronize and terminate
  rights; its start read by GetProcessTimes; its end made by TerminateProcess, which ends its
  Job and so its VM.
- In b4548fcf6, the conformance suite's case and the simulator take a guest back on every
  host as well.

Step 14's own runs, as its agent gave them when kiln marked the step met (ROOT's goal stack,
kiln agent `01a148884a4770fdaf0111f3f792a248`: goal `01a17b1beb75724ab5ead1ffa61d0b4e`, step 14
of plan `01a17b18aa0676509a394733d1471f4a`, its evidence at 2026-11-13T05:23:16Z), as written:

> Tests: macOS arm64 (adas-mac-studio.local) at 3e76620e7: shards-api sim conformance 1/1,
> take-back/borrowed tests 18 passed and 1 ignored by design, clippy -D warnings exit 0.
> jupiter (Windows x86_64) at 5a6255b9c: 10/10, clippy 0.

The parity goal's own evidence (goal `efa444a23c94830c90c053a68a6e5a56`, at
2026-11-13T05:23:26Z) says of them, as written: "Tests: macOS arm64 shards-api tests and clippy
green at 3e76620e7; jupiter 10/10 at 5a6255b9c."

ROOT closed the audit with step 14. Every row's code is built on each host whose OS allows
it, and a refusal names the OS's own limit where it doesn't (row 9, step 13). Rows 5, 6 and 9
stay open above for runs, not code. A run that fails reopens its row:

- Row 5: Windows arm64's full real-VM suite (step 10), and with it Windows arm64's cells of
  rows 1, 6, 9 and 16.
- Row 6: arm64 Linux's GPU, as no arm64 Linux host here has one.
- Row 9: DDA's E2E on a Windows Server host that assigns a function, and x86_64 Linux's VFIO
  on hardware with a function to spare.
- On main, ROOT's:
  - platform-parity.py over crates/, expected to find 0 items alone;
  - CI's run on the fix's commit, Windows' E2E included: the Windows build,
    `attach_takes_back_a_guest_on_windows`,
    `a_guest_taken_back_on_windows_ends_as_it_drops_or_by_itself`, and boot.rs's
    `shards_api_conforms_on_real_guests`, whose attach case runs on WHP now;
  - that case on KVM and HVF again, for PM M541's reap fix.
