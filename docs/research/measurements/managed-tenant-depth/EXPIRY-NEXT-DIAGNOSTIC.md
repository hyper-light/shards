# Minimal next diagnostic proposal for ROOT

Actual single-case RED: EXPIRY-RESULT.md/source bbd/r79, receipt6912. This file
proposes the missing observation, not a product fix or another authorized agent
attempt. No source implements it; the one assigned actor marker stays consumed.

At this source, Capture.receive calls Observer.next and immediately destructures
Record in a let-else. The else panic loses the actual Lost(code), Failed(text) or
End value. No canonical frame was delivered, so neither wire bytes nor successful
driver join recover that event. Timing and co-present memory logs cannot select
the missing variant/cause. Parent Capture/Observer paths are outside this lane's
approved leaf-only edit scope.

Propose ROOT's smallest fixture diagnostic in tenant_exit.rs::Capture::receive:
retain the returned Event before that same Record check; on the existing strict
failure, write its explicit variant/code/cause, member/channel, last canonical
state and consumed wire offset to the already supplied trace directory, then
keep the failure. Existing assertions, offsets, parser and transport remain.
Do not consume a second event, add a wrapper/observer, substitute a guessed error,
alter grants/idle behavior/deadlines, reconnect to obtain a pass or change native
expiry/ownership. ROOT reviews any error-text privacy before retaining it.

Only after exact diagnostic source/light checks and a fresh explicitly authorized
bounded campaign may ROOT obtain that missing live receipt under the same T120000/
host+sibling240000/cleanup15000/hard255 contract. This lane does not rerun it under
the spent one-case authorization. Typed cause and actual effect evidence must
precede choosing any production or fixture contract change; no idle-time/memory/
clock classification or broader completion claim follows from the current RED.
