//! A supplied agent's or harness's startup descriptor, checked after its source is
//! unpacked. Native builds and the BuildKit frontend read the same tree interface and
//! produce the same normalized OSI runtime config beside the domain's content.

use shards_agentfile::llb::{OpAction, OpActionKind};
use shards_agentfile::platform::Platform;
use shards_build::data::Sources;
use shards_image::osi;

use super::{
    exec::Ref,
    skills::{self, Fetched, Found},
};

fn descriptor(kind: &[u8]) -> Result<&'static str, String> {
    match kind {
        b"agent" => Ok(osi::Kind::Agent.config_file()),
        b"harness" => Ok(osi::Kind::Harness.config_file()),
        _ => Err(format!(
            "unknown supplied domain kind {}",
            String::from_utf8_lossy(kind)
        )),
    }
}

/// The source's descriptor, without following links, normalized to `osi.json`.
pub fn layout(
    input: &Ref,
    kind: &[u8],
    name: &[u8],
    platform: Option<&Platform>,
    sources: &mut Sources,
) -> Result<Vec<OpAction>, String> {
    let path = descriptor(kind)?;
    actions(
        skills::lookup(input, path.as_bytes(), sources)?,
        path,
        name,
        platform,
    )
}

/// The same check over a tree read through BuildKit's gateway.
pub fn layout_of(
    tree: &mut dyn Fetched,
    kind: &[u8],
    name: &[u8],
    platform: Option<&Platform>,
) -> Result<Vec<OpAction>, String> {
    let path = descriptor(kind)?;
    actions(tree.lookup(path.as_bytes())?, path, name, platform)
}

fn actions(
    found: Found,
    path: &str,
    name: &[u8],
    platform: Option<&Platform>,
) -> Result<Vec<OpAction>, String> {
    let bytes = match found {
        Found::Missing => None,
        Found::Other => {
            return Err(format!(
                "{path}: a startup descriptor must be a regular file, not a link or another kind of entry"
            ));
        }
        Found::File(bytes) => {
            let name = std::str::from_utf8(name).map_err(|e| e.to_string())?;
            let config =
                shards_agentfile::startup::config(&bytes, name).map_err(|e| format!("{path}: {e}"))?;
            if let (Some(said), Some(target)) = (&config.platform, platform)
                && (said.os.as_bytes() != target.os
                    || said.architecture.as_bytes() != target.architecture
                    || said
                        .variant
                        .as_ref()
                        .is_some_and(|v| v.as_bytes() != target.variant))
            {
                return Err(format!(
                    "{path}: its platform {}/{} does not match this build's {}/{}",
                    said.os,
                    said.architecture,
                    String::from_utf8_lossy(&target.os),
                    String::from_utf8_lossy(&target.architecture)
                ));
            }
            Some(config.to_bytes().map_err(|e| e.to_string())?)
        }
    };
    // BuildKit file actions: create the output root on scratch, then its regular config.
    // Modes are the directory and file modes COPY uses for generated content.
    let mut out = vec![OpAction {
        input: -1,
        secondary_input: -1,
        output: if bytes.is_some() { -1 } else { 0 },
        action: OpActionKind::Mkdir {
            path: b"/".to_vec(),
            mode: 0o755,
            make_parents: true,
            owner: None,
            timestamp: -1,
        },
    }];
    if let Some(bytes) = bytes {
        out.push(OpAction {
            input: 1,
            secondary_input: -1,
            output: 0,
            action: OpActionKind::Mkfile {
                path: b"/osi.json".to_vec(),
                mode: 0o644,
                data: bytes,
                owner: None,
                timestamp: -1,
            },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn read(bytes: &[u8]) -> osi::Config {
        let actions = actions(
            Found::File(bytes.to_vec()),
            "harness.json",
            b"codex",
            Some(&Platform::new("linux", "amd64")),
        )
        .unwrap();
        let OpActionKind::Mkfile { path, data, .. } = &actions.last().unwrap().action else {
            panic!("{actions:?}")
        };
        assert_eq!(path, b"/osi.json");
        osi::Config::parse(data).unwrap()
    }

    #[test]
    fn a_supplied_harness_gets_a_checked_runtime_config() {
        let c = read(
            br#"{"entrypoint":"nohup codex exec ","options":["--dangerously-bypass-approvals-and-sandbox"]}"#,
        );
        assert_eq!(c.name, "codex");
        assert_eq!(
            c.run.unwrap().command,
            [
                "nohup",
                "codex",
                "exec",
                "--dangerously-bypass-approvals-and-sandbox"
            ]
        );
    }

    #[test]
    fn a_plain_source_stays_passive_but_a_bad_descriptor_fails() {
        let actions = actions(Found::Missing, "harness.json", b"codex", None).unwrap();
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a.action, OpActionKind::Mkfile { .. }))
        );
        assert!(
            super::actions(Found::Other, "harness.json", b"codex", None)
                .unwrap_err()
                .contains("regular file")
        );
        assert!(
            super::actions(
                Found::File(br#"{"entrypoint":3}"#.to_vec()),
                "harness.json",
                b"codex",
                None
            )
            .unwrap_err()
            .contains("entrypoint")
        );
    }

    #[test]
    fn a_legacy_descriptor_is_checked_for_schema_paths_and_platform() {
        assert!(
            super::actions(
                Found::File(br#"{"name":"old","run":{"command":["bin/start"],"workdir":"../out"}}"#.to_vec()),
                "harness.json",
                b"codex",
                None
            )
            .unwrap_err()
            .contains("relative")
        );
        let wrong = br#"{"schemaVersion":1,"name":"old","platform":{"os":"linux","architecture":"arm64"},"run":{"command":["bin/start"]}}"#;
        assert!(
            super::actions(
                Found::File(wrong.to_vec()),
                "harness.json",
                b"codex",
                Some(&Platform::new("linux", "amd64"))
            )
            .unwrap_err()
            .contains("does not match")
        );
        assert_eq!(
            read(br#"{"name":"old","run":{"command":["bin/start"]}}"#).name,
            "old"
        );
    }
}
