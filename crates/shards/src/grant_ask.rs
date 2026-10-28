//! A VM's half of its grants (`grant`): it asks its spawner for its paths and takes what
//! it is answered with: on macOS descriptors and bookmarks, which it resolves; on Windows
//! handles and sockets its spawner put in this process.

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

use crate::grant::{Access, MAX_GRANTS, MAX_PATH, Wanted};

/// What a VM asks for, as `kind::GRANT` carries it: a big-endian u32 count, then each path
/// as its access, a big-endian u32 length and its bytes ([`crate::grant::path_bytes`]).
pub fn encode(wanted: &[Wanted]) -> Result<Vec<u8>, String> {
    if wanted.len() > MAX_GRANTS {
        return Err(format!("{} paths to grant, more than {MAX_GRANTS}", wanted.len()));
    }
    let mut out = Vec::new();
    out.extend(
        u32::try_from(wanted.len())
            .map_err(|e| e.to_string())?
            .to_be_bytes(),
    );
    for (access, path) in wanted {
        let bytes = crate::grant::path_bytes(path);
        if bytes.len() > MAX_PATH {
            return Err(format!("{}: a path too long to grant", path.display()));
        }
        out.push(code(*access));
        out.extend(
            u32::try_from(bytes.len())
                .map_err(|e| e.to_string())?
                .to_be_bytes(),
        );
        out.extend(bytes);
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
mod cf {
    use std::ffi::c_void;

    pub type CFTypeRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type CFURLRef = *const c_void;
    pub type CFDataRef = *const c_void;
    pub type CFErrorRef = *const c_void;
    pub type CFArrayRef = *const c_void;
    pub type CFIndex = isize;
    pub type Boolean = u8;

    /// CFString.h: kCFStringEncodingUTF8.
    pub const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFURLCreateByResolvingBookmarkData(
            allocator: CFAllocatorRef,
            bookmark: CFDataRef,
            options: usize,
            relative_to: CFURLRef,
            properties: CFArrayRef,
            is_stale: *mut Boolean,
            error: *mut CFErrorRef,
        ) -> CFURLRef;
        pub fn CFURLStartAccessingSecurityScopedResource(url: CFURLRef) -> Boolean;
        pub fn CFURLGetFileSystemRepresentation(
            url: CFURLRef,
            resolve_against_base: Boolean,
            buffer: *mut u8,
            max_len: CFIndex,
        ) -> Boolean;
        pub fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, len: CFIndex) -> CFDataRef;
        pub fn CFRelease(cf: CFTypeRef);
        pub fn CFStringCreateWithBytes(
            allocator: CFAllocatorRef,
            bytes: *const u8,
            len: CFIndex,
            encoding: u32,
            external: Boolean,
        ) -> CFTypeRef;
        pub fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
        pub static kCFBooleanTrue: CFTypeRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        pub fn SecTaskCreateFromSelf(allocator: CFAllocatorRef) -> CFTypeRef;
        pub fn SecTaskCopyValueForEntitlement(
            task: CFTypeRef,
            entitlement: CFTypeRef,
            error: *mut CFErrorRef,
        ) -> CFTypeRef;
    }
}

#[cfg(target_os = "macos")]
/// Whether this process is signed into App Sandbox, which then confines it from its launch
/// ("App Sandbox Entitlement", Apple), as its own signature says (SecTask.h:
/// `SecTaskCopyValueForEntitlement`).
pub fn sandboxed() -> bool {
    const KEY: &[u8] = b"com.apple.security.app-sandbox";
    // SAFETY: each object made here is checked for null and released once; the key's bytes
    // are a valid UTF-8 buffer of their length.
    unsafe {
        let task = cf::SecTaskCreateFromSelf(std::ptr::null());
        if task.is_null() {
            return false;
        }
        let key = cf::CFStringCreateWithBytes(
            std::ptr::null(),
            KEY.as_ptr(),
            KEY.len() as cf::CFIndex,
            cf::UTF8,
            0,
        );
        let value = if key.is_null() {
            std::ptr::null()
        } else {
            cf::SecTaskCopyValueForEntitlement(task, key, std::ptr::null_mut())
        };
        let yes = !value.is_null() && cf::CFEqual(value, cf::kCFBooleanTrue) != 0;
        for object in [value, key, task] {
            if !object.is_null() {
                cf::CFRelease(object);
            }
        }
        yes
    }
}

#[cfg(target_os = "macos")]
/// Resolves `bookmark`, extending this process's sandbox to what it names for as long as
/// the process lives; returns its path. The resolved URL is never released, since the
/// access is wanted for the process's life.
pub fn resolve(bookmark: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    let len = isize::try_from(bookmark.len()).map_err(|_| "a bookmark too long".to_string())?;
    // SAFETY: a buffer of `len` bytes; the data is released below.
    let data = unsafe { cf::CFDataCreate(std::ptr::null(), bookmark.as_ptr(), len) };
    if data.is_null() {
        return Err("a bookmark CoreFoundation would not take".into());
    }
    let (mut stale, mut error): (cf::Boolean, cf::CFErrorRef) = (0, std::ptr::null());
    // SAFETY: data we own; the URL, if made, is kept for the process's life (above).
    let url = unsafe {
        cf::CFURLCreateByResolvingBookmarkData(
            std::ptr::null(),
            data,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &mut stale,
            &mut error,
        )
    };
    // SAFETY: each reference is ours to release, once.
    unsafe {
        cf::CFRelease(data);
        if !error.is_null() {
            cf::CFRelease(error);
        }
    }
    if url.is_null() {
        return Err("a bookmark that resolves to nothing".into());
    }
    let mut buf = vec![0u8; MAX_PATH + 1];
    let max = isize::try_from(buf.len()).map_err(|e| e.to_string())?;
    // SAFETY: a buffer of `max` bytes, for the URL's path.
    if unsafe { cf::CFURLGetFileSystemRepresentation(url, 1, buf.as_mut_ptr(), max) } == 0 {
        return Err("a bookmark whose path is too long".into());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(&buf));
    // SAFETY: a URL we hold; the extension lasts until it is stopped, which it never is.
    // Outside a sandbox there is nothing to extend: the call says so, and the path is
    // reachable already.
    let _ = unsafe { cf::CFURLStartAccessingSecurityScopedResource(url) };
    Ok(path)
}

#[cfg(target_os = "macos")]
/// Asks the spawner on `link` for `wanted`, and takes what it answers: a file's
/// descriptor, from here on what opens of its path get; a directory's bookmark, resolved;
/// or, for a file it reads if it is there, word that it is not. A bookmark must name the
/// path asked for.
pub fn obtain(link: &std::os::unix::net::UnixStream, wanted: &[Wanted]) -> Result<(), String> {
    use shards_ipc::kind;
    shards_ipc::send(link, kind::GRANT, &encode(wanted)?, &[])
        .map_err(|e| format!("asking for access: {e}"))?;
    for (access, path) in wanted {
        let answer = shards_ipc::recv(link)
            .map_err(|e| format!("the grant for {}: {e}", path.display()))?
            .ok_or_else(|| format!("the spawner went before granting {}", path.display()))?;
        match answer.kind {
            kind::GRANTED => match (*access, answer.fds.into_iter().next(), answer.payload.is_empty()) {
                (
                    Access::Read | Access::ReadIfThere | Access::ReadNewestIfThere | Access::Write,
                    Some(fd),
                    true,
                ) => {
                    shards_vmm::platform::grant_input(path.clone(), Some(fd));
                }
                (Access::ReadIfThere | Access::ReadNewestIfThere, None, true) => {
                    shards_vmm::platform::grant_input(path.clone(), None);
                }
                (Access::Listen, Some(fd), true) => shards_vmm::platform::grant_listener(path.clone(), fd),
                (Access::MakeDir, None, false) => {
                    let resolved =
                        resolve(&answer.payload).map_err(|e| format!("{}: {e}", path.display()))?;
                    let real =
                        |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
                    if real(&resolved)? != real(path)? {
                        return Err(format!(
                            "asked for {} and was granted {}",
                            path.display(),
                            resolved.display()
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "the spawner's answer for {} was not what was asked",
                        path.display()
                    ));
                }
            },
            kind::ERR => return Err(String::from_utf8_lossy(&answer.payload).into_owned()),
            other => return Err(format!("the spawner answered message kind {other}")),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
/// A connection to vsock host port `port`, which the spawner on `link` dials for this VM
/// beside the vsock path it granted it (`kind::DIAL`): App Sandbox lets the VM dial
/// nothing outside its container (PM M67).
pub fn dial(
    link: &std::os::unix::net::UnixStream,
    port: u32,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use shards_ipc::kind;
    shards_ipc::send(link, kind::DIAL, &port.to_be_bytes(), &[])?;
    let answer = shards_ipc::recv(link)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the spawner went"))?;
    match (answer.kind, answer.fds.into_iter().next()) {
        (kind::GRANTED, Some(fd)) => {
            // Had: the spawner lets go of its copy, which held the connection open past
            // the VM's close of it (review 8.7). One not told keeps it until the VM asks
            // again, or goes.
            let _ = shards_ipc::send(link, kind::TAKEN, &[], &[]);
            Ok(std::os::unix::net::UnixStream::from(fd))
        }
        (kind::ERR, _) => Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            String::from_utf8_lossy(&answer.payload).into_owned(),
        )),
        _ => Err(std::io::Error::other(
            "the spawner answered a dial with something else",
        )),
    }
}

/// Asks the spawner on `link` for `wanted`, and takes what it answers: a file's handle and
/// the path it was found at, from here on what opens of its path get; a socket bound for
/// this VM; or, for a file it reads if it is there, word that it is not.
#[cfg(windows)]
pub fn obtain(link: &crate::grant::Link, wanted: &[Wanted]) -> Result<(), String> {
    use shards_ipc::kind;
    shards_ipc::write_message(&link.to, kind::GRANT, &encode(wanted)?)
        .map_err(|e| format!("asking for access: {e}"))?;
    for (access, path) in wanted {
        let answer = shards_ipc::read_message(&link.from)
            .map_err(|e| format!("the grant for {}: {e}", path.display()))?
            .ok_or_else(|| format!("the spawner went before granting {}", path.display()))?;
        let at = |e: String| format!("{}: {e}", path.display());
        match (answer.kind, *access) {
            (
                kind::GRANTED,
                Access::Read | Access::ReadIfThere | Access::ReadNewestIfThere | Access::Write,
            ) if !answer.payload.is_empty() => {
                let handed = handed_file(&answer.payload).map_err(at)?;
                shards_vmm::platform::grant_input(path.clone(), Some(handed), *access == Access::Write);
            }
            (kind::GRANTED, Access::ReadIfThere | Access::ReadNewestIfThere) => {
                shards_vmm::platform::grant_input(path.clone(), None, false)
            }
            (kind::GRANTED, Access::Listen) => {
                let listener = shards_vmm::platform::UnixListener::from_shared(&answer.payload)
                    .map_err(|e| at(e.to_string()))?;
                shards_vmm::platform::grant_listener(path.clone(), listener);
            }
            // Made, for the spawner to stage this VM's snapshots in ([`stage`]).
            (kind::GRANTED, Access::MakeDir) if answer.payload.is_empty() => {}
            (kind::GRANTED, _) => {
                return Err(format!(
                    "the spawner's answer for {} was not what was asked",
                    path.display()
                ));
            }
            (kind::ERR, _) => return Err(String::from_utf8_lossy(&answer.payload).into_owned()),
            (other, _) => return Err(format!("the spawner answered message kind {other}")),
        }
    }
    Ok(())
}

/// A file the spawner duplicated into this process, and the path it found it at: the
/// handle's value there (a little-endian u64), then the path ([`crate::grant::path_bytes`]).
#[cfg(windows)]
fn handed_file(payload: &[u8]) -> Result<(std::fs::File, PathBuf), String> {
    use std::os::windows::io::FromRawHandle as _;
    let (value, real) = payload
        .split_first_chunk::<8>()
        .ok_or("a file's answer cut short")?;
    let real = crate::grant::path_from(real)
        .filter(|p| p.is_absolute())
        .ok_or("a file answered with no whole path")?;
    let value = usize::try_from(u64::from_le_bytes(*value))
        .ok()
        .filter(|&v| v != 0)
        .ok_or("a file answered with no handle")?;
    // SAFETY: a handle the spawner duplicated into this process for this answer alone,
    // owned from here on.
    let file = unsafe { std::fs::File::from_raw_handle(value as std::os::windows::io::RawHandle) };
    Ok((file, real))
}

/// A connection to vsock host port `port`, which the spawner on `link` dials for this VM
/// beside the vsock path it granted it (`kind::DIAL`): an AppContainer dials nothing its
/// user's (PM M201, M203).
#[cfg(windows)]
pub fn dial(link: &crate::grant::Link, port: u32) -> std::io::Result<shards_vmm::platform::UnixStream> {
    use shards_ipc::kind;
    shards_ipc::write_message(&link.to, kind::DIAL, &port.to_be_bytes())?;
    let answer = shards_ipc::read_message(&link.from)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the spawner went"))?;
    match answer.kind {
        kind::GRANTED => {
            let conn = shards_vmm::platform::UnixStream::from_shared(&answer.payload)?;
            // Had: the spawner lets go of its copy, as on macOS (review 8.7).
            let _ = shards_ipc::write_message(&link.to, kind::TAKEN, &[]);
            Ok(conn)
        }
        kind::ERR => Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            String::from_utf8_lossy(&answer.payload).into_owned(),
        )),
        _ => Err(std::io::Error::other(
            "the spawner answered a dial with something else",
        )),
    }
}

/// A snapshot generation the spawner on `link` stages in `dir`, a directory it granted this
/// VM to write in (`kind::STAGE`, D196), with what the VM keeps beside its memory and state
/// (`kept`): its name, then its files in [`Kept::files`]'s order, handles the spawner
/// duplicated into this process.
///
/// [`Kept::files`]: shards_vmm::platform::Kept::files
#[cfg(windows)]
pub fn stage(
    link: &crate::grant::Link,
    dir: &std::path::Path,
    kept: shards_vmm::platform::Kept,
) -> std::io::Result<(String, Vec<std::fs::File>)> {
    use shards_ipc::kind;
    use std::os::windows::io::FromRawHandle as _;
    let asked = [&[kept.bits()][..], &crate::grant::path_bytes(dir)].concat();
    shards_ipc::write_message(&link.to, kind::STAGE, &asked)?;
    let answer = shards_ipc::read_message(&link.from)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the spawner went"))?;
    let refused = |why: String| std::io::Error::other(why);
    match answer.kind {
        kind::GRANTED => {
            let (handles, name) = answer
                .payload
                .split_at_checked(8 * kept.files())
                .ok_or_else(|| refused("a stage answered cut short".into()))?;
            // Every handle owned first, so that an answer refused below closes them all.
            let mut files = Vec::with_capacity(kept.files());
            let mut none = false;
            for value in handles.as_chunks::<8>().0 {
                match usize::try_from(u64::from_le_bytes(*value))
                    .ok()
                    .filter(|&v| v != 0)
                {
                    // SAFETY: a handle the spawner duplicated into this process for this
                    // generation alone, owned from here on.
                    Some(handle) => files.push(unsafe {
                        std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle)
                    }),
                    None => none = true,
                }
            }
            if none {
                return Err(refused("a stage answered with no handle".into()));
            }
            let name = std::str::from_utf8(name)
                .map_err(|_| refused("a generation named in no UTF-8".into()))?
                .to_string();
            Ok((name, files))
        }
        kind::ERR => Err(refused(String::from_utf8_lossy(&answer.payload).into_owned())),
        other => Err(refused(format!(
            "the spawner answered a stage with message kind {other}"
        ))),
    }
}

/// Has the spawner on `link` make generation `name`, which this VM filled, durable and the
/// one in use (`kind::COMMIT`, D196).
#[cfg(windows)]
pub fn commit(link: &crate::grant::Link, name: &str) -> std::io::Result<()> {
    use shards_ipc::kind;
    shards_ipc::write_message(&link.to, kind::COMMIT, name.as_bytes())?;
    let answer = shards_ipc::read_message(&link.from)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the spawner went"))?;
    match answer.kind {
        kind::GRANTED => Ok(()),
        kind::ERR => Err(std::io::Error::other(
            String::from_utf8_lossy(&answer.payload).into_owned(),
        )),
        other => Err(std::io::Error::other(format!(
            "the spawner answered a commit with message kind {other}"
        ))),
    }
}

/// An access's byte in a request.
fn code(access: Access) -> u8 {
    match access {
        Access::Read => 0,
        Access::ReadIfThere => 1,
        Access::ReadNewestIfThere => 5,
        Access::Write => 2,
        Access::MakeDir => 3,
        Access::Listen => 4,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// A test binary is not signed into App Sandbox, and the check says so; a VM process
    /// that is, says it is (the E2E tests' signed shards-vm).
    #[test]
    fn an_unsandboxed_process_says_so() {
        assert!(!super::sandboxed());
    }
}
