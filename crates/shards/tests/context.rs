//! Contexts (D180), end to end on this machine: a home whose daemon listens on loopback invites
//! another, which joins by the invitation's file and then runs commands there whole, chosen by
//! `--context`, `SHARDS_CONTEXT` or `use context`: what they print and their exit status cross,
//! a microVM's run among them. What a context does not carry is refused here; the invitation
//! enrolls one client; a client removed runs nothing more; and the machine's log says each
//! command its clients ran. And a terminal crosses as tests/tty.rs has one here: the client's
//! terminal raw while the command's own reads it, sized and resized, ^C and the detach keys
//! as they are; and without one, the signals the client receives reach the command.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::io::{BufRead, Read as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use common::terminal::{Terminal, cooked};
use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, run_shards_env_in, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// A home whose daemon listens for other machines' clients, stopped as this drops, as when a
/// test fails: a daemon that listens does not end when idle.
struct Listening(PathBuf);

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = run_shards_env(
            &["daemon"],
            &["stop"],
            &[("SHARDS_HOME", self.0.as_os_str())],
            TIMEOUT,
        );
    }
}

/// Two homes: a machine whose daemon listens on loopback, the test image pulled there, and a
/// laptop joined to it as context `gpu`.
fn joined(name: &str) -> Option<(TempDir, Listening, TempDir, String)> {
    // It says why it skips.
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let machine = TempDir::new(&format!("{name}-m"));
    let laptop = TempDir::new(&format!("{name}-l"));
    let on_machine = [
        ("SHARDS_HOME", machine.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    // The machine's owner keeps a secret in the daemon's environment, and their logins in
    // its docker config, as an owner's daemon has them.
    let owners = machine.join("docker");
    let mut daemons = on_machine.to_vec();
    daemons.push(("OWNERS_SECRET", "the owner's own".as_ref()));
    daemons.push(("DOCKER_CONFIG", owners.as_os_str()));
    let started = run_shards_env(
        &["daemon"],
        &["--detached", "--listen", "127.0.0.1:0"],
        &daemons,
        TIMEOUT,
    );
    assert_eq!(started.status, Some(0), "{started}");
    let listening = Listening(machine.to_path_buf());
    let invite = machine.join("laptop.invite");
    let invited = run_shards_env(
        &["invite", "client"],
        &["--name", "laptop", "--output", invite.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let joined = run_shards_env(
        &["create", "context"],
        &["gpu", "--invite-file", invite.to_str().unwrap()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let pulled = run_shards_env(
        &["--context", "gpu", "pull"],
        &["-q", image.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(pulled.status, Some(0), "{pulled}");
    Some((machine, listening, laptop, image))
}

/// `shards --context gpu run --pull never ARGS…` in `laptop` on a terminal of `rows` by
/// `cols`.
fn there(laptop: &Path, args: &[&str], rows: u16, cols: u16) -> Terminal {
    let mut all = vec!["--context", "gpu", "run", "--pull", "never"];
    all.extend_from_slice(args);
    Terminal::run(laptop, &all, rows, cols)
}

/// The client's terminal is the command's own there: raw here while it runs, the size it
/// is, what is typed reaching the command and echoed back, and given back as it was.
#[test]
fn a_terminal_through_a_context_is_the_commands_own() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-tty") else {
        return;
    };
    let mut term = there(&laptop, &["-it", &image, "tty", "read"], 30, 100);
    let shown = term.wait_for("ready\r\n");
    assert!(shown.contains("stdin tty true\r\n"), "{shown}");
    assert!(
        shown.contains("controlling true\r\nforeground true\r\n"),
        "{shown}"
    );
    assert!(shown.contains("size 30 100\r\n"), "{shown}");
    assert_eq!(
        cooked(&term.settings()),
        (false, false, false),
        "raw while it runs"
    );
    term.type_in(b"hello\r");
    let shown = term.wait_for("read hello\r\n");
    assert!(shown.contains("hello\r\n"), "echoed: {shown}");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
}

/// A resize of the client's terminal reaches the command there.
#[test]
fn a_resize_through_a_context_reaches_the_command() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-winch") else {
        return;
    };
    let mut term = there(&laptop, &["-it", &image, "tty", "winch"], 24, 80);
    let shown = term.wait_for("ready\r\n");
    assert!(shown.contains("size 24 80\r\n"), "{shown}");
    term.resize(40, 120);
    term.wait_for("resized 40 120\r\n");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
}

/// ^C and the detach keys do there what they do here (tests/tty.rs): the command
/// interrupted, or left running on its machine, the client gone.
#[test]
fn ctrl_c_and_the_detach_keys_cross_a_context() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-keys") else {
        return;
    };
    let mut term = there(&laptop, &["-it", "--init", &image, "sleep"], 24, 80);
    term.wait_for("ready\r\n");
    term.type_in(&[3]);
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (128 + libc::SIGINT, true));

    let mut term = there(&laptop, &["-it", "--name", "left", &image, "sleep"], 24, 80);
    term.wait_for("ready\r\n");
    term.type_in(&[16, 17]);
    let (shown, status, restored) = term.exit();
    assert_eq!((shown.as_str(), status, restored), ("", 0, true));
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let listed = run_shards_env(&["--context", "gpu", "ps"], &[] as &[&str], &on_laptop, TIMEOUT);
    assert!(listed.stdout.contains(" left\n"), "{listed}");
    let removed = run_shards_env(&["--context", "gpu", "rm"], &["-f", "left"], &on_laptop, TIMEOUT);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// `exec -it` and `attach` take the client's terminal there as `run -it` does: a command
/// exec'd in a running microVM on one of its own, and a client attached to a command's.
#[test]
fn exec_and_attach_take_the_terminal_through_a_context() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-exec") else {
        return;
    };
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let made = run_shards_env(
        &["--context", "gpu", "run"],
        &["--pull", "never", "-d", "--name", "box", &image, "sleep"],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    let mut term = Terminal::run(
        &laptop,
        &[
            "--context",
            "gpu",
            "exec",
            "-it",
            "box",
            "/bin/testguest",
            "tty",
            "read",
        ],
        28,
        90,
    );
    let shown = term.wait_for("ready\r\n");
    assert!(shown.contains("stdin tty true\r\n"), "{shown}");
    assert!(shown.contains("size 28 90\r\n"), "{shown}");
    term.type_in(b"inside\r");
    term.wait_for("read inside\r\n");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));

    let made = run_shards_env(
        &["--context", "gpu", "run"],
        &[
            "--pull", "never", "-d", "-it", "--name", "held", &image, "tty", "read",
        ],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    let mut term = Terminal::run(&laptop, &["--context", "gpu", "attach", "held"], 24, 80);
    term.type_in(b"again\r");
    term.wait_for("read again\r\n");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
    let removed = run_shards_env(
        &["--context", "gpu", "rm"],
        &["-f", "box", "held"],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// A client's command has the client's environment, as a command run here has this
/// machine's: a bare `-e NAME` its value here, and `--env-file` what the file here holds;
/// and nothing of the machine owner's, which a bare `-e NAME` there would have taken.
#[test]
fn a_clients_command_has_the_clients_environment_not_the_owners() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-env") else {
        return;
    };
    let file = laptop.join("vars.env");
    std::fs::write(&file, "FROM_FILE=here\nCLIENTS_TOO\n").unwrap();
    let ran = run_shards_env(
        &["--context", "gpu", "run"],
        &[
            "--pull",
            "never",
            "--rm",
            "-e",
            "OWNERS_SECRET",
            "-e",
            "CLIENTS_OWN",
            "--env-file",
            file.to_str().unwrap(),
            &image,
            "fs",
            "print:/proc/self/environ",
        ],
        &[
            ("SHARDS_HOME", laptop.as_os_str()),
            ("CLIENTS_OWN", "mine".as_ref()),
            ("CLIENTS_TOO", "also mine".as_ref()),
        ],
        TIMEOUT,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let vars: Vec<&str> = ran.stdout.split('\0').collect();
    for wanted in ["CLIENTS_OWN=mine", "FROM_FILE=here", "CLIENTS_TOO=also mine"] {
        assert!(vars.contains(&wanted), "{wanted}: {vars:?}");
    }
    assert!(!ran.stdout.contains("the owner's own"), "{vars:?}");
}

/// The archives a client saves, exports, loads and imports through a context are its own
/// files, as Docker's CLI writes and reads its own: an image saved there is written here,
/// in the client's directory, and loads there from here again; a microVM's files exported
/// there are written here, and import there from here as an image; an image's Agentfile is
/// written here. A save that fails leaves nothing here.
#[test]
fn a_clients_archives_are_its_own_files() {
    let Some((machine, _listening, laptop, image)) = joined("ctx-files") else {
        return;
    };
    let here = laptop.join("files");
    std::fs::create_dir_all(&here).unwrap();
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let there = |command: &[&str], args: &[&str]| {
        let mut words = vec!["--context", "gpu"];
        words.extend_from_slice(command);
        let ran = run_shards_env_in(&here, &words, args, &on_laptop, TIMEOUT);
        assert_eq!(ran.status, Some(0), "{command:?} {args:?}: {ran}");
        ran
    };
    let names = || {
        let mut names: Vec<String> = std::fs::read_dir(&here)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    // A tar's first header holds `ustar` at 257 (POSIX.1-2017, pax, ustar Interchange Format).
    let is_tar = |name: &str| {
        let bytes = std::fs::read(here.join(name)).unwrap();
        bytes.get(257..262) == Some(b"ustar".as_slice())
    };
    there(&["save", "image"], &[image.as_str(), "-o", "image.tar"]);
    assert!(is_tar("image.tar"));
    there(&["rmi"], &[image.as_str()]);
    let loaded = there(&["load"], &["-i", "image.tar"]);
    assert!(loaded.stdout.contains("Loaded image"), "{loaded}");
    there(&["image", "inspect"], &[image.as_str()]);
    there(&["create"], &["--name", "files", image.as_str()]);
    there(&["export", "vm"], &["files", "-o", "files.tar"]);
    assert!(is_tar("files.tar"));
    there(&["import"], &["files.tar", "imported:1"]);
    there(&["image", "inspect"], &["imported:1"]);
    there(&["make", "agentfile"], &[image.as_str(), "-o", "Agentfile"]);
    let agentfile = std::fs::read_to_string(here.join("Agentfile")).unwrap();
    assert!(agentfile.contains("\nFROM "), "{agentfile}");
    assert_eq!(names(), ["Agentfile", "files.tar", "image.tar"]);
    // Nothing of them there, where the command ran.
    for name in names() {
        assert!(!machine.join(&name).exists(), "{name} written there");
    }
    let failed = run_shards_env_in(
        &here,
        &["--context", "gpu", "save"],
        &["-o", "none.tar", "no-such-image:1"],
        &on_laptop,
        TIMEOUT,
    );
    assert_ne!(failed.status, Some(0), "{failed}");
    assert_eq!(names(), ["Agentfile", "files.tar", "image.tar"]);
    // From a terminal: the archive is the file's, not the command's terminal's there, which
    // a save refuses to write to, as Docker's does.
    let tty = here.join("tty.tar");
    let mut term = Terminal::run(
        &laptop,
        &[
            "--context",
            "gpu",
            "save",
            "-o",
            tty.to_str().unwrap(),
            image.as_str(),
        ],
        24,
        80,
    );
    let (shown, status, restored) = term.exit();
    assert_eq!(status, 0, "{shown}");
    assert!(restored);
    assert_eq!(
        std::fs::read(&tty).unwrap(),
        std::fs::read(here.join("image.tar")).unwrap()
    );
    // And loaded from one: the file is what the command reads, not the keyboard.
    there(&["rm"], &["files"]);
    there(&["rmi"], &[image.as_str()]);
    let mut term = Terminal::run(
        &laptop,
        &["--context", "gpu", "load", "-i", tty.to_str().unwrap()],
        24,
        80,
    );
    let (shown, status, restored) = term.exit();
    assert_eq!(status, 0, "{shown}");
    assert!(shown.contains("Loaded image"), "{shown}");
    assert!(restored);
    there(&["image", "inspect"], &[image.as_str()]);
}

/// A client's logins are its own, as Docker's CLI keeps its own: a registry the client logged
/// in to is reached there with the client's credentials, sent with the command; the machine
/// owner's reach nothing for a client, logged in there as they are; and a login and a
/// logout through the context are kept here, the owner's store untouched. Each command's
/// `PATH` is an empty directory and its docker config its own, so that no login reaches
/// this host's keychain.
#[test]
fn a_clients_logins_are_its_own() {
    use base64::Engine as _;
    let Some((machine, _listening, laptop, image)) = joined("ctx-login") else {
        return;
    };
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("pusher:secret")
    );
    let (port, _repos) = common::writable_registry_requiring(Some(basic));
    let registry = format!("127.0.0.1:{port}");
    let target = format!("{registry}/team/app:1");
    let empty = |home: &Path| {
        let dir = home.join("path");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let (owners, owners_path) = (machine.join("docker"), empty(&machine));
    let on_machine = [
        ("SHARDS_HOME", machine.as_os_str()),
        ("DOCKER_CONFIG", owners.as_os_str()),
        ("PATH", owners_path.as_os_str()),
    ];
    let (laptops, laptops_path) = (laptop.join("docker"), empty(&laptop));
    let on_laptop = [
        ("SHARDS_HOME", laptop.as_os_str()),
        ("DOCKER_CONFIG", laptops.as_os_str()),
        ("PATH", laptops_path.as_os_str()),
    ];
    // The owner logs in on the machine, and pushes the image there.
    let login = ["-u", "pusher", "-p", "secret", registry.as_str()];
    let owner_in = run_shards_env(&["login"], &login, &on_machine, TIMEOUT);
    assert_eq!(owner_in.status, Some(0), "{owner_in}");
    let tagged = run_shards_env(&["tag"], &[image.as_str(), target.as_str()], &on_machine, TIMEOUT);
    assert_eq!(tagged.status, Some(0), "{tagged}");
    let pushed = run_shards_env(&["push"], &[target.as_str()], &on_machine, TIMEOUT);
    assert_eq!(pushed.status, Some(0), "{pushed}");
    let owners_kept = std::fs::read(owners.join("config.json")).unwrap();
    // The client, logged in nowhere: refused, though the owner is logged in there.
    let refused = run_shards_env(
        &["--context", "gpu", "pull"],
        &["-q", target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0), "{refused}");
    // Logged in through the context: kept here, the owner's store as it was.
    let client_in = run_shards_env(&["--context", "gpu", "login"], &login, &on_laptop, TIMEOUT);
    assert_eq!(client_in.status, Some(0), "{client_in}");
    let kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(laptops.join("config.json")).unwrap()).unwrap();
    assert!(kept["auths"].get(registry.as_str()).is_some(), "{kept}");
    assert_eq!(std::fs::read(owners.join("config.json")).unwrap(), owners_kept);
    // Pulled there with the client's own credentials.
    let pulled = run_shards_env(
        &["--context", "gpu", "pull"],
        &["-q", target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Logged out here: the next pull there is refused again.
    let client_out = run_shards_env(
        &["--context", "gpu", "logout"],
        &[registry.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(client_out.status, Some(0), "{client_out}");
    assert_eq!(std::fs::read(owners.join("config.json")).unwrap(), owners_kept);
    let gone = run_shards_env(
        &["--context", "gpu", "rmi"],
        &[target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(gone.status, Some(0), "{gone}");
    let again = run_shards_env(
        &["--context", "gpu", "pull"],
        &["-q", target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_ne!(again.status, Some(0), "{again}");
}

/// With no terminal, a signal the client receives reaches the command there, by the number
/// Linux gives it, as `docker run`'s signal proxy sends a container its own: SIGUSR1 is 30
/// on macOS, 10 to the guest.
#[test]
fn a_signal_to_the_client_reaches_the_command_there() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-sig") else {
        return;
    };
    for (name, host, linux) in [("INT", libc::SIGINT, 2), ("USR1", libc::SIGUSR1, 10)] {
        let mut child = common::command()
            .args(["--context", "gpu", "run", "--pull", "never", &image, "trap", name])
            .env("SHARDS_HOME", laptop.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A regression fails the test instead of hanging it.
        let pid = child.id() as libc::pid_t;
        let (done, watch) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if watch.recv_timeout(TIMEOUT).is_err() {
                // SAFETY: kill(2) of our own child, which is only reaped after `done`.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        out.read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n", "{name}");
        // SAFETY: kill(2) of our own child.
        unsafe { libc::kill(pid, host) };
        let mut rest = String::new();
        out.read_to_string(&mut rest).unwrap();
        let mut stderr = String::new();
        child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        let status = child.wait().unwrap().code();
        let _ = done.send(());
        assert_eq!(
            (status, rest.as_str()),
            (Some(0), format!("got {linux}\n").as_str()),
            "{name}: {stderr}"
        );
    }
}

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

    // What a context does not carry yet: refused here, nothing run there. And `-it` with no
    // terminal here is refused there, in the words a run here would say.
    let terminal = run_shards_env(&["run"], &["-it", image.as_str()], &on_laptop, TIMEOUT);
    assert_eq!(terminal.status, Some(1), "{terminal}");
    assert!(
        terminal
            .stderr
            .contains("-it: stdin is not a terminal, so it cannot be the command's"),
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
