# Native record and body backing: actual Linux acquisition objects

Finite primary-source research for ROOT, 2026-11-11. **Initialized owned pages
can back finite reusable records and parser/reply storage; a completely written
memfd can back an immutable transferred body. Neither object supplies missing
owner authority, hard ancestor protection, or all induced kernel resources.**
These are alternatives and obligations for review, not an architectural selection
or an implemented hard-admission contract.

## Identity and boundary

POLICY_READ: implementer/researcher `/root/record_backing_research`, coordinator
ROOT; base/current `origin/main` `06ae067c0beda71c98a8c6f9633df9feb71fc11b`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read full AGENTS, R01–R16, development-evidence, CLAUDE, managed-runtime-plan,
native-authority decision and `/Users/adalundhe/g121-handoff.md` (SHA-256
`480387c325c2114d05e18d62dde185a3c371b82e7425b9a80c43c3d43cf60180`). Own only
this note. The research skill's background lane is this agent; no children.

The supplied [retention contract][retention] identifies retained whole terminal
Jobs and the missing finite/reusable record and retirement producer. Numeric
issuance burns failed-send IDs; independent body readiness can reorder Prepare.
Consequently, acquiring memory does not justify a maximum-arrival watermark,
forgetting duplicate identity, or replacing history with another unbounded list.
Context ceilings remain DATA, as specified by that contract and the existing
[authority decision](managed-native-authority-decision.md). Observed x86 layout
sizes are not runtime capacity constants. This note investigates OS mechanisms,
not Shards compliance, a reproduced defect, or an implementation completion.

## Five facts that must remain distinct

| Fact | Concrete evidence | What it does not establish |
| --- | --- | --- |
| Authority ceiling | Original checked owner policy and applicable limits. Linux limits may be overcommitted. | Acquired pages or exclusive unused capacity. [Distribution models][models] |
| Owned initialized storage | Successful acquisition plus complete writable population/initialization and retained ownership of the mapping/object. Anonymous read faults can map a shared zero page; write faults allocate private pages. | Future growth, residency or capacity for unrelated kernel objects. [Anonymous fault path][anon] |
| Residency | Successful ordinary `mlock` on the actual accessible region; retain its locked mapping. | More memory authority, immunity to OOM, or future allocations. Unevictable charges can still cause memcg OOM. [Lock acquisition][mlock], [pressure consequences][unevictable-pressure] |
| Hard ancestor reclaim protection | Actual effective `memory.min` throughout the relevant hierarchy, usage and competing protections. `memory.low` is best effort. | Unused pages, exclusive admission or a guarantee from one child readback. Effective protection is usage dependent. [Interface][protection], [calculation][effective] |
| Capacity return | The operation's storage becomes safely reusable, or all relevant object/mapping/pin/queued references are released. | That slot reuse or one local close returned physical pages to Linux. [SCM references][scm], [final file release][fput], [shmem eviction][evict] |

## Concrete procedures and practical tradeoffs

| Candidate object/procedure | What is actually acquired | Limits and suitability |
| --- | --- | --- |
| Private writable anonymous mapping; fully populate writable pages, then initialize the exact record/scratch/reply representation | A retained VMA and instantiated anonymous pages. `MADV_POPULATE_WRITE` explicitly requests writable faults and returns errors; a failed range may already be partly populated. Application initialization remains a separate step. [Population implementation][populate], [GUP flags][populate-flags], [anonymous pages][anon] | Suitable for mutable private storage owned by the existing owner. A successful `mmap`, allocator capacity reservation, or read-only prefault alone is insufficient. Population alone does not pin residency; reusing initialized slots retains the arena's charge. Acquisition can encounter reclaim/OOM rather than a harmless allocation error. [Limits][memory-max], [overcommit][overcommit] |
| Ordinary shmem memfd; size it, `fallocate` its required extent and/or fully write its bytes; map/initialize as required | `memfd_create` creates a zero-length shmem object with `VM_NORESERVE`. Size growth alone sets size/accounting, whereas shmem `fallocate` walks the extent and acquires folios; new folios undergo memcg charging and inode-block accounting. [Creation][memfd-create], [size change][resize], [allocation][fallocate], [charge path][shmem-charge] | A real transferable storage object, with FD/inode/page-cache metadata costs. `fallocate` is conditional on `CONFIG_TMPFS`; failure rolls back newly allocated not-uptodate folios, and successful allocation can deliberately leave folios not uptodate. Fully write/initialize before treating the representation as ready. Pages can swap; allocation is not residency. [Operations/config][shmem-ops], [initialization][shmem-init], [tmpfs semantics][tmpfs] |
| Fully written non-executable memfd, then checked `F_SEAL_WRITE/GROW/SHRINK/SEAL` | Immutable extent/content after successful sealing; Linux rejects adding WRITE sealing across writable shared mappings and checks pins. `MFD_NOEXEC_SEAL` removes execute mode and seals its modification. [Sealing][seals], [creation flags][memfd-create] | Fits sealed bodies and policy data. Seals neither instantiate missing payload pages nor authenticate an issuer. WRITE-sealed storage cannot be reset for the next different body: seals cannot be removed; hole punching is also forbidden. A sealed sparse extent can still have missing pages. [Seals][seals], [hole-punch checks][fallocate] |
| Add ordinary `mlock` to an acquired, accessible mapping | Lock accounting and populated locked pages, subject to `RLIMIT_MEMLOCK` or existing `CAP_IPC_LOCK`; errors must be handled. [Implementation][mlock] | A residency strengthening for actual pages, not a replacement for backing/authority. `MLOCK_ONFAULT` defers population. `MAP_LOCKED` can succeed despite incomplete prefaulting, so neither is an acquisition receipt for all pages. Unlocking alone only makes memory reclaimable. [Deferred locking][unevictable-lock], [MAP_LOCKED distinction][map-locked] |
| Existing HugeTLB pool plus successful reserved mapping without `MAP_NORESERVE`, then initialize | A genuine pool reservation: the kernel checks subpool/global availability and reservation charges; persistent huge pages are unavailable for ordinary uses and cannot swap. [Reservation source][hugetlb-reserve], [pool contract][hugetlb] | Requires an actually provisioned/authorized pool and supported page sizes/configuration; reservation does not initialize pages or fund future COW/kernel metadata. Small regions consume huge-page granularity; freed persistent pages return to that pool. `memory.current` includes HugeTLB only with the documented mount option. No pool, mount, privilege or configuration change is selected here. [Pool lifecycle][hugetlb], [accounting option][hugetlb-account] |

Strict system overcommit mode is another commitment mechanism, not residency:
it constrains global virtual commitments against RAM/swap policy and accounts
private writable mappings. It does not establish Shards owner/member admission,
memcg headroom, initialized page tables or all future kernel allocations.
Changing it would be a guest policy decision outside this lane. [Owning contract][overcommit]

## Established implementations: useful precedents, exact boundaries

- **systemd v258** `memfd_new_and_seal` creates an NX/CLOEXEC memfd, checks the
  complete `pwrite` result, then seals it. This is an actual owned-byte constructor,
  not a size-only constructor. Its compatibility fallback for older kernels is
  not adopted here. Separately, cgroup application writes min/low/high/max;
  its documentation requires ancestor protection and describes shared protection
  competition. These do not reserve a unit's future allocations.
  [Byte construction][systemd-memfd], [controller writes][systemd-cgroup],
  [ancestor semantics][systemd-protection]
- **runc v1.3.3** pins `opencontainers/cgroups v0.0.4`; that implementation writes
  `Memory` to `memory.max` and `MemoryReservation` to `memory.low`. Its existing
  reservation name therefore does not mean acquired storage or hard minimum.
  [Dependency][runc-pin], [actual setter][runc-memory]
- **Linux 6.18 io_uring registered buffers** acquire all requested pages with
  `FOLL_WRITE | FOLL_LONGTERM`, unwind partial pins, then acquire mapping/resource
  metadata and account pinned pages. Release is reference dependent and unpins
  explicitly. This is strong precedent for acquisition-before-publication and
  real cleanup, not a reason to add io_uring merely to fund native records.
  Registration adds kernel costs and does not grant Shards ancestor rights or
  reserve later operations. [Pins][uring-pin], [registration][uring-register],
  [accounting][uring-account], [release][uring-release]
- **Linux emergency mempool** initialization actually allocates its element
  array and guaranteed elements, unwinding on failure. General `mempool_alloc`
  first tries another allocation and can wait when depleted; it is not a hard
  finite population or a latency promise. `mempool_alloc_preallocated` only takes
  existing elements and returns NULL immediately if empty; `mempool_free` refills
  the pool and wakes waiters. These are kernel APIs, not a userspace reserve
  syscall. The transferable principle is owned elements plus explicit exhaustion
  and return, with the count justified by the real admitted requirement.
  [Initialization][pool-init], [allocation paths][pool-alloc], [return][pool-return]

## Smallest real producer seam to evaluate

**Inference for ROOT's next implementation assignment:** deepen the existing
protected native `Preparation::acquire` → `into_owner` handoff, rather than
constructing backing from Context or adding a manager. The inspected local
source retains snapshot/policy objects and constructs the same Owner with
`jobs: Vec::new()`; its `retention` reports representation costs rather than
funding them. Source identity: runtime worktree `managed-native-owner-runtime-v1`,
`checkpoint-v3/source/crates/init/src/managed/native.rs` SHA-256
`72f1a879901f3f00dd4e6779c89c117cbbcbaa23cf8f53c722fe56214a32d5bf`,
lines 97–111, 193–209 and 387–500. The working file matched this retained
snapshot. This is a source observation, not main's implementation or an
exhaustive allocation audit. [Exact source][runtime-source], [checkpoint][checkpoint]

The smallest useful producer output would be the **actual retained storage
objects**, their successfully initialized extents and remaining reusable units,
with original owner/lifetime and actual charging scope. A ceiling may restrict
that acquisition; it cannot create it. Choose no count here: derive any usable
record population from acquired bytes, actual target layout/alignment and every
per-record terminal/control obligation, with checked arithmetic. Parser input,
decoded expansion, reply bytes and simultaneously live sealed-body sources must
each fit actual acquired storage before consumption/publication. A memfd producer
must complete acquisition, writes and required seals before transfer; its
construction overlap is still charged. These are contract requirements inferred
from the mechanisms above, not a proposed new public API or completed producer.

Two independent gaps remain. First, the earlier [parent resource review][parent]
finds no checked hard aggregate entitlement producer; this research cannot supply
one. Second, backing does not solve trustworthy identity reuse/retirement. Preserve
the existing original scope/current Owned checks and uncertain lifetime charges;
do not infer a retirement frontier from numeric IDs or arrival order.

## Honest charges, return and unresolved live facts

Pages are charged where instantiated; process migration does not transfer old
charges. Acquire in the existing authorized charging context and identify it;
assigning a member label cannot move kernel costs. `memory.stat` distinguishes
anon, file (including shmem), kernel/page tables and socket buffers: shmem is not
an additional independent total to add to file. Actual init/root membership,
hierarchy and host/guest backing remain unknown here; non-root memory interfaces
cannot be presumed available for PID1's current scope. [Ownership][ownership],
[statistics][stats], [interface scope][protection]

Returning a slot to a retained initialized arena returns **internal usable
capacity**, retaining OS memory. Unlocking does not free it. For a sealed body,
all mappings, received/queued FD references and any pins must finish their actual
lifetimes before claiming OS release; SCM_RIGHTS takes references and allocates
accounted metadata, and final file release can be deferred. Shmem eviction then
truncates backing. A successful send, terminal reply, EOF or one local close is
not that witness. [Lock lifecycle][unevictable-lock], [mapping references][vma],
[SCM][scm], [release][fput], [eviction][evict]

The arena also does not preacquire later SCM metadata, receiving FD-table space,
new sealed-body inodes, cgroup objects or audit/transport allocations. Ordinary
kernel allocations can reclaim/sleep/fail; accounting depends on actual allocating
paths and flags, not the semantic operation label. [SCM allocation][scm],
[kernel allocation contract][kernel-alloc]

ROOT still needs the exact deployed kernel/config/page sizes, existing charging
placement and effective ancestors; owner-authorized available backing and its
serialization; all parser/body/terminal overlaps and kernel object costs; actual
residency/lock availability; safe record retirement; rollback/Unknown ownership;
and real pressure, cancellation, cleanup/return and neighbor-progress evidence.
Guest residency does not establish exclusive host physical backing for a VM.
No free-RAM sample, optional Context field, `try_reserve`, upstream default,
measured layout or invented quota supplies these facts. No live result was run
or inferred by this lane.

## Reproduction and policy account

Sources were read over official GitHub HTTPS using the GitHub connector, at the
immutable revisions embedded in every link. Tag objects were resolved to commits:
Linux v6.18 `7d0a66e4bb9081d75c82ec4957c50034cb0ea449`, systemd v258
`781d9d0789379d1ea1f2ecefb804d41e9c8b6c38`, runc v1.3.3
`d842d7719497cc3b774fd71620278ac9e17710e0`, cgroups v0.0.4
`a3e2ecd1f756a19cee15f85b96337a59c3b5337b`. These are inspected pins, not
claims of latest releases or the exact deployed Shards kernel. Reproduce any
source read with its linked commit/path and the cited numbered lines. Guessed
shmem/mempool documentation paths returned 404; the owning tmpfs documentation
and implementation files above supplied the actual evidence.

| Rules | This finite batch / remaining boundary |
| --- | --- |
| R01/R03/R04 | Documentation only; no production panic path, literal, state machine, resource amount or new refusal implemented. Acquisition failure/partial ownership remains an implementation obligation. |
| R02/R05/R12 | Pinned owning sources and established alternatives; no architecture selection, speculative manager/cache, controller write or OS adapter. Existing single-owner seam is identified for ROOT review. |
| R06/R07/R13 | One scoped note distinguishes facts, inferences and unknowns. |
| R08/R14 | No profile, benchmark, speed, leak-free or 4–5× claim. ROOT owns actual parent/kernel costs and comparisons. |
| R09/R10 | Source research only, no reproduced runtime defect or fabricated before/after/mutation result. Exact revisions/locators retained. |
| R11 | Linux guest mechanism research; seven supported host targets remain required, macOS arm64 only. No Windows ARM VM or held campaign touched. |
| R15/R16 | Implementer only; no children, builds, actors, audits, remote jobs, cleanup, cache mutation or main merge. ROOT owns selection, integration, live proof and signing if no genuine local agent binding exists. |

Mechanical policy check: loaded exact base revision's
`scripts/check-development-policy` with `git show`, executed its unchanged
`check(root)` against a read-only Git-object path adapter (`read_text` =
`git show REV:PATH`, `is_file` = `git cat-file -e REV:PATH`): PASS, exit 0.
The ordinary script was not run from this sparse filesystem; its referenced
source/matrix files are not materialized. Named-document whitespace/reference
checks and the exact unsigned hash are returned to ROOT. No build/runtime test
was appropriate or run; ROOT's full-checkout gates and evidence review remain owed.

[retention]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/RETENTION-CONTRACT.md
[checkpoint]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-v3/README.md
[runtime-source]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-v3/source/crates/init/src/managed/native.rs
[parent]: https://github.com/hyper-light/shards/blob/af7c5647037bf2572ff01c225c3958a5e81bbbc5/docs/research/managed-parent-resource-contract.md
[models]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L684-L740
[protection]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1299-L1350
[memory-max]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1381-L1396
[effective]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/page_counter.c#L337-L409
[anon]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memory.c#L5134-L5234
[populate]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/madvise.c#L968-L1004
[populate-flags]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/gup.c#L1867-L1915
[mlock]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/mlock.c#L612-L674
[unevictable-pressure]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/mm/unevictable-lru.rst#L104-L118
[unevictable-lock]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/mm/unevictable-lru.rst#L250-L311
[map-locked]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/mm/unevictable-lru.rst#L458-L467
[memfd-create]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memfd.c#L459-L490
[resize]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L1289-L1320
[fallocate]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L3649-L3803
[shmem-charge]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L1876-L1944
[shmem-ops]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L5207-L5219
[shmem-init]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L2568-L2587
[tmpfs]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/filesystems/tmpfs.rst#L14-L108
[seals]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memfd.c#L231-L305
[overcommit]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/mm/overcommit-accounting.rst#L5-L67
[hugetlb-reserve]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/hugetlb.c#L7261-L7359
[hugetlb]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/mm/hugetlbpage.rst#L45-L91
[hugetlb-account]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1779-L1782
[systemd-memfd]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/basic/memfd-util.c#L16-L151
[systemd-cgroup]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/cgroup.c#L1541-L1559
[systemd-protection]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/man/systemd.resource-control.xml#L328-L364
[runc-pin]: https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/go.mod#L17-L18
[runc-memory]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/memory.go#L35-L77
[uring-pin]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/io_uring/memmap.c#L40-L79
[uring-register]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/io_uring/rsrc.c#L778-L862
[uring-account]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/io_uring/rsrc.c#L39-L81
[uring-release]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/io_uring/rsrc.c#L111-L150
[pool-init]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/mempool.c#L215-L249
[pool-alloc]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/mempool.c#L395-L510
[pool-return]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/mempool.c#L562-L596
[ownership]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1938-L1949
[stats]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1523-L1580
[scm]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/net/core/scm.c#L69-L145
[fput]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/file_table.c#L518-L545
[vma]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/vma.c#L2387-L2417
[evict]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/shmem.c#L1349-L1383
[kernel-alloc]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/core-api/memory-allocation.rst#L29-L129
