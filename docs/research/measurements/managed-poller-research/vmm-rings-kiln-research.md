**Research:** Maintain the Shards constants/derivation lane through managed-runtime completion, following canonical R01–R16 and the coordinator's approved queue: finish Poller/inotify primary-source checkpoints, continue TLS Handshake length/admission, QUIC datagram/credit bounds, and the four named unmeasured device rings. Send small signed checkpoints and immediately continue the next approved independent item; do not end the lane or mark this goal met when one report is ready. Research only assigned literals/alternatives until ROOT supplies measured/reproduced implementation authorization; all audits/profiling/heavy real E2E remain ROOT-owned. Done only when the coordinator confirms the lane's managed-runtime obligations accepted on main with actual evidence and releases the lane; otherwise propose the next precise test/derivation within these named seams while ROOT validates. Preserve current policy/readbacks, headroom, artifacts and paused Windows ARM; no fake keepalive, production guesses, child agents or self-main merges.

19 findings (A 2, B 15, C 2) from 37 sources (31 internal, 6 external)

- **B · 70 (good)** Shards decisions require current full R01–R16 readback, derived rather than arbitrary runtime values, and preserved authority. Coordinator owns audits/profiling/heavy campaigns/main merges; this lane returns scoped signed research and only explicitly authorized light implementation. [1]
  _1 independent source (a document); 3 of 3 quotes found where cited_
  - [1] "Before edits/builds/tests/delegation, read this whole file" (Start, resume and delegation) · quote found
  - [1] "Learning never enlarges authority." (R03) · quote found
  - [1] "These are not delegated campaigns." (R15) · quote found
- **A · 99 (high)** Poller::wait accepts caller-owned token Vec storage and clears its length; its native output array has existing batch64. Actual caller lifetimes differ: daemon follow_all keeps its Vec across waits, while cleanup Client::wait_change creates a new Vec per invocation. The source does not establish a universal per-wait allocation count or cost. [3][7][17][18]
  _2 independent sources (code); 5 of 5 quotes found where cited_
  - [3] "ready.clear();" (2044) · quote found
  - [3] "const AT_ONCE: usize = 64;" (2045) · quote found
  - [7] "self.len = 0;" (Vec::clear) · quote found
  - [17] "let mut events = Vec::new();" (Client::wait_change before self.poller.wait) · quote found
  - [18] "let mut ready = Vec::new();" (follow_all before its while loop) · quote found
- **B · 79 (good)** Linux and XNU accept supplied event-output lengths;64 is not a protocol registration bound. XNU consumes ordinary exit oneshots, so a candidate that allocates token output only after dequeue needs retention or an explicit failure contract. [5][6]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [5] "#define EP_MAX_EVENTS (INT_MAX / sizeof(struct epoll_event))" (EP_MAX_EVENTS / ep_check_params) · quote found
  - [6] "kn->kn_flags |= (EV_EOF | EV_ONESHOT);" (filt_procevent, around1222) · quote found
  - [6] "knote_drop(kq, kn, &knlc);" (knote_process, around4454) · quote found
- **B · 78 (good)** Memory::relieve's existing4096 array is userspace collection storage. Its single file-watch framing is conditional on unnamed records; Linux rejects a too-small next record before dequeue, and a short positive prefix is not queue-exhaustion proof. [4][8][9][10]
  _2 independent sources (a document, code); 4 of 4 quotes found where cited; inferred from them_
  - [4] "No replacement number or architecture is selected." (opening scope statement) · quote found
  - [8] "length (including nulls) of name" (struct inotify_event.len) · quote found
  - [9] "return ERR_PTR(-EINVAL);" (get_one_event) · quote found
  - [10] "name = NULL;" (fsnotify_handle_event, non-directory-entry inode event) · quote found
- **B · 79 (good)** Rust reservation semantics distinguish length, allocated capacity and a candidate's logical syscall service count. try_reserve_exact requires additional relative to length and permits excess; a reused larger native capacity does not authorize a larger output batch than separately prepared token storage. [7][15]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [7] "self.len() + additional" (Vec::try_reserve_exact documentation) · quote found
  - [7] "Note that the allocator may give the collection more space than it" (Vec::try_reserve_exact documentation) · quote found
  - [15] "events.capacity() as i32," (Selector::select) · quote found
- **A · 99 (high)** Established libraries demonstrate different storage/demand methods without supplying Shards runtime constants: libevent retains native arrays and grows on saturation; mio uses caller-owned native arrays. Their defaults, ET contracts and failure policies require comparison against Shards' actual callers. [14][15]
  _2 independent sources (code); 3 of 3 quotes found where cited_
  - [14] "#define INITIAL_NEVENT 32" (129) · quote found
  - [14] "#define MAX_NEVENT 4096" (130) · quote found
  - [15] "pub type Events = Vec<Event>;" (139) · quote found
- **B · 72 (good)** TLS1.3 Handshake encoding carries a uint24 message length, distinct from TLSPlaintext record limits. This wire representation alone does not derive admitted reassembly memory, cold-start behavior or a permitted parser allocation. [11]
  _1 independent source (a standard); 2 of 2 quotes found where cited; inferred from them_
  - [11] "uint24 length;" (section4 Handshake) · quote found
  - [11] "The length MUST NOT exceed 2^14" (section5.1 TLSPlaintext.length) · quote found
- **B · 72 (good)** QUIC UDP/path size bounds, reliable-stream flow credits and RFC9221 DATAGRAM signaling are different constraints. RFC9221 DATAGRAM data does not consume stream/connection flow limits, while sending still uses congestion control and peer/path frame-size restrictions; no copied datagram or credit default is justified. [12][13]
  _1 independent source (a standard); 4 of 4 quotes found where cited; inferred from them_
  - [12] "max_udp_payload_size" (sections14 and18.2) · quote found
  - [12] "1200 bytes" (section14.1 Initial Datagram Size) · quote found
  - [13] "DATAGRAM frames do not provide any explicit flow control signaling" (section5.3) · quote found
  - [13] "DATAGRAM frames employ the QUIC connection's congestion controller." (section5.4) · quote found
- **C · 65 (moderate)** The explicit sustained-lane goal includes TLS Handshake length/admission, QUIC datagram/credit bounds and four coordinator-named unmeasured rings. A completed report is a checkpoint; lane closure requires coordinator acceptance on main with actual evidence and explicit release. [2]
  _1 independent source (kiln's records); 2 of 2 quotes found where cited_
  - [2] "Maintain the Shards constants/derivation lane through managed-runtime completion" (goal body) · quote found
  - [2] "accepted on main with actual evidence and releases the lane" (goal closure condition) · quote found
- **B · 80 (good)** Durable source identities and bounded signed checkpoints exist, while all live fault, fairness, allocation, comparative performance and portability plans in this lane remain unrun and coordinator-owned. Planning readiness must not be described as runtime completion. [16][1]
  _1 independent source (data, a document); 2 of 2 quotes found where cited_
  - [16] "Two scoped literals: Unix Poller capacity and guest Memory::relieve inotify read storage; no runtime measurement" (scope) · quote found
  - [1] "They do not merge themselves to main." (R15) · quote found
- **B · 80 (good)** The TLS uint24 Handshake envelope bounds one encoded message; it does not itself establish aggregate managed QUIC connection memory. Ordered TLS buffering and unordered QUIC buffering have separate ownership. [11][19][20]
  _3 independent sources (a standard, code); 2 of 3 quotes found where cited; inferred from them_
  - [11] "uint24 length;" (§4) · not checked
  - [19] "TLS is responsible for buffering handshake bytes" (§4.1.3) · quote found
  - [20] "const HANDSHAKE_BYTES: usize = (1 << 24) - 1 + 4;" (lines27–28) · quote found
- **B · 72 (good)** The pinned QUIC CRYPTO reach check and TLS message-body limit are distinct from one total allocation bound. The TLS QUIC path appends ordered bytes before processing; separate undecryptable/packet-backing storage and concurrency require root-only reachable-state accounting and live validation. [21][22][23]
  _1 independent source (code); 3 of 3 quotes found where cited; inferred from them_
  - [21] "if max > self.config.crypto_buffer_size as u64" (read_crypto2948–2950) · quote found
  - [22] "const MAX_HANDSHAKE_SIZE: usize = 0xffff;" (line396) · quote found
  - [23] "let range = self.deframer_buffer.extend(plaintext)?;" (read_hs393) · quote found
- **B · 79 (good)** Managed DATAGRAM_BYTES is UDP scratch/output allowance, not an unreliable QUIC DATAGRAM-frame budget: the managed transport disables that feature. Pinned endpoint receive advertisement uses its own default1472 while driver scratch uses65527; neither copied default establishes a selected storage or service method. [25][26][27]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [25] "pub(crate) const DATAGRAM: usize = 65_527;" (line24) · quote found
  - [26] ".datagram_receive_buffer_size(None)" (transport528) · quote found
  - [27] "max_udp_payload_size: (1500u32 - 28).into()" (EndpointConfig::new62) · quote found
- **B · 80 (good)** QUIC peer flow control, local send storage, application Consumed acknowledgments and caller-held guest/daemon reservation are different constraints. Pinned write_limit uses remaining peer credit and buffered-data allowance; managed retains a record through AwaitingAck, so transport ACK alone is not application credit return. [28][26][20]
  _3 independent sources (code); 4 of 4 quotes found where cited; inferred from them_
  - [28] ".min(self.send_window.saturating_sub(self.buffered_data))" (write_limit994) · quote found
  - [26] "state.0 = Slot::AwaitingAck { start, bytes };" (flush840) · quote found
  - [26] "fn consumed(&self, offset: u64) -> bool" (consumed771) · quote found
  - [20] "if self.charge.grow(extra).is_ok()" (Output::capacity203) · quote found
- **B · 72 (good)** The named block maximum is not an outstanding-I/O count. Pinned Linux derives its default tag depth from actual free descriptors and indirect support; header/data/status scatterlists and refill/completion make descriptor cost and logical request count distinct. [30][35]
  _1 independent source (code); 3 of 3 quotes found where cited; inferred from them_
  - [30] "queue_depth = vblk->vqs[0].vq->num_free;" (1470) · quote found
  - [30] "queue_depth /= 2;" (1473) · quote found
  - [35] "descs_used = total_sg;" (586) · quote found
- **B · 72 (good)** The named FS literal affects both hiprio and normal request rings, and pinned Linux also uses actual request vring size to limit request pages. A size decision must preserve separate control progress and request splitting semantics, rather than treating depth as only concurrency/memory. [31]
  _1 independent source (code); 2 of 2 quotes found where cited; inferred from them_
  - [31] "virtqueue_size - FUSE_HEADER_OVERHEAD" (1704) · quote found
  - [31] "ret = virtqueue_add_outbuf(vq, &sg, 1, forget, GFP_ATOMIC);" (639) · quote found
- **B · 79 (good)** The named GPU control queue can retain fenced requests beyond parsing. Linux waits for actual free descriptor demand and processes response/fence completion; measurement must distinguish command, fence, used/reclaimed buffer and native renderer ownership. [32][37]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [32] "wait_event(vgdev->ctrlq.ack_queue, vq->num_free >= elemcnt);" (398) · quote found
  - [37] "self.held.insert(chain.head);" (290) · quote found
  - [37] "let budget = usize::from(control.size());" (583) · quote found
- **B · 79 (good)** For the named current Shards IOMMU request path, indirect descriptors are not offered and Linux submits two SG elements. Thus actual N main-ring descriptors can hold at most floor(N/2) such requests, not N requests; full-queue sync/retry also changes residence and guest waiting. [34][33][35]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [34] "feature::VERSION_1 | F_INPUT_RANGE | F_DOMAIN_RANGE | F_MAP_UNMAP | F_PROBE | F_MMIO | bypass" (features912) · quote found
  - [33] "ret = virtqueue_add_sgs(vq, sg, 1, 1, req, GFP_ATOMIC);" (248) · quote found
  - [35] "descs_used = total_sg;" (586) · quote found
- **C · 68 (moderate)** Little's law relates long-run mean occupancy, effective arrival rate and residence at a consistent system boundary. It does not choose a worst-case/tail descriptor maximum from one benchmark or a service-only mean; burst, censoring, unit and authority constraints need separate root data. [36]
  _1 independent source (a peer-reviewed paper); 1 of 1 quotes found where cited; inferred from them_
  - [36] "long-term average arrival rate, number in the system, and wait per item" (long-run discussion) · quote found

**Sources**

1. Canonical Shards binding R01–R16, SHA256 d634fcb9… (a document, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-poller-research/docs/development-rules.md
2. User-created sustained constants lane goal and approved topic queue (kiln's records, internal) — plan:01a1c5cfb0f6736f910c8f76dcbfc37f
3. Shards Poller::wait at exact main24f (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-poller-research/crates/vmm/src/platform/unix.rs:2040-2095
4. Scoped Poller/inotify research; c0db checkpoint plus contract continuation (a document, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-poller-research/docs/research/managed-poller-capacity.md
5. Linux v6.18 eventpoll implementation, commit7d0a66e4 (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/eventpoll.c
6. XNU xnu-11417.101.15 event implementation, e3723e1f (code, internal) — https://raw.githubusercontent.com/apple-oss-distributions/xnu/e3723e1f17661b24996789d8afc084c0c3303b26/bsd/kern/kern_event.c
7. Rust1.91 Vec contract, commitf8297e35 (code, internal) — https://raw.githubusercontent.com/rust-lang/rust/f8297e351a40c1439a467bbbb6879088047f50b3/library/alloc/src/vec/mod.rs
8. Linux v6.18 inotify UAPI (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/inotify.h
9. Linux v6.18 inotify complete-record reader (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_user.c
10. Linux v6.18 file versus directory event dispatch (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/fsnotify.c
11. RFC8446 TLS1.3 Handshake and record framing (a standard, external) — https://www.rfc-editor.org/rfc/rfc8446.txt · 2018-08
12. RFC9000 QUIC datagram size, path limits and stream credits (a standard, external) — https://www.rfc-editor.org/rfc/rfc9000.txt · 2021-05
13. RFC9221 unreliable QUIC DATAGRAM transport and flow/congestion rules (a standard, external) — https://www.rfc-editor.org/rfc/rfc9221.txt · 2022-03
14. libevent2.1.12 retained epoll storage, commit5df3037d (code, internal) — https://raw.githubusercontent.com/libevent/libevent/5df3037d10556bfcb675bc73e516978b75fc7bc7/epoll.c
15. mio1.1 caller-owned native event storage, commit3a1b4718 (code, internal) — https://raw.githubusercontent.com/tokio-rs/mio/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/sys/unix/selector/epoll.rs
16. Source URL/revision/date/SHA256 manifest and policy readbacks (data, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-poller-research/docs/research/measurements/managed-poller-research/source-identity.json
17. Exact24f cleanup Client::wait_change caller (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/shards/tests/managed_cleanup.rs:180-205
18. Exact24f daemon follow_all caller (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/shards/src/daemon/follow.rs:220-239
19. RFC 9001 carrying TLS messages and buffering ownership (a standard, external) — https://www.rfc-editor.org/rfc/rfc9001.txt · 2021-05
20. Shards managed capture reservation and handshake parameter at 24f (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/shards/src/daemon/managed.rs
21. Pinned hyper-quic CRYPTO buffering (code, internal) — https://raw.githubusercontent.com/hyper-light/hyper-raft/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/mod.rs
22. Pinned hyper-tls message-size check (code, internal) — https://raw.githubusercontent.com/hyper-light/hyper-raft/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/msgs/deframer/handshake.rs
23. Pinned hyper-tls ordered handshake append (code, internal) — https://raw.githubusercontent.com/hyper-light/hyper-raft/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/quic.rs
24. TLS handshake capacity and admission research with identity ledger (a document, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-poller-research/docs/research/measurements/managed-poller-research/tls-handshake.md
25. Shards bounded UDP Driver at24f unchanged3ea (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/remote/src/net.rs
26. Shards managed transport and application credit at24f unchanged3ea (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/remote/src/managed.rs
27. Pinned hyper-quic endpoint receive advertisement (code, internal) — https://raw.githubusercontent.com/hyper-light/hyper-raft/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/config/mod.rs
28. Pinned hyper-quic stream send storage versus peer flow limit (code, internal) — https://raw.githubusercontent.com/hyper-light/hyper-raft/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/state.rs
29. RFC9002 congestion control independent of application storage admission (a standard, external) — https://www.rfc-editor.org/rfc/rfc9002.txt · 2021-05
30. Pinned Linux virtio block request and queue-depth path (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/block/virtio_blk.c
31. Pinned Linux virtio-fs enqueue/refill and request page limit (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/fuse/virtio_fs.c
32. Pinned Linux virtio GPU control descriptor wait (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/gpu/drm/virtio/virtgpu_vq.c
33. Pinned Linux virtio IOMMU two-SG request and full synchronization (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/iommu/virtio-iommu.c
34. Shards IOMMU actual offered features at24f unchanged7c26 (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/vmm/src/devices/virtio/iommu.rs
35. Pinned Linux main ring direct/indirect descriptor accounting (code, internal) — https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/virtio/virtio_ring.c
36. Little's Law as Viewed on Its 50th Anniversary, author reprint (a peer-reviewed paper, external) — https://projectproduction.org/journal/reprint-littles-law-as-viewed-on-its-50th-anniversary/
37. Shards GPU held-fence and control round at24f unchanged7c26 (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/crates/vmm/src/devices/virtio/gpu/mod.rs
