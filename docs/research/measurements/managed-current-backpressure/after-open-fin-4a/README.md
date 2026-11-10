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
