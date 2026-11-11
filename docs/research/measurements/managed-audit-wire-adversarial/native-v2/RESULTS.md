# First native full-FUSE audit fixture: one actual pass

Candidate `0f8c9e0db375388b31f0580b189478a97f5df77c` ran ONE selected ordinary
native case successfully. It used the real shared FUSE frame functions,
`fs::answer`, `Server::audited`, audit codecs and held files. No mount, VM,
helper, peer/tool operation, fault, mutation or second case ran. This is scoped
P7/P9 evidence; management P0–P13/G01–G25 remains open.

## Source and original inputs

ROOT reviewed c722's883-line leaf; its SHA256 remains
`75427cca006a347f7b6a02ef88203e3b947db3d5f74157192001d1dc097e8a00`.
The minimal cfg(test) include is the only VMM lib token. ROOT then explicitly
authorized the exact three-file ba78 constructor patch in this private branch.
[constructor-composition-v1.json](../constructor-composition-v1.json) records
every old/new hash; all three resulting files equal signed ba78 byte-for-byte.
No active audit-writer source or unrelated startup/dispatcher change was adopted.

The original authoritative corpus is source
`6b86176f7354c4b9655b1a70c5671b1f04068f66`, now published by its owner at
`5da89cb87d2cf8207706c4c88c945b55b708a1ab`. Its exact archive SHA256 is
`7b14ede379a4679e4bb3269c40928dda6139358b3fb6342fc4767a4c6404ad46`;
outside corpus-manifest SHA256 is
`804f3b66b34565517f88caf43a1b48ad0102eaede0e15348b548df0a9eaa6698`.
Locally and on the native runner, [verify-corpus.py](../verify-corpus.py) checked
all389 original payloads plus manifest390, exact364 corpus files, every safe unique
regular path/length/hash, and inside/outside manifest equality. No re-emission.

Selected original member1/3 Send bytes retain broker request1. The actual Journal
first lease is separately call0; FUSE unique IDs are1..24. Both selected datasets
have actual wide intent213B, maximum844B across40 dense terminal shapes, ordinary
intent139B and terminal283B. The leaf derives these from unchanged files. With
actual ABI header118 and slot overhead40 it derives one aggregate B=1333, never
B per member. Synthetic codec Completed/KernelAccepted fields are not live
semantic tool/peer facts.

## Private native dispatch and gates

POLICY_READ current main `0c598386ca1f7c65c6e0634dba77983e4abffff2`, rules
SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
full policy/evidence/CLAUDE/plan/handoff and required closing records reloaded.
Role implementer / assigned adversarial; ROOT readback receipt9320.

Runner `d1323898b255a3ed435eb83fa0f7b0cf`, macOS26.4 ARM64, rustc1.99.1
`58cf8f9c35b355199727627d07ae34a4e2846d67`, LLVM23.1.1. Exclusive scope
`/Users/adalundhe/managed-audit-wire-native-v1` owns source, target, cargo-home,
input, build-records, artifacts, controller, TMPDIR and native-records. Normal
admission was2CPU/2GiB/4GiB, j2, lockf `/tmp/g121-mac.lock`, Mac floor30GiB.
These asks do not establish measured hard parent/kernel peaks.

Original600s scope was conservatively anchored BEFORE the first runner attempt:
epoch1794366367.300, absolute deadline1794366967.300. First cec preflight observed
27,178,512,384B free and stopped BEFORE creating any source/scope/build/case.
[first-floor-failure.json](first-floor-failure.json) preserves that receipt.
The same absolute deadline continued on normally admitted d132; no resource ask,
guard or timeout was relaxed. Its initial free space was2,341,907,615,744B.

The create-exclusive private source package is1,579,854B, SHA256
`c11862f9d9b8c40dbab9125380a4b61aab9dcc15ea6cf71e20207808348c9c31`.
All176 payloads were verified before compilation. It contains the actual eight
dependency-closure crates and policy inputs, with no stub. Product crate bytes
equal candidate0f8. Packaging-only workspace member/default-member selection and
removal of unused AWS-LC/QuickJS patches are explicit; original root manifest is
retained. Cargo's actual lock prunes absent packages; all three retained external
package source/version/checksum identities equal the original lock. There is no
HVF runner/codesigning hook: the native executable runs directly. Undispatched
packagev1 is retained separately; v2 only adds corpus verification/input routing.

| Actual gate | Result |
| --- | --- |
| Full VMM libtest caller compile, test-release ARM64 native, no-run | 0,84.576s |
| Policy checker | 0 |
| Frozen executable exact ignored test list | Exactly1 test |
| Full VMM lib/tests strict Clippy `-Dwarnings` | 0,5.018s |
| ONE exact selected native case | 1passed,0failed,0ignored; no retry |
| Offline raw/source/hash/byte/lock verifier | 0 using installed Python3.12 |

Raw command/result files are inside [raw-native-v1.tar.gz](raw-native-v1.tar.gz).
Build commands are `cargo test -p shards-vmm --lib --profile test-release
--target aarch64-apple-darwin -j2 --no-run --message-format=json` and
`cargo clippy -p shards-vmm --lib --tests --profile test-release
--target aarch64-apple-darwin -j2 -- -Dwarnings`.
Frozen executable SHA256:
`a502bd28e1e55f27ff204788c3737fe148c5ceb5441e6f9287f56e15ba8cea30`.
The exact filter is
`managed_audit_wire_adversarial::full_fuse_frames_share_one_retained_audit_budget`,
arguments `--exact --ignored --nocapture --test-threads=1`.
Controller dispatch was recorded BEFORE actors at1794366661.207; receipt9332
informed ROOT. Case work30s +cleanup15/hard45s, no runtime retry or extension.
Controller elapsed0.059822s and leaf42.136ms are one observation, not performance.

## Actual effects and cleanup

All24 complete request/reply pairs are retained with matching unique IDs and
outer errno0. Rust independently decoded the typed audit replies. Reserve and
both commits, plus intentional duplicate Reserve/CommitIntent/CommitTerminal/
Close controls, produced the expected states/cursors/accepted counts/remaining0.
The same read-only held descriptors captured whole-file snapshots throughout.
Duplicates did not change those bytes or debit a second slot. This does not count
identical overwrite syscalls.

After Reserve, member1 retained1215B and member3 its unchanged118B header. Only
exact intent/terminal data and their committed8B prefixes changed. Close retained
the full charged extent/slack. Member3 Reserve returned typed Empty/Exhausted,
then Identify returned its own Empty/None, both remaining0. No files-only member2
journal appeared. Final held lengths sum to1333; names/dev/inode/nlink remain
the same. RELEASE did not refund or alter the retained bytes.

`sequence_result=Ok(()) attempted_requests=24`; owned client end/drop preceded
`answer_join=Ok(Ok(()))`. Both exposed stream FDs, witness7/8, and Server's root FD
reported EBADF. Owner TempDir deletion then observed actual root absence. All
these receipts preceded the final assertion. Hidden writer/kernel populations
were not independently counted. Actual TempDir soft FD preparation10240→245760,
hard unchanged, is retained; ROOT explicitly allowed this own-process behavior
with a new empty private TMPDIR. It did not change guard entitlement.

## Immutable raw and verification

Native archive16,401B SHA256
`fed4897abc3072942c88e2bc07b158ea3b07ce9eb7640f014a1b4445e57403f0`:
**114 original payloads +manifest=115 total members**. Outside manifest SHA256
`92e39f67d524766e2e0f8d42ebd734a4261d9adebffbeb140a2ee810791edef0`
equals the embedded one. [verify-native-records.py](../verify-native-records.py)
independently verifies every safe unique regular member/hash, source package,
24 frame pairs, duplicate snapshots, unchanged neighbor, aggregate extents and
actual cleanup receipts. Invocation: `/opt/homebrew/bin/python3.12
docs/research/measurements/managed-audit-wire-adversarial/verify-native-records.py`.
The first system Python3.9 invocation lacked tomllib; its actual failure is kept.
A too-large local record-save tool input was rejected before writing; ordinary
smaller saves succeeded. Both mechanical failures are separate from the native
case and preserved. No original raw or failed receipt was normalized/overwritten.

Source/target/cargo-home, frozen executable, corpus, original corpus record tree,
TMPDIR and package/transfer inputs remain on the runner. The raw manifest lists
these exclusions explicitly. No active cache or user evidence was deleted.

## R01–R16 and owed ROOT work

| Rule | Scoped proof / remaining limit |
| --- | --- |
| R01 | cfg(test) assertions; checked field/extents and propagated I/O errors. Real full caller strict lint0. Exact owner constructor selection, no speculative product fix. Whole-repository panic proof unclaimed. |
| R02 | Reviewed actual ba78/8ac APIs and pinned FUSE layout/alternatives in proposal/frame contract; original actual encoder corpus verified. No new architecture selected. |
| R03 | B derives from actual codec/ABI sizes. Call0/request1/FUSE uniques distinct. Time/resource bounds explicit fixture inputs, no production literal. |
| R04 | Actual typed reserve/write/commit/close/refusal transitions, duplicate/cursor/accepted/retained assertions. No lifecycle or semantic effect inference. |
| R05 | Existing one frame reader/writer/answerer/Server/ledger, no second parser/manager/cache/queue or polling. One absolute work deadline; actual notification/I/O behavior retained. |
| R06 | Exact focused fixture helpers and24 serial requests; no added production branch complexity. |
| R07 | Comments explain independent IDs, corpus provenance, raw authority and cleanup uncertainty. |
| R08 | No comparative performance result or4–5× claim. ROOT owed. |
| R09 | Tests an established contract; no new production defect/fix from suspicion. First floor/interpreter/tool failures distinguished from product results. |
| R10 | Every source/input/build/dispatch/binary/raw hash and all actual requests/replies/snapshots retained; no case rerun. Meaningful mutations remain ROOT owed. |
| R11 | Actual native Mac ARM64 only. All7/other hosts/libcs/real VM combination gates unrun here, ROOT owed. Paused WinARM untouched. |
| R12 | Shared VMM/fs/IPC/codecs, no OS/backend duplicate, extra installed binary or security change. |
| R13 | One new leaf/minimal include; only explicitly authorized exact constructor patch adopted. No broader startup/writer redesign. |
| R14 | No profiling/benchmark/allocation/fault/physical/RSS/CPU win claim. Parent/kernel cost and retained hidden resources unknown, ROOT owed. |
| R15 | Assigned adversarial bounded owned case; exact readback/source/list/dispatch BEFORE actor. Signed named-path checkpoint, no children/goals/main merge. ROOT owns evidence review/integration/7/VM/fault/cost. |
| R16 | First floor refusal preserved; no deletes/guard bypass, new exclusive paths and lock, originals/frozen FIN/held campaigns preserved. |

Serial byte/length checks establish neither physical quota nor durable storage,
concurrent fairness, stalled-I/O control progress, semantic effects, hard resource
rights, live producer activation or full management closure. ROOT owes combined
caller/source integration, seven-target/host/VM validation, actual storage fault/
crash/disconnect/revoke/mutation guards, producer authority and costs. The next
finite source proposal is separate; this passing checkpoint does not release the lane.
