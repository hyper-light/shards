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

    // The same build is the same image: its manifest's digest is its ID, the one every
    // command names it by, where it carries no provenance; with it, each build's index
    // is its own (its invocation and times are), as Docker's is.
    let id = |r: &common::Run| r.stdout.trim().to_string();
    let mut plain = env.to_vec();
    plain.push(("BUILDX_NO_DEFAULT_ATTESTATIONS", std::ffi::OsStr::new("1")));
    let q1 = run_shards_env(&["build"], &["-q", ctx.to_str().unwrap()], &plain, TIMEOUT);
    let q2 = run_shards_env(
        &["image", "build"],
        &["-q", ctx.to_str().unwrap()],
        &plain,
        TIMEOUT,
    );
    let attested = run_shards_env(&["build"], &["-q", ctx.to_str().unwrap()], &env, TIMEOUT);
    assert_eq!(attested.status, Some(0), "{}", attested.stderr);
    assert_ne!(id(&attested), id(&q1));
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
    // The manifest an unattested build writes is the ID its -q prints (Docker's types,
    // D83); an attested build's manifest is OCI's, and another.
    let shown_plain = run_shards_env(&["build"], &[ctx.to_str().unwrap()], &plain, TIMEOUT);
    assert!(
        shown_plain
            .stderr
            .contains(&format!("writing image {} done", id(&q1))),
        "{}",
        shown_plain.stderr
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
    // The ID it printed names it, as `docker run $(docker build -q .)` has it.
    let id = built.stdout.trim();
    let inspected = shards(&["image", "inspect", "--format", "{{.Id}}", id]);
    assert_eq!(inspected.status, Some(0), "{id}: {}", inspected.stderr);
    let ran = shards(&["run", "--rm", id, "exit", "0"]);
    assert_eq!(ran.status, Some(0), "{id}: {}", ran.stderr);
    // And by its short ID, as `images` shows it.
    let short: String = id.trim_start_matches("sha256:").chars().take(12).collect();
    let ran = shards(&["run", "--rm", &short, "exit", "0"]);
    assert_eq!(ran.status, Some(0), "{short}: {}", ran.stderr);
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

/// An image's `VOLUME`s, as `docker run` mounts them: an anonymous volume at each, filled
/// from the image where it is empty, listed with the run's mounts, kept by `rm` and
/// removed by `rm -v`.
#[test]
fn an_images_volumes_are_anonymous_volumes_at_run() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-volume-home");
    let ctx = context(
        "build-volume-ctx",
        &format!("FROM {image}\nCOPY seed /data/seed\nVOLUME /data\n"),
    );
    std::fs::write(ctx.join("seed"), "from the image\n").unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let built = shards(&["build", "-t", "volumed:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&[
        "run",
        "--name",
        "vol",
        "-u",
        "root",
        "volumed:1",
        "stat",
        "/data/seed",
    ]);
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    assert!(
        ran.stdout
            .starts_with("/data/seed file 644 0:0 15\n= from the image\\n\n"),
        "{}",
        ran.stdout
    );
    let mounts = shards(&[
        "inspect",
        "--format",
        "{{range .Mounts}}{{.Type}} {{.Destination}} {{.Driver}} {{.RW}};{{end}}",
        "vol",
    ]);
    assert_eq!(mounts.stdout, "volume /data local true;\n", "{}", mounts.stderr);
    let listed = shards(&["volume", "ls", "-q"]);
    assert_eq!(listed.stdout.lines().count(), 1, "{}", listed.stdout);
    let removed = shards(&["rm", "-v", "vol"]);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    assert_eq!(shards(&["volume", "ls", "-q"]).stdout, "");
}

/// `--call=outline`, `targets` and `subrequests.describe`, answered as buildx prints
/// BuildKit's frontend's answers (their every byte held to BuildKit's by the Dockerfile
/// oracle): the target's arguments, secrets and SSH agents; the stages; the subrequests;
/// each as text, and as result.json with `format=json`; nothing built.
#[test]
fn call_answers_the_frontends_subrequests() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-call-home");
    let ctx = context(
        "build-call-ctx",
        &format!(
            "# VERSION is stamped in\nARG VERSION=dev\n\n# app is what runs\nFROM {image} AS app\nARG VERSION\n\
             RUN --mount=type=secret,id=token,required=true --mount=type=ssh true\n"
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let outline = shards(&[
        "build",
        "--call=outline",
        "--build-arg",
        "VERSION=2",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(outline.status, Some(0), "{}", outline.stderr);
    // The stage's ARG, given a value, is the one listed, as BuildKit lists it: the global
    // ARG's comment is not its.
    assert_eq!(
        outline.stdout,
        "TARGET:      app\nDESCRIPTION: is what runs\n\nBUILD ARG   VALUE   DESCRIPTION\nVERSION     2       \n\n\
         SECRET   REQUIRED\ntoken    true\n\nSSH       REQUIRED\ndefault   \n\n"
    );
    let json = shards(&["build", "--call=outline,format=json", ctx.to_str().unwrap()]);
    assert_eq!(json.status, Some(0), "{}", json.stderr);
    let v: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(v["name"], "app");
    // Inherited from the global ARG without a value of its own: the global's, comment and
    // all.
    assert_eq!(v["args"][0]["value"], "dev");
    assert_eq!(v["args"][0]["description"], "is stamped in");
    assert_eq!(v["secrets"][0]["required"], true);
    let targets = shards(&["build", "--call=targets", ctx.to_str().unwrap()]);
    assert_eq!(
        targets.stdout,
        "TARGET        DESCRIPTION\napp (default) is what runs\n"
    );
    let described = shards(&["build", "--call=subrequests.describe", ctx.to_str().unwrap()]);
    assert!(
        described
            .stdout
            .starts_with("NAME                 VERSION DESCRIPTION\noutline "),
        "{}",
        described.stdout
    );
    // Nothing was built: the one image is the base the outline resolved.
    assert_eq!(shards(&["images", "-q"]).stdout.lines().count(), 1);
}

/// A stored build carries its provenance as Docker's does by default (D71): the index of
/// the image and its attestation is its ID, in `image inspect` and the metadata file;
/// the attestation's statement names the image by its tag and manifest, its base image
/// as a package URL with the digest it resolved to, and leaves out the build arguments
/// (mode=min), which the metadata file's provenance keeps with the secrets mounted; with
/// BUILDX_NO_DEFAULT_ATTESTATIONS, the ID is the manifest's.
/// `--sbom` runs the scanner it names over the build's result as BuildKit runs one (D81):
/// the image's attestation holds a statement of each target it wrote, the result's first,
/// then the context's and each stage's that asked to be scanned, then the provenance; each
/// names the image, its predicate as the scanner wrote it (Go's escaping); the scanner is
/// given its parameters, and is among the provenance's materials. A local output holds the
/// SBOM beside its files, and no provenance (inline-only).
#[test]
fn sboms_are_scanned_as_buildkit_scans_them() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("build-sbom-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    // The scanner: the test guest's, as an image of its own.
    let scanner_ctx = context(
        "build-sbom-scanner",
        "FROM scratch\nCOPY testguest /bin/testguest\nENTRYPOINT [\"/bin/testguest\", \"sbomscan\"]\n",
    );
    std::fs::copy(common::test_guest(), scanner_ctx.join("testguest")).unwrap();
    let scanner = format!("127.0.0.1:{port}/test/scanner:1");
    let made = shards(&["build", "-t", &scanner, scanner_ctx.to_str().unwrap()]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", &scanner]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context(
        "build-sbom-ctx",
        &format!(
            "FROM {image} AS deps\nARG BUILDKIT_SBOM_SCAN_STAGE=true\nCOPY a /from-deps\n\
             FROM {image}\nARG BUILDKIT_SBOM_SCAN_CONTEXT=true\nCOPY --from=deps /from-deps /d\nCOPY a /a\n"
        ),
    );
    std::fs::write(ctx.join("a"), "a\n").unwrap();
    let sbom_flag = format!("--sbom=generator={scanner},mode=fast");
    let meta = home.join("meta.json");
    let built = shards(&[
        "build",
        "-t",
        "sbom:1",
        &sbom_flag,
        "--metadata-file",
        meta.to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let md: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    let raw = |d: &str| -> Vec<u8> {
        std::fs::read(
            home.join("images/blobs/sha256")
                .join(d.trim_start_matches("sha256:")),
        )
        .unwrap()
    };
    let blob = |d: &str| -> serde_json::Value { serde_json::from_slice(&raw(d)).unwrap() };
    let index = blob(md["containerimage.digest"].as_str().unwrap());
    let manifest_digest = index["manifests"][0]["digest"].as_str().unwrap().to_string();
    let attestation = blob(index["manifests"][1]["digest"].as_str().unwrap());
    let layers = attestation["layers"].as_array().unwrap();
    let kinds: Vec<&str> = layers
        .iter()
        .map(|l| l["annotations"]["in-toto.io/predicate-type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "https://spdx.dev/Document",
            "https://spdx.dev/Document",
            "https://spdx.dev/Document",
            "https://slsa.dev/provenance/v1"
        ]
    );
    let statements: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| blob(l["digest"].as_str().unwrap()))
        .collect();
    let names: Vec<&str> = statements[..3]
        .iter()
        .map(|s| s["predicate"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sbom", "sbom-context", "sbom-deps"]);
    let files = |i: usize| -> Vec<String> {
        statements[i]["predicate"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_string())
            .collect()
    };
    // The result, with what it copied; the context as the build read it; the stage.
    let result = files(0);
    assert!(
        result.contains(&"a".to_string()) && result.contains(&"d".to_string()),
        "{result:?}"
    );
    assert!(result.contains(&"bin/testguest".to_string()), "{result:?}");
    assert_eq!(files(1), ["a"]);
    assert!(files(2).contains(&"from-deps".to_string()), "{:?}", files(2));
    // Its parameters: the protocol's, and each attribute but the generator, `type` too, as
    // BuildKit passes them.
    let params = &statements[0]["predicate"]["params"];
    assert_eq!(params["BUILDKIT_SCAN_SOURCE"], "/run/src/core/sbom");
    assert_eq!(params["BUILDKIT_SCAN_DESTINATION"], "/run/out/");
    assert_eq!(params["BUILDKIT_SCAN_SOURCE_EXTRAS"], "/run/src/extras/");
    assert_eq!(params["BUILDKIT_SCAN_mode"], "fast");
    assert_eq!(params["BUILDKIT_SCAN_type"], "sbom");
    // Each names the image; its predicate as Go writes it, `<`, `>` and `&` escaped.
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    assert_eq!(
        statements[0]["subject"][0]["name"],
        format!("pkg:docker/sbom@1?platform=linux%2F{arch}").as_str()
    );
    assert_eq!(
        format!(
            "sha256:{}",
            statements[0]["subject"][0]["digest"]["sha256"].as_str().unwrap()
        ),
        manifest_digest
    );
    let text = String::from_utf8(raw(layers[0]["digest"].as_str().unwrap())).unwrap();
    assert!(text.contains(r#""note":"a\u003cb \u0026 c\u003ed""#), "{text}");
    // The scanner among the provenance's materials, with no platform, as BuildKit
    // resolves it.
    let deps: Vec<&str> = statements[3]["predicate"]["buildDefinition"]["resolvedDependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["uri"].as_str().unwrap())
        .collect();
    assert!(
        deps.contains(&format!("pkg:docker/127.0.0.1%3A{port}/test/scanner@1").as_str()),
        "{deps:?}"
    );
    // A local output: the SBOM beside the files, naming them; no provenance.
    let out = home.join("local-out");
    let local = shards(&[
        "build",
        &sbom_flag,
        "-o",
        &format!("type=local,dest={}", out.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(local.status, Some(0), "{}", local.stderr);
    assert!(!out.join("provenance.json").exists());
    let sbom: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("sbom.spdx.json")).unwrap()).unwrap();
    let named: Vec<&str> = sbom["subject"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(named.contains(&"a") && named.contains(&"d"), "{named:?}");
    assert!(out.join("sbom-context.spdx.json").exists());
}

/// `--provenance=mode=max` records what BuildKit's does (D80): the request whole (its build
/// arguments too), the LLB definition as steps with each op's digest, where in the
/// Dockerfile each step comes from, the Dockerfile itself, and the layers each step's output
/// is: the base's, then each layer the image is made of, and none for what is only copied
/// from (a heredoc's file, made on scratch, which no image holds).
#[test]
fn provenance_max_records_a_builds_steps_and_layers() {
    use base64::Engine as _;
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-max-home");
    let dockerfile = format!("FROM {image}\nARG A\nCOPY a /a\nCOPY <<EOF /h\nhello\nEOF\n");
    let ctx = context("build-max-ctx", &dockerfile);
    std::fs::write(ctx.join("a"), "a\n").unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let meta = home.join("meta.json");
    let built = shards(&[
        "build",
        "-t",
        "max:1",
        "--provenance=mode=max",
        "--build-arg",
        "A=1",
        "--metadata-file",
        meta.to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let md: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    let blob = |d: &str| -> serde_json::Value {
        let path = home
            .join("images/blobs/sha256")
            .join(d.trim_start_matches("sha256:"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    };
    let index = blob(md["containerimage.digest"].as_str().unwrap());
    let manifest = blob(index["manifests"][0]["digest"].as_str().unwrap());
    let attestation = blob(index["manifests"][1]["digest"].as_str().unwrap());
    let statement = blob(attestation["layers"][0]["digest"].as_str().unwrap());
    let p = &statement["predicate"];
    // The request whole: its build arguments, and so complete.
    assert_eq!(
        p["buildDefinition"]["externalParameters"]["request"]["args"]["build-arg:A"],
        "1"
    );
    assert_eq!(
        p["runDetails"]["metadata"]["buildkit_completeness"]["request"],
        true
    );
    // The steps: each op once, its inputs earlier steps, the last naming the result.
    let config = &p["buildDefinition"]["internalParameters"]["buildConfig"];
    let steps = config["llbDefinition"].as_array().unwrap();
    for (n, step) in steps.iter().enumerate() {
        assert_eq!(step["id"], format!("step{n}").as_str());
        for input in step["inputs"].as_array().into_iter().flatten() {
            let from: usize = input
                .as_str()
                .and_then(|i| i.strip_prefix("step"))
                .and_then(|i| i.split_once(':'))
                .map(|(n, _)| n.parse().unwrap())
                .unwrap();
            assert!(from < n, "{step}");
        }
    }
    let last = steps.last().unwrap();
    assert_eq!(last["op"], serde_json::json!({"Op": {}}));
    // Each step named by one digest.
    let mapping = config["digestMapping"].as_object().unwrap();
    let mut named: Vec<&str> = mapping.values().map(|v| v.as_str().unwrap()).collect();
    named.sort_unstable();
    named.dedup();
    assert_eq!(named.len(), steps.len(), "{mapping:?}");
    let step_of = |pred: &dyn Fn(&serde_json::Value) -> bool| -> String {
        steps
            .iter()
            .find(|s| pred(&s["op"]["Op"]))
            .map(|s| s["id"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("no such step in {steps:?}"))
    };
    let base = step_of(&|op| {
        op["source"]["identifier"]
            .as_str()
            .is_some_and(|i| i.starts_with("docker-image://"))
    });
    let heredoc = step_of(&|op| op["file"]["actions"][0]["Action"]["mkfile"].is_object());
    let result = last["inputs"][0].as_str().unwrap().to_string();
    // Where each comes from: the base its FROM's line, the heredoc's file none.
    let bk = &p["runDetails"]["metadata"]["buildkit_metadata"];
    assert_eq!(
        bk["source"]["locations"][&base],
        serde_json::json!({"locations": [{"ranges": [{"start": {"line": 1}, "end": {"line": 1}}]}]})
    );
    let info = &bk["source"]["infos"][0];
    assert_eq!(info["filename"], "Dockerfile");
    assert_eq!(
        info["data"],
        base64::engine::general_purpose::STANDARD
            .encode(&dockerfile)
            .as_str()
    );
    // The layers: the base's, the result's the image's own, the heredoc's file none.
    let layers = &bk["layers"];
    let image_layers: Vec<&str> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["digest"].as_str().unwrap())
        .collect();
    let chain = |key: &str| -> Vec<&str> {
        layers[key][0]
            .as_array()
            .unwrap_or_else(|| panic!("no layers of {key} in {layers}"))
            .iter()
            .map(|l| l["digest"].as_str().unwrap())
            .collect()
    };
    assert_eq!(chain(&result), image_layers);
    let base_chain = chain(&format!("{base}:0"));
    assert_eq!(base_chain, image_layers[..base_chain.len()]);
    assert!(base_chain.len() < image_layers.len());
    assert!(layers.get(format!("{heredoc}:0")).is_none(), "{layers}");
    // Asked for by a build argument (`BUILDKIT_ATTEST_PROVENANCE`), as BuildKit reads one.
    let by_arg = shards(&[
        "build",
        "-t",
        "max:2",
        "--build-arg",
        "BUILDKIT_ATTEST_PROVENANCE=mode=max",
        "--metadata-file",
        meta.to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(by_arg.status, Some(0), "{}", by_arg.stderr);
    let md: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    let index = blob(md["containerimage.digest"].as_str().unwrap());
    let attestation = blob(index["manifests"][1]["digest"].as_str().unwrap());
    let statement = blob(attestation["layers"][0]["digest"].as_str().unwrap());
    assert!(
        statement["predicate"]["buildDefinition"]["internalParameters"]["buildConfig"]["llbDefinition"]
            .is_array(),
        "{statement}"
    );
}

#[test]
fn builds_attest_their_provenance_as_docker_does() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-provenance-home");
    let ctx = context(
        "build-provenance-ctx",
        &format!(
            "FROM {image}\nARG A\nRUN --mount=type=secret,id=tok [\"/bin/testguest\", \"exit\", \"0\"]\n"
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let meta = home.join("meta.json");
    let built = shards(&[
        "build",
        "-t",
        "prov:1",
        "--build-arg",
        "A=1",
        "--metadata-file",
        meta.to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let md: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    let index_digest = md["containerimage.digest"].as_str().unwrap().to_string();
    assert_eq!(
        md["containerimage.descriptor"]["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let id = shards(&["image", "inspect", "--format", "{{.Id}}", "prov:1"]);
    assert_eq!(id.stdout.trim(), index_digest, "{}", id.stderr);
    let blob = |d: &str| -> serde_json::Value {
        let path = home
            .join("images/blobs/sha256")
            .join(d.trim_start_matches("sha256:"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    };
    let index = blob(&index_digest);
    let image_manifest = index["manifests"][0]["digest"].as_str().unwrap().to_string();
    let att = &index["manifests"][1];
    assert_eq!(
        att["annotations"]["vnd.docker.reference.digest"],
        image_manifest.as_str()
    );
    assert_eq!(att["platform"]["os"], "unknown");
    let attestation = blob(att["digest"].as_str().unwrap());
    let statement = blob(attestation["layers"][0]["digest"].as_str().unwrap());
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    assert_eq!(statement["predicateType"], "https://slsa.dev/provenance/v1");
    assert_eq!(
        statement["subject"][0]["name"],
        format!("pkg:docker/prov@1?platform=linux%2F{arch}").as_str()
    );
    assert_eq!(
        format!(
            "sha256:{}",
            statement["subject"][0]["digest"]["sha256"].as_str().unwrap()
        ),
        image_manifest
    );
    let p = &statement["predicate"];
    let base = p["buildDefinition"]["resolvedDependencies"][0]["uri"]
        .as_str()
        .unwrap();
    assert!(
        base.starts_with("pkg:docker/127.0.0.1%3A")
            && base.ends_with(&format!("/test/image@v1?platform=linux%2F{arch}")),
        "{base}"
    );
    // mode=min: no build arguments, and the request said incomplete.
    assert!(
        p["buildDefinition"]["externalParameters"]["request"]["args"].is_null(),
        "{p}"
    );
    assert_eq!(
        p["runDetails"]["metadata"]["buildkit_completeness"]["request"],
        false
    );
    assert_eq!(
        p["buildDefinition"]["internalParameters"]["builderPlatform"],
        format!("linux/{arch}").as_str()
    );
    // The metadata file's provenance keeps them, with the secrets mounted.
    let info = &md["buildx.build.provenance"]["invocation"]["parameters"];
    assert_eq!(info["args"]["build-arg:A"], "1", "{info}");
    assert_eq!(info["secrets"][0]["id"], "tok");
    // Asked for by its attributes: the builder's ID, reproducible (D72).
    let asked = shards(&[
        "build",
        "-q",
        "--provenance",
        "builder-id=https://example.com/builder,reproducible=true",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(asked.status, Some(0), "{}", asked.stderr);
    let asked_index = blob(asked.stdout.trim());
    let asked_statement = blob(
        blob(asked_index["manifests"][1]["digest"].as_str().unwrap())["layers"][0]["digest"]
            .as_str()
            .unwrap(),
    );
    let run_details = &asked_statement["predicate"]["runDetails"];
    assert_eq!(run_details["builder"]["id"], "https://example.com/builder");
    assert_eq!(run_details["metadata"]["buildkit_reproducible"], true);
    // Turned off, and what shards does not make yet, refused by name.
    // Unattested, the stored image is Docker's types, as Docker's image exporter stores
    // one (D83).
    let off = shards(&["build", "-q", "--provenance=false", ctx.to_str().unwrap()]);
    assert_eq!(
        blob(off.stdout.trim())["mediaType"],
        "application/vnd.docker.distribution.manifest.v2+json",
        "{}",
        off.stderr
    );
    let refused = shards(&[
        "build",
        "--provenance=true",
        "-o",
        "type=docker,dest=out.tar",
        ctx.to_str().unwrap(),
    ]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("a provenance attestation in a docker output is not supported by shards yet"),
        "{}",
        refused.stderr
    );
    // Asked for in every output: an OCI archive names the index, its statement the name;
    // a local output's provenance.json names its files.
    let layout_tar = home.join("prov.tar");
    let oci = shards(&[
        "build",
        "--provenance=true",
        "-t",
        "prov:oci",
        "-o",
        &format!("type=oci,dest={}", layout_tar.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(oci.status, Some(0), "{}", oci.stderr);
    let entries = tar_entries(&std::fs::read(&layout_tar).unwrap());
    let file = |name: &str| -> serde_json::Value {
        let (_, b) = entries.iter().find(|(h, _)| h.name == name.as_bytes()).unwrap();
        serde_json::from_slice(b).unwrap()
    };
    let top = file("index.json");
    assert_eq!(
        top["manifests"][0]["mediaType"], "application/vnd.oci.image.index.v1+json",
        "{top}"
    );
    let blob_in = |d: &str| file(&format!("blobs/sha256/{}", d.trim_start_matches("sha256:")));
    let oci_index = blob_in(top["manifests"][0]["digest"].as_str().unwrap());
    let oci_statement = blob_in(
        blob_in(oci_index["manifests"][1]["digest"].as_str().unwrap())["layers"][0]["digest"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(
        oci_statement["subject"][0]["name"],
        format!("pkg:docker/prov@oci?platform=linux%2F{arch}").as_str()
    );
    let local = home.join("prov-local");
    let to_local = shards(&[
        "build",
        "--provenance=true",
        "-o",
        &format!("type=local,dest={}", local.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(to_local.status, Some(0), "{}", to_local.stderr);
    let local_statement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(local.join("provenance.json")).unwrap()).unwrap();
    let named: Vec<&str> = local_statement["subject"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(named.contains(&"bin/testguest"), "{named:?}");
    // As BuildKit writes it (measured, Docker 29.3.1): mode 0600, its time the build's
    // SOURCE_DATE_EPOCH (D94).
    let local_epoch = home.join("prov-local-epoch");
    let to_local = shards(&[
        "build",
        "--provenance=true",
        "--build-arg",
        "SOURCE_DATE_EPOCH=1000",
        "-o",
        &format!("type=local,dest={}", local_epoch.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(to_local.status, Some(0), "{}", to_local.stderr);
    let meta = std::fs::metadata(local_epoch.join("provenance.json")).unwrap();
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
        0o600
    );
    assert_eq!(
        meta.modified().unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000)
    );
    // A tar output holds it among its files, in name order, 0600, owner 0, the epoch's
    // time, naming each file by the bytes the tar holds (D94).
    let tar_out = home.join("prov.out.tar");
    let to_tar = shards(&[
        "build",
        "--provenance=true",
        "--build-arg",
        "SOURCE_DATE_EPOCH=1000",
        "-o",
        &format!("type=tar,dest={}", tar_out.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(to_tar.status, Some(0), "{}", to_tar.stderr);
    let entries = tar_entries(&std::fs::read(&tar_out).unwrap());
    let names: Vec<String> = entries
        .iter()
        .map(|(h, _)| String::from_utf8_lossy(&h.name).into_owned())
        .collect();
    let at = names
        .iter()
        .position(|n| n == "provenance.json")
        .unwrap_or_else(|| panic!("{names:?}"));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "in name order");
    let (h, body) = &entries[at];
    assert_eq!((h.mode, h.uid, h.gid, h.mtime.sec), (0o600, 0, 0, 1000));
    let statement: serde_json::Value = serde_json::from_slice(body).unwrap();
    let subjects = statement["subject"].as_array().unwrap();
    let testguest = subjects.iter().find(|s| s["name"] == "bin/testguest").unwrap();
    let (_, bytes) = entries.iter().find(|(h, _)| h.name == b"bin/testguest").unwrap();
    assert_eq!(
        format!("sha256:{}", testguest["digest"]["sha256"].as_str().unwrap()),
        common::sha256_digest(bytes)
    );
    // Asked for none: the manifest is the ID, in Docker's types (D83); in OCI's where
    // the image output asks for them.
    let mut plain = env.to_vec();
    plain.push(("BUILDX_NO_DEFAULT_ATTESTATIONS", std::ffi::OsStr::new("1")));
    let quiet = run_shards_env(&[], &["build", "-q", ctx.to_str().unwrap()], &plain, TIMEOUT);
    assert_eq!(quiet.status, Some(0), "{}", quiet.stderr);
    assert_eq!(
        blob(quiet.stdout.trim())["mediaType"],
        "application/vnd.docker.distribution.manifest.v2+json"
    );
    let asked = run_shards_env(
        &[],
        &[
            "build",
            "-q",
            "-o",
            "type=image,oci-mediatypes=true",
            ctx.to_str().unwrap(),
        ],
        &plain,
        TIMEOUT,
    );
    assert_eq!(asked.status, Some(0), "{}", asked.stderr);
    assert_eq!(
        blob(asked.stdout.trim())["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    // eStargz, OCI's alone, refused in Docker's types, in BuildKit's words.
    let refused = run_shards_env(
        &[],
        &[
            "build",
            "-o",
            "type=oci,dest=never.tar,compression=estargz,oci-mediatypes=false",
            ctx.to_str().unwrap(),
        ],
        &plain,
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("exporter option \"compression=estargz\" conflicts with \"oci-mediatypes=false\""),
        "{}",
        refused.stderr
    );
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
/// that is an archive; a Git repository's commit, its committer's time; none for what is
/// neither, nor for `context`, a local directory; and a source that cannot be fetched
/// fails the build, in BuildKit's words.
#[test]
fn source_date_epoch_is_taken_from_a_source_stage() {
    if cannot_run_vms() {
        return;
    }
    use sha2::Digest as _;
    let url = format!("http://127.0.0.1:{}", epoch_server());
    // A repository whose commit's committer time (git_in's) is not its author's, where
    // this host has git (the Alpine CI container has none).
    let repos = TempDir::new("build-epoch-repos");
    let git_url = if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
    {
        let origin = repos.join("repo.git");
        std::fs::create_dir_all(&origin).unwrap();
        git_in(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("f"), "f\n").unwrap();
        git_in(&origin, &["add", "-A"]);
        git_in(&origin, &["commit", "-q", "-m", "one"]);
        Some(format!(
            "http://127.0.0.1:{}/repo.git",
            git_http_server(repos.to_path_buf(), Vec::new())
        ))
    } else {
        eprintln!("NOTE: no git on this host: a Git source stage is not exercised");
        None
    };
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
    let mut made = vec![
        (1, format!("{url}/lm.tar"), "src", "1994-11-06T08:49:37Z"),
        (
            2,
            format!("--checksum={sum} {url}/lm.tar"),
            "SRC",
            "2027-01-15T08:00:00Z",
        ),
        (3, format!("{url}/nsec.tar"), "src", "2023-11-14T22:13:20Z"),
    ];
    if let Some(g) = &git_url {
        made.push((9, format!("{g}#main"), "src", "2020-09-13T12:26:40Z"));
    }
    for (n, add, epoch, created) in made {
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
    let mut refused = vec![
        (
            6,
            format!("{url}/missing"),
            "failed to solve: invalid response status 404",
        ),
        (
            8,
            format!("{url}/plain"),
            "failed to solve: invalid SOURCE_DATE_EPOCH: nosuch",
        ),
    ];
    if let Some(g) = &git_url {
        refused.push((7, format!("{g}#nope"), "repository does not contain ref nope"));
    }
    for (n, add, said) in refused {
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
                    // A strict server (under /strict/) sends no commit by its name alone
                    // that is no ref's tip, as hosts that keep allowReachableSHA1InWant off.
                    if path.starts_with("/strict/") && !advertise {
                        let tips = std::process::Command::new("git")
                            .arg("-C")
                            .arg(&dir)
                            .args(["for-each-ref", "--format=%(objectname)"])
                            .output()
                            .unwrap()
                            .stdout;
                        let tips = String::from_utf8_lossy(&tips).into_owned();
                        let text = String::from_utf8_lossy(&body).into_owned();
                        let refused = text.split("want ").skip(1).find_map(|w| {
                            let sha = w.get(..40)?;
                            (!tips.contains(sha)).then(|| sha.to_string())
                        });
                        if let Some(sha) = refused {
                            let line = format!("ERR upload-pack: not our ref {sha}\n");
                            let out = format!("{:04x}{line}", line.len() + 4);
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n",
                                out.len()
                            );
                            let _ = conn.write_all(head.as_bytes());
                            let _ = conn.write_all(out.as_bytes());
                            continue;
                        }
                    }
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
/// A layerless image (`FROM scratch` and `LABEL`), its manifest's `layers` and its
/// config's `diff_ids` null as BuildKit writes them, inspects and filters as Docker shows
/// it: its labels, platform and config, behind the index its provenance makes.
#[test]
fn a_layerless_image_inspects_as_docker_shows_it() {
    let home = TempDir::new("layerless-home");
    let ctx = context("layerless-ctx", "FROM scratch\nLABEL tier=web\n");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let built = shards(&["build", "-q", "-t", "layerless:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{built}");
    let shown = shards(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}} {{.Os}} {{.RootFS.Type}}",
        "layerless:1",
    ]);
    assert_eq!(shown.stdout, "{\"tier\":\"web\"} linux layers\n", "{shown}");
    let listed = shards(&[
        "images",
        "--filter",
        "label=tier=web",
        "--format",
        "{{.Repository}}:{{.Tag}}",
    ]);
    assert_eq!(listed.stdout, "layerless:1\n", "{listed}");
}

/// dockerui's build-arg options, as frontend 1.27.1 reads them (D92):
/// BUILDKIT_DISABLE_FILEOP refused once true, in Docker's words; BUILDKIT_GIT_ADVICE read as
/// a bool, refused otherwise; neither in the way of a build that sets them off or on.
#[test]
fn dockeruis_build_args_are_read_as_it_reads_them() {
    let home = TempDir::new("dockerui-args-home");
    let ctx = context("dockerui-args-ctx", "FROM scratch\n");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let build = |arg: &str| {
        run_shards_env(
            &[],
            &[
                "build",
                "--progress=plain",
                "--build-arg",
                arg,
                ctx.to_str().unwrap(),
            ],
            &env,
            TIMEOUT,
        )
    };
    let refused = build("BUILDKIT_DISABLE_FILEOP=1");
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains(
            "ERROR: failed to build: failed to solve: support for \"BUILDKIT_DISABLE_FILEOP\" build-arg was removed in BuildKit 0.11"
        ),
        "{}",
        refused.stderr
    );
    assert_eq!(build("BUILDKIT_DISABLE_FILEOP=false").status, Some(0));
    let refused = build("BUILDKIT_GIT_ADVICE=x");
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains(
            "failed to parse build-arg:BUILDKIT_GIT_ADVICE: strconv.ParseBool: parsing \"x\": invalid syntax"
        ),
        "{}",
        refused.stderr
    );
    assert_eq!(build("BUILDKIT_GIT_ADVICE=true").status, Some(0));
}

/// `RUN --device`, CDI devices as BuildKit gives them to a step (D96), in a microVM: a spec
/// read from its directory (YAML), the device found by its name and granted by `--allow
/// device`; its environment and groups the step's, a host file mounted read-only where the
/// spec puts it, a device node made from the guest's own device of its host path. Refused
/// as BuildKit refuses them: a device not granted, a required device not registered; an
/// optional one not registered left out; and what only a host can do, a hook, by name.
#[test]
fn run_steps_take_cdi_devices() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let specs = TempDir::new("cdi-specs");
    let lib = TempDir::new("cdi-lib");
    std::fs::write(lib.join("data.txt"), "from the host\n").unwrap();
    std::fs::write(
        specs.join("vendor.yaml"),
        format!(
            "cdiVersion: \"0.7.0\"\nkind: vendor.com/dev\ncontainerEdits:\n  env: [VENDOR=1]\ndevices:\n  - name: zero\n    containerEdits:\n      env: [ZERO=yes]\n      additionalGids: [4242]\n      deviceNodes:\n        - path: /dev/vendor0\n          hostPath: /dev/zero\n          fileMode: 0660\n      mounts:\n        - hostPath: {}\n          containerPath: /opt/vendor/data.txt\n          options: [ro, bind]\n  - name: hooked\n    containerEdits:\n      hooks:\n        - hookName: createContainer\n          path: /usr/bin/vendor-hook\n",
            lib.join("data.txt").display()
        ),
    )
    .unwrap();
    let home = TempDir::new("cdi-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_CDI_SPEC_DIRS", specs.as_os_str()),
    ];
    let build = |dockerfile: &str, extra: &[&str]| {
        let ctx = context("cdi-ctx", dockerfile);
        let mut args = vec!["build", "--progress=plain", "--no-cache"];
        args.extend_from_slice(extra);
        args.push(ctx.to_str().unwrap());
        let r = run_shards_env(&[], &args, &env, TIMEOUT);
        (r.status, r.stderr)
    };
    let run = format!(
        "FROM {image}\nUSER root\nRUN --device=vendor.com/dev=zero --device=vendor.com/absent,required=false [\"/bin/testguest\", \"fs\", \"print:/proc/self/environ\", \"print:/proc/self/status\", \"print:/opt/vendor/data.txt\", \"dev:/dev/vendor0\"]\n"
    );
    let (code, log) = build(&run, &["--allow", "device"]);
    assert_eq!(code, Some(0), "{log}");
    assert!(log.contains("VENDOR=1") && log.contains("ZERO=yes"), "{log}");
    assert!(
        log.lines().any(|l| l.contains("Groups:") && l.contains("4242")),
        "{log}"
    );
    assert!(log.contains("from the host"), "{log}");
    // /dev/zero's numbers, 1:5, the mode the spec gives.
    assert!(log.contains("/dev/vendor0 c 1:5 660"), "{log}");
    // Granted by name too.
    let (code, log) = build(&run, &["--allow", "device=vendor.com/dev=zero"]);
    assert_eq!(code, Some(0), "{log}");
    // Not granted.
    let (code, log) = build(&run, &[]);
    assert_ne!(code, Some(0));
    assert!(
        log.contains("device vendor.com/dev=zero is requested by the build but not allowed"),
        "{log}"
    );
    // A required device not registered (`--device` is optional unless `required=true`).
    let (code, log) = build(
        &format!(
            "FROM {image}\nRUN --device=vendor.com/absent,required=true [\"/bin/testguest\", \"exit\", \"0\"]\n"
        ),
        &["--allow", "device"],
    );
    assert_ne!(code, Some(0));
    assert!(
        log.contains("required device \"vendor.com/absent\" is not registered"),
        "{log}"
    );
    // A hook, which only a host runs.
    let (code, log) = build(
        &format!("FROM {image}\nRUN --device=vendor.com/dev=hooked [\"/bin/testguest\", \"exit\", \"0\"]\n"),
        &["--allow", "device"],
    );
    assert_ne!(code, Some(0));
    assert!(
        log.contains("a createContainer hook (/usr/bin/vendor-hook)"),
        "{log}"
    );
}

/// A ustar archive of `files` (path, bytes), regular files mode 0644, as a client's
/// context archive would hold them.
fn ustar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in files {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// Remote build contexts, as `docker build` takes them through buildx and BuildKit's
/// dockerui (D91): a Git URL's tree, its `#REF:SUBDIR` the context, without `.git` unless
/// BUILDKIT_CONTEXT_KEEP_GIT_DIR keeps it, `-f` a file of it; an HTTP(S) URL's archive,
/// unpacked, or its plain Dockerfile, alone in the context as `context`; stdin's archive,
/// or stdin's Dockerfile with an empty context; stdin refused for both. The Dockerfile is
/// read from the context, its source fetched once.
#[test]
fn builds_take_remote_contexts() {
    use std::io::Write as _;
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
    let repos = TempDir::new("remote-ctx-repos");
    let origin = repos.join("repo.git");
    std::fs::create_dir_all(origin.join("app")).unwrap();
    git_in(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("Dockerfile"), "FROM scratch\nCOPY . /\n").unwrap();
    std::fs::write(origin.join("a.txt"), "root\n").unwrap();
    std::fs::write(origin.join("app/Dockerfile"), "FROM scratch\nCOPY . /\n").unwrap();
    std::fs::write(origin.join("app/b.txt"), "app\n").unwrap();
    git_in(&origin, &["add", "-A"]);
    git_in(&origin, &["commit", "-q", "-m", "one"]);
    let hport = git_http_server(repos.to_path_buf(), Vec::new());
    let repo = format!("http://127.0.0.1:{hport}/repo.git");

    // An HTTP server of a context archive, gzipped, and of a plain Dockerfile.
    let archive = {
        let tar = ustar(&[
            ("Dockerfile", b"FROM scratch\nCOPY . /\n"),
            ("c.txt", b"from the archive\n"),
        ]);
        let mut gz = shards_flate::GzipWriter::new(Vec::new(), 6).unwrap();
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    };
    let plain = b"FROM scratch\nCOPY context /d\n".to_vec();
    let served: Vec<(String, Vec<u8>)> = vec![
        ("/ctx.tar.gz".into(), archive),
        ("/Dockerfile".into(), plain.clone()),
        ("/f.Dockerfile".into(), b"FROM scratch\nCOPY f.txt /f\n".to_vec()),
        ("/big".into(), vec![b'#'; 2 * 1024 * 1024 + 1]),
    ];
    let fetched = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let wport = listener.local_addr().unwrap().port();
    {
        let fetched = fetched.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead as _, BufReader};
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let mut out = stream;
                match served.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        fetched.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = write!(
                            out,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = out.write_all(body);
                    }
                    None => {
                        let _ = write!(
                            out,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            }
        });
    }

    let home = TempDir::new("remote-ctx-home");
    let outs = TempDir::new("remote-ctx-out");
    let env: Vec<(&str, std::ffi::OsString)> = vec![
        ("SHARDS_HOME", home.as_os_str().to_owned()),
        ("SHARDS_KERNEL", kernel().as_os_str().to_owned()),
        ("SHARDS_INIT", guest_init().as_os_str().to_owned()),
    ];
    // `shards build ARGS`, `input` its stdin.
    let build = |args: &[&str], input: Option<&[u8]>| -> (Option<i32>, String) {
        let mut cmd = std::process::Command::new(common::shards());
        cmd.arg("build").args(args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.unwrap_or_default().to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let exported = |name: &str, args: &[&str], input: Option<&[u8]>| -> (std::path::PathBuf, String) {
        let dest = outs.join(name);
        let o = format!("type=local,dest={}", dest.display());
        let mut a = vec!["--progress=plain", "-o", &o];
        a.extend_from_slice(args);
        let (code, log) = build(&a, input);
        assert_eq!(code, Some(0), "{name}: {log}");
        (dest, log)
    };
    let files = |dir: &std::path::Path| -> Vec<String> {
        let mut all = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    todo.push(p.clone());
                }
                all.push(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
            }
        }
        all.sort();
        all
    };

    // Git: the repository's tree, its .git left out.
    let (o, log) = exported("git", &[&repo], None);
    assert!(
        log.contains(&format!("[internal] load git source {repo}")),
        "{log}"
    );
    assert!(!log.contains("load build definition"), "{log}");
    assert_eq!(
        files(&o),
        ["Dockerfile", "a.txt", "app", "app/Dockerfile", "app/b.txt"]
    );
    // A ref and a directory of it.
    let (o, _) = exported("git-sub", &[&format!("{repo}#main:app")], None);
    assert_eq!(files(&o), ["Dockerfile", "b.txt"]);
    // Kept with its .git.
    let (o, _) = exported(
        "git-keep",
        &["--build-arg", "BUILDKIT_CONTEXT_KEEP_GIT_DIR=1", &repo],
        None,
    );
    assert!(o.join(".git/HEAD").is_file(), "{:?}", files(&o));
    // `-f` a file of the repository.
    std::fs::write(
        origin.join("other.Dockerfile"),
        "FROM scratch\nCOPY app/b.txt /only\n",
    )
    .unwrap();
    git_in(&origin, &["add", "-A"]);
    git_in(&origin, &["commit", "-q", "-m", "two"]);
    let (o, _) = exported("git-f", &["-f", "other.Dockerfile", &repo], None);
    assert_eq!(files(&o), ["only"]);
    // `-f -`: stdin's Dockerfile, the repository's context.
    let (o, _) = exported(
        "git-stdin-f",
        &["-f", "-", &repo],
        Some(b"FROM scratch\nCOPY a.txt /x\n"),
    );
    assert_eq!(files(&o), ["x"]);

    // HTTP: an archive, unpacked, fetched once though the build reads it twice.
    let before = fetched.load(std::sync::atomic::Ordering::SeqCst);
    let (o, log) = exported(
        "http-tgz",
        &[&format!("http://127.0.0.1:{wport}/ctx.tar.gz")],
        None,
    );
    assert!(log.contains("[internal] load remote build context"), "{log}");
    assert_eq!(files(&o), ["Dockerfile", "c.txt"]);
    assert_eq!(fetched.load(std::sync::atomic::Ordering::SeqCst) - before, 1);
    // A plain Dockerfile: it is the context, as `context`.
    let (o, _) = exported(
        "http-plain",
        &[&format!("http://127.0.0.1:{wport}/Dockerfile")],
        None,
    );
    assert_eq!(std::fs::read(o.join("d")).unwrap(), plain);

    // `-f` a URL: fetched as buildx fetches it, at most 2 MiB, over a local directory.
    let local = context("remote-ctx-local", "FROM scratch\n");
    std::fs::write(local.join("f.txt"), "local\n").unwrap();
    let (o, log) = exported(
        "f-url",
        &[
            "-f",
            &format!("http://127.0.0.1:{wport}/f.Dockerfile"),
            local.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        log.contains(&format!("[internal] load http://127.0.0.1:{wport}/f.Dockerfile")),
        "{log}"
    );
    assert_eq!(files(&o), ["f"]);
    let (code, log) = build(
        &[
            "-f",
            &format!("http://127.0.0.1:{wport}/big"),
            local.to_str().unwrap(),
        ],
        None,
    );
    assert_ne!(code, Some(0));
    assert!(
        log.contains(&format!(
            "Dockerfile http://127.0.0.1:{wport}/big bigger than allowed max size (2.097MB)"
        )),
        "{log}"
    );

    // stdin: an archive is the context; a Dockerfile has an empty one.
    let tar = ustar(&[
        ("Dockerfile", b"FROM scratch\nCOPY . /\n"),
        ("s.txt", b"from stdin\n"),
    ]);
    let (o, _) = exported("stdin-tar", &["-"], Some(&tar));
    assert_eq!(files(&o), ["Dockerfile", "s.txt"]);
    let (o, _) = exported("stdin-file", &["-"], Some(b"FROM scratch\nCOPY . /\n"));
    assert_eq!(files(&o), Vec::<String>::new());
    // Refused as buildx refuses them.
    let (code, log) = build(&["-f", "-", "-"], Some(b""));
    assert_ne!(code, Some(0));
    assert!(
        log.contains("can't use stdin for both build context and dockerfile"),
        "{log}"
    );
    let (code, log) = build(&["-f", "x", "-"], Some(b"FROM scratch\n"));
    assert_ne!(code, Some(0));
    assert!(
        log.contains("ambiguous Dockerfile source: both stdin and flag correspond to Dockerfiles"),
        "{log}"
    );
}

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

/// `ADD` of a repository over SSH (D69): shards' own client against OpenSSH's sshd,
/// three servers each taking one path (`mlkem768x25519-sha256` with
/// `chacha20-poly1305@openssh.com` and an Ed25519 host key; `curve25519-sha256` with
/// `aes256-gcm@openssh.com`, an ECDSA host key, and a rekeying every 256 KiB;
/// `curve25519-sha256@libssh.org` with `aes128-gcm@openssh.com` and an RSA host key), each
/// repository's submodule over SSH too, authenticated by a key file the build serves and
/// by an agent's socket; refused: a host known_hosts does not know, a key the server does
/// not take, a repository it lacks, and a build given no agent.
#[test]
fn add_fetches_git_over_ssh() {
    if cannot_run_vms() {
        return;
    }
    let sshd = std::path::Path::new("/usr/sbin/sshd");
    let tools = ["git", "ssh", "ssh-keygen", "ssh-agent", "ssh-add"];
    if !sshd.exists()
        || tools
            .iter()
            .any(|t| std::process::Command::new(t).arg("--help").output().is_err())
    {
        eprintln!("SKIP: no sshd, git or OpenSSH's tools on this host");
        return;
    }
    let dir = TempDir::new("build-add-ssh");
    let keygen = |args: &[&str], at: &std::path::Path| {
        let made = std::process::Command::new("ssh-keygen")
            .args(["-q", "-N", ""])
            .args(args)
            .arg("-f")
            .arg(at)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    };
    let (client, stranger) = (dir.join("client"), dir.join("stranger"));
    keygen(&["-t", "ed25519"], &client);
    keygen(&["-t", "ecdsa"], &stranger);
    std::fs::copy(dir.join("client.pub"), dir.join("authorized_keys")).unwrap();

    // Each repository: a file too large for one rekeying's window, and a submodule.
    let repos = dir.join("repos");
    let (origin, sub) = (repos.join("repo.git"), repos.join("sub.git"));
    std::fs::create_dir_all(&sub).unwrap();
    git_in(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("subfile.txt"), "in the submodule\n").unwrap();
    git_in(&sub, &["add", "-A"]);
    git_in(&sub, &["commit", "-q", "-m", "sub"]);
    std::fs::create_dir_all(&origin).unwrap();
    git_in(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("a.txt"), "over ssh\n").unwrap();
    let mut noise = vec![0u8; 3 << 20];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for b in noise.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    std::fs::write(origin.join("noise.bin"), &noise).unwrap();
    git_in(&origin, &["add", "-A"]);
    git_in(
        &origin,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            "../sub.git",
            "mods/sub",
        ],
    );
    git_in(&origin, &["commit", "-q", "-m", "one"]);

    struct Ended(std::process::Child);
    impl Drop for Ended {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let free_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    // The hybrid exchange where this host's OpenSSH has it (9.9 and later; Ubuntu 24.04's
    // 9.6 does not), curve25519 where not.
    let kexes = std::process::Command::new("ssh")
        .args(["-Q", "kex"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let hybrid = if kexes.lines().any(|k| k == "mlkem768x25519-sha256") {
        "mlkem768x25519-sha256"
    } else {
        eprintln!(
            "NOTE: this host's OpenSSH has no mlkem768x25519-sha256: the hybrid exchange is not exercised"
        );
        "curve25519-sha256"
    };
    let servers = [
        ("ed25519", hybrid, "chacha20-poly1305@openssh.com", ""),
        (
            "ecdsa",
            "curve25519-sha256",
            "aes256-gcm@openssh.com",
            "RekeyLimit 256K\n",
        ),
        (
            "rsa",
            "curve25519-sha256@libssh.org",
            "aes128-gcm@openssh.com",
            "",
        ),
    ];
    let mut known = String::new();
    let mut daemons = Vec::new();
    let mut ports = Vec::new();
    for (kind, kex, cipher, extra) in servers {
        let host_key = dir.join(format!("host_{kind}"));
        keygen(&["-t", kind], &host_key);
        let port = free_port();
        let config = dir.join(format!("sshd_{kind}"));
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\n\
                 PidFile {}\nUsePAM no\nStrictModes no\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nAcceptEnv GIT_PROTOCOL\nKexAlgorithms {kex}\n\
                 Ciphers {cipher}\n{extra}",
                host_key.display(),
                dir.join("authorized_keys").display(),
                dir.join(format!("sshd_{kind}.pid")).display(),
            ),
        )
        .unwrap();
        daemons.push(Ended(
            std::process::Command::new(sshd)
                .arg("-f")
                .arg(&config)
                .args(["-D", "-e"])
                .stderr(std::fs::File::create(dir.join(format!("sshd_{kind}.log"))).unwrap())
                .spawn()
                .unwrap(),
        ));
        let public = std::fs::read_to_string(dir.join(format!("host_{kind}.pub"))).unwrap();
        let mut fields = public.split_whitespace();
        known.push_str(&format!(
            "[127.0.0.1]:{port} {} {}\n",
            fields.next().unwrap(),
            fields.next().unwrap()
        ));
        ports.push(port);
    }
    for port in &ports {
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", *port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // The client's known_hosts, in a home of the test's own.
    let user_home = dir.join("home");
    std::fs::create_dir_all(user_home.join(".ssh")).unwrap();
    std::fs::write(user_home.join(".ssh/known_hosts"), &known).unwrap();
    let stranger_home = dir.join("stranger-home");
    std::fs::create_dir_all(&stranger_home).unwrap();

    let (image, _) = served();
    let home = TempDir::new("build-add-ssh-home");
    let user = String::from_utf8(
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("ssh://{user}@127.0.0.1:{p}{}", origin.display()))
        .collect();
    let mut dockerfile = format!("FROM {image}\n");
    for (i, u) in urls.iter().enumerate() {
        dockerfile.push_str(&format!("ADD {u}#main /s{i}\n"));
    }
    let ctx = context("build-add-ssh-ctx", &dockerfile);
    let build = |user_home: &std::path::Path, ssh: &[&str]| {
        let env = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
            ("HOME", user_home.as_os_str()),
        ];
        let mut args = vec!["build", "--no-cache", "-t", "sshed:1"];
        args.extend_from_slice(ssh);
        args.push(ctx.to_str().unwrap());
        run_shards_env(&[], &args, &env, TIMEOUT)
    };
    let with_key = format!("default={}", client.display());
    let built = build(&user_home, &["--ssh", &with_key]);
    let logs = || {
        servers
            .iter()
            .map(|(k, ..)| std::fs::read_to_string(dir.join(format!("sshd_{k}.log"))).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(built.status, Some(0), "{}\n{}", built.stderr, logs());
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let mut paths = Vec::new();
    for i in 0..urls.len() {
        paths.push(format!("/s{i}/a.txt"));
        paths.push(format!("/s{i}/mods/sub/subfile.txt"));
    }
    let mut args = vec!["run", "--rm", "-u", "root", "sshed:1", "stat"];
    args.extend(paths.iter().map(String::as_str));
    let stat = run_shards_env(&[], &args, &env, TIMEOUT);
    let mut want = String::new();
    for i in 0..urls.len() {
        want.push_str(&format!(
            "/s{i}/a.txt file 644 0:0 9\n= over ssh\\n\n/s{i}/mods/sub/subfile.txt file 644 0:0 17\n= in the submodule\\n\n"
        ));
    }
    assert_eq!(stat.stdout, want, "{}", stat.stderr);
    let noise_size = run_shards_env(
        &[],
        &["run", "--rm", "sshed:1", "stat", "/s1/noise.bin"],
        &env,
        TIMEOUT,
    );
    assert!(
        noise_size
            .stdout
            .starts_with(&format!("/s1/noise.bin file 644 0:0 {}\n", 3 << 20)),
        "{}",
        noise_size.stdout
    );

    // A host known_hosts does not know.
    let unknown = build(&stranger_home, &["--ssh", &with_key]);
    assert_ne!(unknown.status, Some(0));
    assert!(
        unknown
            .stderr
            .contains("Host key verification failed: [127.0.0.1]:")
            && unknown.stderr.contains("is not in known_hosts"),
        "{}",
        unknown.stderr
    );
    // A key the server does not take.
    let wrong = build(&user_home, &["--ssh", &format!("default={}", stranger.display())]);
    assert_ne!(wrong.status, Some(0));
    assert!(
        wrong
            .stderr
            .contains(&format!("{user}@127.0.0.1: Permission denied (publickey).")),
        "{}",
        wrong.stderr
    );
    // A repository the server lacks: upload-pack's own words.
    let missing_ctx = context(
        "build-add-ssh-missing",
        &format!(
            "FROM {image}\nADD ssh://{user}@127.0.0.1:{}{}/nope.git /x\n",
            ports[0],
            repos.display()
        ),
    );
    let env_home = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("HOME", user_home.as_os_str()),
    ];
    let missing = run_shards_env(
        &[],
        &["build", "--ssh", &with_key, missing_ctx.to_str().unwrap()],
        &env_home,
        TIMEOUT,
    );
    assert_ne!(missing.status, Some(0));
    assert!(
        missing.stderr.contains("does not appear to be a git repository"),
        "{}",
        missing.stderr
    );
    // The same through an agent's socket the build forwards.
    let sock = dir.join("agent.sock");
    let mut agent = Ended(
        std::process::Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&sock)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let added = std::process::Command::new("ssh-add")
        .arg(&client)
        .env("SSH_AUTH_SOCK", &sock)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let through_socket = build(&user_home, &["--ssh", &format!("default={}", sock.display())]);
    assert_eq!(through_socket.status, Some(0), "{}", through_socket.stderr);
    let _ = agent.0.kill();
    // No agent at all.
    let none = build(&user_home, &[]);
    assert_ne!(none.status, Some(0));
    assert!(
        none.stderr
            .contains("no SSH key \"default\" forwarded from the client"),
        "{}",
        none.stderr
    );
}

/// A commit that is no ref's tip, asked for by its name or pinned by a submodule, from a
/// server that will not send it alone ("not our ref"): its refs' history fetched whole, as
/// BuildKit fetches it, and the kept repository not shallow; and a commit that is in no
/// ref's history at all, refused in the server's words.
#[test]
fn add_fetches_a_commit_a_server_will_not_send_alone() {
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
    let repos = TempDir::new("build-add-git-strict-repos");
    let (origin, sub) = (repos.join("strict/repo.git"), repos.join("strict/sub.git"));
    std::fs::create_dir_all(&sub).unwrap();
    git_in(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("v.txt"), "pinned\n").unwrap();
    git_in(&sub, &["add", "-A"]);
    git_in(&sub, &["commit", "-q", "-m", "pinned"]);
    std::fs::create_dir_all(&origin).unwrap();
    git_in(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("a.txt"), "first\n").unwrap();
    git_in(&origin, &["submodule", "add", "-q", "../sub.git", "mods/sub"]);
    git_in(&origin, &["add", "-A"]);
    git_in(&origin, &["commit", "-q", "-m", "first"]);
    let first = git_in(&origin, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(origin.join("a.txt"), "second\n").unwrap();
    git_in(&origin, &["commit", "-q", "-am", "second"]);
    // The submodule moves on: its pinned commit is no longer its tip.
    std::fs::write(sub.join("v.txt"), "moved on\n").unwrap();
    git_in(&sub, &["commit", "-q", "-am", "moved on"]);
    let port = git_http_server(repos.to_path_buf(), Vec::new());
    let url = format!("http://127.0.0.1:{port}/strict/repo.git");

    let (image, _) = served();
    let home = TempDir::new("build-add-git-strict-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-add-git-strict-ctx",
        &format!("FROM {image}\nADD --keep-git-dir=true {url}#{first} /r\n"),
    );
    let built = shards(&["build", "-t", "strict:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let stat = shards(&[
        "run",
        "--rm",
        "-u",
        "root",
        "strict:1",
        "stat",
        "/r/a.txt",
        "/r/mods/sub/v.txt",
        "/r/.git/HEAD",
        "/r/.git/shallow",
    ]);
    assert_eq!(
        stat.stdout,
        format!(
            "/r/a.txt file 644 0:0 6\n= first\\n\n/r/mods/sub/v.txt file 644 0:0 7\n= pinned\\n\n\
             /r/.git/HEAD file 644 0:0 41\n= {first}\\n\n/r/.git/shallow missing No such file or directory (os error 2)\n"
        ),
        "{}",
        stat.stderr
    );
    // A commit in no ref's history: the server's refusal.
    let nowhere = "0123456789abcdef0123456789abcdef01234567";
    let bad = context(
        "build-add-git-strict-bad",
        &format!("FROM {image}\nADD {url}#{nowhere} /x\n"),
    );
    let refused = shards(&["build", bad.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains(&format!("remote error: upload-pack: not our ref {nowhere}")),
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

/// External build caches (D62): a build's records, written by `--cache-to` to a
/// directory (`mode=max`), a registry (`min`) and the image itself (`inline`), answer
/// another home's build from `--cache-from`, as BuildKit's remote caches do: from the
/// directory every step, from the registry and the image those of the image's own
/// layers alone, the build stage's step running again; the image's layers the same.
/// `--no-cache-filter` runs its stages' steps again and no others.
#[test]
fn builds_take_steps_from_caches_written_elsewhere() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let homes: Vec<TempDir> = (0..4)
        .map(|n| TempDir::new(&format!("remote-cache-home-{n}")))
        .collect();
    let shards_in = |home: &TempDir, args: &[&str]| {
        let env = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ];
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let ctx = context(
        "remote-cache-ctx",
        &format!(
            "FROM {image} AS build\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/out=built\"]\n\
             FROM {image}\nUSER root\nCOPY --from=build /out /out\nRUN [\"/bin/testguest\", \"fs\", \"write:/two=2\"]\n"
        ),
    );
    let build = |home: &TempDir, extra: &[&str], tag: &str| {
        let mut args = vec!["build", "--progress=plain"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-t", tag, ctx.to_str().unwrap()]);
        let built = shards_in(home, &args);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
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
    let steps = ["write:/out=built", "COPY --from=build", "write:/two=2"];
    let layers = |home: &TempDir, tag: &str| {
        shards_in(
            home,
            &["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag],
        )
        .stdout
    };
    let dir = TempDir::new("remote-cache-dir");
    let dest = format!("type=local,dest={},mode=max", dir.display());
    let src = format!("type=local,src={}", dir.display());
    let cache_ref = format!("127.0.0.1:{port}/team/cache:1");
    let image_ref = format!("127.0.0.1:{port}/team/app:1");
    let first = build(
        &homes[0],
        &[
            "--cache-to",
            &dest,
            "--cache-to",
            &format!("type=registry,ref={cache_ref}"),
        ],
        "made:1",
    );
    for step in steps {
        assert!(!cached(&first, step), "{step} ran\n{first}");
    }
    assert!(first.contains("exporting cache to client directory"), "{first}");
    assert!(first.contains("exporting cache to registry"), "{first}");
    assert!(dir.join("index.json").exists() && dir.join("oci-layout").exists());
    let made = layers(&homes[0], "made:1");

    // From the directory, everything (`max`).
    let from_dir = build(&homes[1], &["--cache-from", &src], "taken:1");
    for step in steps {
        assert!(cached(&from_dir, step), "{step} was taken\n{from_dir}");
    }
    assert_eq!(layers(&homes[1], "taken:1"), made);

    // From the registry, the image's own steps (`min`): the build stage's runs.
    let from_registry = build(&homes[2], &["--cache-from", &cache_ref], "taken:2");
    assert!(from_registry.contains(&format!("importing cache manifest from {cache_ref}")));
    assert!(!cached(&from_registry, "write:/out=built"), "{from_registry}");
    assert!(cached(&from_registry, "COPY --from=build"), "{from_registry}");
    assert!(cached(&from_registry, "write:/two=2"), "{from_registry}");
    assert_eq!(layers(&homes[2], "taken:2"), made);

    // Inline: the image, pushed, carries its own steps' records.
    let pushed = build(&homes[0], &["--cache-to", "type=inline", "--push"], &image_ref);
    assert!(!pushed.contains("ERROR"), "{pushed}");
    let from_image = build(&homes[3], &["--cache-from", &image_ref], "taken:3");
    assert!(cached(&from_image, "write:/two=2"), "{from_image}");
    assert_eq!(layers(&homes[3], "taken:3"), made);

    // --metadata-file: the image's config and manifest digests, and its name, as buildx
    // writes them.
    let meta = dir.join("meta.json");
    build(&homes[0], &["--metadata-file", meta.to_str().unwrap()], "made:3");
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    let id = shards_in(&homes[0], &["image", "inspect", "--format", "{{.Id}}", "made:3"]).stdout;
    // The image's ID is its manifest's digest, as with Docker's containerd store.
    assert_eq!(
        written["containerimage.digest"].as_str(),
        Some(id.trim()),
        "{written}"
    );
    assert!(written.get("containerimage.config.digest").is_none(), "{written}");
    assert_eq!(
        written["image.name"].as_str(),
        Some("docker.io/library/made:3"),
        "{written}"
    );
    assert_eq!(
        written["containerimage.digest"], written["containerimage.descriptor"]["digest"],
        "{written}"
    );
    assert!(
        written["buildx.build.ref"]
            .as_str()
            .is_some_and(|r| r.starts_with("shards/shards/")),
        "{written}"
    );

    // --no-cache-filter: its stage's step runs, the rest are the cache's.
    let filtered = build(&homes[0], &["--no-cache-filter", "build"], "made:2");
    assert!(!cached(&filtered, "write:/out=built"), "{filtered}");
    assert!(cached(&filtered, "COPY --from=build"), "{filtered}");
    assert!(cached(&filtered, "write:/two=2"), "{filtered}");

    // A cache that is not there is skipped, as BuildKit's are; one asked to be written
    // nowhere is refused.
    let missing = build(
        &homes[1],
        &["--cache-from", "type=local,src=/nonexistent"],
        "taken:4",
    );
    assert!(
        missing.contains("WARNING: local cache import at /nonexistent skipped"),
        "{missing}"
    );
    let refused = shards_in(
        &homes[1],
        &["build", "--cache-to", "type=local", ctx.to_str().unwrap()],
    );
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains("local cache exporter requires dest"),
        "{}",
        refused.stderr
    );
}

/// The S3 cache backend (D88): `--cache-to type=s3` writes a build's layers and records
/// to a bucket as BuildKit's does, under `prefix`, at each of `name`'s names, every
/// request signed and each layer's body signed whole; another home's `--cache-from`
/// takes every step from it (`mode=max`), signing with `$AWS_ACCESS_KEY_ID` or a shared
/// credentials file's profile. Layers older than `touch_refresh` are copied onto
/// themselves, not sent again; a cache with no credentials is refused before the build.
#[test]
fn builds_take_steps_from_an_s3_cache() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, buckets) = common::fake_s3();
    let homes: Vec<TempDir> = (0..3)
        .map(|n| TempDir::new(&format!("s3-cache-home-{n}")))
        .collect();
    let files = TempDir::new("s3-cache-files");
    let shared = files.join("credentials");
    std::fs::write(&shared, "[other]\naws_access_key_id = WRONG\n\n[ci]\naws_access_key_id = FROMFILE\naws_secret_access_key = s3cr3t\n").unwrap();
    let nowhere = files.join("none");
    let shards_in = |home: &TempDir, args: &[&str], extra: &[(&str, &std::ffi::OsStr)]| {
        let mut env = vec![
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
            // None of the host's own AWS settings.
            ("AWS_ACCESS_KEY_ID", std::ffi::OsStr::new("")),
            ("AWS_SECRET_ACCESS_KEY", std::ffi::OsStr::new("")),
            ("AWS_PROFILE", std::ffi::OsStr::new("")),
            ("AWS_ENDPOINT_URL", std::ffi::OsStr::new("")),
            ("AWS_ENDPOINT_URL_S3", std::ffi::OsStr::new("")),
            ("AWS_SHARED_CREDENTIALS_FILE", nowhere.as_os_str()),
            ("AWS_CONFIG_FILE", nowhere.as_os_str()),
        ];
        env.extend_from_slice(extra);
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let ctx = context(
        "s3-cache-ctx",
        &format!(
            "FROM {image} AS build\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/out=built\"]\n\
             FROM {image}\nUSER root\nCOPY --from=build /out /out\nRUN [\"/bin/testguest\", \"fs\", \"write:/two=2\"]\n"
        ),
    );
    let at = format!(
        "type=s3,bucket=cache,region=us-east-1,endpoint_url=http://127.0.0.1:{port},use_path_style=true,prefix=team/"
    );
    let build = |home: &TempDir, cache: &[&str], extra: &[(&str, &std::ffi::OsStr)], tag: &str| {
        let mut args = vec!["build", "--progress=plain"];
        args.extend_from_slice(cache);
        args.extend_from_slice(&["-t", tag, ctx.to_str().unwrap()]);
        let built = shards_in(home, &args, extra);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
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
    let steps = ["write:/out=built", "COPY --from=build", "write:/two=2"];
    let layers = |home: &TempDir, tag: &str| {
        shards_in(
            home,
            &["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag],
            &[],
        )
        .stdout
    };

    let to = format!("{at},mode=max,name=one;two,access_key_id=GIVEN,secret_access_key=s3cr3t");
    let first = build(&homes[0], &["--cache-to", &to], &[], "made:1");
    assert!(first.contains("exporting cache to Amazon S3"), "{first}");
    let blobs: Vec<String> = {
        let held = buckets.lock().unwrap();
        for name in ["one", "two"] {
            assert!(
                held.objects
                    .contains_key(&format!("/cache/team/manifests/{name}")),
                "{held:?}"
            );
        }
        assert!(held.log.iter().all(|l| l.ends_with(" GIVEN")), "{:?}", held.log);
        held.objects
            .keys()
            .filter(|k| k.starts_with("/cache/team/blobs/sha256:"))
            .cloned()
            .collect()
    };
    // Each layer of the build: both stages' steps (`max`).
    assert!(blobs.len() >= 3, "{blobs:?}");
    for b in &blobs {
        assert!(
            first.contains(&format!(
                "writing layer {} done",
                b.trim_start_matches("/cache/team/blobs/")
            )),
            "{first}"
        );
    }
    let made = layers(&homes[0], "made:1");

    // Read back, signed with the environment's key: every step.
    let from = format!("{at},name=two");
    let taken = build(
        &homes[1],
        &["--cache-from", &from],
        &[
            ("AWS_ACCESS_KEY_ID", std::ffi::OsStr::new("FROMENV")),
            ("AWS_SECRET_ACCESS_KEY", std::ffi::OsStr::new("s3cr3t")),
        ],
        "taken:1",
    );
    for step in steps {
        assert!(cached(&taken, step), "{step} was taken\n{taken}");
    }
    assert_eq!(layers(&homes[1], "taken:1"), made);
    assert!(
        buckets
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l == "GET /cache/team/manifests/two FROMENV")
    );

    // And with a shared credentials file's profile.
    let profiled = build(
        &homes[2],
        &["--cache-from", &from],
        &[
            ("AWS_SHARED_CREDENTIALS_FILE", shared.as_os_str()),
            ("AWS_PROFILE", std::ffi::OsStr::new("ci")),
        ],
        "taken:2",
    );
    assert!(cached(&profiled, "write:/two=2"), "{profiled}");
    assert!(
        buckets
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l == "GET /cache/team/manifests/two FROMFILE")
    );

    // Written again, layers past touch_refresh are copied onto themselves; none is sent.
    {
        let mut held = buckets.lock().unwrap();
        for b in &blobs {
            held.objects.get_mut(b).unwrap().1 = std::time::UNIX_EPOCH;
        }
        held.log.clear();
    }
    let again = build(
        &homes[0],
        &["--cache-to", &format!("{to},touch_refresh=1h")],
        &[],
        "made:2",
    );
    assert!(!again.contains("writing layer"), "{again}");
    let log = buckets.lock().unwrap().log.clone();
    for b in &blobs {
        assert!(
            log.contains(&format!("PUT {b} copy GIVEN")),
            "{b} was touched: {log:?}"
        );
        assert!(
            !log.contains(&format!("PUT {b} GIVEN")),
            "{b} was sent again: {log:?}"
        );
    }

    // A cache nothing signs for is refused before the build; one not in the bucket is no
    // cache.
    let refused = shards_in(
        &homes[1],
        &["build", "--cache-to", &at, ctx.to_str().unwrap()],
        &[("HOME", files.as_os_str())],
    );
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains("no AWS credentials for the s3 cache"),
        "{}",
        refused.stderr
    );
    let missing = build(
        &homes[1],
        &[
            "--cache-from",
            &format!("{at},name=absent,access_key_id=GIVEN,secret_access_key=s3cr3t"),
        ],
        &[],
        "taken:3",
    );
    assert!(!missing.contains("WARNING"), "{missing}");
}

/// The Azure Blob Storage cache backend (D89): `--cache-to type=azblob` makes its
/// container and writes a build's layers and records to it as BuildKit's does, under
/// `prefix`, at each of `name`'s names, every request signed with the account's key;
/// another home's `--cache-from` takes every step from it (`mode=max`), saying how many
/// layers it found. Written again, no layer is sent twice; a cache with no key is refused
/// before the build.
#[test]
fn builds_take_steps_from_an_azure_blob_cache() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, blobs) = common::fake_azblob();
    let homes: Vec<TempDir> = (0..2)
        .map(|n| TempDir::new(&format!("azblob-cache-home-{n}")))
        .collect();
    let shards_in = |home: &TempDir, args: &[&str]| {
        let env = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ];
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let ctx = context(
        "azblob-cache-ctx",
        &format!(
            "FROM {image} AS build\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/out=built\"]\n\
             FROM {image}\nUSER root\nCOPY --from=build /out /out\nRUN [\"/bin/testguest\", \"fs\", \"write:/two=2\"]\n"
        ),
    );
    let at = format!(
        "type=azblob,account_url=http://127.0.0.1:{port}/devstoreaccount1,account_name=devstoreaccount1,container=cache,prefix=team"
    );
    let key = "secret_access_key=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
    let build = |home: &TempDir, cache: &[&str], tag: &str| {
        let mut args = vec!["build", "--progress=plain"];
        args.extend_from_slice(cache);
        args.extend_from_slice(&["-t", tag, ctx.to_str().unwrap()]);
        let built = shards_in(home, &args);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
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
    let layers = |home: &TempDir, tag: &str| {
        shards_in(
            home,
            &["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag],
        )
        .stdout
    };

    let to = format!("{at},{key},mode=max,name=one;two");
    let first = build(&homes[0], &["--cache-to", &to], "made:1");
    assert!(first.contains("exporting cache to Azure Blob Storage"), "{first}");
    let written: Vec<String> = {
        let held = blobs.lock().unwrap();
        assert!(held.containers.contains("/devstoreaccount1/cache"), "{held:?}");
        for name in ["one", "two"] {
            assert!(
                held.blobs
                    .contains_key(&format!("/devstoreaccount1/cache/team/manifests/{name}")),
                "{held:?}"
            );
        }
        assert!(
            !held.log.iter().any(|l| l.ends_with(" unsigned")),
            "{:?}",
            held.log
        );
        held.blobs
            .keys()
            .filter_map(|k| k.strip_prefix("/devstoreaccount1/cache/team/blobs/"))
            .map(str::to_string)
            .collect()
    };
    assert!(written.len() >= 3, "{written:?}");
    for d in &written {
        assert!(first.contains(&format!("writing layer {d} done")), "{first}");
    }
    let made = layers(&homes[0], "made:1");

    // Read back: every step, its layers counted.
    let taken = build(
        &homes[1],
        &["--cache-from", &format!("{at},{key},name=two")],
        "taken:1",
    );
    for step in ["write:/out=built", "COPY --from=build", "write:/two=2"] {
        assert!(cached(&taken, step), "{step} was taken\n{taken}");
    }
    assert!(
        taken.contains(&format!("found {} layers in cache", written.len())),
        "{taken}"
    );
    assert_eq!(layers(&homes[1], "taken:1"), made);

    // Written again: the layers are there, none is sent.
    blobs.lock().unwrap().log.clear();
    let again = build(&homes[0], &["--cache-to", &to], "made:2");
    assert!(!again.contains("writing layer"), "{again}");
    let log = blobs.lock().unwrap().log.clone();
    assert!(
        !log.iter()
            .any(|l| l.starts_with("PUT /devstoreaccount1/cache/team/blobs/")),
        "{log:?}"
    );

    // No key, no cache, before the build.
    let refused = shards_in(&homes[1], &["build", "--cache-to", &at, ctx.to_str().unwrap()]);
    assert_ne!(refused.status, Some(0));
    assert!(refused.stderr.contains("secret_access_key"), "{}", refused.stderr);
}

/// The GitHub Actions cache backend (D90): `--cache-to type=gha` saves a build's layers
/// and records in GitHub's cache as BuildKit's does through go-actions-cache, the records
/// numbered anew each export; another home's `--cache-from` takes every step from it.
/// Both protocols: v2 (twirp, signed blob URLs), named by `url_v2` or by the runner's
/// environment as buildx reads it, and the legacy v1 (`version=1`). Each request has the
/// shape go-actions-cache's has (testdata/gha.json, `scripts/gha/generate`). A token past
/// its time is refused before the build.
#[test]
fn builds_take_steps_from_a_github_actions_cache() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let token = common::actions_token(
        r#"[{"Scope":"refs/heads/main","Permission":3},{"Scope":"refs/heads/feature","Permission":1}]"#,
    );
    let (port, actions) = common::fake_gha(token.clone());
    let homes: Vec<TempDir> = (0..4)
        .map(|n| TempDir::new(&format!("gha-cache-home-{n}")))
        .collect();
    let shards_in = |home: &TempDir, args: &[&str], extra: &[(&str, &std::ffi::OsStr)]| {
        let mut env = vec![
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
            ("ACTIONS_RUNTIME_TOKEN", std::ffi::OsStr::new("")),
            ("ACTIONS_RESULTS_URL", std::ffi::OsStr::new("")),
            ("ACTIONS_CACHE_URL", std::ffi::OsStr::new("")),
            ("ACTIONS_CACHE_SERVICE_V2", std::ffi::OsStr::new("")),
        ];
        env.extend_from_slice(extra);
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let ctx = context(
        "gha-cache-ctx",
        &format!(
            "FROM {image} AS build\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/out=built\"]\n\
             FROM {image}\nUSER root\nCOPY --from=build /out /out\nRUN [\"/bin/testguest\", \"fs\", \"write:/two=2\"]\n"
        ),
    );
    let build = |home: &TempDir, cache: &[&str], extra: &[(&str, &std::ffi::OsStr)], tag: &str| {
        let mut args = vec!["build", "--progress=plain"];
        args.extend_from_slice(cache);
        args.extend_from_slice(&["-t", tag, ctx.to_str().unwrap()]);
        let built = shards_in(home, &args, extra);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
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
    let steps = ["write:/out=built", "COPY --from=build", "write:/two=2"];
    let layers = |home: &TempDir, tag: &str| {
        shards_in(
            home,
            &["image", "inspect", "--format", "{{json .RootFS.Layers}}", tag],
            &[],
        )
        .stdout
    };
    // The records' key: the written scope's hash, as BuildKit's indexKey makes it.
    let index = format!(
        "index-buildkit-1-{}",
        common::sha256_digest(b"refs/heads/main").get(7..15).unwrap()
    );

    // v2, named by url_v2.
    let v2 = format!("type=gha,url_v2=http://127.0.0.1:{port}/,token={token}");
    let first = build(
        &homes[0],
        &["--cache-to", &format!("{v2},mode=max")],
        &[],
        "made:1",
    );
    assert!(first.contains("exporting to GitHub Actions Cache"), "{first}");
    let blobs: Vec<String> = {
        let a = actions.lock().unwrap();
        assert!(
            a.entries.contains_key(&format!("{index}#1")),
            "{:?}",
            a.entries.keys()
        );
        a.entries
            .keys()
            .filter(|k| k.starts_with("buildkit-blob-1-sha256:"))
            .cloned()
            .collect()
    };
    assert!(blobs.len() >= 3, "{blobs:?}");
    for b in &blobs {
        assert!(
            first.contains(&format!(
                "writing layer {} done",
                b.trim_start_matches("buildkit-blob-1-")
            )),
            "{first}"
        );
    }
    // Each call has go-actions-cache's shape: its fields, its version, its headers.
    let oracle: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/build/testdata/gha.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let shape = |path: &str| -> Option<(Vec<String>, serde_json::Value)> {
        oracle["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == path)
            .map(|r| {
                let names = r["headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| h[0].as_str().unwrap().to_string())
                    .filter(|n| !["user-agent", "content-length", "accept-encoding"].contains(&n.as_str()))
                    .collect();
                (
                    names,
                    serde_json::from_str(r["body"].as_str().unwrap_or("null")).unwrap_or_default(),
                )
            })
    };
    for (method, path, names, body) in actions.lock().unwrap().log.clone() {
        let Some((want, oracle_body)) = shape(&path) else {
            continue;
        };
        for n in &want {
            assert!(names.contains(n), "{method} {path}: no {n} among {names:?}");
        }
        if let (Some(got), Some(want)) = (
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.as_object().cloned()),
            oracle_body.as_object(),
        ) {
            assert_eq!(
                got.keys().collect::<Vec<_>>(),
                want.keys().collect::<Vec<_>>(),
                "{path}"
            );
            assert_eq!(got.get("version"), want.get("version"), "{path}");
        }
    }
    let made = layers(&homes[0], "made:1");

    // Read back from the runner's environment, as buildx reads it.
    let runner = [
        ("ACTIONS_RUNTIME_TOKEN", std::ffi::OsStr::new(token.as_str())),
        ("ACTIONS_CACHE_SERVICE_V2", std::ffi::OsStr::new("true")),
    ];
    let url = format!("http://127.0.0.1:{port}/");
    let mut env = runner.to_vec();
    env.push(("ACTIONS_RESULTS_URL", std::ffi::OsStr::new(url.as_str())));
    let taken = build(&homes[1], &["--cache-from", "type=gha"], &env, "taken:1");
    for step in steps {
        assert!(cached(&taken, step), "{step} was taken\n{taken}");
    }
    assert_eq!(layers(&homes[1], "taken:1"), made);

    // Written again: the next number, no layer sent.
    let again = build(
        &homes[0],
        &["--cache-to", &format!("{v2},mode=max")],
        &[],
        "made:2",
    );
    assert!(!again.contains("writing layer"), "{again}");
    assert!(
        actions
            .lock()
            .unwrap()
            .entries
            .contains_key(&format!("{index}#2"))
    );

    // The legacy service.
    let v1 = format!("type=gha,url=http://127.0.0.1:{port}/,token={token},version=1,scope=legacy");
    build(
        &homes[0],
        &["--cache-to", &format!("{v1},mode=max")],
        &[],
        "made:3",
    );
    let legacy = format!(
        "index-legacy-1-{}",
        common::sha256_digest(b"refs/heads/main").get(7..15).unwrap()
    );
    assert!(
        actions
            .lock()
            .unwrap()
            .entries
            .contains_key(&format!("{legacy}#1"))
    );
    assert!(
        actions
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|(m, p, _, _)| m == "PATCH" && p.starts_with("/_apis/artifactcache/caches/"))
    );
    let taken = build(&homes[2], &["--cache-from", &v1], &[], "taken:2");
    for step in steps {
        assert!(cached(&taken, step), "{step} was taken\n{taken}");
    }

    // A token past its time, before the build.
    let old = {
        use base64::Engine as _;
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.x",
            enc(b"{}"),
            enc(br#"{"ac":"[]","exp":1600000000,"nbf":1500000000}"#)
        )
    };
    let refused = shards_in(
        &homes[3],
        &[
            "build",
            "--cache-to",
            &format!("type=gha,url_v2={url},token={old}"),
            ctx.to_str().unwrap(),
        ],
        &[],
    );
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("cache token expired at 2020-09-13T12:26:40Z"),
        "{}",
        refused.stderr
    );
}

/// What the build's flags give each `RUN`, as BuildKit's frontend gives them (D64):
/// `--add-host`'s names in its `/etc/hosts`, `--shm-size`'s `/dev/shm`, and the limits of
/// `--memory` and `--cpu-quota` (and `--resource`) in a cgroup of its own, as runc writes
/// them; a step past its memory is ended (137), and the build fails as BuildKit's does.
#[test]
fn run_steps_take_the_builds_hosts_shm_and_limits() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("build-limits-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "build-limits-ctx",
        &format!(
            "FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"print:/etc/hosts\", \"print:/proc/self/mounts\", \
             \"print:/sys/fs/cgroup/memory.max\", \"print:/sys/fs/cgroup/cpu.max\"]\n"
        ),
    );
    let built = shards(&[
        "build",
        "--progress=plain",
        "--no-cache",
        "--add-host",
        "db:10.9.8.7",
        "--shm-size",
        "32m",
        "-m",
        "96m",
        "--resource",
        "cpu-quota=50000",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let log = &built.stderr;
    assert!(log.lines().any(|l| l.ends_with("10.9.8.7\tdb")), "{log}");
    assert!(
        log.lines()
            .any(|l| l.contains(" /dev/shm tmpfs ") && l.contains("size=32768k")),
        "{log}"
    );
    assert!(log.lines().any(|l| l.ends_with(" 100663296")), "{log}");
    assert!(log.lines().any(|l| l.ends_with(" 50000 100000")), "{log}");

    // Past its memory, the step is ended, as runc's container would be.
    let hungry = context(
        "build-limits-hungry",
        &format!("FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"alloc\", \"256\"]\n"),
    );
    let ended = shards(&[
        "build",
        "--progress=plain",
        "-m",
        "64m",
        "--memory-swap",
        "64m",
        hungry.to_str().unwrap(),
    ]);
    assert_eq!(ended.status, Some(1), "{}", ended.stderr);
    assert!(
        ended
            .stderr
            .contains("did not complete successfully: exit code: 137"),
        "{}",
        ended.stderr
    );
    // Without a limit, the same step runs.
    let fed = shards(&["build", "--progress=plain", hungry.to_str().unwrap()]);
    assert_eq!(fed.status, Some(0), "{}", fed.stderr);

    // What BuildKit and buildx refuse, in their words.
    let refused = shards(&["build", "--network", "bridge", ctx.to_str().unwrap()]);
    assert!(
        refused
            .stderr
            .contains("network mode \"bridge\" not supported by buildkit"),
        "{}",
        refused.stderr
    );
    let refused = shards(&["build", "--builder", "nope", ctx.to_str().unwrap()]);
    assert!(
        refused.stderr.contains("no builder \"nope\" found"),
        "{}",
        refused.stderr
    );
    let taken = shards(&[
        "build",
        "-q",
        "--builder",
        "default",
        "-D",
        hungry.to_str().unwrap(),
    ]);
    assert_eq!(taken.status, Some(0), "{}", taken.stderr);
    let refused = shards(&["build", "--resource", "gpu=1", ctx.to_str().unwrap()]);
    assert!(
        refused.stderr.contains("unknown resource \"gpu\""),
        "{}",
        refused.stderr
    );
    let warned = shards(&["build", "--squash", "-q", ctx.to_str().unwrap()]);
    assert!(
        warned
            .stderr
            .contains("WARNING: experimental flag squash is removed with BuildKit."),
        "{}",
        warned.stderr
    );
}

/// `shards builder prune` (D65), as buildx's prune: a record no filter keeps goes, said in
/// buildx's table, and the steps run again; `until` keeps what was used since; a record
/// whose layer an image holds stays unless `--all`; the totals as buildx says them.
#[test]
fn builder_prune_removes_the_build_cache_as_buildx_does() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("builder-prune-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "builder-prune-ctx",
        &format!("FROM {image}\nUSER root\nRUN [\"/bin/testguest\", \"fs\", \"write:/one=1\"]\n"),
    );
    let out = TempDir::new("builder-prune-out");
    let to_dir = format!("type=local,dest={}", out.display());
    let build = |tag: Option<&str>| {
        let mut args = vec!["build", "--progress=plain"];
        match tag {
            Some(t) => args.extend_from_slice(&["-t", t]),
            // Files out alone: no image, dangling or named, holds the step's layer.
            None => args.extend_from_slice(&["-o", &to_dir]),
        }
        args.push(ctx.to_str().unwrap());
        let built = shards(&args);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        built.stderr
    };
    let ran = |log: &str| !log.lines().any(|l| l.ends_with(" CACHED"));
    // Built to a directory: its layer no image holds.
    assert!(ran(&build(None)));
    // A fresh record, kept by an age it has not reached.
    let kept = shards(&["builder", "prune", "-f", "--filter", "until=24h"]);
    assert_eq!(kept.stdout, "Total:\t0B\n", "{}", kept.stderr);
    let pruned = shards(&["builder", "prune", "-f"]);
    assert_eq!(pruned.status, Some(0), "{}", pruned.stderr);
    let lines: Vec<&str> = pruned.stdout.lines().collect();
    assert_eq!(
        lines.first(),
        Some(&"ID\t\t\t\t\t\tRECLAIMABLE\tSIZE\t\tLAST ACCESSED"),
        "{}",
        pruned.stdout
    );
    assert!(lines.iter().any(|l| l.contains("\ttrue \t")), "{}", pruned.stdout);
    assert!(
        lines
            .last()
            .is_some_and(|l| l.starts_with("Total:\t") && *l != "Total:\t0B"),
        "{}",
        pruned.stdout
    );
    assert!(ran(&build(None)), "the step was cached after the prune");

    // Named, its layer an image's: kept without --all, removed with it.
    build(Some("pruned:1"));
    shards(&["builder", "prune", "-f"]);
    assert!(!ran(&build(Some("pruned:2"))), "a shared record was pruned");
    let all = shards(&["buildx", "prune", "-af"]);
    assert!(all.stdout.lines().count() > 1, "{}", all.stdout);
    assert!(ran(&build(Some("pruned:3"))), "--all kept the record");
}

/// `--annotation` (D66): the manifest's in the manifest, the descriptor's on the
/// descriptor (an OCI layout's index, the metadata file), as BuildKit's image exporter puts
/// them; an index's, and another platform's, refused in its words.
#[test]
fn annotations_land_where_buildkit_puts_them() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("annotate-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("annotate-ctx", &format!("FROM {image}\nLABEL a=b\n"));
    let out = TempDir::new("annotate-out");
    let meta = out.join("meta.json");
    let layout = out.join("layout");
    let built = shards(&[
        "build",
        "--annotation",
        "org.opencontainers.image.title=app",
        "--annotation",
        "manifest-descriptor:org.example.where=index",
        "--metadata-file",
        meta.to_str().unwrap(),
        "-o",
        &format!(
            "type=oci,dest={},tar=false,annotation.org.example.from=output",
            layout.display()
        ),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let desc = &index["manifests"][0];
    assert_eq!(desc["annotations"]["org.example.where"], "index", "{index}");
    let digest = desc["digest"].as_str().unwrap().trim_start_matches("sha256:");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("blobs/sha256").join(digest)).unwrap()).unwrap();
    assert_eq!(
        manifest["annotations"]["org.opencontainers.image.title"], "app",
        "{manifest}"
    );
    assert_eq!(
        manifest["annotations"]["org.example.from"], "output",
        "{manifest}"
    );
    assert!(
        manifest["annotations"].get("org.example.where").is_none(),
        "{manifest}"
    );
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    assert_eq!(
        written["containerimage.descriptor"]["annotations"]["org.example.where"], "index",
        "{written}"
    );

    let refused = shards(&["build", "--annotation", "index:k=v", ctx.to_str().unwrap()]);
    assert!(
        refused
            .stderr
            .contains("index annotations not supported for single platform export"),
        "{}",
        refused.stderr
    );
    let other = if cfg!(target_arch = "aarch64") {
        "linux/amd64"
    } else {
        "linux/arm64"
    };
    let refused = shards(&[
        "build",
        "--annotation",
        &format!("manifest[{other}]:k=v"),
        ctx.to_str().unwrap(),
    ]);
    assert!(
        refused.stderr.contains(&format!(
            "invalid annotation: no platform {other} found in source"
        )),
        "{}",
        refused.stderr
    );
}

/// `--check` (D67, D73): the build's checks said as buildx says the lint subrequest's
/// result, as text or BuildKit's JSON, of every stage when no target is named; nothing
/// built, exit 1 for warnings unless `ignorestatus`, and for an error planning it, said
/// where it is; with `--debug`, a build's warnings in full.
#[test]
fn check_says_the_builds_warnings_as_buildx_does() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("check-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("check-ctx", &format!("FROM {image} as Build\nLABEL a=b\n"));
    let checked = shards(&["build", "--check", ctx.to_str().unwrap()]);
    assert_eq!(checked.status, Some(1), "{}", checked.stderr);
    let out = &checked.stdout;
    assert!(
        out.starts_with("Check complete, 2 warnings have been found!\n\nWARNING: "),
        "{out}"
    );
    assert!(
        out.contains(
            "\nWARNING: FromAsCasing - https://docs.docker.com/go/dockerfile/rule/from-as-casing/\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("Dockerfile:1\n--------------------\n   1 | >>> FROM "),
        "{out}"
    );
    assert!(!checked.stderr.contains("exporting"), "{}", checked.stderr);
    let ignored = shards(&[
        "build",
        "--call",
        "check,ignorestatus=true",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(ignored.status, Some(0), "{}", ignored.stderr);

    let clean = context("check-clean", &format!("FROM {image}\n"));
    let fine = shards(&["build", "--check", clean.to_str().unwrap()]);
    assert_eq!(
        (fine.status, fine.stdout.as_str()),
        (Some(0), "Check complete, no warnings found.\n"),
        "{}",
        fine.stderr
    );

    let debugged = shards(&["build", "--debug", "--progress=plain", ctx.to_str().unwrap()]);
    assert_eq!(debugged.status, Some(0), "{}", debugged.stderr);
    assert!(
        !debugged.stderr.contains("use shards --debug"),
        "{}",
        debugged.stderr
    );
    assert!(
        debugged
            .stderr
            .contains("More info: https://docs.docker.com/go/dockerfile/rule/from-as-casing/\n"),
        "{}",
        debugged.stderr
    );

    // With format=json, BuildKit's LintResults: each warning as found, the file, exit 1.
    let json = shards(&["build", "--call", "check,format=json", ctx.to_str().unwrap()]);
    assert_eq!(json.status, Some(1), "{}", json.stderr);
    let results: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let rules: Vec<&str> = results["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["ruleName"].as_str().unwrap())
        .collect();
    assert_eq!(rules.len(), 2, "{}", json.stdout);
    assert!(rules.contains(&"FromAsCasing"), "{}", json.stdout);
    let source = &results["sources"][0];
    assert_eq!(source["filename"], "Dockerfile");
    assert_eq!(source["language"], "Dockerfile");
    assert!(results.get("buildError").is_none(), "{}", json.stdout);
    let clean_json = shards(&["build", "--call=check,format=json", clean.to_str().unwrap()]);
    assert_eq!(clean_json.status, Some(0), "{}", clean_json.stderr);
    assert!(
        clean_json.stdout.starts_with("{\n  \"warnings\": null,\n"),
        "{}",
        clean_json.stdout
    );

    // With no target, every stage is checked, those the build would not reach too
    // (DockerfileLint's AllStages); with one, only what it reaches.
    let unreached = context(
        "check-unreached",
        &format!("FROM {image} AS unused\nCOPY . $ALSO\nFROM {image} AS last\nLABEL a=b\n"),
    );
    let all = shards(&["build", "--check", unreached.to_str().unwrap()]);
    assert_eq!(all.status, Some(1), "{}", all.stderr);
    assert!(
        all.stdout.contains("Usage of undefined variable '$ALSO'"),
        "{}",
        all.stdout
    );
    let last = shards(&[
        "build",
        "--check",
        "--target",
        "last",
        unreached.to_str().unwrap(),
    ]);
    assert_eq!(
        (last.status, last.stdout.as_str()),
        (Some(0), "Check complete, no warnings found.\n"),
        "{}",
        last.stderr
    );

    // An error planning the build fails the call, said after the warnings found before it,
    // where it is in the file; with format=json, the result says it too.
    let broken = context(
        "check-broken",
        &format!("FROM {image} AS Base\nRUN --mount=type=secret,id=a,required=maybe true\n"),
    );
    let failed = shards(&["build", "--check", broken.to_str().unwrap()]);
    assert_eq!(failed.status, Some(1), "{}", failed.stderr);
    assert!(
        failed
            .stderr
            .contains("ERROR: invalid value for required: maybe\nDockerfile:2\n--------------------\n"),
        "{}",
        failed.stderr
    );
    let failed_json = shards(&["build", "--call=check,format=json", broken.to_str().unwrap()]);
    assert_eq!(failed_json.status, Some(1), "{}", failed_json.stderr);
    let results: serde_json::Value = serde_json::from_str(&failed_json.stdout).unwrap();
    assert_eq!(
        results["buildError"]["message"],
        "invalid value for required: maybe"
    );
    assert_eq!(results["buildError"]["location"]["ranges"][0]["start"]["line"], 2);
}

/// `RUN --mount=type=ssh` reaches the client's SSH agent through the builder, as
/// BuildKit's steps reach it (`--ssh default`, `SSH_AUTH_SOCK` in the step): the step
/// sees the agent's keys and has it sign, and cannot have it forget them, which BuildKit's
/// read-only agent refuses; the same of a key file the client serves (D68); without
/// `--ssh`, BuildKit's refusal.
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

    // The key file itself, served by the client as BuildKit's keyring serves it: the
    // step sees the key (its comment dropped, as x/crypto drops it) and has it sign, and
    // is refused what a read-only agent refuses.
    let from_file = shards(&[
        "build",
        "--progress=plain",
        "--no-cache",
        "--ssh",
        &format!("default={}", key.display()),
        "--allow",
        "security.insecure",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(from_file.status, Some(0), "{}", from_file.stderr);
    for line in [
        " agent keys 1 \n",
        " agent signed\n",
        " agent remove refused\n",
        " vsock-agent refused\n",
    ] {
        assert!(from_file.stderr.contains(line), "{line:?}\n{}", from_file.stderr);
    }
    // A key file locked by a passphrase, and keys beside a socket: buildx's refusals.
    let locked = keys.join("locked");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "a passphrase", "-f"])
        .arg(&locked)
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let refused = shards(&[
        "build",
        "--ssh",
        &format!("k={}", locked.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused.stderr.contains(&format!(
            "failed to parse {}: ssh: this private key is passphrase protected",
            locked.display()
        )),
        "{}",
        refused.stderr
    );
    let mixed = shards(&[
        "build",
        "--ssh",
        &format!("k={},{}", key.display(), sock.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_ne!(mixed.status, Some(0));
    assert!(
        mixed.stderr.contains("invalid combination of keys and sockets"),
        "{}",
        mixed.stderr
    );

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
    // The tag names the index of the image and its provenance; each pushed by its digest.
    let (_, index) = manifests.get("1").expect("its tag pushed");
    let index: serde_json::Value = serde_json::from_slice(index).unwrap();
    assert_eq!(
        index["mediaType"], "application/vnd.oci.image.index.v1+json",
        "{index}"
    );
    let child = |i: usize| -> serde_json::Value {
        let d = index["manifests"][i]["digest"].as_str().unwrap();
        serde_json::from_slice(&manifests.get(d).expect("each manifest pushed").1).unwrap()
    };
    assert_eq!(
        index["manifests"][1]["annotations"]["vnd.docker.reference.type"],
        "attestation-manifest"
    );
    let attestation = child(1);
    assert_eq!(
        attestation["layers"][0]["mediaType"],
        "application/vnd.in-toto+json"
    );
    let manifest = child(0);
    let blobs = repos.blobs.get("team/built").expect("its blobs pushed");
    assert!(
        blobs.contains_key(attestation["layers"][0]["digest"].as_str().unwrap()),
        "the statement pushed"
    );
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

/// An OSI artifact of several platforms (§8 Q1, D97): `--platform` makes a manifest for
/// each, its config naming its platform and its content `DIR/<os>_<arch>` where there is
/// one, else `DIR` without those; the name resolves to an index carrying the artifact
/// type, which `push` sends whole; `AGENT … FROM` takes the guest's manifest out of it.
#[test]
fn osi_artifacts_of_several_platforms_are_an_index() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("build-osi-index-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let guest = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let other = if guest == "arm64" { "amd64" } else { "arm64" };
    let dir = TempDir::new("build-osi-index-agent");
    std::fs::create_dir_all(dir.join(format!("linux_{guest}/bin"))).unwrap();
    std::fs::write(dir.join(format!("linux_{guest}/bin/run")), format!("{guest}\n")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/run"), "shared\n").unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"main","version":"1.0.0","run":{"command":["bin/run"]}}"#,
    )
    .unwrap();
    let name = format!("127.0.0.1:{port}/team/multi:1");
    let platforms = format!("linux/{other},linux/{guest}");
    let made = shards(&[
        "build",
        "agent",
        dir.to_str().unwrap(),
        "--platform",
        &platforms,
        "-t",
        &name,
    ]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    {
        let repos = repos.lock().unwrap();
        let (_, index) = repos.manifests["team/multi"]["1"].clone();
        let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
        assert_eq!(index["mediaType"], "application/vnd.oci.image.index.v1+json");
        assert_eq!(index["artifactType"], "application/vnd.osi.agent.v1");
        let entries = index["manifests"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        for (entry, arch) in entries.iter().zip([other, guest]) {
            assert_eq!(entry["platform"]["os"], "linux");
            assert_eq!(entry["platform"]["architecture"], arch);
            assert_eq!(entry["artifactType"], "application/vnd.osi.agent.v1");
        }
    }
    let removed = shards(&["rm", "agent", &name]);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    let ctx = context("build-osi-index-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM {name}\n"),
    )
    .unwrap();
    let out = TempDir::new("build-osi-index-out");
    let built = shards(&[
        "build",
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let root = out.join("root");
    assert_eq!(
        std::fs::read_to_string(root.join("agents/main/bin/run")).unwrap(),
        format!("{guest}\n")
    );
    assert!(!root.join(format!("agents/main/linux_{guest}")).exists());
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("agents/main.d/osi.json")).unwrap()).unwrap();
    assert_eq!(config["platform"]["architecture"], guest);

    // The other platform's manifest holds the shared content, without the guest's directory.
    let other_dir = TempDir::new("build-osi-index-other");
    std::fs::write(
        other_dir.join("agent.json"),
        format!(r#"{{"name":"main","version":"1.0.0","run":{{"command":["bin/run"]}},"platform":{{"os":"linux","architecture":"{guest}"}}}}"#),
    )
    .unwrap();
    let refused = shards(&[
        "build",
        "agent",
        other_dir.to_str().unwrap(),
        "--platform",
        &platforms,
        "-t",
        "local/bad:1",
    ]);
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains(&format!("says its platform is linux/{guest}, not linux/{other}")),
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
/// domain's hidden, a scratch `/tmp` of its own, a `/dev` of six nodes; under Landlock, no
/// write but to its scratch and `/dev`, and no TCP; under seccomp, no vsock, netlink,
/// io_uring, keys or userfaultfd; its output on the
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
        r#"{"name":"main","run":{"command":["bin/testguest","confined","see","/agents/other","/sys","/dev","/proc/self/fd","write","/agents/main/x","/etc/x","/tmp/x","/tmp/../proc/self/comm","bind","127.0.0.1:8080","connect","127.0.0.1:9","call","fork","thread","socket-vsock","socket-netlink","socket-unix","socket-inet6","io_uring_setup","keyctl","userfaultfd"]}}"#,
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
        format!("FROM {image}\nAGENT main FROM {name}\nAGENT other FROM ./other\nAGENT --processes=none solo FROM {name}\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "confined:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // The workload ends once the agent has said what it sees.
    let ran = shards(&["run", "--rm", "confined:1", "await", "confined-ready", "2"]);
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
        "confined see /dev: fd,full,mqueue,null,random,shm,stderr,stdin,stdout,tty,urandom,zero",
        // Its stdio and the directory being listed: nothing of init's.
        "confined see /proc/self/fd: 0,1,2,3",
        // Its directory and the system read-only; its scratch its own.
        "confined write /agents/main/x: errno 30",
        "confined write /etc/x: errno 30",
        "confined write /tmp/x: ok",
        "confined net=lo",
        // Landlock: no write outside its scratch and /dev, though the file is its own,
        // and no TCP, though the loopback is its own.
        "confined write /tmp/../proc/self/comm: errno 13",
        "confined bind 127.0.0.1:8080: errno 13",
        "confined connect 127.0.0.1:9: errno 13",
        // seccomp: no vsock, netlink, io_uring, keys or userfaultfd; Unix and IP sockets.
        "confined call socket-vsock: errno 1",
        "confined call socket-netlink: errno 1",
        "confined call socket-unix: ok",
        "confined call socket-inet6: ok",
        "confined call io_uring_setup: errno 1",
        "confined call keyctl: errno 1",
        "confined call userfaultfd: errno 1",
        "confined call fork: ok",
        "confined call thread: ok",
    ] {
        assert!(said.contains(&want), "no {want:?} in:\n{}", ran.stderr);
    }
    // `--processes=none`: threads, and no process.
    let solo: Vec<&str> = ran
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[agent solo] "))
        .collect();
    for want in [
        "confined uid=200002 gid=200002 groups=0",
        "confined call fork: errno 1",
        "confined call thread: ok",
    ] {
        assert!(solo.contains(&want), "no {want:?} in:\n{}", ran.stderr);
    }
    assert!(
        !ran.stderr.contains("[agent other]"),
        "an agent of files alone runs nothing:\n{}",
        ran.stderr
    );
}

/// IPv6 between agents (D99): on a network with IPv6 (`NETWORK --ipv6`), each member has
/// an address of its IPv6 subnet, and the grants hold by IPv6 as by IPv4: `CONNECT
/// --port=7000 a TO b` lets `a` reach `b` at its IPv6 address on 7000 alone, `b` only
/// answer it, and `d`, paired with `b` alone, reach `b` and not `a`; each names its granted
/// peers at both addresses.
#[test]
fn agents_reach_by_ipv6_only_what_connect_grants() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("links6-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let agent = |name: &str, args: &[&str]| -> String {
        let dir = TempDir::new(&format!("links6-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        let command: Vec<String> = ["bin/testguest", "confined", "cat", "/etc/hosts"]
            .iter()
            .chain(args)
            .map(|a| format!("{a:?}"))
            .collect();
        std::fs::write(
            dir.join("agent.json"),
            format!(
                r#"{{"name":"{name}","run":{{"command":[{}]}}}}"#,
                command.join(",")
            ),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/links6-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        tag
    };
    // Members in order: a ::2, b ::3, d ::4.
    let a = agent("a", &["reach", "[fd31:3::3]:7000", "unreach", "[fd31:3::3]:7001"]);
    let b = agent("b", &["listen", "7000", "7001", "unreach", "[fd31:3::2]:7000"]);
    let d = agent("d", &["reach", "[fd31:3::3]:7000", "unreach", "[fd31:3::2]:7000"]);
    let ctx = context("links6-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT a FROM {a}\nAGENT b FROM {b}\nAGENT d FROM {d}\n\
             NETWORK --ipv6 --subnet=172.31.3.0/24 --subnet=fd31:3::/64 --ingress=7000-7001 back\n\
             CONNECT --port=7000 a TO b ON back\nCONNECT --port=7000 d WITH b ON back\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "links6:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "links6:1", "await", "confined-ready", "3"]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let said = |who: &str| -> Vec<String> {
        let prefix = format!("[agent {who}] ");
        ran.stderr
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix).map(str::to_string))
            .collect()
    };
    for (who, wants) in [
        (
            "a",
            &[
                "confined cat /etc/hosts: fd31:3::2\ta",
                "confined cat /etc/hosts: fd31:3::3\tb",
                "confined reach [fd31:3::3]:7000: ok",
                // b listens on 7001 too, which no CONNECT grants: dropped.
                "confined unreach [fd31:3::3]:7001: timeout",
            ][..],
        ),
        (
            "b",
            &[
                "confined listen 7000: ok",
                // a TO b: b only answers a.
                "confined unreach [fd31:3::2]:7000: timeout",
            ][..],
        ),
        (
            "d",
            &[
                "confined reach [fd31:3::3]:7000: ok",
                // Paired with b alone: its way to a is dropped.
                "confined unreach [fd31:3::2]:7000: timeout",
            ][..],
        ),
    ] {
        let lines = said(who);
        for want in wants {
            assert!(
                lines.iter().any(|l| l == want),
                "no {want:?} from {who} in:\n{}",
                ran.stderr
            );
        }
    }
}

/// Agents reach one another as the Agentfile's `CONNECT`s grant and nothing more (D59,
/// AGENTFILE_ARCH.md §4.6, §4.7, §9.7; networks are default deny), each over one link to
/// a switch that decides by link and port. On `back`, `CONNECT --port=7000 a TO b` lets
/// `a` reach `b` on 7000 alone and `b` only answer it; `d`, paired with `b` alone, reaches
/// `b` and not `a`, a member of the same network; `c`, alone on `side`, may neither bind
/// nor connect. Each resolves itself and the peers it is granted, and no one else.
#[test]
fn agents_reach_only_what_connect_grants() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("links-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let agent = |name: &str, args: &[&str]| -> String {
        let dir = TempDir::new(&format!("links-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        let command: Vec<String> = ["bin/testguest", "confined", "cat", "/etc/hosts"]
            .iter()
            .chain(args)
            .map(|a| format!("{a:?}"))
            .collect();
        std::fs::write(
            dir.join("agent.json"),
            format!(
                r#"{{"name":"{name}","run":{{"command":[{}]}}}}"#,
                command.join(",")
            ),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/links-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        tag
    };
    // b listens on two ports, of which a CONNECT grants one.
    let a = agent(
        "a",
        &[
            "resolve",
            "localhost",
            "listen",
            "7000",
            "reach",
            "b:7000",
            "unreach",
            "b:7001",
        ],
    );
    let b = agent("b", &["listen", "7000", "7001", "unreach", "172.31.1.2:7000"]);
    let c = agent("c", &["listen", "7000", "unreach", "172.31.1.3:7000"]);
    let d = agent("d", &["reach", "b:7000", "unreach", "172.31.1.2:7000"]);
    let ctx = context("links-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT a FROM {a}\nAGENT b FROM {b}\nAGENT c FROM {c}\nAGENT d FROM {d}\n\
             NETWORK --subnet=172.31.1.0/24 --ingress=7000-7001 back\nNETWORK --subnet=172.31.2.0/24 side\n\
             CONNECT --port=7000 a TO b ON back\nCONNECT --port=7000 d WITH b ON back\nCONNECT c WITH c ON side\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "links:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "links:1", "await", "confined-ready", "4"]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let said = |who: &str| -> Vec<String> {
        let prefix = format!("[agent {who}] ");
        ran.stderr
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix).map(str::to_string))
            .collect()
    };
    for (who, wants) in [
        (
            "a",
            &[
                "confined net=lo,eth0",
                "confined resolve localhost: 0,0",
                // No grant lets anything reach a: Landlock refuses its bind.
                "confined listen 7000: errno 13",
                "confined reach b:7000: ok",
                // b listens on 7001 too, which no CONNECT grants: dropped.
                "confined unreach b:7001: timeout",
            ][..],
        ),
        (
            "b",
            &[
                "confined listen 7000: ok",
                "confined listen 7001: ok",
                // a TO b: b only answers a; nothing listens on a, so a timeout is the
                // switch's drop, where a packet let through would be refused. By address:
                // b is granted no flow to a, so it resolves no name of a's.
                "confined unreach 172.31.1.2:7000: timeout",
            ][..],
        ),
        (
            "c",
            &[
                // Alone on side: Landlock lets it neither bind nor connect.
                "confined listen 7000: errno 13",
                "confined unreach 172.31.1.3:7000: errno 13",
            ][..],
        ),
        (
            "d",
            &[
                "confined reach b:7000: ok",
                // A member of back, as a is, and paired with b alone: membership grants
                // no flow, so d's way to a is dropped.
                "confined unreach 172.31.1.2:7000: timeout",
            ][..],
        ),
    ] {
        let lines = said(who);
        for want in wants {
            assert!(
                lines.iter().any(|l| l == want),
                "no {want:?} from {who} in:\n{}",
                ran.stderr
            );
        }
    }
    // Each resolves itself and the peers it may reach, and no one else, after Docker's own
    // lines.
    let hosts = |who: &str| -> Vec<String> {
        said(who)
            .into_iter()
            .filter_map(|l| l.strip_prefix("confined cat /etc/hosts: ").map(str::to_string))
            .filter(|l| l.starts_with("172.31."))
            .collect()
    };
    assert_eq!(hosts("a"), ["172.31.1.2\ta", "172.31.1.3\tb"]);
    assert_eq!(hosts("b"), ["172.31.1.3\tb", "172.31.1.4\td"]);
    assert_eq!(hosts("d"), ["172.31.1.3\tb", "172.31.1.4\td"]);
    assert_eq!(hosts("c"), ["172.31.2.2\tc"]);
}

/// Agents reach past their microVM the ports their networks grant (D59, AGENTFILE_ARCH.md
/// §4.1, §4.6, §9.7, §12 answer 6): `NETWORK --egress` on a network that is not internal,
/// with `EXPOSE ... FOR` it at the microVM's boundary, lets the agents a `CONNECT` joins to
/// it open flows to that port, through the switch's link to the microVM's own network and
/// its network process, and no port only one boundary opens; an agent on an
/// internal network reaches nothing past the microVM, though its network names the port
/// and Landlock lets it connect to its peer; and the run's own command reaches nothing it
/// did not before.
#[test]
fn agents_reach_past_the_microvm_what_their_networks_grant() {
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
    let host = host.ip();
    // Two servers on the host: one port granted, one not. Each answers every connection
    // until the test ends.
    let serve = || {
        let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || for _ in server.incoming() {});
        port
    };
    let (granted, other) = (serve(), serve());
    // And one answering UDP, which only e's network grants.
    let echo = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let echoed = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = common::recv_from(&echo, &mut buf) {
            let _ = echo.send_to(&buf[..n], from);
        }
    });
    // The host's resolver, as the daemon is told to ask (SHARDS_DNS): `api.example` is
    // this host; any other question has no answer.
    let resolver = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let resolver_at = format!("{host}:{}", resolver.local_addr().unwrap().port());
    let address = match host {
        std::net::IpAddr::V4(a) => a.octets(),
        std::net::IpAddr::V6(_) => panic!("an IPv4 host address"),
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = common::recv_from(&resolver, &mut buf) {
            let q = &buf[..n];
            // The question: its name's labels, then type and class (RFC 1035 §4.1.2).
            let mut at = 12;
            let mut name = Vec::new();
            while at < q.len() && q[at] != 0 {
                let len = usize::from(q[at]);
                name.push(String::from_utf8_lossy(&q[at + 1..at + 1 + len]).to_lowercase());
                at += 1 + len;
            }
            let question_end = at + 5;
            let qtype = u16::from_be_bytes([q[at + 1], q[at + 2]]);
            let answer = name.join(".") == "api.example" && qtype == 1;
            let mut r = q[..2].to_vec();
            r.extend_from_slice(&[0x81, 0x80, 0, 1, 0, u8::from(answer), 0, 0, 0, 0]);
            r.extend_from_slice(&q[12..question_end]);
            if answer {
                r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                r.extend_from_slice(&address);
            }
            let _ = resolver.send_to(&r, from);
        }
    });
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("egress-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_DNS", std::ffi::OsStr::new(&resolver_at)),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let agent = |name: &str, args: &[String]| -> String {
        let dir = TempDir::new(&format!("egress-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        let command: Vec<String> = ["bin/testguest", "confined"]
            .iter()
            .map(|s| (*s).to_string())
            .chain(args.iter().cloned())
            .map(|a| format!("{a:?}"))
            .collect();
        std::fs::write(
            dir.join("agent.json"),
            format!(
                r#"{{"name":"{name}","run":{{"command":[{}]}}}}"#,
                command.join(",")
            ),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/egress-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        tag
    };
    let to = |p: u16| format!("{host}:{p}");
    let by_name = format!("api.example:{granted}");
    let a = agent(
        "a",
        &[
            "resolve".into(),
            "api.example".into(),
            "reach".into(),
            to(granted),
            by_name.clone(),
            "unreach".into(),
            to(other),
            // As though answering a flow opened to it on its ingress port: to e's port.
            "udpaskfrom".into(),
            format!("7300,{}", to(echoed)),
        ],
    );
    let b = agent(
        "b",
        &[
            "resolve".into(),
            "api.example".into(),
            "unreach".into(),
            to(granted),
        ],
    );
    let c = agent("c", &["listen".into(), "7000".into()]);
    let e = agent("e", &["udpask".into(), to(echoed)]);
    let ctx = context("egress-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT a FROM {a}\nAGENT b FROM {b}\nAGENT c FROM {c}\nAGENT e FROM {e}\n\
             NETWORK --dns --egress={granted} --egress={other} --ingress=7300/udp out\n\
             EXPOSE {granted} AS egress FOR out\nEXPOSE 7300/udp AS ingress FOR out\n\
             NETWORK --egress={echoed}/udp side\nEXPOSE {echoed}/udp AS egress FOR side\nCONNECT e WITH e ON side\n\
             NETWORK --internal --egress={granted} --ingress=7000 inner\nEXPOSE {granted} FOR inner\n\
             CONNECT a WITH a ON out\nCONNECT --port=7000 b WITH c ON inner\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "egress:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&[
        "run",
        "--rm",
        "egress:1",
        "await",
        "confined-ready",
        "4",
        &to(granted),
    ]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let said = |who: &str| -> Vec<String> {
        let prefix = format!("[agent {who}] ");
        ran.stderr
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix).map(str::to_string))
            .collect()
    };
    for (who, want) in [
        ("a", format!("confined reach {}: ok", to(granted))),
        // By name: the host's resolver, asked through the microVM's network process.
        ("a", format!("confined reach {by_name}: ok")),
        // Without egress, no name past the microVM either: its queries are dropped.
        ("b", "confined resolve api.example: -3,-3".to_string()),
        // A port its network grants, but not the microVM's boundary: a flow crossing both
        // needs both (§12 answer 6), and the switch lets nothing else up.
        ("a", format!("confined unreach {}: timeout", to(other))),
        // e's port, which its network grants, answers it; a, sending to it from the port
        // it is reached on, opens nothing (D61).
        ("e", format!("confined udpask {}: reply", to(echoed))),
        ("a", format!("confined udpaskfrom 7300 {}: none", to(echoed))),
        // On an internal network: nothing past the microVM.
        ("b", format!("confined unreach {}: timeout", to(granted))),
    ] {
        assert!(
            said(who).contains(&want),
            "no {want:?} from {who} in:\n{}",
            ran.stderr
        );
    }
    // The run's own command: as without agents.
    assert!(
        ran.stdout
            .contains(&format!("await unreach {}: timeout", to(granted))),
        "{}{}",
        ran.stdout,
        ran.stderr
    );
    // From its first instruction, before any agent starts: eth0 is confined before the
    // command begins (D99), so a datagram past eth0's subnet on a port the agents are
    // granted is refused by the guest's own stack (EPERM), not sent; and over IPv6 alike,
    // on a network with IPv6.
    let first = shards(&["run", "--rm", "egress:1", "udp", &format!("192.0.2.1:{granted}")]);
    assert!(
        first.stdout.contains("udp error Operation not permitted"),
        "{}{}",
        first.stdout,
        first.stderr
    );
    let made = shards(&["network", "create", "--ipv6", "--subnet", "fd79::/64", "six"]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let first6 = shards(&[
        "run",
        "--rm",
        "--network",
        "six",
        "egress:1",
        "udp",
        &format!("[2001:db8::1]:{granted}"),
    ]);
    assert!(
        first6.stdout.contains("udp error Operation not permitted"),
        "{}{}",
        first6.stdout,
        first6.stderr
    );
}

/// What a network lets in past its microVM reaches its agent (D59, AGENTFILE_ARCH.md §4.1,
/// §4.6, §12 answer 6): a port both its own boundary (`NETWORK --ingress`) and the
/// microVM's (`EXPOSE ... AS ingress FOR` it) open inward, published as `shards run -p`
/// publishes it, is the agent's, which answers with its host name; a port the agent
/// listens on that no grant lets in stays the run's own, and no connection to it reaches
/// the agent.
#[test]
fn agents_answer_what_their_networks_let_in() {
    use std::io::Read as _;
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("ingress-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("ingress-agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"a","run":{"command":["bin/testguest","confined","listen","7100","7200"]}}"#,
    )
    .unwrap();
    let tag = format!("127.0.0.1:{port}/team/ingress-a:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &tag]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context("ingress-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT a FROM {tag}\n\
             NETWORK --ingress=7100 front\nEXPOSE 7100 AS ingress FOR front\nEXPOSE 7200\n\
             EXPOSE 9443 AS egress FOR front\n\
             CONNECT a WITH a ON front\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "ingress:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // A port declared AS egress is a destination: publishing it is refused (§12 answer 6).
    let refused = shards(&["run", "--rm", "-p", "127.0.0.1::9443", "ingress:1", "exit", "0"]);
    assert_ne!(refused.status, Some(0), "{}", refused.stdout);
    assert!(
        refused
            .stderr
            .contains("cannot publish port 9443/tcp: the image's Agentfile declares it AS egress"),
        "{}",
        refused.stderr
    );
    let ran = shards(&[
        "run",
        "-d",
        "--name",
        "front",
        "-p",
        "127.0.0.1::7100",
        "-p",
        "127.0.0.1::7200",
        "ingress:1",
        "sleep",
    ]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let listed = shards(&["port", "front"]);
    assert_eq!(listed.status, Some(0), "{}", listed.stderr);
    let host_port = |container: &str| -> u16 {
        listed
            .stdout
            .lines()
            .find(|l| l.starts_with(&format!("{container}/tcp")))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no {container} in {}", listed.stdout))
    };
    let (granted, other) = (host_port("7100"), host_port("7200"));
    // What a connection to the host's published port reads, in 3 s at most.
    let read = |p: u16| -> String {
        let Ok(mut c) = std::net::TcpStream::connect(("127.0.0.1", p)) else {
            return String::new();
        };
        let _ = c.set_read_timeout(Some(std::time::Duration::from_secs(3)));
        let mut got = String::new();
        let _ = c.read_to_string(&mut got);
        got
    };
    // Once the agent listens: until then a connection finds no one.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut said = String::new();
    while std::time::Instant::now() < deadline {
        said = read(granted);
        if !said.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(said, "hello from agent-a\n", "the published ingress port");
    // No grant lets 7200 in: the agent listens on it, and is not reached.
    assert!(!read(other).contains("agent-a"), "port 7200 reached the agent");
    let _ = shards(&["rm", "-f", "front"]);
}

/// A remote MCP server is a grant of that server alone (D59, AGENTFILE_ARCH.md §4.4, §9.6;
/// networks are default deny): the agent its `FOR` names reaches the server's port, at the
/// addresses its host resolves to and no other, may ask for that host's name and no other,
/// and can do nothing to widen it; an agent not named reaches nothing.
#[test]
fn a_remote_mcp_server_is_a_grant_of_that_server_alone() {
    if cannot_run_vms() {
        return;
    }
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(host) = host else {
        eprintln!("SKIP: this host has no route to give a guest an address of it");
        return;
    };
    let host = host.ip();
    let std::net::IpAddr::V4(address) = host else {
        eprintln!("SKIP: this host's address is not IPv4");
        return;
    };
    let serve = || {
        let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || for _ in server.incoming() {});
        port
    };
    let (mcp_port, other) = (serve(), serve());
    // The host's resolver, by UDP and by TCP: mcp.example and api.example are this host,
    // so that a name refused is shards' refusal, not the resolver's; big.example has an
    // answer too long for UDP, which comes truncated by UDP (RFC 1035 §4.2.1) and whole by
    // TCP.
    let resolver = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let resolver_port = resolver.local_addr().unwrap().port();
    let resolver_tcp = std::net::TcpListener::bind(("0.0.0.0", resolver_port)).unwrap();
    let resolver_at = format!("{host}:{resolver_port}");
    let answer = move |q: &[u8]| -> Vec<u8> {
        let mut at = 12;
        let mut name = Vec::new();
        while at < q.len() && q[at] != 0 {
            let len = usize::from(q[at]);
            name.push(String::from_utf8_lossy(&q[at + 1..at + 1 + len]).to_lowercase());
            at += 1 + len;
        }
        let qtype = u16::from_be_bytes([q[at + 1], q[at + 2]]);
        let count: u8 = match name.join(".").as_str() {
            "mcp.example" | "api.example" if qtype == 1 => 1,
            "big.example" if qtype == 1 => 64,
            _ => 0,
        };
        let mut r = q[..2].to_vec();
        r.extend_from_slice(&[0x81, 0x80, 0, 1, 0, count, 0, 0, 0, 0]);
        r.extend_from_slice(&q[12..at + 5]);
        for i in 0..count {
            r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            if count == 1 {
                r.extend_from_slice(&address.octets());
            } else {
                r.extend_from_slice(&[192, 0, 2, i]);
            }
        }
        r
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = common::recv_from(&resolver, &mut buf) {
            let mut r = answer(&buf[..n]);
            if r.len() > 512 {
                // Truncated: the header and question, TC set, no answers.
                let question_end = 12 + buf[12..n].iter().position(|&b| b == 0).unwrap() + 5;
                r.truncate(question_end);
                r[2] |= 0x02;
                r[6..8].copy_from_slice(&[0, 0]);
            }
            let _ = resolver.send_to(&r, from);
        }
    });
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        for c in resolver_tcp.incoming() {
            let Ok(mut c) = c else { continue };
            std::thread::spawn(move || {
                let mut len = [0u8; 2];
                while c.read_exact(&mut len).is_ok() {
                    let mut q = vec![0u8; usize::from(u16::from_be_bytes(len))];
                    if c.read_exact(&mut q).is_err() {
                        return;
                    }
                    let r = answer(&q);
                    let _ = c.write_all(&[&u16::try_from(r.len()).unwrap().to_be_bytes()[..], &r].concat());
                }
            });
        }
    });
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("mcp-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_DNS", std::ffi::OsStr::new(&resolver_at)),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let agent = |name: &str, args: &[String]| -> String {
        let dir = TempDir::new(&format!("mcp-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        let command: Vec<String> = ["bin/testguest", "confined", "cat", "/etc/resolv.conf"]
            .iter()
            .map(|s| (*s).to_string())
            .chain(args.iter().cloned())
            .map(|a| format!("{a:?}"))
            .collect();
        std::fs::write(
            dir.join("agent.json"),
            format!(
                r#"{{"name":"{name}","run":{{"command":[{}]}}}}"#,
                command.join(",")
            ),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/mcp-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        tag
    };
    let to = |p: u16| format!("{host}:{p}");
    let m = agent(
        "m",
        &[
            // Before its host is resolved: no address of it is known, so none is reached.
            "unreach".into(),
            to(mcp_port),
            "reach".into(),
            format!("mcp.example:{mcp_port}"),
            // Its port alone, at the server's addresses.
            "unreach".into(),
            to(other),
            "write".into(),
            "/etc/resolv.conf".into(),
            "call".into(),
            "socket-raw".into(),
            "unshare-net".into(),
            // A name its grant does not name: refused, though the resolver upstream holds it,
            // by UDP and by TCP; its server's name, by TCP too.
            "dnsprobe".into(),
            "resolver".into(),
            "dnsask".into(),
            "tcp,api.example".into(),
            "tcp,mcp.example".into(),
        ],
    );
    let n = agent("n", &["unreach".into(), to(mcp_port)]);
    // g's network grants it any name: the host then asks any of its resolver for the
    // microVM, and only the agents' resolver keeps m to its server's.
    // An answer too long for UDP comes truncated by UDP, as the resolver sent it, and
    // whole by TCP.
    let g = agent(
        "g",
        &[
            "resolve".into(),
            "api.example".into(),
            "dnsask".into(),
            "udp,big.example".into(),
            "tcp,big.example".into(),
        ],
    );
    let ctx = context("mcp-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT m FROM {m}\nAGENT n FROM {n}\nAGENT g FROM {g}\n\
             MCP web FROM http://mcp.example:{mcp_port}/sse FOR m\n\
             NETWORK --dns names\nCONNECT g WITH g ON names\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "mcp:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "mcp:1", "await", "confined-ready", "3"]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let said = |who: &str| -> Vec<String> {
        let prefix = format!("[agent {who}] ");
        ran.stderr
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix).map(str::to_string))
            .collect()
    };
    let gateway = said("m")
        .iter()
        .find_map(|l| {
            l.strip_prefix("confined cat /etc/resolv.conf: nameserver ")
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("m has no resolver of its own:\n{}", ran.stderr));
    for (who, want) in [
        // The network process has learned no address of mcp.example yet: refused.
        ("m", format!("confined unreach {}: errno 111", to(mcp_port))),
        ("m", format!("confined reach mcp.example:{mcp_port}: ok")),
        ("m", format!("confined unreach {}: timeout", to(other))),
        // Nothing an agent does widens its grant: its resolver file is read-only, it can
        // open no raw socket to send as another, and it can leave its namespace for none.
        ("m", "confined write /etc/resolv.conf: errno 30".to_string()),
        ("m", "confined call socket-raw: errno 1".to_string()),
        ("m", "confined call unshare-net: errno 1".to_string()),
        (
            "m",
            format!("confined dnsprobe {gateway}: from {gateway}:53 rcode 5 answers 0"),
        ),
        // g, granted any name, resolves the one m may not: the host asks it for the
        // microVM, and the agents' resolver alone keeps m from it.
        ("g", "confined resolve api.example: 0,0".to_string()),
        // By TCP as by UDP: m's own server's name answered, the other refused.
        (
            "m",
            "confined dnsask tcp mcp.example: rcode 0 tc 0 answers 1".to_string(),
        ),
        (
            "m",
            "confined dnsask tcp api.example: rcode 5 tc 0 answers 0".to_string(),
        ),
        // An answer too long for UDP: truncated, as the resolver sent it, then whole by
        // TCP (RFC 7766).
        (
            "g",
            "confined dnsask udp big.example: rcode 0 tc 1 answers 0".to_string(),
        ),
        (
            "g",
            "confined dnsask tcp big.example: rcode 0 tc 0 answers 64".to_string(),
        ),
        // Not named: no link, and Landlock lets it connect nowhere.
        ("n", format!("confined unreach {}: errno 13", to(mcp_port))),
    ] {
        assert!(
            said(who).contains(&want),
            "no {want:?} from {who} in:\n{}",
            ran.stderr
        );
    }
}

/// `SKILL --from=<agent>` takes, at build time, a skill the agent's OSI config lists into
/// another's grants (AGENTFILE_ARCH.md §12 answer 12, D54): a declaration of what the agent
/// brings, not one agent reading another, so a path its config does not list, and an agent
/// with no config, are refused.
#[test]
fn a_skill_an_agent_lists_is_taken_from_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("skill-from-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("skill-from-agent");
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    std::fs::write(
        dir.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: Reads PDFs.\n---\nRead them.\n",
    )
    .unwrap();
    std::fs::write(dir.join("secret.txt"), "main's own\n").unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"main","skills":["skills/pdf"]}"#,
    )
    .unwrap();
    let tag = format!("127.0.0.1:{port}/team/skilled:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &tag]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context("skill-from-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("other")).unwrap();
    std::fs::write(ctx.join("other/run"), "other\n").unwrap();
    let build = |agentfile: String| {
        std::fs::write(ctx.join("Agentfile"), agentfile).unwrap();
        let out = TempDir::new("skill-from-out");
        let built = shards(&[
            "build",
            "-o",
            out.join("root").to_str().unwrap(),
            ctx.to_str().unwrap(),
        ]);
        (built, out)
    };
    let base = format!("FROM {image}\nAGENT main FROM {tag}\nAGENT other FROM ./other\n");
    let (built, out) = build(format!("{base}SKILL --from=main skills/pdf FOR other\n"));
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let root = out.join("root");
    assert_eq!(
        std::fs::read_to_string(root.join("agents/other.d/skills/pdf/SKILL.md")).unwrap(),
        "---\nname: pdf\ndescription: Reads PDFs.\n---\nRead them.\n"
    );
    assert!(
        !root.join("agents/other.d/skills/secret.txt").exists(),
        "only the skill is taken"
    );
    // What its config does not list is no skill of its to give.
    let (refused, _) = build(format!("{base}SKILL --from=main secret.txt FOR other\n"));
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("SKILL --from=main secret.txt: its config lists no such skill (it lists skills/pdf)"),
        "{}",
        refused.stderr
    );
    // An agent from a path has no config to list skills.
    let (refused, _) = build(format!("{base}SKILL --from=other run FOR main\n"));
    assert_ne!(refused.status, Some(0));
    assert!(
        refused
            .stderr
            .contains("SKILL --from=other: it has no OSI config listing its skills"),
        "{}",
        refused.stderr
    );
}

/// A file outside an agent's domain owned by its user, which the microVM would run it as,
/// fails the build (D55, AGENTFILE_ARCH.md §9.2): `--chown` to the agent's uid on /etc.
#[test]
fn a_domains_user_owns_nothing_past_its_domain() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("owners-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context("owners-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("agent")).unwrap();
    std::fs::write(ctx.join("agent/run"), "run\n").unwrap();
    std::fs::write(ctx.join("motd"), "hello\n").unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\nCOPY --chown=200000:0 motd /etc/motd\n"),
    )
    .unwrap();
    let out = TempDir::new("owners-out");
    let built = shards(&[
        "build",
        "-o",
        out.join("root").to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_ne!(built.status, Some(0), "{}", built.stdout);
    assert!(
        built.stderr.contains(
            "/etc/motd: owned by the agent main's user (200000), in the system, outside its domain"
        ),
        "{}",
        built.stderr
    );
}

/// What the run's own command opens to anyone is no agent's to reach (D59, AGENTFILE_ARCH.md
/// §9.7, §9.8; default deny): a pathname Unix socket of mode 0777, though the agent sees the
/// system it lies in, and an abstract one, in another network namespace.
#[test]
fn an_agent_reaches_no_socket_of_the_runs_own() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("unix-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("unix-agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"x","run":{"command":["bin/testguest","confined","unix","/work/escape.sock","abstract","shards-escape"]}}"#,
    )
    .unwrap();
    let tag = format!("127.0.0.1:{port}/team/unix-x:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &tag]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context("unix-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT x FROM {tag}\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "unix:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&[
        "run",
        "--rm",
        "unix:1",
        "unix-listen",
        "/work/escape.sock",
        "shards-escape",
        "confined-ready",
        "1",
    ]);
    assert_eq!(ran.status, Some(0), "{}{}", ran.stdout, ran.stderr);
    let said: Vec<&str> = ran
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[agent x] "))
        .collect();
    for line in &said {
        eprintln!("{line}");
    }
    // Each tried while the sockets were there: the agent's unix try ended, and its abstract
    // one began, after the run bound both.
    let all = format!("{}{}", ran.stdout, ran.stderr);
    let time = |prefix: &str| -> u128 {
        all.lines()
            .find_map(|l| l.trim_start_matches("[agent x] ").strip_prefix(prefix))
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {prefix:?} in\n{all}"))
    };
    assert!(time("unix-listen bound ") < time("confined unix tried "), "{all}");
    assert!(
        said.contains(&"confined unix /work/escape.sock: errno 2"),
        "{all}"
    );
    assert!(
        said.contains(&"confined abstract shards-escape: errno 111"),
        "{all}"
    );
}

/// Agents filling their memory, scratch included, end whole and never take the run's own
/// command's (D59): x alone, at the limit its config asks (`asks.memory`, 64 MiB); y, asking
/// none, at what the domains together may take, the memory the microVM has as they start
/// less what the workload's `-m` still promises it, chosen by init over z, which holds more
/// resident memory and no scratch, and which the kernel's choice would end first.
#[test]
fn agents_out_of_memory_end_whole_and_spare_the_run() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("oom-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let agent = |name: &str, verb: &str, asks: &str| {
        let dir = TempDir::new(&format!("oom-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined","{verb}"]}}{asks}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/oom-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        format!("AGENT {name} FROM {tag}\n")
    };
    let x = agent("x", "fill", r#","asks":{"memory":67108864}"#);
    let yz = agent("y", "fill", "") + &agent("z", "hold", "");
    let run = |tag: &str, agents: &str| {
        let ctx = context(&format!("oom-ctx-{tag}"), &format!("FROM {image}\n"));
        std::fs::write(ctx.join("Agentfile"), format!("FROM {image}\n{agents}")).unwrap();
        let built = shards(&["build", "-t", tag, ctx.to_str().unwrap()]);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        let ran = shards(&["run", "--rm", "-m", "128m", tag, "outlive", "64"]);
        let all = format!("{}{}", ran.stdout, ran.stderr);
        assert_eq!(ran.status, Some(0), "{all}");
        // The run's command outlived them, and every domain the kernel ended lost its
        // holder with it: those `held` left are of domains whose writes failed.
        let outlived = all
            .lines()
            .find_map(|l| l.strip_prefix("outlived "))
            .unwrap_or_else(|| panic!("the run's command did not outlive its agents:\n{all}"))
            .to_string();
        let n = |k: &str| -> usize {
            outlived
                .split(' ')
                .find_map(|f| f.strip_prefix(k))
                .and_then(|v| v.parse().ok())
                .unwrap()
        };
        assert_eq!(n("held="), n("stopped="), "{all}");
        let filled = |a: &str| {
            all.lines()
                .filter_map(|l| l.strip_prefix(&format!("[agent {a}] confined filled ")))
                .filter_map(|v| v.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        };
        eprintln!(
            "{tag}: x {} y {} z {}; {outlived}",
            filled("x"),
            filled("y"),
            filled("z")
        );
        (filled("x"), n("innocent="), all)
    };
    // x ends at its own limit, where its memory is its resident 9 MiB and its scratch.
    let (x_filled, _, all) = run("oom-x:1", &x);
    assert!(x_filled > 32 && x_filled < 64, "x filled {x_filled} MiB:\n{all}");
    let (_, innocent, all) = run("oom-yz:1", &yz);
    assert_eq!(innocent, 1, "z, holding no scratch, was ended:\n{all}");
    assert!(
        all.lines()
            .any(|l| l.starts_with("[agent y] shards-init: ended: the agents' memory")),
        "init did not end y:\n{all}"
    );
}

/// Unix sockets an Agentfile grants (`NETWORK --protocol=unix`, `CONNECT
/// --port=unix:<name>`, D59), by name and one way: b, receiving tools and secret, makes
/// both; a, granted tools, connects to it and cannot make one there, and secret does not
/// exist for it; c, granted secret alone, likewise; d, granted none, sees neither.
#[test]
fn agents_reach_only_the_unix_sockets_granted() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("sock-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let tools = "/run/networks/n/tools/sock";
    let secret = "/run/networks/n/secret/sock";
    let mut agents = String::new();
    for (name, verbs) in [
        ("b", format!("\"unix-serve\",\"{tools}\",\"{secret}\"")),
        (
            "a",
            format!("\"unix\",\"{tools}\",\"{secret}\",\"unix-serve\",\"/run/networks/n/tools/a\""),
        ),
        ("c", format!("\"unix\",\"{secret}\",\"{tools}\"")),
        ("d", format!("\"unix\",\"{tools}\"")),
    ] {
        let dir = TempDir::new(&format!("sock-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/sock-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("sock-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}\
             NETWORK --protocol=unix --ingress=unix:tools --ingress=unix:secret n\n\
             CONNECT --port=unix:tools a TO b ON n\n\
             CONNECT --port=unix:secret c TO b ON n\n\
             CONNECT d WITH d ON n\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "sock:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "sock:1", "await", "confined-ready", "4"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    let said = |agent: &str, line: &str| {
        let want = format!("[agent {agent}] confined {line}");
        assert!(all.lines().any(|l| l == want), "no {want:?} in\n{all}");
    };
    said("b", &format!("unix-serve {tools}: ok"));
    said("b", &format!("unix-serve {secret}: ok"));
    said("a", &format!("unix {tools}: ok"));
    said("a", &format!("unix {secret}: errno 2"));
    // EROFS: it connects, and makes nothing there.
    said("a", "unix-serve /run/networks/n/tools/a: errno 30");
    said("c", &format!("unix {secret}: ok"));
    said("c", &format!("unix {tools}: errno 2"));
    said("d", &format!("unix {tools}: errno 2"));
}

/// One agent opening new flows without end takes no other agent's connections (D59): a,
/// granted UDP 7000 to b, sends from a new port each time; c, granted TCP 7001 to b,
/// connects meanwhile, and makes every connection.
#[test]
fn an_agents_flood_of_flows_takes_no_others() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("flood-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let flood = std::env::var("SHARDS_FLOOD_SECS").unwrap_or_else(|_| "10".to_string());
    let mut agents = String::new();
    for (name, verbs) in [
        ("b", "\"listen\",\"7001\"".to_string()),
        ("a", format!("\"udpflood\",\"b:7000,{flood}\"")),
        // Each given 3 s, past a SYN's first retransmission (1 s): the flood's packets may
        // cost one connection its first SYN, which the table taking it would cost them all.
        ("c", "\"pause\",\"2\",\"reachmany\",\"b:7001,200,3\"".to_string()),
    ] {
        let dir = TempDir::new(&format!("flood-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/flood-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("flood-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}\
             NETWORK --ingress=7000/udp --ingress=7001 n\n\
             CONNECT --port=7000/udp a TO b ON n\n\
             CONNECT --port=7001 c TO b ON n\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "flood:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "flood:1", "await", "confined-ready", "3"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    for l in all
        .lines()
        .filter(|l| l.contains("udpflood") || l.contains("reachmany"))
    {
        eprintln!("{l}");
    }
    assert!(
        all.lines()
            .any(|l| l.starts_with("[agent c] confined reachmany b:7001: 200 of 200")),
        "c's connections were taken:\n{all}"
    );
}

/// One agent's flows that conntrack may not evict take no other agent's (D61): a, granted
/// TCP 7002 to x, opens connections from several threads and holds them, each an assured
/// entry, until its table is full, before c begins (past full, a few more come as the
/// table's unanswered entries go: 64 more in 9 s here, none in CI's 2 s); c, meanwhile, connects
/// to b and to x itself, and makes every connection, each given 3 s, past a SYN's first
/// retransmission (1 s), so that what a full table refuses fails and what a busy listener
/// drops once does not. a's entries fill a's gate's table alone; x's gate tracks none of
/// what is opened to it.
#[test]
fn an_agents_assured_flows_take_no_others() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("assured-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let flood: u64 = std::env::var("SHARDS_FLOOD_SECS").map_or(10, |s| s.parse().unwrap());
    let half = flood / 2;
    let mut agents = String::new();
    for (name, verbs) in [
        ("b", "\"listen\",\"7001\"".to_string()),
        ("x", "\"listen\",\"7002\"".to_string()),
        ("a", format!("\"tcpflood\",\"x:7002,{flood}\"")),
        (
            "c",
            format!("\"pause\",\"{half}\",\"reachmany\",\"b:7001,50,3\",\"reachmany\",\"x:7002,50,3\""),
        ),
    ] {
        let dir = TempDir::new(&format!("assured-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/assured-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("assured-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}\
             NETWORK --ingress=7001-7002 n\n\
             CONNECT --port=7002 a TO x ON n\n\
             CONNECT --port=7001 c TO b ON n\n\
             CONNECT --port=7002 c TO x ON n\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "assured:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "assured:1", "await", "confined-ready", "4"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    for l in all
        .lines()
        .filter(|l| l.contains("tcpflood") || l.contains("reachmany"))
    {
        eprintln!("{l}");
    }
    // a filled its table, each connection an entry that stays.
    let flooded = all
        .lines()
        .find_map(|l| l.strip_prefix("[agent a] confined tcpflood x:7002: "))
        .unwrap_or_else(|| panic!("a did not flood:\n{all}"));
    let (made, rest) = flooded.split_once(" made, table ").unwrap();
    let (table, full) = rest.split_once(", full at ").unwrap();
    let (made, table): (u64, u64) = (made.parse().unwrap(), table.parse().unwrap());
    assert!(made >= table, "a made {made}, the table holds {table}:\n{all}");
    // The table was full before c began, else c's connections prove nothing.
    let full: u64 = full
        .strip_suffix(" ms")
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("a never filled the table:\n{all}"));
    assert!(
        full < half * 1000,
        "the table was full at {full} ms, c began at {half} s:\n{all}"
    );
    for target in ["b:7001", "x:7002"] {
        assert!(
            all.lines()
                .any(|l| l.starts_with(&format!("[agent c] confined reachmany {target}: 50 of 50"))),
            "c's connections to {target} were taken:\n{all}"
        );
    }
}

/// An agent that accepts the ports its connections would come from still connects (D61):
/// c, to which b may open 32768 to 60999 (the kernel's own range for them, net/ipv4/
/// af_inet.c), reaches x, its connections taking ports it does not accept, whose answers
/// its gate tracks.
#[test]
fn an_agents_own_flows_take_no_port_it_accepts() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("ports-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut agents = String::new();
    for (name, verbs) in [
        ("x", "\"listen\",\"7002\""),
        ("b", "\"pause\",\"1\""),
        ("c", "\"reach\",\"x:7002\""),
    ] {
        let dir = TempDir::new(&format!("ports-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/ports-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("ports-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}\
             NETWORK --ingress=7002-60999 n\n\
             CONNECT --port=32768-60999 b TO c ON n\n\
             CONNECT --port=7002 c TO x ON n\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "ports:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "ports:1", "await", "confined-ready", "3"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    assert!(
        all.lines().any(|l| l == "[agent c] confined reach x:7002: ok"),
        "{all}"
    );
}

/// What an agent reaches past its grants (AGENTFILE_ARCH.md §9.10, D59; default deny):
/// d, granted TCP 7000 to b alone, and e, granted nothing, each try TCP and UDP to the
/// run's own command, listening on 7100 at every address of the microVM's; to the
/// switch's and init's ends of the uplink; and to the microVM's gateway; d to its own
/// network's gateway too, the switch's end of its link, DNS among it. Nothing answers:
/// each is dropped or has no route. d still reaches b.
#[test]
fn an_agent_reaches_nothing_past_its_grants() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("sweep-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    // Where the microVM is, from a run of its own: its address and gateway.
    let probe = shards(&["run", "--rm", &image, "sweep-target", "7100", "none", "0"]);
    let where_ = |all: &str| -> (String, String) {
        let w = all
            .lines()
            .find_map(|l| l.strip_prefix("where "))
            .unwrap_or_else(|| panic!("no address in\n{all}"))
            .to_string();
        let (a, g) = w.split_once(' ').unwrap();
        (a.to_string(), g.to_string())
    };
    let (own, gw) = where_(&format!("{}{}", probe.stdout, probe.stderr));
    let mut targets = Vec::new();
    for host in [own.as_str(), "169.254.77.1", "169.254.77.2", gw.as_str()] {
        targets.push(format!(
            "\"unreach\",\"{host}:7100\",\"udpask\",\"{host}:7100\",\"udpaskfrom\",\"7300,{host}:7100\""
        ));
    }
    let sweep = targets.join(",");
    // d's own network's gateway, the switch's end of its link: TCP and UDP, DNS among it.
    let on_link =
        "\"unreach\",\"NET1:7100\",\"udpask\",\"NET1:53\",\"udpask\",\"NET1:7100\",\"unreach\",\"NET1:53\"";
    let mut agents = String::new();
    for (name, verbs) in [
        ("b", "\"listen\",\"7000\"".to_string()),
        ("d", format!("\"reach\",\"b:7000\",{sweep},{on_link}")),
        ("e", sweep.clone()),
    ] {
        let dir = TempDir::new(&format!("sweep-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/sweep-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("sweep-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}\
             NETWORK --ingress=7000 n\n\
             NETWORK --ingress=7300/udp --egress=7400 front\n\
             EXPOSE 7300/udp AS ingress FOR front\nEXPOSE 7400 AS egress FOR front\n\
             CONNECT --port=7000 d TO b ON n\nCONNECT d WITH d ON front\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "sweep:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&[
        "run",
        "--rm",
        "sweep:1",
        "sweep-target",
        "7100",
        "confined-ready",
        "3",
    ]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    // The run swept the addresses the probe saw.
    assert_eq!(where_(&all), (own.clone(), gw.clone()), "{all}");
    assert!(
        all.lines().any(|l| l == "[agent d] confined reach b:7000: ok"),
        "{all}"
    );
    let mut tried = 0;
    for agent in ["d", "e"] {
        for line in all
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("[agent {agent}] confined ")))
        {
            eprintln!("{agent}: {line}");
            let Some((what, said)) = line.split_once(": ") else {
                continue;
            };
            if what.starts_with("unreach ") {
                tried += 1;
                // Dropped (timeout), no route (ENETUNREACH 101, EHOSTUNREACH 113), or
                // refused before it leaves (Landlock, EACCES 13, where no grant lets it
                // connect); a refusal by the far end (111) would be an answer.
                assert!(
                    ["timeout", "errno 101", "errno 113", "errno 13"].contains(&said),
                    "{agent} {line}\n{all}"
                );
            } else if what.starts_with("udpask ") || what.starts_with("udpaskfrom ") {
                tried += 1;
                assert!(
                    ["none", "errno 101", "errno 113"].contains(&said),
                    "{agent} {line}\n{all}"
                );
            }
        }
    }
    assert_eq!(tried, 3 * 8 + 4, "{all}");
}

/// A process an agent lets go as daemons are (a double fork and `setsid`) is ended with
/// the agent (AGENTFILE_ARCH.md §9.9): it stays in the agent's PID namespace, which the
/// kernel ends with its first process.
#[test]
fn an_agents_daemon_ends_with_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("daemon-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("daemon-agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
    std::fs::write(
        dir.join("agent.json"),
        r#"{"name":"x","run":{"command":["bin/testguest","confined","daemonize","pause","2","end"]}}"#,
    )
    .unwrap();
    let tag = format!("127.0.0.1:{port}/team/daemon-x:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &tag]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let ctx = context("daemon-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT x FROM {tag}\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "daemon:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "daemon:1", "vanish", "escaped"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert!(
        all.lines().any(|l| l == "[agent x] confined daemonize: done"),
        "{all}"
    );
    assert!(all.lines().any(|l| l == "vanish escaped: vanished"), "{all}");
    assert_eq!(ran.status, Some(0), "{all}");
}

/// No kernel channel §9.8 names joins two domains that no directive joined
/// (AGENTFILE_ARCH.md §9.8): a makes a System V shared memory segment and message queue,
/// a POSIX message queue and a file in /dev/shm, and opens each itself; b opens none of
/// them, and its signal to every process it may signal reaches no other domain's.
#[test]
fn no_kernel_channel_joins_two_agents() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("chan-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut agents = String::new();
    for (name, verbs) in [
        ("a", "\"ipc-make\",\"ipc-try\""),
        ("b", "\"pause\",\"2\",\"ipc-try\""),
    ] {
        let dir = TempDir::new(&format!("chan-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/chan-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("chan-ctx", &format!("FROM {image}\n"));
    std::fs::write(ctx.join("Agentfile"), format!("FROM {image}\n{agents}")).unwrap();
    let built = shards(&["build", "-t", "chan:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // Both still running once b has tried: its signal ended no one of a's.
    let ran = shards(&["run", "--rm", "chan:1", "await", "confined-ready", "2"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    let said = |line: String| assert!(all.lines().any(|l| l == line), "no {line:?} in\n{all}");
    for what in ["shm", "msg", "mq", "file"] {
        said(format!("[agent a] confined ipc-make {what}: ok"));
        said(format!("[agent a] confined ipc-try {what}: ok"));
        said(format!("[agent b] confined ipc-try {what}: errno 2"));
    }
    assert!(!all.lines().any(|l| l.starts_with("await timeout")), "{all}");
}

/// The in-VM server knows each agent by the socket it calls on, over mutual TLS 1.3
/// with the certificate issued for it (AGENTFILE_ARCH.md §5, §12 answer 18; D60): x and y
/// each ask their instance, as an MCP client asks, who they are, and are told. Each
/// instance runs least-privileged: a uid and gid of its own, its agent's group alone
/// beside them, no capability, `no_new_privs`, a seccomp filter, a network namespace of
/// its own.
#[test]
fn the_in_vm_server_knows_each_agent() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("server-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut agents = String::new();
    for name in ["x", "y"] {
        let dir = TempDir::new(&format!("server-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined","see","/run/shards","whoami"]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/server-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("server-ctx", &format!("FROM {image}\n"));
    std::fs::write(ctx.join("Agentfile"), format!("FROM {image}\n{agents}")).unwrap();
    let built = shards(&["build", "-t", "server:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&[
        "run",
        "--rm",
        "server:1",
        "inspect-servers",
        "confined-ready",
        "2",
    ]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    let servers: Vec<&str> = all.lines().filter_map(|l| l.strip_prefix("server ")).collect();
    assert_eq!(servers.len(), 2, "{all}");
    for (n, agent) in [(0u32, 200_000u32), (1, 200_001)] {
        let id = 200_000 + 4 * 1024 * 1024 + n;
        let want = format!(
            "uid={id},{id},{id},{id} gid={id},{id},{id},{id} groups={agent} capeff=0000000000000000 \
             capprm=0000000000000000 capbnd=0000000000000000 nnp=1 seccomp=2 links=lo rss_anon_kb="
        );
        let line = servers.iter().find(|s| s.starts_with(&want));
        assert!(line.is_some(), "no instance as {want:?} in\n{all}");
        eprintln!("instance {n}: {}", line.unwrap_or(&""));
    }
    for name in ["x", "y"] {
        let want = format!("[agent {name}] confined whoami: agent {name}");
        assert!(all.lines().any(|l| l == want), "no {want:?} in\n{all}");
        let sees = format!("[agent {name}] confined see /run/shards: ca.pem,cert.pem,key.pem,server.sock");
        assert!(all.lines().any(|l| l == sees), "no {sees:?} in\n{all}");
    }
}

/// Agents message one another through their server instances as the Agentfile grants,
/// and no further (§12 answer 18, D60; default deny): `CONNECT a TO b` lets a send b
/// requests and b answer them, not b send a its own; c, granted nothing, has no one.
#[test]
fn agents_message_one_another_as_granted() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("msg-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut agents = String::new();
    for (name, verbs) in [
        ("a", r#""srv-peers","srv-send","agent b|hi","srv-receive","30""#),
        (
            "b",
            r#""srv-peers","srv-receive","30","srv-answer","hello a","srv-send","agent a|unasked""#,
        ),
        ("c", r#""srv-peers","srv-send","agent b|x""#),
    ] {
        let dir = TempDir::new(&format!("msg-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined",{verbs}]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/msg-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("msg-ctx", &format!("FROM {image}\n"));
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\n{agents}NETWORK --ingress=7000 n\nCONNECT --port=7000 a TO b ON n\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "msg:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "msg:1", "await", "confined-ready", "3"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    for want in [
        r#"[agent a] confined srv-peers: [{"name":"agent b","send":true,"answer":false}]"#,
        r#"[agent b] confined srv-peers: [{"name":"agent a","send":false,"answer":true}]"#,
        "[agent c] confined srv-peers: []",
        r#"[agent a] confined srv-send agent b: ok {"id":1}"#,
        r#"[agent b] confined srv-receive: [{"from":"agent a","kind":"request","id":1,"text":"hi"}]"#,
        "[agent b] confined srv-answer: ok {}",
        r#"[agent a] confined srv-receive: [{"from":"agent b","kind":"answer","id":1,"text":"hello a"}]"#,
        "[agent b] confined srv-send agent a: refused agent b may not send agent a requests",
        "[agent c] confined srv-send agent b: refused agent c may not send agent b requests",
    ] {
        assert!(all.lines().any(|l| l == want), "no {want:?} in\n{all}");
    }
}

/// MCP servers are offered through the in-VM server to the agents in their scope, and to
/// no other (§4.4, §12 answer 5, D60): a remote one `FOR a` to a alone, by its URL; a
/// local one with no `FOR` to every agent, by where it lies.
#[test]
fn mcp_servers_are_offered_to_those_in_scope() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("offer-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut agents = String::new();
    for name in ["a", "b"] {
        let dir = TempDir::new(&format!("offer-agent-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(
            dir.join("agent.json"),
            format!(r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined","srv-mcp"]}}}}"#),
        )
        .unwrap();
        let tag = format!("127.0.0.1:{port}/team/offer-{name}:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", "agent", &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        agents.push_str(&format!("AGENT {name} FROM {tag}\n"));
    }
    let ctx = context("offer-ctx", &format!("FROM {image}\n"));
    std::fs::create_dir_all(ctx.join("tools")).unwrap();
    std::fs::write(ctx.join("tools/server.py"), "print('tools')\n").unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\n{agents}MCP web FROM https://mcp.example.com/sse FOR a\nMCP tools FROM ./tools\n"
        ),
    )
    .unwrap();
    let built = shards(&["build", "-t", "offer:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "offer:1", "await", "confined-ready", "2"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    let local = r#"{"name":"tools","remote":false,"dir":"/mcp/tools","command":[]}"#;
    for want in [
        format!(
            r#"[agent a] confined srv-mcp: [{{"name":"web","remote":true,"url":"https://mcp.example.com/sse"}},{local}]"#
        ),
        format!("[agent b] confined srv-mcp: [{local}]"),
    ] {
        assert!(all.lines().any(|l| l == want), "no {want:?} in\n{all}");
    }
}

/// The test image behind an index of two platforms: this host's, and another
/// architecture's whose one layer holds `/arch`, its name. The index, its blobs, and that
/// layer's digest.
fn two_platform_image() -> (Vec<u8>, Vec<Vec<u8>>, String) {
    let (ours, mut blobs) = common::test_image();
    let (arch, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let layer = common::tar(&[("arch", 0o644, 0, Some(other.as_bytes()))]);
    let config = format!(
        r#"{{"architecture":"{other}","os":"linux","config":{{"Env":["PATH=/bin"]}},"rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
        common::sha256_digest(&layer)
    )
    .into_bytes();
    let theirs = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        common::sha256_digest(&config),
        config.len(),
        common::sha256_digest(&layer),
        layer.len()
    )
    .into_bytes();
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{other}","os":"linux"}}}}]}}"#,
        common::sha256_digest(&ours),
        ours.len(),
        common::sha256_digest(&theirs),
        theirs.len(),
    )
    .into_bytes();
    let layer_digest = common::sha256_digest(&layer);
    blobs.push(ours);
    blobs.extend([config, layer, theirs]);
    (index, blobs, layer_digest)
}

/// `--platform` for a platform this host's microVMs do not run (D74): its bases are that
/// platform's, fetched without changing what a name in the store holds; steps for the
/// build platform run (`FROM --platform=$BUILDPLATFORM`), and what they made is copied into
/// the image, which is that platform's; a step for that platform is refused at its turn,
/// before anything boots, in words that say what to do; `local` is this host's platform.
#[test]
fn builds_for_another_platform_run_only_the_build_platforms_steps() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs, their_layer) = two_platform_image();
    let (port, _) = common::registry(index, blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = TempDir::new("foreign-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let (arch, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let ctx = context(
        "foreign-ctx",
        &format!(
            "FROM --platform=$BUILDPLATFORM {image} AS build\n\
             RUN [\"/bin/testguest\", \"fs\", \"write:/work/out=built\"]\n\
             FROM {image}\n\
             COPY --from=build /work/out /out\n"
        ),
    );
    let out = TempDir::new("foreign-out");
    let layout = out.join("layout");
    let built = shards(&[
        "build",
        "--platform",
        &format!("linux/{other}"),
        "-o",
        &format!("type=oci,dest={},tar=false", layout.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let desc = &top["manifests"][0];
    assert_eq!(desc["platform"]["architecture"], other, "{top}");
    let blob =
        |d: &str| std::fs::read(layout.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&blob(desc["digest"].as_str().unwrap())).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&blob(manifest["config"]["digest"].as_str().unwrap())).unwrap();
    assert_eq!(config["architecture"], other, "{config}");
    // The other platform's base layer, then what the build platform's step made.
    let layers = manifest["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2, "{manifest}");
    assert_eq!(layers[0]["digest"], their_layer.as_str(), "{manifest}");
    // Gzipped, as BuildKit writes a layer the build made (D75).
    assert_eq!(
        layers[1]["mediaType"], "application/vnd.oci.image.layer.v1.tar+gzip",
        "{manifest}"
    );
    let mut tar = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(&blob(layers[1]["digest"].as_str().unwrap())[..]),
        &mut tar,
    )
    .unwrap();
    let copied = tar_entries(&tar);
    assert!(
        copied
            .iter()
            .any(|(h, data)| h.name == b"out" && data == b"built"),
        "{manifest}"
    );
    // Stored, the image is the other platform's; the base's name holds this host's.
    let stored = shards(&[
        "build",
        "--platform",
        &format!("linux/{other}"),
        "-t",
        "foreign:1",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(stored.status, Some(0), "{}", stored.stderr);
    let inspect = |name: &str| {
        let r = shards(&["image", "inspect", "--format", "{{.Architecture}}", name]);
        assert_eq!(r.status, Some(0), "{}", r.stderr);
        r.stdout.trim().to_string()
    };
    assert_eq!(inspect("foreign:1"), other);
    assert_eq!(inspect(&image), arch);

    let stepped = context(
        "foreign-step",
        &format!("FROM {image}\nRUN [\"/bin/testguest\", \"exit\", \"0\"]\n"),
    );
    let refused = shards(&[
        "build",
        "--platform",
        &format!("linux/{other}"),
        stepped.to_str().unwrap(),
    ]);
    assert_eq!(refused.status, Some(1), "{}", refused.stderr);
    assert!(
        refused.stderr.contains(&format!(
            "shards runs steps for linux/{arch} only, the platform its microVMs run, not linux/{other}"
        )),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("FROM --platform=$BUILDPLATFORM"),
        "{}",
        refused.stderr
    );

    let local = shards(&[
        "build",
        "--platform",
        "local",
        "-t",
        "local:1",
        stepped.to_str().unwrap(),
    ]);
    assert_eq!(local.status, Some(0), "{}", local.stderr);
    assert_eq!(inspect("local:1"), arch);
}

/// Layers compressed as BuildKit's exporters compress them (D75): those a build makes
/// gzipped by default, each its DiffID once decompressed, a base's kept as it came;
/// `compression=uncompressed`; `force-compression` writing the base's too;
/// `compression-level` (9: the gzip header's XFL 2, as Go writes it); eStargz and zstd;
/// a type BuildKit does not know refused in its words. That the bytes are Go's own is
/// crates/flate's oracle.
#[test]
fn layers_are_compressed_as_buildkit_compresses_them() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("compress-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "compress-ctx",
        &format!(
            "FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/work/made=some bytes made by a step\"]\n"
        ),
    );
    let out = TempDir::new("compress-out");
    // The layout's manifest's layers: each media type and its blob.
    let layers_of = |opts: &str| -> Vec<(String, Vec<u8>)> {
        let dir = out.join(format!("layout-{}", opts.len()));
        let built = shards(&[
            "build",
            "-o",
            &format!("type=oci,dest={},tar=false{opts}", dir.display()),
            ctx.to_str().unwrap(),
        ]);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        let blob =
            |d: &str| std::fs::read(dir.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
        let top: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&blob(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
        manifest["layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["mediaType"].as_str().unwrap().to_string(),
                    blob(l["digest"].as_str().unwrap()),
                )
            })
            .collect()
    };
    const TAR: &str = "application/vnd.oci.image.layer.v1.tar";
    const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    let gunzip = |b: &[u8]| {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(b), &mut out).unwrap();
        out
    };

    // The base's layer (the test image's, uncompressed) as it came; the step's gzipped,
    // at Go's default level (XFL 0, OS 255).
    let default = layers_of("");
    assert_eq!(default.len(), 2);
    assert_eq!(default[0].0, TAR);
    assert_eq!(default[1].0, GZIP);
    assert_eq!(&default[1].1[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    let made = gunzip(&default[1].1);
    assert!(
        tar_entries(&made)
            .iter()
            .any(|(h, d)| h.name == b"work/made" && d == b"some bytes made by a step")
    );
    let level9 = layers_of(",compression-level=9");
    assert_eq!(level9[1].1[8], 2, "XFL of level 9");
    assert_eq!(gunzip(&level9[1].1), made);

    let plain = layers_of(",compression=uncompressed");
    assert_eq!((plain[0].0.as_str(), plain[1].0.as_str()), (TAR, TAR));
    assert_eq!(plain[1].1, made);

    let forced = layers_of(",force-compression=true");
    assert_eq!((forced[0].0.as_str(), forced[1].0.as_str()), (GZIP, GZIP));
    assert_eq!(gunzip(&forced[0].1), default[0].1);

    // eStargz (D82): the step's layer an eStargz blob, its descriptor naming its TOC's
    // digest and its tar's size; the config's DiffID that tar's, the TOC entry's too; the
    // footer naming where the TOC starts; the base kept, unless forced.
    let esgz = |opts: &str| -> (serde_json::Value, serde_json::Value, std::path::PathBuf) {
        let dir = out.join(format!("esgz-{}", opts.len()));
        let built = shards(&[
            "build",
            "-o",
            &format!(
                "type=oci,dest={},tar=false,compression=estargz{opts}",
                dir.display()
            ),
            ctx.to_str().unwrap(),
        ]);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        let read =
            |d: &str| std::fs::read(dir.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
        let top: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&read(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&read(manifest["config"]["digest"].as_str().unwrap())).unwrap();
        (manifest, config, dir)
    };
    let all_members = |b: &[u8]| {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(b), &mut out).unwrap();
        out
    };
    let sha = |b: &[u8]| {
        use sha2::Digest as _;
        let hex: String = sha2::Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect();
        format!("sha256:{hex}")
    };
    let (manifest, config, dir) = esgz("");
    let layers = manifest["layers"].as_array().unwrap();
    assert_eq!(layers[0]["mediaType"], TAR);
    assert!(layers[0]["annotations"].is_null(), "{manifest}");
    let step = &layers[1];
    assert_eq!(step["mediaType"], GZIP);
    let blob = std::fs::read(
        dir.join("blobs/sha256")
            .join(step["digest"].as_str().unwrap().trim_start_matches("sha256:")),
    )
    .unwrap();
    let tar = all_members(&blob);
    assert_eq!(config["rootfs"]["diff_ids"][1], sha(&tar).as_str());
    assert_eq!(
        step["annotations"]["io.containers.estargz.uncompressed-size"],
        tar.len().to_string().as_str()
    );
    // The footer: an empty gzip member whose extra field says where the TOC starts.
    let footer = &blob[blob.len() - 51..];
    // XLEN 26: `SG`, the subfield's length 22, then the offset and `STARGZ`.
    assert_eq!(&footer[10..16], &[26, 0, b'S', b'G', 22, 0]);
    let at = u64::from_str_radix(std::str::from_utf8(&footer[16..32]).unwrap(), 16).unwrap();
    assert_eq!(&footer[32..38], b"STARGZ");
    let toc_tar = all_members(&blob[at as usize..blob.len() - 51]);
    let (h, toc) = tar_entries(&toc_tar).into_iter().next().unwrap();
    assert_eq!(h.name, b"stargz.index.json");
    assert_eq!(
        step["annotations"]["containerd.io/snapshot/stargz/toc.digest"],
        sha(&toc).as_str()
    );
    let toc: serde_json::Value = serde_json::from_slice(&toc).unwrap();
    let made_entry = toc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "work/made")
        .unwrap_or_else(|| panic!("{toc}"));
    assert_eq!(made_entry["type"], "reg");
    assert_eq!(made_entry["digest"], sha(b"some bytes made by a step").as_str());
    assert!(
        tar_entries(&tar)
            .iter()
            .any(|(h, d)| h.name == b"work/made" && d == b"some bytes made by a step")
    );
    // zstd (D84): the step's layer klauspost's zstd of its tar, at the default level and
    // at one asked for; the base kept.
    let unzstd = |b: &[u8]| {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut ruzstd::decoding::StreamingDecoder::new(b).unwrap(), &mut out)
            .unwrap();
        out
    };
    for opts in [",compression=zstd", ",compression=zstd,compression-level=19"] {
        let z = layers_of(opts);
        assert_eq!(z[0].0, TAR, "{opts}");
        assert_eq!(z[1].0, "application/vnd.oci.image.layer.v1.tar+zstd", "{opts}");
        assert_eq!(unzstd(&z[1].1), made, "{opts}");
    }
    // Each output its own compression (D85): one build, a zstd layout and a gzip archive,
    // each holding the step's layer as it asked, the same tar in both.
    let both_dir = out.join("both-zstd");
    let both_tar = out.join("both-gzip.tar");
    let both = shards(&[
        "build",
        "-o",
        &format!("type=oci,dest={},tar=false,compression=zstd", both_dir.display()),
        "-o",
        &format!("type=oci,dest={}", both_tar.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(both.status, Some(0), "{}", both.stderr);
    let read_dir = |d: &str| {
        std::fs::read(
            both_dir
                .join("blobs/sha256")
                .join(d.trim_start_matches("sha256:")),
        )
        .unwrap()
    };
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(both_dir.join("index.json")).unwrap()).unwrap();
    let zm: serde_json::Value =
        serde_json::from_slice(&read_dir(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
    assert_eq!(
        zm["layers"][1]["mediaType"],
        "application/vnd.oci.image.layer.v1.tar+zstd"
    );
    assert_eq!(
        unzstd(&read_dir(zm["layers"][1]["digest"].as_str().unwrap())),
        made
    );
    let archive = std::fs::read(&both_tar).unwrap();
    let files: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = tar_entries(&archive)
        .into_iter()
        .map(|(h, d)| (h.name, d))
        .collect();
    let blob_of =
        |d: &str| files[format!("blobs/sha256/{}", d.trim_start_matches("sha256:")).as_bytes()].clone();
    let top: serde_json::Value = serde_json::from_slice(&files[b"index.json".as_slice()]).unwrap();
    let gm: serde_json::Value =
        serde_json::from_slice(&blob_of(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
    assert_eq!(gm["layers"][1]["mediaType"], GZIP);
    assert_eq!(
        gunzip(&blob_of(gm["layers"][1]["digest"].as_str().unwrap())),
        made
    );
    let (forced, _, _) = esgz(",force-compression=true");
    assert!(
        forced["layers"][0]["annotations"]["containerd.io/snapshot/stargz/toc.digest"].is_string(),
        "{forced}"
    );

    for (opts, words) in [
        ("compression=lz4", "unsupported compression type lz4"),
        (
            "compression-level=high",
            "non-integer value high specified for compression-level",
        ),
    ] {
        let refused = shards(&[
            "build",
            "-o",
            &format!("type=oci,dest={},{opts}", out.join("refused.tar").display()),
            ctx.to_str().unwrap(),
        ]);
        assert_eq!(refused.status, Some(1), "{}", refused.stderr);
        assert!(refused.stderr.contains(words), "{opts}: {}", refused.stderr);
    }
}

/// `rewrite-timestamp` (D76): with SOURCE_DATE_EPOCH, every layer past the base's own is
/// read and written again by Go's archive/tar, each time past the epoch set to it, its
/// descriptor annotated with the epoch, the config's DiffID its own; the base's layer is
/// left as it came; without an epoch, BuildKit's warning, and nothing rewritten.
#[test]
fn rewrite_timestamp_rewrites_the_builds_layers_as_buildkit_does() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("rewrite-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "rewrite-ctx",
        &format!("FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/work/made=made now\"]\n"),
    );
    let out = TempDir::new("rewrite-out");
    let build = |dir: &std::path::Path, epoch: Option<&str>| {
        let mut args = vec!["build".to_string()];
        if let Some(e) = epoch {
            args.extend(["--build-arg".into(), format!("SOURCE_DATE_EPOCH={e}")]);
        }
        args.extend([
            "-o".into(),
            format!("type=oci,dest={},tar=false,rewrite-timestamp=true", dir.display()),
            ctx.to_str().unwrap().into(),
        ]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let built = shards(&args);
        assert_eq!(built.status, Some(0), "{}", built.stderr);
        let blob =
            |d: &str| std::fs::read(dir.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
        let top: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&blob(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&blob(manifest["config"]["digest"].as_str().unwrap())).unwrap();
        let layers: Vec<(serde_json::Value, Vec<u8>)> = manifest["layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| (l.clone(), blob(l["digest"].as_str().unwrap())))
            .collect();
        (built.stderr, config, layers)
    };
    let gunzip = |b: &[u8]| {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(b), &mut out).unwrap();
        out
    };
    let sha = |b: &[u8]| {
        use sha2::Digest as _;
        let hex: String = sha2::Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect();
        format!("sha256:{hex}")
    };

    let (_, config, layers) = build(&out.join("at-1000"), Some("1000"));
    // The base's layer as it came: not annotated.
    assert!(layers[0].0.get("annotations").is_none(), "{:?}", layers[0].0);
    let (desc, blob) = &layers[1];
    assert_eq!(
        desc["annotations"]["buildkit/rewritten-timestamp"], "1000",
        "{desc}"
    );
    let tar = gunzip(blob);
    assert_eq!(config["rootfs"]["diff_ids"][1], sha(&tar).as_str(), "{config}");
    let entries = tar_entries(&tar);
    assert!(!entries.is_empty());
    for (h, _) in &entries {
        assert!(
            h.mtime.sec <= 1000,
            "{:?} at {:?}",
            String::from_utf8_lossy(&h.name),
            h.mtime
        );
    }
    assert!(
        entries
            .iter()
            .any(|(h, d)| h.name == b"work/made" && d == b"made now")
    );

    let (stderr, _, layers) = build(&out.join("no-epoch"), None);
    assert!(
        stderr.contains("WARNING: rewrite-timestamp is specified, but no source-date-epoch was found"),
        "{stderr}"
    );
    assert!(layers[1].0.get("annotations").is_none(), "{:?}", layers[1].0);
}

/// `--platform` of several (D77): each platform built as one of several, its steps named
/// with it, then one image: the index of each platform's manifest, in the order asked,
/// then their attestations, which the names resolve to, the ID is, an OCI output holds and
/// a push sends; a local output split by platform; the metadata file names the index and
/// each platform's provenance; a docker archive, a tar output and --cache-to refused,
/// named.
#[test]
fn several_platforms_make_one_image_of_their_manifests() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs, their_layer) = two_platform_image();
    let (port, _) = common::registry(index, blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let (push_port, repos) = common::writable_registry();
    let home = TempDir::new("multi-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let (arch, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let platforms = format!("linux/{other},linux/{arch}");
    let ctx = context(
        "multi-ctx",
        &format!(
            "FROM --platform=$BUILDPLATFORM {image} AS build\n\
             RUN [\"/bin/testguest\", \"fs\", \"write:/work/out=built\"]\n\
             FROM {image}\n\
             COPY --from=build /work/out /out\n"
        ),
    );
    let out = TempDir::new("multi-out");
    let meta = out.join("meta.json");
    let iid = out.join("iid");
    let name = format!("127.0.0.1:{push_port}/team/multi:1");
    let built = shards(&[
        "build",
        "--progress=plain",
        "--platform",
        &platforms,
        "-t",
        &name,
        "--push",
        "--metadata-file",
        meta.to_str().unwrap(),
        "--iidfile",
        iid.to_str().unwrap(),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    assert!(
        built.stderr.contains("] COPY --from=build /work/out /out")
            && built.stderr.contains(&format!("[linux/{other} stage-1 2/2]")),
        "{}",
        built.stderr
    );
    assert_eq!(
        built.stderr.matches("load build definition").count(),
        1,
        "{}",
        built.stderr
    );
    let id = std::fs::read_to_string(&iid).unwrap();
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    assert_eq!(written["containerimage.digest"], id.as_str(), "{written}");
    assert_eq!(
        written["containerimage.descriptor"]["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    for p in [&other, &arch] {
        assert!(
            written
                .get(format!("buildx.build.provenance/linux/{p}"))
                .is_some(),
            "{written}"
        );
    }
    // Stored: the name resolves to the index, its ID.
    let inspected = shards(&["image", "inspect", "--format", "{{.Id}}", &name]);
    assert_eq!(inspected.stdout.trim(), id, "{}", inspected.stderr);
    // Pushed: the index, each platform's manifest and its attestation, in order.
    let repos = repos.lock().unwrap();
    let manifests = repos.manifests.get("team/multi").expect("the repository pushed");
    let (_, pushed) = manifests.get("1").expect("its tag pushed");
    let pushed: serde_json::Value = serde_json::from_slice(pushed).unwrap();
    let entries = pushed["manifests"].as_array().unwrap();
    assert_eq!(entries.len(), 4, "{pushed}");
    assert_eq!(entries[0]["platform"]["architecture"], other, "{pushed}");
    assert_eq!(entries[1]["platform"]["architecture"], arch, "{pushed}");
    for (i, of) in [(2, 0), (3, 1)] {
        assert_eq!(
            entries[i]["annotations"]["vnd.docker.reference.type"],
            "attestation-manifest"
        );
        assert_eq!(
            entries[i]["annotations"]["vnd.docker.reference.digest"],
            entries[of]["digest"]
        );
    }
    let theirs: serde_json::Value =
        serde_json::from_slice(&manifests.get(entries[0]["digest"].as_str().unwrap()).unwrap().1).unwrap();
    assert_eq!(theirs["layers"][0]["digest"], their_layer.as_str(), "{theirs}");
    drop(repos);

    // An OCI layout and a local output of the same build.
    let layout = out.join("layout");
    let files = out.join("files");
    let both = shards(&[
        "build",
        "--platform",
        &platforms,
        "-o",
        &format!("type=oci,dest={},tar=false", layout.display()),
        "-o",
        &format!("type=local,dest={}", files.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(both.status, Some(0), "{}", both.stderr);
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    assert_eq!(
        top["manifests"][0]["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let blob =
        |d: &str| std::fs::read(layout.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap();
    let held: serde_json::Value =
        serde_json::from_slice(&blob(top["manifests"][0]["digest"].as_str().unwrap())).unwrap();
    for (i, p) in [other, arch].iter().enumerate() {
        let m: serde_json::Value =
            serde_json::from_slice(&blob(held["manifests"][i]["digest"].as_str().unwrap())).unwrap();
        let c: serde_json::Value =
            serde_json::from_slice(&blob(m["config"]["digest"].as_str().unwrap())).unwrap();
        assert_eq!(c["architecture"], *p, "{c}");
        assert_eq!(
            std::fs::read(files.join(format!("linux_{p}")).join("out")).unwrap(),
            b"built"
        );
    }

    // A tar of several platforms (D86): each platform's files in a directory of its name,
    // the platforms in name order, as BuildKit's tar exporter writes one.
    let joined = out.join("several.tar");
    let tarred = shards(&[
        "build",
        "--platform",
        &platforms,
        "-o",
        &format!("type=tar,dest={}", joined.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(tarred.status, Some(0), "{}", tarred.stderr);
    let entries = tar_entries(&std::fs::read(&joined).unwrap());
    let mut dirs: Vec<String> = [other, arch].iter().map(|p| format!("linux_{p}")).collect();
    dirs.sort();
    let names: Vec<String> = entries
        .iter()
        .map(|(h, _)| String::from_utf8(h.name.clone()).unwrap())
        .collect();
    for d in &dirs {
        let at = names
            .iter()
            .position(|n| *n == format!("{d}/"))
            .unwrap_or_else(|| panic!("{names:?}"));
        assert_eq!(entries[at].0.mode, 0o755);
        let built = entries
            .iter()
            .find(|(h, _)| h.name == format!("{d}/out").into_bytes())
            .unwrap_or_else(|| panic!("{names:?}"));
        assert_eq!(built.1, b"built");
    }
    let order: Vec<usize> = dirs
        .iter()
        .map(|d| names.iter().position(|n| *n == format!("{d}/")).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{names:?}");

    // One cache of every platform's records (D87): written by a build of both, it gives
    // a fresh home each platform's step.
    let cache = out.join("cache");
    let cached = shards(&[
        "build",
        "--platform",
        &platforms,
        "--cache-to",
        &format!("type=local,dest={}", cache.display()),
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(cached.status, Some(0), "{}", cached.stderr);
    let fresh = TempDir::new("multi-fresh-home");
    let fresh_env = [
        ("SHARDS_HOME", fresh.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let again = run_shards_env(
        &[],
        &[
            "build",
            "--progress=plain",
            "--platform",
            &platforms,
            "--cache-from",
            &format!("type=local,src={}", cache.display()),
            ctx.to_str().unwrap(),
        ],
        &fresh_env,
        TIMEOUT,
    );
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    for p in [other, arch] {
        let n = again
            .stderr
            .lines()
            .find_map(|l| {
                l.contains(&format!("[linux/{p} stage-1 2/2] COPY"))
                    .then(|| l.split(' ').next().unwrap().to_string())
            })
            .unwrap_or_else(|| panic!("no COPY of linux/{p}:\n{}", again.stderr));
        assert!(
            again.stderr.lines().any(|l| l == format!("{n} CACHED")),
            "linux/{p}'s COPY not cached:\n{}",
            again.stderr
        );
    }

    let refused = shards(&[
        "build",
        "--platform",
        &platforms,
        "-o",
        "type=docker,dest=x.tar",
        ctx.to_str().unwrap(),
    ]);
    assert_eq!(refused.status, Some(1), "{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains("docker exporter does not currently support exporting manifest lists"),
        "{}",
        refused.stderr
    );
}

/// An Agentfile made from an image (§10, D78): `FROM` the image by its manifest's digest,
/// which a build finds in the store whatever the image is named here, its settings
/// written out, quoted so that nothing in them expands, its history as comments; built,
/// it makes an image whose config is the original's.
#[test]
fn an_agentfile_made_from_an_image_builds_its_config() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("from-image-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = context(
        "from-image-ctx",
        &format!(
            "FROM {image}\n\
             ENV A=\"a b\" C=\"\\$HOME and \\\"quotes\\\"\"\n\
             LABEL org.example.k=v\n\
             WORKDIR /w2\n\
             USER 1000:1000\n\
             EXPOSE 8080/udp\n\
             VOLUME /data\n\
             STOPSIGNAL SIGINT\n\
             HEALTHCHECK --interval=7s --retries=3 CMD [\"/bin/testguest\", \"exit\", \"0\"]\n\
             ENTRYPOINT [\"/bin/testguest\"]\n\
             CMD [\"report\"]\n"
        ),
    );
    let built = shards(&["build", "-t", "original:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let out = TempDir::new("from-image-out");
    let agentfile = out.join("Agentfile");
    let made = shards(&[
        "make",
        "agentfile",
        "original:1",
        "-o",
        agentfile.to_str().unwrap(),
    ]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let text = std::fs::read_to_string(&agentfile).unwrap();
    assert!(
        text.contains("\nFROM docker.io/library/original:1@sha256:"),
        "{text}"
    );
    assert!(text.contains("ENV C=\"\\$HOME and \\\"quotes\\\"\"\n"), "{text}");
    assert!(text.contains("# How the image was made"), "{text}");
    let rebuilt = shards(&[
        "build",
        "-f",
        agentfile.to_str().unwrap(),
        "-t",
        "rebuilt:1",
        out.to_str().unwrap(),
    ]);
    assert_eq!(rebuilt.status, Some(0), "{}", rebuilt.stderr);
    let config = |name: &str| {
        let r = shards(&["image", "inspect", "--format", "{{json .Config}}", name]);
        assert_eq!(r.status, Some(0), "{}", r.stderr);
        r.stdout
    };
    assert_eq!(config("rebuilt:1"), config("original:1"));
}
/// Reproducible builds are Docker's own (D76): each case of tests/repro/cases.json (what
/// a build copies, adds and configures, over no base), built at SOURCE_DATE_EPOCH with
/// `rewrite-timestamp=true` for a pinned platform into an OCI archive, makes the manifest
/// Docker 29.3.1 makes of it (`scripts/repro/generate`), byte for byte; where it does not,
/// the manifest and config said beside Docker's.
#[test]
fn reproducible_builds_are_dockers() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro");
    let spec: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("cases.json")).unwrap()).unwrap();
    let docker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("docker.json")).unwrap()).unwrap();
    let home = TempDir::new("repro-home");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let mut failures = Vec::new();
    for (case, want) in spec["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(docker["cases"].as_array().unwrap())
    {
        let name = case["name"].as_str().unwrap();
        assert_eq!(want["name"], name);
        let ctx = context(&format!("repro-{name}"), case["dockerfile"].as_str().unwrap());
        for f in case["files"].as_array().unwrap() {
            let path = ctx.join(f["path"].as_str().unwrap());
            let mode = |m: &serde_json::Value| u32::from_str_radix(m.as_str().unwrap(), 8).unwrap();
            if f["dir"] == true {
                std::fs::create_dir_all(&path).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode(&f["mode"]))).unwrap();
            } else if let Some(target) = f["symlink"].as_str() {
                std::os::unix::fs::symlink(target, &path).unwrap();
            } else if let Some(target) = f["hardlink"].as_str() {
                std::fs::hard_link(ctx.join(target), &path).unwrap();
            } else {
                let data = match f["data_b64"].as_str() {
                    Some(b) => base64_decode(b),
                    None => f["data"].as_str().unwrap().as_bytes().to_vec(),
                };
                std::fs::write(&path, data).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode(&f["mode"]))).unwrap();
            }
        }
        let layout = TempDir::new(&format!("repro-{name}-out"));
        let built = shards(&[
            "build",
            "--platform",
            spec["platform"].as_str().unwrap(),
            "--build-arg",
            &format!("SOURCE_DATE_EPOCH={}", spec["epoch"]),
            "-o",
            &format!(
                "type=oci,dest={},tar=false,rewrite-timestamp=true",
                layout.display()
            ),
            ctx.to_str().unwrap(),
        ]);
        assert_eq!(built.status, Some(0), "{name}: {}", built.stderr);
        let top: serde_json::Value =
            serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
        let digest = top["manifests"][0]["digest"].as_str().unwrap();
        if digest != want["manifest_digest"].as_str().unwrap() {
            let blob = |d: &str| {
                String::from_utf8(
                    std::fs::read(layout.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap(),
                )
                .unwrap()
            };
            let manifest = blob(digest);
            let m: serde_json::Value = serde_json::from_str(&manifest).unwrap();
            let config = blob(m["config"]["digest"].as_str().unwrap());
            failures.push(format!(
                "{name}: {digest}, Docker's {}\n  manifest: {manifest}\n  Docker's: {}\n  config:   {config}\n  Docker's: {}",
                want["manifest_digest"], want["manifest"], want["config"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Standard base64, padded: the cases' binary files.
fn base64_decode(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &v)| n | u32::from(v) << (18 - 6 * i));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}
