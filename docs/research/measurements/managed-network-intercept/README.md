# P4 isolated IPv4 TCP interception fixture

**Initial experiment record; no product mechanism adoption.**
The selected-flow fallback DROP experiment is recorded in [ABSENT-DROP.md](ABSENT-DROP.md);
its actual guarded guest outcome and any acceptance change remain coordinator-owned.
The fixture-only missing-listener mode and current policy/light-check checkpoint
are recorded in [RELAY-ABSENT.md](RELAY-ABSENT.md); its actual guest outcome is
still owed to the coordinator. The records below describe earlier source.
The current two same-namespace target additions and their distinct unrun gates are
recorded in [LOCAL-TARGETS.md](LOCAL-TARGETS.md). Prior root peer-case results do
not establish those new cases.
Root owns every real guest/runtime/mutation/resource/performance run. This lane
implemented scoped fixture code and ran only pure unit/compiler/lint checks.
Base `4d3f454f194a0b6927423f0f859071147c43786d`; policy canonical g121-i7
`c921a0b12b06d495b354907162e991376edffd41`, rules SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Role implementer; full AGENTS/R01–R16/evidence/CLAUDE/plan/handoff readback sent
before edits. Coordinator approved the exact private fixture boundary first.

## Boundary and proposed observation

`managed-network intercept-tcp4 {local|forward} DEADLINE_MS MARK TABLE RULE_PRIORITY`
creates three NEW guest network namespaces: router/transparent listener, backend,
and client. PID1 is required, and each new namespace is unshared on a dedicated
owned thread before any configuration. No host/current namespace is administered.
Held namespace FDs and route sockets retain actual objects, and namespace inode
identities are emitted as JSON. No named namespaces, bind mounts, shell, ip, nft,
socat, extra installed helper, product route update or kernel-pin change occurs.
The existing argv diagnostics remain untouched.

RFC5737 documentation addresses are semantic fixture IDs: router/backend
192.0.2.1/192.0.2.2, router/client 198.51.100.1/198.51.100.2, each link /30.
Interface IDs come from actual if_nametoindex lookups; both listener ports are
assigned by the OS. MARK/TABLE/RULE_PRIORITY are explicit decimal 32-bit inputs.
Zero mark and reserved tables 0/253/254/255 refuse. Priorities must follow the
kernel's priority 0 local rule and precede its 32766 main rule. A rule/table creates
with EXCL; it does not silently replace an object. These are protocol/topology
facts and experiment inputs, not production policy caps or learned authority.

Per local/forward case there are exactly two client calls:

1. Baseline, before installation: destination is the real backend's assigned
   IPv4 address/port; request hex5245515545535400ff must receive actual backend
   reply hex4241434b454e4400ff.
2. Install a narrowed IPv4 destination+TCP-port selector. A local-origin case
   uses an OUTPUT **route** chain to mark, a fwmark policy rule and local /32 route
   via loopback, then PREROUTING TPROXY to the transparent listener's assigned
   port plus mark. Forward-origin traffic arrives through PREROUTING directly.
   The client must receive distinct relay reply hex52454c41590080. The accepted
   relay socket's getsockname and peer tuple must equal the original destination
   and actual client local tuple. Backend total accepts must remain 1; relay 1.

ClientObservation and ServerObservation JSON report actual reply bytes/roles and
socket tuples. Namespaces, Baseline, Installed, Joined and Intercepted records
name source state separately. The ignored host test checks both server receipts,
client bytes, namespace identities, tuple equality and final acceptance counts;
it does not infer interception merely from an alert. Meaningful coordinator mutation candidates are removal of OUTPUT marking in the
local case, and removal of the whole PREROUTING interception+mark rule in the
forward case: these are expected to expose actual BACKEND bytes/second backend
accept and no relay accept. This is an inference, not a runtime result. Removing
only TPROXY while retaining mark/policy-local routing may instead refuse traffic;
that alone is not proof of forbidden direct routing. All mutations are **unrun**.

This is the selftest's decisive responder distinction, not a forwarding relay:
the transparent responder does not establish an outbound backend connection.
All roles are trusted testguest threads sharing a process FD table, not isolated
untrusted principals. No established backend connection is passed to a client;
its own native connection is created after entering its origin namespace. This
fixture proves no FD-transfer containment, grants policy or full mediation.

## Source/alternatives and small implementation seam

Pinned downloaded-byte identities are retained in [sources.json](sources.json).

- [Linux v6.18 nft_tproxy_tcp.sh](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh)
  supplies the local/forward topology and responder distinction, and explicitly
  marks locally originated traffic in OUTPUT's route chain before policy-local
  routing and PREROUTING. We replace its subprocesses/polling with native sockets,
  event-driven waits and owned threads, not its arbitrary timeout/port choices.
- [Linux v6.18 TPROXY documentation](https://github.com/torvalds/linux/blob/v6.18/Documentation/networking/tproxy.rst)
  requires transparent listeners and policy routing and explains NAT/REDIRECT's
  destination rewriting and original-destination limitations. A direct native
  socket pair is a useful baseline, but is not evidence of interception. This
  fixture enables the next experiment; it does not choose the final mechanism.
- [Linux v6.18 nft_tproxy.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_tproxy.c)
  constrains the hook to PREROUTING and transports to TCP/UDP, parses family/port
  register attributes, and only assigns a transparent socket. The IPv4 slice
  intentionally makes no IPv6/UDP/Unix/SCM_RIGHTS/QUIC promise.
- Linux v6.18 UAPI netlink/rtnetlink/fib_rules/if_link/veth and
  netfilter nf_tables/nfnetlink headers define every encoded attribute, field
  width, byte order, alignment, register, hook and command. IPv4 destination
  offset 16 and TCP destination-port offset 2 are their packet header layouts.
  [RFC5737](https://www.rfc-editor.org/rfc/rfc5737) supplies test-only address ranges.
- Existing `crates/init/src/links.rs` provides the source-linked wire conventions.
  It is private in a binary-only init crate, coupled to netplan/run/setup/chooser/
  agentdns. Direct inclusion would pull production policy and fake modules;
  extracting a shared production API only for this experiment would expand scope.
  Root explicitly approved fixture-private fixed-message builders instead. There
  is no new generic framework, public API, production cache or route manager.

The existing managed_network Deadline, transparent bind, exact TCP payload and
half-close helpers are reused. Each worker is explicitly Running or Joined.
Drop signals its owned eventfd and joins on every failure path. Socket polling
uses cancellation/readiness plus the one original caller deadline; no periodic
/proc/readiness probe is added. Even if wake fails, socket waits and connect_timeout
retain that absolute deadline. Namespace setup threads are joined synchronously;
all sockets, listeners, namespaces and workers use owned RAII values, so partial
setup unwinds them. No detached reader/helper remains. Actual kernel destruction,
RCU/socket close latency and resource return are unmeasured and must be observed
by root; a user-space deadline does not prove a bound on kernel syscall scheduling.

Wire buffers use fallible reservation and checked lengths/ABI conversions. ACK
parsing checks header/body size, sequence identity/duplicates, errno and alignment;
learned MSG_PEEK|MSG_TRUNC datagram length avoids a copied receive-cap constant.
Synthetic constant inputs and finite role/message counts bound this experiment.
No unsafe memory indexing or explicit production assertion/panic is added. The
inherited std thread/scope, io::Error, formatting and I/O library internals may
allocate; allocation-failure/fault behavior is not proven. This fixture is not
approved for a production admission/allocator seam or an exhaustion claim.

## Actual light results and exact owed coordinator commands

Fedora runner 8fadc1ec213332e16cb0d915a3fb913f; exact 4 CPU/8GiB/20GiB admitted.
Private clone `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-intercept-95a6f594e0`,
private target `~/managed-network-intercept-target`, source cache was a read-only
reflink of this lane's inactive previous guest target; no other lane cache write.
Both v1 and final-guest v2 passed **3/3 pure tests**, strict GNU clippy exit 0 and strict
musl clippy exit 0. No namespaces, native socket runtime, guest, mutation or profile
was executed by this agent. v1 precedes explicit WorkerState/server tuple records;
v2 compiles/tests those final guest bytes. The host test was subsequently renamed
experimental_* so the existing direct_guest_* native 13-boot campaign cannot pick
it up accidentally; its body has not been compiled or run here.

[light-raw.tar.gz](light-raw.tar.gz), SHA256
3b853206ee0afccf1a8e7d91b08cbaf82e16b62d237daf33a64e9bbaefc6c172,
contains 25 raw command/log/exit/source/toolchain/binary records, verified against
its internal manifest. Expanded copy is at
`/Users/adalundhe/g121-managed-network-intercept-light`; frozen unit binary and
SHA record are on Fedora `~/managed-network-intercept-proof/`. Cargo clean of the
owned testguest package and touch of owned source make source freshness explicit;
compiler logs show actual recompilation. No timing counters or measurements.
Local rustfmt, shell syntax, diff whitespace and development-policy checks pass.
Mac free space stayed above 30 GiB; no local compiler or data deletion occurred.

Reproduce light checks with [run-light.sh](run-light.sh), private target/records,
GNU+musl toolchain and scripts/zig-cc setup recorded in the logs. **Still owed:**
host test compilation/strict clippy and its pure record-parser test, arm64 guest
compilation and final supported-target gates; all real VM/runtime/failure cleanup,
missing-feature results, independent mutation/restoration and cost evidence.
Root source/target for its active campaign remain off limits to this lane.

Root may execute [run.sh](run.sh) once per explicit local/forward case using the
existing experimental kernel/config and its reviewed budgets. It intentionally
has no budget/mark/table/kernel defaults. Retain the exact source, binary,
config/provenance and actual record outcomes. Inside an admitted root-owned
4CPU/8GiB/20GiB campaign, after supplying the existing SHARDS_NETWORK_* inputs:

```sh
export CARGO_TARGET_DIR="$HOME/managed-capture-target"
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_EFFECTIVE_CONFIG="$HOME/managed-kernel-tproxy-5d23-v1/out/config-6.18.48-x86_64-managed-tproxy"
# Also supply provenance, absolute new records directory, boot/case milliseconds,
# VM memory/CPUs, and origin/mark/table/priority as reviewed coordinator inputs.
bash docs/research/measurements/managed-network-intercept/run.sh
```

The precise test filter is
`experimental_tcp4_interception_has_distinct_backend_and_relay_observations`.
A passing run must actually execute 1 test and produce the guest's observation files;
a zero-test cargo success is not a result. Host inputs/config symbols alone are
not actual built-kernel capability or installed-interception evidence. The shared
boot helper's pre-execution identity retains its existing unverified status;
returned guest records and decisive actual bytes are separate runtime evidence.

## R01–R16 evidence and remaining gates

| Rule | Scoped record |
| --- | --- |
| R01 | Checked wire buffers/math/slices and owned FDs/joins; GNU/musl strict clippy exit 0. Std/internal allocator, failure and kernel-close limits above, not proven exhaustion-safe. |
| R02 | Pinned selftest/kernel/docs/UAPI plus existing Shards seam; alternatives above. Actual mechanism proof/adoption waits for root runtime data. |
| R03 | Protocol field/alignment/header invariants and semantic RFC5737 fixture IDs; deadline/mark/table/priority explicit, ports/interfaces from OS; no product cap/default copied. |
| R04 | Explicit Origin/Reply/Phase/WorkerState, typed I/O error kinds/errno, JSON actual identity/tuple/count outcomes. Root runtime transitions owed. |
| R05 | Existing socket/deadline helpers reused; private encoder only because binary coupling, no production API/cache/manager, no periodic waits. |
| R06 | Fixed local/forward cases, bounded known role/message set and single original deadline; no open-ended topology controller. |
| R07 | Source and safety/namespace/lifetime comments, derivations and distinctions above. |
| R08 | No competitor/performance result;4–5x target neither measured nor achieved here. |
| R09 | New experimental fixture, no inferred product defect/fix or claimed live interception; root owns exact future repro/green. |
| R10 | Signed source/driver, pinned hashes,25 raw light records; actual runtime red/green and guard-removal evidence explicitly outstanding. |
| R11 | x86 GNU 3/3 and GNU/musl compiler/lint evidence only; arm64, other supported host gates and real KVM/HVF/WHP remain unverified. macOS arm64 only. |
| R12 | No production architecture change/extra installed binary/host root/crypto change. Private test-only wire necessity approved; guest setup always NEW namespaces. |
| R13 | TCP IPv4 selftest-equivalent responder proof only; no forwarding/grants/FD isolation/IPv6/UDP completeness claim. |
| R14 | No allocation/CPU/page-fault/retention/cost profile or resource-return claim; coordinator owns those campaigns. |
| R15 | Implementer readback and approved exact boundary; scoped light results, no nested agents/main merge. Root reviews/composes/executes all heavy and final gates. |
| R16 | Mac floor respected, no deletion; private remote target/clone, inactive read-only cache source, exact envelope, other lanes/root paths untouched. |
