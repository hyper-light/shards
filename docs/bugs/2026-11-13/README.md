# Review findings index (12 codex gpt-6.1-sol reviewers, 79caf2d74..2bc5f78f9), 2026-11-14T02:57:23Z
Each area's report is beside this index. Reviewed by 12 Codex (GPT-6.1 Sol, ultra) adversarial reviewers, read-only, against R01-R16; none verified yet. ROOT assigns each: P0 to Claude implementers, P1 and below to the Codex reviewers; each is confirmed by a red test or refuted with evidence before any fix.
## creds
creds-01 — P0 / critical — Sign-in capture forwards tokens repeated in response headers
creds-02 — P0 / critical — A chunked response can carry the complete credential in an unmasked extension
creds-03 — P0 / critical — Lease expiry or rotation disables masking of responses already in flight
creds-04 — P0 / critical — Compressed echoes bypass the raw-byte response mask
creds-05 — P1 / high — Lease duration restarts at each delivery hop, authorizing use after the lessor's deadline
creds-06 — P1 / high — Repeated retries of one pending lease accumulate unbounded duplicate waiters
creds-07 — P3 / medium — The CA's constraints authorize descendants outside exact host grants
creds-08 — P3 / medium — A sign-in POST with Expect: 100-continue stalls before its headers can reach the origin
## control
control-01 — P0 / critical — accepted output is truncated when the run ends
control-02 — P1 / high — aborting a pending socket lane leaves its workers and frees its admission
control-03 — P1 / high — the MCP stdio bridge cannot return after its first copy failure
control-04 — P3 / medium — an idle stdin feeder survives member/run termination
control-05 — P3 / medium — cap-loss metadata grows forever and log opens load it whole before charging
control-06 — P3 / medium — a stalled attached viewer retains a daemon thread after the run ends
control-07 — P3 / medium — exec lanes reject valid specs above the unrelated 4 MiB frame bound
control-08 — P5 / low — log-follow fallback adopts an explicitly arbitrary polling interval
## confine
confine-01 — P0 / critical — member exec can acquire a wider Landlock filesystem grant
confine-02 — P0 / critical — initial cwd entry bypasses member DAC permissions
confine-03 — P1 / high — staged HOME files are written behind nested mounts
confine-04 — P1 / high — initial member PATH search uses init's directory and mount view
confine-05 — P1 / high — concurrent plugs overwrite destination ownership
confine-06 — P1 / high — overflow/coalescing loses wakes for descendant watches
confine-07 — P3 / medium — rejected exec environment values are copied into errors
confine-08 — P3 / medium — Linux host watch admission failures disappear
confine-09 — P3 / medium — pending notice capacity is an upstream default
## inithex
inithex-01 — P0 / critical — nesting workload can detach its own device ceiling
inithex-02 — P0 / critical — per-member narrowing overlooks reach through image-wide ports
inithex-03 — P1 / high — failed MCP bridge waits forever for open harness stdin
inithex-04 — P3 / medium — accepted absolute-FQDN MCP grant never matches DNS
## remote
remote-01 — P0 / critical — Untrusted CLI prefixes bypass owner-only authorization
remote-02 — P0 / critical — Windows drive-relative Dockerfile names escape the copy
remote-03 — P0 / critical — Removal does not revoke existing Engine authority or cached enrollment identities
remote-04 — P1 / high — An untrusted build timestamp aborts the daemon
remote-05 — P1 / high — Expanded client secrets are persisted in ordinary logs
remote-06 — P3 / medium — Unauthenticated datagrams count as peer progress
remote-07 — P3 / medium — Relay setup errors lose child cleanup/reaping ownership
remote-08 — P3 / medium — Chosen invitation timeout and frame size still violate R03
## waits
waits-01 — P1 / high — A second wall-clock verdict can OOM-kill a member while its authoritative wait is progressing
waits-02 — P2 / medium — The first test-party observation can refuse a process before any CPU interval was observed
waits-03 — P2 / medium — Already-satisfied test states teach tiny W* records
waits-04 — P2 / medium — A FIFO reader's death teaches a successful take
waits-05 — P2 / medium — Buffered registry bytes still teach fake network silences
waits-06 — P2 / medium — Retry-After exceeding a trouble budget is falsely learned as a trouble passed
waits-07 — P2 / medium — The first successful dial suppresses restored route trouble and handshake records
waits-08 — P2 / medium — An unreaped daemon zombie keeps startup waiting forever
waits-09 — P2 / medium — Test parties adopt a reused daemon PID and count zombie observations as ongoing progress
waits-10 — P2 / medium — A cold command wait can hang after its client exits while another process holds output open
waits-11 — P2 / medium — Concurrent record writers can make the persisted slowest answer shrink
waits-12 — P2 / medium — Chosen production timeouts, retries, windows and polls remain
## memory
memory-01 — P0 / critical — Windows DAX can silently replace in-bounds writable file data with anonymous zeros
memory-02 — P1 / high — Live copying races virtio-mem unplug and reads inaccessible memory
memory-03 — P1 / high — Stopping a VM during live precopy can unmap a layer underneath a read
memory-04 — P2 / high — Removing a builder's budget can permanently leave its old guest cap installed
memory-05 — P2 / high — APFS live-copy failure resumes a VM whose layer mapping is lost
memory-06 — P3 / medium — A fixed two-second policy still controls pressure, budget retries, and recent-growth protection
memory-07 — P3 / medium — PSI's stall threshold and fallback percentage are inherited constants
memory-08 — P3 / medium — PSI fallback polls at a chosen ten-second interval
memory-09 — P3 / medium — DAX's minimum file size is a portable fixed benchmark result
memory-10 — P3 / medium — FUSE budget refresh uses an arbitrary 100x coefficient
memory-11 — P3 / medium — Builder memory reserve still includes a fixed 64 MiB slack
memory-12 — P3 / medium — One-millisecond pacing and read-timeout floors remain chosen constants
memory-13 — P3 / medium — Hotplug still gives up after exactly one retry
memory-14 — P3 / medium — Growth and unplug retention use a fixed 2x working-memory multiplier
memory-15 — P3 / medium — Builders always boot with a chosen 512 MiB floor
memory-16 — P4 / low — Head deduplication stops considering new kinds after 64
memory-17 — P5 / low — The builder give-back parser's 4096-byte frame cap is not derived from its protocol
## migrate
migrate-01 — P0 / critical — Archive-created symlinks escape migration staging
migrate-02 — P0 / critical — Ending HVF working-set recording silently disables live dirty tracking
migrate-03 — P1 / high — Pre-copy dereferences hotplug RAM after it can be unplugged
migrate-04 — P1 / high — A forged snapshot header defeats the state allocation bound
migrate-05 — P1 / high — Named-volume hardlinks become independent files after a move
migrate-06 — P1 / high — vCPU shaping re-enables TSC-deadline outside the configured baseline
migrate-07 — P1 / high — A stronger destination's XSAVE buffer size rejects a compatible guest
migrate-08 — P1 / high — Linux arm64 readiness treats baseline-masked features as unmasked
migrate-09 — P3 / medium — D191's hold still uses an explicitly unjustified fixed period
migrate-10 — P3 / medium — Migration/restore retains chosen cutoffs and budgets forbidden by the brief
## network
network-01 — P1 / high: an empty stale FIN can terminate another TCP flow without either sequence number
network-02 — P1 / high: the network process silently omits syscall confinement on supported arm64 Linux
network-03 — P1 / high: DNS-derived authority accumulates forever in host memory
network-04 — P1 / high: UDP holder changes can reuse a still-live conntrack zone and restore an old destination
network-05 — P2 / high: Hex/API-run never programs the IPv6 uplink that daemon runs receive
network-06 — P2 / high: one listener captures every port in a contested range
network-07 — P2 / high: an IPv6-only listener can win an IPv4 published port
network-08 — P2 / high: a persistent TCP DNS client monopolizes 127.0.0.11 for the whole VM
network-09 — P3 / medium: existing DNS TCP sessions retain obsolete per-member names
network-10 — P3 / medium: host-network bind/close cycles retain unlimited listener history
network-11 — P3 / medium: the DNS grant learner treats unrelated answer records as the granted name's addresses
network-12 — P3 / medium: production network controls still use chosen constants forbidden by current R03
## platform
platform-01 — Landlock leaves existing VM threads unrestricted and fails to revoke sibling snapshot access
platform-02 — Linux pathname Unix sockets reach unscoped host authority
platform-03 — Windows snapshot staging leaves the grants broker open after guest start
platform-04 — Unfiltered prlimit64 changes other host processes' hard limits
platform-05 — WHP's emulator retains pointers into partially unmapped memory
platform-06 — WHP's emulator ignores guest read-only memory permissions
platform-07 — Windows spawns leave unrelated handles inheritable
platform-08 — A valid Windows home path can trigger unbounded slot-wait threads
platform-09 — Case-insensitive Windows filenames can make versioned saves loop forever
platform-10 — Windows priority credits do not protect the host against multiple raised threads
platform-11 — Scheduling periods and pipe quotas remain chosen production constants
platform-12 — Delegated-cgroup cleanup polls at an arbitrary 10 ms interval
## runs
runs-01 — P1 / high — Failed owner metadata grants guest-created files to a later run's user
runs-02 — P1 / high — A renamed daemon home can make its daemon mutate another home's state
runs-03 — P1 / high — BootStop cannot cancel a boot blocked sending its initial request
runs-04 — P1 / high — An orchestrator pool hands out a dead warm host and fails its run
runs-05 — P1 / high — Long-lived API runs and warm hosts block all image-store collection
runs-06 — P2 / medium — Container facts and copy ownership silently use another platform's config
runs-07 — P2 / medium — Concurrent processes lose learned end maxima and can corrupt the record
runs-08 — P2 / medium — Concurrent first GPU launches can publish an incomplete helper
runs-09 — P2 / medium — Concurrent disk-setting commands can publish partial settings
runs-10 — P2 / medium — Reused helper bytes are accepted by length without checking their digest
runs-11 — P2 / medium — Runs still use explicitly arbitrary deadlines, retries and sizes
## devices
devices-01 — P0 / critical — Concurrent disk growth can truncate a live filesystem
devices-02 — P1 / high — A full-span IOMMU mapping makes translation loop until allocation failure
devices-03 — P1 / high — IOMMU response descriptors control an infallible host allocation
devices-04 — P1 / high — Reset can strand a vCPU and teardown behind years of I/O debt
devices-05 — P1 / high — Capacity and generation are not published as one coherent configuration
devices-06 — P2 / medium — VFIO hot-unplug retains MSI routes
devices-07 — P2 / medium — Deep joins size /dev/shm from the wrong VM
devices-08 — P2 / medium — Device paths retain fixed budgets without required derivation
devices-09 — P2 / medium — GPU catalogs have an explicitly unjustified 65,536-device ceiling
devices-10 — P2 / medium — New hot-plug refusals lose their types across the control wire
