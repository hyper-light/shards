//! A child made as shards makes a confined process on Windows (D141, PM M201): in an
//! AppContainer of a profile of its own, holding the capabilities given; in a Job of one
//! process, killed with its last handle; with no child processes and Windows' mitigations
//! (the second word left off where refused); made suspended. Shared by the probes M203 and
//! M204.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile};
use windows_sys::Win32::Security::{FreeSid, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute,
};

/// D141's: no child processes (PROCESS_CREATION_CHILD_PROCESS_RESTRICTED); strict handle
/// checks, no Win32k, no extension points, no remote or low-label images; no FSCTLs in the
/// second word, which windows-2022 refuses (M201).
pub const CHILD_PROCESS_RESTRICTED: u32 = 0x01;
pub const MITIGATIONS: [u64; 2] = [(1 << 24) | (1 << 28) | (1 << 32) | (1 << 52) | (1 << 56), 1 << 56];

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

pub fn job() -> Result<HANDLE, String> {
    // SAFETY: an anonymous job.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(format!("CreateJobObjectW: {}", last()));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    // SAFETY: limits of their own size.
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(format!("SetInformationJobObject: {}", last()));
    }
    Ok(job)
}

/// A suspended child of `exe` in the AppContainer `sid` holding `caps`, in `job`, with
/// D141's policy and mitigations (the second word left off where refused), inheriting
/// standard output and `inherit` alone.
pub fn spawn(
    exe: &Path,
    args: &str,
    sid: PSID,
    caps: &[Vec<u8>],
    job: HANDLE,
    inherit: &[HANDLE],
) -> Result<PROCESS_INFORMATION, String> {
    spawn_with_output(exe, args, sid, caps, job, inherit, std::io::stdout().as_raw_handle() as HANDLE)
}

/// [`spawn`], its standard output and error `out`.
pub fn spawn_with_output(
    exe: &Path,
    args: &str,
    sid: PSID,
    caps: &[Vec<u8>],
    job: HANDLE,
    inherit: &[HANDLE],
    out: HANDLE,
) -> Result<PROCESS_INFORMATION, String> {
    let mut handles = vec![out];
    handles.extend(inherit.iter().copied().filter(|h| !h.is_null()));
    for &h in &handles {
        // SAFETY: our handles, inheritable for the list below alone.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    let mut held: Vec<SID_AND_ATTRIBUTES> = caps
        .iter()
        .map(|b| SID_AND_ATTRIBUTES {
            Sid: b.as_ptr().cast_mut().cast(),
            Attributes: SE_GROUP_ENABLED as u32,
        })
        .collect();
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: held.as_mut_ptr(),
        CapabilityCount: held.len() as u32,
        Reserved: 0,
    };
    let jobs = [job];
    let policy = CHILD_PROCESS_RESTRICTED;
    for words in [2usize, 1] {
        let mut cmd = wide(&format!("\"{}\" {args}", exe.display()));
        let mut size = 0usize;
        // SAFETY: the size query.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 5, 0, &raw mut size) };
        let mut list = vec![0usize; size.div_ceil(8)];
        let attrs = list.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: a buffer of the size asked.
        if unsafe { InitializeProcThreadAttributeList(attrs, 5, 0, &raw mut size) } == 0 {
            return Err(format!("InitializeProcThreadAttributeList: {}", last()));
        }
        let update = |attr: u32, value: *const c_void, size: usize| {
            // SAFETY: values that outlive the list.
            unsafe { UpdateProcThreadAttribute(attrs, 0, attr as usize, value, size, std::ptr::null_mut(), std::ptr::null()) }
        };
        update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_ptr().cast(), handles.len() * 8);
        update(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs.as_ptr().cast(), 8);
        update(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, (&raw const policy).cast(), 4);
        update(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, MITIGATIONS.as_ptr().cast(), words * 8);
        update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            (&raw const capabilities).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        );
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdOutput = out;
        si.StartupInfo.hStdError = out;
        si.lpAttributeList = attrs;
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: a command line we own; the startup info and its attributes alive.
        let made = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                (&raw const si).cast(),
                &raw mut pi,
            )
        };
        let error = std::io::Error::last_os_error();
        // SAFETY: the list initialized above.
        unsafe { DeleteProcThreadAttributeList(attrs) };
        if made != 0 {
            return Ok(pi);
        }
        if words == 1 || error.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32) {
            return Err(format!("CreateProcessW: {error}"));
        }
    }
    Err("CreateProcessW: no attempt left".into())
}

/// An AppContainer profile, deleted as it drops.
pub struct Container {
    pub name: Vec<u16>,
    pub sid: PSID,
}

impl Container {
    /// A profile `shards.<probe>.<tag>.<pid>`, made again if one was left.
    pub fn new(probe: &str, tag: &str) -> Result<Container, String> {
        let name = wide(&format!("shards.{probe}.{tag}.{}", std::process::id()));
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: a name we own; the SID freed and the profile deleted as this drops.
        let hr = unsafe {
            DeleteAppContainerProfile(name.as_ptr());
            CreateAppContainerProfile(name.as_ptr(), name.as_ptr(), name.as_ptr(), std::ptr::null(), 0, &raw mut sid)
        };
        if hr < 0 {
            return Err(format!("CreateAppContainerProfile: {:#010x}", hr as u32));
        }
        Ok(Container { name, sid })
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        // SAFETY: what `new` made.
        unsafe {
            DeleteAppContainerProfile(self.name.as_ptr());
            FreeSid(self.sid);
        }
    }
}
