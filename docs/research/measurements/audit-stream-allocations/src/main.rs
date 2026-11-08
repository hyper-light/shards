//! Allocation counts and algorithm probes, not a VM or Firecracker benchmark.
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use shards_abi::run;

const LOG_STDOUT: u8 = 1;
const LOG_STDERR: u8 = 2;

include!(concat!(env!("OUT_DIR"), "/functions.rs"));

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static REQUESTED: AtomicU64 = AtomicU64::new(0);

struct Counted;
#[global_allocator]
static ALLOCATOR: Counted = Counted;

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(size as u64, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ENABLED.load(Relaxed) {
            FREES.fetch_add(1, Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn case(name: &str, n: usize, mut f: impl FnMut() -> usize) {
    ALLOCS.store(0, Relaxed);
    REALLOCS.store(0, Relaxed);
    FREES.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    ENABLED.store(true, Relaxed);
    let result = black_box(f());
    ENABLED.store(false, Relaxed);
    let counts = [
        ALLOCS.load(Relaxed),
        REALLOCS.load(Relaxed),
        FREES.load(Relaxed),
        REQUESTED.load(Relaxed),
    ];
    for _ in 0..10 {
        black_box(f());
    }
    let samples: Vec<u128> = (0..n)
        .map(|_| {
            let start = Instant::now();
            black_box(f());
            start.elapsed().as_nanos()
        })
        .collect();
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let q = |p: usize| sorted[(n * p).div_ceil(100).saturating_sub(1)];
    println!(
        "case,{name},n,{n},allocs,{},reallocs,{},frees,{},requested_bytes,{},result,{result},p50_ns,{},p90_ns,{},p99_ns,{},max_ns,{}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        q(50),
        q(90),
        q(99),
        sorted[n - 1]
    );
    print!("samples,{name}");
    for sample in samples {
        print!(",{sample}");
    }
    println!();
}

fn cursor_frames(buf: &mut Vec<u8>, mut f: impl FnMut(u8, &[u8])) -> bool {
    let mut used = 0;
    while let Some(h) = buf.get(used..).and_then(|b| b.first_chunk::<{ run::HEADER }>()) {
        let Some((which, len)) = run::parse_header(*h) else {
            buf.clear();
            return false;
        };
        let end = used + run::HEADER + len as usize;
        let Some(payload) = buf.get(used + run::HEADER..end) else {
            break;
        };
        f(which, payload);
        used = end;
    }
    buf.drain(..used);
    true
}

fn main() {
    let n = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(500);
    assert!(n > 0);
    for slots in [5, 6] {
        case(&format!("guest_poll_{slots}_fds"), n, || {
            let mut fds = Vec::with_capacity(5);
            for fd in 0..slots {
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            black_box(&fds);
            fds.capacity() * std::mem::size_of::<libc::pollfd>()
        });
    }
    let bytes = vec![b'x'; 64 * 1024];
    case("guest_first_output_64k", n, || {
        let mut to_host = Vec::new();
        to_host.extend_from_slice(&run::header(run::kind::STDOUT, bytes.len() as u32));
        to_host.extend_from_slice(&bytes);
        black_box(&to_host);
        to_host.capacity()
    });
    case("host_log_record_64k", n, || {
        let record = log_record(1, &bytes);
        black_box(&record);
        record.capacity()
    });
    case("cli_plain_stdin_copy_64k", n, || {
        let input = black_box(&bytes).to_vec();
        black_box(&input);
        input.capacity()
    });
    // Fits an anonymous Unix socket's send buffer without a concurrent receiver.
    let ipc_bytes = &bytes[..4096];
    for fds in [0, 3] {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let files: Vec<_> = (0..fds)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        let descriptors: Vec<_> = files.iter().map(AsFd::as_fd).collect();
        case(&format!("ipc_send_recv_4k_{fds}_fds"), n, || {
            shards_ipc::send(&sender, 1, ipc_bytes, &descriptors).unwrap();
            let message = shards_ipc::recv(&receiver).unwrap().unwrap();
            assert_eq!(message.payload.len(), ipc_bytes.len());
            assert_eq!(message.fds.len(), fds);
            black_box(&message);
            message.payload.capacity()
        });
    }
    let spec = run::Spec {
        argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo hello".to_vec()],
        env: (0..64)
            .map(|i| format!("VARIABLE_{i:02}={}", "x".repeat(48)).into_bytes())
            .collect(),
        cwd: b"/work".to_vec(),
        user: b"1000:1000".to_vec(),
        hostname: b"audit".to_vec(),
        tty: None,
    };
    let encoded = spec.encode();
    case("spec_encode_64_env", n, || {
        let encoded = spec.encode();
        black_box(&encoded);
        encoded.len()
    });
    case("spec_decode_64_env", n, || {
        let decoded = run::Spec::decode(&encoded).unwrap();
        black_box(&decoded);
        decoded.env.len()
    });
    case("standby_argv_envp_pointers", n, || {
        let mut argv_ptrs: Vec<*const libc::c_char> = spec.argv.iter().map(|a| a.as_ptr().cast()).collect();
        argv_ptrs.push(std::ptr::null());
        let mut envp_ptrs: Vec<*const libc::c_char> = spec.env.iter().map(|e| e.as_ptr().cast()).collect();
        envp_ptrs.push(std::ptr::null());
        black_box((&argv_ptrs, &envp_ptrs));
        (argv_ptrs.capacity() + envp_ptrs.capacity()) * std::mem::size_of::<*const libc::c_char>()
    });
    case("standby_cstrings_64_env", n, || {
        let cstrings = |items: &[Vec<u8>]| -> Option<Vec<std::ffi::CString>> {
            items
                .iter()
                .map(|b| std::ffi::CString::new(b.clone()).ok())
                .collect()
        };
        let argv = cstrings(&spec.argv).unwrap();
        let envp = cstrings(&spec.env).unwrap();
        black_box((&argv, &envp));
        argv.len() + envp.len()
    });
    // Model the actual initial logs -f read; inspect live Vec capacities, not RSS.
    let mut log_payload = vec![b'x'; 1024 * 1024];
    for line in log_payload.as_chunks_mut::<1024>().0 {
        line[1023] = b'\n';
    }
    let log_path = std::env::temp_dir().join(format!("shards-audit-stream-{}-{}", std::process::id(), now()));
    let mut encoded_log = Vec::new();
    for bytes in log_payload.chunks(64 * 1024) {
        encoded_log.extend_from_slice(&log_record(LOG_STDOUT, bytes));
    }
    std::fs::write(&log_path, &encoded_log).unwrap();
    case("log_initial_history_1mib", n, || {
        let mut log = Log::default();
        log.read(&log_path);
        let lines = log.take_lines(false);
        assert_eq!(lines.len(), 1024);
        assert!(log.pending.is_empty());
        let retained = log.pending.capacity()
            + lines.capacity() * std::mem::size_of::<Line>()
            + lines.iter().map(|line| line.bytes.capacity()).sum::<usize>();
        black_box(lines.first().map(|line| (line.stream, line.at)));
        black_box((&log, &lines));
        retained
    });
    std::fs::remove_file(log_path).unwrap();

    let mut frame = run::header(run::kind::SIGNAL, 4).to_vec();
    frame.extend_from_slice(&15_u32.to_be_bytes());
    let count = (64 * 1024) / frame.len();
    let mut input = frame.repeat(count);
    input.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
    for name in ["each_frame_drains_5461_signals", "cursor_once_5461_signals"] {
        // Allocation/copy to prepare input lies outside parser timing. Refill preserves capacity.
        let mut buf = Vec::with_capacity(input.len());
        let mut raw = Vec::with_capacity(n);
        for _ in 0..n {
            buf.clear();
            buf.extend_from_slice(&input);
            let mut seen = 0;
            let start = Instant::now();
            let ok = if name.starts_with("each_frame") {
                each_frame(&mut buf, |_, payload| {
                    seen += 1;
                    black_box(payload);
                })
            } else {
                cursor_frames(&mut buf, |_, payload| {
                    seen += 1;
                    black_box(payload);
                })
            };
            let elapsed = start.elapsed().as_nanos();
            assert!(ok);
            assert_eq!(seen, count);
            assert_eq!(buf, [0xaa, 0xbb, 0xcc]);
            raw.push(elapsed);
        }
        let mut sorted = raw.clone();
        sorted.sort_unstable();
        let q = |p: usize| sorted[(n * p).div_ceil(100).saturating_sub(1)];
        let moved = if name.starts_with("each_frame") {
            frame.len() * count * (count - 1) / 2 + 3 * count
        } else {
            3
        };
        println!(
            "case,{name},n,{n},frames,{count},input_bytes,{},suffix_bytes_moved,{moved},p50_ns,{},p90_ns,{},p99_ns,{},max_ns,{}",
            input.len(),
            q(50),
            q(90),
            q(99),
            sorted[n - 1]
        );
        print!("samples,{name}");
        for sample in raw {
            print!(",{sample}");
        }
        println!();
    }
}
