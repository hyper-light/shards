//! shards reads `logs --since` and `--until` as the Docker client and dockerd do: every
//! answer in docker-time.json (scripts/docker-cli/time), shards gives byte for byte.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::gotime::{get_timestamp, parse_unix_timestamp};

#[test]
fn timestamps_are_read_as_the_docker_client_and_dockerd_read_them() {
    let golden: serde_json::Value = serde_json::from_str(include_str!("docker-time.json")).unwrap();
    let now = i128::from(golden["now_ns"].as_i64().unwrap());
    let cases = golden["since"].as_array().unwrap();
    assert!(cases.len() > 150);
    for case in cases {
        let value = case["value"].as_str().unwrap();
        let offset = case["offset"].as_i64().unwrap();
        let want = match (case.get("result"), case.get("error")) {
            (Some(r), _) => Ok(r.as_str().unwrap().to_string()),
            (None, Some(e)) => Err(e.as_str().unwrap().to_string()),
            (None, None) => Ok(String::new()),
        };
        assert_eq!(get_timestamp(value, now, offset), want, "{value:?} at {offset}");
    }
    for case in golden["unix"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let want = match (case.get("ns"), case.get("error")) {
            (Some(ns), _) => Ok(Some(i128::from(ns.as_i64().unwrap()))),
            (None, Some(e)) => Err(e.as_str().unwrap().to_string()),
            (None, None) => Ok(None),
        };
        assert_eq!(parse_unix_timestamp(value), want, "{value:?}");
    }
}
