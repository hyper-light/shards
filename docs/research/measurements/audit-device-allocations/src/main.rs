//! Allocation census and focused timings of unchanged Shards device code.
//! No VM runs: queues use isolated RAM, and poll uses local socket pairs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use shards_vmm::devices::Interrupt;
use shards_vmm::devices::virtio::feature;
use shards_vmm::devices::virtio::queue::{Descriptor, Queue, QueueConfig, QueueState};
use shards_vmm::devices::virtio::vsock::Vsock;
use shards_vmm::devices::virtio::vsock::packet::{HEADER_LEN, Header, TYPE_STREAM, op};
use shards_vmm::devices::virtio::{Activation, DeviceInterrupt, VirtioDevice};
use shards_vmm::memory::GuestMemory;

// This private module is included unchanged, rather than a copied implementation.
#[path = "../../../../../crates/vmm/src/devices/virtio/vsock/poll.rs"]
#[allow(dead_code)]
mod production_poll;

struct Census;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static OLD_REALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: delegates allocation, alignment, reallocation and deallocation to System.
// Atomic counters neither allocate nor call instrumented code recursively.
unsafe impl GlobalAlloc for Census {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the allocator's caller supplied a valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the allocator's caller supplied a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            REALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            OLD_REALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the allocator's caller supplied the original allocation and layout.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNT.load(Ordering::Relaxed) {
            DEALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the allocator's caller supplied the original allocation and layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Census = Census;

fn begin_census() {
    for counter in [
        &ALLOCS,
        &REALLOCS,
        &DEALLOCS,
        &ALLOC_BYTES,
        &REALLOC_BYTES,
        &OLD_REALLOC_BYTES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    COUNT.store(true, Ordering::Relaxed);
}

fn end_census() -> [u64; 6] {
    COUNT.store(false, Ordering::Relaxed);
    [
        ALLOCS.load(Ordering::Relaxed),
        REALLOCS.load(Ordering::Relaxed),
        DEALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
        REALLOC_BYTES.load(Ordering::Relaxed),
        OLD_REALLOC_BYTES.load(Ordering::Relaxed),
    ]
}

fn report(name: &str, dimension: usize, counts: [u64; 6], times: &mut [u128]) {
    if let Some(path) = std::env::var_os("SHARDS_AUDIT_DEVICE_SAMPLES") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut out = std::io::BufWriter::new(file);
        write!(
            out,
            "{{\"case\":\"{name}\",\"dimension\":{dimension},\"samples_ns\":["
        )
        .unwrap();
        for (i, sample) in times.iter().enumerate() {
            if i > 0 {
                write!(out, ",").unwrap();
            }
            write!(out, "{sample}").unwrap();
        }
        writeln!(out, "]}}").unwrap();
        out.flush().unwrap();
    }
    times.sort_unstable();
    let n = times.len();
    let percentile = |percent: usize| times[(n * percent).div_ceil(100).saturating_sub(1)];
    println!(
        "{{\"case\":\"{name}\",\"dimension\":{dimension},\"n\":{n},\"allocs\":{},\"reallocs\":{},\"deallocs\":{},\"allocation_requested_bytes\":{},\"reallocation_requested_bytes\":{},\"old_reallocation_bytes\":{},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
        counts[5],
        percentile(50),
        percentile(90),
        percentile(99),
        times[n - 1]
    );
}

const BASE: u64 = 0x8000_0000;

fn raw_descriptor(mem: &GuestMemory, at: u64, addr: u64, len: u32, flags: u16, next: u16) {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&addr.to_le_bytes());
    bytes[8..12].copy_from_slice(&len.to_le_bytes());
    bytes[12..14].copy_from_slice(&flags.to_le_bytes());
    bytes[14..16].copy_from_slice(&next.to_le_bytes());
    mem.write(at, &bytes).unwrap();
}

fn queue_case(descriptors: usize, indirect: bool) {
    let mem = GuestMemory::anonymous(&[(BASE, 4 * 1024 * 1024)]).unwrap();
    let size = if indirect {
        256
    } else {
        descriptors.next_power_of_two() as u16
    };
    let cfg = QueueConfig {
        size,
        desc: BASE + 0x1000,
        avail: BASE + 0x20000,
        used: BASE + 0x30000,
        ready: true,
    };
    let table = if indirect { BASE + 0x80000 } else { cfg.desc };
    for i in 0..descriptors {
        raw_descriptor(
            &mem,
            table + (16 * i) as u64,
            BASE + 0x40000,
            1,
            if i + 1 < descriptors { 1 } else { 0 },
            (i + 1) as u16,
        );
    }
    if indirect {
        raw_descriptor(&mem, cfg.desc, table, (descriptors * 16) as u32, 4, 0);
    }
    mem.write_obj(cfg.avail + 2, 1u16).unwrap();
    mem.write_obj(cfg.avail + 4, 0u16).unwrap();
    let mut queue = Queue::new(cfg, size, &mem, feature::INDIRECT_DESC).unwrap();
    let mut operation = || {
        queue.set_state(QueueState::default());
        let chain = queue.pop(&mem).unwrap().unwrap();
        assert_eq!(chain.descriptors.len(), descriptors);
        black_box(chain.descriptors.capacity());
        queue.add_used(&mem, chain.head, 0).unwrap();
        drop(chain);
    };
    for _ in 0..500 {
        operation();
    }
    begin_census();
    operation();
    let counts = end_census();
    let mut times = Vec::with_capacity(20_000);
    for _ in 0..20_000 {
        let start = Instant::now();
        operation();
        times.push(start.elapsed().as_nanos());
    }
    report(
        if indirect {
            "queue_indirect_pop_and_complete"
        } else {
            "queue_direct_pop_and_complete"
        },
        descriptors,
        counts,
        &mut times,
    );
}

fn poll_case(sockets: usize, both_filters: bool) {
    let pairs: Vec<_> = (0..sockets).map(|_| UnixStream::pair().unwrap()).collect();
    let interests: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(i, (stream, _))| production_poll::Interest {
            fd: stream.as_raw_fd(),
            read: both_filters,
            write: true,
            token: i,
        })
        .collect();
    let mut ready = Vec::with_capacity(2 * sockets);
    let mut operation = || {
        ready.clear();
        production_poll::wait(&interests, Some(Duration::ZERO), &mut ready).unwrap();
        assert!(ready.iter().all(|r| r.write && r.token < sockets));
        assert_eq!(ready.len(), sockets);
    };
    for _ in 0..100 {
        operation();
    }
    begin_census();
    operation();
    let counts = end_census();
    let mut times = Vec::with_capacity(2_000);
    for _ in 0..2_000 {
        let start = Instant::now();
        operation();
        times.push(start.elapsed().as_nanos());
    }
    report(
        if both_filters {
            "poll_zero_timeout_read_and_write"
        } else {
            "poll_zero_timeout_write"
        },
        sockets,
        counts,
        &mut times,
    );
}

// A source-derived model of TxBuf::append's Vec::resize growth, not the private
// production TxBuf. The live-byte credit bound does not cap Vec::capacity.
#[allow(clippy::slow_vector_initialization)] // The two resize calls are the measured source shape.
fn credit_buffer_growth_shape() {
    begin_census();
    let mut bytes = Vec::<u8>::new();
    bytes.resize(65535, 0);
    let first_capacity = bytes.capacity();
    bytes.resize(65536, 0);
    let final_len = bytes.len();
    let final_capacity = bytes.capacity();
    black_box(&bytes);
    drop(bytes);
    let counts = end_census();
    println!(
        "{{\"case\":\"credit_bounded_vec_growth_shape\",\"n\":1,\"initial_len\":65535,\"initial_capacity\":{first_capacity},\"final_len\":{final_len},\"final_capacity\":{final_capacity},\"allocs\":{},\"reallocs\":{},\"deallocs\":{},\"allocation_requested_bytes\":{},\"reallocation_requested_bytes\":{}}}",
        counts[0], counts[1], counts[2], counts[3], counts[4]
    );
}

struct NoInterrupt;
impl Interrupt for NoInterrupt {
    fn set_level(&self, _: bool) {}
}

fn wait_used(mem: &GuestMemory, used: u64, expected: u16) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while mem.atomic_u16(used + 2).unwrap().load(Ordering::Acquire) < expected {
        assert!(Instant::now() < deadline, "device did not complete request");
        std::thread::sleep(Duration::from_micros(50));
    }
}

// Drives the real public Vsock device with an isolated synthetic driver. This is a
// correctness probe, not a throughput benchmark, and never starts a guest CPU.
fn fragmented_packet(segments: usize) {
    let socket = std::env::temp_dir().join(format!("shards-device-audit-{}-{segments}", std::process::id()));
    let endpoint = socket.with_file_name(format!("{}_5000", socket.file_name().unwrap().to_string_lossy()));
    let listener = UnixListener::bind(&endpoint).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 4 * 1024 * 1024)]).unwrap());
    let configs: Vec<_> = (0..3)
        .map(|q| QueueConfig {
            size: 256,
            desc: BASE + 0x1000 + q * 0x10000,
            avail: BASE + 0x3000 + q * 0x10000,
            used: BASE + 0x4000 + q * 0x10000,
            ready: true,
        })
        .collect();
    let rx = configs[0];
    let tx = configs[1];
    let header_at = BASE + 0x90000;
    let data_at = BASE + 0xa0000;
    let table = BASE + 0x80000;
    let response_at = BASE + 0x180000;
    let mut header = Header {
        src_cid: 3,
        dst_cid: 2,
        src_port: 6000,
        dst_port: 5000,
        len: 0,
        kind: TYPE_STREAM,
        op: op::REQUEST,
        flags: 0,
        buf_alloc: 65536,
        fwd_cnt: 0,
    };
    mem.write(header_at, &header.encode()).unwrap();
    mem.write_obj(data_at, 0x5au8).unwrap();
    raw_descriptor(&mem, tx.desc, header_at, HEADER_LEN as u32, 0, 0);
    raw_descriptor(&mem, rx.desc, response_at, 65536 + HEADER_LEN as u32, 2, 0);
    mem.write_obj(tx.avail + 4, 0u16).unwrap();
    mem.atomic_u16(tx.avail + 2).unwrap().store(1, Ordering::Release);
    mem.write_obj(rx.avail + 4, 0u16).unwrap();
    mem.atomic_u16(rx.avail + 2).unwrap().store(1, Ordering::Release);
    let queues = configs
        .iter()
        .map(|&cfg| Queue::new(cfg, 256, &mem, feature::INDIRECT_DESC).unwrap())
        .collect();
    let mut device = Vsock::new(&socket, 3).unwrap();
    device
        .activate(Activation {
            memory: mem.clone(),
            queues,
            interrupt: Arc::new(DeviceInterrupt::new(Arc::new(NoInterrupt))),
            features: feature::INDIRECT_DESC,
            restored: false,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut host = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "device did not connect");
                std::thread::sleep(Duration::from_micros(50));
            }
            Err(e) => panic!("host accept: {e}"),
        }
    };
    host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    wait_used(&mem, tx.used, 1);
    wait_used(&mem, rx.used, 1);
    let mut received_header = [0u8; HEADER_LEN];
    mem.read(response_at, &mut received_header).unwrap();
    assert_eq!(Header::decode(&received_header).op, op::RESPONSE);

    // Accepted by Shards, but exceeds this 256-entry queue's Virtio chain bound:
    // 44-byte header followed by one-byte payload descriptors. This probes hostile
    // input handling, not a conforming driver's compatibility.
    header.op = op::RW;
    header.len = segments as u32;
    mem.write(header_at, &header.encode()).unwrap();
    raw_descriptor(&mem, table, header_at, HEADER_LEN as u32, 1, 1);
    for i in 0..segments {
        raw_descriptor(
            &mem,
            table + (16 * (i + 1)) as u64,
            data_at,
            1,
            if i + 1 < segments { 1 } else { 0 },
            (i + 2) as u16,
        );
    }
    raw_descriptor(&mem, tx.desc, table, (16 * (segments + 1)) as u32, 4, 0);
    mem.write_obj(rx.avail + 6, 0u16).unwrap();
    mem.atomic_u16(rx.avail + 2).unwrap().store(2, Ordering::Release);
    mem.write_obj(tx.avail + 6, 0u16).unwrap();
    mem.atomic_u16(tx.avail + 2).unwrap().store(2, Ordering::Release);
    device.notify(1);
    wait_used(&mem, tx.used, 2);
    let mut bytes = vec![0u8; segments];
    let mut received = 0;
    while received < segments {
        match host.read(&mut bytes[received..]) {
            Ok(0) => break,
            Ok(n) => received += n,
            Err(e) => panic!("host read: {e}"),
        }
    }
    if received < segments {
        wait_used(&mem, rx.used, 2);
    }
    let rx_used = mem.atomic_u16(rx.used + 2).unwrap().load(Ordering::Acquire);
    mem.read(response_at, &mut received_header).unwrap();
    println!(
        "{{\"case\":\"nonconforming_fragmented_vsock_tx\",\"queue_size\":256,\"segments\":{segments},\"payload_bytes_received\":{received},\"tx_used\":2,\"rx_used\":{rx_used},\"last_rx_op\":{}}}",
        Header::decode(&received_header).op
    );
    device.reset();
    drop(device);
    drop(host);
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&endpoint);
}

fn main() {
    if std::env::args().any(|a| a == "--vsock-fragmentation") {
        // SAFETY: sysconf with a constant selector.
        let limit = unsafe { libc::sysconf(libc::_SC_IOV_MAX) } as usize;
        assert!(limit + 2 <= shards_vmm::devices::virtio::queue::MAX_CHAIN);
        fragmented_packet(limit);
        fragmented_packet(limit + 1);
        return;
    }
    println!(
        "{{\"descriptor_size\":{},\"kevent_or_pollfd_size\":{},\"iov_max\":{}}}",
        std::mem::size_of::<Descriptor>(),
        if cfg!(target_os = "macos") {
            32
        } else {
            std::mem::size_of::<libc::pollfd>()
        },
        // SAFETY: sysconf with a constant selector.
        unsafe { libc::sysconf(libc::_SC_IOV_MAX) }
    );
    for n in [1, 3, 8, 32, 256] {
        queue_case(n, false);
    }
    for n in [3, 256, 4096] {
        queue_case(n, true);
    }
    credit_buffer_growth_shape();
    for n in [1, 2, 16, 64] {
        poll_case(n, false);
        poll_case(n, true);
    }
}
