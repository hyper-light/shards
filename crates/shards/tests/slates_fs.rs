//! A shards microVM mounts a volume the slates daemon serves, over virtio-fs, and reads and writes it (D38's share
//! another server serves; slates A-161).
//!
//! The slates side is the `slates` binary `SLATES_BIN` names. The test starts its anchor (which supervises the daemon)
//! under an instance of its own, creates a volume and writes `hello` into it, then runs the VM under
//! `slates shards ID --fd 3 -- shards run … --slates-fs-fd 3`: slates attaches the volume in the Shards form and runs the
//! spawner with one end of an anonymous socket pair at descriptor 3, the end the daemon serves that one attachment on in
//! this device's own le32 + FUSE framing. A VM process is App-Sandboxed (D30) and cannot open a socket itself, so it is
//! handed a connected descriptor, as --warm and --grants are. The guest mounts tag `shards0`, reads `/hello`, writes
//! `/from-guest` and reads it back; the host then reads `from-guest` through slates' own read. No host mount, and in that
//! test no DAX window.
//!
//! With a DAX window (`--share-window`, D189), the second test maps a slates file with DAX (slates' Shards part 3,
//! `docs/wip/shards/dax-protocol.md`). slates answers the device's DESCRIPTOR ask with a descriptor of a shared-memory
//! object holding that one file's bytes, writes every change made through another path into it before the change
//! returns, and writes the guest's stores back into the volume at its fsync. The guest stores through its map and syncs;
//! the host reads the store through slates and rewrites the file through another attachment; the guest waits to see the
//! host's change in its same map.
//!
//! The third test takes Shards' own route instead: slates mounts the volume on the host (`slates mount`, NFS over
//! loopback on macOS), and a microVM under a Shards daemon gets that mount as an ordinary volume (`-v MOUNT:/v`) with
//! its own DAX window (D189). Shards' server shares the host directory, and slates is only the file system beneath it.
//! The guest maps an 8 MiB file into the window and reads it whole through it; a byte the host then rewrites through
//! another slates attachment reaches a fresh open and read in the guest, and the run prints how soon. The mount is
//! unmounted before slates' anchor stops, whatever the test's outcome, since a daemon that dies under a live macOS NFS
//! mount leaves the mount hung.
//!
//! The first test runs on Windows too (WHP, D140): there the stream is a duplex named pipe, and `slates shards ID --
//! shards run … --slates-fs-handle {handle}` starts the spawner holding the pipe's end as a handle, writing its value
//! where `{handle}` stands (slates A-179, A-181); the spawner duplicates it into the confined VM process (D141, PM
//! M203), whose device reads and writes this framing on it with overlapped I/O (PM M340). The DAX test runs there too,
//! its stream handed so and its window WHP's (D189; slates A-180 answers its descriptor asks with sections). The mount
//! test is Unix's: Windows has no `slates mount` a Shards daemon shares. Each test skips loudly (prints and passes)
//! where the host cannot run VMs or `SLATES_BIN` is unset, as the other VM tests do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use common::{TempDir, guest_init, run_shards_env, served};
use common::{as_tests_run, cannot_run_vms, kernel, run_command, shards, test_guest};

/// The host bounds the run; the guest's own waits are unbounded (it powers off on done).
const TIMEOUT: Duration = Duration::from_secs(90);
/// How long slates' anchor gets to serve and settle: seconds on a quiet host, and a verb refused meanwhile (`slates`
/// exits 3, no daemon yet) is asked again.
const START: Duration = Duration::from_secs(60);
/// `slates`' exit code when no daemon answers at the instance (its usage notes).
const NO_DAEMON: i32 = 3;
/// What the volume holds for the guest to read, as the testguest expects it.
const HELLO: &[u8] = b"hello from slates over shards virtio-fs\n";
/// What the guest writes, as the testguest writes it.
const FROM_GUEST: &[u8] = b"written by a shards guest through its slates share\n";
/// The file the guest maps with DAX: 3 MiB, past the 2 MiB slates marks `FUSE_ATTR_DAX` at, byte `i` being `i mod 251`,
/// as the testguest checks it.
const BIG_BYTES: usize = 3 << 20;
/// Where the guest stores through its map, and what, as the testguest stores it.
const GUEST_AT: usize = 4096;
const GUEST_STORE: &[u8] = b"stored by a shards guest through its DAX map\n";
/// Where the host's change through another path lands, and what, as the testguest waits for it.
const HOST_AT: usize = 1 << 20;
const HOST_CHANGE: &[u8] = b"written by the host through slates while the guest held its map\n";
/// Where the host cuts the file once the guest has seen its change, as the testguest expects it: past the change,
/// inside the first range, at no page's end, so the rest of that page must read zeros in the guest's map.
const CUT: usize = (3 << 19) + 100;
/// The DAX window the share gets (D189), in MiB: room for the file's two ranges and to spare.
const SHARE_WINDOW_MIB: &str = "64";

/// A process the test runs, killed and reaped when dropped, so a failed assertion leaves no VM behind (the DAX guest
/// waits for the host with no deadline of its own).
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs `cmd` (given [`as_tests_run`]'s environment) with its stdout read line by line as it comes: `on_line` sees each
/// line on this thread, so the test can act on the host while the VM waits for it. Killed, failing the test, after
/// `timeout`; `asked` names it then. Its exit and everything it printed.
fn run_watching(
    mut cmd: Command,
    asked: &[String],
    timeout: Duration,
    mut on_line: impl FnMut(&str),
) -> common::Run {
    use std::io::{BufRead, BufReader, Read};
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let start = Instant::now();
    let mut child = Reaped(
        cmd.env("SHARDS_TIMING", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning the VM"),
    );
    let (tx, lines) = channel::<String>();
    let mut out = BufReader::new(child.0.stdout.take().unwrap());
    std::thread::spawn(move || {
        let mut line = Vec::new();
        while out.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            if tx.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                return;
            }
            line.clear();
        }
    });
    let mut err = child.0.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut got = Vec::new();
        let _ = err.read_to_end(&mut got);
        String::from_utf8_lossy(&got).into_owned()
    });
    let mut stdout = String::new();
    let late = |stdout: &str| -> ! {
        panic!(
            "{} did not exit within {timeout:?}\n--- stdout so far\n{stdout}",
            asked.join(" ")
        )
    };
    // Each line as it comes, until its stdout ends; then its exit, waited for, not polled.
    loop {
        let left = timeout
            .checked_sub(start.elapsed())
            .unwrap_or_else(|| late(&stdout));
        match lines.recv_timeout(left) {
            Ok(line) => {
                on_line(line.trim_end());
                stdout.push_str(&line);
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => late(&stdout),
        }
    }
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > timeout {
            late(&stdout);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    // A Windows process ends with an exit code, never a signal.
    #[cfg(windows)]
    let signal = None;
    common::Run {
        status: status.code(),
        signal,
        stdout,
        stderr: errors.join().unwrap_or_default(),
        elapsed: start.elapsed(),
    }
}

/// Slates' anchor, killed and reaped when dropped; its daemon leaves with it.
struct Anchor(Child);

impl Drop for Anchor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `slates --instance INSTANCE ARGS…` with `input` on its stdin, asked again while no daemon answers (exit 3) within
/// [`START`]; what it said.
fn slates(bin: &OsStr, instance: &str, args: &[&str], input: &[u8]) -> Output {
    let start = Instant::now();
    loop {
        let mut child = Command::new(bin)
            .args(["--instance", instance])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning slates");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        if out.status.code() != Some(NO_DAEMON) || start.elapsed() > START {
            return out;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// [`slates`], which must succeed; the value of its `KEY: value` line `key`, or its whole stdout for an empty key.
fn slates_ok(bin: &OsStr, instance: &str, args: &[&str], input: &[u8], key: &str) -> String {
    let out = slates(bin, instance, args, input);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "slates {args:?}: {:?}\n--- stdout\n{stdout}\n--- stderr\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    if key.is_empty() {
        return stdout;
    }
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: ")).map(str::to_owned))
        .unwrap_or_else(|| panic!("slates {args:?} printed no {key}: {stdout}"))
}

/// Writes `bytes` as the volume's file `file`, through an attachment of its own.
fn write(bin: &OsStr, instance: &str, id: &str, file: &str, bytes: &[u8]) {
    let writer = slates_ok(bin, instance, &["attach", id, "--write"], b"", "attachment");
    slates_ok(bin, instance, &["write", id, &writer, file], bytes, "");
    slates_ok(bin, instance, &["detach", &writer], b"", "");
}

/// slates' anchor under an instance of the test's own, and a volume `name` made there
/// holding `files`: the anchor (its daemon leaves with it), the instance and the volume's ID.
fn volume(bin: &OsStr, name: &str, files: &[(&str, &[u8])]) -> (Anchor, String, String) {
    let instance = format!("{name}-{}", std::process::id());
    let anchor = Anchor(
        Command::new(bin)
            .args(["--instance", &instance, "anchor", "--quick"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning slates' anchor"),
    );
    slates_ok(bin, &instance, &["bootstrap", "root"], b"", "");
    let id = slates_ok(
        bin,
        &instance,
        &["volume", "create", name, "--bounded", "64MiB"],
        b"",
        "id",
    );
    for (file, bytes) in files {
        write(bin, &instance, &id, file, bytes);
    }
    (anchor, instance, id)
}

/// Do: start slates' anchor, make a volume holding `hello`, and boot a microVM whose `shards0` share is that volume,
/// handed in by `slates shards`; the guest mounts it, reads `/hello`, writes `/from-guest` and reads it back. Expect the
/// guest to print `SHARDS-TEST PASS` (each step held) and slates' own read to return what the guest wrote.
#[test]
fn a_microvm_mounts_reads_and_writes_a_volume_the_slates_daemon_serves() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let Some(bin) = std::env::var_os("SLATES_BIN") else {
        eprintln!("SKIP: SLATES_BIN is not set (the built `slates` binary)");
        return;
    };
    let (_anchor, instance, id) = volume(&bin, "shards-fs", &[("hello", HELLO)]);

    let k = kernel().to_str().unwrap().to_owned();
    let init = test_guest().to_str().unwrap().to_owned();
    // The stream's end reaches the spawner as descriptor 3 on Unix; on Windows as a handle, its value written where
    // `{handle}` stands, since a handle's value is not the caller's to choose (slates A-181).
    #[cfg(unix)]
    let (hand, flag, end): (&[&str], _, _) = (&["--fd", "3"], "--slates-fs-fd", "3");
    #[cfg(windows)]
    let (hand, flag, end): (&[&str], _, _) = (&[], "--slates-fs-handle", "{handle}");
    let mut vm = Command::new(&bin);
    as_tests_run(&mut vm);
    vm.args(["--instance", &instance, "shards", &id])
        .args(hand)
        .arg("--")
        .arg(shards())
        .args([
            "run",
            "--kernel",
            &k,
            "--init",
            &init,
            flag,
            end,
            "--cmdline",
            "console=ttyS0 quiet panic=-1 shards_test=slates_fs",
        ]);
    let asked = vec![
        "slates shards".to_string(),
        id.clone(),
        format!("-- shards run {flag} {end}"),
    ];
    let run = run_command(vm, &asked, TIMEOUT);
    assert_eq!(run.status, Some(0), "{run}");
    assert!(
        run.stdout.contains("SHARDS-TEST PASS"),
        "the guest did not mount, read and write the slates volume: {run}"
    );

    let read = slates(&bin, &instance, &["read", &id, "from-guest"], b"");
    assert!(read.status.success(), "slates read: {read:?}");
    assert_eq!(
        read.stdout, FROM_GUEST,
        "slates reads what the guest wrote through its share"
    );
}

/// Do: start slates' anchor, make a volume holding `big` (3 MiB), and boot a microVM whose `shards0` share is that volume
/// with a DAX window; the guest mounts it `dax=inode`, finds `big` DAX, maps its first range, stores through the map and
/// fsyncs. When it says so, the host reads `big` through slates and rewrites it through another attachment with the
/// guest's store kept and other bytes changed. Expect slates' first read after the guest's word to hold the store (slates
/// writes it back before the fsync returns; how long that read took is printed), the guest to see the host's change and
/// its own store in its same map. When it says so, the host cuts `big` at [`CUT`]; expect the guest to read zeros over
/// the rest of the page that holds the new end, its store and the change kept, and print `SHARDS-TEST PASS`, and
/// slates' read afterwards to hold both and end at the cut. On Windows the
/// stream is handed as a handle and the window is WHP's (slates A-180 answers its descriptor asks with confined sections).
#[test]
fn a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let Some(bin) = std::env::var_os("SLATES_BIN") else {
        eprintln!("SKIP: SLATES_BIN is not set (the built `slates` binary)");
        return;
    };
    let big: Vec<u8> = (0..BIG_BYTES).map(|i| (i % 251) as u8).collect();
    let (_anchor, instance, id) = volume(&bin, "shards-dax", &[("big", big.as_slice())]);

    let k = kernel().to_str().unwrap().to_owned();
    let init = test_guest().to_str().unwrap().to_owned();
    // The stream's end as the first test hands it: descriptor 3 on Unix, a handle on Windows (slates A-181).
    #[cfg(unix)]
    let (hand, flag, end): (&[&str], _, _) = (&["--fd", "3"], "--slates-fs-fd", "3");
    #[cfg(windows)]
    let (hand, flag, end): (&[&str], _, _) = (&[], "--slates-fs-handle", "{handle}");
    let mut vm = Command::new(&bin);
    as_tests_run(&mut vm);
    vm.args(["--instance", &instance, "shards", &id])
        .args(hand)
        .arg("--")
        .arg(shards())
        .args([
            "run",
            "--kernel",
            &k,
            "--init",
            &init,
            flag,
            end,
            "--share-window",
            SHARE_WINDOW_MIB,
            "--cmdline",
            "console=ttyS0 quiet panic=-1 shards_test=slates_fs_dax",
        ]);
    let asked = vec![
        "slates shards".to_string(),
        id.clone(),
        format!("-- shards run {flag} {end} --share-window"),
        SHARE_WINDOW_MIB.to_string(),
    ];
    let mut visible_after = None;
    let mut changed: Option<Vec<u8>> = None;
    let run = run_watching(vm, &asked, TIMEOUT, |line| {
        if line.contains("SLATES-DAX SEEN") {
            let whole = changed.as_ref().expect("the guest saw the change the host made");
            write(&bin, &instance, &id, "big", &whole[..CUT]);
            return;
        }
        if !line.contains("SLATES-DAX STORED") {
            return;
        }
        let said = Instant::now();
        let read = slates(&bin, &instance, &["read", &id, "big"], b"");
        visible_after = Some(said.elapsed());
        assert!(read.status.success(), "slates read: {read:?}");
        assert_eq!(
            &read.stdout[GUEST_AT..GUEST_AT + GUEST_STORE.len()],
            GUEST_STORE,
            "the guest's store reached the volume by its fsync"
        );
        let mut whole = read.stdout;
        whole[HOST_AT..HOST_AT + HOST_CHANGE.len()].copy_from_slice(HOST_CHANGE);
        write(&bin, &instance, &id, "big", &whole);
        changed = Some(whole);
    });
    assert_eq!(run.status, Some(0), "{run}");
    assert!(
        run.stdout.contains("SHARDS-TEST PASS"),
        "the guest did not map, store, and see the host's change: {run}"
    );
    for line in run
        .stdout
        .lines()
        .filter(|line| line.contains("SLATES-DAX") || line.contains("SHARDS-TEST"))
    {
        eprintln!("guest: {line}");
    }
    let visible_after = visible_after.expect("the guest said it stored and synced");
    eprintln!(
        "slates-dax: synced store visible in slates' first read, {} µs after the guest's word",
        visible_after.as_micros()
    );

    let read = slates(&bin, &instance, &["read", &id, "big"], b"");
    assert!(read.status.success(), "slates read: {read:?}");
    assert_eq!(read.stdout.len(), CUT, "the file ends at the cut");
    assert_eq!(&read.stdout[GUEST_AT..GUEST_AT + GUEST_STORE.len()], GUEST_STORE);
    assert_eq!(&read.stdout[HOST_AT..HOST_AT + HOST_CHANGE.len()], HOST_CHANGE);
}

/// The file a guest maps through Shards' own DAX window over a slates mount: 8 MiB, past the 1 MiB from which a volume's
/// files go through its microVM's window (D189), byte `i` being `i mod 251`.
#[cfg(unix)]
const MOUNTED_BYTES: usize = 8 << 20;
/// Where the host's change through another slates attachment lands in that file: inside its second 2 MiB range.
#[cfg(unix)]
const MOUNTED_AT: usize = 3 << 20;
/// The name of the microVM the mount test runs under its Shards daemon.
#[cfg(unix)]
const MOUNTED_VM: &str = "slatesmount";

/// A volume mounted on this host with `slates mount` at a directory, unmounted when dropped (asked until slates answers,
/// then forced), so that slates' daemon never dies under a live mount: declared after the anchor, it is dropped before.
#[cfg(unix)]
struct Mounted<'a> {
    bin: &'a OsStr,
    instance: String,
    point: std::path::PathBuf,
}

#[cfg(unix)]
impl<'a> Mounted<'a> {
    fn new(bin: &'a OsStr, instance: &str, id: &str, point: &std::path::Path) -> Mounted<'a> {
        let at = point.to_str().unwrap();
        slates_ok(bin, instance, &["mount", id, at], b"", "");
        Mounted {
            bin,
            instance: instance.to_owned(),
            point: point.to_owned(),
        }
    }
}

#[cfg(unix)]
impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        let at = self.point.to_str().unwrap_or_default();
        let deadline = Instant::now() + START;
        while Instant::now() < deadline {
            if slates(self.bin, &self.instance, &["unmount", at], b"")
                .status
                .success()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = Command::new("umount").args(["-f", at]).status();
    }
}

/// A microVM under a Shards daemon, removed when dropped: declared after the mount it shares, it is removed first.
#[cfg(unix)]
struct Removed<'a> {
    env: &'a [(&'a str, &'a OsStr)],
}

#[cfg(unix)]
impl Drop for Removed<'_> {
    fn drop(&mut self) {
        let _ = run_shards_env(&[], &["rm", "-f", MOUNTED_VM], self.env, TIMEOUT);
    }
}

/// Do: start slates' anchor, make a volume holding `big` (8 MiB), and mount it on this host with `slates mount` (NFS over
/// loopback on macOS); run a microVM under a Shards daemon whose volume is that mount, with a DAX window of its own (`-v
/// MOUNT:/v`, `SHARDS_SHARE_WINDOW_MIB`, D189). That is the host-share route: Shards' own server shares the host
/// directory, and slates is the file system beneath it. Then rewrite one byte of `big` through another slates attachment.
/// Expect: the guest's kernel maps `big` into the window (`dax /v/big yes`) and reads it whole through it; and a fresh
/// open and read in the guest shows the host's byte within `TIMEOUT`, since each open makes Shards' server open the host
/// file, which revalidates it on the host side.
#[cfg(unix)]
#[test]
fn a_microvm_maps_a_slates_mount_file_through_its_own_dax_window() {
    if cannot_run_vms() {
        return;
    }
    let Some(bin) = std::env::var_os("SLATES_BIN") else {
        eprintln!("SKIP: SLATES_BIN is not set (the built `slates` binary)");
        return;
    };
    let big: Vec<u8> = (0..MOUNTED_BYTES).map(|i| (i % 251) as u8).collect();
    let (_anchor, instance, id) = volume(&bin, "mounted", &[("big", &big)]);
    let dir = TempDir::new("slates-mount");
    let point = dir.join("v");
    std::fs::create_dir(&point).unwrap();
    let _mounted = Mounted::new(&bin, &instance, &id, &point);
    assert_eq!(
        std::fs::read(point.join("big")).unwrap(),
        big,
        "the host's slates mount serves the volume"
    );

    let home = TempDir::new("slates-mount-home");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", SHARE_WINDOW_MIB.as_ref()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let (image, _) = served();
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let volume = format!("{}:/v", point.display());
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", MOUNTED_VM, "-u", "0", "-v", &volume, &image, "sleep",
    ]);
    let _vm = Removed { env: &env };
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", MOUNTED_VM, "/bin/testguest"];
        all.extend_from_slice(args);
        let r = shards(&all);
        assert_eq!(r.status, Some(0), "{args:?}: {r}");
        r.stdout
    };
    assert_eq!(
        exec(&["fs", "dax:/v/big"]),
        "dax /v/big yes\n",
        "the guest maps the slates file into Shards' own window"
    );
    assert_eq!(
        exec(&["digest", "/v/big"]),
        format!("digest /v/big {:016x}\n", shards_testguest::fnv1a(&big)),
        "the guest reads the slates file whole through the window"
    );

    let mut changed = big.clone();
    changed[MOUNTED_AT] = changed[MOUNTED_AT].wrapping_add(1);
    write(&bin, &instance, &id, "big", &changed);
    let want = format!("peek /v/big {MOUNTED_AT} {}\n", changed[MOUNTED_AT]);
    let started = Instant::now();
    let (followed, last) = loop {
        let got = exec(&["fs", &format!("peek:/v/big:{MOUNTED_AT}")]);
        if got == want {
            break (Some(started.elapsed()), got);
        }
        if started.elapsed() >= TIMEOUT {
            break (None, got);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // Measured, not asserted: the goal is the map; whether a change through another path reaches the window is the
    // finding this run records (Shards' M193: a range replaced in the host alone keeps the old pages at stage 2).
    eprintln!(
        "SLATES MOUNT WINDOW: dax yes, digest whole; after the host's write, {}",
        followed.map_or_else(
            || format!("the guest kept the old byte for {TIMEOUT:?} ({})", last.trim()),
            |took| format!("a fresh open and read in the guest saw the new byte after {took:?}"),
        )
    );
}
