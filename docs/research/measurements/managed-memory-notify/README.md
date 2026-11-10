# Native Linux notification-framing fixture

Status: **source and pure checks only; native compilation and every live run owed
to the coordinator.** This is one standalone research artifact, not a production
change or a proposed replacement for `Memory::relieve`'s `4096`.

Implementer: Kiln `01a1c5bda0bf766683384c0705c0a9bc`; coordinator:
`01a148884a4770fdaf0111f3f792a248`. Approved ownership is this new directory only.
Production source, existing research records and held helper artifacts are
read-only. No child agents, plan dispatch or automatic delegation is authorized.

The full canonical policy/plan/handoff was reread at main
`69dbc3180b8c8c55cb03f9f95f822d2752ba2887` before edits. Rules SHA-256:
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Plan SHA-256: `9aafc728a11f6aa529873221827223320512f88ff579ba23d00ee0edfb3f24e8`.
The handoff SHA-256 was
`42f035652d31893551dba0fe9339c75cf4a289377cf3d0ee4a9c0302a85f9cfb`;
its older main/job references do not supersede the coordinator's newer messages.

## Contract and sequence

[fixture.rs](fixture.rs) has exactly four mandatory arguments, with no defaults:

```text
fixture NEW_DIRECTORY DEADLINE_MONOTONIC_NS WRITE_COUNT READ_BYTES
```

- `NEW_DIRECTORY` is an explicit absolute path whose parent the caller owns and
  whose final directory does not exist. The fixture creates that directory with
  requested mode `0700`, then creates `NEW_DIRECTORY/watched` using `create_new`
  and requested mode `0600`. Existing paths cause failure. The caller must select
  a private local regular-file filesystem, without concurrent pathname mutation.
- `DEADLINE_MONOTONIC_NS` is a positive absolute Linux `CLOCK_MONOTONIC` timestamp,
  in nanoseconds. Every retry checks that same deadline; no retry extends it.
  The caller supplies a separate finite outer process timeout for blocking
  filesystem/report I/O, signals, process loss and cleanup failures.
- `WRITE_COUNT` is positive and representable as `usize`. It bounds successful
  one-byte writes and file growth in this fixture. There is no additional seed
  write. Content byte `1` is synthetic fixture data, not a production literal.
- `READ_BYTES` is the positive caller-owned notification buffer envelope, between
  the UAPI header size and `isize::MAX`. Allocation uses `try_reserve_exact` before
  resizing. Allocator rounding, standard-library path/argument allocations and
  kernel charges are not bounded or profiled by this logical envelope.

The runtime is Linux-only and uses `libc = "=0.2.189"`, matching canonical
`Cargo.lock` (crate checksum
`3eaf3ede3fee6db1a4c2ee091bf8a8b4dccdc6d17f656fb07896ee72867612f2`).
The native module is `cfg(all(target_os = "linux", not(test)))`. Compiling the
standalone file with `rustc --test` excludes every native syscall/file path and
does not require the libc crate.

1. Validate inputs/deadline and allocate the admitted notification buffer. Create
   only the new private directory/file. Record file device/inode identity.
2. Create one nonblocking, close-on-exec inotify instance and one **file-inode**
   `IN_MODIFY` watch. This is not a directory watch. Perform the requested actual
   one-byte writes. Reopen the file independently, check device/inode identity,
   verify every byte and EOF, then drop that reader.
3. Wait for `POLLIN`, recording the actual poll return and `revents`. Read with
   `H - 1` bytes, where `H = sizeof(inotify_event)`. Require actual `EINVAL` and
   read readiness again. This checks refusal of the pending record, not an empty
   queue's behavior.
4. Explicitly remove the watch. Record `inotify_rm_watch`'s return/errno. Its
   distinct `IN_IGNORED` is queued after the retained modify notification.
5. Read with `min(READ_BYTES, H + 1)`. Require one complete unnamed modify frame,
   then observe readiness again. If `READ_BYTES > H`, the positive return of `H`
   bytes is **short relative to the requested count**, yet another record remains
   ready. The result reports `short_positive=false` when the envelope is exactly
   `H`; that invocation does not exercise a positive short read.
6. Perform an actual positive **exact-`H` read**, then drain using the caller's
   full `READ_BYTES` envelope. Decode every complete frame without aligned casts.
   Require the owned watch's `IN_IGNORED`, followed by an actual nonblocking
   `EAGAIN`. A short positive read, a full read and readiness are all distinct
   from this instantaneous drain witness.
7. Explicitly close the inotify descriptor and writer, recording results. On
   earlier failure, attempt removal of any still-active owned watch and close
   owned descriptors. A failed run does not claim `IN_IGNORED` was consumed.

Paths transition `Absent -> DirectoryCreated -> FileCreated`. They are
**deliberately retained on success and failure**, with the last ownership state
reported. There is no unlink, recursive removal or pathname deletion anywhere in
the fixture. Root retains the created artifacts as evidence and owns any later
cleanup; pre-existing or replaced paths cannot be deleted by this artifact.
An independent reader uses `File` ownership for all early-return cleanup; root
still owes actual descriptor-return evidence. Watch transitions are
`Absent -> Active(wd) -> Removed(wd) -> Ignored(wd)`, followed by instance close.
Close on a failure path can terminate an active/removed watch without consuming
its final event. No descriptor is retried after `close` reports an error.

Each successful file write, notification read request/return/errno, complete raw
frame, poll result, removal and explicit close is recorded to the caller's
private output. Raw frame byte arrays preserve the native bytes; parsed watch,
mask, cookie and name length accompany them. Truncated framing, wrong owned watch,
unexpected masks, EOF, overflow, missing ignored notification and I/O/deadline
errors terminate with typed internal faults and a nonzero process result.
Unknown mask bits are accepted by the **framing** decoder, then rejected by the
fixture's deliberately narrow owned-watch scenario.

`WRITE_COUNT` is never asserted equal to notification count. Identical unread
events can coalesce; file readback proves actual bytes independently. An
`IN_Q_OVERFLOW` raw frame terminates with notification completeness unknown.
Reading a regular file does not establish a pressure event, cgroup transition,
OOM/victim outcome, kernel charge attribution or any production memory policy.

## Sources, derivations and limits

[Source identities](purechecks/evidence/source-identity.json) retain exact URLs,
revision, byte counts, retrieval timestamps and SHA-256. All five pinned Linux
bodies were re-fetched and hash-matched against the earlier
[Poller/inotify identity record](../managed-poller-research/source-identity.json).
Linux tag `v6.18` resolves there to
`7d0a66e4bb9081d75c82ec4957c50034cb0ea449`; it is a research pin, not the running
Fedora kernel or a new product kernel selection.

| Primary source | Contract used |
| --- | --- |
| [Linux inotify UAPI](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/inotify.h) | Four native-endian 32-bit fields; signed watch descriptor; name length includes NUL/padding; mask/flag identities. SHA-256 `48c09a53335353490f94cd465c96f092d0b3a9736b42aae8691e41b9c6390370`. |
| [Linux inotify userspace implementation](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_user.c) | `get_one_event` refuses a record that does not fit before dequeuing it; `inotify_read` returns a successful prefix ahead of a later size refusal; empty nonblocking read returns `EAGAIN`. SHA-256 `9a98d8a927c6b2ffa2d403cc52bb1cf45a65f30048143be3dc1178d8e6c1cf3e`. |
| [Linux inotify fsnotify adapter](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_fsnotify.c) | Event equality/coalescing and ignored-event generation. SHA-256 `ac62662f1843eb4d899d8adb419fe3393d1f4f0ab29d2123aa1abcaaf1203096`. |
| [Linux fsnotify dispatch](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/fsnotify.c) | File-inode notification removes the parent filename; this scenario expects unnamed records. |
| [Linux notification queues](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/notification.c) | Queue/coalescing/overflow ownership; no event-count equality or unlimited retention promise. |

The decoder copies each scalar from a checked byte range, including unaligned
input. Offset/header/name arithmetic is checked; nonempty names require a NUL,
but names are not decoded as UTF-8 and no extra padding/alignment rule is invented.
Native execution checks `H == size_of::<libc::inotify_event>()`. The supported
Linux amd64/arm64 libc variants still need root's actual build/run evidence.
The raw record representation is native ABI, not a portable network format.

`H - 1`, `H` and `H + 1` are framing boundary probes: one byte below one header,
one header, and one byte above one header but below two headers. They do not
select a production buffer. The two different masks used to keep another frame
pending come from the requested modify watch plus its explicit removal; no extra
watch mode, producer thread or duplicate watched-state cache is introduced.
`poll` milliseconds round upward from the remaining nanoseconds and clamp only
at `i32::MAX`, its timeout ABI bound. Expiry is rechecked against the original
deadline after each wait; the sub-millisecond rounding is not extra authority.
The conversion literals are physical ns/ms/sec units. There is no sleep interval
or periodic state probe. EINTR retries retain the same deadline.

Cold start creates fresh owned objects and refuses existing paths, expired input,
invalid envelopes and allocation/init/watch failures without a fallback. Queue
overflow is explicit loss, not a larger learned buffer. Read `EFAULT` is not given
the undersized-`EINVAL` retention promise: the kernel can remove an event before
copying it to userspace. This fixture does not deliberately execute unsafe invalid
pointer faults. SIGKILL/process loss closes descriptors through the OS; it cannot
produce an observed `IN_IGNORED` or a successful cleanup report after death.

The established alternatives and their pinned sources remain in
[the completed research](../../managed-poller-capacity.md): caller-owned
reusable storage, exact-record framing versus directory-name accommodation,
capacity from actual admitted demand, fallible allocation and level/readiness
fairness. This artifact selects only a controlled **file-inode framing example**.
It does not choose among production alternatives. Replacing `4096` with `H`, a
page size, a copied library default or a measured sample is not justified here.

## Actual light evidence

The local operation was formatting with the already installed native
Rust `1.98.1` rustfmt; no local compiler/build target was created. Remote checks
used an admitted `sync:false` new private directory on Fedora runner
`8fadc1ec213332e16cb0d915a3fb913f`, one CPU affinity and 1 GiB address-space limit;
Kiln admitted 1 CPU/1 GiB/2 GiB. A per-file 2 GiB size limit is not a hard aggregate
disk quota. The retained source/test artifacts are about 22 MB, not a memory or
runtime performance measurement. No root clone/target/cache/VM was touched.

[purechecks/driver.py](purechecks/driver.py) records tool identities, exact argv,
source hashes, outputs, exit codes, private paths and the explicit native exclusions.
It can be replayed through the same admitted Kiln envelope by supplying the exact
fixture bytes as base64 in `SOURCE_B64`. It compiles only `rustc --test` copies.
The native module is absent from those binaries, not merely left uncalled.

- [Attempt 1](purechecks/evidence/remote-attempt-01.json): format-only exit 1 before
  any compilation. A standalone directory lacked canonical `rustfmt.toml`'s
  `max_width = 110`; the raw result is retained.
- [Attempt 2](purechecks/evidence/remote-attempt-02.json): explicit canonical width;
  format 0, nine synthetic tests 0, two independent pure guard-removal mutants 101
  for the relevant assertions, and restored unmodified nine tests 0. Rust
  `1.99.1` / LLVM `23.1.1`, GNU amd64; kernel identity retained in the record but no
  inotify interaction occurred. This attempt's source hash is retained separately
  from the final source below.

The two guard removals accept an unterminated name and round a positive remaining
deadline down to zero milliseconds, respectively. The resulting failed assertions
demonstrate those pure checks detect the lost guards. They are not live production
mutation or performance results. Header/name truncation at every boundary,
concatenated/malformed tails, signed overflow watch, non-UTF-8 name, checked size
overflow, expiry and ABI timeout clamp are also tested.

Native code received a subsequent scoped change to log its absolute configuration,
recheck deadlines before new setup phases, and use the **full caller read envelope**
for the final drain after a positive exact-header read. The final fixture SHA-256 is
`bafc3ff31ae9f5a4c484124911c890494860bc88c50fd2d751aed57c2f44aab7`.
[Attempt 3](purechecks/evidence/remote-admission-03.json) was not admitted because
no CPU was idle, so it created no job or files; no guard was bypassed. After a new
inventory admitted one CPU,
[attempt 4](purechecks/evidence/remote-attempt-04.json) tested this exact final
source in `/home/adalundhe/managed-memory-notify-pure-dn_nvjm8`: format 0,
nine tests 0, both targeted mutants 101, unchanged source nine tests 0. Native
compilation/execution remain explicitly UNRUN. All private artifacts are retained;
there are no active lane jobs or local target/cache paths.

## Coordinator execution and owed evidence

All commands in this section are **ROOT proposals, unrun by the implementer**.
Root chooses/adopts finite inputs and admits its own private source/target/cache.
No repository-wide or active root clone sync is needed. For a standalone build,
copy only `fixture.rs` into a new private source directory and generate this
private (uncommitted) manifest:

```toml
[package]
name = "managed-memory-notify-fixture"
version = "0.0.0"
edition = "2024"

[[bin]]
name = "managed-memory-notify-fixture"
path = "fixture.rs"

[dependencies]
libc = "=0.2.189"
```

Root must retain its generated lockfile, verify the libc checksum above, compile
the **non-test native binary** into its admitted private target, and retain source,
binary, toolchain, libc, kernel, filesystem/mount and raw command identities. The
pure test result does not establish that the excluded native module compiles.
Compile errors, lint findings and live red outcomes require their own retained
record and scoped correction; no guessed production fix follows from them.

Root supplies an explicit new private path, positive write count, read envelope,
absolute deadline and finite outer kill deadline, then captures stdout/stderr and
exit status. If deriving the absolute timestamp from a supplied duration, Linux
Python's `time.clock_gettime_ns(time.CLOCK_MONOTONIC)` plus the caller's explicit
positive budget uses the same clock; reject overflow instead of extending it.

| Owed root case | Required actual evidence |
| --- | --- |
| Native compile/ABI | GNU and musl on amd64/arm64 as available; header identity, exact dependency and source hashes. Unsupported/unavailable cases remain unverified. |
| One write, then several writes | Independent inode/content/EOF readback and successful write calls; actual notification count reported separately, including coalescing. No event/write equality. |
| `READ_BYTES == H` | Actual under-header `EINVAL`, retained modify frame, exact-header ignored frame, subsequent `EAGAIN`; short-positive case explicitly unexercised. |
| `READ_BYTES > H` | Actual `H+1` request returning `H`, raw modify frame, positive readiness for pending ignored record, positive exact-`H` read and final full-envelope `EAGAIN`. Larger envelopes remain caller choices, not production recommendations. |
| Existing directory/file, invalid/expired input | Existing contents/identities unchanged; no overwrite/deletion; created-artifact ownership, typed result and descriptor return. A pre-existing final directory must be refused before opening its child. |
| Allocation/FD/watch limits and I/O/readiness errors | Actual fault/errno and terminal state, no fallback; owned FDs/watch returned or uncertainty explicitly recorded. Synthetic null/malformed bytes alone do not establish native exhaustion. |
| Interrupt/deadline/process/report failures | Original deadline retained across EINTR; no periodic probes; actual close/resource-return observations, with forced process death and incomplete output labelled separately. |
| Guard removals | Root's real lost-record/short-read-as-drained/removal/cleanup mutations fail for the corresponding actual frame/resource effect, followed by source-matching restored passes. Pure tests do not substitute. |
| Cost and production comparison | Root-only equivalent-workload allocation/copy/fault/CPU/kernel/FD and fairness measurements, source-matching `Memory::relieve` baseline, uncertainty and raw distributions. No measurements or 4–5× result exist in this batch. |

This scenario deliberately uses only regular-file notification framing. Genuine
cgroup pressure/OOM notification timing, authoritative counter readback and victim
semantics need a separately reviewed root campaign. It cannot establish fairness
across registrations or production cancellation semantics. Those are explicit
remaining questions in the existing research, not inferred from this fixture.

## R01–R16 account

| Rule | Applicable evidence and remaining gap |
| --- | --- |
| R01 | No production edits. Native helpers return typed faults; checked ranges/conversions, fallible notification-buffer reservation, owned descriptors and exact-once close. Pure tests may assert. Standard-library allocations and all excluded native failure paths remain unverified, so no universal panic/OOM claim. |
| R02 | Pinned Linux sources and established alternatives linked above. Only the explicitly approved research fixture is implemented; native evidence and any production decision remain root work. |
| R03 | Header and boundary sizes derive from UAPI; timeout bounds/units derive from ABI/physical units; resource amounts are explicit caller/admission inputs. No replacement production number/default selected. |
| R04 | Explicit path/watch/read/fault states; removal precedes ignored observation, drain needs EAGAIN, failure preserves uncertainty. Native transitions/cleanup unrun. |
| R05 | Existing libc inotify/poll seam, one authoritative owned watch, readiness plus one absolute deadline; no cache, extra manager, fallback or periodic probe. No measured retention/refactor claim. |
| R06 | Focused decoder/arithmetic/native ownership/observation functions; no module framework or production restructuring. Complexity/native path review belongs to root, with compilation still owed. |
| R07 | Comments explain native-endian unaligned decoding, framing sizes, ownership, close retry risk and source contract. No documentation-generated performance assertion. |
| R08 | No competing-workload or 4–5× result. Root comparative campaign remains open. |
| R09 | No production defect or reproduced `4096` failure claimed. Actual pure malformed-frame/arithmetic mutation failures are retained separately from all live cases. |
| R10 | Source, replayable pure driver, original format red and exact pure commands/results retained here. Native before/after, guard-removal and cleanup traces are owed. |
| R11 | Linux-only research artifact; production matrix unchanged. GNU amd64 pure proof only; actual GNU/musl amd64/arm64 builds/runs unverified. macOS arm64 only; paused Windows ARM VM untouched. |
| R12 | Existing libc platform seam; no installed deliverable, platform workaround, host privilege, security or crypto change. This is not shipped with Shards. |
| R13 | One Rust fixture source with embedded synthetic tests and a small replay driver, explicit inputs/ownership. Paths retained rather than introducing a cleanup manager. Required native behavior remains root-unverified. |
| R14 | No profiling/benchmarking executed by this lane. Native allocations, parent/kernel costs, copies/faults and distributions remain root-owned and unmeasured. |
| R15 | Sole implementer; no new child/plan dispatch. Named signed branch checkpoint, no main merge; all native compilation/execution, audits, campaigns and combined gates returned to root. Held helpers/artifacts preserved. |
| R16 | No local build, target/cache, cleanup, new full worktree or large archive. Admitted new private remote pure files preserved; root targets and paused VM untouched. Mac floor policy retained. |

The earlier auto-child dispatch was caused by this lane's old
`plans_ready(..., then:"goals")` call with parallel dispatch enabled; it was a
violation of the explicit no-child instruction. Root subsequently replaced only
future dispatch through agent-owned `goals_set`, archiving the prior state at
`/Users/adalundhe/g121-constants-scheduler-before-correction.json`. This lane did
not call a personal-settings API. Held helpers and their unrelated untracked
artifacts remain read-only; none is staged in this checkpoint.
