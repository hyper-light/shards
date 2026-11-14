//! The host's memory pressure, as its kernel says it (docs/design/architecture.md D188): what
//! a daemon has its microVMs give back what they hold cold at. Each word is followed by an
//! act of its listener's (a pass of give-backs), whose cost it tells back ([`acted`]): what
//! a pressure is, and how often one that goes on is said again, follow that cost, learned
//! where it runs (R03), never a figure of ours.
//!
//! - Linux: PSI triggers (Documentation/accounting/psi.rst) on the host's memory and on this
//!   process's own cgroup's, where it may arm one, over the least window the kernel lets an
//!   unprivileged writer watch, 2 s (`psi_trigger_create`: a multiple of 2 s, which the
//!   kernel checks in its own averaging work, `PSI_FREQ`, with no real-time thread of its
//!   own; Linux 6.4, d82caa273565). A window's stall is pressure once it is as long as the
//!   act it brings ([`stall_of`]). A kernel that arms none (before 6.4, unprivileged) has
//!   the same trigger kept here: `some total`, which the kernel brings up to date at each
//!   read (`psi_show`), read each window from a first read at once.
//! - macOS: libdispatch's memory pressure source (dispatch/source.h), said again while the
//!   system's pressure stays raised on D215's pace ([`repeat_after`]).
//! - Windows: the system's low memory notification (CreateMemoryResourceNotification,
//!   `LowMemoryResourceNotification`), signalled while its available physical memory is low,
//!   said again while it stays so on the same pace.
//!
//! [`acted`]: MemoryPressure::acted

use std::io;
use std::time::Duration;

/// The window a trigger watches (above): the least an unprivileged writer may arm, as
/// shards runs as its user; a pressure that goes on is heard once each.
#[cfg(target_os = "linux")]
const WINDOW_US: u64 = 2_000_000;

/// The stall within a trigger's window that is pressure, in µs (Linux): as long as the act
/// it brings took last, `acted` (rent or buy: act once what waiting has lost is what acting
/// costs, which costs at most twice what the best choice in hindsight would; Karlin,
/// Manasse, Rudolph and Sleator, "Competitive snoopy caching", Algorithmica 3, 1988). Both
/// are time: the time some task stalled on memory, and the time the host's give-backs took.
/// Before any act (the cold start), the kernel's least, a microsecond (`psi_trigger_create`:
/// a threshold of more than none): the first stall is heard, and its act teaches the next.
/// Never past the window, which is the most a threshold may be.
#[cfg(target_os = "linux")]
fn stall_of(acted: Option<Duration>) -> u64 {
    acted.map_or(1, |d| {
        u64::try_from(d.as_micros())
            .unwrap_or(u64::MAX)
            .clamp(1, WINDOW_US)
    })
}

/// When a pressure raised since `raised` ago is said again (macOS, Windows): D215's pace
/// (`shards_abi::pace`), `sqrt(2 c w)` of the cost `c` of acting on a word and the time `w`
/// it has been raised so far, which stands for the time it has still to go on. Saying it
/// each `p`, a pressure has `w / p` more acts, `c w / p` of their time, and what its
/// microVMs come to hold cold within a pause is given back `p / 2` late on average: the
/// least of their sum. `c` is the act its listener last told ([`MemoryPressure::acted`]), or
/// before any, the watcher's own look at the system's word, what a word costs where no act
/// follows it.
#[cfg(any(target_os = "macos", windows, test))]
fn repeat_after(acted: Option<Duration>, look: Duration, raised: Duration) -> Duration {
    shards_abi::pace::pause(acted.unwrap_or(look), raised, Duration::MAX)
}

/// The host's memory pressure, watched: [`wait`](Self::wait) returns each time it is short
/// of memory.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct MemoryPressure {
    /// The triggers armed, by the file each watches: each readable (POLLPRI) as its stall
    /// passes, at most once a window.
    triggers: Vec<(String, std::fs::File)>,
    /// Triggers armed since on the cgroups its [`PressureWatches`] were given, and what
    /// wakes [`wait`](Self::wait) to take them (an eventfd).
    armed: std::sync::mpsc::Receiver<(String, std::fs::File)>,
    wake: std::fs::File,
    /// The stall a window is pressure at, in µs ([`stall_of`]), each trigger armed at it.
    stall_us: u64,
    /// With no trigger armed: the host's `some total` as last read, in µs.
    sampled: Option<u64>,
}

/// What has a [`MemoryPressure`] watch more cgroups, from any thread.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct PressureWatches {
    armed: std::sync::mpsc::Sender<(String, std::fs::File)>,
    wake: std::fs::File,
}

#[cfg(target_os = "linux")]
impl MemoryPressure {
    const HOST: &'static str = "/proc/pressure/memory";

    /// Watches the host's memory, and this process's cgroup's where it may arm a trigger
    /// on it (one delegated to its user, as systemd delegates a user's own): a cgroup held
    /// to a budget is short of memory before its host says so, which counts a stall of its
    /// against every CPU busy elsewhere.
    pub fn new() -> io::Result<(MemoryPressure, PressureWatches)> {
        use std::os::fd::FromRawFd as _;
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::")).map(str::to_string))
            .filter(|path| path != "/")
            .map(|path| format!("/sys/fs/cgroup{path}/memory.pressure"));
        let stall_us = stall_of(None);
        let mut triggers = Vec::new();
        let mut refused = None;
        for path in std::iter::once(Self::HOST.to_string()).chain(own) {
            match arm(&path, stall_us) {
                Ok(file) => triggers.push((path, file)),
                Err(e) if path == Self::HOST => refused = Some(e),
                Err(_) => {}
            }
        }
        // Where the kernel has PSI and arms no trigger of this user's: its stall so far,
        // read at once, from which the first window's is counted.
        let sampled = if triggers.is_empty() {
            Some(stalled().map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "{}: {e}; and no trigger: {}",
                        Self::HOST,
                        refused.map_or_else(|| "none tried".to_string(), |r| r.to_string())
                    ),
                )
            })?)
        } else {
            None
        };
        // SAFETY: eventfd(2) with flags; its descriptor is owned by the file made of it.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor eventfd(2) just made, owned here alone.
        let wake = unsafe { std::fs::File::from_raw_fd(raw) };
        let (sender, armed) = std::sync::mpsc::channel();
        let watches = PressureWatches {
            armed: sender,
            wake: wake.try_clone()?,
        };
        Ok((
            MemoryPressure {
                triggers,
                armed,
                wake,
                stall_us,
                sampled,
            },
            watches,
        ))
    }

    /// What it watches, for its daemon's log.
    pub fn describe(&self) -> String {
        let at = if self.stall_us == stall_of(None) {
            "any stall, until a give-back has taught what one costs".to_string()
        } else {
            format!("{} µs, as long as a give-back took last", self.stall_us)
        };
        if self.triggers.is_empty() {
            return format!(
                "{}'s stall, read each {} s as a trigger of this user's would watch it: {at}",
                Self::HOST,
                WINDOW_US / 1_000_000
            );
        }
        let watched: Vec<&str> = self.triggers.iter().map(|(w, _)| w.as_str()).collect();
        format!(
            "{}: some task stalled on memory in {} s, {at}",
            watched.join(" and "),
            WINDOW_US / 1_000_000
        )
    }

    /// The most a pressure that goes on waits between words: a trigger's window, as the
    /// kernel says one at most once a window and again the next while its stall goes on.
    pub fn repeat(&self) -> Option<Duration> {
        Some(Duration::from_micros(WINDOW_US))
    }

    /// Its listener's act on the last word took `took` (a pass of give-backs, from its ask
    /// to its answer): the stall that is pressure from now on ([`stall_of`]), each trigger
    /// armed at it again. One that cannot be armed again stays armed as it was. An act of
    /// no time teaches nothing.
    pub fn acted(&mut self, took: Duration) {
        if took.is_zero() {
            return;
        }
        let stall_us = stall_of(Some(took));
        if stall_us == self.stall_us {
            return;
        }
        self.stall_us = stall_us;
        for (path, file) in &mut self.triggers {
            if let Ok(again) = arm(path, stall_us) {
                *file = again;
            }
        }
    }

    /// Waits until the host, or a cgroup watched, is short of memory. A trigger whose
    /// cgroup has gone (POLLERR) is let go of; with none armed, the host's stall is read
    /// each window instead.
    pub fn wait(&mut self) -> io::Result<()> {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        loop {
            self.take_armed();
            if self.triggers.is_empty() {
                let before = match self.sampled {
                    Some(before) => before,
                    None => stalled()?,
                };
                std::thread::sleep(Duration::from_micros(WINDOW_US));
                let now = stalled()?;
                self.sampled = Some(now);
                if now.saturating_sub(before) >= self.stall_us {
                    return Ok(());
                }
                continue;
            }
            let mut polled: Vec<libc::pollfd> = std::iter::once((self.wake.as_raw_fd(), libc::POLLIN))
                .chain(self.triggers.iter().map(|(_, f)| (f.as_raw_fd(), libc::POLLPRI)))
                .map(|(fd, events)| libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                })
                .collect();
            let n = libc::nfds_t::try_from(polled.len()).map_err(io::Error::other)?;
            // SAFETY: poll(2) on pollfds of files this holds open, `n` of them.
            if unsafe { libc::poll(polled.as_mut_ptr(), n, -1) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let Some((woken, triggers)) = polled.split_first() else {
                continue;
            };
            if woken.revents & libc::POLLIN != 0 {
                let mut count = [0u8; 8];
                let _ = (&self.wake).read(&mut count);
            }
            let pressed = triggers
                .iter()
                .any(|p| p.revents & libc::POLLPRI != 0 && p.revents & libc::POLLERR == 0);
            let mut gone = triggers.iter().map(|p| p.revents & libc::POLLERR != 0);
            self.triggers.retain(|_| !gone.next().unwrap_or(false));
            if pressed {
                return Ok(());
            }
        }
    }

    /// Takes the triggers armed since on cgroups its watches were given, each once, armed
    /// again at the stall that is pressure now where it was armed at another.
    fn take_armed(&mut self) {
        while let Ok((path, file)) = self.armed.try_recv() {
            if self.triggers.iter().any(|(watched, _)| *watched == path) {
                continue;
            }
            let file = if self.stall_us == stall_of(None) {
                file
            } else {
                arm(&path, self.stall_us).unwrap_or(file)
            };
            self.triggers.push((path, file));
        }
    }
}

#[cfg(target_os = "linux")]
impl PressureWatches {
    /// Has the memory pressure of the cgroup at `dir` (its directory in the cgroup v2
    /// hierarchy) watched too, its trigger armed here: one this process may not arm is
    /// said, and nothing is watched of it.
    pub fn watch(&self, dir: &std::path::Path) -> io::Result<()> {
        use std::io::Write as _;
        let path = dir.join("memory.pressure").display().to_string();
        let file = arm(&path, stall_of(None))?;
        self.armed
            .send((path, file))
            .map_err(|_| io::Error::other("the memory pressure watcher has ended"))?;
        (&self.wake).write_all(&1u64.to_ne_bytes())
    }
}

/// A trigger on `path`, a PSI file, at a stall of `stall_us` a window: its file, readable
/// (POLLPRI) as the stall passes.
#[cfg(target_os = "linux")]
fn arm(path: &str, stall_us: u64) -> io::Result<std::fs::File> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    // With its NUL, as psi.rst's example writes it.
    file.write_all(format!("some {stall_us} {WINDOW_US}\0").as_bytes())?;
    Ok(file)
}

/// How long some task has been stalled on memory since the host booted, in µs
/// (`/proc/pressure/memory`'s `some total`, brought up to date as it is read).
#[cfg(target_os = "linux")]
fn stalled() -> io::Result<u64> {
    total_of(&std::fs::read_to_string(MemoryPressure::HOST)?)
}

/// The `some total` of a PSI file's `text`.
#[cfg(any(target_os = "linux", test))]
fn total_of(text: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("some "))
        .and_then(|l| l.split_whitespace().find_map(|kv| kv.strip_prefix("total=")))
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no `some total`"))
}

/// The host's memory pressure, watched: [`wait`](Self::wait) returns each time it is short
/// of memory.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct MemoryPressure {
    /// Held for its handler, which goes as it drops.
    _source: dispatch::Source,
    /// What the source's handler writes each level it hears to.
    levels: io::PipeReader,
    /// Since when the last level heard is raised (warning or critical); none while normal.
    raised: Option<std::time::Instant>,
    /// What its listener's last act took ([`acted`](Self::acted)), and what its own last
    /// look at a word took, for the pace it says a raised pressure again at.
    acted: Option<Duration>,
    look: Duration,
}

/// What has a [`MemoryPressure`] watch more: on macOS, nothing, as it has no cgroups; the
/// system's pressure is all there is.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct PressureWatches;

#[cfg(target_os = "macos")]
impl PressureWatches {
    pub fn watch(&self, _dir: &std::path::Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl MemoryPressure {
    pub fn new() -> io::Result<(MemoryPressure, PressureWatches)> {
        let (levels, writer) = io::pipe()?;
        let source = dispatch::Source::new(writer)?;
        Ok((
            MemoryPressure {
                _source: source,
                levels,
                raised: None,
                acted: None,
                look: Duration::ZERO,
            },
            PressureWatches,
        ))
    }

    pub fn describe(&self) -> String {
        "the system's memory pressure (libdispatch), said again while raised on D215's pace of what acting on it costs"
            .to_string()
    }

    /// The most a pressure that goes on waits between words now: its pace ([`repeat_after`])
    /// while raised, none while it is not.
    pub fn repeat(&self) -> Option<Duration> {
        self.raised
            .map(|since| repeat_after(self.acted, self.look, since.elapsed()))
    }

    /// Its listener's act on the last word took `took`: the cost its pace weighs. An act
    /// of no time teaches nothing.
    pub fn acted(&mut self, took: Duration) {
        if !took.is_zero() {
            self.acted = Some(took);
        }
    }

    /// Waits until the system's memory pressure is raised, or its pace after the last time
    /// it said so while it still is.
    pub fn wait(&mut self) -> io::Result<()> {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        loop {
            // poll(2) counts in milliseconds: a pace rounded up to them, never short of it.
            let timeout = self.repeat().map_or(-1, |p| {
                i32::try_from(p.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
            });
            let mut polled = libc::pollfd {
                fd: self.levels.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) on one pollfd of a pipe this holds open.
            let n = unsafe { libc::poll(&mut polled, 1, timeout) };
            let looked = std::time::Instant::now();
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                // Still raised, as it was a pace ago.
                self.look = looked.elapsed();
                return Ok(());
            }
            let mut heard = [0u8; 16];
            let got = self.levels.read(&mut heard)?;
            if got == 0 {
                return Err(io::Error::other("the memory pressure source ended"));
            }
            let Some(&last) = heard.get(..got).and_then(<[u8]>::last) else {
                continue;
            };
            let raised = last & (dispatch::WARN | dispatch::CRITICAL) != 0;
            let was = self.raised;
            self.raised = match (raised, was) {
                (false, _) => None,
                (true, Some(since)) => Some(since),
                (true, None) => Some(looked),
            };
            self.look = looked.elapsed();
            if raised && was.is_none() {
                return Ok(());
            }
        }
    }
}

/// The host's memory pressure, watched: [`wait`](Self::wait) returns each time it is short
/// of memory.
#[cfg(windows)]
#[derive(Debug)]
pub struct MemoryPressure {
    /// Signalled while the system's available memory is low.
    low: std::os::windows::io::OwnedHandle,
    /// Since when it was low when last looked at; none while it was not.
    raised: Option<std::time::Instant>,
    /// What its listener's last act took ([`acted`](Self::acted)), and what its own last
    /// look took, for the pace it says a lasting low again at.
    acted: Option<Duration>,
    look: Duration,
}

/// What has a [`MemoryPressure`] watch more: on Windows, nothing, as on macOS: the system's
/// pressure is all there is.
#[cfg(windows)]
#[derive(Debug)]
pub struct PressureWatches;

#[cfg(windows)]
impl PressureWatches {
    pub fn watch(&self, _dir: &std::path::Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl MemoryPressure {
    pub fn new() -> io::Result<(MemoryPressure, PressureWatches)> {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::Memory::{
            CreateMemoryResourceNotification, LowMemoryResourceNotification,
        };
        // SAFETY: the system's own notification object, opened for this process.
        let raw = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok((
            MemoryPressure {
                // SAFETY: a handle just opened, which nothing else owns.
                low: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) },
                raised: None,
                acted: None,
                look: Duration::ZERO,
            },
            PressureWatches,
        ))
    }

    pub fn describe(&self) -> String {
        "the system's low memory notification (CreateMemoryResourceNotification), said again while it lasts on D215's pace of what acting on it costs"
            .to_string()
    }

    /// The most a lasting low waits between words now: its pace ([`repeat_after`]) while
    /// low, none while it is not.
    pub fn repeat(&self) -> Option<Duration> {
        self.raised
            .map(|since| repeat_after(self.acted, self.look, since.elapsed()))
    }

    /// Its listener's act on the last word took `took`: the cost its pace weighs. An act
    /// of no time teaches nothing.
    pub fn acted(&mut self, took: Duration) {
        if !took.is_zero() {
            self.acted = Some(took);
        }
    }

    /// Waits until the system's memory is low, or its pace after the last time it said so
    /// while it still is.
    pub fn wait(&mut self) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::INFINITE;
        if let Some(pace) = self.repeat() {
            std::thread::sleep(pace);
            let looked = std::time::Instant::now();
            let low = self.signalled(0)?;
            self.look = looked.elapsed();
            if low {
                // Still low, as it was a pace ago.
                return Ok(());
            }
            self.raised = None;
        }
        if self.signalled(INFINITE)? {
            self.raised = Some(std::time::Instant::now());
        }
        Ok(())
    }

    /// Whether the notification is signalled within `ms` milliseconds: the system's memory
    /// low (the object is signalled while it is, CreateMemoryResourceNotification), as
    /// QueryMemoryResourceNotification would say it, and an event that stands in for it
    /// says it too.
    fn signalled(&self, ms: u32) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: a wait on the object this holds.
        match unsafe { WaitForSingleObject(self.low.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!("a wait answered {other}"))),
        }
    }
}

/// libdispatch's memory pressure source (dispatch/source.h), libSystem's: no framework is
/// loaded for it.
#[cfg(target_os = "macos")]
mod dispatch {
    use std::ffi::c_void;
    use std::io::{self, Write as _};

    /// DISPATCH_MEMORYPRESSURE_NORMAL, _WARN and _CRITICAL.
    const NORMAL: usize = 0x01;
    pub const WARN: u8 = 0x02;
    pub const CRITICAL: u8 = 0x04;
    /// QOS_CLASS_UTILITY (sys/qos.h).
    const UTILITY: isize = 0x11;

    #[repr(C)]
    struct SourceType {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        static _dispatch_source_type_memorypressure: SourceType;
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
        fn dispatch_source_create(
            kind: *const SourceType,
            handle: usize,
            mask: usize,
            queue: *mut c_void,
        ) -> *mut c_void;
        fn dispatch_source_get_data(source: *mut c_void) -> usize;
        fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
        fn dispatch_source_set_event_handler_f(
            source: *mut c_void,
            handler: unsafe extern "C" fn(*mut c_void),
        );
        fn dispatch_source_set_cancel_handler_f(
            source: *mut c_void,
            handler: unsafe extern "C" fn(*mut c_void),
        );
        fn dispatch_resume(object: *mut c_void);
        fn dispatch_source_cancel(source: *mut c_void);
        fn dispatch_release(object: *mut c_void);
    }

    /// What the source's handlers are given: its source, and where each level goes.
    struct Handler {
        source: *mut c_void,
        writer: io::PipeWriter,
    }

    /// A memory pressure source, its handlers writing each level it hears to a pipe: once
    /// dropped, cancelled, and its handler's state freed by its cancel handler, which runs
    /// after the last event handler has.
    #[derive(Debug)]
    pub struct Source(*mut c_void);

    // SAFETY: a dispatch source may be cancelled and released from any thread.
    unsafe impl Send for Source {}

    impl Source {
        pub fn new(writer: io::PipeWriter) -> io::Result<Source> {
            use std::os::fd::AsRawFd as _;
            // A handler never waits on a reader that is behind: a level it cannot write is
            // one the reader has others of to read.
            // SAFETY: fcntl(2) on a pipe this holds open.
            let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            let set = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
            if set < 0 {
                return Err(io::Error::last_os_error());
            }
            let mask = NORMAL | usize::from(WARN) | usize::from(CRITICAL);
            // SAFETY: a global queue, which is never released; then a source of the
            // memory pressure type, its handle unused (0) as the header says.
            let source = unsafe {
                let queue = dispatch_get_global_queue(UTILITY, 0);
                dispatch_source_create(&raw const _dispatch_source_type_memorypressure, 0, mask, queue)
            };
            if source.is_null() {
                return Err(io::Error::other(
                    "dispatch_source_create: no memory pressure source",
                ));
            }
            let handler = Box::into_raw(Box::new(Handler { source, writer }));
            // SAFETY: the source was just created and is not resumed yet; its context is
            // the handler, freed by its cancel handler alone.
            unsafe {
                dispatch_set_context(source, handler.cast());
                dispatch_source_set_event_handler_f(source, heard);
                dispatch_source_set_cancel_handler_f(source, cancelled);
                dispatch_resume(source);
            }
            Ok(Source(source))
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            // SAFETY: the source this created, cancelled once and released once.
            unsafe {
                dispatch_source_cancel(self.0);
                dispatch_release(self.0);
            }
        }
    }

    /// A level heard: written to the pipe, its low byte the flags.
    unsafe extern "C" fn heard(context: *mut c_void) {
        // SAFETY: the context is the source's handler, freed only by its cancel handler,
        // which runs after every event handler has returned.
        let handler = unsafe { &*context.cast::<Handler>() };
        // SAFETY: its source lives until its cancel handler has run.
        let level = unsafe { dispatch_source_get_data(handler.source) };
        let _ = (&handler.writer).write(&[u8::try_from(level & 0xff).unwrap_or(0)]);
    }

    /// The source cancelled: its handler's state freed, its pipe's end with it.
    unsafe extern "C" fn cancelled(context: *mut c_void) {
        // SAFETY: the handler Source::new made, freed once, here, after the last event
        // handler.
        drop(unsafe { Box::from_raw(context.cast::<Handler>()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's memory pressure can be watched where shards' daemon runs: on Linux, a
    /// trigger armed on the host's memory (a kernel of 6.4 or later), or its stall read;
    /// on macOS, a source made and dropped; on Windows, the system's notification of low
    /// memory opened, unsignalled on a host with a gigabyte available.
    #[test]
    fn the_hosts_memory_pressure_can_be_watched() {
        let watched = MemoryPressure::new();
        if cfg!(target_os = "linux") && !std::path::Path::new("/proc/pressure/memory").exists() {
            assert!(watched.is_err());
            return;
        }
        let (watched, _) = watched.unwrap();
        assert!(!watched.describe().is_empty());
        // Windows' notification is of low memory: unsignalled while a gigabyte is available,
        // where the low one is raised at 64 MB at most by default and the high one is
        // signalled from three times that (CreateMemoryResourceNotification's documentation).
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
            // SAFETY: MEMORYSTATUSEX is plain data, zeroes a valid value of it.
            let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
            status.dwLength = u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap();
            // SAFETY: a struct of the length it says.
            assert_ne!(unsafe { GlobalMemoryStatusEx(&raw mut status) }, 0);
            if status.ullAvailPhys >= 1 << 30 {
                assert!(
                    !watched.signalled(0).unwrap(),
                    "low, with {} bytes available",
                    status.ullAvailPhys
                );
            }
        }
    }

    /// A window's stall is pressure once it is as long as the act it brings took (rent or
    /// buy): before any act, the kernel's least threshold, a microsecond; an act of 40 ms,
    /// 40,000 µs; one past the window, the window, the most a threshold may be.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_windows_stall_is_pressure_once_it_costs_what_acting_does() {
        assert_eq!(stall_of(None), 1);
        assert_eq!(stall_of(Some(Duration::from_millis(40))), 40_000);
        assert_eq!(stall_of(Some(Duration::from_nanos(1))), 1);
        assert_eq!(stall_of(Some(Duration::from_secs(60))), WINDOW_US);
        assert_eq!(stall_of(Some(Duration::MAX)), WINDOW_US);
    }

    /// Where no trigger is armed, the stall is read from `some total`, never an average.
    #[test]
    fn a_psi_files_stall_so_far_is_its_some_total() {
        let text = "some avg10=7.50 avg60=1.00 avg300=0.10 total=1234567\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=99\n";
        assert_eq!(total_of(text).unwrap(), 1_234_567);
        assert!(total_of("full avg10=0.00 total=5\n").is_err());
        assert!(total_of("some avg10=7.50\n").is_err());
    }

    /// A lasting pressure is said again on D215's pace of what acting on it costs and how
    /// long it has been raised: a 50 ms act, a second raised, 316 ms on; four seconds
    /// raised, 632 ms; before any act, the watcher's own look's cost stands for it; none
    /// raised yet, at once.
    #[test]
    fn a_lasting_pressure_is_said_again_as_its_act_weighs_against_how_long_it_lasted() {
        let ms = Duration::from_millis;
        let us = Duration::from_micros;
        assert_eq!(
            repeat_after(Some(ms(50)), us(3), ms(1000)),
            Duration::from_nanos(316_227_766)
        );
        assert_eq!(
            repeat_after(Some(ms(50)), us(3), ms(4000)),
            Duration::from_nanos(632_455_532)
        );
        assert_eq!(
            repeat_after(None, us(3), ms(1000)),
            Duration::from_nanos(2_449_489)
        );
        assert_eq!(repeat_after(Some(ms(50)), us(3), Duration::ZERO), Duration::ZERO);
    }

    /// On Windows each low-memory signal is a word, one that lasts said again on its pace,
    /// each pause no shorter than the pace at the word before, and none while memory is not
    /// low: an event stands in for the system's notification, a waitable object signalled
    /// while its memory is low (CreateMemoryResourceNotification).
    #[cfg(windows)]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn each_low_memory_signal_is_a_word_and_one_that_lasts_is_said_again() {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use std::time::Instant;
        use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};
        // SAFETY: a manual-reset event, unsignalled, owned below.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!raw.is_null());
        // SAFETY: the handle just made, which nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(raw) };
        let low = |on: bool| {
            // SAFETY: the event made above.
            let done = unsafe {
                if on {
                    SetEvent(event.as_raw_handle())
                } else {
                    ResetEvent(event.as_raw_handle())
                }
            };
            assert_ne!(done, 0);
        };
        let act = Duration::from_millis(20);
        let mut pressure = MemoryPressure {
            low: event.try_clone().unwrap(),
            raised: None,
            acted: None,
            look: Duration::ZERO,
        };
        let (said, words) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(move || {
                for _ in 0..5 {
                    pressure.wait().unwrap();
                    pressure.acted(act);
                    said.send(Instant::now()).unwrap();
                }
            });
            assert!(
                words.recv_timeout(Duration::from_millis(300)).is_err(),
                "a word unasked"
            );
            low(true);
            let first = words.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut last = first;
            for _ in 0..3 {
                let again = words.recv_timeout(Duration::from_secs(5)).unwrap();
                let pace = repeat_after(Some(act), Duration::ZERO, last.duration_since(first));
                assert!(
                    again.duration_since(last) >= pace,
                    "said again {:?} on, its pace {pace:?}",
                    again.duration_since(last)
                );
                last = again;
            }
            low(false);
            assert!(
                words.recv_timeout(Duration::from_secs(2)).is_err(),
                "a word with memory not low"
            );
            low(true);
            words.recv_timeout(Duration::from_secs(5)).unwrap();
        });
    }
}
