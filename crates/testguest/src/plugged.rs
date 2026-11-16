//! `plugged DEST ROOT...`: an agent's or harness's view of a project plugged into it as it
//! runs (D235). Before anything is plugged, it tries to make a directory and a file beneath
//! each of its run's plug roots ROOT..., which grant nothing until a plug lands there; once
//! the host's file `from-host` appears in DEST, it reads it and writes its own,
//! `from-member`. What it saw it says in DEST's `seen`, renamed into place once whole, for
//! its host to read; then it waits to be stopped, so that its project can be unplugged as
//! it runs.
//!
//! `plugged-volumes DEST...`: what an orchestrator plugs into a running member at each DEST,
//! in order (D271): once DEST's `from-host` appears, `DEST from-host TEXT` and then
//! `DEST from-member ok` (or the errno that refused its write, a read-only plug's EROFS) on
//! stdout; then it holds the first DEST busy, its working directory there, says `holding
//! DEST`, and waits to be stopped.
//!
//! `plugged-bound DEST`: a member whose bounded volume is plugged at DEST (D281): once DEST's
//! `from-host` appears, `DEST statfs SIZE ROOM` (its file system's size and the room left in
//! it, in bytes, as df reads them), then a file of its own, `fill-` and a random name, filled
//! until a write is refused: `DEST filled BYTES errno ERRNO`, the bytes it took and the errno
//! that refused the next, its making's where that was refused; then `holding DEST`, and it
//! waits to be stopped.

use std::io::Read as _;
use std::io::Write as _;
use std::time::{Duration, Instant};

/// How long it waits for its project: test patience, the plug's round trips on a loaded
/// host; never a runtime policy.
const PATIENCE: Duration = Duration::from_secs(300);
/// How often it looks for its project meanwhile.
const LOOK: Duration = Duration::from_millis(10);

/// `ok`, or the errno that refused it.
fn said<T>(r: &std::io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
    }
}

pub fn main(args: &[String]) -> i32 {
    let Some((dest, roots)) = args.split_first() else {
        let _ = writeln!(std::io::stderr(), "plugged: DEST ROOT...");
        return 2;
    };
    let mut seen = String::new();
    for root in roots {
        let made = std::fs::create_dir(format!("{root}/plugged-dir"));
        let wrote = std::fs::File::create(format!("{root}/plugged-file"));
        seen.push_str(&format!(
            "root {root} mkdir {} create {}\n",
            said(&made),
            said(&wrote)
        ));
    }
    let from_host = format!("{dest}/from-host");
    let start = Instant::now();
    let host = loop {
        match std::fs::read_to_string(&from_host) {
            Ok(text) => break text,
            Err(_) if start.elapsed() < PATIENCE => std::thread::sleep(LOOK),
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "plugged: {from_host}: {e}");
                return 1;
            }
        }
    };
    seen.push_str(&format!("from-host {}\n", host.trim_end()));
    let wrote = std::fs::write(format!("{dest}/from-member"), "member\n");
    seen.push_str(&format!("from-member {}\n", said(&wrote)));
    let whole = format!("{dest}/seen.part");
    if let Err(e) =
        std::fs::write(&whole, &seen).and_then(|()| std::fs::rename(&whole, format!("{dest}/seen")))
    {
        let _ = writeln!(std::io::stderr(), "plugged: {dest}/seen: {e}");
        return 1;
    }
    let _ = write!(std::io::stdout(), "{seen}");
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

/// `plugged-volumes DEST...` ([the module's words](self)).
pub fn volumes(dests: &[String]) -> i32 {
    let Some(first) = dests.first() else {
        let _ = writeln!(std::io::stderr(), "plugged-volumes: DEST...");
        return 2;
    };
    for dest in dests {
        let from_host = format!("{dest}/from-host");
        let start = Instant::now();
        let host = loop {
            match std::fs::read_to_string(&from_host) {
                Ok(text) => break text,
                Err(_) if start.elapsed() < PATIENCE => std::thread::sleep(LOOK),
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "plugged-volumes: {from_host}: {e}");
                    return 1;
                }
            }
        };
        let wrote = std::fs::write(format!("{dest}/from-member"), "member\n");
        let _ = writeln!(std::io::stdout(), "{dest} from-host {}", host.trim_end());
        let _ = writeln!(std::io::stdout(), "{dest} from-member {}", said(&wrote));
    }
    if let Err(e) = std::env::set_current_dir(first) {
        let _ = writeln!(std::io::stderr(), "plugged-volumes: {first}: {e}");
        return 1;
    }
    let _ = writeln!(std::io::stdout(), "holding {first}");
    let _ = std::io::stdout().flush();
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

/// `plugged-bound DEST` ([the module's words](self)).
pub fn bound(args: &[String]) -> i32 {
    let Some(dest) = args.first() else {
        let _ = writeln!(std::io::stderr(), "plugged-bound: DEST");
        return 2;
    };
    let from_host = format!("{dest}/from-host");
    let start = Instant::now();
    while std::fs::metadata(&from_host).is_err() {
        if start.elapsed() >= PATIENCE {
            let _ = writeln!(std::io::stderr(), "plugged-bound: {from_host} never came");
            return 1;
        }
        std::thread::sleep(LOOK);
    }
    let Ok(path) = std::ffi::CString::new(dest.as_str()) else {
        return 2;
    };
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs(3) of a NUL-terminated path into a struct it owns.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut st) } != 0 {
        let e = std::io::Error::last_os_error();
        let _ = writeln!(std::io::stderr(), "plugged-bound: statvfs {dest}: {e}");
        return 1;
    }
    let _ = writeln!(
        std::io::stdout(),
        "{dest} statfs {} {}",
        st.f_blocks.saturating_mul(st.f_frsize),
        st.f_bavail.saturating_mul(st.f_frsize)
    );
    let mut name = [0u8; 8];
    if let Err(e) = std::fs::File::open("/dev/urandom").and_then(|mut r| r.read_exact(&mut name)) {
        let _ = writeln!(std::io::stderr(), "plugged-bound: a name: {e}");
        return 1;
    }
    let name: String = name.iter().map(|b| format!("{b:02x}")).collect();
    let chunk = vec![7u8; 64 << 10];
    let mut filled = 0u64;
    let refused = match std::fs::File::create(format!("{dest}/fill-{name}")) {
        Err(e) => e,
        Ok(mut f) => loop {
            match f.write(&chunk) {
                Ok(0) => break std::io::Error::from(std::io::ErrorKind::WriteZero),
                Ok(n) => filled = filled.saturating_add(u64::try_from(n).unwrap_or(u64::MAX)),
                Err(e) => break e,
            }
        },
    };
    let errno = refused.raw_os_error().unwrap_or(0);
    let _ = writeln!(std::io::stdout(), "{dest} filled {filled} errno {errno}");
    let _ = writeln!(std::io::stdout(), "holding {dest}");
    let _ = std::io::stdout().flush();
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}
