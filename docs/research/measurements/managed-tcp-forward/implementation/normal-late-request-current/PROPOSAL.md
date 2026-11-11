# Ordinary late request suffix after backend EOF

Source-only proposal for ROOT review. No new Rust source, build, native actor,
runner target or campaign clock exists. The response-limit mutation work was
explicitly skipped by the user before dispatch; its local drafts are preserved
under reverse-extra-current/mutation with DO-NOT-RUN.md. This proposal exercises
ordinary permitted TCP behavior on fixture-owned endpoints, with no guard removal,
hostile control input, fault injection or previously rejected campaign.

POLICY_READ full current origin/main0a6d91b3de0f10a8397868aa3794adab1e09afdc
AGENTS/R01–R16/evidence/CLAUDE/managed plan and updated root handoff, rules SHA256
 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Role implementer + assigned adversarial relay tester. ROOT receipt9831 records
full reading; receipt9944 identifies this additional proposal-only owned path.
Own only this new evidence leaf. No runner/target is admitted or requested yet.
ROOT owns scope review, audits/profiles, product/combined validation and adoption.

## Additional normal behavior to exercise

Existing `client_exchange(BackendFinFirst)` receives the entire response and EOF
before sending any request bytes. Existing ServerFirst sends all request bytes
before receiving the response tail. The reviewed reverse-extra-before-FIN case
has all request bytes accepted before a typed reverse limit fault. Their actual
results are retained separately; none is a result for the proposed interleaving.

This proposed case sends one caller-sized upload slot before backend EOF and the
rest of the request afterward, while preserving the independent TCP half-close
policy. It uses the same Endpoint, Worker, native_inputs, start, shared pump and
absolute Deadline. It does not alter byte ceilings, abort policy, controller,
production code, visibility, notification, queue, timer, process or thread model.

Only a future cfg(test) native function would be added, proposed name:
`native_tcp_late_request_suffix_after_backend_eof`. No new Scenario enum/parser
variant or production hook is needed. Existing caller storage and two finite
slots remain unchanged. This is not the unresolved pressure induction case.

## Exact finite input and sequence for review

Reuse caller request13 `5245515545535400ff0d0a0102`, greeting8
`53455256455200ff`, response tail6 `5441494c0d0a`, upload/download slots3/5.
The split is the existing supplied upload slot, not a selected production number:
checked first3 `524551`, checked remaining10 `5545535400ff0d0a0102`.
No extra application byte is sent. All ports are endpoint-owned OS-assigned
loopback ports. Namespace, BPF, external service and passed-FD paths are absent.

1. Reserve the existing caller-sized Endpoint storage/slots before release. Start
   the same owned backend Worker and relay Worker, using existing explicit finite
   input, one real Deadline and the ordinary caller cancel descriptor.
2. Backend sends greeting8. Client receives8 and sends prefix3. Backend independently
   receives exactly3, then sends tail6 and performs ordinary SHUT_WR. Retain the
   actual prefix receipt while its read side remains open.
3. Client independently receives the rest of response14 and an actual nonempty
   read returning EOF. Record client writeEndOpen, written3, read14, extra null.
   These observations precede any remaining request write.
4. Client sends the exact remaining10 under that SAME original Deadline, records
   actual successful total written13, then performs its late FIN. No retry or
   enlarged wait follows a failed write or unexpected reset.
5. Backend independently receives the remaining10, compares the full original13
   and observes actual EOF. Preserve send/receive/close results as data. Close owned
   endpoints, join existing workers, record actual flow and descriptor return,
   held direct-child pidfd exit readiness/reap and parent FD return before final
   strict decisive assertions.
6. Require typed BothHalvesClosed/no fault/Abort::NotRequired; request read/sent13,
   response read/sent14, both directions pending0/extra_consumed0 and WriteClosed
   with source EOF. Do not infer consumed bytes from an internal counter: require
   independent full backend13 and client14 byte receipts as well.

Expected transitions are a prediction, not an actual pass. A missing EOF, reset,
failed suffix, wrong result or incomplete cleanup is a retained first failure.
No synthetic readiness, queue tuning, sleep, polling scheduler or automatic
runtime repetition is proposed. No production fix follows without actual evidence
and a separate corrective design.

## Eventual finite execution, if separately assigned

After ROOT reviews a concrete source diff, a possible assignment is ONE new GNU
native case plus an ordinary forwarding control, private new source/target/output,
2CPU/2GiB/4GiB/j2, total600s and original native30s/parent45s. These are proposed
caller bounds, not an admission receipt or hard physical/kernel guarantee.
Freeze exact source/package/binary/list1 and send the receipt before actors.
There is currently no permission to execute this proposal or to rerun v6/old r2.

The implementation checkpoint remains d5b7a801 guarded source with the original
unchanged083b pump/0502 engine. Source inspection uses actual client_exchange,
Endpoint::receive/eof/record, native_tcp_pump_both_half_closes and EndPolicy::Independent.
The actual nineteen-of-twenty pressure corpus, old engine VSOCK comparisons and
current reverse-extra proof keep their original provenance. No new matrix, musl,
VSOCK, interception, resource, performance, cleanup universality or lane-closure
claim is made by this proposal. Previously rejected campaigns remain held.
