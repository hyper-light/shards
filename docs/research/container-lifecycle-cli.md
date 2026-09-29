# Container lifecycle in the Docker CLI: IDs, names, `run -d`, `ps`, `stop`, `kill`, `rm`, `wait`, `logs`

Research note, 2026-09-29. Evidence only; nothing here is a final design decision.

It informs the lifecycle milestone built on D26's per-user daemon, where each "container" is a microVM:

- `shards run -d`, `--name` and `--rm`;
- `shards ps`, `stop`, `kill`, `rm`, `wait` and `logs`.

The goal is parity wherever a user or a script can see a difference: stdout, stderr and the exit status.

Pinned sources (citation paths are relative to each repository; abbreviations as listed):

| Tag | Source |
|---|---|
| `cli` | docker/cli v29.8.1, `4a63305d7433` (as in container-engine-internals.md). `C/` = `cli/command/container/`, `F/` = `cli/command/formatter/`, `D/` = `docs/reference/commandline/`, the source of the CLI reference on docs.docker.com |
| `moby` | moby/moby docker-v29.8.1, `464cd50c3d9e`. `dc/` = `daemon/container/`, `routes.go` = `daemon/server/router/container/container_routes.go` |
| `ctrd` | containerd v2.4.1, `f2551031d727` (one file) |
| `go` | Go 1.26.1 standard library, the local toolchain (moby's `go.mod` asks for 1.26.3) |
| `rust` | Rust 1.94.1 `rust-src` (as in warm-pool-daemon.md) |

v29.8.1 is the newest v29 tag of both repositories on 2026-09-29. Two pairs of copies are identical, checked with `diff`:

- moby's `client/` package and the CLI's vendored copy (non-test files);
- go-units v0.5.0's `duration.go` in both vendor trees.

So `client/…` and go-units citations hold for both binaries.

Markers:

- **(derived)**: our arithmetic on cited numbers.
- **(inference)**: reasoning from code or docs, not stated there.
- **(executed)**: printed by a throwaway Go test that ran the pinned CLI formatter and go-units on this host (go1.26.1, darwin/arm64). The test is not committed yet (E2).
- **UNVERIFIED**: no acceptable source found.

## 1. Scope

- **Q1.** Container IDs: generation, short IDs, and how an argument resolves to a container.
- **Q2.** Generated names, `--name` validation and conflicts.
- **Q3.** `docker run -d` and how it combines with `--rm`, `-i`, `-t` and `-a`.
- **Q4.** `docker ps`: columns, formatting, padding, flags, order, `--format`.
- **Q5–Q9.** `stop`, `kill`, `rm`, `wait` and `logs`: defaults, output, errors, exit codes.
- **Q10.** What a container leaves behind, its states, and what a daemon restart does to it.
- **Q11.** Error text and the CLI's exit status.

container-engine-internals.md §2.3 covers the engine side of stop, kill and restart policies, and warm-pool-daemon.md §2.7 covers foreground `docker run`. This note does not repeat them.

## 2. Findings

### 2.1 Q1: container IDs, and how an argument finds its container

**Generation**

- **32 random bytes from `crypto/rand`, hex-encoded: 64 lowercase characters** [moby: daemon/internal/stringid/stringid.go:36-46].
- **Redrawn while the first 12 characters are all digits**, "as it's used as default hostname for containers" [ibid.:48-59, 62-70]. That rejects (5/8)^12 ≈ 0.36% of draws (derived).
- One ID per container, drawn before its name is reserved [moby: daemon/names.go:43-61].
- The API schema says "a 128-bit (64-character) hexadecimal string (32 bytes)", with pattern `^[0-9a-fA-F]{64}$` [moby: api/swagger.yaml:5741-5749]. 32 bytes is 256 bits (derived), so "128-bit" is wrong.
- The creation time is `time.Now().UTC()` at create [moby: daemon/container.go:137]. The list API reports it in whole seconds [moby: dc/view.go:314].

**Short IDs**

- `TruncateID` drops any `algo:` prefix and keeps the first 12 characters. A shorter string is returned whole [moby: daemon/internal/stringid/stringid.go:10-34].
- The client has an identical copy [cli: vendor/github.com/moby/moby/client/pkg/stringid/stringid.go:13-37].

**The ID inside the container**

- The default hostname is `id[:12]`, except with host networking [moby: daemon/container.go:123-133].
- It reaches the guest three ways:
  - the OCI spec's hostname [moby: daemon/oci_linux.go:751];
  - a `hostname` file holding the name plus `\n`, bind-mounted at `/etc/hostname` [moby: dc/container_unix.go:47-55, 82-99];
  - `HOSTNAME=`, second in the environment after `PATH`, plus `TERM=xterm` with `-t`. The image's and the user's variables override them [moby: dc/container.go:818-831].

**Resolution**

The CLI passes the argument to the daemon unchanged; the client only trims whitespace [moby: client/utils.go:27-34]. The daemon's `GetContainer` tries, in order [moby: daemon/container.go:30-69]:

1. an exact full ID;
2. an exact name, with or without the leading `/` [ibid.:159-177];
3. a unique ID prefix of any length, from memdb's prefix index [moby: dc/view.go:102-131].

- So a name wins over an ID prefix, and a full ID over a name (inference from the order). Matching compares bytes, so it is case-sensitive (inference).
- **Ambiguous prefix:** `multiple IDs found with provided prefix: <arg>`, InvalidParameter, HTTP 400 [moby: dc/view.go:120-122; daemon/server/httpstatus/status.go:27-31].
- **No match:** `No such container: <arg>`, NotFound, HTTP 404 [moby: dc/view.go:130].
- **Empty argument:** the client refuses it before sending: `invalid container name or ID: value is empty` [moby: client/utils.go:19-34].
- **On the wire:**
  - The daemon sends errors as JSON `{"message": …}` [moby: daemon/server/server.go:94-108].
  - The client treats 2xx and 3xx as success, and prefixes `Error response from daemon: ` to anything ≥ 400 [moby: client/request.go:221-227, 270-306].
  - The CLI prints a returned error plus `\n` on stderr and exits 1, unless the error carries a status code [cli: cmd/docker/docker.go:43-55, 97-120].

### 2.2 Q2: names

**Generated names**

- **New in v29.8: generation sits behind an experimental extension point**, `org.mobyproject.extension.containernamegenerator.v0` [moby: extpoints/containernamegenerator/v0/containernamegenerator.go:3-5, 38-39].
  - The only built-in provider is the "legacy" generator [moby: daemon/extensions_default.go:21-26; internal/namesgenerator/legacy/names-generator.go:25-43].
  - Out-of-process extensions are discovered in extension directories [moby: daemon/extensions.go:20-33, 52-60]. If one fails, the built-in's name is used and the failure logged [moby: extpoints/internal/namesgenerator/namesgenerator.go:13-39; daemon/names.go:119-130].
  - An in-tree generator that derives names from the image (`<image>-<first 4 + 2·retry ID characters>`) is used only by an integration-test fixture [moby: internal/namesgenerator/image/names-generator.go:1-6, 27-43; integration/extension/testdata/namesgenerator/cmd/namesgenerator/main.go:1-16].
- **The legacy algorithm** [moby: internal/namesgenerator/legacy/names-generator.go:879-892]:
  - `left[rand.IntN(len(left))] + "_" + right[rand.IntN(len(right))]`;
  - `boring_wozniak` is redrawn: "Steve Wozniak is not boring" [ibid.:884-886];
  - on a retry (> 0), one random decimal digit is appended: `eager_hopper7`.
- **Lists:** 108 adjectives and 236 surnames, so 25,488 pairs and 25,487 names (derived, counting [ibid.:53-162, 166-876]). The package is "frozen": "no new additions will be accepted" [ibid.:3].
- **Randomness:** `math/rand/v2`'s top-level functions, fed by the Go runtime's generator [go: src/math/rand/v2/rand.go:254-266]. The package's outputs "might be easily predictable regardless of how it's seeded" [go: ibid.:15-17]. Names are not secrets.
- **Collisions** [moby: daemon/names.go:132-162]:
  - up to 6 draws (retry 0 to 5), each reserved atomically in memdb's unique name index [moby: dc/view.go:176-193];
  - after 6 conflicts, the name is `/` + the 12-character short ID;
  - with n containers, a first draw collides with probability ≤ n/25,487: 3.9% at 1,000 (derived).
- A generated name must be 2–63 ASCII letters, digits, `-` or `_`, starting and ending alphanumeric. Each provider call has 5 s [moby: extpoints/internal/namesgenerator/namesgenerator.go:62-95].

**`--name`**

- One leading `/` is stripped, then the name must match `^[a-zA-Z0-9][a-zA-Z0-9_.-]+$` [moby: daemon/names.go:63-66; daemon/names/names.go:5-9]. The API documents the same pattern [moby: api/swagger.yaml:8422-8428].
  - So a name needs **at least two characters**, and has no maximum length (derived).
- **Invalid:** `Invalid container name (<name as given>), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed`, HTTP 400 [moby: daemon/names.go:64-66].
- **Taken:** `Conflict. The container name "/<name>" is already in use by container "<64-hex ID>". You have to remove (or rename) that container to be able to reuse that name.`, HTTP 409 [moby: daemon/names.go:67-79; daemon/errors.go:57-66].
  - The quoted name carries the leading `/`, because the slash is added before the reservation (inference from [moby: daemon/names.go:67-69]).
- Names are stored and returned with the leading `/` "for historic reasons" [moby: api/swagger.yaml:5750-5761].
- A name stays reserved until the container is removed [moby: dc/view.go:159-174; daemon/delete.go:170-181].
- Under `docker run`, the conflict prints as `docker: Error response from daemon: Conflict. …`, a blank line, then `Run 'docker run --help' for more information`, and exits 125 (§2.3).

### 2.3 Q3: `docker run -d`

**Output**

- The flag is `-d, --detach`, "Run container in background and print container ID" [cli: C/run.go:60].
- After create, a goroutine prints the full 64-character ID plus `\n` on stdout while the CLI calls start. After a successful start, the CLI waits for that print and returns 0 [cli: C/run.go:173-181, 205-227].
- Everything else goes to stderr, so stdout holds only the ID:
  - `Unable to find image '<name:tag>' locally` and the pull progress [cli: C/create.go:135-162, 337-347];
  - `WARNING: <daemon warning>` lines [cli: C/create.go:374-376].
- On a start failure the ID has usually been printed already, and the error follows on stderr. Nothing orders the print before the start error, so this is a race (inference; E5).

**Flag combinations**

| Flags | Behavior | Source |
|---|---|---|
| `-d` | No TTY check; attach flags cleared, `StdinOnce` false | [cli: C/run.go:128-141] |
| `-d --rm` | Allowed. The daemon removes the container when it exits | [moby: daemon/monitor.go:134-140, 340-356; cli: D/container_run.md:817-823] |
| `-d -i` | `OpenStdin` set but stdin not attached, so a later `attach` can use it | [cli: C/opts.go:359-361, 654-658; C/run.go:137-140] (inference) |
| `-d -t` | TTY allocated; no raw mode and no resize, which need an attached client | [cli: C/run.go:222-233] (inference) |
| `-d -a …` | `conflicting options: cannot specify both --attach and --detach`, a plain error, so exit 1 | [cli: C/run.go:132-135; cmd/docker/docker.go:97-120] |
| `--rm --restart …` | `docker: conflicting options: cannot specify both --restart and --rm` + help hint, exit 125 | [cli: C/opts.go:720-722; C/run.go:109-116] |

**Failures and exit codes**

- Create and start errors go through `toStatusError` [cli: C/run.go:151-154, 205-220, 322-356]:
  - 127 if the message contains `executable file not found`, `no such file or directory` or `system cannot find the file specified`;
  - 126 if it contains the text of EACCES or EISDIR;
  - 125 otherwise.
- The message is `docker: <error>`, a blank line, then `Run 'docker run --help' for more information` [cli: C/run.go:317-320].
- With `--rm`, a failed start waits for the removal before the CLI returns [cli: C/run.go:215-218]. The daemon removes the container on a failed start by itself [moby: daemon/start.go:125-132].
- An unreachable daemon fails the first ping: exit 1, `Cannot connect to the Docker daemon at <host>. Is the docker daemon running?` [cli: C/run.go:104-107; moby: client/errors.go:33-43].
- For comparison, `docker create` prints the same ID line but returns create errors unwrapped, so they exit 1 [cli: C/create.go:127-131].
- The image reference is sent as typed, and `ps` shows that string [cli: C/opts.go:661; moby: dc/view.go:343-345].

### 2.4 Q4: `docker ps`

**Flags, and what the daemon returns**

- Aliases: `docker ps`, `docker container ls`, `docker container list`, `docker container ps` [cli: C/list.go:42-45, 64-69].
- Flags: `-a/--all`, `-f/--filter`, `--format`, `-n/--last` (default -1), `-l/--latest`, `--no-trunc`, `-q/--quiet`, `-s/--size` [cli: C/list.go:50-59].
- `-l` means `-n 1` unless `-n` is set [cli: C/list.go:79-81]. The client sends `all=1` only with `-a`, and `limit` only when positive [moby: client/container_list.go:40-50].
- The daemon skips non-running containers unless `all` is set or the limit is positive [moby: daemon/list.go:436-439]. So `-n` and `-l` include every state [cli: D/container_ls.md:17-18; moby: integration-cli/docker_cli_ps_test.go:62-68].
  - Paused and restarting containers count as running here: both keep `Running` true (inference from [moby: dc/state.go:24-35, 294-311]).
- A `status=` filter also implies `all` [moby: daemon/list.go:281-287].
- **Order:** newest first, by creation time in nanoseconds [moby: daemon/list.go:97-104, 146-190, 193-208; integration-cli/docker_cli_ps_test.go:54-60, 775-789].

**Which format**

- `--format`, else `psFormat` from the CLI's config file, else the default table [cli: C/list.go:117-124].
- `-q` replaces a table or custom format with `{{.ID}}`, which has no header [cli: F/container.go:44-80].
  - Given together with `--format`, stderr also gets `WARNING: Ignoring custom format, because both --format and --quiet are set.` [cli: C/list.go:121-123].
- A custom template that uses `.Size` turns `--size` on, unless `--size=false` was given [cli: C/list.go:83-112].

**The default table**

- The template is `table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}`, plus `\t{{.Size}}` with `-s` [cli: F/container.go:22-23, 45-55].
- Headers: `CONTAINER ID`, `IMAGE`, `COMMAND`, `CREATED`, `STATUS`, `PORTS`, `NAMES`, `SIZE` [cli: F/container.go:25-33, 108-128; F/custom.go:8-24].
- **Rendering** [cli: F/formatter.go:93-120]:
  1. each row is the template's output plus `\n`;
  2. the header is the same template run over the map of header names;
  3. both go through `tabwriter.NewWriter(out, 10, 1, 3, ' ', 0)`: minimum cell width 10, padding 3, padded with spaces.
- **Column width** = max(10, widest cell + 3). The last cell of a line is not part of a column, so it is not padded, and no line has trailing spaces [cli: F/tabwriter/tabwriter.go:300-349, 355-411].
- **Widths are display widths**, measured by go-runewidth rather than counted in runes [cli: F/tabwriter/tabwriter.go:13, 21, 420-423].
  - That is the fork's one functional change from Go 1.26.1's `text/tabwriter`, checked with `diff`. The other differences are comments, a panic message, and handling of the `Escape` byte (0xff), which never occurs in UTF-8 text (inference).
  - The ellipsis `…` (U+2026) is East Asian Ambiguous in go-runewidth's table. It counts 2 columns under a CJK locale or `RUNEWIDTH_EASTASIAN=1` [cli: vendor/github.com/mattn/go-runewidth/runewidth_table.go:174, 197; vendor/github.com/mattn/go-runewidth/runewidth.go:139-148; vendor/github.com/mattn/go-runewidth/runewidth_posix.go:75-90].
  - The ellipsizer counts it as 1 [cli: F/displayutils.go:20-29], so in those locales truncated commands misalign by one column (inference).
- **With no containers**, only the header line prints (derived; **(executed)**):

```
CONTAINER ID   IMAGE     COMMAND   CREATED   STATUS    PORTS     NAMES
```

- **With rows** **(executed)**. The same widths and gaps appear in moby's own example [moby: daemon/list.go:591-607] and in the CLI's golden files [cli: C/testdata/container-list-without-format.golden:1-6; C/testdata/container-list-without-format-no-trunc.golden:1-3]. The second row's image was stored as `docker.io/library/busybox:latest`, the third's as a `sha256:` digest.

```
CONTAINER ID   IMAGE            COMMAND                  CREATED         STATUS                     PORTS     NAMES
4bed76d3ad42   nginx:alpine     "/docker-entrypoint.…"   1 second ago    Up Less than a second                test
b0318bca5aef   busybox:latest   "sh"                     4 seconds ago   Exited (0) 3 seconds ago             ecstatic_beaver
c0318bca5aef   3fbc63216742     "echo 12345678901234…"   2 hours ago     Created                              x
```

**Columns**

| Column | Content | Source |
|---|---|---|
| CONTAINER ID | The 12-character short ID; the full ID with `--no-trunc` | [cli: F/container.go:136-143] |
| IMAGE | `<no image>` if empty. With `--no-trunc`, the stored string. Otherwise an image ID or digest becomes its 12-character short form, a digest is stripped from a reference (the tag stays), and `docker.io/library/` is dropped | [cli: F/container.go:181-214] |
| COMMAND | The daemon joins the path and arguments with spaces, single-quoting any argument that contains a space. The CLI cuts it to 20 display columns (19 + `…`) and wraps it in Go's `strconv.Quote` | [moby: dc/view.go:351-364; cli: F/container.go:216-224; F/displayutils.go:39-77] |
| CREATED | `HumanDuration(client now − Created) + " ago"`: whole seconds, on the client's clock | [cli: F/container.go:233-241] |
| STATUS | The daemon's string, computed at list time (below) | [cli: F/container.go:267-271; moby: dc/view.go:309] |
| PORTS | Consecutive private ports grouped (`80-82/tcp`); published ports as `IP:public->private/proto`; separated by `, ` | [cli: F/container.go:387-463] |
| NAMES | Leading `/` stripped. Truncated: the first name without an inner `/`, which skips legacy link aliases. With `--no-trunc`: all names, comma-joined | [cli: F/container.go:145-169] |
| SIZE | `HumanSizeWithPrecision(SizeRw, 3)`, plus ` (virtual …)` when the image size is known; decimal units `B`, `kB`, `MB`… | [cli: F/container.go:273-287; vendor/github.com/docker/go-units/size.go:36, 57-62] |

- If the stored image reference no longer resolves to the container's image, the daemon reports the image ID instead [moby: daemon/list.go:586-650].

**STATUS strings** [moby: dc/state.go:81-116; dc/health.go:19-28]

| Condition (first match) | STATUS |
|---|---|
| Running, paused | `Up <d> (Paused)` |
| Running, restarting | `Restarting (<exit code>) <d since finish> ago` |
| Running, with a healthcheck | `Up <d> (healthy)`, `(unhealthy)` or `(health: starting)` |
| Running | `Up <d>` |
| Being removed | `Removal In Progress` |
| Dead | `Dead` |
| Never started | `Created` |
| No finish time | empty |
| Otherwise | `Exited (<exit code>) <d since finish> ago` |

- `<d>` is `HumanDuration` of now − `StartedAt`, or of now − `FinishedAt`. Hence `Up Less than a second` and `Exited (0) Less than a second ago`, with a capital L.
- **STATE** (`{{.State}}`, and the `status=` filter) is one of `created`, `running`, `paused`, `restarting`, `removing`, `exited` or `dead` [moby: api/types/container/state.go:11-19; dc/state.go:118-147; cli: D/container_ls.md:179-187].

**`HumanDuration`** (go-units v0.5.0) [cli: vendor/github.com/docker/go-units/duration.go:10-35] **(executed)**

Seconds and minutes truncate. Hours round: H = int(hours + 0.5).

| Elapsed | Text |
|---|---|
| < 1 s, including negative skew | `Less than a second` |
| 1 s | `1 second` |
| 2–59 s | `N seconds` |
| 60–119 s | `About a minute` |
| 2–59 min | `N minutes` |
| H = 1 (60 min to under 90 min) | `About an hour` |
| H = 2–47 | `N hours` |
| H = 48–335 | `H/24 days`; 47 h 30 min is already `2 days` |
| H = 336–1439 | `H/168 weeks`; 59 days is `8 weeks` |
| H = 1440–17519 | `H/720 months`; 729 days is `24 months` |
| H ≥ 17520 | `floor(hours)/8760 years` |

**`--format`**

- A `table …` template gets a header and tabwriter alignment; any other template prints each row as is. A literal `\t` or `\n` in the template becomes a tab or newline [cli: F/formatter.go:17-69].
- When the header is rendered, `json`, `upper`, `lower`, `title`, `split`, `join` and `truncate` return the header text unchanged. `pad` still applies, to keep alignment [cli: templates/templates.go:29-61].
- `--format json` prints one `{{json .}}` object per row [cli: F/formatter.go:56-58].
  - The keys are the context's zero-argument methods, sorted, and marshalled with `json.Marshal`, which escapes HTML [cli: F/reflect.go:14-71; F/container_test.go:493-548].
  - They are `Command`, `CreatedAt`, `HealthStatus`, `ID`, `Image`, `Labels`, `LocalVolumes`, `Mounts`, `Names`, `Networks`, `Platform`, `Ports`, `RunningFor`, `Size`, `State` and `Status`.
  - The values are truncated unless `--no-trunc`: a 12-character ID, an ellipsized command **(executed)**.
- `.CreatedAt` is Go's `time.Time.String()` in the client's local zone, e.g. `2026-09-29 04:04:02 -0500 CDT` [cli: F/container.go:226-231] **(executed)**.
- The placeholders are listed at [cli: D/container_ls.md:396-414], the filters at [ibid.:75-91].

### 2.5 Q5: `docker stop`

- **Flags:** `-s/--signal`, `-t/--timeout`, and the deprecated `--time`. Both `--timeout` and `--time` together fail with `conflicting options: cannot specify both --timeout and --time` [cli: C/stop.go:28-57].
- **The timeout is sent only when a flag gives one** [cli: C/stop.go:36, 60-63; moby: client/container_stop.go:40-52]. The reference table's default of `0` is the flag's zero value, not the behavior [cli: D/container_stop.md:12-15, 58-61].
- **Output:**
  - The arguments run in parallel, at most 50 at a time, and results are read in argument order.
  - Each success prints the argument *as typed* plus `\n` on stdout.
  - Failures are joined and printed after the successes, one per line, on stderr. Any failure makes the exit status 1 [cli: C/stop.go:59-82; C/utils.go:52-82; cmd/docker/docker.go:43-55].
  - Example: `docker stop test` prints `test` [cli: D/container_run.md:147-152].
- **Stopping a stopped container succeeds.** The daemon answers 304 (`container is already stopped`), the client treats < 400 as success, and the CLI prints the name [moby: daemon/stop.go:25-37; daemon/server/httpstatus/status.go:40-41; client/request.go:221-227]. This holds for never-started containers too [moby: integration/container/stop_test.go:143-149].
- **Signal:** `--signal`, else the container's `StopSignal` (from `STOPSIGNAL` or `--stop-signal`), else SIGTERM [moby: daemon/stop.go:56-67; dc/container.go:56-57, 596-606].
- **Timeout:**
  - `-t`, else the container's `StopTimeout` (`--stop-timeout`), else the daemon's `default-stop-timeout`: 10 s on Linux, 30 s on Windows [moby: daemon/stop.go:57-70; daemon/config/config_linux.go:40; daemon/config/config_windows.go:21; daemon/config/config.go:193-198].
  - A negative value waits forever [moby: daemon/stop.go:72-75, 93-97, 109-113].
- **Sequence** [moby: daemon/stop.go:85-131; daemon/kill.go:172-216]:
  1. send the signal;
  2. wait up to the timeout for the container to stop, or only 2 s if sending failed;
  3. SIGKILL, and wait up to 10 s;
  4. kill the process directly, and wait 2 s more.
- `docker stop` returns only after the exit has been recorded [moby: daemon/stop.go:76-83].
- **Errors:**
  - an unknown container: `Error response from daemon: No such container: <arg>` [moby: integration/container/stop_test.go:150-156];
  - other failures: `cannot stop container: <arg>: <cause>`, HTTP 500 [moby: daemon/stop.go:38-41];
  - an invalid signal is reported only if the container is running (inference from [moby: daemon/stop.go:30-37, 61-66]).

### 2.6 Q6: `docker kill`

- **Flag:** `-s/--signal`. Output, parallelism and exit status are as for `stop` [cli: C/kill.go:40-66].
- **Default: SIGKILL** [moby: daemon/kill.go:36-40].
- **`--signal`** takes a name, case-insensitive, with or without `SIG`, or a number; `0` is invalid [moby: vendor/github.com/moby/sys/signal/signal.go:38-51; cli: D/container_kill.md:63-70].
  - The errors are `invalid signal: <value>` and `the linux daemon does not support signal <n>` [moby: daemon/kill.go:41-49].
- **The SIGKILL path:**
  - the container must be running, else `container <64-hex ID> is not running` (409);
  - then SIGKILL, up to 10 s for the exit, a direct kill, and 2 s more [moby: daemon/kill.go:172-216; daemon/errors.go:13-26].
- **Other signals:**
  - they need a running task too (`container <ID> is not running`, 409), and return once the signal is sent [moby: daemon/kill.go:66-78; dc/container.go:865-874];
  - any kill marks the container manually stopped, which cancels `unless-stopped` restarts;
  - the next exit also skips the restart policy, except when the container has a custom stop signal and some other signal than SIGKILL is sent [moby: daemon/kill.go:80-103].
- **The route wraps every error:** `cannot kill container: <arg>: <cause>` [moby: routes.go:339-351]. So (derived):
  - a stopped container gives `Error response from daemon: cannot kill container: web: container 4bed…a19 is not running`, with the full ID even when a name was given;
  - a missing one gives `… cannot kill container: nosuch: No such container: nosuch`.
  - The API test for a created container checks "is not running" [moby: integration/container/kill_test.go:133-140].
- **So `stop` is idempotent and `kill` is not.**

### 2.7 Q7: `docker rm`

- **Flags:** `-f/--force` ("uses SIGKILL"), `-v/--volumes`, `-l/--link`. The aliases include `docker container remove` [cli: C/rm.go:43-70].
- **CLI** [cli: C/rm.go:72-99]:
  - it trims `/` from both ends of each argument, and rejects an empty result with `container name cannot be empty`;
  - it runs in parallel and prints each argument as typed;
  - **with `-f`, not-found errors are dropped: nothing printed, exit 0** [cli: C/rm.go:89-92; C/rm_test.go:16-62].
- **Daemon, without `-f`** [moby: daemon/delete.go:57-61, 92-100]:
  - running: `cannot remove container "<arg>": container is running: stop the container before removing or force remove` (409);
  - paused: `cannot remove container "<arg>": container is paused and must be unpaused first`;
  - restarting: the running message with `container is restarting` (derived from `State()`).
- **A concurrent second `rm`:** `removal of container <arg> is already in progress` (409) [moby: daemon/delete.go:38-45].
- **`-f`** kills with SIGKILL, never the graceful stop [moby: daemon/delete.go:101-103]. Removal then [ibid.:110-186]:
  1. runs a stop with a 3 s timeout, a no-op by then;
  2. marks the container dead and saves that;
  3. releases the writable layer and removes the container's directory;
  4. deregisters the container and releases its names; anonymous volumes go only with `-v`;
  5. emits `destroy`.
- API tests: [moby: integration/container/remove_test.go:83-113].

### 2.8 Q8: `docker wait`

- **No flags. The arguments are waited on one at a time, in order** [cli: C/wait.go:20-57].
  - Each exit code prints as a decimal plus `\n` on stdout when that container is done.
  - A later container that exits first prints only after the earlier ones (inference).
- **The CLI exits 0 whatever the containers' codes.** Errors are joined, printed on stderr, and give exit 1 [cli: C/wait.go:45-56].
- **Condition:** the CLI sends none, so the daemon uses `not-running` [moby: routes.go:409-436; api/swagger.yaml:9509-9520].
  - The daemon writes the response header at once, and the JSON body at the exit [moby: routes.go:442-471]. The client returns once it has the header [moby: client/container_wait.go:27-61].
- **A container that is not running answers at once with its stored exit code** [moby: dc/state.go:157-171, 201-211]:
  - an exited container: its code;
  - a never-started container: 0;
  - a container whose start failed: 126, 127 or 128 [moby: daemon/start.go:112-118; daemon/errors.go:113-120] (inference).
  - The reference page says instead that `docker wait` "returns `0`" for a container that had already exited [cli: D/container_wait.md:13-15] (E6).
- Removal wakes waiters too [moby: dc/state.go:395-409]. `docker run` itself waits on `next-exit`, or on `removed` with `--rm` [cli: C/utils.go:12-50].

### 2.9 Q9: `docker logs`

- **Flags:** `--details`, `-f/--follow`, `--since`, `-n/--tail` (default `all`), `-t/--timestamps`, `--until`. Exactly one container [cli: C/logs.go:31-52].
- **Streams.** The CLI inspects the container, then streams its logs [cli: C/logs.go:56-83]:
  - with a TTY, the stream is copied raw to stdout;
  - without one, it is demultiplexed: stdout frames to stdout, stderr frames to stderr. The daemon multiplexes exactly when the container has no TTY [moby: routes.go:265-276];
  - a system-error frame ends the copy with `error from daemon in stream: <text>` [moby: api/pkg/stdcopy/stdcopy.go:18, 78-92, 125-126]. The daemon writes read errors as `Error grabbing logs: <err>\n` on that stream [moby: daemon/server/httputils/logstream/logstream.go:53-56].
- **`-t`:** each message is prefixed with its time as `2006-01-02T15:04:05.000000000Z07:00` and a space. There are always 9 fractional digits [moby: daemon/server/httputils/logstream/logstream.go:18-20, 74-98; cli: D/container_logs.md:44-49].
  - The json-file driver stamps `time.Now().UTC()` as it copies each line, so the zone is always `Z` [moby: daemon/logger/copier.go:120-124, 145-148].
  - `--details` inserts the attributes as sorted, query-escaped `key=value` pairs, comma-separated, then a space [moby: daemon/server/httputils/logstream/logstream.go:92-95, 103-119].
- **Line fidelity.** json-file is the default driver [moby: daemon/config/config.go:47-48, 362-365].
  - The copier splits output on `\n`, and the driver adds the `\n` back [moby: daemon/logger/copier.go:99-131; daemon/logger/jsonfilelog/jsonfilelog.go:127-143].
  - A final line without a newline stays without one [moby: daemon/logger/copier.go:136-165].
  - A line longer than the buffer is logged as partials that share the first partial's timestamp. With `-t`, each partial gets its own prefix (inference).
- **Follow applies only while the container runs.** For a stopped container the daemon opens a fresh reader and ignores `-f`, so `docker logs -f` prints and exits [moby: daemon/logs.go:54-58, 78-84, 148-167].
  - A follow ends when the container exits and its log file closes [moby: daemon/logger/loggerutils/follow.go:123-129; dc/monitor.go:14-59].
- **Tail** [moby: client/container_logs.go:101-104; daemon/logs.go:73-76; daemon/logger/loggerutils/logfile.go:402-405, 728; cli: D/container_logs.md:38-41]:
  - `all` is not sent;
  - a non-integer means all;
  - `0` means none, useful with `-f`;
  - a negative number means all.
- **`--since` and `--until`** are parsed by the client and sent as Unix seconds with nanoseconds [moby: client/container_logs.go:73-86; client/internal/timestamp/timestamp.go:20-96; cli: D/container_logs.md:57-70]. They accept:
  - Go durations such as `42m`, counted back from now;
  - RFC 3339 variants, in the client's local zone when no zone is given;
  - Unix timestamps.
- **Errors:**
  - a missing container: `Error response from daemon: No such container: <arg>`, from the inspect [moby: integration-cli/docker_cli_logs_test.go:381-386];
  - `--log-driver none`: `configured logging driver does not support reading` (501) [moby: daemon/logs.go:50-52; daemon/logger/logger.go:19-26];
  - a dead or removing container: `can not get logs from container which is dead or marked for removal` (409) [moby: daemon/logs.go:46-48].
- **A `--rm` container's logs go with it.** The json log is `<id>-json.log` in the container's directory, which removal deletes [moby: dc/container.go:460-464; daemon/delete.go:161-168] (inference). `docker logs` on an exited `run --rm -d` container fails with "No such container".

### 2.10 Q10: what remains, states, and daemon restarts

**What `docker run` without `--rm` leaves behind**, until `docker rm`. This is inference from the files the daemon writes and what `rm` deletes [moby: dc/container.go:53-54, 460-464; dc/container_unix.go:47-55; daemon/container_operations_unix.go:578-582; daemon/delete.go:126-186]:

- the record, `config.v2.json` and `hostconfig.json`;
- the `hostname`, `hosts` and `resolv.conf` files;
- the json log;
- the writable layer;
- anonymous volumes;
- the name reservation.

"A container's file system persists even after the container exits", and `docker start` can run it again [cli: D/container_run.md:121-126, 1225-1238].

**Transitions**

- `created` → `running` via start → `exited` via exit, stop or kill [moby: dc/state.go:118-147, 246-311, 361-409; cli: D/container_ls.md:179-187].
  - `restarting` and `paused` are sub-states of running.
  - `removing` lasts for the duration of `rm`.
  - `dead` marks a removal that failed or was interrupted.
- **A failed start leaves the container `created`, not `exited`.** It gets exit code 126, 127 or 128 and an error message, but no `StartedAt`, so STATUS says `Created` [moby: daemon/start.go:110-124; daemon/errors.go:113-170; dc/state.go:107-109] (inference).
- **Exit codes:** a process killed by a signal reports 128 + the signal number [ctrd: pkg/sys/reaper/reaper_unix.go:280-289]. So `Exited (143)` after SIGTERM and `Exited (137)` after SIGKILL (derived).
- **Auto-removal (`--rm`) is the daemon's job, not the client's.** It happens:
  - at exit, unless a restart follows [moby: daemon/monitor.go:134-140, 340-356];
  - on a failed start [moby: daemon/start.go:125-132];
  - at the next daemon start (below).

  So `--rm` works even when the client has gone (inference).

**Daemon restarts, with live-restore off** (a boolean, false unless set [moby: daemon/config/config.go:227-229])

- **At shutdown**, every running container is stopped in parallel with its own stop signal and timeout, by default SIGTERM and 10 s, then SIGKILL [moby: daemon/daemon.go:1482-1494, 1533-1574; dc/memory_store.go:73-84].
  - The daemon gives up after the larger of `shutdown-timeout` (default 15 s) and the longest stop timeout + 5 s [moby: daemon/daemon.go:1496-1530; daemon/config/config.go:49-51; daemon/command/daemon.go:602-624].
  - The docs list "Docker daemon restarts which kills all running containers" among the causes of exit 137 [cli: D/container_ls.md:169-173]. By the sequence above, only processes that outlive the stop signal get 137 (derived).
  - A kill during shutdown does not count as a manual stop [moby: daemon/kill.go:95-103; daemon/monitor.go:97-99], so `always` and `unless-stopped` containers restart when the daemon returns (inference).
- **At start** [moby: daemon/daemon.go:440-574, 643-700]:
  - tasks still alive in containerd are shut down;
  - containers recorded as running whose task has died or vanished are marked exited with the task's exit status, or **255** when there is none, e.g. after a host reboot (inference for the reboot case);
  - restart policies run;
  - `--rm` containers are removed;
  - interrupted removals become `dead`.
- **With live-restore on**, shutdown leaves running containers alone [moby: daemon/daemon.go:1543-1554].

### 2.11 Q11: error text and exit status

**Exit status rules** [cli: cmd/docker/docker.go:43-55, 97-120; cli/error.go:3-27; cli/cobra.go:80-91; cli/required.go:9-100]:

- A `cli.StatusError` exits with its code. With an empty message it prints nothing, which is how `docker run` passes on the container's code.
- A flag error exits 125 and prints `<err>`, a blank line, `Usage:  <use line>`, a blank line, then `Run '<cmd> --help' for more information`.
- An argument-count error exits 1. `RequiresMinArgs` ends with `See '<cmd> --help' …`; the other checks say `Run …`.
- Termination by a caught signal exits 128 + the signal number, silently.
- Everything else exits 1.
- Plugin hooks can print after an error in a terminal. They are off unless `DOCKER_CLI_HOOKS`, `DOCKER_CLI_HINTS` or the config's `features.hooks` turns them on [cli: cmd/docker/docker.go:575-582; cli/command/cli.go:179-207].

The table assembles §2.1–2.9's sources (derived); E1 checks it against a real dockerd.

| Case | stderr | Exit |
|---|---|---|
| `stop`, `rm`, `wait` or `logs` on a missing container | `Error response from daemon: No such container: x` | 1 |
| `kill` on a missing container | `Error response from daemon: cannot kill container: x: No such container: x` | 1 |
| Several missing (`stop a b`) | One line each, after every success has printed | 1 |
| Ambiguous prefix | `Error response from daemon: multiple IDs found with provided prefix: 4b` | 1 |
| `stop` on a stopped or created container | Nothing; stdout echoes the name | 0 |
| `kill` on a stopped container | `Error response from daemon: cannot kill container: web: container <64-hex> is not running` | 1 |
| `rm` on a running container | `Error response from daemon: cannot remove container "web": container is running: stop the container before removing or force remove` | 1 |
| `rm -f` on a missing container | Nothing | 0 |
| `logs` with `--log-driver none` | `Error response from daemon: configured logging driver does not support reading` | 1 |
| `run --name taken …` | `docker: Error response from daemon: Conflict. The container name "/taken" is already in use by container "<64-hex>". You have to remove (or rename) that container to be able to reuse that name.`, a blank line, `Run 'docker run --help' for more information` | 125 |
| `run --name a …` | `docker: Error response from daemon: Invalid container name (a), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed` + hint | 125 |
| `run -d -a stdout …` | `conflicting options: cannot specify both --attach and --detach` | 1 |
| `run --rm --restart=always …` | `docker: conflicting options: cannot specify both --restart and --rm` + hint | 125 |
| `run` of a missing executable | `docker: Error response from daemon: <runtime error containing "executable file not found">` + hint | 127 |
| `stop` with no argument | `docker: 'docker stop' requires at least 1 argument`, usage, then `See 'docker stop --help' for more information` | 1 |

### 2.12 Where the reference pages disagree with the code

- The `ps` examples space columns more widely than the code does (`CONTAINER ID` followed by 8 spaces) and leave commands unquoted [cli: D/container_ls.md:32-38, 101-140]. The code pads by 3 and quotes (§2.4). Newer examples match the code [cli: D/container_run.md:136-142].
- `stop`'s options table gives `-t` a default of `0`, but the code sends no timeout unless the flag is given (§2.5) [cli: D/container_stop.md:15].
- `wait`'s note says it returns 0 for a container that has already exited, but the code returns the stored exit code (§2.8) [cli: D/container_wait.md:13-15].
- `run`'s `-a stdin` examples say the container's ID is printed [cli: D/container_run.md:980-1003]. The code prints it only when no stream is attached, and `-a stdin` attaches one [cli: C/run.go:173-181] (inference).
- The API schema calls the 32-byte ID "128-bit" [moby: api/swagger.yaml:5743].

## 3. Implications for shards (ranked)

Ranked by how often a user or a script would see the difference.

1. **The daemon owns IDs, names and argument resolution, exactly as dockerd does.**
   - *What:*
     - 32 bytes from the OS CSPRNG as 64 lowercase hex, redrawn while the first 12 characters are digits;
     - 12-character short IDs;
     - resolution by full ID, then exact name, then unique prefix, with dockerd's three messages (§2.1).
   - *Why first:* scripts address containers through the output of `docker run -d`, `ps -q` and `$(docker ps -q)`.
   - *Snapshot trap:* draw the ID on the host for each run, never from guest state restored from a template, whose RNG and hostname are the template's.
     - A restored guest must take `hostname`, `/etc/hostname` and `HOSTNAME`, plus `TERM=xterm` with `-t`, from the run [moby: daemon/container.go:123-133; dc/container.go:818-831] (inference).
2. **`ps` must be byte-exact, and its parts come from Go** (§2.4):
   - tabwriter with minimum width 10, padding 3, the last column unpadded, go-runewidth display widths;
   - `HumanDuration` with its truncation and rounding; STATUS computed at list time; CREATED from whole seconds on the client's clock;
   - COMMAND cut to 19 + `…` and quoted by Go's `strconv.Quote`, whose escapes differ from Rust's `{:?}`. Go writes `\a`, `\x1b` and `\x00` where Rust writes `\u{7}`, `\u{1b}` and `\0` [go: src/strconv/quote.go:64-109; rust: library/core/src/char/methods.rs:474-489];
   - IMAGE familiar-name trimming, newest-first order, and `-n`/`-l` covering every state.
   - *Test:* golden tables regenerated from the pinned Go code (E2).
3. **One contract for the commands that take several containers** (§2.5–2.8):
   - echo each argument as typed, in argument order, while running up to 50 at a time;
   - join errors on stderr after the successes, and exit 1;
   - `stop` on a stopped container succeeds, `kill` on one fails, and `rm -f` ignores missing ones;
   - `wait` is sequential, prints codes and exits 0.
4. **Error text comes from one formatter** (§2.11).
   - `Error response from daemon: ` + dockerd's exact message.
   - `run`'s create and start errors get `docker: `, the help hint, and 125, 126 or 127.
   - Flag errors exit 125; argument-count errors exit 1.
5. **Names: port the legacy generator verbatim** (§2.2):
   - both lists, the `boring_wozniak` redraw, the digit on retries, six tries, then the short ID;
   - `--name` checked with dockerd's regex (so two characters minimum), and its conflict text, leading `/` included;
   - the name reserved at create and released at `rm`.
6. **Exit bookkeeping lives in the daemon, not the client** (§2.10).
   - A run without `--rm` becomes an exited container that keeps its exit code, times, logs and name.
   - `--rm` removal happens on exit, on a failed start and at daemon start, so it still happens when the client has gone.
   - A failed start stays `Created` with 126, 127 or 128. A signal death is 128 + N; a lost task is 255.
   - D26 first handed a run over and forgot it; commit 38c1d7b now follows each run to its end, which this bookkeeping needs.
7. **Logs must be kept on the host, per container, and outlive the VM** (§2.9).
   - Frames tagged stdout or stderr, stamped in UTC with nanoseconds as each line arrives, with json-file's line semantics.
   - Follow only while running, ending at exit; tail `0` means none.
   - Logs go when the container is removed.
   - The VM's memory is gone once it exits, so the copy has to be made while output flows (inference).
8. **`run -d` prints the ID as soon as create succeeds**, with pulls and warnings on stderr (§2.3). In shards, create is a template lookup plus claiming a warm VM, so the ID can print before the VM resumes (inference).
9. **Choose, and document, what a daemon restart does to runs** (§2.10).
   - dockerd by default stops everything within max(15 s, longest stop timeout + 5 s). When it returns, it restarts `always` and `unless-stopped` containers and removes `--rm` ones.
   - Commit 38c1d7b gives `shards daemon stop` dockerd's shutdown behavior, while an upgrade keeps runs alive, which is closer to live-restore.
   - Either is defensible, but `ps` after a restart must show the state that results.

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| Docker parity vs snapshot starts | The hostname, `HOSTNAME` and the ID belong to each container [moby: daemon/container.go:123-133; dc/container.go:818-831]; a template freezes the guest's |
| Docker parity vs D26's hands-off daemon | dockerd records every exit, auto-removes, and serves logs after the exit (§2.9–2.10). A daemon out of the data path must still learn each run's end (38c1d7b's DONE) |
| Byte parity vs a Rust implementation | Go's `strconv.Quote`, go-units' thresholds and go-runewidth's tables must be reproduced, not approximated (§2.4) |
| Docker's default restart semantics vs upgrades that stop no command | dockerd stops every container at shutdown unless live-restore is on (§2.10); 38c1d7b keeps runs through an upgrade |

## 4. Open questions needing our own measurement

- **E1. A differential test against dockerd v29.8.1.** In a Linux VM or on a CI runner, run every case in §2.11 and the `ps` layouts with the pinned CLI. Record stdout, stderr and the exit status as golden files. First priorities:
  - the not-running `kill` message, with the full ID;
  - `rm`'s quoting;
  - `run -d -a` exiting 1;
  - `wait` on a container that exited non-zero (E6).
- **E2. Commit the formatter harness.** The throwaway Go test behind the **(executed)** items belongs under `docs/research/measurements/`. It regenerates golden `ps` tables and `HumanDuration` values for shards' tests.
  - *Done:* `scripts/docker-cli` runs, inside a docker/cli v29.8.1 checkout, the CLI's formatter on 16 `ps` tables (with and without `--no-trunc` and `-q`, in and out of an East Asian locale) and go-units on 22 durations, and the CLI's command tree on 110 command lines. shards matches every answer byte for byte (crates/shards/src/daemon/docker-ps.json, crates/cmdline/tests/docker-cli.json).
- **E3. The CLI's status after Ctrl-C** during `logs -f`, `wait` and `stop`. `main` exits 128 + the signal only if the returned error is its signal type, and a plain `context.Canceled` exits 0 [cli: cmd/docker/docker.go:43-55, 97-111]. Which one each command returns is UNVERIFIED.
- **E4. TTY logs.** Whether `docker logs` on a `-t` container returns `\r\n` line ends from the pty: UNVERIFIED. The copier stores raw bytes, so it depends on the pty's output settings.
- **E5. `run -d` when start fails:** how often the ID reaches stdout before the error (§2.3).
- **E6. `wait` on an exited container:** whether it prints the stored code (the code's answer) or 0 (the reference page's) (§2.8, §2.12).

## 5. References

**Source code** (paths and lines cited inline)

- docker/cli v29.8.1 (4a63305d7433; tag object 477f1252f239). Its vendor tree includes github.com/docker/go-units v0.5.0, github.com/mattn/go-runewidth v0.0.29, github.com/moby/moby/client v0.6.0 and github.com/moby/moby/api v1.56.0.
- moby/moby docker-v29.8.1 (464cd50c3d9e; tag object b2d20c90a74a). Includes `api/swagger.yaml` (Engine API 1.56) and, vendored, github.com/moby/sys/signal v0.7.1.
- containerd v2.4.1 (f2551031d727): `pkg/sys/reaper/reaper_unix.go`, read raw from GitHub.
- Go 1.26.1: `src/math/rand/v2/rand.go` and `src/strconv/quote.go`, from the local toolchain.
- Rust 1.94.1: `library/core/src/char/methods.rs`, from the local `rust-src`.

**Official documentation**

- docker/cli v29.8.1 `docs/reference/commandline/`: `container_run.md`, `container_ls.md`, `container_stop.md`, `container_kill.md`, `container_rm.md`, `container_wait.md` and `container_logs.md`. These are the source of the CLI reference on docs.docker.com, and their option tables are generated from the code (the `MARKER_GEN` blocks).

**Our notes:** container-engine-internals.md §2.3; warm-pool-daemon.md §2.7; D26 (architecture.md); commit 38c1d7b.
