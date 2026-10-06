//! The filter held to runc's own (opencontainers/cgroups v0.0.4 deviceFilter, as
//! `scripts/devcgroup/generate` records it): for each case, its program's bytes, or its
//! error, the same.

use super::{Kind, MKNOD, READ, Rule, WRITE, compile};

/// devices.Permissions' set: the letters it knows, others dropped (fromSet).
fn perms(s: &str) -> u8 {
    s.chars().fold(0, |acc, c| {
        acc | match c {
            'r' => READ,
            'w' => WRITE,
            'm' => MKNOD,
            _ => 0,
        }
    })
}

#[test]
fn the_filter_is_runcs_own() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/cases.json")).unwrap();
    let answers: serde_json::Value = serde_json::from_str(include_str!("../testdata/oracle.json")).unwrap();
    let (cases, answers) = (cases.as_array().unwrap(), answers.as_array().unwrap());
    assert_eq!(cases.len(), answers.len());
    for (case, answer) in cases.iter().zip(answers) {
        let name = case["name"].as_str().unwrap();
        assert_eq!(answer["name"], name);
        let rules: Vec<Rule> = case["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| Rule {
                kind: match r["type"].as_str().unwrap() {
                    "a" => Kind::All,
                    "b" => Kind::Block,
                    _ => Kind::Char,
                },
                major: r["major"].as_i64().unwrap(),
                minor: r["minor"].as_i64().unwrap(),
                perms: perms(r["perms"].as_str().unwrap()),
                allow: r["allow"].as_bool().unwrap(),
            })
            .collect();
        let ours = compile(&rules).map(|b| b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        match answer.get("error").and_then(|e| e.as_str()) {
            Some(e) => assert_eq!(ours, Err(e.to_string()), "{name}"),
            None => assert_eq!(ours.as_deref(), Ok(answer["program"].as_str().unwrap()), "{name}"),
        }
    }
}
