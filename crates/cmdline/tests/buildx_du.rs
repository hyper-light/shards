//! shards-cmdline answers `buildx du` as buildx does: for every case of buildx-du.json
//! (scripts/buildx/du_test.go, which ran buildx's runDiskUsage against a BuildKit that
//! answered with fixed records), shards prints the same, fails with the same error, and
//! asks for the same filters.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::buildcache::prune_info;
use shards_cmdline::format::{Clock, Context, du, utc};

const NANOS: i128 = 1_000_000_000;

#[test]
fn du_answers_as_buildx_does() {
    let data: serde_json::Value = serde_json::from_str(include_str!("buildx-du.json")).unwrap();
    // Any moment: what the records say of their last use is relative to it.
    let now = 1_800_000_000 * NANOS;
    let strings = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    };
    let records: Vec<du::Usage> = data["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| du::Usage {
            id: r["id"].as_str().unwrap().into(),
            parents: strings(&r["parents"]),
            created_at: shards_cmdline::gotime::parse_timestamp(r["created_at"].as_str().unwrap(), 0, 0)
                .unwrap(),
            mutable: r["mutable"].as_bool().unwrap(),
            in_use: r["in_use"].as_bool().unwrap(),
            shared: r["shared"].as_bool().unwrap(),
            size: r["size"].as_i64().unwrap(),
            description: r["description"].as_str().unwrap().into(),
            usage_count: r["usage_count"].as_i64().unwrap(),
            last_used_at: r["last_used_ago"].as_i64().map(|s| now - i128::from(s) * NANOS),
            kind: r["type"].as_str().unwrap().into(),
        })
        .collect();
    let mut failures = Vec::new();
    for c in data["cases"].as_array().unwrap() {
        let chosen: Vec<du::Usage> = c["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| records[i.as_u64().unwrap() as usize].clone())
            .collect();
        // runDiskUsage: the format, then the filters, then what BuildKit answers laid out.
        let (got, err, asked) =
            match du::format(c["format"].as_str().unwrap(), c["verbose"].as_bool().unwrap()) {
                Err(e) => (String::new(), e, Vec::new()),
                Ok(format) => match prune_info(&strings(&c["filters"])) {
                    Err(e) => (String::new(), e, Vec::new()),
                    Ok(info) => {
                        let clock = Clock { now, zone: &utc };
                        let ctx = Context {
                            format: &format,
                            trunc: true,
                            east_asian: false,
                            clock: &clock,
                        };
                        let mut out = String::new();
                        let err = du::write(&ctx, &chosen, info.given, &mut out)
                            .err()
                            .unwrap_or_default();
                        let mut asked = info.filters;
                        asked.sort();
                        (out, err, asked)
                    }
                },
            };
        let want = (
            c["stdout"].as_str().unwrap(),
            c["error"].as_str().unwrap(),
            strings(&c["asked"]),
        );
        if (got.as_str(), err.as_str(), &asked) != (want.0, want.1, &want.2) {
            failures.push(format!(
                "{} verbose={} filters={}\n  got:  {got:?} {err:?} {asked:?}\n  want: {:?} {:?} {:?}",
                c["format"], c["verbose"], c["filters"], want.0, want.1, want.2
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
