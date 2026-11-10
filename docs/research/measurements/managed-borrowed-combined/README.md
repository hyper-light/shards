# Borrowed API composition: actual library correctness gates

Code candidate `51fa61cef6c2e0435e8549a6014c1024dc497fc0` is a signed, single
composition commit directly on main `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`.
Only signed `3b3817eea489d107fb9820f0f46c4fe429709fa3` and
`d086894864cd5b3152fbf17b355186f99824155b` were applied, sequentially with
`gits cherry-pick --no-commit`; no whole branch ancestry or other native fixture
was adopted. Root owns integration acceptance and the main merge.

POLICY_READ: implementer; current main cbd3928; full AGENTS, R01–R16
SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`,
development evidence, CLAUDE, managed runtime plan and updated handoff read.
Historical receipts in the selected commits remain historical. This record
renews their current-policy limits. Owned composition scope was API/afunix/Cargo
identity and new evidence here; no production semantic edits followed composition.

## Source and decision identity

[COMPOSITION.json](COMPOSITION.json) records all seven source/Cargo hashes,
the exact selected 92-path union and mechanical checks. Source and Cargo are
byte-identical to d086. No API/afunix/Cargo commits occurred between its eb5654
base and current cbd; the current main parser/runtime changes outside those paths
remain exact. Cargo package records add only triomphe 0.1.17 (checksum
`0d15f4f5898bcfc5d8a8e45ed30b2f108d710c1a2e5489dbf80e078f707181bc`)
and its shards-api dependency edge; no removed or otherwise changed packages.

The existing [3b implementation record](../managed-native-borrowed/README.md)
and [d086 immediate-mode record](../managed-borrowed-nowait/README.md) retain the
primary Rust/Winsock/Linux sources, established alternatives, refusal/allocation
decisions and actual baseline reds/greens. This composition makes no new
architecture choice. The shared Ready/Until/Unbounded handshake is retained:
borrowed zero makes one immediate attempt without wait/retry; overflow is Bounds
before identity/dial; positive absolute deadline begins before identity and covers
the handshake. Owning Guest zero/MAX behavior, existing Socket waiting/EINTR
semantics, typed parser/process handling and supported targets remain unchanged.

## Reproduction and actual results

[prepare-source.py](prepare-source.py) builds a small, exact API/ABI/afunix/testdir
source workspace using the original pinned 17-package closure and includes all
taken.rs parser test sources. Only the derivative workspace manifest is reduced;
the repository Cargo files are untouched. The tested archive was
`/tmp/shards-borrowed-combined-51fa-source-v2.tar.gz`, 132621 bytes,
SHA-256 `88de55575d66ceb5d385b2c25ba457e2278fd829513b6a019490ae99755ff95e`.
All [37 source hashes](source-hashes.json) were verified before build and again
after record transfer. The earlier local v1 package remains preserved and was not
used for these gates.

Kiln run r129 used ARM64 Mac runner `b24c5c951260dcdfd0972f2aeaf3cf51`,
private `~/managed-borrowed-combined-51fa-v1`, admitted 1 CPU / 1 GiB memory /
2 GiB disk. [run-checks.py](run-checks.py) is the exact driver, invoked with
`300 180 180`. [results.json](results.json) and the raw bundle retain every
command, result, outer deadline and selected artifact. Rust was 1.98.1; exact
[compiler](rustc.txt), [kernel](uname.txt), [installed targets](installed-targets.txt)
and [storage](storage.txt) records are retained.

| Actual command | Result |
| --- | --- |
| `cargo test --locked -j1 --profile test-release --message-format=json -p shards-api --lib -- --test-threads=1 --nocapture` | 43 passed, 0 failed, 3 existing ignored; exit 0 |
| Same command, exact `client::tests::a_patience_of_zero_dials_once` | Fresh process: 1 passed, 45 filtered; exit 0 |
| Same command, exact `client::tests::a_patience_of_max_dials_until_the_guest_listens` | Fresh process: 1 passed, 45 filtered; exit 0 |
| `cargo clippy --locked -j1 -p shards-api --lib -- -D warnings` | Exit 0 |
| `cargo check --locked -j1 -p shards-api --all-targets --target x86_64-pc-windows-msvc` | Exit 0; compile only |
| `cargo clippy --locked -j1 -p shards-api --lib --target x86_64-pc-windows-msvc -- -D warnings` | Exit 0; compile only |

The full library run includes actual owned attach/drop/stop, listener saturation,
stream/error/half-close, borrowed zero/overflow and deadline cases. Its ignored
entries are existing `child_a_client_that_crashes`,
`child_a_vm_process_serving_its_vsock`, and
`the_cost_of_a_refused_dial` (measurement). No extra ignore/skip/warmup was added;
the child entries are invoked by their existing parent test. No benchmark ran.
No OOM occurred in these ordinary cases. Mac allocation-fault injection is absent
from this inventory: **MR-OOM-MAC-1 remains open**, owned by the separate cold-sync
lane; this pass is not a cold allocation-refusal result.

Cargo reported the selected API test artifact `fresh=false` on the initial build,
with the exact private manifest association; its saved binary is 1649832 bytes,
SHA-256 `f1960cc491b5805d9a6eb6946bab0affa5ae8b7e36bfdbdf3c709951d360f9b8`.
Both extra fresh-process checks deliberately reuse that same selected binary
(`fresh=true`). [artifact-hashes.json](artifact-hashes.json) retains each copy.

The immutable [raw bundle](raw/light-records-v1.tar.gz) is 12251 bytes, SHA-256
`20ba31af9333afa0fb9ddd60d2cf34f25d34011cc5a175b65ee45cac81284dad`:
22 regular members, including 20 individually verified original records and two
generated manifests. Safe-path/regular-file checks and all20 member hashes passed
after transfer; raw logs were not normalized. The source manifest matched the
local submitted bundle exactly. Remote sources, target, Cargo home, binaries and
raw files remain preserved.

## Resource effects, cleanup and limits

All six commands exited within their finite outer budgets; none required
cancellation. Each used a new process group. One bounded post-exit process-group
observation found no matching rows after each command, with ps exit0; this does
not prove all escaped descendants, universal reaping or kernel resource return.
Driver NOFILE remained `[10240, 9223372036854775807]` before/after. The existing
fd-limit child fixture changes only its own process limit, not host settings.

Actual free space was 94803128320 bytes before driver execution and 93839982592
after; private storage was 135384 KiB. New private Cargo home/target were required,
with one build job and no root cache access. The protected root Mac clone/cache,
all earlier private evidence and paused Windows ARM VM were untouched. No VM
boot, OOM/handle campaign, profiling, benchmark, sweep or production workaround
occurred. No native Windows execution or other supported-target gate is claimed.

Full cargo fmt check and policy checker passed on composition. Source-only
staged whitespace check passed. The full selected-path whitespace check returned
2 for four original3b raw evidence blank-at-EOF warnings, retained verbatim in
COMPOSITION.json. That result is not relabelled a pass or waived globally.
New evidence-only checks are in [closing-checks.json](closing-checks.json).

## R01–R16 accounting and remaining acceptance

| Rule | Applicable evidence / limit |
| --- | --- |
| R01 | Production source unchanged from selected candidate; typed fallible core, static allocation refusal and denied panic lints retained; strict library Clippy passes do not prove universal panic freedom. |
| R02 | Selected primary-source and alternative records linked above; source/Cargo compatibility checked; no new architectural choice. |
| R03 | No new production literal; finite fixture bounds are caller inputs, no runtime tuning. |
| R04 | Existing explicit wait modes, typed refusal/identity/deadline transitions retained; current library cases passed, full managed lifecycle remains open. |
| R05 | Existing single handshake/Socket/afunix seam retained; no new wait, cache, manager or polling loop; d086 wait-source review remains applicable. |
| R06 | Composition only; no added production branch or control complexity. |
| R07 | Existing rationale/safety comments unchanged; this record states actual command limits. |
| R08 | No performance comparison or 4–5x claim; ROOT comparison remains owed. |
| R09 | d086 feature-contract baseline reds/after evidence retained; no defect fix inferred from composition or an ordinary suite pass. |
| R10 | Exact sources, driver, raw logs, binary identity and result hashes retained; no new mutation run, selected guard mutations remain revision-specific. |
| R11 | Native ARM64 Mac + Windows x86_64 compilation here; all seven-target and supported real-VM acceptance remains ROOT-owned; Intel Mac unsupported and paused ARM VM unverified. |
| R12 | Shared existing implementation retained; only previously reviewed narrow platform adapters, no new installed binary or authority. |
| R13 | Only two selected checkpoints composed; source-identical d086; new evidence does not add an implementation layer. |
| R14 | No profiling/benchmarking/allocation-free/leak-free claim; ROOT owns measurements, cold fault issue remains open. |
| R15 | Explicit assigned light/native correctness scope, admitted private resource envelope; no child agents/goals/main merge; signed source and evidence checkpoints returned to ROOT. |
| R16 | Actual disk floor preserved; no source/cache/evidence deletion, protected paths untouched; private artifacts preserved. |

ROOT still owes final source/evidence acceptance, integration/main decision,
full supported-target/VM/fault/mutation/handle/resource gates and comparative
measurements as applicable. The managed runtime plan remains open.
