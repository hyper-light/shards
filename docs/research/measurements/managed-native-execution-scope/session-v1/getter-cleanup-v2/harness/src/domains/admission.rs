//! Pre-launch verification of the existing D59 caps, not reservations or guarantees.
//!
//! Linux v6.18 cgroup-v2.rst, page_counter_memparse and page_counter.h define
//! controller delegation, page rounding and the numeric value rendered as `max`.
//! See docs/research/measurements/managed-resource-admission for scope and evidence.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd};

pub(crate) mod execution;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limit {
    Max,
    Value(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Io { errno: i32 },
    ControllerUnavailable,
    WrongFilesystem,
    InvalidName,
    InvalidNumber,
    Overflow,
    Allocation,
    PageSize,
    Readback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rollback {
    NotCreated,
    Removed,
    Failed(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub cause: Cause,
    file: &'static CStr,
    expected: Option<Limit>,
    observed: Option<Limit>,
    rollback: Rollback,
}

impl Refusal {
    fn new(file: &'static CStr, cause: Cause) -> Self {
        Self {
            cause,
            file,
            expected: None,
            observed: None,
            rollback: Rollback::NotCreated,
        }
    }

    fn io(file: &'static CStr, error: io::Error) -> Self {
        Self::new(
            file,
            Cause::Io {
                errno: error.raw_os_error().unwrap_or(libc::EIO),
            },
        )
    }

    /// The existing start API returns presentation Strings. Reserve their exact size
    /// fallibly; even a diagnostic allocation failure must still return Err to it.
    pub(super) fn message(self, label: &str) -> String {
        use std::fmt::Write as _;
        struct Length(usize);
        impl std::fmt::Write for Length {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                self.0 = self.0.checked_add(s.len()).ok_or(std::fmt::Error)?;
                Ok(())
            }
        }
        let mut length = Length(0);
        if write!(length, "{label}: resource admission {self}").is_err() {
            return String::new();
        }
        let mut message = String::new();
        if message.try_reserve_exact(length.0).is_err() {
            return message;
        }
        if write!(Reserved(&mut message), "{label}: resource admission {self}").is_err() {
            return String::new();
        }
        message
    }
}

struct Reserved<'a>(&'a mut String);

impl std::fmt::Write for Reserved<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let remaining = self
            .0
            .capacity()
            .checked_sub(self.0.len())
            .ok_or(std::fmt::Error)?;
        if text.len() > remaining {
            return Err(std::fmt::Error);
        }
        // push_str cannot grow: the complete UTF-8 fragment fits actual capacity.
        self.0.push_str(text);
        Ok(())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: {:?}, expected {:?}, observed {:?}, rollback {:?}",
            self.file, self.cause, self.expected, self.observed, self.rollback
        )
    }
}

impl std::error::Error for Refusal {}

fn open_at(parent: &File, name: &'static CStr, flags: i32) -> Result<File, Refusal> {
    // SAFETY: name is a fixed NUL-terminated control name under the held directory.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(Refusal::io(name, io::Error::last_os_error()));
    }
    // SAFETY: openat returned a fresh descriptor owned only here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn run_group() -> Result<File, Refusal> {
    // SAFETY: the existing guest run authority's fixed absolute path.
    let fd = unsafe {
        libc::open(
            c"/sys/fs/cgroup/run".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(Refusal::io(c"run", io::Error::last_os_error()));
    }
    // SAFETY: open returned a fresh descriptor owned only here.
    let file = unsafe { File::from_raw_fd(fd) };
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs initializes fs on success; the directory remains held.
    if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(Refusal::io(c"run", io::Error::last_os_error()));
    }
    // SAFETY: fstatfs succeeded above.
    if i128::from(unsafe { fs.assume_init() }.f_type) != i128::from(libc::CGROUP2_SUPER_MAGIC) {
        return Err(Refusal::new(c"run", Cause::WrongFilesystem));
    }
    Ok(file)
}

fn has_word(mut reader: impl io::Read, word: &[u8]) -> io::Result<bool> {
    let (mut matched, mut length) = (true, 0);
    let mut byte = [0];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        let [byte] = byte;
        if byte.is_ascii_whitespace() {
            if matched && length == word.len() {
                return Ok(true);
            }
            (matched, length) = (true, 0);
        } else if matched {
            matched = word.get(length) == Some(&byte);
            if matched {
                length += 1;
            }
        }
    }
    Ok(matched && length == word.len())
}

fn controller(parent: &File, file: &'static CStr, word: &[u8]) -> Result<bool, Refusal> {
    has_word(open_at(parent, file, libc::O_RDONLY)?, word).map_err(|e| Refusal::io(file, e))
}

fn enable(parent: &File) -> Result<(), Refusal> {
    for word in [b"pids".as_slice(), b"memory".as_slice()] {
        if !controller(parent, c"cgroup.controllers", word)? {
            return Err(Refusal::new(c"cgroup.controllers", Cause::ControllerUnavailable));
        }
    }
    if !controller(parent, c"cgroup.subtree_control", b"pids")?
        || !controller(parent, c"cgroup.subtree_control", b"memory")?
    {
        write_control(parent, c"cgroup.subtree_control", b"+pids +memory")?;
    }
    for word in [b"pids".as_slice(), b"memory".as_slice()] {
        if !controller(parent, c"cgroup.subtree_control", word)? {
            return Err(Refusal::new(
                c"cgroup.subtree_control",
                Cause::ControllerUnavailable,
            ));
        }
    }
    Ok(())
}

fn write_control(parent: &File, name: &'static CStr, bytes: &[u8]) -> Result<(), Refusal> {
    let file = open_at(parent, name, libc::O_WRONLY)?;
    // SAFETY: write reads this valid slice; each cgroup command is one whole write.
    let written = unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        return Err(Refusal::io(name, io::Error::last_os_error()));
    }
    if usize::try_from(written).ok() != Some(bytes.len()) {
        return Err(Refusal::new(name, Cause::Io { errno: libc::EIO }));
    }
    Ok(())
}

// u64's longest decimal representation plus its kernel newline and one byte to
// detect excess input. This is a scalar grammar bound, not a workload allowance.
const NUMBER_BYTES: usize = u64::MAX.ilog10() as usize + 1;

fn parse_limit(bytes: &[u8]) -> Result<Limit, Cause> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Cause::InvalidNumber)?
        .trim_ascii();
    if text == "max" {
        return Ok(Limit::Max);
    }
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Cause::InvalidNumber);
    }
    text.parse::<u64>().map(Limit::Value).map_err(|_| Cause::Overflow)
}

fn read_limit(parent: &File, name: &'static CStr) -> Result<Limit, Refusal> {
    let mut file = open_at(parent, name, libc::O_RDONLY)?;
    let mut bytes = [0u8; NUMBER_BYTES + 2];
    let mut used = 0usize;
    loop {
        let rest = bytes
            .get_mut(used..)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Refusal::new(name, Cause::InvalidNumber))?;
        let n = match file.read(rest) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|e| Refusal::io(name, e))?,
        };
        if n == 0 {
            break;
        }
        used = used
            .checked_add(n)
            .ok_or_else(|| Refusal::new(name, Cause::Overflow))?;
    }
    let text = bytes
        .get(..used)
        .ok_or_else(|| Refusal::new(name, Cause::InvalidNumber))?;
    parse_limit(text).map_err(|cause| Refusal::new(name, cause))
}

fn decimal(mut value: u64, bytes: &mut [u8; NUMBER_BYTES]) -> &[u8] {
    let mut digits = 0usize;
    for slot in bytes.iter_mut().rev() {
        *slot = b'0' + (value % 10) as u8;
        digits += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    // digits is bounded by this array's iterator; subtraction cannot underflow.
    bytes.get(bytes.len() - digits..).unwrap_or_default()
}

fn verify(parent: &File, name: &'static CStr, expected: Limit) -> Result<(), Refusal> {
    let observed = read_limit(parent, name)?;
    if observed != expected {
        return Err(Refusal {
            expected: Some(expected),
            observed: Some(observed),
            ..Refusal::new(name, Cause::Readback)
        });
    }
    Ok(())
}

fn set(parent: &File, name: &'static CStr, requested: u64, expected: Limit) -> Result<(), Refusal> {
    write_control(parent, name, decimal(requested, &mut [0; NUMBER_BYTES]))?;
    verify(parent, name, expected)
}

fn memory_value(bytes: u64, page: u64) -> Result<Limit, Refusal> {
    if page == 0 {
        return Err(Refusal::new(c"page size", Cause::PageSize));
    }
    let max =
        u64::try_from(libc::c_long::MAX).map_err(|_| Refusal::new(c"page size", Cause::Overflow))? / page;
    let pages = bytes / page;
    if pages >= max {
        return Ok(Limit::Max);
    }
    pages
        .checked_mul(page)
        .map(Limit::Value)
        .ok_or_else(|| Refusal::new(c"memory.max", Cause::Overflow))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Unused,
    Retained,
}

#[derive(Debug)]
struct Removal {
    parent: File,
    name: Vec<u8>,
    disposition: Disposition,
}

impl Removal {
    fn remove(&mut self) -> Rollback {
        if self.disposition == Disposition::Retained {
            return Rollback::NotCreated;
        }
        self.disposition = Disposition::Retained;
        // SAFETY: the name is one validated component under the held parent. rmdir
        // never removes a populated group or recursively removes another object.
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr().cast(),
                libc::AT_REMOVEDIR,
            )
        } == 0
        {
            Rollback::Removed
        } else {
            Rollback::Failed(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
        }
    }
}

impl Drop for Removal {
    fn drop(&mut self) {
        // For later preparation errors outside this module, attempt removal only
        // while no actor owns the group. A failed Drop cleanup is not a success
        // witness; resource-preparation failures below retain its exact errno.
        let _ = self.remove();
    }
}

#[derive(Debug)]
struct Group {
    directory: File,
    removal: Removal,
}

#[derive(Debug)]
enum Acquisition {
    Created(Group),
    Refused(Refusal),
    Retained { refusal: Refusal, removal: Removal },
}

impl Group {
    fn create(parent: &File, name: &str, existing: bool) -> Result<Self, Refusal> {
        match Self::acquire(parent, name, existing) {
            Acquisition::Created(group) => Ok(group),
            Acquisition::Refused(refusal) | Acquisition::Retained { refusal, .. } => Err(refusal),
        }
    }

    /// The lifetime caller retains the SAME failed-cleanup guard, whereas the
    /// compatibility wrapper above preserves its existing Refusal-only result.
    fn acquire(parent: &File, name: &str, existing: bool) -> Acquisition {
        match Self::prepare_removal(parent, name, existing) {
            Ok(removal) => Self::open(removal),
            Err(refusal) => Acquisition::Refused(refusal),
        }
    }

    fn open(mut removal: Removal) -> Acquisition {
        // SAFETY: resolve the validated component under the SAME held parent.
        let fd = unsafe {
            libc::openat(
                removal.parent.as_raw_fd(),
                removal.name.as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            let mut refusal = Refusal::io(c"open cgroup", io::Error::last_os_error());
            refusal.rollback = removal.remove();
            return match refusal.rollback {
                Rollback::Failed(_) => Acquisition::Retained { refusal, removal },
                _ => Acquisition::Refused(refusal),
            };
        }
        // SAFETY: successful openat transferred this newly owned descriptor.
        let directory = unsafe { File::from_raw_fd(fd) };
        Acquisition::Created(Self { directory, removal })
    }

    fn prepare_removal(parent: &File, name: &str, existing: bool) -> Result<Removal, Refusal> {
        if name.is_empty() || matches!(name, "." | "..") || name.bytes().any(|b| b == 0 || b == b'/') {
            return Err(Refusal::new(c"cgroup name", Cause::InvalidName));
        }
        let length = name
            .len()
            .checked_add(1)
            .ok_or_else(|| Refusal::new(c"cgroup name", Cause::Overflow))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| Refusal::new(c"cgroup name", Cause::Allocation))?;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        // Keep the fallibly reserved bytes directly; CString's boxed conversion
        // need not be part of this allocation-failure path.
        let name = bytes;
        let parent = parent.try_clone().map_err(|e| Refusal::io(c"parent cgroup", e))?;
        // SAFETY: name is a validated single component under the held parent.
        // std::fs::create_dir's existing mode, still subject to the guest's umask.
        let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr().cast(), 0o777) };
        let disposition = if made == 0 {
            Disposition::Unused
        } else {
            let error = io::Error::last_os_error();
            if !existing || error.kind() != io::ErrorKind::AlreadyExists {
                return Err(Refusal::io(c"mkdir cgroup", error));
            }
            Disposition::Retained
        };
        Ok(Removal {
            parent,
            name,
            disposition,
        })
    }

    fn refused(mut self, mut refusal: Refusal) -> Refusal {
        refusal.rollback = self.removal.remove();
        refusal
    }

    fn retain(self) -> File {
        let Self {
            directory,
            mut removal,
        } = self;
        removal.disposition = Disposition::Retained;
        directory
    }
}

/// The existing run and domains hierarchy, held independently of mutable paths.
#[derive(Debug)]
pub(super) struct Parent {
    _run: File,
    group: Group,
    page: u64,
}

impl Parent {
    pub(super) fn prepare(memory_high: u64) -> Result<Self, Refusal> {
        let run = run_group()?;
        enable(&run)?;
        // sysconf reports this kernel's actual page size on either guest arch.
        // SAFETY: sysconf(_SC_PAGESIZE) has no pointer arguments.
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| Refusal::new(c"page size", Cause::PageSize))?;
        let group = Group::create(&run, "domains", true)?;
        let configured = (|| {
            enable(&group.directory)?;
            set(
                &group.directory,
                c"memory.high",
                memory_high,
                memory_value(memory_high, page)?,
            )?;
            set(&group.directory, c"memory.swap.max", 0, Limit::Value(0))
        })();
        if let Err(refusal) = configured {
            return Err(group.refused(refusal));
        }
        Ok(Self {
            _run: run,
            group,
            page,
        })
    }

    pub(super) fn prepare_child(&self, name: &str) -> Result<Prepared, Refusal> {
        Ok(Prepared {
            group: Group::create(&self.group.directory, name, false)?,
            page: self.page,
        })
    }
}

/// A new unused cgroup. Only successful verification produces Committed.
#[derive(Debug)]
pub(super) struct Prepared {
    group: Group,
    page: u64,
}

impl Prepared {
    pub(super) fn commit(self, pids: Option<u64>, memory: Option<u64>) -> Result<Committed, Refusal> {
        let configure = || {
            let group = &self.group.directory;
            match pids {
                Some(n) => set(group, c"pids.max", n, Limit::Value(n))?,
                None => verify(group, c"pids.max", Limit::Max)?,
            }
            set(group, c"memory.oom.group", 1, Limit::Value(1))?;
            match memory {
                Some(n) => set(group, c"memory.max", n, memory_value(n, self.page)?)?,
                None => verify(group, c"memory.max", Limit::Max)?,
            }
            // Reserve the existing lifecycle controls before the first release.
            // No write to cgroup.kill is performed during admission.
            let kill = open_at(group, c"cgroup.kill", libc::O_WRONLY)?;
            let events = open_at(group, c"cgroup.events", libc::O_RDONLY)?;
            Ok((kill, events))
        };
        match configure() {
            Ok((kill, events)) => Ok(Committed {
                group: self.group,
                controls: crate::managed::Controls::held(kill, events),
            }),
            Err(refusal) => Err(self.group.refused(refusal)),
        }
    }
}

#[derive(Debug)]
pub(super) struct Committed {
    group: Group,
    controls: crate::managed::Controls,
}

impl Committed {
    pub(super) fn stage(self) -> (Staged, crate::managed::Controls) {
        (Staged { group: self.group }, self.controls)
    }
}

/// The cgroup rollback guard remains armed while its controls move to Launch.
#[derive(Debug)]
pub(super) struct Staged {
    group: Group,
}

impl Staged {
    pub(super) fn group(&self) -> &File {
        &self.group.directory
    }

    /// Called only after successful clone3: lifecycle ownership takes this object.
    pub(super) fn into_file(self) -> File {
        self.group.retain()
    }
}

/// D59's existing workload remainder, without treating a missing file as zero.
/// These separate reads are observations, not atomic resource reservations.
pub(super) fn workload_headroom() -> Result<u64, Refusal> {
    let run = run_group()?;
    let workload = open_at(&run, c"workload", libc::O_RDONLY | libc::O_DIRECTORY)?;
    let max = read_limit(&workload, c"memory.max")?;
    let current = match read_limit(&workload, c"memory.current")? {
        Limit::Value(n) => n,
        Limit::Max => return Err(Refusal::new(c"memory.current", Cause::InvalidNumber)),
    };
    Ok(match max {
        Limit::Max => 0,
        Limit::Value(n) => n.saturating_sub(current),
    })
}

/// Existing D59 pressure policy only. Overflow is unknown, not a larger budget.
pub(super) fn pressure_headroom(available: u64, pluggable: u64, workload: u64) -> Result<u64, Refusal> {
    available
        .checked_add(pluggable)
        .map(|n| n.saturating_sub(workload))
        .ok_or_else(|| Refusal::new(c"domains memory.high", Cause::Overflow))
}

