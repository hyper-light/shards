//! A share's files as its guest sees them, read and made through the share server's own
//! calls: what a microVM moved to another machine carries of its volumes (D176 part 6).
//! Each host keeps what a guest gave a file its own way: Unix the guest's owner, group and
//! mode in the server's owner attribute beside the host's own mode, and a device node the
//! host makes none of as an empty file the attribute names the kind of (D38); Windows all
//! of it in WSL's `$LX` metadata and reparse points (D197); and each its extended
//! attributes under names of its own. `read` gives a file as the server tells its guest
//! of it, and `set` and the makers keep it as the server keeps what a guest asks, so
//! that a volume moved between any two hosts shows its guest there what it showed it here.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

use super::*;

/// A file as a share's guest sees it, beyond its data and a symlink's target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// Its kind and permission bits, Linux's `mode_t`'s.
    pub mode: u32,
    /// A device node's number, Linux's encoding (new_encode_dev); 0 for any other node.
    pub rdev: u32,
    /// Its owner and group, where the server recorded them; none for a file the host made,
    /// which a guest sees as its share's default owner's (D38).
    pub owner: Option<(u32, u32)>,
    /// Its extended attributes by the guest's names, the server's own left out.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Linux's kinds of a node, `Meta::mode`'s.
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFBLK: u32 = 0o060_000;
pub const S_IFSOCK: u32 = 0o140_000;

/// The guest's name of a host's directory entry `host`: on Windows NTFS's, each character
/// WSL keeps escaped its own again; on Unix its own bytes.
pub fn name(host: &OsStr) -> io::Result<Vec<u8>> {
    guest_name_of(host).map(CString::into_bytes).map_err(io_error)
}

/// The host's name of a guest's name `guest`, one component: on Windows each character no
/// Windows name holds escaped as WSL escapes it; on Unix its own bytes.
pub fn host_name(guest: &[u8]) -> io::Result<OsString> {
    host_name_of(guest).map_err(io_error)
}

/// `path`'s directory, opened, and its name there as the server names it.
fn place(path: &Path) -> io::Result<(host::Owned, CString)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other(format!("{}: no file's name", path.display())));
    };
    let dir = open_dir(parent).map_err(io_error)?;
    Ok((dir, guest_name_of(name).map_err(io_error)?))
}

/// `name` in `dir` opened for its attributes, as the server opens a node: none of a node
/// the server holds no descriptor of, which keeps no attribute here (a device node on
/// Unix, a symlink on Linux, which opens none, a socket).
fn attributes_of(dir: host::Raw, name: &CStr, kind: u32) -> io::Result<Option<NodeFd>> {
    if kind == S_IFSOCK {
        return Ok(None);
    }
    match open_meta(dir, name, Some(kind)) {
        Ok(fd) => Ok(Some(fd)),
        Err(ENODATA | ELOOP) => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

/// `fd`'s attribute `host`, or none, also where its host keeps none of a node of its kind
/// (EOPNOTSUPP).
fn get(fd: &NodeFd, host: &CStr) -> io::Result<Option<Vec<u8>>> {
    match fget_xattr(fd.raw(), host) {
        Ok(v) => Ok(v),
        Err(ENODATA | EOPNOTSUPP) => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

/// `path`, never followed, as the server tells its guest of it (`Reply::attr`): its kind,
/// the guest's where the host keeps another in its place; its permission bits, the
/// recorded ones over the host's; its recorded owner; a device's number; and its extended
/// attributes as the guest lists and reads them.
pub fn read(path: &Path) -> io::Result<Meta> {
    let (dir, name) = place(path)?;
    let st = stat_at(dir.raw(), &name).map_err(io_error)?;
    let host_kind = mode_of(&st) & S_IFMT;
    let fd = attributes_of(dir.raw(), &name, host_kind)?;
    let record = match &fd {
        Some(fd) => get(fd, &owner_xattr().map_err(io_error)?)?.and_then(|v| Owner::parse(&v)),
        None => None,
    };
    let perm = record.and_then(|o| o.mode).unwrap_or(mode_of(&st) & 0o7777);
    let emulated = record.and_then(|o| o.kind).filter(|_| host_kind == S_IFREG);
    let kind = emulated.unwrap_or(host_kind);
    let rdev = match emulated {
        Some(_) => record.and_then(|o| o.rdev).unwrap_or(0),
        None if matches!(kind, S_IFCHR | S_IFBLK) => host::encode_dev(st.rdev),
        None => 0,
    };
    let mut xattrs = Vec::new();
    if let Some(fd) = &fd {
        let names = match list_xattr(fd.raw()) {
            Ok(names) => names,
            Err(EOPNOTSUPP) => Vec::new(),
            Err(e) => return Err(io_error(e)),
        };
        for guest in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
            let guest = CString::new(guest).map_err(|_| io_error(EINVAL))?;
            if hidden(&guest) {
                continue;
            }
            let Ok(host) = host_xattr(&guest) else { continue };
            if let Some(value) = get(fd, &host)? {
                xattrs.push((guest.into_bytes(), value));
            }
        }
    }
    Ok(Meta {
        mode: kind | perm,
        rdev,
        owner: record.map(|o| (o.uid, o.gid)),
        xattrs,
    })
}

/// What the symlink `path` holds, as the guest reads it (READLINK): on Windows a WSL
/// symlink's target, or a host's NTFS symlink's or junction's as the server turns it.
pub fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    let (dir, name) = place(path)?;
    readlink_at(dir.raw(), &name).map_err(io_error)
}

/// A directory at `path`, as the server makes the guest's (MKDIR): on Windows
/// case-sensitive, as WSL makes its own. Its owner's alone until [`set`] gives it its mode,
/// once all it holds is in it. One there already is kept.
pub fn make_dir(path: &Path) -> io::Result<()> {
    let (dir, name) = place(path)?;
    make_dir_in(dir.raw(), &name)
}

fn make_dir_in(dir: host::Raw, name: &CStr) -> io::Result<()> {
    match mkdir_at(dir, name, 0o700) {
        Ok(()) => Ok(()),
        Err(EEXIST) if stat_at(dir, name).is_ok_and(|st| mode_of(&st) & S_IFMT == S_IFDIR) => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

/// A symlink at `path` holding `target`, as the server makes the guest's (SYMLINK): on
/// Windows WSL's, which an unelevated process makes.
pub fn make_link(path: &Path, target: &[u8]) -> io::Result<()> {
    let (dir, name) = place(path)?;
    make_link_in(dir.raw(), &name, target)
}

fn make_link_in(dir: host::Raw, name: &CStr, target: &[u8]) -> io::Result<()> {
    let target = CString::new(target).map_err(|_| io_error(EINVAL))?;
    symlink_at(&target, dir, name).map_err(io_error)
}

/// A FIFO, socket or device node at `path` of `meta`'s kind and number, as the server makes
/// the guest's (MKNOD): the host's own where it makes one for this process; else, where it
/// makes one only for a privileged process, an empty file of mode 0600, which [`set`]
/// records the guest's kind and number beside (D38).
pub fn make_node(path: &Path, meta: &Meta) -> io::Result<()> {
    let (dir, name) = place(path)?;
    make_node_in(dir.raw(), &name, meta)
}

fn make_node_in(dir: host::Raw, name: &CStr, meta: &Meta) -> io::Result<()> {
    let kind = meta.mode & S_IFMT;
    if !matches!(kind, S_IFIFO | S_IFSOCK | S_IFCHR | S_IFBLK) {
        return Err(io_error(EINVAL));
    }
    let perm = meta.mode & 0o7777;
    let on_host = if matches!(kind, S_IFCHR | S_IFBLK) {
        perm
    } else {
        host_perm(perm, false)
    };
    match mknod_at(dir, name, kind | on_host, meta.rdev) {
        Ok(()) => Ok(()),
        Err(EPERM) => open_at(dir, name, host::O_CREAT | host::O_EXCL | host::O_WRONLY, 0o600)
            .map(drop)
            .map_err(io_error),
        Err(e) => Err(io_error(e)),
    }
}

/// `meta` given to the file at `path`, of its kind (made by the makers above, or a regular
/// file of its data), as the server keeps what a guest gives a file: its extended
/// attributes; its owner recorded where `meta` has one, or where the host keeps an empty
/// file in the place of its kind; then its mode, the host's own given its owner's access
/// where its owner is recorded and the host's modes refuse opens (`host_perm`). A symlink's
/// mode is none to set, and a node the host keeps no attributes of (Linux's symlinks and
/// special files, a host's own device node) is left without them, as the server leaves
/// one a guest makes.
pub fn set(path: &Path, meta: &Meta) -> io::Result<()> {
    let (dir, name) = place(path)?;
    set_in(dir.raw(), &name, meta, &path.display())
}

/// [`set`] of `name` in `dir`, `shown` as its errors name it.
fn set_in(dir: host::Raw, name: &CStr, meta: &Meta, shown: &dyn std::fmt::Display) -> io::Result<()> {
    let st = stat_at(dir, name).map_err(io_error)?;
    let host_kind = mode_of(&st) & S_IFMT;
    let kind = meta.mode & S_IFMT;
    let perm = meta.mode & 0o7777;
    let emulated = host_kind == S_IFREG && matches!(kind, S_IFIFO | S_IFSOCK | S_IFCHR | S_IFBLK);
    // Where the host keeps none of a node of its kind, as the server's guest finds.
    let must = matches!(host_kind, S_IFREG | S_IFDIR);
    let kept = |r: Result<(), Errno>| match r {
        Ok(()) => Ok(true),
        Err(EPERM | EOPNOTSUPP) if !must => Ok(false),
        Err(e) => Err(io_error(e)),
    };
    let fd = attributes_of(dir, name, host_kind)?;
    let mut recorded = false;
    if let Some(fd) = &fd {
        for (guest, value) in &meta.xattrs {
            let guest = CString::new(guest.as_slice()).map_err(|_| io_error(EINVAL))?;
            if hidden(&guest) {
                return Err(io_error(EINVAL));
            }
            let host = host_xattr(&guest).map_err(io_error)?;
            kept(fset_xattr(fd.raw(), &host, value, 0))?;
        }
        if meta.owner.is_some() || emulated {
            let (uid, gid) = meta.owner.unwrap_or_default();
            let owner = Owner {
                uid,
                gid,
                mode: Some(perm),
                kind: emulated.then_some(kind),
                rdev: emulated.then_some(meta.rdev),
            };
            let value = owner.encode();
            recorded = kept(fset_xattr(
                fd.raw(),
                &owner_xattr().map_err(io_error)?,
                value.as_bytes(),
                0,
            ))?;
        }
    }
    if emulated && !recorded {
        return Err(io::Error::other(format!(
            "{shown}: the host keeps no record of a node of kind {kind:o}"
        )));
    }
    if host_kind == S_IFLNK {
        return Ok(());
    }
    let host = if !MODES_REFUSE_OPENS {
        perm
    } else if emulated {
        0o600
    } else if recorded {
        host_perm(perm, host_kind == S_IFDIR)
    } else {
        perm
    };
    chmod_at(dir, name, host).map_err(io_error)
}

/// A directory held open, beneath which a move puts what another machine's stream holds
/// (D176 part 6): each entry's directories reached from it a component at a time, as the
/// server reaches a guest's, none of them through a symlink or a reparse point (O_NOFOLLOW;
/// NT's FILE_OPEN_REPARSE_POINT), each checked a directory once opened; and each entry made
/// in its own directory by its name, never over anything there, and given what its guest
/// saw there. A symlink an earlier entry made is data, never a way to anywhere: what a
/// stream names below one is refused (ELOOP, ENOTDIR). Names are a host's components, as
/// [`host_name`] gives them.
#[derive(Debug)]
pub struct Beneath(host::Owned);

impl Beneath {
    /// The directory at `root`, held.
    pub fn open(root: &Path) -> io::Result<Beneath> {
        open_dir(root).map(Beneath).map_err(io_error)
    }

    /// The directory that holds `rel`'s last component, reached from this one, each one not
    /// there made where `make`; and that component as the server names it.
    fn place(&self, rel: &[&OsStr], make: bool) -> io::Result<(Option<host::Owned>, CString)> {
        let Some((last, dirs)) = rel.split_last() else {
            return Err(io_error(EINVAL));
        };
        let mut held: Option<host::Owned> = None;
        for part in dirs {
            let at = self.at(held.as_ref());
            let name = guest_name_of(part).map_err(io_error)?;
            held = Some(step(at, &name, make).map_err(|e| match e {
                ELOOP | ENOTDIR => io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!(
                        "{} is no directory of its own (a symlink, never followed here, or a file)",
                        Path::new(part).display()
                    ),
                ),
                e => io_error(e),
            })?);
        }
        Ok((held, guest_name_of(last).map_err(io_error)?))
    }

    /// `held`, or this directory where `place` reached none below it.
    fn at(&self, held: Option<&host::Owned>) -> host::Raw {
        held.map_or(self.0.raw(), host::AsRaw::raw)
    }

    /// A directory at `rel` ([`make_dir`]).
    pub fn make_dir(&self, rel: &[&OsStr]) -> io::Result<()> {
        let (dir, name) = self.place(rel, true)?;
        make_dir_in(self.at(dir.as_ref()), &name)
    }

    /// A symlink at `rel` holding `target` ([`make_link`]), never followed here.
    pub fn make_link(&self, rel: &[&OsStr], target: &[u8]) -> io::Result<()> {
        let (dir, name) = self.place(rel, true)?;
        make_link_in(self.at(dir.as_ref()), &name, target)
    }

    /// A FIFO, socket or device node at `rel` ([`make_node`]).
    pub fn make_node(&self, rel: &[&OsStr], meta: &Meta) -> io::Result<()> {
        let (dir, name) = self.place(rel, true)?;
        make_node_in(self.at(dir.as_ref()), &name, meta)
    }

    /// A regular file at `rel`, new, its owner's alone (mode 0600), open to read and write.
    pub fn make_file(&self, rel: &[&OsStr]) -> io::Result<std::fs::File> {
        let (dir, name) = self.place(rel, true)?;
        let flags = host::O_RDWR | host::O_CREAT | host::O_EXCL | host::O_NOFOLLOW;
        let fd = open_at(self.at(dir.as_ref()), &name, flags, 0o600).map_err(io_error)?;
        Ok(std::fs::File::from(fd))
    }

    /// `meta` given to the file at `rel` ([`set`]), its directories all there already.
    pub fn set(&self, rel: &[&OsStr], meta: &Meta) -> io::Result<()> {
        let (dir, name) = self.place(rel, false)?;
        let shown = std::path::PathBuf::from_iter(rel);
        set_in(self.at(dir.as_ref()), &name, meta, &shown.display())
    }
}

/// The directory `name` in `dir`, made where it is not there and `make` (as
/// `create_dir_all` makes one, the process's umask its mode's), never followed if a
/// symlink, and refused if what opens is no directory (a reparse point NT opens as itself).
fn step(dir: host::Raw, name: &CStr, make: bool) -> Result<host::Owned, Errno> {
    let open = || {
        open_at(
            dir,
            name,
            host::O_RDONLY | host::O_DIRECTORY | host::O_NOFOLLOW,
            0,
        )
    };
    let fd = match open() {
        Err(ENOENT) if make => match mkdir_at(dir, name, 0o777) {
            Ok(()) | Err(EEXIST) => open()?,
            Err(e) => return Err(e),
        },
        opened => opened?,
    };
    if mode_of(&stat_fd(fd.raw())?) & S_IFMT != S_IFDIR {
        return Err(ENOTDIR);
    }
    Ok(fd)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A guest's names turn to the host's and back whole: on Windows a character no NTFS
    /// name holds escaped as WSL escapes it, on Unix each byte its own.
    #[test]
    fn a_guests_names_are_the_hosts_and_back() {
        for guest in [&b"plain"[..], b"a:b", b"what?", b"x*y|z", b"caf\xc3\xa9"] {
            let host = host_name(guest).unwrap();
            assert_eq!(name(&host).unwrap(), guest, "{host:?}");
            #[cfg(unix)]
            assert_eq!(std::os::unix::ffi::OsStrExt::as_bytes(host.as_os_str()), guest);
        }
        #[cfg(windows)]
        assert_ne!(host_name(b"a:b").unwrap(), OsString::from("a:b"));
        assert!(host_name(b"a\0b").is_err());
    }

    /// A file's metadata given by [`set`] reads back as given: a regular file's and a
    /// directory's owner, mode and attributes, the mode a guest gave kept apart from the
    /// host's own access (`0` here, the host's file still its owner's to reach on Unix);
    /// a device node's kind and number, where the host makes none; a symlink's target; and
    /// a file whose owner is not recorded stays so.
    #[test]
    fn what_is_set_reads_back() {
        let root = shards_testdir::TempDir::new("guest-meta").unwrap();
        let file = root.join("f");
        std::fs::write(&file, "data").unwrap();
        let given = Meta {
            mode: S_IFREG,
            rdev: 0,
            owner: Some((1234, 5678)),
            xattrs: vec![
                (b"trusted.t".to_vec(), b"w".to_vec()),
                (b"user.k".to_vec(), b"v".to_vec()),
            ],
        };
        set(&file, &given).unwrap();
        let mut got = read(&file).unwrap();
        got.xattrs.sort();
        assert_eq!(got, given);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"data",
            "the share process still reads it"
        );

        let dir = root.join("d");
        make_dir(&dir).unwrap();
        make_dir(&dir).unwrap();
        let unrecorded = Meta {
            mode: S_IFDIR | 0o750,
            ..Meta::default()
        };
        set(&dir, &unrecorded).unwrap();
        assert_eq!(read(&dir).unwrap(), unrecorded);

        let device = root.join("null");
        let null = Meta {
            mode: S_IFCHR | 0o620,
            rdev: (1 << 8) | 3,
            owner: Some((0, 5)),
            xattrs: Vec::new(),
        };
        make_node(&device, &null).unwrap();
        set(&device, &null).unwrap();
        let seen = read(&device).unwrap();
        assert_eq!((seen.mode, seen.rdev), (null.mode, null.rdev));

        let link = root.join("l");
        make_link(&link, b"f").unwrap();
        set(
            &link,
            &Meta {
                mode: S_IFLNK | 0o777,
                ..Meta::default()
            },
        )
        .unwrap();
        assert_eq!(read_link(&link).unwrap(), b"f");
        assert_eq!(read(&link).unwrap().mode & S_IFMT, S_IFLNK);
    }

    /// [`Beneath`] makes each entry below its root through directories alone, those
    /// missing on the way made: a symlink is made and kept as data, and nothing named below
    /// it, or below a file, is made, whether the link leads out of the root or not; a file
    /// is never made over one there.
    #[test]
    fn beneath_goes_through_no_symlink() {
        fn rel(path: &str) -> Vec<&OsStr> {
            path.split('/').map(OsStr::new).collect()
        }
        let root = shards_testdir::TempDir::new("guest-beneath").unwrap();
        let (held, away) = (root.join("held"), root.join("away"));
        std::fs::create_dir(&held).unwrap();
        std::fs::create_dir(&away).unwrap();
        let b = Beneath::open(&held).unwrap();
        b.make_dir(&rel("a/b")).unwrap();
        assert!(held.join("a").join("b").is_dir());
        b.make_link(&rel("a/out"), b"../../away").unwrap();
        b.make_link(&rel("a/in"), b"b").unwrap();
        let link = Meta {
            mode: S_IFLNK | 0o777,
            ..Meta::default()
        };
        b.set(&rel("a/out"), &link).unwrap();
        drop(b.make_file(&rel("a/f")).unwrap());
        assert!(b.make_file(&rel("a/f")).is_err(), "made over a file there");
        let fifo = Meta {
            mode: S_IFIFO | 0o600,
            ..Meta::default()
        };
        for below in ["a/out", "a/in", "a/f"] {
            let (f, d, l, p) = (
                format!("{below}/f"),
                format!("{below}/d/e"),
                format!("{below}/l"),
                format!("{below}/p"),
            );
            let refused = b.make_file(&rel(&f)).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::NotADirectory, "{below}: {refused}");
            assert!(b.make_dir(&rel(&d)).is_err(), "{below}");
            assert!(b.make_link(&rel(&l), b"/").is_err(), "{below}");
            assert!(b.make_node(&rel(&p), &fifo).is_err(), "{below}");
            assert!(b.set(&rel(&f), &link).is_err(), "{below}");
        }
        assert_eq!(std::fs::read_dir(&away).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(held.join("a").join("b")).unwrap().count(), 0);
        assert_eq!(read_link(&held.join("a").join("out")).unwrap(), b"../../away");
    }
}
