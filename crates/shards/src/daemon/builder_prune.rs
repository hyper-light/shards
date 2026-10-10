//! `shards builder prune` (and `buildx prune`): the build cache's records removed as
//! BuildKit's cache manager prunes them (`crate::build::gc`), asked as buildx v0.37.1 asks
//! (commands/prune.go `toBuildkitPruneInfo`), and said as it says them: a table of the
//! records removed, or each in full with `--verbose`, then their total. And `buildx du`:
//! the same records listed as BuildKit's DiskUsage lists them, for the client to lay out
//! as buildx does (cli/listing.rs, shards_cmdline::format::du).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shards_cmdline::flags::Parsed;

use super::commands::{Asker, Reply, human_duration};
use super::images::human_size4;
use crate::build::gc::{self, Filter, Record, Rule};

/// buildx's `toBuildkitPruneInfo` (shards_cmdline::buildcache): `until`, or the older
/// `unused-for`, as the age a record must reach; the rest as BuildKit's filters.
fn prune_info(given: &[String]) -> Result<(Option<Duration>, Vec<Filter>), String> {
    let info = shards_cmdline::buildcache::prune_info(given)?;
    let until = info
        .keep_duration
        .map(|ns| Duration::from_nanos(u64::try_from(ns).unwrap_or(0)));
    let filters = info
        .filters
        .iter()
        .map(|f| Filter::parse(f))
        .collect::<Result<_, _>>()?;
    Ok((until, filters))
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

/// A removed record as buildx says it: in full with `--verbose`, else a row of the table,
/// its header first.
fn said(r: &Record, verbose: bool, first: bool) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let size = human_size4(i64::try_from(r.size).unwrap_or(i64::MAX));
    let id = r.shown_id().to_string();
    let ago = {
        let used = Duration::from_secs(u64::try_from(r.last_used).unwrap_or(0));
        format!("{} ago", human_duration(now.saturating_sub(used).as_nanos()))
    };
    if verbose {
        // time.Time's String, in UTC: what the record keeps is its second.
        let created = shards_dockerfile::go::Time::from_unix(r.created)
            .rfc3339_nano()
            .map(|t| format!("{} +0000 UTC", t.trim_end_matches('Z').replacen('T', " ", 1)))
            .unwrap_or_default();
        let mut lines = vec![
            vec!["ID:".into(), id],
            vec!["Created at:".into(), created],
            vec!["Mutable:".into(), r.mount.is_some().to_string()],
            vec!["Reclaimable:".into(), "true".into()],
            vec!["Shared:".into(), r.shared.to_string()],
            vec!["Size:".into(), size],
        ];
        if let Some(d) = r.mount.as_ref().filter(|d| !d.is_empty()) {
            lines.push(vec!["Description:".into(), d.clone()]);
        }
        // No `Type:`: BuildKit's prune says no record's type (cache/manager.go
        // `pruneOnce`), so buildx prints none (measured, M141).
        lines.extend([
            vec!["Usage count:".into(), r.usage.to_string()],
            vec!["Last used:".into(), ago],
            vec![String::new()],
        ]);
        return tabs(&lines);
    }
    let size_shown = if r.shared { format!("{size}*") } else { size };
    // A mutable record's ID is marked, as buildx's table marks it.
    let id = if r.mount.is_some() { format!("{id}*") } else { id };
    let row = vec![
        format!("{id:<40}"),
        format!("{:<5}", "true"),
        format!("{size_shown:<10}"),
        ago,
    ];
    if first {
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
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `buildx prune`: the records `parsed` selects removed, smallest use first while a
    /// space target is given, each said as it goes, then their total.
    pub(super) fn builder_prune(&self, parsed: &Parsed, _asker: &Asker, reply: &Reply<'_>) -> u8 {
        let fail = |e: String| {
            reply.err(&format!("ERROR: {e}"));
            1
        };
        let (keep_duration, filters) = match prune_info(parsed.many("filter")) {
            Ok(info) => info,
            Err(e) => return fail(e),
        };
        let bytes = |flag: &str| parsed.string(flag).parse::<i64>().unwrap_or(0);
        let rule = Rule {
            filters,
            all: parsed.bool("all"),
            keep_duration,
            // --keep-storage is --reserved-space, deprecated (prune.go).
            reserved: bytes("reserved-space").max(bytes("keep-storage")),
            max_used: bytes("max-used-space"),
            min_free: bytes("min-free-space"),
        };
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
        let verbose = parsed.bool("verbose");
        let mut first = true;
        let removed = gc::prune(&store, &self.home, &rule, &mut |r| {
            let text = said(r, verbose, std::mem::take(&mut first));
            reply.bytes(1, text.as_bytes()).map_err(|e| e.to_string())
        });
        let removed = match removed {
            Ok(n) => n,
            Err(_) => return 1,
        };
        if removed > 0 {
            self.collect_soon();
        }
        let _ = reply.bytes(1, tabs(&[vec!["Total:".into(), human_size4(removed)]]).as_bytes());
        0
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `buildx du`: the records `parsed` selects, as BuildKit's DiskUsage answers them
    /// (cache/manager.go): a mutable record a step holds sized 0, as its size is changing,
    /// and none with a parent, as no record of shards' has one.
    pub(super) fn builder_du(&self, parsed: &Parsed, _asker: &Asker, reply: &Reply<'_>) -> u8 {
        let fail = |e: String| {
            reply.err(&format!("ERROR: {e}"));
            1
        };
        // What the client refuses before it asks, refused here too for another client.
        if let Err(e) = shards_cmdline::format::du::format(parsed.string("format"), parsed.bool("verbose")) {
            return fail(e);
        }
        // DiskUsage takes no age: `until` is read, and asks nothing.
        let filters = match prune_info(parsed.many("filter")) {
            Ok((_, filters)) => filters,
            Err(e) => return fail(e),
        };
        let records = match self.store() {
            Ok(Some(store)) => match gc::records(&store) {
                Ok(records) => records,
                Err(e) => return fail(e),
            },
            Ok(None) => Vec::new(),
            Err(e) => return fail(e),
        };
        let ns = |s: i64| (i128::from(s) * 1_000_000_000).to_string();
        let rows: Vec<serde_json::Value> = records
            .iter()
            .filter(|(r, in_use)| filters.iter().all(|f| f.matches(r, *in_use)))
            .map(|(r, in_use)| {
                let mutable = r.mount.is_some();
                let size = if mutable && *in_use {
                    0
                } else {
                    i64::try_from(r.size).unwrap_or(i64::MAX)
                };
                serde_json::json!({
                    "id": r.shown_id(), "type": r.kind(), "parents": [],
                    "description": r.mount.clone().unwrap_or_default(), "mutable": mutable,
                    "in_use": in_use, "shared": r.shared, "size": size, "created": ns(r.created),
                    "last_used": ns(r.last_used), "usage": r.usage,
                })
            })
            .collect();
        let mut sheet = shards_ipc::Sheet::new("du-rows");
        sheet.record(&[("rows", serde_json::Value::from(rows).to_string())]);
        reply.sheet(&sheet);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, shared: bool) -> Record {
        Record {
            id: key.into(),
            size: 1,
            created: 0,
            last_used: 0,
            usage: 0,
            shared,
            mount: None,
        }
    }

    #[test]
    fn filters_are_asked_as_buildx_asks_them() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        // `shared=`: FilterOpt takes no filter without `=`; an empty value asks `shared==`.
        let (until, filters) = prune_info(&v(&["until=24h", "id=ab", "shared="])).unwrap();
        assert_eq!(until, Some(Duration::from_secs(24 * 3600)));
        let r = record("abcdef", true);
        assert!(filters.iter().all(|f| f.matches(&r, false)));
        assert!(!filters.iter().all(|f| f.matches(&record("zz", true), false)));
        assert!(!filters.iter().all(|f| f.matches(&record("abc", false), false)));
        // A cache mount's record, by its type.
        let mount = Record {
            mount: Some("cached mount /c from exec sh".into()),
            ..record("m1", false)
        };
        let matched = |given: &[&str], r: &Record| {
            prune_info(&v(given))
                .unwrap()
                .1
                .iter()
                .all(|f| f.matches(r, false))
        };
        assert!(matched(&["type=exec.cachemount"], &mount));
        assert!(!matched(&["type=exec.cachemount"], &r));
        assert_eq!(
            prune_info(&v(&["until=1h", "unused-for=1h"])).err().unwrap(),
            "conflicting filters \"until\" and \"unused-for\""
        );
        assert_eq!(
            prune_info(&v(&["until=soon"])).err().unwrap(),
            "\"until\" filter expects a duration (e.g., '24h'): time: invalid duration \"soon\""
        );
        assert_eq!(
            prune_info(&v(&["type=a", "type=b"])).err().unwrap(),
            "\"type\" filter expects only one value"
        );
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

    /// A cache mount's record said in full, as buildx said one (M141): mutable, described,
    /// no type.
    #[test]
    fn a_cache_mounts_record_is_said_as_buildx_says_it() {
        let mount = Record {
            id: "6c81e0q93yn12ftnxfg6z64pl".into(),
            size: 8192,
            created: 0,
            last_used: 0,
            usage: 2,
            shared: false,
            mount: Some("cached mount /x from exec /bin/sh -c true with id \"/two\"".into()),
        };
        let full = said(&mount, true, true);
        assert!(full.contains("Mutable:\ttrue\n"), "{full}");
        assert!(
            full.contains("Description:\tcached mount /x from exec /bin/sh -c true with id \"/two\"\n"),
            "{full}"
        );
        assert!(!full.contains("Type:"), "{full}");
        let row = said(&mount, false, false);
        assert!(row.starts_with("6c81e0q93yn12ftnxfg6z64pl*"), "{row}");
    }
}
