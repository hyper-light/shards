# G01 supplied-member capture fixture

Role: implementer `/root/managed_capture`; coordinator `/root` owns the live run,
audits, profiling, benchmarking and heavy adversarial validation. This first
commit is a test fixture, with **no production capture fix**. Base:
`6bf5b15da904931b984ecd25662a8b8c571268bf`.

Policy read: `docs/development-rules.md`, SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Owned paths: testguest's `workload.rs`, shards' `tests/build.rs`, this directory.

## Repeatable live example

On an available real KVM, Hypervisor.framework or WHP host, from the fixture commit:

```sh
cargo test -p shards --profile test-release --test build \
  supplied_member_output_does_not_rewrite_the_main_protocol -- --exact --nocapture
```

The test builds a supplied native AGENT directory and HARNESS tar archive, each
with the two-field root manifest. Neither uses RUN, VOLUME, a shell wrapper, extra
network authority or an installed sensor. Both execute testguest's
`managed-capture MEMBER` mode. It writes these bytes independently:

| FD | Bytes, where MEMBER is `agent` or `harness` |
| --- | --- |
| 1 | `MEMBER\0stdout\n\xff\x1b[2Jout-tail` |
| 2 | `MEMBER\0stderr\n\x80\xfeerr-tail` |

After both writes have completed, each process names itself `capture-ready` and
waits for normal run teardown. The host observes both names through the existing
trusted process inventory, then signals the main workload's existing SIGUSR1
handshake. The main workload writes exactly
`{"jsonrpc":"2.0","id":1,"result":"main"}` to stdout and `main-stderr` to stderr,
with no added newline, then exits.

Expected: only those two main-workload strings reach its stdout/stderr. The current
source merges member fd1/fd2 and prefixes/rewrites them onto main stderr; the live
run must demonstrate that actual contamination before a production fix is made.
This record does not claim that reading the source reproduced it.

The existing test runner decodes command output lossily. Its equality check proves
the main protocol's uncontaminated bytes because the expected strings are valid
UTF-8, but does **not** establish a binary canonical member round trip. Raw member
data/channel/offset/EOF assertions need the reviewed managed sink and transport.
Independent clean member exit is also a follow-on case: this fixture deliberately
keeps the members alive until owned teardown to avoid readiness races.

## Policy and evidence status

| Rules | Scoped evidence / remaining requirement |
| --- | --- |
| R01 | Fixture I/O errors return nonzero; checked `prctl`; no added unwrap/assert in guest mode. Host test assertions are test-only. Production remains unchanged. |
| R02, R03 | The test exercises the approved G01 contract and existing readiness mechanism, not a new architecture. Payloads and labels are test data; count two comes from AGENT plus HARNESS. Production wire/credit decisions remain held. |
| R04 | Fixture sequence is write/flush both channels, publish readiness, wait for teardown; main waits for host readiness, writes its protocol, exits. Managed stream lifecycle is not implemented here. |
| R05, R06, R07, R13 | Reuses existing native testguest dispatch and supplied artifact test area; no new framework or helper process. Comments identify syscall safety only. |
| R08, R14 | No performance, allocation or fault claim. Coordinator comparisons/profiling remain owed. |
| R09, R10 | Exact live command and expected symptom above. Coordinator must retain baseline-red log, host/toolchain/kernel identity, fixed-green, mutations and cleanup observations. None is claimed yet. |
| R11, R12 | Shared Linux guest code and host-independent test; supported seven targets exclude Intel Mac. Actual KVM/HVF/WHP runs, unavailable hardware and seven-target checks remain coordinator-owned/unverified. No isolation, crypto or grants changed. |
| R15 | Implementer returns fixture commit and light checks only; no heavy E2E, audit, subagent or main/integration merge. |
| R16 | No cache deletion; no build started below the Mac's 30 GiB floor. No user data or existing evidence changed. |

The coordinator records actual commands, revisions, result/log paths and outcomes
here or in the linked managed-runtime evidence record after its live run. No gap
is closed by this fixture alone.
