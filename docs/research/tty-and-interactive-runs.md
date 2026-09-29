# Terminals in `docker run`: `-t`, `-i`, raw mode, resize, detach, and the pty underneath

Research note, 2026-09-29. Evidence only; nothing here is a final design decision.

It informs `shards run -t` and `-it`. D16 lists TTYs as "Not yet", and D27 parses `--tty` but refuses it [shards: docs/design/architecture.md:243, 714-716, 796-799]. The goal is parity wherever a user or a script can see a difference:

- the bytes on the terminal, and the terminal's state afterwards;
- which process gets which signal;
- the exit status;
- what `logs` prints.

Pinned sources (citation paths are relative to each repository; abbreviations as listed):

| Tag | Source |
|---|---|
| `cli` | docker/cli v29.8.1, `4a63305d7433`. `C/` = `cli/command/container/`, `S/` = `cli/streams/`, `D/` = `docs/reference/commandline/`, `term/` = `vendor/github.com/moby/term/` (v0.5.2) |
| `cli-git` | docker/cli history (blobless clone of github.com/docker/cli): commits `ee295049231c` and `526dfffc26f3` |
| `moby` | moby/moby docker-v29.8.1, `464cd50c3d9e`. `routes.go` = `daemon/server/router/container/container_routes.go`, `dc/` = `daemon/container/`, `stream/` = `daemon/internal/stream/`, `lcd/` = `daemon/internal/libcontainerd/remote/` |
| `ctrd` | containerd v2.3.5, `1294c24a7da8`. `shim/` = `cmd/containerd-shim-runc-v2/`, `console/` = `vendor/github.com/containerd/console/` (v1.0.5), `go-runc/` = `vendor/github.com/containerd/go-runc/` (v1.1.0) |
| `runc` | opencontainers/runc v1.5.1, `8f2685a471d3`. `lc/` = `libcontainer/` |
| `tini` | krallin/tini v0.19.0, `src/tini.c` (docker-init) |
| `linux` | Linux 6.18.48 from the stable tree at git.kernel.org, the version `scripts/build-kernel.sh` builds [shards: scripts/build-kernel.sh:12] |
| `xnu` | apple-oss-distributions/xnu `xnu-12377.121.6` |
| `libc` | apple-oss-distributions/Libc `Libc-1752.120.2`, `gen/FreeBSD/termios.c` |
| `glibc` / `musl` | glibc 2.42 `termios/cfmakeraw.c` (sourceware.org) / musl v1.2.5 `src/termios/cfmakeraw.c` (git.musl-libc.org) |
| `libc-rs` | Rust `libc` 0.2.189, the version in shards' `Cargo.lock` |
| `go` | Go 1.26.1 standard library, the local toolchain (moby's `go.mod` asks for 1.26.3) |
| `posix` | POSIX.1-2024 (IEEE Std 1003.1-2024), pubs.opengroup.org/onlinepubs/9799919799 |
| `shards` | this repository at `5f6da0f`. `PM Mn` = docs/research/platform-measurements.md |

Why these versions:

- moby's Dockerfile pins containerd v2.3.5, runc v1.5.1 and tini v0.19.0 for CI and its static binaries [moby: Dockerfile:141-146, 247-252, 292-295], and `go.mod` requires containerd v2.3.5, moby/term v0.5.2 and containerd/console v1.0.5 [moby: go.mod:31, 89, 164].
- The Dockerfile warns that the .deb and .rpm packages depend on a separate containerd.io package, "which may be a different version" [moby: Dockerfile:143-144].
- container-lifecycle-cli.md cited one containerd v2.4.1 file. This note uses v2.3.5 throughout.

Identity checks (with `diff` or `cmp`):

- moby/term is identical in the CLI's and moby's vendor trees.
- containerd/console is identical in runc's and containerd's vendor trees.
- Every Linux file cited is byte-identical to tag v6.18, except `kernel/signal.c` and `fs/super.c`, which differ by a few lines, none in the functions cited (derived from the diffs' hunks). Line numbers are 6.18.48's.

Markers:

- **(derived)**: our arithmetic or comparison on cited facts.
- **(inference)**: reasoning from code or docs, not stated there.
- **UNVERIFIED**: no acceptable source found.

## 1. Scope

- **Q1.** The CLI with `-t`, `-i`, `-it` and `-t` without `-i`:
  - raw mode, and how macOS differs from Linux;
  - when the CLI refuses, and how it restores the terminal;
  - window size and SIGWINCH, and detach keys;
  - stderr and exit codes;
  - `-t` with a stdout that is not a terminal, and `-t` with `-d`.
- **Q2.** What dockerd, containerd and runc do to give the container a terminal:
  - allocation and devpts;
  - the pty's initial termios and window size;
  - the session, `TERM`, the resize path, and the end of the stream.
- **Q3.** Signals with a TTY: sig-proxy, Ctrl-C, SIGWINCH, job control.
- **Q4.** `docker logs` for a TTY container (container-lifecycle-cli.md E4).
- **Q5.** Implications for shards (§3, §4).

warm-pool-daemon.md §2.7 covers foreground `docker run` and summarizes its client-side TTY handling in five bullets. This note expands those bullets with line-level sources and adds the engine side.

## 2. Findings

### 2.1 Q1: the CLI

**Flags**

- `-t, --tty` "Allocate a pseudo-TTY". `-i, --interactive` "Keep STDIN open even if not attached" [cli: C/opts.go:199, 208].
- `--sig-proxy` (default true) and `--detach-keys` belong to `run` [cli: C/run.go:61-63].
- `-i` attaches stdin. Without `-a`, stdout and stderr are attached. Attached `-i` sets StdinOnce [cli: C/opts.go:359-366, 729-732].

**What each combination does**

| Invocation | Container | The user's terminal | Resize | Container's input | Source |
|---|---|---|---|---|---|
| `-it`, stdin a terminal | pty, `TERM=xterm` | raw | if stdout is a terminal | the CLI's stdin, scanned for the detach keys | [cli: C/hijack.go:95-132; C/run.go:229-233] |
| `-it`, stdin not a terminal | not created | untouched | — | — | [cli: C/run.go:128-131; S/in.go:66-75] |
| `-t` alone | pty | cooked: raw mode needs an attached stdin | if stdout is a terminal | none. The CLI never reads its stdin, and the pty's input side is never written or closed (inference) | [cli: C/hijack.go:96-100; moby: daemon/start.go:233-235; daemon/container.go:86-92; dc/container.go:752-758] |
| `-i` alone | pipes | cooked | none | the CLI's stdin, closed at its end | [cli: C/opts.go:729-732; moby: stream/attach.go:69-72] |
| `-t` or `-it`, stdout not a terminal | pty sized 0×0 (inference) | raw only with `-i` and a terminal stdin | none | as for `-it` or `-t` with a terminal stdout | [cli: C/create.go:327; S/stream.go:65-77; moby: daemon/oci_opts.go:15] |
| `-dt`, `-dit` | pty sized like the CLI's stdout at create | untouched | none | `-dit`: kept open for a later attach | [cli: C/run.go:128-141; C/create.go:327] |

- The reference says `-t` without `-i` "still allocates a pseudo-TTY to the container, but with no way of writing to `STDIN`" [cli: D/container_run.md:1087-1089].
- So a process reading its terminal under `-t` alone blocks with no end-of-file (inference).

**Refusal**

- The check runs before anything is created, and only when not detached [cli: C/run.go:128-131].
- **Condition:** TTY, and stdin attached, and stdin not a terminal. Stdout is not checked [cli: S/in.go:66-75].
- **v29.8.1 text:** `cannot attach stdin to a TTY-enabled container because stdin is not a terminal` [cli: S/in.go:74].
- **Output and status:** a plain error. `main` prints it plus `\n` on stderr and exits 1, with no `docker: ` prefix and no help hint [cli: cmd/docker/docker.go:43-55, 97-120].
- **History:** until v29.4.0 the text was `the input device is not a TTY`. Windows added `.  If you are using mintty, try prefixing the command with 'winpty'`. Commit 526dfffc26f3 (2026-04-01, first tag v29.4.0) changed it [cli-git: 526dfffc26f3].
- `docker attach` runs the same check unless `--no-stdin` [cli: C/attach.go:92-94].
- **What counts as a terminal:** an `*os.File` on which the termios get (TCGETS or TIOCGETA) succeeds [cli: S/stream.go:13-25; term/term_unix.go:28-36, 53-56, 88-94].

**Raw mode: when**

- Raw mode is set only with a TTY and an attached stdin [cli: C/hijack.go:95-100].
- It goes on stdin's terminal. "Raw" on stdout does nothing on Unix [cli: C/hijack.go:204-209; S/out.go:49-57; term/term_unix.go:80-86].
- A non-empty `NORAW` skips it [cli: S/stream.go:53-63].
  - `NORAW` is missing from the environment-variable table of the CLI reference [cli: D/docker.md:115-132].

**Raw mode: which flags.** moby/term's `makeRaw` is the same on every Unix [cli: term/termios_unix.go:15-35]:

| Field | Cleared | Set |
|---|---|---|
| `c_iflag` | IGNBRK, BRKINT, PARMRK, ISTRIP, INLCR, IGNCR, ICRNL, IXON | |
| `c_oflag` | OPOST | |
| `c_lflag` | ECHO, ECHONL, ICANON, ISIG, IEXTEN | |
| `c_cflag` | CSIZE, PARENB | CS8 |
| `c_cc` | | VMIN = 1, VTIME = 0 |

- **glibc, musl and containerd/console** clear and set exactly these [glibc: termios/cfmakeraw.c:20-31; musl: src/termios/cfmakeraw.c:4-13; ctrd: console/tc_unix.go:82-92] (derived by comparison).
- **Apple's `cfmakeraw` is different** [libc: gen/FreeBSD/termios.c:173-189]:
  - it sets IGNBRK, which Docker clears;
  - it also clears IMAXBEL, IXOFF, INPCK and IGNPAR, and ECHOE, ECHOK, NOFLSH, TOSTOP and PENDIN;
  - it also sets CREAD.
  - So a macOS client that calls `cfmakeraw` does not reproduce Docker's raw mode (derived).
  - Rust's `libc::cfmakeraw` is only a declaration of the platform's C function [libc-rs: src/unix/mod.rs:2338-2342]. shards' `RawTerminal`, used for `vm run`'s serial console, calls it [shards: crates/shards/src/terminal.rs:32-45].
- **cfmakeraw is not POSIX.** It has no entry in the XSH function index [posix: XSH index]. POSIX specifies the flags and `tcsetattr()` [posix: XBD 11.2; XSH tcsetattr()].
- **How the mode is applied: "change immediately" on both OSes.**
  - Linux uses TCGETS/TCSETS; macOS and the BSDs use TIOCGETA/TIOCSETA [cli: term/termios_nonbsd.go:10-13; term/termios_bsd.go:10-13].
  - Linux's TCSETS sets termios with neither drain nor flush [linux: drivers/tty/tty_ioctl.c:798-803]. Apple's `tcsetattr(TCSANOW)` is TIOCSETA [libc: gen/FreeBSD/termios.c:71-73].
  - POSIX's TCSANOW means "the change shall occur immediately", and only TCSAFLUSH discards pending input [posix: XSH tcsetattr()]. So keys typed before raw mode is set still reach the container (inference).
- **Layouts and constants differ by OS:**
  - Linux's kernel `struct termios` is four 32-bit flag words, `c_line`, and `c_cc[19]` [linux: include/uapi/asm-generic/termbits.h:7-16].
  - XNU's flag words and speeds are `unsigned long` (8 bytes on LP64), and `c_cc` has 20 slots [xnu: bsd/sys/termios.h:108, 263-275]. Rust's `libc` mirrors that [libc-rs: src/unix/bsd/apple/mod.rs:26, 533-541].
  - Values differ too [linux: termbits.h:48, 127-128; xnu: bsd/sys/termios.h:102, 246-247]:
    - ICANON is 0x2 on Linux and 0x100 on XNU;
    - ISIG is 0x1 and 0x80;
    - the VMIN index is 6 and 16.
  - So flags must be taken per target, never hard-coded (inference).
- **Windows** [cli: term/termios_windows.go:5-35; term/term_windows.go:144-175]:
  - raw mode clears ENABLE_ECHO_INPUT, LINE_INPUT, MOUSE_INPUT, WINDOW_INPUT and PROCESSED_INPUT;
  - it sets EXTENDED_FLAGS, INSERT_MODE, QUICK_EDIT_MODE, and VIRTUAL_TERMINAL_INPUT when supported;
  - it restores the mode and exits 0 on an interrupt;
  - stdout gets DISABLE_NEWLINE_AUTO_RETURN.

**Restoring the terminal**

| Path | What happens | Source |
|---|---|---|
| The container's output ends | Restored at once, before the exit status is read | [cli: C/hijack.go:144-150] |
| Stdin ends or fails | Restored at once | [cli: C/hijack.go:171-177] |
| Detach | Restored, then `run` exits 0 | [cli: C/hijack.go:177-184; C/run.go:236-241] |
| Any other return from the streamer | A deferred restore, run once (`sync.OnceFunc`) | [cli: C/hijack.go:62-67, 108-112] |
| Start fails | The CLI cancels the streamer and waits for it, "to avoid the terminal are not restored" | [cli: C/run.go:206-213] |
| Third SIGINT/SIGTERM | Prints `\ngot 3 SIGTERM/SIGINTs, forcefully exiting\n`, restores stdin, stdout and stderr, exits 1 | [cli: cmd/docker/docker.go:443-472] |
| SIGKILL | Never: it cannot be caught | [posix: XSH 2.4.3; go: src/os/signal/doc.go:13-14] |
| SIGHUP or SIGQUIT with `--sig-proxy=false` | Go's default: exit, or exit with a stack dump; nothing restores (inference) | [go: src/os/signal/doc.go:38-45] |

- **Every catchable signal reaches the CLI.** With the default sig-proxy, `signal.Notify` with no signals relays all of them [cli: C/signals.go:60-64; go: src/os/signal/signal.go:106-108]. So in `docker run` only SIGKILL and SIGSTOP bypass it (derived).
- **Closing stdin.** On restore the CLI also closes stdin, except on darwin and Windows, where "this Close call blocks" [cli: C/hijack.go:211-230].

**Window size**

- **At create.**
  - The CLI sends `HostConfig.ConsoleSize` = its stdout's size, or 0×0 when stdout is not a terminal. It does so on every create, `-d` included [cli: C/create.go:327; S/out.go:64-68; S/stream.go:65-77].
  - dockerd puts the size in the OCI spec only with a TTY and a nonzero dimension [moby: daemon/oci_linux.go:1047-1049; daemon/oci_opts.go:12-26].
  - runc applies it to the pty before it hands the master out, and only if both dimensions are nonzero [runc: lc/init_linux.go:393-401; utils_linux.go:63-66].
  - So the command's first instruction already sees the right size (derived).
- **After start**, with a TTY and a terminal stdout, `MonitorTtySize` runs [cli: C/run.go:229-233]:
  - one resize at once;
  - if that fails, a goroutine retries up to 10 times, after 10, 20, … 100 ms (550 ms in all). If every retry fails it prints `failed to resize tty, using default size` on stderr [cli: C/tty.go:57-77] (derived: the sum).
  - The first attempt can fail because dockerd resizes only a running task (`GetRunningTask`) [moby: daemon/resize.go:21-26] (inference).
  - **Unix:** each SIGWINCH triggers a resize. Its channel holds one signal, and `signal.Notify` drops what does not fit [cli: C/tty.go:96-104; go: src/os/signal/signal.go:110-113], so bursts coalesce (inference).
  - **Windows:** the CLI polls every 250 ms [cli: C/tty.go:82-95].
  - A 0×0 size is never sent, and resize errors are logged only at debug level [cli: C/tty.go:27-30, 45-47].
- **Only rows and columns travel.**
  - The API takes `h` and `w` [moby: routes.go:1112-1127].
  - The shim builds its window size from width and height alone [ctrd: shim/runc/container.go:405-408].
  - So pixel sizes stay 0 (inference).
- **`docker attach`'s +1 trick.**
  - It first resizes to (h+1, w+1), then back. Its comment says this is "the only way to get the shell prompt to display for attaches 2+" [cli: C/attach.go:177-190].
  - Reason: the kernel sends SIGWINCH only on a change (§2.2).

**Detach keys**

- **Default:** ctrl-p ctrl-q, bytes 16 and 17, in both the CLI and dockerd [cli: C/hijack.go:17-19; moby: stream/attach.go:14].
- **Source:** `--detach-keys`, else the config file's `detachKeys` [cli: C/run.go:143-146; cli/config/configfile/file.go:61; D/docker.md:331-349].
- **Syntax** (`term.ToBytes`) [cli: term/ascii.go:9-66]:
  - comma-separated items;
  - a one-byte item stands for that byte;
  - otherwise one of `ctrl-@`, `ctrl-a`…`ctrl-z`, `ctrl-[`, `ctrl-\`, `ctrl-]`, `ctrl-^`, `ctrl-_` (bytes 0–31), or `DEL` (127);
  - anything else fails with `Unknown character: '<item>'`. The match is case-sensitive, so `ctrl-A` is unknown (derived).
- **Validation.**
  - The flag is checked before create: `invalid detach keys (<keys>): Unknown character: '<item>'`. That is a plain error, so exit 1 [cli: C/run.go:147-149; C/hijack.go:33-41; C/errors.go:5-10].
  - The config file's value is not checked there, because only `runOpts.detachKeys` is [cli: C/run.go:147].
  - dockerd rejects a bad value at attach, after create, with `Invalid detach keys (<keys>) provided` [moby: daemon/attach.go:26-30] (inference for the ordering).
- **Detection needs `-i` and `-t`.**
  - The escape proxy wraps stdin only in raw mode's branch [cli: C/hijack.go:95-132].
  - dockerd scans stdin only for TTY containers [moby: stream/attach.go:84-88].
  - The reference: "If the container was run with `-i` and `-t`, you can detach" [cli: D/container_attach.md:36-39].
- **The proxy holds back a partial match.**
  - A ctrl-p is passed on only when the next byte shows it is not the sequence [cli: term/proxy.go:51-87]. So a lone ctrl-p (shell history) reaches the container only with the next key (inference).
  - On the full sequence the proxy returns `EscapeError` ("read escape sequence"). The sequence's bytes are never sent [cli: term/proxy.go:7-13, 55-62].
- **What each command prints on detach.**
  - `docker run` treats `EscapeError` as success: nothing printed, exit 0, and the container keeps running [cli: C/run.go:236-241].
  - `docker attach` returns the error, so it prints `read escape sequence` and exits 1 (inference from [cli: C/attach.go:153-156; cmd/docker/docker.go:49-53]). The reference shows that line after a detach [cli: D/container_attach.md:101-114].
- **A TTY container keeps its stdin.** When the client's input ends in TTY mode, dockerd closes that attach's output pipes but not the container's stdin [moby: stream/attach.go:69-81].

**Streams**

- **In the CLI.** With a TTY, the CLI sends the container's stderr to its own stdout [cli: C/run.go:285-291] and copies the stream raw. Without one, it demultiplexes with stdcopy [cli: C/hijack.go:144-153].
- **In dockerd.** It multiplexes only without a TTY. With one, the content type is `application/vnd.docker.raw-stream` [moby: daemon/attach.go:69-80; routes.go:1139-1159; api/types/types.go:11-20].
- **In the shim.** It gets no stderr FIFO when there is a terminal. It gets a stdin FIFO whenever there is a TTY, even without `-i` [moby: lcd/client_linux.go:92-117; daemon/start.go:233-235].

**Exit status**

- As without a TTY: the command's code, or 128 + N for a signal; 125, 126 or 127 for create and start errors (container-lifecycle-cli.md §2.3, §2.10).
- Otherwise: detach 0, refusal 1, forced exit 1 (above).

### 2.2 Q2: the engine

**The spec**

- `Process.Terminal` = `Config.Tty` [moby: daemon/oci_linux.go:748-749].
- The environment is `PATH`, `HOSTNAME`, then `TERM=xterm` with a TTY, then links. The image's and the user's variables then replace or append [moby: dc/container.go:814-831].

**Allocation: inside the container, by runc**

- **Hand-off channel.** The shim makes a temporary console socket (`pty.sock` in a new temporary directory) and passes it to `runc create` [ctrd: shim/process/init.go:118-122, 145-147; go-runc/console.go:51-80].
- **Order in runc's init.** Once the rootfs is mounted and before it is finalized, init runs `setupConsole`, then `ioctl(0, TIOCSCTTY, 0)` [runc: lc/standard_init_linux.go:97-107; lc/system/linux.go:66-71].
- **`setupConsole`** [runc: lc/init_linux.go:370-417; lc/console_linux.go:18-43, 96-166]:
  1. opens `/dev/pts/ptmx` with `O_PATH|O_NOFOLLOW` and checks that it is inode 2 of a devpts, char 5:2, then reopens it `O_RDWR|O_NOCTTY`: a new master;
  2. reads the peer's number (TIOCGPTN) and unlocks it (TIOCSPTLCK 0) [ctrd: console/console_unix.go:42-55; console/tc_linux.go:31-50];
  3. opens the peer with TIOCGPTPEER (`O_RDWR|O_NOCTTY|O_CLOEXEC`). Kernels before 4.13 fall back to the path, after checking it is char 136:N on devpts [runc: lc/console_linux.go:51-94; internal/linux/linux.go:121-138];
  4. sets ConsoleSize if both dimensions are nonzero;
  5. bind-mounts the peer over `/dev/console`, created 0666 if missing;
  6. sends the master to the shim by `SCM_RIGHTS`, with its name;
  7. dups the peer onto fds 0, 1 and 2.
- **The shim receives the master** and starts copying [ctrd: go-runc/console.go:137-153; shim/process/init.go:159-168].
- **Session.**
  - runc's nsexec calls `setsid()` in stage 2, before init's Go code runs [runc: lc/nsenter/nsexec.c:1195-1196].
  - TIOCSCTTY then makes the pty the controlling terminal, and the caller's process group its foreground group [linux: drivers/tty/tty_jobctrl.c:98-120, 365-412].
  - So the container's first process is a session leader in the foreground (derived).
  - TIOCSCTTY is not POSIX: POSIX leaves how a session gets its controlling terminal implementation-defined, except that `O_NOCTTY` prevents it [posix: XBD 11.1.3]. Linux and XNU both define the ioctl [linux: include/uapi/asm-generic/ioctls.h:33; xnu: bsd/sys/ttycom.h:160].
- **Ownership.** `fixStdioPermissions` chowns stdio, the pty's peer here, to the container user [runc: lc/init_linux.go:518-555].
- **Nothing on Docker's path touches termios.**
  - runc clears ONLCR only in its own relay, used when runc runs a terminal in the foreground; a detached caller "will handle receiving the console master" itself [runc: utils_linux.go:99-136; tty.go:102-114]. The shim path does not clear it.
  - containerd v2.3.5 calls `SetRaw` only in `ctr`'s client commands and never clears ONLCR; moby's daemon calls neither (derived by grep).
  - So a container's pty keeps the kernel's defaults (derived).

**devpts, `/dev/ptmx` and `/dev/console`**

- **Docker's mounts.** Docker mounts devpts at `/dev/pts` with `nosuid,noexec,newinstance,ptmxmode=0666,mode=0620,gid=5`, over a `/dev` tmpfs [moby: daemon/pkg/oci/defaults.go:71-82].
- **The kernel's handling of those options:**
  - `newinstance` is parsed and ignored [linux: fs/devpts/inode.c:247-248];
  - every mount is its own instance: `get_tree_nodev` passes no test for sharing a superblock [ibid.:413-415; linux: fs/super.c:1325-1358];
  - `ptmxmode` defaults to 0000, leaving `/dev/pts/ptmx` unusable unless set; Docker passes 0666 [ibid.:29-36, 244-246].
- **runc's `/dev/ptmx`** is a symlink to `pts/ptmx` [runc: lc/rootfs_linux.go:100-106, 1129-1134].
- **A ptmx outside devpts works too.** Opening one, such as a devtmpfs node, uses the devpts mounted at `pts` in the same directory [linux: fs/devpts/inode.c:113-130, 132-179, 181-211]. devtmpfs creates `/dev/tty` and `/dev/ptmx` with mode 0666 [linux: drivers/tty/tty_io.c:3511-3519].
- **Device rules.** moby allows c 5:0 (`/dev/tty`) and c 5:1 (`/dev/console`) [moby: daemon/pkg/oci/defaults.go:164-177]. runc adds c 136:* (the pts) and c 5:2 (ptmx) [runc: lc/specconv/spec_linux.go:295-352].
- **Kernel config.** devpts is built only with `CONFIG_UNIX98_PTYS` [linux: fs/devpts/Makefile:6-8; drivers/tty/Kconfig:94-116].

**Initial termios and window size**

- **Fresh termios per pty.** Unix98 ptys reset termios at every allocation (`TTY_DRIVER_RESET_TERMIOS`) [linux: drivers/tty/pty.c:875-890].
- **The container's side** (the slave) gets `tty_std_termios`, with `c_cflag` = B38400, CS8, CREAD [linux: pty.c:914-917; tty_io.c:124-134]:
  - `c_iflag` ICRNL, IXON;
  - `c_oflag` OPOST, ONLCR;
  - `c_lflag` ISIG, ICANON, ECHO, ECHOE, ECHOK, ECHOCTL, ECHOKE, IEXTEN.
- **Control characters** [linux: include/linux/termios_internal.h:1-35]:
  - intr ^C, quit ^\, erase DEL, kill ^U, eof ^D;
  - start ^Q, stop ^S, susp ^Z;
  - reprint ^R, discard ^O, werase ^W, lnext ^V;
  - VMIN 1.
- **The master's side** has `c_iflag`, `c_oflag` and `c_lflag` all zero: raw [linux: pty.c:898-904].
- **The window size** starts at 0×0: the tty is `kzalloc`ed [linux: tty_io.c:3098-3102] (inference: the size is a member of that struct). It stays 0×0 unless ConsoleSize or a resize sets it.
- **So** the container echoes its own input, turns `\n` into `\r\n`, and turns ^C, ^\ and ^Z into signals. The user's raw terminal does none of these (inference).

**The resize path**

1. The CLI sends `POST /containers/{id}/resize?h=&w=` [cli: C/tty.go:26-49; moby: routes.go:1112-1127].
2. `ContainerResize` finds the running task, calls `tsk.Resize(w, h)`, and logs a `resize` event [moby: daemon/resize.go:13-35].
3. The containerd client calls `ResizePty` [ctrd: client/task.go:530-546].
4. The shim's `ResizePty` calls `Init.Resize`, whose console `Resize` is TIOCSWINSZ on the master. Width and height are cut to uint16 [ctrd: shim/task/service.go:409-418; shim/runc/container.go:400-410; shim/process/init.go:332-341; console/tc_unix.go:55-65].
5. In the kernel, TIOCSWINSZ on a master applies to the slave (`tty_pair_get_tty`) [linux: tty_io.c:2657-2663, 2707-2710]. The Unix98 slave has no resize operation, so `tty_do_resize` runs [linux: pty.c:762-776; tty_io.c:2324-2342, 2359-2370]:
   - an unchanged size does nothing;
   - otherwise the kernel sends SIGWINCH to the slave's foreground process group, then stores the size.

- **POSIX agrees.** `tcsetwinsize()` delivers SIGWINCH to the foreground process group, and none when the size is unchanged [posix: XSH tcsetwinsize()].
  - SIGWINCH, `struct winsize`, `tcgetwinsize()` and `tcsetwinsize()` are new in POSIX.1-2024 (Austin Group Defects 1151 and 1484) [posix: XBD <signal.h>, <termios.h>, CHANGE HISTORY].
- **XNU** likewise signals only on a change [xnu: bsd/kern/tty.c:1594-1600].

**The end of the stream**

- **In the kernel.**
  - When the slave's last user closes, the master is flagged TTY_OTHER_CLOSED [linux: pty.c:47-81]. "Last" counts the extra reference each side gets at install (inference from [linux: pty.c:54-55, 419-420]).
  - A master read first flushes pending buffer work and returns what remains. Only once nothing remains does it fail with EIO [linux: n_tty.c:2148-2152, 2259-2272].
  - Poll reports EPOLLHUP as soon as the other side is closed, possibly while data is still readable [linux: n_tty.c:2445-2470].
  - So a reader must read until EIO, not stop at POLLHUP (inference).
- **XNU** returns 0 (end of file) instead, once the slave has gone and the queue is empty [xnu: bsd/kern/tty_dev.c:680-686].
- **containerd.**
  - The console is registered in epoll edge-triggered [ctrd: console/console_linux.go:81-105].
  - Its `Read` treats EAGAIN and EIO as "the other side went away" and waits. Once the console has been shut down and nothing was read, it returns (0, EIO), which ends the copy [ctrd: console/console_linux.go:172-210].
  - The shim shuts the console down when the init process exits, then sends `TaskExit` [ctrd: shim/process/init.go:285-290; shim/runc/platform.go:196-205; console/console_linux.go:250-267; shim/task/service.go:767-784].
  - On delete, the shim waits up to 10 s for its copies [ctrd: shim/process/init.go:300-303].
- **The copies.**
  - Output goes to the stdout FIFO 4 KiB at a time [ctrd: shim/runc/platform.go:38-45, 169-191].
  - End of file on the stdin FIFO shuts the console down [ctrd: shim/runc/platform.go:74-90]. dockerd therefore never closes a TTY container's stdin FIFO [moby: dc/container.go:752-758; stream/attach.go:69-81] (inference for the reason).
- **dockerd and the CLI.** dockerd deletes the task, then waits up to 2 s for its copiers [moby: daemon/monitor.go:65-87; stream/streams.go:170-185]. The CLI keeps copying until the stream ends [cli: C/run.go:249-259].
- **SIGHUP at the end.** When the session leader exits, the kernel sends SIGHUP to the pty's foreground group [linux: tty_jobctrl.c:265-285; posix: XSH _exit()].

### 2.3 Q3: signals with a TTY

**sig-proxy stays on with `-t`**

- In `docker run`, only `--sig-proxy=false` turns it off [cli: C/run.go:155-164].
- Before docker/cli ee295049231c (2019-04-18, first tag v20.10.0), TTY mode turned it off. The commit's example is a CLI that was killed while the container kept running, because the signal never reached it [cli-git: ee295049231c].
- `docker attach` still turns it off for TTY containers [cli: C/attach.go:109-118].
- It forwards every signal but SIGCHLD, SIGPIPE, SIGURG and those without a name, by name, through the kill API [cli: C/signals.go:13-64; C/signals_unix.go:11-13].

**Ctrl-C**

- **With `-it`,** raw mode clears ISIG on the user's terminal, so ^C travels as byte 0x03 [cli: term/termios_unix.go:25].
  - The container's pty has ISIG, and sends SIGINT to its foreground group [linux: n_tty.c:1051-1058].
  - Without NOFLSH it also discards the pty's pending input, and output still queued toward the master [linux: n_tty.c:1074-1107; pty.c:204-218].
- **With `-t` alone,** the terminal stays cooked: ^C is SIGINT to the CLI, in the terminal's foreground group, which forwards it by kill (inference).
- **PID 1 ignores default-action signals.**
  - The container's first process is its PID namespace's init. The kernel drops a signal whose action is the default for such a process, except SIGKILL and SIGSTOP from outside the namespace [linux: kernel/signal.c:84-96, 1183-1194, 2958-2970].
  - The reference says so: PID 1 "ignores any signal with the default action" [cli: D/container_run.md:1005-1008; D/container_attach.md:41-44].
  - So `docker run -it IMAGE sleep 1000` ignores ^C, whether SIGINT comes from the pty or from sig-proxy (inference).
- **With `--init`,** tini puts the command in a new process group, makes that group the terminal's foreground, and ignores SIGTTIN and SIGTTOU itself [tini: src/tini.c:150-178, 478-495]. So ^C reaches the command, which is not PID 1 (inference).
- **The CLI's own Ctrl-C.**
  - SIGINT and SIGTERM also cancel the CLI's base context, and the third one ends the CLI, exit 1, after restoring the terminal [cli: cmd/docker/docker.go:57-95, 443-472; cmd/docker/internal/signals/signals_unix.go:11-13].
  - So with `-t` alone, three ^C end the CLI while a PID-1 command keeps running (inference).

**SIGWINCH reaches the container twice**

- With `-t`, one resize of the user's terminal produces two SIGWINCH (inference from [cli: C/tty.go:96-104; C/signals.go:31-56]):
  - `MonitorTtySize` resizes the pty, and the kernel signals the foreground group, if the size changed;
  - sig-proxy forwards the CLI's own SIGWINCH to the main process, by kill.
- Without `-t`, only the forwarded one arrives (inference).

**Job control inside the container**

- **^Z.** With `-it`, ^Z makes the pty send SIGTSTP to its foreground group [linux: n_tty.c:1051-1058; include/linux/termios_internal.h:29].
  - A shell as PID 1 is a session leader with the pty as its controlling terminal (§2.2), so it can run jobs (inference).
  - A PID 1 that leaves SIGTSTP at its default action ignores it (see above).
- **Background jobs.** Reading the terminal gets SIGTTIN; writing with TOSTOP set gets SIGTTOU [posix: XBD 11.1.4; linux: tty_jobctrl.c:33-67].
- **The CLI's own job control.**
  - With sig-proxy on, `Notify` for all signals turns off SIGTSTP's, SIGTTIN's and SIGTTOU's default stop [go: src/os/signal/doc.go:63-74]. So the CLI forwards them rather than stopping (inference).
  - Setting termios from a background process group raises SIGTTOU [posix: XSH tcsetattr(); linux: tty_ioctl.c:444-448; xnu: bsd/kern/tty.c:1074-1120].
  - What `docker run -it … &` then does: UNVERIFIED (E5).

### 2.4 Q4: `docker logs` for a TTY container (resolves container-lifecycle-cli.md E4)

**Yes: `\r\n` line ends, one stream labeled stdout, and the echoed input.**

| Step | Bytes | Source |
|---|---|---|
| `printf 'a\nb\n'` writes to the slave | `a\nb\n` | |
| n_tty output processing, OPOST with ONLCR | `a\r\nb\r\n` on the master | [linux: tty_io.c:124-126; pty.c:914-917] |
| The shim copies the master into the stdout FIFO | unchanged | [ctrd: shim/runc/platform.go:169-191] |
| dockerd's copier splits on `\n`, keeping the `\r` | lines `a\r`, `b\r`, source `stdout` | [moby: dc/container.go:715; daemon/logger/copier.go:99-131] |
| json-file appends `\n` | `a\r\n`, `b\r\n` | [moby: daemon/logger/jsonfilelog/jsonfilelog.go:127-143] |
| The logs API: a raw stream for a TTY | unchanged | [moby: routes.go:254-275; daemon/logs.go:145] |
| The CLI copies it raw to stdout | `a\r\nb\r\n` | [cli: C/logs.go:77-81] |

- **Nothing on this path changes the pty's termios** (§2.2).
- **Everything is stdout.** There is no stderr FIFO with a terminal [moby: lcd/client_linux.go:103-106]. So `docker logs … 2>/dev/null` still shows what the command wrote to fd 2 (inference).
- **Echoes are logged.** With `-it`, the pty echoes typed characters to the master (ECHO, with ECHOCTL showing ^C as `^C`), so they are part of the output and are logged (inference from [linux: tty_io.c:128-129]).
- **`docker run -t` itself** writes the same `\r\n` to its stdout: `docker run -t alpine echo hi | od -c` should show `h i \r \n` (inference; E3).

### 2.5 Where the reference pages disagree with the code

- **Detach key syntax.**
  - The pages allow a letter, or `ctrl-` with a–z, `@`, `[`, `\\`, `_`, `^` [cli: D/container_run.md:855-864; D/container_attach.md:152-161; D/docker.md:336-344].
  - The code also takes any single byte, `ctrl-]` and `DEL` [cli: term/ascii.go:9-66].
- **Ctrl-C and SIGKILL.**
  - `docker attach`'s page: "use `CTRL-c`. This key sequence sends `SIGKILL` to the container" [cli: D/container_attach.md:36-37].
  - The code has no such path. It forwards the signal the CLI received, by its own name [cli: C/attach.go:109-118; C/signals.go:40-53] (inference).
- **`read escape sequence`.** The page's detach example prints it [cli: D/container_attach.md:101-114]. That holds for `attach`; `run` prints nothing (§2.1).

## 3. Implications for shards (ranked)

Ranked by how often a user would see the difference. The design sketches marked (inference) are options, not decisions.

1. **A real pty in the guest, allocated by shards-init with runc's steps.**
   - *What* (inference, following [runc: lc/init_linux.go:370-417; lc/console_linux.go:96-166]):
     - with `-t`, init opens `/dev/ptmx` (the devtmpfs node) or `/dev/pts/ptmx` with `O_RDWR|O_NOCTTY|O_CLOEXEC`;
     - it unlocks the pty (TIOCSPTLCK 0), opens the peer with TIOCGPTPEER, and sets the requested size;
     - it chowns the peer to the command's uid, as it already chowns its pipes [shards: crates/init/src/run.rs:491-497];
     - the child already calls `setsid()` [shards: run.rs:988]. It then dup2s the peer onto 0–2 and calls `ioctl(0, TIOCSCTTY, 0)` before it drops privileges.
   - *Nothing to mount, nothing needed from the image:*
     - init already mounts devtmpfs on `/dev` and devpts with Docker's options [shards: run.rs:199-231];
     - images need no `/dev` [shards: architecture.md:244-246];
     - the kernel has `UNIX98_PTYS=y` [shards: resources/kernel/firecracker-x86_64-6.18.config:2097-2104; resources/kernel/firecracker-aarch64-6.18.config:2137-2144];
     - the devtmpfs ptmx finds the devpts at `/dev/pts` [linux: fs/devpts/inode.c:113-130].
   - *Keep the kernel's termios.* Clearing ONLCR or setting raw mode would lose Docker's `\r\n` and echo (§2.2, §2.4).
   - *`/dev/console`.* Bind-mount the peer over it, as runc does. Today `/dev/console` in the guest is the VM's console (inference: devtmpfs).
   - *Draining.* Init must close its own copy of the peer, then read the master until EIO, not until POLLHUP (§2.2). The relay already treats any read error except EAGAIN as the end of a stream [shards: run.rs:826-838].
   - *The standby is the hard part.* It is forked before the snapshot, with pipes as its stdio [shards: run.rs:387-435], and a pty cannot travel over its orders pipe. Options (inference):
     - allocate at request time, and have the standby open `/dev/pts/N`, which is safe in a VM with no foreign mounts;
     - pass the peer over a socketpair;
     - pre-allocate a pty in every template (E2).
   - *Why first:* everything else rides on it.
2. **The protocol: one stream, a size, a resize, and a stdin that stays open.**
   - *Guest side* (inference) [shards: crates/abi/src/run.rs:21-42, 56-68]:
     - `Spec` gains `tty` and the initial rows and columns;
     - with `-t`, the guest sends only `STDOUT` frames;
     - a new `RESIZE` frame (host to guest, two u16) travels on the signal connection, apart from stdin, as Docker's resize is a separate endpoint.
   - *Host side* (inference) [shards: crates/ipc/src/lib.rs:36-77, 149-173]:
     - the IPC `Run` gains `tty` and the size;
     - a client-to-VM resize message joins `SIGNAL` on the client's connection.
   - *Stdin.* With `-t`, the command's stdin must not end when the client does. dockerd keeps a TTY container's stdin open [moby: stream/attach.go:69-81]. D26's StdinOnce rule for `-i` must not apply [shards: architecture.md:639-642].
   - *`TERM`.* `TERM=xterm` goes after `HOSTNAME`, overridable by `-e` [moby: dc/container.go:818-831; shards: crates/shards/src/workload.rs:79].
3. **The thin client's terminal: Docker's checks, flags and restores.**
   - *Refusal:* `cannot attach stdin to a TTY-enabled container because stdin is not a terminal`, exit 1, before anything is created; skipped with `-d` (§2.1).
   - *Raw mode:*
     - on stdin, only with `-i` and `-t`;
     - moby/term's flag set on every OS, not `libc::cfmakeraw`, which differs on macOS;
     - applied TCSANOW, and skipped when `NORAW` is set (§2.1).
     - libc is already available to the client through shards_ipc [shards: crates/ipc/Cargo.toml; crates/shards/src/bin/shards/client.rs:37-38].
   - *Restore on every path:*
     - on `EXIT`, which the warm VM sends after it has let go of the client's stdio, so all output is written first [shards: crates/shards/src/warm.rs:189-231];
     - on detach and on errors;
     - before the forwarder re-raises a terminating signal. Today it raises with the terminal still raw [shards: client.rs:318-329].
     - Decide whether to copy Docker's forced exit on the third SIGINT/SIGTERM [cli: cmd/docker/docker.go:443-472].
   - *Windows* has no vsock path yet [shards: workload.rs:6-7]. When it does, Docker's console flags and 250 ms size polling apply (§2.1).
4. **Window size and resize.**
   - Put the client's stdout size (TIOCGWINSZ on fd 1) in the request, `-d` included, and apply it in the guest before exec, as runc does (§2.1).
   - The client already forwards SIGWINCH as a signal [shards: crates/ipc/src/unix.rs:31-50]. With `-t` it must also send a resize: Docker does both (§2.3).
   - A resize with the same size is invisible, since the kernel signals only on a change (§2.2). So the post-start resize that Docker retries for up to 550 ms is harmless to copy (inference).
5. **Detach keys in the client's stdin thread.**
   - *Where:* the client already reads its terminal itself, into the command's stdin pipe [shards: client.rs:258-287]. Add moby/term's `ToBytes` and the escape proxy there, holding back partial matches exactly as the proxy does (§2.1).
   - *On a match:* restore the terminal and exit 0, printing nothing. The VM lets go of the client's stdio when the client hangs up, and the command runs on [shards: warm.rs:170-182].
   - *Where the keys come from:* shards has no config file. Whether to read `detachKeys` from Docker's is open (inference).
6. **Ctrl-C and PID 1: document the difference, or add a PID namespace.**
   - shards-init is every command's PID 1, as with `--init` [shards: architecture.md:717-719]. So ^C ends `sleep`, where Docker's PID-1 `sleep` ignores it (§2.3), as it already does for forwarded signals without a TTY.
   - This matches `docker run --init -it` (tini).
   - The alternative is to make the command init of a new PID namespace, which the kernel has (`CONFIG_PID_NS=y` [shards: resources/kernel/firecracker-x86_64-6.18.config:227-232; resources/kernel/firecracker-aarch64-6.18.config:204-209]). That buys `SIGNAL_UNKILLABLE` semantics, at a cost to measure (E2).
7. **Logs need no change.**
   - Records keep the bytes of `STDOUT` frames as they come [shards: workload.rs:335-345, 368-386].
   - With a guest pty, the `\r\n`, the echoes and the single stdout stream follow (§2.4).
8. **After `kill -9` of the client, Docker leaves the terminal raw.**
   - *Docker:* nothing can catch SIGKILL (§2.1). Users then need `stty sane` or `reset` (inference).
   - *shards could do better* (inference): the warm VM holds the client's terminal as its own stdout [shards: warm.rs:123-136], and could restore termios the client sent it when the client's connection drops before `EXIT`.
   - *The risk:* a race with the shell's own settings (E6).
9. **Latency: a keystroke's echo crosses the VM twice.**
   - *The path* (inference):
     1. the client reads the key, and the warm VM's stdin thread frames it [shards: client.rs:258-287; workload.rs:395-411];
     2. vsock carries it into the guest, and init writes it to the master [shards: run.rs:686-849];
     3. a kworker delivers it to the slave, where n_tty echoes it, and a second kworker delivers the echo back to the master [linux: drivers/tty/tty_buffer.c:551-566; pty.c:111-119];
     4. init frames the echo, vsock carries it out, and the VMM writes it to the terminal [shards: workload.rs:335-340].
   - *What we know:* interrupt injection into an idle vCPU alone costs 8.9–19.5 µs at p50 and 61.5–315 µs at p99, over two runs at default QoS [PM M8]. Each exit costs about 0.7–0.8 µs [PM M4].
   - *Docker's path* has three user processes each way, the CLI, dockerd and the shim, plus the same two kworker hops (inference).
   - No primary source gives either total (E1).
10. **Tests need a host pty.**
    - `tests/daemon.rs` already makes one with `openpty`, `setsid` and TIOCSCTTY [shards: crates/shards/tests/daemon.rs:398-405].
    - A master read that ends returns 0 on macOS and EIO on Linux [xnu: bsd/kern/tty_dev.c:680-686; linux: n_tty.c:2148-2152].

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| Docker parity vs D27's `--init` semantics | A PID-1 command ignores ^C in Docker [linux: kernel/signal.c:84-96; cli: D/container_run.md:1005-1008]; shards' command is never PID 1 [shards: architecture.md:717-719] |
| A per-run pty vs a standby forked before the snapshot | The standby's stdio is pipes made before any request [shards: run.rs:387-435]; runc allocates the pty per container [runc: lc/init_linux.go:370-417] |
| D26's StdinOnce for `-i` vs TTY semantics | dockerd closes stdin at the client's end only without a TTY [moby: stream/attach.go:69-81]; shards ends it with the client [shards: architecture.md:639-642] |
| Docker parity vs a better terminal after SIGKILL | Docker cannot restore (§2.1); the warm VM could (§3 item 8) |
| One codebase vs per-OS termios | Flag values, field widths and `cfmakeraw` differ between Linux and macOS (§2.1) |

## 4. Open questions needing our own measurement

- **E1. Keystroke echo latency.**
  - Time from a byte written to a host pty master to its echo read back:
    - `shards run -it` on macOS/HVF and on Linux/KVM;
    - `docker run -it` on Linux.
  - Idle and busy host; report n, p50, p90, p99 and max, with host, OS and revision.
  - Split the time into client, VMM, IRQ, guest kworker and init.
- **E2. Where the pty and its session come from.**
  - Cost on the request path of ptmx open, TIOCGPTPEER, TIOCSWINSZ and TIOCSCTTY in a restored guest.
  - Against a pty pre-allocated before the snapshot; check that a restored copy's pre-allocated pty works.
  - Cost of `CLONE_NEWPID` for the command (§3 item 6).
- **E3. A differential TTY transcript against dockerd v29.8.1**, extending container-lifecycle-cli.md E1:
  - byte-exact output of `run -t` (`\r\n`), `logs` (echo included) and `run -t … | od -c`;
  - the refusal text and status; detach under `run` (0) and `attach` (`read escape sequence`, 1);
  - how many SIGWINCH one resize delivers;
  - `-t` alone blocking on stdin;
  - `-dt` taking the creating terminal's size, and a stdout that is not a terminal giving 0×0;
  - a config-file `detachKeys` that is invalid.
- **E4. Output throughput through the guest pty:** 32 MiB with `-t` against without, as D16's test reads 32 MiB [shards: architecture.md:244-246]. Each pty write queues kworker work [linux: tty_buffer.c:551-566].
- **E5. A backgrounded `docker run -it … &`:** setting raw mode meets SIGTTOU while sig-proxy catches it (§2.3). What does the CLI do, and what should shards do?
- **E6. The terminal after `kill -9`:**
  - confirm that Docker leaves it raw;
  - test a warm-VM-side restore against bash and zsh prompts.
- **E7. Raw mode flags on macOS:** dump termios after Docker's raw mode and after Apple's `cfmakeraw`, and find any visible difference, IGNBRK and IMAXBEL for instance (§2.1).
- **E8. Resize storms:** how many resizes a drag produces through the client, and the time from host SIGWINCH to guest SIGWINCH.

## 5. References

**Source code** (paths and lines cited inline)

- docker/cli v29.8.1 (`4a63305d7433`):
  - its vendor tree has github.com/moby/term v0.5.2 and golang.org/x/sys v0.48.0;
  - history from a blobless clone of github.com/docker/cli: `ee295049231c` "Do not disable sig-proxy when using a TTY" (2019-04-18, first tag v20.10.0) and `526dfffc26f3` "cli/streams: simplify CheckTty" (2026-04-01, first tag v29.4.0).
- moby/moby docker-v29.8.1 (`464cd50c3d9e`), including `Dockerfile`, `go.mod` and `api/types/types.go`.
- containerd v2.3.5 (`1294c24a7da8`), with vendored containerd/console v1.0.5 and containerd/go-runc v1.1.0.
- opencontainers/runc v1.5.1 (`8f2685a471d3`).
- krallin/tini v0.19.0, `src/tini.c`, read raw from GitHub.
- Linux 6.18.48 from git.kernel.org's stable tree:
  - `drivers/tty/{pty,tty_io,n_tty,tty_ioctl,tty_jobctrl,tty_buffer}.c` and `drivers/tty/Kconfig`;
  - `fs/devpts/{inode.c,Makefile}`, `fs/super.c`, `kernel/signal.c`;
  - `include/linux/{tty,termios_internal}.h`;
  - `include/uapi/asm-generic/{termbits,termbits-common,termios,ioctls}.h`.
  - Compared with v6.18 from github.com/torvalds/linux.
- apple-oss-distributions/xnu `xnu-12377.121.6`: `bsd/sys/{termios,ttycom}.h`, `bsd/kern/{tty,tty_dev}.c`.
- apple-oss-distributions/Libc `Libc-1752.120.2`: `gen/FreeBSD/termios.c`.
- glibc 2.42 `termios/cfmakeraw.c` (sourceware.org); musl v1.2.5 `src/termios/cfmakeraw.c` (git.musl-libc.org).
- Rust `libc` 0.2.189: `src/unix/mod.rs`, `src/unix/bsd/apple/mod.rs`, from the local cargo registry.
- Go 1.26.1: `src/os/signal/doc.go`, `src/os/signal/signal.go`, from the local toolchain.

**Specifications**

- POSIX.1-2024 (IEEE Std 1003.1-2024, The Open Group Base Specifications Issue 8):
  - XBD chapter 11 (General Terminal Interface), `<termios.h>`, `<signal.h>`;
  - XSH 2.4.3 (Signal Actions), `tcsetattr()`, `tcsetwinsize()`, `setsid()`, `_exit()`, and the XSH function index.

**Official documentation**

- docker/cli v29.8.1 `docs/reference/commandline/`: `container_run.md`, `container_attach.md` and `docker.md`, the source of the CLI reference on docs.docker.com.

**Our notes:** container-lifecycle-cli.md (E1, E4); warm-pool-daemon.md §2.7; D16, D26, D27 (architecture.md); PM M4, M8 (platform-measurements.md).
