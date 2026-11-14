//! Devices plugged into a running microVM through the daemon, and its disks grown (D214):
//! `shards plug disk|volume`, `shards unplug disk|volume` and `shards grow disk` on a
//! detached run, restored from its image's template where this host's daemon keeps
//! templates and booted where it does not (Windows Server 2022, PM M345), its microVM
//! with PCI slots either way. Real VMs, on every host that runs them; the image's
//! entrypoint is the test guest (tests/common, `served`), whose modes the checks inside the
//! run are.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{TempDir, Vm, cannot_run_vms, guest_init, kernel, run_shards_env, served};

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
        let r = run_shards_env(command, args, &env);
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
    let used = run_shards_env(&["guest"], &args, &env);
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
    let shrunk = run_shards_env(&["grow"], &["disk", "plugged", "/data", "32m"], &env);
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
    let none = run_shards_env(&["unplug"], &["volume", "plugged", "/shared"], &env);
    assert!(
        none.stderr.contains("no volume is plugged in at /shared"),
        "{}",
        none.stderr
    );

    // Concurrent same-destination requests have one owner and leave no hidden mount.
    let contested = format!("{}:/contended", dir.display());
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let attempt = || {
            barrier.wait();
            run_shards_env(&["plug"], &["volume", "plugged", &contested], &env)
        };
        let a = scope.spawn(&attempt);
        let b = scope.spawn(&attempt);
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(
        results.iter().filter(|r| r.status == Some(0)).count(),
        1,
        "{results:?}"
    );
    shards(&["unplug"], &["volume", "plugged", "/contended"]);
    assert!(inside(&["slurp", "/contended/from-host"]).contains("errno 2"));

    // Off Linux no host's function is plugged in: refused by name, with its host's own
    // limit, as a run's start refuses one (D154; PM M401, M402, M403).
    #[cfg(not(target_os = "linux"))]
    {
        let limit = shards_vmm::vm::passthrough_refused();
        for function in ["0000:01:00.0", "/dev/vfio/devices/vfio0"] {
            let r = run_shards_env(&["plug"], &["function", "plugged", function], &env);
            assert_ne!(r.status, Some(0), "{function}: {}", r.stdout);
            let said = format!("shards: cannot plug into plugged: {function}: {limit}");
            assert!(r.stderr.contains(&said), "{function}: {}", r.stderr);
        }
    }

    shards(&["rm"], &["-f", "plugged"]);
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
        let r = run_shards_env(&[], args, &env);
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
    let dest = common::at_host_path(&proj);
    // The roots this host's run is given, as `shards` names them (setup.rs): its OS's own
    // and the bound directory's top.
    let roots = common::plug_roots(&bound);

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
    let refused = run_shards_env(&["plug"], &["volume", "--for", "a", "plugm", &outside], &env);
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
    let mut patience = common::Patience::of(&home);
    let seen = loop {
        if let Ok(seen) = std::fs::read_to_string(&seen_at) {
            break seen;
        }
        assert!(patience.holds(), "the agent wrote no {}", seen_at.display());
        std::thread::sleep(patience.next_look());
    };
    patience.done();
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

/// A host's edit wakes an inotify watch in the guest (D235): a watcher in a running
/// microVM, of a bind, of a read-only bind and of a volume plugged in as it runs, is told
/// of a file the host writes there (`IN_MODIFY` and its name) and of one the host removes
/// (`IN_MODIFY` of the directory itself), though nothing in the guest changed either; and
/// the host's files are as the host left them.
#[test]
fn a_host_edit_wakes_a_watch_in_the_guest() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("notice");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |command: &[&str], args: &[&str]| {
        let r = run_shards_env(command, args, &env);
        assert_eq!(
            r.status,
            Some(0),
            "shards {command:?} {args:?}: {}\n{}",
            r.stderr,
            r.stdout
        );
        r.stdout
    };
    let files = TempDir::new("notice-files");
    let dirs = ["rw", "ro", "plugged"].map(|d| files.join(d));
    for dir in &dirs {
        std::fs::create_dir(dir).unwrap();
    }
    let rw = format!("{}:/rw", dirs[0].display());
    let ro = format!("{}:/ro:ro", dirs[1].display());
    shards(
        &["run"],
        &["-d", "--name", "noticed", "-v", &rw, "-v", &ro, &image, "sleep"],
    );
    let plugged = format!("{}:/plugged", dirs[2].display());
    shards(&["plug"], &["volume", "noticed", &plugged]);
    let vm_env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.to_str().unwrap())).collect();
    for (dir, at) in dirs.iter().zip(["/rw", "/ro", "/plugged"]) {
        let mut watch = Vm::spawn_env(&["exec", "noticed", "/bin/testguest", "inotify", at], &vm_env);
        watch.wait_for("inotify ready");
        std::fs::write(dir.join("f"), "host").unwrap();
        watch.wait_for("inotify IN_MODIFY f");
        assert_eq!(std::fs::read_to_string(dir.join("f")).unwrap(), "host", "{at}");
        std::fs::remove_file(dir.join("f")).unwrap();
        watch.wait_for("inotify IN_MODIFY|IN_ISDIR ");
        watch.kill();
        let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert!(left.is_empty(), "{at}: {left:?}");
    }
    shards(&["rm"], &["-f", "noticed"]);
}

/// How long a host's edit takes to wake a watch in the guest (D235), as its host sees it:
/// from just before the host makes a file in a bind to the guest watcher's line for it
/// reaching this process, the watcher's own report back (the exec's output) included, each
/// file made once the last was heard. `SHARDS_NOTICE_SAMPLES` of them (200 by default), and
/// 10 first that are not counted. Prints n, p50, p90, p99 and max, then each sample, in
/// milliseconds. A measurement: docs/research/measurements/s8-notices/run.sh runs it.
#[test]
#[ignore = "a measurement, run by docs/research/measurements/s8-notices/run.sh"]
fn notices_latency() {
    if cannot_run_vms() {
        return;
    }
    let samples: usize = std::env::var("SHARDS_NOTICE_SAMPLES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(200);
    let (image, _) = served();
    let home = TempDir::new("notice-lat");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let files = TempDir::new("notice-lat-files");
    let bind = format!("{}:/w", files.display());
    let r = run_shards_env(
        &["run"],
        &["-d", "--name", "lat", "-v", &bind, &image, "sleep"],
        &env,
    );
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    let vm_env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.to_str().unwrap())).collect();
    let mut watch = Vm::spawn_env(&["exec", "lat", "/bin/testguest", "inotify", "/w"], &vm_env);
    watch.wait_for("inotify ready");
    let mut took = Vec::with_capacity(samples);
    for i in 0..samples + 10 {
        let name = format!("f{i}");
        let start = std::time::Instant::now();
        std::fs::write(files.join(&name), "x").unwrap();
        watch.wait_for(&format!("inotify IN_MODIFY {name}"));
        if i >= 10 {
            took.push(start.elapsed().as_secs_f64() * 1e3);
        }
    }
    watch.kill();
    let _ = run_shards_env(&["rm"], &["-f", "lat"], &env);
    let mut sorted = took.clone();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1];
    println!(
        "notices latency ms: n {} p50 {:.3} p90 {:.3} p99 {:.3} max {:.3}",
        sorted.len(),
        at(0.50),
        at(0.90),
        at(0.99),
        sorted[sorted.len() - 1]
    );
    println!(
        "samples ms: {}",
        took.iter()
            .map(|t| format!("{t:.3}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Names that differ in case alone, through a share, are what its host makes of them
/// (D235): the steps of `shards_testguest::case` taken in the guest's view of a share give
/// what they give on the host itself, in a directory beside it, whether the host's
/// filesystem tells case (Linux's) or not (macOS's APFS, Windows' NTFS): a name of another
/// case found and created over as the host finds it, and none left found once the host
/// renamed or removed it through another case. A rename of a name's case alone is held, on
/// both, to Linux's rule (fs/namei.c `vfs_rename`: two names of one inode, done, nothing
/// changed), which is all a guest's kernel does with one and Linux's own case-insensitive
/// file systems do.
#[test]
fn a_shares_names_are_its_hosts_case_and_all() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("plugc");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let files = TempDir::new("plugc-files");
    for dir in ["host", "shared"] {
        std::fs::create_dir(files.join(dir)).unwrap();
    }
    let host: Vec<String> = shards_testguest::case::steps(&files.join("host"))
        .into_iter()
        .map(|l| format!("case {l}"))
        .collect();
    let bind = format!("{}:/case", files.join("shared").display());
    let ran = run_shards_env(
        &["run"],
        &["--rm", "-u", "0", "-v", &bind, &image, "case", "/case"],
        &env,
    );
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stderr, ran.stdout);
    let guest: Vec<&str> = ran.stdout.lines().filter(|l| l.starts_with("case ")).collect();
    let differing: Vec<String> = host
        .iter()
        .zip(&guest)
        .filter(|(h, g)| h.as_str() != **g)
        .map(|(h, g)| format!("host {h:?}, guest {g:?}"))
        .collect();
    assert!(
        differing.is_empty() && host.len() == guest.len(),
        "{} steps; the guest's differ from the host's:\n{}\nhost:\n{}\nguest:\n{}",
        host.len(),
        differing.join("\n"),
        host.join("\n"),
        guest.join("\n")
    );
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
        let r = run_shards_env(command, args, &env);
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
        let r = run_shards_env(command, args, &env);
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
        let after = run_shards_env(&["exec"], &["taker", "/bin/testguest", "vfio-dma", address], &env);
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
                let after = run_shards_env(&["exec"], &stat, &env).stdout;
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
