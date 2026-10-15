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

/// `SHARDS_AUTH_CONFIG` or `DOCKER_AUTH_CONFIG`: `auths` alone, each with `auth` alone.
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
}

/// The helper protocol's answer (`credentials/credentials.go`).
#[derive(Deserialize)]
struct HelperAnswer {
    #[serde(rename = "Username", default)]
    username: String,
    #[serde(rename = "Secret", default)]
    secret: String,
}

/// The credentials `docker login` left for the registry `domain` (a reference's domain),
/// and any warnings Docker would print on the way.
pub fn lookup(domain: &str, env: Env<'_>) -> Result<(Credentials, Vec<String>), Error> {
    let key = if domain == "docker.io" || domain == "index.docker.io" {
        HUB_KEY
    } else {
        domain
    };
    let mut warnings = Vec::new();
    // Credentials in the environment: shards' own variable, else Docker's.
    let given = ["SHARDS_AUTH_CONFIG", "DOCKER_AUTH_CONFIG"]
        .into_iter()
        .find_map(|var| env(var).filter(|v| !v.is_empty()).map(|v| (var, v)));
    if let Some((var, value)) = given {
        match env_config(var, &value) {
            Ok(entries) => {
                if let Some((username, password)) = entries.get(key) {
                    return Ok((password_credentials(username, password), warnings));
                }
            }
            Err(e) => warnings.push(format!("Failed to create credential store from {var}: {e}")),
        }
    }
    let Some(dir) = config_dir(env) else {
        return Ok((Credentials::Anonymous, warnings));
    };
    let config = load(&dir.join("config.json"))?;
    let entry = file_entry(&config, key)?;
    let holds_auth =
        !config.creds_store.is_empty() || !config.cred_helpers.is_empty() || !config.auths.is_empty();
    let helper = match config.cred_helpers.get(key) {
        Some(helper) => Some(helper.clone()),
        None if !config.creds_store.is_empty() => Some(config.creds_store.clone()),
        None if !holds_auth => default_helper(env),
        None => None,
    };
    let entry = match helper {
        None => entry,
        // The helper's answer replaces the file's, even when it has none.
        Some(helper) => {
            let mut from_helper = Entry {
                registrytoken: entry.registrytoken,
                ..Entry::default()
            };
            if let Some(answer) = ask_helper(&helper, key, env)? {
                if answer.username == TOKEN_USERNAME {
                    from_helper.identitytoken = answer.secret;
                } else {
                    from_helper.username = answer.username;
                    from_helper.password = answer.secret;
                }
            }
            from_helper
        }
    };
    Ok((credentials(entry), warnings))
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
fn env_config(var: &str, value: &str) -> Result<BTreeMap<String, (String, String)>, Error> {
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
        if entry.auth.is_empty() {
            return Err(Error::new(format!(
                "{var} environment variable is missing key `auth` for {addr}"
            )));
        }
        out.insert(addr, decode_auth(&entry.auth)?);
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

/// Runs `docker-credential-<helper> get` with `key` on its input. Its errors go to our
/// stderr, as Docker's do. A helper that has nothing for `key` gives `None`.
fn ask_helper(helper: &str, key: &str, env: Env<'_>) -> Result<Option<HelperAnswer>, Error> {
    let program = format!("docker-credential-{helper}");
    let (failed, out) = run_helper(&program, "get", key.as_bytes(), env)?;
    let text = String::from_utf8_lossy(&out);
    if let Some(status) = failed {
        if text.trim() == NOT_FOUND {
            return Ok(None);
        }
        return Err(Error::new(format!(
            "error getting credentials - err: {status}, out: `{}`",
            text.trim()
        )));
    }
    let answer = serde_json::Deserializer::from_slice(&out)
        .into_iter::<HelperAnswer>()
        .next()
        .ok_or_else(|| Error::new(format!("{program} answered nothing")))?
        .map_err(|e| Error::new(format!("{program}: {e}")))?;
    Ok(Some(answer))
}

/// Runs `program get`, found on `env`'s PATH (else ours) and given it, with `key` on its
/// input and our stderr for its own: how it failed, if it did, and what it wrote,
/// `MAX_HELPER_OUTPUT` at most. A helper may exit without reading its input: its status
/// and output then say what it had to say, as Go's os/exec, which Docker's helper client
/// uses, ignores the broken pipe (exec.go, skipStdinCopyError). However the talk goes, the
/// helper is waited for, so it leaves no zombie; one still running after a failed talk is
/// ended first.
///
/// Made by shards_ipc, as the daemon's every child is: by its spawner, so that the helper
/// holds nothing the daemon lets go of while it starts, a run's published port among them
/// (PM M158). How it failed is said as Go's os.ProcessState says it, as Docker does: an
/// exit status (128 and its number, for a signal that ended it).
#[cfg(unix)]
fn run_helper(
    program: &str,
    action: &str,
    input: &[u8],
    env: Env<'_>,
) -> Result<(Option<String>, Vec<u8>), Error> {
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
    let (mut from_stdout, stdout) = std::io::pipe().map_err(fail)?;
    let set: Vec<(&str, &std::ffi::OsStr)> = path.iter().map(|p| ("PATH", p.as_ref())).collect();
    let child = shards_ipc::spawn_with(
        &found,
        &[action.as_ref()],
        &[
            (stdin.as_fd(), 0),
            (stdout.as_fd(), 1),
            (std::io::stderr().as_fd(), 2),
        ],
        false,
        &set,
    )
    .map_err(fail)?;
    drop((stdin, stdout));
    let talked = (|| {
        match to_stdin.write_all(input) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(Error::new(format!("{program}: {e}"))),
        }
        drop(to_stdin);
        let mut out = Vec::new();
        (&mut from_stdout)
            .take(MAX_HELPER_OUTPUT)
            .read_to_end(&mut out)
            .map_err(|e| Error::new(format!("{program}: {e}")))?;
        Ok(out)
    })();
    if talked.is_err() {
        let _ = child.kill(libc::SIGKILL);
    }
    let status = child.wait().map_err(|e| Error::new(format!("{program}: {e}")))?;
    Ok(((status != 0).then(|| format!("exit status {status}")), talked?))
}

/// [`run_helper`] where shards has no daemon: by std's Command.
#[cfg(not(unix))]
fn run_helper(
    program: &str,
    action: &str,
    input: &[u8],
    env: Env<'_>,
) -> Result<(Option<String>, Vec<u8>), Error> {
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
    let talked = (|| {
        if let Some(mut stdin) = child.stdin.take() {
            match stdin.write_all(input) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(e) => return Err(Error::new(format!("{program}: {e}"))),
            }
        }
        let mut out = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(MAX_HELPER_OUTPUT)
                .read_to_end(&mut out)
                .map_err(|e| Error::new(format!("{program}: {e}")))?;
        }
        Ok(out)
    })();
    if talked.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| Error::new(format!("{program}: {e}")))?;
    Ok(((!status.success()).then(|| status.to_string()), talked?))
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
        use std::os::unix::fs::PermissionsExt;
        let bin = temp("deaf-helper");
        let path = bin.join("docker-credential-deaf");
        std::fs::write(
            &path,
            "#!/bin/sh\nexec 0<&-\necho '{\"ServerURL\":\"x\",\"Username\":\"u\",\"Secret\":\"s\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = format!("{}:/bin:/usr/bin", bin.display());
        let env = [("PATH", dirs.as_str())];
        let long = "k".repeat(1 << 20);
        let answer = ask_helper("deaf", &long, &|name| {
            env.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())
        });
        assert!(matches!(answer, Ok(Some(_))), "{:?}", answer.err());
        let _ = std::fs::remove_dir_all(&bin);
    }

    /// A credential helper that knows `found.example` and `token.example`.
    #[cfg(unix)]
    fn helper(dir: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let script = r#"#!/bin/sh
read -r key || true
case "$key" in
found.example) echo '{"ServerURL":"found.example","Username":"u","Secret":"s"}' ;;
token.example) echo '{"ServerURL":"token.example","Username":"<token>","Secret":"idt"}' ;;
broken.example) echo 'the keychain is locked'; exit 1 ;;
*) echo 'credentials not found in native keychain'; exit 1 ;;
esac
"#;
        let path = dir.join(format!("docker-credential-{name}"));
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            helper(&bin, "unused");
            std::fs::copy(bin.join("docker-credential-unused"), bin.join("pass")).unwrap();
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
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp("store-helper");
        let log = dir.join("helper.log");
        let helper = dir.join("docker-credential-shardstest");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nIFS= read -r line || true\nprintf '%s %s\\n' \"$1\" \"$line\" >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
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
}
