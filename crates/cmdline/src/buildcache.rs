//! What `builder prune` and `buildx du` ask BuildKit for, from their `--filter`s: buildx
//! v0.37.1's `toBuildkitPruneInfo` (commands/prune.go) over docker/cli's FilterOpt
//! (opts/opts.go), held to buildx's own answers by tests/buildx_du.rs.

use std::collections::{BTreeMap, BTreeSet};

/// A prune's or a listing's filters, as buildx asks BuildKit for them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneInfo {
    /// `until`, or the older `unused-for`, in nanoseconds: the age a record must reach to
    /// be pruned. `du` asks for none.
    pub keep_duration: Option<i64>,
    /// BuildKit's filters: `key` alone, `id~=value`, `key=value` where the key ends in `!`
    /// or `~`, else `key==value`.
    pub filters: Vec<String>,
    /// Whether any filter was given, `until` among them: FilterOpt's map is not empty.
    pub given: bool,
}

/// FilterOpt's map of the `--filter`s given (each `name=value`, the name lowercased and
/// both trimmed, a value given twice once, an empty filter none), then
/// toBuildkitPruneInfo's reading of it.
pub fn prune_info(given: &[String]) -> Result<PruneInfo, String> {
    let mut by_key: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for g in given.iter().filter(|g| !g.is_empty()) {
        let (k, v) = g
            .split_once('=')
            .ok_or("bad format of filter (expected name=value)")?;
        by_key
            .entry(k.trim().to_lowercase())
            .or_default()
            .insert(v.trim().to_string());
    }
    if by_key.contains_key("until") && by_key.contains_key("unused-for") {
        return Err("conflicting filters \"until\" and \"unused-for\"".into());
    }
    let until_key = if by_key.contains_key("unused-for") {
        "unused-for"
    } else {
        "until"
    };
    let one = |k: &str| format!("{} filter expects only one value", crate::go::quote(k));
    let keep_duration = match by_key
        .get(until_key)
        .map(|v| v.iter().collect::<Vec<_>>())
        .as_deref()
    {
        None | Some([]) => None,
        Some([v]) => Some(crate::gotime::duration(v).map_err(|e| {
            format!(
                "{} filter expects a duration (e.g., '24h'): {e}",
                crate::go::quote(until_key)
            )
        })?),
        Some(_) => return Err(one(until_key)),
    };
    let mut filters = Vec::new();
    for (k, values) in &by_key {
        if k == until_key {
            continue;
        }
        let mut values = values.iter();
        filters.push(match (values.next(), values.next()) {
            (None, _) => k.clone(),
            (Some(v), None) if k == "id" => format!("{k}~={v}"),
            (Some(v), None) if k.ends_with('!') || k.ends_with('~') => format!("{k}={v}"),
            (Some(v), None) => format!("{k}=={v}"),
            (Some(_), Some(_)) => return Err(one(k)),
        });
    }
    Ok(PruneInfo {
        keep_duration,
        filters,
        given: !by_key.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn filters_are_read_as_filteropt_reads_them() {
        let info = prune_info(&v(&[" Type = regular ", "type=regular", "", "id=ab", "until=1h"])).unwrap();
        assert_eq!(info.filters, ["id~=ab", "type==regular"]);
        assert_eq!(info.keep_duration, Some(3_600_000_000_000));
        assert!(info.given);
        assert!(!prune_info(&v(&[""])).unwrap().given);
        assert_eq!(
            prune_info(&v(&["until=x"])).unwrap_err(),
            "\"until\" filter expects a duration (e.g., '24h'): time: invalid duration \"x\""
        );
    }
}
