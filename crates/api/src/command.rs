//! An internal command of the `shards` binary run for a caller (docs/design/architecture.md
//! D262, D263): `shards api-check` and `shards api-build`, started as an image run's host is
//! (`image::session::start_host`: exactly its three standard streams, in a group of its own),
//! its answers read as frames from its stdout and its last diagnostics kept from its stderr.
//! Neither pipe's end is taken for the command's (D231): its exit watcher follows its group
//! to its end, reaps it, and only then tells the readers ([`UntilGone`]) that nothing is left
//! to write. Dropped before it has ended, its group is killed.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::image::ImageRefusal;
use crate::image::session::{Gone, Host, UntilGone, kill, said_kept, start_host};

/// Why a command did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotStarted {
    /// The binary did not start: its path and the OS's words.
    Start(String),
    /// What this process could not make for it.
    Os { what: &'static str, kind: io::ErrorKind },
}

/// How a command ended: its exit status (or 128 plus the signal that ended it), where it was
/// reaped, and its last diagnostics, lossily UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    pub(crate) status: Option<i32>,
    pub(crate) said: String,
}

/// A command started, its answers read as they come.
pub(crate) struct Command {
    process: Arc<shards_ipc::Child>,
    to_host: Option<io::PipeWriter>,
    answers: UntilGone,
    watcher: Option<JoinHandle<Option<i32>>>,
    said: Option<JoinHandle<Vec<u8>>>,
}

impl Command {
    /// Starts `binary` (else `SHARDS_BIN`, else `shards` on `PATH`) with `args`, its
    /// environment this process's with `env` over it.
    pub(crate) fn start(
        binary: Option<&Path>,
        args: &[&OsStr],
        env: &[(OsString, OsString)],
    ) -> Result<Command, NotStarted> {
        let binary = binary
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("shards"));
        let Host {
            process,
            to_host,
            from_host,
            said,
        } = start_host(&binary, args, env, None).map_err(|e| match e {
            ImageRefusal::Os { what, kind } => NotStarted::Os { what, kind },
            other => NotStarted::Start(other.to_string()),
        })?;
        let process = Arc::new(process);
        let os = |what: &'static str| move |e: io::Error| NotStarted::Os { what, kind: e.kind() };
        let started = (|| {
            #[cfg(unix)]
            let (told, gone) = io::pipe().map_err(os("the command's watcher"))?;
            #[cfg(windows)]
            let (told, gone) = {
                let gone = shards_ipc::Event::new().map_err(os("the command's watcher"))?;
                (gone.try_clone().map_err(os("the command's watcher"))?, gone)
            };
            let until = |pipe| UntilGone::new(pipe, &told).map_err(os("the command's watcher"));
            let (answers, mut said) = (until(from_host)?, until(said)?);
            let watched = process.clone();
            let watcher = std::thread::Builder::new()
                .name("shards-api-command-exit".into())
                .spawn(move || {
                    let ended = watched.ended().is_ok();
                    if ended {
                        let _ = watched.end_group();
                    }
                    let status = ended.then(|| watched.wait().ok()).flatten();
                    Gone(gone).tell();
                    status
                })
                .map_err(os("the command's watcher"))?;
            let kept = std::thread::Builder::new()
                .name("shards-api-command-said".into())
                .spawn(move || keep_tail(&mut said, said_kept()))
                .map_err(os("the command's diagnostics"));
            match kept {
                Ok(kept) => Ok((answers, watcher, kept)),
                Err(e) => {
                    kill(&process);
                    let _ = watcher.join();
                    Err(e)
                }
            }
        })();
        match started {
            Ok((answers, watcher, said)) => Ok(Command {
                process,
                to_host: Some(to_host),
                answers,
                watcher: Some(watcher),
                said: Some(said),
            }),
            Err(e) => {
                kill(&process);
                Err(e)
            }
        }
    }

    /// Its stdin, until [`close`](Self::close).
    pub(crate) fn input(&mut self) -> Option<&mut io::PipeWriter> {
        self.to_host.as_mut()
    }

    /// Its stdin closed: the command reads its end.
    pub(crate) fn close(&mut self) {
        self.to_host = None;
    }

    /// Its next answer: its kind and payload; none once it and its group have ended.
    pub(crate) fn frame(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        crate::image::wire::read_frame(&mut self.answers)
    }

    /// Waits for it to end, its stdin closed: its status and last diagnostics.
    pub(crate) fn end(mut self) -> Ended {
        self.close();
        let status = self.watcher.take().and_then(|w| w.join().ok()).flatten();
        let said = self.said.take().and_then(|s| s.join().ok()).unwrap_or_default();
        Ended {
            status,
            said: String::from_utf8_lossy(&said).into_owned(),
        }
    }
}

impl Drop for Command {
    /// Killed, with every process it started, unless it has ended: its watcher reaps it.
    fn drop(&mut self) {
        if self.watcher.is_some() {
            kill(&self.process);
        }
    }
}

/// What `from` says until it ends, its last `keep` bytes.
fn keep_tail(from: &mut impl io::Read, keep: usize) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(buf.get(..n).unwrap_or_default());
        let over = tail.len().saturating_sub(keep);
        tail.drain(..over);
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_keeps_the_last_bytes_alone() {
        let said = b"0123456789".repeat(1000);
        assert_eq!(keep_tail(&mut said.as_slice(), 7), b"3456789");
        assert_eq!(keep_tail(&mut &b"ab"[..], 7), b"ab");
    }
}
