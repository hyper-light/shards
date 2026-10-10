//! Linux: a directory paged through by getdents64(2), seeking each page to the `d_off` of
//! the last entry taken, as `DirStream::read` on Linux does, with buffers from 1 KiB up:
//! every entry comes once. `rustc -O getdents.rs && ./getdents DIR N` makes N files in
//! DIR (an empty directory) and checks.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::collections::BTreeSet;
use std::ffi::CStr;
use std::os::fd::AsRawFd;

const SYS_GETDENTS64: CLong = if cfg!(target_arch = "x86_64") { 217 } else { 61 };
type CLong = i64;

unsafe extern "C" {
    fn syscall(num: CLong, ...) -> CLong;
    fn lseek(fd: i32, offset: i64, whence: i32) -> i64;
}

/// Entries from `offset` on, as many as `take` wants, each with the offset after it.
fn read(fd: i32, offset: u64, buf_len: usize, take: &mut dyn FnMut(&str, u64) -> bool) {
    assert!(unsafe { lseek(fd, offset as i64, 0) } >= 0);
    let mut buf = vec![0u8; buf_len];
    loop {
        let n = unsafe { syscall(SYS_GETDENTS64, fd, buf.as_mut_ptr(), buf.len()) };
        assert!(n >= 0, "getdents64");
        if n == 0 {
            return;
        }
        let mut records = &buf[..n as usize];
        while !records.is_empty() {
            let next = u64::from_ne_bytes(records[8..16].try_into().unwrap());
            let reclen = usize::from(u16::from_ne_bytes(records[16..18].try_into().unwrap()));
            let name = CStr::from_bytes_until_nul(&records[19..reclen]).unwrap();
            if !take(name.to_str().unwrap(), next) {
                return;
            }
            records = &records[reclen..];
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::Path::new(&args[1]);
    let n: usize = args[2].parse().unwrap();
    let mut want: BTreeSet<String> = (0..n).map(|i| format!("entry-{i:0>40}")).collect();
    for name in &want {
        std::fs::write(dir.join(name), "").unwrap();
    }
    want.extend([".".to_string(), "..".to_string()]);
    let file = std::fs::File::open(dir).unwrap();
    for (page, buf_len) in [(3, 1024), (17, 4096), (200, 32 << 10)] {
        let mut seen = Vec::new();
        let mut offset = 0;
        loop {
            let mut taken = Vec::new();
            read(file.as_raw_fd(), offset, buf_len, &mut |name, next| {
                if taken.len() == page {
                    return false;
                }
                taken.push((name.to_string(), next));
                true
            });
            let Some(&(_, last)) = taken.last() else { break };
            offset = last;
            seen.extend(taken.into_iter().map(|(name, _)| name));
        }
        let unique: BTreeSet<String> = seen.iter().cloned().collect();
        assert_eq!(unique.len(), seen.len(), "an entry came twice");
        assert_eq!(unique, want, "pages of {page}, buffer {buf_len}");
        println!(
            "pages of {page} entries, buffer {buf_len}: {} entries, each once",
            seen.len()
        );
    }
}
