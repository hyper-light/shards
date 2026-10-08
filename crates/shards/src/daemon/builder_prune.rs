//! `shards builder prune` (and `buildx prune`): the build cache's records removed as
//! BuildKit's cache manager prunes them (dockerfile/1.27.1 cache/manager.go `prune`,
//! `pruneOnce`, `calculateKeepBytes`, `sortDeleteRecords`), asked as buildx v0.37.1 asks
//! (commands/prune.go `toBuildkitPruneInfo`), and said as it says them: a table of the
//! records removed, or each in full with `--verbose`, then their total.
//!
//! A record is shards' (D50): a step's result, of BuildKit's `regular` type, never
//! mutable nor in use while no build runs; shared where an image holds its own layer,
//! which a prune without `--all` keeps, as BuildKit keeps a record an image shares.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shards_cmdline::flags::Parsed;
use shards_image::store::CacheEntry;

use super::commands::{Asker, Reply, human_duration};
use super::images::human_size4;

/// A record as the prune sees it: BuildKit's `UsageInfo` of it.
struct Record {
    entry: CacheEntry,
    shared: bool,
}

/// One of the filters buildx sends (`key`, `key==value`, `key~=value`, ...), as
/// containerd's filters package matches a record's fields (`adaptUsageInfo`).
enum Filter {
    Present(String),
    Equal(String, String),
    NotEqual(String, String),
    Matches(String, regex::Regex),
}

impl Filter {
    fn parse(f: &str) -> Result<Filter, String> {
        for (op, make) in [("==", 0u8), ("!=", 1), ("~=", 2)] {
            if let Some((k, v)) = f.split_once(op) {
                let (k, v) = (k.trim().to_string(), v.trim().to_string());
                return match make {
                    0 => Ok(Filter::Equal(k, v)),
                    1 => Ok(Filter::NotEqual(k, v)),
                    _ => regex::Regex::new(&v)
                        .map(|r| Filter::Matches(k, r))
                        .map_err(|e| format!("filters: {e}")),
                };
            }
        }
        Ok(Filter::Present(f.trim().to_string()))
    }

    /// The record's field, as `adaptUsageInfo` gives it: its value, and whether present.
    fn field(r: &Record, key: &str) -> (String, bool) {
        match key {
            "id" => (r.entry.key.clone(), true),
            "type" => ("regular".into(), true),
            "immutable" => (String::new(), true),
            "shared" => (String::new(), r.shared),
            "private" => (String::new(), !r.shared),
            // parents, description, inuse, mutable: none a record of shards' has.
            _ => (String::new(), false),
        }
    }

    fn matches(&self, r: &Record) -> bool {
        match self {
            Filter::Present(k) => Filter::field(r, k).1,
            Filter::Equal(k, v) => Filter::field(r, k) == (v.clone(), true),
            Filter::NotEqual(k, v) => Filter::field(r, k) != (v.clone(), true),
            Filter::Matches(k, re) => {
                let (value, present) = Filter::field(r, k);
                present && re.is_match(&value)
            }
        }
    }
}

/// buildx's `toBuildkitPruneInfo`: `until` (or the older `unused-for`) as the age a record
/// must reach, the rest as BuildKit's filters, `id` matched as a pattern.
fn prune_info(given: &[String]) -> Result<(Option<Duration>, Vec<Filter>), String> {
    let mut by_key: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for g in given {
        let (k, v) = g.split_once('=').unwrap_or((g, ""));
        by_key.entry(k.to_lowercase()).or_default().push(v.to_string());
    }
    if by_key.contains_key("until") && by_key.contains_key("unused-for") {
        return Err("conflicting filters \"until\" and \"unused-for\"".into());
    }
    let until_key = if by_key.contains_key("unused-for") {
        "unused-for"
    } else {
        "until"
    };
    let until = match by_key.get(until_key).map(Vec::as_slice) {
        None | Some([]) => None,
        Some([v]) => {
            let ns = shards_cmdline::gotime::parse_duration(v).ok_or_else(|| {
                format!(
                    "{} filter expects a duration (e.g., '24h')",
                    shards_cmdline::go::quote(until_key)
                )
            })?;
            Some(Duration::from_nanos(u64::try_from(ns).unwrap_or(0)))
        }
        Some(_) => {
            return Err(format!(
                "{} filter expects only one value",
                shards_cmdline::go::quote(until_key)
            ));
        }
    };
    let mut filters = Vec::new();
    for (k, values) in &by_key {
        if k == until_key {
            continue;
        }
        let f = match values.as_slice() {
            [] => k.clone(),
            [v] if k == "id" => format!("{k}~={v}"),
            [v] if k.ends_with('!') || k.ends_with('~') => format!("{k}={v}"),
            [v] => format!("{k}=={v}"),
            _ => {
                return Err(format!(
                    "{} filter expects only one value",
                    shards_cmdline::go::quote(k)
                ));
            }
        };
        filters.push(Filter::parse(&f)?);
    }
    Ok((until, filters))
}

/// `calculateKeepBytes`: what may stay, given the records' total and the disk's free
/// space; 0 for no cap.
fn keep_bytes(total: i64, free: i64, max_used: i64, reserved: i64, min_free: i64) -> i64 {
    if max_used == 0 && reserved == 0 && min_free == 0 {
        return 0;
    }
    let mut keep = max_used;
    let excess = min_free - free;
    if excess > 0 {
        keep = if keep == 0 {
            total - excess
        } else {
            keep.min(total - excess)
        };
    } else if min_free != 0 && keep == 0 {
        keep = total;
    }
    keep.max(reserved)
}

/// `sortDeleteRecords`: least recently and least often used first, each ranked among the
/// others and the two ranks, each over its highest, added.
fn sort_for_deletion(records: Vec<Record>) -> Vec<Record> {
    let mut ranked: Vec<(Record, f64, f64)> = records.into_iter().map(|r| (r, 0.0, 0.0)).collect();
    ranked.sort_by_key(|(r, ..)| r.entry.last_used);
    let (mut max_used, mut newest) = (1.0f64, i64::MIN);
    for (r, used, _) in &mut ranked {
        if r.entry.last_used > newest {
            newest = r.entry.last_used;
            max_used += 1.0;
        }
        *used = max_used;
    }
    ranked.sort_by_key(|(r, ..)| r.entry.usage);
    let (mut max_count, mut count) = (1.0f64, 0u64);
    for (r, _, counted) in &mut ranked {
        if r.entry.usage != count {
            count = r.entry.usage;
            max_count += 1.0;
        }
        *counted = max_count;
    }
    ranked.sort_by(|(_, ua, ca), (_, ub, cb)| {
        (ua / max_used + ca / max_count).total_cmp(&(ub / max_used + cb / max_count))
    });
    ranked.into_iter().map(|(r, ..)| r).collect()
}

/// Go's text/tabwriter as buildx sets it for the prune (minwidth 1, tabwidth 8, padding
/// 1, tabs for padding): each column as wide as its widest cell and one more, up to a
/// multiple of 8, filled with tabs; the last cell of a line as it is.
fn tabs(lines: &[Vec<String>]) -> String {
    let columns = lines.iter().map(|l| l.len().saturating_sub(1)).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for line in lines {
        for (i, cell) in line.iter().take(line.len().saturating_sub(1)).enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count() + 1);
            }
        }
    }
    let mut out = String::new();
    for line in lines {
        let last = line.len().saturating_sub(1);
        for (i, cell) in line.iter().enumerate() {
            out.push_str(cell);
            if i < last {
                let w = widths.get(i).copied().unwrap_or(0).div_ceil(8) * 8;
                let n = (w - cell.chars().count()).div_ceil(8);
                out.push_str(&"\t".repeat(n));
            }
        }
        out.push('\n');
    }
    out
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `buildx prune`: the records `parsed` selects removed, smallest use first while a
    /// space target is given, each said as it goes, then their total.
    pub(super) fn builder_prune(&self, parsed: &Parsed, _asker: &Asker, reply: &Reply<'_>) -> u8 {
        let fail = |e: String| {
            reply.err(&format!("ERROR: {e}"));
            1
        };
        let (until, filters) = match prune_info(parsed.many("filter")) {
            Ok(info) => info,
            Err(e) => return fail(e),
        };
        let bytes = |flag: &str| parsed.string(flag).parse::<i64>().unwrap_or(0);
        // --keep-storage is --reserved-space, deprecated (prune.go).
        let reserved = bytes("reserved-space").max(bytes("keep-storage"));
        let (max_used, min_free) = (bytes("max-used-space"), bytes("min-free-space"));
        let all = parsed.bool("all");
        let store = match self.store() {
            Ok(Some(store)) => store,
            Ok(None) => {
                reply
                    .bytes(1, tabs(&[vec!["Total:".into(), human_size4(0)]]).as_bytes())
                    .ok();
                return 0;
            }
            Err(e) => return fail(e),
        };
        let held = store.referenced_blobs().unwrap_or_default();
        let shared = |e: &CacheEntry| e.own.as_ref().is_some_and(|d| held.contains(&store.blob_path(d)));
        let entries = match store.cache_entries() {
            Ok(entries) => entries,
            Err(e) => return fail(e.to_string()),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        // DiskUsage's total, shared records left out, where a target needs it.
        let mut total: i64 = if max_used != 0 || reserved != 0 || min_free != 0 {
            entries
                .iter()
                .filter(|e| !shared(e))
                .map(|e| i64::try_from(e.size).unwrap_or(i64::MAX))
                .sum()
        } else {
            0
        };
        let free = if min_free != 0 { free_space(&self.home) } else { 0 };
        let keep = keep_bytes(total, free, max_used, reserved, min_free);
        let mut chosen: Vec<Record> = entries
            .into_iter()
            .map(|entry| Record {
                shared: shared(&entry),
                entry,
            })
            .filter(|r| all || !r.shared)
            .filter(|r| {
                until.is_none_or(|age| {
                    let used = Duration::from_secs(u64::try_from(r.entry.last_used).unwrap_or(0));
                    now.saturating_sub(used) >= age
                })
            })
            .filter(|r| filters.iter().all(|f| f.matches(r)))
            .collect();
        // With a target, one at a time, as gcMode deletes them, until under it.
        if keep != 0 {
            chosen = sort_for_deletion(chosen);
        }
        let verbose = parsed.bool("verbose");
        let (mut removed, mut first) = (0i64, true);
        for r in chosen {
            if keep != 0 && total < keep {
                break;
            }
            if store.cache_remove(&r.entry.key).is_err() {
                continue;
            }
            let size = i64::try_from(r.entry.size).unwrap_or(i64::MAX);
            total -= size;
            removed = removed.saturating_add(size);
            let id = r.entry.key.get(..25).unwrap_or(&r.entry.key).to_string();
            let ago = {
                let used = Duration::from_secs(u64::try_from(r.entry.last_used).unwrap_or(0));
                format!("{} ago", human_duration(now.saturating_sub(used).as_nanos()))
            };
            let text = if verbose {
                // time.Time's String, in UTC: what the record keeps is its second.
                let created = shards_dockerfile::go::Time::from_unix(r.entry.created)
                    .rfc3339_nano()
                    .map(|t| format!("{} +0000 UTC", t.trim_end_matches('Z').replacen('T', " ", 1)))
                    .unwrap_or_default();
                tabs(&[
                    vec!["ID:".into(), id],
                    vec!["Created at:".into(), created],
                    vec!["Mutable:".into(), "false".into()],
                    vec!["Reclaimable:".into(), "true".into()],
                    vec!["Shared:".into(), r.shared.to_string()],
                    vec!["Size:".into(), human_size4(size)],
                    vec!["Usage count:".into(), r.entry.usage.to_string()],
                    vec!["Last used:".into(), ago],
                    vec!["Type:".into(), "regular".into()],
                    vec![String::new()],
                ])
            } else {
                let size_shown = if r.shared {
                    format!("{}*", human_size4(size))
                } else {
                    human_size4(size)
                };
                let row = vec![
                    format!("{id:<40}"),
                    format!("{:<5}", "true"),
                    format!("{size_shown:<10}"),
                    ago,
                ];
                if std::mem::take(&mut first) {
                    tabs(&[
                        vec![
                            "ID".into(),
                            "RECLAIMABLE".into(),
                            "SIZE".into(),
                            "LAST ACCESSED".into(),
                        ],
                        row,
                    ])
                } else {
                    tabs(&[row])
                }
            };
            if reply.bytes(1, text.as_bytes()).is_err() {
                return 1;
            }
        }
        if removed > 0 {
            self.collect_soon();
        }
        let _ = reply.bytes(1, tabs(&[vec!["Total:".into(), human_size4(removed)]]).as_bytes());
        0
    }
}

/// The free space of the filesystem `at` is on, in bytes, as BuildKit's `GetDiskStat`
/// reads it (statfs: available blocks by their size).
fn free_space(at: &std::path::Path) -> i64 {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(path) = std::ffi::CString::new(at.as_os_str().as_bytes()) else {
        return 0;
    };
    // SAFETY: statvfs is plain data, all zeros a valid value of it.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs(3) of a NUL-terminated path into a struct of our own.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut st) } != 0 {
        return 0;
    }
    // Their types are the platform's: u32 on some, u64 on others.
    #[allow(clippy::useless_conversion)]
    let free = u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize));
    i64::try_from(free).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, last_used: i64, usage: u64, shared: bool) -> Record {
        Record {
            entry: CacheEntry {
                key: key.into(),
                size: 1,
                created: 0,
                last_used,
                usage,
                own: None,
            },
            shared,
        }
    }

    #[test]
    fn filters_are_asked_as_buildx_asks_them() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let (until, filters) = prune_info(&v(&["until=24h", "id=ab", "shared"])).unwrap();
        assert_eq!(until, Some(Duration::from_secs(24 * 3600)));
        let r = record("abcdef", 0, 0, true);
        assert!(filters.iter().all(|f| f.matches(&r)));
        assert!(!filters.iter().all(|f| f.matches(&record("zz", 0, 0, true))));
        assert!(!filters.iter().all(|f| f.matches(&record("abc", 0, 0, false))));
        assert_eq!(
            prune_info(&v(&["until=1h", "unused-for=1h"])).err().unwrap(),
            "conflicting filters \"until\" and \"unused-for\""
        );
        assert_eq!(
            prune_info(&v(&["until=soon"])).err().unwrap(),
            "\"until\" filter expects a duration (e.g., '24h')"
        );
        assert_eq!(
            prune_info(&v(&["type=a", "type=b"])).err().unwrap(),
            "\"type\" filter expects only one value"
        );
    }

    /// calculateKeepBytes, case by case.
    #[test]
    fn what_stays_is_buildkits_keep() {
        assert_eq!(keep_bytes(100, 0, 0, 0, 0), 0);
        assert_eq!(keep_bytes(100, 0, 40, 0, 0), 40);
        assert_eq!(keep_bytes(100, 0, 40, 60, 0), 60);
        // 30 short of the free space asked: 30 less than the total.
        assert_eq!(keep_bytes(100, 70, 0, 0, 100), 70);
        // Free enough already: nothing goes.
        assert_eq!(keep_bytes(100, 500, 0, 0, 100), 100);
    }

    #[test]
    fn the_least_used_go_first() {
        let order: Vec<String> = sort_for_deletion(vec![
            record("new-busy", 30, 9, false),
            record("old-idle", 10, 1, false),
            record("mid", 20, 5, false),
        ])
        .into_iter()
        .map(|r| r.entry.key)
        .collect();
        assert_eq!(order, ["old-idle", "mid", "new-busy"]);
    }

    /// As Go's text/tabwriter with buildx's settings lays them out.
    #[test]
    fn tables_are_tabbed_as_buildx_tabs_them() {
        assert_eq!(tabs(&[vec!["Total:".into(), "0B".into()]]), "Total:\t0B\n");
        let row = vec![
            format!("{:<40}", "abc"),
            format!("{:<5}", "true"),
            format!("{:<10}", "1.2MB"),
            "1 second ago".into(),
        ];
        assert_eq!(
            tabs(&[
                vec![
                    "ID".into(),
                    "RECLAIMABLE".into(),
                    "SIZE".into(),
                    "LAST ACCESSED".into()
                ],
                row
            ]),
            format!(
                "ID\t\t\t\t\t\tRECLAIMABLE\tSIZE\t\tLAST ACCESSED\n{:<40}\t{:<5}\t\t{:<10}\t1 second ago\n",
                "abc", "true", "1.2MB"
            )
        );
    }
}
