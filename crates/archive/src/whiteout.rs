//! Whiteouts: how an archive of a layer says a file of the layers below is gone
//! (go-archive v0.3.3 whiteouts.go). In an archive, and in the OCI image spec's layers, a
//! file `.wh.NAME` deletes NAME and `.wh..wh..opq` hides everything below its directory.
//! overlayfs keeps the same facts its own way, in an upper directory: a deleted NAME is a
//! character device 0/0, and an opaque directory has the trusted.overlay.opaque (or, in a
//! user namespace, user.overlay.opaque) attribute `y`. With [`WhiteoutFormat::Overlay`],
//! packing turns overlayfs' form into the archive's and unpacking turns it back
//! (archive_linux.go, overlayWhiteoutConverter); elsewhere than Linux it changes nothing,
//! as go-archive's archive_other.go has no converter.

use crate::error::{Error, quote};
use crate::gopath::{self, NATIVE, posix};
use crate::sys::{self, FileKind, Root, Stat};
use crate::tar::{Header, PAX_SCHILY_XATTR, TYPE_REG};

/// WhiteoutPrefix: `.wh.NAME` deletes NAME.
pub const WHITEOUT_PREFIX: &[u8] = b".wh.";
/// WhiteoutMetaPrefix: whiteouts that are not deletions.
pub const WHITEOUT_META_PREFIX: &[u8] = b".wh..wh.";
/// WhiteoutLinkDir: AUFS's directory of hard link targets shared across layers.
pub const WHITEOUT_LINK_DIR: &[u8] = b".wh..wh.plnk";
/// WhiteoutOpaqueDir: its directory hides what the layers below hold there.
pub const WHITEOUT_OPAQUE_DIR: &[u8] = b".wh..wh..opq";

/// WhiteoutFormat: the form whiteouts take on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteoutFormat {
    /// AUFSWhiteoutFormat: as in the archive, `.wh.` files; nothing is converted.
    #[default]
    Aufs,
    /// OverlayWhiteoutFormat: overlayfs' devices and opaque attributes.
    Overlay,
}

/// overlayWhiteoutConverter: the attribute that marks an opaque directory.
#[derive(Debug)]
pub(crate) struct Overlay {
    opaque: &'static [u8],
}

/// getWhiteoutConverter: overlayfs' converter on Linux, none elsewhere.
pub(crate) fn converter(format: WhiteoutFormat) -> Option<Overlay> {
    if format != WhiteoutFormat::Overlay || !cfg!(any(target_os = "linux", target_os = "android")) {
        return None;
    }
    let opaque: &[u8] = if running_in_user_ns() {
        b"user.overlay.opaque"
    } else {
        b"trusted.overlay.opaque"
    };
    Some(Overlay { opaque })
}

/// moby/sys/userns v0.1.0 RunningInUserNS (userns_linux.go): /proc/self/uid_map's first
/// line maps anything but all of 0..2^32-1 to itself.
fn running_in_user_ns() -> bool {
    let Ok(map) = std::fs::read("/proc/self/uid_map") else {
        return false;
    };
    let Some(line) = map.split(|&c| c == b'\n').next() else {
        return false;
    };
    if map.is_empty() {
        return false;
    }
    uid_map_in_user_ns(line)
}

/// uidMapInUserNS.
fn uid_map_in_user_ns(line: &[u8]) -> bool {
    if line.is_empty() {
        return true;
    }
    let fields: Vec<Option<i64>> = line
        .split(|c| c.is_ascii_whitespace())
        .filter(|f| !f.is_empty())
        .take(3)
        .map(|f| std::str::from_utf8(f).ok().and_then(|f| f.parse().ok()))
        .collect();
    match fields.as_slice() {
        [Some(a), Some(b), Some(c)] => !(*a == 0 && *b == 0 && *c == 4_294_967_295),
        _ => false,
    }
}

impl Overlay {
    /// ConvertWrite: a 0/0 character device becomes the empty file `.wh.NAME`; an opaque
    /// directory's attribute is dropped and the returned header, `.wh..wh..opq` inside it,
    /// follows it in the archive. Err where reading the attribute fails, which go-archive
    /// logs, skipping the file.
    pub(crate) fn convert_write(
        &self,
        hdr: &mut Header,
        src: &[u8],
        st: &Stat,
    ) -> Result<Option<Header>, ()> {
        if st.kind == FileKind::Char && hdr.devmajor == 0 && hdr.devminor == 0 {
            let (dir, file) = posix::split(&hdr.name);
            let name = posix::join(&[dir, &[WHITEOUT_PREFIX, file].concat()]);
            hdr.name = name;
            hdr.mode = 0o600;
            hdr.typeflag = TYPE_REG;
            hdr.size = 0;
        }
        if st.kind != FileKind::Dir {
            return Ok(None);
        }
        let opaque = sys::lgetxattr(src, self.opaque).map_err(|_| ())?;
        if opaque.as_deref() != Some(b"y") {
            return Ok(None);
        }
        hdr.pax.remove(&[PAX_SCHILY_XATTR, self.opaque].concat());
        Ok(Some(Header {
            typeflag: TYPE_REG,
            mode: hdr.mode & 0o777,
            name: posix::join(&[&hdr.name, WHITEOUT_OPAQUE_DIR]),
            size: 0,
            uid: hdr.uid,
            uname: hdr.uname.clone(),
            gid: hdr.gid,
            gname: hdr.gname.clone(),
            atime: hdr.atime,
            ctime: hdr.ctime,
            ..Header::default()
        }))
    }

    /// ConvertRead: `.wh..wh..opq` marks its directory opaque, `.wh.NAME` becomes a 0/0
    /// device NAME owned as the entry is; neither is written itself (false). Anything
    /// else is written as it is (true).
    pub(crate) fn convert_read(&self, root: &Root, hdr: &Header, dst: &[u8]) -> Result<bool, Error> {
        let base = gopath::base(NATIVE, dst);
        let dir = gopath::dir(NATIVE, dst);
        let show = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
        if base == b".wh." || base == b".wh.." || base == b".wh..." {
            return Err(Error::other(format!(
                "invalid whiteout entry {}",
                quote(&hdr.name)
            )));
        }
        if base == WHITEOUT_OPAQUE_DIR {
            root.set_xattr_dir(&dir, self.opaque, b"y")
                .map_err(|e| e.error("openat", &dir))?
                .map_err(|e| {
                    Error::other(format!(
                        "fsetxattr('{}', {}=y): {}",
                        show(&dir),
                        show(self.opaque),
                        crate::error::errno_text(&e)
                    ))
                })?;
            return Ok(false);
        }
        let Some(original) = base.strip_prefix(WHITEOUT_PREFIX) else {
            return Ok(true);
        };
        let original_path = gopath::join(NATIVE, &[&dir, original]);
        root.mknod(&dir, original, sys::S_IFCHR, 0, 0)
            .map_err(|e| e.error("openat", &dir))?
            .map_err(|e| {
                Error::other(format!(
                    "failed to mknod('{}', S_IFCHR, 0): {}",
                    show(&original_path),
                    crate::error::errno_text(&e)
                ))
            })?;
        // The device made for a whiteout owned as the entry is, unless that is root.
        if hdr.uid != 0 || hdr.gid != 0 {
            root.lchown_in(&dir, original, hdr.uid, hdr.gid)
                .map_err(|e| e.error("openat", &dir))?
                .map_err(|e| Error::path("lchown", &original_path, &e))?;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // moby/sys/userns v0.1.0 userns_linux_test.go, TestUIDMapInUserNS.
    #[test]
    fn uid_maps_as_moby() {
        assert!(!uid_map_in_user_ns(b"         0          0 4294967295\n"));
        assert!(uid_map_in_user_ns(b"         0          0          1\n"));
        assert!(uid_map_in_user_ns(
            b"         0       1001          1\n         1     231072      65536\n"
        ));
        assert!(uid_map_in_user_ns(b""));
    }
}
