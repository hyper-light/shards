# Actual implementer light evidence

No BPF program/load/link, socket, namespace, cgroup, child fixture process, VM,
mutation, audit, benchmark or profile was executed by this lane. All runs below
were admitted on Fedora `8fadc1ec213332e16cb0d915a3fb913f` with unchanged
4CPU/1GiB/2GiB requests, own new source/target directories and rustc1.98.1.
No root clone/cache was accessed. Actual work began from c7e87dc5 with canonical
f5cec365/d634 policy receipt; R01–R16 accounting and runtime obligations are in
[README.md](README.md).

|Kiln job|Actual result|Retained directory under light/|
|---|---|---|
|r120|Compiler101: extraction module layout missing intercept; no tests ran|managed-network-sock-addr-proof-v1|
|r121|Compiler101: missing historical golden include and new source type inference|managed-network-sock-addr-proof-v2|
|r122|23 GNU units0;23 musl units0; GNU clippy101 private-interface/collapsible-if|managed-network-sock-addr-proof-v3|
|r123|23 GNU units0;23 musl units0; GNU/musl clippy -Dwarnings0|managed-network-sock-addr-proof-v4|
|r124|Extracted host compile101: assertion separator swallowed by comment; no tests|managed-network-sock-addr-host-red-v1|
|r125|Extracted host compile101 E0308: Run.status Option<i32> versus integer0; no tests|managed-network-sock-addr-host-red-v2|
|r126|Extracted host compile0, exactly1 record-parser unit0; actual common helpers NOT compiled/run|managed-network-sock-addr-host-green-v1|
|r127|GNU and musl explicit panic/unsafe lint commands101: second mount lacked adjacent SAFETY comment. Wrapper exit0 is NOT a passing lint result; both .status files101|managed-network-sock-addr-proof-v4/*-panic.*|
|r128|Final23 GNU units0;23 musl units0; GNU/musl panic/unsafe lint subset0|managed-network-sock-addr-proof-v5|
|r129 and packaging followup|Archive creation/read-only private evidence packaging only; no functional tests|transfer.json and SHA256SUMS.json|

The source-linked helper includes all actual managed_network modules and the actual
existing devcgroup source. The copy of managed_network.rs is also named mod.rs so
Rust's module lookup uses the real sibling hierarchy. These are byte copies, no
instrumented runtime counters. Every run uses a fresh private target/source tree;
copying does not preserve sync mtimes. Source-hash manifests cover all copied
files; the new sock_addr implementation/ABI snapshots are retained. Unchanged
historical modules and host fixture snapshots remain in the original private
proof directories and signed source history; their exact hashes are in manifests.

The host extraction uses the actual complete fixture source, with an explicitly
inert common module matching Run.status Option<i32> and relevant function types.
It is not the actual common implementation and cannot validate full crate wiring,
VM helper behavior or boot. Its single pure test parses actual tuple/mismatch-shaped
JSON; it is not a routing or native compatibility proof. The coordinator must
compile/run the real full host test independently.

Signed source identities (named-path gits commits, required attribution):

- c91d346399eccb2004b2e97b9f27ff9762983ed4: initial source, preserved unchanged.
- a3296a6655ef3e41d89086702c9e96570bf24689: separator-only compiler fix after r124.
- f35e1d48f7373a57b57af246e7180f5154eb7fcc: Some(0)-only fix after r125.
- bda23d97aaa4ca59db81bd625990d6af6783ea63: one SAFETY comment after r127,
  repeatable drivers and explicit result/limitation record. No runtime change.

Final source-linked test-binary identities (bytes retained remotely, NOT transferred):

|Artifact|SHA256|
|---|---|
|v5 GNU sock_addr_pure-af0ca4e97c808d43|5b703b4b8868efcbea12da36775305771a8e39bd946b5a4cbdab48ab666fa8ff|
|v5 musl sock_addr_pure-a6a37d32ac53925d|7f116ca14f4eecfebc347d0bdcf638ec70d562383071229cba2a81113c1f78ed|
|host green sock_addr_host_pure-54cbe5d1127ece41|e6828dc55f9f0418946ed3759d7401c584f7eff82d3c5804cfb17ed83b5e2ffe|

Raw archive63222 bytes, SHA256
`e9cffdedda6af6817f48510302544b7e6baa7872efa03eeff0ea162117a9c4e5`,
107 regular entries,106 checked member hashes. No executable bytes. Chunked
transfer reassembled exact bytes, verified archive size/SHA/member paths/types and
all manifest hashes before writing this private measurement directory. Full v1
archive and all original proof directories/binaries are preserved remotely;
there was no cache or evidence deletion. Packaging v2 trims unchanged source
snapshots from the transfer, not from the original experiment.

Local policy check and owned rustfmt check0. Mac free remained above30GiB (latest
37051524KiB); formatting used lockf; no local compilation. Full workspace/fault
lints, seven targets, all real guest hooks/metadata/cleanup/error cases and cost
remain coordinator obligations, not inherited passes. The suspected CLONE_FS
cleanup behavior remains source-only, unchanged and unverified pending root live
reproduction; no runtime remedy is in this branch.
