//! A VM process's launch with its network process: the one the daemon's runs and warm VMs
//! (daemon.rs `start`), an orchestrator's run (api_run.rs) and a build's builder
//! (build/builder.rs) start their VMs through (R12). The network process is made first, as
//! the VM is handed its side of their ring (D31). The VM is handed its ends at the
//! descriptors (Unix) or by the names (Windows) its kind of VM takes them at, the words
//! naming them put after its command. On macOS it runs in App Sandbox from its launch and
//! asks for what it opens (D30); on Windows it runs in an AppContainer of its own, made
//! suspended so that its network process is given its process to watch (D141, D142).
//! Whatever was started is ended where a later step fails.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::{AsFd as _, BorrowedFd, RawFd};

use crate::daemon::ends::Ends;
use crate::netproc::{NetProcess, VmSide};

/// What a run's VM is told its link to its spawner is (`--warm`, `--resume`): the
/// descriptor it is handed it at on Unix, the name it is handed it by on Windows.
pub const LINK: &str = if cfg!(windows) { "daemon" } else { "3" };

/// A VM's launch: its command and binary, its network, and what it is made under.
#[derive(Debug)]
pub struct Plan<'a> {
    /// The VM process's command line, its command (`run`, `restore`) first: the launch's
    /// words go right after it, before anything of the caller's and so before any `--`.
    pub args: &'a [&'a OsStr],
    /// Its binary, unless its command asks for a GPU (`--gpu`): the GPU's build then (D144).
    pub vm: &'a Path,
    /// Its network, where it has one.
    pub network: Option<Network<'a>>,
    /// The home's record of this host's ends, by which a network process whose VM does not
    /// start is followed to its end (`netproc::reap`, D217).
    pub ends: &'a Ends,
    /// The spawner it and its network process are made by, a group's (D183): in the
    /// group's scope from their first instruction.
    #[cfg(unix)]
    pub group: Option<shards_ipc::Group>,
    /// The job they are made in, their own Jobs nested under it: a group's (D183, slice 3),
    /// or a direct boot's, which ends them as its process ends (D141).
    #[cfg(windows)]
    pub group: Option<&'a shards_ipc::GroupJob>,
    /// Who answers what the VM asks to reach.
    #[cfg(target_os = "macos")]
    pub grants: Grants,
}

/// A VM's network (D31): what its network process allows, the guest's MAC on `bridge`, and
/// whether its run's credential grants are made there (D242).
#[derive(Debug)]
pub struct Network<'a> {
    pub policy: shards_net::Policy,
    pub mac: [u8; 6],
    pub bridge: &'a shards_net::bridge::Bridge,
    pub credentials: bool,
}

/// Who answers what a VM asks to reach on macOS, where it asks its spawner for each file its
/// command names (D30).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grants {
    /// The launch, on a thread of its own that ends with the VM.
    Answered,
    /// Its caller, handed this process's end ([`Launched::grants`]): the daemon's watch,
    /// which answers a pool's VM with its template's directory and judges its start by what
    /// it asks (PM M166).
    Kept,
}

/// A VM process started, and its network process.
#[derive(Debug)]
pub struct Launched {
    pub vm: shards_ipc::Child,
    /// Its network process, and this process's control of it, on which its run's published
    /// ports and policy go.
    pub net: Option<(NetProcess, crate::netproc::Control)>,
    /// This process's end of what it asks to reach, where its plan keeps it
    /// ([`Grants::Kept`]).
    #[cfg(target_os = "macos")]
    pub grants: Option<std::os::unix::net::UnixStream>,
}

impl Launched {
    /// Ends its VM process, then its network process, which goes as its VM does, followed to
    /// its end by `ends` ([`crate::netproc::reap`]).
    pub(crate) fn end(self, ends: &Ends) {
        let _ = self.vm.kill(shards_ipc::signal::SIGKILL);
        let _ = self.vm.wait();
        if let Some((process, control)) = self.net {
            drop(control);
            crate::netproc::reap(&process, ends);
        }
    }
}

/// The kind of VM a launch makes, and what it is handed besides its stdio, its grants and
/// its network as one.
#[derive(Debug)]
enum Kind {
    /// A run's VM, the daemon's or an orchestrator's: its end of its link to its spawner
    /// ([`LINK`]); its network's release, where it has a network (`--net-release`, on which
    /// its run's published ports close as it ends); and on Windows the null device, for the
    /// stdio of the clients it lets go of, as its AppContainer opens no file by name (PM
    /// M351).
    #[cfg(unix)]
    Run { link: shards_ipc::Stream },
    #[cfg(windows)]
    Run {
        link: std::os::windows::io::OwnedHandle,
        null: std::fs::File,
    },
    /// A builder's VM: its join disk's socket (`--attach`, D134) and the pipe it tells how its
    /// console's report came out on (`--said-to`, D12). No release: a build publishes no port.
    Builder {
        attach: shards_vmm::platform::UnixStream,
        said_to: std::io::PipeWriter,
    },
}

impl Kind {
    /// Its name, as its launch's failures say it.
    fn who(&self) -> &'static str {
        match self {
            Kind::Run { .. } => "the VM",
            Kind::Builder { .. } => "the builder",
        }
    }

    /// The name of the thread that answers what it asks to reach.
    #[cfg(any(target_os = "macos", windows))]
    fn answerer(&self) -> &'static str {
        match self {
            Kind::Run { .. } => "vm grants",
            Kind::Builder { .. } => "builder grants",
        }
    }
}

/// Launches a run's VM, the daemon's or an orchestrator's: it, and this process's end of its
/// link ([`LINK`]).
pub fn run(plan: Plan<'_>) -> Result<(Launched, shards_ipc::Stream), String> {
    #[cfg(windows)]
    let record = plan.ends;
    let (ours, theirs) = shards_ipc::Stream::pair().map_err(|e| format!("the VM's link: {e}"))?;
    #[cfg(unix)]
    let kind = Kind::Run { link: theirs };
    // Its end shared, this process's copy of it let go of, so that the VM's end is the end
    // this process reads.
    #[cfg(windows)]
    let kind = {
        let link = theirs.share().map_err(|e| format!("the VM's link: {e}"))?;
        drop(theirs);
        let null = std::fs::File::open(crate::os::NULL).map_err(|e| format!("{}: {e}", crate::os::NULL))?;
        Kind::Run { link, null }
    };
    let launched = launch(plan, kind)?;
    // What this process sends on its link goes to the VM: handles duplicated into it.
    #[cfg(windows)]
    {
        match launched.vm.giver() {
            Ok(giver) => ours.gives_to(giver.into_handle()),
            Err(e) => {
                launched.end(record);
                return Err(format!("the VM's link: {e}"));
            }
        }
    }
    Ok((launched, ours))
}

/// Launches a build's builder VM, with its join disk's socket `attach` and its console
/// report's pipe `said_to`.
pub fn builder(
    plan: Plan<'_>,
    attach: shards_vmm::platform::UnixStream,
    said_to: std::io::PipeWriter,
) -> Result<Launched, String> {
    launch(plan, Kind::Builder { attach, said_to })
}

/// What a VM process is given of this process's environment: what it reads alone
/// (`netproc::child_env`).
type Env = Vec<(&'static str, OsString)>;

/// The VM process's binary for its command `args`, its environment, and whether it runs a
/// GPU: the GPU's build where its command asks for one (D144), which on Unix says MoltenVK's
/// errors alone (`MVK_CONFIG_LOG_LEVEL` 1, MoltenVK's errors-only level); else `vm`.
fn program(args: &[&OsStr], vm: &Path) -> Result<(PathBuf, Env, bool), String> {
    #[cfg_attr(windows, allow(unused_mut))]
    let mut env = crate::netproc::child_env();
    if !args.iter().any(|a| *a == "--gpu") {
        return Ok((vm.to_path_buf(), env, false));
    }
    #[cfg(unix)]
    env.push(("MVK_CONFIG_LOG_LEVEL", "1".into()));
    Ok((crate::helpers::vm_gpu()?, env, true))
}

/// `args` with `words` right after its command, where Windows' `--grants` goes
/// (`shards_ipc::Spawn::grants_at`): before anything of its caller's, and so before any `--`.
fn after_command<'w>(args: &[&'w OsStr], words: &'w [OsString]) -> Vec<&'w OsStr> {
    args.iter()
        .take(1)
        .copied()
        .chain(words.iter().map(OsString::as_os_str))
        .chain(args.iter().skip(1).copied())
        .collect()
}

/// What a VM is handed (Unix), each at the descriptor its kind takes it at, and the words of
/// its command that name them, as the daemon, the run host and the builder each handed them
/// before their launches were one. After its stdio (the null device, and this process's
/// error as its output and error), a run's VM has its link at 3 ([`LINK`]), macOS's grants at
/// 4, its network's ring at [`crate::netproc::VM_FDS`] and its release at
/// [`crate::netproc::VM_RELEASE_FD`]; a builder's has macOS's grants at 3, its join disk's
/// socket at 4, the ring at `VM_FDS` and its console report's pipe at 9.
#[cfg(unix)]
fn handed<'f>(
    kind: &'f Kind,
    null: BorrowedFd<'f>,
    err: BorrowedFd<'f>,
    grants: Option<BorrowedFd<'f>>,
    net: Option<(&'f VmSide, &[u8; 6])>,
) -> (Vec<(BorrowedFd<'f>, RawFd)>, Vec<OsString>) {
    let number = |at: RawFd| OsString::from(at.to_string());
    let ring = |side: &'f VmSide| {
        let [r, s, w] = crate::netproc::VM_FDS;
        [
            (side.region.as_fd(), r),
            (side.sleeps.as_fd(), s),
            (side.rings.as_fd(), w),
        ]
    };
    let mut fds = vec![(null, 0), (err, 1), (err, 2)];
    let mut words = Vec::new();
    match kind {
        Kind::Run { link } => {
            let (link_at, grants_at, release_at) = (3, 4, crate::netproc::VM_RELEASE_FD);
            fds.push((link.as_fd(), link_at));
            if let Some(grants) = grants {
                fds.push((grants, grants_at));
                words.extend(["--grants".into(), number(grants_at)]);
            }
            if let Some((side, mac)) = net {
                fds.extend(ring(side));
                fds.push((side.release.as_fd(), release_at));
                words.extend([
                    "--net".into(),
                    VmSide::arg(mac).into(),
                    "--net-release".into(),
                    number(release_at),
                ]);
            }
        }
        Kind::Builder { attach, said_to } => {
            let (grants_at, attach_at, said_to_at) = (3, 4, 9);
            if let Some(grants) = grants {
                fds.push((grants, grants_at));
                words.extend(["--grants".into(), number(grants_at)]);
            }
            fds.push((attach.as_fd(), attach_at));
            if let Some((side, mac)) = net {
                fds.extend(ring(side));
                words.extend(["--net".into(), VmSide::arg(mac).into()]);
            }
            fds.push((said_to.as_fd(), said_to_at));
            words.extend([
                "--said-to".into(),
                number(said_to_at),
                "--attach".into(),
                number(attach_at),
            ]);
        }
    }
    (fds, words)
}

#[cfg(unix)]
fn launch(plan: Plan<'_>, kind: Kind) -> Result<Launched, String> {
    launch_with(
        plan,
        kind,
        |n, group| crate::netproc::start(n.policy, &n.mac, n.bridge, group, n.credentials),
        |group, program, args, fds, env| shards_ipc::spawn_in_group(group, program, args, fds, false, env),
    )
}

/// [`launch`], its network process started by `network` and its VM process by `spawn`.
#[cfg(unix)]
fn launch_with(
    plan: Plan<'_>,
    kind: Kind,
    network: impl FnOnce(Network<'_>, Option<shards_ipc::Group>) -> Result<(NetProcess, VmSide), String>,
    spawn: impl FnOnce(
        Option<shards_ipc::Group>,
        &Path,
        &[&OsStr],
        &[(BorrowedFd<'_>, RawFd)],
        &[(&str, &OsStr)],
    ) -> std::io::Result<shards_ipc::Child>,
) -> Result<Launched, String> {
    let Plan {
        args,
        vm,
        network: net,
        ends: record,
        group,
        #[cfg(target_os = "macos")]
            grants: answered,
    } = plan;
    let who = kind.who();
    #[cfg(target_os = "macos")]
    let answerer = kind.answerer();
    let (binary, env, _) = program(args, vm)?;
    #[cfg(target_os = "macos")]
    let (grants, granted) =
        std::os::unix::net::UnixStream::pair().map_err(|e| format!("{who}'s grants: {e}"))?;
    #[cfg(target_os = "macos")]
    let granted = Some(granted);
    #[cfg(not(target_os = "macos"))]
    let granted: Option<shards_ipc::Stream> = None;
    let null = std::fs::File::open(crate::os::NULL).map_err(|e| format!("{}: {e}", crate::os::NULL))?;
    let err = std::io::stderr();
    let started = match net {
        Some(n) => {
            let mac = n.mac;
            let (process, side) = network(n, group)?;
            Some((process, side, mac))
        }
        None => None,
    };
    let (fds, words) = handed(
        &kind,
        null.as_fd(),
        err.as_fd(),
        granted.as_ref().map(|g| g.as_fd()),
        started.as_ref().map(|(_, side, mac)| (side, mac)),
    );
    let argv = after_command(args, &words);
    let spawned = spawn(group, &binary, &argv, &fds, &crate::netproc::env_pairs(&env));
    drop(fds);
    let vm = match spawned {
        Ok(vm) => vm,
        Err(e) => {
            // Its side let go of here, its network process goes as its doorbell hangs up.
            if let Some((process, side, _)) = started {
                drop(side);
                crate::netproc::reap(&process, record);
            }
            return Err(format!("starting {who} ({}): {e}", binary.display()));
        }
    };
    // The VM holds its ends now, its side of the ring among them: as it goes, its network
    // process hears its doorbell hang up and goes too.
    drop((kind, null, granted));
    let net = started.map(|(process, side, _)| (process, side.control));
    #[cfg(target_os = "macos")]
    let grants = match answered {
        Grants::Kept => Some(grants),
        Grants::Answered => {
            let answering = std::thread::Builder::new().name(answerer.into()).spawn(move || {
                let _ = crate::grant_answer::serve(&grants, None);
            });
            if let Err(e) = answering {
                Launched {
                    vm,
                    net,
                    grants: None,
                }
                .end(record);
                return Err(format!("answering {who}: {e}"));
            }
            None
        }
    };
    Ok(Launched {
        vm,
        net,
        #[cfg(target_os = "macos")]
        grants,
    })
}

/// What a VM is handed (Windows), by the names its kind takes them by: the handles given it
/// and the sockets shared with it, after the pipes it asks for its files on (`grants`), and
/// the words of its command that name them, as the daemon, the run host and the builder each
/// handed them before their launches were one. A run's VM has its link ([`LINK`]), the null
/// device, its network's ring and doorbells and its release; a builder's has the ring and
/// doorbells, its console report's pipe and its join disk's socket.
#[cfg(windows)]
fn handed<'f>(
    kind: &'f Kind,
    grants: [shards_ipc::Given<'f>; 2],
    net: Option<(&'f VmSide, &[u8; 6])>,
) -> (
    Vec<shards_ipc::Given<'f>>,
    Vec<shards_ipc::Shared<'f>>,
    Vec<OsString>,
) {
    use std::os::windows::io::AsHandle as _;
    let mut given = Vec::from(grants);
    let mut sockets = Vec::new();
    let mut words: Vec<OsString> = Vec::new();
    match kind {
        Kind::Run { link, null } => {
            given.extend([
                shards_ipc::Given {
                    name: LINK,
                    handle: link.as_handle(),
                },
                shards_ipc::Given {
                    name: "null",
                    handle: null.as_handle(),
                },
            ]);
            if let Some((side, mac)) = net {
                given.extend(side.given());
                sockets.extend([side.shared(), side.release()]);
                words.extend(["--net".into(), VmSide::arg(mac).into()]);
            }
        }
        Kind::Builder { attach, said_to } => {
            let (attach_as, said_to_as) = ("attach", "said-to");
            sockets.push(shards_ipc::Shared {
                name: attach_as,
                socket: attach.raw() as u64,
            });
            if let Some((side, mac)) = net {
                given.extend(side.given());
                sockets.push(side.shared());
                words.extend(["--net".into(), VmSide::arg(mac).into()]);
            }
            given.push(shards_ipc::Given {
                name: said_to_as,
                handle: said_to.as_handle(),
            });
            words.extend([
                "--said-to".into(),
                said_to_as.into(),
                "--attach".into(),
                attach_as.into(),
            ]);
        }
    }
    (given, sockets, words)
}

/// [`launch`] on Windows (D141, D142, D195): the VM confined in an AppContainer of its own,
/// made suspended, given by name what its kind is given; its network process started next,
/// given the VM's process to watch; then the VM runs, and this process answers what it asks
/// for on a thread of its own, which ends as the VM does. Each process started is a confined
/// one, ended as its value drops (its Job's last handle closing), so a step that fails ends
/// what was started as it returns.
#[cfg(windows)]
fn launch(plan: Plan<'_>, kind: Kind) -> Result<Launched, String> {
    use std::os::windows::io::{AsHandle as _, OwnedHandle};
    let Plan {
        args,
        vm,
        network,
        ends: record,
        group,
    } = plan;
    let (who, answerer) = (kind.who(), kind.answerer());
    let (binary, env, gpu) = program(args, vm)?;
    let network = match network {
        Some(n) => {
            let (ready, side) = crate::netproc::prepare(n.policy, &n.mac, n.bridge, n.credentials)?;
            Some((ready, side, n.mac))
        }
        None => None,
    };
    let container = crate::helpers::containers().and_then(|records| {
        shards_ipc::AppContainer::new("vm", &records).map_err(|e| format!("{who}'s AppContainer: {e}"))
    })?;
    // A pipe each way: its requests, and the answers.
    let pipes = std::io::pipe().and_then(|asks| Ok((asks, std::io::pipe()?)));
    let ((asks_from, asks_to), (answers_from, answers_to)) =
        pipes.map_err(|e| format!("{who}'s grants: {e}"))?;
    let (asks_to, answers_from) = (OwnedHandle::from(asks_to), OwnedHandle::from(answers_from));
    let (given, sockets, words) = handed(
        &kind,
        [
            shards_ipc::Given {
                name: "ask",
                handle: asks_to.as_handle(),
            },
            shards_ipc::Given {
                name: "answer",
                handle: answers_from.as_handle(),
            },
        ],
        network.as_ref().map(|(_, side, mac)| (side, mac)),
    );
    let argv = after_command(args, &words);
    let err = std::io::stderr();
    let suspended = shards_ipc::spawn_suspended(&shards_ipc::Spawn {
        program: &binary,
        args: &argv,
        // Before its words: `--net` takes the handles its grants name (vm_run.rs `net_host`).
        grants_at: 1,
        given: &given,
        stdin: None,
        stdout: err.as_handle(),
        stderr: err.as_handle(),
        sockets: &sockets,
        env: &crate::netproc::env_pairs(&env),
        container: Some(&container),
        holds: shards_ipc::Holds::Hypervisor,
        group,
        graphics: gpu,
    })
    .map_err(|e| format!("starting {who} ({}): {e}", binary.display()))?;
    // Shared with it already, and its handles duplicated into it.
    drop((given, sockets));
    // Before the VM runs: ended with the suspended VM, where it cannot start.
    let net = match network {
        Some((ready, side, _)) => {
            let process = crate::netproc::start(ready, &suspended, group)?;
            let control = crate::netproc::Control {
                socket: side.control,
                pid: process.id(),
            };
            Some((process, control))
        }
        None => None,
    };
    let giver = suspended.giver().map_err(|e| format!("answering {who}: {e}"))?;
    let mut vm = suspended
        .resume()
        .map_err(|e| format!("starting {who} ({}): {e}", binary.display()))?;
    // Its AppContainer's profile goes once it has, as its follower lets it go.
    vm.hold(container);
    // Its ends are its own now.
    drop((asks_to, answers_from, kind));
    let link = crate::grant::Link {
        from: std::fs::File::from(OwnedHandle::from(asks_from)),
        to: std::fs::File::from(OwnedHandle::from(answers_to)),
    };
    let launched = Launched { vm, net };
    let answering = std::thread::Builder::new().name(answerer.into()).spawn(move || {
        let _ = crate::grant_answer::serve(&link, &giver);
    });
    if let Err(e) = answering {
        launched.end(record);
        return Err(format!("answering {who}: {e}"));
    }
    Ok(launched)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];

    #[test]
    fn the_launchs_words_go_right_after_the_vms_command() {
        let words: Vec<OsString> = ["--net", "5,6,7,02:00:00:00:00:01"].map(OsString::from).into();
        let args = ["restore", "/t", "--resume", LINK, "--", "x"].map(OsStr::new);
        assert_eq!(
            after_command(&args, &words),
            [
                "restore",
                "--net",
                "5,6,7,02:00:00:00:00:01",
                "/t",
                "--resume",
                LINK,
                "--",
                "x"
            ]
            .map(OsStr::new)
        );
    }

    /// A VM's side of its network of descriptors alone: no ring, no network process.
    #[cfg(unix)]
    fn side_of_nothing() -> VmSide {
        let null = || std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let end = || std::os::unix::net::UnixStream::pair().unwrap().0;
        VmSide {
            region: null(),
            sleeps: null(),
            rings: null(),
            control: end(),
            release: end(),
        }
    }

    /// Each fd's number in this process, and where the VM is handed it.
    #[cfg(unix)]
    fn numbers(fds: &[(BorrowedFd<'_>, RawFd)]) -> Vec<(RawFd, RawFd)> {
        use std::os::fd::AsRawFd as _;
        fds.iter().map(|(fd, at)| (fd.as_raw_fd(), *at)).collect()
    }

    /// The daemon's and the run host's VMs are handed what daemon.rs `start` and api_run.rs
    /// `launch` handed them before the fold (main 1acbf2cb9): the link at 3, macOS's grants
    /// at 4 (`--grants 4`), the ring at 5, 6 and 7 (`--net 5,6,7,MAC`) and the release at 8
    /// (`--net-release 8`); Linux's has no grants, and a VM without a network no ring.
    #[cfg(unix)]
    #[test]
    fn a_runs_vm_is_handed_its_ends_where_the_daemon_and_the_run_host_handed_them() {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let (link, _ours) = shards_ipc::Stream::pair().unwrap();
        let link_fd = link.as_raw_fd();
        let null = std::fs::File::open("/dev/null").unwrap();
        let err = std::io::stderr();
        let (grants, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let side = side_of_nothing();
        let kind = Kind::Run { link };
        let stdio = [(null.as_raw_fd(), 0), (2, 1), (2, 2)];
        let ring = [
            (side.region.as_raw_fd(), 5),
            (side.sleeps.as_raw_fd(), 6),
            (side.rings.as_raw_fd(), 7),
            (side.release.as_raw_fd(), 8),
        ];
        let words = |w: &[&str]| w.iter().map(OsString::from).collect::<Vec<_>>();

        // macOS's: grants, and a network.
        let (fds, said) = handed(
            &kind,
            null.as_fd(),
            err.as_fd(),
            Some(grants.as_fd()),
            Some((&side, &MAC)),
        );
        let mut want = stdio.to_vec();
        want.extend([(link_fd, 3), (grants.as_raw_fd(), 4)]);
        want.extend(ring);
        assert_eq!(numbers(&fds), want);
        assert_eq!(
            said,
            words(&[
                "--grants",
                "4",
                "--net",
                "5,6,7,02:00:00:00:00:01",
                "--net-release",
                "8"
            ])
        );

        // Linux's: no grants.
        let (fds, said) = handed(&kind, null.as_fd(), err.as_fd(), None, Some((&side, &MAC)));
        let mut want = stdio.to_vec();
        want.push((link_fd, 3));
        want.extend(ring);
        assert_eq!(numbers(&fds), want);
        assert_eq!(
            said,
            words(&["--net", "5,6,7,02:00:00:00:00:01", "--net-release", "8"])
        );

        // No network (a guest without `shards_net=`, a run without a bridge).
        let (fds, said) = handed(&kind, null.as_fd(), err.as_fd(), Some(grants.as_fd()), None);
        let mut want = stdio.to_vec();
        want.extend([(link_fd, 3), (grants.as_raw_fd(), 4)]);
        assert_eq!(numbers(&fds), want);
        assert_eq!(said, words(&["--grants", "4"]));

        // The link is named as it is handed.
        assert_eq!(LINK, "3");
    }

    /// A builder's VM is handed what build/builder.rs `launch` and `Builder::start` handed it
    /// before the fold (main 1acbf2cb9): macOS's grants at 3 (`--grants 3`), its join disk's
    /// socket at 4 (`--attach 4`), the ring at 5, 6 and 7 (`--net 5,6,7,MAC`) and its console
    /// report's pipe at 9 (`--said-to 9`); no release; Linux's has no grants.
    #[cfg(unix)]
    #[test]
    fn a_builders_vm_is_handed_its_ends_where_the_builder_handed_them() {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let (attach, _ours) = shards_vmm::platform::UnixStream::pair().unwrap();
        let (_from_vm, said_to) = std::io::pipe().unwrap();
        let (attach_fd, said_to_fd) = (attach.as_raw_fd(), said_to.as_raw_fd());
        let null = std::fs::File::open("/dev/null").unwrap();
        let err = std::io::stderr();
        let (grants, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let side = side_of_nothing();
        let kind = Kind::Builder { attach, said_to };
        let words = |w: &[&str]| w.iter().map(OsString::from).collect::<Vec<_>>();
        let ring = [
            (side.region.as_raw_fd(), 5),
            (side.sleeps.as_raw_fd(), 6),
            (side.rings.as_raw_fd(), 7),
        ];

        let (fds, said) = handed(
            &kind,
            null.as_fd(),
            err.as_fd(),
            Some(grants.as_fd()),
            Some((&side, &MAC)),
        );
        let mut want = vec![
            (null.as_raw_fd(), 0),
            (2, 1),
            (2, 2),
            (grants.as_raw_fd(), 3),
            (attach_fd, 4),
        ];
        want.extend(ring);
        want.push((said_to_fd, 9));
        assert_eq!(numbers(&fds), want);
        assert_eq!(
            said,
            words(&[
                "--grants",
                "3",
                "--net",
                "5,6,7,02:00:00:00:00:01",
                "--said-to",
                "9",
                "--attach",
                "4"
            ])
        );

        let (fds, said) = handed(&kind, null.as_fd(), err.as_fd(), None, Some((&side, &MAC)));
        let mut want = vec![(null.as_raw_fd(), 0), (2, 1), (2, 2), (attach_fd, 4)];
        want.extend(ring);
        want.push((said_to_fd, 9));
        assert_eq!(numbers(&fds), want);
        assert_eq!(
            said,
            words(&[
                "--net",
                "5,6,7,02:00:00:00:00:01",
                "--said-to",
                "9",
                "--attach",
                "4"
            ])
        );
    }

    /// A VM process that does not start leaves nothing running: its network process, made
    /// first, is reaped. The network process here is a stand-in, `cat` on the doorbell the
    /// VM's side rings, which goes as that side is let go of, as shards-net goes as its ring's
    /// doorbell hangs up; and the VM's spawner refuses. Reaped, its pid is no process's, not
    /// even a zombie's.
    #[cfg(unix)]
    #[test]
    fn a_vm_that_does_not_start_leaves_its_network_process_reaped() {
        use std::os::fd::AsFd as _;
        let (doorbell, rings) = std::io::pipe().unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let stand_in = shards_ipc::spawn(
            Path::new("/bin/cat"),
            &[],
            &[(doorbell.as_fd(), 0), (null.as_fd(), 1), (null.as_fd(), 2)],
            false,
        )
        .unwrap();
        drop(doorbell);
        let pid = libc::pid_t::try_from(stand_in.id()).unwrap();
        let mut side = side_of_nothing();
        side.rings = rings.into();
        let bridge = shards_net::bridge::Bridge::elect(&[]).unwrap();
        let home = shards_testdir::TempDir::new("launch-reap").unwrap();
        let record = Ends::kept_in(&home);
        let (link, _ours) = shards_ipc::Stream::pair().unwrap();
        let args = ["run", "--warm", LINK].map(OsStr::new);
        let mut asked = None;
        let launched = launch_with(
            Plan {
                args: &args,
                vm: Path::new("/shards-vm"),
                network: Some(Network {
                    policy: shards_net::Policy::DenyAll,
                    mac: MAC,
                    bridge: &bridge,
                    credentials: false,
                }),
                ends: &record,
                group: None,
                #[cfg(target_os = "macos")]
                grants: Grants::Answered,
            },
            Kind::Run { link },
            |_, _| Ok((stand_in, side)),
            |_, program, argv, fds, _| {
                let argv: Vec<OsString> = argv.iter().map(|a| a.to_os_string()).collect();
                let at: Vec<RawFd> = fds.iter().map(|(_, at)| *at).collect();
                asked = Some((program.to_path_buf(), argv, at));
                Err(std::io::Error::other("refused"))
            },
        );
        let e = launched.unwrap_err();
        assert!(e.contains("starting the VM (/shards-vm): refused"), "{e}");
        let (program, argv, at) = asked.unwrap();
        assert_eq!(program, Path::new("/shards-vm"));
        assert_eq!(argv.first().map(OsString::as_os_str), Some(OsStr::new("run")));
        assert!(argv.iter().any(|a| a == "--net-release"), "{argv:?}");
        assert!(at.contains(&3) && at.contains(&8), "{at:?}");
        // SAFETY: kill(2) of signal 0 sends nothing; it asks whether `pid` is a process's.
        let gone = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        assert!(gone, "the network process {pid} is left");
    }

    /// Windows' VMs are handed by name what the daemon's, the run host's and the builder's
    /// Windows launches handed them before the fold (main 1acbf2cb9), each in that order.
    #[cfg(windows)]
    #[test]
    fn each_vm_is_handed_by_name_what_its_launch_handed_it() {
        use std::os::windows::io::{AsHandle as _, OwnedHandle};
        shards_vmm::platform::winsock().unwrap();
        let file = || OwnedHandle::from(std::fs::File::open(crate::os::NULL).unwrap());
        let socket = || shards_vmm::platform::UnixStream::pair().unwrap().0;
        let side = VmSide {
            region: file(),
            sleeps: file(),
            rings: socket(),
            control: socket(),
            release: socket(),
        };
        let (asks, answers) = (file(), file());
        let grants = || {
            [
                shards_ipc::Given {
                    name: "ask",
                    handle: asks.as_handle(),
                },
                shards_ipc::Given {
                    name: "answer",
                    handle: answers.as_handle(),
                },
            ]
        };
        fn names<'n>(
            given: &[shards_ipc::Given<'n>],
            sockets: &[shards_ipc::Shared<'n>],
        ) -> (Vec<&'n str>, Vec<&'n str>) {
            (
                given.iter().map(|g| g.name).collect(),
                sockets.iter().map(|s| s.name).collect(),
            )
        }
        let words = |w: &[&str]| w.iter().map(OsString::from).collect::<Vec<_>>();

        let run = Kind::Run {
            link: file(),
            null: std::fs::File::open(crate::os::NULL).unwrap(),
        };
        let (given, sockets, said) = handed(&run, grants(), Some((&side, &MAC)));
        assert_eq!(
            names(&given, &sockets),
            (
                vec!["ask", "answer", "daemon", "null", "net-region", "net-wake-me"],
                vec!["net-wake-peer", "net-release"]
            )
        );
        assert_eq!(said, words(&["--net", "02:00:00:00:00:01"]));
        let (given, sockets, said) = handed(&run, grants(), None);
        assert_eq!(
            names(&given, &sockets),
            (vec!["ask", "answer", "daemon", "null"], vec![])
        );
        assert!(said.is_empty());

        let (_from_vm, said_to) = std::io::pipe().unwrap();
        let builder = Kind::Builder {
            attach: socket(),
            said_to,
        };
        let (given, sockets, said) = handed(&builder, grants(), Some((&side, &MAC)));
        assert_eq!(
            names(&given, &sockets),
            (
                vec!["ask", "answer", "net-region", "net-wake-me", "said-to"],
                vec!["attach", "net-wake-peer"]
            )
        );
        assert_eq!(
            said,
            words(&[
                "--net",
                "02:00:00:00:00:01",
                "--said-to",
                "said-to",
                "--attach",
                "attach"
            ])
        );
    }
}
