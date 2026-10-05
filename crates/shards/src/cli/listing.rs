//! Listings as docker/cli prints them: the daemon sends what it lists as data, and the
//! client lays it out with `--format` (shards_cmdline::format), in its own clock, zone and
//! locale, as the Docker CLI formats what dockerd sends it.

use std::sync::OnceLock;

use shards_cmdline::format::{self, Clock, Context, Zone};

/// How the command line asked for its listing: `--format`, `-q`, and `--no-trunc`.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub format: String,
    pub quiet: bool,
    pub trunc: bool,
    /// `images --digests`.
    pub digests: bool,
    /// `history --human`.
    pub human: bool,
    /// `system df -v`.
    pub verbose: bool,
}

static ASKED: OnceLock<Asked> = OnceLock::new();

/// Records how this process's listing is to be laid out, before the daemon answers.
pub fn ask(asked: Asked) {
    let _ = ASKED.set(asked);
}

/// The zone at `secs` seconds since the epoch, as Go's `time.Local` has it: the C
/// library's, which reads `TZ` as Go does.
fn local(secs: i64) -> Zone {
    #[cfg(unix)]
    {
        // A 64-bit time_t on every target shards builds for, which libc's alias, deprecated
        // on musl, need not name.
        let t = secs;
        // SAFETY: a zeroed tm is a valid out-parameter for localtime_r(3).
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r(3) reads one time_t and fills one tm.
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            let name = if tm.tm_zone.is_null() {
                String::new()
            } else {
                // SAFETY: tm_zone points at a NUL-terminated abbreviation the C library
                // keeps.
                unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
                    .to_string_lossy()
                    .into_owned()
            };
            return Zone {
                offset: tm.tm_gmtoff,
                name,
            };
        }
    }
    format::utc(secs)
}

/// Now, in nanoseconds since the epoch.
fn now() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX))
}

/// Writes the listing in `sheet` (`ps-rows`, `images-rows`: one record, its rows as JSON)
/// to stdout; whether it was one.
pub fn show(sheet: &shards_ipc::Sheet) -> Result<bool, String> {
    use std::io::Write as _;
    let asked = ASKED.get().cloned().unwrap_or_default();
    let zone = local;
    let clock = Clock {
        now: now(),
        zone: &zone,
    };
    match render(sheet, &asked, &clock)? {
        Some(out) => {
            let _ = std::io::stdout().write_all(out.as_bytes());
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The listing in `sheet` laid out as `asked`, at `clock`; none if it is not one.
pub fn render(sheet: &shards_ipc::Sheet, asked: &Asked, clock: &Clock<'_>) -> Result<Option<String>, String> {
    let rows: serde_json::Value = match sheet.get(0, "rows") {
        Some(json) => serde_json::from_str(json).map_err(|e| e.to_string())?,
        None => return Ok(None),
    };
    let mut out = String::new();
    match sheet.name.as_str() {
        "ps-rows" => {
            let containers: Vec<format::container::Container> = rows
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(container)
                .collect();
            let ctx = Context {
                format: &format::container::format(&asked.format, asked.quiet, false),
                trunc: asked.trunc,
                east_asian: shards_cmdline::width::east_asian(|name| std::env::var(name).ok()),
                clock,
            };
            format::container::write(&ctx, &containers, &mut out)?;
        }
        "images-rows" => {
            let images: Vec<format::image::Image> = rows
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(image)
                .collect();
            // The CLI's default where no format is given (image/list.go).
            let source = if asked.format.is_empty() {
                format::TABLE
            } else {
                &asked.format
            };
            let ctx = Context {
                format: &format::image::format(source, asked.quiet, asked.digests),
                trunc: asked.trunc,
                east_asian: shards_cmdline::width::east_asian(|name| std::env::var(name).ok()),
                clock,
            };
            format::image::write(&ctx, asked.digests, &images, &mut out)?;
        }
        "history-rows" => {
            let steps: Vec<format::history::History> = rows
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|r| format::history::History {
                    id: r
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    created: r.get("created").and_then(serde_json::Value::as_i64).unwrap_or(0),
                    created_by: r
                        .get("created_by")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    size: r.get("size").and_then(serde_json::Value::as_i64).unwrap_or(0),
                    comment: r
                        .get("comment")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect();
            let source = if asked.format.is_empty() {
                format::TABLE
            } else {
                &asked.format
            };
            let ctx = Context {
                format: &format::history::format(source, asked.quiet, asked.human),
                trunc: asked.trunc,
                east_asian: shards_cmdline::width::east_asian(|name| std::env::var(name).ok()),
                clock,
            };
            format::history::write(&ctx, asked.human, &steps, &mut out)?;
        }
        "df-rows" => {
            let items = |k: &str| -> Vec<serde_json::Value> {
                rows.get(k)
                    .and_then(|u| u.get("items"))
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            };
            let kind = |k: &str| counts(&rows, k);
            let (it, ia, is, ir) = kind("images");
            let (ct, ca, cs, cr) = kind("containers");
            let (vt, va, vs, vr) = kind("volumes");
            let (bt, ba, bs, br) = kind("build_cache");
            let du = format::disk::DiskUsage {
                images: format::disk::Usage {
                    total_count: it,
                    active_count: ia,
                    total_size: is,
                    reclaimable: ir,
                    items: items("images").iter().map(image).collect(),
                },
                containers: format::disk::Usage {
                    total_count: ct,
                    active_count: ca,
                    total_size: cs,
                    reclaimable: cr,
                    items: items("containers")
                        .iter()
                        .map(|r| format::container::Container {
                            size_rw: r.get("size").and_then(serde_json::Value::as_i64).unwrap_or(0),
                            ..container(r)
                        })
                        .collect(),
                },
                volumes: format::disk::Usage {
                    total_count: vt,
                    active_count: va,
                    total_size: vs,
                    reclaimable: vr,
                    items: Vec::new(),
                },
                build_cache: format::disk::Usage {
                    total_count: bt,
                    active_count: ba,
                    total_size: bs,
                    reclaimable: br,
                    items: Vec::new(),
                },
            };
            // runDiskUsage: `table` where no format is given.
            let source = if asked.format.is_empty() {
                format::TABLE
            } else {
                &asked.format
            };
            let ctx = Context {
                format: &format::disk::format(source, asked.verbose),
                trunc: false,
                east_asian: shards_cmdline::width::east_asian(|name| std::env::var(name).ok()),
                clock,
            };
            format::disk::write(&ctx, asked.verbose, &du, &mut out)?;
        }
        _ => return Ok(None),
    }
    Ok(Some(out))
}

/// A kind's counts in a `df-rows` sheet: total, active, size and reclaimable.
fn counts(rows: &serde_json::Value, kind: &str) -> (i64, i64, i64, i64) {
    let n = |f: &str| {
        rows.get(kind)
            .and_then(|u| u.get(f))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    (n("total"), n("active"), n("size"), n("reclaimable"))
}

/// An image from the daemon's row.
fn image(row: &serde_json::Value) -> format::image::Image {
    let list = |k: &str| -> Vec<String> {
        row.get(k)
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let int = |k: &str| row.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
    format::image::Image {
        id: row
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        repo_tags: list("tags"),
        repo_digests: list("digests"),
        created: int("created"),
        size: int("size"),
        // What another image holds too, where the daemon worked it out (`system df -v`).
        shared_size: row
            .get("shared")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1),
        containers: int("containers"),
    }
}

/// A container from the daemon's row.
fn container(row: &serde_json::Value) -> format::container::Container {
    let text = |k: &str| {
        row.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    format::container::Container {
        id: text("id"),
        names: vec![format!("/{}", text("name"))],
        image: text("image"),
        image_id: text("image_id"),
        command: text("command"),
        created: row
            .get("created")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        ports: row
            .get("ports")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|p| format::container::Port {
                ip: p
                    .get("ip")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| s.parse().ok()),
                private: p
                    .get("private")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok())
                    .unwrap_or(0),
                public: p
                    .get("public")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok())
                    .unwrap_or(0),
                kind: p
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tcp")
                    .to_string(),
            })
            .collect(),
        state: text("state"),
        status: text("status"),
        health: text("health"),
        labels: row
            .get("labels")
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    }
}
