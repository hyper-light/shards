//! `shards api-run`: an image's run for an orchestrator, hosted by this process (D231).
//! shards-api's image boot starts one for each microVM and speaks to it over its stdin and
//! stdout (`shards_api::image::wire`): a request in; the run's start, each member's output
//! and end, stats and the run's end out. It is to its run what the daemon is to each of its
//! runs, and nothing more: it prepares the run with the daemon's own functions (`run`,
//! `volumes`, `setup`, `agentfile`, `daemon::publish`, `netproc`), starts the run's network,
//! share and VM processes, plays the daemon's side of the VM's link (warm.rs) for that one
//! run, and owns its managed output (P3). Its children go with it: the VM is told at
//! `TAKEN` that its run is killed if its link closes (`STOP_ORDER`), the network process
//! goes as the VM's doorbell end does, the share process as its streams close; on Windows
//! each is made in a job whose last handle is this process's, which ends them as it ends,
//! however it ends. And the run goes with its owner: its stdin closing kills the VM.

use std::io::{self, Write as _};
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use shards_api::image::wire::{self, Cause};
use shards_ipc::kind;

/// Why the run will not be, for the orchestrator.
struct No(Cause, String);

fn no(cause: Cause) -> impl Fn(String) -> No {
    move |why| No(cause, why)
}

/// The answers' channel, this process's stdout: one frame at a time, whichever thread
/// sends it.
struct Answers(Mutex<io::Stdout>);

impl Answers {
    /// Frames `parts` as one payload of `kind`, written in one turn.
    fn send(&self, kind: u8, parts: &[&[u8]]) -> io::Result<()> {
        let len = parts
            .iter()
            .try_fold(0usize, |n, p| n.checked_add(p.len()))
            .ok_or(io::ErrorKind::InvalidInput)?;
        let header = wire::header(kind, len).map_err(io::Error::other)?;
        let mut out = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        out.write_all(&header)?;
        for p in parts {
            out.write_all(p)?;
        }
        out.flush()
    }
}

/// One line of the run's progress on this process's stderr, its diagnostics, which a
/// refusal quotes the last of.
fn say(what: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "shards api-run: {what}");
}

/// `shards api-run`: not for people to run. The run's end is said as its VM ends, before
/// its owner closes the channel; a run that never started is refused instead.
pub fn command() -> ExitCode {
    let answers = Answers(Mutex::new(io::stdout()));
    match host(&answers) {
        Ok(()) => ExitCode::SUCCESS,
        Err(No(cause, why)) => {
            let _ = writeln!(io::stderr(), "shards api-run: {why}");
            let refused = wire::Refused { cause, why };
            let _ = refused
                .encode()
                .map(|p| answers.send(wire::to_client::REFUSED, &[&p]));
            ExitCode::FAILURE
        }
    }
}

fn host(answers: &Answers) -> Result<(), No> {
    let mut input = io::stdin();
    let request = match wire::read_frame(&mut input) {
        Ok(Some((wire::to_host::CREATE, payload))) => {
            wire::Request::decode(&payload).map_err(|e| No(Cause::Request, e.to_string()))?
        }
        Ok(Some((kind, _))) => {
            return Err(No(
                Cause::Request,
                format!("message kind {kind} before the request"),
            ));
        }
        Ok(None) => return Err(No(Cause::Request, "the channel closed before its request".into())),
        Err(e) => return Err(No(Cause::Request, format!("the request: {e}"))),
    };
    let ready_by = deadline(request.ready_by, SystemTime::now())?;
    hosted::run(answers, &request, ready_by, input)
}

/// The nesting grant's deadline (D234) as an instant, made as the request reaches this
/// process: `budget` nanoseconds past `now`, this host's clock, in nanoseconds since the
/// Unix epoch, as `ready-by=` carries it to init; none where the request gives none.
/// Refused where the clock reads before the epoch or past what a u64 of nanoseconds counts
/// (the year 2554), whose instants none is; and where the instant is past that count.
fn deadline(budget: Option<u64>, now: SystemTime) -> Result<Option<u64>, No> {
    let Some(budget) = budget else {
        return Ok(None);
    };
    let clock = now
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_nanos()).ok())
        .ok_or_else(|| {
            No(
                Cause::Boot,
                format!(
                    "the deadline: this host's clock reads {now:?}, which no count of nanoseconds since the Unix epoch in a u64 is"
                ),
            )
        })?;
    clock.checked_add(budget).map(Some).ok_or_else(|| {
        No(
            Cause::Request,
            format!(
                "the deadline: {budget} ns past this host's clock, {clock} ns since the Unix epoch, is past what a u64 of nanoseconds counts"
            ),
        )
    })
}

/// What the run's command is given of the nesting grant (D234), decided as the daemon
/// decides it for a run of its own (`agentfile::nesting`), refused typed, in its words: the
/// grant asked of an image whose Agentfile declares no `NESTING` a widening of its grants;
/// the rest the request's, a deadline without the grant among them. (A crafted image,
/// declaring it beside an agent or harness, never comes here: `run::prepare`'s read of its
/// Agentfile refuses it first, in its build's words.) An image declaring it, run without it,
/// says so.
fn nesting(
    agentfile: Option<&crate::agentfile::Agentfile>,
    run: &shards_ipc::Run,
    say: &dyn Fn(&str),
) -> Result<crate::agentfile::Nesting, No> {
    use crate::agentfile::NestingRefused as Refused;
    let nesting = crate::agentfile::nesting(agentfile, run).map_err(|e| {
        let cause = match e {
            Refused::Undeclared => Cause::Widening,
            Refused::CgroupnsHost
            | Refused::PidHost
            | Refused::Joiner
            | Refused::Joined
            | Refused::Ungranted => Cause::Request,
        };
        No(cause, e.to_string())
    })?;
    if nesting == crate::agentfile::Nesting::Unasked {
        say(&format!("WARNING: {}", crate::agentfile::NESTING_UNASKED));
    }
    Ok(nesting)
}

/// What the orchestrator is told of a message on the VM's link, as an answer's kind and
/// payload: a member's end (D231), and its own command's readiness under the nesting grant
/// (D234), each as the guest said it; none for the rest. A readiness that is none is said
/// on this host's stderr and dropped, as the daemon drops it.
fn relayed(which: u8, payload: &[u8]) -> Option<(u8, Vec<u8>)> {
    match which {
        kind::MEMBER_ENDED => {
            let e = shards_abi::run::members::Ended::decode(payload)?;
            let ended = wire::MemberEnded {
                member: e.member,
                status: e.status,
            };
            Some((wire::to_client::MEMBER_ENDED, ended.encode()))
        }
        kind::WORKLOAD_READY => match wire::Ready::decode(payload) {
            Some(ready) => Some((wire::to_client::READY, ready.encode().to_vec())),
            None => {
                let _ = writeln!(
                    io::stderr(),
                    "shards api-run: a readiness its guest said malformed: {payload:?}"
                );
                None
            }
        },
        _ => None,
    }
}

/// Members a request names, by their declared index, from the image's managed plan's
/// labels (`agent NAME`, `harness NAME`; agentfile.rs `managed_plan`) and which of them run.
fn members_of(labels: &[String], active: &[u32]) -> Vec<wire::MemberInfo> {
    labels
        .iter()
        .enumerate()
        .filter_map(|(i, label)| {
            let index = u32::try_from(i).ok()?;
            let (kind, name) = label.split_once(' ')?;
            let kind = match kind {
                "agent" => wire::MemberKind::Agent,
                "harness" => wire::MemberKind::Harness,
                _ => return None,
            };
            Some(wire::MemberInfo {
                index,
                kind,
                name: name.to_string(),
                runs: active.contains(&index),
            })
        })
        .collect()
}

/// The setup entries staging what `request` gives each member (D231), each member one the
/// image declares and runs, each entry checked as the guest checks it.
fn staged(request: &wire::Request, members: &[wire::MemberInfo]) -> Result<Vec<Vec<u8>>, No> {
    use shards_abi::run::members as staging;
    let mut entries = Vec::new();
    for m in &request.members {
        let info = members
            .iter()
            .find(|i| i.kind == m.kind && i.name == m.name)
            .ok_or_else(|| {
                No(
                    Cause::Image,
                    format!("{:?} {}: the image declares no such member", m.kind, m.name),
                )
            })?;
        if !info.runs {
            return Err(No(
                Cause::Image,
                format!(
                    "{:?} {}: files alone, which run nothing to read what is staged",
                    m.kind, m.name
                ),
            ));
        }
        for e in &m.env {
            entries.push(
                staging::env(info.index, &e.0)
                    .map_err(|e| No(Cause::Request, format!("{}: a variable: {e}", m.name)))?,
            );
        }
        for f in &m.files {
            entries.push(
                staging::file(info.index, f.mode, f.path.as_bytes(), &f.bytes.0)
                    .map_err(|e| No(Cause::Request, format!("{}: {}: {e}", m.name, f.path)))?,
            );
        }
    }
    Ok(entries)
}

/// What of `granted` the orchestrator keeps: all of it, or each it names, each one
/// `granted` holds as written; any other is a widening, refused by name.
fn kept(what: &str, granted: &[Vec<u8>], keep: &wire::Keep) -> Result<Vec<Vec<u8>>, No> {
    match keep {
        wire::Keep::All => Ok(granted.to_vec()),
        wire::Keep::Only(items) => items
            .iter()
            .map(|item| {
                granted
                    .iter()
                    .find(|g| g.as_slice() == item.as_bytes())
                    .cloned()
                    .ok_or_else(|| {
                        No(
                            Cause::Widening,
                            format!("{what} {item}: the image grants no such"),
                        )
                    })
            })
            .collect(),
    }
}

/// The run's network process's policy: the image's grants past the microVM (D59), as the
/// daemon reads them, narrowed by the request and never widened (D231); none where nothing
/// is left.
fn egress(
    grants: Option<&shards_agentfile::agentfile::Grants>,
    narrowing: &wire::Narrowing,
) -> Result<Option<Vec<u8>>, No> {
    let none = shards_agentfile::agentfile::Grants::default();
    let g = grants.unwrap_or(&none);
    let ports = kept("egress", &g.egress, &narrowing.egress)?;
    let mcp = kept("remote MCP server", &g.mcp, &narrowing.mcp)?;
    let destinations = kept("destination", &g.destinations, &narrowing.destinations)?;
    let dns = g.dns && narrowing.dns;
    if ports.is_empty() && mcp.is_empty() && destinations.is_empty() && !dns {
        return Ok(None);
    }
    let text: Vec<String> = ports
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let ports = if text.is_empty() {
        shards_net::Ports::default()
    } else {
        shards_net::Ports::parse(&text.join(","))
            .map_err(|e| No(Cause::Image, format!("the image's egress grants: {e}")))?
    };
    let mut named = crate::agentfile::named(&mcp);
    for d in crate::agentfile::named(&destinations) {
        if !named.contains(&d) {
            named.push(d);
        }
    }
    Ok(Some(shards_net::encode_policy(&ports, &named, dns)))
}

/// Refuses a published port that would widen the run: one the image's Agentfile declares
/// `AS egress` (a destination, never a listener), and, for a run with no command of its
/// own, one the image gives no member for ingress (D122).
fn ports_granted(
    publish: &[wire::Publish],
    agentfile: Option<&crate::agentfile::Agentfile>,
    has_command: bool,
) -> Result<(), No> {
    let proto_of = |p: wire::Proto| match p {
        wire::Proto::Tcp => (6u8, shards_net::Proto::Tcp, "tcp"),
        wire::Proto::Udp => (17u8, shards_net::Proto::Udp, "udp"),
    };
    for p in publish {
        let (number, proto, name) = proto_of(p.proto);
        if let Some(a) = agentfile {
            let text: Vec<String> = a
                .grants
                .egress_declared
                .iter()
                .map(|e| String::from_utf8_lossy(e).into_owned())
                .collect();
            let declared = shards_net::Ports::parse(&text.join(",")).unwrap_or_default();
            if declared.has(proto, p.guest_port) {
                return Err(No(
                    Cause::Widening,
                    format!(
                        "port {}/{name}: the image's Agentfile declares it AS egress, a port its members reach out to",
                        p.guest_port
                    ),
                ));
            }
        }
        let received = agentfile.is_some_and(|a| {
            a.receivers
                .iter()
                .any(|r| r.at.0 == number && r.at.1 <= p.guest_port && p.guest_port <= r.at.2)
        });
        if !has_command && !received {
            return Err(No(
                Cause::Widening,
                format!(
                    "port {}/{name}: the image gives no member this port coming in (EXPOSE … AS ingress), and the run has no command of its own",
                    p.guest_port
                ),
            ));
        }
    }
    Ok(())
}

/// Each port `bound` (its guest port, IP protocol number and host address), with whom it
/// reaches (D122): each member the image gives it coming in, by its declared index among
/// `members`, at the member's own port; none where it reaches the run's own command. A
/// receiver that is none of `members` is refused, never read as the command's.
fn published(
    bound: &[(u16, u8, std::net::SocketAddr)],
    receivers: &[shards_agentfile::agentfile::Receiver],
    members: &[wire::MemberInfo],
) -> Result<Vec<wire::Bound>, No> {
    use shards_agentfile::agentfile::TargetKind;
    let mut out = Vec::with_capacity(bound.len());
    for &(guest_port, number, host) in bound {
        let proto = match number {
            6 => wire::Proto::Tcp,
            17 => wire::Proto::Udp,
            _ => continue,
        };
        let mut to = Vec::new();
        for (r, port) in crate::agentfile::receiving(receivers, number, guest_port) {
            let (kind, word) = match r.kind {
                TargetKind::Agent => (wire::MemberKind::Agent, "agent"),
                TargetKind::Harness => (wire::MemberKind::Harness, "harness"),
            };
            let member = members
                .iter()
                .find(|m| m.kind == kind && m.name.as_bytes() == r.name.as_slice())
                .ok_or_else(|| {
                    No(
                        Cause::Image,
                        format!(
                            "port {guest_port}: the image gives it {word} {}, which is none of its members",
                            String::from_utf8_lossy(&r.name)
                        ),
                    )
                })?;
            to.push(wire::Receiving {
                member: member.index,
                port,
            });
        }
        out.push(wire::Bound {
            guest_port,
            proto,
            host,
            to,
        });
    }
    Ok(out)
}

/// The run's request (`kind::RUN`) as a warm VM takes one (warm.rs `head`): its head, then
/// `spec`. It writes no log, yet its head keeps a run's default retention, as a daemon
/// run's with no log does (`--log-driver none`, daemon.rs `DEFAULT_LOGS`): a warm VM
/// refuses a request whose log keeps nothing.
fn run_request(spec: &shards_abi::run::Spec, published: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    shards_ipc::RunHead {
        flags: if published { shards_ipc::RUN_PUBLISHED } else { 0 },
        log_size: crate::daemon::DEFAULT_LOGS.size,
        log_files: crate::daemon::DEFAULT_LOGS.files,
        ..Default::default()
    }
    .encode_into(&mut payload);
    spec.encode_into(&mut payload);
    payload
}

mod hosted {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::io::{self, Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use shards_abi::run::{self as protocol, managed};
    use shards_api::image::wire::{self, Cause, Channel, RunEnd};
    use shards_ipc::Stream;
    use shards_ipc::fd::{AsFd as _, BorrowedFd, OwnedFd};
    use shards_ipc::kind;

    use super::{Answers, No, no};
    use crate::daemon::publish;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the VM is told its link is: a descriptor's number on Unix, a handle's name on
    /// Windows (daemon.rs `VM_LINK`).
    const LINK: &str = if cfg!(windows) { "daemon" } else { "3" };

    /// The run, prepared: its spec, its members, its ports bound, its policy, its
    /// directories open, and what its VM boots.
    struct Plan {
        prepared: crate::run::Prepared,
        members: Vec<wire::MemberInfo>,
        listeners: Vec<publish::Listener>,
        egress: Option<Vec<u8>>,
        dirs: Vec<(OwnedFd, bool, OsString)>,
        /// The anonymous volumes its `VOLUME`s made, removed as it ends (`--rm`'s).
        made: Vec<String>,
        bridge: Option<shards_net::bridge::Bridge>,
        kernel: PathBuf,
        init: PathBuf,
        server: Option<PathBuf>,
        /// Its managed output's members, labels and active ones, where it has any.
        capture: Option<(u32, Vec<u32>)>,
        /// Its domains' plug roots (D235), by name (`Users`), as its setup names them.
        roots: Vec<Vec<u8>>,
    }

    pub(super) fn run(
        answers: &Answers,
        request: &wire::Request,
        ready_by: Option<u64>,
        input: io::Stdin,
    ) -> Result<(), No> {
        let home = shards_ipc::home().map_err(no(Cause::Boot))?;
        let plan = plan(request, ready_by, &home)?;
        launch_and_serve(answers, input, &home, plan)
    }

    /// The request as `shards run` would have asked it of a daemon: the image, its own
    /// command or none, its published ports, its shares as `-v`, a network only where
    /// something goes past the microVM, and the nesting grant as `--security-opt
    /// nesting=true` asks for it (D234), with `ready_by`, its deadline as an instant
    /// (`super::deadline`).
    pub(super) fn as_run(request: &wire::Request, ready_by: Option<u64>) -> shards_ipc::Run {
        let publish = request
            .publish
            .iter()
            .map(|p| shards_ipc::Publish {
                port: p.guest_port,
                proto: match p.proto {
                    wire::Proto::Tcp => "tcp".into(),
                    wire::Proto::Udp => "udp".into(),
                },
                host_ip: p.host.map(|h| h.to_string()).unwrap_or_default(),
                host_port: if p.host_port == 0 {
                    String::new()
                } else {
                    p.host_port.to_string()
                },
            })
            .collect();
        let binds = request
            .shares
            .iter()
            .map(|s| format!("{}:{}{}", s.host, s.at, if s.read_only { ":ro" } else { "" }))
            .collect();
        // With none of its own, an empty entrypoint and a command the run never sends, as
        // `ATTEND` takes its place: the image's command is not composed in.
        let (entrypoint, cmd) = match &request.command {
            Some(argv) => (Some(Vec::new()), argv.clone()),
            None => (Some(Vec::new()), vec!["attend".into()]),
        };
        // A guest of the caller's, as `shards run` takes one (cli/request.rs).
        let given = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty() && Path::new(v).is_absolute())
        };
        shards_ipc::Run {
            image: request.image.clone(),
            kernel: given("SHARDS_KERNEL"),
            init: given("SHARDS_INIT"),
            entrypoint,
            cmd,
            network: if request.publish.is_empty() {
                "none"
            } else {
                "bridge"
            }
            .into(),
            publish,
            binds,
            security_opt: if request.nesting {
                vec!["nesting=true".into()]
            } else {
                Vec::new()
            },
            ready_by,
            remove: true,
            ..Default::default()
        }
    }

    fn plan(request: &wire::Request, ready_by: Option<u64>, home: &Path) -> Result<Plan, No> {
        let mut run = as_run(request, ready_by);
        let say = |line: &str| {
            let _ = writeln!(io::stderr(), "{line}");
        };
        let cancel = shards_registry::http::Cancel::new();
        let mut prepared = crate::run::prepare(&run, home, &say, &cancel).map_err(no(Cause::Image))?;
        if request.vcpus > 0 {
            prepared.size.0 = request.vcpus;
        }
        if request.memory_mib > 0 {
            prepared.size.1 = request.memory_mib;
        }
        let agentfile = prepared.agentfile.clone();
        let (labels, active) = agentfile
            .as_ref()
            .map(|a| (a.managed_labels.clone(), a.active_members.clone()))
            .unwrap_or_default();
        if request.command.is_none() && active.is_empty() {
            return Err(No(
                Cause::Image,
                format!(
                    "{}: no member of its Agentfile runs, and the run has no command",
                    request.image
                ),
            ));
        }
        let members = super::members_of(&labels, &active);
        let staged = super::staged(request, &members)?;
        super::ports_granted(&request.publish, agentfile.as_ref(), request.command.is_some())?;
        let egress = super::egress(agentfile.as_ref().map(|a| &a.grants), &request.egress)?;
        if egress.is_some() {
            run.network = "bridge".into();
        }
        if let Some(a) = &agentfile {
            a.refuse(&run).map_err(no(Cause::Request))?;
        }
        let nesting = super::nesting(agentfile.as_ref(), &run, &say)?;
        let bridge = if run.network == "bridge" {
            Some(
                shards_net::bridge::elected_here(&mut |note| say(&note))
                    .ok_or_else(|| No(Cause::Boot, shards_net::bridge::NO_SUBNET.into()))?,
            )
        } else {
            None
        };
        // Its published ports, bound now, as dockerd's allocator binds them (D31).
        let (bindings, alone) = publish::wanted(&run, &prepared.exposed);
        if let Some(e) = publish::unsupported(&bindings) {
            return Err(No(Cause::Request, e));
        }
        let bound = publish::bind(&bindings, &alone, None, publish::v6_listenable(), |_, _| {
            publish::InUse::Host
        })
        .map_err(no(Cause::Boot))?;
        // Its volumes: each share at its path as `-v` puts it, given where the image's
        // `VOLUME` there is for a member (D109), each other `VOLUME` a volume of the store.
        let store = crate::volumes::Store::new(home);
        let from = |_: &str| -> Result<Vec<crate::volumes::MountPoint>, String> {
            Err("--volumes-from is no orchestrated run's".into())
        };
        let points = crate::volumes::register(
            &store,
            &run,
            &prepared.image_volumes,
            agentfile.as_ref().map(|a| a.volumes.as_slice()),
            &from,
            "",
        )
        .map_err(no(Cause::Boot))?;
        let points: Vec<crate::volumes::MountPoint> = points.into_iter().map(|(p, _)| p).collect();
        let made = points
            .iter()
            .filter(|p| p.kind == "volume" && store.get(&p.name).is_some_and(|v| v.anonymous))
            .map(|p| p.name.clone())
            .collect();
        let opened = crate::volumes::open_for(&points, true, &store, &crate::volumes::Access::default())
            .map_err(no(Cause::Boot))?;
        // Its setup, as the daemon composes a run's (daemon.rs `start`): its mounts, its
        // capabilities beside its domains, its security, its domains' filter, its managed
        // output, what it stages, its members' ends asked for, and its own command or
        // `ATTEND`.
        let (_, _, most) = crate::run::sizes(&run.resources, crate::build::host_memory());
        let mut setup = crate::setup::setup(&run, &opened.mounts, None, Some(crate::run::shm_bytes(most)))
            .map_err(no(Cause::Request))?;
        if let Some(a) = agentfile.as_ref().filter(|a| a.domains > 0 && a.uplink) {
            setup.retain(|e| !e.starts_with(b"caps="));
            setup.push(format!("caps={}", a.caps(&run)).into_bytes());
        }
        let boot = match &prepared.boot {
            crate::run::Boot::Given(cfg) => (cfg.kernel.clone(), cfg.init.clone()),
            crate::run::Boot::Stored(guest) => (guest.kernel.clone(), Some(guest.init.clone())),
        };
        let (kernel, init) = (
            boot.0,
            boot.1.ok_or_else(|| No(Cause::Boot, "no init to boot".into()))?,
        );
        let version = crate::guest::version_of(&kernel).map_err(no(Cause::Boot))?;
        crate::setup::security_setup(&mut setup, &run, version, None, &[], nesting.grant())
            .map_err(no(Cause::Request))?;
        let server = match &agentfile {
            Some(a) => {
                setup.extend(crate::setup::domain_seccomp(version).map_err(no(Cause::Boot))?);
                setup.extend(a.image_env.iter().cloned());
                Some(crate::guest::server_device(home).map_err(no(Cause::Boot))?)
            }
            None => None,
        };
        // Its domains' plug roots (D235), as a daemon's run names them: this host's own and
        // each share's top-level directory, beneath which a plug asked of it lies.
        let roots = match &agentfile {
            Some(_) => crate::setup::plug_roots(&points),
            None => Vec::new(),
        };
        setup.extend(roots.iter().cloned());
        let roots = roots
            .iter()
            .filter_map(|e| e.strip_prefix(protocol::plug::ROOT_KEY))
            .filter_map(protocol::plug::root_name)
            .map(<[u8]>::to_vec)
            .collect();
        // Grants past the microVM: the run's own processes kept to eth0's subnet (D59).
        if agentfile.as_ref().is_some_and(|a| a.grants.past()) {
            setup.push(b"confine-eth0".to_vec());
        }
        let capture = if active.is_empty() {
            None
        } else {
            let count =
                u32::try_from(labels.len()).map_err(|_| No(Cause::Image, "too many members".into()))?;
            setup.push(format!("managed-capacity={}", protocol::MAX_PAYLOAD).into_bytes());
            setup.push(format!("managed-members={count}").into_bytes());
            let mut activation = managed::ACTIVE_KEY.to_vec();
            for m in &active {
                activation.extend_from_slice(&m.to_be_bytes());
            }
            setup.push(activation);
            Some((count, active.clone()))
        };
        setup.extend(staged);
        setup.push(protocol::members::ENDS.to_vec());
        prepared.spec.setup = setup;
        prepared.spec.cgroup = crate::resources::cgroup(&run.resources, prepared.cpus)
            .map_err(|e| No(Cause::Request, e.message(format_args!("its limits"))))?;
        if bridge.is_some() {
            prepared.spec.resolv = Some(crate::build::step::resolv_with(
                &crate::build::step::host_resolv(),
                false,
                &[],
                &[],
                &[],
            ));
        }
        if request.command.is_none() {
            prepared.spec.argv.clear();
            prepared.spec.builtin = protocol::builtin::ATTEND;
        }
        crate::spec::fits(&prepared.spec).map_err(no(Cause::Request))?;
        Ok(Plan {
            prepared,
            members,
            listeners: bound.listeners,
            egress,
            dirs: opened.dirs,
            made,
            bridge,
            kernel,
            init,
            server,
            capture,
            roots,
        })
    }

    /// The run's processes, started: its network's, its shares', its VM's.
    struct Launched {
        vm: shards_ipc::Child,
        link: Stream,
        net: Option<(crate::netproc::NetProcess, crate::netproc::Control)>,
        shares: Option<Shares>,
        /// On Windows the job each of them is made in, whose last handle is this
        /// process's (`GroupJob::ending_with_this_process`): they end as it ends, however
        /// it ends, as a direct boot's do (D141, D197); let go of once they are reaped.
        #[cfg(windows)]
        job: shards_ipc::GroupJob,
    }

    /// The run's share process (share.rs), and what of it its VM is handed: on Unix its
    /// connection, on which it hands the VM each share's stream; on Windows each share's
    /// stream itself (D197), sent after the run's request.
    struct Shares {
        child: shards_ipc::Child,
        #[cfg(unix)]
        door: Stream,
        #[cfg(windows)]
        ends: Vec<OwnedFd>,
    }

    impl Shares {
        /// Its end, its VM's gone: what of it this process holds let go of, the last of its
        /// streams where the VM never took them (a run refused before `TAKEN`), then waited
        /// for as a network process is, [`crate::netproc::GRACE`] at most, and ended. Waited
        /// for with its door still held here, it never went (CI run 43230466376,
        /// `a_share_at_a_granted_volume_reaches_the_harness`, this process in `do_wait`).
        fn end(self) {
            let Shares {
                child,
                #[cfg(unix)]
                door,
                #[cfg(windows)]
                ends,
            } = self;
            #[cfg(unix)]
            drop(door);
            #[cfg(windows)]
            drop(ends);
            crate::netproc::reap(&child);
        }
    }

    /// The VM process's arguments, as the daemon's for a run's VM (daemon.rs `cold`): its
    /// guest, booted in its warm mode for the one run handed it on its link, the in-VM
    /// server's device after its root, and its shares' count.
    fn vm_args(plan: &Plan) -> Vec<OsString> {
        let mut cmdline = shards_vmm::vm::Config::new(plan.kernel.clone(), None).cmdline;
        if let Some(bridge) = &plan.bridge {
            cmdline.push(' ');
            cmdline.push_str(&bridge.cmdline());
        }
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--kernel".into(),
            plan.kernel.clone().into(),
            "--cmdline".into(),
            cmdline.into(),
            "--cpus".into(),
            plan.prepared.size.0.to_string().into(),
            "--memory".into(),
            plan.prepared.size.1.to_string().into(),
            "--rootfs".into(),
            plan.prepared.rootfs.clone().into(),
            "--init".into(),
            plan.init.clone().into(),
            // Memory its workload frees given back to the host as it reports it (D151).
            "--reporting".into(),
            "--warm".into(),
            LINK.into(),
        ];
        if let Some(server) = &plan.server {
            args.extend(["--pmem".into(), server.clone().into()]);
        }
        // Slots for what is plugged into it as it runs (D214, D235), as a daemon run's.
        let slots = crate::daemon::pci_slots();
        if slots > 0 {
            args.extend(["--pci-slots".into(), slots.to_string().into()]);
        }
        if !plan.dirs.is_empty() {
            args.extend(["--shares".into(), plan.dirs.len().to_string().into()]);
        }
        args
    }

    /// The home's private directory, made if it is not there: where a share process keeps
    /// the files a restored guest holds that no path reaches (D176; daemon.rs
    /// `private_dir`).
    fn private_dir(home: &Path) -> Result<std::fs::File, String> {
        let at = home.join(crate::daemon::PRIVATE);
        match crate::os::owner_only_dir(std::fs::DirBuilder::new().recursive(true)).create(&at) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", at.display()));
            }
            _ => {}
        }
        #[cfg(unix)]
        return std::fs::File::open(&at).map_err(|e| format!("{}: {e}", at.display()));
        // A directory opens on Windows only with backup semantics.
        #[cfg(windows)]
        shards_vmm::platform::open_dir(&at).map_err(|e| format!("{}: {e}", at.display()))
    }

    #[cfg(unix)]
    fn launch(home: &Path, plan: &Plan) -> Result<Launched, No> {
        let boot = |e: String| No(Cause::Boot, e);
        // Its network process, denying all until its policy comes, as a daemon run's.
        let (net, side) = match &plan.bridge {
            Some(bridge) => {
                let mac = shards_net::random_mac().map_err(|e| boot(format!("a guest's MAC: {e}")))?;
                let (child, side) =
                    crate::netproc::start(shards_net::Policy::DenyAll, &mac, bridge, None).map_err(boot)?;
                (Some((child, mac)), Some(side))
            }
            None => (None, None),
        };
        let shares = if plan.dirs.is_empty() {
            None
        } else {
            Some(start_shares(home, &plan.dirs).map_err(boot)?)
        };
        let (ours, theirs) = Stream::pair().map_err(|e| boot(format!("the VM's link: {e}")))?;
        let grants = if cfg!(target_os = "macos") {
            Some(Stream::pair().map_err(|e| boot(format!("the VM's grants: {e}")))?)
        } else {
            None
        };
        let mut args = vm_args(plan);
        if grants.is_some() {
            args.extend(["--grants".into(), "4".into()]);
        }
        let release = crate::netproc::VM_RELEASE_FD.to_string();
        if let Some((_, mac)) = &net {
            args.extend([
                "--net".into(),
                crate::netproc::VmSide::arg(mac).into(),
                "--net-release".into(),
                release.into(),
            ]);
        }
        let null =
            std::fs::File::open(crate::os::NULL).map_err(|e| boot(format!("{}: {e}", crate::os::NULL)))?;
        let err = io::stderr();
        let mut fds: Vec<(BorrowedFd<'_>, i32)> = vec![
            (null.as_fd(), 0),
            (err.as_fd(), 1),
            (err.as_fd(), 2),
            (theirs.as_fd(), 3),
        ];
        if let Some((_, granted)) = &grants {
            fds.push((granted.as_fd(), 4));
        }
        if let Some(side) = &side {
            let [r, s, w] = crate::netproc::VM_FDS;
            fds.extend([
                (side.region.as_fd(), r),
                (side.sleeps.as_fd(), s),
                (side.rings.as_fd(), w),
                (side.release.as_fd(), crate::netproc::VM_RELEASE_FD),
            ]);
        }
        let argv: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let env = crate::netproc::child_env();
        let binary = crate::helpers::vm().map_err(boot)?;
        let spawned = shards_ipc::spawn_in(&binary, &argv, &fds, false, &crate::netproc::env_pairs(&env));
        drop(fds);
        let vm = match spawned {
            Ok(vm) => vm,
            Err(e) => {
                if let Some((child, _)) = &net {
                    crate::netproc::reap(child);
                }
                return Err(boot(format!("starting {}: {e}", binary.display())));
            }
        };
        drop(theirs);
        // On macOS the VM, in App Sandbox from its launch, asks this process for the files
        // its arguments name (D30), on a thread of its own that ends with the VM.
        if let Some((ours, granted)) = grants {
            drop(granted);
            #[cfg(target_os = "macos")]
            if let Err(e) = std::thread::Builder::new()
                .name("vm grants".into())
                .spawn(move || {
                    let _ = crate::grant_answer::serve(&ours, None);
                })
            {
                let _ = vm.kill(shards_ipc::signal::SIGKILL);
                return Err(boot(format!("answering the VM: {e}")));
            }
            #[cfg(not(target_os = "macos"))]
            drop(ours);
        }
        let net = match (net, side) {
            (Some((child, _)), Some(side)) => Some((child, side.control)),
            _ => None,
        };
        Ok(Launched {
            vm,
            link: ours,
            net,
            shares,
        })
    }

    /// [`launch`] on Windows (D141, D142, D195), as the daemon starts a run's VM: every
    /// process of the run's made in a job of this process's that ends them as it ends; the
    /// VM process confined in an AppContainer of its own, made suspended, given by name its
    /// link (`daemon`, the other end of a pair of this process's), the null device, the
    /// pipes it asks for its files on and its network's ring and doorbells; its network
    /// process started next, given the VM's process to watch; then the VM runs, and this
    /// process answers what it asks for on a thread of its own, which ends as the VM does.
    #[cfg(windows)]
    fn launch(home: &Path, plan: &Plan) -> Result<Launched, No> {
        use std::os::windows::io::{AsHandle as _, OwnedHandle};
        let boot = |e: String| No(Cause::Boot, e);
        // Made before any process of the run's, and held until each is reaped.
        let job = shards_ipc::GroupJob::ending_with_this_process()
            .map_err(|e| boot(format!("the run's job: {e}")))?;
        // Its network, denying all until its policy comes, as a daemon run's.
        let network = match &plan.bridge {
            Some(bridge) => {
                let mac = shards_net::random_mac().map_err(|e| boot(format!("a guest's MAC: {e}")))?;
                let (ready, side) =
                    crate::netproc::prepare(shards_net::Policy::DenyAll, &mac, bridge).map_err(boot)?;
                Some((mac, ready, side))
            }
            None => None,
        };
        let shares = if plan.dirs.is_empty() {
            None
        } else {
            Some(start_shares(home, &plan.dirs, &job).map_err(boot)?)
        };
        let (ours, theirs) = Stream::pair().map_err(|e| boot(format!("the VM's link: {e}")))?;
        // Its end, the VM's alone once given it: this process's copy goes, so that the
        // VM's end is the end this process reads.
        let link = theirs.share().map_err(|e| boot(format!("the VM's link: {e}")))?;
        drop(theirs);
        let container = crate::helpers::containers()
            .and_then(|records| {
                shards_ipc::AppContainer::new("vm", &records)
                    .map_err(|e| format!("the VM's AppContainer: {e}"))
            })
            .map_err(boot)?;
        // A pipe each way: the VM's requests, and the answers.
        let pipes = std::io::pipe().and_then(|asks| Ok((asks, std::io::pipe()?)));
        let ((asks_from, asks_to), (answers_from, answers_to)) =
            pipes.map_err(|e| boot(format!("the VM's grants: {e}")))?;
        let (asks_to, answers_from) = (OwnedHandle::from(asks_to), OwnedHandle::from(answers_from));
        // The null device, for the stdio it lets go of: its AppContainer opens no file by
        // name, NUL among them on Windows Server (PM M351).
        let null =
            std::fs::File::open(crate::os::NULL).map_err(|e| boot(format!("{}: {e}", crate::os::NULL)))?;
        let mut given = vec![
            shards_ipc::Given {
                name: "ask",
                handle: asks_to.as_handle(),
            },
            shards_ipc::Given {
                name: "answer",
                handle: answers_from.as_handle(),
            },
            shards_ipc::Given {
                name: LINK,
                handle: link.as_handle(),
            },
            shards_ipc::Given {
                name: "null",
                handle: null.as_handle(),
            },
        ];
        let mut sockets = Vec::new();
        if let Some((_, _, side)) = &network {
            given.extend(side.given());
            sockets.extend([side.shared(), side.release()]);
        }
        let mut args = vm_args(plan);
        if let Some((mac, _, _)) = &network {
            args.extend(["--net".into(), crate::netproc::VmSide::arg(mac).into()]);
        }
        let argv: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let err = io::stderr();
        let env = crate::netproc::child_env();
        let binary = crate::helpers::vm().map_err(boot)?;
        let suspended = shards_ipc::spawn_suspended(&shards_ipc::Spawn {
            program: &binary,
            args: &argv,
            grants_at: 1,
            given: &given,
            stdin: None,
            stdout: err.as_handle(),
            stderr: err.as_handle(),
            sockets: &sockets,
            env: &crate::netproc::env_pairs(&env),
            container: Some(&container),
            holds: shards_ipc::Holds::Hypervisor,
            group: Some(&job),
            graphics: false,
        })
        .map_err(|e| boot(format!("starting {}: {e}", binary.display())))?;
        // Shared with it already, and its handles duplicated into it.
        drop((given, sockets));
        // Its network process, given the VM's process to watch, before the VM runs; its
        // control this process's, where the run's published ports and policy go.
        let net = match network {
            Some((_, ready, side)) => {
                let started = crate::netproc::start(ready, &suspended, Some(&job)).map_err(boot)?;
                let control = crate::netproc::Control {
                    socket: side.control,
                    pid: started.id(),
                };
                Some((started, control))
            }
            None => None,
        };
        let giver = suspended
            .giver()
            .map_err(|e| boot(format!("answering the VM: {e}")))?;
        let gives = suspended
            .giver()
            .map_err(|e| boot(format!("the VM's link: {e}")))?
            .into_handle();
        let mut vm = suspended
            .resume()
            .map_err(|e| boot(format!("starting {}: {e}", binary.display())))?;
        // Its AppContainer's profile goes once it has.
        vm.hold(container);
        // What this process sends on its link goes to the VM: handles duplicated into it.
        ours.gives_to(gives);
        drop((asks_to, answers_from, link, null));
        let grants = crate::grant::Link {
            from: std::fs::File::from(OwnedHandle::from(asks_from)),
            to: std::fs::File::from(OwnedHandle::from(answers_to)),
        };
        if let Err(e) = std::thread::Builder::new()
            .name("vm grants".into())
            .spawn(move || {
                let _ = crate::grant_answer::serve(&grants, &giver);
            })
        {
            let _ = vm.kill(shards_ipc::signal::SIGKILL);
            return Err(boot(format!("answering the VM: {e}")));
        }
        Ok(Launched {
            vm,
            link: ours,
            net,
            shares,
            job,
        })
    }

    /// The share process (share.rs) serving `dirs`, as the daemon starts a run's
    /// (daemon.rs `started_shares`): its connection to it at 3, each directory at 4 + i,
    /// the home's private directory after them. The VM's end of its connection goes with
    /// the run, which hands the VM each share's stream (`SHARE_ENDS`).
    #[cfg(unix)]
    fn start_shares(home: &Path, dirs: &[(OwnedFd, bool, OsString)]) -> Result<Shares, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let (door, share) = Stream::pair().map_err(|e| format!("the share process's connection: {e}"))?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("share")];
        for (_, read_only, only) in dirs {
            args.push(OsStr::new(if *read_only { "ro" } else { "rw" }));
            args.push(only.as_os_str());
        }
        let err = io::stderr();
        let ghosts = private_dir(home)?;
        let number = |at: usize| i32::try_from(at).map_err(|_| "too many shares".to_string());
        let mut fds = vec![(err.as_fd(), 2), (share.as_fd(), 3)];
        for (i, (dir, _, _)) in dirs.iter().enumerate() {
            fds.push((dir.as_fd(), number(4usize.saturating_add(i))?));
        }
        fds.push((ghosts.as_fd(), number(4usize.saturating_add(dirs.len()))?));
        let child = shards_ipc::spawn_in(&exe, &args, &fds, false, &[])
            .map_err(|e| format!("starting the share process: {e}"))?;
        Ok(Shares { child, door })
    }

    /// [`start_shares`] on Windows (D197), as a direct boot's (cli/shares.rs `served`): each
    /// share's stream a duplex pipe this process makes, its server end the share process's
    /// and its other the VM's, which the run's request hands it. Each directory at 4 + i,
    /// the home's private directory at 4 + n and each share's end of its stream at
    /// 5 + n + i, as its Unix twin is numbered; made in the run's job, detached from the
    /// console, out of reach of a Ctrl-C meant for this process's owner.
    #[cfg(windows)]
    fn start_shares(
        home: &Path,
        dirs: &[(OwnedFd, bool, OsString)],
        job: &shards_ipc::GroupJob,
    ) -> Result<Shares, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let streams = dirs
            .iter()
            .map(|_| shards_ipc::byte_pair())
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| format!("a share's stream: {e}"))?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("share")];
        for (_, read_only, only) in dirs {
            args.push(OsStr::new(if *read_only { "ro" } else { "rw" }));
            args.push(only.as_os_str());
        }
        let n = dirs.len();
        let number = |at: usize| i32::try_from(at).map_err(|_| "too many shares".to_string());
        let err = io::stderr();
        let ghosts = private_dir(home)?;
        let mut fds = vec![
            (err.as_fd(), 2),
            (ghosts.as_fd(), number(4usize.saturating_add(n))?),
        ];
        for (i, ((dir, _, _), (server, _))) in dirs.iter().zip(&streams).enumerate() {
            fds.push((dir.as_fd(), number(4usize.saturating_add(i))?));
            fds.push((
                server.as_fd(),
                number(5usize.saturating_add(n).saturating_add(i))?,
            ));
        }
        let env = crate::netproc::child_env();
        let child =
            shards_ipc::spawn_detached(Some(job), &exe, &args, &fds, &crate::netproc::env_pairs(&env))
                .map_err(|e| format!("starting the share process: {e}"))?;
        Ok(Shares {
            child,
            ends: streams.into_iter().map(|(_, vm)| vm).collect(),
        })
    }

    /// Tells the network process, before the VM has the run, its published ports and its
    /// policy, each answered by its own kind in the order asked (daemon/networks.rs
    /// `Asks`); the descriptors kept until answered (M24).
    fn tell_net(
        net: &crate::netproc::Control,
        listeners: &[publish::Listener],
        egress: Option<&[u8]>,
    ) -> Result<(), String> {
        let mut asked = Vec::new();
        for batch in listeners.chunks(shards_ipc::MAX_FDS) {
            let ports: Vec<u8> = batch
                .iter()
                .flat_map(|l| {
                    let [hi, lo] = l.guest_port.to_be_bytes();
                    [hi, lo, l.proto]
                })
                .collect();
            let fds: Vec<crate::netproc::Lent<'_>> =
                batch.iter().map(|l| crate::netproc::lend(&l.fd)).collect();
            crate::netproc::send(net, kind::PUBLISH, &ports, &fds)
                .map_err(|e| format!("published ports: {e}"))?;
            asked.push((kind::PUBLISH, "published ports"));
        }
        if let Some(policy) = egress {
            crate::netproc::send(net, kind::NET_POLICY, policy, &[])
                .map_err(|e| format!("egress grants: {e}"))?;
            asked.push((kind::NET_POLICY, "egress grants"));
        }
        for (which, what) in asked {
            match crate::netproc::recv(net) {
                Ok(Some(m)) if m.kind == which => {}
                Ok(_) => return Err(format!("{what}: the network process did not take them")),
                Err(e) => return Err(format!("{what}: the network process: {e}")),
            }
        }
        Ok(())
    }

    /// The run's managed output, owned here: each active member channel's credit and what
    /// is waiting of it for the orchestrator's room.
    #[derive(Default)]
    struct Flows {
        /// The orchestrator's room for each channel, in bytes.
        room: HashMap<(u32, Channel), u64>,
        /// A record's bytes the orchestrator had no room for yet.
        waiting: HashMap<(u32, Channel), Vec<u8>>,
        /// A credit granted the guest that no record has used yet.
        granted: HashMap<(u32, Channel), bool>,
        /// The channel has ended.
        ended: HashMap<(u32, Channel), bool>,
    }

    /// What the serving threads share.
    struct Live<'a> {
        answers: &'a Answers,
        flows: Mutex<Flows>,
        capture: Option<Mutex<Stream>>,
        link: Mutex<Stream>,
        /// Each exec's client end, by its number, held until the VM says it has it
        /// (`EXEC_TAKEN`), as the daemon holds an exec's (M24): none once the link has
        /// closed, when no VM will.
        held: Mutex<Option<HashMap<u64, Stream>>>,
        /// `STOP` asked, or the owner gone: the run's end is `Stopped`.
        stopping: std::sync::atomic::AtomicBool,
        next_exec: std::sync::atomic::AtomicU64,
        /// The home, whose private directory each share process keeps its ghosts in.
        home: &'a Path,
        /// The run's members, and its domains' plug roots by name (D235).
        members: &'a [wire::MemberInfo],
        roots: &'a [Vec<u8>],
        /// Each device step asked of the VM and not yet answered (`DEVICED`), by its number;
        /// none once the VM's link has closed, as nothing will answer one then.
        devices: Mutex<Option<Vec<Asked>>>,
        /// What is plugged into the run's members, by the path it is at.
        plugged: Mutex<HashMap<String, Plugged>>,
        /// On Windows the run's job, which each plug's share process is made in too.
        #[cfg(windows)]
        job: &'a shards_ipc::GroupJob,
    }

    /// A device step asked of the VM: its number, and who waits for its answer.
    type Asked = (u64, std::sync::mpsc::Sender<Result<String, String>>);

    /// A host directory plugged into a member (D235): its place on the microVM's PCI bus
    /// (`devfn`), its guest's address of it, the member's index, and its share process.
    struct Plugged {
        devfn: u8,
        address: String,
        member: u32,
        share: Shares,
    }

    fn channel_of(c: managed::Channel) -> Channel {
        match c {
            managed::Channel::Stdout => Channel::Stdout,
            managed::Channel::Stderr => Channel::Stderr,
        }
    }

    fn managed_channel(c: Channel) -> managed::Channel {
        match c {
            Channel::Stdout => managed::Channel::Stdout,
            Channel::Stderr => managed::Channel::Stderr,
        }
    }

    impl Live<'_> {
        /// Grants the guest a record on `key`'s channel where it has none, its room is
        /// not spent, nothing waits of it, and it has not ended: as many bytes as the
        /// orchestrator has room for, past the record's head, within the protocol's
        /// ceiling and at least a record of an errno's (`Credit::decode`'s least).
        fn grant(&self, flows: &mut Flows, key: (u32, Channel)) -> io::Result<()> {
            let Some(capture) = &self.capture else {
                return Ok(());
            };
            let room = flows.room.get(&key).copied().unwrap_or(0);
            if room == 0
                || flows.granted.get(&key).copied().unwrap_or(false)
                || flows.ended.get(&key).copied().unwrap_or(false)
                || flows.waiting.get(&key).is_some_and(|w| !w.is_empty())
            {
                return Ok(());
            }
            let least = (managed::HEADER + size_of::<i32>()) as u64;
            let capacity = room
                .saturating_add(managed::HEADER as u64)
                .clamp(least, u64::from(protocol::MAX_PAYLOAD));
            let credit = managed::Credit {
                member: key.0,
                channel: managed_channel(key.1),
                capacity,
            }
            .encode()
            .map_err(|e| io::Error::other(e.to_string()))?;
            let mut frame = protocol::header(kind_credit(), managed::CREDIT_LEN as u32).to_vec();
            frame.extend_from_slice(&credit);
            lock(capture).write_all(&frame)?;
            flows.granted.insert(key, true);
            Ok(())
        }

        /// Sends what waits of `key`'s channel within the orchestrator's room.
        fn flush(&self, flows: &mut Flows, key: (u32, Channel)) -> io::Result<()> {
            let room = flows.room.entry(key).or_insert(0);
            let Some(waiting) = flows.waiting.get_mut(&key) else {
                return Ok(());
            };
            let n = usize::try_from(*room).unwrap_or(usize::MAX).min(waiting.len());
            if n > 0 {
                let part: Vec<u8> = waiting.drain(..n).collect();
                *room = room.saturating_sub(n as u64);
                self.answers.send(
                    wire::to_client::OUTPUT,
                    &[&wire::Output::head(key.0, key.1), &part],
                )?;
            }
            Ok(())
        }
    }

    fn kind_credit() -> u8 {
        protocol::kind::MANAGED_CREDIT
    }

    /// Hands the VM its run, as the daemon hands a warm VM one (daemon.rs `hand_over`): once
    /// it is `READY`, `RUN` with the run's head, spec and descriptors, on Windows its
    /// shares' streams after it (D197), then `TAKEN`; then `STOP_ORDER`, SIGKILL with no
    /// grace, so that a VM whose link closes, this process gone, ends its run.
    fn hand_over(
        link: &Stream,
        plan: &Plan,
        client: &Stream,
        shares: Option<&Shares>,
        capture: Option<&Stream>,
    ) -> Result<(), String> {
        match shards_ipc::recv(link) {
            Ok(Some(m)) if m.kind == kind::READY => super::say(format_args!("its guest is ready")),
            Ok(Some(m)) => return Err(format!("the VM said message kind {} before it was ready", m.kind)),
            Ok(None) => return Err("the VM ended before it was ready".into()),
            Err(e) => return Err(format!("the VM was not ready: {e}")),
        }
        let null = std::fs::File::open(crate::os::NULL).map_err(|e| format!("{}: {e}", crate::os::NULL))?;
        let err = io::stderr();
        let payload = super::run_request(&plan.prepared.spec, !plan.listeners.is_empty());
        // A stream's end as a message hands it on: borrowed on Unix, a handle of its own on
        // Windows (`shards_ipc::handed`).
        let client = shards_ipc::handed(client).map_err(|e| format!("the run's client: {e}"))?;
        let capture = capture
            .map(shards_ipc::handed)
            .transpose()
            .map_err(|e| format!("the run's managed output: {e}"))?;
        // The run's own command's stdin is the null device, its output this process's
        // diagnostics: the orchestrator's channel carries its members'.
        let mut fds: Vec<BorrowedFd<'_>> = vec![client.as_fd(), null.as_fd(), err.as_fd(), err.as_fd()];
        #[cfg(unix)]
        fds.extend(shares.map(|s| s.door.as_fd()));
        fds.extend(capture.as_ref().map(|c| c.as_fd()));
        shards_ipc::send(link, kind::RUN, &payload, &fds)
            .map_err(|e| format!("handing the VM its run: {e}"))?;
        // MAX_FDS a message at most; the VM takes the run only once it has them all.
        #[cfg(windows)]
        if let Some(shares) = shares {
            let ends: Vec<BorrowedFd<'_>> = shares.ends.iter().map(|e| e.as_fd()).collect();
            for batch in ends.chunks(shards_ipc::MAX_FDS) {
                shards_ipc::send(link, kind::SHARE_ENDS, &[], batch)
                    .map_err(|e| format!("handing the VM its shares' streams: {e}"))?;
            }
        }
        match shards_ipc::recv(link) {
            Ok(Some(m)) if m.kind == kind::TAKEN => super::say(format_args!("its VM took the run")),
            Ok(Some(m)) => return Err(format!("the VM said message kind {} for its run", m.kind)),
            Ok(None) => return Err("the VM ended before it took its run".into()),
            Err(e) => return Err(format!("the VM did not take its run: {e}")),
        }
        let mut order = Vec::with_capacity(12);
        order.extend_from_slice(&0u32.to_be_bytes());
        order.extend_from_slice(&0u64.to_be_bytes());
        shards_ipc::send(link, kind::STOP_ORDER, &order, &[]).map_err(|e| format!("the VM's stop order: {e}"))
    }

    /// A pair for a stream the VM is handed, as the daemon makes each (daemon/managed.rs,
    /// the managed output's): on Windows one of bytes alone written in messages, each end
    /// saying the end of its writes as a socket's half-close does, its other end's handle
    /// given the VM ([`Stream::plain_pair`]).
    fn handed_pair() -> io::Result<(Stream, Stream)> {
        #[cfg(unix)]
        return Stream::pair();
        #[cfg(windows)]
        Stream::plain_pair()
    }

    fn launch_and_serve(answers: &Answers, input: io::Stdin, home: &Path, plan: Plan) -> Result<(), No> {
        let boot = |e: String| No(Cause::Boot, e);
        super::say(format_args!(
            "prepared {}, {} vCPUs, {} MiB; starting its VM",
            plan.prepared.image_id, plan.prepared.size.0, plan.prepared.size.1
        ));
        let launched = launch(home, &plan)?;
        super::say(format_args!(
            "its VM process {} started; waiting for its guest",
            launched.vm.id()
        ));
        let served = (|| -> Result<(), No> {
            if let Some((_, control)) = &launched.net {
                tell_net(control, &plan.listeners, plan.egress.as_deref()).map_err(boot)?;
            }
            let (client, client_theirs) =
                handed_pair().map_err(|e| boot(format!("the run's client: {e}")))?;
            let capture = match plan.capture {
                Some(_) => Some(handed_pair().map_err(|e| boot(format!("the run's managed output: {e}")))?),
                None => None,
            };
            hand_over(
                &launched.link,
                &plan,
                &client_theirs,
                launched.shares.as_ref(),
                capture.as_ref().map(|(_, theirs)| theirs),
            )
            .map_err(boot)?;
            let bound: Vec<(u16, u8, std::net::SocketAddr)> = plan
                .listeners
                .iter()
                .map(|l| (l.guest_port, l.proto, l.at))
                .collect();
            let receivers = plan
                .prepared
                .agentfile
                .as_ref()
                .map(|a| a.receivers.as_slice())
                .unwrap_or_default();
            let published = super::published(&bound, receivers, &plan.members)?;
            let started = wire::Started {
                members: plan.members.clone(),
                published,
            }
            .encode()
            .map_err(|e| boot(e.to_string()))?;
            let link = launched
                .link
                .try_clone()
                .map_err(|e| boot(format!("the VM's link: {e}")))?;
            let live = Live {
                answers,
                flows: Mutex::new(Flows::default()),
                capture: capture.map(|(ours, _)| Mutex::new(ours)),
                link: Mutex::new(link),
                held: Mutex::new(Some(HashMap::new())),
                stopping: std::sync::atomic::AtomicBool::new(false),
                next_exec: std::sync::atomic::AtomicU64::new(0),
                home,
                members: &plan.members,
                roots: &plan.roots,
                devices: Mutex::new(Some(Vec::new())),
                plugged: Mutex::new(HashMap::new()),
                #[cfg(windows)]
                job: &launched.job,
            };
            answers
                .send(wire::to_client::STARTED, &[&started])
                .map_err(|e| boot(format!("its orchestrator: {e}")))?;
            serve(&live, input, &launched, client);
            // What was plugged in goes with the VM: each share process ends as its streams
            // close (`Shares::end`).
            for (_, p) in std::mem::take(&mut *lock(&live.plugged)) {
                p.share.end();
            }
            Ok(())
        })();
        // Its processes gone, whatever ended it: the VM killed where it runs on, then each
        // reaped once nothing of this process's holds it up, the share process as its
        // connections to the VM close (`Shares::end`); on Windows its job let go of last,
        // which ends whatever of them is left; its anonymous volumes removed, as `--rm`'s
        // are.
        let Launched {
            vm,
            link,
            net,
            shares,
            #[cfg(windows)]
            job,
        } = launched;
        let _ = vm.kill(shards_ipc::signal::SIGKILL);
        let _ = vm.wait();
        drop(link);
        if let Some((child, control)) = net {
            drop(control);
            crate::netproc::reap(&child);
        }
        if let Some(shares) = shares {
            shares.end();
        }
        #[cfg(windows)]
        drop(job);
        if !plan.made.is_empty() {
            let store = crate::volumes::Store::new(home);
            let _held = crate::volumes::lock();
            for name in &plan.made {
                if let Err(e) = store.remove(name) {
                    let _ = writeln!(io::stderr(), "shards api-run: its volume {name}: {e}");
                }
            }
        }
        served
    }

    /// Serves the run until it ends and its owner has heard: the run's managed output and
    /// the VM's link on threads of their own, the owner's asks on this one. The VM's end
    /// says the run's (`ENDED`), which has the owner close its channel; `STOP`, or the
    /// channel closed first, kills the VM, whose end then says `Stopped`.
    fn serve(live: &Live<'_>, input: io::Stdin, launched: &Launched, client: Stream) {
        std::thread::scope(|scope| {
            if let Some(capture) = &live.capture
                && let Ok(reading) = lock(capture).try_clone()
            {
                let started = std::thread::Builder::new()
                    .name("api-run output".into())
                    .spawn_scoped(scope, move || records(live, reading));
                if let Err(e) = started {
                    let _ = writeln!(io::stderr(), "shards api-run: its members' output: {e}");
                }
            }
            let following = std::thread::Builder::new()
                .name("api-run link".into())
                .spawn_scoped(scope, || {
                    // Its clone taken and the lock let go of before it is read: a match's
                    // guard would be held through `follow`, and an exec's send wait out the run.
                    let link = lock(&live.link).try_clone();
                    let end = match link {
                        Ok(link) => follow(live, &link),
                        Err(e) => RunEnd::Failed(format!("the VM's link: {e}")),
                    };
                    let _ = launched.vm.kill(shards_ipc::signal::SIGKILL);
                    let end = if live.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        RunEnd::Stopped
                    } else {
                        end
                    };
                    if let Ok(payload) = end.encode() {
                        let _ = live.answers.send(wire::to_client::ENDED, &[&payload]);
                    }
                });
            if following.is_err() {
                // Nothing would say its end: the run is ended now, and said so.
                live.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = launched.vm.kill(shards_ipc::signal::SIGKILL);
                lock(&live.held).take();
                if let Ok(payload) = RunEnd::Failed("the run host could not follow its VM".into()).encode() {
                    let _ = live.answers.send(wire::to_client::ENDED, &[&payload]);
                }
            }
            asks(live, input, &launched.vm, scope);
        });
        drop(client);
    }

    /// Reads the VM's link until it closes: each member's end and the command's readiness
    /// to the orchestrator as they come (`super::relayed`), each exec's client end let go of
    /// as the VM takes it, the run's status from its `DONE`.
    fn follow(live: &Live<'_>, link: &Stream) -> RunEnd {
        let mut status = None;
        let failed = loop {
            let m = match shards_ipc::recv(link) {
                Ok(Some(m)) => m,
                Ok(None) => break None,
                Err(e) => break Some(format!("the VM's link: {e}")),
            };
            if m.kind == kind::DONE {
                status = m.payload.first().copied();
            } else if m.kind == kind::EXEC_TAKEN {
                if let Some(number) = m.payload.first_chunk::<8>()
                    && let Some(held) = lock(&live.held).as_mut()
                {
                    held.remove(&u64::from_be_bytes(*number));
                }
            } else if m.kind == kind::DEVICED {
                answered_device(live, &m.payload);
            } else if let Some((answer, payload)) = super::relayed(m.kind, &m.payload) {
                let _ = live.answers.send(answer, &[&payload]);
            }
        };
        // No VM takes an exec now: their clients' ends go, and each waiting for its exit
        // hears its end.
        lock(&live.held).take();
        // Device steps no answer will come for: their askers hear the VM has gone.
        *lock(&live.devices) = None;
        if let Some(why) = failed {
            return RunEnd::Failed(why);
        }
        match status {
            Some(s) => RunEnd::Exited(u32::from(s)),
            None => RunEnd::Failed(
                "the VM ended without its run's end; its last words are on this host's stderr".into(),
            ),
        }
    }

    /// The orchestrator's asks, until its channel closes: credit, stats, stop. A channel
    /// closed without `STOP` is its owner gone, which ends the run as `STOP` does; one
    /// closed after the run's end is its owner having heard it.
    fn asks<'scope, 'env>(
        live: &'env Live<'env>,
        mut input: io::Stdin,
        vm: &'env shards_ipc::Child,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        while let Ok(Some((which, payload))) = wire::read_frame(&mut input) {
            match which {
                wire::to_host::CREDIT => {
                    let Ok(credit) = wire::Credit::decode(&payload) else {
                        break;
                    };
                    let key = (credit.member, credit.channel);
                    let mut flows = lock(&live.flows);
                    let room = flows.room.entry(key).or_insert(0);
                    *room = room.saturating_add(credit.bytes);
                    if live.flush(&mut flows, key).is_err() || live.grant(&mut flows, key).is_err() {
                        break;
                    }
                }
                // On a thread of its own: an exec its guest is slow to answer holds up no
                // credit and no stop.
                wire::to_host::STATS => {
                    let Ok(ask) = wire::StatsAsk::decode(&payload) else {
                        break;
                    };
                    let asked = std::thread::Builder::new()
                        .name("api-run stats".into())
                        .spawn_scoped(scope, move || stats(live, ask));
                    if asked.is_err()
                        && let Ok(answer) = wire::StatsAnswer::encode(ask.id, Err("no thread to ask it on"))
                    {
                        let _ = live.answers.send(wire::to_client::STATS, &[&answer]);
                    }
                }
                // On a thread of its own, as stats: a plug's guest steps hold up nothing else.
                wire::to_host::PLUG | wire::to_host::UNPLUG => {
                    let asked = if which == wire::to_host::PLUG {
                        let Ok(ask) = wire::PlugAsk::decode(&payload) else {
                            break;
                        };
                        let id = ask.id;
                        (
                            id,
                            std::thread::Builder::new()
                                .name("api-run plug".into())
                                .spawn_scoped(scope, move || plugged(live, id, plug(live, &ask))),
                        )
                    } else {
                        let Ok(ask) = wire::UnplugAsk::decode(&payload) else {
                            break;
                        };
                        let id = ask.id;
                        (
                            id,
                            std::thread::Builder::new()
                                .name("api-run unplug".into())
                                .spawn_scoped(scope, move || {
                                    let (result, share) = unplug(live, &ask);
                                    plugged(live, id, result);
                                    // Its share process ends as its device's stream closes.
                                    if let Some(share) = share {
                                        share.end();
                                    }
                                }),
                        )
                    };
                    if let (id, Err(_)) = asked {
                        plugged(
                            live,
                            id,
                            Err((wire::PlugRefusal::Device, "no thread to ask it on".into())),
                        );
                    }
                }
                _ => break,
            }
        }
        live.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = vm.kill(shards_ipc::signal::SIGKILL);
    }

    /// Reads the run's managed output until it ends: each member's bytes to the
    /// orchestrator within its room, its channel's end, each record's credit renewed.
    fn records(live: &Live<'_>, mut capture: Stream) {
        loop {
            let mut header = [0u8; protocol::HEADER];
            if capture.read_exact(&mut header).is_err() {
                return;
            }
            let Some((which, len)) = protocol::parse_header(header) else {
                return;
            };
            if which != protocol::kind::MANAGED {
                return;
            }
            let mut record = Vec::new();
            if record.try_reserve_exact(len as usize).is_err() {
                return;
            }
            record.resize(len as usize, 0);
            if capture.read_exact(&mut record).is_err() {
                return;
            }
            let Ok(parsed) = managed::Record::decode(&record) else {
                return;
            };
            let key = (parsed.member, channel_of(parsed.channel));
            let mut flows = lock(&live.flows);
            flows.granted.insert(key, false);
            let sent = match parsed.event {
                managed::Event::Open(_) => Ok(()),
                managed::Event::Data(bytes) => {
                    flows.waiting.entry(key).or_default().extend_from_slice(bytes);
                    live.flush(&mut flows, key)
                }
                managed::Event::End | managed::Event::Aborted(_) => {
                    flows.ended.insert(key, true);
                    // What waits of it goes first, as its room allows; the rest with its end.
                    let rest = flows.waiting.remove(&key).unwrap_or_default();
                    let mut sent = Ok(());
                    if !rest.is_empty() {
                        sent = live.answers.send(
                            wire::to_client::OUTPUT,
                            &[&wire::Output::head(key.0, key.1), &rest],
                        );
                    }
                    let end = wire::OutputEnd {
                        member: key.0,
                        channel: key.1,
                        end: match parsed.event {
                            managed::Event::Aborted(errno) => wire::StreamEnd::Aborted(errno),
                            _ => wire::StreamEnd::Closed,
                        },
                    };
                    sent.and_then(|()| live.answers.send(wire::to_client::OUTPUT_END, &[&end.encode()]))
                }
            };
            if sent.is_err() || live.grant(&mut flows, key).is_err() {
                return;
            }
        }
    }

    /// Answers `ask` with init's `STATS` of its scope, through an exec on the VM's link as
    /// a health check's exec is sent (daemon/health.rs `send_exec`), on a thread of its own.
    fn stats(live: &Live<'_>, ask: wire::StatsAsk) {
        let answer = |result: Result<&[u8], &str>| {
            if let Ok(payload) = wire::StatsAnswer::encode(ask.id, result) {
                let _ = live.answers.send(wire::to_client::STATS, &[&payload]);
            }
        };
        let args: Vec<Vec<u8>> = match ask.scope {
            wire::StatsScope::Command => Vec::new(),
            wire::StatsScope::Run => vec![protocol::builtin::stats::RUN.to_vec()],
            wire::StatsScope::Member(m) => vec![
                protocol::builtin::stats::MEMBER.to_vec(),
                m.to_string().into_bytes(),
            ],
        };
        match builtin(live, protocol::builtin::STATS, args) {
            Ok(Said {
                status: Some(0), out, ..
            }) => answer(Ok(&out)),
            Ok(Said { err, .. }) => answer(Err(String::from_utf8_lossy(&err).trim_end())),
            Err(e) => answer(Err(&e)),
        }
    }

    /// What a built-in of init's said: its exit status, its answer, and why it failed.
    struct Said {
        status: Option<u8>,
        out: Vec<u8>,
        err: Vec<u8>,
    }

    /// Runs init's built-in `which` with `args` through an exec on the VM's link: what it
    /// said, each of its streams read whole within the run protocol's bound (init's answers
    /// are a few lines).
    fn builtin(live: &Live<'_>, which: u8, args: Vec<Vec<u8>>) -> Result<Said, String> {
        let spec = protocol::Spec {
            argv: args,
            builtin: which,
            ..Default::default()
        };
        let (client, mut out, mut err) =
            (|| -> Result<(Stream, std::io::PipeReader, std::io::PipeReader), String> {
                let (client, theirs) = Stream::pair().map_err(|e| e.to_string())?;
                let (out_r, out_w) = std::io::pipe().map_err(|e| e.to_string())?;
                let (err_r, err_w) = std::io::pipe().map_err(|e| e.to_string())?;
                let null = std::fs::File::open(crate::os::NULL).map_err(|e| e.to_string())?;
                let number = live.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut payload = number.to_be_bytes().to_vec();
                payload.push(0);
                spec.encode_into(&mut payload);
                let handed = shards_ipc::handed(&theirs).map_err(|e| e.to_string())?;
                let fds = [handed.as_fd(), null.as_fd(), out_w.as_fd(), err_w.as_fd()];
                // Held until the VM says it has it; none once its link has closed.
                match lock(&live.held).as_mut() {
                    Some(held) => {
                        held.insert(number, theirs.try_clone().map_err(|e| e.to_string())?);
                    }
                    None => return Err("the run has ended".into()),
                }
                if let Err(e) = shards_ipc::send(&lock(&live.link), kind::EXEC_RUN, &payload, &fds) {
                    if let Some(held) = lock(&live.held).as_mut() {
                        held.remove(&number);
                    }
                    return Err(e.to_string());
                }
                Ok((client, out_r, err_r))
            })()?;
        let bound = u64::from(protocol::MAX_PAYLOAD);
        let mut said = Vec::new();
        let mut why = Vec::new();
        let _ = (&mut out).take(bound).read_to_end(&mut said);
        let _ = (&mut err).take(bound).read_to_end(&mut why);
        let status = match shards_ipc::recv(&client) {
            Ok(Some(m)) if m.kind == kind::EXIT => m.payload.first().copied(),
            _ => None,
        };
        Ok(Said {
            status,
            out: said,
            err: why,
        })
    }

    /// A plug's or an unplug's outcome, told its orchestrator (`PLUGGED`).
    fn plugged(live: &Live<'_>, id: u32, result: Result<(), (wire::PlugRefusal, String)>) {
        if let Ok(payload) = (wire::PlugAnswer { id, result }).encode() {
            let _ = live.answers.send(wire::to_client::PLUGGED, &[&payload]);
        }
    }

    /// The run's plug roots, as a refusal names them.
    fn named(roots: &[Vec<u8>]) -> String {
        let all: Vec<String> = roots
            .iter()
            .map(|r| format!("/{}", String::from_utf8_lossy(r)))
            .collect();
        if all.is_empty() {
            "the run has no plug roots".into()
        } else {
            format!("the run's plug roots are {}", all.join(", "))
        }
    }

    /// `ask`'s host directory plugged into its member (D235), as `shards plug volume --for`
    /// plugs one into a daemon's run (daemon/devices.rs): checked here, and by its guest's
    /// init again; served by a share process of its own; plugged into the microVM's bus;
    /// then mounted in the member at its path by init, the host's files the member's.
    fn plug(live: &Live<'_>, ask: &wire::PlugAsk) -> Result<(), (wire::PlugRefusal, String)> {
        use wire::PlugRefusal as R;
        let member = live
            .members
            .iter()
            .find(|m| m.index == ask.member)
            .ok_or_else(|| {
                (
                    R::Request,
                    format!("member {}: the image declares none", ask.member),
                )
            })?;
        if !member.runs {
            return Err((
                R::Member,
                format!("{}: files alone, it runs nothing", member.name),
            ));
        }
        if !Path::new(&ask.host).is_absolute() {
            return Err((R::Request, format!("{}: not an absolute path", ask.host)));
        }
        let roots: Vec<&[u8]> = live.roots.iter().map(Vec::as_slice).collect();
        match protocol::plug::beneath(ask.at.as_bytes(), &roots) {
            Ok(_) => {}
            Err(protocol::plug::Outside::Rootless) => {
                return Err((
                    R::Outside,
                    format!("{}: not beneath a plug root: {}", ask.at, named(live.roots)),
                ));
            }
            Err(_) => return Err((R::Request, format!("{}: not a plain absolute path", ask.at))),
        }
        if lock(&live.plugged).contains_key(&ask.at) {
            return Err((R::Taken, format!("{} has something plugged in already", ask.at)));
        }
        let dir = shards_vmm::platform::open_dir(Path::new(&ask.host))
            .map_err(|e| (R::Host, format!("{}: {e}", ask.host)))?;
        if !dir
            .metadata()
            .map_err(|e| (R::Host, format!("{}: {e}", ask.host)))?
            .is_dir()
        {
            return Err((R::Host, format!("{}: not a directory", ask.host)));
        }
        let dirs = [(OwnedFd::from(dir), ask.read_only, OsString::new())];
        #[cfg(unix)]
        let share = start_shares(live.home, &dirs).map_err(|e| (R::Host, e))?;
        #[cfg(windows)]
        let share = start_shares(live.home, &dirs, live.job).map_err(|e| (R::Host, e))?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = format!(
            "shards-plug{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let step = [&[shards_ipc::device::SHARE][..], tag.as_bytes()].concat();
        // On Unix its connection, which the share process sends and holds until it is
        // taken (M24); on Windows its stream itself, a duplex pipe made here (D197).
        #[cfg(unix)]
        let answer = match shards_ipc::recv(&share.door) {
            Ok(Some(ends)) if ends.kind == kind::SHARE_ENDS => match ends.fds.first() {
                Some(conn) => {
                    let answer = ask_device(live, &step, &[conn.as_fd()]);
                    let _ = shards_ipc::send(&share.door, kind::TAKEN, &[], &[]);
                    answer
                }
                None => Err("its share process sent no connection".into()),
            },
            Ok(_) => Err("its share process sent no connection".into()),
            Err(e) => Err(format!("its share process: {e}")),
        };
        #[cfg(windows)]
        let answer = match share.ends.first() {
            Some(end) => ask_device(live, &step, &[end.as_fd()]),
            None => Err("no stream was made for its share".into()),
        };
        let placed = answer.and_then(|a| {
            a.split_once(' ')
                .and_then(|(d, addr)| Some((d.parse::<u8>().ok()?, addr.to_string())))
                .ok_or_else(|| format!("its microVM answered {a:?}"))
        });
        let (devfn, address) = match placed {
            Ok(placed) => placed,
            Err(e) => {
                share.end();
                return Err((R::Device, e));
            }
        };
        let flag = if ask.read_only { "ro" } else { "rw" };
        let index = ask.member.to_string();
        if let Err((_, e)) = guest_device(live, &["share", &address, &tag, &ask.at, flag, "member", &index]) {
            // Taken off again, its guest having found it or not.
            let _ = guest_device(live, &["unmount", &address, &ask.at, "member", &index]);
            let _ = ask_device(live, &[shards_ipc::device::UNPLUG, devfn], &[]);
            share.end();
            return Err((R::Device, e));
        }
        lock(&live.plugged).insert(
            ask.at.clone(),
            Plugged {
                devfn,
                address,
                member: ask.member,
                share,
            },
        );
        Ok(())
    }

    /// What is plugged in at `ask`'s path let go of (D235): unmounted in its member by init,
    /// refused while anything there uses it, then taken off the microVM's bus; its share
    /// process given back, to be reaped once its stream has closed.
    fn unplug(
        live: &Live<'_>,
        ask: &wire::UnplugAsk,
    ) -> (Result<(), (wire::PlugRefusal, String)>, Option<Shares>) {
        use wire::PlugRefusal as R;
        let Some((devfn, address, member)) = lock(&live.plugged)
            .get(&ask.at)
            .map(|p| (p.devfn, p.address.clone(), p.member))
        else {
            return (
                Err((R::Unknown, format!("nothing is plugged in at {}", ask.at))),
                None,
            );
        };
        let index = member.to_string();
        if let Err((busy, e)) = guest_device(live, &["unmount", &address, &ask.at, "member", &index]) {
            return (Err((if busy { R::Busy } else { R::Device }, e)), None);
        }
        if let Err(e) = ask_device(live, &[shards_ipc::device::UNPLUG, devfn], &[]) {
            return (Err((R::Device, e)), None);
        }
        (Ok(()), lock(&live.plugged).remove(&ask.at).map(|p| p.share))
    }

    /// Runs init's `DEVICE` built-in with `args` in the guest (init hotplug.rs): its answer,
    /// or why it failed, and whether for something there in use (`DEVICE_BUSY`).
    fn guest_device(live: &Live<'_>, args: &[&str]) -> Result<String, (bool, String)> {
        let argv = args.iter().map(|a| a.as_bytes().to_vec()).collect();
        match builtin(live, protocol::builtin::DEVICE, argv) {
            Ok(Said {
                status: Some(0), out, ..
            }) => Ok(String::from_utf8_lossy(&out).trim().to_string()),
            Ok(Said { status, err, .. }) => Err((
                status == Some(protocol::builtin::DEVICE_BUSY),
                format!("its guest: {}", String::from_utf8_lossy(&err).trim()),
            )),
            Err(e) => Err((false, format!("its guest: {e}"))),
        }
    }

    /// Asks the VM device step `step` (`DEVICE`), with `fds`, and waits for its answer
    /// (`DEVICED`), as the daemon asks one (daemon/devices.rs `ask_device`).
    fn ask_device(live: &Live<'_>, step: &[u8], fds: &[BorrowedFd<'_>]) -> Result<String, String> {
        let number = live.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = std::sync::mpsc::channel();
        match lock(&live.devices).as_mut() {
            Some(asked) => asked.push((number, told)),
            None => return Err("its microVM stopped".into()),
        }
        let ask = [&number.to_be_bytes()[..], step].concat();
        if let Err(e) = shards_ipc::send(&lock(&live.link), kind::DEVICE, &ask, fds) {
            if let Some(asked) = lock(&live.devices).as_mut() {
                asked.retain(|(n, _)| *n != number);
            }
            return Err(format!("asking its microVM: {e}"));
        }
        answer
            .recv()
            .unwrap_or_else(|_| Err("its microVM stopped".into()))
            .map_err(|e| format!("its microVM: {e}"))
    }

    /// The VM's answer to the device step its payload numbers (`DEVICED`), to its asker.
    fn answered_device(live: &Live<'_>, payload: &[u8]) {
        let Some((number, said)) = payload.split_first_chunk::<8>() else {
            return;
        };
        let number = u64::from_be_bytes(*number);
        let mut devices = lock(&live.devices);
        let Some(asked) = devices.as_mut() else {
            return;
        };
        if let Some(i) = asked.iter().position(|(n, _)| *n == number) {
            let (_, told) = asked.swap_remove(i);
            let text = String::from_utf8_lossy(said.get(1..).unwrap_or_default()).into_owned();
            let _ = told.send(match said.first() {
                Some(0) => Ok(text),
                _ => Err(text),
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn members() -> Vec<wire::MemberInfo> {
        members_of(&["agent helper".into(), "harness codex".into()], &[1])
    }

    /// Labels become members by their declared index; only an active one runs.
    #[test]
    fn members_are_the_images_labels() {
        let m = members();
        assert_eq!(m.len(), 2);
        assert_eq!(
            (m[0].index, m[0].kind, m[0].name.as_str(), m[0].runs),
            (0, wire::MemberKind::Agent, "helper", false)
        );
        assert_eq!(
            (m[1].index, m[1].kind, m[1].name.as_str(), m[1].runs),
            (1, wire::MemberKind::Harness, "codex", true)
        );
    }

    fn request(members: Vec<wire::MemberStage>) -> wire::Request {
        wire::Request {
            image: "i".into(),
            vcpus: 0,
            memory_mib: 0,
            command: None,
            members,
            publish: Vec::new(),
            egress: wire::Narrowing::default(),
            shares: Vec::new(),
            nesting: false,
            ready_by: None,
        }
    }

    fn stage(kind: wire::MemberKind, name: &str) -> wire::MemberStage {
        wire::MemberStage {
            kind,
            name: name.into(),
            env: vec![wire::Secret::from("K=v")],
            files: vec![wire::StagedFile {
                path: "f".into(),
                mode: 0o600,
                bytes: wire::Secret::from("x"),
            }],
        }
    }

    /// Staging goes to the member named, by its index; a member the image does not declare,
    /// or one of files alone, is refused by name, nothing staged.
    #[test]
    fn staging_names_a_running_member() {
        let entries = staged(
            &request(vec![stage(wire::MemberKind::Harness, "codex")]),
            &members(),
        )
        .ok()
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].starts_with(b"member-env=1\0K=v"));
        for (kind, name, cause) in [
            (wire::MemberKind::Agent, "codex", Cause::Image),
            (wire::MemberKind::Harness, "nobody", Cause::Image),
            (wire::MemberKind::Agent, "helper", Cause::Image),
        ] {
            match staged(&request(vec![stage(kind, name)]), &members()) {
                Err(No(c, _)) => assert_eq!(c, cause),
                Ok(_) => panic!("{kind:?} {name} staged"),
            }
        }
    }

    /// The image's grants are kept, narrowed or dropped by the orchestrator; any it names
    /// that the image does not grant is refused as a widening; nothing left, no policy.
    #[test]
    fn grants_narrow_and_never_widen() {
        let grants = shards_agentfile::agentfile::Grants {
            egress: vec![b"443/tcp".to_vec(), b"53/udp".to_vec()],
            mcp: vec![b"mcp.example:443".to_vec()],
            destinations: vec![
                b"api.openai.com:443".to_vec(),
                b"host.docker.internal:7070".to_vec(),
            ],
            dns: true,
            egress_declared: Vec::new(),
            nesting: false,
        };
        let all = egress(Some(&grants), &wire::Narrowing::default()).ok().unwrap();
        assert_eq!(
            shards_net::decode_policy(all.as_deref().unwrap()).unwrap().1,
            [
                ("mcp.example".to_string(), 443),
                ("api.openai.com".to_string(), 443),
                ("host.docker.internal".to_string(), 7070)
            ],
            "each server and destination by name"
        );
        let narrowed = wire::Narrowing {
            egress: wire::Keep::Only(vec!["443/tcp".into()]),
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(vec!["host.docker.internal:7070".into()]),
            dns: false,
        };
        let one = egress(Some(&grants), &narrowed).ok().unwrap().unwrap();
        assert_ne!(Some(one.clone()), all);
        assert_eq!(
            shards_net::decode_policy(&one).unwrap().1,
            [("host.docker.internal".to_string(), 7070)]
        );
        let none = wire::Narrowing {
            egress: wire::Keep::Only(Vec::new()),
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(Vec::new()),
            dns: false,
        };
        assert_eq!(egress(Some(&grants), &none).ok().unwrap(), None);
        // A destination the image does not grant, of another port or host: a widening.
        for asked in ["api.openai.com:80", "chatgpt.com:443"] {
            let wider = wire::Narrowing {
                destinations: wire::Keep::Only(vec![asked.into()]),
                ..wire::Narrowing::default()
            };
            assert!(
                matches!(egress(Some(&grants), &wider), Err(No(Cause::Widening, _))),
                "{asked}"
            );
        }
        let wider = wire::Narrowing {
            egress: wire::Keep::Only(vec!["22/tcp".into()]),
            ..wire::Narrowing::default()
        };
        assert!(matches!(
            egress(Some(&grants), &wider),
            Err(No(Cause::Widening, _))
        ));
        // An image with no grants: DNS asked for is no DNS, never a grant.
        assert_eq!(egress(None, &wire::Narrowing::default()).ok().unwrap(), None);
        let asked = wire::Narrowing {
            mcp: wire::Keep::Only(vec!["mcp.example:443".into()]),
            ..wire::Narrowing::default()
        };
        assert!(matches!(egress(None, &asked), Err(No(Cause::Widening, _))));
    }

    /// The run's request is one a warm VM takes: its log keeps something by the VM's own
    /// check (warm.rs `head`), though it writes none, and its head says whether it
    /// publishes ports.
    #[test]
    fn its_request_is_one_a_warm_vm_takes() {
        for published in [false, true] {
            let payload = run_request(&shards_abi::run::Spec::default(), published);
            let (head, spec) = shards_ipc::RunHead::decode(&payload).unwrap();
            assert!(head.keeps_its_log(), "{head:?}");
            assert_eq!(head.flags & shards_ipc::RUN_PUBLISHED != 0, published);
            assert_eq!(head.flags & shards_ipc::RUN_LOG, 0);
            assert!(shards_abi::run::Spec::decode(spec).is_some());
        }
    }

    /// With no command of its own, a run publishes only ports the image gives a member
    /// coming in.
    #[test]
    fn ports_need_a_receiver_without_a_command() {
        let p = |port| wire::Publish {
            guest_port: port,
            proto: wire::Proto::Tcp,
            host: None,
            host_port: 0,
        };
        let agentfile = crate::agentfile::Agentfile {
            receivers: vec![shards_agentfile::agentfile::Receiver {
                at: (6, 8000, 8010),
                to: (6, 8000, 8010),
                kind: shards_agentfile::agentfile::TargetKind::Harness,
                name: b"codex".to_vec(),
            }],
            ..Default::default()
        };
        assert!(ports_granted(&[p(8005)], Some(&agentfile), false).is_ok());
        assert!(matches!(
            ports_granted(&[p(9000)], Some(&agentfile), false),
            Err(No(Cause::Widening, _))
        ));
        assert!(ports_granted(&[p(9000)], Some(&agentfile), true).is_ok());
        let udp = wire::Publish {
            proto: wire::Proto::Udp,
            ..p(8005)
        };
        assert!(matches!(
            ports_granted(&[udp], Some(&agentfile), false),
            Err(No(Cause::Widening, _))
        ));
    }

    /// Each published port names whom it reaches (D122): the member the image gives it, at
    /// its own port where an EXPOSE maps it; several alike where several claim it; none
    /// where it reaches the run's own command, another protocol's port included; and a
    /// receiver the image declares no member as is refused, never read as the command's.
    #[test]
    fn published_ports_name_whom_they_reach() {
        use shards_agentfile::agentfile::{Receiver, TargetKind};
        let at = |a: &str| a.parse::<std::net::SocketAddr>().unwrap();
        let receiver = |at: (u8, u16, u16), to: (u8, u16, u16), kind, name: &str| Receiver {
            at,
            to,
            kind,
            name: name.as_bytes().to_vec(),
        };
        let receivers = vec![
            receiver((6, 8081, 8081), (6, 80, 80), TargetKind::Harness, "codex"),
            receiver((6, 7000, 7010), (6, 7000, 7010), TargetKind::Agent, "helper"),
            receiver((6, 7000, 7010), (6, 7000, 7010), TargetKind::Harness, "codex"),
        ];
        let bound = published(
            &[
                (8081, 6, at("127.0.0.1:40001")),
                (7005, 6, at("[::1]:40002")),
                (9000, 6, at("127.0.0.1:40003")),
                (8081, 17, at("127.0.0.1:40004")),
            ],
            &receivers,
            &members(),
        )
        .ok()
        .unwrap();
        let to = |member, port| wire::Receiving { member, port };
        assert_eq!(bound[0].to, vec![to(1, 80)]);
        assert_eq!(
            (bound[0].guest_port, bound[0].host),
            (8081, at("127.0.0.1:40001"))
        );
        assert_eq!(bound[1].to, vec![to(0, 7005), to(1, 7005)]);
        assert!(bound[2].to.is_empty(), "the run's own command's");
        assert_eq!((bound[3].proto, bound[3].to.len()), (wire::Proto::Udp, 0));
        let stranger = vec![receiver((6, 1, 1), (6, 1, 1), TargetKind::Agent, "nobody")];
        assert!(matches!(
            published(&[(1, 6, at("127.0.0.1:1"))], &stranger, &members()),
            Err(No(Cause::Image, _))
        ));
        // An agent and a harness of one name are told apart.
        let agent_codex = vec![receiver((6, 1, 1), (6, 1, 1), TargetKind::Agent, "codex")];
        assert!(published(&[(1, 6, at("127.0.0.1:1"))], &agent_codex, &members()).is_err());
    }

    /// The nesting grant's deadline (D234) is the budget past this host's clock as the
    /// request arrives, in nanoseconds since the Unix epoch: none for none, the clock's own
    /// instant for none of budget (already past as init reads it, which it takes as passed
    /// at once); refused, typed, where the instant is past what a u64 of nanoseconds
    /// counts, and where the clock reads before the epoch or past that count.
    #[test]
    fn a_deadline_is_its_budget_past_the_clock_as_the_request_arrives() {
        use std::time::Duration;
        let at = |ns: u64| UNIX_EPOCH + Duration::from_nanos(ns);
        let now = at(1_794_466_320_362_000_000);
        assert_eq!(deadline(None, now).ok(), Some(None));
        assert_eq!(
            deadline(None, UNIX_EPOCH - Duration::from_secs(1)).ok(),
            Some(None)
        );
        assert_eq!(deadline(Some(0), now).ok(), Some(Some(1_794_466_320_362_000_000)));
        assert_eq!(
            deadline(Some(5_000_000_007), now).ok(),
            Some(Some(1_794_466_325_362_000_007))
        );
        assert_eq!(deadline(Some(7), UNIX_EPOCH).ok(), Some(Some(7)));
        assert_eq!(
            deadline(Some(u64::MAX - 1_794_466_320_362_000_000), now).ok(),
            Some(Some(u64::MAX))
        );
        for budget in [u64::MAX - 1_794_466_320_361_999_999, u64::MAX] {
            match deadline(Some(budget), now) {
                Err(No(Cause::Request, why)) => assert!(why.contains("past what a u64"), "{why}"),
                Err(No(cause, why)) => panic!("{cause:?}: {why}"),
                Ok(at) => panic!("{budget}: {at:?}"),
            }
        }
        // A second either way, past any clock's resolution (Windows' is 100 ns).
        for clock in [
            UNIX_EPOCH - Duration::from_secs(1),
            at(u64::MAX) + Duration::from_secs(1),
        ] {
            assert!(
                matches!(deadline(Some(0), clock), Err(No(Cause::Boot, _))),
                "{clock:?}"
            );
        }
    }

    /// The nesting grant (D234) is asked of the daemon's own decision as `--security-opt
    /// nesting=true` asks for it from the command line, the deadline with it: given where
    /// the image declares it, with the deadline; asked of an image that declares none,
    /// refused as a widening in the daemon's words; asked by an image's run that does not
    /// ask for it, withheld and said; and a deadline without it refused as the daemon
    /// refuses it, the request's.
    #[test]
    fn the_nesting_grant_is_asked_and_decided_as_the_command_line_asks_it() {
        use crate::agentfile::{Agentfile, NESTING_UNASKED, Nesting};
        use shards_abi::run::nesting::{self as abi, Grant};
        let declaring = Agentfile {
            grants: shards_agentfile::agentfile::Grants {
                nesting: true,
                ..Default::default()
            },
            ..Agentfile::default()
        };
        let said = Mutex::new(Vec::new());
        let say = |line: &str| said.lock().unwrap().push(line.to_string());
        let asking = wire::Request {
            nesting: true,
            command: Some(vec!["node".into()]),
            ..request(Vec::new())
        };
        let run = hosted::as_run(&asking, Some(7));
        assert_eq!(run.security_opt, ["nesting=true"]);
        assert_eq!(run.ready_by, Some(7));
        assert_eq!(
            nesting(Some(&declaring), &run, &say).ok(),
            Some(Nesting::Granted(Grant {
                ready_by: Some(7),
                gpus: Vec::new()
            }))
        );
        assert_eq!(
            nesting(Some(&declaring), &hosted::as_run(&asking, None), &say).ok(),
            Some(Nesting::Granted(Grant::default()))
        );
        for image in [None, Some(&Agentfile::default())] {
            match nesting(image, &run, &say) {
                Err(No(Cause::Widening, why)) => assert_eq!(why, abi::undeclared()),
                Err(No(cause, why)) => panic!("{cause:?}: {why}"),
                Ok(n) => panic!("{n:?}"),
            }
        }
        assert!(said.lock().unwrap().is_empty());
        let plain = hosted::as_run(&request(Vec::new()), None);
        assert!(plain.security_opt.is_empty());
        assert_eq!(plain.ready_by, None);
        assert_eq!(
            nesting(Some(&declaring), &plain, &say).ok(),
            Some(Nesting::Unasked)
        );
        assert_eq!(*said.lock().unwrap(), [format!("WARNING: {NESTING_UNASKED}")]);
        assert_eq!(nesting(None, &plain, &say).ok(), Some(Nesting::Neither));
        let late = hosted::as_run(&request(Vec::new()), Some(7));
        for image in [Some(&declaring), None] {
            match nesting(image, &late, &say) {
                Err(No(Cause::Request, why)) => {
                    assert_eq!(why, abi::Error::Ungranted("ready-by=").to_string());
                }
                Err(No(cause, why)) => panic!("{cause:?}: {why}"),
                Ok(n) => panic!("{n:?}"),
            }
        }
    }

    /// A crafted image, declaring NESTING beside an agent where no build would (D234), is
    /// refused as its Agentfile is read, before any grant is decided: the refusal
    /// `run::prepare` gives and this host sends as the image's, word for word the daemon's,
    /// in the build's words.
    #[test]
    fn a_crafted_image_is_refused_in_its_builds_words_before_its_grant_is_decided() {
        use shards_agentfile::agentfile::{Directive, Domain, Processes, TargetKind};
        use shards_image::erofs::{DataRef, Kind, Meta, Node, Source, Tree};
        struct Mem(Vec<u8>);
        impl Source for Mem {
            fn read_at(&mut self, _: DataRef, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
                let at = usize::try_from(at).unwrap();
                buf.copy_from_slice(&self.0[at..at + buf.len()]);
                Ok(())
            }
        }
        let crafted = shards_agentfile::agentfile::spec(&[
            Directive::Agent(Domain {
                name: b"a".to_vec(),
                source: b"./a".to_vec(),
                to: None,
                processes: Processes::Unbounded,
            }),
            Directive::Nesting,
        ]);
        let digest = String::from_utf8(shards_agentfile::agentfile::digest(&crafted)).unwrap();
        let meta = |mode| Meta {
            mode,
            ..Meta::default()
        };
        let mut tree = Tree::new(meta(0o755));
        tree.insert(
            Tree::ROOT,
            b".agentfile.json",
            Node {
                kind: Kind::File {
                    size: crafted.len() as u64,
                    data: DataRef { source: 0, offset: 0 },
                },
                meta: meta(0o444),
            },
        )
        .unwrap();
        let dir = shards_testdir::TempDir::new("api-run-crafted").unwrap();
        let root = dir.join("root.erofs");
        let mut out = std::fs::File::create(&root).unwrap();
        shards_image::erofs::write(&tree, &mut Mem(crafted), &mut out).unwrap();
        drop(out);
        let why = crate::agentfile::agentfile(&root, &digest).unwrap_err();
        assert_eq!(
            why,
            format!(
                "the image's Agentfile ({digest}): {}",
                shards_agentfile::agentfile::nesting_beside(TargetKind::Agent, b"a")
            )
        );
        let refused = wire::Refused {
            cause: Cause::Image,
            why: why.clone(),
        };
        assert_eq!(
            wire::Refused::decode(&refused.encode().unwrap()).unwrap().why,
            why
        );
    }

    /// The VM's link reaches the orchestrator as the guest said it: each readiness of the
    /// command's under the nesting grant (D234) as READY, its nine bytes whole; a member's
    /// end as MEMBER_ENDED; a readiness that is none, and every other message, not at all.
    #[test]
    fn the_vms_link_is_relayed_to_the_orchestrator_as_its_guest_said_it() {
        for ready in [
            wire::Ready::OnTime,
            wire::Ready::Late(u64::MAX),
            wire::Ready::Passed(3),
        ] {
            let said = ready.encode();
            let (answer, payload) = relayed(kind::WORKLOAD_READY, &said).unwrap();
            assert_eq!(answer, wire::to_client::READY);
            assert_eq!(payload, said);
            assert_eq!(wire::ready(&payload), Ok(ready));
        }
        for malformed in [&[0u8; 9][..], &[1, 0, 0, 0, 0, 0, 0, 0, 1], &[2; 8], &[]] {
            assert_eq!(relayed(kind::WORKLOAD_READY, malformed), None, "{malformed:?}");
        }
        let ended = shards_abi::run::members::Ended {
            member: 1,
            status: 137,
        }
        .encode();
        assert_eq!(
            relayed(kind::MEMBER_ENDED, &ended),
            Some((
                wire::to_client::MEMBER_ENDED,
                wire::MemberEnded {
                    member: 1,
                    status: 137
                }
                .encode()
            ))
        );
        for other in [kind::READY, kind::PROCESS, kind::DONE] {
            assert_eq!(relayed(other, &[1; 9]), None);
        }
    }
}
