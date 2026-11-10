Exact f21-f22 findings and returned Sources excerpt from Kiln research
01a1c5cfb0fa7605a915fa767f65a9e4. Automated source grades are not a
native result, semantic certification or sizing recommendation.

- **B · 79 (good)** For the existing successful finite Poller fixture, a necessary wait-record lower bound is rounds times the sum of ceiling(count/native_batch), not merely one record per case-round. Empty/partial batches can require more, so neither that bound nor the parser's smaller minimum selects a safe record cap or production size. [3][38][39]
  _2 independent sources (code); 3 of 3 quotes found where cited; inferred from them_
  - [3] "const AT_ONCE: usize = 64;" (Poller::wait baseline) · quote found
  - [38] "while round.complete().is_err()" (run_round) · quote found
  - [39] ".checked_mul(rounds)" (Inputs::parse minimum) · quote found
- **B · 72 (good)** The existing Poller baseline's wait timing, write-completion-to-return and return-to-drain samples cover different fixture phases; retained Vec capacity is not an allocation count. The root wrapper suppresses summaries for incomplete runs, and provides no production fairness, oneshot, kernel-ready latency or comparative resource proof. [38][40]
  _1 independent source (code); 2 of 2 quotes found where cited; inferred from them_
  - [38] "wait_ns: observed.duration_since(before).as_nanos()," (WaitSample construction) · quote found
  - [40] "if complete else []" (summary sample selection) · quote found

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
38. Existing root-only Poller fixture exactf2a (code, internal) — https://raw.githubusercontent.com/hyper-light/shards/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/fixture.rs
39. Existing Poller baseline pure input helper exactf2a (code, internal) — https://raw.githubusercontent.com/hyper-light/shards/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/accounting.rs
40. Existing root-only Poller runner exactf2a (code, internal) — https://raw.githubusercontent.com/hyper-light/shards/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/run.py
