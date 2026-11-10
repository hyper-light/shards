//! Startup descriptors for supplied agents and harnesses. A root `agent.json` or
//! `harness.json` may say only `entrypoint` and optional `options`; the runtime receives
//! the existing OSI config. Full OSI descriptors remain readable too. There is no shell,
//! variable expansion, globbing or command substitution in a simple entrypoint.

use shards_image::osi::{self, Config, Run};

/// Read a supplied domain's descriptor and normalize it to the runtime's config.
pub fn config(bytes: &[u8], name: &str) -> Result<Config, String> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let object = value
        .as_object_mut()
        .ok_or("a startup descriptor must be a JSON object")?;
    if object.contains_key("schemaVersion") || object.contains_key("run") || object.contains_key("name") {
        object
            .entry("schemaVersion")
            .or_insert(osi::SCHEMA_VERSION.into());
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        return Config::parse(&bytes).map_err(|e| e.to_string());
    }
    for key in object.keys() {
        if key != "entrypoint" && key != "options" {
            return Err(format!(
                "unknown startup field {key:?}; expected entrypoint or options"
            ));
        }
    }
    let entrypoint = object
        .get("entrypoint")
        .and_then(serde_json::Value::as_str)
        .ok_or("entrypoint must be a nonempty command string")?;
    let mut command = words(entrypoint)?;
    if command.first().is_none_or(|s| s.is_empty()) {
        return Err("entrypoint must name a program".into());
    }
    if let Some(options) = object.get("options") {
        for option in options.as_array().ok_or("options must be an array of strings")? {
            command.push(
                option
                    .as_str()
                    .ok_or("options must be an array of strings")?
                    .to_string(),
            );
        }
    }
    if command.iter().any(|s| s.contains('\0')) {
        return Err("a startup argument contains a NUL".into());
    }
    if name.is_empty() {
        return Err("a simple startup descriptor needs an explicit -t name".into());
    }
    let config = Config {
        schema_version: osi::SCHEMA_VERSION,
        name: name.to_string(),
        version: String::new(),
        description: String::new(),
        platform: None,
        run: Some(Run {
            command,
            env: Vec::new(),
            workdir: None,
        }),
        skills: Vec::new(),
        mcp: Vec::new(),
        asks: osi::Asks::default(),
    };
    config.check().map_err(|e| e.to_string())?;
    Ok(config)
}

/// Quote and escape removal only: every resulting word is a literal argument. Unlike
/// the Dockerfile lexer, this must preserve dollars and empty quoted arguments.
fn words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => word.push(ch),
            (Some('"'), '\\') => {
                let next = chars.peek().copied().ok_or("entrypoint ends in an escape")?;
                if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                    let _ = chars.next();
                    if next != '\n' {
                        word.push(next);
                    }
                } else {
                    word.push(ch);
                }
            }
            (Some(_), _) => word.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, '\\') => {
                let next = chars.next().ok_or("entrypoint ends in an escape")?;
                if next != '\n' {
                    word.push(next);
                    started = true;
                }
            }
            (None, ch) if ch.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, _) => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("entrypoint has an unclosed quote".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_supplied_entrypoint_starts_literal_words_followed_by_options() {
        let c = config(
            br#"{"entrypoint":"nohup codex exec ","options":["--dangerously-bypass-approvals-and-sandbox"]}"#,
            "codex",
        )
        .unwrap();
        assert_eq!(
            c.run.unwrap().command,
            [
                "nohup",
                "codex",
                "exec",
                "--dangerously-bypass-approvals-and-sandbox"
            ]
        );
        let c = config(
            br#"{"entrypoint":"claude","options":["--dangerously-skip-permissions"]}"#,
            "claude",
        )
        .unwrap();
        assert_eq!(
            c.run.unwrap().command,
            ["claude", "--dangerously-skip-permissions"]
        );
        assert_eq!(
            config(br#"{"entrypoint":"codex"}"#, "codex")
                .unwrap()
                .run
                .unwrap()
                .command,
            ["codex"]
        );
    }

    #[test]
    fn quoting_does_not_expand_variables_globs_or_commands() {
        let descriptor = serde_json::json!({
            "entrypoint": r#"bin/codex 'a b' "$HOME" "quoted" '' one\ two `id` $(id) *.rs"#,
            "options": ["a b", "$HOME", ""]
        });
        let c = config(&serde_json::to_vec(&descriptor).unwrap(), "codex").unwrap();
        assert_eq!(
            c.run.unwrap().command,
            [
                "bin/codex",
                "a b",
                "$HOME",
                "quoted",
                "",
                "one two",
                "`id`",
                "$(id)",
                "*.rs",
                "a b",
                "$HOME",
                ""
            ]
        );
    }

    #[test]
    fn invalid_startup_descriptors_are_refused_instead_of_skipped() {
        for text in [
            r#"{}"#,
            r#"{"entrypoint":"   "}"#,
            r#"{"entrypoint":"''"}"#,
            r#"{"entrypoint":"codex 'oops"}"#,
            r#"{"entrypoint":"codex\\"}"#,
            r#"{"entrypoint":"codex","options":"--flag"}"#,
            r#"{"entrypoint":"codex","options":[1]}"#,
            r#"{"entrypoint":"codex","options":["\u0000"]}"#,
            r#"{"entrypoint":"codex","stdout":"/tmp/out"}"#,
        ] {
            assert!(config(text.as_bytes(), "codex").is_err(), "{text}");
        }
    }

    #[test]
    fn a_simple_descriptor_needs_a_declared_name() {
        assert!(
            config(br#"{"entrypoint":"codex"}"#, "")
                .unwrap_err()
                .contains("-t")
        );
    }

    #[test]
    fn an_existing_osi_descriptor_keeps_its_runtime_contract() {
        let c = config(br#"{"schemaVersion":1,"name":"old","run":{"command":["bin/start"],"env":["HOME=/state"],"workdir":"work"}}"#, "alias").unwrap();
        assert_eq!(c.name, "old");
        let run = c.run.unwrap();
        assert_eq!(run.command, ["bin/start"]);
        assert_eq!(run.env, ["HOME=/state"]);
        assert_eq!(run.workdir.as_deref(), Some("work"));
        assert!(config(br#"{"schemaVersion":99,"name":"old"}"#, "alias").is_err());
        assert!(
            config(
                br#"{"name":"old","run":{"command":["bin/start"],"workdir":"../outside"}}"#,
                "alias"
            )
            .is_err()
        );
    }
}
