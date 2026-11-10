# Exact codec input dependency — no emission executed

The ordinary ARM Mac fixture must not create a second record codec merely because
the existing guest broker is Linux-only. ROOT has assigned the audit owner finite
cfg(test)-only authoritative corpus emission from the SAME existing
encoder/count/shape functions, using fixture
member IDs1 and3. This lane keeps its native proposal symbolic until those actual
bytes/bounds arrive. Nothing here permits this lane to build or add a guest seam.

Read frozen `ba78d4ba775e7d83b7fea9a5989e3ca6b9857a2a`:

- `crates/server/src/broker.rs`: `AuditReservation::reserve`, `Count`,
  `intent_record`, `terminal_record`, `terminal_shapes`, `Receipt` and actual
  `Format::Program` rendering.
- `crates/server/src/broker/audit.rs`: `Status::write`, error-shape enumeration
  used by `terminal_shapes`, and existing shared wire types.
- `crates/server/src/main.rs`: broker and native-effect modules are Linux-only.

The counts are dependent on this exact encoder and identity input. Do not reuse
numbers from some other broker/receipt or claim a source-only count was emitted.
An accepted corpus must contain original raw output from the unchanged real
functions, with source/build command/toolchain/result and artifact hashes.

The earlier proposed synthetic input shape below is illustrative. The audit
owner's actual frozen input/output is authoritative, and must be read before
implementation; no different local emission or substitute input is authorized:

```text
Receipt:
  principal = "native-member1"
  peer = "native-member3"
  operation = Operation::Send
  request = 0                       // first call, same as Journal's first call
  effect = Effect::NotStarted
  audit = audit::Status::Reserved
ordinary intent:
  packet_limit = b"audit-wire-proof".len()
  object = None
ordinary terminal:
  same receipt, audit = IntentSynced
  outcome = Failed(Refusal::at(Executing, Capacity))
```

These are encoder inputs, not an actual attempted Send, allocation fault, tool
execution or semantic effect observation. The framed native case submits only
their encoded data to its own two audit files. The eventual evidence must keep
that provenance explicit, including the deliberately synthetic terminal cause.

Emit/check using actual private functions from an owner-approved existing test
context, with no production visibility/method/codec refactor:

1. `Count` through `intent_record(receipt, usize::MAX,
   Some((u64::MAX,u64::MAX)))`, as the real reservation does; materialize those
   same bytes using the same encoder and require exact length equality.
2. For every actual `terminal_shapes()` item, set receipt effect to the returned
   shape and audit to IntentSynced; count and materialize using `terminal_record`.
   Require every counted/materialized length equal and derive Tmax as their max.
3. Materialize the ordinary intent and terminal above with those exact encoders.
   Check nonempty bytes, Ilen<=Imax, Tlen<=Tmax and positive retained slack.
4. Preserve all raw shapes, ordinary bytes, exact input fields and indexed file
   length/SHA256. Record 64-bit architecture; no other architecture inherits
   usize::MAX shape bounds.

The Mac fixture consumes these immutable byte files as test data, derives Imax
and Tmax from their actual extents, checks the emitted count receipts and hashes,
then computes its one B. Its audit-control Requests and Replies still go through
the real shared ABI codec. There is no production runtime dependency on this
manifest, and it never selects scope or confers a storage right.

Verify SHA256 and the manifest outside the native binary during source freeze.
The eventual leaf uses `include_bytes!` for actual codec output and original
emitter-generated Rust integer count data for length equality; it does not add
a JSON manifest parser, SHA implementation or Cargo dependency to the VMM test.

No emitter, fixture leaf, corpus binary, guest seam or API change is included in
this proposal checkpoint. ROOT's audit owner supplies the frozen exact corpus
before dependent implementation; unknown bounds remain unknown. The header,
slot, FUSE, cleanup and source preparation are independently complete for review
without claiming the eventual case was compiled or run.
