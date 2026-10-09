//! Checkpoints and the proofs they anchor: RFC 6962 inclusion proofs as transparency-dev's
//! merkle verifies them; Rekor v1's signed checkpoints (util.SignedNote, its signature
//! lines read with fmt.Fscanf, and util.Checkpoint); and Rekor v2's, as C2SP signed notes
//! (x/mod's note.Open, rekor-tiles' note verifiers, transparency-dev's ParseCheckpoint).
//! Each failure in the words of the code it ports.

use super::gocodec::{STD_ENCODING, corrupt};
use crate::keys::{self, VerifyWith};
use crate::x509::{Hash, PublicKey};

fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().to_vec()
}

/// rfc6962.DefaultHasher.HashLeaf.
pub fn hash_leaf(leaf: &[u8]) -> Vec<u8> {
    sha256(&[&[0], leaf])
}

fn hash_children(l: &[u8], r: &[u8]) -> Vec<u8> {
    sha256(&[&[1], l, r])
}

/// fmt's %v of a byte slice: `[1 2 3]`.
fn bytes_v(b: &[u8]) -> String {
    let parts: Vec<String> = b.iter().map(u8::to_string).collect();
    format!("[{}]", parts.join(" "))
}

/// proof.VerifyInclusion with the RFC 6962 hasher.
pub fn verify_inclusion(
    index: u64,
    size: u64,
    leaf: &[u8],
    proof: &[Vec<u8>],
    root: &[u8],
) -> Result<(), String> {
    if index >= size {
        return Err(format!("index is beyond size: {index} >= {size}"));
    }
    if leaf.len() != 32 {
        return Err(format!("leafHash has unexpected size {}, want 32", leaf.len()));
    }
    let inner = 64 - (index ^ (size - 1)).leading_zeros() as usize;
    let border = (index.checked_shr(inner as u32).unwrap_or(0)).count_ones() as usize;
    if proof.len() != inner + border {
        return Err(format!(
            "wrong proof size {}, want {}",
            proof.len(),
            inner + border
        ));
    }
    let mut seed = leaf.to_vec();
    for (i, h) in proof.iter().take(inner).enumerate() {
        seed = if (index >> i) & 1 == 0 {
            hash_children(&seed, h)
        } else {
            hash_children(h, &seed)
        };
    }
    for h in proof.iter().skip(inner) {
        seed = hash_children(h, &seed);
    }
    if seed != root {
        return Err(format!(
            "calculated root:\n{}\n does not match expected root:\n{}",
            bytes_v(&seed),
            bytes_v(root)
        ));
    }
    Ok(())
}

/// fmt's isSpace (scan.go's table).
fn fmt_space(r: char) -> bool {
    let r = r as u32;
    if r >= 1 << 16 {
        return false;
    }
    matches!(r, 0x9..=0xd | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000)
}

/// The runes of a line as strings.Reader yields them: invalid octets as U+FFFD, one each.
fn runes(line: &[u8]) -> Vec<char> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let rest = line.get(i..).unwrap_or_default();
        let mut got = None;
        for w in 1..=4 {
            if let Some(head) = rest.get(..w)
                && let Ok(t) = std::str::from_utf8(head)
            {
                got = t.chars().next().map(|c| (c, w));
                break;
            }
        }
        match got {
            Some((c, w)) => {
                out.push(c);
                i += w;
            }
            None => {
                out.push('\u{fffd}');
                i += 1;
            }
        }
    }
    out
}

/// fmt.Fscanf(line, "— %s %s\n", &name, &signature) on a line without its newline.
pub fn scan_signature_line(line: &[u8]) -> Result<(String, String), String> {
    let rs = runes(line);
    let mut at = 0;
    let get = |at: &mut usize| -> Option<char> {
        let c = rs.get(*at).copied();
        if c.is_some() {
            *at += 1;
        }
        c
    };
    // The literal dash.
    match get(&mut at) {
        None => return Err("unexpected EOF".into()),
        Some('\u{2014}') => {}
        Some(_) => return Err("input does not match format".into()),
    }
    // " " then %s: one or more spaces (or the end), then the verb skips spaces.
    match rs.get(at) {
        Some(&c) if !fmt_space(c) => return Err("expected space in input to match format".into()),
        _ => {}
    }
    let mut tokens = Vec::new();
    for k in 0..2 {
        while rs.get(at).is_some_and(|c| fmt_space(*c)) {
            at += 1;
        }
        if at >= rs.len() {
            return Err("EOF".into());
        }
        let start = at;
        while rs.get(at).is_some_and(|c| !fmt_space(*c)) {
            at += 1;
        }
        tokens.push(rs.get(start..at).unwrap_or_default().iter().collect::<String>());
        if k == 0 {
            // " " between the verbs: the token stopped at a space or the end.
            while rs.get(at).is_some_and(|c| fmt_space(*c)) {
                at += 1;
            }
        }
    }
    // "\n": spaces, then a newline or the end.
    while rs.get(at).is_some_and(|c| fmt_space(*c)) {
        at += 1;
    }
    if at < rs.len() {
        return Err("newline in format does not match input".into());
    }
    let sig = tokens.pop().unwrap_or_default();
    let name = tokens.pop().unwrap_or_default();
    Ok((name, sig))
}

/// A signed note's signature (note.Signature as rekor's SignedNote keeps it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSignature {
    pub name: String,
    pub hash: u32,
    pub sig: Vec<u8>,
}

/// bufio.Scanner's lines (ScanLines), as many as its 64 KiB buffer lets it read.
fn scanner_lines(data: &[u8]) -> Vec<&[u8]> {
    const MAX: usize = 64 * 1024;
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (line, next, taken) = match rest.iter().position(|c| *c == b'\n') {
            Some(i) => (
                rest.get(..i).unwrap_or_default(),
                rest.get(i + 1..).unwrap_or_default(),
                i + 1,
            ),
            None => (rest, &[][..], rest.len()),
        };
        if taken > MAX {
            break;
        }
        out.push(line.strip_suffix(b"\r").unwrap_or(line));
        rest = next;
    }
    out
}

/// SignedNote.UnmarshalText: the note's text and its signatures.
pub fn unmarshal_signed_note(data: &[u8]) -> Result<(Vec<u8>, Vec<NoteSignature>), String> {
    let split = data
        .windows(2)
        .rposition(|w| w == b"\n\n")
        .ok_or("malformed note")?;
    let text = data.get(..split + 1).unwrap_or_default();
    let sigs = data.get(split + 2..).unwrap_or_default();
    if sigs.last() != Some(&b'\n') {
        return Err("malformed note".into());
    }
    let mut out = Vec::new();
    for line in scanner_lines(sigs) {
        let (name, b64) = scan_signature_line(line).map_err(|e| format!("parsing signature: {e}"))?;
        let raw = STD_ENCODING
            .decode(b64.as_bytes())
            .map_err(|at| format!("decoding signature: {}", corrupt(at)))?;
        if raw.len() < 5 {
            return Err("signature is too small".into());
        }
        let hash = u32::from_be_bytes([
            raw.first().copied().unwrap_or(0),
            raw.get(1).copied().unwrap_or(0),
            raw.get(2).copied().unwrap_or(0),
            raw.get(3).copied().unwrap_or(0),
        ]);
        out.push(NoteSignature {
            name,
            hash,
            sig: raw.get(4..).unwrap_or_default().to_vec(),
        });
    }
    if out.is_empty() {
        return Err("no signatures found in input".into());
    }
    Ok((text.to_vec(), out))
}

/// util.Checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub origin: String,
    pub size: u64,
    pub hash: Vec<u8>,
}

/// strconv.ParseUint(s, 10, 64)'s error.
fn parse_uint(s: &[u8]) -> Result<u64, String> {
    let q = shards_dockerfile::go::quote(s);
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return Err(format!("strconv.ParseUint: parsing {q}: invalid syntax"));
    }
    String::from_utf8_lossy(s)
        .parse::<u64>()
        .map_err(|_| format!("strconv.ParseUint: parsing {q}: value out of range"))
}

/// util.Checkpoint.UnmarshalCheckpoint.
pub fn unmarshal_checkpoint(data: &[u8]) -> Result<Checkpoint, String> {
    let l: Vec<&[u8]> = data.split(|c| *c == b'\n').collect();
    if l.len() < 4 {
        return Err("invalid checkpoint - too few newlines".into());
    }
    let origin = l.first().copied().unwrap_or_default();
    if origin.is_empty() {
        return Err("invalid checkpoint - empty ecosystem".into());
    }
    let size = parse_uint(l.get(1).copied().unwrap_or_default())
        .map_err(|e| format!("invalid checkpoint - size invalid: {e}"))?;
    let hash = STD_ENCODING
        .decode(l.get(2).copied().unwrap_or_default())
        .map_err(|at| format!("invalid checkpoint - invalid hash: {}", corrupt(at)))?;
    Ok(Checkpoint {
        origin: String::from_utf8_lossy(origin).into_owned(),
        size,
        hash,
    })
}

/// getPublicKeyHash: the first four octets of the SHA-256 of the key's PKIX encoding.
fn public_key_hash(key: &PublicKey) -> Option<u32> {
    let der = keys::marshal_pkix(key)?;
    let h = sha256(&[&der]);
    Some(u32::from_be_bytes([
        h.first().copied().unwrap_or(0),
        h.get(1).copied().unwrap_or(0),
        h.get(2).copied().unwrap_or(0),
        h.get(3).copied().unwrap_or(0),
    ]))
}

/// SignedNote.Verify.
pub fn signed_note_verify(text: &[u8], sigs: &[NoteSignature], verifier: &keys::Verifier) -> bool {
    if sigs.is_empty() {
        return false;
    }
    let digest = sha256(&[text]);
    let key = verifier.public_key();
    let Some(hash) = public_key_hash(&key) else {
        return false;
    };
    for s in sigs {
        if s.hash != hash {
            return false;
        }
        let with = match key {
            PublicKey::Rsa { .. } | PublicKey::Ecdsa { .. } => VerifyWith {
                digest: Some(&digest),
                hash: None,
            },
            PublicKey::Ed25519(_) => VerifyWith::default(),
            _ => return false,
        };
        if verifier.verify(&s.sig, text, &with).is_err() {
            return false;
        }
    }
    true
}

/// rekor's VerifyCheckpointSignature: the checkpoint signed by the log, its root hash
/// the proof's.
pub fn verify_checkpoint_signature(
    envelope: &[u8],
    root_hash: &[u8],
    verifier: &keys::Verifier,
) -> Result<(), String> {
    let parsed = unmarshal_signed_note(envelope)
        .map_err(|e| format!("unmarshalling signed note: {e}"))
        .and_then(|(text, sigs)| {
            unmarshal_checkpoint(&text)
                .map_err(|e| format!("unmarshalling checkpoint: {e}"))
                .map(|cp| (text, sigs, cp))
        });
    let (text, sigs, cp) =
        parsed.map_err(|e| format!("unmarshalling log entry checkpoint to SignedCheckpoint: {e}"))?;
    if !signed_note_verify(&text, &sigs, verifier) {
        return Err("signature on checkpoint did not verify".into());
    }
    if root_hash != cp.hash.as_slice() {
        return Err(format!(
            "proof root hash does not match signed tree head, expected {} got {}",
            super::gocodec::hex_encode(root_hash),
            super::gocodec::hex_encode(&cp.hash)
        ));
    }
    Ok(())
}

/// note.isValidName.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace) && !name.contains('+')
}

/// A C2SP note verifier as rekor-tiles' NewNoteVerifier makes one.
#[derive(Debug)]
pub struct NoteVerifier<'a> {
    pub name: String,
    pub hash: u32,
    pub verifier: &'a keys::Verifier,
}

/// rekor-tiles' KeyHash.
fn key_hash(origin: &str, key: &PublicKey) -> Result<u32, String> {
    let first4 = |h: &[u8]| {
        u32::from_be_bytes([
            h.first().copied().unwrap_or(0),
            h.get(1).copied().unwrap_or(0),
            h.get(2).copied().unwrap_or(0),
            h.get(3).copied().unwrap_or(0),
        ])
    };
    match key {
        PublicKey::Ecdsa { .. } => {
            let der = keys::marshal_pkix(key).ok_or("getting ECDSA key hash: marshaling public key")?;
            Ok(first4(&sha256(&[&der])))
        }
        PublicKey::Ed25519(k) => Ok(first4(&sha256(&[origin.as_bytes(), b"\n", &[1], k]))),
        PublicKey::Rsa { .. } => {
            let der = keys::marshal_pkix(key).ok_or("getting RSA key hash: marshaling public key")?;
            let mut alg = vec![255u8];
            alg.extend_from_slice(b"PKIX-RSA-PKCS#1v1.5");
            Ok(first4(&sha256(&[origin.as_bytes(), b"\n", &alg, &der])))
        }
        PublicKey::Dsa => Err("unsupported key type: *dsa.PublicKey".into()),
        PublicKey::Unknown => Err("unsupported key type: <nil>".into()),
    }
}

/// note.NewNoteVerifier.
pub fn new_note_verifier<'a>(origin: &str, verifier: &'a keys::Verifier) -> Result<NoteVerifier<'a>, String> {
    if !valid_name(origin) {
        return Err(format!("invalid name {origin}"));
    }
    let hash = key_hash(origin, &verifier.public_key())?;
    Ok(NoteVerifier {
        name: origin.to_string(),
        hash,
        verifier,
    })
}

/// note.Open with one known verifier: the text and the names and hashes of the
/// signatures it verified.
/// A signer's name and key hash.
type NameHash = (String, u32);

fn note_open(msg: &[u8], v: &NoteVerifier<'_>) -> Result<(Vec<u8>, Vec<NameHash>), String> {
    let malformed = || "malformed note".to_string();
    // Control characters other than newline, and invalid UTF-8, are refused.
    let text = std::str::from_utf8(msg).map_err(|_| malformed())?;
    if text.chars().any(|c| (c as u32) < 0x20 && c != '\n') {
        return Err(malformed());
    }
    let split = msg.windows(2).rposition(|w| w == b"\n\n").ok_or_else(malformed)?;
    let body = msg.get(..split + 1).unwrap_or_default();
    let mut sigs = msg.get(split + 2..).unwrap_or_default();
    if sigs.last() != Some(&b'\n') {
        return Err(malformed());
    }
    let mut verified: Vec<(String, u32)> = Vec::new();
    let mut count = 0;
    while let Some(i) = sigs.iter().position(|c| *c == b'\n') {
        let line = sigs.get(..i).unwrap_or_default();
        sigs = sigs.get(i + 1..).unwrap_or_default();
        let line = line.strip_prefix("\u{2014} ".as_bytes()).ok_or_else(malformed)?;
        let line = std::str::from_utf8(line).map_err(|_| malformed())?;
        let (name, b64) = line.split_once(' ').unwrap_or((line, ""));
        let sig = STD_ENCODING.decode(b64.as_bytes());
        let sig = match sig {
            Ok(s) if valid_name(name) && !b64.is_empty() && s.len() >= 5 => s,
            _ => return Err(malformed()),
        };
        let hash = u32::from_be_bytes([
            sig.first().copied().unwrap_or(0),
            sig.get(1).copied().unwrap_or(0),
            sig.get(2).copied().unwrap_or(0),
            sig.get(3).copied().unwrap_or(0),
        ]);
        count += 1;
        if count > 100 {
            return Err(malformed());
        }
        if name != v.name || hash != v.hash {
            continue;
        }
        if verified.iter().any(|(n, h)| n == name && *h == hash) {
            continue;
        }
        let ok = v
            .verifier
            .verify(sig.get(4..).unwrap_or_default(), body, &VerifyWith::default())
            .is_ok();
        if !ok {
            return Err(format!("invalid signature for key {name}+{hash:08x}"));
        }
        verified.push((name.to_string(), hash));
    }
    if verified.is_empty() {
        return Err("note has no verifiable signatures".into());
    }
    Ok((body.to_vec(), verified))
}

/// formats/log Checkpoint.Unmarshal.
fn log_checkpoint(data: &[u8]) -> Result<Checkpoint, String> {
    let l: Vec<&[u8]> = data.splitn(4, |c| *c == b'\n').collect();
    if l.len() < 4 {
        return Err("invalid checkpoint - too few newlines".into());
    }
    let origin = l.first().copied().unwrap_or_default();
    if origin.is_empty() {
        return Err("invalid checkpoint - empty origin".into());
    }
    let size = parse_uint(l.get(1).copied().unwrap_or_default())
        .map_err(|e| format!("invalid checkpoint - size invalid: {e}"))?;
    let hash = STD_ENCODING
        .decode(l.get(2).copied().unwrap_or_default())
        .map_err(|at| format!("invalid checkpoint - invalid hash: {}", corrupt(at)))?;
    Ok(Checkpoint {
        origin: String::from_utf8_lossy(origin).into_owned(),
        size,
        hash,
    })
}

/// rekor-tiles' VerifyCheckpoint (transparency-dev's ParseCheckpoint).
pub fn verify_checkpoint(envelope: &[u8], v: &NoteVerifier<'_>) -> Result<Checkpoint, String> {
    let inner = (|| {
        let (text, sigs) =
            note_open(envelope, v).map_err(|e| format!("failed to verify signatures on checkpoint: {e}"))?;
        if !sigs.iter().any(|(n, h)| *h == v.hash && *n == v.name) {
            return Err("no log signature found on note".to_string());
        }
        let cp = log_checkpoint(&text).map_err(|e| format!("failed to unmarshal checkpoint: {e}"))?;
        if cp.origin != v.name {
            return Err(format!(
                "got Origin {} but expected {}",
                shards_dockerfile::go::quote(cp.origin.as_bytes()),
                shards_dockerfile::go::quote(v.name.as_bytes())
            ));
        }
        Ok(cp)
    })();
    inner.map_err(|e| format!("unverified checkpoint signature: {e}"))
}

/// The hash a Rekor v1 checkpoint's verifier signs with: the log's signature hash.
pub fn signature_hash(key: &PublicKey) -> Hash {
    match key {
        PublicKey::Ecdsa {
            curve: crate::x509::Curve::P384,
            ..
        } => Hash::Sha384,
        PublicKey::Ecdsa {
            curve: crate::x509::Curve::P521,
            ..
        }
        | PublicKey::Ed25519(_) => Hash::Sha512,
        _ => Hash::Sha256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_lines_scan_as_fscanf_does() {
        let ok = |s: &str| scan_signature_line(s.as_bytes());
        assert_eq!(ok("\u{2014} a b"), Ok(("a".into(), "b".into())));
        assert_eq!(ok("\u{2014}   a   b  "), Ok(("a".into(), "b".into())));
        assert_eq!(
            ok("\u{2014} a b c").unwrap_err(),
            "newline in format does not match input"
        );
        assert_eq!(ok("\u{2014} a").unwrap_err(), "EOF");
        assert_eq!(ok("\u{2014}").unwrap_err(), "EOF");
        assert_eq!(
            ok("\u{2014}a b").unwrap_err(),
            "expected space in input to match format"
        );
        assert_eq!(ok("- a b").unwrap_err(), "input does not match format");
        assert_eq!(ok("").unwrap_err(), "unexpected EOF");
    }

    #[test]
    fn inclusion_proofs_check_as_merkle_s() {
        let leaves: Vec<Vec<u8>> = (0u8..5).map(|i| hash_leaf(&[i])).collect();
        let n01 = hash_children(&leaves[0], &leaves[1]);
        let n23 = hash_children(&leaves[2], &leaves[3]);
        let n0123 = hash_children(&n01, &n23);
        let root = hash_children(&n0123, &leaves[4]);
        verify_inclusion(
            2,
            5,
            &leaves[2],
            &[leaves[3].clone(), n01.clone(), leaves[4].clone()],
            &root,
        )
        .unwrap();
        verify_inclusion(4, 5, &leaves[4], std::slice::from_ref(&n0123), &root).unwrap();
        assert_eq!(
            verify_inclusion(5, 5, &leaves[4], &[], &root).unwrap_err(),
            "index is beyond size: 5 >= 5"
        );
        assert_eq!(
            verify_inclusion(4, 5, &leaves[4], &[], &root).unwrap_err(),
            "wrong proof size 0, want 1"
        );
    }
}
