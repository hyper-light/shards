# Recovering native evidence while SSH is unavailable

The original full run continues. The capture at 2026-11-04 07:57:54 UTC contains
84 completed binary records: 81 passed and three failed. Its controller reports
`daemon` PID 4984 still active through its `WaitForExit` heartbeat. The earlier
`containers` binary completed 131 passed, nine failed and eight ignored in
13,098.38 seconds, with native exit 101. Those nine failures exhausted the original
command deadlines (60 seconds, or 240 for the resource-limit allocation). Their
causes remain unestablished. No workload, assertion, CPU count or deadline changed.

SSH connection failures prevented direct task queries. The measurement therefore
uses a temporary, local Unix-socket NBD export of QEMU's existing `hd` device. QEMU's
`nbd_server_add` defaults to read-only unless `-w` is supplied; the harness supplies
neither `-w` nor `-a` and independently requires the server's READ_ONLY flag before
reading. Its client implements only READ and DISC. It validates the GPT CRCs, parses
the Windows data volume with a file-like read-only NTFS reader, and reads named
native result files. It makes no disk copy, filesystem mount, VM snapshot, VM stop or
native task action. It does not open password or protected credential files.

Each capture closes its client and server and verifies that the temporary listener
has closed. The layout control read 16,896 bytes in two requests. The later periodic
capture read 318,976 bytes in 55 requests; its export flags were `0x50f`, including
READ_ONLY. The native file hashes, byte counts, missing completion marker and export
cleanup are in the capture JSON files. These are flushed bytes from an active
filesystem, not an atomic snapshot or a direct query of scheduled-task state.

Controller output grew between captures, and the completed-record count advanced
from 81 to 82 and then 84. The controller emits `ALIVE` only while its original
child's `WaitForExit` returns false. This establishes continued native activity;
it does not turn partial results into a complete-suite pass. The read-only watcher
retains another capture every ten minutes. It stops only after all 142 ordered
records, the matching unfiltered completion footer and native child exit marker
are on disk. It starts no regression, deploys no candidate and infers no goal pass.

The separate System and OpenSSH event-file capture remains local. Its selected
records retain the known 02:23:59.782587 UTC boot and the last accepted key at
05:51:20 UTC, followed by explicit `MaxStartups` refusals. The event parser returns
lazy substitutions reused by cached templates: retaining those references changed
earlier decoded fields after iteration. The first interpretation was rejected.
The committed parser materializes every field immediately; the mutation control
is recorded, and only that corrected interpretation supplies the selected events.
These logs do not establish why the SSH workers stopped completing handshakes.

`harness/` preserves the executed local layout. The final capture harness's hash is
recorded by `readonly-progress-0001.json`; the earlier captures predate that metadata
addition and are not assigned a retroactive source hash. Their raw native file
hashes and recorded read-only/cleanup observations remain intact. The parser versions
are pinned in `native-event-selection.json`. Text views identify their raw and
display hashes in `transcript-format.json`. Raw EVTX files and framebuffer captures
remain local; no dump or credential is part of this bundle.

The shipping source remains `addc5047`. Its storm, client and assignment candidates
still require native validation. The current original full run already has failures
and client-related skips. Step 10 remains open, and this recovery does not change
the separate unresolved HVF/VHE causal result or the coordinator's combined-pass
requirement.
