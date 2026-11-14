# memory-18: unpublished FUSE reply ownership

Assigned by ROOT; found by the Windows P0 lane in D197. Implementer: Codex.
Before edits, merged main 02d3fe33b, including memory-01's b2e3b069a/7f0cb8ce9.
Policy read at that revision: AGENTS f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41,
rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3,
evidence 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63,
CLAUDE 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010.
Role implementer; owned paths: virtio-fs refusal/ownership code and tests, report,
D197, PM and this evidence. No agents or local builds/tests.

## Red and scope

At abecce1a5, resumed Kiln r28 on adas-mac-studio.local, macOS arm64,
rustc 1.99.1:

```sh
cargo test -p shards-vmm leave_the_held_count_unchanged --lib
```

Both tests failed. After eight refused LOOKUPs, held state was (9 nodes,
9 references, 0 handles, 0 cached descriptors, 0 pins), versus (1,1,0,0,0).
After CREATEs it was (9,9,8,8,0). See `red-memory-18.log`.
The tests serve real framed requests through Slot and Server, then exercise the
production EIO replacement extracted unchanged at a738422f5. They inject the
refusal decision; they do not cause a real MAP_FIXED/WHP failure.

r26 failed to compile because the fixture named an absent O_RDWR constant;
`memory-18-fixture-build.log` retains it. abecce1a5 uses the existing tests'
Linux flag value instead. r27 failed in Kiln with a server timeout and no test
result; after waiting, r28 supplied the actual red. The Mac preflight reported
117 GiB free and swap 811/2048 MiB; the lane-owned target began absent, having
been removed after the preceding assignment. One compiler, CARGO_INCREMENTAL=0.

## Decision and derivation

Sources and established behavior:

- [Linux v6.18 FUSE UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/fuse.h):
  in/out headers, entry/open replies, FORGET, RELEASE/RELEASEDIR, direntplus layout.
  Cleanup uses those existing opcodes and exact wire fields, with stack space for
  one in-header plus release body (40 + 8 + 4 + 4 + 8 bytes) and a 16-byte reply.
- [libfuse 3.17.4 low-level contract](https://github.com/libfuse/libfuse/blob/fuse-3.17.4/include/fuse_lowlevel.h):
  entry/create replies add one lookup reference; FORGET reduces the specified
  count. READDIRPLUS acquires references for its returned non-dot entries.
- [Linux v6.18 fuse_create_open](https://github.com/torvalds/linux/blob/v6.18/fs/fuse/dir.c):
  when a successful create reply cannot become an inode, release its handle before
  forgetting the entry. Cleanup also clears the one-shot create/exclusive/truncate
  flags, as that path does.

Chosen: before EIO, retire the reply's new handle through RELEASE/RELEASEDIR and
its nonzero entry IDs through FORGET(1). Reuse the entry walk used for window
reconciliation, without constructing another list. Every previous reference and
handle remains. All cleanup exchanges share the slot's existing connection lock.
Successful filesystem mutations remain completed. A failed/short guest scatter
uses this same replacement, so it cannot leave unpublished server acquisitions.

Alternatives: a new server-only rollback control message would require a peer
protocol change, including external share servers; provisional ownership until a
new acknowledgment would change the normal serving protocol and state machine.
Existing FUSE cleanup works with the current peers. Waiting for unmount retains
the demonstrated growth and cannot satisfy the resource-return contract.

Cold start uses no estimator, retry count or policy threshold. Wire bounds and one
reference per entry are protocol derivations. Unreleased is typed as Transport,
Refused {opcode, errno}, or Malformed. Failure disconnects the slot; remaining
server resources then depend on connection/session teardown, rather than further
requests using a partial cleanup exchange. This is not a claim of remote cleanup
when the peer is unreachable. Cleanup waits for the existing framed reply/readiness
or connection end; no periodic probe, cache or new deadline was added.

## Validation and limits

Fix c37547114, shared allocation-free entry walk eddbef457, failure tests fbabcfe6b.
New cases also cover accepted handles/pins surviving refused OPENs, repeated
refusal of an error reply, READDIRPLUS and directory handles, and cleanup transport,
malformed and errno failures. Green results are recorded below after main merge.

R01/R04: new byte fields use checked ranges/conversions, poisoned locks recover,
cleanup uses bounded stack frames and typed failure. Existing success entry IDs
are retired exactly once before the original success is replaced. The shared
entry walker also serves the original reconciliation caller. R02/R03/R05/R06/
R07/R12/R13: sources/alternatives/bounds above; existing protocol/owner/platform
seams, no new runtime policy value, peer protocol or manager. R09/R10: retained
actual count-growth red; the compiler and infrastructure failures are distinct.
R08/R11/R14/R15: no performance/matrix/real-VM claim. R16: one owned remote target,
removed after the piece; source and evidence preserved. ROOT owns the policy
checker under the brief's restricted implementer command scope.

Owed to ROOT, not run:

```sh
cargo test -p shards --test saved a_saved_microvm_reads_on_through_its_dax_window
cargo test -p shards --test boot a_direct_boot_writes_through_a_share_and_not_through_a_read_only_one
python3 scripts/check-development-policy
```

Also owed: actual repeated window failures during LOOKUP/CREATE on HVF/KVM/WHP,
external-share peer compatibility/failure, cleanup under real pressure/teardown,
allocation/fault/RPC cost and the supported target matrix. No ratio is claimed.
