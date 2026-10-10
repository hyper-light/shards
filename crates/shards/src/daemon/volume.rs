//! `shards volume` (D38): volumes as dockerd's volume service keeps them (moby
//! docker-v29.8.1 daemon/volume/service, service.go, convert.go, by.go; the local driver,
//! daemon/volume/local), in the home (`volumes.rs`), and the API's volume.Volume of each.

use std::collections::BTreeMap;

use shards_template::{Kind, Struct, Value};

use super::commands::{Asker, Reply};
use super::filters::Filters;
use super::{Daemon, lock};
use crate::containers::Disk;
use crate::volumes::{ANONYMOUS, Store, Volume};

/// The filters `volume ls` takes (acceptedListFilters).
const LIST_FILTERS: [&str; 4] = ["dangling", "name", "driver", "label"];
/// The filters `volume prune` takes (acceptedPruneFilters).
const PRUNE_FILTERS: [&str; 3] = ["label", "label!", "all"];

impl<D: Disk> Daemon<D> {
    /// The containers that mount volume `name`, made or being made: its references, which
    /// keep it from removal (the volume store's reference counts).
    fn volume_refs(&self, name: &str) -> Vec<String> {
        lock(&self.containers)
            .every()
            .filter(|c| c.mounts.iter().any(|m| m.kind == "volume" && m.name == name))
            .map(|c| c.id.clone())
            .collect()
    }

    /// `shards volume create [NAME]` (VolumesService.Create, the local driver's Create): a
    /// volume that is there already is said as it is.
    pub(super) fn volume_create(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        styled: bool,
        reply: &Reply<'_>,
    ) -> u8 {
        let name = parsed
            .args
            .first()
            .cloned()
            .unwrap_or_else(|| parsed.string("name").to_string());
        let refuse = |e: &str| {
            reply.err(&format!("Error response from daemon: create {name}: {e}"));
            1
        };
        let driver = parsed.string("driver");
        if !driver.is_empty() && driver != "local" {
            return refuse(&format!(
                "error looking up volume plugin {driver}: plugin {} not found",
                shards_cmdline::go::quote(driver)
            ));
        }
        // opts.ConvertKVStringsToMap: `KEY` alone is an empty value.
        let kv = |given: &[String]| -> BTreeMap<String, String> {
            given
                .iter()
                .map(|l| {
                    let (k, v) = l.split_once('=').unwrap_or((l.as_str(), ""));
                    (k.to_string(), v.to_string())
                })
                .collect()
        };
        let (labels, options) = (kv(parsed.many("label")), kv(parsed.many("opt")));
        // shards' own namespace, which marks the volumes an Agentfile scopes (D115).
        if let Some(k) = labels
            .keys()
            .find(|k| shards_dockerfile::agentfile::reserved_label(k.as_bytes()))
        {
            reply.err(&format!(
                "Error response from daemon: {}",
                crate::volumes::reserved(k)
            ));
            return 1;
        }
        let made = {
            let _held = crate::volumes::lock();
            Store::new(&self.home).create(&name, &labels, &options)
        };
        let volume = match made {
            Ok((v, _)) => v,
            // The store's words name the volume already.
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        if styled {
            let mut sheet = shards_ipc::Sheet::new("volume");
            sheet.record(&[
                ("made", volume.name.clone()),
                ("at", self.volume_path(&volume.name)),
            ]);
            reply.sheet(&sheet);
        } else {
            reply.out(&volume.name);
        }
        0
    }

    /// Where volume `name`'s files are, as Mountpoint names them.
    fn volume_path(&self, name: &str) -> String {
        Store::new(&self.home).data(name).display().to_string()
    }

    /// Volume `v` as the API's volume.Volume holds it (volumeToAPIType): its time in the
    /// asker's zone, as dockerd's is in its own; labels and options null where it has
    /// none.
    fn volume_value(&self, v: &Volume, offset: i64, usage: Option<(i64, i64)>) -> Value {
        let map = |m: &BTreeMap<String, String>| {
            if m.is_empty() {
                Value::NilMap(Kind::String)
            } else {
                Value::string_map(m.clone())
            }
        };
        let secs = i64::try_from(v.created / 1_000_000_000).unwrap_or(i64::MAX);
        let usage = usage.map_or_else(
            || Struct::nil("volume.UsageData"),
            |(size, refs)| {
                Struct::pointer("volume.UsageData")
                    .field("RefCount", Value::Int(refs))
                    .field("Size", Value::Int(size))
                    .value()
            },
        );
        Struct::new("volume.Volume")
            .tagged(
                "ClusterVolume",
                Some("ClusterVolume"),
                true,
                Struct::nil("volume.ClusterVolume"),
            )
            .tagged(
                "CreatedAt",
                Some("CreatedAt"),
                true,
                Value::String(shards_cmdline::format::rfc3339_at(secs, offset)),
            )
            .tagged("Driver", Some("Driver"), false, Value::String("local".into()))
            .tagged("Labels", Some("Labels"), false, map(&v.labels))
            .tagged(
                "Mountpoint",
                Some("Mountpoint"),
                false,
                Value::String(self.volume_path(&v.name)),
            )
            .tagged("Name", Some("Name"), false, Value::String(v.name.clone()))
            .tagged("Options", Some("Options"), false, map(&v.options))
            .tagged("Scope", Some("Scope"), false, Value::String("local".into()))
            .tagged("Status", Some("Status"), true, Value::NilMap(Kind::Any))
            .tagged("UsageData", Some("UsageData"), true, usage)
            .value()
    }

    /// `shards volume ls` (VolumesService.List, filtersToBy): the volumes as data, for
    /// the client to lay out as the CLI does; a colour terminal's page, where nothing
    /// asks otherwise.
    pub(super) fn volume_ls(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        self.await_removals();
        let filters = Filters::from_flags(parsed.many("filter"));
        let listed = filters
            .validate(&LIST_FILTERS)
            .and_then(|()| self.volumes_by(&filters));
        let volumes = match listed {
            Ok(v) => v,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let offset = i64::from(asker.utc_offset);
        let formatted = !parsed.string("format").is_empty() || parsed.bool("quiet");
        if asker.styled() && !formatted {
            let mut sheet = shards_ipc::Sheet::new("volumes");
            sheet.record(&[
                ("kind", "head".into()),
                (
                    "store",
                    Store::new(&self.home)
                        .data("")
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                ),
            ]);
            let store = Store::new(&self.home);
            for v in &volumes {
                let refs = self.volume_refs(&v.name);
                sheet.record(&[
                    ("name", v.name.clone()),
                    ("anonymous", v.is_anonymous().to_string()),
                    ("size", store.size(&v.name).to_string()),
                    ("vms", refs.len().to_string()),
                    ("created", (v.created / 1_000_000_000).to_string()),
                ]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        let rows: Vec<serde_json::Value> = volumes
            .iter()
            .filter_map(|v| {
                serde_json::from_str(&super::inspect_doc::json(&self.volume_value(v, offset, None))).ok()
            })
            .collect();
        let mut sheet = shards_ipc::Sheet::new("volumes-rows");
        sheet.record(&[("rows", serde_json::Value::Array(rows).to_string())]);
        reply.sheet(&sheet);
        0
    }

    /// The volumes `filters` keep (filtersToBy): by driver, name (Match), labels
    /// (MatchKVList, and `label!`'s none), and whether any container mounts them
    /// (`dangling`).
    fn volumes_by(&self, filters: &Filters) -> Result<Vec<Volume>, String> {
        let dangling = if filters.contains("dangling") {
            Some(filters.bool_or("dangling", false)?)
        } else {
            None
        };
        let names = filters.matcher("name");
        let volumes = Store::new(&self.home).list();
        Ok(volumes
            .into_iter()
            .filter(|_| filters.exact("driver", "local"))
            .filter(|v| names.matches(&v.name))
            .filter(|v| {
                filters.kv("label", &v.labels)
                    && !(filters.contains("label!") && filters.kv("label!", &v.labels))
            })
            .filter(|v| dangling.is_none_or(|d| self.volume_refs(&v.name).is_empty() == d))
            .collect())
    }

    /// `shards volume inspect NAME...` (VolumesService.Get): each found, in order; each
    /// not, said after.
    pub(super) fn volume_inspect(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        self.await_removals();
        let store = Store::new(&self.home);
        let (mut documents, mut errors) = (Vec::new(), Vec::new());
        for name in &parsed.args {
            match store.get(name) {
                Some(v) => documents.push(self.volume_value(&v, i64::from(asker.utc_offset), None)),
                None => errors.push(format!("Error response from daemon: get {name}: no such volume")),
            }
        }
        super::inspect::inspected(parsed.string("format"), &documents, errors, asker.styled(), reply)
    }

    /// Volume `name`'s document, for the top-level `inspect`.
    pub(super) fn volume_doc(&self, name: &str, offset: i64) -> Option<Value> {
        let v = Store::new(&self.home).get(name)?;
        Some(self.volume_value(&v, offset, None))
    }

    /// `shards volume rm NAME...` (VolumesService.Remove): each removed said by its name;
    /// one a container mounts is refused, with the containers, `-f` or not, as dockerd
    /// refuses it; with `-f`, one not there is no error.
    pub(super) fn volume_rm(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        styled: bool,
        reply: &Reply<'_>,
    ) -> u8 {
        self.await_removals();
        let force = parsed.bool("force");
        let store = Store::new(&self.home);
        let mut status = 0;
        let mut removed = Vec::new();
        for name in &parsed.args {
            let said = {
                let _held = crate::volumes::lock();
                match store.get(name) {
                    None if force => Ok(()),
                    None => Err(format!("get {name}: no such volume")),
                    Some(_) => match self.volume_refs(name) {
                        refs if !refs.is_empty() => {
                            Err(format!("remove {name}: volume is in use - [{}]", refs.join(", ")))
                        }
                        _ => store.remove(name),
                    },
                }
            };
            match said {
                Ok(()) => {
                    if styled {
                        removed.push(name.clone());
                    } else {
                        reply.out(name);
                    }
                }
                Err(e) => {
                    reply.err(&format!("Error response from daemon: {e}"));
                    status = 1;
                }
            }
        }
        if styled && !removed.is_empty() {
            let mut sheet = shards_ipc::Sheet::new("volume");
            for name in removed {
                sheet.record(&[("removed", name)]);
            }
            reply.sheet(&sheet);
        }
        status
    }

    /// `shards volume prune` (VolumesService.Prune, withPrune): the local volumes no
    /// container mounts and the filters keep, anonymous ones alone unless `all`; with the
    /// bytes they took.
    pub(super) fn volume_prune(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        let (removed, reclaimed) = match self.prune_volumes(parsed.many("filter"), parsed.bool("all")) {
            Ok(r) => r,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        if asker.styled() {
            let mut sheet = shards_ipc::Sheet::new("prune");
            sheet.record(&[("kind", "head".into()), ("reclaimed", reclaimed.to_string())]);
            for name in &removed {
                sheet.record(&[("volume", name.clone())]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        let mut text = String::new();
        if !removed.is_empty() {
            text.push_str("Deleted Volumes:\n");
            for name in &removed {
                text.push_str(name);
                text.push('\n');
            }
            text.push('\n');
        }
        let freed = if removed.is_empty() { 0 } else { reclaimed };
        text.push_str(&format!(
            "Total reclaimed space: {}\n",
            super::images::human_size4(i64::try_from(freed).unwrap_or(i64::MAX))
        ));
        let _ = reply.bytes(crate::spec::LOG_STDOUT, text.as_bytes());
        0
    }

    /// The volumes a prune removes, and the bytes they took: those no container mounts,
    /// of the local driver with no options, that the filters keep; anonymous ones alone
    /// unless `all` or a filter `all` says otherwise.
    pub(super) fn prune_volumes(&self, given: &[String], all: bool) -> Result<(Vec<String>, u64), String> {
        self.await_removals();
        let mut given = given.to_vec();
        if all {
            given.push("all=true".into());
        }
        let filters = Filters::from_flags(&given);
        // withPrune: one `all` at most, a bool; anonymous ones alone unless it is true.
        let alls: Vec<&str> = filters.get("all").collect();
        let every = match alls.as_slice() {
            [] => false,
            [one] => match *one {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
                "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
                other => {
                    return Err(format!(
                        "invalid filter 'all': strconv.ParseBool: parsing {}: invalid syntax",
                        shards_cmdline::go::quote(other)
                    ));
                }
            },
            many => {
                return Err(format!(
                    "invalid filter 'all={}': only one value is expected",
                    many.join(" ")
                ));
            }
        };
        filters.validate(&PRUNE_FILTERS)?;
        let mut keep = filters.without("all");
        if !every {
            keep = Filters::from_flags(
                &given
                    .iter()
                    .filter(|f| !f.starts_with("all="))
                    .cloned()
                    .chain(std::iter::once(format!("label={ANONYMOUS}")))
                    .collect::<Vec<_>>(),
            );
        }
        let store = Store::new(&self.home);
        let (mut removed, mut reclaimed) = (Vec::new(), 0u64);
        let _held = crate::volumes::lock();
        for v in store.list() {
            let kept = v.options.is_empty()
                && keep.kv("label", &v.labels)
                && !(keep.contains("label!") && keep.kv("label!", &v.labels))
                && self.volume_refs(&v.name).is_empty();
            if !kept {
                continue;
            }
            let size = u64::try_from(store.size(&v.name)).unwrap_or(0);
            if store.remove(&v.name).is_ok() {
                reclaimed = reclaimed.saturating_add(size);
                removed.push(v.name);
            }
        }
        Ok((removed, reclaimed))
    }

    /// `system df`'s volumes: each with its size and references (LocalVolumesSize, those
    /// with no options).
    pub(super) fn volume_usage(&self) -> Vec<(Volume, i64, i64)> {
        self.await_removals();
        let store = Store::new(&self.home);
        store
            .list()
            .into_iter()
            .filter(|v| v.options.is_empty())
            .map(|v| {
                let size = store.size(&v.name);
                let refs = i64::try_from(self.volume_refs(&v.name).len()).unwrap_or(i64::MAX);
                (v, size, refs)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::filters::invalid;
    use super::*;

    #[test]
    fn invalid_filters_are_dockerds_words() {
        let f = Filters::from_flags(&["bogus=1".into()]);
        assert_eq!(f.validate(&LIST_FILTERS), Err(invalid("bogus", None)));
        assert_eq!(f.validate(&LIST_FILTERS), Err("invalid filter 'bogus'".into()));
        let d = Filters::from_flags(&["dangling=maybe".into()]);
        assert_eq!(
            d.bool_or("dangling", false),
            Err("invalid filter 'dangling=[maybe]'".into())
        );
    }
}
