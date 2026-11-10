# Next finite boundary proposal: empty-pending FIN

PROPOSAL ONLY. No new Rust source/shim/actor for this case. The approved Full /
actual write0 / VM mirror campaign has passed at82019; its original Clippy101
and separate4ef assertion-rendering correction are retained. The correction's
r151 source preparation is blocked in git fetch, not compiling. ROOT has the
exact owned-process/source-transfer decision; no process has been stopped.

## Concrete unverified boundary

The retained actual FIN cases submit nonempty consumed/truncated control bytes
before finishing. The current native Subscribe cases submit58B without FIN.
None exercises the shared Client::flush finish arm with an actually empty pending
buffer. This is a coverage gap in the existing concrete test caller, not a source
claim of a defect or a new product contract. Empty FIN must not be counted as an
Io::write return0; that distinct goal now has actual55/0/3 and43/0/15 receipts.

At main a920 (unchanged production in4ef), remote/src/net.rs Io::finish398 calls
the real QUIC send-stream finish. managed.rs Service::readable738 processes actual
Read::End, then refuses Reason::Closed at765. disconnect leaves an already
Finished slot intact. Shared Client::flush finishes after its pending loop.
These are source facts; the proposed conjunction still needs an actual case.

[RFC9000 section2.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-2.4)
separates writing from clean stream termination. [Sections4.5 and19.8](https://www.rfc-editor.org/rfc/rfc9000.html#section-19.8)
define final size from offset and length, including empty stream data. Those
transport semantics do not establish Shards' application refusal or lifetime
behavior; the owned actual Endpoint remains their authority.

## Minimal unapplied seam and caller

Add only actual pending length to the EXISTING cfg(test) Finish receipt:

```diff
 enum ControlReceipt {
     Finish {
         error: Option<String>,
+        pending: usize,
     },
 }
 // Immediately after the existing real io.finish(conn, stream) result:
 ControlReceipt::Finish {
     error: result.as_ref().err().map(ToString::to_string),
+    pending: self.pending.len(),
 }
```

The shared receipt JSON includes that actual value; existing positive/FIN
accounting matches Finish{error,..} with ALL existing conditions/counts intact.
No constructor, parser, transport, queue, capacity, callback wait or manager
changes. This is a small test-only receipt proposal, not an applied patch.

One new explicit native selection after the existing actual Open/Data/Aborted
errno5 + ACK + transport End, while authoritative Control is Finished90/90:
send the SAME Ask::RawControl with an actual empty Vec, finish:true and the
existing receipt sink. The existing bounded-command/pending-empty checks apply.
Use a separate empty-FIN receipt assertion for this selection only; do not relax
Writes::new(0)'s existing rejection or any positive-write assertions. Require
exactly one actual successful Finish receipt with pending0, actual peer typed
Closed (not an assumed local success), no future Record, unchanged supplied
canonical Aborted(offset26,errno5) and Finished90/90. The trace and actual pending
snapshot establish which branch ran; absence of a Write receipt alone is not
evidence of backlog state or an Io::write return0.

Then require one exact owned postterminal Data return as Closed, observer
stop/join preserving Finished, owner abort preserving cursors/finish Closed,
endpoint join and FD4→4. Save empty submitted bytes, actual receipt/refusal and
all canonical/state/returned-byte/cleanup witnesses under a fresh run/endpoint.

## Bounds, alternatives and gate

One native stdout/errno5 case only: three canonical records, one empty control
command, one FIN, one postterminal returned record. Same private admitted Linux
2CPU/4GiB/24GiB/j4, build<=3600s, case240s+cleanup15s/direct outer255s; no retry,
traffic inflation, synthetic return, sleep, polling or replacement connection.
No VM mirror or new fault/held-helper/cotenant scenario is proposed here.

Alternatives: AckAndFinish necessarily submits control bytes; Cancel closes the
connection instead of exercising send-half finish; a fake finish bypasses the
real transport. None verifies this exact empty-pending branch. Forbidden effects
are bogus success/refusal, erased/substituted canonical cause, new credit/payload
after terminal, finish-after-abort resurrection or missing owned cleanup.

ROOT approval, current-policy reread and exact new source/shim/runner/target
readback are required BEFORE this proposed Rust instrumentation/case. ROOT retains
integration, audits/profiles/benchmarks/mutations/matrix/main merge. This proposal
does not release the current lane or relabel any unrun gate as passed.
