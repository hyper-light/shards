use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;
use std::process::{Command, Stdio};
use windows_sys::Wdk::Storage::FileSystem::{FileProcessIdsUsingFileInformation, NtQueryInformationFile};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, UnmapViewOfFile,
};
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};

fn nt_users(label: &str, path: &Path) -> Option<Vec<usize>> {
    #[repr(C)]
    struct Ids {
        count: u32,
        ids: [usize; 64],
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            println!("{label}: NT query open failed: {e}");
            return None;
        }
    };
    let mut io = IO_STATUS_BLOCK::default();
    let mut ids = Ids {
        count: 0,
        ids: [0; 64],
    };
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut io,
            (&raw mut ids).cast(),
            std::mem::size_of::<Ids>() as u32,
            FileProcessIdsUsingFileInformation,
        )
    };
    println!(
        "{label}: NT status={status:#010x} info={} count={} list_offset={}",
        io.Information,
        ids.count,
        std::mem::offset_of!(Ids, ids)
    );
    if status != 0 {
        return None;
    }
    assert!(ids.count as usize <= ids.ids.len());
    let found = ids.ids[..ids.count as usize].to_vec();
    println!(
        "{label}: NT users={found:?}; query itself holds a handle in observer PID{}",
        std::process::id()
    );
    Some(found)
}

fn inspect(label: &str, file: &Path) -> Option<Vec<usize>> {
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let started = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    println!(
        "{label}: file={} metadata={:?} start={started}",
        file.display(),
        file.metadata().map(|m| (m.is_file(), m.is_dir(), m.len()))
    );
    if started != 0 {
        return None;
    }
    let wide: Vec<u16> = file.as_os_str().encode_wide().chain([0]).collect();
    let registered = unsafe {
        RmRegisterResources(
            session,
            1,
            [wide.as_ptr()].as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        )
    };
    let mut found = vec![RM_PROCESS_INFO::default(); 64];
    let (mut needed, mut listed, mut reasons) = (0u32, 64u32, 0u32);
    let got = unsafe {
        RmGetList(
            session,
            &mut needed,
            &mut listed,
            found.as_mut_ptr(),
            &mut reasons,
        )
    };
    println!(
        "{label}: register={registered} get={got} needed={needed} listed={listed} reasons={reasons} struct_size={} users={:?}",
        std::mem::size_of::<RM_PROCESS_INFO>(),
        if got == 0 {
            found
                .iter()
                .take(listed as usize)
                .map(|p| p.Process.dwProcessId)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    );
    println!("{label}: end={}", unsafe { RmEndSession(session) });
    nt_users(label, file)
}

fn actor(path: &Path) {
    let file = File::open(path).unwrap();
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        )
    };
    assert!(!mapping.is_null());
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
    assert!(!view.Value.is_null());
    std::hint::black_box(unsafe { std::ptr::read_volatile(view.Value.cast::<u8>()) });
    println!("ready {}", std::process::id());
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "close-file");
    drop(file);
    println!("mapped-only");
    std::io::stdout().flush().unwrap();
    line.clear();
    std::io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "unmap");
    assert_ne!(unsafe { UnmapViewOfFile(view) }, 0);
    assert_ne!(unsafe { CloseHandle(mapping) }, 0);
    println!("unmapped");
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--hold") {
        actor(Path::new(&std::env::args_os().nth(2).unwrap()));
        return;
    }
    println!("observer_pid={}", std::process::id());
    let root = std::env::temp_dir().join(format!("shards-restart-manager-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory");
    File::create(&path).unwrap().set_len(1 << 20).unwrap();
    inspect("closed", &path);
    let file = File::open(&path).unwrap();
    inspect("open", &path);
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        )
    };
    assert!(!mapping.is_null(), "{}", std::io::Error::last_os_error());
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
    assert!(!view.Value.is_null(), "{}", std::io::Error::last_os_error());
    std::hint::black_box(unsafe { std::ptr::read_volatile(view.Value.cast::<u8>()) });
    inspect("open-and-mapped", &path);
    drop(file);
    inspect("mapped-only", &path);
    assert_ne!(unsafe { UnmapViewOfFile(view) }, 0);
    assert_ne!(unsafe { CloseHandle(mapping) }, 0);
    inspect("unmapped", &path);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--hold")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as usize;
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), format!("ready {pid}"));
    assert!(inspect("child-open-and-mapped", &path).unwrap().contains(&pid));
    child.stdin.as_mut().unwrap().write_all(b"close-file\n").unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "mapped-only");
    assert!(inspect("child-mapped-only", &path).unwrap().contains(&pid));
    child.stdin.as_mut().unwrap().write_all(b"unmap\n").unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "unmapped");
    assert!(child.wait().unwrap().success());
    assert!(!inspect("child-ended", &path).unwrap().contains(&pid));
    inspect("directory-control", &root);
    for extra in std::env::args_os().skip(1) {
        inspect("extra-read-only", Path::new(&extra));
    }
    std::fs::remove_dir_all(&root).unwrap();
}
