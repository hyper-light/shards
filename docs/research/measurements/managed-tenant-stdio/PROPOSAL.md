# Inherited output and actor stdio closure: minimal owned-fixture shapes

Proposal only, submitted before any new native helper mechanism. POLICY_READ
IMPLEMENTER canonical main24f48ee5ebf198485117aa93bf47685ad7d7d53f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
ROOT owns actual VM/socket/namespace/mutation/cleanup runs, audits/profiles and
main merges; none executed here. No production decision/fix or grant widening.

The Linux pipe contract reports EOF only when all descriptors referring to the
write end have closed. Closing fd1/fd2 is distinct from ending the process.
`Stdio::inherit` explicitly inherits the parent's corresponding descriptor.
These facts motivate two separate, bounded native cases; they do not prove the
current Shards lifecycle/capture path behaves correctly.

## A: owned inherited writers after the parent's stdio closure

Keep the actor parent alive as the sole Child owner. Fresh-exec exactly one child
through std::process::Command with inherited fd1/fd2, one private piped stdin
handoff, fixed work/cleanup deadlines, explicit task/report-byte inputs and a held
pidfd. No shell, raw fork, pre_exec, new manager or authority change. Parent flushes
its controlled prefix, closes only its own fd1/fd2 and sends the private one-shot
closed-stdio handoff. Child uses deadline readiness on that handoff before
acknowledging Ready on its explicitly declared owner-control endpoint. This is
the proposed new helper mechanism and requires ROOT approval.

After Ready the host separately proves sibling and main progress, then sends the
owner release. Only then may the child write the exact binary suffix to inherited
fd1/fd2 and exit normally. Parent uses held pidfd readiness plus actual Child.wait,
then exits. Before release, do not assert absence from an empty event queue; the
decisive positive witness is the child's later exact suffix before canonical End.
End/EOF must follow that suffix at the exact payload tail. Keep inherited output
labelled member/channel data, not an exact tool/descendant-writer guarantee.

At most one owned unreaped child exists. Error paths retain ownership, target only
that child for kill and finite cleanup under the original cleanup deadline, report
unreaped Unknown honestly and admit no next child. Parent exit with an unowned
orphan is deliberately excluded from this minimal shape; ROOT may request that
separate lifecycle case later. No core-producing fault or host setting change.

## B: actor output EOF while independent actor control continues

No child is needed. The literal actor writes a prefix, flushes and closes only
its own fd1/fd2, then remains alive serving one explicitly declared finite owner
control exchange. Host requires exact canonical End/EOF on both output channels,
then sends a fresh unpredictable caller-supplied challenge over that control
endpoint and verifies the exact reply after EOF. Independently prove sibling/main
progress after EOF, then ordinary owned stop/remove and joins/FD return. Output
EOF must not be interpreted as losing the lifetime owner or as whole-population
completion. ROOT's independently retained actor/VM facts decide actual lifecycle.

Both cases need caller-selected distinct actor/sibling control ports if both are
used, positive finite deadlines, private record directory and payload/report-byte
envelope validated before effects. Before code, verify exact current NETWORK,
EXPOSE --agents/--harnesses and CONNECT grammar for those two ports. The current
`managed_limits.rs::agentfile` uses repeated `--ingress=ACTOR --ingress=HEALTHY`,
separate `EXPOSE --agents=limited ACTOR AS ingress FOR control` and
`EXPOSE --harnesses=healthy HEALTHY AS ingress FOR control`, plus each member's
own CONNECT declaration. Its negative corpus rejects comma-separated ingress.
Use that checked form with this fixture's helper/driver names. Reuse checked
grants, refuse unsupported seccomp/grants without widening profiles. Capture all
raw records/metadata/errors before assertions and always attempt owned cleanup.
Never relax the existing normal ended/tail-complete or exact-byte checks.

ROOT owes actual compatibility, failure-before-release, guard-removal/wrong-byte,
last-writer closure, lifecycle independence and complete owned cleanup evidence;
seven-target/real-host gates remain unverified. Implementer light checks only via
NEW private source/target after Kiln1CPU/1GiB/2GiB admission. Mac builds held below
30GiB; protected ROOT caches and paused Windows ARM untouched.

Primary sources read on2026-11-10:
[Linux pipe semantics](https://man7.org/linux/man-pages/man7/pipe.7.html),
[close and descriptor reuse](https://man7.org/linux/man-pages/man2/close.2.html),
[Rust Stdio::inherit](https://doc.rust-lang.org/std/process/struct.Stdio.html#method.inherit),
[Rust Child cleanup ownership](https://doc.rust-lang.org/std/process/struct.Child.html),
[Shards managed contract](../../../design/managed-runtime-plan.md).
