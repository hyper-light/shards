//! What shards takes on disk through the API (D170): `shards inspect disk`. Images,
//! microVMs, volumes and the build cache, as dockerd counts them; and the microVMs'
//! templates, the snapshots each image's runs restore, which no container engine has and
//! whose cost no Docker client is told of otherwise.

use serde_json::{Map, Value, json};

use super::Call;
use super::http::Answer;
use crate::containers::Disk;
use crate::daemon::{Daemon, lock};

/// What `v` holds at JSON pointer `path`, null where nothing.
fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

/// A kind's usage as the API has it (DiskUsage of image, container, volume and build): its
/// counts and sizes, each left out where zero, as Go's `omitempty` leaves them, and its
/// items where there are any.
fn usage(rows: &Value, items: Vec<Value>) -> Value {
    let mut out = Map::new();
    for (key, from) in [
        ("ActiveCount", "active"),
        ("Reclaimable", "reclaimable"),
        ("TotalCount", "total"),
        ("TotalSize", "size"),
    ] {
        let n = rows.get(from).and_then(Value::as_i64).unwrap_or(0);
        if n != 0 {
            out.insert(key.into(), json!(n));
        }
    }
    if !items.is_empty() {
        out.insert("Items".into(), Value::Array(items));
    }
    Value::Object(out)
}

impl<D: Disk> Daemon<D> {
    /// `GET /system/df`: `shards inspect disk`, the kinds the query's `type`s name, all
    /// without one. Each kind's usage (API 1.52's ImageUsage, ContainerUsage, VolumeUsage
    /// and BuildCacheUsage), and, unless `verbose`, the fields clients before 1.52 read
    /// (LayersSize, Images, Containers, Volumes, BuildCache), as dockerd answers both;
    /// and `TemplateUsage`, shards' own: its microVMs' templates, their count and size.
    pub(super) fn engine_df(&self, call: &Call<'_>) -> Answer {
        let types: Vec<&str> = call.all("type").collect();
        if let Some(other) = types
            .iter()
            .find(|t| !matches!(**t, "image" | "container" | "volume" | "build-cache"))
        {
            return Answer::error(400, format!("unknown object type: {other}"));
        }
        let typed = !types.is_empty();
        let wants = |t: &str| !typed || types.contains(&t);
        let (images, containers, volumes, cache) = (
            wants("image"),
            wants("container"),
            wants("volume"),
            wants("build-cache"),
        );
        let verbose = call.flag("verbose");
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        let offset = i64::from(crate::cli::utc_offset());
        let du = self.disk_usage(true, now, offset);
        let rows = &du.rows;
        let mut out = Map::new();
        if images {
            let items: Vec<Value> = self
                .listed(&crate::daemon::filters::Filters::default(), false, now, offset)
                .unwrap_or_default()
                .iter()
                .map(|i| {
                    let shared = at(rows, "/images/items")
                        .as_array()
                        .and_then(|a| a.iter().find(|r| *at(r, "/id") == json!(i.id)))
                        .and_then(|r| at(r, "/shared").as_i64())
                        .unwrap_or(-1);
                    self.image_summary(i, shared)
                })
                .collect();
            if !verbose {
                out.insert("LayersSize".into(), at(rows, "/images/size").clone());
                out.insert("Images".into(), Value::Array(items.clone()));
            }
            out.insert("ImageUsage".into(), usage(at(rows, "/images"), items));
        }
        if containers {
            let mut kept: Vec<crate::containers::Container> = lock(&self.containers).all().cloned().collect();
            kept.sort_by_key(|c| std::cmp::Reverse(c.created));
            let items: Vec<Value> = kept.iter().map(|c| self.summary(c, true)).collect();
            if !verbose {
                out.insert("Containers".into(), Value::Array(items.clone()));
            }
            out.insert("ContainerUsage".into(), usage(at(rows, "/containers"), items));
        }
        if volumes {
            let items: Vec<Value> = at(rows, "/volumes/items")
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    let name = at(row, "/Name").as_str()?;
                    let doc = self.volume_doc(name, offset)?;
                    let mut doc: Value =
                        serde_json::from_str(&crate::daemon::inspect_doc::json(&doc)).ok()?;
                    doc.as_object_mut()?
                        .insert("UsageData".into(), at(row, "/UsageData").clone());
                    Some(doc)
                })
                .collect();
            if !verbose {
                out.insert("Volumes".into(), Value::Array(items.clone()));
            }
            out.insert("VolumeUsage".into(), usage(at(rows, "/volumes"), items));
        }
        if cache {
            let time =
                |v: &Value| crate::daemon::inspect_doc::go_time(v.as_str().and_then(|n| n.parse().ok()));
            let items: Vec<Value> = at(rows, "/build_cache/items")
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    json!({
                        "ID": at(r, "/id"),
                        "Type": at(r, "/type"),
                        "Description": at(r, "/description").as_str().unwrap_or_default(),
                        "InUse": at(r, "/in_use").as_bool().unwrap_or(false),
                        "Shared": at(r, "/shared").as_bool().unwrap_or(false),
                        "Size": at(r, "/size"),
                        "CreatedAt": time(at(r, "/created")),
                        "LastUsedAt": time(at(r, "/last_used")),
                        "UsageCount": at(r, "/usage"),
                    })
                })
                .collect();
            if !verbose {
                out.insert("BuildCache".into(), Value::Array(items.clone()));
            }
            out.insert("BuildCacheUsage".into(), usage(at(rows, "/build_cache"), items));
        }
        if !typed {
            let (count, size) = du.templates;
            out.insert(
                "TemplateUsage".into(),
                usage(
                    &json!({ "total": count, "active": count, "size": size }),
                    Vec::new(),
                ),
            );
        }
        Answer::json(200, &Value::Object(out))
    }
}
