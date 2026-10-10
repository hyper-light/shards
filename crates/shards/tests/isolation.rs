//! What a workload reaches of the host from inside its microVM
//! (docs/architecture/AGENTFILE_ARCH.md §9.7).

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The guest reaches the host over vsock alone, and its init holds the host's ports, the
/// run and signal ports, before the workload starts: a workload that dials them, the
/// moment it starts, or any other host port, is refused. Before, a workload took a
/// second connection on the run port, and could take the signal port from init. Under
/// Docker's default seccomp profile (D42) it cannot make a vsock socket at all; with none
/// (`seccomp=unconfined`), the host refuses each connection.
#[test]
fn a_workload_reaches_no_host_port_over_vsock() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("isolation-vsock-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ports = [
        shards_abi::run::PORT,
        shards_abi::run::SIGNAL_PORT,
        1026,
        2000,
        52_000,
    ]
    .map(|p| p.to_string());
    for (opts, said) in [
        (&[][..], "socket Operation not permitted"),
        (&["--security-opt", "seccomp=unconfined"][..], "refused"),
    ] {
        let mut args = vec!["--rm"];
        args.extend_from_slice(opts);
        args.extend([image.as_str(), "vsock"]);
        args.extend(ports.iter().map(String::as_str));
        // The first run boots the image; the second restores its template.
        for _ in 0..2 {
            let ran = run_shards_env(&["run"], &args, &env, TIMEOUT);
            let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
            assert_eq!(ran.status, Some(0), "{shown}");
            let lines: Vec<&str> = ran.stdout.lines().collect();
            assert_eq!(lines.len(), ports.len(), "{shown}");
            for (line, port) in lines.iter().zip(&ports) {
                assert!(
                    line.starts_with(&format!("{port} {said}")),
                    "port {port} must refuse the workload ({opts:?}): {shown}"
                );
            }
        }
    }
    let _ = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
}

/// A local MCP server reaches what its caller reaches and nothing more (§9.6, §9.10): the
/// agent runs the command the in-VM server offers it, in its own confinement, so the
/// server reads its caller's files and never another agent's, though both are offered the
/// same server. A server is never a deputy holding more than its caller.
#[test]
fn a_local_mcp_server_reaches_only_what_its_caller_reaches() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("isolation-mcp-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let artifact = |kind: &str, name: &str, config: &str, secret: Option<&str>| {
        let dir = TempDir::new(&format!("isolation-mcp-{kind}-{name}"));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(common::test_guest(), dir.join("bin/testguest")).unwrap();
        std::fs::write(dir.join(format!("{kind}.json")), config).unwrap();
        if let Some(s) = secret {
            std::fs::write(dir.join("secret"), s).unwrap();
        }
        let tag = format!("127.0.0.1:{port}/team/isolation-mcp-{name}:1");
        let made = shards(&["build", kind, dir.to_str().unwrap(), "-t", &tag]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let pushed = shards(&["push", kind, &tag]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        tag
    };
    let caller = |name: &str| {
        artifact(
            "agent",
            name,
            &format!(
                r#"{{"name":"{name}","run":{{"command":["bin/testguest","confined","mcp-run","tools"]}}}}"#
            ),
            Some(&format!("{name}'s own\n")),
        )
    };
    let (a, b) = (caller("a"), caller("b"));
    let tools = artifact(
        "mcp",
        "tools",
        r#"{"name":"tools","run":{"command":["bin/testguest","slurp","/agents/a/secret","/agents/b/secret"]}}"#,
        None,
    );
    let ctx = TempDir::new("isolation-mcp-ctx");
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT a FROM {a}\nAGENT b FROM {b}\nMCP tools FROM {tools}\n"),
    )
    .unwrap();
    let built = shards(&["build", "-t", "isolation-mcp:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = shards(&["run", "--rm", "isolation-mcp:1", "await", "confined-ready", "2"]);
    let all = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.status, Some(0), "{all}");
    for want in [
        "[agent a] confined mcp-run tools: slurp /agents/a/secret: a's own",
        "[agent a] confined mcp-run tools: slurp /agents/b/secret: errno 13",
        "[agent b] confined mcp-run tools: slurp /agents/a/secret: errno 13",
        "[agent b] confined mcp-run tools: slurp /agents/b/secret: b's own",
    ] {
        assert!(all.lines().any(|l| l == want), "no {want:?} in\n{all}");
    }
}
