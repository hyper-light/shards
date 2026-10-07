//! `shards build`, end to end: an image built from a Dockerfile over a base the build
//! pulls from a registry (a loopback one, as in images.rs), written to the store as
//! BuildKit's exporter writes it, then booted by `shards run` in a real VM, where the
//! build's settings must hold.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, bridge, cannot_run_vms, guest_init, kernel, run_shards_env, served};

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
    if cannot_run_vms() {
        return;
    }
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

    // An empty PATH has nothing in it, not even the working directory (Go's SplitList),
    // as BuildKit's runc finds nothing (measured, Docker Desktop's BuildKit v0.28).
    let ctx = context(
        "build-run-empty-path-ctx",
        &format!("FROM {image}\nENV PATH=\nWORKDIR /bin\nRUN [\"testguest\", \"exit\", \"0\"]\n"),
    );
    let failed = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed
            .stderr
            .contains("exec: \"testguest\": executable file not found in $PATH"),
        "{}",
        failed.stderr
    );
}

/// A build's secrets, ulimits and entitlements reach its `RUN` steps as BuildKit's do: a
/// secret mounted at /run/secrets/ID, mode 0400 and root's, or where and as the step asks,
/// or in a variable, and an optional one missing leaves nothing; every step takes
/// `--ulimit`'s limits, `as` too, which buildx refuses; `--security=insecure` runs with
/// every capability once `--allow security.insecure` grants it, and fails without. No
/// secret is left in the image, and a required one not given fails the build.
#[test]
fn run_steps_take_the_builds_secrets_ulimits_and_entitlements() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-given-home");
    let secrets = TempDir::new("build-given-secrets");
    let token = secrets.join("token");
    std::fs::write(&token, "s3cret").unwrap();
    let ctx = context(
        "build-given-ctx",
        &format!(
            "FROM {image}\n\
             USER root\n\
             RUN --mount=type=secret,id=tok [\"/bin/testguest\", \"stat\", \"/run/secrets/tok\"]\n\
             RUN --mount=type=secret,id=tok,target=/s,mode=0440,uid=7,gid=8 [\"/bin/testguest\", \"stat\", \"/s\"]\n\
             RUN --mount=type=secret,id=var,env=TOKEN --mount=type=secret,id=gone,required=false --mount=type=secret,id=gone,env=GONE,required=false [\"/bin/testguest\", \"report\"]\n\
             RUN --security=insecure [\"/bin/testguest\", \"report\"]\n\
             CMD [\"stat\", \"/run/secrets/tok\", \"/s\"]\n"
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_BUILD_MEMORY", "1024".as_ref()),
        ("SHARDS_TEST_TOKEN", "from the environment".as_ref()),
    ];
    let secret = format!("id=tok,src={}", token.display());
    let args = [
        "--progress=plain",
        "--secret",
        &secret,
        "--secret",
        "id=var,env=SHARDS_TEST_TOKEN",
        "--ulimit",
        "nofile=1024:2048",
        "--ulimit",
        "as=4294967296",
        "--allow",
        "security.insecure",
        "-t",
        "given:1",
        ctx.to_str().unwrap(),
    ];
    let built = run_shards_env(&["build"], &args, &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", built.stdout, built.stderr);
    assert_eq!(built.status, Some(0), "{shown}");
    for line in [
        " /run/secrets/tok file 400 0:0 6\n",
        " = s3cret\n",
        " /s file 440 7:8 6\n",
        " env TOKEN=from the environment\n",
        // An optional secret not given: its variable empty, as loadSecretEnv sets it.
        " env GONE=\n",
        " rlimit-nofile 1024:2048\n",
        " rlimit-as 4294967296:4294967296\n",
    ] {
        assert!(built.stderr.contains(line), "{line:?}\n{shown}");
    }
    let reports = step_reports(&built.stderr);
    assert_eq!(reports.len(), 4, "{shown}");
    let caps = |r: &std::collections::BTreeMap<String, String>| r.get("capeff").cloned().unwrap_or_default();
    assert_eq!(
        caps(&reports[2]),
        "00000000a80425fb",
        "a step as BuildKit runs it\n{shown}"
    );
    assert_ne!(
        caps(&reports[3]),
        "00000000a80425fb",
        "an insecure step, every capability\n{shown}"
    );
    assert!(!caps(&reports[3]).is_empty(), "{shown}");
    // Under moby's default seccomp profile, as BuildKit runs every step (`Seccomp: 2`, a
    // filter); an insecure step unconfined.
    let seccomp =
        |r: &std::collections::BTreeMap<String, String>| r.get("seccomp").cloned().unwrap_or_default();
    assert_eq!(seccomp(&reports[2]), "2", "a step's filter\n{shown}");
    assert_eq!(seccomp(&reports[3]), "0", "an insecure step's none\n{shown}");

    // Nothing of the secrets in the image.
    let ran = run_shards_env(&["run"], &["--rm", "given:1"], &env, TIMEOUT);
    assert_eq!(ran.status, Some(1), "{}", ran.stderr);
    assert!(ran.stdout.contains("/run/secrets/tok missing"), "{}", ran.stdout);
    assert!(ran.stdout.contains("/s missing"), "{}", ran.stdout);

    // Without the grant, an insecure step fails; without a required secret, its step.
    let insecure = context(
        "build-given-insecure-ctx",
        &format!("FROM {image}\nRUN --security=insecure [\"/bin/testguest\", \"exit\", \"0\"]\n"),
    );
    let failed = run_shards_env(&["build"], &[insecure.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed.stderr.contains("security.insecure is not allowed"),
        "{}",
        failed.stderr
    );
    let required = context(
        "build-given-required-ctx",
        &format!(
            "FROM {image}\nRUN --mount=type=secret,id=tok,required=true [\"/bin/testguest\", \"exit\", \"0\"]\n"
        ),
    );
    let failed = run_shards_env(&["build"], &[required.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed.stderr.contains("secret tok: not found"),
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

/// A build given no name is kept all the same, dangling, as dockerd keeps it (measured,
/// Docker 29.3.1): `images -a` lists it as `<untagged>`, and a collection leaves it.
#[test]
fn a_build_given_no_name_is_kept_dangling() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-unnamed-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-unnamed-ctx", &format!("FROM {image}\nLABEL unnamed=1\n"));
    let untagged = |out: &str| out.lines().filter(|row| row.starts_with("<untagged>")).count();
    let built = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let listed = shards(&["images", "-a"]);
    assert_eq!(untagged(&listed.stdout), 1, "{}", listed.stdout);
    // A pull marks a collection due; the next command's runs it.
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    let listed = shards(&["images", "-a"]);
    assert_eq!(untagged(&listed.stdout), 1, "{}", listed.stdout);
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
    for (archive_name, setting, limit, said) in [
        (
            "bomb.tar.gz",
            "SHARDS_MAX_IMAGE_BYTES",
            "16777216",
            "ERROR: what ADD fetches and unpacks comes to more than",
        ),
        (
            "many.tar.gz",
            "SHARDS_MAX_IMAGE_ENTRIES",
            "1000",
            "ERROR: the archives ADD unpacks hold more than",
        ),
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
        assert!(r.stderr.contains(said), "{shown}");
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
    assert_eq!(greeting, format!("from {}\n", bridge().gateway()));
    let waited = shards(&["wait", "all"]);
    assert_eq!(waited.stdout, "0\n", "{}", waited.stderr);
}

/// Each request a test server was sent: its path, and its fields.
type Asked = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<String>)>>>;

/// A server for `ADD` of URLs: each request's path and fields go to `log`; `/a` answers
/// only once `/b` has been asked for, `/hang` holds its request until `release`, and
/// `/endless` sends a body that never ends.
fn url_server(log: Asked, release: std::sync::mpsc::Receiver<()>) -> u16 {
    use std::io::{BufRead as _, Write as _};
    use std::sync::{Arc, Condvar, Mutex};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked_b = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new(Mutex::new(release));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (log, asked_b, release) = (log.clone(), asked_b.clone(), release.clone());
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut fields = Vec::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) <= 2 {
                        break;
                    }
                    fields.push(h.trim_end().to_string());
                }
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push((path.clone(), fields));
                let gz: &[u8] = &[
                    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 171, 202, 44, 40, 72, 77, 225, 2, 0, 172, 0, 58, 199,
                    7, 0, 0, 0,
                ];
                let (head, body): (&str, &[u8]) = match path.as_str() {
                    "/file.txt" => (
                        "200 OK\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
                        b"hello\n",
                    ),
                    "/plain" => ("200 OK", b"x"),
                    "/moved" | "/auth" => ("302 Found\r\nLocation: /final", b""),
                    "/final" => ("200 OK", b"final\n"),
                    "/gz" => ("200 OK\r\nContent-Encoding: GZIP", gz),
                    "/three" => ("300 Multiple Choices", b"choices\n"),
                    "/a" => {
                        let (asked, cv) = &*asked_b;
                        let asked = cv
                            .wait_timeout_while(asked.lock().unwrap(), Duration::from_secs(60), |a| !*a)
                            .unwrap();
                        if *asked.0 {
                            ("200 OK", b"a\n")
                        } else {
                            ("500 Not Concurrent", b"")
                        }
                    }
                    "/b" => {
                        *asked_b.0.lock().unwrap() = true;
                        asked_b.1.notify_all();
                        ("200 OK", b"b\n")
                    }
                    "/hang" => {
                        let _ = release.lock().unwrap().recv();
                        return;
                    }
                    "/endless" => {
                        let mut chunk = b"10000\r\n".to_vec();
                        chunk.extend_from_slice(&[0; 0x10000]);
                        chunk.extend_from_slice(b"\r\n");
                        let _ = out.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
                        while out.write_all(&chunk).is_ok() {}
                        return;
                    }
                    _ => ("404 Not Found", b""),
                };
                let mut response = format!(
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(body);
                let _ = out.write_all(&response);
            });
        }
    });
    port
}

/// Serves what SOURCE_DATE_EPOCH's source stages fetch: `/lm.tar` with a Last-Modified,
/// `/nsec.tar` and `/plain` without, and `/missing`, not found. The archives are those
/// crates/image's newest.go crafted, whose times Go's archive/tar recorded.
fn epoch_server() -> u16 {
    use std::io::{BufRead as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let (head, body) = match line.split(' ').nth(1).unwrap_or("") {
                    "/lm.tar" => (
                        "200 OK\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
                        crafted("links.tar"),
                    ),
                    "/nsec.tar" => ("200 OK", crafted("pax-nsec.tar")),
                    "/plain" => ("200 OK", b"x".to_vec()),
                    _ => ("404 Not Found", Vec::new()),
                };
                let mut response = format!(
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = out.write_all(&response);
            });
        }
    });
    port
}

fn crafted(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../image/testdata/tar-newest")
            .join(name),
    )
    .unwrap()
}

/// SOURCE_DATE_EPOCH naming a stage that only fetches a source takes the build's time
/// from it, as dockerfile/1.27.1 takes it (epoch.go): a URL's Last-Modified when it was
/// fetched without a checksum, else the newest regular file in what it fetched, when
/// that is an archive; none for what is neither, nor for `context`, a local directory;
/// and a source that cannot be fetched fails the build, in BuildKit's words.
#[test]
fn source_date_epoch_is_taken_from_a_source_stage() {
    if cannot_run_vms() {
        return;
    }
    use sha2::Digest as _;
    let url = format!("http://127.0.0.1:{}", epoch_server());
    let home = TempDir::new("build-epoch-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let sum: String = sha2::Sha256::digest(crafted("links.tar"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let sum = format!("sha256:{sum}");
    let build = |n: usize, add: &str, epoch: &str| {
        let ctx = context(
            &format!("build-epoch-ctx-{n}"),
            &format!("FROM scratch AS src\nADD {add} /\n\nFROM scratch\nWORKDIR /w\n"),
        );
        let tag = format!("epoch:{n}");
        let arg = format!("SOURCE_DATE_EPOCH={epoch}");
        let built = shards(&[
            "build",
            "--progress=plain",
            "--build-arg",
            &arg,
            "-t",
            &tag,
            ctx.to_str().unwrap(),
        ]);
        let created = shards(&["image", "inspect", &tag]).stdout;
        (built, created)
    };
    for (n, add, epoch, created) in [
        (1, format!("{url}/lm.tar"), "src", "1994-11-06T08:49:37Z"),
        (
            2,
            format!("--checksum={sum} {url}/lm.tar"),
            "SRC",
            "2027-01-15T08:00:00Z",
        ),
        (3, format!("{url}/nsec.tar"), "src", "2023-11-14T22:13:20Z"),
    ] {
        let (built, inspected) = build(n, &add, epoch);
        assert_eq!(built.status, Some(0), "{add}: {}", built.stderr);
        assert!(
            built
                .stderr
                .contains("[internal] resolve SOURCE_DATE_EPOCH source stage src\n"),
            "{}",
            built.stderr
        );
        assert!(
            inspected.contains(&format!("\"Created\": \"{created}\"")),
            "{add}: {inspected}"
        );
    }
    for (n, add, epoch, step) in [
        (
            4,
            format!("{url}/plain"),
            "src",
            "[internal] resolve SOURCE_DATE_EPOCH source stage src",
        ),
        (
            5,
            format!("{url}/plain"),
            "context",
            "[internal] resolve main build context metadata",
        ),
    ] {
        let (built, inspected) = build(n, &add, epoch);
        assert_eq!(built.status, Some(0), "{add}: {}", built.stderr);
        assert!(built.stderr.contains(step), "{}", built.stderr);
        for time in ["1970-01-01T00:00:00Z", "1994-11-06", "2027-01-15", "2023-11-14"] {
            assert!(!inspected.contains(time), "{add}: {inspected}");
        }
    }
    for (n, add, said) in [
        (
            6,
            format!("{url}/missing"),
            "failed to solve: invalid response status 404",
        ),
        (
            7,
            "https://github.com/moby/buildkit.git#v0.20.0".to_string(),
            "failed to solve: taking SOURCE_DATE_EPOCH from a Git source is not supported yet",
        ),
        (
            8,
            format!("{url}/plain"),
            "failed to solve: invalid SOURCE_DATE_EPOCH: nosuch",
        ),
    ] {
        let epoch = if n == 8 { "nosuch" } else { "src" };
        let (built, _) = build(n, &add, epoch);
        assert_ne!(built.status, Some(0), "{add}");
        assert!(built.stderr.contains(said), "{add}: {}", built.stderr);
    }
}

/// The Dockerfile and ignore files are read whole: BuildKit's frontend refuses either past
/// 16 MiB (dockerui ReadFile, containerd's DefaultMaxRecvMsgSize), its gRPC transport's
/// limit, and a .dockerignore line of 64 KiB (bufio.Scanner), which shards has neither of:
/// a Dockerfile of exactly 16 MiB builds, as it does there, and so do one larger and an
/// ignore file past both.
#[test]
fn files_the_frontend_reads_are_read_whole() {
    if cannot_run_vms() {
        return;
    }
    const MAX: usize = 16 << 20;
    let home = TempDir::new("build-bounded-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    // Lines of 64 bytes: the FROM, then comments.
    let mut exact = format!("FROM scratch{}\n", " ".repeat(51)).into_bytes();
    let comment = format!("#{}\n", "x".repeat(62));
    while exact.len() < MAX {
        exact.extend_from_slice(comment.as_bytes());
    }
    assert_eq!(exact.len(), MAX);
    let ctx = context("build-bounded-ctx", "FROM scratch\n");
    std::fs::write(ctx.join("Dockerfile"), &exact).unwrap();
    let built = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // Past BuildKit's 16 MiB, which its gRPC transport sets and shards has not: built
    // too, where BuildKit refuses it (deviations recorded in architecture.md D33).
    exact.push(b'#');
    exact.push(b'\n');
    std::fs::write(ctx.join("Dockerfile"), &exact).unwrap();
    let built = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    std::fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();
    // A .dockerignore past it, and of one line past bufio.Scanner's 64 KiB.
    let mut ignore = vec![b'#'; MAX + 1];
    ignore.push(b'\n');
    ignore.extend(std::iter::repeat_n(b'a', 70_000));
    std::fs::write(ctx.join(".dockerignore"), &ignore).unwrap();
    let built = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

/// A RUN step's output is clipped as buildkitd clips each stream: past its limit, the
/// rest dropped after a notice, and what came from the clipped write on printed once the
/// step ends; BUILDKIT_STEP_LOG_MAX_SIZE and _MAX_SPEED set the limits as buildkitd reads
/// them, and under them nothing is clipped.
#[test]
fn a_steps_output_is_clipped_as_buildkitd_clips_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let lines: String = (1..=500).map(|i| format!("line-{i:04}\\n")).collect();
    let ctx = context(
        "build-clip-ctx",
        &format!("FROM {image}\nRUN [\"/bin/testguest\", \"stderr\", \"{lines}\"]\n"),
    );
    let home = TempDir::new("build-clip-home");
    let build = |limits: &[(&str, &str)]| {
        let mut env = vec![
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ];
        env.extend(limits.iter().map(|(k, v)| (*k, std::ffi::OsStr::new(v))));
        let built = run_shards_env(
            &[],
            &["build", "--no-cache", "--progress=plain", ctx.to_str().unwrap()],
            &env,
            TIMEOUT,
        );
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
    let printed = build(&[]);
    assert!(
        !printed.contains("clipped") && printed.contains(" line-0500\n"),
        "{printed}"
    );
    let printed = build(&[("BUILDKIT_STEP_LOG_MAX_SIZE", "1000")]);
    let notice = printed
        .find(" [output clipped, log limit 1000B reached]\n")
        .expect(&printed);
    let (before, after) = printed.split_at(notice);
    // Printed lines, not the step's name, which holds them all with escapes.
    assert!(
        before.contains(" line-0100\n") && !before.contains(" line-0101\n"),
        "{printed}"
    );
    assert!(
        after.contains(" line-0101\n") && after.contains(" line-0500\n"),
        "{printed}"
    );
    assert_eq!(printed.matches("[output clipped").count(), 1, "{printed}");
    let printed = build(&[("BUILDKIT_STEP_LOG_MAX_SPEED", "500")]);
    assert!(
        printed.contains(" [output clipped, log limit 500B/s reached]\n"),
        "{printed}"
    );
}

/// ADD's checksum is held to the download by its own algorithm: SHA-384 and SHA-512
/// ones match what they name, where BuildKit hashes with SHA-256 whatever the checksum
/// names, so that none of them ever matches.
#[test]
fn add_checks_a_checksum_by_its_own_algorithm() {
    if cannot_run_vms() {
        return;
    }
    use sha2::Digest as _;
    let url = format!("http://127.0.0.1:{}", epoch_server());
    let home = TempDir::new("build-checksum-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sha384 = format!("sha384:{}", hex(&sha2::Sha384::digest(b"x")));
    let sha512 = format!("sha512:{}", hex(&sha2::Sha512::digest(b"x")));
    let other = format!("sha512:{}", hex(&sha2::Sha512::digest(b"y")));
    let ctx = context(
        "build-checksum-ctx",
        &format!(
            "FROM scratch\nADD --checksum={sha384} {url}/plain /a\nADD --checksum={sha512} {url}/plain /b\n"
        ),
    );
    let built = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ctx = context(
        "build-checksum-bad-ctx",
        &format!("FROM scratch\nADD --checksum={other} {url}/plain /c\n"),
    );
    let failed = shards(&["build", "-q", ctx.to_str().unwrap()]);
    assert_ne!(failed.status, Some(0));
    assert!(
        failed
            .stderr
            .contains(&format!("digest mismatch {sha512}: {other}")),
        "{}",
        failed.stderr
    );
}

/// `ADD` of URLs as BuildKit adds them (dockerfile/1.27.1 source/http through Go's
/// client; measured against Docker Desktop's BuildKit v0.28, and byte for byte on real
/// URLs: scripts/build/realworld/cases/add-url):
/// - a file named for the URL's path, mode 0600 unless `--chmod` says, its mtime the
///   response's `Last-Modified` or the epoch;
/// - redirects followed; a 3xx without one kept; a gzipped body undone;
/// - the userinfo sent as Basic to the URL, not to where it redirects;
/// - every source fetched at once (`/a` answers only once `/b` was asked for);
/// - refused in BuildKit's words: a status from 400 while the cache key is worked out, a
///   checksum that differs (whatever the status) while the snapshot is made;
/// - a failed fetch fails the build at once, while another still hangs.
#[test]
fn add_fetches_urls_as_buildkit_does() {
    if cannot_run_vms() {
        return;
    }
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (release, held) = std::sync::mpsc::channel();
    let port = url_server(log.clone(), held);
    let (image, _) = served();
    let home = TempDir::new("build-add-url-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let url = format!("http://127.0.0.1:{port}");
    let ctx = context(
        "build-add-url-ctx",
        &format!(
            "FROM {image}\nADD {url}/file.txt /a/\nADD --chmod=644 {url}/plain /p\nADD {url}/moved /m\n\
             ADD {url}/gz /gz\nADD {url}/three /three\nADD http://us%65r:pa%40ss@127.0.0.1:{port}/auth /auth\n\
             ADD {url}/a {url}/b /c/\n"
        ),
    );
    let built = shards(&["build", "-t", "added:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let stat = shards(&[
        "run",
        "--rm",
        "-u",
        "root",
        "added:1",
        "stat",
        "/a/file.txt",
        "/p",
        "/m",
        "/gz",
        "/three",
        "/auth",
        "/c/a",
        "/c/b",
    ]);
    assert_eq!(
        stat.stdout,
        "/a/file.txt file 600 0:0 6\n= hello\\n\n/p file 644 0:0 1\n= x\n/m file 600 0:0 6\n= final\\n\n\
         /gz file 600 0:0 7\n= zipped\\n\n/three file 600 0:0 8\n= choices\\n\n/auth file 600 0:0 6\n= final\\n\n\
         /c/a file 600 0:0 2\n= a\\n\n/c/b file 600 0:0 2\n= b\\n\n",
        "{}",
        stat.stderr
    );
    let times = shards(&["run", "--rm", "added:1", "mtime", "/a/file.txt", "/p"]);
    assert_eq!(times.stdout, "/a/file.txt 784111777\n/p 0\n", "{}", times.stderr);
    let asked = log.lock().unwrap().clone();
    let fields = |path: &str| -> Vec<String> {
        asked
            .iter()
            .filter(|(p, _)| p == path)
            .flat_map(|(_, f)| f.iter().map(|f| f.to_ascii_lowercase()))
            .collect()
    };
    assert!(
        fields("/gz").contains(&"accept-encoding: gzip".to_string()),
        "{asked:?}"
    );
    assert!(
        fields("/auth").contains(&"authorization: basic dxnlcjpwyubzcw==".to_string()),
        "{asked:?}"
    );
    assert!(
        !fields("/final").iter().any(|f| f.starts_with("authorization")),
        "{asked:?}"
    );

    let fails = |name: &str, dockerfile: String| {
        let ctx = context(name, &dockerfile);
        shards(&["build", ctx.to_str().unwrap()]).stderr
    };
    let zeros = "0".repeat(64);
    let gone = fails(
        "build-add-url-gone",
        format!("FROM {image}\nADD {url}/nothing /n\n"),
    );
    assert!(
        gone.contains("ERROR: invalid response status 404\n")
            && gone.contains("ERROR: failed to build: failed to solve: failed to load cache key: invalid response status 404"),
        "{gone}"
    );
    let differs = fails(
        "build-add-url-differs",
        format!("FROM {image}\nADD --checksum=sha256:{zeros} {url}/plain /p\n"),
    );
    assert!(
        differs.contains(&format!(
            "ERROR: failed to build: failed to solve: digest mismatch \
             sha256:2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881: sha256:{zeros}"
        )),
        "{differs}"
    );
    // With a checksum the status goes unchecked: the 404's empty body is what differs.
    let missing = fails(
        "build-add-url-missing",
        format!("FROM {image}\nADD --checksum=sha256:{zeros} {url}/nothing /p\n"),
    );
    assert!(
        missing.contains(&format!(
            "digest mismatch sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855: sha256:{zeros}"
        )),
        "{missing}"
    );
    // The hang is still held when the build ends, which cancels it rather than wait
    // for it to time out.
    let started = std::time::Instant::now();
    let hung = fails(
        "build-add-url-hung",
        format!("FROM {image}\nADD {url}/hang /h\nADD {url}/nothing /n\n"),
    );
    assert!(
        hung.contains("failed to load cache key: invalid response status 404"),
        "{hung}"
    );
    assert!(
        started.elapsed() < shards_registry::http::HEAD,
        "{:?}",
        started.elapsed()
    );
    let _ = release.send(());
    // Downloads count against what ADD may write, as unpacked archives do: one past it
    // alone, one that would never end, and two that are only together.
    for (limit, dockerfile) in [
        ("5", format!("FROM scratch\nADD {url}/file.txt /a\n")),
        ("1048576", format!("FROM scratch\nADD {url}/endless /e\n")),
        (
            "10",
            format!("FROM scratch\nADD {url}/file.txt /a\nADD {url}/moved /m\n"),
        ),
    ] {
        let ctx = context(&format!("build-add-url-limit-{limit}"), &dockerfile);
        let limit_value = std::ffi::OsString::from(limit);
        let mut limited = env.to_vec();
        limited.push(("SHARDS_MAX_IMAGE_BYTES", limit_value.as_os_str()));
        let over = run_shards_env(&[], &["build", ctx.to_str().unwrap()], &limited, TIMEOUT);
        assert!(
            over.stderr.contains(&format!(
                "what ADD fetches and unpacks comes to more than {limit} bytes (SHARDS_MAX_IMAGE_BYTES)"
            )),
            "{}",
            over.stderr
        );
    }
}

/// Two bases, the one's tag the start of the other's, as `python:3.12` and
/// `python:3.12-slim` are: each stage is built on its own base's layers.
#[test]
fn bases_whose_names_share_a_start_are_each_their_own() {
    if cannot_run_vms() {
        return;
    }
    let (port, repos) = common::writable_registry();
    {
        let mut repos = repos.lock().unwrap();
        let kind = "application/vnd.oci.image.manifest.v1+json".to_string();
        for (tag, variant) in [("v1", &b"one"[..]), ("v1-x", b"two")] {
            let (manifest, blobs) = common::test_image_with(Some(variant));
            let manifests = repos.manifests.entry("test/base".into()).or_default();
            manifests.insert(tag.into(), (kind.clone(), manifest.clone()));
            manifests.insert(common::sha256_digest(&manifest), (kind.clone(), manifest));
            let stored = repos.blobs.entry("test/base".into()).or_default();
            for blob in blobs {
                stored.insert(common::sha256_digest(&blob), blob);
            }
        }
    }
    let home = TempDir::new("build-two-bases-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let base = format!("127.0.0.1:{port}/test/base");
    let ctx = context(
        "build-two-bases",
        &format!("FROM {base}:v1 AS one\nFROM {base}:v1-x\nCOPY --from=one /etc/variant /one\n"),
    );
    let built = shards(&["build", "-t", "two:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "two:1", "stat", "/etc/variant", "/one"]);
    assert_eq!(
        ran.stdout, "/etc/variant file 644 0:0 3\n= two\n/one file 644 0:0 3\n= one\n",
        "{}",
        ran.stderr
    );
}

/// An Agentfile is read before a Dockerfile beside it, and built as Docker sees it: the
/// ports it listens on exposed, egress ones not, its volumes' mount points listed (D35,
/// AGENTFILE_ARCH.md §12.6, §12.8); an error shows its own lines.
#[test]
fn an_agentfile_is_found_first_and_built_as_docker_sees_it() {
    let (image, _) = served();
    let home = TempDir::new("build-agentfile-home");
    let ctx = context(
        "build-agentfile-ctx",
        &format!("FROM {image}\nLABEL from=dockerfile\n"),
    );
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nNETWORK world\nEXPOSE 7000 AS ingress\nEXPOSE 443 AS egress FOR world\nVOLUME data /data\n"),
    )
    .unwrap();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let built = shards(&[
        "build",
        "--progress=plain",
        "-t",
        "agentfile:1",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    assert!(
        built
            .stderr
            .contains("[internal] load build definition from Agentfile"),
        "{}",
        built.stderr
    );
    let inspected = shards(&["image", "inspect", "agentfile:1"]);
    assert_eq!(inspected.status, Some(0), "{}", inspected.stderr);
    let v: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
    let config = &v[0]["Config"];
    let exposed: Vec<&String> = config["ExposedPorts"].as_object().unwrap().keys().collect();
    assert_eq!(exposed, ["7000/tcp"], "{}", inspected.stdout);
    let volumes: Vec<&String> = config["Volumes"].as_object().unwrap().keys().collect();
    assert_eq!(volumes, ["/data"], "{}", inspected.stdout);
    assert!(config["Labels"].get("from").is_none(), "{}", inspected.stdout);
    // The normalized Agentfile, a layer of its own, its digest the label's.
    let text = std::fs::read(ctx.join("Agentfile")).unwrap();
    let parsed =
        shards_dockerfile::parser::parse_as(&text, shards_dockerfile::parser::Dialect::Agentfile).unwrap();
    let ins =
        shards_dockerfile::instructions::parse(&parsed, &shards_dockerfile::lint::Linter::default()).unwrap();
    let directives: Vec<_> = ins.stages[0]
        .commands
        .iter()
        .filter_map(|c| match &c.kind {
            shards_dockerfile::instructions::Kind::Agentfile(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    let want = shards_dockerfile::agentfile::digest(&shards_dockerfile::agentfile::spec(&directives));
    assert_eq!(
        config["Labels"]["vnd.osi.agentfile.digest"]
            .as_str()
            .map(str::as_bytes),
        Some(want.as_slice()),
        "{}",
        inspected.stdout
    );
    let store = shards_image::store::Store::open(&home.join("images")).unwrap();
    let name = shards_image::reference::Reference::parse("agentfile:1")
        .unwrap()
        .to_string();
    let desc = store.tagged(&name).unwrap().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&store.content(&desc, 1 << 20).unwrap().unwrap()).unwrap();
    let config_desc: shards_image::oci::Descriptor =
        serde_json::from_value(manifest["config"].clone()).unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&store.content(&config_desc, 1 << 20).unwrap().unwrap()).unwrap();
    // One layer more than the base's, the spec's, which its history names (the test
    // image keeps no history, so the exporter adds one for the base's layer as BuildKit's).
    let history = stored["history"].as_array().unwrap();
    assert!(
        history
            .iter()
            .any(|h| h["created_by"] == "AGENTFILE /.agentfile.json" && h.get("empty_layer").is_none()),
        "{stored}"
    );
    assert_eq!(
        stored["rootfs"]["diff_ids"].as_array().unwrap().len(),
        2,
        "{stored}"
    );

    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nSKILL ./review.md FOR ghost\n"),
    )
    .unwrap();
    let refused = shards(&["build", ctx.to_str().unwrap()]);
    assert_eq!(refused.status, Some(1), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("Agentfile:2\n--------------------\n")
            && refused.stderr.contains(
                "dockerfile parse error on line 2: SKILL ... FOR names \"ghost\", which no AGENT or HARNESS of this stage's lineage declares"
            ),
        "{}",
        refused.stderr
    );
}

/// Runs `git` in `dir` with nothing of the user's configuration.
fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["-c", "protocol.file.allow=always"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("GIT_COMMITTER_DATE", "1600000000 +0000")
        .env("GIT_AUTHOR_DATE", "1500000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Smart HTTP for the repositories under `root`, as git's http-backend serves them: each
/// request one stateless run of git's own upload-pack, with the protocol version the
/// client asked for.
/// Repositories under `/private/` answer only requests carrying one of `accepted` as
/// their Authorization.
fn git_http_server(root: std::path::PathBuf, accepted: Vec<String>) -> u16 {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let root = root.clone();
            let accepted = accepted.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut conn = conn;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let target = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                    let (mut length, mut protocol, mut authorization) =
                        (0usize, String::new(), String::new());
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap();
                        match k.to_ascii_lowercase().as_str() {
                            "content-length" => length = v.trim().parse().unwrap(),
                            "git-protocol" => protocol = v.trim().to_string(),
                            "authorization" => authorization = v.trim().to_string(),
                            _ => {}
                        }
                    }
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let path = target.split('?').next().unwrap_or_default();
                    if path.starts_with("/private/") && !accepted.contains(&authorization) {
                        let _ = conn.write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    let (repo, advertise, kind) = if let Some(r) = path.strip_suffix("/info/refs") {
                        (r, true, "application/x-git-upload-pack-advertisement")
                    } else if let Some(r) = path.strip_suffix("/git-upload-pack") {
                        (r, false, "application/x-git-upload-pack-result")
                    } else {
                        let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    };
                    let dir = root.join(repo.trim_start_matches('/'));
                    let mut cmd = std::process::Command::new("git");
                    cmd.args(["upload-pack", "--stateless-rpc"]);
                    if advertise {
                        cmd.arg("--advertise-refs");
                    }
                    let mut child = cmd
                        .arg(&dir)
                        .env("GIT_PROTOCOL", &protocol)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap();
                    child.stdin.take().unwrap().write_all(&body).unwrap();
                    let out = child.wait_with_output().unwrap().stdout;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n",
                        out.len()
                    );
                    if conn
                        .write_all(head.as_bytes())
                        .and_then(|()| conn.write_all(&out))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// `ADD` of Git repositories, as BuildKit's git source makes them, fetched by shards' own
/// client from a real git daemon (`git://`) and over smart HTTP from git's own
/// upload-pack: the default branch, an annotated tag, a subdirectory, a submodule found by
/// its relative URL, `--keep-git-dir`; files 0644 or 0755, symlinks, all root's. Unlike
/// BuildKit, every entry's time is the commit's, so that the same commit makes the same
/// layer: built again without the cache, the layers are the same bytes.
#[test]
fn add_fetches_git_repositories() {
    if cannot_run_vms() {
        return;
    }
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let repos = TempDir::new("build-add-git-repos");
    let (origin, sub) = (repos.join("repo.git"), repos.join("sub.git"));
    std::fs::create_dir_all(&sub).unwrap();
    git_in(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("subfile.txt"), "in the submodule\n").unwrap();
    git_in(&sub, &["add", "-A"]);
    git_in(&sub, &["commit", "-q", "-m", "sub"]);
    std::fs::create_dir_all(origin.join("dir/deep")).unwrap();
    git_in(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("a.txt"), "first\n").unwrap();
    std::fs::write(origin.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(origin.join("dir/deep/d.txt"), "deep\n").unwrap();
    std::os::unix::fs::symlink("a.txt", origin.join("link")).unwrap();
    git_in(&origin, &["add", "-A"]);
    git_in(&origin, &["update-index", "--chmod=+x", "run.sh"]);
    git_in(&origin, &["submodule", "add", "-q", "../sub.git", "mods/sub"]);
    git_in(&origin, &["commit", "-q", "-m", "two"]);
    git_in(&origin, &["tag", "-a", "v1", "-m", "annotated"]);
    let head = git_in(&origin, &["rev-parse", "HEAD"]).trim().to_string();
    // The submodule's URL is relative: each server resolves it to its own sub.git.
    let exec_path = git_in(&origin, &["--exec-path"]);
    let dport = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // Ended however the test ends: left running, it would hold the test's output open.
    struct Ended(std::process::Child);
    impl Drop for Ended {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _daemon = Ended(
        std::process::Command::new(std::path::Path::new(exec_path.trim()).join("git-daemon"))
            .args([
                "--export-all",
                "--reuseaddr",
                "--listen=127.0.0.1",
                &format!("--port={dport}"),
            ])
            .arg(format!("--base-path={}", repos.display()))
            .arg(repos.as_os_str())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let hport = git_http_server(repos.to_path_buf(), Vec::new());
    // The daemon listens a moment after it starts.
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", dport)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let (image, _) = served();
    let home = TempDir::new("build-add-git-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let g = format!("git://127.0.0.1:{dport}/repo.git");
    let h = format!("http://127.0.0.1:{hport}/repo.git");
    let ctx = context(
        "build-add-git-ctx",
        &format!(
            "FROM {image}\nADD {g} /g\nADD {h}#v1 /h\nADD {g}#main:dir /sub\n\
             ADD --keep-git-dir=true {g}#main /k\n"
        ),
    );
    let built = shards(&["build", "-t", "gitted:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let stat = shards(&[
        "run",
        "--rm",
        "-u",
        "root",
        "gitted:1",
        "stat",
        "/g",
        "/g/a.txt",
        "/g/run.sh",
        "/g/link",
        "/g/dir/deep/d.txt",
        "/g/mods/sub/subfile.txt",
        "/g/.git",
        "/h/a.txt",
        "/sub/deep/d.txt",
        "/sub/a.txt",
        "/k/.git/HEAD",
        "/k/.git/shallow",
        "/k/.git/refs/heads/main",
        "/k/mods/sub/.git",
    ]);
    assert_eq!(
        stat.stdout,
        format!(
            "/g dir 755 0:0 132\n/g/a.txt file 644 0:0 6\n= first\\n\n/g/run.sh file 755 0:0 10\n= #!/bin/sh\\n\n\
             /g/link symlink 777 0:0 5\n-> a.txt\n/g/dir/deep/d.txt file 644 0:0 5\n= deep\\n\n\
             /g/mods/sub/subfile.txt file 644 0:0 17\n= in the submodule\\n\n\
             /g/.git missing No such file or directory (os error 2)\n/h/a.txt file 644 0:0 6\n= first\\n\n\
             /sub/deep/d.txt file 644 0:0 5\n= deep\\n\n/sub/a.txt missing No such file or directory (os error 2)\n\
             /k/.git/HEAD file 644 0:0 41\n= {head}\\n\n/k/.git/shallow file 644 0:0 41\n= {head}\\n\n\
             /k/.git/refs/heads/main file 644 0:0 41\n= {head}\\n\n\
             /k/mods/sub/.git file 644 0:0 36\n= gitdir: ../../.git/modules/mods/sub\\n\n"
        ),
        "{}",
        stat.stderr
    );
    // The commit's time, not the clock's at checkout.
    let times = shards(&[
        "run",
        "--rm",
        "gitted:1",
        "mtime",
        "/g",
        "/g/a.txt",
        "/g/dir",
        "/k/.git/HEAD",
    ]);
    assert_eq!(
        times.stdout, "/g 1600000000\n/g/a.txt 1600000000\n/g/dir 1600000000\n/k/.git/HEAD 1600000000\n",
        "{}",
        times.stderr
    );
    // The same commit, the same layers.
    let layers = |tag: &str| shards(&["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag]).stdout;
    let again = shards(&["build", "--no-cache", "-t", "gitted:2", ctx.to_str().unwrap()]);
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    let (first, second) = (layers("gitted:1"), layers("gitted:2"));
    if first != second {
        // Each pair of layers that differ, entry by entry, as tar lists them.
        let list = |digest: &str| {
            let blob = home
                .join("images/blobs/sha256")
                .join(digest.trim_start_matches("sha256:"));
            let out = std::process::Command::new("tar")
                .arg("-tvf")
                .arg(&blob)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let digests = |j: &str| -> Vec<String> { serde_json::from_str(j.trim()).unwrap() };
        let mut shown = String::new();
        for (a, b) in digests(&first).iter().zip(digests(&second)) {
            if *a != b {
                shown.push_str(&format!("--- {a}\n{}--- {b}\n{}", list(a), list(&b)));
            }
        }
        panic!("the same commit made other layers:\n{shown}");
    }
    // A ref the repository lacks is BuildKit's error, on its step.
    let bad = context("build-add-git-bad", &format!("FROM {image}\nADD {g}#nope /x\n"));
    let refused = shards(&["build", bad.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("failed to load cache key: repository does not contain ref nope, output: \"\""),
        "{}",
        refused.stderr
    );
}

/// A repository that asks for credentials is fetched with the build's secrets, as
/// BuildKit's git source takes them: `GIT_AUTH_TOKEN` as `basic` credentials of
/// `x-access-token`, `GIT_AUTH_HEADER.<host>` as the whole header; without them, refused.
#[test]
fn add_fetches_git_with_the_builds_secrets() {
    if cannot_run_vms() {
        return;
    }
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let repos = TempDir::new("build-add-git-auth-repos");
    let origin = repos.join("private/repo.git");
    std::fs::create_dir_all(&origin).unwrap();
    git_in(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("only.txt"), "for the asked\n").unwrap();
    git_in(&origin, &["add", "-A"]);
    git_in(&origin, &["commit", "-q", "-m", "one"]);
    // base64("x-access-token:s3cr3t"), as BuildKit sends a token.
    let token = "basic eC1hY2Nlc3MtdG9rZW46czNjcjN0".to_string();
    let header = "Bearer h3ad3r".to_string();
    let hport = git_http_server(repos.to_path_buf(), vec![token, header]);
    let (image, _) = served();
    let home = TempDir::new("build-add-git-auth-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("TOK", std::ffi::OsStr::new("s3cr3t")),
        ("HDR", std::ffi::OsStr::new("Bearer h3ad3r")),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-add-git-auth-ctx",
        &format!("FROM {image}\nADD http://127.0.0.1:{hport}/private/repo.git /p\n"),
    );
    let refused = shards(&["build", ctx.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains("Authentication failed for"),
        "{}",
        refused.stderr
    );
    for secret in [
        "id=GIT_AUTH_TOKEN,env=TOK".to_string(),
        format!("id=GIT_AUTH_HEADER.127.0.0.1:{hport},env=HDR"),
    ] {
        let built = shards(&[
            "build",
            "--secret",
            &secret,
            "-t",
            "authed:1",
            ctx.to_str().unwrap(),
        ]);
        assert_eq!(built.status, Some(0), "{secret}: {}", built.stderr);
        let stat = shards(&["run", "--rm", "authed:1", "stat", "/p/only.txt"]);
        assert_eq!(
            stat.stdout, "/p/only.txt file 644 0:0 14\n= for the asked\\n\n",
            "{}",
            stat.stderr
        );
    }
}

/// The build cache (D50): built again, every step is answered by the cache, as
/// BuildKit's are (`#N CACHED`), and the image's layers are the same; a changed file of
/// the context runs again what reads it and what follows, not what came before; with
/// `--no-cache`, every step runs.
#[test]
fn builds_reuse_the_steps_they_have_run() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-cache-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-cache-ctx",
        &format!(
            "FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/one=1\"]\nCOPY a.txt /a.txt\n\
             RUN [\"/bin/testguest\", \"fs\", \"write:/two=2\"]\n"
        ),
    );
    std::fs::write(ctx.join("a.txt"), "first\n").unwrap();
    let build = |extra: &[&str], tag: &str| {
        let mut args = vec!["build", "--progress=plain"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-t", tag, ctx.to_str().unwrap()]);
        let built = shards(&args);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
    // Each step's progress, by its name, and whether the cache answered it.
    let cached = |log: &str, step: &str| -> bool {
        let n = log
            .lines()
            .find_map(|l| {
                l.strip_prefix('#')
                    .and_then(|r| r.split_once(' '))
                    .filter(|(_, t)| t.contains(step))
                    .map(|(n, _)| n.to_string())
            })
            .unwrap_or_else(|| panic!("no step {step:?} in\n{log}"));
        log.lines().any(|l| l == format!("#{n} CACHED"))
    };
    let layers = |tag: &str| shards(&["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag]).stdout;
    let first = build(&[], "cached:1");
    for step in ["write:/one=1", "COPY a.txt", "write:/two=2"] {
        assert!(!cached(&first, step), "{step} ran\n{first}");
    }
    let again = build(&[], "cached:2");
    for step in ["write:/one=1", "COPY a.txt", "write:/two=2"] {
        assert!(cached(&again, step), "{step} was cached\n{again}");
    }
    assert_eq!(layers("cached:1"), layers("cached:2"));

    std::fs::write(ctx.join("a.txt"), "second\n").unwrap();
    let changed = build(&[], "cached:3");
    assert!(cached(&changed, "write:/one=1"), "{changed}");
    assert!(!cached(&changed, "COPY a.txt"), "{changed}");
    assert!(!cached(&changed, "write:/two=2"), "{changed}");
    let stat = shards(&["run", "--rm", "-u", "root", "cached:3", "stat", "/a.txt", "/two"]);
    assert_eq!(
        stat.stdout, "/a.txt file 644 0:0 7\n= second\\n\n/two file 644 0:0 1\n= 2\n",
        "{}",
        stat.stderr
    );

    let fresh = build(&["--no-cache"], "cached:4");
    for step in ["write:/one=1", "COPY a.txt", "write:/two=2"] {
        assert!(!cached(&fresh, step), "{step} ran\n{fresh}");
    }

    // `system df` counts the cache's records, `system prune` removes them, as Docker's do;
    // built again, every step runs.
    let df = shards(&["system", "df", "--format", "{{.Type}} {{.TotalCount}}"]);
    let records: usize = df
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("Build Cache "))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(records >= 3, "{}", df.stdout);
    let pruned = shards(&["system", "prune", "-f"]);
    assert_eq!(pruned.status, Some(0), "{}", pruned.stderr);
    assert!(
        pruned.stdout.contains("Deleted build cache objects:\n"),
        "{}",
        pruned.stdout
    );
    let after = build(&[], "cached:5");
    for step in ["write:/one=1", "COPY a.txt", "write:/two=2"] {
        assert!(!cached(&after, step), "{step} ran after the prune\n{after}");
    }
}

/// `RUN --mount=type=ssh` reaches the client's SSH agent through the builder, as
/// BuildKit's steps reach it (`--ssh default`, `SSH_AUTH_SOCK` in the step): the step
/// sees the agent's keys and has it sign, and cannot have it forget them, which BuildKit's
/// read-only agent refuses; without `--ssh`, BuildKit's refusal.
#[test]
fn run_steps_reach_the_clients_ssh_agent() {
    if cannot_run_vms() {
        return;
    }
    let tools = ["ssh-agent", "ssh-keygen", "ssh-add"];
    if tools
        .iter()
        .any(|t| std::process::Command::new(t).arg("-h").output().is_err())
    {
        eprintln!("SKIP: no OpenSSH tools on this host");
        return;
    }
    let keys = TempDir::new("build-ssh-agent");
    let sock = keys.join("agent.sock");
    let mut agent = std::process::Command::new("ssh-agent")
        .args(["-D", "-a"])
        .arg(&sock)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Ended<'a>(&'a mut std::process::Child);
    impl Drop for Ended<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _agent = Ended(&mut agent);
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let key = keys.join("id");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "shards-test-key", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let added = std::process::Command::new("ssh-add")
        .arg(&key)
        .env("SSH_AUTH_SOCK", &sock)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let (image, _) = served();
    let home = TempDir::new("build-ssh-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SSH_AUTH_SOCK", sock.as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-ssh-ctx",
        &format!(
            "FROM {image}\nUSER root\nRUN --mount=type=ssh [\"/bin/testguest\", \"agent\"]\n\
             RUN --security=insecure [\"/bin/testguest\", \"vsock-agent\"]\n"
        ),
    );
    let built = shards(&[
        "build",
        "--progress=plain",
        "--no-cache",
        "--ssh",
        "default",
        "--allow",
        "security.insecure",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // A step given no agent, dialling the relay itself with a token it was never given,
    // is closed on unanswered.
    for line in [
        " agent keys 1 shards-test-key\n",
        " agent signed\n",
        " agent remove refused\n",
        " vsock-agent refused\n",
    ] {
        assert!(built.stderr.contains(line), "{line:?}\n{}", built.stderr);
    }
    // The agent kept its key: the step could not have it forget them.
    let listed = std::process::Command::new("ssh-add")
        .arg("-l")
        .env("SSH_AUTH_SOCK", &sock)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("shards-test-key"));

    // Without --ssh, an optional mount (BuildKit's default) is left out, and a required
    // one refused in BuildKit's words.
    let required = context(
        "build-ssh-required-ctx",
        &format!(
            "FROM {image}\nUSER root\nRUN --mount=type=ssh,required=true [\"/bin/testguest\", \"exit\", \"0\"]\n"
        ),
    );
    let refused = shards(&["build", "--no-cache", required.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("no SSH key \"default\" forwarded from the client"),
        "{}",
        refused.stderr
    );
}

/// `--push`, as `docker build --push` pushes: the image built, stored and named, then
/// pushed under its name, its manifest and every blob it names in the registry; without
/// a name, buildx's refusal.
#[test]
fn builds_push_what_they_build() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("build-push-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-push-ctx", &format!("FROM {image}\nLABEL pushed=yes\n"));
    let refused = shards(&["build", "--push", ctx.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains("tag is needed when pushing to registry"),
        "{}",
        refused.stderr
    );
    let name = format!("127.0.0.1:{port}/team/built:1");
    let built = shards(&["build", "--push", "-t", &name, ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let repos = repos.lock().unwrap();
    let manifests = repos.manifests.get("team/built").expect("the repository pushed");
    let (_, manifest) = manifests.get("1").expect("its tag pushed");
    let manifest: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    let blobs = repos.blobs.get("team/built").expect("its blobs pushed");
    let config = manifest["config"]["digest"].as_str().unwrap();
    assert!(blobs.contains_key(config), "the config pushed");
    for layer in manifest["layers"].as_array().unwrap() {
        let d = layer["digest"].as_str().unwrap();
        assert!(blobs.contains_key(d), "layer {d} pushed");
    }
}

/// The entries of a tar, as Go's reader reads them: each header, and a regular file's bytes.
fn tar_entries(bytes: &[u8]) -> Vec<(shards_archive::tar::Header, Vec<u8>)> {
    use std::io::Read as _;
    let mut r = shards_archive::tar::Reader::new(bytes);
    let mut out = Vec::new();
    while let Some(h) = r.next_header().unwrap() {
        let mut data = Vec::new();
        r.read_to_end(&mut data).unwrap();
        out.push((h, data));
    }
    out
}

/// `--output` writes what BuildKit's exporters write (D52): the whole root filesystem
/// received into a directory (owned by the one who builds, its modes, links and
/// nanosecond times kept), the same as fsutil's tar (times rounded, owners kept, no
/// root), and the image as an OCI or Docker layout of the very blobs the store keeps;
/// none of them leaves an image in the store; what BuildKit refuses before it solves is
/// refused before any step runs.
#[test]
fn builds_write_each_output_as_buildkit_exports_it() {
    use std::os::unix::fs::MetadataExt as _;
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-out-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = context(
        "build-out-ctx",
        &format!(
            "FROM {image}\nUSER root\nCOPY --chown=1000:1000 a.txt /out/owned\n\
             RUN [\"/bin/testguest\", \"fs\", \"write:/out/f=hello\", \"link:/out/f:/out/h\", \
             \"symlink:f:/out/l\", \"mkdir:/out/empty\", \"chmod:4755:/out/f\", \
             \"write:/out/\u{fc}n\u{ef}c\u{f8}d\u{e9}=u\"]\n"
        ),
    );
    std::fs::write(ctx.join("a.txt"), "owned\n").unwrap();
    let out = TempDir::new("build-out-dest");
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let path = |p: &str| out.join(p).to_str().unwrap().to_string();

    // Refused before the build: no step reported.
    for (spec, why) in [
        (
            "type=foo,dest=x",
            "failed to solve: exporter \"foo\" could not be found",
        ),
        (
            "type=docker,tar=false,dest=d",
            "output directory is not supported by moby exporter",
        ),
        (
            "type=oci,tar=maybe,dest=x.tar",
            "non-bool value specified for tar",
        ),
        (
            "type=local",
            "failed to build: dest is required for local exporter",
        ),
    ] {
        let refused = shards(&["build", "--progress=plain", "-o", spec, ctx.to_str().unwrap()]);
        assert_ne!(refused.status, Some(0));
        assert!(refused.stderr.contains(why), "{spec}: {}", refused.stderr);
        assert!(!refused.stderr.contains("#1 "), "{spec} ran: {}", refused.stderr);
    }

    let mut cmd = common::command();
    cmd.args([
        "build",
        "--progress=plain",
        "-t",
        "outs:v1",
        "-o",
        &path("rootfs"),
        "-o",
        "-",
        "-o",
        &format!("type=tar,dest={}", path("nested/a/f.tar")),
        "-o",
        &format!("type=oci,dest={}", path("oci.tar")),
        "-o",
        &format!("type=docker,dest={}", path("docker.tar")),
        "-o",
        &format!("type=oci,tar=false,dest={}", path("layout")),
        ctx.to_str().unwrap(),
    ]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let built = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&built.stderr);
    assert_eq!(built.status.code(), Some(0), "{stderr}");
    for vertex in [
        "exporting to client directory",
        "exporting to client tarball",
        "exporting to oci image format",
        "exporting to docker image format",
    ] {
        assert!(stderr.contains(vertex), "{vertex}: {stderr}");
    }
    assert!(!stderr.contains("exporting to image"), "{stderr}");
    // No image kept: the outputs went to files alone.
    let inspect = shards(&["image", "inspect", "outs:v1"]);
    assert_ne!(inspect.status, Some(0), "{}", inspect.stdout);

    // local: the whole tree, as the builder owns it.
    let root = out.join("rootfs");
    let md = |p: &str| std::fs::symlink_metadata(root.join(p)).unwrap();
    assert_eq!(md(".").mode() & 0o7777, 0o700 & !umask());
    assert!(root.join("bin/testguest").exists(), "the base image's files too");
    assert_eq!(md("out/f").mode() & 0o7777, 0o4755);
    assert_eq!(md("out/f").ino(), md("out/h").ino());
    assert_eq!(
        std::fs::read_link(root.join("out/l")).unwrap().to_str(),
        Some("f")
    );
    assert!(md("out/empty").is_dir());
    let me = std::fs::metadata(&*out).unwrap();
    assert_eq!(
        (md("out/owned").uid(), md("out/owned").gid()),
        (me.uid(), me.gid())
    );
    assert_eq!(std::fs::read(root.join("out/owned")).unwrap(), b"owned\n");

    // tar: stdout's and the file's are one archive.
    let tar = std::fs::read(out.join("nested/a/f.tar")).unwrap();
    assert!(built.stdout == tar, "stdout's tar is the file's");
    assert!(tar.ends_with(&[0u8; 1024]) && tar.len().is_multiple_of(512));
    let entries = tar_entries(&tar);
    let names: Vec<String> = entries
        .iter()
        .map(|(h, _)| String::from_utf8_lossy(&h.name).into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n == "./" || n == "/" || n.is_empty()),
        "no root"
    );
    let mut sorted = names.clone();
    sorted.sort_by(|a, b| {
        a.trim_end_matches('/')
            .split('/')
            .cmp(b.trim_end_matches('/').split('/'))
    });
    assert_eq!(names, sorted, "walk order");
    let entry = |n: &str| &entries.iter().find(|(h, _)| h.name == n.as_bytes()).unwrap().0;
    assert_eq!((entry("out/owned").uid, entry("out/owned").gid), (1000, 1000));
    assert_eq!(entry("out/h").typeflag, b'1');
    assert_eq!(entry("out/h").linkname, b"out/f");
    assert_eq!(entry("out/f").mode, 0o4755);
    assert_eq!(entry("out/empty/").typeflag, b'5');
    assert!(
        names.iter().any(|n| n == "out/\u{fc}n\u{ef}c\u{f8}d\u{e9}"),
        "{names:?}"
    );
    // Times: the snapshot's, to the nearest second.
    let local = md("out/f");
    let rounded = local.mtime() + i64::from(local.mtime_nsec() >= 500_000_000);
    assert_eq!(entry("out/f").mtime.sec, rounded);

    // oci and docker: layouts of the store's own blobs, named as built.
    for (file, docker) in [("oci.tar", false), ("docker.tar", true)] {
        let entries = tar_entries(&std::fs::read(out.join(file)).unwrap());
        let names: Vec<&[u8]> = entries.iter().map(|(h, _)| h.name.as_slice()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "{file}: name order");
        let doc = |n: &str| -> serde_json::Value {
            serde_json::from_slice(&entries.iter().find(|(h, _)| h.name == n.as_bytes()).unwrap().1).unwrap()
        };
        for (h, data) in &entries {
            assert_eq!(h.mtime.sec, 0);
            let name = String::from_utf8_lossy(&h.name);
            let mode = match name.as_ref() {
                "index.json" | "manifest.json" => 0o644,
                n if n.ends_with('/') => 0o755,
                _ => 0o444,
            };
            assert_eq!(h.mode, mode, "{file}: {name}");
            if let Some(hex) = name.strip_prefix("blobs/sha256/").filter(|h| !h.is_empty()) {
                use sha2::Digest as _;
                let got: String = sha2::Sha256::digest(data)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                assert_eq!(got, hex, "{file}: {name} is what it is named");
            }
        }
        let index = doc("index.json");
        let entry = &index["manifests"][0];
        assert_eq!(index["manifests"].as_array().unwrap().len(), 1);
        assert_eq!(
            entry["annotations"]["io.containerd.image.name"],
            "docker.io/library/outs:v1"
        );
        assert_eq!(entry["annotations"]["org.opencontainers.image.ref.name"], "v1");
        assert!(
            entry["annotations"]["org.opencontainers.image.created"]
                .as_str()
                .unwrap()
                .ends_with('Z')
        );
        let media = if docker {
            "application/vnd.docker.distribution.manifest.v2+json"
        } else {
            "application/vnd.oci.image.manifest.v1+json"
        };
        assert_eq!(entry["mediaType"], media);
        let digest = entry["digest"].as_str().unwrap().strip_prefix("sha256:").unwrap();
        let manifest = doc(&format!("blobs/sha256/{digest}"));
        assert_eq!(manifest["mediaType"], media);
        if docker {
            let m = doc("manifest.json");
            assert_eq!(m[0]["RepoTags"][0], "outs:v1");
            assert_eq!(
                m[0]["Layers"].as_array().unwrap().len(),
                manifest["layers"].as_array().unwrap().len()
            );
        } else {
            assert!(!names.contains(&b"manifest.json".as_slice()));
        }
    }

    // oci with tar=false: a content store, its index merged into.
    let layout = out.join("layout");
    assert!(layout.join("ingest").is_dir());
    assert_eq!(
        std::fs::metadata(layout.join("oci-layout")).unwrap().mode() & 0o777,
        0o644
    );
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    assert_eq!(
        index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
        "v1"
    );
    let again = shards(&[
        "build",
        "-o",
        &format!("type=oci,tar=false,dest={}", path("layout")),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let refs: Vec<&str> = index["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            m["annotations"]["org.opencontainers.image.ref.name"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(refs, ["v1", "latest"]);

    // SOURCE_DATE_EPOCH: every entry's time.
    let epoch = shards(&[
        "build",
        "--build-arg",
        "SOURCE_DATE_EPOCH=1700000000",
        "-o",
        &format!("type=tar,dest={}", path("epoch.tar")),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(epoch.status, Some(0), "{}", epoch.stderr);
    for (h, _) in tar_entries(&std::fs::read(out.join("epoch.tar")).unwrap()) {
        if h.typeflag != b'x' {
            assert_eq!(h.mtime.sec, 1_700_000_000, "{}", String::from_utf8_lossy(&h.name));
        }
    }
}

/// The process's umask, as a directory made with 0777 shows it.
fn umask() -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    let probe = TempDir::new("umask-probe");
    let dir = probe.join("d");
    std::fs::DirBuilder::new().create(&dir).unwrap();
    0o777 & !(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777)
}

/// `--build-context` gives a build named contexts as buildx and BuildKit do: a directory
/// a `COPY --from` reads (its own .dockerignore heeded), an image a `FROM` names in place
/// of the one it says, and a stage replaced whole by a directory; a missing directory is
/// refused in buildx's words.
#[test]
fn named_contexts_stand_in_for_what_they_name() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-named-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-named-ctx",
        "FROM scratch AS deps\nCOPY never /d\n\
         FROM base\nCOPY --from=src /x /from-src/\nCOPY --from=deps /d /from-deps/\n",
    );
    // The stage's own steps never run: what it would copy is not even there.
    let src = TempDir::new("build-named-src");
    std::fs::create_dir(src.join("x")).unwrap();
    std::fs::write(src.join("x/kept"), "kept\n").unwrap();
    std::fs::write(src.join("x/ignored"), "ignored\n").unwrap();
    std::fs::write(src.join(".dockerignore"), "x/ignored\n").unwrap();
    let deps = TempDir::new("build-named-deps");
    std::fs::create_dir(deps.join("d")).unwrap();
    std::fs::write(deps.join("d/dep"), "dep\n").unwrap();
    let out = TempDir::new("build-named-out");
    let built = shards(&[
        "build",
        "--progress=plain",
        "--build-context",
        &format!("base=docker-image://{image}"),
        "--build-context",
        &format!("src={}", src.to_str().unwrap()),
        "--build-context",
        &format!("deps={}", deps.to_str().unwrap()),
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    assert!(
        built.stderr.contains("[context base] load metadata for"),
        "{}",
        built.stderr
    );
    assert!(
        built.stderr.contains("[context src] load from client"),
        "{}",
        built.stderr
    );
    let root = out.join("root");
    assert!(root.join("bin/testguest").exists(), "the named image is the base");
    assert_eq!(std::fs::read(root.join("from-src/kept")).unwrap(), b"kept\n");
    assert!(
        !root.join("from-src/ignored").exists(),
        "its .dockerignore heeded"
    );
    assert_eq!(std::fs::read(root.join("from-deps/dep")).unwrap(), b"dep\n");

    let missing = shards(&[
        "build",
        "--build-context",
        "src=/nonexistent-shards-context",
        ctx.to_str().unwrap(),
    ]);
    assert_ne!(missing.status, Some(0));
    assert!(
        missing.stderr.contains(
            "failed to get build context src: stat /nonexistent-shards-context: no such file or directory"
        ),
        "{}",
        missing.stderr
    );
}

/// An `oci-layout://` context is the image an OCI layout holds, as buildx serves one to
/// BuildKit: found by its tag (`latest` when none is given) in the layout's index, its
/// blobs checked and taken into the store, and built on as a base; the layout's only
/// entry taken for a tag it lacks, as buildx takes it; a directory with no index refused
/// in buildx's words.
#[test]
fn oci_layout_contexts_are_the_images_they_hold() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-layout-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let out = TempDir::new("build-layout-out");
    let layout = out.join("layout");
    let first = context("build-layout-first", &format!("FROM {image}\nCOPY made /made\n"));
    std::fs::write(first.join("made"), "in the layout\n").unwrap();
    let made = shards(&[
        "build",
        "-t",
        "laid:v1",
        "-o",
        &format!("type=oci,tar=false,dest={}", layout.to_str().unwrap()),
        first.to_str().unwrap(),
    ]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);

    let second = context("build-layout-second", "FROM base\nCOPY more /more\n");
    std::fs::write(second.join("more"), "on top\n").unwrap();
    let built = shards(&[
        "build",
        "--progress=plain",
        "--build-context",
        &format!("base=oci-layout://{}:v1", layout.to_str().unwrap()),
        "-o",
        out.join("root").to_str().unwrap(),
        second.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    assert!(
        built.stderr.contains("[context base] OCI load from client"),
        "{}",
        built.stderr
    );
    let root = out.join("root");
    assert_eq!(std::fs::read(root.join("made")).unwrap(), b"in the layout\n");
    assert_eq!(std::fs::read(root.join("more")).unwrap(), b"on top\n");
    assert!(root.join("bin/testguest").exists());

    // A tag the index lacks: its one entry, as buildx's resolveDigest falls back to it.
    let single = shards(&[
        "build",
        "--build-context",
        &format!("base=oci-layout://{}:nope", layout.to_str().unwrap()),
        second.to_str().unwrap(),
    ]);
    assert_eq!(single.status, Some(0), "{}", single.stderr);
    let none = out.join("empty");
    std::fs::create_dir(&none).unwrap();
    let missing = shards(&[
        "build",
        "--build-context",
        &format!("base=oci-layout://{}", none.to_str().unwrap()),
        second.to_str().unwrap(),
    ]);
    assert_ne!(missing.status, Some(0));
    assert!(
        missing.stderr.contains("could not be resolved: could not read")
            && missing.stderr.contains("index.json: no such file or directory"),
        "{}",
        missing.stderr
    );
}

/// An Agentfile's `AGENT`, `HARNESS`, `MCP` and `SKILL` lay out what they bring, each a
/// layer of its own (D54): an agent's directory at `/agents/<name>`, a harness's archive
/// unpacked at `/harness/<name>`, an MCP server over stdio at its grantee's
/// `/agents/<name>.d/mcp/<server>`, a remote one fetched not at all, skills checked as the
/// Agent Skills reference checks them and laid out by their names, for one agent or all;
/// and a skill the reference refuses fails the build, saying why.
#[test]
fn agentfile_directives_lay_out_what_they_bring() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-domains-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-domains-ctx", &format!("FROM {image}\n"));
    let write = |rel: &str, text: &str| {
        let p = ctx.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write("agent/run.sh", "#!/bin/sh\necho agent\n");
    write("agent/lib/data.txt", "data\n");
    write("mcp-files/server.py", "print('mcp')\n");
    write(
        "skills/pdf-tools/SKILL.md",
        "---\nname: pdf-tools\ndescription: Works with PDF files.\n---\nUse it.\n",
    );
    write("skills/pdf-tools/scripts/x.sh", "echo x\n");
    write(
        "shared/review/SKILL.md",
        "---\nname: review\ndescription: Reviews code. Use when asked to review.\n---\n",
    );
    write(
        "shared/notes/SKILL.md",
        "---\nname: notes\ndescription: Takes notes.\n---\n",
    );
    // A harness as an archive: unpacked where it goes.
    let tar = ctx.join("harness.tar");
    let status = std::process::Command::new("tar")
        .args([
            "-cf",
            tar.to_str().unwrap(),
            "-C",
            ctx.join("agent").to_str().unwrap(),
            "run.sh",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n\
             AGENT main FROM ./agent\n\
             HARNESS drive FROM ./harness.tar\n\
             MCP files FROM ./mcp-files FOR main\n\
             MCP web FROM https://mcp.example.com/sse\n\
             SKILL ./skills/pdf-tools FOR main\n\
             SKILL ./shared\n\
             ATTACH main FOR drive\n"
        ),
    )
    .unwrap();
    let out = TempDir::new("build-domains-out");
    let built = shards(&[
        "build",
        "--progress=plain",
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let root = out.join("root");
    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    assert_eq!(read("agents/main/run.sh"), "#!/bin/sh\necho agent\n");
    assert_eq!(read("agents/main/lib/data.txt"), "data\n");
    assert_eq!(read("harness/drive/run.sh"), "#!/bin/sh\necho agent\n");
    assert_eq!(read("agents/main.d/mcp/files/server.py"), "print('mcp')\n");
    assert!(!root.join("mcp/web").exists(), "a remote server is not fetched");
    assert!(
        !root.join("mcp/files").exists(),
        "a granted server is its grantee's alone"
    );
    assert!(read("agents/main.d/skills/pdf-tools/SKILL.md").contains("name: pdf-tools"));
    assert_eq!(read("agents/main.d/skills/pdf-tools/scripts/x.sh"), "echo x\n");
    assert!(read("skills/review/SKILL.md").contains("name: review"));
    assert!(read("skills/notes/SKILL.md").contains("name: notes"));
    assert!(
        !root.join("skills/pdf-tools").exists(),
        "a granted skill is its grantee's alone"
    );

    // A skill the reference refuses: the build fails, saying which and why.
    write(
        "shared/Bad_Name/SKILL.md",
        "---\nname: Bad_Name\ndescription: d\n---\n",
    );
    let refused = shards(&["build", ctx.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("skill Bad_Name: Skill name 'Bad_Name' must be lowercase"),
        "{}",
        refused.stderr
    );
}

/// Agents are OSI artifacts (D54, §12.17): `shards build agent` makes one of a directory
/// and its `agent.json`, refusing what no domain may hold; `shards push agent` puts it in
/// a registry as OCI 1.1's own-config-and-layers artifact; `AGENT … FROM` its reference
/// pulls it, checks its type, and lays its content at `/agents/<name>` and its config at
/// `/agents/<name>.d/osi.json`. A harness's `FROM` naming an agent is refused.
#[test]
fn agents_are_osi_artifacts_made_pushed_and_taken() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("build-osi-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("build-osi-agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/run"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"main","version":"1.0.0","run":{"command":["bin/run"]},"asks":{"network":["api.example.com:443"]}}"#,
    )
    .unwrap();
    let name = format!("127.0.0.1:{port}/team/agent:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &name]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    {
        let repos = repos.lock().unwrap();
        let (_, manifest) = repos.manifests["team/agent"]["1"].clone();
        let m: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(m["artifactType"], "application/vnd.osi.agent.v1");
        assert_eq!(
            m["config"]["mediaType"],
            "application/vnd.osi.agent.config.v1+json"
        );
        assert_eq!(
            m["layers"][0]["mediaType"],
            "application/vnd.osi.agent.content.v1.tar"
        );
    }
    let listed = shards(&["ls", "agent"]);
    assert!(listed.stdout.contains(&name), "{}", listed.stdout);
    // Taken from the registry by a build, nothing of it left here first.
    let removed = shards(&["rm", "agent", &name]);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    let ctx = context("build-osi-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM {name}\n"),
    )
    .unwrap();
    let out = TempDir::new("build-osi-out");
    let built = shards(&[
        "build",
        "--progress=plain",
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let root = out.join("root");
    assert_eq!(
        std::fs::read_to_string(root.join("agents/main/bin/run")).unwrap(),
        "#!/bin/sh\necho agent\n"
    );
    assert!(
        !root.join("agents/main/agent.json").exists(),
        "the config is not content"
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("agents/main.d/osi.json")).unwrap()).unwrap();
    assert_eq!(config["name"], "main");
    assert_eq!(config["schemaVersion"], 1);

    // A harness is no agent.
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nHARNESS drive FROM {name}\n"),
    )
    .unwrap();
    let wrong = shards(&["build", ctx.to_str().unwrap()]);
    assert_ne!(wrong.status, Some(0));
    assert!(
        wrong.stderr.contains("is an OSI agent, not an OSI harness"),
        "{}",
        wrong.stderr
    );

    // What no domain may hold is refused when the artifact is made.
    std::os::unix::fs::symlink("/etc/passwd", dir.join("escape")).unwrap();
    let refused = shards(&["build", "agent", dir.to_str().unwrap(), "-t", "local/bad:1"]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("escape: a symlink to /etc/passwd, outside the directory"),
        "{}",
        refused.stderr
    );
}

/// An image with agents is checked before it leaves the build (D55, §9.2): an agent's
/// directory holding a symlink out of its domain fails the build, naming the path and
/// where it leads, as does a grant made set-user-ID; one that stays inside builds.
#[test]
fn domains_are_checked_before_an_image_leaves_the_build() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-isolation-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-isolation-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("agent")).unwrap();
    std::fs::write(ctx.join("agent/run"), "run\n").unwrap();
    std::os::unix::fs::symlink("run", ctx.join("agent/inside")).unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\n"),
    )
    .unwrap();
    let ok = shards(&["build", "--progress=plain", ctx.to_str().unwrap()]);
    assert_eq!(ok.status, Some(0), "{}", ok.stderr);
    assert!(
        ok.stderr.contains("checking the domains' isolation"),
        "{}",
        ok.stderr
    );

    std::os::unix::fs::symlink("/etc/passwd", ctx.join("agent/escape")).unwrap();
    let out = shards(&["build", ctx.to_str().unwrap()]);
    assert_ne!(out.status, Some(0));
    assert!(
        out.stderr.contains(
            "/agents/main/escape -> /etc/passwd: a symlink out of the agent main's domain, to the system"
        ),
        "{}",
        out.stderr
    );
    std::fs::remove_file(ctx.join("agent/escape")).unwrap();

    std::fs::create_dir_all(ctx.join("skills/tool")).unwrap();
    std::fs::write(
        ctx.join("skills/tool/SKILL.md"),
        "---\nname: tool\ndescription: A tool.\n---\n",
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\nSKILL --chmod=4755 ./skills/tool FOR main\n"),
    )
    .unwrap();
    let suid = shards(&["build", ctx.to_str().unwrap()]);
    assert_ne!(suid.status, Some(0));
    assert!(
        suid.stderr.contains("/agents/main.d/skills/tool/SKILL.md: set-user-ID or set-group-ID bits (4755) in the agent main's domain"),
        "{}",
        suid.stderr
    );
}

/// Only a domain's own directives write in it (D55, §9.2, §7 Q19.3): a `RUN` writing in an
/// agent's directory fails the build, before the `AGENT` line or after it, as does a
/// `COPY` into its grants; a `RUN` writing elsewhere builds.
#[test]
fn only_a_domains_own_directives_write_in_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-writes-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-writes-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("agent")).unwrap();
    std::fs::write(ctx.join("agent/run"), "run\n").unwrap();
    std::fs::write(ctx.join("note"), "note\n").unwrap();
    let build = |agentfile: String| {
        std::fs::write(ctx.join("Agentfile"), agentfile).unwrap();
        shards(&["build", ctx.to_str().unwrap()])
    };
    let ok = build(format!(
        "FROM {image}\nUSER root\nAGENT main FROM ./agent\nRUN [\"/bin/testguest\", \"fs\", \"write:/elsewhere=1\"]\n"
    ));
    assert_eq!(ok.status, Some(0), "{}", ok.stderr);
    for (agentfile, says) in [
        (
            format!(
                "FROM {image}\nUSER root\nAGENT main FROM ./agent\nRUN [\"/bin/testguest\", \"fs\", \"write:/agents/main/planted=1\"]\n"
            ),
            "it writes /agents/main/planted in the agent main's domain",
        ),
        (
            format!(
                "FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"mkdir:/agents\", \"mkdir:/agents/main\", \"write:/agents/main/early=1\"]\nAGENT main FROM ./agent\n"
            ),
            "it writes /agents/main/early in the agent main's domain",
        ),
        (
            format!("FROM {image}\nAGENT main FROM ./agent\nCOPY note /agents/main.d/note\n"),
            "it writes /agents/main.d/note in the agent main's domain",
        ),
    ] {
        let out = build(agentfile);
        assert_ne!(out.status, Some(0), "{says}");
        assert!(out.stderr.contains(says), "{says}:\n{}", out.stderr);
    }
}

/// The extensions' sources expand ARGs and ENV as `ADD`'s do, and `COPY --from=<agent>`
/// copies from the agent's content, its files at the root (D56, §7 Q19.1), into any stage
/// without its agent.
#[test]
fn agents_expand_their_words_and_are_copied_from() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-q19-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-q19-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("agent/bin")).unwrap();
    std::fs::write(ctx.join("agent/bin/run"), "run\n").unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image} AS with-agent\nARG SRC=./agent\nAGENT main FROM $SRC\n\
             FROM {image}\nCOPY --from=main /bin/run /copied/run\n"
        ),
    )
    .unwrap();
    let out = TempDir::new("build-q19-out");
    let built = shards(&[
        "build",
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let root = out.join("root");
    assert_eq!(std::fs::read_to_string(root.join("copied/run")).unwrap(), "run\n");
    assert!(
        !root.join("agents").exists(),
        "the stage copied from keeps its agent"
    );

    // Expanded where it is laid out too.
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nARG SRC=./agent\nAGENT main FROM $SRC\n"),
    )
    .unwrap();
    let laid = shards(&[
        "build",
        "-o",
        out.join("laid").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(laid.status, Some(0), "{}", laid.stderr);
    assert_eq!(
        std::fs::read_to_string(out.join("laid/agents/main/bin/run")).unwrap(),
        "run\n"
    );
}

/// An Agentfile's image carries the normalized Agentfile's digest and its agents' and
/// harnesses' names as manifest annotations (D57), so a registry's listing finds it; a
/// Dockerfile's image carries none.
#[test]
fn an_agentfiles_manifest_says_what_it_holds() {
    use shards_image::reference::Reference;
    use shards_image::store::Store;
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-annot-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("build-annot-ctx", &format!("FROM {image}\nLABEL plain=yes\n"));
    std::fs::create_dir_all(ctx.join("a")).unwrap();
    std::fs::write(ctx.join("a/run"), "run\n").unwrap();
    let manifest = |name: &str| -> serde_json::Value {
        let store = Store::open(&home.join("images")).unwrap();
        let desc = store
            .tagged(&Reference::parse(name).unwrap().to_string())
            .unwrap()
            .unwrap();
        serde_json::from_slice(&store.content(&desc, 1 << 20).unwrap().unwrap()).unwrap()
    };
    let plain = shards(&["build", "-t", "plain:1", ctx.to_str().unwrap()]);
    assert_eq!(plain.status, Some(0), "{}", plain.stderr);
    assert!(manifest("plain:1").get("annotations").is_none());
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT zed FROM ./a\nAGENT alpha FROM ./a\nHARNESS drive FROM ./a\nATTACH zed alpha FOR drive\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "agents:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let m = manifest("agents:1");
    let a = &m["annotations"];
    assert_eq!(a["vnd.osi.agentfile.agents"], "alpha,zed");
    assert_eq!(a["vnd.osi.agentfile.harnesses"], "drive");
    assert!(
        a["vnd.osi.agentfile.digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

/// An image's agents run as its microVM starts (D59, AGENTFILE_ARCH.md §9.3), each in its
/// domain: its own uid and no capability, PID 1 of its own PID namespace, its own host
/// name and only a loopback, the system and its own directory read-only, every other
/// domain's hidden, a scratch `/tmp` of its own, a `/dev` of six nodes; its output on the
/// run's stderr, each line prefixed with it. An agent whose config says nothing of how it
/// runs is files alone.
#[test]
fn agents_run_in_their_domains() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("domains-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("domains-agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"main","run":{"command":["bin/testguest","confined","see","/agents/other","/sys","/dev","/proc/self/fd","write","/agents/main/x","/etc/x","/tmp/x"]}}"#,
    )
    .unwrap();
    let name = format!("127.0.0.1:{port}/team/confined:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &name]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context("domains-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("other")).unwrap();
    std::fs::write(ctx.join("other/secret"), "theirs\n").unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM {name}\nAGENT other FROM ./other\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "confined:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // The workload ends once the agent has said what it sees.
    let ran = shards(&["run", "--rm", "confined:1", "await", "confined-ready"]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.stdout, "", "the workload's stdout is its own");
    let said: Vec<&str> = ran
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[agent main] "))
        .collect();
    for want in [
        "confined uid=200000 gid=200000 groups=0",
        "confined pid=1 procs=1",
        "confined hostname=agent-main",
        "confined caps eff=0000000000000000 prm=0000000000000000 inh=0000000000000000 bnd=0000000000000000 amb=0000000000000000 nnp=1",
        // Another domain, and /sys, hidden; its own /dev.
        "confined see /agents/other: errno 13",
        "confined see /sys: errno 13",
        "confined see /dev: fd,full,null,random,shm,stderr,stdin,stdout,tty,urandom,zero",
        // Its stdio and the directory being listed: nothing of init's.
        "confined see /proc/self/fd: 0,1,2,3",
        // Its directory and the system read-only; its scratch its own.
        "confined write /agents/main/x: errno 30",
        "confined write /etc/x: errno 30",
        "confined write /tmp/x: ok",
        "confined net=lo",
    ] {
        assert!(said.contains(&want), "no {want:?} in:\n{}", ran.stderr);
    }
    assert!(
        !ran.stderr.contains("[agent other]"),
        "an agent of files alone runs nothing:\n{}",
        ran.stderr
    );
}
