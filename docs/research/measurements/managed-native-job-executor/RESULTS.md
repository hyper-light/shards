# First ordinary GNU process leaf results

Exact implementation: process SHA256
`05ccf9552e4ad4521584cc6e5fe222eb5c3d4315d63151954e2f9412781c3eec`,
test SHA256 `eb4c09e9841f30a59ffa577cb27c9f4e8b16b788e84e5d3b9b9772326bbe64a9`.
Named two-file patch: `freeze-v4/process.patch`, SHA256
`5d0dae2d64d2288ff7e36da2824e3554a8fb4edf7dcb54b141c152dd92603cb5`.
Base main7d7; rulesdf2. The source, scope and preactor receipt reached ROOT/native/
parent/audit before the first native case (receipts11766–11769).

## Commands and outcomes

The private source is unpacked from `native-packet-v4.tar.gz`, SHA256
`961e9c6943b35e996f40d228d777980564b8ba5f7fe9247e466120291a1deed2`.
Its source manifest SHA is
`e6a465bcab52dfbfce7484c2d00ea01ceecb3ffbd4a470c1be005d7d582515e5`.
The packet includes exact canonical dependency leaves and the native counts
getter; `native_batch.py` pins/verifies them before build and after runtime.

All commands ran on Linux amd64 GNU, existing Rust1.98.1, normal Kiln admission
2CPU/2GiB/4GiB, Cargo j2, affinity[0,1], RLIMIT_AS2GiB and core0. The actual
compiler details are in original toolchain/build records; the kernel at the
case boundary was not separately recorded. Admission and
per-process address limits do not prove aggregate physical/kernel resource caps.
The same original v1 monotonic deadline was copied byte-exact into v2–v4; it was
never reset. The final native result had188,507,318,697ns remaining of600s.

| Command/phase | Actual outcome |
| --- | --- |
| rustfmt with canonical repository config | 0 |
| cargo check --tests --locked --target x86_64-unknown-linux-gnu -j2 | 0 |
| cargo clippy --tests --locked --target x86_64-unknown-linux-gnu -j2 -- -Dwarnings | 0, original workspace panic/unsafe/lint configuration retained |
| ordinary child rustc build | 0 |
| cargo test --lib --no-run, same pinned inputs | 0 |
| exact pure and ignored ordinary listings | each1test,0benchmarks |
| exact pure denial filter | first run0;1passed/0failed/0ignored/16filtered |
| exact ordinary ignored filter | first run0;1passed/0failed/0ignored/16filtered |

Actual invocation from the admitted private root:

```text
python3 /home/adalundhe/native-job-executor-7d7-v4/native_batch.py native
```

The driver names exact filters and uses one30s case bound within the original
total budget; the enclosing native command had45s. No runtime retry, deadline
extension, mutation, VM or profile occurred. Dependency tests compiled and
filtered are unrun, not passes. The source is a leaf projection, not the real
native helper or owner graph.

## Independent actual child and cleanup facts

The executed image received only explicit argv/env, binary input and separate
stdio. It independently required only stdio after exec (plus its own temporary
FD enumeration handle). It emitted exact literal bytes containing command-like
characters and invalid UTF-8 without interpretation, the exact input including
NUL, and the renamed held workdir. The checked pathname replacement remained
unchanged. Full stdout/stderr bytes precede strict terminal assertions in
`recovery-raw-v1/v4/record/ordinary.stdout`.

Actual child3493560: held pidfd became exit-ready, matching direct wait returned
3493560 with native status0, second wait returned-1/ECHILD10. Launch status
reached actual EOF; stdout/stderr matched exactly; no standalone EOF-as-success
claim is made. Child-side fixture parent descriptors returned4→4. Prepared map
retained6 staged descriptors and literal storage173B. Actual held executable
device/inode53:623882964 and workdir53:623882963 matched fstat facts taken before
birth after object renaming. Separate stderr hex was `73746465727200ff`.

All eight final build/list direct children and both final test-command children
had held pidfd readiness, direct reap, controller descriptors9→9 and no forced
cleanup. The ordinary fixture itself also reaped its own direct child. Whole
population/cgroup/kernel return remains Unknown: the fixture did not acquire a
real issued native job Group or exercise the owner admission protocol.

Frozen binaries remain OUTSIDE target on Adacachy:

| Path under `/home/adalundhe/native-job-executor-7d7-v4/artifacts/` | Bytes | Before/after SHA256 |
| --- | --- | --- |
| native-gnu | 9529304 | 3e4160b34da111d6398a825237925078facf5296ebbd2a2f4a8c372ac3b4ffca |
| child-fixture | 4582384 | 4d5ee74d2113851d9256173ed9dbb13d2377f35d8231bffa878d20fb5960dcea |

## Retained failures and provenance

No native actor ran in v1–v3. V1 format checking failed because the projection
omitted canonical `rustfmt.toml`. V2 format passed and compilation failedE0583
because path-attribute module placement changed relative child module lookup.
V3 conventional layout compiled, then strict Clippy rejected the test's
irrefutable if-let and the144B owned Result error. V4 replaced that owned Result
with inline `Preparation::{Prepared,Rejected}`, preserving SAME rejected Request
without boxing or lint suppression, and corrected the test pattern. Original
sources, packets, failure raw and distinct private targets remain untouched.

All74 original raw payloads across v1–v4 were recovered and verified individually
by byte length and SHA256. Manifest75th file: `recovery-raw-v1/RAW-FILES.json`, SHA
`9bb09810b5c0bc47e84575d750448052806280865100f7608bdb3f9f84fc2489`.
Archive `recovery-raw-v1.tar.gz` is12273B, SHA
`38d013df68dc2170f0bdc67bb556d12ba72d301ce139033ecf09e724879e47b0`.
Four source packets and their manifests retain every preactor source version.

## Remaining owned dependency and ROOT gates

Native accepted the exact v4 preparation enum/metadata/status contract and owns
module registration and the actual existing gated Worker consumer. It reported
that the current inherited helper seccomp stage does not contain required native
exec/runtime calls. ROOT must resolve that stage contract; this leaf neither
widens filters nor invents a fallback. This is a source compatibility dependency,
not an actual reproduced production failure. No real job release is established.

Actual owner readiness/issued permission/current commit, held Group birth and
pidfd association, canonical audit barrier, real native helper consumption and
typed outer NotStarted/Started/Uncertain retention remain native/parent/ROOT
integration gates. Static musl, both guest architectures, seven targets, ordinary
composed VM cases, population/revocation/fault behavior and all cost/profile/
benchmark claims are unrun. The completed mechanics batch does not close P0–P13
or release this lane. No skipped or held campaign is revived.

Canonical staged whitespace check returned2 ONLY for the original native stdout trailing blank lines (pure and ordinary). Original bytes remain unchanged. Excluding that recovered raw directory, product/evidence whitespace check returned0; policy returned0. Initial no-index checks returned1 with empty diagnostics because new-file diffs were present, not whitespace repairs.
