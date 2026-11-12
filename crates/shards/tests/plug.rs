//! Devices plugged into a running microVM through the daemon, and its disks grown (D214):
//! `shards plug disk|volume`, `shards unplug disk|volume` and `shards grow disk` on a
//! detached run, restored from its image's template where this host's daemon keeps
//! templates and booted where it does not (Windows Server 2022, PM M345), its microVM
//! with PCI slots either way. Real VMs, on every host that runs them; the image's
//! entrypoint is the test guest (tests/common, `served`), whose modes the checks inside the
//! run are.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// A detached run's microVM given a disk, made empty ext4 of 64 MiB, written through,
/// grown to 128 MiB with its file system as it runs, unplugged, and plugged again with what
/// was written; and a host directory, read and written through, and unplugged.
#[test]
fn devices_are_plugged_into_a_run_and_its_disk_grows() {
    if cannot_run_vms() {
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
    // Its template saved by a first run where templates are kept; the run plugged into
    // restores it.
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
    // A Windows directory without WSL's metadata is root's and 0755 to its guest (ntfs
    // `stat`), which root writes to.
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    std::fs::write(dir.join("from-host"), "host\n").unwrap();
    let spec = format!("{}:/shared", dir.display());
    shards(&["plug"], &["volume", "plugged", &spec]);
    assert!(inside(&["slurp", "/shared/from-host"]).contains("slurp /shared/from-host: host"));
    let mut write: Vec<&str> = if cfg!(windows) {
        vec!["-u", "0"]
    } else {
        Vec::new()
    };
    write.extend([
        "plugged",
        "/bin/testguest",
        "fs",
        "write:/shared/from-guest=guest",
    ]);
    shards(&["exec"], &write);
    assert_eq!(std::fs::read_to_string(dir.join("from-guest")).unwrap(), "guest");
    shards(&["unplug"], &["volume", "plugged", "/shared"]);
    assert!(inside(&["slurp", "/shared/from-host"]).contains("errno 2"));
    let none = run_shards_env(&["unplug"], &["volume", "plugged", "/shared"], &env, TIMEOUT);
    assert!(
        none.stderr.contains("no volume is plugged in at /shared"),
        "{}",
        none.stderr
    );

    // Off Linux no host's function is plugged in: refused by name, with its host's own
    // limit, as a run's start refuses one (D154; PM M401, M402, M403).
    #[cfg(not(target_os = "linux"))]
    {
        let limit = shards_vmm::vm::passthrough_refused();
        for function in ["0000:01:00.0", "/dev/vfio/devices/vfio0"] {
            let r = run_shards_env(&["plug"], &["function", "plugged", function], &env, TIMEOUT);
            assert_ne!(r.status, Some(0), "{function}: {}", r.stdout);
            let said = format!("shards: cannot plug into plugged: {function}: {limit}");
            assert!(r.stderr.contains(&said), "{function}: {}", r.stderr);
        }
    }

    shards(&["rm"], &["-f", "plugged"]);
}

/// Where a host directory is plugged into an agent at its host path (D235): its own path
/// where its top-level directory may be a plug root; a Windows drive's beneath `/mnt` as
/// WSL has it (`C:\x` at `/mnt/c/x`); and beneath `/srv`, a Linux host's root, where its
/// top is `/tmp`, which no root is and in which a Linux host's test directories lie.
fn at_host_path(host: &std::path::Path) -> String {
    let text = host.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => format!("/mnt/{}/{rest}", drive.to_ascii_lowercase()),
        _ if text.starts_with("/tmp/") => format!("/srv/plugm{text}"),
        _ => text,
    }
}

/// A project plugged into a running agent at its host path (D235): an Agentfile's agent,
/// given its host's plug roots as its run starts (and the top-level directory of the
/// directory bound at the start), can make nothing beneath any of them, as its root is
/// read-only, until a host directory is plugged into it (`--for`) beneath one; then it
/// reads the host's file there and writes its own, which is the host's. One plugged outside
/// every root is refused naming them; the project is unplugged as the agent runs.
#[test]
fn a_project_is_plugged_into_a_running_agent_at_its_host_path() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("plugm");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| {
        let r = run_shards_env(&[], args, &env, TIMEOUT);
        assert_eq!(r.status, Some(0), "shards {args:?}: {}\n{}", r.stderr, r.stdout);
        r.stdout
    };
    // The host's project, and a directory beside it bound as the run starts: by their
    // real paths, so that the bound one names its top-level directory a root (macOS's
    // /private), as the project's path is.
    let files = TempDir::new("plugm-files");
    for dir in ["proj", "bound"] {
        std::fs::create_dir(files.join(dir)).unwrap();
    }
    let proj = std::fs::canonicalize(files.join("proj")).unwrap();
    let bound = std::fs::canonicalize(files.join("bound")).unwrap();
    std::fs::write(proj.join("from-host"), "host\n").unwrap();
    let dest = at_host_path(&proj);
    // The roots this host's run is given, as `shards` names them (setup.rs): its OS's own
    // and the bound directory's top.
    let mut roots: Vec<String> = if cfg!(target_os = "macos") {
        vec!["/Users".into(), "/Volumes".into()]
    } else if cfg!(windows) {
        vec!["/mnt".into()]
    } else {
        vec!["/home".into(), "/media".into(), "/mnt".into(), "/srv".into()]
    };
    let top = at_host_path(&bound)
        .split('/')
        .nth(1)
        .map(|t| format!("/{t}"))
        .unwrap();
    if cfg!(unix) && top != "/srv" && !roots.contains(&top) {
        roots.push(top);
    }

    // The agent: the test guest, which says what it saw in the project's `seen`.
    let agent = TempDir::new("plugm-agent");
    std::fs::create_dir_all(agent.join("bin")).unwrap();
    std::fs::copy(common::test_guest(), agent.join("bin/testguest")).unwrap();
    let mut command = vec!["bin/testguest".to_string(), "plugged".into(), dest.clone()];
    command.extend(roots.iter().cloned());
    let command: Vec<String> = command.iter().map(|w| format!("{w:?}")).collect();
    std::fs::write(
        agent.join("agent.json"),
        format!(r#"{{"name":"a","run":{{"command":[{}]}}}}"#, command.join(",")),
    )
    .unwrap();
    let tag = format!("127.0.0.1:{port}/team/plugm-a:1");
    shards(&["build", "agent", agent.to_str().unwrap(), "-t", &tag]);
    shards(&["push", "agent", &tag]);
    let ctx = TempDir::new("plugm-ctx");
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT a FROM {tag}\n"),
    )
    .unwrap();
    shards(&["build", "-t", "plugm:1", ctx.to_str().unwrap()]);
    let bind = format!("{}:/bound", bound.display());
    shards(&["run", "-d", "--name", "plugm", "-v", &bind, "plugm:1", "sleep"]);

    // Outside every root: refused, the roots named.
    let outside = format!("{}:/outside/proj", proj.display());
    let refused = run_shards_env(
        &["plug"],
        &["volume", "--for", "a", "plugm", &outside],
        &env,
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0), "{}", refused.stdout);
    assert!(
        refused.stderr.contains("not beneath a plug root") && refused.stderr.contains(roots[0].as_str()),
        "{}",
        refused.stderr
    );
    // Beneath one, at its host path.
    let spec = format!("{}:{dest}", proj.display());
    let said = shards(&["plug", "volume", "--for", "a", "plugm", &spec]);
    assert!(said.starts_with(&format!("{dest} ")), "{said}");
    let seen_at = proj.join("seen");
    let started = std::time::Instant::now();
    let seen = loop {
        if let Ok(seen) = std::fs::read_to_string(&seen_at) {
            break seen;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the agent wrote no {}",
            seen_at.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    for root in &roots {
        let line = format!("root {root} mkdir errno 30 create errno 30");
        assert!(seen.lines().any(|l| l == line), "no {line:?} in\n{seen}");
    }
    assert!(seen.lines().any(|l| l == "from-host host"), "{seen}");
    assert!(seen.lines().any(|l| l == "from-member ok"), "{seen}");
    assert_eq!(
        std::fs::read_to_string(proj.join("from-member")).unwrap(),
        "member\n"
    );

    // Unplugged as the agent runs; plugged again, refused nowhere.
    shards(&["unplug", "volume", "plugm", &dest]);
    shards(&["plug", "volume", "--for", "a", "plugm", &spec]);
    shards(&["unplug", "volume", "plugm", &dest]);
    shards(&["rm", "-f", "plugm"]);
}

/// The host's QEMU edu function (1234:11e8), where it has one: a test function its own
/// host gave it (`--pci-tests`), as a nested guest of the L1 tests has with
/// SHARDS_L1_PCI_TESTS (boot.rs).
#[cfg(target_os = "linux")]
fn edu_function() -> Option<String> {
    std::fs::read_dir("/sys/bus/pci/devices")
        .ok()?
        .flatten()
        .find_map(|e| {
            let read = |f: &str| std::fs::read_to_string(e.path().join(f)).ok();
            (read("vendor")?.trim() == "0x1234" && read("device")?.trim() == "0x11e8")
                .then(|| e.file_name().to_string_lossy().into_owned())
        })
}

/// A host's PCI function plugged into a detached run as it runs (D214), for its workload
/// to take through VFIO: the run given VFIO's container node alone (`--device
/// /dev/vfio/vfio`), so its microVM boots the `kvm` kernel with its slots behind its IOMMU;
/// the host's edu function, bound to vfio-pci, plugged in, its guest's vfio-pci given it,
/// its group's node made in the workload's `/dev` and its device filter widened to it, so
/// that the workload finds its group by its address as DPDK does and moves a page through
/// it by DMA, its IOMMU's mappings followed by the host's; plugged in twice refused; then
/// unplugged, its node gone and the function the host's again, twice over; then plugged by
/// its VFIO device node, into that run and into one given `/dev/iommu` at its start, and
/// taken through iommufd. Each node the image's user's (`app`) while it is plugged. A run
/// not booted for functions is refused one. Runs where a host has an edu function to give:
/// nested, `SHARDS_L1_PCI_TESTS=01.0 SHARDS_L1_TESTS=plug:a_hosts_function` (boot.rs).
#[cfg(target_os = "linux")]
#[test]
fn a_hosts_function_is_plugged_into_a_run_for_its_workload() {
    if cannot_run_vms() {
        return;
    }
    let Some(name) = edu_function() else {
        eprintln!("SKIP: this host has no QEMU edu function to pass through");
        return;
    };
    let Some(kvm) = common::kvm_kernel() else {
        eprintln!("SKIP: no kernel with VFIO and virtio-iommu for {}", common::ARCH);
        return;
    };
    // Its host's vfio-pci given it, as an administrator gives one (vfio.rst).
    let dir = std::path::Path::new("/sys/bus/pci/devices").join(&name);
    if let Ok(driver) = std::fs::read_link(dir.join("driver"))
        && driver.file_name().is_some_and(|d| d != "vfio-pci")
    {
        std::fs::write(dir.join("driver/unbind"), &name).unwrap();
    }
    std::fs::write(dir.join("driver_override"), "vfio-pci").unwrap();
    let _ = std::fs::write("/sys/bus/pci/drivers_probe", &name);

    let (image, _) = served();
    let home = TempDir::new("plugfn");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kvm.as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
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
    let refused = |command: &[&str], args: &[&str]| {
        let r = run_shards_env(command, args, &env, TIMEOUT);
        assert_ne!(r.status, Some(0), "shards {command:?} {args:?}: {}", r.stdout);
        r.stderr
    };
    shards(
        &["run"],
        &[
            "-d",
            "--name",
            "taker",
            "--device",
            "/dev/vfio/vfio",
            &image,
            "sleep",
        ],
    );
    for round in 0..2 {
        let said = shards(&["plug"], &["function", "taker", &name]);
        let words: Vec<&str> = said.split_whitespace().collect();
        assert_eq!(words.len(), 3, "round {round}: {said}");
        let (node, address) = (words[1], words[2]);
        assert!(node.starts_with("/dev/vfio/"), "round {round}: {said}");
        // The image's user's (app, 1000:1000), devtmpfs's mode kept.
        let owned = shards(&["exec"], &["taker", "/bin/testguest", "stat", node]);
        assert!(
            owned.contains(&format!("{node} other 600 1000:1000 ")),
            "round {round}: {owned}"
        );
        let dma = shards(&["exec"], &["taker", "/bin/testguest", "vfio-dma", address]);
        assert!(
            dma.contains("SHARDS-TEST INFO dma: 4096 bytes into the function and back, through its IOMMU"),
            "round {round}: {dma}"
        );
        let twice = refused(&["plug"], &["function", "taker", &name]);
        assert!(twice.contains("plugged into it already"), "{twice}");
        shards(&["unplug"], &["function", "taker", &name]);
        let gone = shards(&["exec"], &["taker", "/bin/testguest", "slurp", node]);
        assert!(gone.contains("errno 2"), "round {round}: its node, gone: {gone}");
        let after = run_shards_env(
            &["exec"],
            &["taker", "/bin/testguest", "vfio-dma", address],
            &env,
            TIMEOUT,
        );
        assert_ne!(
            after.status,
            Some(0),
            "round {round}: DMA once unplugged: {}",
            after.stdout
        );
    }
    let none = refused(&["unplug"], &["function", "taker", &name]);
    assert!(none.contains("is plugged into it"), "{none}");

    // By its VFIO device node, where this host's kernel makes one (VFIO_DEVICE_CDEV), and
    // taken through iommufd: the workload's /dev/iommu its user's too while it is plugged,
    // made for the run given /dev/vfio/vfio alone and gone after; a run given it at its
    // start (root's, devtmpfs's 0660, as a node a run is given keeps its host's mode, D44)
    // has it so again once unplugged.
    let cdev = std::fs::read_dir(dir.join("vfio-dev"))
        .ok()
        .and_then(|d| d.flatten().next())
        .map(|e| format!("/dev/vfio/devices/{}", e.file_name().to_string_lossy()));
    match cdev {
        None => eprintln!("SKIP: by its device node: this host's kernel makes none (VFIO_DEVICE_CDEV)"),
        Some(cdev) => {
            shards(
                &["run"],
                &["-d", "--name", "iommu", "--device", "/dev/iommu", &image, "sleep"],
            );
            for (run, made) in [("taker", true), ("iommu", false)] {
                let said = shards(&["plug"], &["function", run, &cdev]);
                let words: Vec<&str> = said.split_whitespace().collect();
                assert_eq!(words.len(), 3, "{run}: {said}");
                let (node, address) = (words[1], words[2]);
                assert!(node.starts_with("/dev/vfio/devices/vfio"), "{run}: {said}");
                let stat = [run, "/bin/testguest", "stat", node, "/dev/iommu"];
                let owned = shards(&["exec"], &stat);
                assert!(
                    owned.contains(&format!("{node} other 600 1000:1000 ")),
                    "{run}: {owned}"
                );
                assert!(
                    owned.contains("/dev/iommu other 660 1000:1000 "),
                    "{run}: {owned}"
                );
                let dma = shards(&["exec"], &[run, "/bin/testguest", "vfio-dma-cdev", address]);
                assert!(
                    dma.contains(
                        "SHARDS-TEST INFO dma: 4096 bytes into the function and back, through its IOMMU"
                    ),
                    "{run}: {dma}"
                );
                shards(&["unplug"], &["function", run, &cdev]);
                let after = run_shards_env(&["exec"], &stat, &env, TIMEOUT).stdout;
                assert!(after.contains(&format!("{node} missing")), "{run}: {after}");
                let iommu = if made {
                    "/dev/iommu missing"
                } else {
                    "/dev/iommu other 660 0:0 "
                };
                assert!(after.contains(iommu), "{run}: {after}");
            }
            shards(&["rm"], &["-f", "iommu"]);
        }
    }
    shards(&["rm"], &["-f", "taker"]);

    // A run booted as any is takes none.
    shards(&["run"], &["-d", "--name", "plain", &image, "sleep"]);
    let plain = refused(&["plug"], &["function", "plain", &name]);
    assert!(plain.contains("where its slots are not"), "{plain}");
    shards(&["rm"], &["-f", "plain"]);
}
