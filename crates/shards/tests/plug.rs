//! Devices plugged into a running microVM through the daemon, and its disks grown (D214):
//! `shards plug disk|volume`, `shards unplug disk|volume` and `shards grow disk` on a
//! detached run restored from its image's template, whose microVM has PCI slots. Real VMs;
//! the image's entrypoint is the test guest (tests/common, `served`), whose modes the
//! checks inside the run are.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_pool, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// A detached run's microVM given a disk, made empty ext4 of 64 MiB, written through,
/// grown to 128 MiB with its file system as it runs, unplugged, and plugged again with what
/// was written; and a host directory, read and written through, and unplugged.
#[test]
fn devices_are_plugged_into_a_run_and_its_disk_grows() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("plug");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |command: &[&str], args: &[&str]| {
        let r = run_shards_env(command, args, &env, TIMEOUT);
        assert_eq!(
            r.status,
            Some(0),
            "shards {command:?} {args:?}: {}\n{}",
            r.stderr,
            r.stdout
        );
        r.stdout
    };
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let used = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(used.status, Some(0), "{}", used.stderr);
    // Its template saved by a first run; the run plugged into restores it.
    shards(&["run"], &[&image, "exit", "0"]);
    shards(&["run"], &["-d", "--name", "plugged", &image, "sleep"]);
    let inside = |args: &[&str]| shards(&["exec"], &[&["plugged", "/bin/testguest"], args].concat());

    let files = TempDir::new("plug-files");
    let disk = files.join("data.img");
    let spec = format!("{}:/data", disk.display());
    let said = shards(&["plug"], &["disk", "--size", "64m", "plugged", &spec]);
    assert!(said.starts_with("/data /dev/vd"), "{said}");
    inside(&["fs", "write:/data/hello=hi"]);
    assert!(inside(&["slurp", "/data/hello"]).contains("slurp /data/hello: hi"));
    let before = inside(&["statfs", "/data"]);
    // Grown as it runs: its file, its device, and its ext4 file system.
    assert_eq!(
        shards(&["grow"], &["disk", "plugged", "/data", "128m"]).trim(),
        (128u64 << 20).to_string()
    );
    let after = inside(&["statfs", "/data"]);
    let size = |said: &str| {
        said.split_whitespace()
            .nth(2)
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0)
    };
    assert!(size(&after) > size(&before) + (48 << 20), "{before} then {after}");
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 128 << 20);
    // A disk only grows.
    let shrunk = run_shards_env(&["grow"], &["disk", "plugged", "/data", "32m"], &env, TIMEOUT);
    assert_ne!(shrunk.status, Some(0), "{}", shrunk.stdout);
    shards(&["unplug"], &["disk", "plugged", "/data"]);
    assert!(inside(&["slurp", "/data/hello"]).contains("errno 2"));
    // Plugged again, what was written there.
    let again = format!("{}:/again:ro", disk.display());
    shards(&["plug"], &["disk", "plugged", &again]);
    assert!(inside(&["slurp", "/again/hello"]).contains("slurp /again/hello: hi"));
    shards(&["unplug"], &["disk", "plugged", "/again"]);

    // A host directory.
    let dir = files.join("shared");
    std::fs::create_dir(&dir).unwrap();
    // Writable to the image's user, not root, as a bind mount's host mode says (Docker's).
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    std::fs::write(dir.join("from-host"), "host\n").unwrap();
    let spec = format!("{}:/shared", dir.display());
    shards(&["plug"], &["volume", "plugged", &spec]);
    assert!(inside(&["slurp", "/shared/from-host"]).contains("slurp /shared/from-host: host"));
    inside(&["fs", "write:/shared/from-guest=guest"]);
    assert_eq!(std::fs::read_to_string(dir.join("from-guest")).unwrap(), "guest");
    shards(&["unplug"], &["volume", "plugged", "/shared"]);
    assert!(inside(&["slurp", "/shared/from-host"]).contains("errno 2"));
    let none = run_shards_env(&["unplug"], &["volume", "plugged", "/shared"], &env, TIMEOUT);
    assert!(
        none.stderr.contains("no volume is plugged in at /shared"),
        "{}",
        none.stderr
    );

    shards(&["rm"], &["-f", "plugged"]);
}
