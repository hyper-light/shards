//! `search`'s output as the Docker CLI writes it: every case of docker-search.json
//! (scripts/docker-cli/search_test.go, docker/cli v29.8.1's own formatter) written byte
//! for byte, its error too.

#![allow(clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use shards_cmdline::format::search::{self, Found};
use shards_cmdline::format::{Clock, Context, utc};

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[test]
fn results_are_written_as_the_cli_writes_them() {
    let cases: Value = serde_json::from_str(include_str!("docker-search.json")).unwrap();
    let cases = cases.as_array().unwrap();
    assert!(!cases.is_empty());
    let clock = Clock { now: 0, zone: &utc };
    for case in cases {
        let results: Vec<Found> = case
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| Found {
                name: text(r, "name").into(),
                description: text(r, "description").into(),
                star_count: r.get("star_count").and_then(Value::as_i64).unwrap(),
                is_official: r.get("is_official").and_then(Value::as_bool).unwrap(),
                ..Found::default()
            })
            .collect();
        let trunc = case.get("trunc").and_then(Value::as_bool).unwrap();
        let format = search::format(text(case, "format"));
        let ctx = Context {
            format: &format,
            trunc,
            east_asian: false,
            clock: &clock,
        };
        let mut out = String::new();
        let written = search::write(&ctx, &results, &mut out);
        let what = format!("format {:?}, trunc {trunc}", text(case, "format"));
        assert_eq!(out, text(case, "output"), "{what}");
        assert_eq!(written.err().unwrap_or_default(), text(case, "error"), "{what}");
    }
}
