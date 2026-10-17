# Audit 2026-10-16: where shards speaks, names or behaves as Docker

The rule (the user, 2026-10-13, repeated six times): shards is a microVM platform. It takes
Docker's inputs, the images, Dockerfiles, Compose files, `docker` command lines and Engine
API calls people already have, and what it does with them is its own, designed for
microVMs and better than Docker's. It speaks its own grammar, `shards ACTION THING`
(crates/cmdline/src/grammar.rs), says microVM, and is filesystem based.

This audit lists every place that breaks the rule, by kind. `scripts/audit/docker-framing.py`
counts the kinds a pattern can find (`--list` prints every line); the others are listed here
by hand. Recordings (testdata/, the oracles' answers) keep the names of what they record and
are not counted.

## Found, by kind

| Kind | Count | Where most |
|---|---:|---|
| Code and test names after Docker | 6 | `checkpoints_are_a_microvms_snapshots_in_dockers_words` (tests/saved.rs), `a_console_goes_raw_as_moby_terms_does` (cli/terminal.rs), `fn dockers` (daemon/snapshots.rs), `a_pull_of_no_platform_takes_what_dockerd_takes` and `the_host_runs_what_dockerd_finds_it_runs` (image/platform.rs), `a_register_line_is_dockers_tools` (abi/emulate.rs), `docker_hubs_index_is_searched_as_dockerd_searches_it` (registry/search.rs) |
| Docker's verbs in shards' own grammar and help | 7 verbs | `top`, `stats`, `events`, `diff`, `history`, `logs`, `port` as shards actions; piped `shards --help` was docker/cli's help. Fixed in b3f4b6be |
| Commands written in Docker's form | 397 | architecture.md 94, cmdline help texts 39, daemon messages 39, AGENTFILE_ARCH.md 19, the TTY pages 11, tests 30 |
| shards' behaviour framed as theirs ("as dockerd does", "in Docker's words") | 1,939 | architecture.md 471, tests/containers.rs 112, tests/build.rs 104, build/mod.rs 80, init/run.rs 49 |
| "container" where shards means a microVM | 1,862 | architecture.md 423, daemon.rs 213, tests/containers.rs 117, daemon/commands.rs 79, containers.rs 74 |
| Text users see naming Docker in shards' own voice | 42 | registry.rs 5, help texts 4 ("Show the Docker version information", "Return low-level information on Docker objects", "Search Docker Hub for images", "Show docker disk usage") |
| The README | whole | three programs to install against the one-binary rule; Docker's command forms and words throughout; statuses stale. Rewritten |
| Sockets where shards is filesystem based | see below | |
| Docker's behaviour and bugs copied | see below | |

## Sockets shards makes as its own interfaces

| Socket | What it is for | Where it should go |
|---|---|---|
| `daemon.sock` in the home | the CLI asking its daemon | to decide with the user: whether a request becomes a file the daemon watches in the home |
| `engine.sock` in the home (D170) | tools that speak the Engine API, which only dial a socket (`DOCKER_HOST=unix://`) | the one place a socket is the input itself; kept, a file of the user's alone |
| `e/<id>.sock` per context (D180 part 4) | the same tools, relayed to another machine | as `engine.sock` |
| `a/<id>.sock` per build's SSH agent (D180 part 3h) | a build step's agent connections on the machine | a descriptor the build asks on for each connection, a pair each (build::ssh::asked_of), no path at all. Fixed |
| a Docker socket inside a workload (proposed for `--use-api-socket`, not built) | none: the user's answer is the microVM's own engine | not built |

## Docker's behaviour and bugs copied

Reviewed feature by feature, each D-entry's design against what it took from Docker:

- `--cidfile` (D180 part 3j, and the local run): checked the file was not there, then
  created it, as docker/cli's newCIDFile does; two runs started together could both pass
  the check. Now made in one step that fails where one is there, no link followed. Fixed.
- `--cidfile`'s refusal says "container ID file found, make sure the other container isn't
  running": Docker's words for a microVM's ID, and on a terminal too. Now a terminal shows
  shards' words in a panel; piped, a `docker` script's. Fixed.
- A Windows client polls its console's size every 250 ms (D180 part 2b), docker/cli's
  interval, never measured; a console reports its own resizes as input events
  (WINDOW_BUFFER_SIZE_EVENT), which need no polling.
- The default network said it has a bridge named `docker0`
  (`com.docker.network.bridge.name`, daemon/networks.rs), which shards has none of, and a
  network's bridge name was checked against Linux's 15-byte interface names and against
  other networks' as dockerd checks a bridge's: shards makes no Linux bridge. Fixed.
- The default network reports Docker's default bridge's options, read against what shards
  does: `enable_ip_masquerade` true holds (a run's flows leave from the network process's
  host sockets, under the host's address; D31), and so does `host_binding_ipv4` 0.0.0.0
  (a published port with no address is bound at every address, daemon/publish.rs `bind`).
  `enable_icc` true does not: every microVM on the default network has the same address,
  the subnet's second (net/src/bridge.rs), so that one template serves them all, and none
  reaches another. Nor does MTU 1500: a guest's interface takes its device's, 65520
  (init/src/net.rs, `MTU`).
- A user network's options were checked (two of them) and then ignored: none of
  `enable_icc`, `host_binding_ipv4` or `driver.mtu` reached what its microVMs do
  (daemon/publish.rs bound a port with no address at every address whatever its
  network's `host_binding_ipv4`). Two of them ask for less reach: a network made with
  `enable_icc=false` so that its members do not reach each other, or with
  `host_binding_ipv4=127.0.0.1` so that its published ports stay on the host, got
  neither, and was not told. Fixed (D46): both honoured; an MTU kept as given and said to
  leave members at their devices' (their flows leave through the host's sockets); a
  routed network's ports refused; every option read as dockerd reads it; the default
  network's options true.
- Every build step's hostname is `buildkitsandbox` (abi/src/build.rs), BuildKit's sandbox's
  name, where a step is a microVM's.
- An anonymous volume is labelled `com.docker.volume.anonymous` (volumes.rs) and nothing of
  shards' own.
- The Dockerfile checks send users to docs.docker.com (agentfile/src/lint.rs), for checks an
  Agentfile's build runs too.
- Of an index with no manifest for this host's CPU, a pull or run naming no platform takes
  containerd's order (D179): on arm64, 32-bit Arm before amd64, an order for CPUs that run
  32-bit Arm themselves; a microVM's runs both emulated, and which runs better is not
  measured.
- The rest: open, D-entry by D-entry.

## Fixing order

1. The grammar and help (done, b3f4b6be); the README (done).
2. Names in code and tests (done: 6 to 0).
3. Text users see; Docker's verbs and command forms in messages (usages, the VM process's
   messages, the login hint, refusals on a terminal, seven strings in shards' own voice:
   done; per-command help pages, still docker/cli's prose, next).
4. Sockets: the build's agent sockets as descriptors (done).
5. Docker's behaviour and bugs: each feature reviewed, fixed and tested.
6. Comments and docs: framing and "container", file by file, keeping the evidence each
   cites, shards' own words first.

Each step is gated and committed; this file's counts are re-run as they land.

| Re-run | names | framing | container | grammar | strings | sockets |
|---|---:|---:|---:|---:|---:|---:|
| 2026-10-16, first | 6 | 1,939 | 1,862 | 397 | 42 | 178 |
| 2026-10-16, after steps 1 to 4 | 0 | 1,878 | 1,680 | 388 | 36 | 176 |
