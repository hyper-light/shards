# Finite client byte ceiling: actual receipt and guard removal

Policy cbd392/df2acf, unchanged main33df; implementer + assigned adversarial
relay tester. This is focused cfg(test)/driver coverage of the existing byte
ceiling, with no production pump change. The single shared pump remains
SHA256ab2ac5712e25dcd7210bdde287f126eda3c8b96ed2101c93c2055629d6fe5951.

r99, private `~/relay-tcp-forward-cbd3-light-v11`, passes GNU/static-musl
library and cfg(test) builds0 and two actual exact native tests0. An independent
backend sends8 greeting bytes and receives13 request bytes before its Worker
joins with the actual accepted socket still held, both halves open. The client
has received that greeting. Only after the retained backend receipt does the
client send its caller-supplied extra byte58 and SHUT_WR. No synthetic readiness,
sleep, additional deadline, general notification framework or new manager is used.

The actual backend's next read observes EOF without an extra byte. The shared
pump returns typed `ExtraData { Client, offset:13 }`, retaining read/written13,
pending0 and extra_consumed1. Client local acceptance remains written14; it is
not misreported as13, nor as proof of backend delivery. Client read retains the
8-byte greeting plus actual EOF. Both endpoint halves close, owned workers join,
the original deadline still has positive remaining time, child FDs4→4 and parent
held pidfd exit/reap/FD4→4 pass; exact binary hashes are unchanged. These are
limited descriptor/owned-child witnesses, with kernel return `Unknown`.

r101, NEW `~/relay-tcp-extra-client-mutation-v1`, replaces **only** the client
direction's `limit: request` with the existing `ByteLimit::PeerEnd`. The strict
test returns101 after recording actual backend extra_hex58, the same13-byte
prefix, client written14/greeting8, and pump read/written14/extra_consumed0.
Backend observation happens before relay join, then closes its write half;
there is no timeout or missing-alert substitute for this forbidden-byte witness.
The fixture records flow, endpoints, positive remaining time and FD cleanup
before its strict assertion. All owned joins, child FD4→4 and parent pidfd/
reap/FD4→4 complete on this expected red.

`finally` restores exact shared SHAab2ac571, all private source hash differences
are empty, and named-file `git diff --no-index` returns0. This is a projection
comparison, not an empty HEAD-diff claim. Fresh restored musl binary then passes
six exact tests0: extra boundary, backend reset, client reset, both half-close
orders and held-backend receipt cancellation. Sources, mutant/restored binaries
and full raw logs remain retained remotely.

| Artifact | SHA256 |
| --- | --- |
| Formatted guest fixture | e7a74d6e4c246ed3c5ffa3a3555e0435020fc4e9b40d83117156cd0b69e05089 |
| r99 static-musl binary | 15ef17cbc5e6526062ca8ff192cc8f1adc496f4903767a13560a344d71f295e2 |
| r99 GNU binary | eff2812feeacb5c1bd8f068cc6c985c22118b5f5a4b38d1074c285e9e09b7adb |
| Frozen mutant binary | 56260a6cdcc311cd54c02abfc04ce0045087659d4b0d3944033c8937e8f07929 |
| Frozen restored binary | 7b3e6319f1a6845a10d4831a69bf6250a214290335cc8e0a78b667fee169f270 |

[native.sh](native.sh) and [mutation.sh](mutation.sh) retain the exact submitted
commands. Native reproduction requires its recorded source-packet variables;
all roots refuse overwrite and require a NEW owned path/fresh admission.
`native` and `mutation` retain input/source/binary identities, build logs, parsed
observations and results. [raw-storage.json](raw-storage.json) maps losslessly
compressed stdout originals by exact byte length/hash; final libtest blank lines
are preserved. Rust1.98.1/libc0.2.189, Fedora44/Linux6.19.10 amd64, fresh
1CPU/1GiB/2GiB per scope, case30000ms/parent45000ms, one test thread; no VM,
namespace, grant or protected source/cache mutation in this batch.

R01/R04/R06/R07/R13: focused test assertions, typed original fault/progress,
held owned socket and explicit actual EOF/extra outcomes; no production edit.
R02/R03/R05/R12: existing researched byte-limit/native-read contract, caller
slots3/5 and explicit bounded payload/time inputs, one shared pump/Worker,
no cache/periodic probe/new authority. R09/R10/R15: actual guard-removal effect,
exact restoration and six controls, retained independent byte/cleanup witnesses.
R08/R14: no performance, allocation, fault or hard-budget claim; ROOT profiles.
R11: native amd64 GNU/musl only; KVM interception/reset, other targets and full
normal caller/matrix remain unverified. R16: no local target/cache/build, no
protected root or paused WinARM access, and source/evidence/binaries preserved.
Physical/kernel hard limits and mandatory all-route mediation remain open.
