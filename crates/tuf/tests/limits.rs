//! What a repository's metadata may make the client do: no more than go-tuf does, and
//! nothing that ends the process. Metadata is read before its signatures are checked, so
//! each case is one any mirror can serve. Each is one an audit found: a depth Go's decoder
//! allows that overflowed a thread's stack (recursion in the parse, the canonical form,
//! and a value's clone and drop), and counts that took time quadratic in them (names and
//! signatures each checked against all before them).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use shards_tuf::metadata::{Body, Metadata};

/// A thread smaller than any shards runs the client on: a parser that recursed once a
/// level would overflow it long before Go's limit.
const SMALL: usize = 256 << 10;

/// Far more than any of these takes when linear (each well under a second on any machine
/// shards builds on), and far less than when quadratic (minutes).
const LINEAR: Duration = Duration::from_secs(30);

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(SMALL)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn root(extra: &str, signatures: &str) -> Vec<u8> {
    format!(
        r#"{{"signed":{{"_type":"root","spec_version":"1.0.31","version":1,"expires":"2031-01-01T00:00:00Z","consistent_snapshot":true,"keys":{{}},"roles":{{}}{extra}}},"signatures":{signatures}}}"#
    )
    .into_bytes()
}

fn read(b: &[u8]) -> Result<Metadata, String> {
    Metadata::from_bytes("root", b).map_err(|e| e.to_string())
}

/// An unrecognized field as deep as Go's scanner allows (the document's object and
/// `signed` open around it: 10,000 at once), arrays and objects, read, written
/// canonically, cloned and dropped on a small stack; one deeper refused in the scanner's
/// words (measured: encoding/json, Go 1.26.1).
#[test]
fn metadata_nests_as_deep_as_go_allows_on_any_stack() {
    let arrays = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let objects = |n: usize| format!("{}{{}}{}", r#"{"a":"#.repeat(n - 1), "}".repeat(n - 1));
    for deep in [arrays(9998), objects(9998)] {
        let field = format!(r#","x-deep":{deep}"#);
        let (canonical, version) = on_small_stack(move || {
            let m = read(&root(&field, "[]")).unwrap();
            let canonical = m.signed.canonical().unwrap();
            let copy = m.clone();
            drop(m);
            (canonical, copy.signed.version)
        });
        assert_eq!(version, 1);
        let want = format!(r#","version":1,"x-deep":{deep}}}"#);
        assert!(canonical.ends_with(want.as_bytes()));
    }
    for (deep, c) in [(arrays(9999), '['), (objects(9999), '{')] {
        let b = root(&format!(r#","x-deep":{deep}"#), "[]");
        let got = on_small_stack(move || read(&b).map(|_| ()));
        assert_eq!(got, Err(format!("invalid character '{c}' exceeded max depth")));
    }
}

/// Many signatures, keys, roles and unrecognized members each read in time linear in
/// them; a key ID signing twice refused as go-tuf refuses it.
#[test]
fn many_members_are_read_in_linear_time() {
    const N: usize = 200_000;
    let sigs: Vec<String> = (0..N)
        .map(|i| format!(r#"{{"keyid":"k{i}","sig":"00"}}"#))
        .collect();
    let t = Instant::now();
    let m = read(&root("", &format!("[{}]", sigs.join(",")))).unwrap();
    assert_eq!(m.signatures.len(), N);
    let twice = format!(r#"[{},{{"keyid":"k1","sig":"00"}}]"#, sigs.join(","));
    assert_eq!(
        read(&root("", &twice)).map(|_| ()),
        Err("value error: multiple signatures found for key ID k1".to_string())
    );
    let elapsed = t.elapsed();
    assert!(elapsed < LINEAR, "signatures: {elapsed:?}");

    // Keys and roles, Go maps: a repeated name's last value at its first place.
    let names: Vec<String> = (0..N).map(|i| format!(r#""k{i}":null"#)).collect();
    let roles: Vec<String> = (0..N)
        .map(|i| format!(r#""r{i}":{{"keyids":["k{i}"],"threshold":1}}"#))
        .collect();
    let signed = format!(
        r#"{{"signed":{{"_type":"root","version":1,"keys":{{{},"k0":{{"keytype":"ed25519"}}}},"roles":{{{}}}}},"signatures":[]}}"#,
        names.join(","),
        roles.join(",")
    );
    let t = Instant::now();
    let m = read(signed.as_bytes()).unwrap();
    let elapsed = t.elapsed();
    assert!(elapsed < LINEAR, "keys and roles: {elapsed:?}");
    let Body::Root {
        keys: Some(keys),
        roles: Some(roles),
        ..
    } = &m.signed.body
    else {
        panic!("no keys or roles");
    };
    assert_eq!((keys.len(), roles.len()), (N, N));
    assert_eq!(keys[0].0, "k0");
    assert_eq!(keys[0].1.as_ref().map(|k| k.keytype.as_str()), Some("ed25519"));
    assert!(keys[1].1.is_none());

    // An unrecognized object's members, written canonically: sorted, the last of a name.
    let members: Vec<String> = (0..N).rev().map(|i| format!(r#""m{i:06}":{i}"#)).collect();
    let t = Instant::now();
    let m = read(&root(
        &format!(r#","x-many":{{{},"m000000":-1}}"#, members.join(",")),
        "[]",
    ))
    .unwrap();
    let canonical = m.signed.canonical().unwrap();
    let elapsed = t.elapsed();
    assert!(elapsed < LINEAR, "canonical members: {elapsed:?}");
    let text = String::from_utf8(canonical).unwrap();
    assert!(text.contains(r#""x-many":{"m000000":-1,"m000001":1,"m000002":2,"#));
}

/// What go-tuf's checkType makes of a document before its fields (measured: go-tuf as
/// buildx v0.37.1 vendors it, Go 1.26.1): every number read as a float64, one past its
/// range refused wherever it is, even where a later member of its name replaces it; a
/// null document a nil map, with no `signed`.
#[test]
fn documents_are_checked_as_go_tuf_checks_them() {
    for (b, want) in [
        (
            root(r#","x-n":1e400"#, "[]"),
            "json: cannot unmarshal number 1e400 into Go value of type float64",
        ),
        (
            root(r#","x-n":1e400,"x-m":-1e400"#, "[]"),
            "json: cannot unmarshal number 1e400 into Go value of type float64",
        ),
        (
            br#"{"signed":{"_type":"root","version":1e400},"signatures":[]}"#.to_vec(),
            "json: cannot unmarshal number 1e400 into Go value of type float64",
        ),
        (
            root("", r#"[{"keyid":"a","sig":"00","x":1e400}]"#),
            "json: cannot unmarshal number 1e400 into Go value of type float64",
        ),
        (
            br#"{"signed":{"_type":"root","x":1e400,"x":1},"signatures":[]}"#.to_vec(),
            "json: cannot unmarshal number 1e400 into Go value of type float64",
        ),
        (
            b"null".to_vec(),
            "value error: metadata 'signed' field is missing or not an object",
        ),
        (
            b"[1e400]".to_vec(),
            "json: cannot unmarshal array into Go value of type map[string]interface {}",
        ),
        (
            b"1e400".to_vec(),
            "json: cannot unmarshal number into Go value of type map[string]interface {}",
        ),
    ] {
        assert_eq!(
            read(&b).map(|_| ()),
            Err(want.to_string()),
            "{}",
            String::from_utf8_lossy(&b)
        );
    }
    // One too small is 0, which Go takes.
    assert!(read(&root(r#","x-n":1e-400"#, "[]")).is_ok());
}
