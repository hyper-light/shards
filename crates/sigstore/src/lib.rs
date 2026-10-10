//! Sigstore bundles verified as sigstore-go v1.2.2 verifies them for BuildKit's policy
//! helpers (D105): the bundle and Sigstore's trusted root read as protojson reads them;
//! transparency log entries (Rekor v1's signed entry timestamps, inclusion proofs and
//! checkpoints; Rekor v2's), RFC 3161 timestamps, Fulcio certificates and their
//! certificate transparency, and the signature itself, each checked as sigstore-go and
//! the Go libraries under it check them, and refused in their words.

pub mod asn1;
pub mod bundle;
pub mod cosignkey;
pub mod der;
pub mod gobase64;
pub mod godec;
pub mod gotime;
pub mod helpers;
pub mod image;
pub mod keys;
pub mod platforms;
pub mod proto;
pub mod schemas;
pub mod sct;
pub mod secretbox;
pub mod semver;
pub mod sign;
pub mod signature;
pub mod summary;
pub mod time;
pub mod tlog;
pub mod trusted_root;
pub mod tsa;
pub mod verify;
pub mod x509;
pub mod x509_constraints;

/// A verification's failure, as sigstore-go words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(s)
    }
}
