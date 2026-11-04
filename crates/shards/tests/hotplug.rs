//! Devices plugged into a running microVM's PCI bus, and its disks grown, as it runs
//! (D214): real guests on the host's hypervisor, booted in this process (one VM a process,
//! as Hypervisor.framework allows), the test guest told each step on its console.
//!
//! `SHARDS_PLUG_ROUNDS` plugs, reads, writes and unplugs a disk that many times, and says
//! each step's times as n, p50, p90, p99 and max (PM M430's harness,
//! docs/research/measurements/pci-hotplug).

#![allow(clippy::panic)]

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{cannot_run_vms, kernel, test_guest};
use shards_testdir::TempDir;
use shards_vmm::vm::{self, Config, Console, ExitReason, Handle};

const TIMEOUT: Duration = Duration::from_secs(60);
const MIB: u64 = 1 << 20;
const SALT: u64 = 0x5107;

/// Waits for the guest's console to say `line`, and gives what it said before it since the
/// last time it was asked: the step's answer.
fn until(handle: &Handle, line: &str) -> String {
    let end = Instant::now() + TIMEOUT;
    loop {
        let said = String::from_utf8_lossy(&handle.console_said()).into_owned();
        if let Some(before) = said.rsplit_once(line).map(|(before, _)| before) {
            return before.to_string();
        }
        if Instant::now() > end {
            handle.stop();
            panic!("the guest did not say {line:?} in {TIMEOUT:?}: {said}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The words after the last `prefix` line of `said`.
fn answer(said: &str, prefix: &str) -> Vec<String> {
    let line = said
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(prefix).map(str::to_string))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {said}"));
    line.split_whitespace().map(str::to_string).collect()
}

/// n, p50, p90, p99 and max of `samples`, nearest rank.
fn stats(name: &str, samples: &mut [u128]) -> String {
    samples.sort_unstable();
    let rank = |p: usize| {
        samples
            .get((samples.len() * p).div_ceil(100).saturating_sub(1))
            .copied()
            .unwrap_or_default()
    };
    format!(
        "PLUG-STATS {name}: n {} p50 {} p90 {} p99 {} max {} us",
        samples.len(),
        rank(50),
        rank(90),
        rank(99),
        samples.last().copied().unwrap_or_default()
    )
}

/// A disk plugged into a running microVM, read by its guest as it holds the pattern written
/// to its file, written by it, grown by its host and seen grown, and taken off again once
/// its guest let it go, which the host may not do before; a shared directory plugged in,
/// mounted, read and written; and a disk it was made with grown as it runs.
#[test]
fn devices_are_plugged_into_a_running_microvm_and_its_disks_grow() {
    if cannot_run_vms() {
        return;
    }
    let rounds: usize = std::env::var("SHARDS_PLUG_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(3);
    let dir = TempDir::new("plug").unwrap();
    let pattern = |salt: u64, offset: u64, len: u64| {
        let mut b = vec![0u8; len as usize];
        shards_testguest::fill(salt, offset, &mut b);
        b
    };
    let boot_disk = dir.join("boot");
    std::fs::write(&boot_disk, pattern(1, 0, MIB)).unwrap();
    let disk = dir.join("plugged");
    std::fs::write(&disk, pattern(SALT, 0, 4 * MIB)).unwrap();
    let shared = dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("hello"), "hello from a plugged share\n").unwrap();

    let mut cfg = Config::new(kernel().to_path_buf(), Some(test_guest().to_path_buf()));
    cfg.cmdline = "console=ttyS0 panic=-1 quiet shards_test=pci-plug".into();
    cfg.memory_mib = 512;
    cfg.console = Console::Discard;
    cfg.pci_slots = 4;
    cfg.disks.push(vm::Disk {
        path: boot_disk.clone(),
        read_only: false,
    });
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait());
    });
    let mut step = 0;
    let mut ask = |handle: &Handle, what: String| {
        until(handle, &format!("SHARDS-PLUG WAIT {step}"));
        handle.console_input(format!("{what}\n").as_bytes());
        step += 1;
        until(handle, &format!("SHARDS-PLUG WAIT {step}"))
    };

    // The disk it was made with, grown as it runs, once its driver has it: its driver told.
    until(&handle, "SHARDS-PLUG WAIT 0");
    let grown = std::fs::OpenOptions::new().write(true).open(&boot_disk).unwrap();
    grown.set_len(2 * MIB).unwrap();
    assert_eq!(handle.disk_size(0).unwrap().grow().unwrap(), 2 * MIB);
    let said = ask(&handle, format!("grow vda {}", 2 * MIB / 512));
    assert_eq!(answer(&said, "SHARDS-PLUG GREW ")[0], "vda", "{said}");

    let (mut plugs, mut rescans, mut readies, mut removes, mut unplugs) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for round in 0..rounds {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk)
            .unwrap();
        let t0 = Instant::now();
        let plugged = handle
            .plug_disk(file, false, "plugged")
            .unwrap_or_else(|e| panic!("{e}"));
        plugs.push(t0.elapsed().as_micros());
        let said = ask(&handle, format!("disk {} {SALT}", plugged.address));
        let words = answer(&said, "SHARDS-PLUG DISK ");
        let name = words[0].clone();
        rescans.push(words[1].parse().unwrap());
        readies.push(words[2].parse().unwrap());
        // Driven by its guest: not to be taken from under it.
        let refused = handle.unplug(plugged.devfn).unwrap_err();
        assert!(refused.contains("still drives it"), "{refused}");
        if round == 0 {
            let file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
            file.set_len(8 * MIB).unwrap();
            assert_eq!(plugged.size.as_ref().unwrap().grow().unwrap(), 8 * MIB);
            let said = ask(&handle, format!("grow {name} {}", 8 * MIB / 512));
            assert_eq!(answer(&said, "SHARDS-PLUG GREW ")[0], name, "{said}");
            let shrunk = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
            shrunk.set_len(4 * MIB).unwrap();
            let e = plugged.size.as_ref().unwrap().grow().unwrap_err();
            assert!(e.contains("only grows"), "{e}");
            shrunk.set_len(8 * MIB).unwrap();
        }
        let said = ask(&handle, format!("remove {}", plugged.address));
        removes.push(answer(&said, "SHARDS-PLUG REMOVED ")[0].parse().unwrap());
        let t0 = Instant::now();
        handle.unplug(plugged.devfn).unwrap_or_else(|e| panic!("{e}"));
        unplugs.push(t0.elapsed().as_micros());
        let written = std::fs::read(&disk).unwrap();
        assert_eq!(
            written[MIB as usize..2 * MIB as usize],
            pattern(SALT + 1, MIB, MIB)[..],
            "what the guest wrote to the plugged disk"
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&disk)
            .unwrap()
            .write_all(&pattern(SALT, 0, 4 * MIB))
            .unwrap();
    }
    let unplugged = handle.unplug(0x10).unwrap_err();
    assert!(unplugged.contains("no function is plugged"), "{unplugged}");

    // A shared directory, served by a server of this process's (a Unix host's here: a
    // Windows host's server is reached through a pipe its daemon makes).
    #[cfg(unix)]
    {
        let server = shards_vmm::devices::virtio::fs::server::Server::new(
            std::fs::File::open(&shared).unwrap().into(),
            false,
            None,
        )
        .unwrap();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::spawn(move || shards_vmm::devices::virtio::fs::answer(&server, theirs));
        let slot: shards_vmm::devices::virtio::fs::Share = std::sync::Arc::default();
        slot.attach(ours).unwrap();
        let plugged = handle
            .plug_share("plugged", slot)
            .unwrap_or_else(|e| panic!("{e}"));
        let said = ask(&handle, format!("share {} plugged", plugged.address));
        eprintln!(
            "PLUG-STATS share plug to first write: {} us",
            answer(&said, "SHARDS-PLUG SHARE ")[0]
        );
        ask(&handle, format!("remove {}", plugged.address));
        handle.unplug(plugged.devfn).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            std::fs::read_to_string(shared.join("from-guest")).unwrap(),
            "written through a plugged share\n"
        );
    }

    until(&handle, &format!("SHARDS-PLUG WAIT {step}"));
    handle.console_input(b"done\n");
    let said = until(&handle, "SHARDS-TEST PASS");
    match rx.recv_timeout(TIMEOUT) {
        Ok(reason) => assert_eq!(reason, ExitReason::PowerOff, "{said}"),
        Err(_) => {
            handle.stop();
            panic!("the guest did not power off in {TIMEOUT:?}");
        }
    }
    // What a PCI bus with slots and nothing on it costs a boot: from its vCPUs' release to
    // its PID 1's first word, booted alternately with four slots and with no bus.
    let boots: usize = std::env::var("SHARDS_PLUG_BOOTS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    let (mut with, mut without) = (Vec::new(), Vec::new());
    for i in 0..2 * boots {
        let slots = if i % 2 == 0 { 4 } else { 0 };
        let mut cfg = Config::new(kernel().to_path_buf(), Some(test_guest().to_path_buf()));
        cfg.cmdline = "console=ttyS0 panic=-1 quiet shards_test=pci-plug".into();
        cfg.memory_mib = 512;
        cfg.console = Console::Discard;
        cfg.pci_slots = slots;
        let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
        until(&handle, "SHARDS-PLUG WAIT 0");
        let init = handle
            .markers()
            .into_iter()
            .find(|&(m, _)| m == shards_abi::marker::INIT_STARTED)
            .map(|(_, at)| at)
            .unwrap();
        let boot = init - handle.released_at_us().unwrap();
        if slots > 0 { &mut with } else { &mut without }.push(boot);
        handle.console_input(b"done\n");
        assert_eq!(running.wait(), ExitReason::PowerOff);
    }
    if boots > 0 {
        eprintln!("{}", stats("boot to init, 4 slots", &mut with));
        eprintln!("{}", stats("boot to init, no PCI bus", &mut without));
    }
    for (name, samples) in [
        ("host plug", &mut plugs),
        ("guest rescan", &mut rescans),
        ("guest rescan to first read", &mut readies),
        ("guest remove", &mut removes),
        ("host unplug", &mut unplugs),
    ] {
        eprintln!("{}", stats(name, samples));
    }
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

/// A host's PCI function passed through to a running microVM (D214), outside its guest's
/// IOMMU: its guest rescans, finds QEMU edu's IDs at the place it was plugged into, and
/// reads its identification register and liveness check through its BAR 0, mapped from
/// sysfs where the guest placed it, the BAR's window a memory slot of KVM's that moved
/// with it; then lets it go, and it is unplugged, KVM told to forget its group. Twice, as
/// a function let go is the host's again. Runs where a host has an edu function to give:
/// nested, `SHARDS_L1_PCI_TESTS=01.0 SHARDS_L1_TESTS=hotplug` (boot.rs).
#[cfg(target_os = "linux")]
#[test]
fn a_hosts_function_is_plugged_into_a_running_microvm() {
    if cannot_run_vms() {
        return;
    }
    let Some(name) = edu_function() else {
        eprintln!("SKIP: this host has no QEMU edu function to pass through");
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
    let f = vm::Vfio::of(&name).unwrap_or_else(|e| panic!("{e}"));

    let mut cfg = Config::new(kernel().to_path_buf(), Some(test_guest().to_path_buf()));
    cfg.cmdline = "console=ttyS0 panic=-1 quiet shards_test=pci-plug".into();
    cfg.memory_mib = 512;
    cfg.console = Console::Discard;
    cfg.pci_slots = 2;
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let mut step = 0;
    let mut ask = |handle: &Handle, what: String| {
        until(handle, &format!("SHARDS-PLUG WAIT {step}"));
        handle.console_input(format!("{what}\n").as_bytes());
        step += 1;
        until(handle, &format!("SHARDS-PLUG WAIT {step}"))
    };
    for _ in 0..2 {
        let plugged = handle.plug_vfio(&f).unwrap_or_else(|e| panic!("{e}"));
        let said = ask(&handle, format!("vfio {}", plugged.address));
        eprintln!(
            "PLUG-STATS a host's function, rescan to its BAR read: {} us",
            answer(&said, "SHARDS-PLUG VFIO ")[0]
        );
        ask(&handle, format!("remove {}", plugged.address));
        handle.unplug(plugged.devfn).unwrap_or_else(|e| panic!("{e}"));
    }
    until(&handle, &format!("SHARDS-PLUG WAIT {step}"));
    handle.console_input(b"done\n");
    let said = until(&handle, "SHARDS-TEST PASS");
    assert_eq!(running.wait(), ExitReason::PowerOff, "{said}");
}
