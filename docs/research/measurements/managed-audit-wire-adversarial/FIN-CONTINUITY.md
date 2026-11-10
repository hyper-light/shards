# Original FIN campaign outcome — immutable pointer, no rerun

The separate FIN campaign stopped at its original deadline. Its signed/pushed
checkpoint is `9045904db87ad94673ffa7aa2b02f35994fd7f70` on
`p/managed-after-open-fin-proposal-v1`, in the preserved
`managed-network-assertions` worktree. Original source is
`9764144d5a7170c1ba6607ecd0222a0015db12de`; the frozen real caller binary SHA256 is
`ab5d5820d183da87955f75aa3b31a1a85f39a94319e6ffe88ac9e33c4c791a17`.

Immutable Git evidence at9045904, under
`docs/research/measurements/managed-current-backpressure/after-open-fin-4a/`:

- `CAMPAIGN-STOP.md` and `CAMPAIGN-STOP.json`;
- `original-budget-headroom-observations.json`;
- `execution-source.json` and `stop-evidence-check.json`.

Original budget: start1794349989.118, expiry1794353589.118
(`2026-11-10 23:33:09.118 UTC`). First expired clock receipt was23:33:17UTC,
about60m08s after start. No time extension, native-case execution or retry followed.

Actually executed: remote policy0; initial tool preflight1 before Rust compile;
new private verified pinned-Zig setup and full carried caller no-run build0,
with setup/compile448.379657s. Pure, strict Clippy and native case gates did NOT
run: normal admission for the unchanged2CPU/4GiB/24GiB ask repeatedly reported
zero idle CPU until expiry. Actual native cases0. Status is
**Stopped / CoverageMiss**, not a passing after-Open FIN test.

The original source, targets, compiler/toolchain/artifacts/raw records remain on
Adacachy `451397fcc585f7f7cb9aaf00e42064bb` at
`/home/adalundhe/managed-after-open-fin-4a-v1`. No evidence archive was generated
or transferred for this campaign; full runner raw retrieval/ROOT review remains
owed. No job was running to stop at the expiry checkpoint. No Fedora scope was
created, no read-only home was written and no source/cache was deleted.

The new audit-wire proposal starts separately from8c817dba and inherits none of
that compile result or any older postterminal/FIN/VM/matrix result. Original
positive postterminal proof, historical continuation and all old raw remain tied
to their original revisions. No lane release or management completion is claimed.
