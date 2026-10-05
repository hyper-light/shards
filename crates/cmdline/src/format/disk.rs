//! `system df`'s rows (formatter/disk_usage.go's DiskUsageContext and its contexts, with
//! volume.go's and buildcache.go's for `-v`).

use shards_template::{Object, Value};

use super::container::{self, Container};
use super::image::{self, Image, is_dangling};
use super::reference::parse_normalized_named;
use super::units::{human_size, human_size_precision};
pub use super::volume::Volume;
use super::{
    Clock, Context, Ctx, Header, Methods, RAW, TABLE, context_format, parse, post_format, truncate_id,
};

/// What dockerd's disk usage says of one kind of thing: how many there are and how many
/// in use, their size and what of it could be reclaimed; and, for `-v`, each of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage<T> {
    pub total_count: i64,
    pub active_count: i64,
    pub total_size: i64,
    pub reclaimable: i64,
    pub items: Vec<T>,
}

/// client.DiskUsageResult.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskUsage {
    pub images: Usage<Image>,
    pub containers: Usage<Container>,
    pub volumes: Usage<Volume>,
    pub build_cache: Usage<BuildCache>,
}

/// A build cache record (build.CacheRecord).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildCache {
    pub id: String,
    pub parents: Vec<String>,
    /// Type.
    pub kind: String,
    pub description: String,
    pub in_use: bool,
    pub shared: bool,
    pub size: i64,
    /// Nanoseconds since the epoch.
    pub created_at: i128,
    pub last_used_at: Option<i128>,
    pub usage_count: i64,
}

const IMAGE_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}\t{{.SharedSize}}\t{{.UniqueSize}}\t{{.Containers}}";
const CONTAINER_TABLE: &str = "table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.LocalVolumes}}\t{{.Size}}\t{{.RunningFor}}\t{{.Status}}\t{{.Names}}";
const VOLUME_TABLE: &str = "table {{.Name}}\t{{.Links}}\t{{.Size}}";
const BUILD_CACHE_TABLE: &str = "table {{.ID}}\t{{.CacheType}}\t{{.Size}}\t{{.CreatedSince}}\t{{.LastUsedSince}}\t{{.UsageCount}}\t{{.Shared}}";
const SUMMARY_TABLE: &str = "table {{.Type}}\t{{.TotalCount}}\t{{.Active}}\t{{.Size}}\t{{.Reclaimable}}";

/// NewDiskUsageFormat: the format `system df` writes with, from `--format` (`table` when
/// none) and `--verbose`.
pub fn format(source: &str, verbose: bool) -> String {
    match (verbose, source) {
        (true, RAW) => format!(
            "{{{{range .Images}}}}type: Image\n{}\n{{{{end -}}}}\n\
             {{{{range .Containers}}}}type: Container\n{}\n{{{{end -}}}}\n\
             {{{{range .Volumes}}}}type: Volume\n{}\n{{{{end -}}}}\n\
             {{{{range .BuildCache}}}}type: Build Cache\n{}\n{{{{end -}}}}",
            image::format(RAW, false, true),
            container::format(RAW, false, true),
            "name: {{.Name}}\\ndriver: {{.Driver}}\\n",
            BUILD_CACHE_RAW,
        ),
        (false, TABLE) => SUMMARY_TABLE.into(),
        (false, RAW) => "type: {{.Type}}\ntotal: {{.TotalCount}}\nactive: {{.Active}}\nsize: {{.Size}}\n\
                         reclaimable: {{.Reclaimable}}\n"
            .into(),
        _ => source.into(),
    }
}

/// NewBuildCacheFormat(RAW, false).
const BUILD_CACHE_RAW: &str = "build_cache_id: {{.ID}}\nparent_id: {{.Parent}}\nbuild_cache_type: {{.CacheType}}\n\
description: {{.Description}}\ncreated_at: {{.CreatedAt}}\ncreated_since: {{.CreatedSince}}\n\
last_used_at: {{.LastUsedAt}}\nlast_used_since: {{.LastUsedSince}}\nusage_count: {{.UsageCount}}\n\
in_use: {{.InUse}}\nshared: {{.Shared}}\n";

/// DiskUsageContext.Write: a row for each kind, or with `verbose` each image, container,
/// volume and build cache record. `ctx.trunc` is unused: `-v` truncates in tables.
pub fn write(ctx: &Context<'_>, verbose: bool, du: &DiskUsage, out: &mut String) -> Result<(), String> {
    if verbose {
        return verbose_write(ctx, du, out);
    }
    let tmpl = parse(ctx.format)?;
    let mut buffer = String::new();
    let kinds = [
        (
            Kind::Images,
            du.images.total_count,
            du.images.active_count,
            du.images.total_size,
            du.images.reclaimable,
        ),
        (
            Kind::Containers,
            du.containers.total_count,
            du.containers.active_count,
            du.containers.total_size,
            du.containers.reclaimable,
        ),
        (
            Kind::Volumes,
            du.volumes.total_count,
            du.volumes.active_count,
            du.volumes.total_size,
            du.volumes.reclaimable,
        ),
        (
            Kind::BuildCache,
            du.build_cache.total_count,
            du.build_cache.active_count,
            du.build_cache.total_size,
            du.build_cache.reclaimable,
        ),
    ];
    for (kind, total_count, active_count, total_size, reclaimable) in kinds {
        let row = Ctx::value(Summary {
            kind,
            total_count,
            active_count,
            total_size,
            reclaimable,
        });
        context_format(&tmpl, &row, &mut buffer)?;
    }
    post_format(ctx.format, ctx.east_asian, &tmpl, &SUMMARY_HEADER, &buffer, out);
    Ok(())
}

/// verboseWrite: the default table writes each kind's table under a title; another format
/// runs once against all of them (diskUsageContext), writing what it wrote before an error.
fn verbose_write(ctx: &Context<'_>, du: &DiskUsage, out: &mut String) -> Result<(), String> {
    let trunc = super::is_table(ctx.format);
    let images: Vec<Value> = du
        .images
        .items
        .iter()
        .filter_map(|i| {
            let mut repo = "<none>".to_string();
            let mut tag = "<none>".to_string();
            if let Some(first) = i.repo_tags.first()
                && !is_dangling(i)
            {
                // Only the first tag; an image whose first does not parse is left out.
                let r = parse_normalized_named(first)?;
                if !r.tag.is_empty() {
                    repo = r.familiar_name();
                    tag = r.tag;
                }
            }
            Some(Ctx::value(image::Row::new(
                i,
                trunc,
                ctx.clock,
                repo,
                tag,
                String::new(),
            )))
        })
        .collect();
    let containers: Vec<Container> = du
        .containers
        .items
        .iter()
        .map(|c| Container {
            // The virtual size is not shown.
            size_root_fs: 0,
            ..c.clone()
        })
        .collect();
    let containers = container::rows(&containers, trunc, ctx.clock);
    let volumes: Vec<Value> = du
        .volumes
        .items
        .iter()
        .map(|v| Ctx::value(super::volume::Row(v.clone())))
        .collect();
    let mut cache = du.build_cache.items.clone();
    sort_build_cache(&mut cache);
    let cache: Vec<Value> = cache
        .iter()
        .map(|b| Ctx::value(CacheRow::new(b, trunc, ctx.clock)))
        .collect();

    if ctx.format == TABLE {
        let sections: [(&str, &str, &'static Header, &[Value]); 4] = [
            ("Images space usage:\n\n", IMAGE_TABLE, &image::HEADER, &images),
            (
                "\nContainers space usage:\n\n",
                CONTAINER_TABLE,
                &container::HEADER,
                &containers,
            ),
            (
                "\nLocal Volumes space usage:\n\n",
                VOLUME_TABLE,
                &super::volume::HEADER,
                &volumes,
            ),
            ("", BUILD_CACHE_TABLE, &CACHE_HEADER, &cache),
        ];
        for (title, format, header, rows) in sections {
            let tmpl = parse(format)?;
            if title.is_empty() {
                #[allow(clippy::cast_precision_loss)]
                let size = human_size(du.build_cache.total_size as f64);
                out.push_str(&format!("\nBuild cache usage: {size}\n\n"));
            } else {
                out.push_str(title);
            }
            let mut buffer = String::new();
            for row in rows {
                context_format(&tmpl, row, &mut buffer)?;
            }
            post_format(format, ctx.east_asian, &tmpl, header, &buffer, out);
        }
        return Ok(());
    }
    let tmpl = parse(ctx.format)?;
    let all = Value::object(All {
        images,
        containers,
        volumes,
        cache,
    });
    tmpl.execute_into(&all, out)
}

/// buildCacheSort: never used first, then by when last used, then by ID.
fn sort_build_cache(cache: &mut [BuildCache]) {
    cache.sort_by(|a, b| match (a.last_used_at, b.last_used_at) {
        (None, None) => a.id.cmp(&b.id),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.id.cmp(&b.id)),
    });
}

static SUMMARY_HEADER: Header = Header(&[
    ("Active", "ACTIVE"),
    ("Reclaimable", "RECLAIMABLE"),
    ("Size", "SIZE"),
    ("TotalCount", "TOTAL"),
    ("Type", "TYPE"),
]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Images,
    Containers,
    Volumes,
    BuildCache,
}

/// diskUsageImagesContext, diskUsageContainersContext, diskUsageVolumesContext and
/// diskUsageBuilderContext, which differ in their type and name.
#[derive(Debug)]
struct Summary {
    kind: Kind,
    total_count: i64,
    active_count: i64,
    total_size: i64,
    reclaimable: i64,
}

impl Methods for Summary {
    fn type_name(&self) -> &'static str {
        match self.kind {
            Kind::Images => "*formatter.diskUsageImagesContext",
            Kind::Containers => "*formatter.diskUsageContainersContext",
            Kind::Volumes => "*formatter.diskUsageVolumesContext",
            Kind::BuildCache => "*formatter.diskUsageBuilderContext",
        }
    }
    const METHODS: &'static [&'static str] = &["Active", "Reclaimable", "Size", "TotalCount", "Type"];
    const HEADER: &'static Header = &SUMMARY_HEADER;

    #[allow(clippy::cast_precision_loss)]
    fn get(&self, name: &str) -> Option<Value> {
        let s = match name {
            "Active" => self.active_count.to_string(),
            "Reclaimable" => {
                let r = human_size(self.reclaimable as f64);
                if self.kind != Kind::BuildCache && self.total_size > 0 {
                    // Go's int64 arithmetic, which wraps.
                    let pct = self.reclaimable.wrapping_mul(100) / self.total_size;
                    format!("{r} ({pct}%)")
                } else {
                    r
                }
            }
            "Size" => human_size(self.total_size as f64),
            "TotalCount" => self.total_count.to_string(),
            "Type" => match self.kind {
                Kind::Images => "Images",
                Kind::Containers => "Containers",
                Kind::Volumes => "Local Volumes",
                Kind::BuildCache => "Build Cache",
            }
            .into(),
            _ => return None,
        };
        Some(Value::String(s))
    }
}

static CACHE_HEADER: Header = Header(&[
    ("CacheType", "CACHE TYPE"),
    ("CreatedSince", "CREATED"),
    ("Description", "DESCRIPTION"),
    ("ID", "CACHE ID"),
    ("InUse", "IN USE"),
    ("LastUsedSince", "LAST USED"),
    ("Parent", "PARENT"),
    ("Shared", "SHARED"),
    ("Size", "SIZE"),
    ("UsageCount", "USAGE"),
]);

/// buildCacheContext, with its times worked out from the clock.
#[derive(Debug)]
struct CacheRow {
    b: BuildCache,
    trunc: bool,
    created_at: String,
    created_since: String,
    last_used_at: String,
    last_used_since: String,
}

impl CacheRow {
    fn new(b: &BuildCache, trunc: bool, clock: &Clock<'_>) -> CacheRow {
        CacheRow {
            b: b.clone(),
            trunc,
            created_at: clock.string(b.created_at),
            created_since: clock.ago(b.created_at),
            last_used_at: b.last_used_at.map(|t| clock.string(t)).unwrap_or_default(),
            last_used_since: b.last_used_at.map(|t| clock.ago(t)).unwrap_or_default(),
        }
    }
}

impl Methods for CacheRow {
    fn type_name(&self) -> &'static str {
        "*formatter.buildCacheContext"
    }
    const METHODS: &'static [&'static str] = &[
        "CacheType",
        "CreatedAt",
        "CreatedSince",
        "Description",
        "ID",
        "InUse",
        "LastUsedAt",
        "LastUsedSince",
        "Parent",
        "Shared",
        "Size",
        "UsageCount",
    ];
    const HEADER: &'static Header = &CACHE_HEADER;

    fn get(&self, name: &str) -> Option<Value> {
        let b = &self.b;
        let s = match name {
            "CacheType" => b.kind.clone(),
            "CreatedAt" => self.created_at.clone(),
            "CreatedSince" => self.created_since.clone(),
            "Description" => b.description.clone(),
            "ID" => {
                let id = if self.trunc {
                    truncate_id(&b.id)
                } else {
                    b.id.clone()
                };
                if b.in_use { format!("{id}*") } else { id }
            }
            "InUse" => b.in_use.to_string(),
            "LastUsedAt" => self.last_used_at.clone(),
            "LastUsedSince" => self.last_used_since.clone(),
            "Parent" => {
                let parent = b.parents.join(", ");
                if self.trunc { truncate_id(&parent) } else { parent }
            }
            "Shared" => b.shared.to_string(),
            #[allow(clippy::cast_precision_loss)]
            "Size" => human_size_precision(b.size as f64, 3),
            "UsageCount" => b.usage_count.to_string(),
            _ => return None,
        };
        Some(Value::String(s))
    }
}

/// diskUsageContext: what a `-v` format other than the default table runs against.
#[derive(Debug)]
struct All {
    images: Vec<Value>,
    containers: Vec<Value>,
    volumes: Vec<Value>,
    cache: Vec<Value>,
}

impl Object for All {
    fn type_name(&self) -> &str {
        "*formatter.diskUsageContext"
    }

    fn field(&self, name: &str) -> Option<Value> {
        let items = match name {
            "Images" => &self.images,
            "Containers" => &self.containers,
            "Volumes" => &self.volumes,
            "BuildCache" => &self.cache,
            _ => return None,
        };
        Some(Value::list(items.clone()))
    }

    fn format(&self, out: &mut String) {
        out.push_str("{}");
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push('{');
        let all = [
            ("Images", &self.images),
            ("Containers", &self.containers),
            ("Volumes", &self.volumes),
            ("BuildCache", &self.cache),
        ];
        for (i, (name, items)) in all.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(name);
            out.push_str("\":[");
            for (j, v) in items.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                if let Value::Object(o) = v {
                    o.json(out)?;
                }
            }
            out.push(']');
        }
        out.push('}');
        Ok(())
    }
}
