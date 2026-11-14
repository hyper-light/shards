//! A managed microVM's final use (D260's final use, its managed half): what its machine used in
//! all, read by its VM process once its machine is down, kept in a file beside its vsock
//! socket (`shards_api::final_use_file`), which its client reads once it has ended. The VM
//! process, confined (D30), writes no file: it tells its keeper on a channel of its own
//! (`--final-use`), and the keeper writes the record whole, by rename. On Unix the keeper is
//! `shards final-use PATH`, started beside the VM before this process becomes it, which
//! answers once the record is in place: the VM exits after that answer, so its record is
//! there before its client sees its end. On Windows the CLI that waits for its VM keeps it,
//! and exits after.
//!
//! What the VM tells: its use's answer (`shards_abi::rate::used`'s text), after its length,
//! a u32, big-endian. Nothing, or less than it said it would tell, is no record: its client
//! then reads none (`Unused::Untold`).

use std::io::{self, Read, Write as _};
use std::path::Path;

/// The flag that names a VM's channel to its keeper, and on Windows the name its handle is
/// given by.
pub const FLAG: &str = "--final-use";
#[cfg(windows)]
pub const NAME: &str = "final-use";

/// Reads what a VM told of its final use on `from` and records it at `to`, whole: written
/// beside it, made durable, then renamed over it. Returns whether there was one to record:
/// none where the VM ended before telling it whole. Its length is held to the longest use
/// answer (`shards_abi::rate::ANSWER_MOST`).
pub fn keep(from: &mut impl Read, to: &Path) -> Result<bool, String> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(format!("the VM's channel: {e}")),
    }
    let len = usize::try_from(u32::from_be_bytes(len)).map_err(|e| e.to_string())?;
    if len > shards_abi::rate::ANSWER_MOST {
        return Err(format!("a final use of {len} bytes, past the longest use answer"));
    }
    let mut text = Vec::new();
    text.try_reserve_exact(len).map_err(|e| e.to_string())?;
    match from
        .take(u64::try_from(len).unwrap_or(u64::MAX))
        .read_to_end(&mut text)
    {
        Ok(read) if read == len => {}
        Ok(_) => return Ok(false),
        Err(e) => return Err(format!("the VM's channel: {e}")),
    }
    let mut beside = to.as_os_str().to_owned();
    beside.push(format!(".{}.tmp", std::process::id()));
    let beside = std::path::PathBuf::from(beside);
    let written = std::fs::File::create(&beside)
        .and_then(|mut f| f.write_all(&text).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&beside, to));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&beside);
        return Err(format!("{}: {e}", to.display()));
    }
    Ok(true)
}

/// `shards final-use PATH`, the keeper of a managed VM's final use on Unix: the VM's channel
/// at descriptor 3, its starter's; answers it, one byte, once the record is in place, or
/// closes it unanswered.
#[cfg(unix)]
pub fn keeper(mut args: impl Iterator<Item = std::ffi::OsString>) -> std::process::ExitCode {
    use std::os::fd::FromRawFd as _;
    let kept = (|| -> Result<(), String> {
        let to = std::path::PathBuf::from(args.next().ok_or("no record's path")?);
        // SAFETY: fcntl(2) asks whether descriptor 3, which its starter gives it, is open.
        if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
            return Err("no channel at descriptor 3".into());
        }
        // SAFETY: the socket its starter gave it at 3 (checked open above), owned from here on.
        let mut channel = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
        if keep(&mut channel, &to)? {
            channel
                .write_all(&[1])
                .map_err(|e| format!("answering the VM: {e}"))?;
        }
        Ok(())
    })();
    match kept {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: a managed VM's final use: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Where a managed VM of `vsock`'s socket records its final use: none for one that is not
/// managed (`managed`), or has no vsock socket.
pub fn record_of(vsock: Option<&std::ffi::OsStr>, managed: bool) -> Option<std::path::PathBuf> {
    let vsock = std::path::absolute(vsock.filter(|_| managed)?).ok()?;
    Some(shards_api::final_use_file(&vsock))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// What a VM tells its keeper of `used` (vm_run.rs `tell_final_use`): its length, then it.
    fn told(used: &str) -> Vec<u8> {
        [
            &u32::try_from(used.len()).unwrap().to_be_bytes()[..],
            used.as_bytes(),
        ]
        .concat()
    }

    /// A final use told whole is recorded whole, by rename, nothing left beside it; one told
    /// short, or not at all, records nothing; one past the longest answer is refused.
    #[test]
    fn a_final_use_told_whole_is_recorded_and_nothing_else_is() {
        let dir = shards_testdir::TempDir::new("final-use").unwrap();
        let to = dir.join("v.used");
        let used = shards_abi::rate::used(Err("the microVM has not started"), Err("its VM process: x"));
        assert_eq!(keep(&mut told(&used).as_slice(), &to), Ok(true));
        assert_eq!(std::fs::read(&to).unwrap(), used.as_bytes());
        let left: Vec<_> = std::fs::read_dir(&*dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["v.used"], "nothing left beside it");
        std::fs::remove_file(&to).unwrap();
        let whole = told(&used);
        for short in [&whole[..0], &whole[..3], &whole[..whole.len() - 1]] {
            assert_eq!(keep(&mut &short[..], &to), Ok(false), "{short:?}");
            assert!(!to.exists());
        }
        let long = u32::try_from(shards_abi::rate::ANSWER_MOST + 1)
            .unwrap()
            .to_be_bytes();
        assert!(keep(&mut &long[..], &to).is_err());
        assert!(!to.exists());
    }

    /// A managed VM's record is beside its vsock socket, made absolute; none for one that is
    /// not managed or has no socket.
    #[test]
    fn a_managed_vms_record_is_beside_its_socket() {
        let at = |p: &str| record_of(Some(p.as_ref()), true);
        assert_eq!(at("/run/g/v"), Some("/run/g/v.used".into()));
        assert_eq!(at("g/v"), Some(std::env::current_dir().unwrap().join("g/v.used")));
        assert_eq!(record_of(Some("/run/g/v".as_ref()), false), None);
        assert_eq!(record_of(None, true), None);
    }
}
