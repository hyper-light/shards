# Every decision of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, and where it is recorded

Plan step 37 ("Docs and decision records"): every decision the plan's 39 steps made or need,
where it is written down, whether its evidence is linked there, and what is still open and
whose it is. ROOT's final pass after step 34 checks this table again.

- **Read at:** first pass, Shards main `e7825cb32` (checked again at `dc7d36014`), kiln main
  `d6ffa45d8`, Hex main `d84730ee`, 2026-11-12 about 15:45 PST; second pass (section 4), Shards
  main `1145bd3f2`, kiln main `e86ec22c6`, Hex main `106c7b94`, 2026-11-12 about 18:40 PST, then
  each landing after it as section 4 lists (last: Shards `31662d5d2`, kiln `fe9c17c7d`, Hex
  `cf2d6c32`); the plan at version 11 and ROOT's goal stack throughout. Branches not on main are named as such and
  count as nothing landed.
- **Recorded:** the repository and file, then the section. "arch" is Shards'
  `docs/design/architecture.md`.
- **Evidence:** *linked*, where the record names or links committed evidence (logs, tests,
  sources); *in record*, where a committed evidence record exists but the design record does
  not point at it; *none*.
- **Status:** decided, built, owed (what, whose) or open (who decides). "ROOT" is Shards ROOT,
  the plan's coordinator (kiln agent `01a148884a4770fdaf0111f3f792a248`); "Hex ROOT" is
  `01a1aa77778471339bea3beff90d6bbd`.
- **(s37)** marks a record this step wrote or completed (section 4).

## 1. The person's decisions (the plan's header)

| # | Decision | Recorded | Evidence | Status |
|---|---|---|---|---|
| P1 | Shards runs the harnesses: one microVM per harness under shards-init, Hex the runtime outside it ("Shards runs these. Literally why we're testing that functionality.") | arch D231; Hex `docs/design/shards-image-class.md` | linked: runtime-image-boot-decision-v1, runtime-image-boot-v1, `tests/api_image.rs` 6/6 on KVM x86_64, HVF, WHP x86_64 | decided; built (D231 on main; Hex's class built to Shards `e82b9403`, Hex main `325b01cf`); KVM arm64 and WHP arm64 runs owed (ROOT) |
| P2 | Hex owns TCP/UDP for Shards VMs: Hex decides each VM's policy, Shards' per-VM network process enforces it | Hex `network-grants.md` N3; arch D231 "Ports and grants", D236 (an orchestrator keeps or drops destinations, never adds) | linked | decided; Shards' half built (D236, `edbada623`); Hex vendors it with Shards `e82b9403` (Hex `f0179240`), its programming of destinations by name and reading of `Bound::to` owed (Hex step 15) |
| P3 | Hex charges a microVM by its measured use, its envelope the ceiling ("For 14 - of course go") | Hex `learned-charge.md` "The person's choice"; arch D43 "Each vCPU's guest time" (s37), D188 | linked: kiln research `01a1cd8f5b8b77649ccb95ad26875842`, guest-cpu-time-v1 | the charge decided by the person; its rule (C1-C7) and the step 13 interface (C8) taken by ROOT 2026-11-12; built in Hex (`b19ef663`); density and fairness owed (ROOT) |
| P4 | "all such communications should be AESGCM256 bidirectional encrypted" | arch D239 (s37), D180, D60; kiln `crates/kiln/src/reach/e2e.rs` `PROVIDER` (`d4990d3b`); Hex `hex-quic.md` Q5 | linked: aes256-only-v1 | built on every Shards TLS link (step 3); real-VM runs owed (ROOT); control that crosses no TLS decided by ROOT (2026-11-13, D238 "Control that crosses no TLS", `1b6e1203d`): the directive covers every network hop of control, and the request slots, `engine.sock` and a host's vsock and AF_UNIX legs to its guests never leave the host |
| P5 | Agents are controlled over networking, on hyper-quic, which Hex and Shards both offer and use | arch D238 (s37; its endpoint's text step 11's); Hex `hex-quic.md` | Shards: step 10's harness and ROOT's results linked (PM M540); step 11's endpoint linked (agent-control-v1); Hex: linked | Shards: TLS ends at the host, decided by ROOT (step 10, `1b6e1203d`), standing until WHP, KVM's memory at more rounds and a loaded run are in (ROOT); the endpoint's connection, pinned key, refusals and lanes' first frames built (step 11's slice 1, `2a3b62356`, `4a281e016`), its stdio lanes' service built (slice 2, `c9630c6b4`, `a474b60cd`), the rest building (step 11); Hex: designed (Q1-Q9), its conformance over QUIC is step 4's (Hex ROOT) |
| P6 | Hex and kiln's load balancer are peer to peer and coordinate the fleet | Hex `capacity-view.md` CV1-CV11; kiln `docs/lb.md` (no joint view) | Hex: linked (hex-sim's nemesis, the record sweep) | proposed 2026-11-12; Hex's cores, seam and `hex schedule serve` built; kiln's record and holder owed (step 13, kiln's Load Balancing agent) |
| P7 | No arbitrary constants: every size learned or derived, varying with fleet scale and hardware; a microVM may have more than one vCPU | Shards `docs/development-rules.md` R03 and each D-entry's literals table (D230's rings learned per stream, D191's every host CPU); Hex `learned-charge.md` "Every number", `hex-quic.md` Q8; kiln `AGENTS.md` ("Sized by measurement") | per entry | standing rule |
| P8 | Credentials in Veil's shape: placeholders in the VM, real values filled in at its egress, refresh outside, tmpfs only where nothing else works, Veil later replacing only the backend and the injector | the plan (body, step 33); kiln `docs/seams.md` "Secrets"; kiln's built-in Agentfiles (no credential in an image); kiln `docs/agents.md` (s37) | none for the injector | the shape decided (ROOT's gate `22dfc46c`, from the person's naming Veil); building (step 33: a checkpoint on `s33-checkpoint-v1`, nothing on main); its D-entry owed by step 33 |
| P9 | Users keep their own Agentfiles in kiln, with versions, managed in a UI | kiln `docs/api.md` "Agentfiles"; `docs/agents.md` "From an Agentfile, in a microVM of its own" | in record (merge commits `c09d23207`, `3a4b09005`) | built; step 30's round trip against the running server open (a deploy, the person's word: kiln `AGENTS.md`) |
| P10 | Work agents start from inside their VMs goes through kiln's load balancer | kiln `docs/runners.md` and `docs/ci.md` (`virt.microvm`), `docs/needs.md` (`microvm`); `p/s32-rest-v1` adds `docs/agents.md` and `docs/api.md` text (not on main) | code tests (`harness/lineage.rs`) | placement and isolation's inheritance built; charging and remote commands owed (steps 16, 17, 32) |

## 2. The architecture (the plan's body)

| # | Decision | Recorded | Evidence | Status |
|---|---|---|---|---|
| A1 | Control plane kiln ↔ Hex: hyper-transport on hyper-quic, AES-256-GCM only, identity from kiln's CA by CSR, Hex's calls on lanes | Hex `hex-quic.md` Q1-Q9 | linked | Hex's (step 4, Hex ROOT) |
| A2 | Data plane kiln ↔ each harness's VM: a stream per stdio, a terminal lane, a control lane, a log lane; the VM's own key replaces kiln's peer-process identity | arch D238 (s37) "Decided", "Building: the endpoint" | the plan; agent-control-v1 | decided; the endpoint's connection and its stdio lanes built (step 11's slices 1 and 2; rows 11, 11a, 11b); its control and terminal lanes wait on step 6; with step 10 decided, the endpoint in the daemon, each run's key held there and its SHA-256 in the run's managed metadata, and the log lane are step 11's to build (D238) |
| A3 | Where encryption ends is measured before it is decided | arch D238 "Where its TLS ends: at the host"; PM M540; `docs/research/measurements/control-end` | linked: PM M540, ROOT's summaries in `control-end/results/` (harness `d9caccfe8`..`4bf2fe812`, run at `66113379c`) | measured on HVF and KVM, then decided at the host by ROOT (`1b6e1203d`), the round trip, which favours the guest, recorded beside it; WHP, KVM's memory at more rounds, a loaded run on each host, HVF's distributions and the raw records owed (ROOT, M540); D238 lists what reopens it |
| A4 | Every link `TLS13_AES_256_GCM_SHA384` only, with a test asserting it; QUIC's Initial packets stay AES-128-GCM (RFC 9001 §5) | arch D239 (s37), D180 | linked | built (step 3) |
| A5 | Hex's scheduler gossips capacity over hyper-swim; kiln's balancer decides at the node that asks | Hex `scheduler-architecture.md`; kiln `docs/lb.md` | linked | each built on its own side |
| A6 | One shared capacity view per machine: each side publishes its reservations, each admission checks the combined view | Hex `capacity-view.md` | linked | as P6 |
| A7 | Hex sets budgets per fleet; D188 tells each guest its room; everything a VM holds, ring pins included, comes out of it | arch D188 "Ring pins come out of the room" (s37), D230; Hex `learned-charge.md` C7 | linked: ring-pins-v1 | pins within the room built (step 2); an orchestrated run's room owed: shards-api carries no `ROOM`, so Hex refuses it typed (`Awaits(GuestRoom)`); Hex vendors a Shards with ring-pins-v1 since Hex `f0179240` (Shards `e82b9403`); its `Awaits(RingPinsWithinRoom)` and `Awaits(GuestRoom)` lift with Hex steps 13 and 14 (D241, on `p/hex-s13-s14-v1`, not on main) |
| A8 | Users set budgets and VM sizes, never internal slots | arch D230 ("No user knob"); Hex `learned-charge.md`; kiln `p/s32-rest-v1` (`spec.isolation`'s `cpus`, `memory`; not on main) | — | Shards built; kiln pending |
| A9 | One Agentfile per harness: a pinned official Linux build, HARNESS and harness.json, DNS and egress grants, the project at its host path, the config dir on a per-user volume | kiln `.kiln/images/harness-*/` (each Agentfile's comments), `docs/research/s19-*` to `s28-*`, `docs/agents.md` | linked per step | built for all ten; three parts not yet as decided (rows 19-28) |
| A10 | Agentfiles layer over any base image, so a project's toolchain image works | kiln Agentfiles' `ARG BASE` (`buildpack-deps:trixie-scm` by digest); `docs/agents.md` (s37) | s19's research | built |
| A11 | Linux guests can't run a host OS's own tools (Xcode, MSVC): such sessions stay on the host, or are refused where isolation is required | kiln `docs/agents.md` "What an isolated agent can't do" (s37) | the plan | decided; kiln refuses a process under an isolated parent (`isolation_for`); nothing detects a need for such tools |
| A12 | Shards' managed-runtime plan owns the deeper member work: steps 5, 6 and 33 are its P3, G24, G18 and P10 | Shards `docs/design/managed-runtime-plan.md`; ROOT's gate `e7fa1b5c` | — | recorded; those lanes frozen, their 191 branches stale since 11-11 |

## 3. The steps

| Step | Decision | Recorded | Evidence | Status |
|---|---|---|---|---|
| 1 | Credit a guest at half its own window; answer every credit request | arch "D12's credit and window" (s37) | linked: vsock-credit-v1 (r416, r423) | on main; HVF and WHP real-VM tests owed (ROOT) |
| 1 | A window lent where it is used (64 KiB least, 4 MiB at the measured knee, lent device-wide); host socket buffers 256 KiB | arch "D12's credit and window" (s37) | linked: vsock-window-v1 (r429, r433) | on main; the Linux socket sizing (`confine.rs`) open, ROOT's |
| 1 | The engine's bytes through a ring in guest memory | arch D230; `engine-ring-channel-decision-v1.md` | linked: engine-ring-v1, ring-pins-v1 | on main, "decided; building"; correctness, hostile-offer and throughput measurements owed (ROOT) |
| 1 | The audit's fixes: init's io.stat lines, the in-VM server's `Connection: close`, `read metrics` waits, the tests' checks, the stored request | `docs/research/measurements/audit-fix-{init,server,host,tests,request}-v1`; the request's also arch D37 (s37) | in record | on main; arch points at the request fix alone: the init, server, host and tests records name no D-entry (their `EVIDENCE.json`, `index.json`, scripts and logs), so none is pointed at and none invented; each keeps its reproduction, fix and mutations in its own directory |
| 1 | Nothing reaches main without combined validation | ROOT's goal `3268f6d8` | — | open (ROOT): the 8 CI triples' lint, the suites on KVM, HVF, WHP and nested arm64 KVM, dev CI watched |
| 2 | Hex's 31 Shards patches upstreamed (hex-upstream-v1) | arch D232, D233; hex-upstream-v1/README.md | linked | on main (`482c5d78`); Hex vendors Shards `e82b9403` since Hex `f0179240`, still with no patch to `vendor/shards` and one held packaging patch to `vendor/shards-gpu` (Windows arm64 GPU, D232 "Held") |
| 2 | An Agentfile image booted for an orchestrator (runtime-image-boot-v1) | arch D231; runtime-image-boot-decision-v1.md | linked | on main; KVM arm64, Windows arm64 and R14 costs owed (ROOT) |
| 2 | Ring pins on demand, freed on the device's DETACH, within D188's room, learned per stream, at 1, 2 and many vCPUs (ring-pins-v1) | arch D230 (its record linked, s37), D188 (s37) | linked: ring-pins-v1 r1-r16 | on main; WHP arm64, WHP x86_64 past the daemon's Windows bound, hostile offers and measurements owed (ROOT) |
| 2 | Each vCPU's guest time and a monotone total on HVF, KVM, WHP (guest-cpu-time-v1) | arch D43 (s37) | linked | on main; real-VM test per backend owed (ROOT); shards-api's image `Stats` has no field for it (Hex awaits `GuestCpuTimePerVcpu`) |
| 2 | Every record a reader decodes written whole or absent (request-sites-v1) | arch D37 (s37) | linked | on main |
| 2 | Real Codex and Claude Code start as HARNESS members (harness-bringup-v1; signed-in turns, v2) | arch D180 (H1), D236 (H2); `docs/examples/harness-{codex,claude}`; harness-bringup-v2/README.md | linked (v1); v2 in record | on main; v2's sign-in findings are step 33's input |
| 3 | AES-256-GCM alone, both ways, on every Shards TLS link | arch D239 (s37), D180, D60 | linked: aes256-only-v1 (`e12d5b96e`, `2a328f37e`, via `04abb68d4`) | built; macOS runs, Windows and aarch64 builds, 7-target lint and real-VM E2E owed (ROOT) |
| 4 | Hex's wire over hyper-transport: lanes, AES-256-GCM, CSR enrollment, learned waits | Hex `hex-quic.md`; Hex `docs/design/README.md` | linked | built on Hex main, a peer's ended half read to its end though a reset follows (Hex `325b01cf`); its conformance suite over QUIC on Linux, macOS and Windows open (Hex ROOT; ROOT's goal `d8b5e77b`) |
| 5 | A member's stdin a stream its owner holds; stdout and stderr captured whole | arch D180 "A member's stdin" (completed, s37), D231 (corrected, s37), D240 (s37) | linked: member-stdin-v1 | built for the daemon's owner and the orchestrator (`e7825cb32`); real-VM E2E on KVM x86_64, KVM arm64, HVF, WHP and R14 costs owed (ROOT) |
| 6 | exec, attach and a resizable TTY, confined as the member is | arch D240 "Building" (s37, slice 1 pointed at) | linked: member-exec-tty-v1 | slice 1 on main (`78fc1c8f5`, `532ad27c5`, via `de4897668`): a member starts with its signals at their defaults, as an exec's child does, reproduced, fixed and its guard mutated on HVF; exec, attach and a resizable TTY building; how is step 6's implementer's, ROOT reviews; `tests/member_exec.rs` on KVM x86_64, WHP and arm64 KVM, and init's unit test on arm64 Linux, owed (ROOT) |
| 6 | Exec connections admitted under contention and said when lost (ROOT's exec-hang goal, found by step 2) | arch D16 "An exec's connection" (s37) | linked: exec-admission | on main (`0d4f20288`, `2944d9ab5`); its WHP result, Linux arm64 KVM and WHP arm64 owed |
| 6 | The signal link's explicit states | arch D16 "Signals"; PM M538 | linked: signal-link, restored-kill | on main (`3607aad11`), its sends made off the link's lock by a sender thread of their own (`782275c26`); PM M538 (`1145bd3f2`): CI's one Windows `rm -f` timeout before the fix fits the losses `3607aad11` made typed, "Not closed as the signal link's bug"; its after-fix windows-2025 runs owed, as M538 lists |
| 7 | A member's environment: init's, the image's `Env`, its `run.env`, its run's | arch D59 "A member's environment", D240 (s37) | linked: s7-home-workdir | on main (`239749c60`); real-VM runs owed (ROOT) |
| 7 | A writable, private HOME: its scratch or a volume; staged files under it; no volume at or under `/tmp` | arch D231 "Staging", D109 "A domain's `/tmp` is its own", D240 (s37) | linked: s7-home-workdir, s7-scratch-volume | on main (`ac9bb8589`, `81d2a6a2a`); real-VM runs owed |
| 7 | The working directory the run names (the project path) | arch D231, D240 (s37) | linked | on main (`d6ac7e48f`); a boot-time project path owed (D235's and the orchestrator's to choose) |
| 7 | A member's root keeps the image's directories' owners and modes | arch D59 | linked | on main (`cfcb22755`); real-VM runs owed |
| 7 | `run.env` sets HOME and PATH, the last given winning | arch D59 (`997f4e47e`) | linked | on main |
| 8 | A project plugged into a running member at its host path | arch D235 | linked: s8-project-paths, s8-notices | plugs built (`50ed65bbb`, `2955d2917`); names that differ in case alone on case-insensitive hosts, and host edits' notices to the guest's inotify, built (`c654d53a3`, `373079629`, via `eebae61e6`), their real-VM tests run on KVM x86_64 and HVF; WHP and arm64 KVM runs, the notices' latency (`s8-notices/run.sh`) and `name_valid`'s LOOKUP cost on macOS and Windows owed (ROOT); the notices bound by the guest's own inotify queue (R03) on `p/s8-r03-v1`, WIP, not on main |
| 9 | Egress destinations by name; kiln's MCP reached only by an explicit grant; build and run checks alike | arch D236 | linked: s9-grants-v1 | on main (`edbada623`); real-VM runs owed (ROOT) |
| 9 | A published port names the member it reaches (`Bound::to`, Hex step 15's finding) | arch D231 "Ports and grants" | linked | on main (channel version 3) |
| 10 | Where agent control terminates | arch D238 "Where its TLS ends: at the host" (ROOT's); PM M540 | linked: control-end and its `results/` | decided at the host by ROOT from HVF and KVM (`1b6e1203d`, via `1caf0e3fa`), to stand until WHP, KVM's memory at more rounds and a loaded run are in; those runs owed (ROOT, goal `42b60c13`); why arm g's guest on KVM spends more CPU per handshake than its controller is unmeasured (D238) |
| 11 | The agent-control endpoint: control, terminal, stdio and log lanes; the VM's key enrolled or pinned | arch D238 "Building: the endpoint" (s37; its text step 11's) | linked: agent-control-v1 | slice 1 on main (`2a3b62356`, `4a281e016`, via `455f8cd1a`): the connection (ALPN `shards-control/1`, D239's suite), the pinned key, typed refusals (`control::Ended`), a lane's first frame and its refusal, 6 of 6 mutants killed; slice 2 on main (`c9630c6b4`, `a474b60cd`, via `60c5dfbc6`): a microVM's members' stdin, stdout and stderr as lanes of its controller's one connection, served by step 5's services, a newer connection superseding the older at its cursors; each slice's light tests on Linux x86_64, macOS arm64 and Windows x86_64, clippy on the seven targets; light tests on aarch64 Linux and Windows owed (no native host); slice 2's guard mutations written, not run, one marked UNCERTAIN, ROOT's to run (`agent-control-v1/mutate.py`); the rest in kiln plan `01a1d2c733ea7731bb3204cad78bc94d` (the controller's side of the stdio lanes, the endpoint in the daemon where step 10 put it, exec, signal, inspect and terminal lanes on step 6, the log lane); step 11's endpoint to be measured with step 10's harness against arm g's numbers (ROOT, D238); every real-VM run, R14's costs and the adversarial campaign owed (ROOT) |
| 11a | The microVM's identity: its own Ed25519 key, made where TLS ends after its run's restore, its SHA-256 its name, pinned by its controller, not enrolled by CSR; at the host (step 10), the key is held by the daemon's endpoint, never in guest memory, its SHA-256 in the run's managed metadata; step 33's leases rely on one key per run, known before the run's first lease ask on each host, unchanged while the run stays on one host | arch D238 "The microVM's identity: its own key, pinned (decided)" | linked: D238's comparison (kiln's and Hex's CSR enrollment, kubelet bootstrapping, RFC 7250, WireGuard), agent-control-v1 | decided by step 11 (`2a3b62356`), where the plan left it open; ROOT reviews; D238's step 10 text (ROOT, `1b6e1203d`) builds on it; D238 names what would falsify it (a party that did not start the microVM must verify it: then a certificate is added for the same key) |
| 11c | A moved microVM's key, now that the host's endpoint holds it: carried to the new host's endpoint (which the plan's "the key never travels" refuses), or a new key there, pinned anew, a new name for step 33's leases unless the move reissues them | arch D238 "A moved microVM's key" (ROOT's, `8710cb12a`, `d5bc8a3d0`, via `a22d060b5`) | D238: a run's key made after its restore, as at any run's start; the plan's "the key never travels"; step 33's owner agreed (2026-11-13) | decided by ROOT (2026-11-13): a move makes a new key at its destination after the restore there, its SHA-256 to the controller in the run's managed metadata, the old key never carried; step 33's leases released at the source and asked anew under the new fingerprint, which the orchestrator has before the destination's first lease ask; what kiln's other uses of the key (step 18) do at a move is step 18's (D238) |
| 11b | The wire: hyper-quic under D180's driver, each lane a QUIC stream, not hyper-raft's hyper-transport crate | arch D238 "The wire" | linked: D238 (hyper-raft `233b7fce` `lane.rs`, Hex `hex-quic.md` Q3, RFC 9000) | decided by step 11 (`2a3b62356`), where the plan's words were "hyper-transport on hyper-quic" (A1, A2); ROOT reviews; Hex and kiln take Shards' controller (`control::controller`) through its crates |
| 12 | Hex's Shards class on Agentfile images under shards-init | Hex `shards-image-class.md` | linked | built to Shards `e82b9403` (Hex `325b01cf`): a macOS or Windows node's image-box calls kept on its guest runtime (`8b03ea45`), a Windows image VM awaiting a run host that holds only its stdio (`dd46132b`); its waits' records, real-VM runs and macOS and Windows hosts owed; a run host that holds only its stdio on macOS and Windows owed by Shards (D231; Hex's `Awaits(RunHostStdioOnly)`) |
| 13 | One capacity view per machine | Hex `capacity-view.md` | linked | as P6 |
| 14 | Learned charge, a contention rule, budgets per fleet | Hex `learned-charge.md`; arch D43, D188 (s37) | linked | as P3; an orchestrated run's room owed on shards-api's wire (A7) |
| 15 | Network grants for Hex's microVMs | Hex `network-grants.md` | linked | decided 2026-11-12; `network-grants.md` updated for step 9 (Hex `ba4bf8eb`); programming D236's destinations and reading `Bound::to` owed (Hex step 15, ROOT's goal `c627cc94`) |
| 16 | kiln vendors Hex and Shards at one Shards revision; a carried `shards` starts its helpers beside it, signed as kiln signs its own | arch D237; kiln `docs/shards.md` and `docs/vendoring.md` on `p/s16-vendor-v1` (not on kiln main) | in record (kiln's branch) | Shards' half built (`d42407036`), real-VM runs owed; kiln's record, build time and size owed with its landing |
| 17 | Harnesses launched through Hex, their Duplex the member's stdio over hyper-transport | kiln: nothing on main (`p/s17-launch-seam-v1` begun; T4's `docs/isolation.md` on adacachy's `cp/t4-isolation`) | — | building; until then a machine's kiln refuses an isolated agent (`harness/host.rs`) |
| 18 | What kiln assumes of the host holds in the VM, or is refused with why; auth and reach failures surfaced as they happen (bring-up H6) | kiln `docs/agents.md` (s37: what holds today); harness-bringup-v1 H6 | in record | building (`p/s18-host-in-vm-v1` begun) |
| 19-28 | Ten built-in Agentfiles, each with the publisher's official Linux build pinned by URL and published checksum for amd64 and arm64 | kiln `.kiln/images/harness-*/` (`pin.json`, `pin.sh`), `docs/research/s19-*`..`s28-*`, `docs/agents.md` notes (agy, Gemini) | linked per step | on kiln main; image builds and `--version` smokes per arch, and Windows clippy of later revisions, owed (ROOT); step 20's goal met on kiln main through `2bc6f6dd6` (Codex's arm64 tree's `skip` read by the one unpacker, every built-in's pin read through `unpack::Spec`; record `docs/research/s20-codex-agentfile/step3.md`), its image builds and `--version` smoke on amd64 KVM and arm64 HVF owed (ROOT) |
| 19-28 | The per-user volume is the harness's whole HOME, with HOME in `run.env` (ROOT, 2026-11-12, from step 26) | Agentfile comments; `docs/agents.md` notes (agy, Gemini); `docs/agents.md` (s37) | in record | followed by Claude Code, agy, Cursor, Gemini, opencode and Amp; Codex (`/home/codex/.codex`), Devin (two folders), Goose and Pi (their data folders) mount a folder of HOME instead: a finding for ROOT |
| 19-28 | The project at its host path; providers by name | Agentfile comments ("until step 8", "Step 9 grants its providers by name instead") | — | not yet: each mounts `/workspace` and grants TCP 443 to any address; Shards has both (D235, D236); kiln's built-ins take them once kiln carries such a Shards (step 16) |
| 19-28 | One unpacker for every harness's archives (ROOT, 2026-11-12: step 26's) | kiln `crates/kiln/src/agentfiles/unpack*`; `docs/research/s26-agy-agentfile` | linked | on kiln main (`6e48ee58b`) |
| 19-28 | kiln's image build copies `.kiln/images/` (step 25's finding) | kiln `Dockerfile` (l. 47-48), held by `agentfiles/builtin.rs` | linked | on kiln main |
| 29 | Cursor CLI (ACP) and Pi (its RPC mode) as kiln harnesses | kiln `docs/agents.md` (harness table, sign-in, notes) | linked: transcript tests | on kiln main (`efb40ea0f`), `verified: false` until run against real installs |
| 30 | The Agentfile store and its API (REST, MCP, code mode), owned and versioned, checked by Shards | kiln `docs/api.md` "Agentfiles" | in record | built (`c09d23207`); its live round trip open (the person's word to deploy) |
| 31 | The Agentfile management UI | kiln `docs/agents.md` "From an Agentfile, in a microVM of its own" | linked | built (`3a4b09005`); ROOT's goal open |
| 32 | The balancer places microVM agents and what they start; an isolated agent's remote commands never on a bare host | kiln `docs/runners.md`, `docs/needs.md`, `docs/ci.md`; `p/s32-rest-v1` (not on main) | code tests | placement built (`e4064c9a5`, `290caaae9`); charging and microVM remote commands owed (steps 16, 17) |
| 33 | Credentials in Veil's shape | none committed | none | building (ROOT's subagent `01a1d1f9f5147608a763f9113e859a7a`); its D-entry owed |
| 34 | Real Codex and Claude Code driven by kiln over hyper-quic on three host OSes | — | — | owed (ROOT) |
| 35 | Every harness through kiln's UI and API | — | — | owed (ROOT) |
| 36 | Hundreds of harness microVMs across the fleet, with chaos | kiln `bench/fleet/README.md` (its drivers, what it measures, every number's derivation) | in record: its light tests' records (`bench/fleet/records/2026-11-12-adacachy-light-{1,2}`) | the harness and its direct driver on kiln main (`b82b865aa`, `c28878d05`, `5e9254453`, `43abe0dfe`, via `e86ec22c6`); its `hex` and `kiln` drivers owed with steps 12 to 15, 17 and 32 (its "Owed"); the runs at density, on every host, with chaos, and their numbers owed (ROOT) |
| 37 | This inventory and the records of section 4 | this file | — | ROOT's final pass after step 34 |
| 38 | One designated workload's nested runtime grant, checked at build and at run | arch D234; `nesting-grant-decision-v1.md` | linked: s38-nested-grants | decided and built; GPUs by D234's `gpus=` (ROOT, 2026-11-12; `57b5080a3`), not yet sent by any run; its real-VM test (`tests/nesting.rs`) red on CI's KVM x86_64 for two defects of the test's own (its images' commands not root, its shards-api case's program unnamed), fixed at `231bbdf6d` (D234 at `6a721f4ab`, via `38a99c2d4`), its green owed; real-VM E2E, mutations, arm64 filter tests, 7-target lint and build-ab owed (ROOT) |
| 39 | Hex's node under shards-init; the hex-guest PID 1 retired after conformance | Hex `shards-image-class.md` §7 | — | open: needs steps 11 and 38; Hex ROOT's, with ROOT measuring start latency and memory |

## 4. What this step wrote

From existing evidence only; no number is new.

- Shards `docs/design/architecture.md`:
  - **D238**, agent control over hyper-transport: what the plan decided (transport, suite,
    lanes, the VM's own key), where its TLS ends (open, ROOT's), the endpoint (building), and
    control that crosses no TLS (open, Ada's and ROOT's).
  - **D239**, AES-256-GCM alone: step 3's landing, its tests and mutants, what it leaves
    unnarrowed and why, and what is owed. D180's and D60's paragraphs point at it.
  - **D240**, a member's stdio, terminal and environment: steps 5 and 7 as built, step 6 as
    building, what is owed.
  - Room-bounded pins: D230 links its implementer's record; D188 says that ring pins come out
    of the room, and that an orchestrated run's room is owed on shards-api's wire.
  - Records step 1 and 2 left without a pointer: D12's credit and window (vsock-credit-v1,
    vsock-window-v1, whose record named that D-entry as owed), D16's exec admission
    (exec-admission), D37's records written whole (request-sites-v1), D43's guest time
    (guest-cpu-time-v1).
  - Two stale statements corrected: D180's step 5 section and D231 said a member's stdin was
    refused on the orchestrator's path, which `e7825cb32` built (`ImageGuest::take_input`,
    `tests/api_image.rs`).
- Hex `docs/design/README.md`: the index's rows for step 4 (`hex-quic.md`) and step 39, and
  what changed on Shards' side since its rows' revisions.
- kiln `docs/agents.md`: isolated agents in the reader's voice (Agentfiles, credentials, what an
  isolated agent can't do).

Second pass (goal 67's plan `01a1d2bbb8f27423863fed0f0b9712a0`, step 1), this file alone, from
what landed between the first pass and the "Read at" revisions:
- step 11's slice 1 (rows P5, A2, 11), and two rows for what D238 now records step 11 decided:
  the microVM's key pinned, where the plan left it open (11a), and the wire as QUIC streams under
  D180's driver, where the plan named hyper-transport (11b), each for ROOT to review;
- step 8's part 2 (row 8), PM M538 beside the signal link (row 6), step 20's finish and step 36's
  harness on kiln main (rows 19-28 and 36), and ROOT's step 10 runs not yet committed (row 10);
- the audit fixes of init, server, host and tests: their records name no D-entry, so
  architecture.md gains no pointer to them (row 1);
- then, as each landed: step 11's slice 2 (`60c5dfbc6`; rows P5, A2, 11); step 10's decision as
  ROOT wrote it into D238 and PM M540 (`1b6e1203d`, via `1caf0e3fa`; rows P4, P5, A2, A3, 10,
  11, 11a), and the question it opened of a moved microVM's key (row 11c), which ROOT decided
  and recorded in D238 (`8710cb12a`, `d5bc8a3d0`, via `a22d060b5`); Hex steps 4 and 12 as they
  landed on Hex main (`325b01cf`; rows P1, P2, A7, 2, 4, 12, 15) and Hex's index brought past
  them (Hex `cf2d6c32`); step 6's slice 1 (`de4897668`; row 6, and D240's pointer to it); step
  38's real-VM test's red and fix (`38a99c2d4`; row 38); cold migration's slice 1 (`4a79f7f93`;
  section 5).

Still unrecorded, each with its owner: step 33's D-entry (step 33); step 13's kiln side
(kiln's Load Balancing agent); step 16's kiln record (`p/s16-vendor-v1`); step 10's runs that
may reopen it (ROOT: WHP, KVM's memory at more rounds, a loaded run); step 6's later slices (D240
holds slice 1); step 11's later slices (D238 holds slices 1 and 2); what kiln's other uses of a
moved microVM's key do (row 11c, step 18's); cold migration's later slices (D176 part 6, section 5).

## 5. Beside the plan

Goal 67's own list names one goal that is no step of the plan's, whose record row 11c relies on.

| Goal | Decision | Recorded | Evidence | Status |
|---|---|---|---|---|
| Roadmap 7a, cold migration (ROOT's goal `01a1664e`) | A running microVM saved and restored on another machine, with its CPU baseline, its files, its image by digest, its shared volumes and its connections redialed; a move refused, typed, before anything moves | arch D176 "Moved to another machine" (part 6), "Refused before anything moves"; `docs/research/measurements/migrate/README.md` | linked: migrate (slice 1's light gate on Linux x86_64: its unit tests, and mutants M1 to M3 each failing its test) | slice 1 on main (`a9cf3b94c`, `6fc973d13`, via `4a79f7f93`): a move's refusals typed, a microVM of another hypervisor refused before anything moves; its later slices building in its own plan, its two-host E2E ROOT's to run; ROOT's decision on a moved microVM's key is in D238 (row 11c) |
