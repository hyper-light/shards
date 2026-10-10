//! Samples of virtio-fs's request paths, for audit V's A/B (`run.py`), through APIs every
//! revision under comparison has. Prints `{"case": .., "ns": [..]}`.
//!
//! - `--case open`: a share's OPEN then RELEASE of one file (`Server::handle`), each pair
//!   timed.
//! - `--case serve`: a GETATTR through the device: the driver publishes it and notifies,
//!   and spins on the used index; the share, on a thread, answers at once.
//! - `--case held`: the share takes `--delay-us` to answer; once it has the request, how
//!   long another thread waits for guest memory (`GuestMemory::access`).
//! - `--case list` and `listplus`: whole listings of a directory of `--entries` files,
//!   OPENDIR, READDIR (READDIRPLUS) pages of 4096 bytes, RELEASEDIR.
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_vmm::devices::Interrupt;
use shards_vmm::devices::virtio::fs::{Fs, Share, read_frame, server::Server, write_frame};
use shards_vmm::devices::virtio::queue::{Queue, QueueConfig};
use shards_vmm::devices::virtio::{Activation, DeviceInterrupt, VirtioDevice, feature};
use shards_vmm::memory::GuestMemory;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// A FUSE request: its 40-byte header, then `body`.
fn req(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend_from_slice(&u32::try_from(40 + body.len()).unwrap().to_le_bytes());
    r.extend_from_slice(&opcode.to_le_bytes());
    r.extend_from_slice(&7u64.to_le_bytes());
    r.extend_from_slice(&nodeid.to_le_bytes());
    r.extend_from_slice(&[0u8; 16]);
    r.extend_from_slice(body);
    r
}

fn word(reply: &[u8]) -> u64 {
    assert_eq!(
        i32::from_le_bytes(reply[4..8].try_into().unwrap()),
        0,
        "{reply:?}"
    );
    u64::from_le_bytes(reply[16..24].try_into().unwrap())
}

fn open(warm: usize, n: usize) -> Vec<u128> {
    let dir = std::env::temp_dir().join(format!("virtio-fs-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f"), b"file").unwrap();
    let server = Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
    let node = word(&server.handle(&req(1, 1, b"f\0")).unwrap());
    let mut ns = Vec::with_capacity(n);
    for i in 0..warm + n {
        let t0 = Instant::now();
        let fh = word(&server.handle(&req(14, node, &[0u8; 8])).unwrap());
        let mut release = fh.to_le_bytes().to_vec();
        release.extend_from_slice(&[0u8; 16]);
        server.handle(&req(18, node, &release)).unwrap();
        if i >= warm {
            ns.push(t0.elapsed().as_nanos());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    ns
}

/// A share's LOOKUP then GETATTR of one file, each pair timed: the metadata a guest's
/// `stat` of a file it has not cached asks for.
fn getattr(warm: usize, n: usize) -> Vec<u128> {
    let dir = std::env::temp_dir().join(format!("virtio-fs-audit-attr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f"), b"file").unwrap();
    let server = Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
    let mut ns = Vec::with_capacity(n);
    for i in 0..warm + n {
        let t0 = Instant::now();
        let node = word(&server.handle(&req(1, 1, b"f\0")).unwrap());
        let out = server.handle(&req(3, node, &[0u8; 16])).unwrap();
        assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 0);
        if i >= warm {
            ns.push(t0.elapsed().as_nanos());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    ns
}

/// Whole listings of a directory of `entries` files: OPENDIR, READDIR (or READDIRPLUS) a
/// page of 4096 bytes at a time from the offset the last page ended at, RELEASEDIR.
fn list(warm: usize, n: usize, entries: usize, plus: bool) -> Vec<u128> {
    let dir = std::env::temp_dir().join(format!("virtio-fs-audit-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..entries {
        std::fs::write(dir.join(format!("entry-{i:0>12}")), b"").unwrap();
    }
    let server = Server::new(std::fs::File::open(&dir).unwrap().into(), true, None).unwrap();
    let (opcode, header) = if plus { (44, 128) } else { (28, 0) };
    let mut ns = Vec::with_capacity(n);
    for i in 0..warm + n {
        let t0 = Instant::now();
        let fh = word(&server.handle(&req(27, 1, &[0u8; 8])).unwrap());
        let (mut offset, mut listed) = (0u64, 0);
        loop {
            let mut body = fh.to_le_bytes().to_vec();
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(&4096u32.to_le_bytes());
            body.extend_from_slice(&[0u8; 12]);
            let out = server.handle(&req(opcode, 1, &body)).unwrap();
            let mut rest = &out[16..];
            if rest.is_empty() {
                break;
            }
            while !rest.is_empty() {
                let dirent = &rest[header..];
                offset = u64::from_le_bytes(dirent[8..16].try_into().unwrap());
                let len = u32::from_le_bytes(dirent[16..20].try_into().unwrap()) as usize;
                rest = &rest[(header + 24 + len + 7) & !7..];
                listed += 1;
            }
        }
        assert_eq!(listed, entries + 2);
        let mut release = fh.to_le_bytes().to_vec();
        release.extend_from_slice(&[0u8; 16]);
        server.handle(&req(29, 1, &release)).unwrap();
        if i >= warm {
            ns.push(t0.elapsed().as_nanos());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    ns
}

struct Line;
impl Interrupt for Line {
    fn set_level(&self, _: bool) {}
}

const BASE: u64 = 0x8000_0000;
const SIZE: u16 = 16;
/// Each queue's rings: descriptors, available, used.
const fn ring(q: u64) -> (u64, u64, u64) {
    let at = BASE + 0x4000 * q;
    (at, at + 0x1000, at + 0x2000)
}
const REQUEST: u64 = BASE + 0x10000;
const REPLY: u64 = BASE + 0x11000;

/// A device on guest memory, its share answering on a thread after `delay` (once it has
/// told `asked`), and the guest's GETATTR at `REQUEST`.
fn device(delay: Duration, asked: mpsc::Sender<()>) -> (Arc<GuestMemory>, Fs) {
    let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
    let request = req(3, 1, &[0u8; 16]);
    mem.access().unwrap().write(REQUEST, &request).unwrap();
    // Descriptor 0, the request, then descriptor 1, the reply's buffer.
    let (desc, _, _) = ring(1);
    let mut table = Vec::new();
    for (addr, len, flags, next) in [(REQUEST, 56u32, 1u16, 1u16), (REPLY, 4096, 2, 0)] {
        table.extend_from_slice(&addr.to_le_bytes());
        table.extend_from_slice(&len.to_le_bytes());
        table.extend_from_slice(&flags.to_le_bytes());
        table.extend_from_slice(&next.to_le_bytes());
    }
    mem.access().unwrap().write(desc, &table).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let slot = Share::default();
    slot.attach(ours);
    std::thread::spawn(move || {
        let mut conn = theirs;
        while let Ok(Some(r)) = read_frame(&mut conn) {
            let _ = asked.send(());
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let mut out = 16u32.to_le_bytes().to_vec();
            out.extend_from_slice(&0i32.to_le_bytes());
            out.extend_from_slice(&r[8..16]);
            if write_frame(&mut conn, &out).is_err() {
                return;
            }
        }
    });
    let mut fs = Fs::new("audit", slot).unwrap();
    let queues = (0..2)
        .map(|q| {
            let (desc, avail, used) = ring(q);
            let cfg = QueueConfig {
                size: SIZE,
                desc,
                avail,
                used,
                ready: true,
            };
            Queue::new(cfg, 256, &mem, feature::VERSION_1).unwrap()
        })
        .collect();
    fs.activate(Activation {
        memory: mem.clone(),
        queues,
        interrupt: Arc::new(DeviceInterrupt::new(Arc::new(Line))),
        features: feature::VERSION_1,
        restored: false,
    })
    .unwrap();
    (mem, fs)
}

/// Publishes request `i` (head 0) on the request queue and notifies.
fn publish(mem: &GuestMemory, fs: &Fs, i: u16) {
    let (_, avail, _) = ring(1);
    let a = mem.access().unwrap();
    a.write(avail + 4 + 2 * u64::from(i % SIZE), &0u16.to_le_bytes())
        .unwrap();
    a.store_u16(avail + 2, i.wrapping_add(1), std::sync::atomic::Ordering::Release)
        .unwrap();
    drop(a);
    fs.notify(1);
}

/// Waits for the used index to reach `i + 1`, without taking guest memory from the device.
fn wait_used(mem: &GuestMemory, i: u16) {
    let (_, _, used) = ring(1);
    let p = mem.host_ptr(used + 2, 2).unwrap().cast::<u16>();
    // SAFETY: the used index, two aligned bytes of guest RAM the device stores atomically;
    // read as an atomic, as the guest's driver reads it.
    let idx = unsafe { std::sync::atomic::AtomicU16::from_ptr(p) };
    while idx.load(std::sync::atomic::Ordering::Acquire) != i.wrapping_add(1) {
        std::hint::spin_loop();
    }
}

fn serve(warm: usize, n: usize) -> Vec<u128> {
    let (asked, _requests) = mpsc::channel();
    let (mem, fs) = device(Duration::ZERO, asked);
    let mut ns = Vec::with_capacity(n);
    for i in 0..warm + n {
        let i16 = u16::try_from(i % 65536).unwrap();
        let t0 = Instant::now();
        publish(&mem, &fs, i16);
        wait_used(&mem, i16);
        if i >= warm {
            ns.push(t0.elapsed().as_nanos());
        }
    }
    drop(fs);
    ns
}

fn held(n: usize, delay: Duration) -> Vec<u128> {
    let (asked, requests) = mpsc::channel();
    let (mem, fs) = device(delay, asked);
    let mut ns = Vec::with_capacity(n);
    for i in 0..n {
        let i16 = u16::try_from(i).unwrap();
        publish(&mem, &fs, i16);
        requests.recv().unwrap();
        let t0 = Instant::now();
        drop(mem.access().unwrap());
        ns.push(t0.elapsed().as_nanos());
        wait_used(&mem, i16);
    }
    drop(fs);
    ns
}

/// How many of `dirs` directories a guest can look up, none forgotten, under a descriptor
/// limit of `limit`, as a guest's kernel keeps what it has looked up; and the first
/// failure's errno. Prints `{"case": "fds", "found": N, "errno": E}`.
fn fds(dirs: usize, limit: u64) {
    let lim = libc::rlimit {
        rlim_cur: limit,
        rlim_max: libc::RLIM_INFINITY,
    };
    let mut have = lim;
    // SAFETY: getrlimit(2) into a struct on this stack.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut have) }, 0);
    let lim = libc::rlimit {
        rlim_max: have.rlim_max,
        ..lim
    };
    // SAFETY: setrlimit(2) of this process's soft limit, below its hard one.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
    let dir = std::env::temp_dir().join(format!("virtio-fs-audit-fds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for i in 0..dirs {
        std::fs::create_dir_all(dir.join(format!("d{i}"))).unwrap();
    }
    let server = Server::new(std::fs::File::open(&dir).unwrap().into(), true, None).unwrap();
    let (mut found, mut errno) = (0, 0);
    for i in 0..dirs {
        let out = server.handle(&req(1, 1, format!("d{i}\0").as_bytes())).unwrap();
        match i32::from_le_bytes(out[4..8].try_into().unwrap()) {
            0 => found += 1,
            e => {
                errno = -e;
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "{{\"case\": \"fds\", \"dirs\": {dirs}, \"limit\": {limit}, \"found\": {found}, \"errno\": {errno}}}"
    );
}

fn main() {
    let case = arg("--case").unwrap();
    if case == "fds" {
        fds(
            arg("--dirs").map_or(600, |v| v.parse().unwrap()),
            arg("--limit").map_or(256, |v| v.parse().unwrap()),
        );
        return;
    }
    let n: usize = arg("--n").map_or(2000, |v| v.parse().unwrap());
    let ns = match case.as_str() {
        "open" => open(200, n),
        "getattr" => getattr(200, n),
        "serve" => serve(200, n),
        "held" => held(
            n,
            Duration::from_micros(arg("--delay-us").map_or(5000, |v| v.parse().unwrap())),
        ),
        "list" | "listplus" => list(
            5,
            n,
            arg("--entries").map_or(10_000, |v| v.parse().unwrap()),
            case == "listplus",
        ),
        other => panic!("no case {other}"),
    };
    let list: Vec<String> = ns.iter().map(u128::to_string).collect();
    println!("{{\"case\": \"{case}\", \"ns\": [{}]}}", list.join(","));
}
