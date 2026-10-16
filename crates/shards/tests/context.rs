//! Contexts (D180), end to end on this machine: a home whose daemon listens on loopback invites
//! another, which joins by the invitation's file and then runs commands there whole, chosen by
//! `--context`, `SHARDS_CONTEXT` or `use context`: what they print and their exit status cross,
//! a microVM's run among them. What a context does not carry is refused here; the invitation
//! enrolls one client; a client removed runs nothing more; and the machine's log says each
//! command its clients ran.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn a_context_runs_commands_on_its_machine_whole() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let machine = TempDir::new("ctx-machine");
    let laptop = TempDir::new("ctx-laptop");
    let stranger = TempDir::new("ctx-stranger");
    let on_machine = [
        ("SHARDS_HOME", machine.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let on_stranger = [("SHARDS_HOME", stranger.as_os_str())];

    // No listening daemon: no invitation.
    let invite = machine.join("laptop.invite");
    let early = run_shards_env(
        &["invite", "client"],
        &["--name", "laptop", "--output", invite.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_ne!(early.status, Some(0), "{early}");
    assert!(early.stderr.contains("does not listen for clients"), "{early}");

    let started = run_shards_env(
        &["daemon"],
        &["--detached", "--listen", "127.0.0.1:0"],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(started.status, Some(0), "{started}");
    let invited = run_shards_env(
        &["invite", "client"],
        &["--name", "laptop", "--output", invite.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&invite).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the invitation is private");
    }
    assert!(
        !invited.stdout.contains("secret") && !invited.stderr.contains("secret"),
        "{invited}"
    );

    let joined = run_shards_env(
        &["create", "context"],
        &["gpu", "--invite-file", invite.to_str().unwrap()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    assert_eq!(joined.stdout, "gpu\n");
    assert!(
        joined.stderr.contains("Successfully created context \"gpu\""),
        "{joined}"
    );
    // One client an invitation: the next join is refused, as any that fails is.
    let again = run_shards_env(
        &["create", "context"],
        &["gpu", "--invite-file", invite.to_str().unwrap()],
        &on_stranger,
        TIMEOUT,
    );
    assert_ne!(again.status, Some(0), "{again}");
    assert!(
        again
            .stderr
            .contains("this invitation is not one this machine holds"),
        "{again}"
    );

    // An invitation an hour old enrolls no one.
    let late = machine.join("late.invite");
    let made = run_shards_env(
        &["invite", "client"],
        &["--name", "late", "--output", late.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    let record = machine.join("invitations/late");
    let text = std::fs::read_to_string(&record).unwrap();
    let created: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("created "))
        .unwrap()
        .parse()
        .unwrap();
    let aged = text.replace(
        &format!("created {created}"),
        &format!("created {}", created - 3600),
    );
    std::fs::write(&record, aged).unwrap();
    let lapsed = run_shards_env(
        &["create", "context"],
        &["late", "--invite-file", late.to_str().unwrap()],
        &on_stranger,
        TIMEOUT,
    );
    assert_ne!(lapsed.status, Some(0), "{lapsed}");
    assert!(
        lapsed
            .stderr
            .contains("this invitation is not one this machine holds"),
        "{lapsed}"
    );

    // Another secret than the invitation's: refused, the invitation still open.
    let forged = machine.join("forged.invite");
    let made = run_shards_env(
        &["invite", "client"],
        &["--name", "forged", "--output", forged.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    let text = std::fs::read_to_string(&forged).unwrap();
    let secret = text
        .lines()
        .find_map(|l| l.strip_prefix("secret "))
        .unwrap()
        .to_string();
    let other = if secret.starts_with('0') { "1" } else { "0" };
    let altered = forged.with_extension("altered");
    std::fs::write(
        &altered,
        text.replace(
            &format!("secret {secret}"),
            &format!("secret {other}{}", secret.get(1..).unwrap()),
        ),
    )
    .unwrap();
    let refused = run_shards_env(
        &["create", "context"],
        &["forged", "--invite-file", altered.to_str().unwrap()],
        &on_stranger,
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(
        refused
            .stderr
            .contains("this invitation is not one this machine holds"),
        "{refused}"
    );
    assert!(
        machine.join("invitations/forged").is_file(),
        "the invitation still open"
    );

    // The machine's owner using a context of their own: its clients' commands run on it all
    // the same, never sent on.
    std::fs::create_dir_all(machine.join("contexts")).unwrap();
    std::fs::write(
        machine.join("contexts/away"),
        format!(
            "shards context 1\nname away\naddress 127.0.0.1:9\ndaemon {}\ndescription \n",
            "ab".repeat(32)
        ),
    )
    .unwrap();
    std::fs::write(machine.join("context"), "away\n").unwrap();
    let kept = run_shards_env(&["--context", "gpu", "ps"], &["-a"], &on_laptop, TIMEOUT);
    assert_eq!(kept.status, Some(0), "{kept}");
    std::fs::remove_file(machine.join("context")).unwrap();

    // A run there: its output and its status here.
    let ran = run_shards_env(
        &["--context", "gpu", "run"],
        &["--rm", image.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(ran.stdout.lines().any(|l| l == "uid 1000"), "{ran}");
    let exited = run_shards_env(
        &["--context", "gpu", "run"],
        &["--rm", image.as_str(), "exit", "3"],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(exited.status, Some(3), "{exited}");
    // The image is there, not here.
    let there = run_shards_env(
        &["images"],
        &["--format", "{{.Repository}}"],
        &[
            ("SHARDS_HOME", laptop.as_os_str()),
            ("SHARDS_CONTEXT", "gpu".as_ref()),
        ],
        TIMEOUT,
    );
    assert_eq!(there.status, Some(0), "{there}");
    assert!(there.stdout.contains("test/image"), "{there}");
    let here = run_shards_env(
        &["--context", "default", "images"],
        &["--format", "{{.Repository}}"],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(here.status, Some(0), "{here}");
    assert!(!here.stdout.contains("test/image"), "{here}");
    // Chosen for every command: `use context`.
    let used = run_shards_env(&["use", "context"], &["gpu"], &on_laptop, TIMEOUT);
    assert_eq!(used.status, Some(0), "{used}");
    let listed = run_shards_env(&["list", "contexts"], &[] as &[&str], &on_laptop, TIMEOUT);
    assert!(listed.stdout.lines().any(|l| l.starts_with("gpu *")), "{listed}");
    let ps = run_shards_env(&["ps"], &["-a"], &on_laptop, TIMEOUT);
    assert_eq!(ps.status, Some(0), "{ps}");
    assert!(ps.stdout.starts_with("CONTAINER ID"), "{ps}");

    // What a context does not carry yet: refused here, nothing run there.
    let terminal = run_shards_env(&["run"], &["-it", image.as_str()], &on_laptop, TIMEOUT);
    assert_eq!(terminal.status, Some(125), "{terminal}");
    assert!(
        terminal
            .stderr
            .contains("a terminal on another machine is not carried by a context yet"),
        "{terminal}"
    );
    let build = run_shards_env(&["build"], &["."], &on_laptop, TIMEOUT);
    assert_eq!(build.status, Some(125), "{build}");
    // The machine's own: not a client's.
    let owners = run_shards_env(&["stop", "daemon"], &[] as &[&str], &on_laptop, TIMEOUT);
    assert_ne!(owners.status, Some(0), "{owners}");
    assert!(
        owners
            .stderr
            .contains("this machine's owner's to run, not a client's"),
        "{owners}"
    );

    // Removed there, a client runs nothing more.
    let clients = run_shards_env(&["list", "clients"], &[] as &[&str], &on_machine, TIMEOUT);
    assert!(
        clients.stdout.lines().any(|l| l.starts_with("laptop\t")),
        "{clients}"
    );
    let removed = run_shards_env(&["remove", "client"], &["laptop"], &on_machine, TIMEOUT);
    assert_eq!(removed.status, Some(0), "{removed}");
    let refused = run_shards_env(&["ps"], &[] as &[&str], &on_laptop, TIMEOUT);
    assert_eq!(refused.status, Some(125), "{refused}");
    assert!(
        refused.stderr.contains("this machine has not enrolled you"),
        "{refused}"
    );
    let back = run_shards_env(&["use", "context"], &["default"], &on_laptop, TIMEOUT);
    assert_eq!(back.status, Some(0), "{back}");

    // The machine's log says what its client ran.
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    assert!(log.contains("client laptop joined"), "{log}");
    assert!(
        log.contains(&format!("client laptop: shards run --rm {image} exit 3")),
        "{log}"
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &on_machine, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}
