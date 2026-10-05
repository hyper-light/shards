//! `--filter`'s conditions as dockerd holds and matches them (moby
//! daemon/internal/filters/parse.go, Args): each name with its set of values.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Filters(BTreeMap<String, BTreeSet<String>>);

impl Filters {
    /// The filters given as `name=value` (flags::value made them so; an empty one is
    /// none).
    pub(super) fn from_flags(given: &[String]) -> Filters {
        let mut filters = Filters::default();
        for f in given {
            if let Some((name, value)) = f.split_once('=') {
                filters
                    .0
                    .entry(name.to_string())
                    .or_default()
                    .insert(value.to_string());
            }
        }
        filters
    }

    /// Whether `name` is filtered on at all (Contains).
    pub(super) fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// The values `name` has (Get).
    pub(super) fn get(&self, name: &str) -> impl Iterator<Item = &str> {
        self.0.get(name).into_iter().flatten().map(String::as_str)
    }

    /// No values for `name`, or `source` among them (ExactMatch).
    pub(super) fn exact(&self, name: &str, source: &str) -> bool {
        self.0
            .get(name)
            .is_none_or(|v| v.is_empty() || v.contains(source))
    }

    /// [`exact`](Self::exact), or a value `source` starts with (FuzzyMatch).
    pub(super) fn fuzzy(&self, name: &str, source: &str) -> bool {
        self.exact(name, source) || self.get(name).any(|prefix| source.starts_with(prefix))
    }

    /// No values for `name`, or each, `key` or `key=value`, in `sources` (MatchKVList).
    pub(super) fn kv(&self, name: &str, sources: &BTreeMap<String, String>) -> bool {
        let Some(values) = self.0.get(name).filter(|v| !v.is_empty()) else {
            return true;
        };
        if sources.is_empty() {
            return false;
        }
        values.iter().all(|value| match value.split_once('=') {
            Some((k, v)) => sources.get(k).is_some_and(|s| s == v),
            None => sources.contains_key(value.as_str()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_as_dockerds_do() {
        let f = Filters::from_flags(&["type=container".into(), "container=we".into(), String::new()]);
        assert!(f.exact("type", "container") && !f.exact("type", "image"));
        assert!(f.exact("event", "anything"), "a name not filtered on");
        assert!(f.fuzzy("container", "web") && !f.fuzzy("container", "db"));
        let labels: BTreeMap<String, String> = [("a".to_string(), "1".to_string())].into();
        assert!(Filters::from_flags(&["label=a".into()]).kv("label", &labels));
        assert!(Filters::from_flags(&["label=a=1".into()]).kv("label", &labels));
        assert!(!Filters::from_flags(&["label=a=2".into()]).kv("label", &labels));
        assert!(!Filters::from_flags(&["label=b".into()]).kv("label", &labels));
        assert!(!Filters::from_flags(&["label=a".into()]).kv("label", &BTreeMap::new()));
        assert!(Filters::default().kv("label", &BTreeMap::new()));
    }
}
