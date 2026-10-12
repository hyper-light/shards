//! shards' children on Windows (D141): each given exactly the handles named for it, and
//! run in a Job that ends it with its spawner and lets it start nothing; a VM process, in an
//! AppContainer of its own besides (PM M200-M201).
//!
//! - **Handles.** None is made inheritable for it, where another child started meanwhile
//!   could take it (std's own spawns inherit every inheritable handle): the child is made
//!   suspended, each handle duplicated into it (DuplicateHandle), and their values told it
//!   through a pipe, the one handle it inherits, besides its standard output and error;
//!   then it runs. A child that reads none takes none.
//! - **Its Job** kills it as the Job's last handle closes, which its [`Child`] holds: a
//!   spawner that ends, even killed, takes it along (0.7-1.1 ms later, M201); and lets it
//!   run no process of its own.
//! - **Its AppContainer** reaches no file but those handed it, and no socket but in a
//!   directory whose DACL grants its SID ([`AppContainer::grant_dir`]); a profile of its
//!   own, made for it and deleted after it (4.4-5.7 ms and 1.8-2.1 ms, M201), as a
//!   container's processes may reach each other's. A profile is recorded while it lives, so
//!   that one whose maker went without deleting it is deleted by the next ([`sweep`]). It
//!   holds the capabilities VidExo's DACL grants, read from the device as it is made
//!   ([`hypervisor_capabilities`]), without which it runs no partition (M200).
//! - **Messages** ([`send`], [`recv`]) carry no handles: a handle goes in a payload, its
//!   value in the receiver's process, where its sender put it ([`Child::give`]).
//! - **Windows' mitigations**: strict handle checks, no Win32k, no extension points, no
//!   images from remote devices or of a low label, no child processes, and no FSCTLs where
//!   the Windows takes it (windows-2022 does not, M201). Not dynamic code: windows-2022's
//!   WHP sets no partition up without it; a VM process forbids itself dynamic code once its
//!   partition is set up (M202).

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, GENERIC_ALL,
    HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, FreeSid, GetAce, GetLengthSid, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WriteFile,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_DUP_HANDLE, PROCESS_INFORMATION,
    PROCESS_SYNCHRONIZE, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY's PROCESS_CREATION_CHILD_PROCESS_RESTRICTED
/// (UpdateProcThreadAttribute's documentation).
const CHILD_PROCESS_RESTRICTED: u32 = 0x01;

/// The mitigations a child is made with, as UpdateProcThreadAttribute's documentation gives
/// their values (PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY): strict handle checks (1 << 24),
/// no Win32k system calls (1 << 28), no extension points (1 << 32), no images from remote
/// devices (1 << 52) or of a low label (1 << 56); and in the second word, no FSCTL system
/// calls (1 << 56). Not dynamic code (1 << 36): see the module's documentation.
const MITIGATIONS: [u64; 2] = [(1 << 24) | (1 << 28) | (1 << 32) | (1 << 52) | (1 << 56), 1 << 56];

/// Whether this Windows refuses the second mitigation word (windows-2022, M201): found by
/// the first child made, which then goes without it.
static NO_SECOND_WORD: AtomicBool = AtomicBool::new(false);

fn last() -> io::Error {
    io::Error::last_os_error()
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// A child's command line: each argument quoted as CommandLineToArgvW reads it back
/// (backslashes doubled before a quote, quotes escaped), as std quotes them.
fn command_line(program: &Path, args: &[&OsStr]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::new();
    for (i, arg) in std::iter::once(program.as_os_str())
        .chain(args.iter().copied())
        .enumerate()
    {
        if i > 0 {
            line.push(u16::from(b' '));
        }
        let units: Vec<u16> = arg.encode_wide().collect();
        let plain = !units.is_empty()
            && !units
                .iter()
                .any(|&u| u == u16::from(b' ') || u == u16::from(b'\t') || u == u16::from(b'"'));
        if plain {
            line.extend(units);
            continue;
        }
        line.push(u16::from(b'"'));
        let mut backslashes = 0usize;
        for u in units {
            if u == u16::from(b'\\') {
                backslashes += 1;
                continue;
            }
            if u == u16::from(b'"') {
                line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            } else {
                line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            }
            backslashes = 0;
            line.push(u);
        }
        line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
        line.push(u16::from(b'"'));
    }
    line.push(0);
    line
}

/// A Job that ends its processes as its last handle closes, and lets them start none.
#[derive(Debug)]
struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Job> {
        // SAFETY: an anonymous job, owned below.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(last());
        }
        // SAFETY: a handle just made, which nothing else owns.
        let job = Job(unsafe { OwnedHandle::from_raw_handle(raw) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        // SAFETY: limits of their own size, on our job.
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(last());
        }
        Ok(job)
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}

/// A child made by [`spawn`]: ended as this drops, its Job's last handle going with it.
#[derive(Debug)]
pub struct Child {
    process: OwnedHandle,
    pid: u32,
    job: Job,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Its exit code, once it has ended.
    pub fn wait(&self) -> io::Result<i32> {
        // SAFETY: our own process handle.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(last());
        }
        self.code()
    }

    /// Its exit code, if it has ended.
    pub fn try_wait(&self) -> Option<i32> {
        // SAFETY: our own process handle, not waited on.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => self.code().ok(),
            _ => None,
        }
    }

    fn code(&self) -> io::Result<i32> {
        let mut code = 0u32;
        // SAFETY: our own process handle.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
            return Err(last());
        }
        Ok(code as i32)
    }

    /// Ends it, and anything of its Job.
    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: our own job and process.
        unsafe {
            if TerminateJobObject(self.job.raw(), 1) == 0
                && TerminateProcess(self.process.as_raw_handle(), 1) == 0
            {
                return Err(last());
            }
        }
        Ok(())
    }

    /// Whether it ends within `ms` milliseconds.
    pub fn ended_within(&self, ms: u32) -> bool {
        // SAFETY: our own process handle.
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) != WAIT_TIMEOUT }
    }

    /// `handle` duplicated into the child, of the same access: its value there, which only
    /// the child may use.
    pub fn give(&self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
        give(&self.process, handle)
    }

    /// A handle to the child that may only wait for its end (PROCESS_SYNCHRONIZE): for
    /// another child to watch it by, as a VM's network process watches its VM (D142).
    pub fn waitable(&self) -> io::Result<OwnedHandle> {
        self.with_access(PROCESS_SYNCHRONIZE)
    }

    /// What may give the child handles, as [`Child::give`] does, and no more: for a thread
    /// that answers it while this value is held elsewhere (a builder's grants, D143).
    pub fn giver(&self) -> io::Result<Process> {
        Ok(Process {
            handle: self.with_access(PROCESS_DUP_HANDLE)?,
            pid: self.pid,
        })
    }

    /// Another handle to the child, of `access` alone.
    fn with_access(&self, access: u32) -> io::Result<OwnedHandle> {
        let mut h: HANDLE = std::ptr::null_mut();
        // SAFETY: our own process handle, duplicated within this process, of less access.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.process.as_raw_handle(),
                GetCurrentProcess(),
                &raw mut h,
                access,
                0,
                0,
            )
        } == 0
        {
            return Err(last());
        }
        // SAFETY: the handle just made, owned from here on.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }
}

/// A child's process, to give handles to and nothing more ([`Child::giver`]).
#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    pid: u32,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// As [`Child::give`].
    pub fn give(&self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
        give(&self.handle, handle)
    }
}

/// `handle` duplicated into `process`, of the same access: its value there.
fn give(process: &OwnedHandle, handle: BorrowedHandle<'_>) -> io::Result<usize> {
    let mut theirs: HANDLE = std::ptr::null_mut();
    // SAFETY: a handle of ours, duplicated into a process we made, of the same access.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.as_raw_handle(),
            process.as_raw_handle(),
            &raw mut theirs,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last());
    }
    Ok(theirs as usize)
}

/// An AppContainer of a child's own: a profile made for it, deleted as this drops, and
/// its record with it.
#[derive(Debug)]
pub struct AppContainer {
    name: Vec<u16>,
    sid: Sid,
    /// Dropped after the profile is deleted (fields drop in order, after `drop`).
    _record: Record,
}

/// A profile's record ([`AppContainer::new`]): held open while the profile lives, removed
/// once it is deleted.
#[derive(Debug)]
struct Record {
    file: Option<File>,
    path: std::path::PathBuf,
}

impl Drop for Record {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Deletes the profiles recorded in `records` whose makers went without deleting them:
/// each record no process holds, which opens here (its maker's open shares it with none).
/// Two sweeping at once cannot both open one.
pub fn sweep(records: &Path) {
    let Ok(entries) = std::fs::read_dir(records) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("shards.") {
            continue;
        }
        let path = entry.path();
        // Held by a live maker: a sharing violation, and left.
        let Ok(held) = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path) else {
            continue;
        };
        let profile = wide(&name);
        // SAFETY: a NUL-terminated name. A profile already gone is no error of ours.
        unsafe { DeleteAppContainerProfile(profile.as_ptr()) };
        drop(held);
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether this process runs in an AppContainer (TokenIsAppContainer): a VM process
/// confined as its spawner confines it, or not.
pub fn in_app_container() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's own token, for querying.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return false;
    }
    // SAFETY: a handle just opened, which nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let (mut is, mut len) = (0u32, 0u32);
    // SAFETY: a DWORD of its own size for the call to fill.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIsAppContainer,
            (&raw mut is).cast(),
            size_of::<u32>() as u32,
            &raw mut len,
        )
    };
    ok != 0 && is != 0
}

/// A message as [`send`] and [`recv`] carry it on Windows.
#[derive(Debug)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Sends a message as [`recv`] reads it: its kind, its payload's length (a big-endian u32)
/// and its payload, framed as Unix's messages are.
pub fn send(mut to: impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > crate::MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message payload too large",
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("payload length"))?;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend(len.to_be_bytes());
    frame.extend(payload);
    to.write_all(&frame)
}

/// A message coming in parts on a socket that does not wait: what [`Incoming::take`] holds of
/// it between calls.
#[derive(Debug, Default)]
pub struct Incoming {
    header: [u8; 5],
    /// Bytes of the header that have come, then of the payload.
    have: usize,
    /// Once the header has come, the payload, as long as it says.
    payload: Option<Vec<u8>>,
}

/// What [`Incoming::take`] found.
#[derive(Debug)]
pub enum Took {
    Message(Message),
    /// Some of the next message, or none of it, has come.
    Partial,
    /// The peer's end, with nothing of a message before it.
    Ended,
}

impl Incoming {
    /// The next message whole, from what `from` has now: it waits for nothing.
    pub fn take(&mut self, mut from: impl Read) -> io::Result<Took> {
        loop {
            let (buf, whole) = match &mut self.payload {
                None => (self.header.get_mut(self.have..).unwrap_or_default(), 5),
                Some(payload) => {
                    let len = payload.len();
                    (payload.get_mut(self.have..).unwrap_or_default(), len)
                }
            };
            if self.have < whole {
                match from.read(buf) {
                    Ok(0) if self.have == 0 && self.payload.is_none() => return Ok(Took::Ended),
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => self.have += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Took::Partial),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
                continue;
            }
            if self.payload.is_none() {
                let [_, l0, l1, l2, l3] = self.header;
                let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
                if len > crate::MAX_PAYLOAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "message payload too large",
                    ));
                }
                self.payload = Some(vec![0u8; len]);
                self.have = 0;
                continue;
            }
            let [kind, ..] = self.header;
            let payload = self.payload.take().unwrap_or_default();
            self.have = 0;
            return Ok(Took::Message(Message { kind, payload }));
        }
    }
}

/// The next message, or `None` where the stream ends before one starts.
pub fn recv(mut from: impl Read) -> io::Result<Option<Message>> {
    let mut header = [0u8; 5];
    let mut got = 0;
    while got < header.len() {
        let n = from.read(header.get_mut(got..).unwrap_or_default())?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a message cut short",
            ));
        }
        got += n;
    }
    let [kind, l0, l1, l2, l3] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > crate::MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    from.read_exact(&mut payload)?;
    Ok(Some(Message { kind, payload }))
}

/// A SID the system allocated, freed as it drops.
#[derive(Debug)]
struct Sid(PSID);

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: a SID CreateAppContainerProfile allocated, freed once.
        unsafe { FreeSid(self.0) };
    }
}

// SAFETY: a SID is plain memory, read and freed once.
unsafe impl Send for Sid {}
// SAFETY: as above.
unsafe impl Sync for Sid {}

/// Makes AppContainer names unique within a process; the process's own ID makes them so
/// between processes.
static NEXT_CONTAINER: AtomicU64 = AtomicU64::new(0);

impl AppContainer {
    /// A container of a profile named `shards.<role>.<pid>.<n>`, which no other has,
    /// recorded in `records` as it lives: a file of its name, which this value holds open
    /// and no other process may open meanwhile ([`sweep`]). Profiles whose makers went
    /// without deleting them are swept first.
    pub fn new(role: &str, records: &Path) -> io::Result<AppContainer> {
        sweep(records);
        let n = NEXT_CONTAINER.fetch_add(1, Ordering::Relaxed);
        let text = format!("shards.{role}.{}.{n}", std::process::id());
        let record_path = records.join(&text);
        let record = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&record_path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", record_path.display())))?;
        let record = Record {
            file: Some(record),
            path: record_path,
        };
        let name = wide(OsStr::new(&text));
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: names we own; the SID is owned below.
        let make = |sid: &mut PSID| unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                std::ptr::null(),
                0,
                sid,
            )
        };
        let mut hr = make(&mut sid);
        // One left by a process of this ID gone before, which the sweep had not reached: its
        // container is no one's, and goes.
        if hr == hresult(ERROR_ALREADY_EXISTS) {
            // SAFETY: a name we own.
            unsafe { DeleteAppContainerProfile(name.as_ptr()) };
            hr = make(&mut sid);
        }
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        Ok(AppContainer {
            name,
            sid: Sid(sid),
            _record: record,
        })
    }

    /// Lets the container reach `dir` and what is made in it: its sockets above all.
    pub fn grant_dir(&self, dir: &Path) -> io::Result<()> {
        let name = wide(dir.as_os_str());
        let mut old: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name; the descriptor freed below, the DACL within it.
        let r = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut old,
                std::ptr::null_mut(),
                &raw mut sd,
            )
        };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: self.sid.0.cast(),
            },
        };
        let mut new: *mut ACL = std::ptr::null_mut();
        // SAFETY: one entry and the old DACL; the new one freed below.
        let r = unsafe { SetEntriesInAclW(1, &raw const entry, old, &raw mut new) };
        let set = if r == 0 {
            // SAFETY: the new DACL, set on the directory.
            unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new,
                    std::ptr::null(),
                )
            }
        } else {
            r
        };
        // SAFETY: what the calls allocated.
        unsafe {
            LocalFree(new.cast());
            LocalFree(sd.cast());
        }
        if set != 0 {
            return Err(io::Error::from_raw_os_error(set as i32));
        }
        Ok(())
    }
}

impl Drop for AppContainer {
    fn drop(&mut self) {
        // SAFETY: the profile this container made; its processes have ended or end with it.
        unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
    }
}

/// HRESULT_FROM_WIN32.
fn hresult(code: u32) -> i32 {
    ((code & 0xFFFF) | 0x8007_0000) as i32
}

/// The capability SIDs `\\?\VidExo`'s DACL allows to read and write it, as bytes: what an
/// AppContainer holds to run a WHP partition (M200). Read from the device, once a process,
/// so that a Windows that grants another capability is followed.
pub fn hypervisor_capabilities() -> io::Result<&'static [Vec<u8>]> {
    static CAPS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    if let Some(caps) = CAPS.get() {
        return Ok(caps);
    }
    let found = read_capabilities()?;
    Ok(CAPS.get_or_init(|| found))
}

fn read_capabilities() -> io::Result<Vec<Vec<u8>>> {
    let name = wide(OsStr::new(r"\\?\VidExo"));
    // SAFETY: a NUL-terminated name; READ_CONTROL alone.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(format!(
            "the hypervisor's device, \\\\?\\VidExo: {} (is the Windows Hypervisor Platform on?)",
            last()
        )));
    }
    // SAFETY: a handle just opened, which nothing else owns.
    let device = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: our handle; the descriptor freed below, the DACL within it.
    let r = unsafe {
        GetSecurityInfo(
            device.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if r != 0 || dacl.is_null() {
        return Err(io::Error::other(format!(
            "the hypervisor's device's DACL: error {r}"
        )));
    }
    // FILE_GENERIC_READ and FILE_GENERIC_WRITE, which the device grants a capability that
    // runs partitions (0x1201bf, M200).
    const READ_WRITE: u32 = 0x0012_0089 | 0x0012_0116;
    let mut found = Vec::new();
    // SAFETY: the DACL GetSecurityInfo returned, its ACEs read in place, then freed.
    unsafe {
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &raw mut ace) == 0 {
                continue;
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            if (*allowed).Header.AceType != 0 || (*allowed).Mask & READ_WRITE != READ_WRITE {
                continue;
            }
            let sid: PSID = (&raw mut (*allowed).SidStart).cast();
            let bytes = std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize);
            // A capability SID: S-1-15-3-…, authority 15, first sub-authority 3.
            if bytes.get(7) == Some(&15) && bytes.get(8..12) == Some(&3u32.to_le_bytes()[..]) {
                found.push(bytes.to_vec());
            }
        }
        LocalFree(sd.cast());
    }
    if found.is_empty() {
        return Err(io::Error::other(
            "the hypervisor's device grants no capability: a confined VM process could run no partition",
        ));
    }
    Ok(found)
}

/// A handle given a child, and the name it is told by.
#[derive(Debug, Clone, Copy)]
pub struct Given<'a> {
    pub name: &'a str,
    pub handle: BorrowedHandle<'a>,
}

/// A socket shared with a child (WSADuplicateSocketW, as Winsock documents a socket's
/// sharing, PM M203), and the name it is told by: its raw SOCKET, which the caller holds
/// open until the child has made its own.
#[derive(Debug, Clone, Copy)]
pub struct Shared<'a> {
    pub name: &'a str,
    pub socket: u64,
}

/// What a child's AppContainer holds besides itself: the capabilities VidExo's DACL grants
/// (a VM process, M200), or the network's (a network process: internetClient,
/// internetClientServer and privateNetworkClientServer, PM M204).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holds {
    Nothing,
    Hypervisor,
    Network,
}

/// The network's capability SIDs, S-1-15-3-1 to -3 (winnt.h's
/// SECURITY_CAPABILITY_INTERNET_CLIENT, _CLIENT_SERVER, PRIVATE_NETWORK_CLIENT_SERVER), as
/// bytes.
fn network_capabilities() -> &'static [Vec<u8>] {
    static CAPS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    CAPS.get_or_init(|| {
        (1u32..=3)
            .map(|rid| {
                let mut sid = vec![1u8, 2, 0, 0, 0, 0, 0, 15];
                sid.extend(3u32.to_le_bytes());
                sid.extend(rid.to_le_bytes());
                sid
            })
            .collect()
    })
}

/// What a child was handed, by name ([`given`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handed {
    /// A handle's value in the child.
    Handle(usize),
    /// A shared socket's protocol info, which the child makes its own socket of.
    Socket(Vec<u8>),
}

/// What [`spawn`] starts.
#[derive(Debug)]
pub struct Spawn<'a> {
    pub program: &'a Path,
    pub args: &'a [&'a OsStr],
    /// Where `--grants HANDLE`, the pipe its handles are told on, goes among `args`: before
    /// the one of this index, after a command (a VM process's `run`) and before any `--`
    /// and what follows that.
    pub grants_at: usize,
    pub given: &'a [Given<'a>],
    /// Its standard input, if any, output and error: each inherited, where this process has
    /// one.
    pub stdin: Option<BorrowedHandle<'a>>,
    pub stdout: BorrowedHandle<'a>,
    pub stderr: BorrowedHandle<'a>,
    /// Sockets shared with it.
    pub sockets: &'a [Shared<'a>],
    /// The variables it is given ([`environment`]): of this process's environment, it has
    /// these and no more, as the processes a guest could take over are given on Unix.
    pub env: &'a [(&'a str, &'a OsStr)],
    /// Its container, which holds what `holds` says.
    pub container: Option<&'a AppContainer>,
    pub holds: Holds,
}

/// A child [`spawn_suspended`] made, handed all it is given, which runs once resumed.
#[derive(Debug)]
pub struct Suspended {
    child: Child,
    thread: OwnedHandle,
}

impl Suspended {
    pub fn id(&self) -> u32 {
        self.child.pid
    }

    /// `handle` duplicated into it, as [`Child::give`].
    pub fn give(&self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
        self.child.give(handle)
    }

    /// As [`Child::waitable`].
    pub fn waitable(&self) -> io::Result<OwnedHandle> {
        self.child.waitable()
    }

    /// As [`Child::giver`].
    pub fn giver(&self) -> io::Result<Process> {
        self.child.giver()
    }

    /// Lets it run.
    pub fn resume(self) -> io::Result<Child> {
        // SAFETY: the child's first thread, suspended until now.
        if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
            let e = last();
            let _ = self.child.kill();
            return Err(e);
        }
        Ok(self.child)
    }
}

/// Starts `s`: suspended, its handles duplicated into it and its sockets shared with it,
/// what it was handed (`NAME=VALUE` for a handle, `NAME=@HEX` for a socket's info, a line
/// each) written to its grants pipe, which is then closed, and resumed.
pub fn spawn(s: &Spawn<'_>) -> io::Result<Child> {
    spawn_suspended(s)?.resume()
}

/// [`spawn`], left suspended: for a spawner that hands another child this one's process
/// before either runs.
pub fn spawn_suspended(s: &Spawn<'_>) -> io::Result<Suspended> {
    let (pipe_read, pipe_write) = pipe()?;
    let pipe_value = pipe_read.as_raw_handle() as usize;
    let pipe_arg = pipe_value.to_string();
    let mut args: Vec<&OsStr> = Vec::with_capacity(s.args.len() + 2);
    let at = s.grants_at.min(s.args.len());
    args.extend(s.args.iter().take(at));
    args.push(OsStr::new("--grants"));
    args.push(OsStr::new(&pipe_arg));
    args.extend(s.args.iter().skip(at));
    let mut line = command_line(s.program, &args);
    let job = Job::new()?;
    let caps: &[Vec<u8>] = match s.holds {
        Holds::Hypervisor => hypervisor_capabilities()?,
        Holds::Network => network_capabilities(),
        Holds::Nothing => &[],
    };
    let made = match create(
        s,
        &mut line,
        &pipe_read,
        &job,
        caps,
        !NO_SECOND_WORD.load(Ordering::Relaxed),
    ) {
        Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            // A Windows that knows no second mitigation word (windows-2022, M201).
            NO_SECOND_WORD.store(true, Ordering::Relaxed);
            create(s, &mut line, &pipe_read, &job, caps, false)
        }
        made => made,
    }?;
    // SAFETY: the handles CreateProcessW returned, owned from here on.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(made.hProcess),
            OwnedHandle::from_raw_handle(made.hThread),
        )
    };
    let child = Child {
        process,
        pid: made.dwProcessId,
        job,
    };
    drop(pipe_read);
    let mut told = String::new();
    for g in s.given {
        let theirs = child.give(g.handle).map_err(|e| {
            let _ = child.kill();
            io::Error::new(e.kind(), format!("giving {}: {e}", g.name))
        })?;
        told.push_str(&format!("{}={theirs}\n", g.name));
    }
    for shared in s.sockets {
        let info = share_socket(shared.socket, child.pid).map_err(|e| {
            let _ = child.kill();
            io::Error::new(e.kind(), format!("sharing {}: {e}", shared.name))
        })?;
        let hex: String = info.iter().map(|b| format!("{b:02x}")).collect();
        told.push_str(&format!("{}=@{hex}\n", shared.name));
    }
    let mut written = 0usize;
    while written < told.len() {
        let rest = told.as_bytes().get(written..).unwrap_or_default();
        let mut n = 0u32;
        // SAFETY: our pipe's write end; bytes we own.
        if unsafe {
            WriteFile(
                pipe_write.as_raw_handle(),
                rest.as_ptr(),
                u32::try_from(rest.len()).unwrap_or(u32::MAX),
                &raw mut n,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let e = last();
            let _ = child.kill();
            return Err(e);
        }
        written += n as usize;
    }
    drop(pipe_write);
    Ok(Suspended { child, thread })
}

/// What process `pid` makes its own copy of `socket` from ([`adopt_socket`]):
/// WSADuplicateSocketW's protocol info, its bytes whole (PM M203). Of a socket of any
/// family: a TCP connection made for a process its AppContainer keeps from the host's own
/// addresses (D142) too.
pub fn share_socket(socket: u64, pid: u32) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Networking::WinSock::{WSADuplicateSocketW, WSAGetLastError, WSAPROTOCOL_INFOW};
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: a socket the caller holds; a structure for the call to fill.
    if unsafe { WSADuplicateSocketW(socket as usize, pid, &raw mut info) } != 0 {
        // SAFETY: no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    // SAFETY: the structure's own bytes, all of them fields.
    let bytes =
        unsafe { std::slice::from_raw_parts((&raw const info).cast::<u8>(), size_of::<WSAPROTOCOL_INFOW>()) };
    Ok(bytes.to_vec())
}

/// This process's own copy of a socket another shared with it ([`share_socket`]),
/// overlapped, as every socket of shards' is, and kept from its children.
pub fn adopt_socket(info: &[u8]) -> io::Result<std::os::windows::io::OwnedSocket> {
    use std::os::windows::io::FromRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        FROM_PROTOCOL_INFO, INVALID_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSAGetLastError,
        WSAPROTOCOL_INFOW, WSASocketW,
    };
    let size = size_of::<WSAPROTOCOL_INFOW>();
    if info.len() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} bytes of a socket's protocol info, not {size}", info.len()),
        ));
    }
    let mut parsed = WSAPROTOCOL_INFOW::default();
    // SAFETY: the structure's own size, from bytes of that length; any bytes are some value
    // of its fields, which Winsock checks.
    unsafe { std::ptr::copy_nonoverlapping(info.as_ptr(), (&raw mut parsed).cast::<u8>(), size) };
    // SAFETY: the info as WSADuplicateSocketW wrote it for this process.
    let s = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &raw const parsed,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        // SAFETY: no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    // SAFETY: a socket just made, owned from here on.
    Ok(unsafe { std::os::windows::io::OwnedSocket::from_raw_socket(s as u64) })
}

/// A pipe: its read end, inheritable, for a child's list alone; its write end, not.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill, not inheritable.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
        return Err(last());
    }
    // SAFETY: handles just made, which nothing else owns.
    let (read, write) = unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    };
    // SAFETY: our own handle, inheritable for the child's handle list alone.
    if unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(last());
    }
    Ok((read, write))
}

/// CreateProcessW, suspended, with `s`'s attributes; the mitigations' second word where
/// `second`.
fn create(
    s: &Spawn<'_>,
    line: &mut [u16],
    pipe_read: &OwnedHandle,
    job: &Job,
    caps: &[Vec<u8>],
    second: bool,
) -> io::Result<PROCESS_INFORMATION> {
    // Standard input, output and error are inherited, made inheritable as std makes them;
    // one this process lacks (a null or invalid handle) the child lacks too.
    let mut inherited: Vec<RawHandle> = vec![pipe_read.as_raw_handle()];
    let stdio = |h: Option<BorrowedHandle<'_>>| {
        h.map(|h| h.as_raw_handle())
            .filter(|&h| !h.is_null() && h != INVALID_HANDLE_VALUE)
    };
    let (stdin, stdout, stderr) = (stdio(s.stdin), stdio(Some(s.stdout)), stdio(Some(s.stderr)));
    for h in [stdin, stdout, stderr].into_iter().flatten() {
        // SAFETY: a handle the caller lends us, for the child's handle list alone.
        if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(last());
        }
        if !inherited.contains(&h) {
            inherited.push(h);
        }
    }
    let jobs = [job.raw()];
    let child_policy = CHILD_PROCESS_RESTRICTED;
    let words = MITIGATIONS;
    let mut held: Vec<SID_AND_ATTRIBUTES> = caps
        .iter()
        .map(|b| SID_AND_ATTRIBUTES {
            Sid: b.as_ptr().cast_mut().cast(),
            Attributes: SE_GROUP_ENABLED as u32,
        })
        .collect();
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: s.container.map_or(std::ptr::null_mut(), |c| c.sid.0),
        Capabilities: if held.is_empty() {
            std::ptr::null_mut()
        } else {
            held.as_mut_ptr()
        },
        CapabilityCount: u32::try_from(held.len()).unwrap_or(0),
        Reserved: 0,
    };
    let count = 5;
    let mut size = 0usize;
    // SAFETY: the size query, which fails and fills `size`.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
    let mut list = vec![0usize; size.div_ceil(size_of::<usize>())];
    let attrs = list.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: a buffer of at least the size asked, aligned for the list.
    if unsafe { InitializeProcThreadAttributeList(attrs, count, 0, &raw mut size) } == 0 {
        return Err(last());
    }
    let update = |attr: u32, value: *const std::ffi::c_void, size: usize| -> io::Result<()> {
        // SAFETY: a value that outlives the list, which is deleted below.
        if unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                attr as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(last());
        }
        Ok(())
    };
    let made = (|| -> io::Result<PROCESS_INFORMATION> {
        update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherited.as_ptr().cast(),
            inherited.len() * size_of::<HANDLE>(),
        )?;
        update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            size_of::<HANDLE>(),
        )?;
        update(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
            (&raw const child_policy).cast(),
            size_of::<u32>(),
        )?;
        update(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
            words.as_ptr().cast(),
            if second {
                size_of::<[u64; 2]>()
            } else {
                size_of::<u64>()
            },
        )?;
        if s.container.is_some() {
            update(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                (&raw const capabilities).cast(),
                size_of::<SECURITY_CAPABILITIES>(),
            )?;
        }
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin.unwrap_or(std::ptr::null_mut());
        si.StartupInfo.hStdOutput = stdout.unwrap_or(std::ptr::null_mut());
        si.StartupInfo.hStdError = stderr.unwrap_or(std::ptr::null_mut());
        si.lpAttributeList = attrs;
        let mut pi = PROCESS_INFORMATION::default();
        let env = environment(s.env);
        // SAFETY: a command line we own and may be written; startup info and its
        // attributes, and the environment block, alive through the call.
        if unsafe {
            CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                std::ptr::null(),
                (&raw const si).cast(),
                &raw mut pi,
            )
        } == 0
        {
            return Err(last());
        }
        Ok(pi)
    })();
    // SAFETY: the list initialized above, its values alive still.
    unsafe { DeleteProcThreadAttributeList(attrs) };
    made
}

/// What a child is given of this process's environment besides what it is told (PM M208):
/// `LOCALAPPDATA`, without which Windows makes no child in an AppContainer
/// (ERROR_ENVVAR_NOT_FOUND), and which it points at the container's own folder, `TEMP` and
/// `TMP` at the `Temp` in it; and `SystemRoot`, whose `%SystemRoot%` Winsock's providers are
/// found under as a socket is made.
const KEPT: [&str; 2] = ["LOCALAPPDATA", "SystemRoot"];

/// A child's environment block (CreateProcessW's, Unicode): `given`, and this process's
/// [`KEPT`]; each `NAME=VALUE` and its NUL, sorted by name as Windows keeps a block (case
/// folded), and a NUL to end it.
fn environment(given: &[(&str, &OsStr)]) -> Vec<u16> {
    let kept: Vec<(&str, std::ffi::OsString)> = KEPT
        .iter()
        .filter(|k| !given.iter().any(|(n, _)| n.eq_ignore_ascii_case(k)))
        .filter_map(|&k| std::env::var_os(k).map(|v| (k, v)))
        .collect();
    let mut vars: Vec<(&str, &OsStr)> = given.to_vec();
    vars.extend(kept.iter().map(|(k, v)| (*k, v.as_os_str())));
    vars.sort_by_key(|(n, _)| n.to_ascii_uppercase());
    let mut block = Vec::new();
    for (name, value) in vars {
        block.extend(name.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    // An empty block is two NULs.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// What a child's spawner gave it (`--grants HANDLE`): each line of the pipe, read to its
/// end, a handle's value or a shared socket's info.
pub fn given(pipe: usize) -> io::Result<Vec<(String, Handed)>> {
    use std::io::Read as _;
    // SAFETY: the pipe's read end this process inherited, owned from here on.
    let mut file = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(pipe as RawHandle) });
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("a grant {l:?}"));
            let (name, value) = l.split_once('=').ok_or_else(bad)?;
            let handed = match value.strip_prefix('@') {
                Some(hex) => Handed::Socket(
                    (0..hex.len())
                        .step_by(2)
                        .map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
                        .collect::<Option<Vec<u8>>>()
                        .ok_or_else(bad)?,
                ),
                None => Handed::Handle(value.parse::<usize>().map_err(|_| bad())?),
            };
            Ok((name.to_string(), handed))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let mut w = command_line(Path::new(r"C:\bin\shards-vm.exe"), &args);
        w.pop();
        String::from_utf16(&w).unwrap()
    }

    /// A message goes over whole, kind and payload; a stream that ends between messages
    /// ends them, and one that ends within one is an error.
    #[test]
    fn messages_frame_as_unix_messages_do() {
        let mut wire = Vec::new();
        send(&mut wire, 7, b"payload").unwrap();
        send(&mut wire, 8, b"").unwrap();
        let mut from = wire.as_slice();
        let m = recv(&mut from).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.as_slice()), (7, &b"payload"[..]));
        let m = recv(&mut from).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.len()), (8, 0));
        assert!(recv(&mut from).unwrap().is_none());
        assert!(recv(&wire[..3]).is_err());
        let too_long = [1u8, 0xff, 0xff, 0xff, 0xff];
        assert!(recv(&too_long[..]).is_err());
    }

    /// A message read a byte at a time, as a socket that does not wait gives it, is taken
    /// whole once its last byte has come; an end between messages ends them.
    #[test]
    fn a_message_comes_whole_from_its_parts() {
        let mut wire = Vec::new();
        send(&mut wire, 9, b"parts").unwrap();
        send(&mut wire, 3, b"").unwrap();
        struct Trickle<'a>(&'a [u8], usize);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.1 % 2 == 1 {
                    self.1 += 1;
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.1 += 1;
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                if let Some(b) = buf.first_mut() {
                    *b = *first;
                }
                self.0 = rest;
                Ok(1)
            }
        }
        let mut from = Trickle(&wire, 0);
        let mut incoming = Incoming::default();
        let mut got = Vec::new();
        loop {
            match incoming.take(&mut from).unwrap() {
                Took::Message(m) => got.push((m.kind, m.payload)),
                Took::Partial => {}
                Took::Ended => break,
            }
        }
        assert_eq!(got, [(9, b"parts".to_vec()), (3, Vec::new())]);
    }

    /// A record its maker holds is left; one no process holds is swept, and its file goes.
    #[test]
    fn a_sweep_takes_records_no_maker_holds() {
        let dir = shards_testdir::TempDir::new("ipc-sweep").unwrap();
        let held_path = dir.join("shards.test.held");
        let _held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&held_path)
            .unwrap();
        let left = dir.join("shards.test.left");
        std::fs::write(&left, b"").unwrap();
        let other = dir.join("not-a-record");
        std::fs::write(&other, b"").unwrap();
        sweep(&dir);
        assert!(held_path.exists());
        assert!(!left.exists());
        assert!(other.exists());
    }

    /// A test process runs in no AppContainer.
    #[test]
    fn this_process_is_in_no_app_container() {
        assert!(!in_app_container());
    }

    /// Arguments are quoted as CommandLineToArgvW reads them back: plain ones alone; ones
    /// with spaces or quotes quoted, backslashes doubled before a quote, quotes escaped.
    #[test]
    fn a_command_line_quotes_as_windows_reads_it() {
        assert_eq!(
            line(&["run", "--kernel", r"C:\k"]),
            r"C:\bin\shards-vm.exe run --kernel C:\k"
        );
        assert_eq!(line(&[r"C:\a b\k"]), r#"C:\bin\shards-vm.exe "C:\a b\k""#);
        assert_eq!(line(&[r#"say "hi""#]), r#"C:\bin\shards-vm.exe "say \"hi\"""#);
        assert_eq!(
            line(&[r"dir with space\"]),
            r#"C:\bin\shards-vm.exe "dir with space\\""#
        );
        assert_eq!(line(&[""]), r#"C:\bin\shards-vm.exe """#);
    }

    /// A child's environment is what it is given, LOCALAPPDATA and SystemRoot, sorted by
    /// name with case folded, each variable and the block ended by a NUL.
    #[test]
    fn a_child_is_given_its_variables_and_what_windows_needs_alone() {
        let text = |block: &[u16]| String::from_utf16(block).unwrap();
        let ours = |k: &str| std::env::var_os(k).unwrap().to_string_lossy().into_owned();
        let block = environment(&[("SHARDS_LOG", OsStr::new("debug")), ("a_first", OsStr::new("1"))]);
        assert_eq!(
            text(&block),
            format!(
                "a_first=1\0LOCALAPPDATA={}\0SHARDS_LOG=debug\0SystemRoot={}\0\0",
                ours("LOCALAPPDATA"),
                ours("SystemRoot")
            )
        );
        // One given is kept, not doubled.
        let block = environment(&[
            ("SYSTEMROOT", OsStr::new(r"D:\w")),
            ("localappdata", OsStr::new("L")),
        ]);
        assert_eq!(text(&block), "localappdata=L\0SYSTEMROOT=D:\\w\0\0");
    }
}
