//! `plugged DEST ROOT...`: an agent's or harness's view of a project plugged into it as it
//! runs (D235). Before anything is plugged, it tries to make a directory and a file beneath
//! each of its run's plug roots ROOT..., which grant nothing until a plug lands there; once
//! the host's file `from-host` appears in DEST, it reads it and writes its own,
//! `from-member`. What it saw it says in DEST's `seen`, renamed into place once whole, for
//! its host to read; then it waits to be stopped, so that its project can be unplugged as
//! it runs.

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
