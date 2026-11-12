# The engine's bytes through a ring in its guest's memory (decision v1, D230)

Implementer record, branch `p/engine-ring-v1` from `origin/p/vsock-credit-v1` (1e448c7e).
POLICY_READ: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, docs/development-rules.md
sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer,
coordinator 01a148884a4770fdaf0111f3f792a248
([receipt](measurements/engine-ring-v1/POLICY_READ.json)). Nothing in this record was run
on a VM by this implementer: every number below is cited from a named record, and every
measurement the decision needs is listed as owed in "Acceptance and measurements".

## 1. The need

`--use-api-socket` (D181) gives a run's command its own microVM's engine. Each connection
the command makes to `/run/shards/engine.sock` is taken by init's relay process, which dials
the host's vsock `ENGINE_PORT` (1029), says `HELLO` and the run's token, and pumps bytes
both ways (`crates/init/src/engine.rs`, the pump in `crates/init/src/relay.rs`). On the host
the VM process's vsock device delivers the guest's connection as one end of a socket pair
(`VsockHost::every`), its hello reader checks the token
(`crates/shards/src/workload.rs` `accept_hellos`, `crates/shards/src/warm.rs`
`serve_engine`) and hands that end to the daemon (`ENGINE_CONN`, SCM_RIGHTS), which
serves the Engine API on it; on Windows the VM relays it onto a pipe the daemon gives
(`warm.rs` `relay_engine`, D195).

What that path moves, 256 MiB at p50, KVM on the runner `adacachy`
([vsock-path-v1](https://github.com/hyper-light/shards/blob/p/vsock-path-v1/docs/research/measurements/vsock-path-v1/README.md),
measurement-only branch; PM M8 of docs/research/virtio-io-exits.md asked for it):

| record | path | guest -> host | host -> guest | 64 B round trip |
|---|---|---|---|---|
| r378, load 76-90 | direct vsock | 143.2 ms | 106.5 ms | 53.7 µs (p99 105) |
| r378 | relayed (today) | 238.9 ms | 109.8 ms | 63.6 µs (p99 129) |
| r429, load 27-58, 64 KiB window | relayed | 244.4 ms | 123.6 ms | |
| r429, credit fix, 4 MiB window | relayed | 84.3 ms | 85.9 ms | |
| r380 | Unix socket pair alone, no relay | 11.2 ms | | |
| r380 | the relay's pump over socket pairs | 15.6 ms | | |
| r430, load 38-55 | 4 MiB shared ring, both sides spinning | 3.8 ms | 25.1 ms | |
| r421, M3 Ultra (HVF), 64 KiB socket buffers | direct / relayed | 90.3 / 150.0 ms | 106.0 ms direct | 30.5 / 40.6 µs |

The target: at least 10x today's relayed throughput for bulk transfers (guest to host
24.4 ms or less for 256 MiB, host to guest 12.4 ms or less, KVM; the same ratios on every
other backend against its own baseline), with no regression in small-request latency,
correct and robust against an untrusted guest, on every supported host (Linux x86_64 and
aarch64, gnu and musl, KVM; macOS arm64, Hypervisor.framework; Windows x86_64 and arm64,
WHP) and its own-architecture Linux guest.

What bounds today's path is vsock itself: the relay alone over socket pairs is within 1.4x
of the floor any relay has (r380), and the credit fix with a 4 MiB window (r429), vsock's
best case, still takes 84-86 ms: 2.9x and 1.4x today's, short of 10x either way. A shared ring between
the guest and the host moved 256 MiB in 3.8 ms one way and 25.1 ms the other (r430).

## 2. Decision

1. **The ring is in the guest's own RAM.** Init's relay allocates each ring slot from a
   memory file of its own, has the guest's kernel pin its pages for good (a long-term pin,
   `FOLL_LONGTERM`, through io_uring's registered buffers on a ring init made and holds),
   reads their guest-physical addresses from `/proc/self/pagemap`, and names them to the
   host in its hello. The host reaches those pages as it reaches every virtqueue: through
   the VM process's `GuestMemory`, on every hypervisor, with no window, no new device and
   no snapshot format of its own.
2. **The host end is the VM process's vsock device.** A connection to `ENGINE_PORT` whose
   hello offers a ring, once its owner (the engine's hello reader) has checked the run's
   token, has the device move its bytes between the ring and the connection's host socket,
   the very socket the daemon already serves (Unix) or the VM already relays to the
   daemon's pipe (Windows). The daemon is unchanged. The vsock connection stays: setup, the
   token, wakeups, half-close and EOF, and the whole path wherever no ring is made.
3. **Wakeups are vsock bytes, sent only to a side that said it waits.** Each side keeps a
   waiting flag beside the ring, set with a full fence before it sleeps, and the other side
   swaps it clear after it publishes and sends one doorbell byte on the connection only if
   it was set (netring's protocol, PM M83). A message costs the same wakeups it costs
   today; neither side spins in v1.
4. **Plain vsock, as today, wherever no ring is made**: a host or a guest of a build
   without rings, a guest kernel that cannot pin or hides physical addresses, a relay out
   of ring slots, an offer the device refuses.

## 3. Where the host end lives

The brief named two homes, the VM process (which then writes the daemon's socket) and the
daemon itself. There is a third, which is the one taken: the VM process's vsock device, in
the thread that already moves the connection's bytes.

**Not the daemon.** The daemon is the trusted core: it holds the user's store, credentials
and every other microVM. shards keeps what a guest drives in the confined VM process ("A
flaw in it would reach what the user can, so it narrows itself", D30, "Confining the VM
process") and keeps a second untrusted parser in a process of its own rather than in the
VMM (D31: "A flaw in the network stack, which parses whatever the guest and the Internet
send, reaches only that process"). A daemon that mapped memory the guest writes at any
moment would put the ring's index checks one bug away from the guest owning the daemon;
in the VM process the same bug reaches a process confined to one VM (seccomp on Linux, App
Sandbox on macOS, D30). The daemon end would also need the ring in a memory object both
processes map, so the guest would need a device that maps such an object (a DAX window or
a pmem device, section 4), and the daemon a new framed stream reader on every platform.

**Not a pump thread of the VM process between the device's socket and the daemon's.**
Such a pump (ring <-> new socket pair to the daemon, the vsock connection for doorbells)
needs no change to the vsock device, but it puts one more thread wakeup and one more Unix
socket hop on every message whose reader sleeps: guest to host, the device's worker wakes
the pump, which then wakes the daemon; host to guest, the daemon's write wakes the pump,
which then wakes the worker. Today the worker wakes the daemon directly and the daemon's
write wakes the worker directly. The evidence of what a hop costs: the guest's own relay
hop adds 10 µs to the 64 B round trip on KVM (53.7 direct, 63.6 relayed, r378) and 10 on
HVF (30.5, 40.6, r421); a Unix stream round trip between two processes on macOS took 22.2
µs at p50 (PM M83). The pump would regress small requests by about one such hop each way,
which the task forbids. Spinning would hide it only for answers that come within the spin.

**The vsock device.** Its worker already reads the guest's packets and writes the
connection's host socket, and reads that socket and fills the guest's buffers
(`crates/vmm/src/devices/virtio/vsock/conn.rs` `send_host`, `read_host`). With a ring the
same thread copies between the ring's pages and the same socket (`writev` from the ring,
`readv` into it, as it does today between virtqueue buffers and the socket), so a message
costs exactly the wakeups it costs today: the guest's doorbell wakes the worker as its data
packet does now, and the daemon's write wakes the worker as now. The daemon keeps reading
and writing one socket. On Windows the VM's relay onto the daemon's pipe stays as it is.
What moves is the per-packet work (a virtqueue descriptor, a header, a credit update and an
interrupt for every 64 KiB at most, `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`): a ring moves as much
as its free space each turn. The host's copies stay two (the worker's `writev` into the
socket, the daemon's read), as today; the guest's stay two (the client's write, the relay's
`recv` straight into the ring). The ring logic lives in a new module of the vsock device
(`vsock/ring.rs`); `conn.rs` and `muxer.rs` gain only the hooks that hand a ringed
connection's data path to it.

The single worker moves every ringed connection's bytes, as it moves every connection's
bytes today; one ring stream can keep it busy, and the aggregate a VM's rings reach is one
host thread's copy rate (owed: measured, section 10).

## 4. How the guest maps the ring, on every backend

| mechanism | KVM x86_64 / arm64 | HVF arm64 | WHP x86_64 / arm64 | guest needs | taken |
|---|---|---|---|---|---|
| guest RAM pinned by the guest, named by guest-physical address | the VM process's own mapping of RAM (`GuestMemory`), as virtqueues | same | same | io_uring with a sparse buffer table (5.19+; shared memory files registrable since 5.14), pagemap PFNs (CAP_SYS_ADMIN) | **yes** |
| a file of a share served from a shared-memory object through its DAX window (D189) | one memslot over the window, MAP_FIXED ranges | `hv_vm_map` per 2 MiB range | `WHvMapGpaRange` per range of a section | VIRTIO_FS, FUSE_DAX | no |
| virtio-pmem device in devdax mode | memslot | `hv_vm_map` | `WHvMapGpaRange` | DEV_DAX_PMEM, a dax info block | no |
| /dev/mem of a reserved range | | | | DEVMEM; arm64 maps it Device | no |

**Guest RAM, pinned.** The relay's slot is a `memfd_create(2)` file mapped shared, its
pages populated, then registered as an io_uring fixed buffer: io_uring pins them with
`pin_user_pages_fast(..., FOLL_WRITE | FOLL_LONGTERM, ...)` (Linux v6.18
io_uring/memmap.c l. 40-75, from io_uring/rsrc.c l. 799), the pin
Documentation/core-api/pin_user_pages.rst describes for "DMA buffers... needed for a long
time": pages a pin holds are first moved out of ZONE_MOVABLE and CMA
(`folio_is_longterm_pinnable`, include/linux/mm.h l. 2008-2034; mm/gup.c
`check_and_migrate_movable_pages`) and then cannot migrate, since migration freezes a
folio's references at their expected count, which a pin's bias exceeds (mm/migrate.c l.
563-707), so compaction, memory offlining (virtio-mem's unplug, D148) and khugepaged leave
them in place. A shared memory file is a mapping GUP pins long-term with write access:
shmem needs no dirty tracking (mm/gup.c `writable_file_mapping_allowed`, l. 1182-1198), and
io_uring has registered such files' pages since 5.14 (40dad765c045, "enable shmem/memfd
memory registration").
The physical addresses come from `/proc/self/pagemap`, whose frame numbers only a reader
with CAP_SYS_ADMIN sees (fs/proc/task_mmu.c l. 2204); the relay is init's, root with every
capability. A guest's frame number times its page size is its guest-physical address on
every hypervisor.

Who holds the pin matters. A slot whose relay died would otherwise be unpinned and freed
while the device, which learns of the death only from the vsock RST the guest's kernel
sends as the relay's descriptors close, may still be writing to it: io_uring's teardown
and the RST are both deferred work with no order between them. So init makes the io_uring
instance before it forks the relay (`io_uring_setup(2)` is cheap and returns a
close-on-exec descriptor), keeps its own descriptor for the VM's life, and the relay
registers its slots on the descriptor it inherited: a registered buffer stays pinned until
the instance goes, which init's descriptor prevents. io_uring descriptors cannot be passed
by SCM_RIGHTS (net/core/scm.c l. 117-118, "don't allow io_uring files"), which is why the
instance is inherited and not handed back. A ring of no SINGLE_ISSUER flag takes
registrations from any task (io_uring/register.c l. 635). A slot is never freed; a slot a
connection lets go of is reused by the next (section 7).

What each backend needs to reach those pages: nothing new. The VM process maps all of
guest RAM in every backend and every virtio device reads and writes it there
(`GuestMemory::io_ptr`, `Access::load_u16`/`store_u16` for virtqueue indices). Memory the
guest plugged (virtio-mem, `online_kernel`, D148) is reached the same way while plugged
(`GuestMemory::host_ptr`), and the device pins what a ring uses on the host side too
(`GuestMemory::pin`), which refuses its unplug and its discard (free page reporting, D151)
while the ring holds it (`Access::unplug`, `Access::discard`: "pinned for a device's DMA").

**Not a DAX window.** The prototype of r430 mapped a file of a `--share` directory through
its window. A product ring there needs a virtio-fs device of its own in every VM (the
run's shares are its workload's; the join share's server is another process, started only
for joiners), so every template would be keyed by it and the snapshot format would change;
a FUSE server for its one kind of file in the VM process; shared-memory objects handed to
the window as descriptors (memfd, POSIX shm, a pagefile section); and windows are off by
default on a host that is itself a virtual machine on an AMD CPU (D189, PM M308), where
CI's x86_64 runners are. The window maps a range as the guest first touches it (a FUSE
round trip and a host mapping each 2 MiB). It buys nothing guest RAM lacks: both are
normal write-back memory to both sides (section 5), and RAM needs no device.

**Not virtio-pmem in devdax mode.** The pinned kernel has DEV_DAX_PMEM (Firecracker's base
configs set CONFIG_DEV_DAX=y and CONFIG_DEV_DAX_PMEM=y; the brief said DEV_DAX was unset,
which the configs here contradict), so a pmem device whose head holds a dax info block
(as D190's layer holds a pfn info block) would give init a `/dev/daxN.M` to map. It needs
a pmem device in every VM, a head, template keys and a snapshot format, and nothing in
shards has exercised devdax. Guest RAM needs none of it.

**Not /dev/mem.** shards' kernels have DEVMEM with STRICT_DEVMEM (for the control page), so
init could map a range the machine reserves; but `/dev/mem` maps non-RAM uncached
(x86 UC-, arm64 Device memory), where atomics fault on arm64 and every access is slow.

**What each guest architecture needs.** x86_64 and arm64 guests of 4 KiB pages
(the pinned kernel: `CONFIG_ARM64_4K_PAGES=y`); the offer names extents in bytes on 4 KiB
boundaries, so a 16 KiB or 64 KiB page guest's pages are whole extents too. 64-bit atomics
on naturally aligned words on both.

## 5. Memory ordering and coherence

**Cacheability.** Guest RAM is normal write-back memory to the guest and to the VM process
on every backend: x86 KVM maps it write-back in EPT, ignoring guest PAT unless the VM has
non-coherent DMA (arch/x86/kvm/vmx/vmx.c `vmx_get_mt_mask`, l. 7696-7710; a userspace
shared mapping is write-back in guest PAT either way); arm64 KVM forces a non-cacheable
stage-2 mapping only for PFN-mapped memory without a struct page (arch/arm64/kvm/mmu.c l.
1755-1786), never for RAM; HVF and WHP map the VM process's own memory with
`hv_vm_map`/`WHvMapGpaRange`, where every device's virtqueue reads and writes already
depend on the guest's and the host's views being one (D189's coherence measurements on
HVF and WHP, PM records cited there). The guest's own mapping of the slot is a shared
anonymous file, write-back (`PAGE_SHARED`). So both sides are coherent in one inner
shareable domain, as a virtqueue's device and driver are (virtio 1.3 §2.7, split
virtqueues, whose index updates memory barriers order).

**Ordering.** Each direction is a single-producer single-consumer byte ring. Positions are
free-running u64 byte counts; each side keeps its own position privately and publishes a
copy for its peer with a store-release, and reads the peer's with a load-acquire; the data
it wrote before a release is visible to the peer after its acquire (x86 TSO; arm64 STLR and
LDAR, Arm ARM B2.3, other-multi-copy-atomic). Sleeping is Dekker's handshake, as netring's
(crates/netring/src/lib.rs): a side about to sleep stores its waiting flag, fences
(SeqCst: x86 MFENCE or a locked RMW, arm64 DMB ISH), and looks again at the peer's
position; the peer stores its position, fences, then swaps the flag to 0 and rings only if
it was 1. One of the two always sees the other. The doorbell's own path (vsock packet, the
virtqueue's release/acquire on its index, an interrupt or an ioeventfd) carries the
position store before it to the woken side, which loads with acquire after it wakes.

**False sharing.** Each published field has a line of its own, 128 bytes apart: Apple
silicon's cache line is 128 bytes (`sysctl hw.cachelinesize` reads 128 on this record's
M5 Max), and on x86 the spatial prefetcher pairs 64-byte lines.

## 6. Waking a side, and small requests

A message costs what it costs today: one doorbell packet in place of one data packet, the
same wakeups. While data keeps coming, neither side sleeps, so neither rings: a bulk
transfer moves without doorbells or exits but where a side ran dry or full. There is no
spinning in v1: the device's worker serves every connection and must not spin for one, and
the relay's spin would buy only what the measurement shows it buys. Adaptive spinning is
the follow-up the measurements decide (the two-competitive spin-then-block rule: spin no
longer than a block and wake-up cost, measured online, Karlin et al., "Competitive
randomized algorithms for non-uniform problems", SODA 1990; D8's "adaptive polling only
above a rate threshold"), owed as a measured A/B, not built.

Small-request latency gains nothing and loses nothing by construction; its measurement is
owed (section 10), paced as a user's requests come and back to back.

## 7. Flow control, ends, many connections, memory

**Flow.** The ring is the flow control: a producer writes only into free space, a consumer
frees what it has passed on. vsock's credit governs only doorbell bytes, a byte a wait.
A host socket that takes nothing more leaves the to-host ring full and the relay asleep
on it; a client that reads nothing leaves the to-guest ring full and the device's read of
the host socket off (its interest dropped, as a full guest credit drops it today).

**Half-close and EOF.** The relay that reads its client's EOF publishes its last position,
then sends one `END` byte on the connection; the device, told, drains the to-host ring to
the host socket and only then shuts that socket's write side, as it finishes buffered data
before a shutdown today (`Conn::flush`). Not a vsock `SHUTDOWN(SEND)`: that would end the
relay's doorbells with it, and a client that has ended its writes may still be reading a
long answer, whose ring the device may fill and wait on until the relay rings that it made
room (found as the relay's pump was written; the device's test
`a_clients_end_reaches_the_host_after_its_last_bytes_and_doorbells_go_on`). The host socket's EOF has
the device publish what it read last, then send `SHUTDOWN(SEND)`; the relay, reading EOF,
drains the to-guest ring to its client before it ends the client both ways (D181's engine
end policy, `EndPolicy::EnginePeerEnd`). Order holds because each side's last position is
published before the shutdown that announces it and read after it.

**Errors.** A byte other than the doorbell and the end the protocol names, a position the peer
could not have reached, or a host socket error ends the connection with an RST, as a
broken connection ends today; the relay ends its client the same way.

**Many connections.** Each connection has a ring of its own, so none waits behind another.
The worker turns between connections as now.

**Sizes from data (R03).** Every literal and its derivation, as first decided; the ring's
length, the slots' count and their lives are superseded by section 16 (ring-pins-v1),
whose table holds:

| literal | value | derivation |
|---|---|---|
| ring bytes per direction, S | 2 MiB | Bandwidth-delay product: the producer must not wait for a sleeping consumer's wakeup at the target rate. Target rate: 10x the faster direction's relayed baseline, 256 MiB / 12.36 ms = 21.7 GB/s (r429). Wakeup: one way of the relayed 64 B round trip at p99, 129 / 2 = 64.5 µs (r378; r425 within noise; HVF's 40.6 µs p50, r421, lower). 21.7 GB/s x 64.5 µs = 1.40 MB, rounded up to a power of two so that a position's offset is a mask. Validated by an owed sweep (256 KiB to 4 MiB). Cold start: the first fill of a fresh slot faults nothing (pages populated and pinned at allocation). |
| control block | one 4 KiB page | The smallest page of every supported guest; 8 fields at 128-byte strides (section 5) fill 1 KiB of it. |
| field stride | 128 bytes | Apple silicon's cache line; x86's adjacent-line prefetch. |
| slots a relay pins at most, C | max(1, floor(online CPUs / 2)) | A stream through a ring keeps two of the guest's threads busy (its client's and the relay's thread for it), so no more streams than half the guest's CPUs can move bytes at once; a ring past them would sit idle. Slots are made as connections need them and kept for reuse, never more than C; a connection that finds none takes plain vsock. Worst case C x (2 S + 4 KiB) pinned. |
| doorbell byte, end byte | 0x01, 0x02 | Protocol constants; any other byte is a protocol error. |
| offer extents at most | (4 KiB + 2 S) / 4 KiB | A slot's pages, none merged: the most a relay can name. Bounds the hello the host reads. |
| offer and reply framing | 16-byte offer header, 16-byte extents, 8-byte reply | Protocol constants (section 8). |

The pinned memory comes from the guest's RAM, so a hostile guest gains no host memory by
asking for rings; the host's own cost per ringed connection is its run of host pointers
(16 bytes a merged extent) and nothing per byte.

**Setup and teardown.** Init makes the io_uring instance when it starts a relay whose run's
spec says the host takes rings, and nowhere else. The relay registers a sparse table of C
buffers on it at its first ring and fills an entry as it makes each slot
(IORING_REGISTER_BUFFERS2 with IORING_RSRC_REGISTER_SPARSE, a8da73a32b6e, Linux 5.19;
IORING_REGISTER_BUFFERS_UPDATE, 992da01aa932, 5.13; v6.18 io_uring/rsrc.c
`io_register_rsrc` l. 395-424 registers a table of empty entries, `__io_sqe_buffers_update`
fills one and pins it). The device attaches a ring when the owner's check of
the token passes and the offer's extents are all guest RAM, and detaches it as the
connection ends however it ends (RST, both directions shut, a kill, the device's reset,
the VM's end), unpinning its host pins then; the guest's slot is free again for the relay
once the vsock socket is closed on the guest's side and the device's RST or reply has come,
and the device refuses an offer of a slot whose pages it still holds for another
connection (the host is the authority on which slots are in use).

## 8. Protocol (version 1; version 2 in section 16)

On `ENGINE_PORT`, after the hello's header and token, a relay whose run's spec carries
`engine-ring=1` may send an offer in the same hello: the hello's payload is then the token
followed by the offer, its length `TOKEN + 16 + 16n`. All integers little-endian.

- **Offer:** `magic` u32 (`b"RING"`), `version` u16 (1), `reserved` u16 (0), `ring` u32 (S,
  a power of two from 4 KiB to 2 MiB), `extents` u32 (n), then n extents of `gpa` u64 and
  `len` u64: the slot's bytes in order, its control page then the to-host ring then the
  to-guest ring, 4 KiB + 2S bytes, each extent on 4 KiB boundaries and a whole number of
  4 KiB.
- **Owner (VM process):** reads the hello whole; a wrong token closes the connection as
  today, before anything of the offer is used. A right one with an offer has the device
  attach it before the connection goes to the daemon.
- **Device:** validates each extent against guest RAM, sums them to the slot's length,
  pins them, and answers in band before any of the daemon's bytes: an 8-byte reply,
  `b"RNG"`, a status byte (0 attached; 1 refused, malformed; 2 refused, not guest RAM;
  3 refused, in use), four zero bytes. Attached, every later byte the guest sends is a
  doorbell (0x01) or, once, its client's end (0x02); every later byte the host's socket
  sends goes into the to-guest ring; and the device sends the guest only doorbells, then
  `SHUTDOWN(SEND)` after the host socket's end. Refused, the connection is plain vsock.
- **Owner's cue (host side only):** the owner offers the parsed slot to the device through
  the connection's handle (or a refusal where the offer's bytes are malformed), then
  writes one byte, `CUE`, into the connection before the daemon has it; the device takes
  that byte as its cue to attach and answer.
- **Control page:** at 0, 128, 256 and 384 the to-host head (relay), to-host tail (device),
  to-guest head (device), to-guest tail (relay), each a u64; at 512, 640, 768 and 896 the
  waiting flags, each a u32: device waits for to-host data, relay waits for to-host room,
  relay waits for to-guest data, device waits for to-guest room.

The state machines (R04): the device's connection is Plain, Offered (the owner's offer
waits for the device), or Ringed with each direction Streaming, Draining (the peer's end
seen, its last bytes still to pass) or Closed; Refused returns to Plain once its reply is
sent; any protocol error is a kill (RST). The relay's connection is Offering (offer sent,
reply awaited), Ringed or Plain, each direction Reading, Draining or WriteClosed as the
existing pump's (`relay.rs` `Half`).

## 9. Security

- **The token comes before any address.** An offer names guest-physical addresses the host
  will write. A workload that could offer one could make the host write daemon bytes and
  position words over its guest kernel's memory. So nothing of an offer is used until the
  run's token, which only init and its relay hold (D181), has been checked in constant
  time; a connection without it is closed as today.
- **Every guest byte is hostile to the host.** The offer is copied out of the stream, then
  parsed: lengths bounded before any allocation, each extent checked to be guest RAM
  (`host_ptr`) and on 4 KiB boundaries, their sum exactly the slot's length. The guest's
  positions are read once a turn, with acquire, and believed only where they could be:
  the to-host head at most S ahead of the device's tail and never behind it, the to-guest
  tail never past the device's head nor behind its own last copy; a position that breaks
  either ends the connection. Offsets into the ring are the position modulo S, so no
  guest value reaches an address outside the slot's pinned pages. The ring's bytes are
  never parsed by the host: they go to the daemon's socket as stream bytes, which the
  daemon already treats as hostile, the Engine API's HTTP. Data moves by `readv`/`writev`
  on the pinned pages (the kernel answers a page gone with EFAULT, which ends the
  connection) and positions and flags by atomic access under the device's pin; no Rust
  reference ever covers ring memory.
- **What the guest can do to itself** (a hostile guest kernel lying about frames,
  migrating or reporting pinned pages free) spoils only its own engine streams; the host's
  pins keep its own pointers valid.
- **Only init's relay reaches the ring.** Its slots are memory files the relay made after
  init forked it, mapped in the relay alone (`MADV_DONTFORK`), never passed to any other
  process; the workload runs in its own PID and mount namespaces and cannot open the
  relay's `/proc` (D181). A workload given the whole microVM (`--privileged`,
  `apparmor=unconfined` with CAP_SYS_ADMIN) reaches the relay's memory as it reaches init's
  token today (D121): no new reach.
- **No host memory grows with a guest's asks**: rings are guest RAM; the device keeps a
  run of host pointers a ring.

## 10. Snapshot, restore and templates

Rings exist only while a run's engine connections do. A template is saved before its run,
so it holds no ring and no io_uring instance. A running microVM saved whole (D176) keeps
its guest's slots and init's instance in its memory; its restored device resets every
stream it held with an RST, as today (D12), which ends each ringed connection on both
sides; the relay's next connections offer the same slots, still pinned at the same
guest-physical addresses, which the restored device validates afresh. A live move's
device writes are noted for the ring's pages as for a virtqueue's
(`GuestMemory::device_wrote`). No DAX window is involved (D189 unchanged).

## 11. Fallback

Plain vsock, today's path byte for byte, wherever any step fails: the spec has no
`engine-ring=1` (a host of another build, or none advertised); `io_uring_setup` fails
(no CONFIG_IO_URING, `kernel.io_uring_disabled`), registration fails (a kernel before
5.19's sparse buffer table), `pagemap` shows no frame numbers; C slots are in use; the device refuses the offer. The relay never needs a
kernel feature to serve a connection: rings are an extra layer over what every guest
kernel has (vsock).

## 12. Alternatives rejected, with evidence

- **vsock tuning alone.** The credit fix and a 4 MiB window bring the relayed path to 84-86
  ms both ways (r429), 2.9x and 1.4x today's, and a 16 MiB window no further (75-92 ms):
  short of 10x in both directions.
- **splice or sockmap in the guest.** Either takes the relay's userspace copy out (one of
  four copies); the relay alone already runs within 1.4x of the socket pair's floor
  (r380), and the transport behind it is still vsock's 84-244 ms.
- **The workload given the vsock connection itself** (seccomp user notification with
  `SECCOMP_IOCTL_NOTIF_ADDFD`, the token said first): removes the relay's hop, keeps vsock
  as the transport and its ceiling, and hands the workload a connection it could then use
  past the engine's framing; nothing gained against the target.
- **A guest driver of our own** (a virtio device exposing a shared-memory region to
  userspace): a kernel patch every guest needs, where io_uring and pagemap are in every
  kernel shards boots and most others; virtio-vsock's connection management duplicated.
- **A DAX window ring share, virtio-pmem devdax, /dev/mem**: section 4.
- **The daemon as the host end; a pump thread in the VM process**: section 3.

## 13. Build plan (each step its own commit, unit or light tests, red first where there is
something to be red)

1. `crates/abi`: the ring's layout, offer, reply and doorbell (`shards_abi::ring`), with
   their encodings' tests.
2. `crates/vmm/.../vsock/ring.rs`: an attached ring (offer validation, host pins, runs of
   host pointers), the device's halves (drain the to-host ring into a socket, fill the
   to-guest ring from one), Dekker flags and the doorbell decision; unit tests over
   `GuestMemory::anonymous` and socket pairs, hostile positions and offers among them.
3. The device's hooks (`conn.rs`, `muxer.rs`, `vm::ServedPort`): an offered connection, its
   attach, its data path, its ends; unit tests through the muxer's packet interface.
4. `crates/shards`: the engine's hello reader takes an offer after the token and hands it
   to the device; the daemon's spec says `engine-ring=1`.
5. `crates/init`: the io_uring instance init holds, the relay's slots (memfd, pin,
   pagemap), and the relay's ring pump with its fallback; unit tests with a host stand-in.
6. E2E tests and the measurement harness, written for ROOT to run.

## 14. Acceptance and measurements owed to ROOT (none run here)

Correctness, real VMs, on each of KVM x86_64 (adacachy), KVM arm64 (nested L1 on an M3,
PM M346), HVF arm64, WHP x86_64 and WHP arm64:
- 256 MiB each way through a `--use-api-socket` run's engine (an archive put into and got
  out of one of its containers), the bytes' SHA-256 equal, the device's log saying a ring
  was attached; the same with rings refused (spec without `engine-ring`), equal.
- Half-close each way: a client that ends its writes and still reads the whole answer; an
  engine that ends first, its last bytes all read before the client sees EOF.
- More connections than C at once: each served, those past C over plain vsock.
- Hostile offers from a test guest that knows the token: extents outside RAM, unaligned,
  short, overlapping a slot in use, a lying count; positions past the ring; a wrong
  doorbell byte. Each refused or ended, the VM process alive, its other connections
  served.
- The relay killed mid-transfer: the connection ends, nothing of the guest corrupted (a
  checksum of a workload's memory pattern), a later connection served.
- A run saved whole (D176) with a ringed connection open, restored, served afresh.

Measurements (vsock-path-v1's harness with a ring arm), n >= 11 rounds interleaved, p50,
p90, p99 and max, host, OS, revision and load, on every backend above:
- 256 MiB each way, relayed through the engine path with the ring, against plain vsock with
  the credit fix and against today's: the 10x target.
- 64 B round trips relayed, back to back and paced (gaps past a sleep's), ring against
  plain: no regression at p50 or p99 beyond the arms' spread.
- Connection setup (connect, hello with offer, reply) against today's.
- CPU of the VM process's vsock worker and the guest per GiB moved; the guest's pinned
  memory; the worker's aggregate with 1, 2 and 4 ringed streams.
- S swept from 256 KiB to 4 MiB to check the bandwidth-delay derivation.

macOS, Windows and aarch64 Linux compile checks of every step are owed too; this
implementer builds on the Linux runner alone.

## 15. Open questions that are the person's

- A ring pins up to C x (2S + 4 KiB) of the guest's memory in the relay, which runs
  outside the command's cgroup (D181), so the workload's memory limit does not count it.
  Charging it to the workload would need the relay's slots in the workload's cgroup. Kept
  outside, as the relay's own memory is today, unless the person says otherwise.
  **Answered (2026-11-11, through ROOT):** no C, and no user knob; demand is the run's
  (a slot a stream), supply is the budget owner's (the room init is told, D188), cost is
  the run's (the slots in the run's cgroup, outside the workload's). Section 16.

## 16. Pins bounded by use and by the run's room (ring-pins-v1, protocol version 2)

Implementer record, branch `p/ring-pins-v1` from `origin/p/integrate-engine-20261111`
(032b63b1c). POLICY_READ: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, rules sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer,
coordinator ROOT 01a148884a4770fdaf0111f3f792a248
([receipt](measurements/ring-pins-v1/POLICY_READ.json)); evidence in
[measurements/ring-pins-v1](measurements/ring-pins-v1/README.md). Nothing here ran on a VM.

**The need.** As first built a relay pinned up to max(1, CPUs/2) slots of 2 x 2 MiB + 4 KiB
for the VM's life, outside the workload's cgroup: 64 MiB on a 32-vCPU guest, for hundreds
of microVMs a host. The person's terms (2026-11-11, through ROOT): an idle microVM pins
nothing; pins are bounded by what is used; no number that is not learned or derived; V > 1
designed for (bounds that read CPUs read them as applied); no CPU cap at all; demand the
run's, supply the budget owner's room (D188), cost the run's; no user knob.

**What a slot's life is now.**
1. *Made as a stream starts*, from a memory file, pinned on init's instance (the instance
   and its pins are unchanged: a relay that dies leaves them pinned), where the room fits
   it, in the stream's starting ring; else the stream goes over plain vsock with a plain
   hello. Nothing is made before the first stream; nothing is kept for later ones.
2. *Grown while the stream needs it.* A ring the producer fills whole within its consumer's
   wake held the stream back: the relay swaps it (`SWAP` and an offer) for one twice as
   large, within the room. Growth stops at `MAX_RING`, at the room, or once the stream's
   ring holds it back no more.
3. *Freed after the device's `DETACH`*, and never before: as the stream ends (the host's
   end, which the device says by `DETACH`; or the client's departure, which the relay says
   by `DETACH` and hears answered), as the room asks for it while idle, or as a swap's old
   slot. One whose `DETACH` never comes stays pinned, counted (`lost`), until the VM ends.

**The detach protocol (version 2; `shards_abi::ring`).** On an attached connection each
side's bytes are control bytes until its own `DETACH`, then plain vsock:
- relay to device: `DOORBELL` 0x01, `END` 0x02 (its client's end), `DETACH` 0x03 (let go;
  plain from here), `SWAP` 0x04 then an offer (let go and take this slot instead);
- device to relay: `DOORBELL` 0x01, `DETACH` 0x03: *I hold no pin of the slot and will
  write it no more; the positions I published last are final.* Said in answer to the
  relay's `DETACH` or `SWAP` (then the swap's reply, `Refused(Ended)` where the host had
  already ended), as the host's socket ends (the host's end, once the relay's own `DETACH`
  has come, is vsock's `SHUTDOWN(SEND)`, so that a `SWAP` crossing it is still answered),
  and before an RST the device sends itself (a broken rule, a failed host socket), where
  the guest's credit has room for it. The device lets its `Ring` go (dropping its
  `GuestMemory` pins) before it queues `DETACH`, in its one worker thread, so the guest
  reads the word only after the last write.
- The relay frees a slot only on the device's `DETACH`; after it the relay passes on what
  the slot still holds: the host's bytes its client has not had, to the client, and its
  client's bytes the device had not taken, to the host as plain bytes, then its client's
  end where it came, before any plain byte of the connection. A connection that ends with
  no `DETACH` (a device reset, a restore's RSTs, an RST without credit for the word) keeps
  its slot pinned.
- The merged version 1 attached without saying the device waits (`HOST_WAITS_DATA`), so a
  relay that rings only a side that says it waits never told it of a connection's first
  request: every ringed engine connection hung (logs/r1, red; ROOT's r521 on real VMs:
  engine_ring 0 of 1, the engine suite 9 of 27 failed, all timeouts). The device now says it
  waits as it attaches (3480f9299, cherry-picked as cc1b52e61).

States (R04). Device (`conn/ringed.rs`): `Plain`, `Ringable`, `Refused` (a reply owed, the
host's end after it where it had come), `Ringed`, `Detached` (why: asked, the host's end,
broken; the old ring's reply owed or not; its `DETACH` said or not; the guest's bytes
unheard, plain, a swap's offer in pieces, or answered). Relay (`init ring.rs`): a stream
pumps until its device lets go, its room asks for it while idle, its ring holds it back
(swap: grown, kept, unringed, done, lost), its client goes, or the connection ends with no
word; each ends in the slot's release or its loss.

**Supply, accounting and pressure (D188).**
- *Where pins are counted.* The relay joins `run/engine`, a leaf of the run's cgroup (a
  parent with controllers holds no process: cgroup-v2.rst "No Internal Process
  Constraint"), as it makes its first slot, before any slot's pages are charged (a page's
  charge stays where it was made: "Memory Ownership"). So the slots are the run's memory:
  `memory.current` of `/sys/fs/cgroup/run`, which `plug::hold_within` (ROOM), `shrink_run`
  and `reclaim_cold` (RECLAIM) and the run's metrics (`read metrics`, scope run) read; the
  workload's own cgroup and limits are untouched (D181: the relay stays outside it). The
  kernel also counts every slot pinned to init, whose memory the instance was made in
  (`VmPin` of `/proc/1/status`; v6.18 io_uring/io_uring.c 3858, io_uring/rsrc.c 68-80), and
  the device logs each ring's bytes as it attaches and lets it go (`SHARDS_LOG=debug`).
- *Admission.* A slot of L bytes (or a swap's new one, the old still held) is made only
  where L fits: with a budget, `plug::cap()` less the run's `memory.current` (and the
  slots, where the join failed) less `plug::working_room()`; with none, `MemAvailable`
  less the working room; less slots being made. Read as applied, so V's working room is
  of the CPUs online then.
- *Pressure.* `ROOM` tells the relay first (`ring::told`, before `hold_within`): a told room
  R short of the working room W owes W - R bytes of idle slots, which idle streams pay as
  their slots go. `RECLAIM` asks every idle slot (`ring::give_back`, after its own
  measure); what goes, the next `RECLAIM` answer adds (`ring::given`), each byte once. Both
  reach the relay through a page and an eventfd init makes before it forks the relay
  (`ring::share`) and owns; both do nothing where the relay has made no slot, and never
  wait or fail their builtin. A live stream is never broken: idle is a stream asleep with
  both rings empty and nothing owed; a live one keeps its ring, grown no further, and goes
  plain at its next idle moment if the room still asks.

**Numbers (R03).**

| Number | Value | Derivation, cold start, bounds |
| --- | --- | --- |
| Stream's ring | `MIN_RING`..`MAX_RING`, powers of two | Learned per stream: the bytes found in a ring as its consumer wakes (or put between ringing a sleeping consumer and its first take) are its rate times that wake; a ring filled whole is the bound censored, doubled (Bentley and Yao 1976, doubling search; within 2x of the need). Cold start: the ring the last stream of the relay needed (a power of two past the most it held at a wake, past a ring that held it back), a relay's first at `MIN_RING`. Adversarial: the workload's own streams only cost the run's room. Test: `a_stream_learns_the_rings_it_needs`, `a_stream_starts_where_the_last_one_ended`. |
| `MIN_RING` | 4 KiB | The least an offer names, a page of every guest. |
| `MAX_RING` | 2 MiB | Largest power of two whose every turn one readv/writev reaches whole (S/4096 + 1 pages within 1,024 iovecs). Test: `the_largest_ring_is_the_largest_whose_every_turn_is_one_call`. |
| Slots | none counted | One a stream, two while it swaps; bounded by the room. |
| Room for pins | read as applied | Above. Test: `the_supply_leaves_pins_what_the_run_does_not_need`. |
| Owed by a told room | W - R where positive | Test: `a_told_room_owes_what_it_is_short_of_the_working_room`. |
| Fixed-buffer table | 2 x floor(RLIMIT_NOFILE / 3), at most 16,384 | A stream holds 3 descriptors and at most 2 slots, so the relay's descriptor limit (init's default 1,024 gives 682 entries, 5.4 KiB of the guest kernel's) bounds the slots at once; io_uring registers at most `IORING_MAX_REG_BUFFERS`. Test: `the_table_holds_as_many_slots_as_the_descriptors_let_streams_hold`. |
| Control bytes, version | 0x01-0x04, 2 | Protocol constants; any other byte is a broken rule. |
| Stand-in's 50 ms before its guard check | tests only | Time for a relay that would free early to do so; a correct one holds the slot however long. |

No idle timeout: an idle slot goes when the room or a give-back asks for it, or when its
stream ends; holding it costs nothing anyone asked for until then.

**Tests (unit; light; adacachy, linux/x86_64).** Red first: logs/r1 (the first request).
Green: abi 31, vsock 66, init 173 (logs/r3). Guard mutations: the relay freeing before the
device's `DETACH` fails `the_relay_frees_a_slot_only_after_the_devices_detach` ("the relay
let go of its slot before the device's DETACH"); the device answering `DETACH` with its
pins held fails five device tests, the guard among them
(`a_detach_is_answered_once_the_pins_are_gone_and_plain_vsock_follows`); the first-request
fix removed fails both of its tests (logs/r2).

**Owed to ROOT (real VMs, measurements; none run here).**
- E2E (`crates/shards/tests/engine_ring.rs`): `an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends`
  (1, 2 and the host's vCPUs; V and 2V streams at once), `a_give_back_lets_an_idle_slot_go`
  (`SHARDS_PRESSURE`), `a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain`
  (a user slice's `MemoryHigh`), and the existing bulk and measurement tests, on KVM x86_64
  and arm64, HVF, WHP x86_64 and arm64; the engine suite (D181), isolation's token test,
  budget and reclaim suites (a run with no engine socket unaffected).
- E2E mutations: `told()` removed from `ROOM` (the shrinking-room test must fail: pins past
  the room); the leaf join removed (`read metrics` scope run must lose the slots' bytes:
  pins outside `memory.current`).
- Measurements: slot making and freeing cost by ring (populate, pin, pagemap; unpin,
  unmap), connection setup and first-message latency for 64 B to 1 MiB on fresh and kept
  connections against plain vsock (the cold start's cost), a bulk stream's ramp (swaps and
  their round trips) against a fixed 2 MiB ring, throughput 256 MiB each way, the device
  worker's CPU, and the run's memory with pins at V = 1, 2, many.
