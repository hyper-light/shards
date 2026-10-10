# virtio-fs under hostile requests (audit V)

Evidence for the virtio-fs decisions in docs/design/architecture.md (D38, "Every request
is the guest's to forge"); results are platform-measurements.md M129 and M130.

## `run.py`

    run.py OLD_REV [--runs N] [--n SAMPLES] [--delay-us D]
    run.py --old-bin OLD --new-bin NEW

Builds the harness (`src/main.rs`) against OLD_REV's `crates/vmm` (a temporary git
worktree) and the working tree's, or takes two built binaries, and alternates fresh
processes per case, reporting n, p50, p90, p99 and max per arm and the paired difference
of the processes' medians with a bootstrap 95% interval:

- `open`: a share's OPEN then RELEASE of one file, through `Server::handle`;
- `serve`: a GETATTR through the device, from the driver's notify to its used entry,
  the share answering at once on a thread;
- `held`: the share answering after `--delay-us`, how long another thread waits for
  guest memory once the share has the request;
- `list`, `listplus`: whole listings of a directory of 10,000 entries, READDIR or
  READDIRPLUS pages of 4096 bytes (M130). `--cases` picks among them.

## `probes/`

C programs for the primitives the fixes rest on, run by hand (`cc -O2 -o p p.c`):

- `follow.c DIR`: in DIR, whether `linkat(…, 0)` follows a symlink (it does not) and
  whether `fchmodat(…, 0)` does (it does: the file outside the share changes mode), and
  what AT_SYMLINK_NOFOLLOW changes instead.
- `chmodat.c` (Linux): the steps of `chmod_at` on Linux, `O_PATH`, `fstat`, and the
  `/proc/self/fd` link: a symlink refused, a regular file and an unreadable one changed.
- `dirbuf.c DIR`: the bytes the C library keeps for an open directory stream (M130).
- `getdents.rs DIR N` (Linux, `rustc --edition 2024 -O`): N files made in DIR, then
  paged through by getdents64 at `d_off` cookies with three page and buffer sizes, each
  entry once.
- `src/main.rs --case fds --dirs D --limit L`: how many of D directories a guest can look
  up, none forgotten, under a descriptor limit of L (M132).
