//! shards-cmdline answers command lines as the Docker CLI does: every answer the CLI gave
//! in docker-cli.json (scripts/docker-cli/oracle_test.go), shards gives byte for byte.
//! Where the CLI ran its command, shards names the same flags and arguments, or refuses
//! the flags it does not serve.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::commands::{self, LOGIN, LOGOUT, RUN, SEARCH};
use shards_cmdline::flags::{self, Flag, Outcome};

/// The CLI's `opts.ValidateEnv` in the oracle's environment, where only B is set, and
/// its `NetworkOpt.Set`.
fn validate(flag: &Flag, value: &str) -> Result<String, String> {
    if matches!(flag.name, "network" | "net") {
        return shards_cmdline::network::attachment(value).map(|_| value.to_string());
    }
    if flag.name != "env" {
        return shards_cmdline::flags::value(flag, value);
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
        // Run where the client is, as `run` is read there: no command the daemon serves.
        ["login", ..] => (&LOGIN, "shards login", argv.get(1..).unwrap_or_default()),
        ["logout", ..] => (&LOGOUT, "shards logout", argv.get(1..).unwrap_or_default()),
        ["search", ..] => (&SEARCH, "shards search", argv.get(1..).unwrap_or_default()),
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
const CONVERT: &str =
    "\nShards options:\n      --convert   Convert an image that is not a microVM yet, first\n";

const EXTENDED: &[(&str, &str)] = &[
    (
        "pull",
        "\nShards options:\n      --no-cache                  Fetch every layer again, though stored,\n                                  and build its microVM again\n      --output-agentfile string   Write the image's Agentfile to this\n                                  file, or into this directory\n",
    ),
    (
        "rmi",
        "\nShards options:\n      --vms   Remove stopped microVMs\n",
    ),
    // `image rm` and `image remove` are rmi.
    ("rm", "\nShards options:\n      --vms   Remove stopped microVMs\n"),
    (
        "remove",
        "\nShards options:\n      --vms   Remove stopped microVMs\n",
    ),
    // shards' grammar asks these to convert an image first (`--convert`).
    ("inspect", CONVERT),
    ("history", CONVERT),
    ("save", CONVERT),
    ("push", CONVERT),
    ("tag", CONVERT),
];

/// The flags whose `--help` is shards' own, each a deliberate difference: Docker's for its
/// VM-isolated containers, which its CLI hides and refuses against a Linux daemon and
/// shards serves, as every run is a microVM (`--cpu-count` its vCPUs, `--cpu-percent` a
/// share of the host's CPU time, D155); and those whose meaning is the microVM's
/// (`--uts host` the host's hostname, `--cgroupns host` the microVM's cgroups,
/// `--storage-opt` the writable layer's size, D156).
const MICROVM_FLAGS: [&str; 5] = ["cpu-count", "cpu-percent", "cgroupns", "uts", "storage-opt"];

/// What the CLI says of a flag of Windows' daemons against a Linux one.
const WINDOWS_ONLY: &str = "is only supported on a Docker daemon running on windows";

/// `got` (help), without the lines of [`MICROVM_FLAGS`]: each flag's line and the lines its
/// text wraps onto.
fn without_microvm_flags(got: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in got.lines() {
        let flag = line
            .trim_start()
            .strip_prefix("--")
            .map(|f| f.split(' ').next().unwrap_or_default());
        if let Some(name) = flag {
            skipping = MICROVM_FLAGS.contains(&name);
        } else if !line.starts_with("          ") {
            skipping = false;
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut text = out.join("\n");
    if got.ends_with('\n') {
        text.push('\n');
    }
    text
}

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
fn command_lines_are_answered_byte_for_byte() {
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
        // shards keeps `-h` for help, where docker/cli deprecates it: a deliberate
        // difference, its notice the only one.
        let stdout_want = case["stdout"]
            .as_str()
            .unwrap()
            .replace("Flag shorthand -h has been deprecated, use --help\n", "");
        let want = (
            stdout_want.as_str(),
            case["stderr"].as_str().unwrap(),
            case["status"].as_u64().unwrap(),
        );
        let (stdout, stderr, status, refusal) = answer(&argv);
        // `-h` with nothing after it is help in shards, where `run`'s and `create`'s `-h`
        // is `--hostname` and docker/cli wants its value: shards answers as `--help` does.
        if argv.last().is_some_and(|a| a == "-h")
            && case["stderr"]
                .as_str()
                .unwrap()
                .starts_with("flag needs an argument: 'h' in -h")
        {
            let mut helped = argv.clone();
            helped.pop();
            helped.push("--help".into());
            assert_eq!(answer(&argv).0, answer(&helped).0, "{argv:?}");
            assert_eq!(status, 0, "{argv:?}");
            continue;
        }
        // A flag of Windows' VM-isolated containers, which the CLI refuses against a Linux
        // daemon: shards runs it, its microVM's (MICROVM_FLAGS) or one it does not serve
        // yet that asks for nothing (its default), shows its help, or says it does not
        // serve it yet.
        if want.0.contains(WINDOWS_ONLY) || want.1.contains(WINDOWS_ONLY) {
            assert!(
                refusal || stdout.starts_with("RUN") || stdout.contains("Usage:"),
                "{argv:?}: {stdout:?} {stderr:?}"
            );
            continue;
        }
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
        let shown = without_microvm_flags(without_extensions(&argv, &stdout));
        let wanted = without_microvm_flags(want.0);
        assert_eq!(
            (shown.as_str(), stderr.as_str(), u64::from(status)),
            (wanted.as_str(), want.1, want.2),
            "{argv:?}"
        );
    }
    assert!(refused > 0, "some cases ask for what shards does not serve");
}
