//! Contexts (D180): another machine's shards, which this one's commands run on whole, over
//! QUIC. A home keeps the contexts it joined (`contexts/NAME`) and the one its commands use
//! (`context`); the machine that runs the microVMs keeps the clients it enrolled
//! (`clients/NAME`) and the invitations it made (`invitations/NAME`), and its daemon, where it
//! listens, its address (`listening`). Each a short text, readable by its user alone.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use shards_remote::client::{self, Remote};
use shards_remote::frame::{self, Request};
use shards_remote::identity::{self, Identity};
use shards_remote::invite::{self, Invitation};

/// The exit status of a command shards did not run, as Docker's CLI exits for an error
/// before its command runs.
const NOT_RUN: u8 = 125;

/// A context's name, as docker/cli names one (v29.8.1 cli/context/store, ValidateContextName):
/// a letter or digit, then letters, digits, `_`, `.`, `+` or `-`; and not `default`, this
/// machine's.
fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.len() > 1
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// The context this machine's own shards is.
pub(crate) const DEFAULT: &str = "default";

/// A context: another machine's daemon, its address and its key's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Context {
    pub name: String,
    pub description: String,
    pub address: SocketAddr,
    pub daemon: identity::Fingerprint,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn at(path: &Path) -> impl Fn(io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

/// `bytes` written to `path`, a new file readable by this user alone; an error where a file
/// is there already.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(at(path))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(at(path))
}

/// `bytes` in place of what `path` holds, whole: written beside it, then renamed over it.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let new = path.with_extension(format!("new-{}", std::process::id()));
    let _ = fs::remove_file(&new);
    write_new(&new, bytes)?;
    fs::rename(&new, path).map_err(at(path))
}

fn dir(home: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = home.join(name);
    shards_vmm::platform::create_private_dir(&dir).map_err(at(&dir))?;
    Ok(dir)
}

/// The home's identity, made the first time one is needed.
pub(crate) fn identity(home: &Path) -> Result<Identity, String> {
    shards_vmm::platform::create_private_dir(home).map_err(at(home))?;
    Identity::load_or_create(&home.join("identity")).map_err(|e| e.to_string())
}

/// The fields of a record's lines `key value`, after its first line, which must be `header`.
fn fields(text: &str, header: &str) -> Option<Vec<(String, String)>> {
    let mut lines = text.lines();
    (lines.next()? == header).then(|| {
        lines
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    })
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(bytes.get(i * 2..i * 2 + 2)?).ok()?;
        *b = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

const CONTEXT: &str = "shards context 1";

impl Context {
    fn text(&self) -> String {
        format!(
            "{CONTEXT}\nname {}\naddress {}\ndaemon {}\ndescription {}\n",
            self.name,
            self.address,
            identity::hex(&self.daemon),
            self.description.replace('\n', " ")
        )
    }

    fn parse(text: &str) -> Option<Context> {
        let fields = fields(text, CONTEXT)?;
        Some(Context {
            name: field(&fields, "name")?.to_string(),
            description: field(&fields, "description").unwrap_or_default().to_string(),
            address: field(&fields, "address")?.parse().ok()?,
            daemon: unhex32(field(&fields, "daemon")?)?,
        })
    }

    /// Where its daemon is.
    pub(crate) fn remote(&self) -> Remote {
        Remote {
            address: self.address,
            daemon: self.daemon,
        }
    }
}

/// The context `name`, if this home joined one by that name.
pub(crate) fn load(home: &Path, name: &str) -> Result<Option<Context>, String> {
    if !valid_name(name) {
        return Ok(None);
    }
    let path = home.join("contexts").join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Context::parse(&text)
            .map(Some)
            .ok_or_else(|| format!("{}: not a context's record", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(&path)(e)),
    }
}

/// Every context this home joined, by name.
fn list(home: &Path) -> Result<Vec<Context>, String> {
    let dir = home.join("contexts");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(at(&dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry.map_err(at(&dir))?.file_name();
        if let Some(name) = name.to_str()
            && let Some(context) = load(home, name)?
        {
            out.push(context);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The context the home's commands use, `default` where none is chosen.
fn current(home: &Path) -> String {
    fs::read_to_string(home.join("context"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// What runs a command: this machine, or a context's.
pub(crate) enum Chosen {
    Here,
    There(Context),
}

/// The context `args`' command runs in, its own options taken from the front of `args`:
/// `--context NAME` (`-c NAME`, `--context=NAME`), else `SHARDS_CONTEXT`, else
/// `DOCKER_CONTEXT`, else the home's current one, as docker/cli chooses (v29.8.1
/// cli/command/cli.go, resolveContextName); `default` is this machine.
pub(crate) fn chosen(home: &Path, args: &mut Vec<OsString>) -> Result<Chosen, String> {
    let mut given = None;
    loop {
        let first = args.first().and_then(|a| a.to_str()).map(str::to_string);
        match first.as_deref() {
            Some("--context" | "-c") => {
                let name = args
                    .get(1)
                    .and_then(|a| a.to_str())
                    .ok_or("flag needs an argument: --context")?
                    .to_string();
                args.drain(..2);
                given = Some(name);
            }
            Some(flag) if flag.starts_with("--context=") => {
                given = Some(flag.trim_start_matches("--context=").to_string());
                args.remove(0);
            }
            _ => break,
        }
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let name = match given {
        Some(name) => name,
        None => match env("SHARDS_CONTEXT").or_else(|| env("DOCKER_CONTEXT")) {
            Some(name) => name,
            None => current(home),
        },
    };
    if name == DEFAULT {
        return Ok(Chosen::Here);
    }
    match load(home, &name)? {
        Some(context) => Ok(Chosen::There(context)),
        None => Err(format!("context {name:?} does not exist")),
    }
}

/// What a command needs that a context does not carry yet: the client's terminal, or files
/// of the client's that Docker's CLI sends where shards would read the other machine's. The
/// command is read as shards reads it, its own grammar said as the command it runs, and its
/// flags by its parser.
fn not_carried(args: &[OsString]) -> Option<String> {
    let words: Vec<String> = args
        .iter()
        .map_while(|a| a.to_str().map(str::to_string))
        .collect();
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let first = refs.first().copied().unwrap_or_default();
    if matches!(
        first,
        "build" | "buildx" | "builder" | "load" | "import" | "cp" | "compose"
    ) || refs
        .get(..2)
        .is_some_and(|w| matches!(w, ["container" | "image", "cp" | "load" | "import"]))
    {
        return Some(format!(
            "{first} over a context sends this machine's files, which shards does not carry yet: run it on the machine itself"
        ));
    }
    if matches!(refs.as_slice(), ["attach", ..] | ["container", "attach", ..]) {
        return Some("attach: a terminal on another machine is not carried by a context yet".into());
    }
    // `run` is the CLI's own to read (request.rs), as Docker's flags are its.
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&shards_cmdline::commands::RUN, "shards run", 1),
        ["container", "run", ..] => (&shards_cmdline::commands::RUN, "shards container run", 2),
        _ => shards_cmdline::commands::find(&refs)?,
    };
    let rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return None;
    };
    if parsed.bool("interactive") || parsed.bool("tty") {
        return Some(format!(
            "{path}: a terminal on another machine is not carried by a context yet"
        ));
    }
    let files = !parsed.many("env-file").is_empty()
        || !parsed.many("label-file").is_empty()
        || !parsed.string("cidfile").is_empty()
        || !parsed.string("output").is_empty();
    files.then(|| format!("{path}: a file of this machine's, which a context does not carry yet"))
}

/// Whether `args`, the command after shards', is a context's own, which runs on this machine
/// whatever the context: what it joined, and who joins it.
pub(crate) fn kept_here(args: &[OsString]) -> bool {
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    let own = |w: &str| {
        matches!(
            w,
            "context" | "contexts" | "client" | "clients" | "invitation" | "invitations"
        )
    };
    words.is_empty()
        || words
            .first()
            .is_some_and(|w| own(w) || matches!(*w, "help" | "-h" | "--help"))
        || words.get(1).is_some_and(|w| own(w))
}

/// Whether this home's daemon serves now.
fn daemon_serves(home: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(home.join(shards_ipc::SOCKET)).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        false
    }
}

/// Where this home's daemon serves its own CLI.
fn own_endpoint(home: &Path) -> String {
    #[cfg(unix)]
    {
        format!("unix://{}", home.join(shards_ipc::SOCKET).display())
    }
    #[cfg(not(unix))]
    {
        home.display().to_string()
    }
}

/// Runs `args` on `context`'s machine: what it prints, here, and its exit status, this
/// process's.
pub(crate) fn forward(home: &Path, context: &Context, args: Vec<OsString>) -> ExitCode {
    let fail = |why: String| {
        let _ = writeln!(io::stderr(), "shards: {why}");
        ExitCode::from(NOT_RUN)
    };
    if let Some(why) = not_carried(&args) {
        return fail(why);
    }
    let identity = match identity(home) {
        Ok(identity) => identity,
        Err(e) => return fail(e),
    };
    let mut bytes = Vec::with_capacity(args.len());
    for arg in &args {
        match arg.to_str() {
            Some(text) => bytes.push(text.as_bytes().to_vec()),
            None => {
                return fail(format!(
                    "{arg:?}: an argument that is not UTF-8 does not cross to another machine"
                ));
            }
        }
    }
    let env = frame::ENV
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|v| (name.to_string(), v.into_bytes()))
        })
        .collect();
    let request = Request { args: bytes, env };
    let stdin: Box<dyn io::Read + Send> = Box::new(io::stdin());
    match client::exec(
        &identity,
        context.remote(),
        request,
        Some(stdin),
        io::stdout(),
        io::stderr(),
    ) {
        Ok(Ok(status)) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Ok(Err(refused)) => fail(format!("{}: {refused}", context.name)),
        Err(e) => fail(format!("{}: {e}", context.name)),
    }
}

/// `shards context ...`: the contexts this home joined.
pub(crate) fn command(args: &[OsString]) -> ExitCode {
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&e),
    };
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    let result = match words.as_slice() {
        ["create", name, rest @ ..] => create(&home, name, rest),
        ["use", name] => use_context(&home, name),
        ["ls" | "list"] => list_contexts(&home),
        ["inspect", names @ ..] if !names.is_empty() => inspect(&home, names),
        ["rm" | "remove", names @ ..] if !names.is_empty() => remove(&home, names),
        ["show"] => {
            let _ = writeln!(io::stdout(), "{}", current(&home));
            Ok(())
        }
        _ => Err(CONTEXT_USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

const CONTEXT_USAGE: &str = "usage: shards create context NAME --invite-file FILE [--description TEXT]
       shards use context NAME
       shards list contexts
       shards inspect context NAME...
       shards remove context NAME...
  A context is another machine's shards, which this one's commands run on whole: its
  microVMs, images, volumes and files. Its owner invites this machine (shards invite
  client), and the invitation's file enrolls it once. --context NAME, SHARDS_CONTEXT or
  DOCKER_CONTEXT choose one for a command; `default` is this machine.";

fn failed(why: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {why}");
    ExitCode::FAILURE
}

fn create(home: &Path, name: &str, rest: &[&str]) -> Result<(), String> {
    if name == DEFAULT || !valid_name(name) {
        return Err(format!(
            "context name {name:?} is invalid, names are validated against regexp \"^[a-zA-Z0-9][a-zA-Z0-9_.+-]+$\""
        ));
    }
    // Both flags, each at most once, in any order.
    let mut file = None;
    let mut description = None;
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        match *word {
            "--invite-file" => {
                file = Some(*rest.get(i + 1).ok_or("flag needs an argument: --invite-file")?);
                i += 2;
            }
            "--description" => {
                description = Some(*rest.get(i + 1).ok_or("flag needs an argument: --description")?);
                i += 2;
            }
            "--docker" | "--from" => {
                return Err(format!(
                    "{word}: a shards context is another machine's shards, joined by its owner's invitation (--invite-file); Docker's endpoints are Docker's"
                ));
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let file = file.ok_or("shards create context NAME --invite-file FILE: the invitation the machine's owner made (shards invite client)")?;
    if load(home, name)?.is_some() {
        return Err(format!("context {name:?} already exists"));
    }
    let text = fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let invitation = Invitation::parse(&text).map_err(|e| format!("{file}: {e}"))?;
    let identity = identity(home)?;
    match client::join(&identity, &invitation) {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => return Err(format!("{}: {refused}", invitation.address)),
        Err(e) => return Err(e.to_string()),
    }
    let context = Context {
        name: name.to_string(),
        description: description.unwrap_or_default().to_string(),
        address: invitation.address,
        daemon: invitation.daemon,
    };
    write_new(&dir(home, "contexts")?.join(name), context.text().as_bytes())?;
    let _ = writeln!(io::stdout(), "{name}");
    let _ = writeln!(io::stderr(), "Successfully created context {name:?}");
    Ok(())
}

fn use_context(home: &Path, name: &str) -> Result<(), String> {
    if name != DEFAULT && load(home, name)?.is_none() {
        return Err(format!("context {name:?} does not exist"));
    }
    shards_vmm::platform::create_private_dir(home).map_err(at(home))?;
    replace(&home.join("context"), format!("{name}\n").as_bytes())?;
    let _ = writeln!(io::stdout(), "{name}");
    let _ = writeln!(io::stderr(), "Current context is now {name:?}");
    Ok(())
}

fn list_contexts(home: &Path) -> Result<(), String> {
    let current = current(home);
    let mut rows = vec![(
        format!("{DEFAULT}{}", if current == DEFAULT { " *" } else { "" }),
        "this machine's shards".to_string(),
        own_endpoint(home),
    )];
    for c in list(home)? {
        let mark = if c.name == current { " *" } else { "" };
        rows.push((
            format!("{}{mark}", c.name),
            c.description,
            format!("quic://{}", c.address),
        ));
    }
    let width = |f: &dyn Fn(&(String, String, String)) -> usize, header: usize| {
        rows.iter().map(f).max().unwrap_or(0).max(header)
    };
    let (a, b) = (width(&|r| r.0.len(), 4), width(&|r| r.1.len(), 11));
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{:a$}   {:b$}   ENDPOINT", "NAME", "DESCRIPTION");
    for (name, description, endpoint) in rows {
        let _ = writeln!(out, "{name:a$}   {description:b$}   {endpoint}");
    }
    Ok(())
}

fn inspect(home: &Path, names: &[&str]) -> Result<(), String> {
    let mut docs = Vec::new();
    for name in names {
        let c = load(home, name)?.ok_or_else(|| format!("context {name:?} does not exist"))?;
        docs.push(serde_json::json!({
            "Name": c.name,
            "Description": c.description,
            "Endpoint": format!("quic://{}", c.address),
            "Daemon": identity::hex(&c.daemon),
        }));
    }
    let text = serde_json::to_string_pretty(&docs).map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{text}");
    Ok(())
}

fn remove(home: &Path, names: &[&str]) -> Result<(), String> {
    for name in names {
        if load(home, name)?.is_none() {
            return Err(format!("context {name:?} does not exist"));
        }
        let path = home.join("contexts").join(name);
        fs::remove_file(&path).map_err(at(&path))?;
        if current(home) == *name {
            let _ = fs::remove_file(home.join("context"));
        }
        let _ = writeln!(io::stdout(), "{name}");
    }
    Ok(())
}

// What the machine that runs the microVMs keeps: its clients and invitations.

const CLIENT: &str = "shards client 1";
const INVITATION: &str = "shards invitation record 1";

/// A client this machine's daemon enrolled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client {
    pub name: String,
    pub key: Vec<u8>,
    pub enrolled: u64,
}

fn read_client(path: &Path) -> Option<Client> {
    let text = fs::read_to_string(path).ok()?;
    let fields = fields(&text, CLIENT)?;
    let hex = field(&fields, "key")?;
    let key = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(Client {
        name: field(&fields, "name")?.to_string(),
        key,
        enrolled: field(&fields, "enrolled")?.parse().ok()?,
    })
}

/// Every client this machine enrolled, by name.
pub(crate) fn clients(home: &Path) -> Vec<Client> {
    let mut out: Vec<Client> = fs::read_dir(home.join("clients"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_client(&e.path()))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Enrolls `key` as client `name`: the daemon's, which listens on Unix alone.
#[cfg(unix)]
pub(crate) fn enroll(home: &Path, name: &str, key: &[u8]) -> Result<(), String> {
    let text = format!(
        "{CLIENT}\nname {name}\nkey {}\nenrolled {}\n",
        identity::hex(key),
        now()
    );
    write_new(&dir(home, "clients")?.join(name), text.as_bytes())
}

/// The invitation `name` this machine made, and when, if it holds one.
pub(crate) fn invitation(home: &Path, name: &str) -> Option<(Invitation, u64)> {
    if !invite::valid_name(name) {
        return None;
    }
    let text = fs::read_to_string(home.join("invitations").join(name)).ok()?;
    let (head, rest) = text.split_once('\n')?;
    let (header, created) = (head, rest.lines().next()?);
    if header != INVITATION {
        return None;
    }
    let created = created.strip_prefix("created ")?.parse().ok()?;
    let body = rest.split_once('\n')?.1;
    Some((Invitation::parse(body).ok()?, created))
}

/// Whether an invitation made at `created` has lapsed by now.
pub(crate) fn lapsed(created: u64) -> bool {
    now().saturating_sub(created) >= invite::LIFETIME_SECS
}

/// Redeems invitation `name`: removed, so that it enrolls one client alone.
pub(crate) fn redeem(home: &Path, name: &str) -> Result<(), String> {
    let path = home.join("invitations").join(name);
    fs::remove_file(&path).map_err(at(&path))
}

/// Where this home's daemon listens for clients, while it does.
pub(crate) const LISTENING: &str = "listening";

/// `shards client ...` and `shards invitation ...`: who may join this machine, and who has.
pub(crate) fn admin(thing: &str, args: &[OsString]) -> ExitCode {
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&e),
    };
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    let result = match (thing, words.as_slice()) {
        ("client", ["invite", rest @ ..]) => invite_client(&home, rest),
        ("client", ["ls" | "list"]) => {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "NAME\tKEY\tENROLLED");
            for c in clients(&home) {
                let key = identity::hex(&identity::fingerprint(&c.key));
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}",
                    c.name,
                    key.get(..12).unwrap_or_default(),
                    c.enrolled
                );
            }
            Ok(())
        }
        ("client", ["rm" | "remove", names @ ..]) if !names.is_empty() => names.iter().try_for_each(|name| {
            let path = home.join("clients").join(name);
            if !invite::valid_name(name) || !path.is_file() {
                return Err(format!("no client {name:?}"));
            }
            fs::remove_file(&path).map_err(at(&path))?;
            let _ = writeln!(io::stdout(), "{name}");
            Ok(())
        }),
        ("invitation", ["ls" | "list"]) => {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "NAME\tADDRESS\tMADE\tLAPSED");
            let mut names: Vec<String> = fs::read_dir(home.join("invitations"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            for name in names {
                if let Some((i, created)) = invitation(&home, &name) {
                    let _ = writeln!(out, "{}\t{}\t{created}\t{}", i.name, i.address, lapsed(created));
                }
            }
            Ok(())
        }
        ("invitation", ["rm" | "remove", names @ ..]) if !names.is_empty() => {
            names.iter().try_for_each(|name| {
                if invitation(&home, name).is_none() {
                    return Err(format!("no invitation {name:?}"));
                }
                redeem(&home, name)?;
                let _ = writeln!(io::stdout(), "{name}");
                Ok(())
            })
        }
        _ => Err(ADMIN_USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

const ADMIN_USAGE: &str = "usage: shards invite client --name NAME --output FILE
       shards list clients
       shards remove client NAME...
       shards list invitations
       shards remove invitation NAME...
  On the machine that runs the microVMs, whose daemon listens for clients (shards run
  daemon --listen ADDR:PORT): an invitation, a private file that enrolls one client as
  NAME and lapses an hour after it is made, and the clients enrolled.";

fn invite_client(home: &Path, rest: &[&str]) -> Result<(), String> {
    let mut name = None;
    let mut output = None;
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        match *word {
            "--name" => {
                name = Some(*rest.get(i + 1).ok_or("flag needs an argument: --name")?);
                i += 2;
            }
            "--output" | "-o" => {
                output = Some(*rest.get(i + 1).ok_or("flag needs an argument: --output")?);
                i += 2;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let (Some(name), Some(output)) = (name, output) else {
        return Err(ADMIN_USAGE.to_string());
    };
    let listening = fs::read_to_string(home.join(LISTENING))
        .ok()
        .and_then(|a| a.trim().parse::<SocketAddr>().ok())
        .filter(|_| daemon_serves(home))
        .ok_or("this machine's daemon does not listen for clients: start it with `shards run daemon --listen ADDR:PORT`")?;
    if listening.ip().is_unspecified() {
        return Err(format!(
            "the daemon listens on {listening}, which names no address a client can reach: start it with --listen naming one"
        ));
    }
    if clients(home).iter().any(|c| c.name == name) {
        return Err(format!("a client named {name:?} is enrolled already"));
    }
    if invitation(home, name).is_some_and(|(_, created)| !lapsed(created)) {
        return Err(format!("an invitation named {name:?} is open already"));
    }
    let identity = identity(home)?;
    let made = Invitation::new(name, listening, identity.fingerprint()).map_err(|e| e.to_string())?;
    let record = format!("{INVITATION}\ncreated {}\n{}", now(), made.text());
    let path = dir(home, "invitations")?.join(name);
    let _ = fs::remove_file(&path);
    write_new(&path, record.as_bytes())?;
    made.write(Path::new(output)).map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{output}");
    let _ = writeln!(
        io::stderr(),
        "Invited {name}: the invitation in {output} enrolls one client, and lapses in an hour"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A context's record reads back as written; names are Docker's.
    #[test]
    fn a_contexts_record_reads_back() {
        let c = Context {
            name: "gpu".into(),
            description: "the desktop".into(),
            address: "192.168.1.77:7350".parse().unwrap(),
            daemon: [5; 32],
        };
        assert_eq!(Context::parse(&c.text()), Some(c));
        assert!(valid_name("gpu-desktop.1"));
        assert!(!valid_name("g"));
        assert!(!valid_name("-gpu"));
        assert!(!valid_name("gpu desktop"));
    }

    /// What a context carries, and what it refuses until it carries it.
    #[test]
    fn a_context_refuses_what_it_does_not_carry() {
        let args = |s: &str| s.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert!(not_carried(&args("ps -a")).is_none());
        assert!(not_carried(&args("run --rm alpine echo hi")).is_none());
        assert!(
            not_carried(&args("run --rm alpine ls -t")).is_none(),
            "the command's own -t"
        );
        assert!(not_carried(&args("run -it alpine sh")).is_some());
        assert!(not_carried(&args("run --interactive alpine sh")).is_some());
        assert!(not_carried(&args("build .")).is_some());
        assert!(not_carried(&args("save -o x.tar alpine")).is_some());
        assert!(not_carried(&args("run --env-file e alpine")).is_some());
        assert!(
            not_carried(&args("run --name web -it alpine sh")).is_some(),
            "past a flag's value"
        );
        assert!(
            not_carried(&args("run vm -it alpine sh")).is_some(),
            "shards' own words"
        );
        assert!(kept_here(&args("context ls")));
        assert!(kept_here(&args("list contexts")));
        assert!(!kept_here(&args("ps")));
    }
}
