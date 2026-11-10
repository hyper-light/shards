# Explicit TCP/UDP interception seam — R05 follow-up

Source `24dfe5d7b265d1af6d16c8b4017b497ddd75d233`, fresh branch
`p/managed-network-protocol-seam` from `9df26e1f141379fc6207aa0500b911bb06613fc7`.
Prior source/evidence `30eaaee9c`/`9df26e1f1`/`72a9bc8c4` and all private binaries
and logs remain unchanged. This is the coordinator-requested new-component R05
conformity correction, not a reproduced functional bug or product adoption.

POLICY_READ implementer: FULL current canonical g121-main AGENTS.md,
development-rules.md R01–R16, development-evidence.md, CLAUDE.md, managed plan and
current handoff. Revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.

## Exact change and ownership

Only three fixture source paths change: `intercept.rs` TCP call,
`intercept/udp.rs` UDP call, and `intercept/wire.rs` seam/pure tests. One
`Netlink::interception(input, destination, port, Protocol, deadline)` serves both
real callers. One message encoder and one selector take Protocol explicitly.
Remove `interception_udp`, the default-TCP encoder wrapper and the test-only
selector wrapper; tests/golden calls explicitly pass Tcp. No new generic engine,
state, wait, cache, default, timeout, allocation/reservation, guard or payload path.
The source diff is32insertions/40deletions, including formatting/signature/test
call-site changes. Existing message construction and reservations remain intact.

The existing pinned Linux/RFC/Envoy rationale and all limits are retained in the
[previous UDP record at72a](https://github.com/hyper-light/shards/blob/72a9bc8c42a0b1c6426163ac2cae1bb764bc11ab/docs/research/measurements/managed-network-intercept/UDP.md).
This change adds no architectural selection or external semantic assumption.

## Actual focused checks

One admitted4CPU/1GiB/2GiB run on Fedora8fadc1ec213332e16cb0d915a3fb913f:

```sh
export LIGHT_ROOT="$HOME/managed-network-protocol-seam-light-v1"
export CARGO_TARGET_DIR="$HOME/managed-network-protocol-seam-target-v1"
export RECORDS="$HOME/managed-network-protocol-seam-records-v1"
mkdir "$CARGO_TARGET_DIR"
bash docs/research/measurements/managed-network-intercept/run-udp-light.sh
```

Installed Rust1.98.1 and pinned libc0.2.189. The unchanged driver copies exact
source into a NEW private helper and builds a NEW target, so old mtimes cannot
select previously compiled source. No workspace/C build or root clone/cache write.

- GNU filtered pure tests:14passed, including UDP selectors and existing IPv4
  encoding golden. The golden is the exact8560-byte selector/route/nft record,
  not a guest networking observation.
- Exact UDP parser:1passed; exact existing TCP-family parser:1passed.
- GNU and musl strict clippy:both0. Musl is a compile/lint result, not execution.
- Local scoped rustfmt under lockf, policy checker and diff check passed. Mac
  free38242200KiB before the fresh checkout/formatting; no local compiler/Cargo/C
  build and no source/evidence cleanup.

Raw `protocol-seam-light-raw.tar.gz`:3245bytes,19 regular files, SHA256
`9f17d021bb7523ad8dbae911570f5b11d23ce78a3f60d1506aafa6071829cbe3`.
Its bytes/SHA and regular single-component member paths were verified locally;
`protocol-seam-record.json` retains every raw file and source SHA256. The raw
identity names pre-commit9df plus exact changed-source hashes; those correspond
to24d. Commands, compiler logs and actual test counts/exit codes are preserved.

Frozen GNU pure-test binary:
`~/managed-network-protocol-seam-proof-v1/probe-gnu-tests`,9275760bytes, SHA256
`3436fe2ea1fcd32f71fe4fb25ea3e4cab661a91e4de87238861bbe79d09621f4`.
This is NOT a real guest artifact or a runtime/cost pass.

## R01–R16 and owed coordinator runs

| Rules | Applicable result / remaining gap |
| --- | --- |
| R01 | Removed wrappers; no new unsafe/allocation/arithmetic/error path. GNU/musl lint passed. Existing implicit allocation/fault/panic limitations remain unproven. |
| R02/R03 | Existing primary semantic rationale unchanged; concrete real Tcp/Udp callers justify explicit Protocol. No new production/measurement-derived constants. |
| R04 | Existing explicit UDP Exchange and TCP lifecycle/completion unchanged. No new state or inferred success. |
| R05/R06/R07 | One existing Netlink/encoder/selector seam; unnecessary pass-throughs removed. No cache, probe or framework. Necessary safety/source comments retained. |
| R08/R14 | No profiling, benchmark, allocation/resource or4–5× claim; root owns those gates. |
| R09 | Source-conformance correction, not a claimed live functional issue. No sockets/VMs/mutations ran in this lane. |
| R10 | Signed named source commit, raw commands/logs/counts/source/binary identities and exact golden retained; actual before/after effects remain root work. |
| R11/R12 | Shared v4/v6 and Tcp/Udp implementation; GNU/musl light checks only. Other supported targets and real KVM/HVF/WHP unrun. macOS arm64 only; paused Windows ARM untouched. |
| R13 | Smaller concrete fixture seam, no broader engine or waived route requirement. |
| R15/R16 | Implementer scoped source/light only, no children/main merge/rootcache/interruptions. Fresh private target, preserved artifacts, admitted envelope, Mac above30GiB. |

Coordinator retains full compilation, actual8peer-UDP cases, TCP/native-UDP
regressions, guard removal/restoration, tuple/bytes/cancellation/cleanup evidence,
resources/cost and combined seven-target/VM gates. The existing coordinator
`run.sh` invocation is unchanged by this follow-up. UDP loopback/self,
multi-destination, fragmentation/PMTU/QUIC/full forwarding and UDP absence hard
guarantees remain explicitly owed; none is reclassified as optional or passed.
