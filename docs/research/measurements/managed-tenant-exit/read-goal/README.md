# Give the final fixture record's ACK to ack_and_finish

Separate fixture-only fix from96b1019f6ed027c8af7a2c68928543f489f7d63f.
Owned changes: tenant_exit.rs and this evidence only. No parent, production,
server, queue-capacity, retry, polling or manager change. Crash/reconnect and
partial native-stdio checkpoints remain on their own preserved branches.

POLICY_READ IMPLEMENTER01a1c5d28cea74a5b0241200190d6c54; coordinator
01a148884a4770fdaf0111f3f792a248. Canonical policy files f5cec365, rules SHA256
d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Full policy/evidence/CLAUDE/managed-plan/current handoff read before work.
ROOT explicitly authorized these three local read modes after actual r662.

ROOT actual baseline e180fa7d0380f8bb92609f7de56a34f03bf2d7d9/r662 confirms the
publication fix: release.json records OwnerReleasedNeighborAfterBothMember1Eofs
and gate_peer_eof=true, after both member1 End/ACK/QUIC EOF and main exec. Next
the exact test exits101 at managed_output.rs332: the driver's admitted command
slots are full in Observer::ack_and_finish. r662/ retains ROOT source/test exit,
release, protocol-failure observations and selected exact failure context copied
read-only from ~/root-managed-normal-e180-v1. Full original test.log stays there.
This lane did not execute the helper, socket or VM.

Capture.receive previously enqueued ACK for the last new member2 Data record and
returned; the immediate ack_and_finish then enqueued another ACK+FIN through the
same existing one-slot client handoff. The existing Observer.exact already returns
BEFORE its final ACK for that caller. This is a fixture caller ownership error;
no product/server correction is proposed from this red.

ReadGoal now represents three actual modes:

- CanonicalEnd: ACK records including End, then require exact transport EOF.
- InitialPayload: ACK the complete initial payload and continue owner control.
- FinalPayload: preserve all byte/state/offset checks, return before its final
  ACK, and leave that single ACK+FIN to the existing ack_and_finish caller.

Every strict End/EOF/binary identity/offset/metadata/neighbor/main/cleanup assertion
is retained. The strict terminal function is byte-identical to96. There is no
extra queue, deadline extension, guessed wait or assertion waiver. The same
original real test is the decisive regression loop; no shallow mock queue test
is substituted for ROOT's before/after effect.

Scoped light r20 on Fedora8fadc1ec213332e16cb0d915a3fb913f,
linux/amd64,cpus=1,mem=1g,disk=2g, NEW private
~/managed-tenant-read-goal-light-01a1c5d2-v1/{source,target}: GNU/musl check --tests
and clippy --tests -Dwarnings all0, repository formatter/policy0. Source316KiB,
target81,260KiB observations only; admission is not hard aggregate accounting.
Exact fixture SHA256571ea2e9ff5f8db5d8ff5faf9c702905a67db538c3eccc01288086080b7f84eb.
light.json retains toolchain/lock/commands' logs and checked source hashes;
kiln-v1.command retains transfer hash. Actual host fixture/normal helper bodies
and ABI compile with inert surrounding host glue, never executed. Full host
integration and the actual one-slot interaction are not proved by those stubs.

ROOT next applies this leaf-only patch to its e180/publication+finish-fixed source,
builds matching actual artifacts, and reruns the unchanged exact normal-exit case.
It must observe final member2 data→one ACK+FIN handoff, later independent main
progress, full cleanup and then the existing strict terminal metadata assertions.
Any next metadata red remains a new ROOT reproduction before a production fix.
Actual green/mutation, full population/kernel return and seven-target/real-host
gates remain ROOT-owned and unrun here. Lane goal stays open; continue queue.

| Rule | Scoped accounting / remaining gate |
| --- | --- |
| R01 | Test-only enum/flow; production unchanged, scoped compile/clippy0; no global panic/OOM proof. |
| R02 | Actual ROOT r662 and existing Observer.exact final-ACK ownership pattern; no architecture decision. |
| R03 | No added runtime constant/default; same one-slot handoff and caller deadlines. |
| R04 | Three explicit read goals with original canonical state/typed controls; no boolean terminal mode. |
| R05 | Existing ACK/ACK+FIN ownership, no polling/cache/extra queue/manager; ROOT full audit owed. |
| R06 | Small local enum/caller change and clippy0; ROOT combined review owed. |
| R07 | Comment explains final ACK ownership and why the early return is necessary. |
| R08 | No comparative/speed/4–5× claim. |
| R09 | Exact live ROOT red retained; actual fixed green not run by implementer. |
| R10 | Repeatable original fixture + actual red and light logs/hashes; ROOT before/after/mutation owed. |
| R11 | GNU/musl compile only; actual seven-target/KVM/HVF/WHP unverified for this source; Mac ARM64 only. |
| R12 | Existing shared client/capture seam; no OS, grant, profile, trust/crypto or product change. |
| R13 | Leaf fixture/evidence only, no general framework or coupled newer fixtures. |
| R14 | No profiling/benchmark/alloc/fault/leak claim; ROOT owns all measurements. |
| R15 | No agents/audits/heavy/actual helper/main merge; signed named-path branch checkpoint, ROOT validates/merges. |
| R16 | Reused worktree, admitted private remote light; no local compile/full worktree/deletion, ROOT targets and paused VM preserved. |
