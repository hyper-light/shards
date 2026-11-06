//! `shards run --kernel … --share DIR[:ro]`: host directories a directly booted guest sees,
//! the i-th as virtio-fs tag `shards{i}`, fixed at boot. This process serves them as the
//! daemon serves a run's (D38): it starts a share process (`shards share`, share.rs) on the
//! directories, opened here, takes a connection to it for each, and hands each to the VM
//! as a share another server serves (`--slates-fs-fd`, or on Windows a pipe given by name as
//! `--slates-fs-handle`'s are). All before the VM process starts: it opens no directory, on
//! macOS confined by App Sandbox from its first instruction (D30), on Linux by Landlock and
//! seccomp, on Windows by its AppContainer (D141), so the share process, unconfined, opens
//! them and resolves paths beneath them by the server's own rules.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// A `--share` value: its directory, and whether it is read-only (`:ro`). A value whose
/// last name holds another `:MODE` is refused: no other mode is served.
pub(super) fn parse(value: &OsStr) -> Result<(PathBuf, bool), String> {
    let bytes = value.as_encoded_bytes();
    let last = bytes
        .rsplit(|&b| b == b'/' || (cfg!(windows) && b == b'\\'))
        .next()
        .unwrap_or_default();
    let Some(colon) = last.iter().rposition(|&b| b == b':') else {
        return directory(value, false);
    };
    // A drive's colon (`C:`) is a path's, not a mode's.
    if cfg!(windows) && last.len() == bytes.len() && colon == 1 {
        return directory(value, false);
    }
    let mode = last.get(colon.saturating_add(1)..).unwrap_or_default();
    if mode != b"ro" {
        return Err(format!(
            "--share {}: {:?} is no mode: DIR or DIR:ro",
            value.display(),
            String::from_utf8_lossy(mode)
        ));
    }
    let dir = bytes.get(..bytes.len().saturating_sub(3)).unwrap_or_default();
    // SAFETY: cut just before `:ro`, ASCII, which `from_encoded_bytes_unchecked` documents as
    // a valid boundary.
    directory(unsafe { OsStr::from_encoded_bytes_unchecked(dir) }, true)
}

fn directory(dir: &OsStr, read_only: bool) -> Result<(PathBuf, bool), String> {
    if dir.is_empty() {
        return Err("--share: no directory given".into());
    }
    Ok((PathBuf::from(dir), read_only))
}

/// `args` (a `run`'s, after `run`) with each `--share` taken out, and the shares, in order.
/// Those after `--` are its command's.
pub(super) fn take(args: &[OsString]) -> Result<(Vec<OsString>, Vec<(PathBuf, bool)>), String> {
    let mut kept = Vec::with_capacity(args.len());
    let mut shares = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            kept.push(arg.clone());
            kept.extend(rest.cloned());
            break;
        }
        if arg != "--share" {
            kept.push(arg.clone());
            continue;
        }
        let value = rest.next().ok_or("--share needs a value")?;
        shares.push(parse(value)?);
    }
    Ok((kept, shares))
}

/// Each of `shares`' directories, opened, as the share process takes them.
fn opened(shares: &[(PathBuf, bool)]) -> Result<Vec<std::fs::File>, String> {
    shares
        .iter()
        .map(|(dir, _)| {
            #[cfg(unix)]
            let file = std::fs::File::open(dir);
            // A directory opens on Windows only with backup semantics.
            #[cfg(windows)]
            let file = shards_vmm::platform::open_dir(dir);
            let file = file.map_err(|e| format!("--share {}: {e}", dir.display()))?;
            let is_dir = file.metadata().map(|m| m.is_dir()).unwrap_or(false);
            if !is_dir {
                return Err(format!("--share {}: not a directory", dir.display()));
            }
            Ok(file)
        })
        .collect()
}

/// The home's private directory, which a share process keeps a restored guest's unreached
/// files in (D176), as the daemon gives it its own (`daemon.rs` `private_dir`).
fn private_dir() -> Result<std::fs::File, String> {
    let at = shards_ipc::home()?.join("tmp");
    match crate::os::owner_only_dir(std::fs::DirBuilder::new().recursive(true)).create(&at) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(format!("{}: {e}", at.display()));
        }
        _ => {}
    }
    #[cfg(unix)]
    return std::fs::File::open(&at).map_err(|e| format!("{}: {e}", at.display()));
    #[cfg(windows)]
    shards_vmm::platform::open_dir(&at).map_err(|e| format!("{}: {e}", at.display()))
}

/// The share process's arguments: `share`, then each share's mode and its name, empty for
/// the whole directory.
fn share_args(shares: &[(PathBuf, bool)]) -> Vec<&'static OsStr> {
    let mut args = vec![OsStr::new("share")];
    for (_, read_only) in shares {
        args.push(OsStr::new(if *read_only { "ro" } else { "rw" }));
        args.push(OsStr::new(""));
    }
    args
}

/// `args` (a `run`'s, `run` first) with its `--share`s served: a share process started on
/// them, and each share's connection to it given as `--slates-fs-fd N`, a descriptor kept
/// open through the exec into the VM. The share process ends as its connections close, with
/// the VM, and the kernel reaps it (SIGCHLD stays ignored through the exec).
#[cfg(unix)]
pub(super) fn served(args: &[OsString]) -> Result<Vec<OsString>, String> {
    use std::os::fd::{AsFd as _, IntoRawFd as _};
    use std::os::unix::net::UnixStream;
    if args.first().is_none_or(|a| a != "run") {
        return Ok(args.to_vec());
    }
    let (mut kept, shares) = take(args.get(1..).unwrap_or_default())?;
    if shares.is_empty() {
        return Ok(args.to_vec());
    }
    let dirs = opened(&shares)?;
    let ghosts = private_dir()?;
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let (link, theirs) = UnixStream::pair().map_err(|e| format!("the share process's connection: {e}"))?;
    let stderr = std::io::stderr();
    let number = |at: usize| i32::try_from(at).map_err(|_| "too many shares".to_string());
    let mut fds = vec![(stderr.as_fd(), 2), (theirs.as_fd(), 3)];
    for (i, dir) in dirs.iter().enumerate() {
        fds.push((dir.as_fd(), number(4usize.saturating_add(i))?));
    }
    fds.push((ghosts.as_fd(), number(4usize.saturating_add(dirs.len()))?));
    // SAFETY: signal(2) setting SIGCHLD's disposition, before any thread starts.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    // A session of its own, out of reach of a Ctrl-C meant for the VM, as the grants
    // broker's.
    shards_ipc::spawn(&exe, &share_args(&shares), &fds, true)
        .map_err(|e| format!("starting the share process: {e}"))?;
    drop((theirs, dirs, ghosts));
    let mut ends = Vec::with_capacity(shares.len());
    while ends.len() < shares.len() {
        let m = shards_ipc::recv(&link)
            .map_err(|e| format!("the share process: {e}"))?
            .ok_or("the share process ended first")?;
        if m.kind != shards_ipc::kind::SHARE_ENDS {
            return Err(format!("the share process said message kind {}", m.kind));
        }
        ends.extend(m.fds);
    }
    if ends.len() != shares.len() {
        return Err(format!(
            "the share process gave {} connections for {} shares",
            ends.len(),
            shares.len()
        ));
    }
    // It holds its ends until told they are taken (M24: XNU's collector flushes a socket in
    // flight that no process holds).
    shards_ipc::send(&link, shards_ipc::kind::TAKEN, &[], &[])
        .map_err(|e| format!("the share process: {e}"))?;
    // Its flags as given, `--kernel` first, as the command line knows a direct boot by it;
    // each share's connection after them; then its command.
    let at = kept.iter().position(|a| a == "--").unwrap_or(kept.len());
    let command = kept.split_off(at);
    let mut with: Vec<OsString> = vec!["run".into()];
    with.extend(kept);
    for end in ends {
        let fd = end.into_raw_fd();
        // SAFETY: fcntl(2) clearing close-on-exec on a descriptor this process owns, kept
        // open through the exec, for the VM.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
            return Err(format!(
                "a share's connection: {}",
                std::io::Error::last_os_error()
            ));
        }
        with.extend(["--slates-fs-fd".into(), fd.to_string().into()]);
    }
    with.extend(command);
    Ok(with)
}

/// The share streams of `args`' `--share`s on Windows (D197): a share process started on
/// the directories, given each share's server end of a duplex pipe, and the VM's ends,
/// named `share-{i}`, for the VM to be given as `--slates-fs share-{i}`, which takes the
/// place of each `--share`.
#[cfg(windows)]
pub(super) fn served(
    args: &[OsString],
) -> Result<(Vec<OsString>, Vec<(String, std::os::windows::io::OwnedHandle)>), String> {
    use std::os::windows::io::AsHandle as _;
    if args.first().is_none_or(|a| a != "run") {
        return Ok((args.to_vec(), Vec::new()));
    }
    let (kept, shares) = take(args.get(1..).unwrap_or_default())?;
    if shares.is_empty() {
        return Ok((args.to_vec(), Vec::new()));
    }
    let dirs = opened(&shares)?;
    let ghosts = private_dir()?;
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let streams = shares
        .iter()
        .map(|_| shards_ipc::byte_pair())
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| format!("a share's stream: {e}"))?;
    let n = shares.len();
    let number = |at: usize| i32::try_from(at).map_err(|_| "too many shares".to_string());
    let err = std::io::stderr();
    let mut fds = vec![
        (err.as_handle(), 2),
        (ghosts.as_handle(), number(4usize.saturating_add(n))?),
    ];
    for (i, (dir, (server, _))) in dirs.iter().zip(&streams).enumerate() {
        fds.push((dir.as_handle(), number(4usize.saturating_add(i))?));
        fds.push((
            server.as_handle(),
            number(5usize.saturating_add(n).saturating_add(i))?,
        ));
    }
    let env = crate::netproc::child_env();
    shards_ipc::spawn_in_job(
        None,
        &exe,
        &share_args(&shares),
        &fds,
        &crate::netproc::env_pairs(&env),
    )
    .map_err(|e| format!("starting the share process: {e}"))?;
    let mut with: Vec<OsString> = vec!["run".into()];
    let mut given = Vec::with_capacity(n);
    let at = kept.iter().position(|a| a == "--").unwrap_or(kept.len());
    with.extend(kept.iter().take(at).cloned());
    for (i, (_, vm)) in streams.into_iter().enumerate() {
        let name = format!("share-{i}");
        with.extend(["--slates-fs".into(), name.clone().into()]);
        given.push((name, vm));
    }
    with.extend(kept.iter().skip(at).cloned());
    Ok((with, given))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--share DIR` is writable, `DIR:ro` read-only, and any other mode refused.
    #[test]
    fn a_share_is_a_directory_and_its_mode() {
        assert_eq!(
            parse(OsStr::new("/srv/data")).unwrap(),
            ("/srv/data".into(), false)
        );
        assert_eq!(
            parse(OsStr::new("/srv/data:ro")).unwrap(),
            ("/srv/data".into(), true)
        );
        assert_eq!(parse(OsStr::new("rel/dir:ro")).unwrap(), ("rel/dir".into(), true));
        // A colon in a name above the last is the path's.
        assert_eq!(parse(OsStr::new("/a:b/c")).unwrap(), ("/a:b/c".into(), false));
        for bad in ["/srv/data:rw", "/srv/data:", "/srv/data:RO", ":ro", ""] {
            assert!(parse(OsStr::new(bad)).is_err(), "{bad}");
        }
    }

    /// Every `--share` before `--` is taken, in order; those after it are the command's.
    #[test]
    fn shares_are_taken_from_the_flags() {
        let args: Vec<OsString> = [
            "--kernel", "k", "--share", "/a", "--share", "/b:ro", "--", "--share", "x",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let (kept, shares) = take(&args).unwrap();
        assert_eq!(kept, ["--kernel", "k", "--", "--share", "x"]);
        assert_eq!(shares, [("/a".into(), false), ("/b".into(), true)]);
        assert!(take(&[OsString::from("--share")]).is_err());
    }
}
