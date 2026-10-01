//! The parser and lexer against BuildKit's own (scripts/dockerfile/generate): every file
//! of testdata's corpus and BuildKit's parser test files, every case of BuildKit's lexer
//! tables and of testdata/lex-cases.json, field by field, byte for byte. Where
//! testdata/deviations.json names a case, its fields are what shards gives instead, by
//! design, each with its reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use serde_json::Value;
use shards_dockerfile::go::quote;
use shards_dockerfile::lex::{EnvList, Lex};
use shards_dockerfile::parser;

fn testdata() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata"))
}

fn load(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(testdata().join(name)).unwrap()).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// The deviations for `kind`, each its case's identity and the fields it replaces.
fn deviations(kind: &str) -> Vec<Value> {
    load("deviations.json")
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["kind"] == kind)
        .inspect(|d| {
            assert!(
                d["reason"].as_str().is_some_and(|r| !r.is_empty()),
                "{d}: no reason"
            )
        })
        .cloned()
        .collect()
}

/// What a parse gave, in the oracle's terms.
fn parsed(text: &[u8]) -> Value {
    match parser::parse(text) {
        Err(e) => serde_json::json!({
            "error": quote(&e.message),
            "location": e.location.iter().map(|r| r.iter().map(|&(a, b)| [a, b]).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Ok(p) => {
            let children: Vec<Value> = p
                .instructions
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "dump": quote(&n.dump()),
                        "original": quote(&n.original),
                        "flags": n.flags.iter().map(|f| quote(f)).collect::<Vec<_>>(),
                        "attributes": if n.json { vec!["json"] } else { vec![] },
                        "start_line": n.start_line,
                        "end_line": n.end_line,
                        "prev_comment": n.prev_comment.iter().map(|c| quote(c)).collect::<Vec<_>>(),
                        "heredocs": n.heredocs.iter().map(|h| serde_json::json!({
                            "name": quote(&h.name), "fd": h.file_descriptor, "expand": h.expand,
                            "chomp": h.chomp, "content": quote(&h.content),
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            serde_json::json!({
                "escape": quote(&[p.escape]),
                "dump": quote(&parser::dump(&p.instructions)),
                "children": children,
                "warnings": p.warnings.iter().map(|w| {
                    let mut t = w.short.clone();
                    t.extend_from_slice(format!("|{}|{}", w.url, w.line).as_bytes());
                    quote(&t)
                }).collect::<Vec<_>>(),
            })
        }
    }
}

/// The oracle's record in the same shape: absent lists as empty.
fn expected(e: &Value) -> Value {
    if let Some(err) = s(&e["error"]) {
        return serde_json::json!({ "error": err, "location": e["location"] });
    }
    let children: Vec<Value> = e["children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "dump": c["dump"], "original": c["original"], "flags": strs(&c["flags"]),
                "attributes": strs(&c["attributes"]), "start_line": c["start_line"], "end_line": c["end_line"],
                "prev_comment": strs(&c["prev_comment"]),
                "heredocs": c["heredocs"].as_array().cloned().unwrap_or_default(),
            })
        })
        .collect();
    serde_json::json!({
        "escape": e["escape"], "dump": e["dump"], "children": children, "warnings": strs(&e["warnings"]),
    })
}

#[test]
fn files_parse_as_buildkit_parses_them() {
    let devs = deviations("parse");
    let mut failures = Vec::new();
    let mut used = 0;
    let cases = load("parse.json");
    for e in cases.as_array().unwrap() {
        let file = e["file"].as_str().unwrap();
        let text = std::fs::read(testdata().join(file)).unwrap();
        let ours = parsed(&text);
        let mut want = expected(e);
        if let Some(d) = devs.iter().find(|d| d["file"] == file) {
            used += 1;
            want = d["ours"].clone();
        }
        if ours != want {
            failures.push(format!(
                "{file}:\n  ours: {}\n  want: {}",
                serde_json::to_string(&ours).unwrap(),
                serde_json::to_string(&want).unwrap()
            ));
        }
    }
    assert_eq!(used, devs.len(), "a deviation names no case");
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        cases.as_array().unwrap().len(),
        failures.join("\n")
    );
}

fn lexed(c: &Value) -> Value {
    let mut lex = Lex::new(
        c["escape"]
            .as_str()
            .unwrap()
            .chars()
            .next()
            .map(u32::from)
            .unwrap(),
    );
    lex.raw_quotes = c["raw_quotes"].as_bool().unwrap();
    lex.raw_escapes = c["raw_escapes"].as_bool().unwrap();
    lex.skip_unset_env = c["skip_unset"].as_bool().unwrap();
    lex.skip_process_quotes = c["skip_quotes"].as_bool().unwrap();
    let env: Vec<String> = strs(&c["env"]);
    let env = EnvList::from_entries(env.iter().map(String::as_bytes));
    let input = c["input"].as_str().unwrap().as_bytes();
    match lex.process(input, &env) {
        Ok(p) => serde_json::json!({
            "word": quote(&p.word),
            "matched": p.matched.iter().map(|m| quote(m)).collect::<Vec<_>>(),
            "unmatched": p.unmatched.iter().map(|m| quote(m)).collect::<Vec<_>>(),
            "words": p.words.iter().map(|w| quote(w)).collect::<Vec<_>>(),
        }),
        Err(e) => serde_json::json!({ "word_error": quote(&e.0), "words_error": quote(&e.0) }),
    }
}

#[test]
fn words_process_as_buildkits_lexer_processes_them() {
    let devs = deviations("lex");
    let mut failures = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    let cases = load("lex.json");
    let identity = |c: &Value| {
        serde_json::json!([
            c["escape"],
            c["raw_quotes"],
            c["raw_escapes"],
            c["skip_unset"],
            c["skip_quotes"],
            c["env"],
            c["input"]
        ])
    };
    for c in cases.as_array().unwrap() {
        let ours = lexed(c);
        let mut want = if s(&c["word_error"]).is_some() {
            serde_json::json!({ "word_error": c["word_error"], "words_error": c["words_error"] })
        } else {
            serde_json::json!({
                "word": c["word"], "matched": strs(&c["matched"]), "unmatched": strs(&c["unmatched"]),
                "words": strs(&c["words"]),
            })
        };
        if let Some((i, d)) = devs.iter().enumerate().find(|(_, d)| identity(d) == identity(c)) {
            used.insert(i);
            for (k, v) in d["ours"].as_object().unwrap() {
                want[k] = v.clone();
            }
        }
        if ours != want {
            failures.push(format!(
                "{}:\n  ours: {}\n  want: {}",
                serde_json::to_string(&identity(c)).unwrap(),
                serde_json::to_string(&ours).unwrap(),
                serde_json::to_string(&want).unwrap()
            ));
        }
    }
    assert_eq!(used.len(), devs.len(), "a deviation names no case");
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        cases.as_array().unwrap().len(),
        failures.join("\n")
    );
}

fn qv(b: &[u8]) -> Value {
    Value::String(quote(b))
}

fn qvs(v: &[Vec<u8>]) -> Value {
    Value::Array(v.iter().map(|b| qv(b)).collect())
}

fn ranges_v(r: &[(usize, usize)]) -> Value {
    serde_json::json!(r.iter().map(|&(a, b)| [a, b]).collect::<Vec<_>>())
}

fn kvs(kv: &[shards_dockerfile::instructions::KeyValue]) -> Value {
    Value::Array(
        kv.iter()
            .map(|p| serde_json::json!([qv(&p.key), qv(&p.value), p.no_delim]))
            .collect(),
    )
}

fn shell_v(c: &shards_dockerfile::instructions::CmdLine) -> Value {
    let files: Vec<Value> = c
        .files
        .iter()
        .map(|f| serde_json::json!([qv(&f.name), qv(&f.data), f.chomp]))
        .collect();
    serde_json::json!({
        "cmd_line": qvs(&c.cmd_line), "cmd_line_nil": c.cmd_line_nil,
        "files": if files.is_empty() { Value::Null } else { Value::Array(files) },
        "prepend_shell": c.prepend_shell,
    })
}

fn sources_v(s: &shards_dockerfile::instructions::Sources) -> Value {
    let contents: Vec<Value> = s
        .contents
        .iter()
        .map(|c| serde_json::json!([qv(&c.path), qv(&c.data), c.expand]))
        .collect();
    serde_json::json!({
        "dest": qv(&s.dest), "paths": qvs(&s.paths),
        "contents": if contents.is_empty() { Value::Null } else { Value::Array(contents) },
    })
}

/// A command in the oracle's terms.
fn command_v(c: &shards_dockerfile::instructions::Command) -> Value {
    use shards_dockerfile::instructions::Kind;
    let opt = |b: &Option<Vec<u8>>| b.as_ref().map_or(Value::Null, |v| qv(v));
    let (kind, fields): (&str, Value) = match &c.kind {
        Kind::Env(e) => ("EnvCommand", serde_json::json!({ "env": kvs(e) })),
        Kind::Maintainer(m) => ("MaintainerCommand", serde_json::json!({ "maintainer": qv(m) })),
        Kind::Label(l) => ("LabelCommand", serde_json::json!({ "labels": kvs(l) })),
        Kind::Add(a) => (
            "AddCommand",
            serde_json::json!({
                "sources": sources_v(&a.sources), "chown": qv(&a.chown), "chmod": qv(&a.chmod), "link": a.link,
                "exclude": qvs(&a.exclude), "keep_git_dir": a.keep_git_dir, "checksum": qv(&a.checksum), "unpack": a.unpack,
            }),
        ),
        Kind::Copy(a) => (
            "CopyCommand",
            serde_json::json!({
                "sources": sources_v(&a.sources), "from": qv(&a.from), "chown": qv(&a.chown), "chmod": qv(&a.chmod),
                "link": a.link, "exclude": qvs(&a.exclude), "parents": a.parents,
            }),
        ),
        Kind::Onbuild(e) => ("OnbuildCommand", serde_json::json!({ "expression": qv(e) })),
        Kind::Workdir(p) => ("WorkdirCommand", serde_json::json!({ "path": qv(p) })),
        Kind::Run(r) => {
            let mounts: Vec<Value> = r.mounts.iter().map(|m| serde_json::json!({
                "type": qv(&m.kind), "from": qv(&m.from), "source": qv(&m.source), "target": qv(&m.target),
                "read_only": m.read_only, "size": m.size, "id": qv(&m.id), "sharing": qv(&m.sharing),
                "required": m.required, "env": opt(&m.env), "mode": m.mode, "uid": m.uid, "gid": m.gid,
            })).collect();
            let devices: Vec<Value> = r
                .devices
                .iter()
                .map(|d| serde_json::json!([qv(&d.name), d.required]))
                .collect();
            (
                "RunCommand",
                serde_json::json!({
                    "shell": shell_v(&r.cmd), "flags_used": qvs(&r.flags_used),
                    "mounts": if mounts.is_empty() { Value::Null } else { Value::Array(mounts) },
                    "network": qv(&r.network), "security": qv(&r.security),
                    "devices": if devices.is_empty() { Value::Null } else { Value::Array(devices) },
                }),
            )
        }
        Kind::Cmd(s) => ("CmdCommand", serde_json::json!({ "shell": shell_v(s) })),
        Kind::Entrypoint(s) => ("EntrypointCommand", serde_json::json!({ "shell": shell_v(s) })),
        Kind::Healthcheck(h) => (
            "HealthCheckCommand",
            serde_json::json!({ "health": {
                "test": qvs(&h.test), "interval": h.interval, "timeout": h.timeout, "start_period": h.start_period,
                "start_interval": h.start_interval, "retries": h.retries,
            }}),
        ),
        Kind::Expose(p) => ("ExposeCommand", serde_json::json!({ "ports": qvs(p) })),
        Kind::User(u) => ("UserCommand", serde_json::json!({ "user": qv(u) })),
        Kind::Volume(v) => ("VolumeCommand", serde_json::json!({ "volumes": qvs(v) })),
        Kind::StopSignal(s) => ("StopSignalCommand", serde_json::json!({ "signal": qv(s) })),
        Kind::Arg(a) => (
            "ArgCommand",
            serde_json::json!({ "args": a.iter().map(|d| serde_json::json!([qv(&d.key), opt(&d.value), qv(&d.doc_comment)])).collect::<Vec<_>>() }),
        ),
        Kind::Shell(w) => ("ShellCommand", serde_json::json!({ "shell_words": qvs(w) })),
    };
    let mut out = serde_json::json!({
        "kind": format!("*instructions.{kind}"), "name": qv(&c.name), "code": qv(&c.code),
        "location": ranges_v(&c.location), "comments": qvs(&c.comments),
    });
    for (k, v) in fields.as_object().unwrap() {
        out[k] = v.clone();
    }
    out
}

/// Nulls and empty lists as one, as Go's nil and empty slices are alike to a reader.
fn normalize(v: &Value) -> Value {
    match v {
        Value::Array(a) if a.is_empty() => Value::Null,
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), normalize(v))).collect()),
        other => other.clone(),
    }
}

fn instructions_of(text: &[u8]) -> Value {
    use shards_dockerfile::{instructions, lint};
    let Ok(parsed) = parser::parse(text) else {
        return serde_json::json!({ "parse_error": true });
    };
    let check = shards_dockerfile::parser::directive_value(text, b"check").unwrap_or_default();
    let config = match lint::parse_options(&check) {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({ "error": quote(&[b"failed to parse check options: ".as_slice(), &e].concat()) });
        }
    };
    let linter = lint::Linter::new(config);
    let result = instructions::parse(&parsed, &linter);
    let warnings: Vec<Value> = linter
        .warnings()
        .iter()
        .map(|w| {
            let lines: Vec<String> = w.location.iter().map(|(a, b)| format!("{a}-{b}")).collect();
            let mut t = format!("{}|{}|{}|", w.rule, w.description, w.url).into_bytes();
            t.extend_from_slice(&w.message);
            t.extend_from_slice(format!("|{}", lines.join(",")).as_bytes());
            qv(&t)
        })
        .collect();
    match result {
        Err(e) => serde_json::json!({
            "warnings": warnings, "error": qv(&e.message),
            "location": e.location.iter().map(|r| ranges_v(r)).collect::<Vec<_>>(),
        }),
        Ok(i) => {
            let stages: Vec<Value> = i
                .stages
                .iter()
                .map(|s| serde_json::json!({
                    "name": qv(&s.name), "orig_cmd": qv(&s.orig_cmd), "base_name": qv(&s.base_name),
                    "platform": qv(&s.platform), "doc_comment": qv(&s.doc_comment), "source_code": qv(&s.source_code),
                    "location": ranges_v(&s.location), "comments": qvs(&s.comments),
                    "commands": s.commands.iter().map(command_v).collect::<Vec<_>>(),
                }))
                .collect();
            let mut out = serde_json::json!({
                "warnings": warnings,
                "meta_args": i.meta_args.iter().map(command_v).collect::<Vec<_>>(),
                "stages": stages,
            });
            if linter.failed() {
                out["lint_error"] = Value::Bool(true);
            }
            out
        }
    }
}

#[test]
fn instructions_parse_as_buildkits_do() {
    let devs = deviations("instructions");
    let mut failures = Vec::new();
    let mut used = 0;
    let cases = load("instructions.json");
    for e in cases.as_array().unwrap() {
        let file = e["file"].as_str().unwrap();
        let text = std::fs::read(testdata().join(file)).unwrap();
        let mut ours = instructions_of(&text);
        ours["file"] = Value::String(file.to_string());
        let mut want = e.clone();
        if let Some(d) = devs.iter().find(|d| d["file"] == file) {
            used += 1;
            want = d["ours"].clone();
        }
        let (ours, want) = (normalize(&ours), normalize(&want));
        if ours != want {
            failures.push(format!(
                "{file}:\n  ours: {}\n  want: {}",
                serde_json::to_string(&ours).unwrap(),
                serde_json::to_string(&want).unwrap()
            ));
        }
    }
    assert_eq!(used, devs.len(), "a deviation names no case");
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        cases.as_array().unwrap().len(),
        failures.join("\n")
    );
}
