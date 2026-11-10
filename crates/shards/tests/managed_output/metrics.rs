//! Real owner command, guest cgroup observations and independent canonical output.

use super::*;
use serde_json::Value;

fn attrs(value: &Value) -> BTreeMap<&str, &Value> {
    let mut found = BTreeMap::new();
    for attr in value.as_array().unwrap() {
        let key = attr["key"].as_str().unwrap();
        assert!(found.insert(key, &attr["value"]).is_none(), "unique attribute");
    }
    found
}

fn scopes<'a>(document: &'a Value, run: &str) -> BTreeMap<String, &'a Value> {
    let mut found = BTreeMap::new();
    for resource in document["resourceMetrics"].as_array().unwrap() {
        let identity = attrs(&resource["resource"]["attributes"]);
        assert_eq!(identity["shards.microvm.id"]["stringValue"], run);
        let restore = identity["shards.microvm.restore.generation"]["stringValue"]
            .as_str()
            .unwrap();
        restore.parse::<u64>().unwrap();
        let kind = identity["shards.cgroup.scope"]["stringValue"].as_str().unwrap();
        let key = if kind == "member" {
            format!(
                "member/{}",
                identity["shards.member.id"]["intValue"].as_str().unwrap()
            )
        } else {
            assert!(!identity.contains_key("shards.member.id"));
            kind.to_owned()
        };
        assert!(found.insert(key, resource).is_none());
        let [scope] = resource["scopeMetrics"].as_array().unwrap().as_slice() else {
            panic!("one init instrumentation scope");
        };
        assert_eq!(scope["scope"]["name"], "shards.init");
        let metrics = scope["metrics"].as_array().unwrap();
        for metric in metrics {
            let points = metric.get("sum").or_else(|| metric.get("gauge")).unwrap()["dataPoints"]
                .as_array()
                .unwrap();
            assert!(!points.is_empty());
            for point in points {
                let time = point["timeUnixNano"].as_str().unwrap().parse::<u64>().unwrap();
                assert_ne!(time, 0);
                assert!(
                    point.get("startTimeUnixNano").is_none(),
                    "counter birth is unknown"
                );
                assert_ne!(point.get("asInt").is_some(), point.get("asDouble").is_some());
            }
        }
        let tasks = metrics
            .iter()
            .find(|m| m["name"] == "shards.cgroup.task.count")
            .unwrap();
        assert_eq!(tasks["unit"], "{task}");
        assert_eq!(tasks["sum"]["aggregationTemporality"], 2);
        assert_eq!(tasks["sum"]["isMonotonic"], false);
        let tasks = tasks["sum"]["dataPoints"][0]["asInt"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            tasks > 0,
            "this fixture's workload and members remain alive until owner stop"
        );
        let cpu = metrics
            .iter()
            .find(|m| m["name"] == "shards.cgroup.cpu.time")
            .unwrap();
        assert_eq!(cpu["unit"], "s");
        assert_eq!(cpu["sum"]["isMonotonic"], true);
        assert!(
            cpu["sum"]["dataPoints"][0]["asDouble"]
                .as_f64()
                .unwrap()
                .is_finite()
        );
        let memory = metrics
            .iter()
            .find(|m| m["name"] == "shards.cgroup.memory.usage")
            .unwrap();
        assert_eq!(memory["unit"], "By");
        memory["sum"]["dataPoints"][0]["asInt"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            metrics
                .iter()
                .any(|m| m["name"] == "shards.cgroup.collection.error.count")
        );
    }
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        ["member/1", "member/2", "run", "workload"]
    );
    found
}

#[test]
fn cgroup_metrics_use_declared_scopes_without_merging_member_output() {
    let records = std::env::var_os("SHARDS_OTEL_RECORDS").map(PathBuf::from);
    if let Some(path) = &records {
        assert!(path.is_absolute());
        std::fs::create_dir(path).unwrap();
    }
    let save = |name: &str, bytes: &[u8]| {
        if let Some(path) = &records {
            std::fs::write(path.join(name), bytes).unwrap();
        }
    };
    let Some(fixture) = with_fixture(|fixture| {
        let id = fixture.metadata.as_ref().unwrap().run.clone();
        save(
            "managed-output.ready",
            &std::fs::read(fixture.record.as_ref().unwrap()).unwrap(),
        );
        fixture.main_progress(WITHIN);
        let first = fixture.command(&["read", "metrics", &id], WITHIN);
        save("first.json", first.stdout.as_bytes());
        save("first.stderr", first.stderr.as_bytes());
        assert_eq!(first.status, Some(0), "{first}");
        let first: Value = serde_json::from_str(&first.stdout).unwrap();
        scopes(&first, &id);

        let paused = fixture.command(&["pause", &id], WITHIN);
        assert_eq!(paused.status, Some(0), "{paused}");
        let unavailable = fixture.command(&["read", "metrics", &id], WITHIN);
        save("paused.stdout", unavailable.stdout.as_bytes());
        save("paused.stderr", unavailable.stderr.as_bytes());
        assert_ne!(unavailable.status, Some(0), "{unavailable}");
        assert!(
            unavailable.stdout.is_empty(),
            "no fresh cached artifact while paused"
        );
        let resumed = fixture.command(&["unpause", &id], WITHIN);
        assert_eq!(resumed.status, Some(0), "{resumed}");
        fixture.main_progress(WITHIN);
        let second = fixture.command(&["read", "metrics", &id], WITHIN);
        save("second.json", second.stdout.as_bytes());
        save("second.stderr", second.stderr.as_bytes());
        assert_eq!(second.status, Some(0), "{second}");
        let second: Value = serde_json::from_str(&second.stdout).unwrap();
        scopes(&second, &id);
        for (member, label) in [(1, "agent"), (2, "harness")] {
            for channel in [Channel::Stdout, Channel::Stderr] {
                fixture.consume(member, label, channel);
            }
        }
        fixture.main_progress(WITHIN);
    }) else {
        return;
    };
    if let Some(record) = &fixture.record
        && let Ok(bytes) = std::fs::read(record)
    {
        save("managed-output.after-stop", &bytes);
    }
    save("completed", b"fixture body and owner/observer cleanup returned\n");
}

#[test]
fn a_joiners_metrics_distinguish_its_run_from_the_shared_microvm() {
    let records = std::env::var_os("SHARDS_OTEL_JOINER_RECORDS").map(PathBuf::from);
    if let Some(path) = &records {
        assert!(path.is_absolute());
        std::fs::create_dir(path).unwrap();
    }
    let save = |name: &str, bytes: &[u8]| {
        if let Some(path) = &records {
            std::fs::write(path.join(name), bytes).unwrap();
        }
    };
    let Some(_) = with_fixture(|fixture| {
        let provider = fixture.metadata.as_ref().unwrap().run.clone();
        let (image, _) = common::served();
        let joined = fixture.command(
            &[
                "run",
                "vm",
                "-d",
                "--name",
                "metrics-joiner",
                "--network",
                "container:owner-output",
                &image,
                "sleep",
            ],
            WITHIN,
        );
        assert_eq!(joined.status, Some(0), "{joined}");
        let joiner = joined.stdout.trim();
        assert_ne!(joiner, provider);
        save("provider.id", provider.as_bytes());
        save("joiner.id", joiner.as_bytes());

        let boot = |name: &str| {
            let run = fixture.command(
                &[
                    "exec",
                    name,
                    "/bin/testguest",
                    "confined",
                    "cat",
                    "/proc/sys/kernel/random/boot_id",
                ],
                WITHIN,
            );
            save(&format!("{name}-boot.stdout"), run.stdout.as_bytes());
            save(&format!("{name}-boot.stderr"), run.stderr.as_bytes());
            assert_eq!(run.status, Some(0), "{run}");
            let value = run
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix("confined cat /proc/sys/kernel/random/boot_id: "))
                .unwrap();
            assert!(!value.is_empty() && !value.starts_with("errno"));
            value.to_owned()
        };
        assert_eq!(
            boot("owner-output"),
            boot("metrics-joiner"),
            "actual shared guest boot"
        );

        let first = fixture.command(&["read", "metrics", &provider], WITHIN);
        let second = fixture.command(&["read", "metrics", joiner], WITHIN);
        for (name, run) in [("provider", &first), ("joiner", &second)] {
            save(&format!("{name}.json"), run.stdout.as_bytes());
            save(&format!("{name}.stderr"), run.stderr.as_bytes());
            assert_eq!(run.status, Some(0), "{run}");
        }
        let joined: Value = serde_json::from_str(&second.stdout).unwrap();
        let [workload] = joined["resourceMetrics"].as_array().unwrap().as_slice() else {
            panic!("joiner exports only its workload, not provider/member facts");
        };
        let identity = attrs(&workload["resource"]["attributes"]);
        assert_eq!(identity["shards.cgroup.scope"]["stringValue"], "workload");
        assert_eq!(identity["shards.microvm.id"]["stringValue"], provider);
        assert_eq!(identity["shards.run.id"]["stringValue"], joiner);

        let first: Value = serde_json::from_str(&first.stdout).unwrap();
        for resource in first["resourceMetrics"].as_array().unwrap() {
            let identity = attrs(&resource["resource"]["attributes"]);
            assert_eq!(identity["shards.microvm.id"]["stringValue"], provider);
            assert_eq!(identity["shards.run.id"]["stringValue"], provider);
        }
        let removed = fixture.command(&["rm", "-f", joiner], WITHIN);
        assert_eq!(removed.status, Some(0), "{removed}");
        fixture.main_progress(WITHIN);
        for (member, label) in [(1, "agent"), (2, "harness")] {
            for channel in [Channel::Stdout, Channel::Stderr] {
                fixture.consume(member, label, channel);
            }
        }
    }) else {
        return;
    };
    save(
        "completed",
        b"distinct run/shared VM identity and owner cleanup returned\n",
    );
}
