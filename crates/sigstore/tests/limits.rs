//! What an attacker's input may make the parsers do: no more than Go does, and nothing
//! that ends the process. Each case is one an audit found: a depth Go allows that
//! overflowed a thread's stack (recursion), and a count that took time quadratic in it.
//! The depths are protojson's own (google.golang.org/protobuf, as buildx vendors it),
//! measured: in-toto's Statement takes a predicate nested 9998 deep and refuses 9999.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use shards_sigstore::{proto, schemas};

/// A thread smaller than any shards runs a verification on: a parser that recursed once
/// a level would overflow it long before Go's limit.
const SMALL: usize = 256 << 10;

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(SMALL)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn statement(predicate: &str) -> Vec<u8> {
    format!(
        r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[],"predicateType":"x","predicate":{predicate}}}"#
    )
    .into_bytes()
}

fn objects(n: usize) -> Vec<u8> {
    statement(&format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n)))
}

fn arrays(n: usize) -> Vec<u8> {
    statement(&format!("{{\"a\":{}{}}}", "[".repeat(n), "]".repeat(n)))
}

fn parse(b: Vec<u8>) -> Result<(), String> {
    on_small_stack(move || {
        proto::unmarshal(&b, &schemas::STATEMENT)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

/// A google.protobuf.Struct as deep as protojson allows, objects and arrays both, read on
/// a small stack; one deeper refused in Go's words.
#[test]
fn a_predicate_nests_as_deep_as_go_allows_on_any_stack() {
    assert_eq!(parse(objects(9998)), Ok(()));
    assert_eq!(parse(arrays(9998)), Ok(()));
    let refused = Err("proto: exceeded max recursion depth".to_string());
    assert_eq!(parse(objects(9999)), refused);
    assert_eq!(parse(arrays(9999)), refused);
}

/// Many keys in a Struct and in a map<string, string> each read, a repeated one refused
/// where it repeats, as Go's map finds it.
#[test]
fn many_keys_are_read_and_a_repeated_one_is_refused() {
    let keys = |n: usize, extra: &str| -> String {
        let mut s: Vec<String> = (0..n).map(|i| format!("\"k{i}\":1")).collect();
        if !extra.is_empty() {
            s.push(extra.to_string());
        }
        format!("{{{}}}", s.join(","))
    };
    assert_eq!(parse(statement(&keys(200_000, ""))), Ok(()));
    let repeated = parse(statement(&keys(3, "\"k1\":2")));
    assert_eq!(
        repeated,
        Err("proto: (line 1:111): duplicate map key \"k1\"".to_string())
    );
    // A subject's digests, a map<string, string>.
    let digests: Vec<String> = (0..200_000).map(|i| format!("\"d{i}\":\"x\"")).collect();
    let many = format!(
        r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[{{"name":"s","digest":{{{}}}}}],"predicateType":"x"}}"#,
        digests.join(",")
    );
    let got = proto::unmarshal(many.as_bytes(), &schemas::STATEMENT).map(|_| ());
    assert_eq!(got.map_err(|e| e.to_string()), Ok(()));
    let twice = r#"{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"s","digest":{"a":"x","b":"y","a":"z"}}],"predicateType":"x"}"#;
    assert_eq!(
        proto::unmarshal(twice.as_bytes(), &schemas::STATEMENT)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err("proto: (line 1:93): duplicate map key \"a\"".to_string())
    );
}
