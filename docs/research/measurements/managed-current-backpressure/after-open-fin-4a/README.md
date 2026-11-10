# Prepared after-Open empty-FIN case

[PROPOSAL.md](PROPOSAL.md) contains the exact source/interface and observation/
cleanup contract for ROOT review. **Uncompiled and unexecuted; preparation only.**
The single same-scope replay attempt has an explicit QUIC admission dependency;
failure must be retained without retry or a new Endpoint.

Base4a6c4321686fdcdf1db86a9b18325ec1427436bd preserves the completed
[postterminal empty-FIN result](../empty-fin-0342/RESULTS.md). That distinct pass
does not verify the after-Open/no-ACK state edge.

Owned Rust delta: native_terminal.rs only. Shared Client/Observer/RawControl/
Finish receipt and all positive accounting remain unchanged. See
[native-caller.patch](native-caller.patch), [source-scope.json](source-scope.json)
and [preserved-source.json](preserved-source.json). No new runner/target exists.

The immutable historical2c continuation archive/index paths and Git blobs are
in [continuation-locators.json](continuation-locators.json). ROOT verification is
pending; the original1513 payloads+manifest1514 total members were not regenerated
or adopted into this candidate.

ROOT subsequently approved ONE exact case: [GO-RECEIPT.md](GO-RECEIPT.md).
Execution source stays9764144d5a7170c1ba6607ecd0222a0015db12de. The initial
r171 preflight stopped on host Zig0.17 before compilation; its failure remains
retained. Normally admitted r172 uses private pinned0.16 and completes the actual
fullcaller build0 within the original deadline. The next same-envelope check
admission was refused at0idle; pure/Clippy/native remain unrun and no native job
exists. See [BUILD-RESULT.md](BUILD-RESULT.md), [RUN-HANDOFF.md](RUN-HANDOFF.md),
[PINNED-TOOLCHAIN.md](PINNED-TOOLCHAIN.md) and [execution-source.json](execution-source.json).
ROOT's later Capturing integration failure is separate, as recorded in
[INTEGRATION-NOTE.md](INTEGRATION-NOTE.md); the active campaign is not rebased.
