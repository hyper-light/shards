# Storm protocol candidate: native regression pending

The unchanged full `real-04` run's snapshot binary finished nine cases passed, one
failed and two ignored. The busy original guest passed at the default 2, 8 and maximum
64 vCPUs. The busy-restore case failed while the host awaited `storm checked` from a
64-vCPU restored guest.

The captured guest stream contains the completion text split by concurrent kernel
timer diagnostics: `RDS-TEST INFO st` followed by a printk record, then `orm checked`.
The complete literal marker is absent from the payload. The guest emits this marker
after its worker-progress, clock and disk-record checks; its host then stops streaming
and sends the byte that lets it finish. The split line prevented that handshake.
This evidence establishes a reporting failure; it does not prove the remaining
restore attempts or the complete suite pass.

The candidate emits the storm's bounded protocol messages through `/dev/kmsg` as one
LOG_USER/ERR record per write. The priority keeps them visible with `quiet`; printk
serializes them with its diagnostics. The diagnostic replay accepts kernel-facility
records only, so it cannot replay the test's own control messages. Linux documents
the record interface in [dev-kmsg](https://www.kernel.org/doc/Documentation/ABI/testing/dev-kmsg).
Worker behavior, the host parser, assertions, CPU counts, three restores per CPU count,
and all deadlines are unchanged.

All eight lint targets, the arm64 Linux guest build and a fresh 142-executable Windows
cross-build passed. Every Windows executable is byte-identical to the running `b5b18c94`
payload. The candidate guest is separate and immutable; `candidate.json` records both
guest hashes and the exact source patch. These are build checks, with no candidate
VM pass yet.

The existing full run keeps its original guest. The queued diagnostic requires that
same task to become terminal with all 142 native records and an unfiltered completion
footer, and fetches its complete output archive before installing the candidate in a
separate kit. It then runs the original busy-restore case with its default 2, 8 and
maximum 64 vCPUs and all three restores each, as the same standard user at normal
priority. A selected pass will still require a new complete real-VM suite before
step 10 can close.

The committed-view transcripts normalize line endings and display padding; capture
and display hashes are recorded. Original bytes remain with the local measurement.
