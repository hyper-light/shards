//! The gzip writer held to Go's, byte for byte: the corpus `scripts/flate/generate`
//! compressed with Go 1.25.8's compress/gzip at every level, made again here from the
//! same generator, and compressed by this crate.

#![allow(clippy::indexing_slicing)]

use std::io::Write as _;

use sha2::{Digest as _, Sha256};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 56) as u8
    }
}

const WORDS: [&str; 16] = [
    "the", "flate", "layer", "docker", "shards", "image", "build", "tar", "usr", "bin", "lib", "etc", "a",
    "of", "and", "to",
];

fn text(g: &mut Lcg, n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    while b.len() < n {
        b.extend_from_slice(WORDS[usize::from(g.next() % 16)].as_bytes());
        b.push(if g.next().is_multiple_of(8) { b'\n' } else { b' ' });
    }
    b.truncate(n);
    b
}

fn runs(g: &mut Lcg, n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    while b.len() < n {
        let v = g.next();
        let l = 1 + usize::from(g.next() % 64);
        b.extend(std::iter::repeat_n(v, l));
    }
    b.truncate(n);
    b
}

fn random(g: &mut Lcg, n: usize) -> Vec<u8> {
    (0..n).map(|_| g.next()).collect()
}

fn input(kind: &str, n: usize, seed: u64) -> Vec<u8> {
    let g = &mut Lcg(seed);
    match kind {
        "zeros" => vec![0; n],
        "random" => random(g, n),
        "text" => text(g, n),
        "runs" => runs(g, n),
        _ => {
            let mut b = Vec::new();
            let mut i = 0;
            while b.len() < n {
                match i % 3 {
                    0 => b.extend(text(g, 997)),
                    1 => b.extend(random(g, 997)),
                    _ => b.extend(runs(g, 997)),
                }
                i += 1;
            }
            b.truncate(n);
            b
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn gzip_is_gos() {
    let data: serde_json::Value = serde_json::from_slice(
        &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/gzip.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(data["go"], "go1.25.8");
    let cases = data["cases"].as_array().unwrap();
    assert!(cases.len() > 1500);
    let mut failures = Vec::new();
    let mut made: Option<(u64, Vec<u8>)> = None;
    for c in cases {
        let (kind, size, seed, level) = (
            c["kind"].as_str().unwrap(),
            c["size"].as_u64().unwrap() as usize,
            c["seed"].as_u64().unwrap(),
            c["level"].as_i64().unwrap() as i32,
        );
        if made.as_ref().is_none_or(|(s, _)| *s != seed) {
            let b = input(kind, size, seed);
            assert_eq!(
                hex(&Sha256::digest(&b)),
                c["input"].as_str().unwrap(),
                "{kind} {size}: the corpus is not Go's"
            );
            made = Some((seed, b));
        }
        let b = &made.as_ref().unwrap().1;
        // Written in pieces of 32 KiB, as io.Copy writes a layer.
        let mut w = shards_flate::GzipWriter::new(Vec::new(), level).unwrap();
        for piece in b.chunks(32 * 1024) {
            w.write_all(piece).unwrap();
        }
        let out = w.finish().unwrap();
        let got = hex(&Sha256::digest(&out));
        if got != c["digest"].as_str().unwrap() || out.len() as u64 != c["length"].as_u64().unwrap() {
            let mut why = format!(
                "{kind} {size} level {level}: {} bytes {got}, want {} {}",
                out.len(),
                c["length"],
                c["digest"]
            );
            if let Some(want) = c["output"].as_str() {
                why.push_str(&format!("\n  got:  {}\n  want: {want}", hex(&out)));
            }
            failures.push(why);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {}:\n{}",
        failures.len(),
        cases.len(),
        failures[..failures.len().min(12)].join("\n")
    );
}

/// Go's levels, and none other.
#[test]
fn levels_are_gos() {
    for level in [-3, 10] {
        assert!(shards_flate::GzipWriter::new(Vec::new(), level).is_err());
    }
}
