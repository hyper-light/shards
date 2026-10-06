//! Git repositories, fetched as BuildKit's git source fetches them (`ADD <git URL>`):
//! the protocol's packet lines ([`pktline`]), packfiles and their deltas ([`pack`]), and
//! the objects a checkout reads ([`object`]). Written from git's own documents
//! (Documentation/gitprotocol-common.adoc, gitprotocol-v2.adoc, gitformat-pack.adoc,
//! git v2.51.0) and its code where they leave a format to it (tree-walk.c, commit.c,
//! tag.c), so that `shards` needs no `git` beside it.

pub mod checkout;
pub mod config;
pub mod daemon;
pub mod object;
pub mod pack;
pub mod pktline;
pub mod protocol;
pub mod remote;
pub mod repo;

/// An object's name: its SHA-1, as git names objects in a SHA-1 repository.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    /// The name `hex` spells: 40 hexadecimal digits, of either case.
    pub fn parse(hex: &[u8]) -> Option<Oid> {
        let mut out = [0u8; 20];
        if hex.len() != 40 {
            return None;
        }
        let (pairs, _) = hex.as_chunks::<2>();
        for (byte, [high, low]) in out.iter_mut().zip(pairs) {
            let digit = |c: u8| char::from(c).to_digit(16).and_then(|d| u8::try_from(d).ok());
            *byte = digit(*high)? << 4 | digit(*low)?;
        }
        Some(Oid(out))
    }

    /// Its 40 lowercase hexadecimal digits.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The name of an object of `kind` holding `data`: the SHA-1 of its header and data,
    /// with collision detection; `None` for data shaped as a SHA-1 collision attack, which
    /// git refuses as well.
    pub fn of(kind: object::Kind, data: &[u8]) -> Option<Oid> {
        use sha1_checked::Digest as _;
        let mut hasher = sha1_checked::Sha1::new();
        hasher.update(kind.name());
        hasher.update(b" ");
        hasher.update(data.len().to_string());
        hasher.update(b"\0");
        hasher.update(data);
        match hasher.try_finalize() {
            sha1_checked::CollisionResult::Ok(d) => Some(Oid(d.into())),
            _ => None,
        }
    }
}

impl std::fmt::Debug for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `git hash-object` of the empty blob, and of "hello\n" (git v2.51.0).
    #[test]
    fn objects_are_named_as_git_names_them() {
        assert_eq!(
            Oid::of(object::Kind::Blob, b"").unwrap().hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            Oid::of(object::Kind::Blob, b"hello\n").unwrap().hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        let oid = Oid::parse(b"CE013625030ba8dba906f756967f9e9ca394464a").unwrap();
        assert_eq!(oid.hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(Oid::parse(b"ce01"), None);
        assert_eq!(Oid::parse(&[b'g'; 40]), None);
    }
}
