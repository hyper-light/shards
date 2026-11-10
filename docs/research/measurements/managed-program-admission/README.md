# Broker worker admission before release

Status: coordinator implementation in progress, not an integration gate.

POLICY_READ: coordinator, main `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`,
`docs/development-rules.md` SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Owned source is `server::Programs` admission/worker control, separate from the
concurrent typed peer dispatcher and init resource admission lanes.

## Existing seam and experiment

The per-domain server owns its private helper channel and the program sockets.
A RUN control message transfers one native program descriptor to that helper;
that transfer is permission to create a confined worker. The first probe exercises
actual `Programs::start`, actual `sendmsg`/SCM_RIGHTS and independently receives
at the helper end. Set the broker's issued identity to the final u64 value, ask
for one program, and require a typed refusal without another native RUN or FD.
This is an internal boundary setup, not a claim that ordinary traffic can finish
2^64 calls. No actor payload may choose this broker-owned identity.

Baseline production source is main cbd3928. The source observation is wrapping
identity issuance and allocations after RUN; the probe results have not yet been
recorded and these are not asserted as reproduced defects yet.

## Sources and alternatives

The existing owned helper and restricted QuickJS worker are retained, rather than
adding a shell wrapper, parallel process manager or runtime. P7's broker request
ordering and state contract are in `docs/design/managed-runtime-plan.md`, section7,
and `docs/research/managed-runtime-tool-broker.md`.

- Linux unix(7): sequenced packets retain message boundaries; SCM_RIGHTS transfers
  an open file description: https://man7.org/linux/man-pages/man7/unix.7.html
- Linux send(2): nonblocking full queues report EAGAIN; a successful send is not
  proof the worker executed or an external effect completed:
  https://man7.org/linux/man-pages/man2/sendmsg.2.html
- Rust Vec allocation/capacity guarantees and fallible reservation:
  https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve
  The online document currently labels1.99.1, whereas this repo pins1.98.1;
  exact compiler/source identity will accompany native checks.
- Linux cgroup-v2's existing hierarchy applies to descendants but a software
  reservation does not prove kernel-cost attribution:
  https://docs.kernel.org/admin-guide/cgroup-v2.html

A required pre-release allocation reservation is distinct from a hard owner
entitlement. This slice will not invent per-tenant guarantees, controller support,
durable pre-action audit, native executable grants or complete P7 coverage.

## Owed

Actual baseline/fixed/mutation runs and retained command/output/source identities;
allocation refusal and no-RUN evidence, native helper result/cancel paths,
seven-target combined gates, real-VM matrix, profiling/benchmarking and full
R01–R16 readiness review. No result is inherited from another revision.
