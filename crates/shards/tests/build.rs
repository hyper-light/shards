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

/// The root filesystem a build wrote of `reference`'s image from its last snapshot is the
/// one the store stacks of its layers (shards_build::stack), byte for byte.
/// Which way a build's export went, from the line SHARDS_TIMING (which the tests set) has
/// it print: "snapshot",
/// or "layers: " and why.
fn exported_from(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find_map(|l| l.strip_prefix("shards-export "))
        .unwrap_or_else(|| panic!("no export line in\n{stderr}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    match v["from"].as_str().unwrap() {
        "layers" => format!("layers: {}", v["why"].as_str().unwrap()),
        other => other.to_string(),
    }
}

fn assert_rootfs_is_its_layers(home: &std::path::Path, reference: &str) {
    use shards_image::reference::{Digest, Reference};
    use shards_image::store::{Layer, Limits, Store};
    let store = Store::open(&home.join("images")).unwrap();
    let name = Reference::parse(reference).unwrap().to_string();
    let desc = store.tagged(&name).unwrap().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&store.content(&desc, 1 << 20).unwrap().unwrap()).unwrap();
    let config_desc: shards_image::oci::Descriptor =
        serde_json::from_value(manifest["config"].clone()).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&store.content(&config_desc, 1 << 20).unwrap().unwrap()).unwrap();
    let layers: Vec<Layer> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .zip(config["rootfs"]["diff_ids"].as_array().unwrap())
        .map(|(l, d)| Layer {
            blob: Digest::parse(l["digest"].as_str().unwrap()).unwrap(),
            media_type: l["mediaType"].as_str().unwrap().to_string(),
            diff_id: Digest::parse(d.as_str().unwrap()).unwrap(),
        })
        .collect();
    let path = store.rootfs(&layers, &Limits::none()).unwrap();
    let built = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let stacked = std::fs::read(store.rootfs(&layers, &Limits::none()).unwrap()).unwrap();
    assert!(
        built == stacked,
        "{reference}: the build's root filesystem is not its layers'"
    );
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
    // The tests' SHARDS_TIMING adds its machine-readable line; nothing else is printed.
    assert!(
        q1.stderr.lines().all(|l| l.starts_with("shards-")),
        "-q prints no progress: {}",
        q1.stderr
    );
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

/// What each `RUN` step printed, by step, as the plain progress shows it: `key value`
/// lines, less the seconds.
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

/// `RUN` steps run in a builder microVM as BuildKit runs them (docs/research/
/// buildkit-run.md, with the values Docker Desktop's BuildKit showed, 2026-10-02): PID 1
/// of their own namespaces, `buildkitsandbox`, HOME from passwd, BuildKit's capabilities
/// for root and none but the bounding set for any other user, umask 0022, a working
/// directory made for the step's user; and what a step changes is its layer, a removal a
/// whiteout, as the image then boots.
#[test]
fn run_steps_run_in_a_builder_as_buildkit_runs_them() {
    let (image, _) = served();
    let home = TempDir::new("build-run-home");
    let ctx = context(
        "build-run-ctx",
        &format!(
            "FROM {image}\n\
             USER root\n\
             RUN [\"/bin/testguest\", \"report\"]\n\
             RUN [\"/bin/testguest\", \"fs\", \"mkdir:/opt\", \"write:/opt/f=data\", \"link:/opt/f:/opt/g\", \"symlink:f:/opt/s\", \"chmod:640:/opt/f\", \"rm:/etc/group\"]\n\
             USER 1000:1000\n\
             WORKDIR /made/by/run\n\
             RUN [\"/bin/testguest\", \"report\"]\n\
             USER root\n\
             CMD [\"stat\", \"/opt/f\", \"/opt/g\", \"/opt/s\", \"/etc/group\", \"/made/by/run\", \"/written\"]\n"
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        // A small builder: the steps need little.
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
    ];
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", "-t", "ran:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    let reports = step_reports(&built.stderr);
    assert_eq!(reports.len(), 3, "{shown}");
    let get = |r: &std::collections::BTreeMap<String, String>, k: &str| r.get(k).cloned().unwrap_or_default();
    let root = &reports[0];
    for (k, v) in [
        ("uid", "0"),
        ("gid", "0"),
        ("groups", "0"),
        ("pid", "1"),
        ("hostname", "buildkitsandbox"),
        ("cwd", "/work"),
        ("env", "PATH=/bin"),
        ("umask", "0022"),
        ("capprm", "00000000a80425fb"),
        ("capeff", "00000000a80425fb"),
        ("capbnd", "00000000a80425fb"),
        ("capamb", "0000000000000000"),
        ("nonewprivs", "0"),
        ("writable", "true"),
    ] {
        if k == "env" {
            continue;
        }
        assert_eq!(get(root, k), v, "{k}\n{shown}");
    }
    assert!(built.stderr.contains(" env HOME=/root\n"), "{shown}");
    let user = &reports[2];
    for (k, v) in [
        ("uid", "1000"),
        ("gid", "1000"),
        ("groups", "1000"),
        ("pid", "1"),
        ("cwd", "/made/by/run"),
        ("capprm", "0000000000000000"),
        ("capeff", "0000000000000000"),
        ("capbnd", "00000000a80425fb"),
    ] {
        assert_eq!(get(user, k), v, "{k}\n{shown}");
    }
    assert!(built.stderr.contains(" env HOME=/home/app\n"), "{shown}");
    assert_eq!(exported_from(&built.stderr), "snapshot", "{shown}");
    assert_rootfs_is_its_layers(&home, "ran:1");

    let ran = run_shards_env(&["run"], &["--rm", "ran:1"], &env, TIMEOUT);
    // stat's status says a path was missing: /etc/group, removed.
    assert_eq!(ran.status, Some(1), "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "/opt/f file 640 0:0 4\n= data\n\
         /opt/g file 640 0:0 4\n= data\n\
         /opt/s symlink 777 0:0 1\n-> f\n\
         /etc/group missing No such file or directory (os error 2)\n\
         /made/by/run dir 755 1000:1000 27\n\
         /written file 644 0:0 1\n= x\n",
        "{}",
        ran.stderr
    );

    // A step that fails fails the build, worded as BuildKit words it.
    let ctx = context(
        "build-run-fail-ctx",
        &format!("FROM {image}\nRUN [\"/bin/testguest\", \"exit\", \"3\"]\n"),
    );
    let failed = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed.stderr.ends_with(
            "ERROR: failed to build: failed to solve: process \"/bin/testguest exit 3\" did not complete successfully: exit code: 3\n"
        ),
        "{}",
        failed.stderr
    );
}

/// A `RUN` step reaches the network through its builder's network process, as BuildKit's
/// steps reach their host's: here a server on this host, at the host's own address (its
/// loopback and the gateway are no guest's to reach). With `--network=none` it reaches
/// nothing.
#[test]
fn run_steps_reach_the_network_unless_it_is_none() {
    use std::io::Write as _;
    if common::cannot_run_vms() {
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
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    });
    let (image, _) = served();
    let home = TempDir::new("build-net-home");
    let to = format!("{}:{port}", host.ip());
    let ctx = context(
        "build-net-ctx",
        &format!(
            "FROM {image}\nRUN [\"/bin/testguest\", \"tcp\", \"{to}\"]\nRUN --network=none [\"/bin/testguest\", \"tcp\", \"{to}\"]\n"
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
    ];
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", ctx.to_str().unwrap()],
        &env,
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

/// A step's read-only binds of one layer, a file of the context and a directory of an
/// image, show what BuildKit's would (overlayfs takes no single lower directory without an
/// upper one, which the builder's empty directory makes up); a source the context lacks
/// fails the build before its step runs, as BuildKit's content checksum does.
#[test]
fn steps_bind_one_layer_trees_as_buildkit_binds_them() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-bind-home");
    let ctx = context(
        "build-bind-ctx",
        &format!(
            "FROM {image} AS img\n\
             FROM {image}\n\
             USER root\n\
             RUN --mount=type=bind,source=f,target=/ctx [\"/bin/testguest\", \"stat\", \"/ctx\"]\n\
             RUN --mount=type=bind,from=img,source=/bin,target=/imgbin [\"/bin/testguest\", \"stat\", \"/imgbin/testguest\"]\n\
             RUN --mount=type=bind,source=missing,target=/m [\"/bin/testguest\", \"exit\", \"0\"]\n"
        ),
    );
    std::fs::write(ctx.join("f"), "from the context\n").unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
    ];
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(1), "{}", built.stderr);
    assert!(
        built.stderr.contains(" /ctx file 644 0:0 17\n"),
        "{}",
        built.stderr
    );
    assert!(
        built.stderr.contains(" = from the context\\n\n"),
        "{}",
        built.stderr
    );
    assert!(
        built.stderr.contains(" /imgbin/testguest file 755 0:0 "),
        "{}",
        built.stderr
    );
    // A source the context lacks fails as BuildKit's checksum of it does, before the
    // step runs.
    assert!(
        built.stderr.ends_with(
            "ERROR: failed to build: failed to solve: failed to compute cache key: failed to calculate checksum of ref context: \"/missing\": not found\n"
        ),
        "{}",
        built.stderr
    );
}

/// What a step's setup makes and mounts, it resolves in the step's root, as BuildKit
/// and runc do (fs.RootPath, securejoin): through symlinks an earlier step planted, never
/// past the root. A tmpfs over an absolute symlink covers its target in the image, and a
/// working directory replaced by an absolute symlink is made at its target in the image;
/// resolved from outside the root, both would reach the builder's own files. A working
/// directory under a file fails its step as BuildKit's does, the builder going on.
#[test]
fn steps_resolve_their_paths_in_their_root() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-inroot-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
    ];
    let tg = |ops: &str| format!("RUN [\"/bin/testguest\", \"fs\", {ops}]\n");
    let ctx = context(
        "build-inroot-ctx",
        &format!(
            "FROM {image}\nUSER root\n{}RUN --mount=type=tmpfs,target=/target [\"/bin/testguest\", \"fs\", \"write:/inside/x=1\"]\nWORKDIR /wd/sub\n{}RUN [\"/bin/testguest\", \"exit\", \"0\"]\nCMD [\"stat\", \"/inside/x\", \"/target\", \"/inside2/sub\"]\n",
            tg("\"mkdir:/inside\", \"symlink:/inside:/target\""),
            tg("\"rmdir:/wd\", \"symlink:/inside2:/wd\""),
        ),
    );
    let built = run_shards_env(
        &["build"],
        &["--progress=plain", "-t", "inroot:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = run_shards_env(&["run"], &["--rm", "inroot:1"], &env, TIMEOUT);
    assert_eq!(
        ran.stdout,
        "/inside/x missing No such file or directory (os error 2)\n\
         /target symlink 777 0:0 7\n-> /inside\n\
         /inside2/sub dir 755 0:0 27\n",
        "{}",
        ran.stderr
    );

    let ctx = context(
        "build-inroot-fail-ctx",
        &format!(
            "FROM {image}\nUSER root\nWORKDIR /w/sub\n{}RUN [\"/bin/testguest\", \"exit\", \"0\"]\n",
            tg("\"rmdir:/w\", \"write:/w=x\"")
        ),
    );
    let failed = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed.stderr.ends_with(
            "ERROR: failed to build: failed to solve: process \"/bin/testguest exit 0\" did not complete successfully: working dir /w/sub points to invalid target: lstat /w/sub: not a directory\n"
        ),
        "{}",
        failed.stderr
    );
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
    // COPY --link after other steps: a merge onto a snapshot the stack does not follow.
    assert_eq!(
        exported_from(&built.stderr),
        "layers: a merge onto a snapshot not in its layers' form"
    );
    assert_rootfs_is_its_layers(&home, "files:1");

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

#[test]
fn a_staged_build_copies_from_its_stages_and_images() {
    let (image, _) = served();
    let home = TempDir::new("build-stages-home");
    let ctx = context(
        "build-stages-ctx",
        &format!(
            "FROM {image} AS base\n\
             WORKDIR /src\n\
             COPY a.txt ./\n\
             FROM scratch AS files\n\
             COPY --chmod=0600 b.txt /b.txt\n\
             FROM {image}\n\
             COPY --from=base /src/a.txt /out/a.txt\n\
             COPY --from=files /b.txt /out/b.txt\n\
             COPY --from={image} /etc/group /out/group\n\
             COPY --from=1 /b.txt /out/b1.txt\n\
             USER root\n\
             CMD [\"stat\", \"/out/a.txt\", \"/out/b.txt\", \"/out/group\", \"/out/b1.txt\", \"/src\"]\n"
        ),
    );
    std::fs::write(ctx.join("a.txt"), "a\n").unwrap();
    std::fs::write(ctx.join("b.txt"), "b\n").unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(
        &["build"],
        &["-t", "stages:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    let target = run_shards_env(
        &["build"],
        &["--target", "base", "-t", "stages:base", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown_target = format!("--- stdout\n{}\n--- stderr\n{}", target.stdout, target.stderr);
    assert_eq!(target.status, Some(0), "{shown_target}");
    assert!(
        !target.stderr.contains("COPY --from=base"),
        "--target base builds no later stage\n{shown_target}"
    );
    // A stage from scratch, as hello-world's and the alpine images' official Dockerfiles
    // build: its steps start from scratch's own snapshot, and its image from that snapshot.
    let files = run_shards_env(
        &["build"],
        &["--target", "files", "-t", "stages:files", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    let shown_files = format!("--- stdout\n{}\n--- stderr\n{}", files.stdout, files.stderr);
    assert_eq!(files.status, Some(0), "{shown_files}");
    assert_eq!(exported_from(&built.stderr), "snapshot", "{shown}");
    assert_eq!(exported_from(&target.stderr), "snapshot", "{shown_target}");
    assert_eq!(exported_from(&files.stderr), "snapshot", "{shown_files}");
    assert_rootfs_is_its_layers(&home, "stages:1");
    assert_rootfs_is_its_layers(&home, "stages:base");
    assert_rootfs_is_its_layers(&home, "stages:files");

    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let ran = run_shards_env(&["run"], &["--pull", "never", "--rm", "stages:1"], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(1), "/src is the base stage's alone\n{shown}");
    let group = "root:x:0:\\napp:x:1000:\\nstaff:x:50:app\\n";
    let want = format!(
        "/out/a.txt file 644 0:0 2\n= a\\n\n\
         /out/b.txt file 600 0:0 2\n= b\\n\n\
         /out/group file 644 0:0 {}\n= {group}\n\
         /out/b1.txt file 600 0:0 2\n= b\\n\n",
        group.replace("\\n", "\n").len()
    );
    let got: String = ran
        .stdout
        .lines()
        .take_while(|l| !l.starts_with("/src"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(got, want, "{shown}");
    assert!(ran.stdout.contains("/src missing"), "{shown}");

    // The base stage alone: its own file, none of the final stage's.
    let base = run_shards_env(
        &["run"],
        &[
            "--pull",
            "never",
            "--rm",
            "stages:base",
            "stat",
            "/src/a.txt",
            "/out",
        ],
        &env,
        TIMEOUT,
    );
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", base.stdout, base.stderr);
    assert!(
        base.stdout.starts_with("/src/a.txt file 644 0:0 2\n= a\\n\n"),
        "{shown}"
    );
    assert!(base.stdout.contains("/out missing"), "{shown}");
}

/// The archives `crates/build/testdata/archives.py` writes, by name.
fn archive(name: &str) -> Vec<u8> {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("../../build/testdata/archives.json")).unwrap();
    let b64 = all[name].as_str().unwrap();
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            _ => 63,
        }
    };
    let mut out = Vec::new();
    for chunk in b64.as_bytes().chunks(4) {
        let digits: Vec<u8> = chunk.iter().copied().filter(|&c| c != b'=').collect();
        let n = digits
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &c)| n | (val(c) << (18 - 6 * i)));
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..digits.len() - 1]);
    }
    out
}

#[test]
fn add_unpacks_archives_of_every_compression_in_the_vm() {
    let (image, _) = served();
    let home = TempDir::new("build-add-home");
    let ctx = context(
        "build-add-ctx",
        &format!(
            "FROM {image}\n\
             ADD simple.tar.xz /x/\n\
             ADD simple.tar.gz /g/\n\
             ADD simple.tar.bz2 /b/\n\
             ADD simple.tar.zst /z\n\
             ADD fake.tar /f/\n\
             USER root\n\
             CMD [\"stat\", \"/x/a/x\", \"/x/a/s\", \"/x/a/l\", \"/x/b\", \"/g/b\", \"/b/a/x\", \"/z/b\", \"/f/fake.tar\"]\n"
        ),
    );
    for name in [
        "simple.tar.xz",
        "simple.tar.gz",
        "simple.tar.bz2",
        "simple.tar.zst",
        "fake.tar",
    ] {
        std::fs::write(ctx.join(name), archive(name)).unwrap();
    }
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(&["build"], &["-t", "add:1", ctx.to_str().unwrap()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    assert_eq!(exported_from(&built.stderr), "snapshot", "{shown}");
    assert_rootfs_is_its_layers(&home, "add:1");

    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let ran = run_shards_env(&["run"], &["--pull", "never", "--rm", "add:1"], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{shown}");
    // a/x is the file a/h links to: the link's header, the archive's later word, set it.
    let fake = String::from_utf8(archive("fake.tar"))
        .unwrap()
        .replace('\n', "\\n");
    let want = format!(
        "/x/a/x file 644 0:0 6\n= x-data\n\
         /x/a/s file 4755 0:0 4\n= suid\n\
         /x/a/l symlink 777 0:0 1\n-> x\n\
         /x/b file 644 0:0 3\n= top\n\
         /g/b file 644 0:0 3\n= top\n\
         /b/a/x file 644 0:0 6\n= x-data\n\
         /z/b file 644 0:0 3\n= top\n\
         /f/fake.tar file 644 0:0 {}\n= {fake}\n",
        archive("fake.tar").len()
    );
    assert_eq!(ran.stdout, want, "{shown}");
}

/// An archive that decompresses past the image limits stops the build, as a pull of
/// such an image stops, and leaves nothing behind in the store.
#[test]
fn add_stops_at_the_image_limits_and_leaves_nothing() {
    let (image, _) = served();
    for (archive_name, setting, limit) in [
        ("bomb.tar.gz", "SHARDS_MAX_IMAGE_BYTES", "16777216"),
        ("many.tar.gz", "SHARDS_MAX_IMAGE_ENTRIES", "1000"),
    ] {
        let home = TempDir::new(&format!("build-limit-home-{setting}"));
        let ctx = context(
            &format!("build-limit-ctx-{setting}"),
            &format!("FROM {image}\nADD {archive_name} /x/\n"),
        );
        std::fs::write(ctx.join(archive_name), archive(archive_name)).unwrap();
        let limit = std::ffi::OsString::from(limit);
        let env = [("SHARDS_HOME", home.as_os_str()), (setting, limit.as_os_str())];
        let r = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
        let shown = format!("--- stdout\n{}\n--- stderr\n{}", r.stdout, r.stderr);
        assert_eq!(r.status, Some(1), "{shown}");
        // ADD's own limit, before the archive is written out whole: not the export's.
        assert!(r.stderr.contains("ERROR: the archives ADD unpacks"), "{shown}");
        assert!(r.stderr.contains(&format!("({setting})")), "{shown}");
        let left: Vec<_> = std::fs::read_dir(home.join("images/ingest"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(left.is_empty(), "left in ingest/: {left:?}\n{shown}");
    }
}

/// An image's `STOPSIGNAL` is what `stop` sends its containers unless told otherwise
/// (moby container.StopSignal): SIGUSR1 here, which ends the command 128 + 10.
#[test]
fn an_images_stop_signal_is_what_stop_sends() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-stopsignal-home");
    let ctx = context(
        "build-stopsignal-ctx",
        &format!("FROM {image}\nSTOPSIGNAL SIGUSR1\nCMD [\"sleep\"]\n"),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(
        &["build"],
        &["-t", "usr1:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = run_shards_env(&["run"], &["-d", "--name", "usr1", "usr1:1"], &env, TIMEOUT);
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let stopped = run_shards_env(&["stop"], &["usr1"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let waited = run_shards_env(&["wait"], &["usr1"], &env, TIMEOUT);
    assert_eq!(waited.stdout, "138\n", "{}", waited.stderr);
}

/// An image's `HEALTHCHECK` runs as dockerd runs it: each probe an exec beside the
/// command, the container `(health: starting)` until the first result, then `(healthy)`,
/// or `(unhealthy)` once failures, or probes past their timeout, reach its retries; a
/// run's `--health-*` overrides the image's.
#[test]
fn an_images_healthcheck_runs_as_dockerd_runs_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-health-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let build = |tag: &str, check: &str| {
        let ctx = context(
            &format!("build-health-{tag}"),
            &format!("FROM {image}\nHEALTHCHECK {check}\nCMD [\"sleep\"]\n"),
        );
        let built = run_shards_env(
            &["build"],
            &["-t", &format!("{tag}:1"), ctx.to_str().unwrap()],
            &env,
            TIMEOUT,
        );
        assert_eq!(built.status, Some(0), "{}", built.stderr);
    };
    build(
        "good",
        "--interval=200ms CMD [\"/bin/testguest\", \"exit\", \"0\"]",
    );
    build(
        "bad",
        "--interval=200ms --retries=2 CMD [\"/bin/testguest\", \"exit\", \"1\"]",
    );
    build(
        "slow",
        "--interval=200ms --timeout=100ms --retries=1 CMD [\"/bin/testguest\", \"sleep\"]",
    );
    for (name, image, options) in [
        ("good", "good:1", &[][..]),
        ("bad", "bad:1", &[][..]),
        ("slow", "slow:1", &[][..]),
        // The run's retries over the image's: unhealthy only after 50 failures.
        ("patient", "bad:1", &["--health-retries", "50"][..]),
    ] {
        let mut args = vec!["-d", "--name", name];
        args.extend_from_slice(options);
        args.push(image);
        let ran = run_shards_env(&["run"], &args, &env, TIMEOUT);
        assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    }
    let status = |name: &str| {
        let ps = run_shards_env(&["ps"], &["--no-trunc"], &env, TIMEOUT);
        ps.stdout
            .lines()
            .find(|l| l.ends_with(&format!(" {name}")))
            .map(|l| {
                let up = l.find("Up ").unwrap_or(0);
                l.get(up..).unwrap_or_default().to_string()
            })
            .unwrap_or_default()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let wanted = [
        ("good", "(healthy)"),
        ("bad", "(unhealthy)"),
        ("slow", "(unhealthy)"),
        ("patient", "(health: starting)"),
    ];
    loop {
        let now: Vec<(String, String)> = wanted.iter().map(|(n, _)| (n.to_string(), status(n))).collect();
        if wanted
            .iter()
            .zip(&now)
            .all(|((_, want), (_, got))| got.contains(want))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{now:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Five more probes, each failing: two would have done for the image's retries.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        status("patient").contains("(health: starting)"),
        "{}",
        status("patient")
    );
    for name in ["good", "bad", "slow", "patient"] {
        let _ = run_shards_env(&["rm"], &["-f", name], &env, TIMEOUT);
    }
}

/// `-P`: every port an image `EXPOSE`s published on a port the host picks, as dockerd
/// publishes them; without it, `ps` lists them unpublished.
#[test]
fn an_images_exposed_ports_publish_with_publish_all() {
    use std::io::Read as _;
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-expose-home");
    let ctx = context(
        "build-expose-ctx",
        &format!("FROM {image}\nEXPOSE 7000 7002/tcp\nCMD [\"serve\", \"7000\", \"1\"]\n"),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let built = shards(&["build", "-t", "exposes:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "-d", "--name", "plain", "exposes:1", "sleep"]);
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let ps = shards(&["ps"]);
    assert!(ps.stdout.contains(" 7000/tcp, 7002/tcp "), "{}", ps.stdout);
    let ran = shards(&["run", "-d", "--name", "all", "-P", "exposes:1"]);
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let listed = shards(&["port", "all"]);
    let lines: Vec<&str> = listed.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{}", listed.stdout);
    let port = |i: usize| -> u16 { lines[i].rsplit(':').next().unwrap().parse().unwrap() };
    let (n, m) = (port(0), port(2));
    assert_eq!(
        listed.stdout,
        format!(
            "7000/tcp -> 0.0.0.0:{n}\n7000/tcp -> [::]:{n}\n7002/tcp -> 0.0.0.0:{m}\n7002/tcp -> [::]:{m}\n"
        )
    );
    // The guest listens once it runs: connected to, it says whom it serves.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let greeting = loop {
        let mut got = String::new();
        let read = std::net::TcpStream::connect(("127.0.0.1", n)).and_then(|mut c| {
            c.shutdown(std::net::Shutdown::Write)?;
            c.read_to_string(&mut got)
        });
        if read.is_ok() && !got.is_empty() {
            break got;
        }
        assert!(std::time::Instant::now() < deadline, "{read:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(greeting, "from 172.17.0.1\n");
    let waited = shards(&["wait", "all"]);
    assert_eq!(waited.stdout, "0\n", "{}", waited.stderr);
}
