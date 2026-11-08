//! The shared config and credentials files (D168), as aws-sdk-go-v2's config package reads
//! them (config/shared_config.go, config/internal/ini), so that a profile means what it
//! means to the AWS CLI and every SDK:
//! - its INI dialect: `#` and `;` comments, whole lines or after a space or a tab; `=`, or
//!   `:` where it comes first; a property left empty given indented sub-properties, and an
//!   indented line after one with text a continuation of it; quotes taken off a value;
//! - the config file's profiles are `[profile NAME]`, and `[default]`; its other sections
//!   but `[sso-session NAME]` and `[services NAME]` are passed over; the credentials file's
//!   are `[NAME]`;
//! - the credentials file's values are laid over the config file's, its keys only as a
//!   pair;
//! - a profile's `source_profile` is followed, a profile met again being its keys alone,
//!   and one that has keys ending the chain.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

/// A property's value: its text, or the sub-properties of a property left empty.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Text(String),
    Map(BTreeMap<String, String>),
}

/// A section's properties, and what is wrong with it (a key without its secret).
#[derive(Debug, Clone, Default)]
struct Section {
    values: BTreeMap<String, Value>,
    errors: Vec<String>,
}

impl Section {
    fn text(&self, key: &str) -> Option<&str> {
        match self.values.get(key) {
            Some(Value::Text(t)) => Some(t),
            _ => None,
        }
    }

    fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

type Sections = BTreeMap<String, Section>;

/// The keys a profile's are laid over another's by, beside its keys and token
/// (mergeSections' stringKeys).
const MERGED: [&str; 29] = [
    "role_arn",
    "source_profile",
    "credential_source",
    "external_id",
    "mfa_serial",
    "role_session_name",
    "region",
    "endpoint_discovery_enabled",
    "credential_process",
    "web_identity_token_file",
    "s3_use_arn_region",
    "s3_disable_multiregion_access_points",
    "ec2_metadata_service_endpoint_mode",
    "ec2_metadata_service_endpoint",
    "ec2_metadata_v1_disabled",
    "use_dualstack_endpoint",
    "use_fips_endpoint",
    "defaults_mode",
    "retry_mode",
    "ca_bundle",
    "duration_seconds",
    "max_attempts",
    "sso_session",
    "sso_account_id",
    "sso_region",
    "sso_role_name",
    "sso_start_url",
    "auth_scheme_preference",
    "login_session",
];

/// `text` as the SDK's ini.Parse reads it.
fn parse(text: &str) -> Sections {
    let mut sections = Sections::new();
    let mut current: Option<String> = None;
    let mut key: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start_matches([' ', '\t']).starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = as_profile(line) {
            key = None;
            sections.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let Some(section) = current.as_ref().and_then(|c| sections.get_mut(c)) else {
            continue;
        };
        let indented = line.starts_with([' ', '\t']);
        if !indented {
            // A property, or a line passed over.
            let trimmed = trim_property_comment(line).trim_end_matches([' ', '\t']);
            if let Some((k, v)) = split_property(trimmed) {
                section.values.insert(k.clone(), Value::Text(unquote(&v)));
                key = Some(k);
            }
            continue;
        }
        let trimmed = line.trim_start_matches([' ', '\t']);
        match split_property(trimmed) {
            // A sub-property: of the property before it, where that is empty.
            Some((k, v)) => {
                let parent = key.as_ref().and_then(|p| section.values.get_mut(p));
                match parent {
                    Some(Value::Map(map)) => {
                        map.insert(k, unquote(&v));
                    }
                    Some(value) if *value == Value::Text(String::new()) => {
                        *value = Value::Map(BTreeMap::from([(k, unquote(&v))]));
                    }
                    // An orphan, made a property of its own: unquoted, then its comment
                    // taken off, as the SDK does.
                    _ => {
                        let v = unquote(&v);
                        let v = trim_property_comment(&v).trim().to_string();
                        section.values.insert(k.clone(), Value::Text(v));
                        key = Some(k);
                    }
                }
            }
            // A continuation of the property before it, where that has text.
            None => {
                if let Some(Value::Text(t)) = key.as_ref().and_then(|p| section.values.get_mut(p))
                    && !t.is_empty()
                {
                    t.push('\n');
                    t.push_str(trimmed);
                }
            }
        }
    }
    sections
}

/// A section's name, where `line` is a section's line: `[type name]`, comments taken off,
/// its type and name joined by a space.
fn as_profile(line: &str) -> Option<String> {
    let line = line.split('#').next().unwrap_or_default();
    let line = line.split(';').next().unwrap_or_default().trim();
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    // splitProfile: the type, to the first run of spaces, and the name after it.
    Some(match inner.find([' ', '\t']) {
        None => inner.to_string(),
        Some(first) => {
            let rest = inner
                .get(first..)
                .unwrap_or_default()
                .trim_start_matches([' ', '\t']);
            format!("{} {rest}", inner.get(..first).unwrap_or_default())
        }
    })
}

/// A property's line without its comment: from a `#` or `;` after a space or a tab.
fn trim_property_comment(s: &str) -> &str {
    [" #", " ;", "\t#", "\t;"]
        .iter()
        .filter_map(|c| s.find(c))
        .min()
        .and_then(|end| s.get(..end))
        .unwrap_or(s)
}

/// A property's key, lowercased, and its value, both trimmed: split at `=`, or at `:`
/// where that comes first.
fn split_property(s: &str) -> Option<(String, String)> {
    let at = match (s.find('='), s.find(':')) {
        (Some(e), Some(c)) => e.min(c),
        (Some(e), None) => e,
        (None, Some(c)) => c,
        (None, None) => return None,
    };
    let (k, v) = (s.get(..at)?, s.get(at + 1..)?);
    Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
}

/// A value without the quotes around it, `'…'` or `"…"`.
fn unquote(s: &str) -> String {
    let quoted = s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')));
    match s.get(1..s.len().saturating_sub(1)) {
        Some(inner) if quoted => inner.to_string(),
        _ => s.to_string(),
    }
}

/// The config file's sections as profiles, `[profile NAME]` named `NAME` over a `[NAME]`
/// the file has too; its `[default]`, `[sso-session NAME]` and `[services NAME]`; and
/// none of the others (processConfigSections).
fn config_sections(mut sections: Sections) -> Sections {
    let names: Vec<String> = sections.keys().cloned().collect();
    let mut kept = BTreeSet::new();
    for name in names {
        if kept.contains(&name) {
            continue;
        }
        if let Some(profile) = name.strip_prefix("profile ") {
            if let Some(section) = sections.remove(&name) {
                sections.insert(profile.to_string(), section);
                kept.insert(profile.to_string());
            }
        } else if !(name.starts_with("sso-session ")
            || name.starts_with("services ")
            || name.eq_ignore_ascii_case("default"))
        {
            sections.remove(&name);
        }
    }
    sections
}

/// The credentials file's sections, but those named `profile …` (processCredentialsSections).
fn credentials_sections(mut sections: Sections) -> Sections {
    sections.retain(|name, _| !name.starts_with("profile "));
    sections
}

/// `src`'s sections laid over `dst`'s (mergeSections): its keys only as a pair, with its
/// token; each of [`MERGED`] it has; a section that has a key without its secret, or a
/// secret without its key, wrong.
fn merge(dst: &mut Sections, src: Sections) {
    for (name, mut section) in src {
        if section.has("aws_access_key_id") != section.has("aws_secret_access_key") {
            section
                .errors
                .push(format!("partial credentials found for profile {name}"));
        }
        let Some(into) = dst.get_mut(&name) else {
            dst.insert(name, section);
            continue;
        };
        into.errors.clone_from(&section.errors);
        if section.has("aws_access_key_id") && section.has("aws_secret_access_key") {
            for key in ["aws_access_key_id", "aws_secret_access_key", "aws_session_token"] {
                if let Some(v) = section.values.remove(key) {
                    into.values.insert(key.to_string(), v);
                }
            }
        }
        for key in MERGED {
            if let Some(v) = section.values.remove(key) {
                into.values.insert(key.to_string(), v);
            }
        }
    }
}

/// A profile's static keys.
#[derive(Clone)]
pub struct Keys {
    pub access_key: String,
    pub secret: Zeroizing<String>,
    pub token: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys({})", self.access_key)
    }
}

/// An SSO session's section (`[sso-session NAME]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SsoSession {
    pub name: String,
    pub region: String,
    pub start_url: String,
}

/// A profile, as the SDK's SharedConfig holds what it says of credentials, its region,
/// IMDS and endpoints.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub name: String,
    pub keys: Option<Keys>,
    pub role_arn: String,
    pub external_id: String,
    pub mfa_serial: String,
    pub role_session_name: String,
    pub source_profile: String,
    pub credential_source: String,
    pub region: String,
    pub sso_session_name: String,
    pub sso_region: String,
    pub sso_start_url: String,
    pub sso_account_id: String,
    pub sso_role_name: String,
    /// `duration_seconds`: absent; or its seconds, 0 where it is not a number (the SDK's
    /// quirk).
    pub duration_seconds: Option<u64>,
    pub credential_process: String,
    pub web_identity_token_file: String,
    pub ec2_metadata_service_endpoint: String,
    pub ec2_metadata_service_endpoint_mode: String,
    pub ec2_metadata_v1_disabled: Option<bool>,
    pub ca_bundle: String,
    /// `use_fips_endpoint`, `use_dualstack_endpoint`.
    pub use_fips_endpoint: Option<bool>,
    pub use_dualstack_endpoint: Option<bool>,
    pub endpoint_url: String,
    pub ignore_configured_endpoint_urls: Option<bool>,
    /// The profile's `[services NAME]` section's: each service's sub-properties.
    pub services: BTreeMap<String, BTreeMap<String, String>>,
    pub login_session: String,
    /// The profile `source_profile` names, followed.
    pub source: Option<Box<Profile>>,
    /// The section `sso_session` names.
    pub sso_session: Option<SsoSession>,
}

impl Profile {
    /// Whether it says where credentials come from (hasCredentials).
    pub fn has_credentials(&self) -> bool {
        !self.source_profile.is_empty()
            || !self.credential_source.is_empty()
            || !self.credential_process.is_empty()
            || !self.web_identity_token_file.is_empty()
            || self.has_sso()
            || self.keys.is_some()
    }

    /// Whether it signs in with SSO, by a session (`sso_session`) or the legacy keys.
    pub fn has_sso(&self) -> bool {
        !self.sso_session_name.is_empty() || self.has_legacy_sso()
    }

    pub fn has_legacy_sso(&self) -> bool {
        !self.sso_region.is_empty()
            || !self.sso_account_id.is_empty()
            || !self.sso_start_url.is_empty()
            || !self.sso_role_name.is_empty()
    }

    fn clear_assume_role(&mut self) {
        self.role_arn.clear();
        self.external_id.clear();
        self.mfa_serial.clear();
        self.role_session_name.clear();
        self.source_profile.clear();
    }

    fn clear_credentials(&mut self) {
        self.credential_source.clear();
        self.credential_process.clear();
        self.web_identity_token_file.clear();
        self.keys = None;
        self.sso_account_id.clear();
        self.sso_region.clear();
        self.sso_role_name.clear();
        self.sso_start_url.clear();
    }

    /// Its values, from `section` (setFromIniSection).
    fn set(&mut self, section: &Section) -> Result<(), String> {
        if !section.errors.is_empty() {
            return Err(format!("profile {}: {}", self.name, section.errors.join("; ")));
        }
        let text = |k: &str| section.text(k).map(str::to_string);
        let set = |field: &mut String, k: &str| {
            if let Some(v) = text(k) {
                *field = v;
            }
        };
        set(&mut self.role_arn, "role_arn");
        set(&mut self.external_id, "external_id");
        set(&mut self.mfa_serial, "mfa_serial");
        set(&mut self.role_session_name, "role_session_name");
        set(&mut self.source_profile, "source_profile");
        set(&mut self.credential_source, "credential_source");
        set(&mut self.region, "region");
        set(&mut self.sso_session_name, "sso_session");
        set(&mut self.sso_region, "sso_region");
        set(&mut self.sso_start_url, "sso_start_url");
        set(&mut self.sso_account_id, "sso_account_id");
        set(&mut self.sso_role_name, "sso_role_name");
        if section.has("duration_seconds") {
            self.duration_seconds = Some(text("duration_seconds").and_then(|v| v.parse().ok()).unwrap_or(0));
        }
        set(&mut self.credential_process, "credential_process");
        set(&mut self.web_identity_token_file, "web_identity_token_file");
        set(
            &mut self.ec2_metadata_service_endpoint,
            "ec2_metadata_service_endpoint",
        );
        set(
            &mut self.ec2_metadata_service_endpoint_mode,
            "ec2_metadata_service_endpoint_mode",
        );
        if let Some(v) = text("ec2_metadata_v1_disabled") {
            self.ec2_metadata_v1_disabled = Some(v.eq_ignore_ascii_case("true"));
        }
        set(&mut self.ca_bundle, "ca_bundle");
        if let Some(v) = text("use_fips_endpoint") {
            self.use_fips_endpoint = Some(v.eq_ignore_ascii_case("true"));
        }
        if let Some(v) = text("use_dualstack_endpoint") {
            self.use_dualstack_endpoint = Some(v.eq_ignore_ascii_case("true"));
        }
        set(&mut self.endpoint_url, "endpoint_url");
        if let Some(v) = text("ignore_configured_endpoint_urls") {
            self.ignore_configured_endpoint_urls = Some(v.eq_ignore_ascii_case("true"));
        }
        set(&mut self.login_session, "login_session");
        if let (Some(id), Some(secret)) = (text("aws_access_key_id"), text("aws_secret_access_key"))
            && !id.is_empty()
            && !secret.is_empty()
        {
            self.keys = Some(Keys {
                access_key: id,
                secret: Zeroizing::new(secret),
                token: text("aws_session_token")
                    .filter(|t| !t.is_empty())
                    .map(Zeroizing::new),
            });
        }
        Ok(())
    }
}

/// The files profiles are read from: the config files, then the credentials files.
#[derive(Debug, Clone)]
pub struct Files {
    pub config: Vec<PathBuf>,
    pub credentials: Vec<PathBuf>,
}

impl Files {
    /// The SDK's: `$AWS_CONFIG_FILE` or `~/.aws/config`, `$AWS_SHARED_CREDENTIALS_FILE` or
    /// `~/.aws/credentials`.
    pub fn of(env: &dyn Fn(&str) -> Option<String>) -> Files {
        let var = |k: &str| env(k).filter(|v| !v.is_empty());
        let home = var("HOME").or_else(|| var("USERPROFILE")).unwrap_or_default();
        let in_home = |name: &str| Path::new(&home).join(".aws").join(name);
        Files {
            config: vec![var("AWS_CONFIG_FILE").map_or_else(|| in_home("config"), PathBuf::from)],
            credentials: vec![
                var("AWS_SHARED_CREDENTIALS_FILE").map_or_else(|| in_home("credentials"), PathBuf::from),
            ],
        }
    }
}

/// The sections of `files`, read and laid over each other as the SDK lays them; a file
/// that cannot be read, none of them (loadIniFiles).
fn sections(files: &Files) -> Sections {
    let read = |paths: &[PathBuf], kind: fn(Sections) -> Sections| {
        let mut all = Sections::new();
        for path in paths {
            if let Ok(text) = std::fs::read_to_string(path) {
                merge(&mut all, kind(parse(&text)));
            }
        }
        all
    };
    let mut config = read(&files.config, config_sections);
    merge(&mut config, read(&files.credentials, credentials_sections));
    config
}

/// Profile `name` of `files`, its `source_profile` followed; `None` where neither file has
/// it.
pub fn load(files: &Files, name: &str) -> Result<Option<Profile>, String> {
    let sections = sections(files);
    if !sections.contains_key(name) {
        return Ok(None);
    }
    let mut seen = BTreeSet::new();
    resolve(&sections, name, &mut seen).map(Some)
}

/// Profile `name` of `sections` (setFromIniSections).
fn resolve(sections: &Sections, name: &str, seen: &mut BTreeSet<String>) -> Result<Profile, String> {
    let section = sections
        .get(name)
        .ok_or_else(|| format!("the AWS profile {name} is in no config or credentials file"))?;
    let mut profile = Profile {
        name: name.to_string(),
        ..Profile::default()
    };
    profile.set(section)?;
    if seen.contains(name) {
        // Met again: its keys alone, as the profile it is the source of.
        profile.clear_assume_role();
    } else {
        let needs_arn = [
            ("source_profile", &profile.source_profile),
            ("credential_source", &profile.credential_source),
            ("web_identity_token_file", &profile.web_identity_token_file),
        ]
        .into_iter()
        .find(|(_, v)| !v.is_empty());
        if let Some((key, _)) = needs_arn
            && profile.role_arn.is_empty()
        {
            return Err(format!("the AWS profile {name} has {key} but no role_arn"));
        }
    }
    if !seen.is_empty() && profile.keys.is_some() {
        return Ok(profile);
    }
    seen.insert(name.to_string());
    let kinds = [
        !profile.source_profile.is_empty(),
        !profile.credential_source.is_empty(),
        !profile.credential_process.is_empty(),
        !profile.web_identity_token_file.is_empty(),
    ];
    if kinds.iter().filter(|k| **k).count() > 1 {
        return Err(format!(
            "the AWS profile {name}: only one credential type may be specified per profile: source profile, credential source, credential process, web identity token"
        ));
    }
    if !profile.source_profile.is_empty() {
        profile.clear_credentials();
        let source_name = profile.source_profile.clone();
        let source = resolve(sections, &source_name, seen).map_err(|e| {
            format!(
                "the AWS profile {name} assumes {} from {source_name}: {e}",
                profile.role_arn
            )
        })?;
        if !source.has_credentials() {
            return Err(format!(
                "the AWS profile {name} assumes {} from {source_name}, which gives no credentials",
                profile.role_arn
            ));
        }
        profile.source = Some(Box::new(source));
    }
    if !profile.sso_session_name.is_empty() {
        let session_name = profile.sso_session_name.trim().to_string();
        let section = sections
            .get(&format!("sso-session {session_name}"))
            .ok_or_else(|| format!("the AWS profile {name}: no [sso-session {session_name}]"))?;
        profile.sso_session = Some(SsoSession {
            name: session_name,
            region: section.text("sso_region").unwrap_or_default().to_string(),
            start_url: section.text("sso_start_url").unwrap_or_default().to_string(),
        });
    }
    if let Some(services) = section.text("services").filter(|s| !s.is_empty())
        && let Some(section) = sections.get(&format!("services {services}"))
    {
        for (service, value) in &section.values {
            if let Value::Map(map) = value {
                profile.services.insert(service.clone(), map.clone());
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn files(dir: &Path, config: &str, credentials: &str) -> Files {
        std::fs::write(dir.join("config"), config).unwrap();
        std::fs::write(dir.join("credentials"), credentials).unwrap();
        Files {
            config: vec![dir.join("config")],
            credentials: vec![dir.join("credentials")],
        }
    }

    /// The config files' INI dialect: comments whole and inline, `:`, quotes,
    /// sub-properties and continuations, section names' spaces.
    #[test]
    fn config_files_ini_dialect_is_read_whole() {
        let s = parse(
            "# a comment\n\
             [profile  a  b] ; its comment\n\
             region = us-east-1 # inline\n\
             role_arn: arn:aws:iam::1:role/r\n\
             quoted = \"x y\"\n\
             lone = '\n\
             url=http://h#not-a-comment\n\
             s3 =\n  max_concurrent_requests = 10\n\
             notes = first\n  second\n",
        );
        let a = &s["profile a  b"];
        assert_eq!(a.text("region"), Some("us-east-1"));
        assert_eq!(a.text("role_arn"), Some("arn:aws:iam::1:role/r"));
        assert_eq!(a.text("quoted"), Some("x y"));
        assert_eq!(a.text("lone"), Some("'"));
        assert_eq!(a.text("url"), Some("http://h#not-a-comment"));
        assert_eq!(
            a.values["s3"],
            Value::Map(BTreeMap::from([("max_concurrent_requests".into(), "10".into())]))
        );
        assert_eq!(a.text("notes"), Some("first\nsecond"));
    }

    /// Profiles across the two files: `profile` prefixes, the credentials file's keys over
    /// the config file's as a pair, `source_profile` followed, an SSO session's section.
    #[test]
    fn profiles_merge_both_files_and_follow_their_sources() {
        let dir = shards_testdir::TempDir::new("aws-profile").unwrap();
        let f = files(
            &dir,
            "[default]\nregion = us-west-2\n\
             [dev]\nregion = ignored\n\
             [profile dev]\nregion = eu-west-1\nrole_arn = arn:aws:iam::1:role/dev\nsource_profile = base\n\
             [profile base]\naws_access_key_id = config-id\naws_secret_access_key = config-secret\n\
             [profile sso]\nsso_session = corp\nsso_account_id = 1\nsso_role_name = r\n\
             [sso-session corp]\nsso_region = us-east-1\nsso_start_url = https://corp.awsapps.com/start\n\
             [profile half]\naws_access_key_id = only\n\
             [profile loop]\nrole_arn = arn:aws:iam::1:role/l\nsource_profile = loop\naws_access_key_id = l\naws_secret_access_key = s\n",
            "[base]\naws_access_key_id = file-id\naws_secret_access_key = file-secret\naws_session_token = t\n\
             [profile ignored]\naws_access_key_id = x\naws_secret_access_key = y\n",
        );
        let default = load(&f, "default").unwrap().unwrap();
        assert_eq!(default.region, "us-west-2");
        let dev = load(&f, "dev").unwrap().unwrap();
        assert_eq!(dev.region, "eu-west-1");
        let base = dev.source.unwrap();
        let keys = base.keys.unwrap();
        assert_eq!(
            (
                keys.access_key.as_str(),
                keys.token.as_deref().map(|t| t.as_str())
            ),
            ("file-id", Some("t"))
        );
        let sso = load(&f, "sso").unwrap().unwrap();
        assert_eq!(
            sso.sso_session.unwrap().start_url,
            "https://corp.awsapps.com/start"
        );
        assert!(load(&f, "half").is_err(), "a key without its secret");
        assert!(load(&f, "ignored").unwrap().is_none());
        // A profile its own source: its keys, then its role assumed.
        let looped = load(&f, "loop").unwrap().unwrap();
        assert_eq!(looped.role_arn, "arn:aws:iam::1:role/l");
        assert_eq!(looped.source.unwrap().keys.unwrap().access_key, "l");
    }
}
