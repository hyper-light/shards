//! A nested runtime under the nesting grant (D234), for crates/shards/tests/nesting.rs.
//! `nesting CASE`, run as a microVM's command or an exec into it, drives a pinned runc (the
//! stand-in nested runtime) and the kernel's own interfaces, and says what each step met;
//! `nesting-box WHAT` is what each box runs. Every line is one fact the host test matches.

use std::io::{self, Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the image puts the stand-in runtime (the test's Agentfile `COPY`s it there).
const RUNC: &str = "/usr/local/bin/runc";
/// The most a runc of a case may take before it is asked where it waits: a third of the
/// host test's bound on the whole run (tests/nesting.rs `TIMEOUT`, 180 s), so that its
/// answer comes back within that bound.
const RUNC_MOST: Duration = Duration::from_secs(60);

/// runc, as this stand-in nested runtime runs it for its boxes: without the grant's
/// `NOTIFY_SOCKET` (D234), which runc takes for its own supervisor's and proxies for its
/// container (runc v1.5.2 notify_socket.go): it then waits for the box to say `READY=1`,
/// `run --detach` until the box says it or ends, `run` until it says it, as it watches its
/// own pid there, before it reaps the box (signals.go, `forward`). No box here says it, so
/// every runc case hung on real VMs (HVF at 27ed7cccd, Linux CI at 8a50a9742). A process
/// handed `NOTIFY_SOCKET` keeps it from the children that do not answer for its readiness
/// (sd_notify(3), `unset_environment`).
fn runc() -> Command {
    let mut runc = Command::new(RUNC);
    runc.env_remove("NOTIFY_SOCKET");
    runc
}
/// runc's state, in the run's own `/run`, a tmpfs the run asks for (`--tmpfs /run`).
const STATE: &str = "/run/runc";
/// Each box's bundle, and the one root every box is given, read-only.
const BUNDLES: &str = "/run/boxes";
/// The workload's cgroup namespace, which the grant mounts writable.
const CGROUP: &str = "/sys/fs/cgroup";

fn say(line: &str) {
    let _ = writeln!(io::stdout(), "{line}");
}

fn errno(e: &io::Error) -> String {
    format!("errno {}", e.raw_os_error().unwrap_or(0))
}

/// The workload's side: one case of tests/nesting.rs.
pub fn nesting(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "limits" => limits(),
        "contain" => contain(),
        "first-step" => first_step(),
        "ready" => ready(arg(1).parse().unwrap_or(0)),
        "gpu" => gpu(),
        "exit" => exit_leaving_a_box(arg(1).parse().unwrap_or(1)),
        "hold" => hold(),
        "listen" => listen(arg(1), args.get(2..).unwrap_or_default()),
        other => {
            let _ = writeln!(io::stderr(), "nesting: no case {other:?}");
            2
        }
    }
}

/// A box's side: what its one process does.
pub fn in_box(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "pids" => fork_past(arg(1).parse().unwrap_or(0)),
        "memory" => touch_past(arg(1).parse().unwrap_or(0)),
        "self" => this_box(),
        "open" => open(arg(1)),
        "hold" => hold(),
        other => {
            let _ = writeln!(io::stderr(), "nesting-box: no case {other:?}");
            2
        }
    }
}

/// The root every box is given: this program alone, as `/testguest`, and the places its
/// config mounts `/proc` and `/dev` on. Made once in the run's `/run`.
fn rootfs() -> io::Result<()> {
    let root = format!("{BUNDLES}/rootfs");
    if Path::new(&format!("{root}/testguest")).exists() {
        return Ok(());
    }
    for dir in ["proc", "dev"] {
        std::fs::create_dir_all(format!("{root}/{dir}"))?;
    }
    std::fs::copy("/proc/self/exe", format!("{root}/testguest"))?;
    Ok(())
}

/// Box `id`'s bundle: runc's config for `args`, in a cgroup of its own under the
/// workload's subtree (`/boxes/ID`), with its own PID, mount, IPC, UTS and cgroup
/// namespaces, `resources` (the config's `linux.resources` members) and the device nodes
/// `devices` (its `linux.devices`).
fn bundle(id: &str, args: &[&str], resources: &str, devices: &str) -> io::Result<String> {
    rootfs()?;
    let dir = format!("{BUNDLES}/{id}");
    std::fs::create_dir_all(&dir)?;
    let args: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
    let args = args.join(",");
    let config = format!(
        concat!(
            r#"{{"ociVersion":"1.2.0","process":{{"terminal":false,"user":{{"uid":0,"gid":0}},"#,
            r#""args":[{args}],"env":["PATH=/"],"cwd":"/","noNewPrivileges":true}},"#,
            r#""root":{{"path":"{bundles}/rootfs","readonly":true}},"hostname":"{id}","#,
            r#""mounts":[{{"destination":"/proc","type":"proc","source":"proc"}},"#,
            r#"{{"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","strictatime","mode=755","size=65536k"]}}],"#,
            r#""linux":{{"cgroupsPath":"/boxes/{id}","resources":{{{resources}}},"devices":[{devices}],"#,
            r#""namespaces":[{{"type":"pid"}},{{"type":"mount"}},{{"type":"ipc"}},{{"type":"uts"}},{{"type":"cgroup"}}]}}}}"#
        ),
        args = args,
        bundles = BUNDLES,
        id = id,
        resources = resources,
        devices = devices,
    );
    std::fs::write(format!("{dir}/config.json"), config)?;
    Ok(dir)
}

/// `runc run` of box `id` in the foreground, kept once it ends (`--keep`) so its cgroup's
/// events can be read: each line it wrote, after `box ID:`, then its status. runc joins
/// no new session keyring (`--no-new-keyring`): the grant leaves keyctl refused (D234),
/// and runc fails at any error from it but ENOSYS (libcontainer/standard_init_linux.go).
fn run_box(id: &str, args: &[&str], resources: &str, devices: &str) -> Option<i32> {
    let dir = match bundle(id, args, resources, devices) {
        Ok(dir) => dir,
        Err(e) => {
            say(&format!("box {id}: bundle {}", errno(&e)));
            return None;
        }
    };
    let ran = runc()
        .args(["--root", STATE, "run", "--keep", "--no-new-keyring", "--bundle"])
        .arg(&dir)
        .arg(id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|runc| bounded(runc, id, RUNC_MOST));
    match ran {
        Ok(out) => {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                say(&format!("box {id}: {line}"));
            }
            for line in String::from_utf8_lossy(&out.stderr).lines() {
                say(&format!("box {id} runc: {line}"));
            }
            let code = out.status.code().or_else(|| out.status.signal().map(|s| 128 + s));
            say(&format!(
                "box {id}: status {}",
                code.map_or_else(|| "none".to_string(), |c| c.to_string())
            ));
            code
        }
        Err(e) => {
            say(&format!("box {id}: runc {}", errno(&e)));
            None
        }
    }
}

/// `runc`'s output, as `Command::output` gives it; where it has not ended within
/// [`RUNC_MOST`], once SIGQUIT has had Go's runtime write where each of its goroutines waits
/// to its stderr and exit, so that a runtime that hangs says where (box `what`).
fn bounded(mut runc: std::process::Child, what: &str, most: Duration) -> io::Result<std::process::Output> {
    let read = |pipe: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let out = read(
        runc.stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
    );
    let err = read(
        runc.stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
    );
    let deadline = Instant::now() + most;
    let status = loop {
        if let Some(status) = runc.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            say(&format!(
                "box {what}: runc did not end within {most:?}, asked where it waits (SIGQUIT)"
            ));
            for line in where_it_waits(runc.id()) {
                say(&format!("box {what}: runc {line}"));
            }
            if let Ok(pid) = libc::pid_t::try_from(runc.id()) {
                // SAFETY: kill(2) of this process's own child, not yet reaped.
                unsafe { libc::kill(pid, libc::SIGQUIT) };
            }
            break runc.wait()?;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Where process `pid` waits: nothing, as yet.
fn where_it_waits(_pid: u32) -> Vec<String> {
    Vec::new()
}

/// `KEY`'s count in the kept box's cgroup file `FILE`, as the workload reads it from inside
/// its own subtree.
fn event(id: &str, file: &str, key: &str) {
    let path = format!("{CGROUP}/boxes/{id}/{file}");
    let said = match std::fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(' ')))
            .map_or_else(|| "absent".to_string(), str::to_string),
        Err(e) => errno(&e),
    };
    say(&format!("box {id}: {file} {key} {said}"));
}

fn delete(id: &str) {
    let _ = runc()
        .args(["--root", STATE, "delete", "--force", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// This process's own cgroup, as its namespace shows it: `/init`, the leaf, for the
/// granted command and every exec into it (D234).
fn own_cgroup() -> String {
    let text = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("none")
        .to_string()
}

/// Case (a): boxes with limits, each in a cgroup runc makes under the delegated subtree.
/// pids.max 8 holds the box's own process and 7 more, so its 8th fork fails EAGAIN and
/// pids.events counts it; memory.max 32 MiB, with no swap past it, is met by a box
/// touching 256 MiB, which the kernel kills inside its cgroup, as memory.events counts.
/// A third box says it is PID 1 of its own namespace, in a root of its own (pivot_root):
/// its program there, and the workload's files absent.
fn limits() -> i32 {
    say(&format!("workload cgroup {}", own_cgroup()));
    run_box(
        "pids",
        &["/testguest", "nesting-box", "pids", "64"],
        r#""pids":{"limit":8}"#,
        "",
    );
    event("pids", "pids.events", "max");
    delete("pids");
    run_box(
        "memory",
        &["/testguest", "nesting-box", "memory", "268435456"],
        r#""memory":{"limit":33554432,"swap":33554432}"#,
        "",
    );
    event("memory", "memory.events", "oom_kill");
    delete("memory");
    run_box("self", &["/testguest", "nesting-box", "self"], "", "");
    delete("self");
    0
}

/// Case (b): what the subtree holds the workload to. Its namespace root's own limits stay
/// init's (nsdelegate: EPERM). A cgroup2 mount of its own, which CAP_SYS_ADMIN and no
/// mount filter let it make, shows the same root, nothing above it. And a process in a
/// cgroup namespace of its own below that root moves neither itself out of it nor another
/// process into it from outside (ENOENT, cgroup_procs_write_permission): the boundary
/// that holds the workload's boxes, as the workload's own holds it.
fn contain() -> i32 {
    for file in ["memory.max", "pids.max"] {
        let said = match std::fs::write(format!("{CGROUP}/{file}"), "max") {
            Ok(()) => "written".to_string(),
            Err(e) => errno(&e),
        };
        say(&format!("root {file}: {said}"));
    }
    say(&format!("fresh mount: {}", fresh_mount()));
    let inner = format!("{CGROUP}/inner");
    if let Err(e) = std::fs::create_dir(&inner) {
        say(&format!("inner: {}", errno(&e)));
        return 1;
    }
    // SAFETY: getpid(2).
    let workload = unsafe { libc::getpid() };
    // SAFETY: fork(2) of this single-threaded process; the child writes, then _exits.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let step = |what: &str, done: io::Result<()>| {
            let said = done.map_or_else(|e| errno(&e), |()| "done".to_string());
            say(&format!("inner: {what} {said}"));
        };
        step("joined", std::fs::write(format!("{inner}/cgroup.procs"), "0"));
        // SAFETY: unshare(2) of this process's cgroup namespace.
        let unshared = if unsafe { libc::unshare(libc::CLONE_NEWCGROUP) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
        step("unshared", unshared);
        step(
            "out of its namespace",
            std::fs::write(format!("{CGROUP}/init/cgroup.procs"), "0"),
        );
        step(
            "into its namespace from outside",
            std::fs::write(format!("{inner}/cgroup.procs"), workload.to_string()),
        );
        let _ = io::stdout().flush();
        // SAFETY: _exit(2) ends the child without running the parent's destructors.
        unsafe { libc::_exit(0) };
    }
    if child < 0 {
        say(&format!("inner: fork {}", errno(&io::Error::last_os_error())));
        return 1;
    }
    let mut status = 0;
    // SAFETY: waitpid(2) for the child just forked.
    unsafe { libc::waitpid(child, &mut status, 0) };
    let _ = std::fs::remove_dir(&inner);
    0
}

/// Mounts cgroup2 afresh at `/run/cgroup-again` and compares its directories with the
/// workload's own mount's: `the same root` where they match.
fn fresh_mount() -> String {
    let at = "/run/cgroup-again";
    if let Err(e) = std::fs::create_dir_all(at) {
        return format!("mkdir {}", errno(&e));
    }
    // SAFETY: mount(2) with NUL-terminated literals and no data.
    let mounted = unsafe {
        libc::mount(
            c"cgroup2".as_ptr(),
            c"/run/cgroup-again".as_ptr(),
            c"cgroup2".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if mounted != 0 {
        return format!("mount {}", errno(&io::Error::last_os_error()));
    }
    let dirs = |path: &str| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let (ours, again) = (dirs(CGROUP), dirs(at));
    // SAFETY: umount2(2) of the mount just made.
    unsafe { libc::umount2(c"/run/cgroup-again".as_ptr(), 0) };
    if ours == again {
        format!("the same root [{}]", ours.join(" "))
    } else {
        format!("differs [{}] [{}]", ours.join(" "), again.join(" "))
    }
}

/// Case (c): a nested runtime's first steps, as a run not given the grant meets them:
/// making a box's cgroup, a child's new mount and PID namespaces, and runc itself. The
/// granted run says each done, which keeps the ungranted runs' refusals from passing on a
/// probe that never worked.
fn first_step() -> i32 {
    let said = match std::fs::create_dir(format!("{CGROUP}/boxes")) {
        Ok(()) => "done".to_string(),
        Err(e) => errno(&e),
    };
    say(&format!("first cgroup step: {said}"));
    say(&format!(
        "first namespace step: {}",
        unshare_in_child(libc::CLONE_NEWNS | libc::CLONE_NEWPID)
    ));
    run_box("first", &["/testguest", "nesting-box", "self"], "", "");
    delete("first");
    0
}

/// A child's unshare(2) of `flags`: `done`, or the errno that refused it.
fn unshare_in_child(flags: libc::c_int) -> String {
    // SAFETY: fork(2) of this single-threaded process; the child only unshares and _exits.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: unshare(2), then _exit(2) with its errno.
        unsafe {
            let code = if libc::unshare(flags) == 0 {
                0
            } else {
                io::Error::last_os_error().raw_os_error().unwrap_or(255)
            };
            libc::_exit(code)
        }
    }
    if child < 0 {
        return format!("fork {}", errno(&io::Error::last_os_error()));
    }
    let mut status = 0;
    // SAFETY: waitpid(2) for the child just forked.
    unsafe { libc::waitpid(child, &mut status, 0) };
    match libc::WEXITSTATUS(status) {
        0 => "done".to_string(),
        n => format!("errno {n}"),
    }
}

/// Case (d): says its deadline (`SHARDS_READY_BY`) and its readiness socket, waits
/// `delay_ms`, tells the socket `READY=1` as sd_notify(3) does, says so, then holds until
/// the run is stopped.
fn ready(delay_ms: u64) -> i32 {
    say(&format!(
        "ready-by {}",
        std::env::var("SHARDS_READY_BY").unwrap_or_else(|_| "none".to_string())
    ));
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        say("notify none");
        return 1;
    };
    say(&format!("notify {}", socket.to_string_lossy()));
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    let sent = std::os::unix::net::UnixDatagram::unbound().and_then(|s| s.send_to(b"READY=1", &socket));
    say(&format!(
        "sent READY=1: {}",
        sent.map_or_else(|e| errno(&e), |n| format!("{n} bytes"))
    ));
    hold()
}

/// Case (e), on a host with a GPU, its render node given the run (`--gpus`): one box given
/// the node and a rule for it opens it; one given the node and no rule cannot (its own
/// device program); one given card0 and a rule for it cannot either, as the workload's own
/// program, attached BPF_F_ALLOW_MULTI above the box's, never gave the workload card0
/// (f20).
fn gpu() -> i32 {
    let node = |path: &str, minor: u32| {
        format!(
            r#"{{"path":"{path}","type":"c","major":226,"minor":{minor},"fileMode":438,"uid":0,"gid":0}}"#
        )
    };
    let rules = |minor: Option<u32>| {
        let allow = minor.map_or_else(String::new, |m| {
            format!(r#",{{"allow":true,"type":"c","major":226,"minor":{m},"access":"rw"}}"#)
        });
        format!(r#""devices":[{{"allow":false,"access":"rwm"}}{allow}]"#)
    };
    say(&format!("workload {}", open_said("/dev/dri/renderD128")));
    for (id, path, minor, allowed) in [
        ("given", "/dev/dri/renderD128", 128, Some(128)),
        ("denied", "/dev/dri/renderD128", 128, None),
        ("beyond", "/dev/dri/card0", 0, Some(0)),
    ] {
        run_box(
            id,
            &["/testguest", "nesting-box", "open", path],
            &rules(allowed),
            &node(path, minor),
        );
        delete(id);
    }
    0
}

/// Case (f): starts a box that holds, detached, says its process, then ends with `code`:
/// the run ends with the workload's status, the held box with it.
fn exit_leaving_a_box(code: i32) -> i32 {
    let dir = match bundle("held", &["/testguest", "nesting-box", "hold"], "", "") {
        Ok(dir) => dir,
        Err(e) => {
            say(&format!("box held: bundle {}", errno(&e)));
            return 1;
        }
    };
    // What runc says goes to a file, not a pipe: the held box takes runc's stdio for its
    // own (`--detach`), and would hold a pipe open past runc. Said where runc did not end
    // as it should, its goroutines' stacks among it ([`bounded`]).
    let said = format!("{BUNDLES}/held.runc-stderr");
    let started = std::fs::File::create(&said).and_then(|stderr| {
        runc()
            .args(["--root", STATE, "run", "--detach", "--no-new-keyring", "--bundle"])
            .arg(&dir)
            .arg("held")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .and_then(|runc| bounded(runc, "held", RUNC_MOST))
            .map(|out| out.status)
    });
    if !started.as_ref().is_ok_and(std::process::ExitStatus::success) {
        for line in std::fs::read_to_string(&said).unwrap_or_default().lines() {
            say(&format!("box held runc: {line}"));
        }
    }
    let state = runc()
        .args(["--root", STATE, "state", "held"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let pid = state.ok().and_then(|o| {
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let at = text.find("\"pid\":")?;
        let rest = text.get(at + 6..)?.trim_start();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty()).then_some(digits)
    });
    say(&format!(
        "box held: {} pid {}",
        started.map_or_else(
            |e| errno(&e),
            |s| s
                .code()
                .map_or_else(|| "signal".to_string(), |c| format!("status {c}"))
        ),
        pid.unwrap_or_else(|| "none".to_string())
    ));
    say(&format!("exiting {code}"));
    code
}

/// `nesting listen DIR NAME...` (D251; Hex's step 39 fixture): the workload as Hex's relay is
/// under shards-init. It binds an AF_UNIX stream listener for each name in `dir`, says READY=1
/// once all of them listen (so READY means every socket is there), then serves each connection
/// on a thread of its own: it reads the connection to its end (its controller's half-close),
/// writes back the socket's name, a newline and what it read reversed, and ends its own half.
fn listen(dir: &str, names: &[String]) -> i32 {
    use std::io::{Read as _, Write as _};
    if let Err(e) = std::fs::create_dir_all(dir) {
        say(&format!("mkdir {dir}: {}", errno(&e)));
        return 1;
    }
    let mut listeners = Vec::new();
    for name in names {
        match std::os::unix::net::UnixListener::bind(Path::new(dir).join(name)) {
            Ok(listener) => listeners.push((name.clone(), listener)),
            Err(e) => {
                say(&format!("bind {name}: {}", errno(&e)));
                return 1;
            }
        }
    }
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        say("notify none");
        return 1;
    };
    let sent = std::os::unix::net::UnixDatagram::unbound().and_then(|s| s.send_to(b"READY=1", &socket));
    say(&format!(
        "listening {} in {dir}; sent READY=1: {}",
        names.join(" "),
        sent.map_or_else(|e| errno(&e), |n| format!("{n} bytes"))
    ));
    for (name, listener) in listeners {
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let name = name.clone();
                std::thread::spawn(move || {
                    let mut got = Vec::new();
                    if let Err(e) = conn.read_to_end(&mut got) {
                        say(&format!("{name}: read {}", errno(&e)));
                        return;
                    }
                    let mut back = format!("{name}\n").into_bytes();
                    back.extend(got.iter().rev());
                    let wrote = conn
                        .write_all(&back)
                        .and_then(|()| conn.shutdown(std::net::Shutdown::Write));
                    say(&format!(
                        "{name}: read {} wrote {}",
                        got.len(),
                        wrote.map_or_else(|e| errno(&e), |()| back.len().to_string())
                    ));
                });
            }
        });
    }
    hold()
}

fn hold() -> i32 {
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

/// In a box: forks children that wait, up to `n`, until a fork fails; says how many it
/// made and the errno that stopped it, then ends them.
fn fork_past(n: usize) -> i32 {
    let mut children = Vec::new();
    let mut stopped = "nothing".to_string();
    for _ in 0..n {
        // SAFETY: fork(2) of this single-threaded process; the child only waits for its end.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            loop {
                // SAFETY: blocks until a signal arrives.
                unsafe { libc::pause() };
            }
        }
        if pid < 0 {
            stopped = errno(&io::Error::last_os_error());
            break;
        }
        children.push(pid);
    }
    say(&format!("forked {}, then {stopped}", children.len()));
    for pid in children {
        // SAFETY: kill(2) and waitpid(2) of this process's own children.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    0
}

/// In a box: touches `bytes` of memory a MiB at a time, saying each 8 MiB: a box held to
/// less is killed before it says `touched all`.
fn touch_past(bytes: usize) -> i32 {
    const MIB: usize = 1 << 20;
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut total = 0;
    while total < bytes {
        let mut chunk = vec![0u8; MIB];
        chunk.iter_mut().step_by(4096).for_each(|b| *b = 1);
        held.push(chunk);
        total += MIB;
        if total % (8 * MIB) == 0 {
            say(&format!("touched {} MiB", total / MIB));
        }
    }
    std::hint::black_box(&held);
    say("touched all");
    0
}

/// In a box: its PID in its own namespace, what its root holds, and its cgroup as its own
/// namespace shows it.
fn this_box() -> i32 {
    say(&format!("pid {}", std::process::id()));
    for path in ["/testguest", "/bin/testguest", "/etc/passwd"] {
        let there = if Path::new(path).exists() {
            "present"
        } else {
            "absent"
        };
        say(&format!("{path} {there}"));
    }
    say(&format!("cgroup {}", own_cgroup()));
    0
}

fn open_said(path: &str) -> String {
    let said = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => "ok".to_string(),
        Err(e) => errno(&e),
    };
    format!("open {path}: {said}")
}

/// In a box: opens `path` for reading and writing.
fn open(path: &str) -> i32 {
    say(&open_said(path));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::net::{SocketAddr, UnixListener};

    /// A runc that does not end within its bound ends at it, though it takes SIGQUIT for
    /// itself as runc v1.5.2 does (it relays every signal it can catch to its box,
    /// signals.go): its stand-in here ignores SIGQUIT, and only SIGKILL ends it.
    #[test]
    fn a_runc_that_takes_sigquit_for_itself_ends_at_its_bound() {
        let runc = Command::new("/bin/sh")
            .args(["-c", "trap '' QUIT; exec sleep 600"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(runc.id()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(bounded(runc, "stand-in", Duration::from_millis(200)));
        });
        let ended = rx.recv_timeout(Duration::from_secs(30));
        if ended.is_err() {
            // SAFETY: kill(2) of the stand-in, which its bound left running.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let out = ended
            .expect("its bound passed 30 s ago and the stand-in still runs")
            .unwrap();
        assert_eq!(out.status.signal(), Some(libc::SIGKILL));
    }

    /// Where a process waits names each of its threads' waits and each descriptor it holds,
    /// a Unix socket by the name it is bound to: as runc's notify socket would be named
    /// (`--root`'s `ID/notify/notify.sock`, runc v1.5.2 notify_socket.go).
    #[test]
    fn where_a_process_waits_names_its_threads_and_its_sockets() {
        let name = format!("shards-nesting-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let _listener = UnixListener::bind_addr(&addr).unwrap();
        let said = where_it_waits(std::process::id());
        assert!(
            said.iter()
                .any(|l| l.starts_with("thread ") && l.contains(" waits in ")),
            "{said:#?}"
        );
        let named = format!(" @{name}");
        assert!(
            said.iter().any(|l| l.starts_with("fd ") && l.ends_with(&named)),
            "{said:#?}"
        );
    }
}
