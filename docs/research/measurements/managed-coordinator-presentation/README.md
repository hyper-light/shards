# Finite UDP4 client presentation and closing target checks

Coordinator runs r511/r514 use source
`e1bf8e03445748472448b173a93ec386af958543` on Fedora amd64 KVM. This is a
single known-tuple **scripted responder**, with distinct Binary and Empty
datagrams. It establishes neither forwarding nor a production routing policy.

| Actual case | Strict result | Observed effect |
| --- | --- | --- |
| Connected, destination-only | 101 | `getpeername` and received peer expose the responder endpoint. |
| Unconnected, destination-only | 101 | Received peer exposes the responder endpoint. |
| Connected, fixed-original-peer | 0 | Both responses and original peer presentation match exactly. |
| Unconnected, fixed-original-peer | 0 | Both responses and original received peer match exactly. |

Each baseline/comparison child reaps, both workers join, all selected links
detach, and the scoped cgroup reports removed. There is no claim about a later
observation window. The diagnostic driver requires the two strict failures;
its exit0 does not make destination-only presentation compatible.

## Guard removal with actual effects

Root r514 independently removes five selected hooks, preserving the client
checks. Every removal produces strict101 and its corresponding actual witness:

- Connected Connect4 and unconnected Sendmsg4: the independent backend receives
  the actor's two additional datagrams directly.
- Recvmsg4 in each mode: received peers expose the responder endpoint.
- Connected Getpeername4: `peer_addr` exposes the responder endpoint even though
  the received peers still show the original endpoint.

Every byte-restored same-mode run passes0 with complete cleanup. The final
source SHA256 is
`7c83dc31708476f93c727190e7dd7b7b6d0ccf412c4b6ae584051a4b1a7974c9`;
the final tracked diff is empty. The driver restores in `finally` and advances
source mtime so a mutated artifact cannot serve as the restored result.
Assertions examine actual backend receipts and endpoint values, not just a
missing hook record or alert.

Exact executed drivers, source/build/executable identities, selected environment,
raw console, commands, status, observed records and patches are under
`root-managed-presentation-e1bf-v1/` and
`root-managed-presentation-hook-mutations-e1bf-v1/`. Their `results.json` files
preserve individual strict outcomes. All156 regular records were transferred
privately and checked by `MANIFEST.json`; `TRANSFER.json` retains archive68851B,
SHA256 `aa09dbb2a30b5349536f6525b9a56b1c3e345fbc69ece17ea4e94decc67c7c2a`.
The original archive and source remain retained. No executable bytes or user
credentials are supplied in this record.

## Combined closing matrix

Root r522 runs the complete `scripts/lint` entrypoint on exact combined source
`3f68f8a1325602a56b39d2b03607765a3194f982`: format, all seven supported
targets and the script's GPU checks pass0. This source includes the presentation
fixture, held host-exit proof and reviewed startup/API safety changes. It does
**not** include the later two-leg buffered relay comparison.

`matrix-3f68/` retains ten regular raw records, exact command/toolchain/source,
complete unmodified log and lint.exit0. Verified archive4107B, SHA256
`1bbe3bc863eaa1e4589c432c64f57859681dade6dd3d232b2b42afb00e78cf5a`.
The normal AWS-LC/Zig optional SHA3 compiler-probe fallback warnings are retained;
no compiler, TLS, trust or crypto verification is weakened. The owned Mac clone
keeps only identified untracked `.DS_Store` metadata in place and requires clean
tracked source; no source is ignored or global setting changed.

## Scope and policy

The experimental kernel/config, explicit caller-selected test envelopes and
original endpoint predicates remain as recorded by the source fixture and raw
inputs. They are not a new product pin, learned production constants or evidence
that every route is mediated. Backend source transparency, generalized reverse
state, mandatory flow custody, grant/revocation closure, IPv6/multi-flow coverage,
aggregate/kernel accounting and comparative cost remain open. Windows ARM stays
paused. Compilation is not actual hardware coverage.

Policy receipt: coordinator at main42e3f758, unchanged file revision f5cec365,
R01–R16 SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
R01/R04/R06/R07/R13 source/state review and R02/R03/R05/R12 researched
mechanism/ownership/wait choices are recorded in the linked
[presentation source record](../managed-network-sock-addr/PRESENTATION.md).
R09/R10 guard effects/restoration and R11 target checks are the actual records
above. R08/R14 performance and full hardware/resource proofs remain owed.
R15 campaigns/review/merges stayed coordinator-owned; implementers supplied
scoped source/light checks. R16 preserves original source, evidence, caches and
paused-VM instruction without host privilege changes. No phase/goal completion,
whole-program panic freedom, leak freedom or 4–5× claim follows.
