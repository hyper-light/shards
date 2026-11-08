//! What NTFS keeps of a Linux guest's files, for a FUSE server over a host directory on
//! Windows (PM M362, D197), before one is built: what each step the server would take
//! answers, as the runner's account and again as a standard user (as M343 runs one: the
//! runner's account is an administrator, and a user of shards most often is not).
//!
//! - **Linux's metadata as WSL keeps it** ("File Permissions for WSL", Microsoft Learn): the
//!   extended attributes `$LXUID`, `$LXGID` and `$LXMOD`, set and read back (NtSetEaFile,
//!   NtQueryEaFile).
//! - **Its files that are not regular files or directories, as WSL makes them**: reparse
//!   points of the LX tags, a symlink's buffer as MS-FSCC 2.1.2.7 has it (version 2, then its
//!   target's bytes), a FIFO's, a character device's, a block device's and a Unix socket's
//!   empty (FSCTL_SET_REPARSE_POINT), each read back (FSCTL_GET_REPARSE_POINT); and an NTFS
//!   symlink beside them (CreateSymbolicLinkW, unprivileged).
//! - **POSIX semantics**: an open file's name removed at once and made again while it is held
//!   (FILE_DISPOSITION_FLAG_POSIX_SEMANTICS), and a rename over a file held open
//!   (FILE_RENAME_FLAG_POSIX_SEMANTICS), the held one still reading what it held.
//! - **A hard link** (CreateHardLinkW), **a directory made case-sensitive**
//!   (FileCaseSensitiveInformation), two names differing in case then made in it, and **a
//!   file's ID** (FileIdInfo), a file's key as an inode is.

use std::io::{Read as _, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CASE_SENSITIVE_INFORMATION, FileCaseSensitiveInformation, NtQueryEaFile, NtSetEaFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, CreateHardLinkW, CreateSymbolicLinkW, DELETE, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FileDispositionInfoEx, FileIdInfo, FileRenameInfoEx, GetFileInformationByHandleEx,
    OPEN_EXISTING, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
use windows_sys::Win32::System::Threading::{
    CreateProcessWithLogonW, GetCurrentProcess, GetExitCodeProcess, LOGON_WITH_PROFILE, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::{FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS};

/// The LX reparse tags (ntifs.h; windows-sys's Wdk and SystemServices): a symlink's, a
/// FIFO's, a character device's, a block device's, and a Unix socket's.
const LX_SYMLINK: u32 = 0xA000_001D;
const LX_FIFO: u32 = 0x8000_0024;
const LX_CHR: u32 = 0x8000_0025;
const LX_BLK: u32 = 0x8000_0026;
const AF_UNIX: u32 = 0x8000_0023;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

/// An NTSTATUS as its hex and the Win32 error it maps to.
fn status(s: i32) -> String {
    if s == 0 {
        "ok".into()
    } else {
        // SAFETY: a plain conversion.
        let dos = unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(s) };
        format!("NTSTATUS {:#010x} ({})", s as u32, std::io::Error::from_raw_os_error(dos as i32))
    }
}

/// A handle to `path`, opened with `access`, sharing everything, with `flags`; `create`
/// makes it anew.
fn open(path: &Path, access: u32, create: bool, flags: u32) -> Result<OwnedHandle, String> {
    let name = wide_path(path);
    // SAFETY: a NUL-terminated name and plain arguments.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            if create { CREATE_ALWAYS } else { OPEN_EXISTING },
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    // SAFETY: a handle just opened, ours alone.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// The EAs `entries`, a FILE_FULL_EA_INFORMATION list: each its header, its name and a NUL,
/// its value, the next aligned to four bytes.
fn ea_list(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (name, value)) in entries.iter().enumerate() {
        let start = out.len();
        out.extend([0u8; 4]);
        out.push(0);
        out.push(name.len() as u8);
        out.extend((value.len() as u16).to_le_bytes());
        out.extend(name.as_bytes());
        out.push(0);
        out.extend(*value);
        if i + 1 < entries.len() {
            while (out.len() - start) % 4 != 0 {
                out.push(0);
            }
            let next = (out.len() - start) as u32;
            out[start..start + 4].copy_from_slice(&next.to_le_bytes());
        }
    }
    out
}

/// The EAs a FILE_FULL_EA_INFORMATION list holds, by name.
fn eas_of(buf: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= buf.len() {
        let next = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap_or_default()) as usize;
        let name_len = buf[at + 5] as usize;
        let value_len = u16::from_le_bytes([buf[at + 6], buf[at + 7]]) as usize;
        let name = String::from_utf8_lossy(buf.get(at + 8..at + 8 + name_len).unwrap_or_default()).into_owned();
        let value = buf
            .get(at + 9 + name_len..at + 9 + name_len + value_len)
            .unwrap_or_default()
            .to_vec();
        out.push((name, value));
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

fn eas(dir: &Path) -> String {
    let path = dir.join("ea");
    let h = match open(&path, GENERIC_READ | GENERIC_WRITE, true, 0) {
        Ok(h) => h,
        Err(e) => return format!("make: {e}"),
    };
    let set = ea_list(&[
        ("$LXUID", &1000u32.to_le_bytes()),
        ("$LXGID", &1000u32.to_le_bytes()),
        ("$LXMOD", &0o100644u32.to_le_bytes()),
    ]);
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a list of our own, its length.
    let s = unsafe { NtSetEaFile(h.as_raw_handle(), &mut io, set.as_ptr().cast(), set.len() as u32) };
    let mut buf = vec![0u8; 4096];
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a buffer of our own, its length; every EA, from the first.
    let q = unsafe {
        NtQueryEaFile(
            h.as_raw_handle(),
            &mut io,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            false,
            std::ptr::null(),
            0,
            std::ptr::null(),
            true,
        )
    };
    let read: Vec<String> = eas_of(&buf[..io.Information.min(buf.len())])
        .iter()
        .map(|(n, v)| format!("{n}={:o}", u32::from_le_bytes(v.as_slice().try_into().unwrap_or_default())))
        .collect();
    format!("set {}; read back {}: {}", status(s), status(q), read.join(" "))
}

/// A reparse point of `tag` with `data` on a new empty file `name`, set and read back.
fn reparse(dir: &Path, name: &str, tag: u32, data: &[u8]) -> String {
    let path = dir.join(name);
    let flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS;
    let h = match open(&path, GENERIC_READ | GENERIC_WRITE, true, flags) {
        Ok(h) => h,
        Err(e) => return format!("make: {e}"),
    };
    let mut buf = Vec::new();
    buf.extend(tag.to_le_bytes());
    buf.extend((data.len() as u16).to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend(data);
    let mut n = 0u32;
    // SAFETY: a buffer of our own, its length, on our handle.
    let set = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr().cast(),
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut n,
            std::ptr::null_mut(),
        )
    };
    if set == 0 {
        return format!("set refused: {}", last());
    }
    drop(h);
    let h = match open(&path, GENERIC_READ, false, flags) {
        Ok(h) => h,
        Err(e) => return format!("set; reopen: {e}"),
    };
    let mut back = vec![0u8; 16 * 1024];
    // SAFETY: a buffer of our own, its length, on our handle.
    let got = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            back.as_mut_ptr().cast(),
            back.len() as u32,
            &mut n,
            std::ptr::null_mut(),
        )
    };
    if got == 0 {
        return format!("set; read back refused: {}", last());
    }
    let same = back.get(..n as usize) == Some(buf.as_slice());
    format!("set; read back {n} bytes, the same: {same}")
}

fn nt_symlink(dir: &Path) -> String {
    let (link, target) = (wide_path(&dir.join("ntlink")), wide("ea"));
    // SAFETY: two NUL-terminated names.
    let made = unsafe { CreateSymbolicLinkW(link.as_ptr(), target.as_ptr(), SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE) };
    if made { "made".into() } else { format!("refused: {}", last()) }
}

fn posix_unlink(dir: &Path) -> String {
    let path = dir.join("open1");
    if let Err(e) = std::fs::write(&path, b"held") {
        return format!("make: {e}");
    }
    let held = match std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(&path) {
        Ok(f) => f,
        Err(e) => return format!("hold: {e}"),
    };
    let d = match open(&path, DELETE, false, 0) {
        Ok(h) => h,
        Err(e) => return format!("open to delete: {e}"),
    };
    let info = FILE_DISPOSITION_INFO_EX { Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS };
    // SAFETY: an info of its own size on our handle.
    let set = unsafe {
        SetFileInformationByHandle(d.as_raw_handle(), FileDispositionInfoEx, (&raw const info).cast(), size_of::<FILE_DISPOSITION_INFO_EX>() as u32)
    };
    if set == 0 {
        return format!("refused: {}", last());
    }
    drop(d);
    let gone = !path.exists();
    let made_again = std::fs::write(&path, b"new").is_ok();
    let mut read = String::new();
    let _ = (&held).read_to_string(&mut read);
    format!("name gone while held: {gone}; made again: {made_again}; the held one reads {read:?}")
}

fn posix_rename(dir: &Path) -> String {
    let (target, source) = (dir.join("t"), dir.join("s"));
    if std::fs::write(&target, b"old").and_then(|()| std::fs::write(&source, b"new")).is_err() {
        return "make: failed".into();
    }
    let held = match std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(&target) {
        Ok(f) => f,
        Err(e) => return format!("hold: {e}"),
    };
    let s = match open(&source, DELETE, false, 0) {
        Ok(h) => h,
        Err(e) => return format!("open to rename: {e}"),
    };
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    // FILE_RENAME_INFO: its flags (a union with ReplaceIfExists), RootDirectory, the name's
    // length in bytes, then the name.
    let mut buf = vec![0u8; size_of::<u32>() * 2 + size_of::<usize>() * 2 + name.len() * 2 + 2];
    let flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
    buf[0..4].copy_from_slice(&flags.to_le_bytes());
    let len_at = size_of::<usize>() * 2;
    buf[len_at..len_at + 4].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
    for (i, c) in name.iter().enumerate() {
        buf[len_at + 4 + i * 2..len_at + 6 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    // SAFETY: a FILE_RENAME_INFO laid out above, its length, on our handle.
    let set = unsafe { SetFileInformationByHandle(s.as_raw_handle(), FileRenameInfoEx, buf.as_ptr().cast(), buf.len() as u32) };
    if set == 0 {
        return format!("refused: {}", last());
    }
    drop(s);
    let now = std::fs::read_to_string(&target).unwrap_or_default();
    let mut read = String::new();
    let _ = (&held).read_to_string(&mut read);
    format!("renamed over the held file; the name reads {now:?}; the held one reads {read:?}")
}

fn hard_link(dir: &Path) -> String {
    let (link, existing) = (wide_path(&dir.join("hl")), wide_path(&dir.join("t")));
    // SAFETY: two NUL-terminated names.
    if unsafe { CreateHardLinkW(link.as_ptr(), existing.as_ptr(), std::ptr::null()) } == 0 {
        format!("refused: {}", last())
    } else {
        "made".into()
    }
}

fn case_sensitive(dir: &Path) -> String {
    let path = dir.join("cs");
    if let Err(e) = std::fs::create_dir(&path) {
        return format!("make: {e}");
    }
    let h = match open(&path, FILE_WRITE_ATTRIBUTES, false, FILE_FLAG_BACKUP_SEMANTICS) {
        Ok(h) => h,
        Err(e) => return format!("open: {e}"),
    };
    let info = FILE_CASE_SENSITIVE_INFORMATION { Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR };
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: an info of its own size on our handle.
    let s = unsafe {
        NtSetInformationFile(
            h.as_raw_handle(),
            &mut io,
            (&raw const info).cast(),
            size_of::<FILE_CASE_SENSITIVE_INFORMATION>() as u32,
            FileCaseSensitiveInformation,
        )
    };
    drop(h);
    let both = std::fs::write(path.join("A"), b"A").is_ok() && std::fs::write(path.join("a"), b"a").is_ok();
    let names = std::fs::read_dir(&path).map(|d| d.count()).unwrap_or(0);
    format!("set {}; A and a made: {both}, {names} names", status(s))
}

fn file_id(dir: &Path) -> String {
    let h = match open(&dir.join("t"), GENERIC_READ, false, 0) {
        Ok(h) => h,
        Err(e) => return format!("open: {e}"),
    };
    let mut info = FILE_ID_INFO::default();
    // SAFETY: an info of its own size on our handle.
    if unsafe { GetFileInformationByHandleEx(h.as_raw_handle(), FileIdInfo, (&raw mut info).cast(), size_of::<FILE_ID_INFO>() as u32) } == 0 {
        return format!("refused: {}", last());
    }
    let id = info.FileId.Identifier;
    let high = id[8..].iter().any(|&b| b != 0);
    format!("volume {:#x}, id {:02x?}, its upper 8 bytes used: {high}", info.VolumeSerialNumber, id)
}

fn checks(who: &str, dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        println!("m362 {who}: {}: {e}", dir.display());
        return;
    }
    let mut target = 2u32.to_le_bytes().to_vec();
    target.extend(b"../some/target");
    let lines = [
        ("EAs $LXUID $LXGID $LXMOD", eas(dir)),
        ("an LX symlink", reparse(dir, "lx-symlink", LX_SYMLINK, &target)),
        ("an LX FIFO", reparse(dir, "lx-fifo", LX_FIFO, &[])),
        ("an LX character device", reparse(dir, "lx-chr", LX_CHR, &[])),
        ("an LX block device", reparse(dir, "lx-blk", LX_BLK, &[])),
        ("a Unix socket", reparse(dir, "af-unix", AF_UNIX, &[])),
        ("an NTFS symlink, unprivileged", nt_symlink(dir)),
        ("an open file's POSIX unlink", posix_unlink(dir)),
        ("a POSIX rename over an open file", posix_rename(dir)),
        ("a hard link", hard_link(dir)),
        ("a case-sensitive directory", case_sensitive(dir)),
        ("a file's ID", file_id(dir)),
    ];
    for (what, said) in lines {
        println!("m362 {who}: {what}: {said}");
    }
    let _ = std::io::stdout().flush();
    let _ = std::fs::remove_dir_all(dir);
}

fn elevated() -> Option<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle, a handle for the call to fill.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut written = 0u32;
    // SAFETY: the token just opened, a structure of its own size.
    let ok = unsafe {
        GetTokenInformation(token, TokenElevation, (&raw mut elevation).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut written)
    };
    // SAFETY: the token opened above.
    unsafe { CloseHandle(token) };
    (ok != 0).then_some(elevation.TokenIsElevated != 0)
}

/// The checks again as a standard user (`SHARDS_PROBE_USER`), the probe copied where that user
/// may run it, its output gathered from a file there.
fn as_user(user: &str, password: &str) {
    let public = PathBuf::from(r"C:\Users\Public\shards-m362");
    let (exe, out) = (public.join("m362.exe"), public.join("out.txt"));
    let _ = std::fs::create_dir_all(&public);
    if let Err(e) = std::env::current_exe().and_then(|from| std::fs::copy(from, &exe)) {
        println!("m362 standard user: copy: {e}");
        return;
    }
    let mut cmd = wide(&format!("cmd.exe /c \"\"{}\" as-user > \"{}\" 2>&1\"", exe.display(), out.display()));
    let (user_w, domain, password_w, dir) = (wide(user), wide("."), wide(password), wide(&public.display().to_string()));
    let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: names, a command line and a directory we own; startup info alive.
    let made = unsafe {
        CreateProcessWithLogonW(
            user_w.as_ptr(),
            domain.as_ptr(),
            password_w.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            0,
            std::ptr::null(),
            dir.as_ptr(),
            &raw const si,
            &raw mut pi,
        )
    };
    if made == 0 {
        println!("m362 standard user: CreateProcessWithLogonW: {}", last());
        return;
    }
    let mut code = 0u32;
    // SAFETY: the child's handles, closed once it has ended.
    unsafe {
        WaitForSingleObject(pi.hProcess, 120_000);
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    for line in std::fs::read_to_string(&out).unwrap_or_default().lines() {
        println!("{line}");
    }
    let _ = std::fs::remove_dir_all(&public);
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("as-user") {
        println!("m362 standard user: elevated {:?}", elevated());
        checks("standard user", &PathBuf::from(r"C:\Users\Public\shards-m362\work"));
        return;
    }
    println!("m362 runner: elevated {:?}", elevated());
    // Its account's temp directory (TEMP), where a test's home is.
    let temp = std::env::var_os("TEMP").map_or_else(|| PathBuf::from("."), PathBuf::from);
    checks("runner", &temp.join("shards-m362"));
    if let (Ok(user), Ok(password)) = (std::env::var("SHARDS_PROBE_USER"), std::env::var("SHARDS_PROBE_PASSWORD")) {
        as_user(&user, &password);
    }
}
