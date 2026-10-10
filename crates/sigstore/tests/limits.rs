//! What an attacker's input may make the parsers do: no more than Go does, and nothing
//! that ends the process. Each case is one an audit found: a depth Go allows that
//! overflowed a thread's stack (recursion), and a count that took time quadratic in it.
//! The depths are protojson's own (google.golang.org/protobuf, as buildx vendors it),
//! measured: in-toto's Statement takes a predicate nested 9998 deep and refuses 9999.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use shards_sigstore::tlog::gojson;
use shards_sigstore::{image, proto, schemas};

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

/// An image index (a referrers index among them) read as encoding/json reads one, nested
/// as deep as Go reads, arrays and objects both, on a small stack: Go 1.26's answers
/// (json.Unmarshal into ocispecs.Index, measured), and one level deeper refused in its
/// scanner's words.
#[test]
fn an_index_nests_as_deep_as_go_reads_on_any_stack() {
    let index = |doc: String| on_small_stack(move || image::parse_index(doc.as_bytes()).map(|_| ()));
    let arrays = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let objects = |n: usize| format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n));
    let manifests = |n: usize| format!("{{\"manifests\":{}}}", arrays(n - 1));
    for n in [9_999, 10_000] {
        assert_eq!(
            index(arrays(n)),
            Err("json: cannot unmarshal array into Go value of type v1.Index".to_string())
        );
        assert_eq!(index(objects(n)), Ok(()));
        assert_eq!(
            index(manifests(n)),
            Err(
                "json: cannot unmarshal array into Go struct field Index.manifests of type v1.Descriptor"
                    .to_string()
            )
        );
    }
    let deeper = |c: char| Err(format!("invalid character '{c}' exceeded max depth"));
    assert_eq!(index(arrays(10_001)), deeper('['));
    assert_eq!(index(objects(10_001)), deeper('{'));
    assert_eq!(index(manifests(10_001)), deeper('['));
}

/// A value as deep as Go reads walked in order, copied and let go, on a small stack.
#[test]
fn a_deep_value_is_walked_copied_and_dropped_on_any_stack() {
    let doc = format!("{}1{}", "[{\"a\":".repeat(5_000), "}]".repeat(5_000));
    let got = on_small_stack(move || {
        let (v, spans) = gojson::unmarshal_raw(doc.as_bytes()).unwrap();
        let copy = v.clone();
        let mut walked = Vec::new();
        gojson::pre_order(&copy, &mut walked);
        let kinds: String = walked.iter().map(|x| x.kind().chars().next().unwrap()).collect();
        let last = walked.last().map(|x| (*x).clone());
        drop(walked);
        drop(v);
        (kinds, spans.len(), spans.last().copied(), last)
    });
    assert_eq!(got.0, format!("{}n", "ao".repeat(5_000)));
    assert_eq!(got.1, 10_001);
    // The innermost value's bytes: the `1`.
    assert_eq!(got.2, Some((30_000, 30_001)));
    assert_eq!(got.3, Some(gojson::JValue::Number("1".into())));
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
