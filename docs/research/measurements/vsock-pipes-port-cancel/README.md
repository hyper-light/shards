# A cancelled pipe read on a completion port's handle (D272)

Does a cancelled overlapped read's end reach its OVERLAPPED before anyone takes its packet from
the I/O completion port the handle is associated with? `probe.rs`: a byte-mode duplex instance
opened for overlapped calls, its client opened as `std` opens a file (no FILE_FLAG_OVERLAPPED),
a 4 KiB ReadFile under way on the server end, CancelIoEx, then GetOverlappedResultEx waited for
3 s; after it, whether the port holds the read's packet (GetQueuedCompletionStatus, no wait).
Three cases: no port; a port, the OVERLAPPED naming no event; a port, the OVERLAPPED naming a
manual-reset event.

Host: jupiter, Windows 10.0.26200.9974 (AMD Ryzen AI Max+ 395), rustc 1.99.0, 2026-11-16,
lane whp-verify, Shards f8f95832f. Run: `rustc -O --edition 2024 probe.rs -o probe.exe` then
`./probe.exe`, probe.rs sha256 994ee816fe44a5dc09f19190b50634a19f412004e55b69740ed9dd00bde2277f.

```
plain: made 0 read=0/997 cancel=1 wait(3s)=0/995 after 800ns internal=0xc0000120
port: made 0 read=0/997 cancel=1 wait(3s)=0/258 after 4.7008807s internal=0x103 port packet=0/995 key=7
port-event: made 0 read=0/997 cancel=1 wait(3s)=0/995 after 1.3µs internal=0xc0000120 port packet=0/995 key=7
```

- No port: the read is cancelled at once (ERROR_OPERATION_ABORTED, STATUS_CANCELLED written).
- A port, no event: the wait times out (WAIT_TIMEOUT, 258), the OVERLAPPED still saying the
  read is under way (STATUS_PENDING, 0x103), its handle never set; the cancelled read's packet
  waits in the port (995, key 7). Its end is written as its packet is taken.
- A port and an event: the read is cancelled at once, as with no port, and its packet still
  goes to the port.

What it settled (D272, amended 2026-11-16): `platform::Pipes` and `Listening` cancelled a call
and waited for its end on its handle (`settle`, GetOverlappedResult with bWait 1) on handles
associated with the vsock device's port, whose one taker is the device's worker, the thread
waiting: on jupiter three muxer unit tests and the WHP vsock E2E hung there (cdb: the
`virtio-vsock` thread in GetOverlappedResult). Each call's OVERLAPPED now names an event of
its own.

Earlier runs of the same steps that night printed the same three outcomes (the port case's
3 s wait timed out after 4.8 s); this file's numbers are the committed probe's run.
