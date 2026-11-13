//! `Sandbox`: a harness launched in a boundary (CP-T4.1): its program, its folder, its limits
//! (memory, processes) and the hosts it may reach, with its stdio streamed back and its end said
//! once, last. Its implementations are `host` (today's launch), Shards, then Hex's classes
//! (CP-T4.1, CP-T7.2). The suite asks its fixture for programs by what they do (echo, exit with a
//! code, use memory, start processes, connect), so each implementation supplies its own;
//! [`SimSandbox`] runs built-in programs that do those things.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

/// What a run may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: u64,
    pub processes: u32,
    /// Its share of the CPU against others', on cgroup v2's scale ([`WEIGHT_MIN`] to
    /// [`WEIGHT_MAX`]; 100 is an even share): a build held below an agent.
    pub cpu_weight: u16,
    /// Its share of the disk, on the same scale (2026-10-19's builds starved a disk, not a CPU).
    pub io_weight: u16,
}

/// The weights' scale (cgroup v2's `cpu.weight`, `io.weight`).
pub const WEIGHT_MIN: u16 = 1;
pub const WEIGHT_MAX: u16 = 10_000;

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: 1 << 30, processes: 64, cpu_weight: 100, io_weight: 100 }
    }
}

/// Where a run is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// A process on the host, as kiln runs harnesses today.
    Host,
    /// An OCI image, run rootless (Hex's `oci` class).
    Container { image: String },
    /// A microVM from `image`.
    MicroVm { image: String },
}

/// A run to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    /// Its working folder.
    pub folder: Option<String>,
    /// Its environment (no secrets: those are injected outside the boundary).
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    /// Hosts it may reach; every other is refused.
    pub egress: Vec<String>,
    pub boundary: Boundary,
}

impl Launch {
    /// `program` with `args`, on the host, default limits, no egress.
    pub fn of(program: &str, args: &[&str]) -> Self {
        Self { program: program.to_owned(), args: args.iter().map(|a| (*a).to_owned()).collect(), folder: None, env: Vec::new(), limits: Limits::default(), egress: Vec::new(), boundary: Boundary::Host }
    }
}

/// Which limit a run went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Memory,
    Processes,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed, through `Run::kill`.
    Killed,
    /// Ended by a limit: memory when its boundary killed it for going over (cgroup v2's
    /// `memory.events` counting an `oom_kill`); processes when it was refused one past its limit
    /// (`pids.max`: the fork fails, the run isn't stopped) and then failed. A run refused a process
    /// that coped and ended well ends `Code(0)`.
    OverLimit(Limit),
}

/// What a run says, in order: its output, then its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(Exit),
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no program {0:?}")]
    NoSuchProgram(String),
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("this sandbox can't hold a run in {0}")]
    Unsupported(String),
    #[error("the sandbox runs its most ({limit})")]
    Full { limit: usize },
    #[error("the run has ended")]
    Ended,
}

/// A run under way: what it's given on stdin, what it says (its output, then its end, once), and
/// a way to end it.
pub trait Run: Send + Sync {
    /// Gives the program `bytes` on its stdin, after what it was given before; refused `Ended`
    /// once the run has ended.
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Closes its stdin: the program reads the end of its input.
    fn close_stdin(&self);
    /// What it says next; `None` once its end has been given.
    fn next(&self) -> BoxFuture<'_, Option<Output>>;
    /// Ends it, unless it has ended: its end is then `Exited(Killed)`.
    fn kill(&self);
}

/// Harnesses launched in boundaries.
pub trait Sandbox: Send + Sync + 'static {
    /// Starts `launch` in its boundary, within its limits; refused, typed, when it can't.
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>>;
    /// The boundaries it can hold runs in, by name (`host`, `container`, `microvm`).
    fn boundaries(&self) -> Vec<&'static str>;
}

// ---------------------------------------------------------------------------------- simulator

/// Most runs a [`SimSandbox`] holds at once.
pub const SIM_RUNS: usize = 1000;
/// Most output a simulated run keeps unread; past it, the run is killed (its reader too slow).
pub const SIM_PENDING: usize = 10_000;

/// Built-in programs, run in memory:
///
/// - `say <words…>`: writes them to stdout, exits 0;
/// - `echo`: writes back what comes in on stdin, exits 0 once it closes;
/// - `fail <code>`: writes to stderr, exits with `code`;
/// - `wait`: runs until killed;
/// - `hog <bytes>`: uses that much memory, then exits 0;
/// - `spawn <n>`: starts `n` processes, then exits 0;
/// - `connect <host>`: exits 0 when the host is granted, 1 when it's refused;
/// - `pwd`: writes its working folder.
#[derive(Clone, Default)]
pub struct SimSandbox {
    runs: Arc<Mutex<usize>>,
}

impl SimSandbox {
    pub fn new() -> Self {
        Self::default()
    }
}

struct SimRun {
    state: Arc<Mutex<RunState>>,
    said: Arc<event_listener::Event>,
    runs: Arc<Mutex<usize>>,
}

#[derive(Default)]
struct RunState {
    out: VecDeque<Output>,
    echo: bool,
    ended: bool,
    given_end: bool,
}

impl RunState {
    fn say(&mut self, o: Output) {
        if self.ended {
            return;
        }
        if self.out.len() >= SIM_PENDING {
            self.out.push_back(Output::Exited(Exit::Killed));
            self.ended = true;
            return;
        }
        if matches!(o, Output::Exited(_)) {
            self.ended = true;
        }
        self.out.push_back(o);
    }
}

impl Drop for SimRun {
    fn drop(&mut self) {
        let mut n = self.runs.lock();
        *n = n.saturating_sub(1);
    }
}

impl Run for SimRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = {
            let mut st = self.state.lock();
            if st.ended {
                Err(Refusal::Ended)
            } else {
                if st.echo {
                    st.say(Output::Stdout(bytes));
                }
                Ok(())
            }
        };
        self.said.notify(usize::MAX);
        Box::pin(async move { out })
    }

    fn close_stdin(&self) {
        {
            let mut st = self.state.lock();
            if st.echo {
                st.say(Output::Exited(Exit::Code(0)));
            }
        }
        self.said.notify(usize::MAX);
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            loop {
                let listener = self.said.listen();
                {
                    let mut st = self.state.lock();
                    if let Some(o) = st.out.pop_front() {
                        if matches!(o, Output::Exited(_)) {
                            st.given_end = true;
                        }
                        return Some(o);
                    }
                    if st.given_end {
                        return None;
                    }
                }
                listener.await;
            }
        })
    }

    fn kill(&self) {
        self.state.lock().say(Output::Exited(Exit::Killed));
        self.said.notify(usize::MAX);
    }
}

impl Sandbox for SimSandbox {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        let out = (|| {
            for (what, w) in [("a cpu weight", launch.limits.cpu_weight), ("an io weight", launch.limits.io_weight)] {
                if !(WEIGHT_MIN..=WEIGHT_MAX).contains(&w) {
                    return Err(Refusal::Invalid { what, why: format!("{w} (from {WEIGHT_MIN} to {WEIGHT_MAX})") });
                }
            }
            match launch.boundary {
                Boundary::Host => {}
                Boundary::Container { .. } => return Err(Refusal::Unsupported("a container".into())),
                Boundary::MicroVm { .. } => return Err(Refusal::Unsupported("a microVM".into())),
            }
            let arg = |i: usize| launch.args.get(i).cloned().unwrap_or_default();
            let mut st = RunState::default();
            match launch.program.as_str() {
                "say" => {
                    st.say(Output::Stdout(launch.args.join(" ").into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                "echo" => st.echo = true,
                "fail" => {
                    let code: i32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "fail's code", why: arg(0) })?;
                    st.say(Output::Stderr(format!("failing with {code}").into_bytes()));
                    st.say(Output::Exited(Exit::Code(code)));
                }
                "wait" => {}
                "hog" => {
                    let bytes: u64 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "hog's bytes", why: arg(0) })?;
                    st.say(if bytes > launch.limits.memory_bytes { Output::Exited(Exit::OverLimit(Limit::Memory)) } else { Output::Exited(Exit::Code(0)) });
                }
                "spawn" => {
                    let n: u32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "spawn's count", why: arg(0) })?;
                    // It and what it starts.
                    st.say(if n.saturating_add(1) > launch.limits.processes { Output::Exited(Exit::OverLimit(Limit::Processes)) } else { Output::Exited(Exit::Code(0)) });
                }
                "connect" => {
                    let host = arg(0);
                    if launch.egress.contains(&host) {
                        st.say(Output::Stdout(format!("connected to {host}").into_bytes()));
                        st.say(Output::Exited(Exit::Code(0)));
                    } else {
                        st.say(Output::Stderr(format!("{host}: refused").into_bytes()));
                        st.say(Output::Exited(Exit::Code(1)));
                    }
                }
                "pwd" => {
                    st.say(Output::Stdout(launch.folder.clone().unwrap_or_else(|| "/".into()).into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                other => return Err(Refusal::NoSuchProgram(other.to_owned())),
            }
            let mut n = self.runs.lock();
            if *n >= SIM_RUNS {
                return Err(Refusal::Full { limit: SIM_RUNS });
            }
            *n = n.saturating_add(1);
            let run: Box<dyn Run> = Box::new(SimRun { state: Arc::new(Mutex::new(st)), said: Arc::new(event_listener::Event::new()), runs: Arc::clone(&self.runs) });
            Ok(run)
        })();
        Box::pin(async move { out })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["host"]
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Sandbox` conformance suite runs against: launches for programs that do what each case
/// needs, in this sandbox's terms.
pub trait SandboxFixture: Send + Sync {
    fn sandbox(&self) -> &dyn Sandbox;
    /// Writes `text` to stdout and exits 0.
    fn say(&self, text: &str) -> Launch;
    /// Writes back what comes in on stdin; exits 0 once it closes.
    fn echo(&self) -> Launch;
    /// Writes to stderr and exits with `code`.
    fn fail(&self, code: i32) -> Launch;
    /// Runs until killed.
    fn wait(&self) -> Launch;
    /// Uses `bytes` of memory, under a limit of `limit` bytes.
    fn hog(&self, bytes: u64, limit: u64) -> Launch;
    /// Starts `n` processes, under a limit of `limit`, and fails when one is refused it.
    fn spawn(&self, n: u32, limit: u32) -> Launch;
    /// Connects to `host`, with `granted` the hosts it may reach.
    fn connect(&self, host: &str, granted: &[&str]) -> Launch;
    /// Writes its working folder, given `folder`.
    fn pwd(&self, folder: &str) -> Launch;
    /// A folder that exists for a run.
    fn folder(&self) -> String;
}

/// The fixture for a [`SimSandbox`].
#[derive(Default)]
pub struct SimSandboxFixture(pub SimSandbox);

impl SandboxFixture for SimSandboxFixture {
    fn sandbox(&self) -> &dyn Sandbox {
        &self.0
    }
    fn say(&self, text: &str) -> Launch {
        Launch::of("say", &[text])
    }
    fn echo(&self) -> Launch {
        Launch::of("echo", &[])
    }
    fn fail(&self, code: i32) -> Launch {
        Launch::of("fail", &[&code.to_string()])
    }
    fn wait(&self) -> Launch {
        Launch::of("wait", &[])
    }
    fn hog(&self, bytes: u64, limit: u64) -> Launch {
        Launch { limits: Limits { memory_bytes: limit, ..Limits::default() }, ..Launch::of("hog", &[&bytes.to_string()]) }
    }
    fn spawn(&self, n: u32, limit: u32) -> Launch {
        Launch { limits: Limits { processes: limit, ..Limits::default() }, ..Launch::of("spawn", &[&n.to_string()]) }
    }
    fn connect(&self, host: &str, granted: &[&str]) -> Launch {
        Launch { egress: granted.iter().map(|g| (*g).to_owned()).collect(), ..Launch::of("connect", &[host]) }
    }
    fn pwd(&self, folder: &str) -> Launch {
        Launch { folder: Some(folder.to_owned()), ..Launch::of("pwd", &[]) }
    }
    fn folder(&self) -> String {
        "/work/conformance".into()
    }
}

/// Everything a run says until its end (at most 1000 outputs).
async fn drain(run: &dyn Run) -> Vec<Output> {
    let mut out = Vec::new();
    while out.len() < 1000 {
        match run.next().await {
            Some(o) => out.push(o),
            None => break,
        }
    }
    out
}

fn stdout(outs: &[Output]) -> Vec<u8> {
    outs.iter().filter_map(|o| if let Output::Stdout(b) = o { Some(b.clone()) } else { None }).flatten().collect()
}

fn end(outs: &[Output]) -> Option<Exit> {
    match outs.last() {
        Some(Output::Exited(e)) => Some(*e),
        _ => None,
    }
}

fn c1<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.say("hello there")).await)?;
        let outs = drain(run.as_ref()).await;
        same("its stdout", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), "hello there".to_owned())?;
        same("its end", end(&outs), Some(Exit::Code(0)))?;
        same("after its end", run.next().await, None)
    })
}

fn c2<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.echo()).await)?;
        for line in ["one\n", "two\n", "three\n"] {
            fine("stdin", run.stdin(line.as_bytes().to_vec()).await)?;
        }
        run.close_stdin();
        let outs = drain(run.as_ref()).await;
        same("what came back", String::from_utf8_lossy(&stdout(&outs)).into_owned(), "one\ntwo\nthree\n".to_owned())?;
        same("its end once stdin closed", end(&outs), Some(Exit::Code(0)))
    })
}

fn c3<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.fail(3)).await)?;
        let outs = drain(run.as_ref()).await;
        ensure(outs.iter().any(|o| matches!(o, Output::Stderr(b) if !b.is_empty())), || "nothing on stderr".into())?;
        same("its end", end(&outs), Some(Exit::Code(3)))?;
        same("ends said", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)
    })
}

fn c4<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        run.kill();
        let outs = drain(run.as_ref()).await;
        same("its end", end(&outs), Some(Exit::Killed))?;
        ensure(run.stdin(b"late".to_vec()).await.is_err(), || "stdin to an ended run was taken".into())
    })
}

fn c5<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let over = fine("launch", f.sandbox().launch(f.hog(64 << 20, 16 << 20)).await)?;
        same("over its memory limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Memory)))?;
        let under = fine("launch", f.sandbox().launch(f.hog(4 << 20, 64 << 20)).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c6<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let over = fine("launch", f.sandbox().launch(f.spawn(20, 8)).await)?;
        same("over its process limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Processes)))?;
        let under = fine("launch", f.sandbox().launch(f.spawn(2, 8)).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c7<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let denied = fine("launch", f.sandbox().launch(f.connect("example.com", &["api.anthropic.com"])).await)?;
        ensure(!matches!(end(&drain(denied.as_ref()).await), Some(Exit::Code(0))), || "a host not granted was reached".into())?;
        let granted = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", &["api.anthropic.com"])).await)?;
        same("a granted host", end(&drain(granted.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c8<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let mut l = f.say("x");
        l.program = "conformance-no-such-program".into();
        match f.sandbox().launch(l).await {
            Err(Refusal::NoSuchProgram(_)) => Ok(()),
            Err(e) => Err(format!("refused as {e:?}, not NoSuchProgram")),
            Ok(_) => Err("a program that doesn't exist was launched".into()),
        }
    })
}

fn c10<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        for (cpu, io) in [(0, 100), (100, 0), (WEIGHT_MAX.saturating_add(1), 100), (100, WEIGHT_MAX.saturating_add(1))] {
            let mut l = f.say("x");
            l.limits.cpu_weight = cpu;
            l.limits.io_weight = io;
            match f.sandbox().launch(l).await {
                Err(Refusal::Invalid { .. }) => {}
                Err(e) => return Err(format!("weights {cpu}/{io} refused as {e:?}, not Invalid")),
                Ok(_) => return Err(format!("weights {cpu}/{io} were taken")),
            }
        }
        let mut l = f.say("weighted");
        (l.limits.cpu_weight, l.limits.io_weight) = (WEIGHT_MIN, WEIGHT_MAX);
        let run = fine("a launch at the weights' ends", f.sandbox().launch(l).await)?;
        same("its end", end(&drain(run.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c9<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let folder = f.folder();
        let run = fine("launch", f.sandbox().launch(f.pwd(&folder)).await)?;
        let outs = drain(run.as_ref()).await;
        same("its working folder", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), folder)
    })
}

/// The `Sandbox` contract, clause by clause (docs/seams.md, "Sandbox").
pub fn cases<F: SandboxFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SANDBOX-1", says: "what a run writes reaches its stdout in order, then its end, then nothing", check: c1::<F> },
        Case { id: "SANDBOX-2", says: "stdin reaches the program in order, and closing it is the end of its input", check: c2::<F> },
        Case { id: "SANDBOX-3", says: "a run's stderr and exit code are said as they are, and its end comes once, last", check: c3::<F> },
        Case { id: "SANDBOX-4", says: "a run killed ends Killed, and takes no more input", check: c4::<F> },
        Case { id: "SANDBOX-5", says: "a run over its memory limit is ended OverLimit(Memory); under it, it runs to its end", check: c5::<F> },
        Case { id: "SANDBOX-6", says: "a run refused processes past its process limit, that then fails, ends OverLimit(Processes); under it, it runs to its end", check: c6::<F> },
        Case { id: "SANDBOX-7", says: "a run reaches only the hosts it's granted", check: c7::<F> },
        Case { id: "SANDBOX-8", says: "a launch of a program that doesn't exist is refused NoSuchProgram", check: c8::<F> },
        Case { id: "SANDBOX-9", says: "a run works in the folder it's given", check: c9::<F> },
        Case { id: "SANDBOX-10", says: "cpu and io weights outside cgroup v2's 1 to 10,000 are refused Invalid; at its ends a run goes ahead", check: c10::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_sandbox_passes_the_sandbox_suite() -> Result<(), String> {
        futures::executor::block_on(run("Sandbox", &cases::<SimSandboxFixture>(), || async { SimSandboxFixture::default() })).into_result().map(|_| ())
    }

    /// CP-M2, AC-M2.2: the simulator, bounded, refusing typed past its bounds.
    #[test]
    fn a_microvm_is_refused_as_unsupported_and_runs_are_bounded() {
        let s = SimSandbox::new();
        let vm = Launch { boundary: Boundary::MicroVm { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(vm)), Err(Refusal::Unsupported(_))));
        let container = Launch { boundary: Boundary::Container { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(container)), Err(Refusal::Unsupported(_))));
        let runs: Vec<_> = (0..SIM_RUNS).filter_map(|_| futures::executor::block_on(s.launch(Launch::of("wait", &[]))).ok()).collect();
        assert_eq!(runs.len(), SIM_RUNS);
        assert!(matches!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))), Err(Refusal::Full { .. })));
        drop(runs);
        assert!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))).is_ok(), "an ended run frees its place");
    }
}
