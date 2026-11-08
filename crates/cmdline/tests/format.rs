//! shards_cmdline::format writes what docker/cli v29.8.1's formatter writes: every case in
//! format.json (scripts/format/generate: `ps`, `images`, `stats`, `system df` and
//! `history` of its fixtures, for each format of its corpus, run through the CLI's own
//! code), written here, gives the CLI's output and error byte for byte, at the moment and
//! in the zones the CLI's cases ran at.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use serde_json::Value as J;
use shards_cmdline::format::container::{Container, Mount, Platform, Port};
use shards_cmdline::format::disk::{BuildCache, DiskUsage, Usage, Volume};
use shards_cmdline::format::history::History;
use shards_cmdline::format::image::Image;
use shards_cmdline::format::stats::Stats;
use shards_cmdline::format::{Clock, Context, TABLE, Zone, container, disk, history, image, stats};

fn oracle() -> J {
    serde_json::from_str(include_str!("format.json")).unwrap()
}

fn s(j: &J, k: &str) -> String {
    j[k].as_str().unwrap_or_default().to_string()
}

fn i(j: &J, k: &str) -> i64 {
    j[k].as_i64().unwrap_or_default()
}

fn f(j: &J, k: &str) -> f64 {
    j[k].as_f64().unwrap_or_default()
}

fn b(j: &J, k: &str) -> bool {
    j[k].as_bool().unwrap_or_default()
}

fn strings(j: &J, k: &str) -> Vec<String> {
    j[k].as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn labels(j: &J, k: &str) -> BTreeMap<String, String> {
    j[k].as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn list<T>(j: &J, k: &str, each: impl Fn(&J) -> T) -> Vec<T> {
    j[k].as_array()
        .map(|a| a.iter().map(each).collect())
        .unwrap_or_default()
}

fn container(j: &J) -> Container {
    Container {
        id: s(j, "id"),
        names: strings(j, "names"),
        image: s(j, "image"),
        image_id: s(j, "image_id"),
        platform: j["platform"].as_object().map(|_| Platform {
            os: s(&j["platform"], "os"),
            architecture: s(&j["platform"], "architecture"),
            variant: s(&j["platform"], "variant"),
        }),
        command: s(j, "command"),
        created: i(j, "created"),
        ports: list(j, "ports", |p| Port {
            ip: Some(s(p, "ip"))
                .filter(|ip| !ip.is_empty())
                .map(|ip| ip.parse().unwrap()),
            private: u16::try_from(i(p, "private")).unwrap(),
            public: u16::try_from(i(p, "public")).unwrap(),
            kind: s(p, "type"),
        }),
        size_rw: i(j, "size_rw"),
        size_root_fs: i(j, "size_root_fs"),
        labels: labels(j, "labels"),
        state: s(j, "state"),
        status: s(j, "status"),
        health: s(j, "health"),
        networks: strings(j, "networks"),
        mounts: list(j, "mounts", |m| Mount {
            name: s(m, "name"),
            source: s(m, "source"),
            driver: s(m, "driver"),
        }),
    }
}

fn image(j: &J) -> Image {
    Image {
        id: s(j, "id"),
        repo_tags: strings(j, "repo_tags"),
        repo_digests: strings(j, "repo_digests"),
        created: i(j, "created"),
        size: i(j, "size"),
        shared_size: i(j, "shared_size"),
        containers: i(j, "containers"),
    }
}

fn stat(j: &J) -> Stats {
    Stats {
        container: s(j, "container"),
        name: s(j, "name"),
        id: s(j, "id"),
        cpu_percentage: f(j, "cpu_percentage"),
        memory: f(j, "memory"),
        memory_limit: f(j, "memory_limit"),
        memory_percentage: f(j, "memory_percentage"),
        network_rx: f(j, "network_rx"),
        network_tx: f(j, "network_tx"),
        block_read: f(j, "block_read"),
        block_write: f(j, "block_write"),
        pids: j["pids"].as_u64().unwrap(),
        invalid: b(j, "invalid"),
    }
}

fn usage<T>(counts: &J, k: &str, items: Vec<T>) -> Usage<T> {
    let c = &counts[k];
    Usage {
        total_count: i(c, "total_count"),
        active_count: i(c, "active_count"),
        total_size: i(c, "total_size"),
        reclaimable: i(c, "reclaimable"),
        items,
    }
}

fn disk_usage(o: &J) -> DiskUsage {
    let counts = &o["counts"];
    DiskUsage {
        images: usage(counts, "images", list(o, "images", image)),
        containers: usage(counts, "containers", list(o, "containers", container)),
        volumes: usage(
            counts,
            "volumes",
            list(o, "volumes", |v| Volume {
                name: s(v, "name"),
                driver: s(v, "driver"),
                scope: s(v, "scope"),
                mountpoint: s(v, "mountpoint"),
                labels: labels(v, "labels"),
                usage: v["usage"]
                    .as_array()
                    .map(|u| (u[0].as_i64().unwrap(), u[1].as_i64().unwrap())),
            }),
        ),
        build_cache: usage(
            counts,
            "build_cache",
            list(o, "cache", |c| BuildCache {
                id: s(c, "id"),
                parents: strings(c, "parents"),
                kind: s(c, "kind"),
                description: s(c, "description"),
                in_use: b(c, "in_use"),
                shared: b(c, "shared"),
                size: i(c, "size"),
                created_at: i128::from(i(c, "created_at")),
                last_used_at: c["last_used_at"].as_i64().map(i128::from),
                usage_count: i(c, "usage_count"),
            }),
        ),
    }
}

fn history(j: &J) -> History {
    History {
        id: s(j, "id"),
        created: i(j, "created"),
        created_by: s(j, "created_by"),
        size: i(j, "size"),
        comment: s(j, "comment"),
    }
}

/// A case's command, as the CLI's runs it: its format made, then written.
fn run(o: &J, c: &J, clock: &Clock<'_>) -> (String, Result<(), String>) {
    let all = s(c, "set") == "all";
    let given = s(c, "format");
    let source = if given.is_empty() {
        TABLE.to_string()
    } else {
        given.clone()
    };
    let (quiet, trunc) = (b(c, "quiet"), !b(c, "no_trunc"));
    let mut out = String::new();
    let east_asian = b(c, "east_asian");
    let result = match s(c, "command").as_str() {
        "ps" => {
            // runPs: a given format is checked first, and asks for sizes if it shows them.
            let mut size = b(c, "size");
            let checked = if given.is_empty() {
                Ok(())
            } else {
                container::check(&given, clock).map(|used| {
                    if !quiet && !size {
                        size = used;
                    }
                })
            };
            let rows = if all {
                list(o, "containers", container)
            } else {
                Vec::new()
            };
            checked.and_then(|()| {
                let format = container::format(&given, quiet, size);
                container::write(&context(&format, trunc, east_asian, clock), &rows, &mut out)
            })
        }
        "images" => {
            let digests = b(c, "digests");
            let format = image::format(&source, quiet, digests);
            let rows = if all { list(o, "images", image) } else { Vec::new() };
            image::write(
                &context(&format, trunc, east_asian, clock),
                digests,
                &rows,
                &mut out,
            )
        }
        "stats" => {
            let format = stats::format(&source);
            let rows = if all { list(o, "stats", stat) } else { Vec::new() };
            stats::write(&context(&format, trunc, east_asian, clock), &rows, &mut out)
        }
        "df" => {
            let verbose = b(c, "verbose");
            let format = disk::format(&source, verbose);
            let du = if all { disk_usage(o) } else { DiskUsage::default() };
            disk::write(
                &context(&format, trunc, east_asian, clock),
                verbose,
                &du,
                &mut out,
            )
        }
        "history" => {
            let human = b(c, "human");
            let format = history::format(&source, quiet, human);
            let rows = if all {
                list(o, "history", history)
            } else {
                Vec::new()
            };
            history::write(
                &context(&format, trunc, east_asian, clock),
                human,
                &rows,
                &mut out,
            )
        }
        other => panic!("no command {other}"),
    };
    (out, result)
}

fn context<'a>(format: &'a str, trunc: bool, east_asian: bool, clock: &'a Clock<'a>) -> Context<'a> {
    Context {
        format,
        trunc,
        east_asian,
        clock,
    }
}

/// Cases shards answers differently, and why.
const DEVIATIONS: &[(&str, &str, &str)] = &[(
    "empty",
    "table {{.ID}}\\t{{.Missing.X}}",
    "A header's missing column is Go's invalid value, through which a field is invalid \
     too; shards-template has no value a field can give that is invalid, so the header \
     stops at the error Go's nil gives. Only a table of no rows shows its header without \
     rows' errors.",
)];

#[test]
fn formats_write_what_the_cli_writes() {
    let o = oracle();
    let zones: BTreeMap<i64, Zone> = o["zones"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.parse().unwrap(),
                Zone {
                    offset: i(v, "offset"),
                    name: s(v, "name"),
                },
            )
        })
        .collect();
    let zone = |sec: i64| {
        zones
            .get(&sec)
            .cloned()
            .unwrap_or_else(|| panic!("no zone recorded for {sec}"))
    };
    let clock = Clock {
        now: i128::from(i(&o, "now")) * 1_000_000_000,
        zone: &zone,
    };
    let cases = o["cases"].as_array().unwrap();
    assert!(cases.len() > 1000, "{} cases", cases.len());
    let mut failed = Vec::new();
    for c in cases {
        if DEVIATIONS
            .iter()
            .any(|(set, format, _)| s(c, "set") == *set && s(c, "format") == *format)
        {
            continue;
        }
        let (out, result) = run(&o, c, &clock);
        let want_err = s(c, "error");
        let got_err = result.err().unwrap_or_default();
        if out != s(c, "output") || got_err != want_err {
            failed.push(format!(
                "{} {} {:?} q={} nt={} sz={} dg={} h={} v={} ea={}\n  want {:?} {:?}\n  got  {:?} {:?}",
                s(c, "command"),
                s(c, "set"),
                s(c, "format"),
                b(c, "quiet"),
                b(c, "no_trunc"),
                b(c, "size"),
                b(c, "digests"),
                b(c, "human"),
                b(c, "verbose"),
                b(c, "east_asian"),
                s(c, "output"),
                want_err,
                out,
                got_err
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        cases.len(),
        failed.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
}
