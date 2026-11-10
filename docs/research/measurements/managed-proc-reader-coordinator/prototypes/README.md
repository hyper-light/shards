# Opened-file preferred-I/O initial reserve experiment

TEST ONLY, not a production optimization, supported product contract or selected
reader. New p/managed-proc-reader-blksize from immutable05f026690. Own only this
prototype directory; borrowed and pthread source/binaries remain untouched.
Implementer receipt: full canonical g121-main AGENTS/R01–R16/evidence/CLAUDE/full
managed plan/current handoff atf5cec365d2e435afae5e6f613c92d9d3759ef60f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root owns all comparison/profile/benchmark/audit/heavy work, decision and merge.

## Primary basis and explicit experimental restrictions

[Linux stat(2)](https://man7.org/linux/man-pages/man2/stat.2.html) specifies that
fstat uses the held descriptor. [stat(3type)](https://man7.org/linux/man-pages/man3/stat.3type.html)
defines st_blksize as the preferred block size for efficient filesystem I/O.
It is neither file length nor a guarantee about this procfs record's best size.
[Pinned Rust1.98.1 MetadataExt source](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/os/unix/fs.rs)
exposes blksize as u64 and delegates to the file metadata's st_blksize. Current
online Rust docs are1.99.1; the implementation basis and compiler here stay1.98.1.

The std-only read_preferred function queries **this opened File** once, converts
its actual blksize with checked usize conversion, then uses try_reserve_exact.
Root approved zero/unrepresentable hints as explicit InvalidData experimental
restrictions, with no guessed fallback. Metadata/read errors propagate; capacity
failure becomes nonallocating ErrorKind::OutOfMemory. On supported64-bit hosts
every u64 hint is usize-representable; layout/capacity overflow still returns
OutOfMemory. The smaller-usize branch is checked in source, not native coverage.
Metadata I/O failure is not synthetically forced by violating File ownership.

After that initial reserve, the f26 returning reader's growth/EOF/EINTR/read-error
body is preserved. It initializes only already-owned capacity, reads checked
slices, validates length arithmetic, truncates actual bytes, and retries EINTR
without discarding partial data. One additional byte when growing is the existing
minimum to observe further data/EOF, not a chunk constant. No measured record size,
page guess, allocation cap, cache, global method state, kernel or production
dependency/API change is introduced. This hint is not an admitted resource budget.
The resulting retained capacity, syscall/metadata and initialization costs are
unknown until the coordinator compares them.

The prototype crate has no runtime dependency. Tests reuse existing shards-testdir
and the exact existing libc0.2.189 transitively, plus the unchanged committed
thread-local Rust allocation-refusal helper. No new fixture manager/reader
framework or allocator is added. Fault-injection state is test-only; the method
has only its File and Vec locals. Normal tests cover unlinked-open-file exact
binary bytes, empty EOF, directory read error, zero/capacity overflow and initial/
later growth OOM. Growth input derives from that file's actual preferred size,
not a recorded proc-stat/page/default number. EINTR source behavior is preserved;
live injected EINTR is owed, not claimed.

## Light validation and coordinator handoff

Native checks pending. Runnable light command:
`bash docs/research/measurements/managed-proc-reader-coordinator/prototypes/run-light.sh`.
This small standalone crate needs only Rust/testdir/libc; requested private Linux
envelope4CPU/1GiB/2GiB, target ~/managed-proc-blksize-target, evidence
~/managed-proc-blksize-evidence/v1. No root clone/cache or local Mac build.
The request is admission, not a demonstrated hard quota/peak. Any rejection/failure
is retained; no repeated/lowered guard request. Root can import read_preferred
into its own comparison artifact without test-only modules (cfg(test) is off).

R01/R04/R06/R07/R13: test-only checked allocation/metadata/read errors and the
existing returning loop. R02/R03/R05/R12: primary per-File hint, explicit unsupported
zero/unrepresentable cases, no broader method selection/default/cache/framework.
R08/R14: no cost/ratio/latency/allocation-free claim; root alone compares methods.
R09/R10: this is an experimental alternative prompted by root's retained f26
cost data, not a newly claimed defect/fix; commands/raw/identities retained.
R11: Linux scoped native coverage only, no matrix/VM implication. R15/R16:
implementer light tests only, no children/main merge, private source/target/evidence,
no resource sweep/profile or unrelated cleanup.

Owed root: comparison of CPU/allocation/reallocation/syscall/fault/retention/time
against exact frozen std/f26 alternatives; supported-input decision for metadata
errors/zero hints/admission; Linux musl/arm64/distribution and EINTR coverage;
any combined integration/matrix gates if a method is selected. Frozen f26,
pthread prototype and full Mac owned-OOM evidence are not altered by this piece.
