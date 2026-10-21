//! Credentials, found as the Docker CLI v29.8.1 finds them (docs/research/registry-pull.md
//! §3.3), so `docker login` works unchanged:
//! - `config.json` from `$DOCKER_CONFIG`, else `~/.docker` (`cli/config/config.go`);
//! - a registry's key is its host, or `https://index.docker.io/v1/` for Docker Hub;
//! - `SHARDS_AUTH_CONFIG`, shards' own, else `DOCKER_AUTH_CONFIG`, first; then the
//!   registry's credential helper, the config's `credsStore`, or its `auths`
//!   (`configfile/file.go`, `credentials/`);
//! - the platform's default helper when the config holds no credentials at all;
//! - dockerd's precedence: a registry token, then an identity token, then a password
//!   (`daemon/containerd/resolver.go`).
//!
//! A login is kept as `docker login` keeps one ([`store`]), and forgotten as `docker
//! logout` forgets it ([`erase`]).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use crate::Error;
use crate::auth::Credentials;
use crate::http::Cancel;

/// Where `docker login` keeps Docker Hub's credentials.
const HUB_KEY: &str = "https://index.docker.io/v1/";
/// What a helper answers for a host it has nothing for (docker-credential-helpers v0.9.9
/// `credentials/error.go`).
const NOT_FOUND: &str = "credentials not found in native keychain";
/// A helper's username that marks its secret as an identity token (`native_store.go`).
const TOKEN_USERNAME: &str = "<token>";
/// A helper's answer is read up to this size.
const MAX_HELPER_OUTPUT: u64 = 1 << 20;

/// The environment variables the lookup reads, passed in so it can be tested.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, Entry>,
    #[serde(default, rename = "credsStore")]
    creds_store: String,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Deserialize, Default, Clone)]
struct Entry {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    identitytoken: String,
    #[serde(default)]
    registrytoken: String,
}

/// `SHARDS_AUTH_CONFIG` or `DOCKER_AUTH_CONFIG`: `auths` alone, each with `auth` alone; or
/// in shards' own, a registry's token or an identity token in its place, as `config.json`
/// keeps a login of one, which a context's client sends of its own (D180).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvConfig {
    #[serde(default)]
    auths: BTreeMap<String, EnvEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvEntry {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    identitytoken: String,
    #[serde(default)]
    registrytoken: String,
}

/// The helper protocol's answer (`credentials/credentials.go`).
#[derive(Deserialize)]
struct HelperAnswer {
    #[serde(rename = "Username", default)]
    username: String,
    #[serde(rename = "Secret", default)]
    secret: String,
}

/// What a lookup found for a registry: its credentials, or the credential helper asked
/// for them, whose answer is read only where it is needed ([`Asked`]).
#[derive(Debug)]
pub enum Found {
    Known(Credentials),
    Asked(Asked),
}

impl From<Credentials> for Found {
    fn from(credentials: Credentials) -> Found {
        Found::Known(credentials)
    }
}

impl Found {
    /// The credentials: a helper's answer, waited for, where one was asked.
    pub fn answer(self) -> Result<Credentials, Error> {
        match self {
            Found::Known(credentials) => Ok(credentials),
            Found::Asked(asked) => asked.answer(),
        }
    }
}

/// The credentials `docker login` left for the registry `domain` (a reference's domain),
/// and any warnings Docker would print on the way: [`find`]'s, a helper's answer waited
/// for.
pub fn lookup(domain: &str, env: Env<'_>) -> Result<(Credentials, Vec<String>), Error> {
    let (found, warnings) = find(domain, env, None)?;
    Ok((found.answer()?, warnings))
}

/// What [`lookup`] finds for `domain`, a credential helper started rather than waited for:
/// its answer is read where it is needed (D184), so that a helper that never answers holds
/// nothing that needs no credentials, and it runs while the registry's first round trip
/// does. Started for `cancel`, it leads a process group of its own, which the cancel ends.
pub fn find(domain: &str, env: Env<'_>, cancel: Option<&Cancel>) -> Result<(Found, Vec<String>), Error> {
    let key = key_of(domain);
    let mut warnings = Vec::new();
    // Credentials in the environment: shards' own variable, else Docker's.
    let given = ["SHARDS_AUTH_CONFIG", "DOCKER_AUTH_CONFIG"]
        .into_iter()
        .find_map(|var| env(var).filter(|v| !v.is_empty()).map(|v| (var, v)));
    if let Some((var, value)) = given {
        match env_config(var, &value) {
            Ok(entries) => {
                if let Some(found) = entries.get(key) {
                    return Ok((Found::Known(found.clone()), warnings));
                }
            }
            Err(e) => warnings.push(format!("Failed to create credential store from {var}: {e}")),
        }
    }
    let Some(dir) = config_dir(env) else {
        return Ok((Found::Known(Credentials::Anonymous), warnings));
    };
    let path = dir.join("config.json");
    let config = load(&path)?;
    let entry = file_entry(&config, key)?;
    let holds_auth =
        !config.creds_store.is_empty() || !config.cred_helpers.is_empty() || !config.auths.is_empty();
    let helper = match config.cred_helpers.get(key) {
        Some(helper) => Some((helper.clone(), format!("credHelpers in {}", path.display()))),
        None if !config.creds_store.is_empty() => Some((
            config.creds_store.clone(),
            format!("credsStore in {}", path.display()),
        )),
        None if !holds_auth => default_helper(env).map(|helper| {
            (
                helper,
                format!("this platform's own, as {} keeps no credentials", path.display()),
            )
        }),
        None => None,
    };
    let found = match helper {
        None => Found::Known(credentials(entry)),
        // A registry token wins over whatever the helper would answer: it is not asked.
        Some(_) if !entry.registrytoken.is_empty() => {
            Found::Known(Credentials::RegistryToken(entry.registrytoken))
        }
        Some((helper, whence)) => Found::Asked(Asked::start(&helper, key, whence, env, cancel)),
    };
    Ok((found, warnings))
}

/// A credential helper asked for a registry's credentials (`docker-credential-NAME get`):
/// started by [`find`], its answer read where it is needed ([`Asked::answer`]), else
/// never. Dropped unread, it is ended and reaped; a cancel of the pull it serves ends it
/// at once. One that could not be started says why as its answer.
pub struct Asked {
    program: String,
    key: String,
    /// Where it was named, as its waiting notice says: `credsStore in PATH`.
    whence: String,
    talk: Result<Talk, Error>,
}

impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Asked")
            .field("program", &self.program)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Asked {
    fn start(helper: &str, key: &str, whence: String, env: Env<'_>, cancel: Option<&Cancel>) -> Asked {
        let program = format!("docker-credential-{helper}");
        let talk = Talk::start(&program, "get", key.as_bytes(), env, cancel);
        Asked {
            program,
            key: key.to_string(),
            whence,
            talk,
        }
    }

    /// Whether its answer is all there: the helper has ended, or never started.
    pub fn answered(&self) -> bool {
        self.talk.as_ref().map_or(true, Talk::ended)
    }

    /// What a pull that waits for its answer says of it: the helper, the key it was asked
    /// and where it was named.
    pub fn notice(&self) -> String {
        format!(
            "Waiting for {} to answer for {} ({})",
            self.program, self.key, self.whence
        )
    }

    /// Its answer, waited for: the credentials it has for its key, none where it has none
    /// (`docker login` asks for some then), or why it gave none. Its errors went to our
    /// stderr, as Docker's do.
    pub fn answer(self) -> Result<Credentials, Error> {
        let (failed, out) = self.talk?.finish()?;
        let text = String::from_utf8_lossy(&out);
        if let Some(status) = failed {
            if text.trim() == NOT_FOUND {
                return Ok(Credentials::Anonymous);
            }
            return Err(Error::new(format!(
                "error getting credentials - err: {status}, out: `{}`",
                text.trim()
            )));
        }
        let answer = serde_json::Deserializer::from_slice(&out)
            .into_iter::<HelperAnswer>()
            .next()
            .ok_or_else(|| Error::new(format!("{} answered nothing", self.program)))?
            .map_err(|e| Error::new(format!("{}: {e}", self.program)))?;
        let mut entry = Entry::default();
        if answer.username == TOKEN_USERNAME {
            entry.identitytoken = answer.secret;
        } else {
            entry.username = answer.username;
            entry.password = answer.secret;
        }
        Ok(credentials(entry))
    }
}

/// dockerd's order: a registry token, then an identity token, then a password.
fn credentials(entry: Entry) -> Credentials {
    if !entry.registrytoken.is_empty() {
        Credentials::RegistryToken(entry.registrytoken)
    } else if !entry.identitytoken.is_empty() {
        Credentials::IdentityToken(entry.identitytoken)
    } else {
        password_credentials(&entry.username, &entry.password)
    }
}

/// The key a login of registry `domain` is kept under: its host, or Docker Hub's index.
fn key_of(domain: &str) -> &str {
    if domain == "docker.io" || domain == "index.docker.io" {
        HUB_KEY
    } else {
        domain
    }
}

/// `SHARDS_AUTH_CONFIG`'s value holding `found`, each registry's credentials by its domain,
/// as a context's client sends those of its own a command there needs (D180), and as
/// [`lookup`] reads them back: a password as `auth`, a token as `config.json` keeps one.
pub fn env_config_of(found: &[(String, Credentials)]) -> String {
    let mut auths = serde_json::Map::new();
    for (domain, credentials) in found {
        let entry = match credentials {
            Credentials::Anonymous => continue,
            Credentials::Password { username, password } => {
                serde_json::json!({ "auth": BASE64.encode(format!("{username}:{password}")) })
            }
            Credentials::IdentityToken(token) => serde_json::json!({ "identitytoken": token }),
            Credentials::RegistryToken(token) => serde_json::json!({ "registrytoken": token }),
        };
        auths.insert(key_of(domain).to_string(), entry);
    }
    serde_json::json!({ "auths": auths }).to_string()
}

fn password_credentials(username: &str, password: &str) -> Credentials {
    if username.is_empty() && password.is_empty() {
        Credentials::Anonymous
    } else {
        Credentials::Password {
            username: username.to_string(),
            password: password.to_string(),
        }
    }
}

/// `$DOCKER_CONFIG`, else `.docker` in the home directory (`os.UserHomeDir`).
fn config_dir(env: Env<'_>) -> Option<PathBuf> {
    if let Some(dir) = env("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = if cfg!(windows) {
        env("USERPROFILE")
    } else {
        env("HOME")
    };
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".docker"))
}

/// The config file; a missing one is an empty config (`config.go`, `load`).
fn load(path: &Path) -> Result<ConfigFile, Error> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConfigFile::default()),
        Err(e) => return Err(Error::from(e).context(format!("loading {}", path.display()))),
    };
    // Go's decoder reads the first value; an empty file is an empty config.
    match serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<ConfigFile>()
        .next()
    {
        None => Ok(ConfigFile::default()),
        Some(parsed) => {
            parsed.map_err(|e| Error::new(format!("parsing config file ({}): {e}", path.display())))
        }
    }
}

/// The file store's entry for `key`: exactly, else the first whose key, taken as a URL,
/// names the same host (`fileStore.Get`, `ConvertToHostname`). Go ranges over a map, so
/// its "first" is random; ours is the first in sorted order.
fn file_entry(config: &ConfigFile, key: &str) -> Result<Entry, Error> {
    let entry = config
        .auths
        .get(key)
        .or_else(|| {
            config
                .auths
                .iter()
                .find(|(k, _)| to_hostname(k) == key)
                .map(|(_, e)| e)
        })
        .cloned()
        .unwrap_or_default();
    if entry.auth.is_empty() {
        return Ok(entry);
    }
    let (username, password) = decode_auth(&entry.auth)?;
    Ok(Entry {
        auth: String::new(),
        username,
        password,
        ..entry
    })
}

/// `ConvertToHostname`: the host and port of a URL, or what comes before a `/`.
fn to_hostname(s: &str) -> String {
    let rest = s.split_once("://").map_or(s, |(_, rest)| rest);
    rest.split('/').next().unwrap_or_default().to_string()
}

/// `decodeAuth`: base64 of `user:password`, trailing NULs trimmed from the password.
fn decode_auth(auth: &str) -> Result<(String, String), Error> {
    let decoded = BASE64
        .decode(auth)
        .map_err(|e| Error::new(format!("invalid auth configuration file: {e}")))?;
    let decoded = String::from_utf8_lossy(&decoded);
    match decoded.split_once(':') {
        Some((user, password)) if !user.is_empty() => {
            Ok((user.to_string(), password.trim_matches('\0').to_string()))
        }
        _ => Err(Error::new("invalid auth configuration file")),
    }
}

/// `parseEnvConfig` of variable `var`'s `value`.
fn env_config(var: &str, value: &str) -> Result<BTreeMap<String, Credentials>, Error> {
    let mut values = serde_json::Deserializer::from_str(value).into_iter::<EnvConfig>();
    let parsed = match values.next() {
        None => EnvConfig {
            auths: BTreeMap::new(),
        },
        Some(parsed) => parsed.map_err(|e| Error::new(e.to_string()))?,
    };
    if values.next().is_some() {
        return Err(Error::new(format!(
            "{var} does not support more than one JSON object"
        )));
    }
    let mut out = BTreeMap::new();
    for (addr, entry) in parsed.auths {
        let tokens = !entry.registrytoken.is_empty() || !entry.identitytoken.is_empty();
        // Docker's variable takes `auth` alone (cli/config/credentials, its env store).
        if tokens && var != "SHARDS_AUTH_CONFIG" {
            return Err(Error::new(format!(
                "{var} takes `auth` alone, not a token, for {addr}"
            )));
        }
        // dockerd's precedence: a registry token, then an identity token, then a password.
        let credentials = if !entry.registrytoken.is_empty() {
            Credentials::RegistryToken(entry.registrytoken)
        } else if !entry.identitytoken.is_empty() {
            Credentials::IdentityToken(entry.identitytoken)
        } else if entry.auth.is_empty() {
            return Err(Error::new(format!(
                "{var} environment variable is missing key `auth` for {addr}"
            )));
        } else {
            let (username, password) = decode_auth(&entry.auth)?;
            password_credentials(&username, &password)
        };
        out.insert(addr, credentials);
    }
    Ok(out)
}

/// `DetectDefaultStore`: the platform's helper, if it is on `PATH`.
fn default_helper(env: Env<'_>) -> Option<String> {
    let name = if cfg!(target_os = "macos") {
        "osxkeychain"
    } else if cfg!(windows) {
        "wincred"
    } else if on_path("pass", env) {
        "pass"
    } else {
        "secretservice"
    };
    on_path(&format!("docker-credential-{name}"), env).then(|| name.to_string())
}

/// Whether `program` is a file in one of `PATH`'s directories (with `.exe` on Windows).
fn on_path(program: &str, env: Env<'_>) -> bool {
    let Some(path) = env("PATH") else {
        return false;
    };
    let file = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_string()
    };
    std::env::split_paths(&path).any(|dir| dir.join(&file).is_file())
}

/// Runs `program ACTION` with `input` on its input and our stderr for its own, waited for:
/// how it failed, if it did, and what it wrote ([`Talk`]).
fn run_helper(
    program: &str,
    action: &str,
    input: &[u8],
    env: Env<'_>,
) -> Result<(Option<String>, Vec<u8>), Error> {
    Talk::start(program, action, input, env, None)?.finish()
}

/// A helper's run: `program ACTION`, found on `env`'s PATH (else ours) and given it, its
/// input written and closed at once (a registry's key, far under a pipe's buffer, never
/// waits), its output read and its status taken at [`Talk::finish`]. A helper may exit
/// without reading its input: its status and output then say what it had to say, as Go's
/// os/exec, which Docker's helper client uses, ignores the broken pipe (exec.go,
/// skipStdinCopyError). Dropped unfinished, it is ended and reaped.
///
/// Made by shards_ipc, as the daemon's every child is: by its spawner, so that the helper
/// holds nothing the daemon lets go of while it starts, a run's published port among them
/// (PM M158). Started for a cancel, it leads a process group of its own, which the cancel
/// ends whole, a script's children with it, which hold its output open (D184); otherwise
/// it stays in its caller's, where a terminal's Ctrl-C reaches it and its prompts reach the
/// terminal. How it failed is said as Go's os.ProcessState says it, as Docker does: an exit
/// status (128 and its number, for a signal that ended it).
#[cfg(unix)]
struct Talk {
    program: String,
    child: Option<std::sync::Arc<shards_ipc::Child>>,
    stdout: std::io::PipeReader,
    /// The cancel that ends it, and its number there.
    cancel: Option<(Cancel, u64)>,
    group: bool,
}

#[cfg(unix)]
impl Talk {
    fn start(
        program: &str,
        action: &str,
        input: &[u8],
        env: Env<'_>,
        cancel: Option<&Cancel>,
    ) -> Result<Talk, Error> {
        use std::os::fd::AsFd as _;
        let fail = |e: std::io::Error| Error::new(format!("running {program}: {e}"));
        let path = env("PATH");
        let search = path
            .clone()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let found = std::env::split_paths(&search)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
            .ok_or_else(|| fail(std::io::ErrorKind::NotFound.into()))?;
        let (stdin, mut to_stdin) = std::io::pipe().map_err(fail)?;
        let (stdout, from_stdout) = std::io::pipe().map_err(fail)?;
        let set: Vec<(&str, &std::ffi::OsStr)> = path.iter().map(|p| ("PATH", p.as_ref())).collect();
        let group = cancel.is_some();
        let child = shards_ipc::spawn_with(
            &found,
            &[action.as_ref()],
            &[
                (stdin.as_fd(), 0),
                (from_stdout.as_fd(), 1),
                (std::io::stderr().as_fd(), 2),
            ],
            group,
            &set,
        )
        .map_err(fail)?;
        drop((stdin, from_stdout));
        let child = std::sync::Arc::new(child);
        let mut talk = Talk {
            program: program.to_string(),
            child: Some(child.clone()),
            stdout,
            cancel: cancel.map(|c| (c.clone(), c.watch_helper(child))),
            group,
        };
        match to_stdin.write_all(input) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                talk.end();
                return Err(Error::new(format!("{program}: {e}")));
            }
        }
        Ok(talk)
    }

    /// Whether the helper has ended, its output then all written.
    fn ended(&self) -> bool {
        self.child.as_ref().is_some_and(|c| c.has_ended())
    }

    /// How it failed, if it did, and what it wrote, `MAX_HELPER_OUTPUT` at most: waited
    /// for, unless its cancel ends it first.
    fn finish(mut self) -> Result<(Option<String>, Vec<u8>), Error> {
        let mut out = Vec::new();
        let read = (&mut self.stdout).take(MAX_HELPER_OUTPUT).read_to_end(&mut out);
        let Some(child) = self.child.take() else {
            return Err(Error::new(format!("{}: already finished", self.program)));
        };
        if read.is_err() {
            let _ = self.kill(&child);
        }
        let status = child.wait();
        if let Some((cancel, n)) = self.cancel.take() {
            cancel.forget_helper(n);
            if cancel.is_cancelled() {
                return Err(Error::of(crate::ErrorKind::Cancelled, "cancelled"));
            }
        }
        let status = status.map_err(|e| Error::new(format!("{}: {e}", self.program)))?;
        read.map_err(|e| Error::new(format!("{}: {e}", self.program)))?;
        Ok(((status != 0).then(|| format!("exit status {status}")), out))
    }

    fn kill(&self, child: &shards_ipc::Child) -> std::io::Result<()> {
        if self.group {
            child.kill_group(libc::SIGKILL)
        } else {
            child.kill(libc::SIGKILL)
        }
    }

    /// Ends and reaps the helper, unless it has finished.
    fn end(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = self.kill(&child);
            let _ = child.wait();
        }
        if let Some((cancel, n)) = self.cancel.take() {
            cancel.forget_helper(n);
        }
    }
}

/// [`Talk`] where shards has no daemon: by std's Command. A cancel ends it by a handle of
/// its own.
#[cfg(windows)]
struct Talk {
    program: String,
    child: Option<std::process::Child>,
    cancel: Option<(Cancel, u64)>,
}

#[cfg(windows)]
impl Talk {
    fn start(
        program: &str,
        action: &str,
        input: &[u8],
        env: Env<'_>,
        cancel: Option<&Cancel>,
    ) -> Result<Talk, Error> {
        use std::process::{Command, Stdio};
        let mut command = Command::new(program);
        command
            .arg(action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(path) = env("PATH") {
            command.env("PATH", path);
        }
        let mut child = command
            .spawn()
            .map_err(|e| Error::new(format!("running {program}: {e}")))?;
        let watched = match cancel {
            Some(cancel) => {
                use std::os::windows::io::AsHandle as _;
                match child.as_handle().try_clone_to_owned() {
                    Ok(handle) => Some((cancel.clone(), cancel.watch_helper(handle))),
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::new(format!("{program}: {e}")));
                    }
                }
            }
            None => None,
        };
        let stdin = child.stdin.take();
        let mut talk = Talk {
            program: program.to_string(),
            child: Some(child),
            cancel: watched,
        };
        if let Some(mut stdin) = stdin {
            match stdin.write_all(input) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(e) => {
                    talk.end();
                    return Err(Error::new(format!("{program}: {e}")));
                }
            }
        }
        Ok(talk)
    }

    fn ended(&self) -> bool {
        use std::os::windows::io::AsRawHandle as _;
        self.child.as_ref().is_some_and(|c| {
            // SAFETY: a wait of no time on the process handle the Child holds open.
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(c.as_raw_handle(), 0)
                    == windows_sys::Win32::Foundation::WAIT_OBJECT_0
            }
        })
    }

    fn finish(mut self) -> Result<(Option<String>, Vec<u8>), Error> {
        let Some(mut child) = self.child.take() else {
            return Err(Error::new(format!("{}: already finished", self.program)));
        };
        let mut out = Vec::new();
        let read = match child.stdout.take() {
            Some(stdout) => stdout.take(MAX_HELPER_OUTPUT).read_to_end(&mut out).map(drop),
            None => Ok(()),
        };
        if read.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        if let Some((cancel, n)) = self.cancel.take() {
            cancel.forget_helper(n);
            if cancel.is_cancelled() {
                return Err(Error::of(crate::ErrorKind::Cancelled, "cancelled"));
            }
        }
        let status = status.map_err(|e| Error::new(format!("{}: {e}", self.program)))?;
        read.map_err(|e| Error::new(format!("{}: {e}", self.program)))?;
        Ok(((!status.success()).then(|| status.to_string()), out))
    }

    fn end(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some((cancel, n)) = self.cancel.take() {
            cancel.forget_helper(n);
        }
    }
}

impl Drop for Talk {
    fn drop(&mut self) {
        self.end();
    }
}

/// A registry's credentials to keep, as `docker login` keeps them: its key (as given, or
/// Docker Hub's `https://index.docker.io/v1/`), its user, and the password, or the
/// identity token its token server gave in place of one.
pub struct Login {
    pub server: String,
    pub username: String,
    pub password: String,
    pub identity_token: String,
}

/// Names the login, never its secrets.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("identity_token", &!self.identity_token.is_empty())
            .finish_non_exhaustive()
    }
}

/// Where a login was kept.
#[derive(Debug, PartialEq, Eq)]
pub enum Kept {
    /// By `docker-credential-<helper>`.
    Helper(String),
    /// In the config file, unencrypted.
    File(PathBuf),
}

/// The helper that keeps `key`'s credentials (configfile.GetCredentialsStore): the
/// registry's own, else the config's `credsStore`, else the platform's where the config
/// holds no credentials at all, as [`lookup`] finds them.
fn helper_for(config: &ConfigFile, key: &str, env: Env<'_>) -> Option<String> {
    let holds_auth =
        !config.creds_store.is_empty() || !config.cred_helpers.is_empty() || !config.auths.is_empty();
    match config.cred_helpers.get(key) {
        Some(helper) => Some(helper.clone()),
        None if !config.creds_store.is_empty() => Some(config.creds_store.clone()),
        None if !holds_auth => default_helper(env),
        None => None,
    }
}

/// Keeps `login` as `docker login` does (cli/command/registry/login.go,
/// storeCredentials; credentials/native_store.go and file_store.go): by its credential
/// helper's `store` (an identity token as the user `<token>`), its entry in the config's
/// `auths` then holding nothing secret; without one, in that entry, `auth` the base64 of
/// `username:password`, with `identitytoken` beside it. The config's other keys stay as
/// they were, where docker/cli writes back only those it knows. Where it was kept.
pub fn store(login: &Login, env: Env<'_>) -> Result<Kept, Error> {
    let dir = config_dir(env)
        .ok_or_else(|| Error::new("no config directory: neither DOCKER_CONFIG nor a home is set"))?;
    let path = dir.join("config.json");
    let config = load(&path)?;
    let mut entry = serde_json::Map::new();
    let kept = match helper_for(&config, &login.server, env) {
        Some(helper) => {
            let (username, secret) = if login.identity_token.is_empty() {
                (login.username.as_str(), login.password.as_str())
            } else {
                (TOKEN_USERNAME, login.identity_token.as_str())
            };
            let input =
                serde_json::json!({ "ServerURL": login.server, "Username": username, "Secret": secret })
                    .to_string();
            let program = format!("docker-credential-{helper}");
            let (failed, out) = run_helper(&program, "store", input.as_bytes(), env)?;
            if let Some(status) = failed {
                return Err(Error::new(format!(
                    "error storing credentials - err: {status}, out: `{}`",
                    String::from_utf8_lossy(&out).trim()
                )));
            }
            Kept::Helper(helper)
        }
        None => {
            // An identity token in place of the password, never beside it (login.go).
            let password = if login.identity_token.is_empty() {
                login.password.as_str()
            } else {
                ""
            };
            entry.insert(
                "auth".into(),
                serde_json::Value::String(BASE64.encode(format!("{}:{password}", login.username))),
            );
            if !login.identity_token.is_empty() {
                entry.insert(
                    "identitytoken".into(),
                    serde_json::Value::String(login.identity_token.clone()),
                );
            }
            Kept::File(path.clone())
        }
    };
    rewrite(&path, |auths| {
        auths.insert(login.server.clone(), serde_json::Value::Object(entry));
    })?;
    Ok(kept)
}

/// Forgets the credentials kept for `server` as `docker logout` does (logout.go): the key
/// as given, and for any but Docker Hub's its host, `http://` and `https://` it too, each
/// from its helper and the config's `auths`. Whether any was there to forget, and why
/// each that was could not be.
pub fn erase(server: &str, env: Env<'_>) -> Result<(bool, Vec<String>), Error> {
    let dir = config_dir(env)
        .ok_or_else(|| Error::new("no config directory: neither DOCKER_CONFIG nor a home is set"))?;
    let path = dir.join("config.json");
    let config = load(&path)?;
    let mut keys = vec![server.to_string()];
    if server != HUB_KEY {
        let host = to_hostname(server);
        keys.extend([host.clone(), format!("http://{host}"), format!("https://{host}")]);
        keys.dedup();
    }
    let mut failed = Vec::new();
    let mut forgot = false;
    for key in &keys {
        if let Some(helper) = helper_for(&config, key, env) {
            let program = format!("docker-credential-{helper}");
            match run_helper(&program, "erase", key.as_bytes(), env) {
                Ok((None, _)) => forgot = true,
                Ok((Some(_), out)) if String::from_utf8_lossy(&out).trim() == NOT_FOUND => {}
                Ok((Some(status), out)) => failed.push(format!(
                    "{key}: error erasing credentials - err: {status}, out: `{}`",
                    String::from_utf8_lossy(&out).trim()
                )),
                Err(e) => failed.push(format!("{key}: {e}")),
            }
        }
        forgot |= config.auths.contains_key(key);
    }
    rewrite(&path, |auths| {
        for key in &keys {
            auths.remove(key);
        }
    })?;
    Ok((forgot, failed))
}

/// Rewrites the config at `path` with `change` made to its `auths`, every other key as it
/// was: written whole beside it and renamed over it, so that a reader never finds half of
/// one; a new one, and its directory, the user's alone. A config that is a link (a
/// dotfiles manager's) is written where it leads, the link kept, as docker/cli's Save
/// writes it (EvalSymlinks); staged beside its target, so that the rename stays on one
/// file system, where docker/cli stages it beside the link.
fn rewrite(
    path: &Path,
    change: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<(), Error> {
    let fail = |e: std::io::Error| Error::from(e).context(format!("saving {}", path.display()));
    let mut doc = match std::fs::read(path) {
        Ok(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => {
            serde_json::Deserializer::from_slice(&bytes)
                .into_iter::<serde_json::Value>()
                .next()
                .transpose()
                .map_err(|e| Error::new(format!("parsing config file ({}): {e}", path.display())))?
                .unwrap_or_else(|| serde_json::json!({}))
        }
        Ok(_) => serde_json::json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(fail(e)),
    };
    let Some(object) = doc.as_object_mut() else {
        return Err(Error::new(format!("{}: not a JSON object", path.display())));
    };
    let auths = object.entry("auths").or_insert_with(|| serde_json::json!({}));
    let Some(auths) = auths.as_object_mut() else {
        return Err(Error::new(format!(
            "{}: its auths are not an object",
            path.display()
        )));
    };
    change(auths);
    // As docker/cli writes it: tab-indented.
    let mut text = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut writer = serde_json::Serializer::with_formatter(&mut text, formatter);
    serde::Serialize::serialize(&doc, &mut writer).map_err(|e| Error::new(e.to_string()))?;
    let target = written_at(path).map_err(fail)?;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir).map_err(fail)?;
    let staged = dir.join(format!(".config.json.{}", std::process::id()));
    let written = (|| {
        // One a process of this ID left: ours to replace, a new file in its place.
        let mut file = match private_file(&staged) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&staged)?;
                private_file(&staged)?
            }
            opened => opened?,
        };
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(&text)?;
        file.sync_all()?;
        std::fs::rename(&staged, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written.map_err(fail)
}

/// Where the config at `path` is written: where its links lead, or a dangling one's last
/// link points; `path` where it is no link.
fn written_at(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(target) => return Ok(target),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        Err(_) => {}
    }
    let mut at = path.to_path_buf();
    let mut seen = Vec::new();
    while let Ok(to) = std::fs::read_link(&at) {
        if seen.contains(&at) {
            return Err(std::io::Error::other(format!(
                "{}: its links make a loop",
                path.display()
            )));
        }
        let next = at.parent().map_or_else(|| to.clone(), |dir| dir.join(&to));
        seen.push(std::mem::replace(&mut at, next));
    }
    Ok(at)
}

/// `dir` and its parents, made the user's alone where they are made.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A new file at `path`, the user's alone to read and write: never one already there, nor
/// where a link there leads (`O_EXCL`).
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp(name: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(&format!("creds-{name}")).unwrap()
    }

    fn auth(user_password: &str) -> String {
        BASE64.encode(user_password)
    }

    /// Looks `domain` up with `config` as the config file and `vars` as the environment.
    fn find(config: &str, vars: &[(&str, &str)], domain: &str) -> Result<(Credentials, Vec<String>), Error> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = temp(
            &NEXT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_string(),
        );
        std::fs::write(dir.join("config.json"), config).unwrap();
        let mut env: HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        env.insert("DOCKER_CONFIG".into(), dir.to_string_lossy().into_owned());
        env.entry("PATH".into()).or_default();
        let found = lookup(domain, &|k| env.get(k).cloned());
        let _ = std::fs::remove_dir_all(&dir);
        found
    }

    fn password(u: &str, p: &str) -> Credentials {
        Credentials::Password {
            username: u.into(),
            password: p.into(),
        }
    }

    #[test]
    fn logins_are_found_under_legacy_registry_keys() {
        let config = format!(
            r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{}"}},"https://legacy.example:5000/v1/":{{"auth":"{}"}}}}}}"#,
            auth("hub:hubpass"),
            auth("old:oldpass\0\0")
        );
        assert_eq!(
            find(&config, &[], "docker.io").unwrap().0,
            password("hub", "hubpass")
        );
        assert_eq!(
            find(&config, &[], "index.docker.io").unwrap().0,
            password("hub", "hubpass")
        );
        assert_eq!(
            find(&config, &[], "legacy.example:5000").unwrap().0,
            password("old", "oldpass")
        );
        assert_eq!(find(&config, &[], "ghcr.io").unwrap().0, Credentials::Anonymous);
        assert_eq!(
            find("", &[], "docker.io").unwrap().0,
            Credentials::Anonymous,
            "an empty file"
        );
    }

    #[test]
    fn tokens_come_before_passwords() {
        let config = format!(
            r#"{{"auths":{{"a.example":{{"auth":"{}","identitytoken":"idt"}},"b.example":{{"identitytoken":"idt","registrytoken":"rt"}},"c.example":{{"username":"u","password":"p"}}}}}}"#,
            auth("u:p")
        );
        assert_eq!(
            find(&config, &[], "a.example").unwrap().0,
            Credentials::IdentityToken("idt".into())
        );
        assert_eq!(
            find(&config, &[], "b.example").unwrap().0,
            Credentials::RegistryToken("rt".into())
        );
        assert_eq!(find(&config, &[], "c.example").unwrap().0, password("u", "p"));
    }

    #[test]
    fn malformed_logins_are_refused() {
        for bad in [auth("nocolon"), auth(":emptyuser"), "!!!".to_string()] {
            let config = format!(r#"{{"auths":{{"a.example":{{"auth":"{bad}"}}}}}}"#);
            assert!(find(&config, &[], "a.example").is_err(), "{bad}");
        }
        assert!(find("{not json", &[], "a.example").is_err());
    }

    #[test]
    fn an_auth_config_in_the_environment_comes_first_and_falls_back_with_a_warning() {
        let config = format!(
            r#"{{"auths":{{"a.example":{{"auth":"{}"}}}}}}"#,
            auth("file:filepass")
        );
        let env = format!(
            r#"{{"auths":{{"a.example":{{"auth":"{}"}}}}}}"#,
            auth("env:envpass")
        );
        let (found, warnings) = find(&config, &[("DOCKER_AUTH_CONFIG", &env)], "a.example").unwrap();
        assert_eq!((found, warnings.len()), (password("env", "envpass"), 0));
        let unknown = format!(
            r#"{{"auths":{{"a.example":{{"auth":"{}","username":"x"}}}}}}"#,
            auth("env:envpass")
        );
        let (found, warnings) = find(&config, &[("DOCKER_AUTH_CONFIG", &unknown)], "a.example").unwrap();
        assert_eq!(found, password("file", "filepass"));
        assert!(warnings[0].starts_with("Failed to create credential store from DOCKER_AUTH_CONFIG"));
        let (_, warnings) = find(
            &config,
            &[("DOCKER_AUTH_CONFIG", r#"{"auths":{"a.example":{}}}"#)],
            "a.example",
        )
        .unwrap();
        assert!(warnings[0].contains("missing key `auth`"), "{warnings:?}");
        // shards' own variable before Docker's, and named in what it warns of.
        let ours = format!(
            r#"{{"auths":{{"a.example":{{"auth":"{}"}}}}}}"#,
            auth("ours:ourpass")
        );
        let (found, warnings) = find(
            &config,
            &[("SHARDS_AUTH_CONFIG", &ours), ("DOCKER_AUTH_CONFIG", &env)],
            "a.example",
        )
        .unwrap();
        assert_eq!((found, warnings.len()), (password("ours", "ourpass"), 0));
        let (found, warnings) = find(
            &config,
            &[("SHARDS_AUTH_CONFIG", &unknown), ("DOCKER_AUTH_CONFIG", &env)],
            "a.example",
        )
        .unwrap();
        assert_eq!(found, password("file", "filepass"));
        assert!(warnings[0].starts_with("Failed to create credential store from SHARDS_AUTH_CONFIG"));
    }

    /// A helper that answers without reading its input, as Docker's client allows.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_does_not_read_its_input_is_heard() {
        let bin = temp("deaf-helper");
        program(
            &bin.join("docker-credential-deaf"),
            "#!/bin/sh\nexec 0<&-\necho '{\"ServerURL\":\"x\",\"Username\":\"u\",\"Secret\":\"s\"}'\n",
        );
        let dirs = format!("{}:/bin:/usr/bin", bin.display());
        let env = [("PATH", dirs.as_str())];
        let long = "k".repeat(1 << 20);
        let env = |name: &str| {
            env.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())
        };
        let answer = Asked::start("deaf", &long, String::new(), &env, None).answer();
        assert_eq!(answer.ok(), Some(password("u", "s")));
        let _ = std::fs::remove_dir_all(&bin);
    }

    /// A credential helper that knows `found.example` and `token.example`.
    #[cfg(unix)]
    fn helper(dir: &Path, name: &str) {
        let script = r#"#!/bin/sh
read -r key || true
case "$key" in
found.example) echo '{"ServerURL":"found.example","Username":"u","Secret":"s"}' ;;
token.example) echo '{"ServerURL":"token.example","Username":"<token>","Secret":"idt"}' ;;
broken.example) echo 'the keychain is locked'; exit 1 ;;
*) echo 'credentials not found in native keychain'; exit 1 ;;
esac
"#;
        program(&dir.join(format!("docker-credential-{name}")), script);
    }

    /// The program `text` at `path`, written by a process of its own: a file this one held
    /// open to write, a child another test's thread forked meanwhile holds until it execs,
    /// and an exec of the file fails with ETXTBSY until then (Linux; CI's run 38905368380,
    /// "Text file busy").
    #[cfg(unix)]
    fn program(path: &Path, text: &str) {
        use std::io::Write as _;
        let mut writer = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"cat > "$0" && chmod 755 "$0""#)
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        assert!(writer.wait().unwrap().success(), "{} not written", path.display());
    }

    /// What a context's client sends of its own reads back as it was: a password, an
    /// identity token and a registry's token, Docker Hub's under its index's key; none for a
    /// registry it has none for; and Docker's own variable takes no token.
    #[test]
    fn a_clients_credentials_cross_as_they_were() {
        let found = vec![
            ("docker.io".to_string(), password("u", "p")),
            ("a.example".to_string(), Credentials::IdentityToken("idt".into())),
            ("b.example".to_string(), Credentials::RegistryToken("rt".into())),
            ("c.example".to_string(), Credentials::Anonymous),
        ];
        let sent = env_config_of(&found);
        assert!(sent.contains("https://index.docker.io/v1/"), "{sent}");
        assert!(!sent.contains("c.example"), "{sent}");
        let env = |name: &str| (name == "SHARDS_AUTH_CONFIG").then(|| sent.clone());
        for (domain, credentials) in &found {
            assert_eq!(&lookup(domain, &env).unwrap().0, credentials, "{domain}");
        }
        let docker = |name: &str| (name == "DOCKER_AUTH_CONFIG").then(|| sent.clone());
        let (got, warnings) = lookup("a.example", &docker).unwrap();
        assert_eq!(got, Credentials::Anonymous);
        assert!(
            warnings[0].starts_with("Failed to create credential store from DOCKER_AUTH_CONFIG"),
            "{warnings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn credential_helpers_answer_for_their_hosts() {
        let bin = temp("helpers");
        helper(&bin, "fake");
        let path = format!("{}:/bin:/usr/bin", bin.display());
        let config = format!(
            r#"{{"credHelpers":{{"found.example":"fake","token.example":"fake","missing.example":"fake","broken.example":"fake"}},"auths":{{"missing.example":{{"auth":"{}"}}}}}}"#,
            auth("file:ignored")
        );
        let env = [("PATH", path.as_str())];
        assert_eq!(
            find(&config, &env, "found.example").unwrap().0,
            password("u", "s")
        );
        assert_eq!(
            find(&config, &env, "token.example").unwrap().0,
            Credentials::IdentityToken("idt".into())
        );
        // A helper that has nothing wins over the file: Docker then asks for a login.
        assert_eq!(
            find(&config, &env, "missing.example").unwrap().0,
            Credentials::Anonymous
        );
        let e = find(&config, &env, "broken.example").unwrap_err();
        assert!(e.to_string().contains("the keychain is locked"), "{e}");
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
    #[test]
    fn the_platform_s_helper_serves_configs_without_credentials() {
        let bin = temp("default-helper");
        let name = if cfg!(target_os = "macos") {
            "osxkeychain"
        } else {
            "pass"
        };
        helper(&bin, name);
        if !cfg!(target_os = "macos") {
            // `pass` itself must be installed for Docker to choose its helper.
            program(&bin.join("pass"), "#!/bin/sh\nexit 0\n");
        }
        let path = format!("{}:/bin:/usr/bin", bin.display());
        let env = [("PATH", path.as_str())];
        assert_eq!(find("{}", &env, "found.example").unwrap().0, password("u", "s"));
        // A config with any credentials of its own keeps them.
        let config = format!(r#"{{"auths":{{"other.example":{{"auth":"{}"}}}}}}"#, auth("o:p"));
        assert_eq!(
            find(&config, &env, "found.example").unwrap().0,
            Credentials::Anonymous
        );
        let _ = std::fs::remove_dir_all(&bin);
    }

    /// A login kept as `docker login` keeps one: in the config's `auths` without a helper,
    /// every other key of the file kept, a new file the user's alone, found again as a
    /// pull finds it; an identity token in place of the password; and forgotten again,
    /// under the key's other forms too.
    #[test]
    fn logins_are_kept_in_the_config_and_forgotten() {
        let dir = temp("store-file");
        std::fs::write(
            dir.join("config.json"),
            format!(
                r#"{{"psFormat":"table {{{{.ID}}}}","auths":{{"other.example":{{"auth":"{}"}}}}}}"#,
                auth("o:op")
            ),
        )
        .unwrap();
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DOCKER_CONFIG".into(), dir.to_string_lossy().into_owned());
        vars.insert("PATH".into(), String::new());
        let env = |k: &str| vars.get(k).cloned();
        let login = |server: &str, token: &str| Login {
            server: server.into(),
            username: "u".into(),
            password: if token.is_empty() {
                "p".into()
            } else {
                String::new()
            },
            identity_token: token.into(),
        };
        let kept = store(&login("r.example", ""), &env).unwrap();
        assert_eq!(kept, Kept::File(dir.join("config.json")));
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(doc["psFormat"], serde_json::json!("table {{.ID}}"), "{doc}");
        assert_eq!(
            doc["auths"]["r.example"],
            serde_json::json!({ "auth": auth("u:p") }),
            "{doc}"
        );
        assert!(doc["auths"]["other.example"].is_object(), "{doc}");
        assert_eq!(lookup("r.example", &env).unwrap().0, password("u", "p"));
        // An identity token in place of the password.
        store(&login("t.example", "idt"), &env).unwrap();
        assert_eq!(
            lookup("t.example", &env).unwrap().0,
            Credentials::IdentityToken("idt".into())
        );
        // Forgotten, under the forms docker logout tries.
        std::fs::write(
            dir.join("config.json"),
            format!(
                r#"{{"auths":{{"https://r.example":{{"auth":"{}"}}}}}}"#,
                auth("u:p")
            ),
        )
        .unwrap();
        let (forgot, failed) = erase("r.example", &env).unwrap();
        assert!(forgot && failed.is_empty(), "{failed:?}");
        assert_eq!(lookup("r.example", &env).unwrap().0, Credentials::Anonymous);
        let (forgot, _) = erase("r.example", &env).unwrap();
        assert!(!forgot);
        // A new config, the user's alone.
        let fresh = temp("store-new");
        let fresh_dir = fresh.join("docker");
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DOCKER_CONFIG".into(), fresh_dir.to_string_lossy().into_owned());
        vars.insert("PATH".into(), String::new());
        store(&login("r.example", ""), &|k: &str| vars.get(k).cloned()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(fresh_dir.join("config.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
    }

    /// A config that is a link stays one, what it leads to rewritten; a dangling one is
    /// made where it points; a loop of links is refused, nothing written.
    #[cfg(unix)]
    #[test]
    fn a_linked_config_stays_a_link() {
        let dir = temp("store-link");
        let (home, dotfiles) = (dir.join("home"), dir.join("dotfiles"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("docker.json"), r#"{"psFormat":"x"}"#).unwrap();
        std::os::unix::fs::symlink("../dotfiles/docker.json", home.join("config.json")).unwrap();
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DOCKER_CONFIG".into(), home.to_string_lossy().into_owned());
        vars.insert("PATH".into(), String::new());
        let env = |k: &str| vars.get(k).cloned();
        let login = Login {
            server: "r.example".into(),
            username: "u".into(),
            password: "p".into(),
            identity_token: String::new(),
        };
        store(&login, &env).unwrap();
        let link = std::fs::symlink_metadata(home.join("config.json")).unwrap();
        assert!(link.file_type().is_symlink());
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dotfiles.join("docker.json")).unwrap()).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({"psFormat": "x", "auths": {"r.example": {"auth": auth("u:p")}}})
        );
        let names = |d: &Path| -> Vec<String> {
            let mut n: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };
        assert_eq!(names(&home), ["config.json"]);
        assert_eq!(names(&dotfiles), ["docker.json"]);
        // Dangling: made where it points.
        std::fs::remove_file(dotfiles.join("docker.json")).unwrap();
        store(&login, &env).unwrap();
        assert!(
            std::fs::symlink_metadata(home.join("config.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(lookup("r.example", &env).unwrap().0, password("u", "p"));
        // A loop: refused.
        std::fs::remove_file(home.join("config.json")).unwrap();
        std::os::unix::fs::symlink("config.json", home.join("config.json")).unwrap();
        assert!(store(&login, &env).is_err());
        assert_eq!(names(&home), ["config.json"]);
    }

    /// A login kept by the config's credential helper, as docker/cli's native store keeps
    /// one: its `store` given the server, user and secret (an identity token as `<token>`),
    /// the config's entry for it holding nothing secret; and `erase` given the key.
    #[cfg(unix)]
    #[test]
    fn logins_go_to_the_credential_helper() {
        let dir = temp("store-helper");
        let log = dir.join("helper.log");
        program(
            &dir.join("docker-credential-shardstest"),
            &format!(
                "#!/bin/sh\nIFS= read -r line || true\nprintf '%s %s\\n' \"$1\" \"$line\" >> '{}'\n",
                log.display()
            ),
        );
        std::fs::write(dir.join("config.json"), r#"{"credsStore":"shardstest"}"#).unwrap();
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DOCKER_CONFIG".into(), dir.to_string_lossy().into_owned());
        vars.insert("PATH".into(), dir.to_string_lossy().into_owned());
        let env = |k: &str| vars.get(k).cloned();
        let kept = store(
            &Login {
                server: "r.example".into(),
                username: "u".into(),
                password: String::new(),
                identity_token: "idt".into(),
            },
            &env,
        )
        .unwrap();
        assert_eq!(kept, Kept::Helper("shardstest".into()));
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(doc["auths"]["r.example"], serde_json::json!({}), "{doc}");
        erase("r.example", &env).unwrap();
        let said = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some(r#"store {"Secret":"idt","ServerURL":"r.example","Username":"<token>"}"#),
            "{said}"
        );
        assert!(
            lines.contains(&"erase r.example") && lines.contains(&"erase https://r.example"),
            "{said}"
        );
    }
    /// Whether process `pid` has gone: reaped, as kill(2) of no signal finds none.
    #[cfg(unix)]
    fn gone(pid: i32) -> bool {
        // SAFETY: kill(2) with no signal sends nothing; it asks whether the process is.
        unsafe {
            libc::kill(pid, 0) != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    /// The `n`th pid `file` records, once it is there.
    #[cfg(unix)]
    fn recorded(file: &Path, n: usize) -> i32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let pids: Vec<i32> = std::fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
            if let Some(&pid) = pids.get(n) {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} never recorded pid {n}",
                file.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Waits for `pid` to go: a group's member that `init` reaps once its group is killed.
    #[cfg(unix)]
    fn goes(pid: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !gone(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "{pid} outlived its group's end"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The environment of a config in `dir` naming `helper` as its store, found on `dir`.
    #[cfg(unix)]
    fn stored_by(dir: &Path, helper: &str) -> HashMap<String, String> {
        std::fs::write(dir.join("config.json"), format!(r#"{{"credsStore":"{helper}"}}"#)).unwrap();
        HashMap::from([
            ("DOCKER_CONFIG".to_string(), dir.to_string_lossy().into_owned()),
            ("PATH".to_string(), format!("{}:/bin:/usr/bin", dir.display())),
        ])
    }

    /// Helpers that never answer, their pids recorded in `pids`: `stuck`, and `parent`,
    /// whose own child holds its output open as it waits.
    #[cfg(unix)]
    fn stuck(dir: &Path, pids: &Path) {
        program(
            &dir.join("docker-credential-stuck"),
            &format!(
                "#!/bin/sh\necho $$ >> '{}'\nexec /bin/sleep 3613\n",
                pids.display()
            ),
        );
        program(
            &dir.join("docker-credential-parent"),
            &format!(
                "#!/bin/sh\necho $$ >> '{0}'\n/bin/sleep 3613 &\necho $! >> '{0}'\nwait\n",
                pids.display()
            ),
        );
    }

    /// A helper is started as it is found and never waited for unless its answer is read:
    /// dropped unread, it is ended and reaped. Read, it is waited for, and a cancel ends the
    /// wait at once, and the helper with it, its own child that holds its output open too.
    #[cfg(unix)]
    #[test]
    fn a_helper_is_waited_for_only_where_its_answer_is_read() {
        let dir = temp("stuck-helper");
        let pids = dir.join("pids");
        stuck(&dir, &pids);
        let env = stored_by(&dir, "stuck");
        let env = |k: &str| env.get(k).cloned();
        let (found, _) = super::find("stuck.example", &env, None).unwrap();
        let Found::Asked(asked) = found else {
            panic!("asked: {found:?}");
        };
        let helper = recorded(&pids, 0);
        assert!(!asked.answered() && !gone(helper));
        drop(asked);
        assert!(gone(helper), "ended and reaped as it dropped");
        let parent = stored_by(&dir, "parent");
        let parent = |k: &str| parent.get(k).cloned();
        let cancel = Cancel::new();
        let (found, _) = super::find("stuck.example", &parent, Some(&cancel)).unwrap();
        let Found::Asked(asked) = found else {
            panic!("asked: {found:?}");
        };
        assert_eq!(
            asked.notice(),
            format!(
                "Waiting for docker-credential-parent to answer for stuck.example (credsStore in {})",
                dir.join("config.json").display()
            )
        );
        let (helper, child) = (recorded(&pids, 1), recorded(&pids, 2));
        std::thread::scope(|s| {
            let waiting = s.spawn(move || asked.answer());
            cancel.cancel();
            let e = waiting.join().unwrap().unwrap_err();
            assert_eq!(e.kind(), crate::ErrorKind::Cancelled, "{e}");
        });
        assert!(gone(helper), "ended and reaped with its cancel");
        goes(child);
        // Cancelled first, one is ended as it is found: its answer, which the helper would
        // hold for an hour, is refused at once.
        let (found, _) = super::find("stuck.example", &env, Some(&cancel)).unwrap();
        let e = found.answer().unwrap_err();
        assert_eq!(e.kind(), crate::ErrorKind::Cancelled, "{e}");
    }

    /// A registry token in the file wins over a helper's answer: no helper is asked. One that
    /// is not on PATH fails only the answer that needs it.
    #[cfg(unix)]
    #[test]
    fn a_registry_token_asks_no_helper_and_a_missing_one_fails_only_its_answer() {
        let dir = temp("token-helper");
        let marks = dir.join("marks");
        program(
            &dir.join("docker-credential-marking"),
            &format!("#!/bin/sh\necho asked >> '{}'\n", marks.display()),
        );
        std::fs::write(
            dir.join("config.json"),
            r#"{"credsStore":"marking","auths":{"t.example":{"registrytoken":"rt"}}}"#,
        )
        .unwrap();
        let path = format!("{}:/bin:/usr/bin", dir.display());
        let config = dir.to_string_lossy().into_owned();
        let env = |k: &str| match k {
            "DOCKER_CONFIG" => Some(config.clone()),
            "PATH" => Some(path.clone()),
            _ => None,
        };
        let (found, _) = super::find("t.example", &env, None).unwrap();
        assert!(
            matches!(&found, Found::Known(Credentials::RegistryToken(t)) if t == "rt"),
            "{found:?}"
        );
        assert!(!marks.exists(), "no helper asked");
        let missing = stored_by(&dir, "absent");
        let missing = |k: &str| missing.get(k).cloned();
        let (found, _) = super::find("a.example", &missing, None).unwrap();
        let e = found.answer().unwrap_err();
        assert!(e.to_string().contains("docker-credential-absent"), "{e}");
    }

    /// A 401 carrying `challenge`, as a registry's would, from `client`.
    #[cfg(unix)]
    fn refusal<'c>(client: &'c crate::http::Client, challenge: &str) -> crate::http::Response<'c> {
        let server = crate::testing::serve(
            None,
            vec![(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {challenge}\r\nContent-Length: 0\r\n\r\n"
                )
                .into_bytes(),
                crate::testing::After::Keep,
            )],
        );
        let url = crate::url::Url::parse(&format!("http://127.0.0.1:{}/v2/", server.port)).unwrap();
        client
            .send(&crate::http::Request {
                method: "GET",
                url: &url,
                headers: &[],
                body: &[],
                file: None,
            })
            .unwrap()
    }

    /// A helper's answer is read as its registry's own host challenges, and answers it; a
    /// challenge of another host reads none and waits on none, a stuck helper's included,
    /// and what waits on one says so.
    #[cfg(unix)]
    #[test]
    fn a_helpers_answer_is_read_where_its_registry_challenges() {
        use crate::auth::Authorizer;
        let dir = temp("challenged-helper");
        helper(&dir, "fake");
        let pids = dir.join("pids");
        stuck(&dir, &pids);
        let client = crate::http::Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let registry = crate::url::Url::parse("http://found.example/v2/").unwrap();
        let other = crate::url::Url::parse("http://other.example/v2/").unwrap();
        let fake = stored_by(&dir, "fake");
        let fake = |k: &str| fake.get(k).cloned();
        let (found, _) = super::find("found.example", &fake, None).unwrap();
        let auth = Authorizer::new(&registry, found);
        assert!(
            auth.challenged(&registry, &refusal(&client, r#"Basic realm="r""#), false)
                .unwrap()
        );
        assert_eq!(
            auth.authorization(&client, &registry, &[]).unwrap().as_deref(),
            Some("Basic dTpz")
        );
        let stuck_env = stored_by(&dir, "stuck");
        let stuck_env = |k: &str| stuck_env.get(k).cloned();
        let cancel = Cancel::new();
        let (found, _) = super::find("found.example", &stuck_env, Some(&cancel)).unwrap();
        let auth = Authorizer::new(&registry, found);
        std::thread::scope(|s| {
            let waiting =
                s.spawn(|| auth.challenged(&registry, &refusal(&client, r#"Basic realm="r""#), false));
            let notice = auth.waits().unwrap();
            assert!(
                notice.starts_with("Waiting for docker-credential-stuck to answer for found.example"),
                "{notice}"
            );
            let helper = recorded(&pids, 0);
            // Another host waits on none: refused for want of basic credentials, at once.
            let e = auth
                .challenged(&other, &refusal(&client, r#"Basic realm="o""#), false)
                .unwrap_err();
            assert!(e.to_string().contains("no basic auth credentials"), "{e}");
            assert_eq!(auth.authorization(&client, &other, &[]).unwrap(), None);
            cancel.cancel();
            let e = waiting.join().unwrap().unwrap_err();
            assert_eq!(e.kind(), crate::ErrorKind::Cancelled, "{e}");
            goes(helper);
        });
    }
}
