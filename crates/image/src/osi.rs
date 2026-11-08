//! Open Sandbox Initiative artifacts (AGENTFILE_ARCH.md §8 Q1, §12.17; architecture.md
//! D54): an agent, harness or MCP server as an OCI 1.1 artifact of its own type, with a
//! config of its own and content layers rooted at its directory.
//!
//! The config says what the artifact is and how it runs, every path in it relative to
//! the directory it unpacks to and none leaving it. A reader refuses a schema version it
//! does not know, a name that is no stage-style name, and a path that is absolute or
//! climbs out.

use serde::{Deserialize, Serialize};

use crate::{Error, bad};

/// What kind of artifact: what `AGENT`, `HARNESS` and `MCP` take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Harness,
    Mcp,
}

impl Kind {
    /// The manifest's `artifactType`.
    pub fn artifact_type(self) -> &'static str {
        match self {
            Kind::Agent => "application/vnd.osi.agent.v1",
            Kind::Harness => "application/vnd.osi.harness.v1",
            Kind::Mcp => "application/vnd.osi.mcp.v1",
        }
    }

    /// Its config's media type.
    pub fn config_type(self) -> &'static str {
        match self {
            Kind::Agent => "application/vnd.osi.agent.config.v1+json",
            Kind::Harness => "application/vnd.osi.harness.config.v1+json",
            Kind::Mcp => "application/vnd.osi.mcp.config.v1+json",
        }
    }

    /// Its content layers' media type, uncompressed; `+gzip` and `+zstd` are read too.
    pub fn content_type(self) -> &'static str {
        match self {
            Kind::Agent => "application/vnd.osi.agent.content.v1.tar",
            Kind::Harness => "application/vnd.osi.harness.content.v1.tar",
            Kind::Mcp => "application/vnd.osi.mcp.content.v1.tar",
        }
    }

    /// The kind an `artifactType` names, if any.
    pub fn of(artifact_type: &str) -> Option<Kind> {
        [Kind::Agent, Kind::Harness, Kind::Mcp]
            .into_iter()
            .find(|k| k.artifact_type() == artifact_type)
    }

    pub fn word(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Harness => "harness",
            Kind::Mcp => "MCP server",
        }
    }
}

/// The schema version this reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// How it runs: its first process, relative to its directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
}

/// An MCP server it brings, spoken to over stdio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    pub name: String,
    pub command: Vec<String>,
}

/// What it needs that only an Agentfile grants: reported, never granted by itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<u32>,
    /// The most memory it may hold, in bytes, scratch included; under what the agents of
    /// a microVM may take together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
}

/// The platform it runs on; none for any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// An OSI artifact's config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<Mcp>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub asks: Asks,
}

fn is_default(a: &Asks) -> bool {
    *a == Asks::default()
}

/// A stage-style name: `^[a-z][a-z0-9-_.]*$`, as agents', harnesses' and MCP servers'
/// names are in an Agentfile.
fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// Whether `p` is relative and stays inside the directory it is relative to.
fn inside(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') || p.contains('\0') {
        return false;
    }
    let mut depth = 0i64;
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

impl Config {
    /// Reads a config and checks it as every reader must.
    pub fn parse(bytes: &[u8]) -> Result<Config, Error> {
        // The version first, so that a later schema's fields are refused for what they are.
        let v: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| Error(format!("an OSI config that is no JSON: {e}")))?;
        match v.get("schemaVersion").and_then(serde_json::Value::as_u64) {
            Some(n) if n == u64::from(SCHEMA_VERSION) => {}
            Some(n) => {
                return bad(format!(
                    "an OSI config of schema version {n}, which shards does not read (it reads {SCHEMA_VERSION})"
                ));
            }
            None => return bad("an OSI config without its schemaVersion"),
        }
        let c: Config = serde_json::from_value(v).map_err(|e| Error(format!("an OSI config: {e}")))?;
        c.check()?;
        Ok(c)
    }

    /// What every reader checks.
    pub fn check(&self) -> Result<(), Error> {
        if !valid_name(&self.name) {
            return bad(format!(
                "an OSI config's name {:?} is no name an Agentfile can give: lowercase letters, digits, '-', '_' and '.', a letter first",
                self.name
            ));
        }
        let mut paths: Vec<(&str, &str)> = Vec::new();
        if let Some(run) = &self.run {
            if run.command.is_empty() {
                return bad("an OSI config's run.command is empty");
            }
            if let Some(w) = &run.workdir {
                paths.push(("run.workdir", w));
            }
            for e in &run.env {
                if !e.contains('=') || e.starts_with('=') {
                    return bad(format!("an OSI config's run.env entry {e:?} is no NAME=value"));
                }
            }
        }
        for s in &self.skills {
            paths.push(("skills", s));
        }
        for m in &self.mcp {
            if !valid_name(&m.name) {
                return bad(format!(
                    "an OSI config's MCP server name {:?} is no name an Agentfile can give",
                    m.name
                ));
            }
            if m.command.is_empty() {
                return bad(format!("an OSI config's MCP server {} has no command", m.name));
            }
        }
        for (field, p) in paths {
            if !inside(p) {
                return bad(format!(
                    "an OSI config's {field} path {p:?} must be relative and stay inside its directory"
                ));
            }
        }
        Ok(())
    }

    /// Its bytes as written into the artifact: compact JSON, fields in their order.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(self).map_err(|e| Error(e.to_string()))
    }
}

/// An artifact's manifest: its type, config and content layers, as OCI 1.1's
/// own-config-and-layers case writes one.
pub fn manifest(
    kind: Kind,
    config: &crate::oci::Descriptor,
    layers: &[crate::oci::Descriptor],
) -> Result<Vec<u8>, Error> {
    let v = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": crate::oci::media::OCI_MANIFEST,
        "artifactType": kind.artifact_type(),
        "config": config,
        "layers": layers,
    });
    serde_json::to_vec(&v).map_err(|e| Error(e.to_string()))
}

/// Whether a content layer's media type is one this reads for `kind`, and how it is
/// compressed: `None` for none.
pub fn content_compression(kind: Kind, media_type: &str) -> Option<Option<&'static str>> {
    let base = kind.content_type();
    match media_type.strip_prefix(base) {
        Some("") => Some(None),
        Some("+gzip") => Some(Some("gzip")),
        Some("+zstd") => Some(Some("zstd")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_are_refused_where_a_reader_must_refuse_them() {
        let ok =
            br#"{"schemaVersion":1,"name":"main","run":{"command":["bin/agent"]},"skills":["skills/pdf"]}"#;
        assert_eq!(Config::parse(ok).unwrap().name, "main");
        for (bytes, why) in [
            (&br#"{"schemaVersion":2,"name":"main"}"#[..], "schema version 2"),
            (br#"{"name":"main"}"#, "without its schemaVersion"),
            (br#"{"schemaVersion":1,"name":"Main"}"#, "no name"),
            (
                br#"{"schemaVersion":1,"name":"main","skills":["../x"]}"#,
                "stay inside",
            ),
            (
                br#"{"schemaVersion":1,"name":"main","skills":["/x"]}"#,
                "stay inside",
            ),
            (
                br#"{"schemaVersion":1,"name":"main","run":{"command":[]}}"#,
                "empty",
            ),
            (br#"{"schemaVersion":1,"name":"main","extra":1}"#, "unknown field"),
        ] {
            let e = Config::parse(bytes).unwrap_err().to_string();
            assert!(e.contains(why), "{}: {e}", String::from_utf8_lossy(bytes));
        }
        assert!(inside("a/../b") && !inside("a/../../b"));
    }
}
