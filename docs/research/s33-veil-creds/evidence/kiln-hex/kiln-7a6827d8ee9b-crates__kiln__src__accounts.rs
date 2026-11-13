//! Signing a machine's harnesses in and out from kiln (Claude Code, Codex, Devin, Cursor, Pi), so
//! `/login` and `/logout` work from the console: each harness's own CLI (`claude auth …`, `codex
//! login …`, `devin auth …`, Cursor's `agent login`, `pi auth …`) runs in a terminal kiln opens on
//! that machine for the purpose (never listed as anyone's), and
//! what a sign-in asks for (a page to open, a code to enter there, a code or an API key to give
//! back) is read from its output and shown in the console. What is typed in goes to the CLI only:
//! it is never logged, kept once the sign-in ends, or shown again.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

use crate::hub::Hub;
use crate::termsess::{Frame, Input, Session};

/// Marks a terminal kiln opened for an account: never listed ([`crate::termsess::hidden`]).
pub const MARK: &str = "# kiln-account ";
/// How long a sign-in waits for the person (a device code lives 15 minutes).
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a status check or a sign-out may take.
const QUICK_TIMEOUT: Duration = Duration::from_secs(45);
/// Sign-ins kept for the console to read (the newest; a running one is never dropped).
const KEEP: usize = 16;
/// Wide enough that a CLI never wraps a sign-in URL to fit.
const COLS: u16 = 1000;
/// Output read per flow (the start, where the URL and code are).
const OUTPUT_MAX: usize = 64 * 1024;
/// Lines of output a flow shows.
const LINES: usize = 12;
/// What Claude Code prints when it waits for the code its sign-in page shows.
const CLAUDE_PASTE: &str = "Paste code here";
/// Its whole prompt: dropped from the output shown (the console asks for the code instead).
const CLAUDE_PROMPT: &str = "Paste code here if prompted >";
/// Devin's manual sign-in waits for the code its page shows.
const DEVIN_PASTE: &str = "Paste the code from the sign-in page";
/// Pi's Claude sign-in without a browser (`pi auth login anthropic --no-browser`, its "Copy code
/// login") waits for the code Anthropic's page shows.
const PI_PASTE: &str = "Paste the code Anthropic shows after you sign in";

/// A way to sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AccountMethod"))]
pub struct Method {
    pub id: &'static str,
    pub label: &'static str,
    /// What it asks to be given back: `"code"` (the sign-in page shows it; a browser on the same
    /// machine may finish without one) or `"api_key"`. Nothing: it finishes by itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub asks: Option<&'static str>,
    #[serde(skip)]
    args: &'static str,
}

/// A harness whose account kiln can manage, through its CLI.
struct Cli {
    kind: &'static str,
    name: &'static str,
    program: &'static str,
    /// What its CLI runs with besides its user's environment (`NAME=value`), else nothing.
    env: &'static str,
    status: &'static str,
    /// Signing out: each run in turn.
    logout: &'static [&'static str],
    methods: &'static [Method],
    read: fn(&str) -> Account,
}

const CLIS: &[Cli] = &[
    Cli {
        kind: "claude",
        name: "Claude Code",
        program: "claude",
        env: "",
        status: "auth status --json",
        logout: &["auth logout"],
        methods: &[
            Method { id: "subscription", label: "Claude subscription", asks: Some("code"), args: "auth login --claudeai" },
            Method { id: "console", label: "Anthropic Console", asks: Some("code"), args: "auth login --console" },
            Method { id: "sso", label: "Single sign-on", asks: Some("code"), args: "auth login --sso" },
        ],
        read: read_claude,
    },
    Cli {
        kind: "codex",
        name: "Codex",
        program: "codex",
        env: "",
        status: "login status",
        logout: &["logout"],
        methods: &[
            Method { id: "chatgpt", label: "ChatGPT account", asks: None, args: "login --device-auth" },
            Method { id: "api_key", label: "OpenAI API key", asks: Some("api_key"), args: "login --with-api-key" },
        ],
        read: read_codex,
    },
    Cli {
        kind: "devin",
        name: "Devin",
        program: "devin",
        env: "",
        status: "auth status",
        logout: &["auth logout"],
        methods: &[Method { id: "devin", label: "Devin account", asks: Some("code"), args: "auth login --force-manual-token-flow" }],
        read: read_devin,
    },
    // Cursor's CLI signs in on a page opened on any device and finishes by itself; with
    // NO_OPEN_BROWSER it opens no browser on the machine and prints the page instead
    // (cursor.com/docs/cli/reference/authentication).
    Cli {
        kind: "cursor",
        name: "Cursor CLI",
        program: "agent",
        env: "NO_OPEN_BROWSER=1",
        status: "status --format json",
        logout: &["logout"],
        methods: &[Method { id: "cursor", label: "Cursor account", asks: None, args: "login" }],
        read: read_cursor,
    },
    // Pi signs in to each model provider on its own (`pi auth login <provider>`, its docs/cli.md):
    // kiln offers the subscriptions it signs in to without a browser on the machine
    // (`--no-browser`): Claude's copy-code sign-in, which asks for the code Anthropic's page shows,
    // and ChatGPT's device code, which finishes by itself. Its other providers sign in in its own
    // screen (`/login` there), or with their API keys in its environment.
    Cli {
        kind: "pi",
        name: "Pi",
        program: "pi",
        env: "",
        status: "--offline --list-models",
        logout: &["auth logout anthropic", "auth logout openai-codex"],
        methods: &[
            Method { id: "anthropic", label: "Claude Pro or Max", asks: Some("code"), args: "auth login anthropic --no-browser" },
            Method { id: "openai-codex", label: "ChatGPT Plus or Pro", asks: None, args: "auth login openai-codex --no-browser" },
        ],
        read: read_pi,
    },
];

fn cli(kind: &str) -> anyhow::Result<&'static Cli> {
    CLIS.iter().find(|c| c.kind == kind).ok_or_else(|| anyhow::anyhow!("kiln can't sign {kind} in or out (it can: {})", CLIS.iter().map(|c| c.kind).collect::<Vec<_>>().join(", ")))
}

/// How its CLI is run: the program, with what it runs with first.
fn invocation(cli: &Cli) -> String {
    if cli.env.is_empty() { cli.program.to_owned() } else { format!("env {} {}", cli.env, cli.program) }
}

/// A harness's account on a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Account {
    pub kind: String,
    pub name: String,
    /// `None`: kiln couldn't tell (`error` says why).
    pub signed_in: Option<bool>,
    /// How it is signed in: "Claude subscription", "ChatGPT", "API key", …
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub org: Option<String>,
    /// The plan, where the harness says (Claude: "pro", "max", …).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan: Option<String>,
    pub methods: Vec<Method>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<String>,
}

/// `claude auth status --json`.
fn read_claude(out: &str) -> Account {
    let text = plain(out);
    let json = text.find('{').zip(text.rfind('}')).and_then(|(a, b)| text.get(a..=b));
    let Some(v) = json.and_then(|j| serde_json::from_str::<Value>(j).ok()) else {
        return Account { error: Some(last_words(&text).unwrap_or_else(|| "it gave no status".to_owned())), ..Account::default() };
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let signed_in = v.get("loggedIn").and_then(Value::as_bool);
    let method = match (s("apiProvider").as_deref(), s("authMethod").as_deref()) {
        (Some("bedrock"), _) => Some("Amazon Bedrock".to_owned()),
        (Some("vertex"), _) => Some("Google Vertex AI".to_owned()),
        (_, Some("claude.ai")) => Some("Claude subscription".to_owned()),
        (_, Some("console")) => Some("Anthropic Console".to_owned()),
        (_, Some("api_key" | "apiKey")) => Some("API key".to_owned()),
        (_, Some("none") | None) => None,
        (_, Some(m)) => Some(m.to_owned()),
    };
    Account { signed_in, method: method.filter(|_| signed_in == Some(true)), email: s("email"), org: s("orgName"), plan: s("subscriptionType"), ..Account::default() }
}

/// `codex login status`: one line ("Logged in using ChatGPT", "Not logged in").
fn read_codex(out: &str) -> Account {
    let text = plain(out);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty() && !l.starts_with("WARNING")).unwrap_or_default();
    if line.contains("Not logged in") {
        Account { signed_in: Some(false), ..Account::default() }
    } else if line.starts_with("Logged in") {
        let method = if line.contains("ChatGPT") {
            Some("ChatGPT".to_owned())
        } else {
            line.contains("API key").then(|| "API key".to_owned())
        };
        Account { signed_in: Some(true), method, ..Account::default() }
    } else {
        Account { error: Some(if line.is_empty() { "it gave no status".to_owned() } else { line.to_owned() }), ..Account::default() }
    }
}

/// `agent status --format json` (Cursor's `src/commands/status.ts`): its `status` ("authenticated",
/// "partially-authenticated", "unauthenticated", or "error" with its `message`) and, signed in, its
/// `userInfo` (`email`, `teamName`).
fn read_cursor(out: &str) -> Account {
    let text = plain(out);
    let json = text.find('{').zip(text.rfind('}')).and_then(|(a, b)| text.get(a..=b));
    let Some(v) = json.and_then(|j| serde_json::from_str::<Value>(j).ok()) else {
        return Account { error: Some(last_words(&text).unwrap_or_else(|| "it gave no status".to_owned())), ..Account::default() };
    };
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    match s("/status").as_deref() {
        Some("authenticated") => Account { signed_in: Some(true), method: Some("Cursor account".to_owned()), email: s("/userInfo/email"), org: s("/userInfo/teamName"), ..Account::default() },
        Some("unauthenticated" | "partially-authenticated") => Account { signed_in: Some(false), ..Account::default() },
        _ => Account { error: Some(s("/message").unwrap_or_else(|| "it gave no status".to_owned())), ..Account::default() },
    }
}

/// `pi --offline --list-models` (Pi's `src/cli/list-models.ts`): the models it can use now, those
/// of the providers it has credentials for, one a line under its header (`provider  model  context
/// max-out  thinking  images`), or "No models available." with none. Signed in: some provider it
/// can use; which, its method.
fn read_pi(out: &str) -> Account {
    let text = plain(out);
    if text.lines().any(|l| l.trim_start().starts_with("No models available")) {
        return Account { signed_in: Some(false), ..Account::default() };
    }
    let mut rows = text.lines().map(str::trim).skip_while(|l| !(l.starts_with("provider") && l.contains("model") && l.contains("context")));
    if rows.next().is_none() {
        return Account { error: Some(last_words(&text).unwrap_or_else(|| "it gave no status".to_owned())), ..Account::default() };
    }
    let mut providers: Vec<&str> = Vec::new();
    for p in rows.filter_map(|l| l.split_whitespace().next()) {
        if !providers.contains(&p) {
            providers.push(p);
        }
    }
    if providers.is_empty() {
        return Account { signed_in: Some(false), ..Account::default() };
    }
    Account { signed_in: Some(true), method: Some(providers.join(", ")), ..Account::default() }
}

/// `devin auth status`: its sign-in method, then the user and account fields.
fn read_devin(out: &str) -> Account {
    let text = plain(out);
    if text.lines().any(|l| l.trim() == "Not logged in.") {
        return Account { signed_in: Some(false), ..Account::default() };
    }
    let Some(line) = text.lines().map(str::trim).find(|l| l.starts_with("Logged in")) else {
        return Account { error: Some(last_words(&text).unwrap_or_else(|| "it gave no status".to_owned())), ..Account::default() };
    };
    let field = |name: &str| text.lines().filter_map(|l| l.trim().split_once(':')).find_map(|(k, v)| (k == name && !v.trim().is_empty()).then(|| v.trim().to_owned()));
    let method = line.strip_prefix("Logged in (via ").and_then(|s| s.strip_suffix(").")).map(str::to_owned);
    Account { signed_in: Some(true), method, email: field("Email"), plan: field("Plan"), ..Account::default() }
}

/// A sign-in or sign-out, as the console follows it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AccountFlow"))]
pub struct Flow {
    pub id: String,
    pub target: String,
    pub kind: String,
    /// "login" or "logout".
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub method: Option<String>,
    /// "running", "done", "failed" or "cancelled".
    pub state: &'static str,
    /// The sign-in page to open (on any device).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub url: Option<String>,
    /// A one-time code to enter on that page.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub code: Option<String>,
    /// What it waits to be given now: "code" (the one the page showed) or "api_key".
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub asks: Option<&'static str>,
    /// What the CLI printed, plain (never what was typed in).
    pub lines: Vec<String>,
    /// The account as it is once this ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub account: Option<Account>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<String>,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub ended_at: Option<i64>,
}

struct Entry {
    flow: Flow,
    session: Option<Arc<Session>>,
    /// What it asks for once it waits (`Method::asks`).
    expects: Option<&'static str>,
    /// What was typed in: kept out of `lines`, and forgotten when the flow ends.
    typed: Vec<String>,
}

fn flows() -> &'static Mutex<Vec<Entry>> {
    static FLOWS: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(Vec::new()))
}

/// A sign-in or sign-out by its id.
pub fn get(id: &str) -> Option<Flow> {
    flows().lock().iter().find(|e| e.flow.id == id).map(|e| e.flow.clone())
}

fn remember(e: Entry) {
    let mut g = flows().lock();
    g.push(e);
    while g.len() > KEEP {
        let Some(i) = g.iter().position(|e| e.flow.state != "running") else { break };
        g.remove(i);
    }
}

fn emit(hub: &Hub, flow: &Flow) {
    hub.emit(&crate::wire::events::ServerEvent::AccountFlow { flow: Box::new(flow.clone()) });
}

/// The harnesses' accounts on `target` ("local" or "agent:<machine>"): those installed there.
pub async fn status(hub: &Arc<Hub>, target: &str) -> anyhow::Result<Vec<Account>> {
    reachable(hub, target)?;
    let installed: Option<Vec<String>> = hub.harness.kinds().get(target).map(|k| k.iter().filter(|k| k.available).map(|k| k.id.clone()).collect());
    let checks = CLIS.iter().filter(|c| installed.as_ref().is_none_or(|k| k.iter().any(|k| k == c.kind))).map(|c| account(hub, target, c));
    Ok(futures::future::join_all(checks).await)
}

/// One harness's account on `target`.
async fn account(hub: &Arc<Hub>, target: &str, cli: &'static Cli) -> Account {
    let id = crate::util::new_id();
    let mut a = match run(hub, target, script(&id, &invocation(cli), cli.status, false), QUICK_TIMEOUT).await {
        Ok((Some(127), _)) => Account { error: Some(format!("{} isn't installed there", cli.program)), ..Account::default() },
        Ok((_, out)) => (cli.read)(&out),
        Err(e) => Account { error: Some(format!("{e:#}")), ..Account::default() },
    };
    a.kind = cli.kind.to_owned();
    a.name = cli.name.to_owned();
    a.methods = cli.methods.to_vec();
    a
}

/// Start signing `kind` in on `target` (`method`: one of its methods; the first by default).
/// The flow runs on: follow it with [`get`] or `account_flow` events.
pub async fn login(hub: &Arc<Hub>, target: &str, kind: &str, method: Option<&str>) -> anyhow::Result<Flow> {
    let cli = cli(kind)?;
    let m = match method {
        Some(id) => cli.methods.iter().find(|m| m.id == id).ok_or_else(|| anyhow::anyhow!("{} has no sign-in method {id:?}", cli.name))?,
        None => cli.methods.first().ok_or_else(|| anyhow::anyhow!("{} has no sign-in method", cli.name))?,
    };
    reachable(hub, target)?;
    let id = crate::util::new_id();
    let key = m.asks == Some("api_key");
    let session = open(hub, target, script(&id, &invocation(cli), m.args, key))?;
    let flow = Flow {
        id: id.clone(),
        target: target.to_owned(),
        kind: cli.kind.to_owned(),
        action: "login",
        method: Some(m.id.to_owned()),
        state: "running",
        url: None,
        code: None,
        asks: key.then_some("api_key"),
        lines: Vec::new(),
        account: None,
        error: None,
        started_at: crate::util::now_unix(),
        ended_at: None,
    };
    remember(Entry { flow: flow.clone(), session: Some(Arc::clone(&session)), expects: m.asks, typed: Vec::new() });
    emit(hub, &flow);
    let hub = Arc::clone(hub);
    let target = target.to_owned();
    crate::rt::spawn(async move { drive(&hub, cli, &id, &target, &session).await });
    Ok(flow)
}

/// Follow a sign-in's terminal until it ends, then read the account as it is now.
async fn drive(hub: &Arc<Hub>, cli: &'static Cli, id: &str, target: &str, session: &Arc<Session>) {
    let (code, failed) = follow(hub, id, session, cli.kind == "devin").await;
    session.close();
    forget(hub, session);
    let cancelled = get(id).is_some_and(|f| f.state == "cancelled");
    let account = if cancelled { None } else { Some(account(hub, target, cli).await) };
    let signed_in = account.as_ref().and_then(|a| a.signed_in);
    let lines = get(id).map(|f| f.lines).unwrap_or_default();
    let mut ok = false;
    let told = account.clone();
    finish(hub, id, |f| {
        if f.state != "cancelled" {
            ok = failed.is_none() && code == Some(0) && signed_in == Some(true);
            f.state = if ok { "done" } else { "failed" };
            if !ok {
                f.error = Some(why(failed, code, &lines, signed_in));
            }
        }
        f.account = account;
    });
    // Signed in (perhaps as someone else): the machine's sessions of this harness take it up.
    if ok {
        hub.harness.signed_in(hub, target, cli.kind, told.as_ref()).await;
    }
}

/// Read a sign-in's output into its flow until the terminal ends (or nobody finishes it in
/// time): its exit code, and why it failed when that was not up to the CLI.
async fn follow(hub: &Hub, id: &str, session: &Session, terminal_queries: bool) -> (Option<i32>, Option<String>) {
    let mut att = session.attach();
    let mut out = String::new();
    let mut terminal = terminal_queries.then(|| vt100::Parser::new_with_callbacks(50, COLS, 0, SignInTerminal::default()));
    sign_in_output(&mut out, &mut terminal, session, &att.replay);
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    let result = match att.ended.take() {
        Some((code, error)) => (code, exit_error(error)),
        None => loop {
            update(hub, id, &out);
            let left = deadline.saturating_duration_since(Instant::now());
            match crate::rt::timeout(left, att.live.recv()).await {
                Err(_) => break (None, Some("nobody finished signing in within 15 minutes".to_owned())),
                Ok(Ok(Frame::Data(d))) => sign_in_output(&mut out, &mut terminal, session, &d),
                Ok(Ok(Frame::Exit { code, error })) => break (code, exit_error(error)),
                Ok(Ok(Frame::Size { .. }) | Err(async_broadcast::RecvError::Overflowed(_))) => {}
                Ok(Err(async_broadcast::RecvError::Closed)) => break (None, Some("its terminal closed".to_owned())),
            }
        },
    };
    update(hub, id, &out);
    session.detach();
    result
}

/// Devin waits for terminal replies before showing its code prompt; its hidden terminal has no
/// browser attached to send them.
#[derive(Default)]
struct SignInTerminal(Vec<u8>);

impl vt100::Callbacks for SignInTerminal {
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) {
        if i2.is_some() {
            return;
        }
        let zero = params.iter().all(|p| p.iter().all(|n| *n == 0));
        match (i1, c) {
            (None, 'n') if params.len() == 1 && params.first().copied() == Some(&[6][..]) => {
                let (row, col) = screen.cursor_position();
                self.0.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'c') if zero => self.0.extend_from_slice(b"\x1b[?1;2c"),
            (Some(b'?'), 'u') if zero => self.0.extend_from_slice(b"\x1b[?0u"),
            _ => {}
        }
    }
}

fn sign_in_output(out: &mut String, terminal: &mut Option<vt100::Parser<SignInTerminal>>, session: &Session, bytes: &[u8]) {
    push(out, bytes);
    if let Some(t) = terminal {
        t.process(bytes);
        let reply = std::mem::take(&mut t.callbacks_mut().0);
        if !reply.is_empty() {
            session.send(Input::Data(reply));
        }
    }
}

/// A terminal's end, as the runner reports it: why it failed, when it was not the program.
fn exit_error(error: Option<String>) -> Option<String> {
    error.map(|e| if e == "__reconnect__" { "the machine disconnected".to_owned() } else { e })
}

/// Why a sign-in or sign-out did not work out.
fn why(failed: Option<String>, code: Option<i32>, lines: &[String], signed_in: Option<bool>) -> String {
    if let Some(f) = failed {
        return f;
    }
    let said = lines.iter().rev().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("fail") || l.contains("invalid") || l.contains("denied")
    });
    match (said, code) {
        (Some(l), _) => l.clone(),
        (None, Some(127)) => "its CLI isn't installed on that machine".to_owned(),
        (None, Some(0)) if signed_in == Some(false) => "it finished, but the harness still isn't signed in".to_owned(),
        (None, Some(c)) => format!("it exited with {c}"),
        (None, None) => "it was stopped".to_owned(),
    }
}

/// Read the output so far into the flow; said to the console when that changed it.
fn update(hub: &Hub, id: &str, out: &str) {
    let changed = {
        let mut g = flows().lock();
        let Some(e) = g.iter_mut().find(|e| e.flow.id == id && e.flow.state == "running") else { return };
        let s = seen(out, &e.typed);
        let asks = match e.expects {
            // Asked once per prompt: a code given answers the prompt it saw.
            Some("code") => (s.prompts > e.typed.len()).then_some("code"),
            Some("api_key") => e.typed.is_empty().then_some("api_key"),
            _ => None,
        };
        let next = Flow { url: s.url.or_else(|| e.flow.url.clone()), code: s.code.or_else(|| e.flow.code.clone()), asks, lines: s.lines, ..e.flow.clone() };
        let changed = (&next.url, &next.code, next.asks, &next.lines) != (&e.flow.url, &e.flow.code, e.flow.asks, &e.flow.lines);
        e.flow = next;
        changed.then(|| e.flow.clone())
    };
    if let Some(flow) = changed {
        emit(hub, &flow);
    }
}

fn finish(hub: &Hub, id: &str, f: impl FnOnce(&mut Flow)) {
    let flow = {
        let mut g = flows().lock();
        let Some(e) = g.iter_mut().find(|e| e.flow.id == id) else { return };
        f(&mut e.flow);
        e.flow.asks = None;
        e.flow.ended_at = Some(crate::util::now_unix());
        e.session = None;
        e.typed.clear();
        e.flow.clone()
    };
    emit(hub, &flow);
}

/// Give a waiting sign-in what it asks for: the code its page showed, or an API key.
pub fn input(id: &str, text: &str) -> anyhow::Result<()> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "nothing to send");
    anyhow::ensure!(text.len() <= 4096 && !text.chars().any(char::is_control), "that isn't a code or a key");
    let session = {
        let mut g = flows().lock();
        let e = g.iter_mut().find(|e| e.flow.id == id).ok_or_else(|| anyhow::anyhow!("no such sign-in"))?;
        anyhow::ensure!(e.flow.state == "running", "that sign-in has ended");
        let session = e.session.clone().ok_or_else(|| anyhow::anyhow!("that sign-in has ended"))?;
        e.typed.push(text.to_owned());
        e.flow.asks = None;
        session
    };
    session.send(Input::Data(format!("{text}\r").into_bytes()));
    Ok(())
}

/// Stop a running sign-in.
pub fn cancel(id: &str) -> anyhow::Result<()> {
    let session = {
        let mut g = flows().lock();
        let e = g.iter_mut().find(|e| e.flow.id == id).ok_or_else(|| anyhow::anyhow!("no such sign-in"))?;
        if e.flow.state != "running" {
            return Ok(());
        }
        e.flow.state = "cancelled";
        e.session.clone()
    };
    if let Some(s) = session {
        s.close();
    }
    Ok(())
}

/// Sign `kind` out on `target`: the finished flow, with the account as it is now.
pub async fn logout(hub: &Arc<Hub>, target: &str, kind: &str) -> anyhow::Result<Flow> {
    let cli = cli(kind)?;
    reachable(hub, target)?;
    let id = crate::util::new_id();
    let flow = Flow {
        id: id.clone(),
        target: target.to_owned(),
        kind: cli.kind.to_owned(),
        action: "logout",
        method: None,
        state: "running",
        url: None,
        code: None,
        asks: None,
        lines: Vec::new(),
        account: None,
        error: None,
        started_at: crate::util::now_unix(),
        ended_at: None,
    };
    remember(Entry { flow: flow.clone(), session: None, expects: None, typed: Vec::new() });
    emit(hub, &flow);
    // Each step in turn, its output after the one before; the first that fails says how it went.
    let mut ran: anyhow::Result<(Option<i32>, String)> = Ok((Some(0), String::new()));
    for args in cli.logout {
        match run(hub, target, script(&id, &invocation(cli), args, false), QUICK_TIMEOUT).await {
            Ok((code, out)) => {
                if let Ok((was, all)) = &mut ran {
                    all.push_str(&out);
                    if *was == Some(0) {
                        *was = code;
                    }
                }
            }
            Err(e) => {
                ran = Err(e);
                break;
            }
        }
    }
    let account = account(hub, target, cli).await;
    let signed_in = account.signed_in;
    finish(hub, &id, |f| {
        let (code, failed) = match &ran {
            Ok((code, out)) => {
                f.lines = seen(out, &[]).lines;
                (*code, None)
            }
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let ok = failed.is_none() && code == Some(0) && signed_in != Some(true);
        f.state = if ok { "done" } else { "failed" };
        if !ok {
            f.error = Some(if signed_in == Some(true) && failed.is_none() {
                "it finished, but the harness is still signed in".to_owned()
            } else {
                why(failed, code, &f.lines, signed_in)
            });
        }
        f.account = Some(account);
    });
    get(&id).ok_or_else(|| anyhow::anyhow!("the sign-out was forgotten"))
}

/// Whether kiln can run a harness's CLI on `target`: a connected machine with terminals on. Not
/// on Windows yet (its shells read these scripts differently).
fn reachable(hub: &Hub, target: &str) -> anyhow::Result<()> {
    let windows = if target == "local" {
        anyhow::ensure!(hub.cfg.terminal.enabled, "terminals (and so agents) are disabled on this server");
        cfg!(windows)
    } else {
        let machine = target.strip_prefix("agent:").unwrap_or(target);
        let g = hub.agents.load();
        let a = g.get(machine).ok_or_else(|| anyhow::anyhow!("there is no machine called {machine}"))?;
        anyhow::ensure!(a.connected, "{} is not connected", a.shown());
        anyhow::ensure!(a.terminal && !a.ephemeral, "{} has terminals (and so agents) turned off", a.shown());
        a.os == "windows"
    };
    anyhow::ensure!(!windows, "signing in from kiln isn't available on Windows yet: run `claude auth login`, `codex login`, `devin auth login`, `agent login` or `pi auth login` on that machine");
    Ok(())
}

fn open(hub: &Arc<Hub>, target: &str, command: String) -> anyhow::Result<Arc<Session>> {
    if target == "local" {
        crate::termsess::open_local(hub, Some(command), COLS, 50)
    } else {
        Ok(crate::termsess::open_agent(hub, target.strip_prefix("agent:").unwrap_or(target).to_owned(), Some(command), COLS, 50))
    }
}

/// Its record and replay go with it (they may hold a pasted code).
fn forget(hub: &Hub, session: &Session) {
    hub.terminals.remove(&session.id);
}

/// Run `command` in a hidden terminal on `target` until it exits: its exit code and output.
async fn run(hub: &Arc<Hub>, target: &str, command: String, limit: Duration) -> anyhow::Result<(Option<i32>, String)> {
    let session = open(hub, target, command)?;
    let r = collect(&session, limit).await;
    session.close();
    forget(hub, &session);
    r
}

async fn collect(session: &Session, limit: Duration) -> anyhow::Result<(Option<i32>, String)> {
    let mut att = session.attach();
    let mut out = String::new();
    push(&mut out, &att.replay);
    let deadline = Instant::now() + limit;
    let r = match att.ended.take() {
        Some((code, error)) => exit_error(error).map_or(Ok(code), |e| Err(anyhow::anyhow!(e))),
        None => loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match crate::rt::timeout(left, att.live.recv()).await {
                Err(_) => break Err(anyhow::anyhow!("it didn't answer within {} s", limit.as_secs())),
                Ok(Ok(Frame::Data(d))) => push(&mut out, &d),
                Ok(Ok(Frame::Exit { code, error })) => break exit_error(error).map_or(Ok(code), |e| Err(anyhow::anyhow!(e))),
                Ok(Ok(Frame::Size { .. }) | Err(async_broadcast::RecvError::Overflowed(_))) => {}
                Ok(Err(async_broadcast::RecvError::Closed)) => break Err(anyhow::anyhow!("its terminal closed")),
            }
        },
    };
    session.detach();
    r.map(|code| (code, out))
}

fn push(out: &mut String, bytes: &[u8]) {
    if out.len() < OUTPUT_MAX {
        out.push_str(&String::from_utf8_lossy(bytes));
    }
}

/// The command a flow's terminal runs, marked so it is never listed: `sh -c '…'` around a script
/// free of single quotes and backslashes, so it reads the same whichever shell the user has.
/// `secret`: the CLI takes one on its input (an API key) and won't read it from a terminal, so the
/// script reads it, unechoed, and pipes it in.
fn script(id: &str, program: &str, args: &str, secret: bool) -> String {
    if secret {
        format!("sh -c '{MARK}{id}\nstty -echo 2>/dev/null\nread -r secret\nstty echo 2>/dev/null\necho \"$secret\" | {program} {args}'")
    } else {
        format!("sh -c '{MARK}{id}\nexec {program} {args}'")
    }
}

/// What a CLI's output says so far.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    url: Option<String>,
    code: Option<String>,
    /// How many times it asked for the code its sign-in page shows.
    prompts: usize,
    lines: Vec<String>,
}

fn seen(out: &str, typed: &[String]) -> Seen {
    let text = plain(out);
    // A link's own target first (the text shown may be cut), then any URL in the text.
    let url = links(out).into_iter().chain(urls(&text)).find(|u| u.starts_with("https://"));
    let mut lines: Vec<String> = Vec::new();
    for l in text.lines().map(str::trim) {
        let l = l.split_once(CLAUDE_PROMPT).map_or(l, |(_, rest)| rest.trim());
        if l.is_empty() || l.contains(DEVIN_PASTE) || l.contains(PI_PASTE) || typed.iter().any(|t| l.contains(t.as_str())) || lines.last().is_some_and(|p| p == l) {
            continue;
        }
        lines.push(l.to_owned());
    }
    // On its own line (Codex), or ending one (Pi's "Open <page> and enter code: <code>").
    let code = lines.iter().find_map(|l| device_code(l).or_else(|| l.rsplit_once("code: ").and_then(|(_, c)| device_code(c.trim()))));
    let skip = lines.len().saturating_sub(LINES);
    Seen { url, code, prompts: text.matches(CLAUDE_PASTE).count() + text.matches(DEVIN_PASTE).count() + text.matches(PI_PASTE).count(), lines: lines.split_off(skip) }
}

/// A one-time code as a device sign-in shows it on its own line ("1FYU-ZAUR8").
fn device_code(line: &str) -> Option<String> {
    let (a, b) = line.split_once('-')?;
    let part = |p: &str| (3..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    (part(a) && part(b) && line.chars().any(|c| c.is_ascii_digit())).then(|| line.to_owned())
}

/// The targets of OSC 8 hyperlinks (`ESC ] 8 ; params ; URL ST`).
fn links(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("\x1b]8;") {
        rest = rest.get(i + 4..).unwrap_or_default();
        let Some((_, after)) = rest.split_once(';') else { break };
        let end = after.find(['\x07', '\x1b']).unwrap_or(after.len());
        let url = after.get(..end).unwrap_or_default();
        if !url.is_empty() {
            out.push(url.to_owned());
        }
        rest = after.get(end..).unwrap_or_default();
    }
    out
}

/// URLs in plain text.
fn urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("https://") {
        let tail = rest.get(i..).unwrap_or_default();
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')).unwrap_or(tail.len());
        let url = tail.get(..end).unwrap_or_default().trim_end_matches(['.', ',', ')', ']', ';', ':']);
        if url.len() > "https://".len() {
            out.push(url.to_owned());
        }
        rest = tail.get(end.max(1)..).unwrap_or_default();
    }
    out
}

/// Terminal output as text: escape sequences dropped (a link keeps its text), carriage returns as
/// line breaks.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: up to a final byte in @..~
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// The last thing it said, for when it gave nothing else.
fn last_words(text: &str) -> Option<String> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_sign_in_gives_its_page_and_asks_for_the_code() {
        let link = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=user%3Ainference&state=abc";
        let out = format!(
            "\x1b[?25lOpening browser to sign in…\r\nIf the browser didn't open, visit: \x1b]8;;{link}\x07{}\x1b]8;;\x07\r\n\x1b[2mPaste code here if prompted > \x1b[0m",
            link.get(..60).unwrap_or_default()
        );
        let s = seen(&out, &[]);
        assert_eq!(s.url.as_deref(), Some(link), "the link's own target, not its shortened text");
        assert_eq!((s.prompts, s.code.as_deref()), (1, None));
        assert!(s.lines.iter().any(|l| l.starts_with("Opening browser")));
        assert!(s.lines.iter().all(|l| !l.contains('\x1b')));
        // What was typed in never shows.
        let s = seen(&format!("{out}abcd#efgh-secret\r\nLogin successful.\r\n"), &["abcd#efgh-secret".to_owned()]);
        assert!(s.lines.iter().all(|l| !l.contains("efgh-secret")) && s.lines.iter().any(|l| l == "Login successful."));
        // Its prompt is the console's to show: a failure printed after it reads on its own.
        let s = seen(&format!("{out}Login failed: Request failed with status code 400\r\n"), &[]);
        assert_eq!(s.lines.last().map(String::as_str), Some("Login failed: Request failed with status code 400"));
        assert!(s.lines.iter().all(|l| !l.contains(CLAUDE_PROMPT)));
    }

    #[test]
    fn a_codex_device_sign_in_gives_its_page_and_code() {
        let out = "\r\nWelcome to Codex [v0.162.1]\r\n\x1b[2mOpenAI's command-line coding agent\x1b[0m\r\n\r\nFollow these steps to sign in with ChatGPT using device code authorization:\r\n\r\n1. Open this link in your browser and sign in to your account\r\n   \x1b[94mhttps://auth.openai.com/codex/device\x1b[0m\r\n\r\n2. Enter this one-time code \x1b[2m(expires in 15 minutes)\x1b[0m\r\n   \x1b[94m1FYU-ZAUR8\x1b[0m\r\n";
        let s = seen(out, &[]);
        assert_eq!((s.url.as_deref(), s.code.as_deref(), s.prompts), (Some("https://auth.openai.com/codex/device"), Some("1FYU-ZAUR8"), 0));
        assert_eq!(device_code("v0.162.1"), None);
        assert_eq!(device_code("TO-DO"), None);
    }

    #[test]
    fn a_devin_manual_sign_in_gives_its_page_and_asks_for_the_code() {
        let link = "https://app.devin.ai/auth/cli/continue?state=synthetic";
        let out = format!("Visit \x1b]8;;{link}\x1b\\{link}\x1b]8;;\x1b\\ to sign in, then copy the code and paste it below.\r\n\r\nCode:\r\n❭ {DEVIN_PASTE}\r\n↵ submit · esc cancel");
        let s = seen(&out, &[]);
        assert_eq!((s.url.as_deref(), s.prompts, s.code), (Some(link), 1, None));
        assert!(s.lines.iter().all(|l| !l.contains(DEVIN_PASTE)));
        let s = seen(&format!("{out}\r\nsynthetic-auth-code\r\nLogin successful.\r\n"), &["synthetic-auth-code".to_owned()]);
        assert!(s.lines.iter().all(|l| !l.contains("synthetic-auth-code")) && s.lines.iter().any(|l| l == "Login successful."));
    }

    #[test]
    fn a_devin_sign_in_gets_terminal_replies_even_when_a_query_is_split() {
        let mut t = vt100::Parser::new_with_callbacks(50, COLS, 0, SignInTerminal::default());
        t.process(b"\x1b[?u\x1b[c\x1b[4;8H\x1b[6");
        assert_eq!(std::mem::take(&mut t.callbacks_mut().0), b"\x1b[?0u\x1b[?1;2c");
        t.process(b"n");
        assert_eq!(std::mem::take(&mut t.callbacks_mut().0), b"\x1b[4;8R");
        t.process(b"\x1b[5n\x1b[>c");
        assert!(t.callbacks().0.is_empty());
    }

    #[test]
    fn devin_status_keeps_only_the_account_fields_the_card_uses() {
        let out = "\x1b[32mLogged in (via Devin).\x1b[0m\r\n\r\nCredentials:\r\n  File: /home/test/.local/share/devin/credentials.toml\r\nUser:\r\n  Name: Test User\r\n  Email: agent@example.test\r\n  User ID: user-synthetic\r\nAccount:\r\n  Tier: Teams\r\n  Plan: Teams\r\n  Account ID: account-synthetic\r\n";
        let a = read_devin(out);
        assert_eq!((a.signed_in, a.method.as_deref(), a.email.as_deref(), a.plan.as_deref()), (Some(true), Some("Devin"), Some("agent@example.test"), Some("Teams")));
        assert!(a.org.is_none() && a.error.is_none());
        let out = serde_json::to_string(&a).unwrap_or_default();
        assert!(!out.contains("user-synthetic") && !out.contains("account-synthetic") && !out.contains("credentials.toml"));
        assert_eq!(read_devin("Not logged in.\r\n  Credentials path: /tmp/devin/credentials.toml\r\nRun `devin auth login` to authenticate.\r\n").signed_in, Some(false));
        assert_eq!(read_devin("Error: status unavailable\r\n").error.as_deref(), Some("Error: status unavailable"));
        assert_eq!(read_devin("").signed_in, None);
        let c = cli("devin").ok();
        assert_eq!(c.map(|c| (c.status, c.logout)), Some(("auth status", &["auth logout"][..])));
        assert_eq!(c.and_then(|c| c.methods.first()).map(|m| (m.asks, m.args)), Some((Some("code"), "auth login --force-manual-token-flow")));
    }

    /// Cursor's `agent status --format json`, as its `src/commands/status.ts` (CLI 2026.11.11)
    /// writes it (`JSON.stringify(status, null, 2)`, through a terminal): signed in with the account
    /// and team the card shows, its ids left out; signed out; or what went wrong.
    #[test]
    fn cursor_status_reads_its_json() {
        let signed = "{\r\n  \"status\": \"authenticated\",\r\n  \"isAuthenticated\": true,\r\n  \"hasAccessToken\": true,\r\n  \"hasRefreshToken\": true,\r\n  \"userInfo\": {\r\n    \"email\": \"agent@example.test\",\r\n    \"userId\": 4242,\r\n    \"firstName\": \"Test\",\r\n    \"lastName\": \"User\",\r\n    \"teamId\": 9,\r\n    \"teamName\": \"Example Team\"\r\n  }\r\n}\r\n";
        let a = read_cursor(signed);
        assert_eq!((a.signed_in, a.method.as_deref(), a.email.as_deref(), a.org.as_deref()), (Some(true), Some("Cursor account"), Some("agent@example.test"), Some("Example Team")));
        assert!(!serde_json::to_string(&a).unwrap_or_default().contains("4242"));
        assert_eq!(read_cursor("{\n  \"status\": \"unauthenticated\",\n  \"isAuthenticated\": false,\n  \"hasAccessToken\": false,\n  \"hasRefreshToken\": false,\n  \"message\": \"Not logged in\"\n}\n").signed_in, Some(false));
        assert_eq!(read_cursor("{\"status\": \"partially-authenticated\", \"isAuthenticated\": false, \"hasAccessToken\": true, \"hasRefreshToken\": false, \"message\": \"Partially authenticated (missing refresh token)\"}").signed_in, Some(false));
        assert_eq!(read_cursor("{\n  \"status\": \"error\",\n  \"message\": \"Status check error: fetch failed\"\n}\n").error.as_deref(), Some("Status check error: fetch failed"));
        assert_eq!(read_cursor("sh: 1: agent: not found\r\n").error.as_deref(), Some("sh: 1: agent: not found"));
        assert_eq!(read_cursor("").signed_in, None);
        let c = cli("cursor").ok();
        assert_eq!(c.map(|c| (invocation(c), c.status, c.logout)), Some(("env NO_OPEN_BROWSER=1 agent".to_owned(), "status --format json", &["logout"][..])));
    }

    /// `agent login` with NO_OPEN_BROWSER (its `src/commands/login.ts`): the page to open on any
    /// device, then it waits for the sign-in there and finishes by itself; nothing to give back. Its
    /// script opens no browser and reads the same in any shell.
    #[test]
    fn a_cursor_sign_in_gives_its_page_and_finishes_by_itself() {
        let link = "https://cursor.com/loginDeepControl?challenge=synthetic&uuid=00000000-0000-4000-8000-000000000000&mode=login&redirectTarget=cli&supportsSelectedTeamLogin=true";
        let out = format!("Starting login process...\r\nAuthenticating with Cursor...\r\nWaiting for browser authentication...\r\nOpen a browser and navigate to this link: {link}\r\n\r\nPress q to show a QR code to log in from another device.\r\n");
        let s = seen(&out, &[]);
        assert_eq!((s.url.as_deref(), s.code.as_deref(), s.prompts), (Some(link), None, 0));
        assert!(s.lines.iter().any(|l| l.starts_with("Waiting for browser authentication")));
        let c = cli("cursor").ok();
        assert_eq!(c.and_then(|c| c.methods.first()).map(|m| (m.asks, m.args)), Some((None, "login")));
        let script = c.map(|c| script("01abc", &invocation(c), "login", false)).unwrap_or_default();
        assert!(crate::termsess::hidden(Some(&script)));
        assert!(script.ends_with("exec env NO_OPEN_BROWSER=1 agent login'") && !script.trim_start_matches("sh -c '").trim_end_matches('\'').contains(['\'', '\\']), "{script}");
    }

    /// `pi --offline --list-models` (Pi 1.5.2's `src/cli/list-models.ts`): the providers it has
    /// credentials for, from its models' rows; none, "No models available."
    #[test]
    fn pi_status_says_the_providers_it_can_use() {
        let out = "provider      model                       context  max-out  thinking  images\r\nanthropic     claude-opus-5-5             1M       128K     yes       yes\r\nanthropic     claude-sonnet-5             1M       64K      yes       yes\r\nopenai-codex  gpt-6.1-sol                 400K     128K     yes       yes\r\n";
        let a = read_pi(out);
        assert_eq!((a.signed_in, a.method.as_deref()), (Some(true), Some("anthropic, openai-codex")));
        // What it says on stderr first doesn't count.
        let a = read_pi(&format!("Warning: errors loading models.json:\r\nUnexpected token\r\n{out}"));
        assert_eq!(a.signed_in, Some(true));
        assert_eq!(read_pi("No models available. Use /login or set an API key environment variable. See https://pi.dev/docs/latest/providers\r\n").signed_in, Some(false));
        assert_eq!(read_pi("sh: 1: pi: not found\r\n").error.as_deref(), Some("sh: 1: pi: not found"));
        assert_eq!(read_pi("").signed_in, None);
        let c = cli("pi").ok();
        assert_eq!(c.map(|c| (invocation(c), c.status, c.logout)), Some(("pi".to_owned(), "--offline --list-models", &["auth logout anthropic", "auth logout openai-codex"][..])));
    }

    /// Pi's sign-ins without a browser on the machine (its `src/cli/auth-login.ts` and its
    /// providers' own words): Claude's copy-code sign-in gives its page and asks for the code
    /// Anthropic shows; ChatGPT's device code gives its page and code and finishes by itself.
    #[test]
    fn pi_sign_ins_give_their_page_and_ask_for_a_code_or_finish_by_themselves() {
        let link = "https://claude.ai/oauth/authorize?code=true&client_id=synthetic&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=user%3Ainference&code_challenge=synthetic&code_challenge_method=S256&state=synthetic";
        let out = format!("Select Anthropic login method: Copy code login (headless)\r\nOpen this URL to sign in:\r\n  {link}\r\nComplete login in your browser, then copy the code Anthropic shows and paste it here.\r\nPaste the code Anthropic shows after you sign in: (code#state): ");
        let s = seen(&out, &[]);
        assert_eq!((s.url.as_deref(), s.prompts, s.code), (Some(link), 1, None));
        assert!(s.lines.iter().all(|l| !l.contains(PI_PASTE)), "its prompt is the console's to show");
        let s = seen(&format!("{out}synthetic-code#synthetic\r\nExchanging authorization code for tokens...\r\nLogged in to Anthropic. Credentials saved to /home/test/.pi/agent/auth.json\r\n"), &["synthetic-code#synthetic".to_owned()]);
        assert!(s.lines.iter().all(|l| !l.contains("synthetic-code")) && s.lines.iter().any(|l| l.starts_with("Logged in to Anthropic")));
        let out = "Select OpenAI Codex login method: Device code login (headless)\r\nOpen https://auth.openai.com/codex/device and enter code: 1FYU-ZAUR8\r\nWaiting for authentication...\r\n";
        let s = seen(out, &[]);
        assert_eq!((s.url.as_deref(), s.code.as_deref(), s.prompts), (Some("https://auth.openai.com/codex/device"), Some("1FYU-ZAUR8"), 0));
        let methods: Vec<_> = cli("pi").map(|c| c.methods.iter().map(|m| (m.id, m.asks, m.args)).collect()).unwrap_or_default();
        assert_eq!(methods, [("anthropic", Some("code"), "auth login anthropic --no-browser"), ("openai-codex", None, "auth login openai-codex --no-browser")]);
    }

    #[test]
    fn statuses_are_read() {
        let claude = r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "email": "a@b.c", "orgName": "Org", "subscriptionType": "max"}"#;
        let a = read_claude(claude);
        assert_eq!((a.signed_in, a.method.as_deref(), a.email.as_deref(), a.plan.as_deref()), (Some(true), Some("Claude subscription"), Some("a@b.c"), Some("max")));
        let a = read_claude("{\r\n  \"loggedIn\": false,\r\n  \"authMethod\": \"none\",\r\n  \"apiProvider\": \"firstParty\"\r\n}\r\n");
        assert_eq!((a.signed_in, a.method), (Some(false), None));
        assert_eq!(read_claude("sh: 1: claude: not found\r\n").error.as_deref(), Some("sh: 1: claude: not found"));
        assert_eq!(read_codex("Logged in using ChatGPT\r\n").method.as_deref(), Some("ChatGPT"));
        assert_eq!(read_codex("Logged in using an API key - sk-proj-***ABCD\r\n").method.as_deref(), Some("API key"));
        assert_eq!(read_codex("WARNING: proceeding\r\nNot logged in\r\n").signed_in, Some(false));
        assert_eq!(read_codex("").signed_in, None);
    }

    #[test]
    fn its_terminal_is_hidden_and_its_script_reads_the_same_in_any_shell() {
        let s = script("01abc", "codex", "login --with-api-key", true);
        assert!(crate::termsess::hidden(Some(&s)));
        let inner = s.strip_prefix("sh -c '").and_then(|r| r.strip_suffix('\'')).unwrap_or_default();
        assert!(!inner.is_empty() && !inner.contains('\'') && !inner.contains('\\'));
        assert!(inner.contains("stty -echo") && inner.contains("read -r secret") && inner.ends_with("| codex login --with-api-key"));
        assert!(!script("01abc", "claude", "auth status --json", false).contains("stty"));
    }

    #[test]
    fn why_it_failed() {
        let lines = vec!["Opening browser to sign in…".to_owned(), "OAuth error: invalid code".to_owned()];
        assert_eq!(why(None, Some(1), &lines, Some(false)), "OAuth error: invalid code");
        assert_eq!(why(None, Some(127), &[], None), "its CLI isn't installed on that machine");
        assert_eq!(why(Some("the machine disconnected".into()), None, &lines, None), "the machine disconnected");
        assert_eq!(why(None, Some(0), &[], Some(false)), "it finished, but the harness still isn't signed in");
    }
}
