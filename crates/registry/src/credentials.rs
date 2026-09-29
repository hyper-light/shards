//! Credentials, found as the Docker CLI v29.8.1 finds them (docs/research/registry-pull.md
//! §3.3), so `docker login` works unchanged:
//! - `config.json` from `$DOCKER_CONFIG`, else `~/.docker` (`cli/config/config.go`);
//! - a registry's key is its host, or `https://index.docker.io/v1/` for Docker Hub;
//! - `DOCKER_AUTH_CONFIG` first, then the registry's credential helper, the config's
//!   `credsStore`, or its `auths` (`configfile/file.go`, `credentials/`);
//! - the platform's default helper when the config holds no credentials at all;
//! - dockerd's precedence: a registry token, then an identity token, then a password
//!   (`daemon/containerd/resolver.go`).
//!
//! Nothing is written: `shards login` comes later.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// `DOCKER_AUTH_CONFIG`: `auths` alone, each with `auth` alone.
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
    if let Some(value) = env("DOCKER_AUTH_CONFIG").filter(|v| !v.is_empty()) {
        match env_config(&value) {
            Ok(entries) => {
                if let Some((username, password)) = entries.get(key) {
                    return Ok((password_credentials(username, password), warnings));
                }
            }
            Err(e) => warnings.push(format!(
                "Failed to create credential store from DOCKER_AUTH_CONFIG: {e}"
            )),
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

/// `parseEnvConfig`.
fn env_config(value: &str) -> Result<BTreeMap<String, (String, String)>, Error> {
    let mut values = serde_json::Deserializer::from_str(value).into_iter::<EnvConfig>();
    let parsed = match values.next() {
        None => EnvConfig {
            auths: BTreeMap::new(),
        },
        Some(parsed) => parsed.map_err(|e| Error::new(e.to_string()))?,
    };
    if values.next().is_some() {
        return Err(Error::new(
            "DOCKER_AUTH_CONFIG does not support more than one JSON object",
        ));
    }
    let mut out = BTreeMap::new();
    for (addr, entry) in parsed.auths {
        if entry.auth.is_empty() {
            return Err(Error::new(format!(
                "DOCKER_AUTH_CONFIG environment variable is missing key `auth` for {addr}"
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
    let mut command = Command::new(&program);
    command
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = env("PATH") {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|e| Error::new(format!("running {program}: {e}")))?;
    // A helper may exit without reading its input: its status and output then say what
    // it had to say, as Go's os/exec, which Docker's helper client uses, ignores the
    // broken pipe (exec.go, skipStdinCopyError).
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(key.as_bytes()) {
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
    let status = child.wait().map_err(|e| Error::new(format!("{program}: {e}")))?;
    let text = String::from_utf8_lossy(&out);
    if !status.success() {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-creds-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn logins_are_found_under_docker_s_keys() {
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
    fn docker_auth_config_comes_first_and_falls_back_with_a_warning() {
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
}
