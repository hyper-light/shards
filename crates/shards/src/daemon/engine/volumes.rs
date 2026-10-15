//! Volumes through the API (D170): `shards create volume`, `list volumes`, `inspect
//! volume`, `remove volume` and `prune volumes`. Data that outlives a sandbox, served to
//! each microVM that mounts it by a share process of its own run, every request of the
//! guest's taken as one it could forge, the VM process reaching no host directory (D38).

use serde_json::{Value, json};

use super::http::Answer;
use super::{Call, refused};
use crate::containers::Disk;
use crate::daemon::{Daemon, Threads};

impl<D: Disk> Daemon<D> {
    /// `GET /volumes`: `shards list volumes`, by the query's filters.
    pub(super) fn engine_volumes(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        match self.volumes_listed(&given, i64::from(crate::cli::utc_offset())) {
            Ok(volumes) => {
                let docs: Vec<Value> = volumes
                    .iter()
                    .filter_map(|v| serde_json::from_str(&crate::daemon::inspect_doc::json(v)).ok())
                    .collect();
                Answer::json(200, &json!({ "Volumes": docs, "Warnings": [] }))
            }
            Err(e) => refused(&e, 400),
        }
    }

    /// `GET /volumes/{name}`: `shards inspect volume`.
    pub(super) fn engine_volume(&self, name: &str) -> Answer {
        self.await_removals();
        match self.volume_doc(name, i64::from(crate::cli::utc_offset())) {
            Some(doc) => Answer::document(200, crate::daemon::inspect_doc::json(&doc)),
            None => Answer::error(404, format!("No such volume: {name}")),
        }
    }

    /// `POST /volumes/create`: `shards create volume`, named, or named for it; its
    /// document as made. A cluster volume is a swarm's, which shards serves none of.
    pub(super) fn engine_volume_create<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        call: &Call<'_>,
    ) -> Answer {
        let body: Value = if call.body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(&call.body) {
                Ok(body) => body,
                Err(e) => return Answer::error(400, format!("a volume's body: {e}")),
            }
        };
        if body.get("ClusterVolumeSpec").is_some_and(|c| !c.is_null()) {
            return Answer::error(
                400,
                "ClusterVolumeSpec: a cluster volume is a swarm's, which shards serves none of",
            );
        }
        let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default();
        let pairs = |k: &str| -> Vec<String> {
            body.get(k)
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut argv: Vec<String> = vec!["volume".into(), "create".into()];
        if !text("Driver").is_empty() {
            argv.extend(["--driver".into(), text("Driver").to_string()]);
        }
        for o in pairs("DriverOpts") {
            argv.extend(["--opt".into(), o]);
        }
        for l in pairs("Labels") {
            argv.extend(["--label".into(), l]);
        }
        if !text("Name").is_empty() {
            argv.push(text("Name").to_string());
        }
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.ask_command(threads, &argv) {
            Ok((0, out, _)) => {
                let name = String::from_utf8_lossy(&out).trim().to_string();
                match self.volume_doc(&name, i64::from(crate::cli::utc_offset())) {
                    Some(doc) => Answer::document(201, crate::daemon::inspect_doc::json(&doc)),
                    None => Answer::error(500, format!("volume {name}: made, and not found after")),
                }
            }
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `DELETE /volumes/{name}`: `shards remove volume`, `force` its `-f`.
    pub(super) fn engine_volume_remove<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        name: &str,
        call: &Call<'_>,
    ) -> Answer {
        let mut argv = vec!["volume", "rm"];
        if call.flag("force") {
            argv.push("--force");
        }
        argv.push(name);
        match self.ask_command(threads, &argv) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /volumes/prune`: `shards prune volumes`. The volumes no microVM mounts that
    /// the filters keep, anonymous ones alone unless `all`: their names, and the bytes
    /// reclaimed, counted.
    pub(super) fn engine_volume_prune(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f,
            Err(e) => return Answer::error(400, e),
        };
        let all = given
            .iter()
            .any(|(k, v)| k == "all" && matches!(v.as_str(), "1" | "true"));
        let rest: Vec<String> = given
            .into_iter()
            .filter(|(k, _)| k != "all")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        match self.prune_volumes(&rest, all) {
            Ok((names, bytes)) => {
                Answer::json(200, &json!({ "VolumesDeleted": names, "SpaceReclaimed": bytes }))
            }
            Err(e) => refused(&e, 400),
        }
    }
}
