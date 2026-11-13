//! One agent session on a machine: the harness process, its adapter and its state. The process
//! lives as long as the session (not as long as any browser); when it exits, the session stays and
//! can be continued where it left off (the harness's own resume), automatically when a person
//! sends it a message.

use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use futures::{FutureExt as _, StreamExt as _};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::core::{Core, Update};
use super::kinds::{Kind, Protocol};
use super::model::{Body, Command, ExitInfo, Level, State, Status, Task, ToolKind};

fn is_zero_epoch(n: &u64) -> bool {
    *n == 0
}

/// An isolated session's boundary (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 17 and 32), as kiln's T4 isolation
/// named it: its kind, and the image it boots once one is chosen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Isolation {
    /// `microvm`: a microVM of its own.
    pub kind: String,
    /// The image it boots, once chosen (step 31's choice per harness and project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub image: Option<String>,
}

impl Isolation {
    /// The kind every isolated session has today.
    pub const MICROVM: &'static str = "microvm";

    /// A microVM of its own, its image not yet chosen.
    pub fn microvm() -> Self {
        Self { kind: Self::MICROVM.to_owned(), image: None }
    }
}

/// What to start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    /// Harness id ("claude", "codex", ...).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Continue this harness session (its own id) instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The first message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The terminal session (shell) this agent belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Environment captured from that shell (`kiln spawn`), applied over the machine's own, kiln's
    /// own variables (`KILN_*`) left out of both (`super::kinds`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Offer the harness's skip-all-permissions mode (it is never the default, only a choice).
    /// On unless a session says otherwise, so a person can always choose it.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub allow_bypass: bool,
    /// Run this executable instead of the harness's usual one (a custom install, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// The effort / reasoning level to start with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The speed to start at (`State::speed`: "" standard); None: the harness's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// The most subagents it runs at once (as `State::max_subagents`): Codex takes it as its own
    /// limit when it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subagents: Option<u32>,
    /// With `resume`: continue only up to this point of that session (a message or turn id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<String>,
    /// With `resume`: a new session that branches off it, leaving it as it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
    /// With `resume`: it takes over that session, found running as this process (which is being
    /// ended): the harness starts only once the process is gone, never beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_pid: Option<u32>,
    /// With `takeover_pid`: that session runs outside kiln, so kiln ends its process itself (asks
    /// it to quit, then kills it) rather than only waiting for it to end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub takeover_stop: bool,
    /// Reach (`crate::reach`): the agent's tools for running commands on other runners. None:
    /// it has them; `"off"`: it does not; anything else: where its commands go unless it says
    /// otherwise (a runner's name, or `linux/amd64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    /// The agent that started it (`harness::lineage`): kiln sets it from who asked, never from
    /// what was asked (`POST /api/harness/sessions` drops it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The plan run it does a step of (`plans::agents`): set by kiln, never from what was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_run: Option<String>,
    /// What it helps with when it's a card's helper (`harness::about`: `about`, answering what a
    /// person tapped in its parent's conversation): no row of Mission Control, its parent not told
    /// of it, ended once no card reads it. Set by kiln, never from what was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    /// It's its parent's subagent (`harness::lineage::start_subagent`): kiln started it for one of
    /// its parent's goals or plan steps, on its parent's machine with its parent's settings, and it
    /// shows in its parent's chat rather than as an agent of its own. Set by kiln, never from what
    /// was asked (`POST /api/harness/sessions` drops it). Its machine copies its parent's
    /// environment and program into it as it opens ([`super::host::Host::open`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent: bool,
    /// Where it works when not in its folder itself: a Slates volume of its own laid over the
    /// folder (`crate::workspace`), made on its machine before it starts and kept with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::Workspace>,
    /// How it's held when not as a process on its machine: in a microVM of its own (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d
    /// steps 17 and 32). Set from what was asked, or from its parent's: an isolated agent's agents
    /// are isolated too. A machine that can't hold it so refuses it, never running it as a process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    /// What a person branched it from (`Command::Branch`): a session's id, or `<session>/<task>`
    /// for one of that session's harness's own subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_of: Option<String>,
    /// Its claim on this machine, when a runner's owner placed it here (AUDIT M4,
    /// `crate::reach::place::Claim`): this runner holds its room from its start to its end, in
    /// the same ledger as CI legs and commands. Set by kiln, never from what was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<crate::reach::place::Claim>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            claim: None,
            kind: String::new(),
            cwd: None,
            model: None,
            mode: None,
            resume: None,
            prompt: None,
            name: None,
            shell: None,
            env: Vec::new(),
            allow_bypass: true,
            program: None,
            effort: None,
            speed: None,
            max_subagents: None,
            resume_at: None,
            fork: false,
            takeover_pid: None,
            takeover_stop: false,
            reach: None,
            parent: None,
            plan_run: None,
            helper: None,
            subagent: false,
            workspace: None,
            isolation: None,
            variant_of: None,
        }
    }
}

/// A pending approval, as lists show it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApprovalBrief {
    pub id: String,
    pub tool: ToolKind,
    pub tool_name: String,
    pub title: String,
    pub at: i64,
    /// The subagent that asks, as its request names it (`Approval::agent`): a list says whose it
    /// is without the agent's whole state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub agent: Option<String>,
}

/// A native subagent as lists show it. Its work still belongs to its harness's task, not a
/// separate kiln session, so this is tracking only and carries no execution settings or output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SubagentBrief {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub tool_id: Option<String>,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub parent_task: Option<String>,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub ended: Option<i64>,
    /// What it's doing now, in a line (`Task::last_tool`): where it is, not what it made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub last_tool: Option<String>,
    /// A person can pause it (as `Task::pausable`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pausable: bool,
    /// A person paused it (as `Task::paused_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub paused_at: Option<i64>,
    /// What a person lets it do (as `Task::perms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub perms: Option<super::model::Perms>,
}

impl SubagentBrief {
    pub(super) fn of(tasks: &[Task]) -> Vec<Self> {
        tasks
            .iter()
            .filter(|t| t.subagent())
            .map(|t| Self {
                id: t.id.clone(),
                tool_id: t.tool_id.clone(),
                description: super::model::clip_middle(&t.description, 200).0,
                status: t.status.clone(),
                parent_task: t.parent_task.clone(),
                started: t.started,
                ended: t.ended,
                last_tool: t.last_tool.as_deref().map(|s| super::model::clip_middle(s, 200).0),
                pausable: t.pausable,
                paused_at: t.paused_at,
                perms: t.perms.clone(),
            })
            .collect()
    }
}

/// How a session stands for its machine's agent limit (`super::limit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// At work: on a turn (waiting on a person in the middle of one too), or between turns with
    /// subagents it sent off still at work.
    Yes,
    /// Its harness starting: at work if it was let go to work (it's new, it was sent something, a
    /// person started it again), as its machine keeps it until it's on a turn or isn't.
    Starting,
    /// Finished, ready, paused, ended, or its machine away.
    No,
}

impl Work {
    /// How a session with `status` stands: `paused` at a usage limit, `subagents` any it sent off
    /// still at work.
    pub fn of(status: Status, paused: bool, subagents: bool) -> Self {
        match status {
            Status::Running | Status::Waiting => Self::Yes,
            // Paused at a usage limit: nothing goes to its harness until it goes on.
            Status::Starting if !paused => Self::Starting,
            Status::Idle if !paused && subagents => Self::Yes,
            _ => Self::No,
        }
    }

    /// As its state says it, on its machine.
    pub fn in_state(s: &State) -> Self {
        Self::of(s.status, s.pause.is_some(), s.tasks.iter().any(|t| t.subagent() && t.live()))
    }
}

/// Goals an agent paused for a person, until someone resumes them (`Info::held`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "HeldGoals"))]
pub struct Held {
    /// Why, as the stack says it ("Paused by the agent: …").
    pub why: String,
    /// The plan it waits for you to look at first, when that's why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan: Option<String>,
    /// Unix ms it paused.
    pub since: i64,
}

/// A session as lists show it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentInfo"))]
pub struct Info {
    /// Omitted detail fields in the owner's bounded control projection.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[cfg_attr(test, ts(as = "Option<std::collections::BTreeMap<String, crate::harness::state_view::Descriptor>>", optional))]
    pub deferred: std::collections::BTreeMap<String, super::state_view::Descriptor>,
    pub id: String,
    pub kind: String,
    pub kind_name: String,
    /// "local" or "agent:<machine>" (set by the server).
    #[serde(default)]
    pub target: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub shell: Option<String>,
    pub cwd: String,
    /// The working tree its work is in (`State::tree`): what's asked of git about its work is
    /// asked there ([`Info::work_dir`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub tree: Option<String>,
    pub created_at: i64,
    pub status: Status,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub harness_session: Option<String>,
    #[serde(default)]
    pub approvals: Vec<ApprovalBrief>,
    #[serde(default)]
    pub queued: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub admission: Option<super::admission::Capacity>,
    /// What waits in its queue, each message by its id and a digest of its words (as
    /// `State::queue`): the server's, for the messages kiln keeps there itself
    /// (`super::queued`). Lists don't show it.
    #[serde(skip)]
    pub queue: Vec<super::queued::Queued>,
    /// The bounded index does not cover the whole queue: absence is not proof of removal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queue_partial: bool,
    /// The owner's queue revision, for targeted reads when the bounded index is incomplete.
    #[serde(skip)]
    pub queue_revision: Option<String>,
    #[serde(skip)]
    pub queue_next: Option<usize>,
    /// kiln's own messages its harness was handed lately (a goal stack's words, a parent's news),
    /// each by its id and a digest of the words it had then, as its timeline shows them: the
    /// server's (`super::queued::Kept::look`). Lists don't show it.
    #[serde(skip)]
    pub said: Vec<super::model::Given>,
    #[serde(default)]
    pub tasks_running: usize,
    /// Native harness subagents, including finished work, for the list's child disclosure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentBrief>,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub activity: Option<String>,
    pub last_activity: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub exit: Option<ExitInfo>,
    /// What the person first asked for.
    #[serde(default)]
    pub goal: String,
    /// The goal the harness itself pursues, and how it stands (see `State::objective`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub objective: Option<super::model::Objective>,
    /// The agent's latest words, shortened.
    #[serde(default)]
    pub summary: String,
    /// What it is doing right now, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub doing: Option<String>,
    /// Its writer's epoch ([`State::epoch`]): a machine that announces a copy from before the
    /// session went on elsewhere lets go of it.
    #[serde(default, skip_serializing_if = "is_zero_epoch")]
    pub epoch: u64,
    #[serde(default)]
    pub todos_done: u32,
    #[serde(default)]
    pub todos_total: u32,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    /// How full the context window is, 0–100, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub context_pct: Option<u8>,
    /// Unix ms the current turn started (None when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub turn_started: Option<i64>,
    /// The plan limits of the account it runs on (as `State::limits`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<super::model::Limit>,
    /// That account, as `State::account` (never an address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan: Option<String>,
    /// Tasks it left running with no turn under way (as `State::left_behind`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub left_behind: u32,
    /// Where its commands may run besides its machine (as `State::reach`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub reach: Option<String>,
    /// It waits for a usage limit to reset (as `State::pause`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pause: Option<super::model::Pause>,
    /// A person paused it: its processes stopped since (as `State::frozen`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub frozen: Option<i64>,
    /// What a person lets it do (as `State::perms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub perms: Option<super::model::Perms>,
    /// Since when it waits for room on its machine to go to work (as `State::room_wait`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_wait: Option<i64>,
    /// How its last turn ended (as `State::last_turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub last_turn: Option<super::model::TurnEnd>,
    /// Kiln doesn't keep it going when it stops part-way (`goals::keep`; set on the server).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_off: bool,
    /// Its goals wait on a person: it paused them for you, until you resume them (`goals::held_stacks`;
    /// set on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub held: Option<Held>,
    /// What of its messages couldn't be read (`crate::wire::unread`; set on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub refused: Option<crate::wire::unread::Kept>,
    /// At a usage limit it goes on on another machine with room (as `State::move_on_limit`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub move_on_limit: bool,
    /// Where it goes on then (as `State::move_plan`).
    #[serde(default, skip_serializing_if = "super::model::MovePlan::is_empty")]
    pub move_plan: super::model::MovePlan,
    /// Where it pauses as its limits fill (as `State::pause_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pause_at: Option<u8>,
    /// The most subagents it runs at once (as `State::max_subagents`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub max_subagents: Option<u32>,
    /// The agent that started it (as `Spec::parent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub parent: Option<String>,
    /// The plan run it does a step of (as `Spec::plan_run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan_run: Option<String>,
    /// What it helps with when it's a card's helper (as `Spec::helper`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub helper: Option<String>,
    /// Its parent's subagent (as `Spec::subagent`): lists show it in its parent's chat and on its
    /// parent's row, not as an agent of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent: bool,
    /// What a person branched it from (as `Spec::variant_of`): once it's done, Keep hands its result
    /// to its parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub variant_of: Option<String>,
    /// It was started without its harness's skip-every-check mode (`Spec::allow_bypass` off), so
    /// an agent it starts can't have it either.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_bypass: bool,
    /// Its workspace of its own, when it has one (as `Spec::workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub workspace: Option<crate::workspace::Workspace>,
    /// Its isolation, when it's held in a microVM of its own (as `Spec::isolation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub isolation: Option<Isolation>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Info {
    /// Where git is asked about its work: the working tree its latest change is in, else its
    /// folder (an agent in a worktree commits and pushes from there, not from the checkout it
    /// started in).
    pub fn work_dir(&self) -> &str {
        self.tree.as_deref().filter(|t| !t.is_empty()).unwrap_or(&self.cwd)
    }

    /// At work now, as Mission control shows it working, or needing you in the middle of a turn:
    /// starting, on a turn (waiting on a person for an approval or an answer included), or between
    /// turns with subagents it sent off still at work. What its machine's agent limit counts
    /// (`super::limit`): a finished, ready, paused or ended session doesn't, nor does one whose
    /// goals wait for a person.
    pub fn at_work(&self) -> bool {
        Work::of(self.status, self.pause.is_some(), self.subagents.iter().any(|t| super::model::live_task(&t.status))) != Work::No
    }

    /// Refresh the live fields from the session's state.
    pub fn apply(&mut self, s: &State) {
        self.admission.clone_from(&s.admission);
        self.deferred = s.view.as_ref().map(|v| v.deferred.clone()).unwrap_or_default();
        if !s.name.is_empty() {
            self.name.clone_from(&s.name);
        }
        self.status = s.status;
        self.title.clone_from(&s.title);
        self.model.clone_from(&s.model);
        self.mode.clone_from(&s.mode);
        if !s.cwd.is_empty() {
            self.cwd.clone_from(&s.cwd);
        }
        self.tree.clone_from(&s.tree);
        self.harness_session.clone_from(&s.harness_session);
        self.epoch = s.epoch;
        self.approvals = s
            .approvals
            .iter()
            .map(|a| ApprovalBrief { id: a.id.clone(), tool: a.tool, tool_name: a.tool_name.clone(), title: a.title.clone(), at: a.at, agent: a.agent.clone() })
            .collect();
        self.queued = s.view.as_ref().map_or(s.queue.len(), |v| v.counts.queue);
        let deferred = s.view.as_ref().filter(|v| v.deferred.contains_key("queue"));
        let made = if deferred.is_none() { super::state_view::queue_index(&s.queue, 0, super::state_view::FRAME).ok() } else { None };
        let index = match deferred {
            Some(view) => view.queue_index.as_ref(),
            None => made.as_ref(),
        };
        self.queue = Vec::new();
        self.queue_partial = true;
        self.queue_next = None;
        if let Some(index) = index {
            self.queue_partial = !(index.complete && index.entries.len() == self.queued && index.next.is_none());
            self.queue_next = index.next;
            if self.queue.try_reserve(index.entries.len()).is_err() {
                self.queue_partial = true;
                self.queue_next = Some(0);
            } else {
                for entry in &index.entries {
                    match u64::from_str_radix(&entry.digest, 16) {
                        Ok(digest) => {
                            let mut id = String::new();
                            if id.try_reserve(entry.id.len()).is_err() {
                                self.queue_partial = true;
                                self.queue_next = Some(0);
                                break;
                            }
                            id.push_str(&entry.id);
                            self.queue.push(super::queued::Queued { id, digest, len: entry.len });
                        }
                        Err(_) => self.queue_partial = true,
                    }
                }
            }
        }
        self.queue_revision = s.view.as_ref().and_then(|v| v.revisions.get("queue")).cloned();
        if self.queue_partial && self.queue_revision.is_none() && s.view.is_none() {
            self.queue_revision = super::state_view::field_revision(s, "queue").ok();
        }
        // Attention plan 01a199e1b177752e84dad5dc990e6a15: pending/paused tasks
        // still hold work; Finished must never hide them.
        self.tasks_running = s.tasks.iter().filter(|t| t.live()).count();
        self.subagents = SubagentBrief::of(&s.tasks);
        self.cost_usd = s.usage.cost_usd;
        self.activity.clone_from(&s.activity);
        self.exit.clone_from(&s.exit);
        self.goal = super::model::clip_middle(&s.goal, 200).0;
        self.objective.clone_from(&s.objective);
        self.summary = super::model::clip_middle(&s.summary, 300).0;
        self.doing.clone_from(&s.doing);
        let total = u32::try_from(s.todos.len()).unwrap_or(u32::MAX);
        self.todos_total = total;
        self.todos_done = u32::try_from(s.todos.iter().filter(|t| t.status == "completed").count()).unwrap_or(total);
        if let Some(view) = &s.view {
            self.queued = view.counts.queue;
            self.tasks_running = view.counts.tasks_running;
            self.todos_total = u32::try_from(view.counts.todos).unwrap_or(u32::MAX);
            self.todos_done = u32::try_from(view.counts.todos_done).unwrap_or(u32::MAX);
        }
        self.files_changed = s.files_changed;
        self.lines_added = s.lines_added;
        self.lines_removed = s.lines_removed;
        self.context_pct = match (s.usage.context_tokens, s.usage.context_window) {
            (Some(t), Some(w)) if w > 0 => u8::try_from((t.saturating_mul(100) / w).min(100)).ok(),
            _ => None,
        };
        self.turn_started = s.turn_started;
        // What its harness was handed, added to what was known (a runner started again knows less).
        for g in &s.given {
            super::queued::note_given(&mut self.said, &g.id, g.digest.clone());
        }
        self.limits.clone_from(&s.limits);
        self.account.clone_from(&s.account);
        self.plan.clone_from(&s.plan);
        self.left_behind = s.count_left_behind();
        self.reach.clone_from(&s.reach);
        self.pause.clone_from(&s.pause);
        self.frozen = s.frozen;
        self.perms.clone_from(&s.perms);
        self.room_wait = s.room_wait;
        self.last_turn.clone_from(&s.last_turn);
        self.move_on_limit = s.move_on_limit;
        self.move_plan.clone_from(&s.move_plan);
        self.pause_at = s.pause_at;
        self.max_subagents = s.max_subagents;
    }

    /// The parts lists care about changed (activity timestamps aside, the words of what's queued: a
    /// message edited while it waits changes nothing they show, and what kiln's own were handed). Field by
    /// field where each is, neither side copied: both were copied whole for each state a session's owner
    /// took (goal 4, plan 01a1a59c2a9c step 5: about 1,700 a second in the lab's many-sessions profile).
    /// Every field is named, so one added to `Info` is said here too, or this doesn't build.
    pub fn differs(&self, o: &Self) -> bool {
        let Self { admission, deferred, id, kind, kind_name, target, name, shell, cwd, tree, created_at, status, title, model, mode, harness_session, approvals, queued, queue: _, queue_partial, queue_revision, queue_next: _, said: _, tasks_running, subagents, cost_usd, activity, last_activity: _, exit, goal, objective, summary, doing, epoch, todos_done, todos_total, files_changed, lines_added, lines_removed, context_pct, turn_started, limits, account, plan, left_behind, reach, pause, frozen, perms, room_wait, last_turn, keep_off, held, refused, move_on_limit, move_plan, pause_at, max_subagents, parent, plan_run, helper, subagent, variant_of, no_bypass, workspace, isolation } = self;
        !(*deferred == o.deferred
            && *admission == o.admission
            && *queue_partial == o.queue_partial
            && (!*queue_partial || *queue_revision == o.queue_revision)
            && *id == o.id
            && *kind == o.kind
            && *kind_name == o.kind_name
            && *target == o.target
            && *name == o.name
            && *shell == o.shell
            && *cwd == o.cwd
            && *tree == o.tree
            && *created_at == o.created_at
            && *status == o.status
            && *title == o.title
            && *model == o.model
            && *mode == o.mode
            && *harness_session == o.harness_session
            && *approvals == o.approvals
            && *queued == o.queued
            && *tasks_running == o.tasks_running
            && *subagents == o.subagents
            && *cost_usd == o.cost_usd
            && *activity == o.activity
            && *exit == o.exit
            && *goal == o.goal
            && *objective == o.objective
            && *summary == o.summary
            && *doing == o.doing
            && *epoch == o.epoch
            && *todos_done == o.todos_done
            && *todos_total == o.todos_total
            && *files_changed == o.files_changed
            && *lines_added == o.lines_added
            && *lines_removed == o.lines_removed
            && *context_pct == o.context_pct
            && *turn_started == o.turn_started
            && *limits == o.limits
            && *account == o.account
            && *plan == o.plan
            && *left_behind == o.left_behind
            && *reach == o.reach
            && *pause == o.pause
            && *frozen == o.frozen
            && *perms == o.perms
            && *room_wait == o.room_wait
            && *last_turn == o.last_turn
            && *keep_off == o.keep_off
            && *held == o.held
            && *refused == o.refused
            && *move_on_limit == o.move_on_limit
            && *move_plan == o.move_plan
            && *pause_at == o.pause_at
            && *max_subagents == o.max_subagents
            && *parent == o.parent
            && *plan_run == o.plan_run
            && *helper == o.helper
            && *subagent == o.subagent
            && *variant_of == o.variant_of
            && *no_bypass == o.no_bypass
            && *workspace == o.workspace
            && *isolation == o.isolation)
    }
}

/// Where a session's updates go (the server's registry, or the uplink to it): straight to where they go ([`To`]), or,
/// a runner's, kept in its conversation store on the way, in the session's own turn (`crate::convo::store::tee_held`).
/// Either way a sender waits for room: nothing is dropped.
#[derive(Clone, Debug)]
pub enum Sink {
    To(To),
    Kept(Arc<crate::convo::store::Tee>),
}

/// Where a session's updates go past any store: a queue (`crate::memory::queue`) bounded in bytes (the server's own
/// sessions', goal 11, plan 01a1a87b2e56 step 3) or in places (a runner's, on their way to its loop, whose uplink bounds
/// them in bytes; goal 5, plan 01a1a59c2aae), or a channel's sending end (tests').
#[derive(Clone, Debug)]
pub enum To {
    Bounded(crate::memory::queue::Sender<Local>),
    Channel(async_channel::Sender<(String, Update)>),
    /// A runner's: each update with when it was handed on ([`Taken`]; goal 10, plan 01a1a87b2dae step 5).
    Stamped(crate::memory::queue::Sender<Taken>),
}

/// An update of session `session` on a runner's way to the server, with when its session handed it on: when
/// the runner took it in, which its frame says how long before it was made (`crate::wire::stamp`; goal 10, plan
/// 01a1a87b2dae step 5). What waits between (the store's room, the runner's loop) is the runner's.
#[derive(Debug)]
pub struct Taken {
    pub session: String,
    pub update: Update,
    pub at: std::time::Instant,
}

/// What a session of the server's own machine hands the server's registry ([`To::Bounded`]): its updates, then, the last
/// thing its task hands on, that the task is gone ([`Sink::gone`]).
#[derive(Debug)]
pub enum Local {
    Said(String, Update),
    Gone(String),
}

impl crate::memory::queue::Weigh for Local {
    fn weight(&self) -> usize {
        match self {
            Local::Said(session, update) => crate::memory::queue::Weigh::weight(session).saturating_add(crate::memory::queue::Weigh::weight(update)),
            Local::Gone(session) => crate::memory::queue::Weigh::weight(session),
        }
    }
}

/// Hands an update of a session of the server's own machine to its registry ([`Local::Said`]): what an error gives back is
/// what went (a task's end is said by [`Sink::gone`] alone).
async fn said(q: &crate::memory::queue::Sender<Local>, (session, update): (String, Update)) -> Result<(), async_channel::SendError<(String, Update)>> {
    q.send(Local::Said(session, update)).await.map_err(|e| {
        async_channel::SendError(match e.0 {
            Local::Said(session, update) => (session, update),
            Local::Gone(session) => (session, Update::Closed),
        })
    })
}

impl Sink {
    /// Hands an update on, waiting while there's no room for it: an error, with it, once no one takes
    /// updates any more. A runner's sink notes when ([`To::Stamped`]), and keeps it in its store first ([`Sink::Kept`]).
    pub async fn send(&self, update: (String, Update)) -> Result<(), async_channel::SendError<(String, Update)>> {
        let (to, t) = match self {
            Sink::To(To::Bounded(q)) => return said(q, update).await,
            Sink::To(To::Channel(c)) => return c.send(update).await,
            Sink::To(to) => (to, Taken { session: update.0, update: update.1, at: std::time::Instant::now() }),
            Sink::Kept(tee) => {
                let t = Taken { session: update.0, update: update.1, at: std::time::Instant::now() };
                tee.keep(&t).await;
                (tee.next(), t)
            }
        };
        to.send_taken(t).await.map_err(|e| async_channel::SendError((e.0.session, e.0.update)))
    }

    /// Session `session`'s task is gone, whatever ended it: the last thing it hands on, after all it said. The server's own
    /// registry lets go then of what it held for the session until nothing it said could still be on its way (a close's hold:
    /// `super::copies::Shard::close`, HARNESS-4); a runner's sink and a test's take nothing for it.
    pub async fn gone(&self, session: &str) {
        if let Sink::To(To::Bounded(q)) = self {
            let _ = q.send(Local::Gone(session.to_owned())).await;
        }
    }
}

impl To {
    /// Hands on an update taken in before: when it was taken in goes with it where this keeps that ([`To::Stamped`]).
    async fn send_taken(&self, t: Taken) -> Result<(), Box<async_channel::SendError<Taken>>> {
        let Taken { session, update, at } = t;
        let back = |(session, update): (String, Update)| Box::new(async_channel::SendError(Taken { session, update, at }));
        match self {
            To::Stamped(q) => q.send(Taken { session, update, at }).await.map_err(Box::new),
            To::Bounded(q) => said(q, (session, update)).await.map_err(|e| back(e.0)),
            To::Channel(c) => c.send((session, update)).await.map_err(|e| back(e.0)),
        }
    }
}

impl From<crate::memory::queue::Sender<Taken>> for To {
    fn from(q: crate::memory::queue::Sender<Taken>) -> Self {
        To::Stamped(q)
    }
}

impl From<crate::memory::queue::Sender<Local>> for To {
    fn from(q: crate::memory::queue::Sender<Local>) -> Self {
        To::Bounded(q)
    }
}

impl From<async_channel::Sender<(String, Update)>> for To {
    fn from(c: async_channel::Sender<(String, Update)>) -> Self {
        To::Channel(c)
    }
}

impl From<crate::memory::queue::Sender<Taken>> for Sink {
    fn from(q: crate::memory::queue::Sender<Taken>) -> Self {
        Sink::To(To::Stamped(q))
    }
}

impl From<crate::memory::queue::Sender<Local>> for Sink {
    fn from(q: crate::memory::queue::Sender<Local>) -> Self {
        Sink::To(To::Bounded(q))
    }
}

impl From<async_channel::Sender<(String, Update)>> for Sink {
    fn from(c: async_channel::Sender<(String, Update)>) -> Self {
        Sink::To(To::Channel(c))
    }
}

/// What a session needs from the machine that holds it: where it keeps the files attached to
/// its messages, how it collects ones that arrived separately (uploaded, or sent in pieces from
/// the server), and how it starts another session (a branch).
/// What gives the ids of the messages a session took, by its id, to go with it as it moves.
pub type CarriedIds = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<Vec<(String, String)>, String>> + Send + Sync>;

/// Fences a session's inbox as it moves to another machine (`true`), or lifts the fence (`false`:
/// its move failed, and it goes on here) ([`super::host::Host::moving_fence`]).
pub type FenceFor = Arc<dyn Fn(String, bool) -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// The machine's decision on a try at handing a session to its newer runner process (the try, the
/// session), or (`true`) made now, not taken over, if there's none yet
/// ([`super::host::Host::settle`]).
pub type SettleFor = Arc<dyn Fn(String, String, bool) -> futures::future::BoxFuture<'static, Result<Option<super::host::Decided>, String>> + Send + Sync>;

#[derive(Clone)]
pub struct Files {
    pub host: Option<std::sync::Weak<super::host::Host>>,
    pub dir: std::path::PathBuf,
    pub take: Arc<dyn Fn(&str) -> Option<std::path::PathBuf> + Send + Sync>,
    pub spawn: Arc<dyn Fn(Spec) + Send + Sync>,
    /// The session went on on another machine: this one lets go of it (it isn't closed).
    pub release: Arc<dyn Fn(&str) + Send + Sync>,
    /// The ids of the messages a session took that go with it when it moves ([`super::host::Host::carried`]).
    pub carried: CarriedIds,
    /// Its inbox fenced before its conversation goes to the machine it moves to, or lifted when
    /// the move fails (CP-T3.2).
    pub fence: FenceFor,
    /// How a try at handing it to this machine's newer runner process went, as the machine decided
    /// it (CP-T3.2).
    pub settle: SettleFor,
    /// Whether it may go to work now: room on its machine, nobody waiting longer; at work already,
    /// it may (`super::host::Host::room_for`). Asked with its core in hand: what it takes is never
    /// waited on there.
    pub room: Arc<dyn Fn() -> bool + Send + Sync>,
    /// How it stands for its machine's agent limit now, and whether anything waits to go to its
    /// harness (`super::host::Host::stands`).
    pub work: Arc<dyn Fn(Work, bool) + Send + Sync>,
}

/// What a session that moved here starts from (`super::moving`): its timeline and state as they
/// were, and for a harness that takes one, the history of its new thread.
pub struct Seed {
    pub events: Vec<super::model::Event>,
    pub state: State,
    pub items: Option<Vec<serde_json::Value>>,
    /// Where it came from: the runner and its folder there.
    pub from: String,
    pub from_cwd: String,
    /// Its turn was interrupted to move: it picks the turn up here.
    pub picks_up: bool,
    /// Said with the move (its folder isn't here, so it works in home).
    pub note: Option<String>,
    /// Not a move: the session is started again here after a restart, its timeline and state
    /// taken from the conversation store (`crate::convo`).
    pub restored: bool,
    /// Where its folder stood in the sync log on the machine it left: it goes on once the folder
    /// here has taken the log that far.
    pub synced: Option<crate::sync::runner::Mark>,
    /// Not from another machine: an older runner process of this one handed it over, taking its
    /// leave (`super::handing`). Same folder, same record: only the process holding it changed.
    pub local: bool,
}

/// A preparation failure never converts a communication into a text-only prompt.
#[derive(Debug, Clone, Copy)]
pub(super) enum PreparationCode { AttachmentUnavailable, WorkerUnavailable }

pub(super) struct PreparationFailure { pub message: Option<String>, pub code: PreparationCode }

fn attachment_digest(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use sha2::Digest as _;
    struct DigestWriter(sha2::Sha256);
    impl std::io::Write for DigestWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.update(bytes); Ok(bytes.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > super::host::ATTACHMENT_MAX { return Err(std::io::Error::other("attachment is not a bounded regular file")); }
    let mut digest = DigestWriter(sha2::Sha256::new());
    std::io::copy(&mut std::fs::File::open(path)?, &mut digest)?;
    Ok(digest.0.finalize().to_vec())
}

/// Completed source and destination files are immutable. Only a newly created destination is
/// removed on failure; an earlier completed materialization is verified and never overwritten.
fn immutable_attachment(path: &std::path::Path, source: Option<&std::path::Path>, bytes: &[u8]) -> std::io::Result<()> {
    use sha2::Digest as _;
    use std::io::Write as _;
    let expected = match source { Some(source) => attachment_digest(source)?, None => sha2::Sha256::digest(bytes).to_vec() };
    let opened = std::fs::OpenOptions::new().write(true).create_new(true).open(path);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if attachment_digest(path)? == expected { return Ok(()); }
            return Err(std::io::Error::other("completed attachment materialization differs"));
        }
        Err(error) => return Err(error),
    };
    let written = (|| {
        match source {
            Some(source) => { std::io::copy(&mut std::fs::File::open(source)?, &mut file)?; }
            None => file.write_all(bytes)?,
        }
        file.sync_all()?;
        if attachment_digest(path)? != expected { return Err(std::io::Error::other("attachment materialization changed its bytes")); }
        Ok(())
    })();
    if written.is_err() { drop(file); let _ = std::fs::remove_file(path); }
    written
}

/// All attachments or an explicit failure. Original upload ids/inline bytes remain in metadata
/// so a later replay verifies the same immutable input rather than trusting a supplied path.
fn store_attachments(files: &Files, msg: &str, list: Vec<super::model::Attachment>) -> std::io::Result<Vec<super::model::Attachment>> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    std::fs::create_dir_all(&files.dir)?;
    let mut out = Vec::with_capacity(list.len());
    for (i, mut a) in list.into_iter().enumerate() {
        let index = i.to_string();
        let tag = crate::util::derive_id(&["harness-materialized-v1", msg, &index, &a.name, a.file_id.as_deref().unwrap_or(&a.data_base64)]);
        let name: String = a.name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(80).collect();
        let name = if name.trim_matches('.').is_empty() { format!("file-{i}") } else { name };
        let path = files.dir.join(format!("{tag}-{name}"));
        let source = match a.file_id.as_deref() {
            Some(file) => Some((files.take)(file).ok_or_else(|| std::io::Error::other("completed attachment input is unavailable"))?),
            None => None,
        };
        let bytes = match &source {
            Some(_) => Vec::new(),
            None => {
                let bytes = b64.decode(a.data_base64.as_bytes()).map_err(|_| std::io::Error::other("attachment bytes are invalid"))?;
                if bytes.is_empty() || u64::try_from(bytes.len()).map_or(true, |length| length > super::host::ATTACHMENT_MAX) { return Err(std::io::Error::other("attachment bytes are absent or exceed their bound")); }
                bytes
            }
        };
        immutable_attachment(&path, source.as_deref(), &bytes)?;
        if a.is_image() && a.data_base64.is_empty() { a.data_base64 = b64.encode(std::fs::read(&path)?); }
        a.path = Some(path.to_string_lossy().into_owned());
        out.push(a);
    }
    Ok(out)
}

/// The longest name a session takes.
pub(crate) const MAX_NAME: usize = 80;

/// Names a session (the "rename" control: the console, `/rename`, a broadcast): in kiln's lists,
/// and kept across restarts in its spec. Returns the control to pass on when the harness names
/// its conversations too (Claude Code, Codex). An empty name gives it back its harness's name.
fn name_session(s: &Session, value: Option<&serde_json::Value>) -> Option<Command> {
    let name: String = value.and_then(serde_json::Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
    let kind_name = s.info.lock().kind_name.clone();
    s.spec.lock().name = (!name.is_empty()).then(|| name.clone());
    let mut core = s.core.lock();
    core.state.name = if name.is_empty() { kind_name } else { name.clone() };
    core.touch_state();
    let harness_names = core.meta.controls.iter().any(|c| c.id == "rename");
    (harness_names && !name.is_empty()).then(|| Command::Control { id: "rename".into(), value: Some(serde_json::Value::String(name)) })
}

/// Where its commands may run besides its machine (the "reach" control, as `Spec::reach`): ""
/// anywhere it needs, "off" only its own machine, else a runner or platform it prefers. Kept in
/// its spec (its harness takes it when it next starts) and shown in its state. Whether it changed.
fn set_reach(s: &Session, value: Option<&serde_json::Value>) -> bool {
    let v = value.and_then(serde_json::Value::as_str).map(str::trim).unwrap_or("");
    let reach = (!v.is_empty()).then(|| v.chars().take(MAX_NAME).collect::<String>());
    {
        let mut spec = s.spec.lock();
        if spec.reach == reach {
            return false;
        }
        spec.reach.clone_from(&reach);
    }
    let mut core = s.core.lock();
    core.state.reach = reach;
    core.touch_state();
    true
}

/// The "reach" setting in words, for the timeline.
fn reach_words(reach: Option<&str>) -> String {
    match reach {
        None => "Its commands can run on any runner it needs".to_owned(),
        Some("off") => "Its commands run only on its own machine".to_owned(),
        Some(r) => format!("Its commands can run on other runners, {r} first"),
    }
}

/// A command as the adapter should see it (attachments stored first; another agent's or a
/// harness's message steering, [`super::queued::steers`]).
pub(super) async fn prepare(files: &Files, c: Command) -> Result<Command, PreparationFailure> {
    let c = match c {
        Command::Send { id, text, mode: super::model::SendMode::Queue, attachments, overrides, reply_to, task } if super::queued::steers(&id) => {
            Command::Send { id, text, mode: super::model::SendMode::Steer, attachments, overrides, reply_to, task }
        }
        c => c,
    };
    match c {
        Command::Send { id, text, mode, attachments, overrides, reply_to, task } if !attachments.is_empty() => {
            let files = files.clone();
            let msg = id.clone();
            let stored = crate::rt::blocking(move || store_attachments(&files, &msg, attachments)).await
                .map_err(|_| PreparationFailure { message: Some(id.clone()), code: PreparationCode::WorkerUnavailable })?
                .map_err(|_| PreparationFailure { message: Some(id.clone()), code: PreparationCode::AttachmentUnavailable })?;
            Ok(Command::Send { id, text, mode, attachments: stored, overrides, reply_to, task })
        }
        other => Ok(other),
    }
}

async fn preparation_failed(s: &Session, failure: PreparationFailure) {
    let input = s.core.lock().input.clone();
    if let Some(input) = input && let Some(message) = &failure.message {
        let epoch = input.controller.lock().lease(message).map(|lease| lease.epoch);
        if let Some(epoch) = epoch {
            // Still replayable; no provider-flight intent was persisted and no partial
            // prompt was emitted. Connection/owner recovery may retry the original.
            let _ = input.inbox.stage_pending(&s.id, message, epoch, super::admission::Stage::Carried).await;
            let mut controller = input.controller.lock();
            let _ = controller.stage(message, super::admission::Stage::Carried);
            let _ = controller.unavailable();
            let token = controller.lease(message).and_then(|lease| lease.token);
            let capacity = controller.capacity();
            drop(controller);
            let mut core = s.core.lock();
            core.state.admission = Some(capacity.clone());
            if let Some(token) = token { core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Waiting { token, epoch, cause: super::admission::WaitCause::SourceUnavailable, capacity }) }); }
        }
    }
    let mut core = s.core.lock();
    core.upsert("attachment-preparation-unavailable", None, super::model::Body::Notice { level: Level::Error, text: format!("Communication retained: attachment preparation is unavailable ({:?}). No partial prompt was sent.", failure.code) });
    core.touch_state();
    drop(core);
    s.input_changed();
}

enum Ctl {
    Cmd(Box<Command>),
    /// Owned Reach state changed; publish through this driver, never a stale parallel snapshot.
    OwnedChanged,
    /// A persisted legacy promotion changed; its body work stays on this owner.
    Promotion,
    /// Automatic archive inspects after previously accepted commands reach this actor.
    ArchiveQuiet(futures::channel::oneshot::Sender<bool>),
    Close,
    /// This image of the process ends next (an in-place upgrade): the harness process, still
    /// running, goes to the next one.
    HandOver(futures::channel::oneshot::Sender<Result<Option<Box<Handed>>, String>>),
    /// It went on on another machine: it stops here, and isn't closed.
    LetGo,
    /// There may be room on its machine for it to go to work: what waited for that goes, if it
    /// may now (`super::limit`).
    Room,
    /// Its hand-over to this process is recorded (docs/ci.md W27-R08: a blue/green upgrade's): it
    /// goes on at this epoch, its state saying so.
    Epoch(u64),
    /// Its harness's hook asks before a tool call (Codex's, `codex::hook_args`): the answer goes to
    /// `reply`, at once or once what holds the call lets it go (`hold`).
    Hook(serde_json::Value, futures::channel::oneshot::Sender<serde_json::Value>),
}

/// A session handed from this image of the process to the next (an in-place upgrade) with its
/// harness process still running: the next image goes on with that very process, where this one
/// stopped, so nothing the agent was doing is cut short.
#[derive(Serialize, Deserialize)]
pub struct Handed {
    /// The harness it runs in now (a switch may have changed it from its spec's).
    pub kind: String,
    pub process: crate::proc::Detached,
    pub core: super::core::Frozen,
    pub adapter: super::kinds::Frozen,
    /// What the harness wrote after its last whole line (base64): the rest of that line follows.
    pub partial: String,
}

/// A harness process taken over from this process's previous image, still running.
struct Adopt {
    process: crate::proc::Detached,
    adapter: super::kinds::Adapter,
    partial: Vec<u8>,
}

/// A running session, as its machine holds it.
pub struct Session {
    pub id: String,
    pub spec: Mutex<Spec>,
    pub created_at: i64,
    pub core: Mutex<Core>,
    info: Mutex<Info>,
    /// What it's told (commands, a hook's word, a hand-over's ask), in order: a lock-free queue (goal 5,
    /// plan 01a1a59c2aae, its top goal), unbounded as the channel it was. An async-channel's wait took
    /// event-listener's `Mutex`, on each sender's shard (its lane's, a hook's) and on the session's own as
    /// it waited.
    ctl: crate::memory::queue::Sender<Ctl>,
    /// The process's pid while it runs.
    pub pid: Mutex<Option<u32>>,
    /// A turn of its is under way: set as one starts, cleared only as one ends (its harness ending
    /// leaves it set), so the record a restart reads says which turns it cut short.
    pub turn_open: std::sync::atomic::AtomicBool,
    /// What its harness runs, as last seen (`survivors`): the harness process, and what's under it
    /// with what it left running.
    pub ran: Mutex<(Option<super::survivors::Ran>, Vec<super::survivors::Ran>)>,
    /// What it started that still runs once what started it is gone, each shown as a task
    /// (`left-<pid>`) and stoppable from kiln.
    pub left: Mutex<Vec<super::survivors::Ran>>,
    /// It's on its way to another machine: messages sent to it from now on go where it goes, so
    /// it takes none here (CP-T3.2). A message is checked against it and handed over under this
    /// lock, and a move sets it and gathers what it was sent under it too: none slips in between.
    moving: Mutex<bool>,
    prompted: std::sync::atomic::AtomicBool,
    /// Retained context awaiting provider consumption, persisted by the host across restarts.
    pub(super) carried: Mutex<Option<Vec<serde_json::Value>>>,
    /// Its loop has ended (`close_wait`'s wait: an archive isn't complete while it can still run).
    ended: std::sync::atomic::AtomicBool,
    ended_changed: crate::rt::Changed,
    pub(crate) owned_reach: Mutex<std::collections::HashMap<String, std::sync::Arc<crate::reach::owned::Run>>>,
}

impl Session {
    pub(crate) async fn promotion_changed(&self) -> Result<(), String> { self.ctl.send(Ctl::Promotion).await.map_err(|_| "the legacy session owner stopped before its persisted transition could progress".to_owned()) }
    pub(super) fn input_changed(&self) { let _ = self.ctl.try_send(Ctl::OwnedChanged); }
    /// Host receipts and ordinary State frames share the session owner's ordered output.
    pub(super) fn mail_flow(&self, event: super::admission::FlowEvent) {
        {
            let mut core = self.core.lock();
            let capacity = core.input.as_ref().map(|input| input.controller.lock().capacity()).unwrap_or_else(|| event.capacity().clone());
            core.state.admission = Some(capacity);
            core.out.push(Update::MailFlow { event: Box::new(event) });
            core.touch_state();
        }
        self.input_changed();
    }
    pub fn info(&self) -> Info {
        let mut i = self.info.lock().clone();
        let mut core = self.core.lock();
        core.sync_owned_reach();
        i.apply(&core.state);
        i
    }

    /// Its epoch ([`State::epoch`]): how many times it went on on another machine.
    pub fn epoch(&self) -> u64 {
        self.core.lock().state.epoch
    }

    /// It's on its way to another machine: it takes no messages here.
    pub fn is_moving(&self) -> bool {
        *self.moving.lock()
    }

    /// It's being handed to another runner process of this machine (blue/green): it takes no
    /// messages from now on (each refused `Moved`; sent again, it goes where the session went).
    pub fn stop_taking(&self) {
        *self.moving.lock() = true;
    }

    /// The transfer owner calls this only before an offer or after authoritative abort. An
    /// uncertain/committed transfer must retain its existing acceptance fence.
    pub(super) fn resume_taking(&self) { *self.moving.lock() = false; }

    /// Hands `c` to the session. False when it can't take it: it has ended, or `c` is a message
    /// and the session is on its way to another machine.
    pub fn command(&self, c: Command) -> bool {
        let moving = self.moving.lock();
        if *moving && is_message(&c) {
            return false;
        }
        self.ctl.try_send(Ctl::Cmd(Box::new(c))).is_ok()
    }

    pub fn close(&self) {
        self.stop_owned_reach(None);
        let _ = self.ctl.try_send(Ctl::Close);
    }

    #[cfg(any(unix, test))]
    pub(crate) async fn publish_owned_change(&self)->Result<(),String>{
        self.ctl.send(Ctl::OwnedChanged).await.map_err(|_|"the owning session stopped publishing state".to_owned())
    }

    /// Whether it's quiet for an automatic archive, said now (attention plan 01a199e1b177752e84dad5dc990e6a15): its state
    /// has nothing under way or waiting, nothing waits for its actor, and its actor, the only one that knows what else
    /// it holds (input carried for its next start, a skill run waiting for room, a move, a switch, a reload), holds none
    /// of it. Asked only with nothing ahead of the question, so the answer is its actor's next step, never behind a
    /// backlog; anything else is "not now", and the next maintenance pass asks again. The archive's fence keeps new
    /// input out meanwhile (`Host::archive_if_quiet`).
    pub(crate) async fn archive_quiet(&self)->Result<bool,String>{
        if !self.archive_quiet_now(false) || !self.ctl.is_empty(){return Ok(false);}
        let(reply,answer)=futures::channel::oneshot::channel();
        if self.ctl.try_send(Ctl::ArchiveQuiet(reply)).is_err(){return Ok(false);}
        answer.await.map_err(|_|"the session ended before it said whether it's quiet".to_owned())
    }

    fn archive_quiet_now(&self,pending_local:bool)->bool{
        let mut core=self.core.lock();core.sync_owned_reach();let state=&core.state;
        !pending_local && !self.turn_open.load(Ordering::Acquire)
            && state.status==Status::Idle && state.turn_started.is_none() && state.queue.is_empty()
            && state.approvals.is_empty() && !state.tasks.iter().any(|task|task.live())
            && state.pause.is_none() && state.room_wait.is_none() && state.frozen.is_none()
            && state.switching.is_none() && !state.manual_resume && !core.hold
    }

    fn stop_owned_reach(&self, task:Option<&str>){
        for run in self.owned_reach.lock().values().filter(|run|task.is_none_or(|id|id==run.id)){run.stop();}
    }

    /// Attention plan 01a199e1b177752e84dad5dc990e6a15: don't report an archive
    /// complete while the harness or its commands can still be running.
    pub async fn close_wait(&self) {
        self.stop_owned_reach(None);
        let _ = self.ctl.send(Ctl::Close).await;
        loop {
            let ended = self.ended_changed.listen();
            if self.ended.load(Ordering::Acquire) {
                return;
            }
            ended.await;
        }
    }

    /// Its harness's hook asks about a tool call (`input`, as the harness gives it): the hook's
    /// answer, once the session gives it (a call a person holds waits for them). Nothing (`{}`) lets
    /// the call go: the session ended, or nothing holds it.
    pub async fn hook(&self, input: serde_json::Value) -> serde_json::Value {
        let (tx, rx) = futures::channel::oneshot::channel();
        if self.ctl.send(Ctl::Hook(input, tx)).await.is_err() {
            return serde_json::json!({});
        }
        rx.await.unwrap_or_else(|_| serde_json::json!({}))
    }

    /// There may be room on its machine now for it to go to work ([`Ctl::Room`]).
    pub fn room(&self) {
        let _ = self.ctl.try_send(Ctl::Room);
    }

    /// It goes on at `epoch` ([`Ctl::Epoch`]): its hand-over to this process is recorded.
    pub fn go_on_at(&self, epoch: u64) {
        let _ = self.ctl.try_send(Ctl::Epoch(epoch));
    }

    /// Hand its harness process to this process's next image (an in-place upgrade, next):
    /// `None` when none runs, or it couldn't be handed over (it was stopped then, and starts again
    /// in the next image, continuing its conversation).
    pub async fn hand_over(&self) -> Result<Option<Box<Handed>>, String> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ctl.send(Ctl::HandOver(tx)).await.map_err(|_| "the source owner cannot prepare its live provider handover".to_owned())?;
        rx.await.map_err(|_| "the source provider handover did not complete".to_owned())?
    }

    /// It went on on another machine since: its harness here stops and this machine lets go of
    /// it, without closing it (its viewers follow it there).
    pub fn let_go(&self) {
        self.stop_owned_reach(None);
        let _ = self.ctl.try_send(Ctl::LetGo);
    }

    /// Everything a new viewer (or a restarted server) needs, starting with the announcement.
    pub fn snapshot(&self) -> Vec<Update> {
        let mut out = vec![Update::Open { info: Box::new(self.info()), since: None }];
        out.extend(self.core.lock().snapshot(256 * 1024));
        out
    }

    /// A task's output after byte `since` (its last `TASK_TAIL` bytes without one), read where the
    /// harness keeps it: `{data, offset, live, truncated}` (ask from `offset` next), or `{error}`.
    /// Native Codex children return a replaceable transcript snapshot (`reset: true`).
    /// Blocking (reads a file).
    pub fn task_output(&self, task: &str, since: Option<u64>) -> serde_json::Value {
        use serde_json::json;
        let (t, kept, session, cwd, env, codex) = {
            let core = self.core.lock();
            let spec = self.spec.lock();
            let codex = core.harness.as_deref().map_or(spec.kind == "codex", |h| h == "Codex");
            let t = core.state.tasks.iter().find(|x| x.id == task).cloned();
            if codex && let Some(t) = t.as_ref().filter(|t| t.class.as_deref() == Some("agent")) {
                return codex_task_output(&core, t);
            }
            (t, core.task_output.get(task).cloned(), core.state.harness_session.clone(), core.state.cwd.clone(), spec.env.clone(), codex)
        };
        let Some(t) = t else { return json!({"error": "no such task in this session"}) };
        // Kept by kiln (a harness that streams it).
        if let Some(k) = kept {
            let (bytes, offset, truncated) = k.since(since.unwrap_or_else(|| k.total.saturating_sub(TASK_TAIL)));
            return json!({"data": String::from_utf8_lossy(&bytes), "offset": offset, "live": t.live(), "truncated": truncated});
        }
        // A file the harness writes (Claude Code: while the task lives).
        let Some(path) = t.output.clone().or_else(|| (!codex).then(|| claude_task_file(&env, &cwd, session.as_deref(), task)).flatten()) else {
            return json!({"error": "this task's output is not kept anywhere kiln can read", "live": t.live()});
        };
        let p = std::path::Path::new(&path);
        let named = p.extension().and_then(|e| e.to_str()) == Some("output") && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("tasks");
        if !named || !p.is_absolute() {
            return json!({"error": "not a task output file", "live": t.live()});
        }
        read_tail(p, since).map_or_else(
            || json!({"data": "", "offset": since.unwrap_or(0), "live": t.live(), "gone": true}),
            |(data, offset, truncated)| json!({"data": data, "offset": offset, "live": t.live(), "truncated": truncated, "path": path}),
        )
    }

    /// What resuming this session takes: its spec with the harness's session id to continue.
    /// What its harness keeps as its own record of the conversation, for the conversation store
    /// to follow (`crate::convo::record`): none until the harness has a session of its own. Its
    /// locks are taken one at a time.
    pub fn record_source(&self) -> Option<crate::convo::record::Source> {
        let (env, first) = {
            let spec = self.spec.lock();
            (spec.env.clone(), spec.kind.clone())
        };
        let (harness, own) = {
            let core = self.core.lock();
            (core.harness.clone(), core.state.harness_session.clone())
        };
        // The harness it runs in now (a switch changes it), else the one it started in.
        let kind = harness.as_deref().and_then(|name| Kind::all().iter().find(|k| k.name == name)).or_else(|| Kind::get(&first))?;
        let harness_session = own.filter(|h| !h.is_empty())?;
        let active_ms = self.info.lock().last_activity;
        Some(crate::convo::record::Source { session: self.id.clone(), kind, env, harness_session, active_ms })
    }

    pub fn resume_spec(&self) -> Spec {
        let mut s = self.spec.lock().clone();
        let core = self.core.lock();
        if let Some(h) = core.state.harness_session.clone() {
            s.resume = Some(h);
        }
        // Where it is now, not where it started: its own session, settings as last chosen.
        if self.prompted.load(std::sync::atomic::Ordering::Relaxed) {
            s.prompt = None;
        }
        s.resume_at = None;
        s.fork = false;
        if !core.state.effort.is_empty() {
            s.effort = Some(core.state.effort.clone());
        }
        // Standard too, once it has a faster speed to choose: its harness's own settings may name one.
        if !core.state.speed.is_empty() || !core.meta.speeds.is_empty() {
            s.speed = Some(core.state.speed.clone());
        }
        if !core.state.mode.is_empty() {
            s.mode = Some(core.state.mode.clone());
        }
        if !core.state.model.is_empty() {
            s.model = Some(core.state.model.clone());
        }
        s.max_subagents = core.state.max_subagents;
        s
    }
}

/// Stop what it left running that kiln took back over: one (its task id), or all of them.
async fn stop_left(s: &Arc<Session>, task: Option<&str>) {
    let pid: Option<u32> = task.and_then(|t| t.strip_prefix(super::survivors::LEFT)).and_then(|p| p.parse().ok());
    let roots: Vec<super::survivors::Ran> = s.left.lock().iter().filter(|r| task.is_none() || Some(r.pid) == pid).cloned().collect();
    if roots.is_empty() {
        return;
    }
    let stopping = roots.clone();
    let _ = crate::rt::blocking(move || stopping.iter().for_each(super::survivors::stop_tree)).await;
    let gone: std::collections::HashSet<u32> = roots.iter().map(|r| r.pid).collect();
    s.left.lock().retain(|r| !gone.contains(&r.pid));
    s.ran.lock().1.retain(|r| !gone.contains(&r.pid));
    let mut core = s.core.lock();
    for t in core.state.tasks.iter_mut().filter(|t| t.id.strip_prefix(super::survivors::LEFT).and_then(|p| p.parse::<u32>().ok()).is_some_and(|p| gone.contains(&p))) {
        t.status = "stopped".to_owned();
        t.ended = Some(Core::now());
    }
    core.touch_state();
}

/// Native Codex subagents whose work kiln never read, in a session that runs another harness now
/// (it switched before kiln read them): their work comes from Codex's own record of their threads,
/// read off the event loop; no Codex is asked. It shows at the next flush.
fn read_unread_children(s: &Arc<Session>) {
    let unread = super::codex::unread_children(&s.core.lock());
    if unread.is_empty() {
        return;
    }
    let home = super::handoff::codex_home(&s.spec.lock().env);
    let s = Arc::clone(s);
    crate::rt::spawn(async move {
        let Ok(read) = crate::rt::blocking(move || super::codex::read_rollouts(&home, unread)).await else { return };
        let mut core = s.core.lock();
        for (thread, records) in read {
            super::codex::child_from_rollout(&mut core, &thread, &super::codex::rollout_turns(&records));
        }
    });
}

fn read_state(core: &mut Core, client: String, read: super::state_view::Read) {
    let empty = serde_json::json!({"t":"reply", "client":client, "value":{"result":null}});
    let envelope = serde_json::to_vec(&empty).map_or(super::state_view::FRAME, |v| v.len());
    let stamp = format!(",\"taken_us\":{}", u64::MAX).len();
    let room = crate::proto::harness_json_room(&core.session_id, super::state_view::FRAME);
    let budget = room.saturating_sub(envelope).saturating_sub(stamp);
    let value = match super::state_view::read(&core.state, &read, budget) {
        Ok(result) => serde_json::json!({"result":result}),
        Err(code) => serde_json::json!({"error":{"code":code}}),
    };
    core.reply(client, value);
}

fn at_state(core: &mut Core, client: String, revision: &str, command: Command, apply: impl FnOnce(&mut Core, Command)) -> bool {
    use super::state_view::Error;
    let id = match &command {
        Command::QueueEdit { id, .. } | Command::QueueRemove { id } | Command::QueueMove { id, .. }
        | Command::QueueSteer { id } | Command::QueueSendNow { id } => Some(id),
        _ => None,
    };
    let check = super::state_view::field_revision(&core.state, "queue").and_then(|current| {
        if current != revision { return Err(Error::Stale) }
        if !id.is_some_and(|id| core.state.queue.iter().any(|q| &q.id == id)) { return Err(Error::InvalidPath) }
        Ok(())
    });
    match check {
        Ok(()) => {
            apply(core, command);
            let revision = super::state_view::field_revision(&core.state, "queue").ok();
            core.reply(client, serde_json::json!({"result":{"type":"applied", "revision":revision}}));
            true
        }
        Err(code) => { core.reply(client, serde_json::json!({"error":{"code":code}})); false },
    }
}

/// The actor is the only queue writer while this awaits its journal CAS. AtState is validated
/// before persistence and again by its normal application seam; admission/status changes do not
/// alter that queue revision. No accepted original is changed by an edit.
async fn journal_edit(s: &Session, files: &Files, command: Command, apply_edit: impl FnOnce(&mut Core, Command)) -> Option<Command> {
    let (mut command, at) = match command {
        Command::AtState { client, revision, command } => (*command, Some((client, revision))),
        command => (command, None),
    };
    let result = async {
        if matches!(command, Command::QueueEdit { .. } | Command::QueueRemove { .. } | Command::QueueMove { .. } | Command::QueueSteer { .. } | Command::QueueSendNow { .. }) && !s.core.lock().input_ready() { return Err(super::state_view::Error::Unreadable); }
        let Command::QueueEdit { id, text, attachments } = &command else { return Ok(None) };
        let (input, queued) = {
            let core = s.core.lock();
            if let Some((_, revision)) = &at
                && super::state_view::field_revision(&core.state, "queue").map_err(|_| super::state_view::Error::Unreadable)? != *revision { return Err(super::state_view::Error::Stale); }
            (core.input.clone(), core.state.queue.iter().chain(core.state.tasks.iter().flat_map(|task| &task.queue)).find(|queued| &queued.id == id).cloned())
        };
        let Some(input) = input else { return Ok(None) };
        let Some(old_lease) = input.controller.lock().lease(id).cloned() else { return Ok(None) };
        // A registered response may already be in this turn's native tool input. Rewriting its
        // body would let proof of the old text consume a newer edit under the same immutable id.
        if old_lease.handoff.is_some() { return Err(super::state_view::Error::Stale); }
        let queued = queued.ok_or(super::state_view::Error::InvalidPath)?;
        let pending = input.inbox.pending(&s.id, id).await.map_err(|_| super::state_view::Error::Unreadable)?.ok_or(super::state_view::Error::Unreadable)?;
        let old_bytes = pending.body_bytes.ok_or(super::state_view::Error::Budget)?;
        let mut current = pending.current_command().ok_or(super::state_view::Error::Unreadable)?.clone();
        let Command::Send { text: words, attachments: current_attachments, .. } = &mut current else { return Err(super::state_view::Error::InvalidPath) };
        *words = text.clone();
        *current_attachments = attachments.clone().unwrap_or(queued.attachments);
        let current = prepare(files, current).await.map_err(|_| super::state_view::Error::Unreadable)?;
        let new_bytes = super::admission::command_bytes(&current).map_err(|_| super::state_view::Error::Budget)?;
        let target = old_lease.bytes.checked_sub(old_bytes).and_then(|base| base.checked_add(new_bytes)).ok_or(super::state_view::Error::Budget)?;
        let lease = {
            let mut controller = input.controller.lock();
            if target > old_lease.bytes && !controller.resize(id, target).map_err(|_| super::state_view::Error::Budget)? { return Err(super::state_view::Error::Budget); }
            let mut lease = controller.lease(id).cloned().ok_or(super::state_view::Error::Unreadable)?;
            // The journal records its planned current size, but shrink cannot release the old
            // queue body's authority until the owner has actually replaced that body.
            lease.bytes = target;
            lease
        };
        let materialized = match &current { Command::Send { attachments, .. } => attachments.clone(), _ => return Err(super::state_view::Error::InvalidPath) };
        if input.inbox.edit_pending(&s.id, id, lease, current).await.is_err() {
            if target > old_lease.bytes { let _ = input.controller.lock().resize(id, old_lease.bytes); }
            return Err(super::state_view::Error::Unreadable);
        }
        if let Command::QueueEdit { attachments, .. } = &mut command { *attachments = Some(materialized); }
        Ok(Some((input, old_lease.message, old_lease.bytes, target)))
    }.await;
    match result {
        Ok(None) => Some(match at { Some((client, revision)) => Command::AtState { client, revision, command: Box::new(command) }, None => command }),
        Ok(Some((input, id, before, after))) => {
            let mut core = s.core.lock();
            let applied = match at {
                Some((client, revision)) => at_state(&mut core, client, &revision, command, apply_edit),
                None if core.state.queue.iter().chain(core.state.tasks.iter().flat_map(|task| &task.queue)).any(|item| item.id == id) => { apply_edit(&mut core, command); true }
                None => false,
            };
            if applied {
                let capacity = {
                    let mut controller = input.controller.lock();
                    match controller.applied_edit(&id, before, after) {
                        Ok(true) => { controller.advertised_ready(input.now_ns()); Some(controller.capacity()) }
                        Ok(false) => None,
                        Err(_) => { let _ = controller.unavailable(); Some(controller.capacity()) }
                    }
                };
                if let Some(capacity) = capacity { core.state.admission = Some(capacity.clone()); core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Capacity { capacity }) }); core.touch_state(); }
            }
            None
        }
        Err(code) => {
            let mut core = s.core.lock();
            if let Some((client, _)) = at { core.reply(client, serde_json::json!({"error":{"code":code}})); }
            else { core.upsert("queue-edit-retained", None, super::model::Body::Notice { level: Level::Error, text: format!("Queue edit was not applied ({code:?}); the previous communication remains retained.") }); }
            core.touch_state();
            drop(core);
            s.input_changed();
            None
        }
    }
}

async fn register_handoff(s: &Session, client: String, revision: String, handoff: String, turn: Option<i64>, ids: Vec<String>, response_digest: String) {
    use super::state_view::Error;
    let result = async {
        if !crate::util::is_id(&handoff) || response_digest.len() != 64 || !response_digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) || turn.is_none() || ids.is_empty() { return Err(Error::InvalidPath); }
        let input = {
            let core = s.core.lock();
            if core.state.turn_started != turn || super::state_view::field_revision(&core.state, "queue")? != revision { return Err(Error::Stale); }
            let mut distinct = std::collections::BTreeSet::new();
            for id in &ids {
                if !distinct.insert(id) || !core.state.queue.iter().any(|queued| &queued.id == id && queued.attachments.is_empty()) { return Err(Error::InvalidPath); }
            }
            core.input.clone().ok_or(Error::Unreadable)?
        };
        let metadata = super::admission::Handoff { id: handoff, turn, digest: response_digest };
        let (leases, newly_registered) = {
            let mut controller = input.controller.lock();
            let newly_registered: Vec<_> = ids.iter().filter(|id| controller.lease(id).is_some_and(|lease| lease.handoff.is_none())).cloned().collect();
            if !controller.register_handoff(&ids, &metadata).map_err(|_| Error::InvalidPath)? { return Err(Error::Budget); }
            let leases: Vec<_> = ids.iter().filter_map(|id| controller.lease(id).cloned()).collect();
            (leases, newly_registered)
        };
        if input.inbox.register_handoff(&s.id, &leases).await.is_err() {
            input.controller.lock().withdraw_handoff(&newly_registered, &metadata);
            return Err(Error::Unreadable);
        }
        input.observe_physical();
        let capacity = { let mut controller = input.controller.lock(); controller.advertised_ready(input.now_ns()); controller.capacity() };
        { let mut core = s.core.lock(); core.state.admission = Some(capacity.clone()); core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Capacity { capacity }) }); core.touch_state(); }
        Ok(super::state_view::field_revision(&s.core.lock().state, "queue").ok())
    }.await;
    let value = match result {
        Ok(revision) => serde_json::json!({"result":{"type":"registered","revision":revision}}),
        Err(code) => serde_json::json!({"error":{"code":code}}),
    };
    s.core.lock().reply(client, value);
}

#[cfg(test)]
mod state_command_tests {
    use super::*;

    fn queued() -> Core {
        let mut core = Core::new();
        core.session_id = "state-command".into();
        core.state.queue.push(super::super::model::QueueItem {
            id: "message".into(), text: "original".into(), at: 0,
            attachments: Vec::new(), overrides: None, reply_to: None,
        });
        core
    }

    #[test]
    fn a_paged_edit_is_checked_by_the_owner_before_the_real_adapter_changes_it() -> anyhow::Result<()> {
        let mut core = queued();
        let mut adapter = super::super::codex::Codex::new(None, None, None, false);
        let revision = super::super::state_view::field_revision(&core.state, "queue")?;
        core.state.queue.first_mut().ok_or_else(|| anyhow::anyhow!("no queue"))?.text = "changed elsewhere".into();
        at_state(&mut core, "stale-edit".into(), &revision,
            Command::QueueEdit { id: "message".into(), text: "stale replacement".into(), attachments: None },
            |core, command| adapter.command(core, command));
        assert_eq!(core.state.queue.first().map(|q| q.text.as_str()), Some("changed elsewhere"));
        assert!(core.take_out().iter().any(|u| matches!(u, Update::Reply { client, value }
            if client == "stale-edit" && value.pointer("/error/code").and_then(serde_json::Value::as_str) == Some("stale"))));

        let revision = super::super::state_view::field_revision(&core.state, "queue")?;
        core.state.status = Status::Running;
        at_state(&mut core, "current-edit".into(), &revision,
            Command::QueueEdit { id: "message".into(), text: "current replacement".into(), attachments: None },
            |core, command| adapter.command(core, command));
        assert_eq!(core.state.queue.first().map(|q| q.text.as_str()), Some("current replacement"));
        let current = super::super::state_view::field_revision(&core.state, "queue")?;
        assert!(core.take_out().iter().any(|u| matches!(u, Update::Reply { client, value }
            if client == "current-edit" && value.pointer("/result/revision").and_then(serde_json::Value::as_str) == Some(current.as_str()))));
        Ok(())
    }

    #[test]
    fn a_detail_reply_fits_the_cap_with_its_stamp_and_wire_envelope() -> anyhow::Result<()> {
        let mut core = queued();
        core.state.queue.first_mut().ok_or_else(|| anyhow::anyhow!("no queue"))?.text = "é\\\"\n".repeat(super::super::state_view::FRAME);
        let read = super::super::state_view::Read {
            revision: super::super::state_view::field_revision(&core.state, "queue")?,
            field: "queue".into(),
            path: vec![super::super::state_view::Step::Id { id: "message".into() }, super::super::state_view::Step::Key { key: "text".into() }],
            offset: 0,
        };
        read_state(&mut core, "viewer".into(), read);
        let reply = core.take_out().into_iter().find(|u| matches!(u, Update::Reply { client, .. } if client == "viewer"))
            .ok_or_else(|| anyhow::anyhow!("no page reply"))?;
        let mut json = serde_json::to_string(&reply)?;
        crate::wire::stamp::put(&mut json, Duration::MAX);
        let bytes = crate::proto::encode(&crate::proto::AgentMsg::Harness { session: core.session_id, json })?;
        assert!(bytes.len() <= super::super::state_view::FRAME);
        Ok(())
    }
}

/// The agent ended: what its harness ran ended with it, and so did what kiln took back over for it
/// (stopped with the rest). Its last state, what an ended agent shows, says so.
fn end_tasks(core: &mut Core) {
    core.end_harness_work();
    for t in core.state.tasks.iter_mut().filter(|t| t.live() && !t.id.starts_with(crate::reach::owned::PREFIX)) {
        t.status = "stopped".to_owned();
        t.ended.get_or_insert_with(Core::now);
    }
    // What it was sent and never finished, or that waited to go, doesn't go now.
    core.drop_unfinished(true);
    core.touch_state();
}

/// Whether a turn is under way once a session's status is `status`: one starts it, its end ends it,
/// and the harness ending or starting again leaves it as it was (a turn cut short stays open).
pub fn turn_open(status: Status, was: bool) -> bool {
    match status {
        Status::Running | Status::Waiting => true,
        Status::Idle => false,
        Status::Starting | Status::Exited | Status::Disconnected => was,
    }
}

/// The folder an agent starts in, as this machine knows it: `~` and `~/…` are the home directory
/// of the user kiln runs as (people type them; a process can't `chdir("~")`), nothing means home.
pub fn resolve_cwd(cwd: Option<&str>) -> String {
    let home = crate::os::home_dir();
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None | Some("~") => home.to_string_lossy().into_owned(),
        Some(c) => match c.strip_prefix("~/").or_else(|| c.strip_prefix("~\\")) {
            Some(rest) => home.join(rest).to_string_lossy().into_owned(),
            None => c.to_owned(),
        },
    }
}

/// Start a session; its updates go to `sink`, tagged with its id.
pub fn start(id: String, spec: Spec, sink: impl Into<Sink>, on_change: Arc<dyn Fn() + Send + Sync>, files: Files) -> anyhow::Result<Arc<Session>> {
    start_seeded(id, spec, sink, on_change, files, None)
}

/// Go on with a session this process's previous image handed over ([`Session::hand_over`]): its
/// timeline and state as they were, and its harness process, still running.
pub fn start_handed(id: String, spec: Spec, sink: impl Into<Sink>, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, h: Box<Handed>) -> anyhow::Result<Arc<Session>> {
    start_handed_input(id, spec, sink, on_change, files, h, None)
}

pub(super) fn start_handed_input(id: String, spec: Spec, sink: impl Into<Sink>, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, h: Box<Handed>, input: Option<super::admission::Input>) -> anyhow::Result<Arc<Session>> {
    use base64::Engine as _;
    let kind = Kind::get(&h.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {:?}", h.kind))?;
    let h = *h;
    let mut core = Core::thaw(h.core);
    if let Some(input) = input {
        let _ = input.controller.lock().initial_epoch(core.state.epoch);
        core.input = Some(input);
    }
    let adapter = super::kinds::Adapter::thaw(h.adapter);
    core.input_legacy_flights = adapter.retained_input_ids();
    let adopt = Adopt { process: h.process, adapter, partial: base64::engine::general_purpose::STANDARD.decode(h.partial).unwrap_or_default() };
    begin(id, spec, kind, core, sink.into(), on_change, files, None, Some(adopt))
}

/// Start a session, seeded with what it had on another machine when it moved here.
pub fn start_seeded(id: String, spec: Spec, sink: impl Into<Sink>, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, seed: Option<Seed>) -> anyhow::Result<Arc<Session>> {
    start_seeded_input(id, spec, sink, on_change, files, seed, None)
}

pub(super) fn start_seeded_input(id: String, spec: Spec, sink: impl Into<Sink>, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, seed: Option<Seed>, input: Option<super::admission::Input>) -> anyhow::Result<Arc<Session>> {
    let kind = Kind::get(&spec.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {:?}", spec.kind))?;
    let cwd = resolve_cwd(spec.cwd.as_deref());
    let mut core = Core::new();
    core.state.cwd.clone_from(&cwd);
    core.state.model = spec.model.clone().unwrap_or_default();
    core.state.mode = spec.mode.clone().unwrap_or_default();
    // What it starts at, until its harness says what it applied.
    core.state.effort = spec.effort.clone().unwrap_or_default();
    core.state.speed = spec.speed.clone().unwrap_or_default();
    core.state.harness_session.clone_from(&spec.resume);
    // Moved here: its timeline as it was, then the line saying so.
    let mut items = None;
    let restored = seed.as_ref().is_some_and(|sd| sd.restored);
    if let Some(sd) = seed.as_ref().filter(|_| restored) {
        // Restarted here: its timeline and state as the store kept them, and nothing said of it.
        core.restore(sd.events.clone(), sd.state.clone());
        // Attention plan 01a199e1b177752e84dad5dc990e6a15: an explicit restore
        // from kiln's copy must seed the provider too, not merely the visible timeline.
        items = sd.items.clone();
        core.state.model = spec.model.clone().unwrap_or_default();
        core.state.mode = spec.mode.clone().unwrap_or_default();
        core.harness = Some(kind.name.to_owned());
    } else if let Some(sd) = seed {
        core.adopt(sd.events, sd.state);
        // A new writer of its conversation: what the machine it left still says no longer counts.
        core.state.epoch += 1;
        core.state.model = spec.model.clone().unwrap_or_default();
        core.state.mode = spec.mode.clone().unwrap_or_default();
        core.harness = Some(kind.name.to_owned());
        let folder = if core.state.cwd == sd.from_cwd { String::new() } else { format!(" (it works in {} here; it was in {})", core.state.cwd, sd.from_cwd) };
        let mut text = if sd.local {
            "Taken over by this machine's newer kiln runner, which replaces the older one — the conversation, its timeline and queue came along".to_owned()
        } else {
            format!("Moved here from {}{folder} — the conversation, its timeline and queue came along", sd.from)
        };
        // Interrupted to move: the turn goes on here, first thing.
        if sd.picks_up {
            text.push_str(", and the turn it was in the middle of goes on here");
            let ask = format!("This conversation just moved here from {}, which interrupted your last turn. Pick up where you left off.", sd.from);
            core.state.queue.insert(0, super::model::QueueItem { id: crate::util::new_id(), text: ask, at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
        }
        if let Some(n) = &sd.note {
            text.push_str(". ");
            text.push_str(n);
        }
        core.upsert(&format!("harness-switch:moved-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
        items = sd.items;
        core.state.arriving = sd.synced;
    }
    core.state.reach.clone_from(&spec.reach);
    if let Some(input) = input {
        let _ = input.controller.lock().initial_epoch(core.state.epoch);
        core.input = Some(input);
    }
    named_ahead(&mut core.meta, kind, spec.model.as_deref());
    begin(id, spec, kind, core, sink.into(), on_change, files, items, None)
}

/// Harness `kind`'s lists as this machine knows them, for a session's page to name its settings by
/// before the harness says anything (a new session may wait for room on its machine, then take
/// seconds to start; a switch starts the harness anew): its permission modes, its models (but an
/// ACP agent's), and the effort levels and speeds of the one it runs (`model`; none, its default),
/// as Mission control's composer offered them. What the harness says once it runs takes their place.
fn named_ahead(meta: &mut super::model::Meta, kind: &Kind, model: Option<&str>) {
    let models = super::kinds::models_of(kind.id);
    let runs = match model.filter(|m| !m.is_empty()) {
        Some(id) => models.iter().find(|m| m.id == id),
        None => models.iter().find(|m| m.default).or(models.first()),
    }
    .cloned();
    if meta.modes.is_empty() {
        meta.modes = kind.modes().0;
    }
    if meta.efforts.is_empty() {
        meta.efforts = match runs.as_ref().filter(|m| !m.efforts.is_empty()) {
            Some(m) => m.efforts.iter().map(|e| super::model::ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false }).collect(),
            None => kind.efforts(),
        };
    }
    if meta.speeds.is_empty() {
        meta.speeds = runs.map(|m| m.speeds).unwrap_or_default();
    }
    // An ACP agent's own list says it can run them: it lists none before it's signed in.
    if meta.models.is_empty() && !matches!(kind.protocol, Protocol::Acp(_)) {
        meta.models = models;
    }
}

/// A session with this core, its task started (`adopt`: going on with a harness process the
/// previous image of this process handed over).
#[allow(clippy::too_many_arguments)]
fn begin(id: String, spec: Spec, kind: &'static Kind, mut core: Core, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, items: Option<Vec<serde_json::Value>>, adopt: Option<Adopt>) -> anyhow::Result<Arc<Session>> {
    let now = crate::util::now_ms();
    core.session_id.clone_from(&id);
    core.state.max_subagents = core.state.max_subagents.or(spec.max_subagents);
    // It goes to work only with room on its machine (`super::limit`).
    core.room = Some(Arc::clone(&files.room));
    let cwd = core.state.cwd.clone();
    let info = Info {
        deferred: Default::default(),
        id: id.clone(),
        kind: kind.id.to_owned(),
        kind_name: kind.name.to_owned(),
        target: String::new(),
        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| kind.name.to_owned()),
        shell: spec.shell.clone(),
        cwd,
        tree: None,
        created_at: now,
        status: Status::Starting,
        title: String::new(),
        model: String::new(),
        mode: String::new(),
        harness_session: spec.resume.clone(),
        approvals: Vec::new(),
        queued: 0,
        admission: core.state.admission.clone(),
        queue: Vec::new(),
        queue_partial: false,
        queue_revision: None,
        queue_next: None,
        said: Vec::new(),
        tasks_running: 0,
        subagents: Vec::new(),
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: None,
        goal: String::new(),
        objective: None,
        summary: String::new(),
        doing: None,
        epoch: 0,
        todos_done: 0,
        todos_total: 0,
        files_changed: 0,
        lines_added: 0,
        lines_removed: 0,
        context_pct: None,
        turn_started: None,
        limits: Vec::new(),
        account: None,
        plan: None,
        left_behind: 0,
        reach: spec.reach.clone(),
        pause: None,
        frozen: None,
        perms: None,
        room_wait: None,
        last_turn: None,
        keep_off: false,
        held: None,
        refused: None,
        move_on_limit: false,
        move_plan: Default::default(),
        pause_at: None,
        max_subagents: core.state.max_subagents,
        parent: spec.parent.clone(),
        plan_run: spec.plan_run.clone(),
        helper: spec.helper.clone(),
        subagent: spec.subagent,
        variant_of: spec.variant_of.clone(),
        no_bypass: !spec.allow_bypass,
        workspace: spec.workspace.clone(),
        isolation: spec.isolation.clone(),
    };
    let (ctl, ctl_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let s = Arc::new(Session {
        id,
        spec: Mutex::new(spec),
        created_at: now,
        core: Mutex::new(core),
        info: Mutex::new(info),
        ctl,
        pid: Mutex::new(None),
        turn_open: std::sync::atomic::AtomicBool::new(false),
        ran: Mutex::new((None, Vec::new())),
        left: Mutex::new(Vec::new()),
        moving: Mutex::new(false),
        prompted: std::sync::atomic::AtomicBool::new(false),
        carried: Mutex::new(items.clone()),
        ended: std::sync::atomic::AtomicBool::new(false),
        ended_changed: crate::rt::Changed::new(),
        owned_reach: Mutex::new(std::collections::HashMap::new()),
    });
    let run = Arc::clone(&s);
    // On the shard that owns it, where its lane runs and its frames are taken (runner-fan): its pipes are served there too
    // (`crate::proc::spawn_duplex_in_pool`, from this task).
    crate::rt::spawn_owner(super::host::owner_shard(&s.id), async move {
        let id = run.id.clone();
        if let Err(p) = std::panic::AssertUnwindSafe(drive(Arc::clone(&run), kind, ctl_rx, sink.clone(), on_change, files, items, adopt)).catch_unwind().await {
            tracing::error!(session = %id, panic = %crate::sup::panic_message(p.as_ref()), "agent session task panicked");
        }
        // Gone, however it ended (it said `Closed`, went on elsewhere or panicked): the last it hands on.
        sink.gone(&id).await;
        // Then ended, all of it handed on: what waits for its end (`close_wait`) goes on.
        run.ended.store(true, Ordering::Release);
        run.ended_changed.notify();
    });
    Ok(s)
}

/// A message for the agent (what a session's inbox takes once, CP-T3.2), not a command to kiln.
pub fn is_message(c: &Command) -> bool {
    matches!(c, Command::Send { .. } | Command::SkillRun { .. })
}

/// Most of what's waiting in a session's channel a move looks at.
const GATHER_MAX: usize = 100_000;

/// A move begins: the session takes no messages from now on, and those it was sent and hasn't taken
/// (still in its channel, or carried while its harness was down) join the end of its queue, in the
/// order they came, to go with it (one for a subagent, its subagent's). Everything else in its
/// channel is held back, for if it stays.
fn gather(s: &Session, ctl_rx: &crate::memory::queue::Receiver<Ctl>, carry: &mut Vec<Command>) -> Vec<Ctl> {
    let mut held = Vec::new();
    let mut waiting = Vec::new();
    {
        let mut moving = s.moving.lock();
        *moving = true;
        // Under the lock no message goes in, so this ends; bounded besides.
        for _ in 0..GATHER_MAX {
            match ctl_rx.try_recv() {
                Ok(Ctl::Cmd(c)) if is_message(&c) => waiting.push(*c),
                Ok(other) => held.push(other),
                Err(_) => break,
            }
        }
    }
    let (carried, rest): (Vec<Command>, Vec<Command>) = std::mem::take(carry).into_iter().partition(is_message);
    *carry = rest;
    let mut core = s.core.lock();
    let before = core.state.queue.len();
    for c in carried.into_iter().chain(waiting) {
        let task = match &c {
            Command::Send { task, .. } => task.clone(),
            _ => None,
        };
        let Some(m) = queued(c) else { continue };
        match task {
            // One for a subagent waits in its queue, which goes with the session, or (one that takes
            // none now) is shown not sent there: never in the agent's own (CP-T3.2, the nineteenth
            // adversarial check's).
            Some(task) if core.state.tasks.iter().any(|t| t.id == task && t.takes.contains(&super::model::SendMode::Queue)) => {
                super::subagent::hold(&mut core, &task, super::model::QueueItem { overrides: None, ..m });
            }
            Some(task) => super::subagent::refuse(&mut core, &task, &m, "it takes no message from kiln now."),
            None => core.state.queue.push(m),
        }
    }
    if core.state.queue.len() != before {
        core.touch_state();
    }
    held
}

/// A message as it waits in a session's queue.
fn queued(c: Command) -> Option<super::model::QueueItem> {
    let at = Core::now();
    match c {
        Command::Send { id, text, attachments, overrides, reply_to, .. } => Some(super::model::QueueItem { id, text, at, attachments, overrides, reply_to }),
        Command::SkillRun { name, args, id, .. } => {
            let text = match args.filter(|a| !a.trim().is_empty()) {
                Some(a) => format!("/{name} {a}"),
                None => format!("/{name}"),
            };
            Some(super::model::QueueItem { id: id.unwrap_or_else(crate::util::new_id), text, at, attachments: Vec::new(), overrides: None, reply_to: None })
        }
        _ => None,
    }
}

/// The session goes on at `epoch` (epochs only rise): its state says so, sent with its next flush.
async fn progress_promotion(s: &Session, files: &Files) {
    let Some(host) = files.host.as_ref().and_then(std::sync::Weak::upgrade) else { return };
    // Prepare retained originals, or finish partial row/epoch alignment, before
    // flushing callbacks. Ownership stays fenced until their journal effects commit.
    if let Err(error) = host.reconcile_legacy_owner(s).await {
        let mut core = s.core.lock();
        if matches!(error, super::inbox::PromotionError::Invalid { .. } | super::inbox::PromotionError::Stale) && let Some(input) = &core.input { let _ = input.controller.lock().unavailable(); }
        core.upsert("legacy-promotion-held", None, Body::Notice { level: Level::Warn, text: format!("Legacy ownership remains held: {error}") });
        core.touch_state();
        return;
    }
    // A callback already parsed before the owner event commits under its original lease epoch.
    let _ = flush_input(s).await;
    {
        let mut core = s.core.lock();
        if !core.input_stages.is_empty() || !core.input_retained.is_empty() || !core.input_receipts.is_empty() {
            core.upsert("legacy-promotion-held", None, Body::Notice { level: Level::Warn, text: "Legacy ownership remains held until its existing input effects commit. The original work is retained.".into() });
            core.touch_state();
            return;
        }
    }
    if let Err(error) = host.progress_legacy_promotion(s).await {
        let mut core = s.core.lock();
        if matches!(error, super::inbox::PromotionError::Invalid { .. } | super::inbox::PromotionError::Stale) && let Some(input) = &core.input { let _ = input.controller.lock().unavailable(); }
        core.upsert("legacy-promotion-held", None, Body::Notice { level: Level::Warn, text: format!("Legacy ownership remains held: {error}") });
        core.touch_state();
    }
}

async fn raise_epoch(s: &Session, files: &Files, epoch: u64) {
    if s.epoch() >= epoch { return; }
    let Some(input) = s.core.lock().input.clone() else { return };
    if input.inbox.promotion(&s.id).await.ok().flatten().is_some_and(|p| p.to_epoch == epoch) { progress_promotion(s, files).await; }
    else { let mut core = s.core.lock(); let _ = input.controller.lock().unavailable(); core.upsert("legacy-promotion-held", None, Body::Notice { level: Level::Warn, text: "The certified epoch has no matching durable promotion intent; existing work remains retained.".into() }); core.touch_state(); }
}

/// Background work a step asked for.
struct Jobs {
    previews: Vec<super::core::PreviewJob>,
    changes: Vec<super::core::ChangeJob>,
}

fn legacy_effect_waiting_for_original(s: &Session, input: &super::admission::Input, message: &str) -> bool {
    let core = s.core.lock();
    let controller = input.controller.lock();
    let Some((from, _)) = controller.promoting_epochs() else { return false };
    core.state.epoch == from && controller.capacity().epoch == from && controller.lease(message).is_none_or(|lease| lease.epoch == from && lease.token.is_none())
        && (core.input_legacy_flights.iter().any(|flight| flight.id == message)
            || core.state.queue.iter().chain(core.state.tasks.iter().flat_map(|task| &task.queue)).any(|item| item.id == message))
}

async fn legacy_effects_waiting_for_epoch(s: &Session, input: &super::admission::Input, messages: &[String], refusal: &super::inbox::Refusal) -> bool {
    let super::inbox::Refusal::StaleEpoch { session, sent, current } = refusal else { return false };
    if session != &s.id || !messages.iter().all(|message| legacy_effect_waiting_for_original(s, input, message)) || input.controller.lock().promoting_epochs() != Some((*sent, *current)) { return false; }
    matches!(input.inbox.promotion(&s.id).await, Ok(Some(promotion)) if promotion.phase == super::inbox::PromotionPhase::Rebinding && (promotion.from_epoch, promotion.to_epoch) == (*sent, *current))
}

/// Hand out what the core produced: updates to the sink, lines to the process.
async fn flush_input(s: &Session) -> bool {
    let (input, stages, retained, replay, receipts) = {
        let mut core = s.core.lock();
        (core.input.clone(), std::mem::take(&mut core.input_stages), std::mem::take(&mut core.input_retained), std::mem::take(&mut core.input_replay), std::mem::take(&mut core.input_receipts))
    };
    let Some(input) = input else {
        let mut core = s.core.lock();
        core.input_stages.extend(stages);
        core.input_retained.extend(retained);
        core.input_receipts.extend(receipts);
        core.input_replay = replay;
        return core.input_legacy_flights.is_empty();
    };
    if matches!(replay, super::admission::Replay::ProviderReady) {
        let capacity = {
            let mut controller = input.controller.lock();
            if controller.provider_ready().is_err() { let _ = controller.unavailable(); }
            controller.advertised_ready(input.now_ns());
            controller.capacity()
        };
        let mut core = s.core.lock();
        core.state.admission = Some(capacity.clone());
        core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Capacity { capacity }) });
        core.touch_state();
    }
    let mut write_ready = input.controller.lock().writes_ready();
    for message in stages {
        let epoch = input.controller.lock().lease(&message).map(|l| l.epoch);
        let Some(epoch) = epoch else {
            if legacy_effect_waiting_for_original(s, &input, &message) { s.core.lock().input_stages.push(message); write_ready = false; }
            continue;
        };
        let staged = input.inbox.stage_pending(&s.id, &message, epoch, super::admission::Stage::ProviderFlight).await;
        if !matches!(&staged, Ok(true)) {
            let deferred = match &staged {
                Ok(false) => legacy_effect_waiting_for_original(s, &input, &message),
                Err(error) => legacy_effects_waiting_for_epoch(s, &input, std::slice::from_ref(&message), error).await,
                _ => false,
            };
            s.core.lock().input_stages.push(message);
            if !deferred { let _ = input.controller.lock().unavailable(); }
            write_ready = false;
        }
    }
    for message in retained {
        let lease = input.controller.lock().lease(&message).cloned();
        let Some(lease) = lease else { s.core.lock().input_retained.push(message); continue };
        let staged = input.inbox.stage_pending(&s.id, &message, lease.epoch, super::admission::Stage::Carried).await;
        if !matches!(&staged, Ok(true)) {
            let deferred = match &staged {
                Ok(false) => legacy_effect_waiting_for_original(s, &input, &message),
                Err(error) => legacy_effects_waiting_for_epoch(s, &input, std::slice::from_ref(&message), error).await,
                _ => false,
            };
            s.core.lock().input_retained.push(message);
            if !deferred { let _ = input.controller.lock().unavailable(); }
            write_ready = false;
            continue;
        }
        let capacity = { let mut controller = input.controller.lock(); let _ = controller.stage(&message, super::admission::Stage::Carried); controller.capacity() };
        if let Some(token) = lease.token { s.core.lock().out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Waiting { token, epoch: lease.epoch, cause: super::admission::WaitCause::ConsumptionUnknown, capacity }) }); }
    }
    if matches!(replay, super::admission::Replay::ProviderReady) && write_ready {
        let mut after = None;
        loop {
            let next = input.inbox.pending_next(&s.id, after).await;
            let Ok(Some((key, pending))) = next else { break };
            after = Some(key);
            if pending.disposition.is_some() || pending.lease.stage != super::admission::Stage::Carried || !input.controller.lock().lease(&pending.lease.message).is_some_and(|lease| lease.stage == super::admission::Stage::Carried) { continue; }
            let queued = { let core = s.core.lock(); core.state.queue.iter().chain(core.state.tasks.iter().flat_map(|task| &task.queue)).any(|item| item.id == pending.lease.message) };
            if let Some(command) = pending.current_command() && !queued && s.command(command.clone()) { let _ = input.controller.lock().stage(&pending.lease.message, super::admission::Stage::InCtl); }
        }
    }
    for receipt in receipts {
        let deferred = {
            let core = s.core.lock();
            let controller = input.controller.lock();
            receipt.messages.iter().filter(|message| controller.lease(message).is_none() && core.input_legacy_flights.iter().any(|flight| &flight.id == *message)).cloned().collect::<Vec<_>>()
        };
        let (messages, epoch) = {
            let c = input.controller.lock();
            let messages: Vec<_> = receipt.messages.iter().filter(|message| c.lease(message).is_some()).cloned().collect();
            let epoch = messages.first().and_then(|message| c.lease(message)).map(|lease| lease.epoch);
            (messages, epoch)
        };
        let Some(epoch) = epoch else {
            if !deferred.is_empty() { s.core.lock().input_receipts.push(receipt); write_ready = false; }
            continue;
        };
        let pending = input.inbox.dispose_pending_batch(&s.id, &messages, epoch, &receipt.disposition).await;
        match pending {
            Ok(pending) => {
                if !deferred.is_empty() {
                    s.core.lock().input_receipts.push(super::admission::Receipt { messages: deferred, ..receipt.clone() });
                    write_ready = false;
                }
                let capacity = {
                    let mut c = input.controller.lock();
                    if pending.iter().all(|p| p.disposition == receipt.disposition) {
                        let ids: Vec<_> = pending.iter().map(|p| p.lease.message.as_str()).collect();
                        if c.finish_batch(&ids, receipt.disposition.clone(), receipt.at_ns, receipt.continuously_available).is_err() { let _ = c.unavailable(); }
                    } else {
                        for p in &pending {
                            if c.finish(&p.lease.message, p.disposition.clone(), receipt.at_ns, false).is_err() { let _ = c.unavailable(); }
                        }
                    }
                    c.advertised_ready(input.now_ns());
                    c.capacity()
                };
                input.observe_physical();
                let mut core = s.core.lock();
                core.state.admission = Some(capacity.clone());
                // The durable terminal row is the witness now; retaining this fallback ID
                // would turn a repeated native callback into an unresolvable missing lease.
                core.input_legacy_flights.retain(|flight| !pending.iter().any(|entry| entry.lease.message == flight.id));
                for pending in pending {
                    let Some(token) = pending.lease.token else { continue };
                    let event = match pending.disposition {
                        super::admission::Disposition::Consumed { boundary } => super::admission::FlowEvent::Consumed { token, epoch, boundary, capacity: capacity.clone() },
                        disposition => super::admission::FlowEvent::Disposed { token, epoch, disposition, capacity: capacity.clone() },
                    };
                    core.out.push(Update::MailFlow { event: Box::new(event) });
                }
                core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Capacity { capacity }) });
                core.touch_state();
            }
            Err(super::inbox::CompletionError::MissingOriginal { message }) if legacy_effect_waiting_for_original(s, &input, &message) => {
                write_ready = false;
                s.core.lock().input_receipts.push(receipt);
            }
            Err(super::inbox::CompletionError::Refused(error)) if legacy_effects_waiting_for_epoch(s, &input, &receipt.messages, &error).await => {
                write_ready = false;
                s.core.lock().input_receipts.push(receipt);
            }
            Err(_) => {
                write_ready = false;
                let (tokens, capacity) = { let mut c = input.controller.lock(); let _ = c.unavailable(); (messages.iter().filter_map(|message| c.lease(message).and_then(|l| l.token)).collect::<Vec<_>>(), c.capacity()) };
                let mut core = s.core.lock();
                core.state.admission = Some(capacity.clone());
                core.touch_state();
                core.input_receipts.push(receipt);
                for token in tokens {
                    core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Waiting { token, epoch, cause: super::admission::WaitCause::SourceUnavailable, capacity: capacity.clone() }) });
                }
            }
        }
    }
    write_ready
}

async fn flush(s: &Session, sink: &Sink, stdin: Option<&crate::memory::queue::Sender<Vec<u8>>>) -> Jobs {
    let write_ready = flush_input(s).await;
    let (updates, writes, previews, changes) = {
        let mut core = s.core.lock();
        let u = core.take_out();
        let writes = core.take_writes();
        let writes = if write_ready { writes } else {
            let (held, control): (Vec<_>, Vec<_>) = writes.into_iter().partition(|wire| super::admission::native_input(wire));
            core.writes = held;
            control
        };
        (u, writes, std::mem::take(&mut core.previews), core.take_change_jobs())
    };
    if let Some(stdin) = stdin {
        // W20-R01: bridge the current scoped skills at actual delivery, after queueing/holds.
        // The blocking pool reads files; adapters and the runtime shard do no filesystem work.
        let protocol = Kind::get(&s.info.lock().kind).map(|kind| kind.protocol);
        let writes = if let Some(protocol) = protocol.filter(|p| crate::skills::bridge::needed(*p, &writes)) {
            let folder = crate::skills::place::root().join("sessions").join(&s.id);
            let pending = writes.clone();
            let (native, bridge, refresh) = { let mut core = s.core.lock(); (core.installed_skill_roots, Arc::clone(&core.skill_bridge), std::mem::take(&mut core.skill_refresh)) };
            match crate::rt::blocking(move || {
                let mut bridge = bridge.lock();
                refresh.apply(&mut bridge);
                let delivered = bridge.deliver(&folder, protocol, native, pending);
                let advertised = bridge.advertised();
                (delivered, advertised)
            }).await {
                Ok((delivered, advertised)) => { s.core.lock().skills_delivered(delivered.projections, advertised); delivered.writes }
                Err(_) => { let mut core = s.core.lock(); core.skill_refresh = core.skill_refresh.merge(refresh); writes }
            }
        } else { writes };
        for w in writes {
            let mut line = w.into_bytes();
            line.push(b'\n');
            let _ = stdin.send(line).await;
        }
    }
    if !updates.is_empty() {
        let now = crate::util::now_ms();
        s.info.lock().last_activity = now;
        // Its harness wrote: the recorder looks at its record's files once the store has these (consts-audit B3).
        crate::convo::record::active(now);
    }
    for u in updates {
        if sink.send((s.id.clone(), u)).await.is_err() {
            break;
        }
    }
    Jobs { previews, changes }
}

/// The next chunk from one of the harness's pipes. One already closed never answers again: a
/// closed channel is always ready, so it would be chosen first on every turn of the loop, which
/// would spin without ever yielding (starving every task on its thread, the one that reaps the
/// harness among them) and never see the other pipe close or the process exit.
async fn pipe(rx: &crate::memory::queue::Receiver<Vec<u8>>, open: bool) -> Option<Vec<u8>> {
    if open { rx.recv().await.ok() } else { std::future::pending().await }
}

enum Ev {
    InputBudget,
    PromotionJournal,
    Change((String, super::diff::Found)),
    Out(Option<Vec<u8>>),
    Err(Option<Vec<u8>>),
    Ctl(Option<Ctl>),
    Preview(Option<(String, Vec<super::model::FileDiff>)>),
    Exit(Option<i32>),
    Fx(Option<Done>),
    Tick,
    /// Headroom has room for work it held (docs/headroom.md).
    Room,
}

async fn input_budget(s: &Session) {
    let input = s.core.lock().input.clone();
    match input {
        Some(input) => input.physical_changed().await,
        None => std::future::pending::<()>().await,
    }
}

fn promotion_pending(s: &Session) -> bool {
    let input = s.core.lock().input.clone();
    input.is_some_and(|input| { let controller = input.controller.lock(); controller.promoting() })
}

async fn promotion_journal(s: &Session) {
    let input = s.core.lock().input.clone();
    match input { Some(input) => input.promotion_journal_changed(&s.id).await, None => std::future::pending().await }
}

fn publish_input_budget(s: &Session) -> bool {
    let mut core = s.core.lock();
    let Some(input) = core.input.clone() else { return false };
    let Some(improved) = input.take_budget_change() else { return false };
    let capacity = { let mut c = input.controller.lock(); if c.budget_changed(improved).is_err() { let _ = c.unavailable(); } c.advertised_ready(input.now_ns()); c.capacity() };
    core.state.admission = Some(capacity.clone());
    core.out.push(Update::MailFlow { event: Box::new(super::admission::FlowEvent::Capacity { capacity }) });
    core.touch_state();
    improved
}

/// What of a session's pause is kept on disk (`host::Saved`): a change is written there.
type PauseMarks = (Option<(i64, Option<i64>, bool, bool)>, Option<u8>, bool);

fn pause_marks(st: &State) -> PauseMarks {
    (st.pause.as_ref().map(|p| (p.since, p.until, p.auto, p.checkpoint.is_some())), st.pause_at, st.manual_resume)
}

/// What an effect carried out off the event loop came back with.
enum Done {
    /// A run (or a fetch) ended.
    Run { job: String, code: Option<i32>, output: String },
    /// The answer for one client.
    Reply { client: String, value: serde_json::Value },
}

/// Longest a run of the harness's CLI may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// Most of a run's output handed back.
const RUN_OUTPUT_MAX: usize = 32 * 1024;
/// Largest file a read hands back.
const READ_MAX: u64 = 512 * 1024;
/// How often the adapter gets a tick while the process runs.
const TICK: Duration = Duration::from_secs(5);

/// The wait before a harness whose process ended (nobody ending the agent) starts again; twice
/// the wait before for each that ends after it, at most `AGAIN_MOST`, from `AGAIN_FIRST` again
/// once one runs `AGAIN_RESET`.
const AGAIN_FIRST: Duration = Duration::from_secs(2);
const AGAIN_MOST: Duration = Duration::from_secs(300);
const AGAIN_RESET: Duration = Duration::from_secs(600);
/// A harness that ends within `SOON` of each of its last `SOON_TRIES` starts won't start: it
/// waits for a person.
const SOON: Duration = Duration::from_secs(20);
const SOON_TRIES: u32 = 3;
/// What an agent is told when its process ended part-way through a turn and started again.
const PICK_UP: &str = "Your process ended part-way through your last turn, and kiln started it again: your conversation is as it was. Pick up where you left off.";

/// How a harness's process ended, in words: its exit code, and the signal one past 128 stands for.
fn ended_how(code: Option<i32>) -> String {
    let signal = |n: i32| match n {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        9 => "SIGKILL".to_owned(),
        15 => "SIGTERM".to_owned(),
        n => format!("signal {n}"),
    };
    match code {
        None => "a signal ended it".to_owned(),
        Some(c) if (129..160).contains(&c) => format!("exit {c}: {} from outside it", signal(c - 128)),
        Some(c) => format!("exit {c}"),
    }
}

/// Completes when headroom has room for this session's held work: only while it holds some (so
/// sessions with nothing waiting don't wake for it).
async fn held_room(core: &Mutex<Core>) {
    // Listening before looking: room given between the look and the wait isn't missed.
    let room = crate::headroom::room();
    let (queued, held, granted) = {
        let c = core.lock();
        let queued = !c.state.queue.is_empty();
        (queued, crate::headroom::held(&c.session_id), queued && crate::headroom::take_grant(&c.session_id))
    };
    if queued && granted {
        // Let in (perhaps before this looked): its queue goes now, once.
    } else if queued && held {
        room.await;
        let c = core.lock();
        if !c.state.queue.is_empty() {
            crate::headroom::take_grant(&c.session_id);
        }
    } else {
        futures::future::pending::<()>().await;
    }
}
/// How long a take-over waits for the session it takes over to end.
const TAKEOVER_WAIT: Duration = Duration::from_secs(15);
/// How long a session taken over from outside kiln has to quit before it is killed.
const TAKEOVER_QUIT: Duration = Duration::from_secs(10);

/// Most of a task's output returned at once, and the tail returned without an offset.
const TASK_READ_MAX: u64 = 256 * 1024;
const TASK_TAIL: u64 = 64 * 1024;

/// Codex's child messages and tool results are kept with the parent's events, not in Claude's
/// task files or the terminal byte buffer. Project only this child's output; neither its
/// assignment nor a parent's, sibling's or descendant's messages belong in this view.
/// History reads can replace earlier items, so byte offsets cannot safely append snapshots.
pub(super) fn codex_task_output(core: &Core, task: &Task) -> serde_json::Value {
    use super::core::TaskOutput;
    use super::model::{Body, OUTPUT_MAX, clip_middle};
    use serde_json::json;

    let mut output = TaskOutput::default();
    let mut summary_seen = false;
    let mut truncated = false;
    let mut add = |text: &str| {
        if !text.trim().is_empty() {
            if output.total > 0 {
                output.append(b"\n\n");
            }
            output.append(text.as_bytes());
        }
    };
    if let Some(parent) = task.tool_id.as_deref() {
        let prefix = format!("child:{}:", task.id);
        // A batch spawn may give siblings the same parent tool; the child thread is also
        // part of every Codex event id, so that cannot mix their output.
        for event in core.events().filter(|e| e.parent.as_deref() == Some(parent) && e.id.starts_with(&prefix)) {
            match &event.body {
                Body::Assistant { text, .. } => {
                    summary_seen |= task.summary.as_deref().is_some_and(|s| s == text || s == clip_middle(text, OUTPUT_MAX).0);
                    add(text);
                }
                Body::Tool(tool) => {
                    if let Some(text) = tool.output.as_deref().filter(|s| !s.trim().is_empty()) {
                        add(&format!("{}\n{text}", tool.title));
                        truncated |= tool.truncated;
                    }
                }
                Body::Notice { text, .. } => add(text),
                _ => {}
            }
        }
    }
    // Completed children can outlive the bounded timeline, or report a result before their
    // history is available. Keep that result readable without inventing a transcript.
    if !summary_seen && let Some(summary) = task.summary.as_deref() {
        add(summary);
    }
    if output.total == 0 && !task.live() {
        return json!({"error": "No readable output has been received from this Codex subagent.", "live": false});
    }
    let (bytes, offset, skipped) = output.since(output.total.saturating_sub(TASK_TAIL));
    json!({"data": String::from_utf8_lossy(&bytes), "offset": offset, "live": task.live(),
        "truncated": truncated || skipped || output.total > TASK_TAIL, "reset": true, "pending": output.total == 0})
}

/// Where Claude Code streams a task's output while it lives: `<tmp>/<cwd as a name>/<session>/
/// tasks/<task>.output`, `<tmp>` being `CLAUDE_CODE_TMPDIR` or the system's `claude-<uid>`.
fn claude_task_file(env: &[(String, String)], cwd: &str, session: Option<&str>, task: &str) -> Option<String> {
    let session = session.filter(|s| !s.is_empty() && !s.contains(['/', '\\']))?;
    if task.is_empty() || task.contains(['/', '\\', '.']) {
        return None;
    }
    let root = env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CODE_TMPDIR")
        .map(|(_, v)| std::path::PathBuf::from(v))
        .or_else(|| std::env::var_os("CLAUDE_CODE_TMPDIR").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("claude-{}", crate::os::uid())));
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(root.join(dir).join(session).join("tasks").join(format!("{task}.output")).to_string_lossy().into_owned())
}

/// A file's bytes after `since` (its tail without one, the start again when it shrank below
/// `since`), at most `TASK_READ_MAX`: (text, the offset after them, whether older bytes were
/// skipped). None when it cannot be read. Blocking.
fn read_tail(p: &std::path::Path, since: Option<u64>) -> Option<(String, u64, bool)> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    let want = match since {
        Some(s) if s <= len => s,
        Some(_) => 0,
        None => len.saturating_sub(TASK_TAIL),
    };
    let start = want.max(len.saturating_sub(TASK_READ_MAX));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start + buf.len() as u64, start > want || (since.is_none() && start > 0)))
}

/// An ending agent's group of processes, after `after` (its harness's time to stop): whatever is
/// still in it ended at once, then the group removed.
#[allow(clippy::disallowed_methods)] // (e) a picked grace before it acts
fn end_group(g: crate::os::contain::Group, after: Duration) {
    crate::rt::spawn(async move {
        crate::rt::sleep(after).await;
        let _ = crate::rt::blocking(move || {
            g.kill();
            for _ in 0..20 {
                if g.pids().is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            g.remove();
        })
        .await;
    });
}

/// The processes under `root`, newest first: (pid, command line). Blocking.
fn descendants(root: u32) -> Vec<(u32, Vec<String>)> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).without_tasks());
    let mut kids: std::collections::HashMap<Pid, Vec<Pid>> = std::collections::HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
    }
    let mut out: Vec<(u64, u32, Vec<String>)> = Vec::new();
    let mut todo = vec![Pid::from_u32(root)];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = todo.pop() {
        for c in kids.get(&p).into_iter().flatten() {
            if seen.insert(*c) {
                todo.push(*c);
                if let Some(proc_) = sys.process(*c) {
                    out.push((proc_.start_time(), c.as_u32(), proc_.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()));
                }
            }
        }
    }
    out.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    out.into_iter().map(|(_, pid, cmd)| (pid, cmd)).collect()
}

/// Stop the newest process under `root` that runs `command` (its command line, or the last
/// argument of a `shell -c` one), with what it started: whether one was found. Blocking.
fn kill_command(root: u32, command: &str) -> bool {
    let want = command.trim();
    if want.is_empty() {
        return false;
    }
    let all = descendants(root);
    let found = all.iter().find(|(_, cmd)| cmd.join(" ") == want || cmd.last().is_some_and(|a| a.trim() == want));
    let Some((pid, _)) = found else { return false };
    let mut pids: Vec<u32> = vec![*pid];
    pids.extend(descendants(*pid).into_iter().map(|(p, _)| p));
    crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
    true
}

/// Wait (up to `wait`) for `pid` to be gone: whether it is.
#[allow(clippy::disallowed_methods)] // (e) polls a child process's state; its exit (pidfd, waitpid on the reactor) would tell
async fn gone(pid: u32, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    while crate::os::pid_alive(pid) {
        if Instant::now() >= end {
            return false;
        }
        crate::rt::sleep(Duration::from_millis(200)).await;
    }
    true
}

/// A take-over waits for the process it takes over to end (its terminal is being closed, or with
/// `takeover_stop` kiln ends it): an error to show when it has not, `None` once it is gone (and
/// the spec forgets it).
async fn takeover_ready(s: &Session, sink: &Sink, wait: Duration) -> Option<String> {
    let (pid, stop, kind) = {
        let sp = s.spec.lock();
        (sp.takeover_pid?, sp.takeover_stop, sp.kind.clone())
    };
    // Only the harness it was found as: never whatever has since been given its pid.
    let stop = stop && crate::aisessions::runs(pid, &kind);
    // Said in the session's state, not its timeline (which its history fills once it is free).
    let waiting = stop || crate::os::pid_alive(pid);
    if waiting {
        {
            let mut core = s.core.lock();
            core.state.doing = Some(if stop {
                format!("Ending the session it takes over (process {pid})")
            } else {
                format!("Waiting for the session it takes over (process {pid}) to end")
            });
            core.touch_state();
        }
        flush(s, sink, None).await;
    }
    // Asked to quit first (it keeps its conversation); killed, with what it started, if it won't.
    if stop && !(crate::aisessions::end(pid, false) && gone(pid, TAKEOVER_QUIT).await) {
        crate::aisessions::end(pid, true);
    }
    let free = gone(pid, wait).await;
    if waiting {
        let mut core = s.core.lock();
        core.state.doing = None;
        core.touch_state();
    }
    if free {
        let mut sp = s.spec.lock();
        sp.takeover_pid = None;
        sp.takeover_stop = false;
        return None;
    }
    Some(format!("The session found running as process {pid} has not ended, so kiln did not start a second one beside it. Quit it, then restart this agent."))
}

impl Done {
    fn apply(self, adapter: &mut super::kinds::Adapter, core: &mut Core) {
        match self {
            Self::Run { job, code, output } => adapter.run_done(core, &job, code, &output),
            Self::Reply { client, value } => core.reply(client, value),
        }
    }
}

/// Carry out what the adapter asked for. A branch starts another session; runs, reads and fetches
/// go off the event loop and come back on `done`. A restart is the caller's: its point (the
/// harness's own resume point, if any) is returned.
fn effects(s: &Arc<Session>, kind: &'static Kind, files: &Files, done: &crate::memory::queue::Sender<Done>) -> Option<Option<String>> {
    let effects = std::mem::take(&mut s.core.lock().effects);
    let mut restart = None;
    for e in effects {
        match e {
            super::core::Effect::Branch { resume, resume_at } => {
                let mut b = s.resume_spec();
                b.resume = Some(resume);
                b.resume_at = resume_at;
                b.fork = true;
                b.name = Some(format!("{} (branch)", s.info().name));
                (files.spawn)(b);
            }
            super::core::Effect::Restart { resume_at } => restart = Some(resume_at),
            // The go-back a harness without one gets from kiln (`harness::goback`): the store's
            // timeline truncates, its transcript seeds a branch, the workspace's checkpoint
            // (a hidden ref) restores its files.
            super::core::Effect::GoBack { op, event } => {
                match op {
                    super::core::GoBackOp::Fork | super::core::GoBackOp::Branch => {
                        let mut b = s.resume_spec();
                        // No native resume: the branch is seeded with the conversation in words.
                        b.resume = None;
                        b.resume_at = None;
                        b.fork = false;
                        b.name = Some(format!("{} (branch)", s.info().name));
                        b.prompt = Some(super::goback::branch_prompt(&s.core.lock(), if op == super::core::GoBackOp::Branch { Some(event.as_str()) } else { None }));
                        (files.spawn)(b);
                    }
                    _ => {
                        let mut core = s.core.lock();
                        let (at, words) = match core.get(&event) {
                            Some(e) => (e.at, match &e.body { super::model::Body::User { text, .. } => super::model::clip_middle(text.trim(), 80).0, _ => String::new() }),
                            None => continue,
                        };
                        let cwd = core.state.cwd.clone();
                        // The workspace goes back to the checkpoint the prompt's own moment made.
                        let did = if cwd.is_empty() { "the folder isn't known".to_owned() } else { super::goback::restore_files(&cwd, &s.id, at) };
                        if op == super::core::GoBackOp::RewindFiles {
                            super::goback::notice(&mut core, super::model::Level::Info, format!("Restored files to \"{words}\": {did}."));
                        } else {
                            // The process can't come back at the point natively: it starts over
                            // and the truncated conversation is said to it (the store keeps the
                            // record of what went).
                            core.truncate_from(&event);
                            let transcript = super::goback::seed_prompt(&core, None);
                            let keep: std::collections::HashSet<String> = core.events().map(|e| e.id.clone()).collect();
                            core.state.delivery.retain(|k, _| keep.contains(k));
                            core.state.harness_session = None;
                            s.spec.lock().resume = None;
                            core.state.queue.insert(0, super::model::QueueItem { id: crate::util::new_id(), text: format!("The conversation was rewound here. What it is now:\n\n{transcript}\n\nGo on from the last message."), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
                            super::goback::notice(&mut core, super::model::Level::Info, format!("Rewound to \"{words}\": {did}. The agent starts again with the conversation up to here."));
                            restart = Some(None);
                        }
                        core.touch_state();
                    }
                }
            }
            super::core::Effect::AllowBypass => s.spec.lock().allow_bypass = true,
            super::core::Effect::Run { job, args } => {
                let mut spec = s.spec.lock().clone();
                let cwd = s.core.lock().state.cwd.clone();
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd);
                }
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = match kind.cli(&spec, &args) {
                        Err(e) => (None, format!("{e:#}")),
                        Ok(cmd) => match crate::proc::output(cmd, RUN_TIMEOUT).await {
                            Ok(o) => {
                                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                                let err = String::from_utf8_lossy(&o.stderr);
                                if !err.trim().is_empty() {
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(&err);
                                }
                                (o.status.code(), super::model::clip_middle(text.trim(), RUN_OUTPUT_MAX).0)
                            }
                            Err(e) => (None, format!("{e:#}")),
                        },
                    };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::TranscriptTail { job, config, session, bytes } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let lines = crate::rt::blocking(move || super::transcript::tail(std::path::Path::new(&config), &session, bytes)).await.ok().flatten();
                    let (code, output) = match lines {
                        Some(l) => (Some(0), l.join("\n")),
                        None => (Some(1), String::new()),
                    };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Read { client, paths } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let value = crate::rt::blocking(move || read_first(&paths)).await.unwrap_or_else(|e| serde_json::json!({"error": format!("{e:#}")}));
                    let _ = tx.send(Done::Reply { client, value }).await;
                });
            }
            super::core::Effect::Own { client, ask } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let value = crate::rt::blocking(move || super::diff::own_edits(&ask, super::diff::read_now)).await.unwrap_or_else(|e| serde_json::json!({"error": format!("{e:#}")}));
                    let _ = tx.send(Done::Reply { client, value }).await;
                });
            }
            super::core::Effect::Fetch { job, url } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = crate::rt::blocking(move || fetch_loopback(&url)).await.unwrap_or_else(|e| (None, format!("{e:#}")));
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Checkpoint { at } => {
                let cwd = s.core.lock().state.cwd.clone();
                let id = s.id.clone();
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let made = if cwd.is_empty() { None } else { crate::rt::blocking(move || super::pause::snapshot(&cwd, &id, at)).await.ok().flatten() };
                    let (code, output) = match made {
                        Some((git_ref, commit)) => (Some(0), format!("{git_ref} {commit}")),
                        None => (Some(1), String::new()),
                    };
                    let _ = tx.send(Done::Run { job: format!("{}{at}", super::pause::CHECKPOINT_JOB), code, output }).await;
                });
            }
            super::core::Effect::Kill { job, command } => {
                let tx = done.clone();
                let root = *s.pid.lock();
                crate::rt::spawn(async move {
                    let killed = match root {
                        Some(root) => crate::rt::blocking(move || kill_command(root, &command)).await.unwrap_or(false),
                        None => false,
                    };
                    let (code, output) = if killed { (Some(0), String::new()) } else { (None, "it is not running any more".to_owned()) };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
        }
    }
    restart
}

/// A path with `*` components, matched against the directories there (the most recently changed
/// first); a path without them, as it is.
fn expand(path: &str) -> Vec<String> {
    let p = std::path::Path::new(path);
    let mut out: Vec<std::path::PathBuf> = vec![std::path::PathBuf::new()];
    for part in p.components() {
        if part.as_os_str() == "*" {
            let mut next = Vec::new();
            for base in &out {
                let Ok(entries) = std::fs::read_dir(base) else { continue };
                let mut dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
                    .collect();
                dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
                next.extend(dirs.into_iter().take(64).map(|(_, d)| d));
            }
            out = next;
        } else {
            for o in &mut out {
                o.push(part);
            }
        }
    }
    out.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

/// The first of these Markdown files (skills, commands) that can be read: `{"content", "path"}`,
/// else `{"error"}`. `*` stands for any directory (see `expand`). Blocking.
fn read_first(paths: &[String]) -> serde_json::Value {
    for p in paths.iter().flat_map(|p| expand(p)) {
        let p = &p;
        let path = std::path::Path::new(p);
        if !path.is_absolute() || path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() != Some("md") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > READ_MAX {
            return serde_json::json!({"error": format!("{p} is too large to show here"), "path": p});
        }
        if let Ok(bytes) = std::fs::read(path) {
            return serde_json::json!({"content": String::from_utf8_lossy(&bytes), "path": p});
        }
    }
    serde_json::json!({"error": "The file is not on this machine (any more)."})
}

/// GET a URL on this machine's loopback (a sign-in callback a harness waits for): the HTTP status
/// and the start of the answer, or None and why it failed. Nothing but loopback is fetched.
/// Blocking.
fn fetch_loopback(url: &str) -> (Option<i32>, String) {
    use std::io::{Read as _, Write as _};
    let Some((host, port, target)) = loopback_target(url) else {
        return (None, "Only an http://localhost or http://127.0.0.1 address can be completed on the agent's machine.".to_owned());
    };
    let addrs: Vec<std::net::SocketAddr> = match host.as_str() {
        "::1" => vec![std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
        "127.0.0.1" => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))],
        _ => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)), std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
    };
    let mut last = String::from("nothing is listening there");
    for addr in addrs {
        let mut conn = match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(c) => c,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(15)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let host_header = if host == "::1" { format!("[::1]:{port}") } else { format!("{host}:{port}") };
        let req = format!("GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: kiln\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        if let Err(e) = conn.write_all(req.as_bytes()) {
            last = e.to_string();
            continue;
        }
        let mut answer = Vec::new();
        let _ = conn.take(64 * 1024).read_to_end(&mut answer);
        let text = String::from_utf8_lossy(&answer);
        let code = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse::<i32>().ok());
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        return (code, super::model::clip_middle(body.trim(), 2000).0);
    }
    (None, format!("Could not reach {host}:{port} on the agent's machine: {last}"))
}

/// (host, port, request target) of an http URL on loopback; None for anything else.
fn loopback_target(url: &str) -> Option<(String, u16, String)> {
    let rest = url.trim().strip_prefix("http://")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    let target = target.split('#').next().unwrap_or("");
    let target = match target {
        "" => "/".to_owned(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_owned(),
    };
    if !target.bytes().all(|b| b.is_ascii_graphic()) || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, p) = v6.split_once(']')?;
            (h.to_owned(), p.strip_prefix(':').map_or(Some(80), |p| p.parse().ok())?)
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().ok()?),
            None => (authority.to_owned(), 80),
        },
    };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1").then_some((host, port, target))
}

/// A switch to another harness, waiting for the turn under way to end.
struct Switch {
    to: &'static Kind,
    model: String,
}

/// Nothing is under way: no turn, nothing waiting for a person.
/// Between turns: idle, nothing waiting on a person, no turn under way.
pub(super) fn settled(s: &Session) -> bool {
    settled_core(&s.core.lock())
}

fn settled_core(core: &Core) -> bool {
    core.state.status == Status::Idle && core.state.approvals.is_empty() && core.state.turn_started.is_none()
}

/// Nothing under way for a switch to another harness to wait for: settled, or its harness still
/// starting with no turn begun (one that can't sign in never gets further, and the switch is the
/// way out of it).
fn switch_now(core: &Core) -> bool {
    matches!(core.state.status, Status::Idle | Status::Starting) && core.state.approvals.is_empty() && core.state.turn_started.is_none()
}

/// A person chose a model of harness `to`: the session goes on in it once nothing is under way
/// (kiln's queue waits for it meanwhile). None: it can't (said in the timeline).
fn wanted(s: &Session, from: &'static Kind, to: &str, model: String) -> Option<Switch> {
    let target = Kind::get(to).filter(|k| k.switchable() && from.switchable());
    let installed = target.is_some_and(Kind::available);
    // Its spec before its core: never the one lock inside the other.
    let allow_bypass = s.spec.lock().allow_bypass;
    let mut core = s.core.lock();
    let Some(target) = target else {
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("A conversation can't be carried from {} to {to}.", from.name) });
        return None;
    };
    if !installed {
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("{} is not installed on this machine.", target.name) });
        return None;
    }
    core.hold = true;
    // What it will run with there, shown while the switch waits.
    let (mode, effort, modes, efforts) = carried_choices(from, target, &core.state.mode, &core.state.effort, allow_bypass, &model);
    core.state.switching = Some(super::model::Switching { kind: target.id.to_owned(), model: model.clone(), mode, effort, modes, efforts });
    core.touch_state();
    if !switch_now(&core) && !matches!(core.state.status, Status::Exited) {
        let text = format!("Switching to {} when this turn ends — the conversation carries over", in_harness(target, &model));
        core.upsert("switch-pending", None, super::model::Body::Notice { level: Level::Info, text });
    }
    Some(Switch { to: target, model })
}

/// A model's name as its harness lists it, else its id.
fn model_label(kind: &Kind, model: &str) -> String {
    super::kinds::models_of(kind.id).into_iter().find(|m| m.id == model && !m.label.is_empty()).map_or_else(|| model.to_owned(), |m| m.label)
}

/// A model in its harness, in words: "GPT-6.1-Sol (Codex)"; none chosen, "Gemini CLI (its
/// default model)".
fn in_harness(kind: &Kind, model: &str) -> String {
    if model.is_empty() { format!("{} (its default model)", kind.name) } else { format!("{} ({})", model_label(kind, model), kind.name) }
}

/// Most of a conversation carried to another harness (bytes, about): what fits a smaller model's
/// context window with room to work (≈ 160k tokens). Larger ones lose their long tool outputs'
/// middles first, then their oldest messages.
const CARRY_MAX: usize = 640 * 1024;

/// A conversation as the harness it goes on in keeps its own (`carried`).
enum Record {
    /// Claude Code: a new session's id, and its transcript's records.
    Claude { session: String, records: Vec<serde_json::Value> },
    /// Codex: a new thread's history.
    Codex(Vec<serde_json::Value>),
    /// An ACP agent or Antigravity: the conversation in words, which its new session is given
    /// with the first message it gets (`handoff::as_words`).
    Words(String),
    /// Nothing to carry: it never said anything (or a harness that takes nothing).
    Nothing,
}

/// A conversation carried from one harness to another (`carried`).
struct Carried {
    record: Record,
    /// Its permission mode and effort there: the same latitude (never more), the nearest effort.
    mode: Option<String>,
    effort: Option<String>,
    /// Older messages left out to fit.
    dropped: usize,
    /// There was nothing to carry.
    empty: bool,
}

/// The conversation so far, carried from harness `from` to `to`: read from `from`'s own record of
/// session `session` where kiln reads one (Claude Code's, Codex's), else `timeline` (kiln's, for
/// an ACP agent or Antigravity: [`timeline_of`]), fitted, and put the way `to` keeps its own (in
/// words, for one whose own record kiln doesn't write), with a note telling the model so (and
/// `more`, after it); its permission mode and effort carried over. `cwd`: where it goes on (a
/// Claude Code transcript names it); `model`: the one it goes on with (empty: the harness's
/// default). Err: `from`'s record couldn't be read.
#[allow(clippy::too_many_arguments)]
async fn carried(from: &'static Kind, to: &'static Kind, env: &[(String, String)], session: Option<String>, timeline: Option<super::handoff::Conversation>, cwd: &str, old_model: &str, mode: &str, effort: &str, allow_bypass: bool, model: &str, more: Option<&str>) -> Result<Carried, String> {
    use super::handoff;
    // What the model saw: from the harness's own record (no session yet: nothing to carry), or
    // for one whose record kiln doesn't read, kiln's timeline.
    let read = match (from.own_record(), session) {
        (false, _) => Some(timeline.unwrap_or_default()),
        (true, None) => Some(handoff::Conversation::default()),
        (true, Some(id)) => {
            let env = env.to_vec();
            crate::rt::blocking(move || match from.protocol {
                Protocol::ClaudeStreamJson => handoff::read_claude(&env, &id),
                Protocol::CodexAppServer => handoff::read_codex(&env, &id),
                Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::PiRpc | Protocol::Terminal => None,
            })
            .await
            .ok()
            .flatten()
        }
    };
    let Some(mut conv) = read else { return Err(format!("kiln could not read {}'s record of this conversation", from.name)) };
    // In words it all goes with one message: less of it.
    let words = !to.own_record();
    let dropped = handoff::fit(&mut conv, if words { handoff::WORDS_BUDGET } else { CARRY_MAX });
    let empty = conv.entries.is_empty() && conv.summary.is_none();
    let (mode, effort) = carried_settings(from, to, mode, effort, allow_bypass, model);
    let mut note = handoff::note(from.name, old_model, to.name, &conv.todos, dropped, words);
    if let Some(more) = more {
        note.push(' ');
        note.push_str(more);
    }
    let record = match to.protocol {
        _ if empty => Record::Nothing,
        Protocol::ClaudeStreamJson => {
            let id = uuid::Uuid::now_v7().to_string();
            let records = handoff::to_claude(&conv, Some(&format!("<system-reminder>\n{note}\n</system-reminder>")), &id, cwd, if old_model.is_empty() { "unknown" } else { old_model });
            Record::Claude { session: id, records }
        }
        Protocol::CodexAppServer => Record::Codex(handoff::to_codex(&conv, Some(&format!("<kiln_handoff>\n{note}\n</kiln_handoff>")))),
        // Fitted already (fitting it again would count what it left out anew).
        Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::PiRpc => Record::Words(handoff::as_words(conv, &note, usize::MAX)),
        Protocol::Terminal => Record::Nothing,
    };
    Ok(Carried { record, mode, effort, dropped, empty })
}

/// The conversation as kiln's timeline keeps it, with the plan as it stands: what is carried out
/// of a harness whose own record kiln doesn't read (`carried`). None for one whose record it does.
fn timeline_of<'a>(from: &Kind, events: impl IntoIterator<Item = &'a super::model::Event>, todos: &[super::model::Todo], queue: &[super::model::QueueItem]) -> Option<super::handoff::Conversation> {
    if from.own_record() {
        return None;
    }
    let mut c = super::handoff::from_events(handed_events(events, queue));
    c.todos = todos.to_vec();
    Some(c)
}

/// CP-T3.2: shown queued input still travels in the durable queue, and must not also become
/// past conversation in the next harness. The displayed timeline itself stays whole.
fn handed_events<'a>(events: impl IntoIterator<Item = &'a super::model::Event>, queue: &[super::model::QueueItem]) -> Vec<&'a super::model::Event> {
    let queued: std::collections::HashSet<&str> = queue.iter().map(|q| q.id.as_str()).collect();
    events.into_iter().filter(|e| match &e.body {
        super::model::Body::User { client_id, .. } => !queued.contains(e.id.as_str()) && !client_id.as_deref().is_some_and(|id| queued.contains(id)),
        _ => true,
    }).collect()
}

/// The permission mode and effort a session goes on with in harness `to`, carried from `from`: the
/// same latitude (never more; a mode that asks nothing only where `allow_bypass`) and the nearest
/// effort `model` takes there. With `to`'s choices of each, for a page to say them by their names.
fn carried_settings(from: &Kind, to: &Kind, mode: &str, effort: &str, allow_bypass: bool, model: &str) -> (Option<String>, Option<String>) {
    let (mode, effort, _, _) = carried_choices(from, to, mode, effort, allow_bypass, model);
    (mode, effort)
}

fn carried_choices(from: &Kind, to: &Kind, mode: &str, effort: &str, allow_bypass: bool, model: &str) -> (Option<String>, Option<String>, Vec<super::model::ModeOption>, Vec<super::model::ControlOption>) {
    use super::handoff;
    // An ACP agent that says its modes only once started: kiln's own, which every one takes.
    let named = match to.modes().0 {
        m if m.is_empty() && matches!(to.protocol, Protocol::Acp(_)) => super::acp::kiln_modes(),
        m => m,
    };
    let modes: Vec<super::model::ModeOption> = named.into_iter().filter(|m| !m.dangerous || allow_bypass).collect();
    let offered: Vec<String> = modes.iter().map(|m| m.id.clone()).collect();
    let mode = handoff::mode_for(from.id, mode, to.id, &offered);
    let efforts: Vec<super::model::ControlOption> = super::kinds::models_of(to.id)
        .into_iter()
        .find(|m| m.id == model)
        .map(|m| m.efforts)
        .filter(|e| !e.is_empty())
        .map(|levels| levels.iter().map(|e| super::model::ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false }).collect())
        .unwrap_or_else(|| to.efforts());
    let levels: Vec<String> = efforts.iter().map(|e| e.id.clone()).collect();
    (mode, handoff::effort_for(effort, &levels), modes, efforts)
}

/// The session's state for the harness it goes on in (`session`: its conversation's id there):
/// the model, mode and effort it runs with; what was the old harness's own (its servers, skills,
/// account, plan and limits, its to-dos, what it was doing) goes with it.
fn for_harness(st: &mut super::model::State, session: Option<String>, model: &str, mode: Option<String>, effort: Option<String>) {
    st.harness_session = session;
    model.clone_into(&mut st.model);
    st.mode = mode.unwrap_or_default();
    st.effort = effort.unwrap_or_default();
    // A speed is its harness's own (Claude Code's fast mode isn't one of Codex's tiers).
    st.speed.clear();
    st.speed_blocked = None;
    st.speed_cooldown = false;
    st.mcp.clear();
    st.skills.clear();
    st.plugins.clear();
    st.inventory = Default::default();
    st.harness_status.clear();
    st.limits.clear();
    st.account = None;
    st.plan = None;
    st.control_values.clear();
    st.objective = None;
    st.usage.context_tokens = None;
    st.usage.context_window = None;
    st.exit = None;
    st.activity = None;
    st.doing = None;
    // Its to-dos too: the new harness has them in its note ("The plan as it stands") and keeps
    // a list of its own, so keep-going never holds it to one only the old harness could change.
    st.todos.clear();
}

/// A paused session moving to another machine goes on there: the pause (for this machine's
/// account) stays behind, and so do that account's limits. True when the pause had cut a turn
/// off, which goes on there first.
fn picks_up_there(st: &mut super::model::State) -> bool {
    let Some(p) = st.pause.take() else { return false };
    st.limits.clear();
    st.stop_waived.clear();
    p.cut.is_some()
}

/// Go on in harness `sw.to` (the one the session ran in, `from`, has ended): the conversation so
/// far read from `from`'s own record (or kiln's timeline, for a harness whose record kiln doesn't
/// read) and written the way `sw.to` keeps its own (in words, for one that takes it so), the
/// permission mode and effort carried over. The session's id, timeline, changeset and queue
/// stay. Returns the harness to start and, for one that takes it once started (Codex, and in
/// words an ACP agent or Antigravity), the conversation; None: it couldn't be carried (said in
/// the timeline), and the session stays in `from`.
async fn carry_over(s: &Arc<Session>, from: &'static Kind, sw: Switch, left: usize, sink: &Sink) -> Option<(&'static Kind, Option<Vec<serde_json::Value>>)> {
    use super::handoff;
    let to = sw.to;
    let (env, allow_bypass) = {
        let sp = s.spec.lock();
        (sp.env.clone(), sp.allow_bypass)
    };
    let (cwd, session, old_model, mode, effort, timeline) = {
        let core = s.core.lock();
        let st = &core.state;
        (st.cwd.clone(), st.harness_session.clone(), st.model.clone(), st.mode.clone(), st.effort.clone(), timeline_of(from, core.events(), &st.todos, &st.queue))
    };
    let fail = |s: &Arc<Session>, text: String| {
        let mut core = s.core.lock();
        core.hold = false;
        core.state.switching = None;
        core.touch_state();
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text });
        if let Some(e) = core.get_mut("switch-pending") {
            e.body = super::model::Body::Notice { level: Level::Info, text: "Switch cancelled".to_owned() };
        }
    };
    let c = match carried(from, to, &env, session, timeline, &cwd, &old_model, &mode, &effort, allow_bypass, &sw.model, None).await {
        Ok(c) => c,
        Err(e) => {
            fail(s, format!("{e}, so it goes on in {}.", from.name));
            return None;
        }
    };
    let (dropped, empty, mode, effort) = (c.dropped, c.empty, c.mode, c.effort);
    let (resume, seed) = match c.record {
        Record::Claude { session: id, records } => {
            let (env, cwd, sid) = (env.clone(), cwd.clone(), id.clone());
            match crate::rt::blocking(move || handoff::write_claude(&env, &cwd, &sid, &records)).await {
                Ok(Ok(_)) => (Some(id), None),
                Ok(Err(e)) | Err(e) => {
                    fail(s, format!("kiln could not write the conversation for {}: {e:#}", to.name));
                    return None;
                }
            }
        }
        Record::Codex(items) => (None, Some(items)),
        // Given with the first message it gets, as a move carries it (`moving::CONTEXT`).
        Record::Words(text) => (None, Some(vec![serde_json::json!({"type": super::moving::CONTEXT, "text": text})])),
        Record::Nothing => (None, None),
    };
    {
        let mut sp = s.spec.lock();
        sp.kind = to.id.to_owned();
        // None chosen: the harness's own default.
        sp.model = Some(sw.model.clone()).filter(|m| !m.is_empty());
        sp.mode.clone_from(&mode);
        sp.effort.clone_from(&effort);
        sp.speed = None;
        sp.resume.clone_from(&resume);
        sp.resume_at = None;
        sp.fork = false;
        sp.prompt = None;
        // A program of its own was the old harness's.
        sp.program = None;
        sp.takeover_pid = None;
        sp.takeover_stop = false;
    }
    {
        let mut core = s.core.lock();
        let st = &mut core.state;
        for_harness(st, resume, &sw.model, mode, effort);
        st.status = Status::Starting;
        core.meta = super::model::Meta::default();
        named_ahead(&mut core.meta, to, Some(&sw.model));
        core.hold = false;
        core.state.switching = None;
        // A pause for the old harness's limit ends: the new one runs on its own account. The turn
        // the limit cut off goes on first.
        let unpaused = core.state.pause.take();
        if unpaused.as_ref().is_some_and(|p| p.cut.is_some()) {
            core.state.queue.insert(0, super::model::QueueItem { id: format!("{}{}", super::pause::KILN_ID, crate::util::new_id()), text: super::pause::CONTINUE_ELSEWHERE.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
        }
        let mut text = format!("Now {}", in_harness(to, &sw.model));
        if !empty {
            text.push_str(if to.own_record() { " — the conversation carried over" } else { " — the conversation carried over, in words with the next message" });
        }
        if unpaused.is_some() {
            text.push_str(&format!("; the pause for {}'s usage limit ended", from.name));
        }
        if dropped > 0 {
            text.push_str(&format!(" ({dropped} older messages left out to fit)"));
        }
        if left > 0 {
            text.push_str(&format!("; the {left} task(s) {} left running were stopped", from.name));
        }
        // `harness-switch:` marks it for viewers: a line between the turns of one harness and the next.
        core.upsert(&format!("harness-switch:{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
        core.touch_meta();
        core.touch_state();
    }
    {
        let mut info = s.info.lock();
        if info.name == from.name {
            to.name.clone_into(&mut info.name);
        }
        to.id.clone_into(&mut info.kind);
        to.name.clone_into(&mut info.kind_name);
    }
    // Everyone sees it as the harness it is now.
    for u in s.snapshot() {
        let _ = sink.send((s.id.clone(), u)).await;
    }
    tracing::info!(session = %s.id, from = from.id, to = to.id, model = %sw.model, dropped, "agent session switched harness");
    Some((to, seed))
}

/// A move to another runner, waiting for the turn under way to end.
struct Move {
    to: String,
    cwd: String,
    note: Option<String>,
    /// The turn under way was interrupted to move now: it is picked up over there.
    picks_up: bool,
    /// It goes on there in this harness (its conversation carried over), with this model.
    kind: Option<&'static Kind>,
    model: Option<String>,
    /// To this machine's newer runner process, which takes this one's place (`super::handing`).
    local: bool,
}

/// A move asked for: kiln's queue waits (what is queued goes along), and the timeline says so.
/// `now`: the turn under way isn't waited for (the caller interrupts it).
#[allow(clippy::too_many_arguments)]
fn move_wanted(s: &Session, to: String, cwd: String, note: Option<String>, now: bool, kind: Option<&'static Kind>, model: Option<String>, local: bool) -> Move {
    let mut core = s.core.lock();
    core.hold = true;
    let busy = !(core.state.status == Status::Idle && core.state.approvals.is_empty()) && !matches!(core.state.status, Status::Exited);
    let picks_up = now && busy;
    let into = kind.map(|k| format!(", to go on in {}", k.name)).unwrap_or_default();
    let text = if local {
        if busy { "This machine's newer kiln runner takes this agent over when this turn ends".to_owned() } else { "This machine's newer kiln runner takes this agent over…".to_owned() }
    } else if picks_up {
        format!("Moving to {to}{into} now: this turn stops here and goes on there")
    } else if busy {
        format!("Moving to {to}{into} when this turn ends")
    } else {
        format!("Moving to {to}{into}…")
    };
    core.upsert("move-pending", None, super::model::Body::Notice { level: Level::Info, text });
    core.touch_state();
    Move { to, cwd, note, picks_up: picks_up && !local, kind: kind.filter(|_| !local), model, local }
}

/// A person pauses it, or resumes it (`Command::Pause` without a task): its processes held stopped by
/// its machine's pacer (`crate::headroom::pause`), now or as soon as it has any, or why they can't
/// be, said in its conversation.
fn pause_session(s: &Session, on: bool) {
    let mut core = s.core.lock();
    match crate::headroom::pause(&s.id, on) {
        Ok(()) => {
            core.state.frozen = on.then(Core::now);
            core.touch_state();
        }
        Err(why) => core.upsert(&format!("pause-refused-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("Not paused: {why}.") }),
    }
}

/// A person's branch of it, or of one of its harness's own subagents (`Command::Branch`): a new
/// session on this machine (`goback::branch_spec`), or why not, said in its conversation.
fn branch(s: &Session, files: &Files, words: &str, top: bool, task: Option<&str>) {
    let own = s.resume_spec();
    let core = s.core.lock();
    let can_fork = core.meta.features.iter().any(|f| f == "resume");
    let task = match task {
        None => None,
        Some(t) => match core.state.tasks.iter().find(|x| x.subagent() && (x.id == t || x.tool_id.as_deref() == Some(t))) {
            Some(x) => Some((x.id.clone(), super::goback::thread_words(&core, x.tool_id.as_deref().unwrap_or(""), x.prompt.as_deref()))),
            None => {
                drop(core);
                s.core.lock().upsert(&format!("branch-refused-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: "Not branched: it has no such subagent.".to_owned() });
                return;
            }
        },
    };
    let conversation = if task.is_none() && !(can_fork && own.resume.is_some()) { super::goback::branch_prompt(&core, None) } else { String::new() };
    drop(core);
    let from = super::goback::BranchFrom { id: &s.id, own, can_fork, conversation, task: task.as_ref().map(|(t, w)| (t.as_str(), w.clone())) };
    (files.spawn)(super::goback::branch_spec(from, words, top));
}

/// How a move is getting on before it starts (its repository being copied there).
fn moving_note(s: &Session, text: &str) {
    s.core.lock().upsert("move-pending", None, super::model::Body::Notice { level: Level::Info, text: text.to_owned() });
}

/// How long a session leaving may take to get its folder's last changes into the sync log.
const FILES_OUT_FOR: Duration = Duration::from_secs(600);
/// How often a session leaving, or arriving, asks how its folder's copy stands.
const FILES_EVERY: Duration = Duration::from_millis(250);

/// Where a leaving session's synced folder stood once its last changes were in the log: the index
/// the machine it goes to takes the log to before it goes on there, its copy's folder here, and
/// the repo's name.
struct Synced {
    mark: crate::sync::runner::Mark,
    folder: String,
    name: String,
}

/// Before a session leaves: its folder's last changes into its repo's sync log (looked at now,
/// sent, their contents on the server, the log having brought them back), then where the copy
/// stands, for the machine it goes to to take the log that far before it goes on there. None: its
/// folder isn't a synced copy here. An error, and it doesn't leave: it would leave work behind.
#[allow(clippy::disallowed_methods)] // (e) a session's files flushed every FILES_EVERY; their writes could tell
async fn files_out(s: &Session, cwd: &str, to: &str) -> Result<Option<Synced>, String> {
    let Some(sync) = crate::sync::runner::global() else { return Ok(None) };
    let folder = std::path::Path::new(cwd);
    let started = Instant::now();
    let mut said = String::new();
    loop {
        let Some(looked) = sync.look_now(folder).await else { return Ok(None) };
        let st = sync.standing(folder).await.unwrap_or_default();
        if let Some(mark) = crate::sync::runner::left_at(&looked, &st) {
            left_behind(cwd, &st).await?;
            return Ok(Some(Synced { mark, folder: st.folder, name: st.name }));
        }
        let why = match (&st.error, st.connected) {
            (Some(e), _) => format!(" ({e})"),
            (None, false) => " (this machine isn't connected to kiln)".to_owned(),
            (None, true) => String::new(),
        };
        if started.elapsed() > FILES_OUT_FOR {
            return Err(format!("its folder's last changes didn't reach kiln in {} minutes{why}, so they'd be left behind", FILES_OUT_FOR.as_secs() / 60));
        }
        let now = match st.unsent + looked.proposed {
            0 => format!("Moving to {to}: its folder's last changes are being checked{why}"),
            n => format!("Moving to {to}: sending its folder's last changes ({n} to go){why}"),
        };
        if now != said {
            moving_note(s, &now);
            said = now;
        }
        crate::rt::sleep(FILES_EVERY).await;
    }
}

/// What a session's repository carries besides its synced files.
#[derive(Default)]
struct GitOut {
    git: Option<super::gitstate::GitState>,
    ignored: Option<super::carry::Carried>,
    worktree: Option<super::worktree::Worktree>,
}

/// Before a session leaves: its repository besides its files, less what the machine it goes to
/// has: its branches, stash and what's staged ([`super::gitstate`]), and the files git ignores
/// ([`super::carry`]); or, working in a worktree inside the synced repository, that worktree
/// ([`super::worktree`]). `copy`: the synced folder it's in; `there`: its folder over there. An
/// error, and it doesn't leave: something would be left behind.
async fn git_out(s: &Session, cwd: &str, copy: &str, to: &str, there: &str) -> Result<GitOut, String> {
    let (c, k) = (cwd.to_owned(), copy.to_owned());
    let (root, nested) = crate::rt::blocking(move || {
        let here = std::path::Path::new(&c);
        (super::gitstate::root_of(here), super::worktree::nested(here, std::path::Path::new(&k)))
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    let Some(root) = root else { return Ok(GitOut::default()) };
    moving_note(s, &format!("Moving to {to}: its repository's branches, stash, what's staged and what git ignores"));
    // Its copy over there answers for what it has: the folder it goes on in, or, for a worktree
    // that isn't there yet, the repository it's made in.
    let ask = match &nested {
        Some(super::worktree::Nested::Worktree { .. }) => {
            let depth = std::path::Path::new(cwd).strip_prefix(copy).map_or(0, |r| r.components().count());
            let mut p = std::path::PathBuf::from(there);
            for _ in 0..depth {
                p.pop();
            }
            p.to_string_lossy().into_owned()
        }
        _ => there.to_owned(),
    };
    let theirs = match their_tips(to, &ask).await {
        Some(t) => t,
        // Not told: what was pushed, it likely has.
        None => {
            let r = root.clone();
            crate::rt::blocking(move || super::gitstate::pushed(&r)).await.unwrap_or_default()
        }
    };
    let c = cwd.to_owned();
    crate::rt::blocking(move || -> Result<GitOut, String> {
        match nested {
            Some(super::worktree::Nested::Worktree { root: w, rel }) => {
                let git = super::gitstate::capture(&w, &theirs)?;
                let mut wt = super::worktree::capture(&w, &rel)?;
                wt.sub = std::path::Path::new(&c).strip_prefix(&w).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                Ok(GitOut { git: Some(git), ignored: None, worktree: Some(wt) })
            }
            Some(super::worktree::Nested::Own(p)) => Err(format!("its folder is in {}, a repository of its own inside the synced one (or a submodule), whose files sync doesn't carry: commit and push its work, and it moves", p.display())),
            None => Ok(GitOut { git: Some(super::gitstate::capture(&root, &theirs)?), ignored: Some(super::carry::ignored(&root)?), worktree: None }),
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))?
}

/// What the copy of the folder on machine `to` (at `there`) has at the tips of its refs, as it
/// says over reach. None when it can't be asked.
async fn their_tips(to: &str, there: &str) -> Option<Vec<String>> {
    use crate::reach::wire::{RunSpec, Say, Stream};
    let reach = crate::reach::node::global()?;
    reach.connect(to).await.ok()?;
    let spec = RunSpec {
        command: "git for-each-ref --format='%(objectname)' && git rev-parse -q --verify HEAD; true".into(),
        cwd: Some(there.to_owned()),
        timeout_ms: Some(30_000),
        ..RunSpec::default()
    };
    // What the run says, on a lock-free queue (`reach::node::Says`; goal 5, plan 01a1a59c2aae).
    let (tx, rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    reach.run(to, spec, tx).await.ok()?;
    let mut out = Vec::new();
    let until = Instant::now() + Duration::from_secs(40);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match crate::rt::timeout(left, rx.recv()).await {
            Ok(Ok(Say::Output { stream: Stream::Out, data, .. })) => out.extend(data),
            Ok(Ok(Say::Exit { code: Some(0), .. })) => break,
            Ok(Ok(Say::Exit { .. })) | Ok(Err(_)) | Err(_) => return None,
            Ok(Ok(_)) => {}
        }
    }
    let tips: Vec<String> = String::from_utf8_lossy(&out).lines().map(str::trim).filter(|l| l.len() >= 40 && l.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_owned).collect();
    Some(tips)
}

/// A moved session's repository put as it was on the machine it left, once its files are in:
/// what [`super::gitstate::restore`] did, in its timeline.
async fn git_in(s: &Session, sink: &Sink, files: &Files) {
    let path = super::moving::moved_git(&files.dir);
    let Ok(bytes) = std::fs::read(&path) else { return };
    let cwd = s.core.lock().state.cwd.clone();
    // The synced folder a worktree goes in: the copy its folder here is in.
    let copy = match crate::sync::runner::global() {
        Some(sync) => sync.standing(std::path::Path::new(&cwd)).await.map(|st| st.folder),
        None => None,
    };
    let said = crate::rt::blocking(move || -> Vec<String> {
        let Ok(kept) = serde_json::from_slice::<super::moving::MovedGit>(&bytes) else { return vec!["Its repository's state from the machine it left couldn't be read".into()] };
        if let Some(w) = &kept.worktree {
            let copy = copy.map(std::path::PathBuf::from).or_else(|| {
                // Without sync's word: its folder less the worktree's place in the repository.
                let up = std::path::Path::new(&w.rel).components().count() + std::path::Path::new(&w.sub).components().count();
                std::path::Path::new(&cwd).ancestors().nth(up).map(std::path::Path::to_path_buf)
            });
            let Some(copy) = copy else { return vec![format!("Its worktree couldn't be made here: no copy of its repository holds {cwd}")] };
            if let Err(e) = super::gitstate::fetch(&copy, &kept.state, &kept.from) {
                return vec![format!("Its worktree couldn't be made here ({e}); it stays on {}", kept.from)];
            }
            return match super::worktree::place(&copy, w, &kept.from) {
                Ok((at, mut said)) => {
                    said.extend(super::gitstate::restore_at(&copy, &at, &kept.state, &kept.from));
                    said
                }
                Err(e) => vec![format!("Its worktree couldn't be made here ({e}); it stays on {}", kept.from)],
            };
        }
        match super::gitstate::root_of(std::path::Path::new(&cwd)) {
            Some(root) => {
                let mut said = super::gitstate::restore(&root, &kept.state, &kept.from);
                if let Some(ig) = &kept.ignored {
                    said.extend(super::carry::place(&root, ig, &kept.from));
                }
                said
            }
            None => vec![format!("Its folder here, {cwd}, isn't a git repository, so its branches, stash and the files git ignores stay on {}", kept.from)],
        }
    })
    .await
    .unwrap_or_default();
    {
        let mut core = s.core.lock();
        for (i, text) in said.into_iter().enumerate() {
            let level = if i == 0 { Level::Info } else { Level::Warn };
            core.upsert(&format!("move-git-{}-{i}", Core::now()), None, super::model::Body::Notice { level, text });
        }
    }
    let _ = std::fs::remove_file(&path);
    flush(s, sink, None).await;
}

/// What of a session's folder sync doesn't carry, and would be left behind: then it doesn't move.
/// A repository of its own nested inside the synced one, or a submodule (sync carries the outer
/// one's files, never a nested repository's; a worktree of the same repository moves with the
/// session, [`super::worktree`]), and files too big for sync.
async fn left_behind(cwd: &str, st: &crate::sync::runner::Standing) -> Result<(), String> {
    let (c, copy) = (cwd.to_owned(), st.folder.clone());
    let nested = crate::rt::blocking(move || super::worktree::nested(std::path::Path::new(&c), std::path::Path::new(&copy))).await.ok().flatten();
    if let Some(super::worktree::Nested::Own(inner)) = nested {
        return Err(format!(
            "its folder is in {}, a repository of its own inside {} (or a submodule): sync carries {}'s files and not its, so its work would be left behind. Commit and push it, and it moves",
            inner.display(),
            st.folder,
            st.folder
        ));
    }
    let here = std::path::Path::new(cwd);
    let big: Vec<&String> = st.skipped.iter().filter(|p| std::path::Path::new(&st.folder).join(p).starts_with(here)).collect();
    if !big.is_empty() {
        let names: Vec<&str> = big.iter().take(5).map(|p| p.as_str()).collect();
        return Err(format!(
            "{} {} too big for sync to carry ({}{}), so {} be left behind: move {} out of the repository, or have git ignore {}, and it moves",
            big.len(),
            if big.len() == 1 { "file is" } else { "files are" },
            names.join(", "),
            if big.len() > 5 { ", …" } else { "" },
            if big.len() == 1 { "it would" } else { "they would" },
            if big.len() == 1 { "it" } else { "them" },
            if big.len() == 1 { "it" } else { "them" },
        ));
    }
    Ok(())
}

/// How waiting for a moved session's files ended.
#[derive(Debug, PartialEq, Eq)]
enum Arrived {
    /// They're here (or can't be waited for): it goes on.
    GoOn,
    /// It was closed meanwhile, or (`let_go`) went on elsewhere.
    End { let_go: bool },
}

/// A session that moved here goes on once its folder has taken its repo's sync log as far as the
/// machine it left had it: it never starts on files older than the ones it left. Messages sent
/// meanwhile wait in `carry`; closing it ends the wait.
#[allow(clippy::disallowed_methods)] // (e) a session's files looked at every second; their writes could tell
async fn files_in(s: &Session, sink: &Sink, mark: &crate::sync::runner::Mark, ctl_rx: &crate::memory::queue::Receiver<Ctl>, carry: &mut Vec<Command>, files: &Files) -> Arrived {
    let note = |text: String, level: Level| s.core.lock().upsert("move-files", None, super::model::Body::Notice { level, text });
    let Some(sync) = crate::sync::runner::global() else {
        note("This machine doesn't sync folders, so its folder here may not have its last changes".into(), Level::Warn);
        return Arrived::GoOn;
    };
    let cwd = s.core.lock().state.cwd.clone();
    let folder = std::path::Path::new(&cwd);
    let started = Instant::now();
    let mut said = String::new();
    loop {
        let now = match sync.standing(folder).await {
            Some(st) if crate::sync::runner::arrived(&st, mark) => {
                let waited = started.elapsed();
                note(format!("Its folder's last changes are here (in {}s); it goes on", waited.as_secs()), Level::Info);
                crate::metrics::sync().move_files_wait.observe(waited.as_secs_f64());
                tracing::Span::current().record("repo", st.name.as_str());
                // The server knows this machine as the one it arrived on.
                tell_move(crate::sync::wire::MoveNews {
                    repo: st.repo.clone(),
                    index: mark.index,
                    session: s.id.clone(),
                    phase: "arrived".into(),
                    waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                    ..crate::sync::wire::MoveNews::default()
                });
                flush(s, sink, None).await;
                return Arrived::GoOn;
            }
            Some(st) if st.repo != mark.repo => {
                note(format!("Its folder here, {cwd}, is a copy of another repository, so it goes on without waiting for its files"), Level::Warn);
                flush(s, sink, None).await;
                return Arrived::GoOn;
            }
            Some(st) => {
                let why = st.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default();
                format!("Waiting for its folder's last changes to arrive: {} to take, {} files coming{why}. It goes on once they're here", st.behind.max(1), st.waiting)
            }
            None => "Waiting for its folder's last changes to arrive: this machine's copy of it isn't syncing yet. It goes on once they're here".to_owned(),
        };
        if now != said {
            note(now.clone(), Level::Info);
            flush(s, sink, None).await;
            said = now;
        }
        let tick = crate::rt::sleep(Duration::from_secs(1)).fuse();
        let msg = ctl_rx.recv().fuse();
        futures::pin_mut!(tick, msg);
        futures::select_biased! {
            () = input_budget(s).fuse() => { if publish_input_budget(s) && promotion_pending(s) { progress_promotion(s, files).await; } flush(s,sink,None).await; },
            () = promotion_journal(s).fuse() => { progress_promotion(s, files).await; flush(s,sink,None).await; },
            m = msg => match m {
                Ok(Ctl::Cmd(c)) => match *c {
                    Command::Moving { text } => moving_note(s, &text),
                    c @ (Command::Send { .. } | Command::SkillRun { .. }) => match prepare(files, c).await { Ok(command) => carry.push(command), Err(error) => preparation_failed(s, error).await },
                    c => carry.push(c),
                },
                // No process to hand over: the next image starts it from its record.
                Ok(Ctl::HandOver(reply)) => {
                    let _ = reply.send(Ok(None));
                }
                Ok(Ctl::LetGo) => return Arrived::End { let_go: true },
                Ok(Ctl::Close) | Err(_) => return Arrived::End { let_go: false },
                // What it carries goes once it's on, asking for room then.
                Ok(Ctl::Room) => {}
                Ok(Ctl::OwnedChanged) => {flush(s,sink,None).await;}
                Ok(Ctl::Promotion) => { progress_promotion(s, files).await; flush(s,sink,None).await; }
                Ok(Ctl::ArchiveQuiet(reply)) => {let _=reply.send(false);}
                Ok(Ctl::Epoch(e)) => raise_epoch(s, files, e).await,
                // No harness of its runs here to hold a call.
                Ok(Ctl::Hook(_, reply)) => {
                    let _ = reply.send(serde_json::json!({}));
                }
            },
            () = tick => {}
        }
    }
}

/// A move that couldn't be: the session goes on here, and the timeline says why.
fn move_failed(s: &Session, mv: &Move, why: &str) {
    let mut core = s.core.lock();
    core.hold = false;
    core.touch_state();
    let text = if mv.local {
        format!("This machine's newer kiln runner couldn't take this agent over: {why} — it goes on here")
    } else {
        format!("Couldn't move to {}: {why} — it goes on here", mv.to)
    };
    core.upsert("move-pending", None, super::model::Body::Notice { level: Level::Warn, text });
}

/// Hand the session to this machine's newer runner process (its turn is over, its harness down),
/// which takes this one's place: its timeline, state and queue, by way of the server. Same disk,
/// same home: its folder and its harness's own record stay where they are, and it resumes from
/// them there under the same harness session, as it would after a restart. A harness whose own
/// record kiln doesn't keep (ACP agents, Antigravity) may not load it again: its conversation
/// goes in words, as a move carries it. Ok once that process has taken it over. Its inbox's ids go
/// with it and its inbox here is fenced first, as for a move (`ids`, `fence`; CP-T3.2): the newer
/// process keeps an inbox of its own. With no word of how it went, it waits here and asks again
/// until it's known, as a move does.
#[allow(clippy::disallowed_methods)] // (e) its hand-over's settle sleeps, passed to hand_to_newer_by
async fn hand_to_newer(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor) -> Result<(), String> {
    let id = s.id.clone();
    let send = |bytes: Vec<u8>| {
        let id = id.clone();
        async move { super::handing::send(&id, &bytes).await }
    };
    let handed = hand_to_newer_by(s, kind, mv, ids, fence, settle, send, crate::rt::sleep).await;
    super::handing::done(&s.id);
    handed
}

/// [`hand_to_newer`], each try's package sent by `send` (`super::handing::send`), the time
/// between tries waited by `wait`.
#[allow(clippy::too_many_arguments)]
async fn hand_to_newer_by<F, Fut, W, WFut>(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor, send: F, wait: W) -> Result<(), String>
where
    F: Fn(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), super::handing::NotHanded>>,
    W: Fn(Duration) -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    use super::handing::NotHanded;
    use super::moving::Carry;
    // Its inbox here fenced first, so it takes nothing more: a copy of it this process goes on with
    // takes nothing the newer one may have taken (CP-T3.2). Then the ids of the messages it took,
    // read once what its lane holds is done (a message its inbox took before the fence, and it then
    // refused, is taken back by then), and only then what it holds, copied. So no id goes without
    // its message, which a retry would find taken there and never deliver, and nothing it holds
    // stays behind (the hand-over review's point 2).
    fence(s.id.clone(), true).await.map_err(|e| format!("its inbox couldn't be fenced, so it stays here: {e}"))?;
    // The ids go with it (it takes none while it's handed over): the newer process's inbox would
    // take one sent again as new. Never without them: it stays here.
    let kept = match ids(s.id.clone()).await {
        Ok(kept) => kept,
        Err(e) => return Err(unfenced(s, fence, format!("its inbox couldn't be read, so it stays here: {e}")).await),
    };
    let spec = s.resume_spec();
    let (resume, program) = (spec.resume.clone(), spec.program.clone());
    let (events, state, cwd) = {
        let core = s.core.lock();
        // The note that it was about to go is this process's alone.
        (core.events().filter(|e| e.id != "move-pending").cloned().collect::<Vec<_>>(), core.state.clone(), core.state.cwd.clone())
    };
    let carry = match kind.protocol {
        super::kinds::Protocol::Acp(_) | super::kinds::Protocol::AgyStreamJson => {
            let history: Vec<_> = handed_events(&events, &state.queue).into_iter().cloned().collect();
            super::moving::words(&history, "This conversation went on in this machine's newer kiln runner, which took the older one's place.")
        }
        _ => Carry::Nothing,
    };
    let mut pkg = super::moving::pack(&s.id, spec, &cwd, &mv.to, &cwd, carry, events, state);
    // Signed for as it goes, by its identity at the epoch it leaves (docs/ci.md W27-R08).
    pkg.hand_on = crate::runner::custody::hand_on(&s.id, pkg.state.epoch, &pkg.events, "taken over by this machine's newer runner process");
    // Its harness's own record is here already: it goes on from it, nothing started anew.
    if matches!(pkg.carry, Carry::Nothing) {
        pkg.spec.resume = resume;
    }
    pkg.spec.program = program;
    pkg.handover = true;
    // The same try each time it's sent again: the machine decides it once.
    let attempt = crate::util::new_id();
    pkg.attempt = Some(attempt.clone());
    (pkg.inbox, pkg.inbox_digests) = super::host::split_kept(kept);
    let bytes = match serde_json::to_vec(&pkg) {
        Ok(bytes) => bytes,
        Err(e) => return Err(unfenced(s, fence, e.to_string()).await),
    };
    // How it went, until it's known (CP-T3.2, the eighteenth and nineteenth adversarial checks'):
    // the machine decides each try once, whichever of its processes comes first (`settle`), so the
    // server's word, which may be an earlier try's come late, says only when to look. Taken over,
    // it lets go; refused (or, before any try went without a word, not passed on), it takes the try
    // back unless a newer process took it first, and goes on here. With no word, or the server's
    // own after a try that went without one, it looks: while it isn't decided, the newer process
    // may run it now, so it waits (its harness down, its queue held, its inbox fenced) and its
    // package goes again. Where the machine's record isn't kept, the word alone says, as before.
    let mut pause = UNKNOWN_FIRST;
    let mut unknown = false;
    let handed = loop {
        let word = send(bytes.clone()).await;
        let take_back = match &word {
            Ok(()) => break Ok(()),
            Err(NotHanded::Refused(_)) => true,
            Err(NotHanded::NotPassed(_)) => !unknown,
            Err(NotHanded::Unknown(_)) => false,
        };
        let why = word.err().map(|e| e.to_string()).unwrap_or_default();
        let why = match settle(attempt.clone(), s.id.clone(), take_back).await {
            Ok(Some(super::host::Decided::Taken)) => break Ok(()),
            Ok(Some(super::host::Decided::Refused)) => break Err(why),
            Ok(None) if take_back => break Err(why),
            Ok(None) => why,
            Err(e) => format!("{why}; how it went can't be read here now: {e}"),
        };
        unknown = true;
        tracing::warn!(target: "kiln.sends", event = "handover_unknown", session = %s.id, error = %why, "a session's hand-over to this machine's newer runner process isn't known to have gone or not: it takes no messages here, and runs nothing, until it is; asking again");
        moving_note(s, &format!("Handing over to this machine's newer kiln runner: how it went isn't known yet ({why}), so it may run there now. It takes no messages here until it's known; asking again"));
        wait(pause).await;
        pause = (pause * 2).min(UNKNOWN_MOST);
    };
    // Refused for sure: it goes on here, taking messages again.
    if handed.is_err()
        && let Err(e) = fence(s.id.clone(), false).await
    {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session whose hand-over failed couldn't take messages again: its inbox stays fenced until it's handed over");
    }
    handed
}

/// A move whose answer never came asks the runner it went to again after this, then twice as long
/// each time, up to [`UNKNOWN_MOST`].
const UNKNOWN_FIRST: Duration = Duration::from_secs(5);
const UNKNOWN_MOST: Duration = Duration::from_secs(60);

/// Session `s`'s inbox, fenced as it began to go, takes messages again: nothing of it went, and it
/// goes on here. `why` it didn't go, as it's said.
async fn unfenced(s: &Session, fence: &FenceFor, why: String) -> String {
    if let Err(e) = fence(s.id.clone(), false).await {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session that couldn't go couldn't take messages again: its inbox stays fenced until it goes");
    }
    why
}

/// Hand the session to runner `mv.to` (its turn is over, its harness down): its record, timeline
/// and queue, over reach. Ok once that runner has taken it over and runs it. One that may have taken
/// it (all of it went, and no answer came) is asked again until it says: meanwhile the session
/// takes nothing here.
#[allow(clippy::disallowed_methods)] // (e) a hand-over's measured pause
async fn hand_over(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor) -> Result<(), String> {
    use super::moving::Carry;
    if mv.local {
        // Its process is a terminal's: it stays with the process it runs in until it ends.
        if matches!(kind.protocol, super::kinds::Protocol::Terminal) {
            return Err("an agent in a terminal stays where it runs until it ends".into());
        }
        return hand_to_newer(s, kind, mv, ids, fence, settle).await;
    }
    let reach = crate::reach::node::global().ok_or_else(|| "this runner can't reach other runners (an older one?)".to_owned())?;
    let me = reach.name().to_owned();
    let mut spec = s.resume_spec();
    // Going on there in another harness: its timeline says so (there too, as it travels with it).
    if let Some(to) = mv.kind {
        if !(kind.switchable() && to.switchable()) {
            return Err(format!("a conversation can't be carried from {} to {}", kind.name, to.name));
        }
        let model = mv.model.as_deref().filter(|m| !m.is_empty()).map_or_else(|| "its default model".to_owned(), |m| model_label(to, m));
        let text = format!("Going on in {} ({model}) on {} — the conversation carries over", to.name, mv.to);
        s.core.lock().upsert(&format!("harness-switch:move-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
    }
    let (events, mut state, from_cwd, session) = {
        let core = s.core.lock();
        // The note that it was about to move is this machine's alone.
        (core.events().filter(|e| e.id != "move-pending").cloned().collect::<Vec<_>>(), core.state.clone(), core.state.cwd.clone(), core.state.harness_session.clone())
    };
    let mut note = format!("This conversation moved from {me} (where it worked in {from_cwd}) to {} (it works in {} now).", mv.to, mv.cwd);
    if let Some(n) = &mv.note {
        note.push(' ');
        note.push_str(n);
    }
    let (carry, home) = match mv.kind {
        // Its record, and what its harness keeps for it in its home besides (`homes`).
        None => {
            let env = spec.env.clone();
            let timeline: Vec<_> = handed_events(&events, &state.queue).into_iter().cloned().collect();
            let (cwd, objective) = (from_cwd.clone(), state.objective.clone());
            crate::rt::blocking(move || {
                let carry = super::moving::record(&kind.protocol, &env, session.as_deref(), &timeline, &note)?;
                let home = super::homes::pack(kind.id, &super::homes::Dirs::here(&env), session.as_deref(), &cwd, super::moving::transcript_of(&carry), objective.as_ref());
                Ok::<_, String>((carry, Some(home)))
            })
            .await
            .map_err(|e| format!("{e:#}"))??
        }
        // The other harness's own record of it (or the conversation in words), as a switch
        // writes one (`carried`).
        Some(to) => {
            let model = mv.model.clone().unwrap_or_default();
            let timeline = timeline_of(kind, &events, &state.todos, &state.queue);
            let c = carried(kind, to, &spec.env, session, timeline, &mv.cwd, &state.model, &state.mode, &state.effort, spec.allow_bypass, &model, Some(&note)).await?;
            let (carry, there) = match c.record {
                Record::Claude { session, records } => (Carry::ClaudeLines { session: session.clone(), lines: records.iter().map(serde_json::Value::to_string).collect() }, Some(session)),
                Record::Codex(items) => (Carry::Seed { items }, None),
                Record::Words(text) => (Carry::Words { text }, None),
                Record::Nothing => (Carry::Nothing, None),
            };
            to.id.clone_into(&mut spec.kind);
            spec.model = mv.model.clone().filter(|m| !m.is_empty());
            spec.mode.clone_from(&c.mode);
            spec.effort.clone_from(&c.effort);
            spec.speed = None;
            // A program of its own was the old harness's.
            spec.program = None;
            for_harness(&mut state, there, &model, c.mode, c.effort);
            // What the old harness keeps in its home is its own: the other one doesn't read it.
            (carry, None)
        }
    };
    // Its folder's last changes go first, so it goes on there from them; then its repository's
    // state besides its files (only a synced folder's: there's a repository there to take it).
    let barrier = tracing::info_span!(target: "kiln.sync", "move.barrier", repo = tracing::field::Empty, machine = %mv.to, result = tracing::field::Empty);
    let started = Instant::now();
    let synced = files_out(s, &from_cwd, &mv.to).instrument(barrier.clone()).await;
    let waited = started.elapsed();
    barrier.record("result", if synced.is_ok() { "ok" } else { "failed" });
    if let Ok(Some(x)) = &synced {
        barrier.record("repo", x.name.as_str());
        tracing::Span::current().record("repo", x.name.as_str());
        crate::metrics::sync().move_barrier.observe(waited.as_secs_f64());
    }
    drop(barrier);
    // How this machine's part went, for the repo's activity on the server.
    let news = |repo: &str, index: u64, phase: &str, error: Option<&str>| crate::sync::wire::MoveNews {
        repo: repo.to_owned(),
        index,
        session: s.id.clone(),
        from: me.clone(),
        to: mv.to.clone(),
        phase: phase.to_owned(),
        waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        error: error.map(str::to_owned),
    };
    let synced = match synced {
        Ok(x) => x,
        Err(why) => {
            if let Some(st) = standing(&from_cwd).await {
                tell_move(news(&st.repo, st.reached, "failed", Some(&why)));
            }
            return Err(why);
        }
    };
    // Said now, before the machine it goes to says it went on there.
    if let Some(x) = &synced {
        tell_move(news(&x.mark.repo, x.mark.index, "left", None));
    }
    let out = match &synced {
        Some(x) => git_out(s, &from_cwd, &x.folder, &mv.to, &mv.cwd).await,
        None => Ok(GitOut::default()),
    };
    let out = match out {
        Ok(o) => o,
        Err(why) => {
            if let Some(x) = &synced {
                tell_move(news(&x.mark.repo, x.mark.index, "failed", Some(&why)));
            }
            return Err(why);
        }
    };
    let carried = carried_bytes(&out, home.as_ref());
    // Paused for a limit: it goes on there, where it moved for room, the cut-off turn first.
    let cut = picks_up_there(&mut state);
    let mut pkg = super::moving::pack(&s.id, spec, &mv.cwd, &me, &from_cwd, carry, events, state);
    // Signed for as it goes, by its identity at the epoch it leaves (docs/ci.md W27-R08).
    pkg.hand_on = crate::runner::custody::hand_on(&s.id, pkg.state.epoch, &pkg.events, "moved to another machine");
    if let Some(home) = home {
        pkg = pkg.with_home(home);
    }
    pkg.picks_up = mv.picks_up || cut;
    pkg.synced = synced.as_ref().map(|x| x.mark.clone());
    pkg.git = out.git;
    pkg.ignored = out.ignored;
    pkg.worktree = out.worktree;
    // Its inbox here fenced before its conversation goes: a copy of it this host goes on with
    // after a crash takes nothing for it (it may have gone on there), and is moved again
    // (CP-T3.2). Fenced before the ids of the messages it took are read, once what its lane holds
    // is done, so no id goes without its message (the hand-over review's point 2). What it holds
    // was copied above, after it stopped taking messages: none reached it since.
    fence(s.id.clone(), true).await.map_err(|e| format!("its inbox couldn't be fenced, so it stays here: {e}"))?;
    // The ids of the messages it took go with it (it takes none while it moves).
    // Never without them: a message sent again where it went would be taken there as new. It
    // stays here, and the move can be tried again.
    let kept = match ids(s.id.clone()).await {
        Ok(kept) => kept,
        Err(e) => return Err(unfenced(s, fence, format!("its inbox couldn't be read, so it stays here: {e}")).await),
    };
    (pkg.inbox, pkg.inbox_digests) = super::host::split_kept(kept);
    let bytes = match serde_json::to_vec(&pkg) {
        Ok(bytes) => bytes,
        Err(e) => return Err(unfenced(s, fence, e.to_string()).await),
    };
    moving_note(s, &format!("Moving to {}: handing over the conversation ({} KB)", mv.to, bytes.len() / 1024));
    let mut handed = reach.adopt(&mv.to, &bytes, false).await;
    // All of it went and no answer came: it may run there now. Going on here, it could take a
    // message it takes there too, so it doesn't: its inbox stays fenced, and it asks that runner
    // again until it says (it runs it: moved; it takes it now: moved; it refuses it: it stays).
    let mut pause = UNKNOWN_FIRST;
    while let Err(crate::reach::node::NotAdopted::Unknown(why)) = &handed {
        tracing::warn!(target: "kiln.sends", event = "move_unknown", session = %s.id, to = %mv.to, error = %why, "a session's move got no answer: it takes no messages here until the runner it went to says whether it took it over");
        moving_note(s, &format!("Moving to {}: no answer came ({why}), so it may run there now. It takes no messages here until {} says; asking again", mv.to, mv.to));
        crate::rt::sleep(pause).await;
        pause = (pause * 2).min(UNKNOWN_MOST);
        handed = reach.adopt(&mv.to, &bytes, true).await;
    }
    if handed.is_err()
        && let Err(e) = fence(s.id.clone(), false).await
    {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session whose move failed couldn't take messages again: its inbox stays fenced until it's moved");
    }
    let handed = handed.map_err(|e| e.to_string());
    if let (Some(x), Err(e)) = (&synced, &handed) {
        tell_move(news(&x.mark.repo, x.mark.index, "failed", Some(e)));
    }
    if handed.is_ok() {
        for (what, n) in carried {
            if n > 0 {
                crate::metrics::sync().move_carried_bytes.add(&[what], n);
            }
        }
    }
    handed
}

/// How the synced copy a folder is in stands, if this machine syncs one.
async fn standing(cwd: &str) -> Option<crate::sync::runner::Standing> {
    crate::sync::runner::global()?.standing(std::path::Path::new(cwd)).await
}

/// Tells the server how this machine's part of a move went, for the repo's activity.
fn tell_move(news: crate::sync::wire::MoveNews) {
    if let Some(sync) = crate::sync::runner::global() {
        sync.tell(crate::sync::wire::Msg::Move(news));
    }
}

/// What a leaving session carries besides its synced files, in bytes: what the machine it goes to
/// lacks of its git history, the files git ignores, its worktree, and what its harness keeps for
/// it in its home.
fn carried_bytes(out: &GitOut, home: Option<&super::homes::Home>) -> [(&'static str, u64); 4] {
    // Base64, as the package carries them: three bytes in four.
    let b64 = |s: &str| u64::try_from(s.len() / 4 * 3).unwrap_or(u64::MAX);
    let files = |c: &super::carry::Carried| c.files.iter().map(|f| b64(&f.data)).sum::<u64>();
    [
        ("git", out.git.as_ref().and_then(|g| g.bundle.as_deref()).map_or(0, b64)),
        ("ignored", out.ignored.as_ref().map_or(0, files)),
        ("worktree", out.worktree.as_ref().map_or(0, |w| files(&w.changed).saturating_add(files(&w.ignored)))),
        ("home", home.map_or(0, super::homes::Home::size)),
    ]
}

/// The session's life: start the process, pump it, and after it exits wait to be restarted.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::disallowed_methods)] // (e) a session's next due time; a picked 1.5 s look
async fn drive(s: Arc<Session>, kind: &'static Kind, ctl_rx: crate::memory::queue::Receiver<Ctl>, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, carried: Option<Vec<serde_json::Value>>, adopt: Option<Adopt>) {
    // The harness it runs in (a switch changes it); what it says is marked as its.
    let mut kind = kind;
    s.core.lock().harness = Some(kind.name.to_owned());
    // Its previews' and its effects' results, each back from the task that made it: lock-free queues,
    // unbounded as the channels they were (goal 5, plan 01a1a59c2aae), each as many as the session asked.
    let (pv_tx, pv_rx) = crate::memory::queue::unbounded::<(String, Vec<super::model::FileDiff>)>(&crate::memory::owners::CHANNELS);
    let mut changes = futures::stream::FuturesUnordered::new();
    let (fx_tx, fx_rx) = crate::memory::queue::unbounded::<Done>(&crate::memory::owners::CHANNELS);
    // Going on with the process the previous image of this process ran: its timeline came with it.
    let mut adopt = adopt;
    let handed = adopt.is_some();
    if handed {
        s.prompted.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // A new session (nothing in its timeline yet): the commit its folder has checked out as it
    // begins, the commits after it its own (the Changes tab's Commits). One that came back (a
    // restart, a move) has it, or is from before kiln kept it.
    let new = {
        let core = s.core.lock();
        (core.events().next().is_none() && core.state.started_head.is_none()).then(|| core.state.cwd.clone())
    };
    if let Some(cwd) = new.filter(|_| !handed)
        && let Some(head) = crate::workspace::git::head(std::path::Path::new(&cwd)).await
    {
        let mut core = s.core.lock();
        core.state.started_head = Some(head);
        core.touch_state();
    }
    // Taking over a session: its transcript is read once it has ended (checked again before each
    // start while it has not).
    if !handed {
        let _ = takeover_ready(&s, &sink, TAKEOVER_WAIT).await;
    }
    // A resumed Claude Code session starts from its transcript (its history), not empty.
    if kind.protocol == super::kinds::Protocol::ClaudeStreamJson && !handed {
        let (resume, env, upto) = {
            let sp = s.spec.lock();
            (sp.resume.clone(), sp.env.clone(), sp.resume_at.clone())
        };
        let empty = s.core.lock().events().next().is_none();
        if let Some(r) = resume.filter(|_| empty) {
            let session = r.clone();
            match crate::rt::blocking(move || super::transcript::history(&env, &r)).await {
                Ok(Some(lines)) => {
                    let mut core = s.core.lock();
                    super::transcript::load_upto(&mut core, &lines, upto.as_deref());
                    tracing::info!(id = %s.id, %session, events = core.events().count(), "agent session history loaded from its transcript");
                }
                _ => tracing::info!(id = %s.id, %session, "no transcript found for the resumed agent session"),
            }
        }
    }
    // Paused by a person before this runner started (its pacer's guard started its processes again
    // as the last runner ended): held again, or said to run if this machine can't hold it.
    let frozen = s.core.lock().state.frozen.is_some();
    if frozen && let Err(why) = crate::headroom::pause(&s.id, true) {
        let mut core = s.core.lock();
        core.state.frozen = None;
        core.touch_state();
        core.upsert(&format!("pause-refused-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("No longer paused: {why}.") });
    }
    // Announce it, whole, before anything else: whoever mirrors it starts from all of it. One
    // that started again from the conversation store announces all but its timeline: the store
    // has that, and the server's copy gets what it lacks as the session's stream starts.
    let stored = std::mem::take(&mut s.core.lock().stored);
    for u in s.snapshot() {
        if stored && matches!(u, Update::Reset { .. } | Update::Events { .. }) {
            continue;
        }
        let _ = sink.send((s.id.clone(), u)).await;
    }
    let mut first = !handed;
    // Its process ending never ends it: how many starts in a row ended soon after, the wait before
    // its next start, and when that is.
    let mut soon: u32 = 0;
    let mut again = AGAIN_FIRST;
    let mut again_at: Option<Instant>;
    // A message that arrived while the process was down, sent once it is back.
    let mut carry: Vec<Command> = Vec::new();
    // Skill runs sent while there was no room on its machine for it to go to work: run once there
    // is (a message waits in its queue instead; `super::limit`).
    let mut for_room: Vec<Command> = Vec::new();
    // What its machine was last told of how it stands for its agent limit, and whether anything
    // waits to go to its harness.
    let mut stood: Option<(Work, bool)> = None;
    // Down, it starts again once there's room on its machine (a person restarted it, or it came
    // back by itself after a crash).
    let mut restart_wanted = false;
    // Moved here: on once its folder's last changes are (after a restart too: its state says so).
    let arriving = s.core.lock().state.arriving.clone();
    if let Some(mark) = arriving {
        let span = tracing::info_span!(target: "kiln.sync", "move.files", repo = tracing::field::Empty, session = %s.id, result = tracing::field::Empty);
        let arrived = files_in(&s, &sink, &mark, &ctl_rx, &mut carry, &files).instrument(span.clone()).await;
        span.record("result", if arrived == Arrived::GoOn { "ok" } else { "ended" });
        drop(span);
        if let Arrived::End { let_go } = arrived {
            if let_go {
                (files.release)(&s.id);
            } else {
                let _ = sink.send((s.id.clone(), Update::Closed)).await;
            }
            on_change();
            return;
        }
        {
            let mut core = s.core.lock();
            core.state.arriving = None;
            core.touch_state();
        }
        // Its files in: its repository as it was there.
        git_in(&s, &sink, &files).await;
    }
    // A rewind: the harness starts again, continuing its session from this point.
    let mut rewind_to: Option<Option<String>> = None;
    // A switch to another harness, waiting for the turn under way to end; and the conversation
    // carried over, for a harness that takes it once started (Codex), until it has.
    let mut switching: Option<Switch> = None;
    // A session that moved here with a new thread's history starts with it.
    let mut seed: Option<Vec<serde_json::Value>> = carried;
    // A move to another runner, waiting for the turn under way to end.
    let mut moving: Option<Move> = None;
    // A setting its harness takes when it starts (where its commands run) changed: it starts
    // again once nothing is under way.
    let mut reloading = false;
    // Its harness ran here, a process of its own: one that never did has no group of processes
    // here to end when it lets go.
    let mut ran_here = false;
    // Tasks the harness left running that a switch stopped.
    let mut left = 0usize;
    // The models it last listed (the machine keeps the list for its "new agent" and switches): not
    // those kiln named ahead of its harness (`named_ahead`), which are the machine's list already.
    let mut models_seen = s.core.lock().meta.models.clone();
    loop {
        // The harness it ran in is down: on in the other one.
        // Its turn is over: on to the other runner (or, if that can't be, on here).
        if let Some(mv) = moving.take() {
            // From here on it takes no messages (they go where it goes); those it was sent and
            // hasn't taken go with it in its queue, and the rest it was told waits for the move.
            let held = gather(&s, &ctl_rx, &mut carry);
            let span = tracing::info_span!(target: "kiln.sync", "move", repo = tracing::field::Empty, machine = %mv.to, session = %s.id, result = tracing::field::Empty);
            let handed = hand_over(&s, kind, &mv, &files.carried, &files.fence, &files.settle).instrument(span.clone()).await;
            let result = if handed.is_ok() { "ok" } else { "failed" };
            span.record("result", result);
            drop(span);
            crate::metrics::sync().moves.inc(&[result]);
            match handed {
                Ok(()) => {
                    tracing::info!(session = %s.id, to = %mv.to, "agent session moved");
                    (files.release)(&s.id);
                    return;
                }
                Err(why) => {
                    tracing::warn!(session = %s.id, to = %mv.to, %why, "agent session could not move");
                    // It stays: it takes messages again, and what it was told meanwhile, in order.
                    *s.moving.lock() = false;
                    for c in held {
                        let _ = s.ctl.try_send(c);
                    }
                    move_failed(&s, &mv, &why);
                    flush(&s, &sink, None).await;
                    on_change();
                }
            }
            first = false;
        }
        // Started again for a setting (where its commands run): what waited in kiln's queue goes
        // to the new process.
        if std::mem::take(&mut reloading) {
            s.core.lock().hold = false;
            first = false;
        }
        if let Some(sw) = switching.take() {
            if let Some((to, carried)) = carry_over(&s, kind, sw, std::mem::take(&mut left), &sink).await {
                kind = to;
                seed = carried;
                *s.carried.lock() = seed.clone();
                models_seen = s.core.lock().meta.models.clone();
            }
            first = false;
            on_change();
        }
        let mut spec = if first { s.spec.lock().clone() } else { s.resume_spec() };
        if let Some(at) = rewind_to.take() {
            spec.resume_at = at;
        }
        // A start that takes a carried conversation is the harness's new session, not an old one.
        if seed.is_some() {
            spec.resume = None;
        }
        s.core.lock().harness = Some(kind.name.to_owned());
        // The process the previous image handed over, still running; else a new one.
        let (mut adapter, spawned, partial, adopted) = match adopt.take() {
            Some(a) => match crate::proc::adopt_duplex(a.process) {
                Ok(p) => {
                    tracing::info!(session = %s.id, pid = p.pid, "agent session went on with its process across the upgrade");
                    (a.adapter, Ok(p), a.partial, true)
                }
                Err(e) => {
                    // Its process can't be reached: stopped, and started again (its conversation
                    // goes on), so two of it never run.
                    tracing::warn!(session = %s.id, error = %format!("{e:#}"), "couldn't take over the agent's process after the upgrade: it starts again");
                    crate::proc::terminate_tree(i32::try_from(a.process.pid).unwrap_or(0), Duration::from_secs(2)).await;
                    // What its adapter held for it goes to the new one.
                    let mut gone = a.adapter;
                    gone.let_go(&mut s.core.lock());
                    continue;
                }
            },
            None => {
                let mut adapter = kind.adapter(&spec);
                adapter.offer_mcp(super::kinds::acp_servers(&spec, &s.id));
                if let Some(items) = &seed {
                    adapter.seed(items.clone());
                }
                let blocked = if spec.takeover_pid.is_some() { takeover_ready(&s, &sink, Duration::from_secs(3)).await } else { None };
                // One the machine's own PATH lacks may be on its user's: their login shell is asked
                // first (once, then kept ten minutes).
                if blocked.is_none() && spec.program.is_none() && !kind.available() {
                    crate::reach::serve::login_env().await;
                }
                // What it needs before it starts (kiln's MCP server in Antigravity's own config).
                if blocked.is_none() {
                    kind.prepare(&spec).await;
                }
                let spawned = match blocked {
                    Some(why) => Err(anyhow::anyhow!(why)),
                    // In the session's group of processes where the machine has them, and marked as
                    // its, and so is everything it starts (`survivors`).
                    None => match kind.launch(&spec, &s.id) {
                        Ok(mut p) => {
                            p.env(super::survivors::MARK, &s.id);
                            // Its git signs as the agent, through this runner (W27-R03), where the
                            // server issues identities, and pushes to kiln as the agent where the
                            // server gives it a credential of its own; the person's own
                            // configuration, key and credential never.
                            let (sign, kiln) = (crate::runner::identity::enabled(), crate::runner::identity::kiln_git());
                            if sign || !kiln.is_empty() {
                                match crate::gitsign::installed() {
                                    Ok(program) => {
                                        let had = std::env::var("GIT_CONFIG_COUNT").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
                                        for (k, v) in crate::gitsign::git_env(&program, sign.then_some(crate::gitsign::AGENT_KEY), &kiln, had) {
                                            p.env(k, v);
                                        }
                                    }
                                    Err(e) => tracing::warn!(session = %s.id, error = %format!("{e:#}"), "its git won't sign or push as the agent"),
                                }
                            }
                            // Off the cores of a server beside it, and so is everything it starts
                            // (docs/priority.md, "Legs and agents keep off the server's cores").
                            crate::os::onto_work_cpus(&mut p);
                            crate::proc::spawn_duplex_in_pool(p, crate::os::contain::Group::of(&s.id)).await
                        }
                        Err(e) => Err(e),
                    },
                };
                (adapter, spawned, Vec::new(), false)
            }
        };
        let started = Instant::now();
        again_at = None;
        let launched = spawned.is_ok();
        let mut code = None;
        let mut error = None;
        match spawned {
            Err(e) => error = Some(format!("{e:#}")),
            Ok(mut p) => {
                *s.pid.lock() = Some(p.pid);
                ran_here = true;
                if adopted {
                    let mut core = s.core.lock();
                    // An older image may have left what the harness before a switch ran looking live.
                    core.settle_foreign();
                    adapter.upgraded(&mut core);
                    for c in carry.drain(..) {
                        adapter.command(&mut core, c);
                    }
                    drop(core);
                    if kind.protocol != super::kinds::Protocol::CodexAppServer {
                        read_unread_children(&s);
                    }
                } else {
                    let mut core = s.core.lock();
                    core.state.exit = None;
                    core.set_status(Status::Starting);
                    core.touch_state();
                    adapter.start(&mut core);
                    if !s.prompted.swap(true, std::sync::atomic::Ordering::Relaxed) && let Some(prompt) = spec.prompt.clone().filter(|p| !p.trim().is_empty()) {
                        // Its id is the session's own (CP-T3.2): made once, the same if it's ever made again.
                        let id = crate::util::derive_id(&["first-prompt", &s.id]);
                        adapter.command(&mut core, Command::Send { id, text: prompt, mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None });
                    }
                    for c in carry.drain(..) {
                        adapter.command(&mut core, c);
                    }
                    drop(core);
                    if kind.protocol != super::kinds::Protocol::CodexAppServer {
                        read_unread_children(&s);
                    }
                }
                on_change();
                let mut jobs = flush(&s, &sink, Some(&p.stdin)).await;
                let mut buf: Vec<u8> = partial;
                let mut stderr_tail: Vec<u8> = Vec::new();
                let mut out_open = true;
                let mut err_open = true;
                let mut last_session = s.core.lock().state.harness_session.clone();
                let mut last_queue = 0usize;
                let mut last_pause = pause_marks(&s.core.lock().state);
                let mut next_tick = Instant::now() + TICK;
                // It is being ended for a switch (it is idle).
                let mut ending = false;
                // Stopped by force (`Command::ForceStop`): it starts again now, whatever is under way.
                let mut forcing = false;
                // Where this machine keeps what's an agent's own: whose each file it changes is.
                let roots = std::sync::Arc::new(super::whose::Roots::here(&spec.env));
                loop {
                    for job in jobs.previews.drain(..) {
                        let tx = pv_tx.clone();
                        crate::rt::spawn(async move {
                            let id = job.approval.clone();
                            let diffs = crate::rt::blocking(move || super::claude::preview(&job.tool, &job.input, &job.cwd)).await.unwrap_or_default();
                            let _ = tx.send((id, diffs)).await;
                        });
                    }
                    for job in jobs.changes.drain(..) {
                        // Placed among its folder's repository's working trees (a worktree it went into),
                        // and said whose it is: this project's, the person's or an agent's own.
                        let (home, folders) = {
                            let mut core = s.core.lock();
                            core.start_change(&job.abs);
                            let cwd = core.state.cwd.clone();
                            core.note_folder(&cwd);
                            (cwd, core.folders().to_vec())
                        };
                        let roots = roots.clone();
                        changes.push(async move {
                            let abs = job.abs.clone();
                            let change = crate::rt::blocking(move || super::diff::placed_change(&job, &home, &folders, &roots)).await.unwrap_or_default();
                            (abs, change)
                        });
                    }
                    let ev = futures::select_biased! {
                        c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                        () = input_budget(&s).fuse() => Ev::InputBudget,
                        () = promotion_journal(&s).fuse() => Ev::PromotionJournal,
                        c = changes.select_next_some() => Ev::Change(c),
                        o = pipe(&p.stdout, out_open).fuse() => Ev::Out(o),
                        e = pipe(&p.stderr, err_open).fuse() => Ev::Err(e),
                        d = pv_rx.recv().fuse() => Ev::Preview(d.ok()),
                        f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
                        x = (&mut p.exit).fuse() => Ev::Exit(x.ok().flatten()),
                        _ = crate::rt::sleep_until(next_tick).fuse() => Ev::Tick,
                        () = held_room(&s.core).fuse() => Ev::Room,
                    };
                    match ev {
                        Ev::Out(Some(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            let mut core = s.core.lock();
                            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                adapter.line(&mut core, &String::from_utf8_lossy(&line));
                            }
                            // At its stop point in the middle of a turn: it stops at its next step.
                            if super::pause::stop_mid_turn(&mut core) {
                                adapter.command(&mut core, Command::Interrupt);
                            }
                            if buf.len() > 32 << 20 {
                                tracing::warn!(session = %s.id, "dropping an oversized line from the harness");
                                buf.clear();
                            }
                        }
                        Ev::Out(None) => {
                            out_open = false;
                            if !err_open {
                                // Both pipes closed: the exit status follows.
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Err(Some(chunk)) => {
                            stderr_tail.extend_from_slice(&chunk);
                            let excess = stderr_tail.len().saturating_sub(8192);
                            stderr_tail.drain(..excess);
                        }
                        Ev::Err(None) => {
                            err_open = false;
                            if !out_open {
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Ctl(Some(Ctl::Cmd(c))) => {
                            let prepared = journal_edit(&s, &files, *c, |core, command| adapter.command(core, command)).await;
                            match prepared {
                            None => {},
                            Some(command) => match command {
                            Command::StateRead { client, read } => read_state(&mut s.core.lock(), client, read),
                            Command::MailHandoff { client, revision, handoff, turn, ids, response_digest } => register_handoff(&s, client, revision, handoff, turn, ids, response_digest).await,
                            Command::AtState { client, revision, command } => {
                                at_state(&mut s.core.lock(), client, &revision, *command, |core, command| adapter.command(core, command));
                            }
                            Command::Restart => {
                                if s.core.lock().state.status == Status::Exited {
                                    rewind_to = Some(None);
                                }
                            }
                            Command::ForceStop => {
                                // You ended the turn: a restart before it's back doesn't take it up again.
                                s.turn_open.store(false, std::sync::atomic::Ordering::Relaxed);
                                let text = format!("Stopped by force: {} starts again, the conversation going on; what it was running stopped with it", kind.name);
                                s.core.lock().upsert(&format!("force-stop:{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                                forcing = true;
                            }
                            Command::Snapshot => {
                                for u in s.snapshot() {
                                    let _ = sink.send((s.id.clone(), u)).await;
                                }
                            }
                            // What one who changed its files did alone: the session keeps that, whatever its harness.
                            Command::OwnEdits { client, by } => s.core.lock().ask_own(client, &by),
                            // A subagent of its harness's that ended, removed: the session's own, whatever its harness.
                            Command::RemoveTask { task } => {
                                if s.core.lock().remove_task(&task) {
                                    on_change();
                                }
                            }
                            Command::Control { id, value } if id == "rename" => {
                                if let Some(c) = name_session(&s, value.as_ref()) {
                                    adapter.command(&mut s.core.lock(), c);
                                }
                                on_change();
                            }
                            // Where its commands run: its harness takes that when it starts, so it
                            // starts again (the conversation goes on) once nothing is under way.
                            Command::Control { id, value } if id == "reach" => {
                                if set_reach(&s, value.as_ref()) {
                                    let mut core = s.core.lock();
                                    let when = if settled_core(&core) { "now" } else { "once this turn ends" };
                                    let text = format!("{} — {} starts again with that {when}; the conversation goes on", reach_words(core.state.reach.as_deref()), kind.name);
                                    core.upsert(&format!("harness-switch:reach-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                                    core.hold = true;
                                    drop(core);
                                    reloading = true;
                                }
                                on_change();
                            }
                            Command::SetModel { model, kind: Some(to) } if to != kind.id => {
                                if let Some(sw) = wanted(&s, kind, &to, model) {
                                    switching = Some(sw);
                                }
                            }
                            Command::MoveTo { to, cwd, note, now, kind: into, model, local } => {
                                let mv = move_wanted(&s, to, cwd, note, now, into.as_deref().and_then(Kind::get).filter(|k| k.id != kind.id), model, local);
                                // Not waiting for the turn: it stops here, and goes on there.
                                if mv.picks_up {
                                    adapter.command(&mut s.core.lock(), Command::Interrupt);
                                }
                                moving = Some(mv);
                            }
                            Command::Moving { text } => moving_note(&s, &text),
                            // What it left running that kiln took back over: kiln stops it.
                            Command::StopTask { task } if task.starts_with(crate::reach::owned::PREFIX) => {
                                s.stop_owned_reach(Some(&task));
                            }
                            Command::StopTask { task } if task.starts_with(super::survivors::LEFT) => {
                                stop_left(&s, Some(&task)).await;
                                on_change();
                            }
                            Command::StopTasks => {
                                s.stop_owned_reach(None);
                                stop_left(&s, None).await;
                                adapter.command(&mut s.core.lock(), Command::StopTasks);
                                on_change();
                            }
                            // A pause (`harness::pause`): ended by a person, its timer set, its stop
                            // point set, or one started because another session on the account
                            // reached the limit.
                            Command::Resume => {
                                let mut core = s.core.lock();
                                // Going on regardless: the stop point leaves the windows past it be.
                                super::pause::waive_stop_point(&mut core);
                                if let Some(p) = super::pause::release(&mut core, "Resumed by you.") {
                                    adapter.go_on(&mut core, p);
                                }
                                drop(core);
                                on_change();
                            }
                            Command::AutoResume { on } => {
                                let mut core = s.core.lock();
                                core.state.manual_resume = !on;
                                if let Some(p) = core.state.pause.as_mut() {
                                    p.auto = on;
                                }
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::PauseAt { percent } => {
                                let mut core = s.core.lock();
                                core.state.pause_at = percent.filter(|p| (1..=100).contains(p));
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            // A person's pause of the whole of it: its processes, held where they are.
                            Command::Pause { on, task: None } => {
                                pause_session(&s, on);
                                on_change();
                            }
                            // A person's branch: a new session beside it, or of its own.
                            Command::Branch { words, top, task } => {
                                branch(&s, &files, &words, top, task.as_deref());
                                on_change();
                            }
                            // What a person lets the whole of it do (the server narrowed it to its parent's).
                            Command::Perms { perms, task: None } => {
                                let mut core = s.core.lock();
                                core.state.perms = perms;
                                core.touch_state();
                                adapter.perms_changed(&mut core);
                                drop(core);
                                on_change();
                            }
                            Command::MaxSubagents { count } => {
                                let mut core = s.core.lock();
                                core.state.max_subagents = count.filter(|n| *n >= 1);
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::MoveOnLimit { on, to, kind, model } => {
                                let mut core = s.core.lock();
                                core.state.move_on_limit = on;
                                core.state.move_plan = if on { super::model::MovePlan { to, kind, model } } else { super::model::MovePlan::default() };
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::LimitHold { window, until, said, account, .. } => {
                                let mut core = s.core.lock();
                                super::pause::account_hold(&mut core, super::pause::Hit { window, until, said }, account);
                                drop(core);
                                on_change();
                            }
                            c => {
                                // A model of its own harness: no switch after all.
                                if matches!(c, Command::SetModel { .. }) && switching.take().is_some() {
                                    let mut core = s.core.lock();
                                    core.hold = false;
                                    core.state.switching = None;
                                    core.touch_state();
                                    if let Some(e) = core.get_mut("switch-pending") {
                                        e.body = super::model::Body::Notice { level: Level::Info, text: "Switch cancelled".to_owned() };
                                    }
                                }
                                let c = match prepare(&files, c).await { Ok(command) => command, Err(error) => { preparation_failed(&s, error).await; continue; } };
                                let mut core = s.core.lock();
                                // No room on its machine for it to go to work: a skill run waits
                                // for room (`super::limit`).
                                if matches!(c, Command::SkillRun { .. }) && !core.has_room() {
                                    for_room.push(c);
                                }
                                // Paused (or at its stop point), or no room on its machine for it
                                // to go to work: a message waits in the queue.
                                else if let Some(c) = super::pause::let_through(&mut core, c) {
                                    adapter.command(&mut core, c);
                                }
                            }
                        }
                        }
                        },
                        // There may be room on its machine now: the skill runs that waited for it
                        // go, then its queue, if it may go to work now.
                        Ev::Ctl(Some(Ctl::Room)) => {
                            let mut core = s.core.lock();
                            if !for_room.is_empty() && core.has_room() {
                                for c in for_room.drain(..) {
                                    adapter.command(&mut core, c);
                                }
                            }
                            adapter.room(&mut core);
                        }
                        Ev::Ctl(Some(Ctl::OwnedChanged)) => {},
                        Ev::Ctl(Some(Ctl::Promotion)) => progress_promotion(&s, &files).await,
                        Ev::InputBudget => { if publish_input_budget(&s) && promotion_pending(&s) { progress_promotion(&s, &files).await; } },
                        Ev::PromotionJournal => progress_promotion(&s, &files).await,
                        Ev::Ctl(Some(Ctl::ArchiveQuiet(reply))) => {let _=reply.send(s.archive_quiet_now(!carry.is_empty() || !for_room.is_empty() || moving.is_some() || switching.is_some() || reloading));},
                        Ev::Ctl(Some(Ctl::Epoch(e))) => raise_epoch(&s, &files, e).await,
                        // Its harness's hook asks before a tool call: the adapter answers, at once or
                        // once a person lets the call go.
                        Ev::Ctl(Some(Ctl::Hook(input, reply))) => adapter.hook(&mut s.core.lock(), input, reply),
                        Ev::Ctl(Some(Ctl::HandOver(reply))) => {
                            // This image of the process ends next (an in-place upgrade): the next
                            // one goes on with this very process. What it has said so far is taken
                            // in here, and nothing more is read, so nothing it says is lost.
                            let _ = flush(&s, &sink, Some(&p.stdin)).await;
                            let unresolved = { let core = s.core.lock(); !core.input_stages.is_empty() || !core.input_retained.is_empty() || !core.input_receipts.is_empty() };
                            if unresolved {
                                let _ = reply.send(Err("source input journal effects are not committed; the current owner is retained".into()));
                                continue;
                            }
                            // A switch, move or setting waiting for the turn to end is this image's
                            // alone: such a session starts again in the next one instead.
                            let waiting = switching.is_some() || moving.is_some() || reloading;
                            let detached = if waiting { None } else { p.detach().await };
                            let Some(process) = detached else {
                                // It can't be handed over: stopped, so the next image starts it again
                                // (its conversation goes on) without two of it running.
                                p.terminate(Duration::from_secs(2));
                                let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                                *s.pid.lock() = None;
                                // What its adapter held goes with its record, in its queue.
                                adapter.let_go(&mut s.core.lock());
                                let _ = reply.send(Ok(None));
                                return;
                            };
                            while let Ok(Some(chunk)) = crate::rt::timeout(Duration::from_secs(1), p.stdout.recv()).await.map(Result::ok) {
                                buf.extend_from_slice(&chunk);
                            }
                            {
                                let mut core = s.core.lock();
                                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                    let line: Vec<u8> = buf.drain(..=nl).collect();
                                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                                }
                            }
                            let _ = flush_input(&s).await;
                            let unresolved = { let core = s.core.lock(); !core.input_stages.is_empty() || !core.input_retained.is_empty() || !core.input_receipts.is_empty() };
                            if unresolved {
                                let _ = reply.send(Err("trailing native input effects could not commit; the current owner is retained".into()));
                                match crate::proc::adopt_duplex(process) {
                                    Ok(resumed) => { p = resumed; out_open = true; err_open = true; continue; }
                                    Err(error) => {
                                        crate::proc::terminate_tree(i32::try_from(process.pid).unwrap_or(0), Duration::from_secs(2)).await;
                                        let mut core = s.core.lock();
                                        adapter.let_go(&mut core);
                                        core.upsert("input-handover-retained", None, super::model::Body::Notice { level: Level::Error, text: format!("Source handover was refused; accepted communications remain retained. The provider must restart: {error:#}") });
                                        ending = true;
                                        break;
                                    }
                                }
                            }
                            let (updates, core) = { let mut core = s.core.lock(); (core.take_out(), core.freeze()) };
                            for u in updates {
                                let _ = sink.send((s.id.clone(), u)).await;
                            }
                            *s.pid.lock() = None;
                            use base64::Engine as _;
                            let partial = base64::engine::general_purpose::STANDARD.encode(&buf);
                            let _ = reply.send(Ok(Some(Box::new(Handed { kind: kind.id.to_owned(), process, core, adapter: adapter.freeze(), partial }))));
                            return;
                        }
                        Ev::Ctl(ctl @ (Some(Ctl::Close | Ctl::LetGo) | None)) => {
                            s.stop_owned_reach(None);
                            // Ending the agent ends what it left running: stopped through the
                            // harness first, then whatever is still under it (shells and
                            // terminals that left its process group included), then the harness.
                            // Letting go of it (it went on elsewhere) ends what ran here alike.
                            if s.core.lock().state.tasks.iter().any(super::model::Task::live) {
                                adapter.command(&mut s.core.lock(), Command::StopTasks);
                                let _ = flush(&s, &sink, Some(&p.stdin)).await;
                                crate::rt::sleep(Duration::from_millis(1500)).await;
                            }
                            let root = p.pid;
                            let left = std::mem::take(&mut *s.left.lock());
                            let group = crate::os::contain::Group::existing(&s.id);
                            let members = group.clone();
                            let _ = crate::rt::blocking(move || {
                                // Everything in its group where it has one (what left its tree
                                // too), else what's under it now.
                                let pids: Vec<u32> = match &members {
                                    Some(g) => g.pids().into_iter().filter(|p| *p != root).collect(),
                                    None => descendants(root).into_iter().map(|(pid, _)| pid).collect(),
                                };
                                crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
                                for r in &left {
                                    super::survivors::stop_tree(r);
                                }
                            })
                            .await;
                            p.terminate(Duration::from_secs(3));
                            *s.pid.lock() = None;
                            if let Some(g) = group {
                                end_group(g, Duration::from_millis(3500));
                            }
                            if matches!(ctl, Some(Ctl::LetGo)) {
                                tracing::info!(session = %s.id, "agent session went on on another machine; this one lets go of it");
                                (files.release)(&s.id);
                            } else {
                                // Its last state, what an ended agent shows, says all of it stopped
                                // (the harness's own word on it was never read).
                                {
                                    let mut core = s.core.lock();
                                    adapter.exited(&mut core, None, None);
                                    end_tasks(&mut core);
                                }
                                let _ = flush(&s, &sink, None).await;
                                let _ = sink.send((s.id.clone(), Update::Closed)).await;
                            }
                            on_change();
                            return;
                        }
                        Ev::Preview(Some((id, diffs))) => {
                            let mut core = s.core.lock();
                            if let Some(a) = core.state.approvals.iter_mut().find(|a| a.id == id) {
                                a.diffs = diffs;
                                core.touch_state();
                            }
                        }
                        Ev::Preview(None) => {}
                        Ev::Change((abs, found)) => {
                            s.core.lock().found(&abs, found);
                            while let Some(Some((abs, found))) = changes.next().now_or_never() {
                                s.core.lock().found(&abs, found);
                            }
                        }
                        Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                        Ev::Fx(None) => {}
                        Ev::Tick => {}
                        Ev::Room => {
                            let mut core = s.core.lock();
                            adapter.drain(&mut core);
                        }
                        Ev::Exit(c) => {
                            code = c;
                            // Read what is left on stdout (the last lines often explain the exit).
                            while let Ok(Some(chunk)) = crate::rt::timeout(Duration::from_millis(300), p.stdout.recv()).await.map(Result::ok) {
                                buf.extend_from_slice(&chunk);
                            }
                            {
                                let mut core = s.core.lock();
                                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                    let line: Vec<u8> = buf.drain(..=nl).collect();
                                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                                }
                            }
                            break;
                        }
                    }
                    // Polls the harness only answers when asked: every few seconds, however busy.
                    if Instant::now() >= next_tick {
                        next_tick = Instant::now() + TICK;
                        let mut core = s.core.lock();
                        adapter.tick(&mut core);
                        // A step that ended without a word after it (limits that grew meanwhile).
                        if super::pause::stop_mid_turn(&mut core) {
                            adapter.command(&mut core, Command::Interrupt);
                        }
                    }
                    jobs = flush(&s, &sink, Some(&p.stdin)).await;
                    let listed = {
                        let core = s.core.lock();
                        (core.meta.models != models_seen).then(|| core.meta.models.clone())
                    };
                    if let Some(m) = listed {
                        super::kinds::note_models(kind.id, &m);
                        models_seen = m;
                    }
                    if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                        rewind_to = Some(at);
                    }
                    if rewind_to.is_some() {
                        p.terminate(Duration::from_secs(2));
                        let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        break;
                    }
                    if forcing {
                        p.terminate(Duration::from_secs(2));
                        let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        ending = true;
                        break;
                    }
                    // A switch, now that nothing is under way: the harness ends by itself once its
                    // input closes (its record of the conversation complete), else it is stopped.
                    let go = {
                        let core = s.core.lock();
                        (switching.is_some() && switch_now(&core)) || ((moving.is_some() || reloading) && settled_core(&core))
                    };
                    if go {
                        left = s.core.lock().state.tasks.iter().filter(|t| t.left_running()).count();
                        p.stdin.close();
                        if crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await.is_err() {
                            p.terminate(Duration::from_secs(2));
                            let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        }
                        ending = true;
                        break;
                    }
                    if adapter.seeded() && s.carried.lock().take().is_some() {
                        seed = None;
                        on_change();
                    }
                    let (session, queue, pause, status, stands, unwaited) = {
                        let mut core = s.core.lock();
                        let work = Work::in_state(&core.state);
                        // What waits to go to its harness, which goes once there's room (not
                        // while it's paused, or held for a switch).
                        let waits = (adapter.waits(&core) || !for_room.is_empty()) && core.state.pause.is_none() && !core.hold;
                        // It waits for room no longer: it's at work, or nothing waits to go.
                        let unwaited = (work == Work::Yes || !waits) && core.state.room_wait.take().is_some();
                        if unwaited {
                            core.touch_state();
                        }
                        (core.state.harness_session.clone(), core.state.queue.len(), pause_marks(&core.state), core.state.status, (work, waits), unwaited)
                    };
                    // Its machine keeps who's at work and who waits for room (`super::limit`), told
                    // again once it waits no longer, so no place in line is left behind.
                    if stood != Some(stands) || unwaited {
                        stood = Some(stands);
                        (files.work)(stands.0, stands.1);
                    }
                    let was = s.turn_open.load(std::sync::atomic::Ordering::Relaxed);
                    let open = turn_open(status, was);
                    s.turn_open.store(open, std::sync::atomic::Ordering::Relaxed);
                    if session != last_session || queue != last_queue || pause != last_pause || open != was {
                        last_session = session;
                        last_queue = queue;
                        last_pause = pause;
                        on_change();
                    }
                }
                *s.pid.lock() = None;
                // A conversation carried in that it took is in its own session now, which it
                // starts again on (Antigravity starts again for a new mode, model or effort); one
                // it hasn't taken yet is given to it again.
                if adapter.seeded() {
                    seed = None;
                    if s.carried.lock().take().is_some() { on_change(); }
                }
                if rewind_to.is_some() {
                    adapter.let_go(&mut s.core.lock());
                    first = false;
                    continue;
                }
                if ending {
                    {
                        let mut core = s.core.lock();
                        adapter.exited(&mut core, None, None);
                    }
                    first = false;
                    continue;
                }
                let tail = String::from_utf8_lossy(&stderr_tail).trim().to_owned();
                if !tail.is_empty() && code != Some(0) {
                    error = Some(super::model::clip_middle(&tail, 2000).0);
                }
            }
        }
        {
            let mut core = s.core.lock();
            adapter.exited(&mut core, code, error.clone());
        }
        flush(&s, &sink, None).await;
        on_change();
        first = false;
        if adapter.seeded() {
            seed = None;
            if s.carried.lock().take().is_some() { on_change(); }
        }
        // A switch, a move or a setting waiting for the turn: the turn is over (it ended with the
        // harness).
        if switching.is_some() || moving.is_some() || reloading {
            continue;
        }
        // Its process ending never ends the agent, however it ended (a signal from outside it, a
        // crash, an end of its own): an agent lives until a person, or the agent that started it,
        // ends it (docs/agents.md, "An agent lives until it's ended"). It starts again by itself,
        // its conversation resumed and the turn the end cut short picked up first, once there's
        // room on its machine for it: after `AGAIN_FIRST`, each time twice the wait before (at most
        // `AGAIN_MOST`), from `AGAIN_FIRST` again once it ran `AGAIN_RESET`. A harness that never
        // started, or ended within `SOON` of each of its last `SOON_TRIES` starts, won't start: it
        // waits for a person (a restart or a message), saying why.
        let ran = started.elapsed();
        if ran >= AGAIN_RESET {
            again = AGAIN_FIRST;
        }
        soon = if ran < SOON { soon.saturating_add(1) } else { 0 };
        if launched && soon < SOON_TRIES {
            let cut = s.turn_open.load(std::sync::atomic::Ordering::Relaxed);
            again_at = Instant::now().checked_add(again);
            {
                let mut core = s.core.lock();
                let now = Core::now();
                if let Some(x) = core.state.exit.as_mut() {
                    x.again_at = Some(now.saturating_add(i64::try_from(again.as_millis()).unwrap_or(i64::MAX)));
                }
                let then = if cut { ", and picks up the turn it was on" } else { "" };
                core.upsert(&format!("harness-again:{now}"), None, super::model::Body::Notice { level: Level::Warn, text: format!("Its process ended ({}). kiln starts it again{then}: its conversation goes on.", ended_how(code)) });
                if cut && !core.state.queue.first().is_some_and(|q| q.text == PICK_UP) {
                    core.state.queue.insert(0, super::model::QueueItem { id: format!("{}again-{now}", super::core::KEEP_ID), text: PICK_UP.to_owned(), at: now, attachments: Vec::new(), overrides: None, reply_to: None });
                }
                core.touch_state();
            }
            flush(&s, &sink, None).await;
            on_change();
            again = again.saturating_mul(2).min(AGAIN_MOST);
        } else if launched {
            {
                let mut core = s.core.lock();
                if let Some(x) = core.state.exit.as_mut() {
                    x.error.get_or_insert_with(|| format!("It ended within {} s of each of its last {SOON_TRIES} starts, so kiln stopped starting it again: send a message or restart it.", SOON.as_secs()));
                }
                core.touch_state();
            }
            flush(&s, &sink, None).await;
            on_change();
        }
        // Skill runs that waited for room go with its next start.
        carry.append(&mut for_room);
        // Down: a restart, or a message (which restarts it and is then delivered), brings it back
        // once there's room on its machine for it to go to work. Other commands still get answers
        // (a skill's file, an MCP server added with the CLI).
        loop {
            // Its machine knows it isn't at work, and whether it waits for room to start again.
            let (queued, unwaited) = {
                let mut core = s.core.lock();
                let queued = !core.state.queue.is_empty() && core.state.pause.is_none() && !core.hold;
                // Nothing waits to start it again: it waits for room no longer.
                let unwaited = !(restart_wanted || !carry.is_empty() || queued) && core.state.room_wait.take().is_some();
                if unwaited {
                    core.touch_state();
                }
                (queued, unwaited)
            };
            let stands = (Work::No, restart_wanted || !carry.is_empty() || queued);
            if stood != Some(stands) || unwaited {
                stood = Some(stands);
                (files.work)(stands.0, stands.1);
            }
            if unwaited {
                flush(&s, &sink, None).await;
            }
            // Its process ended without anyone ending it: it starts again when its wait is over.
            let due = again_at;
            let wake = async move {
                match due {
                    Some(at) => crate::rt::sleep(at.saturating_duration_since(Instant::now())).await,
                    None => futures::future::pending::<()>().await,
                }
            };
            let ev = futures::select_biased! {
                c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                () = input_budget(&s).fuse() => Ev::InputBudget,
                        () = promotion_journal(&s).fuse() => Ev::PromotionJournal,
                f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
                () = wake.fuse() => {
                    again_at = None;
                    Ev::Ctl(Some(Ctl::Cmd(Box::new(Command::Restart))))
                }
            };
            match ev {
                Ev::Ctl(Some(Ctl::Cmd(c))) => {
                            let prepared = journal_edit(&s, &files, *c, |core, command| adapter.command(core, command)).await;
                            match prepared {
                    None => {},
                    Some(command) => match command {
                    Command::StateRead { client, read } => read_state(&mut s.core.lock(), client, read),
                            Command::MailHandoff { client, revision, handoff, turn, ids, response_digest } => register_handoff(&s, client, revision, handoff, turn, ids, response_digest).await,
                    Command::AtState { client, revision, command } => {
                        at_state(&mut s.core.lock(), client, &revision, *command, |core, command| adapter.command(core, command));
                    }
                    Command::Restart => {
                        if s.core.lock().has_room() {
                            break;
                        }
                        restart_wanted = true;
                    }
                    Command::Snapshot => {
                        for u in s.snapshot() {
                            let _ = sink.send((s.id.clone(), u)).await;
                        }
                    }
                    Command::OwnEdits { client, by } => s.core.lock().ask_own(client, &by),
                    Command::RemoveTask { task } => {
                        if s.core.lock().remove_task(&task) {
                            on_change();
                        }
                    }
                    // Another harness's model: the session goes on in it, now.
                    Command::SetModel { model, kind: Some(to) } if to != kind.id => {
                        if let Some(sw) = wanted(&s, kind, &to, model) {
                            switching = Some(sw);
                            break;
                        }
                    }
                    // Another runner: it goes there, now.
                    Command::MoveTo { to, cwd, note, now, kind: into, model, local } => {
                        moving = Some(move_wanted(&s, to, cwd, note, now, into.as_deref().and_then(Kind::get).filter(|k| k.id != kind.id), model, local));
                        break;
                    }
                    Command::Moving { text } => moving_note(&s, &text),
                    Command::StopTask { task } if task.starts_with(crate::reach::owned::PREFIX) => {
                        s.stop_owned_reach(Some(&task));
                    }
                    Command::StopTask { task } if task.starts_with(super::survivors::LEFT) => {
                        stop_left(&s, Some(&task)).await;
                        on_change();
                    }
                    Command::StopTasks => {
                        s.stop_owned_reach(None);
                        stop_left(&s, None).await;
                        on_change();
                    }
                    // For a subagent: it ended with its harness, so it isn't started again for it.
                    c @ Command::Send { task: Some(_), .. } => {
                        adapter.command(&mut s.core.lock(), c);
                        on_change();
                    }
                    c @ (Command::Send { .. } | Command::SkillRun { .. }) => {
                        let c = match prepare(&files, c).await { Ok(command) => command, Err(error) => { preparation_failed(&s, error).await; continue; } };
                        let mut core = s.core.lock();
                        // What waits in its queue goes first (held for its harness when that
                        // ended, CP-T3.2): one to go in turn waits behind it, and the harness,
                        // started again, takes them in order.
                        if matches!(&c, Command::Send { mode: super::model::SendMode::Queue, task: None, .. }) && !core.state.queue.is_empty() {
                            if let Some(m) = self::queued(c) {
                                core.state.queue.push(m);
                                core.touch_state();
                            }
                            let room = core.has_room();
                            drop(core);
                            on_change();
                            if room {
                                break;
                            }
                        } else if core.has_room() {
                            drop(core);
                            carry.push(c);
                            break;
                        } else {
                            // No room on its machine for it to go to work: a message waits in its
                            // queue till there is (a skill run with what goes with its next start).
                            match c {
                                Command::Send { id, text, attachments, overrides, reply_to, .. } => {
                                    core.state.queue.push(super::model::QueueItem { id, text, at: Core::now(), attachments, overrides, reply_to });
                                    core.touch_state();
                                }
                                c => carry.push(c),
                            }
                            drop(core);
                            on_change();
                        }
                    }
                    // Where its commands run, while it isn't running: from its next start.
                    Command::Control { id, value } if id == "reach" => {
                        if set_reach(&s, value.as_ref()) {
                            let mut core = s.core.lock();
                            let text = format!("{} from its next start", reach_words(core.state.reach.as_deref()));
                            core.upsert(&format!("harness-switch:reach-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                        }
                        on_change();
                    }
                    // Named while it isn't running: kiln's name now, the harness's when it next runs.
                    Command::Control { id, value } if id == "rename" => {
                        if let Some(c) = name_session(&s, value.as_ref()) {
                            carry.push(c);
                        }
                        on_change();
                    }
                    // Its threshold, set while it isn't running (a person's, or kiln's for a
                    // program's worker): kept, and it holds from its next start.
                    Command::PauseAt { percent } => {
                        let mut core = s.core.lock();
                        core.state.pause_at = percent.filter(|p| (1..=100).contains(p));
                        core.touch_state();
                        drop(core);
                        on_change();
                    }
                    // Paused while it isn't running: its processes are held as soon as it has any.
                    Command::Pause { on, task: None } => {
                        pause_session(&s, on);
                        on_change();
                    }
                    // Branched while it isn't running: from its conversation as it was.
                    Command::Branch { words, top, task } => {
                        branch(&s, &files, &words, top, task.as_deref());
                        on_change();
                    }
                    // Its perms, set while it isn't running: they hold from its next start.
                    Command::Perms { perms, task: None } => {
                        let mut core = s.core.lock();
                        core.state.perms = perms;
                        core.touch_state();
                        drop(core);
                        on_change();
                    }
                    c => adapter.command(&mut s.core.lock(), c),
                },
                }
                },
                // No process to hand over: the next image starts it from its record.
                Ev::Ctl(Some(Ctl::HandOver(reply))) => {
                    let _ = reply.send(Ok(None));
                }
                // No harness of its runs now to hold a call.
                Ev::Ctl(Some(Ctl::Hook(_, reply))) => {
                    let _ = reply.send(serde_json::json!({}));
                }
                // There may be room on its machine now: it starts again if anything waits for that,
                // the first message in its queue first.
                Ev::Ctl(Some(Ctl::Room)) => {
                    let mut core = s.core.lock();
                    let queued = !core.state.queue.is_empty() && core.state.pause.is_none() && !core.hold;
                    if (restart_wanted || !carry.is_empty() || queued) && core.has_room() {
                        if carry.is_empty() && queued {
                            let q = core.state.queue.remove(0);
                            core.touch_state();
                            carry.push(Command::Send { id: q.id, text: q.text, mode: super::model::SendMode::Queue, attachments: q.attachments, overrides: q.overrides, reply_to: q.reply_to, task: None });
                        }
                        break;
                    }
                }
                Ev::Ctl(Some(Ctl::OwnedChanged)) => {},
                Ev::Ctl(Some(Ctl::Promotion)) => progress_promotion(&s, &files).await,
                Ev::InputBudget => { if publish_input_budget(&s) && promotion_pending(&s) { progress_promotion(&s, &files).await; } },
                        Ev::PromotionJournal => progress_promotion(&s, &files).await,
                Ev::Ctl(Some(Ctl::ArchiveQuiet(reply))) => {let _=reply.send(s.archive_quiet_now(restart_wanted || !carry.is_empty() || !for_room.is_empty() || moving.is_some() || switching.is_some() || reloading));},
                Ev::Ctl(Some(Ctl::Epoch(e))) => raise_epoch(&s, &files, e).await,
                Ev::Ctl(ctl @ (Some(Ctl::Close | Ctl::LetGo) | None)) => {
                    s.stop_owned_reach(None);
                    // Ending the agent ends what it left running that kiln took back over, and the
                    // rest of its group of processes, which then goes.
                    let left = std::mem::take(&mut *s.left.lock());
                    if !left.is_empty() {
                        let _ = crate::rt::blocking(move || left.iter().for_each(super::survivors::stop_tree)).await;
                    }
                    // Gone on elsewhere without its harness ever running here (it couldn't start),
                    // it has no group here: one under its id is the machine's it went on to, when
                    // both kiln processes share a cgroup (a test's server and its runners), its
                    // harness starting there, and isn't ended from here.
                    if let Some(g) = crate::os::contain::Group::existing(&s.id).filter(|_| ran_here || !matches!(ctl, Some(Ctl::LetGo))) {
                        end_group(g, Duration::ZERO);
                    }
                    if matches!(ctl, Some(Ctl::LetGo)) {
                        tracing::info!(session = %s.id, "agent session went on on another machine; this one lets go of it");
                        (files.release)(&s.id);
                    } else {
                        end_tasks(&mut s.core.lock());
                        let _ = flush(&s, &sink, None).await;
                        let _ = sink.send((s.id.clone(), Update::Closed)).await;
                    }
                    on_change();
                    return;
                }
                Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                _ => {}
            }
            flush(&s, &sink, None).await;
            if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                rewind_to = Some(at);
                break;
            }
        }
        restart_wanted = false;
    }
}

#[cfg(test)]
mod tests {
    use crate::util::Must as _;
    #[test]
    fn deferred_bodies_retain_the_complete_compact_queue_index() -> anyhow::Result<()> {
        let state = State {
            status: Status::Running,
            queue: (0..600).map(|i| super::super::model::QueueItem {
                id: format!("queued-{i}"), text: "payload".repeat(400), at: i,
                attachments: Vec::new(), overrides: None, reply_to: None,
            }).collect(),
            ..State::default()
        };
        let view = super::super::state_view::view(&state, super::super::state_view::FRAME)?;
        let projected: State = serde_json::from_value(view.into_state()?)?;
        assert!(projected.queue.is_empty());
        let mut info = Info::default();
        info.apply(&projected);
        assert_eq!(info.queued, state.queue.len());
        assert_eq!(info.queue.len(), state.queue.len());
        assert!(!info.queue_partial);
        for (actual, original) in info.queue.iter().zip(&state.queue) {
            assert_eq!(actual, &super::super::queued::Queued::of(&original.id, &original.text));
        }
        assert!(serde_json::to_value(&info)?.get("queue").is_none(), "the compact local index is not a wire body");
        Ok(())
    }

    #[test]
    fn the_local_queue_index_is_bounded_and_marks_its_unseen_suffix() -> anyhow::Result<()> {
        let state = State { queue: (0..128).map(|i| super::super::model::QueueItem {
            id: format!("{i}-{}", "x".repeat(8192)), text: "small".into(), at: i,
            attachments: Vec::new(), overrides: None, reply_to: None,
        }).collect(), ..State::default() };
        let expected = super::super::state_view::queue_index(&state.queue, 0, super::super::state_view::FRAME)?;
        assert!(serde_json::to_vec(&expected)?.len() <= super::super::state_view::FRAME);
        let mut info = Info::default();
        info.apply(&state);
        assert!(info.queue_partial);
        assert_eq!(info.queue.len(), expected.entries.len());
        assert!(info.queue.len() < state.queue.len());
        assert_eq!(info.queue_next, expected.next);
        Ok(())
    }

    #[test]
    fn a_partial_index_revision_wakes_waiters_even_when_its_count_is_unchanged() {
        let before = Info { queued: 10, queue_partial: true, queue_revision: Some("first".into()), ..Info::default() };
        let after = Info { queue_revision: Some("second".into()), ..before.clone() };
        assert!(after.differs(&before));
        let before = Info { queue_partial: false, ..before };
        let after = Info { queue_partial: false, ..after };
        assert!(!after.differs(&before), "ordinary complete-index notification rules are unchanged");
    }

    /// A session that runs nothing, and the receiving end of its channel (what its task would read).
    fn idle(id: &str) -> (Session, crate::memory::queue::Receiver<Ctl>) {
        let (ctl, rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let s = Session {
            id: id.into(),
            spec: Mutex::new(Spec::default()),
            created_at: 0,
            core: Mutex::new(Core::new()),
            info: Mutex::new(Info::default()),
            ctl,
            pid: Mutex::new(None),
            turn_open: std::sync::atomic::AtomicBool::new(false),
            ran: Mutex::new((None, Vec::new())),
            left: Mutex::new(Vec::new()),
            moving: Mutex::new(false),
            prompted: std::sync::atomic::AtomicBool::new(false),
            carried: Mutex::new(None),
            ended: std::sync::atomic::AtomicBool::new(false),
            ended_changed: crate::rt::Changed::new(),
            owned_reach: Mutex::new(std::collections::HashMap::new()),
        };
        (s, rx)
    }

    fn attachment_files(dir: std::path::PathBuf) -> Files {
        Files { host: None, dir, take: Arc::new(|_| None), spawn: Arc::new(|_| {}), release: Arc::new(|_| {}), carried: Arc::new(|_| Box::pin(async { Ok(Vec::new()) })), fence: Arc::new(|_, _| Box::pin(async { Ok(()) })), settle: Arc::new(|_, _, _| Box::pin(async { Ok(None) })), room: Arc::new(|| true), work: Arc::new(|_, _| {}) }
    }

    #[test]
    fn partial_legacy_rebinding_retains_completion_until_epochs_match_and_retires_its_flight() -> anyhow::Result<()> {
        use super::super::{admission::{Consumption, FlightProtocol, Input, Lease, LegacyFlight, PhysicalBudget, Stage}, inbox::{Inbox, Limits as InboxLimits, Pending, PromotionPhase, promotion::RebindStep}};
        crate::rt::block_on(async {
            for prior_unavailable in [false, true] {
                let directory = tempfile::tempdir()?;
                let records = crate::records::open(&directory.path().join("records.db"))?;
                let inbox = Arc::new(Inbox::new(records, InboxLimits::default()));
                let (session, _control) = idle("partial-legacy-effect");
                inbox.open(&session.id, 0).await?;
                let pool = PhysicalBudget::new(super::super::state_view::FRAME);
                let input = Input::new(0, pool.clone(), inbox.clone());
                let ids = ["0123456789abcdef0123456789abcde0a", "0123456789abcdef0123456789abcde0b"];
                let [first, second] = ids;
                let bytes = size_of::<LegacyFlight>() + first.len();
                for id in ids {
                    let lease = Lease { message: id.into(), peer: String::new(), epoch: 0, bytes, stage: Stage::ProviderFlight, token: None, handoff: None };
                    input.controller.lock().restore(lease.clone()).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    inbox.adopt_pending(&session.id, Pending::opaque(lease, Vec::new()), 1).await?;
                }
                input.controller.lock().begin_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                if prior_unavailable { input.controller.lock().unavailable().map_err(|error| anyhow::anyhow!("{error:?}"))?; }
                {
                    let mut core = session.core.lock();
                    core.input = Some(input.clone());
                    core.input_legacy_flights = ids.iter().map(|id| LegacyFlight { id: (*id).into(), protocol: FlightProtocol::Claude, request: None }).collect();
                }
                let frozen = session.core.lock().freeze();
                let (_, events) = frozen.handover_source();
                let proof = crate::runner::custody::LegacyProof::of(&session.id, 0, events, None).map_err(anyhow::Error::msg)?;
                inbox.prepare_promotion(&session.id, 0, 1, proof, &pool, Some(&input)).await?;
                inbox.advance_promotion(&session.id, 1, PromotionPhase::AwaitingCertificate, &pool, Some(&input)).await?;
                inbox.advance_promotion(&session.id, 1, PromotionPhase::Rebinding, &pool, Some(&input)).await?;
                assert_eq!(inbox.rebind_promotion_step(&session.id, &input).await?, RebindStep::Row);
                pool.resize(pool.counts().0);
                assert_eq!(inbox.rebind_promotion_step(&session.id, &input).await, Err(super::super::inbox::PromotionError::ResourceHeld));
                assert_eq!(inbox.pending(&session.id, first).await?.must()?.lease.epoch, 1);
                assert_eq!(input.controller.lock().capacity().epoch, 0);
                // An exact native callback arrived between the real partial commit and its
                // external budget wake. It is a component input, not a model-work claim.
                session.core.lock().input_consumed(first, Consumption::ProviderCompleted);
                assert!(!flush_input(&session).await);
                assert_eq!(session.core.lock().input_receipts.len(), 1);
                assert!(inbox.pending(&session.id, first).await?.must()?.disposition.is_none(), "the epoch guard must not be weakened");
                assert_eq!(pool.counts().0, bytes * 2);
                pool.resize(super::super::state_view::FRAME);
                assert_eq!(inbox.rebind_promotion_step(&session.id, &input).await?, RebindStep::Row);
                assert_eq!(inbox.rebind_promotion_step(&session.id, &input).await?, RebindStep::Complete);
                {
                    let mut core = session.core.lock();
                    input.controller.lock().rebind_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    core.state.epoch = 1;
                }
                assert!(!flush_input(&session).await, "the owner must stay Promoting until the receipt commits");
                assert!(session.core.lock().input_receipts.is_empty());
                assert!(inbox.pending(&session.id, first).await?.must()?.disposition.is_some());
                assert_eq!(inbox.promotion(&session.id).await?.must()?.phase, PromotionPhase::Rebinding);
                assert!(input.controller.lock().lease(second).is_some());
                assert_eq!(pool.counts().0, bytes);
                inbox.advance_promotion(&session.id, 1, PromotionPhase::Active, &pool, Some(&input)).await?;
                inbox.retire_promotion(&session.id, 1, &pool, Some(&input)).await?;
                {
                    let mut controller = input.controller.lock();
                    controller.complete_legacy_reconciliation().map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    controller.activate_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    if prior_unavailable { assert_eq!(controller.capacity().target, 0, "genuine Unavailable remains held"); }
                    else { assert!(controller.capacity().target > 0, "a callback during partial rebinding poisoned readiness"); }
                }
                session.core.lock().input_consumed(first, Consumption::ProviderCompleted);
                let _ = flush_input(&session).await;
                assert!(session.core.lock().input_receipts.is_empty(), "a committed legacy callback must not defer forever after its lease is gone");
                assert!(session.core.lock().input_legacy_flights.iter().any(|flight| flight.id == second));
                assert_eq!(input.controller.lock().capacity().admitted, 1, "a duplicate callback cannot release the other original");
            }
            Ok(())
        })
    }

    #[test]
    fn early_legacy_effect_waits_for_its_original_without_poisoning_readiness() -> anyhow::Result<()> {
        use super::super::{admission::{Consumption, FlightProtocol, Input, Lease, LegacyFlight, PhysicalBudget, Stage}, inbox::{Inbox, Limits as InboxLimits, Pending, PromotionPhase}};
        crate::rt::block_on(async {
            for effect in ["stage", "retained", "receipt"] {
                for prior_unavailable in [false, true] {
                    let directory = tempfile::tempdir()?;
                    let records = crate::records::open(&directory.path().join("records.db"))?;
                    let inbox = Arc::new(Inbox::new(records, InboxLimits::default()));
                    let (session, _control) = idle("early-legacy-effect");
                    inbox.open(&session.id, 0).await?;
                    let pool = PhysicalBudget::new(super::super::state_view::FRAME);
                    let input = Input::new(0, pool.clone(), inbox.clone());
                    let id = "0123456789abcdef0123456789abcdef";
                    let lease = Lease { message: id.into(), peer: String::new(), epoch: 0, bytes: size_of::<LegacyFlight>() + id.len(), stage: Stage::ProviderFlight, token: None, handoff: None };
                    input.controller.lock().restore(lease.clone()).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    input.controller.lock().begin_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    if prior_unavailable { input.controller.lock().unavailable().map_err(|error| anyhow::anyhow!("{error:?}"))?; }
                    {
                        let mut core = session.core.lock();
                        core.input = Some(input.clone());
                        core.input_legacy_flights.push(LegacyFlight { id: id.into(), protocol: FlightProtocol::Claude, request: None });
                        match effect {
                            "stage" => core.input_stages.push(id.into()),
                            "retained" => core.input_retained.push(id.into()),
                            _ => core.input_consumed(id, Consumption::ProviderCompleted),
                        }
                    }
                    let frozen = session.core.lock().freeze();
                    let (_, events) = frozen.handover_source();
                    let proof = crate::runner::custody::LegacyProof::of(&session.id, 0, events, None).map_err(anyhow::Error::msg)?;
                    inbox.prepare_promotion(&session.id, 0, 1, proof, &pool, Some(&input)).await?;
                    inbox.advance_promotion(&session.id, 1, PromotionPhase::AwaitingCertificate, &pool, Some(&input)).await?;
                    assert!(!flush_input(&session).await);
                    assert!(inbox.pending(&session.id, id).await?.is_none());
                    {
                        let core = session.core.lock();
                        let deferred = match effect { "stage" => core.input_stages.len(), "retained" => core.input_retained.len(), _ => core.input_receipts.len() };
                        assert_eq!(deferred, 1, "the exact early {effect} must remain retryable");
                    }
                    assert_eq!((input.controller.lock().capacity().epoch, pool.counts().0), (0, lease.bytes));
                    // This is the component's exact source-journal boundary, not a fabricated
                    // provider response or certificate. The native upgrade fixture proves those.
                    inbox.adopt_pending(&session.id, Pending::opaque(lease.clone(), Vec::new()), 1).await?;
                    assert!(!flush_input(&session).await, "Promoting still withholds new prompt writes");
                    {
                        let core = session.core.lock();
                        assert!(core.input_stages.is_empty() && core.input_retained.is_empty() && core.input_receipts.is_empty());
                    }
                    let row = inbox.pending(&session.id, id).await?.must()?;
                    assert_eq!((row.lease.message.as_str(), row.lease.epoch, row.lease.token), (id, 0, None));
                    assert_eq!(row.disposition.is_some(), effect == "receipt");
                    {
                        let mut controller = input.controller.lock();
                        controller.rebind_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                        controller.complete_legacy_reconciliation().map_err(|error| anyhow::anyhow!("{error:?}"))?;
                        controller.activate_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
                        let capacity = controller.capacity();
                        assert_eq!(capacity.admitted, usize::from(effect != "receipt"));
                        assert_eq!(capacity.bytes, if effect == "receipt" { 0 } else { lease.bytes });
                        if prior_unavailable { assert_eq!(capacity.target, 0, "genuine prior Unavailable must not be cleared"); }
                        else { assert!(capacity.target > 0, "early {effect} missing its legacy row poisoned later successful reconciliation"); }
                    }
                }
            }
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn legacy_promotion_does_not_advance_past_uncommitted_input_effects() -> anyhow::Result<()> {
        use super::super::{admission::{command_bytes, Input, Lease, PhysicalBudget, Stage}, host::{Host, Limits as HostLimits}, inbox::{Inbox, Limits as InboxLimits}};
        use kiln_seams::{clock::SimClock, records::{Consistency, Records as _, SimRecords}};
        crate::rt::block_on(async {
            for pending_effect in [true, false] {
                let directory = tempfile::tempdir()?;
                let (session, _control) = idle(if pending_effect { "legacy-effect-held" } else { "legacy-effect-clear" });
                let records = Arc::new(SimRecords::new());
                let inbox = Arc::new(Inbox::new(records.clone(), InboxLimits::default()));
                let (sink, _updates) = async_channel::unbounded();
                let host = Host::with_records(sink, Some(directory.path().join("sessions.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(records.clone()));
                host.set_input_budget(super::super::state_view::FRAME);
                inbox.open(&session.id, 0).await?;
                let frozen = session.core.lock().freeze();
                let (source_epoch, events) = frozen.handover_source();
                let proof = crate::runner::custody::LegacyProof::of(&session.id, source_epoch, events, None).map_err(anyhow::Error::msg)?;
                host.prepare_legacy_promotion(&session.id, 0, 1, proof).await?;
                let before = records.get("inbox-heads", session.id.as_bytes(), Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("promotion head missing"))?;
                let pool = PhysicalBudget::new(super::super::state_view::FRAME);
                let input = Input::new(0, pool, Arc::clone(&inbox));
                let original = message("uncommitted-native-input", "retained original");
                let bytes = command_bytes(&original).map_err(|error| anyhow::anyhow!("fixture input weight: {error:?}"))?;
                if pending_effect {
                    input.controller.lock().restore(Lease { message: "uncommitted-native-input".into(), peer: "legacy-peer".into(), epoch: 0, bytes, stage: Stage::InCtl, token: None, handoff: None }).map_err(|error| anyhow::anyhow!("fixture lease: {error:?}"))?;
                    let mut core = session.core.lock();
                    core.input = Some(input.clone());
                    core.input_stages.push("uncommitted-native-input".into());
                } else {
                    input.controller.lock().begin_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("fixture promotion: {error:?}"))?;
                    session.core.lock().input = Some(input.clone());
                }
                let mut files = attachment_files(directory.path().join("files"));
                files.host = Some(Arc::downgrade(&host));
                assert!(!flush_input(&session).await, "both an uncommitted effect and a deliberately gated Promoting owner withhold writes");
                if pending_effect {
                    assert!(inbox.pending(&session.id, "uncommitted-native-input").await?.is_none(), "the real journal cannot commit this stage callback without its pending row");
                    assert_eq!(session.core.lock().input_stages, ["uncommitted-native-input"]);
                }
                progress_promotion(&session, &files).await;
                {
                    let controller = input.controller.lock();
                    assert_eq!(controller.capacity().epoch, 0);
                    assert_eq!(session.epoch(), 0);
                    if pending_effect {
                        assert!(!controller.promoting(), "ownership cannot enter Promoting ahead of an earlier uncommitted native effect");
                        assert!(controller.lease("uncommitted-native-input").is_some_and(|lease| lease.epoch == 0 && lease.bytes == bytes && lease.stage == Stage::InCtl));
                        let core = session.core.lock();
                        assert_eq!(core.input_stages, ["uncommitted-native-input"], "the original callback remains retryable");
                        assert!(core.events().any(|event| event.id == "legacy-promotion-held" && matches!(&event.body, Body::Notice { level: Level::Warn, .. })), "the person can see why ownership is held");
                    } else {
                        assert!(controller.promoting());
                        let core = session.core.lock();
                        assert!(core.input_stages.is_empty() && core.input_retained.is_empty() && core.input_receipts.is_empty());
                        assert!(core.out.iter().any(|update| matches!(update, Update::MailFlow { event } if matches!(event.as_ref(), super::super::admission::FlowEvent::Capacity { .. }))), "no unresolved effects: the deliberately gated owner must still execute its promotion step");
                        assert!(!core.events().any(|event| event.id == "legacy-promotion-held"));
                    }
                }
                let after = records.get("inbox-heads", session.id.as_bytes(), Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("promotion head disappeared"))?;
                assert_eq!((after.rev, after.value), (before.rev, before.value), "neither case has authority to advance the persisted head epoch yet");
            }
            Ok(())
        })
    }

    #[test]
    fn attachment_materialization_is_all_or_error_and_never_clobbers_completed_input() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("incoming-original");
        let destination = dir.path().join("materialized");
        std::fs::write(&source, b"original attachment")?;
        immutable_attachment(&destination, Some(&source), &[])?;
        immutable_attachment(&destination, Some(&source), &[])?;
        anyhow::ensure!(std::fs::read(&source)? == b"original attachment", "materialization cannot spend/move durable incoming bytes");
        anyhow::ensure!(immutable_attachment(&destination, None, b"different retry").is_err());
        anyhow::ensure!(std::fs::read(&destination)? == b"original attachment");
        let good = super::super::model::Attachment { name: "first.txt".into(), mime: "text/plain".into(), data_base64: "Zmlyc3Q=".into(), file_id: None, path: None };
        let missing = super::super::model::Attachment { name: "missing.txt".into(), mime: "text/plain".into(), data_base64: String::new(), file_id: Some("missing-upload".into()), path: None };
        anyhow::ensure!(store_attachments(&attachment_files(dir.path().join("files")), "message", vec![good, missing]).is_err(), "a missing attachment cannot become a partial prompt");
        Ok(())
    }

    #[test]
    fn accepted_queue_edit_reserves_growth_and_journals_before_adapter_mutation() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use super::super::{admission::{command_bytes, Input, Lease, PhysicalBudget, Stage}, inbox::{Inbox, Limits, Pending}, mail::Token};
            let dir = tempfile::tempdir()?;
            let files = attachment_files(dir.path().join("files"));
            let (s, _rx) = idle("edit-owner");
            let original = message("edit-message", "old");
            let bytes = command_bytes(&original).map_err(|error| anyhow::anyhow!("fixture weight: {error:?}"))?;
            let pool = PhysicalBudget::new(bytes);
            let inbox = Arc::new(Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), Limits::default()));
            let input = Input::new(0, Arc::clone(&pool), Arc::clone(&inbox));
            let lease = Lease { message: "edit-message".into(), peer: "peer".into(), epoch: 0, bytes, stage: Stage::Queued, token: Some(Token { revision: 1, generation: 1 }), handoff: None };
            input.controller.lock().restore(lease.clone()).map_err(|error| anyhow::anyhow!("fixture lease: {error:?}"))?;
            inbox.accept_pending(&s.id, "edit-message", &super::super::inbox::digest("old", &[]), Some(0), 1, &Pending::new(lease, original)).await?;
            { let mut core = s.core.lock(); core.input = Some(input); core.state.queue.push(super::super::model::QueueItem { id: "edit-message".into(), text: "old".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None }); }
            let edit = || Command::QueueEdit { id: "edit-message".into(), text: "larger replacement".into(), attachments: None };
            anyhow::ensure!(journal_edit(&s, &files, edit(), |_, _| {}).await.is_none(), "growth beyond shared physical authority must retain the old body");
            anyhow::ensure!(inbox.pending(&s.id, "edit-message").await?.is_some_and(|pending| pending.current.is_none()));
            pool.resize(bytes.checked_add(1024).ok_or_else(|| anyhow::anyhow!("fixture bound overflow"))?);
            let mut adapter = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("fixture kind missing"))?.adapter(&Spec::default());
            anyhow::ensure!(journal_edit(&s, &files, edit(), |core, command| {
                assert!(core.state.queue.first().is_some_and(|queued| queued.text == "old"), "persistence must precede owner application");
                adapter.command(core, command);
            }).await.is_none());
            let saved = inbox.pending(&s.id, "edit-message").await?.ok_or_else(|| anyhow::anyhow!("fixture journal missing"))?;
            anyhow::ensure!(matches!(saved.current_command(), Some(Command::Send { text, .. }) if text == "larger replacement"));
            anyhow::ensure!(s.core.lock().state.queue.first().is_some_and(|queued| queued.text == "larger replacement"), "the owner applies the durable edit once");
            Ok(())
        })
    }

    #[test]
    fn accepted_queue_shrink_grants_retry_credit_only_after_cas_and_actual_owner_apply() -> anyhow::Result<()> {
        use kiln_seams::records::{self, Records};
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Conflicting { inner: records::SimRecords, conflict: AtomicBool, pool: Arc<super::super::admission::PhysicalBudget>, before: usize }
        impl Records for Conflicting {
            fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: records::Consistency) -> futures::future::BoxFuture<'a, Result<Option<records::Row>, records::Refusal>> { self.inner.get(family, key, at) }
            fn scan<'a>(&'a self, family: &'a str, scan: records::Scan) -> futures::future::BoxFuture<'a, Result<records::Page, records::Refusal>> { self.inner.scan(family, scan) }
            fn commit<'a>(&'a self, tx: records::Tx) -> futures::future::BoxFuture<'a, Result<records::Rev, records::Refusal>> {
                if self.conflict.swap(false, Ordering::SeqCst) {
                    assert_eq!(self.pool.counts().0, self.before, "a proposed shrink cannot release authority before its journal CAS");
                    return Box::pin(async { Err(records::Refusal::Conflict { family: "inbox-pending".into(), key: b"edit-owner/edit-message".to_vec(), at: Some(2) }) });
                }
                self.inner.commit(tx)
            }
            fn changes<'a>(&'a self, family: &'a str, after: records::Rev, limit: usize) -> futures::future::BoxFuture<'a, Result<records::Changes, records::Refusal>> { self.inner.changes(family, after, limit) }
            fn wait<'a>(&'a self, family: &'a str, after: records::Rev) -> futures::future::BoxFuture<'a, Result<records::Rev, records::Refusal>> { self.inner.wait(family, after) }
            fn head(&self) -> futures::future::BoxFuture<'_, Result<records::Rev, records::Refusal>> { self.inner.head() }
        }
        crate::rt::block_on(async {
            use super::super::{admission::{command_bytes, Input, Lease, PhysicalBudget, Stage}, inbox::{Inbox, Limits, Pending}};
            let dir = tempfile::tempdir()?;
            let files = attachment_files(dir.path().join("files"));
            let (s, _rx) = idle("edit-owner");
            let words = "original accepted communication with retained history".repeat(8);
            let original = message("edit-message", &words);
            let bytes = command_bytes(&original).map_err(|error| anyhow::anyhow!("fixture size: {error:?}"))?;
            let pool = PhysicalBudget::new(bytes);
            let records = Arc::new(Conflicting { inner: records::SimRecords::new(), conflict: AtomicBool::new(false), pool: Arc::clone(&pool), before: bytes });
            let inbox = Arc::new(Inbox::new(records.clone(), Limits::default()));
            let input = Input::new(0, Arc::clone(&pool), Arc::clone(&inbox));
            let lease = Lease { message: "edit-message".into(), peer: "peer".into(), epoch: 0, bytes, stage: Stage::Queued, token: None, handoff: None };
            input.controller.lock().restore(lease.clone()).map_err(|error| anyhow::anyhow!("fixture restore: {error:?}"))?;
            inbox.accept_pending(&s.id, "edit-message", &super::super::inbox::digest(&words, &[]), Some(0), 1, &Pending::new(lease, original)).await?;
            { let mut core = s.core.lock(); core.input = Some(input.clone()); core.state.queue.push(super::super::model::QueueItem { id: "edit-message".into(), text: words.clone(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None }); }
            let before = input.controller.lock().capacity().retry_permission();
            let edit = || Command::QueueEdit { id: "edit-message".into(), text: "short".into(), attachments: None };
            let mut adapter = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("fixture kind missing"))?.adapter(&Spec::default());
            records.conflict.store(true, Ordering::SeqCst);
            let mut applied = false;
            assert!(journal_edit(&s, &files, edit(), |core, command| { applied = true; adapter.command(core, command); }).await.is_none());
            assert!(!applied, "a failed journal CAS cannot apply the edit");
            assert_eq!(input.controller.lock().capacity().retry_permission(), before);
            assert_eq!(input.controller.lock().lease("edit-message").map(|lease| lease.bytes), Some(bytes));
            assert_eq!(pool.counts().0, bytes, "failed CAS rollback returns the exact resident authority");
            assert!(inbox.pending(&s.id, "edit-message").await?.is_some_and(|pending| pending.current.is_none()));
            assert!(s.core.lock().state.queue.first().is_some_and(|queued| queued.text == words));
            assert!(journal_edit(&s, &files, edit(), |core, command| {
                assert_eq!(input.controller.lock().capacity().retry_permission(), before, "CAS alone has not freed the old queue body");
                assert_eq!(pool.counts().0, bytes, "the old body remains charged until the owner replaces it");
                assert!(core.state.queue.first().is_some_and(|queued| queued.text == words));
                assert!(futures::executor::block_on(inbox.pending(&s.id, "edit-message")).is_ok_and(|pending| pending.is_some_and(|pending| pending.current.is_some())), "the current body is durable before owner application");
                adapter.command(core, command);
            }).await.is_none());
            assert!(s.core.lock().state.queue.first().is_some_and(|queued| queued.text == "short"));
            let after = input.controller.lock().capacity().retry_permission();
            assert_eq!(after, before.checked_add(1).ok_or_else(|| anyhow::anyhow!("fixture retry overflow"))?);
            assert!(pool.counts().0 < bytes);
            let saved = inbox.pending(&s.id, "edit-message").await?.ok_or_else(|| anyhow::anyhow!("fixture original missing"))?;
            assert!(matches!(&saved.command, Some(Command::Send { text, .. }) if text == &words));
            assert!(journal_edit(&s, &files, edit(), |core, command| adapter.command(core, command)).await.is_none());
            assert_eq!(input.controller.lock().capacity().retry_permission(), after, "an equal-sized repeated edit is not another release");
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side: what a session is told goes on a lock-free
    /// queue. Messages told from every shard at once each reach it once, each sender's in the order sent (a
    /// move gathers them into its queue so), and once the session is gone, its task sees the end.
    #[test]
    fn g5_a_session_takes_what_every_shard_tells_it_once_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (s, rx) = idle("told");
            let s = Arc::new(s);
            let (senders, per) = (8usize, 300usize);
            let mut telling = Vec::new();
            for t in 0..senders {
                let s = Arc::clone(&s);
                telling.push(crate::rt::spawn_spread_joined(async move { (0..per).all(|n| s.command(message(&format!("{t}-{n}"), "x"))) }));
            }
            for t in telling {
                anyhow::ensure!(t.await.unwrap_or(false), "a message was refused");
            }
            let held = gather(&s, &rx, &mut Vec::new());
            anyhow::ensure!(held.is_empty(), "{} held back", held.len());
            let ids: Vec<String> = s.core.lock().state.queue.iter().map(|q| q.id.clone()).collect();
            anyhow::ensure!(ids.len() == senders * per, "{} of {}", ids.len(), senders * per);
            for t in 0..senders {
                let theirs: Vec<usize> = ids.iter().filter_map(|id| id.strip_prefix(&format!("{t}-"))?.parse().ok()).collect();
                anyhow::ensure!(theirs.iter().copied().eq(0..per), "sender {t}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
            }
            drop(s);
            anyhow::ensure!(matches!(rx.try_recv(), Err(async_channel::TryRecvError::Closed)), "its end, once the session went");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: whether two infos differ in what lists show, compared field by
    /// field without copying either, is what comparing their copies with the activity time, the queue's
    /// words and what was said cleared said: every field changed alone, and those three changed together.
    #[test]
    fn g4_s5_infos_differ_as_their_copies_did() {
        let was = |a: &Info, b: &Info| {
            let strip = |i: &Info| Info { last_activity: 0, queue: Vec::new(), said: Vec::new(), ..i.clone() };
            strip(a) != strip(b)
        };
        let base = Info { id: "s1".into(), kind: "claude".into(), name: "n".into(), cost_usd: 0.25, ..Info::default() };
        let changes: Vec<(&str, Info)> = vec![
            ("id", Info { id: "s2".into(), ..base.clone() }),
            ("name", Info { name: "m".into(), ..base.clone() }),
            ("status", Info { status: super::super::model::Status::Running, ..base.clone() }),
            ("queued", Info { queued: 1, ..base.clone() }),
            ("cost_usd", Info { cost_usd: 0.5, ..base.clone() }),
            ("summary", Info { summary: "said".into(), ..base.clone() }),
            ("doing", Info { doing: Some("reading".into()), ..base.clone() }),
            ("epoch", Info { epoch: 2, ..base.clone() }),
            ("context_pct", Info { context_pct: Some(40), ..base.clone() }),
            ("keep_off", Info { keep_off: true, ..base.clone() }),
            ("no_bypass", Info { no_bypass: true, ..base.clone() }),
            ("parent", Info { parent: Some("p".into()), ..base.clone() }),
            ("last_activity", Info { last_activity: 99, ..base.clone() }),
            ("queue", Info { queue: vec![super::super::queued::Queued::of("q1", "later")], ..base.clone() }),
            ("all three", Info { last_activity: 7, queue: vec![super::super::queued::Queued::of("q1", "x")], said: vec![super::super::model::Given::default()], ..base.clone() }),
        ];
        assert!(!base.differs(&base.clone()), "the same");
        for (what, other) in &changes {
            assert_eq!(base.differs(other), was(&base, other), "{what}");
            assert_eq!(other.differs(&base), was(other, &base), "{what}, the other way");
        }
        assert!(changes.iter().filter(|(w, _)| ["last_activity", "queue", "all three"].contains(w)).all(|(_, o)| !base.differs(o)), "what lists don't show");
        assert!(changes.iter().filter(|(w, _)| !["last_activity", "queue", "all three"].contains(w)).all(|(_, o)| base.differs(o)), "what they show");
    }

    fn message(id: &str, text: &str) -> Command {
        Command::Send { id: id.into(), text: text.into(), mode: super::super::model::SendMode::Now, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
    }

    #[test]
    fn w20_r01_driver_delivers_installed_skills_and_revokes_them() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let sessions = crate::skills::place::root().join("sessions");
            std::fs::create_dir_all(&sessions)?;
            let folder = tempfile::tempdir_in(&sessions)?;
            let id = folder.path().file_name().ok_or_else(|| anyhow::anyhow!("no session folder"))?.to_string_lossy().into_owned();
            let skill = folder.path().join("tools/skills/audit");
            std::fs::create_dir_all(&skill)?;
            std::fs::write(skill.join("SKILL.md"), "---\nname: audit\ndescription: Audit a diff.\n---\nCheck the invariants.")?;
            let (s, _) = idle(&id);
            s.info.lock().kind = "devin".into();
            let (sink, _) = async_channel::unbounded();
            let sink: Sink = sink.into();
            let (stdin, delivered) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
            let prompt = serde_json::json!({"id": 9, "method":"session/prompt", "params":{"sessionId":"native", "prompt":[{"type":"text", "text":"/tools:audit the diff"}]}});
            s.core.lock().write(&prompt);
            flush(&s, &sink, Some(&stdin)).await;
            let line: serde_json::Value = serde_json::from_slice(&delivered.recv().await?)?;
            anyhow::ensure!(line.get("id") == Some(&serde_json::json!(9)));
            anyhow::ensure!(line.to_string().contains("Check the invariants."), "the actual driver omitted the selected instructions");
            std::fs::remove_dir_all(folder.path().join("tools"))?;
            // Removed as kiln removes a skill: its sessions told (skills::reload's SkillsReload), as
            // the bridge keeps the catalogue it read until then.
            s.core.lock().skills_changed();
            s.core.lock().write(&prompt);
            flush(&s, &sink, Some(&stdin)).await;
            let line: serde_json::Value = serde_json::from_slice(&delivered.recv().await?)?;
            anyhow::ensure!(!line.to_string().contains("Check the invariants."), "removal left old instructions in a later delivery");
            Ok(())
        })
    }

    #[test]
    fn mc_attention_owned_change_cannot_overwrite_a_newer_native_turn()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let(s,commands)=idle("single-state-publisher");
            {let mut core=s.core.lock();core.state.status=Status::Idle;core.touch_state();}
            s.publish_owned_change().await.map_err(anyhow::Error::msg)?;
            // A newer native turn is published before the queued owned notification.
            {let mut core=s.core.lock();core.state.status=Status::Running;core.touch_state();}
            let(sink,updates)=async_channel::unbounded();let sink:Sink=sink.into();flush(&s,&sink,None).await;
            assert!(matches!(commands.recv().await?,Ctl::OwnedChanged));
            flush(&s,&sink,None).await;
            let mut states=Vec::new();while let Ok((_,update))=updates.try_recv(){if let Update::State{state}=update{states.push(state.status);}}
            assert_eq!(states,vec![Status::Running],"the deferred notification carries no old Idle snapshot");Ok(())
        })
    }
    #[test]
    fn cp_t3_2_headroom_grants_wake_queued_work_once_and_survive_an_empty_queue() {
        let (s, _rx) = idle("headroom-granted-before-queue");
        s.core.lock().session_id = s.id.clone();
        let admission = crate::headroom::tests::Admission::held(&s.id);
        admission.grant();
        assert!(held_room(&s.core).now_or_never().is_none(), "an empty queue never wakes for a grant");
        s.core.lock().state.queue.push(super::super::model::QueueItem { id: "first".into(), text: "start the work".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        assert!(held_room(&s.core).now_or_never().is_some(), "the grant stays available until there is work to wake");
        assert!(held_room(&s.core).now_or_never().is_none(), "each grant wakes the queue once");

        let (s, _rx) = idle("headroom-granted-after-listening");
        s.core.lock().session_id = s.id.clone();
        s.core.lock().state.queue.push(super::super::model::QueueItem { id: "second".into(), text: "continue the work".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        let admission = crate::headroom::tests::Admission::held(&s.id);
        let room = held_room(&s.core);
        futures::pin_mut!(room);
        assert!(room.as_mut().now_or_never().is_none(), "held work waits for its grant");
        admission.grant();
        assert!(room.as_mut().now_or_never().is_some(), "a grant wakes a listener that was already waiting");
        assert!(held_room(&s.core).now_or_never().is_none(), "the notification also takes the grant so a second wait stays asleep");
    }

    #[test]
    fn a_first_message_its_harness_never_got_stays_in_what_resumes_it_until_it_goes() {
        let (s, _rx) = idle("never-started");
        *s.spec.lock() = Spec { kind: "claude".into(), prompt: Some("fix the flaky test".into()), ..Spec::default() };
        assert_eq!(s.resume_spec().prompt.as_deref(), Some("fix the flaky test"));
        s.prompted.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(s.resume_spec().prompt, None);
    }

    #[cfg(unix)]
    #[test]
    fn cp_t3_2_a_restart_recovers_a_codex_startup_refusal_while_its_process_lives() -> anyhow::Result<()> {
        use serde_json::Value;
        use std::os::unix::fs::PermissionsExt as _;
        crate::rt::block_on(async {
            for failure in ["initialize", "thread/start"] {
                let dir = tempfile::tempdir()?;
                let program = dir.path().join("codex.py");
                let requests = dir.path().join("requests.jsonl");
                std::fs::write(&program, r##"#!/usr/bin/env python3
import json, os, pathlib, sys
mark = pathlib.Path(os.environ["RESTART_MARK"])
first = not mark.exists()
mark.write_text("started")
for line in sys.stdin:
    request = json.loads(line)
    with pathlib.Path(os.environ["RESTART_REQUESTS"]).open("a") as log:
        log.write(json.dumps(request) + "\n")
    if "id" not in request:
        continue
    method = request.get("method")
    if first and method == os.environ["RESTART_FAILURE"]:
        response = {"error": {"message": "startup refused"}}
    elif method == "thread/start":
        response = {"result": {"thread": {"id": "native-thread", "turns": []}}}
    elif method == "turn/start":
        response = {"result": {"turn": {"id": "native-turn"}}}
    else:
        response = {"result": {}}
    print(json.dumps({"id": request["id"], **response}), flush=True)
"##)?;
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
                let (sink, updates) = async_channel::unbounded();
                let files = Files {
                    host: None,
                    dir: dir.path().to_owned(), take: Arc::new(|_| None), spawn: Arc::new(|_| {}), release: Arc::new(|_| {}),
                    carried: Arc::new(|_| Box::pin(async { Ok(Vec::new()) })), fence: Arc::new(|_, _| Box::pin(async { Ok(()) })),
                    settle: Arc::new(|_, _, _| Box::pin(async { Ok(None) })), room: Arc::new(|| true), work: Arc::new(|_, _| {}),
                };
                let s = start(crate::util::new_id(), Spec {
                    kind: "codex".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()),
                    prompt: Some("original task".into()), reach: Some("off".into()),
                    env: vec![("RESTART_MARK".into(), dir.path().join("started").to_string_lossy().into_owned()), ("RESTART_FAILURE".into(), failure.into()), ("RESTART_REQUESTS".into(), requests.to_string_lossy().into_owned())],
                    ..Spec::default()
                }, sink, Arc::new(|| {}), files)?;
                let result = async {
                    loop {
                        if let (_, Update::State { state }) = updates.recv().await? && state.status == Status::Exited {
                            anyhow::ensure!(state.exit.as_ref().and_then(|e| e.error.as_deref()).is_some_and(|e| e.contains("startup refused")), "unexpected startup error");
                            break;
                        }
                    }
                    anyhow::ensure!(s.pid.lock().is_some(), "the refused app-server must still be alive");
                    let original = s.core.lock().state.queue.first().map(|q| q.id.clone()).ok_or_else(|| anyhow::anyhow!("original prompt disappeared"))?;
                    anyhow::ensure!(s.command(Command::Restart) && s.command(Command::Snapshot), "session refused recovery commands");
                    loop {
                        if let (_, Update::Open { info, .. }) = updates.recv().await? {
                            anyhow::ensure!(info.status != Status::Exited, "Restart left the refused app-server running");
                            break;
                        }
                    }
                    loop {
                        if let (_, Update::State { state }) = updates.recv().await? && state.delivery.get(&original) == Some(&super::super::model::Delivery::Started) {
                            anyhow::ensure!(state.queue.is_empty(), "{failure}: original prompt stayed queued after delivery started");
                            break;
                        }
                    }
                    anyhow::Ok(original)
                }.await;
                s.close();
                while !matches!(updates.recv().await?, (_, Update::Closed)) {}
                let original = result?;
                // `State::given` names only kiln's own messages (a goal's word, a parent's news), so
                // this prompt's delivery once is read where it went: the app-server's requests,
                // across both its processes.
                let calls: Vec<Value> = std::fs::read_to_string(&requests)?.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
                let turns: Vec<&Value> = calls.iter().filter(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).collect();
                anyhow::ensure!(turns.len() == 1, "{failure}: expected one turn/start across recovery, got {}", turns.len());
                let turn = turns.first().ok_or_else(|| anyhow::anyhow!("{failure}: original prompt never reached the app-server"))?;
                anyhow::ensure!(turn.pointer("/params/clientUserMessageId").and_then(Value::as_str) == Some(original.as_str()), "{failure}: the recovered turn lost the original prompt id");
                let originals = turn.pointer("/params/input").and_then(Value::as_array).into_iter().flatten()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("text") && item.get("text").and_then(Value::as_str) == Some("original task")).count();
                anyhow::ensure!(originals == 1, "{failure}: the original task reached the app-server {originals} times");
            }
            anyhow::Ok(())
        })
    }

    /// CP-T3.2: a move gathers what a session was sent and hasn't taken (carried while its harness
    /// was down, then still in its channel) into its queue, in the order it came, to go with it; it
    /// holds back everything else; and from then on the session takes no message (its inbox takes
    /// it back, and the sender is told it moved), while commands still go in.
    #[test]
    fn a_move_takes_what_its_session_was_sent_and_lets_no_message_in_after() {
        let (s, rx) = idle("moving");
        s.core.lock().state.queue.push(super::super::model::QueueItem { id: "q0".into(), text: "queued before".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None });
        let mut carry = vec![message("c1", "sent while it was down"), Command::Restart];
        assert!(s.command(message("m1", "in its channel")));
        assert!(s.command(Command::Interrupt));
        assert!(s.command(Command::SkillRun { name: "review".into(), args: Some("the diff".into()), id: Some("k1".into()), mode: Default::default() }));
        let held = gather(&s, &rx, &mut carry);
        let queue: Vec<(String, String)> = s.core.lock().state.queue.iter().map(|q| (q.id.clone(), q.text.clone())).collect();
        assert_eq!(
            queue,
            [("q0", "queued before"), ("c1", "sent while it was down"), ("m1", "in its channel"), ("k1", "/review the diff")].map(|(i, t)| (i.to_owned(), t.to_owned())),
            "in the order they came, after what was queued"
        );
        assert!(matches!(held.as_slice(), [Ctl::Cmd(c)] if matches!(**c, Command::Interrupt)), "the rest held back");
        assert!(matches!(carry.as_slice(), [Command::Restart]), "what isn't a message stays carried");
        assert!(s.is_moving());
        assert!(!s.command(message("m2", "too late")), "no message goes in once it's moving");
        assert!(s.command(Command::Interrupt), "a command still does");
    }

    /// CP-T3.2 (the nineteenth adversarial check's): a move gathers a message for one of its
    /// subagents into that subagent's queue, which goes with the session, never into the agent's
    /// own; one for a subagent that takes none now is shown not sent, there.
    #[test]
    fn a_move_keeps_a_message_for_a_subagent_its_subagent_s() {
        use super::super::model::{Delivery, SendMode, Task};
        let (s, rx) = idle("moving-tasks");
        s.core.lock().state.tasks = vec![
            Task { id: "t1".into(), class: Some("agent".into()), status: "running".into(), takes: vec![SendMode::Queue], ..Task::default() },
            Task { id: "t2".into(), class: Some("agent".into()), status: "completed".into(), ..Task::default() },
        ];
        let to = |id: &str, task: &str| Command::Send { id: id.into(), text: format!("for {task}"), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: Some(task.into()) };
        let mut carry = vec![to("c1", "t1")];
        assert!(s.command(to("m1", "t2")));
        assert!(s.command(message("m2", "for the agent")));
        let _held = gather(&s, &rx, &mut carry);
        let core = s.core.lock();
        let own: Vec<&str> = core.state.queue.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(own, ["m2"], "the agent's own queue holds its own message alone");
        let t1: Vec<&str> = core.state.tasks.iter().find(|t| t.id == "t1").map(|t| t.queue.iter().map(|q| q.id.as_str()).collect()).unwrap_or_default();
        assert_eq!(t1, ["c1"], "the subagent's message waits in its queue, which goes with the session");
        assert!(matches!(core.state.delivery.get("m1"), Some(Delivery::Cancelled)), "one for a subagent that takes none is shown not sent: {:?}", core.state.delivery.get("m1"));
    }

    /// A switch carries the same latitude (never more) and the nearest effort the new model takes,
    /// named by the new harness's own choices: what its page shows while the switch waits, rather
    /// than the old harness's (Opus shown with Codex's Full access, an empty effort, Codex's limit).
    /// CP-T3.2 (the merge with main's one runner a machine, and the eighteenth adversarial check's):
    /// a session handed to its machine's newer runner process carries its inbox's ids, as a move
    /// does, its inbox here fenced before it goes: the newer process keeps an inbox of its own, which
    /// would take a message sent again as new. Refused, the fence lifts and it goes on here. With no
    /// word of it, the newer process may run it: it waits, fenced, and its package goes again until
    /// it's known; once a try went without a word, only the newer process's own word ends it (the
    /// server's own refusal doesn't). Its ids unreadable, it doesn't go at all. And (the nineteenth
    /// adversarial check's) where the machine keeps its record of its hand-overs, that says how it
    /// went, whatever the word: a refusal that's an earlier try's, come late while a newer process
    /// took this one, lets go of it; a refusal takes the try back, unless one took it first; with no
    /// word, it looks, and goes on waiting while it isn't decided, or while the record can't be read.
    #[test]
    fn a_session_handed_to_its_machine_s_newer_runner_takes_its_inbox_s_ids_and_waits_to_know() -> anyhow::Result<()> {
        use super::super::handing::NotHanded;
        use super::super::host::Decided;
        let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
        let mv = Move { to: "newer".into(), cwd: String::new(), note: None, picks_up: false, kind: None, model: None, local: true };
        let kept: CarriedIds = Arc::new(|_| Box::pin(async { Ok(vec![("m1".to_owned(), "d1".to_owned()), ("m2".to_owned(), "d2".to_owned())]) }));
        let unreadable: CarriedIds = Arc::new(|_| Box::pin(async { Err("its records are gone".to_owned()) }));
        let unknown = || -> Result<(), NotHanded> { Err(NotHanded::Unknown("the link dropped".into())) };
        let not_passed = || -> Result<(), NotHanded> { Err(NotHanded::NotPassed("no newer process".into())) };
        let refused = || -> Result<(), NotHanded> { Err(NotHanded::Refused("no room".into())) };
        let none = Vec::<Result<Option<Decided>, String>>::new;
        crate::rt::block_on(async move {
            // (its ids, each try's word, what the machine's record says each time it's asked, how
            // it's fenced, whether it went, how many tries, whether each ask takes the try back)
            for (ids, words, settled, fenced, ok, tries, asks) in [
                (&kept, vec![Ok(())], none(), vec![true], true, 1, vec![]),
                (&kept, vec![refused()], none(), vec![true, false], false, 1, vec![true]),
                (&kept, vec![not_passed()], none(), vec![true, false], false, 1, vec![true]),
                (&kept, vec![unknown(), unknown(), Ok(())], none(), vec![true], true, 3, vec![false, false]),
                (&kept, vec![unknown(), not_passed(), refused()], none(), vec![true, false], false, 3, vec![false, false, true]),
                // Its ids unreadable: fenced first, then the fence lifted, and it stays.
                (&unreadable, vec![Ok(())], none(), vec![true, false], false, 0, vec![]),
                // A refusal that's an earlier try's, come late: the record says it was taken over.
                (&kept, vec![refused()], vec![Ok(Some(Decided::Taken))], vec![true], true, 1, vec![true]),
                (&kept, vec![not_passed()], vec![Ok(Some(Decided::Taken))], vec![true], true, 1, vec![true]),
                // Taken back: decided not taken.
                (&kept, vec![refused()], vec![Ok(Some(Decided::Refused))], vec![true, false], false, 1, vec![true]),
                // No word: it looks, until it's decided.
                (&kept, vec![unknown(), unknown()], vec![Ok(None), Ok(Some(Decided::Taken))], vec![true], true, 2, vec![false, false]),
                (&kept, vec![unknown(), refused()], vec![Ok(None), Ok(Some(Decided::Refused))], vec![true, false], false, 2, vec![false, true]),
                // The record unreadable: a refusal isn't taken at its word; it asks again.
                (&kept, vec![refused(), Ok(())], vec![Err("a disk fault".to_owned())], vec![true], true, 2, vec![true]),
                (&kept, vec![unknown(), refused(), refused()], vec![Ok(None), Err("a disk fault".to_owned()), Ok(Some(Decided::Refused))], vec![true, false], false, 3, vec![false, true, true]),
            ] {
                let (s, _rx) = idle("h1");
                let s = Arc::new(s);
                let calls = Arc::new(Mutex::new(Vec::new()));
                let c = Arc::clone(&calls);
                let fence: FenceFor = Arc::new(move |_, on| {
                    c.lock().push(on);
                    Box::pin(async { Ok(()) })
                });
                let words = Arc::new(Mutex::new(std::collections::VecDeque::from(words)));
                let packages = Arc::new(Mutex::new(Vec::new()));
                let (w, got) = (Arc::clone(&words), Arc::clone(&packages));
                let send = move |bytes: Vec<u8>| {
                    got.lock().push(bytes);
                    let word = w.lock().pop_front().unwrap_or(Ok(()));
                    async move { word }
                };
                let record = Arc::new(Mutex::new(std::collections::VecDeque::from(settled)));
                let asked = Arc::new(Mutex::new(Vec::new()));
                let (r, a) = (Arc::clone(&record), Arc::clone(&asked));
                let settle: SettleFor = Arc::new(move |attempt, _, take_back| {
                    a.lock().push((attempt, take_back));
                    let said = r.lock().pop_front().unwrap_or(Ok(None));
                    Box::pin(async move { said })
                });
                let went = hand_to_newer_by(&s, kind, &mv, ids, &fence, &settle, send, |_| async {}).await;
                let sent = std::mem::take(&mut *packages.lock());
                let asked = std::mem::take(&mut *asked.lock());
                let takes: Vec<bool> = asked.iter().map(|(_, t)| *t).collect();
                anyhow::ensure!(went.is_ok() == ok && *calls.lock() == fenced && sent.len() == tries && takes == asks, "{went:?}, fenced {:?}, {} tries, asked {takes:?}", calls.lock(), sent.len());
                let mut attempts = std::collections::HashSet::new();
                for bytes in sent {
                    let pkg: serde_json::Value = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(pkg.get("inbox") == Some(&serde_json::json!(["m1", "m2"])) && pkg.get("inbox_digests") == Some(&serde_json::json!(["d1", "d2"])) && pkg.get("handover") == Some(&serde_json::json!(true)), "{pkg}");
                    attempts.insert(pkg.get("attempt").and_then(serde_json::Value::as_str).map(str::to_owned));
                }
                // Each try the same one, and the one the record is asked of.
                attempts.extend(asked.into_iter().map(|(a, _)| Some(a)));
                anyhow::ensure!(attempts.len() <= 1 && !attempts.contains(&None), "the tries' attempts: {attempts:?}");
            }
            Ok(())
        })
    }

    /// The hand-over review's point 2: a session handed to its machine's newer runner process fences
    /// its inbox, then reads the ids that go with it, then copies what it holds. What it holds once
    /// its ids are read goes, with every id, so no id goes without its message.
    #[test]
    fn a_hand_over_fences_its_inbox_then_reads_its_ids_then_copies_what_it_holds() -> anyhow::Result<()> {
        let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
        let mv = Move { to: "newer".into(), cwd: String::new(), note: None, picks_up: false, kind: None, model: None, local: true };
        crate::rt::block_on(async move {
            let (s, _rx) = idle("h1");
            let s = Arc::new(s);
            let log = Arc::new(Mutex::new(Vec::<String>::new()));
            let l = Arc::clone(&log);
            let fence: FenceFor = Arc::new(move |_, on| {
                l.lock().push(format!("fence {on}"));
                Box::pin(async { Ok(()) })
            });
            // As its ids are read, what it holds is as its lane left it: m1 in its queue.
            let (l, held) = (Arc::clone(&log), Arc::clone(&s));
            let ids: CarriedIds = Arc::new(move |_| {
                l.lock().push("ids".into());
                held.core.lock().state.queue.push(super::super::model::QueueItem { id: "m1".into(), text: "then test it".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None });
                Box::pin(async { Ok(vec![("m1".to_owned(), "d1".to_owned())]) })
            });
            let settle: SettleFor = Arc::new(|_, _, _| Box::pin(async { Ok(None) }));
            let sent = Arc::new(Mutex::new(Vec::new()));
            let got = Arc::clone(&sent);
            let send = move |bytes: Vec<u8>| {
                got.lock().push(bytes);
                async { Ok(()) }
            };
            hand_to_newer_by(&s, kind, &mv, &ids, &fence, &settle, send, |_| async {}).await.map_err(|e| anyhow::anyhow!(e))?;
            anyhow::ensure!(*log.lock() == ["fence true", "ids"], "{:?}", log.lock());
            let bytes = sent.lock().first().cloned().ok_or_else(|| anyhow::anyhow!("nothing went"))?;
            let pkg: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(pkg.pointer("/state/queue/0/id") == Some(&serde_json::json!("m1")) && pkg.get("inbox") == Some(&serde_json::json!(["m1"])), "{pkg}");
            Ok(())
        })
    }

    #[test]
    fn a_switch_carries_the_same_latitude_and_the_nearest_effort_named_by_the_new_harness() -> anyhow::Result<()> {
        use super::{Kind, carried_choices};
        let codex = Kind::get("codex").ok_or_else(|| anyhow::anyhow!("no codex"))?;
        let claude = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude"))?;
        let (mode, effort, modes, efforts) = carried_choices(codex, claude, "full-access", "ultra", true, "opus");
        // Claude offers asking nothing only where Claude Code allows it (not as root outside a
        // sandbox, as CI's containers run): else the most it may.
        let top = if modes.iter().any(|m| m.id == "bypassPermissions") { "bypassPermissions" } else { "auto" };
        assert_eq!((mode.as_deref(), effort.as_deref()), (Some(top), Some("max")));
        assert!(modes.iter().all(|m| m.id != "bypassPermissions" || m.label == "Bypass"), "{modes:?}");
        assert!(efforts.iter().any(|e| e.id == "max"), "{efforts:?}");
        // Without leave to ask nothing: the most it may. Codex's auto: Claude's accept-edits.
        assert_eq!(carried_choices(codex, claude, "full-access", "", false, "opus").0.as_deref(), Some("auto"));
        assert_eq!(carried_choices(codex, claude, "auto", "high", true, "sonnet").0.as_deref(), Some("acceptEdits"));
        // And back: Claude's max is the highest Codex's model takes.
        let (_, effort, _, efforts) = carried_choices(claude, codex, "acceptEdits", "max", true, "gpt-6.1-sol");
        let top = ["max", "xhigh", "high"].into_iter().find(|l| efforts.iter().any(|e| e.id == *l));
        assert_eq!(effort.as_deref(), top, "{efforts:?}");
        Ok(())
    }

    /// A harness that never gets past starting (Devin that can't sign in) is switched out of at
    /// once: no turn is under way to wait for. A turn under way, or an approval, still waits.
    #[test]
    fn a_switch_out_of_a_harness_stuck_starting_goes_at_once() {
        use super::{Status, switch_now};
        let mut core = Core::new();
        core.state.status = Status::Starting;
        assert!(switch_now(&core), "stuck starting");
        core.state.status = Status::Idle;
        assert!(switch_now(&core), "idle");
        core.state.turn_started = Some(1);
        assert!(!switch_now(&core), "a turn under way");
        core.state.turn_started = None;
        core.state.status = Status::Running;
        assert!(!switch_now(&core), "running");
    }

    /// A switch leaves the old harness's to-dos to the note it gives the new one ("The plan as it
    /// stands": the next test): the session goes on with none, so keep-going counts only the list
    /// the new harness keeps, never one it has no way to change (Claude Code without a to-do tool,
    /// after Devin).
    #[test]
    fn a_switch_leaves_the_to_dos_of_the_old_harness_in_its_note() {
        use super::super::model::{State, Todo};
        use super::{Info, for_harness};
        let todo = |text: &str, status: &str| Todo { text: text.into(), status: status.into() };
        let mut st = State { todos: vec![todo("find the cause", "completed"), todo("upgrade the studios", "in_progress"), todo("check them", "pending")], ..State::default() };
        let mut info = Info::default();
        info.apply(&st);
        assert_eq!((info.todos_done, info.todos_total), (1, 3));
        for_harness(&mut st, Some("S2".into()), "opus", Some("auto".into()), Some("max".into()));
        info.apply(&st);
        assert!(st.todos.is_empty(), "{:?}", st.todos);
        assert_eq!((info.todos_done, info.todos_total), (0, 0), "nothing left open for keep-going to count");
    }

    /// A list says what each of its harness's own subagents is doing (a line), never what it
    /// reported, and which subagent asks each request: Mission control's subagent tree draws from
    /// the list, not the agent's whole state.
    #[test]
    fn a_list_says_what_each_subagent_does_and_which_subagent_asks() {
        use super::super::model::{Approval, State, Task, ToolKind};
        use super::Info;
        let long = "x".repeat(500);
        let task = |id: &str, status: &str, last: Option<&str>, summary: Option<&str>| Task {
            id: id.into(),
            description: format!("sub {id}"),
            status: status.into(),
            class: Some("agent".into()),
            last_tool: last.map(str::to_owned),
            summary: summary.map(str::to_owned),
            ..Task::default()
        };
        let ask = Approval {
            id: "ap1".into(),
            tool_id: Some("toolu_1".into()),
            tool_name: "Bash".into(),
            tool: ToolKind::Shell,
            title: "cargo test".into(),
            detail: None,
            input: serde_json::Value::Null,
            command: Some("cargo test".into()),
            cwd: "/w".into(),
            diffs: Vec::new(),
            reason: None,
            always: Vec::new(),
            default_to_no: false,
            questions: Vec::new(),
            plan: None,
            agent: Some("a1".into()),
            at: 5,
        };
        let st = State { tasks: vec![task("a1", "running", Some("Read src/raft/vote.rs"), None), task("a2", "completed", Some(&long), Some(&long))], approvals: vec![ask], ..State::default() };
        let mut info = Info::default();
        info.apply(&st);
        let brief = |id: &str| info.subagents.iter().find(|b| b.id == id);
        assert_eq!(brief("a1").and_then(|b| b.last_tool.as_deref()), Some("Read src/raft/vote.rs"));
        let clipped = brief("a2").and_then(|b| b.last_tool.as_deref()).map_or(0, str::len);
        assert!(clipped > 0 && clipped < long.len(), "what it's doing is clipped to a line: {clipped}");
        let encoded = serde_json::to_value(brief("a2")).unwrap_or_default();
        assert!(encoded.get("summary").is_none(), "what it reported stays out of the list: {encoded}");
        assert_eq!(info.approvals.first().and_then(|a| a.agent.as_deref()), Some("a1"));
    }

    /// Into Devin, Antigravity or another ACP agent too: named by its own modes (kiln's Ask and
    /// Bypass for an agent that says its own only once started), at the same latitude.
    #[test]
    fn a_switch_into_any_harness_is_named_by_its_modes_at_the_same_latitude() -> anyhow::Result<()> {
        use super::{Kind, carried_choices};
        let kind = |id: &str| Kind::get(id).ok_or_else(|| anyhow::anyhow!("no {id}"));
        let (claude, devin, agy, gemini) = (kind("claude")?, kind("devin")?, kind("antigravity")?, kind("gemini")?);
        let (mode, _, modes, _) = carried_choices(claude, devin, "auto", "", true, "");
        assert_eq!(mode.as_deref(), Some("smart"));
        assert!(modes.iter().any(|m| m.id == "bypass" && m.dangerous), "{modes:?}");
        let (mode, effort, modes, _) = carried_choices(devin, agy, "smart", "high", false, "");
        assert_eq!((mode.as_deref(), effort.as_deref()), (Some("accept-edits"), Some("high")));
        assert!(modes.iter().all(|m| !m.dangerous), "no bypass without leave to: {modes:?}");
        let (mode, effort, modes, efforts) = carried_choices(claude, gemini, "bypassPermissions", "high", true, "");
        assert_eq!(modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), [super::super::acp::KILN_ASK, super::super::acp::KILN_BYPASS]);
        assert_eq!((mode.as_deref(), effort), (Some(super::super::acp::KILN_BYPASS), None), "{efforts:?}");
        // Without leave to skip every check: kiln asks.
        assert_eq!(carried_choices(claude, gemini, "bypassPermissions", "", false, "").0.as_deref(), Some(super::super::acp::KILN_ASK));
        // A mode that only reads: nothing that low there, so its own default (kiln asks).
        assert_eq!(carried_choices(claude, gemini, "plan", "", true, "").0, None);
        // And out of one: Devin's plan stays plan in Claude Code.
        assert_eq!(carried_choices(devin, claude, "plan", "", true, "opus").0.as_deref(), Some("plan"));
        Ok(())
    }

    /// A session's page names its settings by its harness's lists before the harness says anything
    /// (started, or switched to it anew): its modes by their names, the effort levels of the model it
    /// runs where this machine has its list, else the harness's. What the harness said stays.
    #[test]
    fn a_harness_s_lists_name_a_session_s_settings_ahead_of_it() -> anyhow::Result<()> {
        use super::super::model::{Meta, ModeOption};
        use super::{Kind, named_ahead};
        let kind = |id: &str| Kind::get(id).ok_or_else(|| anyhow::anyhow!("no {id}"));
        let mut meta = Meta::default();
        named_ahead(&mut meta, kind("codex")?, Some("gpt-6.1-sol"));
        assert!(meta.modes.iter().any(|m| m.id == "untrusted" && m.label == "Ask"), "{:?}", meta.modes);
        assert!(!meta.efforts.is_empty() && !meta.models.is_empty(), "{meta:?}");
        // A model this machine hasn't listed: the harness's levels.
        let mut meta = Meta::default();
        named_ahead(&mut meta, kind("claude")?, Some("claude-unlisted-1"));
        assert!(meta.modes.iter().any(|m| m.id == "acceptEdits" && m.label == "Accept edits"), "{:?}", meta.modes);
        assert_eq!(meta.efforts.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Low", "Medium", "High", "Extra high", "Max"]);
        assert!(meta.speeds.is_empty(), "{:?}", meta.speeds);
        // An ACP agent's models are its own to list (none before it's signed in); its modes are known.
        let mut meta = Meta::default();
        named_ahead(&mut meta, kind("devin")?, None);
        assert!(meta.modes.iter().any(|m| m.id == "smart" && m.label == "Smart") && meta.models.is_empty(), "{meta:?}");
        let said = vec![ModeOption { id: "said".into(), label: "Said".into(), description: String::new(), dangerous: false }];
        let mut meta = Meta { modes: said.clone(), ..Meta::default() };
        named_ahead(&mut meta, kind("claude")?, None);
        assert_eq!(meta.modes, said);
        Ok(())
    }

    /// A conversation goes from any harness into any other: out of Devin's (kiln's timeline, with
    /// the plan as it stands) into Claude Code's transcript and Codex's thread, each command as
    /// their own call; out of Claude Code's own record into Devin's, and out of Devin's into
    /// Antigravity's, in words with the first message; the permission mode at the same latitude.
    #[test]
    fn a_conversation_goes_from_any_harness_into_any_other() -> anyhow::Result<()> {
        use super::super::handoff;
        use super::super::model::{Body, Event, Todo, ToolCall};
        let kind = |id: &str| Kind::get(id).ok_or_else(|| anyhow::anyhow!("no {id}"));
        let (claude, codex, devin, agy, gemini) = (kind("claude")?, kind("codex")?, kind("devin")?, kind("antigravity")?, kind("gemini")?);
        let ev = |id: &str, body: Body| Event { id: id.into(), seq: 0, at: 0, parent: None, by: None, body };
        let said = |text: &str| Body::User { text: text.into(), images: 0, client_id: None, origin: Some("human".into()), attachments: vec![], reply_to: None };
        let tool: ToolCall = serde_json::from_value(serde_json::json!({"tool_id": "t1", "name": "run_shell_command", "tool": "shell", "title": "date -u", "input": {"command": "date -u"}, "status": "done", "output": "Sun Oct 25 12:00:00 UTC 2026", "exit_code": 0, "started": 0}))?;
        let events = vec![
            ev("u1", said("what time is it?")),
            ev("a1", Body::Assistant { text: "Reading the clock.".into(), streaming: false }),
            ev("t1", Body::Tool(Box::new(tool))),
            ev("a2", Body::Assistant { text: "It's noon, UTC.".into(), streaming: false }),
        ];
        let todos = vec![Todo { text: "read the clock once".into(), status: "in_progress".into() }];
        let nothing: Vec<Event> = Vec::new();
        crate::rt::block_on(async move {
            // Out of Devin into Claude Code: a transcript it resumes, the command its own Bash call.
            let c = carried(devin, claude, &[], None, timeline_of(devin, &events, &todos, &[]), "/w", "", "smart", "high", true, "opus", None).await.map_err(anyhow::Error::msg)?;
            let Record::Claude { session, records } = &c.record else { anyhow::bail!("not a transcript") };
            let lines: Vec<String> = records.iter().map(serde_json::Value::to_string).collect();
            anyhow::ensure!(!session.is_empty() && lines.iter().any(|l| l.contains("what time is it?")) && lines.iter().any(|l| l.contains("\"Bash\"") && l.contains("date -u")), "{lines:?}");
            anyhow::ensure!(lines.iter().any(|l| l.contains("began in Devin") && l.contains("- [~] read the clock once")), "the note, with the plan: {lines:?}");
            assert_eq!((c.mode.as_deref(), c.effort.as_deref(), c.dropped, c.empty), (Some("auto"), Some("high"), 0, false));
            // Into Codex: its new thread's history (from kiln's timeline, its own session's id
            // notwithstanding).
            let c = carried(devin, codex, &[], Some("devin-session".into()), timeline_of(devin, &events, &todos, &[]), "/w", "", "ask", "", true, "gpt-6.1-sol", None).await.map_err(anyhow::Error::msg)?;
            let Record::Codex(items) = &c.record else { anyhow::bail!("not a thread's history") };
            let all = serde_json::to_string(items)?;
            anyhow::ensure!(all.contains("<kiln_handoff>") && all.contains("began in Devin and now continues here in Codex") && all.contains("what time is it?") && all.contains("exec_command") && all.contains("date -u") && all.contains("noon"), "{all}");
            assert_eq!(c.mode.as_deref(), Some("untrusted"));
            // Out of Gemini CLI (an ACP agent's own mode, placed by its words) into Claude Code.
            let c = carried(gemini, claude, &[], None, timeline_of(gemini, &events, &todos, &[]), "/w", "gemini-3-pro", "autoEdit", "", true, "opus", None).await.map_err(anyhow::Error::msg)?;
            let Record::Claude { records, .. } = &c.record else { anyhow::bail!("not a transcript") };
            let all = serde_json::to_string(records)?;
            anyhow::ensure!(all.contains("began in Gemini CLI (model gemini-3-pro)") && all.contains("It's noon, UTC."), "{all}");
            assert_eq!(c.mode.as_deref(), Some("acceptEdits"));
            // Out of Claude Code: from its own record (never kiln's timeline) into Devin, in words.
            let dir = tempfile::tempdir()?;
            let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), dir.path().to_string_lossy().into_owned())];
            let recs = handoff::to_claude(&handoff::from_events(&events), None, "S1", "/w", "claude-opus-5-5");
            handoff::write_claude(&env, "/w", "S1", &recs)?;
            anyhow::ensure!(timeline_of(claude, &events, &todos, &[]).is_none() && timeline_of(codex, &events, &todos, &[]).is_none(), "Claude Code's and Codex's own records are read, not kiln's timeline");
            let c = carried(claude, devin, &env, Some("S1".into()), None, "/w", "claude-opus-5-5", "acceptEdits", "max", true, "", None).await.map_err(anyhow::Error::msg)?;
            let Record::Words(text) = &c.record else { anyhow::bail!("not in words") };
            anyhow::ensure!(text.starts_with("<kiln_handoff>\nThis conversation began in Claude Code (model claude-opus-5-5) and now continues here in Devin") && text.contains("each described in words"), "{text}");
            anyhow::ensure!(text.contains("The person said:\nwhat time is it?") && text.contains("You ran `date -u` (exit code 0); it returned:\nSun Oct 25") && text.ends_with("</kiln_handoff>"), "{text}");
            assert_eq!(c.mode.as_deref(), Some("accept-edits"));
            // Its record unreadable: it says so (and the session stays where it is).
            let e = carried(claude, devin, &env, Some("missing".into()), None, "/w", "", "", "", true, "", None).await.err().unwrap_or_default();
            anyhow::ensure!(e.contains("could not read Claude Code's record"), "{e}");
            // Out of Devin into Antigravity: in words too, a move's note after the switch's.
            let c = carried(devin, agy, &[], None, timeline_of(devin, &events, &todos, &[]), "/w", "", "plan", "", true, "", Some("It moved to mac.")).await.map_err(anyhow::Error::msg)?;
            let Record::Words(text) = &c.record else { anyhow::bail!("not in words") };
            anyhow::ensure!(text.contains("continues here in Antigravity CLI") && text.contains("It moved to mac.") && text.contains("- [~] read the clock once"), "{text}");
            assert_eq!(c.mode.as_deref(), Some("plan"));
            // Nothing said yet: nothing to carry.
            let c = carried(gemini, devin, &[], None, timeline_of(gemini, &nothing, &[], &[]), "/w", "", "", "", true, "", None).await.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(c.empty && matches!(c.record, Record::Nothing));
            // A long one goes in words in less room than a harness's own record takes.
            let mut long = vec![ev("u0", said("the goal"))];
            long.extend((0..400).map(|i| ev(&format!("a{i}"), Body::Assistant { text: format!("{i} {}", "x".repeat(1000)), streaming: false })));
            let c = carried(gemini, devin, &[], None, timeline_of(gemini, &long, &[], &[]), "/w", "", "", "", true, "", None).await.map_err(anyhow::Error::msg)?;
            let Record::Words(text) = &c.record else { anyhow::bail!("not in words") };
            anyhow::ensure!(
                c.dropped > 0 && text.len() < handoff::WORDS_BUDGET + 16 * 1024 && text.contains("the goal") && text.contains(&format!("{} older messages were left out", c.dropped)) && text.contains(&format!("[{} earlier message(s)", c.dropped)),
                "{} bytes, {} left out",
                text.len(),
                c.dropped
            );
            let c = carried(gemini, claude, &[], None, timeline_of(gemini, &long, &[], &[]), "/w", "", "", "", true, "", None).await.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(c.dropped == 0, "{} left out of Claude Code's", c.dropped);
            Ok(())
        })
    }

    /// CP-T3.2: ACP shows startup input before handing it over. A switch or move carries the
    /// genuine conversation as history and the unsent input only in its durable queue.
    #[test]
    fn cp_t3_2_shown_queued_input_is_not_carried_as_conversation_and_sent_again() -> anyhow::Result<()> {
        use super::super::handoff::Entry;
        use super::super::model::{Body, Delivery};
        crate::rt::block_on(async {
            let source = Kind::get("devin").ok_or_else(|| anyhow::anyhow!("no source harness"))?;
            let mut core = Core::new();
            core.state.cwd = "/work".into();
            core.upsert("past", None, Body::User { text: "past-request".into(), images: 0, client_id: Some("past".into()), origin: None, attachments: Vec::new(), reply_to: None });
            core.upsert("answer", None, Body::Assistant { text: "past-answer".into(), streaming: false });
            // A harness's own queued label doesn't make already handed input kiln's queued input.
            core.state.delivery.insert("past".into(), Delivery::Queued);
            let mut before = source.adapter(&Spec { kind: source.id.into(), ..Spec::default() });
            before.start(&mut core);
            before.command(&mut core, Command::Send { id: "pending".into(), text: "pending-only".into(), mode: super::super::model::SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None });
            assert!(core.get("pending").is_some());
            assert_eq!(core.state.delivery.get("pending"), Some(&Delivery::Queued));
            let timeline = timeline_of(source, core.events(), &core.state.todos, &core.state.queue).ok_or_else(|| anyhow::anyhow!("no carried timeline"))?;
            assert_eq!(timeline.entries, [Entry::User { text: "past-request".into(), images: Vec::new() }, Entry::Assistant("past-answer".into())]);
            for destination in ["claude", "codex", "antigravity"] {
                let to = Kind::get(destination).ok_or_else(|| anyhow::anyhow!("no {destination}"))?;
                let carried = carried(source, to, &[], None, Some(timeline.clone()), "/work", "", "", "", true, "", None).await.map_err(anyhow::Error::msg)?;
                let history = match carried.record {
                    Record::Claude { records, .. } => serde_json::to_string(&records)?,
                    Record::Codex(items) => serde_json::to_string(&items)?,
                    Record::Words(text) => text,
                    Record::Nothing => anyhow::bail!("{destination} lost the sent conversation"),
                };
                anyhow::ensure!(history.contains("past-request") && history.contains("past-answer") && !history.contains("pending-only"), "{destination} carried queued input as history: {history}");
            }
            let history: Vec<_> = handed_events(core.events(), &core.state.queue).into_iter().cloned().collect();
            for carry in [super::super::moving::words(&history, "handover"), super::super::moving::record(&source.protocol, &[], None, &history, "move").map_err(anyhow::Error::msg)?] {
                let super::super::moving::Carry::Words { text } = carry else { anyhow::bail!("moving lost the sent conversation") };
                anyhow::ensure!(text.contains("past-request") && text.contains("past-answer") && !text.contains("pending-only"), "moving carried queued input as history: {text}");
            }
            assert_eq!(core.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["pending"]);
            assert!(core.get("pending").is_some(), "carrying changed the displayed timeline");
            before.exited(&mut core, None, None);
            core.take_writes();
            core.state.exit = None;
            core.set_status(Status::Starting);
            let destination = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no destination harness"))?;
            let mut after = destination.adapter(&Spec { kind: destination.id.into(), ..Spec::default() });
            after.start(&mut core);
            let writes = core.take_writes().iter().map(|line| serde_json::from_str::<serde_json::Value>(line)).collect::<Result<Vec<_>, _>>()?;
            let initialize = writes.iter().find(|v| v.pointer("/request/subtype").and_then(serde_json::Value::as_str) == Some("initialize")).ok_or_else(|| anyhow::anyhow!("no Claude initialization"))?;
            after.line(&mut core, &serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": initialize.get("request_id"), "response": {}}}).to_string());
            let writes = core.take_writes().iter().map(|line| serde_json::from_str::<serde_json::Value>(line)).collect::<Result<Vec<_>, _>>()?;
            let prompts: Vec<_> = writes.iter().filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("user")).collect();
            assert_eq!(prompts.len(), 1);
            assert_eq!(prompts.first().and_then(|v| v.get("uuid")), Some(&serde_json::json!("pending")));
            assert_eq!(prompts.first().and_then(|v| v.pointer("/message/content")), Some(&serde_json::json!("pending-only")));
            assert!(core.state.queue.is_empty());
            after.drain(&mut core);
            after.tick(&mut core);
            assert!(!core.take_writes().iter().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).any(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("user")), "queued input was sent twice");
            Ok(())
        })
    }

    #[test]
    fn a_turn_stays_open_until_it_ends_not_when_its_harness_does() {
        use super::turn_open;
        assert!(turn_open(Status::Running, false));
        assert!(turn_open(Status::Waiting, false));
        // Its harness ends under it (a restart, a crash) and starts again: still open.
        assert!(turn_open(Status::Exited, true));
        assert!(turn_open(Status::Starting, true));
        assert!(turn_open(Status::Disconnected, true));
        // It ended: closed, and the harness ending after that doesn't open it.
        assert!(!turn_open(Status::Idle, true));
        assert!(!turn_open(Status::Exited, false));
    }

    #[test]
    fn resolve_cwd_expands_home() {
        let home = crate::os::home_dir();
        assert_eq!(super::resolve_cwd(None), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("~")), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("  ")), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("~/Projects/x")), home.join("Projects/x").to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("/srv/repo")), "/srv/repo");
    }
    use super::*;

    /// A session that went on in Claude Code with a Codex subagent kiln never read: its work comes
    /// from Codex's own record of its thread, off the event loop, into the running session.
    #[cfg(unix)]
    #[test]
    fn a_codex_child_never_read_is_read_from_its_rollout_in_the_harness_after_a_switch() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let quiet = dir.path().join("quiet-claude");
            std::fs::write(&quiet, "#!/bin/sh\nexec sleep 30\n")?;
            std::fs::set_permissions(&quiet, std::fs::Permissions::from_mode(0o755))?;
            let thread = "01a15b9d-5652-7722-8f9b-c750ebfe6940";
            let day = dir.path().join("codex/sessions/2026/10/20");
            std::fs::create_dir_all(&day)?;
            let ev = |payload: serde_json::Value| serde_json::json!({"type": "event_msg", "payload": payload}).to_string();
            let rollout = [
                serde_json::json!({"type": "session_meta", "payload": {"id": thread}}).to_string(),
                ev(serde_json::json!({"type": "task_started", "turn_id": "own", "turn_attribution": {"initiating_agent_path": "/root"}, "started_at": 100})),
                ev(serde_json::json!({"type": "item_completed", "turn_id": "own", "item": {"type": "AgentMessage", "id": "msg_answer", "content": [{"type": "Text", "text": "Fits."}]}})),
                ev(serde_json::json!({"type": "task_complete", "turn_id": "own", "completed_at": 103, "duration_ms": 3000})),
            ];
            std::fs::write(day.join(format!("rollout-2026-10-20T16-20-00-{thread}.jsonl")), rollout.join("\n"))?;
            let (tx, _rx) = async_channel::unbounded();
            let host = super::super::host::Host::new(tx, None, super::super::host::Limits::default());
            let home = dir.path().join("codex").to_string_lossy().into_owned();
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(quiet.to_string_lossy().into_owned()),
                env: vec![("CODEX_HOME".into(), home)],
                ..Spec::default()
            };
            let s = host.open(Some("switched".into()), spec)?;
            let started = crate::rt::timeout(Duration::from_secs(10), async {
                while s.core.lock().meta.features.is_empty() {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            anyhow::ensure!(started.is_ok(), "the harness never started");
            s.core.lock().state.tasks.push(Task { id: thread.into(), tool_id: Some("call_spawn".into()), class: Some("agent".into()), status: "completed".into(), ..Task::default() });
            read_unread_children(&s);
            let read = crate::rt::timeout(Duration::from_secs(10), async {
                while s.core.lock().get(&format!("child:{thread}:msg_answer")).is_none() {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            anyhow::ensure!(read.is_ok(), "its work never came");
            let core = s.core.lock();
            assert_eq!(core.get(&format!("child:{thread}:msg_answer")).and_then(|e| e.parent.clone()).as_deref(), Some("call_spawn"));
            assert_eq!(core.state.tasks.iter().find(|t| t.id == thread).and_then(|t| t.summary.clone()).as_deref(), Some("Fits."));
            drop(core);
            host.close("switched");
            Ok(())
        })
    }

    /// CP-T3.2: Codex crashes before opening its thread; its unsent message stays in kiln's
    /// durable queue. A message sent while it is down starts it again, behind the first.
    #[cfg(unix)]
    #[test]
    fn what_a_crashed_harness_held_goes_first_when_its_agent_starts_again() -> anyhow::Result<()> {
        use super::super::model::SendMode;
        use std::os::unix::fs::PermissionsExt;
        // Each wait's bound only ends a hang: a loaded machine starts, crashes and starts again a shell's stand-in
        // in what time it takes (10 s ran out under load: "its start never came", "the crash never came").
        async fn until(s: &Session, what: &str, done: impl Fn(&Core) -> bool) -> anyhow::Result<()> {
            let came = crate::rt::timeout(Duration::from_secs(300), async {
                while !done(&s.core.lock()) {
                    crate::rt::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            let core = s.core.lock();
            anyhow::ensure!(came.is_ok(), "{what} never came: {:?}, its queue {:?}", core.state.status, core.state.queue.iter().map(|q| &q.id).collect::<Vec<_>>());
            Ok(())
        }
        let queue = |c: &Core| c.state.queue.iter().map(|q| q.id.clone()).collect::<Vec<_>>();
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            // A Codex that never opens a thread, and ends (code 3) a moment after it's told to.
            let crash = dir.path().join("crash");
            let codex = dir.path().join("stuck-codex");
            std::fs::write(&codex, format!("#!/bin/sh\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nsleep 0.3\nexit 3\n", crash.display()))?;
            std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))?;
            let (tx, _rx) = async_channel::unbounded();
            let host = super::super::host::Host::new(tx, None, super::super::host::Limits::default());
            let spec = Spec { kind: "codex".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(codex.to_string_lossy().into_owned()), ..Spec::default() };
            let s = host.open(Some("held".into()), spec)?;
            let send = |id: &str| Command::Send { id: id.into(), text: format!("{id}'s words"), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            until(&s, "its start", |c| !c.meta.features.is_empty()).await?;
            anyhow::ensure!(s.command(send("m1")));
            until(&s, "m1 held", |c| queue(c) == ["m1"]).await?;
            std::fs::write(&crash, "")?;
            until(&s, "the crash", |c| c.state.status == Status::Exited).await?;
            assert_eq!(queue(&s.core.lock()), ["m1"]);
            // Sent while it's down, which starts it again (it ends at once, as it's told to).
            anyhow::ensure!(s.command(send("m2")));
            until(&s, "m2 behind m1", |c| queue(c) == ["m1", "m2"]).await?;
            until(&s, "its second start", |c| c.state.exit.is_none()).await?;
            until(&s, "its second end", |c| c.state.status == Status::Exited).await?;
            let core = s.core.lock();
            assert_eq!(queue(&core), ["m1", "m2"]);
            assert_eq!((core.state.delivery.get("m1"), core.state.delivery.get("m2")), (None, None));
            assert!(core.get("m1").is_none() && core.get("m2").is_none(), "neither process received the queued messages");
            drop(core);
            host.close("held");
            Ok(())
        })
    }

    #[test]
    fn an_ended_agent_shows_nothing_still_running() {
        let row = |id: &str, status: &str| Task { id: id.into(), tool_id: Some(format!("call-{id}")), class: Some("shell".into()), status: status.into(), ..Task::default() };
        let mut core = Core::new();
        core.state.tasks = vec![row("terminal-dev", "running"), row("left-4242", "running"), Task { ended: Some(7), ..row("terminal-done", "completed") }];
        core.upsert("call-terminal-dev", None, super::super::model::Body::Assistant { text: "Serving".into(), streaming: true });
        end_tasks(&mut core);
        assert_eq!(core.state.tasks.iter().map(|t| (t.status.as_str(), t.ended.is_some())).collect::<Vec<_>>(), [("stopped", true), ("stopped", true), ("completed", true)]);
        assert_eq!(core.state.tasks.last().and_then(|t| t.ended), Some(7));
        assert!(core.get("call-terminal-dev").is_some_and(|e| matches!(&e.body, super::super::model::Body::Assistant { streaming: false, .. })));
        assert!(core.take_out().iter().any(|u| matches!(u, Update::State { state } if state.tasks.iter().all(|t| !t.live()))));
    }

    /// CP-T3.2: an agent ended with a message still waiting to go (held for a harness that had
    /// ended, shown queued where it was said) says it was dropped, never queued for good; what it
    /// finished stays done.
    #[test]
    fn an_ended_agent_says_what_never_went_was_dropped() {
        use super::super::model::{Delivery, QueueItem};
        let mut core = Core::new();
        core.state.queue.push(QueueItem { id: "m2".into(), text: "and a search".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None });
        core.state.delivery = [("m1".to_owned(), Delivery::Completed), ("m2".to_owned(), Delivery::Queued)].into_iter().collect();
        end_tasks(&mut core);
        assert_eq!((core.state.delivery.get("m1"), core.state.delivery.get("m2")), (Some(&Delivery::Completed), Some(&Delivery::Cancelled)));
    }

    #[test]
    fn info_lists_native_subagents_and_keeps_their_finished_work() -> anyhow::Result<()> {
        let native = |id: &str, kind: &str, status: &str| Task {
            id: id.into(),
            tool_id: Some(format!("spawn-{id}")),
            description: format!("Review {id}"),
            kind: kind.into(),
            class: Some("agent".into()),
            status: status.into(),
            started: 10,
            ..Task::default()
        };
        let mut state = State {
            tasks: vec![
                native("claude-child", "general-purpose", "running"),
                Task {
                    parent_task: Some("claude-child".into()),
                    ended: Some(30),
                    prompt: Some("The full task prompt".into()),
                    summary: Some("A long result".into()),
                    output: Some("/tmp/private-output".into()),
                    ..native("codex-thread", "reviewer", "completed")
                },
                native("failed-child", "", "failed"),
                native("stopped-child", "", "stopped"),
                Task {
                    class: None,
                    ..native("legacy-child", "Explore", "paused")
                },
            ],
            ..State::default()
        };
        for class in ["shell", "monitor", "workflow", "mcp"] {
            state.tasks.push(Task {
                class: Some(class.into()),
                ..native(class, "", "running")
            });
        }
        for kind in [
            "local_bash",
            "monitor",
            "monitor_background",
            "local_workflow",
            "mcp_task",
        ] {
            state.tasks.push(Task {
                class: None,
                ..native(kind, kind, "running")
            });
        }
        let mut info = Info::default();
        info.apply(&state);
        assert_eq!(
            info.subagents
                .iter()
                .map(|s| (s.id.as_str(), s.status.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("claude-child", "running"),
                ("codex-thread", "completed"),
                ("failed-child", "failed"),
                ("stopped-child", "stopped"),
                ("legacy-child", "paused")
            ],
        );
        let encoded = serde_json::to_value(&info.subagents)?;
        assert_eq!(
            encoded.get(1),
            Some(&serde_json::json!({
                "id": "codex-thread", "tool_id": "spawn-codex-thread", "description": "Review codex-thread",
                "status": "completed", "parent_task": "claude-child", "started": 10, "ended": 30,
            })),
            "list entries identify native work without copying prompts, results or output",
        );
        assert!(
            info.parent.is_none(),
            "native tasks are not kiln session lineage"
        );
        Ok(())
    }

    #[test]
    fn info_subagents_update_from_old_runner_state_and_notify_lists() -> anyhow::Result<()> {
        // A list entry from before subagent briefs still loads; the runner's state fills them.
        let old = serde_json::to_value(Info::default())?;
        assert!(
            old.get("subagents").is_none(),
            "empty briefs stay absent on the wire"
        );
        let mut info: Info = serde_json::from_value(old)?;
        assert!(info.subagents.is_empty());
        let mut state = State {
            tasks: vec![Task {
                id: "child".into(),
                description: "é".repeat(400),
                kind: "Explore".into(),
                status: "pending".into(),
                ..Task::default()
            }],
            ..State::default()
        };
        info.apply(&state);
        assert_eq!(info.subagents.len(), 1);
        assert!(
            info.subagents.iter().all(|s| s.description.len() < 256),
            "list labels stay bounded and valid UTF-8"
        );
        let pending = info.clone();
        for task in &mut state.tasks {
            task.status = "completed".into();
            task.ended = Some(50);
        }
        info.apply(&state);
        assert!(
            info.differs(&pending),
            "finishing a native subagent refreshes Mission Control"
        );
        let finished = info.clone();
        for task in &mut state.tasks {
            task.description = "Renamed native task".into();
        }
        info.apply(&state);
        assert!(
            info.differs(&finished),
            "renaming a native subagent refreshes Mission Control"
        );
        state.tasks.clear();
        info.apply(&state);
        assert!(
            info.subagents.is_empty(),
            "clearing task history clears the disclosure too"
        );
        Ok(())
    }

    #[test]
    fn info_follows_state() {
        let mut i = Info {
            id: "a".into(),
            deferred: Default::default(), queue_partial: false, queue_revision: None, queue_next: None,
            kind: "claude".into(),
            kind_name: "Claude Code".into(),
            target: "local".into(),
            name: "x".into(),
            shell: None,
            cwd: "/w".into(),
            tree: None,
            created_at: 1,
            status: Status::Starting,
            title: String::new(),
            model: String::new(),
            mode: String::new(),
            harness_session: None,
            approvals: vec![],
            queued: 0,
            admission: None,
            queue: Vec::new(),
            said: Vec::new(),
            tasks_running: 0,
            subagents: Vec::new(),
            cost_usd: 0.0,
            activity: None,
            last_activity: 1,
            exit: None,
            goal: String::new(),
            objective: None,
            summary: String::new(),
            doing: None,
            epoch: 0,
            todos_done: 0,
            todos_total: 0,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            context_pct: None,
            turn_started: None,
            limits: vec![],
            account: None,
            plan: None,
            left_behind: 0,
            reach: None,
            pause: None,
            frozen: None,
            variant_of: None,
            perms: None,
            room_wait: None,
            last_turn: None,
            keep_off: false,
            held: None,
            refused: None,
            move_on_limit: false,
            move_plan: Default::default(),
            pause_at: None,
            max_subagents: None,
            parent: None,
            plan_run: None,
            helper: None,
            subagent: false,
            no_bypass: false,
            workspace: None,
            isolation: None,
        };
        let before = i.clone();
        let mut st = State { status: Status::Running, mode: "plan".into(), ..State::default() };
        st.tasks.push(super::super::model::Task { id: "t".into(), status: "running".into(), ..Default::default() });
        i.apply(&st);
        assert_eq!((i.status, i.mode.as_str(), i.tasks_running, i.cwd.as_str()), (Status::Running, "plan", 1, "/w"));
        assert!(i.differs(&before));
        let mut later = i.clone();
        later.last_activity = 99;
        assert!(!later.differs(&i));

        // A name given to it reaches lists through its state; none given leaves the one it had.
        let named = i.clone();
        i.apply(&State { name: "Docs fixer".into(), ..State::default() });
        assert_eq!(i.name, "Docs fixer");
        assert!(i.differs(&named));
        i.apply(&State::default());
        assert_eq!(i.name, "Docs fixer");

        // Where its commands run reaches lists through its state too, and reads as said.
        i.apply(&State { reach: Some("off".into()), ..State::default() });
        assert_eq!(i.reach.as_deref(), Some("off"));
        i.apply(&State::default());
        assert_eq!(i.reach, None, "absent: anywhere it needs");
        assert_eq!(reach_words(None), "Its commands can run on any runner it needs");
        assert_eq!(reach_words(Some("off")), "Its commands run only on its own machine");
        assert_eq!(reach_words(Some("linux/amd64")), "Its commands can run on other runners, linux/amd64 first");
    }

    /// What a machine's agent limit counts (docs/agents.md, "How many agents a machine runs"): a
    /// session at work, as Mission control shows it working or needing you in the middle of a turn;
    /// not one finished, ready, paused, ended or away. Each as its state says it, through
    /// `Info::apply` as a runner and the server's mirror both see it.
    #[test]
    fn only_a_session_at_work_counts_toward_its_machines_limit() {
        use super::super::model::{Pause, Task};
        let at_work = |state: State| {
            let mut i = Info::default();
            i.apply(&state);
            i.at_work()
        };
        let of = |status: Status| State { status, ..State::default() };
        let paused = || Some(Pause { reason: "limit".into(), ..Pause::default() });
        let subagent = |status: &str| Task { id: "t1".into(), kind: "local_agent".into(), class: Some("agent".into()), status: status.into(), description: "looks".into(), ..Task::default() };
        assert!(at_work(of(Status::Starting)), "starting");
        assert!(at_work(of(Status::Running)), "on a turn");
        assert!(at_work(of(Status::Waiting)), "waiting on a person in the middle of a turn");
        assert!(at_work(State { pause: paused(), ..of(Status::Running) }), "a turn a usage limit stops at its next step goes on till then");
        assert!(!at_work(of(Status::Idle)), "finished, or ready");
        assert!(!at_work(of(Status::Exited)), "ended, or stopped");
        assert!(!at_work(of(Status::Disconnected)), "its machine away");
        assert!(!at_work(State { pause: paused(), ..of(Status::Idle) }), "paused at a usage limit");
        assert!(!at_work(State { pause: paused(), ..of(Status::Starting) }), "paused, starting again");
        let mut held = Info::default();
        held.apply(&of(Status::Idle));
        held.held = Some(Held { why: "Paused by the agent: a plan to look at".into(), plan: None, since: 1 });
        assert!(!held.at_work(), "its goals paused for a person");
        // Its turn over, subagents it sent off still at work keep it at work; finished ones don't,
        // nor what it left running that isn't a subagent.
        for s in ["running", "pending", "paused"] {
            assert!(at_work(State { tasks: vec![subagent(s)], ..of(Status::Idle) }), "a subagent {s}");
        }
        for s in ["completed", "failed", "stopped", "killed"] {
            assert!(!at_work(State { tasks: vec![subagent(s)], ..of(Status::Idle) }), "a subagent {s}");
        }
        let shell = Task { class: Some("shell".into()), kind: "local_bash".into(), ..subagent("running") };
        assert!(!at_work(State { tasks: vec![shell], ..of(Status::Idle) }), "a shell left running");
        assert!(!at_work(State { tasks: vec![subagent("running")], pause: paused(), ..of(Status::Idle) }), "paused, its subagents too");
    }

    #[test]
    fn only_loopback_is_fetched() {
        assert_eq!(loopback_target("http://localhost:41234/callback?code=a&state=b"), Some(("localhost".into(), 41234, "/callback?code=a&state=b".into())));
        assert_eq!(loopback_target("http://127.0.0.1:9/?x=1#frag"), Some(("127.0.0.1".into(), 9, "/?x=1".into())));
        assert_eq!(loopback_target("http://[::1]:7/cb"), Some(("::1".into(), 7, "/cb".into())));
        assert_eq!(loopback_target("http://LOCALHOST?code=1"), Some(("localhost".into(), 80, "/?code=1".into())));
        for bad in ["https://localhost/cb", "http://example.com/cb", "http://localhost.example.com/cb", "http://user@localhost/cb", "http://localhost:1/a b", "http://localhost:99999/", "file:///etc/passwd", "http://10.0.0.1/"] {
            assert_eq!(loopback_target(bad), None, "{bad}");
        }
        assert_eq!(fetch_loopback("http://example.com/").0, None);
    }

    #[test]
    fn a_callback_on_loopback_is_fetched() -> anyhow::Result<()> {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> Option<String> {
            let (conn, _) = listener.accept().ok()?;
            let mut reader = std::io::BufReader::new(conn.try_clone().ok()?);
            let mut first = String::new();
            reader.read_line(&mut first).ok()?;
            let mut conn = conn;
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nsigned in").ok()?;
            Some(first)
        });
        let (code, body) = fetch_loopback(&format!("http://127.0.0.1:{port}/callback?code=abc&state=s"));
        assert_eq!((code, body.as_str()), (Some(200), "signed in"));
        let first = server.join().map_err(|_| anyhow::anyhow!("server thread"))?.unwrap_or_default();
        assert_eq!(first.trim_end(), "GET /callback?code=abc&state=s HTTP/1.1");
        Ok(())
    }

    #[test]
    fn reads_are_markdown_files_only() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-read-{}", crate::util::new_id()));
        std::fs::create_dir_all(dir.join("skill"))?;
        let md = dir.join("skill").join("SKILL.md");
        std::fs::write(&md, "# hi")?;
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "no")?;
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        let missing = dir.join("missing").join("SKILL.md");
        assert_eq!(read_first(&[p(&missing), p(&md)]).get("content").and_then(serde_json::Value::as_str), Some("# hi"));
        assert!(read_first(&[p(&secret)]).get("error").is_some());
        assert!(read_first(&["relative/SKILL.md".to_owned()]).get("error").is_some());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_star_matches_the_newest_directory_first() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-expand-{}", crate::util::new_id()));
        for (bucket, text) in [("old", "old"), ("new", "new")] {
            std::fs::create_dir_all(dir.join("synced").join(bucket).join("docs"))?;
            std::fs::write(dir.join("synced").join(bucket).join("docs").join("SKILL.md"), text)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        let pattern = dir.join("synced").join("*").join("docs").join("SKILL.md").to_string_lossy().into_owned();
        assert_eq!(read_first(&[pattern]).get("content").and_then(serde_json::Value::as_str), Some("new"));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_takeover_waits_for_the_process_to_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut short = std::process::Command::new("sleep").arg("0.3").spawn()?;
            let pid = short.id();
            // Reaped as soon as it exits (as the taken-over session's parent would): gone however long a loaded
            // machine takes to end it (the bound only ends a hang).
            let reaper = std::thread::spawn(move || short.wait());
            assert!(gone(pid, Duration::from_secs(300)).await);
            let _ = reaper.join();
            // One that runs on isn't gone: said once its wait is over, never before (a pace under 3 s failed under
            // load), and the wait ends (the bound around it only ends a hang).
            let mut long = std::process::Command::new("sleep").arg("30").spawn()?;
            let t = Instant::now();
            let ended = crate::rt::timeout(Duration::from_secs(300), gone(long.id(), Duration::from_millis(400))).await;
            assert_eq!(ended, Ok(false), "a process still running is never gone, and the wait ends");
            assert!(t.elapsed() >= Duration::from_millis(400), "said before its wait was over: {:?}", t.elapsed());
            let _ = long.kill();
            let _ = long.wait();
            anyhow::Ok(())
        })
    }

    #[test]
    fn native_subagent_output_keeps_results_after_history_eviction_and_bounds_snapshots() {
        use super::super::model::Body;
        let mut core = Core::new();
        let mut task = Task { id: "child".into(), tool_id: Some("spawn".into()), class: Some("agent".into()), status: "completed".into(), ..Task::default() };
        // An empty completed child is unavailable, not a successful empty file read.
        assert!(codex_task_output(&core, &task).get("error").is_some());
        task.summary = Some("The completed result remains available.".into());
        let result = codex_task_output(&core, &task);
        assert_eq!(result.get("data").and_then(serde_json::Value::as_str), task.summary.as_deref());
        assert_eq!(result.get("live").and_then(serde_json::Value::as_bool), Some(false));
        // Newest output is bounded; the final answer is present once even when Task.summary
        // contains its clipped representation.
        let long = format!("{}Final result.", "x".repeat(TASK_TAIL as usize + 20));
        task.summary = Some(super::super::model::clip_middle(&long, super::super::model::OUTPUT_MAX).0);
        core.upsert("child:child:answer", Some("spawn".into()), Body::Assistant { text: long.clone(), streaming: false });
        let result = codex_task_output(&core, &task);
        let text = result.get("data").and_then(serde_json::Value::as_str).unwrap_or_default();
        assert_eq!(text.len(), TASK_TAIL as usize);
        assert!(text.ends_with("Final result."));
        assert_eq!(text.matches("Final result.").count(), 1);
        assert_eq!(result.get("offset").and_then(serde_json::Value::as_u64), Some(long.len() as u64));
        assert_eq!(result.get("truncated").and_then(serde_json::Value::as_bool), Some(true));
    }

    #[test]
    fn task_output_files_and_their_tails() -> anyhow::Result<()> {
        let env = vec![("CLAUDE_CODE_TMPDIR".to_owned(), "/tmp/cc".to_owned())];
        // A path of this machine's, its separators this platform's.
        let file = std::path::Path::new("/tmp/cc").join("-home-u-my-app").join("s1").join("tasks").join("b7.output");
        assert_eq!(claude_task_file(&env, "/home/u/my app", Some("s1"), "b7").as_deref(), file.to_str());
        assert!(claude_task_file(&env, "/w", Some("s1"), "../x").is_none());
        assert!(claude_task_file(&env, "/w", None, "b7").is_none());
        let dir = std::env::temp_dir().join(format!("kiln-tail-{}", crate::util::new_id())).join("tasks");
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("b7.output");
        std::fs::write(&f, "one\ntwo\n")?;
        assert_eq!(read_tail(&f, None), Some(("one\ntwo\n".to_owned(), 8, false)));
        assert_eq!(read_tail(&f, Some(4)), Some(("two\n".to_owned(), 8, false)));
        // Shorter than asked for (written again from the start): from the start.
        assert_eq!(read_tail(&f, Some(100)), Some(("one\ntwo\n".to_owned(), 8, false)));
        std::fs::remove_dir_all(dir.parent().unwrap_or(&dir))?;
        Ok(())
    }
}
