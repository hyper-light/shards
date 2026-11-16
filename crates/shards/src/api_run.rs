//! `shards api-run`: an image's run for an orchestrator, hosted by this process (D231).
//! shards-api's image boot starts one for each microVM and speaks to it over its stdin and
//! stdout (`shards_api::image::wire`): a request in; the run's start, each member's output
//! and end, stats and the run's end out. It is to its run what the daemon is to each of its
//! runs, and nothing more: it prepares the run with the daemon's own functions (`run`,
//! `volumes`, `setup`, `agentfile`, `daemon::publish`, `netproc`), starts the run's network,
//! share and VM processes, its VM restoring its image's template where one is saved, the daemon's
//! runs' shape (D253), plays the daemon's side of the VM's link (warm.rs) for that one
//! run, and owns its managed output (P3). Its children go with it: the VM is told at
//! `TAKEN` that its run is killed if its link closes (`STOP_ORDER`), the network process
//! goes as the VM's doorbell end does, the share process as its streams close; on Windows
//! each is made in a job whose last handle is this process's, which ends them as it ends,
//! however it ends. And the run goes with its owner: its stdin closing kills the VM.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::num::NonZeroU64;
use std::process::ExitCode;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shards_api::image::wire::{self, Cause};
use shards_ipc::kind;
use shards_net::lease::Refusal;
use shards_net::signin::{self, Declined};

/// Why the run will not be, for the orchestrator.
#[derive(Debug)]
struct No(Cause, String);

fn no(cause: Cause) -> impl Fn(String) -> No {
    move |why| No(cause, why)
}

/// The answers' channel, this process's stdout: one frame at a time, whichever thread
/// sends it.
struct Answers(Mutex<Box<dyn io::Write + Send>>);

impl Answers {
    /// Frames `parts` as one payload of `kind`, written in one turn, as they lie: no copy of a
    /// part is made here, so a sign-in's tokens (D242, step 8) are in no buffer but their
    /// sender's, which wipes them (this process's stdout is its own descriptor, unbuffered:
    /// [`stdout`]).
    fn send(&self, kind: u8, parts: &[&[u8]]) -> io::Result<()> {
        let len = parts
            .iter()
            .try_fold(0usize, |n, p| n.checked_add(p.len()))
            .ok_or(io::ErrorKind::InvalidInput)?;
        let header = wire::header(kind, len).map_err(io::Error::other)?;
        let mut slices: Vec<io::IoSlice<'_>> = Vec::with_capacity(parts.len().saturating_add(1));
        slices.push(io::IoSlice::new(&header));
        slices.extend(parts.iter().map(|p| io::IoSlice::new(p)));
        let mut rest = slices.as_mut_slice();
        let mut out = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        while !rest.is_empty() {
            match out.write_vectored(rest) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                // At most what `rest` holds is written, so it advances within it.
                Ok(n) => io::IoSlice::advance_slices(&mut rest, n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        out.flush()
    }
}

/// Whether this process's lines say their times (`SHARDS_TIMING`), read once.
fn timed() -> bool {
    static TIMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TIMED.get_or_init(|| std::env::var_os("SHARDS_TIMING").is_some())
}

/// One line of the run's progress on this process's stderr, its diagnostics, which a
/// refusal quotes the last of; with `SHARDS_TIMING`, its time: microseconds since this
/// process's `main` (`shards_vmm::log::uptime_us`), whose wall clock it says once as it
/// starts ([`command`]). Written in one write, so that its VM's lines on the same stderr
/// come between lines, never in one.
fn say(what: std::fmt::Arguments<'_>) {
    let _ = io::stderr().write_all(said(timed().then(shards_vmm::log::uptime_us), what).as_bytes());
}

/// [`say`]'s line, timed `at` where its lines are timed.
fn said(at: Option<u128>, what: std::fmt::Arguments<'_>) -> String {
    match at {
        Some(us) => format!("shards api-run: +{us} us {what}\n"),
        None => format!("shards api-run: {what}\n"),
    }
}

/// A step of the run's start, said only where its lines are timed (`SHARDS_TIMING`).
fn step(what: std::fmt::Arguments<'_>) {
    if timed() {
        say(what);
    }
}

/// The run's channel from its orchestrator, this process's stdin (D231), read a frame at a
/// time into a buffer of its own (`wire::read_frame_into`), each frame's bytes wiped as it is
/// let go of ([`Frame`]), so that between frames the buffer is all zeros: never through
/// std's `Stdin`, whose buffer reads ahead and keeps what it read (a lease's value, D242; a
/// staged credential; a member's typed input) until later frames overwrite it.
struct Channel<R> {
    from: R,
    buf: Vec<u8>,
}

/// A frame of the [`Channel`]'s: its kind, and its payload in the channel's buffer, wiped
/// there as the frame is let go of.
struct Frame<'a> {
    kind: u8,
    payload: &'a mut [u8],
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut *self.payload);
    }
}

impl std::ops::Deref for Frame<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.payload
    }
}

impl<R: io::Read> Channel<R> {
    fn new(from: R) -> Channel<R> {
        Channel {
            from,
            buf: Vec::new(),
        }
    }

    /// The next frame, none at a clean end before its header; refused past the channel's
    /// bound or cut short.
    fn next(&mut self) -> io::Result<Option<Frame<'_>>> {
        match wire::read_frame_into(&mut self.from, &mut self.buf) {
            Ok(Some((kind, len))) => {
                let payload = self
                    .buf
                    .get_mut(..len)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                Ok(Some(Frame { kind, payload }))
            }
            Ok(None) => Ok(None),
            // What came of a frame cut short is wiped as well.
            Err(e) => {
                zeroize::Zeroize::zeroize(self.buf.as_mut_slice());
                Err(e)
            }
        }
    }

    /// Lets go of its buffer's memory: after the run's request, which may be far the largest
    /// frame it reads.
    fn shed(&mut self) {
        self.buf = Vec::new();
    }
}

/// The run's channel, as this process reads it.
type Input = Channel<std::fs::File>;

/// This process's stdin as the run's [`Channel`] reads it: its own descriptor (on Windows its
/// own handle), read unbuffered.
fn stdin() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        Ok(std::fs::File::from(io::stdin().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle as _;
        Ok(std::fs::File::from(io::stdin().as_handle().try_clone_to_owned()?))
    }
}

/// This process's stdout as its [`Answers`] write it: its own descriptor (on Windows its own
/// handle), written unbuffered, never through std's `Stdout`, whose buffer keeps what went
/// through it (a sign-in's tokens, D242 step 8) until later frames overwrite it.
fn stdout() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        Ok(std::fs::File::from(io::stdout().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle as _;
        Ok(std::fs::File::from(
            io::stdout().as_handle().try_clone_to_owned()?,
        ))
    }
}

/// `shards api-run`: not for people to run. The run's end is said as its VM ends, before
/// its owner closes the channel; a run that never started is refused instead.
pub fn command() -> ExitCode {
    // Its clock on the wall's, said first, for lining its lines up with its caller's and its
    // VM's (api_image's `image_start_costs`); and its launches' steps said with it.
    if timed() {
        crate::launch::say_steps(say);
        if let Some(at) = shards_vmm::log::began_unix_us() {
            say(format_args!("its clock began at unix {at} us"));
        }
    }
    let answers = match stdout() {
        Ok(out) => Answers(Mutex::new(Box::new(out))),
        // No channel to say anything on: its owner reads its end, and this, on its stderr.
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards api-run: its stdout: {e}");
            return ExitCode::FAILURE;
        }
    };
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
    let mut input = Channel::new(stdin().map_err(|e| No(Cause::Boot, format!("its stdin: {e}")))?);
    let (kind, request) = match input.next() {
        Ok(Some(frame)) if frame.kind == wire::to_host::CREATE || frame.kind == wire::to_host::WARM => {
            let request = wire::Request::decode(&frame).map_err(|e| No(Cause::Request, e.to_string()))?;
            (frame.kind, request)
        }
        Ok(Some(frame)) => return Err(before_request(frame.kind)),
        Ok(None) => return Err(No(Cause::Request, "the channel closed before its request".into())),
        Err(e) => return Err(No(Cause::Request, format!("the request: {e}"))),
    };
    step(format_args!("its request read"));
    input.shed();
    // A warm host's exemplar (D253's pools): its run comes once its microVM is ready.
    if kind == wire::to_host::WARM {
        return hosted::warm(answers, &request, input);
    }
    let ready_by = deadline(request.ready_by, SystemTime::now())?;
    hosted::run(answers, &request, ready_by, input)
}

/// Why frame `kind`, come before its run's request, refuses the run: the channel is no run
/// owner's until its request, a warm host's its pool's (D253), so that nothing is served
/// before it, a save above all (D259), whose microVM's memory is its owner's alone.
fn before_request(kind: u8) -> No {
    No(
        Cause::Request,
        format!("message kind {kind} before the run's request"),
    )
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

/// The run's socket grant (D251), held as shards-api holds it, as init holds it again: its
/// directory absolute and plain, a lane and a byte at least (its caller's numbers, R03), and a
/// run's own command's, which a run of its image's members alone has none of.
fn socket_grant(grant: &wire::SocketGrant, command: bool) -> Result<(), No> {
    let refuse = |why: String| Err(No(Cause::Request, format!("its socket grant: {why}")));
    if !shards_abi::run::socket::dir_ok(grant.dir.as_bytes()) {
        return refuse(format!(
            "{:?}: {}",
            grant.dir,
            shards_abi::run::socket::Error::Dir
        ));
    }
    if grant.lanes == 0 || grant.window == 0 {
        return refuse(format!(
            "{} lanes of {} bytes: a grant admits a lane and a byte at least",
            grant.lanes, grant.window
        ));
    }
    if !command {
        return refuse("a run without a command of its own has no sockets to grant".into());
    }
    Ok(())
}

/// The run's microVM, made as every run's is (D253): its config, its guest's (the GPU's where
/// the run is given its GPU, D144) shaped as the daemon shapes its runs' (`run::Shape`) and
/// naming the GPUs it renders on (D232); and its template where it has one, named as the
/// daemon names one (`run::template`), with the guest it is keyed by and recorded with and
/// its root filesystem's stamp. None for a guest its caller gave, as the daemon's runs of one
/// have none, or on a host whose hypervisor restores no snapshot (`restores`: Windows Server
/// 2022's sets no vCPU's APIC page back, PM M345).
struct Microvm {
    cfg: shards_vmm::vm::Config,
    template: Option<Template>,
}

/// A run's template: where it is, the guest it is keyed by, and its root filesystem's stamp
/// as the run found it (`shards_image::store::rootfs_stamp`).
struct Template {
    dir: std::path::PathBuf,
    guest: crate::guest::Guest,
    stamp: Option<String>,
}

fn microvm(
    home: &std::path::Path,
    prepared: &crate::run::Prepared,
    shape: &crate::run::Shape<'_>,
    gpu_devices: &[shards_api::PhysicalGpu],
    restores: bool,
) -> Microvm {
    let gpu = prepared.gpu.as_deref();
    let (mut cfg, guest) = match &prepared.boot {
        crate::run::Boot::Given(cfg) => ((**cfg).clone(), None),
        crate::run::Boot::Stored(guest) => {
            let guest = gpu.map_or_else(|| guest.clone(), |kernel| crate::run::gpu_guest(guest, kernel));
            let cfg = shards_vmm::vm::Config::new(guest.kernel.clone(), Some(guest.init.clone()));
            (cfg, Some(guest))
        }
    };
    shape.give(&mut cfg);
    cfg.gpu = gpu.is_some();
    cfg.gpu_devices = gpu_devices.to_vec();
    let template = guest.filter(|_| restores).map(|guest| {
        let rootfs = &prepared.rootfs;
        let stamp = shards_image::store::rootfs_stamp(rootfs);
        let dir = crate::run::template(home, &guest, rootfs, stamp.as_deref(), &cfg, prepared.layer);
        Template { dir, guest, stamp }
    });
    Microvm { cfg, template }
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
fn relayed(which: u8, payload: &[u8], generation: impl Fn(u32) -> (u32, bool)) -> Option<(u8, Vec<u8>)> {
    match which {
        kind::MEMBER_ENDED => {
            let e = shards_abi::run::members::Ended::decode(payload)?;
            let (generation, kill_sent) = generation(e.member);
            let ended = wire::MemberEnded {
                member: e.member,
                generation,
                end: e.end,
                // Its restart's kill ended it where its end is the kill's, SIGKILL (Linux's 9 on
                // every architecture): one that ended of itself as the kill went is not killed.
                killed: kill_sent && e.end == shards_abi::run::members::End::Signaled(9),
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
    let mut worked = std::collections::BTreeSet::new();
    let mut argued = std::collections::BTreeSet::new();
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
        // Its working directory, the project path it works in: one, as the guest takes one.
        if let Some(w) = &m.workdir {
            if !worked.insert(info.index) {
                return Err(No(
                    Cause::Request,
                    format!("{}: {}", m.name, staging::Error::Twice),
                ));
            }
            entries.push(
                staging::workdir(info.index, w.as_bytes())
                    .map_err(|e| No(Cause::Request, format!("{}: {w:?}: {e}", m.name)))?,
            );
        }
        // Its run-time arguments (D256), after its own command's, in their order: one list, as
        // two would have no order between them.
        if !m.args.is_empty() && !argued.insert(info.index) {
            return Err(No(
                Cause::Request,
                format!("{:?} {}: its arguments given twice", m.kind, m.name),
            ));
        }
        for (i, a) in m.args.iter().enumerate() {
            entries.push(
                staging::arg(info.index, a.as_bytes())
                    .map_err(|e| No(Cause::Request, format!("{}: its argument {i}: {e}", m.name)))?,
            );
        }
    }
    Ok(entries)
}

/// A share `request` gives members as they start (D277): where the guest sees it, and each
/// member it is given to, by declared index, kind and name, ascending by index.
struct Given {
    at: String,
    members: Vec<(u32, wire::MemberKind, String)>,
}

/// Each share `request` gives members as they start (D277): each member one the image declares
/// that runs, or every such member for one given to every member; refused by name otherwise, as
/// a staged member is.
fn share_members(request: &wire::Request, members: &[wire::MemberInfo]) -> Result<Vec<Given>, No> {
    let mut given = Vec::new();
    for share in &request.shares {
        let Some(to) = &share.members else { continue };
        let mut found: Vec<&wire::MemberInfo> = Vec::new();
        match to {
            wire::ShareTo::Every => found.extend(members.iter().filter(|m| m.runs)),
            wire::ShareTo::Members(named) => {
                for n in named {
                    let info = members
                        .iter()
                        .find(|i| i.kind == n.kind && i.name == n.name)
                        .ok_or_else(|| {
                            No(
                                Cause::Image,
                                format!(
                                    "{}: {:?} {}: the image declares no such member",
                                    share.at, n.kind, n.name
                                ),
                            )
                        })?;
                    if !info.runs {
                        return Err(No(
                            Cause::Image,
                            format!(
                                "{}: {:?} {}: files alone, which run nothing to be given a share",
                                share.at, n.kind, n.name
                            ),
                        ));
                    }
                    found.push(info);
                }
            }
        }
        if found.is_empty() {
            return Err(No(
                Cause::Image,
                format!(
                    "{}: given to every member that runs, and none of the image's runs",
                    share.at
                ),
            ));
        }
        found.sort_unstable_by_key(|m| m.index);
        found.dedup_by_key(|m| m.index);
        given.push(Given {
            at: share.at.clone(),
            members: found.iter().map(|m| (m.index, m.kind, m.name.clone())).collect(),
        });
    }
    Ok(given)
}

/// Whether the image's volume `v` gives itself to the member `kind` `name`: every agent and
/// harness where `FOR` names none; else one it names, of the kind `--target-kind` says where it
/// says one.
fn volume_gives(v: &crate::agentfile::AgentVolume, kind: wire::MemberKind, name: &str) -> bool {
    use shards_agentfile::agentfile::TargetKind;
    let of_kind = match v.scope.kind {
        None => true,
        Some(TargetKind::Agent) => kind == wire::MemberKind::Agent,
        Some(TargetKind::Harness) => kind == wire::MemberKind::Harness,
    };
    v.scope.names.is_empty() || (of_kind && v.scope.names.iter().any(|n| n.as_slice() == name.as_bytes()))
}

/// `mounts`' entry of each share given members (D277) told so: its members, each by its
/// declared index (`member=N`), as an image's volume is given its members (`domains`), beside
/// the run's own root as before. Where the image's own `VOLUME` lies there, its Agentfile is
/// the most it is given: a share there may narrow whom it reaches, refused as a widening for a
/// member its `FOR` gives it none. Refused for a file shared alone, which no member stands in a
/// directory for.
fn give_members(
    mounts: &mut [(String, Vec<u8>)],
    given: &[Given],
    volumes: &[crate::agentfile::AgentVolume],
) -> Result<(), No> {
    for g in given {
        let at = &g.at;
        let dest = shards_cmdline::mounts::clean(at);
        let Some((_, entry)) = mounts.iter_mut().find(|(d, _)| *d == dest) else {
            return Err(No(
                Cause::Request,
                format!("{at}: no share there to give its members"),
            ));
        };
        let parts: Vec<&[u8]> = entry.splitn(4, |&b| b == 0).collect();
        let [head, point, flags, only] = parts.as_slice() else {
            return Err(No(
                Cause::Boot,
                format!("{at}: its share's setup entry is not one"),
            ));
        };
        if !only.is_empty() {
            return Err(No(
                Cause::Request,
                format!("{at}: a file shared alone is given to no member; share the directory it lies in"),
            ));
        }
        let image_volume = flags.split(|&b| b == b',').any(|f| f == b"domains");
        if image_volume {
            let volume = volumes
                .iter()
                .find(|v| v.paths.iter().any(|p| shards_cmdline::mounts::clean(p) == dest));
            for (_, kind, name) in &g.members {
                if !volume.is_some_and(|v| volume_gives(v, *kind, name)) {
                    return Err(No(
                        Cause::Widening,
                        format!(
                            "{at}: the image's VOLUME there gives it as its Agentfile says, and {kind:?} {name} none; a share there narrows whom it reaches, never widens"
                        ),
                    ));
                }
            }
        } else if !head.starts_with(b"volume=") {
            return Err(No(
                Cause::Boot,
                format!("{at}: its share's setup entry is not one"),
            ));
        }
        let mut words: Vec<Vec<u8>> = flags
            .split(|&b| b == b',')
            .filter(|f| !f.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        if !image_volume {
            words.push(b"domains".to_vec());
        }
        words.extend(
            g.members
                .iter()
                .map(|(m, _, _)| format!("member={m}").into_bytes()),
        );
        let (nul, joined): (&[u8], Vec<u8>) = (b"\0", words.join(&b','));
        let told = [*head, nul, *point, nul, joined.as_slice(), nul, *only].concat();
        *entry = told;
    }
    Ok(())
}

/// What the orchestrator is told of a member its guest could not start in its working
/// directory (D277; init's `entering its working directory`), `told` as its guest said it:
/// `Workdir`, naming the member as the image declares it, the directory and the errno.
fn workdir_refused(
    members: &[wire::MemberInfo],
    told: &shards_abi::run::members::Unentered,
) -> wire::Refused {
    let who = members.iter().find(|m| m.index == told.member).map_or_else(
        || format!("member {}", told.member),
        |m| format!("{:?} {}", m.kind, m.name),
    );
    wire::Refused {
        cause: Cause::Workdir,
        why: format!(
            "{who}: {}: errno {}: its guest could not enter it as it started",
            String::from_utf8_lossy(&told.workdir),
            told.errno
        ),
    }
}

/// The members `request` asks `asked` of, by their declared index, ascending: each one of the
/// image's that runs a program, each asked once, refused by name otherwise; `purpose` what one
/// of files alone runs nothing for, `what` what is asked.
fn asked_members(
    request: &wire::Request,
    members: &[wire::MemberInfo],
    asked: impl Fn(&wire::MemberStage) -> bool,
    purpose: &str,
    what: &str,
) -> Result<Vec<u32>, No> {
    let mut found = Vec::new();
    for m in request.members.iter().filter(|m| asked(m)) {
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
                    "{:?} {}: files alone, which run nothing {purpose}",
                    m.kind, m.name
                ),
            ));
        }
        if found.contains(&info.index) {
            return Err(No(
                Cause::Request,
                format!("{:?} {}: its {what} asked for twice", m.kind, m.name),
            ));
        }
        found.push(info.index);
    }
    found.sort_unstable();
    Ok(found)
}

/// The members whose stdin `request` asks to write, by their declared index: each one the
/// image declares and runs, ascending; every other member's stdin is `/dev/null` (plan step
/// 5, D231 decision §3.3).
fn inputs(request: &wire::Request, members: &[wire::MemberInfo]) -> Result<Vec<u32>, No> {
    asked_members(request, members, |m| m.input, "to read a stdin", "input")
}

/// The members whose stdio the request asks to be a terminal of their own (plan step 6,
/// D244), by their declared index: each one of the image's that runs a program, each asked
/// once, refused by name otherwise.
fn terminals(request: &wire::Request, members: &[wire::MemberInfo]) -> Result<Vec<u32>, No> {
    asked_members(request, members, |m| m.terminal, "to have a terminal", "terminal")
}

/// The members the request gives an MCP bridge (D254), by their declared index, as
/// [`terminals`] names them; refused where it asks the run's agent-control endpoint none,
/// which alone carries their connections to its controller.
fn mcps(request: &wire::Request, members: &[wire::MemberInfo]) -> Result<Vec<u32>, No> {
    let mcps = asked_members(
        request,
        members,
        |m| m.mcp,
        "to reach an MCP bridge",
        "MCP bridge",
    )?;
    if !mcps.is_empty() && request.control.is_none() {
        return Err(No(
            Cause::Request,
            "a member's MCP bridge reaches its controller over the run's agent-control endpoint, \
             which the request asks none of"
                .into(),
        ));
    }
    Ok(mcps)
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
    policy_with(grants, narrowing, &[])
}

/// [`egress`] with members' own reach beside the run's (D278): the network process holds the
/// whole microVM to the union of every member's, each member's guest to its own. `given` is
/// each member's reach its orchestrator set, or each member's ceiling for the most the run may
/// ever reach. Each item once; none where nothing is.
fn policy_with(
    grants: Option<&shards_agentfile::agentfile::Grants>,
    narrowing: &wire::Narrowing,
    given: &[&wire::Reach],
) -> Result<Option<Vec<u8>>, No> {
    let none = shards_agentfile::agentfile::Grants::default();
    let g = grants.unwrap_or(&none);
    let ports = kept("egress", &g.egress, &narrowing.egress)?;
    let mcp = kept("remote MCP server", &g.mcp, &narrowing.mcp)?;
    let destinations = kept("destination", &g.destinations, &narrowing.destinations)?;
    let mut dns = g.dns && narrowing.dns;
    let mut text: Vec<String> = ports
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let mut named = crate::agentfile::named(&mcp);
    for d in crate::agentfile::named(&destinations) {
        if !named.contains(&d) {
            named.push(d);
        }
    }
    for r in given {
        for p in &r.ports {
            if !text.contains(p) {
                text.push(p.clone());
            }
        }
        for at in r.mcp.iter().chain(&r.destinations) {
            let d = shards_net::dns::destination(at).map_err(|why| No(Cause::Request, why))?;
            if !named.contains(&d) {
                named.push(d);
            }
        }
        dns |= r.dns;
    }
    if text.is_empty() && named.is_empty() && !dns {
        return Ok(None);
    }
    let ports = if text.is_empty() {
        shards_net::Ports::default()
    } else {
        shards_net::Ports::parse(&text.join(","))
            .map_err(|e| No(Cause::Image, format!("the run's egress grants: {e}")))?
    };
    Ok(Some(shards_net::encode_policy(&ports, &named, dns)))
}

/// A port range as an Agentfile writes one (`443`, `53/udp`, `8000-8010/tcp`), TCP where it
/// names none: its IP protocol's number and its ends (init's netplan `port_range`).
fn port_range(s: &str) -> Option<(u8, u16, u16)> {
    let (range, proto) = match s.rsplit_once('/') {
        Some((r, "tcp")) => (r, 6),
        Some((r, "udp")) => (r, 17),
        Some(_) => return None,
        None => (s, 6),
    };
    let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p > 0);
    let (lo, hi) = match range.split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => (port(range)?, port(range)?),
    };
    (lo <= hi).then_some((proto, lo, hi))
}

/// A member's reach as its guest takes it (D278, `run::egress`): each port range, each remote
/// MCP server's and destination's host and TCP port, and whether it asks names; refused, saying
/// which, for one that is none.
fn reach_grants(r: &wire::Reach) -> Result<shards_abi::run::egress::Grants, String> {
    let mut g = shards_abi::run::egress::Grants {
        ports: Vec::new(),
        named: Vec::new(),
        dns: r.dns,
    };
    for p in &r.ports {
        let range = port_range(p).ok_or_else(|| format!("{p:?} is no port range"))?;
        if !g.ports.contains(&range) {
            g.ports.push(range);
        }
    }
    for at in r.mcp.iter().chain(&r.destinations) {
        let d = shards_net::dns::destination(at)?;
        if !g.named.contains(&d) {
            g.named.push(d);
        }
    }
    Ok(g)
}

/// The first item of `asked` past what the image grants the run and `ceiling` grants its
/// member together (D278), as init's `egress::past` reads it for the member alone: a port range
/// their port grants cover, a destination either names or whose TCP port either grants to any
/// address, names where either grants them. None where every one is within.
fn past_run(
    asked: &shards_abi::run::egress::Grants,
    image: Option<&shards_agentfile::agentfile::Grants>,
    ceiling: Option<&shards_abi::run::egress::Grants>,
) -> Option<String> {
    let none = shards_agentfile::agentfile::Grants::default();
    let image = image.unwrap_or(&none);
    let text = |b: &Vec<u8>| String::from_utf8_lossy(b).into_owned();
    let mut wide: Vec<(u8, u16, u16)> = image.egress.iter().filter_map(|p| port_range(&text(p))).collect();
    let mut named: Vec<(String, u16)> = image
        .mcp
        .iter()
        .chain(&image.destinations)
        .filter_map(|d| shards_net::dns::destination(&text(d)).ok())
        .collect();
    let mut dns = image.dns;
    if let Some(c) = ceiling {
        wide.extend(c.ports.iter().copied());
        named.extend(c.named.iter().cloned());
        dns |= c.dns;
    }
    let covers = |(proto, lo, hi): (u8, u16, u16)| {
        let mut of: Vec<(u16, u16)> = wide.iter().filter(|r| r.0 == proto).map(|r| (r.1, r.2)).collect();
        of.sort_unstable();
        let mut next = u32::from(lo);
        for (a, b) in of {
            if u32::from(a) > next {
                break;
            }
            next = next.max(u32::from(b) + 1);
        }
        next > u32::from(hi)
    };
    let proto = |p: u8| if p == 17 { "UDP" } else { "TCP" };
    for &range in &asked.ports {
        if !covers(range) {
            return Some(format!("{} ports {}-{}", proto(range.0), range.1, range.2));
        }
    }
    for (host, port) in &asked.named {
        if !named.iter().any(|(h, p)| h == host && p == port) && !covers((6, *port, *port)) {
            return Some(format!("{host}:{port}"));
        }
    }
    if asked.dns && !dns {
        return Some("names past the microVM (dns)".into());
    }
    None
}

/// The EROFS image of the component of manifest digest `digest` for path `at` (D270 slice 4):
/// the OSI artifact of the home's store (`shards pull`, `shards build agent`, a component's
/// build), checked as a build checks one it takes (`agent::checked`: its config, its content,
/// its kind the path's: an agent's at `/agents/`, a harness's at `/harness/`), its content tar
/// its only layer, built into an EROFS root once, which the store keeps by its chain, opened
/// read-only. Refused typed: no such artifact here, or one unread (`Artifact`); one of another
/// kind (`Kind`).
fn component_image(
    home: &std::path::Path,
    agentfile: Option<&crate::agentfile::Agentfile>,
    ceilings: &[(u32, shards_abi::run::egress::Grants)],
    members: &[wire::MemberInfo],
    at: &str,
    digest: &str,
) -> Result<std::fs::File, (wire::SwapRefusal, String)> {
    use shards_image::osi::Kind;
    use wire::SwapRefusal as R;
    let want = if at.starts_with("/agents/") {
        Kind::Agent
    } else if at.starts_with("/harness/") {
        Kind::Harness
    } else {
        return Err((R::Unknown, format!("{at} is no member's path")));
    };
    let store = crate::pull::store(home).map_err(|e| (R::Artifact, e))?;
    let d = shards_image::reference::Digest::parse(digest)
        .map_err(|e| (R::Artifact, format!("{digest}: {e}")))?;
    let size = std::fs::metadata(store.blob_path(&d))
        .map_err(|e| (R::Artifact, format!("{digest}: not in this host's store: {e}")))?
        .len();
    let desc = shards_image::oci::Descriptor {
        media_type: shards_image::oci::media::OCI_MANIFEST.to_string(),
        digest: digest.to_string(),
        size: i64::try_from(size).map_err(|_| (R::Artifact, format!("{digest}: too large")))?,
        platform: None,
        annotations: std::collections::BTreeMap::new(),
    };
    let (manifest, config, kind) =
        crate::agent::checked(&store, digest, &desc, None).map_err(|e| (R::Artifact, e))?;
    if kind != want {
        return Err((
            R::Kind,
            format!("{digest} is {}, where {at} takes {}", kind.word(), want.word()),
        ));
    }
    component_fits(&config, agentfile, ceilings, members, at)
        .map_err(|(cause, why)| (cause, format!("{digest}: {why}")))?;
    let diff_ids = crate::agent::diff_ids(&store, kind, &manifest).map_err(|e| (R::Artifact, e))?;
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for (l, diff_id) in manifest.layers.iter().zip(diff_ids) {
        let blob = l.digest().map_err(|e| (R::Artifact, format!("{digest}: {e}")))?;
        let media_type = crate::agent::layer_type(kind, &l.media_type)
            .ok_or_else(|| (R::Artifact, format!("{digest}: a layer of type {}", l.media_type)))?;
        layers.push(shards_image::store::Layer {
            blob,
            media_type: media_type.to_string(),
            diff_id,
        });
    }
    let limits = crate::pull::limits().map_err(|e| (R::Failed, e))?;
    let rootfs = store
        .rootfs(&layers, &limits)
        .map_err(|e| (R::Failed, format!("{digest}'s image: {e}")))?;
    step(format_args!("swap at {at}: {digest}'s image ready"));
    std::fs::File::open(&rootfs).map_err(|e| (R::Failed, format!("{}: {e}", rootfs.display())))
}

/// Whether a component of config `config` fits the member at path `at` (D270 slice 4): its
/// platform one its microVM runs (none, any), and its asks within what that member is granted:
/// each network ask a port range or a destination the image's grants and its ceiling cover
/// (`past_run`, D278's reading), each volume one the image gives it (`VOLUME ... FOR`). Refused
/// typed (`Platform`, `Asks`), saying which.
fn component_fits(
    config: &shards_image::osi::Config,
    agentfile: Option<&crate::agentfile::Agentfile>,
    ceilings: &[(u32, shards_abi::run::egress::Grants)],
    members: &[wire::MemberInfo],
    at: &str,
) -> Result<(), (wire::SwapRefusal, String)> {
    use wire::SwapRefusal as R;
    if let Some(p) = &config.platform {
        let platform = shards_image::oci::Platform {
            architecture: p.architecture.clone(),
            os: p.os.clone(),
            variant: p.variant.clone(),
            os_features: Vec::new(),
        };
        if !shards_image::platform::runs(&platform, &shards_image::platform::guest()) {
            return Err((
                R::Platform,
                format!("{}/{} runs on no microVM of this host", p.os, p.architecture),
            ));
        }
    }
    let (kind, name) = match at.strip_prefix("/agents/") {
        Some(name) => (wire::MemberKind::Agent, name),
        None => (
            wire::MemberKind::Harness,
            at.strip_prefix("/harness/").unwrap_or(at),
        ),
    };
    let member = members
        .iter()
        .find(|m| m.kind == kind && m.name == name)
        .map(|m| m.index);
    let ceiling = member
        .and_then(|m| ceilings.iter().find(|(c, _)| *c == m))
        .map(|(_, g)| g);
    let (ports, destinations): (Vec<String>, Vec<String>) = config
        .asks
        .network
        .iter()
        .cloned()
        .partition(|a| port_range(a).is_some());
    let asked = reach_grants(&wire::Reach {
        ports,
        mcp: Vec::new(),
        destinations,
        dns: false,
    })
    .map_err(|why| (R::Asks, format!("its network asks: {why}")))?;
    if let Some(item) = past_run(&asked, agentfile.map(|a| &a.grants), ceiling) {
        return Err((
            R::Asks,
            format!("it asks {item}, past what its member is granted"),
        ));
    }
    let target = match kind {
        wire::MemberKind::Agent => shards_agentfile::agentfile::TargetKind::Agent,
        wire::MemberKind::Harness => shards_agentfile::agentfile::TargetKind::Harness,
    };
    let volumes = agentfile.map(|a| a.volumes.as_slice()).unwrap_or_default();
    for path in &config.asks.volumes {
        let given = volumes.iter().any(|v| {
            v.paths.iter().any(|p| p == path)
                && v.scope.kind.is_none_or(|k| k == target)
                && (v.scope.names.is_empty() || v.scope.names.iter().any(|n| n.as_slice() == name.as_bytes()))
        });
        if !given {
            return Err((
                R::Asks,
                format!("it asks the volume at {path}, which its image gives its member not"),
            ));
        }
    }
    Ok(())
}

/// The PCI slots an image run's microVM has (D214, D270 slice 4): one for a swap's old and one
/// for its new component of each agent and harness its image declares, then one for each plug
/// root, which a plug lies beneath; never fewer than any run's (`SHARDS_PCI_SLOTS`, D214's
/// four).
fn image_slots<T>(agentfile: Option<&crate::agentfile::Agentfile>, roots: &[T]) -> u8 {
    let derived = agentfile
        .map_or(0u32, |a| a.domains.saturating_mul(2))
        .saturating_add(u32::try_from(roots.len()).unwrap_or(u32::MAX));
    u8::try_from(derived)
        .unwrap_or(u8::MAX)
        .max(crate::daemon::pci_slots())
}

/// `a`'s reach and `b`'s together, each item once (D278).
fn reach_union(a: &wire::Reach, b: &wire::Reach) -> wire::Reach {
    let mut out = a.clone();
    for (into, from) in [
        (&mut out.ports, &b.ports),
        (&mut out.mcp, &b.mcp),
        (&mut out.destinations, &b.destinations),
    ] {
        for item in from {
            if !into.contains(item) {
                into.push(item.clone());
            }
        }
    }
    out.dns |= b.dns;
    out
}

/// Each member's ceiling past its microVM as its init takes it (D278): its declared index, of a
/// member the image declares and runs, and its reach as its guest takes it; refused, by its
/// member, for one of no member that runs, or a reach that is none.
fn ceilings(
    asked: &[wire::MemberCeiling],
    members: &[wire::MemberInfo],
) -> Result<Vec<(u32, shards_abi::run::egress::Grants)>, No> {
    asked
        .iter()
        .map(|c| {
            let m = members
                .iter()
                .find(|m| m.kind == c.kind && m.name == c.name && m.runs)
                .ok_or_else(|| {
                    No(
                        Cause::Request,
                        format!(
                            "a ceiling for {:?} {:?}, which runs in no member of the image",
                            c.kind, c.name
                        ),
                    )
                })?;
            let g = reach_grants(&c.reach).map_err(|why| {
                No(
                    Cause::Request,
                    format!("{:?} {:?}'s ceiling: {why}", c.kind, c.name),
                )
            })?;
            Ok((m.index, g))
        })
        .collect()
}

/// `narrowing` as its run holds it (D273): what of its image's grants each list keeps, written
/// out whole as the image writes them (`Keep::Only`), names asked where both grant them, and
/// its members' reach as asked; any other is a widening, refused by name, as [`egress`]
/// refuses it.
fn resolved(
    grants: Option<&shards_agentfile::agentfile::Grants>,
    narrowing: &wire::Narrowing,
) -> Result<wire::Narrowing, No> {
    let none = shards_agentfile::agentfile::Grants::default();
    let g = grants.unwrap_or(&none);
    let only = |what: &str, granted: &[Vec<u8>], keep: &wire::Keep| -> Result<wire::Keep, No> {
        Ok(wire::Keep::Only(
            kept(what, granted, keep)?
                .iter()
                .map(|item| String::from_utf8_lossy(item).into_owned())
                .collect(),
        ))
    };
    Ok(wire::Narrowing {
        egress: only("egress", &g.egress, &narrowing.egress)?,
        mcp: only("remote MCP server", &g.mcp, &narrowing.mcp)?,
        destinations: only("destination", &g.destinations, &narrowing.destinations)?,
        dns: g.dns && narrowing.dns,
        members: narrowing.members.clone(),
    })
}

/// The run's credential grants for its network process (D242), by reference and never a
/// value: each toward one host at least, and each host one its egress grants reach (D236),
/// as a grant that could never be filled is refused, not kept silently; each placeholder a
/// token, and one credential's alone, as its orchestrator's side checked them; the network
/// process checks them again as it takes them (D109). None where the run has none and signs
/// nothing in (`signs_in`): a run's sign-ins make its CA with its grants, none as they may be.
/// Its upstream roots (`roots`, each a certificate's DER) ride ahead of them, within what the
/// network process's wire carries, the only roots its injector trusts beside the host's: none
/// where it injects nothing.
fn credentials(
    grants: &[wire::CredentialGrant],
    roots: &[Vec<u8>],
    egress: Option<&[u8]>,
    signs_in: bool,
) -> Result<Option<Vec<u8>>, No> {
    if grants.is_empty() && !signs_in {
        return Ok(None);
    }
    shards_net::dns::credential::check(grants)
        .map_err(|refused| No(Cause::Request, format!("credential {refused}")))?;
    shards_net::dns::credential::check_roots(roots)
        .map_err(|refused| No(Cause::Request, refused.to_string()))?;
    let (ports, named, _) = egress.and_then(shards_net::decode_policy).unwrap_or_default();
    let mut held = Vec::with_capacity(grants.len());
    for g in grants {
        if g.toward.is_empty() {
            return Err(No(
                Cause::Request,
                format!(
                    "credential {}: a grant toward no host could never be filled",
                    g.reference
                ),
            ));
        }
        if let Some(host) = g.toward.iter().find(|h| !shards_net::reaches(&ports, &named, h)) {
            return Err(No(
                Cause::Request,
                format!(
                    "credential {} toward {host}: none of the run's egress grants reaches it (D236), so it could never be filled",
                    g.reference
                ),
            ));
        }
        held.push(g.clone());
    }
    shards_net::encode_credentials(roots, &held)
        .map(Some)
        .ok_or_else(|| {
            No(
                Cause::Request,
                "the run's credential grants: a reference past 65535 bytes, or a host past 255".into(),
            )
        })
}

/// The run's sign-in grants for its network process (`NET_SIGN_INS`, D242 step 8): each
/// sign-in's CLI's token host and each host its access token goes to one its egress grants
/// reach (D236), as a sign-in whose tokens could never be taken or used is refused, not kept
/// silently; one sign-in of a CLI; each placeholder an earlier capture gave a token, and one
/// credential's alone, the run's credential grants' included, as its orchestrator's side checked
/// them; the network process checks them again as it takes them (D109). None where the run has
/// none.
fn sign_ins(
    sign_ins: &[wire::SignInGrant],
    grants: &[wire::CredentialGrant],
    egress: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, No> {
    if sign_ins.is_empty() {
        return Ok(None);
    }
    let (ports, named, _) = egress.and_then(shards_net::decode_policy).unwrap_or_default();
    for s in sign_ins {
        let token = shards_net::signin::profile(s.cli).token.host;
        if s.toward.is_empty() {
            return Err(No(
                Cause::Request,
                format!("sign-in {}: its access token goes to no host", s.sign_in),
            ));
        }
        if let Some(host) = std::iter::once(token)
            .chain(s.toward.iter().map(String::as_str))
            .find(|h| !shards_net::reaches(&ports, &named, h))
        {
            return Err(No(
                Cause::Request,
                format!(
                    "sign-in {} toward {host}: none of the run's egress grants reaches it (D236), so its tokens could never be taken or used",
                    s.sign_in
                ),
            ));
        }
    }
    shards_net::dns::credential::check_sign_ins(grants, sign_ins)
        .map_err(|refused| No(Cause::Request, format!("sign-in {refused}")))?;
    shards_net::signin::encode_grants(sign_ins).map(Some).ok_or_else(|| {
        No(
            Cause::Request,
            "the run's sign-in grants: a name or placeholder past 65535 bytes, a host past 255, or more than 65535 hosts".into(),
        )
    })
}

/// The characters of a PEM line: "Generators MUST wrap the base64-encoded lines so that each
/// line consists of exactly 64 characters except for the final line" (RFC 7468 §2).
const PEM_LINE: usize = 64;

/// The setup entries giving the run's members its CA (D242, `shards_abi::run::trust`), once
/// its network process has answered `grants` (as `NET_CREDENTIALS` carries them) with the
/// CA's certificate `der`: the certificate in PEM (RFC 7468), and each host the grants name,
/// and each sign-in's (its CLI's token host and the hosts its access token goes to), once.
/// init lays it in the trust of each member whose link reaches one of those hosts.
fn trust(der: &[u8], grants: &[u8], sign_ins: &[wire::SignInGrant]) -> Result<Vec<Vec<u8>>, String> {
    use base64::Engine as _;
    use shards_abi::run::trust as entries;
    let (_, grants) =
        shards_net::decode_credentials(grants).ok_or("the run's credential grants are malformed")?;
    let base64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = b"-----BEGIN CERTIFICATE-----\n".to_vec();
    for line in base64.as_bytes().chunks(PEM_LINE) {
        pem.extend_from_slice(line);
        pem.push(b'\n');
    }
    pem.extend_from_slice(b"-----END CERTIFICATE-----\n");
    let mut hosts: Vec<&str> = Vec::new();
    let signed = sign_ins.iter().flat_map(|s| {
        std::iter::once(shards_net::signin::profile(s.cli).token.host)
            .chain(s.toward.iter().map(String::as_str))
    });
    for host in grants
        .iter()
        .flat_map(|g| g.toward.iter().map(String::as_str))
        .chain(signed)
    {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    let said = |e: entries::Error| format!("the run's CA: {e}");
    let mut out = vec![entries::ca(&pem).map_err(said)?];
    for host in hosts {
        out.push(entries::host(host.as_bytes()).map_err(said)?);
    }
    Ok(out)
}

/// Each member held to its reach of the names kept, where the request holds its members
/// apart (`Narrowing::members`, version 12, D246). A member reaches each destination and
/// remote MCP server kept that it reaches by name (`Agentfile::named`): its init lets it out
/// on their ports alone, the network process holds the whole microVM to the names kept
/// (D236), and `held` makes the members that reach a port by name reach it alike. So a run
/// in which a member the image declares would reach other than the request lets it (one the
/// request names in no entry: nothing) is refused before it starts, a widening of what was
/// asked; as is an entry for a member the image declares not, or a member's second entry.
fn held_apart(
    agentfile: Option<&crate::agentfile::Agentfile>,
    narrowing: &wire::Narrowing,
) -> Result<(), No> {
    use shards_agentfile::agentfile::TargetKind;
    let Some(asked) = &narrowing.members else {
        return Ok(());
    };
    let none = crate::agentfile::Agentfile::default();
    let a = agentfile.unwrap_or(&none);
    let mut names = kept("destination", &a.grants.destinations, &narrowing.destinations)?;
    names.extend(kept("remote MCP server", &a.grants.mcp, &narrowing.mcp)?);
    let kind_of = |k: TargetKind| match k {
        TargetKind::Agent => wire::MemberKind::Agent,
        TargetKind::Harness => wire::MemberKind::Harness,
    };
    for (i, m) in asked.iter().enumerate() {
        if !a
            .named
            .iter()
            .any(|n| kind_of(n.kind) == m.kind && n.name == m.name)
        {
            return Err(No(
                Cause::Image,
                format!("{:?} {}: the image declares no such member", m.kind, m.name),
            ));
        }
        if asked.iter().take(i).any(|o| o.kind == m.kind && o.name == m.name) {
            return Err(No(
                Cause::Request,
                format!("{:?} {}: its reach is named twice", m.kind, m.name),
            ));
        }
    }
    let said = |list: &[&str]| {
        if list.is_empty() {
            "nothing".to_string()
        } else {
            list.join(", ")
        }
    };
    for n in &a.named {
        let mut reach: Vec<&str> = n
            .reaches
            .iter()
            .map(String::as_str)
            .filter(|r| names.iter().any(|k| k.as_slice() == r.as_bytes()))
            .collect();
        reach.sort_unstable();
        reach.dedup();
        let mut lets: Vec<&str> = asked
            .iter()
            .find(|m| m.kind == kind_of(n.kind) && m.name == n.name)
            .map(|m| m.destinations.iter().map(String::as_str).collect())
            .unwrap_or_default();
        lets.sort_unstable();
        lets.dedup();
        if reach != lets {
            let word = match n.kind {
                TargetKind::Agent => "agent",
                TargetKind::Harness => "harness",
            };
            return Err(No(
                Cause::Widening,
                format!(
                    "{word} {}: of the names kept it would reach {}, where the request lets it reach {} (D246)",
                    n.name,
                    said(&reach),
                    said(&lets)
                ),
            ));
        }
    }
    Ok(())
}

/// The run's network and its network process's policy (D231, D250): the image's grants as the
/// request narrows them ([`egress`]), each member held apart where asked ([`held_apart`]),
/// and a bridge where the request keeps anything past the microVM or the image links its
/// members past it (`Agentfile::linked_past`), whatever is kept. Their init links them to
/// eth0 as their image grants (its `/.agentfile.json`, which no narrowing reaches), and the
/// network process holds the microVM to what is kept, nothing where nothing is (no policy
/// told: its `Policy::DenyAll`). Then the run refused what of it would reach the image's
/// domains ([`crate::agentfile::Agentfile::refuse`]).
fn network(
    agentfile: Option<&crate::agentfile::Agentfile>,
    request: &wire::Request,
    run: &mut shards_ipc::Run,
) -> Result<Option<Vec<u8>>, No> {
    let egress = egress(agentfile.map(|a| &a.grants), &request.egress)?;
    held_apart(agentfile, &request.egress)?;
    // A member that may be given a way past the microVM as the run goes has one from the
    // start (D278): its guest holds it shut until it is given.
    if egress.is_some()
        || agentfile.is_some_and(crate::agentfile::Agentfile::linked_past)
        || !request.egress_ceiling.is_empty()
    {
        run.network = "bridge".into();
    }
    if let Some(a) = agentfile {
        a.refuse(run).map_err(no(Cause::Request))?;
    }
    Ok(egress)
}

/// eth0's IPv6 for the run's agents' flows past the microVM (D99), as a daemon's run on the
/// default bridge has it: where its image's agents have IPv6 and are granted anything past
/// it, and the run is on the bridge, which gives eth0 none. The home's
/// (`crate::networks::host_ipv6`), which its init is told (`uplink6=`) and its network
/// process (`NET_ADDRESS`), which holds the agents to their grants as the request narrows
/// them; its command is kept to eth0's subnet as ever (`confine-eth0`), of IPv6 too.
fn past6(
    agentfile: Option<&crate::agentfile::Agentfile>,
    run: &shards_ipc::Run,
    home: &std::path::Path,
) -> Option<(std::net::Ipv6Addr, std::net::Ipv6Addr)> {
    (run.network == "bridge" && agentfile.is_some_and(crate::agentfile::Agentfile::egress6))
        .then(|| crate::networks::host_ipv6(home))
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
/// `spec`; `timing` asks for its VM's timing line. It writes no log, yet its head keeps a
/// run's default retention, as a daemon run's with no log does (`--log-driver none`,
/// daemon.rs `DEFAULT_LOGS`): a warm VM refuses a request whose log keeps nothing. Its head
/// carries `plug`, the most memory its guest may plug (`Prepared::plug_bytes`, D148), which
/// its VM sets before its guest hears of the run: a run told none has what it booted with,
/// its members' share what was free as their domains started.
fn run_request(spec: &shards_abi::run::Spec, plug: u64, published: bool, timing: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    let flags = if published { shards_ipc::RUN_PUBLISHED } else { 0 };
    shards_ipc::RunHead {
        // Its VM's timing line in its `EXIT`, its guest's markers in it, where its lines are
        // timed (`SHARDS_TIMING`).
        flags: if timing {
            flags | shards_ipc::RUN_TIMING
        } else {
            flags
        },
        log_size: crate::daemon::DEFAULT_LOGS.size,
        log_files: crate::daemon::DEFAULT_LOGS.files,
        plug,
        ..Default::default()
    }
    .encode_into(&mut payload);
    spec.encode_into(&mut payload);
    payload
}

/// What its VM said it holds a run's vCPUs to (`RATED`'s answer, D247), as asked with
/// `rate`, its line a managed machine's rate answer (`shards_abi::rate`): the rate it was
/// asked where it holds them, nothing where it lets them be, and the period its governor
/// derived for it (D191). A VM holds what it is told or nothing, so any other rate is a
/// broken word, never a hold; its refusal is its words.
fn rated_of(rate: Option<std::num::NonZeroU64>, said: &str) -> Result<wire::Rated, String> {
    use shards_abi::rate::{Answer, read_answer};
    match read_answer(said.as_bytes()) {
        Ok(Answer::Held { milli, period_ns }) if milli.is_none() || milli == rate => Ok(wire::Rated {
            milli,
            period: Duration::from_nanos(period_ns.get()),
        }),
        Ok(Answer::Held { milli, .. }) => Err(format!(
            "its VM held its vCPUs to {milli:?} milli-CPU, asked {rate:?}"
        )),
        Ok(Answer::Refused(why)) => Err(why.to_string()),
        Err(broken) => Err(format!("its VM's rate answer {said:?}: {broken}")),
    }
}

/// What init said it held a run within past what it uses (`builtin::ROOM`'s answer, D241),
/// as asked with `room`: a decimal where it was told one, nothing where it was told none.
fn held_of(room: Option<i64>, out: &[u8]) -> Result<Option<u64>, String> {
    match (room, std::str::from_utf8(out).map(str::trim)) {
        (None, Ok("")) => Ok(None),
        (Some(_), Ok(n)) => n
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("its init held it within {n:?}, no number of bytes")),
        (None, Ok(n)) => Err(format!("its init held it within {n:?} with no budget")),
        (_, Err(_)) => Err("its init's answer is no text".into()),
    }
}

/// How a run ended, from its VM's link (D249): why the link failed, else its `DONE`. That is
/// the status its microVM keeps and, where its init said why its run failed (`SYSTEM_ERR`),
/// init's words (warm.rs `done`). Such a run failed, in init's words, whether before its guest
/// said `STARTED` (its command never started) or after (its init failed it as it ran): the
/// status alone, the code dockerd gives it, would read as the command's own exit.
fn run_end(failed: Option<String>, done: Option<&[u8]>, started: bool) -> wire::RunEnd {
    if let Some(why) = failed {
        return wire::RunEnd::Failed(why);
    }
    match done.and_then(<[u8]>::split_first) {
        Some((status, words)) if !words.is_empty() => {
            let words = String::from_utf8_lossy(words);
            wire::RunEnd::Failed(if started {
                format!("its init failed after its command started: {words}")
            } else {
                format!("its command never started (status {status}): {words}")
            })
        }
        Some((status, _)) => wire::RunEnd::Exited(u32::from(*status)),
        None => wire::RunEnd::Failed(
            "the VM ended without its run's end; its last words are on this host's stderr".into(),
        ),
    }
}

/// What a run's VM told of its end on its link (warm.rs `finish`): whether its command started
/// (`STARTED`), why the VM could not serve its run, where it could not (`UNSERVED`), its
/// `DONE`, and what its machine used in all once down (`FINAL_USE`, D260).
#[derive(Debug, Default)]
struct Told {
    started: bool,
    unserved: Option<String>,
    done: Option<Vec<u8>>,
    used: Option<Vec<u8>>,
}

/// What a run's microVM used in all (D260's final use), as its VM told it (`FINAL_USE`, a
/// use's answer as `shards_abi::rate::used` writes it): each part its reading or its words; an
/// answer none of the protocol's typed by what breaks it. Where it told none, why not: its VM
/// killed for its orchestrator's asks (`STOP`, or their failure) before its machine was down;
/// else it ended without telling it.
fn final_use(told: Option<&[u8]>, asks: Option<&AsksEnd>) -> wire::FinalUse {
    let Some(told) = told else {
        return wire::FinalUse::Unread(match asks {
            Some(_) => wire::Unused::Killed,
            None => wire::Unused::Untold,
        });
    };
    match shards_abi::rate::read_used(told) {
        Ok(used) => wire::FinalUse::Read(wire::Microvm {
            guest: used.guest.map_err(str::to_owned),
            host: used.host.map_err(str::to_owned),
        }),
        Err(broken) => wire::FinalUse::Unread(wire::Unused::Broken(broken)),
    }
}

impl Told {
    /// The run's end, its VM's link closed, or broken as `failed` says. A VM that could not
    /// serve its run failed it, in its words: the status it gives its client then
    /// (`spec::NOT_RUN`, which `docker run` gives for dockerd's own failures) is no
    /// command's, and a link broken after it is its consequence.
    fn end(self, failed: Option<String>) -> wire::RunEnd {
        let failed = self
            .unserved
            .map(|why| format!("its VM could not serve its run: {why}"))
            .or(failed);
        run_end(failed, self.done.as_deref(), self.started)
    }
}

/// How the orchestrator's asks of a run ended (D231): its `STOP`; or the run failed as they
/// were served, for the reason given: its channel ended without `STOP` (its owner gone), or
/// broke (a frame this host cannot read, or an ask that breaks its contract), or what an ask
/// needed failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AsksEnd {
    Stopped,
    Failed(String),
}

/// The run's end: as its VM said it (`vm`; `said`, its `DONE` came), whatever its asks did
/// meanwhile; else as its orchestrator's asks ended it ([`AsksEnd`]), its VM killed for them,
/// which leaves it no end of its own to say.
fn ended_by(asks: Option<&AsksEnd>, vm: wire::RunEnd, said: bool) -> wire::RunEnd {
    match asks {
        Some(AsksEnd::Stopped) if !said => wire::RunEnd::Stopped,
        Some(AsksEnd::Failed(why)) if !said => wire::RunEnd::Failed(why.clone()),
        _ => vm,
    }
}

/// An orchestrator's ask, as its frame reads (D231): a lease's answer as its frame holds it.
enum Ask<'a> {
    Credit(wire::Credit),
    Input(wire::Input),
    InputEnd(wire::InputEnd),
    Leased(&'a [u8]),
    /// A sign-in's capture answered (D242, step 8), as its frame holds it.
    Captured(&'a [u8]),
    /// A sign-in's refresh answered, its token response as its frame holds it.
    Refreshed(&'a [u8]),
    Stats(wire::StatsAsk),
    Room(wire::RoomAsk),
    Rate(wire::RateAsk),
    Resize(wire::ResizeAsk),
    Plug(wire::PlugAsk),
    Unplug(wire::UnplugAsk),
    Save(wire::SaveAsk),
    Pause(wire::PauseAsk),
    Net(wire::NetAsk),
    /// A member started again in its microVM (D270 slice 3b).
    Restart(wire::RestartAsk),
    /// A member's component swapped, or its image's back (D270 slice 4).
    Swap(wire::SwapAsk),
}

/// The orchestrator's next ask, from what reading its next frame gave (`read`: its kind and
/// payload), or how that frame ends its asks.
fn read_ask<'a>(read: Result<Option<(u8, &'a [u8])>, &io::Error>) -> Result<Ask<'a>, AsksEnd> {
    match read {
        Ok(Some((which, payload))) => judged(which, payload),
        Ok(None) => Err(AsksEnd::Failed(
            "its orchestrator's channel ended without STOP".into(),
        )),
        Err(e) => Err(broke(format_args!("{e}"))),
    }
}

/// The run failed as its orchestrator broke its channel, as `why` says.
fn broke(why: std::fmt::Arguments<'_>) -> AsksEnd {
    AsksEnd::Failed(format!("its orchestrator's channel broke: {why}"))
}

/// The ask frame `which` carries, or how it ends the asks: `STOP`; or a payload its kind does
/// not read, or a kind that is no ask, which breaks the channel.
fn judged(which: u8, payload: &[u8]) -> Result<Ask<'_>, AsksEnd> {
    let (what, ask) = match which {
        wire::to_host::STOP => return Err(AsksEnd::Stopped),
        wire::to_host::CREDIT => ("CREDIT", wire::Credit::decode(payload).map(Ask::Credit)),
        wire::to_host::INPUT => ("INPUT", wire::Input::decode(payload).map(Ask::Input)),
        wire::to_host::INPUT_END => ("INPUT_END", wire::InputEnd::decode(payload).map(Ask::InputEnd)),
        wire::to_host::LEASED => ("LEASED", Ok(Ask::Leased(payload))),
        wire::to_host::CAPTURED => ("CAPTURED", Ok(Ask::Captured(payload))),
        wire::to_host::REFRESHED => ("REFRESHED", Ok(Ask::Refreshed(payload))),
        wire::to_host::STATS => ("STATS", wire::StatsAsk::decode(payload).map(Ask::Stats)),
        wire::to_host::ROOM => ("ROOM", wire::RoomAsk::decode(payload).map(Ask::Room)),
        wire::to_host::RATE => ("RATE", wire::RateAsk::decode(payload).map(Ask::Rate)),
        wire::to_host::RESIZE => ("RESIZE", wire::ResizeAsk::decode(payload).map(Ask::Resize)),
        wire::to_host::PLUG => ("PLUG", wire::PlugAsk::decode(payload).map(Ask::Plug)),
        wire::to_host::UNPLUG => ("UNPLUG", wire::UnplugAsk::decode(payload).map(Ask::Unplug)),
        wire::to_host::SAVE => ("SAVE", wire::SaveAsk::decode(payload).map(Ask::Save)),
        wire::to_host::PAUSE => ("PAUSE", wire::PauseAsk::decode(payload).map(Ask::Pause)),
        wire::to_host::NET => ("NET", wire::NetAsk::decode(payload).map(Ask::Net)),
        wire::to_host::RESTART => ("RESTART", wire::RestartAsk::decode(payload).map(Ask::Restart)),
        wire::to_host::SWAP => ("SWAP", wire::SwapAsk::decode(payload).map(Ask::Swap)),
        other => return Err(broke(format_args!("a frame of kind {other}, which is no ask"))),
    };
    ask.map_err(|e| broke(format_args!("a malformed {what}: {e}")))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a live lease of the run's stands (D242, R04): asked of the orchestrator; or held
/// until `until` on this process's clock, none where its end is past what that clock can
/// name, which the run's end releases. Refused and released are its ends, where it leaves
/// the ledger.
enum State {
    Asked,
    Held { until: Option<Instant> },
}

impl State {
    fn lapsed(&self, now: Instant) -> bool {
        matches!(self, State::Held { until: Some(until) } if *until <= now)
    }
}

/// A live lease: its credential, the network process's id for its ask, and where it stands.
struct Entry {
    reference: String,
    asked_as: u32,
    state: State,
}

/// What an orchestrator's answer to a lease comes to.
#[derive(Debug, PartialEq, Eq)]
enum Answered {
    /// The network process's ask `asked_as`, of `reference`, to be told it.
    Told { asked_as: u32, reference: String },
    /// One after the run released its leases: told nobody.
    Late,
}

/// A sign-in's ask of the orchestrator under way (D242, step 8; R04): a capture of its tokens,
/// or a refresh. Answered or let go of at the run's release, its ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signing {
    Capture,
    Refresh,
}

/// The run's leases, as its host keeps them (D242): each one its network process asked, by
/// the id the run's channel names it by. Asked of the orchestrator by reference, then held
/// until its end, or refused; every one released as the run ends. A run asks only what its
/// grants name, and one lease of a credential at a time, as its network process asks one,
/// so at most one for each grant is here: what grows past that is refused, never kept.
///
/// And the run's sign-ins (step 8): each grant, with what its last capture gave, whose access
/// token is one of the run's grants from then on; and each ask of the orchestrator under way,
/// by the id its network process gave it, which the run's channel names it by. That process
/// asks one refresh of a sign-in at a time (`shards_net::signin::SignIns`), so at most one ask
/// of each kind for each sign-in is here, bounded by the run's grants.
struct Ledger {
    granted: Vec<String>,
    live: HashMap<u32, Entry>,
    next: u32,
    released: bool,
    sign_ins: Vec<wire::SignInGrant>,
    signing: HashMap<u32, (usize, Signing)>,
}

impl Ledger {
    /// The run's ledger, its grants' references `granted` and its sign-ins `sign_ins`, whose
    /// access tokens an earlier capture gave are granted too.
    fn new(mut granted: Vec<String>, sign_ins: Vec<wire::SignInGrant>) -> Ledger {
        granted.extend(
            sign_ins
                .iter()
                .filter_map(|s| s.captured.as_ref().map(|c| c.access.clone())),
        );
        Ledger {
            granted,
            live: HashMap::new(),
            next: 0,
            released: false,
            sign_ins,
            signing: HashMap::new(),
        }
    }

    /// The network process's ask `id` of sign-in `sign_in`, a capture or a refresh: asked of the
    /// orchestrator by the same id; or declined here, typed, nothing asked: after the run's
    /// release, or of a sign-in no grant of the run's names. An id under way asked again breaks
    /// that process's contract (`Err`).
    fn sign_in(&mut self, id: u32, sign_in: &str, asking: Signing) -> Result<Result<(), Declined>, String> {
        if self.released {
            return Ok(Err(Declined::Ended));
        }
        let Some(index) = self.sign_ins.iter().position(|s| s.sign_in == sign_in) else {
            return Ok(Err(Declined::Denied("no sign-in of the run's".into())));
        };
        if self.signing.contains_key(&id) {
            return Err(format!("its network process asked by {id}, an id under way"));
        }
        self.signing.insert(id, (index, asking));
        Ok(Ok(()))
    }

    /// The orchestrator's answer to sign-in ask `id`, of kind `asking`: the sign-in it was of;
    /// none after the run's release, which goes nowhere. One to no ask of that kind under way
    /// breaks the channel's contract.
    fn signed_in(&mut self, id: u32, asking: Signing) -> Result<Option<usize>, String> {
        if self.released {
            return Ok(None);
        }
        match self.signing.remove(&id) {
            Some((index, asked)) if asked == asking => Ok(Some(index)),
            Some(other) => {
                self.signing.insert(id, other);
                Err(format!("an answer of sign-in ask {id} of another kind"))
            }
            None => Err(format!(
                "an answer of sign-in ask {id}, which no ask of the run awaits"
            )),
        }
    }

    /// Sign-in `index` captured as `c`: its access token's reference granted, in place of the
    /// one an earlier capture gave where no other grant names that one.
    fn captured(&mut self, index: usize, c: &wire::Captured) {
        let Some(s) = self.sign_ins.get_mut(index) else {
            return;
        };
        if let Some(old) = s.captured.replace(c.clone())
            && old.access != c.access
            && let Some(at) = self.granted.iter().position(|g| *g == old.access)
        {
            self.granted.remove(at);
        }
        if !self.granted.contains(&c.access) {
            self.granted.push(c.access.clone());
        }
    }

    /// Sign-in `index` refreshed outside (Q2): its access token's held lease let go of, its
    /// value rotated, so that the next ask of it is asked anew rather than refused as live; an
    /// ask of it under way kept, as its network process keeps it.
    fn refreshed(&mut self, index: usize) {
        let Some(access) = self
            .sign_ins
            .get(index)
            .and_then(|s| s.captured.as_ref())
            .map(|c| c.access.clone())
        else {
            return;
        };
        self.live
            .retain(|_, e| !(e.reference == access && matches!(e.state, State::Held { .. })));
    }

    /// Sign-in `sign_in`'s name where it is one of the run's.
    fn signs_in(&self, sign_in: &str) -> bool {
        self.sign_ins.iter().any(|s| s.sign_in == sign_in)
    }

    /// The network process's ask `asked_as` of credential `reference`, at `now`: a lease to
    /// ask of the orchestrator by the id returned (its lease that ended released first); or
    /// refused here, typed, nothing asked.
    fn ask(&mut self, asked_as: u32, reference: &str, now: Instant) -> Result<u32, Refusal> {
        if self.released {
            return Err(Refusal::Ended);
        }
        if !self.granted.iter().any(|g| g == reference) {
            return Err(Refusal::Ungranted);
        }
        self.live
            .retain(|_, e| !(e.reference == reference && e.state.lapsed(now)));
        if self.live.values().any(|e| e.reference == reference) {
            return Err(Refusal::Live);
        }
        // An id no live lease has: at most one more try than there are of them.
        let id = loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            if !self.live.contains_key(&id) {
                break id;
            }
        };
        self.live.insert(
            id,
            Entry {
                reference: reference.to_string(),
                asked_as,
                state: State::Asked,
            },
        );
        Ok(id)
    }

    /// The orchestrator's answer to lease `id` at `now`: held for `lives` where given, else
    /// refused. One to no ask the run awaits breaks the channel's contract; one after the run
    /// released its leases is late, and goes nowhere.
    fn answer(&mut self, id: u32, lives: Option<NonZeroU64>, now: Instant) -> Result<Answered, String> {
        if self.released {
            return Ok(Answered::Late);
        }
        let Some(entry) = self.live.get_mut(&id).filter(|e| matches!(e.state, State::Asked)) else {
            return Err(format!("an answer of lease {id}, which no ask of the run awaits"));
        };
        let told = Answered::Told {
            asked_as: entry.asked_as,
            reference: entry.reference.clone(),
        };
        match lives {
            Some(lives) => {
                entry.state = State::Held {
                    until: now.checked_add(Duration::from_nanos(lives.get())),
                };
            }
            None => {
                self.live.remove(&id);
            }
        }
        Ok(told)
    }

    /// Every live lease released, once: how many were held and how many asked; none where
    /// the run released them already. Every ask after is refused, the run ended.
    fn release(&mut self) -> Option<(usize, usize)> {
        if self.released {
            return None;
        }
        self.released = true;
        let held = self
            .live
            .values()
            .filter(|e| matches!(e.state, State::Held { .. }))
            .count();
        let asked = self.live.len().saturating_sub(held);
        self.live.clear();
        self.signing.clear();
        Some((held, asked))
    }
}

/// What the run host says of its leases on its diagnostics (D242): each ask, answer and the
/// run's release, by its credential's reference and its lease's id, and never a value.
enum Audit<'a> {
    Asked {
        reference: &'a str,
        id: u32,
    },
    Held {
        reference: &'a str,
        id: u32,
        lives: Duration,
    },
    Refused {
        reference: &'a str,
        id: Option<u32>,
        refusal: &'a Refusal,
    },
    Released {
        held: usize,
        asked: usize,
    },
    /// A sign-in's ask relayed to the orchestrator (D242, step 8), by the sign-in's name.
    Signing {
        sign_in: &'a str,
        id: u32,
        asking: Signing,
    },
    /// A sign-in's capture kept outside: its access token leased by this reference from now.
    Captured {
        sign_in: &'a str,
        id: u32,
        reference: &'a str,
    },
    /// A sign-in refreshed outside: its access token's held lease let go of.
    Refreshed {
        sign_in: &'a str,
        id: u32,
    },
    /// A sign-in's ask declined, here (none) or by the orchestrator.
    Declined {
        sign_in: &'a str,
        id: Option<u32>,
        why: &'a Declined,
    },
    /// The run's VM revoked a sign-in, answered in its network process, its tokens kept.
    Revoked {
        sign_in: &'a str,
    },
}

impl std::fmt::Display for Audit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Audit::Asked { reference, id } => write!(f, "credential {reference}: lease {id} asked"),
            Audit::Held { reference, id, lives } => {
                write!(f, "credential {reference}: lease {id} held for {lives:?}")
            }
            Audit::Refused {
                reference,
                id: Some(id),
                refusal,
            } => write!(f, "credential {reference}: lease {id} refused: {refusal}"),
            Audit::Refused {
                reference,
                id: None,
                refusal,
            } => write!(f, "credential {reference}: an ask refused here: {refusal}"),
            Audit::Released { held, asked } => {
                write!(f, "its leases released as it ends: {held} held, {asked} asked")
            }
            Audit::Signing {
                sign_in,
                id,
                asking: Signing::Capture,
            } => write!(f, "sign-in {sign_in}: capture {id} asked"),
            Audit::Signing {
                sign_in,
                id,
                asking: Signing::Refresh,
            } => write!(f, "sign-in {sign_in}: refresh {id} asked"),
            Audit::Captured {
                sign_in,
                id,
                reference,
            } => write!(
                f,
                "sign-in {sign_in}: capture {id} kept, its access token leased as {reference}"
            ),
            Audit::Refreshed { sign_in, id } => {
                write!(
                    f,
                    "sign-in {sign_in}: refresh {id} answered, its access token's held lease let go"
                )
            }
            Audit::Declined {
                sign_in,
                id: Some(id),
                why,
            } => write!(f, "sign-in {sign_in}: ask {id} declined: {why}"),
            Audit::Declined {
                sign_in,
                id: None,
                why,
            } => {
                write!(f, "sign-in {sign_in}: an ask declined here: {why}")
            }
            Audit::Revoked { sign_in } => {
                write!(
                    f,
                    "sign-in {sign_in}: revoked in its VM, answered there, its tokens kept"
                )
            }
        }
    }
}

/// A run's network process's control as its run goes on (D242, D269): written one whole
/// message at a time, by one thread at a time, and read by one alone ([`relay`]), which takes
/// that process's asks (its leases and sign-ins, where the run has grants) and the answers to
/// this host's own asks of it ([`NetControl::ask`]), which that process answers in the order
/// asked, each with its ask's kind.
struct NetControl<'a> {
    control: &'a crate::netproc::Control,
    sending: Mutex<()>,
    /// None once the control is read no more (closed, or broken), as nothing will answer an
    /// ask then; every waiter's sender dropped with it.
    asked: Mutex<Option<NetAsks>>,
}

/// Each ask of a run host's of its network process sent and not yet answered, in the order
/// sent: its kind, and who waits for its answer's payload ([`NetControl::ask`]).
type NetAsks = std::collections::VecDeque<(u8, std::sync::mpsc::Sender<Vec<u8>>)>;

impl<'a> NetControl<'a> {
    fn new(control: &'a crate::netproc::Control) -> NetControl<'a> {
        NetControl {
            control,
            sending: Mutex::new(()),
            asked: Mutex::new(Some(std::collections::VecDeque::new())),
        }
    }

    /// Sends `kind` with `payload` and the sockets `socks`, which its network process does not
    /// answer (`NET_PUBLISHED`): their sender keeps its copies until it lets them go (M24).
    fn tell(&self, kind: u8, payload: &[u8], socks: &[crate::netproc::Lent<'_>]) -> io::Result<()> {
        let _one = lock(&self.sending);
        crate::netproc::send(self.control, kind, payload, socks)
    }

    /// Sends `kind` with `payload`, and waits for its network process's answer of that kind,
    /// which the relay hands back in the order asked: its payload; refused where it could not
    /// be sent, or where the control is read no more before it is answered.
    fn ask(&self, kind: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let (answered, answer) = std::sync::mpsc::channel();
        {
            let _one = lock(&self.sending);
            match lock(&self.asked).as_mut() {
                Some(asked) => asked.push_back((kind, answered)),
                None => return Err("its network process's control is read no more".into()),
            }
            if let Err(e) = crate::netproc::send(self.control, kind, payload, &[]) {
                // Its own entry the last: none goes in while `sending` is held.
                if let Some(asked) = lock(&self.asked).as_mut() {
                    asked.pop_back();
                }
                return Err(format!("its network process: {e}"));
            }
        }
        answer
            .recv()
            .map_err(|_| "its network process ended before it answered".to_string())
    }

    /// The answer of `kind` its network process sent, `payload`, handed to the oldest ask,
    /// which it must be of: whether it was.
    fn answered(&self, kind: u8, payload: Vec<u8>) -> bool {
        let mut asked = lock(&self.asked);
        match asked.as_mut().and_then(|a| a.front().map(|(k, _)| *k)) {
            Some(oldest) if oldest == kind => {
                if let Some((_, waiter)) = asked.as_mut().and_then(std::collections::VecDeque::pop_front) {
                    let _ = waiter.send(payload);
                }
                true
            }
            _ => false,
        }
    }

    /// Nothing more is read of it: every ask waiting refused, as is every one after.
    fn closed(&self) {
        lock(&self.asked).take();
    }
}

/// A run's leases, as its host relays them (D242): its ledger, and its network process's
/// control, on which that process's asks come and their answers go.
struct Leasing<'a> {
    ledger: Mutex<Ledger>,
    net: &'a NetControl<'a>,
}

impl Leasing<'_> {
    /// Tells the network process its ask `asked_as`'s answer: the value and its lease's
    /// life, or why not. The message's copy of a value is wiped once sent, and sent as it
    /// lies, no other copy made: on Unix in one sendmsg of its parts, on Windows in two writes
    /// (`shards_ipc::write_message_parts`), as this one thread at a time writes the control.
    fn tell(&self, asked_as: u32, answer: Result<(NonZeroU64, &[u8]), &Refusal>) -> io::Result<()> {
        let payload = shards_net::lease::encode_answer(asked_as, answer)
            .ok_or_else(|| io::Error::other("a lease's answer past what its lengths count"))?;
        let _one = lock(&self.net.sending);
        #[cfg(unix)]
        return crate::netproc::send(self.net.control, kind::NET_LEASE, &payload, &[]);
        #[cfg(windows)]
        return shards_ipc::write_message_parts(&self.net.control.socket, kind::NET_LEASE, &payload);
    }

    /// Tells the network process a sign-in's answer (D242, step 8), wiped as it drops, sent as
    /// it lies as [`tell`](Self::tell) sends a lease's.
    fn tell_sign_in(&self, answered: &signin::Answered) -> io::Result<()> {
        let payload = answered
            .encode()
            .ok_or_else(|| io::Error::other("a sign-in's answer past what its lengths count"))?;
        let _one = lock(&self.net.sending);
        #[cfg(unix)]
        return crate::netproc::send(self.net.control, kind::NET_SIGN_IN, &payload, &[]);
        #[cfg(windows)]
        return shards_ipc::write_message_parts(&self.net.control.socket, kind::NET_SIGN_IN, &payload);
    }
}

/// The network process's refusal for the orchestrator's `refusal`, each cause kept.
fn net_refusal(refusal: &wire::LeaseRefusal) -> Refusal {
    match refusal {
        wire::LeaseRefusal::Denied(why) => Refusal::Denied(why.clone()),
        wire::LeaseRefusal::KeyReplaced { asked, current } => Refusal::KeyReplaced {
            asked: asked.clone(),
            current: current.clone(),
        },
        wire::LeaseRefusal::Unavailable(why) => Refusal::Unavailable(why.clone()),
        wire::LeaseRefusal::Ended => Refusal::Ended,
        wire::LeaseRefusal::Lapsed => Refusal::Lapsed,
    }
}

/// Reads the run's network process's control until it closes (D242, D269): each answer to an
/// ask of this host's handed to its asker ([`NetControl::answered`]); each lease that process
/// asks that the ledger takes asked of the orchestrator as `LEASE`, by reference and never a
/// value, the rest refused here, typed; each sign-in's word relayed. A message that is none of
/// these, or a lease or sign-in of a run with no grants, breaks that process's contract, and
/// nothing more of it is read. Once nothing is, every ask of this host's waiting is refused.
fn relay(net: &NetControl<'_>, leasing: Option<&Leasing<'_>>, answers: &Answers) {
    read_control(net, leasing, answers);
    net.closed();
}

/// [`relay`]'s reading, until the control closes or breaks.
fn read_control(net: &NetControl<'_>, leasing: Option<&Leasing<'_>>, answers: &Answers) {
    loop {
        let mut m = match crate::netproc::recv(net.control) {
            Ok(Some(m)) => m,
            Ok(None) | Err(_) => return,
        };
        let asks = m.kind == kind::NET_SIGN_IN || m.kind == kind::NET_LEASE;
        if !asks && net.answered(m.kind, std::mem::take(&mut m.payload)) {
            continue;
        }
        let leasing = match leasing {
            Some(leasing) if asks => leasing,
            _ => {
                zeroize::Zeroize::zeroize(&mut m.payload);
                say(format_args!(
                    "its network process said message kind {}, no answer to this host's oldest ask of it nor an ask of a run with grants: nothing more of it is read",
                    m.kind
                ));
                return;
            }
        };
        // A sign-in's word (D242, step 8): its message's copy of a capture's tokens wiped as it
        // goes, whatever came of it.
        if m.kind == kind::NET_SIGN_IN {
            let told = signin::Told::decode(&m.payload);
            zeroize::Zeroize::zeroize(&mut m.payload);
            match told.map(|told| sign_in_told(leasing, answers, told)) {
                Some(Ok(())) => continue,
                Some(Err(why)) => say(format_args!("{why}: nothing more of it is read")),
                None => say(format_args!(
                    "its network process said a sign-in's word this side does not read: nothing more of it is read"
                )),
            }
            return;
        }
        let Some((asked_as, reference)) = shards_net::lease::decode_ask(&m.payload) else {
            say(format_args!(
                "its network process asked a lease this side does not read: nothing more of it is read"
            ));
            return;
        };
        let asked = lock(&leasing.ledger).ask(asked_as, reference, Instant::now());
        match asked {
            Ok(id) => {
                say(format_args!("{}", Audit::Asked { reference, id }));
                let ask = wire::LeaseAsk {
                    id,
                    reference: reference.to_string(),
                };
                // The orchestrator gone, its run ends, which releases this lease with the
                // rest.
                if let Ok(payload) = ask.encode() {
                    let _ = answers.send(wire::to_client::LEASE, &[&payload]);
                }
            }
            Err(refusal) => {
                say(format_args!(
                    "{}",
                    Audit::Refused {
                        reference,
                        id: None,
                        refusal: &refusal,
                    }
                ));
                if leasing.tell(asked_as, Err(&refusal)).is_err() {
                    return;
                }
            }
        }
    }
}

/// The orchestrator's answer to a lease (`LEASED`, D242), relayed to the network process
/// that asked it: its value for its lease's life, or its refusal, typed. The decoded copy of
/// a value is wiped as it drops (`Secret`). One to no ask the run awaits breaks the channel's
/// contract.
fn leased(leasing: &Leasing<'_>, payload: &[u8]) -> Result<(), String> {
    let answer = wire::LeaseAnswer::decode(payload).map_err(|e| e.to_string())?;
    let lives = answer.result.as_ref().ok().map(|lease| lease.lives);
    let told = lock(&leasing.ledger).answer(answer.id, lives, Instant::now())?;
    let Answered::Told { asked_as, reference } = told else {
        return Ok(());
    };
    // A network process gone is told nothing: its values went with it.
    let _ = match &answer.result {
        Ok(lease) => {
            let audit = Audit::Held {
                reference: &reference,
                id: answer.id,
                lives: Duration::from_nanos(lease.lives.get()),
            };
            say(format_args!("{audit}"));
            leasing.tell(asked_as, Ok((lease.lives, &lease.value.0)))
        }
        Err(refusal) => {
            let refusal = net_refusal(refusal);
            let audit = Audit::Refused {
                reference: &reference,
                id: Some(answer.id),
                refusal: &refusal,
            };
            say(format_args!("{audit}"));
            leasing.tell(asked_as, Err(&refusal))
        }
    };
    Ok(())
}

/// A sign-in's word from the network process (D242, step 8), relayed to the orchestrator: a
/// capture's tokens (`CAPTURE`, written as they lie and wiped as they go) or a refresh
/// (`REFRESH`), each asked by its id once the ledger takes it, else declined here, typed; a
/// revocation in the VM told (`REVOKE`), answered by nobody. Only the orchestrator's broker
/// calls the provider (Q1): nothing here does. An ask by an id under way breaks that process's
/// contract (`Err`).
fn sign_in_told(leasing: &Leasing<'_>, answers: &Answers, told: signin::Told) -> Result<(), String> {
    let (id, sign_in, asking) = match &told {
        signin::Told::Capture { id, sign_in, .. } => (*id, sign_in.clone(), Signing::Capture),
        signin::Told::Refresh { id, sign_in } => (*id, sign_in.clone(), Signing::Refresh),
        signin::Told::Revoke { sign_in } => {
            if lock(&leasing.ledger).signs_in(sign_in) {
                say(format_args!("{}", Audit::Revoked { sign_in }));
                if let Ok(payload) = (wire::Revoked {
                    sign_in: sign_in.clone(),
                })
                .encode()
                {
                    let _ = answers.send(wire::to_client::REVOKE, &[&payload]);
                }
            }
            return Ok(());
        }
    };
    let taken = lock(&leasing.ledger).sign_in(id, &sign_in, asking)?;
    if let Err(why) = taken {
        let audit = Audit::Declined {
            sign_in: &sign_in,
            id: None,
            why: &why,
        };
        say(format_args!("{audit}"));
        let answered = match asking {
            Signing::Capture => signin::Answered::Captured { id, result: Err(why) },
            Signing::Refresh => signin::Answered::Refreshed { id, result: Err(why) },
        };
        return leasing.tell_sign_in(&answered).map_err(|e| e.to_string());
    }
    say(format_args!(
        "{}",
        Audit::Signing {
            sign_in: &sign_in,
            id,
            asking
        }
    ));
    // The orchestrator gone, the run ends, and its network process with it.
    let _ = match told {
        signin::Told::Capture { taken, .. } => {
            let signin::Taken {
                mut access,
                refresh,
                expires_in,
                id_token,
                scope,
            } = taken;
            // Each token moved, never copied, into what wipes it as it drops.
            let secret = |t: &mut zeroize::Zeroizing<Vec<u8>>| wire::Secret(std::mem::take(&mut **t));
            let ask = wire::CaptureAsk {
                id,
                sign_in,
                tokens: wire::SignInTokens {
                    access: secret(&mut access),
                    refresh: refresh.map(|mut r| secret(&mut r)),
                    expires_in,
                    id_token: id_token.map(|mut t| secret(&mut t)),
                    scope,
                },
            };
            let payload = ask.encode().map(zeroize::Zeroizing::new);
            drop(ask);
            match payload {
                Ok(payload) => answers.send(wire::to_client::CAPTURE, &[&payload]),
                Err(e) => {
                    let why = Declined::Unavailable(format!("its tokens: {e}"));
                    lock(&leasing.ledger).signed_in(id, Signing::Capture)?;
                    return leasing
                        .tell_sign_in(&signin::Answered::Captured { id, result: Err(why) })
                        .map_err(|e| e.to_string());
                }
            }
        }
        signin::Told::Refresh { .. } => match (wire::RefreshAsk { id, sign_in }).encode() {
            Ok(payload) => answers.send(wire::to_client::REFRESH, &[&payload]),
            Err(e) => Err(io::Error::other(e)),
        },
        signin::Told::Revoke { .. } => Ok(()),
    };
    Ok(())
}

/// The network process's refusal of a sign-in's ask for the orchestrator's `refusal`, each cause
/// kept.
fn declined(refusal: wire::SignInRefusal) -> Declined {
    match refusal {
        wire::SignInRefusal::Denied(why) => Declined::Denied(why),
        wire::SignInRefusal::Unavailable(why) => Declined::Unavailable(why),
        wire::SignInRefusal::Ended => Declined::Ended,
        wire::SignInRefusal::Unanswered => Declined::Unanswered,
    }
}

/// The orchestrator's answer to a sign-in's capture (`CAPTURED`, D242 step 8), relayed to the
/// network process that asked it: the placeholders its CLI is to hold, its access token one of
/// the run's grants from now; or why not. One to no capture the run awaits breaks the
/// channel's contract; one after the run's release goes nowhere.
fn captured(leasing: &Leasing<'_>, payload: &[u8]) -> Result<(), String> {
    let answer = wire::CaptureAnswer::decode(payload).map_err(|e| e.to_string())?;
    let mut ledger = lock(&leasing.ledger);
    let Some(index) = ledger.signed_in(answer.id, Signing::Capture)? else {
        return Ok(());
    };
    let sign_in = ledger
        .sign_ins
        .get(index)
        .map(|s| s.sign_in.clone())
        .unwrap_or_default();
    let result = match answer.result {
        Ok(c) => {
            ledger.captured(index, &c);
            let audit = Audit::Captured {
                sign_in: &sign_in,
                id: answer.id,
                reference: &c.access,
            };
            say(format_args!("{audit}"));
            Ok(c)
        }
        Err(refusal) => {
            let why = declined(refusal);
            let audit = Audit::Declined {
                sign_in: &sign_in,
                id: Some(answer.id),
                why: &why,
            };
            say(format_args!("{audit}"));
            Err(why)
        }
    };
    drop(ledger);
    // A network process gone is told nothing: the run ends with it.
    let _ = leasing.tell_sign_in(&signin::Answered::Captured {
        id: answer.id,
        result,
    });
    Ok(())
}

/// The orchestrator's answer to a sign-in's refresh (`REFRESHED`, D242 step 8), relayed to the
/// network process that asked it: the token response its CLI is to have, which that process
/// gives the sign-in's placeholders whatever it holds (Q2: they stay), the access token's held
/// lease let go of here first, as its value rotated; or why not. Its decoded copy is wiped as it
/// drops. One to no refresh the run awaits breaks the channel's contract.
fn refreshed(leasing: &Leasing<'_>, payload: &[u8]) -> Result<(), String> {
    let answer = wire::RefreshAnswer::decode(payload).map_err(|e| e.to_string())?;
    let mut ledger = lock(&leasing.ledger);
    let Some(index) = ledger.signed_in(answer.id, Signing::Refresh)? else {
        return Ok(());
    };
    let sign_in = ledger
        .sign_ins
        .get(index)
        .map(|s| s.sign_in.clone())
        .unwrap_or_default();
    let result = match answer.result {
        Ok(mut body) => {
            ledger.refreshed(index);
            say(format_args!(
                "{}",
                Audit::Refreshed {
                    sign_in: &sign_in,
                    id: answer.id
                }
            ));
            Ok(zeroize::Zeroizing::new(std::mem::take(&mut body.0)))
        }
        Err(refusal) => {
            let why = declined(refusal);
            let audit = Audit::Declined {
                sign_in: &sign_in,
                id: Some(answer.id),
                why: &why,
            };
            say(format_args!("{audit}"));
            Err(why)
        }
    };
    drop(ledger);
    let _ = leasing.tell_sign_in(&signin::Answered::Refreshed {
        id: answer.id,
        result,
    });
    Ok(())
}

/// Says the run's end (`ENDED`), whatever ended it; where it has leases, every one released
/// first and the orchestrator told (`RELEASED`), which releases each its broker gave (D242).
/// The network process holds no value past this: it wipes its own as it ends with the VM.
fn end_run(answers: &Answers, leasing: Option<&Leasing<'_>>, end: &wire::RunEnded) {
    if let Some((held, asked)) = leasing.and_then(|l| lock(&l.ledger).release()) {
        say(format_args!("{}", Audit::Released { held, asked }));
        let _ = answers.send(wire::to_client::RELEASED, &[]);
    }
    if let Ok(payload) = end.encode() {
        let _ = answers.send(wire::to_client::ENDED, &[&payload]);
    }
}

/// How the run's microVM starts (D253), as the daemon starts a run's (daemon.rs `warm_for`):
/// from its image's template where one is saved; else booted, saving it on the way at a name of
/// this process's alone that becomes the template once complete; or booted alone where it has
/// no template (a guest its caller gave, a host whose hypervisor restores no snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Way {
    Boot,
    Save {
        dir: std::path::PathBuf,
        fresh: std::path::PathBuf,
    },
    Restore {
        dir: std::path::PathBuf,
    },
}

/// The name a save of template `dir` is written at until it is complete: this process's alone,
/// as the daemon names its own (`warm_for`), its one VM's.
fn fresh_of(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_extension(format!("new-{}-0", std::process::id()))
}

/// The way a run whose template is `template`, none where it has none, starts: restoring it
/// where it `exists`, else saving it.
fn way_of(template: Option<&std::path::Path>, exists: impl Fn(&std::path::Path) -> bool) -> Way {
    match template {
        None => Way::Boot,
        Some(dir) if exists(dir) => Way::Restore {
            dir: dir.to_path_buf(),
        },
        Some(dir) => Way::Save {
            dir: dir.to_path_buf(),
            fresh: fresh_of(dir),
        },
    }
}

/// What starts a run's microVM (`started`): its VM process launched the way asked, waited for
/// until it is ready, ended where it never is; its template's record written beside it, and
/// its files held open.
trait Machine {
    type Vm;
    type Held;
    /// Its VM process, launched the way `way` says.
    fn launch(&mut self, way: &Way) -> Result<Self::Vm, String>;
    /// Waits until `vm` says it is ready (`READY`): its end, or anything else it says first,
    /// is its failure, in those words.
    fn ready(&mut self, vm: &mut Self::Vm) -> Result<(), String>;
    /// Ends `vm`, which was never ready: killed, and its processes reaped.
    fn end(&mut self, vm: Self::Vm);
    /// Writes template `dir`'s record beside it (`run::Origin`), its microVM having saved it.
    fn record(&mut self, dir: &std::path::Path) -> Result<(), String>;
    /// Holds template `dir`'s files open (`shards_vmm::snapshot::hold`).
    fn hold(&mut self, dir: &std::path::Path) -> Result<Self::Held, String>;
    /// Says `line` on this host's diagnostics.
    fn say(&mut self, line: &str);
}

/// A run's microVM, ready, how it started, and its template's files held ([`started`]).
type StartedVm<M> = (
    <M as Machine>::Vm,
    wire::StartedFrom,
    Option<<M as Machine>::Held>,
);

/// The run's microVM, ready; how it started, which its orchestrator is told (`Started::from`);
/// and its template's files, held open for the run's life where it has a template (D253), as
/// the daemon starts a run's (daemon.rs `warm_for`):
/// - restoring its template, whose files it holds as its VM restores it (refill.rs
///   `start_planned`). One that fails to restore, its VM ending or saying anything else before
///   it is ready, is removed with its record, and the run boots and saves another,
///   `Replaced`. A VM that cannot be launched at all is the host's failure, which saving the
///   template again would not mend: the template stays, and the run fails.
/// - booting and saving its template where none is saved: its record written once its
///   microVM is ready, and the save made the template unless another run's got there first
///   (`run::settle`), whose files it then holds. What is at the template's name and is no
///   snapshot is removed first, as it would keep every save from becoming the template.
/// - booting alone where it has no template.
///
/// A boot that fails is the run's failure (`Cause::Boot`), its save removed.
fn started<M: Machine>(m: &mut M, way: Way) -> Result<StartedVm<M>, No> {
    let boot = |e: String| No(Cause::Boot, e);
    let (dir, fresh, replaced) = match way {
        Way::Boot => {
            let mut vm = m.launch(&Way::Boot).map_err(boot)?;
            if let Err(e) = m.ready(&mut vm) {
                m.end(vm);
                return Err(boot(e));
            }
            return Ok((vm, wire::StartedFrom::Booted, None));
        }
        Way::Restore { dir } => {
            let restore = Way::Restore { dir: dir.clone() };
            let mut vm = m.launch(&restore).map_err(boot)?;
            let held = m.hold(&dir);
            match m.ready(&mut vm) {
                Ok(()) => {
                    let held = match held {
                        Ok(held) => Some(held),
                        Err(e) => {
                            m.say(&format!("{}: its files are not held open: {e}", dir.display()));
                            None
                        }
                    };
                    return Ok((vm, wire::StartedFrom::Restored, held));
                }
                Err(e) => {
                    m.end(vm);
                    drop(held);
                    m.say(&format!(
                        "its template {} does not restore ({e}); removed, and booting instead",
                        dir.display()
                    ));
                    crate::run::Origin::remove(&dir);
                    let _ = std::fs::remove_dir_all(&dir);
                    let fresh = fresh_of(&dir);
                    (dir, fresh, true)
                }
            }
        }
        Way::Save { dir, fresh } => {
            // What is at its name and is no snapshot (a removal cut short leaves its pointer
            // gone first, `snapshot::remove`) would keep every save from becoming the template.
            if dir.exists() && !shards_vmm::snapshot::exists(&dir) {
                m.say(&format!(
                    "{}: no snapshot; removed, and saving it again",
                    dir.display()
                ));
                crate::run::Origin::remove(&dir);
                let _ = std::fs::remove_dir_all(&dir);
            }
            (dir, fresh, false)
        }
    };
    let save = Way::Save {
        dir: dir.clone(),
        fresh: fresh.clone(),
    };
    // A VM never ready makes no template, whatever it wrote: its save is removed
    // (`run::settle_boot`).
    let mut vm = match m.launch(&save) {
        Ok(vm) => vm,
        Err(e) => {
            let _ = crate::run::settle_boot(false, &fresh, &dir, || Ok(()));
            return Err(boot(e));
        }
    };
    if let Err(e) = m.ready(&mut vm) {
        m.end(vm);
        let _ = crate::run::settle_boot(false, &fresh, &dir, || Ok(()));
        return Err(boot(e));
    }
    if let Err(e) = crate::run::settle_boot(true, &fresh, &dir, || m.record(&dir)) {
        m.say(&format!("{}: its record: {e}", dir.display()));
    }
    let held = if shards_vmm::snapshot::exists(&dir) {
        match m.hold(&dir) {
            Ok(held) => Some(held),
            Err(e) => {
                m.say(&format!("{}: its files are not held open: {e}", dir.display()));
                None
            }
        }
    } else {
        None
    };
    let from = if replaced {
        wire::StartedFrom::Replaced
    } else {
        wire::StartedFrom::Saved
    };
    Ok((vm, from, held))
}

/// A save of the run's microVM, as its host holds it (D259, R04): none under way; one its VM
/// was asked for, its answer awaited by `told`; or none to be made, its VM's link closed.
/// Idle to asked as its owner asks; asked to idle as its VM answers, or where it never reached
/// its VM; either to gone as the link closes, which a save awaiting its answer hears as its
/// microVM stopped. One at a time, as the daemon's (D176).
enum Saving {
    Idle,
    Asked(std::sync::mpsc::Sender<Result<(), String>>),
    Gone,
}

/// What its VM said of a save (`SAVED`): empty for saved, else why not.
type SaveSaid = std::sync::mpsc::Receiver<Result<(), String>>;

impl Saving {
    /// A save asked now: what its VM will say of it; refused where one is under way or its VM
    /// is gone.
    fn ask(&mut self) -> Result<SaveSaid, (wire::SaveRefusal, String)> {
        match self {
            Saving::Idle => {
                let (told, said) = std::sync::mpsc::channel();
                *self = Saving::Asked(told);
                Ok(said)
            }
            Saving::Asked(_) => Err((wire::SaveRefusal::State, "a save of it is under way".into())),
            Saving::Gone => Err((wire::SaveRefusal::State, "its microVM has stopped".into())),
        }
    }

    /// Its VM's answer (`SAVED`'s `payload`) told the save asked, which is then done; with
    /// none asked it answers nothing: the VM's word answers its owner's ask alone.
    fn answered(&mut self, payload: &[u8]) {
        if !matches!(self, Saving::Asked(_)) {
            return;
        }
        if let Saving::Asked(told) = std::mem::replace(self, Saving::Idle) {
            let said = if payload.is_empty() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(payload).into_owned())
            };
            let _ = told.send(said);
        }
    }

    /// The save asked never reached its VM: none is under way.
    fn unasked(&mut self) {
        if matches!(self, Saving::Asked(_)) {
            *self = Saving::Idle;
        }
    }

    /// Its VM's link has closed: no save is made from now on, and one awaiting its answer hears
    /// that its microVM stopped.
    fn gone(&mut self) {
        *self = Saving::Gone;
    }
}

/// The run's pause, as its host holds it (D267, R04). Running to pausing as its owner asks a
/// pause, which refuses every ask of its guest from then on; pausing to paused as its VM says
/// every vCPU is held, or back to running where its VM held none; paused to resuming as its
/// owner asks it to go on, and resuming to running once its VM says they go on, or back to
/// paused where it did not. Any to gone as its VM's link closes. A pause of a paused run and a
/// resume of a running one are done already: an owner that lost an answer asks again and is
/// answered the same. Nothing of it waits on its guest: a pause is its VM's alone.
#[derive(Debug)]
enum Pause {
    Running,
    Pausing,
    /// Every vCPU held since `since`.
    Paused {
        since: Instant,
    },
    /// Going on, its VM asked: its guest still answers nothing until its VM says so.
    Resuming {
        since: Instant,
    },
    Gone,
}

/// The run's pause (D267): its state; the time every pause that has ended took; what its init
/// last said of the run's stats, which its stats say while it is paused; and the pauses and
/// resumes asked, applied in their order by one worker at a time.
#[derive(Debug)]
struct Pauses {
    state: Pause,
    ended: Duration,
    last: Option<Vec<u8>>,
    queue: std::collections::VecDeque<wire::PauseAsk>,
    draining: bool,
}

/// What a pause or a resume asked now is to do ([`Pauses::pause`], [`Pauses::resume`]).
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Done already: the run is as asked, its paused time this.
    Done(wire::Paused),
    /// Its VM is to be asked.
    Ask,
    /// Its VM has gone.
    Gone,
}

impl Pauses {
    fn new() -> Pauses {
        Pauses {
            state: Pause::Running,
            ended: Duration::ZERO,
            last: None,
            queue: std::collections::VecDeque::new(),
            draining: false,
        }
    }

    /// The run's paused time at `now`: every pause that has ended, and the one under way so far.
    fn paused(&self, now: Instant) -> wire::Paused {
        match &self.state {
            Pause::Paused { since } | Pause::Resuming { since } => wire::Paused {
                total: self.ended.saturating_add(now.saturating_duration_since(*since)),
                now: true,
            },
            Pause::Running | Pause::Pausing | Pause::Gone => wire::Paused {
                total: self.ended,
                now: false,
            },
        }
    }

    /// Whether its guest runs to answer an ask now: not while it is pausing, paused or going
    /// on. A gone VM's asks go on, to be refused as its VM has stopped.
    fn guest_runs(&self) -> bool {
        matches!(self.state, Pause::Running | Pause::Gone)
    }

    /// A pause asked: to ask its VM where it runs, the pause then under way; done already where
    /// it is paused or going on.
    fn pause(&mut self, now: Instant) -> Step {
        match self.state {
            Pause::Running => {
                self.state = Pause::Pausing;
                Step::Ask
            }
            Pause::Pausing | Pause::Paused { .. } | Pause::Resuming { .. } => Step::Done(self.paused(now)),
            Pause::Gone => Step::Gone,
        }
    }

    /// Its VM held every vCPU at `now`: paused, and its time then; none where its VM went
    /// meanwhile.
    fn held(&mut self, now: Instant) -> Option<wire::Paused> {
        if !matches!(self.state, Pause::Pausing) {
            return None;
        }
        self.state = Pause::Paused { since: now };
        Some(self.paused(now))
    }

    /// Its VM held none: it runs as it did.
    fn unheld(&mut self) {
        if matches!(self.state, Pause::Pausing) {
            self.state = Pause::Running;
        }
    }

    /// A resume asked: to ask its VM where it is paused, then going on; done already where it
    /// runs.
    fn resume(&mut self, now: Instant) -> Step {
        match self.state {
            Pause::Paused { since } => {
                self.state = Pause::Resuming { since };
                Step::Ask
            }
            Pause::Gone => Step::Gone,
            Pause::Running | Pause::Pausing | Pause::Resuming { .. } => Step::Done(self.paused(now)),
        }
    }

    /// Its VM let its vCPUs go on at `now`: running, the pause's time among those ended.
    fn resumed(&mut self, now: Instant) -> Option<wire::Paused> {
        let Pause::Resuming { since } = self.state else {
            return None;
        };
        self.ended = self.ended.saturating_add(now.saturating_duration_since(since));
        self.state = Pause::Running;
        Some(self.paused(now))
    }

    /// Its VM did not let them go on: paused still.
    fn unresumed(&mut self) {
        if let Pause::Resuming { since } = self.state {
            self.state = Pause::Paused { since };
        }
    }

    /// What its init last said of the run's stats, while it is pausing, paused or going on
    /// (`Some(None)`: it said none before its pause); none while its guest runs to say it.
    fn said(&self) -> Option<Option<&[u8]>> {
        match &self.state {
            Pause::Pausing | Pause::Paused { .. } | Pause::Resuming { .. } => Some(self.last.as_deref()),
            Pause::Running | Pause::Gone => None,
        }
    }

    /// Its VM's link closed at `now`: gone, a pause under way ended then.
    fn gone(&mut self, now: Instant) {
        self.ended = self.paused(now).total;
        self.state = Pause::Gone;
    }
}

/// Where a save its owner asks at `to` is made (D259): `to` with its parent's canonical path,
/// so that what follows names no link. Refused unless `to` is absolute and names a directory,
/// its parent is the run's `home` or beneath it, and neither is beneath a directory its guest
/// reaches (`reached`, canonical): what a save holds is its owner's alone, written where no one
/// but its owner, its guest least of all, reaches. Nothing is made here: the save makes it,
/// refusing a name taken.
fn destination(
    to: &std::path::Path,
    home: &std::path::Path,
    reached: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, (wire::SaveRefusal, String)> {
    let refused = |why: String| (wire::SaveRefusal::Destination, why);
    if !to.is_absolute() {
        return Err(refused(format!("{}: not an absolute path", to.display())));
    }
    let (Some(parent), Some(name)) = (to.parent(), to.file_name()) else {
        return Err(refused(format!("{}: names no directory to make", to.display())));
    };
    let parent = std::fs::canonicalize(parent).map_err(|e| refused(format!("{}: {e}", parent.display())))?;
    let home = std::fs::canonicalize(home)
        .map_err(|e| refused(format!("the run's home {}: {e}", home.display())))?;
    if !parent.starts_with(&home) {
        return Err(refused(format!(
            "{}: outside the run's home, {}",
            to.display(),
            home.display()
        )));
    }
    let target = parent.join(name);
    // The home's volumes, whether or not it has any yet: a save made where they are to be
    // would hold every later run's.
    let volumes = home.join(crate::volumes::ROOT);
    for shared in reached.iter().chain(std::iter::once(&volumes)) {
        let how = if target == *shared {
            "at"
        } else if target.starts_with(shared) {
            "beneath"
        } else if shared.starts_with(&target) {
            "holding"
        } else {
            continue;
        };
        return Err(refused(format!(
            "{}: {how} {}, which a guest reaches",
            to.display(),
            shared.display()
        )));
    }
    Ok(target)
}

/// The directory a save is made in, `target` ([`destination`]'s), made now: its user's alone
/// (`os::owner_only_dir`), and refused where anything is there already, so that a save writes
/// into nothing it did not make.
fn made(target: &std::path::Path) -> Result<(), (wire::SaveRefusal, String)> {
    crate::os::owner_only_dir(&mut std::fs::DirBuilder::new())
        .create(target)
        .map_err(|e| {
            let why = match e.kind() {
                io::ErrorKind::AlreadyExists => "something is there already".to_string(),
                _ => e.to_string(),
            };
            (
                wire::SaveRefusal::Destination,
                format!("{}: {why}", target.display()),
            )
        })
}

mod hosted {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::io::{self, Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Instant;

    use shards_abi::run::{self as protocol, managed};
    use shards_api::image::wire::{self, Cause, Channel, RunEnd};
    use shards_ipc::Stream;
    use shards_ipc::device::Refusal;
    use shards_ipc::fd::{AsFd as _, BorrowedFd, OwnedFd};
    use shards_ipc::kind;

    use super::{Answers, No, no};
    use crate::daemon::ends::Ends;
    use crate::daemon::managed::{Charge, execs, mcp, members};
    use crate::daemon::publish;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The run, prepared: its spec, its members, its ports bound, its policy, its
    /// directories open, and what its VM boots.
    struct Plan {
        prepared: crate::run::Prepared,
        members: Vec<wire::MemberInfo>,
        listeners: Vec<publish::Listener>,
        egress: Option<Vec<u8>>,
        /// The most it could ever be granted past its microVM, its image's grants as its network
        /// process takes them (`NET_POLICY`, D273), where its image grants any.
        ceiling: Option<Vec<u8>>,
        /// Its grants past its microVM as its request keeps them, written out whole (D273).
        narrowing: wire::Narrowing,
        /// Each member's ceiling past its microVM, by its index (D278).
        member_ceilings: Vec<(u32, protocol::egress::Grants)>,
        /// Its credential grants as its network process takes them (`NET_CREDENTIALS`), where
        /// it has any, or sign-ins.
        credentials: Option<Vec<u8>>,
        /// Its sign-in grants as its network process takes them (`NET_SIGN_INS`, D242 step 8),
        /// where it has any.
        sign_ins: Option<Vec<u8>>,
        /// The references its grants name, the leases its network process may ask (D242).
        granted: Vec<String>,
        /// Its sign-in grants, whose asks its host relays (D242, step 8).
        signing: Vec<wire::SignInGrant>,
        dirs: Vec<(OwnedFd, bool, OsString)>,
        /// The bound each bounded volume's share among them is served with (D281).
        bounds: Vec<crate::volumes::Bounded>,
        /// The named volumes among them, held since they were opened (D281): its live state's
        /// once it is served ([`Live::volumes_held`]).
        volumes: crate::volumes::Holds,
        /// The anonymous volumes its `VOLUME`s made, removed as it ends (`--rm`'s).
        made: Vec<String>,
        bridge: Option<shards_net::bridge::Bridge>,
        /// What its network process is told of its addresses (`NET_ADDRESS`), where its
        /// agents' flows past the microVM go by IPv6 (`past6`).
        address: Option<Vec<u8>>,
        /// The in-VM server's device, where its image is an Agentfile's (D60).
        server: Option<PathBuf>,
        /// Its microVM, made as every run's is, and its template (D253).
        microvm: super::Microvm,
        /// Its managed output's members, labels and active ones, where it has any.
        capture: Option<(u32, Vec<u32>)>,
        /// The members whose stdin its orchestrator writes, by their declared index.
        inputs: Vec<u32>,
        /// Its members with a terminal of their own (D244), by their declared index.
        terminals: Vec<u32>,
        /// Its members with an MCP bridge (D254), by their declared index.
        mcps: Vec<u32>,
        /// Its domains' plug roots (D235), by name (`Users`), as its setup names them.
        roots: Vec<Vec<u8>>,
        /// Its agent-control endpoint's ask (D252), where its orchestrator made one.
        control: Option<wire::ControlAsk>,
        /// The host directories its shares give its guest, canonical: where no save of it is
        /// made (D259).
        reached: Vec<PathBuf>,
        /// Its GPU report's scope, where its orchestrator asked for one (D233).
        gpu_report: Option<[u8; 16]>,
        /// It has a command of its own, which a port no member takes coming in reaches.
        command: bool,
    }

    pub(super) fn run(
        answers: &Answers,
        request: &wire::Request,
        ready_by: Option<u64>,
        input: super::Input,
    ) -> Result<(), No> {
        let mut checking = Checking::begin();
        let home = shards_ipc::home().map_err(no(Cause::Boot))?;
        let (shaped, layering) = shape(request, &home, &mut checking)?;
        let made = shaped.made.clone();
        let plan = plan(request, ready_by, shaped).inspect_err(|_| end(&home, None, &made))?;
        launch_and_serve(answers, input, &home, plan, checking, layering)
    }

    /// The request as `shards run` would have asked it of a daemon: the image, its own
    /// command or none, its published ports, its shares as `-v`, a network where a port is
    /// published (`super::network` gives one where more needs it), and the nesting grant as
    /// `--security-opt
    /// nesting=true` asks for it (D234), with `ready_by`, its deadline as an instant
    /// (`super::deadline`).
    pub(super) fn as_run(request: &wire::Request, ready_by: Option<u64>) -> shards_ipc::Run {
        let publish = request.publish.iter().map(binding).collect();
        // Each as `-v HOST:AT[:MODE]`, its mode `ro` where read-only and `noexec` where its files
        // may not be run (D279).
        let binds = request
            .shares
            .iter()
            .map(|s| {
                let mode: Vec<&str> = [s.read_only.then_some("ro"), (!s.exec).then_some("noexec")]
                    .into_iter()
                    .flatten()
                    .collect();
                if mode.is_empty() {
                    format!("{}:{}", s.host, s.at)
                } else {
                    format!("{}:{}:{}", s.host, s.at, mode.join(","))
                }
            })
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
            // GPUs named (D232) ask for the microVM's GPU as `--gpus 1` asks it, so that
            // `run::prepare` gives it the GPU's guest kernel, DRM's virtio-gpu driver built in
            // (D144), which its VM then boots (`super::microvm`).
            gpus: if request.gpu_devices.is_empty() {
                Vec::new()
            } else {
                vec!["1".into()]
            },
            ready_by,
            remove: true,
            ..Default::default()
        }
    }

    /// A port to publish as `shards run -p` asks it of a daemon: its host address, or none for
    /// every one, and its host port, or none for one the host picks.
    fn binding(p: &wire::Publish) -> shards_ipc::Publish {
        shards_ipc::Publish {
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
        }
    }

    /// What a run's microVM is made of, made before it starts (D253): its image prepared, its
    /// network chosen and its volumes open, its microVM composed. All of it a warm run host
    /// makes of its exemplar before its run asks (D253's pools), from what alone the runs of
    /// one microVM share (`wire::Request::exemplar`): never a member's stages, the run's
    /// deadline or its control ask.
    struct Shaped {
        run: shards_ipc::Run,
        prepared: crate::run::Prepared,
        agentfile: Option<crate::agentfile::Agentfile>,
        labels: Vec<String>,
        active: Vec<u32>,
        egress: Option<Vec<u8>>,
        ceiling: Option<Vec<u8>>,
        credentials: Option<Vec<u8>>,
        sign_ins: Option<Vec<u8>>,
        bridge: Option<shards_net::bridge::Bridge>,
        /// eth0's IPv6 for its agents' flows past the microVM, where it has any (`past6`).
        past6: Option<(std::net::Ipv6Addr, std::net::Ipv6Addr)>,
        points: Vec<crate::volumes::MountPoint>,
        opened: crate::volumes::Opened,
        made: Vec<String>,
        server: Option<PathBuf>,
        microvm: super::Microvm,
        /// Its security's costly parts ([`secure`]), or why not, which its plan says where it
        /// would have made them, after its run's own refusals.
        secured: Result<Secured, No>,
    }

    /// What of a run's security its microVM alone decides (D253), made once of its exemplar:
    /// the filters its setup installs, compiled for its guest's kernel (`setup::filters`), and
    /// its domains' filters where its image is an Agentfile's. All a run's plan made of them
    /// before, each run's own grant entries apart (its deadline), which its plan composes.
    struct Secured {
        filters: crate::setup::Filters,
        domains: Option<[Vec<u8>; 2]>,
    }

    /// [`Secured`] of the microVM `microvm` that `run` runs in, its Agentfile `agentfile`;
    /// refused as its plan refused them: its kernel unread `Boot`, its options or a filter
    /// `Request`, its domains' filters `Boot`. Whether its command has the nesting grant is
    /// its microVM's: the grant asks its run's asking and its image's declaring alone (D234,
    /// `agentfile::nesting`), a run's deadline only its entries; a run refused the grant is
    /// refused by its plan before anything made here is taken.
    fn secure(
        agentfile: Option<&crate::agentfile::Agentfile>,
        run: &shards_ipc::Run,
        microvm: &super::Microvm,
    ) -> Result<Secured, No> {
        let version = crate::guest::version_of(&microvm.cfg.kernel).map_err(no(Cause::Boot))?;
        let granted = matches!(
            crate::agentfile::nesting(agentfile, run),
            Ok(crate::agentfile::Nesting::Granted(_))
        );
        let filters = crate::setup::filters(run, version, granted).map_err(no(Cause::Request))?;
        let domains = agentfile
            .map(|_| crate::setup::domain_seccomp(version))
            .transpose()
            .map_err(no(Cause::Boot))?;
        Ok(Secured { filters, domains })
    }

    /// Its microVM, shaped of `request` ([`Shaped`]); and its writable layer, its making begun
    /// beside the rest of its start once its way is known ([`Layering`]).
    fn shape(
        request: &wire::Request,
        home: &Path,
        checking: &mut Checking,
    ) -> Result<(Shaped, Layering), No> {
        let mut run = as_run(request, None);
        let say = |line: &str| {
            let _ = writeln!(io::stderr(), "{line}");
        };
        // A host that gives a microVM no GPU (`run::GIVES_GPU`) refuses GPUs named, before
        // anything of the run is made.
        if !request.gpu_devices.is_empty() && !crate::run::GIVES_GPU {
            return Err(No(
                Cause::Unsupported,
                "this host gives a microVM no GPU (D144, D201, D202)".into(),
            ));
        }
        let cancel = shards_registry::http::Cancel::new();
        let mut prepared = crate::run::prepare(&run, home, &say, &cancel).map_err(no(Cause::Image))?;
        super::step(format_args!("its image prepared"));
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
        super::ports_granted(&request.publish, agentfile.as_ref(), request.command.is_some())?;
        let egress = super::network(agentfile.as_ref(), request, &mut run)?;
        // The most the run may ever reach past its microVM (D273, D278): its image's grants with
        // every member's ceiling.
        let ceilings: Vec<&wire::Reach> = request.egress_ceiling.iter().map(|c| &c.reach).collect();
        let ceiling = super::policy_with(
            agentfile.as_ref().map(|a| &a.grants),
            &wire::Narrowing::default(),
            &ceilings,
        )?;
        let sign_ins = super::sign_ins(&request.sign_ins, &request.credentials, egress.as_deref())?;
        let credentials = super::credentials(
            &request.credentials,
            &request.upstream_roots,
            egress.as_deref(),
            sign_ins.is_some(),
        )?;
        let bridge = if run.network == "bridge" {
            checking.networked();
            Some(
                shards_net::bridge::elected_here(&mut |note| say(&note))
                    .ok_or_else(|| No(Cause::Boot, shards_net::bridge::NO_SUBNET.into()))?,
            )
        } else {
            None
        };
        let past6 = super::past6(agentfile.as_ref(), &run, home);
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
        let made: Vec<String> = points
            .iter()
            .filter(|p| p.kind == "volume" && store.get(&p.name).is_some_and(|v| v.anonymous))
            .map(|p| p.name.clone())
            .collect();
        // What it made goes where what follows fails, as `--rm`'s volumes go.
        let rest = || -> Result<_, No> {
            let opened = crate::volumes::open_for(&points, true, &store, &crate::volumes::Access::default())
                .map_err(no(Cause::Boot))?;
            // The in-VM server's device after its root filesystem, where its image is an
            // Agentfile's (D60).
            let server = match &agentfile {
                Some(_) => Some(crate::guest::server_device(home).map_err(no(Cause::Boot))?),
                None => None,
            };
            // Its microVM, made as every run's is, the daemon's alike, so that its template is
            // the one a daemon's run of the same shape saves and restores (D253): its size, the
            // home's CPU baseline (D176 part 6), the host's memory to plug, the in-VM server's
            // and the emulator's devices, a slot and a DAX window for each share (D38, D189), PCI
            // slots (D214) and its bridge.
            let cpu_baseline = crate::daemon::home_baseline(home);
            let share_window_mib =
                crate::daemon::share_window_mib(&|line| say(line)).map_err(no(Cause::Boot))?;
            let shares = u32::try_from(opened.dirs.len())
                .map_err(|_| No(Cause::Request, "more shares than a microVM counts".into()))?;
            let shape = crate::run::Shape {
                size: prepared.size,
                cpu_baseline: &cpu_baseline,
                host_memory: crate::build::host_memory(),
                server: server.as_deref(),
                emulator: prepared.emulator.as_ref(),
                shares,
                share_window_mib,
                max_map_count: crate::daemon::max_map_count(),
                pci_slots: super::image_slots(agentfile.as_ref(), &request.plug_roots),
                bridge: bridge.as_ref(),
            };
            let microvm = super::microvm(
                home,
                &prepared,
                &shape,
                &request.gpu_devices,
                shards_vmm::vm::restores_here(),
            );
            if microvm.cfg.init.is_none() {
                return Err(No(Cause::Boot, "no init to boot".into()));
            }
            Ok((opened, server, microvm))
        };
        let (opened, server, microvm) = rest().inspect_err(|_| end(home, None, &made))?;
        super::step(format_args!("its microVM shaped"));
        let template = microvm.template.as_ref().map(|t| t.dir.as_path());
        let layering = Layering::begin(
            home,
            super::way_of(template, shards_vmm::snapshot::exists),
            prepared.layer,
        );
        // Before its run asks, where its host is warm (D253): never on a warm run's way.
        let secured = secure(agentfile.as_ref(), &run, &microvm);
        super::step(format_args!(
            "its security made: its guest's kernel read, its filters compiled"
        ));
        let shaped = Shaped {
            run,
            prepared,
            agentfile,
            labels,
            active,
            egress,
            ceiling,
            credentials,
            sign_ins,
            bridge,
            past6,
            points,
            opened,
            made,
            server,
            microvm,
            secured,
        };
        Ok((shaped, layering))
    }

    /// The run `request`, its deadline `ready_by`, of its microVM `shaped`: its members'
    /// stages, its published ports bound, its nesting grant decided, its setup and spec, its
    /// security's filters its microVM's own (`Shaped::secured`).
    fn plan(request: &wire::Request, ready_by: Option<u64>, shaped: Shaped) -> Result<Plan, No> {
        let Shaped {
            mut run,
            mut prepared,
            agentfile,
            labels,
            active,
            egress,
            ceiling,
            credentials,
            sign_ins,
            bridge,
            past6,
            points,
            mut opened,
            made,
            server,
            microvm,
            secured,
        } = shaped;
        run.ready_by = ready_by;
        let say = |line: &str| {
            let _ = writeln!(io::stderr(), "{line}");
        };
        let members = super::members_of(&labels, &active);
        let staged = super::staged(request, &members)?;
        let inputs = super::inputs(request, &members)?;
        let terminals = super::terminals(request, &members)?;
        let mcps = super::mcps(request, &members)?;
        let mut nesting = super::nesting(agentfile.as_ref(), &run, &say)?;
        // The run's GPUs, its command's to read under the grant (D234's `gpus=`), as its VM
        // process binds them (D232).
        if let crate::agentfile::Nesting::Granted(grant) = &mut nesting {
            grant.gpus = request.gpu_devices.clone();
        }
        // Its published ports, bound now, as dockerd's allocator binds them (D31).
        let (bindings, alone) = publish::wanted(&run, &prepared.exposed);
        if let Some(e) = publish::unsupported(&bindings) {
            return Err(No(Cause::Request, e));
        }
        let bound = publish::bind(&bindings, &alone, None, publish::v6_listenable(), |_, _| {
            publish::InUse::Host
        })
        .map_err(no(Cause::Boot))?;
        super::step(format_args!("its members staged, its ports bound"));
        // Its setup, as the daemon composes a run's (daemon.rs `start`): its mounts, its
        // capabilities beside its domains, its security, its domains' filter, its managed
        // output, what it stages, its members' ends asked for, and its own command or
        // `ATTEND`.
        let (_, _, most) = crate::run::sizes(&run.resources, crate::build::host_memory());
        // The shares it gives members as they start (D277), told in their entries.
        let given = super::share_members(request, &members)?;
        let volumes = agentfile
            .as_ref()
            .map(|a| a.volumes.as_slice())
            .unwrap_or_default();
        super::give_members(&mut opened.mounts, &given, volumes)?;
        let mut setup = crate::setup::setup(&run, &opened.mounts, None, Some(crate::run::shm_bytes(most)))
            .map_err(no(Cause::Request))?;
        if let Some(a) = agentfile.as_ref().filter(|a| a.domains > 0 && a.uplink) {
            setup.retain(|e| !e.starts_with(b"caps="));
            setup.push(format!("caps={}", a.caps(&run)).into_bytes());
        }
        let Secured { filters, domains } = secured?;
        crate::setup::security_setup_with(&mut setup, &run, &filters, None, &[], nesting.grant())
            .map_err(no(Cause::Request))?;
        if let Some(a) = &agentfile {
            setup.extend(domains.into_iter().flatten());
            setup.extend(a.image_env.iter().cloned());
        }
        super::step(format_args!("its setup's security composed"));
        // Its domains' plug roots (D235), as a daemon's run names them: this host's own and
        // each share's top-level directory, beneath which a plug asked of it lies.
        let roots = match &agentfile {
            Some(_) => granted_roots(crate::setup::plug_roots(&points), &request.plug_roots)?,
            None => Vec::new(),
        };
        setup.extend(roots.iter().cloned());
        let roots = roots
            .iter()
            .filter_map(|e| e.strip_prefix(protocol::plug::ROOT_KEY))
            .filter_map(protocol::plug::root_name)
            .map(<[u8]>::to_vec)
            .collect();
        // Each member's ceiling past its microVM (D278), which its init holds its own grants
        // within as the run goes.
        let ceilings = super::ceilings(&request.egress_ceiling, &members)?;
        for (member, grants) in &ceilings {
            setup.push(protocol::egress::entry(
                protocol::egress::CEILING_KEY,
                *member,
                grants,
            ));
        }
        // Grants past the microVM, the image's or a member's ceiling's: the run's own processes
        // kept to eth0's subnet (D59).
        if agentfile.as_ref().is_some_and(|a| a.grants.past()) || !ceilings.is_empty() {
            setup.push(b"confine-eth0".to_vec());
        }
        // Its agents' flows past the microVM by IPv6 (D99): eth0's IPv6 for them alone.
        if let Some(v6) = past6 {
            setup.push(crate::networks::uplink6_entry(v6));
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
            if !inputs.is_empty() {
                let mut named = managed::INPUT_KEY.to_vec();
                for m in &inputs {
                    named.extend_from_slice(&m.to_be_bytes());
                }
                setup.push(named);
            }
            Some((count, active.clone()))
        };
        setup.extend(staged);
        setup.extend(terminals.iter().map(|&m| protocol::members::terminal(m)));
        setup.push(protocol::members::ENDS.to_vec());
        // Each member given an MCP bridge, and the run-wide token its connections are carried
        // by (D254), made here, which its VM process holds them to and its members never see.
        if !mcps.is_empty() {
            let mut token = [0u8; protocol::TOKEN];
            shards_vmm::platform::fill_random(&mut token)
                .map_err(|e| No(Cause::Boot, format!("its MCP connections' token: {e}")))?;
            setup.push(protocol::mcp::token_entry(&token));
            setup.extend(mcps.iter().map(|&m| protocol::mcp::entry(m)));
        }
        // The run's own command's socket directory its controller may reach (D251), held to
        // what shards-api holds it to, as init holds it again.
        if let Some(grant) = request.control.as_ref().and_then(|c| c.sockets.as_ref()) {
            super::socket_grant(grant, request.command.is_some())?;
            setup.push(
                protocol::socket::grant(grant.dir.as_bytes())
                    .map_err(|e| No(Cause::Request, e.to_string()))?,
            );
        }
        prepared.spec.setup = setup;
        // Of its microVM's CPUs, which its spec sizes (`size.0`): not the host's, which a
        // run asking fewer vCPUs than the host has would name past its guest's, and a guest
        // whose workload has the cpuset controller (the nesting grant, D234) refuses.
        let vcpus = usize::try_from(prepared.size.0)
            .map_err(|_| No(Cause::Request, "its vCPUs pass what this host counts".into()))?;
        prepared.spec.cgroup = crate::resources::cgroup(&run.resources, vcpus)
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
        // Each share's host directory as its guest reaches it, which no save is made in
        // (D259); one that is not there now is none it reaches.
        let reached = request
            .shares
            .iter()
            .filter_map(|s| std::fs::canonicalize(&s.host).ok())
            .collect();
        super::step(format_args!("its run planned"));
        let narrowing = super::resolved(agentfile.as_ref().map(|a| &a.grants), &request.egress)?;
        Ok(Plan {
            prepared,
            members,
            listeners: bound.listeners,
            egress,
            ceiling,
            narrowing,
            member_ceilings: ceilings,
            credentials,
            sign_ins,
            granted: request.credentials.iter().map(|g| g.reference.clone()).collect(),
            signing: request.sign_ins.clone(),
            dirs: opened.dirs,
            bounds: opened.bounds,
            volumes: opened.held,
            made,
            address: bridge
                .as_ref()
                .zip(past6)
                .map(|(bridge, v6)| crate::networks::bridge_address(bridge, v6)),
            bridge,
            server,
            microvm,
            capture,
            inputs,
            terminals,
            mcps,
            roots,
            control: request.control.clone(),
            reached,
            gpu_report: request.gpu_report,
            command: request.command.is_some(),
        })
    }

    /// The setup entries of the run's plug roots (D235, D271): its host's own and its shares'
    /// top-level directories (`setup::plug_roots`), then each its orchestrator grants that is
    /// none of those, each checked again as its guest takes one (`plug::root_name`).
    fn granted_roots(mut roots: Vec<Vec<u8>>, granted: &[String]) -> Result<Vec<Vec<u8>>, No> {
        for root in granted {
            if protocol::plug::root_name(root.as_bytes()).is_none() {
                return Err(No(
                    Cause::Request,
                    format!(
                        "{root:?}: no plug root, which is one top-level directory and none of init's own"
                    ),
                ));
            }
            let entry = [protocol::plug::ROOT_KEY, root.as_bytes()].concat();
            if !roots.contains(&entry) {
                roots.push(entry);
            }
        }
        Ok(roots)
    }

    /// The run's processes, started: its network's, its shares', its VM's; how its VM
    /// started, and its template's files, held open for the run's life (D253).
    struct Launched {
        vm: shards_ipc::Child,
        link: Stream,
        net: Option<(crate::netproc::NetProcess, crate::netproc::Control)>,
        shares: Option<Shares>,
        from: wire::StartedFrom,
        held: Option<shards_vmm::snapshot::Held>,
        /// Its writable layer's file's name (D190), its VM's file once it is ready: let go of
        /// once its run has said its start (`STARTED`), off its caller's way; a warm host's as
        /// its VM is ready, before any run asks.
        layer: Option<crate::daemon::LayerFile>,
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
        /// The pipe it tells what its host changes on (D235), until its guest's NOTICE
        /// built-in is given it.
        notices: Option<OwnedFd>,
    }

    impl Shares {
        /// Its end, its VM's gone: what of it this process holds let go of, the last of its
        /// streams where the VM never took them (a run refused before `TAKEN`), then waited
        /// for as a network process is, while it goes on with its end, and ended once it does
        /// not (`netproc::reap`, the home's `record` of this host's ends). Waited for with its
        /// door still held here, it never went (CI run 43230466376,
        /// `a_share_at_a_granted_volume_reaches_the_harness`, this process in `do_wait`).
        fn end(self, record: &Ends) {
            let Shares {
                child,
                #[cfg(unix)]
                door,
                #[cfg(windows)]
                ends,
                notices,
            } = self;
            drop(notices);
            #[cfg(unix)]
            drop(door);
            #[cfg(windows)]
            drop(ends);
            crate::netproc::reap(&child, record);
        }
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

    /// What a run's VM is launched from (`launch`): its microVM and what of the run its
    /// processes are handed, a run's plan's or its microVM's shape's alike (D253's pools).
    struct Launch<'a> {
        prepared: &'a crate::run::Prepared,
        dirs: &'a [(OwnedFd, bool, OsString)],
        bounds: &'a [crate::volumes::Bounded],
        bridge: Option<&'a shards_net::bridge::Bridge>,
        /// Whether its network process makes its run's credential grants (D242).
        credentials: bool,
        server: Option<&'a Path>,
        microvm: &'a super::Microvm,
        /// Its GPU report's scope (D233): a run's, never a warm host's shape's, whose VM starts
        /// before its run names one.
        gpu_report: Option<[u8; 16]>,
    }

    impl Plan {
        fn launch(&self) -> Launch<'_> {
            Launch {
                prepared: &self.prepared,
                dirs: &self.dirs,
                bounds: &self.bounds,
                bridge: self.bridge.as_ref(),
                credentials: self.credentials.is_some(),
                server: self.server.as_deref(),
                microvm: &self.microvm,
                gpu_report: self.gpu_report,
            }
        }
    }

    impl Shaped {
        fn launch(&self) -> Launch<'_> {
            Launch {
                prepared: &self.prepared,
                dirs: &self.opened.dirs,
                bounds: &self.opened.bounds,
                bridge: self.bridge.as_ref(),
                credentials: self.credentials.is_some(),
                server: self.server.as_deref(),
                microvm: &self.microvm,
                gpu_report: None,
            }
        }
    }

    /// Its VM process's binary, checked on a thread of its own from the run host's first step,
    /// beside its run's making, which needs nothing of it: `helpers::vm` reads the binary whole
    /// and hashes it as a process first starts it (helpers.rs `checked`, runs-10), and a run
    /// host is a process of one run, so every run's start paid it before its launch. Its
    /// network process's binary likewise, once its run is found to have a network
    /// ([`networked`](Self::networked)): checked on the same thread after the VM's, and done
    /// before its VM is launched, as `taken` waits for both, so that the network process's start
    /// (`netproc::start`, before its VM's) finds it checked (helpers.rs `CHECKED`) and reads it
    /// no more. Taken as its VM is launched; checked there where no thread could be started.
    struct Checking {
        thread: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
        /// Tells its thread that its run has a network; untold as it is taken, none checked.
        network: Option<std::sync::mpsc::Sender<()>>,
        networked: bool,
    }

    impl Checking {
        fn begin() -> Checking {
            let (tell, told) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::Builder::new()
                .name("api-run binaries".into())
                .spawn(move || {
                    let vm = crate::helpers::vm();
                    // A network process's that cannot be checked is refused as its process
                    // starts, which checks it again (`helpers::program`).
                    if told.recv().is_ok() {
                        let _ = crate::helpers::net();
                    }
                    vm
                })
                .ok();
            Checking {
                network: thread.as_ref().map(|_| tell),
                thread,
                networked: false,
            }
        }

        /// Its run has a network: its network process's binary is checked too.
        fn networked(&mut self) {
            self.networked = true;
            if let Some(tell) = self.network.take() {
                let _ = tell.send(());
            }
        }

        /// The VM's binary checked, or why not; and whether its network process's was too,
        /// done by now.
        fn taken(self) -> (Result<PathBuf, String>, bool) {
            let Checking {
                thread,
                network,
                networked,
            } = self;
            drop(network);
            let vm = match thread {
                Some(checking) => checking
                    .join()
                    .unwrap_or_else(|_| Err("its VM's binary: its check ended unanswered".into())),
                None => crate::helpers::vm(),
            };
            (vm, networked)
        }
    }

    /// Its writable layer's file (D190), made on a thread of its own from the moment its
    /// microVM is shaped, beside the rest of its start up to its VM's launch, which needs
    /// nothing of it, so that its making, file system work alone, is off that way: for the way
    /// its microVM was then to start ([`super::way_of`]), as a launch that way makes one
    /// ([`layer_for`]). Its first launch takes it where that launch is of the same way. A
    /// launch of another way (its template saved or removed meanwhile, or its restore failed)
    /// makes its own, as one does where no thread could be started; the thread of one not
    /// taken is let go of, and its file goes as that thread ends, its result dropped with it
    /// (std's thread `Packet`), or, where its host ends first, by its orchestrator's hand
    /// (D253's `clear_layers`).
    enum Layering {
        /// On its thread, made for a start of `way`.
        Making {
            way: super::Way,
            thread: std::thread::JoinHandle<Result<Option<crate::daemon::LayerFile>, String>>,
        },
        /// None to take: taken, let go of, or no thread started for it.
        Gone,
    }

    impl Layering {
        /// Its making begun on a thread of its own, for a start of way `way` in `home` whose
        /// layer holds `holds` bytes where it boots; [`Gone`](Self::Gone) where no thread could
        /// be started.
        fn begin(home: &Path, way: super::Way, holds: u64) -> Layering {
            let (home, making) = (home.to_path_buf(), way.clone());
            let thread = std::thread::Builder::new()
                .name("api-run layer".into())
                .spawn(move || {
                    let began = Instant::now();
                    let layer = layer_for(&home, &making, holds);
                    if let Ok(made) = &layer {
                        super::step(format_args!(
                            "its writable layer {} on its own thread, in {} us",
                            layer_said(made.as_ref()),
                            began.elapsed().as_micros()
                        ));
                    }
                    layer
                });
            match thread {
                Ok(thread) => Layering::Making { way, thread },
                Err(_) => Layering::Gone,
            }
        }

        /// Its layer for a launch of way `way`, taken once: what its thread made, waited for,
        /// where it was made for that way; else none, its thread let go of, for its launch to
        /// make its own.
        fn taken(&mut self, way: &super::Way) -> Option<Result<Option<crate::daemon::LayerFile>, String>> {
            match std::mem::replace(self, Layering::Gone) {
                Layering::Making {
                    way: made_for,
                    thread,
                } if made_for == *way => Some(
                    thread
                        .join()
                        .unwrap_or_else(|_| Err("its writable layer: its making ended unanswered".into())),
                ),
                _ => None,
            }
        }
    }

    /// The writable layer's file a start of way `way` is given (D190): made empty, holding
    /// `holds` bytes, for a boot or a save; for a restore, a clone of its template's where its
    /// template keeps one, else none.
    fn layer_for(
        home: &Path,
        way: &super::Way,
        holds: u64,
    ) -> Result<Option<crate::daemon::LayerFile>, String> {
        let dir = match way {
            super::Way::Boot | super::Way::Save { .. } => {
                return crate::daemon::LayerFile::made(home, holds).map(Some);
            }
            super::Way::Restore { dir } => dir,
        };
        let layered = shards_vmm::snapshot::has_layer(dir)
            .map_err(|e| super::say(format_args!("reading the template {}: {e}", dir.display())))
            .unwrap_or(false);
        if !layered {
            return Ok(None);
        }
        let generation = shards_vmm::snapshot::generation(dir)?;
        crate::daemon::LayerFile::for_restore(home, &generation).map(Some)
    }

    /// What a writable layer is, as a start's steps say it: a clone of its template's, made,
    /// or none.
    fn layer_said(layer: Option<&crate::daemon::LayerFile>) -> &'static str {
        match layer {
            Some(l) if l.cloned_of().is_some() => "cloned",
            Some(_) => "made",
            None => "none",
        }
    }

    /// The run's processes, started as the daemon starts a run's VM (launch.rs): its share
    /// process, ended where its VM does not start; then its network process, denying all
    /// until its policy comes, and its VM, the way its template says (D253, `super::started`):
    /// restoring it, or booting and saving it, one that does not restore replaced. On Windows
    /// each is made in a job of this process's that ends them as it ends (D141, D197). Where
    /// it asks for its GPU report (D233), `reported` is what its last VM wrote of it: the
    /// ready one's, or the one's whose start failed last. Its VM process's binary is
    /// `checking`'s, checked beside its run's making, and its writable layer `layering`'s, made
    /// beside it.
    fn launch(
        home: &Path,
        plan: &Launch<'_>,
        checking: Checking,
        layering: Layering,
        reported: &mut Option<Vec<u8>>,
    ) -> Result<Launched, No> {
        let boot = |e: String| No(Cause::Boot, e);
        // Made before any process of the run's, and held until each is reaped.
        #[cfg(windows)]
        let job = shards_ipc::GroupJob::ending_with_this_process()
            .map_err(|e| boot(format!("the run's job: {e}")))?;
        let ends = Ends::kept_in(home);
        let tags = run_tags(plan.dirs.len());
        #[cfg(unix)]
        let shares = (!plan.dirs.is_empty())
            .then(|| start_shares(home, plan.dirs, plan.bounds, Some(&tags)))
            .transpose()
            .map_err(boot)?;
        #[cfg(windows)]
        let shares = (!plan.dirs.is_empty())
            .then(|| start_shares(home, plan.dirs, plan.bounds, &job, Some(&tags)))
            .transpose()
            .map_err(boot)?;
        if let Some(shares) = &shares {
            super::step(format_args!("its share process {} started", shares.child.id()));
        }
        let (vm, networked) = checking.taken();
        super::step(format_args!(
            "its VM's binary checked{}",
            if networked {
                ", its network process's too"
            } else {
                ""
            }
        ));
        let started = vm.map_err(boot).and_then(|vm| {
            let mut host = Host {
                home,
                plan,
                vm: &vm,
                ends: &ends,
                layering,
                #[cfg(windows)]
                job: &job,
                reported: None,
            };
            let template = plan.microvm.template.as_ref().map(|t| t.dir.as_path());
            let started = super::started(&mut host, super::way_of(template, shards_vmm::snapshot::exists));
            *reported = host.reported.take();
            super::step(format_args!("its VM's start settled"));
            started
        });
        let ((launched, link, layer, _), from, held) = match started {
            Ok(started) => started,
            Err(e) => {
                if let Some(shares) = shares {
                    shares.end(&ends);
                }
                return Err(e);
            }
        };
        Ok(Launched {
            vm: launched.vm,
            link,
            net: launched.net,
            shares,
            from,
            held,
            layer,
            #[cfg(windows)]
            job,
        })
    }

    /// What starts the run's VM (`super::Machine`): its launch through the one seam (R12),
    /// its words every run's microVM's (`run::boot_args`, `run::restore_args`) with its
    /// writable layer's file (D190); its template's record and files (D253).
    struct Host<'a> {
        home: &'a Path,
        plan: &'a Launch<'a>,
        vm: &'a Path,
        ends: &'a Ends,
        /// Its writable layer, made beside its run's making for its first launch's way.
        layering: Layering,
        #[cfg(windows)]
        job: &'a shards_ipc::GroupJob,
        /// What its last VM wrote of its GPU report (D233), read once that VM was ready or
        /// ended.
        reported: Option<Vec<u8>>,
    }

    /// What a VM wrote of its GPU report (D232, D233), its pipe read to its end, which the VM's
    /// close of its one writer is (vm_run.rs `write_report`, before its guest runs): at most a
    /// byte past what a report of `gpus` GPUs holds, so that its orchestrator refuses a longer
    /// one (`Count`), as a managed launch's client reads its own, with no more of it read.
    pub(super) fn read_report(from: io::PipeReader, gpus: usize) -> Vec<u8> {
        let most = shards_abi::gpu::report_bytes(gpus).map_or(0, |bytes| bytes.saturating_add(1));
        let mut frame = Vec::new();
        if let Err(e) = from
            .take(u64::try_from(most).unwrap_or(u64::MAX))
            .read_to_end(&mut frame)
        {
            super::say(format_args!("its VM's GPU report: {e}"));
        }
        frame
    }

    impl super::Machine for Host<'_> {
        /// Its VM, its link, its writable layer's file until its run says its start, and its GPU
        /// report's pipe until it is read (D233).
        type Vm = (
            crate::launch::Launched,
            Stream,
            Option<crate::daemon::LayerFile>,
            Option<io::PipeReader>,
        );
        type Held = shards_vmm::snapshot::Held;

        fn launch(&mut self, way: &super::Way) -> Result<Self::Vm, String> {
            self.reported = None;
            let plan = self.plan;
            let cfg = &plan.microvm.cfg;
            let rootfs = &plan.prepared.rootfs;
            // A network device where its run has a bridge, or, restoring, where its template
            // has one, as the daemon's warm VMs are given theirs (refill.rs `start_planned`);
            // a MAC of its own each time, which its guest takes as it resumes (D31). A template
            // that cannot be read gets none: its VM then fails to restore it, as any broken
            // template's does, and it is replaced.
            let wired = match way {
                super::Way::Restore { dir } => shards_vmm::snapshot::net(dir)
                    .map_err(|e| super::say(format_args!("reading the template {}: {e}", dir.display())))
                    .is_ok_and(|net| net.is_some()),
                _ => true,
            };
            let network = match (plan.bridge, wired) {
                (Some(bridge), true) => Some(crate::launch::Network {
                    policy: shards_net::Policy::DenyAll,
                    mac: shards_net::random_mac().map_err(|e| format!("a guest's MAC: {e}"))?,
                    bridge,
                    credentials: plan.credentials,
                }),
                _ => None,
            };
            super::step(format_args!("its network decided"));
            // Its writable layer (D190): the one made beside its run's making for this way
            // (`Layering`), else made now.
            let (layer, beside) = match self.layering.taken(way) {
                Some(made) => (made?, ", taken from its thread"),
                None => (layer_for(self.home, way, plan.prepared.layer)?, ""),
            };
            super::step(format_args!(
                "its writable layer {}{beside}",
                layer_said(layer.as_ref())
            ));
            let mut args = match way {
                super::Way::Boot | super::Way::Save { .. } => {
                    let save = match way {
                        super::Way::Save { fresh, .. } => Some(fresh.as_path()),
                        _ => None,
                    };
                    crate::run::boot_args(cfg, rootfs, save, crate::launch::LINK)
                }
                super::Way::Restore { dir } => {
                    // Its backing files as the VM saving its template recorded them: resolved.
                    let rootfs = crate::daemon::resolved(rootfs);
                    let server = plan.server.map(crate::daemon::resolved);
                    let emulator = plan
                        .prepared
                        .emulator
                        .as_ref()
                        .map(|e| crate::daemon::resolved(&e.root));
                    let backings = crate::run::Backings {
                        rootfs: Some(&rootfs),
                        server: server.as_deref(),
                        emulator: emulator.as_deref(),
                    };
                    crate::run::restore_args(dir, &backings, cfg.gpu, crate::launch::LINK)
                }
            };
            args.extend(layer.iter().flat_map(crate::daemon::LayerFile::args));
            let argv: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
            // Its GPU report's pipe (D233), its write end the VM's alone once it is launched.
            let (report, reader) = match plan.gpu_report {
                Some(scope) => {
                    let (reader, to) = io::pipe().map_err(|e| format!("its GPU report's pipe: {e}"))?;
                    (Some(crate::launch::Report { scope, to }), Some(reader))
                }
                None => (None, None),
            };
            super::step(format_args!("launching its VM"));
            let (launched, link) = crate::launch::run_reported(
                crate::launch::Plan {
                    args: &argv,
                    vm: self.vm,
                    network,
                    ends: self.ends,
                    #[cfg(unix)]
                    group: None,
                    #[cfg(windows)]
                    group: Some(self.job),
                    #[cfg(target_os = "macos")]
                    grants: crate::launch::Grants::Answered,
                },
                report,
            )?;
            Ok((launched, link, layer, reader))
        }

        fn ready(&mut self, vm: &mut Self::Vm) -> Result<(), String> {
            let said = match shards_ipc::recv(&vm.1) {
                Ok(Some(m)) if m.kind == kind::READY => {
                    super::step(format_args!("its VM said READY"));
                    Ok(())
                }
                Ok(Some(m)) => Err(format!("the VM said message kind {} before it was ready", m.kind)),
                Ok(None) => Err("the VM ended before it was ready".into()),
                Err(e) => Err(format!("the VM was not ready: {e}")),
            };
            // Its writable layer's file is its VM's once it is ready or has failed (D190): one
            // that failed lets its name go as it ends (`end`); a ready one's goes once its run
            // has said its start (`Launched::layer`), its unlink off the run's way.
            // A ready VM's GPU report was written, and its writer closed, before its guest ran
            // (D233): read now, at once. One not ready is read once it has ended (`end`).
            if said.is_ok()
                && let Some(report) = vm.3.take()
            {
                self.reported = Some(read_report(report, self.plan.microvm.cfg.gpu_devices.len()));
            }
            said
        }

        fn end(&mut self, vm: Self::Vm) {
            let (launched, link, layer, report) = vm;
            drop(link);
            launched.end(self.ends);
            drop(layer);
            // Its GPU report, what it wrote of it before it ended: a refusal its renderer typed,
            // or none (D232, D233).
            if let Some(report) = report {
                self.reported = Some(read_report(report, self.plan.microvm.cfg.gpu_devices.len()));
            }
        }

        fn record(&mut self, dir: &Path) -> Result<(), String> {
            let plan = self.plan;
            let template = plan
                .microvm
                .template
                .as_ref()
                .ok_or("a run of no template records none")?;
            crate::run::Origin::of(
                &template.guest,
                &plan.prepared.rootfs,
                template.stamp.as_deref(),
                plan.server.is_some(),
                plan.microvm.cfg.gpu,
                plan.prepared.emulator.as_ref().map(|e| e.root.as_path()),
            )
            .write(dir)
        }

        fn hold(&mut self, dir: &Path) -> Result<Self::Held, String> {
            shards_vmm::snapshot::hold(dir)
        }

        fn say(&mut self, line: &str) {
            super::say(format_args!("{line}"));
        }
    }

    /// The share process (share.rs) serving `dirs`, as the daemon starts a run's
    /// (daemon.rs `started_shares`): its connection to it at 3, each directory at 4 + i,
    /// the home's private directory after them. The VM's end of its connection goes with
    /// the run, which hands the VM each share's stream (`SHARE_ENDS`). Each of `bounds`
    /// holds a bounded volume's share to its bound (D281).
    #[cfg(unix)]
    fn start_shares(
        home: &Path,
        dirs: &[(OwnedFd, bool, OsString)],
        bounds: &[crate::volumes::Bounded],
        tags: Option<&[String]>,
    ) -> Result<Shares, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let (door, share) = Stream::pair().map_err(|e| format!("the share process's connection: {e}"))?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("share")];
        // Its notices of what its host changes (D235), on a pipe past its others.
        let told = tags.filter(|t| t.len() == dirs.len()).map(|t| t.join(","));
        let notices = match &told {
            Some(told) => {
                args.extend([OsStr::new(crate::share::NOTICES), OsStr::new(told)]);
                Some(io::pipe().map_err(|e| format!("its notices' pipe: {e}"))?)
            }
            None => None,
        };
        let bounded = crate::share::bound_args(bounds.iter().map(|b| (b.share, b)));
        args.extend(bounded.iter().map(OsString::as_os_str));
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
        if let Some((_, w)) = &notices {
            fds.push((w.as_fd(), number(5usize.saturating_add(dirs.len()))?));
        }
        let child = shards_ipc::spawn_in(&exe, &args, &fds, false, &[])
            .map_err(|e| format!("starting the share process: {e}"))?;
        Ok(Shares {
            child,
            door,
            notices: notices.map(|(r, _)| OwnedFd::from(r)),
        })
    }

    /// The tags of a run's `n` shares, as its VM names their devices (vm::Config,
    /// `shards{i}`).
    fn run_tags(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("shards{i}")).collect()
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
        bounds: &[crate::volumes::Bounded],
        job: &shards_ipc::GroupJob,
        tags: Option<&[String]>,
    ) -> Result<Shares, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let streams = dirs
            .iter()
            .map(|_| crate::os::share_pipe())
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| format!("a share's stream: {e}"))?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("share")];
        // Its notices of what its host changes (D235), on a pipe past its others.
        let told = tags.filter(|t| t.len() == dirs.len()).map(|t| t.join(","));
        let notices = match &told {
            Some(told) => {
                args.extend([OsStr::new(crate::share::NOTICES), OsStr::new(told)]);
                Some(io::pipe().map_err(|e| format!("its notices' pipe: {e}"))?)
            }
            None => None,
        };
        let bounded = crate::share::bound_args(bounds.iter().map(|b| (b.share, b)));
        args.extend(bounded.iter().map(OsString::as_os_str));
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
        if let Some((_, w)) = &notices {
            fds.push((w.as_fd(), number(5usize.saturating_add(n.saturating_mul(2)))?));
        }
        let env = crate::netproc::child_env();
        let child =
            shards_ipc::spawn_detached(Some(job), &exe, &args, &fds, &crate::netproc::env_pairs(&env))
                .map_err(|e| format!("starting the share process: {e}"))?;
        Ok(Shares {
            child,
            ends: streams.into_iter().map(|(_, vm)| vm).collect(),
            notices: notices.map(|(r, _)| OwnedFd::from(r)),
        })
    }

    /// Tells the network process, before the VM has the run, its addresses where its agents
    /// reach past the microVM by IPv6 (`NET_ADDRESS`, D99), its published ports and its
    /// policy, each answered by its own kind in the order asked (daemon/networks.rs
    /// `Asks`); the descriptors kept until answered (M24).
    pub(super) fn tell_net(
        net: &crate::netproc::Control,
        address: Option<&[u8]>,
        ports: &[Port],
        ceiling: Option<&[u8]>,
        granted: Option<&[u8]>,
        sign_ins: Option<&[u8]>,
        credentials: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut asked = Vec::new();
        if let Some(address) = address {
            crate::netproc::send(net, kind::NET_ADDRESS, address, &[])
                .map_err(|e| format!("addresses: {e}"))?;
            asked.push((kind::NET_ADDRESS, "addresses"));
        }
        // Each published port by a number of its own (D269), by which its owner may let it go
        // as the run goes; unanswered, its socket held here meanwhile (M24).
        for port in ports {
            crate::netproc::send(
                net,
                kind::NET_PUBLISHED,
                &port.told(),
                &[crate::netproc::lend(&port.socket)],
            )
            .map_err(|e| format!("published ports: {e}"))?;
        }
        // The most the run could ever be granted past its microVM, its image's grants (D273),
        // which its network process is held to (Landlock's layer on Linux), its grants as the
        // run goes within them.
        if let Some(policy) = ceiling {
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
        // Its grants as its request keeps them (D273), where they are less than its ceiling:
        // nothing past it where it keeps nothing. Before its sign-ins and credential grants,
        // whose hosts its network process checks its grants reach.
        if let Some(ceiling) = ceiling
            && granted != Some(ceiling)
        {
            crate::netproc::send(net, kind::NET_GRANTS, granted.unwrap_or_default(), &[])
                .map_err(|e| format!("its grants: {e}"))?;
            match crate::netproc::recv(net) {
                Ok(Some(m)) if m.kind == kind::NET_GRANTS && m.payload == [0] => {}
                Ok(Some(m)) if m.kind == kind::NET_GRANTS => {
                    return Err(format!(
                        "its grants: {}",
                        String::from_utf8_lossy(m.payload.get(1..).unwrap_or_default())
                    ));
                }
                Ok(_) => return Err("its grants: the network process did not take them".into()),
                Err(e) => return Err(format!("its grants: the network process: {e}")),
            }
        }
        // The run's sign-in grants (D242, step 8), once its policy is taken, which the network
        // process checks them against, and before its credential grants, whose CA names their
        // hosts too.
        if let Some(sign_ins) = sign_ins {
            crate::netproc::send(net, kind::NET_SIGN_INS, sign_ins, &[])
                .map_err(|e| format!("sign-in grants: {e}"))?;
            match crate::netproc::recv(net) {
                Ok(Some(m)) if m.kind == kind::NET_SIGN_INS => {}
                Ok(_) => return Err("sign-in grants: the network process did not take them".into()),
                Err(e) => return Err(format!("sign-in grants: the network process: {e}")),
            }
        }
        // The run's credential grants (D242), once its policy is taken, which the network
        // process checks them against: answered with the run's CA's certificate, made for
        // their hosts alone.
        let Some(grants) = credentials else {
            return Ok(None);
        };
        crate::netproc::send(net, kind::NET_CREDENTIALS, grants, &[])
            .map_err(|e| format!("credential grants: {e}"))?;
        match crate::netproc::recv(net) {
            Ok(Some(m)) if m.kind == kind::NET_CREDENTIALS && !m.payload.is_empty() => Ok(Some(m.payload)),
            Ok(_) => Err("credential grants: the network process did not take them".into()),
            Err(e) => Err(format!("credential grants: the network process: {e}")),
        }
    }

    /// A port the run publishes (D269), its start's or one published since: its host socket,
    /// held here while it is published, as its network process is told it under its number
    /// (`NET_PUBLISHED`, which that process does not answer), by which that process lets it go
    /// (`NET_UNPUBLISHED`, answered once its copies are closed); and the port as its owner is
    /// told it.
    pub(super) struct Port {
        id: u32,
        bound: wire::Bound,
        socket: crate::netproc::Sock,
    }

    impl Port {
        /// What its network process is told of it with its socket: its number, then its guest
        /// port and IP protocol number, as `PUBLISH` names one.
        fn told(&self) -> [u8; 7] {
            let [a, b, c, d] = self.id.to_be_bytes();
            let [hi, lo] = self.bound.guest_port.to_be_bytes();
            [a, b, c, d, hi, lo, self.bound.proto as u8]
        }
    }

    /// The ports the run publishes, in the order published, and the number the next takes:
    /// held through each change of them, so that changes are made one at a time and each is
    /// answered before the next is made.
    #[derive(Default)]
    struct Ports {
        held: Vec<Port>,
        next: u32,
    }

    impl Ports {
        /// Holds `listeners`, each as `bound` names it, in their order, under numbers of their
        /// own (the wire's u32): refused for sockets and ports that do not pair up, and once the
        /// numbers are spent, those not held then closed.
        fn hold(&mut self, listeners: Vec<publish::Listener>, bound: Vec<wire::Bound>) -> Result<(), String> {
            if listeners.len() != bound.len() {
                return Err(format!(
                    "{} sockets for {} ports published",
                    listeners.len(),
                    bound.len()
                ));
            }
            for (listener, bound) in listeners.into_iter().zip(bound) {
                let id = self.next;
                self.next = id
                    .checked_add(1)
                    .ok_or_else(|| "every number a port is told by is spent".to_string())?;
                self.held.push(Port {
                    id,
                    bound,
                    socket: listener.fd,
                });
            }
            Ok(())
        }

        /// Every port it holds, as its owner is told them.
        fn published(&self) -> Vec<wire::Bound> {
            self.held.iter().map(|p| p.bound.clone()).collect()
        }
    }

    /// An IP protocol as a port's words name it.
    fn proto_name(p: wire::Proto) -> &'static str {
        match p {
            wire::Proto::Tcp => "tcp",
            wire::Proto::Udp => "udp",
        }
    }

    /// The run's network changed as its owner asks (D269), in its running microVM in place:
    /// one change at a time under its ports' lock, each answered before the next is made, with
    /// every port the run publishes once it is.
    fn net_change(live: &Live<'_>, ask: wire::NetAsk) {
        let mut ports = lock(&live.ports);
        let result = match ask.change {
            wire::NetChange::Publish(p) => publish_live(live, &mut ports, &p),
            wire::NetChange::Unpublish { proto, host } => unpublish_live(live, &mut ports, proto, host),
            wire::NetChange::Egress(asked) => egress_live(live, &asked).map(|()| Vec::new()),
            wire::NetChange::Member { member, reach } => {
                member_live(live, member, reach.as_ref()).map(|()| Vec::new())
            }
        }
        .map(|changed| wire::NetChanged {
            changed,
            published: ports.published(),
            egress: lock(&live.egress).clone(),
        });
        net_answered(live, ask.id, result);
        drop(ports);
    }

    /// Network change `id`'s answer told its owner.
    /// What came of a restart (D270 slice 3b), told its orchestrator by its ask's id.
    fn restarted(live: &Live<'_>, id: u32, result: Result<u32, (wire::RestartRefusal, String)>) {
        if let Err((cause, why)) = &result {
            super::say(format_args!("a restart refused, {cause:?}: {why}"));
        }
        let payload = wire::RestartAnswer { id, result }.encode().or_else(|e| {
            wire::RestartAnswer {
                id,
                result: Err((wire::RestartRefusal::Failed, format!("its answer: {e}"))),
            }
            .encode()
        });
        if let Ok(payload) = payload {
            let _ = live.answers.send(wire::to_client::RESTARTED, &[&payload]);
        }
    }

    /// What the guest said of a member's start (`MEMBER_STARTED`, D270 slice 3b), kept for the
    /// restart that waits on it.
    ///
    /// A generation started is its member's current from here, on the thread that reads its
    /// VM, before anything it tells after (its end, which may come at once): its ends are its
    /// own, and its member's lanes are its (D238).
    fn started_told(live: &Live<'_>, payload: &[u8]) {
        let Ok(answer) = protocol::members::StartAnswer::decode(payload) else {
            super::say(format_args!("a member's start its guest answered malformed"));
            return;
        };
        let mut generations = lock(&live.generations);
        let current = generations.current.get(&answer.member).copied().unwrap_or(0);
        if answer.refused.is_none() && answer.generation > current {
            generations.current.insert(answer.member, answer.generation);
            if let Some(agent) = &live.agent {
                agent.ends.again(answer.member);
            }
        }
        generations.started.insert(answer.member, answer);
        drop(generations);
        live.generations_moved.notify_all();
    }

    /// A member its guest could not start in its working directory (D277), refused typed to its
    /// orchestrator as its members start (`REFUSED`, `Cause::Workdir`), before the run's end its
    /// guest's words fail; one told malformed said on this host's stderr alone.
    fn unentered(live: &Live<'_>, payload: &[u8]) {
        let Some(told) = protocol::members::Unentered::decode(payload) else {
            super::say(format_args!(
                "a working directory its guest could not enter, told malformed"
            ));
            return;
        };
        if let Ok(refused) = super::workdir_refused(live.members, &told).encode() {
            let _ = live.answers.send(wire::to_client::REFUSED, &[&refused]);
        }
    }

    /// Starts member `ask.member` again (D270 slice 3b): held for its next generation, its
    /// command asked to end (SIGTERM), its generation's end waited for at most its patience and
    /// its generation ended at once past that (`MEMBER_KILL`), then its next started, `ask.args`
    /// after its image's command, in the namespaces, server instance and volumes its
    /// generations keep, its input its own where it has one: the generation started, or why
    /// not. One restart of a member at a time.
    fn restart<'scope, 'env>(
        live: &'env Live<'env>,
        ask: &wire::RestartAsk,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<u32, (wire::RestartRefusal, String)> {
        use wire::RestartRefusal as R;
        let member = ask.member;
        if !live.members.iter().any(|m| m.index == member && m.runs) {
            return Err((R::Unknown, format!("no member {member} runs in this run")));
        }
        let generation = {
            let mut generations = lock(&live.generations);
            if generations.gone {
                return Err((R::Failed, "its microVM has gone".into()));
            }
            let current = generations.current.get(&member).copied().unwrap_or(0);
            // A generation that ended with no restart under way ended its whole member, its
            // server instance with it: nothing of it is kept for a next one.
            if generations.ended.contains(&(member, current)) && !generations.restarting.contains(&member) {
                return Err((
                    R::Unknown,
                    format!("member {member} ended with its generation {current}, and runs no more"),
                ));
            }
            if !generations.restarting.insert(member) {
                return Err((R::Running, format!("member {member}'s restart is under way")));
            }
            generations.started.remove(&member);
            current
        };
        let result = restart_turn(live, ask, generation, scope);
        if result.is_err() {
            // Let go: the end its generation had under the hold, or has after, ends its whole
            // member, as an end with no restart under way does.
            let go = protocol::members::Hold { member, held: false };
            let _ = shards_ipc::send(&lock(&live.link), kind::MEMBER_HOLD, &go.encode(), &[]);
            // Lanes of a next generation that may never run are revoked (D252), not left open.
            if let (Some(stdio), Some(next)) = (live.stdio.get(), generation.checked_add(1)) {
                stdio.abort_generation(member, next);
            }
        }
        lock(&live.generations).restarting.remove(&member);
        result
    }

    /// [`restart`]'s turn, its member held as restarting.
    fn restart_turn<'scope, 'env>(
        live: &'env Live<'env>,
        ask: &wire::RestartAsk,
        generation: u32,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<u32, (wire::RestartRefusal, String)> {
        use wire::RestartRefusal as R;
        let member = ask.member;
        let failed = |why: String| (R::Failed, why);
        let next = generation.checked_add(1).ok_or_else(|| {
            (
                R::Generation,
                format!("member {member} has no generation past {generation}"),
            )
        })?;
        // Held, so that its generation's end keeps its server instance and its namespaces.
        let hold = protocol::members::Hold { member, held: true };
        shards_ipc::send(&lock(&live.link), kind::MEMBER_HOLD, &hold.encode(), &[])
            .map_err(|e| failed(format!("its VM: {e}")))?;
        let ended = |g: &Generations| g.ended.contains(&(member, generation)) || g.gone;
        if !ended(&lock(&live.generations)) {
            let said = builtin(
                live,
                protocol::builtin::SIGNAL,
                protocol::members::signal_args(member, 15),
            )
            .map_err(failed)?;
            // A generation that ended already, between its hold and its signal, is no refusal.
            if said.status != Some(0) && !ended(&lock(&live.generations)) {
                return Err(failed(format!(
                    "its SIGTERM: {}",
                    String::from_utf8_lossy(&said.err).trim()
                )));
            }
        }
        if !wait_generations(live, ask.patience, ended) {
            // Its graceful end outlasted its patience: its generation ended at once, its PID
            // namespace's reaper killed, its server instance and namespaces kept; its end told
            // its orchestrator as killed (`MemberEnded::killed`).
            lock(&live.generations).killed.insert((member, generation));
            let said = builtin(
                live,
                protocol::builtin::MEMBER_KILL,
                protocol::members::kill_args(member),
            )
            .map_err(failed)?;
            if said.status != Some(0) && !ended(&lock(&live.generations)) {
                return Err(failed(format!(
                    "its kill: {}",
                    String::from_utf8_lossy(&said.err).trim()
                )));
            }
            if !wait_generations(live, ask.patience, ended) {
                return Err(failed(format!(
                    "its generation {generation} did not end within its patience, killed or not"
                )));
            }
        }
        if lock(&live.generations).gone {
            return Err(failed("its microVM has gone".into()));
        }
        if !last_output_ended(live, member, generation, ask.patience) {
            return Err(failed(format!(
                "its generation {generation}'s output did not end within its patience: is it read?"
            )));
        }
        // Its input, where it has one: its next generation's own, its feed waiting for the
        // connection its guest dials with the token its VM mints.
        let has_input = lock(&live.feeds).contains_key(&member);
        // Its components as its orchestrator swapped them (D270 slice 4): laid at its path.
        let laid = lock(&live.components).desired.get(&member).cloned();
        let start = protocol::members::Start {
            member,
            generation: next,
            args: ask.args.clone(),
            env: Vec::new(),
            workdir: None,
            components: laid
                .iter()
                .map(|(at, key)| protocol::members::Component {
                    path: at.as_bytes().to_vec(),
                    key: *key,
                })
                .collect(),
            input: None,
        };
        let mut payload = vec![u8::from(has_input)];
        payload.extend_from_slice(&start.encode().map_err(|e| failed(format!("its start: {e}")))?);
        if has_input {
            lock(&live.feeds).insert(
                member,
                Feed {
                    generation: next,
                    ..Feed::default()
                },
            );
        }
        // Its next generation's streams on its endpoint's lanes, where it serves its members'
        // stdio (D252): joined before its start, as its feed is.
        if let Some(stdio) = live.stdio.get() {
            stdio.join(live, member, next, has_input, scope).map_err(failed)?;
        }
        shards_ipc::send(&lock(&live.link), kind::MEMBER_START, &payload, &[])
            .map_err(|e| failed(format!("its VM: {e}")))?;
        let answered = |g: &Generations| g.started.contains_key(&member) || g.gone;
        if !wait_generations(live, ask.patience, answered) {
            return Err(failed(
                "its guest did not answer its start within its patience".into(),
            ));
        }
        let answer = lock(&live.generations).started.remove(&member);
        let Some(answer) = answer else {
            return Err(failed("its microVM has gone".into()));
        };
        if let Some((why, said)) = answer.refused {
            return Err(start_refused(live.members, why, &said));
        }
        if has_input && live.stdio.get().is_none() {
            let started = std::thread::Builder::new()
                .name("api-run input".into())
                .spawn_scoped(scope, move || write_input(live, member));
            if let Err(e) = started {
                let _ = writeln!(io::stderr(), "shards api-run: member {member}'s input: {e}");
                end_input(live, member);
            }
        }
        // What its new generation runs with (D270 slice 4), and the components no generation
        // runs with nor wants let go of.
        {
            let mut c = lock(&live.components);
            match laid {
                Some(laid) => c.held.insert(member, laid),
                None => c.held.remove(&member),
            };
        }
        release_unused(live);
        Ok(next)
    }

    /// What a restart is refused as, its guest having refused its member's next generation `why`
    /// (D270 slice 3b), `said` its answer's bytes: its init's refusal, in its words; one that could
    /// not enter its working directory (D277) named as a first generation's is
    /// (`workdir_refused`): the member as the image declares it, the directory and the errno.
    fn start_refused(
        members: &[wire::MemberInfo],
        why: protocol::members::StartRefusal,
        said: &[u8],
    ) -> (wire::RestartRefusal, String) {
        use protocol::members::StartRefusal as S;
        use wire::RestartRefusal as R;
        let words = || String::from_utf8_lossy(said).into_owned();
        match why {
            S::Unknown => (R::Unknown, words()),
            S::Running => (R::Running, words()),
            S::Generation => (R::Generation, words()),
            S::Unsupported => (R::Unsupported, words()),
            S::Failed => (R::Failed, words()),
            S::Workdir => match protocol::members::Unentered::decode(said) {
                Some(told) => (R::Workdir, super::workdir_refused(members, &told).why),
                None => (
                    R::Failed,
                    "its guest told a working directory it could not enter malformed".into(),
                ),
            },
        }
    }

    /// What came of a swap (D270 slice 4), told its orchestrator by its ask's id.
    fn swapped(live: &Live<'_>, id: u32, result: Result<wire::Swapped, (wire::SwapRefusal, String)>) {
        if let Err((cause, why)) = &result {
            super::say(format_args!("a swap refused, {cause:?}: {why}"));
        }
        let payload = wire::SwapAnswer { id, result }.encode().or_else(|e| {
            wire::SwapAnswer {
                id,
                result: Err((wire::SwapRefusal::Failed, format!("its answer: {e}"))),
            }
            .encode()
        });
        if let Ok(payload) = payload {
            let _ = live.answers.send(wire::to_client::SWAPPED, &[&payload]);
        }
    }

    /// The member running at path `at` (D270 slice 4): its image's agent or harness of that
    /// directory (`/agents/NAME`, `/harness/NAME`, AGENTFILE_ARCH.md §12.1), by its index.
    fn member_at(live: &Live<'_>, at: &str) -> Option<u32> {
        let (kind, name) = match at.strip_prefix("/agents/") {
            Some(name) => (wire::MemberKind::Agent, name),
            None => (wire::MemberKind::Harness, at.strip_prefix("/harness/")?),
        };
        live.members
            .iter()
            .find(|m| m.kind == kind && m.name == name && m.runs)
            .map(|m| m.index)
    }

    /// A component's key (D270 slice 2): its manifest digest's 64 lowercase hex digits, as init
    /// takes one (`components/KEY`).
    fn key_of(digest: &str) -> Result<protocol::members::ComponentKey, (wire::SwapRefusal, String)> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|h| {
                h.len() == protocol::members::KEY_LEN
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(|| {
                (
                    wire::SwapRefusal::Artifact,
                    format!("{digest:?} is no sha256 digest"),
                )
            })?;
        <protocol::members::ComponentKey>::try_from(hex.as_bytes()).map_err(|_| {
            (
                wire::SwapRefusal::Artifact,
                format!("{digest:?} is no sha256 digest"),
            )
        })
    }

    /// The component of `digest` plugged into the run's microVM and mounted beside its writable
    /// layer where it is not already (D270 slices 2 and 4): its EROFS image, checked for path
    /// `at` (`component_image`), plugged as a read-only virtio-pmem function (the VM process's
    /// `PMEM` and its key, which a snapshot keeps it by (slice 5): `DEVFN ADDRESS GPA LEN`),
    /// then mounted by its guest (init's `DEVICE pmem ADDRESS GPA KEY`), its function let go
    /// where its guest refused it. Its key.
    fn plug_component(
        live: &Live<'_>,
        at: &str,
        digest: &str,
    ) -> Result<protocol::members::ComponentKey, (wire::SwapRefusal, String)> {
        use wire::SwapRefusal as R;
        let key = key_of(digest)?;
        if lock(&live.components).plugged.contains_key(&key) {
            return Ok(key);
        }
        let image = (live.component_image)(at, digest)?;
        let step = [&[shards_ipc::device::PMEM][..], &key[..]].concat();
        let said = ask_device(live, &step, &[image.as_fd()])
            .map_err(|refusal| (R::Device, format!("its microVM: {refusal}")))?;
        let words: Vec<&str> = said.split_whitespace().collect();
        let [devfn, address, gpa, _len] = words.as_slice() else {
            return Err((R::Failed, format!("its microVM answered its plug {said:?}")));
        };
        let devfn: u8 = devfn
            .parse()
            .map_err(|_| (R::Failed, format!("its microVM answered its plug {said:?}")))?;
        let mounted = builtin(
            live,
            protocol::builtin::DEVICE,
            vec![
                b"pmem".to_vec(),
                address.as_bytes().to_vec(),
                gpa.as_bytes().to_vec(),
                key.to_vec(),
            ],
        );
        let refused = match mounted {
            Ok(said) if said.status == Some(0) => None,
            Ok(said) => Some(String::from_utf8_lossy(&said.err).trim().to_string()),
            Err(e) => Some(e),
        };
        if let Some(words) = refused {
            let _ = ask_device(live, &[shards_ipc::device::UNPLUG, devfn], &[]);
            return Err((R::Failed, format!("its guest: {words}")));
        }
        lock(&live.components).plugged.insert(
            key,
            PluggedComponent {
                devfn,
                address: (*address).to_string(),
                digest: digest.to_string(),
            },
        );
        Ok(key)
    }

    /// Each component plugged that no member's generation runs with nor its next start lays
    /// let go of (D270 slice 4): unmounted by its guest (init's `DEVICE unpmem`), then its
    /// function unplugged. One its guest still holds stays, said.
    fn release_unused(live: &Live<'_>) {
        let unused: Vec<(protocol::members::ComponentKey, PluggedComponent)> = {
            let c = lock(&live.components);
            c.plugged
                .iter()
                .filter(|(key, _)| !c.desired.values().chain(c.held.values()).any(|(_, k)| k == *key))
                .map(|(key, p)| (*key, p.clone()))
                .collect()
        };
        for (key, p) in unused {
            let unmounted = builtin(
                live,
                protocol::builtin::DEVICE,
                vec![b"unpmem".to_vec(), p.address.as_bytes().to_vec(), key.to_vec()],
            );
            match unmounted {
                Ok(said) if said.status == Some(0) => {}
                Ok(said) => {
                    super::say(format_args!(
                        "component {} kept: {}",
                        p.digest,
                        String::from_utf8_lossy(&said.err).trim()
                    ));
                    continue;
                }
                Err(e) => {
                    super::say(format_args!("component {} kept: {e}", p.digest));
                    continue;
                }
            }
            if let Err(e) = ask_device(live, &[shards_ipc::device::UNPLUG, p.devfn], &[]) {
                super::say(format_args!("component {}'s function: {e}", p.digest));
            }
            lock(&live.components).plugged.remove(&key);
        }
    }

    /// Every member's component as the run holds them (D270 slice 4), in their members' order.
    fn components_of(live: &Live<'_>) -> Vec<wire::ComponentAt> {
        let c = lock(&live.components);
        let digest = |key: &protocol::members::ComponentKey| c.plugged.get(key).map(|p| p.digest.clone());
        let mut members: Vec<u32> = c.desired.keys().chain(c.held.keys()).copied().collect();
        members.sort_unstable();
        members.dedup();
        members
            .into_iter()
            .filter_map(|m| {
                let desired = c.desired.get(&m);
                let held = c.held.get(&m);
                let at = desired.or(held).map(|(at, _)| at.clone())?;
                Some(wire::ComponentAt {
                    at,
                    desired: desired.and_then(|(_, k)| digest(k)),
                    held: held.and_then(|(_, k)| digest(k)),
                })
            })
            .collect()
    }

    /// Swap `ask` (D270 slice 4): its member's component at its path plugged (where it swaps one)
    /// and wanted from its next start, or its image's wanted back; now, its member restarted
    /// with it as a restart restarts it ([`restart`]). What the run's components are after it,
    /// and the generation started now.
    fn swap<'scope, 'env>(
        live: &'env Live<'env>,
        ask: &wire::SwapAsk,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<wire::Swapped, (wire::SwapRefusal, String)> {
        use wire::SwapRefusal as R;
        let (at, digest) = match &ask.change {
            wire::SwapChange::Swap { at, digest } => (at, Some(digest)),
            wire::SwapChange::Restore { at } => (at, None),
        };
        let member = member_at(live, at)
            .ok_or_else(|| (R::Unknown, format!("no member of the image runs at {at}")))?;
        let key = digest.map(|d| plug_component(live, at, d)).transpose()?;
        if key.is_some() {
            super::step(format_args!(
                "swap at {at}: its component's image plugged and mounted"
            ));
        }
        {
            let mut c = lock(&live.components);
            match key {
                Some(key) => c.desired.insert(member, (at.clone(), key)),
                None => c.desired.remove(&member),
            };
        }
        let started = match ask.when {
            wire::SwapWhen::NextStart => None,
            wire::SwapWhen::Now => {
                let again = wire::RestartAsk {
                    id: ask.id,
                    member,
                    args: Vec::new(),
                    patience: ask.patience,
                };
                let generation = restart(live, &again, scope).map_err(|(cause, why)| match cause {
                    wire::RestartRefusal::Running => (R::Running, why),
                    _ => (R::Failed, format!("its member's restart, {cause:?}: {why}")),
                })?;
                super::step(format_args!(
                    "swap at {at}: its member's generation {generation} started"
                ));
                Some((member, generation))
            }
        };
        // A component swapped for its next start, the one before it never run: let go.
        release_unused(live);
        Ok(wire::Swapped {
            components: components_of(live),
            started,
        })
    }

    /// Waits until `done` says its generations are as asked, at most `patience`: whether they
    /// were.
    /// Waits at most `patience` for every channel of member `member`'s generation `generation`
    /// that its guest was given a credit for to end (D270 slice 3b), then seals it: no credit
    /// goes to it after. Whether they all ended. Its records come in the order its init sent
    /// them, and its next generation's channels open on the streams its last's ended on, so
    /// a next started before then would find its last's stream still open on a channel (its
    /// relay refusing the next's open, and every member's output with it). A channel no credit
    /// went to was never opened: nothing of it waits.
    fn last_output_ended(
        live: &Live<'_>,
        member: u32,
        generation: u32,
        patience: std::time::Duration,
    ) -> bool {
        let deadline = Instant::now().checked_add(patience);
        let mut flows = lock(&live.flows);
        loop {
            let open = [Channel::Stdout, Channel::Stderr].into_iter().any(|channel| {
                let key = (member, generation, channel);
                flows.granted.contains_key(&key) && !flows.ended.get(&key).copied().unwrap_or(false)
            });
            if !open {
                flows.sealed.insert((member, generation));
                // A lane on a channel no credit went to (D252) reads nothing more: revoked, as
                // its generation is past.
                if let Some(stdio) = live.stdio.get() {
                    for channel in [Channel::Stdout, Channel::Stderr] {
                        let key = (member, generation, channel);
                        if !flows.granted.contains_key(&key) {
                            stdio.abort(key);
                        }
                    }
                }
                return true;
            }
            let left = match deadline {
                Some(at) => match at.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
                None => std::time::Duration::MAX,
            };
            flows = live
                .flows_moved
                .wait_timeout(flows, left)
                .map(|(f, _)| f)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    fn wait_generations(
        live: &Live<'_>,
        patience: std::time::Duration,
        done: impl Fn(&Generations) -> bool,
    ) -> bool {
        let deadline = Instant::now().checked_add(patience);
        let mut generations = lock(&live.generations);
        while !done(&generations) {
            let left = match deadline {
                Some(at) => match at.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
                None => std::time::Duration::MAX,
            };
            generations = live
                .generations_moved
                .wait_timeout(generations, left)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        true
    }

    fn net_answered(live: &Live<'_>, id: u32, result: Result<wire::NetChanged, (wire::NetRefusal, String)>) {
        if let Err((cause, why)) = &result {
            super::say(format_args!("a network change refused, {cause:?}: {why}"));
        }
        let payload = wire::NetAnswer { id, result }.encode().or_else(|e| {
            wire::NetAnswer {
                id,
                result: Err((wire::NetRefusal::Network, format!("its answer: {e}"))),
            }
            .encode()
        });
        if let Ok(payload) = payload {
            let _ = live.answers.send(wire::to_client::NET_CHANGED, &[&payload]);
        }
    }

    /// What a run with no network device is refused a change of it as.
    fn no_network() -> (wire::NetRefusal, String) {
        (
            wire::NetRefusal::NoNetwork,
            "the run started with nothing published, kept or linked past its microVM, which so has no network device".into(),
        )
    }

    /// `asked` published as the run goes (D269): checked as its start checks its own
    /// (`super::ports_granted`), bound as its start binds them (`publish::bind`), each binding
    /// told its network process under a number of its own and held here. What it bound.
    fn publish_live(
        live: &Live<'_>,
        ports: &mut Ports,
        asked: &wire::Publish,
    ) -> Result<Vec<wire::Bound>, (wire::NetRefusal, String)> {
        use wire::NetRefusal as R;
        let net = live.net.ok_or_else(no_network)?;
        super::ports_granted(std::slice::from_ref(asked), live.agentfile, live.command)
            .map_err(|super::No(_, why)| (R::Widening, why))?;
        let bound = publish::bind(&[binding(asked)], &[], None, publish::v6_listenable(), |_, _| {
            publish::InUse::Host
        })
        .map_err(|why| (R::Bind, why))?;
        let at: Vec<(u16, u8, std::net::SocketAddr)> = bound
            .listeners
            .iter()
            .map(|l| (l.guest_port, l.proto, l.at))
            .collect();
        let receivers = live.agentfile.map(|a| a.receivers.as_slice()).unwrap_or_default();
        let bounds =
            super::published(&at, receivers, live.members).map_err(|super::No(_, why)| (R::Image, why))?;
        let first = ports.held.len();
        if let Err(why) = ports.hold(bound.listeners, bounds) {
            ports.held.truncate(first);
            return Err((R::Network, why));
        }
        let unsent = ports.held.iter().enumerate().skip(first).find_map(|(i, port)| {
            net.tell(
                kind::NET_PUBLISHED,
                &port.told(),
                &[crate::netproc::lend(&port.socket)],
            )
            .err()
            .map(|e| (i, e))
        });
        if let Some((i, e)) = unsent {
            // Those its network process was not told are let go of; those it was are its.
            ports.held.truncate(i);
            return Err((R::Network, format!("its network process: {e}")));
        }
        Ok(ports.held.iter().skip(first).map(|p| p.bound.clone()).collect())
    }

    /// The run's grants past its microVM made `asked` as it goes (D273), within its image's:
    /// checked as its start's are (`super::egress`, `super::held_apart`: a grant its image does
    /// not give, or a member's reach other than its image lets it have, is a widening), told its
    /// network process (`NET_GRANTS`), which holds every flow to them from then on and ends
    /// those they no longer reach, and held here once it took them.
    fn egress_live(live: &Live<'_>, asked: &wire::Narrowing) -> Result<(), (wire::NetRefusal, String)> {
        use wire::NetRefusal as R;
        let net = live.net.ok_or_else(no_network)?;
        let refused = |super::No(cause, why)| match cause {
            Cause::Widening => (R::Widening, why),
            _ => (R::Image, why),
        };
        let grants = live.agentfile.map(|a| &a.grants);
        // Each member's own reach its orchestrator set stays in what its network process holds
        // (D278).
        let given = lock(&live.given).clone();
        let reaches: Vec<&wire::Reach> = given.values().collect();
        let policy = super::policy_with(grants, asked, &reaches).map_err(refused)?;
        super::held_apart(live.agentfile, asked).map_err(refused)?;
        let resolved = super::resolved(grants, asked).map_err(refused)?;
        if !live.ceiled {
            // Its image grants nothing past its microVM, which its network process was told
            // as no policy: nothing can be granted, and nothing is what it holds.
            if policy.is_some() {
                return Err((R::Widening, "its image grants nothing past its microVM".into()));
            }
            *lock(&live.egress) = resolved;
            return Ok(());
        }
        let said = net
            .ask(kind::NET_GRANTS, policy.as_deref().unwrap_or_default())
            .map_err(|why| (R::Network, why))?;
        if said != [0] {
            return Err((
                R::Network,
                format!(
                    "its network process: {}",
                    String::from_utf8_lossy(said.get(1..).unwrap_or_default())
                ),
            ));
        }
        *lock(&live.egress) = resolved;
        Ok(())
    }

    /// Member `member`'s own reach past its microVM made `reach`, whole, or its image's grants
    /// back (D278), in place: its guest holds it (init's `EGRESS`: its gate, its switch's and
    /// init's tables, its resolver, its open TCP connections to a port it loses reset), and its
    /// network process the union of the run's grants and every member's. Widening, the network
    /// process takes the union of what was and what is to be first, then the guest its own, then
    /// the network process what is; so no flow a guest lets out meets a network process that
    /// refuses it, nor one the guest shuts one that lets it through after. Refused typed by its
    /// guest's word: past its image and its ceiling `Widening`, members it could not hold apart
    /// `Unheld`, no such member linked `Unknown`; the network process's union put back where the
    /// guest refused.
    fn member_live(
        live: &Live<'_>,
        member: u32,
        reach: Option<&wire::Reach>,
    ) -> Result<(), (wire::NetRefusal, String)> {
        use wire::NetRefusal as R;
        let net = live.net.ok_or_else(no_network)?;
        if !live.members.iter().any(|m| m.index == member && m.runs) {
            return Err((R::Unknown, format!("no member {member} runs in this run")));
        }
        let args = match reach {
            None => protocol::egress::args_image(member),
            Some(r) => {
                let asked = super::reach_grants(r).map_err(|why| (R::Image, why))?;
                // Within what its image grants the run and its own ceiling, before anything is
                // told: its guest holds it to its image's grants for it alone (`EGRESS_PAST`).
                let ceiling = live.ceilings.iter().find(|(m, _)| *m == member).map(|(_, g)| g);
                if let Some(item) = super::past_run(&asked, live.agentfile.map(|a| &a.grants), ceiling) {
                    return Err((
                        R::Widening,
                        format!(
                            "member {member}: {item} is past what its image and its ceiling let it reach"
                        ),
                    ));
                }
                protocol::egress::args(member, &asked)
            }
        };
        let refused = |super::No(cause, why)| match cause {
            Cause::Widening => (R::Widening, why),
            _ => (R::Image, why),
        };
        let grants = live.agentfile.map(|a| &a.grants);
        let narrowing = lock(&live.egress).clone();
        let before = lock(&live.given).clone();
        let mut after = before.clone();
        let mut during = before.clone();
        match reach {
            Some(r) => {
                after.insert(member, r.clone());
                let both = during
                    .get(&member)
                    .map_or_else(|| r.clone(), |was| super::reach_union(was, r));
                during.insert(member, both);
            }
            None => {
                after.remove(&member);
            }
        }
        let policy = |given: &HashMap<u32, wire::Reach>| {
            let reaches: Vec<&wire::Reach> = given.values().collect();
            super::policy_with(grants, &narrowing, &reaches).map_err(refused)
        };
        let (was, between, is) = (policy(&before)?, policy(&during)?, policy(&after)?);
        let tell = |p: &Option<Vec<u8>>| -> Result<(), (R, String)> {
            if !live.ceiled {
                // No ceiling was told its network process: its image grants nothing past its
                // microVM and no member has a ceiling, so nothing can be reached.
                return match p {
                    Some(_) => Err((R::Widening, "the run may reach nothing past its microVM".into())),
                    None => Ok(()),
                };
            }
            let said = net
                .ask(kind::NET_GRANTS, p.as_deref().unwrap_or_default())
                .map_err(|why| (R::Network, why))?;
            if said == [0] {
                return Ok(());
            }
            Err((
                R::Network,
                format!(
                    "its network process: {}",
                    String::from_utf8_lossy(said.get(1..).unwrap_or_default())
                ),
            ))
        };
        if between != was {
            tell(&between)?;
        }
        let said = builtin(live, protocol::builtin::EGRESS, args)
            .map_err(|e| (R::Network, format!("its guest: {e}")));
        let refusal = match said {
            Ok(said) if said.status == Some(0) => None,
            Ok(said) => {
                let words = String::from_utf8_lossy(&said.err).trim().to_string();
                Some(match said.status {
                    Some(protocol::builtin::EGRESS_PAST) => (R::Widening, words),
                    Some(protocol::builtin::EGRESS_UNHELD) => (R::Unheld, words),
                    Some(protocol::builtin::EGRESS_UNKNOWN) => (R::Unknown, words),
                    _ => (R::Network, format!("its guest: {words}")),
                })
            }
            Err(e) => Some(e),
        };
        if let Some(refused) = refusal {
            if between != was {
                let _ = tell(&was);
            }
            return Err(refused);
        }
        *lock(&live.given) = after;
        if is != between {
            tell(&is)?;
        }
        Ok(())
    }

    /// The port the run publishes at `host` for `proto` let go of (D269): its network process
    /// asked to close its copies (`NET_UNPUBLISHED`), then this one closed, which frees it.
    /// What it let go of.
    fn unpublish_live(
        live: &Live<'_>,
        ports: &mut Ports,
        proto: wire::Proto,
        host: std::net::SocketAddr,
    ) -> Result<Vec<wire::Bound>, (wire::NetRefusal, String)> {
        use wire::NetRefusal as R;
        let net = live.net.ok_or_else(no_network)?;
        let Some(i) = ports
            .held
            .iter()
            .position(|p| p.bound.proto == proto && p.bound.host == host)
        else {
            return Err((
                R::Unknown,
                format!("the run publishes no {} port at {host}", proto_name(proto)),
            ));
        };
        let id = ports
            .held
            .get(i)
            .map(|p| p.id)
            .ok_or_else(|| (R::Unknown, format!("{host}")))?;
        net.ask(kind::NET_UNPUBLISHED, &id.to_be_bytes())
            .map_err(|why| (R::Network, why))?;
        Ok(vec![ports.held.remove(i).bound])
    }

    /// The run's managed output, owned here: each active member channel's credit and what
    /// is waiting of it for the orchestrator's room.
    #[derive(Default)]
    struct Flows {
        /// The orchestrator's room for each channel, in bytes.
        room: HashMap<Key, u64>,
        /// A record's bytes the orchestrator had no room for yet.
        waiting: HashMap<Key, Vec<u8>>,
        /// A credit granted the guest that no record has used yet.
        granted: HashMap<Key, bool>,
        /// The channel has ended.
        ended: HashMap<Key, bool>,
        /// The generations no credit goes to now, by member and generation (D270 slice 3b): a
        /// restart's last, once every channel of it its guest was given a credit for ended.
        sealed: std::collections::HashSet<(u32, u32)>,
        /// A grant its VM could not take has been said (`take`): once a run.
        ungranted: bool,
    }

    /// A member's output channel, by its index, its generation (D270) and the channel: each
    /// generation's its own, so that a record of one that comes after the next started (its
    /// end and its member's are on two connections, in no order between them) is never the
    /// next's.
    type Key = (u32, u32, Channel);

    /// Where a run's component images come from (D270 slice 4): path `at`'s EROFS image of
    /// manifest digest `digest`, or why not.
    type ComponentImage<'a> =
        dyn Fn(&str, &str) -> Result<std::fs::File, (wire::SwapRefusal, String)> + Sync + 'a;

    /// Each member's components (D270 slice 4): what its next start lays at its path and what
    /// its current generation runs with, each its path and key, by its index; and each component
    /// plugged into its microVM, by its key.
    #[derive(Default)]
    struct Components {
        desired: HashMap<u32, (String, protocol::members::ComponentKey)>,
        held: HashMap<u32, (String, protocol::members::ComponentKey)>,
        plugged: HashMap<protocol::members::ComponentKey, PluggedComponent>,
    }

    /// A component plugged into a run's microVM (D270 slice 2): its function's slot and address,
    /// and its manifest's digest.
    #[derive(Clone)]
    struct PluggedComponent {
        devfn: u8,
        address: String,
        digest: String,
    }

    /// Each member's generations (D270 slice 3b), as its restarts and its guest move them.
    #[derive(Default)]
    struct Generations {
        /// Each member's current generation, by its index: 0 where it never restarted.
        current: HashMap<u32, u32>,
        /// The members a restart is under way of.
        restarting: std::collections::HashSet<u32>,
        /// The generations whose ends its guest told (`MEMBER_ENDED`), by member and generation.
        ended: std::collections::HashSet<(u32, u32)>,
        /// The generations a restart ended at once (`MEMBER_KILL`), by member and generation.
        killed: std::collections::HashSet<(u32, u32)>,
        /// What its guest said of each member's last start asked (`MEMBER_STARTED`).
        started: HashMap<u32, protocol::members::StartAnswer>,
        /// Each output stream's generation, fixed as it opened.
        streams: HashMap<(u32, Channel), u32>,
        /// Its VM's link has closed: no end or answer comes now.
        gone: bool,
    }

    /// A member's stdin, which its orchestrator writes (plan step 5): its guest's input
    /// connection once its VM hands it over, what the orchestrator sent and its writer has
    /// not yet written, the room it was given and has not used, and its end. What it sent is
    /// what a person typed, a credential as often as not: each part a [`wire::Secret`], wiped
    /// as it is written, cleared or let go of (D242).
    #[derive(Default)]
    struct Feed {
        /// The generation it is the stdin of (D270): its frames say it, and its orchestrator's
        /// for another generation go nowhere.
        generation: u32,
        conn: Conn,
        queued: std::collections::VecDeque<wire::Secret>,
        room: u64,
        /// The orchestrator's end or abort, to go after what is queued.
        end: Option<wire::StreamEnd>,
        /// Its init told its end, or the run's ended: nothing more goes.
        ended: bool,
    }

    /// A member's input connection, as far as this host has it: once here, never again.
    #[derive(Default)]
    enum Conn {
        #[default]
        Awaited,
        Here(Stream),
        Gone,
    }

    impl Conn {
        /// Ends it, and none comes after.
        fn end(&mut self) {
            if let Conn::Here(conn) = std::mem::replace(self, Conn::Gone) {
                shards_ipc::end(&conn);
            }
        }
    }

    /// What the serving threads share.
    struct Live<'a> {
        answers: &'a Answers,
        /// The home's record of this host's ends, by which a share process plugged in is
        /// followed to its end ([`Shares::end`]).
        ends: Ends,
        flows: Mutex<Flows>,
        /// Told as a channel ends, for a restart that waits on its last generation's output.
        flows_moved: std::sync::Condvar,
        /// Each input member's stdin, by its index.
        feeds: Mutex<HashMap<u32, Feed>>,
        fed: std::sync::Condvar,
        /// Its members' generations (D270 slice 3b), and what waits on them told as they move.
        generations: Mutex<Generations>,
        generations_moved: std::sync::Condvar,
        capture: Option<Mutex<Stream>>,
        link: Mutex<Stream>,
        /// Each exec's client end, by its number, held until the VM says it has it
        /// (`EXEC_TAKEN`), as the daemon holds an exec's (M24): none once the link has
        /// closed, when no VM will.
        held: Mutex<Option<HashMap<u64, Stream>>>,
        /// How the orchestrator's asks ended, where they did ([`super::ended_by`]).
        asks_end: Mutex<Option<super::AsksEnd>>,
        next_exec: std::sync::atomic::AtomicU64,
        /// The home, whose private directory each share process keeps its ghosts in.
        home: &'a Path,
        /// The run's members, and its domains' plug roots by name (D235).
        members: &'a [wire::MemberInfo],
        roots: &'a [Vec<u8>],
        /// Each device step asked of the VM and not yet answered (`DEVICED`), by its number;
        /// none once the VM's link has closed, as nothing will answer one then.
        devices: Mutex<Option<Vec<Asked<Refusal>>>>,
        /// Each guest time asked of the VM and not yet answered (`GUEST_TIMED`), as devices.
        guest_times: Mutex<Option<Vec<Asked>>>,
        /// Each rate told the VM and not yet answered (`RATED`, D247), as devices.
        rates: Mutex<Option<Vec<Asked>>>,
        /// Each socket lane asked of the VM and not yet answered (`LANE_OPEN`, D251), by its
        /// number; none once the VM's link has closed, as nothing will answer one then.
        lanes: Mutex<Option<LaneWaits>>,
        next_lane: std::sync::atomic::AtomicU64,
        /// What is plugged into the run's members, by the path it is at: each member's.
        plugged: Mutex<HashMap<String, Vec<Plugged>>>,
        /// A plug/unplug's ownership turn, held through device and guest completion;
        /// replies never need this lock. The mounted registry stays readable meanwhile.
        plug_turn: Mutex<()>,
        /// Each named volume of its home it mounts or plugged, held for its life (D281): a
        /// clearing of one is refused while it runs.
        volumes_held: Mutex<crate::volumes::Holds>,
        /// The pipe the run's share process tells what its host changes on (D235), until
        /// its guest's NOTICE built-in is given it as the run starts; and the streams each
        /// of those built-ins' ends are told on, held while the run is.
        notices_due: Mutex<Option<OwnedFd>>,
        notices: Mutex<Vec<Stream>>,
        /// Its credential leases, where it has grants (D242).
        leasing: Option<super::Leasing<'a>>,
        /// Its save, as its VM is asked for one (D259).
        saving: Mutex<super::Saving>,
        /// Its pause (D267).
        pauses: Mutex<super::Pauses>,
        /// Each pause or resume asked of the VM and not yet answered (`PAUSED`), as devices.
        pause_asks: Mutex<Option<Vec<Asked<Unheld>>>>,
        /// The host directories its shares give its guest, canonical (D259).
        reached: &'a [PathBuf],
        /// What its own lanes of its agent-control endpoint are served with, where its
        /// orchestrator asked for the endpoint (D238, D252, D254).
        agent: Option<Agent>,
        /// Its members' stdio its endpoint serves as lanes, where its orchestrator asked for it
        /// (D252): set once as its endpoint starts serving ([`serve_stdio`]), before anything
        /// reads its VM.
        stdio: std::sync::OnceLock<Stdio>,
        /// Its network process's control, where it has a network (D242, D269).
        net: Option<&'a super::NetControl<'a>>,
        /// The ports it publishes (D269).
        ports: Mutex<Ports>,
        /// Its grants past its microVM as they stand, written out whole (D273): held under its
        /// ports' lock's turn, as every change of its network is.
        egress: Mutex<wire::Narrowing>,
        /// Each member's own reach past its microVM its orchestrator set, by its index (D278):
        /// one not here reaches its image's grants. Held under the same turn.
        given: Mutex<HashMap<u32, wire::Reach>>,
        /// Each member's ceiling past its microVM, by its index (D278): none, nothing past its
        /// image's grants.
        ceilings: Vec<(u32, protocol::egress::Grants)>,
        /// Each member's components as its orchestrator swapped them (D270 slice 4).
        components: Mutex<Components>,
        /// A component's EROFS image for path `at` of digest `digest` (D270 slice 4): its host's
        /// store's, checked against its member; a test's stand-in.
        component_image: &'a ComponentImage<'a>,
        /// Whether its network process was told a ceiling (`NET_POLICY`): its image grants
        /// something past its microVM (D273).
        ceiled: bool,
        /// Its image's Agentfile, which a port published is checked against and reaches its
        /// members by (D122).
        agentfile: Option<&'a crate::agentfile::Agentfile>,
        /// It has a command of its own, which a port no member takes coming in reaches.
        command: bool,
        /// On Windows the run's job, which each plug's share process is made in too.
        #[cfg(windows)]
        job: &'a shards_ipc::GroupJob,
    }

    /// What a run's own lanes of its agent-control endpoint are served with, as the daemon's
    /// runs' are (daemon/managed): its execs, its members' MCP connections and its members' own
    /// lanes, each through the daemon's hosts.
    struct Agent {
        /// What each exec's and MCP connection's streams are admitted, as the daemon's runs
        /// admit them (`shards_api::control::exec_admissions`).
        admissions: shards_remote::control::exec::Admissions,
        /// The budget its lanes are charged to: this host's own, held to the host's available
        /// memory as the daemon's runs' is.
        charge: std::sync::Arc<Charge>,
        /// Where its command stands, for each lane that waits on its start.
        begun: Mutex<Begun>,
        began: std::sync::Condvar,
        /// Each member's end as its init tells it (`MEMBER_ENDED`), for its own lanes.
        ends: members::Ends,
        /// Its members that run a program, and those with a terminal of their own (D244).
        active: Vec<u32>,
        terminals: Vec<u32>,
        /// Its members whose stdin its orchestrator writes, by their declared index.
        inputs: Vec<u32>,
        /// Its endpoint serves its members' stdio as lanes (`ControlAsk::stdio`, D252).
        stdio: bool,
        /// Its members' MCP connections, where its request gave any a bridge (D254).
        bridges: Option<mcp::Bridges>,
    }

    /// Where a run's command stands, as its VM says it (R04): awaited until its `STARTED`;
    /// over once its link has closed, started or not.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Begun {
        Awaited,
        Started,
        Over,
    }

    impl Agent {
        /// A run's, `plan`'s.
        fn of(plan: &Plan) -> Result<Agent, String> {
            let admissions = shards_api::control::exec_admissions();
            let budget = std::sync::Arc::new(Mutex::new(Some(0)));
            let charge = Charge::reserve(&budget, 0).map_err(|e| format!("its lanes' budget: {e:?}"))?;
            let charge = std::sync::Arc::new(charge);
            let ends =
                members::Ends::new(plan.members.len()).map_err(|e| format!("its members' ends: {e}"))?;
            let bridges = (!plan.mcps.is_empty()).then(|| {
                let record = admissions.output.record_bytes;
                mcp::Bridges::new(plan.mcps.clone(), record, std::sync::Arc::clone(&charge))
            });
            Ok(Agent {
                admissions,
                charge,
                begun: Mutex::new(Begun::Awaited),
                began: std::sync::Condvar::new(),
                ends,
                active: plan.members.iter().filter(|m| m.runs).map(|m| m.index).collect(),
                terminals: plan.terminals.clone(),
                inputs: plan.inputs.clone(),
                stdio: plan.control.as_ref().is_some_and(|c| c.stdio),
                bridges,
            })
        }

        /// Says where its command stands now: told every lane that waits on its start.
        fn stands(&self, now: Begun) {
            *lock(&self.begun) = now;
            self.began.notify_all();
        }

        /// The run is over: every lane that waits on its start, or on a member's end, told so,
        /// and what its members' bridges held let go.
        fn over(&self) {
            self.stands(Begun::Over);
            self.ends.close();
            if let Some(bridges) = &self.bridges {
                bridges.close();
            }
        }

        /// Waits for its command to start: whether it has, or none once the run is over.
        fn started(&self) -> bool {
            let mut begun = lock(&self.begun);
            while *begun == Begun::Awaited {
                begun = self.began.wait(begun).unwrap_or_else(PoisonError::into_inner);
            }
            *begun == Begun::Started
        }
    }

    /// Its members' stdio its endpoint serves as lanes, where its orchestrator asked for it
    /// (`ControlAsk::stdio`, D252), as a daemon's run's endpoint serves the streams its owner
    /// reads (D238): each stream's control by member, generation (D270) and channel, held for
    /// the run's life, as a control let go revokes its stream; each output's way to the thread
    /// that serves it ([`lane_output`]); and where each next generation's streams join the
    /// endpoint.
    struct Stdio {
        generations: shards_remote::control::Generations,
        outputs: Mutex<HashMap<Key, LaneOutput>>,
        inputs: Mutex<HashMap<(u32, u32), shards_remote::managed::input::Control>>,
    }

    /// A member output its endpoint serves: its records' way to its thread, none once its
    /// capture has ended; and its control.
    struct LaneOutput {
        records: Option<std::sync::mpsc::Sender<(Vec<u8>, bool)>>,
        control: shards_remote::managed::Control,
    }

    impl Stdio {
        /// Serves output `key` from now on: its control held, its thread started; one that has
        /// no thread is revoked, which its lane is told.
        fn output<'scope, 'env>(
            &'env self,
            live: &'env Live<'env>,
            key: Key,
            producer: shards_remote::managed::Producer,
            control: shards_remote::managed::Control,
            scope: &'scope std::thread::Scope<'scope, 'env>,
        ) {
            let (records, taken) = std::sync::mpsc::channel();
            lock(&self.outputs).insert(
                key,
                LaneOutput {
                    records: Some(records),
                    control,
                },
            );
            let started = std::thread::Builder::new()
                .name("api-run lane".into())
                .spawn_scoped(scope, move || lane_output(live, self, key, &producer, &taken));
            if let Err(e) = started {
                super::say(format_args!("member {}'s output lane has no thread: {e}", key.0));
                self.abort(key);
            }
        }

        /// Serves member `key.0`'s generation `key.1`'s stdin from now on: its control held, its
        /// relay's thread started ([`lane_input`]); one that has no thread is revoked.
        fn input<'scope, 'env>(
            &'env self,
            live: &'env Live<'env>,
            key: (u32, u32),
            relay: shards_remote::managed::input::Relay,
            control: shards_remote::managed::input::Control,
            scope: &'scope std::thread::Scope<'scope, 'env>,
        ) {
            lock(&self.inputs).insert(key, control);
            let started = std::thread::Builder::new()
                .name("api-run stdin".into())
                .spawn_scoped(scope, move || lane_input(live, key, &relay));
            if let Err(e) = started {
                super::say(format_args!("member {}'s stdin lane has no thread: {e}", key.0));
                if let Some(control) = lock(&self.inputs).get(&key) {
                    control.abort();
                }
            }
        }

        /// Member `member`'s generation `generation`'s streams joined to its endpoint (D270),
        /// before its start, so that its guest's first record and its input find them: its
        /// stdout and stderr, and its stdin where it has one, each served as its first
        /// generation's are.
        fn join<'scope, 'env>(
            &'env self,
            live: &'env Live<'env>,
            member: u32,
            generation: u32,
            stdin: bool,
            scope: &'scope std::thread::Scope<'scope, 'env>,
        ) -> Result<(), String> {
            for channel in [Channel::Stdout, Channel::Stderr] {
                let (named, _) = shards_api::control::member_output(member, generation, channel);
                let (producer, control) = self
                    .generations
                    .output(named)
                    .map_err(|e| format!("its generation {generation}'s output lane: {e}"))?;
                self.output(live, (member, generation, channel), producer, control, scope);
            }
            if stdin {
                let (named, _) = shards_api::control::member_input(member, generation);
                let (relay, control) = self
                    .generations
                    .input(named)
                    .map_err(|e| format!("its generation {generation}'s stdin lane: {e}"))?;
                self.input(live, (member, generation), relay, control, scope);
            }
            Ok(())
        }

        /// The record `record` of output `key` to its thread, `last` its end or abort: whether
        /// a thread took it.
        fn record(&self, key: Key, record: Vec<u8>, last: bool) -> bool {
            lock(&self.outputs)
                .get(&key)
                .and_then(|o| o.records.as_ref())
                .is_some_and(|records| records.send((record, last)).is_ok())
        }

        /// Output `key`'s last record has gone to its lane: its end follows as it is consumed;
        /// one its stream refuses is revoked.
        fn finish(&self, key: Key) {
            if let Some(lane) = lock(&self.outputs).get(&key)
                && lane.control.finish().is_err()
            {
                lane.control.abort();
            }
        }

        /// Output `key` revoked: its lane told so (`Revoked`), a later one refused `Closed`.
        fn abort(&self, key: Key) {
            if let Some(lane) = lock(&self.outputs).get(&key) {
                lane.control.abort();
            }
        }

        /// Member `member`'s generation `generation`'s streams revoked: a restart that failed
        /// past joining them (D270), whose generation may never run.
        fn abort_generation(&self, member: u32, generation: u32) {
            for channel in [Channel::Stdout, Channel::Stderr] {
                self.abort((member, generation, channel));
            }
            if let Some(control) = lock(&self.inputs).get(&(member, generation)) {
                control.abort();
            }
        }

        /// Member `key.0`'s generation `key.1`'s stdin has ended as its init told it: its lane
        /// told, as the daemon tells its owner's (`Outputs::input_ended`).
        fn input_ended(&self, key: (u32, u32), written: u64, end: managed::InputEnd) {
            if let Some(control) = lock(&self.inputs).get(&key) {
                control.ended(written, end);
            }
        }
    }

    /// Serves member output `key` on its endpoint's lane (D252), as the daemon's credit worker
    /// serves an owned one (daemon/managed.rs `send_credits`): its guest credited one record of
    /// the room its lane's subscriber has (`Producer::wait_capacity`), that record put whole in
    /// the lane's own buffer as its capture brought it (the guest's record, which its
    /// controller decodes), and the output finished after its last, its end or its abort. A
    /// record that came as its subscriber left waits for the next; one past its credit revokes
    /// the output. Ends with its output, with its credits (its generation past, or its VM
    /// gone), or with its capture, which cut it ([`Live::cut`]).
    fn lane_output(
        live: &Live<'_>,
        stdio: &Stdio,
        key: Key,
        producer: &shards_remote::managed::Producer,
        records: &std::sync::mpsc::Receiver<(Vec<u8>, bool)>,
    ) {
        loop {
            let Ok(room) = producer.wait_capacity() else {
                return;
            };
            if !live.credit_lane(key, room) {
                return;
            }
            let Ok((record, last)) = records.recv() else {
                return;
            };
            let mut buffer = loop {
                match producer.take_buffer() {
                    Ok(Some(buffer)) => break buffer,
                    // Its subscriber left between its credit and its record: it waits for the
                    // next, at the same place.
                    Ok(None) => {
                        if producer.wait_capacity().is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        stdio.abort(key);
                        return;
                    }
                }
            };
            // A record past its credit is its guest's breach of it: the output revoked, the
            // buffer it took with it.
            if record.len() > buffer.capacity() {
                stdio.abort(key);
                return;
            }
            buffer.clear();
            buffer.extend_from_slice(&record);
            if producer.submit(buffer).is_err() {
                stdio.abort(key);
                return;
            }
            if last {
                stdio.finish(key);
                return;
            }
        }
    }

    /// Relays member `key.0`'s generation `key.1`'s stdin from its endpoint's lane to its input
    /// connection (D252) with the daemon's relay (daemon/managed.rs `relay_input`): the
    /// connection its feed holds once its VM has handed it over, none once that generation's
    /// input has ended or its feed is another generation's.
    fn lane_input(live: &Live<'_>, key: (u32, u32), relay: &shards_remote::managed::input::Relay) {
        let (member, generation) = key;
        crate::daemon::managed::relay_input(relay, &|| feed_conn(live, member, generation), &|| {
            feed_close(live, member, generation)
        });
    }

    /// Waits for member `member`'s generation `generation`'s input connection: a copy of it,
    /// none where it will not come.
    fn feed_conn(live: &Live<'_>, member: u32, generation: u32) -> Option<Stream> {
        let mut feeds = lock(&live.feeds);
        loop {
            let feed = feeds
                .get(&member)
                .filter(|f| f.generation == generation && !f.ended)?;
            match &feed.conn {
                Conn::Here(conn) => return conn.try_clone().ok(),
                Conn::Gone => return None,
                Conn::Awaited => {}
            }
            feeds = live.fed.wait(feeds).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Ends member `member`'s generation `generation`'s input connection, where its feed is
    /// that generation's: none comes after.
    fn feed_close(live: &Live<'_>, member: u32, generation: u32) {
        if let Some(feed) = lock(&live.feeds)
            .get_mut(&member)
            .filter(|f| f.generation == generation)
        {
            feed.conn.end();
        }
        live.fed.notify_all();
    }

    /// Serves its members' stdio on its endpoint's lanes from now on (D252), where its
    /// orchestrator asked for it: `outputs` and `inputs`, the streams its endpoint was bound
    /// with ([`first_streams`]' order), held in [`Live::stdio`] with `generations`, where each
    /// next generation's join it, and each one's thread started.
    fn serve_stdio<'scope, 'env>(
        live: &'env Live<'env>,
        agent: &Agent,
        generations: shards_remote::control::Generations,
        outputs: Vec<(shards_remote::managed::Producer, shards_remote::managed::Control)>,
        inputs: Vec<(
            shards_remote::managed::input::Relay,
            shards_remote::managed::input::Control,
        )>,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        let (keys, members) = match first_streams(agent) {
            Ok(streams) => streams,
            Err(why) => {
                super::say(format_args!("its members' stdio is served none: {why}"));
                return;
            }
        };
        if keys.is_empty() && members.is_empty() {
            return;
        }
        let stdio = Stdio {
            generations,
            outputs: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
        };
        if live.stdio.set(stdio).is_err() {
            return;
        }
        let Some(stdio) = live.stdio.get() else {
            return;
        };
        for (key, (producer, control)) in keys.into_iter().zip(outputs) {
            stdio.output(live, key, producer, control, scope);
        }
        for (member, (relay, control)) in members.into_iter().zip(inputs) {
            stdio.input(live, (member, 0), relay, control, scope);
        }
    }

    /// An ask of the VM: its number, and who waits for its answer, or why not: its text, or
    /// a device step's refusal by cause (devices-10).
    type Asked<E = String> = (u64, std::sync::mpsc::Sender<Result<String, E>>);

    /// What an ask of the VM is refused with, as its answer says it.
    trait VmRefusal: Sized {
        /// Its answer after its number.
        fn read(said: &[u8]) -> Result<String, Self>;
        /// Its VM's link closed before it answered.
        fn stopped() -> Self;
        /// It could not be sent.
        fn unsent(e: &std::io::Error) -> Self;
        /// Its VM's refusal, as its asker is told it.
        fn said(self) -> Self;
    }

    /// `GUEST_TIMED` and `RATED`: 0 and what it said, or anything else and why not.
    impl VmRefusal for String {
        fn read(said: &[u8]) -> Result<String, String> {
            let text = String::from_utf8_lossy(said.get(1..).unwrap_or_default()).into_owned();
            match said.first() {
                Some(0) => Ok(text),
                _ => Err(text),
            }
        }
        fn stopped() -> String {
            "its microVM stopped".into()
        }
        fn unsent(e: &std::io::Error) -> String {
            format!("asking its microVM: {e}")
        }
        fn said(self) -> String {
            format!("its microVM: {self}")
        }
    }

    /// Why its VM did not pause or let go on its vCPUs (`PAUSED`, D267), in its words.
    #[derive(Debug)]
    struct Unheld(String);

    /// `PAUSED`: nothing for done, else why not.
    impl VmRefusal for Unheld {
        fn read(said: &[u8]) -> Result<String, Unheld> {
            if said.is_empty() {
                Ok(String::new())
            } else {
                Err(Unheld(String::from_utf8_lossy(said).into_owned()))
            }
        }
        fn stopped() -> Unheld {
            Unheld("its microVM stopped".into())
        }
        fn unsent(e: &std::io::Error) -> Unheld {
            Unheld(format!("asking its microVM: {e}"))
        }
        fn said(self) -> Unheld {
            Unheld(format!("its microVM: {}", self.0))
        }
    }

    /// `DEVICED`, by cause (shards_ipc::device).
    impl VmRefusal for Refusal {
        fn read(said: &[u8]) -> Result<String, Refusal> {
            shards_ipc::device::decode(said)
        }
        fn stopped() -> Refusal {
            Refusal::ConnectionEnded
        }
        fn unsent(e: &std::io::Error) -> Refusal {
            Refusal::Host(format!("asking its microVM: {e}"))
        }
        fn said(self) -> Refusal {
            self
        }
    }

    /// The VM's answer to a socket lane (`LANE_OPEN`, D251): its payload, and the connection
    /// beside it on Unix.
    type LaneAnswer = (Vec<u8>, Vec<OwnedFd>);

    /// The socket lanes asked of the VM and not yet answered, by number.
    type LaneWaits = HashMap<u64, std::sync::mpsc::SyncSender<LaneAnswer>>;

    /// A host directory plugged into a member (D235): its place on the microVM's PCI bus
    /// (`devfn`), its guest's address of it, the member's index, and its share process.
    struct Plugged {
        devfn: u8,
        address: String,
        member: u32,
        /// Its host directory, canonical: where no save of the run is made (D259).
        host: PathBuf,
        share: Shares,
        /// The stream its guest's NOTICE built-in's end is told on (D235), held while it is
        /// plugged in.
        _told: Option<Stream>,
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
        /// Member `member`'s current generation (D270): 0 where it never restarted.
        fn generation_of(&self, member: u32) -> u32 {
            lock(&self.generations).current.get(&member).copied().unwrap_or(0)
        }

        /// Member `member`'s current generation, the one an end its guest tells now is of, and
        /// whether its restart sent it a kill.
        fn ended_as(&self, member: u32) -> (u32, bool) {
            let generations = lock(&self.generations);
            let current = generations.current.get(&member).copied().unwrap_or(0);
            (current, generations.killed.contains(&(member, current)))
        }

        /// Grants the guest a record on `key`'s channel where it has none, its room is
        /// not spent, nothing waits of it, and it has not ended: as many bytes as the
        /// orchestrator has room for, past the record's head, within the protocol's
        /// ceiling and at least a record of an errno's (`Credit::decode`'s least).
        fn grant(&self, flows: &mut Flows, key: Key) -> io::Result<()> {
            let Some(capture) = &self.capture else {
                return Ok(());
            };
            // Room for a generation past is no record of its guest's: a member runs one
            // generation at a time.
            if key.1 != self.generation_of(key.0) || flows.sealed.contains(&(key.0, key.1)) {
                return Ok(());
            }
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
            Self::credit(capture, flows, key, capacity)
        }

        /// Credits the guest one record of output `key`, `room` bytes at most, the room its
        /// lane's subscriber has (D252), where its generation is its member's current and not
        /// sealed, and its end has not come: whether it went. One its VM can take no more of
        /// is its VM ending, said once, as [`Live::grant`]'s.
        fn credit_lane(&self, key: Key, room: usize) -> bool {
            let Some(capture) = &self.capture else {
                return false;
            };
            let mut flows = lock(&self.flows);
            if key.1 != self.generation_of(key.0)
                || flows.sealed.contains(&(key.0, key.1))
                || flows.ended.get(&key).copied().unwrap_or(false)
            {
                return false;
            }
            let least = (managed::HEADER + size_of::<i32>()) as u64;
            let capacity = u64::try_from(room)
                .unwrap_or(u64::MAX)
                .min(u64::from(protocol::MAX_PAYLOAD));
            if capacity < least {
                return false;
            }
            match Self::credit(capture, &mut flows, key, capacity) {
                Ok(()) => true,
                Err(e) => {
                    if !std::mem::replace(&mut flows.ungranted, true) {
                        super::say(format_args!("its VM takes no more of its members' output: {e}"));
                    }
                    false
                }
            }
        }

        /// Credits the guest `capacity` bytes of output `key`'s next record
        /// (`MANAGED_CREDIT`), its credit out from then on.
        fn credit(capture: &Mutex<Stream>, flows: &mut Flows, key: Key, capacity: u64) -> io::Result<()> {
            let credit = managed::Credit {
                member: key.0,
                channel: managed_channel(key.2),
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

        /// Ends `key`'s channel `end`: what waits of it goes first, its room or not, as no
        /// credit after its end could let it go: at most one record's, as `grant` grants the
        /// guest none while anything waits.
        fn end_channel(&self, flows: &mut Flows, key: Key, end: wire::StreamEnd) -> io::Result<()> {
            flows.ended.insert(key, true);
            self.flows_moved.notify_all();
            let rest = flows.waiting.remove(&key).unwrap_or_default();
            if !rest.is_empty() {
                self.answers.send(
                    wire::to_client::OUTPUT,
                    &[&wire::Output::head(key.0, key.1, key.2), &rest],
                )?;
            }
            let end = wire::OutputEnd {
                member: key.0,
                generation: key.1,
                channel: key.2,
                end,
            };
            self.answers.send(wire::to_client::OUTPUT_END, &[&end.encode()])
        }

        /// Its VM's capture over, ends cut every channel its orchestrator was told of or gave
        /// room to that has no end (control-01's follow-up): nothing more of it can come, so
        /// what this host holds of it goes, and its end says it was cut, `Aborted(CUT)`,
        /// rather than its orchestrator waiting for this process's exit to learn it ended.
        fn cut(&self) -> io::Result<()> {
            let mut flows = lock(&self.flows);
            // Each of its members' streams its endpoint serves (D252) whose end did not come is
            // revoked, its lane told so; each whose end came goes on to its lane, consumed as
            // its controller reads it; every lane's thread let go.
            if let Some(stdio) = self.stdio.get() {
                for (key, lane) in lock(&stdio.outputs).iter_mut() {
                    lane.records = None;
                    if !flows.ended.get(key).copied().unwrap_or(false) {
                        lane.control.abort();
                    }
                }
            }
            let known: std::collections::HashSet<Key> = flows
                .room
                .keys()
                .chain(flows.granted.keys())
                .chain(flows.waiting.keys())
                .copied()
                .collect();
            for key in known {
                if !flows.ended.get(&key).copied().unwrap_or(false) {
                    self.end_channel(&mut flows, key, wire::StreamEnd::Aborted(CUT))?;
                }
            }
            Ok(())
        }

        /// Sends what waits of `key`'s channel within the orchestrator's room.
        fn flush(&self, flows: &mut Flows, key: Key) -> io::Result<()> {
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
                    &[&wire::Output::head(key.0, key.1, key.2), &part],
                )?;
            }
            Ok(())
        }
    }

    /// The errno a channel cut by its VM's capture's end is aborted with: ECONNRESET's Linux
    /// number (include/uapi/asm-generic/errno.h), as the stream it came on ended under it; a
    /// guest's errno, as init's own aborts are, whatever this host's own numbers.
    const CUT: i32 = 104;

    fn kind_credit() -> u8 {
        protocol::kind::MANAGED_CREDIT
    }

    /// Hands the VM its run, as the daemon hands a warm VM one (daemon.rs `hand_over`), the
    /// VM `READY` already (`launch`): `RUN` with the run's head, spec and descriptors, on
    /// Windows its shares' streams after it (D197), then `TAKEN`; then `STOP_ORDER`, SIGKILL
    /// with no grace, so that a VM whose link closes, this process gone, ends its run.
    /// `trust` joins its setup: the run's CA for its members' trust (D242), known only now.
    /// Hands its VM the run `plan`, which publishes ports where it has a network (`networked`):
    /// its start's, and those its owner publishes as it goes (D269), each of which its VM has
    /// its network process close as the run ends, before its end is told.
    fn hand_over(
        link: &Stream,
        plan: &Plan,
        networked: bool,
        trust: &[Vec<u8>],
        client: &Stream,
        shares: Option<&Shares>,
        capture: Option<&Stream>,
    ) -> Result<(), String> {
        let null = std::fs::File::open(crate::os::NULL).map_err(|e| format!("{}: {e}", crate::os::NULL))?;
        let err = io::stderr();
        // Its guest's most to plug, its own, as a daemon run's request carries it (D148).
        let plug = plan.prepared.plug_bytes();
        let payload = if trust.is_empty() {
            super::run_request(&plan.prepared.spec, plug, networked, super::timed())
        } else {
            let mut spec = plan.prepared.spec.clone();
            spec.setup.extend_from_slice(trust);
            crate::spec::fits(&spec)?;
            super::run_request(&spec, plug, networked, super::timed())
        };
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

    fn launch_and_serve(
        answers: &Answers,
        input: super::Input,
        home: &Path,
        plan: Plan,
        checking: Checking,
        layering: Layering,
    ) -> Result<(), No> {
        super::say(format_args!(
            "prepared {}, {} vCPUs, {} MiB; starting its VM",
            plan.prepared.image_id, plan.prepared.size.0, plan.prepared.size.1
        ));
        let mut reported = None;
        let launched = launch(home, &plan.launch(), checking, layering, &mut reported);
        // Its VM's GPU report (D233), relayed before its start, or before its refusal where its
        // VM did not start.
        if let Some(frame) = reported
            && let Err(e) = answers.send(wire::to_client::GPU_REPORT, &[&frame])
        {
            if let Ok(launched) = launched {
                end(home, Some(launched), &plan.made);
            }
            return Err(No(Cause::Boot, format!("its orchestrator: {e}")));
        }
        serve_launched(answers, input, home, plan, launched?, None)
    }

    /// What VM process `vm` costs its host (D241's `HostUse`): read at once, as the run's
    /// stats and a warm host's `USED` alike read it, never asking its guest.
    fn host_use(vm: u32) -> Result<wire::HostUse, String> {
        shards_vmm::platform::process_usage(vm)
            .map(|u| wire::HostUse {
                cpu_ns: u.cpu_ns,
                resident_bytes: u.resident,
                footprint_bytes: u.footprint,
                // Another process's count: its vCPUs' clocks are its own (D265).
                charged: wire::Charged::Unknown,
            })
            .map_err(|e| format!("its VM process: {e}"))
    }

    /// A warm run host (D253's pools): its microVM made of `exemplar` and started, and once
    /// ready said (`WARMED`); then its run, as it comes (`CREATE`), served as any run of this
    /// process is, its start said `Warm`. Meanwhile each `USE` is answered `USED`, its VM
    /// process's use as its run's stats would tell it. A run of another microVM's than its
    /// exemplar's is refused `Unfit`, for its orchestrator to start a host of its own for it;
    /// any other frame refuses it, and its channel closed first ends it, its pool having let
    /// it go.
    pub(super) fn warm(
        answers: &Answers,
        exemplar: &wire::Request,
        mut input: super::Input,
    ) -> Result<(), No> {
        let mut checking = Checking::begin();
        let home = shards_ipc::home().map_err(no(Cause::Boot))?;
        let (mut shaped, layering) = shape(exemplar, &home, &mut checking)?;
        let mut launched = match launch(&home, &shaped.launch(), checking, layering, &mut None) {
            Ok(launched) => launched,
            Err(e) => {
                end(&home, None, &shaped.made);
                return Err(e);
            }
        };
        // Its writable layer's name let go of now, before any run asks, so that a host that
        // goes unasked, however it goes, leaves none behind.
        drop(launched.layer.take());
        super::step(format_args!("its writable layer's name let go of"));
        // The ready VM holds its rootfs and settled template files itself (D26).
        // An idle spare must not keep unrelated collection waiting for its CREATE.
        drop(shaped.prepared.lease.take());
        // Its endpoint's key, its VM started, made as it waits for a run that may ask for one
        // (D252); none made here is made again as its endpoint binds, refused there as before.
        let premade = ControlKey::made().ok();
        super::step(format_args!("its agent-control key made"));
        super::say(format_args!(
            "warm, its VM process {} ready ({:?})",
            launched.vm.id(),
            launched.from
        ));
        // Its run: its request, the same microVM's, and its deadline from now; none where its
        // pool let it go.
        let taken = (|| -> Result<Option<(wire::Request, Option<u64>)>, No> {
            answers
                .send(wire::to_client::WARMED, &[&wire::warmed(launched.from)])
                .map_err(|e| No(Cause::Boot, format!("its orchestrator: {e}")))?;
            let vm = launched.vm.id();
            let request = loop {
                match input.next() {
                    Ok(Some(frame)) if frame.kind == wire::to_host::CREATE => {
                        break wire::Request::decode(&frame)
                            .map_err(|e| No(Cause::Request, e.to_string()))?;
                    }
                    Ok(Some(frame)) if frame.kind == wire::to_host::USE => {
                        wire::read_use(&frame).map_err(|e| No(Cause::Request, e.to_string()))?;
                        let used = wire::used(&host_use(vm)).map_err(|e| No(Cause::Boot, e.to_string()))?;
                        answers
                            .send(wire::to_client::USED, &[&used])
                            .map_err(|e| No(Cause::Boot, format!("its orchestrator: {e}")))?;
                    }
                    Ok(Some(frame)) => return Err(super::before_request(frame.kind)),
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(No(Cause::Request, format!("the request: {e}"))),
                }
            };
            super::step(format_args!("its run's request read"));
            input.shed();
            if request.exemplar() != *exemplar {
                return Err(No(
                    Cause::Unfit,
                    "its run asks for another microVM than the one it made ready".into(),
                ));
            }
            // Its VM process started before the run named its GPU report's scope (D233), which
            // that process frames its report with as its renderer is made.
            if request.gpu_report.is_some() {
                return Err(No(
                    Cause::Unfit,
                    "its run asks for a GPU report, which a VM started before its run cannot frame".into(),
                ));
            }
            let ready_by = super::deadline(request.ready_by, std::time::SystemTime::now())?;
            Ok(Some((request, ready_by)))
        })();
        let made = shaped.made.clone();
        let (request, ready_by) = match taken {
            Ok(Some(taken)) => taken,
            Ok(None) => {
                end(&home, Some(launched), &made);
                return Ok(());
            }
            Err(e) => {
                end(&home, Some(launched), &made);
                return Err(e);
            }
        };
        let plan = match plan(&request, ready_by, shaped) {
            Ok(plan) => plan,
            Err(e) => {
                end(&home, Some(launched), &made);
                return Err(e);
            }
        };
        launched.from = wire::StartedFrom::Warm;
        serve_launched(answers, input, &home, plan, launched, premade)
    }

    /// Ends what of a run was made: its processes, `launched` (its VM killed, then each reaped
    /// once nothing of this process's holds it up, the share process as its connections to
    /// the VM close, `Shares::end`; on Windows its job let go of last, which ends whatever of
    /// them is left); and its anonymous volumes `made`, removed as `--rm`'s are.
    fn end(home: &Path, launched: Option<Launched>, made: &[String]) {
        if let Some(Launched {
            vm,
            link,
            net,
            shares,
            from: _,
            held,
            layer,
            #[cfg(windows)]
            job,
        }) = launched
        {
            let _ = vm.kill(shards_ipc::signal::SIGKILL);
            let _ = vm.wait();
            drop(link);
            // Its template's files, held open while its VM ran (D253); its writable layer's name,
            // where its run ended before it said its start.
            drop(held);
            drop(layer);
            let record = Ends::kept_in(home);
            if let Some((child, control)) = net {
                drop(control);
                crate::netproc::reap(&child, &record);
            }
            if let Some(shares) = shares {
                shares.end(&record);
            }
            #[cfg(windows)]
            drop(job);
        }
        if !made.is_empty() {
            let store = crate::volumes::Store::new(home);
            let _held = crate::volumes::lock();
            for name in made {
                if let Err(e) = store.remove(name) {
                    let _ = writeln!(io::stderr(), "shards api-run: its volume {name}: {e}");
                }
            }
        }
    }

    /// Serves the run `plan` on its VM, `launched`, ready: its network told, its run handed
    /// over, its start said, and its run served until it ends; then its processes and volumes
    /// ended.
    fn serve_launched(
        answers: &Answers,
        input: super::Input,
        home: &Path,
        mut plan: Plan,
        mut launched: Launched,
        mut premade: Option<ControlKey>,
    ) -> Result<(), No> {
        // Readiness and template settlement ended the preparation lease's work.
        drop(plan.prepared.lease.take());
        let boot = |e: String| No(Cause::Boot, e);
        super::say(format_args!(
            "its VM process {} is ready ({:?})",
            launched.vm.id(),
            launched.from
        ));
        // Its share process's notices (D235), given its guest as its run starts.
        let due = launched.shares.as_mut().and_then(|s| s.notices.take());
        let listeners = std::mem::take(&mut plan.listeners);
        let volumes = std::mem::take(&mut plan.volumes);
        let served = (|| -> Result<(), No> {
            // Its published ports, each held under a number of its own (D269), by which its
            // owner may let it go as the run goes.
            let bound: Vec<(u16, u8, std::net::SocketAddr)> =
                listeners.iter().map(|l| (l.guest_port, l.proto, l.at)).collect();
            let receivers = plan
                .prepared
                .agentfile
                .as_ref()
                .map(|a| a.receivers.as_slice())
                .unwrap_or_default();
            let published = super::published(&bound, receivers, &plan.members)?;
            let mut ports = Ports::default();
            ports.hold(listeners, published).map_err(boot)?;
            // The run's CA for its members' trust (D242), once its network process made it.
            let mut trust = Vec::new();
            if let Some((_, control)) = &launched.net {
                let ca = tell_net(
                    control,
                    plan.address.as_deref(),
                    &ports.held,
                    plan.ceiling.as_deref(),
                    plan.egress.as_deref(),
                    plan.sign_ins.as_deref(),
                    plan.credentials.as_deref(),
                )
                .map_err(boot)?;
                if let (Some(der), Some(grants)) = (&ca, plan.credentials.as_deref()) {
                    let digest = <sha2::Sha256 as sha2::Digest>::digest(der);
                    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                    super::say(format_args!(
                        "its credential grants taken; its CA's SHA-256 {hex}"
                    ));
                    trust = super::trust(der, grants, &plan.signing).map_err(boot)?;
                }
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
                launched.net.is_some(),
                &trust,
                &client_theirs,
                launched.shares.as_ref(),
                capture.as_ref().map(|(_, theirs)| theirs),
            )
            .map_err(boot)?;
            // Its agent-control endpoint (D252), its key made past its VM's start (`ControlKey`),
            // held by the endpoint alone once bound: no template, and no other run, has it; and
            // what its own lanes are served with.
            let agent = plan
                .control
                .as_ref()
                .map(|_| Agent::of(&plan))
                .transpose()
                .map_err(boot)?;
            let control = match (plan.control.as_ref(), agent.as_ref()) {
                (Some(ask), Some(agent)) => {
                    let key = control_key(premade.take()).map_err(boot)?;
                    let (outputs, inputs) = first_streams(agent).map_err(boot)?;
                    let bound = bind_control(key, ask, agent.admissions, &outputs, &inputs).map_err(boot)?;
                    super::step(format_args!("its agent-control endpoint bound"));
                    Some(bound)
                }
                _ => None,
            };
            let started = wire::Started {
                members: plan.members.clone(),
                published: ports.published(),
                control: control.as_ref().map(|(endpoint, _)| control_at(endpoint)),
                from: launched.from,
                egress: plan.narrowing.clone(),
            }
            .encode()
            .map_err(|e| boot(e.to_string()))?;
            let link = launched
                .link
                .try_clone()
                .map_err(|e| boot(format!("the VM's link: {e}")))?;
            // Its network process's control as the run goes on (D242, D269), and its
            // credentials' leases, where it has grants and so a network process.
            let net = launched
                .net
                .as_ref()
                .map(|(_, control)| super::NetControl::new(control));
            let leasing = net
                .as_ref()
                .filter(|_| plan.credentials.is_some())
                .map(|net| super::Leasing {
                    ledger: Mutex::new(super::Ledger::new(plan.granted.clone(), plan.signing.clone())),
                    net,
                });
            let live = Live {
                answers,
                ends: Ends::kept_in(home),
                flows: Mutex::new(Flows::default()),
                flows_moved: std::sync::Condvar::new(),
                feeds: Mutex::new(plan.inputs.iter().map(|m| (*m, Feed::default())).collect()),
                fed: std::sync::Condvar::new(),
                generations: Mutex::new(Generations::default()),
                generations_moved: std::sync::Condvar::new(),
                capture: capture.map(|(ours, _)| Mutex::new(ours)),
                link: Mutex::new(link),
                held: Mutex::new(Some(HashMap::new())),
                asks_end: Mutex::new(None),
                next_exec: std::sync::atomic::AtomicU64::new(0),
                home,
                members: &plan.members,
                roots: &plan.roots,
                devices: Mutex::new(Some(Vec::new())),
                guest_times: Mutex::new(Some(Vec::new())),
                rates: Mutex::new(Some(Vec::new())),
                lanes: Mutex::new(Some(HashMap::new())),
                next_lane: std::sync::atomic::AtomicU64::new(0),
                plugged: Mutex::new(HashMap::new()),
                plug_turn: Mutex::new(()),
                volumes_held: Mutex::new(volumes),
                notices_due: Mutex::new(due),
                notices: Mutex::new(Vec::new()),
                leasing,
                saving: Mutex::new(super::Saving::Idle),
                pauses: Mutex::new(super::Pauses::new()),
                pause_asks: Mutex::new(Some(Vec::new())),
                reached: &plan.reached,
                agent,
                stdio: std::sync::OnceLock::new(),
                net: net.as_ref(),
                ports: Mutex::new(ports),
                egress: Mutex::new(plan.narrowing.clone()),
                given: Mutex::new(HashMap::new()),
                ceilings: plan.member_ceilings.clone(),
                components: Mutex::new(Components::default()),
                component_image: &|at, digest| {
                    super::component_image(
                        home,
                        plan.prepared.agentfile.as_ref(),
                        &plan.member_ceilings,
                        &plan.members,
                        at,
                        digest,
                    )
                },
                ceiled: plan.ceiling.is_some(),
                agentfile: plan.prepared.agentfile.as_ref(),
                command: plan.command,
                #[cfg(windows)]
                job: &launched.job,
            };
            answers
                .send(wire::to_client::STARTED, &[&started])
                .map_err(|e| boot(format!("its orchestrator: {e}")))?;
            super::step(format_args!("its start told its orchestrator"));
            drop(launched.layer.take());
            super::step(format_args!("its writable layer's name let go of"));
            serve(&live, input, &launched, client, control);
            // What was plugged in goes with the VM: each share process ends as its streams
            // close (`Shares::end`).
            for p in std::mem::take(&mut *lock(&live.plugged)).into_values().flatten() {
                p.share.end(&live.ends);
            }
            Ok(())
        })();
        // Its processes gone, whatever ended it: the VM killed where it runs on, then each
        // reaped once nothing of this process's holds it up, the share process as its
        // connections to the VM close (`Shares::end`); on Windows its job let go of last,
        // which ends whatever of them is left; its anonymous volumes removed, as `--rm`'s
        // are.
        end(home, Some(launched), &plan.made);
        served
    }

    /// Serves the run until it ends and its owner has heard: the run's managed output and
    /// the VM's link on threads of their own, the owner's asks on this one. The VM's end
    /// says the run's (`ENDED`), which has the owner close its channel; `STOP`, or the
    /// channel closed first, kills the VM, whose end then says `Stopped`.
    fn serve(
        live: &Live<'_>,
        input: super::Input,
        launched: &Launched,
        client: Stream,
        control: Option<(shards_remote::control::Endpoint, shards_remote::control::Served)>,
    ) {
        if super::timed() {
            timing_line(&client);
        }
        std::thread::scope(|scope| {
            // Its agent-control endpoint's driver, and its socket lanes, each opened through the
            // VM (D251, D252); revoked as the run's asks end, which ends both.
            let revoke = control.map(|(endpoint, served)| {
                let driver = std::thread::Builder::new()
                    .name("api-run control".into())
                    .spawn_scoped(scope, move || {
                        if let Err(e) = endpoint.run() {
                            super::say(format_args!("its agent-control endpoint ended: {e}"));
                        }
                    });
                if let Err(e) = driver {
                    super::say(format_args!("its agent-control endpoint has no thread: {e}"));
                }
                if let Some(connects) = served.sockets {
                    let lanes = std::thread::Builder::new()
                        .name("api-run lanes".into())
                        .spawn_scoped(scope, move || {
                            for connect in connects {
                                let opening = std::thread::Builder::new()
                                    .name("api-run lane".into())
                                    .spawn_scoped(scope, move || open_lane(live, connect));
                                if let Err(e) = opening {
                                    super::say(format_args!("a socket lane has no thread: {e}"));
                                }
                            }
                        });
                    if let Err(e) = lanes {
                        super::say(format_args!("its socket lanes have no thread: {e}"));
                    }
                }
                // Its members' stdio, where its orchestrator asked for it (D252); its execs, its
                // members' MCP connections (D254) and its members' own lanes, hosted as the
                // daemon hosts its runs' until the endpoint that brings them ends.
                if let Some(agent) = &live.agent {
                    serve_stdio(
                        live,
                        agent,
                        served.generations,
                        served.outputs,
                        served.inputs,
                        scope,
                    );
                    host_lanes(live, agent, served.execs, served.members, scope);
                }
                served.revoke
            });
            // Each stdin its orchestrator writes on this channel; one its endpoint serves is its
            // lane's ([`serve_stdio`]).
            let members: Vec<u32> = match live.stdio.get() {
                Some(_) => Vec::new(),
                None => lock(&live.feeds).keys().copied().collect(),
            };
            for member in members {
                let started = std::thread::Builder::new()
                    .name("api-run input".into())
                    .spawn_scoped(scope, move || write_input(live, member));
                if let Err(e) = started {
                    let _ = writeln!(io::stderr(), "shards api-run: member {member}'s input: {e}");
                    end_input(live, member);
                }
            }
            if let Some(capture) = &live.capture
                && let Ok(reading) = lock(capture).try_clone()
            {
                let started = std::thread::Builder::new()
                    .name("api-run output".into())
                    .spawn_scoped(scope, move || records(live, reading, &launched.vm));
                if let Err(e) = started {
                    let _ = writeln!(io::stderr(), "shards api-run: its members' output: {e}");
                }
            }
            // Its network process's control, read until that process ends with the VM (D242,
            // D269): its asks for leases relayed, and its answers to this host's own asks handed
            // back. With none read, a flow that needs a value waits out the run, and a change of
            // its network is refused.
            if let Some(net) = live.net {
                let started = std::thread::Builder::new()
                    .name("api-run net".into())
                    .spawn_scoped(scope, move || {
                        super::relay(net, live.leasing.as_ref(), live.answers)
                    });
                if let Err(e) = started {
                    let _ = writeln!(io::stderr(), "shards api-run: its network process's control: {e}");
                    net.closed();
                }
            }
            let following = std::thread::Builder::new()
                .name("api-run link".into())
                .spawn_scoped(scope, move || {
                    // Its clone taken and the lock let go of before it is read: a match's
                    // guard would be held through `follow`, and an exec's send wait out the run.
                    let link = lock(&live.link).try_clone();
                    let (end, said, used) = match link {
                        Ok(link) => follow(live, &link, scope),
                        Err(e) => (RunEnd::Failed(format!("the VM's link: {e}")), false, None),
                    };
                    let _ = launched.vm.kill(shards_ipc::signal::SIGKILL);
                    let asks = lock(&live.asks_end).clone();
                    let end = wire::RunEnded {
                        end: super::ended_by(asks.as_ref(), end, said),
                        final_use: super::final_use(used.as_deref(), asks.as_ref()),
                        paused: lock(&live.pauses).paused(Instant::now()).total,
                    };
                    // Every end of the run's comes here, its leases released before it is said:
                    // its own command's, `STOP`, its owner's channel closed, its VM gone.
                    super::end_run(live.answers, live.leasing.as_ref(), &end);
                    net_gone(live, launched);
                });
            if following.is_err() {
                // Nothing would say its end: the run is ended now, and said so.
                let _ = launched.vm.kill(shards_ipc::signal::SIGKILL);
                lock(&live.held).take();
                if let Some(agent) = &live.agent {
                    agent.over();
                }
                let end = wire::RunEnded::unused(
                    RunEnd::Failed("the run host could not follow its VM".into()),
                    wire::Unused::Untold,
                );
                super::end_run(live.answers, live.leasing.as_ref(), &end);
                net_gone(live, launched);
            }
            asks(live, input, &launched.vm, scope);
            // The run is over: its endpoint, and every lane on it, with it.
            drop(revoke);
            lock(&live.lanes).take();
        });
        drop(client);
    }

    /// Says its VM's timing line (`RUN_TIMING`), which comes in its `EXIT` on the run's client
    /// as its command ends, its guest's markers in it: read on a thread of its own, not the
    /// run's scope's, as no `EXIT` comes of a VM killed (a `STOP`) and this process holds the
    /// VM's end of the client until its run is over; it goes as this process does.
    fn timing_line(client: &Stream) {
        let reading = client.try_clone().and_then(|client| {
            std::thread::Builder::new()
                .name("api-run timing".into())
                .spawn(move || {
                    if let Ok(Some(m)) = shards_ipc::recv(&client)
                        && m.kind == kind::EXIT
                        && let Some((_, line)) = m.payload.split_first()
                        && !line.is_empty()
                    {
                        super::say(format_args!(
                            "its VM's timing line: {}",
                            String::from_utf8_lossy(line)
                        ));
                    }
                })
        });
        if let Err(e) = reading {
            super::say(format_args!("its VM's timing line is not read: {e}"));
        }
    }

    /// Its VM gone and its end said, a run's network process, whose control is read for the
    /// run's life (D242, D269), reaped as the run's end reaps it (`netproc::reap`: followed
    /// while it goes on with its end, then ended): so that its control closes, and the reading
    /// of it with it, whatever that process does.
    fn net_gone(live: &Live<'_>, launched: &Launched) {
        if live.net.is_some()
            && let Some((child, _)) = &launched.net
        {
            crate::netproc::reap(child, &live.ends);
        }
    }

    /// The microVM's own key for its run's agent-control endpoint (D252): made once its VM has
    /// started, never in its guest's memory nor a template's, held by its run host until its
    /// endpoint takes it as it binds ([`bind_control`]), then by the endpoint alone. A warm
    /// host makes its one as it waits for its run, which may ask for an endpoint: a host
    /// serves one run, so no two runs share one, and a warm run binds without making one on
    /// its way.
    pub(super) struct ControlKey(shards_remote::identity::Identity);

    impl ControlKey {
        pub(super) fn made() -> Result<ControlKey, String> {
            shards_remote::identity::Identity::generate()
                .map(ControlKey)
                .map_err(|e| format!("its microVM's key: {e}"))
        }

        /// The SHA-256 its controller pins of it, which its endpoint says once bound.
        #[cfg(test)]
        pub(super) fn fingerprint(&self) -> shards_remote::identity::Fingerprint {
            self.0.fingerprint()
        }
    }

    /// The key a run's endpoint takes: the one its warm host made as it waited, else one made
    /// now, its VM started; refused where none can be made.
    pub(super) fn control_key(premade: Option<ControlKey>) -> Result<ControlKey, String> {
        premade.map_or_else(ControlKey::made, Ok)
    }

    /// The run's agent-control endpoint (D252), bound on loopback, as step 5's endpoints bind:
    /// its microVM's own key `key`, which it takes, and the controller's key the one it admits;
    /// serving the run's socket lanes where its ask grants them (D251), its execs, its
    /// members' MCP connections (D254) and its members' own lanes, their streams admitted as
    /// `execs` says, and its members' stdio streams `outputs` and `inputs` (their first
    /// generation's, [`first_streams`]), each admitted as shards-api names it to its controller
    /// (`member_output`, `member_input`), as a daemon's run's endpoint serves its owned ones
    /// (daemon/managed.rs `bind_agent`).
    pub(super) fn bind_control(
        key: ControlKey,
        ask: &wire::ControlAsk,
        execs: shards_remote::control::exec::Admissions,
        outputs: &[Key],
        inputs: &[u32],
    ) -> Result<(shards_remote::control::Endpoint, shards_remote::control::Served), String> {
        let ControlKey(vm) = key;
        let sockets = ask
            .sockets
            .as_ref()
            .map(|grant| shards_remote::control::SocketAdmission {
                lanes: grant.lanes,
                window: grant.window,
            });
        let room = |e: std::collections::TryReserveError| format!("its members' lanes: {e}");
        let mut stdio = Vec::new();
        stdio.try_reserve_exact(outputs.len()).map_err(room)?;
        stdio.extend(outputs.iter().map(|&(member, generation, channel)| {
            shards_api::control::member_output(member, generation, channel)
        }));
        let mut stdins = Vec::new();
        stdins.try_reserve_exact(inputs.len()).map_err(room)?;
        stdins.extend(
            inputs
                .iter()
                .map(|&member| shards_api::control::member_input(member, 0)),
        );
        shards_remote::control::Endpoint::bind(
            &vm,
            ask.controller,
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
            shards_remote::control::Streams {
                outputs: &stdio,
                inputs: &stdins,
                execs: Some(execs),
                sockets,
            },
        )
        .map_err(|e| format!("its agent-control endpoint: {e}"))
    }

    /// The member streams a run's endpoint serves where its orchestrator asked for its members'
    /// stdio (`ControlAsk::stdio`, D252), each its first generation's, in the order the endpoint
    /// is bound with them: every running member's stdout and stderr, and each member's stdin
    /// its orchestrator asked for; none where it asked for none.
    fn first_streams(agent: &Agent) -> Result<(Vec<Key>, Vec<u32>), String> {
        if !agent.stdio {
            return Ok((Vec::new(), Vec::new()));
        }
        let room = |e: std::collections::TryReserveError| format!("its members' lanes: {e}");
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(agent.active.len().saturating_mul(2))
            .map_err(room)?;
        for &member in &agent.active {
            outputs.extend([(member, 0, Channel::Stdout), (member, 0, Channel::Stderr)]);
        }
        let mut inputs = Vec::new();
        inputs.try_reserve_exact(agent.inputs.len()).map_err(room)?;
        inputs.extend_from_slice(&agent.inputs);
        Ok((outputs, inputs))
    }

    /// What the run's orchestrator is told of its endpoint (D252): where it serves, its
    /// microVM's key, and the two other sockets it holds, its waker and its wake.
    pub(super) fn control_at(endpoint: &shards_remote::control::Endpoint) -> wire::ControlAt {
        wire::ControlAt {
            address: endpoint.address,
            vm: endpoint.fingerprint,
            waker: endpoint.waker,
            wake: endpoint.wake,
        }
    }

    /// Opens a socket lane (D251) through the run's VM (`LANE_OPEN`): its connection, as the VM
    /// hands it, given the lane's endpoint; or the errno its guest refused it with. A lane whose
    /// VM is gone is refused ESRCH, as its command is.
    fn open_lane(live: &Live<'_>, connect: shards_remote::control::unix::Connect) {
        use protocol::socket::errno::{EIO, ESRCH};
        use shards_remote::control::unix::Reply;
        // Linux's errnos fit a u16 (MAX_ERRNO, 4095); EIO for one that would not.
        let refuse = |errno: u32| {
            let as_said = |e: u32| u16::try_from(e).ok().and_then(std::num::NonZeroU16::new);
            Reply::Unreached(
                as_said(errno)
                    .or(as_said(EIO))
                    .unwrap_or(std::num::NonZeroU16::MIN),
            )
        };
        let ended = ESRCH;
        // Numbers below 2^63, a VM's own lanes' at or above (shards_ipc LANE_HELD).
        let number = live.next_lane.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & (u64::MAX >> 1);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let registered = lock(&live.lanes)
            .as_mut()
            .is_some_and(|lanes| lanes.insert(number, tx).is_none());
        if !registered {
            let _ = connect.reply(refuse(ended));
            return;
        }
        let ask = [&number.to_be_bytes()[..], connect.name().as_bytes()].concat();
        if let Err(e) = shards_ipc::send(&lock(&live.link), kind::LANE_OPEN, &ask, &[]) {
            super::say(format_args!("asking its VM for a socket lane: {e}"));
            if let Some(lanes) = lock(&live.lanes).as_mut() {
                lanes.remove(&number);
            }
            let _ = connect.reply(refuse(ended));
            return;
        }
        let Ok((payload, fds)) = rx.recv() else {
            let _ = connect.reply(refuse(ended));
            return;
        };
        let errno = payload
            .get(8..12)
            .and_then(|e| <[u8; 4]>::try_from(e).ok())
            .map_or(EIO, u32::from_be_bytes);
        if errno != 0 {
            let _ = connect.reply(refuse(errno));
            return;
        }
        let held = number.to_be_bytes();
        #[cfg(unix)]
        let lane = {
            let lane = fds.into_iter().next().map(Stream::from);
            // Its VM's copy goes, the lane being here (M24).
            let _ = shards_ipc::send(&lock(&live.link), kind::LANE_HELD, &held, &[]);
            lane.ok_or_else(|| "a lane connected without its descriptor".to_string())
        };
        #[cfg(windows)]
        let lane = {
            drop(fds);
            (|| -> Result<_, String> {
                let (ours, theirs) = handed_pair().map_err(|e| e.to_string())?;
                let shared = theirs.share().map_err(|e| e.to_string())?;
                shards_ipc::send(&lock(&live.link), kind::LANE_HELD, &held, &[shared.as_fd()])
                    .map_err(|e| format!("giving its VM the lane's socket: {e}"))?;
                Ok(ours)
            })()
        };
        match lane {
            Ok(lane) => {
                if let Err(shards_remote::control::unix::Gone) = connect.reply(Reply::Connected(lane)) {
                    super::say(format_args!("a socket lane connected after its endpoint ended"));
                }
            }
            Err(why) => {
                super::say(format_args!("a socket lane: {why}"));
                let _ = connect.reply(refuse(EIO));
            }
        }
    }

    /// The VM's answer to socket lane `payload` names (`LANE_OPEN`, D251), to the lane that
    /// waits for it; one no lane waits for is let go, its connection with it.
    fn lane_answered(live: &Live<'_>, payload: Vec<u8>, fds: Vec<OwnedFd>) {
        let Some(number) = payload.first_chunk::<8>().map(|n| u64::from_be_bytes(*n)) else {
            return;
        };
        let waiting = lock(&live.lanes).as_mut().and_then(|lanes| lanes.remove(&number));
        match waiting {
            Some(tx) => {
                let _ = tx.try_send((payload, fds));
            }
            // Nothing waits: its VM's copy goes (M24), and the connection with this one.
            None => {
                let _ = shards_ipc::send(&lock(&live.link), kind::LANE_HELD, &number.to_be_bytes(), &[]);
            }
        }
    }

    /// A member's MCP connection its VM handed over unprompted on `link` (`LANE_ACCEPTED`,
    /// D254): its VM's copy let go once this host has it (M24), and held in `bridges` for its
    /// controller's asks, the number it is held by returned, its watcher its caller's to start
    /// ([`mcp::Bridges::watch`]). On Windows its VM holds it until this host's `LANE_HELD`
    /// brings a socket to relay it onto, as a socket lane's (D251). One its run gave no bridge,
    /// or holds no more, is let go, which ends its member's bridge; one its VM did not number
    /// as its own is let go alone.
    fn mcp_accepted(
        link: &Stream,
        bridges: Option<&mcp::Bridges>,
        payload: &[u8],
        fds: Vec<OwnedFd>,
    ) -> Option<u64> {
        let accepted = shards_ipc::Accepted::decode(payload)?;
        let held = accepted.number.to_be_bytes();
        let let_go = |why: &dyn std::fmt::Display| {
            super::say(format_args!(
                "member {}'s MCP connection let go: {why}",
                accepted.member
            ));
        };
        #[cfg(unix)]
        let conn = {
            let conn = fds.into_iter().next().map(Stream::from);
            if let Err(e) = shards_ipc::send(link, kind::LANE_HELD, &held, &[]) {
                let_go(&format_args!("answering its VM: {e}"));
            }
            conn.ok_or_else(|| "its VM handed no descriptor".to_string())
        };
        #[cfg(windows)]
        let conn = {
            drop(fds);
            let given = (|| -> Result<Stream, String> {
                if !bridges.is_some_and(|b| b.bridged(accepted.member)) {
                    return Err("its run gave it no MCP bridge".into());
                }
                let (ours, theirs) = handed_pair().map_err(|e| e.to_string())?;
                let shared = theirs.share().map_err(|e| e.to_string())?;
                shards_ipc::send(link, kind::LANE_HELD, &held, &[shared.as_fd()])
                    .map_err(|e| format!("giving its VM the connection's socket: {e}"))?;
                Ok(ours)
            })();
            if given.is_err() {
                // Its VM's copy closed: nothing relays it.
                let _ = shards_ipc::send(link, kind::LANE_HELD, &held, &[]);
            }
            given
        };
        let held_here = conn.and_then(|conn| match bridges {
            Some(bridges) => bridges
                .hold(accepted.number, accepted.member, conn)
                .map_err(|why| why.to_string()),
            None => {
                shards_ipc::end(&conn);
                Err("its run gave it no MCP bridge".into())
            }
        });
        match held_here {
            Ok(()) => Some(accepted.number),
            Err(why) => {
                let_go(&why);
                None
            }
        }
    }

    /// Hosts the run's own lanes of its endpoint as the daemon hosts its runs' (daemon/managed):
    /// each exec and MCP connection `execs` brings, and each member's lane `members` brings,
    /// each on threads of its own, until the endpoint that brings them has ended.
    fn host_lanes<'scope, 'env>(
        live: &'env Live<'env>,
        agent: &'env Agent,
        execs: Option<std::sync::mpsc::Receiver<shards_remote::control::exec::Start>>,
        members: Option<std::sync::mpsc::Receiver<shards_remote::control::exec::MemberStart>>,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        if let Some(starts) = execs {
            let hosting = std::thread::Builder::new()
                .name("api-run execs".into())
                .spawn_scoped(scope, move || {
                    execs::host(
                        &starts,
                        &execs::Hosting {
                            admissions: agent.admissions,
                            charge: &agent.charge,
                            launch: &|spec, _, flags, stdio, theirs| {
                                hand_exec(live, agent, spec, flags, stdio, theirs)
                            },
                            logs: None,
                            bridges: agent.bridges.as_ref(),
                            say: &|line| super::say(format_args!("{line}")),
                        },
                    );
                });
            if let Err(e) = hosting {
                super::say(format_args!("its execs have no thread: {e}"));
            }
        }
        if let Some(lanes) = members {
            let hosting = std::thread::Builder::new()
                .name("api-run members".into())
                .spawn_scoped(scope, move || {
                    members::host(
                        &lanes,
                        &members::Hosting {
                            ends: &agent.ends,
                            active: &agent.active,
                            terminals: &agent.terminals,
                            charge: &agent.charge,
                            started: &|| agent.started(),
                            builtin: &|spec, stdio, theirs| {
                                hand_exec(live, agent, spec, shards_ipc::EXEC_TOLD, stdio, theirs)
                            },
                            say: &|line| super::say(format_args!("{line}")),
                        },
                    );
                });
            if let Err(e) = hosting {
                super::say(format_args!("its members' lanes have no thread: {e}"));
            }
        }
    }

    /// Hands the run's VM a lane's exec of `spec` (`EXEC_RUN`, its `flags`) with `stdio` and
    /// `theirs`, as the daemon hands one (health.rs `hand_exec`): once its command has started,
    /// refused once its run is over.
    fn hand_exec(
        live: &Live<'_>,
        agent: &Agent,
        spec: &protocol::Spec,
        flags: u8,
        stdio: [BorrowedFd<'_>; 3],
        theirs: &Stream,
    ) -> io::Result<()> {
        if !agent.started() {
            return Err(io::Error::other("its run does not run"));
        }
        send_exec(live, spec, flags, stdio, theirs)
    }

    /// Sends the run's VM an exec of `spec` (`EXEC_RUN`, its `flags`) with `stdio` and
    /// `theirs`, its client's end of the connection its end is told on, held until the VM says
    /// it has it (`EXEC_TAKEN`, M24); refused once its link has closed.
    fn send_exec(
        live: &Live<'_>,
        spec: &protocol::Spec,
        flags: u8,
        stdio: [BorrowedFd<'_>; 3],
        theirs: &Stream,
    ) -> io::Result<()> {
        let number = live.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut payload = Vec::new();
        // Its number's eight bytes, its flags' one, then its spec.
        payload
            .try_reserve_exact(9 + spec.encoded_len().unwrap_or(0))
            .map_err(io::Error::other)?;
        payload.extend_from_slice(&number.to_be_bytes());
        payload.push(flags);
        spec.encode_into(&mut payload);
        let handed = shards_ipc::handed(theirs)?;
        match lock(&live.held).as_mut() {
            Some(held) => {
                held.insert(number, theirs.try_clone()?);
            }
            None => return Err(io::Error::other("the run has ended")),
        }
        let [stdin, stdout, stderr] = stdio;
        let fds = [handed.as_fd(), stdin, stdout, stderr];
        if let Err(e) = shards_ipc::send(&lock(&live.link), kind::EXEC_RUN, &payload, &fds) {
            if let Some(held) = lock(&live.held).as_mut() {
                held.remove(&number);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Reads the VM's link until it closes: each member's end and the command's readiness
    /// to the orchestrator as they come (`super::relayed`), each member's end kept for its own
    /// lanes, each exec's client end let go of as the VM takes it, each member's MCP connection
    /// held for its controller's asks, its watcher on a thread of `scope`'s (D254), the run's
    /// end from its `DONE` (`super::run_end`), whether its `DONE` came, and what its machine
    /// used in all, where its VM told it as it went (`FINAL_USE`, D260).
    fn follow<'scope, 'env>(
        live: &'env Live<'env>,
        link: &Stream,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> (RunEnd, bool, Option<Vec<u8>>) {
        let mut told = super::Told::default();
        let failed = loop {
            let m = match shards_ipc::recv(link) {
                Ok(Some(m)) => m,
                Ok(None) => break None,
                Err(e) => break Some(format!("the VM's link: {e}")),
            };
            if m.kind == kind::DONE {
                told.done = Some(m.payload);
            } else if m.kind == kind::FINAL_USE {
                told.used = Some(m.payload);
            } else if m.kind == kind::UNSERVED {
                told.unserved = Some(String::from_utf8_lossy(&m.payload).into_owned());
            } else if m.kind == kind::STARTED {
                super::step(format_args!("its VM said STARTED"));
                told.started = true;
                if let Some(agent) = &live.agent {
                    agent.stands(Begun::Started);
                }
                // Its guest told what its host changes in its shares from now on (D235).
                if let Some(pipe) = lock(&live.notices_due).take() {
                    match notice_exec(live, pipe) {
                        Ok(told) => lock(&live.notices).push(told),
                        Err(e) => {
                            super::say(format_args!("its guest is told none of its shares' changes: {e}"))
                        }
                    }
                }
            } else if m.kind == kind::EXEC_TAKEN {
                if let Some(number) = m.payload.first_chunk::<8>()
                    && let Some(held) = lock(&live.held).as_mut()
                {
                    held.remove(&u64::from_be_bytes(*number));
                }
            } else if m.kind == kind::DEVICED {
                answered(&live.devices, &m.payload);
            } else if m.kind == kind::GUEST_TIMED {
                answered(&live.guest_times, &m.payload);
            } else if m.kind == kind::RATED {
                answered(&live.rates, &m.payload);
            } else if m.kind == kind::SAVED {
                lock(&live.saving).answered(&m.payload);
            } else if m.kind == kind::PAUSED {
                answered(&live.pause_asks, &m.payload);
            } else if m.kind == kind::MEMBER_STARTED {
                started_told(live, &m.payload);
            } else if m.kind == kind::WORKDIR_UNENTERED {
                unentered(live, &m.payload);
            } else if m.kind == kind::MANAGED_INPUT {
                input_arrived(live, &m.payload, m.fds);
            } else if m.kind == kind::MANAGED_INPUT_ENDED {
                input_ended(live, &m.payload);
            } else if m.kind == kind::LANE_OPEN {
                lane_answered(live, m.payload, m.fds);
            } else if m.kind == kind::LANE_ACCEPTED {
                let bridges = live.agent.as_ref().and_then(|a| a.bridges.as_ref());
                let held = mcp_accepted(&lock(&live.link), bridges, &m.payload, m.fds);
                if let (Some(number), Some(bridges)) = (held, bridges) {
                    let watching = std::thread::Builder::new()
                        .name("api-run mcp".into())
                        .spawn_scoped(scope, move || bridges.watch(number));
                    if let Err(e) = watching {
                        bridges.release(number);
                        super::say(format_args!("a member's MCP connection has no thread: {e}"));
                    }
                }
            } else {
                // A member's end (D231), kept for its own lanes (D238), and told its
                // orchestrator as each relayed message is.
                if m.kind == kind::MEMBER_ENDED
                    && let Some(agent) = &live.agent
                    && let Some(ended) = protocol::members::Ended::decode(&m.payload)
                    && let Err(e) = agent.ends.told(ended.member, ended.end)
                {
                    super::say(format_args!("a member's end its guest told: {e}"));
                }
                // A generation's end, for the restart that waits on it (D270 slice 3b).
                if m.kind == kind::MEMBER_ENDED
                    && let Some(ended) = protocol::members::Ended::decode(&m.payload)
                {
                    let mut generations = lock(&live.generations);
                    let current = generations.current.get(&ended.member).copied().unwrap_or(0);
                    generations.ended.insert((ended.member, current));
                    drop(generations);
                    live.generations_moved.notify_all();
                }
                if let Some((answer, payload)) = super::relayed(m.kind, &m.payload, |m| live.ended_as(m)) {
                    let _ = live.answers.send(answer, &[&payload]);
                }
            }
        };
        // No VM takes an exec now: their clients' ends go, and each waiting for its exit
        // hears its end.
        lock(&live.held).take();
        // Device steps and guest times no answer will come for: their askers hear the VM has
        // gone.
        *lock(&live.devices) = None;
        *lock(&live.guest_times) = None;
        *lock(&live.rates) = None;
        *lock(&live.pause_asks) = None;
        lock(&live.generations).gone = true;
        live.generations_moved.notify_all();
        lock(&live.saving).gone();
        lock(&live.pauses).gone(Instant::now());
        // Socket lanes no answer will come for: their asks hear the VM has gone.
        lock(&live.lanes).take();
        // Its VM gone: its own lanes hear the run is over, and what its members' bridges held
        // is let go.
        if let Some(agent) = &live.agent {
            agent.over();
        }
        // Its VM gone: no input goes on.
        let members: Vec<u32> = lock(&live.feeds).keys().copied().collect();
        for member in members {
            end_input(live, member);
        }
        let said = told.done.is_some();
        let used = told.used.take();
        (told.end(failed), said, used)
    }

    /// The orchestrator's asks, until its channel closes: credit, stats, stop. `STOP` ends
    /// the run `Stopped`; its channel closed without one (its owner gone), or broken, ends it
    /// failed, saying why, as an ask whose need fails does ([`super::AsksEnd`]); one closed
    /// after the run's end is its owner having heard it.
    fn asks<'scope, 'env>(
        live: &'env Live<'env>,
        mut input: super::Input,
        vm: &'env shards_ipc::Child,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        let ended = loop {
            // Each frame read into the channel's own buffer, and wiped there as it goes once
            // its ask is served (`super::Frame`): a lease's value is never copied.
            let next = input.next();
            let ask = super::read_ask(next.as_ref().map(|f| f.as_ref().map(|f| (f.kind, &**f))));
            if let Err(end) = ask.and_then(|ask| take(live, ask, vm, scope)) {
                break end;
            }
        };
        if let super::AsksEnd::Failed(why) = &ended {
            super::say(format_args!("its asks ended: {why}"));
        }
        // The first end of its asks stands: its members' output's, where that came first.
        lock(&live.asks_end).get_or_insert(ended);
        let _ = vm.kill(shards_ipc::signal::SIGKILL);
    }

    /// How a sign-in's answer to a run that signs nothing in ends its asks: it breaks the channel.
    fn unsigned() -> super::AsksEnd {
        super::broke(format_args!("a sign-in's answer, of a run that signs nothing in"))
    }

    /// Serves the orchestrator's ask `ask`, or says how it ends its asks.
    fn take<'scope, 'env>(
        live: &'env Live<'env>,
        ask: super::Ask<'_>,
        vm: &'env shards_ipc::Child,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<(), super::AsksEnd> {
        use super::{Ask, AsksEnd};
        match ask {
            Ask::Credit(credit) => {
                // Room for a stream its endpoint serves is its lane's (D252).
                if live.stdio.get().is_some() {
                    return Err(super::broke(format_args!(
                        "a CREDIT for member {}'s output, which its run's endpoint serves (D252)",
                        credit.member
                    )));
                }
                let key = (credit.member, credit.generation, credit.channel);
                let mut flows = lock(&live.flows);
                let room = flows.room.entry(key).or_insert(0);
                *room = room.saturating_add(credit.bytes);
                live.flush(&mut flows, key)
                    .map_err(|e| AsksEnd::Failed(format!("its answers to its orchestrator: {e}")))?;
                // Room its VM can take no more of is its VM ending, or ended, its managed
                // output closed before its `DONE` can be: its end is the run's, which `follow`
                // tells, and ending its asks here would kill it before it says that end (WHP
                // at 3a05bc3f1: a template's run of `exit 3` ended before its orchestrator's
                // first credit came). Said once on its stderr, which shows why a run whose
                // output stops while its VM runs on stopped; its `STOP` still ends that run.
                if let Err(e) = live.grant(&mut flows, key)
                    && !std::mem::replace(&mut flows.ungranted, true)
                {
                    super::say(format_args!("its VM takes no more of its members' output: {e}"));
                }
            }
            Ask::Input(given) => {
                input_given(live, given).map_err(|why| super::broke(format_args!("{why}")))?
            }
            Ask::InputEnd(end) => input_end(live, end).map_err(|why| super::broke(format_args!("{why}")))?,
            // A lease the orchestrator answered (D242), relayed to the network process
            // that asked it from the frame itself, wiped as it goes. One of a run with no
            // grants, or to no ask, breaks the channel.
            Ask::Leased(payload) => {
                let Some(leasing) = &live.leasing else {
                    return Err(super::broke(format_args!(
                        "a lease's answer, of a run that asks none"
                    )));
                };
                super::leased(leasing, payload).map_err(|e| super::broke(format_args!("{e}")))?;
            }
            // A sign-in's answer the orchestrator gave (D242, step 8), relayed to the network
            // process that asked it, its token response from the frame itself, wiped as it goes.
            // One of a run with no sign-in, or to no ask, breaks the channel.
            Ask::Captured(payload) => {
                let leasing = live.leasing.as_ref().ok_or_else(unsigned)?;
                super::captured(leasing, payload).map_err(|e| super::broke(format_args!("{e}")))?;
            }
            Ask::Refreshed(payload) => {
                let leasing = live.leasing.as_ref().ok_or_else(unsigned)?;
                super::refreshed(leasing, payload).map_err(|e| super::broke(format_args!("{e}")))?;
            }
            // On a thread of its own: an exec its guest is slow to answer holds up no
            // credit and no stop. Its command's or a member's, an ask of its guest, refused at
            // once while it is paused (D267); the run's answered then from what it said as it
            // paused.
            Ask::Stats(ask) => {
                let vm = vm.id();
                if !matches!(ask.scope, wire::StatsScope::Run) && !lock(&live.pauses).guest_runs() {
                    return refused_paused(live, ask.id);
                }
                let asked = std::thread::Builder::new()
                    .name("api-run stats".into())
                    .spawn_scoped(scope, move || stats(live, ask, vm));
                if asked.is_err()
                    && let Ok(answer) = wire::StatsAnswer::encode(ask.id, Err("no thread to ask it on"))
                {
                    let _ = live.answers.send(wire::to_client::STATS, &[&answer]);
                }
            }
            // On a thread of its own, as stats: a guest slow to hold its run holds up
            // nothing else.
            Ask::Room(ask) => {
                if !lock(&live.pauses).guest_runs() {
                    return refused_paused(live, ask.id);
                }
                let asked = std::thread::Builder::new()
                    .name("api-run room".into())
                    .spawn_scoped(scope, move || held(live, ask.id, room(live, ask.room)));
                if asked.is_err() {
                    held(live, ask.id, Err("no thread to ask it on".into()));
                }
            }
            // On a thread of its own, as a room: a VM slow to answer holds up nothing
            // else.
            Ask::Rate(ask) => {
                let asked = std::thread::Builder::new()
                    .name("api-run rate".into())
                    .spawn_scoped(scope, move || rated(live, ask.id, rate(live, ask.rate)));
                if asked.is_err() {
                    rated(live, ask.id, Err("no thread to ask it on".into()));
                }
            }
            // On a thread of its own, as a room: a guest slow to size a member's terminal
            // holds up nothing else (D244).
            Ask::Resize(ask) => {
                if !lock(&live.pauses).guest_runs() {
                    return refused_paused(live, ask.id);
                }
                let asked = std::thread::Builder::new()
                    .name("api-run resize".into())
                    .spawn_scoped(scope, move || resized(live, ask.id, resize(live, ask)));
                if asked.is_err() {
                    resized(live, ask.id, Err("no thread to ask it on".into()));
                }
            }
            // On a thread of its own, as stats: a plug's guest steps hold up nothing else.
            Ask::Plug(ask) => {
                let id = ask.id;
                if !lock(&live.pauses).guest_runs() {
                    return refused_paused(live, id);
                }
                let asked = std::thread::Builder::new()
                    .name("api-run plug".into())
                    .spawn_scoped(scope, move || plugged(live, id, plug(live, &ask)));
                if asked.is_err() {
                    plugged(
                        live,
                        id,
                        Err((wire::PlugRefusal::Device, "no thread to ask it on".into())),
                    );
                }
            }
            // On a thread of its own, as a plug: a save takes as long as its memory's writing.
            Ask::Save(ask) => {
                let id = ask.id;
                let asked = std::thread::Builder::new()
                    .name("api-run save".into())
                    .spawn_scoped(scope, move || saved(live, id, save(live, &ask.to)));
                if asked.is_err() {
                    saved(
                        live,
                        id,
                        Err((wire::SaveRefusal::State, "no thread to save it on".into())),
                    );
                }
            }
            Ask::Unplug(ask) => {
                let id = ask.id;
                if !lock(&live.pauses).guest_runs() {
                    return refused_paused(live, id);
                }
                let asked = std::thread::Builder::new()
                    .name("api-run unplug".into())
                    .spawn_scoped(scope, move || {
                        let (result, shares) = unplug(live, &ask);
                        plugged(live, id, result);
                        // Each share process ends as its device's stream closes.
                        for share in shares {
                            share.end(&live.ends);
                        }
                    });
                if asked.is_err() {
                    plugged(
                        live,
                        id,
                        Err((wire::PlugRefusal::Device, "no thread to ask it on".into())),
                    );
                }
            }
            Ask::Pause(ask) => pause_asked(live, ask, vm.id(), scope),
            // On a thread of its own, as a plug: a network process slow to answer holds up
            // nothing else. Made while the run is paused too: its network process makes it,
            // which the pause does not hold (D267, D269).
            Ask::Net(ask) => {
                let id = ask.id;
                let asked = std::thread::Builder::new()
                    .name("api-run net".into())
                    .spawn_scoped(scope, move || net_change(live, ask));
                if asked.is_err() {
                    net_answered(
                        live,
                        id,
                        Err((wire::NetRefusal::Network, "no thread to make it on".into())),
                    );
                }
            }
            // On a thread of its own, as a plug: a member's graceful end may take as long as
            // its patience (D270 slice 3b).
            Ask::Restart(ask) => {
                let id = ask.id;
                let asked = std::thread::Builder::new()
                    .name("api-run restart".into())
                    .spawn_scoped(scope, move || restarted(live, id, restart(live, &ask, scope)));
                if asked.is_err() {
                    restarted(
                        live,
                        id,
                        Err((wire::RestartRefusal::Failed, "no thread to restart it on".into())),
                    );
                }
            }
            // On a thread of its own, as a restart: its image may be built, and its member
            // restarted (D270 slice 4).
            Ask::Swap(ask) => {
                let id = ask.id;
                let asked = std::thread::Builder::new()
                    .name("api-run swap".into())
                    .spawn_scoped(scope, move || swapped(live, id, swap(live, &ask, scope)));
                if asked.is_err() {
                    swapped(
                        live,
                        id,
                        Err((wire::SwapRefusal::Failed, "no thread to swap it on".into())),
                    );
                }
            }
        }
        Ok(())
    }

    /// Ask `id` refused at once, its run paused or pausing (`ASK_PAUSED`, D267).
    fn refused_paused(live: &Live<'_>, id: u32) -> Result<(), super::AsksEnd> {
        let _ = live
            .answers
            .send(wire::to_client::ASK_PAUSED, &[&wire::AskPaused { id }.encode()]);
        Ok(())
    }

    /// A pause or a resume asked (D267), queued behind those before it and applied in their
    /// order by one worker on a thread of its own, made as one is asked with none at work and
    /// ending once none is left: a pause waits on its guest's asks and its VM, which hold up no
    /// other ask.
    fn pause_asked<'scope, 'env>(
        live: &'env Live<'env>,
        ask: wire::PauseAsk,
        vm: u32,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        {
            let mut pauses = lock(&live.pauses);
            pauses.queue.push_back(ask);
            if std::mem::replace(&mut pauses.draining, true) {
                return;
            }
        }
        let worked = std::thread::Builder::new()
            .name("api-run pause".into())
            .spawn_scoped(scope, move || pauses_worked(live, vm, scope));
        if worked.is_err() {
            let asked: Vec<wire::PauseAsk> = {
                let mut pauses = lock(&live.pauses);
                pauses.draining = false;
                pauses.queue.drain(..).collect()
            };
            for ask in asked {
                pause_answered(live, ask.id, Err("no thread to ask it on".into()), vm);
            }
        }
    }

    /// Applies the pauses and resumes asked, in their order, until none is left: VM process `vm`
    /// held while each answer says the run is paused.
    fn pauses_worked<'scope, 'env>(
        live: &'env Live<'env>,
        vm: u32,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) {
        loop {
            let next = {
                let mut pauses = lock(&live.pauses);
                let next = pauses.queue.pop_front();
                if next.is_none() {
                    pauses.draining = false;
                }
                next
            };
            let Some(ask) = next else {
                return;
            };
            let done = if ask.pause {
                pause(live)
            } else {
                resume(live, scope)
            };
            pause_answered(live, ask.id, done, vm);
        }
    }

    /// A pause's or a resume's outcome, told its owner (`PAUSED`), with VM process `vm` where
    /// it says the run is paused.
    fn pause_answered(live: &Live<'_>, id: u32, result: Result<wire::Paused, String>, vm: u32) {
        let held = match &result {
            Ok(paused) if paused.now => vec![vm],
            _ => Vec::new(),
        };
        if let Ok(payload) = (wire::PauseAnswer { id, result, held }).encode() {
            let _ = live.answers.send(wire::to_client::PAUSED, &[&payload]);
        }
    }

    /// The run paused (D267): asks of its guest refused from now, its VM asked to hold every
    /// vCPU at once (`PAUSE`, as the daemon's `pause` asks it), its paused time counted from its
    /// word. Nothing of it waits on its guest, which may be slow or hung: an ask of it already
    /// under way is answered as it goes on. Done already where it is paused.
    fn pause(live: &Live<'_>) -> Result<wire::Paused, String> {
        match lock(&live.pauses).pause(Instant::now()) {
            super::Step::Done(paused) => return Ok(paused),
            super::Step::Gone => return Err("its microVM has stopped".into()),
            super::Step::Ask => {}
        }
        let held = ask_pause(live, true);
        let mut pauses = lock(&live.pauses);
        let paused = match held {
            Ok(()) => pauses
                .held(Instant::now())
                .ok_or_else(|| "its microVM has stopped".to_string()),
            Err(why) => {
                pauses.unheld();
                Err(why)
            }
        };
        drop(pauses);
        paused
    }

    /// The paused run goes on where it stopped (D267): its VM asked to let its vCPUs go
    /// (`PAUSE`), the pause's time ended at its word; then its init asked, on a thread of its
    /// own, to set its clock to its host's (`builtin::CLOCK`, as a restored VM sets its own), so
    /// that a step its host's clock took meanwhile is its too, holding up nothing. Done already
    /// where it runs.
    fn resume<'scope, 'env>(
        live: &'env Live<'env>,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<wire::Paused, String> {
        match lock(&live.pauses).resume(Instant::now()) {
            super::Step::Done(paused) => return Ok(paused),
            super::Step::Gone => return Err("its microVM has stopped".into()),
            super::Step::Ask => {}
        }
        let went = ask_pause(live, false);
        let mut pauses = lock(&live.pauses);
        let resumed = match went {
            Ok(()) => pauses
                .resumed(Instant::now())
                .ok_or_else(|| "its microVM has stopped".to_string()),
            Err(why) => {
                pauses.unresumed();
                Err(why)
            }
        };
        drop(pauses);
        if resumed.is_ok() {
            let synced = std::thread::Builder::new()
                .name("api-run clock".into())
                .spawn_scoped(scope, move || {
                    match builtin(live, protocol::builtin::CLOCK, Vec::new()) {
                        Ok(Said { status: Some(0), .. }) => {}
                        Ok(Said { err, .. }) => super::say(format_args!(
                            "setting its clock as it went on: {}",
                            String::from_utf8_lossy(&err).trim_end()
                        )),
                        Err(e) => super::say(format_args!("setting its clock as it went on: {e}")),
                    }
                });
            if let Err(e) = synced {
                super::say(format_args!("setting its clock as it went on: {e}"));
            }
        }
        resumed
    }

    /// Asks the VM to hold every vCPU out of its guest (`frozen`) or let them go on, as the
    /// daemon's `pause_vm` asks it (`PAUSE`: whether, then its number), and waits for its answer
    /// (`PAUSED`: its number, then nothing, or why not), which comes as long as its link is open.
    fn ask_pause(live: &Live<'_>, frozen: bool) -> Result<(), String> {
        let number = live.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = std::sync::mpsc::channel();
        match lock(&live.pause_asks).as_mut() {
            Some(asked) => asked.push((number, told)),
            None => return Err(Unheld::stopped().0),
        }
        let ask = [&[u8::from(frozen)][..], &number.to_be_bytes()].concat();
        if let Err(e) = shards_ipc::send(&lock(&live.link), kind::PAUSE, &ask, &[]) {
            if let Some(asked) = lock(&live.pause_asks).as_mut() {
                asked.retain(|(n, _)| *n != number);
            }
            return Err(Unheld::unsent(&e).0);
        }
        answer
            .recv()
            .unwrap_or_else(|_| Err(Unheld::stopped()))
            .map(drop)
            .map_err(|why| why.said().0)
    }

    /// A member's input connection, as its VM handed it over (`MANAGED_INPUT`): on Unix beside
    /// the message, its VM's copy going as the VM is told this host has it (M24); on Windows,
    /// where a VM gives its host no handle, a pipe this host makes, whose other end the VM is
    /// given to relay the connection onto, as the daemon gives one (daemon.rs). Its
    /// orchestrator is then given room for its first part. One the run does not await is
    /// closed, or given no pipe: its init then tells its input's end.
    fn input_arrived(live: &Live<'_>, payload: &[u8], fds: Vec<OwnedFd>) {
        let member = <[u8; 4]>::try_from(payload).map(u32::from_be_bytes).ok();
        let awaited = member.filter(|m| {
            lock(&live.feeds)
                .get(m)
                .is_some_and(|f| matches!(f.conn, Conn::Awaited) && !f.ended)
        });
        #[cfg(unix)]
        let conn = {
            if let Err(e) = shards_ipc::send(&lock(&live.link), kind::MANAGED_INPUT, payload, &[]) {
                let _ = writeln!(io::stderr(), "shards api-run: answering a member's input: {e}");
            }
            fds.into_iter().next().map(Stream::from)
        };
        #[cfg(windows)]
        let conn = {
            drop(fds);
            let given = awaited
                .ok_or("an input the run does not await".to_string())
                .and_then(|_| {
                    let (ours, theirs) = handed_pair().map_err(|e| e.to_string())?;
                    let shared = theirs.share().map_err(|e| e.to_string())?;
                    shards_ipc::send(&lock(&live.link), kind::MANAGED_INPUT, payload, &[shared.as_fd()])
                        .map_err(|e| format!("giving its VM its pipe: {e}"))?;
                    Ok(ours)
                });
            match given {
                Ok(ours) => Some(ours),
                Err(why) => {
                    let _ = writeln!(io::stderr(), "shards api-run: a member's input connection: {why}");
                    let _ = shards_ipc::send(&lock(&live.link), kind::MANAGED_INPUT, payload, &[]);
                    None
                }
            }
        };
        let (Some(member), Some(conn)) = (awaited, conn) else {
            let _ = writeln!(
                io::stderr(),
                "shards api-run: a member's input connection the run does not await, or without its descriptor"
            );
            return;
        };
        let room = protocol::CHUNK as u64;
        // A stdin its endpoint serves takes its room from its lane (D252): none is told here.
        let lanes = live.stdio.get().is_some();
        let generation = {
            let mut feeds = lock(&live.feeds);
            let Some(feed) = feeds
                .get_mut(&member)
                .filter(|f| matches!(f.conn, Conn::Awaited) && !f.ended)
            else {
                shards_ipc::end(&conn);
                return;
            };
            feed.conn = Conn::Here(conn);
            if !lanes {
                feed.room = feed.room.saturating_add(room);
            }
            feed.generation
        };
        live.fed.notify_all();
        if lanes {
            return;
        }
        let given = wire::InputRoom {
            member,
            generation,
            bytes: room,
        };
        let _ = live.answers.send(wire::to_client::INPUT_ROOM, &[&given.encode()]);
    }

    /// A member's input's end, as its init told it (`MANAGED_INPUT_ENDED`): its orchestrator
    /// told, its connection ended here.
    fn input_ended(live: &Live<'_>, payload: &[u8]) {
        let Ok(ended) = managed::InputEnded::decode(payload) else {
            let _ = writeln!(
                io::stderr(),
                "shards api-run: a member's input end said malformed"
            );
            return;
        };
        let generation = lock(&live.feeds).get(&ended.member).map_or(0, |f| f.generation);
        end_input(live, ended.member);
        // A stdin its endpoint serves (D252): its end told its lane alone.
        if let Some(stdio) = live.stdio.get() {
            stdio.input_ended((ended.member, generation), ended.written, ended.end);
            return;
        }
        let told = wire::InputEnded {
            member: ended.member,
            generation,
            written: ended.written,
            end: match ended.end {
                managed::InputEnd::Closed => wire::StreamEnd::Closed,
                managed::InputEnd::Aborted(errno) => wire::StreamEnd::Aborted(errno),
            },
        };
        let _ = live.answers.send(wire::to_client::INPUT_ENDED, &[&told.encode()]);
    }

    /// Ends member `member`'s input here: nothing more goes, its connection ended, its
    /// writer told.
    fn end_input(live: &Live<'_>, member: u32) {
        if let Some(feed) = lock(&live.feeds).get_mut(&member) {
            feed.ended = true;
            feed.queued.clear();
            feed.conn.end();
        }
        live.fed.notify_all();
    }

    /// The orchestrator's bytes for a member's stdin, within the room it was given: queued
    /// for its writer. Past its room, after its end, or for a member with no input, its
    /// contract is broken, as said.
    fn input_given(live: &Live<'_>, given: wire::Input) -> Result<(), String> {
        let member = given.member;
        if live.stdio.get().is_some() {
            return Err(format!(
                "an INPUT for member {member}, whose stdin its run's endpoint serves (D252)"
            ));
        }
        let mut feeds = lock(&live.feeds);
        let feed = feeds
            .get_mut(&member)
            .ok_or_else(|| format!("an INPUT for member {member}, whose stdin its run did not ask for"))?;
        // Bytes for another generation's stdin than the one it has now go nowhere (D270).
        if given.generation != feed.generation {
            return Ok(());
        }
        let n = u64::try_from(given.bytes.0.len())
            .map_err(|_| format!("member {member}'s INPUT has no length"))?;
        if n > feed.room {
            return Err(format!(
                "member {member}'s INPUT of {n} bytes, past the {} its room holds",
                feed.room
            ));
        }
        if feed.end.is_some() {
            return Err(format!("member {member}'s INPUT after its end"));
        }
        feed.room -= n;
        if !feed.ended && n > 0 {
            feed.queued.push_back(given.bytes);
        }
        drop(feeds);
        live.fed.notify_all();
        Ok(())
    }

    /// The orchestrator's end of a member's stdin, or its abort, which ends its connection at
    /// once, so that a writer held by a member that does not read lets go.
    fn input_end(live: &Live<'_>, end: wire::InputEnd) -> Result<(), String> {
        let member = end.member;
        if live.stdio.get().is_some() {
            return Err(format!(
                "an INPUT_END for member {member}, whose stdin its run's endpoint serves (D252)"
            ));
        }
        let mut feeds = lock(&live.feeds);
        let feed = feeds.get_mut(&member).ok_or_else(|| {
            format!("an INPUT_END for member {member}, whose stdin its run did not ask for")
        })?;
        if end.generation != feed.generation {
            return Ok(());
        }
        if feed.end.replace(end.end).is_some() {
            return Err(format!("member {member}'s input ended twice"));
        }
        if matches!(end.end, wire::StreamEnd::Aborted(_)) {
            feed.queued.clear();
            if let Conn::Here(conn) = &feed.conn {
                shards_ipc::end(conn);
            }
        }
        drop(feeds);
        live.fed.notify_all();
        Ok(())
    }

    /// What a member's input writer does next.
    enum Write {
        Part(Stream, wire::Secret),
        End(Stream),
        Done,
    }

    /// Writes member `member`'s input to its connection as it comes, each part one `STDIN`
    /// frame, the orchestrator given room for each part once it is in the connection, and its
    /// end an empty frame; ends with its input. A part the connection refuses is not
    /// delivered, nor any after: its init tells how the input ended.
    fn write_input(live: &Live<'_>, member: u32) {
        loop {
            let next = {
                let mut feeds = lock(&live.feeds);
                loop {
                    let Some(feed) = feeds.get_mut(&member) else {
                        break Write::Done;
                    };
                    if feed.ended {
                        break Write::Done;
                    }
                    if let Conn::Here(conn) = &feed.conn {
                        let wants = !feed.queued.is_empty() || feed.end == Some(wire::StreamEnd::Closed);
                        if wants {
                            let Ok(conn) = conn.try_clone() else {
                                break Write::Done;
                            };
                            break match feed.queued.pop_front() {
                                Some(part) => Write::Part(conn, part),
                                None => Write::End(conn),
                            };
                        }
                    }
                    feeds = live.fed.wait(feeds).unwrap_or_else(PoisonError::into_inner);
                }
            };
            match next {
                Write::Part(conn, part) => {
                    let sent = u32::try_from(part.0.len()).ok().filter(|len| {
                        (&conn)
                            .write_all(&protocol::header(protocol::kind::STDIN, *len))
                            .and_then(|()| (&conn).write_all(&part.0))
                            .is_ok()
                    });
                    let Some(len) = sent else {
                        // Its init tells how the input ended; nothing more goes meanwhile.
                        if let Some(feed) = lock(&live.feeds).get_mut(&member) {
                            feed.queued.clear();
                            feed.conn.end();
                        }
                        continue;
                    };
                    let generation = match lock(&live.feeds).get_mut(&member) {
                        Some(feed) => {
                            feed.room = feed.room.saturating_add(u64::from(len));
                            feed.generation
                        }
                        None => 0,
                    };
                    let room = wire::InputRoom {
                        member,
                        generation,
                        bytes: u64::from(len),
                    };
                    let _ = live.answers.send(wire::to_client::INPUT_ROOM, &[&room.encode()]);
                }
                Write::End(conn) => {
                    // Every part before it is in the connection; its init closes the
                    // member's stdin, then tells the end.
                    let _ = (&conn).write_all(&protocol::header(protocol::kind::STDIN, 0));
                    return;
                }
                Write::Done => return,
            }
        }
    }

    /// Reads the run's managed output until it ends: each member's bytes to the
    /// orchestrator within its room, its channel's end, each record's credit renewed. Its
    /// VM's end of it, at a frame's start or within one, is the VM's end, which `follow`
    /// tells. Where this host cannot take what comes (a frame of no record's kind, cut short
    /// in its header or malformed, a record it cannot hold or read) or cannot send its
    /// orchestrator what it has, the run fails in those words: its asks' end
    /// ([`super::AsksEnd`]), and `vm`, its VM, killed for it, so that no run waits for its
    /// `STOP` on a failure its host saw (D258). However its reads end, every channel without
    /// an end is then ended cut (`Live::cut`), what this host held of it first.
    fn records(live: &Live<'_>, capture: Stream, vm: &shards_ipc::Child) {
        let read = read_records(live, capture);
        let cut = live
            .cut()
            .map_err(|e| format!("its answers to its orchestrator: {e}"));
        if let Err(why) = read.and(cut) {
            super::say(format_args!("its asks ended: {why}"));
            lock(&live.asks_end).get_or_insert(super::AsksEnd::Failed(why));
            let _ = vm.kill(shards_ipc::signal::SIGKILL);
        }
    }

    /// [`records`]' reads, to their end: its VM's, or why not.
    fn read_records(live: &Live<'_>, mut capture: Stream) -> Result<(), String> {
        let broke = |why: std::fmt::Arguments<'_>| format!("its VM's members' output broke: {why}");
        loop {
            let mut header = [0u8; protocol::HEADER];
            if capture.read_exact(&mut header).is_err() {
                return Ok(());
            }
            let (which, len) =
                protocol::parse_header(header).ok_or_else(|| broke(format_args!("a frame's header")))?;
            if which != protocol::kind::MANAGED {
                return Err(broke(format_args!("a frame of kind {which}, no record's")));
            }
            let mut record = Vec::new();
            record
                .try_reserve_exact(len as usize)
                .map_err(|e| broke(format_args!("no room for a record of {len} bytes: {e}")))?;
            record.resize(len as usize, 0);
            if capture.read_exact(&mut record).is_err() {
                return Ok(());
            }
            let parsed =
                managed::Record::decode(&record).map_err(|e| broke(format_args!("a record: {e}")))?;
            // Its stream's generation, fixed as it opened (D270): a record of one that comes
            // after its member's next generation started is still its own's.
            let channel = channel_of(parsed.channel);
            let generation = {
                let mut generations = lock(&live.generations);
                let current = generations.current.get(&parsed.member).copied().unwrap_or(0);
                if matches!(parsed.event, managed::Event::Open(_)) {
                    generations.streams.insert((parsed.member, channel), current);
                    current
                } else {
                    generations
                        .streams
                        .get(&(parsed.member, channel))
                        .copied()
                        .unwrap_or(current)
                }
            };
            let key = (parsed.member, generation, channel);
            let end = match parsed.event {
                managed::Event::End => Some(wire::StreamEnd::Closed),
                managed::Event::Aborted(errno) => Some(wire::StreamEnd::Aborted(errno)),
                managed::Event::Open(_) | managed::Event::Data(_) => None,
            };
            let mut flows = lock(&live.flows);
            flows.granted.insert(key, false);
            // A member's stream its endpoint serves (D252): the record whole to its lane's
            // thread, which credits the next as its lane has room ([`lane_output`]); its end
            // told on this channel too, as a channel's is, for its orchestrator's
            // `wait_channel`.
            if let Some(stdio) = live.stdio.get() {
                if !stdio.record(key, record, end.is_some()) {
                    super::say(format_args!(
                        "member {}'s output came with no lane to take it",
                        key.0
                    ));
                }
                if let Some(end) = end {
                    live.end_channel(&mut flows, key, end)
                        .map_err(|e| format!("its answers to its orchestrator: {e}"))?;
                }
                continue;
            }
            let sent = match parsed.event {
                managed::Event::Open(_) => Ok(()),
                managed::Event::Data(bytes) => {
                    flows.waiting.entry(key).or_default().extend_from_slice(bytes);
                    live.flush(&mut flows, key)
                }
                managed::Event::End => live.end_channel(&mut flows, key, wire::StreamEnd::Closed),
                managed::Event::Aborted(errno) => {
                    live.end_channel(&mut flows, key, wire::StreamEnd::Aborted(errno))
                }
            };
            sent.map_err(|e| format!("its answers to its orchestrator: {e}"))?;
            // Room its VM can take no more of is its VM ending, its end the VM's (`take`'s
            // credits likewise), said once.
            if let Err(e) = live.grant(&mut flows, key)
                && !std::mem::replace(&mut flows.ungranted, true)
            {
                super::say(format_args!("its VM takes no more of its members' output: {e}"));
            }
        }
    }

    /// Answers `ask` with init's `STATS` of its scope, through an exec on the VM's link as
    /// a health check's exec is sent (daemon/health.rs `send_exec`), on a thread of its own;
    /// for the run's, beside it what its microVM costs its host (D43's `microvm`, D241): its
    /// vCPUs' guest time, then its VM process `vm`'s use, which holds that time, so that the
    /// later read is never the less.
    ///
    /// The run's, while it is paused (D267), with what its init last said of it before the pause,
    /// its guest running nothing since, its microVM read as it stands; its command's and a
    /// member's are refused before this where it is paused. Each run's init answers is kept as
    /// its last.
    fn stats(live: &Live<'_>, ask: wire::StatsAsk, vm: u32) {
        let answer = |result: Result<wire::StatsSaid<'_>, &str>| {
            if let Ok(payload) = wire::StatsAnswer::encode(ask.id, result) {
                let _ = live.answers.send(wire::to_client::STATS, &[&payload]);
            }
        };
        let microvm = || wire::Microvm {
            guest: guest_times(live),
            host: host_use(vm),
        };
        let run = matches!(ask.scope, wire::StatsScope::Run);
        if run {
            let pauses = lock(&live.pauses);
            if let Some(said) = pauses.said() {
                // Its init's last lines, or none where it said none before: its microVM's
                // part stands either way.
                let lines = said.map(<[u8]>::to_vec).unwrap_or_default();
                let paused = pauses.paused(Instant::now());
                drop(pauses);
                return answer(Ok((&lines, Some(&microvm()), Some(&paused))));
            }
        }
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
            }) => {
                let microvm = run.then(microvm);
                let paused = run.then(|| {
                    let mut pauses = lock(&live.pauses);
                    pauses.last = Some(out.clone());
                    pauses.paused(Instant::now())
                });
                answer(Ok((&out, microvm.as_ref(), paused.as_ref())))
            }
            Ok(Said { err, .. }) => answer(Err(String::from_utf8_lossy(&err).trim_end())),
            Err(e) => answer(Err(&e)),
        }
    }

    /// The room its orchestrator's budget leaves the run (D241), told its init as the
    /// daemon tells a run of its budget's (daemon/budget.rs, `builtin::ROOM`): what init
    /// holds it within past what it uses, none where no budget holds it, or why not.
    fn room(live: &Live<'_>, room: Option<i64>) -> Result<Option<u64>, String> {
        let args = room.map(|r| r.to_string().into_bytes()).into_iter().collect();
        match builtin(live, protocol::builtin::ROOM, args)? {
            Said {
                status: Some(0), out, ..
            } => super::held_of(room, &out),
            Said { err, .. } => Err(String::from_utf8_lossy(&err).trim_end().to_string()),
        }
    }

    /// The CPU rate its orchestrator holds the run's vCPUs to (D247), told its VM as the
    /// daemon tells a run's its CPUs (`CPUS`, D191) but answered (`RATED`): what the VM
    /// holds them to, none where it lets them be, and the period it holds over; or why not.
    fn rate(live: &Live<'_>, rate: Option<std::num::NonZeroU64>) -> Result<wire::Rated, String> {
        let milli = rate.map_or(0, std::num::NonZeroU64::get);
        let said = ask_vm(live, &live.rates, kind::RATE, &milli.to_be_bytes(), &[])?;
        super::rated_of(rate, &said)
    }

    /// A rate's outcome, told its orchestrator (`RATED`).
    fn rated(live: &Live<'_>, id: u32, result: Result<wire::Rated, String>) {
        if let Ok(payload) = (wire::RateAnswer { id, result }).encode() {
            let _ = live.answers.send(wire::to_client::RATED, &[&payload]);
        }
    }

    /// The run's microVM saved whole at `to`, its owner's ask (D259), as the daemon's `save vm`
    /// saves one (D176, daemon/snapshots.rs): one at a time, where its owner may have it
    /// (`super::destination`), in a directory made for it; refused, typed, by what refused it,
    /// and what it made removed.
    fn save(live: &Live<'_>, to: &str) -> Result<(), (wire::SaveRefusal, String)> {
        if !shards_vmm::vm::SNAPSHOTS {
            return Err((
                wire::SaveRefusal::Unsupported,
                "this host's microVMs take no snapshots".into(),
            ));
        }
        let mut reached = live.reached.to_vec();
        reached.extend(lock(&live.plugged).values().flatten().map(|p| p.host.clone()));
        let target = super::destination(Path::new(to), live.home, &reached)?;
        let said = lock(&live.saving).ask()?;
        if let Err(refused) = super::made(&target) {
            lock(&live.saving).unasked();
            return Err(refused);
        }
        let filled = fill(live, &target, &said);
        if filled.is_err() {
            // Nothing of a save that failed stays, as nothing of a refused `save vm`'s does.
            let _ = std::fs::remove_dir_all(&target);
        }
        filled
    }

    /// Fills a save's directory `target`: a generation `shards_vmm::snapshot::prepare` stages
    /// there, its files handed to the run's VM, which parks its vCPUs, quiets its devices and
    /// its guest, fills them and goes on (`SAVE`, left running), its answer heard by `said`;
    /// then committed, synced and pointed at, as the daemon commits its saves.
    fn fill(
        live: &Live<'_>,
        target: &Path,
        said: &super::SaveSaid,
    ) -> Result<(), (wire::SaveRefusal, String)> {
        use wire::SaveRefusal as R;
        let staged =
            shards_vmm::snapshot::prepare(target, true).inspect_err(|_| lock(&live.saving).unasked());
        let staged = staged.map_err(|e| (R::Failed, e))?;
        let asked = (|| -> Result<(), (R, String)> {
            let files = staged.handed().map_err(|e| (R::Failed, e))?;
            let fds: Vec<BorrowedFd<'_>> = files.iter().map(|f| f.as_fd()).collect();
            // The tokens its guest would dial its host again with, restored from it (D176):
            // fresh, and kept nowhere, as no save of an orchestrator's run is restored.
            let mut tokens = [0u8; 2 * shards_abi::run::TOKEN];
            shards_vmm::platform::fill_random(&mut tokens)
                .map_err(|e| (R::Failed, format!("its tokens: {e}")))?;
            let snapshot = target
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let mut payload = crate::daemon::snapshots::save_ask(
                shards_ipc::SAVE_LEAVE_RUNNING,
                &tokens,
                [staged.name(), &snapshot, ""],
            );
            zeroize::Zeroize::zeroize(&mut tokens);
            let sent = shards_ipc::send(&lock(&live.link), kind::SAVE, &payload, &fds);
            zeroize::Zeroize::zeroize(&mut payload);
            sent.map_err(|e| (R::State, format!("asking its microVM: {e}")))
        })();
        if let Err(refused) = asked {
            lock(&live.saving).unasked();
            return Err(refused);
        }
        // As long as its memory's writing takes: its VM answers, or its link closes.
        match said.recv() {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Err((R::Failed, why)),
            Err(_) => return Err((R::State, "its microVM stopped".into())),
        }
        staged.commit().map_err(|e| (R::Failed, e))
    }

    /// A save's outcome, told its owner (`SAVED`).
    fn saved(live: &Live<'_>, id: u32, result: Result<(), (wire::SaveRefusal, String)>) {
        if let Ok(payload) = (wire::SaveAnswer { id, result }).encode() {
            let _ = live.answers.send(wire::to_client::SAVED, &[&payload]);
        }
    }

    /// A room's outcome, told its orchestrator (`HELD`).
    fn held(live: &Live<'_>, id: u32, result: Result<Option<u64>, String>) {
        if let Ok(payload) = (wire::RoomAnswer { id, result }).encode() {
            let _ = live.answers.send(wire::to_client::HELD, &[&payload]);
        }
    }

    /// A member's own terminal sized (plan step 6, D244), as its init's `RESIZE` built-in
    /// sizes it; refused in its init's words.
    fn resize(live: &Live<'_>, ask: wire::ResizeAsk) -> Result<(), String> {
        let size = protocol::Size {
            rows: ask.rows,
            cols: ask.cols,
        };
        let args = protocol::members::resize_args(ask.member, size);
        match builtin(live, protocol::builtin::RESIZE, args)? {
            Said { status: Some(0), .. } => Ok(()),
            Said { err, .. } => Err(String::from_utf8_lossy(&err).trim_end().to_string()),
        }
    }

    fn resized(live: &Live<'_>, id: u32, result: Result<(), String>) {
        if let Ok(payload) = (wire::ResizeAnswer { id, result }).encode() {
            let _ = live.answers.send(wire::to_client::RESIZED, &[&payload]);
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
                send_exec(
                    live,
                    &spec,
                    0,
                    [null.as_fd(), out_w.as_fd(), err_w.as_fd()],
                    &theirs,
                )
                .map_err(|e| e.to_string())?;
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

    /// Starts the guest's NOTICE built-in (D235): its stdin `pipe`, on which a share
    /// process tells what its host changes, its output this process's stderr; the stream
    /// its end is told on, to be held while it runs.
    fn notice_exec(live: &Live<'_>, pipe: OwnedFd) -> Result<Stream, String> {
        let spec = protocol::Spec {
            builtin: protocol::builtin::NOTICE,
            ..Default::default()
        };
        let (client, theirs) = Stream::pair().map_err(|e| e.to_string())?;
        let err = io::stderr();
        send_exec(
            live,
            &spec,
            shards_ipc::EXEC_INTERACTIVE,
            [pipe.as_fd(), err.as_fd(), err.as_fd()],
            &theirs,
        )
        .map_err(|e| e.to_string())?;
        Ok(client)
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

    /// `ask`'s source plugged into each of its members (D235, D271), as `shards plug volume
    /// --for` plugs one into a daemon's run (daemon/devices.rs): checked here, and by its
    /// guest's init again; each member's its own device, served by a share process of its own,
    /// plugged into the microVM's bus and mounted in the member at its path by init, the host's
    /// files the member's. A member that refuses it has those given it first let go of it.
    fn plug(live: &Live<'_>, ask: &wire::PlugAsk) -> Result<(), (wire::PlugRefusal, String)> {
        use wire::PlugRefusal as R;
        for &index in &ask.members {
            let member = live
                .members
                .iter()
                .find(|m| m.index == index)
                .ok_or_else(|| (R::Request, format!("member {index}: the image declares none")))?;
            if !member.runs {
                return Err((
                    R::Member,
                    format!("{}: files alone, it runs nothing", member.name),
                ));
            }
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
        let _turn = lock(&live.plug_turn);
        if lock(&live.plugged).contains_key(&ask.at) {
            return Err((R::Taken, format!("{} has something plugged in already", ask.at)));
        }
        let (host, bound) = source_dir(live, &ask.from, &ask.at)?;
        let mut given: Vec<Plugged> = Vec::with_capacity(ask.members.len());
        for &member in &ask.members {
            match plug_one(
                live,
                member,
                &host,
                bound.as_ref(),
                &ask.at,
                ask.read_only,
                ask.exec,
            ) {
                Ok(plugged) => given.push(plugged),
                Err(refused) => {
                    // Those given it first let go of it: none of theirs has used it yet, and
                    // one that has keeps it, for an unplug to let go of.
                    let (kept, _, gone) = unplug_each(live, given, &ask.at);
                    for share in gone {
                        share.end(&live.ends);
                    }
                    if !kept.is_empty() {
                        lock(&live.plugged).insert(ask.at.clone(), kept);
                    }
                    return Err(refused);
                }
            }
        }
        lock(&live.plugged).insert(ask.at.clone(), given);
        Ok(())
    }

    /// The host directory a plug gives (D271): the one named, absolute; or the data of the
    /// home's volume named, made where it is not there, as a run's `-v NAME:PATH` makes one,
    /// and refused where its store refuses a run's own mount of it at `at` (D109's scoped
    /// volumes, another microVM's engine's). With a bounded volume's, its bound and its count
    /// file (D281): the bound its plug asks, which the volume is made with or, where no run
    /// holds it, set to; else its own. Refused where its holders serve it with another.
    fn source_dir(
        live: &Live<'_>,
        from: &wire::PlugFrom,
        at: &str,
    ) -> Result<(PathBuf, Option<(u64, PathBuf)>), (wire::PlugRefusal, String)> {
        use wire::PlugRefusal as R;
        match from {
            wire::PlugFrom::Host(host) => {
                if !Path::new(host).is_absolute() {
                    return Err((R::Request, format!("{host}: not an absolute path")));
                }
                Ok((PathBuf::from(host), None))
            }
            wire::PlugFrom::Volume { name, bound } => {
                let store = crate::volumes::Store::new(live.home);
                crate::volumes::Store::valid(name).map_err(|e| (R::Volume, e))?;
                // Held before it is made or found, so that no clearing removes it under the
                // run, and kept with what the run mounts for its life once taken (D281);
                // refused at once while a clearing holds it, never waited for.
                let mut hold = crate::volumes::Holds::default();
                hold.take(&store, name).map_err(|e| (R::Volume, e))?;
                let asked = bound.map(std::num::NonZeroU64::get);
                {
                    let _held = crate::volumes::lock();
                    // Made with the bound asked, as `--opt size` makes one (D281).
                    let options: std::collections::BTreeMap<String, String> = asked
                        .map(|bytes| ("size".to_string(), bytes.to_string()))
                        .into_iter()
                        .collect();
                    let (volume, _) = store
                        .create(name, &std::collections::BTreeMap::new(), &options, "")
                        .map_err(|e| (R::Volume, e))?;
                    crate::volumes::scoped_to(&volume, at, live.agentfile.map(|a| a.volumes.as_slice()))
                        .map_err(|e| (R::Volume, e))?;
                }
                // Counted with the run, its first holder walking it (D281).
                let data = store.data(name);
                let bounded = hold
                    .count(&store, name, &data, asked)
                    .map_err(|e| (R::Volume, e))?;
                lock(&live.volumes_held).extend(hold);
                Ok((data, bounded))
            }
        }
    }

    /// Host directory `host` plugged into member `member` at `at` (D235): its own share
    /// process, its own device on the microVM's bus, mounted in the member by init, `noexec`
    /// where its files may not be run (`exec` false, D279).
    fn plug_one(
        live: &Live<'_>,
        member: u32,
        host: &Path,
        bound: Option<&(u64, PathBuf)>,
        at: &str,
        read_only: bool,
        exec: bool,
    ) -> Result<Plugged, (wire::PlugRefusal, String)> {
        use wire::PlugRefusal as R;
        let dir = shards_vmm::platform::open_dir(host)
            .map_err(|e| (R::Host, format!("{}: {e}", host.display())))?;
        if !dir
            .metadata()
            .map_err(|e| (R::Host, format!("{}: {e}", host.display())))?
            .is_dir()
        {
            return Err((R::Host, format!("{}: not a directory", host.display())));
        }
        let dirs = [(OwnedFd::from(dir), read_only, OsString::new())];
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = format!(
            "shards-plug{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let tags = [tag.clone()];
        // A bounded volume's share held to its bound (D281), its only share.
        let bounds: Vec<crate::volumes::Bounded> = bound
            .map(|(limit, count)| crate::volumes::Bounded {
                share: 0,
                limit: *limit,
                count: count.clone(),
            })
            .into_iter()
            .collect();
        #[cfg(unix)]
        let mut share = start_shares(live.home, &dirs, &bounds, Some(&tags)).map_err(|e| (R::Host, e))?;
        #[cfg(windows)]
        let mut share =
            start_shares(live.home, &dirs, &bounds, live.job, Some(&tags)).map_err(|e| (R::Host, e))?;
        let step = [&[shards_ipc::device::SHARE][..], tag.as_bytes()].concat();
        // On Unix its connection, which the share process sends and holds until it is
        // taken (M24); on Windows its stream itself, a duplex pipe made here (D197).
        let refused = |e: String| (R::Host, e);
        #[cfg(unix)]
        let answer = match shards_ipc::recv(&share.door) {
            Ok(Some(ends)) if ends.kind == kind::SHARE_ENDS => match ends.fds.first() {
                Some(conn) => {
                    let answer = ask_device(live, &step, &[conn.as_fd()]).map_err(device_refused);
                    let _ = shards_ipc::send(&share.door, kind::TAKEN, &[], &[]);
                    answer
                }
                None => Err(refused("its share process sent no connection".into())),
            },
            Ok(_) => Err(refused("its share process sent no connection".into())),
            Err(e) => Err(refused(format!("its share process: {e}"))),
        };
        #[cfg(windows)]
        let answer = match share.ends.first() {
            Some(end) => ask_device(live, &step, &[end.as_fd()]).map_err(device_refused),
            None => Err(refused("no stream was made for its share".into())),
        };
        let placed = answer.and_then(|a| {
            a.split_once(' ')
                .and_then(|(d, addr)| Some((d.parse::<u8>().ok()?, addr.to_string())))
                .ok_or_else(|| (R::Device, format!("its microVM answered {a:?}")))
        });
        let (devfn, address) = match placed {
            Ok(placed) => placed,
            Err(e) => {
                share.end(&live.ends);
                return Err(e);
            }
        };
        let flag = if read_only { "ro" } else { "rw" };
        let index = member.to_string();
        let mut asked: Vec<&str> = vec!["share", &address, &tag, at, flag, "member", &index];
        if !exec {
            asked.push("noexec");
        }
        if let Err((_, e)) = guest_device(live, &asked) {
            // Taken off again, its guest having found it or not.
            let _ = guest_device(live, &["unmount", &address, at, "member", &index]);
            let _ = ask_device(live, &[shards_ipc::device::UNPLUG, devfn], &[]);
            share.end(&live.ends);
            return Err((R::Device, e));
        }
        // Its guest told what its host changes beneath it from now on (D235).
        let told = share.notices.take().and_then(|pipe| {
            notice_exec(live, pipe)
                .map_err(|e| super::say(format_args!("{}: its guest is told none of its changes: {e}", at)))
                .ok()
        });
        Ok(Plugged {
            devfn,
            address,
            member,
            host: std::fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf()),
            share,
            _told: told,
        })
    }

    /// What is plugged in at `ask`'s path let go of in each member it was plugged into (D235,
    /// D271): unmounted there by init, refused while anything there uses it, then taken off the
    /// microVM's bus; each member's share process given back, to be reaped once its stream has
    /// closed. Those a member refused stay plugged, the refusal naming them.
    fn unplug(
        live: &Live<'_>,
        ask: &wire::UnplugAsk,
    ) -> (Result<(), (wire::PlugRefusal, String)>, Vec<Shares>) {
        use wire::PlugRefusal as R;
        let _turn = lock(&live.plug_turn);
        let Some(plugged) = lock(&live.plugged).remove(&ask.at) else {
            return (
                Err((R::Unknown, format!("nothing is plugged in at {}", ask.at))),
                Vec::new(),
            );
        };
        let (kept, refused, gone) = unplug_each(live, plugged, &ask.at);
        let result = match refused {
            None => Ok(()),
            Some((cause, why)) => {
                let names: Vec<&str> = kept
                    .iter()
                    .filter_map(|p| live.members.iter().find(|m| m.index == p.member))
                    .map(|m| m.name.as_str())
                    .collect();
                Err((
                    cause,
                    format!("{}: still plugged into {}: {why}", ask.at, names.join(", ")),
                ))
            }
        };
        if !kept.is_empty() {
            lock(&live.plugged).insert(ask.at.clone(), kept);
        }
        (result, gone)
    }

    /// A plug's refusal, by what refused it (D235).
    type PlugRefused = (wire::PlugRefusal, String);

    /// Each of `plugged` at `at` let go of (D271): those a member refused, kept, and the first
    /// refusal; and the share processes of those let go of.
    fn unplug_each(
        live: &Live<'_>,
        plugged: Vec<Plugged>,
        at: &str,
    ) -> (Vec<Plugged>, Option<PlugRefused>, Vec<Shares>) {
        use wire::PlugRefusal as R;
        let (mut kept, mut gone, mut refused) = (Vec::new(), Vec::new(), None);
        for p in plugged {
            let index = p.member.to_string();
            let let_go = guest_device(live, &["unmount", &p.address, at, "member", &index])
                .map_err(|(busy, e)| (if busy { R::Busy } else { R::Device }, e))
                .and_then(|_| {
                    ask_device(live, &[shards_ipc::device::UNPLUG, p.devfn], &[]).map_err(device_refused)
                });
            match let_go {
                Ok(_) => gone.push(p.share),
                Err(e) => {
                    refused.get_or_insert(e);
                    kept.push(p);
                }
            }
        }
        (kept, refused, gone)
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

    /// A device step's refusal as a plug's (D235): the host's failure its own, the rest
    /// the microVM's, which could not take it.
    fn device_refused(refusal: Refusal) -> (wire::PlugRefusal, String) {
        let cause = match refusal {
            Refusal::Host(_) => wire::PlugRefusal::Host,
            _ => wire::PlugRefusal::Device,
        };
        (cause, format!("its microVM: {refusal}"))
    }

    /// Asks the VM device step `step` (`DEVICE`), with `fds`, and waits for its answer
    /// (`DEVICED`), as the daemon asks one (daemon/devices.rs `ask_device`).
    fn ask_device(live: &Live<'_>, step: &[u8], fds: &[BorrowedFd<'_>]) -> Result<String, Refusal> {
        ask_vm(live, &live.devices, kind::DEVICE, step, fds)
    }

    /// What each of the VM's vCPUs has run of its guest (`GUEST_TIME`, answered
    /// `GUEST_TIMED`), as the daemon asks it (daemon/commands.rs `guest_times_of`).
    fn guest_times(live: &Live<'_>) -> Result<wire::GuestTimes, String> {
        let said = ask_vm(live, &live.guest_times, kind::GUEST_TIME, &[], &[])?;
        let times = shards_vmm::vm::GuestTimes::decode(&said)
            .ok_or_else(|| format!("its microVM's guest time is none: {said:?}"))?;
        Ok(wire::GuestTimes {
            departed_ns: times.departed_ns,
            vcpus: times
                .vcpus
                .into_iter()
                .map(|v| wire::VcpuTime {
                    index: v.index,
                    guest_ns: v.guest_ns,
                    unread: v.unread,
                })
                .collect(),
        })
    }

    /// Asks the VM `which` with `step` after its number, and `fds`, and waits for its answer
    /// by that number (`answered`), which comes as long as its link is open.
    fn ask_vm<E: VmRefusal>(
        live: &Live<'_>,
        asks: &Mutex<Option<Vec<Asked<E>>>>,
        which: u8,
        step: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> Result<String, E> {
        let number = live.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = std::sync::mpsc::channel();
        match lock(asks).as_mut() {
            Some(asked) => asked.push((number, told)),
            None => return Err(E::stopped()),
        }
        let ask = [&number.to_be_bytes()[..], step].concat();
        if let Err(e) = shards_ipc::send(&lock(&live.link), which, &ask, fds) {
            if let Some(asked) = lock(asks).as_mut() {
                asked.retain(|(n, _)| *n != number);
            }
            return Err(E::unsent(&e));
        }
        answer
            .recv()
            .unwrap_or_else(|_| Err(E::stopped()))
            .map_err(E::said)
    }

    /// The VM's answer to the ask its payload numbers (`DEVICED`, `GUEST_TIMED`): its number,
    /// then 0 and what it said, or anything else and why not; to its asker in `asks`.
    fn answered<E: VmRefusal>(asks: &Mutex<Option<Vec<Asked<E>>>>, payload: &[u8]) {
        let Some((number, said)) = payload.split_first_chunk::<8>() else {
            return;
        };
        let number = u64::from_be_bytes(*number);
        let mut asks = lock(asks);
        let Some(asked) = asks.as_mut() else {
            return;
        };
        if let Some(i) = asked.iter().position(|(n, _)| *n == number) {
            let (_, told) = asked.swap_remove(i);
            let _ = told.send(E::read(said));
        }
    }

    #[cfg(all(test, unix))]
    #[allow(clippy::unwrap_used)]
    mod tests {
        /// A descriptor of this process that is a socket bound at `at`, by `/dev/fd` (each open
        /// descriptor, Linux's and macOS's) and getsockname(2): one the run host, or the network
        /// process this test stands in for, still holds.
        fn bound_here(at: std::net::SocketAddr) -> Option<i32> {
            let fds: Vec<i32> = std::fs::read_dir("/dev/fd")
                .unwrap()
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect();
            fds.into_iter().find(|&fd| local_address(fd) == Some(at))
        }

        /// The local address of socket `fd`, or none where it is no IP socket, or no longer open.
        fn local_address(fd: i32) -> Option<std::net::SocketAddr> {
            // SAFETY: sockaddr_storage is plain data, valid all zero.
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            // SAFETY: getsockname(2) into storage of the length given.
            if unsafe { libc::getsockname(fd, (&raw mut storage).cast(), &mut len) } != 0 {
                return None;
            }
            match i32::from(storage.ss_family) {
                libc::AF_INET => {
                    // SAFETY: an AF_INET address, as its family says, read from storage that holds it.
                    let a: libc::sockaddr_in = unsafe { std::ptr::read((&raw const storage).cast()) };
                    Some(std::net::SocketAddr::from((
                        std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)),
                        u16::from_be(a.sin_port),
                    )))
                }
                libc::AF_INET6 => {
                    // SAFETY: an AF_INET6 address, as its family says, read from storage that holds it.
                    let a: libc::sockaddr_in6 = unsafe { std::ptr::read((&raw const storage).cast()) };
                    Some(std::net::SocketAddr::from((
                        std::net::Ipv6Addr::from(a.sin6_addr.s6_addr),
                        u16::from_be(a.sin6_port),
                    )))
                }
                _ => None,
            }
        }

        use super::*;

        /// The VM binary checked beside a run's making is the one its launch would have checked
        /// itself: the same path, or the same refusal (this build's carries no VM process,
        /// `SHARDS_HELPERS=skip`, unless `SHARDS_VM_BINARY` names one); checked at once where
        /// no thread was started for it. Its network process's is checked once its run is said
        /// to have a network, and its taking waits for that check, told or not, and never on it
        /// where untold.
        #[test]
        fn a_vm_binary_checked_beside_its_runs_making_is_its_launchs_own() {
            // Untold of a network, its thread ends with the VM's check; told, after both.
            assert_eq!(Checking::begin().taken(), (crate::helpers::vm(), false));
            let mut told = Checking::begin();
            told.networked();
            assert_eq!(told.taken(), (crate::helpers::vm(), true));
            let none = Checking {
                thread: None,
                network: None,
                networked: false,
            };
            assert_eq!(none.taken(), (crate::helpers::vm(), false));
        }

        /// Do: begin a writable layer's making beside a run's, for a boot and for a save, and
        /// take each for a launch of its way, then again; begin one for a restore of a template
        /// that holds no snapshot and take it; take one never begun. Expect: each made on its
        /// thread as its launch makes one: an ext4 file system in the home's `layers`, its file
        /// the length of a launch's own of that size, gone with its handle; the restore's none;
        /// each taken once; one never begun none, for its launch to make its own.
        #[test]
        fn a_layer_made_beside_a_runs_making_is_its_launchs_own() {
            let home = shards_testdir::TempDir::new("api-layer").unwrap();
            let holds = 64 << 20;
            let own = crate::daemon::LayerFile::made(home.path(), holds).unwrap();
            let own_len = std::fs::metadata(&own.args()[1]).unwrap().len();
            drop(own);
            let template = home.path().join("t");
            let save = super::super::Way::Save {
                fresh: super::super::fresh_of(&template),
                dir: template.clone(),
            };
            for way in [super::super::Way::Boot, save] {
                let mut layering = Layering::begin(home.path(), way.clone(), holds);
                let layer = layering.taken(&way).unwrap().unwrap().unwrap();
                assert_eq!(layer_said(Some(&layer)), "made");
                let path = PathBuf::from(&layer.args()[1]);
                assert_eq!(
                    path.parent(),
                    Some(home.path().join(shards_ipc::LAYERS).as_path())
                );
                assert_eq!(std::fs::metadata(&path).unwrap().len(), own_len);
                let mut magic = [0u8; 2];
                let file = std::fs::File::open(&path).unwrap();
                shards_vmm::platform::read_exact_at(&file, &mut magic, 1024 + 0x38).unwrap();
                assert_eq!(u16::from_le_bytes(magic), 0xEF53);
                assert!(layering.taken(&way).is_none());
                drop(layer);
                assert!(!path.exists());
            }
            let restore = super::super::Way::Restore { dir: template };
            let mut layering = Layering::begin(home.path(), restore.clone(), holds);
            assert!(layering.taken(&restore).unwrap().unwrap().is_none());
            assert!(Layering::Gone.taken(&super::super::Way::Boot).is_none());
        }

        /// Do: take a layer made beside a run's making for a boot for a launch that saves, then
        /// for one that boots. Expect: none each time: a launch of another way makes its own, and
        /// the one let go of is no later launch's.
        #[test]
        fn a_layer_made_for_another_way_is_no_launchs() {
            let mut layering = Layering::Making {
                way: super::super::Way::Boot,
                thread: std::thread::spawn(|| Ok(None)),
            };
            let template = PathBuf::from("t");
            let save = super::super::Way::Save {
                fresh: super::super::fresh_of(&template),
                dir: template,
            };
            assert!(layering.taken(&save).is_none());
            assert!(layering.taken(&super::super::Way::Boot).is_none());
        }

        /// A lane's ask, its answer waited for as long as it takes (`Duration::MAX`), as a caller
        /// with no bound of its own waits: what it answered, or why not.
        macro_rules! answered {
            ($ask:expr) => {
                $ask.and_then(|mut ask| ask.wait(std::time::Duration::MAX))
                    .map(|got| got.expect("Duration::MAX waits until the answer"))
            };
        }

        /// A run's live state as its host holds it, of no members, its VM's link `link` and its
        /// managed output `capture`, kept in `home`.
        fn live<'a>(
            answers: &'a super::super::Answers,
            home: &'a Path,
            link: Stream,
            capture: Option<Stream>,
        ) -> Live<'a> {
            Live {
                answers,
                ends: Ends::kept_in(home),
                flows: Mutex::new(Flows::default()),
                flows_moved: std::sync::Condvar::new(),
                feeds: Mutex::new(HashMap::new()),
                fed: std::sync::Condvar::new(),
                generations: Mutex::new(Generations::default()),
                generations_moved: std::sync::Condvar::new(),
                capture: capture.map(Mutex::new),
                link: Mutex::new(link),
                held: Mutex::new(Some(HashMap::new())),
                asks_end: Mutex::new(None),
                next_exec: std::sync::atomic::AtomicU64::new(0),
                home,
                members: &[],
                roots: &[],
                devices: Mutex::new(Some(Vec::new())),
                guest_times: Mutex::new(Some(Vec::new())),
                rates: Mutex::new(Some(Vec::new())),
                lanes: Mutex::new(Some(HashMap::new())),
                next_lane: std::sync::atomic::AtomicU64::new(0),
                plugged: Mutex::new(HashMap::new()),
                plug_turn: Mutex::new(()),
                volumes_held: Mutex::default(),
                notices_due: Mutex::new(None),
                notices: Mutex::new(Vec::new()),
                leasing: None,
                saving: Mutex::new(super::super::Saving::Idle),
                pauses: Mutex::new(super::super::Pauses::new()),
                pause_asks: Mutex::new(Some(Vec::new())),
                reached: &[],
                agent: None,
                stdio: std::sync::OnceLock::new(),
                net: None,
                ports: Mutex::new(Ports::default()),
                egress: Mutex::new(wire::Narrowing::default()),
                given: Mutex::new(HashMap::new()),
                ceilings: Vec::new(),
                components: Mutex::new(Components::default()),
                component_image: &|_, _| Err((wire::SwapRefusal::Artifact, "no store in this test".into())),
                ceiled: false,
                agentfile: None,
                command: false,
            }
        }

        /// Do, on a run with a command of its own and a stand-in network process (D269):
        /// publish a port on 127.0.0.1 at one the host picks; connect to it; let it go; let it
        /// go again; publish one on a run with no command of its own and an image that gives no
        /// member it, and one on a run with no network. Expect: the network process told the
        /// port under a number of its own with its socket (`NET_PUBLISHED`), unanswered; its
        /// owner answered with it and every port published; a connection taken while it is
        /// published; the port let go by its number (`NET_UNPUBLISHED`), once that process
        /// answered, then free to bind; and each refusal typed: `Unknown`, `Widening`,
        /// `NoNetwork`, nothing published by any.
        #[test]
        fn a_port_published_live_is_told_by_its_number_and_let_go_by_it() {
            let dir = shards_testdir::TempDir::new("api-net").unwrap();
            let (mut orchestrator, out) = std::io::pipe().unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(out)));
            let (link, _vm) = Stream::pair().unwrap();
            let (netproc, run_host) = std::os::unix::net::UnixStream::pair().unwrap();
            netproc
                .set_read_timeout(Some(std::time::Duration::from_secs(120)))
                .unwrap();
            let control = super::super::NetControl::new(&run_host);
            let live = Live {
                net: Some(&control),
                command: true,
                ..live(&answers, dir.path(), link, None)
            };
            let mut answered = |id| {
                let (which, payload) = wire::read_frame(&mut orchestrator).unwrap().unwrap();
                assert_eq!(which, wire::to_client::NET_CHANGED);
                let answer = wire::NetAnswer::decode(&payload).unwrap();
                assert_eq!(answer.id, id);
                answer.result
            };
            let asked = wire::Publish {
                guest_port: 8080,
                proto: wire::Proto::Tcp,
                host: Some(std::net::Ipv4Addr::LOCALHOST.into()),
                host_port: 0,
            };
            net_change(
                &live,
                wire::NetAsk {
                    id: 1,
                    change: wire::NetChange::Publish(asked),
                },
            );
            let told = shards_ipc::recv(&netproc).unwrap().unwrap();
            assert_eq!(told.kind, kind::NET_PUBLISHED);
            assert_eq!(told.payload, [0, 0, 0, 0, 0x1f, 0x90, 6]);
            assert_eq!(told.fds.len(), 1);
            let changed = answered(1).unwrap();
            let [bound] = changed.changed.as_slice() else {
                panic!("{changed:?}");
            };
            assert_eq!((bound.guest_port, bound.proto), (8080, wire::Proto::Tcp));
            assert!(
                bound.host.ip().is_loopback() && bound.host.port() != 0,
                "{bound:?}"
            );
            assert!(
                bound.to.is_empty(),
                "a port no member takes reaches the run's command"
            );
            assert_eq!(changed.published, changed.changed);
            std::net::TcpStream::connect(bound.host).unwrap();
            std::thread::scope(|s| {
                let reading = s.spawn(|| super::super::relay(&control, None, &answers));
                let letting_go = s.spawn(|| {
                    net_change(
                        &live,
                        wire::NetAsk {
                            id: 2,
                            change: wire::NetChange::Unpublish {
                                proto: bound.proto,
                                host: bound.host,
                            },
                        },
                    )
                });
                let asked = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(
                    (asked.kind, asked.payload.as_slice()),
                    (kind::NET_UNPUBLISHED, &[0, 0, 0, 0][..])
                );
                // Its copy closed before it answers, as the network process closes its own.
                drop(told);
                shards_ipc::send(&netproc, kind::NET_UNPUBLISHED, &[], &[]).unwrap();
                letting_go.join().unwrap();
                let gone = answered(2).unwrap();
                assert_eq!(gone.changed, vec![bound.clone()]);
                assert!(gone.published.is_empty());
                // Let go of here, by every descriptor of this process: a bind of its port again
                // would race the tests beside it, whose children hold a copy of every descriptor
                // of this process, the listener's among them, from their fork to their exec
                // (CLOEXEC's), 11 to 14 us past this point in 1 of 4 parallel runs (Linux
                // container, 2026-11-16: no process held it at the failed bind but two forked
                // `api_run::hosted` children still running this binary).
                assert_eq!(bound_here(bound.host), None, "the port let go is held here");
                net_change(
                    &live,
                    wire::NetAsk {
                        id: 3,
                        change: wire::NetChange::Unpublish {
                            proto: bound.proto,
                            host: bound.host,
                        },
                    },
                );
                assert!(matches!(answered(3), Err((wire::NetRefusal::Unknown, _))));
                drop(netproc);
                reading.join().unwrap();
            });
            let alone = Live {
                command: false,
                ..live
            };
            net_change(
                &alone,
                wire::NetAsk {
                    id: 4,
                    change: wire::NetChange::Publish(asked),
                },
            );
            assert!(matches!(answered(4), Err((wire::NetRefusal::Widening, _))));
            let unnetworked = Live { net: None, ..alone };
            net_change(
                &unnetworked,
                wire::NetAsk {
                    id: 5,
                    change: wire::NetChange::Publish(asked),
                },
            );
            assert!(matches!(answered(5), Err((wire::NetRefusal::NoNetwork, _))));
            assert!(lock(&unnetworked.ports).held.is_empty());
        }

        /// D273. Do, on a run whose image grants 443/tcp and a destination, its stand-in
        /// network process taking or refusing what it is told: keep 443/tcp alone; keep a port
        /// its image grants not; have its network process refuse; keep nothing on a run whose
        /// image grants nothing past its microVM, and something there; and change one with no
        /// network. Expect: `NET_GRANTS` told the policy kept, taken, and the run's grants
        /// written out whole; `Widening` for the port, nothing told; `Network` in its network
        /// process's words, the grants before kept; nothing told and nothing held of the run
        /// with no ceiling, its widening refused; `NoNetwork`.
        #[test]
        fn a_runs_grants_change_within_its_images_as_it_goes() {
            use wire::NetRefusal as R;
            let dir = shards_testdir::TempDir::new("api-egress").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, _vm) = Stream::pair().unwrap();
            let (netproc, run_host) = std::os::unix::net::UnixStream::pair().unwrap();
            netproc
                .set_read_timeout(Some(std::time::Duration::from_secs(120)))
                .unwrap();
            let control = super::super::NetControl::new(&run_host);
            let agentfile = crate::agentfile::Agentfile {
                grants: shards_agentfile::agentfile::Grants {
                    egress: vec![b"443/tcp".to_vec()],
                    destinations: vec![b"api.example.com:443".to_vec()],
                    ..Default::default()
                },
                ..Default::default()
            };
            let only = |items: &[&str]| wire::Keep::Only(items.iter().map(|i| i.to_string()).collect());
            let asked = |egress: &[&str]| wire::Narrowing {
                egress: only(egress),
                mcp: only(&[]),
                destinations: only(&[]),
                dns: false,
                members: None,
            };
            let live = Live {
                net: Some(&control),
                ceiled: true,
                agentfile: Some(&agentfile),
                ..live(&answers, dir.path(), link, None)
            };
            std::thread::scope(|s| {
                let reading = s.spawn(|| super::super::relay(&control, None, &answers));
                let changing = s.spawn(|| egress_live(&live, &asked(&["443/tcp"])));
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(told.kind, kind::NET_GRANTS);
                assert_eq!(
                    shards_net::decode_policy(&told.payload),
                    Some((
                        shards_net::Ports(vec![(shards_net::Proto::Tcp, 443, 443)]),
                        Vec::new(),
                        false
                    ))
                );
                shards_ipc::send(&netproc, kind::NET_GRANTS, &[0], &[]).unwrap();
                assert_eq!(changing.join().unwrap(), Ok(()));
                assert_eq!(*lock(&live.egress), asked(&["443/tcp"]));
                assert!(matches!(
                    egress_live(&live, &asked(&["22/tcp"])),
                    Err((R::Widening, _))
                ));
                let changing = s.spawn(|| egress_live(&live, &asked(&[])));
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!((told.kind, told.payload.len()), (kind::NET_GRANTS, 0));
                shards_ipc::send(&netproc, kind::NET_GRANTS, b"\x01past its ceiling", &[]).unwrap();
                match changing.join().unwrap() {
                    Err((R::Network, why)) => assert!(why.contains("past its ceiling"), "{why}"),
                    other => panic!("{other:?}"),
                }
                assert_eq!(*lock(&live.egress), asked(&["443/tcp"]), "the grants before kept");
                drop(netproc);
                reading.join().unwrap();
            });
            let bare = crate::agentfile::Agentfile::default();
            let unceiled = Live {
                ceiled: false,
                agentfile: Some(&bare),
                ..live
            };
            assert_eq!(egress_live(&unceiled, &asked(&[])), Ok(()));
            assert_eq!(*lock(&unceiled.egress), asked(&[]));
            assert!(matches!(
                egress_live(&unceiled, &asked(&["443/tcp"])),
                Err((R::Widening, _))
            ));
            let unnetworked = Live {
                net: None,
                ..unceiled
            };
            assert!(matches!(
                egress_live(&unnetworked, &asked(&[])),
                Err((R::NoNetwork, _))
            ));
        }

        /// D270 slice 4. Do, on a run whose member 1 (a harness, `/harness/codex`) runs, its
        /// stand-in VM and guest taking what they are asked: swap `/harness/codex` to digest D
        /// now; give it its image's back at its next start; restart it; swap a path no member
        /// runs at, and one to no sha256 digest. Expect: D's image plugged (the VM's `PMEM` and
        /// D's key, slice 5's, answered `DEVFN ADDRESS GPA LEN`) and mounted (init's `DEVICE pmem
        /// ADDRESS GPA KEY`), its member restarted with D laid at its path, the swap answering D
        /// wanted and held and the generation started; the restore answering D held and nothing
        /// wanted, nothing let go; the restart laying nothing, then D unmounted (`DEVICE unpmem
        /// ADDRESS KEY`) and its function unplugged; `Unknown` and `Artifact`, nothing asked.
        #[test]
        fn a_component_swapped_is_plugged_mounted_laid_at_its_members_start_and_let_go_of_unused() {
            use protocol::members::{Component, Start, StartAnswer};
            use wire::SwapRefusal as R;
            let dir = shards_testdir::TempDir::new("api-swap").unwrap();
            let image = dir.join("component.erofs");
            std::fs::write(&image, b"erofs").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            let members = [wire::MemberInfo {
                index: 1,
                kind: wire::MemberKind::Harness,
                name: "codex".into(),
                runs: true,
            }];
            let opened =
                |_: &str, _: &str| std::fs::File::open(&image).map_err(|e| (R::Artifact, e.to_string()));
            let live = Live {
                members: &members,
                component_image: &opened,
                ..live(&answers, dir.path(), link, None)
            };
            let hex = "ab".repeat(32);
            let digest = format!("sha256:{hex}");
            let key = <protocol::members::ComponentKey>::try_from(hex.as_bytes()).unwrap();
            let (address, gpa) = ("0000:00:05.0", "274877906944");
            let ask = |id, change, when| wire::SwapAsk {
                id,
                change,
                when,
                patience: std::time::Duration::from_secs(60),
            };
            let swap_to = |at: &str| wire::SwapChange::Swap {
                at: at.into(),
                digest: digest.clone(),
            };
            std::thread::scope(|scope| {
                let guest = scope.spawn(|| {
                    let device = |step: &[u8], reply: &str| {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::DEVICE);
                        let (number, asked) = m.payload.split_first_chunk::<8>().unwrap();
                        assert_eq!(asked, step);
                        let said = shards_ipc::device::encode(&Ok(reply.to_string())).unwrap();
                        answered(&live.devices, &[&number[..], &said].concat());
                    };
                    let builtin = |which: u8, argv: Vec<Vec<u8>>| {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::EXEC_RUN);
                        let (_, rest) = m.payload.split_first_chunk::<8>().unwrap();
                        let spec = protocol::Spec::decode(&rest[1..]).unwrap();
                        assert_eq!((spec.builtin, spec.argv), (which, argv));
                        let conn = Stream::from(m.fds.into_iter().next().unwrap());
                        shards_ipc::send(&conn, kind::EXIT, &[0], &[]).unwrap();
                    };
                    let restarted = |generation: u32, components: Vec<Component>| {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::MEMBER_HOLD);
                        builtin(protocol::builtin::SIGNAL, protocol::members::signal_args(1, 15));
                        lock(&live.generations).ended.insert((1, generation - 1));
                        live.generations_moved.notify_all();
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::MEMBER_START);
                        let start = Start::decode(&m.payload[1..]).unwrap();
                        assert_eq!((start.generation, start.components), (generation, components));
                        let answer = StartAnswer {
                            member: 1,
                            generation,
                            refused: None,
                        };
                        started_told(&live, &answer.encode().unwrap());
                    };
                    device(
                        &[&[shards_ipc::device::PMEM][..], &key[..]].concat(),
                        &format!("5 {address} {gpa} 2097152"),
                    );
                    builtin(
                        protocol::builtin::DEVICE,
                        vec![b"pmem".to_vec(), address.into(), gpa.into(), key.to_vec()],
                    );
                    let laid = Component {
                        path: b"/harness/codex".to_vec(),
                        key,
                    };
                    restarted(1, vec![laid]);
                    // The restart past the restore: nothing laid, then D let go of.
                    restarted(2, Vec::new());
                    builtin(
                        protocol::builtin::DEVICE,
                        vec![b"unpmem".to_vec(), address.into(), key.to_vec()],
                    );
                    device(&[shards_ipc::device::UNPLUG, 5], "");
                });
                let held = |desired: Option<&str>, held: Option<&str>| wire::ComponentAt {
                    at: "/harness/codex".into(),
                    desired: desired.map(str::to_string),
                    held: held.map(str::to_string),
                };
                let swapped = swap(
                    &live,
                    &ask(1, swap_to("/harness/codex"), wire::SwapWhen::Now),
                    scope,
                )
                .unwrap();
                assert_eq!(swapped.components, vec![held(Some(&digest), Some(&digest))]);
                assert_eq!(swapped.started, Some((1, 1)));
                let restore = wire::SwapChange::Restore {
                    at: "/harness/codex".into(),
                };
                let restored = swap(&live, &ask(2, restore, wire::SwapWhen::NextStart), scope).unwrap();
                assert_eq!(restored.components, vec![held(None, Some(&digest))]);
                assert_eq!(restored.started, None);
                let again = wire::RestartAsk {
                    id: 3,
                    member: 1,
                    args: Vec::new(),
                    patience: std::time::Duration::from_secs(60),
                };
                assert_eq!(restart(&live, &again, scope), Ok(2));
                guest.join().unwrap();
                assert_eq!(components_of(&live), Vec::new());
                assert!(lock(&live.components).plugged.is_empty(), "let go of");
                assert!(matches!(
                    swap(
                        &live,
                        &ask(4, swap_to("/harness/nobody"), wire::SwapWhen::Now),
                        scope
                    ),
                    Err((R::Unknown, _))
                ));
                let bad = wire::SwapChange::Swap {
                    at: "/harness/codex".into(),
                    digest: "sha256:XYZ".into(),
                };
                assert!(matches!(
                    swap(&live, &ask(5, bad, wire::SwapWhen::Now), scope),
                    Err((R::Artifact, _))
                ));
            });
        }

        /// D278. Do, on a run whose image grants 443/tcp and whose member 1 (a harness) runs
        /// with a ceiling of 8000-9999/tcp, its stand-in network process and VM taking what they
        /// are told: give member 1 8443/tcp; give it its image's back; give it 9999/tcp, its guest
        /// refusing it as unheld (`EGRESS_UNHELD`); give it 22/tcp, past its ceiling; and give a
        /// member that does not run anything. Expect: widening, the network process told the
        /// union (443 and 8443) before its guest's `EGRESS` (member 1, 6/8443-8443); narrowing,
        /// its guest's `EGRESS` (member 1, `image`) before the network process told 443 alone;
        /// the refused grant told, then the guest refusing it, then the union before put back,
        /// `Unheld`, member 1's reach as it was; `Widening` for 22/tcp and `Unknown`, nothing
        /// told.
        #[test]
        fn a_members_own_reach_changes_in_its_guest_and_the_runs_union() {
            use wire::NetRefusal as R;
            let dir = shards_testdir::TempDir::new("api-member-egress").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            let (netproc, run_host) = std::os::unix::net::UnixStream::pair().unwrap();
            netproc
                .set_read_timeout(Some(std::time::Duration::from_secs(120)))
                .unwrap();
            let control = super::super::NetControl::new(&run_host);
            let agentfile = crate::agentfile::Agentfile {
                grants: shards_agentfile::agentfile::Grants {
                    egress: vec![b"443/tcp".to_vec()],
                    ..Default::default()
                },
                ..Default::default()
            };
            let members = [wire::MemberInfo {
                index: 1,
                kind: wire::MemberKind::Harness,
                name: "codex".into(),
                runs: true,
            }];
            let grants = |lo, hi| protocol::egress::Grants {
                ports: vec![(6, lo, hi)],
                named: Vec::new(),
                dns: false,
            };
            let live = Live {
                net: Some(&control),
                ceiled: true,
                agentfile: Some(&agentfile),
                members: &members,
                ceilings: vec![(1, grants(8000, 9999))],
                ..live(&answers, dir.path(), link, None)
            };
            let ports = |told: &shards_ipc::Message| {
                assert_eq!(told.kind, kind::NET_GRANTS);
                shards_net::decode_policy(&told.payload).map(|(p, _, _)| p.0)
            };
            let tcp = |lo, hi| (shards_net::Proto::Tcp, lo, hi);
            // Its VM: the guest's EGRESS taken, its exit as asked.
            let egress = |argv: Vec<Vec<u8>>, status: u8| {
                let m = shards_ipc::recv(&vm).unwrap().unwrap();
                assert_eq!(m.kind, kind::EXEC_RUN);
                let (_, rest) = m.payload.split_first_chunk::<8>().unwrap();
                let spec = protocol::Spec::decode(&rest[1..]).unwrap();
                assert_eq!((spec.builtin, spec.argv), (protocol::builtin::EGRESS, argv));
                let conn = Stream::from(m.fds.into_iter().next().unwrap());
                shards_ipc::send(&conn, kind::EXIT, &[status], &[]).unwrap();
            };
            let reach = |port: &str| wire::Reach {
                ports: vec![port.into()],
                ..wire::Reach::default()
            };
            let (wide, past) = (reach("8443/tcp"), reach("9999/tcp"));
            std::thread::scope(|s| {
                let reading = s.spawn(|| super::super::relay(&control, None, &answers));
                // Widening: the union first, then its guest.
                let changing = s.spawn(|| member_live(&live, 1, Some(&wide)));
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(ports(&told), Some(vec![tcp(443, 443), tcp(8443, 8443)]));
                shards_ipc::send(&netproc, kind::NET_GRANTS, &[0], &[]).unwrap();
                egress(protocol::egress::args(1, &grants(8443, 8443)), 0);
                assert_eq!(changing.join().unwrap(), Ok(()));
                assert_eq!(lock(&live.given).get(&1), Some(&wide));
                // Narrowing to its image's: its guest first, then the union.
                let changing = s.spawn(|| member_live(&live, 1, None));
                egress(protocol::egress::args_image(1), 0);
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(ports(&told), Some(vec![tcp(443, 443)]));
                shards_ipc::send(&netproc, kind::NET_GRANTS, &[0], &[]).unwrap();
                assert_eq!(changing.join().unwrap(), Ok(()));
                assert!(lock(&live.given).is_empty());
                // Refused by its guest: the union put back.
                let changing = s.spawn(|| member_live(&live, 1, Some(&past)));
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(ports(&told), Some(vec![tcp(443, 443), tcp(9999, 9999)]));
                shards_ipc::send(&netproc, kind::NET_GRANTS, &[0], &[]).unwrap();
                egress(
                    protocol::egress::args(1, &grants(9999, 9999)),
                    protocol::builtin::EGRESS_UNHELD,
                );
                let told = shards_ipc::recv(&netproc).unwrap().unwrap();
                assert_eq!(ports(&told), Some(vec![tcp(443, 443)]), "put back");
                shards_ipc::send(&netproc, kind::NET_GRANTS, &[0], &[]).unwrap();
                assert!(matches!(changing.join().unwrap(), Err((R::Unheld, _))));
                assert!(lock(&live.given).is_empty(), "its reach as it was");
                // Past its image's and its ceiling: refused here, nothing told.
                match member_live(&live, 1, Some(&reach("22/tcp"))) {
                    Err((R::Widening, why)) => assert!(why.contains("TCP ports 22-22"), "{why}"),
                    other => panic!("{other:?}"),
                }
                assert!(matches!(member_live(&live, 0, None), Err((R::Unknown, _))));
                drop(netproc);
                reading.join().unwrap();
            });
        }

        /// D271. Do: ask a plug's source of a host directory, of a relative one, of a volume of
        /// the run's home its store has not got, of that volume again, of a name no volume takes,
        /// and of a volume an image's Agentfile gives its members elsewhere; and grant a run plug
        /// roots beside its host's own, one of them its host's already. Expect: the host
        /// directory as named; a relative one refused (`Request`); the volume made, its data
        /// directory in the home, and the same one taken again; a bad name and a scoped
        /// volume refused (`Volume`); each root granted once, after its host's, and one of
        /// init's own refused. D281: the volume plugged held by the run while it lives, no
        /// clearing taking it alone, and let go of with it; the scoped one it was refused, not
        /// held.
        #[test]
        fn a_plugs_source_is_a_host_directory_or_its_homes_volume_beneath_granted_roots() {
            use wire::PlugRefusal as R;
            let dir = shards_testdir::TempDir::new("api-vol").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, _vm) = Stream::pair().unwrap();
            let live = live(&answers, dir.path(), link, None);
            let at = "/session/x";
            assert_eq!(
                source_dir(&live, &wire::PlugFrom::Host("/Users/u/proj".into()), at),
                Ok((PathBuf::from("/Users/u/proj"), None))
            );
            assert!(matches!(
                source_dir(&live, &wire::PlugFrom::Host("proj".into()), at),
                Err((R::Request, _))
            ));
            let store = crate::volumes::Store::new(dir.path());
            let volume = |name: &str, bound: Option<u64>| wire::PlugFrom::Volume {
                name: name.into(),
                bound: bound.and_then(std::num::NonZeroU64::new),
            };
            assert!(store.get("sessions").is_none());
            let made = source_dir(&live, &volume("sessions", None), at).unwrap();
            assert_eq!(made, (store.data("sessions"), None), "made, with no bound");
            assert!(store.get("sessions").is_some());
            assert_eq!(source_dir(&live, &volume("sessions", None), at), Ok(made));
            assert!(matches!(
                source_dir(&live, &volume("a/b", None), at),
                Err((R::Volume, _))
            ));
            // D281: one made with its bound is served with it and its count file; another
            // bound asked while it is held is refused.
            assert_eq!(
                source_dir(&live, &volume("bounded", Some(1 << 20)), at),
                Ok((
                    store.data("bounded"),
                    Some((1 << 20, dir.path().join("volumes").join(".bounded.charge")))
                ))
            );
            assert_eq!(store.get("bounded").unwrap().bound(), Ok(Some(1 << 20)));
            assert!(matches!(
                source_dir(&live, &volume("bounded", Some(2 << 20)), at),
                Err((R::Volume, _))
            ));
            let scoped: std::collections::BTreeMap<String, String> =
                [(crate::volumes::SCOPED.to_string(), "1".to_string())]
                    .into_iter()
                    .collect();
            {
                let _held = crate::volumes::lock();
                store
                    .create("scoped", &scoped, &std::collections::BTreeMap::new(), "")
                    .unwrap();
            }
            assert!(matches!(
                source_dir(&live, &volume("scoped", None), at),
                Err((R::Volume, _))
            ));
            assert!(store.alone("sessions").unwrap().is_none());
            assert!(store.alone("scoped").unwrap().is_some());
            drop(live);
            assert!(store.alone("sessions").unwrap().is_some());
            let own = vec![b"plug-root=/Users".to_vec()];
            assert_eq!(
                granted_roots(own.clone(), &["/session".into(), "/Users".into()]).unwrap(),
                vec![b"plug-root=/Users".to_vec(), b"plug-root=/session".to_vec()]
            );
            assert!(granted_roots(own, &["/tmp".into()]).is_err());
        }

        /// Do pause a run whose init last said its stats, its stand-in VM answering each `PAUSE`
        /// and init's `CLOCK`; ask its guest while it is paused, pause it again, read its stats,
        /// resume it twice, and pause it once its VM's link has closed (D267). Expect: its VM
        /// asked to hold at once (`PAUSE` 1, its number), nothing of its guest asked first; an
        /// ask of its guest refused while it is paused; the second pause and resume done already,
        /// unasked; its stats what its init last said; its paused time counting from its VM's word
        /// and ending at its resume's; its init asked to set its clock once it goes on; and a
        /// pause after its link closed refused.
        #[test]
        fn a_pause_holds_its_vm_at_once_and_its_resume_sets_its_guests_clock() {
            let dir = shards_testdir::TempDir::new("api-pause").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            vm.set_read_timeout(Some(std::time::Duration::from_secs(120)))
                .unwrap();
            let followed = link.try_clone().unwrap();
            let live = live(&answers, dir.path(), link, None);
            lock(&live.pauses).last = Some(b"cpu 5\nmemory 9\n".to_vec());
            std::thread::scope(|scope| {
                let (watched, followed) = (&live, &followed);
                let following = scope.spawn(move || follow(watched, followed, scope));
                let playing = scope.spawn(move || {
                    // Held, then let go: each `PAUSE` its whether, then its number.
                    for frozen in [1u8, 0] {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::PAUSE, "its VM was asked something else first");
                        let (whether, number) = m.payload.split_first().unwrap();
                        assert_eq!(*whether, frozen);
                        shards_ipc::send(&vm, kind::PAUSED, number, &[]).unwrap();
                    }
                    // Its init's clock set, once it goes on.
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::EXEC_RUN);
                    let spec = protocol::Spec::decode(&m.payload[9..]).unwrap();
                    assert_eq!(spec.builtin, protocol::builtin::CLOCK);
                    let conn = Stream::from(m.fds.into_iter().next().unwrap());
                    shards_ipc::send(&conn, kind::EXIT, &[0], &[]).unwrap();
                    drop(vm);
                });
                let held = pause(&live).unwrap();
                assert!(held.now);
                assert!(!lock(&live.pauses).guest_runs(), "its guest asked while paused");
                let again = pause(&live).unwrap();
                assert!(again.now && again.total >= held.total);
                assert_eq!(
                    lock(&live.pauses).said(),
                    Some(Some(&b"cpu 5\nmemory 9\n"[..])),
                    "its stats while paused are not what its init last said"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
                let resumed = resume(&live, scope).unwrap();
                assert!(!resumed.now);
                assert!(resumed.total >= std::time::Duration::from_millis(20));
                assert_eq!(
                    resume(&live, scope),
                    Ok(resumed),
                    "a resume of a running run is done already"
                );
                assert!(lock(&live.pauses).guest_runs());
                playing.join().unwrap();
                following.join().unwrap();
                assert_eq!(pause(&live), Err("its microVM has stopped".to_string()));
                assert_eq!(lock(&live.pauses).paused(Instant::now()).total, resumed.total);
            });
        }

        /// Do ask a paused run's guest for a room, a resize, a member's stats, a plug and an
        /// unplug (D267). Expect: each refused at once, `ASK_PAUSED` by its id, none of them
        /// sent to its VM; and its pause's own ask answered the run's paused time.
        #[test]
        fn asks_of_a_paused_runs_guest_are_refused_at_once() {
            let dir = shards_testdir::TempDir::new("api-paused").unwrap();
            let said = Said::default();
            let answers = super::super::Answers(Mutex::new(Box::new(said.clone())));
            let (link, vm) = Stream::pair().unwrap();
            let live = live(&answers, dir.path(), link, None);
            lock(&live.pauses).state = super::super::Pause::Paused {
                since: Instant::now(),
            };
            let args = [OsStr::new("-c"), OsStr::new("sleep 30")];
            let child = shards_ipc::spawn_in(Path::new("/bin/sh"), &args, &[], false, &[]).unwrap();
            let asks = [
                super::super::Ask::Room(wire::RoomAsk {
                    id: 1,
                    room: Some(1 << 20),
                }),
                super::super::Ask::Resize(wire::ResizeAsk {
                    id: 2,
                    member: 0,
                    rows: 24,
                    cols: 80,
                }),
                super::super::Ask::Stats(wire::StatsAsk {
                    id: 3,
                    scope: wire::StatsScope::Member(0),
                }),
                super::super::Ask::Plug(wire::PlugAsk {
                    id: 4,
                    members: vec![0],
                    from: wire::PlugFrom::Host("/tmp".into()),
                    at: "/work".into(),
                    read_only: true,
                    exec: false,
                }),
                super::super::Ask::Unplug(wire::UnplugAsk {
                    id: 5,
                    at: "/work".into(),
                }),
                super::super::Ask::Pause(wire::PauseAsk { id: 6, pause: true }),
            ];
            std::thread::scope(|scope| {
                for ask in asks {
                    assert!(take(&live, ask, &child, scope).is_ok());
                }
                // Nothing of its guest's asks reached its VM; its link then closed, so that one
                // that did is answered (its VM gone) rather than waiting on it for ever.
                let reached = shards_ipc::recv_by(&vm, Instant::now());
                drop(vm);
                assert!(
                    matches!(&reached, Err(e) if e.kind() == io::ErrorKind::TimedOut),
                    "an ask of its paused guest reached its VM: {reached:?}"
                );
            });
            let _ = child.kill(libc::SIGKILL);
            let _ = child.wait();
            let all = lock(&said.0).clone();
            let mut from = &all[..];
            let mut refused = Vec::new();
            let mut paused = None;
            while let Some((kind, payload)) = wire::read_frame(&mut from).unwrap() {
                match kind {
                    wire::to_client::ASK_PAUSED => {
                        refused.push(wire::AskPaused::decode(&payload).unwrap().id)
                    }
                    wire::to_client::PAUSED => paused = Some(wire::PauseAnswer::decode(&payload).unwrap()),
                    other => panic!("answered kind {other}"),
                }
            }
            assert_eq!(refused, [1, 2, 3, 4, 5]);
            let paused = paused.unwrap();
            assert_eq!(paused.id, 6);
            assert!(paused.result.unwrap().now);
        }

        /// Do give room to a run whose VM takes no more of its managed output, its end of it
        /// closed as at its end, as an orchestrator's first credit can come after a run that
        /// ends at once (WHP at 3a05bc3f1: a template's run of `exit 3`). Expect: the ask
        /// served, the asks going on, and no end of the asks' making, which would kill the VM
        /// before its `DONE` and end the run failed in the credit's words.
        #[test]
        fn room_its_vm_takes_no_more_of_leaves_its_end_to_its_vm() {
            let dir = shards_testdir::TempDir::new("api-credit").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (capture, its_vms) = Stream::pair().unwrap();
            drop(its_vms);
            let (link, _vm_link) = Stream::pair().unwrap();
            let live = live(&answers, dir.path(), link, Some(capture));
            let args = [OsStr::new("-c"), OsStr::new("exit 0")];
            let vm = shards_ipc::spawn_in(Path::new("/bin/sh"), &args, &[], false, &[]).unwrap();
            let credit = wire::Credit {
                member: 0,
                generation: 0,
                channel: Channel::Stdout,
                bytes: 4096,
            };
            let taken =
                std::thread::scope(|scope| take(&live, super::super::Ask::Credit(credit), &vm, scope));
            assert_eq!(taken, Ok(()));
            assert_eq!(*lock(&live.asks_end), None);
            assert!(lock(&live.flows).ungranted);
            let _ = vm.wait();
        }

        /// Do have a run's VM bring its managed output a frame its host cannot take, here one
        /// of no record's kind; and end that output at a frame's start, as a VM does as its
        /// run ends. Expect: the first to end the run's asks failed in its words, its VM killed
        /// for it, so that no run waits for its `STOP` on a failure its host saw (D258); the
        /// second to end nothing of the run, whose end is its VM's to say.
        #[test]
        fn members_output_its_host_cannot_take_ends_its_run_failed_saying_why() {
            for refused in [true, false] {
                let dir = shards_testdir::TempDir::new("api-records").unwrap();
                let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
                let (capture, its_vms) = Stream::pair().unwrap();
                let (link, _vm_link) = Stream::pair().unwrap();
                let live = live(&answers, dir.path(), link, None);
                let args = [OsStr::new("-c"), OsStr::new("sleep 30")];
                let vm = shards_ipc::spawn_in(Path::new("/bin/sh"), &args, &[], false, &[]).unwrap();
                if refused {
                    (&its_vms)
                        .write_all(&protocol::header(protocol::kind::STDOUT, 0))
                        .unwrap();
                } else {
                    drop(its_vms);
                }
                records(&live, capture, &vm);
                if refused {
                    assert_eq!(
                        *lock(&live.asks_end),
                        Some(super::super::AsksEnd::Failed(format!(
                            "its VM's members' output broke: a frame of kind {}, no record's",
                            protocol::kind::STDOUT
                        )))
                    );
                    assert_eq!(vm.wait().unwrap(), 128 + libc::SIGKILL);
                } else {
                    assert_eq!(*lock(&live.asks_end), None);
                    assert_eq!(vm.try_wait(), None, "its VM, killed for an end of its own");
                    let _ = vm.kill(libc::SIGKILL);
                    let _ = vm.wait();
                }
            }
        }

        /// What an `Answers` wrote, read back by the test.
        #[derive(Clone, Default)]
        struct Said(std::sync::Arc<Mutex<Vec<u8>>>);

        impl io::Write for Said {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                lock(&self.0).extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        /// A member's bytes its host held past its orchestrator's room reach the orchestrator,
        /// and the channel's end says it was cut, as its VM's capture ends without the
        /// channel's end (control-01's follow-up, docs/bugs/2026-11-13/control.md). Do: give
        /// member 1's stdout 5 bytes of room and the other channel the orchestrator pumps,
        /// its stderr, room too; have the VM bring a record of 10 bytes of stdout, then end
        /// its capture. Expect: the 5 the room took, then the 5 held, then each channel's end,
        /// `Aborted` with ECONNRESET's Linux number, and nothing more. Before, the 5 held and
        /// both ends were dropped: the orchestrator's pipes waited for its host's exit, and
        /// ended short.
        #[test]
        fn bytes_held_past_the_room_reach_the_orchestrator_as_the_capture_is_cut() {
            let dir = shards_testdir::TempDir::new("api-cut").unwrap();
            let said = Said::default();
            let answers = super::super::Answers(Mutex::new(Box::new(said.clone())));
            let (capture, its_vms) = Stream::pair().unwrap();
            let (link, _vm_link) = Stream::pair().unwrap();
            let live = live(&answers, dir.path(), link, None);
            {
                let mut flows = lock(&live.flows);
                flows.room.insert((1, 0, Channel::Stdout), 5);
                flows.room.insert((1, 0, Channel::Stderr), 4096);
            }
            let record = managed::Record {
                member: 1,
                channel: managed::Channel::Stdout,
                offset: 0,
                event: managed::Event::Data(b"0123456789"),
            };
            let mut bytes = vec![0u8; record.encoded_len().unwrap()];
            record.encode(&mut bytes).unwrap();
            (&its_vms)
                .write_all(&protocol::header(protocol::kind::MANAGED, bytes.len() as u32))
                .unwrap();
            (&its_vms).write_all(&bytes).unwrap();
            drop(its_vms);
            let args = [OsStr::new("-c"), OsStr::new("sleep 30")];
            let vm = shards_ipc::spawn_in(Path::new("/bin/sh"), &args, &[], false, &[]).unwrap();
            records(&live, capture, &vm);
            assert_eq!(*lock(&live.asks_end), None);
            let _ = vm.kill(libc::SIGKILL);
            let _ = vm.wait();
            let all = lock(&said.0).clone();
            let mut from = &all[..];
            let mut out = Vec::new();
            let mut ends = Vec::new();
            while let Some((kind, payload)) = wire::read_frame(&mut from).unwrap() {
                match kind {
                    wire::to_client::OUTPUT => {
                        let o = wire::Output::decode(&payload).unwrap();
                        assert!(
                            !ends.iter().any(|e: &wire::OutputEnd| e.channel == o.channel),
                            "output after its channel's end"
                        );
                        out.push((o.member, o.channel, o.bytes));
                    }
                    wire::to_client::OUTPUT_END => ends.push(wire::OutputEnd::decode(&payload).unwrap()),
                    other => panic!("an answer of kind {other}"),
                }
            }
            assert_eq!(
                out,
                [
                    (1, Channel::Stdout, b"01234".to_vec()),
                    (1, Channel::Stdout, b"56789".to_vec())
                ]
            );
            ends.sort_by_key(|e| e.channel as u8);
            let cut = |channel| wire::OutputEnd {
                member: 1,
                generation: 0,
                channel,
                end: wire::StreamEnd::Aborted(104),
            };
            assert_eq!(ends, [cut(Channel::Stdout), cut(Channel::Stderr)]);
        }

        /// A run's share process ends as its run ends, by its own going: [`Shares::end`]
        /// lets go of its door before it waits, and the process, which goes once its
        /// connection to the VM ends, has gone well before `reap`'s grace ends it (CI run
        /// 43230466376, its run host in `do_wait` with the door still held, 56 minutes).
        /// The process here is a shell that reads its door to its end and then says so in a
        /// file, which one killed at the grace's end never writes.
        #[test]
        fn a_share_process_goes_as_its_door_is_let_go_of() {
            let dir = shards_testdir::TempDir::new("share-end").unwrap();
            let said = dir.join("gone");
            let (door, theirs) = Stream::pair().unwrap();
            let script = OsStr::new("read -r _ <&3; : >\"$0\"");
            let args = [OsStr::new("-c"), script, said.as_os_str()];
            let child = shards_ipc::spawn_in(Path::new("/bin/sh"), &args, &[(theirs.as_fd(), 3)], false, &[])
                .unwrap();
            drop(theirs);
            Shares {
                child,
                door,
                notices: None,
            }
            .end(&Ends::kept_in(&dir));
            assert!(
                said.exists(),
                "the share process was ended at the grace's end, not by its door's"
            );
        }

        /// D270 slice 3b: a member started again. Its VM is asked to hold it, its command's
        /// SIGTERM goes as init's SIGNAL built-in, its generation's end comes, and its next is
        /// asked of its VM (`MEMBER_START`: no input asked, as its run asked no stdin of it; its
        /// arguments as given), which its guest's answer starts: generation 1, its member's from
        /// then on. A second, whose graceful end outlasts its patience, is killed (`MEMBER_KILL`)
        /// before its next starts: generation 2. A third its guest refuses is that refusal, its
        /// member at 2 still. A member that runs in no run's, or one whose restart is under way,
        /// is refused typed, asking its VM nothing.
        #[test]
        fn a_member_starts_again_as_its_generation_ends() {
            use protocol::members::{Start, StartAnswer, StartRefusal};
            let dir = shards_testdir::TempDir::new("api-restart").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            let members = [wire::MemberInfo {
                index: 1,
                kind: wire::MemberKind::Harness,
                name: "codex".into(),
                runs: true,
            }];
            let live = Live {
                members: &members,
                ..live(&answers, dir.path(), link, None)
            };
            let (long, short) = (
                std::time::Duration::from_secs(60),
                std::time::Duration::from_millis(50),
            );
            let ask = |id, args: &[&[u8]], patience| wire::RestartAsk {
                id,
                member: 1,
                args: args.iter().map(|a| a.to_vec()).collect(),
                patience,
            };
            std::thread::scope(|scope| {
                // Its VM and its guest: each built-in taken, its member's ends and answers told.
                let guest = scope.spawn(|| {
                    let builtin = |which: u8, argv: Vec<Vec<u8>>| {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::EXEC_RUN);
                        let (_, rest) = m.payload.split_first_chunk::<8>().unwrap();
                        let spec = protocol::Spec::decode(&rest[1..]).unwrap();
                        assert_eq!((spec.builtin, spec.argv), (which, argv));
                        // Untold of its start (no `EXEC_TOLD`): its exit alone.
                        let conn = Stream::from(m.fds.into_iter().next().unwrap());
                        shards_ipc::send(&conn, kind::EXIT, &[0], &[]).unwrap();
                    };
                    let hold = |held| {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        let hold = protocol::members::Hold { member: 1, held };
                        assert_eq!((m.kind, m.payload), (kind::MEMBER_HOLD, hold.encode().to_vec()));
                    };
                    let held = || hold(true);
                    let ended = |generation| {
                        lock(&live.generations).ended.insert((1, generation));
                        live.generations_moved.notify_all();
                    };
                    let started =
                        |generation, args: Vec<Vec<u8>>, refused: Option<(StartRefusal, Vec<u8>)>| {
                            let m = shards_ipc::recv(&vm).unwrap().unwrap();
                            assert_eq!(m.kind, kind::MEMBER_START);
                            let (flag, start) = m.payload.split_first().unwrap();
                            let start = Start::decode(start).unwrap();
                            assert_eq!(
                                (*flag, start.member, start.generation, start.args, start.input),
                                (0, 1, generation, args, None)
                            );
                            let answer = StartAnswer {
                                member: 1,
                                generation,
                                refused,
                            };
                            started_told(&live, &answer.encode().unwrap());
                        };
                    held();
                    builtin(protocol::builtin::SIGNAL, protocol::members::signal_args(1, 15));
                    ended(0);
                    started(1, vec![b"--resume".to_vec()], None);
                    held();
                    builtin(protocol::builtin::SIGNAL, protocol::members::signal_args(1, 15));
                    builtin(protocol::builtin::MEMBER_KILL, protocol::members::kill_args(1));
                    assert!(
                        lock(&live.generations).killed.contains(&(1, 1)),
                        "its kill is marked before it goes"
                    );
                    ended(1);
                    started(2, Vec::new(), None);
                    held();
                    builtin(protocol::builtin::SIGNAL, protocol::members::signal_args(1, 15));
                    ended(2);
                    started(
                        3,
                        Vec::new(),
                        Some((StartRefusal::Failed, b"its exec: ENOENT".to_vec())),
                    );
                    // Its restart failed: let go, so that the end it had under the hold ends its
                    // whole member.
                    hold(false);
                });
                assert_eq!(restart(&live, &ask(1, &[b"--resume"], long), scope), Ok(1));
                assert_eq!(live.generation_of(1), 1);
                assert_eq!(live.ended_as(1), (1, false));
                assert_eq!(restart(&live, &ask(2, &[], short), scope), Ok(2));
                assert_eq!(live.ended_as(1), (2, false), "its kill was its last generation's");
                assert_eq!(
                    restart(&live, &ask(3, &[], long), scope),
                    Err((wire::RestartRefusal::Failed, "its exec: ENOENT".into()))
                );
                assert_eq!(live.generation_of(1), 2);
                guest.join().unwrap();
                // Its generation ended, its restart let go: its member ended whole, and runs no
                // more.
                let refused = restart(&live, &ask(6, &[], long), scope).unwrap_err();
                assert_eq!(refused.0, wire::RestartRefusal::Unknown, "{refused:?}");
                assert!(refused.1.contains("runs no more"), "{refused:?}");
                let none = wire::RestartAsk {
                    id: 4,
                    member: 9,
                    args: Vec::new(),
                    patience: long,
                };
                assert!(matches!(
                    restart(&live, &none, scope),
                    Err((wire::RestartRefusal::Unknown, _))
                ));
                lock(&live.generations).restarting.insert(1);
                assert!(matches!(
                    restart(&live, &ask(5, &[], long), scope),
                    Err((wire::RestartRefusal::Running, _))
                ));
            });
        }

        /// D277 for a restart (D270 slice 3b): a next generation its guest refused as it could not
        /// enter its working directory is refused `Workdir`, naming the member as its image
        /// declares it, the directory and the errno, as a first generation's is; one told
        /// malformed is `Failed`; each other refusal is its own, in its guest's words.
        #[test]
        fn a_restart_that_could_not_enter_its_workdir_is_refused_typed() {
            use protocol::members::{StartRefusal as S, Unentered};
            use wire::RestartRefusal as R;
            let members = [wire::MemberInfo {
                index: 1,
                kind: wire::MemberKind::Harness,
                name: "codex".into(),
                runs: true,
            }];
            let told = Unentered {
                member: 1,
                errno: 2,
                workdir: b"/Users/ada/proj".to_vec(),
            };
            let (why, words) = start_refused(&members, S::Workdir, &told.encode());
            assert_eq!(why, R::Workdir);
            assert!(
                words.starts_with("Harness codex: /Users/ada/proj: errno 2"),
                "{words}"
            );
            assert_eq!(start_refused(&members, S::Workdir, b"errno 2").0, R::Failed);
            assert_eq!(
                start_refused(&members, S::Failed, b"its exec: ENOENT"),
                (R::Failed, "its exec: ENOENT".into())
            );
            assert_eq!(start_refused(&members, S::Running, b"under way").0, R::Running);
        }

        /// D270 slice 3b: a restart starts its member's next generation once every channel of its
        /// last that its guest was given a credit for has ended, and not before: no start is asked
        /// of its VM while either is open; once both ended it is, and its next starts. Then its last
        /// is sealed, no credit going to it whatever room its orchestrator gives it; a channel
        /// that never ends within the patience is said so. Taking `last_output_ended` out of the
        /// restart asks the start while its last's output is open.
        #[test]
        fn a_restart_starts_its_next_generation_once_its_lasts_output_has_ended() {
            use protocol::members::{Start, StartAnswer};
            use std::os::fd::AsRawFd as _;
            let dir = shards_testdir::TempDir::new("api-restart-out").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            let (capture, _guest_capture) = Stream::pair().unwrap();
            let members = [wire::MemberInfo {
                index: 1,
                kind: wire::MemberKind::Harness,
                name: "codex".into(),
                runs: true,
            }];
            let live = Live {
                members: &members,
                ..live(&answers, dir.path(), link, Some(capture))
            };
            // Its orchestrator's room on both of generation 0's channels: a credit to its guest for each.
            let room = |generation, channel| {
                let mut flows = lock(&live.flows);
                let key = (1, generation, channel);
                flows.room.insert(key, 4096);
                live.grant(&mut flows, key).unwrap();
                flows.granted.get(&key).copied().unwrap_or(false)
            };
            assert!(room(0, Channel::Stdout) && room(0, Channel::Stderr));
            let end = |channel| {
                let mut flows = lock(&live.flows);
                flows.granted.insert((1, 0, channel), false);
                live.end_channel(&mut flows, (1, 0, channel), wire::StreamEnd::Closed)
                    .unwrap();
            };
            // Whether its VM was asked anything within `ms`.
            let asked_within = |ms| {
                let mut fd = libc::pollfd {
                    fd: vm.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd of a descriptor this test holds open.
                unsafe { libc::poll(&mut fd, 1, ms) == 1 }
            };
            std::thread::scope(|scope| {
                let guest = scope.spawn(|| {
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::MEMBER_HOLD);
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::EXEC_RUN);
                    let conn = Stream::from(m.fds.into_iter().next().unwrap());
                    shards_ipc::send(&conn, kind::EXIT, &[0], &[]).unwrap();
                    lock(&live.generations).ended.insert((1, 0));
                    live.generations_moved.notify_all();
                    assert!(
                        !asked_within(200),
                        "its next asked while its last's output is open"
                    );
                    end(Channel::Stdout);
                    assert!(
                        !asked_within(100),
                        "its next asked while its last's stderr is open"
                    );
                    end(Channel::Stderr);
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::MEMBER_START);
                    let start = Start::decode(&m.payload[1..]).unwrap();
                    assert_eq!((start.member, start.generation), (1, 1));
                    let answer = StartAnswer {
                        member: 1,
                        generation: 1,
                        refused: None,
                    };
                    started_told(&live, &answer.encode().unwrap());
                });
                let ask = wire::RestartAsk {
                    id: 1,
                    member: 1,
                    args: Vec::new(),
                    patience: std::time::Duration::from_secs(60),
                };
                assert_eq!(restart(&live, &ask, scope), Ok(1));
                guest.join().unwrap();
            });
            assert!(lock(&live.flows).sealed.contains(&(1, 0)), "its last sealed");
            assert!(room(1, Channel::Stdout), "its next's channels are given credits");
            // A generation whose channel never ends: not sealed, and said so within its patience.
            let mut flows = lock(&live.flows);
            flows.granted.insert((1, 1, Channel::Stdout), true);
            drop(flows);
            assert!(!last_output_ended(
                &live,
                1,
                1,
                std::time::Duration::from_millis(50)
            ));
            assert!(!lock(&live.flows).sealed.contains(&(1, 1)));
            // One whose channels got no credit: nothing waits, and none goes to it after.
            assert!(last_output_ended(&live, 1, 7, std::time::Duration::ZERO));
            let mut flows = lock(&live.flows);
            flows.room.insert((1, 7, Channel::Stderr), 4096);
            lock(&live.generations).current.insert(1, 7);
            live.grant(&mut flows, (1, 7, Channel::Stderr)).unwrap();
            assert!(
                !flows.granted.contains_key(&(1, 7, Channel::Stderr)),
                "a sealed generation's credit went"
            );
        }

        /// D238, D252, D254: an orchestrator's run hosts its own lanes as the daemon hosts its
        /// runs'. Through its endpoint, of no member stream, a controller asks in the zero run:
        /// its members 1 and 2 run; a TERM to member 1 and a KILL to member 2 each go to its VM
        /// as init's SIGNAL built-in, told (`EXEC_TOLD`), once its command has started, and are
        /// taken; a resize of member 1, which has no terminal of its own, is refused `Terminal`;
        /// each one's end as its init told it (143 and 137) reaches its lane; member 0, files alone, is refused `Refused`; its MCP connection, held as its VM
        /// handed it over, carries the harness's bytes to the controller and back; and once the
        /// run is over a member's lane is refused `Failed`.
        #[test]
        fn an_orchestrators_run_hosts_its_lanes_as_the_daemons_runs() {
            use shards_remote::control::exec::{Unstarted, Unsteered};
            use shards_remote::control::{
                Controller, Fault, Got, McpConnection, Member, MemberControl, Subscriptions,
            };
            use std::io::Read as _;
            let dir = shards_testdir::TempDir::new("api-lanes").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            let kiln = shards_remote::identity::Identity::generate().unwrap();
            let admissions = shards_api::control::exec_admissions();
            let budget = std::sync::Arc::new(Mutex::new(Some(0)));
            let charge = std::sync::Arc::new(Charge::reserve(&budget, 0).unwrap());
            let agent = Agent {
                admissions,
                charge: std::sync::Arc::clone(&charge),
                begun: Mutex::new(Begun::Awaited),
                began: std::sync::Condvar::new(),
                ends: members::Ends::new(3).unwrap(),
                active: vec![1, 2],
                terminals: Vec::new(),
                inputs: Vec::new(),
                stdio: false,
                bridges: Some(mcp::Bridges::new(vec![1], protocol::CHUNK, charge)),
            };
            let live = Live {
                agent: Some(agent),
                ..live(&answers, dir.path(), link, None)
            };
            let agent = live.agent.as_ref().unwrap();
            let ask = wire::ControlAsk {
                controller: kiln.fingerprint(),
                sockets: None,
                stdio: false,
            };
            let (endpoint, served) =
                bind_control(ControlKey::made().unwrap(), &ask, admissions, &[], &[]).unwrap();
            let (key, address) = (endpoint.fingerprint, endpoint.address);
            let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
            std::thread::scope(|scope| {
                let serving = scope.spawn(move || endpoint.run());
                host_lanes(&live, agent, served.execs, served.members, scope);
                // Its VM: the built-ins it is handed, a TERM to member 1 and a KILL to member 2,
                // each taken.
                let process = scope.spawn(|| {
                    for (number, member, signal) in [(0, 1, 15), (1, 2, 9)] {
                        let m = shards_ipc::recv(&vm).unwrap().unwrap();
                        assert_eq!(m.kind, kind::EXEC_RUN);
                        let (n, rest) = m.payload.split_first_chunk::<8>().unwrap();
                        assert_eq!((u64::from_be_bytes(*n), rest[0]), (number, shards_ipc::EXEC_TOLD));
                        let spec = protocol::Spec::decode(&rest[1..]).unwrap();
                        assert_eq!(spec.builtin, protocol::builtin::SIGNAL);
                        assert_eq!(spec.argv, protocol::members::signal_args(member, signal));
                        let mut fds = m.fds.into_iter();
                        let conn = Stream::from(fds.next().unwrap());
                        drop(fds);
                        shards_ipc::send(&conn, kind::STARTED, &[], &[]).unwrap();
                        shards_ipc::send(&conn, kind::EXIT, &[0], &[]).unwrap();
                    }
                });
                let (controller, dialed) = Controller::dial(
                    &kiln,
                    key,
                    address,
                    Subscriptions {
                        outputs: &[],
                        inputs: &[],
                        execs: Some(admissions),
                        sockets: None,
                    },
                )
                .unwrap();
                let driver = scope.spawn(move || controller.run());
                let execs = dialed.execs.as_ref().unwrap();
                agent.stands(Begun::Started);
                let Member::Running(MemberControl {
                    mut steering,
                    mut end,
                }) = answered!(execs.member(1)).unwrap()
                else {
                    panic!("member 1 runs");
                };
                assert_eq!(steering.signal(15, std::time::Duration::MAX), Ok(Some(())));
                let Member::Running(MemberControl {
                    steering: mut killed,
                    end: mut kill_end,
                }) = answered!(execs.member(2)).unwrap()
                else {
                    panic!("member 2 runs");
                };
                assert_eq!(killed.signal(9, std::time::Duration::MAX), Ok(Some(())));
                process.join().unwrap();
                let size = protocol::Size { rows: 50, cols: 132 };
                assert_eq!(
                    steering.resize(size, std::time::Duration::MAX),
                    Err(Fault::Unsteered(Unsteered::Terminal))
                );
                agent.ends.told(1, protocol::members::End::Signaled(15)).unwrap();
                agent.ends.told(2, protocol::members::End::Signaled(9)).unwrap();
                assert_eq!(end.wait(), Ok(shards_remote::control::exec::End::Signaled(15)));
                assert_eq!(
                    kill_end.wait(),
                    Ok(shards_remote::control::exec::End::Signaled(9))
                );
                assert_eq!(
                    answered!(execs.member(0)).map(|_| ()),
                    Err(Fault::Unstarted(Unstarted::Refused))
                );
                // Its MCP connection, as its VM hands it over.
                let (conn, mut harness) = Stream::pair().unwrap();
                let first = shards_abi::run::mcp::ACCEPTED_FIRST;
                let accepted = shards_ipc::Accepted {
                    number: first,
                    member: 1,
                };
                let bridges = agent.bridges.as_ref();
                let held = mcp_accepted(
                    &lock(&live.link),
                    bridges,
                    &accepted.encode(),
                    vec![OwnedFd::from(conn)],
                );
                assert_eq!(held, Some(first));
                let m = shards_ipc::recv(&vm).unwrap().unwrap();
                assert_eq!(
                    (m.kind, m.payload),
                    (kind::LANE_HELD, first.to_be_bytes().to_vec())
                );
                scope.spawn(move || bridges.unwrap().watch(first));
                std::io::Write::write_all(&mut harness, request).unwrap();
                let McpConnection {
                    mut from,
                    mut to,
                    mut end,
                } = answered!(execs.mcp(1)).unwrap();
                assert_eq!(
                    from.next_record().unwrap(),
                    Got::Record {
                        at: 0,
                        bytes: request.to_vec()
                    }
                );
                assert_eq!(to.write(b"{}\n").unwrap(), 3);
                let mut came = [0; 3];
                shards_ipc::Polled(&harness).read_exact(&mut came).unwrap();
                assert_eq!(&came, b"{}\n");
                harness.shutdown(std::net::Shutdown::Write).unwrap();
                assert_eq!(from.next_record().unwrap(), Got::Finished);
                assert_eq!(end.wait(), Ok(0));
                assert_eq!(
                    answered!(execs.mcp(0)).map(|_| ()),
                    Err(Fault::Unstarted(Unstarted::Refused))
                );
                agent.over();
                assert_eq!(
                    answered!(execs.member(1)).map(|_| ()),
                    Err(Fault::Unstarted(Unstarted::Failed))
                );
                assert_eq!(
                    answered!(execs.mcp(1)).map(|_| ()),
                    Err(Fault::Unstarted(Unstarted::Failed))
                );
                served.revoke.revoke();
                serving.join().unwrap().unwrap();
                let _ = driver.join();
            });
            assert_eq!(*lock(&budget), Some(0), "its lanes' charge outlived them");
        }

        /// D252: an orchestrator's run that asks for its members' stdio (`ControlAsk::stdio`)
        /// serves it on its endpoint as the daemon's runs serve their owners' streams (D238),
        /// each generation's its own (D270). Do: bind member 1's streams; dial its stdout, its
        /// stdin and member 0's stdout; have its guest bring a record each time it is
        /// credited; write its stdin, end it, and have its init tell that end; start its next
        /// generation and dial that one's stdout and a generation not started. Expect: its
        /// guest credited only as its lane has room, each record its lane's whole, then the
        /// output's end; its orchestrator told each channel's end and none of its bytes; its
        /// stdin's record one `STDIN` frame on its input connection, its end an empty one, and
        /// its init's word its lane's end; member 0, which runs nothing, refused `Scope`; its
        /// next generation's records its own lane's; a generation not started refused `Stale`.
        #[test]
        fn an_orchestrators_run_serves_its_members_stdio_on_its_endpoint_by_generation() {
            use shards_remote::control::{Controller, Fault, Got, Subscriptions};
            use shards_remote::managed::Reason;
            use std::io::Read as _;
            /// Member 1's stdout record at `offset` saying `event`, as its guest's capture
            /// writes it.
            fn encoded(offset: u64, event: managed::Event<'_>) -> Vec<u8> {
                let record = managed::Record {
                    member: 1,
                    channel: managed::Channel::Stdout,
                    offset,
                    event,
                };
                let mut bytes = vec![0u8; record.encoded_len().unwrap()];
                record.encode(&mut bytes).unwrap();
                bytes
            }
            /// Its guest's capture: each of `records` written once member 1's stdout is credited.
            fn guest_brings(capture: &Stream, records: &[Vec<u8>]) {
                use std::io::{Read as _, Write as _};
                for record in records {
                    let mut head = [0u8; protocol::HEADER];
                    (&*capture).read_exact(&mut head).unwrap();
                    let (which, len) = protocol::parse_header(head).unwrap();
                    assert_eq!(which, protocol::kind::MANAGED_CREDIT);
                    let mut credit = vec![0u8; len as usize];
                    (&*capture).read_exact(&mut credit).unwrap();
                    let credit = managed::Credit::decode(&credit).unwrap();
                    assert_eq!((credit.member, credit.channel), (1, managed::Channel::Stdout));
                    assert!(credit.capacity >= protocol::CHUNK as u64, "{credit:?}");
                    (&*capture)
                        .write_all(&protocol::header(protocol::kind::MANAGED, record.len() as u32))
                        .unwrap();
                    (&*capture).write_all(record).unwrap();
                }
            }
            let dir = shards_testdir::TempDir::new("api-stdio").unwrap();
            let said = Said::default();
            let answers = super::super::Answers(Mutex::new(Box::new(said.clone())));
            let (link, _vm_link) = Stream::pair().unwrap();
            let (capture, its_guest) = Stream::pair().unwrap();
            let reading = capture.try_clone().unwrap();
            let kiln = shards_remote::identity::Identity::generate().unwrap();
            let admissions = shards_api::control::exec_admissions();
            let budget = std::sync::Arc::new(Mutex::new(Some(0)));
            let agent = Agent {
                admissions,
                charge: std::sync::Arc::new(Charge::reserve(&budget, 0).unwrap()),
                begun: Mutex::new(Begun::Awaited),
                began: std::sync::Condvar::new(),
                ends: members::Ends::new(2).unwrap(),
                active: vec![1],
                terminals: Vec::new(),
                inputs: vec![1],
                stdio: true,
                bridges: None,
            };
            let live = Live {
                agent: Some(agent),
                feeds: Mutex::new(HashMap::from([(1, Feed::default())])),
                ..live(&answers, dir.path(), link, Some(capture))
            };
            let live = &live;
            let agent = live.agent.as_ref().unwrap();
            let (outputs, inputs) = first_streams(agent).unwrap();
            assert_eq!(outputs, [(1, 0, Channel::Stdout), (1, 0, Channel::Stderr)]);
            assert_eq!(inputs, [1]);
            let ask = wire::ControlAsk {
                controller: kiln.fingerprint(),
                sockets: None,
                stdio: true,
            };
            let (endpoint, served) =
                bind_control(ControlKey::made().unwrap(), &ask, admissions, &outputs, &inputs).unwrap();
            let (key, address) = (endpoint.fingerprint, endpoint.address);
            let (stdout0, admitted) = shards_api::control::member_output(1, 0, Channel::Stdout);
            let (files_alone, _) = shards_api::control::member_output(0, 0, Channel::Stdout);
            let (stdin0, input_admitted) = shards_api::control::member_input(1, 0);
            let (stdout1, _) = shards_api::control::member_output(1, 1, Channel::Stdout);
            let (stdout2, _) = shards_api::control::member_output(1, 2, Channel::Stdout);
            let first = [
                encoded(0, managed::Event::Open("agent one")),
                encoded(0, managed::Event::Data(b"hello\n")),
                encoded(6, managed::Event::End),
            ];
            let next = [
                encoded(0, managed::Event::Open("agent one")),
                encoded(0, managed::Event::End),
            ];
            let revoke = served.revoke;
            std::thread::scope(|scope| {
                let serving = scope.spawn(move || endpoint.run());
                serve_stdio(
                    live,
                    agent,
                    served.generations,
                    served.outputs,
                    served.inputs,
                    scope,
                );
                let capturing = scope.spawn(move || {
                    let read = read_records(live, reading);
                    let cut = live.cut();
                    (read, cut.is_ok())
                });
                let (controller, mut dialed) = Controller::dial(
                    &kiln,
                    key,
                    address,
                    Subscriptions {
                        outputs: &[(stdout0, admitted, 0), (files_alone, admitted, 0)],
                        inputs: &[(stdin0, input_admitted, 0)],
                        execs: None,
                        sockets: None,
                    },
                )
                .unwrap();
                let driver = scope.spawn(move || controller.run());
                // Its stdout: each record as its guest brought it, credited one at a time.
                let feeding = scope.spawn(|| guest_brings(&its_guest, &first));
                let mut stdout = dialed.outputs.remove(0);
                let mut at = 0u64;
                for record in &first {
                    assert_eq!(
                        stdout.next_record(),
                        Ok(Got::Record {
                            at,
                            bytes: record.clone()
                        })
                    );
                    at += record.len() as u64;
                }
                assert_eq!(stdout.next_record(), Ok(Got::Finished));
                feeding.join().unwrap();
                // Member 0 runs nothing: the endpoint serves no stream of its.
                let mut files = dialed.outputs.remove(0);
                assert_eq!(files.next_record(), Err(Fault::Refused(Reason::Scope)));
                // Its stdin: its record and its end on its input connection, as its VM handed
                // it over, and its init's word on its end its lane's.
                let (ours, init) = Stream::pair().unwrap();
                input_arrived(live, &1u32.to_be_bytes(), vec![OwnedFd::from(ours)]);
                let initing = scope.spawn(move || {
                    let mut frame = [0u8; protocol::HEADER + 5];
                    (&init).read_exact(&mut frame).unwrap();
                    let mut end = [0u8; protocol::HEADER];
                    (&init).read_exact(&mut end).unwrap();
                    let told = managed::InputEnded {
                        member: 1,
                        written: 5,
                        end: managed::InputEnd::Closed,
                    }
                    .encode()
                    .unwrap();
                    input_ended(live, &told);
                    (frame, end)
                });
                let mut stdin = dialed.inputs.remove(0);
                assert_eq!(stdin.write(b"typed"), Ok(5));
                assert_eq!(stdin.end(), Ok((5, managed::InputEnd::Closed)));
                let (frame, end) = initing.join().unwrap();
                assert_eq!(
                    frame[..protocol::HEADER],
                    protocol::header(protocol::kind::STDIN, 5)
                );
                assert_eq!(&frame[protocol::HEADER..], b"typed");
                assert_eq!(end, protocol::header(protocol::kind::STDIN, 0));
                // Member 1 started again (D270): its next generation's lanes joined, as its
                // restart joins them before its start, and dialed by its controller anew.
                lock(&live.generations).current.insert(1, 1);
                live.stdio.get().unwrap().join(live, 1, 1, false, scope).unwrap();
                let (again, mut redialed) = Controller::dial(
                    &kiln,
                    key,
                    address,
                    Subscriptions {
                        outputs: &[(stdout1, admitted, 0), (stdout2, admitted, 0)],
                        inputs: &[],
                        execs: None,
                        sockets: None,
                    },
                )
                .unwrap();
                let redriver = scope.spawn(move || again.run());
                let feeding = scope.spawn(|| guest_brings(&its_guest, &next));
                let mut stdout = redialed.outputs.remove(0);
                let mut at = 0u64;
                for record in &next {
                    assert_eq!(
                        stdout.next_record(),
                        Ok(Got::Record {
                            at,
                            bytes: record.clone()
                        })
                    );
                    at += record.len() as u64;
                }
                assert_eq!(stdout.next_record(), Ok(Got::Finished));
                feeding.join().unwrap();
                let mut unstarted = redialed.outputs.remove(0);
                assert_eq!(unstarted.next_record(), Err(Fault::Refused(Reason::Stale)));
                // Its capture's end cuts what never ended (each stderr), and the run's end
                // revokes its endpoint.
                its_guest.shutdown(std::net::Shutdown::Both).unwrap();
                let (read, cut) = capturing.join().unwrap();
                assert_eq!(read, Ok(()));
                assert!(cut);
                revoke.revoke();
                serving.join().unwrap().unwrap();
                drop(dialed);
                drop(redialed);
                let _ = driver.join();
                let _ = redriver.join();
            });
            // Its orchestrator heard each output's end, and none of its bytes.
            let all = lock(&said.0).clone();
            let mut from = &all[..];
            let mut ends = Vec::new();
            while let Some((kind, payload)) = wire::read_frame(&mut from).unwrap() {
                assert_eq!(kind, wire::to_client::OUTPUT_END, "a lane's bytes on the channel");
                ends.push(wire::OutputEnd::decode(&payload).unwrap());
            }
            let closed = |generation| wire::OutputEnd {
                member: 1,
                generation,
                channel: Channel::Stdout,
                end: wire::StreamEnd::Closed,
            };
            assert_eq!(ends, [closed(0), closed(1)]);
        }

        /// D254: a member's MCP connection its VM hands over unprompted is answered as taken
        /// (M24) and held where its run gave the member a bridge, its far end open, for its
        /// controller's asks; let go where it gave it none, or where the run gave no member one:
        /// its far end reads its end. One its VM did not number as its own is let go unanswered.
        #[test]
        fn a_members_mcp_connection_is_held_where_its_run_gave_it_a_bridge() {
            use std::io::Read as _;
            let (link, vm) = Stream::pair().unwrap();
            let first = shards_abi::run::mcp::ACCEPTED_FIRST;
            let budget = std::sync::Arc::new(Mutex::new(Some(0)));
            let charge = std::sync::Arc::new(Charge::reserve(&budget, 0).unwrap());
            let bridges = mcp::Bridges::new(vec![1], protocol::CHUNK, charge);
            let owners = shards_ipc::Accepted {
                number: first - 1,
                member: 1,
            };
            assert_eq!(
                mcp_accepted(&link, Some(&bridges), &owners.encode(), Vec::new()),
                None
            );
            for (number, member, bridges, held) in [
                (first, 1, Some(&bridges), true),
                (first + 1, 0, Some(&bridges), false),
                (first + 2, 1, None, false),
            ] {
                let (conn, far) = Stream::pair().unwrap();
                let accepted = shards_ipc::Accepted { number, member };
                let said = mcp_accepted(&link, bridges, &accepted.encode(), vec![OwnedFd::from(conn)]);
                assert_eq!(said, held.then_some(number), "member {member}");
                let m = shards_ipc::recv(&vm).unwrap().unwrap();
                assert_eq!(
                    (m.kind, m.payload),
                    (kind::LANE_HELD, number.to_be_bytes().to_vec())
                );
                far.set_nonblocking(true).unwrap();
                let read = shards_ipc::Polled(&far).read(&mut [0; 1]);
                match held {
                    true => assert!(
                        matches!(&read, Err(e) if e.kind() == io::ErrorKind::WouldBlock),
                        "{read:?}"
                    ),
                    false => assert_eq!(read.unwrap(), 0, "member {member}: let go"),
                }
            }
            bridges.close();
        }

        /// D259's run host, its VM played by the test on the VM's end of the link and the link
        /// followed as a run's is. Do: save where the owner may, and while its VM holds its
        /// answer ask a second; save again, the VM refusing; save once more, the VM's link
        /// closing before it answers; then save after. Expect: `SAVE` brings the generation's
        /// six files, `SAVE_LEAVE_RUNNING`, fresh tokens and the generation's and the save's
        /// names; the second refused `State`, nothing of it made; the first committed, a
        /// snapshot holding what its VM wrote; the refusal `Failed` in the VM's words and the
        /// stopped VM `State`, nothing of either left; and no save once its VM is gone.
        #[test]
        fn a_save_is_handed_to_its_vm_and_committed_once_its_vm_filled_it() {
            use std::io::Write as _;
            let dir = shards_testdir::TempDir::new("api-save").unwrap();
            let answers = super::super::Answers(Mutex::new(Box::new(Vec::<u8>::new())));
            let (link, vm) = Stream::pair().unwrap();
            // A bound on the VM's waits, as the session's tests bound theirs: it decides no
            // result, but ends a run whose save went wrong failing, never hanging, its link
            // closing as its thread goes.
            vm.set_read_timeout(Some(std::time::Duration::from_secs(120)))
                .unwrap();
            let followed = link.try_clone().unwrap();
            let live = live(&answers, dir.path(), link, None);
            let to = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
            if !shards_vmm::vm::SNAPSHOTS {
                assert_eq!(
                    save(&live, &to("s1")).err().map(|e| e.0),
                    Some(wire::SaveRefusal::Unsupported)
                );
                return;
            }
            std::thread::scope(|scope| {
                let (watched, followed) = (&live, &followed);
                let following = scope.spawn(move || follow(watched, followed, scope));
                let playing = scope.spawn(|| {
                    // Its first save, filled; its second, refused; its third never answered.
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::SAVE);
                    let (flags, rest) = m.payload.split_first().unwrap();
                    assert_eq!(*flags, shards_ipc::SAVE_LEAVE_RUNNING);
                    let (tokens, names) = rest.split_at(2 * shards_abi::run::TOKEN);
                    assert!(tokens.iter().any(|&b| b != 0), "its tokens are fresh");
                    let names: Vec<&[u8]> = names.split(|&b| b == 0).collect();
                    assert!(names[0].starts_with(b"g-") && names[1] == b"s1" && names[2] == b"");
                    assert_eq!(m.fds.len(), 6);
                    assert_eq!(
                        save(&live, &to("s2")),
                        Err((wire::SaveRefusal::State, "a save of it is under way".into()))
                    );
                    assert!(!dir.path().join("s2").exists());
                    let mut memory = std::fs::File::from(m.fds.into_iter().next().unwrap());
                    memory.write_all(b"its guest's memory").unwrap();
                    drop(memory);
                    shards_ipc::send(&vm, kind::SAVED, &[], &[]).unwrap();
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::SAVE);
                    shards_ipc::send(&vm, kind::SAVED, b"its guest did not go quiet", &[]).unwrap();
                    let m = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(m.kind, kind::SAVE);
                    drop(vm);
                });
                assert_eq!(save(&live, &to("s1")), Ok(()));
                let saved = dir.path().join("s1");
                assert!(shards_vmm::snapshot::exists(&saved));
                let generation = shards_vmm::snapshot::generation(&saved).unwrap();
                let memory = std::fs::read(generation.join("memory")).unwrap();
                assert_eq!(memory, b"its guest's memory");
                assert_eq!(
                    save(&live, &to("s3")),
                    Err((wire::SaveRefusal::Failed, "its guest did not go quiet".into()))
                );
                assert!(!dir.path().join("s3").exists());
                assert_eq!(
                    save(&live, &to("s4")),
                    Err((wire::SaveRefusal::State, "its microVM stopped".into()))
                );
                assert!(!dir.path().join("s4").exists());
                playing.join().unwrap();
                following.join().unwrap();
                assert_eq!(
                    save(&live, &to("s5")),
                    Err((wire::SaveRefusal::State, "its microVM has stopped".into()))
                );
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// D270 slice 4: a component fits the member at its path where its platform is one its
    /// microVM runs (or none) and its asks are within what that member is granted: a network ask
    /// the image's grants or the member's ceiling cover, a volume the image gives it. Another
    /// platform, a port or a destination past them, a volume of none and one the image gives
    /// another member, each refused typed.
    #[test]
    fn a_component_fits_its_member_within_its_platform_and_grants() {
        use wire::SwapRefusal as R;
        let agentfile = crate::agentfile::Agentfile {
            grants: shards_agentfile::agentfile::Grants {
                egress: vec![b"443/tcp".to_vec()],
                ..Default::default()
            },
            volumes: vec![crate::agentfile::AgentVolume {
                name: None,
                paths: vec!["/data".into()],
                scoped: true,
                scope: shards_agentfile::agentfile::Scope {
                    kind: Some(shards_agentfile::agentfile::TargetKind::Harness),
                    names: vec![b"codex".to_vec()],
                },
            }],
            ..Default::default()
        };
        let members = members_of(&["agent helper".into(), "harness codex".into()], &[0, 1]);
        let ceilings = vec![(
            1,
            shards_abi::run::egress::Grants {
                ports: vec![(6, 8000, 8100)],
                named: Vec::new(),
                dns: false,
            },
        )];
        let here = shards_image::platform::guest()
            .first()
            .unwrap()
            .architecture
            .clone();
        let config = |arch: &str, network: &[&str], volumes: &[&str]| shards_image::osi::Config {
            schema_version: 1,
            name: "codex".into(),
            version: String::new(),
            description: String::new(),
            platform: Some(shards_image::osi::Platform {
                os: "linux".into(),
                architecture: arch.into(),
                variant: None,
            }),
            run: None,
            skills: Vec::new(),
            mcp: Vec::new(),
            asks: shards_image::osi::Asks {
                network: network.iter().map(|n| n.to_string()).collect(),
                volumes: volumes.iter().map(|v| v.to_string()).collect(),
                processes: None,
                memory: None,
            },
        };
        let fits = |c: &shards_image::osi::Config, at: &str| {
            component_fits(c, Some(&agentfile), &ceilings, &members, at)
        };
        assert_eq!(
            fits(
                &config(&here, &["443/tcp", "8080/tcp"], &["/data"]),
                "/harness/codex"
            ),
            Ok(())
        );
        assert_eq!(
            fits(&config(&here, &["api.example.com:443"], &[]), "/harness/codex"),
            Ok(())
        );
        let unbound = shards_image::osi::Config {
            platform: None,
            ..config(&here, &[], &[])
        };
        assert_eq!(
            fits(&unbound, "/harness/codex"),
            Ok(()),
            "no platform runs anywhere"
        );
        assert!(matches!(
            fits(&config("riscv64", &[], &[]), "/harness/codex"),
            Err((R::Platform, _))
        ));
        for past in [&["22/tcp"][..], &["evil.example:9443"]] {
            assert!(
                matches!(
                    fits(&config(&here, past, &[]), "/harness/codex"),
                    Err((R::Asks, _))
                ),
                "{past:?}"
            );
        }
        assert!(matches!(
            fits(&config(&here, &[], &["/other"]), "/harness/codex"),
            Err((R::Asks, _))
        ));
        // The ceiling's ports are codex's alone; the volume too.
        assert!(matches!(
            fits(&config(&here, &["8080/tcp"], &[]), "/agents/helper"),
            Err((R::Asks, _))
        ));
        assert!(matches!(
            fits(&config(&here, &[], &["/data"]), "/agents/helper"),
            Err((R::Asks, _))
        ));
    }

    /// D270 slice 4: an image run's microVM has a slot for a swap's old and new component of each
    /// agent and harness its image declares, and one for each plug root; never fewer than a run's.
    #[test]
    fn an_image_runs_slots_hold_a_swaps_old_and_new_and_its_plugs() {
        let agentfile = crate::agentfile::Agentfile {
            domains: 2,
            ..Default::default()
        };
        let floor = crate::daemon::pci_slots();
        assert_eq!(image_slots(Some(&agentfile), &[0; 3]), 7u8.max(floor));
        assert_eq!(image_slots::<u8>(None, &[]), floor);
        let many = crate::agentfile::Agentfile {
            domains: 1000,
            ..Default::default()
        };
        assert_eq!(image_slots::<u8>(Some(&many), &[]), u8::MAX);
    }

    fn members() -> Vec<wire::MemberInfo> {
        members_of(&["agent helper".into(), "harness codex".into()], &[1])
    }

    /// D233: a VM's GPU report is read to its pipe's end, at most a byte past what a report of
    /// its GPUs holds, so that its orchestrator refuses a longer one (`Count`) with no more of
    /// it read; none written is none read.
    #[test]
    fn a_vms_gpu_report_is_read_to_its_end_within_its_bound() {
        use std::io::Write as _;
        let most = shards_abi::gpu::report_bytes(1).unwrap();
        for (wrote, read) in [(5, 5), (most, most), (most + 10, most + 1), (0, 0)] {
            let (from, mut to) = std::io::pipe().unwrap();
            // On a thread of its own: past a pipe's buffer, a write waits for its reader.
            let writer = std::thread::spawn(move || {
                let _ = to.write_all(&vec![b'x'; wrote]);
            });
            assert_eq!(hosted::read_report(from, 1).len(), read, "{wrote} written");
            writer.join().unwrap();
        }
    }

    /// D259: a save is its run owner's alone, asked on the run's own channel. Do: read a `SAVE`
    /// as the run's asks read its owner's frames, a malformed one, and one come before the
    /// run's request, as on a warm host's channel, its pool's. Expect: the owner's decoded, its
    /// directory whole; the malformed one breaking the channel; the early one refusing the run
    /// (`Cause::Request`), so that no save is served on a channel no run owner holds.
    #[test]
    fn a_save_is_served_on_its_run_owners_channel_alone() {
        let ask = wire::SaveAsk {
            id: 3,
            to: "/home/u/.local/share/shards/saves/s1".into(),
        };
        let payload = ask.encode().unwrap();
        match judged(wire::to_host::SAVE, &payload) {
            Ok(Ask::Save(read)) => assert_eq!(read, ask),
            _ => panic!("the owner's SAVE is not its ask"),
        }
        let malformed = wire::SaveAsk::decode(&payload[..4]).err().unwrap();
        assert_eq!(
            judged(wire::to_host::SAVE, &payload[..4]).err(),
            Some(AsksEnd::Failed(format!(
                "its orchestrator's channel broke: a malformed SAVE: {malformed}"
            )))
        );
        let No(cause, why) = before_request(wire::to_host::SAVE);
        assert_eq!(cause, Cause::Request);
        assert_eq!(
            why,
            format!("message kind {} before the run's request", wire::to_host::SAVE)
        );
    }

    /// D267: a `PAUSE` is its owner's ask once its run runs, a malformed one breaking the
    /// channel; before the run's request (a warm host waiting for its `CREATE`) it is refused,
    /// typed, as every ask before a run is.
    #[test]
    fn a_pause_is_an_ask_of_a_run_and_refused_before_one() {
        for pause in [true, false] {
            let ask = wire::PauseAsk { id: 3, pause };
            match judged(wire::to_host::PAUSE, &ask.encode()) {
                Ok(Ask::Pause(read)) => assert_eq!(read, ask),
                _ => panic!("the owner's PAUSE is not its ask"),
            }
        }
        assert!(matches!(
            judged(wire::to_host::PAUSE, &[0, 0, 0, 3, 7]),
            Err(AsksEnd::Failed(_))
        ));
        let No(cause, why) = before_request(wire::to_host::PAUSE);
        assert_eq!(cause, Cause::Request);
        assert_eq!(
            why,
            format!("message kind {} before the run's request", wire::to_host::PAUSE)
        );
    }

    /// D259, R04: a run's saves go one at a time, and none once its VM's link has closed. Do:
    /// ask a save, ask another while it is under way, answer it, ask again and have its VM
    /// refuse, give one up before its VM heard it, answer when none was asked, then close the
    /// link with one asked. Expect: the second refused `State`; each answer told its own save
    /// alone, saved or in its VM's words; an answer none asked changing nothing; the save asked
    /// as the link closes hearing its microVM stopped; and every save after refused `State`.
    /// Do walk the run's pause through its states (D267): a pause, a second pause, a resume, a
    /// second resume, a pause its VM refused, a resume its VM refused, and the VM gone while
    /// paused. Expect: its guest asked nothing from the pause's start to its resume's end; a
    /// pause or resume of a run already so done, unasked; paused time counting from the VM's
    /// word to its resume's and folding in a pause under way as its VM goes; what its init last
    /// said its stats while paused alone; a refused pause leaving it running and a refused
    /// resume paused.
    #[test]
    fn a_runs_pause_moves_by_its_vms_word_and_counts_its_time() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut p = Pauses::new();
        p.last = Some(b"cpu 5\n".to_vec());
        assert!(p.guest_runs());
        assert_eq!(p.said(), None, "a running run's stats are its guest's to say");
        assert_eq!(p.pause(at(0)), Step::Ask);
        assert!(!p.guest_runs(), "its guest asked as it paused");
        assert_eq!(p.said(), Some(Some(&b"cpu 5\n"[..])));
        let held = p.held(at(10)).unwrap();
        assert_eq!(
            held,
            wire::Paused {
                total: Duration::ZERO,
                now: true
            }
        );
        assert_eq!(
            p.pause(at(30)),
            Step::Done(wire::Paused {
                total: Duration::from_millis(20),
                now: true
            })
        );
        assert_eq!(p.resume(at(40)), Step::Ask);
        assert!(!p.guest_runs(), "its guest asked before its VM let it go on");
        let resumed = p.resumed(at(50)).unwrap();
        assert_eq!(
            resumed,
            wire::Paused {
                total: Duration::from_millis(40),
                now: false
            }
        );
        assert_eq!(p.resume(at(60)), Step::Done(resumed));
        assert!(p.guest_runs());
        // Its VM refuses the pause: running; then paused, a resume refused: paused still.
        assert_eq!(p.pause(at(70)), Step::Ask);
        p.unheld();
        assert!(matches!(p.state, Pause::Running));
        assert_eq!(p.pause(at(80)), Step::Ask);
        assert!(p.held(at(80)).is_some());
        assert_eq!(p.resume(at(90)), Step::Ask);
        p.unresumed();
        assert!(matches!(p.state, Pause::Paused { .. }));
        // Gone while paused: the pause's time so far is kept, and nothing is asked again.
        p.gone(at(100));
        assert_eq!(
            p.paused(at(500)),
            wire::Paused {
                total: Duration::from_millis(60),
                now: false
            }
        );
        assert_eq!(p.pause(at(500)), Step::Gone);
        assert_eq!(p.resume(at(500)), Step::Gone);
        assert!(p.held(at(500)).is_none());
    }

    #[test]
    fn a_runs_saves_go_one_at_a_time_and_end_with_its_vm() {
        let mut saving = Saving::Idle;
        let said = saving.ask().unwrap();
        assert_eq!(
            saving.ask().err(),
            Some((wire::SaveRefusal::State, "a save of it is under way".into()))
        );
        saving.answered(b"");
        assert_eq!(said.try_recv(), Ok(Ok(())));
        let said = saving.ask().unwrap();
        saving.answered(b"its guest did not go quiet");
        assert_eq!(said.try_recv(), Ok(Err("its guest did not go quiet".into())));
        let said = saving.ask().unwrap();
        saving.unasked();
        assert!(said.try_recv().is_err(), "a save given up hears nothing");
        saving.answered(b"");
        assert!(
            matches!(saving, Saving::Idle),
            "an answer none asked asks nothing"
        );
        let said = saving.ask().unwrap();
        saving.gone();
        assert_eq!(said.recv(), Err(std::sync::mpsc::RecvError));
        saving.answered(b"");
        saving.unasked();
        assert_eq!(
            saving.ask().err(),
            Some((wire::SaveRefusal::State, "its microVM has stopped".into()))
        );
    }

    /// D259: a save is made only where its owner may have it. Do: ask for one at a relative
    /// path, outside the run's home, through a link out of it, beneath a missing directory,
    /// beneath and holding a directory its guest reaches, at and beneath the home's volumes
    /// (none made yet), at a fresh name in it, then make it twice. Expect: each refused
    /// `Destination` saying why; the fresh one its parent's canonical path and name, made its
    /// user's alone, and refused once something is there.
    #[test]
    fn a_save_is_made_only_where_its_owner_may_have_it() {
        use std::path::Path;
        let root = shards_testdir::TempDir::new("api-save-to").unwrap();
        let home = root.path().join("home");
        let elsewhere = root.path().join("elsewhere");
        let share = home.join("project");
        for dir in [&home, &elsewhere, &share] {
            std::fs::create_dir(dir).unwrap();
        }
        let reached = [std::fs::canonicalize(&share).unwrap()];
        let refused = |to: &Path| {
            let (cause, why) = destination(to, &home, &reached).err().unwrap();
            assert_eq!(cause, wire::SaveRefusal::Destination, "{}", to.display());
            why
        };
        // A parent that is not there is refused in its own words, naming it.
        let missing = |to: &Path| {
            let why = refused(to);
            let parent = to.parent().unwrap().display().to_string();
            assert!(why.starts_with(&format!("{parent}: ")), "{why}");
        };
        assert_eq!(refused(Path::new("saves/s1")), "saves/s1: not an absolute path");
        assert!(refused(&elsewhere.join("s1")).contains(": outside the run's home, "));
        missing(&home.join("missing").join("s1"));
        missing(&share.join("deep").join("s1"));
        assert!(refused(&share).contains(": at "));
        assert!(refused(&share.join("s1")).contains(": beneath "));
        let volumes = home.join(crate::volumes::ROOT);
        assert!(refused(&volumes).contains(": at "), "none made yet");
        std::fs::create_dir(&volumes).unwrap();
        missing(&volumes.join("v").join("s1"));
        assert!(refused(&volumes.join("s1")).contains(": beneath "));
        #[cfg(unix)]
        {
            let link = home.join("out");
            std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
            assert!(refused(&link.join("s1")).contains(": outside the run's home, "));
        }
        let within = [std::fs::canonicalize(&share).unwrap().join("x")];
        let (cause, why) = destination(&share, &home, &within).err().unwrap();
        assert_eq!(cause, wire::SaveRefusal::Destination);
        assert!(why.contains(": holding "), "{why}");
        let target = destination(&home.join("s1"), &home, &reached).unwrap();
        assert_eq!(target, std::fs::canonicalize(&home).unwrap().join("s1"));
        made(&target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let (cause, why) = made(&target).err().unwrap();
        assert_eq!(cause, wire::SaveRefusal::Destination);
        assert!(why.ends_with(": something is there already"), "{why}");
    }

    /// Do end a run whose guest said `STARTED` with a `DONE` of a status alone; one whose
    /// guest never did with a status and the words of a command that never started (warm.rs
    /// `done`); one after a broken link; and one with no `DONE`. Expect: the status as the
    /// run's exit; a failure saying the command never started, its status and init's words,
    /// never that status as an exit; the link's failure; a failure.
    #[test]
    fn a_command_that_never_started_ends_its_run_failed_saying_why() {
        assert_eq!(run_end(None, Some(&[3]), true), wire::RunEnd::Exited(3));
        assert_eq!(run_end(None, Some(&[0]), true), wire::RunEnd::Exited(0));
        let never = b"\x80exec: \"/bin/sh\": operation not permitted";
        match run_end(None, Some(never), false) {
            wire::RunEnd::Failed(why) => assert_eq!(
                why,
                "its command never started (status 128): exec: \"/bin/sh\": operation not permitted"
            ),
            other => panic!("a command that never started ended its run as {other:?}"),
        }
        assert_eq!(
            run_end(Some("the VM's link: reset".into()), Some(&[3]), true),
            wire::RunEnd::Failed("the VM's link: reset".into())
        );
        assert!(matches!(run_end(None, None, false), wire::RunEnd::Failed(_)));
    }

    /// Do end a run whose guest said `STARTED` and whose init then failed it: `SYSTEM_ERR`'s
    /// words and `EXIT` 125 from init (init run.rs, the relay's failure), kept by its VM as
    /// dockerd keeps a run's (128, warm.rs `finish`), its `DONE` that code and init's words.
    /// Expect: the run failed in init's words, typed (the person's decision, 2026-11-14), never
    /// `Exited(128)`, which reads as its command's own exit (the apiend128 lane's report).
    #[test]
    fn an_init_that_fails_a_started_run_ends_it_failed_in_inits_words() {
        let words = "its capture: the managed output's control connection ended";
        let done = [&[128u8][..], words.as_bytes()].concat();
        assert_eq!(
            run_end(None, Some(&done), true),
            wire::RunEnd::Failed(format!("its init failed after its command started: {words}"))
        );
        let told = Told {
            started: true,
            unserved: None,
            done: Some(done),
            used: None,
        };
        assert_eq!(
            told.end(None),
            wire::RunEnd::Failed(format!("its init failed after its command started: {words}"))
        );
    }

    /// Do end a run whose VM could not serve it, as warm.rs `finish` tells it: why, then a
    /// `DONE` of the status a client exits with then (125), its command started or not; and
    /// one whose link broke after the VM said why. Expect: a failure in the VM's words each
    /// time, never the command's exit (api_image on HVF at 2b89e8504: `Exited(125)`, nothing
    /// said, its harness's output read whole).
    #[test]
    fn a_vm_that_could_not_serve_its_run_ends_it_failed_in_its_words() {
        let why = "the guest stopped before the command ended: Connection reset by peer";
        let unserved = |started: bool| Told {
            started,
            unserved: Some(why.into()),
            done: Some(vec![125]),
            used: None,
        };
        let failed = wire::RunEnd::Failed(format!("its VM could not serve its run: {why}"));
        assert_eq!(unserved(true).end(None), failed);
        assert_eq!(unserved(false).end(None), failed);
        assert_eq!(unserved(true).end(Some("the VM's link: reset".into())), failed);
        // One that served its run has its command's end, as before.
        let served = Told {
            started: true,
            unserved: None,
            done: Some(vec![125]),
            used: None,
        };
        assert_eq!(served.end(None), wire::RunEnd::Exited(125));
    }

    /// Do read a run's final use (D260) as its VM told it: each part read, each part's words, an
    /// answer none of the protocol's; and none told, its VM killed for its orchestrator's asks
    /// or not. Expect: each part as told, a broken answer typed by what breaks it, and no
    /// final use typed by why, killed only where its asks ended it, never a zero.
    #[test]
    fn a_runs_final_use_is_as_its_vm_told_it_or_typed_by_why_not() {
        let times = shards_abi::guest_time::GuestTimes {
            vcpus: vec![shards_abi::guest_time::VcpuTime {
                index: 0,
                guest_ns: 70,
                unread: None,
            }],
            departed_ns: 0,
        };
        let host = shards_abi::guest_time::HostUse {
            cpu_ns: 100,
            resident_bytes: 1,
            footprint_bytes: 2,
            charged: shards_abi::guest_time::Charged::ToThread,
        };
        let told = shards_abi::rate::used(Ok(&times), Ok(&host));
        assert_eq!(
            final_use(Some(told.as_bytes()), Some(&AsksEnd::Stopped)),
            wire::FinalUse::Read(wire::Microvm {
                guest: Ok(times),
                host: Ok(host),
            }),
            "told before its kill"
        );
        let words = shards_abi::rate::used(Err("the microVM has not started"), Err("its VM process: x"));
        assert_eq!(
            final_use(Some(words.as_bytes()), None),
            wire::FinalUse::Read(wire::Microvm {
                guest: Err("the microVM has not started".into()),
                host: Err("its VM process: x".into()),
            })
        );
        assert_eq!(
            final_use(Some(&told.as_bytes()[..told.len() - 1]), None),
            wire::FinalUse::Unread(wire::Unused::Broken(wire::UseBroken::Text))
        );
        assert_eq!(
            final_use(Some(b"USE\nx\n"), None),
            wire::FinalUse::Unread(wire::Unused::Broken(wire::UseBroken::Word))
        );
        for asks in [AsksEnd::Stopped, AsksEnd::Failed("its channel ended".into())] {
            assert_eq!(
                final_use(None, Some(&asks)),
                wire::FinalUse::Unread(wire::Unused::Killed)
            );
        }
        assert_eq!(
            final_use(None, None),
            wire::FinalUse::Unread(wire::Unused::Untold)
        );
    }

    /// Do end the orchestrator's asks by its `STOP`; by its channel's end without one; by a
    /// frame cut short; by a malformed credit; and by a frame of no ask's kind: each before
    /// the run's VM said its end, and after. Expect: `Stopped` for `STOP` alone; each other a
    /// failure naming its frame and why, never a silent `Stopped` (Hex's grants-none run
    /// ended `Stopped`, nothing said); and the VM's own end wherever it said one first.
    #[test]
    fn an_orchestrators_channel_that_breaks_ends_its_run_failed_saying_why() {
        // What a VM killed for its asks leaves its run host: no `DONE`.
        let killed = run_end(None, None, true);
        let ends = |read: io::Result<Option<(u8, Vec<u8>)>>| {
            let asks = read_ask(
                read.as_ref()
                    .map(|f| f.as_ref().map(|(kind, p)| (*kind, p.as_slice()))),
            )
            .err()
            .unwrap();
            assert_eq!(
                ended_by(Some(&asks), wire::RunEnd::Exited(0), true),
                wire::RunEnd::Exited(0),
                "{asks:?} after the VM's own end"
            );
            ended_by(Some(&asks), killed.clone(), false)
        };
        let frame = |which: u8, payload: &[u8]| {
            let bytes = wire::frame(which, payload).unwrap();
            wire::read_frame(&mut bytes.as_slice())
        };
        assert_eq!(ends(frame(wire::to_host::STOP, &[])), wire::RunEnd::Stopped);
        assert_eq!(
            ends(Ok(None)),
            wire::RunEnd::Failed("its orchestrator's channel ended without STOP".into())
        );
        let cut = wire::read_frame(&mut &[wire::to_host::CREDIT, 0, 0][..]);
        let why = cut.as_ref().err().map(ToString::to_string).unwrap();
        assert_eq!(
            ends(cut),
            wire::RunEnd::Failed(format!("its orchestrator's channel broke: {why}"))
        );
        let malformed = wire::Credit::decode(&[1]).err().unwrap();
        assert_eq!(
            ends(frame(wire::to_host::CREDIT, &[1])),
            wire::RunEnd::Failed(format!(
                "its orchestrator's channel broke: a malformed CREDIT: {malformed}"
            ))
        );
        assert_eq!(
            ends(frame(wire::to_host::CREATE, &[])),
            wire::RunEnd::Failed(format!(
                "its orchestrator's channel broke: a frame of kind {}, which is no ask",
                wire::to_host::CREATE
            ))
        );
    }

    /// A run that keeps nothing past the microVM of what the image grants its members has
    /// the network their links need all the same (their init links each member granted
    /// anything past the microVM through eth0, D59, D250), and its network process lets
    /// nothing past (no policy told: `Policy::DenyAll`). Do: narrow an image whose harness is
    /// granted egress and DNS to nothing, as Hex's image class asks by default (kiln plan
    /// 01a1cd8e862e7169a074db3d5fa33f3d step 12); keep it whole; and run an image that
    /// grants nothing. Expect: the first on a bridge with no policy, never refused "cannot
    /// run without a network"; the second on a bridge with the image's policy; the third on
    /// none.
    #[test]
    fn a_run_keeping_nothing_past_the_microvm_has_its_members_network_and_no_policy() {
        let granted = crate::agentfile::Agentfile {
            domains: 2,
            uplink: true,
            grants: shards_agentfile::agentfile::Grants {
                egress: vec![b"443/tcp".to_vec()],
                dns: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut nothing = request(Vec::new());
        nothing.egress = wire::Narrowing {
            egress: wire::Keep::Only(Vec::new()),
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(Vec::new()),
            dns: false,
            members: None,
        };
        let mut run = hosted::as_run(&nothing, None);
        let policy = network(Some(&granted), &nothing, &mut run);
        assert!(
            matches!(&policy, Ok(None)),
            "{}",
            policy.err().map(|No(_, why)| why).unwrap_or_default()
        );
        assert_eq!(run.network, "bridge");
        let whole = request(Vec::new());
        let mut run = hosted::as_run(&whole, None);
        assert!(matches!(network(Some(&granted), &whole, &mut run), Ok(Some(_))));
        assert_eq!(run.network, "bridge");
        let plain = crate::agentfile::Agentfile {
            domains: 2,
            ..Default::default()
        };
        let mut run = hosted::as_run(&whole, None);
        assert!(matches!(network(Some(&plain), &whole, &mut run), Ok(None)));
        assert_eq!(run.network, "none");
    }

    /// network-05: an orchestrator's run on the default bridge, whose image's agents have IPv6
    /// and are granted a port past the microVM, as a daemon's run there (D99). Do: shape the
    /// network of that image run whole, of one without IPv6, and of one granting nothing past.
    /// Expect: the first's eth0 given the home's IPv6 for its agents, the address a daemon's
    /// run on the bridge has (`uplink6=`, `NET_ADDRESS`); the others none.
    #[test]
    fn an_orchestrated_runs_agents_reach_past_the_microvm_by_ipv6_as_a_daemons_do() {
        let home = shards_testdir::TempDir::new("api-past6").unwrap();
        let agentfile = |ipv6: bool, egress: Vec<Vec<u8>>| crate::agentfile::Agentfile {
            domains: 1,
            uplink: !egress.is_empty(),
            ipv6,
            grants: shards_agentfile::agentfile::Grants {
                egress,
                ..Default::default()
            },
            ..Default::default()
        };
        let whole = request(Vec::new());
        let six = agentfile(true, vec![b"443/tcp".to_vec()]);
        let mut run = hosted::as_run(&whole, None);
        network(Some(&six), &whole, &mut run).ok().unwrap();
        assert_eq!(run.network, "bridge");
        let v6 = past6(Some(&six), &run, home.path());
        assert_eq!(v6, Some(crate::networks::host_ipv6(home.path())));
        let (guest6, gateway6) = v6.unwrap();
        assert_eq!(
            crate::networks::uplink6_entry((guest6, gateway6)),
            format!("uplink6={guest6}/64,{gateway6}").into_bytes()
        );
        let four = agentfile(false, vec![b"443/tcp".to_vec()]);
        let mut run = hosted::as_run(&whole, None);
        network(Some(&four), &whole, &mut run).ok().unwrap();
        assert_eq!(past6(Some(&four), &run, home.path()), None);
        let nothing = agentfile(true, Vec::new());
        let mut run = hosted::as_run(&whole, None);
        network(Some(&nothing), &whole, &mut run).ok().unwrap();
        assert_eq!(past6(Some(&nothing), &run, home.path()), None);
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

    /// Each share as the run's `-v` asks it (D279): its mode `ro` where read-only and `noexec`
    /// where its files may not be run, both, or neither.
    #[test]
    fn a_shares_bind_says_read_only_and_noexec() {
        let mut r = request(Vec::new());
        let share = |read_only, exec| wire::ShareAt {
            host: "/h".into(),
            at: "/w".into(),
            read_only,
            members: None,
            exec,
        };
        r.shares = vec![
            share(false, true),
            share(true, true),
            share(false, false),
            share(true, false),
        ];
        assert_eq!(
            hosted::as_run(&r, None).binds,
            ["/h:/w", "/h:/w:ro", "/h:/w:noexec", "/h:/w:ro,noexec"]
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
            credentials: Vec::new(),
            control: None,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
            upstream_roots: Vec::new(),
            plug_roots: Vec::new(),
            egress_ceiling: Vec::new(),
            gpu_report: None,
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
            workdir: Some("/Users/ada/proj".into()),
            input: false,
            terminal: false,
            args: Vec::new(),
            mcp: false,
        }
    }

    /// A terminal of its own (D244) goes to a member the image runs, by its index, each
    /// asked once; one the image does not declare, one of files alone, or one asked twice
    /// is refused by name, and none asked is none.
    #[test]
    fn a_terminal_is_a_running_members_once() {
        let with_terminal = |kind, name: &str| wire::MemberStage {
            terminal: true,
            ..stage(kind, name)
        };
        assert!(matches!(
            terminals(&request(vec![stage(wire::MemberKind::Harness, "codex")]), &members()),
            Ok(none) if none.is_empty()
        ));
        assert!(matches!(
            terminals(
                &request(vec![with_terminal(wire::MemberKind::Harness, "codex")]),
                &members()
            ),
            Ok(asked) if asked == [1]
        ));
        for (asked, cause) in [
            (
                vec![with_terminal(wire::MemberKind::Harness, "nosuch")],
                Cause::Image,
            ),
            (
                vec![with_terminal(wire::MemberKind::Agent, "helper")],
                Cause::Image,
            ),
            (
                vec![
                    with_terminal(wire::MemberKind::Harness, "codex"),
                    with_terminal(wire::MemberKind::Harness, "codex"),
                ],
                Cause::Request,
            ),
        ] {
            assert!(
                matches!(terminals(&request(asked.clone()), &members()), Err(No(c, _)) if c == cause),
                "{asked:?}"
            );
        }
    }

    /// An MCP bridge (D254) goes to a member the image runs, by its index, each asked once, and
    /// only in a run whose request asks its agent-control endpoint, which alone carries its
    /// connections; one the image does not declare, one of files alone, one asked twice, or one
    /// in a run with no endpoint is refused by name, and none asked is none.
    #[test]
    fn a_bridge_is_a_running_members_once_and_needs_the_runs_control() {
        let bridged = |kind, name: &str| wire::MemberStage {
            mcp: true,
            ..stage(kind, name)
        };
        let controlled = |members| wire::Request {
            control: Some(wire::ControlAsk {
                controller: [7; 32],
                sockets: None,
                stdio: false,
            }),
            ..request(members)
        };
        assert!(matches!(
            mcps(&request(vec![stage(wire::MemberKind::Harness, "codex")]), &members()),
            Ok(none) if none.is_empty()
        ));
        assert!(matches!(
            mcps(&controlled(vec![bridged(wire::MemberKind::Harness, "codex")]), &members()),
            Ok(asked) if asked == [1]
        ));
        for (asked, cause) in [
            (
                request(vec![bridged(wire::MemberKind::Harness, "codex")]),
                Cause::Request,
            ),
            (
                controlled(vec![bridged(wire::MemberKind::Harness, "nosuch")]),
                Cause::Image,
            ),
            (
                controlled(vec![bridged(wire::MemberKind::Agent, "helper")]),
                Cause::Image,
            ),
            (
                controlled(vec![
                    bridged(wire::MemberKind::Harness, "codex"),
                    bridged(wire::MemberKind::Harness, "codex"),
                ]),
                Cause::Request,
            ),
        ] {
            assert!(
                matches!(mcps(&asked, &members()), Err(No(c, _)) if c == cause),
                "{asked:?}"
            );
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
        assert_eq!(entries.len(), 3);
        assert!(entries[0].starts_with(b"member-env=1\0K=v"));
        assert_eq!(entries[2], b"member-workdir=1\0/Users/ada/proj");
        // A relative or escaping working directory is refused, and so is a second one for
        // the same member, as the guest refuses them.
        for workdir in ["proj", "/Users/ada/../../etc"] {
            let mut s = stage(wire::MemberKind::Harness, "codex");
            s.workdir = Some(workdir.into());
            assert!(
                matches!(staged(&request(vec![s]), &members()), Err(No(Cause::Request, _))),
                "{workdir}"
            );
        }
        let twice = vec![
            stage(wire::MemberKind::Harness, "codex"),
            stage(wire::MemberKind::Harness, "codex"),
        ];
        assert!(matches!(
            staged(&request(twice), &members()),
            Err(No(Cause::Request, _))
        ));
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

    /// D277: a member its guest could not start in its working directory is refused its
    /// orchestrator typed `Workdir`, naming the member as the image declares it, the directory
    /// and the errno, as its guest told them; one of an index the image declares not, by it.
    #[test]
    fn a_member_its_guest_could_not_start_in_its_workdir_is_refused_typed() {
        let told = |member| shards_abi::run::members::Unentered {
            member,
            errno: 2,
            workdir: b"/Users/ada/proj".to_vec(),
        };
        let refused = workdir_refused(&members(), &told(1));
        assert_eq!(refused.cause, Cause::Workdir);
        assert_eq!(
            refused.why,
            "Harness codex: /Users/ada/proj: errno 2: its guest could not enter it as it started"
        );
        let stranger = workdir_refused(&members(), &told(9));
        assert_eq!(stranger.cause, Cause::Workdir);
        assert!(
            stranger.why.starts_with("member 9: /Users/ada/proj: errno 2"),
            "{}",
            stranger.why
        );
    }

    /// A member's run-time arguments (D256) are staged after its other entries, one
    /// `member-arg=` each by its index, in their order, an empty one among them, and read back
    /// so by the guest's reader; none given, none staged. One holding a NUL, or arguments in
    /// two of one member's stages, which have no order between them, refuse the request.
    #[test]
    fn a_members_arguments_are_staged_after_its_own_in_order() {
        let none = staged(
            &request(vec![stage(wire::MemberKind::Harness, "codex")]),
            &members(),
        )
        .ok()
        .unwrap();
        assert_eq!(none.len(), 3);
        let mut s = stage(wire::MemberKind::Harness, "codex");
        s.args = vec!["--resume".into(), "s-1".into(), String::new()];
        let entries = staged(&request(vec![s.clone()]), &members()).ok().unwrap();
        assert_eq!(entries[..3], none[..]);
        assert_eq!(
            entries[3..],
            [
                b"member-arg=1\0--resume".to_vec(),
                b"member-arg=1\0s-1".to_vec(),
                b"member-arg=1\0".to_vec(),
            ]
        );
        assert_eq!(
            shards_abi::run::members::args_of(&entries, 1).unwrap(),
            [&b"--resume"[..], b"s-1", b""]
        );
        // Each of a key the run protocol registers (D257), as the guest refuses any other.
        assert_eq!(shards_abi::run::setup::check(&entries), Ok(()));
        let mut nul = s.clone();
        nul.args.push("a\0b".into());
        assert!(matches!(
            staged(&request(vec![nul]), &members()),
            Err(No(Cause::Request, _))
        ));
        let mut second = s.clone();
        second.workdir = None;
        assert!(matches!(
            staged(&request(vec![s.clone(), second.clone()]), &members()),
            Err(No(Cause::Request, why)) if why.contains("its arguments given twice")
        ));
        second.args.clear();
        assert!(staged(&request(vec![s, second]), &members()).is_ok());
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
            members: None,
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
            members: None,
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

    /// The run's credential grants reach its network process by reference alone, each
    /// toward a host its egress grants reach (D242, D236); a grant toward a host past them,
    /// or toward none, could never be filled and is refused before anything starts, and a
    /// run with none sends none (R10: each refusal's test fails without it).
    #[test]
    fn credential_grants_reach_the_network_process_by_reference() {
        let grant = |reference: &str, toward: &[&str]| wire::CredentialGrant {
            reference: reference.into(),
            toward: toward.iter().map(|h| (*h).to_string()).collect(),
            placeholder: format!("kiln-placeholder-{}", reference.replace('/', "-")),
        };
        let named = shards_net::encode_policy(
            &shards_net::Ports::default(),
            &[("api.anthropic.com".into(), 443)],
            false,
        );
        assert_eq!(credentials(&[], &[], Some(&named), false).ok().unwrap(), None);
        let sent = credentials(
            &[grant("anthropic/claude-max", &["api.anthropic.com"])],
            &[],
            Some(&named),
            false,
        )
        .ok()
        .unwrap()
        .unwrap();
        assert_eq!(
            shards_net::decode_credentials(&sent).map(|(_, grants)| grants),
            Some(vec![shards_net::Grant {
                reference: "anthropic/claude-max".into(),
                toward: vec!["api.anthropic.com".into()],
                placeholder: "kiln-placeholder-anthropic-claude-max".into(),
            }])
        );
        // A placeholder that is no token, and one shared by two credentials, as an
        // orchestrator's side that checked nothing could send them.
        let mut untoken = grant("anthropic/claude-max", &["api.anthropic.com"]);
        untoken.placeholder = "two words".into();
        let mut shared = grant("anthropic/console", &["api.anthropic.com"]);
        shared.placeholder = "kiln-placeholder-anthropic-claude-max".into();
        for refused in [
            credentials(
                &[grant("openai/chatgpt", &["chatgpt.com"])],
                &[],
                Some(&named),
                false,
            ),
            credentials(&[grant("anthropic/claude-max", &[])], &[], Some(&named), false),
            credentials(
                &[grant("anthropic/claude-max", &["api.anthropic.com"])],
                &[],
                None,
                false,
            ),
            credentials(&[untoken], &[], Some(&named), false),
            credentials(
                &[grant("anthropic/claude-max", &["api.anthropic.com"]), shared],
                &[],
                Some(&named),
                false,
            ),
            // An upstream root past what the network process's wire carries (D242).
            credentials(
                &[grant("anthropic/claude-max", &["api.anthropic.com"])],
                &[vec![0x30; usize::from(u16::MAX) + 1]],
                Some(&named),
                false,
            ),
        ] {
            assert!(matches!(refused, Err(No(Cause::Request, _))));
        }
        // A TCP port granted reaches every host on it.
        let any443 = shards_net::encode_policy(&shards_net::Ports::parse("443/tcp").unwrap(), &[], false);
        assert!(
            credentials(
                &[grant("openai/chatgpt", &["chatgpt.com"])],
                &[],
                Some(&any443),
                false
            )
            .is_ok()
        );
    }

    /// The run's CA goes to its guest for its members' trust (D242): its certificate in PEM,
    /// its base64 in lines of 64 characters but the last (RFC 7468 §2), read back to the DER
    /// by rustls' own PEM reader; and each host the grants name, once. Malformed grants
    /// give nothing.
    #[test]
    fn credential_grants_ca_goes_to_the_guest_in_pem_with_each_host_once() {
        let der: Vec<u8> = (0..=255u8).cycle().take(700).collect();
        let grants = shards_net::encode_credentials(
            &[],
            &[
                shards_net::Grant {
                    reference: "anthropic/claude-max".into(),
                    toward: vec!["api.anthropic.com".into(), "claude.ai".into()],
                    placeholder: "kiln-placeholder-0a".into(),
                },
                shards_net::Grant {
                    reference: "anthropic/console".into(),
                    toward: vec!["api.anthropic.com".into()],
                    placeholder: "kiln-placeholder-1b".into(),
                },
            ],
        )
        .unwrap();
        let entries = trust(&der, &grants, &[]).unwrap();
        // Each of a key the run protocol registers (D257), as the guest refuses any other.
        assert_eq!(shards_abi::run::setup::check(&entries), Ok(()));
        let got = shards_abi::run::trust::of(&entries).unwrap().unwrap();
        assert_eq!(got.hosts, [b"api.anthropic.com".as_slice(), b"claude.ai"]);
        let pem = std::str::from_utf8(got.pem).unwrap();
        let lines: Vec<&str> = pem.lines().collect();
        assert_eq!(lines.first(), Some(&"-----BEGIN CERTIFICATE-----"));
        assert_eq!(lines.last(), Some(&"-----END CERTIFICATE-----"));
        let body = &lines[1..lines.len() - 1];
        assert!(
            body[..body.len() - 1].iter().all(|l| l.len() == PEM_LINE),
            "{body:?}"
        );
        assert!((1..=PEM_LINE).contains(&body[body.len() - 1].len()));
        use rustls::pki_types::pem::PemObject as _;
        let back = rustls::pki_types::CertificateDer::from_pem_slice(got.pem).unwrap();
        assert_eq!(back.as_ref(), der.as_slice());
        assert!(trust(&der, b"\x00", &[]).is_err());
    }

    /// Each member held to its reach of the names kept (D246). Do: keep a destination that
    /// harness codex and agent helper both reach by name, the host itself that codex alone
    /// reaches, and a remote MCP server helper alone reaches, and hold each member to what
    /// the request lets it reach. Expect: a request letting each reach exactly its own passes;
    /// one letting codex alone reach the shared destination, which helper would reach too,
    /// letting codex nothing, letting helper more than it would reach, or leaving out the MCP
    /// server helper would reach, is refused as a widening naming the member and both
    /// reaches; an entry for a member the image declares not is the image's refusal, a
    /// member's second entry the request's; and a request holding no member apart, or keeping
    /// nothing, passes.
    #[test]
    fn each_member_is_held_to_its_reach_of_the_names_kept() {
        use crate::agentfile::{Agentfile, NamedReach};
        use shards_agentfile::agentfile::TargetKind;
        let agentfile = Agentfile {
            grants: shards_agentfile::agentfile::Grants {
                mcp: vec![b"mcp.example:9443".to_vec()],
                destinations: vec![b"api.example:443".to_vec(), b"host.docker.internal:7070".to_vec()],
                ..Default::default()
            },
            named: vec![
                NamedReach {
                    kind: TargetKind::Harness,
                    name: "codex".into(),
                    reaches: vec!["api.example:443".into(), "host.docker.internal:7070".into()],
                },
                NamedReach {
                    kind: TargetKind::Agent,
                    name: "helper".into(),
                    reaches: vec!["api.example:443".into(), "mcp.example:9443".into()],
                },
            ],
            ..Agentfile::default()
        };
        let reach = |kind, name: &str, destinations: &[&str]| wire::MemberReach {
            kind,
            name: name.into(),
            destinations: destinations.iter().map(|d| (*d).to_string()).collect(),
        };
        let held = |members: Option<Vec<wire::MemberReach>>| wire::Narrowing {
            egress: wire::Keep::Only(Vec::new()),
            mcp: wire::Keep::Only(vec!["mcp.example:9443".into()]),
            destinations: wire::Keep::Only(vec![
                "api.example:443".into(),
                "host.docker.internal:7070".into(),
            ]),
            dns: false,
            members,
        };
        let (harness, agent) = (wire::MemberKind::Harness, wire::MemberKind::Agent);
        let codex_whole = || {
            reach(
                harness,
                "codex",
                &["host.docker.internal:7070", "api.example:443"],
            )
        };
        let helper_whole = || reach(agent, "helper", &["mcp.example:9443", "api.example:443"]);
        assert!(held_apart(Some(&agentfile), &held(Some(vec![codex_whole(), helper_whole()]))).is_ok());
        for (members, says) in [
            (
                vec![codex_whole()],
                "agent helper: of the names kept it would reach api.example:443, mcp.example:9443, \
                 where the request lets it reach nothing",
            ),
            (
                vec![reach(harness, "codex", &[]), helper_whole()],
                "harness codex: of the names kept it would reach api.example:443, \
                 host.docker.internal:7070, where the request lets it reach nothing",
            ),
            (
                vec![
                    codex_whole(),
                    reach(
                        agent,
                        "helper",
                        &["api.example:443", "host.docker.internal:7070", "mcp.example:9443"],
                    ),
                ],
                "agent helper: of the names kept it would reach api.example:443, mcp.example:9443, \
                 where the request lets it reach api.example:443, host.docker.internal:7070, mcp.example:9443",
            ),
            (
                vec![codex_whole(), reach(agent, "helper", &["api.example:443"])],
                "agent helper: of the names kept it would reach api.example:443, mcp.example:9443, \
                 where the request lets it reach api.example:443",
            ),
        ] {
            match held_apart(Some(&agentfile), &held(Some(members))) {
                Err(No(Cause::Widening, why)) => assert_eq!(why, format!("{says} (D246)")),
                other => panic!("{says}: {:?}", other.map_err(|No(c, w)| (c, w))),
            }
        }
        assert!(matches!(
            held_apart(Some(&agentfile), &held(Some(vec![reach(harness, "nobody", &[])]))),
            Err(No(Cause::Image, _))
        ));
        assert!(matches!(
            held_apart(
                Some(&agentfile),
                &held(Some(vec![codex_whole(), helper_whole(), helper_whole()]))
            ),
            Err(No(Cause::Request, _))
        ));
        assert!(held_apart(Some(&agentfile), &held(None)).is_ok());
        let nothing_kept = wire::Narrowing {
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(Vec::new()),
            ..held(Some(Vec::new()))
        };
        assert!(held_apart(Some(&agentfile), &nothing_kept).is_ok());
        // An image with no Agentfile declares no member to hold: none named passes, any
        // named is the image's refusal.
        let bare = wire::Narrowing {
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(Vec::new()),
            ..held(Some(Vec::new()))
        };
        assert!(held_apart(None, &bare).is_ok());
        let named = wire::Narrowing {
            members: Some(vec![codex_whole()]),
            ..bare
        };
        assert!(matches!(held_apart(None, &named), Err(No(Cause::Image, _))));
    }

    /// A member granted a port to any address is held to the names kept on that port too
    /// (D246, inithex-02): its init lets it out there to any address and the network process
    /// admits a kept name's addresses for any member, so it reaches the name whatever its
    /// entry says. Do: derive the run host's Agentfile from one whose agent a reaches a
    /// provider by name, and a and b are both granted its port to any address; ask that
    /// port's grant dropped, the provider kept, and a alone let it. Expect: refused as a
    /// widening naming b; a run that lets both reach it passes.
    #[test]
    fn a_member_granted_a_names_port_is_held_to_that_name() {
        use crate::agentfile::{Agentfile, NamedReach};
        use shards_agentfile::{agentfile as af, instructions, lint::Linter, parser};
        let text = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n\
            NETWORK --egress=api.example:443 named\nEXPOSE 443 AS egress FOR named\n\
            CONNECT a WITH a ON named\n\
            NETWORK --egress=443 wide\nEXPOSE 443 AS egress FOR wide\n\
            CONNECT a WITH a ON wide\nCONNECT b WITH b ON wide\n";
        let parsed = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile).unwrap();
        let stages = instructions::parse(&parsed, &Linter::default()).unwrap().stages;
        let directives: Vec<af::Directive> = stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                instructions::Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let directives = af::resolve_targets(&directives).unwrap();
        af::held(&directives).unwrap();
        // As the run host reads it (`agentfile::agentfile`).
        let agentfile = Agentfile {
            grants: af::grants(&directives),
            named: af::named_reach(&directives)
                .into_iter()
                .map(|(kind, name, reaches)| NamedReach {
                    kind,
                    name: String::from_utf8(name).unwrap(),
                    reaches: reaches
                        .iter()
                        .map(|(h, p)| format!("{}:{p}", String::from_utf8_lossy(h)))
                        .collect(),
                })
                .collect(),
            ..Agentfile::default()
        };
        let reach = |name: &str, destinations: &[&str]| wire::MemberReach {
            kind: wire::MemberKind::Agent,
            name: name.into(),
            destinations: destinations.iter().map(|d| (*d).to_string()).collect(),
        };
        let held = |members| wire::Narrowing {
            egress: wire::Keep::Only(Vec::new()),
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(vec!["api.example:443".into()]),
            dns: false,
            members: Some(members),
        };
        let provider = &["api.example:443"][..];
        match held_apart(
            Some(&agentfile),
            &held(vec![reach("a", provider), reach("b", &[])]),
        ) {
            Err(No(Cause::Widening, why)) => assert_eq!(
                why,
                "agent b: of the names kept it would reach api.example:443, where the request lets it reach nothing (D246)"
            ),
            other => panic!("{:?}", other.map_err(|No(c, w)| (c, w))),
        }
        assert!(
            held_apart(
                Some(&agentfile),
                &held(vec![reach("a", provider), reach("b", provider)])
            )
            .is_ok()
        );
    }

    /// The run's request is one a warm VM takes: its log keeps something by the VM's own
    /// check (warm.rs `head`), though it writes none, its head says whether it publishes
    /// ports, and it carries the most its guest may plug as given, none included.
    #[test]
    fn its_request_is_one_a_warm_vm_takes() {
        for (published, timing) in [(false, false), (true, false), (false, true), (true, true)] {
            for plug in [0, 1 << 20, u64::MAX] {
                let payload = run_request(&shards_abi::run::Spec::default(), plug, published, timing);
                let (head, spec) = shards_ipc::RunHead::decode(&payload).unwrap();
                assert!(head.keeps_its_log(), "{head:?}");
                assert_eq!(head.flags & shards_ipc::RUN_PUBLISHED != 0, published);
                assert_eq!(head.flags & shards_ipc::RUN_TIMING != 0, timing);
                assert_eq!(head.flags & shards_ipc::RUN_LOG, 0);
                assert_eq!(head.plug, plug);
                assert!(shards_abi::run::Spec::decode(spec).is_some());
            }
        }
    }

    /// An orchestrator's run may plug what a daemon's run of the same size may (D148): its
    /// most, all its host's memory where it names no limit, less what it boots with, the
    /// least envelope's 64 MiB or any other; none where it boots with all of it. The
    /// orchestrator's run had none: kiln's harnesses at 64 MiB were ended at what was free as
    /// their members started (D275).
    #[test]
    fn an_orchestrators_run_may_plug_its_most_less_its_boot() {
        let home = shards_testdir::TempDir::new("plug-most").unwrap();
        let rootfs = home.path().join("root.erofs");
        let guest = crate::guest::Guest {
            kernel: home.path().join("vmlinux"),
            init: home.path().join("init"),
            kernel_digest: "sha256:k".into(),
            init_digest: "sha256:i".into(),
        };
        let mut run = prepared(&rootfs, crate::run::Boot::Stored(guest), None);
        for (boot, most, plug) in [
            (64, 36_864, (36_864 - 64) << 20),
            (128, 8192, (8192 - 128) << 20),
            (256, 256, 0),
            (512, 256, 0),
        ] {
            (run.size.1, run.memory_max) = (boot, most);
            assert_eq!(run.plug_bytes(), plug, "{boot} MiB booted of {most}");
            let payload = run_request(&run.spec, run.plug_bytes(), false, false);
            assert_eq!(shards_ipc::RunHead::decode(&payload).unwrap().0.plug, plug);
        }
        run.memory_max = u64::MAX;
        run.size.1 = 0;
        assert_eq!(run.plug_bytes(), u64::MAX, "saturates, never wraps");
    }

    /// A line says its time only where its lines are timed, as `+US us` after the host's name.
    #[test]
    fn a_line_says_its_time_only_where_its_lines_are_timed() {
        assert_eq!(
            said(None, format_args!("its VM said READY")),
            "shards api-run: its VM said READY\n"
        );
        assert_eq!(
            said(Some(1234), format_args!("its VM said READY")),
            "shards api-run: +1234 us its VM said READY\n"
        );
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
                fetch: Default::default(),
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
            let (answer, payload) = relayed(kind::WORKLOAD_READY, &said, |_| (0, false)).unwrap();
            assert_eq!(answer, wire::to_client::READY);
            assert_eq!(payload, said);
            assert_eq!(wire::ready(&payload), Ok(ready));
        }
        for malformed in [&[0u8; 9][..], &[1, 0, 0, 0, 0, 0, 0, 0, 1], &[2; 8], &[]] {
            assert_eq!(
                relayed(kind::WORKLOAD_READY, malformed, |_| (0, false)),
                None,
                "{malformed:?}"
            );
        }
        for end in [
            shards_abi::run::members::End::Exited(143),
            shards_abi::run::members::End::Signaled(15),
        ] {
            let ended = shards_abi::run::members::Ended { member: 1, end }.encode();
            let (answer, payload) = relayed(kind::MEMBER_ENDED, &ended, |_| (0, false)).unwrap();
            assert_eq!(answer, wire::to_client::MEMBER_ENDED);
            assert_eq!(
                wire::MemberEnded::decode(&payload),
                Ok(wire::MemberEnded {
                    member: 1,
                    generation: 0,
                    end,
                    killed: false,
                })
            );
        }
        // A generation its restart sent a kill: killed where its end is the kill's, SIGKILL,
        // and not where it ended of itself as the kill went (D270 slice 3b).
        for (end, killed) in [
            (shards_abi::run::members::End::Signaled(9), true),
            (shards_abi::run::members::End::Exited(0), false),
            (shards_abi::run::members::End::Signaled(15), false),
        ] {
            let ended = shards_abi::run::members::Ended { member: 1, end }.encode();
            let (_, payload) = relayed(kind::MEMBER_ENDED, &ended, |_| (3, true)).unwrap();
            let told = wire::MemberEnded::decode(&payload).unwrap();
            assert_eq!((told.generation, told.end, told.killed), (3, end, killed));
        }
        for other in [kind::READY, kind::PROCESS, kind::DONE] {
            assert_eq!(relayed(other, &[1; 9], |_| (0, false)), None);
        }
    }

    /// What its VM says it holds a run's vCPUs to reads as it was asked (D247): the rate it
    /// was asked where it holds them, nothing where it lets them be; another rate, no rate
    /// or a word that is no number is a broken word from its VM, never a hold.
    #[test]
    fn a_rates_answer_reads_as_it_was_asked() {
        let rate = std::num::NonZeroU64::new;
        let held = |milli, period| {
            Ok(wire::Rated {
                milli: rate(milli),
                period,
            })
        };
        let p = Duration::from_micros(2_667);
        assert_eq!(rated_of(rate(1500), "RATED 1500 2667000\n"), held(1500, p));
        assert_eq!(
            rated_of(rate(4000), "RATED 0 1050000\n"),
            held(0, Duration::from_micros(1_050)),
            "every vCPU's whole CPU: no hold, over g"
        );
        assert_eq!(
            rated_of(None, "RATED 0 1000000\n"),
            held(0, Duration::from_millis(1))
        );
        assert_eq!(
            rated_of(rate(1500), "REFUSED its hold's period: outgrown\n"),
            Err("its hold's period: outgrown".into())
        );
        for (asked, said) in [
            (rate(1500), "RATED 1000 2667000\n"),
            (None, "RATED 1500 2667000\n"),
            (rate(1500), "RATED 1500 0\n"),
            (rate(1500), "RATED 1500\n"),
            (rate(1500), "1500"),
            (rate(1500), ""),
        ] {
            assert!(rated_of(asked, said).is_err(), "{asked:?} {said:?}");
        }
    }

    /// What init says it held a run within reads as it was asked (D241): a number where it
    /// was told a room, nothing where it was told none; anything else is a broken word from
    /// its init, never a hold.
    #[test]
    fn a_rooms_answer_reads_as_it_was_asked() {
        assert_eq!(held_of(Some(64 << 20), b"67108864"), Ok(Some(64 << 20)));
        assert_eq!(held_of(Some(-1), b"3407872\n"), Ok(Some(3_407_872)));
        assert_eq!(held_of(None, b""), Ok(None));
        for (room, said) in [
            (Some(1), &b""[..]),
            (Some(1), b"-5"),
            (Some(1), b"5 bytes"),
            (None, b"5"),
            (Some(1), &[0xff]),
        ] {
            assert!(held_of(room, said).is_err(), "{room:?} {said:?}");
        }
    }

    const CLAUDE: &str = "anthropic/claude-max";
    const CHATGPT: &str = "openai/chatgpt";
    const VALUE: &str = "sk-ant-lease-test-value";

    /// The run's ledger of leases (D242, R04). Do: ask a credential no grant names; ask a
    /// granted one, then again while it is asked and while it is held; ask it once its lease
    /// has ended, refuse that, and answer it again; answer a lease never asked; hold one and
    /// ask another; release the run's leases, twice; then ask, and answer the other. Expect:
    /// the ungranted refused, nothing asked; one lease of a credential at a time; the ended
    /// lease released by the next ask, asked anew; the refused one gone, its second answer
    /// and the stray one refused; the release counting the held and the asked, once; and after
    /// it, asks refused as the run ended, the answer late.
    #[test]
    fn a_runs_ledger_holds_one_lease_of_a_credential_and_releases_them_all() {
        let mut ledger = Ledger::new(vec![CLAUDE.into(), CHATGPT.into()], Vec::new());
        let now = Instant::now();
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        let told = |asked_as| Answered::Told {
            asked_as,
            reference: CLAUDE.into(),
        };
        assert_eq!(ledger.ask(1, "github/token", now), Err(Refusal::Ungranted));
        let first = ledger.ask(1, CLAUDE, now).unwrap();
        assert_eq!(
            ledger.ask(2, CLAUDE, now),
            Err(Refusal::Live),
            "asked while asked"
        );
        assert_eq!(ledger.answer(first, Some(life), now), Ok(told(1)));
        assert_eq!(ledger.ask(3, CLAUDE, now), Err(Refusal::Live), "asked while held");
        let ended = now + Duration::from_secs(60);
        let second = ledger.ask(4, CLAUDE, ended).unwrap();
        assert_ne!(second, first, "a new lease, a new id");
        assert_eq!(ledger.answer(second, None, ended), Ok(told(4)));
        assert!(
            ledger.answer(second, Some(life), ended).is_err(),
            "a refused lease answered"
        );
        assert!(
            ledger.answer(u32::MAX, Some(life), ended).is_err(),
            "no ask answered"
        );
        let held = ledger.ask(5, CLAUDE, ended).unwrap();
        assert_eq!(ledger.answer(held, Some(life), ended), Ok(told(5)));
        let asked = ledger.ask(6, CHATGPT, ended).unwrap();
        assert_eq!(ledger.release(), Some((1, 1)));
        assert_eq!(ledger.release(), None, "released twice");
        assert_eq!(ledger.ask(7, CHATGPT, ended), Err(Refusal::Ended));
        assert_eq!(ledger.answer(asked, Some(life), ended), Ok(Answered::Late));
    }

    /// A run with a grant, its lease relayed just in time between its network process (its
    /// lease client, shards_net::lease) and a simulated orchestrator, over a socket and pipes as
    /// the run host has them (D242). Do, for a run ending as its command exits, as it is
    /// stopped or its owner's channel closes, and as its VM fails: ask the credential on its
    /// first use; answer it as the orchestrator, as the run host's asks take `LEASED`; end the
    /// run; ask again; end the network process. Expect: no value before the first use; the
    /// orchestrator asked by reference; the value held by the network process; the run's
    /// release told before its end, every time; the later ask refused as ended, nothing asked
    /// of the orchestrator; the relay ended with the network process's control.
    #[cfg(unix)]
    #[test]
    fn a_granted_runs_lease_is_relayed_on_first_use_and_released_as_it_ends() {
        use std::io::Read as _;
        use std::os::unix::net::UnixStream;
        let soon = || Instant::now() + Duration::from_secs(120);
        for end in [
            wire::RunEnd::Exited(0),
            wire::RunEnd::Stopped,
            wire::RunEnd::Failed("its VM ended".into()),
        ]
        .map(|end| wire::RunEnded::unused(end, wire::Unused::Untold))
        {
            let (net, run_host) = UnixStream::pair().unwrap();
            let (mut orchestrator, out) = std::io::pipe().unwrap();
            let answers = Answers(Mutex::new(Box::new(out)));
            let control = NetControl::new(&run_host);
            let leasing = Leasing {
                ledger: Mutex::new(Ledger::new(vec![CLAUDE.into()], Vec::new())),
                net: &control,
            };
            std::thread::scope(|s| {
                let relaying = s.spawn(|| relay(&control, Some(&leasing), &answers));
                let mut leases = shards_net::lease::Leases::new(vec![CLAUDE.into()]);
                assert_eq!(
                    leases.held(CLAUDE, Instant::now()),
                    None,
                    "a value before its use"
                );
                assert!(matches!(
                    leases.ask(CLAUDE, 1, Instant::now()),
                    shards_net::lease::Asked::Waiting
                ));
                for ask in leases.asks() {
                    shards_ipc::send(&net, kind::NET_LEASE, &ask, &[]).unwrap();
                }
                let (which, payload) = wire::read_frame(&mut orchestrator).unwrap().unwrap();
                assert_eq!(which, wire::to_client::LEASE);
                let ask = wire::LeaseAsk::decode(&payload).unwrap();
                assert_eq!(ask.reference, CLAUDE);
                let answer = wire::LeaseAnswer {
                    id: ask.id,
                    result: Ok(wire::Lease {
                        value: wire::Secret::from(VALUE),
                        lives: NonZeroU64::new(3_600_000_000_000).unwrap(),
                    }),
                };
                leased(&leasing, &answer.encode().unwrap()).unwrap();
                let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
                assert_eq!(m.kind, kind::NET_LEASE);
                assert!(leases.answered(&m.payload, Instant::now()));
                assert_eq!(leases.held(CLAUDE, Instant::now()), Some(VALUE.as_bytes()));
                end_run(&answers, Some(&leasing), &end);
                let (which, payload) = wire::read_frame(&mut orchestrator).unwrap().unwrap();
                assert_eq!(
                    (which, wire::released(&payload)),
                    (wire::to_client::RELEASED, Ok(())),
                    "{end:?}: its leases not released before its end"
                );
                let (which, payload) = wire::read_frame(&mut orchestrator).unwrap().unwrap();
                assert_eq!(which, wire::to_client::ENDED);
                assert_eq!(wire::RunEnded::decode(&payload).unwrap(), end);
                let late = shards_net::lease::encode_ask(9, CLAUDE);
                shards_ipc::send(&net, kind::NET_LEASE, &late, &[]).unwrap();
                let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
                assert_eq!(
                    shards_net::lease::decode_answer(&m.payload),
                    Some((9, Err(Refusal::Ended)))
                );
                drop(net);
                relaying.join().unwrap();
            });
            drop(answers);
            let mut rest = Vec::new();
            orchestrator.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty(), "{end:?}: told the orchestrator more: {rest:?}");
        }
    }

    /// A frame that carried a lease's value (D242) leaves nothing of it in the run host's own
    /// buffer once let go of. Do: read a `LEASED` frame through the run host's channel, decode
    /// it, let it go, then read the frame after it. Expect: the value read whole; then not one
    /// byte of the channel's buffer other than zero, so nothing of the value left in it; and
    /// the next frame read whole after it, the channel having read nothing past its frame.
    #[test]
    fn a_leased_frame_leaves_nothing_of_its_value_in_the_channels_buffer() {
        let value = VALUE.as_bytes();
        let answer = wire::LeaseAnswer {
            id: 7,
            result: Ok(wire::Lease {
                value: wire::Secret::from(VALUE),
                lives: NonZeroU64::MIN,
            }),
        };
        let mut sent = wire::frame(wire::to_host::LEASED, &answer.encode().unwrap()).unwrap();
        sent.extend(wire::frame(wire::to_host::STOP, &[]).unwrap());
        let mut channel = Channel::new(&sent[..]);
        {
            let frame = channel.next().unwrap().unwrap();
            assert_eq!(frame.kind, wire::to_host::LEASED);
            let lease = wire::LeaseAnswer::decode(&frame).unwrap().result.unwrap();
            assert_eq!(lease.value.0, value);
        }
        assert!(
            !channel.buf.windows(value.len()).any(|w| w == value),
            "the lease's value left in the run host's buffer"
        );
        assert!(
            channel.buf.iter().all(|&b| b == 0),
            "a frame's bytes left in the run host's buffer"
        );
        let frame = channel.next().unwrap().unwrap();
        assert_eq!((frame.kind, frame.len()), (wire::to_host::STOP, 0));
        drop(frame);
        assert!(channel.next().unwrap().is_none());
    }

    /// The network process's control read by one reader for the run's life (D242, D269). Do:
    /// ask two answers of a stand-in network process, one on each of two threads, and answer
    /// them in order; then send what answers no ask, of a run with no grants; then ask once
    /// more. Expect: each asker handed the answer of its own kind, in the order asked; the
    /// stray message ends the reading, and the ask after it refused at once, as nothing reads
    /// an answer then.
    #[cfg(unix)]
    #[test]
    fn the_network_processs_answers_reach_their_askers_in_the_order_asked() {
        use std::os::unix::net::UnixStream;
        let (net, run_host) = UnixStream::pair().unwrap();
        let answers = Answers(Mutex::new(Box::new(Vec::<u8>::new())));
        let control = NetControl::new(&run_host);
        std::thread::scope(|s| {
            let reading = s.spawn(|| relay(&control, None, &answers));
            let first = s.spawn(|| control.ask(kind::NET_UNPUBLISHED, &1u32.to_be_bytes()));
            let m = shards_ipc::recv(&net).unwrap().unwrap();
            assert_eq!(
                (m.kind, m.payload.as_slice()),
                (kind::NET_UNPUBLISHED, &[0, 0, 0, 1][..])
            );
            let second = s.spawn(|| control.ask(kind::NET_NAMES, &[]));
            let m = shards_ipc::recv(&net).unwrap().unwrap();
            assert_eq!(m.kind, kind::NET_NAMES);
            shards_ipc::send(&net, kind::NET_UNPUBLISHED, &[], &[]).unwrap();
            assert_eq!(first.join().unwrap(), Ok(Vec::new()));
            shards_ipc::send(&net, kind::NET_NAMES, &[7], &[]).unwrap();
            assert_eq!(second.join().unwrap(), Ok(vec![7]));
            // An answer no ask awaits, and no lease a run with no grants could ask: nothing
            // more is read, and an ask is refused at once.
            shards_ipc::send(&net, kind::NET_LEASE, &[], &[]).unwrap();
            reading.join().unwrap();
            assert!(control.ask(kind::NET_UNPUBLISHED, &2u32.to_be_bytes()).is_err());
        });
    }

    /// An answer to no ask of the run's breaks the orchestrator's channel; one of a run that
    /// has released its leases goes nowhere (D242).
    #[cfg(unix)]
    #[test]
    fn an_answer_to_no_ask_breaks_the_channel() {
        let (_net, run_host) = std::os::unix::net::UnixStream::pair().unwrap();
        let control = NetControl::new(&run_host);
        let leasing = Leasing {
            ledger: Mutex::new(Ledger::new(vec![CLAUDE.into()], Vec::new())),
            net: &control,
        };
        let answer = wire::LeaseAnswer {
            id: 7,
            result: Err(wire::LeaseRefusal::Ended),
        }
        .encode()
        .unwrap();
        assert!(leased(&leasing, &answer).is_err());
        assert!(leased(&leasing, &[0, 0]).is_err());
        assert!(lock(&leasing.ledger).release().is_some());
        assert!(leased(&leasing, &answer).is_ok());
    }

    /// What the run host says of its leases names each by its credential's reference and its
    /// lease's id, and never a value: none is given it (D242).
    #[test]
    fn the_audit_names_leases_never_their_values() {
        let denied = Refusal::Denied("no such secret".into());
        let lines = [
            Audit::Asked {
                reference: CLAUDE,
                id: 3,
            }
            .to_string(),
            Audit::Held {
                reference: CLAUDE,
                id: 3,
                lives: Duration::from_secs(3600),
            }
            .to_string(),
            Audit::Refused {
                reference: CLAUDE,
                id: Some(4),
                refusal: &denied,
            }
            .to_string(),
            Audit::Refused {
                reference: "github/token",
                id: None,
                refusal: &Refusal::Ungranted,
            }
            .to_string(),
            Audit::Released { held: 1, asked: 2 }.to_string(),
        ];
        assert_eq!(
            lines,
            [
                "credential anthropic/claude-max: lease 3 asked",
                "credential anthropic/claude-max: lease 3 held for 3600s",
                "credential anthropic/claude-max: lease 4 refused: its broker leases the run none of it: no such secret",
                "credential github/token: an ask refused here: no grant of the run names it",
                "its leases released as it ends: 1 held, 2 asked",
            ]
        );
        assert!(lines.iter().all(|l| !l.contains(VALUE)));
    }

    /// D251: the run's socket grant is held as shards-api holds it, as init holds it again:
    /// its directory absolute and plain, a lane and a byte at least, a run's own command's.
    #[test]
    fn a_socket_grant_is_held_as_shards_api_holds_it() {
        let grant = |dir: &str, lanes, window| wire::SocketGrant {
            dir: dir.into(),
            lanes,
            window,
        };
        assert!(socket_grant(&grant("/run/hex/channel", 8, 65_536), true).is_ok());
        for (g, command) in [
            (grant("run/hex", 8, 1), true),
            (grant("/a/../b", 8, 1), true),
            (grant("/a", 0, 1), true),
            (grant("/a", 1, 0), true),
            (grant("/a", 1, 1), false),
        ] {
            assert!(
                matches!(socket_grant(&g, command), Err(No(Cause::Request, _))),
                "{g:?} {command}"
            );
        }
    }

    /// D252: a run's endpoint is bound on loopback under a key of its own, made past its VM's
    /// start, which no two runs share; it hands its host the socket lanes its grant admits.
    #[test]
    fn a_runs_endpoint_is_bound_on_loopback_under_a_key_of_its_own() {
        let ask = wire::ControlAsk {
            controller: [7; 32],
            sockets: Some(wire::SocketGrant {
                dir: "/run/hex/channel".into(),
                lanes: 2,
                window: 4096,
            }),
            stdio: false,
        };
        let execs = shards_api::control::exec_admissions();
        let (first, served) =
            hosted::bind_control(hosted::ControlKey::made().unwrap(), &ask, execs, &[], &[]).unwrap();
        assert!(first.address.ip().is_loopback(), "{:?}", first.address);
        assert!(served.sockets.is_some());
        // Its execs, its members' MCP connections and its members' own lanes (D254, D238).
        assert!(served.execs.is_some() && served.members.is_some());
        let (second, _) =
            hosted::bind_control(hosted::ControlKey::made().unwrap(), &ask, execs, &[], &[]).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint, "two runs share one key");
    }

    /// D252, a warm host: its run's endpoint takes the key its host made as it waited for the
    /// run, its VM started, rather than making one on the run's way; two warm hosts' keys are
    /// never one, each host serving one run; and a run whose host made none makes its own as it
    /// binds.
    #[test]
    fn a_warm_runs_endpoint_takes_the_key_its_host_made_as_it_waited() {
        let ask = wire::ControlAsk {
            controller: [7; 32],
            sockets: None,
            stdio: false,
        };
        let execs = shards_api::control::exec_admissions();
        let (one, other) = (
            hosted::ControlKey::made().unwrap(),
            hosted::ControlKey::made().unwrap(),
        );
        let (made, made_other) = (one.fingerprint(), other.fingerprint());
        assert_ne!(made, made_other, "two warm hosts share one key");
        let (endpoint, _) =
            hosted::bind_control(hosted::control_key(Some(one)).unwrap(), &ask, execs, &[], &[]).unwrap();
        assert_eq!(
            endpoint.fingerprint, made,
            "its endpoint took another key than its host made"
        );
        let (own, _) =
            hosted::bind_control(hosted::control_key(None).unwrap(), &ask, execs, &[], &[]).unwrap();
        assert!(
            own.fingerprint != made && own.fingerprint != made_other,
            "a run whose host made none bound with another's"
        );
    }

    /// D252, version 30 (Hex's ask): what the orchestrator reads of a run's endpoint names
    /// every UDP socket it holds, as the host's sockets hold them. Do: bind a real endpoint as
    /// a run does, tell it through `Started` and read it back as the orchestrator does. Expect:
    /// its served address, its waker's and its wake's, each the endpoint's own, three distinct
    /// loopback addresses that the host refuses to bind again while the endpoint lives and
    /// gives back once it and every handle of it are gone.
    #[test]
    fn the_orchestrator_is_told_every_socket_its_runs_endpoint_holds() {
        let ask = wire::ControlAsk {
            controller: [7; 32],
            sockets: Some(wire::SocketGrant {
                dir: "/run/hex/channel".into(),
                lanes: 2,
                window: 4096,
            }),
            stdio: false,
        };
        let (endpoint, served) = hosted::bind_control(
            hosted::ControlKey::made().unwrap(),
            &ask,
            shards_api::control::exec_admissions(),
            &[],
            &[],
        )
        .unwrap();
        let started = wire::Started {
            members: Vec::new(),
            published: Vec::new(),
            control: Some(hosted::control_at(&endpoint)),
            from: wire::StartedFrom::Booted,
            egress: wire::Narrowing::default(),
        };
        let told = wire::Started::decode(&started.encode().unwrap())
            .unwrap()
            .control
            .unwrap();
        assert_eq!(told.address, endpoint.address);
        assert_eq!(told.vm, endpoint.fingerprint);
        assert_eq!((told.waker, told.wake), (endpoint.waker, endpoint.wake));
        let sockets = told.sockets();
        assert!(sockets.iter().all(|a| a.ip().is_loopback()), "{sockets:?}");
        assert!(
            sockets[0] != sockets[1] && sockets[1] != sockets[2] && sockets[0] != sockets[2],
            "{sockets:?}"
        );
        for addr in sockets {
            let again = std::net::UdpSocket::bind(addr);
            assert!(
                matches!(&again, Err(e) if e.kind() == std::io::ErrorKind::AddrInUse),
                "{addr} is not the endpoint's: {again:?}"
            );
        }
        drop((endpoint, served));
        for addr in sockets {
            std::net::UdpSocket::bind(addr).unwrap_or_else(|e| panic!("{addr} still held: {e}"));
        }
    }

    /// D232 through D231, what a run naming GPUs asks (2d60abb24 asked none, so its guest had
    /// no GPU driver and its command no render node): the microVM's GPU, as `--gpus 1` asks
    /// it, where this host gives one, so that `run::prepare` gives it the GPU's guest kernel
    /// (D144), and its command the GPU's render node, as `--gpus` gives a run's; a run naming
    /// none asks for neither.
    #[test]
    fn a_run_naming_gpus_asks_for_the_gpu_guest_and_its_render_node() {
        let naming = wire::Request {
            command: Some(vec!["node".into()]),
            gpu_devices: vec![shards_api::PhysicalGpu::MetalRegistry(0x1_0000_02a1)],
            ..request(Vec::new())
        };
        let gpus = hosted::as_run(&naming, None);
        let none = hosted::as_run(&request(Vec::new()), None);
        assert_eq!(
            crate::run::wants_gpu(&gpus).is_ok_and(|wants| wants),
            crate::run::GIVES_GPU
        );
        assert_eq!(crate::run::wants_gpu(&none), Ok(false));
        let renders = |run: &shards_ipc::Run| {
            let node = crate::run::RENDER_NODE.as_bytes();
            let mut setup = Vec::new();
            crate::setup::security_setup(&mut setup, run, shards_seccomp::Kernel(6, 18), None, &[], None)
                .unwrap();
            setup
                .iter()
                .any(|e| e.starts_with(b"devices=") && e.windows(node.len()).any(|w| w == node))
        };
        assert_eq!(renders(&gpus), crate::run::GIVES_GPU);
        assert!(!renders(&none));
    }

    /// A stand-in snapshot at `dir`, as a VM's save leaves one (`snapshot::exists`): a pointer
    /// to a generation with its state.
    fn snapshot_at(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("g-1")).unwrap();
        std::fs::write(dir.join("current"), b"g-1\n").unwrap();
        std::fs::write(dir.join("g-1").join("state"), b"").unwrap();
    }

    /// A machine of stand-ins (no VM): each VM the way it was launched; ready, or not, as
    /// told, launch by launch; a save's VM leaving its snapshot at its fresh name as it is
    /// launched, where `saves`; what it ended, recorded, held and said, kept.
    #[derive(Default)]
    struct Stand {
        unready: Vec<bool>,
        unlaunchable: bool,
        saves: bool,
        launched: Vec<Way>,
        ended: Vec<Way>,
        recorded: Vec<std::path::PathBuf>,
        held: Vec<std::path::PathBuf>,
        said: Vec<String>,
    }

    impl Machine for Stand {
        type Vm = Way;
        type Held = std::path::PathBuf;
        fn launch(&mut self, way: &Way) -> Result<Way, String> {
            if self.unlaunchable {
                return Err("starting shards-vm: no such file".into());
            }
            self.launched.push(way.clone());
            if let (Way::Save { fresh, .. }, true) = (way, self.saves) {
                snapshot_at(fresh);
            }
            Ok(way.clone())
        }
        fn ready(&mut self, _vm: &mut Way) -> Result<(), String> {
            let unready = !self.unready.is_empty() && self.unready.remove(0);
            if unready {
                return Err("the VM ended before it was ready".into());
            }
            Ok(())
        }
        fn end(&mut self, vm: Way) {
            self.ended.push(vm);
        }
        fn record(&mut self, dir: &std::path::Path) -> Result<(), String> {
            self.recorded.push(dir.to_path_buf());
            Ok(())
        }
        fn hold(&mut self, dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
            self.held.push(dir.to_path_buf());
            Ok(dir.to_path_buf())
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    /// D253: a run whose template is saved restores it, holding its files as its VM restores
    /// it, and is told so; one whose template is not saved boots saving it at a name of its
    /// process's alone, which becomes the template once its VM is ready, its record written,
    /// its files held; one of no template boots alone, holding nothing.
    #[test]
    fn a_runs_template_restores_where_saved_else_its_boot_saves_it() {
        let home = shards_testdir::TempDir::new("tmpl").unwrap();
        let dir = home.path().join("templates").join("abc");
        let exists = |d: &std::path::Path| shards_vmm::snapshot::exists(d);
        let fresh = fresh_of(&dir);
        assert_eq!(
            way_of(Some(&dir), exists),
            Way::Save {
                dir: dir.clone(),
                fresh: fresh.clone()
            }
        );
        let mut m = Stand {
            saves: true,
            ..Stand::default()
        };
        let (vm, from, held) = started(&mut m, way_of(Some(&dir), exists)).unwrap();
        assert_eq!(from, wire::StartedFrom::Saved);
        assert!(matches!(vm, Way::Save { .. }));
        assert!(shards_vmm::snapshot::exists(&dir) && !fresh.exists());
        assert_eq!(
            (m.recorded, m.held, held),
            (vec![dir.clone()], vec![dir.clone()], Some(dir.clone()))
        );

        assert_eq!(way_of(Some(&dir), exists), Way::Restore { dir: dir.clone() });
        let mut m = Stand::default();
        let (_, from, held) = started(&mut m, way_of(Some(&dir), exists)).unwrap();
        assert_eq!(from, wire::StartedFrom::Restored);
        assert_eq!(m.launched, [Way::Restore { dir: dir.clone() }]);
        assert_eq!((m.held, held), (vec![dir.clone()], Some(dir.clone())));
        assert!(m.recorded.is_empty() && m.ended.is_empty());

        assert_eq!(way_of(None, exists), Way::Boot);
        let mut m = Stand::default();
        let (_, from, held) = started(&mut m, Way::Boot).unwrap();
        assert_eq!((from, held), (wire::StartedFrom::Booted, None));
        assert_eq!(m.launched, [Way::Boot]);
        assert!(m.held.is_empty());
    }

    /// D253: a template whose VM ends before it is ready is removed, its record with it, and
    /// the run boots saving another, said to its orchestrator as `Replaced` and on this host's
    /// diagnostics; a VM that cannot be launched at all is the host's failure, which leaves
    /// the template as it is and fails the run.
    #[test]
    fn a_template_that_does_not_restore_is_replaced_and_the_run_told_so() {
        let home = shards_testdir::TempDir::new("broken").unwrap();
        let dir = home.path().join("templates").join("abc");
        snapshot_at(&dir);
        let record = crate::run::Origin::path(&dir).unwrap();
        std::fs::write(&record, b"{}").unwrap();
        let mut m = Stand {
            unready: vec![true],
            saves: true,
            ..Stand::default()
        };
        let (_, from, held) = started(&mut m, Way::Restore { dir: dir.clone() }).unwrap();
        assert_eq!(from, wire::StartedFrom::Replaced);
        assert_eq!(m.ended, [Way::Restore { dir: dir.clone() }]);
        assert_eq!(
            m.launched,
            [
                Way::Restore { dir: dir.clone() },
                Way::Save {
                    dir: dir.clone(),
                    fresh: fresh_of(&dir)
                }
            ]
        );
        assert!(!record.exists(), "the broken template's record");
        assert!(shards_vmm::snapshot::exists(&dir), "the template saved again");
        assert_eq!((m.recorded, held), (vec![dir.clone()], Some(dir.clone())));
        assert!(
            m.said.iter().any(|l| l.contains("does not restore")),
            "{:?}",
            m.said
        );

        let mut m = Stand {
            unlaunchable: true,
            ..Stand::default()
        };
        match started(&mut m, Way::Restore { dir: dir.clone() }) {
            Err(No(Cause::Boot, why)) => assert!(why.contains("no such file"), "{why}"),
            Err(No(cause, why)) => panic!("{cause:?}: {why}"),
            Ok(_) => panic!("started"),
        }
        assert!(
            shards_vmm::snapshot::exists(&dir),
            "the host's failure removes no template"
        );
    }

    /// D253: a boot whose VM is never ready fails the run, `Boot`, ended, its save removed and
    /// no template made of it; another run's template, saved first, stays the template.
    #[test]
    fn a_boot_that_fails_leaves_no_save_and_another_runs_save_wins() {
        let home = shards_testdir::TempDir::new("save").unwrap();
        let dir = home.path().join("templates").join("abc");
        let fresh = fresh_of(&dir);
        let mut m = Stand {
            unready: vec![true],
            saves: true,
            ..Stand::default()
        };
        let way = Way::Save {
            dir: dir.clone(),
            fresh: fresh.clone(),
        };
        assert!(matches!(started(&mut m, way.clone()), Err(No(Cause::Boot, _))));
        assert_eq!(m.ended, std::slice::from_ref(&way));
        assert!(!fresh.exists() && !dir.exists());
        assert!(m.recorded.is_empty() && m.held.is_empty());

        snapshot_at(&dir);
        std::fs::write(dir.join("g-1").join("first"), b"").unwrap();
        let mut m = Stand {
            saves: true,
            ..Stand::default()
        };
        let (_, from, held) = started(&mut m, way).unwrap();
        assert_eq!((from, held), (wire::StartedFrom::Saved, Some(dir.clone())));
        assert!(
            dir.join("g-1").join("first").exists(),
            "the first save stays the template"
        );
        assert!(!fresh.exists());
    }

    /// D253: what is at a template's name and is no snapshot, as a removal cut short leaves
    /// it, is removed before the run's save, which then becomes the template.
    #[test]
    fn a_name_holding_no_snapshot_is_cleared_for_the_save() {
        let home = shards_testdir::TempDir::new("cleared").unwrap();
        let dir = home.path().join("templates").join("abc");
        std::fs::create_dir_all(dir.join("g-1")).unwrap();
        std::fs::write(dir.join("g-1").join("memory"), b"").unwrap();
        let way = way_of(Some(&dir), shards_vmm::snapshot::exists);
        assert!(matches!(way, Way::Save { .. }));
        let mut m = Stand {
            saves: true,
            ..Stand::default()
        };
        let (_, from, held) = started(&mut m, way).unwrap();
        assert_eq!((from, held), (wire::StartedFrom::Saved, Some(dir.clone())));
        assert!(shards_vmm::snapshot::exists(&dir));
        assert!(!dir.join("g-1").join("memory").exists());
        assert!(m.said.iter().any(|l| l.contains("no snapshot")), "{:?}", m.said);
    }

    /// A run prepared as `run::prepare` makes one, of `boot`, its root filesystem at `rootfs`,
    /// given its GPU's kernel where `gpu` names one.
    fn prepared(
        rootfs: &std::path::Path,
        boot: crate::run::Boot,
        gpu: Option<std::path::PathBuf>,
    ) -> crate::run::Prepared {
        crate::run::Prepared {
            cpus: crate::resources::host_cpus(),
            boot,
            gpu,
            vfio: Ok(Vec::new()),
            vfio_kernel: None,
            cdi: Ok(None),
            rootfs: rootfs.to_path_buf(),
            spec: shards_abi::run::Spec::default(),
            interactive: false,
            lease: None,
            stop_signal: None,
            options: crate::spec::Options::default(),
            health: None,
            shell: Vec::new(),
            labels: Default::default(),
            parent: String::new(),
            exposed: Vec::new(),
            image_id: String::new(),
            image_manifest: String::new(),
            size: (2, 256),
            memory_max: 256,
            image_volumes: Vec::new(),
            agentfile: None,
            shares: 0,
            emulator: None,
            layer: crate::run::LAYER_HOLDS,
        }
    }

    /// D253: an orchestrator's run's microVM is the daemon's for the same inputs, its words
    /// and its template's name alike: its guest's config shaped by `run::Shape`, its template
    /// named by `run::template`. A run given its GPU boots the GPU's guest under a template of
    /// its own (D150), and one naming GPUs keys them too, each apart (D232); a guest its
    /// caller gave, or a host that restores no snapshot, has no template.
    #[test]
    fn an_orchestrators_runs_microvm_is_the_daemons_for_the_same_inputs() {
        use std::path::Path;
        let home = shards_testdir::TempDir::new("same").unwrap();
        let home = home.path();
        let rootfs = home.join("root.erofs");
        std::fs::write(&rootfs, b"root").unwrap();
        let guest = crate::guest::Guest {
            kernel: home.join("vmlinux"),
            init: home.join("init"),
            kernel_digest: "sha256:k".into(),
            init_digest: "sha256:i".into(),
        };
        let baseline = shards_vmm::arch::cpu::Id::default();
        let shape = crate::run::Shape {
            size: (2, 256),
            cpu_baseline: &baseline,
            host_memory: Some(8 << 30),
            server: None,
            emulator: None,
            shares: 1,
            share_window_mib: 0,
            max_map_count: None,
            pci_slots: 4,
            bridge: None,
        };
        let stamp = shards_image::store::rootfs_stamp(&rootfs);
        let daemons = |guest: &crate::guest::Guest, gpu: bool| {
            let mut cfg = shards_vmm::vm::Config::new(guest.kernel.clone(), Some(guest.init.clone()));
            shape.give(&mut cfg);
            cfg.gpu = gpu;
            let dir = crate::run::template(
                home,
                guest,
                &rootfs,
                stamp.as_deref(),
                &cfg,
                crate::run::LAYER_HOLDS,
            );
            (cfg, dir)
        };
        let words = |cfg: &shards_vmm::vm::Config| crate::run::boot_args(cfg, &rootfs, None, "3");

        let plain = prepared(&rootfs, crate::run::Boot::Stored(guest.clone()), None);
        let ours = microvm(home, &plain, &shape, &[], true);
        let (cfg, dir) = daemons(&guest, false);
        assert_eq!(words(&ours.cfg), words(&cfg));
        let template = ours.template.as_ref().unwrap();
        assert_eq!(
            (&template.dir, &template.guest, &template.stamp),
            (&dir, &guest, &stamp)
        );

        let kernel = home.join("vmlinux-gpu");
        let gpu = prepared(
            &rootfs,
            crate::run::Boot::Stored(guest.clone()),
            Some(kernel.clone()),
        );
        let ours = microvm(home, &gpu, &shape, &[], true);
        let gpu_guest = crate::run::gpu_guest(&guest, &kernel);
        let (cfg, gpu_dir) = daemons(&gpu_guest, true);
        // The GPU's guest kernel with its guest's init (2d60abb24 booted the stored kernel).
        assert_eq!(
            (&ours.cfg.kernel, &ours.cfg.init),
            (&kernel, &Some(guest.init.clone()))
        );
        assert_eq!(words(&ours.cfg), words(&cfg));
        assert_eq!(ours.template.as_ref().map(|t| &t.dir), Some(&gpu_dir));
        assert_ne!(gpu_dir, dir);

        // The GPUs it names, in D232's grammar after `--gpu`, as shards-api's direct boot gives
        // them, in its key too.
        let selected = [
            shards_api::PhysicalGpu::Pci {
                domain: 0,
                bus: 1,
                device: 0,
                function: 0,
            },
            shards_api::PhysicalGpu::MetalRegistry(0x1_0000_02a1),
        ];
        let ours = microvm(home, &gpu, &shape, &selected, true);
        let named = ours.template.as_ref().map(|t| t.dir.clone()).unwrap();
        assert!(named != gpu_dir && named != dir);
        let said = words(&ours.cfg);
        let at = said.iter().position(|w| w == "--gpu").unwrap();
        assert_eq!(
            said.get(at..at + 5).unwrap(),
            [
                "--gpu",
                "--gpu-device",
                "pci:00000000:01:00.0",
                "--gpu-device",
                "metal:00000001000002a1"
            ]
            .map(std::ffi::OsString::from)
        );

        let given = crate::run::Boot::Given(Box::new(shards_vmm::vm::Config::new(
            guest.kernel.clone(),
            Some(guest.init.clone()),
        )));
        assert!(
            microvm(home, &prepared(&rootfs, given, None), &shape, &[], true)
                .template
                .is_none()
        );
        assert!(microvm(home, &plain, &shape, &[], false).template.is_none());
        assert!(Path::new(&dir).starts_with(home.join("templates")));
    }

    const SIGNED: &str = "claude/acct-1";
    const SIGNED_ACCESS: &str = "sign-ins/claude/acct-1/access";
    const ACCESS_HOLDER: &str = "kiln-placeholder-access-0123456789abcdef0123456789abcdef";
    const REFRESH_HOLDER: &str = "kiln-placeholder-refresh-fedcba9876543210fedcba9876543210";

    /// A run's sign-in of Claude Code (D242, step 8), as an earlier capture gave it where
    /// `captured`.
    fn signs_in(captured: bool) -> wire::SignInGrant {
        wire::SignInGrant {
            sign_in: SIGNED.into(),
            cli: wire::Cli::ClaudeCode,
            toward: vec!["api.anthropic.com".into()],
            captured: captured.then(|| wire::Captured {
                access: SIGNED_ACCESS.into(),
                access_placeholder: ACCESS_HOLDER.into(),
                refresh_placeholder: Some(REFRESH_HOLDER.into()),
            }),
        }
    }

    /// network-05: an orchestrated run whose agents reach past the microVM by IPv6 tells its
    /// network process eth0's addresses, as a daemon's run on the bridge does, before its
    /// policy. Do: tell a stand-in network process a bridge run's addresses and policy.
    /// Expect: NET_ADDRESS first, its payload the bridge's IPv4 and the home's IPv6; then
    /// NET_POLICY.
    #[cfg(unix)]
    #[test]
    fn an_orchestrated_runs_network_process_is_told_its_ipv6_before_its_policy() {
        use std::os::unix::net::UnixStream;
        let home = shards_testdir::TempDir::new("api-addr6").unwrap();
        let bridge = shards_net::bridge::Bridge::elect(&[]).unwrap();
        let (guest6, gateway6) = crate::networks::host_ipv6(home.path());
        let address = crate::networks::bridge_address(&bridge, (guest6, gateway6));
        let policy = shards_net::encode_policy(
            &shards_net::Ports::default(),
            &[("[2001:db8::7]".into(), 443)],
            false,
        );
        let (net, run_host) = UnixStream::pair().unwrap();
        let told = std::thread::scope(|s| {
            let network = s.spawn(|| {
                let mut told = Vec::new();
                let soon = || Instant::now() + Duration::from_secs(120);
                while let Ok(Some(m)) = shards_ipc::recv_by(&net, soon()) {
                    shards_ipc::send(&net, m.kind, &[], &[]).unwrap();
                    told.push((m.kind, m.payload));
                }
                told
            });
            let ca = hosted::tell_net(
                &run_host,
                Some(&address),
                &[],
                Some(&policy),
                Some(&policy),
                None,
                None,
            );
            assert_eq!(ca, Ok(None));
            drop(run_host);
            network.join().unwrap()
        });
        let kinds: Vec<u8> = told.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [kind::NET_ADDRESS, kind::NET_POLICY]);
        let mut want = bridge.guest().octets().to_vec();
        want.push(bridge.subnet().1);
        want.extend_from_slice(&bridge.gateway().octets());
        want.extend_from_slice(&guest6.octets());
        want.push(64);
        want.extend_from_slice(&gateway6.octets());
        assert_eq!(told[0].1, want);
    }

    /// A run whose request grants a sign-in has the grant reach its network process, after its
    /// policy and before its credential grants, whose CA names its hosts and whose trust its
    /// members are given; a run that grants none tells it none (D242, step 8). Do: tell a
    /// simulated network process a run's network, once with a sign-in of Claude Code and no
    /// credential grant, once with neither; and grant a sign-in whose token host the run's egress
    /// does not reach. Expect: the first told its policy, its sign-in (as the run asked it) and
    /// its grants, none, in that order, and its CA's hosts the sign-in's; the second its policy
    /// alone; the third refused before anything starts.
    #[cfg(unix)]
    #[test]
    fn a_run_whose_request_grants_a_sign_in_tells_its_network_process_and_one_that_grants_none_tells_none() {
        use std::os::unix::net::UnixStream;
        let policy = shards_net::encode_policy(
            &shards_net::Ports::default(),
            &[
                ("platform.claude.com".into(), 443),
                ("api.anthropic.com".into(), 443),
            ],
            false,
        );
        let der = b"the run's CA".to_vec();
        for granted in [true, false] {
            let asked = if granted {
                vec![signs_in(false)]
            } else {
                Vec::new()
            };
            let sign_ins = sign_ins(&asked, &[], Some(&policy)).ok().unwrap();
            let grants = credentials(&[], &[], Some(&policy), sign_ins.is_some())
                .ok()
                .unwrap();
            let (net, run_host) = UnixStream::pair().unwrap();
            let (ca, told) = std::thread::scope(|s| {
                let network = s.spawn(|| {
                    let mut told = Vec::new();
                    let soon = || Instant::now() + Duration::from_secs(120);
                    while let Ok(Some(m)) = shards_ipc::recv_by(&net, soon()) {
                        let answer = if m.kind == kind::NET_CREDENTIALS {
                            der.clone()
                        } else {
                            Vec::new()
                        };
                        shards_ipc::send(&net, m.kind, &answer, &[]).unwrap();
                        told.push((m.kind, m.payload));
                    }
                    told
                });
                let ca = hosted::tell_net(
                    &run_host,
                    None,
                    &[],
                    Some(&policy),
                    Some(&policy),
                    sign_ins.as_deref(),
                    grants.as_deref(),
                );
                drop(run_host);
                (ca, network.join().unwrap())
            });
            let kinds: Vec<u8> = told.iter().map(|(k, _)| *k).collect();
            if granted {
                assert_eq!(
                    kinds,
                    [kind::NET_POLICY, kind::NET_SIGN_INS, kind::NET_CREDENTIALS],
                    "its sign-in not told, or out of order"
                );
                assert_eq!(
                    shards_net::signin::decode_grants(&told[1].1),
                    Some(vec![signs_in(false)])
                );
                assert_eq!(
                    shards_net::decode_credentials(&told[2].1),
                    Some((Vec::new(), Vec::new()))
                );
                assert_eq!(ca, Ok(Some(der.clone())));
                let entries = trust(&der, &told[2].1, &asked).unwrap();
                let got = shards_abi::run::trust::of(&entries).unwrap().unwrap();
                assert_eq!(
                    got.hosts,
                    [b"platform.claude.com".as_slice(), b"api.anthropic.com"]
                );
            } else {
                assert_eq!(kinds, [kind::NET_POLICY], "a run with no sign-in told one");
                assert_eq!(ca, Ok(None));
            }
        }
        let api_alone = shards_net::encode_policy(
            &shards_net::Ports::default(),
            &[("api.anthropic.com".into(), 443)],
            false,
        );
        assert!(matches!(
            sign_ins(&[signs_in(false)], &[], Some(&api_alone)),
            Err(No(Cause::Request, _))
        ));
    }

    /// A sign-in relayed between its network process (shards_net::signin's `SignIns` and the
    /// lease client) and a simulated orchestrator, over a socket and pipes as the run host has
    /// them (D242, step 8). Do: capture the sign-in's tokens; lease its access token; refresh
    /// it; lease it again; revoke it in the VM; end the run, and lease it once more. Expect: the
    /// tokens reach the orchestrator whole, nothing of the provider called here; the capture's
    /// placeholders reach the network process and its access token is leased by the reference
    /// the orchestrator gave; the refresh answered with the same placeholders kept, the rotated
    /// token's response written with them and no rotated token, and the held lease let go of
    /// here so the next lease is asked anew rather than refused as live; the revocation told
    /// the orchestrator; the leases released as the run ends, before its end, and a lease asked
    /// after refused as ended.
    #[cfg(unix)]
    #[test]
    fn a_sign_in_is_relayed_its_placeholders_kept_through_a_refresh_and_its_lease_released_at_the_runs_end() {
        use std::os::unix::net::UnixStream;
        use zeroize::Zeroizing;
        let soon = || Instant::now() + Duration::from_secs(120);
        let (net, run_host) = UnixStream::pair().unwrap();
        let (mut orchestrator, out) = std::io::pipe().unwrap();
        let answers = Answers(Mutex::new(Box::new(out)));
        let control = NetControl::new(&run_host);
        let leasing = Leasing {
            ledger: Mutex::new(Ledger::new(Vec::new(), vec![signs_in(false)])),
            net: &control,
        };
        let life = NonZeroU64::new(3_600_000_000_000).unwrap();
        std::thread::scope(|s| {
            let relaying = s.spawn(|| relay(&control, Some(&leasing), &answers));
            let mut sign_ins = shards_net::signin::SignIns::new(vec![signs_in(false)]);
            let mut leases = shards_net::lease::Leases::new(Vec::new());
            let send = |kind, payloads: Vec<Zeroizing<Vec<u8>>>| {
                for p in payloads {
                    shards_ipc::send(&net, kind, &p, &[]).unwrap();
                }
            };
            let next =
                |orchestrator: &mut std::io::PipeReader| wire::read_frame(orchestrator).unwrap().unwrap();
            // The capture: the tokens to the orchestrator, its placeholders back.
            let taken = shards_net::signin::Taken {
                access: Zeroizing::new(b"sk-ant-oat01-real-access".to_vec()),
                refresh: Some(Zeroizing::new(b"sk-ant-ort01-real-refresh".to_vec())),
                expires_in: Some(28800),
                id_token: None,
                scope: Some("user:inference user:profile".into()),
            };
            sign_ins.capture(0, taken, 1).unwrap();
            send(kind::NET_SIGN_IN, sign_ins.asks());
            let (which, payload) = next(&mut orchestrator);
            assert_eq!(which, wire::to_client::CAPTURE);
            let ask = wire::CaptureAsk::decode(&payload).unwrap();
            assert_eq!(ask.sign_in, SIGNED);
            assert_eq!(ask.tokens.access, wire::Secret::from("sk-ant-oat01-real-access"));
            assert_eq!(ask.tokens.scope.as_deref(), Some("user:inference user:profile"));
            let kept = wire::CaptureAnswer {
                id: ask.id,
                result: Ok(signs_in(true).captured.unwrap()),
            };
            captured(&leasing, &kept.encode().unwrap()).unwrap();
            let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
            assert_eq!(m.kind, kind::NET_SIGN_IN);
            assert!(matches!(
                sign_ins.answered(&m.payload, &[]),
                Some(shards_net::signin::Changed::Captured { index: 0, .. })
            ));
            assert_eq!(sign_ins.grants[0].captured, signs_in(true).captured);
            // Its access token leased by the orchestrator's reference, as a grant's is.
            let lease = |orchestrator: &mut std::io::PipeReader,
                         leases: &mut shards_net::lease::Leases,
                         waiter,
                         value: &str| {
                assert!(matches!(
                    leases.ask(SIGNED_ACCESS, waiter, Instant::now()),
                    shards_net::lease::Asked::Waiting
                ));
                for ask in leases.asks() {
                    shards_ipc::send(&net, kind::NET_LEASE, &ask, &[]).unwrap();
                }
                let (which, payload) = next(orchestrator);
                assert_eq!(
                    which,
                    wire::to_client::LEASE,
                    "its access token's lease not asked"
                );
                let ask = wire::LeaseAsk::decode(&payload).unwrap();
                assert_eq!(ask.reference, SIGNED_ACCESS);
                let answer = wire::LeaseAnswer {
                    id: ask.id,
                    result: Ok(wire::Lease {
                        value: wire::Secret::from(value),
                        lives: life,
                    }),
                };
                leased(&leasing, &answer.encode().unwrap()).unwrap();
                let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
                assert_eq!(m.kind, kind::NET_LEASE);
                assert!(leases.answered(&m.payload, Instant::now()));
                assert_eq!(leases.held(SIGNED_ACCESS, Instant::now()), Some(value.as_bytes()));
            };
            leases.grant(SIGNED_ACCESS);
            lease(&mut orchestrator, &mut leases, 1, "sk-ant-oat01-real-access");
            // The refresh: asked of the orchestrator, never of the provider here; answered with
            // the rotated tokens' response; the placeholders the same.
            sign_ins.refresh(0, 2).unwrap();
            send(kind::NET_SIGN_IN, sign_ins.asks());
            let (which, payload) = next(&mut orchestrator);
            assert_eq!(which, wire::to_client::REFRESH);
            let ask = wire::RefreshAsk::decode(&payload).unwrap();
            assert_eq!(ask.sign_in, SIGNED);
            let rotated = r#"{"access_token":"sk-ant-oat01-rotated","refresh_token":"sk-ant-ort01-rotated","expires_in":28800}"#;
            let answer = wire::RefreshAnswer {
                id: ask.id,
                result: Ok(wire::Secret::from(rotated)),
            };
            refreshed(&leasing, &answer.encode().unwrap()).unwrap();
            assert!(
                !lock(&leasing.ledger)
                    .live
                    .values()
                    .any(|e| e.reference == SIGNED_ACCESS),
                "the rotated token's held lease kept: its next ask would be refused as live"
            );
            let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
            assert_eq!(m.kind, kind::NET_SIGN_IN);
            assert_eq!(
                sign_ins.answered(&m.payload, &[]),
                Some(shards_net::signin::Changed::Refreshed { index: 0 })
            );
            assert_eq!(
                sign_ins.grants[0].captured,
                signs_in(true).captured,
                "a refresh changed the sign-in's placeholders"
            );
            let Some(shards_net::signin::Answer::Refreshed(body)) = sign_ins.take(2) else {
                panic!("the refresh not answered")
            };
            let tokens = shards_net::signin::read(&body).unwrap().unwrap();
            let held = shards_net::signin::Placeholders {
                access: ACCESS_HOLDER.into(),
                refresh: Some(REFRESH_HOLDER.into()),
            };
            let written = shards_net::signin::write(&body, &tokens, &held).unwrap();
            let written = String::from_utf8(written.to_vec()).unwrap();
            assert!(written.contains(ACCESS_HOLDER) && written.contains(REFRESH_HOLDER));
            assert!(!written.contains("rotated"), "{written}");
            // Its held lease let go of, here as in the network process: the next is asked anew.
            leases.forget(SIGNED_ACCESS);
            lease(&mut orchestrator, &mut leases, 3, "sk-ant-oat01-rotated");
            // A revocation in the VM: told the orchestrator, answered by nobody.
            sign_ins.revoke(0);
            send(kind::NET_SIGN_IN, sign_ins.asks());
            let (which, payload) = next(&mut orchestrator);
            assert_eq!(which, wire::to_client::REVOKE);
            assert_eq!(wire::Revoked::decode(&payload).unwrap().sign_in, SIGNED);
            // The run's end: its leases released first, the captured token's among them.
            end_run(
                &answers,
                Some(&leasing),
                &wire::RunEnded::unused(wire::RunEnd::Exited(0), wire::Unused::Untold),
            );
            let (which, _) = next(&mut orchestrator);
            assert_eq!(
                which,
                wire::to_client::RELEASED,
                "its leases not released before its end"
            );
            let (which, _) = next(&mut orchestrator);
            assert_eq!(which, wire::to_client::ENDED);
            let late = shards_net::lease::encode_ask(9, SIGNED_ACCESS);
            shards_ipc::send(&net, kind::NET_LEASE, &late, &[]).unwrap();
            let m = shards_ipc::recv_by(&net, soon()).unwrap().unwrap();
            assert_eq!(
                shards_net::lease::decode_answer(&m.payload),
                Some((9, Err(Refusal::Ended)))
            );
            drop(net);
            relaying.join().unwrap();
        });
    }

    /// The run host's ledger of sign-ins (D242, step 8; R04). Do: ask of a sign-in no grant
    /// names; capture the run's; answer a capture never asked and one of another kind; refresh
    /// it while its access token's lease is held, and while one is asked; release the run.
    /// Expect: the stranger declined here; the capture's access token granted, in place of an
    /// earlier capture's; the stray answers refused; the refresh letting the held lease go and
    /// keeping the asked one; after the release, asks declined as ended and answers late.
    #[test]
    fn a_runs_ledger_relays_its_own_sign_ins_and_lets_a_refreshed_lease_go() {
        let mut ledger = Ledger::new(vec![CLAUDE.into()], vec![signs_in(false)]);
        let now = Instant::now();
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        assert_eq!(
            ledger.sign_in(1, "codex/other", Signing::Capture),
            Ok(Err(Declined::Denied("no sign-in of the run's".into())))
        );
        assert_eq!(ledger.sign_in(1, SIGNED, Signing::Capture), Ok(Ok(())));
        assert!(
            ledger.sign_in(1, SIGNED, Signing::Refresh).is_err(),
            "an id under way asked again"
        );
        assert!(
            ledger.signed_in(1, Signing::Refresh).is_err(),
            "an answer of another kind"
        );
        assert!(
            ledger.signed_in(2, Signing::Capture).is_err(),
            "an answer of no ask"
        );
        assert_eq!(ledger.signed_in(1, Signing::Capture), Ok(Some(0)));
        assert_eq!(ledger.ask(5, SIGNED_ACCESS, now), Err(Refusal::Ungranted));
        ledger.captured(0, &signs_in(true).captured.unwrap());
        let held = ledger.ask(5, SIGNED_ACCESS, now).unwrap();
        ledger.answer(held, Some(life), now).unwrap();
        assert_eq!(ledger.ask(6, SIGNED_ACCESS, now), Err(Refusal::Live));
        assert_eq!(ledger.sign_in(2, SIGNED, Signing::Refresh), Ok(Ok(())));
        assert_eq!(ledger.signed_in(2, Signing::Refresh), Ok(Some(0)));
        ledger.refreshed(0);
        let asked = ledger.ask(7, SIGNED_ACCESS, now).unwrap();
        ledger.refreshed(0);
        assert_eq!(
            ledger.ask(8, SIGNED_ACCESS, now),
            Err(Refusal::Live),
            "an ask under way let go"
        );
        // A second capture's reference in place of the first's.
        let mut again = signs_in(true).captured.unwrap();
        again.access = "sign-ins/claude/acct-1/access-2".into();
        ledger.captured(0, &again);
        assert_eq!(ledger.ask(9, SIGNED_ACCESS, now), Err(Refusal::Ungranted));
        assert!(ledger.ask(9, &again.access, now).is_ok());
        assert_eq!(ledger.sign_in(3, SIGNED, Signing::Refresh), Ok(Ok(())));
        assert!(ledger.release().is_some());
        assert_eq!(
            ledger.sign_in(4, SIGNED, Signing::Capture),
            Ok(Err(Declined::Ended))
        );
        assert_eq!(ledger.signed_in(3, Signing::Refresh), Ok(None));
        assert_eq!(ledger.answer(asked, Some(life), now), Ok(Answered::Late));
    }
}
