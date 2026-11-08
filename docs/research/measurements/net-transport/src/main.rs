//! Frames between two processes, as a VM process and its network process would move them
//! (architecture.md D31, networking.md E2): over a Unix datagram socket pair (the send
//! retried on ENOBUFS, which macOS returns instead of blocking and does not signal
//! through poll), a Unix stream socket pair with a length before each frame, and a
//! shared-memory ring of slots with a pipe to wake a sleeping side. Throughput one way
//! for frames of 1514, 9014 and 65561 bytes, and the round trip of a 64-byte frame.
//!
//!     cargo run --release -- [seconds per case]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SIZES: [usize; 3] = [1514, 9014, 65561];

fn die(what: &str) -> ! {
    eprintln!("{what}: {}", std::io::Error::last_os_error());
    std::process::exit(1)
}

fn pair(kind: i32, frame: usize) -> (i32, i32) {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } != 0 {
        die("socketpair");
    }
    for fd in fds {
        let snd = frame as libc::c_int;
        let rcv = (4 * frame).max(1 << 18) as libc::c_int;
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, (&raw const snd).cast(), 4);
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&raw const rcv).cast(), 4);
        }
    }
    (fds[0], fds[1])
}

fn write_all(fd: i32, mut b: &[u8]) {
    while !b.is_empty() {
        let n = unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            die("write");
        }
        b = &b[n as usize..];
    }
}

fn read_exact(fd: i32, mut b: &mut [u8]) -> bool {
    while !b.is_empty() {
        let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), b.len()) };
        if n == 0 {
            return false;
        }
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            die("read");
        }
        b = &mut b[n as usize..];
    }
    true
}

/// A transport's two ends, as a sender and a receiver of frames.
trait End {
    fn send(&mut self, frame: &[u8]);
    /// The next frame's length, its bytes in `buf`; 0 at the end.
    fn recv(&mut self, buf: &mut [u8]) -> usize;
}

struct Dgram(i32, u64);
impl End for Dgram {
    fn send(&mut self, frame: &[u8]) {
        loop {
            let n = unsafe { libc::send(self.0, frame.as_ptr().cast(), frame.len(), 0) };
            if n >= 0 {
                return;
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOBUFS) | Some(libc::EAGAIN) => {
                    self.1 += 1;
                    unsafe { libc::sched_yield() };
                }
                Some(libc::EINTR) => {}
                _ => die("send"),
            }
        }
    }
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let n = unsafe { libc::recv(self.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            die("recv");
        }
        n as usize
    }
}

struct Stream(i32);
impl End for Stream {
    fn send(&mut self, frame: &[u8]) {
        let len = (frame.len() as u32).to_le_bytes();
        let iov = [
            libc::iovec { iov_base: len.as_ptr() as *mut _, iov_len: 4 },
            libc::iovec { iov_base: frame.as_ptr() as *mut _, iov_len: frame.len() },
        ];
        let n = unsafe { libc::writev(self.0, iov.as_ptr(), 2) };
        if n < 0 {
            die("writev");
        }
        let n = n as usize;
        if n < 4 + frame.len() {
            // A partial write: finish it.
            let mut all = len.to_vec();
            all.extend_from_slice(frame);
            write_all(self.0, &all[n..]);
        }
    }
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let mut len = [0u8; 4];
        if !read_exact(self.0, &mut len) {
            return 0;
        }
        let n = u32::from_le_bytes(len) as usize;
        read_exact(self.0, &mut buf[..n]);
        n
    }
}

/// A one-way ring of `SLOTS` slots of `SLOT` bytes in memory both processes map, with a
/// pipe the consumer sleeps on once it has spun, and the producer writes only then.
const SLOTS: u64 = 64;
const SLOT: usize = 65561 + 8;
#[repr(C)]
struct Ring {
    head: AtomicU64,
    tail: AtomicU64,
    sleeping: AtomicU32,
}
struct Shm {
    ring: *mut Ring,
    data: *mut u8,
    wake_w: i32,
    wake_r: i32,
}
fn shm() -> (*mut Ring, *mut u8) {
    let len = 4096 + SLOTS as usize * SLOT;
    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANON, -1, 0)
    };
    if p == libc::MAP_FAILED {
        die("mmap");
    }
    (p.cast(), unsafe { p.cast::<u8>().add(4096) })
}
impl End for Shm {
    fn send(&mut self, frame: &[u8]) {
        let r = unsafe { &*self.ring };
        let head = r.head.load(Ordering::Relaxed);
        while head - r.tail.load(Ordering::Acquire) == SLOTS {
            std::hint::spin_loop();
        }
        let slot = unsafe { self.data.add((head % SLOTS) as usize * SLOT) };
        unsafe {
            slot.cast::<u64>().write_unaligned(frame.len() as u64);
            std::ptr::copy_nonoverlapping(frame.as_ptr(), slot.add(8), frame.len());
        }
        r.head.store(head + 1, Ordering::Release);
        if r.sleeping.load(Ordering::SeqCst) == 1 {
            r.sleeping.store(0, Ordering::SeqCst);
            write_all(self.wake_w, &[1]);
        }
    }
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let r = unsafe { &*self.ring };
        let tail = r.tail.load(Ordering::Relaxed);
        let mut spins = 0u32;
        while r.head.load(Ordering::Acquire) == tail {
            spins += 1;
            if spins > 2000 {
                r.sleeping.store(1, Ordering::SeqCst);
                if r.head.load(Ordering::SeqCst) == tail {
                    let mut b = [0u8; 1];
                    read_exact(self.wake_r, &mut b);
                }
                r.sleeping.store(0, Ordering::SeqCst);
                spins = 0;
            } else {
                std::hint::spin_loop();
            }
        }
        let slot = unsafe { self.data.add((tail % SLOTS) as usize * SLOT) };
        let n = unsafe { slot.cast::<u64>().read_unaligned() } as usize;
        unsafe { std::ptr::copy_nonoverlapping(slot.add(8), buf.as_mut_ptr(), n) };
        r.tail.store(tail + 1, Ordering::Release);
        n
    }
}

fn fork() -> bool {
    match unsafe { libc::fork() } {
        -1 => die("fork"),
        0 => true,
        _ => false,
    }
}

fn wait() {
    let mut st = 0;
    unsafe { libc::wait(&mut st) };
}

/// Frames of `size` one way for `secs`; Gbit/s, frames/s.
fn throughput(make: &dyn Fn() -> (Box<dyn End>, Box<dyn End>), size: usize, secs: f64) -> (f64, f64) {
    let (mut tx, mut rx) = make();
    // The count, written by the receiver at its end.
    let (count_r, count_w) = {
        let mut p = [0; 2];
        unsafe { libc::pipe(p.as_mut_ptr()) };
        (p[0], p[1])
    };
    if fork() {
        let mut buf = vec![0u8; SLOT];
        let mut frames = 0u64;
        loop {
            let n = rx.recv(&mut buf);
            if n == 1 {
                break;
            }
            frames += 1;
        }
        write_all(count_w, &frames.to_le_bytes());
        unsafe { libc::_exit(0) };
    }
    let frame = vec![7u8; size];
    let start = Instant::now();
    let until = start + Duration::from_secs_f64(secs);
    let mut sent = 0u64;
    while Instant::now() < until {
        for _ in 0..64 {
            tx.send(&frame);
        }
        sent += 64;
    }
    tx.send(&[0]);
    let mut c = [0u8; 8];
    read_exact(count_r, &mut c);
    let el = start.elapsed().as_secs_f64();
    wait();
    let got = u64::from_le_bytes(c);
    assert_eq!(got, sent);
    (got as f64 * size as f64 * 8.0 / el / 1e9, got as f64 / el)
}

/// Round trips of a 64-byte frame; p50, p99, max in µs.
fn rtt(make: &dyn Fn() -> ((Box<dyn End>, Box<dyn End>), (Box<dyn End>, Box<dyn End>)), n: usize) -> (f64, f64, f64) {
    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = make();
    if fork() {
        let mut buf = vec![0u8; SLOT];
        loop {
            let len = b_rx.recv(&mut buf);
            b_tx.send(&buf[..len]);
            if len == 1 {
                break;
            }
        }
        unsafe { libc::_exit(0) };
    }
    let mut buf = vec![0u8; SLOT];
    let frame = [9u8; 64];
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        a_tx.send(&frame);
        a_rx.recv(&mut buf);
        v.push(t.elapsed().as_secs_f64() * 1e6);
    }
    a_tx.send(&[0]);
    a_rx.recv(&mut buf);
    wait();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[n / 2], v[n * 99 / 100], v[n - 1])
}

fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let dgram = || {
        let (a, b) = pair(libc::SOCK_DGRAM, 65561);
        (Box::new(Dgram(a, 0)) as Box<dyn End>, Box::new(Dgram(b, 0)) as Box<dyn End>)
    };
    let stream = || {
        let (a, b) = pair(libc::SOCK_STREAM, 65561);
        (Box::new(Stream(a)) as Box<dyn End>, Box::new(Stream(b)) as Box<dyn End>)
    };
    let shm1 = || {
        let (ring, data) = shm();
        let mut p = [0; 2];
        unsafe { libc::pipe(p.as_mut_ptr()) };
        let tx = Shm { ring, data, wake_w: p[1], wake_r: p[0] };
        let rx = Shm { ring, data, wake_w: p[1], wake_r: p[0] };
        (Box::new(tx) as Box<dyn End>, Box::new(rx) as Box<dyn End>)
    };
    println!("load {:.1}", { let mut l = [0f64; 3]; unsafe { libc::getloadavg(l.as_mut_ptr(), 3) }; l[0] });
    for (name, make) in [("datagram", &dgram as &dyn Fn() -> _), ("stream", &stream), ("shm ring", &shm1)] {
        for size in SIZES {
            let (gbps, fps) = throughput(make, size, secs);
            println!("{name:9} {size:6} B: {gbps:6.1} Gbit/s {:8.0} frames/s", fps);
        }
    }
    let two = |m: &dyn Fn() -> (Box<dyn End>, Box<dyn End>)| (m(), m());
    for (name, make) in [("datagram", &dgram as &dyn Fn() -> _), ("stream", &stream), ("shm ring", &shm1)] {
        // Each direction its own pair: a's frames to b, b's back to a.
        let ends = || {
            let (a_tx, b_rx) = make();
            let (b_tx, a_rx) = make();
            ((a_tx, a_rx), (b_tx, b_rx))
        };
        let _ = two;
        let (p50, p99, max) = rtt(&ends, 20000);
        println!("{name:9} rtt 64 B: p50 {p50:.1} p99 {p99:.1} max {max:.1} us (n 20000)");
    }
}
