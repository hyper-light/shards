//! A run's log sent where its driver sends it (D163). The run's VM keeps its output in
//! shards' store, as every run's (D161); the daemon follows the log of each container
//! whose driver sends it elsewhere, from where it last got to, as `logs -f` follows one,
//! and sends each line to the driver. So the VM, its guest and its run reach no network
//! for their log; a driver that falls behind, or is down, holds up nothing but its own
//! sending; and what it has not yet sent waits in the store, kept as the store keeps it,
//! through the daemon's restarts. moby sends from the run's own copier, and drops what its
//! driver refuses (daemon/logger/copier.go, logDriverError).

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::logs::{LogFile, Piece, Pos, Reader};
use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;
use crate::spec::LOG_STDERR;

mod gelf;
mod sockets;
mod syslog;

/// A line of a run's output, or a part of a longer one, as a driver is given it.
pub(super) struct Message<'a> {
    pub stream: u8,
    /// When the line's first byte came, nanoseconds since the epoch.
    pub at: u64,
    /// Its bytes, without the line's newline.
    pub line: &'a [u8],
}

/// What sends a run's messages where its driver says.
pub(super) trait Driver: Send {
    /// Sends `m`, or says why it could not, and is given it again. `wait` waits as long as
    /// the driver's own pace asks between its tries, and says whether the daemon stops
    /// meanwhile, which ends them.
    fn send(&mut self, m: &Message<'_>, wait: &dyn Fn(Duration) -> bool) -> io::Result<()>;
}

/// The longest message: moby's copier's buffer (copier.go, defaultBufSize), past which a
/// line goes in parts.
const PART: usize = 16 * 1024;

/// How long a driver that could not send waits before it tries again: a second at first,
/// half as long again after each failure, a minute at most, as fluent-logger-golang waits
/// for its server (fluent.go: defaultRetryWait, defaultReconnectWaitIncreRate,
/// defaultMaxRetryWait), through which moby's fluentd driver sends.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MOST: Duration = Duration::from_secs(60);

/// Where a container's shipper keeps its place in its log, beside it.
const CURSOR: &str = "ship";
const CURSOR_NEW: &str = "ship.new";

/// The shippers running, by container: each one's end, which is told to stop.
#[derive(Default)]
pub(super) struct Shipping {
    running: std::sync::Mutex<HashMap<String, UnixStream>>,
}

impl Shipping {
    /// Stops every shipper: as a test's daemon's scope ends.
    #[cfg(test)]
    pub(super) fn end(&self) {
        for stop in lock(&self.running).values() {
            let _ = stop.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Each stream's line as its messages go, cut as moby's copier cuts one (copier.go,
/// copySrc): a line of less than [`PART`] bytes is one message; a longer one, parts of
/// [`PART`] bytes, then the rest, which may be nothing.
#[derive(Default)]
struct Lines {
    open: [Open; 2],
}

#[derive(Default)]
struct Open {
    held: Vec<u8>,
    at: u64,
}

impl Lines {
    /// Takes `piece`, sending each message it completes with `send`, which returns once
    /// it is sent, or says why it never will be.
    fn take(
        &mut self,
        piece: &Piece<'_>,
        send: &mut dyn FnMut(&Message<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        let stream = piece.stream;
        let open = self
            .open
            .get_mut(usize::from(stream == LOG_STDERR))
            .ok_or_else(|| io::Error::other("no such stream"))?;
        if piece.first {
            open.at = piece.at;
        }
        let (bytes, ends) = match piece.bytes.strip_suffix(b"\n") {
            Some(line) => (line, true),
            None => (piece.bytes, false),
        };
        open.held.extend_from_slice(bytes);
        // A part as soon as one is held whole, without its newline: the copier's buffer
        // full, which holds the line's newline only where there is room for it.
        while open.held.len() >= PART {
            let rest = open.held.split_off(PART);
            let part = std::mem::replace(&mut open.held, rest);
            send(&Message {
                stream,
                at: open.at,
                line: &part,
            })?;
        }
        if ends {
            open.finish(stream, send)?;
        }
        Ok(())
    }

    /// Sends what is left of each stream's unfinished line, as the run has ended.
    fn finish(&mut self, send: &mut dyn FnMut(&Message<'_>) -> io::Result<()>) -> io::Result<()> {
        for (s, open) in self.open.iter_mut().enumerate() {
            let stream = if s == 1 {
                LOG_STDERR
            } else {
                crate::spec::LOG_STDOUT
            };
            if !open.held.is_empty() {
                open.finish(stream, send)?;
            }
        }
        Ok(())
    }
}

impl Open {
    /// Sends the rest of the line.
    fn finish(&mut self, stream: u8, send: &mut dyn FnMut(&Message<'_>) -> io::Result<()>) -> io::Result<()> {
        let held = std::mem::take(&mut self.held);
        send(&Message {
            stream,
            at: self.at,
            line: &held,
        })
    }
}

/// Where a shipper of the log in `dir` got to: each stream's place, and whether its run
/// had ended with all of it sent. None where none was kept, or what was is not one.
fn read_cursor(dir: &Path) -> Option<([Pos; 2], bool)> {
    let b = std::fs::read(dir.join(CURSOR)).ok()?;
    let (&done, rest) = b.split_first()?;
    let (out, rest) = rest.split_first_chunk::<24>()?;
    let (err, rest) = rest.split_first_chunk::<24>()?;
    rest.is_empty()
        .then(|| ([Pos::from_bytes(out), Pos::from_bytes(err)], done == 1))
}

/// Keeps where a shipper of the log in `dir` got to: written aside, then renamed into
/// place, so that a crash leaves the last place kept whole. Not made durable: a place
/// lost costs sending again what was sent since the one before it.
fn write_cursor(dir: &Path, at: [Pos; 2], done: bool) -> io::Result<()> {
    let mut b = Vec::with_capacity(49);
    b.push(u8::from(done));
    for p in at {
        b.extend(p.to_bytes());
    }
    std::fs::write(dir.join(CURSOR_NEW), &b)?;
    std::fs::rename(dir.join(CURSOR_NEW), dir.join(CURSOR))
}

/// Removes the segments of the log in `dir` wholly sent, where `at` says each stream's
/// sending got to: those before the first segment either stream is in, which `logs`
/// keeps no copy of (`cache-disabled`).
fn drop_sent(dir: &Path, at: [Pos; 2]) {
    let first = first(at);
    let Ok(names) = std::fs::read_dir(dir) else {
        return;
    };
    for name in names.filter_map(|e| e.ok()?.file_name().into_string().ok()) {
        if let Some(seq) = super::logs::segment_of(&name)
            && seq < first
        {
            let (log_name, index_name) = crate::segments::log_segment(seq);
            for gone in [index_name, log_name, crate::segments::packed_segment(seq)] {
                let _ = std::fs::remove_file(dir.join(gone));
            }
        }
    }
}

/// Where a shipper keeps its place, and when: as the segment it reads changes, as it
/// catches up, and as its run ends. So a crash costs sending again at most a segment's
/// lines, a log written on without pause costs a write a segment, not one a line, and
/// where `logs` keeps no copy (`cache-disabled`) the segments wholly sent go as they are.
#[derive(Default)]
struct Place {
    kept: Option<([Pos; 2], bool)>,
    seq: Option<u64>,
}

impl Place {
    /// Whether `at` is in another segment than the place kept.
    fn moved(&self, at: [Pos; 2]) -> bool {
        Some(first(at)) != self.seq
    }

    fn keep(&mut self, id: &str, dir: &Path, at: [Pos; 2], done: bool, cached: bool) {
        if self.kept == Some((at, done)) {
            return;
        }
        if let Err(e) = write_cursor(dir, at, done) {
            log(format!("container {id}: where sending its log got to: {e}"));
        }
        if !cached && self.moved(at) {
            drop_sent(dir, at);
        }
        (self.kept, self.seq) = (Some((at, done)), Some(first(at)));
    }
}

/// The first segment either stream's place is in.
fn first(at: [Pos; 2]) -> u64 {
    at[0].seq().min(at[1].seq())
}

/// Whether any of `fds` is readable now.
fn ready_now(fds: &[std::os::fd::BorrowedFd<'_>; 3]) -> bool {
    let mut polled = fds.map(|fd| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    // SAFETY: poll(2), not waiting, on three pollfds of descriptors the caller holds.
    let ready = unsafe { libc::poll(polled.as_mut_ptr(), 3, 0) };
    ready > 0
}

/// How a shipper's following of a run ended.
enum Shipped {
    /// Its run ended, and all it wrote is sent.
    Ended,
    /// The daemon stops.
    Stopped,
}

/// Waits until `stop` is told, or `wait` has passed: whether it was told. A wait poll(2)
/// cannot make is taken for a stop: none is left to wait with.
fn stopped_within(stop: &UnixStream, wait: Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let mut pfd = libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: poll(2) on one pollfd of a descriptor this thread holds open.
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            0 => return false,
            n if n > 0 => return true,
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return true,
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// Has container `id`'s log sent where its driver says, from where it last got to, on
    /// a thread of its own, unless one does it already: as its run starts, and as the
    /// daemon starts, for what the one before it had not sent.
    pub(super) fn ship_soon<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, id: &str) {
        let dir = lock(&self.containers).dir(id);
        let Some(run) = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
        else {
            return;
        };
        let cached = crate::logging::kept(&run, self.logs)
            .ok()
            .flatten()
            .is_none_or(|k| k.cached);
        let mut driver = match self.driver_of(id, &run) {
            Ok(Some(driver)) => driver,
            Ok(None) => return,
            Err(e) => {
                log(format!("container {id}: its log's driver: {e}"));
                return;
            }
        };
        let (stop, stopped) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                log(format!("container {id}: sending its log: {e}"));
                return;
            }
        };
        {
            let mut running = lock(&self.shipping.running);
            if running.contains_key(id) {
                return;
            }
            running.insert(id.to_string(), stop);
        }
        let owned = id.to_string();
        let spawned = std::thread::Builder::new()
            .name("shipper".into())
            .spawn_scoped(threads, move || {
                loop {
                    let shipped = self.ship(&owned, &dir, driver.as_mut(), cached, &stopped);
                    // Decided under the lock a run's start asks under, the run started as it
                    // asks: a run of it that began as the last one's end was sent is followed
                    // too, by this thread, or by the one that run's start makes.
                    let mut running = lock(&self.shipping.running);
                    match shipped {
                        Ok(Shipped::Ended) if self.running(&owned) => continue,
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => log(format!("container {owned}: sending its log: {e}")),
                    }
                    running.remove(&owned);
                    return;
                }
            });
        if let Err(e) = spawned {
            lock(&self.shipping.running).remove(id);
            log(format!("container {id}: its log's shipper's thread: {e}"));
        }
    }

    /// Ships the log in `dir` with `driver`, from where it last got to, until its run has
    /// ended and all it wrote is sent, or `stopped` is told.
    fn ship(
        &self,
        id: &str,
        dir: &Path,
        driver: &mut dyn Driver,
        cached: bool,
        stopped: &UnixStream,
    ) -> io::Result<Shipped> {
        let file = LogFile::open(dir)?;
        let mut reader = match read_cursor(dir) {
            Some((at, _)) => Reader::from(at),
            None => Reader::new(),
        };
        let mut lines = Lines::default();
        let mut place = Place::default();
        // Sent once it is, whatever the driver's failures meanwhile; given up only as the
        // daemon stops, its place kept from before it.
        let mut send = |m: &Message<'_>| -> io::Result<()> {
            let mut wait = RETRY_FIRST;
            loop {
                let Err(e) = driver.send(m, &|d| stopped_within(stopped, d)) else {
                    return Ok(());
                };
                log(format!("container {id}: sending its log: {e}; again in {wait:?}"));
                if stopped_within(stopped, wait) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "the daemon stops"));
                }
                wait = wait.mul_f64(1.5).min(RETRY_MOST);
            }
        };
        // Followed as it is written, while its run runs.
        if let Some((number, end)) = self.wake_at_end(id)? {
            let followed = (|| -> io::Result<Shipped> {
                let mut watch = shards_vmm::platform::FileWatch::new(file.dir())?;
                let mut watched = None;
                loop {
                    let mut each = |p: Piece<'_>| lines.take(&p, &mut send).map(|()| true);
                    reader.read(&file, &mut each)?;
                    let at = reader.resume_at();
                    if place.moved(at) {
                        place.keep(id, dir, at, false, cached);
                    }
                    if let Some(segment) = reader.segment()
                        && watched != Some(segment.seq())
                    {
                        watch.file(segment.index())?;
                        watched = Some(segment.seq());
                        continue;
                    }
                    let fds = [watch.fd(), end.as_fd(), stopped.as_fd()];
                    if !ready_now(&fds) {
                        place.keep(id, dir, at, false, cached);
                    }
                    let ready = super::commands::wait_readable(&fds)?;
                    if ready[1] {
                        return Ok(Shipped::Ended);
                    }
                    if ready[2] {
                        return Ok(Shipped::Stopped);
                    }
                    watch.clear();
                }
            })();
            self.forget_waiter(id, number);
            if let Shipped::Stopped = followed? {
                return Ok(Shipped::Stopped);
            }
        }
        // Its run is over: what it left, whole.
        let mut each = |p: Piece<'_>| lines.take(&p, &mut send).map(|()| true);
        if reader.read(&file, &mut each)? {
            reader.finish(&mut each)?;
            lines.finish(&mut send)?;
        }
        place.keep(id, dir, reader.resume_at(), true, cached);
        Ok(Shipped::Ended)
    }

    /// The driver container `id`'s run sends its log with, if one sends it elsewhere.
    fn driver_of(&self, id: &str, run: &shards_ipc::Run) -> Result<Option<Box<dyn Driver>>, String> {
        let Some(remote) = crate::logging::remote(run)? else {
            return Ok(None);
        };
        let details = self.log_details(id)?;
        let tag = crate::logging::tag(run, &details.info())?;
        Ok(Some(match remote {
            crate::logging::Remote::Syslog(config) => Box::new(syslog::Syslog::new(
                config,
                crate::logging::syslog::From {
                    tag,
                    hostname: details.hostname.clone(),
                    pid: std::process::id(),
                },
            )?),
            crate::logging::Remote::Gelf(config) => {
                let attributes = crate::logging::attributes(run, &details.info())?;
                let fields = [
                    ("_container_id", details.id.clone()),
                    (
                        "_container_name",
                        details.name.trim_start_matches('/').to_string(),
                    ),
                    ("_image_id", details.image_id.clone()),
                    ("_image_name", details.image_name.clone()),
                    ("_command", details.command()),
                    ("_tag", tag),
                    ("_created", details.created.clone()),
                ];
                Box::new(gelf::Gelf::new(
                    config,
                    crate::logging::gelf::From {
                        host: details.hostname.clone(),
                        extra: crate::logging::gelf::extra(&fields, &attributes),
                    },
                ))
            }
        }))
    }

    /// As the daemon starts: has the logs sent that a daemon before it had not finished
    /// sending.
    pub(super) fn ship_left<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let left: Vec<(String, PathBuf)> = {
            let registry = lock(&self.containers);
            registry
                .all()
                .filter(|c| c.log_ships)
                .map(|c| (c.id.clone(), registry.dir(&c.id)))
                .collect()
        };
        for (id, dir) in left {
            if !read_cursor(&dir).is_some_and(|(_, done)| done) {
                self.ship_soon(threads, &id);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::spec::LOG_STDOUT;

    /// The messages `pieces` make, then the run's end: each its stream, time and bytes.
    fn cut(pieces: &[(u8, u64, bool, &[u8])]) -> Vec<(u8, u64, Vec<u8>)> {
        let mut sent = Vec::new();
        let mut lines = Lines::default();
        let mut send = |m: &Message<'_>| {
            sent.push((m.stream, m.at, m.line.to_vec()));
            Ok(())
        };
        for &(stream, at, first, bytes) in pieces {
            lines
                .take(
                    &Piece {
                        stream,
                        at,
                        first,
                        bytes,
                    },
                    &mut send,
                )
                .unwrap();
        }
        lines.finish(&mut send).unwrap();
        sent
    }

    /// Lines are cut where moby's copier cuts them: one of less than 16 KiB whole; one of
    /// 16 KiB exactly, or longer, in parts of 16 KiB, then what is left, nothing where
    /// nothing is; each stream its own; and a line the run left unfinished whole.
    #[test]
    fn lines_are_cut_as_moby_s_copier_cuts_them() {
        let x = |n: usize| vec![b'x'; n];
        let line = |n: usize| [x(n), b"\n".to_vec()].concat();
        assert_eq!(
            cut(&[(LOG_STDOUT, 1, true, b"out\n")]),
            [(LOG_STDOUT, 1, b"out".to_vec())]
        );
        assert_eq!(
            cut(&[(LOG_STDOUT, 1, true, &line(PART - 1))]),
            [(LOG_STDOUT, 1, x(PART - 1))]
        );
        assert_eq!(
            cut(&[(LOG_STDOUT, 1, true, &line(PART))]),
            [(LOG_STDOUT, 1, x(PART)), (LOG_STDOUT, 1, Vec::new())]
        );
        // 40,000 bytes, as the reader gives them, in pieces, each with the line's time.
        let long = line(40_000);
        let sent = cut(&[
            (LOG_STDOUT, 7, true, &long[..20_000]),
            (LOG_STDERR, 8, true, b"err\n"),
            (LOG_STDOUT, 9, false, &long[20_000..]),
        ]);
        assert_eq!(
            sent,
            [
                (LOG_STDOUT, 7, x(PART)),
                (LOG_STDERR, 8, b"err".to_vec()),
                (LOG_STDOUT, 7, x(PART)),
                (LOG_STDOUT, 7, x(40_000 - 2 * PART)),
            ]
        );
        assert_eq!(
            cut(&[(LOG_STDERR, 3, true, b"no end")]),
            [(LOG_STDERR, 3, b"no end".to_vec())]
        );
    }

    /// Where a shipper got to is kept and read back; what is not one is none.
    #[test]
    fn a_shippers_place_is_kept() {
        let dir = shards_testdir::TempDir::new("ship-cursor").unwrap();
        assert_eq!(read_cursor(&dir), None);
        let at = [Pos::from_bytes(&[1; 24]), Pos::from_bytes(&[2; 24])];
        write_cursor(&dir, at, true).unwrap();
        assert_eq!(read_cursor(&dir), Some((at, true)));
        std::fs::write(dir.join(CURSOR), b"short").unwrap();
        assert_eq!(read_cursor(&dir), None);
    }

    /// A log kept from `logs` loses the segments wholly sent: those before the first either
    /// stream is still in, compressed or not.
    #[test]
    fn segments_wholly_sent_go() {
        let dir = shards_testdir::TempDir::new("ship-drop").unwrap();
        let names = [
            "log",
            "log.idx",
            "log.1.zst",
            "log.1.idx",
            "log.2",
            "log.2.idx",
            "log.3",
            "log.3.idx",
        ];
        for name in names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let seq = |s: u64| {
            let mut b = [0u8; 24];
            b[..8].copy_from_slice(&s.to_be_bytes());
            Pos::from_bytes(&b)
        };
        drop_sent(&dir, [seq(3), seq(2)]);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["log.2", "log.2.idx", "log.3", "log.3.idx"]);
    }
}
