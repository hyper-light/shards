//! shards takes every flag the Docker CLI's commands take, served or not (a command's
//! `flags` and `unserved`), as the CLI defines it: by its name and shorthand, the kind of
//! value it takes and its default, and the flags bound to one value with it. The CLI's are
//! docker-flags.json's (scripts/docker-cli/oracle_test.go, TestShardsFlags).

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use shards_cmdline::commands::{self, RUN};
use shards_cmdline::flags::{Flag, Kind};

/// The kind of value a flag of pflag type `go` takes, as an `unserved` line writes it:
/// none (`b`), a number (`i`), one value of any other type (`s`), or any number of
/// values, each kept (`m`).
fn go_kind(go: &str) -> char {
    match go {
        "bool" => 'b',
        "int" | "int64" | "uint" | "uint16" | "uint64" => 'i',
        "filter" | "gpu-request" | "ipNetSlice" | "ipSlice" | "list" | "map" | "mount" | "network"
        | "port" | "stringSlice" | "ulimit" => 'm',
        _ => 's',
    }
}

fn kind(flag: &Flag) -> char {
    match flag.kind {
        Kind::Bool => 'b',
        Kind::Int => 'i',
        Kind::Many(_) => 'm',
        Kind::Value(go) => go_kind(go),
        Kind::String | Kind::Duration => 's',
    }
}

/// A default as pflag writes it, its spellings of nothing as one: an empty list, map or
/// address.
fn default(text: &str) -> &str {
    match text {
        "[]" | "map[]" | "<nil>" => "",
        text => text,
    }
}

/// The groups of `names` bound to one value, by `owner`, the flag that holds each one's.
fn bound<'a>(names: impl Iterator<Item = &'a str>, owner: impl Fn(&'a str) -> &'a str) -> Vec<Vec<&'a str>> {
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in names {
        groups.entry(owner(name)).or_default().push(name);
    }
    let mut groups: Vec<Vec<&str>> = groups.into_values().filter(|g| g.len() > 1).collect();
    for group in &mut groups {
        group.sort_unstable();
    }
    groups.sort_unstable();
    groups
}

#[test]
fn shards_takes_every_flag_of_its_commands() {
    let golden: serde_json::Value = serde_json::from_str(include_str!("docker-flags.json")).unwrap();
    let mut wrong = Vec::new();
    for (path, flags) in golden.as_object().unwrap() {
        let words: Vec<&str> = path.split(' ').collect();
        let command = match words.as_slice() {
            ["run"] | ["container", "run"] => &RUN,
            _ => match commands::find(&words) {
                Some((command, _, _)) => command,
                // A command shards does not have.
                None => continue,
            },
        };
        let mut ours: BTreeMap<&str, Flag> = BTreeMap::new();
        for flag in command.every_flag().filter(|f| f.name != "help" && !f.extension) {
            if ours.insert(flag.name, flag).is_some() {
                wrong.push(format!("{path}: --{} is listed twice", flag.name));
            }
        }
        let theirs: BTreeMap<&str, &serde_json::Value> = flags
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["name"].as_str().unwrap(), f))
            .collect();
        for name in ours.keys().filter(|n| !theirs.contains_key(*n)) {
            wrong.push(format!("{path}: --{name} is not the CLI's"));
        }
        for (name, theirs) in &theirs {
            let Some(flag) = ours.get(name) else {
                wrong.push(format!("{path}: --{name} is missing"));
                continue;
            };
            let short = theirs["short"].as_str().and_then(|s| s.bytes().next());
            let go = theirs["type"].as_str().unwrap();
            let theirs_default = theirs["default"].as_str().unwrap();
            // A default the CLI's help prints, shards' prints too (command_lines.rs); one that
            // shards does not serve it takes alone, refusing any other value.
            let defaults = flag.supported || default(flag.default) == default(theirs_default);
            if flag.short != short || kind(flag) != go_kind(go) || !defaults {
                wrong.push(format!(
                    "{path}: --{name}: shards {:?} {} {:?}, the CLI {:?} {go} {theirs_default:?}",
                    flag.short.map(char::from),
                    kind(flag),
                    flag.default,
                    short.map(char::from),
                ));
            }
        }
        // The flags bound to one value: in the CLI each names the first one defined, in
        // shards the one it sets too.
        let ours_bound = bound(ours.keys().copied(), |name| {
            ours.get(name).and_then(|f| f.shares).unwrap_or(name)
        });
        let theirs_bound = bound(theirs.keys().copied(), |name| {
            theirs
                .get(name)
                .and_then(|f| f["shares"].as_str())
                .unwrap_or(name)
        });
        if ours_bound != theirs_bound {
            wrong.push(format!(
                "{path}: flags bound to one value: shards {ours_bound:?}, the CLI {theirs_bound:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
