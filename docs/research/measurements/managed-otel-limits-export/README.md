# On-demand local memory/task limit export

ROOT accepted [the pinned contract](../../managed-cgroup-limit-otlp-contract.md)
and transferred the existing Reader/codec/fixture seams for this slice. The
Reader now populates actual local memory.max/pids.max results in Snapshot, and
the existing streaming codec emits exact integer Gauges with separate typed
state and existing diagnostics. No daemon/run/CLI/host fixture, cadence, endpoint,
dependency, cache, ancestor walk, admission rule or resource identity is added.
Whole-caller/Collector/live-controller adoption remains ROOT-owned and unrun here.

## Exact semantics

One pure `Limit::{Finite(u64), Unbounded}` lives at the codec seam, re-exported
by Reader. Snapshot retains `Result<Limit,Failure>` in memory_limit/task_limit;
the two fixed-name reads occur once each through the original held directory,
tokenizer and fallible actual-page buffer. The original scope/timestamp/canonical
run+microVM/stable-member/VMM-lineage semantics are preserved.

| Actual field result | Numeric Gauge | State | Diagnostics |
| --- | --- | --- | --- |
| Finite0 through i64::MAX | Exact integer, including genuine0 | finite | No limit error. |
| Unbounded | Omitted | unbounded | No error merely because the successful file read says max. |
| Failure(Missing/Invalid/Overflow/Io(errno)) | Omitted | unavailable | Existing actual typed cause and errno retained. |
| Finite above i64::MAX | Omitted without wrap/clamp/type switch | finite | Existing typed Overflow from numeric encoding; error.type stays overflow. |

The numeric instruments are `shards.cgroup.memory.limit`, Gauge `By`, and
`shards.cgroup.task.limit`, Gauge `{task}`. They report this cgroup's configured
kernel value, not effective ancestor capacity, available bytes/tasks, process
leaders, a reservation or entitlement. Reader full-u64 cases do not assert the
kernel accepts every u64 as a finite PID limit.

`shards.cgroup.limit.state`, Gauge `1`, contains all three fixed finite/unbounded/
unavailable Boolean indicators for **each attempted field**, with fixed point
attributes shards.cgroup.metric and shards.cgroup.limit.state. All six points
derive from the SAME original Snapshot results: one true1 per field, others
false0. A false indicator is not a zero cap. The numeric conversion does not
reclassify a valid large finite read as unavailable. No failed value defaults
to max or0; no stale marker/infinity encoding/deletion guarantee is introduced.

[D229.md](D229.md) supplies every new literal/width/cardinality derivation:
exactly six state points plus at most two numeric points per scope, with up to
two existing diagnostic points on failure/encoding overflow. These are source
shapes, not an invented performance allowance or measured allocation/kernel cost.
Collection remains non-atomic across files and scopes.

## Source and ownership

Branch parent is signed/pushed18d9cc88e70dbaee67419ca7198766d8a87a8b03,
p/managed-otel-memory-limit-v1. Policy/current owned-seam base is main
b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe; rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Implementer01a1c5be192972d8b05dc7c207cc6fca, coordinator01a148884a4770fdaf0111f3f792a248.
FULL AGENTS/R01–R16/evidence/CLAUDE/current plan/handoff and required0ae closing
record read. POLICY_READ key managed-limit-export-policy-read-b57-v1.
After context loss, all were read again from fetched current main
0342b85d9c808e3ced041a6c3ef0e9bf70438272 with unchanged rules hash; receipt key
managed-limit-export-policy-read-0342-resume-v1. The transferred source seams
are byte-identical between b57 and0342 before this slice; this is not a new
whole-caller gate or a rebase of the older branch.

The branch's older codec preceded the reviewed run/VM identity split. ONLY
transferred owned seams were first based on exact b57 source; original signed
source was preserved. [current-owned-base.json](records/current-owned-base.json)
records original/current hashes. No branch rebase or unowned run/daemon/CLI edit
occurred. Untouched old branch run.rs predates the new four-argument signature;
the actual ROOT b57 caller already has it. Standalone module compilation is not
a full old-branch or actual init-caller pass.

The standalone encoder still uses the real codec. The pinned official Rust
generated-proto reference still includes the SAME concrete Snapshot fixture and
retains actual decimal-format/parse f64 conversion and generated defaults. It
now constructs the matching limit/state Gauges through official types; complete
requests are compared, not selected byte fragments or post-processed equal bytes.
The original3scope fixture's derived shape is18 Metric descriptors/41 points,
with error counts0,6,2. Reference unit tests additionally cover the fixed typed
boundary corpus, both limit fields and all three actual scope variants.

Profile src/main.rs has ONLY six Snapshot input field additions versus b57;
no measurement/allocator/reporting algorithm changed. Both configurations are
compiled/checked only. **No profile executable is invoked.** Its old outputs
stay associated with their original source, not with this export change.

## Actual light results

First private attempt returned0 for all29 recorded command phases. Actual
scope: Fedora8fadc1ec213332e16cb0d915a3fb913f, NEW private
/home/adalundhe/managed-otel-limits-export-18d-light-v1/{source,target,cargo-home,tmp,out};
normal requested/admitted2CPU/2GiB memory/4GiB disk/<=600s/j2/sync:false.
These are requested resources, not measured peaks or proved kernel hard bounds.
No local build/target/cache or protected ROOT path was used.

- Actual GNU and musl Reader+codec each passed27 tests,0failed/0ignored
  (16Reader+11codec); their builds and strict all-target lint returned0.
- Actual encoder build, native payload/structured shape and strict lint returned0.
- Actual reference2 unit tests and1 complete-equivalence integration test passed,
  0failed/0ignored. Strict lint, three encoding modes and complete normalized
  semantic comparison returned0.
- Both profile configurations passed cargo check only; profiles_executed=false.
- The private TMPDIR returned no remaining files/symlinks, not an FD/population/
  whole-operation leak proof.

The original first-attempt source closure has29 files/46813 bytes, SHA256
03a28a41d67864723c62aeea9e4526c4893f6899dbe794c72cd057e43936878f.
The machine reported raw41172 bytes, SHA256
1d61cf2bcd43092016f0701d5a4bc5e0c368ec2ab2e0f803f1453bdf7c28a5f0.
[kiln-run.json](records/light-v1/kiln-run.json) retains the actual full receipt;
[returned-result.json](records/light-v1/returned-result.json) is a decoded copy
of the driver's final structured result, not a claim of original raw JSON bytes.

The first honest read-only1CPU/256MiB/1GiB raw retrieval was refused before start
because the runner reported0idle. Its [original refusal](records/light-v1/transfer-denied.json)
is retained. A fresh ordinary inventory later reported20/32idle; the SAME read
input/envelope/timeout was admitted and returned0. [transfer-retry.json](records/light-v1/transfer-retry.json)
retains input/admission/output identities, with decoded archive bytes preserved
instead of a redundant base64 payload. No guard bypass, local formatter fallback,
build retry or envelope change occurred.

[raw.tar.gz](records/light-v1/raw.tar.gz) contains **99 original payloads plus
one embedded manifest**. Every unique safe relative regular member was checked
against the original length/SHA256; [archive.json](records/light-v1/archive.json)
has100 rows including an explicitly labelled manifest row. [result.json](records/light-v1/result.json)
is the original raw member, byte for byte. [verified.json](records/light-v1/verified.json)
records the complete source/raw checks, actual counts and copy associations.
All29 original source-package members were also independently checked. Exactly
eight owned formatted Rust files were copied back only after matching both the
local pre-format input and actual post-format compiled source hashes. No private
wrapper or unowned source was copied. Earlier first-format failure, all19+1/35+1
historical payload/manifest roles and every earlier source/raw file stay preserved
at their signed/relocated associations.

The actual tools were installed rustc1.98.1 (48a229ceaefd4985c50990b14116b6d856af0985,
LLVM22.1.8), cargo1.98.1 (797e8a9bc), rustfmt1.9.0-stable and clippy0.1.98.
This harness explicitly selected the installed toolchain; repository rust-version
is an MSRV, not an enforced rust-toolchain pin. The raw command records retain
every argv, working directory, environment and separate stdout/stderr. Repeat
only in a NEW private admitted scope: materialize the exact29-file closure,
then run `python3 source/docs/research/measurements/managed-otel-limits-export/run-light.py`
from that private parent. [launch.json](records/light-v1/launch.json) retains the
exact original bootstrap/input command; [run-light.py](run-light.py) never invokes
the profile executable.

| Actual compiled source | SHA256 |
| --- | --- |
| Reader metrics.rs | 427146bd2d8838d647b281ea1cb58ae99b8bafdada8988022f499aab937fd7d0 |
| Codec metrics/otlp.rs | 0c7fc47a67b70dbbb16b94de4a0b43cd9bb2c6605a527f94e97b2af378c713da |
| Reader metrics/tests.rs | 7211b1570c857d374f4ca261755e86cf52f2eca1a4985ff944321c9b4d9141bb |
| Official reference src/reference.rs | 16b86e859d799b0b1006f2fb87afa7e022ff83e4d725999784b7f6e6e220d1f9 |
| Profile src/main.rs, input additions only | f4099923a3a891fa34852ff76b0fd98c9ef6904c6763919b61df3c9b7584086a |

Exact artifact paths, lengths and full hashes are in result.json: GNU Reader/codec
7637320B/328720fb9514cb22c89fc82ac65ad1eee41aedecc0330a2744328acc5e80a325;
musl7935320B/60e53ef4ea57dc83133e54ac6e3b8eb20264f5065436b3ad0866faa26fb7bf2d.
All four executed test artifacts had identical hashes before/after execution;
the encoder and reference CLI identities are retained separately there.

The same3scope corpus, extended with the limit fields, produces3resources/
18Metric descriptors/41points in each mode. Native payload10150B, generated
compact13099B and pretty34568B are
the actual fixed-fixture output sizes, not timing/allocation or whole-path cost
claims. Complete normalized receipts match SHA256
142d39088f0ef6ecb4c31e56143e63a3b40ceaf72a1d3bf273295475fbadc3ce.
The actual official Rust typed decoder also compares complete requests in the
boundary and three-mode integration tests. Generated JSON emits41 explicit
startTimeUnixNano="0" defaults; native omits them. These are valid unknown-start
equivalents, not an invented known start. Payloads were not post-processed to
manufacture equal bytes. Official Go decoder/Collector remains unrun here.

The required `python3 scripts/check-development-policy` returned0; its exact
command/source/output are retained in records/light-v1/policy.*. The local Mac
had195511066624 bytes free during that small read/check; no local compiler or
Cargo invocation occurred.

The first final source/link association check returned1 only because its disk-only
link check rejected the correct contract link omitted by sparse checkout. All
source hash checks preceding it passed. [final-check-first.json](records/light-v1/final-check-first.json)
retains the command and exact copied diagnostic excerpt, explicitly not an original
separate stderr file. Git identifies that contract as skip-worktree blob
fb473cc7767adcc58b6ea93ecdd7a58b3f8f8a4f. The corrected sparse-aware check returned0,
retained in final-check.*; no source/link change or compiler/test rerun resulted.
The first named-path staging also returned1 for the explicitly transferred
paths outside this sparse definition; stage-first.json records the command and
diagnostic summary. The three in-cone metrics files had staged. The same owned
paths were then staged with Git's explicit --sparse option; no reset, checkout
expansion or source change was needed.

## R01–R16 and owed ROOT gates

| Rule | Scoped proof and limit |
| --- | --- |
| R01 | New paths use existing fallible page allocation, held RAII files, checked integer conversion, bounded arrays and Write errors; no new production assert/unwrap/index/allocation API. Actual strict GNU/musl and reference/encoder lint returned0, not whole-operation OOM proof. |
| R02 | Exact pinned OTel/kernel/established exporter research a41, read and accepted by ROOT before implementation; fixed pure boundary/reference corpus. Live controller/exporter behavior remains ROOT's. |
| R03 | D229 table derives names, units, signed width, two fields/three states, Boolean truth values and fixture counts. No cap/timeout/cache/overhead threshold is invented. |
| R04 | Original Result<Limit,Failure> is authoritative for state; numeric optionality is only successful unbounded omission. Existing causes stay typed; above-wire-range finite stays finite. |
| R05 | One Reader/page/codec and one real generated-proto reference; no generic sensor/manager/ledger/new cache or polling. Existing on-demand actor/caller contract retained. |
| R06 | One existing numeric-observation pipeline extended by two fields; fixed state loop and existing diagnostic seam. No OS-specific copy or framework. |
| R07 | Type/D229 comments preserve local/TID/non-entitlement and serialization distinctions. |
| R08 | No performance/4–5x result or inherited baseline ratio claimed. |
| R09 | This is an explicitly approved new export requirement, not a speculative production-defect fix. First actual run returned0; original subsequent read refusal retained. |
| R10 | Exact source/current-base/argv/tool/artifact receipts retained; EVERY29 source member and99 raw payloads+1manifest verified, eight actual formatted source associations checked/copied. First read refusal, sparse-link checker failure and historical first-format failure retained with their limits. No old-path/source rerun or fabricated original-count claim. |
| R11 | Actual x86 GNU+musl module/light checks only. Actual full carried caller, remaining targets/architectures/VMs and live controller source unknowns remain unverified here; Mac ARM64 only/paused WinARM preserved. |
| R12 | Same Linux guest Reader across hosts, standard OTLP shape/official reference, no installed binary, production dependency or platform adapter change. |
| R13 | Required actual caller data now enters the codec; no parallel native encoder or speculative public parameters. |
| R14 | Six state/<=two numeric point cost disclosed, no timings/alloc/fault/profile run. Existing source conversion/default costs retained. Only compile checks touch profile source. |
| R15 | Explicit temporary path transfer; private admitted pure/light only. No child/goals/audit/VM/Collector/matrix/mutation/profile execution or main merge. Signed named-path checkpoint follows raw verification. |
| R16 | No local build/cache or protected-root/paused-VM writes/deletion; all own prior/current sources, targets, binaries and raw evidence preserved. |

ROOT owes source/adoption review, actual b57 whole-caller/carried-helper and host
assertions; real controllers, member/run/VM identity, timestamps/generation,
cleanup and finite→max→error→finite transitions; official Go decoder/Collector,
supported-target/VM matrix and applicable guard/restoration gates; actual costs
and induced parent/kernel attribution. Numeric omission is not backend deletion,
cap is not guarantee, and this slice does not close P0–P13/G25 or owner policy.
