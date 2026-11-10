# UDP loopback/self compatibility fixture

TEST ONLY. Source `b047371dff9e8443409fda99f80ec44694fc7294`, fresh branch
`p/managed-network-udp-local` from combined
`99474d2e94ae8cdfa17cf84340339761031e9d3c`. The coordinator approved the minimal
proposal before edits. No socket, namespace, VM, guard mutation, audit, profile,
benchmark or heavy E2E ran in this implementer lane. Actual reservation outcome
and any remedy remain coordinator decisions after the real compatibility red.

POLICY_READ implementer: FULL current canonical g121-main AGENTS.md, R01–R16,
development-evidence.md, CLAUDE.md, managed plan and refreshed handoff; revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.

## Finite cases and native boundary

Existing `intercept-udp4`/`intercept-udp6` and connected/unconnected modes now
accept `local-loopback` and `local-self`. Reuse the existing three NEW guest
namespaces and finite binary/empty script. The backend and client share the NEW
router namespace for the new cases; the third topology objects remain held.

| Origin | Client namespace/source | Native backend namespace/address |
| --- | --- | --- |
| local | router / existing router-backend address | backend / existing peer address |
| forward | client / existing client address | backend / existing peer address |
| local-loopback | router /127.0.0.1 or::1 | router /127.0.0.1 or::1 |
| local-self | router /192.0.2.1 or2001:db8:1::1 | router /the same address |

Ports are OS-assigned from actual bound sockets. There is one authoritative setup
path with an explicit Origin match; no second manager, socket strategy or generic
topology abstraction. The transparent UNCONNECTED reply reservation is unchanged.
No reuse/reuse-port flag, raw/NAT/socket-handoff/routing workaround is introduced.
Wire, worker, client, deadline, cancellation and strict native receive behavior
remain unchanged. The existing route builder already derives its target from the
actual destination, and no wire/production/kernel/config file changes here.

Immediately before reservation, `UdpSetup` records `ReserveReplyTuple`, the
actual bound backend destination, family/origin/mode and held role-namespace
inodes. It explicitly records baseline, backend_worker and relay_worker as
`NotStarted`. Those objects have not been created at this point. If reservation
returns an error, existing typed `Fault::Io` reporting emits `UdpError` for
ReplySocket at ReserveReplyTuple with its actual kind/errno, then returns nonzero.
The stage identifies the actual scoped namespace/native `udp_socket(Transparent)`
reservation boundary; it does not invent a sub-syscall trace or assume the errno
came from bind rather than another failed setup operation.

A reservation failure is an experimental RED. It is never accepted as
interception success, completed native baseline, joined workers or proven cleanup.
The host checks the selected role facts and explicitly rejects the reservation
error before requiring the existing binary/empty original-tuple/count/join proof.
If reservation unexpectedly succeeds, those full success conditions still apply;
there is no alternate pass predicate for local cases. Baseline-before-failure is
explicitly untested/NotStarted, rather than inferred from the bound endpoint.

The prior kernel/UAPI/Envoy rationale remains in
[the peer UDP record](../managed-network-intercept/UDP.md). This extension supplies
actual compatibility cases for coordinator execution, not an architectural remedy
or a newly reproduced product defect. Root r398 established eight peer UDP and
two TCP real guests at7beb only. Those are transparent responders, not a full
forwarding relay, and they are not passing evidence for these new local cases.

## Source preservation and actual light checks

`source-preservation.json` compares exact source against9947:

- UDP worker/client/completion block unchanged,9510bytes, SHA256
  `2fde378851320beb52ab249f584a8f967210a36b88ee526fd5d01bf4d267ce23`.
- Existing native UDP socket/receive helpers unchanged,13479bytes, SHA256
  `49afcccd92cb9581361d905251ad3e100742a83549c62ecb26e4d603aedfef5b`.
- Entire interception wire source unchanged. Peer native setup order and payload
  operations remain the same; the new setup observation is additional evidence.

Admitted Fedora8fadc1ec213332e16cb0d915a3fb913f4CPU/1GiB/2GiB, installed Rust1.98.1.
Fresh private helpers/targets avoid stale synchronized source mtimes. No full
workspace/C build, root clone/target write or local compiler build.

| Private paths | Actual result |
| --- | --- |
| `~/managed-network-udp-local-{light,target,records}-v1` |14pure tests passed, including unchanged exact8560-byte IPv4 encoding golden; exact updated UDP parser1passed and TCP parser1passed. Strict GNU/musl clippy0. |
| `~/managed-network-udp-local-host-{light,target,records}-v1` | Exact extracted host assertion body:1test passed over16synthetic complete-record combinations,16rejected synthetic reservation errors and80bad-fact controls; strict clippy0. Helper non-test-lib build retains2unused warnings. No full host caller build or actual datagram observation. |
| Local | Scoped rustfmt under lockf, shell/Python parse, diff and policy checks passed. Free37755536KiB before checkout/format, above30GiB; no local rustc/Cargo/C build. |

The synthetic reservation error tests assert rejection specifically through the
native reservation diagnostic; they are not live bind failures. No zero-test
doc-test result is counted as proof. Exact commands/compiler logs/counts/source
identities are retained in `light-raw.tar.gz`:31regular files,11875bytes, SHA256
`7c47d379521ef6b45eed845f087879058f56da2dcb15921f2a1778075389f1d0`.
All30payload lengths/SHA256 values and safe regular paths were verified locally;
see `record.json`. No older source/evidence/binary was replaced.

Frozen pure GNU test executable:
`~/managed-network-udp-local-proof-v1/probe-gnu-tests`,9275688bytes, SHA256
`fee53b57f799e80f12b3fd24974ff57a11faefdcacb9ff77c6c1e760e3a38ef9`.
This is a private compiler artifact, not the real guest or an installed deliverable.

To replay light checks, supply NEW absolute LIGHT_ROOT, CARGO_TARGET_DIR and
RECORDS for each script. The shell driver also requires precreating its NEW target;
the host Python driver creates its NEW target itself:

```sh
bash docs/research/measurements/managed-network-udp-local/run-light.sh
python3 docs/research/measurements/managed-network-udp-local/run-host-light.py
```

The previous peer light driver is preserved as historical source and still names
the earlier parser test. Use this directory's driver for the extended parser.

## Coordinator actual invocation

Use the existing ignored host test directly: the old peer `run.sh` intentionally
retains its peer-only origin allowlist. Supply absolute experimental SHARDS_KERNEL,
SHARDS_NETWORK_EFFECTIVE_CONFIG, explicit CONFIG_PROVENANCE/RECORDS/BOOT_MS/CASE_MS/
MEMORY_MIB/CPUS using their full SHARDS_NETWORK_ names, private CARGO_TARGET_DIR,
and SHARDS_INTERCEPT_MARK/TABLE/RULE_PRIORITY. No timeout/resource default is added.
Root's VM envelope remains4CPU/8GiB/20GiB, separate from the light envelope.

One coordinator-selected case, with all required inputs already exported:

```sh
export SHARDS_NETWORK_SOURCE_REVISION="$(git rev-parse HEAD)"
export SHARDS_INTERCEPT_ORIGIN=local-loopback
export SHARDS_INTERCEPT_UDP_MODE=connected
# Force actual source freshness after synchronization; bytes are unchanged.
touch crates/testguest/src/managed_network.rs crates/testguest/src/managed_network/intercept/udp.rs crates/shards/tests/managed_network.rs
cargo test --locked -j4 --profile test-release -p shards --test managed_network experimental_udp4_interception_has_datagram_and_original_tuple_observations -- --ignored --exact --nocapture --test-threads=1
```

Use the corresponding udp6 test for IPv6 and select local-self/unconnected
explicitly. The host writes actual stdout/stderr/status, command line and supplied
artifact identities under SHARDS_NETWORK_RECORDS. Expected artifact paths remain
`~/managed-kernel-tproxy-5d23-v1/out/{vmlinux,config}-6.18.48-x86_64-managed-tproxy`,
with supplied kernel SHA256
`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae` and config SHA256
`fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d`.
These inputs are supplied association evidence, not independently inferred built
configuration. Record source/status and hashes alongside actual guest results.

Owed root: full actual host/guest compilation; all selected new compatibility
cases with actual native stage/kind/errno; conditional baseline/intercept tuple,
binary/empty and join evidence if setup succeeds; peer/TCP/native-UDP regression;
cleanup/resources, independent relevant guard effects/restoration and cost/matrix.
Only root chooses any mechanism change after actual outcomes. Multi-destination,
UDP absence hard guarantees, fragments/PMTU/QUIC/full forwarding and other route
classes remain required/unverified, never waived by a setup failure.

## R01–R16 account

| Rules | Applicable proof and remaining gap |
| --- | --- |
| R01 | Existing fallible native reservation/error returned; no new unsafe socket logic. GNU/musl checks pass. std/libc/kernel allocation, formatter failure and actual cleanup/fault behavior remain unproven. |
| R02/R03 | Approved source proposal at the existing native seam; prior primary rationale retained. Protocol loopback/documentation addresses and OS ports; no new runtime policy/cap/timeout. Actual compatibility remains owed. |
| R04 | Explicit Origin selection, ReserveReplyTuple and actual NotStarted observations. Native failure remains nonzero; no error/absence-as-success. |
| R05/R06/R07 | One setup path; unchanged worker/client/wire bodies; existing typed fault renderer. No manager/cache/probe or workaround. Comments explain observation and ownership limits. |
| R08/R14 | No performance/profiling or4–5× achievement claim. Root owns allocation/kernel/parent/resource/cost comparisons. |
| R09 | Fixture extension, not a reproduced bind/product fix. Synthetic errors remain labeled synthetic; root actual red precedes any remedy. |
| R10 | Signed source, exact light commands/results/hashes/raw and owed actual gates retained. Root before/after/mutations/restores still required. |
| R11/R12 | Shared v4/v6 native implementation; GNU/musl light only. Full seven-target/real KVM/HVF/WHP gates owed. macOS arm64 only; paused Windows ARM untouched. |
| R13 | Minimal finite role extension and diagnostic; no fallback or reclassification of local requirements. |
| R15/R16 | Implementer source/light only, no children/main merge/audits/heavy/rootcache/host admin. Fresh private dirs, preserved sources/evidence/binaries, admitted envelope, Mac above30GiB. |
