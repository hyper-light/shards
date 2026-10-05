//! shards-cmdline answers command lines as the Docker CLI does: every answer the CLI gave
//! in docker-cli.json (scripts/docker-cli/oracle_test.go), shards gives byte for byte.
//! Where the CLI ran its command, shards names the same flags and arguments, or refuses
//! the flags it does not serve.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::commands::{self, RUN};
use shards_cmdline::flags::{self, Flag, Outcome};

/// The CLI's `opts.ValidateEnv` in the oracle's environment, where only B is set, and
/// its `NetworkOpt.Set`.
fn validate(flag: &Flag, value: &str) -> Result<String, String> {
    if matches!(flag.name, "network" | "net") {
        return shards_cmdline::network::attachment(value).map(|_| value.to_string());
    }
    if flag.name != "env" {
        return Ok(value.to_string());
    }
    let (key, has_value) = match value.split_once('=') {
        Some((k, _)) => (k, true),
        None => (value, false),
    };
    if key.is_empty() {
        return Err(format!("invalid environment variable: {value}"));
    }
    if !has_value && key == "B" {
        return Ok("B=from-env".to_string());
    }
    Ok(value.to_string())
}

/// What shards answers to `argv`: stdout, stderr, the status, and whether it refused
/// flags it does not serve.
fn answer(argv: &[String]) -> (String, String, u8, bool) {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (command, path, rest) = match words.as_slice() {
        ["run", ..] => (&RUN, "shards run", argv.get(1..).unwrap_or_default()),
        _ => {
            let (command, path, named) = commands::find(&words).unwrap();
            (command, path, argv.get(named..).unwrap_or_default())
        }
    };
    match flags::parse(command, path, rest, &validate) {
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
            format!("{notices}{}", flags::help(command, path, 80)),
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
    }
}

/// What shards' `--help` adds after the CLI's text, by command: its own flags
/// (`Flag::extension`), each a deliberate difference.
const EXTENDED: &[(&str, &str)] = &[(
    "pull",
    "\nShards options:\n      --no-cache                  Fetch every layer again, though stored,\n                                  and build its microVM again\n      --output-agentfile string   Write the image's Agentfile to this\n                                  file, or into this directory\n",
)];

/// `got`, with the section shards adds to `argv`'s help taken off: that section must be
/// what [`EXTENDED`] says, and the rest is the CLI's.
fn without_extensions<'a>(argv: &[String], got: &'a str) -> &'a str {
    let Some(&(_, added)) = EXTENDED
        .iter()
        .find(|(command, _)| argv.iter().any(|a| a == command) && got.contains("\nShards options:\n"))
    else {
        return got;
    };
    got.strip_suffix(added)
        .unwrap_or_else(|| panic!("{argv:?}: not {added:?} at the end: {got:?}"))
}

#[test]
fn command_lines_are_answered_as_the_docker_cli_answers_them() {
    let golden: serde_json::Value = serde_json::from_str(include_str!("docker-cli.json")).unwrap();
    let cases = golden.as_array().unwrap();
    assert!(cases.len() > 100, "{} cases", cases.len());
    let mut refused = 0;
    for case in cases {
        let argv: Vec<String> = case["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let want = (
            case["stdout"].as_str().unwrap(),
            case["stderr"].as_str().unwrap(),
            case["status"].as_u64().unwrap(),
        );
        let (stdout, stderr, status, refusal) = answer(&argv);
        if refusal {
            // The CLI ran it; shards says which flags it does not serve yet.
            assert!(
                want.0.contains("RUN"),
                "{argv:?}: the CLI did not run it: {want:?}"
            );
            assert_eq!(status, 1, "{argv:?}");
            refused += 1;
            continue;
        }
        assert_eq!(
            (
                without_extensions(&argv, &stdout),
                stderr.as_str(),
                u64::from(status)
            ),
            want,
            "{argv:?}"
        );
    }
    assert!(refused > 0, "some cases ask for what shards does not serve");
}
