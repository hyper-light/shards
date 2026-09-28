//! Commands run in images, end to end: `shards vm run --rootfs IMAGE -- COMMAND` boots a
//! real VM into an EROFS image, shards-init runs the command as `docker run` would, and
//! its output and exit status come back through shards. The command is the test guest,
//! run as a workload (crates/testguest/src/workload.rs).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{TempDir, cannot_run_vms, guest_init, kernel, shards, test_guest};
use shards_image::erofs::{self, DataRef, Kind, Meta, Node, NodeId, Source, Tree};

const TIMEOUT: Duration = Duration::from_secs(60);
const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n";
const GROUP: &str = "root:x:0:\napp:x:1000:\nstaff:x:50:app\n";

struct Files(Vec<Vec<u8>>);

impl Source for Files {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let bytes = &self.0[data.source as usize];
        let start = (data.offset + at) as usize;
        buf.copy_from_slice(&bytes[start..start + buf.len()]);
        Ok(())
    }
}

/// A minimal image: the test guest as /bin/testguest, users, and nothing else. There is
/// no /proc, /sys or /dev: init must make them.
fn image(dir: &Path) -> PathBuf {
    let meta = |mode: u16, owner: u32| Meta {
        mode,
        uid: owner,
        gid: owner,
        mtime: 1_700_000_000,
        ..Meta::default()
    };
    let mut tree = Tree::new(meta(0o755, 0));
    let mut files = Files(Vec::new());
    let mut file = |tree: &mut Tree, at: NodeId, name: &str, mode: u16, bytes: Vec<u8>| {
        let data = DataRef {
            source: files.0.len() as u32,
            offset: 0,
        };
        let size = bytes.len() as u64;
        files.0.push(bytes);
        let kind = Kind::File { size, data };
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind,
                meta: meta(mode, 0),
            },
        )
        .unwrap();
    };
    let dir_node = |tree: &mut Tree, at: NodeId, name: &str, mode: u16, owner: u32| {
        let kind = Kind::Dir(BTreeMap::new());
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind,
                meta: meta(mode, owner),
            },
        )
        .unwrap()
    };
    let bin = dir_node(&mut tree, Tree::ROOT, "bin", 0o755, 0);
    file(
        &mut tree,
        bin,
        "testguest",
        0o755,
        std::fs::read(test_guest()).unwrap(),
    );
    let etc = dir_node(&mut tree, Tree::ROOT, "etc", 0o755, 0);
    file(&mut tree, etc, "passwd", 0o644, PASSWD.into());
    file(&mut tree, etc, "group", 0o644, GROUP.into());
    let home = dir_node(&mut tree, Tree::ROOT, "home", 0o755, 0);
    dir_node(&mut tree, home, "app", 0o755, 1000);
    dir_node(&mut tree, Tree::ROOT, "tmp", 0o1777, 0);
    let path = dir.join("image.erofs");
    let mut out = io::BufWriter::new(std::fs::File::create(&path).unwrap());
    erofs::write(&tree, &mut files, &mut out).unwrap();
    out.flush().unwrap();
    path
}

struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status,
            String::from_utf8_lossy(&self.stdout),
            self.stderr
        )
    }
}

/// Runs `shards vm run ... --rootfs IMAGE <options> -- <command>`, with `stdin` as its
/// input.
fn run(image: &Path, options: &[&str], command: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(shards())
        .args(["vm", "run", "--kernel"])
        .arg(kernel())
        .arg("--init")
        .arg(guest_init())
        .arg("--rootfs")
        .arg(image)
        .args(options)
        .arg("--")
        .args(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).unwrap();
        v
    });
    let mut err = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        err.read_to_string(&mut s).unwrap();
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("shards did not exit within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    feeder.join().unwrap();
    Output {
        status: status.code(),
        stdout: reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}

/// `report`'s `key value` lines.
fn report(out: &Output) -> BTreeMap<String, String> {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| {
            // Environment and mount lines share a key; keep them apart by what follows.
            match k {
                "env" | "mount" => {
                    let (name, value) = v.split_once(['=', ' ']).unwrap();
                    (format!("{k} {name}"), value.to_string())
                }
                _ => (k.to_string(), v.to_string()),
            }
        })
        .collect()
}

#[test]
fn commands_run_in_the_image_as_docker_runs_them() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run");
    let image = image(&dir);

    let out = run(
        &image,
        &["--hostname", "box", "-e", "GREETING=hi", "-e", "HOSTNAME=renamed"],
        &["/bin/testguest", "report"],
        b"",
    );
    assert_eq!(out.status, Some(0), "{out}");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    assert_eq!((get("uid"), get("gid"), get("groups")), ("0", "0", "0"), "{out}");
    assert_eq!((get("cwd"), get("hostname")), ("/", "box"), "{out}");
    assert_eq!(
        get("env PATH"),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    );
    assert_eq!((get("env HOSTNAME"), get("env GREETING")), ("renamed", "hi"));
    assert_eq!(get("env HOME"), "/root", "HOME comes from /etc/passwd");
    assert_eq!(get("writable"), "true", "the root filesystem takes writes: {out}");
    // Signals as a new process has them, whatever init itself blocks or ignores.
    assert_eq!(
        (get("sigpipe"), get("sigblk")),
        ("default", "0000000000000000"),
        "{out}"
    );
    for (target, fstype) in [
        ("/", "overlay"),
        ("/proc", "proc"),
        ("/sys", "sysfs"),
        ("/dev", "devtmpfs"),
        ("/dev/pts", "devpts"),
        ("/dev/shm", "tmpfs"),
        ("/dev/mqueue", "mqueue"),
    ] {
        assert_eq!(get(&format!("mount {target}")), fstype, "{out}");
    }

    // A named user with its groups and home, found on PATH, in a working directory that
    // does not exist yet.
    let out = run(
        &image,
        &["-u", "app", "-w", "/work/dir"],
        &["testguest", "report"],
        b"",
    );
    assert_eq!(out.status, Some(0), "{out}");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    // getgroups(2) reports the kernel's order: setgroups(2) sorts them (kernel/groups.c).
    assert_eq!(
        (get("uid"), get("gid"), get("groups")),
        ("1000", "1000", "50,1000"),
        "{out}"
    );
    assert_eq!((get("cwd"), get("env HOME")), ("/work/dir", "/home/app"), "{out}");
    assert_eq!(get("writable"), "false", "app may not write to /");

    // A numeric user with no passwd entry.
    let out = run(&image, &["-u", "4242"], &["/bin/testguest", "report"], b"");
    let r = report(&out);
    let get = |k: &str| r.get(k).map(String::as_str).unwrap_or_default();
    assert_eq!(
        (get("uid"), get("gid"), get("env HOME")),
        ("4242", "0", "/"),
        "{out}"
    );
}

#[test]
fn exit_statuses_are_the_ones_docker_run_gives() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run-status");
    let image = image(&dir);
    for (options, command, status, stderr) in [
        (&[][..], &["/bin/testguest", "exit", "3"][..], 3, ""),
        (&[], &["/bin/testguest", "kill"], 128 + 9, ""),
        (&[], &["/bin/testguest", "stderr", "to stderr"], 0, "to stderr"),
        (&[], &["nothere"], 127, "executable file not found in $PATH"),
        (&[], &["/bin/nothere"], 127, "no such file or directory"),
        (&[], &["/bin"], 126, "is a directory"),
        (
            &["-u", "nobody"],
            &["/bin/testguest", "report"],
            125,
            "unable to find user nobody",
        ),
        (
            &["-w", "relative"],
            &["/bin/testguest", "report"],
            125,
            "not absolute",
        ),
    ] {
        let out = run(&image, options, command, b"");
        assert_eq!(out.status, Some(status), "{command:?}: {out}");
        assert!(
            out.stderr.to_lowercase().contains(&stderr.to_lowercase()),
            "{command:?}: {out}"
        );
    }
    // A container ends with its main process: a child left holding stdout is killed.
    let out = run(&image, &[], &["/bin/testguest", "orphan"], b"");
    assert_eq!(
        (out.status, out.stdout.as_slice()),
        (Some(0), &b"parent done\n"[..]),
        "{out}"
    );
}

#[test]
fn stdio_carries_bulk_data_both_ways() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("run-stdio");
    let image = image(&dir);
    let mut input = vec![0u8; 8 << 20];
    shards_testguest::fill(7, 0, &mut input);
    let out = run(&image, &["-i"], &["/bin/testguest", "cat"], &input);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert!(
        out.stdout == input,
        "8 MiB through stdin and back: got {} bytes",
        out.stdout.len()
    );

    // Without -i, stdin is closed.
    let out = run(&image, &[], &["/bin/testguest", "cat"], b"ignored");
    assert_eq!((out.status, out.stdout.len()), (Some(0), 0), "{out}");

    let len = 32 << 20;
    let out = run(
        &image,
        &[],
        &["/bin/testguest", "bulk", &len.to_string(), "9"],
        b"",
    );
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.len(), len);
    assert_eq!(shards_testguest::first_mismatch(9, 0, &out.stdout), None);
}
