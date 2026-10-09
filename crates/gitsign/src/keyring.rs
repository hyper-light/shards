//! Key rings as go-crypto's openpgp.ReadKeyRing reads them (keys.go): entities of a
//! primary key, its identities and their certifications, its subkeys and their bindings,
//! each self-signature verified as it is read; an entity that fails skipped up to the next
//! primary key, as go-crypto skips one. Where go-crypto would dereference a missing
//! self-signature (and panic), this reads it as absent (D103).

use std::collections::BTreeMap;

use crate::key::PublicKey;
use crate::signature::Signature;
use crate::verify;
use crate::{Error, Packet, Reader};

/// SigType values (RFC 9580 §5.2.1).
pub const GENERIC_CERT: u8 = 0x10;
pub const PERSONA_CERT: u8 = 0x11;
pub const CASUAL_CERT: u8 = 0x12;
pub const POSITIVE_CERT: u8 = 0x13;
pub const SUBKEY_BINDING: u8 = 0x18;
pub const DIRECT_SIGNATURE: u8 = 0x1F;
pub const KEY_REVOCATION: u8 = 0x20;
pub const SUBKEY_REVOCATION: u8 = 0x28;
pub const CERTIFICATION_REVOCATION: u8 = 0x30;

/// Identity.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The user ID's octets.
    pub name: Vec<u8>,
    pub self_signature: Option<Signature>,
    pub revocations: Vec<Signature>,
}

/// Subkey.
#[derive(Debug, Clone)]
pub struct Subkey {
    pub key: PublicKey,
    pub sig: Signature,
    pub revocations: Vec<Signature>,
}

/// Entity.
#[derive(Debug, Clone)]
pub struct Entity {
    pub primary: PublicKey,
    /// By name, as go-crypto's map keys them.
    pub identities: BTreeMap<Vec<u8>, Identity>,
    pub revocations: Vec<Signature>,
    pub subkeys: Vec<Subkey>,
    pub self_signature: Option<Signature>,
}

/// What a key ring reads of a packet.
enum Item {
    Key(PublicKey),
    UserId(Vec<u8>),
    /// A user attribute or padding: read, and passed over.
    Other,
    Signature(Box<Signature>),
}

/// A packet reader with go-crypto's Unread.
struct Packets<'a> {
    reader: Reader<'a>,
    queue: Vec<Item>,
}

impl Packets<'_> {
    fn next(&mut self) -> Result<Option<Item>, Error> {
        if let Some(i) = self.queue.pop() {
            return Ok(Some(i));
        }
        Ok(match self.reader.next_packet()? {
            None => None,
            Some(Packet::Signature(s)) => Some(Item::Signature(s)),
            Some(Packet::PublicKey(k)) => Some(Item::Key(*k)),
            Some(Packet::UserId(id)) => Some(Item::UserId(id)),
            Some(Packet::UserAttribute | Packet::Padding) => Some(Item::Other),
        })
    }

    fn unread(&mut self, i: Item) {
        self.queue.push(i);
    }
}

/// shouldPreferIdentity.
fn prefer(existing: Option<&Identity>, new: &Identity) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    if existing.revocations.len() > new.revocations.len() {
        return true;
    }
    if existing.revocations.len() < new.revocations.len() {
        return false;
    }
    let Some(old) = &existing.self_signature else {
        return true;
    };
    let new_sig = new.self_signature.as_ref();
    let new_primary = new_sig.and_then(|s| s.is_primary_id) == Some(true);
    if old.is_primary_id == Some(true) && !new_primary {
        return false;
    }
    if old.is_primary_id != Some(true) && new_primary {
        return true;
    }
    new_sig.and_then(|s| s.creation_time) > old.creation_time
}

impl Entity {
    /// PrimaryIdentity: over go-crypto's map, which it walks in no order; here in name
    /// order, which decides only between identities go-crypto ranks equal.
    pub fn primary_identity(&self) -> Option<&Identity> {
        let mut best: Option<&Identity> = None;
        for id in self.identities.values() {
            if prefer(best, id) {
                best = Some(id);
            }
        }
        best
    }

    /// PrimarySelfSignature.
    pub fn primary_self_signature(&self) -> (Option<&Signature>, Option<&Identity>) {
        if self.primary.version == 6 {
            return (self.self_signature.as_ref(), None);
        }
        match self.primary_identity() {
            Some(id) => (id.self_signature.as_ref(), Some(id)),
            None => (None, None),
        }
    }
}

/// revoked: a revocation for a compromised key, or one not expired at `now`.
pub fn revoked(revocations: &[Signature], now: u32) -> bool {
    revocations
        .iter()
        .any(|r| r.revocation_reason.as_ref().is_some_and(|(code, _)| *code == 2) || !sig_expired(r, now))
}

/// Signature.SigExpired.
pub fn sig_expired(sig: &Signature, now: u32) -> bool {
    let created = sig.creation_time.unwrap_or(0);
    if created > now {
        return true;
    }
    match sig.sig_lifetime {
        None | Some(0) => false,
        Some(life) => u64::from(now) > u64::from(created) + u64::from(life),
    }
}

/// PublicKey.KeyExpired.
pub fn key_expired(key: &PublicKey, sig: &Signature, now: u32) -> bool {
    if key.created > now {
        return true;
    }
    match sig.key_lifetime {
        None | Some(0) => false,
        Some(life) => u64::from(now) > u64::from(key.created) + u64::from(life),
    }
}

/// ReadKeyRing: every entity read; one malformed or unsupported skipped to the next
/// primary key, and its error the ring's where none was read.
pub fn read_key_ring(data: &[u8]) -> Result<Vec<Entity>, Error> {
    let mut packets = Packets {
        reader: Reader::new(data),
        queue: Vec::new(),
    };
    let mut out = Vec::new();
    let mut last: Option<Error> = None;
    loop {
        match read_entity(&mut packets) {
            Ok(e) => out.push(e),
            Err(Error::Eof) => break,
            Err(e @ (Error::Unsupported(_) | Error::Structural(_))) => {
                last = Some(e);
                match to_next_public_key(&mut packets) {
                    Ok(()) => {}
                    Err(Error::Eof) => break,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    match (out.is_empty(), last) {
        (true, Some(e)) => Err(e),
        _ => Ok(out),
    }
}

/// readToNextPublicKey.
fn to_next_public_key(packets: &mut Packets<'_>) -> Result<(), Error> {
    loop {
        match packets.next() {
            Ok(None) => return Err(Error::Eof),
            // A public key's packet only, as go-crypto matches *packet.PublicKey: a
            // secret key's is passed over.
            Ok(Some(Item::Key(k))) if !k.is_subkey && !k.secret => {
                packets.unread(Item::Key(k));
                return Ok(());
            }
            Ok(Some(_)) => {}
            Err(Error::Unsupported(_)) => {}
            Err(e) => return Err(e),
        }
    }
}

/// ReadEntity.
fn read_entity(packets: &mut Packets<'_>) -> Result<Entity, Error> {
    let primary = match packets.next()? {
        None => return Err(Error::Eof),
        Some(Item::Key(k)) => k,
        Some(other) => {
            packets.unread(other);
            return Err(Error::Structural(
                "first packet was not a public/private key".into(),
            ));
        }
    };
    if !primary.can_sign() {
        return Err(Error::Structural(
            "primary key cannot be used for signatures".into(),
        ));
    }
    let mut e = Entity {
        primary,
        identities: BTreeMap::new(),
        revocations: Vec::new(),
        subkeys: Vec::new(),
        self_signature: None,
    };
    let mut revocations = Vec::new();
    let mut direct = Vec::new();
    loop {
        let item = match packets.next()? {
            None => break,
            Some(i) => i,
        };
        match item {
            Item::UserId(id) => add_user_id(&mut e, packets, id)?,
            Item::Signature(s) => {
                if s.sig_type == KEY_REVOCATION {
                    revocations.push(*s);
                } else if s.sig_type == DIRECT_SIGNATURE {
                    direct.push(*s);
                }
            }
            Item::Key(k) if !k.is_subkey => {
                packets.unread(Item::Key(k));
                break;
            }
            Item::Key(k) => add_subkey(&mut e, packets, k)?,
            Item::Other => {}
        }
    }
    if e.identities.is_empty() && e.primary.version < 6 {
        return Err(Error::Structural(format!(
            "v{} entity without any identities",
            e.primary.version
        )));
    }
    if e.primary.version == 6 {
        if direct.is_empty() {
            return Err(Error::Structural(
                "v6 entity without a valid direct-key signature".into(),
            ));
        }
        let mut main: Option<&Signature> = None;
        for d in &direct {
            if d.sig_type == DIRECT_SIGNATURE
                && verify::check_key_id_or_fingerprint(d, &e.primary)
                && main.is_none_or(|m| d.creation_time > m.creation_time)
            {
                main = Some(d);
            }
        }
        let Some(main) = main.cloned() else {
            return Err(Error::Structural(
                "no valid direct-key self-signature for v6 primary key found".into(),
            ));
        };
        if verify::direct_key_signature(&e.primary, &main).is_err() {
            return Err(Error::Structural(
                "invalid direct-key self-signature for v6 primary key".into(),
            ));
        }
        e.self_signature = Some(main);
    }
    for r in revocations {
        if verify::revocation_signature(&e.primary, &r).is_ok() {
            e.revocations.push(r);
        } else {
            return Err(Error::Structural(
                "revocation signature signed by alternate key".into(),
            ));
        }
    }
    Ok(e)
}

/// addUserID: the identity's signatures, its own verified as they come.
fn add_user_id(e: &mut Entity, packets: &mut Packets<'_>, id: Vec<u8>) -> Result<(), Error> {
    let mut identity = Identity {
        name: id.clone(),
        self_signature: None,
        revocations: Vec::new(),
    };
    loop {
        let item = match packets.next()? {
            None => break,
            Some(i) => i,
        };
        let Item::Signature(sig) = item else {
            packets.unread(item);
            break;
        };
        if ![
            GENERIC_CERT,
            PERSONA_CERT,
            CASUAL_CERT,
            POSITIVE_CERT,
            CERTIFICATION_REVOCATION,
        ]
        .contains(&sig.sig_type)
        {
            return Err(Error::Structural("user ID signature with wrong type".into()));
        }
        if verify::check_key_id_or_fingerprint(&sig, &e.primary) {
            if let Err(err) = verify::user_id_signature(&e.primary, &id, &sig) {
                return Err(Error::Structural(format!(
                    "user ID self-signature invalid: {err}"
                )));
            }
            if sig.sig_type == CERTIFICATION_REVOCATION {
                identity.revocations.push(*sig);
            } else if identity
                .self_signature
                .as_ref()
                .is_none_or(|s| sig.creation_time > s.creation_time)
            {
                identity.self_signature = Some(*sig);
            }
            e.identities.insert(id.clone(), identity.clone());
        }
    }
    Ok(())
}

/// addSubkey: the subkey's bindings and revocations, each verified.
fn add_subkey(e: &mut Entity, packets: &mut Packets<'_>, key: PublicKey) -> Result<(), Error> {
    let mut binding: Option<Signature> = None;
    let mut revocations = Vec::new();
    loop {
        let item = match packets.next() {
            Ok(None) => break,
            Ok(Some(i)) => i,
            Err(err) => return Err(Error::Structural(format!("subkey signature invalid: {err}"))),
        };
        let Item::Signature(sig) = item else {
            packets.unread(item);
            break;
        };
        if sig.sig_type != SUBKEY_BINDING && sig.sig_type != SUBKEY_REVOCATION {
            return Err(Error::Structural("subkey signature with wrong type".into()));
        }
        if let Err(err) = verify::key_signature(&e.primary, &key, &sig) {
            return Err(Error::Structural(format!("subkey signature invalid: {err}")));
        }
        if sig.sig_type == SUBKEY_REVOCATION {
            revocations.push(*sig);
        } else if binding
            .as_ref()
            .is_none_or(|b| sig.creation_time > b.creation_time)
        {
            binding = Some(*sig);
        }
    }
    let Some(sig) = binding else {
        return Err(Error::Structural(
            "subkey packet not followed by signature".into(),
        ));
    };
    e.subkeys.push(Subkey {
        key,
        sig,
        revocations,
    });
    Ok(())
}
