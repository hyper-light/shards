//! Claude Code's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19; plan
//! 01a1cf48e8157494ac9a51f7a564f526, step 2): what its pin, its two harness files and its Agentfile say of
//! one another, of kiln's Kind `claude`, and of credentials. Each check reads texts, so each test also
//! hands it changed copies, and each change must fail it.

use serde_json::Value;

use super::{ALL, Builtin};
use crate::harness::kinds::{Kind, Protocol};

/// Anthropic's release key, as code.claude.com/docs/en/setup publishes it: pin.sh takes a pin only
/// through it.
const ANTHROPIC_KEY: &str = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE";

fn claude() -> anyhow::Result<&'static Builtin> {
    ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs claude"))
}

/// The instructions of an Agentfile, comments and blank lines aside.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

/// The paths of the Agentfile's volumes for claude alone.
fn volumes(text: &str) -> Vec<&str> {
    instructions(text).into_iter().filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(" FOR claude")).collect()
}

/// One of Claude Code's harness files, as JSON.
fn harness_file(b: &Builtin, name: &str) -> anyhow::Result<Value> {
    let text = b.files.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no {name}", b.name))?;
    Ok(serde_json::from_str(text)?)
}

/// The strings of a harness file's list at `at` (`/run/command`, `/run/env`).
fn strings(h: &Value, at: &str) -> Vec<String> {
    h.pointer(at).and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

fn is_sha256(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What in Claude Code's pin would mislead a build or its smoke:
/// - a version not x.y.z, or a base not pinned by digest or not the one its Agentfile builds on;
/// - a pin not taken through Anthropic's key;
/// - builds other than linux/amd64 and linux/arm64, each in gnu and musl, or a build not of one file,
///   Anthropic's binary of that version from downloads.claude.ai at bin/claude, mode 0755, with its
///   SHA-256 and size and nothing to unpack;
/// - a smoke that isn't `bin/claude --version` printing `<version> (Claude Code)`, 2.1.311's bytes as
///   measured (evidence/e_step2_version_bytes.txt), `stdout` the line without its newline as every pin
///   writes it.
fn pin_problems(text: &str, agentfile: &str) -> Vec<String> {
    let pin: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return vec![format!("not JSON: {e}")],
    };
    let mut found = Vec::new();
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    if !(version.split('.').count() == 3 && version.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))) {
        found.push(format!("version `{version}` isn't x.y.z"));
    }
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
        found.push(format!("base `{base}` isn't pinned by digest"));
    }
    if instructions(agentfile).into_iter().find_map(|l| l.strip_prefix("ARG BASE=")) != Some(base) {
        found.push(format!("the Agentfile doesn't build on the pin's base `{base}`"));
    }
    if pin.pointer("/verified/signer/fingerprint").and_then(Value::as_str) != Some(ANTHROPIC_KEY) {
        found.push("the pin wasn't taken through Anthropic's key".to_owned());
    }
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen: Vec<String> = builds.iter().map(|b| format!("{} {}", b.get("platform").and_then(Value::as_str).unwrap_or_default(), b.get("variant").and_then(Value::as_str).unwrap_or_default())).collect();
    seen.sort();
    if seen != ["linux/amd64 gnu", "linux/amd64 musl", "linux/arm64 gnu", "linux/arm64 musl"] {
        found.push(format!("its builds are {seen:?}"));
    }
    let from = format!("https://downloads.claude.ai/claude-code-releases/{version}/");
    for b in &builds {
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        let [f] = files.as_slice() else {
            found.push(format!("a build of {} files", files.len()));
            continue;
        };
        let s = |k: &str| f.get(k).and_then(Value::as_str);
        if !s("url").is_some_and(|u| u.starts_with(&from) && u.ends_with("/claude")) {
            found.push(format!("{:?} isn't Anthropic's {version} binary", s("url")));
        }
        if !s("sha256").is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none_or(|n| n == 0) {
            found.push("a file without its SHA-256 and size".to_owned());
        }
        if s("path") != Some("bin/claude") || s("mode") != Some("0755") || f.get("unpack").is_some() {
            found.push(format!("a file at {:?}, mode {:?}: not the binary at bin/claude, 0755", s("path"), s("mode")));
        }
    }
    let smoke = |k: &str| pin.get("smoke").and_then(|s| s.get(k));
    if smoke("program").and_then(Value::as_str) != Some("bin/claude") {
        found.push("the smoke doesn't run bin/claude".to_owned());
    }
    if smoke("args") != Some(&Value::from(vec!["--version"])) || smoke("env").is_some() {
        found.push("the smoke isn't `--version` alone".to_owned());
    }
    let (line, want) = (smoke("stdout").and_then(Value::as_str).unwrap_or_default(), format!("{version} (Claude Code)"));
    if line != want {
        found.push(format!("the smoke's line {line:?} isn't Claude Code's {want:?}"));
    }
    found
}

/// How a harness file drifts from kiln's own launch of Claude Code. Its command is the kind's program
/// and claude::args for a session that asks nothing of its own (a session's own words come at launch).
/// Its environment is HOME on its volume, DISABLE_UPDATES (a pinned build never updates itself),
/// claude::FIXED_ENV and then `extra`, and nothing else.
fn launch_problems(h: &Value, kind: &Kind, home: &str, extra: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let own = crate::harness::session::Spec { kind: kind.id.to_owned(), allow_bypass: false, ..Default::default() };
    let want: Vec<String> = std::iter::once(kind.program.to_owned()).chain(crate::harness::claude::args(&own, None)).collect();
    if strings(h, "/run/command") != want {
        found.push(format!("command {:?} isn't kiln's {want:?}", strings(h, "/run/command")));
    }
    let mut env = vec![format!("HOME={home}"), "DISABLE_UPDATES=1".to_owned()];
    env.extend(crate::harness::claude::FIXED_ENV.iter().map(|(k, v)| format!("{k}={v}")));
    env.extend(extra.iter().map(|e| (*e).to_owned()));
    if strings(h, "/run/env") != env {
        found.push(format!("env {:?} isn't {env:?}", strings(h, "/run/env")));
    }
    found
}

/// What in `text` holds or names a credential: an ENV or ARG named like one, a variable named like one
/// given a value, Claude Code's own (ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN, apiKeyHelper, its
/// .credentials.json) or a provider's key (`sk-…`). A sign-in belongs on its owner's HOME volume alone.
fn credentials_in(text: &str) -> Vec<String> {
    const WORDS: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    let named = |name: &str| WORDS.iter().any(|w| name.to_ascii_uppercase().contains(w));
    let mut found = Vec::new();
    for l in instructions(text) {
        let Some(rest) = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG ")) else { continue };
        for name in rest.split_whitespace().map(|w| w.split('=').next().unwrap_or(w)) {
            if named(name) {
                found.push(format!("{l}: names {name}"));
            }
        }
    }
    for word in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "apiKeyHelper", ".credentials.json", "sk-ant-"] {
        if text.contains(word) {
            found.push(format!("names {word}"));
        }
    }
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text.get(s..i).unwrap_or_default();
                if c == '=' && named(word) {
                    found.push(format!("{word}= gives a value"));
                }
                start = None;
            }
            _ => {}
        }
    }
    found
}

/// Claude Code's pin holds (both platforms in both libcs, Anthropic's binaries through Anthropic's key,
/// the base its Agentfile builds on, the smoke Claude Code's), and each harness file is an OSI config of
/// claude at the pin's version.
#[test]
fn step19_claude_pin_and_harness_files_hold() -> anyhow::Result<()> {
    let b = claude()?;
    let problems = pin_problems(b.pin, b.text);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let version = serde_json::from_str::<Value>(b.pin)?.get("version").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
    for name in ["harness.json", "harness.musl.json"] {
        let h = harness_file(b, name)?;
        anyhow::ensure!(h.get("schemaVersion") == Some(&Value::from(1)) && h.get("name").and_then(Value::as_str) == Some("claude"), "{name}: {h}");
        anyhow::ensure!(h.get("version").and_then(Value::as_str) == Some(version.as_str()), "{name} isn't the pin's {version}");
        anyhow::ensure!(!strings(&h, "/run/command").is_empty(), "{name}: no run.command");
    }
    Ok(())
}

/// A pin no build or smoke could trust is caught: a digest one digit short, a variant missing, another
/// key, a base by tag or not the Agentfile's, a file over http or at another path or mode, and a smoke
/// that runs another program, asks something else or expects another line.
#[test]
fn step19_a_claude_pin_that_would_mislead_is_caught() -> anyhow::Result<()> {
    let b = claude()?;
    let real: Value = serde_json::from_str(b.pin)?;
    let edited = |path: &str, to: Value| -> anyhow::Result<String> {
        let mut v = real.clone();
        let at = v.pointer_mut(path).ok_or_else(|| anyhow::anyhow!("{path}"))?;
        *at = to;
        Ok(v.to_string())
    };
    let short = real.pointer("/builds/0/files/0/sha256").and_then(Value::as_str).and_then(|d| d.get(1..)).map(Value::from).unwrap_or_default();
    let url = real.pointer("/builds/0/files/0/url").and_then(Value::as_str).unwrap_or_default().replacen("https://", "http://", 1);
    let says = |text: String, what: &str| pin_problems(&text, b.text).iter().any(|p| p.contains(what));
    anyhow::ensure!(says(edited("/builds/0/files/0/sha256", short)?, "without its SHA-256"));
    anyhow::ensure!(says(edited("/builds/1/variant", Value::from("gnu"))?, "its builds are"));
    anyhow::ensure!(says(edited("/verified/signer/fingerprint", Value::from("0000000000000000000000000000000000000000"))?, "Anthropic's key"));
    anyhow::ensure!(says(edited("/base", Value::from("buildpack-deps:trixie-scm"))?, "pinned by digest"));
    anyhow::ensure!(says(edited("/base", Value::from("debian@sha256:0000000000000000000000000000000000000000000000000000000000000000"))?, "doesn't build on"));
    anyhow::ensure!(says(edited("/builds/0/files/0/url", Value::from(url))?, "isn't Anthropic's"));
    anyhow::ensure!(says(edited("/builds/0/files/0/path", Value::from("claude"))?, "not the binary"));
    anyhow::ensure!(says(edited("/builds/0/files/0/mode", Value::from("0644"))?, "not the binary"));
    anyhow::ensure!(says(edited("/smoke/program", Value::from("claude"))?, "doesn't run bin/claude"));
    anyhow::ensure!(says(edited("/smoke/args", Value::from(vec!["-v", "--json"]))?, "`--version` alone"));
    let version = real.get("version").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(says(edited("/smoke/stdout", Value::from(format!("Claude Code {version}")))?, "smoke's line"));
    anyhow::ensure!(says(edited("/smoke/stdout", Value::from(version))?, "smoke's line"));
    anyhow::ensure!(pin_problems(b.pin, b.text).is_empty());
    Ok(())
}

/// Claude Code runs as kiln's Claude kind starts it, with its HOME on the person's volume; musl's file
/// also takes the system's ripgrep. A dropped argument, a HOME left on the scratch and a stray variable
/// are each caught.
#[test]
fn step19_claude_runs_as_kilns_claude_kind_with_its_home_on_its_volume() -> anyhow::Result<()> {
    let b = claude()?;
    let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
    anyhow::ensure!(matches!(kind.protocol, Protocol::ClaudeStreamJson), "kiln's claude kind isn't stream-json");
    let vs = volumes(b.text);
    anyhow::ensure!(vs == ["/workspace", "/home/claude"], "{vs:?}");
    let home = vs.get(1).copied().unwrap_or_default();
    let variants: [(&str, &[&str]); 2] = [("harness.json", &[]), ("harness.musl.json", &["USE_BUILTIN_RIPGREP=0"])];
    for (name, extra) in variants {
        let problems = launch_problems(&harness_file(b, name)?, kind, home, extra);
        anyhow::ensure!(problems.is_empty(), "{name}: {problems:?}");
    }
    let h = harness_file(b, "harness.json")?;
    let mut dropped = h.clone();
    if let Some(c) = dropped.pointer_mut("/run/command").and_then(Value::as_array_mut) {
        c.retain(|a| a.as_str() != Some("--verbose"));
    }
    anyhow::ensure!(!launch_problems(&dropped, kind, home, &[]).is_empty(), "a dropped argument passed");
    let mut scratch = h.clone();
    if let Some(e) = scratch.pointer_mut("/run/env/0") {
        *e = Value::from("HOME=/tmp");
    }
    anyhow::ensure!(!launch_problems(&scratch, kind, home, &[]).is_empty(), "HOME on the scratch passed");
    anyhow::ensure!(!launch_problems(&h, kind, home, &["USE_BUILTIN_RIPGREP=0"]).is_empty(), "a stray variable passed");
    Ok(())
}

/// Claude Code's Agentfile is the plan's shape: the base by digest, a stage for each libc with a RUN only
/// in musl's (Anthropic's packages), Anthropic's own build per platform and libc, exactly the bring-up's
/// three grants, exactly its two volumes, and no ENV.
#[test]
fn step19_claude_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
    let lines = instructions(claude()?.text);
    let want = [
        "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
        "ARG CLAUDE_LIBC=gnu",
        "FROM ${BASE} AS base-gnu",
        "FROM ${BASE} AS base-musl",
        "RUN apk add --no-cache libgcc libstdc++ ripgrep",
        "FROM base-${CLAUDE_LIBC}",
        "ARG TARGETARCH",
        "ARG CLAUDE_LIBC",
        "HARNESS claude FROM ./claude-${TARGETARCH}-${CLAUDE_LIBC}",
        "NETWORK --dns --egress=443/tcp provider FOR claude",
        "EXPOSE --harnesses=claude 443/tcp AS egress FOR provider",
        "CONNECT claude WITH claude ON provider",
        "VOLUME --target-kind=harness /workspace FOR claude",
        "VOLUME --target-kind=harness /home/claude FOR claude",
    ];
    anyhow::ensure!(lines == want, "{lines:#?}");
    Ok(())
}

/// Nothing kiln ships for Claude Code holds a credential or names one; adding one is caught.
#[test]
fn step19_no_claude_file_holds_a_credential() -> anyhow::Result<()> {
    let b = claude()?;
    for (name, text) in [("Agentfile", b.text), ("pin.json", b.pin)].into_iter().chain(b.files.iter().copied()) {
        let found = credentials_in(text);
        anyhow::ensure!(found.is_empty(), "{name}: {found:?}");
    }
    anyhow::ensure!(!credentials_in(&format!("{}ENV ANTHROPIC_API_KEY=x\n", b.text)).is_empty());
    anyhow::ensure!(!credentials_in(&format!("{}ARG GITHUB_TOKEN\n", b.text)).is_empty());
    anyhow::ensure!(!credentials_in(r#"{"run":{"env":["CLAUDE_CODE_OAUTH_TOKEN=y"]}}"#).is_empty());
    anyhow::ensure!(!credentials_in(r#"{"apiKeyHelper":"cat /run/key"}"#).is_empty());
    Ok(())
}
