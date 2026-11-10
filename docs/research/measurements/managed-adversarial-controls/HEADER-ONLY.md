# Complete control header with no cursor body

Selection `control-header-only` follows the actual prior4-byte header prefix
and12-byte consumed prefix cases. It covers the intervening concrete boundary:
the complete known consumed header, with zero of its declared u64 cursor bytes,
then actual send-side FIN after an observed canonical Open. The existing real
consumed encoder supplies the bytes; the retained header length derives from
size_of u8+u32. Checked frame peek must recognize Stdin with an8-byte body,
while the submitted actual buffer has only the5-byte header. No declared body
is allocated and no production literal/capacity/state changes.

The existing Service::controls parser returns incomplete until the cursor body
exists; Service::readable then handles actual End as typed Closed. That expected
contract uses the approved RawControl finish-after-pending path and actual
write/read/FIN receipts. It is not inferred from enqueue or local acceptance.
No retry, replacement connection, polling, timeout reset or new seam is needed.

One fresh owned run/endpoint per stdout/stderr selection must retain Closed4,
exact held accepted28/consumed0 with unknown tail, no future target records,
actual fresh controlled member2 fd1/fd2 and main bytes, owner stop/remove/unlink/
observer joins/FD return. Existing strict assertions remain. Native c705/79f
initial cursor/scope/stale cases and held helper/allocator campaigns are untouched.

Full normal caller build, two selected KVM cases, changed input pure checks
and scoped full Clippy are pending at this source checkpoint. Same explicit
positive finite bounds/private admitted Fedora scope as LIVE-9E7; no local
build or protected ROOT cache. Original patches and earlier sources/binaries/
records remain frozen; no main/common-module rebase. LIVE-9E7/LIVE-1E0 retain
primary contracts/alternatives and R01–R16 limits. This change owns only the
new leaf's contract/input children and new evidence. ROOT owns current combined
integration/matrix/mutations/audits/profiles/benchmarks and main merge.
