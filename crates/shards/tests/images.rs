//! Images from a registry, end to end: `shards run IMAGE` pulls the image from a registry
//! (a loopback one here, which containerd's rules reach over plain HTTP), builds its root
//! filesystem, boots a real VM into it, and runs the image's command as `docker run`
//! would, through the daemon each test's first run starts. The image's program is the
//! test guest (crates/testguest/src/workload.rs). Runs need vsock, which shards has on
//! Unix hosts.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    TempDir, cannot_run_vms, guest_init, kernel, rootfs_dir, run_shards_env, served, served_counting,
    sha256_digest, test_image,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The report's lines as a map from each line's first word.
fn report(stdout: &str) -> std::collections::BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn images_run_from_a_registry() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, served) = served_counting();
    let home = TempDir::new("images");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];

    // Not here yet: pulled, then run with the image's settings.
    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", first.stdout, first.stderr);
    assert_eq!(first.status, Some(0), "{shown}");
    assert!(
        first
            .stderr
            .contains(&format!("Unable to find image '{image}' locally")),
        "{shown}"
    );
    for line in ["uid 1000", "cwd /work", "env FROM_IMAGE=yes", "env PATH=/bin"] {
        assert!(first.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }

    // Now stored: nothing is fetched, and the command line wins over the image.
    let asked = served.load(Ordering::SeqCst);
    let args = [
        "--pull",
        "never",
        "-u",
        "root",
        "-e",
        "FROM_IMAGE=cli",
        "-w",
        "/",
        image.as_str(),
        "report",
    ];
    let second = run_shards_env(&["run"], &args, &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
    assert_eq!(second.status, Some(0), "{shown}");
    for line in ["uid 0", "cwd /", "env FROM_IMAGE=cli"] {
        assert!(second.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        asked,
        "the stored image served the second run"
    );

    // Named by the digest its tag resolved to, as dockerd's store finds one pulled by its
    // tag: the stored image, whether the run may pull or not, nothing fetched.
    let digests = run_shards_env(
        &["image"],
        &["inspect", "--format", "{{index .RepoDigests 0}}", image.as_str()],
        &env,
        TIMEOUT,
    );
    assert_eq!(digests.status, Some(0), "{}", digests.stderr);
    let pinned = digests.stdout.trim().to_string();
    assert!(pinned.contains("@sha256:"), "{pinned}");
    for pull in ["never", "missing"] {
        let run = run_shards_env(&["run"], &["--pull", pull, pinned.as_str()], &env, TIMEOUT);
        let shown = format!("--- stdout\n{}\n--- stderr\n{}", run.stdout, run.stderr);
        assert_eq!(run.status, Some(0), "--pull {pull}\n{shown}");
        assert!(
            run.stdout.lines().any(|l| l == "uid 1000"),
            "--pull {pull}\n{shown}"
        );
        assert!(
            !run.stderr.contains("Unable to find image"),
            "--pull {pull}\n{shown}"
        );
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        asked,
        "runs by the digest fetched nothing"
    );

    // The stored config changed under its digest, to run `exit 9`: a run that may not
    // pull refuses it rather than run a spec nobody pulled; one that may pulls it again,
    // and runs what was pulled (audit A11).
    let (_, blobs) = test_image();
    let config = &blobs[0];
    let digest = sha256_digest(config);
    let stored = home
        .join("images/blobs/sha256")
        .join(digest.trim_start_matches("sha256:"));
    assert_eq!(&std::fs::read(&stored).unwrap(), config);
    let text = String::from_utf8(config.clone()).unwrap();
    let changed = text.replace(r#""Cmd":["report"]"#, r#""Cmd":["exit","9"]"#);
    assert_ne!(changed, text);
    std::fs::write(&stored, changed).unwrap();
    let refused = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", refused.stdout, refused.stderr);
    assert_eq!(refused.status, Some(125), "{shown}");
    assert!(
        refused.stderr.contains(&format!(
            "the stored copy has changed; pull '{image}' again to mend it"
        )),
        "{shown}"
    );
    let mended = run_shards_env(&["run"], &["--pull", "missing", image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", mended.stdout, mended.stderr);
    assert_eq!(mended.status, Some(0), "{shown}");
    assert!(
        mended.stdout.lines().any(|l| l == "uid 1000"),
        "the pulled command ran\n{shown}"
    );
    assert!(
        mended
            .stderr
            .contains(&format!("the stored copy has changed; pulling '{image}' again")),
        "{shown}"
    );
    assert!(!mended.stderr.contains("Unable to find image"), "{shown}");
    assert_eq!(&std::fs::read(&stored).unwrap(), config, "the stored copy mended");
}

/// An image whose root filesystem would take more than the daemon's limits allow is
/// refused, as `docker run` refuses what its daemon cannot do, and nothing of it is built
/// (audit A10): here it has more entries than `SHARDS_MAX_IMAGE_ENTRIES`.
#[test]
fn an_image_past_its_limits_is_refused() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("images-limits");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_MAX_IMAGE_ENTRIES", "3".as_ref()),
    ];
    let refused = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", refused.stdout, refused.stderr);
    assert_eq!(refused.status, Some(125), "{shown}");
    assert!(
        refused
            .stderr
            .contains("more than 3 entries (SHARDS_MAX_IMAGE_ENTRIES)"),
        "{shown}"
    );
    let built = std::fs::read_dir(home.join(rootfs_dir()))
        .unwrap()
        .filter(|e| !e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(built, 0, "{shown}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0));
}

/// A pull's platform is `--platform`'s, else SHARDS_DEFAULT_PLATFORM's, else
/// DOCKER_DEFAULT_PLATFORM's, as docker/cli defaults the flag (v29.8.1
/// cli/command/image/pull.go): an index of two platforms, pulled into a home of its own
/// each time, is stored for the one the defaults say, and for the one a pull names whatever
/// they say.
#[test]
fn a_pulls_platform_defaults_to_the_default_platforms() {
    let (index, blobs, _) = common::two_platform_image();
    let (port, _) = common::registry(index, blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let (arch, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let (ours, theirs) = (format!("linux/{arch}"), format!("linux/{other}"));
    for (defaults, named, want) in [
        (vec![("DOCKER_DEFAULT_PLATFORM", theirs.as_str())], None, other),
        (
            vec![
                ("SHARDS_DEFAULT_PLATFORM", theirs.as_str()),
                ("DOCKER_DEFAULT_PLATFORM", ours.as_str()),
            ],
            None,
            other,
        ),
        (
            vec![("DOCKER_DEFAULT_PLATFORM", theirs.as_str())],
            Some(ours.as_str()),
            arch,
        ),
        (vec![], None, arch),
    ] {
        let home = TempDir::new("pull-platform");
        let mut env = vec![("SHARDS_HOME", home.as_os_str())];
        env.extend(defaults.iter().map(|(k, v)| (*k, std::ffi::OsStr::new(*v))));
        let mut args = Vec::new();
        if let Some(p) = named {
            args.extend(["--platform", p]);
        }
        args.extend(["-q", image.as_str()]);
        let pulled = run_shards_env(&["pull"], &args, &env, TIMEOUT);
        assert_eq!(pulled.status, Some(0), "{defaults:?} {named:?}: {pulled}");
        let inspected = run_shards_env(
            &["image", "inspect"],
            &["--format", "{{.Architecture}}", image.as_str()],
            &env,
            TIMEOUT,
        );
        assert_eq!(inspected.status, Some(0), "{inspected}");
        assert_eq!(inspected.stdout.trim(), want, "{defaults:?} {named:?}");
    }
}

/// A registry off loopback that speaks plain HTTP is pulled from once Docker's daemon
/// configuration names it insecure (D194), as dockerd pulls from one: the rootless
/// engine's daemon.json, under `XDG_CONFIG_HOME` (the native one's on Windows, under
/// `PROGRAMDATA`). Not named, it is asked in HTTPS alone, and its plain answer is the
/// pull's failure.
#[test]
fn a_registry_daemon_json_names_insecure_is_pulled_from_in_plain_http() {
    let Some(ip) = common::routable_v4() else {
        eprintln!("SKIP: this host has no IPv4 route to reach itself off loopback at");
        return;
    };
    let (manifest, blobs) = test_image();
    let (port, _) = common::registry_at(ip.into(), manifest, blobs);
    let image = format!("{ip}:{port}/test/image:v1");
    let home = TempDir::new("insecure");
    let config = home.join("config");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("PROGRAMDATA", config.as_os_str()),
    ];
    let unnamed = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_ne!(unnamed.status, Some(0), "{unnamed}");
    assert!(
        unnamed
            .stderr
            .contains("server gave HTTP response to HTTPS client"),
        "{unnamed}"
    );
    let file = if cfg!(windows) {
        config.join("docker").join("config").join("daemon.json")
    } else {
        config.join("docker").join("daemon.json")
    };
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!(r#"{{"insecure-registries": ["{ip}:{port}"]}}"#)).unwrap();
    let pulled = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    assert_eq!(pulled.stdout.trim(), image, "{pulled}");
    // A configuration dockerd would not start with fails the pull, saying where it is.
    std::fs::write(&file, r#"{"insecure-registries": ["ftp://a"]}"#).unwrap();
    let refused = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(refused.stderr.contains("should not contain '://'"), "{refused}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// Docker Hub's mirrors (D194): a pull of a Docker Hub repository asks the mirror Docker's
/// daemon configuration names first, as dockerd does, and an image the mirror holds is
/// the one pulled: the test image, at a Docker Hub name it is not at, served by a
/// registry on loopback. Its digest is the test image's, which Docker Hub has not.
#[test]
fn a_mirror_daemon_json_names_serves_docker_hubs_pull() {
    let (manifest, blobs) = test_image();
    let digest = sha256_digest(&manifest);
    let (port, served) = common::registry(manifest, blobs);
    let home = TempDir::new("mirror");
    let config = home.join("config");
    let file = if cfg!(windows) {
        config.join("docker").join("config").join("daemon.json")
    } else {
        config.join("docker").join("daemon.json")
    };
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(r#"{{"registry-mirrors": ["http://127.0.0.1:{port}"]}}"#),
    )
    .unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("PROGRAMDATA", config.as_os_str()),
    ];
    let pulled = run_shards_env(&["pull"], &["-q", "test/image:v1"], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    assert_eq!(pulled.stdout.trim(), "docker.io/test/image:v1", "{pulled}");
    assert!(served.load(Ordering::SeqCst) > 0, "the mirror served it");
    let digests = run_shards_env(
        &["image", "inspect"],
        &["--format", "{{json .RepoDigests}}", "test/image:v1"],
        &env,
        TIMEOUT,
    );
    assert_eq!(digests.status, Some(0), "{digests}");
    assert!(digests.stdout.contains(&digest), "{digests}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// `pull --output-agentfile` writes the Agentfile of the image it pulled once it is
/// through, as `make agentfile` writes one: into a directory as its `Agentfile`, where
/// `shards build DIR` finds one, or to the file named; of the platform pulled. A pull that
/// fails writes nothing, and `-a`, which names no one image, is refused before it pulls.
#[test]
fn a_pull_writes_its_images_agentfile_where_asked() {
    let (index, blobs, _) = common::two_platform_image();
    let (port, _) = common::registry(index, blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let theirs = if cfg!(target_arch = "aarch64") {
        "linux/amd64"
    } else {
        "linux/arm64"
    };
    let home = TempDir::new("pull-agentfile");
    let out = home.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |command: &[&str], args: &[&str]| run_shards_env(command, args, &env, TIMEOUT);
    let names = || {
        let mut names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let made = |platform: &[&str]| {
        let mut args = platform.to_vec();
        args.push(image.as_str());
        let made = shards(&["make", "agentfile"], &args);
        assert_eq!(made.status, Some(0), "{made}");
        made.stdout
    };
    let dir = out.to_str().unwrap();
    let pulled = shards(
        &["pull"],
        &[
            "-q",
            "--platform",
            theirs,
            "--output-agentfile",
            dir,
            image.as_str(),
        ],
    );
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let their_agentfile = std::fs::read_to_string(out.join("Agentfile")).unwrap();
    assert!(
        their_agentfile.contains(&format!("\nFROM {image}@sha256:")),
        "{their_agentfile}"
    );
    assert_eq!(their_agentfile, made(&["--platform", theirs]));
    let ours = out.join("ours");
    let pulled = shards(
        &["pull"],
        &["-q", "--output-agentfile", ours.to_str().unwrap(), image.as_str()],
    );
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let our_agentfile = std::fs::read_to_string(&ours).unwrap();
    assert_eq!(our_agentfile, made(&[]));
    assert_ne!(our_agentfile, their_agentfile, "each platform's own");
    assert_eq!(names(), ["Agentfile", "ours"]);
    let missing = format!("127.0.0.1:{port}/test/missing:1");
    let none = out.join("none");
    let failed = shards(
        &["pull"],
        &[
            "-q",
            "--output-agentfile",
            none.to_str().unwrap(),
            missing.as_str(),
        ],
    );
    assert_ne!(failed.status, Some(0), "{failed}");
    let repository = format!("127.0.0.1:{port}/test/image");
    let all = shards(
        &["pull"],
        &[
            "-a",
            "--output-agentfile",
            none.to_str().unwrap(),
            repository.as_str(),
        ],
    );
    assert_eq!(all.status, Some(1), "{all}");
    assert!(all.stderr.contains("--all-tags does not name"), "{all}");
    assert_eq!(names(), ["Agentfile", "ours"]);
}

/// A run of another CPU's image runs under its CPU's emulator (D179), which its microVM's
/// own kernel registers as it boots: from a cold boot, which saves its template, then from a
/// copy of the template, which keeps it registered. Asked for by `--platform`, it is not
/// warned of; named by its lone manifest and no platform, it runs too, warned of as dockerd
/// warns of it (moby docker-v29.3.1 daemon/create.go), as does an index of its CPU's alone
/// (PM M250). An image keeps each platform pulled, as dockerd's does (M243): pulled for
/// another platform where this host's is here, it is up to date, and a run that asks no
/// platform runs this host's still. A platform not here is refused where the run may not
/// pull; a lone manifest of another platform than the one asked, once pulled; an ID's, as
/// dockerd refuses it, where Docker's CLI would pull the ID as a repository's name.
#[test]
fn a_run_of_another_cpus_image_runs_under_its_emulator() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = TempDir::new("emulated-runs");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    // alpine:3.22's index and its 64-bit manifests (Docker Hub, 2026-10-15).
    const INDEX: &str = "alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8";
    let (other, machine, lone, lone_platform) = if cfg!(target_arch = "aarch64") {
        (
            "amd64",
            "x86_64",
            "alpine@sha256:3e9b4b680bfc9fb5269227cffbd6d42be39fbf7c0b908123913864aa4447e764",
            "linux/amd64",
        )
    } else {
        (
            "arm64",
            "aarch64",
            "alpine@sha256:2e1a7aa4cbc4e9e5222bb4c24a839aa1a6170ea5492d644777ce7b178824e44f",
            "linux/arm64/v8",
        )
    };
    let platform = format!("linux/{other}");
    let ours = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let run = |args: &[&str]| run_shards_env(&["run"], args, &env, TIMEOUT);
    let shown = |r: &common::Run| format!("--- stdout\n{}\n--- stderr\n{}", r.stdout, r.stderr);
    let pulled = run_shards_env(&["pull"], &[INDEX], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{}", shown(&pulled));
    // By its digest alone: here by another name of its repository, so up to date.
    let digest_only = INDEX.replace(":3.22@", "@");
    let again = run_shards_env(&["pull"], &[&digest_only], &env, TIMEOUT);
    let shown_again = shown(&again);
    assert_eq!(again.status, Some(0), "{shown_again}");
    assert!(
        shown_again.contains(&format!("Status: Image is up to date for {digest_only}")),
        "{shown_again}"
    );
    let first = run(&["--rm", "--platform", &platform, INDEX, "uname", "-m"]);
    let shown_first = shown(&first);
    assert_eq!(first.status, Some(0), "{shown_first}");
    assert_eq!(first.stdout.trim(), machine, "{shown_first}");
    assert!(booted(&first), "{shown_first}");
    assert!(
        !first.stderr.contains("WARNING"),
        "asked for, not warned of: {shown_first}"
    );
    assert!(
        first
            .stderr
            .contains(&format!("Unable to find image '{INDEX}' locally"))
            && first
                .stderr
                .contains(&format!("Status: Image is up to date for {INDEX}")),
        "another platform of an image here: {shown_first}"
    );
    if shards_vmm::vm::SNAPSHOTS {
        let second = run(&[
            "--rm",
            "--pull",
            "never",
            "--platform",
            &platform,
            INDEX,
            "uname",
            "-m",
        ]);
        let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
        assert_eq!(second.status, Some(0), "{shown}");
        assert_eq!(
            second.stdout.trim(),
            machine,
            "its emulator kept in the template: {shown}"
        );
        assert!(!booted(&second), "restored, not booted: {shown}");
    }
    // This host's still, where no platform is asked.
    let native = run(&["--rm", INDEX, "uname", "-m"]);
    let shown_native = shown(&native);
    assert_eq!(native.status, Some(0), "{shown_native}");
    assert_eq!(native.stdout.trim(), std::env::consts::ARCH, "{shown_native}");
    assert!(
        !native.stderr.contains("WARNING") && !native.stderr.contains("Unable to find"),
        "{shown_native}"
    );
    let never = run(&[
        "--rm",
        "--pull",
        "never",
        "--platform",
        "linux/s390x",
        INDEX,
        "true",
    ]);
    let shown_never = shown(&never);
    assert_eq!(never.status, Some(125), "{shown_never}");
    assert!(
        never.stderr.contains(&format!(
            "image with reference {INDEX} was found but does not provide the specified platform (linux/s390x)"
        )),
        "{shown_never}"
    );
    // By an ID: refused, no repository of its name pulled.
    let id = INDEX.rsplit(':').next().and_then(|hex| hex.get(..12)).unwrap();
    let by_id = run(&["--rm", "--platform", "linux/s390x", id, "true"]);
    let shown_id = shown(&by_id);
    assert_eq!(by_id.status, Some(125), "{shown_id}");
    assert!(
        by_id.stderr.contains(&format!(
            "image with reference {INDEX} was found but does not provide the specified platform (linux/s390x)"
        )) && !by_id.stderr.contains("Unable to find"),
        "{shown_id}"
    );
    let warned = run(&["--rm", lone, "uname", "-m"]);
    let shown_warned = shown(&warned);
    assert_eq!(warned.status, Some(0), "{shown_warned}");
    assert_eq!(warned.stdout.trim(), machine, "{shown_warned}");
    assert!(
        warned.stderr.contains(&format!(
            "WARNING: The requested image's platform ({lone_platform}) does not match the detected host platform (linux/"
        )) && warned
            .stderr
            .contains(") and no specific platform was requested"),
        "{shown_warned}"
    );
    // Its content here as the index's, but no name of its repository resolved to it.
    assert!(
        warned
            .stderr
            .contains(&format!("Status: Downloaded newer image for {lone}")),
        "{shown_warned}"
    );
    // An index of the other CPU's alone (Docker Hub's per-CPU alpine:3.22, 2026-10-17, whose
    // one manifest is `lone`): its manifest runs, as `lone` does, where shards found none to
    // pull (PM M250).
    let alone = if cfg!(target_arch = "aarch64") {
        "amd64/alpine:3.22@sha256:33e4ce3b40f4bd419050b3b78101fa58b44c20c5288c0a50ce400e99a3768fc7"
    } else {
        "arm64v8/alpine:3.22@sha256:9a5c524dcd9acdcdb0683f7706045be0a6c464cc40ebad5708a5912c6c17eeda"
    };
    let foreign = run(&["--rm", alone, "uname", "-m"]);
    let shown_foreign = shown(&foreign);
    assert_eq!(foreign.status, Some(0), "{shown_foreign}");
    assert_eq!(foreign.stdout.trim(), machine, "{shown_foreign}");
    assert!(
        foreign.stderr.contains(&format!("Unable to find image '{alone}' locally"))
            && foreign.stderr.contains(&format!(
                "WARNING: The requested image's platform ({lone_platform}) does not match the detected host platform (linux/"
            )),
        "{shown_foreign}"
    );
    // 32-bit Arm's, which no guest runs itself (PM M250), on either host: emulated, and
    // warned of where no platform was asked (Docker Hub's arm32v7/alpine:3.22, 2026-10-18,
    // whose one manifest is arm/v7's).
    const ARM32: &str =
        "arm32v7/alpine:3.22@sha256:f7d3e5b6884ca6ad7154b91c3c9734db235bad4213092e7eba04dd96d4d61d3e";
    let arm32 = run(&["--rm", ARM32, "uname", "-m"]);
    let shown_arm32 = shown(&arm32);
    assert_eq!(arm32.status, Some(0), "{shown_arm32}");
    assert_eq!(arm32.stdout.trim(), "armv7l", "{shown_arm32}");
    assert!(
        arm32.stderr.contains(
            "WARNING: The requested image's platform (linux/arm/v7) does not match the detected host platform (linux/"
        ),
        "{shown_arm32}"
    );
    // A program the emulator runs that dies of a fault ends its run as a native one's does,
    // 132 for SIGILL: the emulator sends itself the signal to die by, which a namespace's
    // PID 1 is never sent, so the command runs under shards' reaper (PM M251), where as PID
    // 1 its run never ended.
    let crashing = TempDir::new("emulated-crash");
    let crash = crashing.join("crash");
    std::fs::write(&crash, common::crashing_program(other)).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&crash, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let bound = format!("{}:/t:ro", crashing.display());
    let crashed = run(&["--rm", "-v", &bound, lone, "/t/crash"]);
    assert_eq!(crashed.status, Some(132), "{}", shown(&crashed));
    // A lone manifest is pulled whatever its platform, and run for its own alone.
    let asked = format!("linux/{ours}");
    let strict = run(&["--rm", "--platform", &asked, lone, "true"]);
    let shown_strict = shown(&strict);
    assert_eq!(strict.status, Some(125), "{shown_strict}");
    assert!(
        strict
            .stderr
            .contains(&format!("Unable to find image '{lone}' locally"))
            && strict.stderr.contains(&format!(
                "image with reference {lone} was found but does not provide the specified platform ({asked})"
            )),
        "{shown_strict}"
    );
    let lenient = run_shards_env(&["pull"], &["--platform", &asked, lone], &env, TIMEOUT);
    assert_eq!(lenient.status, Some(0), "{}", shown(&lenient));
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// A guest runs its host's x86-64 level: the flags its own kernel reads of its CPU hold
/// every feature of the level the host's CPU reaches (the x86-64 psABI's levels, as
/// go-archvariant reads them), the basis on which an x86_64 host's guests take amd64's
/// levelled builds as their own (shards_image::platform::guest). x86_64 hosts alone.
#[test]
fn a_guest_runs_its_hosts_x86_64_level() {
    if !cfg!(target_arch = "x86_64") {
        eprintln!("SKIP: an x86_64 host's");
        return;
    }
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = TempDir::new("guest-level");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    // Docker Hub's alpine:3.22, amd64's manifest (2026-10-15).
    let alpine = "alpine@sha256:3e9b4b680bfc9fb5269227cffbd6d42be39fbf7c0b908123913864aa4447e764";
    let read = run_shards_env(&["run"], &["--rm", alpine, "cat", "/proc/cpuinfo"], &env, TIMEOUT);
    assert_eq!(read.status, Some(0), "{}", read.stderr);
    let flags = read
        .stdout
        .lines()
        .find_map(|l| {
            l.strip_prefix("flags")
                .and_then(|r| r.split_once(':'))
                .map(|(_, f)| f)
        })
        .expect("the guest's flags");
    let has = |all: &[&str]| all.iter().all(|f| flags.split_whitespace().any(|g| g == *f));
    let level = if !has(&["cx16", "lahf_lm", "popcnt", "pni", "sse4_1", "sse4_2", "ssse3"]) {
        1
    } else if !has(&[
        "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "abm", "movbe", "xsave",
    ]) {
        2
    } else if !has(&["avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"]) {
        3
    } else {
        4
    };
    assert_eq!(
        format!("v{level}"),
        shards_image::platform::maximum().variant,
        "the guest's flags: {flags}"
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// With the recorded guest, the first run of an image boots and saves a template, and the
/// next restores it: no `INIT_STARTED` marker, since init started in the template. The
/// restored run has the image's settings and the host's clock.
#[test]
fn repeat_runs_restore_a_template_of_the_image() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("templates");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    // The templates, each a directory: the daemon's record of each is a file beside it.
    let templates = || -> Vec<String> {
        std::fs::read_dir(home.join("templates"))
            .map(|d| {
                d.map(|e| e.unwrap())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert!(booted(&first), "{}", first.stderr);
    if !shards_vmm::vm::SNAPSHOTS {
        assert!(templates().is_empty(), "no snapshots here: {:?}", templates());
        return;
    }
    let saved = templates();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(!saved[0].contains(".new-"), "{saved:?}");

    // The working set that restores prefetch, where the backend records one: what the
    // first run touched, once it had answered (HVF), or the first warm restore, the second
    // run's (KVM, `vm::RESTORES_RECORD`). The daemon's pool was restored before it
    // existed, so a new daemon restores the next run.
    if shards_vmm::vm::RESTORES_RECORD {
        let recording = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
        assert_eq!(recording.status, Some(0), "{}", recording.stderr);
        assert!(!booted(&recording), "{}", recording.stderr);
        assert_eq!(recording.prefetched(), Some(0), "{}", recording.stderr);
    }
    // Stopped at once, the daemon writes it before it exits, which `daemon stop` waits
    // for: no restore here records it again where restores record none.
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let working_set = common::snapshot_file(&home.join("templates").join(&saved[0]), "working-set");
    let recorded = std::fs::metadata(&working_set).map(|m| m.len()).unwrap_or(0);
    assert!(
        recorded > 1024 || !shards_vmm::vm::WORKING_SETS,
        "a working set of {recorded} bytes"
    );

    let before = now_ns();
    let second = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    let after = now_ns();
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
    assert_eq!(second.status, Some(0), "{shown}");
    assert!(!booted(&second), "restored, not booted: {shown}");
    assert!(second.marker_us(shards_abi::marker::RESUMED).is_some(), "{shown}");
    let prefetched = second.prefetched().unwrap_or(0);
    assert!(
        (prefetched > 100 && prefetched * 8 < u128::from(recorded)) || !shards_vmm::vm::WORKING_SETS,
        "{prefetched} pages prefetched of a {recorded}-byte working set: {shown}"
    );
    for line in ["uid 1000", "cwd /work", "env FROM_IMAGE=yes"] {
        assert!(second.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
    let realtime: u128 = report(&second.stdout)["realtime"].parse().unwrap();
    assert!(
        (before..=after).contains(&realtime),
        "{before} <= {realtime} <= {after}"
    );
    assert_eq!(templates(), saved, "the template was reused");

    // A damaged working set is only a lost prefetch; without working sets, a file that is
    // none is ignored alike.
    std::fs::write(&working_set, b"not a working set").unwrap();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let unprefetched = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(unprefetched.status, Some(0), "{}", unprefetched.stderr);
    assert!(!booted(&unprefetched), "{}", unprefetched.stderr);
    assert_eq!(unprefetched.prefetched(), Some(0), "{}", unprefetched.stderr);
    assert!(
        unprefetched.stdout.lines().any(|l| l == "uid 1000"),
        "{}",
        unprefetched.stdout
    );

    // A template that no longer restores is removed, and the run boots instead, saving it
    // again. The daemon's pool holds VMs restored before the damage, so a new daemon, with
    // none, has to find out.
    std::fs::write(
        common::snapshot_file(&home.join("templates").join(&saved[0]), "state"),
        b"not a snapshot",
    )
    .unwrap();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let third = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(third.status, Some(0), "{}", third.stderr);
    assert!(booted(&third), "{}", third.stderr);
    assert!(
        third.stderr.contains("does not restore; booting instead"),
        "{}",
        third.stderr
    );
    assert_eq!(templates(), saved, "saved again under the same key");
    let fourth = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(fourth.status, Some(0), "{}", fourth.stderr);
    assert!(
        !booted(&fourth),
        "restored from the template saved again: {}",
        fourth.stderr
    );
}

/// An Agentfile's image restores its template as any image does. Its template is saved
/// with the in-VM server's device after its root filesystem (D60), and each restore is
/// given that device too: given its root filesystem alone, every restore was refused ("the
/// template names files it was not given") and each run booted. The guest is recorded
/// here, as users run, where `SHARDS_KERNEL` and `SHARDS_INIT` would boot every run.
#[test]
fn an_agentfiles_image_restores_its_template_too() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("agentfile-templates");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    let ctx = TempDir::new("agentfile-templates-ctx");
    std::fs::create_dir_all(ctx.join("agent")).unwrap();
    std::fs::write(ctx.join("agent/agent.json"), r#"{"name":"main"}"#).unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\n"),
    )
    .unwrap();
    let built = run_shards_env(
        &["build"],
        &["-t", "agents:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    let first = run_shards_env(&["run"], &["agents:1"], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert!(booted(&first), "{}", first.stderr);
    if !shards_vmm::vm::SNAPSHOTS {
        return;
    }
    for _ in 0..2 {
        let again = run_shards_env(&["run"], &["agents:1"], &env, TIMEOUT);
        let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
        let shown = format!(
            "--- stdout\n{}\n--- stderr\n{}\n--- daemon.log\n{log}",
            again.stdout, again.stderr
        );
        assert_eq!(again.status, Some(0), "{shown}");
        assert!(!again.stderr.contains("does not restore"), "{shown}");
        assert!(!booted(&again), "restored, not booted: {shown}");
        assert!(again.marker_us(shards_abi::marker::RESUMED).is_some(), "{shown}");
    }
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    assert!(!log.contains("names files it was not given"), "{log}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// The guest `kernel()` and `guest_init()` recorded in `home`, as users run, so that runs
/// save and restore templates.
fn record_guest(env: &[(&str, &std::ffi::OsStr)]) {
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
}

/// The one template in `home`.
fn the_template(home: &std::path::Path) -> std::path::PathBuf {
    let dirs: Vec<std::path::PathBuf> = std::fs::read_dir(home.join("templates"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.into_iter().next().unwrap()
}

/// A VM taken over while it saves a template leaves what it likes in the directory it was
/// granted, which every later run restores (PM M165); here, as one would, a link named for
/// the daemon's record to a file of the user's (a VM process wrapped by
/// `SHARDS_VM_BINARY`, which plants it and becomes this build's). The daemon writes its
/// record beside the template, through nothing the VM left: the file is untouched, and the
/// template still restores.
#[test]
fn a_templates_saver_leaves_nothing_the_daemon_writes_through() {
    if cannot_run_vms() || !shards_vmm::vm::SNAPSHOTS {
        eprintln!("SKIP: this host saves no templates");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("saver-plants");
    let victim = TempDir::new("saver-plants-victim");
    let canary = victim.join("canary");
    std::fs::write(&canary, b"the user's\n").unwrap();
    let wrapper = victim.join("shards-vm");
    // Exec'd by the daemon's spawner.
    common::program(
        &wrapper,
        &format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = --snapshot-dir ]; then mkdir -p \"$a\" && ln -s '{canary}' \"$a/origin.json\"; fi\n  prev=$a\ndone\nexec '{vm}' \"$@\"\n",
            canary = canary.display(),
            vm = common::shards_vm().display()
        ),
    );
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_VM_BINARY", wrapper.as_os_str()),
    ];
    record_guest(&env);
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert!(booted(&first), "{}", first.stderr);
    let template = the_template(&home);
    assert!(
        std::fs::symlink_metadata(template.join("origin.json")).is_ok_and(|m| m.file_type().is_symlink()),
        "the wrapped VM planted its link"
    );
    assert_eq!(
        std::fs::read(&canary).unwrap(),
        b"the user's\n",
        "written through the link"
    );
    let second = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(second.status, Some(0), "{}", second.stderr);
    assert!(!booted(&second), "restored, not booted: {}", second.stderr);
    assert_eq!(std::fs::read(&canary).unwrap(), b"the user's\n");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// What a VM taken over while it saved a template could have left in it, for every later
/// restore to meet (PM M165): its memory a link to a FIFO outside the template, and its
/// working set a FIFO. A restore opens neither, nor waits on either: no reader ever holds
/// either FIFO while the next run happens, which boots instead, the template refused.
#[test]
fn a_restore_reaches_nothing_its_saver_left_in_its_template() {
    use std::os::unix::fs::OpenOptionsExt as _;
    if cannot_run_vms() || !shards_vmm::vm::SNAPSHOTS {
        eprintln!("SKIP: this host saves no templates");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("saver-left");
    let outside = TempDir::new("saver-left-outside");
    let env = [("SHARDS_HOME", home.as_os_str())];
    record_guest(&env);
    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let template = the_template(&home);
    let fifo = |path: &std::path::Path| {
        let _ = std::fs::remove_file(path);
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo(2) of a NUL-terminated path.
        let made = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(made, 0, "{}", path.display());
    };
    let target = outside.join("memory");
    fifo(&target);
    let memory = common::snapshot_file(&template, "memory");
    std::fs::remove_file(&memory).unwrap();
    std::os::unix::fs::symlink(&target, &memory).unwrap();
    let working_set = common::snapshot_file(&template, "working-set");
    fifo(&working_set);

    let (again, home_path) = (image.clone(), home.to_path_buf());
    let runner = std::thread::spawn(move || {
        let env = [("SHARDS_HOME", home_path.as_os_str())];
        run_shards_env(&["run"], &["--pull", "never", again.as_str()], &env, TIMEOUT)
    });
    // A FIFO opened for writing without waiting is opened only while a reader holds it.
    let mut held: Vec<String> = Vec::new();
    while !runner.is_finished() {
        for path in [&target, &working_set] {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path);
            if opened.is_ok() {
                held.push(path.display().to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let second = runner.join().unwrap();
    assert!(held.is_empty(), "read by a restore: {held:?}");
    assert_eq!(second.status, Some(0), "{}", second.stderr);
    assert!(
        second.stderr.contains("does not restore; booting instead"),
        "{}",
        second.stderr
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// shards' grammar makes an image a microVM before any action but its removal, as `run`
/// does: `inspect vm`, `inspect image`, `history image` and `tag image` of an image not
/// here pull and convert it first. Docker's own order (`image inspect`) answers as
/// Docker's does, and removal fetches nothing.
#[test]
fn actions_on_an_image_not_here_convert_it_first() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, served) = served_counting();
    let home = TempDir::new("convert");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| {
        let (command, rest) = args.split_first().unwrap();
        run_shards_env(&[command], rest, &env, TIMEOUT)
    };
    let shown = |r: &common::Run| format!("--- stdout\n{}\n--- stderr\n{}", r.stdout, r.stderr);
    let rmi = || {
        let r = shards(&["rmi", image.as_str()]);
        assert_eq!(r.status, Some(0), "{}", shown(&r));
    };

    // Neither the image-first order nor removal fetches anything.
    let inspected = shards(&["image", "inspect", image.as_str()]);
    assert_eq!(inspected.status, Some(1), "{}", shown(&inspected));
    assert!(
        inspected.stderr.contains("No such image"),
        "{}",
        shown(&inspected)
    );
    let removed = shards(&["remove", "image", image.as_str()]);
    assert_ne!(removed.status, Some(0), "{}", shown(&removed));
    assert_eq!(served.load(Ordering::SeqCst), 0, "nothing was fetched");

    // `inspect vm IMAGE`: converted, and its microVM described.
    let vm = shards(&[
        "inspect",
        "vm",
        "--format",
        "{{.Id}} {{.Config.Cmd}}",
        image.as_str(),
    ]);
    assert_eq!(vm.status, Some(0), "{}", shown(&vm));
    assert!(
        vm.stderr
            .contains(&format!("Unable to find image '{image}' locally")),
        "{}",
        shown(&vm)
    );
    assert!(
        vm.stdout.starts_with("sha256:") && vm.stdout.trim_end().ends_with("[report]"),
        "{}",
        shown(&vm)
    );
    rmi();

    let inspected = shards(&["inspect", "image", "--format", "{{.Id}}", image.as_str()]);
    assert_eq!(inspected.status, Some(0), "{}", shown(&inspected));
    assert_eq!(
        inspected.stdout,
        vm.stdout.split(' ').next().unwrap().to_string() + "\n"
    );
    rmi();

    let history = shards(&["read", "history", "-q", image.as_str()]);
    assert_eq!(history.status, Some(0), "{}", shown(&history));
    // The test image's config records no history, so none is listed; it is stored now.
    let stored = shards(&["images", "-q"]);
    assert!(!stored.stdout.trim().is_empty(), "{}", shown(&stored));
    rmi();

    let tagged = shards(&["tag", "image", image.as_str(), "mine:converted"]);
    assert_eq!(tagged.status, Some(0), "{}", shown(&tagged));
    let listed = shards(&["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    assert!(
        listed.stdout.lines().any(|l| l == "mine:converted"),
        "{}",
        shown(&listed)
    );
    let _ = shards(&["stop", "daemon"]);
}
