# Every decision of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, and where it is recorded

Plan step 37 ("Docs and decision records"): every decision the plan's 39 steps made or need,
where it is written down, whether its evidence is linked there, and what is still open and
whose it is. ROOT's final pass after step 34 checks this table again.

- **Read at:** Shards main `e7825cb32` (checked again at `dc7d36014`), kiln main `d6ffa45d8`,
  Hex main `d84730ee`; the plan at version 11 and ROOT's goal stack, 2026-11-12, about 15:45 PST. Branches not on main are named
  as such and count as nothing landed.
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
| P1 | Shards runs the harnesses: one microVM per harness under shards-init, Hex the runtime outside it ("Shards runs these. Literally why we're testing that functionality.") | arch D231; Hex `docs/design/shards-image-class.md` | linked: runtime-image-boot-decision-v1, runtime-image-boot-v1, `tests/api_image.rs` 6/6 on KVM x86_64, HVF, WHP x86_64 | decided; built (D231 on main; Hex's class built to Shards `482c5d78`); KVM arm64 and WHP arm64 runs owed (ROOT) |
| P2 | Hex owns TCP/UDP for Shards VMs: Hex decides each VM's policy, Shards' per-VM network process enforces it | Hex `network-grants.md` N3; arch D231 "Ports and grants", D236 (an orchestrator keeps or drops destinations, never adds) | linked | decided; Shards' half built (D236, `edbada623`); Hex programs what Shards `482c5d78` expresses (N4) and has not yet vendored D236 |
| P3 | Hex charges a microVM by its measured use, its envelope the ceiling ("For 14 - of course go") | Hex `learned-charge.md` "The person's choice"; arch D43 "Each vCPU's guest time" (s37), D188 | linked: kiln research `01a1cd8f5b8b77649ccb95ad26875842`, guest-cpu-time-v1 | the charge decided by the person; its rule (C1-C7) and the step 13 interface (C8) taken by ROOT 2026-11-12; built in Hex (`b19ef663`); density and fairness owed (ROOT) |
| P4 | "all such communications should be AESGCM256 bidirectional encrypted" | arch D239 (s37), D180, D60; kiln `crates/kiln/src/reach/e2e.rs` `PROVIDER` (`d4990d3b`); Hex `hex-quic.md` Q5 | linked: aes256-only-v1 | built on every Shards TLS link (step 3); real-VM runs owed (ROOT); control that crosses no TLS open (Ada and ROOT, D238) |
| P5 | Agents are controlled over networking, on hyper-quic, which Hex and Shards both offer and use | arch D238 (s37); Hex `hex-quic.md` | Shards: harness committed, no numbers; Hex: linked | Shards: where TLS ends open (step 10, ROOT), its endpoint building (step 11); Hex: designed (Q1-Q9), its conformance over QUIC is step 4's (Hex ROOT) |
| P6 | Hex and kiln's load balancer are peer to peer and coordinate the fleet | Hex `capacity-view.md` CV1-CV11; kiln `docs/lb.md` (no joint view) | Hex: linked (hex-sim's nemesis, the record sweep) | proposed 2026-11-12; Hex's cores, seam and `hex schedule serve` built; kiln's record and holder owed (step 13, kiln's Load Balancing agent) |
| P7 | No arbitrary constants: every size learned or derived, varying with fleet scale and hardware; a microVM may have more than one vCPU | Shards `docs/development-rules.md` R03 and each D-entry's literals table (D230's rings learned per stream, D191's every host CPU); Hex `learned-charge.md` "Every number", `hex-quic.md` Q8; kiln `AGENTS.md` ("Sized by measurement") | per entry | standing rule |
| P8 | Credentials in Veil's shape: placeholders in the VM, real values filled in at its egress, refresh outside, tmpfs only where nothing else works, Veil later replacing only the backend and the injector | the plan (body, step 33); kiln `docs/seams.md` "Secrets"; kiln's built-in Agentfiles (no credential in an image); kiln `docs/agents.md` (s37) | none for the injector | the shape decided (ROOT's gate `22dfc46c`, from the person's naming Veil); building (step 33, `p/s33-veil-creds`, nothing committed); its D-entry owed by step 33 |
| P9 | Users keep their own Agentfiles in kiln, with versions, managed in a UI | kiln `docs/api.md` "Agentfiles"; `docs/agents.md` "From an Agentfile, in a microVM of its own" | in record (merge commits `c09d23207`, `3a4b09005`) | built; step 30's round trip against the running server open (a deploy, the person's word: kiln `AGENTS.md`) |
| P10 | Work agents start from inside their VMs goes through kiln's load balancer | kiln `docs/runners.md` and `docs/ci.md` (`virt.microvm`), `docs/needs.md` (`microvm`); `p/s32-rest-v1` adds `docs/agents.md` and `docs/api.md` text (not on main) | code tests (`harness/lineage.rs`) | placement and isolation's inheritance built; charging and remote commands owed (steps 16, 17, 32) |

## 2. The architecture (the plan's body)

| # | Decision | Recorded | Evidence | Status |
|---|---|---|---|---|
| A1 | Control plane kiln ↔ Hex: hyper-transport on hyper-quic, AES-256-GCM only, identity from kiln's CA by CSR, Hex's calls on lanes | Hex `hex-quic.md` Q1-Q9 | linked | Hex's (step 4, Hex ROOT) |
| A2 | Data plane kiln ↔ each harness's VM: a stream per stdio, a terminal lane, a control lane, a log lane; the VM's own key replaces kiln's peer-process identity | arch D238 (s37) | the plan | decided; the endpoint building (step 11) |
| A3 | Where encryption ends is measured before it is decided | arch D238 "Open: where its TLS ends"; `docs/research/measurements/control-end` | harness committed (`d9caccfe8`..`4bf2fe812`), no numbers | open: ROOT decides (step 10) |
| A4 | Every link `TLS13_AES_256_GCM_SHA384` only, with a test asserting it; QUIC's Initial packets stay AES-128-GCM (RFC 9001 §5) | arch D239 (s37), D180 | linked | built (step 3) |
| A5 | Hex's scheduler gossips capacity over hyper-swim; kiln's balancer decides at the node that asks | Hex `scheduler-architecture.md`; kiln `docs/lb.md` | linked | each built on its own side |
| A6 | One shared capacity view per machine: each side publishes its reservations, each admission checks the combined view | Hex `capacity-view.md` | linked | as P6 |
| A7 | Hex sets budgets per fleet; D188 tells each guest its room; everything a VM holds, ring pins included, comes out of it | arch D188 "Ring pins come out of the room" (s37), D230; Hex `learned-charge.md` C7 | linked: ring-pins-v1 | pins within the room built (step 2); an orchestrated run's room owed: shards-api carries no `ROOM`, so Hex refuses it typed (`Awaits(GuestRoom)`); Hex's `Awaits(RingPinsWithinRoom)` lifts once it vendors a Shards with ring-pins-v1 (its pin, `482c5d78`, predates it) |
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
| 1 | The audit's fixes: init's io.stat lines, the in-VM server's `Connection: close`, `read metrics` waits, the tests' checks, the stored request | `docs/research/measurements/audit-fix-{init,server,host,tests,request}-v1`; the request's also arch D37 (s37) | in record | on main; arch points at the request fix alone |
| 1 | Nothing reaches main without combined validation | ROOT's goal `3268f6d8` | — | open (ROOT): the 8 CI triples' lint, the suites on KVM, HVF, WHP and nested arm64 KVM, dev CI watched |
| 2 | Hex's 31 Shards patches upstreamed (hex-upstream-v1) | arch D232, D233; hex-upstream-v1/README.md | linked | on main (`482c5d78`); Hex vendors that revision with no patch to `vendor/shards`, one held packaging patch to `vendor/shards-gpu` (Windows arm64 GPU, D232 "Held") |
| 2 | An Agentfile image booted for an orchestrator (runtime-image-boot-v1) | arch D231; runtime-image-boot-decision-v1.md | linked | on main; KVM arm64, Windows arm64 and R14 costs owed (ROOT) |
| 2 | Ring pins on demand, freed on the device's DETACH, within D188's room, learned per stream, at 1, 2 and many vCPUs (ring-pins-v1) | arch D230 (its record linked, s37), D188 (s37) | linked: ring-pins-v1 r1-r16 | on main; WHP arm64, WHP x86_64 past the daemon's Windows bound, hostile offers and measurements owed (ROOT) |
| 2 | Each vCPU's guest time and a monotone total on HVF, KVM, WHP (guest-cpu-time-v1) | arch D43 (s37) | linked | on main; real-VM test per backend owed (ROOT); shards-api's image `Stats` has no field for it (Hex awaits `GuestCpuTimePerVcpu`) |
| 2 | Every record a reader decodes written whole or absent (request-sites-v1) | arch D37 (s37) | linked | on main |
| 2 | Real Codex and Claude Code start as HARNESS members (harness-bringup-v1; signed-in turns, v2) | arch D180 (H1), D236 (H2); `docs/examples/harness-{codex,claude}`; harness-bringup-v2/README.md | linked (v1); v2 in record | on main; v2's sign-in findings are step 33's input |
| 3 | AES-256-GCM alone, both ways, on every Shards TLS link | arch D239 (s37), D180, D60 | linked: aes256-only-v1 (`e12d5b96e`, `2a328f37e`, via `04abb68d4`) | built; macOS runs, Windows and aarch64 builds, 7-target lint and real-VM E2E owed (ROOT) |
| 4 | Hex's wire over hyper-transport: lanes, AES-256-GCM, CSR enrollment, learned waits | Hex `hex-quic.md` | linked | Hex's: its gate in ROOT's stack (`d8b5e77b`) waits on Hex's own goals |
| 5 | A member's stdin a stream its owner holds; stdout and stderr captured whole | arch D180 "A member's stdin" (completed, s37), D231 (corrected, s37), D240 (s37) | linked: member-stdin-v1 | built for the daemon's owner and the orchestrator (`e7825cb32`); real-VM E2E on KVM x86_64, KVM arm64, HVF, WHP and R14 costs owed (ROOT) |
| 6 | exec, attach and a resizable TTY, confined as the member is | arch D240 "Building" (s37) | — | building (`p/s6-exec-tty-v1`, nothing committed); how is step 6's implementer's, ROOT reviews |
| 6 | Exec connections admitted under contention and said when lost (ROOT's exec-hang goal, found by step 2) | arch D16 "An exec's connection" (s37) | linked: exec-admission | on main (`0d4f20288`, `2944d9ab5`); its WHP result, Linux arm64 KVM and WHP arm64 owed |
| 6 | The signal link's explicit states | arch D16 "Signals" | linked: signal-link | on main (`3607aad11`), its sends made off the link's lock by a sender thread of their own (`782275c26`) |
| 7 | A member's environment: init's, the image's `Env`, its `run.env`, its run's | arch D59 "A member's environment", D240 (s37) | linked: s7-home-workdir | on main (`239749c60`); real-VM runs owed (ROOT) |
| 7 | A writable, private HOME: its scratch or a volume; staged files under it; no volume at or under `/tmp` | arch D231 "Staging", D109 "A domain's `/tmp` is its own", D240 (s37) | linked: s7-home-workdir, s7-scratch-volume | on main (`ac9bb8589`, `81d2a6a2a`); real-VM runs owed |
| 7 | The working directory the run names (the project path) | arch D231, D240 (s37) | linked | on main (`d6ac7e48f`); a boot-time project path owed (D235's and the orchestrator's to choose) |
| 7 | A member's root keeps the image's directories' owners and modes | arch D59 | linked | on main (`cfcb22755`); real-VM runs owed |
| 7 | `run.env` sets HOME and PATH, the last given winning | arch D59 (`997f4e47e`) | linked | on main |
| 8 | A project plugged into a running member at its host path | arch D235 | linked: s8-project-paths | plugs built (`50ed65bbb`, `2955d2917`); case handling and host edits' notices on `p/s8-part2-v1` (WIP, not on main); real-VM runs owed |
| 9 | Egress destinations by name; kiln's MCP reached only by an explicit grant; build and run checks alike | arch D236 | linked: s9-grants-v1 | on main (`edbada623`); real-VM runs owed (ROOT) |
| 9 | A published port names the member it reaches (`Bound::to`, Hex step 15's finding) | arch D231 "Ports and grants" | linked | on main (channel version 3) |
| 10 | Where agent control terminates | arch D238 (s37) | harness committed, no numbers | open: ROOT decides |
| 11 | The agent-control endpoint: control, terminal, stdio and log lanes; the VM's key enrolled or pinned | arch D238 "Building" (s37) | — | building (`p/s11-control-v1`, nothing committed) |
| 12 | Hex's Shards class on Agentfile images under shards-init | Hex `shards-image-class.md` | linked | built to Shards `482c5d78`; its waits' records, real-VM runs and macOS and Windows hosts owed (§8 there) |
| 13 | One capacity view per machine | Hex `capacity-view.md` | linked | as P6 |
| 14 | Learned charge, a contention rule, budgets per fleet | Hex `learned-charge.md`; arch D43, D188 (s37) | linked | as P3; an orchestrated run's room owed on shards-api's wire (A7) |
| 15 | Network grants for Hex's microVMs | Hex `network-grants.md` | linked | decided 2026-11-12; programming waits on step 12's class; it reads "until Shards' step 9", which has landed (D236): Hex's to update as it vendors |
| 16 | kiln vendors Hex and Shards at one Shards revision; a carried `shards` starts its helpers beside it, signed as kiln signs its own | arch D237; kiln `docs/shards.md` and `docs/vendoring.md` on `p/s16-vendor-v1` (not on kiln main) | in record (kiln's branch) | Shards' half built (`d42407036`), real-VM runs owed; kiln's record, build time and size owed with its landing |
| 17 | Harnesses launched through Hex, their Duplex the member's stdio over hyper-transport | kiln: nothing on main (`p/s17-launch-seam-v1` begun; T4's `docs/isolation.md` on adacachy's `cp/t4-isolation`) | — | building; until then a machine's kiln refuses an isolated agent (`harness/host.rs`) |
| 18 | What kiln assumes of the host holds in the VM, or is refused with why; auth and reach failures surfaced as they happen (bring-up H6) | kiln `docs/agents.md` (s37: what holds today); harness-bringup-v1 H6 | in record | building (`p/s18-host-in-vm-v1` begun) |
| 19-28 | Ten built-in Agentfiles, each with the publisher's official Linux build pinned by URL and published checksum for amd64 and arm64 | kiln `.kiln/images/harness-*/` (`pin.json`, `pin.sh`), `docs/research/s19-*`..`s28-*`, `docs/agents.md` notes (agy, Gemini) | linked per step | on kiln main; image builds and `--version` smokes per arch, and Windows clippy of later revisions, owed (ROOT); step 20's goal still open |
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
| 36 | Hundreds of harness microVMs across the fleet, with chaos | — | — | owed (ROOT) |
| 37 | This inventory and the records of section 4 | this file | — | ROOT's final pass after step 34 |
| 38 | One designated workload's nested runtime grant, checked at build and at run | arch D234; `nesting-grant-decision-v1.md` | linked: s38-nested-grants | decided and built; GPUs by D234's `gpus=` (ROOT, 2026-11-12; `57b5080a3`), not yet sent by any run; real-VM E2E, mutations, arm64 filter tests, 7-target lint and build-ab owed (ROOT) |
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

Still unrecorded, each with its owner: step 33's D-entry (step 33); step 13's kiln side
(kiln's Load Balancing agent); step 16's kiln record (`p/s16-vendor-v1`); steps 10, 11 and 6's
outcomes (D238 and D240 wait for them); architecture.md pointers for the audit fixes of
`audit-fix-{init,server,host,tests}-v1`, each recorded with its evidence in its own directory.
