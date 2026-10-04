//! Torn snapshots of a file rewritten under them (PM M100): a writer rewrites the file,
//! each version one write(2) of one byte value, `PAUSE` µs apart; a reader takes it
//! `TAKES` times as shards' Stage once took files, fstat, take, fstat, where taking is
//! reading it all, or with `clone` on macOS, cloning it (fclonefileat) and reading the
//! clone. Counts the snapshots that mix versions, those the fstat comparison misses, and
//! on macOS those the file's write count (getattrlist ATTR_CMN_GEN_COUNT) misses.
//!
//!   rustc -O --edition 2021 torn.rs && ./torn SIZE TAKES DIR [PAUSE [clone]]
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};

fn key(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec())
}

#[cfg(target_os = "macos")]
fn writes(f: &std::fs::File) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        groups: [u32; 5],
    }
    extern "C" {
        fn fgetattrlist(fd: i32, list: *mut AttrList, buf: *mut u32, size: usize, options: u32) -> i32;
    }
    // ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_GEN_COUNT, with FSOPT_ATTR_CMN_EXTENDED.
    let mut l = AttrList { bitmapcount: 5, reserved: 0, groups: [0x8008_0000, 0, 0, 0, 0] };
    let mut buf = [0u32; 7];
    let rc = unsafe { fgetattrlist(f.as_raw_fd(), &mut l, buf.as_mut_ptr(), 28, 0x20) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    (buf[1] & 0x0008_0000 != 0).then_some(buf[6])
}

#[cfg(not(target_os = "macos"))]
fn writes(_: &std::fs::File) -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
fn clone(f: &std::fs::File, to: &std::path::Path) {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn fclonefileat(src: i32, dir: i32, dst: *const i8, flags: u32) -> i32;
    }
    let dst = std::ffi::CString::new(to.as_os_str().as_bytes()).unwrap();
    // AT_FDCWD: -2 on macOS.
    let rc = unsafe { fclonefileat(f.as_raw_fd(), -2, dst.as_ptr(), 0) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
}

#[cfg(not(target_os = "macos"))]
fn clone(_: &std::fs::File, _: &std::path::Path) {
    unimplemented!("clones are measured on macOS")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let size: usize = args[1].parse().unwrap();
    let takes: usize = args[2].parse().unwrap();
    let path = std::path::Path::new(&args[3]).join(format!("torn-{size}-{}", std::process::id()));
    let pause: u64 = args.get(4).map_or(0, |p| p.parse().unwrap());
    let cloning = args.get(5).is_some_and(|m| m == "clone");
    let copy = path.with_extension("clone");
    std::fs::write(&path, vec![b'a'; size]).unwrap();
    let stop = AtomicBool::new(false);
    let (mut torn, mut missed, mut counted_missed) = (0, 0, 0);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut f = std::fs::File::options().write(true).open(&path).unwrap();
            let (mut v, mut buf) = (0u8, vec![0u8; size]);
            while !stop.load(Ordering::Relaxed) {
                v = v.wrapping_add(1);
                buf.fill(b'a' + v % 26);
                f.seek(SeekFrom::Start(0)).unwrap();
                assert_eq!(f.write(&buf).unwrap(), size);
                if pause > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(pause));
                }
            }
        });
        let mut got = Vec::with_capacity(size + 1);
        for _ in 0..takes {
            let mut f = std::fs::File::open(&path).unwrap();
            let (w0, before) = (writes(&f), key(&f.metadata().unwrap()));
            got.clear();
            if cloning {
                clone(&f, &copy);
                std::fs::File::open(&copy).unwrap().read_to_end(&mut got).unwrap();
                std::fs::remove_file(&copy).unwrap();
            } else {
                (&mut f).take(size as u64 + 1).read_to_end(&mut got).unwrap();
            }
            let (after, w1) = (key(&f.metadata().unwrap()), writes(&f));
            if got.iter().any(|&c| c != got[0]) || got.len() != size {
                torn += 1;
                if before == after {
                    missed += 1;
                    if w0.is_some() && w0 == w1 {
                        counted_missed += 1;
                    }
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
    let counted = if cfg!(target_os = "macos") { format!(", the write count missed {counted_missed}") } else { String::new() };
    let how = if cloning { "cloned" } else { "read" };
    println!("size {size} {how} takes {takes} pause {pause} µs: torn {torn}, the times missed {missed}{counted}");
    let _ = std::fs::remove_file(&path);
}
