# Image boot for orchestrators (decision v1, D231)

Implementer record, branch `p/runtime-image-boot-v1` from `origin/p/integrate-engine-20261111`
(82083f27). POLICY_READ: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f,
docs/development-rules.md sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer,
coordinator 01a148884a4770fdaf0111f3f792a248
([receipt](measurements/runtime-image-boot-v1/POLICY_READ.json)). Nothing in this record was
run on a VM by this implementer: every behaviour below is cited from source at the branch
base, and every live result the decision needs is listed as owed in §8.

## 1. The need

Hex (github.com/hyper-light/hex) will run each harness (Codex, Claude Code, opencode, ...)
in a Shards microVM of its own, defined by an Agentfile (`HARNESS`/`AGENT` with
`harness.json`/`agent.json`, D54). Today Hex boots its guests through `shards-api`
(`crates/api`, D215) by direct kernel boot: `Shards::boot` starts `shards run --kernel …
--vsock <socket> --no-console` (crates/api/src/client.rs:240-268) with Hex's own `hex` as
PID 1 from an initramfs, then reaches its node agent over vsock (Hex
`docs/design/shards-guest.md` §2-§4a, at Hex main 922c70db; its client is
`hex-runtime/src/guest/*`). That boot never runs shards-init, which is what applies an
Agentfile: it reads the image's normalized Agentfile, starts each declared member in a
confined domain (namespaces, cgroup, uid `200000+n`, no capabilities, seccomp filter the
host compiles, its network grants), and captures its output (crates/init/src/domains.rs:1-25,
1344-1352; crates/init/src/run.rs:205-264). Hex avoids the daemon's Engine API, which boots
images under shards-init as containers from warm pools (D25-D27) and gives the caller no
microVM of its own, and it may not link the VMM (D30, D215).

The orchestrator needs, for an image built from an Agentfile, booted under shards-init with
no daemon:

1. each member's stdin, stdout, stderr and an optional terminal, and its exit status;
2. env and files staged into a named member at create (credentials per box, never in the
   image), saying whether that is native-sign-in delegation or opaque (G18);
3. guest ports published on host ports the orchestrator chooses, TCP and UDP, and the run's
   network grants as the image declares them, narrowed and never widened by the orchestrator;
4. host paths shared into the guest, read-only or read-write, including ones added to a
   running microVM where Shards can, and what it cannot;
5. the microVM's stats;
6. stop.

What Hex does with a guest today, read in its checkout (read-only): it boots one guest per
node with `GuestSpec { kernel, init, cmdline, vcpus, memory_mib, shares, ports, binary }`,
dials control, stdio, files and stamps ports (`0x4845_5800..03`), stages each box's root and
per-box files over its files port, takes a stream per exec stream, records `Identity` to
`attach` after its own crash (Unix), and ends the guest with `stop()`
(`hex-runtime/src/guest/mod.rs:63-141`, `docs/design/shards-guest.md` §3-§4a). Its box
accounting reads cgroup numbers: memory, peak, OOM kills, pids, refused forks, CPU, I/O
(`hex-api/src/runtime.rs:1335-1347`, `Usage`). Its program streams are pipe or socket ends the
caller holds (`hex-api/src/spec.rs:643-690`, `Stdio`, `Ask`, `Given`). ROOT will forward Hex's
own answer about its relay's operations; nothing here depends on that answer.

## 2. What exists, per need (source at the branch base)

| Need | Exists | Where | Gap |
|---|---|---|---|
| A daemonless boot into an image under shards-init | `shards run vm --kernel K --init SHARDS-INIT --rootfs IMAGE -- CMD` boots the EROFS image on `/dev/pmem0`, sends one command as `docker run` does, its output shards' own output, its status shards' exit status | vm_run.rs:21-38, 1165-1209, 1486-1925; workload.rs:602-848 | No Agentfile setup: the host compiles the domains' seccomp filter, attaches the in-VM server device and admits managed capture only in the daemon (daemon.rs:3809, 7408; daemon/managed.rs), and init starts no domain without that filter (domains.rs:1340-1343). One command's stdio only. No network. |
| The run protocol | vsock host ports 1024 (run), 1025 (signals), 1027 (exec), frames of stdcopy's header | abi/src/run.rs:1-180 | none for this need |
| One run handed to a VM over a link | `--warm FD`: the VM boots, says `READY`, takes `RUN` (head + spec + stdio, shares, capture descriptors), says `TAKEN`, serves it | vm_run.rs:1464-1484, warm.rs:452-640; ipc/src/lib.rs:1958-2180 | The only party at the link's other end is the daemon |
| A VM process started with its network and confinement, outside the daemon | the builder's `launch`: network process first, VM in App Sandbox asking its spawner (macOS), AppContainer made suspended then its network process then resumed (Windows) | build/builder.rs:556-728 | A builder's arguments; a second copy lives in `Daemon::start` (daemon.rs:8109, 8270) |
| A run prepared from an image | `run::prepare`: the image from the store, the guest (D28), the Agentfile read in the image and checked against its digest (D109), the spec merged | run.rs:742; agentfile.rs:181 | none: a free function |
| A run's setup entries | `setup::setup`, `security_setup`, `domain_seccomp`; volumes `open_for` | setup.rs:534, 672, 382; volumes.rs:1221 | none: free functions |
| Members' stdout and stderr | managed capture (P3): a pipe per member and channel, exact bytes, credit per tenant and channel, `Open`/`Data`/`End`/`Aborted` records on the authenticated managed connection | abi/src/run/managed.rs; init/src/capture.rs; workload/capture.rs | The owner side is the daemon's endpoint alone (daemon/managed.rs) |
| Members' stdin | none: a domain's stdin is `/dev/null` | domains.rs:22 | P3's "input policy is explicitly closed unless a native adapter or authorized session owns it"; G24 |
| A member's terminal | none | | G24 |
| A member's exit status | init's lifecycle holds it (`Draining`/`Ended { status }`) | init/src/managed.rs:122-210; run.rs:4804-4815 | never reported to the host |
| Env and files into a member | env: `PATH`, `HOME=/tmp`, `SHARDS_DOMAIN`, then its OSI config's `run.env`; `/tmp` a scratch tmpfs of its own | domains.rs:487-500, 15-18 | nothing from the host |
| Published ports | the daemon binds each host port (dockerd's allocator rules) and hands the listening sockets to the VM's network process (`PUBLISH`) before the run; the network process opens each connection to the guest | daemon/publish.rs:1-12, 188; daemon.rs:5311-5367; D31 | `publish` is the daemon module's (`pub(super)`) |
| Egress grants | the Agentfile's egress ports, remote MCP hosts and DNS become the network process's one policy (`NET_POLICY`, a second refused, Landlock narrowing) | daemon.rs:3378-3452; D31 "As built"; D59 | none for narrowing: a policy is a list the host may shorten |
| Ingress to members | `EXPOSE … AS ingress FOR` names one receiver per incoming port (D122); init routes a published port to it | AGENTFILE_ARCH.md §4.1; D122 | none |
| Shares at boot | `--share DIR[:ro]` served by a share process the run's front starts (D38); DAX windows (D189); a run's `-v` at a declared `VOLUME` is given to the members that `VOLUME` is `FOR`, and to no other (D109) | cli/shares.rs; volumes.rs; domains.rs:102-125; D109 | none |
| Shares added to a running microVM | `plug volume`: the daemon starts a share process, sends `DEVICE` with its connection; the VM plugs a PCI slot (`--pci-slots N`); init's `DEVICE` built-in mounts it in the workload's mount namespace; no DAX window | D214; init/src/hotplug.rs | Not into a member's mount namespace; slots only where the machine was made with them |
| Stats | `builtin::STATS` through an exec: cpu, memory, inactive_file, limit, pids, read, write, rx, tx of the workload's cgroup | abi/src/run.rs:643; init/src/run.rs:882-954, 3359-3367; daemon/commands.rs:3340 | reads the workload's cgroup alone, not the members' (`/sys/fs/cgroup/run/domains`) |
| Stop | `Guest::stop` kills the VM process at once; no host-to-guest power-off | crates/api/src/lib.rs:500-508 | none |
| Templates and warm pools | the daemon keeps templates by content and pools of warm VMs per template | D25, D26; daemon.rs:7379 | per home, the daemon's |

## 3. Decision

### 3.1 One run, one host: `shards api-run`

An orchestrator's image run is a run of its own, hosted by one `shards` process per microVM,
which is to that run what the daemon is to each of its runs and nothing more.
`shards_api::image::ImageShards::boot` starts `shards api-run` (an internal command, not for
people, as `share`, `grants` and `spawner` are; main.rs:83-120) and talks to it over that
process's stdin and stdout; its stderr stays diagnostics, kept for a refusal to quote, as
`Shards::boot` keeps the direct boot's (client.rs:41-58). The run host:

1. reads the orchestrator's request from its stdin, where credentials travel and nowhere else
   (never argv, which every process of the user can read, never an environment variable);
2. prepares the run with the daemon's own functions: `run::prepare` (image, guest, Agentfile,
   spec), `volumes::open_for` and `setup::setup`/`security_setup`/`domain_seccomp`,
   `guest::server_device`, managed capture admission, the egress policy as the daemon makes
   it, narrowed by the request (§3.3);
3. starts the network process and the VM process with the builder's `launch`, made shared
   (§3.5), the VM booting cold into the image in its existing warm mode (`--warm`), its
   published ports bound by the daemon's `publish::bind` and handed to its network process
   before the run;
4. plays the daemon's side of the warm link for that one run: answers `READY` with `RUN`,
   waits for `TAKEN`, follows the run, and asks the VM what the orchestrator asks;
5. is the owner of the run's managed output, and relays each member's channels, each
   member's end and the run's end to the orchestrator.

Its children go with it. The VM process is told at `TAKEN` how its run stops if its link
closes (`STOP_ORDER`, warm.rs:1062-1078, as the daemon tells each of its runs), and the run
host gives SIGKILL with no grace: the workload's end ends the microVM (init kills the rest
and powers off, run.rs:4855-4873, 308). The network process goes as the VM's doorbell end does
(netproc.rs:210-216); the share process as its streams close. On Windows each is also in a
job that ends with the run host, as the direct boot's are (`GroupJob::ending_with_this_process`,
cli/mod.rs:1377-1382, D141). The run ends with its owner: the request's channel closing (the
orchestrator gone) has the run host kill the VM, so no microVM outlives the program that
owns it (managed-runtime-plan.md §4, "no fallback to an unowned process tree").

Alternatives, rejected:

- **The orchestrator's own process as the host** (crates/api speaking the warm link): the
  library would carry the daemon's preparation (image store, Agentfile checks, seccomp
  compilation), macOS's grant answering (`grant_answer`, which needs crates/apple) and
  Windows' AppContainer spawning, which are crates/shards' and which the requirement keeps
  out of a client of the binary (D215); R12 forbids the copy.
- **The VM process as the host**, prepared before it starts: it is confined (App Sandbox,
  AppContainer, seccomp and Landlock; D30, D141) and starts nothing, so a share added while
  it runs (a share process to start, D214) or a stats exec answered to an unconfined owner
  would have no party; and it would grow the process every microVM maps whole (PM M34).
- **The daemon** (its CLI or Engine API): the requirement; its records, pools and request
  slots are per home and its runs containers, where Hex owns each microVM's life as it owns
  each direct boot's.
- **One host serving all of an orchestrator's microVMs**: a daemon by another name, whose
  loss would end every microVM at once, and whose state a crash recovers. Kept as the
  fallback if the per-run host's measured cost (§8, R14) exceeds the daemon's per run.

Cost to measure (owed, R14): one more process per microVM, the run host. Its resident and
retained memory, CPU and the induced kernel cost are ROOT's to measure against the daemon's
share per run and against Firecracker's per-VM process.

### 3.2 The channel and its states

Frames on the run host's stdin and stdout carry the header the run protocol uses (a kind,
three zero bytes, a big-endian u32 length; abi/src/run.rs `header`, `parse_header`), payloads
encoded by `shards_api::image::wire`, which both sides take: crates/api owns the contract it
offers orchestrators, and crates/shards serves it. A payload past `run::MAX_PAYLOAD`, an
unknown kind or version, or a malformed field is a typed protocol error that ends the run,
never a default (R04). Credentials ride only in the create request; `Debug` of every type
that holds one prints its length, never its bytes.

States of an image guest, explicit (R04): `Requested → Preparing → Booting → Running →
Ending → Ended(End)`, and `Refused(Refusal)` from the first three. `Ended` carries how:
`Exited { status }` (the run's own command, or every member, ended), `Stopped` (`stop`),
`OwnerLost` (its channel closed), `Failed { why }` (its VM or host failed). Each member's
streams and end are separate: `Member { stdout, stderr: StreamState, ended: Option<status> }`.

### 3.3 The six needs

**1. Members' streams and ends.**
- *stdout and stderr* are managed capture's, unchanged in the guest (P3): exact bytes, a
  stream per member and channel, credit per channel. The run host is their owner, the role
  the daemon's endpoint plays (`capture::Given`, warm.rs:557-575): it takes each record, and
  grants each channel credit as the orchestrator takes its bytes, so a member whose output no
  one reads holds up neither its other channel nor any other member (P3's independence). The
  orchestrator reads each as a byte stream of its own.
- *Its end* is new: once a member's population has ended (init's `Ended { status }`, its
  leader reaped and its cgroup empty, managed.rs:122-210; leader exit and pipe EOF alone are
  not an end, plan §4), init sends `MEMBER_ENDED` (member, status as `docker run` reports it:
  code, or 128 plus a fatal signal), once; the VM process passes it to its owner; the run host
  to the orchestrator.
- *stdin* is designed here and owed (v1's client refuses it `Unsupported`): closed unless the
  orchestrator asks for it at create (P3's input policy: owned by an authorized session).
  Asked, init makes the member's stdin the read end of a
  pipe, and the member's input arrives on a connection of its own that init dials to the
  exec port with a token the setup names, as an exec's does (abi/src/run.rs:20-23, 79-88), so
  vsock's own credit backs it up and an unread stdin holds up nothing else; the VM process
  hands that connection to its owner as it hands the engine's (`ENGINE_CONN`, D181/D230).
  End of input closes the member's stdin.
- *A terminal* (pty) is designed and owed (G24): a pty pair made by init, its slave the
  member's stdio and controlling terminal in the member's own mount and PID namespaces, its
  master relayed over the member's input connection (input) and its stdout channel (output),
  resized by frames on it. Refused until built: `Refusal::Unsupported { what: "a member's
  terminal" }`, the same on every host (not an OS limit: no host has it yet).

**2. Env and files staged into a named member at create.** New setup entries name the
member by its declared index (agents first, then harnesses, as the Agentfile declares
them, D59): `member-env=<i>\0KEY=VALUE` replaces or adds a variable of that member's
environment after its image's (`SHARDS_DOMAIN`, the trusted label, refused);
`member-file=<i>\0<mode>\0<path>\0<bytes>` writes a regular file under the member's home,
its own scratch tmpfs (`HOME=/tmp`), at a relative path with no `..`, owned by the member's
uid and gid, its mode permission bits alone (no set-ID, no sticky), its directories made
0700, opened without following a link, before the member's program executes. They are held
to the spec's bound (`run::MAX_PAYLOAD`, `spec::fits`) and refused past it, typed. A member
the Agentfile does not declare, or one with no program (files alone), is refused by name.
**This is native-sign-in delegation, not opacity (G18):** the member reads every byte it is
given, as a harness signed in on a workstation reads its own auth cache; nothing is
brokered. The bytes live in the orchestrator's memory, the channel, the run host's and VM
process's memory, the vsock connection, init's memory and the member's tmpfs; no file on the
host, no image, no log, no Debug output. Opaque credentials (a trusted holder that never
gives the member the secret) remain plan P10's.

**3. Ports and grants.**
- *Published ports*: the request names each `(guest port, tcp|udp, host address, host
  port)`, host port 0 for one the OS picks. The run host binds them with the daemon's
  `publish::bind` (dockerd's rules: `SO_REUSEADDR`, `IPV6_V6ONLY`, D31) and hands the
  sockets to the network process before the run (`PUBLISH`); the answer says each bound
  address. Never widening: a guest port is published only where the image's Agentfile gives
  it one receiver for ingress (`EXPOSE … AS ingress`, D122), or where the run has a command
  of its own, which `docker run -p` reaches (AGENTFILE_ARCH.md §4.1, "a published port no
  member claims reaches the run's own command"); any other is refused by name.
- *Egress*: the image's grants as the daemon reads them (egress ports, remote MCP hosts,
  DNS; daemon.rs:3415-3452), narrowed by the request: the orchestrator may keep a subset of
  the egress ports, of the MCP hosts, and DNS or not; anything it names that the image does
  not grant is refused, not added. The network process takes that one policy
  (`NET_POLICY`), as it does a daemon run's.
- No network at all where the image grants nothing past the microVM and nothing is
  published: no device (D31, "absent device, no attack surface").

**4. Shares.**
- *At boot*: each `(host directory, guest path, read-only)` becomes the run's `-v
  HOST:PATH[:ro]` through `volumes::open_for` and `setup::setup`, served by a share process
  (D38); a DAX window as the daemon gives one (D189) is owed (v1's go through FUSE). A
  member gets it where the image's
  `VOLUME` at that path is for it (D109); a path no `VOLUME` declares reaches the run's own
  command alone. Never widening: no share reaches a member the image does not grant.
- *Added while running*: designed on D214's path (a share process started by the run host,
  `DEVICE` to the VM, a slot of its PCI bus, init's `DEVICE` built-in) and owed. Shards
  cannot give a running member a new mount: a member's mounts are made as it starts in its
  own mount namespace, and D214's built-in attaches into the workload's alone; refused
  `Unsupported { what: "a share added to a running member" }` on every host. A plugged
  share has no DAX window (D214). Where the machine was made without slots, nothing can be
  plugged: `--pci-slots` costs an x86_64 boot 4 ms at p50 on KVM (PM M430), so the request
  asks for slots at create.

**5. Stats.** `builtin::STATS` through an exec, as `shards stats` asks it, with one argument
added: no argument reads the workload's cgroup as today; `run` the run's cgroup
(`/sys/fs/cgroup/run`, its command and every member); `member <i>` a member's
(`/sys/fs/cgroup/run/domains/<kind>-<name>`). Its lines are typed by the api (`Stats`): CPU
ns, memory, inactive file, limit, pids, bytes read and written, network bytes in and out,
host CPU and online CPUs, each as init reads it; a line missing is `None`, never 0.

**6. Stop.** `stop()` kills the VM process at once, as `Guest::stop` does (no host-to-guest
power-off in the VMM, D215); the run host then ends, its children with it. The end is
`Stopped`. A graceful end is the orchestrator's: ask its member to finish and wait for its
`MEMBER_ENDED`.

**The run's own command.** An orchestrator names one or none. With none, the run's command is
a new built-in, `ATTEND`: the standby holds, as `HOLD` holds (D37), but the image's members
start (`HOLD` starts none, run.rs:217-221). The run ends when the orchestrator stops it, or
its owner goes.

### 3.4 What the orchestrator sees

`shards_api::image` (crates/api/src/image.rs): `Images` (`backend`, `boot`); `ImageGuest`
(`members`, `published`, `take_output(member, Channel)` a pipe's read end once each,
`take_input`, `wait_member`, `stats(StatsScope, patience)`, `wait_ready(seen, timeout)` (the
nesting grant's readiness, D234), `stop`, `wait_for`);
`ImageSpec` (`image`, `vcpus`, `memory_mib`, `command`, `members: Vec<MemberSpec>`,
`publish`, `egress: Narrowing`, `shares`, `binary`, `env` of the run host's own process,
and D234's `nesting`, the run's consent to the nesting grant, and `ready_by`, its deadline's
budget);
`MemberSpec` (`member: MemberName`, `env: Vec<(String, Secret)>`, `files: Vec<MemberFile>`,
`stdin`, `terminal`); `Secret` (bytes, `Debug` by length); `MemberEnd`, `RunEnd`, `Stats`;
`ImageRefusal`, its own typed enum, so that D215's `Refusal` keeps its variants. The channel
is `shards_api::image::wire`, which `shards api-run` serves. `ImageShards` is the client of
the binary; a simulator of image guests is owed (§5).

### 3.5 Shared, and where v1 is not yet

- Shared and reused as they are: `run::prepare`, `volumes::register`/`open_for`,
  `setup::setup`/`security_setup`/`domain_seccomp`, `guest::server_device`/`version_of`,
  `netproc::start`, the Agentfile's `refuse`/`caps`, `shards_net::encode_policy`, the warm
  link's messages (`RunHead`, `READY`, `RUN`, `TAKEN`, `STOP_ORDER`, `EXEC_RUN`), the VM's
  managed capture; `daemon::publish` made `pub(crate)` for its binding.
- **Not yet shared (R12 gap, owed):** the run host's Unix launch of its VM process
  (api_run.rs `launch`) takes the same steps as the builder's (build/builder.rs:570-640) and
  the daemon's (daemon.rs:8109): fds at fixed numbers, macOS grants answered in-process,
  the network side handed. Folding the three into one launch is owed; v1 adds a third copy
  rather than change the builder's and the daemon's under other lanes' work.

### 3.6 Literals (R03)

None new at run time. Each bound is an existing one: `run::MAX_PAYLOAD` (Linux's exec bound,
abi/src/run.rs:31-41) for a request, a staged member's entries and every frame; the exec
port's token (`run::TOKEN`, 16 bytes) for an input connection; vsock credit and
`run::BUFFERED` for input backpressure; the mode mask `0o777` is the permission bits by
definition (inode(7)); `128 + signal` is `docker run`'s status rule (abi/src/run.rs:61-65).
The run host waits on facts (the VM's `READY`, `TAKEN`, the VM process's exit, its channel's
end), never on a clock of its own; a stuck boot is the orchestrator's to bound, as a direct
boot's is (D215): `Images::boot` takes its patience, and a boot it outlasts is refused with
what the run host said last.

## 4. Per host

What v1 builds (code), against what is owed. On 2026-11-12 macOS's v1 row ran on real VMs,
`crates/shards/tests/api_image.rs` 6 of 6; Linux x86_64's boots ran on CI and failed
before the fixes, its run at the landed revision owed
(measurements/runtime-image-boot-v1/README.md).

| Host | 1 stdout/stderr, end | 1 stdin | 1 terminal | 2 staging | 3 ports, grants | 4 shares at boot | 4 added | 5 stats | 6 stop |
|---|---|---|---|---|---|---|---|---|---|
| Linux x86_64 KVM (gnu, musl) | v1 | owed | owed | v1 | v1 | v1 (FUSE, no DAX window) | owed (to the command); members cannot | v1 | v1 |
| Linux arm64 KVM (gnu, musl) | v1 | owed | owed | v1 | v1 | v1 (as above) | as above | v1 | v1 |
| macOS arm64 HVF | v1 | owed | owed | v1 | v1 | v1 (as above) | as above | v1 | v1 |
| Windows x86_64 WHP | owed: the run host refuses `Unsupported` | owed | owed | guest side v1 | owed | owed | as above | owed | the client's v1 |
| Windows arm64 WHP (D209) | as x86_64 | owed | owed | guest side v1 | owed | owed | as above | owed | as x86_64 |
| macOS x86_64 | refused `NoBackend`, as D215 | | | | | | | | |

Windows is owed, not refused for a limit of its own: its run host needs the AppContainer
launch with its network process started on the suspended VM and its link, grants and share
streams handed by name (D141, D142, D197, as build/builder.rs:646-728 and daemon.rs:8270 do
for theirs), which v1 does not yet write; `shards api-run` refuses there with
`Cause::Unsupported` naming it, and the client, the wire, init and the VM process's changes
compile and are the same there. The guest side is Linux on every host, so the init pieces
are one implementation. An orchestrator's identity and `attach` after its own crash (D215's
`Guests::attach`) do not carry over in v1: the run ends with its owner (§3.1);
re-ownership is P11's (owner transfer).

## 5. Built in v1, in committed steps

1. This record and the D-entry.
2. abi and ipc: `kind::MEMBER_ENDED`, `builtin::ATTEND`, `builtin::stats` scopes, the
   staging entries' codec and bounds (`run::members`), `member-ends`; ipc's `MEMBER_ENDED`.
3. init: staging applied as a member starts (`staged.rs`: files into its scratch tmpfs, by
   its descriptor, before its clone; variables over its image's); `MEMBER_ENDED` once a
   member's population has ended, where `member-ends` asks; `ATTEND`; `STATS` by scope.
4. The VM process passes `MEMBER_ENDED` to its owner (workload.rs, warm.rs); the test guest's
   `staged` mode.
5. crates/api: the `image` module, its wire codec, `ImageShards`, `ImageRefusal`.
6. crates/shards: `shards api-run` (prepare, network, publish, egress narrowing, shares at
   boot, the warm link's daemon side, the capture owner, stats, stop) on Linux and macOS;
   real-VM E2E tests (`crates/shards/tests/api_image.rs`), written for ROOT to run.

Owed beyond v1, each its own slice: a member's stdin (its input connection, §3.3, refused
`Unsupported` by the client today) and terminal (G24); the Windows run host (§4); shares
added while running (D214, to the run's own command); DAX windows for an orchestrated run's
shares (D189: v1 passes no `--share-window`, so its shares go through FUSE); templates and
pools for orchestrated runs (D25, D26: a cold boot each run until measured); `attach` after
the owner's crash; the run's own
command's stdout and stderr as streams of their own (v1 gives them the run host's stderr,
diagnostics); the launch shared with the builder's and the daemon's (§3.5); a simulator of
image guests for orchestrators' tests.

## 6. Tests that hold it (acceptance, not results)

- *Unit*: the wire codec round-trips every message and refuses each malformed one by type;
  `Secret`'s Debug never prints its bytes; staging entries refuse `..`, absolute paths, set-ID
  bits, `SHARDS_DOMAIN`, an unknown member, past-bound sizes; narrowing refuses widening;
  `STATS` scopes; `MEMBER_ENDED` once per member.
- *Real VM* (ROOT, per host): an Agentfile image with a harness that prints its staged env and
  file to stdout and stderr and exits 3, booted with no daemon running: exact bytes on each
  channel, `MEMBER_ENDED 3`, no file of the staged bytes on the host; a member reading its
  stdin; a published TCP and UDP port reaching the member that `EXPOSE` names, and one no
  member claims refused before boot; an egress narrowed away refused at the network
  process; a read-only share at a `VOLUME` the member is granted and refused where it is not;
  stats by scope; `stop` ending the VM, the network and share processes gone; the owner's
  channel closed ending the run.
- *Mutation*: removing the narrowing's subset check lets an ungranted egress port through;
  removing the member-index check stages into the wrong member; removing `MEMBER_ENDED`'s
  population check reports an end while a descendant runs.

## 7. What would falsify it

A run host whose resident cost per microVM exceeds the daemon's per run on any host (then one
host per orchestrator, §3.1); an orchestrator that needs to link the VMM; a member whose
credentials must not be readable by it (then P10's opaque broker, not staging).

## 8. Owed to ROOT

All real-VM E2E above on KVM x86_64 and arm64 (nested on an M3+ Mac for arm64 per CLAUDE.md),
HVF arm64, WHP x86_64 (and WHP arm64 where its backend boots); the seven-target lint; the run
host's cost (R14: resident, retained, CPU, kernel, against the daemon's per run and
Firecracker's); boot-to-member-running latency of an orchestrated run against a daemon run
of the same image (cold and pooled), n with p50/p90/p99/max.

## 9. Open questions for the person

1. An orchestrated run ends when its owner's channel closes (§3.1). Should a microVM instead
   outlive its owner's crash for a later `attach`, as a direct boot does on Unix? That needs
   an owner-transfer contract (P11) and a choice of who may take it over.
2. Should a published port that no member claims reach the run's own command where the
   orchestrator gives one, as `docker run -p` does, or be refused for orchestrated runs
   whatever their command?
