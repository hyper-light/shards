# Native record retirement: independent-body ordering

ROOT coordinator, current main `28ba5083bd65a9a3bff4896921821cbbd4f1fee7`,
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
This new sparse worktree preserves all older sources, compiler targets and raw
records. Both audit holds and every paused/held campaign remain active.

The [retirement research](../../managed-native-retirement-frontier-research-v1.md)
requires accounting for an earlier separate body before certifying that its
first publication cannot happen. `ordering.py` observes the relevant OS ordering
without importing Shards source or inventing a second production codec.

The fixture sends two labelled control packets carrying real pipe read FDs, then
a later labelled cut on one anonymous nonblocking UNIX seqpacket connection.
It receives the packets in order, writes only the newer body, and observes the
older body still pending with its writer held. It then completes that older body
after receiving the cut. Real FD object identity, CLOEXEC, explicit close and
before/after controller FD snapshots are recorded. No child is spawned.

The textual labels and body strings are fixture data, not Programs IDs, canonical
wire tags, capacity constants or a selected retirement protocol. A successful
result establishes this OS counterexample; it does not establish actual helper
ordering, a live product defect, record reuse, audit completion or resource
return. No retirement rule or record population is selected from this sample.

Primary contracts: Linux [`unix(7)`](https://man7.org/linux/man-pages/man7/unix.7.html)
describes ordered seqpacket delivery and ancillary FD transfer;
[`pipe(7)`](https://man7.org/linux/man-pages/man7/pipe.7.html) distinguishes an
empty nonblocking pipe with a live writer from EOF. These contracts do not tie
independent pipe completion to a control-packet receipt.

Actual NEW execution completed0 on runner451397, Linux7.2.9-2-cachyos/x86_64,
UID1000, one observed thread. Scope:
`/home/adalundhe/Projects/root-native-retirement-order-v1-1794404338971`.
The request used a 2CPU/2GiB/4GiB admission envelope and one60-second outer
timeout. These requested resources are not a hard physical-memory reservation;
the actual process limits and affinity are retained in `recovered-linux/raw/identity.json`.

| Actual observation | Result |
| --- | --- |
| Control receipt | older, newer, cut, in that order |
| Older pipe when cut and newer body were received | writer still held; read returned saved EAGAIN11 |
| Newer pipe at that point | readable; exact fixture body received |
| Older pipe afterward | exact older body successfully written/read after the cut |
| EOF | observed only after both actual writers were explicitly closed |
| Controlled FD cleanup | all created/received FDs closed; original before/after snapshots equal |

All FIVE source/setup/raw files were recovered as safe regular files and
independently checked for exact lengths/SHA256. The executed7627-byte harness
SHA256 is `9186cafacc0890f6159f7af0ee8f0f7264149243768975bf9ecc7cf53c3325e6`;
it equals the local source. `VERIFIED.json` pins every payload, actual identity,
result and FD snapshots. `REMOTE-COMMAND.sh` and `TOOL-RESULT.json` retain the
exact command and original completed result. Artifact recovery was read-only;
the fixture was executed once.

Reproduce on Linux with an absent output directory:

```sh
python3 ordering.py NEW_OUTPUT_DIRECTORY
```

The observed control order agrees with the seqpacket contract, while the later
old-body completion demonstrates the independent publication obligation. This
rules out treating control receipt alone as that obligation's retirement proof.
The actual helper must establish a drain/retirement condition or keep the older
obligation represented. The experiment selects no production cut, wire tag,
record count or storage population. Actual helper/owner/caller composition and
bounded reuse remain implementation work.

## Policy and remaining proof

| Rules | This batch and explicit limits |
| --- | --- |
| R01/R03 | Fixture assertions/data only; no production code or runtime quota. Buffer lengths and ancillary space derive from the exact fixture messages and one native FD. |
| R02 | Existing frozen retirement/backing research, primary OS contracts and this real ordering observation; no production architecture selected. |
| R04/R05 | Trace distinguishes sent control, received FD, pending body, later data, actual EOF and explicit close; one readiness snapshot, no retry loop/manager/cache. |
| R06/R07/R13 | One focused question in a standalone harness; comments explain transport independence and the limits of FD snapshots. |
| R08/R14 | No comparative cost, latency, allocation-free, leak-free or4-5x claim. Elapsed runtime is method metadata, not a benchmark. |
| R09/R10 | Live OS counterexample with exact source, command, identity and raw trace. No product bug, fixed production guard or meaningful production mutation is claimed. |
| R11/R12 | One Linux x86_64 host; no pinned guest6.18, musl/arm64/Windows/macOS or actual VM result. Standard OS primitives, no second Shards codec. |
| R15 | ROOT ran one Python probe, which spawned no subprocess. ZERO Rust commands, Shards actors/helpers/VMs, cgroup/permission/limit changes or held-scope imports/dispatches. |
| R16 | New sparse source worktree and exclusive remote directory; older sources/raw/targets/binaries preserved. Actual kernel-reference/population return is unproved. |

Mechanical `scripts/check-development-policy` passed on this exact sparse
checkout with its required policy/matrix files materialized. Syntax and named
whitespace checks are separate source checks. Both audit validation holds,
paused WinARM and all other held/skipped campaigns remain unchanged.
