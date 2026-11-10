# Native own-cgroup Reader/codec integration

This is one read-only observation of the disposable fixture process's own Linux
cgroup, using the exact c298 Reader, codec and official Rust reference. It is
not a Shards VM, cap-transition, Collector, performance or entitlement test.
ROOT adoption of c298 and its whole-caller/Collector/profile/matrix gates remain
pending. All original source, raw failures and frozen artifacts are preserved.

The original archive has now been retrieved under normal admission and EVERY
53payloads+one manifest verified; see [RAW-VERIFICATION.md](RAW-VERIFICATION.md).
The pending-raw paragraphs below preserve81632's historical checkpoint status.
Live finite/max cap reads remain UNVERIFIED.

## Actual result and limitation

The corrected private run returned0 for all13 command phases, including the
product-locked build, focused resolver test command, strict binary Clippy and
**one actual native source invocation**. The complete native OTLP request decoded
through official Rust types equals the request built from the SAME actual
Snapshot by the unchanged official reference, including all legacy fields,
timestamp, identity inputs, limit states and diagnostics.

Both actual Snapshot cap fields are **Failure::Io(ENOENT2)**. Their six state
indicators remain unavailable, with numeric limits omitted and existing typed
diagnostics preserved. The live finite and symbolic-max paths remain UNVERIFIED.
This passing semantic/error-path check does not establish readable configured
caps on this runner. No other cgroup is selected and no missing source is retried.
The exact kernel files and resolution record remain the authority for what was
visible; no guessed underlying cause or production fix is introduced.

The fixture's own FD checkpoints were `[0,1,2,3]` before and after, including the
temporary enumeration descriptor. This is a bounded own-process FD observation,
not a whole-population/parent/kernel or allocation/leak-free claim. The private
TMPDIR reported no remaining files/symlinks. Synthetic fixture run and VM labels
are explicit64-hex command inputs ending1/2; generation0 is fixture input, not
an observed VMM lineage or management epoch. Scope::Workload is a fixture label.
The real sampled source is a cgroup aggregate, not exclusive process usage.

## Source selection and primary contracts

The fixture reads its own `/proc/self/cgroup` and `/proc/self/mountinfo`, selects
one matching cgroup2 mount by its root and mount point, opens only its own group,
and holds that File through collection and encoding. It checks cgroup2 fstatfs,
device and the held descriptor's own fdinfo mount ID against mountinfo, and
records the held inode. It does not claim an atomic inode-to-membership proof.
After sampling it compares its own membership and selected mount view again.
These are sequential observations, not atomic membership or quiescence proof.
The mount ID is checked while held; no permanent mount-ID identity is promised.
[Linux proc contract, sections3.5/3.8](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/filesystems/proc.rst),
[actual mount renderer](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/proc_namespace.c).

The supported fixture view is deliberately one unambiguous unified0:: record.
Multiple/malformed views, outside-namespace paths, invisible/ambiguous mounts,
changed identity and actual I/O failures produce typed limitations. The fixture
does not reinterpret `/..`, select an ancestor, resolve another PID, change a
namespace, create a group or write a controller. Kernel cgroup paths are emitted
without mountinfo's escaping; mountinfo's four path escapes are decoded separately.
Unknown optional mount tags are ignored. [Actual proc_cgroup_show/cgroup_show_path](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/cgroup/cgroup.c),
[namespace and controller semantics](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/admin-guide/cgroup-v2.rst),
[cgroup2 filesystem magic](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/magic.h).

Reader::snapshot performs its existing CPU/memory/current-tasks/IO reads and the
two cap reads through ONE page buffer and the SAME held group. Extra fixed-name
raw memory.max/pids.max reads occur separately afterward, with bytes or actual
errno retained; they are not asserted simultaneous with Snapshot. No independent
numeric cap parser/encoder, ancestor-cap calculation or runtime sensor is added.
Configured local ceilings still do not mean availability, admitted rights or
guarantees. This uses the [accepted c298 contract](../D229.md).

[source-provenance.json](records/source-provenance.json) records exact downloaded
v6.18 primary-source lengths/hashes and c298 input associations. This pins the
researched contract; it does not pretend the native runner kernel is v6.18.
Actual runner: Linux x86_64,6.19.10-300.fc44.x86_64, recorded in result metadata.

## Packaging failure, correction and resources

The first attempt's build returned101/E0583: directly path-loading metrics.rs
made its child otlp lookup search init/src/otlp.rs. The consequent unused-import
warning is retained too. Formatting/checking passed; **native_invocations=0**.
[Original compiler stderr](records/light-v1/build.stderr) and first archive
12389B/SHA25618a95a6dd1aed17782edb99ce6a611d92d7aff5dfd2a439ff25cadad9d6b72b1
remain unchanged. Every30 original payloads+one embedded manifest was verified;
the outside [inventory](records/light-v1/archive.json) includes the manifest row.

The correction changes only fixture packaging: lib.rs is generated next to the
PRIVATE copied Reader so normal `pub mod metrics` child lookup works, exactly as
in the established Reader light harness. Cargo points to that generated private
library. It re-exports Reader's own codec, so the unchanged reference sees ONE
Limit/Snapshot type, not a second native encoder. No actual repository file under
init/src is created or edited. [Rust module filename rules](https://doc.rust-lang.org/reference/items/modules.html).
The original source/target/out stay frozen; corrected output is fresh in attempt-v2.
There was no missing-cgroup retry, semantic fallback, assertion weakening or
production source change.

One owned remote scope on Fedora8fadc1ec213332e16cb0d915a3fb913f:
`/home/adalundhe/managed-otel-own-cgroup-c298-light-v1`, with first attempt and
`attempt-v2/{source,target,cargo-home,tmp,out}`. Each serial light invocation
requested the same2CPU/2GiB memory/4GiB disk/<=600s/j2, sync:false; these are
admission requests, not measured peaks or proof of hard cgroup enforcement.
No local compiler/Cargo or protected ROOT target/cache was used. No cache was
deleted. The single actual native observation ran only in the corrected attempt.

The product Cargo.lock is included byte-exact:45993B/SHA256
1ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133,
pinning libc0.2.189. Fixture Cargo.lock derives from the existing official
reference lock, replacing its root/unused encoder entry and libc entry with the
product version/checksum. Its --locked build passed; no production dependency or
lock changed. Official proto0.33.1/commit372ebbc74d4dba71bb9820732f57f7316b4e7a1e
retains gen-tonic-messages/metrics/with-serde, serde_json1.0.151. The same used
generated subset/older embedded schema compatibility remains c298's contract.
This is generated-proto encoding/decoding, not a complete SDK pipeline.

## Exact execution and current raw status

[run.py](run.py) records every argv/cwd/environment, separate original stdout/
stderr, source/lock and before/after binary hashes. [launch.json](records/light-v2/launch.json)
retains the actual exclusive bootstrap. Reproduce only with a NEW fresh private
source/output directory and normal admission, then run
`python3 source/docs/research/measurements/managed-otel-limits-export/native-own-cgroup/run.py`
from that private parent. The script does not retry a missing cgroup or invoke a
profile/VM/Collector. It retains typed source/identity limitations and FD results.

The corrected16-file source archive is44435B/SHA256
e99653803a9c568e01e03b1b19980f2fb49002d711124bea42c2233741543779;
one derived151B private wrapper is additionally identified in the actual compiled
closure. Actual GNU native artifact7494944B/SHA256
a28c8e23ea6d9c7daccf5cf3af7c6d6d61bc6fe48d88aa55e90d063466c18c18;
resolver-test artifact9464104B/SHA256
8eae2efb9f696e10f36b5f9e35879d4988ee2b53a09a28ce333173ee41eb99c1.
Both artifacts' before/after hashes match in the returned metadata.

PENDING RAW VERIFICATION: the corrected archive is reported28609B/SHA256
75766b677283916248de29e7aaac209236db41c6ee7ec034082c749fa9bf0e8d.
Its honest separate read-only1CPU/256MiB/1GiB fetch was refused before start at
0idle; [original refusal](records/light-v2/transfer-denied.json) and exact input
are retained. No read envelope/timeout change or guard bypass occurred. Detailed
libtest counts, original cap raw/error files, selected mount details and EVERY
member verification remain pending normal read admission. Returned result metadata
is explicitly a decoded copy, not original raw JSON bytes. Current owned Rust/
manifest/lock/driver bytes already match returned post-format identities; c298
Reader/codec/reference remain byte-identical. No pass is inferred for unrun
finite/max transitions or downstream consumers.

This signed source/test-metadata checkpoint records that read hold rather than
waiting to publish the reviewed source. [local-source-association.json](records/local-source-association.json)
verifies EVERY member in BOTH16-file source archives and all final compiled
source associations. Corrected original raw retrieval/verification remains
assigned as a follow-up before an adoption/readiness claim. The required policy
script returned0, with exact policy.* command/output retained; local free space
was190909906944B. No local compiler or Cargo invocation occurred.

## R01–R16 and owed ROOT gates

| Rule | Scoped proof and remaining limit |
| --- | --- |
| R01 | Frozen fallible Reader/codec, RAII files; new fixture returns typed I/O/format/visibility/identity failures, checked module/identity parsing and documented FFI ownership. Actual strict bin Clippy0, not an OOM/whole-operation panic proof. Assertions are only in pure tests. |
| R02 | Pinned primary proc/cgroup renderer/namespace sources, actual first compiler RED, unchanged accepted limit contract. No resource/export architecture decision. |
| R03 | Existing c298 metric/state literal table retained. Fixture0::/mount fields/escapes/magic/fdinfo derive from the linked kernel contracts; six indicators derive from2fields×3states. Resource/deadline bounds and synthetic identity inputs are explicit assigned/test inputs, not learned runtime policy. |
| R04 | Limit Result/cause retained; fixture limitations have explicit type/operation/errno/field/reason. Missing cap stays unavailable, not zero or unbounded. |
| R05 | One actual Reader/page/held directory and same codec/reference; two finite source-identity views, no poll/cache/ledger/sensor. No missing source retry/fallback. |
| R06 | Resolver handles only the required own unified view. Fixed-name raw reads and fixture metadata stay private; no generic resource framework. |
| R07 | Comments explain module layout, escaping, FFI ownership, fixture identity and non-atomic semantics. |
| R08 | No performance/competitive result or inherited ratio claimed. |
| R09 | Exact first E0583 and warning retained before the packaging correction; no speculative cap/caller fix. |
| R10 | Exact packages/locks/commands/failures/binaries retained; first30payload+1manifest fully verified. Corrected raw verification pending read admission, explicitly not passed yet. |
| R11 | Native Linux GNU amd64 only in this slice. New fixture musl/other hosts/VMs/seven-target gate unrun; old source gates do not certify this fixture. Mac ARM64 only, WinARM pause preserved. |
| R12 | Shared c298 Linux guest Reader/codec, official types; tiny private fixture wrapper only, no installed binary or production adapter/dependency. |
| R13 | One actual own-group Snapshot and full typed comparison. No duplicate native encoder, alternate exporter or API. |
| R14 | No profiling/benchmarking/timing/fault/allocator run. FD/TMP observations are narrow cleanup witnesses; parent/kernel and whole-path costs unknown. |
| R15 | Explicit light native read authorization, same private resources, no child/goals/audit/campaign/Collector/VM/matrix/main merge. Signed source/test-metadata checkpoint discloses the raw read hold; verification follow-up remains assigned before adoption. |
| R16 | Original/attempt-v2 source/target/raw/frozen artifacts preserved, no active cache deletion or ROOT/paused-VM writes; normal headroom refusals respected. |

ROOT owes adoption/review of c298 plus this fixture; actual managed caller/held
cgroup cap availability, finite/max/error/recovery and identity/cleanup guards;
official Go decoder/Collector, supported-target/VM matrix and mutations/restoration;
actual encoding/collection/transport/parent/kernel costs and hierarchical owner
policy. This live unavailable-field observation closes none of those obligations.
