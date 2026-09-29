//! Commands run in images, end to end: `shards vm run --rootfs IMAGE -- COMMAND` boots a
//! real VM into an EROFS image, shards-init runs the command as `docker run` would, and
//! its output and exit status come back through shards. The command is the test guest,
//! run as a workload (crates/testguest/src/workload.rs). Runs need vsock, which shards has
//! on Unix hosts.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, shards, workload_image};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The host's wall clock, in nanoseconds since the Unix epoch.
fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status,
            String::from_utf8_lossy(&self.stdout),
            self.stderr
        )
    }
}

/// Runs `shards vm run ... --rootfs IMAGE <options> -- <command>`, with `stdin` as its
/// input.
fn run(image: &Path, options: &[&str], command: &[&str], stdin: &[u8]) -> Output {
    let mut args: Vec<&OsStr> = vec![
        "vm".as_ref(),
        "run".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
    ];
    args.extend([
        "--init".as_ref(),
        guest_init().as_os_str(),
        "--rootfs".as_ref(),
        image.as_os_str(),
    ]);
    args.extend(options.iter().map(OsStr::new));
    args.push("--".as_ref());
    args.extend(command.iter().map(OsStr::new));
    shards_with(&args, stdin)
}

/// Runs `shards vm restore TEMPLATE <options> -- <command>`.
fn restore(template: &Path, options: &[&str], command: &[&str]) -> Output {
    let mut args: Vec<&OsStr> = vec!["vm".as_ref(), "restore".as_ref(), template.as_os_str()];
    args.extend(options.iter().map(OsStr::new));
    args.push("--".as_ref());
    args.extend(command.iter().map(OsStr::new));
    shards_with(&args, b"")
}

/// Runs shards with `args`, `stdin` as its input, and a timeout.
fn shards_with(args: &[&OsStr], stdin: &[u8]) -> Output {
    let mut child = Command::new(shards())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).unwrap();
        v
    });
    let mut err = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        err.read_to_string(&mut s).unwrap();
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("shards did not exit within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    feeder.join().unwrap();
    Output {
        status: status.code(),
        stdout: reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}

/// `report`'s `key value` lines.
fn report(out: &Output) -> BTreeMap<String, String> {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| {
            // Environment and mount lines share a key; keep them apart by what follows.
            match k {
                "env" | "mount" => {
                    let (name, value) = v.split_once(['=', ' ']).unwrap();
                    (format!("{k} {name}"), value.to_string())
                }
                _ => (k.to_string(), v.to_string()),
            }
        })
        .collect()
}

#[test]
fn commands_run_in_the_image_as_docker_runs_them() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run");
    let image = workload_image(&dir);

    let before = now_ns();
    let out = run(
        &image,
        &["--hostname", "box", "-e", "GREETING=hi", "-e", "HOSTNAME=renamed"],
        &["/bin/testguest", "report"],
        b"",
    );
    let after = now_ns();
    assert_eq!(out.status, Some(0), "{out}");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    // The guest's clock is the host's, not the RTC's whole seconds.
    let realtime: u128 = get("realtime").parse().unwrap();
    assert!(
        (before..=after).contains(&realtime),
        "{before} <= {realtime} <= {after}"
    );
    assert_eq!((get("uid"), get("gid"), get("groups")), ("0", "0", "0"), "{out}");
    assert_eq!((get("cwd"), get("hostname")), ("/", "box"), "{out}");
    assert_eq!(
        get("env PATH"),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    );
    assert_eq!((get("env HOSTNAME"), get("env GREETING")), ("renamed", "hi"));
    assert_eq!(get("env HOME"), "/root", "HOME comes from /etc/passwd");
    assert_eq!(get("writable"), "true", "the root filesystem takes writes: {out}");
    // Signals as a new process has them, whatever init itself blocks or ignores.
    assert_eq!(
        (get("sigpipe"), get("sigblk")),
        ("default", "0000000000000000"),
        "{out}"
    );
    for (target, fstype) in [
        ("/", "overlay"),
        ("/proc", "proc"),
        ("/sys", "sysfs"),
        ("/dev", "devtmpfs"),
        ("/dev/pts", "devpts"),
        ("/dev/shm", "tmpfs"),
        ("/dev/mqueue", "mqueue"),
    ] {
        assert_eq!(get(&format!("mount {target}")), fstype, "{out}");
    }

    // A named user with its groups and home, found on PATH, in a working directory that
    // does not exist yet.
    let out = run(
        &image,
        &["-u", "app", "-w", "/work/dir"],
        &["testguest", "report"],
        b"",
    );
    assert_eq!(out.status, Some(0), "{out}");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    // getgroups(2) reports the kernel's order: setgroups(2) sorts them (kernel/groups.c).
    assert_eq!(
        (get("uid"), get("gid"), get("groups")),
        ("1000", "1000", "50,1000"),
        "{out}"
    );
    assert_eq!((get("cwd"), get("env HOME")), ("/work/dir", "/home/app"), "{out}");
    assert_eq!(get("writable"), "false", "app may not write to /");

    // A numeric user with no passwd entry.
    let out = run(&image, &["-u", "4242"], &["/bin/testguest", "report"], b"");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    assert_eq!(
        (get("uid"), get("gid"), get("env HOME")),
        ("4242", "0", "/"),
        "{out}"
    );
}

#[test]
fn exit_statuses_are_the_ones_docker_run_gives() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run-status");
    let image = workload_image(&dir);
    for (options, command, status, stderr) in [
        (&[][..], &["/bin/testguest", "exit", "3"][..], 3, ""),
        (&[], &["/bin/testguest", "kill"], 128 + 9, ""),
        (&[], &["/bin/testguest", "stderr", "to stderr"], 0, "to stderr"),
        (&[], &["nothere"], 127, "executable file not found in $PATH"),
        (&[], &["/bin/nothere"], 127, "no such file or directory"),
        (&[], &["/bin"], 126, "is a directory"),
        (
            &["-u", "nobody"],
            &["/bin/testguest", "report"],
            125,
            "unable to find user nobody",
        ),
        (
            &["-w", "relative"],
            &["/bin/testguest", "report"],
            125,
            "not absolute",
        ),
    ] {
        let out = run(&image, options, command, b"");
        assert_eq!(out.status, Some(status), "{command:?}: {out}");
        assert!(
            out.stderr.to_lowercase().contains(&stderr.to_lowercase()),
            "{command:?}: {out}"
        );
    }
    // A container ends with its main process: a child left holding stdout is killed.
    let out = run(&image, &[], &["/bin/testguest", "orphan"], b"");
    assert_eq!(
        (out.status, out.stdout.as_slice()),
        (Some(0), &b"parent done\n"[..]),
        "{out}"
    );
}

#[test]
fn stdio_carries_bulk_data_both_ways() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run-stdio");
    let image = workload_image(&dir);
    let mut input = vec![0u8; 8 << 20];
    shards_testguest::fill(7, 0, &mut input);
    let out = run(&image, &["-i"], &["/bin/testguest", "cat"], &input);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert!(
        out.stdout == input,
        "8 MiB through stdin and back: got {} bytes",
        out.stdout.len()
    );

    // Without -i, stdin is closed.
    let out = run(&image, &[], &["/bin/testguest", "cat"], b"ignored");
    assert_eq!((out.status, out.stdout.len()), (Some(0), 0), "{out}");

    let len = 32 << 20;
    let out = run(
        &image,
        &[],
        &["/bin/testguest", "bulk", &len.to_string(), "9"],
        b"",
    );
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.len(), len);
    assert_eq!(shards_testguest::first_mismatch(9, 0, &out.stdout), None);
}

#[test]
fn templates_restore_into_runs_of_their_own() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("run-template");
    let image = workload_image(&dir);
    let template = dir.join("template");
    let saved = Command::new(shards())
        .args(["vm", "run", "--kernel"])
        .arg(kernel())
        .args([
            "--init".as_ref(),
            guest_init().as_os_str(),
            "--rootfs".as_ref(),
            image.as_os_str(),
        ])
        .args([
            "--snapshot-dir".as_ref(),
            template.as_os_str(),
            "--no-console".as_ref(),
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );

    // Each run resumes the mounted image and writes to its own copy of it, on the host's
    // clock: the template's time stopped when it was saved.
    let before = now_ns();
    let first = restore(&template, &[], &["/bin/testguest", "report"]);
    let after = now_ns();
    assert_eq!(first.status, Some(0), "{first}");
    let first = report(&first);
    let realtime: u128 = first.get("realtime").unwrap().parse().unwrap();
    assert!(
        (before..=after).contains(&realtime),
        "{before} <= {realtime} <= {after}"
    );
    let second = restore(&template, &["-u", "app", "-e", "X=1"], &["testguest", "report"]);
    assert_eq!(second.status, Some(0), "{second}");
    let second = report(&second);
    let get = |r: &BTreeMap<String, String>, k: &str| r.get(k).cloned().unwrap_or_default();
    assert_eq!(
        (get(&first, "uid"), get(&first, "writable")),
        ("0".into(), "true".into())
    );
    assert_eq!(
        (get(&second, "uid"), get(&second, "env X")),
        ("1000".into(), "1".into())
    );
    assert_eq!(
        get(&second, "existed"),
        "false",
        "the first run's file is not the second's"
    );
    assert_ne!(
        get(&first, "hostname"),
        get(&second, "hostname"),
        "each run has its own name"
    );
    assert_eq!(get(&second, "mount /"), "overlay");
}

/// Runs `command` in the image, sends `signal` to shards once the command says `ready`,
/// and returns what shards returns.
fn signaled(image: &Path, command: &[&str], signal: libc::c_int) -> Output {
    signaled_ignoring(image, command, signal, false)
}

/// [`signaled`], with shards started ignoring SIGINT and SIGQUIT if `ignoring`, as a
/// non-interactive shell starts `cmd &` (POSIX.1-2024, XCU 2.9.3.1).
fn signaled_ignoring(image: &Path, command: &[&str], signal: libc::c_int, ignoring: bool) -> Output {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let mut run = Command::new(shards());
    if ignoring {
        // SAFETY: signal(2) only, between fork and exec.
        unsafe {
            run.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGQUIT, libc::SIG_IGN);
                Ok(())
            });
        }
    }
    let mut child = run
        .args(["vm", "run", "--kernel"])
        .arg(kernel())
        .args([
            "--init".as_ref(),
            guest_init().as_os_str(),
            "--rootfs".as_ref(),
            image.as_os_str(),
        ])
        .arg("--")
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A regression fails the test instead of hanging it.
    let pid = child.id() as libc::pid_t;
    let (done, watch) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if watch.recv_timeout(TIMEOUT).is_err() {
            // SAFETY: kill(2) of our own child, which is only reaped after `done`.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    out.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    // SAFETY: kill(2) of our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    let mut rest = Vec::new();
    out.read_to_end(&mut rest).unwrap();
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    let status = child.wait().unwrap().code();
    let _ = done.send(());
    Output {
        status,
        stdout: rest,
        stderr,
    }
}

#[test]
fn signals_reach_the_command_as_docker_run_forwards_them() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run-signals");
    let image = workload_image(&dir);
    // The command sees Linux's numbers, whatever the host's are (SIGUSR1 is 30 on macOS).
    for (name, host, linux) in [("INT", libc::SIGINT, 2), ("USR1", libc::SIGUSR1, 10)] {
        let out = signaled(&image, &["/bin/testguest", "trap", name], host);
        assert_eq!(out.status, Some(0), "{name}: {out}");
        assert_eq!(out.stdout, format!("got {linux}\n").into_bytes(), "{name}: {out}");
    }
    // Even one shards was started ignoring, as `docker run`'s signal proxy takes it.
    let out = signaled_ignoring(&image, &["/bin/testguest", "trap", "INT"], libc::SIGINT, true);
    assert_eq!(
        (out.status, out.stdout.as_slice()),
        (Some(0), &b"got 2\n"[..]),
        "{out}"
    );
    // A signal the command does not catch ends it, and `docker run`'s status says which.
    let out = signaled(&image, &["/bin/testguest", "sleep"], libc::SIGTERM);
    assert_eq!(out.status, Some(128 + 15), "{out}");
}
