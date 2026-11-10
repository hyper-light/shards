# Combined tenant-failure scope: real KVM and independent mutation

Coordinator /root, POLICY_READ c921a0b1 / SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Fedora x86_64 KVM runner8fadc, uid1000, Rust1.98.1. Private root clone
~/managed-capture-check and ~/managed-capture-target;4CPU/8GiB/20GiB admitted.
No other agent owns those paths. Fresh test homes/private empty DOCKER_CONFIG,
no host root or credential use. Raw records and SHA256 manifest are beside this file.

At combined45bc7960, r94's actual mixed AGENT/HARNESS green passed1/1 in19.84s.
The independent fatal-scope mutation then failed0/1 after120.75s, but only because
the observer missed VM shutdown while waiting for a quarantine diagnostic. That
is NOT accepted as the required mutation proof. The log retains actual kernel
power-down; the campaign stopped before its restored/output steps. No production
fix follows from that fixture failure.

Fixture08d7b9d1 also recognizes that actual termination event, then still attempts
a fresh main exec before any quarantine-message assertion. R96 completed:

| Exact source | Actual result |
| --- | --- |
|08d7 plus only retained cgroup.events/console observation patches |1/1 passed4.11s: trusted EISDIR quarantine, main exec0, fresh healthy16byte challenge echoed and privately synced, explicit VM stop.|
|Same with independent fatal-return patch |0/1 failed1.70s, cargo101: actual VM power-down followed by main exec1, No such container. The failed assertion is lost sibling execution, not a missing alert.|
|Restored08d7 with original observation patches |1/1 passed2.84s, same main/healthy progression.|
|Unmodified08d7 production, exact build output test |1/1 passed2.68s: independent main JSON-RPC stdout/stderr.|

The top-level R96 command required the fatal case's101 and actual exec-failure
assertion, verified source restoration, then ran both restored tests. It exited0.
The observations substitute a held directory for ONLY agent-faulty's population
read handle; pread returns real EISDIR, while held pidfd/cgroup.kill remain real.
The mutation changes only that error's failure scope. It does not simulate a real
kernel cgroup failure or prove a boundary impossible to compromise.

Root reviewed/composed registry cancellation d414d21d and proxy completion events
3a1a5de2. R96's runtime contains registry changes; it predates the proxy merge.
Those newer exact-tip combined gates are not inherited from these results.

R01–R16: scoped source is fixture/measurement only; runtime containment and typed
failure design are documented in ../fault-scope.md and ../../managed-capture/typed-failures.md.
No new production constant, wait, cache, authority or OS adapter is introduced here.
One existing FileWatch observes complete console lines, followed by actual
command/nonce effects; no alert-only success or polling fallback. Live commands,
source patches, environments, statuses and prior fixture failures are retained.
No performance, full accounting/leak, arbitrary descendant depth, telemetry
completeness, malicious takeover or full seven-target claim. Actual coverage is
GNU/Linux x86_64 KVM; other hosts and broader chaos/quotas remain unrun. No cleanup
or interruption of other implementers occurred.

Repeat from an isolated admitted checkout using ../run-tenant-observation.sh with
new absolute prefixes, green→fatal→green, requiring the exact fatal exec assertion.
Then run the unmodified build.rs output test as the setup-metadata record documents.
