//! What reading a key ring costs grows with the ring, not with its square: a user ID
//! with n certifications of its own (each verified) is read in allocations linear in n.
//! go-crypto keeps the identity it builds by pointer; a copy of it at each signature
//! (as shards once made) costs n²/2 copies of the signatures before it. Counted by an
//! allocator that sees only the thread that asks: its own binary, so no other test's
//! allocator is this.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use aws_lc_rs::digest;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use shards_gitsign::keyring;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: Counting = Counting;

/// `f`'s result, and the allocations it made on this thread.
fn counted<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(Cell::get))
}

/// A new-format packet of `tag` (RFC 9580 §4.2.1).
fn packet(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0xC0 | tag];
    match body.len() {
        n if n < 192 => out.push(n as u8),
        n => {
            let n = n - 192;
            out.extend_from_slice(&[(n >> 8) as u8 + 192, n as u8]);
        }
    }
    out.extend_from_slice(body);
    out
}

/// An MPI of `b`, its leading zero octets dropped.
fn mpi(b: &[u8]) -> Vec<u8> {
    let b = &b[b.iter().take_while(|&&x| x == 0).count()..];
    let bits = b.first().map_or(0, |f| 8 * b.len() - f.leading_zeros() as usize);
    let mut out = (bits as u16).to_be_bytes().to_vec();
    out.extend_from_slice(b);
    out
}

/// A v4 EdDSA (legacy) key with one user ID and `n` certification revocations of its
/// own, each signed over SHA-256 as RFC 9580 §5.2.4 hashes a certification.
fn ring(n: usize) -> Vec<u8> {
    let pair = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
    let mut key = vec![4, 0x60, 0, 0, 0, 22, 9];
    key.extend_from_slice(&[0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01]);
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
        let created = 0x6000_0000u32 + i as u32;
        let mut hashed = vec![5, 2];
        hashed.extend_from_slice(&created.to_be_bytes());
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

#[test]
fn a_user_ids_certifications_are_read_in_allocations_linear_in_them() {
    let read = |n: usize| {
        let data = ring(n);
        let (entities, allocs) = counted(|| keyring::read_key_ring(&data).unwrap());
        let identity = entities[0].identities.values().next().unwrap();
        assert_eq!(
            identity.revocations.len(),
            n,
            "every revocation verified and kept"
        );
        allocs
    };
    let (small, large) = (read(200), read(400));
    // Twice the signatures, about twice the allocations; n²/2 copies make it four times.
    assert!(
        large * 10 < small * 25,
        "allocations for 200 signatures {small}, for 400 {large}"
    );
}
