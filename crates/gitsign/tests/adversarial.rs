//! Inputs an attacker shapes (a Git object's signature), read as Go reads them but in
//! bounded stack and linear time. go-crypto reads an embedded signature recursively, on a
//! stack that grows to 1 GiB; shards with no stack that grows with the input (held to
//! go-crypto by testdata/nested.json, `scripts/gitsign/generate-nested`). PEM as Go 1.26
//! finds it (pem.json, `generate-stdlib`). ASCII armor as go-crypto decodes it and
//! io.ReadAll reads its body, headers by name, lines trimmed as Go trims them, and the
//! body's base64 in Go's chunks (armor.json, `generate-armor`).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)]

use base64::Engine as _;
use sha2::Digest as _;
use shards_gitsign::{Packet, Reader, armor, pgpsign};

/// A v6 signature packet (Ed25519, SHA-256) whose hashed area embeds a primary-key-binding
/// signature, which embeds another, `depth` deep: built outside in, each level's prefix
/// then each suffix, so building it is linear in its size (nested_oracle_test.go's
/// nestedDeep, byte for byte).
fn nested_signature(depth: usize) -> Vec<u8> {
    const SUFFIX: usize = 4 + 2 + 1 + 16 + 64;
    // Each level: version, type, algorithm, hash, hashed length (4), a creation time
    // (6), and where it embeds, the embedded subpacket's header (255, length, type 32).
    const PREFIX_EMBEDDING: usize = 1 + 3 + 4 + 6 + 5 + 1;
    const PREFIX_LEAF: usize = 1 + 3 + 4 + 6;
    let mut sizes = vec![0usize; depth + 1];
    sizes[depth] = PREFIX_LEAF + SUFFIX;
    for k in (0..depth).rev() {
        sizes[k] = PREFIX_EMBEDDING + sizes[k + 1] + SUFFIX;
    }
    let mut out = Vec::with_capacity(sizes[0] + 6);
    out.push(0xC2);
    out.push(255);
    out.extend_from_slice(&(sizes[0] as u32).to_be_bytes());
    for k in 0..=depth {
        let sig_type = if k == 0 { 0x00 } else { 0x19 };
        out.extend_from_slice(&[6, sig_type, 27, 8]);
        if k == depth {
            out.extend_from_slice(&6u32.to_be_bytes());
            out.extend_from_slice(&[5, 2, 0, 0, 0, 1]);
        } else {
            let child = sizes[k + 1];
            out.extend_from_slice(&((6 + 5 + 1 + child) as u32).to_be_bytes());
            out.extend_from_slice(&[5, 2, 0, 0, 0, 1]);
            out.push(255);
            out.extend_from_slice(&((1 + child) as u32).to_be_bytes());
            out.push(32);
        }
    }
    for _ in 0..=depth {
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&[0, 0, 16]);
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&[0u8; 64]);
    }
    out
}

/// nestedAnswer: what reading `packet` gives, on a thread of a 512 KiB stack: the error,
/// or the signature's type and its embedded signature's.
fn answer(packet: Vec<u8>) -> String {
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || match Reader::new(&packet).next_packet() {
            Ok(Some(Packet::Signature(sig))) => match &sig.embedded {
                Some(e) => {
                    // A cross-signature's own embedded signature is not kept (D103).
                    assert!(e.embedded.is_none());
                    format!("ok {} {}", sig.sig_type, e.sig_type)
                }
                None => format!("ok {} none", sig.sig_type),
            },
            Ok(other) => format!("{other:?}"),
            Err(e) => e.to_string(),
        })
        .unwrap()
        .join()
        .unwrap()
}

/// Every case of nested.json as go-crypto answered it: malformed signatures embedded
/// below the first, each refused in go-crypto's words; and one embedding 20000 deep,
/// deeper than any stack holds frames for, read whole.
#[test]
fn embedded_signatures_are_read_as_go_crypto_reads_them_without_recursion() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/nested.json");
    let cases: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut deep = 0;
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let packet = match c["packet"].as_str() {
            Some(p) => b64.decode(p).unwrap(),
            None => {
                let depth = usize::try_from(c["depth"].as_u64().unwrap()).unwrap();
                let built = nested_signature(depth);
                assert_eq!(
                    hex(&sha2::Sha256::digest(&built)),
                    c["sha256"].as_str().unwrap(),
                    "{name}: built as the oracle built it"
                );
                deep = deep.max(depth);
                built
            }
        };
        assert_eq!(answer(packet), c["answer"].as_str().unwrap(), "{name}");
    }
    assert!(deep >= 20_000, "the oracle holds a case deeper than a stack");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Every case of pem.json as Go 1.26's encoding/pem.Decode answered it (the first
/// block's type, headers as its map keeps them, bytes; or none): the line ends it reads,
/// the headers it trims, and inputs of tens of thousands of BEGIN lines, which Go reads in
/// time linear in them (since CVE-2025-61723) and so does this.
#[test]
fn pem_blocks_are_found_as_go_finds_them() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pem.json");
    let cases: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut failures = Vec::new();
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let input = match c["sha256"].as_str() {
            None => b64.decode(c["input"].as_str().unwrap_or_default()).unwrap(),
            Some(_) => {
                let unit = b64.decode(c["unit"].as_str().unwrap()).unwrap();
                let mut data = unit.repeat(usize::try_from(c["repeat"].as_u64().unwrap()).unwrap());
                data.extend_from_slice(&b64.decode(c["tail"].as_str().unwrap_or_default()).unwrap());
                assert_eq!(
                    hex(&sha2::Sha256::digest(&data)),
                    c["sha256"].as_str().unwrap(),
                    "{name}"
                );
                data
            }
        };
        let got = shards_gitsign::pem::decode(&input).map(|b| {
            // Go's map: each name's last value.
            let mut headers = std::collections::BTreeMap::new();
            for (k, v) in &b.headers {
                headers.insert(k.clone(), v.clone());
            }
            let headers: Vec<Vec<String>> = headers.into_iter().map(|(k, v)| vec![k, v]).collect();
            (b.kind.clone(), headers, b64.encode(&*b.bytes))
        });
        let want = c["found"].as_bool().unwrap().then(|| {
            let headers: Vec<Vec<String>> = c["headers"]
                .as_array()
                .map(|hs| {
                    hs.iter()
                        .map(|h| {
                            h.as_array()
                                .unwrap()
                                .iter()
                                .map(|s| s.as_str().unwrap().to_string())
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                c["type"].as_str().unwrap_or_default().to_string(),
                headers,
                c["bytes"].as_str().unwrap_or_default().to_string(),
            )
        });
        if got != want {
            failures.push(format!("{name}: got {got:?}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// armor_oracle_test.go's xorshift64*.
struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Xorshift {
        Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn next(&mut self) -> u64 {
        let mut v = self.0;
        v ^= v >> 12;
        v ^= v << 25;
        v ^= v >> 27;
        self.0 = v;
        v.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const SYMBOLS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const SPECIALS: [&str; 10] = ["=", "==", "\r", " ", "\t", "\x0b", "\u{a0}", "!", ":", "-"];

/// fuzzText.
fn fuzz_text(x: &mut Xorshift, max: usize, percent: usize) -> String {
    let mut b = String::new();
    for _ in 0..x.below(max + 1) {
        if x.below(100) < percent {
            b.push_str(SPECIALS[x.below(SPECIALS.len())]);
        } else {
            b.push(char::from(SYMBOLS[x.below(64)]));
        }
    }
    b
}

/// armorFuzz.
fn armor_fuzz(seed: u64) -> Vec<u8> {
    let mut x = Xorshift::new(seed);
    let mut b = String::new();
    for _ in 0..x.below(3) {
        b += &fuzz_text(&mut x, 140, 10);
        b.push('\n');
    }
    b += "-----BEGIN PGP SIGNATURE-----\n";
    for _ in 0..x.below(4) {
        b += &format!("K{}: ", x.below(3));
        b += &fuzz_text(&mut x, 220, 5);
        b.push('\n');
    }
    b += ["\n", " \n", "\r\n", "\u{a0}\n"][x.below(4)];
    if x.below(2) == 0 {
        let width = 4 * (1 + x.below(24));
        let lines = 1 + x.below(120);
        let pad = x.below(lines * width / 4);
        for l in 0..lines {
            for q in 0..width / 4 {
                if l * width / 4 + q == pad {
                    b += "QQ==";
                    continue;
                }
                for _ in 0..4 {
                    b.push(char::from(SYMBOLS[x.below(64)]));
                }
            }
            b.push('\n');
        }
    } else {
        for _ in 0..1 + x.below(24) {
            b += &fuzz_text(&mut x, 100, 6);
            b += ["\n", "\r\n"][x.below(2)];
        }
    }
    b += [
        "-----END PGP SIGNATURE-----\n",
        "=AAAA\n-----END PGP SIGNATURE-----\n",
        "",
        "-----END PGP SIGNATURE-----",
    ][x.below(4)];
    b.into_bytes()
}

/// sweepInput.
fn sweep_input(width: usize, lines: usize, pad: usize) -> Vec<u8> {
    let mut x = Xorshift::new(width as u64);
    let mut stream: Vec<u8> = (0..width * lines).map(|_| SYMBOLS[x.below(64)]).collect();
    stream[4 * pad..4 * pad + 4].copy_from_slice(b"QQ==");
    let mut b = b"-----BEGIN PGP SIGNATURE-----\n\n".to_vec();
    for line in stream.chunks(width) {
        b.extend_from_slice(line);
        b.push(b'\n');
    }
    b.extend_from_slice(b"-----END PGP SIGNATURE-----\n");
    b
}

fn headers_digest(headers: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut sorted: Vec<_> = headers.iter().collect();
    sorted.sort();
    let mut d = sha2::Sha256::new();
    for (k, v) in sorted {
        d.update(k);
        d.update(b"\0");
        d.update(v);
        d.update(b"\n");
    }
    hex(&d.finalize())
}

fn digest16(b: &[u8]) -> String {
    hex(&sha2::Sha256::digest(b)[..8])
}

/// armorSummary.
fn armor_summary(data: &[u8]) -> String {
    let block = match armor::decode(data) {
        Ok(b) => b,
        Err(e) => return e.to_string(),
    };
    let kind = block.kind.clone();
    let headers = headers_digest(&block.headers);
    match block.read_body().0 {
        Ok(body) => format!(
            "ok {kind:?} {} {} {}",
            headers.get(..16).unwrap(),
            body.len(),
            digest16(&body)
        ),
        Err(e) => e.to_string(),
    }
}

/// armorAnswer: the fields armor_oracle_test.go writes of a case's answer.
fn armor_answer(data: &[u8], summarize: bool) -> serde_json::Map<String, serde_json::Value> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut a = serde_json::Map::new();
    let block = match armor::decode(data) {
        Ok(b) => b,
        Err(e) => {
            a.insert("error".into(), e.to_string().into());
            return a;
        }
    };
    let kind = block.kind.clone();
    let headers = block.headers.clone();
    let body = match block.read_body().0 {
        Ok(body) => body,
        Err(e) => {
            a.insert("error".into(), e.to_string().into());
            return a;
        }
    };
    a.insert("type".into(), kind.into());
    if summarize {
        // Go's omitempty: a count of none is left out.
        if !headers.is_empty() {
            a.insert("headerCount".into(), headers.len().into());
        }
        a.insert("headersSha256".into(), headers_digest(&headers).into());
        a.insert("body".into(), digest16(&body).into());
        return a;
    }
    if !headers.is_empty() {
        let mut sorted = headers;
        sorted.sort();
        let list: Vec<serde_json::Value> = sorted
            .iter()
            .map(|(k, v)| serde_json::json!([String::from_utf8_lossy(k), String::from_utf8_lossy(v)]))
            .collect();
        a.insert("headers".into(), list.into());
    }
    if !body.is_empty() {
        a.insert("body".into(), b64.encode(&*body).into());
    }
    a
}

/// Every case of armor.json as go-crypto's armor.Decode and io.ReadAll answered it: its
/// headers by name, lines trimmed as bytes.TrimSpace trims them, lines too long, and the
/// body's base64 decoded in the chunks io.ReadAll's reads make, so that padding ending a
/// chunk is taken and errors point where Go's do (the sweeps put padding at every place
/// of lines 92, 96 and 64 symbols wide, past slices whose capacities Go's size classes
/// round); 600 inputs armorFuzz builds; and ReadAllArmoredKeyRings's errors. Its large
/// cases, tens of thousands of headers, read in time linear in them.
#[test]
fn armor_is_read_as_go_crypto_reads_it() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/armor.json");
    let o: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut failures = Vec::new();
    for c in o["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let (input, summarize) = match c["sha256"].as_str() {
            None => (
                b64.decode(c["input"].as_str().unwrap_or_default()).unwrap(),
                false,
            ),
            Some(sum) => {
                let unit = String::from_utf8(b64.decode(c["unit"].as_str().unwrap()).unwrap()).unwrap();
                let mut data = b64.decode(c["head"].as_str().unwrap()).unwrap();
                for i in 0..c["repeat"].as_u64().unwrap() {
                    data.extend_from_slice(unit.replace("%d", &i.to_string()).as_bytes());
                }
                data.extend_from_slice(&b64.decode(c["tail"].as_str().unwrap()).unwrap());
                assert_eq!(
                    hex(&sha2::Sha256::digest(&data)),
                    sum,
                    "{name}: built as the oracle built it"
                );
                (data, true)
            }
        };
        let mut want = c.as_object().unwrap().clone();
        for k in ["name", "input", "head", "unit", "repeat", "tail", "sha256"] {
            want.remove(k);
        }
        let got = armor_answer(&input, summarize);
        if got != want {
            failures.push(format!("{name}: got {got:?}, want {want:?}"));
        }
    }
    for c in o["fuzz"].as_array().unwrap() {
        let seed = c["seed"].as_u64().unwrap();
        let input = armor_fuzz(seed);
        assert_eq!(
            hex(&sha2::Sha256::digest(&input)),
            c["sha256"].as_str().unwrap(),
            "fuzz {seed}: built as the oracle built it"
        );
        let got = armor_summary(&input);
        if got != c["answer"].as_str().unwrap() {
            failures.push(format!(
                "fuzz {seed} {:?}: got {got}, want {}",
                String::from_utf8_lossy(&input),
                c["answer"]
            ));
        }
    }
    for s in o["sweeps"].as_array().unwrap() {
        let width = usize::try_from(s["width"].as_u64().unwrap()).unwrap();
        let lines = usize::try_from(s["lines"].as_u64().unwrap()).unwrap();
        for (pad, want) in s["answers"].as_array().unwrap().iter().enumerate() {
            let got = armor_summary(&sweep_input(width, lines, pad));
            if got != want.as_str().unwrap() {
                failures.push(format!("sweep {width}x{lines} pad {pad}: got {got}, want {want}"));
            }
        }
    }
    for c in o["keyRings"].as_array().unwrap() {
        let input = b64.decode(c["input"].as_str().unwrap()).unwrap();
        let got = match pgpsign::read_all_armored_key_rings(&input) {
            Ok(entities) => format!("ok {}", entities.len()),
            Err(e) => e,
        };
        if got != c["answer"].as_str().unwrap() {
            failures.push(format!(
                "key rings {}: got {got}, want {}",
                c["name"], c["answer"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
