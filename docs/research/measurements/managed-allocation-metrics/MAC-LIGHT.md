# ARM64 Mac standalone unit checkpoint

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Canonical main receipt is now
`69dbc3180b8c8c55cb03f9f95f822d2752ba2887`; rules SHA-256 remains
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
No child/plan/goal expansion occurred; the existing active lane goal is unchanged.

The source at own `9e346b16408ea7d0353ac16808b285637071cf20` is byte-identical to
the previously reviewed counter and pure fault model:

| Module | Source SHA-256 | ARM64 Mac unit result |
| --- | --- | --- |
| `managed_alloc.rs` | `7f59e4732260b41b0f9a4e47f3c1a94f73d579f9c52fbb71e4fbbb0db26c2abc` | 5 passed,0 failed,0 ignored |
| `native_faults.rs` | `3946267e81571fcfd1c0853b1033bf170ef45632a7d9d99d747bef9ffd710513` | 5 passed,0 failed,0 ignored |

[Original raw records](records/macos-9e346b164) retain19 verified files, the
exact build/test argv and tool identity. Both `rustc --test -D warnings` builds
returned0. Installed compiler was explicitly selected with
`rustup which --toolchain 1.98.1 rustc`; actual output was Rust1.98.1, commit
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8,
`aarch64-apple-darwin`. No toolchain download, Cargo, helper build, formatter or
Mac Clippy run was performed. Cargo's MSRV is not used as proof of the actual tool.

Runner `b24c5c951260dcdfd0972f2aeaf3cf51` was macOS26.2/Darwin25.2.0, arm64.
Kiln admission stayed1CPU/1GiB/2GiB, `sync:false`; both module builds/tests were
serial under nonblocking `lockf -t 0 /tmp/g121-mac.lock`. Recorded free space was
95,170,613,248 bytes before and95,168,372,736 after, with a30GiB check before
each compiler/test invocation. These are individual observations, not continuous
disk monitoring or a hard kernel resource-limit proof.

Only these NEW private paths were used on b24:

* `/Users/adalundhe/managed-allocation-metrics-source-macos-9e346b16408ea7d0353ac16808b285637071cf20`
* `/Users/adalundhe/managed-allocation-metrics-target-macos-9e346b16408ea7d0353ac16808b285637071cf20`
* `/Users/adalundhe/managed-allocation-metrics-light-macos-9e346b16408ea7d0353ac16808b285637071cf20`

The target's `managed_alloc-tests` SHA-256 is
`4d09cea84c7ff70fa296711bcc48e230a5cc42b81dc27f594b8b01ac2a33c721`;
`native_faults-tests` is
`d51b4f387aa921689d14917e10705b55678fa48de588dc78021aef18a011cdad`.
Both were hashed before execution and rechecked at transfer. Final binaries,
source and evidence remain reserved even though all own jobs have completed.
No local target/cache exists; no root b24/Linux source/target/cache was written.

## Earlier floor hold

The same source/command first reached remote Mac
`2768d0d5d2974f1e21d348edadff9d1b`. Its actual free space was6,549,282,816 bytes,
below the required32,212,254,720-byte floor. The preflight stopped before querying
the toolchain or creating the private paths; **no build or test ran there**.
[Full command/result/path receipt](records/macos-preflight-2768-9e346b164) is
preserved, including the complete command text. A later read-only check confirmed
all three proposed source/target/light paths absent under `/Users/adalundhe`.
No host cleanup, data removal, goal reset or actor stop was attempted. Root owns
any R16 reclamation; a Kiln resource admission alone did not establish this floor.

## Remaining meaning and root gates

The counter tests use the same valid small `System` operations and pure arithmetic
cases as Linux; synthetic null/sentinel accounting is not native OOM evidence.
The fault-model tests exercise only source labels, widths and conditional/Unknown
arithmetic. They call no native process/fault API. No global allocator was
registered here, and neither binary is the actual managed-output caller.

This does not validate actual native fault probes, the Linux-only cost fixture,
daemon/VM/guest/kernel/foreign-C allocation coverage, source-matching carried
helpers, full supported targets, a paired executor or a performance distribution.
Root still owns all actual caller/native/paired/interleaved/profile, uncertainty/
perturbation, adversarial/mutation/restoration and cleanup/matrix gates recorded
in [QUEUE-CHECKPOINT.md](QUEUE-CHECKPOINT.md). No RSS, retention, leak-freedom,
cross-OS fault equivalence or optimization claim follows from these units.
Windows ARM remains paused; the startup proposal remains unapplied.

R01/R04/R06/R07/R13: unchanged test-only implementation, no new unsafe/control
path. R02/R03/R05/R12: same researched standard wrapper/model, no new adapter,
interface, cache or production literal. R08/R14: units only, no profile or
comparative claim. R09/R10: exact commands/raw/source/binaries and held result
retained, not a speculative product fix. R11: native ARM64 Mac standalone scope
only; other actual host/caller gaps remain explicit. R15/R16: admitted light,
no children/main merge/local build/new worktree, common build lock and floor
hold respected; source/evidence/final binaries and root jobs/caches preserved.
