# Coordinator KVM checks through 4d3f454f

Policy receipt: coordinator, c921a0b12b06d495b354907162e991376edffd41,
rules SHA256 1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.
Runner 8fadc1ec213332e16cb0d915a3fb913f, Fedora amd64, ordinary UID1000,
Rust1.98.1; exact per-run identities and commands are retained alongside logs.
All real VMs and the independent mutation below were run by the coordinator.

## Actual results

| Source / job | Result and limit |
| --- | --- |
| cbe3d027 / r161 | Full shards-remote test-release corpus: 19 units and 17 integration cases passed; doc tests had zero cases. |
| 1ef0b40c / r164 | Owner fixture RED: received 28/30 bytes versus expected 26/25. |
| 4c91a993 / r166 | Controlled bytes identify the canonical inner ABI Open record, including member/channel and label. This was a fixture decoding defect, not evidence of product line rewriting. |
| 5342489c / r168 | Owner output: 3 passed, 0 failed, 1 existing helper ignored. Four channel byte matches, held last-data credit, independent main/neighbor progress, typed invalid-scope/generation refusals, FD12→12. |
| cab396ce / r173 | Actual guest argv includes `reboot=k` and `pci=off` after the intended probe arguments; capability fixture fails. |
| 4d3f454f / r182 | Two host tests passed, exercising 13 actual boots: capability plus 12 IPv4/IPv6 TCP/UDP/connected-UDP binary/empty cases. Finished in 27.45s excluding compilation. |
| 4d3f454f with old append restored / r185 | Capability test cargo101, actual extra init args and `SHARDS-TEST FAIL`. Byte-identical source restoration and forced rebuild: test passed. |

The owner fixture now interprets canonical capture records rather than treating
their envelopes as raw stdout. Outer encoded transport cursors and inner data
offsets are distinct. It acknowledges Open/intermediate Data to permit progress,
keeps the final held Data unconsumed and waits for a typed closing response.
The metadata records encoded accepted/consumed positions. A later numeric-log-only
change at37aa was not rerun in r168. Independent scope/generation/accounting
mutations and hard aggregate/parent/kernel accounting remain owed; these bounded
passes are not a P3 completion or leak-freedom claim.

The network cases use the experimental kernel SHA256
97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae and built-config
SHA256 fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d.
The coordinator built these in r122; see ../managed-network/kernel-build.
The fixture reports the config association separately and does not establish it
from an untrusted payload. No production pin changed. Native socket matches and
IP_TRANSPARENT capability do not prove an nft hook or mandatory mediation.
Actual TPROXY routing, backend/relay distinction, UDP return semantics and the
full route ledger remain unverified.

## Replay and source protection

Root owns `~/managed-capture-check` and `~/managed-capture-target` on that runner.
Use the exact committed scripts in ../managed-network/guest/run.sh and the
retained source/identity records, private empty DOCKER_CONFIG and carried helpers.
r182 allocated 4CPU/8GiB/20GiB to the host job and explicitly selected test inputs
1vCPU/256MiB, boot120000ms and socket30000ms. Those are experiment inputs, never
runtime policy constants. The r185 command, exact old-append patch, source hash,
both logs/statuses and verdict are under managed-kernel-argv-4d3f-mutation-v1.
The original failure must be observed, not merely a compiler error or lost alert.

Transfer archive SHA256
c740e7f1f4b391a958591903766c3e13b05e2b18d832d8f701cdb0d409dd31c5,
43789 bytes/107 regular files. Paths were checked relative and hashes verified
before extraction. SHA256SUMS covers retained raw bytes; terminal CR/trailing
spaces in original logs are preserved. Private keys, guest binaries and images
are not included. No source/cache was deleted to obtain these records.

## Rule account and remaining gates

R01/R04/R06/R07/R13: this batch records checks and a restored experiment, not new
production behavior; the checked kernel-argv helper's detailed account is in
../managed-kernel-argv. R02/R03/R05/R12: underlying Linux parsing was reproduced
and fixed at its existing assembly seam; no new cache, polling policy, installed
binary, authority, crypto rule or resource default is introduced by this record.
R08/R14: no paired performance/profile claim or 4–5× result; all such costs remain
coordinator work. R09/R10: actual red/green and independent original-behavior
mutation with raw logs, source restoration and exact commands above. R11: these
are amd64 KVM results only; real HVF/WHP/arm64, final combined matrix and broader
platform behavior remain owed. R15/R16: coordinator-only heavy work, exact admitted
envelopes, protected source/targets and retained evidence; paused Windows ARM VM
was not touched. All managed-plan requirements outside these observations remain
open until their own evidence gates pass.
