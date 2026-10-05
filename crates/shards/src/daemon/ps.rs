//! Which microVMs `shards ps` lists, as dockerd chooses containers for `docker ps` (moby
//! daemon/list.go: Containers, foldFilter, filterByNameIDMatches, includeContainerInList),
//! its `--filter`s included.

use std::collections::{BTreeMap, HashSet};

use shards_cmdline::flags::Parsed;

use super::filters::Filters;
use super::{Daemon, lock};
use crate::containers::{Container, Registry, State as Life};

/// The filters dockerd takes for a list (acceptedPsFilterTags).
const ACCEPTED: [&str; 16] = [
    "ancestor",
    "annotation",
    "before",
    "exited",
    "id",
    "isolation",
    "label",
    "name",
    "status",
    "health",
    "since",
    "volume",
    "network",
    "is-task",
    "publish",
    "expose",
];

/// The states dockerd has (api/types/container/state.go, validStates).
const STATES: [&str; 7] = [
    "created",
    "running",
    "paused",
    "restarting",
    "removing",
    "exited",
    "dead",
];
/// The health statuses dockerd has (health.go, validHealths).
const HEALTHS: [&str; 4] = ["none", "starting", "healthy", "unhealthy"];

/// The microVMs a list shows, newest first, and whether stopped ones are among them.
pub(super) struct Listing {
    pub containers: Vec<Container>,
    pub all: bool,
}

impl<D: crate::containers::Disk> Daemon<D> {
    /// The microVMs `parsed` (`ps`'s flags) asks for, or dockerd's words for why not.
    pub(super) fn ps_listing(&self, parsed: &Parsed) -> Result<Listing, String> {
        let filters = Filters::from_flags(parsed.many("filter"));
        filters.validate(&ACCEPTED)?;
        // `-l` is `-n 1`, unless `-n` says otherwise (docker/cli list.go).
        let last = match parsed.int("last") {
            -1 if parsed.bool("latest") => 1,
            n => n,
        };
        let limit = usize::try_from(last).ok().filter(|&n| n > 0);
        let mut all = parsed.bool("all");
        // foldFilter.
        let mut exited = Vec::new();
        for value in filters.get("exited") {
            let code = atoi(value).map_err(|e| format!("invalid filter 'exited={value}': {e}"))?;
            exited.push(code);
        }
        for value in filters.get("status") {
            if !STATES.contains(&value) {
                return Err(format!(
                    "invalid filter 'status={value}': invalid value for state ({value}): must be one of {}",
                    STATES.join(", ")
                ));
            }
            all = true;
        }
        let task_filter = filters.contains("is-task");
        let is_task = filters.bool_or("is-task", false)?;
        for value in filters.get("health") {
            if !HEALTHS.contains(&value) {
                return Err(format!(
                    "invalid filter 'health={value}': invalid value for health ({value}): must be one of {}",
                    HEALTHS.join(", ")
                ));
            }
        }
        let registry = lock(&self.containers);
        let mut before = None;
        for value in filters.get("before") {
            before = Some(id_or_name(&registry, value)?);
        }
        let mut since = None;
        for value in filters.get("since") {
            since = Some(id_or_name(&registry, value)?);
        }
        let ancestor = filters.contains("ancestor");
        let images = if ancestor {
            self.ancestors(filters.get("ancestor"))
        } else {
            HashSet::new()
        };
        let mut publish = HashSet::new();
        for value in filters.get("publish") {
            ports("publish", value, &mut publish)?;
        }
        let mut expose = HashSet::new();
        for value in filters.get("expose") {
            ports("expose", value, &mut expose)?;
        }
        let candidates = by_name_or_id(&registry, &filters);
        drop(registry);
        // includeContainerInList, for each, newest first.
        let paused = lock(&self.paused).clone();
        let removing = lock(&self.removing).clone();
        let health = lock(&self.health);
        let (names, ids, statuses) = (
            filters.matcher("name"),
            filters.matcher("id"),
            filters.matcher("status"),
        );
        let no_annotations = BTreeMap::new();
        let mut listed = Vec::new();
        for c in candidates {
            if let Some(b) = &before {
                if c.id == *b {
                    before = None;
                }
                continue;
            }
            if since.as_ref() == Some(&c.id) {
                break;
            }
            // One waiting to restart is running, to dockerd's list (State.Running).
            let running = c.state == Life::Running || c.restart.restarting;
            if !running && !all && limit.is_none() {
                continue;
            }
            if !names.matches(&format!("/{}", c.name)) && !names.matches(&c.name) {
                continue;
            }
            if !ids.matches(&c.id) {
                continue;
            }
            // No microVM is a swarm task.
            if task_filter && is_task {
                continue;
            }
            // Nor has annotations: `--annotation` is unserved.
            if !filters.kv("label", &c.labels) || !filters.kv("annotation", &no_annotations) {
                continue;
            }
            if limit.is_some_and(|l| listed.len() == l) {
                break;
            }
            if !exited.is_empty()
                && !exited
                    .iter()
                    .any(|&code| Some(code) == c.exit_code.map(i64::from) && !running && c.started.is_some())
            {
                continue;
            }
            let state = match c.state {
                Life::Running if paused.contains(&c.id) => "paused",
                Life::Running => "running",
                _ if removing.contains(&c.id) => "removing",
                Life::Created => "created",
                Life::Exited if c.restart.restarting => "restarting",
                Life::Exited => "exited",
            };
            if !statuses.matches(state) {
                continue;
            }
            let healthy = match health.get(&c.id).map(|h| h.status) {
                Some(super::health::Status::Starting) => "starting",
                Some(super::health::Status::Healthy) => "healthy",
                Some(super::health::Status::Unhealthy) => "unhealthy",
                None => "none",
            };
            if !filters.exact("health", healthy) {
                continue;
            }
            // No microVM mounts a volume or joins a network dockerd would name: `-v` and
            // `--network` are unserved.
            if filters.contains("volume") || filters.contains("network") {
                continue;
            }
            if ancestor && !c.image_id.as_ref().is_some_and(|i| images.contains(i)) {
                continue;
            }
            if !publish.is_empty() || !expose.is_empty() {
                // dockerd lists ports while a container runs.
                let shown = if running { c.ports.as_slice() } else { &[] };
                if !shown.iter().any(|p| {
                    publish.contains(&format!("{}/{}", p.public, p.proto))
                        || expose.contains(&format!("{}/{}", p.private, p.proto))
                }) {
                    continue;
                }
            }
            listed.push(c);
        }
        Ok(Listing {
            containers: listed,
            all: all || limit.is_some(),
        })
    }

    /// The images `ancestor`s name, and those made from them, and so on (GetImage, then
    /// populateImageFilterByParents over Children: the images whose parent it is). One
    /// that names nothing is passed over, as dockerd passes it over with a warning.
    fn ancestors<'a>(&self, given: impl Iterator<Item = &'a str>) -> HashSet<String> {
        let mut found = HashSet::new();
        let Ok(Some(store)) = self.store() else {
            return found;
        };
        let Ok(named) = store.named() else {
            return found;
        };
        let mut pending: Vec<String> = given
            .filter_map(|a| super::images::resolve(&named, a).ok())
            .map(|i| i.id.to_string())
            .collect();
        while let Some(id) = pending.pop() {
            if found.insert(id.clone()) {
                pending.extend(
                    named
                        .iter()
                        .filter(|i| i.parent.as_ref().is_some_and(|p| p.to_string() == id))
                        .map(|i| i.id.to_string()),
                );
            }
        }
        found
    }
}

/// The containers a list considers, newest first: every one, or, where names or IDs are
/// filtered on, those an ID's unique prefix names and those whose name matches
/// (filterByNameIDMatches).
fn by_name_or_id(registry: &Registry, filters: &Filters) -> Vec<Container> {
    let names: Vec<&str> = filters.get("name").collect();
    let ids: Vec<&str> = filters.get("id").collect();
    let mut list: Vec<Container> = if names.is_empty() && ids.is_empty() {
        registry.all().cloned().collect()
    } else {
        let mut matched: HashSet<&str> = HashSet::new();
        for prefix in &ids {
            let mut found = registry
                .all()
                .filter(|c| !prefix.is_empty() && c.id.starts_with(prefix));
            if let (Some(c), None) = (found.next(), found.next()) {
                matched.insert(&c.id);
            }
        }
        if !names.is_empty() {
            let m = filters.matcher("name");
            let by_id = !ids.is_empty();
            let named: Vec<&str> = registry
                .all()
                .filter(|c| !by_id || matched.contains(c.id.as_str()))
                .filter(|c| m.matches(&format!("/{}", c.name)) || m.matches(&c.name))
                .map(|c| c.id.as_str())
                .collect();
            matched.extend(named);
        }
        registry
            .all()
            .filter(|c| matched.contains(c.id.as_str()))
            .cloned()
            .collect()
    };
    list.sort_by_key(|c| std::cmp::Reverse(c.created));
    list
}

/// idOrNameFilter: the container whose whole ID is `value`, else the one named it.
fn id_or_name(registry: &Registry, value: &str) -> Result<String, String> {
    if let Some(c) = registry.all().find(|c| c.id == value) {
        return Ok(c.id.clone());
    }
    let name = value.strip_prefix('/').unwrap_or(value);
    registry
        .all()
        .find(|c| c.name == name)
        .map(|c| c.id.clone())
        .ok_or_else(|| format!("No such container: {value}"))
}

/// portOp: the ports `value` (`PORT[-END][/PROTO]`) names, each as `NUM/PROTO`, into
/// `into`.
fn ports(key: &str, value: &str, into: &mut HashSet<String>) -> Result<(), String> {
    if value.contains(':') {
        return Err(format!("filter for '{key}' should not contain ':': {value}"));
    }
    let (start, end, proto) = shards_cmdline::ports::parse_port_range(value)
        .map_err(|e| format!("error while looking up for {key} {value}: {e}"))?;
    for p in start..=end {
        into.insert(format!("{p}/{proto}"));
    }
    Ok(())
}

/// strconv.Atoi's answer and words: an optional sign, then decimal digits, in 64 bits.
fn atoi(s: &str) -> Result<i64, String> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("strconv.Atoi: parsing {}: invalid syntax", go_quote(s)));
    }
    s.parse()
        .map_err(|_| format!("strconv.Atoi: parsing {}: value out of range", go_quote(s)))
}

/// `s` as Go's strconv.Quote writes it, for the printable ASCII a filter holds; other
/// characters escaped as `\u` or `\x`.
fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// moby's port_test.go cases for ParsePortRange, and its words.
    #[test]
    fn port_ranges_parse_as_mobys_do() {
        assert_eq!(
            shards_cmdline::ports::parse_port_range("80"),
            Ok((80, 80, "tcp".into()))
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("80/UDP"),
            Ok((80, 80, "udp".into()))
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("80-82/tcp"),
            Ok((80, 82, "tcp".into()))
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("80-80"),
            Ok((80, 80, "tcp".into()))
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range(""),
            Err("invalid port range: value is empty".into())
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("x"),
            Err("invalid start port 'x': invalid syntax".into())
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("+1"),
            Err("invalid start port '+1': invalid syntax".into())
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("70000"),
            Err("invalid start port '70000': value out of range".into())
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("82-80"),
            Err("invalid port range: 82-80".into())
        );
        assert_eq!(
            shards_cmdline::ports::parse_port_range("80-"),
            Err("invalid end port '': value is empty".into())
        );
        let mut into = HashSet::new();
        assert_eq!(
            ports("publish", "1:2", &mut into),
            Err("filter for 'publish' should not contain ':': 1:2".into())
        );
        ports("expose", "8080-8081", &mut into).unwrap();
        assert!(into.contains("8080/tcp") && into.contains("8081/tcp") && into.len() == 2);
    }

    #[test]
    fn exit_codes_parse_as_atoi_does() {
        assert_eq!(atoi("0"), Ok(0));
        assert_eq!(atoi("-1"), Ok(-1));
        assert_eq!(atoi("+7"), Ok(7));
        assert_eq!(
            atoi("x"),
            Err("strconv.Atoi: parsing \"x\": invalid syntax".into())
        );
        assert_eq!(atoi(""), Err("strconv.Atoi: parsing \"\": invalid syntax".into()));
        assert_eq!(
            atoi("99999999999999999999"),
            Err("strconv.Atoi: parsing \"99999999999999999999\": value out of range".into())
        );
    }
}
