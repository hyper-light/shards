# Typed memory.max Reader continuation

Role implementer `01a1c5be192972d8b05dc7c207cc6fca`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Current policy/base main
`a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
FULL AGENTS/R01–R16/evidence/CLAUDE/plan/current global handoff read before code;
receipt key `managed-otel-memory-limit-policy-read-a920-v1`.

New sparse worktree `/Users/adalundhe/Projects/shards/.claude/worktrees/managed-otel-limits`,
branch `p/managed-otel-memory-limit-v1`, exact a920 before this patch. OWN ONLY
`crates/init/src/metrics.rs`, `crates/init/src/metrics/tests.rs` and NEW
`docs/measurements/managed-otel-limits/**` (literal user-assigned evidence path).
Old af7/8338, admission92b937 and allocation/startup branches/checkpoints preserved.

Current patch adds `MemoryLimit::{Finite(u64),Unbounded}` and
`Reader::memory_limit(&mut self, &File) -> Result<MemoryLimit, Failure>` through
the original `tokens`, fixed held-directory `memory.max` openat/O_NOFOLLOW and
checked `number`. No new buffer/parser/module/dependency/numeric policy.
Finite zero remains distinct from max. Filesystem absence is existing Io(ENOENT),
empty contents existing Missing. Snapshot/codec/run/daemon remain untouched.
Four new regular-file cases cover finite/max, malformed/overflow/duplicates,
missing/symlink, actual page crossings and buffer reuse with snapshot reads.

Test-only standalone manifests reference actual Reader/codec/tests and actual
testdir source; they do not copy maintained implementations. Root rustfmt.toml
max_width110 is included in the private source closure. No local compile/targets/
caches. ONLY private Linux8fadc 2CPU/2GiB/4GiB<=600s/j2 sync:false is authorized.
No VM/controller/Collector/profile/audit/campaign/children/goals/rootcache/mainmerge.

ACTUAL first light attempt completed1 at cargo fmt before compilation. Toolchain
queries passed; exact cause is cross-directory path wrapper looking for otlp.rs
beside metrics.rs instead of in metrics/. Source package/raw commands/result/
exactstderr preserved locally in records/light-v1; original remote directory
`/home/adalundhe/managed-otel-limits-a920-light-v1/{source,target,cargo-home,tmp,out}`.
Remote raw archive3506B SHA256
`c1f05d8a6b6837eca452ed8f95cf681f3929fdfb81c02241c8a150252d90a7e6`.
Two readback requests held before start: zero idle CPUs, needs2. A changed actual
inventory showed3idle and original was then retrieved; ROOT subsequently assigned
read-only1CPU/256MiB/1GiB, used for V2 transfer. No test executable from V1.

V2 NEW private source/target at managed-otel-limits-a920-light-v2 completed0:
format/build/14Reader tests (0fail,0ignored,10unrelatedcodecfiltered)/strictClippy.
Only the private import layout changed: generate the exact2line lib.rs beside
the private Reader, using normal mod metrics lookup. No production header/module
or caller changed. No remaining private TMPDIR files/symlinks. Actual binary
7579032B SHA e3084f49f5627651915e52c5ec5af2fdecab588c3dc95edbfb18136276665c36;
Reader0b00dbbe509a768c60a5c24d2a7892d11a37dab5e9e32fef50ca6a5346cb4515;
tests d77238f28651ca84f9f7c059920e3c21649c462b206712cef19a9d3f98b733cb.
V2 raw11971B SHA91b536596d9f24322e4c90f8f1ac84d9552f7141133cdfa0c825ec7ebbf0cc23;
all36unique regular members/35originals verified. V1 all20/19 likewise verified.
Compiled owned bytes match local, codec/testdir original bytes unchanged.

Next: required small policy/whitespace checks, sign named paths through gits with
Claude Opus5.5 trailer, push own branch and send ROOT checkpoint immediately.
No additional repeat/broad tests are needed. No self-main merge/goal completion.

ROOT owns on-demand metric/Snapshot/codec/run/daemon wiring and active joiner fix,
actual full caller/combined/target/runtime/collector/profiling/guard gates. The
temporarily unused getter/enum is not feature readiness or an entitlement.
Literal max stays unbounded/absent at later metric wiring, never finite zero.
