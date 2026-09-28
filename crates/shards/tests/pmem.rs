//! virtio-pmem with real guests: files mapped read-only into guest memory, sized in 2 MiB
//! steps, their bytes exact and the padding zero, before and after a snapshot restore.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, cannot_snapshot, kernel, run_shards, test_guest, vm_run};

const TIMEOUT: Duration = Duration::from_secs(60);
const MIB: u64 = 1 << 20;

struct Image {
    path: PathBuf,
    len: u64,
    salt: u64,
}

fn image(dir: &Path, name: &str, len: u64, salt: u64) -> Image {
    let mut data = vec![0u8; len as usize];
    shards_testguest::fill(salt, 0, &mut data);
    let path = dir.join(name);
    std::fs::write(&path, &data).unwrap();
    Image { path, len, salt }
}

/// The two images: one needing padding to its 2 MiB step, one exactly on it.
fn images(dir: &Path) -> [Image; 2] {
    [
        image(dir, "a.img", 3 * MIB + 1234, 7),
        image(dir, "b.img", 2 * MIB, 8),
    ]
}

fn args(images: &[Image], extra_cmdline: &str) -> Vec<std::ffi::OsString> {
    let spec: Vec<String> = images.iter().map(|i| format!("{}:{}", i.len, i.salt)).collect();
    let mut a: Vec<std::ffi::OsString> = vec![
        "--kernel".into(),
        kernel().into(),
        "--init".into(),
        test_guest().into(),
        "--memory".into(),
        "256".into(),
        "--cmdline".into(),
        format!(
            "console=ttyS0 quiet shards_test=pmem shards_pmem={}{extra_cmdline}",
            spec.join(",")
        )
        .into(),
    ];
    for i in images {
        a.push("--pmem".into());
        a.push(i.path.clone().into());
    }
    a
}

fn assert_unchanged(images: &[Image]) {
    for i in images {
        let data = std::fs::read(&i.path).unwrap();
        assert_eq!(data.len() as u64, i.len, "{} changed size", i.path.display());
        assert_eq!(
            shards_testguest::first_mismatch(i.salt, 0, &data),
            None,
            "{} changed",
            i.path.display()
        );
    }
}

#[test]
fn guests_read_pmem_files_exactly() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("pmem");
    let images = images(&dir);
    let r = vm_run(&args(&images, ""), TIMEOUT);
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert_unchanged(&images);
}

#[test]
fn restored_copies_map_the_same_files() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("pmem-restore");
    let images = images(&dir);
    let snap = dir.join("snap");
    let mut a = args(&images, " shards_snapshot=1");
    a.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let original = vm_run(&a, TIMEOUT);
    assert_eq!(original.status, Some(0), "{original}");
    for _ in 0..2 {
        let r = run_shards(&["vm", "restore"], &[&snap], TIMEOUT);
        assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    }
    assert_unchanged(&images);
}
