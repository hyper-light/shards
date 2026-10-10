//! A share serves a guest however many directories it walks and files it holds open, with a
//! process limit on descriptors only a little above what it holds already (audit V09): a
//! test of its own, as it sets its process's limit.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use shards_vmm::devices::virtio::fs::server::Server;

const ROOT: u64 = 1;
const LOOKUP: u32 = 1;
const MKDIR: u32 = 9;
const RENAME: u32 = 12;
const OPEN: u32 = 14;
const READ: u32 = 15;
const RELEASE: u32 = 18;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;

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

fn name(n: &str) -> Vec<u8> {
    let mut v = n.as_bytes().to_vec();
    v.push(0);
    v
}

/// The reply's errno (0 for none) and payload.
fn answer(s: &Server, r: &[u8]) -> (i32, Vec<u8>) {
    let out = s.handle(r).unwrap();
    (
        -i32::from_le_bytes(out[4..8].try_into().unwrap()),
        out[16..].to_vec(),
    )
}

fn node(s: &Server, parent: u64, n: &str) -> u64 {
    let (e, entry) = answer(s, &req(LOOKUP, parent, &name(n)));
    assert_eq!(e, 0, "LOOKUP {n} in {parent}");
    u64::from_le_bytes(entry[0..8].try_into().unwrap())
}

/// Every name in directory `dir`, read a page at a time.
fn list(s: &Server, dir: u64) -> Vec<String> {
    let (e, opened) = answer(s, &req(OPENDIR, dir, &[0u8; 8]));
    assert_eq!(e, 0, "OPENDIR {dir}");
    let fh = u64::from_le_bytes(opened[0..8].try_into().unwrap());
    let (mut names, mut offset) = (Vec::new(), 0u64);
    loop {
        let mut body = fh.to_le_bytes().to_vec();
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(&512u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 12]);
        let (e, mut page) = answer(s, &req(READDIR, dir, &body));
        assert_eq!(e, 0, "READDIR {dir}");
        if page.is_empty() {
            break;
        }
        while !page.is_empty() {
            offset = u64::from_le_bytes(page[8..16].try_into().unwrap());
            let len = u32::from_le_bytes(page[16..20].try_into().unwrap()) as usize;
            names.push(String::from_utf8(page[24..24 + len].to_vec()).unwrap());
            page.drain(..(24 + len + 7) & !7);
        }
    }
    let mut release = fh.to_le_bytes().to_vec();
    release.extend_from_slice(&[0u8; 16]);
    assert_eq!(answer(s, &req(RELEASEDIR, dir, &release)).0, 0);
    names
}

/// Descriptors this process has open.
fn open_now() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count().saturating_sub(1)
}

#[test]
fn a_guest_walks_any_number_of_directories_within_a_few_descriptors() {
    let dir = std::env::temp_dir().join(format!("shards-dir-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for i in 0..8 {
        for j in 0..8 {
            for k in 0..8 {
                let leaf = dir.join(format!("d{i}/d{j}/d{k}"));
                std::fs::create_dir_all(&leaf).unwrap();
                std::fs::write(leaf.join("f"), "f").unwrap();
            }
        }
    }
    let root = std::fs::File::open(&dir).unwrap();
    // Room for what is open now and 24 more: a few of the 584 directories, not all. A share
    // process has its limit before its servers reckon what they may hold.
    let limit = (open_now() + 24) as libc::rlim_t;
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) and setrlimit(2) of this process's own limit, into a local.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        lim.rlim_cur = limit;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
    }
    let server = Server::new(root.into(), false, None).unwrap();
    // A guest walks the tree as `find` does, every directory looked up and listed.
    let mut leaves = 0;
    let mut first = None;
    for i in 0..8 {
        let a = node(&server, ROOT, &format!("d{i}"));
        assert_eq!(list(&server, a).len(), 10);
        for j in 0..8 {
            let b = node(&server, a, &format!("d{j}"));
            assert_eq!(list(&server, b).len(), 10);
            for k in 0..8 {
                let c = node(&server, b, &format!("d{k}"));
                let mut names = list(&server, c);
                names.sort();
                assert_eq!(names, [".", "..", "f"]);
                first.get_or_insert((a, b, c));
                leaves += 1;
            }
        }
    }
    assert_eq!(leaves, 512);
    // Directories looked up long since still serve: renamed between, made in, listed. The
    // first request takes the most a request may past the server's room (`REQUEST_FDS`): a
    // RENAME between two directories both let go, the second's path walked a component at a
    // time while the first is held, with the server at its room.
    let (a, b, c) = first.unwrap();
    // A lookup in a directory let go fills the server's room again, which the walk's last
    // OPENDIR left one short.
    let d6 = node(&server, ROOT, "d6");
    node(&server, d6, "d0");
    let mut rename = b.to_le_bytes().to_vec();
    rename.extend(name("f"));
    rename.extend(name("g"));
    assert_eq!(answer(&server, &req(RENAME, c, &rename)).0, 0);
    let mut back = c.to_le_bytes().to_vec();
    back.extend(name("g"));
    back.extend(name("f"));
    assert_eq!(answer(&server, &req(RENAME, b, &back)).0, 0);
    let mut mkdir = 0o755u32.to_le_bytes().to_vec();
    mkdir.extend_from_slice(&[0u8; 4]);
    mkdir.extend(name("made"));
    assert_eq!(answer(&server, &req(MKDIR, c, &mkdir)).0, 0);
    let mut rename = b.to_le_bytes().to_vec();
    rename.extend(name("made"));
    rename.extend(name("moved"));
    assert_eq!(answer(&server, &req(RENAME, c, &rename)).0, 0);
    assert!(list(&server, b).contains(&"moved".to_string()));
    assert!(list(&server, a).contains(&"d0".to_string()));
    // The guest's open files take no descriptor past the server's room: more of them than
    // the process may have descriptors are opened and each read, a directory opened besides.
    let file = node(&server, c, "f");
    let handles: Vec<u64> = (0..64)
        .map(|_| {
            let (e, opened) = answer(&server, &req(OPEN, file, &[0u8; 8]));
            assert_eq!(e, 0, "OPEN");
            u64::from_le_bytes(opened[0..8].try_into().unwrap())
        })
        .collect();
    for fh in &handles {
        let mut read = fh.to_le_bytes().to_vec();
        read.extend_from_slice(&0u64.to_le_bytes());
        read.extend_from_slice(&8u32.to_le_bytes());
        read.extend_from_slice(&[0u8; 20]);
        assert_eq!(answer(&server, &req(READ, file, &read)), (0, b"f".to_vec()));
    }
    assert_eq!(answer(&server, &req(OPENDIR, b, &[0u8; 8])).0, 0);
    // What takes no handle goes on serving, in directories let go long since.
    let leaf = node(&server, ROOT, "d7");
    assert_eq!(node(&server, b, "moved"), node(&server, b, "moved"));
    let mut again = 0o755u32.to_le_bytes().to_vec();
    again.extend_from_slice(&[0u8; 4]);
    again.extend(name("again"));
    assert_eq!(answer(&server, &req(MKDIR, leaf, &again)).0, 0);
    assert_eq!(answer(&server, &req(MKDIR, a, &again)).0, 0);
    let mut release = handles[0].to_le_bytes().to_vec();
    release.extend_from_slice(&[0u8; 16]);
    assert_eq!(answer(&server, &req(RELEASE, file, &release)).0, 0);
    assert_eq!(list(&server, b).len(), 11);
    assert_eq!(answer(&server, &req(OPEN, file, &[0u8; 8])).0, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
