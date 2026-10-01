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
