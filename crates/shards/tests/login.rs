//! `shards login registry` and `logout registry` (D171), against a registry that takes
//! Basic credentials: checked with it, kept as docker keeps them, used by the next push,
//! and forgotten.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use common::{TempDir, guest_init, kernel, program, run_shards_env};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The `PATH` each command here is given: `home`'s empty directory, where no credential
/// helper is found, so that what a login keeps is kept in the test's config and never in
/// this host's keychain or secret store, as the platform's helper would keep it.
fn no_helpers(home: &Path) -> std::path::PathBuf {
    let dir = home.join("path");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `shards ARGS` in `home` with `config` as its docker config, `stdin` on its input: its
/// status, stdout and stderr.
fn shards(home: &Path, config: &Path, args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
    let mut child = common::command()
        .args(args)
        .env("SHARDS_HOME", home)
        .env("DOCKER_CONFIG", config)
        .env("PATH", no_helpers(home))
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .env_remove("DOCKER_AUTH_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A login is checked with its registry, refused with the registry's word where its
/// credentials are not taken; kept in the config as `docker login` keeps it, said to be
/// unencrypted; tried first the next time; used by a push; and forgotten by a logout, the
/// push then refused. Without a terminal, a login that would ask is refused.
#[test]
fn logins_are_checked_kept_used_and_forgotten() {
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("pusher:secret")
    );
    let (port, repos) = common::writable_registry_requiring(Some(basic));
    let registry = format!("127.0.0.1:{port}");
    let home = TempDir::new("login");
    let config = home.join("docker");
    // Refused: the registry's word for it.
    let (status, _, err) = shards(
        &home,
        &config,
        &["login", "-u", "pusher", "-p", "wrong", &registry],
        "",
    );
    assert_eq!(status, Some(1), "{err}");
    assert!(
        err.contains("WARNING! Using --password via the CLI is insecure."),
        "{err}"
    );
    assert!(err.contains("unauthorized"), "{err}");
    // Taken, from stdin, and kept as docker keeps it.
    let (status, out, err) = shards(
        &home,
        &config,
        &["login", "-u", "pusher", "--password-stdin", &registry],
        "secret\n",
    );
    assert_eq!((status, out.as_str()), (Some(0), "Login Succeeded\n"), "{err}");
    let kept = config.join("config.json");
    assert!(
        err.contains(&format!(
            "WARNING! Your credentials are stored unencrypted in '{}'.",
            kept.display()
        )),
        "{err}"
    );
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
    assert_eq!(
        doc["auths"][&registry]["auth"],
        serde_json::json!(base64::engine::general_purpose::STANDARD.encode("pusher:secret")),
        "{doc}"
    );
    // Asked again with nothing given: what was kept, tried first.
    let (status, out, err) = shards(&home, &config, &["login", &registry], "");
    assert_eq!((status, out.as_str()), (Some(0), "Login Succeeded\n"), "{err}");
    assert!(
        err.starts_with("Authenticating with existing credentials... [Username: pusher]\n"),
        "{err}"
    );
    // Used by a push of an image pulled from elsewhere.
    let (image, _) = common::served();
    let path = no_helpers(&home);
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("DOCKER_CONFIG", config.as_os_str()),
        ("PATH", path.as_os_str()),
    ];
    let pulled = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let target = format!("{registry}/team/app:1");
    let tagged = run_shards_env(&["tag"], &[image.as_str(), target.as_str()], &env, TIMEOUT);
    assert_eq!(tagged.status, Some(0), "{tagged}");
    let pushed = run_shards_env(&["push"], &[target.as_str()], &env, TIMEOUT);
    assert_eq!(pushed.status, Some(0), "{pushed}");
    assert!(repos.lock().unwrap().manifests["team/app"].contains_key("1"));
    // Forgotten, and the push refused after.
    let (status, out, err) = shards(&home, &config, &["logout", &registry], "");
    assert_eq!(
        (status, out.as_str()),
        (
            Some(0),
            format!("Removing login credentials for {registry}\n").as_str()
        ),
        "{err}"
    );
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
    assert!(doc["auths"].get(&registry).is_none(), "{doc}");
    let refused = run_shards_env(&["push"], &[target.as_str()], &env, TIMEOUT);
    assert_ne!(refused.status, Some(0), "{refused}");
    // shards' grammar says it so too.
    let (status, out, err) = shards(
        &home,
        &config,
        &["login", "registry", "-u", "pusher", "-p", "-", &registry],
        "secret",
    );
    assert_eq!((status, out.as_str()), (Some(0), "Login Succeeded\n"), "{err}");
    assert!(!err.contains("insecure"), "{err}");
    // Nothing given, nothing kept, and no terminal to ask at.
    let (status, _, err) = shards(&home, &config, &["login", "127.0.0.1:1"], "");
    assert_eq!(
        (status, err.as_str()),
        (
            Some(1),
            "error: cannot perform an interactive login from a non-TTY device\n"
        ),
    );
}

/// Whether process `pid` has gone: reaped, as kill(2) of no signal finds none.
fn gone(pid: i32) -> bool {
    // SAFETY: kill(2) with no signal sends nothing; it asks whether the process is.
    unsafe { libc::kill(pid, 0) != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) }
}

/// The pids `file` records, a line each.
fn pids(file: &Path) -> Vec<i32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// A credential helper that never answers (as Docker Desktop's did on 2026-10-17, its
/// backend gone, for 12 minutes and more) holds no pull from a registry that asks for no
/// credentials: it is asked, never waited for, and ended with the pull. A pull from one that
/// asks says what it waits on, and its client's going ends the helper with it (D184).
#[test]
fn a_credential_helper_that_never_answers_holds_only_what_needs_its_answer() {
    let home = TempDir::new("stuck-helper");
    let config = home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), r#"{"credsStore":"stuck"}"#).unwrap();
    // Its pid recorded, then a wait for ever, its input never read.
    let recorded = home.join("helper-pids");
    program(
        &no_helpers(&home).join("docker-credential-stuck"),
        &format!(
            "#!/bin/sh\necho $$ >> '{}'\nexec /bin/sleep 3613\n",
            recorded.display()
        ),
    );
    let deadline = std::time::Instant::now() + TIMEOUT;
    let ends = |asked: &[i32], what: &str| {
        while !asked.iter().all(|&p| gone(p)) {
            assert!(std::time::Instant::now() < deadline, "{what}: {asked:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // A registry that asks for nothing: pulled, never waiting on the helper, and the helper
    // ended with it (one killed so soon it recorded nothing is gone too).
    let (image, _) = common::served();
    let mut pull = common::command()
        .args(["pull", &image])
        .env("SHARDS_HOME", &*home)
        .env("DOCKER_CONFIG", &config)
        .env("PATH", no_helpers(&home))
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .env_remove("DOCKER_AUTH_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pulled = loop {
        if let Some(status) = pull.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = pull.kill();
            panic!("the pull waited on a helper whose answer its registry never asked for");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = pull.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(pulled.success(), "{err}");
    assert!(!err.contains("Waiting for"), "{err}");
    let asked = pids(&recorded);
    ends(&asked, "ended with the pull");
    // One that asks for Basic credentials: the pull says what it waits on, and its
    // client's going ends the helper.
    let (port, repos) = common::writable_registry_requiring(Some(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("u:p")
    )));
    let name = format!("127.0.0.1:{port}/team/app:1");
    let mut client = common::command()
        .args(["pull", &name])
        .env("SHARDS_HOME", &*home)
        .env("DOCKER_CONFIG", &config)
        .env("PATH", no_helpers(&home))
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .env_remove("DOCKER_AUTH_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let notice = format!(
        "Waiting for docker-credential-stuck to answer for 127.0.0.1:{port} (credsStore in {})",
        config.join("config.json").display()
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = client.stderr.take().unwrap();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut said: Vec<String> = Vec::new();
    while !said.contains(&notice) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => said.push(line),
            Err(_) => panic!("never said {notice:?}: {said:?}"),
        }
    }
    // The registry asked, and the helper it waits on runs.
    assert!(
        repos.lock().unwrap().log.iter().any(|l| l.starts_with("401 ")),
        "the registry challenged"
    );
    // Its pid, once its shell has come to record it.
    let waiting = loop {
        let waiting: Vec<i32> = pids(&recorded)
            .into_iter()
            .filter(|p| !asked.contains(p))
            .collect();
        if !waiting.is_empty() {
            break waiting;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the helper it waits on never ran"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        waiting.iter().all(|&p| !gone(p)),
        "the helper it waits on runs: {waiting:?}"
    );
    client.kill().unwrap();
    let _ = client.wait();
    ends(&waiting, "ended with its client");
    let (status, _, err) = shards(&home, &config, &["stop", "daemon"], "");
    assert_eq!(status, Some(0), "{err}");
}
