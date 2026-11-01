//! A shards home's identity (D180): an Ed25519 key (RFC 8032) its machine made, which TLS
//! 1.3 presents as a raw public key (RFC 7250), and the SHA-256 of that public key, which the
//! machine at the other end pins.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use sha2::{Digest as _, Sha256};

use crate::Error;

/// An Ed25519 SubjectPublicKeyInfo's DER before its 32-byte key (RFC 8410 §4).
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The SHA-256 of a public key's SubjectPublicKeyInfo: what a machine pins of another's.
pub type Fingerprint = [u8; 32];

/// A home's key pair: its private key (PKCS#8) and its public key's SubjectPublicKeyInfo.
pub struct Identity {
    pkcs8: Vec<u8>,
    spki: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    // The private key is never shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &hex(&self.fingerprint()))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A new key pair, from the operating system's random number generator.
    pub fn generate() -> Result<Identity, Error> {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| Error::new("no random bytes for a key"))?;
        Identity::from_pkcs8(pkcs8.as_ref().to_vec())
    }

    fn from_pkcs8(pkcs8: Vec<u8>) -> Result<Identity, Error> {
        let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| Error::new("not an Ed25519 key"))?;
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        Ok(Identity { pkcs8, spki })
    }

    /// The identity kept at `path`, made there first where there is none: written whole to a
    /// file of its own, readable by this user alone, and then linked into place, so that of
    /// two processes making one at once, both keep the one that was linked first.
    pub fn load_or_create(path: &Path) -> Result<Identity, Error> {
        match fs::read(path) {
            Ok(pkcs8) => return Identity::from_pkcs8(pkcs8),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::new(format!("{}: {e}", path.display()))),
        }
        let made = Identity::generate()?;
        let new = path.with_extension(format!("new-{}", std::process::id()));
        let at = |e: io::Error| Error::new(format!("{}: {e}", new.display()));
        // Its user's alone wherever the home is (shards_ipc::create_private).
        let mut file = shards_ipc::create_private(&new).map_err(at)?;
        let written = file.write_all(&made.pkcs8).and_then(|()| file.sync_all());
        drop(file);
        let linked = written.and_then(|()| fs::hard_link(&new, path));
        let _ = fs::remove_file(&new);
        match linked {
            Ok(()) => Ok(made),
            // Another process made one first: its.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let pkcs8 = fs::read(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
                Identity::from_pkcs8(pkcs8)
            }
            Err(e) => Err(Error::new(format!("{}: {e}", path.display()))),
        }
    }

    /// The private key, PKCS#8.
    pub(crate) fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The public key's SubjectPublicKeyInfo, DER: what TLS presents (RFC 7250).
    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    /// The SHA-256 of [`Identity::spki`].
    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.spki)
    }
}

/// The SHA-256 of a SubjectPublicKeyInfo.
pub fn fingerprint(spki: &[u8]) -> Fingerprint {
    Sha256::digest(spki).into()
}

/// Whether `spki` is an Ed25519 public key's SubjectPublicKeyInfo, the only kind a shards
/// home presents.
pub fn is_ed25519(spki: &[u8]) -> bool {
    spki.len() == SPKI_PREFIX.len() + 32 && spki.starts_with(&SPKI_PREFIX)
}

/// `bytes` in lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A home's identity is made once, kept private, and read back the same; its public key
    /// is an Ed25519 SubjectPublicKeyInfo, which the fingerprint is the SHA-256 of.
    #[test]
    fn an_identity_is_made_once_and_kept() {
        let dir = shards_testdir::TempDir::new("identity").unwrap();
        let path = dir.join("identity");
        let made = Identity::load_or_create(&path).unwrap();
        let again = Identity::load_or_create(&path).unwrap();
        assert_eq!(made.spki(), again.spki());
        assert!(is_ed25519(made.spki()));
        assert_eq!(made.fingerprint(), fingerprint(made.spki()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let other = Identity::generate().unwrap();
        assert_ne!(other.spki(), made.spki());
        assert!(!format!("{made:?}").contains(&hex(made.pkcs8())));
    }
}
