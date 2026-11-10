# Coordinator RPK compatibility and resource gate

POLICY_READ: coordinator, canonical c921a0b12b06d495b354907162e991376edffd41,
rules SHA256 1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.
The coordinator ran these heavy checks after the newest role restriction. No
implementer started another campaign. This record covers only the TLS policy pin;
it selects neither the managed endpoint topology nor a larger resource allowance.

## Decision and exact code

The existing RPK handshake encoded a 53-byte Certificate using a Brotli path that
requested a 33,554,432-byte Rust allocation. Its compressed Certificate body was
65 bytes. The live stack attribution, source revisions, alternatives, original
results and four independent guard-removal tests are preserved in
`p/managed-performance-rpk` at a875e8b0fe68434fc972093879818fa37b02bdb8:
`docs/research/measurements/managed-performance/RPK-POLICY.md`.

Root reviewed the complete upstream production diff: four conditions at existing
validated certificate-type seams. RPK receivers omit optional compression and RPK
senders choose an ordinary Certificate, including when old peers offered it.
Verification, trust, signatures, key material, compression quality/window, X509
behavior and existing size limits remain on their existing paths. No size cutoff,
new cache, resource entitlement or runtime constant is introduced.

[RFC 8879 section 4](https://www.rfc-editor.org/rfc/rfc8879#section-4) permits an
ordinary Certificate response. The policy also follows the explicit X509-only
compressed-send conditions in pinned OpenSSL 3.6.0
[client](https://github.com/openssl/openssl/blob/7b371d80d959ec9ab4139d09d78e83c090de9779/ssl/statem/statem_clnt.c#L402)
and [server](https://github.com/openssl/openssl/blob/7b371d80d959ec9ab4139d09d78e83c090de9779/ssl/statem/statem_srvr.c#L430).
The input-derived Brotli-window alternative was rejected: its large-input peak
rose from 87.8 to 213.8 MB. Primary-source alternatives, the 32-case old/new RPK/
X509 matrix, exact unchanged X509 wire, wrong-pin cases and mutations remain in
upstream `p/managed-rpk-negotiation` at 7cc9b56f9e4b20ef66d5e749d808ddb121b4b2f0.

The consumer pin is the exact tested production revision
233b7fcef7a363327c5439b191724a38065062cd, a child of the original pinned
f116ac276d2d53296de24c2a43837baf66ed1cbd. Only remote/Cargo.toml and its two
corresponding lock sources change. No unrelated upstream changes or measurement
hooks are adopted into the combined runtime.

## Central live results

Fedora runner 8fadc1ec213332e16cb0d915a3fb913f, Linux 6.19.10 amd64/glibc,
uid 1000, Rust 1.98.1. Each run was admitted at 4 CPUs / 8 GiB / 10 GiB.
The frozen, separately compiled role executables were SHA-verified before use:

| Role | SHA-256 |
| --- | --- |
| Old f116 | 54fd2548aaf799287b2fbf515bfe196e41563af9af7e99ac1ca276856d5cec08 |
| New 233 | 8a0a6b040ca13ec640c31b4bccc33f95402d36623c88b3c7a2c6069263bc10d4 |

The identical corrected role source has SHA-256
e16efa92d6d6a5a72e7a3892b1dfa3d77d890023b951a0183d48d41abcbe2f2e.
Its source-only checkpoints are p/managed-rpk-compat 2bc6da2881967fc630b5529d32e8953c1aa8bebe
and p/managed-rpk-compat-old 7bf115def108b9e61f481369c83161a1196b2661.
The executed campaign at candidate 8e45e0024a245282631af2af3db87a7e44fb6b5d is
byte-identical to the later checkpoint: SHA-256
dd52602ab6ebde58510840dc8cad947f745fc485c8b721e3f6db0a897ef30916.

* All 16 compatibility cases passed: both server/client version orders, two/four
  channels, normal/stalled output and wrong server/client pin. Actual binary
  bytes, application-consumed ACK, neighbor progress before owner EOF, typed
  abort and zero accepted/consumed payload for wrong pins were checked.
* Three additional resource repetitions passed all eight ordinary/stall cases:
  24 cases, 48 role processes. Combined with compatibility: 40 cases, 80 processes.
* Each role returned to its own initial FD/task tuple; the Python coordinator
  also returned to its initial FD/task tuple after every case. A stalled channel
  retained accepted=1024/consumed=0 while neighbors completed their 16 records.
* An earlier implementer run passed five cases, then exposed a fixture marker
  bug. Libtest prefixed the first record; the parent missed it and never issued
  owner EOF. The corrected central run includes that formerly failing case.
  It was an orchestration failure, not a TLS interoperability failure.

`results/` retains 465 raw logs, exits, commands and process/parent/socket records;
`SHA256SUMS` verifies them. Private fixture keys and binaries were excluded.
The verified transfer archive SHA-256 is
f45ddc0d82d3755eb5c5b46ecf56013b6f6711723b936d4097e5211e1c9f7149.
`summary.json` indexes all cases and boundary RSS/PSS/private samples. Its p50 is
the median; p90/p99 use nearest rank. Each resource row has only n=3, so tails
are the maximum and are not a stable percentile estimate or a performance claim.

## Reproduction

Use the committed `process-compat/campaign.py` and `build.py` at the source-only
checkpoints above, then the frozen matching-source executables. Exact build
commands and provenance are in their README. In an admitted private Linux scope:

```sh
export MANAGED_OLD_BINARY=/absolute/old-role
export MANAGED_NEW_BINARY=/absolute/new-role-corrected
export MANAGED_PROCESS_EVIDENCE=/absolute/NEW-UNUSED-interop
unset MANAGED_PROCESS_RESOURCES
python3 docs/research/measurements/managed-performance/process-compat/campaign.py
```

For resource observations, use a new unused prefix and
`MANAGED_PROCESS_RESOURCES=1`. Root used three repetitions, not a runtime tuning
value. Parent readiness is actual bound endpoints and observed record/completion
events; no retry polling, reconnection workaround or capacity increase is used.

## Limits and R01–R16 account

This is a Linux cross-process compatibility gate, not an exact combined-tree
seven-target or VM gate. Those remain owed after the pin. The ordinary candidate
remote corpus previously passed 36 tests; upstream has meaningful independent
mutations. Immediate reconnect during QUIC draining still refuses and remains a
separate unresolved contract. No shortened drain or increased admission is adopted.

Resource records include both roles, the parent, owned UDP queues and readable
cgroup files. Missing memory-controller files are explicitly unknown. Host-wide
sockstat, configured buffer limits and socket queue samples are not exclusive
charges. C malloc, copies, transient kernel/slab peaks, hard per-tenant accounting,
universal leak freedom and actual VM lifecycle cleanup are unproven here. No 4–5x
overall result is claimed; the earlier cold-handshake improvement was about 1.7–1.8x.

R01/R04: four existing-type predicates add no allocation, arithmetic, state owner
or unchecked precondition; original verification/errors stay intact. Existing
implicit-allocation hazards remain. R02/R03: primary alternatives and actual
cost precede this policy; no new literal or learned authority. R05/R06/R07/R13:
address optional encoder work at its existing type seam, three production files,
no manager/cache/polling, provenance comments only. R08/R14: costs and misses above,
raw parent/kernel limits retained. R09/R10: live attributed red, upstream independent
mutations, central green and raw repeatable evidence. R11/R12: one shared TLS
implementation, no platform duplication/root/crypto weakening/installed binary;
other supported targets and hosts unverified. R15/R16: root owns central checks
and adoption, preserves sources/keys/evidence, respects admission and storage floor.
