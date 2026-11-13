# Step 10 HVF campaign r411 (ROOT): adas-mac-studio.local, Apple M4 Max, macOS 26.2, Shards and harness 66113379c

Run as a launchd job, `--vms 32 --rounds 5`, 1 vCPU and 512 MiB each. Raw data on the Studio in
~/s10-run-1794523928/ (summary.txt, records.jsonl).

Each figure is a p50; p99 follows where it's given.

| Metric | g (TLS ends in the guest) | h (TLS ends at a host endpoint) |
|---|---|---|
| Memory per VM, idle, connected, over its baseline | +1817 KiB | +302 KiB |
| Host handshake CPU per VM, first | 3231 µs | 1590 µs |
| Host handshake CPU per VM, reconnect | 1573 µs | 936 µs |
| Client handshake CPU, first | 674 µs | 529 µs |
| Handshake, first | 14.1 ms | 19.1 ms |
| Handshake, reconnect | 3.0 ms | 5.7 ms |
| Connect to first round trip, first | 15.1 ms | 24.1 ms |
| Connect to first round trip, reconnect | 3.9 ms | 9.0 ms |
| Round trip, 64 B | 435 µs (p99 4572) | 1505 µs (p99 8658) |
| Download | 300.6 MiB/s | 326.6 MiB/s |
| Upload | 320 MiB/s | 273 MiB/s |
| Idle CPU per VM, connected | 0.3 % (baseline 0.3) | 0.0–0.1 % |
| Guest program Pss | 1562 KiB | 1220 KiB |
| Guest program RssAnon | 288 KiB | 92 KiB |

So far on HVF:
- g is ahead on latency: 3.5× better round trips at p50, and faster handshakes.
- h is ahead on what each VM costs: 1.5 MiB less memory per VM, half the host's handshake CPU, and faster downloads.

KVM (adacachy2, r653) and WHP (jupiter, after its gate) are still to come.
