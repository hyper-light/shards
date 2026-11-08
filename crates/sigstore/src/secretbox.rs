//! NaCl's secretbox, XSalsa20 and Poly1305, as Go's golang.org/x/crypto/nacl/secretbox
//! seals and opens it (the cipher of cosign's encrypted keys, go-securesystemslib
//! encrypted.go): the box is the tag, then the message XORed with the XSalsa20 stream
//! past its first 32 bytes, which are the Poly1305 key (Bernstein, "Cryptography in NaCl",
//! §9; "Extending the Salsa20 nonce"). Poly1305 is AWS-LC's; Salsa20, which AWS-LC has
//! none of, is the reference's core.

/// The bytes a box adds to its message: the tag.
pub const OVERHEAD: usize = 16;

/// "expand 32-byte k".
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Little-endian words of `bytes`.
fn words<const N: usize>(bytes: &[u8]) -> [u32; N] {
    let mut out = [0u32; N];
    for (w, b) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*b);
    }
    out
}

/// The words a block starts from: σ, the key, and the 16 bytes `input` (a nonce and a
/// counter, or HSalsa20's input).
fn state(key: &[u8; 32], input: &[u8; 16]) -> [u32; 16] {
    let [k0, k1, k2, k3, k4, k5, k6, k7] = words::<8>(key);
    let [i0, i1, i2, i3] = words::<4>(input);
    let [s0, s1, s2, s3] = SIGMA;
    [s0, k0, k1, k2, k3, s1, i0, i1, i2, i3, s2, k4, k5, k6, k7, s3]
}

/// The 20 rounds of Salsa20's core: ten double rounds, each a column round then a row
/// round, as the reference writes them.
fn rounds(x: [u32; 16]) -> [u32; 16] {
    let [
        mut x0,
        mut x1,
        mut x2,
        mut x3,
        mut x4,
        mut x5,
        mut x6,
        mut x7,
        mut x8,
        mut x9,
        mut x10,
        mut x11,
        mut x12,
        mut x13,
        mut x14,
        mut x15,
    ] = x;
    for _ in 0..10 {
        x4 ^= x0.wrapping_add(x12).rotate_left(7);
        x8 ^= x4.wrapping_add(x0).rotate_left(9);
        x12 ^= x8.wrapping_add(x4).rotate_left(13);
        x0 ^= x12.wrapping_add(x8).rotate_left(18);
        x9 ^= x5.wrapping_add(x1).rotate_left(7);
        x13 ^= x9.wrapping_add(x5).rotate_left(9);
        x1 ^= x13.wrapping_add(x9).rotate_left(13);
        x5 ^= x1.wrapping_add(x13).rotate_left(18);
        x14 ^= x10.wrapping_add(x6).rotate_left(7);
        x2 ^= x14.wrapping_add(x10).rotate_left(9);
        x6 ^= x2.wrapping_add(x14).rotate_left(13);
        x10 ^= x6.wrapping_add(x2).rotate_left(18);
        x3 ^= x15.wrapping_add(x11).rotate_left(7);
        x7 ^= x3.wrapping_add(x15).rotate_left(9);
        x11 ^= x7.wrapping_add(x3).rotate_left(13);
        x15 ^= x11.wrapping_add(x7).rotate_left(18);
        x1 ^= x0.wrapping_add(x3).rotate_left(7);
        x2 ^= x1.wrapping_add(x0).rotate_left(9);
        x3 ^= x2.wrapping_add(x1).rotate_left(13);
        x0 ^= x3.wrapping_add(x2).rotate_left(18);
        x6 ^= x5.wrapping_add(x4).rotate_left(7);
        x7 ^= x6.wrapping_add(x5).rotate_left(9);
        x4 ^= x7.wrapping_add(x6).rotate_left(13);
        x5 ^= x4.wrapping_add(x7).rotate_left(18);
        x11 ^= x10.wrapping_add(x9).rotate_left(7);
        x8 ^= x11.wrapping_add(x10).rotate_left(9);
        x9 ^= x8.wrapping_add(x11).rotate_left(13);
        x10 ^= x9.wrapping_add(x8).rotate_left(18);
        x12 ^= x15.wrapping_add(x14).rotate_left(7);
        x13 ^= x12.wrapping_add(x15).rotate_left(9);
        x14 ^= x13.wrapping_add(x12).rotate_left(13);
        x15 ^= x14.wrapping_add(x13).rotate_left(18);
    }
    [
        x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15,
    ]
}

/// `w`'s words as little-endian bytes, laid end to end.
fn bytes_of<const N: usize, const B: usize>(w: [u32; N]) -> [u8; B] {
    let mut out = [0u8; B];
    for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(w) {
        *o = w.to_le_bytes();
    }
    out
}

/// HSalsa20: the key XSalsa20 encrypts with, of `key` and the nonce's first 16 bytes.
fn hsalsa20(key: &[u8; 32], input: &[u8; 16]) -> [u8; 32] {
    let [x0, _, _, _, _, x5, x6, x7, x8, x9, x10, _, _, _, _, x15] = rounds(state(key, input));
    bytes_of([x0, x5, x10, x15, x6, x7, x8, x9])
}

/// Salsa20's stream of `key` and the 8-byte `nonce`, from block 0, XORed into `data`.
fn salsa20_xor(key: &[u8; 32], nonce: &[u8; 8], data: &mut [u8]) {
    for (block, chunk) in (0u64..).zip(data.chunks_mut(64)) {
        let mut input = [0u8; 16];
        let (n, counter) = input.split_at_mut(8);
        n.copy_from_slice(nonce);
        counter.copy_from_slice(&block.to_le_bytes());
        let start = state(key, &input);
        let mut words = rounds(start);
        for (w, s) in words.iter_mut().zip(start) {
            *w = w.wrapping_add(s);
        }
        let stream: [u8; 64] = bytes_of(words);
        for (b, s) in chunk.iter_mut().zip(stream) {
            *b ^= s;
        }
    }
}

/// XSalsa20's stream of `key` and `nonce` XORed into `data`: what Go's secretbox takes
/// the Poly1305 key from (32 bytes of zeros) and encrypts the message with (the rest).
fn xsalsa20_xor(key: &[u8; 32], nonce: &[u8; 24], data: &mut [u8]) {
    let (first, last) = nonce.split_at(16);
    let (Ok(first), Ok(last)) = (<[u8; 16]>::try_from(first), <[u8; 8]>::try_from(last)) else {
        return;
    };
    salsa20_xor(&hsalsa20(key, &first), &last, data);
}

/// Poly1305 of `msg` under the one-time `key` (AWS-LC's).
fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    let mut state: aws_lc_sys::poly1305_state = [0u8; 512];
    let mut tag = [0u8; 16];
    // SAFETY: AWS-LC's Poly1305 over a state it aligns within its 512 bytes, a 32-byte
    // key, the message's bytes and a 16-byte tag, all of this frame.
    unsafe {
        aws_lc_sys::CRYPTO_poly1305_init(&mut state, key.as_ptr());
        aws_lc_sys::CRYPTO_poly1305_update(&mut state, msg.as_ptr(), msg.len());
        aws_lc_sys::CRYPTO_poly1305_finish(&mut state, tag.as_mut_ptr());
    }
    tag
}

/// The stream's first 32 bytes, the Poly1305 key, and the message encrypted after them.
fn stream(key: &[u8; 32], nonce: &[u8; 24], message: &[u8]) -> ([u8; 32], Vec<u8>) {
    let mut buf = vec![0u8; 32 + message.len()];
    if let Some(rest) = buf.get_mut(32..) {
        rest.copy_from_slice(message);
    }
    xsalsa20_xor(key, nonce, &mut buf);
    // `buf` holds the stream's first 32 bytes and then the message's.
    let mut one_time = [0u8; 32];
    for (k, s) in one_time.iter_mut().zip(&buf) {
        *k = *s;
    }
    let message = buf.split_off(32);
    (one_time, message)
}

/// secretbox.Seal: the tag, then `message` encrypted.
pub fn seal(message: &[u8], nonce: &[u8; 24], key: &[u8; 32]) -> Vec<u8> {
    let (one_time, ciphertext) = stream(key, nonce, message);
    let mut out = poly1305(&one_time, &ciphertext).to_vec();
    out.extend_from_slice(&ciphertext);
    out
}

/// secretbox.Open: the message, if `boxed` is a box `key` and `nonce` sealed; none where
/// it is shorter than its tag, or its tag is not the ciphertext's.
pub fn open(boxed: &[u8], nonce: &[u8; 24], key: &[u8; 32]) -> Option<Vec<u8>> {
    let (tag, ciphertext) = boxed.split_at_checked(OVERHEAD)?;
    let (one_time, _) = stream(key, nonce, &[]);
    let want = poly1305(&one_time, ciphertext);
    // SAFETY: CRYPTO_memcmp of two 16-byte arrays, in constant time.
    if unsafe { aws_lc_sys::CRYPTO_memcmp(want.as_ptr().cast(), tag.as_ptr().cast(), OVERHEAD) } != 0 {
        return None;
    }
    let (_, message) = stream(key, nonce, ciphertext);
    Some(message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        crate::tlog::gocodec::hex_decode(s.as_bytes()).unwrap()
    }

    /// "Cryptography in NaCl" §9's first box: `firstkey`, its nonce and 131-byte message,
    /// whose box is the 16-byte tag and the 131 bytes of `c` past its 32 zero bytes.
    #[test]
    fn the_nacl_papers_box_is_sealed_and_opened() {
        let key: [u8; 32] = hex("1b27556473e985d462cd51197a9a46c76009549eac6474f206c4ee0844f68389")
            .try_into()
            .unwrap();
        let nonce: [u8; 24] = hex("69696ee955b62b73cd62bda875fc73d68219e0036b7a0b37")
            .try_into()
            .unwrap();
        let message = hex(concat!(
            "be075fc53c81f2d5cf141316ebeb0c7b5228c52a4c62cbd44b66849b64244ffc",
            "e5ecbaaf33bd751a1ac728d45e6c61296cdc3c01233561f41db66cce314adb31",
            "0e3be8250c46f06dceea3a7fa1348057e2f6556ad6b1318a024a838f21af1fde",
            "048977eb48f59ffd4924ca1c60902e52f0a089bc76897040e082f93776384864",
            "5e0705"
        ));
        let boxed = hex(concat!(
            "f3ffc7703f9400e52a7dfb4b3d3305d9",
            "8e993b9f48681273c29650ba32fc76ce48332ea7164d96a4476fb8c531a1186a",
            "c0dfc17c98dce87b4da7f011ec48c97271d2c20f9b928fe2270d6fb863d51738",
            "b48eeee314a7cc8ab932164548e526ae90224368517acfeabd6bb3732bc0e9da",
            "99832b61ca01b6de56244a9e88d5f9b37973f622a43d14a6599b1f654cb45a74",
            "e355a5"
        ));
        assert_eq!(seal(&message, &nonce, &key), boxed);
        assert_eq!(open(&boxed, &nonce, &key).unwrap(), message);
        // Any bit changed, of the tag or the ciphertext, opens nothing.
        for at in [0, 15, 16, boxed.len() - 1] {
            let mut bad = boxed.clone();
            bad[at] ^= 1;
            assert!(open(&bad, &nonce, &key).is_none(), "byte {at}");
        }
        assert!(open(&boxed[..15], &nonce, &key).is_none());
        // An empty message is its tag alone.
        let empty = seal(&[], &nonce, &key);
        assert_eq!(empty.len(), OVERHEAD);
        assert_eq!(open(&empty, &nonce, &key).unwrap(), Vec::<u8>::new());
    }

    /// Boxes Go's x/crypto/nacl/secretbox sealed (scripts/cosign/generate), of messages
    /// from empty to past several blocks: sealed alike, and opened.
    #[test]
    fn boxes_are_gos() {
        let o: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/cosign/oracle.json")).unwrap();
        let boxes = o["secretbox"].as_array().unwrap();
        assert!(boxes.len() >= 10);
        for b in boxes {
            let key: [u8; 32] = hex(b["key"].as_str().unwrap()).try_into().unwrap();
            let nonce: [u8; 24] = hex(b["nonce"].as_str().unwrap()).try_into().unwrap();
            let message = hex(b["message"].as_str().unwrap());
            let boxed = hex(b["box"].as_str().unwrap());
            assert_eq!(seal(&message, &nonce, &key), boxed, "{} bytes", message.len());
            assert_eq!(open(&boxed, &nonce, &key).unwrap(), message);
        }
    }
}
