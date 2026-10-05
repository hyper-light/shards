//! Archives built to hurt whoever unpacks them, as a guest's can hurt `docker cp`'s client:
//! nothing may be written outside the destination, and no archive may exhaust the stack or
//! memory.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs;
use std::path::PathBuf;

use shards_archive::tar::{Format, Header, TYPE_REG, Time, Writer};
use shards_archive::{UnpackOptions, unpack};

fn tmp(name: &str) -> PathBuf {
    let dir = fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("shards-archive-adv-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("dest")).unwrap();
    dir
}

fn opts() -> UnpackOptions {
    UnpackOptions {
        no_lchown: true,
        ..UnpackOptions::default()
    }
}

/// A name of 200,000 components: os.Root walks it a component at a time and the
/// resolution of its missing parents is iterative, so this ends in an error (the
/// system's path limit), not a crash, and nothing appears beside the destination.
#[test]
fn deep_names_end_without_exhausting_the_stack() {
    let dir = tmp("deep");
    let mut name = b"a/".repeat(200_000);
    name.extend_from_slice(b"x");
    let mut tw = Writer::new(Vec::new());
    let hdr = Header {
        typeflag: TYPE_REG,
        name,
        mode: 0o644,
        size: 1,
        mtime: Time::unix(1, 0),
        format: Format::PAX,
        ..Header::default()
    };
    tw.write_header(&hdr).unwrap();
    std::io::Write::write_all(&mut tw, b"x").unwrap();
    let archive = tw.finish().unwrap();
    let _ = unpack(archive.as_slice(), &dir.join("dest"), &opts());
    let beside: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(beside, ["dest"]);
    fs::remove_dir_all(&dir).unwrap();
}

/// An old GNU sparse header whose extension blocks never end: held to 1 MiB of map.
#[test]
fn sparse_maps_are_bounded() {
    let mut hdr = [0u8; 512];
    hdr[..4].copy_from_slice(b"bomb");
    hdr[100..108].copy_from_slice(b"0000644\0");
    hdr[124..136].copy_from_slice(b"00000000000\0");
    hdr[136..148].copy_from_slice(b"00000000000\0");
    hdr[156] = b'S';
    hdr[257..263].copy_from_slice(b"ustar ");
    hdr[263..265].copy_from_slice(b" \0");
    hdr[482] = 1;
    hdr[483..495].copy_from_slice(b"00000000000\0");
    hdr[148..156].copy_from_slice(b"        ");
    let sum: u32 = hdr.iter().map(|&c| u32::from(c)).sum();
    hdr[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut ext = [0u8; 512];
    ext[504] = 1;
    let mut archive = hdr.to_vec();
    for _ in 0..4096 {
        archive.extend_from_slice(&ext);
    }
    let dir = tmp("sparse");
    let err = unpack(archive.as_slice(), &dir.join("dest"), &opts()).unwrap_err();
    assert_eq!(err.to_string(), "archive/tar: sparse map too long");
    fs::remove_dir_all(&dir).unwrap();
}
