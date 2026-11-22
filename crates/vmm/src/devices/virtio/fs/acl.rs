//! A guest's POSIX ACL as Linux keeps one, for this server's `FUSE_POSIX_ACL` (goal 92: Shards' owed item, "its own
//! server enforcing a guest's ACLs as slates now does"; slates A-302, A-314, A-378, whose rules this is).
//!
//! **Who enforces it.** Under `FUSE_POSIX_ACL` the guest's kernel checks every access against the access ACL it reads
//! with GETXATTR (virtio-fs mounts `default_permissions`), as a Linux bind mount enforces a container's `setfacl`. It
//! leaves the rest to its server (`fs/fuse/acl.c`: "Fuse userspace is responsible for updating access permissions in
//! the inode"), and this module is that rest, as Linux's `fs/posix_acl.c` does it for a disk file system:
//! - **The mode is the access ACL's owner, mask and other entries** (the owning group's where there is no mask).
//!   Setting an access ACL sets those bits of the mode, and the ACL is kept only where it says more than a mode can, a
//!   named entry or a mask ([`Acl::permission_bits`], [`Acl::exceeds_a_mode`]: `posix_acl_equiv_mode`,
//!   `posix_acl_update_mode`). A chmod changes those three entries by changing the mode: a reader is shown the kept
//!   ACL with them taken from the mode as it reads ([`Acl::showing`]: `__posix_acl_chmod_masq`), so no chmod needs to
//!   rewrite an attribute.
//! - **A directory's default ACL is what it gives what is made in it** ([`Acl::given`]: `posix_acl_create_masq`). The
//!   creator's umask is not applied under one (`FUSE_DONT_MASK` hands the server both); the new node's mode keeps
//!   only what the default grants, it takes the default's entries as its access ACL where they say more than its mode,
//!   and a new directory takes the default too.
//!
//! **Hostile input.** A value is a guest's bytes, refused whole as `posix_acl_from_xattr` and `posix_acl_valid` refuse
//! one: shorter than its header, another version, not a whole number of entries, an unknown tag, a permission outside
//! `rwx`, a named entry naming no one (`ACL_UNDEFINED_ID`), tags out of order or repeated where they may not be, or a
//! named entry without a mask. Nothing is allocated past the value's own length.

/// The attribute a file's access ACL is (`XATTR_NAME_POSIX_ACL_ACCESS`).
pub const ACCESS: &[u8] = b"system.posix_acl_access";
/// The attribute a directory's default ACL is (`XATTR_NAME_POSIX_ACL_DEFAULT`).
pub const DEFAULT: &[u8] = b"system.posix_acl_default";

/// Format: the version word every Linux ACL value starts with (`POSIX_ACL_XATTR_VERSION`).
const VERSION: u32 = 2;
/// Format: the version word's bytes.
const HEADER_BYTES: usize = 4;
/// Format: one entry's bytes (`e_tag` le16, `e_perm` le16, `e_id` le32).
const ENTRY_BYTES: usize = 8;
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
/// Format: the bits an entry may hold, `rwx`, one class of a mode.
const PERMISSIONS: u16 = 0x07;
/// Format: the id naming no one, which a named entry may not carry.
const UNDEFINED_ID: u32 = u32::MAX;
const OWNER_SHIFT: u32 = 6;
const GROUP_SHIFT: u32 = 3;
/// The three classes' bits of a mode, the part an ACL speaks for.
pub const MODE_PERMISSION_BITS: u32 = 0o777;
/// The order every ACL keeps its tags in, as `posix_acl_valid` walks them.
const TAG_ORDER: [u16; 6] = [USER_OBJ, USER, GROUP_OBJ, GROUP, MASK, OTHER];

/// Why a value is no ACL Linux would take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Another format version: `EOPNOTSUPP`, as Linux refuses it.
    Version,
    /// Malformed or invalid: `EINVAL`.
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    tag: u16,
    perm: u16,
    id: u32,
}

/// A valid ACL, its entries in Linux's order. One of no entries is the empty ACL, which a kernel reads as none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acl {
    entries: Vec<Entry>,
}

impl Acl {
    /// The ACL `value` holds, or why Linux would refuse it.
    pub fn parse(value: &[u8]) -> Result<Acl, Refusal> {
        let header = value.get(..HEADER_BYTES).ok_or(Refusal::Invalid)?;
        let version =
            u32::from_le_bytes(<[u8; HEADER_BYTES]>::try_from(header).map_err(|_| Refusal::Invalid)?);
        if version != VERSION {
            return Err(Refusal::Version);
        }
        let body = value.get(HEADER_BYTES..).unwrap_or_default();
        let (whole, torn) = body.as_chunks::<ENTRY_BYTES>();
        if !torn.is_empty() {
            return Err(Refusal::Invalid);
        }
        let entries = whole
            .iter()
            .map(entry_of)
            .collect::<Result<Vec<Entry>, Refusal>>()?;
        valid(&entries)?;
        Ok(Acl { entries })
    }

    /// Its value, its version word first.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut value =
            Vec::with_capacity(HEADER_BYTES.saturating_add(self.entries.len().saturating_mul(ENTRY_BYTES)));
        value.extend_from_slice(&VERSION.to_le_bytes());
        for e in &self.entries {
            value.extend_from_slice(&e.tag.to_le_bytes());
            value.extend_from_slice(&e.perm.to_le_bytes());
            value.extend_from_slice(&e.id.to_le_bytes());
        }
        value
    }

    /// Whether it is the empty ACL.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The mode's three classes this ACL makes: the owner's from its owner entry, the group's from its mask (the owning
    /// group's where there is none), everyone else's from its other entry (`posix_acl_equiv_mode`).
    pub fn permission_bits(&self) -> u32 {
        let class = |tag: u16| {
            self.entries
                .iter()
                .find(|e| e.tag == tag)
                .map(|e| u32::from(e.perm & PERMISSIONS))
        };
        let group = class(MASK).or_else(|| class(GROUP_OBJ)).unwrap_or(0);
        (class(USER_OBJ).unwrap_or(0) << OWNER_SHIFT) | (group << GROUP_SHIFT) | class(OTHER).unwrap_or(0)
    }

    /// Whether it says more than a mode can: a named user or group, or a mask. One that does not is the mode alone,
    /// kept as none (`posix_acl_update_mode`).
    pub fn exceeds_a_mode(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.tag, USER | GROUP | MASK))
    }

    /// Its owner, mask (or owning group) and other entries taken from `mode` (`__posix_acl_chmod_masq`): the kept ACL
    /// as a reader is shown it, a chmod being its change too.
    pub fn showing(&mut self, mode: u32) {
        let has_mask = self.entries.iter().any(|e| e.tag == MASK);
        let class = |shift: u32| u16::try_from((mode >> shift) & u32::from(PERMISSIONS)).unwrap_or(0);
        for e in &mut self.entries {
            match e.tag {
                USER_OBJ => e.perm = class(OWNER_SHIFT),
                MASK => e.perm = class(GROUP_SHIFT),
                GROUP_OBJ if !has_mask => e.perm = class(GROUP_SHIFT),
                OTHER => e.perm = class(0),
                _ => {}
            }
        }
    }

    /// What a node made with `mode` in a directory whose default ACL this is takes, in place of the umask
    /// (`posix_acl_create_masq`): its mode, keeping of the three classes only what the default's owner, mask (or owning
    /// group) and other entries grant, its set-id and sticky bits as asked; and its access ACL, the default's entries
    /// with those three cut to the mode, where they say more than the mode can.
    pub fn given(&self, mode: u32) -> (u32, Option<Acl>) {
        let mut access = self.clone();
        let mut kept = mode & MODE_PERMISSION_BITS;
        let class =
            |bits: u32, shift: u32| u16::try_from((bits >> shift) & u32::from(PERMISSIONS)).unwrap_or(0);
        let cut = |kept: &mut u32, perm: u16, shift: u32| {
            *kept &= (u32::from(perm) << shift) | !(u32::from(PERMISSIONS) << shift);
        };
        for e in &mut access.entries {
            match e.tag {
                USER_OBJ => {
                    e.perm &= class(kept, OWNER_SHIFT);
                    cut(&mut kept, e.perm, OWNER_SHIFT);
                }
                OTHER => {
                    e.perm &= class(kept, 0);
                    cut(&mut kept, e.perm, 0);
                }
                _ => {}
            }
        }
        let has_mask = access.entries.iter().any(|e| e.tag == MASK);
        let group_tag = if has_mask { MASK } else { GROUP_OBJ };
        if let Some(group) = access.entries.iter_mut().find(|e| e.tag == group_tag) {
            group.perm &= class(kept, GROUP_SHIFT);
            cut(&mut kept, group.perm, GROUP_SHIFT);
        }
        let made = (mode & !MODE_PERMISSION_BITS) | kept;
        (made, access.exceeds_a_mode().then_some(access))
    }
}

/// One entry from its eight bytes, refused for an unknown tag, a permission outside `rwx` or a named entry naming no one.
fn entry_of(bytes: &[u8; ENTRY_BYTES]) -> Result<Entry, Refusal> {
    let [t0, t1, p0, p1, i0, i1, i2, i3] = *bytes;
    let entry = Entry {
        tag: u16::from_le_bytes([t0, t1]),
        perm: u16::from_le_bytes([p0, p1]),
        id: u32::from_le_bytes([i0, i1, i2, i3]),
    };
    if rank(entry.tag).is_none() || entry.perm & !PERMISSIONS != 0 {
        return Err(Refusal::Invalid);
    }
    if matches!(entry.tag, USER | GROUP) && entry.id == UNDEFINED_ID {
        return Err(Refusal::Invalid);
    }
    Ok(entry)
}

/// A tag's place in [`TAG_ORDER`], none for any other tag.
fn rank(tag: u16) -> Option<usize> {
    TAG_ORDER.iter().position(|known| *known == tag)
}

/// Refuses what `posix_acl_valid` refuses: the owner first and once, named users after it, the owning group once,
/// named groups after it, a mask at most once and required by a named entry, everyone else last and once. The empty
/// ACL stands.
fn valid(entries: &[Entry]) -> Result<(), Refusal> {
    if entries.is_empty() {
        return Ok(());
    }
    let ordered = entries.windows(2).all(|pair| match pair {
        [first, second] => {
            let (before, after) = (rank(first.tag), rank(second.tag));
            before < after || (before == after && matches!(second.tag, USER | GROUP))
        }
        _ => true,
    });
    let has = |tag: u16| entries.iter().any(|e| e.tag == tag);
    let named = has(USER) || has(GROUP);
    if ordered && has(USER_OBJ) && has(GROUP_OBJ) && has(OTHER) && (!named || has(MASK)) {
        Ok(())
    } else {
        Err(Refusal::Invalid)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut value = VERSION.to_le_bytes().to_vec();
        for &(tag, perm, id) in entries {
            value.extend_from_slice(&tag.to_le_bytes());
            value.extend_from_slice(&perm.to_le_bytes());
            value.extend_from_slice(&id.to_le_bytes());
        }
        value
    }

    /// A named user's ACL: the owner rw-, user 1000 r--, the owning group r--, a mask r--, everyone else ---.
    fn named() -> Vec<u8> {
        acl(&[
            (USER_OBJ, 6, 0),
            (USER, 4, 1000),
            (GROUP_OBJ, 4, 0),
            (MASK, 4, 0),
            (OTHER, 0, 0),
        ])
    }

    /// goal 92 (Shards' owed item 2): a guest's ACL reads back as it was set, its mode the owner's, the mask's and
    /// everyone else's bits; one that says no more than a mode is kept as none.
    #[test]
    fn an_acl_round_trips_and_makes_its_mode() {
        let a = Acl::parse(&named()).unwrap();
        assert_eq!(a.to_bytes(), named());
        assert_eq!(a.permission_bits(), 0o640);
        assert!(a.exceeds_a_mode());
        let plain = Acl::parse(&acl(&[(USER_OBJ, 7, 0), (GROUP_OBJ, 5, 0), (OTHER, 1, 0)])).unwrap();
        assert_eq!(plain.permission_bits(), 0o751);
        assert!(!plain.exceeds_a_mode());
        assert!(Acl::parse(&acl(&[])).unwrap().is_empty());
    }

    /// What Linux refuses is refused: another version, a torn entry, an unknown tag, a permission past rwx, a named
    /// entry naming no one, tags out of order or repeated, a named entry with no mask, a missing owner.
    #[test]
    fn what_linux_refuses_is_refused() {
        let mut v3 = named();
        v3[0] = 3;
        assert_eq!(Acl::parse(&v3), Err(Refusal::Version));
        let torn = named();
        assert_eq!(Acl::parse(&torn[..torn.len() - 1]), Err(Refusal::Invalid));
        assert_eq!(Acl::parse(&[2, 0]), Err(Refusal::Invalid));
        for bad in [
            acl(&[(USER_OBJ, 6, 0), (0x40, 4, 0), (GROUP_OBJ, 4, 0), (OTHER, 0, 0)]),
            acl(&[(USER_OBJ, 8, 0), (GROUP_OBJ, 4, 0), (OTHER, 0, 0)]),
            acl(&[
                (USER_OBJ, 6, 0),
                (USER, 4, u32::MAX),
                (GROUP_OBJ, 4, 0),
                (MASK, 4, 0),
                (OTHER, 0, 0),
            ]),
            acl(&[(GROUP_OBJ, 4, 0), (USER_OBJ, 6, 0), (OTHER, 0, 0)]),
            acl(&[
                (USER_OBJ, 6, 0),
                (USER_OBJ, 6, 0),
                (GROUP_OBJ, 4, 0),
                (OTHER, 0, 0),
            ]),
            acl(&[
                (USER_OBJ, 6, 0),
                (USER, 4, 1000),
                (GROUP_OBJ, 4, 0),
                (OTHER, 0, 0),
            ]),
            acl(&[(GROUP_OBJ, 4, 0), (OTHER, 0, 0)]),
        ] {
            assert_eq!(Acl::parse(&bad), Err(Refusal::Invalid), "{bad:?}");
        }
    }

    /// A chmod is the ACL's change: the owner, the mask (not the owning group, which a mask hides) and everyone else
    /// are shown from the mode; named entries stay.
    #[test]
    fn a_chmod_is_shown_as_the_acls_change() {
        let mut a = Acl::parse(&named()).unwrap();
        a.showing(0o751);
        assert_eq!(
            a.to_bytes(),
            acl(&[
                (USER_OBJ, 7, 0),
                (USER, 4, 1000),
                (GROUP_OBJ, 4, 0),
                (MASK, 5, 0),
                (OTHER, 1, 0)
            ])
        );
        let mut plain = Acl::parse(&acl(&[(USER_OBJ, 6, 0), (GROUP_OBJ, 4, 0), (OTHER, 4, 0)])).unwrap();
        plain.showing(0o700);
        assert_eq!(plain.permission_bits(), 0o700);
    }

    /// A default ACL gives what is made under it, in place of the umask: a mode cut to what it grants, set-id bits as
    /// asked, and its entries as the new node's access ACL where they say more than the mode.
    #[test]
    fn a_default_acl_gives_what_is_made_under_it() {
        let default = Acl::parse(&acl(&[
            (USER_OBJ, 7, 0),
            (USER, 6, 1000),
            (GROUP_OBJ, 5, 0),
            (MASK, 7, 0),
            (OTHER, 0, 0),
        ]))
        .unwrap();
        let (mode, access) = default.given(0o2666);
        assert_eq!(mode, 0o2660);
        assert_eq!(
            access.unwrap().to_bytes(),
            acl(&[
                (USER_OBJ, 6, 0),
                (USER, 6, 1000),
                (GROUP_OBJ, 5, 0),
                (MASK, 6, 0),
                (OTHER, 0, 0)
            ])
        );
        let plain = Acl::parse(&acl(&[(USER_OBJ, 7, 0), (GROUP_OBJ, 5, 0), (OTHER, 5, 0)])).unwrap();
        assert_eq!(plain.given(0o777), (0o755, None));
    }
}
