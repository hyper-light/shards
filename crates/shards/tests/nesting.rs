//! A nested runtime makes boxes in its own microVM under the nesting grant, and not without
//! it (D234): real microVMs on this host's hypervisor, runc as the stand-in nested runtime,
//! and the test guest's probes (crates/testguest/src/nesting.rs) for the kernel's side.
//!
//! Each test names the guards it holds. For ROOT's run, each guard's mutant and the test
//! that fails on it:
//! - the daemon's consent check (an image without NESTING granted): `no_box_is_made_
//!   without_both_words_or_beside_a_domain` (refused);
//! - the build's refusal beside a domain: the same test (built);
//! - the daemon's re-run of the build's checks, and init's re-check, on a crafted image:
//!   the same test (crafted);
//! - the joiner's refusal, by the daemon and by init: `an_exec_takes_the_commands_grant_
//!   and_a_joiner_is_refused`;
//! - the leaf (the command in its namespace root) and each exec's leaf: `a_granted_commands_
//!   runtime_makes_boxes_in_its_own_subtree` and the exec test (`workload cgroup /init`);
//! - the writable hierarchy, CAP_SYS_ADMIN, CAP_NET_ADMIN, the filter's pivot_root, and
//!   D121's mount filter left off: the boxes of `a_granted_commands_runtime_makes_boxes_
//!   in_its_own_subtree` (runc fails at its first step that needs each);
//! - nsdelegate's boundary: `a_granted_workload_is_held_to_its_subtree`;
//! - the readiness relay: `a_granted_workloads_readiness_reaches_the_host`;
//! - the deadline's conversion and passthrough through shards-api, and READY's relay to
//!   the run host: `a_deadline_given_through_shards_api_is_met_or_said_passed`;
//! - the workload's device program, attached BPF_F_ALLOW_MULTI: `a_granted_runs_boxes_
//!   reach_its_gpu_as_their_rules_and_its_own_say` (box `beyond`);
//! - AF_VSOCK still refused: `a_granted_workload_still_reaches_no_host_port_over_vsock`.
//! - D251's cut through the guest's legs, init's hang-up probe and the vsock muxer's `Cut`:
//!   `a_nested_workloads_socket_cut_mid_stream_resets_its_lane_at_the_controller` (`Ok(0)`
//!   where `ConnectionReset` is due); a half-close taken for a cut by either:
//!   `a_nested_workloads_half_close_ends_its_way_in_order_while_the_controllers_goes_on`.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{Artifact, Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(180);

/// What a run asks to be given the grant with, beside a tmpfs at `/run` for runc's state
/// and its boxes' bundles (D234's own example).
const GRANTED: [&str; 4] = ["--security-opt", "nesting=true", "--tmpfs", "/run"];

/// What the daemon says of a run of an image declaring the grant that does not ask for it
/// (crates/shards/src/agentfile.rs, `NESTING_UNASKED`): it runs without it, and says so.
const UNASKED: &str = "WARNING: the nesting grant withheld from the command: the image's Agentfile declares NESTING, and the run does not ask for it (--security-opt nesting=true)";

/// runc v1.5.2 for this host's architecture, which is its guests' (opencontainers/runc's
/// release, static-pie, as the test image has no libc), pinned by content: the release's
/// own SHA-256 of each asset (runc.sha256sum). Each binary's signature was checked against
/// runc.keyring at the tag as it was pinned (docs/research/measurements/s38-nested-grants/
/// step5/). Fetched once for the binary's tests, which run side by side, as
/// [`common::fetch`] names its download by the process alone.
fn runc() -> &'static Path {
    static RUNC: OnceLock<PathBuf> = OnceLock::new();
    RUNC.get_or_init(fetch_runc)
}

fn fetch_runc() -> PathBuf {
    let artifact = match std::env::consts::ARCH {
        "aarch64" => Artifact {
            name: "runc-1.5.2-arm64",
            url: "https://github.com/opencontainers/runc/releases/download/v1.5.2/runc.arm64",
            sha256: "d10ecae898361832a059be2089bab92d158aec54661b18ed7346ed79628b46b0",
        },
        "x86_64" => Artifact {
            name: "runc-1.5.2-amd64",
            url: "https://github.com/opencontainers/runc/releases/download/v1.5.2/runc.amd64",
            sha256: "599f6f94ff8c5057241eff0d54c3c74f95c34935b6457b33fe545defc61e9488",
        },
        other => panic!("no runc is pinned for {other}"),
    };
    common::fetch(&artifact)
}

/// One test's home, its daemon stopped as it goes.
struct Home {
    dir: TempDir,
    kernel: PathBuf,
    vm: Option<PathBuf>,
}

impl Home {
    fn new(name: &str) -> Home {
        Home {
            dir: TempDir::new(name),
            kernel: kernel().to_path_buf(),
            vm: None,
        }
    }

    fn env(&self) -> Vec<(&str, &OsStr)> {
        let mut env = vec![
            ("SHARDS_HOME", self.dir.as_os_str()),
            ("SHARDS_KERNEL", self.kernel.as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ];
        if let Some(vm) = &self.vm {
            env.push(("SHARDS_VM_BINARY", vm.as_os_str()));
        }
        env
    }

    fn shards(&self, command: &[&str], args: &[&str]) -> Run {
        run_shards_env(command, args, &self.env(), TIMEOUT)
    }

    /// Builds the test image with runc at /usr/local/bin/runc, its command run as root, then
    /// `directives`, as `tag`; its context holds `helper`, an agent of files alone, for the
    /// Agentfiles that name one. The served image runs as `app`, but a nested runtime runs
    /// as root, as D234's own example image does: runc at any other euid is rootless and
    /// wants a user namespace (runc v1.5.2's utils_linux.go), and a command whose UIDs all
    /// leave 0 keeps none of the grant's capabilities (capabilities(7)). So each case, given
    /// the grant or not, holds the grant alone.
    fn build(&self, tag: &str, directives: &str) -> Run {
        let (image, _) = served();
        let ctx = TempDir::new("nesting-ctx");
        std::fs::copy(runc(), ctx.join("runc")).unwrap();
        std::fs::create_dir(ctx.join("helper")).unwrap();
        std::fs::write(ctx.join("helper/notes.txt"), "files alone\n").unwrap();
        std::fs::write(
            ctx.join("Agentfile"),
            format!("FROM {image}\nCOPY --chmod=0755 runc /usr/local/bin/runc\nUSER root\n{directives}"),
        )
        .unwrap();
        self.shards(&["build", "image"], &["-q", "-t", tag, ctx.to_str().unwrap()])
    }

    /// [`Home::build`] of an image declaring the grant, as `nesting:1`, which must build.
    fn nesting_image(&self) {
        let built = self.build("nesting:1", "NESTING\n");
        assert_eq!(built.status, Some(0), "{}", built.stderr);
    }

    /// `run vm --rm` of `nesting:1` given the grant, its command `args`.
    fn granted(&self, args: &[&str]) -> Run {
        let mut all = vec!["--rm"];
        all.extend(GRANTED);
        all.push("nesting:1");
        all.extend_from_slice(args);
        self.shards(&["run", "vm"], &all)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = run_shards_env(&["stop", "daemon"], &[] as &[&str], &self.env(), TIMEOUT);
    }
}

fn shown(ran: &Run) -> String {
    format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr)
}

/// Every line of `lines` is a line of what `ran` wrote.
fn said(ran: &Run, lines: &[&str]) {
    for want in lines {
        assert!(
            ran.stdout.lines().any(|l| l == *want),
            "no {want:?} in\n{}",
            shown(ran)
        );
    }
}

fn now() -> String {
    format!(
        "{:.9}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    )
}

/// What the stand-in runtime makes of a granted workload's subtree, from its command or an
/// exec into it (testguest `nesting limits`): the process sits in its leaf; a box held to 8
/// processes forks 7 and its 8th fails EAGAIN, as its pids.events counts; a box held to 32
/// MiB, with no swap past it, is killed inside its cgroup (runc's status 137), as its
/// memory.events counts; a box is PID 1 of its own namespace in a root of its own
/// (pivot_root), its own cgroup namespace's root its cgroup.
const LIMITS: [&str; 12] = [
    "workload cgroup /init",
    "box pids: forked 7, then errno 11",
    "box pids: status 0",
    "box pids: pids.events max 1",
    "box memory: status 137",
    "box memory: memory.events oom_kill 1",
    "box self: pid 1",
    "box self: /testguest present",
    "box self: /bin/testguest absent",
    "box self: /etc/passwd absent",
    "box self: cgroup /",
    "box self: status 0",
];

/// Case (a): runc, run by a granted command, makes boxes with limits in cgroups it makes
/// under the delegated subtree, each with its own PID namespace and root. Guards: the
/// leaf, the writable hierarchy, CAP_SYS_ADMIN and CAP_NET_ADMIN, the filter's pivot_root
/// and D121's mount filter left off, each of which runc needs at one of its steps.
#[test]
fn a_granted_commands_runtime_makes_boxes_in_its_own_subtree() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-a");
    home.nesting_image();
    let ran = home.granted(&["nesting", "limits"]);
    assert_eq!(ran.status, Some(0), "{}", shown(&ran));
    said(&ran, &LIMITS);
}

/// An exec into a granted run takes its command's grant, in its leaf, as runc's exec takes
/// a container's process spec (D234): its runtime makes the same boxes. A run joining the
/// granted run's microVM is refused, in the words the daemon and init share, as the grant
/// makes the command its microVM's only principal. Guards: each exec's leaf, the joiner's
/// refusal.
#[test]
fn an_exec_takes_the_commands_grant_and_a_joiner_is_refused() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-exec");
    home.nesting_image();
    let mut args = vec!["-d", "--name", "nesting-held"];
    args.extend(GRANTED);
    args.extend(["nesting:1", "nesting", "hold"]);
    let held = home.shards(&["run", "vm"], &args);
    assert_eq!(held.status, Some(0), "{}", shown(&held));
    let exec = home.shards(
        &["exec", "vm"],
        &["nesting-held", "/bin/testguest", "nesting", "limits"],
    );
    assert_eq!(exec.status, Some(0), "{}", shown(&exec));
    said(&exec, &LIMITS);
    let (image, _) = served();
    let joined = home.shards(
        &["run", "vm"],
        &["--rm", "--network", "container:nesting-held", &image, "report"],
    );
    assert_ne!(joined.status, Some(0), "{}", shown(&joined));
    let words = shards_abi::run::nesting::joiner();
    assert!(joined.stderr.contains(&words), "{}", shown(&joined));
    let removed = home.shards(&["remove", "vm"], &["-f", "nesting-held"]);
    assert_eq!(removed.status, Some(0), "{}", shown(&removed));
}

/// Case (b): the subtree holds the workload. Its namespace root's own limits stay init's
/// (EPERM: nsdelegate); a cgroup2 mount it makes itself shows the same root and nothing
/// above it; and below that root, a process in a cgroup namespace of its own moves neither
/// itself out nor another process in from outside (ENOENT), the boundary that holds the
/// workload's boxes as the workload's own holds it. Guard: nsdelegate's boundary.
#[test]
fn a_granted_workload_is_held_to_its_subtree() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-b");
    home.nesting_image();
    let ran = home.granted(&["nesting", "contain"]);
    assert_eq!(ran.status, Some(0), "{}", shown(&ran));
    said(
        &ran,
        &[
            "root memory.max: errno 1",
            "root pids.max: errno 1",
            "inner: joined done",
            "inner: unshared done",
            "inner: out of its namespace errno 2",
            "inner: into its namespace from outside errno 2",
        ],
    );
    assert!(
        ran.stdout
            .lines()
            .any(|l| l.starts_with("fresh mount: the same root [")),
        "{}",
        shown(&ran)
    );
}

/// Case (c): no box without both words, and none beside a domain. Given the grant, a nested
/// runtime's first steps are done (the probe works). Without the run's consent, an image
/// declaring the grant runs without it, and says so; its first cgroup step fails EROFS, its
/// first namespace step EPERM, and runc fails. A run asking for it of an image declaring no
/// NESTING is refused; an Agentfile declaring it beside an agent does not build; and an
/// image whose normalized Agentfile was given `nesting` beside its agent by hand is refused
/// as the build refuses it, in the same words. Guards: the consent check, the build's
/// refusal beside a domain, the daemon's re-run of the build's checks and init's re-check.
#[test]
fn no_box_is_made_without_both_words_or_beside_a_domain() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-c");
    home.nesting_image();
    let ran = home.granted(&["nesting", "first-step"]);
    assert_eq!(ran.status, Some(0), "{}", shown(&ran));
    said(
        &ran,
        &[
            "first cgroup step: done",
            "first namespace step: done",
            "box first: status 0",
        ],
    );

    let unasked = home.shards(
        &["run", "vm"],
        &["--rm", "--tmpfs", "/run", "nesting:1", "nesting", "first-step"],
    );
    assert_eq!(unasked.status, Some(0), "{}", shown(&unasked));
    assert!(
        unasked.stderr.lines().any(|l| l == UNASKED),
        "no word of the grant withheld: {}",
        shown(&unasked)
    );
    said(
        &unasked,
        &["first cgroup step: errno 30", "first namespace step: errno 1"],
    );
    assert!(
        unasked
            .stdout
            .lines()
            .any(|l| l.starts_with("box first: status ") && l != "box first: status 0"),
        "{}",
        shown(&unasked)
    );

    let plain = home.build("plain:1", "");
    assert_eq!(plain.status, Some(0), "{}", plain.stderr);
    let mut args = vec!["--rm"];
    args.extend(GRANTED);
    args.extend(["plain:1", "nesting", "first-step"]);
    let refused = home.shards(&["run", "vm"], &args);
    assert_ne!(refused.status, Some(0), "{}", shown(&refused));
    let words = shards_abi::run::nesting::undeclared();
    assert!(refused.stderr.contains(&words), "{}", shown(&refused));

    let beside = shards_agentfile::agentfile::nesting_beside(
        shards_agentfile::agentfile::TargetKind::Agent,
        b"helper",
    );
    let built = home.build("beside:1", "AGENT helper FROM ./helper\nNESTING\n");
    assert_ne!(built.status, Some(0), "{}", shown(&built));
    assert!(built.stderr.contains(&beside), "{}", shown(&built));

    let built = home.build("crafted:1", "AGENT helper FROM ./helper\n");
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let created = home.shards(
        &["create", "vm"],
        &["--name", "crafted-c", "crafted:1", "nesting", "hold"],
    );
    assert_eq!(created.status, Some(0), "{}", shown(&created));
    let host = TempDir::new("nesting-crafted");
    let spec_at = host.join("spec.json");
    let out = home.shards(
        &["copy", "vm"],
        &["crafted-c:/.agentfile.json", spec_at.to_str().unwrap()],
    );
    assert_eq!(out.status, Some(0), "{}", shown(&out));
    let mut spec: serde_json::Value = serde_json::from_slice(&std::fs::read(&spec_at).unwrap()).unwrap();
    spec["nesting"] = serde_json::Value::Bool(true);
    let crafted = serde_json::to_vec(&spec).unwrap();
    let crafted_at = host.join("crafted.json");
    std::fs::write(&crafted_at, &crafted).unwrap();
    let put = home.shards(
        &["copy", "vm"],
        &[crafted_at.to_str().unwrap(), "crafted-c:/.agentfile.json"],
    );
    assert_eq!(put.status, Some(0), "{}", shown(&put));
    let label = format!(
        "LABEL vnd.osi.agentfile.digest={}",
        common::sha256_digest(&crafted)
    );
    let committed = home.shards(&["commit", "vm"], &["--change", &label, "crafted-c", "crafted:2"]);
    assert_eq!(committed.status, Some(0), "{}", shown(&committed));
    let mut args = vec!["--rm"];
    args.extend(GRANTED);
    args.extend(["crafted:2", "nesting", "first-step"]);
    let refused = home.shards(&["run", "vm"], &args);
    assert_ne!(refused.status, Some(0), "{}", shown(&refused));
    assert!(refused.stderr.contains(&beside), "{}", shown(&refused));
    let removed = home.shards(&["remove", "vm"], &["-f", "crafted-c"]);
    assert_eq!(removed.status, Some(0), "{}", shown(&removed));
}

/// Case (d): a granted workload's `READY=1` to its NOTIFY_SOCKET reaches the host as the
/// run's `ready` event, read from the home's events journal. A run from the command line
/// gives no deadline (D234), so its workload has none and no `ready-by-passed` is said.
/// Guard: the readiness relay.
#[test]
fn a_granted_workloads_readiness_reaches_the_host() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-d");
    home.nesting_image();
    let since = now();
    let mut args = vec!["-d", "--name", "nesting-ready"];
    args.extend(GRANTED);
    args.extend(["nesting:1", "nesting", "ready", "500"]);
    let started = home.shards(&["run", "vm"], &args);
    assert_eq!(started.status, Some(0), "{}", shown(&started));
    let actions = |ran: &Run| -> Vec<String> {
        ran.stdout
            .lines()
            .filter_map(|l| l.split(' ').nth(2).map(str::to_string))
            .collect()
    };
    let begun = Instant::now();
    let told = loop {
        let until = now();
        let events = home.shards(
            &["watch", "system"],
            &[
                "--since",
                &since,
                "--until",
                &until,
                "--filter",
                "container=nesting-ready",
            ],
        );
        assert_eq!(events.status, Some(0), "{}", shown(&events));
        let told = actions(&events);
        if told.iter().any(|a| a == "ready") {
            break told;
        }
        assert!(
            begun.elapsed() < TIMEOUT,
            "no ready event in {TIMEOUT:?}: {}",
            shown(&events)
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(!told.iter().any(|a| a == "ready-by-passed"), "{told:?}");
    let logs = home.shards(&["read", "logs"], &["nesting-ready"]);
    said(
        &logs,
        &[
            "ready-by none",
            "notify /run/shards/notify",
            "sent READY=1: 7 bytes",
        ],
    );
    let removed = home.shards(&["remove", "vm"], &["-f", "nesting-ready"]);
    assert_eq!(removed.status, Some(0), "{}", shown(&removed));
}

/// The variables a run host is given through shards-api (D231): this home, this build's
/// guest, and the tests' own docker config, with no local store (tests/api_image.rs's).
fn host_env(home: &Home) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vec![
        ("SHARDS_HOME".into(), home.dir.as_os_str().into()),
        ("SHARDS_KERNEL".into(), home.kernel.as_os_str().into()),
        ("SHARDS_INIT".into(), guest_init().as_os_str().into()),
        ("SHARDS_LOCAL_STORE".into(), "none".into()),
        ("DOCKER_CONFIG".into(), common::docker_config().as_os_str().into()),
    ]
}

/// Case (d), its deadline: given through shards-api's image spec (D231's image boot,
/// `ready_by`), a budget the workload meets says it ready on time; a budget spent before
/// the run reaches its guest says its ready-by passed at once (`SHARDS_READY_BY=0`), then
/// ready, late, at its `READY=1`; each as the orchestrator reads it (`wait_ready`). Guards:
/// the budget's conversion and passthrough, and READY's relay to the run host.
#[test]
fn a_deadline_given_through_shards_api_is_met_or_said_passed() {
    use shards_api::image::{ImageShards, ImageSpec, Images as _, Narrowing, Ready};
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-d-api");
    home.nesting_image();
    let spec = |ready_by: Duration| ImageSpec {
        image: "nesting:1".into(),
        vcpus: 1,
        memory_mib: 512,
        // The whole argv: api-run gives the run an empty entrypoint, so the image's
        // `/bin/testguest` is not composed in (crates/shards/src/api_run.rs).
        command: Some(
            ["/bin/testguest", "nesting", "ready", "500"]
                .map(String::from)
                .to_vec(),
        ),
        members: Vec::new(),
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        lessor: None,
        shares: Vec::new(),
        binary: Some(common::shards().to_path_buf()),
        env: host_env(&home),
        nesting: true,
        ready_by: Some(ready_by),
        control: None,
        gpu_devices: Vec::new(),
        sign_ins: Vec::new(),
    };
    let guest = ImageShards::new().boot(&spec(TIMEOUT), TIMEOUT).unwrap();
    assert_eq!(guest.wait_ready(None, TIMEOUT).unwrap(), Some(Ready::OnTime));
    drop(guest);
    let guest = ImageShards::new().boot(&spec(Duration::ZERO), TIMEOUT).unwrap();
    let passed = guest.wait_ready(None, TIMEOUT).unwrap();
    assert!(matches!(passed, Some(Ready::Passed(_))), "{passed:?}");
    let late = guest.wait_ready(passed, TIMEOUT).unwrap();
    assert!(matches!(late, Some(Ready::Late(_))), "{late:?}");
}

/// The kernel of a guest given this host's GPU and the VM process that gives it, as
/// tests/gpu.rs finds them; or why this host has none to give. A Mac's GPU is Metal's
/// wherever its VMs run; a Linux host's is a render node or NVIDIA's device; a Windows
/// host's, a display adapter naming a Vulkan driver.
fn gpu() -> Result<(PathBuf, Option<PathBuf>), &'static str> {
    let host_gpu = if cfg!(target_os = "macos") {
        true
    } else if cfg!(windows) {
        std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
                "/s",
                "/v",
                "VulkanDriverName",
            ])
            .output()
            .is_ok_and(|o| o.status.success())
    } else {
        std::fs::read_dir("/dev/dri").is_ok_and(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        }) || std::path::Path::new("/dev/nvidia0").exists()
    };
    if !host_gpu {
        return Err("this host has no GPU to give a microVM");
    }
    let vm = std::env::var_os("SHARDS_TEST_GPU_VM").map(PathBuf::from);
    if vm.is_none() && !common::carries_gpu_vm() {
        return Err("this shards carries no GPU VM process (SHARDS_GPU_LIBS)");
    }
    let kernel = match std::env::var_os("SHARDS_TEST_GPU_KERNEL") {
        Some(k) => PathBuf::from(k),
        None => common::gpu_kernel()
            .ok_or("no gpu kernel is pinned for this architecture")?
            .to_path_buf(),
    };
    Ok((kernel, vm))
}

/// Case (e), on a host with a GPU, its render node given the run (`--gpus all`): the
/// workload opens it; a box given the node and a rule for it opens it; one given the node
/// and no rule cannot (its own device program); one given card0 and a rule for it cannot
/// either, as the workload's own program, attached BPF_F_ALLOW_MULTI above the box's,
/// never gave the workload card0 (f20). Guard: the workload's device program.
#[test]
fn a_granted_runs_boxes_reach_its_gpu_as_their_rules_and_its_own_say() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (gpu_kernel, vm) = match gpu() {
        Ok(found) => found,
        Err(why) => {
            eprintln!("SKIP: {why}");
            return;
        }
    };
    let mut home = Home::new("nesting-e");
    home.kernel = gpu_kernel;
    home.vm = vm;
    home.nesting_image();
    let mut args = vec!["--rm", "--gpus", "all"];
    args.extend(GRANTED);
    args.extend(["nesting:1", "nesting", "gpu"]);
    let ran = home.shards(&["run", "vm"], &args);
    assert_eq!(ran.status, Some(0), "{}", shown(&ran));
    said(
        &ran,
        &[
            "workload open /dev/dri/renderD128: ok",
            "box given: open /dev/dri/renderD128: ok",
            "box denied: open /dev/dri/renderD128: errno 1",
            "box beyond: open /dev/dri/card0: errno 1",
        ],
    );
}

/// Case (f): the workload's end is its run's, with its status, though a box it started
/// still holds: the box is in the workload's PID namespace, which ends with it (f13), and
/// the run waits on nothing of it.
#[test]
fn a_granted_run_ends_with_its_command_and_its_boxes_with_it() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-f");
    home.nesting_image();
    let ran = home.granted(&["nesting", "exit", "7"]);
    assert_eq!(ran.status, Some(7), "{}", shown(&ran));
    said(&ran, &["exiting 7"]);
    assert!(
        ran.stdout.lines().any(|l| l
            .strip_prefix("box held: status 0 pid ")
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))),
        "{}",
        shown(&ran)
    );
}

/// Case (g): the grant opens no way to the host over vsock: Docker's filter, resolved for
/// the grant's capabilities, still refuses an AF_VSOCK socket (D234). Guard: AF_VSOCK's
/// refusal.
#[test]
fn a_granted_workload_still_reaches_no_host_port_over_vsock() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-g");
    home.nesting_image();
    let ports = [
        shards_abi::run::PORT,
        shards_abi::run::SIGNAL_PORT,
        shards_abi::run::ENGINE_PORT,
        2000,
    ]
    .map(|p| p.to_string());
    let mut args = vec!["vsock"];
    args.extend(ports.iter().map(String::as_str));
    let ran = home.granted(&args);
    assert_eq!(ran.status, Some(0), "{}", shown(&ran));
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines.len(), ports.len(), "{}", shown(&ran));
    for (line, port) in lines.iter().zip(&ports) {
        assert!(
            line.starts_with(&format!("{port} socket Operation not permitted")),
            "port {port}: {}",
            shown(&ran)
        );
    }
}

/// Case (f), D251 and D252 for a nested runtime (Hex's step 39 acceptance): a nested workload's
/// own sockets, reached by its orchestrator's controller.
/// - **The run.** The image's command listens as Hex's relay does: four sockets in
///   `/run/hex/channel`, then READY=1. shards-api's image boot asks for the run's endpoint, of
///   the orchestrator's key, with the socket grant.
/// - **The lanes.** The controller dials the endpoint where the run's `control()` says and opens
///   a lane to each socket at once; on each it reads back, after its half-close, the socket's
///   name and its own bytes reversed, exactly.
/// - **The refusals.** A lane to a name nothing listens on is Unreached(ENOENT); a name out of
///   the directory is refused Protocol.
/// - **The end.** The run's stop ends the controller's connection, typed, never Silent.
///
/// Guards: the grant's directory, the endpoint's key and pin, each half-close, and the relay's
/// exact bytes.
#[test]
fn a_nested_workloads_sockets_are_lanes_of_its_orchestrators_controller() {
    use std::io::{Read as _, Write as _};
    use std::num::NonZeroU16;

    use shards_api::control::{Controller, Ended, Fault, Identity, Reason, SocketAdmission, Subscriptions};
    use shards_api::image::{ControlAsk, ImageShards, ImageSpec, Images as _, Narrowing, Ready, SocketGrant};
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-f-sockets");
    home.nesting_image();
    let hex = Identity::generate().unwrap();
    // The run's admission, as the orchestrator gives it (R03: its numbers).
    let (lanes, window) = (8u32, 64 * 1024u32);
    let names = ["control.sock", "stdio.sock", "files.sock", "stamps.sock"];
    let mut command: Vec<String> = ["/bin/testguest", "nesting", "listen", "/run/hex/channel"]
        .map(String::from)
        .to_vec();
    command.extend(names.map(String::from));
    let spec = ImageSpec {
        image: "nesting:1".into(),
        vcpus: 1,
        memory_mib: 512,
        command: Some(command),
        members: Vec::new(),
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        lessor: None,
        shares: Vec::new(),
        binary: Some(common::shards().to_path_buf()),
        env: host_env(&home),
        nesting: true,
        ready_by: None,
        control: Some(ControlAsk {
            controller: hex.fingerprint(),
            sockets: Some(SocketGrant {
                dir: "/run/hex/channel".into(),
                lanes,
                window,
            }),
        }),
        gpu_devices: Vec::new(),
        sign_ins: Vec::new(),
    };
    let guest = ImageShards::new().boot(&spec, TIMEOUT).unwrap();
    // READY=1 comes once every socket listens: no lane below races its bind.
    assert_eq!(guest.wait_ready(None, TIMEOUT).unwrap(), Some(Ready::OnTime));
    let at = *guest.control().expect("the run's agent-control endpoint");
    assert!(at.address.ip().is_loopback(), "{:?}", at.address);
    let (controller, dialed) = Controller::dial(
        &hex,
        at.vm,
        at.address,
        Subscriptions {
            outputs: &[],
            inputs: &[],
            execs: None,
            sockets: Some(SocketAdmission { lanes, window }),
        },
    )
    .unwrap();
    let driver = std::thread::spawn(move || controller.run());
    let sockets = dialed.sockets.expect("socket lanes subscribed");
    // Four lanes open at once, as Hex holds a control connection while an exec's three open.
    let open: Vec<_> = names
        .iter()
        .map(|name| (*name, sockets.connect(name).unwrap()))
        .collect();
    for (i, (name, mut lane)) in open.into_iter().enumerate() {
        let sent: Vec<u8> = (0..=255u8)
            .cycle()
            .skip(i)
            .take(3 * window as usize + 1)
            .collect();
        lane.write_all(&sent).unwrap();
        lane.shutdown_write().unwrap();
        let mut back = Vec::new();
        lane.read_to_end(&mut back).unwrap();
        let mut expected = format!("{name}\n").into_bytes();
        expected.extend(sent.iter().rev());
        assert_eq!(back.len(), expected.len(), "{name}");
        assert!(back == expected, "{name}: bytes differ");
    }
    // The guest's errno: Linux's ENOENT (include/uapi/asm-generic/errno-base.h), whatever the
    // host's.
    assert_eq!(
        sockets.connect("absent.sock").map(|_| ()),
        Err(Fault::Unreached(NonZeroU16::new(2).unwrap()))
    );
    assert_eq!(
        sockets.connect("../node.sock").map(|_| ()),
        Err(Fault::Refused(Reason::Protocol))
    );
    guest.stop().unwrap();
    // The run's end ends its endpoint, and the controller's connection with it, typed: never
    // Silent, which would be its idle timeout's guess rather than the run's end told.
    let after = sockets.connect("control.sock").map(|_| ());
    assert!(
        matches!(after, Err(Fault::Lost(ended)) if ended != Ended::Silent),
        "{after:?}"
    );
    let _ = driver.join().unwrap();
}

/// D251's cut through a real microVM's legs (*Its cut through the guest's legs*): runs
/// testguest's `nesting ends` with its socket directory granted to an orchestrator's key, dials
/// the run's endpoint, and gives `case` the run's lanes and the bytes the workload reads to its
/// cut (testguest's `cut.sock`), more than three of the admitted windows.
fn over_lanes(home: &str, case: impl FnOnce(&shards_api::control::Sockets, &[u8])) {
    use shards_api::control::{Controller, Identity, SocketAdmission, Subscriptions};
    use shards_api::image::{ControlAsk, ImageShards, ImageSpec, Images as _, Narrowing, Ready, SocketGrant};
    let home = Home::new(home);
    home.nesting_image();
    let hex = Identity::generate().unwrap();
    // The orchestrator's admission (R03: its numbers): a lane to an end and one to its `.said`
    // open at once, and the window of case (f).
    let (lanes, window) = (2u32, 64 * 1024u32);
    let sent: Vec<u8> = (0..=255u8).cycle().take(3 * window as usize + 1).collect();
    let command = ["/bin/testguest", "nesting", "ends", "/run/hex/channel"]
        .into_iter()
        .map(String::from)
        .chain([sent.len().to_string()])
        .collect();
    let spec = ImageSpec {
        image: "nesting:1".into(),
        vcpus: 1,
        memory_mib: 512,
        command: Some(command),
        members: Vec::new(),
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        lessor: None,
        shares: Vec::new(),
        binary: Some(common::shards().to_path_buf()),
        env: host_env(&home),
        nesting: true,
        ready_by: None,
        control: Some(ControlAsk {
            controller: hex.fingerprint(),
            sockets: Some(SocketGrant {
                dir: "/run/hex/channel".into(),
                lanes,
                window,
            }),
        }),
        gpu_devices: Vec::new(),
    };
    let guest = ImageShards::new().boot(&spec, TIMEOUT).unwrap();
    // READY=1 comes once every socket listens: no lane below races its bind.
    assert_eq!(guest.wait_ready(None, TIMEOUT).unwrap(), Some(Ready::OnTime));
    let Some(&at) = guest.control() else {
        panic!("no agent-control endpoint for the run")
    };
    let (controller, dialed) = Controller::dial(
        &hex,
        at.vm,
        at.address,
        Subscriptions {
            outputs: &[],
            inputs: &[],
            execs: None,
            sockets: Some(SocketAdmission { lanes, window }),
        },
    )
    .unwrap();
    let driver = std::thread::spawn(move || controller.run());
    let Some(sockets) = &dialed.sockets else {
        panic!("no socket lanes for the subscription")
    };
    case(sockets, &sent);
    guest.stop().unwrap();
    let _ = driver.join().unwrap();
}

/// What reached the workload over the end `said` tells of (testguest's `NAME.said`): the
/// controller's half-close first, so that the workload's close after it ends the lane in order.
fn told(sockets: &shards_api::control::Sockets, said: &str) -> Vec<u8> {
    use std::io::Read as _;
    let mut lane = sockets.connect(said).unwrap();
    lane.shutdown_write().unwrap();
    let mut got = Vec::new();
    lane.read_to_end(&mut got).unwrap();
    got
}

/// D251's cut, mid-stream, through init's relay, the VM process's vsock muxer and the endpoint
/// (Windows: `warm.rs`'s `pass` too): the workload reads all its controller sends, answers, and
/// closes its socket while the controller's way is still open. The controller reads
/// `ConnectionReset`, never `Ok(0)`, after no byte but a prefix of the answer (a cut promises
/// none before it), and its write after the cut is refused the same. Guards: init's hang-up probe
/// (`ClientCut`), the muxer's `Cut`, and the endpoint's.
#[test]
fn a_nested_workloads_socket_cut_mid_stream_resets_its_lane_at_the_controller() {
    use std::io::{ErrorKind, Read as _, Write as _};
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    over_lanes("nesting-f-cut", |sockets, sent| {
        let mut lane = sockets.connect("cut.sock").unwrap();
        // All the workload reads, and no half-close: the controller's way stays open.
        lane.write_all(sent).unwrap();
        let mut rest = Vec::new();
        assert_eq!(
            lane.read_to_end(&mut rest).map_err(|e| e.kind()),
            Err(ErrorKind::ConnectionReset),
            "a socket cut short read as its end in order"
        );
        let mut answer = b"cut.sock\n".to_vec();
        answer.extend(sent.iter().rev());
        assert!(
            answer.starts_with(&rest),
            "{} bytes read before the cut are no prefix of the workload's answer",
            rest.len()
        );
        assert_eq!(
            lane.write_all(b"after").map_err(|e| e.kind()),
            Err(ErrorKind::ConnectionReset),
            "a write after the cut"
        );
        let read = told(sockets, "cut.said");
        assert!(
            read == sent,
            "the workload read {} bytes before its close, not the {} sent: {:?}",
            read.len(),
            sent.len(),
            String::from_utf8_lossy(read.get(..64).unwrap_or(&read))
        );
    });
}

/// The cut's twin: the workload half-closes its way (SHUT_WR) and keeps reading. The controller
/// reads its bytes, then `Ok(0)` in order, never a reset, and every byte it writes after reaches
/// the workload, to its own half-close. Guards: the same legs' half-close, kept beside the cut.
#[test]
fn a_nested_workloads_half_close_ends_its_way_in_order_while_the_controllers_goes_on() {
    use std::io::{Read as _, Write as _};
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    over_lanes("nesting-f-half", |sockets, sent| {
        let mut lane = sockets.connect("half.sock").unwrap();
        let mut said = Vec::new();
        assert_eq!(
            lane.read_to_end(&mut said).map_err(|e| e.kind()),
            Ok(b"half.sock\n".len()),
            "a half-close read as a cut"
        );
        assert_eq!(said, b"half.sock\n");
        lane.write_all(sent).unwrap();
        lane.shutdown_write().unwrap();
        let read = told(sockets, "half.said");
        assert!(
            read == sent,
            "the workload read {} bytes after its half-close, not the {} sent: {:?}",
            read.len(),
            sent.len(),
            String::from_utf8_lossy(read.get(..64).unwrap_or(&read))
        );
    });
}

/// Case (g), D252 (Hex's step 39 acceptance): an orchestrator's run is bound an agent-control
/// endpoint of its own, its key the run's own and its controller the orchestrator's key alone.
/// - **The orchestrator dials it**, on loopback, its link TLS13_AES_256_GCM_SHA384 at both
///   ends (D239).
/// - **A stranger is refused**, NotEnrolled at its handshake.
/// - **Each run has its own key**: a second run's endpoint has another fingerprint.
/// - **It ends with its run**: the run's stop ends its connections, typed, never Silent.
/// - **No ask, no endpoint.**
///
/// Its runs grant a socket directory: an endpoint serving nothing at all is refused, as
/// shards-remote's own endpoints are (D252's note).
#[test]
fn an_orchestrators_run_binds_its_own_agent_control_endpoint() {
    use shards_api::control::{Controller, Ended, Fault, Identity, SocketAdmission, Subscriptions};
    use shards_api::image::{ControlAsk, ImageShards, ImageSpec, Images as _, Narrowing, SocketGrant};
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let home = Home::new("nesting-g-control");
    home.nesting_image();
    let (hex, stranger) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let admission = SocketAdmission {
        lanes: 1,
        window: 4096,
    };
    let held = |control: Option<ControlAsk>| {
        let spec = ImageSpec {
            image: "nesting:1".into(),
            vcpus: 1,
            memory_mib: 512,
            command: Some(["/bin/testguest", "nesting", "hold"].map(String::from).to_vec()),
            members: Vec::new(),
            publish: Vec::new(),
            egress: Narrowing::default(),
            credentials: Vec::new(),
            lessor: None,
            shares: Vec::new(),
            binary: Some(common::shards().to_path_buf()),
            env: host_env(&home),
            nesting: true,
            ready_by: None,
            control,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
        };
        ImageShards::new().boot(&spec, TIMEOUT).unwrap()
    };
    let ask = || {
        Some(ControlAsk {
            controller: hex.fingerprint(),
            sockets: Some(SocketGrant {
                dir: "/run/hex/channel".into(),
                lanes: admission.lanes,
                window: admission.window,
            }),
        })
    };
    let subscriptions = || Subscriptions {
        outputs: &[],
        inputs: &[],
        execs: None,
        sockets: Some(admission),
    };

    let first = held(ask());
    let at = *first.control().expect("the run's endpoint");
    assert!(at.address.ip().is_loopback(), "{:?}", at.address);
    let (controller, mut dialed) = Controller::dial(&hex, at.vm, at.address, subscriptions()).unwrap();
    let driver = std::thread::spawn(move || controller.run());
    let link = dialed.link.wait().unwrap();
    for end in [link.controller, link.endpoint] {
        assert_eq!(end.suite_name(), Some("TLS13_AES_256_GCM_SHA384"), "{end:?}");
    }

    let (refused, mut gone) = Controller::dial(&stranger, at.vm, at.address, subscriptions()).unwrap();
    let refused = std::thread::spawn(move || refused.run());
    assert!(
        matches!(gone.link.wait(), Err(Fault::Lost(Ended::NotEnrolled(_)))),
        "a stranger was let in"
    );
    let _ = refused.join().unwrap();

    let second = held(ask());
    let other = *second.control().expect("the second run's endpoint");
    assert_ne!(other.vm, at.vm, "two runs share one key");
    drop(second);

    first.stop().unwrap();
    let sockets = dialed.sockets.take().expect("socket lanes subscribed");
    let after = sockets.connect("control.sock").map(|_| ());
    assert!(
        matches!(after, Err(Fault::Lost(ended)) if ended != Ended::Silent),
        "{after:?}"
    );
    let _ = driver.join().unwrap();

    let bare = held(None);
    assert!(
        bare.control().is_none(),
        "a run asked for no endpoint was bound one"
    );
}
