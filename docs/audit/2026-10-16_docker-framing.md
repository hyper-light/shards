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
- A Windows client polled its console's size every 250 ms (D180 part 2b, context.rs
  `steer_console`), docker/cli's interval, never measured; a console reports its own
  resizes as input events (WINDOW_BUFFER_SIZE_EVENT), which need no polling. Now its stdin
  is read as the console's records (cli/terminal.rs `ConsoleInput`, ReadConsoleInputW):
  keys as the VT input mode gives them, ^Z among them, and each resize as it comes; the
  250 ms look stays only where a resize leaves no record (a window within a larger
  buffer) or stdin is not the console. Held on a pseudo console its test resizes
  (ResizePseudoConsole), on CI's Windows runner. Fixed.
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
  name, where a step is a microVM's. Kept, decided: it is what a step's commands see, and a
  build's steps see what BuildKit's see so that a Dockerfile builds the same image; no
  layer keeps it (a step's /etc/hostname is a mount), and nothing names a microVM by it.
- An anonymous volume is labelled `com.docker.volume.anonymous` (volumes.rs) and nothing of
  shards' own. Kept, decided: it is how the Engine API's clients tell an anonymous volume
  (a volume prune with no `all` filter takes those alone, API 1.42 and on), and shards'
  own prune reads it the same way; a label of shards' would say the same twice.
- The Dockerfile checks send users to docs.docker.com (agentfile/src/lint.rs), for checks an
  Agentfile's build runs too. Kept, decided: the checks are BuildKit's, held to it by the
  oracle, and those pages are the checks' own documentation; shards has none of its own
  to send users to.
- Of an index with no manifest for this host's CPU, a pull or run naming no platform took
  containerd's order (D179): on arm64, 32-bit Arm before amd64, an order for CPUs that run
  32-bit Arm themselves, and an index of amd64's alone refused. Measured (PM M250): a
  microVM's runs both emulated, at much the same cost, so what a run can use orders them:
  the other 64-bit CPU's first, as a 32-bit process addresses 4 GiB at most, then 32-bit
  Arm's; and an index of another CPU's alone runs emulated, as its lone manifest did.
  Fixed. Its warning was moby's matcher copied (`OnlyPlatformWithFallback`), which counts
  32-bit Arm as an arm64 host's own: those runs, emulated here, went unwarned. Now a run
  that names no platform is warned of exactly when it runs emulated. Fixed.
- With every pool of dockerd's in use (a VPN that routes every private range), runs on the
  default network were refused, as dockerd makes no bridge (D31). Now a /24 of
  benchmarking's or shared address space the host does not use. Fixed.
- An Agentfile's `NETWORK` given an IPv6 subnet, range or gateway without `--ipv6` had it
  dropped without a word, as dockerd drops a network's IPv6 configs; the Agentfile is
  shards' own language, which no script of Docker's speaks. Now refused, at build and at
  run (D99, D109). Fixed.
- Defaults taken from Docker, not derived from what a microVM's host measures (the
  defaults rule: from measured costs and the machine's resources, tests pinning the
  basis). Open, each a measurement first:
  - (Not one: the design said layers download 3 at a time, dockerd's default; the
    fetcher has been measured and adaptive since PM M114, four connections doubling to
    sixteen while they gain, large layers split into ranges. The design is corrected.)
  - (Decided by the user, 2026-10-18, and built, D186: the build cache's collection keeps
    dockerd's 10% reserve, 80% most and 20% free as its defaults, each the user's to set
    with `shards configure disk`; each now exactly that share of the disk, where dockerd's
    arithmetic, which shards had taken, kept 9.3%, 74.5% and 18.6%.)
  - A terminal run's `TERM=xterm`, dockerd's value: open. The client's own terminal type,
    where the image's terminfo has it, would serve its programs better (colours, keys);
    xterm where it has not.
  - A download that stops without progress fails after three (containerd's
    `httpReadSeeker`'s count): open, to derive from what a stalling server costs.
  - Splunk's batch of 1,000 events (moby's default): open, to derive from what a batch costs
    the daemon and the collector.
  - A log's non-blocking queue of 1,000,000 bytes where `max-buffer-size` gives none (moby's
    ring logger's): open, to derive from what a run's output costs the host to hold.
  - A long line cut into parts of 16 KiB for the drivers that send a log away (moby's copier's
    size): open. It is what receivers that join Docker's parts expect, so it stays until a
    receiver-led size is measured.
  - A registry's request tried 5 times, 50 ms apart, and a 500, 502, 503 or 504 tried again
    only once unless it changes (containerd's `maxAttempts` and pause, crates/registry
    `ATTEMPTS`, `PAUSE`): open. Docker Hub's token server answered 502 twice within the
    pause and a CI build failed (job 117733752147, 2026-10-18). To derive from how long a
    registry's 5xx last, measured.
  - A builder's ceiling of half the host's memory (Docker Desktop's VM's share, which
    `builder_memory_mib` took): open. Its memory is plugged as its build needs it (D133),
    so the ceiling can come from what the host can give as the build asks, measured.
  - A restart's wait (100 ms doubling to a minute, from scratch after a run of 10 s), dockerd's
    restart manager's figures: open, to derive from what a microVM's restart costs, so that
    a crash loop's restarts take a bounded share of a core.
  - A daemon's shutdown ends a VM whose command outlives its stop timeout by 5 s (dockerd's
    `ShutdownTimeout` grace): open, to derive from how long a guest takes to say a killed
    command's end, measured.
  - The build cache's other rules are still dockerd's, open: the temporary share of the
    reserve (e·π·φ %, 512 MB at least) that what is easiest to make again may take, the
    ages at which records go (two days for those, 60 for any), and 2 GB reserved where the
    disk cannot be read. Each to derive from what remaking a record of each kind costs a
    microVM build, measured.
  - (Decided by the user, 2026-10-18, and built, D186: a log keeps at most 1 GiB of its
    run's newest output by default, where it kept Docker's `local` driver's 5 segments of
    20 MiB, and the user sets it with `shards configure disk --log-history`.)
  - (Fixed: a fresh volume's top where its image has nothing at the mount point was
    root's, 0755, as Docker's local driver leaves one, which shut a run of any other user
    out of the volume it was given. Now the run's user's (D38), measured first: of the
    official images that run as other than root, one met it, percona's declared
    `/var/log/mysql`, and none other changes (PM M249).)
  - Found in the code's constants (2026-10-18): what a microVM needs, then the figure of
    Docker's machinery a constant took for it, none a contract its clients read. Open, each
    a measurement or a design first.
    - A microVM's /dev/shm is its own kernel's tmpfs, bounded by the microVM's memory alone,
      and what agents run share memory through it: a browser's renderers, a data loader's
      workers (Playwright's docs and PyTorch's README tell containers' users to raise it).
      The microVM's holds half its memory, as Linux sizes a tmpfs (tmpfs.rst); each run's
      own was 64 MiB, dockerd's `DefaultShmSize`. Done: every /dev/shm of a microVM, each
      run's and each build step's, is half the memory it may have (D156).
    - A command's terminal was told TERM=xterm, dockerd's, 8 colours, whatever its client's
      terminal was. Done: its client's type where its root describes it, else
      xterm-256color where the client's terminal speaks it, else xterm (D16, PM M263).
    - What happened to a home's microVMs, for `events --since`: the last 256 events,
      dockerd's `eventsLimit`, gone with the daemon. Done: a journal in the home, its
      history a log's, `configure disk --events-history` (D187).
    - A microVM's image pushed: 5 blobs at once, dockerd's `max-concurrent-uploads`, where a
      pull's fetcher measures and adapts (PM M114). To adapt as the fetcher does, measured.
    - A microVM whose command outlives its SIGKILL: its VM ended 10 s later, and a signal
      that could not be sent waited out 2 s, moby's kill.go and stop.go. To derive from how
      long a guest takes to say a killed command's end, with the shutdown's 5 s above.
    - A microVM paused: its cgroup waited for 10 s to say it is frozen, runc's `waitFrozen`.
      To derive from how long a freeze takes a busy microVM, measured.
    - A builder's cache mount whose records are all held: its taker looks again every
      100 ms, BuildKit's mount.go, a timer where the lock's release could wake it.
    - A builder's cache collected: at most once a minute, BuildKit's controller. To derive
      from what a collection costs, measured.
- The rest: open, D-entry by D-entry.

## Fixing order

1. The grammar and help (done, b3f4b6be); the README (done).
2. Names in code and tests (done: 6 to 0).
3. Text users see; Docker's verbs and command forms in messages (usages, the VM process's
   messages, the login hint, refusals on a terminal, seven strings in shards' own voice:
   done; each form of the grammar's help page its own, `-h` help on every form, and the
   options Docker's words misstate for a microVM (`--device` a host's, `--annotation` an
   OCI runtime's, `--isolation`, `--runtime`, `--privileged`, `--oom-score-adj`) said as
   shards does them: done).
4. Sockets: the build's agent sockets as descriptors (done).
5. Docker's behaviour and bugs: each feature reviewed, fixed and tested.
6. Comments and docs: framing and "container", file by file, keeping the evidence each
   cites, shards' own words first.

Each step is gated and committed; this file's counts are re-run as they land.

| Re-run | names | framing (unbacked) | container | grammar | strings | sockets |
|---|---:|---:|---:|---:|---:|---:|
| 2026-10-16, first | 6 | 1,939 | 1,862 | 397 | 42 | 178 |
| 2026-10-16, after steps 1 to 4 | 0 | 1,878 | 1,680 | 388 | 36 | 176 |
| 2026-10-17, help, networks, engine | 0 | 1,898 (1,119) | 1,685 | 388 | 36 | 176 |

Reviewed by hand (2026-10-17), the design's unbacked lines to line 5,800: nearly every one
says what shards matches for compatibility, its evidence a few lines off (a log driver's
wire format, a registry's answer, a tar header, an exit status), and stays. Reworded where
shards' own design was said as Docker's: a run (D27), the volumes' and shares' headings,
and the stale component box and phase 4 that called the microVM's own engine pending, the
text that misled me into telling the user shards had none.

`unbacked` (2026-10-17): framing with no evidence beside it. The rest of `framing` cites
what an input or output is held to (a version, a source file, a measurement), as the
evidence rule asks; those lines are reworded where they say shards' own behaviour is
Docker's, kept where they say what shards matches for compatibility. Of the 36 strings,
the rest name protocols (`Docker-Content-Digest`, `X-Docker-Token`,
`X-Docker-Container-Path-Stat`), Docker Hub as a registry's name, Docker's own commands'
pages (held to docker/cli), the build frontend's refusal naming the BuildKit that runs it
(its `BUILDKIT_EXPORTEDPRODUCT`), and recordings.
