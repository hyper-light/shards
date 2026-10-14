//! The awslogs driver's options and events (D168), as moby's awslogs driver
//! (daemon/logger/awslogs) takes and makes them for CloudWatch Logs' PutLogEvents: each
//! line an event, of at most 262,118 bytes as CloudWatch counts them (its UTF-8, a byte
//! that is not UTF-8 counted as U+FFFD's three), a longer one cut where no character is;
//! or, with `awslogs-multiline-pattern` (or `awslogs-datetime-format`, its strftime made a
//! pattern), lines joined, each followed by a newline, until one matches the pattern. A
//! put's events are at most 10,000 and 1 MiB, each counted 26 bytes more, sorted by time,
//! and, past moby's, span less than the 24 hours CloudWatch takes in one. Their JSON is
//! written as smithy-go's encoder writes it.

use std::collections::BTreeMap;
use std::time::Duration;

/// The options the driver takes besides those every driver does (moby awslogs
/// ValidateLogOpt, with Docker 29.8's entity).
pub const OPTIONS: [&str; 15] = [
    "awslogs-region",
    "awslogs-endpoint",
    "awslogs-group",
    "awslogs-stream",
    "awslogs-create-group",
    "awslogs-create-stream",
    "awslogs-datetime-format",
    "awslogs-multiline-pattern",
    "awslogs-credentials-endpoint",
    "awslogs-force-flush-interval-seconds",
    "awslogs-max-buffered-events",
    "awslogs-format",
    "awslogs-entity-service-name",
    "awslogs-entity-environment",
    "awslogs-entity-attributes",
];

/// What an event costs a put besides its message (perEventBytes).
pub const PER_EVENT: usize = 26;
/// The most of an event's bytes, as CloudWatch counts them (maximumBytesPerEvent): the
/// longest part of a line its shipper cuts, as moby's copier is given its BufSize.
pub const EVENT: usize = 262_144 - PER_EVENT;
/// The most of a put's bytes, each event's counted with [`PER_EVENT`] (maximumBytesPerPut).
pub const PUT_BYTES: usize = 1_048_576;
/// The most of a put's events (maximumLogEventsPerPut).
pub const PUT_EVENTS: usize = 10_000;
/// How long a put's events may span, less a millisecond: CloudWatch takes none that
/// spans more than 24 hours (PutLogEvents), which a shipper catching up on a long log
/// would otherwise send.
pub const SPAN_MS: i64 = 24 * 3600 * 1000;
/// How long a multiline event is held for lines that continue it, unless the options say
/// (defaultForceFlushInterval).
pub const FLUSH: Duration = Duration::from_secs(5);
/// Embedded Metric Format's, `awslogs-format`'s one value, sent as `x-amzn-logs-format`.
pub const EMF: &str = "json/emf";

/// A run's awslogs driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub group: String,
    /// `awslogs-stream`, where given; else the run's tag, by default its container's ID.
    pub stream: Option<String>,
    pub create_group: bool,
    pub create_stream: bool,
    pub region: Option<String>,
    /// `awslogs-endpoint`: CloudWatch Logs' address, where not AWS's own.
    pub endpoint: Option<String>,
    /// `awslogs-credentials-endpoint`: a path at the ECS agent's address that gives the
    /// credentials, in the chain's place.
    pub credentials_endpoint: Option<String>,
    /// The pattern a line that starts an event matches, where lines are joined.
    pub multiline: Option<String>,
    /// `awslogs-force-flush-interval-seconds`: how long a joined event waits for more.
    pub flush: Duration,
    /// `awslogs-format=json/emf`.
    pub emf: bool,
    pub entity: Option<Entity>,
}

/// The CloudWatch entity a run's events are of (Docker 29.8): a service's name and
/// environment, templates of the run's details, and its attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub service: String,
    pub environment: String,
    pub attributes: BTreeMap<String, String>,
}

/// The longest an entity's name, environment or attribute's value is, in characters; an
/// attribute's name's; and how many attributes it has at most (CloudWatch's Entity).
const ENTITY_VALUE: usize = 512;
const ENTITY_KEY: usize = 256;
const ENTITY_ATTRIBUTES: usize = 10;

/// The driver `opts` asks for, or why not, in shards' words: moby's refusals, in its
/// order, then those CloudWatch would make of the run's every put, made as it is created.
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    let get = |k: &str| opts.get(k).filter(|v| !v.is_empty());
    let group = get("awslogs-group")
        .ok_or("--log-opt awslogs-group: the log group to send the log to")?
        .clone();
    let flag = |k: &str| -> Result<Option<bool>, String> {
        get(k)
            .map(|v| super::go_bool(v).ok_or_else(|| format!("--log-opt {k}={v}: true or false")))
            .transpose()
    };
    let create_group = flag("awslogs-create-group")?.unwrap_or(false);
    let create_stream = flag("awslogs-create-stream")?.unwrap_or(true);
    let positive = |k: &str, what: &str| -> Result<Option<u64>, String> {
        get(k)
            .map(|v| match v.parse::<i64>() {
                Ok(n) if n > 0 => Ok(n.unsigned_abs()),
                _ => Err(format!("--log-opt {k}={v}: {what}, more than 0")),
            })
            .transpose()
    };
    let flush = positive(
        "awslogs-force-flush-interval-seconds",
        "a whole number of seconds",
    )?
    .map_or(FLUSH, Duration::from_secs);
    // The log is the run's, kept on disk until sent: no event waits in a queue, so none
    // is ever dropped from one; the option is taken, and holds nothing back.
    positive("awslogs-max-buffered-events", "a whole number of events")?;
    let datetime = opts.get("awslogs-datetime-format");
    let pattern = opts.get("awslogs-multiline-pattern");
    if datetime.is_some() && pattern.is_some() {
        return Err(
            "--log-opt awslogs-datetime-format and awslogs-multiline-pattern: one or the other, each says how an event starts"
                .into(),
        );
    }
    let emf = match get("awslogs-format").map(String::as_str) {
        None => false,
        Some(EMF) if datetime.is_some() || pattern.is_some() => {
            return Err(format!(
                "--log-opt awslogs-format={EMF}: each line is an event of its own, which {} would join",
                if datetime.is_some() {
                    "awslogs-datetime-format"
                } else {
                    "awslogs-multiline-pattern"
                }
            ));
        }
        Some(EMF) => true,
        Some(f) => return Err(format!("--log-opt awslogs-format={f}: {EMF}, or none")),
    };
    let entity = entity(opts)?;
    // Then what CloudWatch would refuse of every put.
    if group.len() > 512
        || !group
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_/#".contains(&b))
    {
        return Err(format!(
            "--log-opt awslogs-group={group}: a log group's name is 1 to 512 of A-Z, a-z, 0-9, '.', '-', '_', '/' and '#'"
        ));
    }
    let stream = get("awslogs-stream").cloned();
    if let Some(s) = &stream {
        stream_name(s).map_err(|why| format!("--log-opt awslogs-stream={s}: {why}"))?;
    }
    let region = get("awslogs-region").cloned();
    if let Some(r) = &region
        && !host_label(r)
    {
        return Err(format!(
            "--log-opt awslogs-region={r}: a region's name, as us-east-1"
        ));
    }
    let endpoint = get("awslogs-endpoint").cloned();
    if let Some(e) = &endpoint
        && !(e.starts_with("http://") || e.starts_with("https://"))
    {
        return Err(format!(
            "--log-opt awslogs-endpoint={e}: CloudWatch Logs' URL, http(s)://HOST[:PORT]"
        ));
    }
    if let Some(e) = &endpoint {
        shards_registry::url::Url::parse(e)
            .map_err(|err| format!("--log-opt awslogs-endpoint={e}: {err}"))?;
    }
    let multiline = match (datetime, pattern) {
        (Some(d), _) => Some(strftime_pattern(d)),
        (_, Some(p)) => Some(p.clone()),
        _ => None,
    }
    .filter(|p| !p.is_empty());
    if let Some(p) = &multiline {
        let key = if datetime.is_some() {
            "awslogs-datetime-format"
        } else {
            "awslogs-multiline-pattern"
        };
        regex::bytes::Regex::new(p).map_err(|e| format!("--log-opt {key}: {e}"))?;
    }
    Ok(Config {
        group,
        stream,
        create_group,
        create_stream,
        region,
        endpoint,
        credentials_endpoint: opts.get("awslogs-credentials-endpoint").cloned(),
        multiline,
        flush,
        emf,
        entity,
    })
}

/// Whether `name` is a log stream's (CloudWatch's LogStreamName): 1 to 512 characters,
/// neither `:` nor `*` among them.
pub fn stream_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.chars().count() > 512 || name.contains([':', '*']) {
        return Err("a log stream's name is 1 to 512 characters, none of them ':' or '*'");
    }
    Ok(())
}

/// Whether `s` is a host's labels, as the SDK's endpoint rules take a region
/// (isValidHostLabel, its subdomains allowed).
fn host_label(s: &str) -> bool {
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// The entity the options give, where they give one (moby ValidateLogOpt, parseEntity):
/// its name and environment together, each a template; its attributes a CSV list of
/// `KEY=VALUE`.
fn entity(opts: &BTreeMap<String, String>) -> Result<Option<Entity>, String> {
    let get = |k: &str| opts.get(k).filter(|v| !v.is_empty());
    let (service, environment) = (
        get("awslogs-entity-service-name"),
        get("awslogs-entity-environment"),
    );
    let raw = get("awslogs-entity-attributes");
    if service.is_none() && environment.is_none() && raw.is_none() {
        return Ok(None);
    }
    let (Some(service), Some(environment)) = (service, environment) else {
        return Err(
            "--log-opt awslogs-entity-service-name and awslogs-entity-environment: both, or neither: an entity is a service in an environment"
                .into(),
        );
    };
    for (k, text) in [
        ("awslogs-entity-service-name", service),
        ("awslogs-entity-environment", environment),
    ] {
        shards_template::Template::parse(k, text).map_err(|e| format!("--log-opt {k}={text}: {e}"))?;
    }
    let attributes = match raw {
        None => BTreeMap::new(),
        Some(raw) => {
            attributes(raw).map_err(|why| format!("--log-opt awslogs-entity-attributes={raw}: {why}"))?
        }
    };
    Ok(Some(Entity {
        service: service.clone(),
        environment: environment.clone(),
        attributes,
    }))
}

/// An entity's attributes (parseEntityAttributes): one CSV record, as Go's encoding/csv
/// reads it with TrimLeadingSpace, of `KEY=VALUE`s, each side trimmed, none empty, no
/// key twice, at most 10.
fn attributes(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let fields = csv_record(raw).ok_or("a comma-separated list of KEY=VALUE, on one line")?;
    let mut out = BTreeMap::new();
    for pair in fields {
        if pair.trim().is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((&pair, ""));
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || v.is_empty() || !pair.contains('=') {
            return Err(format!("{pair}: KEY=VALUE, neither empty"));
        }
        if out.contains_key(k) {
            return Err(format!("{k}: given twice"));
        }
        if k.chars().count() > ENTITY_KEY || v.chars().count() > ENTITY_VALUE {
            return Err(format!(
                "{k}: past CloudWatch's {ENTITY_KEY} characters for a name, {ENTITY_VALUE} for a value"
            ));
        }
        out.insert(k.to_string(), v.to_string());
    }
    if out.len() > ENTITY_ATTRIBUTES {
        return Err(format!(
            "{} attributes, past CloudWatch's {ENTITY_ATTRIBUTES}",
            out.len()
        ));
    }
    Ok(out)
}

/// The fields of `text`'s one CSV record, as Go's csv.Reader reads it, leading spaces
/// trimmed; none where it holds more records than one, or a quote out of place. Blank
/// lines are no records, as Go's are not.
fn csv_record(text: &str) -> Option<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        // A line of its own: a record, unless it is blank.
        if matches!(chars.peek(), Some('\n')) {
            chars.next();
            continue;
        }
        if chars.peek() == Some(&'\r') {
            let mut ahead = chars.clone();
            ahead.next();
            if ahead.peek() == Some(&'\n') {
                chars.next();
                chars.next();
                continue;
            }
        }
        let mut record = Vec::new();
        loop {
            while chars
                .peek()
                .is_some_and(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
            {
                chars.next();
            }
            let mut field = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                loop {
                    match chars.next()? {
                        '"' if chars.peek() == Some(&'"') => {
                            chars.next();
                            field.push('"');
                        }
                        '"' => break,
                        c => field.push(c),
                    }
                }
                match chars.peek() {
                    None | Some(',' | '\n') => {}
                    Some('\r') => {}
                    Some(_) => return None,
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ','
                        || c == '\n'
                        || (c == '\r' && {
                            let mut ahead = chars.clone();
                            ahead.next();
                            ahead.peek().is_none_or(|n| *n == '\n')
                        })
                    {
                        break;
                    }
                    if c == '"' {
                        return None;
                    }
                    field.push(c);
                    chars.next();
                }
            }
            record.push(field);
            match chars.next() {
                Some(',') => continue,
                Some('\r') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    break;
                }
                _ => break,
            }
        }
        records.push(record);
    }
    match <[Vec<String>; 1]>::try_from(records) {
        Ok([record]) => Some(record),
        Err(_) => None,
    }
}

/// `awslogs-datetime-format` made a pattern, as moby makes one: each `%` and the
/// character after it the strftime table's pattern for it, or nothing where the table has
/// none (parseMultilineOptions, strftimeToRegex).
pub fn strftime_pattern(format: &str) -> String {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Go's `%.`: any character but a newline.
        match chars.clone().next() {
            Some(n) if n != '\n' => {
                chars.next();
                out.push_str(match n {
                    'a' => "(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)",
                    'A' => "(?:Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)",
                    'w' => "[0-6]",
                    'd' => "(?:0[1-9]|[1,2][0-9]|3[0,1])",
                    'b' => "(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)",
                    'B' => "(?:January|February|March|April|May|June|July|August|September|October|November|December)",
                    'm' => "(?:0[1-9]|1[0-2])",
                    'Y' => r"\d{4}",
                    'y' => r"\d{2}",
                    'H' => "(?:[0,1][0-9]|2[0-3])",
                    'I' => "(?:0[0-9]|1[0-2])",
                    'p' => "[A,P]M",
                    'M' | 'S' => "[0-5][0-9]",
                    'f' => r"\d{6}",
                    'z' => r"[+-]\d{4}",
                    'Z' => "[A-Z]{1,4}T",
                    'j' => "(?:0[0-9][1-9]|[1,2][0-9][0-9]|3[0-5][0-9]|36[0-6])",
                    'L' => r"\.\d{3}",
                    _ => "",
                });
            }
            _ => out.push('%'),
        }
    }
    out
}

/// An event: its time, in milliseconds since the epoch, its message, the bytes CloudWatch
/// counts of it, and how many of the run's lines end in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at: i64,
    pub message: Vec<u8>,
    pub size: usize,
    pub lines: usize,
}

/// `line`'s characters as Go ranges over a string: each its offset and the bytes
/// CloudWatch counts of it, a byte that is not UTF-8 counted as U+FFFD's three.
fn characters(line: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut at = 0;
    line.utf8_chunks().flat_map(move |chunk| {
        let base = at;
        let valid = chunk.valid();
        at += valid.len() + chunk.invalid().len();
        let bad = base + valid.len();
        valid
            .char_indices()
            .map(move |(i, c)| (base + i, c.len_utf8()))
            .chain((0..chunk.invalid().len()).map(move |i| (bad + i, 3)))
    })
}

/// The bytes CloudWatch counts of `line` (effectiveLen).
pub fn size(line: &[u8]) -> usize {
    characters(line).map(|(_, n)| n).sum()
}

/// `line` cut into events' messages of at most [`EVENT`] bytes as CloudWatch counts them,
/// never within a character (processEvent, findValidSplit): each its bytes and its size.
pub fn cut(line: &[u8]) -> Vec<(&[u8], usize)> {
    let mut out = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (offset, counted) in characters(line) {
        if size + counted > EVENT {
            out.push((line.get(start..offset).unwrap_or_default(), size));
            (start, size) = (offset, 0);
        }
        size += counted;
    }
    if start < line.len() {
        out.push((line.get(start..).unwrap_or_default(), size));
    }
    out
}

/// A run's lines made events, as moby's collectBatch makes them: each line its own; or,
/// with a multiline pattern, lines joined, each followed by a newline (unless it is an
/// event's whole), until a line matches the pattern or the event would be too long; the
/// event's time its first line's.
pub struct Events {
    pattern: Option<regex::bytes::Regex>,
    held: Vec<u8>,
    held_size: usize,
    held_at: i64,
    held_lines: usize,
}

impl Events {
    pub fn new(pattern: Option<regex::bytes::Regex>) -> Events {
        Events {
            pattern,
            held: Vec::new(),
            held_size: 0,
            held_at: 0,
            held_lines: 0,
        }
    }

    /// Takes `line`, made at `at`: the events it completes, onto `out`.
    pub fn take(&mut self, line: &[u8], at: i64, out: &mut Vec<Event>) {
        let Some(pattern) = &self.pattern else {
            push(out, line, at, 1);
            return;
        };
        let counted = size(line);
        if pattern.is_match(line) || self.held_size + counted > EVENT {
            self.finish(out);
        }
        if self.held_lines == 0 {
            self.held_at = at;
        }
        self.held.extend_from_slice(line);
        self.held_size += counted;
        if counted < EVENT {
            self.held.push(b'\n');
            self.held_size += 1;
        }
        self.held_lines += 1;
    }

    /// The event the lines held make, whatever follows them.
    pub fn finish(&mut self, out: &mut Vec<Event>) {
        if self.held_lines > 0 {
            let held = std::mem::take(&mut self.held);
            push(out, &held, self.held_at, self.held_lines);
            self.held_size = 0;
            self.held_lines = 0;
        }
    }

    /// When the lines held were made, and how many they are, where some are.
    pub fn held(&self) -> Option<(i64, usize)> {
        (self.held_lines > 0).then_some((self.held_at, self.held_lines))
    }
}

/// `line`'s events, made at `at`, onto `out`: the last of them where `lines` end. An empty
/// line's is empty, which no put sends, as CloudWatch takes no empty message: it holds
/// its line's place until the events before it are delivered.
fn push(out: &mut Vec<Event>, line: &[u8], at: i64, lines: usize) {
    let pieces = cut(line);
    let n = pieces.len();
    if n == 0 {
        out.push(Event {
            at,
            message: Vec::new(),
            size: 0,
            lines,
        });
        return;
    }
    for (i, (piece, size)) in pieces.into_iter().enumerate() {
        out.push(Event {
            at,
            message: piece.to_vec(),
            size,
            lines: if i + 1 == n { lines } else { 0 },
        });
    }
}

/// A put's events, as many as PutLogEvents takes: each counted with [`PER_EVENT`] more;
/// an empty one, which is sent with none, counted not at all.
#[derive(Debug, Default)]
pub struct Batch {
    pub events: Vec<Event>,
    /// Its events that are sent, and their bytes.
    count: usize,
    bytes: usize,
    span: Option<(i64, i64)>,
}

impl Batch {
    /// Whether `e` fits: its events fewer than [`PUT_EVENTS`], their bytes within
    /// [`PUT_BYTES`], their times within [`SPAN_MS`] of each other (eventBatch.add).
    pub fn fits(&self, e: &Event) -> bool {
        if e.message.is_empty() {
            return true;
        }
        let span = self
            .span
            .map_or((e.at, e.at), |(lo, hi)| (lo.min(e.at), hi.max(e.at)));
        self.count < PUT_EVENTS
            && self.bytes + e.size + PER_EVENT <= PUT_BYTES
            && span.1.saturating_sub(span.0) < SPAN_MS
    }

    pub fn add(&mut self, e: Event) {
        if !e.message.is_empty() {
            self.count += 1;
            self.bytes += e.size + PER_EVENT;
            self.span = Some(
                self.span
                    .map_or((e.at, e.at), |(lo, hi)| (lo.min(e.at), hi.max(e.at))),
            );
        }
        self.events.push(e);
    }

    /// Its events that are sent, sorted by time, those of one time as they came
    /// (eventBatch.events).
    pub fn sorted(&self) -> Vec<&Event> {
        let mut events: Vec<&Event> = self.events.iter().filter(|e| !e.message.is_empty()).collect();
        events.sort_by_key(|e| e.at);
        events
    }

    /// Whether a put has anything to send of it.
    pub fn sends(&self) -> bool {
        self.count > 0
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn lines(&self) -> usize {
        self.events.iter().map(|e| e.lines).sum()
    }

    pub fn clear(&mut self) {
        *self = Batch::default();
    }
}

/// `s` as smithy-go's JSON encoder writes a string (escapeStringBytes): `"` and `\`
/// escaped, newline, return and tab as `\n`, `\r`, `\t`, other control characters
/// `\u00XX`, a byte that is not UTF-8 `�`, U+2028 and U+2029 escaped; the rest as
/// it is.
pub fn json_string(out: &mut Vec<u8>, s: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for chunk in s.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.extend_from_slice(b"\\\""),
                '\\' => out.extend_from_slice(b"\\\\"),
                '\n' => out.extend_from_slice(b"\\n"),
                '\r' => out.extend_from_slice(b"\\r"),
                '\t' => out.extend_from_slice(b"\\t"),
                '\u{2028}' => out.extend_from_slice(b"\\u2028"),
                '\u{2029}' => out.extend_from_slice(b"\\u2029"),
                c if u32::from(c) < 0x20 => {
                    let b = u8::try_from(u32::from(c)).unwrap_or(0);
                    out.extend_from_slice(b"\\u00");
                    for nibble in [b >> 4, b & 0xf] {
                        out.push(HEX.get(usize::from(nibble)).copied().unwrap_or(b'0'));
                    }
                }
                c => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        for _ in chunk.invalid() {
            out.extend_from_slice(b"\\ufffd");
        }
    }
    out.push(b'"');
}

/// CreateLogGroup's body.
pub fn create_group(group: &str) -> Vec<u8> {
    let mut out = b"{\"logGroupName\":".to_vec();
    json_string(&mut out, group.as_bytes());
    out.push(b'}');
    out
}

/// CreateLogStream's body.
pub fn create_stream(group: &str, stream: &str) -> Vec<u8> {
    let mut out = b"{\"logGroupName\":".to_vec();
    json_string(&mut out, group.as_bytes());
    out.extend_from_slice(b",\"logStreamName\":");
    json_string(&mut out, stream.as_bytes());
    out.push(b'}');
    out
}

/// An entity as it is sent: its rendered name and environment, and its attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub service: String,
    pub environment: String,
    pub attributes: BTreeMap<String, String>,
}

/// PutLogEvents' body: its members as smithy-go's serializer writes them, in their order,
/// an entity's keys sorted.
pub fn put(
    group: &str,
    stream: &str,
    events: &[&Event],
    token: Option<&str>,
    entity: Option<&Rendered>,
) -> Vec<u8> {
    let mut out = b"{".to_vec();
    if let Some(e) = entity {
        out.extend_from_slice(b"\"entity\":{");
        if !e.attributes.is_empty() {
            out.extend_from_slice(b"\"attributes\":");
            map(
                &mut out,
                e.attributes.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            );
            out.push(b',');
        }
        out.extend_from_slice(b"\"keyAttributes\":");
        map(
            &mut out,
            [
                ("Environment", e.environment.as_str()),
                ("Name", e.service.as_str()),
                ("Type", "Service"),
            ]
            .into_iter(),
        );
        out.extend_from_slice(b"},");
    }
    out.extend_from_slice(b"\"logEvents\":[");
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"message\":");
        json_string(&mut out, &e.message);
        out.extend_from_slice(format!(",\"timestamp\":{}}}", e.at).as_bytes());
    }
    out.extend_from_slice(b"],\"logGroupName\":");
    json_string(&mut out, group.as_bytes());
    out.extend_from_slice(b",\"logStreamName\":");
    json_string(&mut out, stream.as_bytes());
    if let Some(t) = token {
        out.extend_from_slice(b",\"sequenceToken\":");
        json_string(&mut out, t.as_bytes());
    }
    out.push(b'}');
    out
}

fn map<'a>(out: &mut Vec<u8>, pairs: impl Iterator<Item = (&'a str, &'a str)>) {
    out.push(b'{');
    for (i, (k, v)) in pairs.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json_string(out, k.as_bytes());
        out.push(b':');
        json_string(out, v.as_bytes());
    }
    out.push(b'}');
}

/// An entity's name and environment made of the run's details: each its template's, not
/// empty, at most 512 characters (parseEntity, validateKeyAttributeValue).
pub fn render(entity: &Entity, run: &shards_ipc::Run, info: &super::Info<'_>) -> Result<Rendered, String> {
    let opts = super::options(run);
    let one = |k: &str, text: &str| -> Result<String, String> {
        let v = super::render_tag(text, info, &opts).map_err(|e| format!("--log-opt {k}={text}: {e}"))?;
        if v.is_empty() {
            return Err(format!("--log-opt {k}={text}: it makes nothing of this run"));
        }
        if v.chars().count() > ENTITY_VALUE {
            return Err(format!(
                "--log-opt {k}={text}: it makes {} characters of this run, past CloudWatch's {ENTITY_VALUE}",
                v.chars().count()
            ));
        }
        Ok(v)
    };
    Ok(Rendered {
        service: one("awslogs-entity-service-name", &entity.service)?,
        environment: one("awslogs-entity-environment", &entity.environment)?,
        attributes: entity.attributes.clone(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// What Docker 29's awslogs driver asked of CloudWatch Logs, and which of its options
    /// Docker took (scripts/logdrivers/generate).
    fn oracle() -> Value {
        serde_json::from_str(include_str!("testdata/awslogs.json")).unwrap()
    }

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Each request the recording holds is what shards makes of the same lines, byte for
    /// byte, but for the stream's name, which is the run's: its stream made, its group
    /// made where it was not there, and its events put, joined where a pattern or a
    /// datetime's format says.
    #[test]
    fn requests_match_the_recording() {
        for case in oracle()["cases"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let requests = case["requests"].as_array().unwrap();
            let body = |r: &Value| r["body"].as_str().unwrap().to_string();
            let target = |r: &Value| r["fields"]["x-amz-target"].as_str().unwrap().to_string();
            let created: Value = serde_json::from_str(&body(&requests[0])).unwrap();
            let (group, stream) = (
                created["logGroupName"].as_str().unwrap(),
                created["logStreamName"].as_str().unwrap(),
            );
            let mut ours = vec![(
                "Logs_20140328.CreateLogStream".to_string(),
                create_stream(group, stream),
            )];
            if name == "created" {
                ours.push(("Logs_20140328.CreateLogGroup".into(), create_group(group)));
                ours.push((
                    "Logs_20140328.CreateLogStream".into(),
                    create_stream(group, stream),
                ));
            }
            // The run's lines, as the recorded put has them: each its own event, at its time;
            // joined, the oracle's lines, at the time its events have.
            let put: Value = serde_json::from_str(&body(requests.last().unwrap())).unwrap();
            let recorded_events = put["logEvents"].as_array().unwrap();
            let at = recorded_events[0]["timestamp"].as_i64().unwrap();
            let (pattern, lines): (Option<String>, Vec<(i64, Vec<u8>)>) = match name {
                "multiline" => (
                    Some("^[0-9]".into()),
                    ["1 first", "  more", "2 second", "  more", "3 third"]
                        .map(|l| (at, l.as_bytes().to_vec()))
                        .into(),
                ),
                "datetime" => (
                    Some(strftime_pattern("%Y-%m-%d")),
                    ["2026-10-14 first", "  more", "2026-10-15 second"]
                        .map(|l| (at, l.as_bytes().to_vec()))
                        .into(),
                ),
                _ => (
                    None,
                    recorded_events
                        .iter()
                        .map(|e| {
                            (
                                e["timestamp"].as_i64().unwrap(),
                                e["message"].as_str().unwrap().as_bytes().to_vec(),
                            )
                        })
                        .collect(),
                ),
            };
            let mut events = Events::new(pattern.map(|p| regex::bytes::Regex::new(&p).unwrap()));
            let mut made = Vec::new();
            for (at, line) in &lines {
                events.take(line, *at, &mut made);
            }
            events.finish(&mut made);
            let mut batch = Batch::default();
            for e in made {
                assert!(batch.fits(&e));
                batch.add(e);
            }
            ours.push((
                "Logs_20140328.PutLogEvents".into(),
                put_body(&batch, group, stream),
            ));
            let recorded: Vec<(String, Vec<u8>)> = requests
                .iter()
                .map(|r| (target(r), body(r).into_bytes()))
                .collect();
            assert_eq!(
                ours.iter()
                    .map(|(t, b)| (t.clone(), String::from_utf8_lossy(b).into_owned()))
                    .collect::<Vec<_>>(),
                recorded
                    .iter()
                    .map(|(t, b)| (t.clone(), String::from_utf8_lossy(b).into_owned()))
                    .collect::<Vec<_>>(),
                "{name}"
            );
            // Every request JSON 1.1's, and EMF's said where asked.
            for r in requests {
                assert_eq!(r["fields"]["content-type"], "application/x-amz-json-1.1");
                assert_eq!(
                    r["fields"]["x-amzn-logs-format"].as_str() == Some(EMF),
                    name == "emf",
                    "{name}"
                );
            }
        }
    }

    fn put_body(batch: &Batch, group: &str, stream: &str) -> Vec<u8> {
        put(group, stream, &batch.sorted(), None, None)
    }

    /// The options the recording accepted, shards accepts, but where shards does better: a
    /// group not yet made is made by the run's shipper, which retries until it can send,
    /// where the recorded run failed to start; and the entity's options, newer than the
    /// recording, as their source takes them.
    #[test]
    fn options_are_accepted_and_refused_as_recorded() {
        for case in oracle()["options"].as_array().unwrap() {
            let given: Vec<String> = case["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_str().unwrap().to_string())
                .collect();
            let said = case["refused"].as_str().unwrap_or_default();
            let entity_attributes = given
                .iter()
                .find_map(|o| o.strip_prefix("awslogs-entity-attributes="));
            let refused = match () {
                // Refused as the recorded run started, of CloudWatch: shards' run starts,
                // its shipper sending once it can.
                () if said.contains("failed to create Cloudwatch log stream") => false,
                // The entity's, newer than the recording: a name without its environment;
                // attributes not KEY=VALUE, or a key twice.
                () if said.contains("unknown log opt 'awslogs-entity") => {
                    !given.iter().any(|o| o.starts_with("awslogs-entity-environment="))
                        || entity_attributes.is_some_and(|a| attributes(a).is_err())
                }
                () => !said.is_empty(),
            };
            let run = shards_ipc::Run {
                log_driver: "awslogs".into(),
                log_opts: [
                    "awslogs-region=us-east-1",
                    "awslogs-endpoint=http://127.0.0.1:4566",
                    "awslogs-credentials-endpoint=/creds",
                ]
                .iter()
                .map(|s| (*s).to_string())
                .chain(given.clone())
                .collect(),
                ..shards_ipc::Run::default()
            };
            let ours = crate::logging::kept(
                &run,
                crate::spec::LogRetention {
                    size: 1 << 20,
                    files: 5,
                },
            );
            assert_eq!(
                ours.is_err(),
                refused,
                "{given:?}: recorded {said}, shards {ours:?}"
            );
        }
    }

    /// A line is cut where its bytes, as CloudWatch counts them, would pass an event's;
    /// never within a character; a byte not UTF-8 counted as U+FFFD's three.
    #[test]
    fn lines_are_cut_where_an_event_would_pass_its_size() {
        let long = vec![b'x'; EVENT + 1];
        let cuts = cut(&long);
        assert_eq!(
            cuts.iter().map(|(b, n)| (b.len(), *n)).collect::<Vec<_>>(),
            [(EVENT, EVENT), (1, 1)]
        );
        // A three-byte character that would pass the bound goes whole to the next event.
        let mut line = vec![b'x'; EVENT - 2];
        line.extend_from_slice("€".as_bytes());
        let cuts = cut(&line);
        assert_eq!(
            cuts.iter().map(|(b, n)| (b.len(), *n)).collect::<Vec<_>>(),
            [(EVENT - 2, EVENT - 2), (3, 3)]
        );
        // Bytes not UTF-8: three each.
        let bad = vec![0xffu8; EVENT / 3 + 1];
        assert_eq!(size(&bad), (EVENT / 3 + 1) * 3);
        let cuts = cut(&bad);
        assert_eq!(
            cuts.iter().map(|(b, _)| b.len()).collect::<Vec<_>>(),
            [EVENT / 3, 1]
        );
        assert!(cut(b"").is_empty());
    }

    /// Strings are escaped as CloudWatch's JSON bodies hold them.
    #[test]
    fn strings_are_escaped_as_json_bodies_hold_them() {
        let mut out = Vec::new();
        json_string(&mut out, "a\"b\\c\n\r\t\u{1}\u{7f}<&>\u{2028}é".as_bytes());
        out.extend_from_slice(b" ");
        json_string(&mut out, b"\xff\xe2\x82x");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\"a\\\"b\\\\c\\n\\r\\t\\u0001\u{7f}<&>\\u2028é\" \"\\ufffd\\ufffd\\ufffdx\""
        );
    }

    /// Joined lines: each followed by a newline, a new event where one matches; one held
    /// is said held; an empty line is no event of its own, its line ending at once.
    #[test]
    fn lines_are_joined_until_one_starts_an_event() {
        let mut events = Events::new(Some(regex::bytes::Regex::new("^[0-9]").unwrap()));
        let mut out = Vec::new();
        for (at, l) in [(1, "1 a"), (2, " b"), (3, "2 c")] {
            events.take(l.as_bytes(), at, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].at, out[0].message.as_slice(), out[0].lines),
            (1, b"1 a\n b\n".as_slice(), 2)
        );
        assert_eq!(events.held(), Some((3, 1)));
        events.finish(&mut out);
        assert_eq!(
            (out[1].at, out[1].message.as_slice(), out[1].lines),
            (3, b"2 c\n".as_slice(), 1)
        );
        let mut plain = Events::new(None);
        let mut out = Vec::new();
        plain.take(b"", 7, &mut out);
        assert_eq!(
            out,
            [Event {
                at: 7,
                message: Vec::new(),
                size: 0,
                lines: 1
            }]
        );
    }

    /// A put holds 10,000 events at most, a mebibyte with 26 bytes more for each, and
    /// less than 24 hours.
    #[test]
    fn batches_hold_what_one_put_takes() {
        let e = |at: i64, size: usize| Event {
            at,
            message: vec![b'x'; size],
            size,
            lines: 1,
        };
        let mut b = Batch::default();
        for _ in 0..PUT_EVENTS {
            b.add(e(0, 1));
        }
        assert!(!b.fits(&e(0, 1)));
        // An empty line's event is sent with none: it always fits, and counts for nothing.
        assert!(b.fits(&e(0, 0)));
        b.add(e(0, 0));
        assert_eq!((b.sorted().len(), b.lines()), (PUT_EVENTS, PUT_EVENTS + 1));
        let mut b = Batch::default();
        b.add(e(0, PUT_BYTES - 2 * PER_EVENT - 10));
        assert!(b.fits(&e(0, 10)) && !b.fits(&e(0, 11)));
        let mut b = Batch::default();
        b.add(e(1000, 1));
        assert!(b.fits(&e(1000 + SPAN_MS - 1, 1)) && !b.fits(&e(1000 + SPAN_MS, 1)));
        assert!(b.fits(&e(1001 - SPAN_MS, 1)) && !b.fits(&e(1000 - SPAN_MS, 1)));
        b.add(e(5, 1));
        assert_eq!(b.sorted().iter().map(|e| e.at).collect::<Vec<_>>(), [5, 1000]);
    }

    /// Entity attributes are one CSV record of KEY=VALUEs, read as Go's CSV reader reads one.
    #[test]
    fn entity_attributes_are_one_csv_record_of_pairs() {
        assert_eq!(
            attributes(r#"a=1, b = 2 ,"c=x,y",,"#).unwrap(),
            [("a", "1"), ("b", "2"), ("c", "x,y")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into()
        );
        for bad in ["a", "a=", "=1", "a=1,a=2", "a=\"1\"", "a=1\nb=2", "\"a=1"] {
            assert!(attributes(bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..11).map(|i| format!("k{i}=v")).collect();
        assert!(attributes(&many.join(",")).is_err());
        assert_eq!(attributes("\n\na=1\n").unwrap().len(), 1);
    }

    /// A datetime's format is made a pattern by the strftime table: what the table lacks,
    /// nothing.
    #[test]
    fn datetime_formats_become_patterns_by_the_strftime_table() {
        assert_eq!(
            strftime_pattern("[%b %d, %Y %H:%M:%S%L]%Q%"),
            r"[(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) (?:0[1-9]|[1,2][0-9]|3[0,1]), \d{4} (?:[0,1][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]\.\d{3}]%"
        );
        let config = config(&opts(&[
            ("awslogs-group", "g"),
            ("awslogs-datetime-format", "%Y-%m-%d"),
        ]))
        .unwrap();
        let re = regex::bytes::Regex::new(config.multiline.as_deref().unwrap()).unwrap();
        assert!(re.is_match(b"2026-10-14 x") && !re.is_match(b"  more"));
    }

    /// The names CloudWatch takes, and nothing else, refused as the run is made.
    #[test]
    fn names_cloudwatch_refuses_are_refused_as_the_run_is_made() {
        for (k, v, ok) in [
            ("awslogs-group", "a/b-c_d.e#1", true),
            ("awslogs-group", "a b", false),
            ("awslogs-stream", "any thing", true),
            ("awslogs-stream", "a:b", false),
            ("awslogs-region", "us-east-1", true),
            ("awslogs-region", "us east", false),
            ("awslogs-endpoint", "http://127.0.0.1:4566", true),
            ("awslogs-endpoint", "127.0.0.1:4566", false),
            ("awslogs-multiline-pattern", "(", false),
        ] {
            let mut o = opts(&[("awslogs-group", "g")]);
            o.insert(k.into(), v.into());
            assert_eq!(config(&o).is_ok(), ok, "{k}={v}");
        }
    }
}
