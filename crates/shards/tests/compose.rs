//! A Compose project through the Engine API (D173): docker compose's own client,
//! unchanged, with `DOCKER_HOST` at shards' socket, makes each service a microVM. Its
//! services start in the order their dependencies' health says, reach each other by name
//! on the project's network and nothing else through it, keep their data in its volumes,
//! and go with `down`.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(60);
/// A client's call past this is taken for hung, and fails its test with what it said
/// (`common::output_within`): well past the longest wait a call here asks for, `up`'s 120 s.
const CLIENT: Duration = Duration::from_secs(600);

/// docker compose, where this host has it: on `PATH`, or where the Docker CLI looks for
/// its plugins (cli-plugins/manager's system directories, and Docker Desktop's).
fn compose() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("docker-compose"));
    let plugins = [
        "/usr/local/lib/docker/cli-plugins",
        "/usr/local/libexec/docker/cli-plugins",
        "/usr/lib/docker/cli-plugins",
        "/usr/libexec/docker/cli-plugins",
        "/Applications/Docker.app/Contents/Resources/cli-plugins",
    ]
    .into_iter()
    .map(|d| Path::new(d).join("docker-compose"));
    on_path.chain(plugins).find(|p| p.is_file())
}

/// A home whose daemon runs, the test image pulled into it.
fn home(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let pulled = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    Some((home, image))
}

/// A project of two services on one network: `db` serves on 7000 and says it is healthy
/// once it does; `app`, started once `db` is healthy, keeps a named volume.
#[test]
fn a_compose_project_runs_as_microvms() {
    let Some(compose) = compose() else {
        eprintln!("SKIP: no docker compose on this host");
        return;
    };
    let Some((home, image)) = home("compose") else {
        return;
    };
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("compose.yaml"),
        format!(
            r#"name: shp
services:
  db:
    image: {image}
    command: ["serve", "7000", "1000"]
    ports: ["127.0.0.1::7000"]
    healthcheck:
      test: ["CMD", "/bin/testguest", "exit", "0"]
      interval: 1s
      retries: 30
    networks: [back]
  app:
    image: {image}
    command: ["sleep"]
    depends_on:
      db:
        condition: service_healthy
    networks: [back]
    volumes:
      - data:/data
networks:
  back: {{}}
volumes:
  data: {{}}
"#
        ),
    )
    .unwrap();
    let config = home.join("docker-config");
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let run = |args: &[&str]| {
        let out = common::output_within(
            std::process::Command::new(&compose)
                .current_dir(&project)
                .args(["-p", "shp"])
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env_remove("DOCKER_CONTEXT")
                .env_remove("COMPOSE_PROJECT_NAME")
                .env_remove("COMPOSE_FILE")
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = run(&["up", "-d", "--wait", "--wait-timeout", "120"]);
    assert_eq!(status, Some(0), "{out}{err}");
    // Both running, each a microVM.
    let (status, out, err) = run(&["ps", "--format", "{{.Service}} {{.State}}"]);
    assert_eq!(status, Some(0), "{err}");
    let mut listed: Vec<&str> = out.lines().collect();
    listed.sort_unstable();
    assert_eq!(listed, ["app running", "db running"], "{out}{err}");
    // app reaches db by its service's name on the project's network.
    let (status, out, err) = run(&["exec", "-T", "app", "/bin/testguest", "ask", "db:7000"]);
    assert_eq!(status, Some(0), "{out}{err}");
    assert!(out.starts_with("ask from "), "{out}{err}");
    // What db said, and app's volume kept.
    let (status, out, err) = run(&["logs", "--no-color", "db"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.contains("ready"), "{out}");
    // db's port, published on the host, which reaches it there.
    let (status, out, err) = run(&["port", "db", "7000"]);
    assert_eq!(status, Some(0), "{err}");
    let published = out.trim().to_string();
    assert!(published.starts_with("127.0.0.1:"), "{out}{err}");
    let mut conn = std::net::TcpStream::connect(&published).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    conn.shutdown(std::net::Shutdown::Write).unwrap();
    let mut said = String::new();
    std::io::Read::read_to_string(&mut conn, &mut said).unwrap();
    assert!(said.starts_with("from "), "{published}: {said:?}");
    // A one-off, removed when it ends, its status the command's.
    let (status, _, err) = run(&["run", "--rm", "--no-deps", "app", "exit", "3"]);
    assert_eq!(status, Some(3), "{err}");
    // Stopped, started, restarted, frozen and thawed; its processes listed.
    for verb in [
        &["stop", "-t", "1", "app"][..],
        &["start", "app"],
        &["restart", "-t", "1", "app"],
        &["pause", "app"],
        &["unpause", "app"],
    ] {
        let (status, out, err) = run(verb);
        assert_eq!(status, Some(0), "{verb:?}: {out}{err}");
    }
    let (status, out, err) = run(&["top", "app"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.contains("testguest sleep"), "{out}");
    // A file copied into app's volume, and back out of it.
    let local = home.join("kept.txt");
    std::fs::write(&local, "kept\n").unwrap();
    let (status, _, err) = run(&["cp", local.to_str().unwrap(), "app:/data/kept.txt"]);
    assert_eq!(status, Some(0), "{err}");
    let back = home.join("back");
    std::fs::create_dir_all(&back).unwrap();
    let (status, _, err) = run(&["cp", "app:/data/kept.txt", back.to_str().unwrap()]);
    assert_eq!(status, Some(0), "{err}");
    assert_eq!(std::fs::read_to_string(back.join("kept.txt")).unwrap(), "kept\n");
    // Gone with down: its microVMs, network and volume.
    let (status, out, err) = run(&["down", "-v", "--timeout", "1"]);
    assert_eq!(status, Some(0), "{out}{err}");
    let (status, out, _) = run(&["ps", "-a", "-q"]);
    assert_eq!((status, out.trim()), (Some(0), ""));
    let env: [(&str, &OsStr); 1] = [("SHARDS_HOME", home.as_os_str())];
    let volumes = run_shards_env(&["volume", "ls"], &["-q"], &env, TIMEOUT);
    assert!(!volumes.stdout.lines().any(|v| v == "shp_data"), "{volumes}");
    let networks = run_shards_env(&["network", "ls"], &["--format", "{{.Name}}"], &env, TIMEOUT);
    assert!(!networks.stdout.lines().any(|n| n == "shp_back"), "{networks}");
}

/// What else a project says of its services, as Compose sends it: a job that must end
/// well before its dependent starts; environment from the file and from `env_file`;
/// secrets from a file and from the environment, and a config of its own content; a
/// read-only root with a tmpfs; init; a hostname; a sidecar in another's network; replicas;
/// and a job in a profile, run once.
#[test]
fn a_compose_projects_services_get_what_it_says() {
    let Some(compose) = compose() else {
        eprintln!("SKIP: no docker compose on this host");
        return;
    };
    let Some((home, image)) = home("compose-rich") else {
        return;
    };
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("web.env"), "FROM_FILE=yes\n").unwrap();
    std::fs::write(project.join("token.txt"), "file-secret\n").unwrap();
    std::fs::write(
        project.join("compose.yaml"),
        format!(
            r#"name: rich
services:
  migrate:
    image: {image}
    command: ["exit", "0"]
  web:
    image: {image}
    command: ["serve", "8000", "1000"]
    depends_on:
      migrate:
        condition: service_completed_successfully
    environment:
      MODE: from-compose
    env_file: [web.env]
    secrets: [token, inline]
    configs: [settings]
    init: true
    hostname: webhost
    stop_grace_period: 2s
  vault:
    image: {image}
    command: ["sleep"]
    read_only: true
    tmpfs: [/scratch]
  sidecar:
    image: {image}
    command: ["sleep"]
    network_mode: "service:web"
  worker:
    image: {image}
    command: ["sleep"]
    deploy:
      replicas: 2
  job:
    image: {image}
    command: ["exit", "7"]
    profiles: [once]
secrets:
  token:
    file: ./token.txt
  inline:
    environment: SHP_TOKEN
configs:
  settings:
    content: |
      key=value
"#
        ),
    )
    .unwrap();
    let config = home.join("docker-config");
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let run = |args: &[&str]| {
        let out = common::output_within(
            std::process::Command::new(&compose)
                .current_dir(&project)
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env("SHP_TOKEN", "from-env")
                .env_remove("DOCKER_CONTEXT")
                .env_remove("COMPOSE_PROJECT_NAME")
                .env_remove("COMPOSE_FILE")
                .env_remove("COMPOSE_PROFILES")
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = run(&["up", "-d", "--wait", "--wait-timeout", "120"]);
    assert_eq!(status, Some(0), "{out}{err}");
    let (_, out, err) = run(&["ps", "-a", "--format", "{{.Service}} {{.State}}"]);
    let mut listed: Vec<&str> = out.lines().collect();
    listed.sort_unstable();
    assert_eq!(
        listed,
        [
            "migrate exited",
            "sidecar running",
            "vault running",
            "web running",
            "worker running",
            "worker running"
        ],
        "{out}{err}"
    );
    // Its environment, from the file and env_file; its hostname.
    let (status, out, err) = run(&["exec", "-T", "web", "/bin/testguest", "report"]);
    assert_eq!(status, Some(0), "{err}");
    for line in ["env MODE=from-compose", "env FROM_FILE=yes", "hostname webhost"] {
        assert!(out.lines().any(|l| l == line), "{line}: {out}");
    }
    // Its secrets and config, where Compose puts them.
    let (status, out, err) = run(&[
        "exec",
        "-T",
        "web",
        "/bin/testguest",
        "slurp",
        "/run/secrets/token",
        "/run/secrets/inline",
        "/settings",
    ]);
    assert_eq!(status, Some(0), "{err}");
    assert_eq!(
        out,
        "slurp /run/secrets/token: file-secret\nslurp /run/secrets/inline: from-env\nslurp /settings: key=value\n"
    );
    // A read-only root, a tmpfs that takes writes.
    let (status, _, err) = run(&[
        "exec",
        "-T",
        "vault",
        "/bin/testguest",
        "fs",
        "write:/scratch/x=1",
    ]);
    assert_eq!(status, Some(0), "{err}");
    let (status, _, _) = run(&["exec", "-T", "vault", "/bin/testguest", "fs", "write:/tmp/x=1"]);
    assert_ne!(status, Some(0));
    // The sidecar in web's network: web's server on its own loopback.
    let (status, out, err) = run(&["exec", "-T", "sidecar", "/bin/testguest", "ask", "127.0.0.1:8000"]);
    assert_eq!(status, Some(0), "{out}{err}");
    assert!(out.starts_with("ask from "), "{out}");
    // The job of a profile, run once: its status.
    let (status, _, err) = run(&["run", "--rm", "job"]);
    assert_eq!(status, Some(7), "{err}");
    let (status, out, err) = run(&["down", "-v", "--timeout", "1"]);
    assert_eq!(status, Some(0), "{out}{err}");
}

/// A service built from its Dockerfile, as Compose builds where the client's builder is
/// the API's first one (`DOCKER_BUILDKIT=0`); and `up` attached, its status the exit
/// code of the service it names. That one starts once the built one has ended well, so
/// the built one's output is all out before `up` ends with the other's.
#[test]
fn a_compose_service_is_built_and_run_attached() {
    let Some(compose) = compose() else {
        eprintln!("SKIP: no docker compose on this host");
        return;
    };
    let Some((home, image)) = home("compose-build") else {
        return;
    };
    let project = home.join("project");
    std::fs::create_dir_all(project.join("app")).unwrap();
    std::fs::write(
        project.join("app/Dockerfile"),
        format!("FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/built=yes\"]\n"),
    )
    .unwrap();
    std::fs::write(
        project.join("compose.yaml"),
        r#"name: built
services:
  app:
    build: ./app
    command: ["slurp", "/tmp/built"]
  done:
    image: built-app
    command: ["exit", "5"]
    depends_on:
      app:
        condition: service_completed_successfully
"#,
    )
    .unwrap();
    let config = home.join("docker-config");
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let run = |args: &[&str], set: &[(&str, &str)]| {
        let out = common::output_within(
            std::process::Command::new(&compose)
                .current_dir(&project)
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env_remove("DOCKER_CONTEXT")
                .env_remove("COMPOSE_PROJECT_NAME")
                .env_remove("COMPOSE_FILE")
                .envs(set.iter().copied())
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = run(&["build"], &[("DOCKER_BUILDKIT", "0")]);
    assert_eq!(status, Some(0), "{out}{err}");
    let (status, out, err) = run(
        &["up", "--no-build", "--exit-code-from", "done", "--no-color"],
        &[("DOCKER_BUILDKIT", "0")],
    );
    assert_eq!(status, Some(5), "{out}{err}");
    assert!(out.contains("slurp /tmp/built: yes"), "{out}{err}");
    let (status, out, err) = run(&["down", "--rmi", "local", "--timeout", "1"], &[]);
    assert_eq!(status, Some(0), "{out}{err}");
}
