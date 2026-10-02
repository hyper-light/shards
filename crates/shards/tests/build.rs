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
