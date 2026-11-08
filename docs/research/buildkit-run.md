# What a Dockerfile `RUN` step does, in BuildKit and runc

The behavior shards' builder guests reproduce (architecture.md D34), read from source:
BuildKit at `dockerfile/1.27.1-labs` (08f4630c6), and runc v1.4.3, which BuildKit pins
(`Dockerfile:3`) and which sets up the process inside the container. Paths are into those
trees. Where shards does otherwise, D34 says so and why.

## 1. The process

- **Arguments** (`frontend/dockerfile/dockerfile2llb/convert.go:1346-1508`). Shell form:
  the words joined with spaces, after the image's `SHELL` or `["/bin/sh","-c"]`
  (`convert.go:1914-1922`, `defaultshell.go:3-8`); never variable-expanded, only
  `--mount` values are (`instructions/commands.go:372-377`). Exec form: verbatim; an empty
  array fails with `arguments are required` (`client/llb/exec.go:113-115`).
- **Heredocs** (`convert.go:1355-1405`, `parser/parser.go:121,371-504`): content keeps each
  line's `\n`, not the terminator; `<<-` strips leading tabs. A command that is one heredoc
  starting `#!` is written as file `<name>` mode 0755 in a scratch state, bind-mounted
  read-only at `/dev/pipes/`, and run as `["/bin/sh","-c","/dev/pipes/<name>"]`. One
  heredoc without `#!`: its content is the shell's command. Otherwise the command becomes
  `args[0]` followed by `"\n" + data + name` for each heredoc, for the shell to read.
- **Environment**, in this order: the stage's (image `Env`, a `PATH` default
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` if the image has none,
  which also lands in the image config, `ENV`s, and every `ARG` the stage declared with a
  value, each key listed once at its latest set, `client/llb/meta.go:420-463`); then
  `SSH_AUTH_SOCK` (the first ssh mount's target) if unset; proxy variables from every
  build arg named `http_proxy`, `https_proxy`, `ftp_proxy`, `no_proxy`, `all_proxy` in any
  case, each emitted upper then lower, not in the cache key (`convert.go:1876-1908`,
  `solver/llbsolver/ops/exec.go:132,489-491,576-594`); the `PATH` default again if absent;
  secret env `NAME=<bytes>` (`ops/exec.go:501-505,609-635`). runc then keeps each key's
  last value, drops entries without `=`, and sets `HOME` from the user's `/etc/passwd`
  entry (`/` without one) when it is unset or empty (runc `libcontainer/env.go`). BuildKit
  sets no `HOME`, `TERM`, `HOSTNAME` or `SOURCE_DATE_EPOCH`.
- **Working directory**: the stage's (`/` by default). If it is missing when the step
  runs, it is made with parents, each new one mode 0755 owned by the step's resolved
  uid:gid, and stays in the layer (`executor/runcexecutor/executor.go:314-341`,
  `moby/sys/user/idtools_unix.go:13-68`). `WORKDIR` itself makes its directory with a
  file operation as the image's user.
- **User** (`executor/oci/user.go:22-157`, `moby/sys/user/user.go:245-413`): empty is
  `0:0`. `a:b` with both parts decimal (or `root`) is taken as is, files unread, no
  supplementary groups. Otherwise `/etc/passwd` and `/etc/group` of the rootfs (regular
  files, at most 10 MiB) are read: a numeric user matches by uid first and, unmatched, is
  `uid=N gid=0`; an unknown name fails `unable to find user X: no matching entries in
  passwd file`. An explicit group matches by number or name (an unknown name fails
  `unable to find group G: no matching entries in group file`) and gives no supplementary
  groups; without one, the user's groups are every group listing the user's name. The
  primary gid is always among the additional gids.
- **Limits and privileges**: umask 0022 (runc `rootfs_linux.go:298-301`); rlimits
  inherited unless `--ulimit`; `no_new_privileges` false; no terminal; stdin `/dev/null`;
  stdout and stderr to the build's log.
- **Namespaces**: pid, ipc, uts, mount, network, and cgroup where supported: the
  process is PID 1 of its own namespace.

## 2. Hostname, /etc/hosts, /etc/resolv.conf

- Hostname `buildkitsandbox` (`executor/oci/hosts.go:15`), or build arg
  `BUILDKIT_SANDBOX_HOSTNAME`. `/etc/hostname` is not touched.
- `/etc/hosts`, exactly `127.0.0.1\tlocalhost <hostname>\n::1\tlocalhost ip6-localhost
  ip6-loopback\n`, then one `<ip>\t<host>\n` per `--add-host` (host lowercased,
  `hosts.go:17-90`).
- `/etc/resolv.conf`: the host's, loopback nameservers dropped and, if none remain,
  `8.8.8.8`, `8.8.4.4`, `2001:4860:4860::8888`, `2001:4860:4860::8844`; written as
  nameservers, `search`, `options`, then other lines, no comments
  (`executor/oci/resolvconf.go:32-158`, `util/resolvconf/resolvconf.go`). Network `none`
  gets the same file.
- Both are bind mounts `nosuid,noexec,nodev,rbind,ro`, never in the layer
  (`executor/oci/spec_linux.go:44-52,232-242`).

## 3. Mounts and the device set

| Path | Mount |
|---|---|
| `/proc` | proc, `nosuid,noexec,nodev` |
| `/dev` | tmpfs, `nosuid,strictatime,mode=755,size=65536k` |
| `/dev/pts` | devpts, `nosuid,noexec,newinstance,ptmxmode=0666,mode=0620,gid=5` |
| `/dev/shm` | tmpfs, `nosuid,noexec,nodev,mode=1777,size=65536k` |
| `/dev/mqueue` | mqueue, `nosuid,noexec,nodev` |
| `/sys` | sysfs, `nosuid,noexec,nodev,ro` (`rw` under insecure) |
| `/sys/fs/cgroup` | cgroup, `ro,nosuid,noexec,nodev` (`rw` under insecure) |

No `/run` mount (`spec_linux.go:47`). In `/dev`, runc makes `null` (1:3), `zero` (1:5),
`full` (1:7), `random` (1:8), `urandom` (1:9), `tty` (5:0), each 0666, `ptmx` a symlink to
`pts/ptmx`, and links `fd`, `stdin`, `stdout`, `stderr` to `/proc/self/fd[/0-2]`, and
`core` to `/proc/kcore` where it exists (runc `specconv/spec_linux.go:236-330`,
`rootfs_linux.go:905-935,1140-1143`). Masked: `/proc/acpi`, `/proc/asound`,
`/proc/interrupts`, `/proc/kcore`, `/proc/keys`, `/proc/latency_stats`,
`/proc/timer_list`, `/proc/timer_stats`, `/proc/sched_debug`, `/sys/firmware`,
`/sys/devices/virtual/powercap`, `/proc/scsi`; read-only: `/proc/bus`, `/proc/fs`,
`/proc/irq`, `/proc/sys`, `/proc/sysrq-trigger`.

## 4. Security and network

- Sandbox: capabilities `CHOWN, DAC_OVERRIDE, FSETID, FOWNER, MKNOD, NET_RAW, SETGID,
  SETUID, SETFCAP, SETPCAP, NET_BIND_SERVICE, SYS_CHROOT, KILL, AUDIT_WRITE` (bounding,
  permitted, effective), moby's default seccomp profile.
- `--security=insecure` (entitlement `security.insecure`, else `security.insecure is not
  allowed`): every capability, no masked or read-only paths, no seccomp, all devices
  allowed, `/dev/kmsg`, `/dev/fuse`, `/dev/kvm`, `/dev/net/tun`, `/dev/loop-control` and
  loop devices added, `/sys` and cgroups `rw` (`util/entitlements/security/
  security_linux.go:20-114`).
- `--network=none`: a namespace whose loopback runc brings up (runc
  `specconv/spec_linux.go:456-463`); `host` needs entitlement `network.host`; `default`
  is the daemon's provider (CNI, a bridge, or the host's namespace).

## 5. `RUN --mount`

Type defaults to `bind`; read-only by default for bind, secret and ssh; `mode`, `uid`,
`gid` only for secret, ssh and cache; a relative target joins the working directory; `/`
is refused (`instructions/commands_runmount.go:133-310`,
`dockerfile2llb/convert_runmount.go:64-149`).

- **bind**: from the context unless `from`; `rw` writes are discarded.
- **cache**: id `<BUILDKIT_CACHE_MOUNT_NS>/<id or cleaned target>`; sharing `shared`
  (default), `private` (an unlocked instance or a new one), `locked` (wait); a new cache
  is a root-owned 0755 directory, or with uid/gid/mode a directory of `mode|0755`
  owned uid:gid; never in the output (`solver/llbsolver/mounts/mount.go:88-154`).
- **tmpfs**: `nosuid`, `size=` if given.
- **secret**: id from `source`, `id`, or the target's base name; target
  `/run/secrets/<id>` unless `env=` alone; optional by default (a missing one is no
  mount); required and missing fails `secret <id>: not found`; mode 0400, uid 0, gid 0;
  mounted `ro,nodev,nosuid`, `noexec` unless executable.
- **ssh**: id `default`; target `/run/buildkit/ssh_agent.<i>`; mode 0600; missing and
  required fails `no SSH key "<id>" forwarded from the client`.
- **Stubs** (`executor/stubs.go:49-138`): before the step, every missing path among
  `/etc/resolv.conf`, `/etc/hosts` and the mounts' targets, with each missing parent, is
  recorded; after it, each is removed if it is an empty directory or an empty file, its
  parent's times restored. `/proc`, `/dev` and `/sys` are not among them: made where the
  image lacks them, they stay in the layer.

## 6. The layer

The overlay differ walks the upper directory (`util/overlay/overlay_linux.go:113-359`): a
0:0 device is a removal only where the lower has the path; a path the lower has is
written unless `sameDirent` (same file, or same mode, owner, rdev, `security.capability`
and, for files, size and whole-second mtime, content compared when both nanoseconds are
0; a directory's mtime is ignored); an opaque directory is written as individual
whiteouts and additions, no `.wh..wh..opq`. containerd's writer writes whiteouts as
empty regular files at the epoch, PAX headers, whole-second mtimes, sockets skipped,
each parent once before its first child (`containerd/v2/pkg/archive/tar.go:557-745`).

## 7. Failure

`process "<argv joined with spaces, Go-quoted>" did not complete successfully: exit code:
N`, after `failed to solve: ` (`ops/exec.go:559`, `client/solve.go:341`); an OOM kill
reads `cannot allocate memory`. A signal's status is runc's 128 + signal.
