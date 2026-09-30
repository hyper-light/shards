# Confining a VM process on macOS with supported APIs only

Research note, 2026-09-30. It replaces D30's use of `sandbox_init`: the SDK's header marks it
`API_DEPRECATED("No longer supported", macos(10.5, 10.8))` and says "This header is
deprecated and may be removed in a future release" [SDK usr/include/sandbox.h:7,45], and its
manual page says developers "should instead adopt the App Sandbox feature"
[man: sandbox_init(3)]. shards uses no deprecated or unsupported API.

Tags: `[Apple: …]` is developer.apple.com documentation, `[SDK …]` a header of the macOS 26.4
SDK, `[man: …]` a manual page, `[PM Mn]` our measurement in platform-measurements.md.

## 1. What Apple supports

- **App Sandbox is the supported sandbox, and command-line tools adopt it.** A tool gets a
  bundle identifier, which becomes its code-signing identifier, and the App Sandbox
  capability, which is the `com.apple.security.app-sandbox` entitlement
  [Apple: Embedding a command-line tool in a sandboxed app; Apple: App Sandbox Entitlement].
  A standalone tool carries its bundle information in an Info.plist linked into its
  `__TEXT,__info_plist` section; without one, the probe below is killed at launch (§2).
- **The hypervisor entitlement works in any process.** "The entitlement is required to use
  the Hypervisor APIs in any process" [Apple: com.apple.security.hypervisor].
- **A sandboxed process gets its own container**, `~/Library/Containers/<identifier>`, with
  full access to it; since macOS 14 the container is tied to the process's code signature
  [Apple: Accessing files from the macOS App Sandbox, "Use files in your app's container"].
- **Another process can grant it files, one by one.** "Share file access between processes
  with URL bookmarks": a bookmark made with options 0 "grants access to the resource to a
  process that resolves the bookmark", and "the receiving process automatically attempts to
  extend its sandbox to include the bookmarked resource" [same page]. A bookmark to a folder
  extends the sandbox "to items within that folder, and recursively in nested folders"
  (stated there for user-selected folders).
- **Restrictions are enforced when a resource is acquired** [man: sandbox_init(3), "Keep in
  mind that sandbox(7) restrictions are typically enforced at resource acquisition time"]:
  a descriptor opened before, or received, is used as it is.
- **Hardened Runtime** protects runtime integrity (code injection, library hijacking) and is
  best practice for new code, required for notarization [Apple: Hardened Runtime]. It does
  not confine files; it goes beside App Sandbox.
- **Not confinement:** Endpoint Security observes rather than confines, and needs its own
  restricted entitlement; the Virtualization framework's own sandboxing is for VZ's
  process, not ours.

## 2. What a sandboxed tool can do: measured

`docs/research/measurements/app-sandbox/` (`probe.c`, `bookmark.c`, `entitlements.plist`,
`Info.plist`, `cost.py`): a C probe signed ad hoc with App Sandbox and the hypervisor
entitlement, Hardened Runtime on, its Info.plist embedded; each trial prints ok or refused.
macOS 26.4.1, Apple M5 Max, 2026-09-30 [PM M67].

| Trial | Result |
|---|---|
| Launch without an embedded Info.plist | killed at launch (SIGTRAP, exit 133) |
| Launch with one | runs; `HOME` is `~/Library/Containers/dev.shards.probe/Data` |
| `hv_vm_create` / `hv_vm_destroy` | ok |
| Read, write or create a file it was not given | refused (EPERM) |
| TCP bind on 127.0.0.1 | refused |
| Unix `connect` to a socket outside its container | refused |
| Read and write descriptors inherited from its parent | ok |
| `openat` under a directory passed as a descriptor | refused: the check is on the path |
| A file granted by bookmark | read ok; a file beside it still refused |
| A directory granted by bookmark | files made in it (`state`, `memory`) ok |
| Unix `bind` or `connect` in a directory granted by bookmark | refused |
| Unix `bind` and `connect` inside its own container | ok |
| `accept` on a listening socket its parent bound and passed | ok |
| A rebuild, re-signed ad hoc (another CDHash), using the same container | ok, no prompt |

Launch to exit, creating and destroying a VM, alternating, n = 200 each, load average about
16: sandboxed p50 7,200 / p90 8,106 / p99 8,594 / max 8,867 µs; the same binary without
App Sandbox 4,115 / 4,793 / 5,075 / 5,501 µs. App Sandbox costs a launch about 3.1 ms at
the median, as the Seatbelt profile's compiling cost 3.7 ms [PM M53].

## 3. The design this gives (D30, macOS)

- `shards-vm` is signed with App Sandbox, the hypervisor entitlement and Hardened Runtime,
  its identifier `dev.shards.vm` and Info.plist linked in. `scripts/hvf-run` and releases
  sign it so; `shards` and `shardsd` stay outside the sandbox, as the brokers.
- **Files by bookmark.** The spawner (the daemon, or `shards vm` for a direct run) makes a
  bookmark for each file the VM reads or writes (kernel, initrd, init, disks, pmem, the
  snapshot a restore reads and the files it records) and for each directory it writes in
  (the snapshot it saves, a warm VM's container logs), and passes them; the VM resolves them
  before it opens anything. Nothing else outside its container is reachable.
- **Sockets by descriptor.** A Unix socket cannot be bound or dialled outside the container,
  even in a granted directory. Every VM process shares one container, since they share one
  identity, so sockets there would be reachable by every other VM: they are not used.
  Instead the spawner binds the vsock device's listening socket and passes it (`accept` on
  it works), and a guest's connection to a host port is dialled by the spawner and the
  connected descriptor passed back over the control socket the VM already has.
- **Cost:** about 3.1 ms at launch, before a warm VM's request; on a cold `shards vm
  restore`, on its way, as Seatbelt's was.
- **Open, to measure before relying on it:** that a guest-initiated vsock connection brokered
  by descriptor meets the latency of a direct `connect`; that a bookmark to a directory
  created after the grant's parent (the template's final name, D30 `--settles-to`) is not
  needed, since the snapshot directory is granted before its rename; how the container prompt
  behaves for a binary signed by another identity than the last (an upgrade from a release
  to a local build).

## 4. What Linux has, for comparison

Landlock is a supported kernel interface; D30's Linux confinement (seccomp, Landlock at ABI
v5 or later, failing closed) uses no deprecated interface. `tkill(2)`, which the kernel
documents as obsolete beside `tgkill(2)` [man: tkill(2)], is refused by the filter; shards
calls `tgkill`.
