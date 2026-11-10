# Separate-process compatibility checkpoint — rerun owed

This is a runnable fixture checkpoint, **not a passing complete interop campaign**.
The ordinary experimental-pin corpus passed; the first separate-process campaign
passed five cases, then exposed a fixture marker bug. That bug is corrected and
compile/lint checked, but its cross-process rerun remains coordinator-owned. No
production source, manifest, lock entry, API, connection limit or protocol changed
in this batch. The a875 experimental pin and upstream233/7cc9 remain unadopted.

Policy receipt: canonical c921a0b12b06d495b354907162e991376edffd41, all R01–R16,
rules SHA2561ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.
AGENTS, full rules/evidence, CLAUDE, current managed plan and handoff reread.
Root coordinator owns profiling/audits/heavy E2E/repetition/resource gates under
the newest user instruction; this lane now owns focused implementation/light checks
only. The already-started r162 was preserved until it ended; no further profiling
or separate-process campaign was started after that instruction.

## Sources, scope and actual results

Candidate worktree/branch managed-rpk-compat / p/managed-rpk-compat starts at
clean a875e8b0fe68434fc972093879818fa37b02bdb8, retaining the experimental233 TLS
pin. Old comparator managed-rpk-compat-old / p/managed-rpk-compat-old starts at
77226fb5 with originalf116 TLS. Each receives identical test role source; production
content and pins are read-only. Owned files: remote/tests/managed_process.rs and
this measurement directory. Existing managed/net diagnostics/hooks were untouched.

- r155: `cargo test --locked -j4 --profile test-release -p shards-remote` at a875:
  **19 unit +17 loopback integration passed,0 failed**,2 measurement tests ignored,
 0 doctests. The final new ignored role fixture is compile checked separately;
  this result is not represented as an all-target/final combined gate.
- Candidate f9dd852b8 and old c97df7f36 fixtures compiled as fresh release artifacts,
  with matching source SHA a5e52ab47f162bcda38dcc2fcf33ca76e269ab11d38c77d032a49804cdc0f617.
  Candidate strict clippy passed; initial test-only assertion/collapsible-if lint
  failures and their corrections are retained.
- r162: old-server/new-client,2channels normal/stall/wrong-server/wrong-client and
 4channels normal passed: **5 cases**, including exact1024B binary records,
  application-consumed ACK, zero accepted/consumed payload for wrong pins, and
  role/parent FD/task return in those passing cases.
- The sixth case, old-server/new-client4channel stall, failed client101 at10s.
  The stalled channel received one exact record; all three neighbors finished16
  records. Its first marker was actually
  `test managed_process_role ... managed_record\t0`, but Python required startswith.
  Parent therefore missed readiness and did not send private owner EOF after
  neighbor completion. The failure log also exposed a fixture terminal outcome
  overwritten by its deadline callback. This is a reproduced fixture failure,
  **not proof of a TLS/QUIC interoperability defect**.
- Corrected candidate role at8e45e0024a245282631af2af3db87a7e44fb6b5d and old at
 7bf115def108b9e61f481369c83161a1196b2661 pass strict clippy and fresh release
  no-run builds (r164/r165). Glued-marker recognition was checked directly with
  the actual failing line. Terminal outcome is now assigned once; terminal state
  has no further fixture deadline. **No corrected cross-process result is claimed.**
- Reverse new-server/old-client cases, remaining forward cases and all resource
  repetitions are **NOT RUN**. r162 stopped before any resource stage began.

[All retained logs/build JSON/source and binary identities](results/) include the
actual red case. Ephemeral private fixture keys and binaries are excluded from Git.
Archive SHA256 eb562f34a83003d3cecf59189d54923300f5841c5e24728f77b4cb047373db24.

## Fixture behavior and limits

[role.rs](role.rs) uses only public existing Identity, Endpoint, Producer, Control,
Driver, frame::take, subscribe and consumed APIs. It creates no parser, crypto,
trust or control replacement. Each channel has its actual owner-pinned QUIC
endpoint and declared run/generation/member/channel scope, one unacknowledged
record, and unchanged admitted transport/handshake bounds. The old/new build changes
only which pre-existing production TLS pin is linked, never a runtime compression
switch. Integration-test executables use the ordinary System allocator, with no
instrumented timing or allocation claims.

Server readiness is the actual bound endpoints and written private fixture addresses.
Client records are parsed by canonical framing and checked byte-for-byte, including
cursor. ACK is sent only after fixture consumption. Each server worker waits for
actual returned credit before finish. A stalled first channel withholds ACK; all
neighbors must finish before the parent closes server stdin. That EOF is private
fixture owner cancellation, invoking existing Control::abort. It is not product
stdin or a new production control protocol. Wrong pins use separately generated
private fixture identities; no user/provider credentials or external service is read.
A timeout is a failed fixture, never successful pin rejection.

Public Handler::lost exposes reason text/code rather than an inner typed TLS error;
this fixture requires no received payload and a non-application close for wrong
pins, while server progress must remain accepted=consumed=0. Exact TLS verifier/
signature behavior is reused unchanged. Stronger negative-path attribution remains
available in the upstream canonical verifier tests; no public API is added solely
for this fixture.

All connections are fresh first attempts. No reconnect retry, time polling, limit
increase, early drain discard or retirement diagnostic is introduced. Observer-quality
owns f2b61f239 and its separate post-drop event study; immediate closed-not-drained
reconnect refusal remains unresolved production behavior. No diagnostic is imported.

With MANAGED_PROCESS_RESOURCES=1, roles can snapshot RSS/PSS/private maps, stat CPU/
fault counters, FD/task counts, configured socket limits, uniquely owned UDP inode
queue rows and readable cgroup files. The corrected parent can also snapshot itself
and both child processes at the stalled-record/neighbor-complete boundary before
owner EOF. These observations are not hard kernel quota enforcement or a reservation
proof. Missing cgroup/controller/process values remain explicitly unknown. UDP queue
bytes and socket buffer limits omit transient slab/full kernel charge. C malloc,
copy volume, exclusive cgroup accounting and stable latency tails are not proved.
No new learned quota or memory authority is invented.

## Frozen artifacts available to root

Runner: Fedora8fadc1ec213332e16cb0d915a3fb913f, Linux amd64/glibc, rustc1.98.1.
Private target ~/managed-rpk-compat-target; private evidence
~/managed-rpk-compat-evidence. No active lane job after r165. Root may take these
inactive private resources for central gates; root's r108 clone/target were untouched.

| Corrected role | Runner path | SHA256 |
| --- | --- | --- |
|new233|~/managed-rpk-compat-evidence/new-role-corrected|8a0a6b040ca13ec640c31b4bccc33f95402d36623c88b3c7a2c6069263bc10d4|
|oldf116|~/managed-rpk-compat-evidence/corrected/old-role|54fd2548aaf799287b2fbf515bfe196e41563af9af7e99ac1ca276856d5cec08|

Both corrected role source SHA256:
e16efa92d6d6a5a72e7a3892b1dfa3d77d890023b951a0183d48d41abcbe2f2e.
Source revisions and exact no-run commands are in their role JSON records. Cargo
artifact fresh=false is required, rather than trusting a copied target path after
mtime-preserving sync. The initial red binaries remain separately preserved.

## Coordinator invocation and owed gates

Only root runs the heavier campaign, after kiln admits **4CPU/8GiB/10GiB**.
No VM work is needed here; any later real-VM scope needs its separate20GiB admission.
Use a fresh unused evidence prefix. On the existing runner, with the corrected
fixture Python source synced into the private candidate checkout:

```sh
export MANAGED_OLD_BINARY="$HOME/managed-rpk-compat-evidence/corrected/old-role"
export MANAGED_NEW_BINARY="$HOME/managed-rpk-compat-evidence/new-role-corrected"
export MANAGED_PROCESS_EVIDENCE="$HOME/managed-rpk-compat-evidence/interop-NEW"
unset MANAGED_PROCESS_RESOURCES
python3 docs/research/measurements/managed-performance/process-compat/campaign.py
# Only after all16 compatibility cases pass:
export MANAGED_PROCESS_RESOURCES=1
export MANAGED_PROCESS_EVIDENCE="$HOME/managed-rpk-compat-evidence/resources-NEW"
python3 docs/research/measurements/managed-performance/process-compat/campaign.py
```

The first command checks16 cases/32 role processes: both server/client version
orders ×2/4channels ×normal/stall/both wrong-pin directions. It stops on failure
and reaps only its own children. Resource mode checks8 ordinary/stall cases; root
chooses/records bounded repetitions and interprets missing kernel charges honestly.

To rebuild without touching production files/pins, in each respective worktree:

```sh
export CARGO_TARGET_DIR="$HOME/managed-rpk-compat-target"
export MANAGED_PROCESS_BUILD_EVIDENCE="$HOME/managed-rpk-compat-evidence/build-NEW"
python3 docs/research/measurements/managed-performance/process-compat/build.py old
# Run with `new` in the candidate checkout, using the same new evidence directory.
```

[build.py](build.py) touches only owned fixture sources, requires fresh artifact JSON,
freezes the binary and records its source/fixture/manifest hashes. No existing frozen
binary may be overwritten. Verify matching role-source hashes before cross-version
runs. Run the ordinary corpus/current strict lint as root deems required afterward.
Full upstream/consumer gates, supported seven-target and actual VM-host matrix,
corrected complete interop and resource distributions remain owed. Experimental pin
adoption and any shared-listener architecture decision remain exclusively root's review.

## R01–R16 account

|Rule|Evidence / remaining gap|
|---|---|
|R01|Production read-only. Canonical failure handling retained; tests may assert. Corrected role no-run and strict clippy pass. Broader production implicit-allocation proof remains owed.|
|R02|Previous pinned certificate-type primary research retained at a875/upstream7cc9; no new architecture selected.|
|R03|No runtime constant/threshold/quota added. Fixture counts/allowances explicit and bounded.|
|R04|Fixture terminal enum assigned once; actual marker/deadline failure retained. Production states unchanged.|
|R05|Readiness/credit/EOF/process events; no new polling, cache or reconnect workaround.|
|R06|Roles hide test transport work behind existing APIs; coordinator build/campaign helpers are private.|
|R07|Comments explain private EOF, source freshness, measurement and missing-accounting limits.|
|R08|No new performance claim; corrected interop and resource campaigns unrun. Prior misses remain in parent measurement record.|
|R09|Actual libtest-glued marker and timeout symptom retained before fixture fix. No inferred product fix.|
|R10|Corpus, partial5pass/1fail, lint/compile failures/corrections and frozen artifacts retained; live corrected rerun owed.|
|R11|Linux amd64/glibc only; other seven-target/VM-host coverage unverified, Intel Mac unsupported, paused Windows ARM untouched.|
|R12|Existing public identity/TLS/Driver/managed/framing APIs reused. No host root, framework, crypto, installable binary or production adapter.|
|R13|Only integration fixture and measurement files differ from each baseline; no new production API/control protocol.|
|R14|Resource helper prepared but resource stage NOT RUN. No child-RSS-only or zero-allocation/leak-free claim.|
|R15|Newest coordinator-only profiling boundary honored after receipt; existing job preserved, then only light compile/lint checks. No children, agent interruption or main merge.|
|R16|Own private source/target/evidence only. No local builds. Mac fell to29GiB below30GiB floor; root informed immediately, local builds held, source/evidence preserved.|
