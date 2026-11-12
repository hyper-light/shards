//! Real owner command, guest cgroup observations and independent canonical output.

use super::*;
use serde_json::Value;
use std::collections::BTreeSet;

fn attrs(value: &Value) -> BTreeMap<&str, &Value> {
    let mut found = BTreeMap::new();
    for attr in value.as_array().unwrap() {
        let key = attr["key"].as_str().unwrap();
        assert!(found.insert(key, &attr["value"]).is_none(), "unique attribute");
    }
    found
}

fn limit<'a>(resource: &'a Value, name: &str, unit: &str) -> (&'a str, Option<u64>) {
    let metrics = resource["scopeMetrics"][0]["metrics"].as_array().unwrap();
    let states = metrics
        .iter()
        .find(|metric| metric["name"] == "shards.cgroup.limit.state")
        .unwrap();
    assert_eq!(states["unit"], "1");
    assert!(states.get("sum").is_none());
    let points = states["gauge"]["dataPoints"].as_array().unwrap();
    assert_eq!(
        points.len(),
        2 * 3,
        "two observed ceilings, three typed states each"
    );
    let mut indicators = BTreeMap::new();
    for point in points {
        let attributes = attrs(&point["attributes"]);
        assert_eq!(attributes.len(), 2);
        if attributes["shards.cgroup.metric"]["stringValue"] != name {
            continue;
        }
        let state = attributes["shards.cgroup.limit.state"]["stringValue"]
            .as_str()
            .unwrap();
        let value = point["asInt"].as_str().unwrap();
        assert!(matches!(value, "0" | "1"));
        assert!(indicators.insert(state, value).is_none());
    }
    assert_eq!(
        indicators.keys().copied().collect::<Vec<_>>(),
        ["finite", "unavailable", "unbounded"]
    );
    let mut active = indicators.iter().filter(|(_, value)| **value == "1");
    let (&state, _) = active.next().unwrap();
    assert!(active.next().is_none(), "exactly one current state per ceiling");
    let numeric = metrics.iter().find(|metric| metric["name"] == name);
    let numeric = numeric.map(|metric| {
        assert_eq!(metric["unit"], unit);
        assert!(metric.get("sum").is_none());
        let [point] = metric["gauge"]["dataPoints"].as_array().unwrap().as_slice() else {
            panic!("one actual local ceiling");
        };
        let value = point["asInt"].as_str().unwrap().parse::<i64>().unwrap();
        u64::try_from(value).unwrap()
    });
    if state != "finite" {
        assert!(numeric.is_none(), "max/error must not invent a numeric ceiling");
    }
    (state, numeric)
}

fn workload_limits(resource: &Value, kernel: &str) {
    for (file, name, unit) in [
        ("memory.max", "shards.cgroup.memory.limit", "By"),
        ("pids.max", "shards.cgroup.task.limit", "{task}"),
    ] {
        let prefix = format!("slurp /sys/fs/cgroup/{file}: ");
        let actual = kernel
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap();
        let observed = limit(resource, name, unit);
        if actual == "max" {
            assert_eq!(observed, ("unbounded", None));
        } else {
            assert_eq!(observed, ("finite", Some(actual.parse::<u64>().unwrap())));
        }
    }
}

fn events(resource: &Value) -> BTreeMap<String, u64> {
    let metrics = resource["scopeMetrics"][0]["metrics"].as_array().unwrap();
    let mut found = BTreeMap::new();
    for (family, keys) in [
        (
            "memory",
            ["low", "high", "max", "oom", "oom_kill", "oom_group_kill"].as_slice(),
        ),
        ("task", ["max"].as_slice()),
    ] {
        let name = format!("shards.cgroup.{family}.events");
        let metric = metrics.iter().find(|metric| metric["name"] == name).unwrap();
        assert_eq!(metric["unit"], "{event}");
        assert_eq!(metric["sum"]["aggregationTemporality"], 2);
        assert_eq!(metric["sum"]["isMonotonic"], true);
        let mut actual = BTreeSet::new();
        for point in metric["sum"]["dataPoints"].as_array().unwrap() {
            let attributes = attrs(&point["attributes"]);
            assert_eq!(attributes.len(), 1);
            let event = attributes["shards.cgroup.event"]["stringValue"].as_str().unwrap();
            assert!(actual.insert(event));
            let value = point["asInt"].as_str().unwrap().parse::<i64>().unwrap();
            assert!(
                found
                    .insert(format!("{family}.{event}"), u64::try_from(value).unwrap())
                    .is_none()
            );
        }
        assert_eq!(actual, keys.iter().copied().collect());
    }
    found
}

fn kernel_events(kernel: &str) -> BTreeMap<String, u64> {
    let mut found = BTreeMap::new();
    for (file, family) in [("memory.events", "memory"), ("pids.events", "task")] {
        let header = format!("== /sys/fs/cgroup/{file}\n");
        let (_, body) = kernel.split_once(&header).unwrap();
        for line in body.split("== ").next().unwrap().lines() {
            let mut fields = line.split_whitespace();
            let key = fields.next().unwrap();
            let value = fields.next().unwrap().parse::<u64>().unwrap();
            assert!(fields.next().is_none());
            assert!(found.insert(format!("{family}.{key}"), value).is_none());
        }
    }
    found
}

fn scopes<'a>(document: &'a Value, run: &str) -> BTreeMap<String, &'a Value> {
    let mut found = BTreeMap::new();
    for resource in document["resourceMetrics"].as_array().unwrap() {
        let identity = attrs(&resource["resource"]["attributes"]);
        assert_eq!(identity["shards.microvm.id"]["stringValue"], run);
        assert_eq!(identity["shards.run.id"]["stringValue"], run);
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
        limit(resource, "shards.cgroup.memory.limit", "By");
        limit(resource, "shards.cgroup.task.limit", "{task}");
        events(resource);
    }
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        ["member/1", "member/2", "run", "workload"]
    );
    found
}

#[test]
fn task_refusals_export_the_actual_kernel_event_counter() {
    let records = std::env::var_os("SHARDS_OTEL_EVENTS_RECORDS").map(PathBuf::from);
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
        fixture.main_progress(WITHIN);
        let collect = |phase: &str| {
            let exported = fixture.command(&["read", "metrics", &id], WITHIN);
            save(&format!("{phase}.json"), exported.stdout.as_bytes());
            save(&format!("{phase}.stderr"), exported.stderr.as_bytes());
            assert_eq!(exported.status, Some(0), "{exported}");
            let document: Value = serde_json::from_str(&exported.stdout).unwrap();
            scopes(&document, &id);
            document
        };
        let kernel = |phase: &str| {
            let read = fixture.command(
                &[
                    "exec",
                    "owner-output",
                    "/bin/testguest",
                    "files",
                    "/sys/fs/cgroup/memory.events",
                    "/sys/fs/cgroup/pids.events",
                ],
                WITHIN,
            );
            save(&format!("{phase}-kernel.stdout"), read.stdout.as_bytes());
            save(&format!("{phase}-kernel.stderr"), read.stderr.as_bytes());
            assert_eq!(read.status, Some(0), "{read}");
            kernel_events(&read.stdout)
        };
        let before_kernel = kernel("before");
        let before = collect("before");
        let workload = scopes(&before, &id)["workload"];
        let before_events = events(workload);
        let tasks = workload["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["name"] == "shards.cgroup.task.count")
            .unwrap()["sum"]["dataPoints"][0]["asInt"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let update = fixture.command(&["update", "--pids-limit", &tasks.to_string(), &id], WITHIN);
        save("finite-update.stdout", update.stdout.as_bytes());
        save("finite-update.stderr", update.stderr.as_bytes());
        assert_eq!(update.status, Some(0), "{update}");
        let finite = collect("finite");
        assert_eq!(
            limit(
                scopes(&finite, &id)["workload"],
                "shards.cgroup.task.limit",
                "{task}"
            ),
            ("finite", Some(tasks))
        );
        // The owner exec may enter a full group; its own child creation must not.
        let refused = fixture.command(
            &[
                "exec",
                "owner-output",
                "/bin/testguest",
                "spawn",
                "streams",
                "event-child-ran",
                "event-child-stderr",
            ],
            WITHIN,
        );
        save("refused.stdout", refused.stdout.as_bytes());
        save("refused.stderr", refused.stderr.as_bytes());
        let after = collect("after-refusal");
        let after_events = events(scopes(&after, &id)["workload"]);
        let reset = fixture.command(&["update", "--pids-limit", "-1", &id], WITHIN);
        save("reset.stdout", reset.stdout.as_bytes());
        save("reset.stderr", reset.stderr.as_bytes());
        assert_eq!(reset.status, Some(0), "{reset}");
        let after_kernel = kernel("after");
        let resumed = fixture.command(
            &[
                "exec",
                "owner-output",
                "/bin/testguest",
                "spawn",
                "streams",
                "event-child-ran",
                "event-child-stderr",
            ],
            WITHIN,
        );
        save("resumed.stdout", resumed.stdout.as_bytes());
        save("resumed.stderr", resumed.stderr.as_bytes());
        assert_eq!(refused.status, Some(1), "{refused}");
        assert!(refused.stdout.starts_with("spawn error "), "{refused}");
        assert!(!refused.stdout.contains("event-child-ran"), "{refused}");
        assert_eq!(resumed.status, Some(0), "{resumed}");
        assert!(resumed.stdout.contains("event-child-ran"), "{resumed}");
        assert!(resumed.stderr.contains("event-child-stderr"), "{resumed}");
        assert!(after_events["task.max"] > before_events["task.max"]);
        for (key, value) in before_events {
            assert!(before_kernel[&key] <= value);
            assert!(value <= after_events[&key]);
            assert!(after_events[&key] <= after_kernel[&key]);
        }
        for (member, label) in [(1, "agent"), (2, "harness")] {
            for channel in [Channel::Stdout, Channel::Stderr] {
                fixture.consume(member, label, channel);
            }
        }
        fixture.main_progress(WITHIN);
    }) else {
        return;
    };
    if let Some(record) = &fixture.record {
        save("managed-output.after-stop", &std::fs::read(record).unwrap());
    }
    save("completed", b"event case and owner/observer cleanup returned\n");
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
        let kernel = |phase: &str| {
            let read = fixture.command(
                &[
                    "exec",
                    "owner-output",
                    "/bin/testguest",
                    "slurp",
                    "/sys/fs/cgroup/memory.max",
                    "/sys/fs/cgroup/pids.max",
                ],
                WITHIN,
            );
            save(&format!("{phase}-kernel-limits.stdout"), read.stdout.as_bytes());
            save(&format!("{phase}-kernel-limits.stderr"), read.stderr.as_bytes());
            assert_eq!(read.status, Some(0), "{read}");
            read.stdout
        };
        let initial_kernel = kernel("initial");
        let first = fixture.command(&["read", "metrics", &id], WITHIN);
        save("first.json", first.stdout.as_bytes());
        save("first.stderr", first.stderr.as_bytes());
        assert_eq!(first.status, Some(0), "{first}");
        let first: Value = serde_json::from_str(&first.stdout).unwrap();
        let first_scopes = scopes(&first, &id);
        workload_limits(first_scopes["workload"], &initial_kernel);

        let paused = fixture.command(&["pause", &id], WITHIN);
        assert_eq!(paused.status, Some(0), "{paused}");
        let unavailable = fixture.command(&["read", "metrics", &id], WITHIN);
        save("paused.stdout", unavailable.stdout.as_bytes());
        save("paused.stderr", unavailable.stderr.as_bytes());
        assert_eq!(unavailable.status, Some(1), "{unavailable}");
        assert!(
            unavailable.stderr.contains(&format!(
                "shards: read metrics: {id} is paused: its metrics can be read once it is unpaused"
            )),
            "{unavailable}"
        );
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
        let second_scopes = scopes(&second, &id);
        workload_limits(second_scopes["workload"], &initial_kernel);
        for (scope, before) in &first_scopes {
            assert_eq!(before["resource"], second_scopes[scope]["resource"]);
        }
        let tasks = first_scopes["workload"]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["name"] == "shards.cgroup.task.count")
            .unwrap()["sum"]["dataPoints"][0]["asInt"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        // Fixture input: twice its observed live task population; no runtime policy.
        let finite = tasks.checked_mul(2).unwrap();
        for (phase, requested, expected) in [
            ("finite", finite.to_string(), Some(finite)),
            ("unbounded", "-1".to_owned(), None),
        ] {
            let update = fixture.command(&["update", "--pids-limit", &requested, &id], WITHIN);
            save(&format!("{phase}-update.stdout"), update.stdout.as_bytes());
            save(&format!("{phase}-update.stderr"), update.stderr.as_bytes());
            assert_eq!(update.status, Some(0), "{update}");
            let actual_kernel = kernel(phase);
            let exported = fixture.command(&["read", "metrics", &id], WITHIN);
            save(&format!("{phase}.json"), exported.stdout.as_bytes());
            save(&format!("{phase}.stderr"), exported.stderr.as_bytes());
            assert_eq!(exported.status, Some(0), "{exported}");
            let document: Value = serde_json::from_str(&exported.stdout).unwrap();
            let observed = scopes(&document, &id);
            workload_limits(observed["workload"], &actual_kernel);
            assert_eq!(
                limit(observed["workload"], "shards.cgroup.task.limit", "{task}"),
                (phase, expected)
            );
            for (scope, before) in &first_scopes {
                assert_eq!(before["resource"], observed[scope]["resource"]);
            }
        }
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
                    "slurp",
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
                .find_map(|line| line.strip_prefix("slurp /proc/sys/kernel/random/boot_id: "))
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
        limit(workload, "shards.cgroup.memory.limit", "By");
        limit(workload, "shards.cgroup.task.limit", "{task}");

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
