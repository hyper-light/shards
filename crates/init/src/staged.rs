//! What an orchestrator stages into a member before its program runs (D231): variables
//! over its image's, its working directory, its run-time arguments (D256), and regular files
//! under its home, where its `HOME` lies (domains.rs): the scratch tmpfs init makes for it
//! (`HOME=/tmp`), or a writable volume given it. The host checked each entry as it made it;
//! each is read and checked again here, from the guest's own copy (D109: every check at both
//! ends).

use std::ffi::CString;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use shards_abi::run::members;

/// Refuses an entry naming a member this run starts no program for: one the image does
/// not declare, or one of files alone, which nothing would ever read it in.
pub fn check(setup: &[Vec<u8>], starts: impl Fn(u32) -> bool) -> Result<(), String> {
    let staged = members::staged_members(setup).map_err(|e| format!("a staging entry: {e}"))?;
    match staged.into_iter().find(|&m| !starts(m)) {
        Some(m) => Err(format!(
            "something is staged into member {m}, which this image starts no program for"
        )),
        None => Ok(()),
    }
}

/// Member `member`'s environment: its image's (`image`), each variable staged for it
/// replacing the image's of the same key in place, or added after them, in the order
/// staged; a key staged twice keeps its last.
pub fn env(image: &[CString], member: u32, setup: &[Vec<u8>]) -> Result<Vec<CString>, String> {
    let staged = members::envs_of(setup, member).map_err(|e| format!("a staged variable: {e}"))?;
    let mut out: Vec<CString> = image.to_vec();
    for e in staged {
        let variable = CString::new(e.variable).map_err(|_| "a staged variable holding a NUL".to_string())?;
        let key = e.key();
        let same = |c: &CString| c.as_bytes().split(|&b| b == b'=').next() == Some(key);
        match out.iter_mut().find(|c| same(c)) {
            Some(slot) => *slot = variable,
            None => out.push(variable),
        }
    }
    Ok(out)
}

/// Member `member`'s working directory, where its run names one: the project path it works
/// in, absolute and plain, checked again here as the host checked it; one at most.
pub fn workdir(member: u32, setup: &[Vec<u8>]) -> Result<Option<CString>, String> {
    members::workdir_of(setup, member)
        .map_err(|e| format!("a staged working directory: {e}"))?
        .map(|w| CString::new(w).map_err(|_| "a staged working directory holding a NUL".to_string()))
        .transpose()
}

/// Member `member`'s run-time arguments (D256), in the order staged, checked again here as
/// the host checked them; none where its run stages none.
pub fn args(member: u32, setup: &[Vec<u8>]) -> Result<Vec<CString>, String> {
    members::args_of(setup, member)
        .map_err(|e| format!("a staged argument: {e}"))?
        .into_iter()
        .map(|a| CString::new(a).map_err(|_| "a staged argument holding a NUL".to_string()))
        .collect()
}

/// A mount a member may write as it starts, as init holds it: its scratch's descriptor, or
/// the directory of init's own mount of a volume given it, opened only where files are
/// staged.
#[derive(Debug, Clone, Copy)]
pub enum Mount<'a> {
    Held(BorrowedFd<'a>),
    At(&'a str),
}

/// A mount in the member's visible filesystem, including read-only covers and
/// image-resolved aliases. `mount` is init's backing directory, not a guest pathname.
#[derive(Debug, Clone, Copy)]
pub struct MountPoint<'a> {
    pub path: &'a str,
    pub mount: Mount<'a>,
    pub read_only: bool,
}

/// Writes the files staged for `member` beneath its home, `home`, each owned by `owner`
/// (uid, gid) with its mode: the directories on the way, its home's own among them, made
/// 0700 and owned so where they are missing; nothing followed that a link could redirect;
/// a file there before (a run's, on a volume that outlives it) replaced, as nothing of the
/// member runs yet to read it; a path staged twice refused. Every final target is routed
/// through the visible mount plan, with read-only covers refused before any write.
pub fn files(
    home: &str,
    mounts: &[MountPoint<'_>],
    owner: (u32, u32),
    member: u32,
    setup: &[Vec<u8>],
) -> Result<(), String> {
    let staged = members::files_of(setup, member).map_err(|e| format!("a staged file: {e}"))?;
    if staged.is_empty() {
        return Ok(());
    }
    destination(home.as_bytes(), mounts)
        .map_err(|e| format!("files are staged for member {member}, whose HOME {e}"))?;
    let mut paths = std::collections::BTreeSet::new();
    if let Some(f) = staged.iter().find(|f| !paths.insert(f.path)) {
        return Err(format!(
            "{} is staged twice into member {member}",
            String::from_utf8_lossy(f.path)
        ));
    }
    // Validate the entire plan before changing any backing volume. Paths may contain
    // non-UTF-8 bytes; routing compares components without lossy conversion.
    let targets: Vec<Vec<u8>> = staged
        .iter()
        .map(|f| [home.as_bytes(), b"/", f.path].concat())
        .collect();
    let places = targets
        .iter()
        .map(|path| {
            let place = destination(path, mounts)?;
            if place.1.is_empty() {
                return Err(DestinationRefused::MountPoint);
            }
            Ok(place)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("staging for member {member}: {e}"))?;
    for (f, (mount, parts)) in staged.iter().zip(places) {
        let (name, beneath) = parts
            .split_last()
            .ok_or_else(|| DestinationRefused::MountPoint.to_string())?;
        let held;
        let mount = match mount {
            Mount::Held(fd) => fd,
            Mount::At(at) => {
                held = std::fs::File::open(at).map_err(|e| format!("its volume at {at}: {e}"))?;
                held.as_fd()
            }
        };
        let relative = members::File { path: name, ..*f };
        write(mount, beneath, owner, &relative).map_err(|e| {
            format!(
                "staging {} into member {member}: {e}",
                String::from_utf8_lossy(f.path)
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationRefused {
    Outside,
    ReadOnly,
    MountPoint,
}

impl std::fmt::Display for DestinationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Outside => "is no place it may write",
            Self::ReadOnly => "is covered by a read-only mount",
            Self::MountPoint => "a staged file would replace a mount point",
        })
    }
}

fn destination<'p, 'm>(
    path: &'p [u8],
    mounts: &[MountPoint<'m>],
) -> Result<(Mount<'m>, Vec<&'p [u8]>), DestinationRefused> {
    let path = path.strip_prefix(b"/").ok_or(DestinationRefused::Outside)?;
    let parts: Vec<_> = path
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
        .collect();
    if parts.contains(&&b".."[..]) {
        return Err(DestinationRefused::Outside);
    }
    let (point, depth) = mounts
        .iter()
        .filter_map(|point| {
            let names: Vec<_> = point
                .path
                .as_bytes()
                .split(|&b| b == b'/')
                .filter(|p| !p.is_empty() && *p != b".")
                .collect();
            parts.starts_with(&names).then_some((point, names.len()))
        })
        .max_by_key(|(_, depth)| *depth)
        .ok_or(DestinationRefused::Outside)?;
    if point.read_only {
        return Err(DestinationRefused::ReadOnly);
    }
    Ok((
        point.mount,
        parts.get(depth..).ok_or(DestinationRefused::Outside)?.to_vec(),
    ))
}

fn write(
    mount: BorrowedFd<'_>,
    beneath: &[&[u8]],
    (uid, gid): (u32, u32),
    f: &members::File<'_>,
) -> io::Result<()> {
    let mut parts = f.path.rsplitn(2, |&b| b == b'/');
    let name = CString::new(parts.next().unwrap_or_default()).map_err(io::Error::other)?;
    let dirs = parts.next().map(|d| d.split(|&b| b == b'/'));
    let mut dir: Option<OwnedFd> = None;
    for part in beneath.iter().copied().chain(dirs.into_iter().flatten()) {
        let at = dir.as_ref().map_or(mount.as_raw_fd(), AsRawFd::as_raw_fd);
        dir = Some(directory(at, part, (uid, gid))?);
    }
    let at = dir.as_ref().map_or(mount.as_raw_fd(), AsRawFd::as_raw_fd);
    // What is there before goes: a link itself, never what it names; a directory stays, and
    // refuses the entry.
    // SAFETY: unlinkat(2) of a NUL-terminated name beneath a directory descriptor of ours.
    if unsafe { libc::unlinkat(at, name.as_ptr(), 0) } != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) {
            return Err(e);
        }
    }
    // SAFETY: openat(2) of a NUL-terminated name beneath a directory descriptor of ours;
    // O_EXCL and O_NOFOLLOW refuse anything made there since, and a link.
    let fd = unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            f.mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor openat just made, owned from here on.
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(f.bytes)?;
    // Owner first, then the mode: a chown clears set-ID bits (chown(2)), and the mode is set
    // whole, past the umask the creation took.
    // SAFETY: fchown(2) of the descriptor held.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fchmod(2) of the descriptor held.
    if unsafe { libc::fchmod(file.as_raw_fd(), f.mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The directory `name` beneath `at`, made 0700 and `owner`'s where it is missing, opened
/// refusing a link and anything but a directory. One there before keeps its owner.
fn directory(at: RawFd, name: &[u8], (uid, gid): (u32, u32)) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: mkdirat(2) of a NUL-terminated name beneath a directory descriptor of ours.
    let made = unsafe { libc::mkdirat(at, name.as_ptr(), 0o700) } == 0;
    if !made {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e);
        }
    }
    // SAFETY: openat(2) of a NUL-terminated name beneath a directory descriptor of ours,
    // refusing a link and anything but a directory.
    let fd = unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor openat just made, owned from here on.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fchown(2) of the descriptor held.
    if made && unsafe { libc::fchown(dir.as_raw_fd(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn own() -> (u32, u32) {
        // SAFETY: getuid(2) and getgid(2) cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn cstrings(list: &[&str]) -> Vec<CString> {
        list.iter().map(|s| CString::new(*s).unwrap()).collect()
    }

    /// A staged variable replaces the image's of its key in place and adds the rest in
    /// order; another member's are not its own.
    #[test]
    fn staged_variables_replace_by_key() {
        let setup = vec![
            members::env(1, b"HOME=/tmp/elsewhere").unwrap(),
            members::env(1, b"OPENAI_API_KEY=sk").unwrap(),
            members::env(2, b"NOT=mine").unwrap(),
            members::env(1, b"OPENAI_API_KEY=sk2").unwrap(),
        ];
        let image = cstrings(&["PATH=/bin", "HOME=/tmp", "SHARDS_DOMAIN=harness codex"]);
        let got = env(&image, 1, &setup).unwrap();
        assert_eq!(
            got,
            cstrings(&[
                "PATH=/bin",
                "HOME=/tmp/elsewhere",
                "SHARDS_DOMAIN=harness codex",
                "OPENAI_API_KEY=sk2"
            ])
        );
        assert_eq!(env(&image, 0, &setup).unwrap(), image);
    }

    /// A crafted entry naming init's own variable is refused in the guest too.
    #[test]
    fn the_domains_label_is_never_staged() {
        let crafted = vec![[members::ENV_KEY, b"0\0SHARDS_DOMAIN=agent x"].concat()];
        assert!(env(&cstrings(&["A=b"]), 0, &crafted).is_err());
    }

    /// A member's working directory is the one its run names, read again as the host made
    /// it; a crafted relative or escaping one, or a second, is refused here too.
    #[test]
    fn a_working_directory_is_read_again_in_the_guest() {
        let setup = vec![
            members::workdir(1, b"/Users/ada/proj").unwrap(),
            members::env(1, b"A=b").unwrap(),
        ];
        assert_eq!(
            workdir(1, &setup).unwrap(),
            Some(CString::new("/Users/ada/proj").unwrap())
        );
        assert_eq!(workdir(0, &setup).unwrap(), None);
        for crafted in [&b"1\0proj"[..], b"1\0/Users/../etc", b"1\0/Users//x"] {
            let setup = vec![[members::WORKDIR_KEY, crafted].concat()];
            assert!(workdir(1, &setup).is_err(), "{crafted:?}");
        }
        let mut twice = setup.clone();
        twice.push(members::workdir(1, b"/elsewhere").unwrap());
        assert!(
            workdir(1, &twice)
                .unwrap_err()
                .contains("second working directory")
        );
    }

    /// A member's run-time arguments (D256) are its own, in the order staged, an empty one
    /// among them, read again as the host made them; none staged, none. A crafted one holding
    /// a NUL is refused here too.
    #[test]
    fn a_members_arguments_are_read_again_in_the_guest() {
        let setup = vec![
            members::arg(1, b"--resume").unwrap(),
            members::env(1, b"A=b").unwrap(),
            members::arg(2, b"--not-mine").unwrap(),
            members::arg(1, b"").unwrap(),
            members::arg(1, b"s 1").unwrap(),
        ];
        assert_eq!(args(1, &setup).unwrap(), cstrings(&["--resume", "", "s 1"]));
        assert!(args(0, &setup).unwrap().is_empty());
        let crafted = vec![[members::ARG_KEY, b"1\0a\0b"].concat()];
        assert!(
            args(1, &crafted)
                .unwrap_err()
                .contains("an argument holding a NUL")
        );
    }

    /// Entries naming a member the run starts nothing for are refused as a whole, an
    /// argument's (D256) among them.
    #[test]
    fn staging_into_no_running_member_is_refused() {
        let setup = vec![members::env(4, b"A=b").unwrap()];
        assert!(check(&setup, |m| m == 4).is_ok());
        assert!(check(&setup, |m| m == 3).unwrap_err().contains("member 4"));
        assert!(check(&[], |_| false).is_ok());
        let argued = vec![members::arg(5, b"--resume").unwrap()];
        assert!(check(&argued, |m| m == 5).is_ok());
        assert!(check(&argued, |m| m == 4).unwrap_err().contains("member 5"));
    }

    fn files_at(at: &std::fs::File, beneath: &[&str], member: u32, setup: &[Vec<u8>]) -> Result<(), String> {
        let home = format!("/home/{}", beneath.join("/"));
        files(
            &home,
            &[MountPoint {
                path: "/home",
                mount: Mount::Held(at.as_fd()),
                read_only: false,
            }],
            own(),
            member,
            setup,
        )
    }

    #[test]
    fn a_staged_file_uses_its_visible_nested_mount() {
        let outer = shards_testdir::TempDir::new("staged-outer").unwrap();
        let inner = shards_testdir::TempDir::new("staged-inner").unwrap();
        std::fs::create_dir(outer.path().join(".config")).unwrap();
        let a = std::fs::File::open(outer.path()).unwrap();
        let b = std::fs::File::open(inner.path()).unwrap();
        let points = [
            MountPoint {
                path: "/home/h",
                mount: Mount::Held(a.as_fd()),
                read_only: false,
            },
            MountPoint {
                path: "/home/h/.config",
                mount: Mount::Held(b.as_fd()),
                read_only: false,
            },
        ];
        let setup = vec![members::file(0, 0o600, b".config/auth.json", b"public-sentinel").unwrap()];
        files("/home/h", &points, own(), 0, &setup).unwrap();
        assert_eq!(
            std::fs::read(inner.path().join("auth.json")).unwrap(),
            b"public-sentinel"
        );
        assert!(!outer.path().join(".config/auth.json").exists());
    }

    #[test]
    fn a_read_only_cover_refuses_staging_before_any_write() {
        let outer = shards_testdir::TempDir::new("staged-rw").unwrap();
        let inner = shards_testdir::TempDir::new("staged-ro").unwrap();
        let a = std::fs::File::open(outer.path()).unwrap();
        let b = std::fs::File::open(inner.path()).unwrap();
        let points = [
            MountPoint {
                path: "/home/h",
                mount: Mount::Held(a.as_fd()),
                read_only: false,
            },
            MountPoint {
                path: "/home/h/.config",
                mount: Mount::Held(b.as_fd()),
                read_only: true,
            },
        ];
        let setup = vec![
            members::file(0, 0o600, b"first", b"first").unwrap(),
            members::file(0, 0o600, b".config/auth.json", b"public-sentinel").unwrap(),
        ];
        assert!(files("/home/h", &points, own(), 0, &setup).is_err());
        assert_eq!(std::fs::read_dir(outer.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(inner.path()).unwrap().count(), 0);
        let points = [
            points[0],
            MountPoint {
                path: "/home/h",
                ..points[1]
            },
        ];
        assert!(files("/home/h", &points, own(), 0, &setup).is_err());
    }

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// Files land beneath the home at their paths, with exactly their bytes and modes, their
    /// new directories 0700; another member's are not its own.
    #[test]
    fn files_are_written_beneath_the_home() {
        let dir = shards_testdir::TempDir::new("staged").unwrap();
        let home = std::fs::File::open(dir.path()).unwrap();
        let setup = vec![
            members::file(0, 0o600, b".codex/auth.json", b"{\"k\":\"v\"}").unwrap(),
            members::file(0, 0o755, b"bin/run", b"#!/bin/sh\n").unwrap(),
            members::file(1, 0o600, b"other", b"x").unwrap(),
        ];
        files_at(&home, &[], 0, &setup).unwrap();
        let auth = dir.path().join(".codex/auth.json");
        assert_eq!(std::fs::read(&auth).unwrap(), b"{\"k\":\"v\"}");
        assert_eq!(mode(&auth), 0o600);
        assert_eq!(mode(&dir.path().join(".codex")), 0o700);
        assert_eq!(mode(&dir.path().join("bin/run")), 0o755);
        assert_eq!(std::fs::metadata(&auth).unwrap().uid(), own().0);
        assert!(!dir.path().join("other").exists());
    }

    /// A home beneath its mount (`HOME=/tmp/h`, or a directory of a volume) is made where it
    /// is missing, 0700 and the member's, and its files land in it, not beside it.
    #[test]
    fn a_home_beneath_its_mount_is_made_and_holds_its_files() {
        let dir = shards_testdir::TempDir::new("staged-beneath").unwrap();
        let mount = std::fs::File::open(dir.path()).unwrap();
        let setup = vec![members::file(2, 0o600, b".claude/.credentials.json", b"{}").unwrap()];
        files_at(&mount, &["home", "claude"], 2, &setup).unwrap();
        let home = dir.path().join("home/claude");
        assert_eq!(
            std::fs::read(home.join(".claude/.credentials.json")).unwrap(),
            b"{}"
        );
        for d in ["home", "home/claude", "home/claude/.claude"] {
            assert_eq!(mode(&dir.path().join(d)), 0o700, "{d}");
        }
        assert!(!dir.path().join(".claude").exists());
    }

    /// A file an earlier run left on a home that outlives it (a volume) is replaced whole, its
    /// directory kept as it was; a path staged twice in one run is refused, nothing of it
    /// written over.
    #[test]
    fn a_file_from_before_is_replaced_and_one_staged_twice_refused() {
        let dir = shards_testdir::TempDir::new("staged-again").unwrap();
        let home = std::fs::File::open(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        std::fs::set_permissions(dir.path().join(".codex"), std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::write(
            dir.path().join(".codex/auth.json"),
            b"an earlier run's, longer than the new",
        )
        .unwrap();
        let setup = vec![members::file(0, 0o600, b".codex/auth.json", b"new").unwrap()];
        files_at(&home, &[], 0, &setup).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(".codex/auth.json")).unwrap(),
            b"new"
        );
        assert_eq!(mode(&dir.path().join(".codex/auth.json")), 0o600);
        assert_eq!(mode(&dir.path().join(".codex")), 0o750);
        let twice = vec![
            members::file(0, 0o600, b"one", b"first").unwrap(),
            members::file(0, 0o600, b".codex/auth.json", b"again").unwrap(),
            members::file(0, 0o600, b".codex/auth.json", b"and again").unwrap(),
        ];
        let e = files_at(&home, &[], 0, &twice).unwrap_err();
        assert!(e.contains("staged twice"), "{e}");
        assert_eq!(
            std::fs::read(dir.path().join(".codex/auth.json")).unwrap(),
            b"new"
        );
        assert!(!dir.path().join("one").exists());
    }

    /// A link where a staged path's directory goes, or on the way to the home, is never
    /// followed; one where the file goes is replaced itself, what it names untouched.
    #[test]
    fn links_in_the_way_are_not_followed() {
        let dir = shards_testdir::TempDir::new("staged-links").unwrap();
        let outside = shards_testdir::TempDir::new("staged-outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("f"), dir.path().join("f")).unwrap();
        let home = std::fs::File::open(dir.path()).unwrap();
        let through = vec![members::file(0, 0o600, b"d/x", b"x").unwrap()];
        assert!(files_at(&home, &[], 0, &through).is_err());
        let beneath = vec![members::file(0, 0o600, b"x", b"x").unwrap()];
        assert!(files_at(&home, &["d"], 0, &beneath).is_err());
        let over = vec![members::file(0, 0o600, b"f", b"x").unwrap()];
        files_at(&home, &[], 0, &over).unwrap();
        assert!(std::fs::symlink_metadata(dir.path().join("f")).unwrap().is_file());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    /// A home on a volume is init's mount of it, by its path, opened only where files are
    /// staged: one that cannot be opened refuses the files, and nothing else.
    #[test]
    fn a_volume_home_is_opened_where_files_are_staged() {
        let dir = shards_testdir::TempDir::new("staged-volume").unwrap();
        let at = dir.path().to_str().unwrap();
        let setup = vec![members::file(0, 0o600, b".x", b"x").unwrap()];
        let stage = |at, member| {
            files(
                "/home/h",
                &[MountPoint {
                    path: "/home",
                    mount: Mount::At(at),
                    read_only: false,
                }],
                own(),
                member,
                &setup,
            )
        };
        stage(at, 0).unwrap();
        assert_eq!(std::fs::read(dir.path().join("h/.x")).unwrap(), b"x");
        let gone = dir.path().join("gone");
        let gone = gone.to_str().unwrap();
        assert!(stage(gone, 0).unwrap_err().contains("its volume at"));
        assert!(stage(gone, 1).is_ok());
    }

    /// Files with no home the member may write are refused, saying why; none staged needs
    /// none.
    #[test]
    fn files_need_a_home() {
        let setup = vec![members::file(0, 0o600, b"a", b"x").unwrap()];
        let none = || files("/nowhere", &[], own(), 0, &setup);
        let e = none().unwrap_err();
        assert!(
            e.contains("files are staged for member 0, whose HOME is no place it may write"),
            "{e}"
        );
        assert!(files("/nowhere", &[], own(), 1, &setup).is_ok());
    }
}
