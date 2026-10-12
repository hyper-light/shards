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
    for (k, v) in [("uid", "0"), ("gid", "0"), ("hostname", "buildkitsandbox")] {
        assert_eq!(root.get(k).map(String::as_str), Some(v), "{k}\n{shown}");
    }
    assert_eq!(
        std::fs::read_to_string(out.join("opt").join("f")).unwrap_or_default(),
        "data",
        "{shown}"
    );
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
