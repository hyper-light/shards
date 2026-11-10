use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

pub fn probe(path: &str) -> i32 {
    match report(path) {
        Ok(()) => hold(),
        Err(e) => {
            let _ = writeln!(io::stderr(), "managed lifecycle probe: {e}");
            1
        }
    }
}

pub fn exit_after_report(path: &str) -> i32 {
    match report(path) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(io::stderr(), "managed lifecycle exit probe: {e}");
            1
        }
    }
}

pub fn challenge(directory: &str) -> i32 {
    match answer(directory) {
        Ok(()) => hold(),
        Err(e) => {
            let _ = writeln!(io::stderr(), "managed lifecycle challenge: {e}");
            1
        }
    }
}

fn answer(directory: &str) -> io::Result<()> {
    use std::io::Read as _;
    let root = std::path::Path::new(directory);
    let path = std::ffi::CString::new(directory)?;
    // SAFETY: inotify_init1 creates a private, blocking notification descriptor.
    let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call just created this sole owned descriptor.
    let events = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: add_watch reads a valid NUL-terminated directory name.
    if unsafe {
        libc::inotify_add_watch(
            events.as_raw_fd(),
            path.as_ptr(),
            libc::IN_CREATE | libc::IN_MOVED_TO,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    std::fs::write(root.join("ready"), b"ready")?;
    let mut event = [0u8; size_of::<libc::inotify_event>() + libc::NAME_MAX as usize + 1];
    let mut input = loop {
        match std::fs::File::open(root.join("go")) {
            Ok(file) => break file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // SAFETY: the owned descriptor writes only within this initialized buffer.
        let read = unsafe { libc::read(events.as_raw_fd(), event.as_mut_ptr().cast(), event.len()) };
        if read < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    };
    let mut challenge = [0u8; size_of::<u128>()];
    input.read_exact(&mut challenge)?;
    let mut extra = [0u8; 1];
    if input.read(&mut extra)? != 0 {
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }
    let mut result = std::fs::File::create(root.join("answer"))?;
    result.write_all(&challenge)?;
    result.sync_all()?;
    let _ = writeln!(io::stdout(), "tenant challenge answered");
    Ok(())
}

fn report(path: &str) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    // SAFETY: stat(2) initializes this output and kill(2) with signal zero only checks access.
    let (owner, signal) = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(c"/proc/1".as_ptr(), &raw mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        let signal = if libc::kill(1, 0) == 0 {
            0
        } else {
            io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        (st.st_uid, signal)
    };
    let memory = proc_access(c"/proc/1/mem", libc::O_RDONLY);
    let descriptors = proc_access(c"/proc/1/fd", libc::O_RDONLY | libc::O_DIRECTORY);
    writeln!(file, "actor_pid={}", std::process::id())?;
    // SAFETY: getppid(2) has no arguments or memory access.
    writeln!(file, "actor_ppid={}", unsafe { libc::getppid() })?;
    writeln!(file, "namespace_init_owner={owner}")?;
    writeln!(file, "namespace_init_signal_errno={signal}")?;
    writeln!(file, "namespace_init_memory_errno={memory}")?;
    writeln!(file, "namespace_init_descriptors_errno={descriptors}")?;
    file.sync_all()
}

fn proc_access(path: &std::ffi::CStr, flags: libc::c_int) -> i32 {
    // SAFETY: open(2) reads a fixed NUL-terminated path; any returned descriptor closes here.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        // SAFETY: fd is the descriptor just opened.
        unsafe { libc::close(fd) };
        0
    }
}

fn hold() -> ! {
    // SAFETY: prctl(2) reads a fixed thread name; pause(2) has no arguments.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"lifecycle-ready".as_ptr());
        loop {
            libc::pause();
        }
    }
}
