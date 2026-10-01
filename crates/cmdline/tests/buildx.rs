//! shards-cmdline answers `build` command lines as buildx does: every answer buildx gave in
//! buildx.json (scripts/buildx/oracle_test.go), shards gives byte for byte. Where buildx
//! ran its build, shards names the same flags and arguments, or refuses the flags it does
//! not serve.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::commands::BUILD;
use shards_cmdline::flags::{self, Flag, Outcome};

fn accept(_: &Flag, value: &str) -> Result<String, String> {
    Ok(value.to_string())
}

#[test]
fn build_answers_as_buildx() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/buildx.json");
    let answers: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut failures = Vec::new();
    for a in answers.as_array().unwrap() {
        let argv: Vec<String> = a["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let (stdout, stderr, status) = (
            a["stdout"].as_str().unwrap(),
            a["stderr"].as_str().unwrap(),
            a["status"].as_u64().unwrap() as u8,
        );
        let (got_out, got_err, got_status, refused) =
            match flags::parse(&BUILD, "shards buildx build", &argv, &accept) {
                Outcome::Run(parsed) => {
                    let mut line = String::from("RUN");
                    for (name, value) in parsed.given() {
                        line.push_str(&format!(" --{name}={value}"));
                    }
                    for arg in &parsed.args {
                        line.push(' ');
                        line.push_str(&shards_cmdline::go::quote(arg));
                    }
                    (format!("{}{line}\n", parsed.notices), String::new(), 0, false)
                }
                Outcome::Help { notices } => (
                    format!("{notices}{}", flags::help(&BUILD, "shards buildx build", 80)),
                    String::new(),
                    0,
                    false,
                ),
                Outcome::Fail {
                    notices,
                    text,
                    status,
                } => {
                    let refused = text.contains("is not supported by shards yet");
                    (notices, format!("{text}\n"), status, refused)
                }
            };
        let ok = if refused {
            stdout.starts_with("RUN") && got_status == 1
        } else {
            got_out == stdout && got_err == stderr && got_status == status
        };
        if !ok {
            failures.push(format!(
                "{argv:?}\n  got:  {got_status} {got_out:?} {got_err:?}\n  want: {status} {stdout:?} {stderr:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} answers differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
