//! What Docker's daemon configuration says of registries (docs/design/architecture.md
//! D194): `insecure-registries`, which may be asked in plain HTTP, and `registry-mirrors`,
//! Docker Hub's. Read from each file Docker's daemons read (docker/docs
//! `content/manuals/engine/daemon/_index.md`; Docker Desktop's,
//! `content/manuals/desktop/settings-and-maintenance/settings.md`), what they name taken
//! together, and checked as dockerd checks them (moby docker-v29.8.1
//! `daemon/pkg/registry/config.go`, `loadInsecureRegistries`, `ValidateMirror`):
//! - Docker Desktop's `~/.docker/daemon.json`;
//! - the rootless engine's `$XDG_CONFIG_HOME/docker/daemon.json`, or
//!   `~/.config/docker/daemon.json`;
//! - the native engine's `/etc/docker/daemon.json`, or
//!   `%PROGRAMDATA%\docker\config\daemon.json` on Windows.
//!
//! A missing file names nothing; one that cannot be read, or whose two keys do not hold
//! what dockerd takes, is an error that names it, as dockerd would not start with it.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;

use crate::Error;
use crate::credentials::Env;

/// The registries Docker's daemon configuration names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Registries {
    /// Registries named by `host[:port]`, lowercased, in the order named.
    hosts: Vec<String>,
    /// Networks whose registries may be asked in plain HTTP: an address and its prefix's
    /// length, masked.
    networks: Vec<(IpAddr, u8)>,
    /// Docker Hub's mirrors, in order, each `scheme://host[:port][/path]/`.
    mirrors: Vec<String>,
}

#[derive(Deserialize, Default)]
struct DaemonJson {
    #[serde(rename = "insecure-registries", default)]
    insecure: Option<Vec<String>>,
    #[serde(rename = "registry-mirrors", default)]
    mirrors: Option<Vec<String>>,
}

/// The networks dockerd counts insecure whatever its configuration says
/// (`loadInsecureRegistries`): loopback's, which a registry is asked in plain HTTP on
/// (auth.rs, `loopback`).
pub const LOOPBACK: [&str; 2] = ["::1/128", "127.0.0.0/8"];

impl Registries {
    /// What the files Docker's daemons read name, found as `env` says.
    pub fn from_env(env: Env<'_>) -> Result<Registries, Error> {
        let mut registries = Registries::default();
        for path in files(env) {
            let text = match std::fs::read(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::from(e).context(path.display())),
            };
            registries
                .take(&text)
                .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
        }
        Ok(registries)
    }

    /// Takes in what one daemon.json names.
    fn take(&mut self, text: &[u8]) -> Result<(), String> {
        let file: DaemonJson = serde_json::from_slice(text).map_err(|e| e.to_string())?;
        for given in file.insecure.unwrap_or_default() {
            match insecure(&given)? {
                Listed::Host(host) => {
                    if !self.hosts.contains(&host) {
                        self.hosts.push(host);
                    }
                }
                Listed::Network(net) => {
                    if !self.networks.contains(&net) {
                        self.networks.push(net);
                    }
                }
            }
        }
        for given in file.mirrors.unwrap_or_default() {
            let mirror = mirror(&given)?;
            if !self.mirrors.contains(&mirror) {
                self.mirrors.push(mirror);
            }
        }
        Ok(())
    }

    /// Whether the registry at `authority` (`host[:port]`, as a reference names it) is
    /// named insecure: by that name itself, as dockerd keys its index configurations; or
    /// by a network one of its addresses is in, a name looked up for it (`isCIDRMatch`),
    /// where any network is named. Loopback's are not looked up for (auth.rs,
    /// `loopback`): a registry is taken for loopback's by its name or address alone.
    pub fn insecure(&self, authority: &str) -> bool {
        let authority = authority.to_ascii_lowercase();
        if self.hosts.contains(&authority) {
            return true;
        }
        if self.networks.is_empty() {
            return false;
        }
        // Looked up as a connection to it looks it up, within a dial's time (http.rs,
        // `dial`); a name that cannot be is in no network, as `isCIDRMatch` finds.
        let within = crate::http::Patience::default().connect;
        let addresses: Vec<IpAddr> = crate::url::Url::parse(&format!("https://{authority}/"))
            .and_then(|url| crate::http::resolve(&url, Instant::now() + within, None))
            .map(|found| found.iter().map(|a| a.ip()).collect())
            .unwrap_or_default();
        addresses
            .iter()
            .any(|ip| self.networks.iter().any(|&(net, bits)| contains(net, bits, *ip)))
    }

    /// The registries named by `host[:port]`, in the order named.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The networks named, as dockerd shows them (`InsecureRegistryCIDRs`), loopback's
    /// last.
    pub fn networks(&self) -> Vec<String> {
        let mut shown: Vec<String> = self
            .networks
            .iter()
            .map(|(ip, bits)| format!("{ip}/{bits}"))
            .collect();
        for net in LOOPBACK {
            if !shown.iter().any(|s| s == net) {
                shown.push(net.to_string());
            }
        }
        shown
    }

    /// Docker Hub's mirrors, in order.
    pub fn mirrors(&self) -> &[String] {
        &self.mirrors
    }

    /// What one daemon.json of `json` names.
    pub fn parse(json: &str) -> Result<Registries, String> {
        let mut registries = Registries::default();
        registries.take(json.as_bytes()).map(|()| registries)
    }
}

/// The files Docker's daemons read their configuration from, as `env` finds them.
fn files(env: Env<'_>) -> Vec<PathBuf> {
    let home = if cfg!(windows) {
        env("USERPROFILE")
    } else {
        env("HOME")
    }
    .filter(|h| !h.is_empty());
    let mut files: Vec<PathBuf> = home
        .iter()
        .map(|h| Path::new(h).join(".docker").join("daemon.json"))
        .collect();
    if cfg!(windows) {
        if let Some(data) = env("PROGRAMDATA").filter(|d| !d.is_empty()) {
            files.push(Path::new(&data).join("docker").join("config").join("daemon.json"));
        }
    } else {
        let config = env("XDG_CONFIG_HOME")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| Path::new(h).join(".config")));
        files.extend(config.map(|c| c.join("docker").join("daemon.json")));
        files.push(PathBuf::from("/etc/docker/daemon.json"));
    }
    files
}

/// What an `insecure-registries` entry names.
enum Listed {
    Host(String),
    Network((IpAddr, u8)),
}

/// An `insecure-registries` entry, checked as `loadInsecureRegistries` checks it: an
/// `http://` or `https://` in front taken off, any other scheme refused; a network in
/// CIDR notation; else `host[:port]`.
fn insecure(given: &str) -> Result<Listed, String> {
    if given.starts_with('-') || given.ends_with('-') {
        return Err(format!(
            "invalid index name ({given}). Cannot begin or end with a hyphen"
        ));
    }
    let entry = match given.split_once("://") {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            rest
        }
        Some(_) => return Err(format!("insecure registry {given} should not contain '://'")),
        None => given,
    };
    if let Some(net) = network(entry) {
        return Ok(Listed::Network(net));
    }
    let (host, port) = split_port(entry);
    let named = host.parse::<IpAddr>().is_ok() || domain(host);
    if !named {
        return Err(format!(
            "insecure registry {given} is not valid: invalid host {host:?}"
        ));
    }
    if let Some(port) = port
        && port.parse::<u16>().is_err()
    {
        return Err(format!(
            "insecure registry {given} is not valid: invalid port {port:?}"
        ));
    }
    Ok(Listed::Host(entry.to_ascii_lowercase()))
}

/// `entry` as a network in CIDR notation, masked, as netip.ParsePrefix reads one.
fn network(entry: &str) -> Option<(IpAddr, u8)> {
    let (ip, bits) = entry.split_once('/')?;
    let ip: IpAddr = ip.parse().ok()?;
    if bits.is_empty()
        || bits.len() > 3
        || !bits.bytes().all(|b| b.is_ascii_digit())
        || (bits.len() > 1 && bits.starts_with('0'))
    {
        return None;
    }
    let bits: u8 = bits.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (bits <= max).then(|| (mask(ip, bits), bits))
}

fn mask(ip: IpAddr, bits: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let keep = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            IpAddr::V4((u32::from(v4) & keep).into())
        }
        IpAddr::V6(v6) => {
            let keep = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            IpAddr::V6((u128::from(v6) & keep).into())
        }
    }
}

/// Whether the network `net`/`bits` holds `ip`, an IPv4-mapped IPv6 address taken as its
/// IPv4 one (netip's `Unmap`).
fn contains(net: IpAddr, bits: u8, ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    net.is_ipv4() == ip.is_ipv4() && mask(ip, bits) == net
}

/// `host[:port]` split, a bracketed IPv6 address's brackets taken off.
fn split_port(entry: &str) -> (&str, Option<&str>) {
    if let Some(rest) = entry.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        return (host, after.strip_prefix(':'));
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (entry, None),
    }
}

/// A domain as a reference names one (distribution/reference `domainNameComponent`):
/// labels of letters, digits and hyphens, none beginning or ending with a hyphen.
fn domain(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// A `registry-mirrors` entry, checked as `ValidateMirror` checks it, with one slash at
/// its end.
fn mirror(given: &str) -> Result<String, String> {
    let Some((scheme, _)) = given.split_once("://").filter(|(s, _)| !s.is_empty()) else {
        return Err(format!(
            "invalid mirror: no scheme specified for {given:?}: must use either 'https://' or 'http://'"
        ));
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(format!(
            "invalid mirror: unsupported scheme {scheme:?} in {given:?}: must use either 'https://' or 'http://'"
        ));
    }
    if given.contains('?') || given.contains('#') {
        return Err(format!(
            "invalid mirror: query or fragment at end of the URI {given:?}"
        ));
    }
    let authority = given.split("://").nth(1).and_then(|rest| rest.split('/').next());
    if authority.is_some_and(|a| a.contains('@')) {
        return Err(format!(
            "invalid mirror: username/password not allowed in URI {given:?}"
        ));
    }
    crate::url::Url::parse(given)
        .map_err(|e| format!("invalid mirror: {given:?} is not a valid URI: {e}"))?;
    Ok(format!("{}/", given.strip_suffix('/').unwrap_or(given)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn read(json: &str) -> Result<Registries, String> {
        Registries::parse(json)
    }

    /// dockerd's checks: a scheme in front taken off but for others; networks masked;
    /// hosts and ports as a reference names them; mirrors with a scheme of HTTP's, no
    /// query, fragment or credentials, one slash at their end; repeats once.
    #[test]
    fn daemon_json_names_registries_as_dockerd_reads_it() {
        let r = read(
            r#"{"insecure-registries": ["Registry.LAN:5000", "https://other.lan", "10.1.2.3/8",
                "fd00::1/64", "registry.lan:5000", "[fd00::5]:5000"],
               "registry-mirrors": ["https://mirror.gcr.io", "http://10.0.0.5:5000/", "https://mirror.gcr.io/"],
               "debug": true}"#,
        )
        .unwrap();
        assert_eq!(r.hosts(), ["registry.lan:5000", "other.lan", "[fd00::5]:5000"]);
        assert_eq!(
            r.networks(),
            ["10.0.0.0/8", "fd00::/64", "::1/128", "127.0.0.0/8"]
        );
        assert_eq!(r.mirrors(), ["https://mirror.gcr.io/", "http://10.0.0.5:5000/"]);
        assert_eq!(read("{}").unwrap(), Registries::default());
        assert_eq!(
            read(r#"{"insecure-registries": null, "registry-mirrors": null}"#).unwrap(),
            Registries::default()
        );
        for (json, said) in [
            (
                r#"{"insecure-registries": ["ftp://a.lan"]}"#,
                "should not contain '://'",
            ),
            (
                r#"{"insecure-registries": ["-a.lan"]}"#,
                "Cannot begin or end with a hyphen",
            ),
            (r#"{"insecure-registries": ["a_b.lan:5000"]}"#, "invalid host"),
            (r#"{"insecure-registries": ["a.lan:99999"]}"#, "invalid port"),
            (r#"{"insecure-registries": "a.lan"}"#, "invalid type"),
            (
                r#"{"registry-mirrors": ["mirror.gcr.io"]}"#,
                "no scheme specified",
            ),
            (
                r#"{"registry-mirrors": ["ftp://mirror.gcr.io"]}"#,
                "unsupported scheme",
            ),
            (
                r#"{"registry-mirrors": ["https://mirror.gcr.io/?a=b"]}"#,
                "query or fragment",
            ),
            (
                r#"{"registry-mirrors": ["https://u:p@mirror.gcr.io"]}"#,
                "username/password",
            ),
            ("{", "EOF"),
        ] {
            let got = read(json).unwrap_err();
            assert!(got.contains(said), "{json}: {got}");
        }
    }

    /// A registry is insecure by the name it is listed by, its port included, or by a
    /// network one of its addresses is in; an IPv4-mapped address is its IPv4 one. Names
    /// looked up are RFC 6761's `invalid`, which no resolver finds.
    #[test]
    fn a_registry_is_insecure_by_its_name_or_its_network() {
        let r = read(
            r#"{"insecure-registries": ["registry.invalid:5000", "plain.invalid", "192.168.0.0/16", "fd00::/8"]}"#,
        )
        .unwrap();
        assert!(r.insecure("registry.invalid:5000"));
        assert!(r.insecure("Registry.INVALID:5000"));
        assert!(
            !r.insecure("registry.invalid"),
            "another port is another registry"
        );
        assert!(r.insecure("plain.invalid"));
        assert!(!r.insecure("plain.invalid:5000"));
        assert!(r.insecure("192.168.1.10:5000"));
        assert!(r.insecure("192.168.1.10"));
        assert!(!r.insecure("192.169.1.10:5000"));
        assert!(r.insecure("[fd12::1]:5000"));
        assert!(r.insecure("[::ffff:192.168.3.4]:5000"));
        assert!(!r.insecure("[fe80::1]:5000"));
        // A name is looked up: localhost's addresses are loopback's, in no network named.
        assert!(!r.insecure("localhost:5000"));
        let loopback = read(r#"{"insecure-registries": ["127.0.0.0/8"]}"#).unwrap();
        assert!(loopback.insecure("localhost:5000"));
        // Nothing named, nothing looked up.
        assert!(!Registries::default().insecure("192.168.1.10:5000"));
    }

    /// Each file Docker's daemons read, Desktop's first; what they name together.
    #[test]
    fn every_daemon_json_is_read_together() {
        let home = shards_testdir::TempDir::new("registries").unwrap();
        let desktop = home.join(".docker");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(
            desktop.join("daemon.json"),
            r#"{"insecure-registries": ["a.lan:5000"], "registry-mirrors": ["https://m.lan"]}"#,
        )
        .unwrap();
        let config = home.join("config").join("docker");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("daemon.json"),
            r#"{"insecure-registries": ["b.lan:5000", "a.lan:5000"]}"#,
        )
        .unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let config_s = home.join("config").to_string_lossy().into_owned();
        let env = |k: &str| match k {
            "HOME" | "USERPROFILE" => Some(home_s.clone()),
            "XDG_CONFIG_HOME" => Some(config_s.clone()),
            "PROGRAMDATA" => Some(home.join("none").to_string_lossy().into_owned()),
            _ => None,
        };
        // The native engine's file is the machine's own: what it names comes after.
        let r = Registries::from_env(&env).unwrap();
        assert_eq!(r.mirrors().first().map(String::as_str), Some("https://m.lan/"));
        let ours = if cfg!(windows) {
            vec!["a.lan:5000"]
        } else {
            vec!["a.lan:5000", "b.lan:5000"]
        };
        assert_eq!(
            r.hosts().get(..ours.len()),
            Some(&ours.iter().map(|h| h.to_string()).collect::<Vec<_>>()[..])
        );
        std::fs::write(desktop.join("daemon.json"), "{").unwrap();
        let said = Registries::from_env(&env).unwrap_err().to_string();
        assert!(
            said.contains(&desktop.join("daemon.json").display().to_string()),
            "{said}"
        );
    }
}
