// ROOT-only Linux framing fixture. Contracts and invocation: adjacent README.md.
// `rustc --test` excludes the entire native module, including its libc dependency.
use std::time::Duration;

const WORD: usize = std::mem::size_of::<u32>();
const HEADER: usize = 4 * WORD; // Linux inotify_event: s32 wd; u32 mask,cookie,len.

#[derive(Debug, PartialEq, Eq)]
enum DecodeFault {
    Offset,
    Overflow,
    Header,
    Name,
    MissingNul,
}

#[derive(Debug, PartialEq, Eq)]
struct Frame<'a> {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: &'a [u8], // Includes the UAPI's NUL and padding; never assumed UTF-8.
    raw: &'a [u8],
}

fn checked_end(start: usize, length: usize) -> Result<usize, DecodeFault> {
    start.checked_add(length).ok_or(DecodeFault::Overflow)
}

fn word(bytes: &[u8], offset: usize) -> Result<[u8; WORD], DecodeFault> {
    bytes
        .get(offset..checked_end(offset, WORD)?)
        .ok_or(DecodeFault::Header)?
        .try_into()
        .map_err(|_| DecodeFault::Header)
}

fn decode_next(bytes: &[u8], offset: usize) -> Result<Option<(Frame<'_>, usize)>, DecodeFault> {
    if offset > bytes.len() {
        return Err(DecodeFault::Offset);
    }
    if offset == bytes.len() {
        return Ok(None);
    }
    let name_start = checked_end(offset, HEADER)?;
    let header = bytes.get(offset..name_start).ok_or(DecodeFault::Header)?;
    // Copies into scalar arrays: neither the input nor the next record needs alignment.
    let wd = i32::from_ne_bytes(word(header, 0)?);
    let mask = u32::from_ne_bytes(word(header, WORD)?);
    let cookie = u32::from_ne_bytes(word(header, 2 * WORD)?);
    let length =
        usize::try_from(u32::from_ne_bytes(word(header, 3 * WORD)?)).map_err(|_| DecodeFault::Overflow)?;
    let end = checked_end(name_start, length)?;
    let name = bytes.get(name_start..end).ok_or(DecodeFault::Name)?;
    if !name.is_empty() && !name.contains(&0) {
        return Err(DecodeFault::MissingNul);
    }
    let raw = bytes.get(offset..end).ok_or(DecodeFault::Name)?;
    Ok(Some((
        Frame {
            wd,
            mask,
            cookie,
            name,
            raw,
        },
        end,
    )))
}

#[derive(Debug, PartialEq, Eq)]
enum TimeFault {
    Deadline,
    Overflow,
}

fn remaining(deadline_ns: u64, now_ns: u64) -> Result<Duration, TimeFault> {
    let ns = deadline_ns
        .checked_sub(now_ns)
        .filter(|v| *v > 0)
        .ok_or(TimeFault::Deadline)?;
    Ok(Duration::from_nanos(ns))
}

fn poll_millis(left: Duration) -> Result<i32, TimeFault> {
    if left.is_zero() {
        return Err(TimeFault::Deadline);
    }
    // poll's unit is milliseconds: round up, then clamp only to its signed ABI bound.
    let ms = left
        .as_millis()
        .checked_add(u128::from(left.subsec_nanos() % 1_000_000 != 0))
        .ok_or(TimeFault::Overflow)?;
    i32::try_from(ms.min(i32::MAX as u128)).map_err(|_| TimeFault::Overflow)
}

#[cfg(all(target_os = "linux", not(test)))]
mod native {
    use super::*;
    use std::ffi::{CString, OsStr};
    use std::fs::{DirBuilder, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::{Component, PathBuf};

    #[derive(Debug)]
    enum Fault {
        Input(&'static str),
        Io {
            operation: &'static str,
            errno: Option<i32>,
        },
        Allocation,
        Time(TimeFault),
        Decode(DecodeFault),
        Contract(&'static str),
    }

    impl std::fmt::Display for Fault {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Input(cause) => write!(out, "input: {cause}"),
                Self::Io { operation, errno } => write!(out, "{operation}: errno={errno:?}"),
                Self::Allocation => out.write_str("notification-buffer allocation refused"),
                Self::Time(cause) => write!(out, "time: {cause:?}"),
                Self::Decode(cause) => write!(out, "framing: {cause:?}"),
                Self::Contract(cause) => write!(out, "scenario: {cause}"),
            }
        }
    }
    type Result<T> = std::result::Result<T, Fault>;

    fn io_fault(operation: &'static str, error: io::Error) -> Fault {
        Fault::Io {
            operation,
            errno: error.raw_os_error(),
        }
    }

    fn log(out: &mut impl Write, message: std::fmt::Arguments<'_>) -> Result<()> {
        out.write_fmt(message)
            .and_then(|()| out.write_all(b"\n"))
            .map_err(|e| io_fault("report", e))
    }

    struct Config {
        directory: PathBuf,
        file: PathBuf,
        cfile: CString,
        deadline: u64,
        writes: usize,
        read_bytes: usize,
    }

    fn positive<T: std::str::FromStr + Default + PartialEq>(v: &OsStr) -> Result<T> {
        let value = v
            .to_str()
            .and_then(|s| s.parse::<T>().ok())
            .ok_or(Fault::Input("positive integer required"))?;
        if value == T::default() {
            return Err(Fault::Input("zero is refused"));
        }
        Ok(value)
    }

    impl Config {
        fn read() -> Result<Self> {
            let mut args = std::env::args_os().skip(1);
            let directory = PathBuf::from(args.next().ok_or(Fault::Input("NEW_DIRECTORY"))?);
            if !directory.is_absolute()
                || directory.file_name().is_none()
                || directory
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return Err(Fault::Input(
                    "new absolute private directory without dot components required",
                ));
            }
            let deadline = positive(&args.next().ok_or(Fault::Input("DEADLINE_MONOTONIC_NS"))?)?;
            let writes = positive(&args.next().ok_or(Fault::Input("WRITE_COUNT"))?)?;
            let read_bytes = positive(&args.next().ok_or(Fault::Input("READ_BYTES"))?)?;
            if args.next().is_some() || read_bytes < HEADER || read_bytes > isize::MAX as usize {
                return Err(Fault::Input(
                    "exactly four arguments; HEADER <= READ_BYTES <= isize::MAX",
                ));
            }
            let file = directory.join("watched");
            let cfile = CString::new(file.as_os_str().as_bytes()).map_err(|_| Fault::Input("path NUL"))?;
            Ok(Self {
                directory,
                file,
                cfile,
                deadline,
                writes,
                read_bytes,
            })
        }
    }

    fn now() -> Result<u64> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is writable, correctly aligned and lives through this call.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return Err(io_fault("clock_gettime", io::Error::last_os_error()));
        }
        let sec = u64::try_from(ts.tv_sec).map_err(|_| Fault::Time(TimeFault::Overflow))?;
        let ns = u64::try_from(ts.tv_nsec).map_err(|_| Fault::Time(TimeFault::Overflow))?;
        if ns >= 1_000_000_000 {
            return Err(Fault::Time(TimeFault::Overflow));
        }
        sec.checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(ns))
            .ok_or(Fault::Time(TimeFault::Overflow))
    }

    fn left(config: &Config) -> Result<Duration> {
        remaining(config.deadline, now()?).map_err(Fault::Time)
    }

    #[derive(Debug)]
    enum Watch {
        Absent,
        Active(i32),
        Removed(i32),
        Ignored(i32),
    }

    #[derive(Debug)]
    enum Paths {
        Absent,
        DirectoryCreated,
        FileCreated,
    }

    struct Resources {
        file: Option<File>,
        notify: Option<OwnedFd>,
        watch: Watch,
        paths: Paths,
        // Created artifacts are deliberately retained on every outcome. No unlink,
        // recursive removal or deletion of a possibly replaced pathname occurs.
    }

    impl Resources {
        fn new() -> Self {
            Self {
                file: None,
                notify: None,
                watch: Watch::Absent,
                paths: Paths::Absent,
            }
        }

        fn fd(&self) -> Result<i32> {
            self.notify
                .as_ref()
                .map(AsRawFd::as_raw_fd)
                .ok_or(Fault::Contract("notify closed"))
        }

        fn remove_watch(&mut self, out: &mut impl Write) -> Result<()> {
            let Watch::Active(wd) = self.watch else {
                return Ok(());
            };
            // SAFETY: this instance owns both the open descriptor and this watch.
            let rc = unsafe { libc::inotify_rm_watch(self.fd()?, wd) };
            let errno = if rc < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            if rc == 0 {
                self.watch = Watch::Removed(wd);
            }
            log(out, format_args!("remove_watch wd={wd} rc={rc} errno={errno:?}"))?;
            if rc < 0 {
                return Err(Fault::Io {
                    operation: "inotify_rm_watch",
                    errno,
                });
            }
            Ok(())
        }

        fn close(&mut self, out: &mut impl Write) -> Result<()> {
            let removal = self.remove_watch(out);
            let mut first = removal.err();
            if let Some(fd) = self.notify.take() {
                let fd = fd.into_raw_fd();
                // SAFETY: ownership was taken out of OwnedFd; close exactly once.
                // On Linux close errors do not justify retrying a possibly reused fd.
                let rc = unsafe { libc::close(fd) };
                let errno = if rc < 0 {
                    io::Error::last_os_error().raw_os_error()
                } else {
                    None
                };
                if let Err(e) = log(
                    out,
                    format_args!(
                        "close_notify fd={fd} rc={rc} errno={errno:?} watch={:?}",
                        self.watch
                    ),
                ) {
                    first.get_or_insert(e);
                }
                if rc < 0 {
                    first.get_or_insert(Fault::Io {
                        operation: "close_notify",
                        errno,
                    });
                }
            }
            if let Some(file) = self.file.take() {
                let fd = file.into_raw_fd();
                // SAFETY: the file descriptor's sole Rust owner has been consumed.
                let rc = unsafe { libc::close(fd) };
                let errno = if rc < 0 {
                    io::Error::last_os_error().raw_os_error()
                } else {
                    None
                };
                if let Err(e) = log(out, format_args!("close_writer fd={fd} rc={rc} errno={errno:?}")) {
                    first.get_or_insert(e);
                }
                if rc < 0 {
                    first.get_or_insert(Fault::Io {
                        operation: "close_writer",
                        errno,
                    });
                }
            }
            if let Err(e) = log(out, format_args!("paths_retained ownership={:?}", self.paths)) {
                first.get_or_insert(e);
            }
            match first {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }

    fn wait(config: &Config, fd: i32, out: &mut impl Write) -> Result<()> {
        loop {
            let timeout = poll_millis(left(config)?).map_err(Fault::Time)?;
            let mut entry = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live, writable pollfd; timeout is finite, nonnegative.
            let rc = unsafe { libc::poll(&mut entry, 1, timeout) };
            let errno = if rc < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            log(
                out,
                format_args!(
                    "poll timeout_ms={timeout} rc={rc} revents={} errno={errno:?}",
                    entry.revents
                ),
            )?;
            left(config)?;
            if errno == Some(libc::EINTR) || rc == 0 {
                continue;
            }
            if rc < 0 {
                return Err(Fault::Io {
                    operation: "poll",
                    errno,
                });
            }
            if entry.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(Fault::Contract("poll terminal/error readiness"));
            }
            if entry.revents & libc::POLLIN == 0 {
                return Err(Fault::Contract("poll returned without read readiness"));
            }
            return Ok(());
        }
    }

    #[derive(Debug)]
    enum ReadOutcome {
        Bytes(usize),
        Errno(i32),
    }

    fn read(config: &Config, fd: i32, buffer: &mut [u8], out: &mut impl Write) -> Result<ReadOutcome> {
        loop {
            left(config)?;
            // SAFETY: buffer is writable for its exact supplied count; fd is owned.
            let rc = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            let errno = if rc < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            log(
                out,
                format_args!("read requested={} rc={rc} errno={errno:?}", buffer.len()),
            )?;
            if errno == Some(libc::EINTR) {
                continue;
            }
            if rc < 0 {
                return errno
                    .map(ReadOutcome::Errno)
                    .ok_or(Fault::Contract("missing read errno"));
            }
            let count = usize::try_from(rc).map_err(|_| Fault::Contract("read count"))?;
            if count == 0 {
                return Err(Fault::Contract("unexpected inotify EOF"));
            }
            if count > buffer.len() {
                return Err(Fault::Contract("read beyond buffer"));
            }
            return Ok(ReadOutcome::Bytes(count));
        }
    }

    fn frames(
        bytes: &[u8],
        wd: i32,
        watch: &mut Watch,
        modifies: &mut usize,
        out: &mut impl Write,
    ) -> Result<()> {
        let mut offset = 0;
        while let Some((frame, end)) = decode_next(bytes, offset).map_err(Fault::Decode)? {
            log(
                out,
                format_args!(
                    "frame wd={} mask={} cookie={} name_len={} raw={:?}",
                    frame.wd,
                    frame.mask,
                    frame.cookie,
                    frame.name.len(),
                    frame.raw
                ),
            )?;
            if frame.mask & libc::IN_Q_OVERFLOW != 0 {
                return Err(Fault::Contract(
                    "queue overflow: notification completeness unknown",
                ));
            }
            if frame.wd != wd || frame.cookie != 0 || !frame.name.is_empty() {
                return Err(Fault::Contract("unexpected private file-inode frame"));
            }
            match (&*watch, frame.mask) {
                (Watch::Removed(current), libc::IN_MODIFY) if *current == wd => {
                    *modifies = modifies
                        .checked_add(1)
                        .ok_or(Fault::Contract("frame counter overflow"))?;
                }
                (Watch::Removed(current), libc::IN_IGNORED) if *current == wd => *watch = Watch::Ignored(wd),
                _ => return Err(Fault::Contract("unexpected frame or watch transition")),
            }
            offset = end;
        }
        Ok(())
    }

    fn exercise(config: &Config, owned: &mut Resources, out: &mut impl Write) -> Result<()> {
        left(config)?;
        if HEADER != std::mem::size_of::<libc::inotify_event>() {
            return Err(Fault::Contract("libc/UAPI header mismatch"));
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(config.read_bytes)
            .map_err(|_| Fault::Allocation)?;
        buffer.resize(config.read_bytes, 0);
        log(
            out,
            format_args!(
                "config deadline_monotonic_ns={} header_bytes={HEADER} writes={} read_bytes={}",
                config.deadline, config.writes, config.read_bytes
            ),
        )?;
        left(config)?;
        DirBuilder::new()
            .mode(0o700)
            .create(&config.directory)
            .map_err(|e| io_fault("create_new_directory", e))?;
        owned.paths = Paths::DirectoryCreated;
        log(
            out,
            format_args!(
                "directory_created path={:?} mode_requested=0700",
                config.directory
            ),
        )?;
        left(config)?;
        owned.file = Some(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config.file)
                .map_err(|e| io_fault("create_new_file", e))?,
        );
        owned.paths = Paths::FileCreated;
        let identity = owned
            .file
            .as_ref()
            .ok_or(Fault::Contract("writer absent"))?
            .metadata()
            .map_err(|e| io_fault("writer_metadata", e))?;
        log(
            out,
            format_args!(
                "file_created path={:?} dev={} ino={} read_bytes={} writes_requested={}",
                config.file,
                identity.dev(),
                identity.ino(),
                config.read_bytes,
                config.writes
            ),
        )?;
        left(config)?;
        // SAFETY: no pointers; returned descriptor is immediately uniquely owned.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io_fault("inotify_init1", io::Error::last_os_error()));
        }
        // SAFETY: fd is a fresh descriptor returned by inotify_init1.
        owned.notify = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        left(config)?;
        // SAFETY: config owns a live NUL-terminated path and an open instance.
        let wd = unsafe { libc::inotify_add_watch(fd, config.cfile.as_ptr(), libc::IN_MODIFY) };
        if wd < 0 {
            return Err(io_fault("inotify_add_watch", io::Error::last_os_error()));
        }
        owned.watch = Watch::Active(wd);
        log(
            out,
            format_args!("watch_active fd={fd} wd={wd} mask={}", libc::IN_MODIFY),
        )?;
        let writer = owned.file.as_mut().ok_or(Fault::Contract("writer absent"))?;
        for index in 0..config.writes {
            loop {
                left(config)?;
                // One successful one-byte write per requested write; no hidden seed write.
                match writer.write(&[1]) {
                    Ok(1) => {
                        log(out, format_args!("write index={index} rc=1 errno=None"))?;
                        break;
                    }
                    Ok(n) => {
                        return Err(if n == 0 {
                            Fault::Contract("zero file write")
                        } else {
                            Fault::Contract("file write count")
                        });
                    }
                    Err(e) => {
                        let errno = e.raw_os_error();
                        log(out, format_args!("write index={index} rc=-1 errno={errno:?}"))?;
                        if e.kind() != io::ErrorKind::Interrupted {
                            return Err(io_fault("file_write", e));
                        }
                    }
                }
            }
        }
        // A separately opened reader verifies both inode identity and actual file bytes.
        left(config)?;
        let mut reader = File::open(&config.file).map_err(|e| io_fault("independent_open", e))?;
        let observed = reader.metadata().map_err(|e| io_fault("reader_metadata", e))?;
        if (observed.dev(), observed.ino()) != (identity.dev(), identity.ino()) {
            return Err(Fault::Contract("reader inode changed"));
        }
        for _ in 0..config.writes {
            let mut byte = [0];
            loop {
                left(config)?;
                match reader.read(&mut byte) {
                    Ok(1) => break,
                    Ok(_) => return Err(Fault::Contract("independent readback premature EOF")),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(io_fault("independent_read", e)),
                }
            }
            if byte != [1] {
                return Err(Fault::Contract("independent byte mismatch"));
            }
        }
        left(config)?;
        let mut extra = [0];
        if reader
            .read(&mut extra)
            .map_err(|e| io_fault("independent_eof", e))?
            != 0
        {
            return Err(Fault::Contract("independent readback extra bytes"));
        }
        log(
            out,
            format_args!(
                "readback bytes={} matched=true eof=true dev={} ino={}",
                config.writes,
                observed.dev(),
                observed.ino()
            ),
        )?;
        drop(reader);
        wait(config, fd, out)?;
        let small = buffer
            .get_mut(..HEADER - 1)
            .ok_or(Fault::Contract("subheader buffer"))?;
        if !matches!(read(config, fd, small, out)?, ReadOutcome::Errno(libc::EINVAL)) {
            return Err(Fault::Contract("under-header pending read did not refuse EINVAL"));
        }
        wait(config, fd, out)?;
        left(config)?;
        owned.remove_watch(out)?; // Distinct IN_IGNORED follows the retained IN_MODIFY.
        let mut modifies = 0;
        // HEADER+1, when admitted, cannot fit two unnamed headers: a positive
        // short prefix with IN_IGNORED still queued, independent of coalescing.
        let prefix = config
            .read_bytes
            .min(HEADER.checked_add(1).ok_or(Fault::Contract("probe size"))?);
        let target = buffer.get_mut(..prefix).ok_or(Fault::Contract("prefix buffer"))?;
        let ReadOutcome::Bytes(count) = read(config, fd, target, out)? else {
            return Err(Fault::Contract("retained first record missing"));
        };
        if count != HEADER {
            return Err(Fault::Contract("file-inode first record not header-only"));
        }
        frames(
            target.get(..count).ok_or(Fault::Contract("prefix count"))?,
            wd,
            &mut owned.watch,
            &mut modifies,
            out,
        )?;
        if modifies != 1 || !matches!(owned.watch, Watch::Removed(_)) {
            return Err(Fault::Contract("undersized read did not retain original modify"));
        }
        wait(config, fd, out)?;
        log(
            out,
            format_args!(
                "retention_witness=true short_positive={} still_ready_after_prefix=true",
                count < prefix
            ),
        )?;
        // The pending IN_IGNORED also makes an exact-header positive read possible.
        let target = buffer.get_mut(..HEADER).ok_or(Fault::Contract("header buffer"))?;
        let ReadOutcome::Bytes(count) = read(config, fd, target, out)? else {
            return Err(Fault::Contract("exact-header pending record missing"));
        };
        if count != HEADER {
            return Err(Fault::Contract("exact-header framing"));
        }
        frames(target, wd, &mut owned.watch, &mut modifies, out)?;
        // A successful prefix, including a short one, never substitutes for this
        // nonblocking drain using the caller's full requested read envelope.
        loop {
            match read(config, fd, &mut buffer, out)? {
                ReadOutcome::Bytes(count) => frames(
                    buffer.get(..count).ok_or(Fault::Contract("drain count"))?,
                    wd,
                    &mut owned.watch,
                    &mut modifies,
                    out,
                )?,
                ReadOutcome::Errno(libc::EAGAIN) => {
                    if !matches!(owned.watch, Watch::Ignored(current) if current == wd) {
                        return Err(Fault::Contract("drained before IN_IGNORED"));
                    }
                    log(
                        out,
                        format_args!(
                            "drained_witness=EAGAIN writes={} modify_frames={} coalescing_possible=true exact_header_read=true",
                            config.writes, modifies
                        ),
                    )?;
                    break;
                }
                ReadOutcome::Errno(errno) => {
                    return Err(Fault::Io {
                        operation: "drain",
                        errno: Some(errno),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn main() -> i32 {
        let mut out = io::stdout().lock();
        let mut owned = Resources::new();
        let result = Config::read().and_then(|config| exercise(&config, &mut owned, &mut out));
        let cleanup = owned.close(&mut out);
        let passed = result.is_ok() && cleanup.is_ok();
        // Faults are typed internally; Debug is the final private fixture presentation.
        let reported = writeln!(
            out,
            "terminal result={result:?} cleanup={cleanup:?} passed={passed}"
        )
        .is_ok();
        if passed && reported { 0 } else { 1 }
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn main() {
    std::process::exit(native::main());
}

#[cfg(all(not(target_os = "linux"), not(test)))]
fn main() {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "unsupported: Linux native notification fixture"
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(wd: i32, mask: u32, name: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&wd.to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&u32::try_from(name.len()).unwrap().to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes
    }

    #[test]
    fn unaligned_header_and_signed_watch() {
        let mut storage = vec![0xff];
        storage.extend(record(-1, 0x4000, &[]));
        let bytes = &storage[1..];
        let (frame, end) = decode_next(bytes, 0).unwrap().unwrap();
        assert_eq!((frame.wd, frame.mask, end), (-1, 0x4000, HEADER));
        assert_eq!(frame.raw, bytes);
        assert_eq!(decode_next(bytes, end), Ok(None));
    }

    #[test]
    fn concatenated_frames_and_non_utf8_name() {
        let mut bytes = record(7, 2, &[0xff, 0, 0]);
        bytes.extend(record(7, 0x8000, &[]));
        let (first, next) = decode_next(&bytes, 0).unwrap().unwrap();
        assert_eq!(first.name, &[0xff, 0, 0]);
        let (last, end) = decode_next(&bytes, next).unwrap().unwrap();
        assert_eq!(last.mask, 0x8000);
        assert_eq!(end, bytes.len());
    }

    #[test]
    fn every_header_truncation_and_past_end_refused() {
        let bytes = record(7, 2, &[]);
        assert_eq!(decode_next(&[], 0), Ok(None));
        for length in 1..HEADER {
            assert_eq!(decode_next(&bytes[..length], 0), Err(DecodeFault::Header));
        }
        assert_eq!(decode_next(&bytes, bytes.len() + 1), Err(DecodeFault::Offset));
    }

    #[test]
    fn every_name_truncation_refused() {
        let bytes = record(7, 2, b"abc\0");
        for length in HEADER..bytes.len() {
            assert_eq!(decode_next(&bytes[..length], 0), Err(DecodeFault::Name));
        }
        assert!(decode_next(&bytes, 0).is_ok());
    }

    #[test]
    fn unterminated_name_and_huge_claim_refused() {
        assert_eq!(
            decode_next(&record(7, 2, b"abc"), 0),
            Err(DecodeFault::MissingNul)
        );
        let mut bytes = record(7, 2, &[]);
        bytes[3 * WORD..HEADER].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(matches!(
            decode_next(&bytes, 0),
            Err(DecodeFault::Name | DecodeFault::Overflow)
        ));
    }

    #[test]
    fn malformed_tail_is_not_silently_drained() {
        let mut bytes = record(7, 2, &[]);
        bytes.push(0);
        let (_, end) = decode_next(&bytes, 0).unwrap().unwrap();
        assert_eq!(decode_next(&bytes, end), Err(DecodeFault::Header));
    }

    #[test]
    fn checked_range_overflow() {
        assert_eq!(checked_end(usize::MAX, 1), Err(DecodeFault::Overflow));
        assert_eq!(checked_end(usize::MAX - HEADER, HEADER), Ok(usize::MAX));
    }

    #[test]
    fn deadline_never_resets() {
        assert_eq!(remaining(10, 9), Ok(Duration::from_nanos(1)));
        assert_eq!(remaining(10, 10), Err(TimeFault::Deadline));
        assert_eq!(remaining(10, 11), Err(TimeFault::Deadline));
    }

    #[test]
    fn poll_rounds_up_and_clamps_only_abi_bound() {
        assert_eq!(poll_millis(Duration::ZERO), Err(TimeFault::Deadline));
        assert_eq!(poll_millis(Duration::from_nanos(1)), Ok(1));
        assert_eq!(poll_millis(Duration::from_millis(1)), Ok(1));
        assert_eq!(poll_millis(Duration::from_nanos(1_000_001)), Ok(2));
        assert_eq!(poll_millis(Duration::from_secs(u64::MAX)), Ok(i32::MAX));
    }
}
