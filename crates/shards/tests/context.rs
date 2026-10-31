//! Contexts (D180), end to end on this machine: a home whose daemon listens on loopback invites
//! another, which joins by the invitation's file and then runs commands there whole, chosen by
//! `--context`, `SHARDS_CONTEXT` or `use context`: what they print and their exit status cross,
//! a microVM's run among them. What a context does not carry is refused here; the invitation
//! enrolls one client; a client removed runs nothing more; and the machine's log says each
//! command its clients ran. And a terminal crosses as tests/tty.rs has one here: the client's
//! terminal raw while the command's own reads it, sized and resized, ^C and the detach keys
//! as they are; and without one, the signals the client receives reach the command.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::io::{BufRead, Read as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use common::terminal::{Terminal, cooked};
use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, run_shards_env_in, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// SIGINT, as Linux numbers it, which a client ended by an interrupt exits by (128 + it), on
/// every host: Windows' clients exit by Linux's numbers.
const SIGINT: i32 = 2;

/// A connection to the engine served at `socket`: its Unix socket; on Windows the named pipe
/// the file there names.
#[cfg(unix)]
fn connect(socket: &Path) -> std::os::unix::net::UnixStream {
    let conn = std::os::unix::net::UnixStream::connect(socket).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    conn
}

#[cfg(windows)]
fn connect(socket: &Path) -> std::fs::File {
    let named = std::fs::read_to_string(socket).unwrap();
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(named.trim())
        .unwrap()
}

/// What `DOCKER_HOST` names the engine served at `socket` by: its socket; on Windows its
/// pipe, which the file there names.
fn docker_host(socket: &Path) -> String {
    #[cfg(unix)]
    return format!("unix://{}", socket.display());
    #[cfg(windows)]
    return std::fs::read_to_string(socket)
        .unwrap()
        .trim()
        .strip_prefix(r"\\.\pipe\")
        .map(|pipe| format!("npipe:////./pipe/{pipe}"))
        .unwrap();
}

/// Where a home's own engine is served, in it: its socket; on Windows the file that names
/// its pipe.
const OWN_ENGINE: &str = if cfg!(windows) {
    "engine.pipe"
} else {
    "engine.sock"
};

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
    assert_eq!((status, restored), (128 + SIGINT, true));

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
    // A seccomp profile's file here confines the command there: mkdir refused. Named from
    // the client's directory, which the command there does not run in.
    std::fs::write(
        laptop.join("deny-mkdir.json"),
        r#"{ "defaultAction": "SCMP_ACT_ALLOW",
             "syscalls": [ { "names": ["mkdir", "mkdirat"], "action": "SCMP_ACT_ERRNO" } ] }"#,
    )
    .unwrap();
    let confined = common::run_shards_env_in(
        &laptop,
        &["--context", "gpu", "run"],
        &[
            "--pull",
            "never",
            "--rm",
            "--security-opt",
            "seccomp=deny-mkdir.json",
            &image,
            "fs",
            "mkdir:/made",
        ],
        &[("SHARDS_HOME", laptop.as_os_str())],
        TIMEOUT,
    );
    assert_ne!(confined.status, Some(0), "{confined}");
    assert!(confined.stderr.contains("Operation not permitted"), "{confined}");
    // A profile not here is refused here, as a run here refuses it.
    let missing = common::run_shards_env_in(
        &laptop,
        &["--context", "gpu", "run"],
        &["--rm", "--security-opt", "seccomp=gone.json", &image, "true"],
        &[("SHARDS_HOME", laptop.as_os_str())],
        TIMEOUT,
    );
    assert_eq!(missing.status, Some(125), "{missing}");
    assert!(
        missing
            .stderr
            .starts_with("shards: opening seccomp profile (gone.json) failed: open gone.json: no such file or directory\n\nRun 'shards run --help'"),
        "{missing}"
    );
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
    // A pull's Agentfile, asked of the machine once the pull is through, into a directory
    // here as its `Agentfile`.
    std::fs::create_dir(here.join("pulled")).unwrap();
    there(&["pull"], &["-q", "--output-agentfile", "pulled", image.as_str()]);
    assert_eq!(
        std::fs::read_to_string(here.join("pulled/Agentfile")).unwrap(),
        agentfile
    );
    assert!(!machine.join("pulled").exists());
}

/// A build through a context is of the client's context, kept in step on the machine (D180
/// part 3c): the first build sends the context whole, as its ignore file leaves it; the next,
/// none of its files; one after a change, that change alone; a file removed here is gone
/// there. Each image is the context's as it was. What came is the machine's daemon's log's.
#[test]
fn a_build_through_a_context_is_of_the_clients_context_kept_in_step() {
    let Some((machine, _listening, laptop, image)) = joined("ctx-build") else {
        return;
    };
    let app = laptop.join("app");
    std::fs::create_dir_all(app.join("sub")).unwrap();
    std::fs::write(app.join("Dockerfile"), format!("FROM {image}\nCOPY . /app\n")).unwrap();
    std::fs::write(app.join(".dockerignore"), "ignored\n").unwrap();
    std::fs::write(app.join("a.txt"), "one").unwrap();
    std::fs::write(app.join("sub/b.txt"), "two").unwrap();
    std::fs::write(app.join("ignored"), "never sent").unwrap();
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let shards = |command: &[&str], args: &[&str]| {
        let mut words = vec!["--context", "gpu"];
        words.extend_from_slice(command);
        run_shards_env_in(&laptop, &words, args, &on_laptop, TIMEOUT)
    };
    // What the machine's daemon last said came of a context's files.
    let came = || -> u64 {
        let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap_or_default();
        let line = log
            .lines()
            .rev()
            .find(|l| l.contains(" bytes of files came"))
            .unwrap_or_else(|| panic!("no context came: {log}"))
            .to_string();
        line.rsplit_once(": ")
            .and_then(|(_, n)| n.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{line}"))
    };
    let built = |tag: &str| {
        let built = shards(&["build"], &["--progress=plain", "-t", tag, "app"]);
        assert_eq!(built.status, Some(0), "{built}");
    };
    let read = |tag: &str, path: &str| {
        let ran = shards(&["run"], &["--rm", tag, "fs", &format!("print:{path}")]);
        (ran.status, ran.stdout)
    };
    built("app:1");
    // Each file the build takes: Dockerfile, .dockerignore, a.txt and sub/b.txt.
    let whole = (format!("FROM {image}\nCOPY . /app\n").len() + "ignored\n".len() + 3 + 3) as u64;
    assert_eq!(came(), whole);
    assert_eq!(read("app:1", "/app/a.txt"), (Some(0), "one".into()));
    assert_ne!(read("app:1", "/app/ignored").0, Some(0), "an ignored file went");
    // Nothing changed: none of its files.
    built("app:2");
    assert_eq!(came(), 0);
    // A change: those files alone. Rewritten in place, `sub` is as it was here, and is so
    // there, though its file was replaced there.
    std::fs::write(app.join("a.txt"), "uno!").unwrap();
    std::fs::write(app.join("sub/b.txt"), "dos").unwrap();
    built("app:3");
    assert_eq!(came(), 7);
    assert_eq!(read("app:3", "/app/a.txt"), (Some(0), "uno!".into()));
    assert_eq!(read("app:3", "/app/sub/b.txt"), (Some(0), "dos".into()));
    let copy_of = || {
        let copies: Vec<PathBuf> = std::fs::read_dir(machine.join("synced/laptop"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let [copy] = copies.as_slice() else {
            panic!("one copy of one context: {copies:?}");
        };
        copy.clone()
    };
    let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
    assert_eq!(mtime(&copy_of().join("context/sub")), mtime(&app.join("sub")));
    // A build file outside the context goes with it.
    std::fs::create_dir_all(laptop.join("elsewhere")).unwrap();
    std::fs::write(
        laptop.join("elsewhere/Containerfile"),
        format!("FROM {image}\nLABEL from=elsewhere\n"),
    )
    .unwrap();
    let outside = shards(
        &["build"],
        &[
            "--progress=plain",
            "-f",
            "elsewhere/Containerfile",
            "-t",
            "app:outside",
            "app",
        ],
    );
    assert_eq!(outside.status, Some(0), "{outside}");
    // Its context is the same one, kept in step: none of its files came.
    assert_eq!(came(), 0);
    let label = shards(
        &["image", "inspect"],
        &["--format", "{{index .Config.Labels \"from\"}}", "app:outside"],
    );
    assert_eq!(label.stdout.trim(), "elsewhere", "{label}");
    // A file removed here is gone there, and its directory's time is the client's.
    std::fs::remove_file(app.join("sub/b.txt")).unwrap();
    built("app:4");
    assert_eq!(came(), 0);
    assert_ne!(
        read("app:4", "/app/sub/b.txt").0,
        Some(0),
        "a removed file stayed"
    );
    assert_eq!(read("app:2", "/app/sub/b.txt"), (Some(0), "two".into()));
    let copy = copy_of();
    assert_eq!(mtime(&copy.join("context/sub")), mtime(&app.join("sub")));
    assert!(
        !copy.join("context/ignored").exists(),
        "an ignored file kept there"
    );
    // A build arg named alone takes this client's value, as buildx takes it where it runs.
    std::fs::write(
        app.join("Dockerfile"),
        format!("FROM {image}\nARG MSG\nLABEL msg=$MSG\n"),
    )
    .unwrap();
    let labelled = run_shards_env_in(
        &laptop,
        &["--context", "gpu", "build"],
        &["--progress=plain", "--build-arg", "MSG", "-t", "app:5", "app"],
        &[
            ("SHARDS_HOME", laptop.as_os_str()),
            ("MSG", "from the client".as_ref()),
        ],
        TIMEOUT,
    );
    assert_eq!(labelled.status, Some(0), "{labelled}");
    let label = shards(
        &["image", "inspect"],
        &["--format", "{{index .Config.Labels \"msg\"}}", "app:5"],
    );
    assert_eq!(label.stdout.trim(), "from the client", "{label}");
    // A client removed takes the copies of its contexts with it.
    let removed = run_shards_env(
        &["remove", "client"],
        &["laptop"],
        &[("SHARDS_HOME", machine.as_os_str())],
        TIMEOUT,
    );
    assert_eq!(removed.status, Some(0), "{removed}");
    assert!(!machine.join("synced/laptop").exists(), "its copies kept");
}

/// What a client of Docker's API reads of `path` on the engine socket `socket`, on a
/// connection of its own: the answer's status, and its body, a length's or a chunked one's.
fn engine_get(socket: &Path, path: &str) -> (u16, String) {
    use std::io::{BufRead as _, Write as _};
    let mut conn = connect(socket);
    write!(conn, "GET {path} HTTP/1.1\r\nHost: api\r\n\r\n").unwrap();
    let mut reader = std::io::BufReader::new(conn);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut fields = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        fields.push(line.to_ascii_lowercase());
    }
    let mut body = Vec::new();
    if fields.iter().any(|f| f == "transfer-encoding: chunked") {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let n = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0u8; n + 2];
            reader.read_exact(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(n) = fields.iter().find_map(|f| f.strip_prefix("content-length: ")) {
        body = vec![0u8; n.trim().parse().unwrap()];
        reader.read_exact(&mut body).unwrap();
    }
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Docker's tools through a context (D180 part 4): the client's daemon serves the context's
/// engine at the socket `inspect context` names, and what is asked there is the machine's
/// Engine API's: a microVM made there through the context is listed there, and not by the
/// client's own engine; and the context's removal takes its socket with it.
#[test]
fn a_contexts_engine_is_its_machines() {
    let Some((machine, _listening, laptop, image)) = joined("ce") else {
        return;
    };
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let started = run_shards_env(&["daemon"], &["--detached"], &on_laptop, TIMEOUT);
    assert_eq!(started.status, Some(0), "{started}");
    let _laptops = Listening(laptop.to_path_buf());
    let inspected = run_shards_env(&["inspect", "context"], &["gpu"], &on_laptop, TIMEOUT);
    assert_eq!(inspected.status, Some(0), "{inspected}");
    let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
    // On Windows the file of the home that names the pipe, which the endpoint names once the
    // context's engine is served, and its own path until then.
    #[cfg(unix)]
    let engine = PathBuf::from(
        doc[0]["Engine"]
            .as_str()
            .and_then(|e| e.strip_prefix("unix://"))
            .unwrap_or_else(|| panic!("{doc}")),
    );
    #[cfg(windows)]
    let engine = {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(b"gpu");
        let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        laptop.join("e").join(format!("{short}.sock"))
    };
    // Served as the daemon starts, for the contexts its home joined.
    let served = |engine: &Path| {
        let start = std::time::Instant::now();
        while !engine.exists() {
            assert!(start.elapsed() < TIMEOUT, "{} never served", engine.display());
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    served(&engine);
    // On Windows, once served, the endpoint names its pipe, as Docker's clients write one.
    #[cfg(windows)]
    {
        let _ = doc;
        let inspected = run_shards_env(&["inspect", "context"], &["gpu"], &on_laptop, TIMEOUT);
        let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
        assert_eq!(
            doc[0]["Engine"].as_str(),
            Some(docker_host(&engine).as_str()),
            "{doc}"
        );
    }
    assert_eq!(engine_get(&engine, "/_ping"), (200, "OK".into()));
    // A client that reads to the end, as one asking to close does: the API's end of its
    // answer is the client's end of reading, through the relay.
    {
        use std::io::{Read as _, Write as _};
        let mut conn = connect(&engine);
        conn.write_all(b"GET /_ping HTTP/1.1\r\nHost: api\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut answer = String::new();
        conn.read_to_string(&mut answer).unwrap();
        assert!(
            answer.starts_with("HTTP/1.1 200 OK\r\n") && answer.ends_with("OK"),
            "{answer:?}"
        );
    }
    let made = run_shards_env(
        &["--context", "gpu", "create"],
        &["--name", "relayed", image.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    let (status, there) = engine_get(&engine, "/containers/json?all=1");
    assert_eq!(status, 200, "{there}");
    assert!(there.contains("\"/relayed\""), "{there}");
    let (status, here) = engine_get(&laptop.join(OWN_ENGINE), "/containers/json?all=1");
    assert_eq!(status, 200, "{here}");
    assert!(!here.contains("relayed"), "{here}");
    // Docker's own CLI, where this host has one: its listing, and a run, whose attach takes
    // the connection over for the microVM's streams. Its config its own, no credential
    // helper on its PATH.
    let docker = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(format!("docker{}", std::env::consts::EXE_SUFFIX)))
        .find(|p| p.is_file());
    if docker.is_none() {
        eprintln!("no docker CLI on this host: its part not run");
    }
    let (config, no_helpers) = (laptop.join("docker-config"), laptop.join("no-helpers"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&no_helpers).unwrap();
    // Named as it is now: on Windows each pipe the context's engine is served at again is
    // a new name.
    let docker = |args: &[&str], stdin: Stdio| {
        docker.as_ref().map(|docker| {
            let out = common::output_within(
                std::process::Command::new(docker)
                    .args(args)
                    .env("DOCKER_HOST", docker_host(&engine))
                    .env("DOCKER_CONFIG", &config)
                    .env("PATH", &no_helpers)
                    .env_remove("DOCKER_CONTEXT")
                    .env("DOCKER_CLI_HINTS", "false")
                    .stdin(stdin),
                TIMEOUT,
            );
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        })
    };
    if let Some((status, out, err)) = docker(&["ps", "-a", "--format", "{{.Names}}"], Stdio::null()) {
        assert_eq!(status, Some(0), "{err}");
        assert!(out.lines().any(|l| l == "relayed"), "{out}");
    }
    let ran = docker(
        &["run", "--rm", image.as_str(), "fs", "print:/etc/hostname"],
        Stdio::null(),
    );
    if let Some((status, out, err)) = ran {
        assert_eq!(status, Some(0), "{err}");
        assert!(!out.trim().is_empty(), "{out}");
    }
    // Its stdin, to its end: the end crosses as the attach's half-close.
    let input = laptop.join("input");
    std::fs::write(&input, "relayed through\n").unwrap();
    let catted = docker(
        &["run", "-i", "--rm", image.as_str(), "cat"],
        Stdio::from(std::fs::File::open(&input).unwrap()),
    );
    if let Some((status, out, err)) = catted {
        assert_eq!((status, out.as_str()), (Some(0), "relayed through\n"), "{err}");
    }
    let removed = run_shards_env(&["remove", "context"], &["gpu"], &on_laptop, TIMEOUT);
    assert_eq!(removed.status, Some(0), "{removed}");
    assert!(!engine.exists(), "its engine's socket kept");
    // Joined while the daemon runs: served as the daemon sees it come, no command asked of
    // it.
    let on_machine = [("SHARDS_HOME", machine.as_os_str())];
    let invite = machine.join("again.invite");
    let invited = run_shards_env(
        &["invite", "client"],
        &["--name", "again", "--output", invite.to_str().unwrap()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    let rejoined = run_shards_env(
        &["create", "context"],
        &["gpu", "--invite-file", invite.to_str().unwrap()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(rejoined.status, Some(0), "{rejoined}");
    served(&engine);
    assert_eq!(engine_get(&engine, "/_ping"), (200, "OK".into()));
    // Joined again as it was enrolled: one client of its key, the invitation redeemed.
    let clients = run_shards_env(&["list", "clients"], &[] as &[&str], &on_machine, TIMEOUT);
    let named: Vec<&str> = clients
        .stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(named, ["laptop"], "{clients}");
    assert!(!machine.join("invitations/again").exists(), "the invitation kept");
    // A socket someone removed is served again as the daemon next looks.
    std::fs::remove_file(&engine).unwrap();
    let listed = run_shards_env(&["ps"], &["-a"], &on_laptop, TIMEOUT);
    assert_eq!(listed.status, Some(0), "{listed}");
    served(&engine);
    assert_eq!(engine_get(&engine, "/_ping"), (200, "OK".into()));
    // A stream held open through it ends as the daemon stops, which takes the contexts'
    // sockets with it as it closes: an attach to a microVM there, which the end of its
    // client's writing would not end.
    let held = run_shards_env(
        &["--context", "gpu", "run"],
        &["-d", "--name", "held", image.as_str(), "sleep"],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(held.status, Some(0), "{held}");
    {
        use std::io::{BufRead as _, Read as _, Write as _};
        let attach = connect(&engine);
        (&attach)
            .write_all(
                b"POST /containers/held/attach?stream=1&stdout=1&stderr=1 HTTP/1.1\r\nHost: api\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
            )
            .unwrap();
        let mut reader = std::io::BufReader::new(&attach);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 101 "), "{status:?}");
        let stopped = run_shards_env(&["stop", "daemon"], &[] as &[&str], &on_laptop, TIMEOUT);
        assert_eq!(stopped.status, Some(0), "{stopped}");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(!engine.exists(), "its engine's socket kept by a daemon gone");
    }
    let removed = run_shards_env(&["--context", "gpu", "rm"], &["-f", "held"], &on_laptop, TIMEOUT);
    assert_eq!(removed.status, Some(0), "{removed}");
    // Served again by the next daemon; and a machine that refuses the client is said so
    // there, as the API says an error, which Docker's clients print.
    let started = run_shards_env(&["daemon"], &["--detached"], &on_laptop, TIMEOUT);
    assert_eq!(started.status, Some(0), "{started}");
    served(&engine);
    let gone = run_shards_env(&["remove", "client"], &["laptop"], &on_machine, TIMEOUT);
    assert_eq!(gone.status, Some(0), "{gone}");
    let (status, said) = engine_get(&engine, "/_ping");
    assert_eq!(status, 502, "{said}");
    let said: serde_json::Value = serde_json::from_str(&said).unwrap();
    assert_eq!(
        said["message"].as_str(),
        Some("context gpu: this machine has not enrolled you: join it by an invitation first"),
        "{said}"
    );
    if let Some((status, _, err)) = docker(&["ps"], Stdio::null()) {
        assert_eq!(status, Some(1), "{err}");
        assert!(
            err.contains("Error response from daemon: context gpu: this machine has not enrolled you"),
            "{err}"
        );
    }
}

/// A copy through a context is the client's, as Docker's CLI copies its own files (D180
/// part 3b): driven here, as this machine's CLI drives one (cli/cp.rs), each step asked of
/// the machine's daemon. A directory here goes into a microVM there and comes back as it was,
/// relative paths the client's; a file into a directory there is said with its sizes; an
/// archive goes to stdout; a file not there is said in Docker's words; and nothing is
/// written where the command runs.
#[test]
fn a_microvms_files_are_copied_to_and_from_the_client() {
    let Some((machine, _listening, laptop, image)) = joined("ctx-copy") else {
        return;
    };
    let here = laptop.join("files");
    std::fs::create_dir_all(here.join("src/sub")).unwrap();
    std::fs::write(here.join("src/a.txt"), "one").unwrap();
    std::fs::write(here.join("src/sub/b.txt"), "two").unwrap();
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let there = |command: &[&str], args: &[&str]| {
        let mut words = vec!["--context", "gpu"];
        words.extend_from_slice(command);
        run_shards_env_in(&here, &words, args, &on_laptop, TIMEOUT)
    };
    let started = there(
        &["run"],
        &["-d", "--name", "copier", "-u", "0", image.as_str(), "sleep"],
    );
    assert_eq!(started.status, Some(0), "{started}");
    let into = there(&["copy", "vm"], &["src", "copier:/in"]);
    assert_eq!(
        (into.status, into.stdout.as_str(), into.stderr.as_str()),
        (Some(0), "", ""),
        "{into}"
    );
    let out = there(&["cp"], &["copier:/in", "back"]);
    assert_eq!(out.status, Some(0), "{out}");
    assert_eq!(std::fs::read_to_string(here.join("back/a.txt")).unwrap(), "one");
    assert_eq!(
        std::fs::read_to_string(here.join("back/sub/b.txt")).unwrap(),
        "two"
    );
    let said = there(&["cp"], &["-q=false", "src/a.txt", "copier:/in/sub"]);
    assert_eq!(
        (said.status, said.stderr.as_str()),
        (
            Some(0),
            "Successfully copied 3B (transferred 2.05kB) to copier:/in/sub\n"
        ),
        "{said}"
    );
    let streamed = there(&["cp"], &["copier:/in/sub/a.txt", "-"]);
    assert_eq!(streamed.status, Some(0), "{streamed}");
    assert!(
        streamed.stdout.contains("a.txt\0") && streamed.stdout.contains("one"),
        "{streamed}"
    );
    let missing = there(&["cp"], &["copier:/nope", "x"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(1),
            "shards: Could not find the file /nope in container copier\n"
        ),
        "{missing}"
    );
    assert!(!machine.join("back").exists(), "copied where the command runs");
    let removed = there(&["rm"], &["-f", "copier"]);
    assert_eq!(removed.status, Some(0), "{removed}");
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
    // And builds an agent there, to push.
    let agent_dir = machine.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("run.sh"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(
        agent_dir.join("agent.json"),
        r#"{"name":"main","version":"1.0.0"}"#,
    )
    .unwrap();
    let agent = format!("{registry}/team/agent:1");
    let built = run_shards_env(
        &["build", "agent"],
        &[agent_dir.to_str().unwrap(), "-t", agent.as_str()],
        &on_machine,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let owners_kept = std::fs::read(owners.join("config.json")).unwrap();
    // The client, logged in nowhere: refused, though the owner is logged in there.
    let refused = run_shards_env(
        &["--context", "gpu", "pull"],
        &["-q", target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0), "{refused}");
    let agent_refused = run_shards_env(
        &["--context", "gpu", "push", "agent"],
        &[agent.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_ne!(agent_refused.status, Some(0), "{agent_refused}");
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
    // As shards' own commands reach it: an agent pushed, and an image's Agentfile made of
    // a pull, the image gone there first.
    let agent_pushed = run_shards_env(
        &["--context", "gpu", "push", "agent"],
        &[agent.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(agent_pushed.status, Some(0), "{agent_pushed}");
    let removed = run_shards_env(
        &["--context", "gpu", "rmi"],
        &[target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(removed.status, Some(0), "{removed}");
    let made = run_shards_env(
        &["--context", "gpu", "make", "agentfile"],
        &[target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(made.status, Some(0), "{made}");
    assert!(
        made.stdout.contains(&format!("\nFROM {target}@sha256:")),
        "{made}"
    );
    // And a build of a base there, its plan's registries' logins sent with it.
    let app = laptop.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("Dockerfile"), format!("FROM {target}\n")).unwrap();
    let removed = run_shards_env(
        &["--context", "gpu", "rmi"],
        &[target.as_str()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(removed.status, Some(0), "{removed}");
    let built = run_shards_env(
        &["--context", "gpu", "build"],
        &["--progress=plain", "-t", "on-a-login:1", app.to_str().unwrap()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{built}");
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
/// on macOS, 10 to the guest. On Windows, which sends a process no signal but its console's
/// events, the client's interrupt, Ctrl-Break to its process group, is SIGINT there.
#[test]
fn a_signal_to_the_client_reaches_the_command_there() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-sig") else {
        return;
    };
    #[cfg(unix)]
    let signals = [("INT", libc::SIGINT, 2), ("USR1", libc::SIGUSR1, 10)];
    #[cfg(windows)]
    let signals = [("INT", 0, 2)];
    for (name, host, linux) in signals {
        let mut command = common::command();
        command
            .args(["--context", "gpu", "run", "--pull", "never", &image, "trap", name])
            .env("SHARDS_HOME", laptop.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = common::interruptible(&mut command).spawn().unwrap();
        // A regression fails the test instead of hanging it.
        let ender = Ender::of(&child);
        let (done, watch) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if watch.recv_timeout(TIMEOUT).is_err() {
                ender.end();
            }
        });
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        out.read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n", "{name}");
        signal(&child, host);
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

/// What ends a test's child that outlives its time, from another thread: its process.
struct Ender {
    #[cfg(unix)]
    pid: libc::pid_t,
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
}

impl Ender {
    fn of(child: &std::process::Child) -> Ender {
        #[cfg(unix)]
        return Ender {
            pid: libc::pid_t::try_from(child.id()).unwrap(),
        };
        #[cfg(windows)]
        return Ender {
            process: std::os::windows::io::AsHandle::as_handle(child)
                .try_clone_to_owned()
                .unwrap(),
        };
    }

    fn end(&self) {
        // SAFETY: kill(2) of our own child, which is only reaped once the test is done.
        #[cfg(unix)]
        unsafe {
            libc::kill(self.pid, libc::SIGKILL)
        };
        // SAFETY: a handle of the child's process, which this holds open.
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(
                std::os::windows::io::AsRawHandle::as_raw_handle(&self.process),
                1,
            )
        };
    }
}

/// Sends `child`, started [`common::interruptible`], signal `sig`; on Windows its
/// interrupt ([`common::interrupt`]), the one event a process sends another's group.
fn signal(child: &std::process::Child, sig: i32) {
    #[cfg(unix)]
    // SAFETY: kill(2) of our own child.
    unsafe {
        libc::kill(libc::pid_t::try_from(child.id()).unwrap(), sig)
    };
    #[cfg(windows)]
    {
        let _ = sig;
        common::interrupt(child);
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&invite).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the invitation is private");
        }
        #[cfg(windows)]
        assert!(common::private_to_its_user(&invite), "the invitation is private");
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
    let policy = run_shards_env(&["buildx", "policy", "eval"], &["alpine"], &on_laptop, TIMEOUT);
    assert_eq!(policy.status, Some(125), "{policy}");
    assert!(
        policy
            .stderr
            .contains("buildx policy eval over a context reads this machine's files"),
        "{policy}"
    );
    // A build of a context with no build file is the build's to refuse, in its words.
    let empty = laptop.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let unfiled = run_shards_env(&["build"], &[empty.to_str().unwrap()], &on_laptop, TIMEOUT);
    assert_eq!(unfiled.status, Some(1), "{unfiled}");
    assert!(
        unfiled
            .stderr
            .contains("failed to read dockerfile: open Dockerfile: no such file or directory"),
        "{unfiled}"
    );
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

/// A key cosign v3.1.3 encrypted, its password, and its PKCS #8 (crates/sigstore's oracle).
fn cosign_key() -> (String, String, Vec<u8>) {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../../sigstore/testdata/cosign/oracle.json")).unwrap();
    let k = oracle["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "ecdsa p256 standard")
        .unwrap();
    let hex = k["pkcs8"].as_str().unwrap();
    let pkcs8 = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap(), 16).unwrap())
        .collect();
    (
        k["pem"].as_str().unwrap().to_string(),
        k["password"].as_str().unwrap().to_string(),
        pkcs8,
    )
}

/// Shards' own artifacts through a context (D180): an agent made of a directory here is made
/// there of the machine's copy of it, byte for byte the agent made here, a hard link and a
/// symlink in it; made again after a change, only what changed goes, a new name of a file
/// the copy holds as no bytes of its own; what the build says names this client's paths;
/// and signed by this client's key, the signature kept there, which a push carries to a
/// registry, of the agent, and verified by the key's public half.
#[test]
fn an_artifact_through_a_context_is_made_of_the_clients_files_and_signed_by_its_key() {
    let Some((machine, _listening, laptop, _image)) = joined("ctx-art") else {
        return;
    };
    let (port, repos) = common::writable_registry();
    let name = format!("127.0.0.1:{port}/team/agent:1");
    let dir = laptop.join("agent");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
    std::fs::write(dir.join("bin/run.sh"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(dir.join("data"), vec![7u8; 64 << 10]).unwrap();
    std::fs::hard_link(dir.join("data"), dir.join("data-too")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("bin/run.sh", dir.join("run")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("bin/run.sh", dir.join("run")).unwrap();
    let (pem, password, pkcs8) = cosign_key();
    let key = laptop.join("cosign.key");
    std::fs::write(&key, &pem).unwrap();
    let on_laptop = [
        ("SHARDS_HOME", laptop.as_os_str()),
        ("COSIGN_PASSWORD", std::ffi::OsStr::new(&password)),
    ];
    let here = |args: &[&str]| run_shards_env(&[], args, &on_laptop, TIMEOUT);
    let there = |args: &[&str]| run_shards_env(&["--context", "gpu"], args, &on_laptop, TIMEOUT);
    let made = |run: common::Run| {
        assert_eq!(run.status, Some(0), "{run}");
        run.stdout.trim().to_string()
    };
    let build = ["build", "agent", dir.to_str().unwrap(), "-t", name.as_str()];
    let made_here = made(here(&build));
    let made_there = made(there(&build));
    assert_eq!(
        made_there, made_here,
        "the agent made there is not the one made here"
    );
    // Made again after a change: a file rewritten, and a new name of a file the copy holds.
    std::fs::write(dir.join("bin/run.sh"), "#!/bin/sh\necho agent, again\n").unwrap();
    std::fs::hard_link(dir.join("data"), dir.join("data-three")).unwrap();
    let made_here = made(here(&build));
    let made_there = made(there(&build));
    assert_eq!(
        made_there, made_here,
        "made again, the agent there is not the one here"
    );
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    let came: Vec<u64> = log
        .lines()
        .filter_map(|l| {
            l.strip_suffix(" bytes of files came")?
                .rsplit(' ')
                .next()?
                .parse()
                .ok()
        })
        .collect();
    assert_eq!(came.len(), 2, "{log}");
    // Each file's bytes once: the config, the script and the data, whose second name goes
    // as a link; then the script rewritten alone, the data's third name none of its own.
    let script = "#!/bin/sh\necho agent, again\n".len() as u64;
    assert_eq!(
        came,
        [33 + 21 + (64 << 10), script],
        "the bytes of files that came"
    );
    // What the build says names this client's paths, not the machine's copy's.
    std::fs::write(dir.join("agent.json"), "{").unwrap();
    let refused = there(&build);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused
            .stderr
            .contains(&format!("{}: ", dir.join("agent.json").display())),
        "{refused}"
    );
    assert!(!refused.stderr.contains("synced"), "{refused}");
    std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
    let made_there = made(there(&build));
    // Signed by this client's key, kept there: pushed with the agent, of it.
    let signed = there(&["sign", "agent", &name, "--key", key.to_str().unwrap()]);
    assert_eq!(signed.status, Some(0), "{signed}");
    assert!(
        signed
            .stdout
            .contains(&format!("{made_there} signed by the key ")),
        "{signed}"
    );
    let pushed = there(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{pushed}");
    let repos = repos.lock().unwrap();
    let manifests = &repos.manifests["team/agent"];
    let (_, list) = &manifests[&made_there.replacen(':', "-", 1)];
    let list: serde_json::Value = serde_json::from_slice(list).unwrap();
    let signature = list["manifests"][0]["digest"].as_str().unwrap();
    let m: serde_json::Value = serde_json::from_slice(&manifests[signature].1).unwrap();
    assert_eq!(m["subject"]["digest"], made_there.as_str());
    let bundle = &repos.blobs["team/agent"][m["layers"][0]["digest"].as_str().unwrap()];
    let bundle = shards_sigstore::bundle::parse(bundle).unwrap();
    let shards_sigstore::bundle::SignatureContent::Envelope(envelope) = bundle.signature_content().unwrap()
    else {
        panic!("not an envelope");
    };
    let signer = shards_sigstore::sign::Signer::from_pkcs8(&pkcs8).unwrap();
    let public = shards_sigstore::x509::parse_pkix_public_key(signer.public_key_der()).unwrap();
    let verifier = shards_sigstore::signature::compat_verifier(&public, false, true).unwrap();
    shards_sigstore::signature::verify_envelope(&verifier, &envelope).unwrap();
    let statement = shards_sigstore::sign::statement(&made_there).unwrap();
    assert!(matches!(
        &bundle.content,
        shards_sigstore::bundle::Content::Envelope { payload: Some(p), .. } if *p == statement
    ));
}

/// The regular files under `dir` that hold `bytes`, by path.
fn files_holding(dir: &Path, bytes: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t)
                    if t.is_file()
                        && std::fs::read(&path)
                            .is_ok_and(|held| held.windows(bytes.len()).any(|w| w == bytes)) =>
                {
                    found.push(path)
                }
                _ => {}
            }
        }
    }
    found
}

/// A build's secrets through a context are the client's (D180): a file of the client's and
/// a variable of its environment, each read by a step there as a build here gives it, the
/// step saying only what they hash to; no file of the machine's holds either, as they
/// crossed in memory and were read from pipes; and one not here is said as a build here
/// says it.
#[test]
fn a_builds_secrets_through_a_context_are_the_clients_held_in_memory_alone() {
    let Some((machine, _listening, laptop, image)) = joined("ctx-sec") else {
        return;
    };
    let (file_secret, env_secret) = ("s3cret-of-the-clients-file", "s3cret-of-the-clients-environment");
    let app = laptop.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("Dockerfile"),
        format!(
            "FROM {image}\nUSER root\nRUN --mount=type=secret,id=tok --mount=type=secret,id=var,env=TOKEN [\"/bin/testguest\", \"digest\", \"/run/secrets/tok\", \"env:TOKEN\"]\n"
        ),
    )
    .unwrap();
    let token = laptop.join("token");
    std::fs::write(&token, file_secret).unwrap();
    let on_laptop = [
        ("SHARDS_HOME", laptop.as_os_str()),
        ("SHARDS_TEST_TOKEN", env_secret.as_ref()),
    ];
    let secret = format!("id=tok,src={}", token.display());
    let built = run_shards_env(
        &["--context", "gpu", "build"],
        &[
            "--progress=plain",
            "--secret",
            &secret,
            "--secret",
            "id=var,env=SHARDS_TEST_TOKEN",
            "-t",
            "secrets:1",
            app.to_str().unwrap(),
        ],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let digest = |b: &str| format!("{:016x}", shards_testguest::fnv1a(b.as_bytes()));
    for said in [
        format!(" digest /run/secrets/tok {}\n", digest(file_secret)),
        format!(" digest env:TOKEN {}\n", digest(env_secret)),
    ] {
        assert!(built.stderr.contains(&said), "{said:?}\n{built}");
    }
    for secret in [file_secret, env_secret] {
        let found = files_holding(&machine, secret.as_bytes());
        assert!(found.is_empty(), "{secret} held by {found:?}");
    }
    // Nor did the client's own words of them cross: its file's path, its variable's name.
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    for here in [token.to_str().unwrap(), "SHARDS_TEST_TOKEN"] {
        assert!(
            !log.contains(here),
            "{here} in what the machine was asked:\n{log}"
        );
    }
    // One not here: said as a build here says it, the build never sent.
    let missing = run_shards_env(
        &["--context", "gpu", "build"],
        &["--secret", "id=tok,src=/nowhere/token", app.to_str().unwrap()],
        &on_laptop,
        TIMEOUT,
    );
    assert_eq!(missing.status, Some(1), "{missing}");
    assert!(
        missing.stderr.starts_with("ERROR: failed to stat /nowhere/token"),
        "{missing}"
    );
}

/// A build's outputs through a context are the client's own files (D180): a directory of
/// its result (`-o type=local,dest=DIR`), a tar of it to a file (`-o type=tar,dest=FILE`),
/// its image's ID (`--iidfile`) and its metadata (`--metadata-file`), each written here as
/// the build ends, a file only once whole; a result to stdout (`-o -`) the build's stdout
/// still; and nothing of them left on the machine.
#[test]
fn a_builds_outputs_through_a_context_are_the_clients_files() {
    let Some((machine, _listening, laptop, _image)) = joined("ctx-out") else {
        return;
    };
    let app = laptop.join("app");
    std::fs::create_dir_all(app.join("sub")).unwrap();
    std::fs::write(app.join("Dockerfile"), "FROM scratch\nCOPY a.txt sub /\n").unwrap();
    std::fs::write(app.join("a.txt"), "one").unwrap();
    std::fs::write(app.join("sub/b.txt"), "two").unwrap();
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let build = |args: &[&str]| {
        let mut all = args.to_vec();
        all.push(app.to_str().unwrap());
        let built = run_shards_env(&["--context", "gpu", "build"], &all, &on_laptop, TIMEOUT);
        assert_eq!(built.status, Some(0), "{built}");
        built
    };
    let dir = laptop.join("out");
    build(&["-o", &format!("type=local,dest={}", dir.display())]);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "two");
    let tar = laptop.join("result.tar");
    build(&["-o", &format!("type=tar,dest={}", tar.display())]);
    let held = std::fs::read(&tar).unwrap();
    for (name, bytes) in [(&b"a.txt"[..], &b"one"[..]), (b"b.txt", b"two")] {
        assert!(
            held.windows(name.len()).any(|w| w == name),
            "{name:?} not in the tar"
        );
        assert!(
            held.windows(bytes.len()).any(|w| w == bytes),
            "{bytes:?} not in the tar"
        );
    }
    let (iid, meta_file) = (laptop.join("iid"), laptop.join("meta.json"));
    build(&[
        "-t",
        "outputs:1",
        "--iidfile",
        iid.to_str().unwrap(),
        "--metadata-file",
        meta_file.to_str().unwrap(),
    ]);
    let id = std::fs::read_to_string(&iid).unwrap();
    assert!(id.starts_with("sha256:") && id.len() == 71, "{id:?}");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_file).unwrap()).unwrap();
    assert!(meta.as_object().is_some_and(|m| !m.is_empty()), "{meta}");
    let to_stdout = build(&["-o", "-"]);
    assert!(to_stdout.stdout.contains("a.txt"), "no tar on stdout");
    // None of this client's paths crossed: each output was the machine's to write, there.
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    for here in [&dir, &tar, &iid] {
        assert!(
            !log.contains(here.to_str().unwrap()),
            "{} in what the machine was asked",
            here.display()
        );
    }
    // Nothing of them left there: each build's outputs went once sent.
    let synced = machine.join("synced");
    let mut stack = vec![synced];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
            assert_ne!(entry.file_name(), "out", "{} kept", entry.path().display());
            if entry.file_type().is_ok_and(|t| t.is_dir()) && entry.file_name() != "context" {
                stack.push(entry.path());
            }
        }
    }
}

/// What else a build names of the client's, through a context (D180): a named context's
/// directory, its ignore file applied; an OCI layout a build made here, named as a base; a
/// local cache, exported to a directory here and imported from it; and a policy's file,
/// which refuses the build in its words. Each kept in step or carried there, none of the
/// client's paths crossing.
#[test]
fn what_a_build_names_of_the_clients_is_carried_through_a_context() {
    let Some((machine, _listening, laptop, _image)) = joined("ctx-names") else {
        return;
    };
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let build = |dir: &Path, args: &[&str]| {
        let mut all = args.to_vec();
        all.push(dir.to_str().unwrap());
        run_shards_env(&["--context", "gpu", "build"], &all, &on_laptop, TIMEOUT)
    };
    let app = laptop.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("Dockerfile"),
        "FROM scratch\nCOPY --from=other . /other/\nCOPY a.txt /\n",
    )
    .unwrap();
    std::fs::write(app.join("a.txt"), "one").unwrap();
    let other = laptop.join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("x.txt"), "named").unwrap();
    std::fs::write(other.join("ignored.txt"), "never").unwrap();
    std::fs::write(other.join(".dockerignore"), "ignored.txt\n").unwrap();
    let (out, cache, layout) = (laptop.join("out"), laptop.join("cache"), laptop.join("layout"));
    let named = format!("other={}", other.display());
    let built = build(
        &app,
        &[
            "--build-context",
            &named,
            "-o",
            &format!("type=local,dest={}", out.display()),
            "--cache-to",
            &format!("type=local,dest={}", cache.display()),
        ],
    );
    assert_eq!(built.status, Some(0), "{built}");
    assert_eq!(std::fs::read_to_string(out.join("other/x.txt")).unwrap(), "named");
    assert!(
        !out.join("other/ignored.txt").exists(),
        "the named context's ignore file not applied"
    );
    assert!(cache.join("index.json").is_file(), "no cache exported here");
    let from = format!("type=local,src={}", cache.display());
    let again = build(&app, &["--build-context", &named, "--cache-from", &from]);
    assert_eq!(again.status, Some(0), "{again}");
    // A layout made here, by a build there, named as the base of the next.
    let made = build(
        &app,
        &[
            "--build-context",
            &named,
            "-o",
            &format!("type=oci,dest={},tar=false", layout.display()),
        ],
    );
    assert_eq!(made.status, Some(0), "{made}");
    assert!(layout.join("index.json").is_file(), "no layout here");
    let on_it = laptop.join("on-it");
    std::fs::create_dir_all(&on_it).unwrap();
    std::fs::write(on_it.join("Dockerfile"), "FROM base\nCOPY b.txt /\n").unwrap();
    std::fs::write(on_it.join("b.txt"), "two").unwrap();
    let (base, out2) = (
        format!("base=oci-layout://{}", layout.display()),
        laptop.join("out2"),
    );
    let stacked = build(
        &on_it,
        &[
            "--build-context",
            &base,
            "-o",
            &format!("type=local,dest={}", out2.display()),
        ],
    );
    assert_eq!(stacked.status, Some(0), "{stacked}");
    assert_eq!(std::fs::read_to_string(out2.join("a.txt")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(out2.join("b.txt")).unwrap(), "two");
    // A directory that holds no layout is refused in the build's words, named as this
    // client names it, not as its copy there.
    let not_a_layout = format!("base=oci-layout://{}", other.display());
    let refused = build(&on_it, &["--build-context", &not_a_layout]);
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(refused.stderr.contains(other.to_str().unwrap()), "{refused}");
    assert!(!refused.stderr.contains("/synced/"), "the copy's path: {refused}");
    // A policy of this client's, in its context and left out of it by its ignore file, as
    // the build reads it: it refuses the build, in its words.
    let policy = app.join("deny.rego");
    std::fs::write(
        &policy,
        "package docker\n\ndefault allow := false\n\ndeny_msg contains msg if {\n  not allow\n  msg := \"denied by the client's policy\"\n}\n\ndecision := {\"allow\": allow, \"deny_msg\": deny_msg}\n",
    )
    .unwrap();
    std::fs::write(app.join(".dockerignore"), "*.rego\n").unwrap();
    for named_as in ["deny.rego".to_string(), policy.display().to_string()] {
        let refused = build(
            &app,
            &[
                "--build-context",
                &named,
                "--policy",
                &format!("filename={named_as}"),
            ],
        );
        assert_eq!(refused.status, Some(1), "{refused}");
        assert!(
            refused.stderr.contains("denied by the client's policy"),
            "{refused}"
        );
    }
    // None of this client's paths crossed.
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    for here in [&other, &cache, &layout, &policy] {
        assert!(
            !log.contains(here.to_str().unwrap()),
            "{} in what the machine was asked",
            here.display()
        );
    }
}

/// A build's SSH agents through a context are the client's (D180): a step there lists the
/// key of the client's agent and has it sign, and is refused its removal, read-only there
/// and here; the agent keeps its key; a key file of the client's is answered here, as
/// BuildKit's keyring answers; and no path of the client's agent crosses.
#[test]
fn a_builds_ssh_agents_through_a_context_are_the_clients() {
    let Some((machine, _listening, laptop, image)) = joined("ctx-ssh") else {
        return;
    };
    let key = laptop.join("id");
    // On Unix OpenSSH's own agent, its key made and added by its tools; on Windows an agent
    // of the test's own at a pipe (common::SshAgent), whose OpenSSH agent is a service of the
    // machine's, its key written as ssh-keygen writes one.
    #[cfg(unix)]
    let (sock, _agent) = {
        let tools = ["ssh-agent", "ssh-keygen", "ssh-add"];
        if tools
            .iter()
            .any(|t| std::process::Command::new(t).arg("-h").output().is_err())
        {
            eprintln!("SKIP: no OpenSSH tools on this host");
            return;
        }
        let sock = laptop.join("agent.sock");
        struct Ended(std::process::Child);
        impl Drop for Ended {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let agent = Ended(common::ssh_agent(&sock));
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
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
        (sock, agent)
    };
    #[cfg(windows)]
    let (sock, agent) = {
        let seed = [7u8; 32];
        common::ssh_key_file(&key, &seed, "shards-test-key");
        let agent = common::ssh_agent_windows(seed, "shards-test-key");
        (PathBuf::from(&agent.pipe), agent)
    };
    let app = laptop.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("Dockerfile"),
        format!("FROM {image}\nUSER root\nRUN --mount=type=ssh [\"/bin/testguest\", \"agent\"]\n"),
    )
    .unwrap();
    let on_laptop = [
        ("SHARDS_HOME", laptop.as_os_str()),
        ("SSH_AUTH_SOCK", sock.as_os_str()),
    ];
    let build = |ssh: &str| {
        run_shards_env(
            &["--context", "gpu", "build"],
            &[
                "--progress=plain",
                "--no-cache",
                "--ssh",
                ssh,
                app.to_str().unwrap(),
            ],
            &on_laptop,
            TIMEOUT,
        )
    };
    let built = build("default");
    assert_eq!(built.status, Some(0), "{built}");
    for line in [
        " agent keys 1 shards-test-key\n",
        " agent signed\n",
        " agent remove refused\n",
    ] {
        assert!(built.stderr.contains(line), "{line:?}\n{built}");
    }
    // The agent keeps its key: no removal reached it.
    #[cfg(unix)]
    {
        let listed = std::process::Command::new("ssh-add")
            .arg("-l")
            .env("SSH_AUTH_SOCK", &sock)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&listed.stdout).contains("shards-test-key"));
    }
    #[cfg(windows)]
    assert_eq!(
        agent.removals.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a removal reached the client's agent"
    );
    // The key file itself, answered here: the key without its comment, as x/crypto drops it.
    let from_file = build(&format!("default={}", key.display()));
    assert_eq!(from_file.status, Some(0), "{from_file}");
    for line in [" agent keys 1 \n", " agent signed\n", " agent remove refused\n"] {
        assert!(from_file.stderr.contains(line), "{line:?}\n{from_file}");
    }
    let log = std::fs::read_to_string(machine.join("daemon.log")).unwrap();
    for here in [&sock, &key] {
        assert!(
            !log.contains(here.to_str().unwrap()),
            "{} in what the machine was asked",
            here.display()
        );
    }
    // The build reached its agents by descriptors alone: no socket file the machine's other
    // processes could dial. On Windows its pairs are pipes no name reaches.
    #[cfg(unix)]
    let sockets: Vec<PathBuf> = every_path(&machine)
        .into_iter()
        .filter(|p| {
            std::fs::symlink_metadata(p)
                .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
        })
        .filter(|p| !p.ends_with("engine.sock"))
        .collect();
    #[cfg(unix)]
    assert!(sockets.is_empty(), "sockets in the machine's home: {sockets:?}");
}

/// Every path under `root`.
#[cfg(unix)]
fn every_path(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}

/// A build through a context on a terminal is drawn as a build here is, shards' own
/// display, not BuildKit's plain lines: on a terminal of the machine's of this one's size,
/// resized with it, so that what it draws after a resize is laid out to the new width.
#[test]
fn a_build_through_a_context_on_a_terminal_is_drawn_as_here() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-live") else {
        return;
    };
    let app = laptop.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("Dockerfile"),
        format!("FROM {image}\nRUN [\"/bin/testguest\", \"sleep-for\", \"3000\"]\n"),
    )
    .unwrap();
    let mut term = Terminal::run(
        &laptop,
        &[
            "--context",
            "gpu",
            "build",
            "--no-cache",
            "-t",
            "drawn",
            app.to_str().unwrap(),
        ],
        30,
        100,
    );
    let head = term.wait_for("\"sleep-for\"");
    assert!(head.contains("B U I L D"), "{head:?}");
    assert!(!head.contains("#1 "), "plain lines: {head:?}");
    // Narrower as the step runs: the build's end is laid out to it.
    term.resize(30, 36);
    let (shown, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true), "{shown:?}");
    let end = shown.rsplit("Built.").next().unwrap_or_default();
    let mut plain = String::new();
    let mut chars = end.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    assert!(plain.contains("steps"), "{plain:?}");
    assert!(
        plain.contains('…'),
        "clipped to the terminal as it is now: {plain:?}"
    );
    for line in plain.split('\n') {
        let line = line.trim_end_matches('\r');
        assert!(line.chars().count() <= 36, "{line:?} in {plain:?}");
    }
}

/// `--cidfile` through a context is the client's file, as Docker's CLI keeps its own: made
/// here before the microVM is, named as here (a relative name of this client's working
/// directory), the ID written as soon as the machine has made it, refused where a file is
/// there already, and left by no run that was never made; the run records the client's
/// path.
#[test]
fn a_cidfile_through_a_context_is_the_clients() {
    let Some((_machine, _listening, laptop, image)) = joined("ctx-cid") else {
        return;
    };
    let on_laptop = [("SHARDS_HOME", laptop.as_os_str())];
    let there = |args: &[&str]| run_shards_env_in(&laptop, &["--context", "gpu"], args, &on_laptop, TIMEOUT);
    let made = there(&[
        "run",
        "--pull",
        "never",
        "-d",
        "--name",
        "held",
        "--cidfile",
        "held.id",
        &image,
        "sleep",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let held = laptop.join("held.id");
    let id = std::fs::read_to_string(&held).unwrap();
    assert_eq!(id.len(), 64, "{id:?}");
    assert_eq!(made.stdout.trim(), id, "{made}");
    // Named as a run here names it: its path from the working directory, as getcwd(3) says
    // it.
    let named = laptop.canonicalize().unwrap().join("held.id");
    let recorded = there(&["inspect", "-f", "{{.HostConfig.ContainerIDFile}}", "held"]);
    assert_eq!(recorded.stdout.trim(), named.to_str().unwrap(), "{recorded}");
    // The file there already: refused as a run here refuses it, nothing run there.
    let again = there(&[
        "run",
        "--pull",
        "never",
        "-d",
        "--cidfile",
        "held.id",
        &image,
        "sleep",
    ]);
    assert_eq!(again.status, Some(125), "{again}");
    assert!(
        again.stderr.contains(&format!(
            "shards: container ID file found, make sure the other container isn't running or delete {}\n",
            named.display()
        )),
        "{again}"
    );
    assert_eq!(
        std::fs::read_to_string(&held).unwrap(),
        id,
        "the file left as it was"
    );
    // A create's.
    let created = there(&[
        "create",
        "--pull",
        "never",
        "--cidfile",
        "created.id",
        &image,
        "sleep",
    ]);
    assert_eq!(created.status, Some(0), "{created}");
    assert_eq!(
        std::fs::read_to_string(laptop.join("created.id")).unwrap(),
        created.stdout.trim()
    );
    // An attached run's, while it runs.
    let mut attached = common::command()
        .current_dir(&*laptop)
        .env("SHARDS_HOME", laptop.as_os_str())
        .args(["--context", "gpu", "run", "--pull", "never", "--name", "attached"])
        .args(["--cidfile", "attached.id", &image, "sleep"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let attached_id = laptop.join("attached.id");
    let deadline = std::time::Instant::now() + TIMEOUT;
    while std::fs::read_to_string(&attached_id).map_or(0, |id| id.len()) != 64 {
        assert!(attached.try_wait().unwrap().is_none(), "the run ended first");
        assert!(std::time::Instant::now() < deadline, "no ID while the run runs");
        std::thread::sleep(Duration::from_millis(20));
    }
    let removed = there(&["rm", "-f", "held", "attached", created.stdout.trim()]);
    assert_eq!(removed.status, Some(0), "{removed}");
    let _ = attached.wait();
    assert_eq!(
        std::fs::read_to_string(&attached_id).unwrap().len(),
        64,
        "kept once the run ended"
    );
    // A run never made leaves no file.
    let missing = there(&[
        "run",
        "--pull",
        "never",
        "--cidfile",
        "never.id",
        "shards-test/none:never",
        "x",
    ]);
    assert_ne!(missing.status, Some(0), "{missing}");
    assert!(!laptop.join("never.id").exists(), "{missing}");
}
