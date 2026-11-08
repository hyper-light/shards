//! A run's log sent where its driver sends it (D163). The run's VM keeps its output in
//! shards' store, as every run's (D161); the daemon follows the log of each microVM
//! whose driver sends it elsewhere, from where it last got to, as `logs -f` follows one,
//! and sends each line to the driver. So the VM, its guest and its run reach no network
//! for their log; a driver that falls behind, or is down, holds up nothing but its own
//! sending; and what it has not yet sent waits in the store, kept as the store keeps it,
//! through the daemon's restarts. moby sends from the run's own copier, and drops what its
//! driver refuses (daemon/logger/copier.go, logDriverError).

use std::collections::{HashMap, VecDeque};
use std::io;
#[cfg(unix)]
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use shards_ipc::Stream as UnixStream;
use shards_vmm::platform::FileWatch;

use super::logs::{LogFile, Piece, Pos, Reader};
use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;
use crate::spec::LOG_STDERR;

mod awslogs;
#[cfg(windows)]
mod etwlogs;
mod fluentd;
mod gcplogs;
mod gelf;
#[cfg(target_os = "linux")]
mod journald;
#[cfg(target_os = "macos")]
mod oslog;
mod sockets;
mod splunk;
mod stream;
mod syslog;

/// A line of a run's output, or a part of a longer one, as a driver is given it.
pub(super) struct Message<'a> {
    pub stream: u8,
    /// When the line's first byte came, nanoseconds since the epoch.
    pub at: u64,
    /// Its bytes, without the line's newline.
    pub line: &'a [u8],
    /// Its place in its line, where the line is longer than one message of its receiver's
    /// carries ([`Driver::part`], [`Driver::fits`]): the ID its parts have in common, its
    /// place among them and whether it is the last, the fields receivers join parts by.
    pub part: Option<crate::logging::fluentd::Part<'a>>,
}

/// What sends a run's messages where its driver says.
pub(super) trait Driver: Send {
    /// Sends `m`, or says why it could not, and is given it again. `wait` waits as long as
    /// the driver's own pace asks between its tries, and says whether the daemon stops
    /// meanwhile, which ends them.
    fn send(&mut self, m: &Message<'_>, wait: &Wait<'_>) -> io::Result<()>;

    /// Sends what it holds back to send together (splunk's batch), as its shipper has
    /// caught up with its log: nothing waits on a tick, nor longer than its log does.
    fn flush(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        Ok(())
    }

    /// How many of the messages it has sent its receiver may not have yet (stream.rs):
    /// none where nothing says what arrives, as over UDP.
    fn undelivered(&mut self) -> usize {
        0
    }

    /// The most of a line's bytes one message to its receiver carries, as the receiver
    /// says it takes a message: a longer line goes in parts, each as long as it may be
    /// (D163). Each driver's is its receiver's own bound, cited where it is set; it is the
    /// most a shipper holds of a line, too.
    fn part(&self) -> usize;

    /// Whether `m` goes in one message its receiver takes whole, where what the receiver
    /// bounds is the message as it is written (escaped JSON, UTF-16, the fields beside its
    /// line) rather than the line's bytes: a line, or a part, that does not is cut shorter.
    /// Asked of a line at its end and of each part cut; every message fits where the
    /// receiver bounds no more than [`Driver::part`] does.
    fn fits(&mut self, _m: &Message<'_>) -> io::Result<bool> {
        Ok(true)
    }

    /// How long until it would flush what it holds back though its shipper has nothing
    /// new for it (awslogs' joined lines, which wait so long for more): none where it waits
    /// for nothing.
    fn due(&self) -> Option<Duration> {
        None
    }

    /// Ends its connection once its receiver has all it was sent (stream.rs), or says why
    /// it could not, and is asked again.
    fn close(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        Ok(())
    }
}

/// How a driver waits: a while, or for a socket of its own to be readable, either cut
/// short as the daemon stops.
pub(super) struct Wait<'a>(&'a UnixStream);

impl Wait<'_> {
    /// Waits `d`: whether the daemon stops meanwhile.
    pub(super) fn pause(&self, d: Duration) -> bool {
        #[cfg(unix)]
        {
            stopped_within(self.0, None, d)
        }
        // A wait that cannot be made is taken for a stop, as on Unix.
        #[cfg(windows)]
        {
            let deadline = std::time::Instant::now().checked_add(d);
            self.0.wait_readable(None, deadline).unwrap_or(true)
        }
    }

    /// Waits until `fd` is readable, or `d` has passed; an error where the daemon stops
    /// first.
    #[cfg(unix)]
    pub(super) fn readable(&self, fd: BorrowedFd<'_>, d: Duration) -> io::Result<()> {
        if stopped_within(self.0, Some(fd), d) {
            Err(stops())
        } else {
            Ok(())
        }
    }

    /// Waits until `socket` is readable, or `d` has passed; an error where the daemon stops
    /// first: what it reads and its close as an event (WSAEventSelect), waited on with the
    /// daemon's stop, and let go of after, the socket left not blocking, as its caller has
    /// it.
    #[cfg(windows)]
    pub(super) fn readable(
        &self,
        socket: std::os::windows::io::BorrowedSocket<'_>,
        d: Duration,
    ) -> io::Result<()> {
        use std::os::windows::io::{AsHandle as _, AsRawSocket as _};
        use windows_sys::Win32::Networking::WinSock::{FD_CLOSE, FD_READ, SOCKET, WSAEVENT, WSAEventSelect};
        let raw = SOCKET::try_from(socket.as_raw_socket())
            .map_err(|_| io::Error::other("a socket's value past SOCKET's range"))?;
        let event = shards_ipc::Event::new()?;
        // SAFETY: a socket its caller holds, and an event of ours, which outlives the
        // selection: it is let go of below.
        if unsafe { WSAEventSelect(raw, event.raw() as WSAEVENT, (FD_READ | FD_CLOSE) as i32) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = std::time::Instant::now().checked_add(d);
        let waited = shards_ipc::await_any(&[self.0], &[event.as_handle()], deadline);
        // SAFETY: as above: no event of the socket's is set from here on.
        unsafe { WSAEventSelect(raw, 0, 0) };
        let (stopped, _) = waited?;
        if stopped.first().copied().unwrap_or(false) {
            Err(stops())
        } else {
            Ok(())
        }
    }
}

/// What a wait ended by the daemon's stop says.
pub(super) fn stops() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the daemon stops")
}

/// A driver as a shipper gives it messages: each until it is sent, and a count of them.
struct Sender<'d> {
    id: &'d str,
    driver: &'d mut dyn Driver,
    wait: Wait<'d>,
    /// The messages sent.
    sent: u64,
}

impl Sender<'_> {
    /// Sends `m`, again until it is sent, whatever the driver's failures meanwhile; given
    /// up only as the daemon stops.
    fn send(&mut self, m: &Message<'_>) -> io::Result<()> {
        let (driver, wait) = (&mut *self.driver, &self.wait);
        until_done(self.id, wait, || driver.send(m, wait))?;
        self.sent += 1;
        Ok(())
    }

    /// Whether `m` fits one message of its driver's receiver, asked again until the driver
    /// can say (gcplogs' resource, found as first needed); given up only as the daemon
    /// stops.
    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
        let (driver, wait) = (&mut *self.driver, &self.wait);
        let mut fits = false;
        until_done(self.id, wait, || driver.fits(m).map(|f| fits = f))?;
        Ok(fits)
    }

    /// Sends what its driver holds back, as its shipper has caught up, again until it is
    /// sent; given up only as the daemon stops.
    fn flush(&mut self) -> io::Result<()> {
        let (driver, wait) = (&mut *self.driver, &self.wait);
        until_done(self.id, wait, || driver.flush(wait))
    }

    /// How many of the messages sent their receiver has.
    fn delivered(&mut self) -> u64 {
        let undelivered = u64::try_from(self.driver.undelivered()).unwrap_or(u64::MAX);
        self.sent.saturating_sub(undelivered)
    }

    /// Ends its driver's connection once its receiver has all sent, asked again until it
    /// does; given up only as the daemon stops.
    fn close(&mut self) -> io::Result<()> {
        let (driver, wait) = (&mut *self.driver, &self.wait);
        until_done(self.id, wait, || driver.close(wait))
    }
}

/// Does `f` until it succeeds: again a second after it fails, half as long again after
/// each failure, a minute at most ([`RETRY_FIRST`]); given up only as the daemon stops. A
/// receiver that has not yet done what it was asked (WouldBlock: splunk's indexing) is
/// waited for so too, without a word.
fn until_done(id: &str, wait: &Wait<'_>, mut f: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut pause = RETRY_FIRST;
    loop {
        let Err(e) = f() else {
            return Ok(());
        };
        // The daemon stops: no failure of the driver's.
        if wait.pause(Duration::ZERO) {
            return Err(stops());
        }
        if e.kind() != io::ErrorKind::WouldBlock {
            log(format!(
                "container {id}: sending its log: {e}; again in {pause:?}"
            ));
        }
        if wait.pause(pause) {
            return Err(stops());
        }
        pause = pause.mul_f64(1.5).min(RETRY_MOST);
    }
}

/// How long a driver that could not send waits before it tries again: a second at first,
/// half as long again after each failure, a minute at most, as fluent-logger-golang waits
/// for its server (fluent.go: defaultRetryWait, defaultReconnectWaitIncreRate,
/// defaultMaxRetryWait), through which moby's fluentd driver sends.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MOST: Duration = Duration::from_secs(60);

/// Where a microVM's shipper keeps its place in its log, beside it.
const CURSOR: &str = "ship";
const CURSOR_NEW: &str = "ship.new";

/// The shippers running, by microVM: each one's end, which is told to stop; and the
/// environments that runs' clients gave, by microVM, where its driver ships to a cloud
/// (D168): in memory alone.
#[derive(Default)]
pub(super) struct Shipping {
    running: std::sync::Mutex<HashMap<String, UnixStream>>,
    clouds: std::sync::Mutex<HashMap<String, Vec<String>>>,
}

impl Shipping {
    /// Keeps `run`'s client's environment for microVM `id`'s shipper, where its driver
    /// ships to a cloud (AWS's, Google's): the latest client's, which started it.
    pub(super) fn give(&self, id: &str, run: &shards_ipc::Run) {
        if matches!(crate::logging::driver(run), "awslogs" | "gcplogs") {
            lock(&self.clouds).insert(id.to_string(), run.cloud_env.clone());
        }
    }

    /// Lets microVM `id`'s environment go, as it is removed.
    pub(super) fn forget(&self, id: &str) {
        lock(&self.clouds).remove(id);
    }
}

impl Shipping {
    /// Stops every shipper: as a test's daemon's scope ends.
    #[cfg(test)]
    pub(super) fn end(&self) {
        for stop in lock(&self.running).values() {
            shards_ipc::end(stop);
        }
    }
}

/// Where a shipper's lines go: each message sent, and whether one fits its receiver.
trait Ship {
    fn send(&mut self, m: &Message<'_>) -> io::Result<()>;
    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool>;
}

impl Ship for Sender<'_> {
    fn send(&mut self, m: &Message<'_>) -> io::Result<()> {
        Sender::send(self, m)
    }

    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
        Sender::fits(self, m)
    }
}

/// Each stream's line as its messages go, cut where its receiver bounds a message (D163):
/// a line one message carries whole goes whole; a longer one in parts, each as long as one
/// message carries ([`Driver::part`], [`Driver::fits`]), with an ID in common and their
/// places, the last saying so. A part is cut at a character's boundary where the line is
/// UTF-8 there, so that no character is split across two messages. A line the run left
/// unfinished goes whole, or as its last part. moby's copier cuts every line past 16 KiB
/// whatever its receiver takes (copier.go, defaultBufSize), a character in two among them.
struct Lines {
    open: [Open; 2],
    part: usize,
}

#[derive(Default)]
struct Open {
    held: Vec<u8>,
    at: u64,
    /// Its parts sent so far: their ID, and the next one's place.
    parts: Option<(String, u32)>,
}

impl Lines {
    fn new(part: usize) -> Lines {
        Lines {
            open: Default::default(),
            part: part.max(1),
        }
    }

    /// Takes `piece`, sending each message it completes through `ship`, which returns once
    /// it is sent, or says why it never will be.
    fn take(&mut self, piece: &Piece<'_>, ship: &mut dyn Ship) -> io::Result<()> {
        let stream = piece.stream;
        let part = self.part;
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
        // A part as soon as more is held than one message carries: what is held then is
        // the most a shipper keeps of a line.
        while open.held.len() > part {
            open.cut(stream, part, ship)?;
        }
        if ends {
            open.finish(stream, part, ship)?;
        }
        Ok(())
    }

    /// Sends what is left of each stream's unfinished line, as the run has ended.
    fn finish(&mut self, ship: &mut dyn Ship) -> io::Result<()> {
        let part = self.part;
        for (s, open) in self.open.iter_mut().enumerate() {
            let stream = if s == 1 {
                LOG_STDERR
            } else {
                crate::spec::LOG_STDOUT
            };
            if !open.held.is_empty() || open.parts.is_some() {
                open.finish(stream, part, ship)?;
            }
        }
        Ok(())
    }
}

/// The most of `line`'s first `n` bytes that ends at a character's boundary: `n` less the
/// bytes of a UTF-8 character it would cut in two; `n` itself where none is cut, where the
/// bytes there are no UTF-8, or where the character begins the line.
fn boundary(line: &[u8], n: usize) -> usize {
    let Some(&next) = line.get(n) else {
        return n.min(line.len());
    };
    // Not a continuation byte: no character is cut.
    if next & 0xc0 != 0x80 {
        return n;
    }
    // The character's lead, within the three bytes before.
    for back in 1..=n.min(3) {
        let start = n - back;
        let Some(&lead) = line.get(start) else {
            return n;
        };
        if lead & 0xc0 == 0x80 {
            continue;
        }
        let len = match lead {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return n,
        };
        let whole = line
            .get(start..start + len)
            .is_some_and(|c| std::str::from_utf8(c).is_ok());
        return if whole && start + len > n && start > 0 {
            start
        } else {
            n
        };
    }
    n
}

impl Open {
    /// The length of the line's next part, `id`'s `ordinal`th: the most of its first
    /// `most` bytes, cut at a character's boundary, that one message carries.
    fn longest(
        &self,
        stream: u8,
        most: usize,
        id: &str,
        ordinal: u32,
        ship: &mut dyn Ship,
    ) -> io::Result<usize> {
        let held = &self.held;
        let fits = |n: usize, ship: &mut dyn Ship| {
            ship.fits(&Message {
                stream,
                at: self.at,
                line: held.get(..n).unwrap_or_default(),
                part: Some(crate::logging::fluentd::Part {
                    id,
                    ordinal,
                    last: false,
                }),
            })
        };
        let most = boundary(held, most.min(held.len()));
        if fits(most, ship)? {
            return Ok(most);
        }
        // The longest prefix that fits, by halves: every shorter one fits where a longer
        // one does.
        let (mut lo, mut hi) = (0, most);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if fits(boundary(held, mid), ship)? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let n = boundary(held, lo);
        // At least a character, though none fits: the receiver refuses or cuts it as it
        // does, and the line goes on.
        Ok(if n > 0 {
            n
        } else {
            (1..=held.len().min(4))
                .find(|&n| std::str::from_utf8(held.get(..n).unwrap_or_default()).is_ok())
                .unwrap_or(1)
        })
    }

    /// Sends the longest part of the line that one message carries, of at most `most`
    /// bytes, as the line's next part, not its last.
    fn cut(&mut self, stream: u8, most: usize, ship: &mut dyn Ship) -> io::Result<()> {
        if self.parts.is_none() {
            self.parts = Some((partial_id()?, 1));
        }
        let (id, ordinal) = self.parts.clone().unwrap_or_default();
        let n = self.longest(stream, most, &id, ordinal, ship)?;
        let rest = self.held.split_off(n.min(self.held.len()));
        let part = std::mem::replace(&mut self.held, rest);
        self.send_part(stream, &part, false, ship)
    }

    /// Sends `part` as the line's next part.
    fn send_part(&mut self, stream: u8, part: &[u8], last: bool, ship: &mut dyn Ship) -> io::Result<()> {
        if self.parts.is_none() {
            self.parts = Some((partial_id()?, 1));
        }
        let Some((id, ordinal)) = &mut self.parts else {
            return Ok(());
        };
        ship.send(&Message {
            stream,
            at: self.at,
            line: part,
            part: Some(crate::logging::fluentd::Part {
                id,
                ordinal: *ordinal,
                last,
            }),
        })?;
        *ordinal = ordinal.saturating_add(1);
        Ok(())
    }

    /// Sends the rest of the line: whole where one message carries it, else as its last
    /// part, cut into more first where one message does not carry that either.
    fn finish(&mut self, stream: u8, most: usize, ship: &mut dyn Ship) -> io::Result<()> {
        loop {
            let part = self
                .parts
                .as_ref()
                .map(|(id, ordinal)| crate::logging::fluentd::Part {
                    id,
                    ordinal: *ordinal,
                    last: true,
                });
            let fits = ship.fits(&Message {
                stream,
                at: self.at,
                line: &self.held,
                part,
            })?;
            if fits || self.held.is_empty() {
                break;
            }
            self.cut(stream, most, ship)?;
        }
        let held = std::mem::take(&mut self.held);
        if self.parts.is_some() {
            self.send_part(stream, &held, true, ship)?;
        } else {
            ship.send(&Message {
                stream,
                at: self.at,
                line: &held,
                part: None,
            })?;
        }
        self.parts = None;
        Ok(())
    }
}

/// A line's ID for its parts, in the form receivers that join parts read (D163, moby's
/// stringid.GenerateRandomID): 32 random bytes in hex.
fn partial_id() -> io::Result<String> {
    let mut b = [0u8; 32];
    shards_vmm::platform::fill_random(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
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
/// catches up, and as its run ends; and only as far as its receiver has the messages
/// before it (stream.rs). So a crash, or the daemon's stop, costs sending again at most a
/// segment's lines and what was not yet delivered, never a line lost; a log written on
/// without pause costs a write a segment, not one a line; and where `logs` keeps no copy
/// (`cache-disabled`) the segments wholly delivered go as they are.
#[derive(Default)]
struct Place {
    kept: Option<([Pos; 2], bool)>,
    seq: Option<u64>,
    /// The places read to, oldest first, each with how many messages had been sent by
    /// then: kept once those are delivered.
    read: VecDeque<(u64, [Pos; 2])>,
}

impl Place {
    /// Whether `at` is in another segment than the place kept.
    fn moved(&self, at: [Pos; 2]) -> bool {
        Some(first(at)) != self.seq
    }

    /// Notes that the log is read to `at`, `sent` messages sent by then.
    fn reached(&mut self, sent: u64, at: [Pos; 2]) {
        match self.read.back_mut() {
            Some((s, place)) if *s == sent => *place = at,
            _ => self.read.push_back((sent, at)),
        }
    }

    /// Keeps the furthest place read to whose messages are among the first `delivered`
    /// sent; with `done`, where that is the last, as its run's end, all of it delivered.
    fn keep(&mut self, id: &str, dir: &Path, delivered: u64, done: bool, cached: bool) {
        let mut at = self.kept.map(|(at, _)| at);
        while let Some(&(sent, place)) = self.read.front()
            && sent <= delivered
        {
            at = Some(place);
            self.read.pop_front();
        }
        let Some(at) = at else {
            return;
        };
        let done = done && self.read.is_empty();
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

/// What a shipper following a run waits on: its log's watch, where it has one, its run's
/// end, and the daemon's stop.
struct Waits<'a> {
    watch: Option<&'a FileWatch>,
    end: &'a UnixStream,
    stopped: &'a UnixStream,
}

#[cfg(windows)]
impl Waits<'_> {
    /// Whether any has something now: its watch's event set, or a stream readable.
    fn ready_now(&self) -> bool {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        self.end.readable()
            || self.stopped.readable()
            || self.watch.is_some_and(|w| {
                // SAFETY: the watch's event, which it holds, looked at without waiting.
                unsafe { WaitForSingleObject(w.fd().as_raw_handle(), 0) == WAIT_OBJECT_0 }
            })
    }

    /// Waits until one has something, or `within` has passed, where it is given: which do,
    /// as [`Waits::wait`] on Unix answers. The watch's event is a manual-reset one, which a
    /// wait leaves set (windows_watch.rs).
    fn wait(&self, within: Option<Duration>) -> io::Result<[bool; 3]> {
        let deadline = within.and_then(|d| std::time::Instant::now().checked_add(d));
        let watch: Vec<_> = self.watch.map(FileWatch::fd).into_iter().collect();
        let (streams, set) = shards_ipc::await_any(&[self.end, self.stopped], &watch, deadline)?;
        let at = |v: &[bool], i: usize| v.get(i).copied().unwrap_or(false);
        Ok([at(&set, 0), at(&streams, 0), at(&streams, 1)])
    }
}

#[cfg(unix)]
impl Waits<'_> {
    /// Each as poll(2) takes it: one not given is no descriptor, which it passes over.
    fn fds(&self) -> [Option<BorrowedFd<'_>>; 3] {
        [
            self.watch.map(FileWatch::fd),
            Some(self.end.as_fd()),
            Some(self.stopped.as_fd()),
        ]
    }

    /// Whether any is readable now.
    fn ready_now(&self) -> bool {
        let mut polled = self.fds().map(|fd| libc::pollfd {
            fd: fd.map_or(-1, |fd| fd.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: poll(2), not waiting, on three pollfds of descriptors the caller holds.
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), 3, 0) };
        ready > 0
    }

    /// Waits until one is readable, or `within` has passed, where it is given: which are.
    fn wait(&self, within: Option<Duration>) -> io::Result<[bool; 3]> {
        wait_ready(&self.fds(), within)
    }
}

/// Waits until one of `fds` is readable, or `within` has passed, where it is given:
/// which are. One not given is no descriptor, which poll(2) passes over.
#[cfg(unix)]
fn wait_ready(fds: &[Option<BorrowedFd<'_>>; 3], within: Option<Duration>) -> io::Result<[bool; 3]> {
    let deadline = within.and_then(|d| std::time::Instant::now().checked_add(d));
    let mut polled = fds.map(|fd| libc::pollfd {
        fd: fd.map_or(-1, |fd| fd.as_raw_fd()),
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // Rounded up: a wait of 0 ms would return at once, and spin.
        let ms = match (within, deadline) {
            (None, _) | (Some(_), None) => -1,
            (Some(_), Some(at)) => libc::c_int::try_from(
                at.saturating_duration_since(std::time::Instant::now())
                    .as_micros()
                    .div_ceil(1000),
            )
            .unwrap_or(libc::c_int::MAX),
        };
        // SAFETY: poll(2) on three pollfds of descriptors the caller holds open.
        let n = unsafe { libc::poll(polled.as_mut_ptr(), 3, ms) };
        if n >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(polled.map(|p| p.revents != 0))
}

/// How a shipper's following of a run ended.
enum Shipped {
    /// Its run ended, and all it wrote is sent.
    Ended,
    /// The daemon stops.
    Stopped,
}

/// Waits until `stop` is told, or `also` is readable, or `wait` has passed: whether `stop`
/// was told. A wait poll(2) cannot make is taken for a stop: none is left to wait with.
#[cfg(unix)]
fn stopped_within(stop: &UnixStream, also: Option<BorrowedFd<'_>>, wait: Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    loop {
        // poll(2) passes over a negative descriptor.
        let mut polled = [stop.as_raw_fd(), also.map_or(-1, |fd| fd.as_raw_fd())].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
        // SAFETY: poll(2) on two pollfds, of descriptors the caller holds open or none.
        match unsafe { libc::poll(polled.as_mut_ptr(), 2, ms) } {
            0 => return false,
            n if n > 0 => return polled[0].revents != 0,
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return true,
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// Has microVM `id`'s log sent where its driver says, from where it last got to, on
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
        let cached = crate::logging::kept(&run, super::DEFAULT_LOGS)
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
        let mut lines = Lines::new(driver.part());
        let mut place = Place::default();
        let mut sender = Sender {
            id,
            driver,
            wait: Wait(stopped),
            sent: 0,
        };
        let shipped = (|| -> io::Result<Shipped> {
            // Followed as it is written, while its run runs.
            if let Some((number, end)) = self.wake_at_end(id)? {
                let followed = (|| -> io::Result<Shipped> {
                    // Where no watch can be had (on Linux, its user's inotify instances
                    // spent: PM M257), it reads again every RETRY, as `logs -f` does.
                    let mut watch = FileWatch::new(file.dir()).ok();
                    let mut watched = None;
                    loop {
                        let mut each = |p: Piece<'_>| lines.take(&p, &mut sender).map(|()| true);
                        reader.read(&file, &mut each)?;
                        let at = reader.resume_at();
                        place.reached(sender.sent, at);
                        if place.moved(at) {
                            place.keep(id, dir, sender.delivered(), false, cached);
                        }
                        if let Some(segment) = reader.segment()
                            && watched != Some(segment.seq())
                            && let Some(watch) = watch.as_mut()
                        {
                            watch.file(segment.index())?;
                            watched = Some(segment.seq());
                            continue;
                        }
                        let waits = Waits {
                            watch: watch.as_ref(),
                            end: &end,
                            stopped,
                        };
                        if !waits.ready_now() {
                            sender.flush()?;
                            place.keep(id, dir, sender.delivered(), false, cached);
                        }
                        // Woken as the driver asks, for what it holds back; without a watch,
                        // at least every RETRY.
                        let within = match (sender.driver.due(), &watch) {
                            (due, Some(_)) => due,
                            (Some(due), None) => Some(due.min(super::RETRY)),
                            (None, None) => Some(super::RETRY),
                        };
                        let ready = waits.wait(within)?;
                        if ready[1] {
                            return Ok(Shipped::Ended);
                        }
                        if ready[2] {
                            return Ok(Shipped::Stopped);
                        }
                        if let Some(watch) = &watch {
                            watch.clear();
                        }
                    }
                })();
                self.forget_waiter(id, number);
                if let Shipped::Stopped = followed? {
                    return Ok(Shipped::Stopped);
                }
            }
            // Its run is over: what it left, whole, and its place kept as all of it sent
            // once its receiver has all of it.
            let mut each = |p: Piece<'_>| lines.take(&p, &mut sender).map(|()| true);
            if reader.read(&file, &mut each)? {
                reader.finish(&mut each)?;
                lines.finish(&mut sender)?;
            }
            place.reached(sender.sent, reader.resume_at());
            sender.close()?;
            place.keep(id, dir, sender.delivered(), true, cached);
            Ok(Shipped::Ended)
        })();
        // However else it ends: its place, as far as its receiver has what was sent.
        if !matches!(shipped, Ok(Shipped::Ended)) {
            place.keep(id, dir, sender.delivered(), false, cached);
        }
        shipped
    }

    /// The driver microVM `id`'s run sends its log with, if one sends it elsewhere.
    fn driver_of(&self, id: &str, run: &shards_ipc::Run) -> Result<Option<Box<dyn Driver>>, String> {
        let Some(remote) = crate::logging::remote(run)? else {
            return Ok(None);
        };
        let details = self.log_details(id)?;
        let tag = crate::logging::tag(run, &details.info())?;
        Ok(Some(match remote {
            // macOS keeps nothing sent to its syslogd's socket (PM M374): its own syslog,
            // the local default's, is its unified log, which os_log writes, the run's tag
            // its category (D199).
            #[cfg(target_os = "macos")]
            crate::logging::Remote::Syslog(config) if config.to == crate::logging::syslog::To::Local => {
                Box::new(oslog::Oslog::new(&tag).map_err(|e| e.to_string())?)
            }
            crate::logging::Remote::Syslog(config) => Box::new(syslog::Syslog::new(
                config,
                crate::logging::syslog::From {
                    tag,
                    hostname: details.hostname.clone(),
                    pid: std::process::id(),
                },
            )?),
            crate::logging::Remote::Gelf(config) => {
                let attributes = crate::logging::extra(run, &details.info())?;
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
            crate::logging::Remote::Fluentd(config) => Box::new(fluentd::Fluentd::new(
                config,
                crate::logging::fluentd::From {
                    tag,
                    id: details.id.clone(),
                    name: details.name.clone(),
                    extra: crate::logging::extra(run, &details.info())?,
                },
            )?),
            crate::logging::Remote::Awslogs(config) => {
                let env = lock(&self.shipping.clouds).get(id).cloned().ok_or(
                    "its credentials are found with the environment of the client that starts its run, which no daemon keeps: the rest of its log is sent once it is started again",
                )?;
                // Its stream: as the options name it, else the run's tag, which is the
                // microVM's whole ID unless `tag` says (moby awslogs newStreamConfig).
                let stream = match &config.stream {
                    Some(s) => s.clone(),
                    None => crate::logging::tag_or(run, &details.info(), "{{.FullID}}")?,
                };
                crate::logging::awslogs::stream_name(&stream)
                    .map_err(|why| format!("--log-opt tag: it makes {stream:?} of this run: {why}"))?;
                let entity = config
                    .entity
                    .as_ref()
                    .map(|e| crate::logging::awslogs::render(e, run, &details.info()))
                    .transpose()?;
                Box::new(awslogs::Awslogs::new(config, env, stream, entity)?)
            }
            crate::logging::Remote::Gcplogs(config) => {
                let env = lock(&self.shipping.clouds).get(id).cloned().ok_or(
                    "its credentials are found with the environment of the client that starts its run, which no daemon keeps: the rest of its log is sent once it is started again",
                )?;
                let attributes = crate::logging::extra(run, &details.info())?;
                Box::new(gcplogs::Gcplogs::new(config, env, details.clone(), attributes)?)
            }
            crate::logging::Remote::Splunk(config) => {
                // Its tag, unless `tag` is given empty, which takes it away (moby splunk New).
                let untagged = crate::logging::options(run)
                    .get("tag")
                    .is_some_and(String::is_empty);
                Box::new(splunk::Splunk::new(
                    config,
                    crate::logging::splunk::From {
                        host: details.hostname.clone(),
                        tag: (!untagged).then_some(tag),
                        attrs: crate::logging::extra(run, &details.info())?,
                    },
                )?)
            }
            #[cfg(target_os = "linux")]
            crate::logging::Remote::Journald => {
                // Its epoch, which its ordinals count from: an ID of its own, the field
                // journal readers group a run's entries by (CONTAINER_LOG_EPOCH, D166).
                let epoch = crate::containers::new_id().map_err(|e| e.to_string())?;
                let from = crate::logging::journald::From::new(
                    &details.id,
                    &details.name,
                    &details.image_name,
                    &tag,
                    &epoch,
                    &crate::logging::extra(run, &details.info())?,
                );
                if !from.left_out.is_empty() {
                    log(format!(
                        "container {id}: its log's attributes {} are left out of the journal: no journal field can be named so, or the field is the driver's own",
                        from.left_out.join(", ")
                    ));
                }
                Box::new(journald::Journald::new(from))
            }
            #[cfg(not(target_os = "linux"))]
            crate::logging::Remote::Journald => {
                return Err(
                    "--log-driver journald: the journal is systemd's, on Linux, and this host keeps none"
                        .into(),
                );
            }
            #[cfg(windows)]
            crate::logging::Remote::Etwlogs => {
                Box::new(etwlogs::Etwlogs::new(&details).map_err(|e| e.to_string())?)
            }
            #[cfg(not(windows))]
            crate::logging::Remote::Etwlogs => {
                return Err(
                    "--log-driver etwlogs: Event Tracing for Windows is Windows' own, and this host keeps none"
                        .into(),
                );
            }
            #[cfg(target_os = "macos")]
            crate::logging::Remote::Oslog => Box::new(oslog::Oslog::new(&tag).map_err(|e| e.to_string())?),
            #[cfg(not(target_os = "macos"))]
            crate::logging::Remote::Oslog => {
                return Err(
                    "--log-driver oslog: the unified log is macOS's own, and this host keeps none".into(),
                );
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

    /// A receiver for the tests: what is sent, each part's ID and place checked, and
    /// whether a message fits, as its test says.
    struct Seen<F: FnMut(&Message<'_>) -> bool> {
        sent: Vec<(u8, u64, Vec<u8>)>,
        ids: [Option<String>; 2],
        fits: F,
        asked: usize,
    }

    impl<F: FnMut(&Message<'_>) -> bool> Ship for Seen<F> {
        fn send(&mut self, m: &Message<'_>) -> io::Result<()> {
            let s = usize::from(m.stream == LOG_STDERR);
            match &m.part {
                Some(p) => {
                    let id = self.ids[s].get_or_insert_with(|| p.id.to_string());
                    assert_eq!((p.id.len(), &**id), (64, p.id), "one ID a line");
                    if p.last {
                        self.ids[s] = None;
                    }
                }
                None => assert!(self.ids[s].is_none(), "a whole line amid a line's parts"),
            }
            self.sent.push((m.stream, m.at, m.line.to_vec()));
            Ok(())
        }

        fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
            self.asked += 1;
            Ok((self.fits)(m))
        }
    }

    /// The messages `pieces` make, at most `part` bytes each, then the run's end: each its
    /// stream, time and bytes; each part's place and whether it is the last, every part of
    /// a line one ID; and how many times a fit was asked.
    fn cut_by(
        part: usize,
        fits: impl FnMut(&Message<'_>) -> bool,
        pieces: &[(u8, u64, bool, &[u8])],
    ) -> (Vec<(u8, u64, Vec<u8>)>, usize) {
        let mut seen = Seen {
            sent: Vec::new(),
            ids: [None, None],
            fits,
            asked: 0,
        };
        let mut lines = Lines::new(part);
        for &(stream, at, first, bytes) in pieces {
            lines
                .take(
                    &Piece {
                        stream,
                        at,
                        first,
                        bytes,
                    },
                    &mut seen,
                )
                .unwrap();
        }
        lines.finish(&mut seen).unwrap();
        (seen.sent, seen.asked)
    }

    fn cut(part: usize, pieces: &[(u8, u64, bool, &[u8])]) -> Vec<(u8, u64, Vec<u8>)> {
        cut_by(part, |_| true, pieces).0
    }

    /// A line no longer than its receiver's message carries goes whole, one of exactly
    /// that length too; a longer one in parts that long, then what is left; each stream
    /// its own; and a line the run left unfinished whole.
    #[test]
    fn long_lines_go_in_parts_as_long_as_their_receiver_takes() {
        const PART: usize = 16;
        let x = |n: usize| vec![b'x'; n];
        let line = |n: usize| [x(n), b"\n".to_vec()].concat();
        assert_eq!(
            cut(PART, &[(LOG_STDOUT, 1, true, b"out\n")]),
            [(LOG_STDOUT, 1, b"out".to_vec())]
        );
        assert_eq!(
            cut(PART, &[(LOG_STDOUT, 1, true, &line(PART))]),
            [(LOG_STDOUT, 1, x(PART))]
        );
        assert_eq!(
            cut(PART, &[(LOG_STDOUT, 1, true, &line(PART + 1))]),
            [(LOG_STDOUT, 1, x(PART)), (LOG_STDOUT, 1, x(1))]
        );
        // 40 bytes, as the reader gives them, in pieces, each with the line's time.
        let long = line(40);
        let sent = cut(
            PART,
            &[
                (LOG_STDOUT, 7, true, &long[..20]),
                (LOG_STDERR, 8, true, b"err\n"),
                (LOG_STDOUT, 9, false, &long[20..]),
            ],
        );
        assert_eq!(
            sent,
            [
                (LOG_STDOUT, 7, x(PART)),
                (LOG_STDERR, 8, b"err".to_vec()),
                (LOG_STDOUT, 7, x(PART)),
                (LOG_STDOUT, 7, x(40 - 2 * PART)),
            ]
        );
        assert_eq!(
            cut(PART, &[(LOG_STDERR, 3, true, b"no end")]),
            [(LOG_STDERR, 3, b"no end".to_vec())]
        );
    }

    /// A part is cut at a character's boundary: a character that would cross it goes
    /// whole into the next part; bytes that are no UTF-8 are cut where they fall.
    #[test]
    fn a_part_never_cuts_a_character_in_two() {
        let mut line = vec![b'x'; 9];
        line.extend("€".as_bytes()); // bytes 9 to 11
        line.extend(b"yz\n");
        let lens = |sent: Vec<(u8, u64, Vec<u8>)>| sent.into_iter().map(|(_, _, b)| b).collect::<Vec<_>>();
        let parts = lens(cut(10, &[(LOG_STDOUT, 1, true, &line)]));
        assert_eq!(parts, [vec![b'x'; 9], "€yz".as_bytes().to_vec()]);
        let mut bad = vec![b'x'; 9];
        bad.extend([0x80, 0x80, 0x80, b'\n']);
        let parts = lens(cut(10, &[(LOG_STDOUT, 1, true, &bad)]));
        assert_eq!(parts.iter().map(Vec::len).collect::<Vec<_>>(), [10, 2]);
        assert_eq!(
            boundary("€".as_bytes(), 1),
            1,
            "a character the line begins with is cut"
        );
    }

    /// Where a receiver bounds a message as it is written, a line that fits goes whole,
    /// however close to its bound; a longer one in the longest parts that fit, each part
    /// asked as a part; and a line well within its bound is asked once.
    #[test]
    fn parts_are_the_longest_their_receiver_takes_as_written() {
        // A receiver that takes 25 bytes a message, each byte of the line written twice,
        // and a part's place 4 bytes more.
        let fits = |m: &Message<'_>| m.line.len() * 2 + if m.part.is_some() { 4 } else { 0 } <= 25;
        let (sent, asked) = cut_by(1000, fits, &[(LOG_STDOUT, 1, true, b"0123456789ab\n")]);
        assert_eq!(sent, [(LOG_STDOUT, 1, b"0123456789ab".to_vec())]);
        assert_eq!(asked, 1);
        let (sent, _) = cut_by(
            1000,
            fits,
            &[(LOG_STDOUT, 1, true, b"0123456789abcdefghijklmnopqrstu\n")],
        );
        let parts: Vec<&[u8]> = sent.iter().map(|(_, _, b)| &b[..]).collect();
        assert_eq!(parts, [&b"0123456789"[..], b"abcdefghij", b"klmnopqrst", b"u"]);
        // A receiver that takes nothing still gets every character, one a message.
        let (sent, _) = cut_by(
            1000,
            |m: &Message<'_>| m.line.is_empty(),
            &[(LOG_STDOUT, 1, true, "a€\n".as_bytes())],
        );
        let parts: Vec<&[u8]> = sent.iter().map(|(_, _, b)| &b[..]).collect();
        assert_eq!(parts, [&b"a"[..], "€".as_bytes(), b""]);
    }

    /// What cutting a long line costs the shipper (PM M494): a line of 1 MiB, cut in 16 KiB
    /// parts as moby's copier cuts every line, in syslog's parts (no fit asked), and in the
    /// longest GELF UDP messages that 128 chunks carry, compressed by gzip or not, of bytes
    /// that compress and of bytes that do not; each case `SHARDS_CUT_ROUNDS` times (30),
    /// its time a line as n, p50, p90, p99 and max, and its parts. Run with `--ignored
    /// --nocapture` under the release profile.
    #[test]
    #[ignore = "a measurement (PM M494)"]
    fn what_cutting_a_long_line_costs() {
        use crate::logging::gelf::{self, Compression, Config, To};
        let rounds: usize = std::env::var("SHARDS_CUT_ROUNDS")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(30);
        let mut random = vec![0u8; 1 << 20];
        shards_vmm::platform::fill_random(&mut random).unwrap();
        let same = vec![b'x'; 1 << 20];
        let from = gelf::From {
            host: "host.example".into(),
            extra: "\"_container_id\":\"0123456789abcdef\"".into(),
        };
        let gelf_fits = |compression| {
            let config = Config {
                to: To::Udp("h:1".into()),
                compression,
                level: 1,
                reconnects: 3,
                delay: 1,
            };
            move |m: &Message<'_>| {
                config
                    .fits(&gelf::message(&from, m.stream, m.at, m.line))
                    .unwrap()
            }
        };
        let cases: Vec<(&str, usize, Box<dyn Fn(&Message<'_>) -> bool>, &[u8])> = vec![
            (
                "16 KiB parts (moby's copier)",
                16 * 1024,
                Box::new(|_: &Message<'_>| true),
                &same,
            ),
            (
                "syslog's parts",
                8192 - 100,
                Box::new(|_: &Message<'_>| true),
                &same,
            ),
            (
                "gelf udp, none, compressible",
                gelf::UDP_MOST,
                Box::new(gelf_fits(Compression::None)),
                &same,
            ),
            (
                "gelf udp, gzip, compressible",
                gelf::UDP_MOST,
                Box::new(gelf_fits(Compression::Gzip)),
                &same,
            ),
            (
                "gelf udp, gzip, random",
                gelf::UDP_MOST,
                Box::new(gelf_fits(Compression::Gzip)),
                &random,
            ),
        ];
        for (name, part, fits, bytes) in cases {
            let line = [bytes, b"\n"].concat();
            let mut took = Vec::new();
            let mut parts = 0;
            for _ in 0..rounds {
                let started = std::time::Instant::now();
                let (sent, _) = cut_by(part, |m| fits(m), &[(LOG_STDOUT, 1, true, &line)]);
                took.push(started.elapsed());
                parts = sent.len();
            }
            took.sort();
            let at = |q: f64| took[((took.len() - 1) as f64 * q).round() as usize];
            println!(
                "{name}: {parts} parts; a line: n {} p50 {:?} p90 {:?} p99 {:?} max {:?}",
                took.len(),
                at(0.5),
                at(0.9),
                at(0.99),
                took[took.len() - 1]
            );
        }
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

    /// A shipper keeps its place only as far as its receiver has the messages before it,
    /// and keeps it as its run's end, all sent, only once its receiver has them all.
    #[test]
    fn a_place_is_kept_as_far_as_its_messages_are_delivered() {
        let dir = shards_testdir::TempDir::new("ship-place").unwrap();
        let at = |n: u8| [Pos::from_bytes(&[n; 24]); 2];
        let mut place = Place::default();
        place.reached(0, at(1));
        place.reached(3, at(2));
        // Read on, no message sent since.
        place.reached(3, at(3));
        place.reached(5, at(4));
        // None delivered: the place before the first message.
        place.keep("c", &dir, 0, false, true);
        assert_eq!(read_cursor(&dir), Some((at(1), false)));
        // Four delivered: the furthest place read to with three sent.
        place.keep("c", &dir, 4, false, true);
        assert_eq!(read_cursor(&dir), Some((at(3), false)));
        // Its run's end, one not yet delivered: not all sent.
        place.keep("c", &dir, 4, true, true);
        assert_eq!(read_cursor(&dir), Some((at(3), false)));
        place.keep("c", &dir, 5, true, true);
        assert_eq!(read_cursor(&dir), Some((at(4), true)));
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
