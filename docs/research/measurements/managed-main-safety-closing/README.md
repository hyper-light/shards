# Main startup and process-identity safety batch

Checked source: `08316c12f7c980b580d0182431e31d445f9a86f6`, based directly on
current main `f5cec365d2e435afae5e6f613c92d9d3759ef60f`. This batch preserves
typed startup/process errors and reads Linux process names as opaque bytes.
It does not adopt the experimental managed relay/capture work.

Before this change, formatting a kernel command-line failure or process-identity
refusal could abort under allocation failure. Valid non-UTF-8 Linux `comm` bytes
also prevented reading the numeric process start time. The final change keeps
the command-line cause and process operation/native code typed, builds the fixed
proc path from its exact decimal bound, grows the reader fallibly, and parses
only the required numeric field. Unknown I/O representations remain explicitly
unknown. Existing source/method/reproduction records are in
[the argv record](../managed-kernel-argv/main-backport.md) and
[the process record](../managed-proc-main/README.md).

Both public `vm::start` and `vm::run` now return `StartError`, and process failures
are represented by `Refusal::Process`. Consumers must follow the
[migration guide](../../../development/managed-safety-migration.md).
`shards-api` is unpublished (`publish = false`); this is still a source migration,
not a promise that existing downstream matches or annotations compile unchanged.

## Closing results

| Check | Actual scope and result |
| --- | --- |
| r517, exact08316c12 | Format and all seven `scripts/lint` targets, including GPU checks, pass. Commands, source, toolchain, log, stages and exit0 are retained here. |
| r462, exact08316c12 | Native Windows x64: backend identity, real WHP PID1 boot/power-off, invalid VM configuration and malformed VM argument checks all pass. |
| r399/r400/r449, exact66dc | API library/simulator: Linux GNU42/15, native macOS ARM33/15, native Windows20/4 pass; original ignored cases remain unverified. The later083 changes are documentation only. |
| Startup/process fault guards | Actual construction and public attach allocation/raw-name guard removals reproduce the corresponding failure, followed by exact restoration; these are narrow path results, not whole-library OOM safety. |

The existing coordinator records are retained at immutable integration commit
`9fbde14bd441a7f1aede5be60ea731893629a9ec`:
[native WHP raw records](https://github.com/hyper-light/shards/tree/9fbde14bd441a7f1aede5be60ea731893629a9ec/docs/research/measurements/managed-coordinator-udp-api-reporting/windows-whp),
[API and public fault records](https://github.com/hyper-light/shards/tree/9fbde14bd441a7f1aede5be60ea731893629a9ec/docs/research/measurements/managed-coordinator-udp-api-reporting),
[startup records](https://github.com/hyper-light/shards/tree/9fbde14bd441a7f1aede5be60ea731893629a9ec/docs/research/measurements/managed-coordinator-tcp6-startup).
Record links preserve exact source/artifact/command distinctions; different
revisions are not silently reported as one full-suite result.

## Limits and rule accounting

The owned macOS Mutex/Once allocation failure and cold standard-stderr reporting
failure remain open. This batch does not claim that the whole API, CLI, standard
library or allocator-failure surface is panic-free. Native Windows ARM remains
unverified and its VM stays paused. This is not a full workspace/VM-matrix gate,
a 4–5x result or completion of Step10 or the managed-runtime plan.

R01/R04/R06/R07/R13: typed fallible paths, explicit causes, focused reviewed
source and precise assertions; broader panic/safety limits above remain open.
R02/R03/R05/R12: existing documented kernel/proc interfaces and actual fault
examples, exact protocol/path bounds, shared platform seams, no new cache,
periodic probe, extra installed binary, production quota or host policy change.
R08/R14: no comparative cost claim; profiling and remaining costs stay root-owned.
R09/R10: source-linked original failures, guard removals, restoration and retained
raw records. R11: seven-target compilation and named native results only;
unavailable hardware/ignored cases are not passing skips. R15/R16: coordinator
review/heavy validation, signed named-path commits, private targets, headroom,
preserved evidence and unchanged host trust/admin/crypto settings.

Policy receipt: coordinator, file revision f5cec365, R01–R16 SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
All 14 regular closing-matrix records are hashed in `MANIFEST.json`; private
archive4842 bytes, SHA256
`d6bff2440f093b52bd57d077f0e50a28c82cb33ec3e1fa13168be6202c3cb5f1`.
Normal AWS-LC compiler-feature probe/fallback warnings remain unmodified in the
passing log. Untracked macOS metadata is preserved and classified separately
from clean tracked source. No executable bytes are transferred.
The original staged whitespace check returns2 for two blank AWS-LC warning
lines in the raw lint log. That log stays byte-identical. All other staged paths
pass the same check; exact commands, paths and original output are retained in
`WHITESPACE.json`. No global whitespace rule is changed.
