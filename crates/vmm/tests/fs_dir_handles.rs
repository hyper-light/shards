//! What a share holds for the directories a guest opens (audit V05), counted by this
//! binary's allocator: a test of its own, so that no other test's allocations are counted.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use shards_vmm::devices::virtio::fs::server::Server;

/// Bytes allocated and not yet freed, by everything in this binary.
static LIVE: AtomicIsize = AtomicIsize::new(0);

struct Counting;

fn size(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

// SAFETY: every call goes to System's, which upholds GlobalAlloc's contract; the count is
// a side effect.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(size(layout.size()), Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            LIVE.fetch_add(size(layout.size()), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(size(layout.size()), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        // SAFETY: forwarded.
        let p = unsafe { System.realloc(ptr, layout, new) };
        if !p.is_null() {
            LIVE.fetch_add(size(new) - size(layout.size()), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

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

const ROOT: u64 = 1;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;

/// A guest that opens a large directory again and again, never releasing it, has the share
/// hold a descriptor a handle, as many as the process may have, and none of the listing:
/// a copy of it each was host memory without bound, a handle's worth for one request.
#[test]
fn directory_handles_hold_no_listing() {
    let dir = std::env::temp_dir().join(format!("shards-dir-handles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..2000 {
        std::fs::write(dir.join(format!("entry-{i:0>14}")), "").unwrap();
    }
    // Room for the 101 handles opened below, whatever this host's budget for a share.
    let root = std::fs::File::open(&dir).unwrap();
    let server = Server::with_budget(root.into(), true, None, 1000).unwrap();
    // The server's own tables grow first, outside what is counted.
    let warm = server.handle(&req(OPENDIR, ROOT, &[0u8; 8])).unwrap();
    assert_eq!(i32::from_le_bytes(warm[4..8].try_into().unwrap()), 0);
    let before = LIVE.load(Ordering::Relaxed);
    let mut handles = Vec::new();
    for _ in 0..100 {
        let out = server.handle(&req(OPENDIR, ROOT, &[0u8; 8])).unwrap();
        assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 0);
        handles.push(u64::from_le_bytes(out[16..24].try_into().unwrap()));
    }
    let held = LIVE.load(Ordering::Relaxed) - before;
    assert!(
        held < 100 * 1024,
        "{held} bytes held for 100 handles of 2000 entries"
    );
    // Each still lists the directory.
    let mut body = handles[99].to_le_bytes().to_vec();
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&4096u32.to_le_bytes());
    body.extend_from_slice(&[0u8; 12]);
    let out = server.handle(&req(READDIR, ROOT, &body)).unwrap();
    assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 0);
    assert!(out.len() > 16 + 1000, "{} bytes listed", out.len());
    let _ = std::fs::remove_dir_all(&dir);
}
