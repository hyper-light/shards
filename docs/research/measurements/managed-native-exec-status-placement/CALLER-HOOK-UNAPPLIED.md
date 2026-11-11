# Exact tested status boundary; production splice unapplied

The real producer hook is owned by the native helper lead. No production patch
is applied by this scope. The available frozen helper caller does not yet have
the per-job body plus actual owner-issued scope/commit/ACK inputs needed to form
a truthful complete job-launch patch. A made-up caller, token or fake grant is
not supplied in its place.

The complete compiling caller is
`harness/src/native_effects/process/status_placement.rs`, using exact v4 product
bytes. It captures the actual staged status **before clone and before install**:

```rust
let staged_status = plan.launch_status_before_install()?.as_raw_fd();
```

In the existing trusted child failure branch, select only the typed location:

```rust
let status = match failure.status_location() {
    StatusLocation::Staged => staged_status,
    StatusLocation::Installed => STATUS_FD,
    StatusLocation::Unknown => /* retain/report uncertainty; never infer success */,
};
```

The first snippet's `?` stands for the real caller's typed preparation refusal,
not an introduced Option-based production result. The Unknown arm is an explicit
integration obligation, not executable placeholder product code. The fixture's
actual implementation raw-exits that case as a test failure.

The real error converter must preserve the captured typed cause/positive errno
before any further syscalls. The fixture uses the **unchanged canonical**
`lifecycle::Failure` and `Packet::failed(...).encode()` for its two exact cases,
and `Packet::decode` on the parent; it defines no duplicate binary parser.
`Install { destination: 0 } / EBADF` maps to canonical Setup/EBADF; Exec/ENOEXEC
maps to canonical Exec/ENOEXEC. The fixture deliberately clobbers ambient errno
after conversion, then writes the saved canonical packet.

This is a tested narrow placement pattern, not a complete production error map.
The actual helper must retain all other local error/codec/transport uncertainty,
return typed NotStarted/Started/Uncertain with actual job/pidfd/status ownership,
keep cgroup/charges on unknown, and resolve release only through the real owner
and audit path. A decoded packet/PID, status EOF or leader wait grants nothing
and proves no descendant population empty. Freeze that actual caller span with
the native/parent owners before making its unapplied patch concrete.
