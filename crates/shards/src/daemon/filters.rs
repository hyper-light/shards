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

    /// These filters but `name`'s (each of its values Del'd).
    pub(super) fn without(&self, name: &str) -> Filters {
        let mut rest = self.clone();
        rest.0.remove(name);
        rest
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

    /// Refuses a filter not among `accepted` (Validate), in dockerd's words.
    pub(super) fn validate(&self, accepted: &[&str]) -> Result<(), String> {
        match self.0.keys().find(|name| !accepted.contains(&name.as_str())) {
            Some(name) => Err(invalid(name, None)),
            None => Ok(()),
        }
    }

    /// `key`'s value as a bool, `default` where it is not filtered on, and an error
    /// where its values say neither or both (GetBoolOrDefault).
    pub(super) fn bool_or(&self, key: &str, default: bool) -> Result<bool, String> {
        let Some(values) = self.0.get(key) else {
            return Ok(default);
        };
        if values.is_empty() {
            return Err(invalid(key, None));
        }
        let is_false = values.contains("0") || values.contains("false");
        let is_true = values.contains("1") || values.contains("true");
        if is_false == is_true {
            let given: Vec<&str> = values.iter().map(String::as_str).collect();
            return Err(invalid(key, Some(&given)));
        }
        Ok(is_true)
    }

    /// What `name`'s values match (Match): each exactly, or as a regular expression
    /// anywhere in the source, one that does not compile matching nothing. Compiled
    /// once for every source, where dockerd compiles each value again for each.
    pub(super) fn matcher(&self, name: &str) -> Matcher<'_> {
        let values = self.0.get(name);
        Matcher {
            values,
            patterns: values
                .into_iter()
                .flatten()
                .filter_map(|v| regex::Regex::new(v).ok())
                .collect(),
        }
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

/// A filter's values, ready to match sources ([`Filters::matcher`]).
pub(super) struct Matcher<'a> {
    values: Option<&'a BTreeSet<String>>,
    patterns: Vec<regex::Regex>,
}

impl Matcher<'_> {
    pub(super) fn matches(&self, source: &str) -> bool {
        match self.values {
            None => true,
            Some(v) if v.is_empty() || v.contains(source) => true,
            Some(_) => self.patterns.iter().any(|p| p.is_match(source)),
        }
    }
}

/// invalidFilter's words (moby daemon/internal/filters/errors.go): the filter, and the
/// values as Go prints a slice.
pub(super) fn invalid(name: &str, values: Option<&[&str]>) -> String {
    match values {
        Some(v) => format!("invalid filter '{name}=[{}]'", v.join(" ")),
        None => format!("invalid filter '{name}'"),
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
    }

    /// moby's parse_test.go cases for Match, GetBoolOrDefault and Validate.
    #[test]
    fn match_bools_and_validation_are_dockerds() {
        let f =
            |given: &[&str]| Filters::from_flags(&given.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let names = f(&["name=^ab", "name=x.z", "name=[bad"]);
        let m = names.matcher("name");
        assert!(m.matches("abc") && m.matches("xyz") && m.matches("[bad"));
        assert!(!m.matches("cab") && !m.matches("bad"));
        assert!(f(&[]).matcher("name").matches("anything"));
        assert_eq!(f(&[]).bool_or("dangling", false), Ok(false));
        assert_eq!(f(&["dangling=true"]).bool_or("dangling", false), Ok(true));
        assert_eq!(f(&["dangling=0"]).bool_or("dangling", true), Ok(false));
        assert_eq!(
            f(&["dangling=true", "dangling=false"]).bool_or("dangling", false),
            Err("invalid filter 'dangling=[false true]'".into())
        );
        assert_eq!(
            f(&["dangling=yes"]).bool_or("dangling", false),
            Err("invalid filter 'dangling=[yes]'".into())
        );
        assert_eq!(f(&["name=a"]).validate(&["name"]), Ok(()));
        assert_eq!(
            f(&["nope=a"]).validate(&["name"]),
            Err("invalid filter 'nope'".into())
        );
        assert!(Filters::default().kv("label", &BTreeMap::new()));
    }
}
