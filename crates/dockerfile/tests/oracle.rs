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
        Kind::Agentfile(d) => panic!("BuildKit's cases are Dockerfiles, without {d:?}"),
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

/// A config's text: a string, or `{"base64": ...}` for text that is not UTF-8.
fn config_text(v: &Value) -> Vec<u8> {
    if let Some(s) = v.as_str() {
        return s.as_bytes().to_vec();
    }
    let enc = v["base64"].as_str().unwrap().trim_end_matches('=');
    let digit = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            _ => 63,
        }
    };
    let mut out = Vec::new();
    for chunk in enc.as_bytes().chunks(4) {
        let n = chunk.iter().fold(0u32, |n, &c| n << 6 | digit(c)) << (6 * (4 - chunk.len()));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    out
}

/// Image configs read as Go reads one into a `DockerOCIImage`, and written as
/// `json.Marshal` writes it: every case of testdata/configs.json, byte for byte, errors
/// included.
#[test]
fn image_configs_read_and_write_as_gos() {
    use shards_dockerfile::image::Image;
    let answers = load("configs-answers.json");
    let answers = answers.as_array().unwrap();
    assert_eq!(answers.len(), load("configs.json").as_array().unwrap().len());
    for a in answers {
        let text = config_text(&a["input"]);
        let got = match Image::from_json(&text) {
            Err(e) => serde_json::json!({ "error": quote(&e) }),
            Ok(image) => match image.to_json() {
                Err(e) => serde_json::json!({ "marshal_error": quote(&e) }),
                Ok(config) => serde_json::json!({ "config": config }),
            },
        };
        let mut want = a.clone();
        want.as_object_mut().unwrap().remove("input");
        assert_eq!(got, want, "{}", String::from_utf8_lossy(&text));
    }
}

/// Sizes read as go-units' `RAMInBytes` reads a tmpfs mount's `size=`: every case of
/// testdata/sizes.json.
#[test]
fn sizes_read_as_go_units_reads_them() {
    let devs = deviations("sizes");
    for a in load("sizes-answers.json").as_array().unwrap() {
        let input = a["input"].as_str().unwrap();
        let got = match shards_dockerfile::go::ram_in_bytes(input.as_bytes()) {
            Ok(n) => serde_json::json!({ "input": input, "bytes": n.to_string() }),
            Err(e) => serde_json::json!({ "input": input, "error": quote(&e) }),
        };
        let mut want = a.clone();
        if let Some(d) = devs.iter().find(|d| d["input"] == input) {
            for (k, v) in d["fields"].as_object().unwrap() {
                want[k] = v.clone();
            }
        }
        assert_eq!(got, want, "{input}");
    }
}

/// The fake base images of testdata/images.json, by reference.
struct Images(serde_json::Map<String, Value>);

impl shards_dockerfile::plan::Resolver for Images {
    fn resolve(
        &self,
        name: &[u8],
        _: &shards_dockerfile::platform::Platform,
        _: &[u8],
    ) -> Result<shards_dockerfile::plan::Resolved, Vec<u8>> {
        let key = String::from_utf8_lossy(name).to_string();
        let Some(img) = self.0.get(&key) else {
            return Err(format!("{key}: not found").into_bytes());
        };
        Ok(shards_dockerfile::plan::Resolved {
            reference: img["ref"].as_str().unwrap().as_bytes().to_vec(),
            digest: img["digest"].as_str().map(|d| d.as_bytes().to_vec()),
            config: serde_json::to_vec(&img["config"]).unwrap(),
        })
    }

    /// The generator plans with no client, with which BuildKit resolves no source's time.
    fn epoch(&self, _: &shards_dockerfile::plan::EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
        Ok(None)
    }
}

fn ustr(b: &[u8]) -> Value {
    Value::String(String::from_utf8(b.to_vec()).unwrap())
}

fn ustrs(v: &[Vec<u8>]) -> Value {
    Value::Array(v.iter().map(|s| ustr(s)).collect())
}

/// Sets `key` unless `v` is what protojson leaves out: false, 0, "", [], {}.
fn put(o: &mut serde_json::Map<String, Value>, key: &str, v: Value) {
    let empty = match &v {
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_i64() == Some(0),
        Value::String(s) => s.is_empty() || s == "0",
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Null => true,
    };
    if !empty {
        o.insert(key.to_string(), v);
    }
}

fn platform_v(p: &shards_dockerfile::platform::Platform) -> Value {
    let mut o = serde_json::Map::new();
    put(&mut o, "Architecture", ustr(&p.architecture));
    put(&mut o, "OS", ustr(&p.os));
    put(&mut o, "Variant", ustr(&p.variant));
    put(&mut o, "OSVersion", ustr(&p.os_version));
    put(&mut o, "OSFeatures", ustrs(&p.os_features));
    Value::Object(o)
}

fn owner_v(c: &Option<shards_dockerfile::llb::OpChown>) -> Value {
    use shards_dockerfile::llb::OpUser;
    let Some(c) = c else {
        return Value::Null;
    };
    let user = |u: &Option<OpUser>| match u {
        None => Value::Null,
        Some(OpUser::Id(id)) => serde_json::json!({ "byID": id }),
        Some(OpUser::Name { name, input }) => {
            let mut n = serde_json::Map::new();
            put(&mut n, "name", ustr(name));
            put(&mut n, "input", Value::String(input.to_string()));
            serde_json::json!({ "byName": n })
        }
    };
    let mut o = serde_json::Map::new();
    put(&mut o, "user", user(&c.user));
    put(&mut o, "group", user(&c.group));
    Value::Object(o)
}

/// An operation as the oracle prints it: protojson with BuildKit's field names.
fn op_v(op: &shards_dockerfile::llb::Op, md: &shards_dockerfile::llb::Meta, order: &[usize]) -> Value {
    use shards_dockerfile::llb::{NetMode, OpActionKind, OpKind, OpMountKind, Security, Sharing};
    let pos = |i: usize| order.iter().position(|&o| o == i).unwrap();
    let mut o = serde_json::Map::new();
    o.insert("constraints".into(), serde_json::json!({}));
    o.insert(
        "inputs".into(),
        Value::Array(
            op.inputs
                .iter()
                .map(|i| serde_json::json!([pos(i.op), i.index]))
                .collect(),
        ),
    );
    if let Some(p) = &op.platform {
        o.insert("platform".into(), platform_v(p));
    }
    let s64 = |n: i64| Value::String(n.to_string());
    match &op.kind {
        OpKind::Source { identifier, attrs } => {
            let mut s = serde_json::Map::new();
            put(&mut s, "identifier", ustr(identifier));
            let mut a = serde_json::Map::new();
            for (k, v) in attrs {
                let k = String::from_utf8(k.clone()).unwrap();
                let v = if k == "local.unique" {
                    Value::String("*".into())
                } else {
                    ustr(v)
                };
                a.insert(k, v);
            }
            put(&mut s, "attrs", Value::Object(a));
            o.insert("source".into(), Value::Object(s));
        }
        OpKind::Exec {
            process,
            mounts,
            network,
            security,
            secret_env,
            devices,
        } => {
            let mut m = serde_json::Map::new();
            put(&mut m, "args", ustrs(&process.args));
            put(&mut m, "env", ustrs(&process.env));
            put(&mut m, "cwd", ustr(&process.cwd));
            put(&mut m, "user", ustr(&process.user));
            if let Some(p) = &process.proxy {
                let mut pe = serde_json::Map::new();
                put(&mut pe, "http_proxy", ustr(&p.http));
                put(&mut pe, "https_proxy", ustr(&p.https));
                put(&mut pe, "ftp_proxy", ustr(&p.ftp));
                put(&mut pe, "no_proxy", ustr(&p.no));
                put(&mut pe, "all_proxy", ustr(&p.all));
                m.insert("proxy_env".into(), Value::Object(pe));
            }
            put(&mut m, "hostname", ustr(&process.hostname));
            if !process.ulimits.is_empty() {
                // protojson: zero fields left out, 64-bit integers as strings.
                let us = process
                    .ulimits
                    .iter()
                    .map(|u| {
                        let mut o = serde_json::Map::new();
                        put(&mut o, "Name", ustr(&u.name));
                        for (k, v) in [("Soft", u.soft), ("Hard", u.hard)] {
                            if v != 0 {
                                o.insert(k.into(), Value::String(v.to_string()));
                            }
                        }
                        Value::Object(o)
                    })
                    .collect();
                m.insert("ulimit".into(), Value::Array(us));
            }
            put(&mut m, "cgroupParent", ustr(&process.cgroup_parent));
            m.insert("removeMountStubsRecursive".into(), Value::Bool(true));
            let mut e = serde_json::Map::new();
            e.insert("meta".into(), Value::Object(m));
            let ms: Vec<Value> = mounts
                .iter()
                .map(|mt| {
                    let mut x = serde_json::Map::new();
                    put(&mut x, "input", s64(mt.input));
                    put(&mut x, "selector", ustr(&mt.selector));
                    put(&mut x, "dest", ustr(&mt.dest));
                    put(&mut x, "output", s64(mt.output));
                    put(&mut x, "readonly", Value::Bool(mt.readonly));
                    match &mt.kind {
                        OpMountKind::Bind => {}
                        OpMountKind::Cache { id, sharing } => {
                            x.insert("mountType".into(), "CACHE".into());
                            let mut c = serde_json::Map::new();
                            put(&mut c, "ID", ustr(id));
                            match sharing {
                                Sharing::Shared => {}
                                Sharing::Private => {
                                    c.insert("sharing".into(), "PRIVATE".into());
                                }
                                Sharing::Locked => {
                                    c.insert("sharing".into(), "LOCKED".into());
                                }
                            }
                            x.insert("cacheOpt".into(), Value::Object(c));
                        }
                        OpMountKind::Tmpfs { size } => {
                            x.insert("mountType".into(), "TMPFS".into());
                            let mut t = serde_json::Map::new();
                            put(&mut t, "size", s64(*size));
                            x.insert("TmpfsOpt".into(), Value::Object(t));
                        }
                        OpMountKind::Secret {
                            id,
                            uid,
                            gid,
                            mode,
                            optional,
                        }
                        | OpMountKind::Ssh {
                            id,
                            uid,
                            gid,
                            mode,
                            optional,
                        } => {
                            let (kind, key) = match &mt.kind {
                                OpMountKind::Secret { .. } => ("SECRET", "secretOpt"),
                                _ => ("SSH", "SSHOpt"),
                            };
                            x.insert("mountType".into(), kind.into());
                            let mut s = serde_json::Map::new();
                            put(&mut s, "ID", ustr(id));
                            put(&mut s, "uid", (*uid).into());
                            put(&mut s, "gid", (*gid).into());
                            put(&mut s, "mode", (*mode).into());
                            put(&mut s, "optional", Value::Bool(*optional));
                            x.insert(key.into(), Value::Object(s));
                        }
                    }
                    Value::Object(x)
                })
                .collect();
            put(&mut e, "mounts", Value::Array(ms));
            match network {
                NetMode::Sandbox => {}
                NetMode::Host => {
                    e.insert("network".into(), "HOST".into());
                }
                NetMode::None => {
                    e.insert("network".into(), "NONE".into());
                }
            }
            if *security == Security::Insecure {
                e.insert("security".into(), "INSECURE".into());
            }
            let se: Vec<Value> = secret_env
                .iter()
                .map(|(id, name, optional)| {
                    let mut x = serde_json::Map::new();
                    put(&mut x, "ID", ustr(id));
                    put(&mut x, "name", ustr(name));
                    put(&mut x, "optional", Value::Bool(*optional));
                    Value::Object(x)
                })
                .collect();
            put(&mut e, "secretenv", Value::Array(se));
            let dv: Vec<Value> = devices
                .iter()
                .map(|d| {
                    let mut x = serde_json::Map::new();
                    put(&mut x, "name", ustr(&d.name));
                    put(&mut x, "optional", Value::Bool(d.optional));
                    Value::Object(x)
                })
                .collect();
            put(&mut e, "cdiDevices", Value::Array(dv));
            o.insert("exec".into(), Value::Object(e));
        }
        OpKind::File { actions } => {
            let acts: Vec<Value> = actions
                .iter()
                .map(|a| {
                    let mut x = serde_json::Map::new();
                    put(&mut x, "input", s64(a.input));
                    put(&mut x, "secondaryInput", s64(a.secondary_input));
                    put(&mut x, "output", s64(a.output));
                    let mut b = serde_json::Map::new();
                    let key = match &a.action {
                        OpActionKind::Mkdir {
                            path,
                            mode,
                            make_parents,
                            owner,
                            timestamp,
                        } => {
                            put(&mut b, "path", ustr(path));
                            put(&mut b, "mode", (*mode).into());
                            put(&mut b, "makeParents", Value::Bool(*make_parents));
                            put(&mut b, "owner", owner_v(owner));
                            put(&mut b, "timestamp", s64(*timestamp));
                            "mkdir"
                        }
                        OpActionKind::Mkfile {
                            path,
                            mode,
                            data,
                            owner,
                            timestamp,
                        } => {
                            put(&mut b, "path", ustr(path));
                            put(&mut b, "mode", (*mode).into());
                            put(&mut b, "data", Value::String(base64(data)));
                            put(&mut b, "owner", owner_v(owner));
                            put(&mut b, "timestamp", s64(*timestamp));
                            "mkfile"
                        }
                        OpActionKind::Copy {
                            src,
                            dest,
                            owner,
                            mode,
                            mode_str,
                            follow_symlink,
                            dir_copy_contents,
                            attempt_unpack,
                            create_dest_path,
                            allow_wildcard,
                            allow_empty_wildcard,
                            timestamp,
                            include_patterns,
                            exclude_patterns,
                            required_paths,
                        } => {
                            put(&mut b, "src", ustr(src));
                            put(&mut b, "dest", ustr(dest));
                            put(&mut b, "owner", owner_v(owner));
                            put(&mut b, "mode", (*mode).into());
                            put(&mut b, "followSymlink", Value::Bool(*follow_symlink));
                            put(&mut b, "dirCopyContents", Value::Bool(*dir_copy_contents));
                            put(
                                &mut b,
                                "attemptUnpackDockerCompatibility",
                                Value::Bool(*attempt_unpack),
                            );
                            put(&mut b, "createDestPath", Value::Bool(*create_dest_path));
                            put(&mut b, "allowWildcard", Value::Bool(*allow_wildcard));
                            put(&mut b, "allowEmptyWildcard", Value::Bool(*allow_empty_wildcard));
                            put(&mut b, "timestamp", s64(*timestamp));
                            put(&mut b, "include_patterns", ustrs(include_patterns));
                            put(&mut b, "exclude_patterns", ustrs(exclude_patterns));
                            put(&mut b, "modeStr", ustr(mode_str));
                            put(&mut b, "required_paths", ustrs(required_paths));
                            "copy"
                        }
                    };
                    x.insert(key.into(), Value::Object(b));
                    Value::Object(x)
                })
                .collect();
            o.insert("file".into(), serde_json::json!({ "actions": acts }));
        }
        OpKind::Merge => {
            let ins: Vec<Value> = (0..op.inputs.len())
                .map(|i| {
                    let mut x = serde_json::Map::new();
                    put(&mut x, "input", s64(i as i64));
                    Value::Object(x)
                })
                .collect();
            o.insert("merge".into(), serde_json::json!({ "inputs": ins }));
        }
    }
    o.insert("metadata".into(), meta_v(md));
    Value::Object(o)
}

fn meta_v(md: &shards_dockerfile::llb::Meta) -> Value {
    let mut m = serde_json::Map::new();
    put(&mut m, "ignore_cache", Value::Bool(md.ignore_cache));
    let mut d = serde_json::Map::new();
    for (k, v) in &md.description {
        d.insert(String::from_utf8(k.clone()).unwrap(), ustr(v));
    }
    put(&mut m, "description", Value::Object(d));
    if let Some(pg) = &md.progress_group {
        let mut p = serde_json::Map::new();
        p.insert("id".into(), "*".into());
        put(&mut p, "name", ustr(&pg.name));
        put(&mut p, "weak", Value::Bool(pg.weak));
        m.insert("progress_group".into(), Value::Object(p));
    }
    Value::Object(m)
}

fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &c)| n | u32::from(c) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A definition as the oracle lists it: depth first from the end, each input before what
/// reads it, then the end itself, which reads the target's output.
fn definition_v(def: &shards_dockerfile::llb::Definition) -> Value {
    let Some(root) = def.root else {
        return Value::Array(Vec::new());
    };
    let mut order = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    fn visit(
        i: usize,
        def: &shards_dockerfile::llb::Definition,
        seen: &mut std::collections::BTreeSet<usize>,
        order: &mut Vec<usize>,
    ) {
        if !seen.insert(i) {
            return;
        }
        for inp in &def.ops[i].inputs {
            visit(inp.op, def, seen, order);
        }
        order.push(i);
    }
    visit(root.op, def, &mut seen, &mut order);
    let mut out: Vec<Value> = order
        .iter()
        .map(|&i| op_v(&def.ops[i], &def.metadata[i], &order))
        .collect();
    let pos = order.iter().position(|&o| o == root.op).unwrap();
    out.push(serde_json::json!({ "inputs": [[pos, root.index]], "metadata": {} }));
    Value::Array(out)
}

/// Build plans as BuildKit's Dockerfile2LLB makes them: every file of testdata/corpus/plan
/// with its options, against images.json's base images. The graph op by op, the image
/// config byte for byte, the checks' warnings and any error.
#[test]
fn plans_are_buildkits() {
    use shards_dockerfile::plan::{Options, plan};
    use shards_dockerfile::platform::Platform;
    let images = Images(load("images.json").as_object().unwrap().clone());
    let devs = deviations("plan");
    let mut failures = Vec::new();
    for want in load("plan.json").as_array().unwrap() {
        let file = want["file"].as_str().unwrap();
        let text = std::fs::read(testdata().join(file)).unwrap();
        let opts_v: Value = std::fs::read(testdata().join(format!("{file}.opts.json")))
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap_or(Value::Null);
        let map = |v: &Value| -> std::collections::BTreeMap<Vec<u8>, Vec<u8>> {
            v.as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_str().unwrap().as_bytes().to_vec()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let opts = Options {
            target_platform: Platform::new("linux", "amd64"),
            build_platforms: vec![Platform::new("linux", "amd64")],
            build_args: map(&opts_v["build_args"]),
            target: opts_v["target"].as_str().unwrap_or_default().as_bytes().to_vec(),
            labels: map(&opts_v["labels"]),
            hostname: opts_v["hostname"]
                .as_str()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
            ulimits: opts_v["ulimit"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(|v| {
                    shards_cmdline::go::csv_fields(v.as_bytes())
                        .unwrap()
                        .iter()
                        .map(|f| {
                            let u = shards_cmdline::buildflags::parse_ulimit(std::str::from_utf8(f).unwrap())
                                .unwrap();
                            shards_dockerfile::llb::Ulimit {
                                name: u.name.into_bytes(),
                                soft: u.soft,
                                hard: u.hard,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            multi_platform: false,
            context_id: b"*".to_vec(),
            excludes: Vec::new(),
            dialect: shards_dockerfile::parser::Dialect::Dockerfile,
            contexts: map(&opts_v["contexts"]),
            context_keys: map(&opts_v["shared_keys"]),
            context_excludes: Default::default(),
        };
        let mut got = serde_json::Map::new();
        got.insert("file".into(), file.into());
        let warnings = |ws: &[shards_dockerfile::lint::Warning]| -> Value {
            if ws.is_empty() {
                return Value::Null;
            }
            let mut v: Vec<String> = ws
                .iter()
                .map(|w| {
                    let lines: Vec<String> = w.location.iter().map(|(s, e)| format!("{s}-{e}")).collect();
                    quote(
                        format!(
                            "{}|{}|{}",
                            w.rule,
                            String::from_utf8_lossy(&w.message),
                            lines.join(",")
                        )
                        .as_bytes(),
                    )
                })
                .collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        };
        match plan(&text, &opts, &images) {
            Err(e) => {
                got.insert("warnings".into(), warnings(&e.warnings));
                got.insert("error".into(), quote(&e.message).into());
                if !e.location.is_empty() {
                    got.insert("location".into(), serde_json::to_value(&e.location).unwrap());
                }
            }
            Ok(p) => {
                got.insert("warnings".into(), warnings(&p.warnings));
                got.insert("image".into(), p.image.to_json().unwrap().into());
                got.insert("ops".into(), definition_v(&p.definition()));
            }
        }
        let mut want = want.clone();
        if let Some(Value::Array(ws)) = want.get_mut("warnings") {
            let mut v: Vec<String> = ws.iter().map(|w| w.as_str().unwrap().to_string()).collect();
            v.sort();
            *ws = v.into_iter().map(Value::String).collect();
        }
        if let Some(d) = devs.iter().find(|d| d["file"] == file) {
            for (k, v) in d["fields"].as_object().unwrap() {
                want[k] = v.clone();
            }
        }
        let got = Value::Object(got);
        if got != want {
            failures.push(format!(
                "{file}\n  got:  {}\n  want: {}",
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&want).unwrap()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of the plans differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Sources read as Go's net/url, BuildKit's gitutil.ParseURL and dfgitutil.ParseGitRef
/// read them: every case of testdata/urls.json.
#[test]
fn urls_read_as_gos() {
    use shards_dockerfile::{git, url};
    let s = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
    let mut failures = Vec::new();
    for want in load("urls-answers.json").as_array().unwrap() {
        let input = want["input"].as_str().unwrap();
        let b = input.as_bytes();
        let mut got = serde_json::Map::new();
        got.insert("input".into(), input.into());
        match url::parse(b) {
            Err(e) => {
                got.insert("url_error".into(), quote(&e).into());
            }
            Ok(u) => {
                let user = u
                    .user
                    .as_ref()
                    .map(|x| format!("{}@", s(&url::userinfo_string(x))))
                    .unwrap_or_default();
                got.insert(
                    "url".into(),
                    serde_json::json!({
                        "scheme": s(&u.scheme), "opaque": s(&u.opaque), "user": user, "host": s(&u.host),
                        "path": s(&u.path), "raw_path": s(&u.raw_path), "raw_query": s(&u.raw_query),
                        "fragment": s(&u.fragment), "string": s(&u.string()),
                    }),
                );
            }
        }
        match git::parse_url(b) {
            Err(git::UrlError::UnknownProtocol) => {
                got.insert("git_url_error".into(), quote(b"unknown protocol").into());
            }
            Err(git::UrlError::Other(e)) => {
                got.insert("git_url_error".into(), quote(&e).into());
            }
            Ok(g) => {
                got.insert(
                    "git_url".into(),
                    serde_json::json!({"scheme": s(&g.scheme), "host": s(&g.host), "path": s(&g.path), "remote": s(&g.remote), "opts": g.opts.is_some()}),
                );
            }
        }
        match git::parse_git_ref(b) {
            git::Parsed::NotGit => {
                got.insert("is_git".into(), false.into());
            }
            git::Parsed::BadGit(e) => {
                got.insert("is_git".into(), true.into());
                got.insert("git_ref_error".into(), quote(&e).into());
            }
            git::Parsed::Git(r) => {
                got.insert("is_git".into(), true.into());
                let pair = |o: Option<bool>| format!("{} {}", o == Some(true), o.is_some());
                got.insert(
                    "git_ref".into(),
                    serde_json::json!({
                        "remote": s(&r.remote), "short_name": s(&r.short_name), "ref": s(&r.reference),
                        "checksum": s(&r.checksum), "subdir": s(&r.subdir), "local": r.indistinguishable_from_local,
                        "tcp": r.unencrypted_tcp, "keep": pair(r.keep_git_dir), "submodules": pair(r.submodules),
                        "mtime": s(&r.mtime), "fetch_by_commit": r.fetch_by_commit,
                    }),
                );
            }
        }
        let got = Value::Object(got);
        if &got != want {
            failures.push(format!("{input:?}\n  got:  {got}\n  want: {want}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of the sources differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Images exported as BuildKit's exporter exports them: every case of testdata/exports.json,
/// its config and manifest byte for byte.
#[test]
fn exports_are_buildkits() {
    use sha2::{Digest as _, Sha256};
    use shards_dockerfile::export::{self, Layer};
    use shards_dockerfile::go::Time;
    use shards_dockerfile::image::Image;
    let digest = |s: &str| {
        let h = Sha256::digest(s.as_bytes());
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}").into_bytes()
    };
    let plans = load("plan.json");
    let image_of = |file: &str| -> String {
        plans
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["file"] == file)
            .and_then(|p| p["image"].as_str())
            .unwrap()
            .to_string()
    };
    let mut failures = Vec::new();
    for c in load("exports.json").as_array().unwrap() {
        let file = c["file"].as_str().unwrap();
        let image = Image::from_json(image_of(file).as_bytes()).unwrap();
        let n = c["layers"].as_u64().unwrap() as usize;
        let layers: Vec<Layer> = (0..n)
            .map(|i| Layer {
                media_type: b"application/vnd.docker.image.rootfs.diff.tar.gzip".to_vec(),
                digest: digest(&format!("blob {i}")),
                size: 100 + i as u64,
                diff_id: digest(&format!("diff {i}")),
                annotations: [
                    (
                        b"containerd.io/uncompressed".to_vec(),
                        digest(&format!("diff {i}")),
                    ),
                    (b"buildkit/createdat".to_vec(), b"x".to_vec()),
                    (b"org.example.kept".to_vec(), format!("v{i} <&>").into_bytes()),
                ]
                .into_iter()
                .collect(),
                created: None,
                description: Vec::new(),
            })
            .collect();
        let epoch = c["epoch"]
            .as_bool()
            .unwrap()
            .then(|| Time::from_unix(1_700_000_000));
        let base = c["base"].as_bool().unwrap().then_some(&image);
        let mut got = serde_json::Map::new();
        for k in ["file", "layers", "epoch", "base"] {
            got.insert(k.into(), c[k].clone());
        }
        match export::config(&image, &layers, epoch, base) {
            Err(e) => {
                got.insert("error".into(), String::from_utf8(e).unwrap().into());
            }
            Ok(config) => {
                let h = Sha256::digest(&config);
                let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
                let manifest = export::manifest(&config, format!("sha256:{hex}").as_bytes(), &layers);
                got.insert("config".into(), String::from_utf8(config).unwrap().into());
                got.insert("manifest".into(), String::from_utf8(manifest).unwrap().into());
            }
        }
        let got = Value::Object(got);
        if &got != c {
            failures.push(format!(
                "{file} {n} {} {}\n  got:  {got}\n  want: {c}",
                c["epoch"], c["base"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} exports differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Paths matched as Go's filepath.Match and moby/patternmatcher match them: every set of
/// testdata/patterns.json.
#[test]
fn patterns_match_as_gos() {
    use shards_dockerfile::glob::{self, MatchInfo, PatternMatcher};
    let ans = |r: Result<bool, Vec<u8>>| match r {
        Ok(b) => b.to_string(),
        Err(e) => format!("error {}", String::from_utf8_lossy(&e)),
    };
    let devs = deviations("patterns");
    let mut failures = Vec::new();
    for want in load("patterns-answers.json").as_array().unwrap() {
        let patterns: Vec<Vec<u8>> = strs(&want["patterns"])
            .into_iter()
            .map(String::into_bytes)
            .collect();
        let paths: Vec<Vec<u8>> = strs(&want["paths"]).into_iter().map(String::into_bytes).collect();
        let mut got = serde_json::Map::new();
        got.insert("patterns".into(), want["patterns"].clone());
        got.insert("paths".into(), want["paths"].clone());
        let fm: Vec<Value> = patterns
            .iter()
            .map(|p| {
                Value::Array(
                    paths
                        .iter()
                        .map(|path| {
                            ans(glob::filepath_match(p, path)
                                .map_err(|_| glob::BAD_PATTERN.as_bytes().to_vec()))
                            .into()
                        })
                        .collect(),
                )
            })
            .collect();
        got.insert("filepath_match".into(), Value::Array(fm));
        match PatternMatcher::new(&patterns) {
            Err(e) => {
                got.insert(
                    "new_error".into(),
                    String::from_utf8_lossy(&e).into_owned().into(),
                );
            }
            Ok(mut pm) => {
                let (mut m, mut pmm, mut walked) = (Vec::new(), Vec::new(), Vec::new());
                for path in &paths {
                    m.push(Value::String(ans(pm.matches(path))));
                    pmm.push(Value::String(ans(pm.matches_or_parent_matches(path))));
                    let parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
                    let mut info = MatchInfo::default();
                    let mut last = String::new();
                    for i in 0..parts.len() {
                        match pm.matches_using_parent_results(&parts[..=i].join(&b'/'), &info) {
                            Ok((ok, next)) => {
                                last = ok.to_string();
                                info = next;
                            }
                            Err(e) => {
                                last = format!("error {}", String::from_utf8_lossy(&e));
                                break;
                            }
                        }
                    }
                    walked.push(Value::String(last));
                }
                got.insert("matches".into(), Value::Array(m));
                got.insert("parent_matches".into(), Value::Array(pmm));
                got.insert("walked".into(), Value::Array(walked));
            }
        }
        let mut want = want.clone();
        if let Some(d) = devs.iter().find(|d| d["patterns"] == want["patterns"]) {
            for (k, v) in d["fields"].as_object().unwrap() {
                want[k] = v.clone();
            }
        }
        let got = Value::Object(got);
        if got != want {
            failures.push(format!("{}\n  got:  {got}\n  want: {want}", want["patterns"]));
        }
    }
    assert!(
        failures.is_empty(),
        "{} pattern sets differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// .dockerignore files read as ignorefile.ReadAll reads them: testdata/ignores.json.
#[test]
fn dockerignore_files_read_as_buildkits_frontend_reads_them() {
    let files: Vec<String> =
        serde_json::from_slice(&std::fs::read(testdata().join("ignores.json")).unwrap()).unwrap();
    let answers = load("ignores-answers.json");
    let answers = answers.as_array().unwrap();
    assert_eq!(files.len(), answers.len());
    let devs = deviations("ignore");
    for (i, (file, want)) in files.iter().zip(answers).enumerate() {
        let mut got = serde_json::Map::new();
        got.insert("file".into(), qv(file.as_bytes()));
        let p = shards_dockerfile::ignore::read_all(file.as_bytes());
        got.insert(
            "patterns".into(),
            if p.is_empty() { Value::Null } else { qvs(&p) },
        );
        let want = match devs.iter().find(|d| d["case"] == i) {
            Some(d) => d["ours"].clone(),
            None => want.clone(),
        };
        assert_eq!(Value::Object(got), want, "{file:?}");
    }
}

/// A chain of stages each reading the next, none of them the target, longer than the
/// stack of a test's thread held when the cycle check recursed (10,000 overflowed 2 MiB):
/// planned, as BuildKit plans it, and a cycle through it still found.
#[test]
fn a_long_chain_of_stages_plans() {
    const STAGES: usize = 20_000;
    let mut text = String::new();
    for i in 0..STAGES - 1 {
        text.push_str(&format!("FROM scratch AS s{i}\nCOPY --from=s{} /a /a\n", i + 1));
    }
    text.push_str(&format!("FROM scratch AS s{}\n", STAGES - 1));
    let opts = shards_dockerfile::plan::Options {
        target_platform: shards_dockerfile::platform::Platform::new("linux", "amd64"),
        build_platforms: vec![shards_dockerfile::platform::Platform::new("linux", "amd64")],
        ..Default::default()
    };
    let none = Images(serde_json::Map::new());
    let planned = shards_dockerfile::plan::plan(text.as_bytes(), &opts, &none);
    assert!(planned.is_ok(), "{:?}", planned.err());
    text.push_str("COPY --from=s0 /a /a\n");
    let cycle = shards_dockerfile::plan::plan(text.as_bytes(), &opts, &none);
    let said = cycle
        .err()
        .map(|e| String::from_utf8_lossy(&e.message).into_owned());
    assert_eq!(said.as_deref(), Some("circular dependency detected on stage: s0"));
}
