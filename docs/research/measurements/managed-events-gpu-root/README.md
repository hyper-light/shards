# Metrics and GPU core integration candidate

This isolated ROOT candidate composes the byte-identical a8ad cgroup event
implementation and the two-file GPU core patch e506d40. It changes no admission,
helper, credentials, native executor, renderer selector, GPU ABI or dependency.
Main remains749c3ca06 until the relevant combined gates pass.

ROOT recovered and verified all95 original a8ad payloads plus their manifest.
They retain35 metrics-unit passes, native init/shards strict0, the normal carried
build and matching guest, and26 ordinary KVM passes with10ignored/noSKIP.
Actual kernel and exported task-event values changed0→1; both cotenant values
remained0. Child creation refused with EAGAIN11 at the observed task ceiling,
then succeeded after reset with both original stdout/stderr markers. Every
recorded direct child was pidfd-ready/reaped with controller FD return. These
are exact a8ad results, not a passing gate of this newly composed candidate.
The original complete archive, manifest and ROOT receipt are retained here.

Official nine-artifact decoding/Collector acceptance and ROOT verification
already landed in main749c3ca06 under managed-otel-cgroup-events/official-current.
Production HTTP export, generation/continuity, hard resource rights and all costs
remain open. ROOT's a8ad data-integrity mutation/restoration runs separately as
r61; no outcome is inferred from admission.

ROOT also verified every286 GPU evidence file and42 selected raw files, including
actual stdout counts. Exact final GPU core bytes have native14/0/0 Mac ARM64,
15/0/0 GNU and15/0/0 musl results, plus native Mac strict0. Empty descriptor
responses now use checked borrowed slices. Successful context destruction yields
to the existing signal drain before a later request reuses its ID. This closes
the retained queued-before-return component case; first-late-after-return backend
callbacks, actual GPU guest/driver behavior, restore, physical selection and
renderer lifetime remain unverified. No L2 producer patch is imported here.

R01/R04: checked bounds, typed failures/private explicit completion transition;
existing allocation hazards remain unproved, no production panic introduced.
R02/R03: original pinned research/D229/spec and live component records retained;
no new production literal, grant or architecture. R05/R06/R07/R12/R13: existing
Reader/encoder/device worker and platform seams; no manager/cache/polling layer.
R08/R14: all new comparative and allocation/fault costs remain open. R09/R10:
original red/green/failures/source identities retained; corruption and restored
results pending. R11: current combined seven-target/GPU and real-VM gates open.
R15: ROOT owns this integration, audit, gates, profiles and merge; no prior source
result is relabelled. R16: fresh source tree, no local Rust build and no source,
evidence, frozen binary or library deletion. Paused WinARM and all held campaigns
remain untouched. FullP0–P13/G25 is incomplete.
