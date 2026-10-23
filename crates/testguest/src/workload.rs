//! The test guest run as a workload, not as PID 1: `shards-testguest <mode> [args]`, run
//! inside an image by shards-init (crates/shards/tests/run.rs).

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use shards_testguest::fill;

/// SIGPIPE's disposition as the workload started: 0 default, 1 ignored, 2 handled. Rust's
/// runtime ignores SIGPIPE before main, so a constructor records it first.
static SIGPIPE_AT_START: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record_sigpipe() {
    // SAFETY: sigaction(2) only reads the disposition.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut old) == 0 {
            let v = match old.sa_sigaction {
                libc::SIG_DFL => 0,
                libc::SIG_IGN => 1,
                _ => 2,
            };
            SIGPIPE_AT_START.store(v, Ordering::Relaxed);
        }
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static RECORD_SIGPIPE: extern "C" fn() = record_sigpipe;

pub fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    let code = match arg(0) {
        "report" => report(),
        "cat" => cat(),
        // DMA through each PCI function at ADDRESS..., one group's, by its VFIO (D154):
        // its group's, or its own device node's and iommufd.
        "vfio-dma" | "vfio-dma-cdev" => {
            let functions = args.get(1..).unwrap_or_default();
            let done = if arg(0) == "vfio-dma" {
                crate::linux::vfio_dma(functions)
            } else {
                crate::linux::vfio_dma_cdev(functions)
            };
            match done {
                Ok(()) => 0,
                Err(e) => {
                    let _ = writeln!(io::stderr(), "{}: {e}", arg(0));
                    1
                }
            }
        }
        // Each file read whole: its first line, or the errno that refused it.
        "slurp" => {
            for p in args.get(1..).unwrap_or_default() {
                let said = match std::fs::read_to_string(p) {
                    Ok(t) => t.lines().next().unwrap_or_default().to_string(),
                    Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
                };
                let _ = writeln!(io::stdout(), "slurp {p}: {said}");
            }
            0
        }
        // Each file whole, after a line naming it, or the errno that refused it: what a
        // failing test keeps of the guest's state. `@cgroup/NAME` is NAME in this
        // process's own cgroup, which an exec shares with its run's command.
        "files" => {
            let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
            let own = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/");
            for p in args.get(1..).unwrap_or_default() {
                let path = match p.strip_prefix("@cgroup/") {
                    Some(name) => format!("/sys/fs/cgroup{}/{name}", own.trim_end_matches('/')),
                    None => p.clone(),
                };
                let said = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| format!("errno {}\n", e.raw_os_error().unwrap_or(0)));
                let _ = write!(io::stdout(), "== {path}\n{said}");
            }
            0
        }
        // Real-time scheduling asked for: SCHED_FIFO at priority 1, or the errno that
        // refused it.
        "fifo" => {
            // SAFETY: sched_param is plain integers, all zero valid (musl's has more than
            // the priority).
            let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
            param.sched_priority = 1;
            // The system call itself, as glibc makes it: musl's wrapper says ENOSYS without
            // asking the kernel (src/sched/sched_setscheduler.c).
            // SAFETY: sched_setscheduler(2) reads the sched_param it is given.
            let said = if unsafe {
                libc::syscall(
                    libc::SYS_sched_setscheduler,
                    0,
                    libc::SCHED_FIFO,
                    &raw const param,
                )
            } == 0
            {
                "ok".to_string()
            } else {
                format!("errno {}", io::Error::last_os_error().raw_os_error().unwrap_or(0))
            };
            let _ = writeln!(io::stdout(), "fifo: {said}");
            0
        }
        "stat" => stat(args.get(1..).unwrap_or_default()),
        // Each path's device: `PATH char MAJOR:MINOR`, `PATH block MAJOR:MINOR`, or
        // `PATH none` (an overlay's whiteout is a character device of 0:0).
        "devnode" => {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
            for p in args.get(1..).unwrap_or_default() {
                let said = match std::fs::symlink_metadata(p) {
                    Ok(m) if m.file_type().is_char_device() || m.file_type().is_block_device() => {
                        let kind = if m.file_type().is_char_device() {
                            "char"
                        } else {
                            "block"
                        };
                        let rdev = m.rdev();
                        format!("{kind} {}:{}", libc::major(rdev), libc::minor(rdev))
                    }
                    Ok(_) => "none".into(),
                    Err(e) => format!("missing {e}"),
                };
                let _ = writeln!(io::stdout(), "{p} {said}");
            }
            0
        }
        "tree" => tree(arg(1)),
        "ps" => ps(),
        "stacks" => stacks(),
        "mtime" => {
            use std::os::unix::fs::MetadataExt as _;
            for p in args.get(1..).unwrap_or_default() {
                let t = std::fs::symlink_metadata(p).map_or(-1, |m| m.mtime());
                let _ = writeln!(io::stdout(), "{p} {t}");
            }
            0
        }
        "stderr" => {
            let _ = io::stderr().write_all(arg(1).as_bytes());
            0
        }
        // N lines on stdout, `line 1` to `line N`: a log's text, past its size.
        "count" => count(arg(1).parse().unwrap_or(0)),
        // One line of N `x`s on stdout: longer than a log driver takes whole.
        "wide" => {
            let line = [vec![b'x'; arg(1).parse().unwrap_or(0)], b"\n".to_vec()].concat();
            i32::from(
                io::stdout()
                    .write_all(&line)
                    .and_then(|()| io::stdout().flush())
                    .is_err(),
            )
        }
        // OUT on stdout, then ERR on stderr, a line each: which streams a client attached.
        "streams" => {
            let _ = writeln!(io::stdout(), "{}", arg(1));
            let _ = io::stdout().flush();
            let _ = writeln!(io::stderr(), "{}", arg(2));
            0
        }
        "exit" => arg(1).parse().unwrap_or(1),
        // A step that lasts `arg(1)` ms, for what shows it to be seen as it runs (`sleep`
        // prints `ready` and lasts until a signal ends it).
        "sleep-for" => {
            std::thread::sleep(std::time::Duration::from_millis(arg(1).parse().unwrap_or(0)));
            0
        }
        "limits" => match limits() {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(io::stderr(), "{e}");
                1
            }
        },
        // Writes a file of N MiB, a byte in each page: what a layer holds.
        "big" => match big(arg(1), arg(2).parse().unwrap_or(0)) {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(io::stderr(), "{}: {e}", arg(1));
                1
            }
        },
        // Reads each file named through, a megabyte at a time, and says how much: what it
        // read stays in the page cache, the reader's own memory one megabyte.
        "read" => read_through(args.get(1..).unwrap_or_default()),
        // Touches N MiB, a page at a time, then says so: what a memory limit stops.
        "alloc" => {
            let mib: usize = arg(1).parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..mib {
                let mut chunk = vec![0u8; 1 << 20];
                for page in chunk.chunks_mut(4096) {
                    if let Some(b) = page.first_mut() {
                        *b = 1;
                    }
                }
                held.push(chunk);
            }
            let _ = writeln!(io::stdout(), "allocated {}", held.len());
            0
        }
        // Walks MIB MiB, one word a page, STEPS times in an order of its own, each step
        // the address the step before read, and says the nanoseconds a step took: a load
        // whose translation every step misses, as a host's address space shapes it (D159,
        // PM M230).
        "walk" => walk(arg(1).parse().unwrap_or(0), arg(2).parse().unwrap_or(0)),
        // Touches N MiB as `alloc` does, frees it, says so, and idles for SECONDS: memory
        // its microVM may give back (D151).
        "alloc-free" => {
            let mib: usize = arg(1).parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..mib {
                held.push(vec![1u8; 1 << 20]);
            }
            let n = held.len();
            drop(held);
            let _ = writeln!(io::stdout(), "freed {n}");
            std::thread::sleep(std::time::Duration::from_secs(arg(2).parse().unwrap_or(0)));
            0
        }
        // N MiB in one allocation, every page touched, then says so: what the kernel's
        // overcommit heuristic refuses past the memory it counts (D149).
        "alloc-once" => {
            let mib: usize = arg(1).parse().unwrap_or(0);
            let mut held: Vec<u8> = Vec::new();
            match held.try_reserve_exact(mib << 20) {
                Ok(()) => {
                    held.resize(mib << 20, 1);
                    let _ = writeln!(io::stdout(), "allocated {} at once", held.len() >> 20);
                    0
                }
                Err(e) => {
                    let _ = writeln!(io::stdout(), "alloc-once {mib}: {e}");
                    1
                }
            }
        }
        // Who it runs as, real and effective: what a set-user-ID binary is given.
        "creds" => {
            // SAFETY: plain getters.
            let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
            let _ = writeln!(io::stdout(), "creds uid={uid} euid={euid}");
            0
        }
        "kill" => {
            // SAFETY: raise(2) on ourselves.
            unsafe { libc::raise(libc::SIGKILL) };
            1
        }
        "bulk" => bulk(arg(1).parse().unwrap_or(0), arg(2).parse().unwrap_or(0)),
        // Each file's bytes, or variable's (`env:NAME`), as their FNV-1a, never the bytes.
        "digest" => {
            for named in args.iter().skip(1) {
                let bytes = match named.strip_prefix("env:") {
                    Some(name) => std::env::var_os(name).map(|v| v.into_encoded_bytes()),
                    None => std::fs::read(named).ok(),
                };
                let said = bytes.map_or("none".into(), |b| format!("{:016x}", shards_testguest::fnv1a(&b)));
                let _ = writeln!(io::stdout(), "digest {named} {said}");
            }
            0
        }
        "hash" => {
            let hash =
                shards_testguest::pattern_hash(arg(2).parse().unwrap_or(0), arg(1).parse().unwrap_or(0));
            let _ = writeln!(io::stdout(), "{hash:016x}");
            0
        }
        "clean-cache" => clean_cache(arg(1)),
        "orphan" => orphan(),
        "heavy" => heavy(arg(1).parse().unwrap_or(0)),
        "burn" => burn(
            arg(1).parse().unwrap_or(1),
            arg(2).parse().unwrap_or(1000),
            arg(3) == "escape",
        ),
        "trap" => trap(arg(1)),
        "ignore" => ignore(arg(1)),
        "tty" => tty(arg(1)),
        "vsock" => vsock(args.get(1..).unwrap_or_default()),
        // Dials host port `arg(1)` itself, says a hello with a token none gave it, then asks
        // an Engine API ping: what a process of a microVM's reaches of its engine (D181)
        // past the socket its init made, `PORT closed`, `PORT answered N bytes`, `PORT
        // nothing` or `PORT refused E`.
        "vsock-hello" => vsock_hello(arg(1).parse().unwrap_or(0)),
        "loopback" => loopback(),
        "fs" => fs(args.get(1..).unwrap_or_default()),
        "addrs" => addrs(),
        "neighbors" => neighbors(),
        "spawn" => spawn(args.get(1..).unwrap_or_default()),
        "spawns" => spawns(arg(1).parse().unwrap_or(1), args.get(2..).unwrap_or_default()),
        "tcp" => tcp(arg(1)),
        "fetch" => fetch(args.get(1..).unwrap_or_default()),
        // One call of the Engine API at $DOCKER_HOST, as a program in a run reaches its
        // microVM's own engine (D181): `api METHOD PATH [BODY [upgrade]]`, `upgrade` asking
        // for the connection to be taken over for a stream, as attaches and execs ask.
        "api" => api(arg(1), arg(2), arg(3), arg(4) == "upgrade"),
        // `docker run`'s calls, as its CLI makes them, through $DOCKER_HOST: a create, an
        // attach before the start, the start and a wait on one kept connection, the
        // attach's own: `api-run IMAGE CMD...` says what the command wrote, then `status N`
        // (D181).
        "api-run" => api_run(arg(1), args.get(2..).unwrap_or_default()),
        "ask" => ask(arg(1), arg(2).parse().ok()),
        "resolve" => resolve(arg(1)),
        "dnsttl" => dnsttl(arg(1)),
        "dnsrtt" => dnsrtt(arg(1), arg(2).parse().unwrap_or(1000)),
        "udp" => udp(arg(1)),
        "serve" => serve(arg(1), arg(2).parse().unwrap_or(1)),
        "listen" => listen(arg(1), arg(2).parse().unwrap_or(1)),
        "rebind" => rebind(arg(1), arg(2).parse().unwrap_or(1)),
        "binds" => binds(arg(1).parse().unwrap_or(1)),
        "udp-serve" => udp_serve(arg(1), arg(2) == "forget"),
        "udp-send" => udp_send(
            arg(1),
            arg(2).parse().unwrap_or(1),
            arg(3).parse().unwrap_or(64),
            arg(4).parse().unwrap_or(0),
        ),
        "udp-count" => udp_count(arg(1), arg(2).parse().unwrap_or(500)),
        "agent" => agent(),
        "vsock-agent" => vsock_agent(arg(1).parse().unwrap_or(1028)),
        "hold" => hold(arg(1)),
        "conns" => conns(
            arg(1),
            arg(2).parse().unwrap_or(0),
            arg(3).parse().unwrap_or(0),
            arg(4).parse().unwrap_or(0),
        ),
        "udp-echo" => udp_echo(arg(1), arg(2)),
        "spin" => {
            let _ = writeln!(io::stdout(), "ready");
            let mut n = 0u64;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        "confined" => confined(args.get(1..).unwrap_or_default()),
        // An SBOM generator, as BuildKit's protocol runs one (D81).
        "sbomscan" => sbom_scan(),
        "await" => await_process(arg(1), arg(2).parse().unwrap_or(1), arg(3)),
        // `await`, `arg(1)` ms late, as a command slow to start under load is: the host must
        // not signal it before it waits (`run_watching`).
        "await-late" => {
            std::thread::sleep(std::time::Duration::from_millis(arg(1).parse().unwrap_or(0)));
            await_process("late", 1, "")
        }
        // A workload's own Unix sockets, open to anyone: a pathname one at arg 1, mode
        // 0777, and an abstract one named arg 2; held while it awaits arg 3's processes.
        "unix-listen" => {
            use std::os::unix::fs::PermissionsExt as _;
            use std::os::unix::net::UnixListener;
            let path = arg(1);
            let _ = std::fs::remove_file(path);
            let held = UnixListener::bind(path).and_then(|l| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
                Ok(l)
            });
            let abstract_held = abstract_listener(arg(2));
            match (held, abstract_held) {
                (Ok(_l), Ok(_a)) => {
                    let _ = writeln!(io::stdout(), "unix-listen bound {}", monotonic_ns());
                    await_process(arg(3), arg(4).parse().unwrap_or(1), "")
                }
                (Err(e), _) | (_, Err(e)) => {
                    let _ = writeln!(io::stdout(), "unix-listen error {e}");
                    1
                }
            }
        }
        "outlive" => outlive(arg(1).parse().unwrap_or(0)),
        // Once the host says `arg(2)` processes named `arg(1)` run (`await`), what the run's
        // own command sees of the microVM's processes (D115).
        "beside" => match await_process(arg(1), arg(2).parse().unwrap_or(0), "") {
            0 => beside(),
            code => code,
        },
        // Where the microVM is (its first non-loopback IPv4 address and its default
        // gateway), then TCP and UDP answered on `arg(1)`, until `arg(3)` processes named
        // `arg(2)` exist.
        "sweep-target" => {
            let _ = writeln!(io::stdout(), "where {} {}", own_address(), gateway());
            let port: u16 = arg(1).parse().unwrap_or(0);
            if let Ok(l) = std::net::TcpListener::bind(("0.0.0.0", port)) {
                let _ = std::thread::Builder::new().spawn(move || {
                    for c in l.incoming().flatten() {
                        let mut c = c;
                        let _ = c.write_all(b"workload\n");
                    }
                });
            }
            if let Ok(u) = std::net::UdpSocket::bind(("0.0.0.0", port)) {
                let _ = std::thread::Builder::new().spawn(move || {
                    let mut b = [0u8; 64];
                    while let Ok((n, from)) = u.recv_from(&mut b) {
                        let _ = u.send_to(b.get(..n).unwrap_or_default(), from);
                    }
                });
            }
            await_process(arg(2), arg(3).parse().unwrap_or(1), "")
        }
        // MIB MiB of a pattern of its own, more than a memory limit lets it hold, so that
        // some is swapped out: then each round reads all of it back, every 200 ms, and says
        // `kept ROUND`, or `lost AT` and ends 1 at the first byte not as written: what a
        // saved microVM keeps of its swap, and a restore of it maps back (D176).
        "swap-kept" => swap_kept(arg(1).parse().unwrap_or(0)),
        // A counter in memory, printed as it grows, every `arg(1)` ms (100 if none): what a
        // saved microVM keeps, and a restore of it goes on from (D176).
        "counter" => {
            let every = std::time::Duration::from_millis(arg(1).parse().unwrap_or(100));
            let _ = writeln!(io::stdout(), "ready");
            let mut n = 0u64;
            loop {
                n += 1;
                let _ = writeln!(io::stdout(), "{n}");
                std::thread::sleep(every);
            }
        }
        // A counter written to file `arg(1)` as it grows, a line every `arg(2)` ms, each line
        // synced, through one descriptor opened once in the file's directory, which it works
        // in, and printed: a volume's file and directory a saved microVM holds, which a
        // restore of it goes on writing through (D176).
        "appender" => appender(arg(1), arg(2).parse().unwrap_or(100)),
        // `arg(2)` bytes written to file `arg(1)` in writes of `arg(3)`, each synced, then
        // `poured N in MS ms`: what a microVM's I/O limits let through, and how fast (D178).
        "pour" => pour(
            arg(1),
            arg(2).parse().unwrap_or(0),
            arg(3).parse().unwrap_or(4096),
        ),
        // File `arg(1)` made holding `held`, of `arg(2)` bytes where given (the rest a
        // hole), held open, and its name removed; then, every 100 ms, a count written after
        // its first line through that descriptor and synced, which its server must take, and
        // the line read back and printed: a file a guest holds that no path reaches, whose
        // bytes a save keeps and a restore of it holds again (D176).
        "unlinked-held" => unlinked_held(arg(1), arg(2).parse().ok()),
        "sleep" => {
            let _ = writeln!(io::stdout(), "ready");
            loop {
                // SAFETY: blocks until a signal arrives.
                unsafe { libc::pause() };
            }
        }
        // As a nested runtime starts (BuildKit's buildkitd, Docker in Docker's entrypoint):
        // moves itself into a cgroup below its own, and enables a controller for its own
        // cgroup's children, so that cgroup v2 keeps every process out of it from then on
        // (no internal processes); says `nested`, then sleeps.
        "nest" => {
            let root = "/sys/fs/cgroup";
            let nested = std::fs::create_dir_all(format!("{root}/init"))
                .and_then(|()| std::fs::write(format!("{root}/init/cgroup.procs"), "0"))
                .and_then(|()| std::fs::write(format!("{root}/cgroup.subtree_control"), "+pids"));
            match nested {
                Ok(()) => {
                    let _ = writeln!(io::stdout(), "nested");
                    loop {
                        // SAFETY: blocks until a signal arrives.
                        unsafe { libc::pause() };
                    }
                }
                Err(e) => {
                    let _ = writeln!(io::stdout(), "nest error {e}");
                    1
                }
            }
        }
        // Its IPC as it has it (D156): its namespace, its /dev/shm's size or none, and
        // whether it sees System V segment KEY and file /dev/shm/KEY.
        "ipc" => ipc_report(arg(1)),
        // Makes System V segment KEY and file /dev/shm/KEY, says so, and holds them.
        "ipc-hold" => match ipc_hold(arg(1)) {
            0 => loop {
                // SAFETY: blocks until a signal arrives.
                unsafe { libc::pause() };
            },
            code => code,
        },
        // The kernel's log records holding any of WORDS, as /dev/kmsg has them now (read
        // without waiting for more): what a kernel said of a mount it refused.
        "kernlog" => kernlog(args.get(1..).unwrap_or_default()),
        // The lines of file PATH holding any of WORDS (/proc/vmstat, /dev/kmsg's, ...).
        "lines" => {
            let text = std::fs::read(arg(1)).map(|b| String::from_utf8_lossy(&b).into_owned());
            match text {
                Ok(text) => {
                    for line in text.lines() {
                        if args
                            .get(2..)
                            .unwrap_or_default()
                            .iter()
                            .any(|w| line.contains(w.as_str()))
                        {
                            let _ = writeln!(io::stdout(), "{line}");
                        }
                    }
                    0
                }
                Err(e) => {
                    let _ = writeln!(io::stdout(), "lines {}: {e}", arg(1));
                    1
                }
            }
        }
        // Each KEY's line of /proc/meminfo, as the kernel says it (`SwapTotal`, ...).
        "meminfo" => {
            let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
            for key in args.get(1..).unwrap_or_default() {
                let line = info
                    .lines()
                    .find(|l| l.split(':').next() == Some(key.as_str()))
                    .unwrap_or("none");
                let _ = writeln!(
                    io::stdout(),
                    "{}",
                    line.split_whitespace().collect::<Vec<_>>().join(" ")
                );
            }
            0
        }
        // The objects in use of slab cache NAME, as /proc/slabinfo says them: `slab NAME N`,
        // or `slab NAME none`.
        "slab" => {
            let info = std::fs::read_to_string("/proc/slabinfo").unwrap_or_default();
            let active = info.lines().find_map(|l| {
                let mut f = l.split_whitespace();
                (f.next() == Some(arg(1))).then(|| f.next()).flatten()
            });
            let _ = writeln!(io::stdout(), "slab {} {}", arg(1), active.unwrap_or("none"));
            0
        }
        // Its umask, as the kernel tells it (/proc/self/status).
        "umask" => {
            let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let mask = status
                .lines()
                .find_map(|l| l.strip_prefix("Umask:"))
                .map_or("none", str::trim);
            let _ = writeln!(io::stdout(), "umask {mask}");
            0
        }
        other => {
            let _ = writeln!(io::stderr(), "unknown mode {other:?}");
            2
        }
    };
    std::process::exit(code)
}

/// Prints each process its `/proc` shows but itself, by PID: `PID STATE ARGS`, its state
/// as `/proc/PID/stat` says it and its arguments with NULs as spaces (none for a zombie).
fn ps() -> i32 {
    let me = std::process::id();
    let mut pids: Vec<u32> = std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    pids.sort_unstable();
    let mut out = String::new();
    for pid in pids.into_iter().filter(|&p| p != me) {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?");
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<String> = args
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        out.push_str(&format!("{pid} {state} {}\n", args.join(" ")));
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

/// What each process of the guest's is doing, for a test's hang to be read by (CI's
/// scripts/ci/hang-watch.sh): `PID STATE WCHAN ARGS`, then its kernel stack, indented.
fn stacks() -> i32 {
    let mut pids: Vec<u32> = std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    pids.sort_unstable();
    let mut out = String::new();
    for pid in pids {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?");
        let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan")).unwrap_or_default();
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<String> = args
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        out.push_str(&format!("{pid} {state} {} {}\n", wchan.trim(), args.join(" ")));
        for line in std::fs::read_to_string(format!("/proc/{pid}/stack"))
            .unwrap_or_default()
            .lines()
        {
            out.push_str(&format!("    {line}\n"));
        }
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

/// Prints each path as lstat(2) sees it: `PATH TYPE MODE UID:GID SIZE`, then `= TEXT`
/// for a file, its bytes with newlines as `\n`, or `-> TARGET` for a symlink.
fn stat(paths: &[String]) -> i32 {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let mut out = String::new();
    let mut code = 0;
    for p in paths {
        let m = match std::fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) => {
                out.push_str(&format!("{p} missing {e}\n"));
                code = 1;
                continue;
            }
        };
        let t = m.file_type();
        let kind = if t.is_dir() {
            "dir"
        } else if t.is_symlink() {
            "symlink"
        } else if t.is_file() {
            "file"
        } else if t.is_fifo() {
            "fifo"
        } else {
            "other"
        };
        out.push_str(&format!(
            "{p} {kind} {:o} {}:{} {}\n",
            m.mode() & 0o7777,
            m.uid(),
            m.gid(),
            m.size()
        ));
        if t.is_file() {
            match std::fs::read(p) {
                Ok(b) => out.push_str(&format!(
                    "= {}\n",
                    String::from_utf8_lossy(&b).replace('\n', "\\n")
                )),
                Err(e) => out.push_str(&format!("= unreadable {e}\n")),
            }
        } else if t.is_symlink()
            && let Ok(target) = std::fs::read_link(p)
        {
            out.push_str(&format!("-> {}\n", target.display()));
        }
    }
    let _ = io::stdout().write_all(out.as_bytes());
    code
}

/// Everything under `dir`, sorted by path, a line each: its kind, mode and owner, a
/// file's size and bytes (a hash of them past 64), a symlink's target, a device's numbers,
/// its `user.` xattrs, and, for a file of several names, the first of them.
fn tree(dir: &str) -> i32 {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let mut entries = Vec::new();
    let mut todo = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = todo.pop() {
        let listed = match std::fs::read_dir(&d) {
            Ok(listed) => listed,
            Err(e) => {
                let _ = writeln!(io::stderr(), "{}: {e}", d.display());
                return 1;
            }
        };
        for e in listed.flatten() {
            let p = e.path();
            match std::fs::symlink_metadata(&p) {
                Ok(m) => {
                    if m.is_dir() {
                        todo.push(p.clone());
                    }
                    entries.push((p, m));
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "{}: {e}", p.display());
                    return 1;
                }
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut first: std::collections::HashMap<(u64, u64), String> = std::collections::HashMap::new();
    let mut out = String::new();
    for (p, m) in &entries {
        let rel = p.strip_prefix(dir).unwrap_or(p).display().to_string();
        let t = m.file_type();
        let kind = if t.is_dir() {
            "dir"
        } else if t.is_symlink() {
            "symlink"
        } else if t.is_file() {
            "file"
        } else if t.is_fifo() {
            "fifo"
        } else if t.is_char_device() {
            "char"
        } else if t.is_block_device() {
            "block"
        } else {
            "socket"
        };
        out.push_str(&format!(
            "{rel} {kind} {:o} {}:{}",
            m.mode() & 0o7777,
            m.uid(),
            m.gid()
        ));
        if t.is_file() {
            let bytes = std::fs::read(p).unwrap_or_default();
            if bytes.len() <= 64 {
                out.push_str(&format!(" {} = {}", m.size(), String::from_utf8_lossy(&bytes)));
            } else {
                // FNV-1a, 64 bits.
                let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
                });
                out.push_str(&format!(" {} #{hash:016x}", m.size()));
            }
        } else if t.is_symlink() {
            let target = std::fs::read_link(p).unwrap_or_default();
            out.push_str(&format!(" -> {}", target.display()));
        } else if t.is_char_device() || t.is_block_device() {
            out.push_str(&format!(" {}:{}", libc::major(m.rdev()), libc::minor(m.rdev())));
        }
        if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) {
            let mut names = vec![0u8; 4096];
            // SAFETY: a NUL-terminated path, and a buffer of the length given.
            let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
            let mut user: Vec<String> = Vec::new();
            for name in names
                .get(..usize::try_from(n).unwrap_or(0))
                .unwrap_or_default()
                .split(|&b| b == 0)
            {
                if !name.starts_with(b"user.") {
                    continue;
                }
                let Ok(cn) = std::ffi::CString::new(name) else {
                    continue;
                };
                let mut value = vec![0u8; 4096];
                // SAFETY: NUL-terminated strings, and a buffer of the length given.
                let len = unsafe {
                    libc::lgetxattr(c.as_ptr(), cn.as_ptr(), value.as_mut_ptr().cast(), value.len())
                };
                value.truncate(usize::try_from(len).unwrap_or(0));
                user.push(format!(
                    "{}={}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&value)
                ));
            }
            user.sort();
            for x in user {
                out.push_str(&format!(" {x}"));
            }
        }
        if !t.is_dir() && m.nlink() > 1 {
            match first.entry((m.dev(), m.ino())) {
                std::collections::hash_map::Entry::Occupied(f) => {
                    out.push_str(&format!(" =link {}", f.get()))
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(rel.clone());
                }
            }
        }
        out.push('\n');
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

/// Cleans the data cache over every page of `path`, mapped read-only and never read, as a
/// JIT or a kernel making a page executable does: `DC CVAU` on arm64, `CLFLUSH` on x86_64.
/// From an image served with DAX, each page is the image's own, in memory the host maps.
/// Prints how many pages it cleaned.
fn clean_cache(path: &str) -> i32 {
    use std::os::fd::AsRawFd as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{path}: {e}");
            return 1;
        }
    };
    let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    // SAFETY: sysconf(3) has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    if len == 0 {
        let _ = writeln!(io::stderr(), "{path}: empty");
        return 1;
    }
    // SAFETY: a read-only shared mapping of a file held open, unmapped below.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        let _ = writeln!(io::stderr(), "mmap: {}", io::Error::last_os_error());
        return 1;
    }
    let mut cleaned = 0;
    for offset in (0..len).step_by(page) {
        let line = at.cast::<u8>().wrapping_add(offset);
        // SAFETY: an address in the mapping; cleaning the cache reads and writes no data.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("dc cvau, {0}", in(reg) line, options(nostack, preserves_flags));
        }
        // SAFETY: as above.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!("clflush [{0}]", in(reg) line, options(nostack, preserves_flags));
        }
        cleaned += 1;
    }
    // SAFETY: as above; the barrier completes the maintenance.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(at, len) };
    let _ = writeln!(io::stdout(), "cleaned {cleaned}");
    0
}

/// Prints who and where the workload is, one `key value` per line, and proves the root
/// filesystem is writable.
/// Prints each record of the kernel's log whose text holds any of `words`, read from
/// /dev/kmsg to its end without waiting (a record a read missed, overwritten, is
/// skipped: EPIPE).
fn kernlog(words: &[String]) -> i32 {
    use std::os::unix::fs::OpenOptionsExt as _;
    let Ok(log) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        let _ = writeln!(io::stdout(), "kernlog error: /dev/kmsg");
        return 1;
    };
    let mut log = log;
    let mut buf = vec![0u8; 8192];
    loop {
        match io::Read::read(&mut log, &mut buf) {
            Ok(0) => return 0,
            Ok(n) => {
                let record = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());
                let text = record
                    .split_once(';')
                    .map_or(record.as_ref(), |(_, t)| t)
                    .trim_end();
                if words.iter().any(|w| text.contains(w.as_str())) {
                    let _ = writeln!(io::stdout(), "{text}");
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {}
            Err(_) => return 0,
        }
    }
}

fn report() -> i32 {
    let mut out = String::new();
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut groups = [0 as libc::gid_t; 64];
    // SAFETY: getgroups(2) writes at most 64 entries.
    let n = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
    let groups: Vec<String> = groups
        .iter()
        .take(usize::try_from(n).unwrap_or(0))
        .map(u32::to_string)
        .collect();
    out.push_str(&format!("uid {uid}\ngid {gid}\ngroups {}\n", groups.join(",")));
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    out.push_str(&format!("cwd {cwd}\n"));
    // SAFETY: an all-zero utsname is a valid out-parameter.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::uname(&mut uts) };
    // SAFETY: uname(2) NUL-terminates nodename.
    let host = unsafe { std::ffi::CStr::from_ptr(uts.nodename.as_ptr()) }.to_string_lossy();
    out.push_str(&format!("hostname {host}\n"));
    out.push_str(&format!("pid {}\n", std::process::id()));
    // What its stdin is: Docker gives /dev/null without `-i` or a terminal.
    let stdin = std::fs::read_link("/proc/self/fd/0")
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let stdin = if stdin == "/dev/null" {
        "null"
    } else if stdin.starts_with("pipe:") {
        "pipe"
    } else if stdin.starts_with("/dev/pts/") {
        "tty"
    } else {
        "other"
    };
    out.push_str(&format!("stdin {stdin}\n"));
    // The kernel's own account of the process: umask, capabilities, seccomp.
    for line in std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
    {
        if let Some((k, v)) = line.split_once(':')
            && matches!(
                k,
                "Umask" | "CapInh" | "CapPrm" | "CapEff" | "CapBnd" | "CapAmb" | "NoNewPrivs" | "Seccomp"
            )
        {
            out.push_str(&format!("{} {}\n", k.to_lowercase(), v.trim()));
        }
    }
    // Its resource limits, as `ulimit` and `--ulimit` name them: soft and hard, -1 for
    // none.
    for (name, resource) in [
        ("nofile", libc::RLIMIT_NOFILE),
        ("nproc", libc::RLIMIT_NPROC),
        ("memlock", libc::RLIMIT_MEMLOCK),
        ("core", libc::RLIMIT_CORE),
        ("as", libc::RLIMIT_AS),
        ("rttime", libc::RLIMIT_RTTIME),
    ] {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) writing into a limit of ours.
        if unsafe { libc::getrlimit(resource, &mut limit) } == 0 {
            let shown = |v: libc::rlim_t| if v == libc::RLIM_INFINITY { -1 } else { v as i64 };
            out.push_str(&format!(
                "rlimit-{name} {}:{}\n",
                shown(limit.rlim_cur),
                shown(limit.rlim_max)
            ));
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    out.push_str(&format!("realtime {now}\n"));
    for (k, v) in std::env::vars() {
        out.push_str(&format!("env {k}={v}\n"));
    }
    let sigpipe = match SIGPIPE_AT_START.load(Ordering::Relaxed) {
        0 => "default",
        1 => "ignored",
        _ => "other",
    };
    out.push_str(&format!("sigpipe {sigpipe}\n"));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let blocked = status
        .lines()
        .find_map(|l| l.strip_prefix("SigBlk:"))
        .unwrap_or_default();
    out.push_str(&format!("sigblk {}\n", blocked.trim()));
    let existed = std::path::Path::new("/written").exists();
    out.push_str(&format!("existed {existed}\n"));
    let written =
        std::fs::write("/written", b"x").is_ok() && std::fs::read("/written").is_ok_and(|d| d == b"x");
    out.push_str(&format!("writable {written}\n"));
    // What a container's /dev holds, and its paths read-only and masked.
    let mut devs: Vec<String> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    devs.sort();
    out.push_str(&format!("devs {}\n", devs.join(",")));
    let sysctl = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/kernel/domainname")
        .is_ok();
    out.push_str(&format!("sysctl_writable {sysctl}\n"));
    let kcore = {
        use std::os::unix::fs::FileTypeExt as _;
        std::fs::metadata("/proc/kcore").is_ok_and(|m| m.file_type().is_char_device())
    };
    out.push_str(&format!("kcore_masked {kcore}\n"));
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for target in [
        "/",
        "/proc",
        "/sys",
        "/dev",
        "/dev/pts",
        "/dev/shm",
        "/dev/mqueue",
    ] {
        let fstype = mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split(' ');
                let (_, t, fs) = (f.next(), f.next(), f.next());
                (t == Some(target)).then_some(fs.unwrap_or_default())
            })
            .next_back()
            .unwrap_or("none");
        out.push_str(&format!("mount {target} {fstype}\n"));
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

/// Connects to `addr` (HOST:PORT, a name resolved as the C library resolves it), within
/// `within` milliseconds where given, says nothing, its side closed, and prints what comes
/// back until the far end closes: `ask BYTES`, or `ask error E`.
fn ask(addr: &str, within: Option<u64>) -> i32 {
    use std::io::Read as _;
    use std::net::ToSocketAddrs as _;
    let mut got = Vec::new();
    let connected = match within {
        None => std::net::TcpStream::connect(addr),
        Some(ms) => addr.to_socket_addrs().and_then(|mut all| {
            let to = all
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
            std::net::TcpStream::connect_timeout(&to, std::time::Duration::from_millis(ms))
        }),
    };
    let asked = connected.and_then(|mut s| {
        s.shutdown(std::net::Shutdown::Write)?;
        s.read_to_end(&mut got)
    });
    match asked {
        Ok(_) => {
            let _ = writeln!(io::stdout(), "ask {}", String::from_utf8_lossy(&got).trim_end());
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "ask error {e}");
            1
        }
    }
}

/// Every address the guest's resolver (musl's getaddrinfo: /etc/hosts, then the
/// resolv.conf's servers, A and AAAA) gives `name`, one a line, as `resolve ADDR`; or
/// `resolve error E`.
fn resolve(name: &str) -> i32 {
    use std::net::ToSocketAddrs as _;
    match (name, 0).to_socket_addrs() {
        Ok(addrs) => {
            for a in addrs {
                let _ = writeln!(io::stdout(), "resolve {}", a.ip());
            }
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "resolve error {e}");
            1
        }
    }
}

/// Asks the resolver `/etc/resolv.conf` names for `name`'s A records, by hand over UDP,
/// and prints each answer's TTL, how long a cache may keep it, and its address (`dnsttl
/// TTL ADDR`); the answer's code where it is not NOERROR (`dnsttl rcode N`); or why none
/// came.
fn dnsttl(name: &str) -> i32 {
    let server = resolver();
    let q = a_query(name);
    let asked = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        s.send_to(&q, (server.as_str(), 53))?;
        let mut buf = vec![0u8; 4096];
        let n = s.recv(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    });
    let a = match asked {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(io::stdout(), "dnsttl error {e}");
            return 1;
        }
    };
    let be16 = |at: usize| {
        a.get(at..at + 2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_be_bytes)
    };
    let be32 = |at: usize| {
        a.get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_be_bytes)
    };
    let rcode = a.get(3).map_or(0, |b| b & 0x0f);
    if rcode != 0 {
        let _ = writeln!(io::stdout(), "dnsttl rcode {rcode}");
        return 0;
    }
    // Its answers, past the question it repeats (RFC 1035 §4.1.3): each named by a pointer
    // or by labels to their zero, then its type, class, TTL, data's length and data.
    let mut at = q.len();
    for _ in 0..be16(6).unwrap_or(0) {
        while let Some(&len) = a.get(at) {
            if len & 0xc0 == 0xc0 {
                at += 2;
                break;
            }
            at += 1 + usize::from(len);
            if len == 0 {
                break;
            }
        }
        let (Some(kind), Some(ttl), Some(len)) = (be16(at), be32(at + 4), be16(at + 8)) else {
            break;
        };
        let data = a.get(at + 10..at + 10 + usize::from(len)).unwrap_or_default();
        if kind == 1
            && let Ok(ip) = <[u8; 4]>::try_from(data)
        {
            let _ = writeln!(io::stdout(), "dnsttl {ttl} {}", std::net::Ipv4Addr::from(ip));
        }
        at += 10 + usize::from(len);
    }
    0
}

/// The resolver `/etc/resolv.conf` names first, or none.
fn resolver() -> String {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("nameserver ").map(|a| a.trim().to_string()))
        .unwrap_or_default()
}

/// A query for `name`'s A records, recursion desired (RFC 1035 §4.1), its ID 0x537c.
fn a_query(name: &str) -> Vec<u8> {
    let mut q = vec![0x53, 0x7c, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(u8::try_from(label.len()).unwrap_or(0));
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

/// Asks the resolver `/etc/resolv.conf` names for `name`'s A records `n` times over UDP,
/// one at a time and a millisecond apart, as a program asking each time it connects does,
/// and prints how long each took to be answered, in microseconds, from the asker's side:
/// `dnsrtt n N p50 P p90 P p99 P max M`; or the first that went unanswered (3 s).
fn dnsrtt(name: &str, n: usize) -> i32 {
    let server = resolver();
    let mut q = a_query(name);
    let sock = match std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        s.connect((server.as_str(), 53))?;
        Ok(s)
    }) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io::stdout(), "dnsrtt error {e}");
            return 1;
        }
    };
    let mut took: Vec<u128> = Vec::with_capacity(n);
    let mut buf = vec![0u8; 4096];
    for i in 0..n {
        let id = u16::try_from(i % 65536).unwrap_or(0).to_be_bytes();
        if let Some(head) = q.get_mut(..2) {
            head.copy_from_slice(&id);
        }
        let at = std::time::Instant::now();
        let answered = sock.send(&q).and_then(|_| {
            loop {
                let got = sock.recv(&mut buf)?;
                if buf.get(..2) == Some(&id[..]) && got > 2 {
                    return Ok(());
                }
            }
        });
        if let Err(e) = answered {
            let _ = writeln!(io::stdout(), "dnsrtt error at {i}: {e}");
            return 1;
        }
        took.push(at.elapsed().as_micros());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    took.sort_unstable();
    let at = |p: usize| {
        took.get(took.len().saturating_sub(1) * p / 100)
            .copied()
            .unwrap_or(0)
    };
    let _ = writeln!(
        io::stdout(),
        "dnsrtt n {} p50 {} p90 {} p99 {} max {}",
        took.len(),
        at(50),
        at(90),
        at(99),
        at(100)
    );
    0
}

/// Connects to `addr` (IP:PORT), reads until the far end closes, and prints what came:
/// `tcp N BYTES` then the bytes.
fn tcp(addr: &str) -> i32 {
    use std::io::Read as _;
    let mut got = Vec::new();
    match std::net::TcpStream::connect(addr).and_then(|mut s| s.read_to_end(&mut got)) {
        Ok(n) => {
            let _ = writeln!(
                io::stdout(),
                "tcp {n} {}",
                String::from_utf8_lossy(&got).trim_end()
            );
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "tcp error {e}");
            1
        }
    }
}

/// Fetches each URL as curl does through the proxy its environment names (a build's,
/// D110): plain HTTP as an absolute URL to `HTTP_PROXY`; HTTPS through `CONNECT` to
/// `HTTPS_PROXY`, then TLS trusting the system's bundle alone
/// (`/etc/ssl/certs/ca-certificates.crt`); with no proxy named, or `--direct`, straight
/// to its host. Before a URL, `-X METHOD`, `-d BODY` and `-H 'Name: value'` shape its
/// request. Says `fetch STATUS BODY` of each, or `fetch error E`, and goes on. With
/// `--repeat N`, makes it N times, each on a connection of its own, and says how long each
/// took instead: `fetch-times n=N p50 P p90 P p99 P max M`, in microseconds.
/// One call of the Engine API at `DOCKER_HOST`, a `unix://` socket, its body JSON where
/// one is given: `api STATUS-LINE`, then its body, chunks put together and a stream's
/// frames (stdcopy) taken apart, stdout's bytes then stderr's ([`demuxed`]); `api error E`
/// where it could not ask.
fn api(method: &str, path: &str, body: &str, upgrade: bool) -> i32 {
    use std::os::unix::net::UnixStream;
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    let asked = host
        .strip_prefix("unix://")
        .ok_or_else(|| format!("DOCKER_HOST {host:?} names no socket"))
        .and_then(|socket| UnixStream::connect(socket).map_err(|e| format!("{socket}: {e}")))
        .and_then(|mut conn| {
            let connection = if upgrade {
                "Connection: Upgrade\r\nUpgrade: tcp"
            } else {
                "Connection: close"
            };
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: api\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{connection}\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
            let mut answer = Vec::new();
            conn.read_to_end(&mut answer).map_err(|e| e.to_string())?;
            Ok(answer)
        });
    let answer = match asked {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(io::stdout(), "api error {e}");
            return 1;
        }
    };
    let split = answer.windows(4).position(|w| w == b"\r\n\r\n");
    let (head, mut rest) = match split {
        Some(at) => (
            String::from_utf8_lossy(answer.get(..at).unwrap_or_default()).into_owned(),
            answer.get(at + 4..).unwrap_or_default().to_vec(),
        ),
        None => (String::from_utf8_lossy(&answer).into_owned(), Vec::new()),
    };
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_ascii_lowercase())
        })
    };
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        let mut whole = Vec::new();
        let mut at = rest.as_slice();
        while let Some(end) = at.windows(2).position(|w| w == b"\r\n") {
            let size = std::str::from_utf8(at.get(..end).unwrap_or_default())
                .ok()
                .and_then(|t| usize::from_str_radix(t.split(';').next().unwrap_or_default().trim(), 16).ok())
                .unwrap_or(0);
            if size == 0 {
                break;
            }
            whole.extend_from_slice(at.get(end + 2..end + 2 + size).unwrap_or_default());
            at = at.get(end + 2 + size + 2..).unwrap_or_default();
        }
        rest = whole;
    }
    if header("content-type").is_some_and(|v| v.contains("multiplexed-stream")) {
        rest = demuxed(&rest);
    }
    let _ = writeln!(io::stdout(), "api {}", head.lines().next().unwrap_or_default());
    let _ = io::stdout().write_all(&rest);
    let _ = io::stdout().flush();
    0
}

/// A multiplexed stream's bytes (stdcopy's frames), stdout's then stderr's: what a
/// command writes to the two reaches its engine down two pipes, in no order between them.
fn demuxed(streamed: &[u8]) -> Vec<u8> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut at = streamed;
    while let Some((frame, after)) = at.split_first_chunk::<8>() {
        let [kind, _, _, _, a, b, c, d] = *frame;
        let len = u32::from_be_bytes([a, b, c, d]) as usize;
        let bytes = after.get(..len).unwrap_or(after);
        if kind == 2 {
            err.extend_from_slice(bytes)
        } else {
            out.extend_from_slice(bytes)
        }
        at = after.get(len..).unwrap_or_default();
    }
    out.extend_from_slice(&err);
    out
}

/// An HTTP answer: its status line, its fields (names lower-cased) and its body.
type HttpAnswer = (String, Vec<(String, String)>, Vec<u8>);

/// An HTTP answer read whole from `conn`, its body by its length or its chunks; none where
/// the connection ended first.
fn read_answer(conn: &mut impl Read) -> Option<HttpAnswer> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let status = lines.next()?.to_string();
    let fields: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let field = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.clone());
    let mut body = Vec::new();
    if field("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        loop {
            let mut line = Vec::new();
            while !line.ends_with(b"\r\n") {
                if conn.read(&mut byte).ok()? == 0 {
                    return None;
                }
                line.push(byte[0]);
            }
            let size =
                usize::from_str_radix(String::from_utf8_lossy(&line).trim().split(';').next()?, 16).ok()?;
            let mut chunk = vec![0u8; size + 2];
            conn.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(chunk.get(..size)?);
        }
    } else if let Some(n) = field("content-length").and_then(|n| n.parse::<usize>().ok()) {
        body = vec![0u8; n];
        conn.read_exact(&mut body).ok()?;
    }
    Some((status, fields, body))
}

/// `docker run IMAGE CMD...` as the docker CLI makes its calls (docker/cli run.go): the
/// create, then the attach on a connection of its own, which the engine takes over for the
/// streams, then the start and the wait on the first, kept open between them.
fn api_run(image: &str, cmd: &[String]) -> i32 {
    use std::os::unix::net::UnixStream;
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    let Some(socket) = host.strip_prefix("unix://") else {
        let _ = writeln!(io::stdout(), "api-run error DOCKER_HOST {host:?}");
        return 1;
    };
    let fail = |what: &str| {
        let _ = writeln!(io::stdout(), "api-run error {what}");
        1
    };
    let ask = |conn: &mut UnixStream, method: &str, path: &str, body: &str, extra: &str| {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: api\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
            body.len()
        );
        conn.write_all(request.as_bytes()).is_ok()
    };
    let Ok(mut calls) = UnixStream::connect(socket) else {
        return fail("connecting");
    };
    let cmd: Vec<String> = cmd.iter().map(|a| format!("{a:?}")).collect();
    let body = format!(
        r#"{{"Image":"{image}","Cmd":[{}],"AttachStdout":true,"AttachStderr":true}}"#,
        cmd.join(",")
    );
    if !ask(&mut calls, "POST", "/containers/create", &body, "") {
        return fail("asking for the create");
    }
    let Some((status, _, said)) = read_answer(&mut calls) else {
        return fail("the create's answer");
    };
    let said = String::from_utf8_lossy(&said).into_owned();
    let Some(id) = said
        .split("\"Id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(str::to_string)
    else {
        return fail(&format!("the create: {status} {said}"));
    };
    let Ok(mut attached) = UnixStream::connect(socket) else {
        return fail("connecting for the attach");
    };
    let path = format!("/containers/{id}/attach?stream=1&stdout=1&stderr=1");
    if !ask(
        &mut attached,
        "POST",
        &path,
        "",
        "Connection: Upgrade\r\nUpgrade: tcp\r\n",
    ) {
        return fail("asking for the attach");
    }
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match attached.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return fail("the attach's answer"),
        }
    }
    let reader = std::thread::spawn(move || {
        let mut streamed = Vec::new();
        let _ = attached.read_to_end(&mut streamed);
        demuxed(&streamed)
    });
    if !ask(&mut calls, "POST", &format!("/containers/{id}/start"), "", "") {
        return fail("asking for the start");
    }
    match read_answer(&mut calls) {
        Some((status, _, _)) if status.contains(" 204 ") => {}
        other => return fail(&format!("the start: {other:?}")),
    }
    if !ask(&mut calls, "POST", &format!("/containers/{id}/wait"), "", "") {
        return fail("asking for the wait");
    }
    let Some((_, _, waited)) = read_answer(&mut calls) else {
        return fail("the wait's answer");
    };
    let waited = String::from_utf8_lossy(&waited).into_owned();
    let code = waited
        .split("\"StatusCode\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .unwrap_or("?")
        .to_string();
    let out = reader.join().unwrap_or_default();
    let _ = io::stdout().write_all(&out);
    let _ = writeln!(io::stdout(), "status {code}");
    0
}

fn fetch(args: &[String]) -> i32 {
    let mut method = "GET".to_string();
    let mut body = String::new();
    let mut headers: Vec<String> = Vec::new();
    let mut direct = false;
    let mut repeat = 0usize;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let next = args.get(i + 1).cloned().unwrap_or_default();
        match a.as_str() {
            "-X" => method = next,
            "-d" => body = next,
            "-H" => headers.push(next),
            "--repeat" => repeat = next.parse().unwrap_or(0),
            "--direct" => {
                direct = true;
                i += 1;
                continue;
            }
            url if repeat > 0 => {
                let mut took = Vec::with_capacity(repeat);
                let mut failed = None;
                for _ in 0..repeat {
                    let t = std::time::Instant::now();
                    if let Err(e) = fetch_one(&method, url, &headers, &body, direct) {
                        failed = Some(e);
                        break;
                    }
                    took.push(t.elapsed().as_micros());
                }
                took.sort_unstable();
                let q = |p: usize| {
                    took.get((took.len() * p / 100).min(took.len().saturating_sub(1)))
                        .copied()
                };
                match (failed, q(50), q(90), q(99), took.last()) {
                    (None, Some(a), Some(b), Some(c), Some(d)) => {
                        let _ = writeln!(
                            io::stdout(),
                            "fetch-times n={repeat} p50 {a} p90 {b} p99 {c} max {d}"
                        );
                    }
                    (e, ..) => {
                        let _ = writeln!(io::stdout(), "fetch error {}", e.unwrap_or_default());
                    }
                }
                (method, body, headers, direct, repeat) =
                    ("GET".to_string(), String::new(), Vec::new(), false, 0);
                i += 1;
                continue;
            }
            url => {
                match fetch_one(&method, url, &headers, &body, direct) {
                    Ok((status, text)) => {
                        let _ = writeln!(io::stdout(), "fetch {status} {}", text.trim_end());
                    }
                    Err(e) => {
                        let _ = writeln!(io::stdout(), "fetch error {e}");
                    }
                }
                (method, body, headers, direct) = ("GET".to_string(), String::new(), Vec::new(), false);
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    0
}

/// One request of [`fetch`]'s: its status and body.
fn fetch_one(
    method: &str,
    url: &str,
    headers: &[String],
    body: &str,
    direct: bool,
) -> Result<(u16, String), String> {
    use rustls::pki_types::pem::PemObject as _;
    use std::sync::Arc;
    let (scheme, rest) = url.split_once("://").ok_or("not a URL")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (rest.get(..i).unwrap_or_default(), rest.get(i..).unwrap_or("/")),
        None => (rest, "/"),
    };
    let default = if scheme == "https" { 443 } else { 80 };
    let target = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:{default}")
    };
    let proxy = ["HTTPS_PROXY", "https_proxy"]
        .iter()
        .filter(|_| scheme == "https")
        .chain(["HTTP_PROXY", "http_proxy"].iter().filter(|_| scheme == "http"))
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
        .filter(|_| !direct)
        .map(|p| p.trim_start_matches("http://").trim_end_matches('/').to_string());
    let mut head = String::new();
    let line = match (&proxy, scheme) {
        (Some(_), "http") => format!("{method} {url} HTTP/1.1\r\n"),
        _ => format!("{method} {path} HTTP/1.1\r\n"),
    };
    head.push_str(&line);
    head.push_str(&format!(
        "Host: {authority}\r\nUser-Agent: testguest\r\nConnection: close\r\n"
    ));
    for h in headers {
        head.push_str(h);
        head.push_str("\r\n");
    }
    if !body.is_empty() || method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    head.push_str(body);
    let e = |what: &str, x: &dyn std::fmt::Display| format!("{what}: {x}");
    let mut sock =
        std::net::TcpStream::connect(proxy.as_deref().unwrap_or(&target)).map_err(|x| e("connect", &x))?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .map_err(|x| e("timeout", &x))?;
    let mut got = Vec::new();
    if scheme == "https" {
        if proxy.is_some() {
            sock.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
                .map_err(|x| e("connect", &x))?;
            let mut answer = Vec::new();
            let mut byte = [0u8; 1];
            while !answer.ends_with(b"\r\n\r\n") {
                if sock.read(&mut byte).map_err(|x| e("connect", &x))? == 0 {
                    return Err("the proxy closed the tunnel".into());
                }
                answer.extend_from_slice(&byte);
            }
            let first = String::from_utf8_lossy(&answer)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            if !first.starts_with("HTTP/1.1 200") {
                return Err(format!("CONNECT: {first}"));
            }
        }
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::CertificateDer::pem_file_iter("/etc/ssl/certs/ca-certificates.crt")
            .map_err(|x| e("bundle", &x))?
        {
            let _ = roots.add(c.map_err(|x| e("bundle", &x))?);
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|x| e("tls", &x))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let host = authority.split(':').next().unwrap_or_default().to_string();
        let name = rustls::pki_types::ServerName::try_from(host).map_err(|x| e("name", &x))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|x| e("tls", &x))?;
        let mut tls = rustls::StreamOwned::new(conn, sock);
        tls.write_all(head.as_bytes()).map_err(|x| e("tls", &x))?;
        // A peer that ends without close_notify still said what it said.
        match tls.read_to_end(&mut got) {
            Ok(_) => {}
            Err(x) if x.kind() == io::ErrorKind::UnexpectedEof && !got.is_empty() => {}
            Err(x) => return Err(e("tls", &x)),
        }
    } else {
        sock.write_all(head.as_bytes()).map_err(|x| e("write", &x))?;
        sock.read_to_end(&mut got).map_err(|x| e("read", &x))?;
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    let (response_head, rest) = text.split_once("\r\n\r\n").ok_or("no response head")?;
    let status: u16 = response_head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("no status")?;
    let chunked = response_head
        .lines()
        .any(|l| l.to_ascii_lowercase().replace(' ', "") == "transfer-encoding:chunked");
    if !chunked {
        return Ok((status, rest.to_string()));
    }
    // Its chunks, joined.
    let mut out = String::new();
    let mut rest = rest;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).map_err(|x| e("chunk", &x))?;
        if n == 0 {
            break;
        }
        out.push_str(after.get(..n).ok_or("a chunk cut short")?);
        rest = after.get(n + 2..).unwrap_or_default();
    }
    Ok((status, out))
}

/// Sends a datagram to `addr` from a connected socket, and waits up to 5 s for an answer:
/// `udp N` its length, or `udp error E`, as an ICMP error or the wait ends it.
fn udp(addr: &str) -> i32 {
    // A socket of the address's version.
    let any = if addr.starts_with('[') {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let answered = std::net::UdpSocket::bind(any).and_then(|s| {
        s.connect(addr)?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        s.send(b"?")?;
        let mut buf = [0u8; 512];
        s.recv(&mut buf)
    });
    match answered {
        Ok(n) => {
            let _ = writeln!(io::stdout(), "udp {n}");
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp error {e}");
            1
        }
    }
}

/// Listens on TCP `port` at every address, says `ready`, then serves `connections`
/// connections in turn: to each, `from IP\n` (its peer's address), then all it sends
/// back, until it closes its side.
/// Dials the host's vsock `port` as a builder's agent relay does, with a token it was never
/// given, and asks for the agent's keys: `vsock-agent refused` if the host closes it
/// unanswered, `vsock-agent answered` if it answers, `vsock-agent dial ERROR` if it cannot
/// dial.
fn vsock_agent(port: u32) -> i32 {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        let _ = writeln!(io::stdout(), "vsock-agent dial {}", io::Error::last_os_error());
        return 1;
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let mut sock = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    // SAFETY: connect(2) with an address of its own type and size.
    if unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    } != 0
    {
        let _ = writeln!(io::stdout(), "vsock-agent dial {}", io::Error::last_os_error());
        return 1;
    }
    let id = b"default";
    let mut hello = vec![0u8; 16];
    hello.extend_from_slice(&(id.len() as u16).to_be_bytes());
    hello.extend_from_slice(id);
    hello.extend_from_slice(&[0, 0, 0, 1, 11]);
    let _ = sock.write_all(&hello);
    let mut answer = [0u8; 1];
    let said = match sock.read(&mut answer) {
        Ok(0) | Err(_) => "refused",
        Ok(_) => "answered",
    };
    let _ = writeln!(io::stdout(), "vsock-agent {said}");
    0
}

/// Speaks to the SSH agent at `SSH_AUTH_SOCK`: lists its keys (`agent keys N COMMENT...`),
/// signs with the first (`agent signed` or `agent sign refused`), and asks it to forget
/// them all (`agent remove refused` or `agent removed`).
fn agent() -> i32 {
    use std::io::Read as _;
    let Some(path) = std::env::var_os("SSH_AUTH_SOCK") else {
        let _ = writeln!(io::stdout(), "agent no SSH_AUTH_SOCK");
        return 1;
    };
    let mut c = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io::stdout(), "agent connect {e}");
            return 1;
        }
    };
    let mut ask = |body: &[u8]| -> io::Result<Vec<u8>> {
        c.write_all(&(body.len() as u32).to_be_bytes())?;
        c.write_all(body)?;
        let mut n = [0u8; 4];
        c.read_exact(&mut n)?;
        let mut answer = vec![0u8; u32::from_be_bytes(n) as usize];
        c.read_exact(&mut answer)?;
        Ok(answer)
    };
    let take = |b: &[u8], at: &mut usize| -> Option<Vec<u8>> {
        let n = u32::from_be_bytes(b.get(*at..*at + 4)?.try_into().ok()?) as usize;
        let v = b.get(*at + 4..*at + 4 + n)?.to_vec();
        *at += 4 + n;
        Some(v)
    };
    let Ok(list) = ask(&[11]) else { return 1 };
    let mut out = String::new();
    let mut first_key = None;
    if list.first() == Some(&12) {
        let n = u32::from_be_bytes(list.get(1..5).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]));
        let mut at = 5;
        let mut comments = Vec::new();
        for _ in 0..n {
            let (Some(key), Some(comment)) = (take(&list, &mut at), take(&list, &mut at)) else {
                break;
            };
            first_key.get_or_insert(key);
            comments.push(String::from_utf8_lossy(&comment).into_owned());
        }
        out.push_str(&format!("agent keys {n} {}\n", comments.join(" ")));
    }
    if let Some(key) = first_key {
        let mut body = vec![13];
        body.extend_from_slice(&(key.len() as u32).to_be_bytes());
        body.extend_from_slice(&key);
        body.extend_from_slice(&5u32.to_be_bytes());
        body.extend_from_slice(b"hello");
        body.extend_from_slice(&0u32.to_be_bytes());
        match ask(&body) {
            Ok(a) if a.first() == Some(&14) => out.push_str("agent signed\n"),
            _ => out.push_str("agent sign refused\n"),
        }
    }
    match ask(&[19]) {
        Ok(a) if a.first() == Some(&5) => out.push_str("agent remove refused\n"),
        _ => out.push_str("agent removed\n"),
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

fn serve(port: &str, connections: usize) -> i32 {
    // Both versions on one socket where the guest has IPv6 (a v6 socket takes v4 as
    // mapped addresses, ipv6(7)), IPv4 alone where it has not.
    let listener = match std::net::TcpListener::bind(format!("[::]:{port}"))
        .or_else(|_| std::net::TcpListener::bind(format!("0.0.0.0:{port}")))
    {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io::stdout(), "serve error {e}");
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    for _ in 0..connections {
        let served = listener.accept().and_then(|(mut c, peer)| {
            writeln!(c, "from {}", peer.ip().to_canonical())?;
            let mut read = c.try_clone()?;
            let echoed = io::copy(&mut read, &mut c)?;
            // What it had of the connection, for a test to tell a stream cut on its way in
            // from one cut on its way back.
            let _ = writeln!(io::stdout(), "served {echoed}");
            c.shutdown(std::net::Shutdown::Write)
        });
        if let Err(e) = served {
            let _ = writeln!(io::stdout(), "serve error {e}");
            return 1;
        }
    }
    0
}

/// Listens on TCP at exactly `addr` (port 0 for one of the kernel's choosing), says
/// `ready PORT`, then serves `connections` connections as [`serve`] does; `listen error E`
/// where it cannot bind.
fn listen(addr: &str, connections: usize) -> i32 {
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io::stdout(), "listen error {e}");
            return 1;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    let _ = writeln!(io::stdout(), "ready {port}");
    for _ in 0..connections {
        let served = listener.accept().and_then(|(mut c, peer)| {
            writeln!(c, "from {}", peer.ip().to_canonical())?;
            let mut read = c.try_clone()?;
            io::copy(&mut read, &mut c)?;
            c.shutdown(std::net::Shutdown::Write)
        });
        if let Err(e) = served {
            let _ = writeln!(io::stdout(), "listen error {e}");
            return 1;
        }
    }
    0
}

/// Listens on TCP at `addr`, then `times` times closes the listener and at once listens
/// on the same address and port again, as a server restarting does: `rebound PORT`, or
/// `rebind error E` at the first that fails.
fn rebind(addr: &str, times: usize) -> i32 {
    let rebound = std::net::TcpListener::bind(addr).and_then(|mut l| {
        let at = l.local_addr()?;
        for _ in 0..times {
            drop(l);
            l = std::net::TcpListener::bind(at)?;
        }
        Ok(at)
    });
    match rebound {
        Ok(at) => {
            let _ = writeln!(io::stdout(), "rebound {}", at.port());
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "rebind error {e}");
            1
        }
    }
}

/// Times `n` TCP listeners at every IPv4 address, each at a port of the kernel's choosing
/// (bind(2) and listen(2), as std's `TcpListener::bind`), then `n` UDP sockets bound there
/// so (bind(2) alone, as a resolver's query socket is), each closed before the next: a
/// line of each, `binds tcp|udp n=N p50 P p90 P p99 P max M`, in microseconds.
fn binds(n: usize) -> i32 {
    let mut code = 0;
    for kind in ["tcp", "udp"] {
        let mut took = Vec::with_capacity(n);
        for _ in 0..n {
            let t0 = std::time::Instant::now();
            let made = if kind == "tcp" {
                std::net::TcpListener::bind("0.0.0.0:0").map(drop)
            } else {
                std::net::UdpSocket::bind("0.0.0.0:0").map(drop)
            };
            match made {
                Ok(()) => took.push(t0.elapsed().as_micros()),
                Err(e) => {
                    let _ = writeln!(io::stdout(), "binds {kind} error {e}");
                    code = 1;
                    break;
                }
            }
        }
        took.sort_unstable();
        let at = |q: usize| {
            took.get((took.len() * q / 100).min(took.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0)
        };
        let _ = writeln!(
            io::stdout(),
            "binds {kind} n={} p50 {} p90 {} p99 {} max {}",
            took.len(),
            at(50),
            at(90),
            at(99),
            took.last().copied().unwrap_or(0)
        );
    }
    code
}

/// Sends `count` datagrams of `size` bytes to `addr` as fast as it can, then lingers
/// `linger_ms` and exits: `sent N`, or `udp-send error E`. Exiting at once, what it sent
/// is the guest's to send still as its command ends (PM M182).
fn udp_send(addr: &str, count: usize, size: usize, linger_ms: u64) -> i32 {
    let payload = vec![7u8; size];
    let sent = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|sock| {
        let mut sent = 0;
        for _ in 0..count {
            sock.send_to(&payload, addr)?;
            sent += 1;
        }
        Ok(sent)
    });
    match sent {
        Ok(n) => {
            let _ = writeln!(io::stdout(), "sent {n}");
            std::thread::sleep(std::time::Duration::from_millis(linger_ms));
            let _ = writeln!(io::stdout(), "{}", net_counters());
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp-send error {e}");
            1
        }
    }
}

/// The guest's UDP counters (/proc/net/snmp's `Udp:`) and eth0's packets and drops
/// (/sys/class/net/eth0/statistics), on one line: where a datagram went.
fn net_counters() -> String {
    let snmp = std::fs::read_to_string("/proc/net/snmp").unwrap_or_default();
    let mut udp = snmp.lines().filter(|l| l.starts_with("Udp:"));
    let (names, values) = (udp.next().unwrap_or_default(), udp.next().unwrap_or_default());
    let mut out: Vec<String> = names
        .split_whitespace()
        .zip(values.split_whitespace())
        .skip(1)
        .map(|(n, v)| format!("{n}={v}"))
        .collect();
    for stat in ["rx_packets", "rx_dropped", "tx_packets", "tx_dropped"] {
        let v = std::fs::read_to_string(format!("/sys/class/net/eth0/statistics/{stat}")).unwrap_or_default();
        out.push(format!("{stat}={}", v.trim()));
    }
    out.join(" ")
}

/// Binds UDP at `addr`, says `ready PORT`, then counts the datagrams that come until
/// `quiet_ms` pass without one after the first, or 10 s without a first: `got N`.
fn udp_count(addr: &str, quiet_ms: u64) -> i32 {
    let counted = std::net::UdpSocket::bind(addr).and_then(|sock| {
        let _ = writeln!(io::stdout(), "ready {}", sock.local_addr()?.port());
        let mut buf = vec![0u8; 65536];
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        if sock.recv(&mut buf).is_err() {
            return Ok(0);
        }
        sock.set_read_timeout(Some(std::time::Duration::from_millis(quiet_ms.max(1))))?;
        let mut got = 1u64;
        while sock.recv(&mut buf).is_ok() {
            got += 1;
        }
        Ok(got)
    });
    match counted {
        Ok(n) => {
            let _ = writeln!(io::stdout(), "got {n}");
            let _ = writeln!(io::stdout(), "{}", net_counters());
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp-count error {e}");
            1
        }
    }
}

/// Binds UDP at exactly `addr`, says `ready PORT`, then answers one datagram with
/// `from PEER ` and the datagram: `udp-serve error E` where it cannot.
fn udp_serve(addr: &str, forget: bool) -> i32 {
    let served = forget_neighbors(forget)
        .and_then(|()| std::net::UdpSocket::bind(addr))
        .and_then(|sock| {
            let _ = writeln!(io::stdout(), "ready {}", sock.local_addr()?.port());
            let mut buf = [0u8; 2048];
            let (n, peer) = sock.recv_from(&mut buf)?;
            let mut answer = format!("from {} ", peer.ip().to_canonical()).into_bytes();
            answer.extend_from_slice(buf.get(..n).unwrap_or_default());
            sock.send_to(&answer, peer)
        });
    match served {
        Ok(_) => 0,
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp-serve error {e}");
            1
        }
    }
}

/// Listens on TCP `port` at every address, says `ready`, then holds every connection it
/// is given, echoing what each sends, until it is killed: many idle connections beside
/// the few that talk. On epoll(7), so that the connections held cost it nothing an
/// event; its descriptor limit raised to the hard one first.
fn hold(port: &str) -> i32 {
    use std::collections::HashMap;
    use std::os::fd::AsRawFd as _;
    raise_descriptor_limit();
    let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
    {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io::stdout(), "hold error {e}");
            return 1;
        }
    };
    // SAFETY: epoll_create1(2) with a flag.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let watch = |fd: i32, token: u64| {
        let mut e = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: token,
        };
        // SAFETY: epoll_ctl(2) adding a descriptor we hold.
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut e) == 0 }
    };
    if ep < 0 || !watch(listener.as_raw_fd(), u64::MAX) {
        let _ = writeln!(io::stdout(), "hold error {}", io::Error::last_os_error());
        return 1;
    }
    let _ = writeln!(io::stdout(), "ready");
    let mut held: HashMap<u64, std::net::TcpStream> = HashMap::new();
    let mut next = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    // SAFETY: epoll_event is plain data, for which all zeroes is a value.
    let mut events: [libc::epoll_event; 64] = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: epoll_wait(2) into a buffer of 64 events.
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, -1) };
        for e in events.iter().take(usize::try_from(n).unwrap_or(0)) {
            let token = e.u64;
            if token == u64::MAX {
                while let Ok((c, _)) = listener.accept() {
                    if c.set_nonblocking(true).is_ok() && watch(c.as_raw_fd(), next) {
                        held.insert(next, c);
                        next += 1;
                    }
                }
                continue;
            }
            let Some(c) = held.get_mut(&token) else { continue };
            loop {
                match c.read(&mut buf) {
                    Ok(0) => {
                        held.remove(&token);
                        break;
                    }
                    Ok(got) => {
                        let _ = c.set_nonblocking(false);
                        let _ = c.write_all(buf.get(..got).unwrap_or_default());
                        let _ = c.set_nonblocking(true);
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

/// Raises this process's soft limit on open descriptors to its hard limit.
fn raise_descriptor_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) and setrlimit(2) of our own limit, through a local.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            lim.rlim_cur = lim.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

/// Holds `n` connections to `addr` (IP:PORT) open while the host measures what they cost
/// its network process (benches/net_memory.rs): each says `i` and is answered; or, with
/// `bytes` above 0, says `b` and that length (8 bytes, big-endian), sends that many bytes
/// and reads as many back, then idles. One more says `t`, makes `trips` round trips of 64
/// bytes, and says `H`; every connection is held until the far end closes that one. Then
/// prints each round trip's time, `conns rtt-us T...`, and `conns n=N bytes=B rtt
/// n=TRIPS p50 P p90 P p99 P max M`, in microseconds.
fn conns(addr: &str, n: usize, trips: usize, bytes: u64) -> i32 {
    use std::io::Read as _;
    raise_descriptor_limit();
    let failed = |what: &str, e: io::Error| {
        let _ = writeln!(io::stdout(), "conns error {what}: {e}");
        1
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut held = Vec::with_capacity(n);
    // Each connect's time, the guest's SYN to its SYN-ACK taken.
    let mut connects: Vec<u128> = Vec::with_capacity(n);
    for _ in 0..n {
        let t = std::time::Instant::now();
        let opened = std::net::TcpStream::connect(addr).and_then(|mut c| {
            connects.push(t.elapsed().as_micros());
            if bytes == 0 {
                c.write_all(b"i")?;
                c.read_exact(buf.get_mut(..1).unwrap_or_default())?;
                return Ok(c);
            }
            let mut head = vec![b'b'];
            head.extend(bytes.to_be_bytes());
            c.write_all(&head)?;
            // All of it sent, then all of it back: the far end reads it all first.
            for sending in [true, false] {
                let mut left = bytes;
                while left > 0 {
                    let m = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
                    let part = buf.get_mut(..m).unwrap_or_default();
                    if sending {
                        c.write_all(part)?;
                    } else {
                        c.read_exact(part)?;
                    }
                    left -= m as u64;
                }
            }
            Ok(c)
        });
        match opened {
            Ok(c) => held.push(c),
            Err(e) => return failed(&format!("connection {}", held.len()), e),
        }
    }
    let talk = std::net::TcpStream::connect(addr).and_then(|mut c| {
        c.set_nodelay(true)?;
        c.write_all(b"t")?;
        c.read_exact(buf.get_mut(..1).unwrap_or_default())?;
        Ok(c)
    });
    let mut talk = match talk {
        Ok(c) => c,
        Err(e) => return failed("the talking connection", e),
    };
    let (out, mut back) = ([b'x'; 64], [0u8; 64]);
    let mut took = Vec::with_capacity(trips);
    for _ in 0..trips {
        let t = std::time::Instant::now();
        if let Err(e) = talk.write_all(&out).and_then(|()| talk.read_exact(&mut back)) {
            return failed("a round trip", e);
        }
        took.push(t.elapsed().as_micros());
    }
    if let Err(e) = talk.write_all(b"H") {
        return failed("saying it holds them", e);
    }
    loop {
        match talk.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return failed("holding them", e),
        }
    }
    drop(held);
    // A thousand to a line, which keeps each short where there are many.
    for some in connects.chunks(1000) {
        let each: Vec<String> = some.iter().map(u128::to_string).collect();
        let _ = writeln!(io::stdout(), "conns connect-us {}", each.join(" "));
    }
    let each: Vec<String> = took.iter().map(u128::to_string).collect();
    let _ = writeln!(io::stdout(), "conns rtt-us {}", each.join(" "));
    took.sort_unstable();
    let q = |p: usize| {
        took.get((took.len() * p / 100).min(took.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    let _ = writeln!(
        io::stdout(),
        "conns n={n} bytes={bytes} rtt n={trips} p50 {} p90 {} p99 {} max {}",
        q(50),
        q(90),
        q(99),
        took.last().copied().unwrap_or(0)
    );
    0
}

/// Binds UDP `port` at every address, says `ready`, then answers each datagram with
/// `from IP:PORT ` (its sender) and what it held, saying `datagram from IP:PORT LEN` for
/// each, until one holds `stop`: a published port on a shared host may be sent anything,
/// so no count of datagrams ends it.
fn udp_echo(port: &str, stop: &str) -> i32 {
    let sock = match std::net::UdpSocket::bind(format!("0.0.0.0:{port}")) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp error {e}");
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    let mut buf = [0u8; 2048];
    loop {
        let answered = sock.recv_from(&mut buf).and_then(|(n, peer)| {
            let _ = writeln!(io::stdout(), "datagram from {peer} {n}");
            let held = buf.get(..n).unwrap_or_default();
            let mut answer = format!("from {peer} ").into_bytes();
            answer.extend_from_slice(held);
            sock.send_to(&answer, peer).map(|_| held == stop.as_bytes())
        });
        match answered {
            Ok(true) => {
                let _ = writeln!(io::stdout(), "stopped");
                return 0;
            }
            Ok(false) => {}
            Err(e) => {
                let _ = writeln!(io::stdout(), "udp error {e}");
                return 1;
            }
        }
    }
}

/// Runs `/bin/testguest ARGS` as a child and waits for it: `spawned PID code N`, a fork,
/// an exec and an exit for a record of processes to hold (D123).
fn spawn(args: &[String]) -> i32 {
    match std::process::Command::new("/bin/testguest").args(args).spawn() {
        Ok(mut child) => {
            let pid = child.id();
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            let _ = writeln!(io::stdout(), "spawned {pid} code {code}");
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "spawn error {e}");
            1
        }
    }
}

/// Runs `/bin/testguest ARGS I` as a child `n` times, one after another, I its round, each
/// waited for: as many forks, execs and exits, each process over at once and reaped by its
/// parent, for a record of processes to hold whole (D123); `spawned N` once all have.
fn spawns(n: u32, args: &[String]) -> i32 {
    for i in 0..n {
        if let Err(e) = std::process::Command::new("/bin/testguest")
            .args(args)
            .arg(i.to_string())
            .status()
        {
            let _ = writeln!(io::stdout(), "spawn error {e}");
            return 1;
        }
    }
    let _ = writeln!(io::stdout(), "spawned {n}");
    0
}

/// Every address of every interface, one a line, `NAME ADDR/PREFIX`, as getifaddrs(3)
/// lists them: what `ip addr` would show of each.
/// eth0's neighbors, IPv4's and IPv6's: one line each, `neighbor ADDR MAC STATE`, the
/// state as `ip neigh` names it, `permanent` for one that never changes.
fn neighbors() -> i32 {
    match Rtnl::open().and_then(|r| r.neighbors()) {
        Ok(list) => {
            for (addr, mac, state) in list {
                let _ = writeln!(io::stdout(), "neighbor {addr} {mac} {state}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "neighbors error {e}");
            1
        }
    }
}

/// Where `forget`, every neighbor of eth0's deleted (CAP_NET_ADMIN): what the guest's next
/// datagram to one waits on its answer for, as a guest that never knew its gateway's MAC
/// waited (PM M180).
fn forget_neighbors(forget: bool) -> io::Result<()> {
    if !forget {
        return Ok(());
    }
    let rtnl = Rtnl::open()?;
    for (addr, _, _) in rtnl.neighbors()? {
        let mut n = Vec::new();
        let (family, octets) = match addr {
            std::net::IpAddr::V4(a) => (libc::AF_INET, a.octets().to_vec()),
            std::net::IpAddr::V6(a) => (libc::AF_INET6, a.octets().to_vec()),
        };
        n.extend_from_slice(&[family as u8, 0, 0, 0]);
        n.extend_from_slice(&rtnl.eth0.to_ne_bytes());
        n.extend_from_slice(&[0; 4]);
        n.extend_from_slice(&u16::try_from(4 + octets.len()).unwrap_or(0).to_ne_bytes());
        n.extend_from_slice(&1u16.to_ne_bytes());
        n.extend_from_slice(&octets);
        rtnl.send(libc::RTM_DELNEIGH, libc::NLM_F_ACK as u16, &n)?;
        rtnl.each(|kind, msg| {
            if kind == libc::NLMSG_ERROR as u16 {
                return match msg
                    .get(..4)
                    .and_then(|e| <[u8; 4]>::try_from(e).ok())
                    .map(i32::from_ne_bytes)
                {
                    Some(0) => Some(Ok(())),
                    Some(e) => Some(Err(io::Error::from_raw_os_error(-e))),
                    None => Some(Err(io::Error::other("a short answer"))),
                };
            }
            None
        })?;
    }
    Ok(())
}

/// An rtnetlink socket, and eth0's index.
struct Rtnl {
    sock: std::os::fd::OwnedFd,
    eth0: u32,
}

impl Rtnl {
    fn open() -> io::Result<Rtnl> {
        use std::os::fd::FromRawFd as _;
        // SAFETY: if_nametoindex(3) with a NUL-terminated name.
        let eth0 = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, owned here alone.
        let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Ok(Rtnl { sock, eth0 })
    }

    /// Sends a request of `kind` with `body` (struct nlmsghdr: length, type, flags,
    /// sequence, port).
    fn send(&self, kind: u16, flags: u16, body: &[u8]) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let mut req = Vec::new();
        req.extend_from_slice(&u32::try_from(16 + body.len()).unwrap_or(0).to_ne_bytes());
        req.extend_from_slice(&kind.to_ne_bytes());
        req.extend_from_slice(&(libc::NLM_F_REQUEST as u16 | flags).to_ne_bytes());
        req.extend_from_slice(&[0; 8]);
        req.extend_from_slice(body);
        // SAFETY: send(2) of a buffer of ours, of its length.
        if unsafe { libc::send(self.sock.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Each message answered, its type and body, to `f` until it says what they come to,
    /// or a dump's end does.
    fn each(&self, mut f: impl FnMut(u16, &[u8]) -> Option<io::Result<()>>) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let mut buf = vec![0u8; 65536];
        loop {
            // SAFETY: recv(2) into a buffer of ours, of its length.
            let n = unsafe { libc::recv(self.sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            let Some(got) = usize::try_from(n).ok().and_then(|n| buf.get(..n)) else {
                return Err(io::Error::last_os_error());
            };
            let mut at = 0usize;
            while let (Some(len), Some(kind)) = (u32_at(got, at), u16_at(got, at.saturating_add(4))) {
                let len = len as usize;
                if len < 16 {
                    return Err(io::Error::other("a malformed message"));
                }
                let msg = got
                    .get(at.saturating_add(16)..at.saturating_add(len))
                    .unwrap_or_default();
                at = at.saturating_add(len.saturating_add(3) & !3);
                if kind == libc::NLMSG_DONE as u16 {
                    return Ok(());
                }
                if let Some(said) = f(kind, msg) {
                    return said;
                }
            }
        }
    }

    /// eth0's neighbors from rtnetlink's dump (RTM_GETNEIGH): each address, MAC and state.
    fn neighbors(&self) -> io::Result<Vec<(std::net::IpAddr, String, &'static str)>> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        // struct ndmsg of no family: every neighbor.
        self.send(libc::RTM_GETNEIGH, libc::NLM_F_DUMP as u16, &[0; 12])?;
        let mut list = Vec::new();
        self.each(|kind, msg| {
            if kind == libc::NLMSG_ERROR as u16 {
                return Some(Err(io::Error::other("the dump failed")));
            }
            let (Some(index), Some(state)) = (u32_at(msg, 4), u16_at(msg, 8)) else {
                return None;
            };
            if kind != libc::RTM_NEWNEIGH || index != self.eth0 {
                return None;
            }
            // Its attributes: NDA_DST (1), NDA_LLADDR (2).
            let (mut addr, mut mac) = (None, String::from("-"));
            let mut a = 12usize;
            while let (Some(alen), Some(akind)) = (u16_at(msg, a), u16_at(msg, a.saturating_add(2))) {
                let alen = usize::from(alen);
                if alen < 4 {
                    break;
                }
                let value = msg
                    .get(a.saturating_add(4)..a.saturating_add(alen))
                    .unwrap_or_default();
                match akind {
                    1 => {
                        addr = <[u8; 4]>::try_from(value)
                            .map(|o| IpAddr::V4(Ipv4Addr::from(o)))
                            .or_else(|_| <[u8; 16]>::try_from(value).map(|o| IpAddr::V6(Ipv6Addr::from(o))))
                            .ok();
                    }
                    2 => {
                        mac = value
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(":")
                    }
                    _ => {}
                }
                a = a.saturating_add(alen.saturating_add(3) & !3);
            }
            let state = match state {
                0x01 => "incomplete",
                0x02 => "reachable",
                0x04 => "stale",
                0x08 => "delay",
                0x10 => "probe",
                0x20 => "failed",
                0x40 => "noarp",
                0x80 => "permanent",
                _ => "none",
            };
            if let Some(addr) = addr {
                list.push((addr, mac, state));
            }
            None
        })?;
        Ok(list)
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    b.get(i..i.saturating_add(2))
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map(u16::from_ne_bytes)
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i.saturating_add(4))
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_ne_bytes)
}

fn addrs() -> i32 {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs(3) into a list it allocates, freed below.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        let _ = writeln!(io::stderr(), "getifaddrs: {}", io::Error::last_os_error());
        return 1;
    }
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of the list getifaddrs made, read before it is freed.
        let ifa = unsafe { &*at };
        at = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: the name getifaddrs gives each node, NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        // SAFETY: a sockaddr getifaddrs made; its family says which one it is.
        let (addr, prefix) = match i32::from(unsafe { (*ifa.ifa_addr).sa_family }) {
            libc::AF_INET => {
                // SAFETY: AF_INET's address and mask are sockaddr_in.
                let (a, m) = unsafe {
                    (
                        &*ifa.ifa_addr.cast::<libc::sockaddr_in>(),
                        &*ifa.ifa_netmask.cast::<libc::sockaddr_in>(),
                    )
                };
                (
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr))),
                    m.sin_addr.s_addr.count_ones(),
                )
            }
            libc::AF_INET6 => {
                // SAFETY: AF_INET6's address and mask are sockaddr_in6.
                let (a, m) = unsafe {
                    (
                        &*ifa.ifa_addr.cast::<libc::sockaddr_in6>(),
                        &*ifa.ifa_netmask.cast::<libc::sockaddr_in6>(),
                    )
                };
                (
                    IpAddr::V6(Ipv6Addr::from(a.sin6_addr.s6_addr)),
                    m.sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum(),
                )
            }
            _ => continue,
        };
        let _ = writeln!(io::stdout(), "{name} {addr}/{prefix}");
    }
    // SAFETY: the list getifaddrs made, freed once.
    unsafe { libc::freeifaddrs(list) };
    0
}

/// File operations, in order: `mkdir:P`, `write:P=DATA`, `link:OLD:NEW`, `symlink:T:P`,
/// `rm:P`, `rmdir:P` (and what it holds), `chmod:OCTAL:P`, `chown:UID:GID:P` (not
/// followed), `mknod:b|c:MAJOR:MINOR:P`, `open:r|w:P`, `dev:P`, which prints a device
/// node's type, numbers and mode, `ino:P`, which prints P's device and inode numbers (not
/// followed), `print:P`, which prints a file, `readn:N:P`, which reads
/// N bytes of P, and `sleep:MS`; and the mount family, each its syscall alone:
/// `mount:FSTYPE:SOURCE:TARGET`, `umount:TARGET`, `remount-rw:TARGET` (a bind's flags
/// changed), `fsopen:FSTYPE` and `open-tree:P` (a detached clone). Stops at the first that
/// fails, saying which.
fn fs(ops: &[String]) -> i32 {
    use std::os::unix::fs::PermissionsExt as _;
    for op in ops {
        let (kind, rest) = op.split_once(':').unwrap_or((op.as_str(), ""));
        let done = match kind {
            "mkdir" => std::fs::create_dir(rest),
            "write" => {
                let (p, data) = rest.split_once('=').unwrap_or((rest, ""));
                std::fs::write(p, data)
            }
            "link" => {
                let (a, b) = rest.split_once(':').unwrap_or((rest, ""));
                std::fs::hard_link(a, b)
            }
            "symlink" => {
                let (t, p) = rest.split_once(':').unwrap_or((rest, ""));
                std::os::unix::fs::symlink(t, p)
            }
            "rm" => std::fs::remove_file(rest),
            "rename" => {
                let (a, b) = rest.split_once(':').unwrap_or((rest, ""));
                std::fs::rename(a, b)
            }
            "rmdir" => std::fs::remove_dir_all(rest),
            "chmod" => {
                let (m, p) = rest.split_once(':').unwrap_or((rest, ""));
                u32::from_str_radix(m, 8)
                    .map_err(io::Error::other)
                    .and_then(|m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)))
            }
            "mknod" => mknod(rest),
            "ino" => std::fs::symlink_metadata(rest).map(|m| {
                use std::os::unix::fs::MetadataExt as _;
                let _ = writeln!(io::stdout(), "{rest} {}:{}", m.dev(), m.ino());
            }),
            "mkfifo" => std::ffi::CString::new(rest)
                .map_err(io::Error::other)
                .and_then(|p| {
                    // SAFETY: mkfifo(3) of a NUL-terminated path.
                    if unsafe { libc::mkfifo(p.as_ptr(), 0o640) } == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                }),
            // NAME=VALUE:PATH, its last symlink not followed.
            "xattr" => {
                let (nv, p) = rest.split_once(':').unwrap_or((rest, ""));
                let (n, v) = nv.split_once('=').unwrap_or((nv, ""));
                match (std::ffi::CString::new(p), std::ffi::CString::new(n)) {
                    (Ok(p), Ok(n)) => {
                        // SAFETY: NUL-terminated strings and a value of the length given.
                        let r =
                            unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), v.as_ptr().cast(), v.len(), 0) };
                        if r == 0 {
                            Ok(())
                        } else {
                            Err(io::Error::last_os_error())
                        }
                    }
                    _ => Err(io::Error::other("a NUL in an xattr")),
                }
            }
            "mount" | "umount" | "remount-rw" | "fsopen" | "open-tree" | "overlay" => mounts(kind, rest),
            "chown" => {
                let mut parts = rest.splitn(3, ':');
                let id = |p: Option<&str>| p.and_then(|n| n.parse::<u32>().ok());
                match (id(parts.next()), id(parts.next()), parts.next()) {
                    (Some(u), Some(g), Some(p)) => std::os::unix::fs::lchown(p, Some(u), Some(g)),
                    _ => Err(io::Error::other("a malformed chown")),
                }
            }
            "sleep" => rest
                .parse::<u64>()
                .map_err(io::Error::other)
                .map(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
            // sync(2): what was written committed to its file systems' journals.
            "sync" => {
                // SAFETY: sync(2) takes no arguments and cannot fail.
                unsafe { libc::sync() };
                Ok(())
            }
            "open" => {
                let (m, p) = rest.split_once(':').unwrap_or((rest, ""));
                std::fs::OpenOptions::new()
                    .read(m == "r")
                    .write(m == "w")
                    .open(p)
                    .map(drop)
            }
            "print" => std::fs::read(rest).map(|b| {
                let _ = io::stdout().write_all(&b);
            }),
            "readn" => {
                let (n, p) = rest.split_once(':').unwrap_or((rest, ""));
                n.parse::<u64>().map_err(io::Error::other).and_then(|n| {
                    let mut f = std::fs::File::open(p)?.take(n);
                    let read = io::copy(&mut f, &mut io::sink())?;
                    if read == n {
                        Ok(())
                    } else {
                        Err(io::Error::other(format!("{read} bytes")))
                    }
                })
            }
            "dev" => std::fs::symlink_metadata(rest).map(|m| {
                use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
                let t = if m.file_type().is_block_device() { 'b' } else { 'c' };
                let rdev = m.rdev();
                let _ = writeln!(
                    io::stdout(),
                    "{rest} {t} {}:{} {:o}",
                    libc::major(rdev),
                    libc::minor(rdev),
                    m.mode() & 0o7777
                );
            }),
            "roundrobin" => roundrobin(rest),
            // Whether the guest's kernel maps a file into a DAX window (D189):
            // `dax PATH yes|no`, as statx(2) says (STATX_ATTR_DAX).
            "dax" => dax_of(rest).map(|dax| {
                let _ = writeln!(io::stdout(), "dax {rest} {}", if dax { "yes" } else { "no" });
            }),
            // One byte written in place, opened to write alone: `poke:PATH:OFFSET:BYTE`.
            "poke" => {
                use std::os::unix::fs::FileExt as _;
                let mut parts = rest.rsplitn(3, ':');
                let (byte, offset, path) = (parts.next(), parts.next(), parts.next());
                match (
                    path,
                    offset.and_then(|o| o.parse::<u64>().ok()),
                    byte.and_then(|b| b.parse::<u8>().ok()),
                ) {
                    (Some(path), Some(offset), Some(byte)) => std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .and_then(|f| f.write_all_at(&[byte], offset)),
                    _ => Err(io::Error::other("poke:PATH:OFFSET:BYTE")),
                }
            }
            // Whole pages written in place in one write, opened to write alone:
            // `pages:PATH:OFFSET:COUNT:BYTE`, COUNT pages of BYTE at OFFSET, a page's
            // multiple, as a DAX file's guest writes whole pages (D189, patch 0009).
            "pages" => {
                use std::os::unix::fs::FileExt as _;
                let mut parts = rest.rsplitn(4, ':');
                let (byte, count, offset, path) = (parts.next(), parts.next(), parts.next(), parts.next());
                match (
                    path,
                    offset.and_then(|o| o.parse::<u64>().ok()),
                    count.and_then(|c| c.parse::<usize>().ok()),
                    byte.and_then(|b| b.parse::<u8>().ok()),
                ) {
                    (Some(path), Some(offset), Some(count), Some(byte)) => std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .and_then(|f| f.write_all_at(&vec![byte; count * 4096], offset)),
                    _ => Err(io::Error::other("pages:PATH:OFFSET:COUNT:BYTE")),
                }
            }
            // A read through a handle held across the host's change of its file,
            // `hold:PATH:OFFSET:SIGNAL`: PATH read whole through a handle of its own,
            // SIGNAL.ready made, and once the host makes SIGNAL, a byte read at OFFSET
            // through the handle held, said as `hold PATH OFFSET BYTE`. Nothing is asked
            // about the file between: what it reads is what the guest's mapping holds.
            "hold" => {
                use std::io::Read as _;
                use std::os::unix::fs::FileExt as _;
                let mut parts = rest.rsplitn(3, ':');
                let (signal, offset, path) = (parts.next(), parts.next(), parts.next());
                match (path, offset.and_then(|o| o.parse::<u64>().ok()), signal) {
                    (Some(path), Some(offset), Some(signal)) => (|| {
                        let file = std::fs::File::open(path)?;
                        (&file).read_to_end(&mut Vec::new())?;
                        std::fs::write(format!("{signal}.ready"), b"")?;
                        while std::fs::symlink_metadata(signal).is_err() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        let mut byte = [0u8];
                        file.read_exact_at(&mut byte, offset)?;
                        let _ = writeln!(io::stdout(), "hold {path} {offset} {}", byte[0]);
                        Ok(())
                    })(),
                    _ => Err(io::Error::other("hold:PATH:OFFSET:SIGNAL")),
                }
            }
            // A file mapped 2 MiB at a time across the host's change of its size,
            // `map2m:PATH:SIGNAL`: its first 8 MiB mapped shared at a 2 MiB boundary and each
            // page read, which a DAX file's mapping takes in 2 MiB pages (fs/dax.c,
            // `dax_iomap_pmd_fault`), SIGNAL.ready made; once the host makes SIGNAL, and its
            // attributes have aged (VALID_S, a second), the file stat'ed and opened again,
            // and each page read again, said as `map2m PATH SIZE`.
            "map2m" => {
                let (path, signal) = rest.rsplit_once(':').unwrap_or((rest, ""));
                map_2m(path, signal).map(|size| {
                    let _ = writeln!(io::stdout(), "map2m {path} {size}");
                })
            }
            // A write from a mapping of another file, `frommap:SRC:DST:HOW`: SRC's whole
            // pages mapped shared and written to DST in one write, appended to what it
            // holds (`append`) or in place of it, O_DIRECT (`direct`), and what DST then
            // holds there read back and held to SRC's bytes, said as `frommap DST BYTES`.
            // Where SRC's file system maps it with DAX, a pmem device's or a share's
            // window's (D189), the pages written are device memory a device is given to
            // read.
            "frommap" => from_map(rest).map(|(dst, n)| {
                let _ = writeln!(io::stdout(), "frommap {dst} {n}");
            }),
            // A read into a mapping of another file, `intomap:SRC:DST`: DST's whole pages
            // mapped shared to write, SRC read into them in one read, O_DIRECT, and what DST
            // then holds read back and held to SRC's bytes, said as `intomap DST BYTES`.
            // Where DST's file system maps it with DAX, the pages read into are device
            // memory a device is given to write.
            "intomap" => into_map(rest).map(|(dst, n)| {
                let _ = writeln!(io::stdout(), "intomap {dst} {n}");
            }),
            // The kernel's log records holding `WORD` (`kmsg:WORD`), each said, then how many:
            // /dev/kmsg, which only a privileged exec reads (CAP_SYSLOG).
            "kmsg" => kmsg_holding(rest).map(|n| {
                let _ = writeln!(io::stdout(), "kmsg {rest} {n}");
            }),
            // One byte read, `peek:PATH:OFFSET`, said as `peek PATH OFFSET BYTE`.
            "peek" => {
                use std::os::unix::fs::FileExt as _;
                let (path, offset) = rest.rsplit_once(':').unwrap_or((rest, ""));
                offset
                    .parse::<u64>()
                    .map_err(io::Error::other)
                    .and_then(|offset| {
                        let mut byte = [0u8];
                        std::fs::File::open(path)?.read_exact_at(&mut byte, offset)?;
                        let _ = writeln!(io::stdout(), "peek {path} {offset} {}", byte[0]);
                        Ok(())
                    })
            }
            _ => Err(io::Error::other("an unknown operation")),
        };
        if let Err(e) = done {
            let _ = writeln!(io::stderr(), "{op}: {e}");
            return 1;
        }
    }
    0
}

/// [`fs`]'s `map2m`: `path`'s size as stat'ed once the host has made `signal`.
fn map_2m(path: &str, signal: &str) -> io::Result<u64> {
    use std::os::fd::AsRawFd as _;
    const PMD: usize = 2 << 20;
    const LEN: usize = 4 * PMD;
    let file = std::fs::File::open(path)?;
    // SAFETY: an anonymous reservation of no access, a 2 MiB page longer than the mapping,
    // which goes at its first 2 MiB boundary; this process's until it ends.
    let space = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            LEN + PMD,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if space == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let at = (space as usize).next_multiple_of(PMD);
    // SAFETY: MAP_FIXED within the reservation above.
    let mapped = unsafe {
        libc::mmap(
            at as *mut libc::c_void,
            LEN,
            libc::PROT_READ,
            libc::MAP_SHARED | libc::MAP_FIXED,
            file.as_raw_fd(),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let read_all = || {
        (0..LEN).step_by(4096).fold(0u64, |sum, off| {
            // SAFETY: within the file's mapping, its first 8 MiB, which stays mapped until
            // the process ends.
            let byte = unsafe { std::ptr::read_volatile(mapped.cast::<u8>().add(off)) };
            sum.wrapping_add(u64::from(byte))
        })
    };
    let _ = read_all();
    std::fs::write(format!("{signal}.ready"), b"")?;
    while std::fs::symlink_metadata(signal).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Past its attributes' second (the share's VALID_S): the stat asks the host.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let size = std::fs::metadata(path)?.len();
    drop(std::fs::File::open(path)?);
    let _ = read_all();
    Ok(size)
}

/// `file`'s whole pages mapped shared, `prot` their access: where, and how many bytes,
/// mapped until the process ends.
fn map_pages(file: &std::fs::File, prot: libc::c_int) -> io::Result<(*mut libc::c_void, usize)> {
    use std::os::fd::AsRawFd as _;
    let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)? / 4096 * 4096;
    if len == 0 {
        return Err(io::Error::other("no whole page"));
    }
    // SAFETY: a shared mapping of an open file's first `len` bytes, which nothing unmaps.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok((at, len))
}

/// Whether `dst` holds `want`'s first `n` bytes at `at`, read through a handle of its own.
fn holds(dst: &str, at: u64, want: &[u8], n: usize) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let mut back = vec![0u8; n];
    std::fs::File::open(dst)?.read_exact_at(&mut back, at)?;
    if want.get(..n) == Some(&back[..]) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{n} bytes moved, not the bytes read back"
        )))
    }
}

/// [`fs`]'s `frommap`: DST and how many bytes the write wrote.
fn from_map(spec: &str) -> io::Result<(&str, usize)> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut parts = spec.rsplitn(3, ':');
    let (Some(how), Some(dst), Some(src)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(io::Error::other("frommap:SRC:DST:append|direct"));
    };
    let (map, len) = map_pages(&std::fs::File::open(src)?, libc::PROT_READ)?;
    let out = match how {
        "append" => std::fs::OpenOptions::new().append(true).open(dst)?,
        "direct" => std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_DIRECT)
            .open(dst)?,
        _ => return Err(io::Error::other("append or direct")),
    };
    let at = out.metadata()?.len();
    // SAFETY: write(2) of the mapping's `len` bytes.
    let n = unsafe { libc::write(out.as_raw_fd(), map, len) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    holds(dst, at, &std::fs::read(src)?, n)?;
    Ok((dst, n))
}

/// [`fs`]'s `intomap`: DST and how many bytes the read read.
fn into_map(spec: &str) -> io::Result<(&str, usize)> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let (src, dst) = spec
        .rsplit_once(':')
        .ok_or_else(|| io::Error::other("intomap:SRC:DST"))?;
    let target = std::fs::OpenOptions::new().read(true).write(true).open(dst)?;
    let (map, len) = map_pages(&target, libc::PROT_READ | libc::PROT_WRITE)?;
    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(src)?;
    // SAFETY: read(2) into the mapping's `len` bytes, mapped to be written.
    let n = unsafe { libc::read(input.as_raw_fd(), map, len) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    // SAFETY: msync(2) of the mapping above.
    if unsafe { libc::msync(map, len, libc::MS_SYNC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    holds(dst, 0, &std::fs::read(src)?, n)?;
    Ok((dst, n))
}

/// [`fs`]'s `kmsg`: how many of the kernel's log lines hold `word`, each said: its ring
/// read whole through syslog(2) (SYSLOG_ACTION_READ_ALL), which a privileged exec may
/// (CAP_SYSLOG) where its container has no /dev/kmsg.
fn kmsg_holding(word: &str) -> io::Result<usize> {
    // syslog(2)'s SYSLOG_ACTION_SIZE_BUFFER and SYSLOG_ACTION_READ_ALL.
    const SIZE_BUFFER: libc::c_int = 10;
    const READ_ALL: libc::c_int = 3;
    // SAFETY: klogctl(3) asking the ring's size, which reads no buffer.
    let size = unsafe { libc::klogctl(SIZE_BUFFER, std::ptr::null_mut(), 0) };
    let size = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
    let mut ring = vec![0u8; size.max(1)];
    let len = libc::c_int::try_from(ring.len()).map_err(io::Error::other)?;
    // SAFETY: klogctl(3) reading into `ring`, of `len` bytes.
    let read = unsafe { libc::klogctl(READ_ALL, ring.as_mut_ptr().cast(), len) };
    let read = usize::try_from(read).map_err(|_| io::Error::last_os_error())?;
    let text = String::from_utf8_lossy(ring.get(..read).unwrap_or_default());
    let mut held = 0;
    for line in text.lines().filter(|l| l.contains(word)) {
        held += 1;
        let _ = writeln!(io::stdout(), "{line}");
    }
    Ok(held)
}

/// Whether statx(2) says the kernel maps `path` directly (STATX_ATTR_DAX).
fn dax_of(path: &str) -> io::Result<bool> {
    // include/uapi/linux/stat.h: struct statx's 256 bytes, `stx_attributes` its second
    // word and `stx_attributes_mask` its eighth; its basic fields asked for.
    const STATX_BASIC_STATS: libc::c_uint = 0x7ff;
    const STATX_ATTR_DAX: u64 = 0x0020_0000;
    let c = std::ffi::CString::new(path).map_err(io::Error::other)?;
    let mut st = [0u64; 32];
    // SAFETY: statx(2) of a NUL-terminated path into a buffer of struct statx's size.
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            c.as_ptr(),
            0,
            STATX_BASIC_STATS,
            st.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st[7] & STATX_ATTR_DAX != 0 && st[1] & STATX_ATTR_DAX != 0)
}

/// The mount family's syscalls, as `fs` names them, each made once.
fn mounts(kind: &str, spec: &str) -> io::Result<()> {
    let c = |s: &str| std::ffi::CString::new(s).map_err(io::Error::other);
    let said = |r: libc::c_long| {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    // The new mount API's numbers, the same on x86_64 and arm64 (asm-generic/unistd.h).
    const OPEN_TREE: libc::c_long = 428;
    const FSOPEN: libc::c_long = 430;
    match kind {
        "mount" => {
            let mut parts = spec.splitn(3, ':');
            let (Some(fstype), Some(source), Some(target)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(io::Error::other("a malformed mount"));
            };
            let (fstype, source, target) = (c(fstype)?, c(source)?, c(target)?);
            // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
            let r = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            said(r.into())
        }
        "umount" => {
            let target = c(spec)?;
            // SAFETY: umount2(2) of a NUL-terminated path.
            said(unsafe { libc::umount2(target.as_ptr(), 0) }.into())
        }
        // `LOWER:UPPER:WORK:TARGET`: an overlay, as a nested runtime's snapshotter mounts
        // one, refused where the kernel mounted it read-only (its upper layer's filesystem
        // lacking what overlayfs needs of it).
        "overlay" => {
            let parts: Vec<&str> = spec.splitn(4, ':').collect();
            let [lower, upper, work, target] = parts.as_slice() else {
                return Err(io::Error::other("a malformed overlay"));
            };
            let data = c(&format!("lowerdir={lower},upperdir={upper},workdir={work}"))?;
            let (fstype, target) = (c("overlay")?, c(target)?);
            // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
            let r = unsafe {
                libc::mount(
                    fstype.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    data.as_ptr().cast(),
                )
            };
            said(r.into())?;
            // SAFETY: an all-zero statvfs is a valid out-parameter.
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: statvfs(2) of a NUL-terminated path into our struct.
            said(unsafe { libc::statvfs(target.as_ptr(), &mut st) }.into())?;
            if st.f_flag & libc::ST_RDONLY != 0 {
                return Err(io::Error::other("the overlay mounted read-only"));
            }
            Ok(())
        }
        "remount-rw" => {
            let target = c(spec)?;
            let flags = libc::MS_REMOUNT | libc::MS_BIND;
            // SAFETY: mount(2) of a NUL-terminated target, no source, type or data.
            let r = unsafe {
                libc::mount(
                    std::ptr::null(),
                    target.as_ptr(),
                    std::ptr::null(),
                    flags,
                    std::ptr::null(),
                )
            };
            said(r.into())
        }
        "fsopen" => {
            let fstype = c(spec)?;
            // SAFETY: fsopen(2) of a NUL-terminated type.
            let fd = unsafe { libc::syscall(FSOPEN, fstype.as_ptr(), 0) };
            if let Ok(fd) = libc::c_int::try_from(fd)
                && fd >= 0
            {
                // SAFETY: closing the descriptor just made.
                unsafe { libc::close(fd) };
            }
            said(fd)
        }
        _ => {
            let path = c(spec)?;
            // SAFETY: open_tree(2) of a NUL-terminated path, OPEN_TREE_CLONE.
            let fd = unsafe { libc::syscall(OPEN_TREE, libc::AT_FDCWD, path.as_ptr(), 1) };
            if let Ok(fd) = libc::c_int::try_from(fd)
                && fd >= 0
            {
                // SAFETY: closing the descriptor just made.
                unsafe { libc::close(fd) };
            }
            said(fd)
        }
    }
}

/// `KIND:MAJOR:MINOR:PATH`'s node, mode 0600.
fn mknod(spec: &str) -> io::Result<()> {
    let mut parts = spec.splitn(4, ':');
    let bad = || io::Error::other("a malformed mknod");
    let kind = match parts.next() {
        Some("b") => libc::S_IFBLK,
        Some("c") => libc::S_IFCHR,
        _ => return Err(bad()),
    };
    let major: u32 = parts.next().and_then(|n| n.parse().ok()).ok_or_else(bad)?;
    let minor: u32 = parts.next().and_then(|n| n.parse().ok()).ok_or_else(bad)?;
    let path = std::ffi::CString::new(parts.next().ok_or_else(bad)?).map_err(|_| bad())?;
    // SAFETY: mknod(2) of a NUL-terminated path.
    if unsafe { libc::mknod(path.as_ptr(), kind | 0o600, libc::makedev(major, minor)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends a byte to itself over TCP on 127.0.0.1, on ::1, and from any address to its own
/// host name as the C library resolves it, and prints how each went.
fn loopback() -> i32 {
    use std::net::{TcpListener, TcpStream, ToSocketAddrs};
    let mut code = 0;
    let mut name = [0u8; 256];
    // SAFETY: a buffer of the given length.
    let named = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0;
    let end = name.iter().position(|&b| b == 0).unwrap_or(0);
    let own = String::from_utf8_lossy(name.get(..end).unwrap_or_default()).into_owned();
    for addr in ["127.0.0.1:0", "[::1]:0", "own name"] {
        let tried = (|| -> io::Result<()> {
            let (listener, to) = if addr == "own name" {
                if !named {
                    return Err(io::Error::last_os_error());
                }
                let listener = TcpListener::bind("0.0.0.0:0")?;
                let port = listener.local_addr()?.port();
                let to = (own.as_str(), port)
                    .to_socket_addrs()?
                    .next()
                    .ok_or_else(|| io::Error::other("no address"))?;
                (listener, to)
            } else {
                let listener = TcpListener::bind(addr)?;
                let to = listener.local_addr()?;
                (listener, to)
            };
            let mut client = TcpStream::connect(to)?;
            client.write_all(b"x")?;
            let (mut server, _) = listener.accept()?;
            let mut byte = [0u8; 1];
            server.read_exact(&mut byte)?;
            if byte != *b"x" {
                return Err(io::Error::other("a different byte"));
            }
            Ok(())
        })();
        let _ = match tried {
            Ok(()) => writeln!(io::stdout(), "{addr} ok"),
            Err(e) => {
                code = 1;
                writeln!(io::stdout(), "{addr} {e}")
            }
        };
    }
    code
}

fn cat() -> i32 {
    let mut buf = vec![0u8; 64 * 1024];
    let (mut stdin, mut stdout) = (io::stdin().lock(), io::stdout().lock());
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return 0,
            Ok(n) => {
                if stdout.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return 1;
                }
            }
            Err(_) => return 1,
        }
    }
}

/// `read`'s: each file of `paths` read to its end, then `read PATH BYTES in MS ms` said.
fn read_through(paths: &[String]) -> i32 {
    let mut buf = vec![0u8; 1 << 20];
    for path in paths {
        let began = std::time::Instant::now();
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                let _ = writeln!(io::stderr(), "{path}: {e}");
                return 1;
            }
        };
        let mut total = 0u64;
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = writeln!(io::stderr(), "{path}: {e}");
                    return 1;
                }
            }
        }
        let _ = writeln!(
            io::stdout(),
            "read {path} {total} in {} ms",
            began.elapsed().as_millis()
        );
    }
    0
}

/// Writes `len` pattern bytes of `salt` to stdout.
fn bulk(len: u64, salt: u64) -> i32 {
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdout = io::stdout().lock();
    let mut at = 0;
    while at < len {
        let n = (len - at).min(buf.len() as u64) as usize;
        let chunk = buf.get_mut(..n).unwrap_or_default();
        fill(salt, at, chunk);
        if stdout.write_all(chunk).is_err() {
            return 1;
        }
        at += n as u64;
    }
    0
}

/// Leaves a child that holds stdout open and never exits, then exits: the run must still
/// end, as a container ends with its main process.
fn orphan() -> i32 {
    // SAFETY: fork(2); the child only sleeps.
    if unsafe { libc::fork() } == 0 {
        loop {
            // SAFETY: blocks until a signal arrives.
            unsafe { libc::pause() };
        }
    }
    let _ = io::stdout().write_all(b"parent done\n");
    0
}

/// Leaves a child holding `mib` MiB, its output closed, and exits once the child holds
/// them: the run's end kills the child after its command and output have ended, and the
/// child's end takes freeing them, for a record of processes to wait for (D123).
/// Spins `threads` threads for `ms` milliseconds each (D191), first, with `escape`,
/// leaving the run's cgroup for its microVM's root, as a workload root of its own guest
/// may with `--privileged --cgroupns host`: `/sys/fs/cgroup` is then the whole tree,
/// writable, and it writes itself into the root's `cgroup.procs`, out of the run's cpuset.
/// Says the cgroup it spins in (`cgroup PATH`), then `burned`.
fn burn(threads: usize, ms: u64, escape: bool) -> i32 {
    if escape && let Err(e) = std::fs::write("/sys/fs/cgroup/cgroup.procs", "0") {
        let _ = writeln!(io::stdout(), "escape: cgroup.procs: {e}");
        return 1;
    }
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let _ = writeln!(io::stdout(), "cgroup {}", cgroup.trim());
    let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    let spinning: Vec<_> = (0..threads)
        .filter_map(|_| {
            std::thread::Builder::new()
                .spawn(move || {
                    while std::time::Instant::now() < until {
                        std::hint::spin_loop();
                    }
                })
                .ok()
        })
        .collect();
    let spun = spinning.len();
    for t in spinning {
        let _ = t.join();
    }
    if spun < threads {
        let _ = writeln!(io::stdout(), "burn: {spun} of {threads} threads");
        return 1;
    }
    let _ = writeln!(io::stdout(), "burned");
    0
}

fn heavy(mib: usize) -> i32 {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) into a two-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return 1;
    }
    // SAFETY: fork(2) of this single-threaded process; the child touches memory, says so
    // and sleeps.
    match unsafe { libc::fork() } {
        0 => {
            if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                use std::os::fd::AsRawFd as _;
                // SAFETY: dup2(2) of a descriptor of ours onto stdout and stderr.
                unsafe {
                    libc::dup2(null.as_raw_fd(), 1);
                    libc::dup2(null.as_raw_fd(), 2);
                }
            }
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..mib {
                held.push(vec![1u8; 1 << 20]);
            }
            // SAFETY: write(2) of a byte of ours to the pipe's write end.
            unsafe { libc::write(fds[1], b"r".as_ptr().cast(), 1) };
            loop {
                // SAFETY: blocks until a signal arrives.
                unsafe { libc::pause() };
            }
        }
        pid if pid > 0 => {
            let mut ready = 0u8;
            // SAFETY: close(2) of our write end; read(2) of a byte into a local.
            let n = unsafe {
                libc::close(fds[1]);
                libc::read(fds[0], (&raw mut ready).cast(), 1)
            };
            let _ = writeln!(io::stdout(), "holding {mib} MiB in {pid}");
            i32::from(n != 1)
        }
        _ => 1,
    }
}

/// Dials each host vsock port (CID 2) and prints `PORT connected`, then what the host
/// sends within a second (`read N bytes`, `closed` or `nothing`), or `PORT refused ERRNO`:
/// what a workload reaches of the host when nothing confines it.
fn vsock_hello(port: u32) -> i32 {
    use std::os::fd::FromRawFd as _;
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        let _ = writeln!(io::stdout(), "{port} refused {}", io::Error::last_os_error());
        return 0;
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let mut sock = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: connect(2) with an address of its own type and size.
    if unsafe { libc::connect(fd, (&raw const addr).cast(), len) } != 0 {
        let _ = writeln!(io::stdout(), "{port} refused {}", io::Error::last_os_error());
        return 0;
    }
    let mut asked =
        shards_abi::run::header(shards_abi::run::kind::HELLO, shards_abi::run::TOKEN as u32).to_vec();
    asked.extend_from_slice(&[0u8; shards_abi::run::TOKEN]);
    asked.extend_from_slice(b"GET /_ping HTTP/1.1\r\nHost: x\r\n\r\n");
    let wait = libc::timeval {
        tv_sec: 2,
        tv_usec: 0,
    };
    // SAFETY: setsockopt(2) of a timeval of ours, of its size.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const wait).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    let said = sock.write_all(&asked);
    let mut buf = [0u8; 4096];
    let line = match (said, sock.read(&mut buf)) {
        (_, Ok(0)) | (Err(_), _) => format!("{port} closed"),
        (_, Ok(n)) => format!("{port} answered {n} bytes"),
        (_, Err(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            format!("{port} nothing")
        }
        (_, Err(_)) => format!("{port} closed"),
    };
    let _ = writeln!(io::stdout(), "{line}");
    0
}

fn vsock(ports: &[String]) -> i32 {
    for port in ports {
        let Ok(n) = port.parse::<u32>() else {
            return 2;
        };
        // SAFETY: socket(2), connect(2), setsockopt(2) and read(2) on a descriptor this
        // function owns and closes, with a sockaddr_vm it initializes.
        let line = unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                format!("{n} socket {}", io::Error::last_os_error())
            } else {
                let mut addr: libc::sockaddr_vm = std::mem::zeroed();
                addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
                addr.svm_cid = libc::VMADDR_CID_HOST;
                addr.svm_port = n;
                let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
                let line = if libc::connect(fd, (&raw const addr).cast(), len) == 0 {
                    let wait = libc::timeval {
                        tv_sec: 1,
                        tv_usec: 0,
                    };
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVTIMEO,
                        (&raw const wait).cast(),
                        std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                    );
                    let mut buf = [0u8; 4096];
                    match libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) {
                        0 => format!("{n} connected closed"),
                        r if r > 0 => format!("{n} connected read {r} bytes"),
                        _ => format!("{n} connected nothing"),
                    }
                } else {
                    format!("{n} refused {}", io::Error::last_os_error())
                };
                libc::close(fd);
                line
            }
        };
        let _ = writeln!(io::stdout(), "{line}");
    }
    0
}

/// What a terminal workload sees, one `key value` per line: whether each of its stdio is
/// a terminal, whether that is its session's controlling terminal with it in the
/// foreground, the terminal's size and TERM. Then `winch` waits for SIGWINCH and reports
/// the new size, and `read` reads a line from stdin and reports it.
fn tty(then: &str) -> i32 {
    fn size() -> String {
        // SAFETY: TIOCGWINSZ fills a zeroed winsize.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) } != 0 {
            return "none".into();
        }
        format!("{} {}", ws.ws_row, ws.ws_col)
    }
    // SIGWINCH is blocked from the start, so none is lost before sigsuspend(2) waits.
    // SAFETY: sigset operations on locals; the handler only stores an atomic.
    let waiting = unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGWINCH);
        let mut before: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut before);
        libc::signal(
            libc::SIGWINCH,
            caught as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::sigdelset(&mut before, libc::SIGWINCH);
        before
    };
    let mut out = String::new();
    for (fd, name) in [(0, "stdin"), (1, "stdout"), (2, "stderr")] {
        // SAFETY: isatty(3) on a standard descriptor.
        let tty = unsafe { libc::isatty(fd) } == 1;
        out.push_str(&format!("{name} tty {tty}\n"));
    }
    let controlling = std::fs::File::open("/dev/tty").is_ok();
    // SAFETY: plain getters.
    let foreground = unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
    out.push_str(&format!("controlling {controlling}\nforeground {foreground}\n"));
    out.push_str(&format!("size {}\n", size()));
    let term = std::env::var("TERM").unwrap_or_default();
    out.push_str(&format!("term {term}\nready\n"));
    let _ = io::stdout().write_all(out.as_bytes());
    match then {
        "winch" => {
            while CAUGHT.load(Ordering::Relaxed) == 0 {
                // SAFETY: waits with SIGWINCH unblocked, on a valid set.
                unsafe { libc::sigsuspend(&waiting) };
            }
            let _ = writeln!(io::stdout(), "resized {}", size());
        }
        "read" => {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            let _ = writeln!(io::stdout(), "read {}", line.trim_end());
        }
        _ => {}
    }
    0
}

/// The signal `trap` caught, by its number.
static CAUGHT: AtomicI32 = AtomicI32::new(0);

extern "C" fn caught(sig: libc::c_int) {
    CAUGHT.store(sig, Ordering::Relaxed);
}

/// Catches the named signal, says `ready`, and reports the number it arrives with.
/// Ignores the signal named, says it is ready, and waits for ever: what only another
/// signal ends.
fn ignore(name: &str) -> i32 {
    let sig = match name {
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        _ => return 2,
    };
    // SAFETY: a disposition of this process's own.
    unsafe { libc::signal(sig, libc::SIG_IGN) };
    let _ = writeln!(io::stdout(), "ready");
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

fn trap(name: &str) -> i32 {
    let sig = match name {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        _ => return 2,
    };
    // The signal stays blocked but while sigsuspend(2) waits for it: one that arrived
    // between a check of CAUGHT and pause(2) would leave pause waiting for ever.
    // SAFETY: sigset operations on locals, and an async-signal-safe handler that only
    // stores an atomic.
    let waiting = unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, sig);
        let mut before: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut before);
        libc::signal(sig, caught as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::sigdelset(&mut before, sig);
        before
    };
    let _ = writeln!(io::stdout(), "ready");
    while CAUGHT.load(Ordering::Relaxed) == 0 {
        // SAFETY: waits with `sig` unblocked, on a valid set.
        unsafe { libc::sigsuspend(&waiting) };
    }
    let _ = writeln!(io::stdout(), "got {}", CAUGHT.load(Ordering::Relaxed));
    0
}

/// As an agent's first process (D59): what its domain gives it, a line each, `confined`
/// first, then `comm` set to `confined-ready` and held until killed. Each path in `see`
/// is listed or its errno said; each in `write` written or its errno said.
/// vfork(2) as the kernel makes it, the child sharing this process's memory until it ends:
/// so the child ends in the instruction after, touching no memory (libc's vfork, a
/// function the child returns through, corrupts its parent's stack, rust-lang/libc#1596).
/// x86_64's `vfork`; arm64, which has none, `clone` with CLONE_VM and CLONE_VFORK, as its
/// libc's vfork. The child's pid, or the errno that kept it from starting.
fn vfork_and_exit() -> Result<libc::pid_t, i32> {
    let ret: i64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the child runs only the instructions below, in registers, then exit_group.
    unsafe {
        std::arch::asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "mov eax, 231",
            "xor edi, edi",
            "syscall",
            "2:",
            inlateout("rax") libc::SYS_vfork => ret,
            out("rdi") _,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above: the child runs only these instructions, then exit_group.
    unsafe {
        std::arch::asm!(
            "svc #0",
            "cbnz x0, 2f",
            "mov x8, #94",
            "mov x0, #0",
            "svc #0",
            "2:",
            inlateout("x8") libc::SYS_clone => _,
            inlateout("x0") (libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD) as i64 => ret,
            in("x1") 0i64,
            in("x2") 0i64,
            in("x3") 0i64,
            in("x4") 0i64,
            options(nostack),
        );
    }
    if ret < 0 {
        Err(i32::try_from(-ret).unwrap_or(libc::EINVAL))
    } else {
        Ok(ret as libc::pid_t)
    }
}

/// What a probe that starts a process says of `pid`: the child waited for, or the error
/// that kept it from starting (errno as the kernel left it).
fn reaped(pid: libc::pid_t) -> libc::c_long {
    if pid > 0 {
        // SAFETY: waitpid(2) of the child just started.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        0
    } else {
        -1
    }
}

fn confined(args: &[String]) -> i32 {
    let mut out = String::new();
    // SAFETY: plain getters, and getgroups(2) into 64 entries.
    let (uid, gid, groups) = unsafe {
        let mut g = [0 as libc::gid_t; 64];
        let n = libc::getgroups(64, g.as_mut_ptr());
        (libc::getuid(), libc::getgid(), usize::try_from(n).unwrap_or(0))
    };
    out.push_str(&format!("confined uid={uid} gid={gid} groups={groups}\n"));
    let procs = std::fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                })
                .count()
        })
        .unwrap_or(0);
    out.push_str(&format!("confined pid={} procs={procs}\n", std::process::id()));
    // SAFETY: an all-zero utsname is a valid out-parameter, and uname(2) NUL-terminates.
    let host = unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        libc::uname(&mut uts);
        std::ffi::CStr::from_ptr(uts.nodename.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    out.push_str(&format!("confined hostname={host}\n"));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()))
            .unwrap_or_default()
    };
    out.push_str(&format!(
        "confined caps eff={} prm={} inh={} bnd={} amb={} nnp={}\n",
        field("CapEff:"),
        field("CapPrm:"),
        field("CapInh:"),
        field("CapBnd:"),
        field("CapAmb:"),
        field("NoNewPrivs:")
    ));
    let errno = |e: &io::Error| format!("errno {}", e.raw_os_error().unwrap_or(0));
    let mut mode: &str = "";
    // The last request `srv-receive` saw: its sender, and its ID.
    let mut asked: Option<(String, String)> = None;
    for a in args {
        match a.as_str() {
            "see" | "write" | "bind" | "connect" | "call" | "listen" | "reach" | "unreach" | "cat"
            | "resolve" | "dnsprobe" | "dnsask" | "unix" | "abstract" | "unix-serve" | "pause"
            | "udpflood" | "tcpflood" | "udpaskfrom" | "reachmany" | "reachrounds" | "udpask"
            | "srv-send" | "srv-receive" | "srv-answer" | "owner" | "mcp-run" | "srv-run"
            | "srv-run-timeout" | "srv-run-cancel" | "srv-run-abandon" | "srv-bench" | "srv-modern"
            | "hold-tell" | "make" | "srv-hand" | "follow" | "hardlink" | "exec" | "nosuid" | "hear"
            | "size" => mode = a.as_str(),
            "srv-mcp" => {
                let said = match server_call("mcp", "{}") {
                    Ok((t, false)) => t,
                    Ok((t, true)) => format!("refused {t}"),
                    Err(e) => format!("error {e}"),
                };
                out.push_str(&format!("confined srv-mcp: {said}\n"));
            }
            "srv-peers" => {
                let said = match server_call("peers", "{}") {
                    Ok((t, false)) => t,
                    Ok((t, true)) => format!("refused {t}"),
                    Err(e) => format!("error {e}"),
                };
                out.push_str(&format!("confined srv-peers: {said}\n"));
            }
            // The in-VM server, as an MCP client asks it: who it knows this agent as.
            "whoami" => {
                let said = match server_call("whoami", "{}") {
                    Ok((name, false)) => name,
                    Ok((why, true)) => format!("refused {why}"),
                    Err(e) => format!("error {e}"),
                };
                out.push_str(&format!("confined whoami: {said}\n"));
            }
            // A grandchild let go as daemons are (fork, setsid, fork), named `escaped`,
            // that waits until it is ended.
            "daemonize" => {
                // SAFETY: fork(2) in a process of one thread; the children only call the
                // kernel and wait.
                unsafe {
                    match libc::fork() {
                        0 => {
                            libc::setsid();
                            if libc::fork() == 0 {
                                libc::prctl(libc::PR_SET_NAME, c"escaped".as_ptr());
                                loop {
                                    libc::pause();
                                }
                            }
                            libc::_exit(0);
                        }
                        pid if pid > 0 => {
                            libc::waitpid(pid, std::ptr::null_mut(), 0);
                        }
                        _ => {}
                    }
                }
                out.push_str("confined daemonize: done\n");
            }
            // §9.8's kernel channels, made: a System V shared memory segment and message queue
            // of one key, a POSIX message queue, a file in /dev/shm.
            "ipc-make" => {
                let said = |rc: libc::c_int| {
                    if rc < 0 {
                        errno(&io::Error::last_os_error())
                    } else {
                        "ok".to_string()
                    }
                };
                // SAFETY: each a system call on constant arguments; descriptors are kept, so
                // that what they name lives while the agent does.
                let made = unsafe {
                    [
                        ("shm", said(libc::shmget(IPC_KEY, 4096, libc::IPC_CREAT | 0o666))),
                        ("msg", said(libc::msgget(IPC_KEY, libc::IPC_CREAT | 0o666))),
                        (
                            "mq",
                            said(libc::mq_open(
                                c"/shards-chan".as_ptr(),
                                libc::O_CREAT | libc::O_RDWR,
                                0o666 as libc::mode_t,
                                std::ptr::null::<libc::mq_attr>(),
                            )),
                        ),
                        (
                            "file",
                            said(libc::open(
                                c"/dev/shm/shards-chan".as_ptr(),
                                libc::O_CREAT | libc::O_RDWR,
                                0o666,
                            )),
                        ),
                    ]
                };
                for (what, r) in made {
                    out.push_str(&format!("confined ipc-make {what}: {r}\n"));
                }
            }
            // The same, opened as another would: each, and a signal to every process it can.
            "ipc-try" => {
                let said = |rc: libc::c_int| {
                    if rc < 0 {
                        errno(&io::Error::last_os_error())
                    } else {
                        "ok".to_string()
                    }
                };
                // SAFETY: each a system call on constant arguments.
                let tried = unsafe {
                    [
                        ("shm", said(libc::shmget(IPC_KEY, 0, 0))),
                        ("msg", said(libc::msgget(IPC_KEY, 0))),
                        (
                            "mq",
                            said(libc::mq_open(c"/shards-chan".as_ptr(), libc::O_RDONLY)),
                        ),
                        (
                            "file",
                            said(libc::open(c"/dev/shm/shards-chan".as_ptr(), libc::O_RDONLY)),
                        ),
                        ("kill", said(libc::kill(-1, libc::SIGUSR1))),
                    ]
                };
                for (what, r) in tried {
                    out.push_str(&format!("confined ipc-try {what}: {r}\n"));
                }
            }
            // Its first process ends here.
            "end" => {
                let _ = io::stdout().write_all(out.as_bytes());
                return 0;
            }
            "fill" => {
                let _ = io::stdout().write_all(out.as_bytes());
                return fill_scratch();
            }
            // `TO|TEXT`: an innocent agent, as `hold`, that once it holds its memory tells TO
            // so, a request of TEXT, for an agent that waits for it before it starts.
            spec if mode == "hold-tell" => {
                let _ = io::stdout().write_all(out.as_bytes());
                let held: Vec<u8> = vec![1; 16 << 20];
                // SAFETY: prctl(2) with a NUL-terminated name.
                unsafe { libc::prctl(libc::PR_SET_NAME, c"innocent".as_ptr()) };
                let (to, text) = spec.split_once('|').unwrap_or((spec, ""));
                let args = format!(r#"{{"to":{},"text":{}}}"#, quote_json(to), quote_json(text));
                let said = match server_call("send", &args) {
                    Ok((t, false)) => format!("ok {t}"),
                    Ok((t, true)) => format!("refused {t}"),
                    Err(e) => format!("error {e}"),
                };
                let _ = writeln!(io::stdout(), "confined hold-tell: {said}");
                let _ = io::stdout().flush();
                loop {
                    // SAFETY: pause(2) until a signal.
                    unsafe { libc::pause() };
                    std::hint::black_box(&held);
                }
            }
            // An innocent agent: 16 MiB resident, more than a filler, and no scratch.
            "hold" => {
                let _ = io::stdout().write_all(out.as_bytes());
                let held: Vec<u8> = vec![1; 16 << 20];
                // SAFETY: prctl(2) with a NUL-terminated name; pause(2) until a signal.
                unsafe {
                    libc::prctl(libc::PR_SET_NAME, c"innocent".as_ptr());
                    loop {
                        libc::pause();
                        std::hint::black_box(&held);
                    }
                }
            }
            path if mode == "unix" => {
                // Until it exists, 3 s at most: the run's own command makes it as it starts.
                // The last try's time (CLOCK_MONOTONIC, which the domain shares) says whether
                // it was made by then.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let tried = loop {
                    let r = std::os::unix::net::UnixStream::connect(path);
                    match &r {
                        Err(e)
                            if e.kind() == io::ErrorKind::NotFound
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        _ => break r,
                    }
                };
                let said = match tried {
                    Ok(_) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined unix {path}: {said}\n"));
                out.push_str(&format!("confined unix tried {}\n", monotonic_ns()));
            }
            // A datagram to ADDR, and whether anything answered in 2 s.
            // `PORT,ADDR`: a question to ADDR from the agent's PORT, as an answer to a flow
            // opened to it on PORT would be sent, and whether ADDR answers.
            spec if mode == "udpaskfrom" => {
                let (from, addr) = spec.split_once(',').unwrap_or(("0", spec));
                let said = match std::net::UdpSocket::bind(format!("0.0.0.0:{from}")).and_then(|s| {
                    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
                    s.send_to(b"ask", addr)?;
                    let mut b = [0u8; 64];
                    s.recv_from(&mut b).map(drop)
                }) {
                    Ok(()) => "reply".to_string(),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                        "none".to_string()
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined udpaskfrom {from} {addr}: {said}\n"));
            }
            addr if mode == "udpask" => {
                // `NET1` is the first address of its own network, its gateway there.
                let addr = addr.replace("NET1", &first_of_own());
                let addr = addr.as_str();
                let said = match std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
                    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
                    s.send_to(b"ask", addr)?;
                    let mut b = [0u8; 64];
                    s.recv_from(&mut b).map(drop)
                }) {
                    Ok(()) => "reply".to_string(),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                        "none".to_string()
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined udpask {addr}: {said}\n"));
            }
            // CODE: code mode's program, run by the in-VM server (D136); `MS|CODE` with its
            // timeout.
            spec if mode == "srv-run" || mode == "srv-run-timeout" => {
                let (timeout, code) = match mode {
                    "srv-run-timeout" => spec.split_once('|').unwrap_or(("0", spec)),
                    _ => ("", spec),
                };
                let args = match timeout {
                    "" => format!(r#"{{"code":{}}}"#, quote_json(code)),
                    ms => format!(r#"{{"code":{},"timeout_ms":{ms}}}"#, quote_json(code)),
                };
                let said = match server_call("run", &args) {
                    Ok((t, false)) => format!("ok {t}"),
                    Ok((t, true)) => format!("refused {t}"),
                    Err(e) => format!("error {e}"),
                };
                out.push_str(&format!("confined srv-run: {said}\n"));
            }
            // CODE: code mode's program, let go of as it runs (MCP's
            // `notifications/cancelled`), twice: from a session of its own, which names
            // another request 2 than the program's; and from the program's session, on
            // another connection. What the program's connection is told each time.
            code if mode == "srv-run-cancel" => {
                let cancelled = |own: bool| -> Result<String, String> {
                    let args = format!(r#"{{"code":{}}}"#, quote_json(code));
                    let mut running = Session::open()?;
                    running.post(&tool_call("run", &args))?;
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let mut other = if own { running.join()? } else { Session::open()? };
                    other.post(
                        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"enough"}}"#,
                    )?;
                    other.answer()?;
                    running.answer()
                };
                for (whose, own) in [("stranger", false), ("own", true)] {
                    let said = cancelled(own).unwrap_or_else(|e| format!("error {e}"));
                    out.push_str(&format!("confined srv-run-cancel {whose}: {said}\n"));
                }
            }
            // `ROUNDS|CALLS`: what code mode costs its caller (PM M196), measured where the
            // caller is: in each round, one after another in one session, CALLS MCP calls
            // of `whoami`; one `run` of a program that makes those CALLS; and one `run` of
            // a program that makes none. Each's n, p50, p90, p99 and max, in µs.
            spec if mode == "srv-bench" => {
                let said = (|| -> Result<String, String> {
                    let (rounds, calls) = spec.split_once('|').ok_or("no ROUNDS|CALLS")?;
                    let rounds: usize = rounds.parse().map_err(|_| "ROUNDS")?;
                    let calls: usize = calls.parse().map_err(|_| "CALLS")?;
                    let program = format!(
                        r#"{{"code":{}}}"#,
                        quote_json(&format!(
                            "for (let i = 0; i < {calls}; i++) await shards.whoami(); return {calls};"
                        ))
                    );
                    let mut session = Session::open()?;
                    let mut ask = |args: (&str, &str)| -> Result<f64, String> {
                        let t = std::time::Instant::now();
                        session.post(&tool_call(args.0, args.1))?;
                        let answer = session.answer()?;
                        if !answer.contains(r#""isError":false"#) {
                            return Err(answer);
                        }
                        Ok(t.elapsed().as_secs_f64() * 1e6)
                    };
                    let (mut mcp, mut run, mut empty) = (Vec::new(), Vec::new(), Vec::new());
                    for _ in 0..rounds {
                        let mut took = 0.0;
                        for _ in 0..calls {
                            took += ask(("whoami", "{}"))?;
                        }
                        mcp.push(took);
                        run.push(ask(("run", &program))?);
                        empty.push(ask(("run", r#"{"code":"return 0;"}"#))?);
                    }
                    Ok(format!(
                        "mcp-calls={calls} {}; run-calls={calls} {}; run-empty {}",
                        summary_us(&mut mcp),
                        summary_us(&mut run),
                        summary_us(&mut empty)
                    ))
                })()
                .unwrap_or_else(|e| format!("error {e}"));
                out.push_str(&format!("confined srv-bench: {said}\n"));
            }
            // NAME: the in-VM server asked as a client of MCP 2026-07-28 asks it, each
            // request standing alone (D138): what each answer's status and body say,
            // a line each, NAME heading them.
            name if mode == "srv-modern" => {
                for (what, said) in modern_asks() {
                    out.push_str(&format!("confined srv-modern {name} {what}: {said}\n"));
                }
            }
            // CODE: code mode's program, its session closed as it runs.
            code if mode == "srv-run-abandon" => {
                let said = (|| -> Result<String, String> {
                    let args = format!(r#"{{"code":{}}}"#, quote_json(code));
                    let mut running = Session::open()?;
                    running.post(&tool_call("run", &args))?;
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    drop(running);
                    Ok("abandoned".to_string())
                })()
                .unwrap_or_else(|e| format!("error {e}"));
                out.push_str(&format!("confined srv-run-abandon: {said}\n"));
            }
            // `TO|TEXT`: a request to TO.
            spec if mode == "srv-send" => {
                let (to, body) = spec.split_once('|').unwrap_or((spec, ""));
                let args = format!(r#"{{"to":{},"text":{}}}"#, quote_json(to), quote_json(body));
                let said = match server_call("send", &args) {
                    Ok((t, false)) => format!("ok {t}"),
                    Ok((t, true)) => format!("refused {t}"),
                    Err(e) => format!("error {e}"),
                };
                out.push_str(&format!("confined srv-send {to}: {said}\n"));
            }
            // SECS: what peers sent, asked for until something is there or SECS pass; the
            // last request among it is what `srv-answer` answers.
            secs if mode == "srv-receive" => {
                let until =
                    std::time::Instant::now() + std::time::Duration::from_secs(secs.parse().unwrap_or(0));
                let said = loop {
                    match server_call("receive", "{}") {
                        Ok((t, false)) if t != "[]" || std::time::Instant::now() >= until => break t,
                        Ok((_, false)) => std::thread::sleep(std::time::Duration::from_millis(50)),
                        Ok((t, true)) => break format!("refused {t}"),
                        Err(e) => break format!("error {e}"),
                    }
                };
                // The last request: its sender and ID, from `"from":"…","kind":"request","id":N`.
                if let Some(at) = said.rfind(r#""kind":"request","id":"#) {
                    let id: String = said
                        .get(at + r#""kind":"request","id":"#.len()..)
                        .unwrap_or_default()
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    let from = said
                        .get(..at)
                        .and_then(|h| h.rfind(r#""from":""#).map(|i| (h, i)))
                        .map(|(h, i)| unquote(h.get(i + r#""from":""#.len()..).unwrap_or_default()))
                        .unwrap_or_default();
                    asked = Some((from, id));
                }
                out.push_str(&format!("confined srv-receive: {said}\n"));
            }
            // `KIND:PATH[:ARG]`: a file made, as an agent might plant one: `file` with ARG
            // its text, `link` to ARG, `fifo`, or `big` of ARG bytes.
            spec if mode == "make" => {
                let mut parts = spec.splitn(3, ':');
                let (kind, path, arg) = (
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                );
                let made = match kind {
                    "file" => std::fs::write(path, arg),
                    "link" => std::os::unix::fs::symlink(arg, path),
                    "fifo" => std::ffi::CString::new(path)
                        .map_err(io::Error::other)
                        .and_then(|c| {
                            // SAFETY: mkfifo(3) of a NUL-terminated path.
                            if unsafe { libc::mkfifo(c.as_ptr(), 0o644) } == 0 {
                                Ok(())
                            } else {
                                Err(io::Error::last_os_error())
                            }
                        }),
                    "big" => std::fs::write(path, vec![b'b'; arg.parse().unwrap_or(0)]),
                    _ => Err(io::Error::other("no such kind")),
                };
                let said = match made {
                    Ok(()) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined make {spec}: {said}\n"));
            }
            // PATH: a file handed across, the answer to the last request received (D139).
            path if mode == "srv-hand" => {
                let said = match &asked {
                    Some((from, id)) => {
                        let args = format!(
                            r#"{{"to":{},"id":{id},"path":{}}}"#,
                            quote_json(from),
                            quote_json(path)
                        );
                        match server_call("hand", &args) {
                            Ok((t, false)) => format!("ok {t}"),
                            Ok((t, true)) => format!("refused {t}"),
                            Err(e) => format!("error {e}"),
                        }
                    }
                    None => "no request".to_string(),
                };
                out.push_str(&format!("confined srv-hand {path}: {said}\n"));
            }
            // TEXT: the answer to the last request received.
            body if mode == "srv-answer" => {
                let said = match &asked {
                    Some((from, id)) => {
                        let args = format!(
                            r#"{{"to":{},"id":{id},"text":{}}}"#,
                            quote_json(from),
                            quote_json(body)
                        );
                        match server_call("answer", &args) {
                            Ok((t, false)) => format!("ok {t}"),
                            Ok((t, true)) => format!("refused {t}"),
                            Err(e) => format!("error {e}"),
                        }
                    }
                    None => "no request".to_string(),
                };
                out.push_str(&format!("confined srv-answer: {said}\n"));
            }
            secs if mode == "pause" => {
                std::thread::sleep(std::time::Duration::from_secs(secs.parse().unwrap_or(0)));
            }
            // `ADDR,SECS`: a datagram to ADDR from a socket of a new port each, for SECS:
            // a new flow each to whatever tracks them.
            spec if mode == "udpflood" => {
                let (addr, secs) = spec.split_once(',').unwrap_or((spec, "0"));
                let until =
                    std::time::Instant::now() + std::time::Duration::from_secs(secs.parse().unwrap_or(0));
                let mut sent = 0u64;
                while std::time::Instant::now() < until {
                    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
                        && s.send_to(b"x", addr).is_ok()
                    {
                        sent += 1;
                    }
                }
                out.push_str(&format!("confined udpflood {addr}: {sent} sent\n"));
            }
            // `ADDR,SECS`: connections to ADDR for SECS seconds, each held until then: each
            // an assured entry of conntrack's, which no full table evicts (`early_drop`).
            // Opened from several threads at once, so that a loaded host fills the table
            // soon all the same. How many were made, against the table's size where it is
            // readable, and when, from the start, as many as it holds had been made.
            spec if mode == "tcpflood" => {
                use std::net::ToSocketAddrs as _;
                let (addr, secs) = spec.split_once(',').unwrap_or((spec, "0"));
                let to = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
                let (began, start_ns) = (std::time::Instant::now(), monotonic_ns());
                let until = began + std::time::Duration::from_secs(secs.parse().unwrap_or(0));
                let max = std::fs::read_to_string("/proc/sys/net/netfilter/nf_conntrack_max")
                    .map(|m| m.trim().to_string())
                    .unwrap_or_else(|e| errno(&e));
                let table: usize = max.parse().unwrap_or(usize::MAX);
                let held = std::sync::Mutex::new(Vec::new());
                let full = std::sync::Mutex::new(None::<u128>);
                if let Some(to) = to {
                    std::thread::scope(|scope| {
                        for _ in 0..16 {
                            let _ = std::thread::Builder::new().spawn_scoped(scope, || {
                                while std::time::Instant::now() < until {
                                    let Ok(c) = std::net::TcpStream::connect_timeout(
                                        &to,
                                        std::time::Duration::from_millis(100),
                                    ) else {
                                        continue;
                                    };
                                    let mut held =
                                        held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    held.push(c);
                                    if held.len() >= table {
                                        full.lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .get_or_insert(began.elapsed().as_millis());
                                    }
                                }
                            });
                        }
                    });
                }
                let made = held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len();
                // When, from its start and on the clock every agent of the microVM shares.
                let full = match *full.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                    Some(ms) => format!("{ms} ms, at {} ns", start_ns + ms * 1_000_000),
                    None => "never".to_string(),
                };
                out.push_str(&format!(
                    "confined tcpflood {addr}: {made} made, table {max}, full at {full}\n"
                ));
            }
            // `ADDR|ADDR…,N,SECS,UNTIL`: rounds of N connections to each ADDR, each given SECS
            // seconds, every half second until UNTIL seconds have passed: how many each
            // round made, and when it began on the microVM's clock, for a test to judge the
            // rounds that came after something another agent did.
            spec if mode == "reachrounds" => {
                use std::net::ToSocketAddrs as _;
                let mut parts = spec.split(',');
                let addrs: Vec<&str> = parts.next().unwrap_or_default().split('|').collect();
                let n: u32 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let wait =
                    std::time::Duration::from_secs(parts.next().and_then(|s| s.parse().ok()).unwrap_or(1));
                let until = std::time::Instant::now()
                    + std::time::Duration::from_secs(parts.next().and_then(|s| s.parse().ok()).unwrap_or(0));
                while std::time::Instant::now() < until {
                    for addr in &addrs {
                        let to = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
                        let began = monotonic_ns();
                        let made = (0..n)
                            .filter(|_| {
                                to.is_some_and(|to| std::net::TcpStream::connect_timeout(&to, wait).is_ok())
                            })
                            .count();
                        out.push_str(&format!(
                            "confined reachround {addr}: {made} of {n} from {began} ns\n"
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
            // `ADDR,N[,SECS]`: N connections to ADDR, each given SECS (1) seconds, and how
            // many were made.
            spec if mode == "reachmany" => {
                use std::net::ToSocketAddrs as _;
                let (addr, rest) = spec.split_once(',').unwrap_or((spec, "0"));
                let (n, secs) = rest.split_once(',').unwrap_or((rest, "1"));
                let n: u32 = n.parse().unwrap_or(0);
                let wait = std::time::Duration::from_secs(secs.parse().unwrap_or(1));
                let to = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
                let mut made = 0u32;
                let began = std::time::Instant::now();
                for _ in 0..n {
                    if let Some(to) = to
                        && std::net::TcpStream::connect_timeout(&to, wait).is_ok()
                    {
                        made += 1;
                    }
                }
                out.push_str(&format!(
                    "confined reachmany {addr}: {made} of {n} in {} ms\n",
                    began.elapsed().as_millis()
                ));
            }
            path if mode == "unix-serve" => {
                let _ = std::fs::remove_file(path);
                let said = match std::os::unix::net::UnixListener::bind(path) {
                    Ok(l) => {
                        let _ = std::thread::Builder::new().spawn(move || {
                            for c in l.incoming().flatten() {
                                let mut c = c;
                                let _ = c.write_all(b"hello\n");
                            }
                        });
                        "ok".to_string()
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined unix-serve {path}: {said}\n"));
            }
            name if mode == "abstract" => {
                let said = match abstract_connect(name) {
                    Ok(()) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined abstract {name}: {said}\n"));
            }
            addr if mode == "dnsprobe" => {
                // A query for api.example's A record, by hand: what answers, if anything.
                // `resolver` asks the one /etc/resolv.conf names.
                let named = std::fs::read_to_string("/etc/resolv.conf")
                    .unwrap_or_default()
                    .lines()
                    .find_map(|l| l.strip_prefix("nameserver ").map(|a| a.trim().to_string()));
                let addr = if addr == "resolver" {
                    named.as_deref().unwrap_or(addr)
                } else {
                    addr
                };
                let mut q = vec![0x51, 0x7a, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
                q.extend_from_slice(b"\x03api\x07example\x00\x00\x01\x00\x01");
                let said = std::net::UdpSocket::bind("0.0.0.0:0")
                    .and_then(|s| {
                        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
                        s.send_to(&q, (addr, 53))?;
                        let mut buf = [0u8; 512];
                        let (n, from) = s.recv_from(&mut buf)?;
                        let a = buf.get(..n).unwrap_or_default();
                        Ok(format!(
                            "from {from} rcode {} answers {}",
                            a.get(3).map_or(0, |b| b & 0x0f),
                            a.get(7).copied().unwrap_or(0)
                        ))
                    })
                    .unwrap_or_else(|e| match e.kind() {
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "timeout".to_string(),
                        _ => errno(&e),
                    });
                out.push_str(&format!("confined dnsprobe {addr}: {said}\n"));
            }
            asked if mode == "dnsask" => {
                // `udp,NAME` or `tcp,NAME`: its A records asked of the resolver
                // /etc/resolv.conf names, by hand, by that protocol: the answer's code,
                // whether it came truncated, and how many answers it holds.
                let (proto, name) = asked.split_once(',').unwrap_or(("udp", asked));
                let server = std::fs::read_to_string("/etc/resolv.conf")
                    .unwrap_or_default()
                    .lines()
                    .find_map(|l| l.strip_prefix("nameserver ").map(|a| a.trim().to_string()))
                    .unwrap_or_default();
                let mut q = vec![0x52, 0x7b, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
                for label in name.split('.') {
                    q.push(u8::try_from(label.len()).unwrap_or(0));
                    q.extend_from_slice(label.as_bytes());
                }
                q.extend_from_slice(&[0, 0, 1, 0, 1]);
                let wait = Some(std::time::Duration::from_secs(3));
                let answer = if proto == "tcp" {
                    std::net::TcpStream::connect_timeout(
                        &std::net::SocketAddr::new(
                            server.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into()),
                            53,
                        ),
                        std::time::Duration::from_secs(3),
                    )
                    .and_then(|mut s| {
                        s.set_read_timeout(wait)?;
                        let len = u16::try_from(q.len()).unwrap_or(0).to_be_bytes();
                        s.write_all(&[&len[..], &q].concat())?;
                        let mut len = [0u8; 2];
                        s.read_exact(&mut len)?;
                        let mut a = vec![0u8; usize::from(u16::from_be_bytes(len))];
                        s.read_exact(&mut a)?;
                        Ok(a)
                    })
                } else {
                    std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
                        s.set_read_timeout(wait)?;
                        s.send_to(&q, (server.as_str(), 53))?;
                        let mut buf = vec![0u8; 65535];
                        let n = s.recv(&mut buf)?;
                        buf.truncate(n);
                        Ok(buf)
                    })
                };
                let said = match answer {
                    Ok(a) => format!(
                        "rcode {} tc {} answers {}",
                        a.get(3).map_or(0, |b| b & 0x0f),
                        a.get(2).map_or(0, |b| (b >> 1) & 1),
                        u16::from_be_bytes([a.get(6).copied().unwrap_or(0), a.get(7).copied().unwrap_or(0)])
                    ),
                    Err(e) => match e.kind() {
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "timeout".to_string(),
                        _ => errno(&e),
                    },
                };
                out.push_str(&format!("confined dnsask {proto} {name}: {said}\n"));
            }
            name if mode == "resolve" => {
                let c = std::ffi::CString::new(name).unwrap_or_default();
                let mut res: *mut libc::addrinfo = std::ptr::null_mut();
                // SAFETY: getaddrinfo(3) of a NUL-terminated name, its list freed after.
                let mut rcs = Vec::new();
                for stream in [false, true] {
                    // SAFETY: an all-zero addrinfo is valid hints.
                    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
                    hints.ai_socktype = if stream { libc::SOCK_STREAM } else { 0 };
                    // SAFETY: getaddrinfo(3) of a NUL-terminated name, its list freed after.
                    let rc = unsafe {
                        let rc =
                            libc::getaddrinfo(c.as_ptr(), std::ptr::null(), &raw const hints, &raw mut res);
                        if rc == 0 {
                            libc::freeaddrinfo(res);
                        }
                        rc
                    };
                    rcs.push(rc.to_string());
                }
                out.push_str(&format!("confined resolve {name}: {}\n", rcs.join(",")));
            }
            path if mode == "cat" => match std::fs::read_to_string(path) {
                Ok(text) => {
                    for line in text.lines() {
                        out.push_str(&format!("confined cat {path}: {line}\n"));
                    }
                }
                Err(e) => out.push_str(&format!("confined cat {path}: {}\n", errno(&e))),
            },
            port if mode == "listen" => {
                // Both IP versions where the domain has IPv6, else IPv4 alone.
                let said = match std::net::TcpListener::bind(format!("[::]:{port}"))
                    .or_else(|_| std::net::TcpListener::bind(format!("0.0.0.0:{port}")))
                {
                    Ok(l) => {
                        // Answers every connection with its host name, for as long as the
                        // agent runs, so that who answered is known.
                        let hello = format!("hello from {host}\n");
                        let _ = std::thread::Builder::new().spawn(move || {
                            for c in l.incoming().flatten() {
                                let mut c = c;
                                let _ = c.write_all(hello.as_bytes());
                            }
                        });
                        "ok".to_string()
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined listen {port}: {said}\n"));
            }
            // What ADDR says, read to its end: the host's word, which holds what follows
            // until the host has made ready what it is for.
            addr if mode == "hear" => {
                let heard = std::net::TcpStream::connect(addr).and_then(|mut c| {
                    let mut text = String::new();
                    c.read_to_string(&mut text).map(|_| text)
                });
                let said = match heard {
                    Ok(text) => text.trim_end().to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined hear {addr}: {said}\n"));
            }
            addr if mode == "reach" || mode == "unreach" => {
                use std::net::ToSocketAddrs as _;
                // `NET1` is the first address of its own network, its gateway there.
                let addr = addr.replace("NET1", &first_of_own());
                let addr = addr.as_str();
                let once = |wait: u64| -> Result<(), io::Error> {
                    let to = addr
                        .to_socket_addrs()?
                        .next()
                        .ok_or_else(|| io::Error::other("no address"))?;
                    std::net::TcpStream::connect_timeout(&to, std::time::Duration::from_secs(wait)).map(drop)
                };
                let said = |r: Result<(), io::Error>| match r {
                    Ok(()) => "ok".to_string(),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => "timeout".to_string(),
                    Err(e) if e.raw_os_error().is_none() => format!("{e}"),
                    Err(e) => errno(&e),
                };
                let result = if mode == "reach" {
                    // Until it listens: a refusal still says packets arrive.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let r = once(2);
                        if r.is_ok() || std::time::Instant::now() >= deadline {
                            break r;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                } else {
                    once(3)
                };
                out.push_str(&format!("confined {mode} {addr}: {}\n", said(result)));
            }
            what if mode == "call" => {
                // SAFETY: each a system call with constant or null arguments, its
                // descriptor, if any, closed after.
                let (rc, err) = unsafe {
                    let rc = match what {
                        "socket-vsock" => libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) as libc::c_long,
                        "socket-netlink" => libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, 0) as libc::c_long,
                        "socket-unix" => libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) as libc::c_long,
                        "socket-inet6" => libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) as libc::c_long,
                        "io_uring_setup" => {
                            libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null_mut::<u8>())
                        }
                        // KEYCTL_GET_KEYRING_ID of the session keyring, made if absent.
                        "keyctl" => libc::syscall(libc::SYS_keyctl, 0, -3i32, 1),
                        "userfaultfd" => libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC),
                        // What spoofing another's address would need, and leaving its own
                        // network namespace.
                        "socket-raw" => {
                            libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_UDP) as libc::c_long
                        }
                        "unshare-net" => libc::c_long::from(libc::unshare(libc::CLONE_NEWNET)),
                        "fork" => {
                            let pid = libc::fork();
                            if pid == 0 {
                                libc::_exit(0);
                            }
                            if pid > 0 {
                                libc::waitpid(pid, std::ptr::null_mut(), 0);
                                0
                            } else {
                                -1
                            }
                        }
                        "thread" => match std::thread::Builder::new().spawn(|| ()) {
                            Ok(t) => i64::from(t.join().is_ok()) - 1,
                            Err(_) => -1,
                        },
                        // Each other way to start a process: vfork, a clone that is no
                        // thread, clone3, and a spawn as libc makes it.
                        "vfork" => match vfork_and_exit() {
                            Ok(pid) => reaped(pid),
                            Err(e) => {
                                *libc::__errno_location() = e;
                                -1
                            }
                        },
                        "clone" => {
                            let pid = libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0);
                            if pid == 0 {
                                libc::_exit(0);
                            }
                            reaped(pid as libc::pid_t)
                        }
                        "clone3" => {
                            // struct clone_args (include/uapi/linux/sched.h): as fork(2).
                            let mut args = [0u64; 11];
                            args[4] = libc::SIGCHLD as u64;
                            let pid = libc::syscall(
                                libc::SYS_clone3,
                                args.as_mut_ptr(),
                                std::mem::size_of_val(&args),
                            );
                            if pid == 0 {
                                libc::_exit(0);
                            }
                            reaped(pid as libc::pid_t)
                        }
                        "spawn" => match std::process::Command::new("/proc/self/exe")
                            .args(["exit", "0"])
                            .status()
                        {
                            Ok(_) => 0,
                            Err(e) => {
                                *libc::__errno_location() = e.raw_os_error().unwrap_or(0);
                                -1
                            }
                        },
                        _ => -1,
                    };
                    (rc, io::Error::last_os_error())
                };
                let said = if rc >= 0 {
                    if !matches!(
                        what,
                        "keyctl" | "fork" | "thread" | "unshare-net" | "vfork" | "clone" | "clone3" | "spawn"
                    ) {
                        // SAFETY: closing the descriptor just made.
                        unsafe { libc::close(rc as libc::c_int) };
                    }
                    "ok".to_string()
                } else {
                    errno(&err)
                };
                out.push_str(&format!("confined call {what}: {said}\n"));
            }
            addr if mode == "bind" => {
                let said = match std::net::TcpListener::bind(addr) {
                    Ok(_) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined bind {addr}: {said}\n"));
            }
            addr if mode == "connect" => {
                let said = match std::net::TcpStream::connect(addr) {
                    Ok(_) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined connect {addr}: {said}\n"));
            }
            // A local MCP server the in-VM server offers, run as its caller runs one: a
            // child of this agent, in its confinement; each line it says.
            name if mode == "mcp-run" => {
                let said = server_call("mcp", "{}")
                    .map_err(|e| format!("error {e}"))
                    .and_then(|(t, refused)| {
                        if refused {
                            return Err(format!("refused {t}"));
                        }
                        offered_command(&t, name).ok_or_else(|| format!("no local server {name} in {t}"))
                    });
                let lines = match said {
                    Ok(command) => match command.split_first() {
                        Some((program, args)) => {
                            match std::process::Command::new(program).args(args).output() {
                                Ok(o) => String::from_utf8_lossy(&o.stdout)
                                    .lines()
                                    .map(str::to_string)
                                    .collect(),
                                Err(e) => vec![errno(&e)],
                            }
                        }
                        None => vec!["no command".to_string()],
                    },
                    Err(e) => vec![e],
                };
                for line in lines {
                    out.push_str(&format!("confined mcp-run {name}: {line}\n"));
                }
            }
            // A symlink it plants, `LINK=TARGET`, followed: what it reads through it.
            pair if mode == "follow" => {
                let (link, target) = pair.split_once('=').unwrap_or((pair, ""));
                let _ = std::fs::remove_file(link);
                let said = match std::os::unix::fs::symlink(target, link).and_then(|()| std::fs::read(link)) {
                    Ok(b) => format!("ok {} bytes", b.len()),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined follow {pair}: {said}\n"));
            }
            // A hard link it makes, `LINK=TARGET`.
            pair if mode == "hardlink" => {
                let (link, target) = pair.split_once('=').unwrap_or((pair, ""));
                let said = match std::fs::hard_link(target, link) {
                    Ok(()) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined hardlink {pair}: {said}\n"));
            }
            // A program it runs, asked who it runs as.
            path if mode == "exec" => {
                let said = match std::process::Command::new(path).arg("creds").output() {
                    Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined exec {path}: {said}\n"));
            }
            // Whether a path's filesystem ignores set-ID bits.
            path if mode == "nosuid" => {
                let said = match std::ffi::CString::new(path) {
                    Ok(c) => {
                        // SAFETY: an all-zero statvfs is a valid out-parameter, which
                        // statvfs(3) fills, of a NUL-terminated path.
                        let (rc, s) = unsafe {
                            let mut s: libc::statvfs = std::mem::zeroed();
                            (libc::statvfs(c.as_ptr(), &mut s), s)
                        };
                        if rc != 0 {
                            errno(&io::Error::last_os_error())
                        } else if s.f_flag & libc::ST_NOSUID != 0 {
                            "yes".to_string()
                        } else {
                            "no".to_string()
                        }
                    }
                    Err(_) => "errno 22".to_string(),
                };
                out.push_str(&format!("confined nosuid {path}: {said}\n"));
            }
            path if mode == "owner" => {
                use std::os::unix::fs::MetadataExt as _;
                let said = match std::fs::metadata(path) {
                    Ok(m) => format!("{}:{} {:o}", m.uid(), m.gid(), m.mode() & 0o7777),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined owner {path}: {said}\n"));
            }
            // The bytes of the file system at `path`, as statvfs(3) says them.
            path if mode == "size" => {
                let said = match std::ffi::CString::new(path) {
                    Ok(c) => {
                        // SAFETY: a struct of plain integers, for statvfs to fill.
                        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
                        // SAFETY: statvfs(3) of a NUL-terminated path into `st`.
                        if unsafe { libc::statvfs(c.as_ptr(), &raw mut st) } == 0 {
                            st.f_blocks.saturating_mul(st.f_frsize).to_string()
                        } else {
                            errno(&io::Error::last_os_error())
                        }
                    }
                    Err(_) => "errno 22".to_string(),
                };
                out.push_str(&format!("confined size {path}: {said}\n"));
            }
            path if mode == "see" => {
                let said = match std::fs::read_dir(path) {
                    Ok(d) => {
                        let mut names: Vec<String> = d
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect();
                        names.sort();
                        names.join(",")
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined see {path}: {said}\n"));
            }
            path => {
                let said = match std::fs::write(path, "x") {
                    Ok(()) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined write {path}: {said}\n"));
            }
        }
    }
    let ifaces: Vec<String> = std::fs::read_to_string("/proc/net/dev")
        .unwrap_or_default()
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next().map(|n| n.trim().to_string()))
        .collect();
    out.push_str(&format!("confined net={}\n", ifaces.join(",")));
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    // SAFETY: prctl(2) naming this thread, with a NUL-terminated name of 15 bytes or fewer.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"confined-ready".as_ptr()) };
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

/// The command of local MCP server `name` in the in-VM server's `mcp` answer (a JSON
/// array of objects, as shards-init writes it): its strings, unescaped.
fn offered_command(json: &str, name: &str) -> Option<Vec<String>> {
    let at = json.find(&format!("\"name\":\"{name}\",\"remote\":false"))?;
    let rest = json.get(at..)?;
    let list = rest.get(rest.find("\"command\":[")? + "\"command\":[".len()..)?;
    let mut out = Vec::new();
    let mut chars = list.chars();
    loop {
        match chars.next()? {
            ']' => return Some(out),
            ',' | ' ' => {}
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            'u' => {
                                let hex: String = chars.by_ref().take(4).collect();
                                s.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                            }
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            c => s.push(c),
                        },
                        c => s.push(c),
                    }
                }
                out.push(s);
            }
            _ => return None,
        }
    }
}

/// Set by SIGUSR1's handler: the host's go-ahead came before [`await_process`] blocked it.
static GONE_AHEAD: AtomicBool = AtomicBool::new(false);

extern "C" fn go_ahead(_: libc::c_int) {
    GONE_AHEAD.store(true, Ordering::SeqCst);
}

/// Waits for the host's go-ahead, SIGUSR1, which it sends once `count` of the microVM's
/// processes are named `name` (`shards top`; the workload sees none of its agents', its
/// PID namespace being its own, D115), none for a count of 0; then, where an address is
/// given, says whether this process reaches it, in 3 s. A minute without one is a timeout.
/// The signal is caught first, which the host waits to see (`shards top -o caught`): a
/// process that neither handles nor blocks it would lose it as its PID namespace's first
/// (kernel/signal.c, `sig_task_ignored`); then blocked and waited for, unless it came
/// between the two.
fn await_process(name: &str, count: usize, then: &str) -> i32 {
    // SAFETY: sigaction(2) of a handler that only stores to an atomic, async-signal-safe;
    // a signal set of our own, SIGUSR1 blocked, then waited for, a minute at most.
    let got = (count > 0).then(|| unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = go_ahead as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGUSR1, &act, std::ptr::null_mut());
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if GONE_AHEAD.load(Ordering::SeqCst) {
            libc::SIGUSR1
        } else {
            let minute = libc::timespec {
                tv_sec: 60,
                tv_nsec: 0,
            };
            libc::sigtimedwait(&set, std::ptr::null_mut(), &minute)
        }
    });
    if got.is_some_and(|s| s != libc::SIGUSR1) {
        let _ = writeln!(io::stdout(), "await timeout {name}");
        return 1;
    }
    if !then.is_empty() {
        use std::net::ToSocketAddrs as _;
        let said = match then.to_socket_addrs().map(|mut a| a.next()) {
            Ok(Some(to)) => {
                match std::net::TcpStream::connect_timeout(&to, std::time::Duration::from_secs(3)) {
                    Ok(_) => "ok".to_string(),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => "timeout".to_string(),
                    Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
                }
            }
            Ok(None) => "no address".to_string(),
            Err(e) => e.to_string(),
        };
        let _ = writeln!(io::stdout(), "await unreach {then}: {said}");
    }
    0
}

/// An abstract Unix socket's address (unix(7)): a zero byte, then the name.
fn abstract_addr(name: &str) -> (libc::sockaddr_un, libc::socklen_t) {
    // SAFETY: all-zero is a valid sockaddr_un.
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, b) in a.sun_path.iter_mut().skip(1).zip(name.bytes()) {
        *slot = b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len();
    (a, len as libc::socklen_t)
}

/// A listening abstract Unix socket named `name`, closed when its descriptor is.
fn abstract_listener(name: &str) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;
    let (a, len) = abstract_addr(name);
    // SAFETY: socket(2), bind(2) and listen(2) of a descriptor owned from here on.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = std::os::fd::OwnedFd::from_raw_fd(fd);
        if libc::bind(fd, (&raw const a).cast(), len) != 0 || libc::listen(fd, 8) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(owned)
    }
}

fn abstract_connect(name: &str) -> io::Result<()> {
    let (a, len) = abstract_addr(name);
    // SAFETY: socket(2) and connect(2), the descriptor closed after.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = libc::connect(fd, (&raw const a).cast(), len);
        let e = io::Error::last_os_error();
        libc::close(fd);
        if r != 0 { Err(e) } else { Ok(()) }
    }
}

/// CLOCK_MONOTONIC, in nanoseconds: one clock for every process of the microVM.
fn monotonic_ns() -> u128 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) into a timespec it owns.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut t) };
    u128::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u128::try_from(t.tv_nsec).unwrap_or(0)
}

/// An agent filling its scratch: this process, its PID 1, named `held`, holds nothing and
/// waits, so that only a kill of its whole domain ends it (the kernel ends a PID namespace
/// with its PID 1, which a kill of the filler alone would not reach); its child, named
/// `filling`, writes `/tmp/fill` a MiB at a time and says each, until a write fails, when
/// it says so, is named `fill-stopped` and holds; or until it is killed. The child holds 4
/// MiB more, so that the OOM killer's choice by resident memory is it.
fn fill_scratch() -> i32 {
    // SAFETY: fork(2) in a process of one thread; the parent only names itself and waits.
    match unsafe { libc::fork() } {
        0 => {}
        pid if pid < 0 => return 1,
        // SAFETY: prctl(2) with a NUL-terminated name; pause(2) until a signal ends it.
        _ => unsafe {
            libc::prctl(libc::PR_SET_NAME, c"held".as_ptr());
            loop {
                libc::pause();
            }
        },
    }
    // SAFETY: prctl(2) with a NUL-terminated name.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"filling".as_ptr()) };
    let resident: Vec<u8> = vec![1; 4 << 20];
    let chunk = vec![b'f'; 1 << 20];
    let mut file = match std::fs::File::create("/tmp/fill") {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(
                io::stdout(),
                "confined fill: errno {}",
                e.raw_os_error().unwrap_or(0)
            );
            return 1;
        }
    };
    let mut mib = 0u64;
    loop {
        if let Err(e) = file.write_all(&chunk) {
            let _ = writeln!(
                io::stdout(),
                "confined fill: errno {}",
                e.raw_os_error().unwrap_or(0)
            );
            break;
        }
        mib += 1;
        let _ = writeln!(io::stdout(), "confined filled {mib}");
        let _ = io::stdout().flush();
    }
    std::hint::black_box(&resident);
    // SAFETY: prctl(2) with a NUL-terminated name; pause(2) until a signal ends it.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"fill-stopped".as_ptr());
        loop {
            libc::pause();
        }
    }
}

/// What the run's own command sees of the microVM's processes (D115): each its `/proc`
/// lists, with its name, whether it is this one, and whether this process may signal it,
/// read its environment and open its network namespace; then this process's capabilities.
fn beside() -> i32 {
    let me = std::process::id();
    let errno = |e: &io::Error| format!("errno {}", e.raw_os_error().unwrap_or(0));
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        // SAFETY: kill(2) with signal 0, which checks permission and sends nothing.
        let signal = if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            "ok".to_string()
        } else {
            errno(&io::Error::last_os_error())
        };
        let open = |p: &str| match std::fs::File::open(e.path().join(p)) {
            Ok(_) => "ok".to_string(),
            Err(err) => errno(&err),
        };
        let _ = writeln!(
            io::stdout(),
            "beside {pid} {} self={} signal={signal} environ={} netns={}",
            comm.trim_end(),
            pid == me,
            open("environ"),
            open("ns/net"),
        );
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .map(str::trim)
        .unwrap_or_default();
    let _ = writeln!(io::stdout(), "beside caps {eff}");
    0
}

/// The run's own command, outliving agents that fill their scratch: it holds `hold` MiB
/// resident until the host, which watches the agents (`shards top`, D115), says it may go
/// (SIGUSR1, a minute at most).
fn outlive(hold: usize) -> i32 {
    let held: Vec<u8> = vec![1; hold << 20];
    let _ = writeln!(io::stdout(), "outlive holding {hold}");
    let _ = io::stdout().flush();
    let code = await_process("filling", 1, "");
    std::hint::black_box(&held);
    code
}

/// The first IPv4 address of an interface other than loopback, or `none`.
fn own_address() -> String {
    // SAFETY: getifaddrs(3) into a list freed below; each entry read while it lives.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return "none".to_string();
        }
        let mut found = "none".to_string();
        let mut at = list;
        while !at.is_null() {
            let a = (*at).ifa_addr;
            if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET {
                let sin = &*a.cast::<libc::sockaddr_in>();
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() {
                    found = ip.to_string();
                    break;
                }
            }
            at = (*at).ifa_next;
        }
        libc::freeifaddrs(list);
        found
    }
}

/// The default route's gateway, or `none`: `/proc/net/route` writes the address's bytes,
/// in network order, as a native-endian number in hex (net/ipv4/fib_trie.c, `%08X`).
fn gateway() -> String {
    std::fs::read_to_string("/proc/net/route")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.get(1) != Some(&"00000000") {
                return None;
            }
            let g = u32::from_str_radix(f.get(2)?, 16).ok()?;
            Some(std::net::Ipv4Addr::from(g.to_ne_bytes()).to_string())
        })
        .unwrap_or_else(|| "none".to_string())
}

/// Its own network's gateway, the switch's end of its link: the gateway of its first
/// route to a network (`/proc/net/route`, as [`gateway`] reads it), an agent reaching its
/// peers through the switch; `none` where it has none. Read there, not through
/// `getifaddrs`, which asks over netlink, which an agent may not open.
fn first_of_own() -> String {
    std::fs::read_to_string("/proc/net/route")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.get(1) == Some(&"00000000") || f.get(2) == Some(&"00000000") {
                return None;
            }
            let g = u32::from_str_radix(f.get(2)?, 16).ok()?;
            Some(std::net::Ipv4Addr::from(g.to_ne_bytes()).to_string())
        })
        .unwrap_or_else(|| "none".to_string())
}

/// The System V key §9.8's channel probes use: "SHAR".
const IPC_KEY: libc::key_t = 0x5348_4152;

/// A connection to the in-VM server, as an MCP client makes one: over the agent's socket and
/// mutual TLS 1.3 with the certificate and key beside it; and the session it is in, if the
/// server began one, which each request carries (MCP 2025-06-18, "Session Management").
struct Session {
    s: rustls::StreamOwned<rustls::ClientConnection, std::os::unix::net::UnixStream>,
    id: Option<String>,
}

impl Session {
    /// A connection, its session begun with MCP's `initialize`.
    fn open() -> Result<Session, String> {
        let mut session = Session::connect()?;
        session.post(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"testguest","version":"1"}}}"#,
        )?;
        let (head, _) = session.answered()?;
        session.id = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("mcp-session-id")
                .then(|| v.trim().to_string())
        });
        Ok(session)
    }

    /// Another connection, in this one's session.
    fn join(&self) -> Result<Session, String> {
        let mut other = Session::connect()?;
        other.id = self.id.clone();
        Ok(other)
    }

    fn connect() -> Result<Session, String> {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
        use std::sync::Arc;
        let at = "/run/shards";
        let e = |what: &str, x: &dyn std::fmt::Display| format!("{what}: {x}");
        let ca = CertificateDer::from_pem_file(format!("{at}/ca.pem")).map_err(|x| e("ca.pem", &x))?;
        let cert = CertificateDer::from_pem_file(format!("{at}/cert.pem")).map_err(|x| e("cert.pem", &x))?;
        let key = PrivateKeyDer::from_pem_file(format!("{at}/key.pem")).map_err(|x| e("key.pem", &x))?;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).map_err(|x| e("ca", &x))?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|x| e("tls", &x))?
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)
            .map_err(|x| e("client cert", &x))?;
        let name = ServerName::try_from("shards").map_err(|x| e("name", &x))?;
        let tls = rustls::ClientConnection::new(Arc::new(config), name).map_err(|x| e("tls", &x))?;
        let sock = std::os::unix::net::UnixStream::connect(format!("{at}/server.sock"))
            .map_err(|x| format!("errno {}", x.raw_os_error().unwrap_or(0)))?;
        Ok(Session {
            s: rustls::StreamOwned::new(tls, sock),
            id: None,
        })
    }

    /// Posts `body` with `headers` (each ending CRLF), and none of a session's.
    fn post_with(&mut self, body: &str, headers: &str) -> Result<(), String> {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: shards\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.s
            .write_all(req.as_bytes())
            .map_err(|x| format!("write: {x}"))
    }

    /// Posts `body`, a JSON-RPC message, to `/mcp`, in this connection's session.
    fn post(&mut self, body: &str) -> Result<(), String> {
        let session = self
            .id
            .as_ref()
            .map(|id| format!("Mcp-Session-Id: {id}\r\nMCP-Protocol-Version: 2025-06-18\r\n"))
            .unwrap_or_default();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: shards\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.s
            .write_all(req.as_bytes())
            .map_err(|x| format!("write: {x}"))
    }

    /// The next answer's body.
    fn answer(&mut self) -> Result<String, String> {
        self.answered().map(|(_, body)| body)
    }

    /// The next answer: its head, and its body.
    fn answered(&mut self) -> Result<(String, String), String> {
        let mut got = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            let n = self.s.read(&mut b).map_err(|x| format!("read: {x}"))?;
            if n == 0 {
                return Err("closed".to_string());
            }
            got.extend_from_slice(b.get(..n).unwrap_or_default());
            let text = String::from_utf8_lossy(&got).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if body.len() >= len {
                    return Ok((head.to_string(), body.to_string()));
                }
            }
        }
    }
}

/// `values`, sorted: n, and the nearest-rank p50, p90, p99 and max, in µs.
fn summary_us(values: &mut [f64]) -> String {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let rank = |p: usize| {
        values
            .get((n * p).div_ceil(100).clamp(1, n.max(1)) - 1)
            .copied()
            .unwrap_or(0.0)
    };
    format!(
        "n={n} p50={:.0} p90={:.0} p99={:.0} max={:.0}",
        rank(50),
        rank(90),
        rank(99),
        rank(100)
    )
}

/// A request of MCP 2026-07-28, `method` with `params` (a JSON object's fields, or none):
/// its body, its `_meta` saying `version`, and its headers, as a client of that revision
/// sends them.
fn modern_request(method: &str, params: &str, version: &str) -> (String, String) {
    let meta = format!(
        r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"{version}","io.modelcontextprotocol/clientInfo":{{"name":"testguest","version":"1"}},"io.modelcontextprotocol/clientCapabilities":{{}}}}"#
    );
    let params = if params.is_empty() {
        meta
    } else {
        format!("{params},{meta}")
    };
    let body = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{method}","params":{{{params}}}}}"#);
    let mut headers = format!("MCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n");
    if let Some(name) = params
        .split(r#""name":""#)
        .nth(1)
        .and_then(|r| r.split('"').next())
    {
        headers.push_str(&format!("Mcp-Name: {name}\r\n"));
    }
    (body, headers)
}

/// The in-VM server asked as MCP 2026-07-28 asks: each answer's status line and body, a
/// connection each, with what a client of that revision sends and what it must not.
fn modern_asks() -> Vec<(&'static str, String)> {
    let ask = |body: &str, headers: &str| -> String {
        (|| -> Result<String, String> {
            let mut s = Session::connect()?;
            s.post_with(body, headers)?;
            let (head, body) = s.answered()?;
            Ok(format!("{} {body}", head.lines().next().unwrap_or_default()))
        })()
        .unwrap_or_else(|e| format!("error {e}"))
    };
    let (discover, h) = modern_request("server/discover", "", "2026-07-28");
    let mut asks = vec![("discover", ask(&discover, &h))];
    let (list, h) = modern_request("tools/list", "", "2026-07-28");
    let listed = ask(&list, &h);
    let (tools, tail) = listed.rsplit_once(']').unwrap_or_default();
    asks.push((
        "list",
        format!("{} tools]{tail}", tools.matches(r#""inputSchema""#).count()),
    ));
    let (whoami, h) = modern_request("tools/call", r#""name":"whoami","arguments":{}"#, "2026-07-28");
    asks.push(("whoami", ask(&whoami, &h)));
    let (run, h) = modern_request(
        "tools/call",
        r#""name":"run","arguments":{"code":"return (await shards.whoami()).length;"}"#,
        "2026-07-28",
    );
    asks.push(("run", ask(&run, &h)));
    // Its headers must say what its body does.
    let (whoami, _) = modern_request("tools/call", r#""name":"whoami","arguments":{}"#, "2026-07-28");
    asks.push((
        "method-mismatch",
        ask(
            &whoami,
            "MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list\r\nMcp-Name: whoami\r\n",
        ),
    ));
    asks.push((
        "name-base64",
        ask(
            &whoami,
            "MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: =?base64?d2hvYW1p?=\r\n",
        ),
    ));
    asks.push((
        "no-version-header",
        ask(&whoami, "Mcp-Method: tools/call\r\nMcp-Name: whoami\r\n"),
    ));
    let (old, h) = modern_request("tools/list", "", "1900-01-01");
    asks.push(("unsupported", ask(&old, &h)));
    let (unknown, h) = modern_request("prompts/list", "", "2026-07-28");
    asks.push(("unknown", ask(&unknown, &h)));
    let no_caps = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    asks.push((
        "no-capabilities",
        ask(
            no_caps,
            "MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list\r\n",
        ),
    ));
    let (list, h) = modern_request("tools/list", "", "2026-07-28");
    asks.push((
        "origin",
        ask(&list, &format!("{h}Origin: https://example.com\r\n")),
    ));
    // A revision before it still begins a session.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"testguest","version":"1"}}}"#;
    asks.push((
        "legacy",
        (|| -> Result<String, String> {
            let mut s = Session::connect()?;
            s.post_with(init, "")?;
            let (head, body) = s.answered()?;
            Ok(format!(
                "{} session={} revision={}",
                head.lines().next().unwrap_or_default(),
                head.lines()
                    .any(|l| l.to_ascii_lowercase().starts_with("mcp-session-id: ")),
                body.contains(r#""protocolVersion":"2025-11-25""#)
            ))
        })()
        .unwrap_or_else(|e| format!("error {e}")),
    ));
    asks
}

/// `tools/call` of `tool` with `args`, request 2.
fn tool_call(tool: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#
    )
}

/// Calls the in-VM server's `tool` with `args` (a JSON object), as an MCP client calls
/// one, in a session of its own. Its text, and whether it is an error.
fn server_call(tool: &str, args: &str) -> Result<(String, bool), String> {
    let mut session = Session::open()?;
    session.post(&tool_call(tool, args))?;
    let answer = session.answer()?;
    let at = answer
        .find(r#""text":""#)
        .map(|i| i + r#""text":""#.len())
        .ok_or_else(|| format!("no text in {answer}"))?;
    Ok((
        unquote(answer.get(at..).unwrap_or_default()),
        answer.contains(r#""isError":true"#),
    ))
}

/// The JSON string whose text starts `s` (past its opening quote), unescaped: up to its
/// closing quote.
fn unquote(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// `s` as a JSON string.
fn quote_json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // Control characters escaped, as RFC 8259 §7 requires of a string.
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An SBOM generator of BuildKit's protocol (docs/attestations/sbom-protocol.md): for
/// `BUILDKIT_SCAN_SOURCE` and each directory in `BUILDKIT_SCAN_SOURCE_EXTRAS`, a statement
/// in `BUILDKIT_SCAN_DESTINATION/<name>.spdx.json` of an SPDX document naming the target,
/// listing its regular files, and the `BUILDKIT_SCAN_*` parameters it was given; with a
/// note of characters Go escapes, written as they are.
fn sbom_scan() -> i32 {
    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    fn files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            let p = e.path();
            if t.is_dir() {
                files(root, &p, out);
            } else if t.is_file()
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let dest = var("BUILDKIT_SCAN_DESTINATION");
    let mut targets = vec![std::path::PathBuf::from(var("BUILDKIT_SCAN_SOURCE"))];
    let extras = var("BUILDKIT_SCAN_SOURCE_EXTRAS");
    if !extras.is_empty()
        && let Ok(entries) = std::fs::read_dir(&extras)
    {
        let mut more: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
        more.sort();
        targets.extend(more);
    }
    let mut params: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("BUILDKIT_SCAN_"))
        .collect();
    params.sort();
    let params: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}:{}", quote(k), quote(v)))
        .collect();
    for t in targets {
        let name = t
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut list = Vec::new();
        files(&t, &t, &mut list);
        list.sort();
        let list: Vec<String> = list.iter().map(|f| quote(f)).collect();
        let text = format!(
            "{{\n  \"_type\": \"https://in-toto.io/Statement/v0.1\",\n  \"predicateType\": \"https://spdx.dev/Document\",\n  \"predicate\": {{\"name\": {}, \"files\": [{}], \"params\": {{{}}}, \"note\": \"a<b & c>d\"}}\n}}\n",
            quote(&name),
            list.join(", "),
            params.join(", ")
        );
        if std::fs::write(
            std::path::Path::new(&dest).join(format!("{name}.spdx.json")),
            text,
        )
        .is_err()
        {
            return 1;
        }
    }
    0
}

/// `N:DIR:ROUNDS`: DIR's files `f0` to `fN-1` held open at once, past the page cache
/// (O_DIRECT), and read 4 KiB at a time round-robin, ROUNDS times after one round untimed;
/// prints `roundrobin n N p50 P p90 P p99 P max M ns` of the reads (audit V09: a share's
/// cache of descriptors at its worst, each read opening its file again).
fn roundrobin(spec: &str) -> io::Result<()> {
    let mut parts = spec.splitn(3, ':');
    let bad = || io::Error::other("a malformed roundrobin");
    let n: usize = parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let dir = parts.next().ok_or_else(bad)?;
    let rounds: usize = parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let room = libc::rlim_t::try_from(n + 64).map_err(|_| bad())?;
    let lim = libc::rlimit {
        rlim_cur: room,
        rlim_max: room,
    };
    // SAFETY: setrlimit(2) of this process's own limit, from a local.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::with_capacity(n);
    for i in 0..n {
        let path = std::ffi::CString::new(format!("{dir}/f{i}")).map_err(|_| bad())?;
        // SAFETY: open(2) of a NUL-terminated path.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        fds.push(fd);
    }
    // A page-aligned buffer, as O_DIRECT may ask.
    let mut space = vec![0u8; 8192];
    let skip = space.as_ptr().align_offset(4096);
    let buf = space.get_mut(skip..skip + 4096).ok_or_else(bad)?;
    let mut ns = Vec::with_capacity(n * rounds);
    for round in 0..=rounds {
        for &fd in &fds {
            let t0 = monotonic_ns();
            // SAFETY: pread(2) into a buffer of the length given.
            let got = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
            let took = monotonic_ns().saturating_sub(t0);
            if got < 0 {
                return Err(io::Error::last_os_error());
            }
            if round > 0 {
                ns.push(took);
            }
        }
    }
    for fd in fds {
        // SAFETY: close(2) of a descriptor this opened.
        unsafe { libc::close(fd) };
    }
    ns.sort_unstable();
    let at = |q: usize| {
        ns.get((ns.len().saturating_sub(1)) * q / 100)
            .copied()
            .unwrap_or(0)
    };
    let max = ns.last().copied().unwrap_or(0);
    let _ = writeln!(
        io::stdout(),
        "roundrobin n {} p50 {} p90 {} p99 {} max {max} ns",
        ns.len(),
        at(50),
        at(90),
        at(99)
    );
    Ok(())
}

/// Says each of shards_testguest::LIMITS, `SHARDS-TEST LIMIT NAME VALUE`, and the
/// process's RLIMIT_NPROC and RLIMIT_SIGPENDING, soft and hard.
pub fn limits() -> Result<(), String> {
    let mut out = io::stdout().lock();
    for name in shards_testguest::LIMITS {
        let path = format!("/proc/sys/{}", name.replace('.', "/"));
        let value = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let value: Vec<&str> = value.split_whitespace().collect();
        let _ = writeln!(out, "SHARDS-TEST LIMIT {name} {}", value.join(","));
    }
    for (name, resource) in [
        ("nproc", libc::RLIMIT_NPROC),
        ("sigpending", libc::RLIMIT_SIGPENDING),
    ] {
        let mut l = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) fills `l`.
        if unsafe { libc::getrlimit(resource, &mut l) } != 0 {
            return Err(format!("getrlimit {name}: {}", io::Error::last_os_error()));
        }
        let _ = writeln!(
            out,
            "SHARDS-TEST LIMIT rlimit.{name} {},{}",
            l.rlim_cur, l.rlim_max
        );
    }
    Ok(())
}

/// Writes `mib` MiB to `path`, a nonzero byte at each page so that none is a hole.
fn big(path: &str, mib: u64) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    let mut chunk = vec![0u8; 1 << 20];
    for page in chunk.chunks_mut(4096) {
        if let Some(b) = page.first_mut() {
            *b = 1;
        }
    }
    for _ in 0..mib {
        file.write_all(&chunk)?;
    }
    file.sync_all()
}

/// Says `ns`, the IPC namespace this process is in; `shm BYTES` for its /dev/shm, or `shm
/// none` where /dev/shm is no mount of its own; and, for `key`, `sysv KEY` and `file KEY`,
/// each `yes` or the errno of why not.
fn ipc_report(key: &str) -> i32 {
    use std::os::unix::fs::MetadataExt as _;
    let ns =
        std::fs::read_link("/proc/self/ns/ipc").map_or_else(|e| e.to_string(), |p| p.display().to_string());
    let mut out = format!("ns {ns}\n");
    let mounted = match (std::fs::metadata("/dev/shm"), std::fs::metadata("/dev")) {
        (Ok(shm), Ok(dev)) => shm.dev() != dev.dev(),
        _ => false,
    };
    // SAFETY: all-zero is a valid statfs, which statfs(2) fills.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs(2) of a NUL-terminated literal into our own struct.
    if mounted && unsafe { libc::statfs(c"/dev/shm".as_ptr(), &raw mut fs) } == 0 {
        out.push_str(&format!("shm {}\n", fs.f_blocks as u64 * fs.f_bsize as u64));
    } else {
        out.push_str("shm none\n");
    }
    if let Ok(k) = key.parse::<libc::key_t>() {
        // SAFETY: shmget(2) of an existing segment, made by none here.
        let sysv = if unsafe { libc::shmget(k, 0, 0) } >= 0 {
            "yes".to_string()
        } else {
            format!("errno {}", io::Error::last_os_error().raw_os_error().unwrap_or(0))
        };
        let file = match std::fs::metadata(format!("/dev/shm/{key}")) {
            Ok(_) => "yes".to_string(),
            Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
        };
        out.push_str(&format!("sysv {key} {sysv}\nfile {key} {file}\n"));
    }
    let _ = write!(io::stdout(), "{out}");
    0
}

/// Makes System V segment `key` and file /dev/shm/`key`, and says `held` once both are.
fn ipc_hold(key: &str) -> i32 {
    let Ok(k) = key.parse::<libc::key_t>() else {
        let _ = writeln!(io::stdout(), "ipc-hold: a key is a number");
        return 2;
    };
    // SAFETY: shmget(2) making a segment of one page.
    if unsafe { libc::shmget(k, 4096, libc::IPC_CREAT | 0o600) } < 0 {
        let _ = writeln!(io::stdout(), "ipc-hold: shmget: {}", io::Error::last_os_error());
        return 1;
    }
    if let Err(e) = std::fs::write(format!("/dev/shm/{key}"), b"held") {
        let _ = writeln!(io::stdout(), "ipc-hold: /dev/shm/{key}: {e}");
        return 1;
    }
    let _ = writeln!(io::stdout(), "held");
    let _ = io::stdout().flush();
    0
}

/// Walks `mib` MiB, one word a page, `steps` times in an order of its own, each step the
/// address the step before read, and says the nanoseconds a step took: a load whose
/// translation every step misses, as a host's address space shapes it (D159, PM M230).
fn walk(mib: usize, steps: u64) -> i32 {
    const PAGE_WORDS: usize = 4096 / 8;
    let pages = (mib << 20) / 4096;
    if pages < 2 {
        let _ = writeln!(io::stdout(), "walk: at least 8 KiB");
        return 1;
    }
    let mut words = vec![0usize; pages * PAGE_WORDS];
    // A cycle through every page, in an order a xorshift sets (Sattolo's shuffle, which
    // leaves one cycle).
    let mut order: Vec<usize> = (0..pages).collect();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in (1..pages).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = usize::try_from(x % i as u64).unwrap_or(0);
        order.swap(i, j);
    }
    let first = order.first().copied().unwrap_or(0);
    for (i, &page) in order.iter().enumerate() {
        let next = order.get(i + 1).copied().unwrap_or(first);
        if let Some(w) = words.get_mut(page * PAGE_WORDS) {
            *w = next * PAGE_WORDS;
        }
    }
    let mut at = 0usize;
    let start = std::time::Instant::now();
    for _ in 0..steps {
        at = words.get(at).copied().unwrap_or(0);
    }
    let ns = start.elapsed().as_nanos() as f64 / steps.max(1) as f64;
    let _ = writeln!(io::stdout(), "walk {ns:.2} ns ended at {at}");
    0
}

/// `swap-kept MIB`: MIB MiB of a pattern of its own, then each round reads all of it back
/// (`main`).
/// `unlinked-held`'s: see its words.
fn unlinked_held(path: &str, size: Option<u64>) -> i32 {
    use std::io::Write as _;
    use std::os::unix::fs::FileExt as _;
    let held = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .and_then(|mut f| {
            f.write_all(b"held\n")?;
            if let Some(size) = size {
                f.set_len(size)?;
            }
            f.sync_data()?;
            std::fs::remove_file(path).map(|()| f)
        });
    let held = match held {
        Ok(held) => held,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{path}: {e}");
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    let mut n = 0u64;
    loop {
        n += 1;
        let mut line = [0u8; 5];
        let done = held
            .write_all_at(format!("{n:016}").as_bytes(), 5)
            .and_then(|()| held.sync_data())
            .and_then(|()| held.read_exact_at(&mut line, 0));
        match done {
            Ok(()) => {
                let _ = io::stdout().write_all(&line);
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "{path}: {e}");
                return 1;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `pour`'s: see its words.
fn pour(path: &str, bytes: u64, chunk: usize) -> i32 {
    use std::io::Write as _;
    let started = std::time::Instant::now();
    let poured = std::fs::File::create(path).and_then(|mut f| {
        let block = vec![0x5au8; chunk.max(1)];
        let mut left = bytes;
        while left > 0 {
            let n = usize::try_from(left).unwrap_or(usize::MAX).min(block.len());
            f.write_all(block.get(..n).unwrap_or_default())?;
            f.sync_data()?;
            left -= n as u64;
        }
        Ok(())
    });
    match poured {
        Ok(()) => {
            let _ = writeln!(
                io::stdout(),
                "poured {bytes} in {} ms",
                started.elapsed().as_millis()
            );
            0
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "{path}: {e}");
            1
        }
    }
}

/// `appender`'s: see its words.
fn appender(path: &str, ms: u64) -> i32 {
    let path = std::path::Path::new(path);
    let opened = path
        .parent()
        .map_or(Ok(()), std::env::set_current_dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.file_name().unwrap_or_default())
        });
    let mut file = match opened {
        Ok(file) => file,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{}: {e}", path.display());
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    let mut n = 0u64;
    loop {
        n += 1;
        if let Err(e) = writeln!(file, "{n}").and_then(|()| file.sync_data()) {
            let _ = writeln!(io::stderr(), "{n}: {e}");
            return 1;
        }
        let _ = writeln!(io::stdout(), "{n}");
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

fn swap_kept(mib: usize) -> i32 {
    let byte = |i: usize| (i.wrapping_mul(2_654_435_761) >> 13) as u8;
    let held: Vec<Vec<u8>> = (0..mib)
        .map(|m| (0..1 << 20).map(|i| byte((m << 20) | i)).collect())
        .collect();
    let _ = writeln!(io::stdout(), "ready");
    for round in 1u64.. {
        for (m, chunk) in held.iter().enumerate() {
            if let Some(at) = chunk
                .iter()
                .enumerate()
                .position(|(i, &b)| b != byte((m << 20) | i))
            {
                let _ = writeln!(io::stdout(), "lost {}", (m << 20) + at);
                return 1;
            }
        }
        let _ = writeln!(io::stdout(), "kept {round}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    0
}

/// Writes `n` lines on stdout, `line 1` to `line N`.
fn count(n: u64) -> i32 {
    let mut out = io::BufWriter::new(io::stdout().lock());
    for i in 1..=n {
        if writeln!(out, "line {i}").is_err() {
            return 1;
        }
    }
    i32::from(out.flush().is_err())
}
