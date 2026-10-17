//! The volumes a microVM's own engine makes (D181): each that microVM's own, in the host's
//! store on disk, found by the name its engine gave it whatever it is named on the host,
//! where it is named after the microVM as its containers are; listed, inspected, made,
//! removed and pruned in the calls Compose and Docker's SDKs make, and answered in the
//! shapes they read with no path of the host's.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::Call;
use super::http::Answer;
use super::scope::Scope;
use crate::containers::Disk;
use crate::daemon::{Daemon, lock};
use crate::filters::Filters;
use crate::volumes::{Store, Volume};

/// The name a microVM's engine gives volume `v`: the one it was made by, or, for one made
/// for a container alone, the one it was given.
fn name_there(v: &Volume) -> &str {
    if v.there.is_empty() { &v.name } else { &v.there }
}

impl<D: Disk> Daemon<D> {
    /// The volumes microVM `scope`'s engine made.
    fn own_volumes(&self, scope: &Scope) -> Vec<Volume> {
        Store::new(&self.home)
            .list()
            .into_iter()
            .filter(|v| v.of == scope.provider)
            .collect()
    }

    /// The one of microVM `scope`'s volumes its engine calls `given`.
    fn own_volume_named(&self, scope: &Scope, given: &str) -> Option<Volume> {
        self.own_volumes(scope)
            .into_iter()
            .find(|v| name_there(v) == given)
    }

    /// Volume `v` as the shape its engine's clients read: by its name there, at no path of
    /// the host's, which is nothing of the microVM's.
    fn own_volume_value(&self, v: &Volume) -> Value {
        let doc = self.volume_value(v, i64::from(crate::cli::utc_offset()), None);
        let mut doc: Value =
            serde_json::from_str(&crate::daemon::inspect_doc::json(&doc)).unwrap_or_default();
        if let Some(o) = doc.as_object_mut() {
            o.insert("Name".into(), json!(name_there(v)));
            o.insert("Mountpoint".into(), json!(""));
        }
        doc
    }

    /// `GET /volumes` of a microVM's own engine: its own, by the filters its client gives
    /// (`name`, `label` and `label!`, `driver`, `dangling`).
    pub(super) fn scoped_volumes(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let filters = Filters::from_flags(&given);
        if let Err(e) = filters.validate(&["dangling", "name", "driver", "label", "label!"]) {
            return Answer::error(400, e);
        }
        let dangling = if filters.contains("dangling") {
            match filters.bool_or("dangling", false) {
                Ok(d) => Some(d),
                Err(e) => return Answer::error(400, e),
            }
        } else {
            None
        };
        self.await_removals();
        let names = filters.matcher("name");
        let listed: Vec<Value> = self
            .own_volumes(scope)
            .iter()
            .filter(|_| filters.exact("driver", "local"))
            .filter(|v| names.matches(name_there(v)))
            .filter(|v| {
                filters.kv("label", &v.labels)
                    && !(filters.contains("label!") && filters.kv("label!", &v.labels))
            })
            .filter(|v| dangling.is_none_or(|d| self.volume_refs(&v.name).is_empty() == d))
            .map(|v| self.own_volume_value(v))
            .collect();
        Answer::json(200, &json!({ "Volumes": listed, "Warnings": [] }))
    }

    /// `GET /volumes/{name}` of a microVM's own engine.
    pub(super) fn scoped_volume(&self, given: &str, scope: &Scope) -> Answer {
        self.await_removals();
        match self.own_volume_named(scope, given) {
            Some(v) => Answer::json(200, &self.own_volume_value(&v)),
            None => Answer::error(404, format!("no volume of this microVM's is named {given}")),
        }
    }

    /// `POST /volumes/create` of a microVM's own engine: its own volume of the name, made
    /// where it is not there, after its microVM's name on the host; one with no name made
    /// for none, named at random. The local driver alone, and no driver options, which
    /// would bind a directory of its engine's host.
    pub(super) fn scoped_volume_create(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let body: Value = if call.body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(&call.body) {
                Ok(body) => body,
                Err(e) => return Answer::error(400, format!("a volume's body: {e}")),
            }
        };
        let text = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let name = text("Name");
        if body.get("ClusterVolumeSpec").is_some_and(|c| !c.is_null()) {
            return Answer::error(
                400,
                "ClusterVolumeSpec: a cluster volume is a swarm's, which no microVM's engine is",
            );
        }
        if !matches!(text("Driver").as_str(), "" | "local") {
            return Answer::error(
                400,
                format!(
                    "volume {name}: driver {}: a microVM's own volumes are kept by shards itself (local)",
                    text("Driver")
                ),
            );
        }
        if body
            .get("DriverOpts")
            .and_then(Value::as_object)
            .is_some_and(|o| !o.is_empty())
        {
            return Answer::error(
                400,
                format!(
                    "volume {name}: a microVM's own volume takes no driver options: the local driver's bind a directory of its engine's host"
                ),
            );
        }
        let labels: BTreeMap<String, String> = body
            .get("Labels")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(k) = labels
            .keys()
            .find(|k| shards_agentfile::agentfile::reserved_label(k.as_bytes()))
        {
            return Answer::error(400, crate::volumes::reserved(k));
        }
        if !name.is_empty()
            && let Err(e) = Store::valid(&name)
        {
            return Answer::error(400, format!("volume {name}: {e}"));
        }
        let prefix = lock(&self.containers)
            .get(&scope.provider)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let made = {
            let _held = crate::volumes::lock();
            let store = Store::new(&self.home);
            if name.is_empty() {
                store.create("", &labels, &BTreeMap::new(), &scope.provider)
            } else {
                store.own(&scope.provider, &prefix, &name, &labels, &BTreeMap::new())
            }
        };
        match made {
            Ok((v, _)) => Answer::json(201, &self.own_volume_value(&v)),
            Err(e) => Answer::error(400, format!("volume {name}: {e}")),
        }
    }

    /// `DELETE /volumes/{name}` of a microVM's own engine: its own, none of its containers
    /// mounting it; with `force`, one not there is no error.
    pub(super) fn scoped_volume_remove(&self, given: &str, call: &Call<'_>, scope: &Scope) -> Answer {
        self.await_removals();
        let _held = crate::volumes::lock();
        let Some(v) = self.own_volume_named(scope, given) else {
            if call.flag("force") {
                return Answer::text(204, "");
            }
            return Answer::error(404, format!("no volume of this microVM's is named {given}"));
        };
        let refs = self.volume_refs(&v.name);
        if !refs.is_empty() {
            let registry = lock(&self.containers);
            let names: Vec<String> = refs
                .iter()
                .map(|id| {
                    registry
                        .get(id)
                        .and_then(|c| c.engine_made.as_ref().map(|m| m.name.clone()))
                        .unwrap_or_else(|| id.chars().take(12).collect())
                })
                .collect();
            return Answer::error(409, format!("volume {given}: in use by {}", names.join(", ")));
        }
        match Store::new(&self.home).remove(&v.name) {
            Ok(()) => Answer::text(204, ""),
            Err(e) => Answer::error(500, format!("volume {given}: {e}")),
        }
    }

    /// `POST /volumes/prune` of a microVM's own engine: its own volumes none of its
    /// containers mounts that the filters keep (`label`, `label!`), those made for none
    /// alone unless `all`; their names there, and the bytes their files took.
    pub(super) fn scoped_volume_prune(&self, call: &Call<'_>, scope: &Scope) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let filters = Filters::from_flags(&given);
        if let Err(e) = filters.validate(&["label", "label!", "all"]) {
            return Answer::error(400, e);
        }
        let all = match filters.bool_or("all", false) {
            Ok(all) => all,
            Err(e) => return Answer::error(400, e),
        };
        self.await_removals();
        let _held = crate::volumes::lock();
        let store = Store::new(&self.home);
        let (mut removed, mut bytes) = (Vec::new(), 0u64);
        for v in self.own_volumes(scope) {
            let kept = filters.kv("label", &v.labels)
                && !(filters.contains("label!") && filters.kv("label!", &v.labels));
            if !kept || !(all || v.is_anonymous()) || !self.volume_refs(&v.name).is_empty() {
                continue;
            }
            let size = u64::try_from(store.size(&v.name)).unwrap_or(0);
            if store.remove(&v.name).is_ok() {
                removed.push(name_there(&v).to_string());
                bytes = bytes.saturating_add(size);
            }
        }
        Answer::json(
            200,
            &json!({ "VolumesDeleted": removed, "SpaceReclaimed": bytes }),
        )
    }

    /// Container document `doc` as its microVM's engine says it: each volume it mounts by
    /// the name that engine gave it, at no path of the host's.
    pub(super) fn scoped_mounts(&self, doc: String) -> String {
        let Ok(mut d) = serde_json::from_str::<Value>(&doc) else {
            return doc;
        };
        let is_volume = |m: &Value| m.get("Type").and_then(Value::as_str) == Some("volume");
        let volumes = d
            .get("Mounts")
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(is_volume));
        if !volumes {
            return doc;
        }
        let store = Store::new(&self.home);
        if let Some(mounts) = d.get_mut("Mounts").and_then(Value::as_array_mut) {
            for m in mounts.iter_mut().filter(|m| is_volume(m)) {
                let there = m
                    .get("Name")
                    .and_then(Value::as_str)
                    .and_then(|n| store.get(n))
                    .map(|v| name_there(&v).to_string());
                if let Some(o) = m.as_object_mut() {
                    if let Some(there) = there {
                        o.insert("Name".into(), json!(there));
                    }
                    o.insert("Source".into(), json!(""));
                }
            }
        }
        serde_json::to_string(&d).unwrap_or(doc)
    }

    /// Removes the volumes microVM `provider`'s own engine made, as `provider` goes (D181):
    /// each no container mounts; one a container of the host's mounts stays, said in the
    /// log.
    pub(in crate::daemon) fn remove_own_volumes(&self, provider: &str) {
        let _held = crate::volumes::lock();
        let store = Store::new(&self.home);
        for v in store.list().into_iter().filter(|v| v.of == provider) {
            if !self.volume_refs(&v.name).is_empty() {
                crate::daemon::log(format!(
                    "volume {}: its microVM {provider} has gone, and a container still mounts it",
                    v.name
                ));
                continue;
            }
            if let Err(e) = store.remove(&v.name) {
                crate::daemon::log(format!("volume {}: as its microVM went: {e}", v.name));
            }
        }
    }
}
