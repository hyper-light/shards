//! `shards buildx policy`: buildx v0.37.1's policy commands (commands/policy, D108): the
//! group, which says its commands; `eval`, a policy's decision for one source, or the
//! source's input (`--print`); and `test`, a policy's tests. Each runs here, its sources'
//! metadata resolved as a build resolves them, unseen, as buildx's resolver shows no
//! progress; the builder's platform is this host's (`host_platform`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use shards_cmdline::commands::{POLICY, POLICY_COMMANDS, POLICY_EVAL, POLICY_TEST};
use shards_cmdline::flags::{self, Command, Outcome, Parsed};
use shards_dockerfile::platform::{self, Platform};

use super::{Bases, Progress, host_platform, http, policy};

const GROUP: &str = "shards buildx policy";
const EVAL: &str = "shards buildx policy eval";
const TEST: &str = "shards buildx policy test";

/// `shards buildx policy ARGS…`.
pub fn command(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut argv = Vec::new();
    for a in args {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => return error(&format!("{a:?} is not UTF-8")),
        }
    }
    match argv.split_first() {
        Some((sub, rest)) if sub == "eval" => match read(&POLICY_EVAL, EVAL, rest) {
            Ok(parsed) => match eval(&parsed) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => error(&e),
            },
            Err(code) => code,
        },
        Some((sub, rest)) if sub == "test" => match read(&POLICY_TEST, TEST, rest) {
            Ok(parsed) => test(&parsed),
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

/// logrus as buildx's main formats it: the level, then the message.
fn log(level: &str, text: &str) {
    let _ = writeln!(std::io::stderr(), "{level}: {text}");
}

/// runEval.
fn eval(parsed: &Parsed) -> Result<(), String> {
    let arg = parsed.args.first().map(String::as_str).unwrap_or_default();
    let source = parse_source(arg)?;
    let platform = match parsed.string("platform") {
        "" => host_platform(),
        p => platform::parse(p.as_bytes(), &host_platform())
            .map(|p| platform::normalize(&p))
            .map_err(|e| {
                format!(
                    "invalid platform {}: {}",
                    shards_cmdline::go::quote(p),
                    String::from_utf8_lossy(&e)
                )
            })?,
    };
    with_resolver(&|resolver| {
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
        decide(parsed, &source, &platform, resolver)
    })
}

/// runEval's check: the policy file, then CheckPolicy asked at most four times, each
/// question of the source's metadata answered.
fn decide(
    parsed: &Parsed,
    source: &policy::Source,
    platform: &Platform,
    resolver: &dyn policy::Resolve,
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
    let log = Said { debug };
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
    use shards_dockerfile::git::{Parsed, parse_git_ref};
    if let Some(r) = input.strip_prefix("docker-image://") {
        let mut r = shards_image::reference::Reference::parse_normalized(r)
            .map_err(|e| format!("failed to parse image source reference: {e}"))?;
        if r.tag.is_none() && r.digest.is_none() {
            r.tag = Some("latest".into());
        }
        return Ok(policy::Source::new(format!("docker-image://{r}")));
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
/// its functions read fetched with no step to show.
struct Said {
    debug: bool,
}

impl policy::Log for Said {
    fn line(&self, text: &str) {
        if self.debug {
            log("DEBUG", text);
        }
    }

    fn fetch(&self, _name: &str, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String> {
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
    let said = Said { debug: false };
    let bases = Bases {
        home: &home,
        store: &store,
        pull: false,
        progress: &progress,
        resolved: RefCell::new(BTreeMap::new()),
        layouts: BTreeMap::new(),
        artifacts: RefCell::new(BTreeMap::new()),
        secrets: &secrets,
        agents: &agents,
        answered: RefCell::new(BTreeMap::new()),
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
fn test(parsed: &Parsed) -> ExitCode {
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
    let ran = with_resolver(&|resolver| {
        let summary = policy::tester::run_policy_tests(path, &opts, Some(&Builder { resolver }))?;
        status.set(policy::tester::report(&summary, &mut std::io::stdout()));
        Ok(())
    });
    match ran {
        Ok(()) => ExitCode::from(status.get()),
        Err(e) => error(&e),
    }
}

/// `policy test`'s TestOptionsProvider: the builder's platform, this host's, and a
/// source's metadata as a build resolves it.
struct Builder<'r> {
    resolver: &'r dyn policy::Resolve,
}

impl policy::tester::TestProvider for Builder<'_> {
    fn platform(&self) -> Result<Platform, String> {
        Ok(host_platform())
    }

    fn resolve(&self, source: &policy::Source, req: &policy::MetaRequest) -> Result<policy::Meta, String> {
        self.resolver.resolve(source, req)
    }
}
