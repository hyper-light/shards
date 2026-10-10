//! What reading the inputs an attacker or a policy shapes costs: wall clock per read, in
//! microseconds, over `--runs N` reads each (default 20), after one read not counted.
//!
//! - `armor_headers`: an armored signature of 60000 distinct headers
//!   (parse_armored_detached_signature).
//! - `armor_body`: an armored signature of 20000 body lines of 76 symbols, as io.ReadAll
//!   reads it.
//! - `pem_begins`: 50000 BEGIN lines and no END (pem::decode).
//! - `nested_2000`, `nested_20000`: a signature embedding signatures 2000 and 20000 deep
//!   (packet reader; `--shallow` leaves out the 20000, which a recursive reader cannot
//!   read on the main thread's stack).
//! - `keyring_400`: a key ring of one user ID with 400 revocations of its own, each
//!   verified (Ed25519).
//!
//! `cargo bench -p shards-gitsign --bench parse [-- --runs N] [--shallow]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::cast_possible_truncation
)]

use std::process::Command;
use std::time::Instant;

use aws_lc_rs::digest;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use shards_gitsign::{Reader, keyring, pem};

fn option(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn flag(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == name)
}

/// Microseconds per call of `f`, `runs` times after one call not counted.
fn timed(runs: usize, mut f: impl FnMut()) -> Vec<f64> {
    f();
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect()
}

/// Nearest-rank percentile of sorted `v`.
fn percentile(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn stamp() -> String {
    let (host, os, load) = if cfg!(target_os = "macos") {
        (
            format!(
                "{} ({})",
                output("sysctl", &["-n", "machdep.cpu.brand_string"]),
                output("sysctl", &["-n", "hw.model"])
            ),
            format!(
                "macOS {} ({})",
                output("sw_vers", &["-productVersion"]),
                output("sw_vers", &["-buildVersion"])
            ),
            output("sysctl", &["-n", "vm.loadavg"]),
        )
    } else {
        (
            std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|c| {
                    c.lines()
                        .find(|l| l.starts_with("model name"))
                        .and_then(|l| l.split(':').nth(1))
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|| "unknown".into()),
            format!("{} {}", output("uname", &["-s"]), output("uname", &["-r"])),
            std::fs::read_to_string("/proc/loadavg").unwrap_or_default(),
        )
    };
    let dir = env!("CARGO_MANIFEST_DIR");
    let mut rev = output("git", &["-C", dir, "rev-parse", "--short", "HEAD"]);
    if !output(
        "git",
        &["-C", dir, "status", "--porcelain", "--untracked-files=no"],
    )
    .ends_with("unknown")
    {
        rev.push_str("-dirty");
    }
    format!("host: {host}\nos: {os}\nrevision: {rev}\nload: {}", load.trim())
}

fn nested_signature(depth: usize) -> Vec<u8> {
    const SUFFIX: usize = 4 + 2 + 1 + 16 + 64;
    const PREFIX_EMBEDDING: usize = 1 + 3 + 4 + 6 + 5 + 1;
    const PREFIX_LEAF: usize = 1 + 3 + 4 + 6;
    let mut sizes = vec![0usize; depth + 1];
    sizes[depth] = PREFIX_LEAF + SUFFIX;
    for k in (0..depth).rev() {
        sizes[k] = PREFIX_EMBEDDING + sizes[k + 1] + SUFFIX;
    }
    let mut out = vec![0xC2, 255];
    out.extend_from_slice(&(sizes[0] as u32).to_be_bytes());
    for k in 0..=depth {
        out.extend_from_slice(&[6, if k == 0 { 0x00 } else { 0x19 }, 27, 8]);
        if k == depth {
            out.extend_from_slice(&6u32.to_be_bytes());
            out.extend_from_slice(&[5, 2, 0, 0, 0, 1]);
        } else {
            let child = sizes[k + 1];
            out.extend_from_slice(&((6 + 5 + 1 + child) as u32).to_be_bytes());
            out.extend_from_slice(&[5, 2, 0, 0, 0, 1, 255]);
            out.extend_from_slice(&((1 + child) as u32).to_be_bytes());
            out.push(32);
        }
    }
    for _ in 0..=depth {
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 16]);
        out.extend_from_slice(&[0u8; 16 + 64]);
    }
    out
}

fn packet(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0xC0 | tag];
    if body.len() < 192 {
        out.push(body.len() as u8);
    } else {
        let n = body.len() - 192;
        out.extend_from_slice(&[(n >> 8) as u8 + 192, n as u8]);
    }
    out.extend_from_slice(body);
    out
}

fn mpi(b: &[u8]) -> Vec<u8> {
    let b = &b[b.iter().take_while(|&&x| x == 0).count()..];
    let bits = b.first().map_or(0, |f| 8 * b.len() - f.leading_zeros() as usize);
    let mut out = (bits as u16).to_be_bytes().to_vec();
    out.extend_from_slice(b);
    out
}

/// tests/keyring_cost.rs's ring: a v4 EdDSA key, a user ID, `n` revocations of its own.
fn ring(n: usize) -> Vec<u8> {
    let pair = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
    let mut key = vec![
        4, 0x60, 0, 0, 0, 22, 9, 0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01,
    ];
    let mut point = vec![0x40];
    point.extend_from_slice(pair.public_key().as_ref());
    key.extend_from_slice(&mpi(&point));
    let mut key_hash = vec![0x99];
    key_hash.extend_from_slice(&(key.len() as u16).to_be_bytes());
    key_hash.extend_from_slice(&key);
    let fingerprint = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &key_hash);
    let fingerprint = fingerprint.as_ref();
    let uid = b"Revoked Often <often@example.com>";
    let mut out = packet(6, &key);
    out.extend_from_slice(&packet(13, uid));
    for i in 0..n {
        let mut hashed = vec![5, 2];
        hashed.extend_from_slice(&(0x6000_0000u32 + i as u32).to_be_bytes());
        hashed.extend_from_slice(&[22, 33, 4]);
        hashed.extend_from_slice(fingerprint);
        let mut suffix = vec![4, keyring::CERTIFICATION_REVOCATION, 22, 8];
        suffix.extend_from_slice(&(hashed.len() as u16).to_be_bytes());
        suffix.extend_from_slice(&hashed);
        let mut signed = key_hash.clone();
        signed.push(0xB4);
        signed.extend_from_slice(&(uid.len() as u32).to_be_bytes());
        signed.extend_from_slice(uid);
        signed.extend_from_slice(&suffix);
        signed.extend_from_slice(&[4, 0xFF]);
        signed.extend_from_slice(&(suffix.len() as u32).to_be_bytes());
        let h = digest::digest(&digest::SHA256, &signed);
        let sig = pair.sign(h.as_ref());
        let sig = sig.as_ref();
        let mut body = suffix;
        body.extend_from_slice(&[0, 10, 9, 16]);
        body.extend_from_slice(&fingerprint[12..]);
        body.extend_from_slice(&h.as_ref()[..2]);
        body.extend_from_slice(&mpi(&sig[..32]));
        body.extend_from_slice(&mpi(&sig[32..]));
        out.extend_from_slice(&packet(2, &body));
    }
    out
}

fn main() {
    let runs: usize = option("--runs").map_or(20, |v| v.parse().expect("--runs N"));
    const BEGIN: &str = "-----BEGIN PGP SIGNATURE-----\n";
    const END: &str = "-----END PGP SIGNATURE-----\n";
    let mut headers = String::from(BEGIN);
    for i in 0..60000 {
        headers += &format!("Header-{i}: value\n");
    }
    headers += "\nQUJD\n";
    headers += END;
    let mut body = String::from(BEGIN);
    body.push('\n');
    for _ in 0..20000 {
        body += "QUJDQUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAx\n";
    }
    body += END;
    let begins = "\n-----BEGIN A-----".repeat(50000);
    let nested = nested_signature(2000);
    let deep = nested_signature(20000);
    let keys = ring(400);
    let mut rows = vec![
        (
            "armor_headers",
            timed(runs, || {
                let _ = shards_gitsign::parse_armored_detached_signature(headers.as_bytes());
            }),
        ),
        (
            "armor_body",
            timed(runs, || {
                let _ = shards_gitsign::parse_armored_detached_signature(body.as_bytes());
            }),
        ),
        (
            "pem_begins",
            timed(runs, || {
                let _ = pem::decode(begins.as_bytes());
            }),
        ),
        (
            "nested_2000",
            timed(runs, || {
                let _ = Reader::new(&nested).next_packet();
            }),
        ),
        (
            "keyring_400",
            timed(runs, || {
                let _ = keyring::read_key_ring(&keys).unwrap();
            }),
        ),
    ];
    if !flag("--shallow") {
        rows.push((
            "nested_20000",
            timed(runs, || {
                let _ = Reader::new(&deep).next_packet();
            }),
        ));
    }
    println!("parse: runs={runs}\n{}\n", stamp());
    println!(
        "{:<16} {:>12} {:>12} {:>12} {:>12}",
        "input", "p50", "p90", "p99", "max"
    );
    for (name, mut v) in rows {
        v.sort_by(f64::total_cmp);
        println!(
            "{name:<16} {:>12.1} {:>12.1} {:>12.1} {:>12.1}  us",
            percentile(&v, 50.0),
            percentile(&v, 90.0),
            percentile(&v, 99.0),
            v[v.len() - 1]
        );
    }
}
