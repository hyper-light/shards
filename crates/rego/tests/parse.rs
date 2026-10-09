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
