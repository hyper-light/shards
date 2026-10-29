//! `shards build`'s `RUN` steps on Windows hosts (D143), end to end: over a base a loopback
//! registry serves, each step run in a builder microVM confined as D141 confines a VM
//! process, its network the network process's (D142); what the steps made exported to a
//! directory of this host (`--output type=local`). tests/build.rs holds `shards build`'s
//! on Unix hosts, which these follow.

#![cfg(windows)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(180);

/// A build context holding `dockerfile` as its Dockerfile.
fn context(name: &str, dockerfile: &str) -> TempDir {
    let dir = TempDir::new(name);
    std::fs::write(dir.join("Dockerfile"), dockerfile).unwrap();
    dir
}

/// What each `RUN` step's `testguest report` said, in order: its `key value` lines.
fn step_reports(stderr: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let mut steps: Vec<(String, std::collections::BTreeMap<String, String>)> = Vec::new();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let Some((n, text)) = rest.split_once(' ') else {
            continue;
        };
        if text.contains("] RUN ") {
            steps.push((n.to_string(), Default::default()));
            continue;
        }
        let Some((last_n, map)) = steps.last_mut() else {
            continue;
        };
        if last_n != n {
            continue;
        }
        // "#6 0.123 key value": the seconds, then what the step printed.
        let Some((secs, out)) = text.split_once(' ') else {
            continue;
        };
        if secs.parse::<f64>().is_err() {
            continue;
        }
        let (k, v) = out.split_once(' ').unwrap_or((out, ""));
        map.insert(k.to_string(), v.to_string());
    }
    steps.into_iter().map(|(_, m)| m).collect()
}

/// What a build is given of this host: its home, the kernel and init its builder boots, and
/// a small builder, as its steps need little.
fn build_env(home: &TempDir) -> [(&'static str, &std::ffi::OsStr); 4] {
    [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
    ]
}

/// `RUN` steps run in the builder as root, BuildKit's sandbox's hostname theirs, and what
/// they wrote is the image's: exported to this host, its files whole.
#[test]
fn run_steps_run_in_a_confined_builder_and_their_changes_are_built() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("bw-run-home");
    let out = TempDir::new("bw-run-out");
    let ctx = context(
        "bw-run-ctx",
        &format!(
            "FROM {image}\n\
             USER root\n\
             RUN [\"/bin/testguest\", \"report\"]\n\
             RUN [\"/bin/testguest\", \"fs\", \"mkdir:/opt\", \"write:/opt/f=data\"]\n"
        ),
    );
    let output = format!("type=local,dest={}", out.display());
    let built = run_shards_env(
        &["build"],
        &[
            "--progress=plain",
            "--output",
            output.as_str(),
            ctx.to_str().unwrap(),
        ],
        &build_env(&home),
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    let reports = step_reports(&built.stderr);
    let root = reports
        .first()
        .unwrap_or_else(|| panic!("no step reported\n{shown}"));
    // Docker's build steps' limits, as on every host (`run_steps_start_with_high_file_and_process_limits`).
    for (k, v) in [
        ("uid", "0"),
        ("gid", "0"),
        ("hostname", "buildkitsandbox"),
        ("rlimit-nofile", "1048576:1048576"),
    ] {
        assert_eq!(root.get(k).map(String::as_str), Some(v), "{k}\n{shown}");
    }
    assert_eq!(
        std::fs::read_to_string(out.join("opt").join("f")).unwrap_or_default(),
        "data",
        "{shown}"
    );
}

/// A builder boots with little memory and is given more as its build needs it (D133), on
/// Windows as tests/build.rs's test of the name has it on Unix: a step of a builder that may
/// have 2,560 MiB, which boots with 512 MiB, allocates and touches three times that, and
/// succeeds. (On jupiter, one that may have 4 GiB touched 3 GiB too.)
#[test]
fn a_builders_memory_is_plugged_as_its_build_needs_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("bw-plug-home");
    let [home_env, kernel_env, init_env, _] = build_env(&home);
    let env = [
        home_env,
        kernel_env,
        init_env,
        ("SHARDS_BUILD_MEMORY", "2560".as_ref()),
    ];
    let ctx = context(
        "bw-plug-ctx",
        &format!("FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"alloc\", \"1536\"]\n"),
    );
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    assert!(built.stderr.contains("allocated 1536"), "{shown}");
}

/// A build whose jobs take its builder's memory all at once, as compilers do, over a
/// gigabyte of its layers, has it plugged in time: tests/build.rs's on Unix (PM M392).
#[test]
fn a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("bw-fast-home");
    let [home_env, kernel_env, init_env, _] = build_env(&home);
    let env = [
        home_env,
        kernel_env,
        init_env,
        ("SHARDS_BUILD_MEMORY", "8192".as_ref()),
    ];
    // A job on a quarter of its builder's vCPUs, one for each of its host's CPUs, as 8
    // compilers on 32 (build::builder_cpus), over a gigabyte written to a tmpfs first.
    let jobs = (std::thread::available_parallelism().map_or(1, std::num::NonZero::get) / 4).max(1);
    let ctx = context(
        "bw-fast-ctx",
        &format!(
            "FROM {image}\n\
             USER root\n\
             RUN --mount=type=tmpfs,target=/held,size=2g [\"/bin/testguest\", \"alloc-par\", \"{jobs}\", \"3072\", \"/held/big\", \"1024\"]\n"
        ),
    );
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    eprintln!(
        "{jobs} jobs: {}",
        built
            .stderr
            .lines()
            .find(|l| l.contains("allocated"))
            .unwrap_or_default()
    );
    assert_eq!(built.status, Some(0), "{shown}");
    assert!(built.stderr.contains("allocated 3072"), "{shown}");
}

/// A `RUN` step reaches this host's own address on its default route through its
/// network process, and one with `--network=none` reaches nothing, failing the build.
#[test]
fn run_steps_reach_the_network_unless_it_is_none() {
    use std::io::Write as _;
    if cannot_run_vms() {
        return;
    }
    // This host's address on its default route: a UDP connect sends nothing.
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(host) = host else {
        eprintln!("SKIP: this host has no route to give a guest an address of it");
        return;
    };
    let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = server.local_addr().unwrap().port();
    // Serves one connection if one comes before the build is done; a build that never
    // connects leaves it to give up, so that the test fails rather than hangs.
    server.set_nonblocking(true).unwrap();
    let (built_tx, built_rx) = std::sync::mpsc::channel::<()>();
    let serving = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while std::time::Instant::now() < deadline && built_rx.try_recv().is_err() {
            match server.accept() {
                Ok((mut c, _)) => {
                    c.set_nonblocking(false).unwrap();
                    c.write_all(b"hello from the host\n").unwrap();
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    let (image, _) = served();
    let home = TempDir::new("bw-net-home");
    let to = format!("{}:{port}", host.ip());
    let ctx = context(
        "bw-net-ctx",
        &format!(
            "FROM {image}\nRUN [\"/bin/testguest\", \"tcp\", \"{to}\"]\nRUN --network=none [\"/bin/testguest\", \"tcp\", \"{to}\"]\n"
        ),
    );
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", ctx.to_str().unwrap()],
        &build_env(&home),
        TIMEOUT,
    );
    let _ = built_tx.send(());
    serving.join().unwrap();
    assert!(
        built.stderr.contains(" tcp 20 hello from the host\n"),
        "{}",
        built.stderr
    );
    // The step without a network fails, and the build with it.
    assert_eq!(built.status, Some(1), "{}", built.stderr);
    assert!(built.stderr.contains(" tcp error "), "{}", built.stderr);
}

/// A fake SSH agent's answer to `request`: one key, `shards-test-key`, listed (11) and
/// signed with (13); anything else refused (SSH_AGENT_FAILURE).
fn agent_answer(request: &[u8]) -> Vec<u8> {
    let string = |out: &mut Vec<u8>, b: &[u8]| {
        out.extend((b.len() as u32).to_be_bytes());
        out.extend_from_slice(b);
    };
    let mut out = Vec::new();
    match request.first() {
        Some(11) => {
            out.push(12);
            out.extend(1u32.to_be_bytes());
            string(&mut out, b"a fake key's blob");
            string(&mut out, b"shards-test-key");
        }
        Some(13) => {
            out.push(14);
            string(&mut out, b"a fake signature");
        }
        _ => out.push(5),
    }
    out
}

/// Serves one connection of the SSH agent protocol on `c`, message by message, until it
/// ends.
fn serve_agent(mut c: impl std::io::Read + std::io::Write) {
    loop {
        let mut n = [0u8; 4];
        if c.read_exact(&mut n).is_err() {
            return;
        }
        let mut request = vec![0u8; u32::from_be_bytes(n) as usize];
        if c.read_exact(&mut request).is_err() {
            return;
        }
        let answer = agent_answer(&request);
        if c.write_all(&(answer.len() as u32).to_be_bytes()).is_err() || c.write_all(&answer).is_err() {
            return;
        }
    }
}

/// A fake agent at a named pipe, as Windows' OpenSSH agent serves its own: a new instance
/// for each client, served on a thread of its own, for the test's life.
fn pipe_agent(name: &str) {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain([0]).collect();
    std::thread::spawn(move || {
        loop {
            // SAFETY: a NUL-terminated name; default security.
            let h = unsafe {
                CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    64 << 10,
                    64 << 10,
                    0,
                    std::ptr::null(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return;
            }
            // SAFETY: the instance just made, owned by the file from here on.
            let pipe = unsafe { std::fs::File::from_raw_handle(h) };
            // SAFETY: the instance's handle; waits for a client (synchronous).
            let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
                || std::io::Error::last_os_error().raw_os_error() == Some(535); // ERROR_PIPE_CONNECTED
            if connected {
                std::thread::spawn(move || serve_agent(pipe));
            }
        }
    });
}

/// `RUN --mount=type=ssh` reaches the client's SSH agent through the builder, as BuildKit's
/// steps reach it: one at an AF_UNIX socket, and one at a named pipe, as Windows' OpenSSH
/// agent serves (BuildKit's winio.DialPipe). The step sees the agent's key and has it sign,
/// and cannot have it forget its keys, which BuildKit's read-only agent refuses.
#[test]
fn run_steps_reach_the_clients_ssh_agent_by_socket_or_pipe() {
    if cannot_run_vms() {
        return;
    }
    let keys = TempDir::new("bw-ssh-agent");
    let sock = keys.join("agent.sock");
    let listener = shards_vmm::platform::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        while let Ok((c, _)) = listener.accept() {
            std::thread::spawn(move || serve_agent(c));
        }
    });
    let pipe = format!(r"\\.\pipe\shards-test-agent-{}", std::process::id());
    pipe_agent(&pipe);
    let (image, _) = served();
    for (what, at) in [
        ("socket", sock.to_string_lossy().into_owned()),
        ("pipe", pipe.clone()),
    ] {
        let home = TempDir::new(&format!("bw-ssh-home-{what}"));
        let ctx = context(
            &format!("bw-ssh-ctx-{what}"),
            &format!("FROM {image}\nUSER root\nRUN --mount=type=ssh [\"/bin/testguest\", \"agent\"]\n"),
        );
        let ssh = format!("default={at}");
        let built = run_shards_env(
            &["build"],
            &[
                "--progress=plain",
                "--no-cache",
                "--ssh",
                ssh.as_str(),
                ctx.to_str().unwrap(),
            ],
            &build_env(&home),
            TIMEOUT,
        );
        let shown = format!(
            "{what}\n--- stdout\n{}\n--- stderr\n{}",
            built.stdout, built.stderr
        );
        assert_eq!(built.status, Some(0), "{shown}");
        for said in [
            " agent keys 1 shards-test-key\n",
            " agent signed\n",
            " agent remove refused\n",
        ] {
            assert!(built.stderr.contains(said), "{said:?}\n{shown}");
        }
    }
}

/// The OCI layout at `out`'s one image's layers, by their diff IDs.
fn layers(out: &std::path::Path) -> serde_json::Value {
    let blob = |digest: &str| -> serde_json::Value {
        let path = out
            .join("blobs")
            .join("sha256")
            .join(digest.trim_start_matches("sha256:"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    };
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("index.json")).unwrap()).unwrap();
    let manifest = blob(top["manifests"][0]["digest"].as_str().unwrap());
    blob(manifest["config"]["digest"].as_str().unwrap())["rootfs"]["diff_ids"].clone()
}

/// The VM process of the builder under `build`, a build's process: its descendant named
/// shards-vm.exe, as Windows' table of processes has them (CreateToolhelp32Snapshot).
fn builder_vm(build: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot of the system's processes, closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut listed = Vec::new();
    // SAFETY: PROCESSENTRY32W is plain data, zeroes a valid value of it.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(size_of::<PROCESSENTRY32W>()).ok()?;
    // SAFETY: the snapshot just taken, and an entry its size says it is.
    let mut more = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        listed.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: the snapshot taken above.
    unsafe { CloseHandle(snapshot) };
    let mut todo = vec![build];
    while let Some(parent) = todo.pop() {
        for (pid, ppid, name) in &listed {
            if *ppid != parent || *pid == parent {
                continue;
            }
            if name.eq_ignore_ascii_case("shards-vm.exe") {
                return Some(*pid);
            }
            todo.push(*pid);
        }
    }
    None
}

/// A builder gives back what it holds cold at its build's word, on Windows (D188 for D133's
/// builders; the platform parity plan's step 7), as tests/build.rs's test of the name has it
/// on Unix: a step reads a 256 MiB file of a cache mount, which an earlier build wrote, into
/// its builder's page cache, then waits; the build is told once that its host is short of
/// memory, through a named pipe it opens as its builder starts (SHARDS_BUILD_PRESSURE), as no
/// unprivileged process may raise Windows' low memory notification; and its builder gives
/// back most of the file, its VM process then holding that much less, the step going on to
/// its end; and the build's layers are the same as the same build's not told.
#[test]
fn a_builder_gives_back_what_it_holds_cold_at_its_builds_word() {
    use std::io::{BufRead as _, Write as _};
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("bw-give-home");
    let env = build_env(&home);
    const MIB: u64 = 256;
    let fill = context(
        "bw-give-fill",
        &format!(
            "FROM {image}\nUSER root\nRUN --mount=type=cache,target=/c [\"/bin/testguest\", \"big\", \"/c/big\", \"{MIB}\"]\n"
        ),
    );
    let filled = run_shards_env(
        &["build"],
        &["--progress=plain", fill.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(filled.status, Some(0), "{filled}");
    let ctx = context(
        "bw-give-ctx",
        &format!(
            "FROM {image}\nUSER root\nARG TOLD\nRUN --mount=type=cache,target=/c [\"/bin/testguest\", \"read-dropped\", \"/c/big\", \"20\", \"/sum\"]\n"
        ),
    );
    // The pipe the build's watcher reads its host's word from, there before the build is.
    let pipe = format!(r"\\.\pipe\shards-test-pressure-{}", std::process::id());
    let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated name and plain arguments.
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        server,
        INVALID_HANDLE_VALUE,
        "{pipe}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the pipe just made, ours alone.
    let mut word = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(server) });
    let build = |told: bool, out: &std::path::Path| {
        let mut cmd = common::command();
        cmd.args([
            "build",
            "--progress=plain",
            "--build-arg",
            "SOURCE_DATE_EPOCH=0",
            "--build-arg",
        ])
        .arg(format!("TOLD={told}"))
        .arg("-o")
        .arg(format!(
            "type=oci,dest={},tar=false,rewrite-timestamp=true",
            out.display()
        ))
        .arg(ctx.as_os_str())
        .envs(env)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
        if told {
            cmd.env("SHARDS_BUILD_PRESSURE", &pipe);
        }
        cmd.spawn().unwrap()
    };
    // What the VM process holds of this host's memory, in KiB, as boot.rs reads a guest's.
    let held_kib = |vm: u32| {
        shards_vmm::platform::process_usage(vm)
            .ok()
            .map(|u| u.footprint >> 10)
    };
    let told_out = TempDir::new("bw-give-told");
    let mut told = build(true, &told_out);
    let stderr = std::io::BufReader::new(told.stderr.take().unwrap());
    let (mut lines, mut before, mut gave, mut after) = (Vec::new(), None, None, None);
    for line in stderr.lines() {
        let line = line.unwrap();
        if before.is_none() && line.contains(&format!("read /c/big {}", MIB << 20)) {
            let vm = builder_vm(told.id()).unwrap_or_else(|| panic!("no builder VM under {}", told.id()));
            before = Some((vm, held_kib(vm)));
            word.write_all(b"\n").unwrap();
        }
        if let Some((_, rest)) = line.split_once("the build's builders gave back ") {
            gave = rest
                .split_whitespace()
                .next()
                .and_then(|mib| mib.parse::<u64>().ok());
            after = before.map(|(vm, _)| held_kib(vm));
        }
        lines.push(line);
    }
    let status = told.wait().unwrap();
    let shown = lines.join("\n");
    assert!(status.success(), "{shown}");
    let gave = gave.unwrap_or_else(|| panic!("nothing given back said:\n{shown}"));
    assert!(
        gave >= MIB / 2,
        "gave back {gave} MiB of the {MIB} MiB file:\n{shown}"
    );
    assert!(
        shown.contains("dropped "),
        "its step's page cache kept the file:\n{shown}"
    );
    let (Some((_, Some(before))), Some(Some(after))) = (before, after) else {
        panic!("its builder's VM process's memory not read:\n{shown}");
    };
    let _ = writeln!(
        std::io::stderr(),
        "its builder gave back {gave} MiB of the {MIB} MiB file; its VM process held {before} KiB, then {after} KiB"
    );
    assert!(
        before.saturating_sub(after) >= (gave << 10) / 2,
        "its builder's VM process held {before} KiB, then {after} KiB, having given back {gave} MiB:\n{shown}"
    );
    let untold_out = TempDir::new("bw-give-untold");
    let untold = build(false, &untold_out).wait_with_output().unwrap();
    assert!(
        untold.status.success(),
        "{}",
        String::from_utf8_lossy(&untold.stderr)
    );
    assert_eq!(layers(&told_out), layers(&untold_out), "{shown}");
}
