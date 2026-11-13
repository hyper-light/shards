//! Codex's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 20; its plan
//! 01a1ce705d7076648c1d9f9ee26214d3, step 2): what its pin, its harness.json and its Agentfile say of one
//! another, of kiln's Kind `codex` and of sign-ins. Each check reads texts, so each test also hands it
//! changed copies, and each change alone must fail it.

use anyhow::{Context, anyhow, bail, ensure};
use serde_json::Value;

use crate::harness::kinds::{Kind, Protocol};

/// Where OpenAI's release workflow signs from, and the issuer of its certificates (pin.sh's WORKFLOW
/// and ISSUER).
const WORKFLOW: &str = "https://github.com/openai/codex/.github/workflows/rust-release.yml";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
/// kiln's platforms and OpenAI's package target for each.
const TARGETS: [(&str, &str); 2] = [("linux/amd64", "x86_64-unknown-linux-musl"), ("linux/arm64", "aarch64-unknown-linux-musl")];
/// What harness.json passes after kiln's own arguments: an image is pinned, so Codex checks for no update.
const NO_UPDATE_CHECK: [&str; 2] = ["-c", "check_for_update_on_startup=false"];

fn codex() -> anyhow::Result<&'static super::Builtin> {
    super::ALL.iter().find(|b| b.harness == "codex").ok_or_else(|| anyhow!("no built-in Agentfile runs codex"))
}

fn harness_json(b: &super::Builtin) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(name, _)| *name == "harness.json").map(|(_, text)| *text).ok_or_else(|| anyhow!("{} carries no harness.json", b.name))
}

/// What kiln starts for Kind `codex` before a session's own flags: its program, then `codex::args()`
/// (`Kind::launch`).
fn kind_command() -> anyhow::Result<Vec<String>> {
    let kind = Kind::get("codex").ok_or_else(|| anyhow!("no Kind codex"))?;
    ensure!(matches!(kind.protocol, Protocol::CodexAppServer), "Kind codex doesn't speak Codex's app-server");
    Ok(std::iter::once(kind.program.to_owned()).chain(crate::harness::codex::args()).collect())
}

fn x_y_z(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn lowercase_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path inside the directory it names a place in: `.` itself, or relative names with no `.` or `..`.
fn inside(path: &str) -> bool {
    path == "." || (!path.starts_with('/') && path.split('/').all(|c| !c.is_empty() && c != "." && c != ".."))
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn strings(v: &Value, pointer: &str) -> Vec<String> {
    v.pointer(pointer).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// One pinned file: an HTTPS URL, a SHA-256 in 64 lowercase hex digits, a size, a path inside the
/// harness directory, a mode of 0644 or 0755 where it has one, and a tar.gz to unpack: the whole tree at
/// its path, or one member (inside the archive, with its own SHA-256 and size) written at it.
fn check_file(platform: &str, file: &Value) -> anyhow::Result<()> {
    let path = str_at(file, "/path");
    let at = format!("{platform} {path}");
    ensure!(inside(path), "{at}: its path leaves the harness directory");
    ensure!(str_at(file, "/url").starts_with("https://"), "{at}: its URL isn't HTTPS");
    let sha = str_at(file, "/sha256");
    ensure!(lowercase_sha256(sha), "{at}: `{sha}` isn't a SHA-256 in 64 lowercase hex digits");
    ensure!(file.get("size").and_then(Value::as_u64).is_some_and(|s| s > 0), "{at}: no size");
    if let Some(mode) = file.get("mode") {
        ensure!(matches!(mode.as_str(), Some("0644" | "0755")), "{at}: mode {mode} isn't 0644 or 0755");
    }
    let unpack = file.get("unpack").ok_or_else(|| anyhow!("{at}: an archive with no unpack"))?;
    ensure!(str_at(unpack, "/format") == "tar.gz", "{at}: its unpack isn't tar.gz");
    match unpack.get("member").and_then(Value::as_str) {
        Some(member) => {
            ensure!(inside(member), "{at}: its member {member} leaves the archive");
            ensure!(lowercase_sha256(str_at(unpack, "/sha256")), "{at}: its member has no SHA-256");
            ensure!(unpack.get("size").and_then(Value::as_u64).is_some_and(|s| s > 0), "{at}: its member has no size");
            ensure!(file.get("mode").is_some(), "{at}: a member with no mode");
        }
        None => {
            ensure!(file.get("mode").is_none(), "{at}: a whole tree with a mode, whose entries keep their own");
            // The shared unpack holds a tree to its strip and the archive's own totals (step 25's module).
            ensure!(unpack.get("strip").and_then(Value::as_u64).is_some(), "{at}: a tree with no strip");
            for total in ["entries", "unpacked_bytes"] {
                ensure!(unpack.get(total).and_then(Value::as_u64).is_some_and(|n| n > 0), "{at}: a tree with no {total}");
            }
            for skipped in strings(unpack, "/skip") {
                ensure!(inside(&skipped) && skipped != ".", "{at}: it skips {skipped}, which leaves the tree");
            }
        }
    }
    Ok(())
}

/// The pin: Codex's version and tag, how pin.sh verified them, the base by digest, the version smoke,
/// and linux/amd64 and linux/arm64 once each, each OpenAI's whole package for its target at the
/// harness's root; on arm64, ripgrep's static rg of the version OpenAI pins in the package's place.
fn check_pin(pin: &str) -> anyhow::Result<Value> {
    let p: Value = serde_json::from_str(pin).context("pin.json doesn't parse")?;
    let version = str_at(&p, "/version");
    ensure!(x_y_z(version), "the pin's version `{version}` isn't x.y.z");
    let tag = format!("rust-v{version}");
    ensure!(str_at(&p, "/tag") == tag, "the pin's tag isn't {tag}");
    ensure!(str_at(&p, "/verified/identity") == format!("{WORKFLOW}@refs/tags/{tag}"), "the pin wasn't verified for OpenAI's release workflow at {tag}");
    ensure!(str_at(&p, "/verified/issuer") == ISSUER, "the pin wasn't verified for GitHub Actions' issuer");
    ensure!(lowercase_sha256(str_at(&p, "/verified/cosign/sha256")), "the pin names no cosign checksum");
    ensure!(str_at(&p, "/base").contains("@sha256:"), "the pin's base isn't pinned by digest");
    let smoke = serde_json::json!({"program": "bin/codex", "args": ["--version"], "stdout": format!("codex-cli {version}")});
    ensure!(p.get("smoke") == Some(&smoke), "the pin's smoke isn't {smoke}");
    let builds = p.get("builds").and_then(Value::as_array).ok_or_else(|| anyhow!("the pin has no builds"))?;
    let mut platforms: Vec<&str> = builds.iter().map(|b| str_at(b, "/platform")).collect();
    platforms.sort_unstable();
    ensure!(platforms == ["linux/amd64", "linux/arm64"], "the pin's builds are {platforms:?}, not linux/amd64 and linux/arm64 once each");
    for build in builds {
        let platform = str_at(build, "/platform");
        let target = TARGETS.iter().find(|(p, _)| *p == platform).map(|(_, t)| *t).unwrap_or_default();
        ensure!(build.get("variant").is_none(), "{platform}: Codex's builds have no variant");
        let files = build.get("files").and_then(Value::as_array).ok_or_else(|| anyhow!("{platform}: no files"))?;
        for file in files {
            check_file(platform, file)?;
        }
        let package = files.first().ok_or_else(|| anyhow!("{platform}: no package"))?;
        let url = format!("https://github.com/openai/codex/releases/download/{tag}/codex-package-{target}.tar.gz");
        ensure!(str_at(package, "/url") == url && str_at(package, "/path") == ".", "{platform}: its first file isn't {url} at the harness's root");
        ensure!(package.pointer("/unpack/member").is_none(), "{platform}: the package isn't taken whole");
        let skipped = strings(package, "/unpack/skip");
        match platform {
            "linux/amd64" => ensure!(files.len() == 1 && skipped.is_empty(), "linux/amd64: its package's own rg is static, and nothing takes its place"),
            _ => {
                let rgv = str_at(&p, "/verified/ripgrep/version");
                ensure!(x_y_z(rgv), "{platform}: the pin names no ripgrep version OpenAI's package pins");
                let rg = files.get(1).filter(|_| files.len() == 2).ok_or_else(|| anyhow!("{platform}: no rg in place of the package's"))?;
                let name = format!("ripgrep-{rgv}-aarch64-unknown-linux-musl");
                ensure!(skipped == ["codex-path/rg"], "{platform}: the package's codex-path/rg isn't skipped");
                ensure!(str_at(rg, "/path") == "codex-path/rg" && str_at(rg, "/mode") == "0755", "{platform}: ripgrep's rg isn't codex-path/rg, 0755");
                ensure!(str_at(rg, "/url") == format!("https://github.com/BurntSushi/ripgrep/releases/download/{rgv}/{name}.tar.gz"), "{platform}: its rg isn't ripgrep's {name}");
                ensure!(str_at(rg, "/unpack/member") == format!("{name}/rg"), "{platform}: its rg isn't {name}/rg");
            }
        }
    }
    Ok(p)
}

/// The Agentfile's instructions, each a keyword (upper-cased) and the rest of its line, without
/// blank lines and comments.
fn instructions(text: &str) -> Vec<(String, &str)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (keyword, rest) = line.split_once(' ').unwrap_or((line, ""));
            (keyword.to_ascii_uppercase(), rest.trim())
        })
        .collect()
}

/// Codex's harness-scoped volumes, as the Agentfile declares them.
fn harness_volumes(agentfile: &str) -> Vec<&str> {
    instructions(agentfile)
        .into_iter()
        .filter(|(keyword, _)| keyword == "VOLUME")
        .filter_map(|(_, rest)| rest.strip_prefix("--target-kind=harness ")?.strip_suffix(" FOR codex"))
        .collect()
}

/// The Agentfile, exactly: the pin's base, one stage, the harness from each platform's directory, the
/// bring-up's three provider grants, the project and CODEX_HOME volumes, and nothing that installs or
/// configures beside the pin and harness.json (no RUN, no ENV).
fn check_agentfile(agentfile: &str, pin: &Value) -> anyhow::Result<()> {
    let all = instructions(agentfile);
    let has = |keyword: &str, rest: &str| all.iter().any(|(k, r)| k == keyword && *r == rest);
    let count = |keyword: &str| all.iter().filter(|(k, _)| k == keyword).count();
    let base = str_at(pin, "/base");
    ensure!(has("ARG", &format!("BASE={base}")), "the Agentfile's ARG BASE isn't the pin's base {base}");
    ensure!(has("FROM", "${BASE}") && count("FROM") == 1, "the Agentfile isn't one stage FROM ${{BASE}}");
    ensure!(has("ARG", "TARGETARCH") && count("ARG") == 2, "the Agentfile's ARGs aren't BASE and TARGETARCH");
    ensure!(has("HARNESS", "codex FROM ./codex-${TARGETARCH}") && count("HARNESS") == 1, "the Agentfile's harness isn't codex FROM ./codex-${{TARGETARCH}}");
    let grants = [("NETWORK", "--dns --egress=443/tcp provider FOR codex"), ("EXPOSE", "--harnesses=codex 443/tcp AS egress FOR provider"), ("CONNECT", "codex WITH codex ON provider")];
    for (keyword, rest) in grants {
        ensure!(has(keyword, rest), "the Agentfile lacks `{keyword} {rest}`");
    }
    ensure!(count("NETWORK") + count("EXPOSE") + count("CONNECT") == grants.len(), "the Agentfile grants more than the bring-up's three lines");
    ensure!(count("VOLUME") == 2 && harness_volumes(agentfile) == ["/workspace", "/home/codex/.codex"], "the Agentfile's volumes aren't /workspace and /home/codex/.codex, Codex's alone");
    let known = ["ARG", "FROM", "HARNESS", "NETWORK", "EXPOSE", "CONNECT", "VOLUME"];
    if let Some((keyword, rest)) = all.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        bail!("the Agentfile has `{keyword} {rest}`: only the pin and harness.json install and configure Codex");
    }
    Ok(())
}

/// harness.json: Codex's (schema 1, the pin's version), starting what kiln starts for Kind `codex`
/// (`command`) with the update check off, and CODEX_HOME, alone in its env, in the Agentfile's volume
/// for it; no workdir, for kiln sends each thread its own.
fn check_harness(harness: &str, pin: &Value, agentfile: &str, command: &[String]) -> anyhow::Result<()> {
    let h: Value = serde_json::from_str(harness).context("harness.json doesn't parse")?;
    ensure!(h.get("schemaVersion").and_then(Value::as_u64) == Some(1), "harness.json isn't an OSI config of schema 1");
    ensure!(str_at(&h, "/name") == "codex", "harness.json isn't codex's");
    let version = str_at(pin, "/version");
    ensure!(str_at(&h, "/version") == version, "harness.json's version isn't the pin's {version}");
    let expected: Vec<String> = command.iter().cloned().chain(NO_UPDATE_CHECK.map(str::to_owned)).collect();
    let run = strings(&h, "/run/command");
    ensure!(run == expected, "harness.json runs {run:?}, not {expected:?}: kiln's command for Kind codex with the update check off");
    ensure!(h.pointer("/run/workdir").is_none(), "harness.json sets a workdir");
    let env = strings(&h, "/run/env");
    ensure!(env == ["CODEX_HOME=/home/codex/.codex"], "harness.json's env is {env:?}, not CODEX_HOME alone");
    ensure!(harness_volumes(agentfile).contains(&"/home/codex/.codex"), "CODEX_HOME isn't one of the Agentfile's harness volumes");
    Ok(())
}

/// What no embedded file may hold: OpenAI's key variables, a sign-in file's name, a key's `sk-`
/// prefix, or an ENV or ARG named like a credential.
fn check_secrets(name: &str, text: &str) -> anyhow::Result<()> {
    for word in ["OPENAI_API_KEY", "CODEX_API_KEY", "auth.json"] {
        ensure!(!text.contains(word), "{name} holds {word}");
    }
    // `sk-` where a word starts, as OpenAI's keys do (not `task-`, say).
    let key_start = text.match_indices("sk-").any(|(i, _)| text.get(..i).and_then(|before| before.chars().next_back()).is_none_or(|c| !c.is_ascii_alphanumeric()));
    ensure!(!key_start, "{name} holds something like an OpenAI key (sk-)");
    for (keyword, rest) in instructions(text) {
        if keyword == "ENV" || keyword == "ARG" {
            let var = rest.split(['=', ' ']).next().unwrap_or_default().to_ascii_lowercase();
            if let Some(word) = ["key", "token", "secret", "password", "credential"].into_iter().find(|w| var.contains(w)) {
                bail!("{name}: `{keyword} {rest}` names a {word}");
            }
        }
    }
    Ok(())
}

/// A copy of `pin` with one value changed.
fn changed(pin: &str, pointer: &str, value: Value) -> anyhow::Result<String> {
    let mut p: Value = serde_json::from_str(pin)?;
    *p.pointer_mut(pointer).ok_or_else(|| anyhow!("the pin has no {pointer}"))? = value;
    Ok(serde_json::to_string(&p)?)
}

#[test]
fn codex_pin_holds_both_platforms_with_arm64s_rg_in_place() -> anyhow::Result<()> {
    let b = codex()?;
    let pin = check_pin(b.pin)?;
    let digest = str_at(&pin, "/builds/0/files/0/sha256");
    let mut one_platform = pin.clone();
    one_platform.get_mut("builds").and_then(Value::as_array_mut).ok_or_else(|| anyhow!("no builds"))?.retain(|b| str_at(b, "/platform") != "linux/arm64");
    let mutants = [
        ("a 63-character digest", changed(b.pin, "/builds/0/files/0/sha256", digest.get(..63).unwrap_or_default().into())?),
        ("a missing platform", serde_json::to_string(&one_platform)?),
        ("a member path with ..", changed(b.pin, "/builds/1/files/1/unpack/member", "ripgrep/../rg".into())?),
        ("a file's path with ..", changed(b.pin, "/builds/1/files/1/path", "../codex-path/rg".into())?),
        ("the package's rg not skipped", changed(b.pin, "/builds/1/files/0/unpack/skip", serde_json::json!([]))?),
        ("a mode of 0777", changed(b.pin, "/builds/1/files/1/mode", "0777".into())?),
        ("a tree with no entries", changed(b.pin, "/builds/0/files/0/unpack/entries", Value::Null)?),
        ("a tree with no unpacked bytes", changed(b.pin, "/builds/1/files/0/unpack/unpacked_bytes", serde_json::json!(0))?),
        ("another smoke", changed(b.pin, "/smoke/stdout", "codex-cli 0.0.1".into())?),
        ("another tag's identity", changed(b.pin, "/verified/identity", format!("{WORKFLOW}@refs/tags/rust-v0.0.1").into())?),
    ];
    for (what, text) in mutants {
        ensure!(check_pin(&text).is_err(), "a pin with {what} passes");
    }
    Ok(())
}

#[test]
fn codex_harness_json_runs_kilns_command_with_codex_home_in_its_volume() -> anyhow::Result<()> {
    let b = codex()?;
    let pin = check_pin(b.pin)?;
    let harness = harness_json(b)?;
    let command = kind_command()?;
    check_harness(harness, &pin, b.text, &command)?;
    // kiln's arguments for Codex changed, and harness.json no longer runs what kiln starts.
    let other: Vec<String> = command.iter().cloned().chain(["--strict".to_owned()]).collect();
    ensure!(check_harness(harness, &pin, b.text, &other).is_err(), "harness.json passes against other arguments for Kind codex");
    let homeless = harness.replace("/home/codex/.codex", "/root/.codex");
    ensure!(check_harness(&homeless, &pin, b.text, &command).is_err(), "a CODEX_HOME outside the Agentfile's volume passes");
    let older = harness.replace(str_at(&pin, "/version"), "0.0.1");
    ensure!(check_harness(&older, &pin, b.text, &command).is_err(), "a harness.json of another version passes");
    Ok(())
}

#[test]
fn codex_agentfile_is_exactly_its_base_harness_grants_and_volumes() -> anyhow::Result<()> {
    let b = codex()?;
    let pin = check_pin(b.pin)?;
    check_agentfile(b.text, &pin)?;
    let mutants = [
        ("a RUN", format!("{}\nRUN true\n", b.text)),
        ("an ENV", format!("{}\nENV CODEX_HOME=/tmp\n", b.text)),
        ("another base", b.text.replace("@sha256:", "@sha256:0")),
        ("a fourth grant", format!("{}\nEXPOSE --harnesses=codex 80/tcp AS egress FOR provider\n", b.text)),
        ("no CODEX_HOME volume", b.text.replace("VOLUME --target-kind=harness /home/codex/.codex FOR codex", "")),
    ];
    for (what, text) in mutants {
        ensure!(check_agentfile(&text, &pin).is_err(), "an Agentfile with {what} passes");
    }
    Ok(())
}

#[test]
fn codex_embedded_files_hold_no_credential() -> anyhow::Result<()> {
    let b = codex()?;
    check_secrets("the Agentfile", b.text)?;
    check_secrets("pin.json", b.pin)?;
    for (name, text) in b.files {
        check_secrets(name, text)?;
    }
    let mutants = [
        ("ENV OPENAI_API_KEY=x", format!("{}\nENV OPENAI_API_KEY=x\n", b.text)),
        ("ARG CODEX_TOKEN", format!("{}\nARG CODEX_TOKEN\n", b.text)),
        ("an sk- key", format!("{}\n# sk-proj-0123456789\n", b.text)),
        ("auth.json", format!("{}\n# COPY auth.json /home/codex/.codex/\n", b.text)),
    ];
    for (what, text) in mutants {
        ensure!(check_secrets("the Agentfile", &text).is_err(), "an Agentfile with {what} passes");
    }
    Ok(())
}
