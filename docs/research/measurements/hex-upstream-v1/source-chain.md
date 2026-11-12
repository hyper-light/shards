# A refusal's chain of causes says each cause once (Hex patches 0011, 0023, 0030)

**Defect.** Error reporters print an error's Display, then each `source()` after it. A refusal
whose Display already says its failure, and whose `source()` returns that same failure, prints it
twice. Upstream shards had `Refusal::Process` right (its source is the failure's own source) and
`Refusal::Borrowed` and `BorrowFailure::Channel` wrong: `Borrowed`'s Display is `vsock port N:
<failure>` and its source the failure; `Channel`'s Display is the handshake error's own and its
source that error. A borrowed connection refused in its handshake printed the handshake error
three times. Hex's 0011 made `Process` repeat too and its 0030 restored it; Hex's `Scope` (0011)
and `Connect` (0023) repeat theirs. ROOT asked that all four say their cause once.

**Fix.** Each such arm forwards its failure's own source, as `Process` does; `BorrowFailure`'s
`Channel` forwards the handshake error's. `Scope` and `Connect` take the same form where this port
carries Hex's 0011 and 0023.

**Red, green, guard removal.** Test `tests::a_refusals_chain_says_each_cause_once` (lib.rs) prints
`Borrowed` with a handshake failure, `Borrowed` with `Expired` and `Process` with `Exited` as a
reporter would and counts the failure's text. adacachy (451397fc), 2026-11-11, base 03786682:

| Build | Patch | Result |
|---|---|---|
| green | the fix | passes; whole lib 54 passed |
| red | `mutations/chain-red.patch` (both arms as upstream had them) | fails, lib.rs:729 |
| guard removal | `mutations/chain-refusal-borrowed.patch` (`Refusal::Borrowed` returns its failure) | fails, lib.rs:729 |
| guard removal | `mutations/chain-borrow-failure-channel.patch` (`Channel` returns its error) | fails, lib.rs:729 |

Callers match `Refusal`'s typed variants (R04). No caller walks a refusal's `source()`: shards'
only `.source()` calls are client_binds.rs's and client_grants.rs's relay and file methods, and
Hex's crates have none (git grep, 2026-11-11, Hex main 2dc8295f).
