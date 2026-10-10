//! The parser against what OPA v1.14.1's parser makes of testdata/parse.json
//! (testdata/parse-oracle.json, written by scripts/rego/generate): each module's text,
//! or its errors' text, byte for byte.

use shards_rego::parser::parse_module;

#[test]
fn modules_parse_as_opa_parses_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/parse.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/parse-oracle.json")).unwrap();
    let mut failed = Vec::new();
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        let got = match parse_module("p.rego", c["src"].as_str().unwrap()) {
            Ok(m) => ("module", m.to_string()),
            Err(e) => ("error", e.to_string()),
        };
        let wanted = match want.get("module") {
            Some(m) => ("module", m.as_str().unwrap().to_string()),
            None => ("error", want["error"].as_str().unwrap().to_string()),
        };
        if got != wanted {
            failed.push(format!(
                "--- {name}\ngot {}:\n{}\nOPA {}:\n{}",
                got.0, got.1, wanted.0, wanted.1
            ));
        }
    }
    let total = cases.as_array().unwrap().len();
    assert!(
        failed.is_empty(),
        "{} of {total} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// A line of 40000 members, a tab before each, parses in time: each term's location
/// copied the columns of the line's tabs so far, a list only OPA's formatter reads, which
/// made the line quadratic (28 s before; 31 ms after, OPA 7.7 s, on an M5 Max).
#[test]
fn a_long_line_of_tabs_parses_in_time() {
    let src = format!("package p\n\nx := [{}]\n", vec!["\t1"; 40_000].join(","));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let m = parse_module("p.rego", &src).map(|m| m.rules.len());
        let _ = tx.send(m.is_ok());
    });
    let parsed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("40000 tabbed members did not parse within 10 s");
    assert!(parsed);
}
