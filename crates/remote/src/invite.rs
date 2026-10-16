//! Invitations (D180), as focal's nodes and clients join (../focal docs/network-startup.md):
//! a private file that names the daemon's address and key, and holds a 256-bit secret. A
//! joining client proves the secret bound to its TLS session: HMAC-SHA-256 under the secret of
//! the session's exporter (RFC 8446 §7.5), so that a proof is good for that connection alone,
//! neither replayed on another nor relayed through one, and the secret itself never crosses.

use std::fs;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path;

use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};

use crate::Error;
use crate::identity::{self, Fingerprint};

/// How long an unused invitation lasts once made, as focal's do ("one hour after its first
/// preparation"): long enough to carry a file from one machine to another, and no standing
/// credential after.
pub const LIFETIME_SECS: u64 = 3600;

/// The exporter's label: RFC 5705 §4 keeps those beginning `EXPERIMENTAL` for private use.
pub const EXPORTER_LABEL: &[u8] = b"EXPERIMENTAL-shards-join";

/// An invitation's name: 1 to 63 ASCII letters, digits, dots, underscores or hyphens, as
/// focal's are, and what the enrolled client is known by.
pub fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// An invitation to join the daemon at `address`, whose key's SHA-256 is `daemon`.
#[derive(Clone, PartialEq, Eq)]
pub struct Invitation {
    pub name: String,
    pub address: SocketAddr,
    pub daemon: Fingerprint,
    pub secret: [u8; 32],
}

impl std::fmt::Debug for Invitation {
    // The secret is never shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("daemon", &identity::hex(&self.daemon))
            .finish_non_exhaustive()
    }
}

const HEADER: &str = "shards invitation 1";

impl Invitation {
    /// A new invitation for `name`, its secret from the operating system's random number
    /// generator.
    pub fn new(name: &str, address: SocketAddr, daemon: Fingerprint) -> Result<Invitation, Error> {
        if !valid_name(name) {
            return Err(Error::new(format!(
                "{name:?}: an invitation's name is 1 to 63 letters, digits, dots, underscores or hyphens"
            )));
        }
        let mut secret = [0u8; 32];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| Error::new("no random bytes for an invitation"))?;
        Ok(Invitation {
            name: name.to_string(),
            address,
            daemon,
            secret,
        })
    }

    /// The invitation as its file holds it.
    pub fn text(&self) -> String {
        format!(
            "{HEADER}\nname {}\naddress {}\ndaemon {}\nsecret {}\n",
            self.name,
            self.address,
            identity::hex(&self.daemon),
            identity::hex(&self.secret)
        )
    }

    /// The invitation a file holds.
    pub fn parse(text: &str) -> Result<Invitation, Error> {
        let bad = |why: &str| Error::new(format!("not a shards invitation: {why}"));
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(bad("its first line"));
        }
        let mut field = |key: &str| -> Result<String, Error> {
            let line = lines.next().ok_or_else(|| bad(&format!("no {key}")))?;
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(' '))
                .map(str::to_string)
                .ok_or_else(|| bad(&format!("no {key}")))
        };
        let name = field("name")?;
        let address = field("address")?
            .parse::<SocketAddr>()
            .map_err(|_| bad("its address"))?;
        let daemon = unhex::<32>(&field("daemon")?).ok_or_else(|| bad("its daemon's key"))?;
        let secret = unhex::<32>(&field("secret")?).ok_or_else(|| bad("its secret"))?;
        if !valid_name(&name) {
            return Err(bad("its name"));
        }
        Ok(Invitation {
            name,
            address,
            daemon,
            secret,
        })
    }

    /// Writes the invitation to `path`, a new file readable by this user alone: a file there
    /// already is never overwritten, unless it holds exactly this invitation, as focal writes
    /// its invitations.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        let text = self.text();
        let at = |e: std::io::Error| Error::new(format!("{}: {e}", path.display()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(path) {
            Ok(mut file) => file
                .write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(at),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::read(path).map_err(at)? == text.as_bytes() {
                    Ok(())
                } else {
                    Err(Error::new(format!("{}: a file is there already", path.display())))
                }
            }
            Err(e) => Err(at(e)),
        }
    }
}

/// The proof a client joining with `secret` sends over the connection whose exporter gave
/// `exported` ([`EXPORTER_LABEL`], no context, 32 bytes).
pub fn proof(secret: &[u8; 32], exported: &[u8; 32]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let tag = hmac::sign(&key, exported);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Whether `proof` is [`proof`]'s of `secret` and `exported`, compared in constant time.
pub fn verify(secret: &[u8; 32], exported: &[u8; 32], proof: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, exported, proof).is_ok()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = digit(*bytes.get(i * 2)?)?;
        let lo = digit(*bytes.get(i * 2 + 1)?)?;
        *byte = hi << 4 | lo;
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// An invitation's file reads back as it was written, is private, and is never written
    /// over; its secret never shows in what it prints.
    #[test]
    fn an_invitation_is_written_private_and_read_back() {
        let dir = shards_testdir::TempDir::new("invitation").unwrap();
        let path = dir.join("gpu.invite");
        let made = Invitation::new("gpu-desktop", "192.168.1.77:7350".parse().unwrap(), [7; 32]).unwrap();
        made.write(&path).unwrap();
        made.write(&path).unwrap();
        let read = Invitation::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, made);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let other = Invitation::new("gpu-desktop", made.address, [7; 32]).unwrap();
        assert!(other.write(&path).is_err(), "written over another invitation");
        assert!(!format!("{made:?}").contains(&identity::hex(&made.secret)));
        assert!(Invitation::new("no spaces", made.address, [7; 32]).is_err());
        assert!(Invitation::parse("shards invitation 1\nname x\n").is_err());
    }

    /// A proof is good for the secret and the session it was made over alone.
    #[test]
    fn a_proof_holds_for_its_secret_and_session_alone() {
        let (secret, exported) = ([1u8; 32], [2u8; 32]);
        let good = proof(&secret, &exported);
        assert!(verify(&secret, &exported, &good));
        assert!(!verify(&[3u8; 32], &exported, &good), "another secret");
        assert!(!verify(&secret, &[4u8; 32], &good), "another session");
        assert!(!verify(&secret, &exported, &good[..31]));
    }
}
