//! `shards build`, end to end: an image built from a Dockerfile over a base the build
//! pulls from a registry (a loopback one, as in images.rs), written to the store as
//! BuildKit's exporter writes it, then booted by `shards run` in a real VM, where the
//! build's settings must hold.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// A build context holding `dockerfile` as its Dockerfile.
fn context(name: &str, dockerfile: &str) -> TempDir {
    let dir = TempDir::new(name);
    std::fs::write(dir.join("Dockerfile"), dockerfile).unwrap();
    dir
}

#[test]
fn an_image_built_of_settings_runs_as_built() {
    let (image, _) = served();
    let home = TempDir::new("build-runs-home");
    let ctx = context(
        "build-runs-ctx",
        &format!("FROM {image}\nENV BUILT=yes FROM_IMAGE=overridden\nUSER root\nCMD [\"report\"]\n"),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(
        &["build"],
        &["-t", "built:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    assert!(
        built
            .stderr
            .contains(&format!("[internal] load metadata for {image}")),
        "{shown}"
    );
    assert!(
        built.stderr.contains("naming to docker.io/library/built:1 done"),
        "{shown}"
    );

    // The same build is the same image: the config's digest is its ID.
    let id = |r: &common::Run| r.stdout.trim().to_string();
    let q1 = run_shards_env(&["build"], &["-q", ctx.to_str().unwrap()], &env, TIMEOUT);
    let q2 = run_shards_env(&["image", "build"], &["-q", ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(q1.status, Some(0), "{}", q1.stderr);
    assert!(
        id(&q1).starts_with("sha256:") && id(&q1).len() == 71,
        "{}",
        q1.stdout
    );
    assert_eq!(id(&q1), id(&q2));
    assert!(q1.stderr.is_empty(), "-q prints no progress: {}", q1.stderr);
    assert!(
        built.stderr.contains(&format!("writing image {} done", id(&q1))),
        "{shown}"
    );

    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let ran = run_shards_env(&["run"], &["--pull", "never", "--rm", "built:1"], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{shown}");
    for line in [
        "uid 0",
        "cwd /work",
        "env BUILT=yes",
        "env FROM_IMAGE=overridden",
        "env PATH=/bin",
    ] {
        assert!(ran.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
}

#[test]
fn a_step_shards_cannot_run_yet_fails_the_build_saying_so() {
    let (image, _) = served();
    let home = TempDir::new("build-refused-home");
    let ctx = context("build-refused-ctx", &format!("FROM {image}\nRUN echo hi\n"));
    let env = [("SHARDS_HOME", home.as_os_str())];
    let r = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(r.status, Some(1), "{}", r.stderr);
    assert!(
        r.stderr
            .contains("#5 [2/2] RUN echo hi\n#5 ERROR: RUN echo hi: this step is not supported"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.ends_with("ERROR: failed to build: failed to solve: RUN echo hi: this step is not supported by shards build yet\n"), "{}", r.stderr);
}

#[test]
fn a_dockerfile_error_shows_its_lines() {
    let home = TempDir::new("build-error-home");
    let ctx = context("build-error-ctx", "FROM scratch\nFOO bar\n");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let r = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(r.status, Some(1), "{}", r.stderr);
    let want = "Dockerfile:2\n--------------------\n   1 |     FROM scratch\n   2 | >>> FOO bar\n   3 |     \n--------------------\nERROR: failed to build: failed to solve: dockerfile parse error on line 2: unknown instruction: FOO (did you mean FROM?)\n";
    assert!(r.stderr.ends_with(want), "{}", r.stderr);
}

#[test]
fn files_a_build_copies_are_in_the_vm_as_built() {
    use std::os::unix::fs::PermissionsExt;
    let (image, _) = served();
    let home = TempDir::new("build-files-home");
    let ctx = context(
        "build-files-ctx",
        &format!(
            "FROM {image}\n\
             WORKDIR /app\n\
             COPY a.txt sub/ ./\n\
             COPY --chown=7:8 --chmod=0640 secret.txt /etc/secret\n\
             COPY --link link.txt /linked/\n\
             COPY <<EOF /notes.txt\nhello\nEOF\n\
             COPY . /all/\n\
             USER root\n\
             CMD [\"stat\", \"/app\", \"/app/a.txt\", \"/app/c.txt\", \"/etc/secret\", \"/linked/link.txt\", \"/notes.txt\", \"/all/a.txt\", \"/all/x.log\"]\n"
        ),
    );
    let write = |name: &str, text: &str, mode: u32| {
        let p = ctx.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write("a.txt", "a\n", 0o644);
    write("sub/c.txt", "c\n", 0o600);
    write("secret.txt", "s\n", 0o777);
    write("link.txt", "l\n", 0o644);
    write("x.log", "ignored\n", 0o644);
    write(".dockerignore", "*.log\n", 0o644);
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(
        &["build"],
        &["-t", "files:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    for step in [
        "WORKDIR /app",
        "COPY a.txt sub/ ./",
        "COPY --link link.txt /linked/",
        "COPY . /all/",
    ] {
        assert!(built.stderr.contains(step), "{step}\n{shown}");
    }

    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let ran = run_shards_env(&["run"], &["--pull", "never", "--rm", "files:1"], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
    assert_eq!(
        ran.status,
        Some(1),
        "x.log is ignored, so stat fails on it\n{shown}"
    );
    // The base runs as 1000:1000, and WORKDIR makes its directory for the user it has.
    let want = "/app dir 755 1000:1000 4096\n\
                /app/a.txt file 644 0:0 2\n= a\\n\n\
                /app/c.txt file 600 0:0 2\n= c\\n\n\
                /etc/secret file 640 7:8 2\n= s\\n\n\
                /linked/link.txt file 644 0:0 2\n= l\\n\n\
                /notes.txt file 644 0:0 6\n= hello\\n\n\
                /all/a.txt file 644 0:0 2\n= a\\n\n";
    let got: String = ran
        .stdout
        .lines()
        .take_while(|l| !l.starts_with("/all/x.log"))
        .map(|l| {
            // Directory sizes depend on the file system; only the rest is compared.
            match l.strip_prefix("/app dir ") {
                Some(rest) => format!("/app dir {} 4096\n", rest.rsplit_once(' ').map_or(rest, |r| r.0)),
                None => format!("{l}\n"),
            }
        })
        .collect();
    assert_eq!(got, want, "{shown}");
    assert!(ran.stdout.contains("/all/x.log missing"), "{shown}");
}
