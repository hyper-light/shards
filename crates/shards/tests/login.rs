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
use common::{TempDir, guest_init, kernel, run_shards_env};

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
