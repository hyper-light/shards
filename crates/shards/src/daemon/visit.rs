//! Visits to a stopped container's files (D37). dockerd reads and writes a stopped
//! container's writable layer where it keeps it, for `docker cp`, `diff` and `export`; a
//! microVM's files are read by the init that made them, so a stopped one's are read in a
//! VM booted over them: its image, its writable layer put back, and no command of the
//! image's (`builtin::HOLD`), the same built-ins answering as for a running one. The
//! container stays stopped throughout: no start, no end, no event of the visit's own; what
//! the visit changed (`cp` into it) is saved as its layer when the visit ends, as a run's
//! is.

use std::fs::File;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_cmdline::flags::{self, Outcome};
use shards_ipc::{Run, kind};

use super::{
    Daemon, Inbox, Keep, LAYER, LAYER_NEW, REQUEST, RunState, Threads, lock, log, network, open_layer,
};

/// How long a visit's VM may take to start holding.
const START_LIMIT: Duration = Duration::from_secs(30);

impl<D: crate::containers::Disk> Daemon<D> {
    /// Starts a visit to the files of the stopped container container command `argv` reads
    /// (`cp`'s steps, `diff`, `export`), if it reads one; returns the container visited,
    /// for [`end_visit`](Self::end_visit). A container that runs, or does not exist, or
    /// whose visit cannot start, is left to the command, which says so.
    pub(super) fn visit_for<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[String],
    ) -> Option<String> {
        let (given, copies) = target(argv)?;
        // What any client has seen of its last run first, as every command takes it
        // (`settle`): a run that has just returned its status may have sent a DONE not yet
        // taken, and the container would read as running still, its files then read in a
        // VM whose run is over.
        self.settle();
        let id = self.resolve(&given).ok()?;
        // One visit at a time to a container.
        {
            let mut visiting = lock(&self.visiting);
            while visiting.contains(&id) {
                visiting = self
                    .visited
                    .wait(visiting)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            visiting.insert(id.clone());
        }
        // Only where nothing runs or starts: a running container is read as it is.
        let free = {
            let mut runs = lock(&self.runs);
            let free = !runs.contains_key(&id);
            if free {
                runs.insert(id.clone(), RunState::Pending { cancelled: false });
            }
            free
        };
        if free {
            // Its files as its last run left them.
            self.await_settled(&id);
            match self.start_visit(threads, &id, copies) {
                Ok(()) => return Some(id),
                Err(e) => {
                    log(format!("container {id}: visiting its files: {e}"));
                    let mut runs = lock(&self.runs);
                    if matches!(runs.get(&id), Some(RunState::Pending { .. })) {
                        runs.remove(&id);
                    }
                    drop(runs);
                    self.resolved.notify_all();
                }
            }
        }
        lock(&self.visiting).remove(&id);
        self.visited.notify_all();
        None
    }

    /// Ends the visit to container `id`: its VM killed, and what it changed saved as the
    /// container's layer before anything else reads it.
    pub(super) fn end_visit(&self, id: &str) {
        let socket = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) if t.visit => Some(t.socket.clone()),
            _ => None,
        };
        if let Some(socket) = socket
            && let Err(e) = socket.send(kind::SIGNAL, &9u32.to_be_bytes(), &[])
        {
            log(format!("container {id}: ending a visit: {e}"));
        }
        // Its layer saved (or its VM gone without it), and its run over.
        self.await_settled(id);
        {
            let mut runs = lock(&self.runs);
            while matches!(runs.get(id), Some(RunState::Tracked(t)) if t.visit) {
                runs = self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
            }
        }
        lock(&self.visiting).remove(id);
        self.visited.notify_all();
    }

    /// Waits until no visit to container `id` is under way.
    pub(super) fn await_visit(&self, id: &str) {
        let mut visiting = lock(&self.visiting);
        while visiting.contains(id) {
            visiting = self
                .visited
                .wait(visiting)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Boots a VM over container `id`'s files, holding, as its run, pending in `runs`; for
    /// a copy (`copies`), with its shared directories.
    fn start_visit<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        copies: bool,
    ) -> Result<(), String> {
        let dir = lock(&self.containers).dir(id);
        let stored = std::fs::read(dir.join(REQUEST))
            .ok()
            .and_then(|b| Run::decode(&b))
            .ok_or_else(|| format!("container {id} was not made to be started again"))?;
        let cancel = shards_registry::http::Cancel::new();
        let mut prepared = crate::run::prepare(&stored, &self.home, &|_| {}, &cancel)?;
        // Its files as its runs see them, init's own included (handle()), and its limits.
        self.name_guest(&stored, &mut prepared.spec)?;
        prepared.spec.cgroup = crate::resources::cgroup(&stored.resources, crate::resources::host_cpus());
        prepared.spec.builtin = shards_abi::run::builtin::HOLD;
        // A copy sees the files its runs see (D38): its shared directories served and
        // mounted where they mount them, and its root read-only where theirs is, so that
        // a file copied in is where its command will find it, and none goes where its runs
        // could not write. A tmpfs is a run's own, and no stopped container's; a
        // directory only its agents mount is none of its root's.
        let link = if copies {
            let (points, first) = lock(&self.containers)
                .made(id)
                .map(|c| (c.mounts.clone(), c.started.is_none()))
                .unwrap_or_default();
            let points: Vec<_> = points.into_iter().filter(|p| !p.domains_only).collect();
            let opened = crate::volumes::open(&points, first, &crate::volumes::Store::new(&self.home))?;
            prepared.spec.setup = crate::setup::setup(&stored, &opened.mounts, None)?;
            crate::spec::fits(&prepared.spec)?;
            let link = self.start_shares(threads, &opened.dirs, &prepared.parent)?;
            prepared.shares = u32::try_from(opened.dirs.len()).map_err(|_| "too many shares")?;
            link
        } else {
            None
        };
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a visit's connection: {e}"))?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let layer_in = File::open(dir.join(LAYER)).ok();
        let layer_out = open_layer(&dir.join(LAYER_NEW)).map_err(|e| format!("its writable layer: {e}"))?;
        let mut flags = shards_ipc::RUN_LAYER_OUT;
        let mut fds: Vec<BorrowedFd<'_>> = vec![theirs.as_fd(), null.as_fd(), null.as_fd(), null.as_fd()];
        if let Some(file) = &layer_in {
            flags |= shards_ipc::RUN_LAYER_IN;
            fds.push(file.as_fd());
        }
        fds.push(layer_out.as_fd());
        if let Some(link) = &link {
            fds.push(link.as_fd());
        }
        let mut payload =
            Vec::with_capacity(shards_ipc::RunHead::LEN + prepared.spec.encoded_len().unwrap_or(0));
        let kept = self.log_retention()?;
        shards_ipc::RunHead {
            flags,
            log_size: kept.size,
            log_files: kept.files,
            first: 0,
            // The most memory its guest may plug (D148): a run's, for a visit's built-ins.
            plug: prepared
                .memory_max
                .saturating_sub(prepared.size.1)
                .saturating_mul(1 << 20),
            // No log, so none queued.
            queue: None,
            // Its own built-ins' reads: no run's I/O to limit.
            io_bytes: 0,
            io_requests: 0,
        }
        .encode_into(&mut payload);
        prepared.spec.encode_into(&mut payload);
        // No network: nothing of the image's runs.
        let start = network::Start::Attach(network::Net::None);
        let started = self.start_run(
            threads,
            id,
            &payload,
            &fds,
            Keep {
                detached: None,
                options: prepared.options.clone(),
                health: None,
                published: Vec::new(),
                named: None,
                restarted: false,
                layer_pending: true,
                log_files: kept.files,
                log_compress: false,
                visit: true,
                egress: None,
                agentfile: None,
                mac: None,
                link_local: Vec::new(),
                engine: None,
            },
            || self.warm_for(threads, &prepared, &start, &|_| {}),
        );
        drop(prepared.lease.take());
        drop(fds);
        drop((theirs, null, layer_in, layer_out, link));
        let inbox = started?;
        let held = self.await_holding(id, &inbox);
        self.follow(threads, id.to_string(), inbox);
        if let Err(e) = held {
            // Why, as its VM told its client: what it said on stderr, read until it ends.
            return Err(match said_to(&ours) {
                said if said.is_empty() => e,
                said => format!("{e}: {said}"),
            });
        }
        // What the VM says to its client, which nobody reads, read to its end.
        let drained = std::thread::Builder::new()
            .name("visit-client".into())
            .spawn_scoped(threads, move || {
                let mut sink = [0u8; 4096];
                while matches!(shards_ipc::read(&ours, &mut sink), Ok(n) if n > 0) {}
            });
        if let Err(e) = drained {
            log(format!("container {id}: a visit's client: {e}"));
        }
        Ok(())
    }

    /// Takes run `id`'s messages until its VM says it holds: the built-ins' requests then
    /// find its files in place.
    fn await_holding(&self, id: &str, inbox: &Arc<Mutex<Inbox>>) -> Result<(), String> {
        let fd = lock(inbox).socket.stream.as_raw_fd();
        let deadline = Instant::now() + START_LIMIT;
        loop {
            let ended = self.take_messages(id, inbox);
            {
                let held = lock(inbox);
                if held.started && !held.ended {
                    return Ok(());
                }
                if ended || held.ended {
                    return Err("its VM ended before it held".into());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("its VM did not start in time".into());
            }
            let mut polled = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = libc::c_int::try_from(left.as_millis().min(1000)).unwrap_or(1000);
            // SAFETY: poll(2) on one pollfd of a descriptor the inbox holds open.
            unsafe { libc::poll(&raw mut polled, 1, ms) };
        }
    }
}

/// What a visit's VM said on stderr to its client, `client`, until it ended (a second at
/// most), trimmed: why it ended before it held.
fn said_to(client: &UnixStream) -> String {
    let _ = client.set_read_timeout(Some(Duration::from_secs(1)));
    let mut said = Vec::new();
    while let Ok(Some(m)) = shards_ipc::recv(client) {
        if m.kind == kind::ERR {
            said.extend(m.payload);
        }
    }
    String::from_utf8_lossy(&said).trim().to_string()
}

/// The container container command `argv` reads the files of, given as its client gave
/// it: `cp`'s steps', `diff`'s and `export`'s; and whether it copies.
fn target(argv: &[String]) -> Option<(String, bool)> {
    use shards_cmdline::commands::{self, DIFF, EXPORT};
    if argv.first().is_some_and(|a| a == super::COPY_STEP) {
        return argv.get(2).cloned().map(|c| (c, true));
    }
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (command, path, named) = commands::find(&words)?;
    if !std::ptr::eq(command, &DIFF) && !std::ptr::eq(command, &EXPORT) {
        return None;
    }
    match flags::parse(
        command,
        path,
        argv.get(named..).unwrap_or_default(),
        &flags::value,
    ) {
        Outcome::Run(parsed) => parsed.args.first().cloned().map(|c| (c, false)),
        _ => None,
    }
}
