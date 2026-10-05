//! `shards events`: what happens to microVMs and images, as dockerd tells it (moby
//! daemon/events/events.go, daemon/events.go, daemon/events/filter.go;
//! daemon/server/router/system/system_routes.go, getEvents). The last 256 are kept for
//! `--since`, as dockerd keeps them; each asker hears the rest as they happen, filtered as
//! it asked, until it goes or `--until` passes.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use shards_cmdline::flags::Parsed;
use shards_cmdline::gotime;

use super::commands::{Asker, Reply, rfc3339_nano};
use super::filters::Filters;
use super::{Daemon, lock};
use crate::spec::LOG_STDOUT;

/// How many events dockerd keeps for `--since` (events.go, eventsLimit).
const KEPT: usize = 256;

/// An event (moby api/types/events, Message): its type, what happened, to what, and
/// when, in nanoseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Event {
    pub kind: &'static str,
    pub action: String,
    pub id: String,
    pub attributes: BTreeMap<String, String>,
    pub time_nano: i128,
}

/// What a listener hears: an event, or that its asker has gone.
pub(super) enum Heard {
    Event(Event),
    Gone,
}

/// The events kept, and who is listening, each with its filters.
#[derive(Default)]
pub(super) struct Events {
    kept: std::sync::Mutex<VecDeque<Event>>,
    listeners: std::sync::Mutex<Vec<(u64, Filters, mpsc::Sender<Heard>)>>,
    next: AtomicU64,
}

impl Events {
    /// Records an event (events.go, Log), and tells each listener it passes the filters of.
    pub(super) fn log(
        &self,
        kind: &'static str,
        action: impl Into<String>,
        id: &str,
        attributes: BTreeMap<String, String>,
    ) {
        let event = Event {
            kind,
            action: action.into(),
            id: id.to_string(),
            attributes,
            time_nano: crate::spec::now() as i128,
        };
        let mut kept = lock(&self.kept);
        if kept.len() == KEPT {
            kept.pop_front();
        }
        kept.push_back(event.clone());
        // Under `kept`'s lock, as dockerd publishes under its own: a listener gets each
        // event once, kept or heard.
        lock(&self.listeners).retain(|(_, filters, to)| {
            !include(filters, &event) || to.send(Heard::Event(event.clone())).is_ok()
        });
    }

    /// Listens: the kept events from `since` to `until` that pass `filters`
    /// (loadBufferedEvents; none unless either is given), and what is heard after them.
    fn listen(
        &self,
        since: Option<i128>,
        until: Option<i128>,
        filters: Filters,
    ) -> (Vec<Event>, u64, mpsc::Sender<Heard>, mpsc::Receiver<Heard>) {
        let (to, from) = mpsc::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let kept = lock(&self.kept);
        let past = if since.is_none() && until.is_none() {
            Vec::new()
        } else {
            kept.iter()
                .filter(|e| since.is_none_or(|s| e.time_nano >= s))
                .filter(|e| until.is_none_or(|u| e.time_nano <= u))
                .filter(|e| include(&filters, e))
                .cloned()
                .collect()
        };
        lock(&self.listeners).push((id, filters, to.clone()));
        (past, id, to, from)
    }

    fn forget(&self, id: u64) {
        lock(&self.listeners).retain(|(n, _, _)| *n != id);
    }
}

/// Whether `event` passes `filters` (filter.go, Include), for the types shards has:
/// containers, images and the daemon.
fn include(filters: &Filters, event: &Event) -> bool {
    let named = |kind: &str| {
        event.kind != kind
            || filters.fuzzy(kind, &event.id)
            || event
                .attributes
                .get("name")
                .is_some_and(|n| filters.fuzzy(kind, n))
    };
    // matchEvent: exec and health events are matched by their prefix.
    let action = if filters
        .get("event")
        .any(|v| matches!(v, "health_status" | "exec_create" | "exec_start"))
    {
        filters.fuzzy("event", &event.action)
    } else {
        filters.exact("event", &event.action)
    };
    // matchImage: the image's name, with or without its tag.
    let image_attr = if event.kind == "image" { "name" } else { "image" };
    let name = event.attributes.get(image_attr).map_or("", String::as_str);
    let image = filters.exact("image", &event.id)
        || filters.exact("image", name)
        || filters.exact("image", &familiar_name(&event.id))
        || filters.exact("image", &familiar_name(name));
    action
        && filters.exact("type", event.kind)
        && (!filters.contains("scope") || filters.exact("scope", "local"))
        && [
            "daemon",
            "container",
            "plugin",
            "volume",
            "network",
            "service",
            "node",
            "secret",
            "config",
        ]
        .iter()
        .all(|k| named(k))
        && image
        && (!filters.contains("label") || filters.kv("label", &event.attributes))
}

/// An image reference without its tag or digest, as distribution/reference's
/// FamiliarName gives it; what does not parse, as it is (filter.go, stripTag).
fn familiar_name(image: &str) -> String {
    shards_image::reference::Reference::parse_normalized(image)
        .map(|r| r.familiar_name())
        .unwrap_or_else(|_| image.to_string())
}

/// `time` in nanoseconds as docker/cli prints an event's (events.go, rfc3339NanoFixed:
/// `2006-01-02T15:04:05.000000000Z07:00`), in a zone `offset` seconds east of UTC.
fn stamp(time: i128, offset: i32) -> String {
    let local = time.saturating_add(i128::from(offset) * 1_000_000_000);
    let utc = rfc3339_nano(u64::try_from(local).unwrap_or(0));
    if offset == 0 {
        return utc;
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let off = offset.unsigned_abs();
    format!(
        "{}{sign}{:02}:{:02}",
        utc.strip_suffix('Z').unwrap_or(&utc),
        off / 3600,
        off % 3600 / 60
    )
}

/// An event as docker/cli prints it (events.go, prettyPrintEvent): its time, type,
/// action and actor, then the actor's attributes in key order.
pub(super) fn pretty(event: &Event, offset: i32) -> String {
    let mut line = format!(
        "{} {} {} {}",
        stamp(event.time_nano, offset),
        event.kind,
        event.action,
        event.id
    );
    if !event.attributes.is_empty() {
        let attrs: Vec<String> = event.attributes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        line.push_str(&format!(" ({})", attrs.join(", ")));
    }
    line.push('\n');
    line
}

/// An event as `--format json` prints it: docker/cli's `json` template function
/// (templates.go: encoding/json without HTML escaping) of events.Message as the API sends
/// it from version 1.52, its fields in their struct's order, its attributes' keys sorted.
pub(super) fn json(event: &Event) -> String {
    let mut out = String::from("{\"Type\":");
    go_string(&mut out, event.kind);
    out.push_str(",\"Action\":");
    go_string(&mut out, &event.action);
    out.push_str(",\"Actor\":{\"ID\":");
    go_string(&mut out, &event.id);
    out.push_str(",\"Attributes\":{");
    for (i, (k, v)) in event.attributes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        go_string(&mut out, k);
        out.push(':');
        go_string(&mut out, v);
    }
    out.push_str(&format!(
        "}}}},\"scope\":\"local\",\"time\":{},\"timeNano\":{}}}\n",
        event.time_nano.div_euclid(1_000_000_000),
        event.time_nano
    ));
    out
}

/// `s` as Go's encoding/json writes a string without HTML escaping (encode.go,
/// appendString): `"` and `\\` escaped, `\b \f \n \r \t` short, other controls as
/// `\u00XX`, U+2028 and U+2029 as `\u2028` and `\u2029`.
fn go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// An event as `--format`'s templates see it: Go's events.Message (moby
/// api/types/events/events.go), with its Actor.
#[derive(Debug)]
struct Message(Event);

#[derive(Debug)]
struct Actor(String, BTreeMap<String, String>);

impl shards_template::Object for Message {
    fn type_name(&self) -> &str {
        "events.Message"
    }

    fn field(&self, name: &str) -> Option<shards_template::Value> {
        use shards_template::Value;
        let e = &self.0;
        Some(match name {
            "Type" => Value::String(e.kind.to_string()),
            "Action" => Value::String(e.action.clone()),
            "Actor" => Value::object(Actor(e.id.clone(), e.attributes.clone())),
            "Scope" => Value::String("local".into()),
            "Time" => Value::Int(i64::try_from(e.time_nano.div_euclid(1_000_000_000)).unwrap_or(i64::MAX)),
            "TimeNano" => Value::Int(i64::try_from(e.time_nano).unwrap_or(i64::MAX)),
            _ => return None,
        })
    }

    fn format(&self, out: &mut String) {
        let e = &self.0;
        out.push_str(&format!("{{{} {} ", e.kind, e.action));
        Actor(e.id.clone(), e.attributes.clone()).format(out);
        out.push_str(&format!(
            " local {} {}}}",
            e.time_nano.div_euclid(1_000_000_000),
            e.time_nano
        ));
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push_str(json(&self.0).trim_end());
        Ok(())
    }
}

impl shards_template::Object for Actor {
    fn type_name(&self) -> &str {
        "events.Actor"
    }

    fn field(&self, name: &str) -> Option<shards_template::Value> {
        match name {
            "ID" => Some(shards_template::Value::String(self.0.clone())),
            "Attributes" => Some(shards_template::Value::string_map(self.1.clone())),
            _ => None,
        }
    }

    fn format(&self, out: &mut String) {
        let attrs: Vec<String> = self.1.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        out.push_str(&format!("{{{} map[{}]}}", self.0, attrs.join(" ")));
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push_str("{\"ID\":");
        go_string(out, &self.0);
        out.push_str(",\"Attributes\":{");
        for (i, (k, v)) in self.1.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            go_string(out, k);
            out.push(':');
            go_string(out, v);
        }
        out.push_str("}}");
        Ok(())
    }
}

/// `--format` as docker/cli reads it for events (events.go, makeTemplate): `json` is
/// `{{json .}}`; a template must parse and run against an empty event.
fn template(format: &str) -> Result<Option<shards_template::Template>, String> {
    let text = match format {
        "" => return Ok(None),
        "json" => "{{json .}}",
        other => other,
    };
    let t = shards_template::Template::parse("", text)?;
    let empty = Event {
        kind: "",
        action: String::new(),
        id: String::new(),
        attributes: BTreeMap::new(),
        time_nano: 0,
    };
    t.execute(&shards_template::Value::object(Message(empty)))?;
    Ok(Some(t))
}

impl<D: crate::containers::Disk> Daemon<D> {
    /// Logs image event `action` of image `id` named `name` (moby
    /// daemon/containerd/image_events.go, LogImageEvent).
    pub(super) fn image_event(&self, id: &str, name: &str, action: &str) {
        let mut attributes = BTreeMap::new();
        if !name.is_empty() {
            attributes.insert("name".to_string(), name.to_string());
        }
        self.events.log("image", action, id, attributes);
    }

    /// `shards events [--since T] [--until T] [--filter F]... [--format json]`
    /// (system_routes.go, getEvents; docker/cli system/events.go).
    pub(super) fn events(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let mut bounds = [None, None];
        for (flag, bound) in ["since", "until"].iter().zip(&mut bounds) {
            let given = parsed.string(flag);
            if given.is_empty() {
                continue;
            }
            // What the client makes of it, then what dockerd makes of that.
            let sent = match gotime::get_timestamp(given, i128::from(asker.now), i64::from(asker.utc_offset))
            {
                Ok(sent) => sent,
                Err(e) => {
                    reply.err(&format!("invalid value for \"{flag}\": {e}"));
                    return 1;
                }
            };
            match gotime::parse_unix_timestamp(&sent) {
                Ok(at) => *bound = at,
                Err(e) => {
                    reply.err(&format!(
                        "Error response from daemon: invalid value for '{flag}': {e}"
                    ));
                    return 1;
                }
            }
        }
        let [since, until] = bounds;
        if let (Some(s), Some(u)) = (since, until)
            && u < s
        {
            reply.err(&format!(
                "Error response from daemon: `since` time ({}) cannot be after `until` time ({})",
                parsed.string("since"),
                parsed.string("until")
            ));
            return 1;
        }
        let format = parsed.string("format");
        // As the CLI parses it before it asks (status 64, cli.StatusError).
        let template = match template(format) {
            Ok(t) => t,
            Err(e) => {
                reply.err(&format!("Error parsing format: {e}"));
                return 64;
            }
        };
        let now = crate::spec::now() as i128;
        let only_past = until.is_some_and(|u| u < now);
        let filters = Filters::from_flags(parsed.many("filter"));
        let (past, id, to, from) = self.events.listen(since, until, filters);
        // On a colour terminal, each event a record of the page the client draws as they
        // come, its head with the first.
        let styled = asker.styled() && format.is_empty();
        if styled {
            let mut sheet = shards_ipc::Sheet::new("events");
            sheet.record(&[("kind", "head".into()), ("live", (!only_past).to_string())]);
            if !reply.sheet_taken(&sheet) {
                self.events.forget(id);
                return 0;
            }
        }
        let show = |event: &Event| -> bool {
            if styled {
                let mut sheet = shards_ipc::Sheet::new("events");
                let mut record = vec![
                    ("time", stamp(event.time_nano, asker.utc_offset)),
                    ("type", event.kind.to_string()),
                    ("action", event.action.clone()),
                    ("id", event.id.clone()),
                ];
                for key in ["name", "image", "exitCode", "signal", "oldName"] {
                    if let Some(v) = event.attributes.get(key) {
                        record.push((key, v.clone()));
                    }
                }
                sheet.record(&record);
                return reply.sheet_taken(&sheet);
            }
            let text = match &template {
                None => pretty(event, asker.utc_offset),
                // A template that fails on an event ends the stream with its error, as
                // docker/cli's handleEvent returns it.
                Some(t) => match t.execute(&shards_template::Value::object(Message(event.clone()))) {
                    Ok(mut text) => {
                        text.push('\n');
                        text
                    }
                    Err(e) => {
                        reply.err(&e);
                        return false;
                    }
                },
            };
            reply.bytes(LOG_STDOUT, text.as_bytes()).is_ok()
        };
        let mut going = past.iter().all(&show);
        if going && !only_past {
            std::thread::scope(|scope| {
                // The asker going is heard as it goes: it says nothing, so its socket
                // reads only at its end.
                let gone = to.clone();
                let conn = reply.0;
                let watch = std::thread::Builder::new()
                    .name("events-asker".into())
                    .spawn_scoped(scope, move || {
                        let mut byte = [0u8; 1];
                        let _ = (&*conn).read(&mut byte);
                        let _ = gone.send(Heard::Gone);
                    });
                if watch.is_err() {
                    going = false;
                }
                while going {
                    let heard = match until {
                        Some(u) => {
                            let left = u.saturating_sub(crate::spec::now() as i128).max(0);
                            match from
                                .recv_timeout(Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX)))
                            {
                                Ok(h) => h,
                                Err(_) => break,
                            }
                        }
                        None => match from.recv() {
                            Ok(h) => h,
                            Err(_) => break,
                        },
                    };
                    match heard {
                        Heard::Event(e) => going = show(&e),
                        Heard::Gone => break,
                    }
                }
                self.events.forget(id);
                // The watcher ends as the asker does, after this command's end.
                let _ = (*conn).shutdown(std::net::Shutdown::Read);
            });
        }
        self.events.forget(id);
        drop(to);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &'static str, action: &str, id: &str, attrs: &[(&str, &str)]) -> Event {
        Event {
            kind,
            action: action.into(),
            id: id.into(),
            attributes: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            time_nano: 1_759_651_200_123_456_789,
        }
    }

    #[test]
    fn events_print_as_docker_cli_prints_them() {
        let e = event(
            "container",
            "start",
            "abc",
            &[("name", "web"), ("image", "alpine")],
        );
        assert_eq!(
            pretty(&e, 0),
            "2025-10-05T08:00:00.123456789Z container start abc (image=alpine, name=web)\n"
        );
        assert_eq!(
            pretty(&e, 7200),
            "2025-10-05T10:00:00.123456789+02:00 container start abc (image=alpine, name=web)\n"
        );
        assert_eq!(
            pretty(&e, -12_600),
            "2025-10-05T04:30:00.123456789-03:30 container start abc (image=alpine, name=web)\n"
        );
        assert_eq!(
            json(&e),
            "{\"Type\":\"container\",\"Action\":\"start\",\"Actor\":{\"ID\":\"abc\",\"Attributes\":{\"image\":\"alpine\",\"name\":\"web\"}},\"scope\":\"local\",\"time\":1759651200,\"timeNano\":1759651200123456789}\n"
        );
        let mut odd = String::new();
        go_string(&mut odd, "a\"\\<>&\u{1}\u{8}\n\u{2028}é");
        assert_eq!(odd, "\"a\\\"\\\\<>&\\u0001\\b\\n\\u2028é\"");
    }

    #[test]
    fn events_are_filtered_as_dockerd_filters_them() {
        let start = event(
            "container",
            "start",
            "abc123",
            &[("name", "web"), ("image", "alpine:3"), ("tier", "front")],
        );
        let exec = event("container", "exec_start: sh -c ls", "abc123", &[("name", "web")]);
        let pull = event("image", "pull", "alpine:3", &[("name", "alpine")]);
        let f =
            |given: &[&str]| Filters::from_flags(&given.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(include(&f(&[]), &start));
        assert!(include(&f(&["type=container"]), &start) && !include(&f(&["type=container"]), &pull));
        assert!(include(&f(&["container=we"]), &start), "a name's prefix");
        assert!(include(&f(&["container=abc"]), &start), "an ID's prefix");
        assert!(include(&f(&["container=web"]), &pull), "not a container's event");
        assert!(include(&f(&["event=exec_start"]), &exec), "exec events by prefix");
        assert!(!include(&f(&["event=exec"]), &exec), "others exactly");
        assert!(include(&f(&["image=alpine"]), &start), "an image without its tag");
        assert!(include(&f(&["image=alpine:3"]), &pull));
        assert!(include(&f(&["label=tier=front"]), &start) && !include(&f(&["label=tier=back"]), &start));
        assert!(include(&f(&["scope=local"]), &start) && !include(&f(&["scope=swarm"]), &start));
    }
}
