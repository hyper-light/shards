//! A simulator of [`Guests`]: its "guest" is a closure a test gives, run on a thread of its
//! own as PID 1 would be, serving vsock ports over in-process socket pairs. It refuses what
//! [`Shards`](crate::Shards) refuses (the same [`check`] and allowlist), and ends as a guest
//! does: as its closure returns, or as it is stopped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use crate::stream::{self, Socket};
use crate::{End, Guest, GuestSpec, Guests, Identity, Refusal, Stream, allowed, backend, check};

/// How many connections may wait to be accepted, as shards' muxer bounds its slots
/// (`crates/vmm/src/devices/virtio/vsock/muxer.rs` `slots`): what this process's descriptor
/// limit leaves past those it holds, two to a connection, a socket pair's ends
/// (`shards_abi::bounds::connections`). Read as each guest boots, its limit never raised: the
/// process is its program's. Where it cannot say what it holds, the limit alone; where it
/// cannot say its limit, no number, and a pair the kernel refuses (EMFILE) is refused as a
/// connect is.
fn slots() -> usize {
    match descriptor_limit() {
        Some(limit) => shards_abi::bounds::connections(limit, open_descriptors().unwrap_or(0), 2),
        None => usize::MAX,
    }
}

/// How many descriptors this process may have open: its soft limit (getrlimit(2)), and on
/// macOS no more than `kern.maxfilesperproc`, which the kernel holds a process to whatever
/// its limit (as shards-vmm's `platform::descriptor_limit`).
#[cfg(unix)]
fn descriptor_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut most: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: sysctlbyname(3) reading one int into a local of its size.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut most).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if read == 0
            && let Ok(most) = u64::try_from(most)
        {
            return Some(lim.rlim_cur.min(most));
        }
    }
    Some(lim.rlim_cur)
}

/// Windows: the kernel handles a process may hold, "The per-process limit on kernel handles
/// is 2^24" (Microsoft, "Kernel Objects"), with nothing to raise.
#[cfg(windows)]
fn descriptor_limit() -> Option<u64> {
    Some(1 << 24)
}

/// How many descriptors this process has open: the entries of /proc/self/fd on Linux and of
/// /dev/fd on macOS, less the one that lists them (as shards-vmm's
/// `platform::open_descriptors`).
#[cfg(unix)]
fn open_descriptors() -> Option<u64> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let count = std::fs::read_dir(dir).ok()?.count();
    u64::try_from(count.checked_sub(1)?).ok()
}

/// Windows: the handles this process has open (GetProcessHandleCount).
#[cfg(windows)]
fn open_descriptors() -> Option<u64> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0u32;
    // SAFETY: this process's pseudo-handle, and a count to fill.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    (ok != 0).then_some(u64::from(count))
}

/// The most vCPUs the simulated hypervisor runs: the CPUs this process may run on, as std
/// reads them, where shards-vmm asks its hypervisor (`vm/runtime.rs:1059` `check_vcpus`).
fn max_vcpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
}

/// A guest's program: given its side of the guest, it returns how the guest ends
/// ([`End::PowerOff`], say).
pub type Program = dyn Fn(&SimHost) -> End + Send + Sync;

/// Guests whose PID 1 is `program`, and those booted so far by their simulated VM's pid, for
/// [`Guests::attach`].
pub struct SimGuests {
    program: Arc<Program>,
    booted: Mutex<Booted>,
}

/// The guests a simulator booted: each one's identity's start, and its state while anything holds
/// it. One entry per boot, the bound a test's boots set.
#[derive(Default)]
struct Booted {
    next: u32,
    guests: BTreeMap<u32, (u64, Weak<Shared>)>,
}

impl std::fmt::Debug for SimGuests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimGuests").finish_non_exhaustive()
    }
}

impl SimGuests {
    pub fn new(program: impl Fn(&SimHost) -> End + Send + Sync + 'static) -> SimGuests {
        SimGuests {
            program: Arc::new(program),
            booted: Mutex::new(Booted::default()),
        }
    }
}

/// The guest's side: what it was booted with, the ports it listens on, and the
/// connections that come to them.
#[derive(Debug)]
pub struct SimHost {
    spec: GuestSpec,
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Connections that may wait to be accepted, at most ([`slots`]), read as it booted.
    slots: usize,
}

#[derive(Debug, Default)]
struct State {
    listening: BTreeSet<u32>,
    /// Connections made and not yet accepted, at most its slots ([`slots`]).
    pending: VecDeque<(u32, Socket)>,
    end: Option<End>,
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SimHost {
    /// The spec the guest was booted with.
    pub fn spec(&self) -> &GuestSpec {
        &self.spec
    }

    /// Listens on `port`: connections to it are made from now on.
    pub fn listen(&self, port: u32) {
        lock(&self.shared).listening.insert(port);
        self.shared.changed.notify_all();
    }

    /// The next connection, and its port; `None` once the guest has ended.
    pub fn accept(&self) -> Option<(u32, Box<dyn Stream>)> {
        let mut state = lock(&self.shared);
        loop {
            if state.end.is_some() {
                return None;
            }
            if let Some((port, s)) = state.pending.pop_front() {
                return Some((port, Box::new(s)));
            }
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether the guest has ended (stopped, say).
    pub fn ended(&self) -> bool {
        lock(&self.shared).end.is_some()
    }
}

impl Guests for SimGuests {
    fn backend(&self) -> Result<crate::Backend, Refusal> {
        backend()
    }

    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        check(spec)?;
        backend()?;
        let max = max_vcpus();
        if spec.vcpus > max {
            return Err(Refusal::Bounds {
                what: "vcpus",
                why: format!("{} vCPUs requested; this host supports {max}", spec.vcpus),
            });
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            slots: slots(),
        });
        let host = SimHost {
            spec: spec.clone(),
            shared: shared.clone(),
        };
        let program = self.program.clone();
        std::thread::Builder::new()
            .name("shards-api-sim".into())
            .spawn(move || {
                let end = program(&host);
                let mut state = lock(&host.shared);
                state.end.get_or_insert(end);
                host.shared.changed.notify_all();
            })
            .map_err(|e| Refusal::Boot(format!("the simulated guest's thread: {e}")))?;
        let identity = {
            let mut booted = self.booted.lock().unwrap_or_else(PoisonError::into_inner);
            // A simulated pid and start: each boot's its own, a start no other boot has.
            booted.next = booted.next.wrapping_add(1);
            let pid = booted.next;
            let started = u64::from(pid);
            booted.guests.insert(pid, (started, Arc::downgrade(&shared)));
            Identity {
                pid,
                started,
                vsock: PathBuf::from(format!("sim-vsock-{pid}")),
            }
        };
        Ok(Box::new(SimGuest {
            ports: spec.ports.clone(),
            shared,
            identity,
        }))
    }

    fn attach(&self, identity: &Identity, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        check(spec)?;
        let gone = |why: String| Refusal::Gone {
            pid: identity.pid,
            why,
        };
        // As shards on Windows, where a guest's VM ends with the process that booted it (D141).
        if cfg!(windows) {
            return Err(gone(
                "on Windows a guest's VM ends with the process that booted it (D141): none \
                 outlives its client to be taken back"
                    .into(),
            ));
        }
        let shared = {
            let booted = self.booted.lock().unwrap_or_else(PoisonError::into_inner);
            match booted.guests.get(&identity.pid) {
                None => return Err(gone("this simulator booted no guest as it".into())),
                Some((started, _)) if *started != identity.started => {
                    return Err(gone(format!(
                        "it started at {started}, not {}: another process",
                        identity.started
                    )));
                }
                Some((_, shared)) => shared.upgrade(),
            }
        };
        let shared = shared.ok_or_else(|| gone("its VM's process has ended".into()))?;
        if let Some(end) = &lock(&shared).end {
            return Err(Refusal::Ended(end.clone()));
        }
        Ok(Box::new(SimGuest {
            ports: spec.ports.clone(),
            shared,
            identity: identity.clone(),
        }))
    }
}

/// A simulated guest.
#[derive(Debug)]
struct SimGuest {
    ports: Vec<u32>,
    shared: Arc<Shared>,
    identity: Identity,
}

impl Guest for SimGuest {
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal> {
        allowed(&self.ports, port)?;
        // None: past `Instant`'s range (`Duration::MAX`): until the guest listens or ends.
        let deadline = Instant::now().checked_add(patience);
        let mut state = lock(&self.shared);
        loop {
            if let Some(end) = &state.end {
                return Err(Refusal::Ended(end.clone()));
            }
            if state.listening.contains(&port) {
                if state.pending.len() >= self.shared.slots {
                    return Err(Refusal::Channel {
                        port,
                        why: format!(
                            "{} connections wait to be accepted: what this process's descriptors leave",
                            self.shared.slots
                        ),
                    });
                }
                let (ours, theirs) = stream::pair().map_err(|e| Refusal::Channel {
                    port,
                    why: format!("a socket pair: {e}"),
                })?;
                state.pending.push_back((port, theirs));
                self.shared.changed.notify_all();
                return Ok(Box::new(ours));
            }
            state = match deadline.map(|d| d.saturating_duration_since(Instant::now())) {
                Some(left) if left.is_zero() => {
                    return Err(Refusal::Channel {
                        port,
                        why: format!("no OK within {patience:?}: the guest does not listen there"),
                    });
                }
                Some(left) => self.wait(state, left),
                None => self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    fn stop(&self) -> Result<(), Refusal> {
        let mut state = lock(&self.shared);
        state.end.get_or_insert(End::Stopped);
        // Connections not yet accepted close, as a stopped guest's do.
        state.pending.clear();
        self.shared.changed.notify_all();
        Ok(())
    }

    fn identity(&self) -> Result<Identity, Refusal> {
        Ok(self.identity.clone())
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal> {
        // None: past `Instant`'s range (`Duration::MAX`): until it ends.
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock(&self.shared);
        loop {
            if let Some(end) = &state.end {
                return Ok(Some(end.clone()));
            }
            state = match deadline.map(|d| d.saturating_duration_since(Instant::now())) {
                Some(left) if left.is_zero() => return Ok(None),
                Some(left) => self.wait(state, left),
                None => self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

impl SimGuest {
    /// Waits at most `left` for a change.
    fn wait<'a>(&'a self, state: MutexGuard<'a, State>, left: Duration) -> MutexGuard<'a, State> {
        self.shared
            .changed
            .wait_timeout(state, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }
}

impl Drop for SimGuest {
    /// Ends the guest, as dropping a real one does.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Descriptor limits are Unix's (setrlimit(2)); Windows has none to lower.
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::panic, clippy::print_stdout)]
mod tests {
    use super::*;
    use crate::{Init, min_memory_mib};

    /// Set in the child `past_what_its_descriptors_leave_a_connect_is_refused_by_its_bound`
    /// runs.
    const LOWERED: &str = "SHARDS_API_SIM_LOWERED";

    /// Connections waiting to be accepted are bounded as shards' muxer bounds its slots: by
    /// what this process's descriptors leave, two to a connection here, a socket pair's ends
    /// (`shards_abi::bounds::connections`). Past them a connect is refused naming that bound,
    /// not by the kernel (EMFILE). Run in a child of this test binary, which lowers its soft
    /// descriptor limit (setrlimit(2)) before the guest boots, leaving room for a few
    /// connections and a descriptor more, which no pair fits (review: connections derived,
    /// refused typed past them).
    #[test]
    fn past_what_its_descriptors_leave_a_connect_is_refused_by_its_bound() {
        const NAME: &str = "sim::tests::past_what_its_descriptors_leave_a_connect_is_refused_by_its_bound";
        const REFUSED: &str = "sim child: refused by its bound";
        // A vsock port: an identifier, not tuning.
        const PORT: u32 = 1234;
        if std::env::var_os(LOWERED).is_none() {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
                .env(LOWERED, "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&child.stdout);
            assert!(
                child.status.success() && said.contains(REFUSED),
                "{:?}\n{said}{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            return;
        }
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        // Listens, accepts nothing, and ends as the test lets it go.
        let guests = SimGuests::new(move |host| {
            host.listen(PORT);
            let _ = wait.lock().unwrap_or_else(PoisonError::into_inner).recv();
            End::PowerOff
        });
        let spec = GuestSpec {
            kernel: "kernel".into(),
            init: Init::Executable("init".into()),
            cmdline: String::new(),
            vcpus: 1,
            memory_mib: min_memory_mib(std::env::consts::ARCH),
            gpu: false,
            disks: vec![],
            shares: vec![],
            ports: vec![PORT],
            binary: None,
        };
        // An input: room for three connections, and a descriptor more, which no pair fits.
        let room = 3;
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        // Less the one that lists them.
        let held = std::fs::read_dir(dir).unwrap().count() - 1;
        let mut was = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) into a local.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut was) }, 0);
        let lowered = libc::rlimit {
            rlim_cur: libc::rlim_t::try_from(held + 2 * room + 1).unwrap(),
            rlim_max: was.rlim_max,
        };
        // SAFETY: setrlimit(2) from a local, in this child alone.
        let set = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const lowered) };
        assert_eq!(set, 0);
        let guest = guests.boot(&spec).unwrap();
        let mut kept = Vec::new();
        let refused = loop {
            match guest.connect(PORT, Duration::MAX) {
                Ok(stream) => kept.push(stream),
                Err(e) => break e,
            }
        };
        // SAFETY: setrlimit(2) from a local: the limit as it was.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const was) }, 0);
        drop(done);
        assert_eq!(kept.len(), room, "{refused:?}");
        match refused {
            Refusal::Channel { why, .. } if why.starts_with(&format!("{room} connections wait")) => {
                println!("{REFUSED}");
            }
            other => panic!("{other:?}"),
        }
    }
}
