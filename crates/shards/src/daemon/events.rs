//! `shards events`: what happens to microVMs and images, as dockerd tells it (moby
//! daemon/events/events.go, daemon/events.go, daemon/events/filter.go;
//! daemon/server/router/system/system_routes.go, getEvents). Each is kept in the home's
//! journal for `--since` (D187), where dockerd keeps its last 256 in its memory, gone with
//! it; each asker hears the rest as they happen, filtered as it asked, until it goes or
//! `--until` passes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use shards_cmdline::flags::Parsed;
use shards_cmdline::gotime;

use super::commands::{Asker, Reply, rfc3339_nano};
use super::filters::Filters;
use super::{Daemon, lock};
use crate::spec::LOG_STDOUT;

/// The journal's directory in the home.
const JOURNAL: &str = "events";

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
    journal: std::sync::Mutex<Journal>,
    listeners: std::sync::Mutex<Vec<(u64, Filters, mpsc::Sender<Heard>)>>,
    next: AtomicU64,
}

/// The home's journal of events (D187): its `events` directory, segments of an event a
/// line as `--format json` prints one, each named by its number, the newest written as
/// events happen. A segment is begun as the last fills, each of the size the home's
/// `disk` settings give its events' history (`configure disk --events-history`, a log's
/// default: D186), the oldest removed so that the journal keeps at most that history.
/// What a daemon heard before it ended, by its idle exit too (D185), is read back for
/// `--since` by the next.
#[derive(Debug, Default)]
struct Journal {
    /// The home; none where the daemon keeps no journal (a test's).
    home: Option<PathBuf>,
    /// The segment written: its number, its file, and its length.
    open: Option<(u64, File, u64)>,
    /// How the history is kept, as read when the segment written was begun.
    retention: Option<crate::spec::LogRetention>,
    /// Whether a write failed and was said: once, the journal going on.
    failed: bool,
}

/// A listener's: where the journal was written to as it came, if there is one, its number,
/// and its end of the channel it hears on.
type Listening = (
    Option<(PathBuf, End)>,
    u64,
    mpsc::Sender<Heard>,
    mpsc::Receiver<Heard>,
);

/// How far the journal was written as a listener was taken: what is up to it is read
/// from the journal, and what comes after it is heard.
#[derive(Debug, Clone, Copy, Default)]
struct End {
    seq: u64,
    len: u64,
}

impl Journal {
    fn dir(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.join(JOURNAL))
    }

    /// Appends `line`, an event's, beginning a segment where the last is full.
    fn append(&mut self, line: &str) {
        let Some(dir) = self.dir() else {
            return;
        };
        if let Err(e) = self.write(&dir, line.as_bytes()) {
            // Not written: the next is written to a segment found anew.
            self.open = None;
            if !self.failed {
                self.failed = true;
                super::log(format!("{}: an event not kept: {e}", dir.display()));
            }
        }
    }

    fn write(&mut self, dir: &Path, bytes: &[u8]) -> io::Result<()> {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let (seq, len) = match &self.open {
            Some((seq, _, len)) => (*seq, *len),
            None => {
                let (seq, file, len) = self.resume(dir)?;
                self.open = Some((seq, file, len));
                (seq, len)
            }
        };
        let most = self.retention.map_or(u64::MAX, |r| r.size);
        if len > 0 && len.saturating_add(size) > most {
            self.begin(dir, seq.saturating_add(1))?;
        }
        let Some((_, file, len)) = self.open.as_mut() else {
            return Err(io::Error::other("no segment open"));
        };
        file.write_all(bytes)?;
        *len = len.saturating_add(size);
        Ok(())
    }

    /// The newest segment, to write on, where it ends with a whole line: one a write cut
    /// short is let be, and the next begun.
    fn resume(&mut self, dir: &Path) -> io::Result<(u64, File, u64)> {
        std::fs::create_dir_all(dir)?;
        if let Some(&seq) = segments(dir)?.last() {
            use std::os::unix::fs::FileExt as _;
            let path = dir.join(seq.to_string());
            let file = std::fs::OpenOptions::new().append(true).read(true).open(&path)?;
            let len = file.metadata()?.len();
            let mut last = [0u8; 1];
            let whole = len == 0 || {
                file.read_exact_at(&mut last, len - 1)?;
                last[0] == b'\n'
            };
            if whole {
                self.retention = Some(self.retention_now());
                return Ok((seq, file, len));
            }
            self.begin(dir, seq.saturating_add(1))?;
        } else {
            self.begin(dir, 1)?;
        }
        self.open
            .take()
            .ok_or_else(|| io::Error::other("no segment begun"))
    }

    /// Begins segment `seq`, the history read anew, and removes the oldest past it.
    fn begin(&mut self, dir: &Path, seq: u64) -> io::Result<()> {
        let retention = self.retention_now();
        self.retention = Some(retention);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join(seq.to_string()))?;
        self.open = Some((seq, file, 0));
        let all = segments(dir)?;
        let keep = usize::try_from(retention.files).unwrap_or(usize::MAX).max(1);
        for old in all.iter().take(all.len().saturating_sub(keep)) {
            match std::fs::remove_file(dir.join(old.to_string())) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// The history the home's settings give, a log's where they do not read (said where
    /// a run's start reads them: D186).
    fn retention_now(&self) -> crate::spec::LogRetention {
        self.home
            .as_ref()
            .and_then(|h| crate::disk::read(h).ok())
            .unwrap_or_default()
            .events_retention()
    }

    /// Where it ends now.
    fn end(&self) -> End {
        match &self.open {
            Some((seq, _, len)) => End { seq: *seq, len: *len },
            None => self
                .dir()
                .and_then(|d| {
                    let seq = *segments(&d).ok()?.last()?;
                    let len = std::fs::metadata(d.join(seq.to_string())).ok()?.len();
                    Some(End { seq, len })
                })
                .unwrap_or_default(),
        }
    }
}

/// The segments in `dir`, oldest first.
fn segments(dir: &Path) -> io::Result<Vec<u64>> {
    let mut seqs: Vec<u64> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    seqs.sort_unstable();
    Ok(seqs)
}

/// An event as [`json`] wrote it, a line of the journal; none where the line is not one.
fn parse(line: &[u8]) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    // Its type as one shards logs: the journal is shards' own.
    let kind = match v.get("Type")?.as_str()? {
        "container" => "container",
        "image" => "image",
        "network" => "network",
        "volume" => "volume",
        "daemon" => "daemon",
        _ => return None,
    };
    let actor = v.get("Actor")?;
    let attributes = actor
        .get("Attributes")?
        .as_object()?
        .iter()
        .map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect::<Option<BTreeMap<_, _>>>()?;
    Some(Event {
        kind,
        action: v.get("Action")?.as_str()?.to_string(),
        id: actor.get("ID")?.as_str()?.to_string(),
        attributes,
        time_nano: i128::from(v.get("timeNano")?.as_i64()?),
    })
}

/// The time of the first event in segment `seq` of `dir`, where it has a whole one.
fn first_time(dir: &Path, seq: u64) -> Option<i128> {
    use std::io::BufRead as _;
    let file = File::open(dir.join(seq.to_string())).ok()?;
    let mut line = Vec::new();
    io::BufReader::new(file).read_until(b'\n', &mut line).ok()?;
    parse(&line).map(|e| e.time_nano)
}

/// Each event in `dir`'s journal up to `end` from `since` to `until` that passes
/// `filters`, oldest first, to `each`, until it says to stop: from the newest segment that
/// begins no later than `since`, each read whole, one at a time.
fn past(
    dir: &Path,
    end: End,
    since: Option<i128>,
    until: Option<i128>,
    filters: &Filters,
    each: &mut dyn FnMut(&Event) -> bool,
) -> bool {
    let Ok(all) = segments(dir) else {
        return true;
    };
    let all: Vec<u64> = all.into_iter().filter(|s| *s <= end.seq).collect();
    let start = match since {
        Some(s) => all
            .iter()
            .rposition(|seq| first_time(dir, *seq).is_some_and(|t| t <= s))
            .unwrap_or(0),
        None => 0,
    };
    for &seq in all.get(start..).unwrap_or_default() {
        let Ok(mut bytes) = std::fs::read(dir.join(seq.to_string())) else {
            // Removed as the history moved on: what it held is gone.
            continue;
        };
        if seq == end.seq {
            bytes.truncate(usize::try_from(end.len).unwrap_or(usize::MAX));
        }
        for line in bytes.split(|b| *b == b'\n') {
            let Some(e) = parse(line) else {
                continue;
            };
            if until.is_some_and(|u| e.time_nano > u) {
                return true;
            }
            if since.is_none_or(|s| e.time_nano >= s) && include(filters, &e) && !each(&e) {
                return false;
            }
        }
    }
    true
}

impl Events {
    /// Events kept in `home`'s journal.
    pub(super) fn kept_in(home: &Path) -> Events {
        Events {
            journal: std::sync::Mutex::new(Journal {
                home: Some(home.to_path_buf()),
                ..Journal::default()
            }),
            ..Events::default()
        }
    }

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
        let mut journal = lock(&self.journal);
        journal.append(&json(&event));
        // Under the journal's lock, as dockerd publishes under its own: a listener gets
        // each event once, read from the journal or heard.
        lock(&self.listeners).retain(|(_, filters, to)| {
            !include(filters, &event) || to.send(Heard::Event(event.clone())).is_ok()
        });
    }

    /// Listens with `filters`: what is heard from now on, and how far the journal was
    /// written, which what was kept before is read up to ([`Events::past`]).
    fn listen(&self, filters: Filters) -> Listening {
        let (to, from) = mpsc::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let journal = lock(&self.journal);
        let kept = journal.dir().map(|d| (d, journal.end()));
        lock(&self.listeners).push((id, filters, to.clone()));
        (kept, id, to, from)
    }

    /// The kept events from `since` to `until` that pass `filters`, up to where `listen`
    /// found the journal written (loadBufferedEvents; none unless either is given), to
    /// `each` until it says to stop: whether it did not.
    fn past(
        kept: Option<&(PathBuf, End)>,
        since: Option<i128>,
        until: Option<i128>,
        filters: &Filters,
        each: &mut dyn FnMut(&Event) -> bool,
    ) -> bool {
        match kept {
            Some((dir, end)) if since.is_some() || until.is_some() => {
                past(dir, *end, since, until, filters, each)
            }
            _ => true,
        }
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

/// The times events are watched from and to, each given as `by` names it (`--since`, or
/// the API's `since`): read as the client reads them, then as the daemon reads that; none
/// where not given. A start after the end is refused.
pub(super) fn bounds(given: [(&str, &str); 2], now: i128, offset: i64) -> Result<[Option<i128>; 2], String> {
    let mut out = [None, None];
    for ((value, by), bound) in given.iter().zip(&mut out) {
        if value.is_empty() {
            continue;
        }
        let sent = gotime::get_timestamp(value, now, offset).map_err(|e| format!("{by} {value}: {e}"))?;
        *bound = gotime::parse_unix_timestamp(&sent).map_err(|e| format!("{by} {value}: {e}"))?;
    }
    if let [Some(s), Some(u)] = out
        && u < s
    {
        let [(since, since_by), (until, until_by)] = given;
        return Err(format!("{since_by} {since} is after {until_by} {until}"));
    }
    Ok(out)
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
        let given = [
            (parsed.string("since"), "--since"),
            (parsed.string("until"), "--until"),
        ];
        let [since, until] = match bounds(given, i128::from(asker.now), i64::from(asker.utc_offset)) {
            Ok(b) => b,
            Err(e) => {
                reply.err(&format!("shards: {e}"));
                return 1;
            }
        };
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
        let (kept, id, to, from) = self.events.listen(filters.clone());
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
        let mut going = Events::past(kept.as_ref(), since, until, &filters, &mut |e| show(e));
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// What `events` kept in `home`'s journal, read as `--since` reads it: all of it.
    fn read_back(home: &Path) -> Vec<Event> {
        let events = Events::kept_in(home);
        let (kept, id, _to, _from) = events.listen(Filters::default());
        let mut got = Vec::new();
        Events::past(kept.as_ref(), Some(0), None, &Filters::default(), &mut |e| {
            got.push(e.clone());
            true
        });
        events.forget(id);
        got
    }

    fn named(n: usize) -> BTreeMap<String, String> {
        [("name".to_string(), format!("run-{n}"))].into()
    }

    /// Events are kept in the home for the next daemon, each as `--format json` prints it,
    /// read back whole: its type, action, actor, attributes and time.
    #[test]
    fn events_are_kept_in_the_homes_journal_for_the_next_daemon() {
        let home = shards_testdir::TempDir::new("events-kept").unwrap();
        let first = Events::kept_in(&home);
        for n in 0..3 {
            first.log("container", "start", &format!("id{n}"), named(n));
        }
        let odd = "a\"b\\\n\u{2028}é";
        first.log(
            "image",
            "pull",
            "alpine:3",
            [("name".to_string(), odd.to_string())].into(),
        );
        drop(first);
        let got = read_back(&home);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(
            (got[0].kind, got[0].action.as_str(), got[0].id.as_str()),
            ("container", "start", "id0")
        );
        assert_eq!(got[2].attributes, named(2));
        assert_eq!(got[3].attributes["name"], odd);
        assert!(got.windows(2).all(|w| w[0].time_nano <= w[1].time_nano));
        // Each as it was written.
        assert_eq!(parse(json(&got[3]).trim_end().as_bytes()).unwrap(), got[3]);
    }

    /// The journal keeps the history the home's settings give, in two segments below
    /// 40 MiB, the oldest going as the newest begins: the newest events stay, in order.
    #[test]
    fn the_journal_keeps_its_history_and_lets_the_oldest_go() {
        let home = shards_testdir::TempDir::new("events-history").unwrap();
        std::fs::write(home.join("disk"), "events-history 2000\n").unwrap();
        let events = Events::kept_in(&home);
        for n in 0..200 {
            events.log("container", "start", &format!("id{n}"), named(n));
        }
        let dir = home.join(JOURNAL);
        let kept = segments(&dir).unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        let bytes: u64 = kept
            .iter()
            .map(|s| std::fs::metadata(dir.join(s.to_string())).unwrap().len())
            .sum();
        assert!(bytes <= 2000, "{bytes}");
        drop(events);
        let got = read_back(&home);
        assert_eq!(got.last().unwrap().id, "id199");
        assert!(got.len() < 200 && !got.is_empty());
        let ids: Vec<usize> = got
            .iter()
            .map(|e| e.id.strip_prefix("id").unwrap().parse().unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[1] == w[0] + 1), "{ids:?}");
    }

    /// A line a write cut short is no event: the next daemon begins a segment past it,
    /// and what is read back is every whole event.
    #[test]
    fn a_line_cut_short_is_passed_by() {
        let home = shards_testdir::TempDir::new("events-cut").unwrap();
        let first = Events::kept_in(&home);
        first.log("container", "start", "whole", BTreeMap::new());
        drop(first);
        let dir = home.join(JOURNAL);
        let last = *segments(&dir).unwrap().last().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(last.to_string()))
            .unwrap();
        file.write_all(b"{\"Type\":\"container\",\"Act").unwrap();
        drop(file);
        let next = Events::kept_in(&home);
        next.log("container", "die", "after", BTreeMap::new());
        drop(next);
        assert_eq!(segments(&dir).unwrap().len(), 2);
        let got: Vec<String> = read_back(&home).into_iter().map(|e| e.id).collect();
        assert_eq!(got, ["whole", "after"]);
    }

    /// A listener hears what comes after it listens, and reads what came before up to
    /// then: no event both, none neither.
    #[test]
    fn a_listener_reads_what_came_before_and_hears_what_comes_after() {
        let home = shards_testdir::TempDir::new("events-listen").unwrap();
        let events = Events::kept_in(&home);
        events.log("container", "create", "before", BTreeMap::new());
        let (kept, id, _to, from) = events.listen(Filters::default());
        events.log("container", "start", "after", BTreeMap::new());
        let mut read = Vec::new();
        Events::past(kept.as_ref(), Some(0), None, &Filters::default(), &mut |e| {
            read.push(e.id.clone());
            true
        });
        assert_eq!(read, ["before"]);
        match from.try_recv() {
            Ok(Heard::Event(e)) => assert_eq!(e.id, "after"),
            _ => panic!("nothing heard"),
        }
        assert!(from.try_recv().is_err());
        events.forget(id);
    }

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
    fn events_print_line_by_line() {
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
    fn events_are_filtered() {
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
