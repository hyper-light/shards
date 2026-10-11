//! The build cache's records pruned as BuildKit's cache manager prunes them (dockerfile/1.27.1
//! cache/manager.go `prune`, `pruneOnce`, `calculateKeepBytes`, `sortDeleteRecords`): by
//! `builder prune` (D65), and by the collector each build runs once it is done (D114), with
//! the default policy dockerd derives from its disk (moby docker-v29.3.1
//! daemon/internal/builder-next/worker/gc.go `DefaultGCPolicy`), at most once a minute, as
//! BuildKit's controller throttles it (control/control.go `throttledGC`).
//!
//! A record is a step's result (D50), of BuildKit's `regular` type, never mutable nor in
//! use while no build runs, and shared where an image holds its own layer, which a prune
//! without `all` keeps, as BuildKit keeps a record an image shares; or a cache mount's
//! (D114), of type `exec.cachemount`, mutable, described as BuildKit describes it, and
//! never removed while a step holds it, as BuildKit removes no record in use.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shards_image::store::Store;

/// A record as a prune sees it: BuildKit's `UsageInfo` of it.
#[derive(Debug)]
pub struct Record {
    /// A step's key, or a cache mount's id.
    pub id: String,
    pub size: u64,
    // Said by `builder prune` alone, which the daemon serves, where there is one (Unix).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub created: i64,
    pub last_used: i64,
    pub usage: u64,
    pub shared: bool,
    /// A cache mount's description: none for a step's record.
    pub mount: Option<String>,
}

impl Record {
    /// Its ID as BuildKit's are, 25 characters: as listings show it and filters match it.
    pub fn shown_id(&self) -> &str {
        self.id.get(..25).unwrap_or(&self.id)
    }

    pub fn kind(&self) -> &'static str {
        if self.mount.is_some() {
            "exec.cachemount"
        } else {
            "regular"
        }
    }
}

/// One of the filters a prune is asked by (`key`, `key==value`, `key~=value`, ...), as
/// containerd's filters package matches a record's fields (`adaptUsageInfo`).
#[derive(Debug)]
pub enum Filter {
    Present(String),
    Equal(String, String),
    NotEqual(String, String),
    Matches(String, regex::Regex),
}

impl Filter {
    pub fn parse(f: &str) -> Result<Filter, String> {
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
    fn field(r: &Record, in_use: bool, key: &str) -> (String, bool) {
        let description = r.mount.clone().unwrap_or_default();
        match key {
            "id" => (r.shown_id().into(), true),
            "inuse" => (String::new(), in_use),
            "type" => (r.kind().into(), true),
            "mutable" => (String::new(), r.mount.is_some()),
            "immutable" => (String::new(), r.mount.is_none()),
            "shared" => (String::new(), r.shared),
            "private" => (String::new(), !r.shared),
            "description" => {
                let present = !description.is_empty();
                (description, present)
            }
            // parents, which no record of shards' has.
            _ => (String::new(), false),
        }
    }

    /// Whether `r`, which a step holds or not as `in_use` says, has what the filter asks.
    pub fn matches(&self, r: &Record, in_use: bool) -> bool {
        match self {
            Filter::Present(k) => Filter::field(r, in_use, k).1,
            Filter::Equal(k, v) => Filter::field(r, in_use, k) == (v.clone(), true),
            Filter::NotEqual(k, v) => Filter::field(r, in_use, k) != (v.clone(), true),
            Filter::Matches(k, re) => {
                let (value, present) = Filter::field(r, in_use, k);
                present && re.is_match(&value)
            }
        }
    }
}

/// What a prune takes, as `client.PruneInfo` says it: the records its filters match, all of
/// them or those no image shares, unused for its age at least; while the records' total is
/// past what its space targets keep, the least used first, one at a time.
#[derive(Debug, Default)]
pub struct Rule {
    pub filters: Vec<Filter>,
    pub all: bool,
    pub keep_duration: Option<Duration>,
    pub reserved: i64,
    pub max_used: i64,
    pub min_free: i64,
}

/// `calculateKeepBytes`: what may stay, given the records' total and the disk's free
/// space; 0 for no cap.
pub fn keep_bytes(total: i64, free: i64, max_used: i64, reserved: i64, min_free: i64) -> i64 {
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
pub fn sort_for_deletion(records: Vec<Record>) -> Vec<Record> {
    let mut ranked: Vec<(Record, f64, f64)> = records.into_iter().map(|r| (r, 0.0, 0.0)).collect();
    ranked.sort_by_key(|(r, ..)| r.last_used);
    let (mut max_used, mut newest) = (1.0f64, i64::MIN);
    for (r, used, _) in &mut ranked {
        if r.last_used > newest {
            newest = r.last_used;
            max_used += 1.0;
        }
        *used = max_used;
    }
    ranked.sort_by_key(|(r, ..)| r.usage);
    let (mut max_count, mut count) = (1.0f64, 0u64);
    for (r, _, counted) in &mut ranked {
        if r.usage != count {
            count = r.usage;
            max_count += 1.0;
        }
        *counted = max_count;
    }
    ranked.sort_by(|(_, ua, ca), (_, ub, cb)| {
        (ua / max_used + ca / max_count).total_cmp(&(ub / max_used + cb / max_count))
    });
    ranked.into_iter().map(|(r, ..)| r).collect()
}

/// Every record of `store`'s, and whether a step holds it now.
pub fn records(store: &Store) -> Result<Vec<(Record, bool)>, String> {
    let held = store.referenced_blobs().map_err(|e| e.to_string())?;
    let entries = store.cache_entries().map_err(|e| e.to_string())?;
    let mounts = store.mount_entries().map_err(|e| e.to_string())?;
    Ok(entries
        .into_iter()
        .map(|e| {
            let shared = e.own.as_ref().is_some_and(|d| held.contains(&store.blob_path(d)));
            let r = Record {
                id: e.key,
                size: e.size,
                created: e.created,
                last_used: e.last_used,
                usage: e.usage,
                shared,
                mount: None,
            };
            (r, false)
        })
        .chain(mounts.into_iter().map(|m| {
            let r = Record {
                id: m.id,
                size: m.size,
                created: m.created,
                last_used: m.last_used,
                usage: m.usage,
                shared: false,
                mount: Some(m.description),
            };
            (r, m.in_use)
        }))
        .collect())
}

/// What `rule` may remove of `records` (each with whether a step holds it), in the order
/// it removes them: none a step holds, as BuildKit's `pruneOnce` takes only records nothing
/// references; under a target (`keep`), the least used first, ranked among those alone.
fn removable(records: Vec<(Record, bool)>, rule: &Rule, keep: i64, now: Duration) -> Vec<Record> {
    let chosen: Vec<Record> = records
        .into_iter()
        .filter(|(_, in_use)| !in_use)
        .map(|(r, _)| r)
        .filter(|r| rule.all || !r.shared)
        .filter(|r| {
            rule.keep_duration.is_none_or(|age| {
                let used = Duration::from_secs(u64::try_from(r.last_used).unwrap_or(0));
                now.saturating_sub(used) >= age
            })
        })
        .filter(|r| rule.filters.iter().all(|f| f.matches(r, false)))
        .collect();
    // With a target, one at a time, as gcMode deletes them, until under it.
    if keep != 0 {
        sort_for_deletion(chosen)
    } else {
        chosen
    }
}

/// Prunes `store`, on the disk of `disk`, as `rule` says: each record removed said to
/// `removed`, which may stop it; the bytes they held.
pub fn prune(
    store: &Store,
    disk: &Path,
    rule: &Rule,
    removed: &mut dyn FnMut(&Record) -> Result<(), String>,
) -> Result<i64, String> {
    let records = records(store)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    // DiskUsage's total, shared records left out, where a target needs it; in use or not.
    let targeted = rule.max_used != 0 || rule.reserved != 0 || rule.min_free != 0;
    let mut total: i64 = if targeted {
        records
            .iter()
            .filter(|(r, _)| !r.shared)
            .map(|(r, _)| i64::try_from(r.size).unwrap_or(i64::MAX))
            .sum()
    } else {
        0
    };
    let free = if rule.min_free != 0 {
        shards_vmm::platform::disk_space(disk).map_or(0, |(free, _)| i64::try_from(free).unwrap_or(i64::MAX))
    } else {
        0
    };
    let keep = keep_bytes(total, free, rule.max_used, rule.reserved, rule.min_free);
    let mut freed = 0i64;
    for r in removable(records, rule, keep, now) {
        if keep != 0 && total < keep {
            break;
        }
        // A cache mount's goes unless a step took it meanwhile.
        let gone = match &r.mount {
            Some(_) => store.remove_mount(&r.id).map_err(|e| e.to_string())?,
            None => store.cache_remove(&r.id).is_ok(),
        };
        if !gone {
            continue;
        }
        let size = i64::try_from(r.size).unwrap_or(i64::MAX);
        total -= size;
        freed = freed.saturating_add(size);
        removed(&r)?;
    }
    Ok(freed)
}

/// `diskPercentage`: `percentage` of a disk of `total` bytes, its GiB counted and one more,
/// in decimal gigabytes, as dockerd rounds it up.
fn disk_percentage(total: u64, percentage: u64) -> i64 {
    let gib = total.saturating_mul(percentage) / 100 / (1 << 30);
    i64::try_from(gib.saturating_add(1).saturating_mul(1_000_000_000)).unwrap_or(i64::MAX)
}

/// `tempCachePercent`, e·π·φ as Go's constant arithmetic rounds it to a float64: the share
/// of the reserve the easiest to make again may take.
const TEMP_CACHE_PERCENT: f64 = 13.817_580_227_176_494;

/// dockerd's default GC policy for a disk of `total` bytes, where its size is known
/// (`DefaultGCPolicy`, with no space configured): what is easiest to make again (context
/// transfers, Git checkouts, cache mounts) unused for two days, past a temporary share of
/// the reserve; anything unused for 60 days; the unshared cache under the cap; then all of
/// it under the cap. Where the disk cannot be read, 2 GB reserved and no cap.
pub fn default_policy(total: Option<u64>) -> Vec<Rule> {
    let (reserved, max_used, min_free) = match total {
        Some(t) => (
            disk_percentage(t, 10),
            disk_percentage(t, 80),
            disk_percentage(t, 20),
        ),
        None => (2_000_000_000, 0, 0),
    };
    // The temporary share, 512 MB at least.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let temp = ((reserved as f64 / 100.0 * TEMP_CACHE_PERCENT).round() as i64).max(512_000_000);
    let types = || {
        ["source.local", "exec.cachemount", "source.git.checkout"]
            .into_iter()
            .filter_map(|t| Filter::parse(&format!("type=={t}")).ok())
            .collect::<Vec<_>>()
    };
    let capped = |all, keep_duration| Rule {
        filters: Vec::new(),
        all,
        keep_duration,
        reserved,
        max_used,
        min_free,
    };
    vec![
        Rule {
            filters: types(),
            keep_duration: Some(Duration::from_secs(48 * 3600)),
            max_used: temp,
            ..Rule::default()
        },
        capped(false, Some(Duration::from_secs(60 * 24 * 3600))),
        capped(false, None),
        capped(true, None),
    ]
}

/// How long a collection waits after the last, as BuildKit's controller throttles its GC.
const EVERY: Duration = Duration::from_secs(60);

/// Collects the build cache of `home`'s store as BuildKit's controller does after a build:
/// its default policy, unless a collection ran less than a minute ago (the home's
/// `buildcache-gc`, any process's). What it frees, the store's collector finds due.
pub fn after_build(home: &Path, store: &Store) -> Result<(), String> {
    let stamp = home.join("buildcache-gc");
    let recent = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < EVERY);
    if recent {
        return Ok(());
    }
    std::fs::write(&stamp, b"").map_err(|e| format!("{}: {e}", stamp.display()))?;
    let disk = home.join("images");
    let total = shards_vmm::platform::disk_space(&disk)
        .ok()
        .map(|(_, total)| total);
    let mut freed = 0i64;
    for rule in default_policy(total) {
        freed = freed.saturating_add(prune(store, &disk, &rule, &mut |_| Ok(()))?);
    }
    if freed > 0 {
        let due = crate::pull::collect_due(home);
        std::fs::write(&due, b"").map_err(|e| format!("{}: {e}", due.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, last_used: i64, usage: u64, shared: bool) -> Record {
        Record {
            id: key.into(),
            size: 1,
            created: 0,
            last_used,
            usage,
            shared,
            mount: None,
        }
    }

    #[test]
    fn records_match_filters_by_buildkits_fields() {
        let all = |given: &[&str], r: &Record| {
            given
                .iter()
                .map(|f| Filter::parse(f).unwrap())
                .all(|f| f.matches(r, false))
        };
        let step = record("abcdef", 0, 0, true);
        let mount = Record {
            mount: Some("cached mount /c from exec sh".into()),
            ..record("m1", 0, 0, false)
        };
        assert!(all(&["id~=ab", "shared"], &step));
        assert!(!all(&["id~=ab", "private"], &step));
        assert!(all(&["type==exec.cachemount"], &mount));
        assert!(!all(&["type==exec.cachemount"], &step));
        assert!(all(&["mutable"], &mount) && !all(&["immutable"], &mount));
        assert!(all(&["immutable"], &step) && !all(&["mutable"], &step));
        assert!(all(&["description~=cached mount /c"], &mount));
        assert!(!all(&["description"], &step));
        assert!(!all(&["inuse"], &mount));
        assert!(Filter::parse("inuse").unwrap().matches(&mount, true));
        // A step's key matched by the 25 characters shown of it.
        let long = record(&"a".repeat(64), 0, 0, false);
        let shown = format!("id=={}", "a".repeat(25));
        assert!(all(&[shown.as_str()], &long));
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

    /// A store of its own, removed when dropped.
    /// Under a cap, the least used go first, one at a time, until what stays is under it;
    /// a cache mount a step holds stays whatever the rule.
    #[test]
    fn a_prune_takes_the_least_used_until_under_its_cap() {
        use shards_image::store::{MountLayer, Sharing};
        let tmp = shards_testdir::TempDir::new("gc").unwrap();
        let store = Store::open(&tmp).unwrap();
        for (key, size, uses) in [("aa", 100, 0), ("bb", 200, 0), ("cc", 300, 3)] {
            store.cache_put(key, &[], size, "[]").unwrap();
            for _ in 0..uses {
                store.cache_used(key).unwrap();
            }
        }
        let layer = |size| MountLayer {
            blob: "sha256:00".into(),
            diff_id: "sha256:00".into(),
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            size,
        };
        let held = store
            .take_mount("/c", Sharing::Shared, "cached mount /c", "held")
            .unwrap();
        store
            .used_mount("/c", &held, Some(layer(1000)), Default::default())
            .unwrap();
        // 1,600 bytes, the held cache mount's counted, 1,350 kept: `aa`, then `bb`, the least
        // used, go; `cc` and the held cache mount stay.
        let mut gone = Vec::new();
        let rule = Rule {
            max_used: 1350,
            ..Rule::default()
        };
        let freed = prune(&store, &tmp, &rule, &mut |r| {
            gone.push(r.id.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!((gone, freed), (vec!["aa".to_string(), "bb".to_string()], 300));
        // Every record, with no cap: all but the one held.
        let mut gone = Vec::new();
        prune(&store, &tmp, &Rule::default(), &mut |r| {
            gone.push(r.id.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(gone, ["cc"]);
        drop(held);
        let all = Rule {
            filters: vec![Filter::parse("type==exec.cachemount").unwrap()],
            ..Rule::default()
        };
        assert_eq!(prune(&store, &tmp, &all, &mut |_| Ok(())).unwrap(), 1000);
    }

    /// What a step holds is neither removed nor ranked, as BuildKit ranks only what it may
    /// delete: ranked with the two held, `c` would go before `a`.
    #[test]
    fn what_a_step_holds_is_neither_removed_nor_ranked() {
        let order: Vec<String> = removable(
            vec![
                (record("a", 1, 2, false), false),
                (record("b", 1, 3, false), false),
                (record("c", 2, 1, false), false),
                (record("held-1", 3, 1, false), true),
                (record("held-2", 4, 1, false), true),
            ],
            &Rule::default(),
            1,
            Duration::ZERO,
        )
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(order, ["a", "c", "b"]);
    }

    #[test]
    fn the_least_used_go_first() {
        let order: Vec<String> = sort_for_deletion(vec![
            record("new-busy", 30, 9, false),
            record("old-idle", 10, 1, false),
            record("mid", 20, 5, false),
        ])
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(order, ["old-idle", "mid", "new-busy"]);
    }

    /// dockerd's default policy, derived from the disk as `DefaultGCPolicy` derives it: on
    /// a disk of 2,014 GiB, `docker buildx inspect` in shards-dind (Docker 29.3.1, M141)
    /// shows these rules, 25.99GiB, 188.1GiB, 1.466TiB and 375.3GiB as go-units says them.
    #[test]
    fn the_default_policy_is_dockerds_for_the_disk() {
        let rules = default_policy(Some(2014 * (1 << 30) + 1));
        let shown: Vec<(usize, bool, Option<u64>, i64, i64, i64)> = rules
            .iter()
            .map(|r| {
                (
                    r.filters.len(),
                    r.all,
                    r.keep_duration.map(|d| d.as_secs() / 3600),
                    r.reserved,
                    r.max_used,
                    r.min_free,
                )
            })
            .collect();
        let (reserved, max, min_free) = (202_000_000_000, 1_612_000_000_000, 403_000_000_000);
        assert_eq!(
            shown,
            [
                (3, false, Some(48), 0, 27_911_512_059, 0),
                (0, false, Some(1440), reserved, max, min_free),
                (0, false, None, reserved, max, min_free),
                (0, true, None, reserved, max, min_free),
            ]
        );
        // As go-units' BytesSize says them, the way `buildx inspect` shows them.
        #[allow(clippy::cast_precision_loss)]
        let gib = |b: i64| b as f64 / f64::from(1u32 << 30);
        assert_eq!(format!("{:.2}", gib(27_911_512_059)), "25.99");
        assert_eq!(format!("{:.1}", gib(reserved)), "188.1");
        assert_eq!(format!("{:.3}", gib(max) / 1024.0), "1.466");
        assert_eq!(format!("{:.1}", gib(min_free)), "375.3");
        // A small disk: the temporary share at its floor, 512 MB.
        assert_eq!(
            default_policy(Some(20 << 30)).first().map(|r| r.max_used),
            Some(512_000_000)
        );
        // No disk read: 2 GB reserved, no cap.
        let unread = default_policy(None);
        assert_eq!(
            unread.get(1).map(|r| (r.reserved, r.max_used, r.min_free)),
            Some((2_000_000_000, 0, 0))
        );
        assert_eq!(unread.first().map(|r| r.max_used), Some(512_000_000));
    }
}
