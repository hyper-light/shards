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

/// A `--share` value: its directory, and whether it is read-only, as `--disk` reads its
/// path (vm_run.rs): a trailing `:ro` is the mode, and every other colon is the path's, so a
/// directory whose names hold colons is shared as it is named. A mode mistyped (`DIR:rw`)
/// names a directory that is not there, and its open says so.
pub(super) fn parse(value: &OsStr) -> Result<(PathBuf, bool), String> {
    match value.as_encoded_bytes().strip_suffix(b":ro") {
        // SAFETY: cut just before `:ro`, ASCII, which `from_encoded_bytes_unchecked`
        // documents as a valid boundary.
        Some(dir) => directory(unsafe { OsStr::from_encoded_bytes_unchecked(dir) }, true),
        None => directory(value, false),
    }
}

fn directory(dir: &OsStr, read_only: bool) -> Result<(PathBuf, bool), String> {
    if dir.is_empty() {
        return Err("--share: no directory given".into());
    }
    Ok((PathBuf::from(dir), read_only))
}

/// A share as `--share` names it: its directory, and whether it is read-only.
type Named = (PathBuf, bool);

/// `args` (a `run`'s, after `run`) with each `--share` taken out, and the shares, in order.
/// Those after `--` are its command's. Read word by word as `run` reads them
/// (`spec::next_word`): a flag's value is its own, whatever it reads as.
pub(super) fn take(args: &[OsString]) -> Result<(Vec<OsString>, Vec<Named>), String> {
    let mut kept = Vec::with_capacity(args.len());
    let mut shares = Vec::new();
    let mut rest = args.iter().cloned();
    while let Some((arg, value)) = crate::spec::next_word(&mut rest) {
        if arg == "--" {
            kept.push(arg);
            kept.extend(rest);
            break;
        }
        if arg == "--share" {
            let value = rest.next().ok_or("--share needs a value")?;
            shares.push(parse(&value)?);
            continue;
        }
        kept.push(arg);
        kept.extend(value);
    }
    Ok((kept, shares))
}

/// Each of `shares`' directories, opened, as the share process takes them.
fn opened(shares: &[(PathBuf, bool)]) -> Result<Vec<std::fs::File>, String> {
    shares
        .iter()
        .map(|(dir, _)| {
            // A directory alone (O_DIRECTORY), and never waited on (O_NONBLOCK): a FIFO
            // named as a share is refused at once, not opened for a writer that never comes.
            #[cfg(unix)]
            let file = {
                use std::os::unix::fs::OpenOptionsExt as _;
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
                    .open(dir)
            };
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
    let at = shards_ipc::home()?.join(crate::daemon::PRIVATE);
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
/// them, and each share's connection to it given as `--slates-fs-fd N`, a descriptor
/// returned close-on-exec, for [`inherit`] to keep open through the exec into the VM alone,
/// right before it. The share process ends as its connections close, with the VM, and the
/// kernel reaps it (SIGCHLD stays ignored through the exec).
#[cfg(unix)]
pub(super) fn served(args: &[OsString]) -> Result<(Vec<OsString>, Vec<std::os::fd::OwnedFd>), String> {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use std::os::unix::net::UnixStream;
    if args.first().is_none_or(|a| a != "run") {
        return Ok((args.to_vec(), Vec::new()));
    }
    let (mut kept, shares) = take(args.get(1..).unwrap_or_default())?;
    if shares.is_empty() {
        return Ok((args.to_vec(), Vec::new()));
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
    for end in &ends {
        with.extend(["--slates-fs-fd".into(), end.as_raw_fd().to_string().into()]);
    }
    with.extend(command);
    Ok((with, ends))
}

/// Keeps `fds` open through the exec that follows at once: their close-on-exec cleared, the
/// last thing before it, so that no process started before (the grants broker, a working
/// set's writer) inherits a share's connection (review 12).
#[cfg(unix)]
pub(super) fn inherit(fds: Vec<std::os::fd::OwnedFd>) -> Result<(), String> {
    use std::os::fd::IntoRawFd as _;
    for end in fds {
        let fd = end.into_raw_fd();
        // SAFETY: fcntl(2) clearing close-on-exec on a descriptor this process owns, kept
        // open through the exec, for the VM.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
            return Err(format!(
                "a share's connection: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// `args` with each `--share` in its served place, and the VM's ends, each named.
#[cfg(windows)]
pub(super) type Served = (Vec<OsString>, Vec<(String, std::os::windows::io::OwnedHandle)>);

/// The share streams of `args`' `--share`s on Windows (D197): a share process started on
/// the directories, given each share's server end of a duplex pipe, and the VM's ends,
/// named `share-{i}`, for the VM to be given as `--slates-fs share-{i}`, which takes the
/// place of each `--share`.
#[cfg(windows)]
pub(super) fn served(args: &[OsString]) -> Result<Served, String> {
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
    // Detached, as Unix's leads a session of its own: out of reach of a Ctrl-C meant for the
    // VM (review 11).
    shards_ipc::spawn_detached(&exe, &share_args(&shares), &fds, &crate::netproc::env_pairs(&env))
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

    /// `--share DIR` is writable and `DIR:ro` read-only; every other colon is the path's, as
    /// `--disk` reads it.
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
        // Colons anywhere but a trailing `:ro` are the path's, the last name's too, as
        // `--disk`'s (review 8).
        assert_eq!(parse(OsStr::new("/a:b/c")).unwrap(), ("/a:b/c".into(), false));
        for path in [
            "/srv/a:b",
            "/srv/v1:2024",
            "/srv/data:rw",
            "/srv/data:",
            "/srv/data:RO",
        ] {
            assert_eq!(parse(OsStr::new(path)).unwrap(), (path.into(), false), "{path}");
        }
        assert_eq!(
            parse(OsStr::new("/srv/v1:2024:ro")).unwrap(),
            ("/srv/v1:2024".into(), true)
        );
        for bad in [":ro", ""] {
            assert!(parse(OsStr::new(bad)).is_err(), "{bad}");
        }
    }

    /// A FIFO named as a share is refused at once as no directory, not opened for a writer
    /// that never comes, which blocked open(2) for ever (review 10).
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_as_a_share_is_refused_at_once() {
        let dir = shards_testdir::TempDir::new("share-fifo").unwrap();
        let fifo = dir.path().join("f");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        let e = opened(&[(fifo.clone(), false)]).unwrap_err();
        assert!(e.contains(&fifo.display().to_string()), "{e}");
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

    /// A flag's value is not a flag: `--cmdline --share` keeps `--share` as the command
    /// line's, and `--env --` keeps `--` as the variable (review 9).
    #[test]
    fn a_flags_value_is_not_taken_as_a_flag() {
        let args: Vec<OsString> = [
            "--kernel",
            "k",
            "--cmdline",
            "--share",
            "-e",
            "--",
            "--share",
            "/a",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let (kept, shares) = take(&args).unwrap();
        assert_eq!(kept, ["--kernel", "k", "--cmdline", "--share", "-e", "--"]);
        assert_eq!(shares, [("/a".into(), false)]);
    }

    /// Every flag `run` reads a value after is one whose value `take` keeps as its own, as
    /// `run` reads its command line (vm_run.rs `parse_run`) and not by a list beside it:
    /// `--cpu-baseline` (D176 part 6), a flag `parse_run` takes a value for, keeps a value
    /// that reads `--share` (integrate's review: vm_run's own parser).
    #[test]
    fn take_reads_values_as_run_does() {
        let args: Vec<OsString> = ["--kernel", "k", "--cpu-baseline", "--share", "--share", "/a"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let (kept, shares) = take(&args).unwrap();
        assert_eq!(kept, ["--kernel", "k", "--cpu-baseline", "--share"]);
        assert_eq!(shares, [("/a".into(), false)]);
    }
}
