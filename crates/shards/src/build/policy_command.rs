//! `shards buildx policy`: buildx v0.37.1's policy commands (commands/policy, D108): the
//! group, which says its commands; `eval`, a policy's decision for one source, or the
//! source's input (`--print`); and `test`, a policy's tests. Each runs here, of this
//! host's files, against a builder ([`Builder`]): its sources' metadata resolved as the
//! builder's builds resolve them, unseen (buildx's resolver shows no progress there), and
//! the builder's platform. The builder is this host (`host_platform`), or through a context the
//! machine it reaches (D180), as buildx evaluates a policy against a remote builder: the
//! policy, its tests and their files this client's, the builder's answers the machine's,
//! asked of it by the steps [`step`] serves there.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use shards_agentfile::platform::{self, Platform};
use shards_cmdline::commands::{POLICY, POLICY_COMMANDS, POLICY_EVAL, POLICY_TEST};
use shards_cmdline::flags::{self, Command, Outcome, Parsed};

use super::{Bases, Progress, host_platform, http, policy};

const GROUP: &str = "shards buildx policy";
const EVAL: &str = "shards buildx policy eval";
const TEST: &str = "shards buildx policy test";

/// The builder a policy command asks what it cannot know of its own: its platform, a
/// source's metadata as its builds resolve it, and an HTTP source a policy's function reads.
pub(crate) trait Builder {
    fn platform(&self) -> Result<Platform, String>;

    /// `f` given a resolver of sources' metadata, the builder's.
    fn resolving(&self, f: &dyn Fn(&dyn policy::Resolve) -> Result<(), String>) -> Result<(), String>;

    /// The content of `url`, asked for with `accept`; the error as the read of the solved
    /// source words it.
    fn fetch(&self, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String>;
}

/// What marks a request's step of a policy command run through a context to the machine's
/// daemon (D180): no command line's word, as no command line can hold NUL. The daemon runs
/// it as a command of its own (`buildx policy`), the step named by [`STEP_VAR`], so that the
/// client's logins reach the registries it asks of as a command's do.
pub(crate) const POLICY_STEP: &str = "\u{0}policy";

/// The variable that names the step the machine's daemon runs ([`step`]).
pub(crate) const STEP_VAR: &str = "SHARDS_POLICY_STEP";

/// `shards buildx policy ARGS…`, this host the builder; or the step of a client's that
/// the machine's daemon runs ([`STEP_VAR`]).
pub fn command(args: impl Iterator<Item = OsString>) -> ExitCode {
    if let Some(name) = std::env::var_os(STEP_VAR) {
        return step(&name.to_string_lossy(), args);
    }
    command_with(args, &Here)
}

/// `shards buildx policy ARGS…`, asking `builder`.
pub(crate) fn command_with(args: impl Iterator<Item = OsString>, builder: &dyn Builder) -> ExitCode {
    let mut argv = Vec::new();
    for a in args {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => return error(&format!("{a:?} is not UTF-8")),
        }
    }
    match argv.split_first() {
        Some((sub, rest)) if sub == "eval" => match read(&POLICY_EVAL, EVAL, rest) {
            Ok(parsed) => match eval(&parsed, builder) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => error(&e),
            },
            Err(code) => code,
        },
        Some((sub, rest)) if sub == "test" => match read(&POLICY_TEST, TEST, rest) {
            Ok(parsed) => test(&parsed, builder),
            Err(code) => code,
        },
        // A group runs nothing: asked anything it can read, it says its commands.
        _ => match flags::parse(&POLICY, GROUP, &argv, &flags::value) {
            Outcome::Run(Parsed { notices, .. }) | Outcome::Help { notices } => {
                let _ = write!(
                    std::io::stdout(),
                    "{notices}{}",
                    flags::group_help(&POLICY, GROUP, POLICY_COMMANDS, 80)
                );
                ExitCode::SUCCESS
            }
            Outcome::Fail {
                notices,
                text,
                status,
            } => fail(&notices, &text, status),
        },
    }
}

/// A subcommand's flags and arguments, or what it answered: its help, or its refusal.
fn read(command: &'static Command, path: &str, argv: &[String]) -> Result<Parsed, ExitCode> {
    match flags::parse(command, path, argv, &flags::value) {
        Outcome::Run(parsed) => {
            let _ = write!(std::io::stdout(), "{}", parsed.notices);
            Ok(parsed)
        }
        Outcome::Help { notices } => {
            let _ = write!(std::io::stdout(), "{notices}{}", flags::help(command, path, 80));
            Err(ExitCode::SUCCESS)
        }
        Outcome::Fail {
            notices,
            text,
            status,
        } => Err(fail(&notices, &text, status)),
    }
}

fn fail(notices: &str, text: &str, status: u8) -> ExitCode {
    let _ = write!(std::io::stdout(), "{notices}");
    let _ = writeln!(std::io::stderr(), "{text}");
    ExitCode::from(status)
}

/// buildx's main: `ERROR: ` and the error, status 1.
fn error(e: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "ERROR: {e}");
    ExitCode::FAILURE
}

/// logrus as buildx v0.37.1's main formats it: the level, then the message.
fn log(level: &str, text: &str) {
    let _ = writeln!(std::io::stderr(), "{level}: {text}");
}

/// runEval.
fn eval(parsed: &Parsed, builder: &dyn Builder) -> Result<(), String> {
    let arg = parsed.args.first().map(String::as_str).unwrap_or_default();
    let source = parse_source(arg)?;
    let platform = match parsed.string("platform") {
        "" => builder.platform()?,
        p => platform::parse(p.as_bytes(), &builder.platform()?)
            .map(|p| platform::normalize(&p))
            .map_err(|e| {
                format!(
                    "invalid platform {}: {}",
                    shards_cmdline::go::quote(p),
                    String::from_utf8_lossy(&e)
                )
            })?,
    };
    builder.resolving(&|resolver| {
        if parsed.bool("print") {
            let fields = parsed.many("fields");
            let (json, invalid, unresolved) = policy::print_input(&source, &platform, fields, resolver)?;
            if !invalid.is_empty() {
                log("WARNING", &format!("invalid fields: {}", invalid.join(", ")));
            }
            if !unresolved.is_empty() {
                log("INFO", &format!("unresolved fields: {}", unresolved.join(", ")));
            }
            let _ = writeln!(std::io::stdout(), "{json}");
            return Ok(());
        }
        decide(parsed, &source, &platform, resolver, builder)
    })
}

/// runEval's check: the policy file, then CheckPolicy asked at most four times, each
/// question of the source's metadata answered.
fn decide(
    parsed: &Parsed,
    source: &policy::Source,
    platform: &Platform,
    resolver: &dyn policy::Resolve,
    builder: &dyn Builder,
) -> Result<(), String> {
    let filename = parsed.string("file");
    if filename.is_empty() {
        return Err("filename is required".into());
    }
    let (name, file) = if filename == "-" {
        ("stdin".to_string(), filename.to_string())
    } else {
        (filename.to_string(), format!("{filename}.rego"))
    };
    let data = read_policy(&file).map_err(|e| format!("failed to read policy file {file}: {e}"))?;
    let base = |p: &str| {
        Path::new(p)
            .file_name()
            .map_or_else(|| p.to_string(), |n| n.to_string_lossy().into_owned())
    };
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let debug = parsed.bool("debug");
    let Some(policies) = policy::Policies::configure(policy::Setup {
        default: policy::Opt {
            files: vec![policy::FileSpec {
                filename: base(&file),
                optional: false,
                data: Some(data),
            }],
            context_dir: Some(cwd.clone()),
            ..policy::Opt::default()
        },
        configs: &[],
        env: policy::Env {
            filename: base(&name),
            ..policy::Env::default()
        },
        cwd,
        default_platform: platform.clone(),
        debug,
        default_policy: false,
        remote: None,
    })?
    else {
        return Err("policy returned no decision".into());
    };
    let log = Said { debug, builder };
    let mut meta = policy::Meta::default();
    let mut attempts = 5;
    loop {
        attempts -= 1;
        if attempts <= 0 {
            return Err("maximum attempts reached for resolving policy metadata".into());
        }
        match policies.check_once(source, Some(platform), &meta, resolver, &log)? {
            policy::Answer::Resolve(request) => meta = resolver.resolve(source, &request)?,
            policy::Answer::Allow | policy::Answer::Convert(_) => return Ok(()),
            // evalDecisionError.
            policy::Answer::Deny(messages) => {
                let messages: Vec<&str> = messages
                    .iter()
                    .map(String::as_str)
                    .filter(|m| !m.is_empty())
                    .collect();
                return Err(if messages.is_empty() {
                    "policy denied".into()
                } else {
                    format!("policy denied: {}", messages.join("; "))
                });
            }
        }
    }
}

/// readPolicyData: the file, or stdin for `-`; its error as Go's os package words it.
fn read_policy(file: &str) -> Result<Vec<u8>, String> {
    if file == "-" {
        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        return Ok(data);
    }
    if Path::new(file).is_dir() {
        return Err(format!("read {file}: is a directory"));
    }
    std::fs::read(file).map_err(|e| format!("open {file}: {}", os_error(&e)))
}

/// An error of the OS, as Go's syscall.Errno says it.
fn os_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::NotADirectory => "not a directory".into(),
        _ => e.to_string(),
    }
}

/// parseSource: an image, pinned to its tag where it names none; a Git repository; an
/// HTTP(S) URL, or a Git repository behind one; or a path, which is the build's context.
fn parse_source(input: &str) -> Result<policy::Source, String> {
    use shards_agentfile::git::{Parsed, parse_git_ref};
    if let Some(r) = input.strip_prefix("docker-image://") {
        let mut r = shards_image::reference::Reference::parse_normalized(r)
            .map_err(|e| format!("failed to parse image source reference: {e}"))?;
        if r.tag.is_none() && r.digest.is_none() {
            r.tag = Some("latest".into());
        }
        return Ok(policy::Source::new(format!("docker-image://{r}")));
    }
    // An OSI artifact of a registry (D116), of whatever kind it is: shards' own source.
    if let Some(r) = input.strip_prefix("osi-artifact://") {
        let r = shards_image::reference::Reference::parse_normalized(r)
            .map_err(|e| format!("failed to parse OSI artifact source reference: {e}"))?;
        return Ok(policy::Source::new(format!("osi-artifact://{r}")));
    }
    if input.starts_with("git://") {
        return match parse_git_ref(input.as_bytes()) {
            Parsed::NotGit => Err(format!("invalid git context {input}")),
            Parsed::BadGit(e) => Err(String::from_utf8_lossy(&e).into_owned()),
            Parsed::Git(_) => Ok(policy::Source::new(input)),
        };
    }
    if input.starts_with("http://") || input.starts_with("https://") {
        return Ok(match parse_git_ref(input.as_bytes()) {
            // A Git repository, whatever its error.
            Parsed::Git(_) | Parsed::BadGit(_) => {
                let mut s = policy::Source::new(format!("git://{input}"));
                s.attrs.insert("git.fullurl".into(), input.into());
                s
            }
            Parsed::NotGit => policy::Source::new(input),
        });
    }
    std::fs::metadata(input)
        .map_err(|e| format!("invalid local path {input}: stat {input}: {}", os_error(&e)))?;
    Ok(policy::Source::new("local://context"))
}

/// Where `eval`'s policy logs: logrus at debug level, shown with `--debug`; the sources
/// its functions read fetched by the builder with no step to show.
struct Said<'b> {
    debug: bool,
    builder: &'b dyn Builder,
}

impl policy::Log for Said<'_> {
    fn line(&self, text: &str) {
        if self.debug {
            log("DEBUG", text);
        }
    }

    fn fetch(&self, _name: &str, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String> {
        self.builder.fetch(url, accept)
    }
}

/// This host, the builder.
struct Here;

impl Builder for Here {
    fn platform(&self) -> Result<Platform, String> {
        Ok(host_platform())
    }

    fn resolving(&self, f: &dyn Fn(&dyn policy::Resolve) -> Result<(), String>) -> Result<(), String> {
        with_resolver(f)
    }

    fn fetch(&self, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String> {
        let home = shards_ipc::home()?;
        let store = crate::pull::store(&home)?;
        let limits = crate::pull::limits()?;
        let limits = shards_image::store::Limits {
            bytes: http::GATEWAY_MOST.min(limits.bytes),
            ..limits
        };
        let stage = store.stage().map_err(|e| e.to_string())?;
        http::fetch_accepting(url, None, accept, stage.path().join("source"), &limits)
            .and_then(|d| std::fs::read(&d.path).map_err(|e| format!("{}: {e}", d.path.display())))
            .map_err(|e| format!("failed to load cache key: {e}"))
    }
}

/// A source's metadata resolved as a build resolves it, with nothing to show.
fn with_resolver(f: &dyn Fn(&dyn policy::Resolve) -> Result<(), String>) -> Result<(), String> {
    let home = shards_ipc::home()?;
    let store = crate::pull::store(&home)?;
    let progress = RefCell::new(Progress::quiet());
    let secrets = BTreeMap::new();
    let agents = super::Agents::new();
    let said = Said {
        debug: false,
        builder: &Here,
    };
    let bases = Bases {
        home: &home,
        store: &store,
        pull: false,
        progress: &progress,
        resolved: RefCell::new(BTreeMap::new()),
        layouts: BTreeMap::new(),
        artifacts: RefCell::new(BTreeMap::new()),
        pulled: RefCell::new(std::collections::BTreeSet::new()),
        secrets: &secrets,
        agents: &agents,
        answered: RefCell::new(BTreeMap::new()),
        steps: RefCell::new(BTreeMap::new()),
        epochs: RefCell::new(BTreeMap::new()),
        policies: None,
        policy_log: &said,
        refused: RefCell::new(None),
        local_dockerfile: false,
    };
    f(&super::PolicyMeta { bases: &bases })
}

/// runTest: the tests under the path, every name read in the working directory; an image
/// input's metadata resolved as a build resolves a source's, unseen. A failed test ends
/// the run with status 1 and nothing more (cobrautil.ExitCodeError).
fn test(parsed: &Parsed, builder: &dyn Builder) -> ExitCode {
    let path = parsed.args.first().map(String::as_str).unwrap_or_default();
    let root = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return error(&e.to_string()),
    };
    let opts = policy::tester::TestOptions {
        run: parsed.string("run").to_string(),
        filename: parsed.string("filename").to_string(),
        root,
    };
    let status = std::cell::Cell::new(0);
    let platform = match builder.platform() {
        Ok(p) => p,
        Err(e) => return error(&e),
    };
    let ran = builder.resolving(&|resolver| {
        let provider = Provider {
            platform: &platform,
            resolver,
        };
        let summary = policy::tester::run_policy_tests(path, &opts, Some(&provider))?;
        status.set(policy::tester::report(&summary, &mut std::io::stdout()));
        Ok(())
    });
    match ran {
        Ok(()) => ExitCode::from(status.get()),
        Err(e) => error(&e),
    }
}

/// `policy test`'s TestOptionsProvider: the builder's platform, and a source's metadata as
/// its builds resolve it.
struct Provider<'r> {
    platform: &'r Platform,
    resolver: &'r dyn policy::Resolve,
}

impl policy::tester::TestProvider for Provider<'_> {
    fn platform(&self) -> Result<Platform, String> {
        Ok(self.platform.clone())
    }

    fn resolve(&self, source: &policy::Source, req: &policy::MetaRequest) -> Result<policy::Meta, String> {
        self.resolver.resolve(source, req)
    }
}

/// The most of a request a step reads: a source, its attributes and what is asked of it,
/// as BuildKit's gateway takes them, in one `ResolveSourceMetaRequest` of at most
/// containerd's `DefaultMaxRecvMsgSize`, 16 MiB (BuildKit v0.33.0
/// frontend/gateway/gateway.go l. 459; containerd v2 defaults/defaults.go l. 22), as
/// [`policy::wire::ask`] writes them in JSON: each byte at most six (RFC 8259 §7, a control
/// character's `\u00XX`).
const ASK_MOST: u64 = 6 * (16 << 20);

/// A step of a client's policy command (D180), this host its builder, as the machine's
/// daemon runs it: `platform`, this host's; `resolve`, a source's metadata, the source and
/// what is asked of it read from stdin ([`policy::wire`]), resolved as a build here
/// resolves it, with the logins its client sent; `fetch URL ACCEPT`, an HTTP source's
/// content. Each answer on stdout; an error said on stderr alone, status 1, which the
/// client takes as the builder's error.
fn step(name: &str, args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let answered = match name {
        "platform" => policy::wire::platform(&host_platform()),
        "resolve" => {
            let mut asked = Vec::new();
            match std::io::stdin().take(ASK_MOST).read_to_end(&mut asked) {
                Ok(_) => policy::wire::read_ask(&asked).and_then(|(source, request)| {
                    let known = RefCell::new(Vec::new());
                    with_resolver(&|resolver| {
                        let meta = resolver.resolve(&source, &request)?;
                        *known.borrow_mut() = policy::wire::known(&meta)?;
                        Ok(())
                    })
                    .map(|()| known.into_inner())
                }),
                Err(e) => Err(format!("reading the request: {e}")),
            }
        }
        "fetch" => {
            let url = args.first().map(String::as_str).unwrap_or_default();
            let accept = args.get(1).map(String::as_str).filter(|a| !a.is_empty());
            Here.fetch(url, accept)
        }
        other => Err(format!("{other:?}: no step of a policy command")),
    };
    match answered {
        Ok(bytes) => match std::io::stdout().write_all(&bytes) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = write!(std::io::stderr(), "writing the answer: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            let _ = write!(std::io::stderr(), "{e}");
            ExitCode::FAILURE
        }
    }
}
