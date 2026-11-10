# Stopped: original deadline coverage miss

Original deadline **2026-11-10 23:33:09.118 UTC** expired before pure/Clippy.
First actual clock receipt after expiration was23:33:17 UTC, approximately
**60m08s** from the recorded original budget start. This campaign is STOPPED,
with outcome **CoverageMiss**. No additional campaign gate may launch under it.

Source remains immutable9764144d5a7170c1ba6607ecd0222a0015db12de, adacachy451397,
scope `/home/adalundhe/managed-after-open-fin-4a-v1`. Native leafcd3c763c... and
shared client6add609e... are unchanged. The frozen actual fullcaller executable
is artifacts/managed_output, SHA256
ab5d5820d183da87955f75aa3b31a1a85f39a94319e6ffe88ac9e33c4c791a17.

| Gate actually executed | Outcome |
| --- | --- |
| r171 policy | 0 |
| r171 original tool preflight | 1 before compilation: host Zig0.17 versus pinned0.16; retained |
| r172 private pinned compiler | Official0.16 digest/version verified, no host/global install |
| r172 normal carried fullcaller compile | 0; Cargo5m09, pinned setup+compile448.379657s |
| Source-matching pure tests | NOT RUN: normal original-envelope admission refused |
| Strict fullcaller Clippy | NOT RUN: same admission refusal |
| AfterOpenEmptyFinish native case | NOT RUN; actual executed count0, no job ID |

The last actual headroom receipt before expiration,23:32:56 UTC, still reports
adacachy0idle under the original2CPU/4GiB/24GiB requirement. No smaller ask,
admission bypass, executed-case retry or time extension occurred. There was no
running check/native job or actor to kill. No cache/source/artifact deletion or
cleanup was performed. The read-only Fedora proposal was explicitly denied;
there were ZERO Fedora operations and no new or reused scope there.

[CAMPAIGN-STOP.json](CAMPAIGN-STOP.json) retains the exact clock, original budget,
gate states, source/artifact identity and limits.
[original-budget-headroom-observations.json](original-budget-headroom-observations.json)
retains the actual read-only clock/headroom receipts. Prior successful build,
wrong-tool preflight and both real admission refusals remain unchanged.

Full original build/compiler/source/provenance records remain on adacachy in
records/build and records/build-pinned-zig-v1. They have not been transferred or
independently verified locally; the CLI hash remains there, and no new raw
archive was created. The signed local build receipts retain their actual output
and executable hash. Sources, target, Cargo/Zig/compiler/runtime artifacts,
binaries, logs and all historical references remain preserved.

The after-Open/no-ACK FIN, retained record, replay/ACK credit, owned-return,
joins/FD effects are UNVERIFIED for this new case. No production bug, protocol
pass, no-effect or cleanup guarantee is inferred from0 executed cases. ROOT's
later Capturing integration and historical2c continuation remain separate.

R01–R16 accounting and source/interface choices remain in PROPOSAL.md; this
record adds the actual R10/R15 budget miss and R16 preservation facts, not a
waiver or passing substitute. No new source/test/VM/mutation/profile was run.
Current policy is mainca3ab10c151aa5539e757f1cc76029746ccbb561/rulesdf2acf47...
as acknowledged before the final evidence edits.

No campaign restart is authorized. A new explicit ROOT assignment/budget is
needed for further execution; the overall management lane remains unreleased
and P0–P13 remains open.
