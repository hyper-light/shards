# G22 native Windows amd64 light result

Kiln r190 completed0 on jupiter `f766f09294f2e17e3b8a631ffd4e6a4b`, admitted
4CPU/8GiB/20GiB. It compiled clean, detached source
`122332036eb689c33fd52b4a065c53c7dc5e4530` (production53535ab6) in the private
`C:\Users\corph\managed-borrowed-windows-check`, target
`C:\Users\corph\managed-borrowed-windows-target`. No production compile defect
was observed, so this batch changes no production source. Root Windows
clones/targets were untouched; no VM or repeated handle/resource sweep ran.

Native environment: Windows11 Professional build26200 amd64, Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, host `x86_64-pc-windows-msvc`,
LLVM22.1.8, Cargo1.98.1. These are the private-repo context results, not a home
default compiler. [Identity](windows-r190/identity.json),
[rustc](windows-r190/rustc.log), [Cargo](windows-r190/cargo.log),
[source hashes](windows-r190/source.sha256) retained.

| Actual command | Result |
| --- | --- |
| `cargo test --locked -j4 --profile test-release -p shards-api --lib --no-run --message-format=json-render-diagnostics` |0, actual native compile |
| `cargo test --locked -j4 --profile test-release -p shards-api --lib borrowed::tests -- --test-threads=1 --nocapture` |12 pass,0fail,1 private child role ignored |
| Same command with `stream::tests` filter |9 pass,0fail; two additional Mac/Linux cases are cfg(unix) |
| `cargo clippy --locked -j4 -p shards-api -p shards-afunix --all-targets -- -D warnings` |0 |

The borrowed tests observe actual no-CONNECT for zero/overflow and stale identity,
dead-process rejection, actual child response after stream drop, exact payload/
clone/half-close, withheld/dribbled finite deadline, malformed/closed replies,
and stream-box/clone allocation failure. On Windows the shared-state injection
subcase guarded by cfg(unix) is not run; no coverage is inferred for it. The
Linux-only FD/task assertion also does not run here. There is no repeated Windows
handle-count, full-library, afunix-unit execution or leak-free claim.

Exact native test binary (reported by actual compiler artifact JSON):
`C:\Users\corph\managed-borrowed-windows-target\test-release\deps\shards_api-d7d07df9356ba62a.exe`,
SHA256 `ef9565ec1fcb3bfaa3f59e46801086be4ee5b764cb04424c5f9b368745f0512f`.
[Binary record](windows-r190/binaries.json), [build](windows-r190/build.log),
[borrowed](windows-r190/borrowed.log), [stream](windows-r190/stream.log),
[clippy](windows-r190/clippy.log), [native exit summary](windows-r190/summary.json).

Normalized readable copies decode Windows PowerShell5's UTF16/BOM output to UTF8;
the original bytes and exact tested launcher are preserved in
[raw-archive.tgz](windows-r190/raw-archive.tgz), SHA256
`5d9dc819fabda5a5396f30522ec3d8efbbffd86d139d40549e8dbbb726732048`.
PowerShell decorates the first ordinary native stderr line as NativeCommandError;
the retained native exit statuses and Rust test summaries establish these results.
No such decoration is counted as a product failure or silently removed.

## Setup failures preserved separately

r188 was admitted under the same envelope but ended49 before clone/compiler:
python3 resolved to a Microsoft Store execution alias. No interpreter, execution
alias or security setting was changed. [Setup record](windows-setup-failure.json).
r189 then ended1 before clone/compiler because the harness combined two
Get-Command application paths into one string; this was the implementer's
launcher defect. [Setup record](windows-powershell-setup-failure.json).
The first-PATH correction followed the existing command search order; it was
actually exercised by r190. Both earlier results are setup reds, not Windows
program/compiler reds. Root authorized corrected setup dispatches after accepted
requests; there was no lowered ask or rejection polling.

The tested launcher is retained as evidence. Future replay needs only the existing
Git Bash and cargo.exe; no expanding launcher or Python dependency is required:

```sh
cd "$HOME/managed-borrowed-windows-check"
test "$(git rev-parse HEAD)" = 122332036eb689c33fd52b4a065c53c7dc5e4530
export CARGO_TARGET_DIR="$HOME/managed-borrowed-windows-target"
cargo.exe test --locked -j4 --profile test-release -p shards-api --lib borrowed::tests -- --test-threads=1 --nocapture
cargo.exe test --locked -j4 --profile test-release -p shards-api --lib stream::tests -- --test-threads=1 --nocapture
cargo.exe clippy --locked -j4 -p shards-api -p shards-afunix --all-targets -- -D warnings
```

## Policy and remaining gates

Implementer receipt renewed from canonical g121-i7 c921a0b1, FULL R01–R16,
rules SHA256 `1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`,
AGENTS/evidence/CLAUDE/plan/handoff read before work. Ownership is the existing
API/afunix scope plus this evidence; no nested agents, main merge or broader design.

R01: focused native allocation/error checks passed; full Mac owned-OOM abort stays
RED. Root subsequently confirmed the actual OnceBox pthread-mutex stack in its
separate trace build; no warming or synchronization remedy is adopted here.
R02/R03/R05/R12: no production architecture/constant/wait/cache decision changed;
the existing [candidate research](CANDIDATE.md) remains the decision record.
R04/R06/R07/R13: no production control/interface change in this validation batch.
R08/R14: no profiling/benchmark or comparative/resource claim; root owns them.
R09/R10: actual setup failures separated from actual compile/test results; exact
source/commands/raw bytes/native exits/binary identity retained. R11: native
Windowsamd64/MSVC only, no Windowsarm64 or full seven-target/VM pass. R15: scoped
light tests and signed evidence only. R16: private clone/target/evidence, unchanged
admission ask, no unrelated cleanup. Full R01–R16 account also links from
[CANDIDATE](CANDIDATE.md).

Latest Linux complete gate/raw retrieval, full Mac library RED, Windowsarm64,
repeated native handle return, real-VM and combined final review remain root-owned
and owed. Windows focused success does not close those gates or imply ready-main.
