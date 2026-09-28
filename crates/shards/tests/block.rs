//! virtio-blk end to end: real Linux block layer in the guest, real files on the host.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{kernel, test_guest, vm_run};
use shards_testguest::{fill, first_mismatch};

const TIMEOUT: Duration = Duration::from_secs(120);
const RO_SALT: u64 = 1;
const RW_SALT: u64 = 2;

struct Disks {
    dir: PathBuf,
    ro: PathBuf,
    rw: PathBuf,
}

impl Disks {
    fn new(tag: &str, ro_bytes: usize, rw_bytes: usize) -> Disks {
        let dir = common::workspace().join(format!("target/e2e/{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ro, rw) = (dir.join("ro.img"), dir.join("rw.img"));
        let mut data = vec![0u8; ro_bytes];
        fill(RO_SALT, 0, &mut data);
        std::fs::write(&ro, &data).unwrap();
        std::fs::write(&rw, vec![0u8; rw_bytes]).unwrap();
        Disks { dir, ro, rw }
    }
}

impl Drop for Disks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run_test(disks: &Disks, test: &str, extra: &str) -> common::Run {
    let ro_bytes = std::fs::metadata(&disks.ro).unwrap().len();
    let rw_bytes = std::fs::metadata(&disks.rw).unwrap().len();
    let cmdline = format!(
        "console=ttyS0 quiet panic=-1 shards_test={test} shards_vda_bytes={ro_bytes} shards_vdb_bytes={rw_bytes} {extra}"
    );
    let ro = format!("{}:ro", disks.ro.display());
    let rw = disks.rw.display().to_string();
    let args = [
        "--kernel",
        kernel().to_str().unwrap(),
        "--init",
        test_guest().to_str().unwrap(),
        "--cpus",
        "4",
        "--memory",
        "512",
        "--disk",
        &ro,
        "--disk",
        &rw,
        "--cmdline",
        &cmdline,
    ];
    vm_run(&args, TIMEOUT)
}

#[test]
fn guest_reads_writes_and_flushes_through_virtio_blk() {
    let disks = Disks::new("blk", 64 << 20, 16 << 20);
    let r = run_test(&disks, "blk", "");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert_eq!(r.status, Some(0), "{r}");

    // What the guest flushed is on the host file, byte for byte.
    let written = std::fs::read(&disks.rw).unwrap();
    assert_eq!(
        first_mismatch(RW_SALT, 0, &written),
        None,
        "guest writes did not reach the file"
    );
    // The read-only disk is untouched.
    let ro = std::fs::read(&disks.ro).unwrap();
    assert_eq!(
        first_mismatch(RO_SALT, 0, &ro),
        None,
        "read-only disk was modified"
    );
}

#[test]
fn concurrent_readers_see_consistent_data() {
    let disks = Disks::new("blk-stress", 32 << 20, 1 << 20);
    let r = run_test(&disks, "blk_stress", "shards_seconds=3");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    let ops: u64 = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("SHARDS-TEST INFO ops="))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(ops > 1000, "only {ops} verified reads in 3 s: {r}");
}
