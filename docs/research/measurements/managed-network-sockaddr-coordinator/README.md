# Socket-address comparison: coordinator records

This is a test-only mechanism comparison, not an adopted network implementation.
The existing transparent UDP responder cannot reserve its original reply tuple
for the eight local-loopback/self-address cases; see the separate
`managed-network-udp-local/COORDINATOR.md` record. No forwarding, grant, accounting
or fail-closed guarantee follows from this comparison.

## Actual caller compilation

Fedora amd64 coordinator r463 compiled the full `managed_network` host test at
`c91d346399eccb2004b2e97b9f27ff9762983ed4`. It failed with exit101: an inline
comment swallowed the comma in a multi-line assertion. The actual full caller,
not an extracted helper, established this failure.

Coordinator r465 retained a second actual compile failure at
`a3296a6655ef3e41d89086702c9e96570bf24689`: `run.status` is `Option<i32>`,
so comparison with integer zero is E0308. After the focused fixes, the full host
caller at `bda23d97aaa4ca59db81bd625990d6af6783ea63` compiled successfully.
Neither compile success is a guest/interception result.

## Actual guest failure before interception

Coordinator r466 ran both connected and unconnected IPv4 cases on real KVM guests
at `f4f29cc3184c5540013db5851e6f5073fcaa5453`. Both test invocations failed with
exit101. Both guests reported Cgroup/NotFound/errno2 before target creation,
clients, hook loading or datagrams. There are no identity, byte, return-tuple or
cleanup success results to infer from those runs. The diagnostic driver's exit0
means that it retained and classified those failures, not compatibility success.

Coordinator r467 instrumented only two operations in the same guest source.
Opening `/proc/thread-self/ns/mnt` succeeded. Creating the exclusive mount
directory failed with errno2, and the guest observed that `/tmp` did not exist.
This establishes the mount-parent failure directly; it is not inferred from
source. The original file was restored byte-for-byte in a `finally` path,
with its final SHA and empty source diff retained. The instrumentation patch is
included. The other cleanup suspicion involving CLONE_FS remains unverified;
it was not changed as part of this fix.

The follow-up source `22286c3cf385742ef31af5097a48bbf022664994` prepares `/tmp`
before the existing exclusive mount-directory creation. Actual replay and all
later setup, socket, tuple and cleanup results are separate owed gates.

## Replay and provenance

Each record retains exact source, command, toolchain, original exit and raw logs.
Guest records additionally retain the exact CLI/guest/kernel identities and
coordinator-supplied effective config. All runs used the experimental kernel
SHA256 `97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`
and config SHA256 `fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d`.
This is not a product kernel pin. Config/build association and actual capability
observations remain distinct.

The actual command is retained in each `command.json`; the strict ignored host
test is `direct_guest_socket_address_udp4_requires_original_native_tuple`.
Caller inputs were boot120000ms, case30000ms, 256MiB and one vCPU, with mode
selected explicitly. These are finite test envelopes, not production constants.
Run in an exclusively owned coordinator checkout with real carried helpers and
the retained `SHARDS_TEST_KERNEL` override; never run against a paused host.

`TRANSFER.json` records the private chunked 16990-byte archive, SHA256
`b0314fae6ea648eb56d849db9702f2af7c5c5ef12321f9bc3d43f5de147599ba`.
`MANIFEST.json` hashes all 47 regular raw files. Archive length/hash, paths,
regular-file status and absence of executable artifacts were checked before
retention. Originals remain on the runner and in the local archive.

Policy: coordinator at origin/main `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
R01–R16 SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
No production source, authority, global host setting or dependency changed.
No topology, cache, polling, quota, performance or portability claim is made.
Root owns the actual tests, review, records and next mutation/compatibility gates;
implementers own only scoped source and light checks. The Windows ARM VM stays
paused. Full route mediation, hard parent/kernel budgets and the managed-runtime
plan remain incomplete.
