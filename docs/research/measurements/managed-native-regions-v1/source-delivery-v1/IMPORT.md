# Acquired region source handoff

Apply the two new leaf files and the cfgLinux module hunk only. Preserve newer
IPC registrations. Exact hashes and preimages are in MANIFEST.json.

The real protected producer supplies the exact nonzero representation length;
Region::acquire_zeroed acquires/locks/initializes the physical storage. This
length, the Context and Region itself grant no effect or Session authority.
Use bytes()/bytes_mut() as fallible borrows, with no resize or capacity inference.
Explicit release returns the same mapping in Retained if cleanup fails. Retain
that object in the actual owner/launch-failure lifetime, not a String/log or
reported byte count. Rejected::Retained preserves the acquisition cause as well.
Drop is only best effort and cannot refund a charge. Wrong-process access is
refused before touching the DONTFORK address.

ROOT's source is frozen for a new independent GNU batch: full IPC check/test
compiler, three new ordinary cases, strict, one exact fork guard mutation and
original guarded result. Formatter already passed; all compiler/runtime results
are pending. The batch imports no held audit composition or five cases. No
hard parent/kernel/transport, finite record or NativeSession readiness follows.
