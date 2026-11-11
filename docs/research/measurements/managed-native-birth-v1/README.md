# Native birth access: checked component checkpoint

ROOT implemented the actual member/job inode access producer and its caller
delta on the immutable runtime v3 graph. This is not activation on main or a
complete native executor, Session, resource entitlement or actor-isolation proof.

The exact compiled source manifest is
`3b29342bfa1da90d9648c9a4055108824ec6dd28dbd9a19bb9746e148f23b8ec`:
4769 regular files plus one fixture-symlink target compared as metadata only.
The new birth.rs is 8897 bytes, SHA256
`d128d7ca109b78372c6dcba2bec8807e81e9377b79e49cd39cd650c867fe27ff`.
The seven-path `ON-runtime-v3-COMPILED-UNAPPLIED.patch` is 19618 bytes, SHA256
`73a14fe70f654582c50b0edbc37129f5a33f8db26cc74d9bf486e231a9db0f07`.
`COMPILED-CALLER-MANIFEST.json` pins every exact preimage and postimage. Apply
only the narrow caller hunks to a newer runtime; never replace its whole files.
The earlier d948 patch and initial source packets are retained pre-format inputs.

`Staged::prepare_native_birth` consumes the original normalized Domain facts and
Scope, verifies its unused group's component/member/filesystem, acquires retained
member/procs descriptors, and changes only the held procs inode UID. Its actual
GID/mode and directory/controller access remain unchanged. The same Member flows
through Launch/Preparation to Owner. Actual per-job acquisition derives its
group from that held member and installs job access before publication, keeping
the existing failed-cleanup objects and typed causes. No process permission,
current-owner result, resource promise or Session comes from this object.

Actual first GNU gate r21 completed0 in 27.748 seconds under the normal
451397 2CPU/2GiB/4GiB/600s/j2 envelope, fresh source/cargo/target/tmp. Full init
all-target check0 and strict Clippy `-D warnings`0 passed. The exact test binary
was frozen and listed before three new/changed cases each passed1/failed0/ignored0
with175 other tests filtered. Eleven direct stages were pidfd-ready, waited,
second-wait ECHILD, with controller FD4->4 each. Whole descendant/kernel resource
return remains Unknown. No old owner units, actor/helper/VM or held audit cases
were executed in this compiler scope. Test binary 74710248 bytes, SHA256
`14d7b793b3c0a17f6cee977f49a6fa60ed1f2ea567dff99c69afe1b33709ede0`,
remains frozen outside target on the runner. Compiler raw41+manifest42 all
verified: archive31706 bytes SHA256
`4d285467093f274bf8045c1c83badf64a9d8676f43cdb543a8aa03a6bce2eb96`.

The separate actual disposable guest probe validates the Linux two-inode
mechanism. Privileged opener only and destination-only gave EACCES; member plus
job procs UID access allowed direct child placement in /member/job with actual
pidfd/wait/ECHILD; sibling remained EACCES; restored ancestor ownership restored
EACCES. Helper UID/GID200000 had zero effective/permitted capabilities. Actual
job/member empty, helper reap, exact removals and procs ownership restoration
were checked. Monitored directory/subtree inode metadata stayed unchanged. The
raw field `directories_controllers_unchanged` refers only to those listed inode
metadata observations, not every possible controller file or kernel cost.
This used pinned baseline kernel48a3824b, not the experimental network kernel.
Guest raw25+manifest26 all verified: archive23771 bytes SHA256
`3341c9b7e5db4468e1b487d4442c073c13d4e592f315a741bf85c8bed89d8864`.

The first guest fixture stopped after actual EACCES because its pidfd output
slot sentinel expectation was wrong. Linux can write a numeric slot before
refusing cgroup placement, then release that reserved descriptor. The corrected
fixture checks F_GETFD/EBADF. First source/binary/stdout and both direct-child
receipts remain retained; no production defect or descriptor ownership was
inferred from the numeric slot. Read `DECISION.md` and the two pinned research
notes on main0d261aa93 for alternatives and authority boundaries.

An explicit consumer limitation remains in the checked v1 patch: it checks the
member before every Prepare. The actual new runtime v7 must apply birth-specific
checks only to Process and pass no birth object for Read/Write. It has implemented
that narrow adaptation in its own source; those later results are separate and
cannot inherit this gate. This checkpoint preserves the actual v1 bytes.

## Policy and remaining work

POLICY_READ coordinator01a148884a4770fdaf0111f3f792a248 at06ae067c0; current
main0d261aa93 adds only reviewed research documents, policy bytes unchanged.
Rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.

| Rules | Actual evidence and boundary |
| --- | --- |
| R01/R04 | Checked source and typed refusals/cleanup, actual full-init check/strict0 and three cases. No new production panic or string grant. Member Debug hides original scope. |
| R02/R03/R05/R12 | Pinned kernel/systemd/runc alternatives, real guest controls, original Domain principal, kernel mode flags and bounded hierarchy. No sampled capacity, new manager/cache/poller, helper credential or host-root change. |
| R06/R07/R13 | One leaf and seven narrow caller paths. Existing Group/Controls/removal reused. Comments state kernel predicates and remaining actor obligations. |
| R08/R14 | No comparative cost, allocation-free, leak-free, hard-backing or4–5x claim. Two retained member descriptors and temporary job-procs acquisition still need induced-cost accounting. |
| R09/R10 | Required new feature, not a legacy defect fix. Actual five guest controls, first fixture failure, source pins, pre-case binary and complete raw retained. Actual production-helper/actor guard mutations remain unrun. |
| R11 | GNU amd64 component and pinned amd64 KVM mechanism only. Full seven targets and actual supported guest/host combinations remain open; paused WinARM untouched. |
| R15/R16 | ROOT-owned implementation/guest proof, bounded scopes, no peer-file overwrite, no old scope reset, no cleanup or held graph/case execution. Exact source handed to sole runtime owner; main readiness not claimed. |

Still open: actual normalized launch/current/commit/helper path in a composed
guest; actor path/FD/procfs exclusion; effective LSM/topology conditions; actual
Read/Write execution; finite record retirement and acquired parser/reply/body/FD/
transport backing; ancestor protection; control under pressure, platform,
adversarial and comparative gates. Automatic review also continues to hold the
separate audit4819 compiler graph and its five portable tests. No alternate
executor or this component gate lifts those holds.
