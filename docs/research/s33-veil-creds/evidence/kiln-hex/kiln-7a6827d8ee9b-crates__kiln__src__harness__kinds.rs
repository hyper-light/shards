//! The registry of harnesses kiln can run as structured sessions, and the adapter each one uses.
//! Adding a harness is adding an entry here and, if it speaks a protocol kiln does not know yet,
//! an adapter. `verified` says whether kiln has been tested against a real install of it.

use std::process::Command as Process;

use super::acp::Acp;
use super::antigravity::Agy;
use super::claude::Claude;
use super::codex::Codex;
use super::core::Core;
use super::model::{Command, Delivery, QueueItem, SendMode};
use super::pi::Pi;
use super::session::Spec;

#[cfg(test)]
mod delivery_tests;

pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    /// The executable looked up on PATH.
    pub program: &'static str,
    /// Tested end to end against a real install.
    pub verified: bool,
    /// How kiln talks to it.
    pub protocol: Protocol,
    /// Its own terminal interface: the program and arguments that open it.
    pub screen: &'static [&'static str],
    /// That interface starts on a slash command given as its first argument (`claude /doctor`).
    pub screen_takes_command: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Claude Code's stream-json + control protocol.
    ClaudeStreamJson,
    /// Codex's app-server (JSON-RPC v2).
    CodexAppServer,
    /// The Agent Client Protocol, started with these arguments.
    Acp(&'static [&'static str]),
    /// Antigravity's headless multi-turn stream-json (`agy --input-format stream-json`).
    AgyStreamJson,
    /// Pi's RPC mode (`pi --mode rpc`): JSON commands on stdin, responses and events on stdout.
    PiRpc,
    /// No structured mode: it runs as a terminal program, in a kiln terminal.
    Terminal,
}

const KINDS: &[Kind] = &[
    Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson, screen: &["claude"], screen_takes_command: true },
    Kind { id: "codex", name: "Codex", program: "codex", verified: true, protocol: Protocol::CodexAppServer, screen: &["codex"], screen_takes_command: false },
    Kind { id: "devin", name: "Devin", program: "devin", verified: true, protocol: Protocol::Acp(&["acp"]), screen: &["devin"], screen_takes_command: false },
    Kind { id: "gemini", name: "Gemini CLI", program: "gemini", verified: false, protocol: Protocol::Acp(&["--acp"]), screen: &["gemini"], screen_takes_command: false },
    Kind { id: "opencode", name: "opencode", program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["opencode"], screen_takes_command: false },
    Kind { id: "goose", name: "Goose", program: "goose", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["goose", "session"], screen_takes_command: false },
    // Amp has no ACP mode of its own; its ACP adapter (amp-acp) wraps the signed-in Amp CLI.
    Kind { id: "amp", name: "Amp", program: "amp-acp", verified: false, protocol: Protocol::Acp(&[]), screen: &["amp"], screen_takes_command: false },
    Kind { id: "antigravity", name: "Antigravity CLI", program: "agy", verified: true, protocol: Protocol::AgyStreamJson, screen: &["agy"], screen_takes_command: false },
    // Cursor's CLI agent: `agent acp`, its ACP server (cursor.com/docs/cli/acp); its installer
    // puts `agent` (and the older name, `cursor-agent`) in ~/.local/bin.
    Kind { id: "cursor", name: "Cursor CLI", program: "agent", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["agent"], screen_takes_command: false },
    // Pi (pi.dev): its RPC mode (`pi --mode rpc`).
    Kind { id: "pi", name: "Pi", program: "pi", verified: false, protocol: Protocol::PiRpc, screen: &["pi"], screen_takes_command: false },
];

impl Kind {
    pub fn all() -> &'static [Kind] {
        KINDS
    }

    pub fn get(id: &str) -> Option<&'static Kind> {
        KINDS.iter().find(|k| k.id == id)
    }

    /// Installed on this machine.
    pub fn available(&self) -> bool {
        self.find().is_some()
    }

    /// Its executable, on [`search_path`].
    pub fn find(&self) -> Option<std::path::PathBuf> {
        crate::os::which_in(self.program, &search_path()?)
    }

    /// kiln can run it as a structured session (else only as a terminal program).
    pub fn structured(&self) -> bool {
        self.protocol != Protocol::Terminal
    }

    pub fn adapter(&self, spec: &Spec) -> Adapter {
        match self.protocol {
            Protocol::ClaudeStreamJson => Adapter::Claude(Box::new(Claude::new(spec.allow_bypass).with_config_dir(claude_config_dir(spec)).with_speed(spec.speed.clone()))),
            Protocol::CodexAppServer => Adapter::Codex(Box::new(Codex::from_spec(spec))),
            Protocol::AgyStreamJson => Adapter::Agy(Box::new(Agy::new(spec))),
            Protocol::PiRpc => Adapter::Pi(Box::new(Pi::new(spec))),
            Protocol::Acp(_) | Protocol::Terminal => Adapter::Acp(Box::new(Acp::from_spec(spec))),
        }
    }

    /// The process to start for session `session` of `spec` (cwd, environment and arguments).
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn launch(&self, spec: &Spec, session: &str) -> anyhow::Result<Process> {
        if self.protocol == Protocol::Terminal {
            anyhow::bail!("{} has no structured mode; run it in a terminal ({})", self.name, self.program);
        }
        let program = match &spec.program {
            Some(p) => std::path::PathBuf::from(p),
            None => self.find().ok_or_else(|| anyhow::anyhow!("{} ({}) is not installed on this machine", self.name, self.program))?,
        };
        let mut p = Process::new(program);
        let cwd = super::session::resolve_cwd(spec.cwd.as_deref());
        if !std::path::Path::new(&cwd).is_dir() {
            anyhow::bail!("{cwd} is not a directory on this machine");
        }
        p.current_dir(&cwd);
        set_env(&mut p, spec);
        match self.protocol {
            Protocol::ClaudeStreamJson => {
                let session_id = spec.resume.is_none().then(|| uuid::Uuid::now_v7().to_string());
                p.args(super::claude::args(spec, session_id.as_deref()));
                // kiln's MCP server: on a runner, its own (code mode over the kiln's machines and
                // kiln's operations, through the runner); else the server's, where this machine's
                // kiln serves it.
                let local = local_tools(spec);
                match (&local, crate::mcp::kit()) {
                    #[cfg(unix)]
                    (Some((kiln, sock, prefer)), _) => {
                        p.arg("--mcp-config").arg(crate::reach::mcp::claude_config(kiln, sock, *prefer, session));
                    }
                    // Its own token: kiln knows which agent calls, and whom it acts for.
                    (_, Some(kit)) => {
                        p.arg("--mcp-config").arg(kit.config_for(session));
                    }
                    _ => {}
                }
                // kiln's skills.
                if let Some(dir) = crate::mcp::kit().map(|k| k.plugin_dir.as_path()).or_else(crate::mcp::plugin_dir) {
                    p.arg("--plugin-dir").arg(dir);
                }
                // The skills kiln installed for it (docs/agents.md, "Skills kiln installs"): a folder
                // of plugins its machine fills as they're laid out, loaded again as they change.
                if let Some(dir) = crate::skills::place::session_folder(session) {
                    p.arg("--plugin-dir").arg(dir);
                }
                p.envs(super::claude::FIXED_ENV.iter().copied());
            }
            // Model, mode and resume go over the protocol (thread/start, turn/start).
            Protocol::CodexAppServer => {
                p.args(super::codex::args());
                // The most subagents it runs at once (its Agent settings), as Codex's own limit.
                if let Some(n) = spec.max_subagents {
                    p.arg("-c").arg(format!("agents.max_concurrent_threads_per_session={n}"));
                }
                #[cfg(unix)]
                if let Some((kiln, sock, prefer)) = local_tools(spec) {
                    p.args(crate::reach::mcp::codex_config(&kiln, sock, prefer, session));
                }
                // kiln's hook before each tool call, trusted for this session alone: what holds a
                // subagent (a pause, what it may do) holds its calls (docs/agents.md). It asks this
                // runner process, which runs the session, else the machine's socket.
                #[cfg(unix)]
                if let (Some(sock), Ok(kiln)) = (super::socket::own_served().or_else(super::socket::served), crate::os::this_kiln()) {
                    p.args(super::codex::hook_args(&kiln, sock));
                }
            }
            Protocol::Acp(args) => {
                p.args(args);
                // Cursor's sign-in (its ACP `authenticate`, `agent login`) opens no browser on the
                // machine: its link is shown wherever the person is (`crate::accounts`).
                if self.id == "cursor" {
                    p.env("NO_OPEN_BROWSER", "1");
                }
            }
            // Model, effort, mode and the conversation are flags: a change starts it again.
            Protocol::AgyStreamJson => {
                p.args(super::antigravity::args(spec));
                // kiln's MCP server, which `prepare` put in agy's own config: whom it serves is
                // this session, by agy's environment, which agy hands on to the servers it starts.
                #[cfg(unix)]
                if let Some((_, sock, prefer)) = local_tools(spec) {
                    p.envs(crate::reach::mcp::agy_env(sock, prefer, session));
                }
            }
            // Its session, model, thinking level and mode's tools are flags (`pi::args`).
            Protocol::PiRpc => {
                p.args(super::pi::args(spec));
            }
            Protocol::Terminal => {}
        }
        Ok(p)
    }
}

impl Kind {
    /// The harness's own CLI with these arguments (`mcp add ...`), run the way the session runs
    /// the harness: same executable, directory and environment, so it changes the configuration
    /// the session uses. Its program is the session's own when the spec names one.
    pub fn cli(&self, spec: &Spec, args: &[String]) -> anyhow::Result<Process> {
        if !self.structured() {
            anyhow::bail!("{} has no command line kiln can run", self.name);
        }
        let program = match &spec.program {
            Some(p) => std::path::PathBuf::from(p),
            None => self.find().ok_or_else(|| anyhow::anyhow!("{} ({}) is not installed on this machine", self.name, self.program))?,
        };
        let mut p = Process::new(program);
        let cwd = spec.cwd.clone().unwrap_or_else(|| crate::os::home_dir().to_string_lossy().into_owned());
        if std::path::Path::new(&cwd).is_dir() {
            p.current_dir(&cwd);
        }
        set_env(&mut p, spec);
        // Nothing it runs from here may open a browser on the machine's desktop (a sign-in is
        // kiln's to show, wherever the person is).
        p.env("BROWSER", "true");
        p.args(args);
        p.stdin(std::process::Stdio::null());
        Ok(p)
    }

    /// What a session's start needs before its harness starts: kiln's MCP server put in
    /// Antigravity's own config, the only place agy reads one from (`crate::reach::mcp::agy_add`),
    /// unless it's there already or a person turned it off; and its page tools allowed in agy's
    /// settings, as agy can't ask once started (`agy_allow_in`). It names this kiln, so a runner
    /// upgraded since puts its own. A session whose server couldn't be put there starts without it.
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub async fn prepare(&self, spec: &Spec) {
        #[cfg(unix)]
        if self.protocol == Protocol::AgyStreamJson
            && let Some((kiln, _, _)) = local_tools(spec)
        {
            let home = crate::os::home_dir();
            let config = home.join(".gemini/config/mcp_config.json");
            let k = kiln.clone();
            let has = crate::rt::blocking(move || std::fs::read(&config).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|c| crate::reach::mcp::agy_has(&c, &k)));
            if !has.await.unwrap_or(false) {
                let added = match self.cli(spec, &crate::reach::mcp::agy_add(&kiln)) {
                    Ok(cmd) => crate::proc::output(cmd, AGY_ADD_TIMEOUT).await.and_then(|o| if o.status.success() { Ok(()) } else { Err(anyhow::anyhow!("{}", String::from_utf8_lossy(&o.stderr).trim())) }),
                    Err(e) => Err(e),
                };
                if let Err(e) = added {
                    tracing::warn!(error = %format!("{e:#}"), "kiln's MCP server couldn't be put in Antigravity's config: its agents start without kiln's tools");
                }
            }
            let settings = home.join(".gemini/antigravity-cli/settings.json");
            if let Err(e) = crate::rt::blocking(move || agy_allow_in(&settings)).await.and_then(|r| r) {
                tracing::warn!(error = %format!("{e:#}"), "kiln's page tools couldn't be allowed in Antigravity's settings: agy decides on them as on its other tools");
            }
        }
        // kiln's hook in agy's own hooks.json, the only place agy reads hooks from: every
        // conversation's tool calls, model calls and stops come to kiln, so a person's pause and
        // what each subagent may do hold them (docs/agents.md). One agy runs outside kiln passes.
        #[cfg(unix)]
        if self.protocol == Protocol::AgyStreamJson
            && let Ok(kiln) = crate::os::this_kiln()
        {
            let hooks = crate::os::home_dir().join(".gemini/config/hooks.json");
            if let Err(e) = crate::rt::blocking(move || agy_hooks_in(&hooks, &kiln.to_string_lossy())).await.and_then(|r| r) {
                tracing::warn!(error = %format!("{e:#}"), "kiln's hook couldn't be put in Antigravity's hooks.json: its subagents can't be paused or limited");
            }
        }
    }
}

/// How long `agy mcp add` may take, putting kiln's MCP server in Antigravity's config.
#[cfg(unix)]
const AGY_ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// kiln's page tools (`crate::reach::mcp::AGY_ALLOWED`) allowed in agy's settings at `path` where
/// no rule covers them already (`crate::reach::mcp::agy_allow`): its other settings kept, written
/// whole and readable by its user alone, as agy keeps it. Whether it wrote.
#[cfg(unix)]
fn agy_allow_in(path: &std::path::Path) -> anyhow::Result<bool> {
    let settings = match std::fs::read(path) {
        Ok(b) => serde_json::from_slice::<serde_json::Value>(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };
    let Some(next) = crate::reach::mcp::agy_allow(&settings) else { return Ok(false) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::runner::auth::write_private(path, &serde_json::to_vec_pretty(&next)?)?;
    Ok(true)
}

/// kiln's hook entry for this kiln binary (`kiln`) in agy's hooks.json at `path`
/// (`antigravity::hooks_with`): its other hooks kept, written whole and readable by its user alone,
/// only when it differs; a file it can't read is left as it is. Whether it wrote.
#[cfg(unix)]
fn agy_hooks_in(path: &std::path::Path, kiln: &str) -> anyhow::Result<bool> {
    let config = match std::fs::read(path) {
        Ok(b) => serde_json::from_slice::<serde_json::Value>(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };
    let Some(next) = super::antigravity::hooks_with(&config, kiln) else { return Ok(false) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::runner::auth::write_private(path, &serde_json::to_vec_pretty(&next)?)?;
    Ok(true)
}

/// Whether a session gets the runner's own kiln MCP server (`crate::reach::mcp`), and how to
/// start it: this kiln binary, the runner's reach socket, the session's preferred runner.
#[cfg(unix)]
fn local_tools(spec: &Spec) -> Option<(std::path::PathBuf, &'static str, Option<&str>)> {
    let pref = spec.reach.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if pref == Some("off") {
        return None;
    }
    let sock = crate::reach::local::socket()?;
    let kiln = crate::os::this_kiln().ok()?;
    Some((kiln, sock, pref))
}

/// Without Unix sockets there is no runner of the machine to talk to.
#[cfg(not(unix))]
fn local_tools(_spec: &Spec) -> Option<(std::path::PathBuf, &'static str, Option<&str>)> {
    None
}

/// The MCP servers an ACP agent's session gets (`session/new`'s `mcpServers`): the runner's own.
pub fn acp_servers(spec: &Spec, session: &str) -> Vec<serde_json::Value> {
    #[cfg(unix)]
    if let Some((kiln, sock, prefer)) = local_tools(spec) {
        return vec![crate::reach::mcp::acp_config(&kiln, sock, prefer, session)];
    }
    let _ = (spec, session);
    Vec::new()
}

/// Where harnesses are looked for, and the PATH they run with: this process's, then the folders
/// only its user's login shell has (`crate::reach::serve::login_env`, which a runner asks as it
/// starts and again as one of that shell's startup files changes, [`changed`]). A runner's service keeps the PATH of the terminal it was
/// installed from: a harness installed since in a folder that PATH lacks (Claude Code's and
/// Codex's installers put theirs in `~/.local/bin`) is on its user's PATH alone. CI legs' steps
/// run with it too, and the runner's tool facts are found on it (docs/ci.md W13-R01, W13-R03): a
/// toolchain installed since (node from nvm, cargo from rustup) is on its user's PATH alone.
pub fn search_path() -> Option<std::ffi::OsString> {
    let users = crate::reach::serve::login_path();
    joined_path(std::env::var_os("PATH").as_deref(), users.as_deref().map(std::ffi::OsStr::new))
}

// --------------------------------------------------------------------- what tells it changed

/// Told when what [`offered`] finds may have changed (goal 9): a folder of [`search_path`] changed
/// (a harness or a tool installed, upgraded or removed), the login shell's PATH was asked again
/// (one of its startup files changed: [`shell_files`]), or a harness's sessions listed new models
/// ([`note_models`]).
static CHANGED: std::sync::LazyLock<crate::rt::Notify> = std::sync::LazyLock::new(crate::rt::Notify::new);

/// Told when a folder of [`search_path`] changed, or the search path itself did (the login
/// shell's PATH asked again and found otherwise): what the runner's facts find their tools on
/// (`crate::runner::probe`, W13-R01).
static PATH: std::sync::LazyLock<crate::rt::Notify> = std::sync::LazyLock::new(crate::rt::Notify::new);

/// A folder of [`search_path`] changed, or the search path did, since the last wake taken
/// (W13-R01; goal 9): waits for the next. Starts the watches as [`changed`] does.
pub async fn path_changed() {
    watching();
    PATH.notified().await;
}

/// What [`offered`] finds may have changed since the last wake taken (goal 9; it was looked at
/// again every minute, and the login shell asked again every ten): waits for the next, and a
/// change between waits counts at the next. The first call starts the watches ([`watching`]),
/// which ask the login shell for its environment as they start, and tell once as their folders
/// stand then.
pub async fn changed() {
    watching();
    CHANGED.notified().await;
}

/// Starts, once, what tells [`changed`]: a watch on the login shell's startup files, and on the
/// folders of [`search_path`] as that shell's PATH stands, watched again as it's asked again.
/// `crate::reach::serve::login_env` starts it too, so whatever asks the login shell hears its
/// files change.
pub fn watching() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        drop(crate::rt::spawn(watch()));
    }
}

async fn watch() {
    use futures::FutureExt as _;
    let (named, names, whole) = shell_files();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (files, folders) = (crate::os::watch::Watched::new(&named, false, &names), crate::os::watch::Watched::new(&whole, false, &[]));
    let mut was: Option<std::ffi::OsString> = None;
    loop {
        crate::reach::serve::login_env().await;
        let now = search_path();
        let dirs: Vec<std::path::PathBuf> = now.as_ref().map(|p| std::env::split_paths(p).filter(|d| d.is_absolute()).collect()).unwrap_or_default();
        let path = crate::os::watch::Watched::new(&dirs, false, &[]);
        // What it offers with the search path as it stands now (the login shell's PATH known, or
        // known anew); the facts again only when the path moved (they were probed on it).
        CHANGED.notify();
        if was.is_some() && was != now {
            PATH.notify();
        }
        was = now;
        loop {
            futures::select_biased! {
                () = path.changed().fuse() => {
                    CHANGED.notify();
                    PATH.notify();
                }
                () = files.changed().fuse() => break,
                () = folders.changed().fuse() => break,
            }
        }
        // A startup file changed: the shell is asked again, and its PATH's folders watched anew.
        crate::reach::serve::login_stale();
    }
}

/// What a login shell reads as it starts, where its PATH is made: each folder of named files
/// with their names, and the folders read whole (`paths.d`, `profile.d`, fish's `conf.d`). The
/// startup files of sh, bash, zsh and fish, the user's and the system's; none on Windows, whose
/// runners take their PATH from their own environment.
fn shell_files() -> (Vec<std::path::PathBuf>, Vec<String>, Vec<std::path::PathBuf>) {
    if cfg!(windows) {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let home = crate::os::home_dir();
    let mut named = vec![home.clone(), home.join(".config").join("fish"), std::path::PathBuf::from("/etc"), std::path::PathBuf::from("/etc/zsh"), std::path::PathBuf::from("/etc/fish")];
    if let Some(z) = std::env::var_os("ZDOTDIR").map(std::path::PathBuf::from).filter(|z| z.is_absolute()) {
        named.push(z);
    }
    let names = [".profile", ".bash_profile", ".bash_login", ".bashrc", ".zshenv", ".zprofile", ".zshrc", ".zlogin", "config.fish", "profile", "environment", "paths", "bashrc", "bash.bashrc", "zshenv", "zprofile", "zshrc", "zlogin"];
    let whole = vec![std::path::PathBuf::from("/etc/paths.d"), std::path::PathBuf::from("/etc/profile.d"), home.join(".config").join("fish").join("conf.d")];
    (named, names.iter().map(|n| (*n).to_owned()).collect(), whole)
}

/// `first`'s folders, then the whole paths of `then` it hasn't (a relative one would be the
/// runner's folder's), as one PATH (none without a folder).
fn joined_path(first: Option<&std::ffi::OsStr>, then: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
    let mut dirs: Vec<std::path::PathBuf> = first.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    for dir in then.into_iter().flat_map(|p| std::env::split_paths(p).collect::<Vec<_>>()) {
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).ok().filter(|p| !p.is_empty())
}

/// The session's environment over the machine's: what its shell had (minus the shell's own
/// terminal plumbing), with no `KILN_*` variable of either but those kiln gives it, and without what
/// a Claude Code session sets for its children.
fn set_env(p: &mut Process, spec: &Spec) {
    // kiln's own variables are the server's or the runner's that starts it (its data folder, its
    // config, its supervisor's socket, its admin token), or those of the shell `kiln spawn` took the
    // environment of, which a kiln terminal has from the server: none goes on to the agent, which
    // gets its own from kiln (its sockets here, its mark and its tools' as it starts). A test server
    // an agent started opened production's database through its KILN_DATA_DIR (2026-11-06).
    for (k, _) in std::env::vars_os() {
        if kilns(&k) {
            p.env_remove(k);
        }
    }
    // The PATH it was found on, so what it starts (its interpreter, its tools) is found too.
    if let Some(path) = search_path() {
        p.env("PATH", path);
    }
    for (k, v) in &spec.env {
        if !kilns(std::ffi::OsStr::new(k)) && !matches!(k.as_str(), "TERM" | "COLUMNS" | "LINES") {
            p.env(k, v);
        }
    }
    // `kiln run` in the agent's shell reaches other runners through this machine's runner.
    if let Some(sock) = crate::reach::local::socket() {
        p.env("KILN_REACH_SOCK", sock);
    }
    // `kiln spawn` and `kiln agents` in its commands: what it starts are its children (the socket
    // knows the agent by its process).
    if let Some(sock) = super::socket::served() {
        p.env("KILN_HARNESS_SOCK", sock);
    }
    // What it asks goes to this process, which runs it and holds its key (W27-R03), whichever
    // process of the machine serves the machine's socket.
    if let Some(sock) = super::socket::own_served() {
        p.env("KILN_RUNNER_SOCK", sock);
    }
    for k in super::claude::SCRUB_ENV {
        p.env_remove(k);
    }
}

/// Whether `name` is one of kiln's own variables (`KILN_*`).
fn kilns(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b"KILN_")
}

/// Where Claude Code keeps its configuration for this session: `CLAUDE_CONFIG_DIR` from the
/// session's environment or the machine's, else `~/.claude`.
fn claude_config_dir(spec: &Spec) -> String {
    spec.env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| crate::os::home_dir().join(".claude").to_string_lossy().into_owned())
}

/// A harness adapter: turns the harness's output into the normalized model and commands into
/// its input. Pure state machines over [`Core`]; the session owns the process.
pub enum Adapter {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
    Acp(Box<Acp>),
    Agy(Box<Agy>),
    Pi(Box<Pi>),
}

/// An adapter written out whole, for this process's next image (an in-place upgrade).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "adapter", content = "state", rename_all = "snake_case")]
pub enum Frozen {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
    Acp(Box<Acp>),
    Agy(Box<Agy>),
    Pi(Box<Pi>),
}

impl Adapter {
    /// Actual retained provider requests during live-process adoption. Presentation delivery
    /// maps are not evidence of a native flight, and a new provider starts with none.
    pub(crate) fn retained_input_ids(&self) -> Vec<super::admission::LegacyFlight> {
        match self { Self::Claude(adapter) => adapter.retained_input_ids(), Self::Codex(adapter) => adapter.retained_input_ids(), Self::Acp(adapter) => adapter.retained_input_ids(), Self::Agy(adapter) => adapter.retained_input_ids(), Self::Pi(adapter) => adapter.retained_input_ids() }
    }
    /// The MCP servers a session it opens gets, for an adapter that gives them over its protocol
    /// (ACP); the others get theirs as they start ([`Kind::launch`]).
    pub fn offer_mcp(&mut self, servers: Vec<serde_json::Value>) {
        if let Self::Acp(a) = self {
            a.offer_mcp(servers);
        }
    }

    /// Its state, whole, to go on with in the next image ([`Adapter::thaw`]).
    pub fn freeze(self) -> Frozen {
        match self {
            Self::Claude(a) => Frozen::Claude(a),
            Self::Codex(a) => Frozen::Codex(a),
            Self::Acp(a) => Frozen::Acp(a),
            Self::Agy(a) => Frozen::Agy(a),
            Self::Pi(a) => Frozen::Pi(a),
        }
    }

    pub fn thaw(f: Frozen) -> Self {
        match f {
            Frozen::Claude(a) => Self::Claude(a),
            Frozen::Codex(a) => Self::Codex(a),
            Frozen::Acp(a) => Self::Acp(a),
            Frozen::Agy(a) => Self::Agy(a),
            Frozen::Pi(a) => Self::Pi(a),
        }
    }

    pub fn start(&mut self, core: &mut Core) {
        // A new process: nothing an earlier one ran, in this harness or the one before a switch,
        // goes on under it.
        core.end_harness_work();
        match self {
            Self::Claude(a) => a.start(core),
            Self::Codex(a) => a.start(core),
            Self::Acp(a) => a.start(core),
            Self::Agy(a) => a.start(core),
            Self::Pi(a) => a.start(core),
        }
        self.settle_subagents(core);
    }

    pub fn line(&mut self, core: &mut Core, line: &str) {
        match self {
            Self::Claude(a) => a.line(core, line),
            Self::Codex(a) => a.line(core, line),
            Self::Acp(a) => a.line(core, line),
            Self::Agy(a) => a.line(core, line),
            Self::Pi(a) => a.line(core, line),
        }
        self.settle_subagents(core);
    }

    pub fn command(&mut self, core: &mut Core, c: Command) {
        core.shown_queued(&c);
        // A message waiting for one of its subagents is changed where it waits, or goes to it now.
        match super::subagent::queue_command(core, c) {
            super::subagent::Queued::Done => {}
            super::subagent::Queued::Send(task, m, mode) => self.send_task(core, &task, m, mode),
            super::subagent::Queued::Not(Command::Send { id, text, mode, attachments, reply_to, task: Some(task), .. }) => {
                self.send_task(core, &task, QueueItem { id, text, at: Core::now(), attachments, overrides: None, reply_to }, mode);
            }
            super::subagent::Queued::Not(c) => match self {
                Self::Claude(a) => a.command(core, c),
                Self::Codex(a) => a.command(core, c),
                Self::Acp(a) => a.command(core, c),
                Self::Agy(a) => a.command(core, c),
                Self::Pi(a) => a.command(core, c),
            },
        }
        self.settle_subagents(core);
    }

    /// A person's message to its subagent `task`, the way that one takes one now (`Task::takes`),
    /// through its harness's adapter; one it can't take is shown not sent, and why.
    fn send_task(&mut self, core: &mut Core, task: &str, mut m: QueueItem, asked: SendMode) {
        m.text = m.text.trim_end().to_owned();
        if m.text.is_empty() && m.attachments.is_empty() {
            return;
        }
        let Some(t) = core.state.tasks.iter().find(|t| t.id == task).cloned() else {
            core.state.delivery.insert(m.id.clone(), super::model::Delivery::Cancelled);
            core.upsert(&format!("subagent-unsent-{}", m.id), None, super::model::Body::Notice { level: super::model::Level::Warn, text: "Not sent: kiln no longer knows that subagent.".to_owned() });
            return;
        };
        let Some(mode) = super::subagent::mode_for(&t, asked) else {
            let why = if t.subagent() && !t.live() && t.status != "completed" { "it has ended." } else { "its harness takes no message for it from kiln." };
            super::subagent::refuse(core, task, &m, why);
            return;
        };
        match self {
            Self::Claude(a) => a.send_task(core, &t, m, mode),
            Self::Codex(a) => a.send_task(core, &t, m, mode),
            Self::Acp(a) => a.send_task(core, &t, m),
            Self::Agy(a) => a.send_task(core, &t, m, mode),
            Self::Pi(a) => a.send_task(core, &t, m),
        }
    }

    /// How a message reaches each of its subagents now (`Task::takes`, `Task::via`), as its harness
    /// allows; what waited for one that takes none any more is said not sent.
    fn settle_subagents(&self, core: &mut Core) {
        match self {
            Self::Claude(a) => super::subagent::settle(core, |core, t| a.takes(core, t)),
            Self::Codex(a) => super::subagent::settle(core, |core, t| a.takes(core, t)),
            Self::Acp(a) => super::subagent::settle(core, |core, t| a.takes(core, t)),
            Self::Agy(a) => super::subagent::settle(core, |core, t| a.takes(core, t)),
            Self::Pi(a) => super::subagent::settle(core, |core, t| a.takes(core, t)),
        }
        // Held at its next tool call: Claude Code asks kiln before each (`claude::HOLD_HOOK`), and so
        // does Codex once it trusts kiln's hook (`codex::hook_args`).
        let can = match self {
            Self::Claude(_) => true,
            Self::Codex(a) => a.holds_calls(),
            // At each call an ACP agent asks about, where it names its subagent (opencode's).
            Self::Acp(_) => true,
            // And Antigravity, once kiln's hook in its hooks.json is heard (`antigravity::hooks_with`).
            Self::Agy(a) => a.holds_calls(),
            // Pi has no subagents.
            Self::Pi(_) => false,
        };
        super::subagent::settle_pausable(core, |t| can && t.tool_id.is_some());
        self.note_delivery(core);
    }

    /// What its adapter holds till its harness is ready (Codex's thread, an ACP agent's session).
    fn held(&self) -> &[QueueItem] {
        match self {
            Self::Codex(a) => a.held(),
            Self::Acp(a) => a.held(),
            Self::Claude(_) | Self::Agy(_) | Self::Pi(_) => &[],
        }
    }

    /// Tells headroom what of the session waits to be delivered now (docs/headroom.md, "Agent
    /// messages"), the same way for every harness: what its harness was handed and hasn't taken (its
    /// delivery `Sent`), what its adapter holds till the harness is ready, and what waits in kiln's
    /// queue while the harness is starting or idle (not up yet, or held). What waits in the queue
    /// while a turn runs, or for a person's answer, waits by design and isn't counted; an exited
    /// harness's isn't either (`Adapter::exited`).
    fn note_delivery(&self, core: &Core) {
        use super::model::{Delivery, Status};
        if core.state.status == Status::Exited {
            crate::headroom::messages::gone(&core.session_id);
            return;
        }
        let sent = core.state.delivery.iter().filter(|(_, d)| **d == Delivery::Sent).map(|(id, _)| id.as_str());
        let held = self.held().iter().map(|q| q.id.as_str());
        let queued = matches!(core.state.status, Status::Starting | Status::Idle).then(|| core.state.queue.iter().map(|q| q.id.as_str())).into_iter().flatten();
        let ids: Vec<&str> = sent.chain(held).chain(queued).collect();
        crate::headroom::messages::waiting(&core.session_id, &ids);
    }

    /// What waits for its subagents goes to each that can take it now (one at a time each).
    fn drain_subagents(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.drain_subagents(core),
            Self::Codex(a) => a.drain_subagents(core),
            Self::Acp(a) => a.drain_subagents(core),
            Self::Agy(_) | Self::Pi(_) => {}
        }
        self.settle_subagents(core);
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        match self {
            Self::Claude(a) => a.exited(core, code, error),
            Self::Codex(a) => a.exited(core, code, error),
            Self::Acp(a) => a.exited(core, code, error),
            Self::Agy(a) => a.exited(core, code, error),
            Self::Pi(a) => a.exited(core, code, error),
        }
        // Whatever the harness and whatever its adapter knew: what it ran ends with its process
        // (a switch, a stop, a restart or a crash), its subagents' and shells' calls with it.
        core.end_harness_work();
        self.settle_subagents(core);
        self.let_go(core);
        // What waited for this harness isn't measured: it waits for the next one, or for nothing.
        crate::headroom::messages::gone(&core.session_id);
    }

    /// Its process is gone, and this adapter goes with it (it ended, was stopped, or starts again,
    /// in this harness or another: a switch, a move, a rewind, a setting): no message it was given
    /// is lost or left on its way (CP-T3.2). What it held for its harness, which never got it (sent
    /// as Codex or an ACP agent started, or to go first as a turn stopped), goes first in kiln's
    /// queue, in order, still shown queued: the next process takes it, whichever harness that is.
    /// One a rewind took out of the conversation goes with it. What its harness was handed and
    /// never finished ends dropped (`Core::drop_unfinished`).
    pub fn let_go(&mut self, core: &mut Core) {
        let held = match self {
            Self::Codex(a) => a.take_held(),
            Self::Acp(a) => a.take_held(),
            Self::Claude(_) | Self::Agy(_) | Self::Pi(_) => Vec::new(),
        };
        let mut changed = false;
        // CP-T3.2: an older adapter and the durable queue can hold the same input after an
        // upgrade. Keep its edited durable copy once, never revive a removal or dispatched input,
        // and keep the rewind's filtering of messages removed from the conversation.
        for m in held.into_iter().rev() {
            let durable = core.state.queue.iter().position(|q| q.id == m.id);
            let removed = core.get(&m.id).is_none() && (core.state.delivery.contains_key(&m.id) || durable.is_none());
            if removed || core.state.delivery.get(&m.id).is_some_and(|d| matches!(d, Delivery::Sent | Delivery::Started | Delivery::Completed | Delivery::Cancelled)) {
                let before = core.state.queue.len();
                core.state.queue.retain(|q| q.id != m.id);
                changed |= core.state.queue.len() != before;
                continue;
            }
            let m = match durable {
                Some(pos) => core.state.queue.remove(pos),
                None => m,
            };
            core.state.queue.retain(|q| q.id != m.id);
            core.state.queue.insert(0, m);
            changed = true;
        }
        if changed {
            core.touch_state();
        }
        core.drop_unfinished(false);
    }

    /// A run (`Effect::Run` or `Effect::Fetch`) ended: its exit code (None: it could not start
    /// or was stopped) and its output (stdout then stderr, shortened).
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        if let Some(at) = job.strip_prefix(super::pause::CHECKPOINT_JOB) {
            super::pause::checkpointed(core, at, code, output);
            return;
        }
        match self {
            Self::Claude(a) => a.run_done(core, job, code, output),
            Self::Codex(a) => a.run_done(core, job, code, output),
            Self::Acp(a) => a.run_done(core, job, code, output),
            Self::Agy(a) => a.run_done(core, job, code, output),
            Self::Pi(a) => a.run_done(core, job, code, output),
        }
    }

    /// What waits in kiln's queue goes on, if it may now (headroom had room for it).
    pub fn drain(&mut self, core: &mut Core) {
        if core.state.queue.is_empty() {
            return;
        }
        match self {
            Self::Claude(a) => a.drain_queue(core),
            Self::Codex(a) => a.next(core),
            Self::Acp(a) => a.next(core),
            Self::Agy(a) => a.next(core),
            Self::Pi(a) => a.next(core),
        }
    }

    /// Every few seconds while the process runs: a chance to poll what the harness only reports
    /// when asked (plan usage, servers still connecting).
    pub fn tick(&mut self, core: &mut Core) {
        // A pause's timer: time to check whether its limit reset (or to stop waiting for the answer).
        let key = core.state.harness_session.clone().unwrap_or_else(|| core.state.cwd.clone());
        match super::pause::step(core, Core::now(), &key) {
            super::pause::Step::Check => self.check_limits(core),
            super::pause::Step::GoOn(p) => self.go_on(core, *p),
            super::pause::Step::Nothing => {}
        }
        match self {
            Self::Claude(a) => a.tick(core),
            Self::Codex(a) => a.tick(core),
            Self::Agy(a) => a.tick(core),
            Self::Acp(a) => a.tick(core),
            Self::Pi(a) => a.tick(core),
        }
        self.drain_subagents(core);
    }

    /// Ask the harness whether a pause's limit has reset (its answer goes to `pause::checked`). One
    /// that can't say goes on, and finds out.
    fn check_limits(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.check_limits(core),
            Self::Codex(a) => a.check_limits(core),
            Self::Acp(a) => {
                if let Some(p) = super::pause::release(core, "The limit's reset time came; going on.") {
                    a.go_on(core, p);
                }
            }
            Self::Agy(a) => {
                if let Some(p) = super::pause::release(core, "The limit's reset time came; going on.") {
                    a.go_on(core, p);
                }
            }
            Self::Pi(a) => {
                if let Some(p) = super::pause::release(core, "The limit's reset time came; going on.") {
                    a.go_on(core, p);
                }
            }
        }
    }

    /// A pause ended (it reset, or a person resumed it): the turn it cut off goes on, then kiln's
    /// queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        match self {
            Self::Claude(a) => a.go_on(core, p),
            Self::Codex(a) => a.go_on(core, p),
            Self::Acp(a) => a.go_on(core, p),
            Self::Agy(a) => a.go_on(core, p),
            Self::Pi(a) => a.go_on(core, p),
        }
        // What waited for its subagents meanwhile goes too.
        self.drain_subagents(core);
    }

    /// There may be room on its machine now for it to go to work (`super::limit`): what waits goes,
    /// as it does once a turn ends, then what waits for its subagents.
    pub fn room(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.room(core),
            Self::Codex(a) => a.room(core),
            Self::Acp(a) => a.room(core),
            Self::Agy(a) => a.room(core),
            Self::Pi(a) => a.room(core),
        }
        self.drain_subagents(core);
    }

    /// What the person lets the session do changed (`Command::Perms` for the whole session): what
    /// only its harness's own settings hold follows (Codex's web search).
    pub fn perms_changed(&mut self, core: &mut Core) {
        if let Self::Codex(a) = self {
            a.perms_changed(core);
        }
    }

    /// Its harness's hook asks before a tool call (`input`): the answer goes to `reply`, at once or
    /// once a person lets the call go (`hold`). Codex's and Antigravity's hooks ask this way; Claude
    /// Code's comes over its own control protocol.
    pub fn hook(&mut self, core: &mut Core, input: serde_json::Value, reply: futures::channel::oneshot::Sender<serde_json::Value>) {
        match self {
            Self::Codex(a) => a.hook(core, input, reply),
            Self::Agy(a) => a.hook(core, input, reply),
            Self::Claude(_) | Self::Acp(_) | Self::Pi(_) => {
                let _ = reply.send(serde_json::json!({}));
            }
        }
        // What the hook told of its subagents (Antigravity's: one ended, its hook heard).
        self.settle_subagents(core);
    }

    /// Something waits to go to its harness: kiln's queue, what its adapter holds till its harness
    /// is ready, or what waits for one of its subagents.
    pub fn waits(&self, core: &Core) -> bool {
        let held = match self {
            Self::Codex(a) => a.holds(),
            Self::Acp(a) => a.holds(),
            Self::Claude(_) | Self::Agy(_) | Self::Pi(_) => false,
        };
        held || !core.state.queue.is_empty() || core.state.tasks.iter().any(|t| !t.queue.is_empty())
    }

    /// Start with a conversation carried over from another harness, in the harness's own form
    /// (Codex: Responses items for its new thread; Claude Code reads its own from a transcript;
    /// an ACP agent and Antigravity take it in words, with the first message they get).
    pub fn seed(&mut self, items: Vec<serde_json::Value>) {
        // The conversation in words (`moving::Carry::Words`), for a harness that takes it only with
        // its first message.
        let words = || items.iter().find(|i| i.get("type").and_then(serde_json::Value::as_str) == Some(super::moving::CONTEXT)).and_then(|i| i.get("text")).and_then(serde_json::Value::as_str).map(str::to_owned);
        match self {
            Self::Codex(a) => a.seed(items),
            Self::Acp(a) => a.offer_context(words()),
            Self::Agy(a) => a.offer_context(words()),
            Self::Pi(a) => a.offer_context(words()),
            Self::Claude(_) => {}
        }
    }

    /// That conversation is in (or there was none): an ACP agent and Antigravity have it once
    /// they were given it with a message, so one started again before that is given it then.
    pub fn seeded(&self) -> bool {
        match self {
            Self::Codex(a) => a.seeded(),
            Self::Acp(a) => a.seeded(),
            Self::Agy(a) => a.seeded(),
            Self::Pi(a) => a.seeded(),
            Self::Claude(_) => true,
        }
    }

    /// A session an older image handed over runs on this one: it says what this image's harness
    /// can do, as a start would (its adapter doesn't start again), keeping what its harness turned
    /// on or off while it ran.
    pub fn upgraded(&self, core: &mut Core) {
        let (features, learned) = match self {
            Self::Claude(_) => (super::claude::FEATURES, super::claude::LEARNED),
            Self::Codex(_) => (super::codex::FEATURES, super::codex::LEARNED),
            Self::Acp(_) => (super::acp::FEATURES, super::acp::LEARNED),
            Self::Agy(_) => (super::antigravity::FEATURES, super::antigravity::LEARNED),
            Self::Pi(_) => (super::pi::FEATURES, super::pi::LEARNED),
        };
        let now = upgraded_features(features, learned, &core.meta.features);
        if now != core.meta.features {
            core.meta.features = now;
            core.touch_meta();
        }
    }
}

/// This image's `features`, those in `learned` as the session had them.
fn upgraded_features(features: &[&str], learned: &[&str], had: &[String]) -> Vec<String> {
    let mut now: Vec<String> = features.iter().filter(|f| !learned.contains(f)).map(|f| (*f).to_owned()).collect();
    for f in had {
        if learned.contains(&f.as_str()) && !now.contains(f) {
            now.push(f.clone());
        }
    }
    now
}

/// What a machine offers, for the "new agent" dialog.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct KindInfo {
    pub id: String,
    pub name: String,
    pub verified: bool,
    pub available: bool,
    /// Its permission modes, those that skip every check marked `dangerous` (a choice that takes
    /// a confirmation, never a default). ACP agents other than Devin announce theirs per session.
    #[serde(default)]
    pub modes: Vec<super::model::ModeOption>,
    #[serde(default)]
    pub default_mode: String,
    /// The effort levels it takes, lowest first (empty: none, or announced per session). Starting
    /// without one leaves the model's own default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<super::model::ControlOption>,
    /// kiln runs it as a structured session; false: it runs in a kiln terminal (`program`).
    #[serde(default = "yes")]
    pub structured: bool,
    /// The executable (what a terminal runs for one that is not structured).
    #[serde(default)]
    pub program: String,
    /// The models it offers: as its sessions on this machine last listed them, else the ones it
    /// is known to have (empty: not known).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<super::model::ModelOption>,
    /// A session of another harness can go on in this one, its conversation carried over
    /// (`Command::SetModel::kind`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switch: bool,
}

fn yes() -> bool {
    true
}


impl Kind {
    /// Its permission modes, and the one a new session starts in.
    pub fn modes(&self) -> (Vec<super::model::ModeOption>, &'static str) {
        match self.protocol {
            Protocol::ClaudeStreamJson => {
                let mut m = super::claude::modes(true);
                // As root (outside a sandbox) Claude Code refuses to skip its checks.
                if !super::claude::bypass_possible() {
                    m.retain(|x| !x.dangerous);
                }
                (m, "default")
            }
            Protocol::CodexAppServer => (super::codex::modes(true), "auto"),
            Protocol::AgyStreamJson => (super::antigravity::modes(), super::antigravity::DEFAULT_MODE),
            Protocol::PiRpc => (super::pi::modes(), super::pi::DEFAULT_MODE),
            Protocol::Acp(_) if self.id == "cursor" => (cursor_modes(), CURSOR_DEFAULT_MODE),
            Protocol::Acp(_) if self.id == "devin" => (
                [
                    ("accept-edits", "Code", "Write and edit code", false),
                    ("smart", "Smart", "Auto-approve actions the model judges safe", false),
                    ("ask", "Ask", "Answer questions without code changes", false),
                    ("plan", "Plan", "Plan changes before implementing", false),
                    ("bypass", "Bypass Permissions", "Auto-approve all tool calls unless your organization's policy requires approval", true),
                ]
                .into_iter()
                .map(|(id, label, description, dangerous)| super::model::ModeOption { id: id.into(), label: label.into(), description: description.into(), dangerous })
                .collect(),
                "accept-edits",
            ),
            Protocol::Acp(_) | Protocol::Terminal => (Vec::new(), ""),
        }
    }

    /// A conversation can be carried into it from another harness (and out of it): every harness
    /// kiln runs as a structured session. One with its [`own_record`](Self::own_record) is read
    /// and written as it keeps it; any other is read from kiln's timeline and given the
    /// conversation in words, with the first message it gets.
    pub fn switchable(&self) -> bool {
        self.structured()
    }

    /// kiln reads and writes its own record of a conversation (Claude Code's transcript, Codex's
    /// thread), so a conversation carried into it or out of it goes as that record.
    pub fn own_record(&self) -> bool {
        matches!(self.protocol, Protocol::ClaudeStreamJson | Protocol::CodexAppServer)
    }

    /// The effort levels a new session can start at, before the harness says which its model has:
    /// Codex's default model; Devin's common levels, safe for a model not learned yet too.
    pub fn efforts(&self) -> Vec<super::model::ControlOption> {
        let levels: Vec<String> = match self.protocol {
            Protocol::ClaudeStreamJson => super::claude::EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            Protocol::CodexAppServer => super::codex::default_efforts(),
            Protocol::AgyStreamJson => super::antigravity::EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            Protocol::PiRpc => super::pi::EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            Protocol::Acp(_) if self.id == "devin" => common_efforts(&models_of("devin")),
            Protocol::Acp(_) | Protocol::Terminal => Vec::new(),
        };
        levels
            .iter()
            .map(|e| super::model::ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false })
            .collect()
    }
}

/// Cursor's modes, as its ACP server offers them (`agent acp`'s `session/new`: Agent, Plan, Ask),
/// and kiln's Bypass, which answers yes to every request to go ahead itself; known before a
/// session says them.
pub(super) fn cursor_modes() -> Vec<super::model::ModeOption> {
    let mut modes: Vec<super::model::ModeOption> = [
        ("agent", "Agent", "Full agent capabilities with tool access"),
        ("plan", "Plan", "Read-only mode for planning and designing before implementation"),
        ("ask", "Ask", "Q&A mode - no edits or command execution"),
    ]
    .into_iter()
    .map(|(id, label, description)| super::model::ModeOption { id: id.into(), label: label.into(), description: description.into(), dangerous: false })
    .collect();
    modes.extend(super::acp::kiln_modes().into_iter().filter(|m| m.dangerous));
    modes
}

/// A new Cursor session's mode: its own default, Agent.
pub(super) const CURSOR_DEFAULT_MODE: &str = "agent";

fn common_efforts(models: &[super::model::ModelOption]) -> Vec<String> {
    models.first().map(|first| first.efforts.iter().filter(|e| models.iter().all(|m| m.efforts.contains(e))).cloned().collect()).unwrap_or_default()
}

pub fn offered() -> Vec<KindInfo> {
    #[cfg(not(test))]
    super::acp::warm_models();
    KINDS
        .iter()
        .map(|k| {
            let (modes, default_mode) = k.modes();
            KindInfo {
                id: k.id.to_owned(),
                name: k.name.to_owned(),
                verified: k.verified,
                available: k.available(),
                modes,
                default_mode: default_mode.to_owned(),
                efforts: k.efforts(),
                structured: k.structured(),
                program: k.program.to_owned(),
                models: models_of(k.id),
                switch: k.switchable(),
            }
        })
        .collect()
}

/// The models each harness's sessions on this machine last listed, by harness, and the file
/// they are kept in (so they are known before any session runs, after a restart too).
struct Seen {
    file: Option<std::path::PathBuf>,
    models: std::collections::HashMap<String, Vec<super::model::ModelOption>>,
}

static SEEN: std::sync::LazyLock<parking_lot::Mutex<Seen>> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(Seen { file: None, models: std::collections::HashMap::new() }));

/// Keep the models seen in `file` (and know the ones kept there). Blocking (reads it).
pub fn keep_models_in(file: std::path::PathBuf) {
    let kept: std::collections::HashMap<String, Vec<super::model::ModelOption>> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut g = SEEN.lock();
    for (k, v) in kept {
        g.models.entry(k).or_insert(v);
    }
    g.file = Some(file);
}

/// A session of harness `kind` listed these models: the machine's list for it from now on
/// (written to the file off the caller's thread).
pub fn note_models(kind: &str, models: &[super::model::ModelOption]) {
    if models.is_empty() {
        return;
    }
    let (file, all) = {
        let mut g = SEEN.lock();
        if g.models.get(kind).is_some_and(|m| m.as_slice() == models) {
            return;
        }
        g.models.insert(kind.to_owned(), models.to_vec());
        (g.file.clone(), g.models.clone())
    };
    // What the machine offers changed: said again (`changed`).
    CHANGED.notify();
    if let (Some(file), Ok(json)) = (file, serde_json::to_vec_pretty(&all)) {
        crate::rt::spawn(async move {
            let _ = crate::rt::blocking(move || crate::util::write_secret(&file, &json, 0o600)).await;
        });
    }
}

/// The models harness `kind` offers on this machine: as its sessions last listed them, else the
/// ones it is known to have.
pub fn models_of(kind: &str) -> Vec<super::model::ModelOption> {
    if let Some(m) = SEEN.lock().models.get(kind).filter(|m| !m.is_empty()) {
        return m.clone();
    }
    known_models(kind)
}

/// The models harness `kind` is known to have, its default first.
fn known_models(kind: &str) -> Vec<super::model::ModelOption> {
    let known: &[(&str, &str, &[&str])] = match kind {
        // Claude Code's aliases: each the newest of its family.
        "claude" => &[("opus", "Opus", super::claude::EFFORTS), ("sonnet", "Sonnet", super::claude::EFFORTS), ("haiku", "Haiku", &[])],
        "codex" => &[
            ("gpt-6.1-sol", "GPT-6.1-Sol", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-sol", "GPT-6-Sol", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-astra", "GPT-6-Astra", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-luna", "GPT-6-Luna", &["low", "medium", "high", "xhigh", "max"]),
        ],
        _ => &[],
    };
    // The first is the harness's default.
    known
        .iter()
        .enumerate()
        .map(|(i, (id, label, efforts))| super::model::ModelOption {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default: i == 0,
            ..super::model::ModelOption::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9: what a machine offers is said again as it changes, never on a clock: a harness's
    /// sessions listing new models tells `changed`'s notifier, and the same models said again
    /// don't (no other test notes this harness's). The planted mistake it catches: `note_models`
    /// without its notify. (The search path's folders are `os::watch`'s, tested there.)
    #[test]
    fn g9_new_models_tell_what_says_the_harnesses() -> anyhow::Result<()> {
        use std::time::Duration;
        crate::rt::block_on(async {
            let models = [super::super::model::ModelOption { id: "g9-model".into(), label: "g9".into(), ..Default::default() }];
            while crate::rt::timeout(Duration::from_millis(300), CHANGED.notified()).await.is_ok() {}
            note_models("g9-kind", &models);
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(5), CHANGED.notified()).await.is_ok(), "new models told nothing");
            note_models("g9-kind", &models);
            let again = crate::rt::timeout(Duration::from_millis(300), CHANGED.notified()).await.is_ok();
            // Another test's models may tell it meanwhile; this harness's same models don't.
            anyhow::ensure!(!again || SEEN.lock().models.len() > 1, "the same models said again told it");
            Ok(())
        })
    }

    /// A Devin session handed over from an image before every harness could switch runs on this
    /// image's features (switching among them), with the model and steering its agent said it takes
    /// kept, and one it never said left off; what this image dropped is gone.
    #[test]
    fn a_session_handed_over_runs_on_this_images_features() {
        let had: Vec<String> = ["send_now", "queue", "interrupt", "set_mode", "resume", "attachments", "steer", "queue_steer", "set_model", "gone"].iter().map(|s| (*s).to_owned()).collect();
        let now = upgraded_features(super::super::acp::FEATURES, super::super::acp::LEARNED, &had);
        for f in super::super::acp::FEATURES {
            assert!(now.iter().any(|n| n == f), "{f} missing: {now:?}");
        }
        assert!(now.iter().any(|n| n == "switch_harness") && now.iter().any(|n| n == "set_model") && now.iter().any(|n| n == "steer"), "{now:?}");
        assert!(!now.iter().any(|n| n == "set_effort" || n == "gone"), "{now:?}");
        // Again: nothing changes.
        assert_eq!(upgraded_features(super::super::acp::FEATURES, super::super::acp::LEARNED, &now), now);
    }

    /// kiln's page tools go into Antigravity's settings as agy keeps them: beside its other
    /// settings, readable by its user alone, once; settings it can't read are left as they are.
    #[cfg(unix)]
    #[test]
    fn antigravitys_settings_get_kilns_page_tools_once() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("antigravity-cli/settings.json");
        let read = |p: &std::path::Path| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) };
        // None yet (agy never ran here): made, with the rules alone.
        assert!(agy_allow_in(&path)?);
        assert_eq!(read(&path)?.pointer("/permissions/allow"), Some(&serde_json::json!(crate::reach::mcp::AGY_ALLOWED)));
        // agy's own: kept, and as private as agy keeps them; once there, left alone.
        std::fs::write(&path, r#"{"model":"m","trustedWorkspaces":["/w"]}"#)?;
        assert!(agy_allow_in(&path)?);
        let kept = read(&path)?;
        assert_eq!((kept.get("model"), kept.get("trustedWorkspaces")), (Some(&serde_json::json!("m")), Some(&serde_json::json!(["/w"]))));
        assert_eq!(kept.pointer("/permissions/allow"), Some(&serde_json::json!(crate::reach::mcp::AGY_ALLOWED)));
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert!(!agy_allow_in(&path)?);
        std::fs::write(&path, "{not json")?;
        assert!(agy_allow_in(&path).is_err());
        assert_eq!(std::fs::read(&path)?, b"{not json");
        Ok(())
    }

    /// kiln's hook goes into agy's hooks.json beside the person's own, once, for this kiln binary
    /// (another rewrites it); hooks it can't read are left as they are. Run as agy runs it (a
    /// shell), it hands each event to this kiln, and where the binary is gone (a version pruned,
    /// kiln uninstalled) it passes, so the person's own agy goes on as without kiln.
    #[cfg(unix)]
    #[test]
    fn antigravitys_hooks_get_kilns_hook_once() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config/hooks.json");
        let read = |p: &std::path::Path| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) };
        let command = |v: &serde_json::Value, ev: &str| v.pointer(&format!("/kiln/{ev}/0/hooks/0/command")).or_else(|| v.pointer(&format!("/kiln/{ev}/0/command"))).and_then(serde_json::Value::as_str).map(str::to_owned).unwrap_or_default();
        let run = |command: &str| -> anyhow::Result<String> { Ok(String::from_utf8(std::process::Command::new("sh").arg("-c").arg(command).output()?.stdout)?.trim().to_owned()) };
        let gone = dir.path().join("versions/old/kiln");
        let gone = gone.to_string_lossy();
        assert!(agy_hooks_in(&path, &gone)?);
        let made = read(&path)?;
        assert_eq!(made.pointer("/kiln/PreToolUse/0/matcher"), Some(&serde_json::json!("*")));
        assert_eq!(made.pointer("/kiln/PreToolUse/0/hooks/0/timeout"), Some(&serde_json::json!(super::super::antigravity::HOOK_FOR_S)));
        // Its binary gone: each event passes.
        assert_eq!(run(&command(&made, "PreToolUse"))?, r#"{"decision":"allow"}"#);
        assert_eq!(run(&command(&made, "PreInvocation"))?, "{}");
        assert_eq!(run(&command(&made, "Stop"))?, r#"{"decision":""}"#);
        // There: the event goes to it (a stand-in that says what it was asked), a path with a
        // space and a quote in it too.
        let bin = dir.path().join("it's kiln/kiln");
        std::fs::create_dir_all(bin.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?)?;
        std::fs::write(&bin, "#!/bin/sh\necho \"$@\"\n")?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        assert!(agy_hooks_in(&path, &bin.to_string_lossy())?);
        let there = read(&path)?;
        assert_eq!(run(&command(&there, "PreToolUse"))?, "harness-hook --agy PreToolUse");
        assert_eq!(run(&command(&there, "Stop"))?, "harness-hook --agy Stop");
        // The person's own: kept; once there, left alone; another kiln, rewritten.
        std::fs::write(&path, r#"{"lint":{"PostToolUse":[{"matcher":"write_to_file","hooks":[{"command":"./lint.sh"}]}]}}"#)?;
        assert!(agy_hooks_in(&path, "/k/kiln")?);
        let kept = read(&path)?;
        assert_eq!(kept.pointer("/lint/PostToolUse/0/hooks/0/command"), Some(&serde_json::json!("./lint.sh")));
        assert!(command(&kept, "PreInvocation").contains("k='/k/kiln';"));
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert!(!agy_hooks_in(&path, "/k/kiln")?);
        assert!(agy_hooks_in(&path, "/k2/kiln")?);
        assert!(read(&path)?.get("lint").is_some());
        std::fs::write(&path, "{not json")?;
        assert!(agy_hooks_in(&path, "/k/kiln").is_err());
        assert_eq!(std::fs::read(&path)?, b"{not json");
        Ok(())
    }

    #[test]
    fn devin_runs_its_acp_protocol_and_keeps_its_terminal_interface() -> anyhow::Result<()> {
        let kind = Kind::get("devin").ok_or_else(|| anyhow::anyhow!("no Devin harness"))?;
        assert_eq!(kind.protocol, Protocol::Acp(&["acp"]));
        assert_eq!(kind.screen, &["devin"]);
        assert!(!kind.screen_takes_command);
        let cwd = tempfile::tempdir()?;
        let spec = Spec {
            kind: "devin".into(),
            program: Some("/path/to/devin".into()),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            model: Some("default".into()),
            mode: Some("plan".into()),
            effort: Some("high".into()),
            resume: Some("conversation".into()),
            env: vec![("DEVIN_SESSION_TEST".into(), "kept".into()), ("CLAUDECODE".into(), "scrubbed".into())],
            ..Spec::default()
        };
        let launch = kind.launch(&spec, "session")?;
        assert_eq!(launch.get_program(), "/path/to/devin");
        assert_eq!(launch.get_args().collect::<Vec<_>>(), ["acp"]);
        assert_eq!(launch.get_current_dir(), Some(cwd.path()));
        let env: std::collections::HashMap<_, _> = launch.get_envs().collect();
        assert_eq!(env.get(std::ffi::OsStr::new("DEVIN_SESSION_TEST")), Some(&Some(std::ffi::OsStr::new("kept"))));
        assert_eq!(env.get(std::ffi::OsStr::new("CLAUDECODE")), Some(&None));
        assert_eq!(Kind::get("claude").map(|k| k.protocol), Some(Protocol::ClaudeStreamJson));
        assert_eq!(Kind::get("codex").map(|k| k.protocol), Some(Protocol::CodexAppServer));
        let (modes, default) = kind.modes();
        assert_eq!(default, "accept-edits");
        assert_eq!(modes.iter().map(|m| (m.id.as_str(), m.dangerous)).collect::<Vec<_>>(), [("accept-edits", false), ("smart", false), ("ask", false), ("plan", false), ("bypass", true)]);
        Ok(())
    }

    /// Cursor's CLI runs its ACP server (`agent acp`, cursor.com/docs/cli/acp), told to open no
    /// browser on the machine; its own terminal interface is `agent`. Its modes are known before it
    /// says them, each placed in lineage's order, so a child starts in one within its parent's.
    #[test]
    fn cursor_runs_its_acp_server_without_a_browser_and_its_modes_are_placed() -> anyhow::Result<()> {
        use super::super::lineage::{Level, child_mode, level};
        let kind = Kind::get("cursor").ok_or_else(|| anyhow::anyhow!("no Cursor harness"))?;
        assert_eq!((kind.program, kind.protocol, kind.screen, kind.screen_takes_command), ("agent", Protocol::Acp(&["acp"]), &["agent"][..], false));
        let cwd = tempfile::tempdir()?;
        let spec = Spec { kind: "cursor".into(), program: Some("/path/to/agent".into()), cwd: Some(cwd.path().to_string_lossy().into_owned()), mode: Some("plan".into()), ..Spec::default() };
        let launch = kind.launch(&spec, "session")?;
        assert_eq!(launch.get_args().collect::<Vec<_>>(), ["acp"]);
        let env: std::collections::HashMap<_, _> = launch.get_envs().collect();
        assert_eq!(env.get(std::ffi::OsStr::new("NO_OPEN_BROWSER")), Some(&Some(std::ffi::OsStr::new("1"))));
        let (modes, default) = kind.modes();
        assert_eq!(default, "agent");
        assert_eq!(modes.iter().map(|m| (m.id.as_str(), m.dangerous)).collect::<Vec<_>>(), [("agent", false), ("plan", false), ("ask", false), (super::super::acp::KILN_BYPASS, true)]);
        for (mode, l) in [("plan", Level::Plan), ("ask", Level::Plan), ("agent", Level::AcceptEdits), (super::super::acp::KILN_BYPASS, Level::Bypass)] {
            assert_eq!(level("cursor", mode), Some(l), "{mode}");
        }
        assert_eq!(level("cursor", ""), Some(Level::AcceptEdits), "its own default, Agent");
        // Under a parent that asks, a child doesn't start in Agent (it may edit unasked).
        assert_eq!(child_mode("cursor", None, Level::Ask, false), Ok(Some("plan".to_owned())));
        assert_eq!(child_mode("cursor", None, Level::AcceptEdits, false), Ok(Some("agent".to_owned())));
        assert!(child_mode("cursor", Some("agent"), Level::Ask, false).is_err());
        Ok(())
    }

    /// Pi runs in its RPC mode (`pi --mode rpc`) on its session, its mode's tools its flags; its
    /// own terminal interface is `pi`. It asks before nothing, so its modes are placed by what their
    /// tools can do: Full access is Bypass.
    #[test]
    fn pi_runs_its_rpc_mode_with_its_modes_tools_and_its_modes_are_placed() -> anyhow::Result<()> {
        use super::super::lineage::{Level, child_mode, level};
        let kind = Kind::get("pi").ok_or_else(|| anyhow::anyhow!("no Pi harness"))?;
        assert_eq!((kind.program, kind.protocol, kind.screen, kind.screen_takes_command), ("pi", Protocol::PiRpc, &["pi"][..], false));
        let cwd = tempfile::tempdir()?;
        let spec = Spec { kind: "pi".into(), program: Some("/path/to/pi".into()), cwd: Some(cwd.path().to_string_lossy().into_owned()), resume: Some("s1".into()), mode: Some("read-only".into()), ..Spec::default() };
        let launch = kind.launch(&spec, "session")?;
        assert_eq!(launch.get_args().collect::<Vec<_>>(), ["--mode", "rpc", "--session-id", "s1", "--tools", "read,grep,find,ls", "--exclude-tools", "mcp__*"]);
        assert!(matches!(kind.adapter(&spec), Adapter::Pi(_)));
        let (modes, default) = kind.modes();
        assert_eq!(default, "edit");
        assert_eq!(modes.iter().map(|m| (m.id.as_str(), m.dangerous)).collect::<Vec<_>>(), [("read-only", false), ("edit", false), ("full", true)]);
        assert_eq!(kind.efforts().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["off", "minimal", "low", "medium", "high"]);
        for (mode, l) in [("read-only", Level::Plan), ("edit", Level::AcceptEdits), ("full", Level::Bypass)] {
            assert_eq!(level("pi", mode), Some(l), "{mode}");
        }
        assert_eq!(child_mode("pi", None, Level::Ask, false), Ok(Some("read-only".to_owned())));
        assert_eq!(child_mode("pi", None, Level::Bypass, true), Ok(Some("edit".to_owned())), "never Full access unasked");
        assert!(child_mode("pi", Some("full"), Level::Bypass, false).is_err(), "Full access needs its parent's own permission to skip checks");
        assert!(!kind.own_record() && kind.switchable());
        Ok(())
    }

    /// None of kiln's own variables in the environment of the server that starts an agent (its data
    /// folder, config, supervisor's socket and admin token: a test server an agent started opened
    /// production's database through its KILN_DATA_DIR, 2026-11-06), nor in the shell `kiln spawn`
    /// took the environment of (a kiln terminal's has the server's), reaches the agent's harness: it
    /// has the rest of what it was started with, what it was given, and its own mark. Runs again in a
    /// process of its own whose environment has a server's.
    #[cfg(unix)]
    #[test]
    fn an_agent_inherits_none_of_the_servers_own_kiln_variables() -> anyhow::Result<()> {
        const SERVERS: [(&str, &str); 7] = [
            ("KILN_DATA_DIR", "/srv/kiln/data"),
            ("KILN_CONFIG", "/srv/kiln/server.toml"),
            ("KILN_SUPERVISOR", "/run/user/1000/kiln/supervise-0.sock"),
            ("KILN_SUPERVISED", "1"),
            ("KILN_GENERATION", "3"),
            ("KILN_ADMIN_TOKEN", "not-a-token"),
            ("MACHINE_SAYS", "kept"),
        ];
        const AS_SERVER: &str = "KILN_TEST_AS_SERVER";
        if std::env::var_os(AS_SERVER).is_none() {
            let me = format!("{}::an_agent_inherits_none_of_the_servers_own_kiln_variables", module_path!().split_once("::").map_or("", |(_, path)| path));
            let out = std::process::Command::new(std::env::current_exe()?).args(["--exact", me.as_str(), "--nocapture", "--test-threads=1"]).env(AS_SERVER, "1").envs(SERVERS).output()?;
            let said = String::from_utf8_lossy(&out.stdout);
            anyhow::ensure!(out.status.success(), "as a server ({}): {said}{}", out.status, String::from_utf8_lossy(&out.stderr));
            anyhow::ensure!(said.contains("1 passed"), "{me} didn't run as a server: {said}");
            return Ok(());
        }
        // A stand-in harness that writes down the environment it starts with.
        let dir = tempfile::tempdir()?;
        let seen = dir.path().join("env");
        let harness = dir.path().join("devin");
        std::fs::write(&harness, "#!/bin/sh\nenv > \"$SEEN\"\n")?;
        crate::os::make_executable(&harness)?;
        let kind = Kind::get("devin").ok_or_else(|| anyhow::anyhow!("no Devin harness"))?;
        // Started from a kiln terminal on the server, whose shell has the server's variables too.
        let spec = Spec {
            kind: "devin".into(),
            program: Some(harness.to_string_lossy().into_owned()),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            env: vec![("SEEN".into(), seen.to_string_lossy().into_owned()), ("KILN_DATA_DIR".into(), "/srv/kiln/data".into()), ("KILN_TERM_SESSION".into(), "t1".into())],
            ..Spec::default()
        };
        let mut p = kind.launch(&spec, "s1")?;
        // Marked as the agent's, as a session starts it (`session.rs`).
        p.env(super::super::survivors::MARK, "s1");
        let status = p.stdin(std::process::Stdio::null()).status()?;
        anyhow::ensure!(status.success(), "the stand-in harness: {status}");
        let env = std::fs::read_to_string(&seen)?;
        let vars: Vec<(&str, &str)> = env.lines().filter_map(|l| l.split_once('=')).collect();
        let kilns: Vec<&str> = vars.iter().map(|(k, _)| *k).filter(|k| k.starts_with("KILN_")).collect();
        anyhow::ensure!(kilns == ["KILN_AGENT"], "the harness has kiln variables that aren't its own: {kilns:?}");
        anyhow::ensure!(vars.contains(&("KILN_AGENT", "s1")) && vars.contains(&("MACHINE_SAYS", "kept")) && vars.iter().any(|(k, _)| *k == "SEEN"), "{vars:?}");
        Ok(())
    }

    /// A runner's service keeps the PATH it was installed with: a harness installed since in a
    /// folder only its user's PATH has (`~/.local/bin`) is found on the two together, the
    /// runner's folders first and each once (the user's relative ones left out), and runs with
    /// them.
    #[test]
    fn a_harness_on_its_users_path_alone_is_found_and_runs_with_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (own, users) = (dir.path().join("usr/bin"), dir.path().join("home/.local/bin"));
        std::fs::create_dir_all(&own)?;
        std::fs::create_dir_all(&users)?;
        // As it's installed: a script on Unix, a batch file on Windows (which starts no shell script).
        let claude = users.join(if cfg!(windows) { "claude.cmd" } else { "claude" });
        std::fs::write(&claude, if cfg!(windows) { "@echo off\r\n" } else { "#!/bin/sh\n" })?;
        crate::os::make_executable(&claude)?;
        let runner = std::env::join_paths([&own])?;
        assert_eq!(crate::os::which_in("claude", &runner), None);
        let both = joined_path(Some(&runner), Some(&std::env::join_paths([users.as_path(), std::path::Path::new("bin"), &own])?));
        assert_eq!(both, Some(std::env::join_paths([&own, &users])?));
        assert_eq!(both.as_deref().and_then(|p| crate::os::which_in("claude", p)), Some(claude.clone()));
        assert_eq!(joined_path(Some(&runner), None), Some(runner));
        assert_eq!(joined_path(None, None), None);
        // Started, it has the PATH it was looked for on: this process's folders first.
        let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no Claude Code harness"))?;
        let spec = Spec { kind: "claude".into(), program: Some(claude.to_string_lossy().into_owned()), cwd: Some(dir.path().to_string_lossy().into_owned()), ..Spec::default() };
        let launch = kind.launch(&spec, "session")?;
        let dirs = |p: Option<&std::ffi::OsStr>| p.map(|p| std::env::split_paths(p).collect::<Vec<_>>()).unwrap_or_default();
        let ran = dirs(launch.get_envs().find(|(k, _)| *k == "PATH").and_then(|(_, v)| v));
        let mine = dirs(std::env::var_os("PATH").as_deref());
        assert!(!ran.is_empty() && ran.starts_with(&mine), "{ran:?} starts with {mine:?}");
        Ok(())
    }

    #[test]
    fn common_efforts_never_offers_a_level_missing_from_another_model() {
        use super::super::model::ModelOption;
        let mut models = vec![
            ModelOption { efforts: vec!["low".into(), "high".into()], ..ModelOption::default() },
            ModelOption { efforts: vec!["medium".into(), "high".into()], ..ModelOption::default() },
        ];
        assert_eq!(common_efforts(&models), ["high"]);
        models.push(ModelOption::default());
        assert!(common_efforts(&models).is_empty());
        assert!(common_efforts(&[]).is_empty());
    }

    #[test]
    fn codex_offers_every_effort_its_default_model_takes() {
        let known = known_models("codex");
        let default = known.iter().find(|m| m.default);
        assert_eq!(default.map(|m| m.id.as_str()), Some("gpt-6.1-sol"));
        assert_eq!(default.map(|m| m.efforts.clone()), Some(["low", "medium", "high", "xhigh", "max", "ultra"].map(str::to_owned).to_vec()));
        let labels: Vec<String> = ["low", "xhigh", "max", "ultra"].iter().map(|e| super::super::model::effort_label(e)).collect();
        assert_eq!(labels, ["Low", "Extra high", "Max", "Ultra"]);
    }

    /// Each adapter, started (its first requests to the harness under way), is written out and read
    /// back whole: what an in-place upgrade hands the next image of the runner.
    #[test]
    fn every_adapter_is_written_out_and_read_back_whole() -> anyhow::Result<()> {
        for id in ["claude", "codex", "gemini", "devin", "cursor", "pi"] {
            let kind = Kind::get(id).ok_or_else(|| anyhow::anyhow!("no kind {id}"))?;
            let mut adapter = kind.adapter(&Spec { kind: id.to_owned(), ..Spec::default() });
            let mut core = Core::new();
            adapter.start(&mut core);
            let out = serde_json::to_value(adapter.freeze())?;
            assert!(out.pointer("/state/pending").and_then(serde_json::Value::as_object).is_some_and(|p| !p.is_empty()), "{id}: nothing under way to carry");
            let back = Adapter::thaw(serde_json::from_value(out.clone())?);
            assert_eq!(serde_json::to_value(back.freeze())?, out, "{id}");
        }
        Ok(())
    }

    /// A session moved from Codex to another harness: what Codex left live (an older image kept
    /// it so) can never be settled by the adapter after it, whichever that is.
    #[test]
    fn whatever_the_harness_what_it_ran_ends_with_its_process_and_a_new_one_inherits_none_of_it() {
        use super::super::model::{Body, Task, ToolCall, ToolKind, ToolStatus};
        let call = |id: &str| {
            Body::Tool(Box::new(ToolCall {
                tool_id: id.into(),
                name: "shell".into(),
                tool: ToolKind::Shell,
                title: "python3 -m http.server".into(),
                detail: None,
                input: serde_json::Value::Null,
                status: ToolStatus::Running,
                output: None,
                truncated: false,
                diffs: Vec::new(),
                todos: Vec::new(),
                task: None,
                exit_code: None,
                started: 0,
                ended: None,
                server: None,
                tool_short: None,
                skill: None,
                runner: None,
            }))
        };
        let row = |id: &str, tool: &str, class: &str| Task { id: id.into(), tool_id: Some(tool.into()), class: Some(class.into()), status: "running".into(), ..Task::default() };
        let mut core = Core::new();
        core.state.tasks = vec![row("terminal-exec-1", "exec-1", "shell"), row("01a15afd", "call_spawn", "agent"), Task { id: "left-77".into(), status: "running".into(), ..Task::default() }];
        core.upsert("exec-1", None, call("exec-1"));
        // An adapter that keeps no tasks of its own (ACP) still ends them with its process.
        let mut acp = Adapter::Acp(Box::new(Acp::new(None)));
        acp.exited(&mut core, Some(0), None);
        assert_eq!(core.state.tasks.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(), ["stopped", "stopped", "running"]);
        assert!(core.get("exec-1").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled && t.ended.is_some())));
        // Codex's own exit ends its terminals' calls too, not only their rows.
        core.state.tasks.push(row("terminal-exec-2", "exec-2", "shell"));
        core.upsert("exec-2", None, call("exec-2"));
        let mut codex = Adapter::Codex(Box::new(Codex::new(None, None, None, false)));
        codex.exited(&mut core, None, None);
        assert!(core.get("exec-2").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled)));
        // A new process starts with nothing of an earlier one's still running.
        core.state.tasks.push(row("terminal-exec-3", "exec-3", "shell"));
        core.upsert("exec-3", None, call("exec-3"));
        let mut claude = Adapter::Claude(Box::new(Claude::new(false)));
        claude.start(&mut core);
        assert!(core.state.tasks.iter().filter(|t| t.id != "left-77").all(|t| !t.live()));
        assert!(core.get("exec-3").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled)));
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "left-77").map(|t| t.status.as_str()), Some("running"), "kiln's own, taken back over");
    }

    fn message(id: &str, text: &str) -> Command {
        Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
    }

    /// The ids of the user messages kiln wrote its harness since the last look (Claude Code's
    /// `uuid`), and the id of its request `subtype` among what it wrote.
    fn wrote(core: &mut Core, subtype: &str) -> (Vec<String>, Option<serde_json::Value>) {
        let lines: Vec<serde_json::Value> = core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect();
        let users = lines.iter().filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("user")).filter_map(|v| v.get("uuid").and_then(serde_json::Value::as_str).map(str::to_owned)).collect();
        let asked = lines.iter().find(|v| v.pointer("/request/subtype").and_then(serde_json::Value::as_str) == Some(subtype)).and_then(|v| v.get("request_id").cloned());
        (users, asked)
    }

    /// docs/headroom.md, "Agent messages": what waits to reach a session's harness is told to
    /// headroom from the shared state alone, the same for every harness: a message written and
    /// not yet taken, and one in kiln's queue while the harness is starting or idle; not one queued
    /// behind a running turn; and nothing once the harness has exited.
    #[test]
    fn what_waits_for_a_harness_is_told_the_same_way_for_every_harness() {
        use super::super::model::{Delivery, Status};
        let mut core = Core::new();
        core.session_id = "t-delivery-shared".into();
        let w = || crate::headroom::messages::waits("t-delivery-shared");
        let claude = Adapter::Claude(Box::new(Claude::new(false)));
        let note_delivery = |core: &Core| claude.note_delivery(core);
        core.state.status = Status::Starting;
        core.state.queue.push(QueueItem { id: "m1".into(), text: "hi".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        note_delivery(&core);
        assert_eq!(w(), ["m1"], "held while its harness starts");
        // Written: it waits on, the same wait; one queued behind the turn it started doesn't.
        core.state.queue.clear();
        core.state.queue.push(QueueItem { id: "m2".into(), text: "next".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        core.state.delivery.insert("m1".into(), Delivery::Sent);
        core.state.status = Status::Running;
        note_delivery(&core);
        assert_eq!(w(), ["m1"]);
        let before = crate::headroom::messages::ended();
        core.state.delivery.insert("m1".into(), Delivery::Started);
        note_delivery(&core);
        assert!(w().is_empty() && crate::headroom::messages::ended() > before, "taken, it's measured");
        core.state.delivery.insert("m3".into(), Delivery::Sent);
        note_delivery(&core);
        core.state.status = Status::Exited;
        note_delivery(&core);
        assert!(w().is_empty(), "an exited harness's waits aren't kept");
        // Through every kind's own adapter (Claude Code, Codex, each ACP agent, Antigravity): a
        // message for a harness that hasn't answered yet waits, whichever it is.
        for kind in Kind::all() {
            let mut a = kind.adapter(&Spec { kind: kind.id.to_owned(), ..Spec::default() });
            let mut core = Core::new();
            core.session_id = format!("t-delivery-{}", kind.id);
            a.start(&mut core);
            a.command(&mut core, message("m1", "are you there?"));
            assert!(!crate::headroom::messages::waits(&core.session_id).is_empty(), "{}: a message for a harness still starting isn't told as waiting", kind.id);
        }
    }

    fn queue(core: &Core) -> Vec<&str> {
        core.state.queue.iter().map(|q| q.id.as_str()).collect()
    }

    /// CP-T3.2: a switch out of Codex before its thread opens leaves its messages in kiln's
    /// durable queue, in order. Claude Code takes them once ready, each shown sent only then.
    #[test]
    fn a_message_held_for_a_harness_that_ends_goes_to_the_one_after_it() {
        use super::super::model::{Delivery, Status};
        let mut core = Core::new();
        let mut codex = Adapter::Codex(Box::new(Codex::new(None, None, None, false)));
        codex.start(&mut core);
        codex.command(&mut core, message("m1", "Plans and Research, one tab each"));
        codex.command(&mut core, message("m2", "Research searchable, its results as cards"));
        assert_eq!(queue(&core), ["m1", "m2"]);
        assert_eq!([core.state.delivery.get("m1"), core.state.delivery.get("m2")], [None; 2]);
        assert!(core.get("m1").is_none() && core.get("m2").is_none(), "unsent messages stay in the durable queue");
        // The switch: Codex's process ends.
        codex.exited(&mut core, None, None);
        assert_eq!(queue(&core), ["m1", "m2"]);
        assert_eq!([core.state.delivery.get("m1"), core.state.delivery.get("m2")], [None; 2]);
        // Claude Code, once up, takes the first; the next once that's done.
        core.state.exit = None;
        core.set_status(Status::Starting);
        let mut claude = Adapter::Claude(Box::new(Claude::new(false)));
        claude.start(&mut core);
        let (_, init) = wrote(&mut core, "initialize");
        claude.line(&mut core, &serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": init, "response": {}}}).to_string());
        assert_eq!(wrote(&mut core, "").0, ["m1"]);
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Sent));
        assert!(core.get("m1").is_some() && core.get("m2").is_none(), "only the message handed to Claude is shown sent");
        assert_eq!(queue(&core), ["m2"]);
        for line in [
            r#"{"type":"command_lifecycle","command_uuid":"m1","state":"started","uuid":"x","session_id":"s"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Two designs.","stop_reason":"end_turn","session_id":"s"}"#,
            r#"{"type":"command_lifecycle","command_uuid":"m1","state":"completed","uuid":"y","session_id":"s"}"#,
        ] {
            claude.line(&mut core, line);
        }
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        assert_eq!(wrote(&mut core, "").0, ["m2"]);
        assert!(core.state.queue.is_empty());
    }

    /// An ACP agent (Devin, Gemini) holds a message till its session opens: one that ends first
    /// (it can't sign in, and is switched out of) lets go of it for the next harness.
    #[test]
    fn an_acp_agent_that_ends_before_its_session_opens_lets_go_of_what_it_held() {
        use super::super::model::Delivery;
        let mut core = Core::new();
        let mut acp = Adapter::Acp(Box::new(Acp::new(None)));
        acp.start(&mut core);
        acp.command(&mut core, message("m1", "Plans and Research, one tab each"));
        assert_eq!((core.state.delivery.get("m1"), queue(&core).len()), (Some(&Delivery::Queued), 1));
        acp.exited(&mut core, Some(1), Some("not signed in".into()));
        assert_eq!(queue(&core), ["m1"]);
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
    }

    /// CP-T3.2: a legacy adapter and the durable queue may overlap across an upgrade. A switch
    /// carries the edited input once, preserves order, and leaves removed or dispatched input
    /// behind, including a message a rewind removed from the conversation.
    #[test]
    fn cp_t3_2_legacy_held_input_switches_once_with_durable_edits_and_no_resurrection() -> anyhow::Result<()> {
        use super::super::model::{Attachment, Body, Overrides, Status};
        for (kind, exited) in [("codex", false), ("codex", true), ("devin", false), ("devin", true)] {
            let harness = Kind::get(kind).ok_or_else(|| anyhow::anyhow!("no {kind} harness"))?;
            let adapter = harness.adapter(&Spec { kind: kind.into(), ..Default::default() });
            let item = |id: &str| QueueItem { id: id.into(), text: id.into(), at: 1, attachments: vec![], overrides: None, reply_to: None };
            let mut core = Core::new();
            let states = [("first", Delivery::Queued), ("second", Delivery::Queued), ("removed", Delivery::Cancelled), ("sent", Delivery::Sent), ("started", Delivery::Started), ("completed", Delivery::Completed), ("rewound", Delivery::Queued)];
            for (id, d) in states {
                core.state.delivery.insert(id.into(), d);
                if id != "rewound" {
                    core.upsert(id, None, Body::User { text: id.into(), images: 0, client_id: Some(id.into()), origin: None, attachments: vec![], reply_to: None });
                }
            }
            let edited = QueueItem {
                text: "the durable edit".into(),
                attachments: vec![Attachment { name: "notes.txt".into(), mime: "text/plain".into(), data_base64: String::new(), file_id: None, path: Some("/work/notes.txt".into()) }],
                overrides: Some(Overrides { model: Some("chosen".into()), effort: Some("high".into()) }),
                reply_to: Some("earlier".into()),
                ..item("first")
            };
            core.state.queue.push(edited.clone());
            core.state.queue.push(QueueItem { text: "unshown durable edit".into(), ..item("unshown") });
            core.state.queue.extend(["removed", "sent", "started", "completed", "rewound", "later", "first"].map(item));
            let mut frozen = serde_json::to_value(adapter.freeze())?;
            let state = frozen.get_mut("state").and_then(serde_json::Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("{kind} adapter has no state"))?;
            // An old image can leave both copies, or more than one buffered copy of an id.
            let mut legacy = ["first", "second", "unshown", "removed", "sent", "started", "completed", "rewound", "first"].map(item).to_vec();
            if !exited {
                legacy.push(item("absent"));
            }
            state.insert("waiting".into(), serde_json::to_value(legacy)?);
            let mut adapter = Adapter::thaw(serde_json::from_value(frozen)?);
            if exited {
                adapter.exited(&mut core, Some(1), None);
            } else {
                adapter.let_go(&mut core);
            }
            assert_eq!(queue(&core), ["first", "second", "unshown", "later"], "{kind}");
            assert_eq!(core.state.queue.first(), Some(&edited), "{kind}: the durable edit lost metadata");
            assert_eq!(core.state.queue.iter().find(|m| m.id == "unshown").map(|m| m.text.as_str()), Some("unshown durable edit"), "{kind}: unshown durable input disappeared");
            for id in ["removed", "sent", "started", "rewound"] {
                assert_eq!(core.state.delivery.get(id), Some(&Delivery::Cancelled), "{kind}: {id}");
            }
            assert_eq!(core.state.delivery.get("completed"), Some(&Delivery::Completed), "{kind}");
            adapter.let_go(&mut core);
            assert_eq!(queue(&core), ["first", "second", "unshown", "later"], "{kind}: let_go repeated input");

            core.state.exit = None;
            core.set_status(Status::Starting);
            let mut claude = Adapter::Claude(Box::new(Claude::new(false)));
            claude.start(&mut core);
            let (_, init) = wrote(&mut core, "initialize");
            claude.line(&mut core, &serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": init, "response": {}}}).to_string());
            assert_eq!(wrote(&mut core, "").0, ["first"], "{kind}");
            for (id, next) in [("first", vec!["second"]), ("second", vec!["unshown"]), ("unshown", vec!["later"]), ("later", Vec::new())] {
                for line in [
                    serde_json::json!({"type": "command_lifecycle", "command_uuid": id, "state": "started", "uuid": format!("{id}-started"), "session_id": "s"}),
                    serde_json::json!({"type": "result", "subtype": "success", "is_error": false, "result": "done", "stop_reason": "end_turn", "session_id": "s"}),
                    serde_json::json!({"type": "command_lifecycle", "command_uuid": id, "state": "completed", "uuid": format!("{id}-completed"), "session_id": "s"}),
                ] {
                    claude.line(&mut core, &line.to_string());
                }
                assert_eq!(wrote(&mut core, "").0, next, "{kind}: after {id}");
                assert_eq!(core.state.delivery.get(id), Some(&Delivery::Completed), "{kind}: {id}");
            }
            assert!(core.state.queue.is_empty(), "{kind}");
        }
        Ok(())
    }

    /// CP-T3.2: its process gone, what its harness was handed and never finished (sent, in its own
    /// queue, under way) ends dropped, never on its way for good; it may have run, so it isn't sent
    /// again. What waits in kiln's queue or a subagent's still goes, and what ended stays as it
    /// ended. The agent ended, nothing that waited goes either.
    #[test]
    fn what_a_harness_never_finished_ends_dropped_with_its_process() {
        use super::super::model::{Delivery, Task};
        let item = |id: &str| QueueItem { id: id.into(), text: "next".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None };
        let mut core = Core::new();
        let all = [("sent", Delivery::Sent), ("its-queue", Delivery::Queued), ("under-way", Delivery::Started), ("done", Delivery::Completed), ("unsent", Delivery::Cancelled), ("kiln-queue", Delivery::Queued), ("sub-queue", Delivery::Queued)];
        core.state.delivery = all.into_iter().map(|(id, d)| (id.to_owned(), d)).collect();
        core.state.queue.push(item("kiln-queue"));
        core.state.tasks.push(Task { id: "t1".into(), queue: vec![item("sub-queue")], ..Task::default() });
        core.drop_unfinished(false);
        let now = |core: &Core| all.map(|(id, _)| core.state.delivery.get(id).copied());
        use Delivery::{Cancelled, Completed, Queued};
        assert_eq!(now(&core), [Some(Cancelled), Some(Cancelled), Some(Cancelled), Some(Completed), Some(Cancelled), Some(Queued), Some(Queued)]);
        core.drop_unfinished(true);
        assert_eq!(now(&core), [Some(Cancelled), Some(Cancelled), Some(Cancelled), Some(Completed), Some(Cancelled), Some(Cancelled), Some(Cancelled)]);
    }

    /// CP-T3.2: Claude Code's process ends (a crash, a stop by force) while it works on a message:
    /// that message ends dropped, never "being worked on" for good, and one waiting in kiln's
    /// queue behind it goes to the process after it.
    #[test]
    fn a_message_under_way_when_its_harness_ends_is_dropped_and_the_next_still_goes() {
        use super::super::model::{Delivery, Status};
        let mut core = Core::new();
        let mut claude = Adapter::Claude(Box::new(Claude::new(false)));
        claude.start(&mut core);
        let (_, init) = wrote(&mut core, "initialize");
        claude.line(&mut core, &serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": init, "response": {}}}).to_string());
        claude.command(&mut core, message("m1", "Plans and Research, one tab each"));
        claude.line(&mut core, r#"{"type":"command_lifecycle","command_uuid":"m1","state":"started","uuid":"x","session_id":"s"}"#);
        claude.command(&mut core, message("m2", "and a search"));
        assert_eq!((core.state.delivery.get("m1"), queue(&core)), (Some(&Delivery::Started), vec!["m2"]));
        claude.exited(&mut core, Some(137), None);
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
        assert_eq!(queue(&core), ["m2"]);
        core.state.exit = None;
        core.set_status(Status::Starting);
        let mut again = Adapter::Claude(Box::new(Claude::new(false)));
        again.start(&mut core);
        let (_, init) = wrote(&mut core, "initialize");
        again.line(&mut core, &serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": init, "response": {}}}).to_string());
        assert_eq!(wrote(&mut core, "").0, ["m2"]);
    }

    /// CP-T3.2: Edit and Unsend reach Codex's startup queue before and after a crash. A legacy
    /// message already shown in the conversation is changed or dropped there too.
    #[test]
    fn a_message_shown_waiting_in_kiln_s_queue_is_changed_and_taken_back_there() {
        use super::super::model::{Body, Delivery};
        let shown = |core: &Core, id: &str| match core.get(id).map(|e| &e.body) {
            Some(Body::User { text, .. }) => text.clone(),
            _ => String::new(),
        };
        for legacy in [false, true] {
            let mut core = Core::new();
            let mut codex = Adapter::Codex(Box::new(Codex::new(None, None, None, false)));
            codex.start(&mut core);
            codex.command(&mut core, message("m1", "Plans"));
            codex.command(&mut core, message("m2", "Research"));
            if legacy {
                for (id, text) in [("m1", "Plans"), ("m2", "Research")] {
                    core.state.delivery.insert(id.into(), Delivery::Queued);
                    core.upsert(id, None, Body::User { text: text.into(), images: 0, client_id: Some(id.into()), origin: None, attachments: Vec::new(), reply_to: None });
                }
            }
            codex.command(&mut core, Command::QueueRemove { id: "m2".into() });
            assert_eq!(queue(&core), ["m1"], "Unsend reaches startup work");
            assert_eq!(core.state.delivery.get("m2"), legacy.then_some(&Delivery::Cancelled));
            codex.exited(&mut core, None, None);
            codex.command(&mut core, Command::QueueEdit { id: "m1".into(), text: "Plans, one tab".into(), attachments: None });
            assert_eq!(core.state.queue.first().map(|q| q.text.as_str()), Some("Plans, one tab"));
            assert_eq!(shown(&core, "m1"), if legacy { "Plans, one tab" } else { "" });
            assert_eq!(shown(&core, "m2"), if legacy { "Research" } else { "" });
        }
    }
}
