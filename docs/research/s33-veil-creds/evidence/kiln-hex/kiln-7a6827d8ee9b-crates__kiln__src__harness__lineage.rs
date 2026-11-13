//! Agents that run agents (docs/agents.md, "Agents that run agents"): an agent an agent starts is
//! its child, and it gets no more than its parent has.
//!
//! - **Who the parent is** comes from how the caller reached kiln (its MCP token, its runner
//!   vouching for it, the machine's socket seeing its process: `Caller::agent`), never from what
//!   it asks. The child's spec carries it (`Spec::parent`), so it holds across restarts and moves.
//! - **No more than its parent.** Every harness's permission modes stand in one order: plan < ask
//!   < accept edits < auto < bypass ([`Level`]). A child's mode is at most its parent's (checked as
//!   it starts); bypass needs the parent's own `allow_bypass`; a parent whose commands run only on
//!   its own machine (`reach` off) has children that do too, on that machine.
//! - **Limits.** Chains go at most `[agents] max_depth` deep below the agent a person started, and
//!   one agent has at most as many subagents at work at once as its own Subagents setting says, one
//!   pool whatever each does ([`room`]); with no setting, no limit (unless kiln's config sets
//!   `[agents] max_children`).
//! - **Where.** `on` is a machine's name, or what it needs (`linux/amd64`, `+gpu`, `cpus>=16`),
//!   picked as reach picks: of the machines that suit and have the harness, the idlest.

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::kinds::{Kind, Protocol};
use super::model::Status;
use super::session::{Info, Isolation, Spec};
use crate::hub::Hub;

/// A refusal: an HTTP status and why, in words the caller can act on.
pub type Refused = (StatusCode, String);

fn refuse(msg: impl Into<String>) -> Refused {
    (StatusCode::FORBIDDEN, msg.into())
}

fn bad(msg: impl Into<String>) -> Refused {
    (StatusCode::BAD_REQUEST, msg.into())
}

/// How much a permission mode lets an agent do without asking a person, the same for every
/// harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// It reads and plans; it changes nothing.
    Plan,
    /// It asks before edits and commands.
    Ask,
    /// It edits files without asking; it asks before commands.
    AcceptEdits,
    /// It goes ahead with what's safe (a classifier, a sandbox) and asks about the rest.
    Auto,
    /// It skips every check.
    Bypass,
}

impl Level {
    pub fn words(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept edits",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }
}

/// The mode a harness starts in when none is given.
fn default_mode(k: &Kind) -> &'static str {
    if k.id == "devin" {
        return "accept-edits";
    }
    if k.id == "cursor" {
        return super::kinds::CURSOR_DEFAULT_MODE;
    }
    match k.protocol {
        Protocol::Acp(_) => super::acp::KILN_ASK,
        _ => k.modes().1,
    }
}

/// An ACP agent's own mode, placed by its words (each agent names its own: Gemini CLI's
/// `autoEdit` and `yolo`, opencode's `plan` and `build`, Goose's `approve` and `auto`).
pub(super) fn acp_level(mode: &str) -> Option<Level> {
    let m: String = mode.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let has = |w: &str| m.contains(w);
    Some(if has("yolo") || has("bypass") || has("danger") || has("fullaccess") || has("skip") {
        Level::Bypass
    } else if has("plan") || has("readonly") || has("read") || has("chat") {
        Level::Plan
    } else if has("edit") || has("accept") {
        Level::AcceptEdits
    } else if has("auto") || has("build") || has("smart") {
        Level::Auto
    } else if has("default") || has("ask") || has("approve") || has("manual") || has("normal") {
        Level::Ask
    } else {
        return None;
    })
}

/// Where mode `mode` of harness `kind` stands in [`Level`]'s order (an empty mode is the harness's
/// own default). `None`: a harness or mode kiln can't place.
pub fn level(kind: &str, mode: &str) -> Option<Level> {
    let k = Kind::get(kind)?;
    let mode = match mode.trim() {
        "" => default_mode(k),
        m => m,
    };
    match k.protocol {
        Protocol::ClaudeStreamJson => match mode {
            "plan" => Some(Level::Plan),
            // dontAsk never asks: it refuses whatever isn't already allowed.
            "default" | "dontAsk" => Some(Level::Ask),
            "acceptEdits" => Some(Level::AcceptEdits),
            "auto" => Some(Level::Auto),
            "bypassPermissions" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::CodexAppServer => match mode {
            "read-only" => Some(Level::Plan),
            "untrusted" => Some(Level::Ask),
            // It edits and runs commands in its folder's sandbox without asking.
            "auto" => Some(Level::Auto),
            "full-access" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::AgyStreamJson => match mode {
            super::antigravity::PLAN => Some(Level::Plan),
            super::antigravity::ASK => Some(Level::Ask),
            super::antigravity::ACCEPT_EDITS => Some(Level::AcceptEdits),
            super::antigravity::BYPASS => Some(Level::Bypass),
            _ => None,
        },
        // Its modes are the tools it starts with; it asks before none of them.
        Protocol::PiRpc => match mode {
            super::pi::READ_ONLY => Some(Level::Plan),
            super::pi::EDIT => Some(Level::AcceptEdits),
            super::pi::FULL => Some(Level::Bypass),
            _ => None,
        },
        // Cursor's Plan and Ask change nothing; its Agent has every tool and asks first for what its
        // own approval settings don't let go ahead, edits among them or not as those say: placed
        // with Accept edits, the most it may do unasked. kiln's Bypass answers every request yes.
        Protocol::Acp(_) if k.id == "cursor" => match mode {
            "plan" | "ask" => Some(Level::Plan),
            "agent" => Some(Level::AcceptEdits),
            super::acp::KILN_BYPASS => Some(Level::Bypass),
            _ => None,
        },
        Protocol::Acp(_) if k.id == "devin" => match mode {
            "plan" => Some(Level::Plan),
            "ask" => Some(Level::Ask),
            "accept-edits" => Some(Level::AcceptEdits),
            "smart" => Some(Level::Auto),
            "bypass" => Some(Level::Bypass),
            _ => None,
        },
        Protocol::Acp(_) => match mode {
            super::acp::KILN_ASK => Some(Level::Ask),
            super::acp::KILN_BYPASS => Some(Level::Bypass),
            other => acp_level(other),
        },
        Protocol::Terminal => None,
    }
}

/// Where an agent stands as a parent: its mode's level, an unplaceable one taken as ask (it asks
/// a person, as most modes do).
pub fn ceiling(parent: &Info) -> Level {
    level(&parent.kind, &parent.mode).unwrap_or(Level::Ask)
}

/// The modes of harness `k` kiln knows before it starts, with their levels, in its own order
/// (whether this machine could use bypass is its harness's to say as it starts).
fn modes_of(k: &Kind) -> Vec<(String, Level)> {
    let ids: Vec<String> = match k.protocol {
        Protocol::ClaudeStreamJson => super::claude::modes(true).into_iter().map(|m| m.id).collect(),
        Protocol::CodexAppServer => super::codex::modes(true).into_iter().map(|m| m.id).collect(),
        Protocol::AgyStreamJson => super::antigravity::modes().into_iter().map(|m| m.id).collect(),
        Protocol::PiRpc => super::pi::modes().into_iter().map(|m| m.id).collect(),
        Protocol::Acp(_) if k.id == "cursor" => super::kinds::cursor_modes().into_iter().map(|m| m.id).collect(),
        Protocol::Acp(_) if k.id == "devin" => ["plan", "ask", "accept-edits", "smart", "bypass"].map(str::to_owned).to_vec(),
        Protocol::Acp(_) => vec![super::acp::KILN_ASK.to_owned(), super::acp::KILN_BYPASS.to_owned()],
        Protocol::Terminal => Vec::new(),
    };
    ids.into_iter().filter_map(|id| level(k.id, &id).map(|l| (id, l))).collect()
}

/// The mode a child in harness `kind` starts in under a parent at `ceiling`: the one `asked` for,
/// when it's within it; with none asked, its harness's default when that's within it, else the
/// most its harness offers that is. Always one, so the harness's own settings (Claude Code's
/// `defaultMode`) can't start it higher. `bypass_ok`: the parent may use bypass itself. ACP agents
/// other than Devin and Cursor (whose modes kiln knows before they run) asked for nothing start in
/// their own default mode (kiln learns their modes as they run).
pub fn child_mode(kind: &str, asked: Option<&str>, ceiling: Level, bypass_ok: bool) -> Result<Option<String>, String> {
    let k = Kind::get(kind).ok_or_else(|| format!("unknown harness {kind:?}"))?;
    let known = modes_of(k);
    let within: Vec<&(String, Level)> = known.iter().filter(|(_, l)| *l <= ceiling && (*l != Level::Bypass || bypass_ok)).collect();
    let list = |v: &[&(String, Level)]| v.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ");
    match asked.map(str::trim).filter(|m| !m.is_empty()) {
        Some(mode) => {
            let Some(l) = level(kind, mode) else {
                return Err(format!("kiln can't tell where {}'s mode {mode:?} stands, so it can't hold it to its parent's (at most {}): pick one of {}", k.name, ceiling.words(), list(&within)));
            };
            let offered = matches!(k.protocol, Protocol::Acp(_)) || known.iter().any(|(id, _)| id == mode);
            if !offered {
                return Err(format!("{} has no mode {mode:?}: its modes are {}", k.name, known.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if l > ceiling {
                return Err(format!(
                    "an agent can't start one with more than it has: {mode} is {}, above its own {}. Pick a mode at most {}{}",
                    l.words(),
                    ceiling.words(),
                    ceiling.words(),
                    if within.is_empty() { String::new() } else { format!(": {}", list(&within)) }
                ));
            }
            if l == Level::Bypass && !bypass_ok {
                return Err("bypass needs its parent's own permission to skip checks, and its parent was started without it".to_owned());
            }
            Ok(Some(mode.to_owned()))
        }
        None if matches!(k.protocol, Protocol::Acp(_)) && k.id != "devin" && k.id != "cursor" && ceiling >= Level::Ask => Ok(None),
        None => {
            let default = default_mode(k);
            if within.iter().any(|(id, _)| id == default) {
                return Ok(Some(default.to_owned()));
            }
            let top = within.iter().map(|(_, l)| *l).max().ok_or_else(|| format!("{} has no mode at most {}, its parent's: start a harness that has one (Claude Code and Codex can plan)", k.name, ceiling.words()))?;
            within.iter().find(|(_, l)| *l == top).map(|(id, _)| Some(id.clone())).ok_or_else(|| format!("{} has no mode at most {}", k.name, ceiling.words()))
        }
    }
}

/// Where kiln keeps which agent started agent `id` (beside its spec, for one whose machine forgot).
pub fn parent_key(id: &str) -> String {
    format!("agents/parent/{id}")
}

/// How deep agent `id` is: how many agents started it, one from the other (0: a person did).
pub async fn depth(hub: &Hub, id: &str) -> u32 {
    let mut n = 0u32;
    let mut at = id.to_owned();
    // A chain never loops (a parent exists before its child), but a bound costs nothing.
    for _ in 0..64 {
        let parent = match hub.harness.info(&at).and_then(|i| i.parent) {
            Some(p) => Some(p),
            None => hub.store.kv_get(parent_key(&at)).await.ok().flatten(),
        };
        match parent {
            Some(p) if !p.is_empty() => {
                n = n.saturating_add(1);
                at = p;
            }
            _ => break,
        }
    }
    n
}

/// How far a caller reaches one agent session (docs/agents.md, "Whose agents").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Not at all: kiln answers as if it ran no such agent.
    None,
    /// It may list it, read it and watch it (an agent that works for the same person).
    See,
    /// It may also send to it, steer it, change, interrupt or end it.
    Act,
}

/// Whom a person calling acts as, in the form a session's opener is kept in (`owner`,
/// `user:<id>`): the owner's tokens, narrowed or not, are the owner's. `None` for a job or a peer.
pub fn person_of(caller: &crate::authz::Caller) -> Option<String> {
    use crate::authz::gate::Kind;
    match caller.kind {
        Kind::Job | Kind::Peer => None,
        Kind::Owner => Some("owner".to_owned()),
        _ => Some(caller.user.as_deref().map_or_else(|| "owner".to_owned(), |u| format!("user:{u}"))),
    }
}

/// The rule, on what kiln knows of the caller and of session `id` (`all`, the sessions kiln runs;
/// `mine` and `theirs`, whom the calling agent and the session act for):
/// - an agent acts on itself and the agents it started (and theirs), and sees the other agents
///   that work for the same person;
/// - the owner, and a person with `agents.manage` (`manager`), act on every agent;
/// - any other person acts on the agents that act for them (their agents' children do too) and
///   reaches no other.
///
/// A session kiln can't say whom it acts for is reached only by the owner and managers (on a kiln
/// nobody else signs in to, every session acts for the owner: `crate::mcp::acts_for`).
pub fn access_of(caller: &crate::authz::Caller, manager: bool, all: &[Info], id: &str, mine: Option<&str>, theirs: Option<&str>) -> Access {
    if let Some(me) = caller.agent.as_deref() {
        if me == id || descends(all, id, me) {
            return Access::Act;
        }
        return match (mine, theirs) {
            (Some(m), Some(t)) if m == t => Access::See,
            _ => Access::None,
        };
    }
    if caller.is_owner() || manager {
        return Access::Act;
    }
    match (person_of(caller), theirs) {
        (Some(p), Some(t)) if p == t => Access::Act,
        _ => Access::None,
    }
}

fn manages(caller: &crate::authz::Caller) -> bool {
    caller.can("agents.manage", &crate::authz::Scope::Server)
}

/// How far `caller` reaches agent session `id`, as [`access_of`] has it.
pub async fn access(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Access {
    // Whoever sees every session acts on each, whoever's it is: the store isn't asked whose. Asked
    // before every read of an agent's page, its waits were those reads' tail (goal 10, plan
    // 01a1a87b2dae step 2: an agent's events, goals and info at p99 100 to 400 ms, its list 1 ms).
    if sees_all(caller) {
        return Access::Act;
    }
    let mine = match caller.agent.as_deref() {
        Some(me) => crate::mcp::acts_for(hub, me).await,
        None => None,
    };
    let theirs = crate::mcp::whose(hub, id).await;
    // Only an agent's reach reads the lineage (whether `id` descends from it), and only `id`'s
    // line of it: not a copy of every session, sorted, on every request.
    let line = if caller.agent.as_deref().is_some_and(|me| me != id) { hub.harness.lineage_of(id) } else { Vec::new() };
    access_of(caller, manages(caller), &line, id, mine.as_deref(), theirs.as_deref())
}

/// What a caller is told of an agent it doesn't reach: the same as of one kiln doesn't run.
pub fn unseen() -> Refused {
    (StatusCode::NOT_FOUND, "no such agent session".to_owned())
}

/// Whether `caller` sees agent `id`: lists it, reads it, watches it.
pub async fn may_see(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<(), Refused> {
    if access(hub, caller, id).await >= Access::See { Ok(()) } else { Err(unseen()) }
}

/// The sessions of `all` that `caller` sees, in their order.
pub async fn seen(hub: &Hub, caller: &crate::authz::Caller, all: Vec<Info>) -> Vec<Info> {
    reached(hub, caller, all, Access::See).await
}

/// Whether `caller` sees every session, whatever it is: a person who owns the server or manages its
/// agents (an agent sees by lineage).
pub fn sees_all(caller: &crate::authz::Caller) -> bool {
    caller.agent.is_none() && (caller.is_owner() || manages(caller))
}

/// The sessions of `all` that `caller` reaches at least as far as `least`, in their order.
pub async fn reached(hub: &Hub, caller: &crate::authz::Caller, all: Vec<Info>, least: Access) -> Vec<Info> {
    if sees_all(caller) {
        return all;
    }
    let manager = manages(caller);
    let mine = match caller.agent.as_deref() {
        Some(me) => crate::mcp::acts_for(hub, me).await,
        None => None,
    };
    let mut out = Vec::with_capacity(all.len());
    for i in &all {
        let theirs = crate::mcp::whose(hub, &i.id).await;
        if access_of(caller, manager, &all, &i.id, mine.as_deref(), theirs.as_deref()) >= least {
            out.push(i.clone());
        }
    }
    out
}

/// The scope machine `target` is in: `local`, the server's own; `agent:<name>`, that runner's.
pub fn machine_scope(target: &str) -> crate::authz::Scope {
    match target.strip_prefix("agent:") {
        Some(runner) => crate::authz::Scope::Runner { runner: runner.to_owned() },
        None => crate::authz::Scope::Server,
    }
}

/// Whether `caller` may run programs on machine `target` as its terminal does (`runners.terminal`
/// there; the server's own scope for `local`): open an agent there, see and take over the sessions
/// found running there, read its history (docs/agents.md, "Whose agents"). `what` says what the
/// caller was doing, for the refusal.
pub fn may_run_on(caller: &crate::authz::Caller, target: &str, what: &str) -> Result<(), Refused> {
    let scope = machine_scope(target);
    if caller.can("runners.terminal", &scope) {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, format!("{} may not {what}: that needs runners.terminal on {}", caller.name, scope.text())))
}

/// Whether `caller` may steer agent `id`: send it messages, plans or goals, change or clear its
/// goals, interrupt or end it. An agent steers only itself and the agents it started (and theirs);
/// a person the agents that act for them, or every agent as the owner or with `agents.manage`.
pub async fn may_steer(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<(), Refused> {
    match (access(hub, caller, id).await, caller.agent.as_deref()) {
        (Access::Act, _) => Ok(()),
        (_, Some(me)) => Err((StatusCode::FORBIDDEN, format!("an agent steers and ends only the agents it started (and theirs): {} isn't one of {}'s", short(id), short(me)))),
        (_, None) => Err(unseen()),
    }
}

/// How an agent may send agent `id` a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A person, the agent itself, or an agent it started (and theirs): in any mode, as its own.
    Own,
    /// Another agent that works for the same person: steered or queued, its receiver told who sent it.
    Peer,
}

/// Whether `caller` may send agent `id` a message (docs/agents.md, "Messages between agents"): a
/// person the agents they act on ([`access`]); an agent itself and the agents it started in any
/// mode, as [`may_steer`] has it, and any other agent that works for the same person, as a steered
/// or queued message from it. Agents that work for someone else, or for someone kiln can't name,
/// aren't sent to.
pub async fn may_message(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Result<Reach, Refused> {
    let Some(me) = caller.agent.as_deref() else {
        return if access(hub, caller, id).await == Access::Act { Ok(Reach::Own) } else { Err(unseen()) };
    };
    if me == id || descends(&hub.harness.list(), id, me) {
        return Ok(Reach::Own);
    }
    match (crate::mcp::acts_for(hub, me).await, crate::mcp::acts_for(hub, id).await) {
        (Some(mine), Some(theirs)) if mine == theirs => Ok(Reach::Peer),
        _ => Err(refuse(format!("an agent messages the agents it started and those that work for the same person: {} works for someone else", short(id)))),
    }
}

/// How far up an agent's line [`descends`] looks: the sessions it reads, `id` and its ancestors,
/// at most (a line that loops ends there).
pub const LINE_MAX: usize = 64;

/// Whether agent `id` descends from agent `ancestor` (a child of it, or of one of its children…).
pub fn descends(all: &[Info], id: &str, ancestor: &str) -> bool {
    let mut at = id;
    for _ in 0..LINE_MAX {
        let Some(p) = all.iter().find(|i| i.id == at).and_then(|i| i.parent.as_deref()) else { return false };
        if p == ancestor {
            return true;
        }
        at = p;
    }
    false
}

/// Every agent kiln still lists that agent `id` started, and theirs, nearest first.
pub fn descendants(all: &[Info], id: &str) -> Vec<Info> {
    let mut out: Vec<Info> = Vec::new();
    let mut frontier = vec![id.to_owned()];
    while let Some(p) = frontier.pop() {
        for c in all.iter().filter(|i| i.parent.as_deref() == Some(p.as_str())) {
            if out.iter().any(|o| o.id == c.id) || c.id == id {
                continue;
            }
            frontier.push(c.id.clone());
            out.push(c.clone());
        }
    }
    out
}

/// The subagents kiln started under agent `id` (`Info::subagent`, each in its parent's chat), and
/// theirs, nearest first, ended or not: what a person's pause of it holds too (`Command::Pause`).
pub fn subagents_under(all: &[Info], id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut frontier = vec![id.to_owned()];
    while let Some(p) = frontier.pop() {
        for c in all.iter().filter(|i| i.subagent && i.parent.as_deref() == Some(p.as_str())) {
            if c.id == id || out.contains(&c.id) {
                continue;
            }
            frontier.push(c.id.clone());
            out.push(c.id.clone());
        }
    }
    out
}

/// What each session is sent when a person sets session `id`'s perms (`Command::Perms`): its own,
/// never more than its parent's; then each subagent kiln started under it, nearest first, its own
/// never more than its parent's new ones. Narrowing one narrows those under it; widening one
/// widens none (each was narrowed when it was set).
pub fn perms_down(all: &[Info], id: &str, perms: Option<&super::model::Perms>) -> Vec<(String, Option<super::model::Perms>)> {
    use super::model::Perms;
    let of = |x: &str| all.iter().find(|i| i.id == x);
    let parent = of(id).and_then(|i| i.parent.as_deref()).and_then(of).and_then(|p| p.perms.as_ref());
    let mut now: Vec<(String, Option<Perms>)> = vec![(id.to_owned(), Perms::within(perms, parent))];
    for sub in subagents_under(all, id) {
        let Some(info) = of(&sub) else { continue };
        let up = info.parent.as_deref().and_then(|p| now.iter().find(|(x, _)| x == p)).and_then(|(_, p)| p.as_ref());
        now.push((sub, Perms::within(info.perms.as_ref(), up)));
    }
    now
}

/// The agents still running that agent `id` started, and theirs, nearest first.
pub fn live_descendants(all: &[Info], id: &str) -> Vec<Info> {
    descendants(all, id).into_iter().filter(|c| c.status != Status::Exited).collect()
}

/// What an agent asks to start (`agents.start`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// The harness (`claude`, `codex`, …).
    pub kind: String,
    /// Where: a machine's name (`local` for the server), or what it needs (`linux/amd64`,
    /// `+gpu`). The caller's own machine when it's an agent, else the server or the best runner.
    pub on: Option<String>,
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub name: Option<String>,
    pub count: Option<usize>,
    /// `folder` (the folder itself, the only one there is until workspaces are built).
    pub workspace: Option<String>,
    /// `none`, a process on its machine (the default), or `microvm`, a microVM of its own (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d
    /// steps 17 and 32). An isolated agent's agents are isolated whatever it asks ([`isolation_for`]).
    pub isolation: Option<String>,
    /// A card's helper (`super::about`), when kiln starts one for a person (`agents.about`): never
    /// from what an agent asks (`agents.start` can't hide the agents it starts).
    #[serde(skip)]
    pub helper: Option<String>,
    /// Told the placer's words once a start waits in its queue for room ([`place_agents`]), so
    /// the caller answers then rather than past its own wait: never from what's asked.
    #[serde(skip)]
    pub queued: Queued,
}

/// Where a start waiting for room says so, once ([`Ask::queued`]).
#[derive(Debug, Clone, Default)]
pub struct Queued(Option<std::sync::Arc<parking_lot::Mutex<Option<futures::channel::oneshot::Sender<String>>>>>);

impl Queued {
    /// A notice, and what hears it.
    pub fn new() -> (Self, futures::channel::oneshot::Receiver<String>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        (Self(Some(std::sync::Arc::new(parking_lot::Mutex::new(Some(tx))))), rx)
    }

    fn say(&self, words: String) {
        if let Some(tx) = self.0.as_ref().and_then(|t| t.lock().take()) {
            let _ = tx.send(words);
        }
    }
}

/// Who starts agents: the agent that asks (none: a person), whom they act for, how the audit
/// trail names them, and the plan run they're for.
#[derive(Debug, Clone, Default)]
pub struct Starter {
    pub parent: Option<Info>,
    pub opener: Option<String>,
    pub actor: String,
    pub plan_run: Option<String>,
    /// Who asked, when a person or an agent did: it needs `runners.terminal` on the machine the
    /// agent is started on ([`may_run_on`]). None when kiln starts it (a plan's step, a goal's
    /// planner, an attachment).
    pub asker: Option<crate::authz::Caller>,
}

/// The server itself as a machine to pick (`local`), when it runs agents.
fn server_as_runner(hub: &Hub) -> Option<crate::reach::directory::RunnerInfo> {
    if !hub.cfg.terminal.enabled {
        return None;
    }
    let harnesses = hub.harness.kinds().get("local").map(|ks| ks.iter().filter(|k| k.available).map(|k| k.id.clone()).collect()).unwrap_or_default();
    Some(crate::reach::directory::RunnerInfo {
        name: "local".to_owned(),
        hostname: crate::os::hostname(),
        label: None,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        connected: true,
        reach: true,
        harnesses,
        ..crate::reach::directory::RunnerInfo::default()
    })
}

/// The machines that could run harness `kind` for `want`, best first (the idlest, as reach picks),
/// and why each other one was passed over. The server counts as `local`.
fn candidates(hub: &Hub, want: &crate::reach::directory::Want) -> (Vec<String>, Vec<String>) {
    let outposts: Vec<String> = hub.agents.load().values().filter(|a| a.outpost).map(|a| a.name.clone()).collect();
    let mut machines = hub.reach_directory();
    machines.retain(|r| !outposts.contains(&r.name));
    machines.extend(server_as_runner(hub));
    let mut ok: Vec<(f32, String)> = Vec::new();
    let mut passed: Vec<String> = Vec::new();
    for r in &machines {
        let local = r.name == "local";
        // The server is named `local` (or `server`) when asked for by name.
        let named = want.name.as_deref().is_some_and(|n| local && (n == "local" || n == "server" || n == r.hostname));
        let mut w = want.clone();
        if named {
            w.name = None;
        }
        let why_not = w.mismatch(r).or_else(|| (!r.connected).then(|| "offline".to_owned()));
        if let Some(why) = why_not {
            passed.push(format!("{}: {why}", r.shown()));
            continue;
        }
        // Its room as reach measures it (an overloaded machine only when none other suits); CI
        // slots aside, which agents don't take.
        let score = crate::reach::directory::room(r.strain(), 0, 0);
        ok.push((score, r.name.clone()));
    }
    ok.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    (ok.into_iter().map(|(_, n)| n).collect(), passed)
}

/// The target (`local`, `agent:<machine>`) agents of harness `kind` start on for `on`; none given,
/// `default` (the parent's machine), else the server, else the best runner that has the harness.
pub fn pick(hub: &Hub, on: Option<&str>, kind: &str, default: Option<&str>) -> Result<String, Refused> {
    let on = on.map(str::trim).filter(|o| !o.is_empty());
    let on = match (on, default) {
        (Some(o), _) => o.to_owned(),
        (None, Some(d)) => return Ok(d.to_owned()),
        (None, None) => String::new(),
    };
    // A runner kiln knows by its machine id is named by its label too (docs/machines.md).
    if let Some(m) = on.strip_prefix("agent:") {
        return Ok(format!("agent:{}", hub.machine_named(m).unwrap_or_else(|| m.to_owned())));
    }
    let mut want = crate::reach::directory::Want::parse(&on).map_err(|e| bad(format!("`on`: {e:#}")))?;
    // A machine named, and nothing else asked of it: that one (it says itself if it can't start
    // the harness), whatever its list of harnesses says yet.
    if let Some(name) = want.name.clone().filter(|_| want == crate::reach::directory::Want { name: want.name.clone(), ..Default::default() }) {
        if matches!(name.as_str(), "local" | "server") || (name == crate::os::hostname() && hub.machine_named(&name).is_none()) {
            return Ok("local".to_owned());
        }
        // A runner kiln knows by its machine id is named by its label too (docs/machines.md).
        let name = hub.machine_named(&name).unwrap_or(name);
        return match hub.agents.load().get(&name) {
            Some(a) if a.connected && !a.outpost && !a.ephemeral => Ok(format!("agent:{name}")),
            Some(a) => Err((StatusCode::CONFLICT, format!("{} isn't connected (or doesn't run agents): pick another machine", a.shown()))),
            None => Err((StatusCode::CONFLICT, format!("kiln has no machine named {name}"))),
        };
    }
    want.harness = Some(kind.to_owned());
    let (best, passed) = candidates(hub, &want);
    match best.into_iter().next() {
        Some(m) if m == "local" => Ok("local".to_owned()),
        Some(m) => Ok(format!("agent:{m}")),
        None => {
            let what = if on.is_empty() { format!("a machine with {kind}") } else { want.describe() };
            Err((StatusCode::CONFLICT, if passed.is_empty() { format!("no machine suits {what}: none is connected") } else { format!("no machine suits {what}: {}", passed.join("; ")) }))
        }
    }
}

/// The machine `on` names, as a target (`local`, `agent:<machine>`), when it names one and asks
/// nothing else of it; `None` when it asks for what a machine must be.
fn named(hub: &Hub, on: &str) -> Option<Result<String, Refused>> {
    if let Some(m) = on.strip_prefix("agent:") {
        return Some(Ok(format!("agent:{}", hub.machine_named(m).unwrap_or_else(|| m.to_owned()))));
    }
    let want = crate::reach::directory::Want::parse(on).ok()?;
    let name = want.name.clone().filter(|_| want == crate::reach::directory::Want { name: want.name.clone(), ..Default::default() })?;
    if matches!(name.as_str(), "local" | "server") || (name == crate::os::hostname() && hub.machine_named(&name).is_none()) {
        return Some(Ok("local".to_owned()));
    }
    let name = hub.machine_named(&name).unwrap_or(name);
    Some(match hub.agents.load().get(&name) {
        Some(a) if a.connected && !a.outpost && !a.ephemeral => Ok(format!("agent:{name}")),
        Some(a) => Err((StatusCode::CONFLICT, format!("{} isn't connected (or doesn't run agents): pick another machine", a.shown()))),
        None => Err((StatusCode::CONFLICT, format!("kiln has no machine named {name}"))),
    })
}

/// The machines agents of harness `kind` may start on for `on` (`default`: its parent's machine,
/// when `on` says nothing), as targets: the one named or the parent's, else every connected one
/// that suits `on` and has the harness, in no order. A filter, never a ranking (AUDIT M4): where
/// among them is placement's to choose.
fn may_start_on(hub: &Hub, on: Option<&str>, kind: &str, default: Option<&str>) -> Result<Vec<String>, Refused> {
    let on = on.map(str::trim).filter(|o| !o.is_empty());
    match (on, default) {
        (None, Some(d)) => return Ok(vec![d.to_owned()]),
        (Some(o), _) => {
            if let Some(t) = named(hub, o) {
                return t.map(|t| vec![t]);
            }
        }
        (None, None) => {}
    }
    let mut want = match on {
        Some(o) => crate::reach::directory::Want::parse(o).map_err(|e| bad(format!("`on`: {e:#}")))?,
        None => crate::reach::directory::Want::default(),
    };
    want.harness = Some(kind.to_owned());
    let (mut ok, passed) = candidates(hub, &want);
    if ok.is_empty() {
        let what = on.map_or_else(|| format!("a machine with {kind}"), |_| want.describe());
        return Err((StatusCode::CONFLICT, if passed.is_empty() { format!("no machine suits {what}: none is connected") } else { format!("no machine suits {what}: {}", passed.join("; ")) }));
    }
    ok.sort();
    Ok(ok.into_iter().map(|m| if m == "local" { m } else { format!("agent:{m}") }).collect())
}

/// Where each of `count` agents of harness `kind` starts for `on`, with its claim there (AUDIT M4,
/// docs/needs.md E5): placed as a runner's owner places its runs, by the runners' own admission,
/// in the same ledger as CI legs and commands (G1). Among the machines that can run it
/// ([`may_start_on`]); its owner the parent's runner, else one of those machines, else any runner
/// that places sessions (no central choice: the owner probes, the runners admit). It waits for
/// the owner's word with no deadline, nothing polling: queued, it starts when there's room; a
/// lost owner ends the wait at once. The server itself (`local`) isn't a runner and holds no
/// claim: it's taken when it alone can run it. A fleet with no runner that places sessions yet is
/// picked from as before ([`pick`]).
async fn place_agents(hub: &Hub, parent: Option<&Info>, on: Option<&str>, kind: &str, count: usize, isolated: bool, queued: &Queued) -> Result<Vec<(String, Option<crate::reach::place::Claim>)>, Refused> {
    use crate::reach::place::{SessionAsk, SessionWord};
    let default = parent.map(target_of);
    let targets = may_start_on(hub, on, kind, default.as_deref())?;
    let runners: Vec<String> = targets.iter().filter_map(|t| t.strip_prefix("agent:")).map(str::to_owned).collect();
    if runners.is_empty() {
        return Ok(vec![("local".to_owned(), None); count]);
    }
    let places = |n: &str| {
        let i = hub.instances.load();
        let speaking = i.speaking(crate::proto::feature::SESSIONS);
        i.of(n).iter().any(|p| speaking.contains(p))
    };
    let draw = |names: Vec<String>| names.get(usize::try_from(rand::random::<u64>().checked_rem(u64::try_from(names.len()).unwrap_or(0)).unwrap_or(0)).unwrap_or(0)).cloned();
    let connected: Vec<String> = hub.agents.load().values().filter(|a| a.connected && !a.outpost && !a.ephemeral).map(|a| a.name.clone()).collect();
    let owner = parent.and_then(|p| p.target.strip_prefix("agent:")).filter(|n| places(n)).map(str::to_owned).or_else(|| draw(runners.iter().filter(|n| places(n)).cloned().collect())).or_else(|| draw(connected.into_iter().filter(|n| places(n)).collect()));
    let Some(owner) = owner else {
        let t = pick(hub, on, kind, default.as_deref())?;
        return Ok(vec![(t, None); count]);
    };
    // What it needs of a machine beyond where it may go, as `on` spells it (`cpus=4`, `+gpu`).
    let mut needs = on.filter(|o| named(hub, o).is_none()).and_then(|o| kiln_ci::sched::needs::parse(o).ok()).unwrap_or_default();
    // An isolated agent goes only where its runner starts microVMs (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32).
    if isolated {
        needs.add("microvm").map_err(bad)?;
    }
    let actor = parent.map_or_else(|| "person".to_owned(), |p| p.id.clone());
    let mut out = Vec::new();
    for _ in 0..count {
        let ask = kiln_ci::place::Ask { run: format!("session-{}", crate::util::new_id()), needs: needs.clone(), class: kiln_ci::sched::state::Class::Interactive, actor: actor.clone(), among: runners.clone(), key: Some(format!("session:{kind}")), ..kiln_ci::place::Ask::default() };
        let json = serde_json::to_string(&SessionAsk::Ask { ask: Box::new(ask) }).map_err(|e| bad(e.to_string()))?;
        let Some((req, rx)) = hub.place_session(&owner, json).await else {
            let t = pick(hub, on, kind, default.as_deref())?;
            out.push((t, None));
            continue;
        };
        let placed = loop {
            let Ok(word) = rx.recv().await else {
                break Err((StatusCode::CONFLICT, format!("{}, which was placing it, went away before it was placed", hub.shown(&owner))));
            };
            match serde_json::from_str::<SessionWord>(&word) {
                Ok(SessionWord::Go { runner, claim }) => break Ok((format!("agent:{runner}"), Some(claim))),
                Ok(SessionWord::Answer { answer: a @ kiln_ci::place::Answer::Never { .. } }) => break Err((StatusCode::CONFLICT, a.words(crate::util::now_ms()))),
                Ok(SessionWord::Fallback { .. }) => break pick(hub, on, kind, default.as_deref()).map(|t| (t, None)),
                // Queued, its place and why: it waits there, and is told to go when there's room.
                // Who asked hears so now, not past its own wait (agents.start, 2026-11-11).
                Ok(SessionWord::Answer { answer }) => queued.say(answer.words(crate::util::now_ms())),
                Err(_) => {}
            }
        };
        hub.unplace_session(req);
        match placed {
            Ok(p) => out.push(p),
            Err(e) => {
                let_go(hub, &out).await;
                return Err(e);
            }
        }
    }
    Ok(out)
}

/// [`place_agents`], for reach's doors tests to drive with their lab's runners (docs/ci.md W05-R03).
#[cfg(all(test, unix))]
pub(crate) async fn place_agents_for_tests(hub: &Hub, parent: Option<&Info>, on: Option<&str>, kind: &str, count: usize) -> Result<Vec<(String, Option<crate::reach::place::Claim>)>, Refused> {
    place_agents(hub, parent, on, kind, count, false, &Queued::default()).await
}

/// The claims of agents placed that won't start (a check refused them): their owners let them go.
async fn let_go(hub: &Hub, placed: &[(String, Option<crate::reach::place::Claim>)]) {
    for c in placed.iter().filter_map(|(_, c)| c.as_ref()) {
        if let Ok(json) = serde_json::to_string(&crate::reach::place::SessionAsk::Cancel { run: c.run.clone() }) {
            hub.cancel_session(&c.owner, json).await;
        }
    }
}

async fn lacks(hub: &Hub, target: &str, kind: &'static Kind) -> bool {
    if !hub.harness.lacks(target, kind.id) {
        return false;
    }
    if target != "local" {
        return true;
    }
    crate::reach::serve::login_env().await;
    !crate::rt::blocking(move || kind.available()).await.unwrap_or(false)
}

fn harness(id: &str) -> Result<&'static Kind, Refused> {
    let kind = Kind::get(id).ok_or_else(|| bad(format!("unknown harness {id:?}: claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor or pi")))?;
    if !kind.structured() {
        return Err(bad(format!("{} has no structured mode", kind.name)));
    }
    Ok(kind)
}

pub async fn placed(hub: &Hub, parent: Option<&Info>, on: Option<&str>, kind: &str) -> Result<String, Refused> {
    let kind = harness(kind)?;
    let default_target = parent.map(|p| if p.target.is_empty() { "local".to_owned() } else { p.target.clone() });
    let target = pick(hub, on, kind.id, default_target.as_deref())?;
    if lacks(hub, &target, kind).await {
        return Err(not_installed(hub, &target, kind));
    }
    Ok(target)
}

fn not_installed(hub: &Hub, target: &str, kind: &Kind) -> Refused {
    let machine = target.strip_prefix("agent:").map_or_else(|| "the kiln server (`local`)".to_owned(), |m| hub.shown(m));
    let (with, _) = candidates(hub, &crate::reach::directory::Want { harness: Some(kind.id.to_owned()), ..Default::default() });
    // As people know them, as `on` takes them.
    let with: Vec<String> = with.iter().map(|m| hub.shown(m)).collect();
    let why = if with.is_empty() {
        format!("{} isn't installed on {machine}, nor on any other connected machine", kind.name)
    } else {
        format!("{} isn't installed on {machine}: pick a machine that has it with `on`: {}", kind.name, with.join(", "))
    };
    (StatusCode::CONFLICT, why)
}

/// The isolation an agent starts with (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 17 and 32): what was asked
/// (`none`, a process on its machine; `microvm`, a microVM of its own), and an isolated parent's,
/// which its agents keep whatever they ask: one asked as a process under it is refused.
pub fn isolation_for(asked: Option<&str>, parent: Option<&Info>) -> Result<Option<Isolation>, String> {
    let inherited = parent.and_then(|p| p.isolation.clone());
    match asked.map(str::trim) {
        None | Some("") => Ok(inherited),
        Some("none") => match inherited {
            Some(_) => Err("an isolated agent's agents are isolated too: it can't start one as a process on its machine".to_owned()),
            None => Ok(None),
        },
        Some(Isolation::MICROVM) => Ok(Some(inherited.unwrap_or_else(Isolation::microvm))),
        Some(other) => Err(format!("`isolation` is \"none\" (a process on its machine) or \"microvm\" (a microVM of its own), not {other:?}")),
    }
}

/// Whether agents on `target` (`local`, `agent:<machine>`) can be held isolated, in microVMs of
/// their own: its runner says it starts microVMs (`virt.microvm`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d
/// step 32); else why not. The server's own kiln starts none until it carries their runtime (step 16).
pub fn isolation_on(hub: &Hub, target: &str) -> Result<(), String> {
    let Some(machine) = target.strip_prefix("agent:") else {
        return Err("the server's kiln starts no microVM itself until it carries their runtime (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 16): pick a runner that does with `on` (`microvm`)".to_owned());
    };
    match hub.agents.load().get(machine).and_then(|a| a.facts.as_ref().map(|f| f.virt.microvm)) {
        Some(true) => Ok(()),
        Some(false) => Err(format!("{} doesn't start microVMs: its kiln starts them once it carries their runtime (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 16)", hub.shown(machine))),
        None => Err(format!("{} hasn't said yet whether it starts microVMs: try again in a minute", hub.shown(machine))),
    }
}

/// Whether agents on `target` (`local`, `agent:<machine>`) can have workspaces of their own: its
/// kiln asks its machine about folders, and its machine has Slates and can mount a volume.
pub async fn workspaces_on(hub: &Hub, target: &str) -> Result<(), String> {
    let facts = match target.strip_prefix("agent:") {
        None => crate::workspace::slates::facts().await,
        Some(machine) => {
            let caps = hub.instances.load().newest_with(machine, crate::proto::CAP_WORKSPACE).is_some();
            if !caps {
                return Err(format!("{}'s kiln is older than workspaces: upgrade it (System → Updates)", hub.shown(machine)));
            }
            let said = hub.agents.load().get(machine).and_then(|a| a.facts.as_ref().and_then(|f| f.workspaces.clone()));
            match said {
                Some(f) => f,
                None => return Err(format!("{} hasn't said yet whether it has Slates: try again in a minute", hub.shown(machine))),
            }
        }
    };
    if facts.available { Ok(()) } else { Err(facts.why_not.unwrap_or_else(|| "Slates isn't installed there".to_owned())) }
}

/// Whether `target` names the machine `info` runs on.
fn same_machine(info: &Info, target: &str) -> bool {
    info.target == target || (target == "local" && info.target.is_empty())
}

/// How many subagents `parent` has at work, and the most it may have at once: one pool, whatever
/// each does (docs/agents.md, "Agents that run agents"; docs/agentic.md P2). At work: the agents it
/// started that work, wait on a person, or are held to a goal still to go on a goal stack that runs
/// (between turns too), and its harness's own still going; one idle with its work done (a met
/// step's agent kept for review), or ended, holds no place. The most: its own Subagents setting, or
/// `[agents] max_children` when kiln's config sets one lower; neither set, there's no most at all.
struct AtOnce {
    at_work: usize,
    max: Option<usize>,
    /// Its own setting, when that's the most.
    own: Option<usize>,
}

/// Whether `child`, an agent its parent started, holds a place in its parent's pool (`held`: the
/// agents held to a goal still to go on a goal stack that runs).
fn in_pool(child: &Info, held: &[String]) -> bool {
    child.status != Status::Exited && (matches!(super::wait::stand(child), "working" | "needs_you") || held.contains(&child.id))
}

/// How many of `parent`'s harness's own subagents are still going, as its Agent tool's hook counts
/// them (`Task::live`).
fn own_at_work(parent: &Info) -> usize {
    parent.subagents.iter().filter(|s| matches!(s.status.as_str(), "running" | "pending" | "paused")).count()
}

/// The most subagents at work at once that `own` (an agent's Subagents setting) and `kiln` (`[agents]
/// max_children`) let it have, and its own when that's the most. Neither set: no most at all.
fn most(own: Option<u32>, kiln: Option<u32>) -> (Option<usize>, Option<usize>) {
    let n = |m: Option<u32>| m.and_then(|m| usize::try_from(m).ok());
    match (n(own), n(kiln)) {
        (Some(o), Some(k)) if k < o => (Some(k), None),
        (Some(o), _) => (Some(o), Some(o)),
        (None, k) => (k, None),
    }
}

/// Of agents `ids`, those held to a goal still to go on a goal stack that runs (docs/goals.md): at
/// work between turns too, as their stacks keep them at it. A read, its goals sought by agent and
/// status (`goal_items_by_status`, migration 20261106-2105_dbq_agents), not walked one by one.
async fn on_goals(hub: &Hub, ids: Vec<String>) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    let Ok(list) = serde_json::to_string(&ids) else { return Vec::new() };
    let held = hub
        .store
        .read(move |c| {
            let mut st = c.prepare_cached(
                "SELECT agent FROM goal_stacks s WHERE state = 'running' AND agent IN (SELECT value FROM json_each(?1))
                 AND EXISTS (SELECT 1 FROM goal_items i WHERE i.agent = s.agent AND i.status IN ('pending', 'active'))",
            )?;
            let v = st.query_map([list], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await;
    held.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "agents held to goals: not read");
        Vec::new()
    })
}

/// How many of the agents `parent` started hold a place in its pool now (at work, as [`room`]
/// counts them), its harness's own aside: what its harness's own limit is to add to the subagents it
/// counts itself, so the two count one pool.
pub async fn started_at_work(hub: &Hub, parent: &Info) -> usize {
    let children: Vec<Info> = hub.harness.list().into_iter().filter(|i| i.parent.as_deref() == Some(parent.id.as_str()) && i.status != Status::Exited).collect();
    let held = on_goals(hub, children.iter().map(|c| c.id.clone()).collect()).await;
    children.iter().filter(|c| in_pool(c, &held)).count()
}

async fn at_once(hub: &Hub, parent: &Info) -> AtOnce {
    let started = started_at_work(hub, parent).await;
    let (max, own) = most(parent.max_subagents, hub.cfg.agents.max_children);
    AtOnce { at_work: started.saturating_add(own_at_work(parent)), max, own }
}

/// How many more agents `parent` may start now ([`hold`]'s count): the most it may have at work at
/// once, less those at work; with no most, no end to them (`usize::MAX`). A plan's steps side by
/// side wait for room (docs/agentic.md P2).
pub async fn room(hub: &Hub, parent: &Info) -> usize {
    let a = at_once(hub, parent).await;
    a.max.map_or(usize::MAX, |m| m.saturating_sub(a.at_work))
}

/// Whether `parent` has subagents at work now, as [`room`] counts them: the room it lacks comes
/// back as they finish.
pub async fn busy(hub: &Hub, parent: &Info) -> bool {
    at_once(hub, parent).await.at_work > 0
}

/// The room an agent has for subagents, for what hands it work to go side by side (docs/agentic.md
/// P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    /// None, ever: it's as deep as agents may go (`[agents] max_depth`), or its limit is 0. It does
    /// its work itself.
    Never,
    /// No limit: neither its Subagents setting nor kiln's config sets one.
    Any,
    /// This many more may be at work now (0: until one of those at work finishes).
    Left(usize),
}

impl Room {
    /// As many more as may start now (`usize::MAX`: no end to them).
    pub fn more(self) -> usize {
        match self {
            Self::Never => 0,
            Self::Any => usize::MAX,
            Self::Left(n) => n,
        }
    }
}

/// [`Room`] for an agent with `at_work` subagents at work, at most `max` at once; `deep`: it's as
/// deep as agents may go.
fn room_in(deep: bool, max: Option<usize>, at_work: usize) -> Room {
    match max {
        _ if deep => Room::Never,
        Some(0) => Room::Never,
        Some(m) => Room::Left(m.saturating_sub(at_work)),
        None => Room::Any,
    }
}

/// The room `parent` has for subagents now ([`Room`]), counted as [`room`] counts it.
pub async fn room_of(hub: &Hub, parent: &Info) -> Room {
    let deep = depth(hub, &parent.id).await >= hub.cfg.agents.max_depth;
    let a = at_once(hub, parent).await;
    room_in(deep, a.max, a.at_work)
}

/// The limit `parent` starts agents within, in words: its own Subagents setting when that's the
/// most, else kiln's `[agents] max_children`; neither set, none.
pub async fn limit_words(hub: &Hub, parent: &Info) -> String {
    match at_once(hub, parent).await {
        AtOnce { own: Some(n), .. } => format!("its Subagents setting: {n} at once"),
        AtOnce { max: Some(n), .. } => format!("[agents] max_children: {n} at once"),
        AtOnce { max: None, .. } => "no limit".to_owned(),
    }
}

/// What a parent's children may be, checked before any starts: its depth, how many it runs, its
/// mode and its reach. Returns the child's mode and reach.
pub async fn hold(hub: &Hub, parent: &Info, ask: &Ask, target: &str, count: usize) -> Result<(Option<String>, Option<String>), Refused> {
    let limits = &hub.cfg.agents;
    let d = depth(hub, &parent.id).await;
    if d >= limits.max_depth {
        return Err(refuse(format!(
            "{} is {} agents deep below the one a person started, the most there may be ([agents] max_depth = {}): it can't start more",
            parent.name, d, limits.max_depth
        )));
    }
    let AtOnce { at_work, max, own } = at_once(hub, parent).await;
    if let Some(max) = max
        && at_work.saturating_add(count) > max
    {
        let s = if at_work == 1 { "" } else { "s" };
        return Err(refuse(match own {
            Some(_) => format!("{} has {at_work} subagent{s} at work, and its settings let it have at most {max} at once: wait for one to finish, or raise that in its Agent settings", parent.name),
            None => format!("{} has {at_work} subagent{s} at work, and one agent has at most {max} at once ([agents] max_children): wait for one to finish", parent.name),
        }));
    }
    let mode = child_mode(&ask.kind, ask.mode.as_deref(), ceiling(parent), !parent.no_bypass).map_err(refuse)?;
    // Its commands run only on its own machine: so do its children's, there.
    let reach = parent.reach.clone();
    if reach.as_deref() == Some("off") && !same_machine(parent, target) {
        return Err(refuse(format!("{}'s commands run only on its own machine (its reach is off), so the agents it starts run there too", parent.name)));
    }
    Ok((mode, reach))
}

/// Start agents for `by`: checked against its parent when it's an agent, on the machine picked,
/// each recorded with whom it acts for and who started it.
pub async fn start(hub: &Hub, by: &Starter, ask: Ask) -> Result<Vec<Info>, Refused> {
    start_as(hub, by, ask, |_| {}).await
}

/// The machine `parent` runs on, as a target (`local`, `agent:<machine>`).
fn target_of(parent: &Info) -> String {
    if parent.target.is_empty() { "local".to_owned() } else { parent.target.clone() }
}

/// What a subagent of `parent`'s is asked for where its own ask says nothing (`state`: its parent's
/// live state, as the server last had it): its parent's harness and, in that harness, its model,
/// effort and mode (a mode kiln can't place is left to the harness, within its parent's); its
/// parent's machine; one of it. With the speed (in its parent's harness) and the Subagents setting
/// it starts with, which only its spec takes.
fn as_its_parent(mut ask: Ask, parent: &Info, state: Option<&super::model::State>) -> (Ask, Option<String>, Option<u32>) {
    let some = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    let unset = |o: &Option<String>| o.as_deref().is_none_or(|s| s.trim().is_empty());
    if ask.kind.trim().is_empty() {
        ask.kind.clone_from(&parent.kind);
    }
    let same = ask.kind == parent.kind;
    let mut speed = None;
    if same {
        if unset(&ask.model) {
            ask.model = state.and_then(|s| some(&s.model)).or_else(|| some(&parent.model));
        }
        if unset(&ask.effort) {
            ask.effort = state.and_then(|s| some(&s.effort));
        }
        if unset(&ask.mode) {
            ask.mode = state.and_then(|s| some(&s.mode)).or_else(|| some(&parent.mode)).filter(|m| level(&ask.kind, m).is_some());
        }
        // Its speed, standard ("") too: the harness's own default may not be its parent's.
        speed = state.map(|s| s.speed.clone());
    }
    if unset(&ask.on) {
        ask.on = Some(target_of(parent));
    }
    ask.count = Some(1);
    (ask, speed, parent.max_subagents.or_else(|| state.and_then(|s| s.max_subagents)))
}

/// Starts a subagent of `by.parent`'s for one of its goals or plan steps (docs/goals.md, "A plan's
/// steps side by side"): its child, and its subagent besides (`Spec::subagent`), which shows in its
/// chat and on its row rather than as an agent of its own. It works where and as its parent does:
/// on its parent's machine (a plan run may place a step elsewhere: `ask.on`), never on one kiln
/// picks; in its folder there; in its harness and, there, with its model, effort, speed and mode
/// (at most its own, as every child's) unless `ask` says otherwise; with its Subagents setting, its
/// pause point and its way of ending a pause; and, opened beside it, with its environment and
/// program (`super::host::Host::open`), so it has its parent's account and configuration. It
/// doesn't go on elsewhere at a usage limit by itself: it stays with its parent.
pub async fn start_subagent(hub: &Hub, by: &Starter, ask: Ask) -> Result<Info, Refused> {
    let parent = by.parent.clone().ok_or_else(|| bad("a subagent is an agent's: kiln starts one only for an agent"))?;
    let state = hub.harness.state(&parent.id);
    let (ask, speed, most) = as_its_parent(ask, &parent, state.as_ref());
    let infos = start_as(hub, by, ask, |spec| {
        spec.subagent = true;
        spec.speed = speed;
        spec.max_subagents = most;
    })
    .await?;
    let info = infos.into_iter().next().ok_or_else(|| (StatusCode::CONFLICT, "the subagent didn't start".to_owned()))?;
    // Settings only its live state holds: given to it as a person would, at once (each holds from
    // its first turn, whether it has started yet or not).
    if let Some(s) = &state {
        if s.pause_at.is_some() {
            let _ = hub.harness.command(hub, &info.id, super::model::Command::PauseAt { percent: s.pause_at }).await;
        }
        if s.manual_resume {
            let _ = hub.harness.command(hub, &info.id, super::model::Command::AutoResume { on: false }).await;
        }
        // Its harness's other settings (its controls), once its harness lists them ([`settle_settings`]).
        let controls = settings_of(hub.harness.meta(&parent.id).as_ref(), s);
        if info.kind == parent.kind && !controls.is_empty() {
            let pending = json!({"controls": controls, "at": crate::util::now_ms()});
            let _ = hub.store.kv_set(format!("{LIKE}{}", info.id), pending.to_string()).await;
        }
    }
    Ok(info)
}

/// Where kiln keeps the settings a subagent is still to be given, by the subagent (`kv`).
const LIKE: &str = "agents/like/";
/// How long a subagent's harness has to list the settings it's to be given.
const LIKE_FOR_MS: i64 = 10 * 60_000;
/// Controls that are kiln's own, or another way of saying what a subagent is given already.
const NOT_SETTINGS: &[&str] = &["rename", "reach", "keep_going", "account", "reload_config"];

/// The settings among an agent's controls (`meta`'s; their values `state`'s): those that hold for
/// its session (a select, a toggle or words), not actions nor kiln's own, with the value it has now.
fn settings_of(meta: Option<&Value>, state: &super::model::State) -> serde_json::Map<String, Value> {
    let listed = meta.and_then(|m| m.get("controls")).and_then(Value::as_array).cloned().unwrap_or_default();
    let setting = |id: &str| {
        listed.iter().any(|c| {
            let is = |k: &str, v: &str| c.get(k).and_then(Value::as_str) == Some(v);
            is("id", id) && !is("kind", "action") && !is("scope", "turn")
        })
    };
    state.control_values.iter().filter(|(id, v)| !v.is_null() && !NOT_SETTINGS.contains(&id.as_str()) && setting(id)).map(|(id, v)| (id.clone(), v.clone())).collect()
}

/// Gives each subagent kiln started the settings of its parent's it's still to be given (its
/// harness's controls), each once its harness lists that control, as a person changing it would;
/// what isn't listed within [`LIKE_FOR_MS`], or a subagent gone, is let go. Each tick of the goal
/// driver.
pub async fn settle_settings(hub: &Hub) {
    let Ok(pending) = hub.store.kv_prefix(LIKE.to_owned()).await else { return };
    let now = crate::util::now_ms();
    for (key, value) in pending {
        let Some(child) = key.strip_prefix(LIKE) else { continue };
        let mut v: Value = serde_json::from_str(&value).unwrap_or(Value::Null);
        let at = v.get("at").and_then(Value::as_i64).unwrap_or(0);
        let listed: Vec<String> = hub.harness.meta(child).as_ref().and_then(|m| m.get("controls")).and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
        let mut left = serde_json::Map::new();
        let mut sent = false;
        if hub.harness.info(child).is_some() {
            for (id, value) in v.get_mut("controls").and_then(Value::as_object_mut).map(std::mem::take).unwrap_or_default() {
                if listed.contains(&id) {
                    let _ = hub.harness.command(hub, child, super::model::Command::Control { id, value: Some(value) }).await;
                    sent = true;
                } else {
                    left.insert(id, value);
                }
            }
        }
        if left.is_empty() || now.saturating_sub(at) > LIKE_FOR_MS {
            let _ = hub.store.kv_delete(key).await;
        } else if sent {
            let _ = hub.store.kv_set(key, json!({"controls": left, "at": at}).to_string()).await;
        }
    }
}

/// [`start`], the spec made for the ask then tuned by `tune` before anything starts.
async fn start_as(hub: &Hub, by: &Starter, ask: Ask, tune: impl FnOnce(&mut Spec) + Send) -> Result<Vec<Info>, Refused> {
    let kind = harness(&ask.kind)?;
    let volume = match ask.workspace.as_deref().map(str::trim) {
        None | Some("" | "folder") => false,
        Some("volume") => true,
        Some(w) => return Err(bad(format!("`workspace` is \"folder\" (its folder itself) or \"volume\" (a Slates volume of its own over the folder), not {w:?}"))),
    };
    let count = ask.count.unwrap_or(1).clamp(1, 16);
    // A parent's reach off holds its children to its machine.
    let on = by.parent.as_ref().filter(|p| p.reach.as_deref() == Some("off")).map(target_of).or_else(|| ask.on.clone());
    let isolation = isolation_for(ask.isolation.as_deref(), by.parent.as_ref()).map_err(bad)?;
    let placed = place_agents(hub, by.parent.as_ref(), on.as_deref(), kind.id, count, isolation.is_some(), &ask.queued).await?;
    let checked = async {
        let mut targets: Vec<&str> = placed.iter().map(|(t, _)| t.as_str()).collect();
        targets.dedup();
        for target in &targets {
            if let Some(asker) = &by.asker {
                may_run_on(asker, target, "start an agent there")?;
            }
            if lacks(hub, target, kind).await {
                return Err(not_installed(hub, target, kind));
            }
            if volume {
                workspaces_on(hub, target).await.map_err(|why| (StatusCode::CONFLICT, format!("an agent can't have a workspace of its own there: {why}")))?;
            }
            // Before anything starts, so its claims are let go: a fallback's pick is checked too.
            if isolation.is_some() {
                isolation_on(hub, target).map_err(|why| (StatusCode::CONFLICT, format!("an agent can't run isolated there: {why}")))?;
            }
            if *target == "local" && !hub.cfg.terminal.enabled {
                return Err(refuse("terminals (and so agents) are off on the server: pick a runner with `on`"));
            }
        }
        match &by.parent {
            Some(p) => {
                let mut held = (None, None);
                for target in &targets {
                    held = hold(hub, p, &ask, target, count).await?;
                }
                Ok::<_, Refused>((held.0, held.1, !p.no_bypass))
            }
            None => Ok((ask.mode.clone().filter(|m| !m.trim().is_empty()), None, true)),
        }
    };
    let (mode, reach, allow_bypass) = match checked.await {
        Ok(c) => c,
        Err(e) => {
            let_go(hub, &placed).await;
            return Err(e);
        }
    };
    let target = placed.first().map_or_else(|| "local".to_owned(), |(t, _)| t.clone());
    // Its folder: the one asked for, else its parent's where they share a machine, else home.
    let cwd = ask.cwd.clone().filter(|c| !c.trim().is_empty()).or_else(|| by.parent.as_ref().filter(|p| placed.iter().all(|(t, _)| same_machine(p, t))).map(|p| p.cwd.clone()).filter(|c| !c.is_empty()));
    let mut spec = Spec {
        kind: kind.id.to_owned(),
        cwd,
        model: ask.model.clone().filter(|m| !m.trim().is_empty()),
        mode,
        prompt: ask.prompt.clone().filter(|p| !p.trim().is_empty()),
        name: ask.name.clone().filter(|n| !n.trim().is_empty()),
        effort: ask.effort.clone().filter(|e| !e.trim().is_empty()),
        allow_bypass,
        reach,
        parent: by.parent.as_ref().map(|p| p.id.clone()),
        plan_run: by.plan_run.clone(),
        helper: ask.helper.clone(),
        workspace: volume.then(|| crate::workspace::Workspace { kind: "volume".to_owned(), ..Default::default() }),
        isolation,
        ..Spec::default()
    };
    tune(&mut spec);
    let what = if spec.subagent { format!("{} subagent", kind.name) } else { kind.name.to_owned() };
    // Each where it was placed, with its claim there (AUDIT M4); those placed alike open together.
    let mut infos = Vec::new();
    let mut at = 0;
    while let Some((t, claim)) = placed.get(at) {
        let n = if claim.is_none() { placed.iter().skip(at).take_while(|(u, c)| u == t && c.is_none()).count() } else { 1 };
        let spec = Spec { claim: claim.clone(), ..spec.clone() };
        match hub.harness.open(hub, t, spec, n).await {
            Ok(i) => infos.extend(i),
            Err(e) => {
                let_go(hub, placed.get(at..).unwrap_or_default()).await;
                if infos.is_empty() {
                    return Err((StatusCode::CONFLICT, format!("{e:#}")));
                }
                tracing::warn!(error = %format!("{e:#}"), "some of the agents asked for didn't start");
                break;
            }
        }
        at += n;
    }
    // Whose they are: the person who asked, whatever token they used; kiln's own starts are the
    // person they act for's.
    let person = by.asker.as_ref().and_then(person_of).or_else(|| by.opener.clone());
    for i in &infos {
        if let Some(o) = &by.opener {
            let _ = hub.store.kv_set(crate::mcp::opener_key(&i.id), o.clone()).await;
        }
        if let Some(p) = &person {
            let _ = hub.store.kv_set(crate::mcp::opened_by_key(&i.id), p.clone()).await;
        }
        if let Some(p) = &by.parent {
            let _ = hub.store.kv_set(parent_key(&i.id), p.id.clone()).await;
        }
        if ask.prompt.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            super::wait::sent(&i.id, None);
        }
    }
    // Whose they are says which installed skills they have (docs/agents.md, "Skills kiln installs").
    crate::api::boxed(|| crate::skills::sync(hub)).await;
    let parent = by.parent.as_ref().map_or_else(String::new, |p| format!(" for {} ({})", p.name, short(&p.id)));
    hub.store.audit(by.actor.clone(), "agent.open".into(), format!("{} {what} on {target}{parent}", infos.len())).await;
    Ok(infos)
}

/// An agent's id as lists shorten it.
pub fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// What a parent's live children are, for ending it (`agents.stop` with `children`, and the
/// console's End).
pub fn children_brief(hub: &Hub, id: &str) -> Value {
    let all = hub.harness.list();
    json!(live_descendants(&all, id).iter().filter(|i| i.helper.is_none()).map(|i| json!({"id": i.id, "name": i.name, "kind": i.kind, "status": i.status, "parent": i.parent})).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32: an agent is isolated as asked, an isolated agent's agents are
    /// isolated whatever they ask (its own image with them), and one asked as a process under it is refused.
    #[test]
    fn step32_an_isolated_agents_agents_are_isolated() -> Result<(), String> {
        let host = Info { id: "host-1".into(), ..Info::default() };
        let imaged = Isolation { image: Some("harness-claude".into()), ..Isolation::microvm() };
        let vm = Info { id: "vm-1".into(), isolation: Some(imaged.clone()), ..Info::default() };
        assert_eq!(isolation_for(None, None)?, None);
        assert_eq!(isolation_for(Some("none"), Some(&host))?, None);
        assert_eq!(isolation_for(Some("microvm"), None)?, Some(Isolation::microvm()));
        assert_eq!(isolation_for(None, Some(&vm))?, Some(imaged.clone()));
        assert_eq!(isolation_for(Some(" microvm "), Some(&vm))?, Some(imaged));
        assert!(isolation_for(Some("none"), Some(&vm)).is_err(), "a process under an isolated agent is refused");
        assert!(isolation_for(Some("container"), None).is_err(), "an isolation kiln doesn't know is refused");
        Ok(())
    }

    /// Step 32: an isolated agent goes only to a runner that says it starts microVMs; the server's
    /// kiln, a runner that says it doesn't, one that hasn't said yet and one kiln doesn't know each
    /// refuse it, saying why.
    #[test]
    fn step32_isolated_agents_go_only_where_the_runner_starts_microvms() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            let said = |name: &str, microvm: bool| crate::hub::AgentLive {
                facts: Some(Box::new(kiln_ci::facts::RunnerFacts { virt: kiln_ci::facts::Virt { microvm, ..Default::default() }, ..Default::default() })),
                ..crate::hub::tests::live_runner(name, "linux", "x86_64")
            };
            hub.put_agent(said("vm-box", true));
            hub.put_agent(said("plain", false));
            hub.put_agent(crate::hub::tests::live_runner("quiet", "linux", "x86_64"));
            anyhow::ensure!(isolation_on(&hub, "agent:vm-box").is_ok(), "a runner that starts microVMs holds it");
            for target in ["local", "agent:plain", "agent:quiet", "agent:nowhere"] {
                let why = isolation_on(&hub, target).err().unwrap_or_default();
                anyhow::ensure!(!why.is_empty(), "{target} refuses an isolated agent, saying why");
            }
            Ok(())
        })
    }

    /// How far `caller` reached `id` before goal 10 (plan 01a1a87b2dae step 2) gave the callers who
    /// see every session their answer without asking the store: what `access` must still answer.
    async fn access_before(hub: &Hub, caller: &crate::authz::Caller, id: &str) -> Access {
        let mine = match caller.agent.as_deref() {
            Some(me) => crate::mcp::acts_for(hub, me).await,
            None => None,
        };
        let theirs = crate::mcp::whose(hub, id).await;
        let line = if caller.agent.as_deref().is_some_and(|me| me != id) { hub.harness.lineage_of(id) } else { Vec::new() };
        access_of(caller, manages(caller), &line, id, mine.as_deref(), theirs.as_deref())
    }

    /// Goal 10, plan 01a1a87b2dae step 2: on every (caller, session), `access` decides as it did
    /// before; it asks the store nothing for a caller who sees every session (the owner, a manager)
    /// and still does for every other: an agent on its own line, its child's, another's, one unknown;
    /// people on theirs and another's; a manager denied it, a manager's token narrower than its role,
    /// an owner's token with a grant of its own.
    #[test]
    fn g10_s2_access_decides_as_before_and_asks_the_store_only_where_it_must() -> anyhow::Result<()> {
        use crate::authz::gate::{Caller, Kind as By, Rights};
        use crate::authz::{Grant, Scope};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = super::super::registry::Location::Machine("box".into());
            // a and its child c are u1's; b is the owner's; zz is no session at all.
            for (id, parent) in [("a", None), ("c", Some("a")), ("b", None)] {
                let info = Info { id: id.into(), kind: "claude".into(), parent: parent.map(str::to_owned), ..Info::default() };
                hub.harness.ingest(&there, id, super::super::core::Update::Open { info: Box::new(info), since: None }).await;
            }
            for (id, whose) in [("a", "user:u1"), ("c", "user:u1"), ("b", "owner")] {
                hub.store.kv_set(crate::mcp::opener_key(id), whose.into()).await?;
            }
            let grant = |what: &str, deny: bool| Grant { what: what.into(), scope: Scope::Server, deny };
            let rights = |grants: Vec<Grant>| Some(Rights { grants, custom: std::collections::HashMap::new() });
            let person = |name: &str, user: &str| {
                let mut c = Caller::with(By::User, name, rights(vec![grant("runs.read", false)]), None);
                c.user = Some(user.into());
                c
            };
            let agent = |me: &str| {
                let mut c = Caller::with(By::Agent, &format!("agent:{me}"), None, None);
                c.agent = Some(me.into());
                c.user = Some("u1".into());
                c
            };
            let callers = [
                ("the owner", Caller::owner(), true),
                ("a manager", Caller::with(By::User, "user:m", rights(vec![grant("agents.manage", false)]), None), true),
                ("a manager denied it", Caller::with(By::User, "user:m", rights(vec![grant("agents.manage", false), grant("agents.manage", true)]), None), false),
                ("a manager's narrower token", Caller::with(By::Pat, "pat:m", rights(vec![grant("agents.manage", false)]), rights(vec![grant("runs.read", false)])), false),
                ("an owner's token with a grant of its own", Caller::with(By::Pat, "pat:o", None, rights(vec![grant("runs.read", false)])), false),
                ("u1", person("user:u1", "u1"), false),
                ("u2", person("user:u2", "u2"), false),
                ("agent a", agent("a"), false),
                ("agent c", agent("c"), false),
                ("agent b", agent("b"), false),
            ];
            for (who, caller, sees_every) in &callers {
                assert_eq!(sees_all(caller), *sees_every, "{who}");
                for id in ["a", "c", "b", "zz"] {
                    let asked = hub.store.asked();
                    let now = access(&hub, caller, id).await;
                    let asked_it = hub.store.asked() > asked;
                    assert_eq!(now, access_before(&hub, caller, id).await, "{who} on {id}");
                    if *sees_every {
                        assert!(!asked_it, "{who} on {id}: the store asked");
                        assert_eq!(now, Access::Act, "{who} on {id}");
                    }
                }
            }
            // An agent acts on its own line, and not on another's or on none.
            assert_eq!(access(&hub, &agent("a"), "c").await, Access::Act, "its child");
            assert!(access(&hub, &agent("c"), "zz").await < Access::Act, "no session");
            Ok(())
        })
    }

    #[test]
    fn every_harness_s_modes_stand_in_one_order() {
        assert!(Level::Plan < Level::Ask && Level::Ask < Level::AcceptEdits && Level::AcceptEdits < Level::Auto && Level::Auto < Level::Bypass);
        assert_eq!(level("claude", "plan"), Some(Level::Plan));
        assert_eq!(level("claude", "default"), Some(Level::Ask));
        assert_eq!(level("claude", ""), Some(Level::Ask), "Claude Code's own default asks");
        assert_eq!(level("claude", "dontAsk"), Some(Level::Ask));
        assert_eq!(level("claude", "acceptEdits"), Some(Level::AcceptEdits));
        assert_eq!(level("claude", "auto"), Some(Level::Auto));
        assert_eq!(level("claude", "bypassPermissions"), Some(Level::Bypass));
        assert_eq!(level("codex", "read-only"), Some(Level::Plan));
        assert_eq!(level("codex", "untrusted"), Some(Level::Ask));
        assert_eq!(level("codex", ""), Some(Level::Auto), "Codex's own default works in its sandbox unasked");
        assert_eq!(level("codex", "full-access"), Some(Level::Bypass));
        assert_eq!(level("antigravity", "plan"), Some(Level::Plan));
        assert_eq!(level("antigravity", "default"), Some(Level::Ask));
        assert_eq!(level("antigravity", ""), Some(Level::AcceptEdits));
        assert_eq!(level("antigravity", "bypass"), Some(Level::Bypass));
        assert_eq!(level("gemini", "kiln-ask"), Some(Level::Ask));
        assert_eq!(level("gemini", "kiln-bypass"), Some(Level::Bypass));
        assert_eq!(level("gemini", "yolo"), Some(Level::Bypass));
        assert_eq!(level("gemini", "autoEdit"), Some(Level::AcceptEdits));
        assert_eq!(level("opencode", "plan"), Some(Level::Plan));
        assert_eq!(level("opencode", "build"), Some(Level::Auto));
        assert_eq!(level("claude", "turbo"), None);
        assert_eq!(level("nonsense", "plan"), None);
    }

    #[test]
    fn a_child_gets_no_more_than_its_parent() {
        // Asked for bypass under a parent that asks: refused, saying what it may have.
        let e = child_mode("claude", Some("bypassPermissions"), Level::Ask, true).err().unwrap_or_default();
        assert!(e.contains("bypassPermissions is bypass, above its own ask") && e.contains("default"), "{e}");
        assert!(child_mode("codex", Some("full-access"), Level::Ask, true).is_err());
        assert!(child_mode("codex", Some("auto"), Level::AcceptEdits, true).is_err(), "Codex's auto is above accept edits");
        // Within it: as asked.
        assert_eq!(child_mode("claude", Some("plan"), Level::Ask, true), Ok(Some("plan".to_owned())));
        assert_eq!(child_mode("claude", Some("acceptEdits"), Level::Auto, true), Ok(Some("acceptEdits".to_owned())));
        // Bypass under bypass needs the parent's own allow_bypass.
        assert_eq!(child_mode("claude", Some("bypassPermissions"), Level::Bypass, true), Ok(Some("bypassPermissions".to_owned())));
        assert!(child_mode("claude", Some("bypassPermissions"), Level::Bypass, false).is_err());
        // Nothing asked: the harness's default when it's within, else the most that is.
        assert_eq!(child_mode("claude", None, Level::Ask, true), Ok(Some("default".to_owned())));
        assert_eq!(child_mode("claude", None, Level::Plan, true), Ok(Some("plan".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Ask, true), Ok(Some("untrusted".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Plan, true), Ok(Some("read-only".to_owned())));
        assert_eq!(child_mode("codex", None, Level::Bypass, true), Ok(Some("auto".to_owned())), "its default, not the most it could have");
        assert_eq!(child_mode("antigravity", None, Level::Ask, true), Ok(Some("default".to_owned())));
        // An ACP agent below ask has nothing kiln knows to start it in.
        assert!(child_mode("gemini", None, Level::Plan, true).is_err());
        assert_eq!(child_mode("gemini", None, Level::Ask, true), Ok(None));
        // Unknown modes and modes a harness doesn't have are refused, never let through.
        assert!(child_mode("claude", Some("turbo"), Level::Bypass, true).is_err());
        assert!(child_mode("codex", Some("acceptEdits"), Level::Bypass, true).is_err(), "Codex has no acceptEdits");
    }

    #[test]
    fn devin_children_keep_native_modes_within_their_parents_permissions() {
        assert_eq!(level("devin", ""), Some(Level::AcceptEdits));
        for (mode, l) in [("plan", Level::Plan), ("ask", Level::Ask), ("accept-edits", Level::AcceptEdits), ("smart", Level::Auto), ("bypass", Level::Bypass)] {
            assert_eq!(level("devin", mode), Some(l));
            assert_eq!(child_mode("devin", Some(mode), l, true), Ok(Some(mode.to_owned())));
        }
        assert_eq!(child_mode("devin", None, Level::Plan, false), Ok(Some("plan".into())));
        assert_eq!(child_mode("devin", None, Level::Ask, false), Ok(Some("ask".into())));
        assert_eq!(child_mode("devin", None, Level::AcceptEdits, false), Ok(Some("accept-edits".into())));
        assert_eq!(child_mode("devin", None, Level::Bypass, true), Ok(Some("accept-edits".into())));
        assert!(child_mode("devin", Some("accept-edits"), Level::Ask, true).is_err());
        assert!(child_mode("devin", Some("smart"), Level::AcceptEdits, true).is_err());
        assert!(child_mode("devin", Some("bypass"), Level::Auto, true).is_err());
        assert!(child_mode("devin", Some("bypass"), Level::Bypass, false).is_err());
        for unknown in ["kiln-ask", "kiln-bypass", "auto", "code", "readonly"] {
            assert_eq!(level("devin", unknown), None);
            assert!(child_mode("devin", Some(unknown), Level::Bypass, true).is_err());
        }
    }

    fn info(id: &str, parent: Option<&str>, status: Status) -> Info {
        Info { id: id.into(), parent: parent.map(str::to_owned), status, ..Info::default() }
    }

    /// A goal's or plan step's subagent goes where and as its agent does (its harness, machine,
    /// model, effort, speed, mode and Subagents setting), whatever kiln could pick: an agent at max
    /// effort and fast hands its steps to subagents at max effort and fast, on its own machine. What
    /// a plan run says for a step stays (another machine, another harness, another model); in
    /// another harness its parent's model, effort, mode and speed aren't its; a mode kiln can't place
    /// is left to the harness (within its parent's).
    #[test]
    fn a_subagent_goes_where_and_as_its_agent_does() {
        let parent = Info { kind: "devin".into(), target: "agent:studio".into(), model: "opus-medium".into(), mode: "bypass".into(), max_subagents: Some(3), ..info("main", None, Status::Running) };
        let state = super::super::model::State { model: "opus-medium".into(), mode: "bypass".into(), effort: "max".into(), speed: "fast".into(), ..Default::default() };
        let (ask, speed, most) = as_its_parent(Ask { name: Some("Step 2: API".into()), ..Ask::default() }, &parent, Some(&state));
        assert_eq!((ask.kind.as_str(), ask.on.as_deref(), ask.model.as_deref(), ask.effort.as_deref(), ask.mode.as_deref(), ask.count), ("devin", Some("agent:studio"), Some("opus-medium"), Some("max"), Some("bypass"), Some(1)));
        assert_eq!((speed.as_deref(), most, ask.name.as_deref()), (Some("fast"), Some(3), Some("Step 2: API")));
        // Standard speed is its parent's too, and the Subagents setting its live state holds.
        let standard = super::super::model::State { speed: String::new(), max_subagents: Some(2), ..state.clone() };
        let (_, speed, most) = as_its_parent(Ask::default(), &Info { max_subagents: None, ..parent.clone() }, Some(&standard));
        assert_eq!((speed.as_deref(), most), (Some(""), Some(2)));
        // What the run says for a step stays.
        let asked = Ask { kind: "devin".into(), on: Some("linux/amd64".into()), model: Some("swe-1".into()), effort: Some("low".into()), mode: Some("ask".into()), ..Ask::default() };
        let (ask, ..) = as_its_parent(asked, &parent, Some(&state));
        assert_eq!((ask.on.as_deref(), ask.model.as_deref(), ask.effort.as_deref(), ask.mode.as_deref()), (Some("linux/amd64"), Some("swe-1"), Some("low"), Some("ask")));
        // Another harness: its parent's machine, nothing of its parent's harness.
        let (ask, speed, _) = as_its_parent(Ask { kind: "claude".into(), ..Ask::default() }, &parent, Some(&state));
        assert_eq!((ask.on.as_deref(), ask.model, ask.effort, ask.mode, speed), (Some("agent:studio"), None, None, None, None));
        // On the server itself; its live state not read yet: its info's model and mode.
        let (ask, speed, _) = as_its_parent(Ask::default(), &Info { target: String::new(), ..parent.clone() }, None);
        assert_eq!((ask.on.as_deref(), ask.model.as_deref(), ask.mode.as_deref(), ask.effort, speed), (Some("local"), Some("opus-medium"), Some("bypass"), None, None));
        // A mode kiln can't place isn't handed on: its harness's default within its parent's.
        let odd = Info { mode: "turbo".into(), ..parent.clone() };
        let (ask, ..) = as_its_parent(Ask::default(), &odd, Some(&super::super::model::State { mode: "turbo".into(), ..state }));
        assert_eq!(ask.mode, None);
    }

    /// What a subagent is given of its agent's controls: its session settings (a select, a toggle,
    /// words) as they are now; not actions, not a prompt's, not kiln's own, not one left unset.
    #[test]
    fn a_subagent_is_given_its_agent_s_session_settings_alone() {
        let meta = json!({"controls": [
            {"id": "approval_policy", "kind": "select"}, {"id": "web_search", "kind": "toggle"}, {"id": "notes", "kind": "text"},
            {"id": "compact", "kind": "action"}, {"id": "rewind", "kind": "select", "scope": "turn"}, {"id": "rename", "kind": "text"}, {"id": "unset", "kind": "toggle"},
        ]});
        let mut state = super::super::model::State::default();
        for (id, v) in [("approval_policy", json!("on-request")), ("web_search", json!(true)), ("notes", json!("terse")), ("compact", json!("x")), ("rewind", json!("e1")), ("rename", json!("Main")), ("unset", Value::Null), ("unlisted", json!(1))] {
            state.control_values.insert(id.to_owned(), v);
        }
        let got = settings_of(Some(&meta), &state);
        assert_eq!(Value::Object(got), json!({"approval_policy": "on-request", "web_search": true, "notes": "terse"}));
        assert!(settings_of(None, &state).is_empty(), "with no controls listed, nothing");
    }

    #[test]
    fn descendants_are_followed_down_the_chain() {
        let all = vec![info("a", None, Status::Idle), info("b", Some("a"), Status::Running), info("c", Some("b"), Status::Idle), info("d", Some("a"), Status::Exited), info("e", Some("d"), Status::Running), info("x", None, Status::Idle)];
        assert!(descends(&all, "c", "a") && descends(&all, "b", "a") && descends(&all, "e", "a"));
        assert!(!descends(&all, "a", "a") && !descends(&all, "x", "a") && !descends(&all, "a", "c"));
        let live: Vec<String> = live_descendants(&all, "a").into_iter().map(|i| i.id).collect();
        assert!(live.contains(&"b".to_owned()) && live.contains(&"c".to_owned()) && live.contains(&"e".to_owned()));
        assert!(!live.contains(&"d".to_owned()), "an ended child isn't live (its own children are)");
        assert!(live_descendants(&all, "x").is_empty());
        assert_eq!(descendants(&all, "a").len(), 4, "ended ones too, for ending them with their parent");
    }

    #[test]
    fn a_pause_holds_the_subagents_kiln_started_under_it_and_theirs_not_agents_it_started_beside_it() {
        let sub = |id: &str, parent: &str, status: Status| Info { subagent: true, ..info(id, Some(parent), status) };
        let all = vec![info("lead", None, Status::Idle), sub("s1", "lead", Status::Running), sub("s1a", "s1", Status::Running), sub("s2", "lead", Status::Exited), info("own", Some("lead"), Status::Running), sub("under-own", "own", Status::Running)];
        let mut under = subagents_under(&all, "lead");
        under.sort();
        assert_eq!(under, ["s1", "s1a", "s2"], "an agent it started as one of its own (not a subagent) isn't under its pause, nor what that started");
        assert_eq!(subagents_under(&all, "s1"), ["s1a"]);
        assert!(subagents_under(&all, "s1a").is_empty());
    }

    #[test]
    fn a_session_s_perms_are_its_parent_s_at_most_and_narrow_the_subagents_under_it() {
        use super::super::model::{Grant, Perms};
        let sub = |id: &str, parent: &str, perms: Option<Perms>| Info { subagent: true, perms, ..info(id, Some(parent), Status::Running) };
        let lead = Info { perms: Some(Perms { run: Grant::Ask, ..Perms::default() }), ..info("lead", None, Status::Idle) };
        let all = vec![lead, sub("s1", "lead", None), sub("s1a", "s1", Some(Perms { web: Grant::Off, ..Perms::default() })), sub("s2", "lead", None)];
        let sent = perms_down(&all, "s1", Some(&Perms { edit: Grant::Off, ..Perms::default() }));
        let by = |id: &str| sent.iter().find(|(x, _)| x == id).and_then(|(_, p)| p.clone());
        // Its own, held to its parent's Ask on run.
        assert_eq!(by("s1"), Some(Perms { edit: Grant::Off, run: Grant::Ask, ..Perms::default() }));
        // Under it: its own Off on web kept, and its new parent's narrowing on top.
        assert_eq!(by("s1a"), Some(Perms { edit: Grant::Off, run: Grant::Ask, web: Grant::Off, ..Perms::default() }));
        assert!(by("s2").is_none() && sent.len() == 2, "a sibling isn't touched");
        // Cleared, it's its parent's.
        assert_eq!(perms_down(&all, "s1", None).first().and_then(|(_, p)| p.clone()), Some(Perms { run: Grant::Ask, ..Perms::default() }));
    }

    /// docs/agentic.md P2 (Ada's decisions 4 and 6, 2026-10-24): one pool of subagents at work per
    /// agent, whatever each does. An agent it started holds a place while it works, waits on a
    /// person, or is held to a goal still to go (between turns too); idle with its work done (a met
    /// step's agent kept for review), or ended, it holds none. Its harness's own hold one while they
    /// go. The most is its own Subagents setting; with none there's no most at all, unless kiln's
    /// config sets `[agents] max_children`, which holds it when lower.
    #[test]
    fn p2_one_pool_of_subagents_at_work_and_no_setting_is_no_limit() {
        let held = ["pool-between".to_owned()];
        let child = |id: &str, status: Status| info(id, Some("p"), status);
        for (id, status) in [("pool-working", Status::Running), ("pool-starting", Status::Starting), ("pool-asks", Status::Waiting), ("pool-between", Status::Idle)] {
            assert!(in_pool(&child(id, status), &held), "{id}");
        }
        assert!(!in_pool(&child("pool-done", Status::Idle), &held), "idle, its work done");
        assert!(!in_pool(&child("pool-between", Status::Exited), &held), "ended, though its stack still holds the goal");
        let sub = |status: &str| super::super::session::SubagentBrief { id: status.into(), tool_id: None, description: String::new(), status: status.into(), parent_task: None, started: 0, ended: None, last_tool: None, pausable: false, paused_at: None, perms: None };
        let parent = Info { subagents: ["running", "pending", "paused", "completed", "failed"].into_iter().map(sub).collect(), ..info("p", None, Status::Running) };
        assert_eq!(own_at_work(&parent), 3, "its harness's own still going");
        assert_eq!(most(None, None), (None, None), "no setting: no limit at all");
        assert_eq!(most(Some(3), None), (Some(3), Some(3)));
        assert_eq!(most(Some(3), Some(24)), (Some(3), Some(3)));
        assert_eq!(most(Some(30), Some(24)), (Some(24), None), "kiln's config, set lower, holds it");
        assert_eq!(most(None, Some(24)), (Some(24), None));
        // What hands work out side by side: none ever as deep as agents may go, or at a limit of 0
        // (its work its own), else the room left, which comes back as those at work finish.
        assert_eq!(room_in(false, None, 7), Room::Any);
        assert_eq!(room_in(false, Some(3), 1), Room::Left(2));
        assert_eq!(room_in(false, Some(3), 5), Room::Left(0), "full: until one finishes");
        assert_eq!(room_in(false, Some(0), 0), Room::Never);
        assert_eq!(room_in(true, None, 0), Room::Never);
        assert_eq!((Room::Never.more(), Room::Any.more(), Room::Left(2).more()), (0, usize::MAX, 2));
    }

    /// The agents held to a goal still to go on a goal stack that runs, as the store has them: not
    /// one whose stack is paused, one whose goals are all over, nor one with no stack.
    #[test]
    fn p2_agents_held_to_goals_are_read_from_their_stacks() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.store
                .call(|c| {
                    for (agent, state, status) in [("on", "running", "active"), ("next", "running", "pending"), ("held", "paused", "pending"), ("over", "running", "met")] {
                        c.execute("INSERT INTO goal_stacks (agent, state, created_at, updated_at) VALUES (?1, ?2, 0, 0)", rusqlite::params![agent, state])?;
                        c.execute("INSERT INTO goal_items (id, agent, pos, text, status) VALUES (?1, ?1, 0, 'a goal', ?2)", rusqlite::params![agent, status])?;
                    }
                    Ok(())
                })
                .await?;
            let mut held = on_goals(&hub, ["on", "next", "held", "over", "none"].map(str::to_owned).to_vec()).await;
            held.sort();
            anyhow::ensure!(held == ["next", "on"], "{held:?}");
            anyhow::ensure!(on_goals(&hub, Vec::new()).await.is_empty());
            Ok(())
        })
    }

    #[test]
    fn an_agent_never_starts_on_a_machine_kiln_knows_lacks_its_harness() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let offering = |ids: &[&str]| {
                super::super::kinds::offered()
                    .into_iter()
                    .map(|mut k| {
                        k.available = ids.contains(&k.id.as_str());
                        k
                    })
                    .collect::<Vec<_>>()
            };
            for (name, os, arch) in [("box", "linux", "x86_64"), ("studio", "macos", "aarch64"), ("new", "linux", "x86_64")] {
                hub.put_agent(crate::hub::tests::live_runner(name, os, arch));
            }
            hub.harness.set_kinds("local", offering(&["claude"]));
            hub.harness.set_kinds("agent:box", offering(&["claude", "devin"]));
            hub.harness.set_kinds("agent:studio", offering(&["claude"]));
            let person = Starter { actor: "admin".into(), ..Starter::default() };
            let ask = |kind: &str, on: Option<&str>| Ask { kind: kind.into(), on: on.map(str::to_owned), ..Ask::default() };
            for on in ["studio", "agent:studio"] {
                let (code, why) = start(&hub, &person, ask("devin", Some(on))).await.err().ok_or_else(|| anyhow::anyhow!("started on {on}"))?;
                anyhow::ensure!(code == StatusCode::CONFLICT && why == "Devin isn't installed on studio: pick a machine that has it with `on`: box", "{on}: {why}");
            }
            let parent = Info { id: "p".into(), kind: "claude".into(), target: "agent:studio".into(), mode: "bypassPermissions".into(), ..Info::default() };
            let child = Starter { parent: Some(parent), actor: "agent:p".into(), ..Starter::default() };
            let why = start(&hub, &child, ask("devin", None)).await.err().map(|(_, w)| w).unwrap_or_default();
            anyhow::ensure!(why == "Devin isn't installed on studio: pick a machine that has it with `on`: box", "{why}");
            for on in ["local", "server"] {
                let why = start(&hub, &person, ask("amp", Some(on))).await.err().map(|(_, w)| w).unwrap_or_default();
                anyhow::ensure!(why == "Amp isn't installed on the kiln server (`local`), nor on any other connected machine", "{on}: {why}");
            }
            anyhow::ensure!(!hub.harness.lacks("agent:new", "devin") && !hub.harness.lacks("agent:box", "devin") && hub.harness.lacks("agent:box", "codex"));
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            anyhow::ensure!(!hub.harness.lacks("agent:studio", "devin"), "tests' sessions run no harness, whatever the machine has");
            Ok(())
        })
    }

    /// docs/machines.md: a runner kiln knows by its machine id is picked by its label as by its id.
    #[test]
    fn a_runner_known_by_its_machine_id_is_picked_by_the_name_people_see() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let id = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
            let mut laptop = crate::hub::tests::live_runner(id, "macos", "aarch64");
            laptop.label = Some("adas-macbook-pro-3.local".into());
            hub.put_agent(laptop);
            let there = format!("agent:{id}");
            for on in ["adas-macbook-pro-3.local", id] {
                anyhow::ensure!(pick(&hub, Some(on), "claude", None).ok().as_ref() == Some(&there), "{on}");
            }
            anyhow::ensure!(pick(&hub, Some("adas-macbook-pro.local"), "claude", None).is_err(), "another machine's name");
            Ok(())
        })
    }

    /// docs/agents.md, "Whose agents": who sees and who acts on which agent session.
    #[test]
    fn whose_agents_a_caller_reaches() {
        use crate::authz::Caller;
        use crate::authz::gate::{Kind, Rights};
        let info = |id: &str, parent: Option<&str>| Info { id: id.into(), parent: parent.map(str::to_owned), ..Default::default() };
        // `a` was opened by user u1; `a1` is its child; `b` was opened by user u2.
        let all = vec![info("a", None), info("a1", Some("a")), info("b", None)];
        let person = |user: &str| {
            let mut c = Caller::with(Kind::User, &format!("user:{user}"), Some(Rights::default()), None);
            c.user = Some(user.to_owned());
            c
        };
        let agent = |session: &str, user: &str| {
            let mut c = person(user);
            c.kind = Kind::Agent;
            c.agent = Some(session.to_owned());
            c
        };
        let (u1, u2) = (Some("user:u1"), Some("user:u2"));

        // The owner, and a person with agents.manage, act on every agent.
        assert_eq!(access_of(&Caller::owner(), false, &all, "b", None, u2), Access::Act);
        assert_eq!(access_of(&person("u3"), true, &all, "b", None, u2), Access::Act);
        // A person acts on the agents that act for them, their children included, and no other.
        assert_eq!(access_of(&person("u1"), false, &all, "a", None, u1), Access::Act);
        assert_eq!(access_of(&person("u1"), false, &all, "a1", None, u1), Access::Act);
        assert_eq!(access_of(&person("u1"), false, &all, "b", None, u2), Access::None);
        // A session kiln can't say whom it acts for: the owner and managers only.
        assert_eq!(access_of(&person("u1"), false, &all, "b", None, None), Access::None);
        assert_eq!(access_of(&Caller::owner(), false, &all, "b", None, None), Access::Act);
        assert_eq!(access_of(&person("u1"), true, &all, "b", None, None), Access::Act);
        // The owner's own tokens are the owner's, narrowed or not.
        let narrowed = Caller::with(Kind::Pat, "pat:t1", None, Some(Rights::default()));
        assert_eq!(access_of(&narrowed, false, &all, "a", None, Some("owner")), Access::Act);
        assert_eq!(access_of(&narrowed, false, &all, "b", None, u2), Access::None);
        // A job reaches no agent.
        let job = Caller::with(Kind::Job, "job:leg", Some(Rights::default()), None);
        assert_eq!(access_of(&job, false, &all, "a", None, Some("owner")), Access::None);
        // An agent acts on itself and its descendants, sees the other agents of its person, and
        // reaches no one else's, whatever its person may do.
        assert_eq!(access_of(&agent("a", "u1"), false, &all, "a", u1, u1), Access::Act);
        assert_eq!(access_of(&agent("a", "u1"), false, &all, "a1", u1, u1), Access::Act);
        assert_eq!(access_of(&agent("a1", "u1"), false, &all, "a", u1, u1), Access::See, "a child doesn't steer its parent");
        assert_eq!(access_of(&agent("a", "u1"), true, &all, "b", u1, u2), Access::None, "a manager's agent reaches only its own and its person's");
        assert_eq!(access_of(&agent("a", "u1"), false, &all, "b", u1, None), Access::None);
    }
}
