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
  - (Done, the same item as the constants' `TERM=xterm` below: the client's own terminal
    type where the run's root describes it, D16, PM M263.)
  - A download that stops without progress fails after three (containerd's
    `httpReadSeeker`'s count): done (D222). A download silent past what its path's own
    silences say (RFC 6298's estimator, kept in the home by path) is cut and resumed, and
    one that brings nothing is resumed within its path's budget.
  - Splunk's batch of 1,000 events. Kept, decided (2026-11-03): it is the documented
    meaning of `SPLUNK_LOGGING_DRIVER_POST_MESSAGES_BATCH_SIZE` unset (docker/docs
    content/manuals/engine/logging/drivers/splunk.md), the knob a collector's operator
    sizes posts by, and it bounds only a backlog's posts: the shipper posts whatever it
    holds as soon as it has caught up with its log (D167), where moby's waits for its 5 s
    tick, so no event waits for a batch to fill.
  - A log's non-blocking queue of 1,000,000 bytes where `max-buffer-size` gives none. Kept,
    decided (2026-11-03): it is the input's meaning, not a figure of Docker's machinery: a
    run that asks for `mode=non-blocking` and names no buffer asks for the one the option
    is documented to mean unsaid ("The default is `1m` meaning 1 MB (1 million bytes)",
    docker/docs content/manuals/engine/logging/configure.md), and a run that wants another
    names it.
  - A long line cut into parts of 16 KiB for the drivers that send a log away (moby's copier's
    size). Done (2026-11-08, D163, PM M492, M494): each driver's parts are as long as its
    receiver takes a message (syslog's 8,192 octets, Graylog's 128 chunks and 2 MiB, Fluent
    Bit's forward buffer, the journal's LineMax, HEC's content over a batch, CloudWatch's and
    Cloud Logging's event and entry, os_log's 1,015 bytes, an 8 KB ETW session's 8,040), each
    asked to fit as written, never a character in two; receivers join them by the same
    fields.
  - A registry's request tried 5 times, 50 ms apart, and a 500, 502, 503 or 504 tried again
    only once unless it changes (containerd's `maxAttempts` and pause, crates/registry
    `ATTEMPTS`, `PAUSE`): done (D222). Docker Hub's token server answered 502 twice within
    the pause and a CI build failed (job 117733752147, 2026-10-18). A trouble is ridden out
    with no count, within twice the longest its path has passed and never less than TCP's
    R2 (RFC 1122), and a request's answer waited for as long as its path's answers say;
    each learned from the path as shards runs, kept in the home, PM M486 the method's
    check, not its number.
  - A builder's ceiling of half the host's memory (Docker Desktop's VM's share, which
    `builder_memory_mib` took). Done (D133, PM M490): from the host it runs on and its
    home's builds, never a figure of ours: what the host has available as the builder
    starts, the most a build of the home needed where more (each builder's guest says its
    build's need, recorded in the home's `builder-memory`), and all the host's memory once
    a build was ended short of it.
  - A restart's wait (100 ms doubling to a minute, from scratch after a run of 10 s), dockerd's
    restart manager's figures. Done (D219): the doubling kept, its ends learned on each host
    as it runs, from its VM processes' CPU: the first a restart's CPU at the share of a CPU
    the run's limits give it, the longest a restart's CPU at an idle microVM's rate a
    second, past which a microVM failing for good costs its host what one idling does; kept
    in the home for the next daemon, uncapped before an idle microVM is measured (PM M484).
  - A daemon's shutdown ended a VM whose command outlived its stop timeout by 5 s (dockerd's
    `ShutdownTimeout` grace). Done (D217): its teardown is followed as long as its microVM
    goes on ending it, each slice the slowest end its host has shown (the home's `ends`),
    PM M480 and M481 validating the method.
  - The build cache's other rules were still dockerd's: the temporary share of the
    reserve (e·π·φ %, 512 MB at least) that what is easiest to make again might take, the
    ages at which records went (two days for those, 60 for any), and 2 GB reserved where
    the disk cannot be read. Done (D212): what the rules protect is the disk, which the
    user's shares already bound, so no record goes for its age (a cache mount unused for
    two days went whatever room the disk had, and its next build fetched it all again);
    where the disk's size cannot be read, only sizes the user set bound the cache.
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
      pull's fetcher measures and adapts (PM M114). Done (D226, PM M488): pushes and pulls
      learn their path, kept in the home: from one over a path never seen, doubling
      while a doubling pays past the path's own noise, to its bandwidth-delay product, at
      most the cores; and a blob goes in chunks of the size that costs least a byte where
      the path's sends fail, or of what a registry that refused it whole (a 413) takes.
    - A microVM whose command outlives its SIGKILL: its VM ended 10 s later, its end given
      up 2 s after, and a signal that could not be sent waited out 2 s, moby's kill.go and
      stop.go. Done (D217): followed as the shutdown's above, its killed VM process too; a
      signal that cannot be sent kills its VM at once, nothing being able to come. A run's
      stop timeout of 10 s where it sets none stays: the input's meaning (Compose's
      `stop_grace_period`), not a wait on work.
    - A microVM paused: its cgroup waited for 10 s to say it is frozen, runc's `waitFrozen`.
      Done (D217): waited for to its end, which comes as its processes leave the kernel
      work they were in (PM M482: 0.23 s at most), its kill ending one that cannot.
    - A builder's cache mount whose records are all held: its taker looked again every
      100 ms, BuildKit's mount.go, a timer where the lock's release could wake it. Done
      (2026-11-03): a `locked` taker waits on the held record's lock, and is woken as its
      step lets go of it (crates/image store/mounts.rs `take_mount`), on every host's
      flock(2) or LockFileEx alike; no timer.
    - A builder's cache collected: at most once a minute, BuildKit's controller. Done
      (D212, PM M426): a collection reads every record, 1.3 ms for 100 and 86 ms for
      10,000 at p50 on a loaded host, so a build collects where it grew the cache, the only
      way the cache passes its bounds, one collection at a time in a home, and a build
      answered from the cache collects nothing; no timer.
    - A build's SSH agent on Windows: its pipe waited for 2 s, go-winio's default, which
      BuildKit's call leaves unset (build/ssh.rs; found 2026-11-03). Done (2026-11-08,
      D51, PM M493): OpenSSH's agent frees an instance 1.4 ms apart at the median however
      many dial, while a dial's whole wait passed 2 s at 128 clients; a dial now follows the
      agent, with no time of shards': it dials again as each instance is freed, each wait
      the agent's pipe's own, and ends as the agent serves it or goes.
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
| 2026-11-03, before step 6's sweep (de2b1cf5) | 2 | 1,807 (688) | 1,944 | 400 | 42 | 189 |
| 2026-11-03, every unbacked line resolved | 0 | 1,268 (0) | 1,876 | 400 | 42 | 189 |
| 2026-11-07, before step 6's second sweep (8501f093) | 1 | 1,287 (12) | 1,966 | 412 | 43 | 189 |
| 2026-11-08, `container` and `grammar` swept | 0 | 1,276 (0) | 700 | 130 | 42 | 189 |

Reviewed by hand (2026-10-17), the design's unbacked lines to line 5,800: nearly every one
says what shards matches for compatibility, its evidence a few lines off (a log driver's
wire format, a registry's answer, a tar header, an exit status), and stays. Reworded where
shards' own design was said as Docker's: a run (D27), the volumes' and shares' headings,
and the stale component box and phase 4 that called the microVM's own engine pending, the
text that misled me into telling the user shards had none.

Swept (2026-11-03), every unbacked line, file by file across the tree (688 at de2b1cf5,
two test names among the `names` kind's): each said shards' own behaviour as Docker's,
dockerd's, BuildKit's, runc's or containerd's, and now says what shards does and why,
microVM where it meant one; or it stated a compatibility contract (an answer shape, a
wire format, words scripts match, a Dockerfile's meaning held to BuildKit), and now cites
what it is held to: the D-entry, the pinned source and version, the oracle or recording,
the RFC or the measurement. Citations were checked against the pinned upstream sources.
The `framing` lines left each have such evidence beside them.

Swept (2026-11-07 and 08, branch p/agentfile-close, comments and docs alone, no string or
identifier but the two below): the regressions since 2026-11-03 first, a test named after
Docker's sizes (`sizes_read_in_bytes_or_binary_units` now) and 12 unbacked lines, each now
shards' reason or the source, version or D-entry it rests on; a test's message in Docker's
name (a `strings` line). Then every `container` line, file by file: where shards' own voice
said container for its microVM, its run or the run's record, it says microVM, run or
record; for a run in another's microVM, by `--network container:` or made by a microVM's
own engine, joiner (D119, D181). 1,966 lines to 700, each left one of these: Docker's,
dockerd's, runc's, containerd's or OCI's own container, said as theirs or compared with a
microVM ("as a container's page cache is its host's to drop"); a byte-exact text, an API
field, an event or object kind, an identifier of docker/cli's or moby's (`container.StopSignal`,
`DiskUsage of image, container, volume and build`); a CLI word a user types (`--network
container:NAME`, `shards container ls`) or Docker's `--volume` syntax's container path; a
file or test name (`containers.rs`); another system's own container: VFIO's IOMMU
container, macOS App Sandbox's, Windows' AppContainer and NTFS's WCI container, Azure Blob
Storage's, AWS's container credentials, CDI's, JSON's, D-Bus's and Go's; a host that is a
container itself; the conversion said as such ("each container a microVM"); and the
comments beside the copied defaults still being derived (the kill, shutdown and freeze
waits, restarts), which their own changes reword. Then every `grammar` line: a command said
in Docker's form in shards' own voice is said in shards' grammar where the grammar has a
form for it (`grammar::rewrite`): `shards ps` is `shards list vms`, `shards rmi` `shards
remove image`, `shards logs -f` `shards watch logs`, `shards diff` `shards read changes`,
`shards events` `shards watch system`, a microVM's actions with `vm` and an image's with
`image`. 412 lines to 130, each left one of these: an action the grammar has no form for
(`top` 18, `attach` 14, `checkpoint` 7, `import` 6, `update` 5, `port` 5: `read ports` and
`read processes` say other things); the command tables held to docker/cli
(cmdline/src/commands.rs) and the grammar's own account of what each form runs; tests that
pin Docker's command line itself; the README's lines on what scripts written for `docker`
type; and the pattern's own misses (`shards build-writer`, a key's label, forms split by a
line's end). The `strings` left are protocol names (`Docker-Content-Digest`,
`X-Docker-Token`, `Docker-Experimental`, `X-Docker-Container-Path-Stat`), Docker Hub as a
registry's name, docker/cli's own command texts and `--use-api-socket`'s option text (held
to it, `look::ours` rewording them on a terminal), dockerd's own refusals matched by tests,
the frontend's capability table (BuildKit's), Docker's install paths for its CLI plugins,
seccomp's oracle comparing against Docker's profile, and a context's refusal saying it
reaches no Docker daemon.

`unbacked` (2026-10-17): framing with no evidence beside it. The rest of `framing` cites
what an input or output is held to (a version, a source file, a measurement), as the
evidence rule asks; those lines are reworded where they say shards' own behaviour is
Docker's, kept where they say what shards matches for compatibility. Of the 36 strings,
the rest name protocols (`Docker-Content-Digest`, `X-Docker-Token`,
`X-Docker-Container-Path-Stat`), Docker Hub as a registry's name, Docker's own commands'
pages (held to docker/cli), the build frontend's refusal naming the BuildKit that runs it
(its `BUILDKIT_EXPORTEDPRODUCT`), and recordings.
