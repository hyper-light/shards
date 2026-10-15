//! `docker buildx build` through shards' engine (D174): buildx's own builder, BuildKit as
//! a privileged container of its docker-container driver, is a microVM on the host's
//! network (D124). Its daemon nests its cgroups (D173), keeps its snapshots as overlays on
//! its state volume, and runs each RUN step in a container of its own runc, all in the
//! microVM; the image it builds is loaded into shards and runs as a microVM.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(60);
/// The BuildKit the builder runs: pinned, as the research it is held to is
/// (docs/research/docker-compose-compat.md, BK).
const BUILDKIT: &str = "moby/buildkit:v0.33.0";

/// The docker CLI and its buildx plugin, where this host has them: on `PATH`, and where
/// the CLI looks for its plugins (cli-plugins/manager's system directories, and Docker
/// Desktop's).
fn docker_and_buildx() -> Option<(PathBuf, PathBuf)> {
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    };
    let docker = on_path("docker")?;
    let buildx = [
        "/usr/local/lib/docker/cli-plugins",
        "/usr/local/libexec/docker/cli-plugins",
        "/usr/lib/docker/cli-plugins",
        "/usr/libexec/docker/cli-plugins",
        "/Applications/Docker.app/Contents/Resources/cli-plugins",
    ]
    .into_iter()
    .map(|d| Path::new(d).join("docker-buildx"))
    .find(|p| p.is_file())?;
    Some((docker, buildx))
}

/// A Dockerfile's RUN step, built by buildx's own builder in a microVM, its image loaded
/// into shards and run there.
#[test]
fn buildx_builds_with_its_own_builder_in_a_microvm() {
    let Some((docker, buildx)) = docker_and_buildx() else {
        eprintln!("SKIP: no docker CLI with buildx on this host");
        return;
    };
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("buildx");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let pulled = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let context = home.join("ctx");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        format!("FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/built=yes\"]\n"),
    )
    .unwrap();
    // The CLI's own config, its plugin the host's buildx, and no credential helper.
    let config = home.join("docker-config");
    std::fs::create_dir_all(config.join("cli-plugins")).unwrap();
    std::os::unix::fs::symlink(&buildx, config.join("cli-plugins/docker-buildx")).unwrap();
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let docker = |args: &[&str]| {
        let out = std::process::Command::new(&docker)
            .args(args)
            .env("DOCKER_HOST", &host)
            .env("DOCKER_CONFIG", &config)
            .env("PATH", &no_helpers)
            .env_remove("DOCKER_CONTEXT")
            .env("BUILDX_NO_DEFAULT_ATTESTATIONS", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let builder = format!("shards-test-{}", std::process::id());
    let (status, out, err) = docker(&[
        "buildx",
        "create",
        "--name",
        &builder,
        "--driver",
        "docker-container",
        "--driver-opt",
        &format!("image={BUILDKIT}"),
        "--driver-opt",
        "network=host",
    ]);
    assert_eq!(status, Some(0), "{out}{err}");
    let (status, out, err) = docker(&[
        "buildx",
        "build",
        "--builder",
        &builder,
        "--progress",
        "plain",
        "--load",
        "-t",
        "shp/buildx-built:1",
        context.to_str().unwrap(),
    ]);
    let built = (status, out, err);
    let (removed, _, gone) = docker(&["buildx", "rm", &builder]);
    assert_eq!(built.0, Some(0), "{}{}", built.1, built.2);
    assert_eq!(removed, Some(0), "{gone}");
    let (status, out, err) = docker(&["run", "--rm", "shp/buildx-built:1", "slurp", "/tmp/built"]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "slurp /tmp/built: yes\n"),
        "{err}"
    );
}
