//! `shards login registry` and `shards logout registry` (D171), `shards login` and `logout`
//! as Docker says them: a registry's credentials, checked with the registry as dockerd
//! checks them (moby daemon/pkg/registry/auth.go, loginV2) and kept as the Docker CLI keeps
//! them (docker/cli cli/command/registry/login.go, logout.go). What shards keeps, docker
//! finds, and what docker keeps, shards finds, as each pull and push reads them; the
//! daemon keeps none itself. Docker Hub's web login, which is Docker's own OAuth client,
//! is none of shards': its credentials are asked for at a terminal, as Docker asks them
//! where that login cannot start.

use std::ffi::OsString;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::process::ExitCode;

use shards_cmdline::commands::{LOGIN, LOGOUT};
use shards_cmdline::flags::{self, Parsed};
use shards_registry::ErrorKind;
use shards_registry::auth::Credentials;
use shards_registry::credentials::{self, Kept, Login};

/// Docker Hub's key, which `docker login` keeps its credentials under.
pub(crate) const HUB: &str = "https://index.docker.io/v1/";

/// docker/cli's file store's warning (credentials/file_store.go), said once a password
/// is kept in the config file.
const UNENCRYPTED: &str = "\nWARNING! Your credentials are stored unencrypted in '{}'.\nConfigure a credential helper to remove this warning. See\nhttps://docs.docker.com/go/credential-store/\n";

/// `ConvertToHostname`: the host and port a server's key names.
pub(crate) fn hostname(server: &str) -> &str {
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    rest.split('/').next().unwrap_or_default()
}

/// The registry a login to `server` is checked with: Docker Hub's for its key.
pub(crate) fn registry_host(server: &str) -> &str {
    if server == HUB {
        "registry-1.docker.io"
    } else {
        hostname(server)
    }
}

/// What a client's environment holds: its credentials' helpers, config and certificates.
fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// `text` on stderr, and status 1, as the Docker CLI fails a command; in a panel under
/// `command`'s head on a colour terminal.
#[cfg_attr(not(unix), allow(unused_variables))]
fn failed(command: &str, text: &str) -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = crate::cli::look::styled_err() {
        let first = text.lines().next().unwrap_or(text);
        let first = ["error: ", "shards: "]
            .iter()
            .find_map(|prefix| first.strip_prefix(prefix))
            .unwrap_or(first);
        crate::cli::look::error(&p, command, first, &[]);
        return ExitCode::from(1);
    }
    let _ = writeln!(std::io::stderr(), "{text}");
    ExitCode::from(1)
}

/// docker/cli's `tui` note, `header` before its first line and each further line set in by
/// its width (internal/tui/note.go), plain: what shards says where the Docker CLI says one.
fn note(header: &str, text: &str) -> String {
    let mut out = format!("\n{header}");
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push_str(&" ".repeat(header.len()));
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// DOCKER_AUTH_CONFIG, where it is set, takes the place of what a login keeps
/// (maybePrintEnvAuthWarning): said, as the Docker CLI says it.
fn warn_env() {
    if std::env::var_os("DOCKER_AUTH_CONFIG").is_some_and(|v| !v.is_empty()) {
        let _ = write!(
            std::io::stderr(),
            "{}",
            note(
                " Warn -> ",
                "DOCKER_AUTH_CONFIG is set and takes precedence.\nUnset DOCKER_AUTH_CONFIG to restore the CLI auth behaviour.\n"
            )
        );
    }
}

/// `args` as a command line of `command`, at `path`: its parse, or how it ended, as the
/// CLI reads every command's (its help and mistakes shards' own on a colour terminal).
fn parsed(
    command: &'static flags::Command,
    path: &str,
    args: impl Iterator<Item = OsString>,
) -> Result<Parsed, ExitCode> {
    let mut argv = Vec::new();
    for a in args {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => {
                let name = path.strip_prefix("shards ").unwrap_or(path);
                return Err(failed(name, &format!("{a:?} is not UTF-8")));
            }
        }
    }
    let parsed = crate::cli::read(command, path, &argv, &flags::value)?;
    let _ = write!(std::io::stdout(), "{}", parsed.notices);
    Ok(parsed)
}

/// verifyLoginFlags, then verifyLoginOptions (docker/cli v29.8.1): the user and password
/// a login is given, the password from stdin with `--password-stdin` or `-p -`, its line
/// end left off and nothing else (NIST SP 800-63B §5.1.1.2, as docker/cli reads it).
fn given(parsed: &Parsed) -> Result<(String, String), String> {
    let (user, mut password) = (
        parsed.string("username").to_string(),
        parsed.string("password").to_string(),
    );
    if parsed.changed("password-stdin") || password == "-" {
        if parsed.changed("password") && password != "-" {
            return Err("conflicting options: cannot specify both --password and --password-stdin".into());
        }
        if !parsed.changed("username") {
            return Err("the --password-stdin option requires --username to be set".into());
        }
    }
    if parsed.changed("username") && user.is_empty() {
        return Err("username is empty".into());
    }
    if parsed.changed("password") && password.is_empty() {
        return Err("password is empty".into());
    }
    let mut from_stdin = parsed.bool("password-stdin");
    if password == "-" {
        password.clear();
        from_stdin = true;
    }
    if !password.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "WARNING! Using --password via the CLI is insecure. Use --password-stdin."
        );
    }
    if from_stdin {
        let mut read = Vec::new();
        std::io::stdin()
            .read_to_end(&mut read)
            .map_err(|e| e.to_string())?;
        let mut text = String::from_utf8(read).map_err(|_| "the password is not UTF-8".to_string())?;
        for end in ["\r\n", "\n", "\r"] {
            if let Some(cut) = text.strip_suffix(end) {
                text = cut.to_string();
                break;
            }
        }
        if text.trim().is_empty() {
            return Err("password is empty".into());
        }
        password = text;
    }
    Ok((user, password))
}

/// Why a login did not succeed: its credentials refused by the registry, or anything
/// else (the registry not reached, the credentials not kept), as docker/cli tells them
/// apart (`errdefs.IsUnauthorized`). Each says what happened.
enum Failure {
    Refused(String),
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(text) | Failure::Other(text) => f.write_str(text),
        }
    }
}

/// Checks `username` and `password` with `server`'s registry, then keeps them, or the
/// identity token its token server gave in their place.
fn check_and_keep(server: &str, username: &str, password: &str) -> Result<(), Failure> {
    let credentials = Credentials::Password {
        username: username.to_string(),
        password: password.to_string(),
    };
    let token = crate::pull::check_login(registry_host(server), credentials, &env).map_err(|e| {
        if e.kind() == ErrorKind::Unauthorized {
            Failure::Refused(e.to_string())
        } else {
            Failure::Other(e.to_string())
        }
    })?;
    let login = Login {
        server: server.to_string(),
        username: username.to_string(),
        password: password.to_string(),
        identity_token: token.unwrap_or_default(),
    };
    let kept = credentials::store(&login, &env)
        .map_err(|e| Failure::Other(format!("error saving credentials: {e}")))?;
    if let Kept::File(path) = kept
        && login.identity_token.is_empty()
    {
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            UNENCRYPTED.replace("{}", &path.display().to_string())
        );
    }
    Ok(())
}

/// `shards login [SERVER]`: Docker Hub's unless SERVER names another. Credentials kept
/// before are tried first where none are given; else the user and password given, asked
/// for at a terminal where they are not.
pub fn login(args: impl Iterator<Item = OsString>) -> ExitCode {
    let parsed = match parsed(&LOGIN, "shards login", args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let (mut user, mut password) = match given(&parsed) {
        Ok(g) => g,
        Err(e) => return failed("login", &e),
    };
    warn_env();
    let server = match parsed.args.first() {
        Some(s) if !s.is_empty() && s != "docker.io" => s.clone(),
        _ => HUB.to_string(),
    };
    let domain = if server == HUB {
        "docker.io"
    } else {
        hostname(&server)
    };
    // What a login kept before, where nothing is given (GetDefaultAuthConfig).
    let mut stored_user = String::new();
    if user.is_empty()
        && password.is_empty()
        && let Ok((
            Credentials::Password {
                username,
                password: stored,
            },
            _,
        )) = credentials::lookup(domain, &env)
    {
        stored_user.clone_from(&username);
        // On a terminal in shards' own words; piped, what a `docker` script reads.
        let other = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            "To log in as another account, run 'shards logout registry', then 'shards login registry'"
        } else {
            "To login with a different account, run 'shards logout' followed by 'shards login'"
        };
        let _ = write!(
            std::io::stderr(),
            "Authenticating with existing credentials... [Username: {username}]\n{}\n\n",
            note(" Info -> ", other)
        );
        match check_and_keep(&server, &username, &stored) {
            Ok(()) => {
                let _ = writeln!(std::io::stdout(), "Login Succeeded");
                return ExitCode::SUCCESS;
            }
            Err(Failure::Refused(_)) => {
                let _ = writeln!(std::io::stderr(), "Stored credentials invalid or expired");
            }
            Err(Failure::Other(e)) => {
                let _ = writeln!(std::io::stderr(), "Login did not succeed, error: {e}");
            }
        }
    }
    if (user.is_empty() || password.is_empty()) && !std::io::stdin().is_terminal() {
        return failed(
            "login",
            "error: cannot perform an interactive login from a non-TTY device",
        );
    }
    if user.is_empty() {
        let ask = if stored_user.is_empty() {
            "Username: ".to_string()
        } else {
            format!("Username ({stored_user}): ")
        };
        let _ = write!(std::io::stdout(), "{ask}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if let Err(e) = std::io::stdin().read_line(&mut line) {
            return failed("login", &e.to_string());
        }
        user = match line.trim() {
            "" => stored_user.clone(),
            typed => typed.to_string(),
        };
        if user.is_empty() {
            return failed("login", "error: username is required");
        }
    }
    if password.trim().is_empty() {
        let _ = write!(std::io::stdout(), "Password: ");
        let _ = std::io::stdout().flush();
        password = match crate::cli::terminal::read_without_echo() {
            Ok(p) => p,
            Err(e) => return failed("login", &e.to_string()),
        };
        let _ = writeln!(std::io::stdout());
        if password.is_empty() {
            return failed("login", "error: password is required");
        }
    }
    match check_and_keep(&server, &user, &password) {
        Ok(()) => {
            let _ = writeln!(std::io::stdout(), "Login Succeeded");
            ExitCode::SUCCESS
        }
        Err(e) => failed("login", &format!("shards: {e}")),
    }
}

/// `shards logout [SERVER]`: what a login kept for SERVER, Docker Hub's unless it names
/// another, forgotten under each key a login may have kept it by.
pub fn logout(args: impl Iterator<Item = OsString>) -> ExitCode {
    let parsed = match parsed(&LOGOUT, "shards logout", args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    warn_env();
    let (server, shown) = match parsed.args.first() {
        Some(s) if !s.is_empty() => (s.clone(), hostname(s).to_string()),
        _ => (HUB.to_string(), HUB.to_string()),
    };
    let _ = writeln!(std::io::stdout(), "Removing login credentials for {shown}");
    match credentials::erase(&server, &env) {
        Ok((forgot, failures)) => {
            if !forgot && !failures.is_empty() {
                let mut said = String::from("WARNING: could not erase credentials:\n");
                for f in failures {
                    said.push_str(&f);
                    said.push('\n');
                }
                let _ = write!(std::io::stderr(), "{said}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => failed("logout", &format!("shards: {e}")),
    }
}
