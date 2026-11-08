//! Read-only product audit probes; each experiment owns and removes a private store.
use shards_image::erofs::{Kind, Tree};
use shards_image::reference::Digest;
use shards_image::store::Store;
use shards_registry::auth::Credentials;
use shards_registry::http::{Cancel, Client};
use shards_registry::registry::Registry;
use shards_image::oci::Descriptor;
use shards_image::reference::Reference;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

const GOOD_DIGEST: &str = "sha256:278f14e96cc67489e5c0d6cebec8a2718fb158ec656fd41fed7ecd031cd472b2";

struct Scratch(PathBuf);
impl Scratch {
    fn new(case: &str, sample: usize) -> Scratch {
        let path = std::env::temp_dir().join(format!("shards-audit-registry-{}-{case}-{sample}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Scratch(path)
    }
    fn store(&self) -> Store { Store::open(&self.0).unwrap() }
}
impl Drop for Scratch {
    fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); }
}

fn partial(root: &Scratch, digest: &Digest) -> PathBuf {
    root.0.join("ingest").join(format!("{}-{}.partial", digest.algorithm().name(), digest.hex()))
}

/// Observe that the waiter has opened the locked inode before the owner removes it.
/// The process is this probe alone; fstat neither closes nor mutates its descriptors.
fn open_copies(file: &fs::Metadata) -> usize {
    let mut count = 0;
    for fd in 0..1024 {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes a valid stat only on success; invalid FD numbers fail.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0 {
            // SAFETY: successful fstat initialized stat.
            let stat = unsafe { stat.assume_init() };
            if stat.st_ino as u64 == file.ino() && stat.st_dev as u64 == file.dev() {
                count += 1;
            }
        }
    }
    count
}

fn wait_for_waiter(meta: &fs::Metadata) {
    let until = Instant::now() + Duration::from_secs(3);
    while open_copies(meta) < 2 {
        assert!(Instant::now() < until, "waiter did not open old inode");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn store_corruption() {
    let root = Scratch::new("corruption", 0);
    let store = root.store();
    let digest = Digest::parse(GOOD_DIGEST).unwrap();
    let mut first = store.download(&digest, 4).unwrap().unwrap();
    first.write(b"BADX").unwrap();
    let old = fs::metadata(partial(&root, &digest)).unwrap();
    let second_root = root.0.clone();
    let second_digest = digest.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let store = Store::open(&second_root).unwrap();
        tx.send(store.download(&second_digest, 4).unwrap().unwrap()).unwrap();
    });
    wait_for_waiter(&old);
    let mismatch = first.commit().unwrap_err().to_string();
    let mut second = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    waiter.join().unwrap();
    assert!(!partial(&root, &digest).exists());
    let mut third = store.download(&digest, 4).unwrap().unwrap();
    third.write(b"GO").unwrap();
    // Restart models a server ignoring the old waiter's resumed Range, then serving GOOD.
    second.restart().unwrap();
    second.write(b"GOOD").unwrap();
    let newer = fs::metadata(partial(&root, &digest)).unwrap();
    let path = second.commit().unwrap();
    let published = fs::metadata(&path).unwrap();
    let initial = fs::read(&path).unwrap();
    // The third writer still owns the inode that was put under the verified name.
    third.write(b"ZZ").unwrap();
    // BufWriter buffers these bytes; dropping it flushes them to the published inode.
    drop(third);
    let later = fs::read(&path).unwrap();
    println!("{{\"case\":\"store-corruption\",\"owner_error\":{:?},\"old_inode\":{},\"replacement_inode\":{},\"published_inode\":{},\"accepted_bytes\":{:?},\"bytes_after_other_writer_drop\":{:?},\"expected_bytes\":\"GOOD\",\"store_has\":{},\"verified_read_rejects\":{}}}", mismatch, old.ino(), newer.ino(), published.ino(), String::from_utf8(initial).unwrap(), String::from_utf8(later).unwrap(), store.has(&digest), store.read(&digest, 4).is_err());
}

struct Together { bytes: Arc<Vec<u8>>, at: usize, end: Arc<Barrier>, waited: bool }
impl Read for Together {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.bytes.len() {
            if !self.waited { self.waited = true; self.end.wait(); }
            return Ok(0);
        }
        let n = out.len().min(self.bytes.len() - self.at);
        out[..n].copy_from_slice(&self.bytes[self.at..self.at+n]);
        self.at += n;
        Ok(n)
    }
}

fn store_publication(n: usize, digest_text: &str) {
    let bytes = Arc::new(vec![b'x'; 8 << 20]);
    let digest = Digest::parse(digest_text).unwrap();
    for sample in 0..n {
        let root = Scratch::new("publication", sample);
        let barrier = Arc::new(Barrier::new(2));
        let start = Instant::now();
        let values = std::thread::scope(|scope| {
            let work = || {
                let store = root.store();
                let mut source = Together { bytes: bytes.clone(), at: 0, end: barrier.clone(), waited: false };
                let path = store.ingest(&digest, bytes.len() as u64, &mut source).unwrap();
                let pinned = File::open(&path).unwrap();
                let ino = pinned.metadata().unwrap().ino();
                (ino, pinned)
            };
            let one = scope.spawn(work);
            let two = scope.spawn(work);
            [one.join().unwrap(), two.join().unwrap()]
        });
        let elapsed = start.elapsed().as_nanos();
        let final_inode = fs::metadata(root.store().blob_path(&digest)).unwrap().ino();
        println!("{{\"case\":\"store-publication\",\"sample\":{},\"size\":{},\"inode1\":{},\"inode2\":{},\"final_inode\":{},\"replaced_after_success\":{},\"elapsed_ns\":{}}}", sample, bytes.len(), values[0].0, values[1].0, final_inode, values[0].0 != final_inode || values[1].0 != final_inode, elapsed);
        drop(values);
    }
}

fn cancel_waiter() {
    let root = Scratch::new("cancel", 0);
    let store = root.store();
    let digest = Digest::parse(GOOD_DIGEST).unwrap();
    let owner = store.download(&digest, 4).unwrap().unwrap();
    let old = fs::metadata(partial(&root, &digest)).unwrap();
    let root2 = root.0.clone();
    let cancel = Cancel::new();
    let token = cancel.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let store = Store::open(&root2).unwrap();
        let reference = Reference::parse("127.0.0.1:9/a:b").unwrap();
        // No network I/O occurs before the held store lock is released. Cancellation
        // then prevents sending, so this unused TLS factory is never called.
        let http = Client::new(Box::new(|_| panic!("unused TLS")), "shards-audit").cancelled_by(token);
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let desc = Descriptor { media_type: "application/octet-stream".into(), digest: GOOD_DIGEST.into(), size: 4, platform: None };
        tx.send(registry.fetch_blob(&store, &desc, &|_| {}).unwrap_err().kind()).unwrap();
    });
    wait_for_waiter(&old);
    let start = Instant::now();
    cancel.cancel();
    let waited = rx.recv_timeout(Duration::from_millis(300));
    let blocked = matches!(waited, Err(mpsc::RecvTimeoutError::Timeout));
    let before_release_ns = start.elapsed().as_nanos();
    drop(owner);
    let result = match waited { Ok(result) => result, Err(_) => rx.recv_timeout(Duration::from_secs(3)).unwrap() };
    let after_release_ns = start.elapsed().as_nanos();
    waiter.join().unwrap();
    println!("{{\"case\":\"cancel-waiter\",\"blocked_after_cancel\":{},\"held_lock_ns\":{},\"completion_ns\":{},\"error_kind\":{:?}}}", blocked, before_release_ns, after_release_ns, format!("{result:?}"));
}

fn member(out: &mut Vec<u8>, name: &[u8], flag: u8, link: &[u8], data: &[u8]) {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name);
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].fill(b' ');
    header[156] = flag;
    header[157..157+link.len()].copy_from_slice(link);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    header[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(512), 0);
}

fn pax(out: &mut Vec<u8>, key: &str, value: &str) {
    let tail = format!(" {key}={value}\n");
    let mut len = tail.len() + 1;
    while len.to_string().len() + tail.len() != len { len = len.to_string().len() + tail.len(); }
    member(out, b"pax", b'x', b"", format!("{len}{tail}").as_bytes());
}

fn filesystem() {
    let mut archive = Vec::new();
    member(&mut archive, b"alias", b'2', b".wh.secret", b"");
    member(&mut archive, b"alias/data", b'0', b"", b"secret");
    archive.resize(archive.len() + 1024, 0);
    let mut tree = shards_image::layer::root();
    let result = shards_image::layer::apply(&mut tree, 0, archive.as_slice());
    let forbidden = tree.child(Tree::ROOT, b".wh.secret");
    let data_present = forbidden.and_then(|id| tree.child(id, b"data")).is_some();
    println!("{{\"case\":\"whiteout-symlink\",\"accepted\":{},\"forbidden_name_created\":{},\"payload_created\":{}}}", result.is_ok(), forbidden.is_some(), data_present);

    let mut lower = Vec::new();
    pax(&mut lower, "SCHILY.xattr.user.note", "retained");
    member(&mut lower, b"file", b'0', b"", b"data");
    lower.resize(lower.len() + 1024, 0);
    let mut upper = Vec::new();
    member(&mut upper, b"link", b'1', b"file", b"");
    upper.resize(upper.len() + 1024, 0);
    let mut tree = shards_image::layer::root();
    shards_image::layer::apply(&mut tree, 0, lower.as_slice()).unwrap();
    let file = tree.child(Tree::ROOT, b"file").unwrap();
    let before = tree.node(file).unwrap().meta.xattrs.len();
    shards_image::layer::apply(&mut tree, 1, upper.as_slice()).unwrap();
    let after = tree.node(file).unwrap().meta.xattrs.len();
    assert!(matches!(tree.node(file).unwrap().kind, Kind::File { .. }));
    println!("{{\"case\":\"hardlink-xattrs\",\"same_inode\":{},\"xattrs_before\":{},\"xattrs_after\":{}}}", tree.child(Tree::ROOT, b"link") == Some(file), before, after);

    let root = Scratch::new("empty", 0);
    let empty = root.store().rootfs(&[], 1 << 20).unwrap_err().to_string();
    let digest = GOOD_DIGEST;
    for version in [0, 1, 2, 3] {
        let json = format!("{{\"schemaVersion\":{version},\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"config\":{{\"mediaType\":\"application/vnd.oci.image.config.v1+json\",\"digest\":\"{digest}\",\"size\":4}},\"layers\":[]}}");
        let accepted = shards_image::oci::parse_document(json.as_bytes(), "application/vnd.oci.image.manifest.v1+json").is_ok();
        println!("{{\"case\":\"manifest-schema\",\"schema_version\":{version},\"accepted\":{accepted}}}");
    }
    println!("{{\"case\":\"empty-rootfs\",\"error\":{empty:?}}}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "store-corruption" => store_corruption(),
        "store-publication" => store_publication(args[2].parse().unwrap(), &args[3]),
        "cancel-waiter" => cancel_waiter(),
        "filesystem" => filesystem(),
        _ => panic!("unknown probe"),
    }
}
