# Pending cancellation before native turn recovery

Current-main base0342b85d9c808e3ced041a6c3ef0e9bf70438272; rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Full current AGENTS/R01–R16/evidence/CLAUDE/plan/handoff and its required current
closing records were read; POLICY_READ accepted seq6890. Implementer plus assigned
adversarial relay tester. ROOT owns audits/profiles/benchmarks, combined gates,
evidence review and main integration. No children, goal dispatch or main merge.

This NEW branch starts at0342 using the same clean worktree. It carries ONLY
the previously tested cfg(test) declaration and private turn test from frozen
df7b7148e/ce6019582, then adds the approved Terminal selector/cancel case. All
716 production pump lines remain byte-identical083bce5e…; current0342 engine,
host fixture, parser, grants and deadline are retained. No wholesale older branch
merge or production notification/state/queue/visibility change. The old branch
and every previous raw/source/failure/frozen artifact remain intact.

The reviewed [proposal at df7](https://github.com/hyper-light/shards/blob/df7b7148e626cc6cb5ef9c5cd5a76bf52429318c/docs/research/measurements/managed-tcp-forward/implementation/turn-pressure/NEXT.md)
adds a cfg(test) Terminal::{Forward,CancelBeforeRecovery} at the SAME actual
Direction seam. Both cases require real finite prefill/send-WouldBlock, real
source recv to obtain pending and one real blocked turn with exact retention.
The independent backend then drains and records only the prefill.

CancelBeforeRecovery writes one native uint64 value1 to the SAME owned eventfd
and records the actual8-byte result with positive original time, before another
real turn. That turn must return typed Cancelled, with suffix/cursors/read/write/
half/extra and all WouldBlock/partial-write counters unchanged. The eventfd is
never consumed. After closing the owned output, the independent nonempty backend
read must actually return EOF0 with no relay byte. Every byte/result/counter and
owned descriptor fact precedes strict assertions.

All forwarding turns retain Control::Until with that real eventfd and SAME
original at. The terminal observer uses the already existing trusted original
test Deadline(cancel=None) at that SAME at, so it can perform its independent
read after cancellation. This test-only observer does not disable cancellation
for forwarding, create a new deadline or grant an allowance.

Forward remains the existing positive control and must independently receive
the exact prefill+payload and EOF. The new two-case driver retains the prior
private child launcher/pidfd/release/wait/reap mechanics and checks the actual
signal→cancelled turn→nonempty probe→cleanup order. No synthetic event, retry,
sleep, tuning, new manager, mutation or VM. Missing events remain failures.

Same caller inputs: request13, slots3/5,1MiB prefill envelope/seed00ff0d0a01020304,
queue inputs4096, original case30s/parent45s. No capacity guarantee follows from
those inputs or a native readback. GNU+musl one pending-cancel and one forwarding
control each; private Linux runner8fadc1ec213332e16cb0d915a3fb913f,
`~/relay-turn-pending-cancel-0342-v1/{source,check,target,cargo-home,record,artifacts,cases}`,
2CPU/2GiB/4GiB admission, <=600s/-j2. Formatter-only packet retained separately
at `~/relay-turn-pending-cancel-0342-format-v1`; no local target/cache/build.

The first source checkpoint8de7fb74d explicitly had unrun compilation/native
cases. Its [actual validation record](VALIDATION.md) now retains GNU/musl strict
checks and all FOUR first native invocations passing0: one pending-cancel and
one forwarding control per libc. Exact pre-build/source-after hashes and raw
signal/cancel/suffix/EOF/FD/pidfd/reap facts are preserved. No mutation ran.
Prior ce601 and5a4/2300 records keep
their original scope; original db4 pressure result remains19/20 plus a separate
unchanged passing repeat, never20/20.

Main0342's carried VSOCK/matrix adoption is acknowledged as actual ROOT evidence.
This new cfg(test) slice still owes ROOT combined-source review/integration gates.
It does not establish a full scheduler, new VSOCK result, performance/hard-resource
guarantee, mandatory route coverage or management P0–P13/G25 completion. The
checked-ready/concurrent-cancel race after admission remains explicitly open.
