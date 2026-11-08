// Does a listener dropped while another thread spawns a child stay bound for a while?
// One thread: bind P, drop, bind P again (each with SO_REUSEADDR, as std does); others
// spawn /usr/bin/true as fast as they can, by std's Command, or by posix_spawn with
// POSIX_SPAWN_CLOEXEC_DEFAULT (macOS) as shards_ipc::spawn does. Counts the second binds
// that fail.
//
//     cargo run --release -- SPAWNERS SECONDS std|cloexec-default
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
fn spawn_cloexec_default() {
    use std::ffi::CString;
    let path = CString::new("/usr/bin/true").unwrap();
    let argv = [path.as_ptr().cast_mut(), std::ptr::null_mut()];
    let envp = [std::ptr::null_mut()];
    // SAFETY: attributes initialized before use and destroyed once; pointers live until
    // posix_spawn returns.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&mut attr);
        libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short);
        let mut pid = 0;
        if libc::posix_spawn(&mut pid, path.as_ptr(), std::ptr::null(), &attr, argv.as_ptr(), envp.as_ptr()) == 0 {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        libc::posix_spawnattr_destroy(&mut attr);
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_cloexec_default() {
    let _ = std::process::Command::new("/usr/bin/true").status();
}

fn main() {
    let spawners: usize = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(4);
    let secs: u64 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(10);
    let mode = std::env::args().nth(3).unwrap_or_else(|| "std".into());
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..spawners {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    if mode == "std" {
                        let _ = std::process::Command::new("/usr/bin/true").status();
                    } else {
                        spawn_cloexec_default();
                    }
                }
            });
        }
        let port = {
            let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let end = Instant::now() + Duration::from_secs(secs);
        let (mut tries, mut holds) = (0u64, Vec::<Duration>::new());
        while Instant::now() < end {
            let Ok(first) = std::net::TcpListener::bind(("0.0.0.0", port)) else {
                continue;
            };
            drop(first);
            tries += 1;
            if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
                let began = Instant::now();
                while std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {}
                holds.push(began.elapsed());
            }
        }
        stop.store(true, Ordering::Relaxed);
        holds.sort();
        let at = |q: f64| holds.get(((holds.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or_default();
        println!(
            "{mode}, spawners {spawners}: {tries} drops, {} second binds refused; hold n {} p50 {:?} p90 {:?} p99 {:?} max {:?}",
            holds.len(),
            holds.len(),
            at(0.5),
            at(0.9),
            at(0.99),
            holds.last().copied().unwrap_or_default()
        );
    });
}
