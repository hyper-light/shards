use std::io::{self, Write as _};

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
    let saved = (|| {
        let mut file = std::fs::File::create(path)?;
        writeln!(file, "actor_pid={}", std::process::id())?;
        file.sync_all()
    })();
    match saved {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(io::stderr(), "managed lifecycle exit probe: {e}");
            1
        }
    }
}

pub fn challenge(directory: &str, port: &str) -> i32 {
    match answer(directory, port) {
        Ok(()) => hold(),
        Err(e) => {
            let _ = writeln!(io::stderr(), "managed lifecycle challenge: {e}");
            1
        }
    }
}

fn answer(directory: &str, port: &str) -> io::Result<()> {
    use std::io::Read as _;
    let root = std::path::Path::new(directory);
    let port = port.parse::<u16>().map_err(io::Error::other)?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))?;
    std::fs::write(root.join("ready"), b"ready")?;
    let (mut input, _) = listener.accept()?;
    let mut challenge = [0u8; size_of::<u128>()];
    input.read_exact(&mut challenge)?;
    let mut extra = [0u8; 1];
    if input.read(&mut extra)? != 0 {
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }
    let pending = root.join("answer.pending");
    let mut result = std::fs::File::create(&pending)?;
    result.write_all(&challenge)?;
    result.sync_all()?;
    std::fs::rename(pending, root.join("answer"))?;
    input.write_all(&challenge)?;
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
