//! The server's view of every agent session, wherever it runs: its own (`local`) and each
//! runner's. A copy per session (the latest state, meta and timeline, as JSON) serves any number
//! of viewers on any device, its live frames on kiln's bus (`super::live`, docs/bus.md); their
//! commands go to the machine that runs the session. Each copy belongs to one shard, which alone
//! changes it, and is read from the views that shard publishes (`super::copies`): no lock guards
//! any session's copy (goal 4, plan 01a1a59c2a9c; plan 01a17197 step 9).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize as _;
use serde_json::{Value, json};

use crate::rcu::Rcu;

use super::copies::{Copies, Ingest, Mirror, Msg, Shard};
use super::core::{MAX_EVENTS, Update};
use super::files;
use super::inbox::{self, Answer, Said};
use super::kept::{Job, Keeper, Kept};
use super::host::{Host, Inventory, Limits};
use super::kinds::KindInfo;
use super::model::{Command, State, Status};
use super::session::{Held, Info, MAX_NAME, Spec, SubagentBrief};
use super::state_doc::StateDoc;
use super::live::Viewer;
use crate::hub::Hub;
use crate::proto::{LANE_TERM, ServerMsg};
/// How long a session this server opened on a machine waits for the machine to announce it before
/// a report from the machine that leaves it out ends it.
const OPEN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// A message's id (what its session's inbox takes once): none for a command that isn't a message,
/// or a message without one.
pub fn message_id(c: &Command) -> Option<String> {
    match c {
        Command::Send { id, .. } if !id.is_empty() => Some(id.clone()),
        Command::SkillRun { id: Some(id), .. } if !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Local,
    Machine(String),
}

impl Location {
    pub fn target(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Machine(m) => format!("agent:{m}"),
        }
    }

    pub fn of_target(target: &str) -> Self {
        match target.strip_prefix("agent:") {
            Some(m) => Self::Machine(m.to_owned()),
            None => Self::Local,
        }
    }
}

/// One of a copy's events, or one of its changeset's files: its JSON as frames carry it, written out once
/// as it comes in (every viewer's snapshot, every live frame and every digest take that as it is), and
/// what's read of it without the JSON: an event's id, number and kind and whether it's on its session's
/// main thread, a file's path and tree. Its JSON's tree isn't kept: a long session's copy kept every
/// event's beside its text, 8,260 bytes for each event it kept where its JSON is about 3,500 (goal 11, plan
/// 01a1a87b2e56 step 2; `tests/session_resident.rs`). One that text is streamed into takes each piece into
/// its JSON where it is ([`Streaming`]).
pub struct Entry {
    body: Body,
    head: Head,
    /// Its digest as a runner that starts again compares it (`crate::wire::sums`), once asked for.
    sum: std::sync::OnceLock<u64>,
    /// Its lean form's JSON, for viewers that take events lean (`super::window::lean`), once made:
    /// None when it goes as it is.
    lean: std::sync::OnceLock<Option<Box<str>>>,
}

/// An entry's JSON: as written out, or as text streamed into it grows it.
#[derive(Clone)]
enum Body {
    Text(Box<str>),
    Streaming(Box<Streaming>),
}

/// An entry text is being streamed into (goal 9, plan 01a1a87b2cec step 3): its JSON, its top-level `text` string's closing
/// quote at byte `end`. Each piece goes in there, escaped as serde_json escapes a string's characters, each on its own (so a
/// piece's escaping is its share of the whole text's), and the JSON stays what writing out its tree with the text grown
/// writes, byte for byte: what its digest and every frame take. Its tree was held instead, copied whole and written out whole
/// again at each piece: O(the message so far) on its owner's shard at every delta, past the step's quantum for a message past
/// ~200 KB (a long extended thinking).
#[derive(Clone)]
struct Streaming {
    json: String,
    end: usize,
}

/// `more` as serde_json escapes it inside a string (without its quotes): what it adds to a [`Streaming`] entry's JSON.
fn escaped(more: &str) -> Option<String> {
    let quoted = serde_json::to_string(more).ok()?;
    quoted.get(1..quoted.len().checked_sub(1)?).map(str::to_owned)
}

/// Past the JSON string that begins at byte `at` of `b` (its opening quote): its closing quote's byte.
fn string_end(b: &[u8], at: usize) -> Option<usize> {
    let mut i = at.checked_add(1)?;
    loop {
        match *b.get(i)? {
            b'\\' => i = i.checked_add(2)?,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
}

/// Past the JSON value that begins at byte `at` of `b`: the byte after it. Strings and nested objects and arrays read
/// through, their strings' brackets and quotes passed over.
fn value_end(b: &[u8], at: usize) -> Option<usize> {
    match *b.get(at)? {
        b'"' => string_end(b, at).map(|e| e + 1),
        b'{' | b'[' => {
            let (mut depth, mut i) = (0usize, at);
            loop {
                match *b.get(i)? {
                    b'"' => i = string_end(b, i)?,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let mut i = at;
            while b.get(i).is_some_and(|c| !matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')) {
                i += 1;
            }
            (i > at).then_some(i)
        }
    }
}

/// Where the top-level `text` string of `json` ends (its closing quote's byte), where `json` is an object as serde_json writes
/// one: no space between its tokens, its keys as they are (none escaped), its text given once, nothing after it. None else (a
/// JSON another wrote, or one with no text string), and the entry's JSON is written out from its tree first, as before
/// (`Entry::streaming_at`): so a piece put in where this says is what writing out its tree with the text grown writes.
fn text_end(json: &str) -> Option<usize> {
    let b = json.as_bytes();
    if b.first() != Some(&b'{') {
        return None;
    }
    let (mut i, mut found) = (1, None);
    loop {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = string_end(b, i)?;
        let key = b.get(i + 1..key_end)?;
        if key.contains(&b'\\') || b.get(key_end + 1) != Some(&b':') {
            return None;
        }
        let at = key_end + 2;
        let end = value_end(b, at)?;
        if key == b"text" {
            if b.get(at) != Some(&b'"') || found.is_some() {
                return None;
            }
            found = Some(end - 1);
        }
        match b.get(end)? {
            b',' => i = end + 1,
            b'}' if end + 1 == b.len() => return found,
            _ => return None,
        }
    }
}

/// What's read of an entry without its JSON.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(PartialEq, Debug))]
struct Head {
    id: Option<Box<str>>,
    seq: Option<u64>,
    kind: Option<std::borrow::Cow<'static, str>>,
    /// An event of its session's main thread: no `parent`, or a null one.
    main: bool,
    /// A changeset file's path and tree ([`file_ref`]).
    file: Option<Box<(Option<Box<str>>, Option<Box<str>>)>>,
}

/// The kinds events have (`super::model::Body`'s tags), kept without a copy each.
const KINDS: [&str; 5] = ["user", "assistant", "thinking", "tool", "notice"];

impl Head {
    fn of(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str);
        let kind = s("kind").map(|k| KINDS.iter().find(|known| **known == k).map_or_else(|| std::borrow::Cow::Owned(k.to_owned()), |known| std::borrow::Cow::Borrowed(*known)));
        let file = (v.get("path").is_some() || v.get("tree").is_some()).then(|| Box::new((s("path").map(Box::from), s("tree").map(Box::from))));
        Self { id: s("id").map(Box::from), seq: v.get("seq").and_then(Value::as_u64), kind, main: v.get("parent").is_none_or(Value::is_null), file }
    }

    /// [`Head::of`] the JSON `text` holds, read from the text without making its tree (goal 9, plan 01a1a87b2cec
    /// step 3). Each event of a compact chunk read again becomes an entry this way ([`Entry::from_text`]), often on a
    /// runtime shard, where a poll may hold the shard for 266.7 µs at most; the tree allocated every string, map and
    /// array of an event's JSON, a tool event's tens of KB of it, for a head of five fields. Only the top-level
    /// object's `id`, `seq`, `kind`, `parent`, `path` and `tree` are taken; every other value is read through and
    /// nothing of it kept. Each value is read as `Value` reads one (serde_json's `deserialize_any`), so what the tree
    /// refuses this refuses, for [`Head::default`]: more than whitespace after the value, a number past f64's range,
    /// a lone surrogate, nesting past serde_json's 128 levels. `serde::de::IgnoredAny` would take the last three:
    /// serde_json's `ignore_value`, which it reads through with, checks none of them.
    fn of_text(text: &str) -> Self {
        use std::borrow::Cow;
        use std::fmt;

        use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

        /// How a value is read: the top-level one (an object's head fields taken), a head field's (a string kept),
        /// or any other (read through, nothing kept).
        #[derive(Clone, Copy)]
        enum Look {
            Top,
            Field,
            Through,
        }

        /// What's taken of a value: the top-level object's head, a head field's string, a number as `Value::as_u64`
        /// takes one (an integer from 0 to u64::MAX), a null, or nothing.
        enum Seen<'de> {
            Object(Head),
            Str(Cow<'de, str>),
            U64(u64),
            Null,
            Other,
        }

        impl<'de> Seen<'de> {
            fn text(self) -> Option<Cow<'de, str>> {
                if let Seen::Str(s) = self { Some(s) } else { None }
            }
            fn number(self) -> Option<u64> {
                if let Seen::U64(n) = self { Some(n) } else { None }
            }
        }

        /// A top-level key: one of the head's, or another.
        #[derive(Clone, Copy)]
        enum Key {
            Id,
            Seq,
            Kind,
            Parent,
            Path,
            Tree,
            Other,
        }

        /// Reads a top-level key, its escapes undone first, as the tree's map has its keys.
        struct Keys;

        impl<'de> DeserializeSeed<'de> for Keys {
            type Value = Key;
            fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Key, D::Error> {
                d.deserialize_str(self)
            }
        }

        impl<'de> Visitor<'de> for Keys {
            type Value = Key;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a key")
            }
            fn visit_str<E: de::Error>(self, k: &str) -> Result<Key, E> {
                Ok(match k {
                    "id" => Key::Id,
                    "seq" => Key::Seq,
                    "kind" => Key::Kind,
                    "parent" => Key::Parent,
                    "path" => Key::Path,
                    "tree" => Key::Tree,
                    _ => Key::Other,
                })
            }
        }

        /// A kind as [`Head::of`] keeps it: one of `KINDS` without a copy.
        fn kind(k: Cow<'_, str>) -> Cow<'static, str> {
            KINDS.into_iter().find(|known| *known == k).map_or_else(|| Cow::Owned(k.into_owned()), Cow::Borrowed)
        }

        impl<'de> DeserializeSeed<'de> for Look {
            type Value = Seen<'de>;
            fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Seen<'de>, D::Error> {
                d.deserialize_any(self)
            }
        }

        impl<'de> Visitor<'de> for Look {
            type Value = Seen<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Seen<'de>, E> {
                Ok(Seen::Other)
            }
            // serde_json gives a negative integer only here.
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Seen<'de>, E> {
                Ok(u64::try_from(n).map_or(Seen::Other, Seen::U64))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Seen<'de>, E> {
                Ok(Seen::U64(n))
            }
            // A number with a fraction or an exponent, -0, and an integer past u64::MAX or below i64::MIN: without its
            // `arbitrary_precision`, which kiln's build hasn't, serde_json reads those as f64.
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Seen<'de>, E> {
                Ok(Seen::Other)
            }
            fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Seen<'de>, E> {
                Ok(if matches!(self, Look::Field) { Seen::Str(Cow::Borrowed(s)) } else { Seen::Other })
            }
            // A string with escapes, undone into serde_json's scratch buffer.
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Seen<'de>, E> {
                Ok(if matches!(self, Look::Field) { Seen::Str(Cow::Owned(s.to_owned())) } else { Seen::Other })
            }
            fn visit_unit<E: de::Error>(self) -> Result<Seen<'de>, E> {
                Ok(Seen::Null)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Seen<'de>, A::Error> {
                while seq.next_element_seed(Look::Through)?.is_some() {}
                Ok(Seen::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Seen<'de>, A::Error> {
                if !matches!(self, Look::Top) {
                    while map.next_key_seed(Look::Through)?.is_some() {
                        map.next_value_seed(Look::Through)?;
                    }
                    return Ok(Seen::Other);
                }
                // A key given more than once counts with its last value, as the tree's map keeps it.
                let mut head = Head { main: true, ..Head::default() };
                let (mut path, mut tree) = (None, None);
                while let Some(key) = map.next_key_seed(Keys)? {
                    let seen = map.next_value_seed(if matches!(key, Key::Id | Key::Kind | Key::Path | Key::Tree) { Look::Field } else { Look::Through })?;
                    match key {
                        Key::Id => head.id = seen.text().map(Box::from),
                        Key::Seq => head.seq = seen.number(),
                        Key::Kind => head.kind = seen.text().map(kind),
                        Key::Parent => head.main = matches!(seen, Seen::Null),
                        Key::Path => path = Some(seen.text()),
                        Key::Tree => tree = Some(seen.text()),
                        Key::Other => {}
                    }
                }
                head.file = (path.is_some() || tree.is_some()).then(|| Box::new((path.flatten().map(Box::from), tree.flatten().map(Box::from))));
                Ok(Seen::Object(head))
            }
        }

        let mut json = serde_json::Deserializer::from_str(text);
        let seen = Look::Top.deserialize(&mut json);
        // Nothing but whitespace after the value, as `serde_json::from_str` has it.
        match seen.and_then(|seen| json.end().map(|()| seen)) {
            Ok(Seen::Object(head)) => head,
            // A value that isn't an object: nothing found in it, `parent` among it.
            Ok(_) => Head { main: true, ..Head::default() },
            Err(_) => Head::default(),
        }
    }

    /// [`Head::of`] an event's JSON, read from the event itself: its kind is its body's tag (`super::model::Body`
    /// is tagged `kind`, in snake case), and an event's JSON has no changeset file's path or tree of its own.
    fn of_event(e: &super::model::Event) -> Self {
        use super::model::Body as B;
        let kind = match &e.body {
            B::User { .. } => "user",
            B::Assistant { .. } => "assistant",
            B::Thinking { .. } => "thinking",
            B::Tool(_) => "tool",
            B::Notice { .. } => "notice",
        };
        Self { id: Some(e.id.as_str().into()), seq: Some(e.seq), kind: Some(std::borrow::Cow::Borrowed(kind)), main: e.parent.is_none(), file: None }
    }
}

/// Event `e`'s JSON, as `serde_json::to_string` writes it, written into room made for it once (goal 9, plan
/// 01a1a87b2cec step 5). `to_string` starts with 128 bytes and doubles them as it writes, so an event of the lab's
/// 3.5 KB (the stand-in's mean reply) was copied into five larger buffers on its way out, each past what glibc keeps in
/// a thread's own cache (1,032 bytes), so each a trip to its arena. The room is what the event weighs
/// (`super::weigh::event`: its place, each text, each tool input's node and keys, more than its JSON's keys, quotes
/// and commas take), and an eighth more for its texts' escapes: a newline (two bytes in JSON) every eighth byte
/// at the most before it would grow again, where the stand-in's replies have one every 17 words (its `words`).
fn event_json(e: &super::model::Event) -> Option<String> {
    let weight = super::weigh::event(e);
    let mut out = Vec::with_capacity(weight.saturating_add(weight / 8));
    serde_json::to_writer(&mut out, e).ok()?;
    String::from_utf8(out).ok()
}

impl Entry {
    /// `value`, its JSON written out now.
    pub(super) fn new(value: Value) -> Self {
        let text = serde_json::to_string(&value).unwrap_or_default();
        Self::written(value, text)
    }

    /// An entry as `text`, its JSON, says (an event of a compact chunk read again): what's looked up of it
    /// read from the text, no tree made of it ([`Head::of_text`]).
    pub(super) fn from_text(text: &str) -> Self {
        let head = Head::of_text(text);
        crate::memory::SESSION_ENTRIES.add(text.len());
        Self { head, body: Body::Text(text.into()), sum: std::sync::OnceLock::new(), lean: std::sync::OnceLock::new() }
    }

    /// `value` as `text` writes it (made already, for a live frame): the text kept, the tree let go of.
    pub(super) fn written(value: Value, text: String) -> Self {
        crate::memory::SESSION_ENTRIES.add(text.len());
        Self { head: Head::of(&value), body: Body::Text(text.into_boxed_str()), sum: std::sync::OnceLock::new(), lean: std::sync::OnceLock::new() }
    }

    /// Event `e` as a copy keeps it, written out once, straight from the event (goal 9, plan 01a1a87b2cec
    /// step 5: its owner wrote each event into a JSON tree, then the tree out, then the event again for the
    /// store's row). It's the text the tree wrote, as a runner's digests of it take it (`crate::agent`):
    /// serde_json's maps keep their keys in the order they come (its `preserve_order`, which kiln's build
    /// has). What's looked up of it is read from the event ([`Head::of_event`]); its text goes to `keep`
    /// as well (the conversation store's row), when anyone keeps it. One that can't be written out is
    /// `null`, as before.
    pub(super) fn of_event(e: &super::model::Event, keep: Option<&mut dyn FnMut(String)>) -> Self {
        let Some(text) = event_json(e) else { return Self::written(Value::Null, "null".to_owned()) };
        crate::memory::SESSION_ENTRIES.add(text.len());
        let body: Box<str> = match keep {
            Some(keep) => {
                let body = Box::from(text.as_str());
                keep(text);
                body
            }
            None => text.into_boxed_str(),
        };
        Self { head: Head::of_event(e), body: Body::Text(body), sum: std::sync::OnceLock::new(), lean: std::sync::OnceLock::new() }
    }

    /// The bytes it holds as text, as `crate::memory::SESSION_ENTRIES` counts them: its JSON, and its lean
    /// form once made (W21-R15).
    fn counted(&self) -> usize {
        let lean = self.lean.get().and_then(Option::as_ref).map_or(0, |t| t.len());
        match &self.body {
            Body::Text(t) => t.len() + lean,
            Body::Streaming(s) => s.json.len() + lean,
        }
    }

    /// Its JSON as frames carry it: `serde_json`'s compact form of its value.
    pub fn text(&self) -> &str {
        match &self.body {
            Body::Text(t) => t,
            Body::Streaming(s) => &s.json,
        }
    }

    /// Its digest, of its text (`crate::wire::sums::event_text`): made once, however many ask.
    pub fn sum(&self) -> u64 {
        *self.sum.get_or_init(|| crate::wire::sums::event_text(self.text()))
    }

    /// Its JSON as a viewer that takes events lean (`lean`) is sent it: its lean form, written out
    /// once when first asked for (and counted as the entry's), or its whole text when it goes as it is
    /// (docs/wire.md, "Windows").
    pub fn sent(&self, lean: bool) -> &str {
        if !lean {
            return self.text();
        }
        let made = self.lean.get_or_init(|| {
            let made = lean_text(self.text(), self.kind());
            if let Some(t) = &made {
                crate::memory::SESSION_ENTRIES.add(t.len());
            }
            made
        });
        match made {
            Some(t) => t,
            None => self.text(),
        }
    }

    /// Its id: what the copy finds an event by.
    pub fn id(&self) -> Option<&str> {
        self.head.id.as_deref()
    }

    /// Its number in its session's timeline.
    pub fn seq(&self) -> Option<u64> {
        self.head.seq
    }

    /// An event's kind (`user`, `assistant`, `thinking`, `tool`, `notice`).
    pub fn kind(&self) -> Option<&str> {
        self.head.kind.as_deref()
    }

    /// Whether it's an event of its session's main thread (one of no subagent's).
    pub fn on_main_thread(&self) -> bool {
        self.head.main
    }

    /// Whether its JSON is `null`: an event that couldn't be written out as JSON, nothing for a copy to
    /// take.
    pub fn is_null(&self) -> bool {
        self.text() == "null"
    }

    /// A changeset file's path and tree, as [`file_ref`] names a file's JSON.
    pub(super) fn file(&self) -> FileRef<'_> {
        self.head.file.as_deref().map_or((None, None), |(path, tree)| (path.as_deref(), tree.as_deref()))
    }

    /// Its JSON's tree, read again from its text: for what reads more of it than the above (an agent's
    /// conclusion, a changeset as JSON).
    pub fn value(&self) -> Value {
        serde_json::from_str(self.text()).unwrap_or(Value::Null)
    }

    /// `more` added to its `text`, as a runner streams it, in its JSON where it is ([`Streaming`]; goal 9, plan 01a1a87b2cec
    /// step 3): its digest made again when next asked for. False when it has no text to add to.
    pub(super) fn append_text(&mut self, more: &str) -> bool {
        let Some(add) = escaped(more) else { return false };
        let was = self.counted();
        match &mut self.body {
            Body::Streaming(s) if s.json.is_char_boundary(s.end) => {
                s.json.insert_str(s.end, &add);
                s.end += add.len();
            }
            Body::Streaming(_) => return false,
            Body::Text(t) => {
                let Some(made) = Self::streaming_at(t).and_then(|(json, end)| grown(&json, end, &add)) else { return false };
                self.body = Body::Streaming(Box::new(made));
            }
        }
        // Its lean form, if made, says what it was.
        drop(self.lean.take());
        self.sum = std::sync::OnceLock::new();
        let now = self.counted();
        crate::memory::SESSION_ENTRIES.add(now);
        crate::memory::SESSION_ENTRIES.sub(was);
        true
    }

    /// It with `more` added to its `text` ([`Entry::append_text`]), made anew where a view shares it: its JSON copied once,
    /// the piece in it (goal 9, plan 01a1a87b2cec step 3: the tree was copied whole and written out whole again). None when
    /// it has no text to add to.
    pub(super) fn appended(&self, more: &str) -> Option<Entry> {
        let add = escaped(more)?;
        let made = match &self.body {
            Body::Text(t) => Self::streaming_at(t).and_then(|(json, end)| grown(&json, end, &add))?,
            Body::Streaming(s) => grown(&s.json, s.end, &add)?,
        };
        let entry = Entry { head: self.head.clone(), body: Body::Streaming(Box::new(made)), sum: std::sync::OnceLock::new(), lean: std::sync::OnceLock::new() };
        crate::memory::SESSION_ENTRIES.add(entry.counted());
        Some(entry)
    }

    /// JSON `json` as a [`Streaming`] entry takes a piece: as it is, where its text's end is found in it; else as its tree
    /// writes it (what holding the tree wrote: its keys and text as serde_json writes them), found there. None without a
    /// top-level text string.
    fn streaming_at(json: &str) -> Option<(std::borrow::Cow<'_, str>, usize)> {
        if let Some(end) = text_end(json) {
            return Some((std::borrow::Cow::Borrowed(json), end));
        }
        let value = serde_json::from_str::<Value>(json).ok()?;
        if !value.get("text").is_some_and(Value::is_string) {
            return None;
        }
        let written = serde_json::to_string(&value).ok()?;
        let end = text_end(&written)?;
        Some((std::borrow::Cow::Owned(written), end))
    }
}

#[cfg(test)]
thread_local! {
    /// Events whose JSON was read whole on this thread for its lean form ([`lean_text`]): goal 11, plan 01a1a87b2e56 step 5's
    /// tests of what an attach reads (the memory audit's row A2a: each of a lean window's events read once an attach).
    pub(crate) static LEAN_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The lean form of an event whose JSON is `text` and whose kind is `kind` (`super::window::lean`), written out; none where
/// it goes as it is. Only a tool event has one, so another's JSON isn't read at all: a lean viewer's window read every one of
/// its events' JSON whole for this, a person's prompts and the agent's replies among them, to find nothing to hold back (goal
/// 11, plan 01a1a87b2e56 step 5; the memory audit's row A2a). The kind is the head's, read from the same JSON's top-level
/// `kind` as `lean` reads it (`Head`).
fn lean_text(text: &str, kind: Option<&str>) -> Option<Box<str>> {
    if kind != Some("tool") {
        return None;
    }
    #[cfg(test)]
    LEAN_READS.with(|n| n.set(n.get() + 1));
    let value = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    super::window::lean(&value).and_then(|v| serde_json::to_string(&v).ok()).map(String::into_boxed_str)
}

/// Event JSON `text`, one of a compact chunk's read again (`super::copies::Sending`), as a viewer that takes events `lean` is
/// sent it, and whether it begins a person's turn (a message to its session's main thread, `super::window::begins_turn`):
/// what's looked up of it read from the text ([`Head::of_text`]), its lean form made where it has one ([`lean_text`]), and
/// no entry made of it, its text copied once, into what's sent (goal 11, plan 01a1a87b2e56 step 5; the memory audit's row
/// A2a: each was made an entry, its text copied, for each of the four times an attach read it).
pub(super) fn sent_of_text(text: &str, lean: bool) -> (Box<str>, bool) {
    let head = Head::of_text(text);
    let kind = head.kind.as_deref();
    let begins = kind == Some("user") && head.main;
    let made = if lean { lean_text(text, kind) } else { None };
    (made.unwrap_or_else(|| Box::from(text)), begins)
}

/// `json` with `add` (escaped already) put in at byte `end` (its text's closing quote), copied once into room for both.
fn grown(json: &str, end: usize, add: &str) -> Option<Streaming> {
    let mut made = String::with_capacity(json.len().checked_add(add.len())?);
    made.push_str(json.get(..end)?);
    made.push_str(add);
    made.push_str(json.get(end..)?);
    Some(Streaming { json: made, end: end.checked_add(add.len())? })
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        crate::memory::SESSION_ENTRIES.add(self.counted());
        Self { body: self.body.clone(), head: self.head.clone(), sum: self.sum.clone(), lean: self.lean.clone() }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        crate::memory::SESSION_ENTRIES.sub(self.counted());
    }
}

/// A copy's changeset: its files in order, each shared, so its snapshots and patches take every
/// file that didn't change as it is (no copy, no compare, no writing it out again).
pub(super) type Files = Arc<Vec<Arc<Entry>>>;

/// The session socket's frames, written straight out (into a frame's buffer, a bus payload, a
/// snapshot's gzip): `{"t":<tag>,<fields>}`, then `"rev"` last when there is one, byte for byte as
/// [`SessionDown`](crate::wire::ui::SessionDown) says them (`frames_are_written_as_the_socket_says_them`),
/// without first copying what they carry into one. Writes to a `Vec` can't fail; to a gzip
/// encoder, they fail as it does.
pub(super) mod write {
    use std::io::{self, Write};

    use serde_json::Value;

    use super::Entry;

    /// `{"t":"<tag>"` and `,"<key>":`. Tags and keys are the socket's own words: nothing to escape.
    fn head(w: &mut impl Write, tag: &str, key: Option<&str>) -> io::Result<()> {
        w.write_all(br#"{"t":""#)?;
        w.write_all(tag.as_bytes())?;
        w.write_all(b"\"")?;
        if let Some(k) = key {
            w.write_all(b",\"")?;
            w.write_all(k.as_bytes())?;
            w.write_all(b"\":")?;
        }
        Ok(())
    }

    /// `,"rev":N}`, or `}`.
    fn tail(w: &mut impl Write, rev: Option<u64>) -> io::Result<()> {
        match rev {
            Some(r) => write!(w, ",\"rev\":{r}}}"),
            None => w.write_all(b"}"),
        }
    }

    /// The bytes [`head`] and [`tail`] write: a frame but for what it carries.
    pub fn envelope(tag: &str, key: Option<&str>, rev: Option<u64>) -> usize {
        let head = r#"{"t":""#.len() + tag.len() + r#"""#.len() + key.map_or(0, |k| r#",""#.len() + k.len() + r#"":"#.len());
        let tail = rev.map_or("}".len(), |r| r#","rev":"#.len() + r.checked_ilog10().map_or(1, |d| d as usize + 1) + "}".len());
        head + tail
    }

    /// The bytes [`texts`] writes of `all`: `[`, their texts with a `,` between each, `]`.
    pub fn texts_len<'a>(all: impl IntoIterator<Item = &'a Entry>) -> usize {
        strs_len(all.into_iter().map(Entry::text))
    }

    /// The bytes [`strs`] writes of `all`.
    pub fn strs_len<'a>(all: impl IntoIterator<Item = &'a str>) -> usize {
        let (n, bytes) = all.into_iter().fold((0usize, 0usize), |(n, bytes), t| (n + 1, bytes + t.len()));
        "[".len() + bytes + n.saturating_sub(1) * ",".len() + "]".len()
    }

    /// `{"t":"meta","meta":<json>}`, a catalog's JSON as it was written once (`super::super::copies::Catalog`).
    pub fn meta(w: &mut impl Write, json: &[u8], rev: Option<u64>) -> io::Result<()> {
        head(w, "meta", Some("meta"))?;
        w.write_all(json)?;
        tail(w, rev)
    }

    /// `{"t":"state","state":<state>}`, the state written out as its fields are kept (goal 9, plan 01a1a87b2cec
    /// step 5): the bytes [`value`] wrote of its tree.
    pub fn state(w: &mut impl Write, s: &super::StateDoc, rev: Option<u64>) -> io::Result<()> {
        head(w, "state", Some("state"))?;
        s.write(w)?;
        tail(w, rev)
    }

    /// `[<a>,<b>,…]` of entries' texts.
    fn texts<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a Entry>) -> io::Result<()> {
        strs(w, all.into_iter().map(Entry::text))
    }

    /// `[<a>,<b>,…]` of JSON texts.
    fn strs<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a str>) -> io::Result<()> {
        w.write_all(b"[")?;
        for (i, t) in all.into_iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            w.write_all(t.as_bytes())?;
        }
        w.write_all(b"]")
    }

    /// `{"t":"events","events":[…]}`.
    pub fn events<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a Entry>, rev: Option<u64>) -> io::Result<()> {
        sent_events(w, all.into_iter().map(Entry::text), rev)
    }

    /// `{"t":"events","events":[…]}` of events as a viewer is sent them ([`Entry::sent`]).
    pub fn sent_events<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a str>, rev: Option<u64>) -> io::Result<()> {
        head(w, "events", Some("events"))?;
        strs(w, all)?;
        tail(w, rev)
    }

    /// `{"t":"events","events":[…]}` of texts already joined by `,` (a compact chunk's: goal 11).
    pub fn joined_events(w: &mut impl Write, joined: &[u8], rev: Option<u64>) -> io::Result<()> {
        head(w, "events", Some("events"))?;
        w.write_all(b"[")?;
        w.write_all(joined)?;
        w.write_all(b"]")?;
        tail(w, rev)
    }

    /// `{"t":"changes","files":[…]}`.
    pub fn changes<'a>(w: &mut impl Write, files: impl IntoIterator<Item = &'a Entry>, rev: Option<u64>) -> io::Result<()> {
        head(w, "changes", Some("files"))?;
        texts(w, files)?;
        tail(w, rev)
    }

    /// `{"t":"changes_patch","set":[…],"gone":[…]}`, each left out when empty.
    pub fn changes_patch(w: &mut impl Write, set: &[&Entry], gone: &[Value], rev: Option<u64>) -> io::Result<()> {
        head(w, "changes_patch", None)?;
        if !set.is_empty() {
            w.write_all(br#","set":"#)?;
            texts(w, set.iter().copied())?;
        }
        if !gone.is_empty() {
            w.write_all(br#","gone":"#)?;
            serde_json::to_writer(&mut *w, gone)?;
        }
        tail(w, rev)
    }

    /// `{"t":"state_patch","set":{…},"gone":[…]}`: `set` always (each field's key and its value's JSON as the state
    /// keeps it: `super::StateDoc::patch`), `gone` left out when empty.
    pub fn state_patch(w: &mut impl Write, set: &[(&str, &[u8])], gone: &[&str], rev: Option<u64>) -> io::Result<()> {
        head(w, "state_patch", Some("set"))?;
        w.write_all(b"{")?;
        for (i, (k, v)) in set.iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            serde_json::to_writer(&mut *w, k)?;
            w.write_all(b":")?;
            w.write_all(v)?;
        }
        w.write_all(b"}")?;
        if !gone.is_empty() {
            w.write_all(br#","gone":"#)?;
            serde_json::to_writer(&mut *w, gone)?;
        }
        tail(w, rev)
    }

    /// `{"t":"stream","id":…,"text":…}`.
    pub fn stream(w: &mut impl Write, id: &str, text: &str, rev: Option<u64>) -> io::Result<()> {
        head(w, "stream", Some("id"))?;
        serde_json::to_writer(&mut *w, id)?;
        w.write_all(br#","text":"#)?;
        serde_json::to_writer(&mut *w, text)?;
        tail(w, rev)
    }

    /// What `f` writes, as text: what the writers above write is JSON, so UTF-8.
    pub fn string(room: usize, f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::with_capacity(room);
        if f(&mut out).is_err() {
            return String::new();
        }
        String::from_utf8(out).unwrap_or_default()
    }
}

/// This server process's incarnation: viewers resume from a copy's revision only under it. 53
/// bits: it travels as a JSON number, which a browser reads exactly only up to 2^53.
pub fn born() -> u64 {
    static BORN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BORN.get_or_init(|| rand::random::<u64>() >> 11)
}

/// `value` kept in `dir`'s `file`, whole (written beside it, then moved over it).
fn keep_json_in(dir: &std::path::Path, file: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let path = dir.join(file);
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A snapshot of a copy, or what a viewer lacks of it since a revision, not yet made into frames
/// (`super::copies::View::parts`, `View::parts_since`): references to what an immutable view holds.
pub struct Parts {
    /// Its info, made into the head's JSON where the snapshot is written (off the shard that takes
    /// it: goal 10, plan 01a1a87b2dae step 3).
    pub(super) info: Arc<Info>,
    pub(super) meta: Arc<super::copies::Catalog>,
    pub(super) state: Arc<StateDoc>,
    pub(super) changes: Option<Files>,
    pub(super) events: super::copies::Events,
    /// Since a revision: only the events changed after it (it begins with `resumed`). None: the
    /// session whole (it begins with `open`).
    pub(super) since: Option<u64>,
    /// Its state's last frame's size: the room its takes.
    pub(super) state_bytes: usize,
    /// The copy's revision as of the parts.
    pub(super) rev: u64,
    /// Its events go lean ([`Entry::sent`]).
    pub(super) lean: bool,
    /// Its events are a window of the session's (its `open` says where it begins), not all of them.
    pub(super) window: Option<crate::wire::ui::EventsWindow>,
    /// Where among its events a window begins (0: the first).
    pub(super) from: usize,
    /// A window still to be found, where the snapshot is written ([`Parts::resolved`]): its viewer takes a window,
    /// and none was found where the parts were taken (on the socket's shard).
    pub(super) find: Option<Find>,
    /// Opt-in cursor policy on this exact immutable copy, not another event store.
    pub(super) history: Option<History>,
}

pub(super) struct History {
    limits: super::history::Limits,
    token: String,
    durable: bool,
}

/// A window to find where a snapshot is written, off the shards (goal 9, plan 01a1a87b2cec step 3: no poll past the
/// step's quantum; goal 11, plan 01a1a87b2e56 step 5): finding it reads the session's newest events as its viewer
/// takes them, a lean viewer's each made lean (its JSON read whole), and each of a compact chunk's read again, which
/// the socket's shard did as the parts were taken (`super::copies::View::parts_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Find {
    /// The turns it holds at the most (`super::window::TURNS`).
    pub turns: usize,
    /// Events before the copy's first may be kept elsewhere (the server's conversation store).
    pub older: bool,
}

impl Parts {
    /// What it brings, its window found where one is still to be ([`Parts::find`]): since its revision while what
    /// changed since fits a window's bounds (a resume never costs more than a first load), else the window of the
    /// newest events; and where among its events that begins. Its events read as its viewer takes them by `sending`,
    /// which keeps each read for what takes it next, its frames' plan and their writing (`super::copies::Sending`: goal
    /// 11, plan 01a1a87b2e56 step 5, the memory audit's row A2a). What a snapshot's plan does first, where the snapshot is
    /// written: off the shards.
    pub(super) fn resolved(&self, sending: &mut super::copies::Sending<'_>) -> (Option<u64>, Option<crate::wire::ui::EventsWindow>, usize) {
        let Some(find) = self.find else { return (self.since, self.window, self.from) };
        if let Some(after) = self.since
            && self.events.fit_since(after, sending)
        {
            return (Some(after), None, 0);
        }
        let from = self.events.window_start(self.events.len(), find.turns, sending);
        (None, Some(super::copies::window_at(&self.events, &self.state, from, find.older)), from)
    }
}

/// How a viewer takes a session's events, as its socket's query says (docs/wire.md, "Windows"):
/// `window` (`?window=1`), its first load the newest alone, as many as a window holds, older pages
/// asked for as it scrolls back; `lean` (`?lean=1`), settled tool calls without their bodies, each
/// asked for when it's opened. One from before either takes every event, whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct View {
    pub window: bool,
    pub lean: bool,
}

/// An older page of a session's events, from its copy ([`Registry::page`]).
pub enum Paged {
    /// The events' JSON as the viewer takes them, oldest first, and where the page begins.
    Page { events: Vec<String>, window: crate::wire::ui::EventsWindow },
    /// The copy has none before `before`: what's kept of them is in the conversation store, from
    /// number `floor` (where the timeline begins) on.
    Store { before: u64, floor: u64 },
}

/// A snapshot's events go in frames of about this many bytes.
const EVENTS_FRAME: usize = 256 * 1024;

/// A viewer's snapshot (or catch-up), taken from a copy's view but not written out: written on the
/// blocking pool, off the runtime's threads, a long session's being megabytes of JSON
/// (`crate::rt::blocking`). Its frames say the copy as of when it was taken, whatever comes in
/// meanwhile.
pub struct Snapshot {
    pub(super) parts: Parts,
    /// What's kept of the files its messages mention (`Keeper::frame`): ahead of the events.
    pub(super) files: Option<String>,
    /// Where a spilled chunk whose file doesn't give it back is read from instead (`super::spill`; goal 11,
    /// plan 01a1a87b2e56 step 5): the conversation store, which keeps every event of the session, by its
    /// events' numbers, with the session's id. None where the server keeps no conversations (its runners
    /// keep each session whole), and in tests.
    pub(super) store: Option<(crate::convo::store::Store, Arc<str>)>,
}

/// A whole snapshot's events at the most: as many as a copy keeps before it lets its oldest go
/// (`MAX_EVENTS` and a quarter: `super::copies::Mirror::upsert`), at the packed size an event measured
/// (`super::copies::PACKED_EVENT_BYTES`, research f20), 3.2 MB (goal 11, plan 01a1a87b2e56 step 3).
const SNAPSHOT_BYTES: usize = (super::core::MAX_EVENTS + super::core::MAX_EVENTS / 4) * super::copies::PACKED_EVENT_BYTES;

/// The room of viewers' snapshots in flight, made and not yet written to their sockets
/// (`crate::memory::SNAPSHOTS`; goal 11, plan 01a1a87b2e56 step 3): as many as the blocking pool makes at
/// once (its `fs` share's places, which the server's profile takes from the lab's measure,
/// `crate::rt::SERVER_FS`: 26), each a whole snapshot's events at the most ([`SNAPSHOT_BYTES`]), 84 MB on
/// the server. At the bound an attach waits its turn ([`InFlight::take`]: a task's wait, its shard goes on
/// with its other work) until one in flight is written out; one larger than the room goes alone.
pub fn snapshot_room() -> &'static crate::memory::queue::Room {
    static ROOM: std::sync::OnceLock<crate::memory::queue::Room> = std::sync::OnceLock::new();
    ROOM.get_or_init(|| crate::memory::queue::Room::new(&crate::memory::SNAPSHOTS, snapshot_bound(crate::rt::profile().pool.2)))
}

/// The room of snapshots in flight for a blocking pool whose `fs` share has `places`.
fn snapshot_bound(places: usize) -> usize {
    places.max(1).saturating_mul(SNAPSHOT_BYTES)
}

/// A snapshot's bytes taken of a room of snapshots in flight ([`snapshot_room`]): what's left of them given
/// back as it goes, however it ends (written, an error, a socket gone).
pub struct InFlight {
    room: &'static crate::memory::queue::Room,
    bytes: usize,
}

impl InFlight {
    /// `bytes` of `room`, waiting for them to fit (a task's wait): none taken once it's closed.
    pub async fn take(room: &'static crate::memory::queue::Room, bytes: usize) -> Self {
        let bytes = if room.take(bytes).await { bytes } else { 0 };
        Self { room, bytes }
    }

    /// The snapshot as it was made, `n` bytes: the room holds that, not the guess it was taken by (more taken
    /// whatever the room holds then, as it's made already; or what wasn't needed given back).
    pub fn made(&mut self, n: usize) {
        match n.checked_sub(self.bytes) {
            Some(more) => self.room.force(more),
            None => self.room.give(self.bytes.saturating_sub(n)),
        }
        self.bytes = n;
    }

    /// `n` of its bytes written to the socket: given back now.
    pub fn written(&mut self, n: usize) {
        let n = n.min(self.bytes);
        self.room.give(n);
        self.bytes = self.bytes.saturating_sub(n);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.room.give(self.bytes);
    }
}

/// One of a snapshot's frames, in the order they go: its head (`open` or `resumed`), its meta,
/// state, changes and kept files, then its events: a whole snapshot's a chunk of the copy's at a time
/// (`super::copies::Chunk`, each packed once: `super::packed`), a catch-up's those it brings a batch at
/// a time (where they are among them).
enum Part {
    Head,
    History,
    Meta,
    State,
    Changes,
    Files,
    /// A batch of the events it brings (where they are among them), and the bytes its frame takes, reckoned as it was
    /// planned (its room: nothing read again for it).
    Events(std::ops::Range<usize>, usize),
    /// The copy's chunk of events at this place among its chunks, whole.
    Chunk(usize),
}

/// What a snapshot written out holds of its frames: its frames in order, and what they're written of.
struct Plan {
    parts: Vec<Part>,
    of: Planned,
}

/// What a snapshot's frames are written of ([`Plan`]): where the events it brings are in its parts', each as its viewer is
/// sent it where it was read planning them (none for one read again where it's written: past a window's bytes, kept by
/// none), taken as its frame is written; and since what revision it brings them, or the window they are
/// ([`Parts::resolved`]), which its head says.
struct Planned {
    brought: Vec<usize>,
    sent: Vec<Option<super::copies::Sent>>,
    since: Option<u64>,
    window: Option<crate::wire::ui::EventsWindow>,
    history: Option<super::history::Window>,
}

impl Snapshot {
    /// About the bytes it takes made, `packed` as one message or as frames: what it takes of the room of
    /// snapshots in flight before it's made ([`snapshot_room`]; [`InFlight::made`] counts it as it was made).
    /// Packed, each chunk's segment as it's kept (in memory or spilled) and a quarter of the texts of one
    /// that isn't packed (deflate's ratio on them), beside the other frames whole; as frames, its texts.
    ///
    /// Reckoned on its viewer's shard, as its attach's first poll (`api::send_snapshot`): a chunk's texts are read from
    /// its metas only where it has no segment to say them (the newest, and any the packer hasn't come to: a chunk or two),
    /// so it's a few steps a chunk (goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A5). Each chunk's texts were
    /// added up from its metas, a meta for each of a long session's events (polls of 0.65 to 2.4 ms on the shard in g9p16
    /// with hogs, goal 9, plan 01a1a87b2cec step 3), and then on the blocking pool, a round trip for every attach.
    pub fn weight(&self, packed: bool) -> usize {
        let p = &self.parts;
        // Its meta's frame only where it sends one (`plan`): a null meta has none.
        let meta = if p.meta.value().is_null() { 0 } else { self.room(&Part::Meta) };
        let frames = meta.saturating_add(p.state_bytes).saturating_add(self.files.as_ref().map_or(0, String::len));
        let events = p.events.chunks().iter().map(|c| match (packed, c.packed(), c.spilled_bytes()) {
            (true, Some(s), _) => s.deflated.len(),
            (true, None, Some(n)) => n,
            (true, None, None) => c.texts_len() / 4,
            (false, _, _) => c.segment_texts_len().unwrap_or_else(|| c.texts_len()),
        });
        events.fold(frames, usize::saturating_add)
    }

    /// Its frames, in order. The events it brings are all of them, a frame for each of the copy's
    /// chunks (goal 10, plan 01a1a87b2dae step 3: each chunk's frame is packed once, then taken as it
    /// is), or those changed since its revision, in batches of about [`EVENTS_FRAME`] bytes each (one
    /// alone when it's larger). The whole session planned from the events' metas alone; the rest from its
    /// events as their viewer is sent them, each read once for finding its window, planning its frames and
    /// writing them (goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A2a: the UI's lean window read
    /// each of its compact chunks again, inflated and each of their events read whole, four times an attach).
    fn plan(&self) -> Plan {
        let p = &self.parts;
        let mut parts = vec![Part::Head];
        if !p.meta.value().is_null() {
            parts.push(Part::Meta);
        }
        if !p.state.is_null() {
            parts.push(Part::State);
        }
        if p.changes.is_some() {
            parts.push(Part::Changes);
        }
        if self.files.is_some() {
            parts.push(Part::Files);
        }
        // Its window found here, where it's written (goal 9, plan 01a1a87b2cec step 3), each event read for it kept for
        // what follows.
        let mut sending = p.events.sending(p.lean);
        let (mut since, mut window, begins) = p.resolved(&mut sending);
        let mut history = None;
        let mut selected = None;
        if let Some(policy) = &p.history {
            // A resume retains all explicitly loaded pages. If its changes exceed the negotiated
            // budget it becomes a fresh bounded snapshot, as legacy window resumes already do.
            if since.is_some_and(|after| p.events.metas().filter(|m| m.at > after).fold(0usize, |n, m| n.saturating_add(m.text_len())) > policy.limits.bytes) { since = None; }
            if since.is_none() {
                let rows: Vec<_> = (0..p.events.len()).map(|i| sending.take(i).map_or_else(super::history::Row::default, |s| super::history::Row::read(s.text(p.lean)))).collect();
                let (indices, h) = super::history::select(&rows, &p.state.to_value(), policy.limits, policy.token.clone(), policy.durable);
                window = Some(crate::wire::ui::EventsWindow { before: h.before, more: h.has_more });
                selected = Some(indices); history = Some(h);
                parts.insert(1, Part::History);
            }
        }
        // The session whole, every event as it is: the copy's chunks, each packed once.
        if since.is_none() && window.is_none() && !p.lean {
            parts.extend(p.events.chunks().iter().enumerate().filter(|(_, c)| !c.is_empty()).map(|(i, _)| Part::Chunk(i)));
            return Plan { parts, of: Planned { brought: Vec::new(), sent: Vec::new(), since, window, history } };
        }
        // A window (W21-R15: from where it begins), what changed since a revision, or events that go lean.
        let brought: Vec<usize> = selected.unwrap_or_else(|| p.events.metas().enumerate().skip(begins).filter(|(_, m)| since.is_none_or(|s| m.at > s)).map(|(i, _)| i).collect());
        // Each as sent: as finding the window read it, or read now, and kept for its frame's writing. What isn't a window's
        // (all of what changed since a revision, or of a session that goes lean whole) is kept while what's kept is within a
        // window's bytes (`super::window::BYTES`), and the rest read again where it's written, so what a plan holds is
        // about a window's at the most; a window's own and what's read already are all kept.
        let mut sent = Vec::with_capacity(brought.len());
        let (mut kept, mut from, mut size) = (0usize, 0usize, 0usize);
        for (i, at) in brought.iter().enumerate() {
            let (s, n) = if kept <= super::window::BYTES || sending.has(*at) {
                let s = sending.take(*at);
                let n = s.as_ref().map_or(0, |s| s.text(p.lean).len());
                kept = kept.saturating_add(n);
                (s, n)
            } else if p.lean {
                (None, sending.take(*at).map_or(0, |s| s.text(true).len()))
            } else {
                (None, p.events.meta(*at).map_or(0, |m| m.text_len()))
            };
            if size + n > EVENTS_FRAME && i > from {
                parts.push(Part::Events(from..i, events_room(size, i - from)));
                from = i;
                size = 0;
            }
            size += n;
            sent.push(s);
        }
        if from < brought.len() {
            parts.push(Part::Events(from..brought.len(), events_room(size, brought.len() - from)));
        }
        Plan { parts, of: Planned { brought, sent, since, window, history } }
    }

    /// The events of `batch` (where they are among those `of` brings), as sent: each as its plan read it, taken (a batch is
    /// written once), and those it didn't keep read now, each compact chunk among them once.
    fn batch(&self, of: &mut Planned, batch: &std::ops::Range<usize>) -> Vec<super::copies::Sent> {
        let mut sending: Option<super::copies::Sending<'_>> = None;
        let mut out = Vec::with_capacity(batch.len());
        for j in batch.clone() {
            let s = match of.sent.get_mut(j).and_then(Option::take) {
                Some(s) => Some(s),
                None => of.brought.get(j).and_then(|at| sending.get_or_insert_with(|| self.parts.events.sending(self.parts.lean)).take(*at)),
            };
            out.extend(s);
        }
        out
    }

    /// About the bytes `part` takes: the room it's written into.
    fn room(&self, part: &Part) -> usize {
        let p = &self.parts;
        match part {
            // Its info, written as it's made (a few hundred bytes).
            Part::Head => 0,
            Part::History => 192 + p.history.as_ref().map_or(0, |h| h.token.len()),
            Part::Meta => write::envelope("meta", Some("meta"), None) + p.meta.json().len(),
            Part::State => p.state_bytes,
            Part::Changes => p.changes.as_ref().map_or(0, |files| write::envelope("changes", Some("files"), None) + write::texts_len(files.iter().map(|f| &**f))),
            Part::Files => self.files.as_ref().map_or(0, String::len),
            Part::Events(_, bytes) => *bytes,
            // Its texts joined, and the `[` and `]` around them.
            Part::Chunk(i) => write::envelope("events", Some("events"), None) + p.events.chunks().get(*i).map_or(0, |c| c.texts_len()) + "[]".len(),
        }
    }

    /// Writes `part` to `w`, of what `of` holds.
    fn write(&self, of: &mut Planned, part: &Part, w: &mut impl std::io::Write) -> std::io::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let p = &self.parts;
        match part {
            Part::Head => {
                let (info, born) = (serde_json::to_value(&*p.info).unwrap_or(Value::Null), Some(born()));
                let head = SessionDown { frame: if of.since.is_some() { SessionFrame::Resumed { info, born } } else { SessionFrame::Open { info, born, window: of.window } }, rev: Some(p.rev) }.text();
                w.write_all(head.as_bytes())
            }
            Part::History => {
                match &of.history {
                    Some(h) => {
                        let text = SessionDown::from(SessionFrame::History { cursor: h.cursor.clone(), before: h.before, has_more: h.has_more, oversized: h.oversized }).text();
                        w.write_all(text.as_bytes())
                    }
                    None => Ok(()),
                }
            }
            Part::Meta => write::meta(w, p.meta.json(), None),
            Part::State => write::state(w, &p.state, None),
            Part::Changes => match &p.changes {
                Some(files) => write::changes(w, files.iter().map(|f| &**f), None),
                None => Ok(()),
            },
            Part::Files => w.write_all(self.files.as_deref().unwrap_or_default().as_bytes()),
            Part::Events(batch, _) => write::sent_events(w, self.batch(of, batch).iter().map(|s| s.text(p.lean)), None),
            Part::Chunk(i) => match p.events.chunks().get(*i) {
                // A compact chunk's texts are its segment's, as they were joined.
                Some(c) if c.is_compact() => write::joined_events(w, &self.joined(c)?, None),
                Some(c) => write::events(w, c.evs()?.iter().map(|e| &*e.entry), None),
                None => write::events(w, std::iter::empty::<&Entry>(), None),
            },
        }
    }

    /// A compact chunk's texts joined: its segment's, or a spilled one's its file doesn't give back, its
    /// events as the conversation store keeps them ([`Snapshot::read_from_store`]).
    fn joined(&self, c: &super::copies::Chunk) -> std::io::Result<Vec<u8>> {
        match c.joined() {
            Ok(joined) => Ok(joined.into_owned()),
            Err(e) if c.is_spilled() => self.read_from_store(c).map_err(|s| std::io::Error::other(format!("{e}; from the store: {s}"))),
            Err(e) => Err(e),
        }
    }

    /// A spilled chunk whose file didn't give it back (`super::spill` refuses what doesn't read back as
    /// written), its events' texts as the conversation store keeps them, by their numbers, joined as an
    /// `events` frame joins them: the session's events are never lost to a viewer while the store has them
    /// (goal 11, plan 01a1a87b2e56 step 5). Read where the snapshot is made, on the blocking pool.
    fn read_from_store(&self, c: &super::copies::Chunk) -> std::io::Result<Vec<u8>> {
        let (store, id) = self.store.as_ref().ok_or_else(|| std::io::Error::other("no conversation store to read it from"))?;
        let (Some(first), Some(last)) = (c.metas().first().and_then(|m| m.seq()), c.metas().last().and_then(|m| m.seq())) else {
            return Err(std::io::Error::other("its events have no numbers to read them by"));
        };
        let n = usize::try_from(last.saturating_sub(first).saturating_add(1)).unwrap_or(usize::MAX);
        let rows = store.events(id, last.checked_add(1), n).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        tracing::warn!(session = &**id, first, last, "a spilled chunk's file didn't give it back: its events are read from the conversation store");
        let mut joined = Vec::new();
        for (i, r) in rows.iter().filter(|r| r.seq >= first && r.seq <= last).enumerate() {
            if i > 0 {
                joined.push(b',');
            }
            joined.extend_from_slice(r.body.as_bytes());
        }
        Ok(joined)
    }

    /// The frames that bring a viewer to the copy as it was when this was taken, a message each,
    /// written here and now (tests, and what isn't on a shard).
    pub fn frames(&self) -> Vec<String> {
        let Plan { parts, mut of } = self.plan();
        parts.iter().map(|part| write::string(self.room(part), |b| self.write(&mut of, part, b))).collect()
    }

    /// Its frames and then `last` (the socket's `synced`), a frame a line, gzip'd as one message, put
    /// together from pieces (goal 10, plan 01a1a87b2dae step 3; `super::packed`): each chunk of events
    /// as it was packed once ([`super::copies::Chunk::pack`]; the newest, which the next event changes,
    /// deflated now and kept only once it's full), between them the few bytes of its frame stored as
    /// they are, and the other frames deflated as they come. One gzip member: every decoder reads it.
    pub fn packed(&self, last: &str) -> std::io::Result<Vec<u8>> {
        let Plan { parts, mut of } = self.plan();
        let chunks = self.parts.events.chunks();
        let room = chunks.iter().map(|c| c.packed().map_or_else(|| c.texts_len() / 4, |s| s.deflated.len())).sum::<usize>();
        let mut gz = super::packed::Gzip::new(room);
        // The frames between the chunks, deflated together.
        let mut loose: Vec<u8> = Vec::new();
        for part in &parts {
            self.part_into(&mut of, part, &mut gz, &mut loose)?;
        }
        loose.extend_from_slice(last.as_bytes());
        gz.deflate(flate2::Compression::fast(), &loose)?;
        Ok(gz.finish())
    }

    /// `part` of a plan, of what `of` holds, as [`Snapshot::packed`] puts it in: a chunk of events after the frames
    /// before it (in `loose`, deflated together), as it was packed (the newest, which the next event changes, deflated
    /// now), between the few bytes of its frame stored as they are; another frame onto `loose`.
    fn part_into(&self, of: &mut Planned, part: &Part, gz: &mut super::packed::Gzip, loose: &mut Vec<u8>) -> std::io::Result<()> {
        use flate2::Compression;
        match part {
            Part::Chunk(i) => {
                let chunks = self.parts.events.chunks();
                let Some(chunk) = chunks.get(*i) else { return Ok(()) };
                gz.deflate(Compression::fast(), loose)?;
                loose.clear();
                gz.stored(br#"{"t":"events","events":["#);
                if *i + 1 == chunks.len() && !chunk.is_full() {
                    gz.segment(&chunk.deflate(Compression::fast())?);
                } else {
                    let packed = match chunk.pack() {
                        Ok(packed) => packed,
                        // A spilled one its file doesn't give back: its events as the store keeps them.
                        Err(e) if chunk.is_spilled() => {
                            let joined = self.read_from_store(chunk).map_err(|s| std::io::Error::other(format!("{e}; from the store: {s}")))?;
                            Arc::new(super::packed::deflate(Compression::default(), [joined.as_slice()])?)
                        }
                        Err(e) => return Err(e),
                    };
                    gz.segment(&packed);
                }
                gz.stored(b"]}\n");
            }
            other => {
                loose.reserve(self.room(other).saturating_add(1));
                self.write(of, other, loose)?;
                loose.push(b'\n');
            }
        }
        Ok(())
    }

    /// It packed as it goes out ([`Packing`]): the message [`Snapshot::packed`] makes at once, made a piece
    /// at a time, `last` its end. Planned as its first piece is made, off the shards, not here (goal 9, plan
    /// 01a1a87b2cec step 3: `api::send_snapshot` takes it on the socket's shard, and a plan finds its window).
    pub fn packing(self, last: &str) -> Packing {
        Packing { snapshot: self, plan: None, next: 0, gz: Some(super::packed::Gzip::new(0)), loose: Vec::new(), last: last.to_owned() }
    }
}

/// A snapshot packed as it goes out (goal 11, plan 01a1a87b2e56 step 5): its message a piece at a time, each
/// made on the blocking pool and written before the next is made. A viewer that reads slowly holds a piece and
/// the next, not its whole snapshot, nor a place of the pool while it reads: made at once, a long session's
/// packed snapshot (megabytes) was held until its last piece went, and a place held for all of its making.
pub struct Packing {
    snapshot: Snapshot,
    /// Its plan, made with its first piece.
    plan: Option<Plan>,
    /// Its next part.
    next: usize,
    /// The message being made; none once it's all out.
    gz: Option<super::packed::Gzip>,
    loose: Vec<u8>,
    last: String,
}

impl Packing {
    /// Its next piece: at least `piece` bytes of the message as it's made (the last may be fewer), none once
    /// it's all out. Piece after piece, the bytes [`Snapshot::packed`] makes at once.
    pub fn next_piece(&mut self, piece: usize) -> std::io::Result<Option<Vec<u8>>> {
        let Self { snapshot, plan, next, gz: making, loose, last } = self;
        let Some(gz) = making.as_mut() else { return Ok(None) };
        let plan = plan.get_or_insert_with(|| snapshot.plan());
        while gz.made() < piece {
            let Some(part) = plan.parts.get(*next) else {
                // Every part in: the frames after the last chunk and `last`, then the member's end.
                loose.extend_from_slice(last.as_bytes());
                gz.deflate(flate2::Compression::fast(), loose)?;
                *loose = Vec::new();
                return Ok(making.take().map(super::packed::Gzip::finish));
            };
            *next += 1;
            snapshot.part_into(&mut plan.of, part, gz, loose)?;
        }
        Ok(Some(gz.take()))
    }

    /// Whether its message is all made: its last piece was (goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A6: a
    /// piece known last as it's made, no job is asked for after it to find it all out).
    pub fn done(&self) -> bool {
        self.gz.is_none()
    }
}

/// The bytes an `events` frame of `n` events takes whose texts as sent are `texts` bytes in all: its envelope, the `[` and
/// `]`, and a `,` between each two (`write::sent_events`).
fn events_room(texts: usize, n: usize) -> usize {
    write::envelope("events", Some("events"), None) + "[]".len() + texts + n.saturating_sub(1) * ",".len()
}

/// A long session's whole snapshot, for `crate::api`'s tests of how one is sent (goal 11, plan 01a1a87b2e56 step 5, the
/// memory audit's rows A5, A6 and C12): `events` replies of text deflate shortens by about half (hex digits of numbers
/// hashed apart), about 1 KiB each, its sealed chunks packed and kept compact, its newest open.
#[cfg(test)]
pub(crate) fn long_snapshot(events: u64) -> Snapshot {
    let mut m = super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
    for q in 1..=events {
        m.rev = q;
        let text: String = (0..64u64).map(|k| format!("{:016x}", (q * 64 + k).wrapping_mul(0x9E37_79B9_7F4A_7C15))).collect();
        m.upsert(Arc::new(Entry::new(serde_json::json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": text}))));
    }
    for c in m.events.chunks().iter().filter(|c| c.is_full()) {
        let _ = c.pack();
    }
    m.events.compact(None);
    m.publish();
    Snapshot { parts: m.view().parts(), files: None, store: None }
}

/// The epoch to send a message once more for, when its session's inbox refused it as sent for an
/// epoch the session has since left (the server sends it once more, for the one named). The sends
/// simulation runs this too (`inbox::sim`).
pub(crate) fn again_for(r: &inbox::Refusal) -> Option<u64> {
    match r {
        inbox::Refusal::StaleEpoch { current, sent, .. } if current > sent => Some(*current),
        _ => None,
    }
}

/// How a command fared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The server committed custody; this is not receiver admission or model consumption.
    Published(super::mail::Receipt),
    /// Custody was not accepted. The original sender must retain its complete communication.
    PublicationRefused(super::mail::Error),
    /// On its way to the session's machine, which can't say more: a message whose inbox's answer
    /// didn't come in time, or a command or message to a runner from before the channel (or from
    /// before inboxes). Also what kiln does here itself (a name kept).
    Ok,
    /// The session's inbox took the message, or had: `repeat`, the first time's answer (CP-T3.2).
    Accepted(Answer),
    /// The session's inbox refused the message, saying why.
    Refused(inbox::Refusal),
    /// The session took the command: its runner applied it, or the server did (docs/wire.md,
    /// "Commands").
    Taken,
    /// The session's runner refused the command, saying how and why: one it doesn't run there any
    /// more is `unknown`.
    Rejected { code: crate::wire::channel::Refusal, why: String },
    /// The runner may or may not have applied it: its link dropped before it answered a command
    /// that doesn't wait, or no answer came in time.
    Unanswered(String),
    /// A command that waits for its runner, which is away: it goes once the runner is back.
    Parked,
    NoSession,
    /// The session's machine is not connected right now.
    Unreachable,
}

/// A command sent ([`Registry::dispatch`]): how it fared already, or the answer still to come.
pub enum Pending {
    Now(Sent),
    /// A message on its way, its inbox's answer waited for ([`Registry::settle`]).
    Answer { message: String, rx: futures::channel::oneshot::Receiver<Said> },
    /// A command on its runner's command stream, its answer to come ([`Sent::of`]).
    Command(crate::hub::Pending),
}

/// What a sender is told of message `message` the session's machine took: its inbox's answer and,
/// from a machine without inboxes (`seq` 0: none took it), that nothing there checks it for repeats
/// (docs/sends.md, "Off").
pub fn accepted(message: impl serde::Serialize, a: &Answer) -> Value {
    let mut said = json!({"sent": true, "message": message, "accepted": a});
    if a.seq == 0
        && let Some(o) = said.as_object_mut()
    {
        o.insert("unchecked".into(), json!("the agent's machine checks no messages for repeats (its inbox is off): sent again, this is another message"));
    }
    said
}

impl Sent {
    /// It got to the session's machine (and its inbox didn't refuse it), or waits for it.
    pub fn went(&self) -> bool {
        matches!(self, Self::Ok | Self::Accepted(_) | Self::Taken | Self::Parked)
    }

    /// What a runner's answer to a command says of it (`crate::wire::commands`).
    pub fn of(a: crate::wire::commands::Answer) -> Self {
        use crate::wire::commands::Answer;
        match a {
            Answer::Taken => Self::Taken,
            Answer::Refused { code, why } => Self::Rejected { code, why },
            Answer::Unreachable { .. } => Self::Unreachable,
            Answer::Unanswered { why } => Self::Unanswered(why),
            Answer::Sent => Self::Ok,
            Answer::Parked => Self::Parked,
        }
    }

    /// For a command's caller over HTTP: the status and words of one that didn't go (a message's
    /// answers are its inbox's, `accepted` and `inbox::Refusal`). None: it was taken, sent, or
    /// waits for its runner.
    pub fn failure(&self) -> Option<(http::StatusCode, String)> {
        use crate::wire::channel::Refusal as Code;
        use http::StatusCode;
        match self {
            Self::Published(_) => None,
            Self::PublicationRefused(error) => {
                Some((error.status_code(), serde_json::to_string(error).unwrap_or_else(|_| "communication custody was not accepted".into())))
            }
            Self::Ok | Self::Accepted(_) | Self::Taken | Self::Parked => None,
            Self::Refused(r) => Some((StatusCode::CONFLICT, r.to_string())),
            Self::Rejected { code: Code::Unknown, why } => Some((StatusCode::NOT_FOUND, format!("its runner doesn't have it: {why}"))),
            Self::Rejected { code: Code::StaleEpoch, why } => Some((StatusCode::CONFLICT, format!("its runner let go of it: {why}"))),
            Self::Rejected { why, .. } => Some((StatusCode::CONFLICT, format!("its runner refused it: {why}"))),
            Self::Unanswered(why) => Some((StatusCode::GATEWAY_TIMEOUT, format!("its runner didn't say whether it did it: {why}"))),
            Self::NoSession => Some((StatusCode::NOT_FOUND, "no such agent session".to_owned())),
            Self::Unreachable => Some((StatusCode::CONFLICT, "the session's machine is not connected".to_owned())),
        }
    }
}

/// How long a message waits for its session's inbox to answer: past it, it's said to be on its way.
pub const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Messages waiting for their inbox's answer at once, at most: past it, one isn't waited for.
pub const WAITING_MAX: usize = 10_000;

/// What a viewer gets on attach: its snapshot (written out off the runtime's threads,
/// [`Snapshot`]), and where on the session's topic the live frames after it start (none while the
/// bus can't hold its feed: snapshots alone, then).
pub struct Attachment {
    pub snapshot: Snapshot,
    pub topic: String,
    pub at: Option<kiln_seams::bus::Offset>,
    /// Its place among the session's viewers, while it watches.
    pub viewer: Viewer,
    /// The feed's refusals when the snapshot was taken: more later means the feed broke since.
    pub refusals: u64,
    /// The copy's revision as of `frames`: what its viewer resumes from.
    pub rev: u64,
}

impl Attachment {
    /// Its snapshot's frames, written out here and now (tests, and what isn't on a runtime thread).
    pub fn frames(&self) -> Vec<String> {
        self.snapshot.frames()
    }
}

/// A live frame for a session's viewers: a frame of the session socket's ([`SessionDown`],
/// serialized), as the session's feed carries it, so the registry and its viewers share the type. A
/// state comes as well as only what changed since the last one (`patch`), for a viewer that takes
/// that: during a turn its usage and activity change all the time, its skills, plugins and servers
/// seldom. So does a changeset: an agent touches a file or two of the hundreds it may have changed.
///
/// [`SessionDown`]: crate::wire::ui::SessionDown
#[derive(Clone)]
pub struct Frame {
    pub text: Arc<str>,
    pub patch: Option<Arc<str>>,
    /// What `patch` is a patch of: a viewer gets it if it takes that kind.
    pub patch_of: PatchOf,
    /// The copy's revision as it went (`"rev"` in its JSON): what its viewer resumes from.
    pub rev: u64,
}

/// What a frame's patch is a patch of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOf {
    /// A state (`state_patch`).
    State,
    /// A changeset (`changes_patch`).
    Changes,
}

impl PatchOf {
    /// What `patch` (a frame's JSON, its tag first) is a patch of: a frame on the bus carries no
    /// more than its text (`super::live::encode`), and one from before changesets had patches is a
    /// state's.
    pub fn of(patch: &str) -> Self {
        if patch.starts_with(r#"{"t":"changes_patch""#) { Self::Changes } else { Self::State }
    }
}

/// The patches a viewer takes, as its socket's query says (`patch=state`, `patch=changes`): one
/// from before a kind takes that kind whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Patches {
    pub state: bool,
    pub changes: bool,
}

impl Patches {
    /// Whether a viewer takes `of` as patches.
    pub fn takes(self, of: PatchOf) -> bool {
        match of {
            PatchOf::State => self.state,
            PatchOf::Changes => self.changes,
        }
    }
}

impl Frame {
    /// `down`, as the feed carries it.
    pub fn of(down: crate::wire::ui::SessionDown) -> Self {
        Self { text: Arc::from(down.text()), patch: None, patch_of: PatchOf::State, rev: down.rev.unwrap_or(0) }
    }

    /// The feed's resync marker ([`super::live::RESYNC`]): its viewers start over from a snapshot.
    /// It's the feed's own, never sent to a viewer.
    pub fn resync() -> Self {
        Self { text: Arc::from(super::live::RESYNC), patch: None, patch_of: PatchOf::State, rev: 0 }
    }

    /// Any text as a frame: the feed's tests, which carry words of their own.
    #[cfg(test)]
    pub fn text(text: &str) -> Self {
        Self { text: Arc::from(text), patch: None, patch_of: PatchOf::State, rev: 0 }
    }

    /// The frame with its copy's revision in it, as `"rev"`: as [`SessionDown`] says it, its last
    /// field (`a_frame_says_its_copys_revision`). Live frames are written with it now
    /// (`write`); this says what they must match.
    ///
    /// [`SessionDown`]: crate::wire::ui::SessionDown
    #[cfg(test)]
    fn stamped(self, rev: u64) -> Self {
        let stamp = |t: &str| -> Arc<str> {
            match t.strip_suffix('}') {
                Some(head) if head.len() > 1 => Arc::from(format!("{head},\"rev\":{rev}}}")),
                _ => Arc::from(t),
            }
        };
        Self { text: stamp(&self.text), patch: self.patch.as_deref().map(stamp), patch_of: self.patch_of, rev }
    }

    /// The frame for a viewer that takes the patches `takes` says.
    pub fn for_viewer(&self, takes: Patches) -> &str {
        match &self.patch {
            Some(p) if takes.takes(self.patch_of) => p,
            _ => &self.text,
        }
    }
}

/// A changeset's file as its JSON names it ([`crate::harness::model::FileKey`]): its path, and
/// the top of its working tree when it's in one. Borrowed from the file: naming a file makes nothing.
pub(super) type FileRef<'a> = (Option<&'a str>, Option<&'a str>);

pub(super) fn file_ref(f: &Value) -> FileRef<'_> {
    (f.get("path").and_then(Value::as_str), f.get("tree").and_then(Value::as_str))
}

/// A changeset's file's key, as a patch says it's gone (`{"path", "tree"?}`).
fn file_key((path, tree): FileRef<'_>) -> Value {
    let mut key = serde_json::Map::new();
    for (k, v) in [("path", path), ("tree", tree)] {
        if let Some(v) = v {
            key.insert(k.to_owned(), Value::String(v.to_owned()));
        }
    }
    Value::Object(key)
}

/// Takes `set` and `gone` into `files`, as a changeset's patch says: each file set goes in place of
/// the one with its key, or after the rest when it's new; each gone goes. A viewer takes a patch
/// the same way (the UI's `changesPatched`), so its list keeps this one's order.
pub(super) fn merge_changes(files: &mut Vec<Arc<Entry>>, set: Vec<Value>, gone: &[Value]) {
    merge_change_entries(files, set.into_iter().map(|f| Arc::new(Entry::new(f))).collect(), gone);
}

/// [`merge_changes`] of files written out already (`set`, each its entry: where a session's owner wrote them off its shard,
/// `super::copies::Shard::want_off`; goal 9, plan 01a1a87b2cec step 3), each keyed by its entry's path and tree as
/// [`file_ref`] keys its JSON.
pub(super) fn merge_change_entries(files: &mut Vec<Arc<Entry>>, set: Vec<Arc<Entry>>, gone: &[Value]) {
    if !gone.is_empty() {
        let out: std::collections::HashSet<FileRef<'_>> = gone.iter().map(file_ref).collect();
        files.retain(|f| !out.contains(&f.file()));
    }
    // Where each goes, by its key, borrowed (nothing copied to look it up): in place of the file
    // with its key, or after the rest, as one new before it with its key was.
    let places: Vec<usize> = {
        let mut at: HashMap<FileRef<'_>, usize> = files.iter().enumerate().map(|(i, f)| (f.file(), i)).collect();
        let mut next = files.len();
        set.iter()
            .map(|f| {
                *at.entry(f.file()).or_insert_with(|| {
                    next += 1;
                    next - 1
                })
            })
            .collect()
    };
    for (entry, i) in set.into_iter().zip(places) {
        match files.get_mut(i) {
            Some(slot) => *slot = entry,
            None => files.push(entry),
        }
    }
}

/// What changed in a changeset, for `{"t":"changes_patch","set":[..],"gone":[..]}`: the files of
/// `now` new or different from `was`'s, in `now`'s order (a file shared with `was` didn't change,
/// and isn't compared), and the keys of those it no longer has: taken into `was` as
/// [`merge_changes`] does, they make `now`. None when they wouldn't make `now` (a changeset that
/// started over in another order): then it goes whole.
pub(super) fn changes_patch_of<'a>(was: &'a [Arc<Entry>], now: &'a [Arc<Entry>]) -> Option<(Vec<&'a Entry>, Vec<Value>)> {
    let before: HashMap<FileRef<'a>, &'a Arc<Entry>> = was.iter().map(|f| (f.file(), f)).collect();
    let kept: std::collections::HashSet<FileRef<'a>> = now.iter().map(|f| f.file()).collect();
    let set: Vec<&'a Entry> = now.iter().filter(|f| before.get(&f.file()).is_none_or(|b| !Arc::ptr_eq(b, f) && b.text() != f.text())).map(|f| &**f).collect();
    let gone: Vec<Value> = was.iter().filter(|f| !kept.contains(&f.file())).map(|f| file_key(f.file())).collect();
    // Taken in, the files keep their places and the new ones come after: `now` must be in that order.
    let order = was.iter().map(|f| f.file()).filter(|id| kept.contains(id)).chain(set.iter().map(|f| f.file()).filter(|id| !before.contains_key(id)));
    if !order.eq(now.iter().map(|f| f.file())) {
        return None;
    }
    Some((set, gone))
}

/// [`changes_patch_of`] of two changesets as JSON, written out: None when either isn't one.
#[cfg(test)]
fn changes_patch(was: &Value, now: &Value) -> Option<String> {
    let files = |v: &Value| v.as_array().map(|a| a.iter().cloned().map(|f| Arc::new(Entry::new(f))).collect::<Vec<_>>());
    let (was, now) = (files(was)?, files(now)?);
    changes_patch_of(&was, &now).map(|(set, gone)| write::string(256, |b| write::changes_patch(b, &set, &gone, None)))
}

/// Immutable metadata for a durable uploaded file. Non-CAS namespace keys pin both objects
/// through the existing Blobs store; no registry lifetime/TTL cache owns accepted file bytes.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Upload {
    name: String,
    mime: String,
    size: u64,
    sha256: String,
}

impl Upload {
    fn bytes(id: &str) -> String { format!("attachments/uploads/{id}") }
    fn metadata(id: &str) -> String { format!("attachments/uploads/{id}.json") }
}

fn upload_refused(error: kiln_seams::blobs::Refusal) -> super::mail::Error {
    use kiln_seams::blobs::Refusal;
    match error {
        Refusal::Full { .. } => super::mail::Error::StorageFull,
        Refusal::TooLarge { .. } => super::mail::Error::TooLarge { revision: 0, budget: usize::try_from(super::host::ATTACHMENT_MAX).unwrap_or(usize::MAX) },
        Refusal::Invalid { .. } | Refusal::PreconditionFailed(_) => super::mail::Error::Invalid { reason: "attachment identity or content is invalid; custody was not accepted".into() },
        Refusal::NotFound(_) => super::mail::Error::Unavailable { reason: "the original attachment is unavailable; keep its original bytes and upload it again before sending".into() },
        Refusal::BadChecksum { .. } | Refusal::InvalidRange { .. } | Refusal::NoSuchUpload(_) | Refusal::BadParts(_) | Refusal::Expired | Refusal::Forbidden(_) | Refusal::Unavailable(_) => super::mail::Error::Unavailable { reason: "the original attachment could not be stored or verified; its communication remains held".into() },
    }
}

/// Pieces a file is sent to a machine in (lane frames are bounded).
const FILE_CHUNK: usize = 512 * 1024;

/// Restoring the conversation store from a mirror's copy of it (`docs/conversations.md`,
/// "Mirrors"): `GET /api/conversations/restore`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Restore {
    Idle,
    /// `from` was asked for its copy.
    Asked { from: String, at: i64 },
    /// `from` is sending its copy `store`.
    Copying { from: String, store: String, at: i64 },
    /// Done: `sessions` came back.
    Done { from: String, sessions: usize, at: i64 },
    Failed { from: String, error: String, at: i64 },
}

/// A mirror asked for its copy that hasn't begun sending it after this long isn't answering.
const RESTORE_ANSWER_MS: i64 = 120_000;

pub struct Registry {
    /// Every session's copy, each owned by one shard: changed only by messages to that shard's
    /// owner, read from the views it publishes, no lock guarding any (`super::copies`).
    sessions: Copies,
    /// The bus each session's live feed rides (`super::live`).
    bus: Arc<dyn kiln_seams::Bus>,
    /// What each machine offers ("local" included).
    kinds: Rcu<HashMap<String, Vec<KindInfo>>>,
    /// The server's own sessions.
    pub local: Arc<Host>,
    /// The server's conversation store (docs/conversations.md), when it's on.
    pub convo: Option<crate::convo::store::Store>,
    /// What runners' feeds send into it (`crate::convo::feed`), and what to answer them.
    pub feed: Option<crate::convo::feed::Intake>,
    feed_out: Option<crate::memory::queue::Receiver<crate::convo::feed::Out>>,
    /// The runners that keep a copy of it (`crate::convo::mirror`), and what to send them.
    pub copies: Option<Arc<crate::convo::mirror::Mirrors>>,
    copies_out: Option<crate::memory::queue::Receiver<crate::convo::mirror::Out>>,
    /// Every store, as the server sees it: the System page's (`super::stores`).
    pub board: crate::convo::board::Board,
    /// What agents' pages read of the store, kept in memory while it holds (`crate::reads::Views`:
    /// the plans' list, the goal stacks; goal 10, plan 01a1a87b2dae step 2).
    pub views: crate::reads::Views,
    local_rx: crate::memory::queue::Receiver<super::session::Local>,
    /// Where `kiln spawn` in the server's own terminals reaches `local`.
    local_sock: Rcu<Option<String>>,
    /// The shell pid of each of the server's own terminals (their directory and environment are
    /// where agents started "in this shell" begin).
    local_shells: Rcu<HashMap<String, u32>>,
    dir: std::path::PathBuf,
    /// The server machine's transcripts, for its past sessions (made on first use).
    scanner: Arc<Mutex<Option<crate::aisessions::Scanner>>>,
    /// Names people gave sessions, by id (the "rename" control), kept in `names.json`: a session
    /// takes its name here at once, whatever its machine's runner knows of names (one from before
    /// kiln named sessions never reports it), and keeps it across restarts. Read on every state a
    /// session's machine sends: by session id, which kiln makes (`crate::util::new_id`), a
    /// [`crate::util::FastMap`] (goal 9, plan 01a1a87b2cec step 5).
    names: Rcu<crate::util::FastMap<String, String>>,
    /// Sessions kiln doesn't keep going when they stop part-way (`goals::keep`; the "keep_going"
    /// control), kept in `keep-off.json` so it holds across restarts.
    keep_off: Rcu<std::collections::HashSet<String>>,
    /// The UI's commands by the ids their clients made, each taken once (`super::asked`).
    pub asked: super::asked::Asked,
    /// What sessions' copies lack of states their runners sent bounded, which the server reads from them
    /// (`super::fill`).
    pub fills: super::fill::Fills,
    /// One event pump over committed communication custody, with no blocked-body task list.
    pub mail_delivery: super::mail_delivery::Relay,
    /// Which of its machine's runner processes holds each session on a machine (its instance:
    /// `main`, a uuid, `legacy`), kept in `holders.json` so it holds across restarts: a process
    /// that says what it holds speaks for its own sessions, never for another's that is away
    /// ([`Registry::inventory`]). Shared with the writes of it, which go on the blocking pool.
    holders: Arc<Rcu<HashMap<String, String>>>,
    /// A write of `holders.json` is on its way (it writes what's newest as it goes); and the one
    /// writing it now (they share the file, and the one it's written beside first).
    holders_saving: Arc<std::sync::atomic::AtomicBool>,
    holders_writing: Arc<Mutex<()>>,
    /// Each session's changeset to keep in the conversation store, the newest alone (shared with
    /// its copy, nothing written out yet), and whether its writer is at work
    /// ([`Registry::store_changes`]).
    changes_to_store: Arc<Rcu<HashMap<String, Files>>>,
    changes_storing: Arc<std::sync::atomic::AtomicBool>,
    /// Each session's latest epoch (`State::epoch`) seen, those that went on on another machine at
    /// least once, kept in `epochs.json`: a machine that announces an older copy lets go of it. Read
    /// on every update: by session id, a [`crate::util::FastMap`] as [`Registry::names`] is.
    epochs: Rcu<crate::util::FastMap<String, u64>>,
    /// The plan each session last proposed (plan mode), not yet kept as one of kiln's plans: the
    /// goals' driver keeps it ([`Self::take_proposed`], `crate::plans::keep_proposed`).
    proposed: Rcu<HashMap<String, String>>,
    /// Restoring the conversation store from a mirror's copy of it.
    restore: Rcu<Restore>,
    /// The mirrors to ask for their copy as they connect: the store was found empty at start
    /// while the server had mirrors (it was lost, the rest wasn't).
    restore_auto: Rcu<Vec<String>>,
    /// Files finished messages mention, kept so they still show after they go (`super::kept`).
    pub keeper: Keeper,
    /// Pages agents show in their conversations (`super::visuals`).
    pub visuals: super::visuals::Visuals,
    /// Pages streamed from agents' machines, relayed to their viewers (`super::streams`).
    pub streams: super::streams::Relay,
    /// Streams to their viewers over WebRTC, when the server has it (`super::webrtc`).
    pub webrtc: super::webrtc::Webrtc,
    /// Connections to ports of agents' machines, and the apps opened on people's devices over
    /// them (`super::opened`).
    pub ports: super::opened::Tunnels,
    pub opened: super::opened::Openings,
    /// Messages waiting for their session's inbox to answer (CP-T3.2), by session and message id: one owner
    /// of their own, no lock (`super::answers`; goal 5, plan 01a1a59c2aae, its top goal).
    answers: super::answers::Answers,
    /// The ids starts under way asked for ([`Registry::open_as`]), each until its start is done.
    opening: Rcu<std::collections::HashSet<String>>,
    /// What a viewer's attach waits for while the server starts ([`Registry::returned`]).
    pub returning: super::returning::Returning,
    /// Tests: what every session started here runs instead of its harness (a program that isn't
    /// there, so no harness ever starts, whatever this machine has installed).
    #[cfg(test)]
    test_program: Mutex<Option<String>>,
}

/// The name a session's machine reports as its own: none when it's empty or its harness's name.
fn own_name<'a>(name: &'a str, kind_name: &str) -> Option<&'a str> {
    let n = name.trim();
    (!n.is_empty() && !n.eq_ignore_ascii_case(kind_name.trim())).then_some(n)
}

/// Whose name a session shows once its machine reports `reported` in a state.
#[derive(Debug, PartialEq, Eq)]
enum Naming<'a> {
    /// A name of its own the machine gives anew (not the `last` it gave): it's kept here too.
    Adopt(&'a str),
    /// The name kept here: the machine has none of its own (a runner from before names, or one
    /// upgraded since), or hasn't taken the one given here yet.
    Keep,
    /// The machine's: nothing is kept here.
    Theirs,
}

fn naming<'a>(reported: &'a str, last: &str, kind_name: &str, kept: bool) -> Naming<'a> {
    match own_name(reported, kind_name) {
        Some(own) if own != last => Naming::Adopt(own),
        _ if kept => Naming::Keep,
        _ => Naming::Theirs,
    }
}

async fn send_package(hub: &Hub, id: &str, to: &str, bytes: &[u8]) -> Result<(), String> {
    // In pieces that fit a command's frame, each answered before the next goes (docs/wire.md,
    // "Commands"): a move whose machine went away mid-way says so.
    const PIECE: usize = 768 << 10;
    let pieces: Vec<&[u8]> = bytes.chunks(PIECE).collect();
    let mut offset = 0u64;
    for (n, piece) in pieces.iter().enumerate() {
        use crate::wire::commands::Answer;
        let msg = ServerMsg::HarnessPackage { session: id.to_owned(), offset, data: piece.to_vec(), done: n + 1 == pieces.len() };
        match hub.command(to, &msg, crate::hub::COMMAND_WITHIN).await {
            Answer::Taken | Answer::Sent => {}
            Answer::Refused { why, .. } => return Err(format!("{} couldn't take the session: {why}", hub.shown(to))),
            Answer::Unreachable { .. } | Answer::Parked => return Err(format!("{} went away while the session was on its way", hub.shown(to))),
            Answer::Unanswered { why } => return Err(format!("{} didn't answer while the session was on its way: {why}", hub.shown(to))),
        }
        offset += piece.len() as u64;
    }
    Ok(())
}

/// A start was asked under an id that's a session's already, or another start's under way
/// ([`Registry::open_as`]).
#[derive(Debug)]
pub struct IdTaken(pub String);

impl std::fmt::Display for IdTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "an agent {} is there already, or starting", self.0)
    }
}

impl std::error::Error for IdTaken {}

/// The ids a start holds while it's under way ([`Registry::claim`]), let go of as it ends.
struct Claim<'a> {
    held: &'a Rcu<std::collections::HashSet<String>>,
    ids: Vec<String>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        self.held.update(|held| {
            for id in &self.ids {
                held.remove(id);
            }
        });
    }
}

impl Registry {
    /// A registry of tests: its store in `dir`, its feeds on the bus's simulator.
    #[cfg(test)]
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self::with(dir, &crate::config::Conversations::default(), crate::rt::clock(), Arc::new(kiln_seams::bus::SimBus::default()))
    }

    /// The registry, its store in `dir`, its mirrors as `conversations` says, the server's own
    /// sessions' host on `clock` (the hub's), its sessions' live feeds on `bus`.
    pub fn with(dir: std::path::PathBuf, conversations: &crate::config::Conversations, clock: Arc<dyn kiln_seams::Clock>, bus: Arc<dyn kiln_seams::Bus>) -> Self {
        // The server's own sessions' updates, waiting for the loop that hands them to their owners
        // (`run_local`): as many bytes as an owner's inbox holds (`crate::net::EVENTS_BYTES`), where they go
        // next; past it a session waits to send (goal 11, plan 01a1a87b2e56 step 3). Its 8,192 places, made
        // up front, took 1.05 MB of an idle server's heap (research 01a1a87b2e57, f10).
        let (tx, rx) = crate::memory::queue::bounded::<super::session::Local>(&crate::memory::owners::LOCAL_UPDATES, crate::net::EVENTS_BYTES);
        // Every session's conversation, on every machine, kept on disk as it comes in
        // (docs/conversations.md): the server's own sessions' too, as they pass through `ingest`.
        let convo = if conversations.off { None } else { crate::convo::open(&dir, conversations.upkeep()) };
        // No record goes before kiln's store of bytes has it: held back entirely until the archive
        // of transcripts (`super::archive`) says how far it has come.
        if let Some(c) = &convo {
            c.hold_prune(0);
        }
        // Runners that keep a store send it here; the server keeps their sessions from that. What
        // a mirror sends back to restore the store from isn't sent to it again.
        let echoes = std::sync::Arc::new(crate::convo::feed::Echoes::default());
        let (feed, feed_out) = match convo.as_ref().map(|st| crate::convo::feed::Intake::start(st.clone(), crate::convo::feed::Brings::TheirMachine, Some(echoes.clone()))) {
            Some(Ok((i, o))) => (Some(i), Some(o)),
            Some(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "runners' conversation stores won't reach the server's");
                (None, None)
            }
            None => (None, None),
        };
        let (copies, copies_out) = match convo.clone().filter(|_| conversations.mirrors > 0) {
            Some(st) => {
                let (m, out) = crate::convo::mirror::Mirrors::start(st, conversations.mirrors, conversations.mirror_on.clone(), dir.join("mirrors.json"), Some(echoes));
                (Some(m), Some(out))
            }
            None => (None, None),
        };
        let restore_auto = match (&copies, &convo) {
            (Some(m), Some(st)) if st.is_empty().unwrap_or(false) => m.chosen_machines(),
            _ => Vec::new(),
        };
        if !restore_auto.is_empty() {
            tracing::info!(mirrors = ?restore_auto, "the conversation store is empty: it is restored from a mirror's copy once one connects");
        }
        let local = Host::with_clock(tx, Some(dir.join("local.json")), Limits::default(), clock);
        // The server's own agents' workspaces of their own are kept beside their record.
        crate::workspace::slates::init(&dir);
        super::whose::note_kiln(&dir);
        let keeper = Keeper::new(&dir);
        let visuals = super::visuals::Visuals::new(&dir);
        // Its copies' sealed chunks spilled beside its store (goal 11, plan 01a1a87b2e56 step 5): a folder
        // that can't be made leaves them in memory, said.
        let sessions = Copies::start(Arc::clone(&bus));
        if let Err(e) = sessions.spill_to(&dir.join("spill")) {
            tracing::warn!(error = %e, "the session copies' spill folder couldn't be made: their sealed chunks stay in memory");
        }
        // The room of viewers' snapshots in flight made now, so /metrics says its bound from the start (goal 11,
        // plan 01a1a87b2e56 step 3).
        let _ = snapshot_room();
        Self {
            sessions,
            bus,
            kinds: Rcu::default(),
            local,
            convo,
            feed,
            feed_out,
            copies,
            copies_out,
            board: crate::convo::board::Board::default(),
            views: crate::reads::Views::default(),
            local_rx: rx,
            local_sock: Rcu::default(),
            local_shells: Rcu::default(),
            names: Rcu::new(std::fs::read(dir.join("names.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            keep_off: Rcu::new(std::fs::read(dir.join("keep-off.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            asked: super::asked::Asked::default(),
            fills: super::fill::Fills::default(),
            mail_delivery: super::mail_delivery::Relay::default(),
            holders: Arc::new(Rcu::new(std::fs::read(dir.join("holders.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())),
            holders_saving: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            holders_writing: Arc::new(Mutex::new(())),
            changes_to_store: Arc::default(),
            changes_storing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            epochs: Rcu::new(std::fs::read(dir.join("epochs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            proposed: Rcu::default(),
            dir,
            scanner: Arc::new(Mutex::new(None)),
            restore: Rcu::new(Restore::Idle),
            restore_auto: Rcu::new(restore_auto),
            keeper,
            visuals,
            streams: super::streams::Relay::default(),
            webrtc: super::webrtc::Webrtc::default(),
            ports: super::opened::Tunnels::default(),
            opened: super::opened::Openings::default(),
            answers: super::answers::Answers::new(WAITING_MAX),
            opening: Rcu::default(),
            returning: super::returning::Returning::default(),
            #[cfg(test)]
            test_program: Mutex::new(None),
        }
    }

    /// The hub this registry serves, once it's made: what each session's owner acts through
    /// (`super::copies`).
    pub fn bind(&self, hub: &Arc<Hub>) {
        self.sessions.bind(hub);
    }

    /// Back once every session's owner has applied what was sent it before this (tests, and what
    /// reads every copy next).
    pub async fn settled(&self) {
        self.sessions.settled().await;
    }

    /// Back once session `id`'s owner has applied what was sent it before this.
    pub async fn applied(&self, id: &str) {
        self.sessions.applied(id).await;
    }

    /// Tests: session `id`'s copy's revision, as its newest view says.
    #[cfg(test)]
    pub fn rev(&self, id: &str) -> Option<u64> {
        self.sessions.with(id, |v| v.rev)
    }

    /// Tests: every session's id, in order.
    #[cfg(test)]
    pub fn ids(&self) -> Vec<String> {
        let mut ids = self.sessions.ids();
        ids.sort();
        ids
    }

    /// Tests: sessions started here from now on run `program` instead of their harness.
    #[cfg(test)]
    pub fn set_test_program(&self, program: &str) {
        *self.test_program.lock() = Some(program.to_owned());
    }

    /// Where restoring the conversation store from a mirror's copy stands.
    pub fn restore_view(&self) -> Restore {
        // Read off its copy; one swapped in only as an ask goes unanswered too long.
        let now = crate::util::now_ms();
        let unanswered = |r: &Restore| match r {
            Restore::Asked { from, at } if now - at > RESTORE_ANSWER_MS => Some(Restore::Failed { from: from.clone(), error: "no answer: its kiln may be too old to send its copy back".into(), at: now }),
            _ => None,
        };
        if unanswered(&self.restore.load()).is_some() {
            self.restore.update(|r| {
                if let Some(failed) = unanswered(r) {
                    *r = failed;
                }
            });
        }
        Restore::clone(&self.restore.load())
    }

    /// Ask `machine`, a mirror, for its copy of the conversation store, to restore this one from:
    /// what the copy brings fills in what the store lacks (`docs/conversations.md`, "Mirrors").
    pub fn restore_from(&self, hub: &Arc<Hub>, machine: &str) -> anyhow::Result<()> {
        let st = self.convo.as_ref().ok_or_else(|| anyhow::anyhow!("the conversation store is off"))?;
        anyhow::ensure!(self.feed.is_some(), "this server takes no conversation feeds");
        let peer = hub.instances.load().newest_with(machine, crate::proto::CAP_CONVO).ok_or_else(|| anyhow::anyhow!("{} isn't connected, or keeps no conversation store", hub.shown(machine)))?;
        if matches!(self.restore_view(), Restore::Asked { .. } | Restore::Copying { .. }) {
            anyhow::bail!("a restore is already under way");
        }
        self.restore.set(Restore::Asked { from: machine.to_owned(), at: crate::util::now_ms() });
        self.restore_auto.set(Vec::new());
        tracing::info!(%machine, "asked a mirror for its copy of the conversation store, to restore this one from");
        let json = crate::convo::feed::encode(&crate::convo::feed::Msg::Restore { store: st.id().to_owned() });
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data: Vec::new() }) {
                h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
            }
        });
        Ok(())
    }

    /// Mirror `machine` begins sending back its copy `store`: whether it was asked for.
    pub fn restore_accepts(&self, machine: &str, store: &str) -> bool {
        let at = crate::util::now_ms();
        self.restore.update(|r| {
            let begins = match &*r {
                Restore::Asked { from, .. } => from == machine,
                // Said again (it asks again after a silence).
                Restore::Copying { from, store: s, .. } => return from == machine && s == store,
                _ => false,
            };
            if begins {
                *r = Restore::Copying { from: machine.to_owned(), store: store.to_owned(), at };
            }
            begins
        })
    }

    /// Mirror `machine` has sent back its copy `store` whole (an empty `store`: it has none):
    /// the sessions it brought back are listed, away, on the machines they ran on.
    pub fn restore_done(&self, hub: &Arc<Hub>, machine: &str, store: &str) {
        let state = Restore::clone(&self.restore.load());
        match state {
            Restore::Asked { from, .. } if from == machine && store.is_empty() => {
                self.restore.set(Restore::Failed { from, error: format!("{} keeps no copy of the conversation store", hub.shown(machine)), at: crate::util::now_ms() });
            }
            Restore::Copying { from, store: s, .. } if from == machine && s == store => {
                let (h, st) = (Arc::clone(hub), self.convo.clone());
                crate::rt::spawn(async move {
                    let ended = match st {
                        Some(st) => crate::rt::blocking(move || st.end_copy()).await.and_then(|r| r),
                        None => Err(anyhow::anyhow!("the conversation store is off")),
                    };
                    let at = crate::util::now_ms();
                    match ended {
                        Ok(sessions) => {
                            tracing::info!(machine = %from, sessions, "the conversation store was restored from a mirror's copy");
                            Self::list_away(&h).await;
                            h.harness.restore.set(Restore::Done { from, sessions, at });
                        }
                        Err(e) => h.harness.restore.set(Restore::Failed { from, error: format!("{e:#}"), at }),
                    }
                });
            }
            _ => {}
        }
    }

    /// Runner `machine` keeping a store connected: asked for its copy when the store was found
    /// empty at start and it is one of the server's mirrors.
    pub fn restore_due(&self, hub: &Arc<Hub>, machine: &str) {
        let due = self.restore_auto.load().iter().any(|m| m == machine);
        if due
            && matches!(self.restore_view(), Restore::Idle)
            && let Err(e) = self.restore_from(hub, machine)
        {
            tracing::warn!(%machine, error = %format!("{e:#}"), "couldn't ask a mirror for its copy of the conversation store");
        }
    }

    fn save_names(&self) {
        let names = self.names.get();
        if let Err(e) = self.keep_json("names.json", &*names) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' names");
        }
    }

    /// `value` as `file` in the registry's folder, whole or not at all: written off the calling
    /// thread, soon after, in the order kept (goal 3, plan 01a1a59c2a8e step 5: `crate::keep`).
    /// Written here, renaming an agent or raising its epoch waited on the disk, on a shard.
    fn keep_json(&self, file: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
        crate::keep::later(self.dir.join(file), serde_json::to_vec(value)?);
        Ok(())
    }

    /// Whether kiln keeps a session going when it stops part-way (the "keep_going" control, from
    /// any device): kept here, so it holds across restarts whatever its machine's runner knows.
    async fn set_keep_going(&self, hub: &Hub, id: &str, on: bool) {
        let off: Vec<String> = self.keep_off.update(|g| {
            if on {
                g.remove(id);
            } else {
                g.insert(id.to_owned());
            }
            g.iter().cloned().collect()
        });
        if let Err(e) = self.keep_json("keep-off.json", &off) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep which agents kiln doesn't keep going");
        }
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    Arc::make_mut(&mut m.info).keep_off = !on;
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        let Some(info) = info else { return };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// The plans sessions proposed (plan mode) since the last look, by session, each its latest.
    pub fn take_proposed(&self) -> Vec<(String, String)> {
        // Every tick of the goals' driver: a copy swapped in only when there's one to take.
        if self.proposed.load().is_empty() {
            return Vec::new();
        }
        self.proposed.update(std::mem::take).into_iter().collect()
    }

    /// What each agent's goals wait on a person for (`goals::held_stacks`, every tick of the goals'
    /// driver): each one that changes told to every viewer.
    pub async fn set_held(&self, hub: &Hub, held: &HashMap<String, Held>) {
        let held = Arc::new(held.clone());
        let now = Arc::clone(&held);
        // A session at a time, as its owner's turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let changed: Vec<Info> = self
            .sessions
            .on_each_session(
                move |id, m| m.info.held.as_ref() != now.get(id),
                move |s, id| {
                    let m = s.get_mut(id)?;
                    Arc::make_mut(&mut m.info).held = held.get(id).cloned();
                    m.commit(|_| None);
                    Some(Info::clone(&m.info))
                },
            )
            .await;
        for info in changed {
            super::attention::observe(hub, &info);
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What of session `id`'s messages couldn't be read (`crate::wire::unread`) changed: told to its
    /// viewers.
    pub async fn set_refused(&self, hub: &Hub, id: &str, refused: Option<crate::wire::unread::Kept>) {
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| match s.get_mut(&sid) {
                Some(m) if m.info.refused != refused => {
                    Arc::make_mut(&mut m.info).refused = refused;
                    m.commit(|_| None);
                    Some(Info::clone(&m.info))
                }
                _ => None,
            })
            .await
            .flatten();
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// A session is given a name here (the "rename" control from any device; empty gives it its
    /// harness's name back): its page and every list show it at once.
    async fn rename(&self, hub: &Hub, id: &str, value: Option<&Value>) {
        let name: String = value.and_then(Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
        self.names.update(|g| {
            if name.is_empty() {
                g.remove(id);
            } else {
                g.insert(id.to_owned(), name.clone());
            }
        });
        self.save_names();
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    let info = Arc::make_mut(&mut m.info);
                    info.name = if name.is_empty() { info.kind_name.clone() } else { name };
                    let named = json!(info.name);
                    if m.state.is_object() {
                        Arc::make_mut(&mut m.state).set("name", &named);
                        m.commit(|m| m.state_frame());
                    } else {
                        m.commit(|_| None);
                    }
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        let Some(info) = info else { return };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// Environment for one of the server's own terminals, so `kiln spawn` there works.
    pub fn terminal_env(&self, terminal: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![("KILN_TERM_SESSION", terminal.to_owned())];
        if let Some(sock) = self.local_sock.load().as_deref() {
            env.push(("KILN_HARNESS_SOCK", sock.to_owned()));
        }
        env
    }

    /// One of the server's own terminals started (its shell runs as `pid`).
    pub fn note_shell(&self, terminal: &str, pid: Option<u32>) {
        self.local_shells.update(|g| {
            if let Some(pid) = pid {
                g.insert(terminal.to_owned(), pid);
            }
            // Forget shells that are gone.
            if g.len() > 256 {
                g.retain(|_, p| crate::os::pid_alive(*p));
            }
        });
    }

    /// The server's own sessions' updates, into the mirrors (supervised; restartable).
    /// Sessions of machines that are away, as the server's store has them (the server started
    /// again since they went away): listed as away, so they can be moved on, until their machine
    /// says what it has.
    pub async fn restore_away(&self, hub: &Hub, away: Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>) {
        // Each copy is made here, not by its owner: a session's stored events are megabytes of
        // JSON to read, its text written out once for every snapshot and digest after. Those
        // already there when they're put in are left as they are.
        let mut made = Vec::new();
        for (u, events) in away {
            if self.sessions.contains(&u.id) {
                continue;
            }
            let Ok(mut info) = serde_json::from_str::<Info>(&u.info) else { continue };
            // The machine whose feed brought it, else the one its updates came from.
            let Some(machine) = u.machine.clone().or_else(|| info.target.strip_prefix("agent:").map(str::to_owned)) else { continue };
            info.target = format!("agent:{machine}");
            if hub.agents.load().get(&machine).is_some_and(|a| a.connected) {
                continue;
            }
            info.status = Status::Disconnected;
            let state: Value = u.state.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
            // Old stored list entries predate native subagent briefs. Their last task
            // snapshot still supplies those read-only children while the machine is away.
            if let Some(tasks) = state.get("tasks").and_then(|v| Vec::<super::model::Task>::deserialize(v).ok()) {
                info.subagents = SubagentBrief::of(&tasks);
            }
            let mut m = Mirror::new(info.clone(), Location::Machine(machine), Arc::clone(&self.bus));
            if let Some(read) = u.meta.as_deref().map(super::copies::Catalog::read) {
                match read {
                    Ok(c) => m.meta = Arc::new(c),
                    Err(e) => tracing::warn!(session = %u.id, error = %e, "an away session's stored meta doesn't read: its copy has none until its machine says it"),
                }
            }
            let mut state = StateDoc::of_value(&state);
            state.set("status", &json!("disconnected"));
            m.state = Arc::new(state);
            // A conversation cleared since: what came before it is history, not its timeline.
            let cleared = m.state.get::<u64>("cleared").unwrap_or(0);
            for e in events.into_iter().filter(|e| e.seq >= cleared) {
                if let Ok(v) = serde_json::from_str::<Value>(&e.body) {
                    let text = serde_json::to_string(&v).unwrap_or_default();
                    m.upsert(Arc::new(Entry::written(v, text)));
                }
            }
            made.push((u.id, m, info));
        }
        // To their owners, each a message of its own, so an owner's turns take as many as its quantum holds (goal 9, plan
        // 01a1a87b2cec step 3: a shard's were one job, every copy published in it). One there already stays as it is.
        let put = futures::future::join_all(made.into_iter().map(|(id, m, info)| {
            let at = id.clone();
            async move { self.sessions.on(&at, move |s| s.insert(id, m).then_some(info)).await.flatten() }
        }))
        .await;
        let infos: Vec<Info> = put.into_iter().flatten().collect();
        if !infos.is_empty() {
            tracing::info!(sessions = infos.len(), "agent sessions on machines that are away, from the conversation store");
        }
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// Lists the sessions of machines that are away, from the store (the last month's): after a
    /// start, and after a restore from a mirror's copy.
    ///
    /// The server lists them as it starts, before it takes in any runner's messages: a runner that
    /// comes back finds its sessions' copies here, and its streams send only what they lack
    /// (`ServerMsg::Kept`, docs/wire.md "Restarts"). A session that changed within the last day
    /// is rebuilt with as many events as a live copy keeps, so the copy its machine goes on from
    /// is the session's; an older one with its last few, to show.
    pub async fn list_away(hub: &Arc<Hub>) {
        let Some(st) = hub.harness.convo.clone() else { return };
        const AWAY_DAYS: i64 = 30;
        const AWAY_EVENTS: usize = 200;
        const RECENT: i64 = 24 * 3600 * 1000;
        let now = crate::util::now_ms();
        let since = now - AWAY_DAYS * 24 * 3600 * 1000;
        let read = crate::rt::blocking(move || -> anyhow::Result<Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>> {
            let recent: std::collections::HashSet<String> = st.unended(now - RECENT)?.into_iter().map(|u| u.id).collect();
            let mut out = Vec::new();
            for u in st.unended(since)? {
                let keep = if recent.contains(&u.id) { MAX_EVENTS } else { AWAY_EVENTS };
                let mut events = st.events(&u.id, None, keep)?;
                // Cut at `keep`: from where a run of the digests begins, as a live copy lets go of
                // its oldest (`crate::wire::sums::run_after`).
                if events.len() >= keep
                    && let Some(first) = events.first().map(|e| e.seq)
                {
                    let from = crate::wire::sums::run_after(first);
                    events.retain(|e| e.seq >= from);
                }
                out.push((u, events));
            }
            Ok(out)
        })
        .await;
        match read {
            Ok(Ok(away)) => hub.harness.restore_away(hub, away).await,
            Ok(Err(e)) | Err(e) => tracing::warn!(error = %format!("{e:#}"), "sessions of machines that are away couldn't be read from the conversation store"),
        }
    }

    pub async fn run_local(hub: Arc<Hub>) -> anyhow::Result<()> {
        // Once a process, however often this loop starts again (it's supervised).
        static RESTORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        // Sessions of machines that are away: listed from the store.
        Self::list_away(&hub).await;
        let rx = hub.harness.local_rx.clone();
        let offered = crate::rt::blocking(super::kinds::offered).await.unwrap_or_default();
        hub.harness.kinds.update(|k| k.insert("local".to_owned(), offered.clone()));
        if !RESTORED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            // Files finished messages mention, kept as they come.
            crate::rt::spawn(Keeper::run(hub.clone()));
            // The server's own sessions back, on the blocking pool: their record read, what
            // outlived them stopped (seconds each), the conversation store asked what it kept of
            // each (up to 10 s): none of it on the runtime's thread, which serves the listener.
            let (local, convo) = (hub.harness.local.clone(), hub.harness.convo.clone());
            if let Err(e) = crate::rt::blocking(move || local.restore_from(convo.as_ref())).await {
                tracing::warn!(error = %format!("{e:#}"), "the server's own agent sessions couldn't be restored");
            }
            // Each has a copy once this loop takes its Open (below): an attach to one waits for it (`super::returning`).
            hub.harness.returning.own(hub.harness.local.list().into_iter().map(|i| i.id));
            // The server's own sessions' harness records, followed into its store.
            if let Some(st) = &hub.harness.convo {
                let local = hub.harness.local.clone();
                if let Err(e) = crate::convo::record::Recorder::start(st.clone(), move || local.record_sources()) {
                    tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
                }
            }
            // What the server sends its mirrors, on the feed's lane.
            if let Some(out) = hub.harness.copies_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok((peer, json, data)) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data }) {
                            h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server answers runners' feeds, on the feed's lane.
            if let Some(out) = hub.harness.feed_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok(o) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json: o.json, data: Vec::new() }) {
                            h.net.send(o.peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server's own terminals and agents ask of kiln (`kiln spawn --on`, `kiln
            // agents wait`) is answered here.
            let forward = Some(crate::harness::ops::forward(Arc::clone(&hub)));
            match super::socket::serve_with(&hub.harness.local, super::socket::path("server", &hub.harness.dir), forward, false) {
                Ok(p) => hub.harness.local_sock.set(Some(p.to_string_lossy().into_owned())),
                Err(e) => tracing::info!(error = %format!("{e:#}"), "`kiln spawn` will not be available in the server's own terminals"),
            }
        }
        // The store's copies and the server's own sessions are listed: an attach waits no more for that.
        hub.harness.returning.listed();
        // The server's own sessions' updates, as fast as they come, each to its owner (waited for
        // only while its inbox is full): in bounded turns of the shard.
        let mut turns = crate::rt::Turns::new();
        loop {
            let Ok(said) = rx.recv().await else { return Ok(()) };
            if let super::session::Local::Said(id, Update::MailFlow { event }) = &said {
                if hub.harness.location(id) == Some(Location::Local)
                    && let Err(error) = super::mail_delivery::flow(&hub, id, Location::Local, (**event).clone()).await
                { tracing::warn!(session = %id, error = %error, "local communication receipt remains unacknowledged"); }
                turns.end().await;
                continue;
            }
            let lifecycle = match &said {
                super::session::Local::Said(id, Update::Open { .. } | Update::Closed) | super::session::Local::Gone(id) => Some(id.clone()),
                _ => None,
            };
            hub.harness.take_local(said).await;
            if let Some(id) = lifecycle {
                // W20-R01: restored, moved or closed local sessions get their current skill scope.
                hub.harness.applied(&id).await;
                hub.harness.mail_delivery.changed();
                let h = Arc::clone(&hub);
                crate::rt::spawn(async move { crate::api::boxed(|| crate::skills::sync(&h)).await });
            }
            turns.end().await;
        }
    }

    /// What one of the server's own sessions handed on ([`Registry::run_local`]): an update, to its copy's owner, or that its
    /// task is gone, after which its owner holds nothing for it (HARNESS-4). Each goes to that owner after what was handed on
    /// before it, so the owner takes a task's end after all it said.
    pub(crate) async fn take_local(&self, said: super::session::Local) {
        match said {
            super::session::Local::Said(id, u) => self.ingest_from(&Location::Local, &id, u, false).await,
            super::session::Local::Gone(id) => {
                let of = id.clone();
                if !self.sessions.send(&id, Msg::Run(Box::new(move |s: &mut Shard| s.task_gone(&of)))).await {
                    tracing::debug!(session = %id, "an agent session's end came as the server's copies went");
                }
            }
        }
    }

    /// Session `id` and its ancestors, nearest first ([`super::lineage::LINE_MAX`] at most), each its
    /// id and parent alone: what [`super::lineage::descends`] reads of it, without a copy of every
    /// session.
    pub fn lineage_of(&self, id: &str) -> Vec<Info> {
        let mut out = Vec::new();
        let mut at = id.to_owned();
        while out.len() < super::lineage::LINE_MAX {
            let Some((sid, parent)) = self.sessions.with(&at, |v| (v.info.id.clone(), v.info.parent.clone())) else { break };
            out.push(Info { id: sid, parent: parent.clone(), ..Info::default() });
            match parent {
                Some(p) => at = p,
                None => break,
            }
        }
        out
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = Vec::new();
        self.sessions.each(|_, view| v.push(Info::clone(&view.info)));
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// [`Registry::list`], each info as its owner shares it with its view, none copied (goal 9, plan 01a1a87b2cec
    /// step 5: `agents.list` copied every session's whole info to read a few fields of each).
    pub fn list_shared(&self) -> Vec<Arc<Info>> {
        let mut v: Vec<Arc<Info>> = Vec::new();
        self.sessions.each(|_, view| v.push(Arc::clone(&view.info)));
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// The ids of the sessions whose target is `target` (`agent:<machine>`), in no order, none of their infos copied:
    /// what a machine's runner processes may hold (goal 9, plan 01a1a87b2cec step 3: `crate::hub::Hub::runner_processes`
    /// and `check_retire` copied every session's whole info, [`Registry::list`], to find them, the latter for each
    /// machine with an older process).
    pub fn ids_on(&self, target: &str) -> Vec<String> {
        let mut v = Vec::new();
        self.sessions.each(|id, view| {
            if view.info.target == target {
                v.push(id.to_owned());
            }
        });
        v
    }

    /// [`Registry::list`] as `GET /api/harness/sessions` answers it, in its order: each session's
    /// info as its owner encoded it once ([`super::copies::View::info_json`]), joined into a JSON
    /// array once for every read that finds the sessions as they were, and answered as it is (goal 10,
    /// plan 01a1a87b2dae steps 2 and 5: `super::copies::Copies::list_json`).
    pub fn list_json(&self) -> bytes::Bytes {
        self.sessions.list_json()
    }

    /// [`Registry::list_json`] and its weak ETag, named once for every read that finds the list as it was
    /// (`super::copies::Copies::list_tagged`; goal 9, plan 01a1a87b2cec step 5).
    pub fn list_tagged(&self) -> (bytes::Bytes, Option<http::HeaderValue>) {
        self.sessions.list_tagged()
    }

    /// The shard whose owner holds session `id`'s copy and publishes its frames (`super::copies::owner_of`), among the
    /// runtime's as `crate::rt::spawn_owner` takes them (goal 10, plan 01a1a87b2dae step 5).
    pub fn owner_shard(&self, id: &str) -> usize {
        self.sessions.owner_of(id)
    }

    /// How many times the sessions' list was joined ([`Registry::list_tagged`]), so a read can tell its own join's time
    /// from a find (goal 10, plan 01a1a87b2dae step 5).
    pub fn list_joins(&self) -> u64 {
        self.sessions.list_joins()
    }

    /// Session `id`'s info as `GET /api/harness/sessions/{id}` answers it, as its owner encoded it
    /// ([`super::copies::View::info_json`]). None when there's no such session.
    pub fn info_json(&self, id: &str) -> Option<bytes::Bytes> {
        self.sessions.with(id, |v| v.info_json.clone())
    }

    /// Session `id`'s newest `limit` events before number `before`, as its copy holds them
    /// ([`super::copies::View::events_json`]). None when there's no such session or its copy can't
    /// say all of them.
    pub fn events_json(&self, id: &str, before: Option<u64>, limit: usize) -> Option<bytes::Bytes> {
        self.sessions.with(id, |v| v.events_json(before, limit)).flatten()
    }

    /// [`Registry::events_json`] in the pieces it's made of, for an answer that sends them as they are
    /// ([`super::copies::View::events_page`]; goal 10, plan 01a1a87b2dae step 5).
    pub fn events_page(&self, id: &str, before: Option<u64>, limit: usize) -> Option<Vec<bytes::Bytes>> {
        self.sessions.with(id, |v| v.events_page(before, limit)).flatten()
    }

    /// [`Registry::events_page`] of what's in memory as it is, inflating nothing: what a read on a shard answers with
    /// ([`super::copies::View::events_page_kept`]; goal 10, plan 01a1a87b2dae step 5). No such session: `Cannot`.
    pub fn events_page_kept(&self, id: &str, before: Option<u64>, limit: usize) -> Result<Vec<bytes::Bytes>, super::copies::Unread> {
        self.sessions.with(id, |v| v.events_page_kept(before, limit)).unwrap_or(Err(super::copies::Unread::Cannot))
    }

    /// Session `id`'s newest view, held: for what's read of it off the shards (a page whose compact chunk's texts
    /// are made there: goal 10, plan 01a1a87b2dae step 5).
    pub fn view_held(&self, id: &str) -> Option<Arc<super::copies::View>> {
        self.sessions.view(id)
    }

    pub fn kinds(&self) -> HashMap<String, Vec<KindInfo>> {
        #[cfg(not(test))]
        super::acp::warm_models();
        let mut all: HashMap<String, Vec<KindInfo>> = HashMap::clone(&self.kinds.load());
        // The server's own harnesses: the models its sessions listed since it started.
        for k in all.get_mut("local").into_iter().flatten() {
            let m = super::kinds::models_of(&k.id);
            if !m.is_empty() {
                k.models = m;
            }
            if k.id == "devin" && let Some(kind) = super::kinds::Kind::get(&k.id) {
                k.efforts = kind.efforts();
            }
        }
        all
    }

    pub fn lacks(&self, target: &str, kind: &str) -> bool {
        #[cfg(test)]
        if self.test_program.lock().is_some() {
            return false;
        }
        self.kinds.load().get(target).is_some_and(|ks| !ks.iter().any(|k| k.id == kind && k.available))
    }

    #[cfg(test)]
    pub fn set_kinds(&self, target: &str, kinds: Vec<KindInfo>) {
        self.kinds.update(|k| k.insert(target.to_owned(), kinds.clone()));
    }

    /// A session's meta as last reported (its harness's catalog: slash commands, models, modes).
    pub fn meta(&self, id: &str) -> Option<Value> {
        // Copied from the view held, not from where it's read.
        let meta = self.sessions.with(id, |v| Arc::clone(&v.meta))?;
        Some(meta.value().clone())
    }

    pub fn location(&self, id: &str) -> Option<Location> {
        self.sessions.with(id, |v| Location::clone(&v.location))
    }

    /// A task's output, read on the session's machine (`None`: no such session).
    pub async fn task_output(&self, hub: &Hub, id: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<Value>> {
        match self.location(id) {
            None => Ok(None),
            Some(Location::Local) => {
                let Some(s) = self.local.get(id) else { return Ok(None) };
                let task = task.to_owned();
                Ok(Some(crate::rt::blocking(move || s.task_output(&task, since)).await?))
            }
            Some(Location::Machine(m)) => hub.harness_task_output(&m, id, task, since).await,
        }
    }

    /// A machine's past sessions, newest first, as JSON (`None`: that machine does not look, e.g.
    /// an ephemeral runner). The server's own skip what its sessions are running.
    pub async fn history(&self, hub: &Hub, target: &str, kind: Option<String>, limit: usize) -> anyhow::Result<Option<Value>> {
        match Location::of_target(target) {
            Location::Local => {
                let running: std::collections::HashSet<String> = self.local.list().into_iter().filter_map(|i| i.harness_session).collect();
                let scanner = Arc::clone(&self.scanner);
                let past = crate::rt::blocking(move || {
                    let mut g = scanner.lock();
                    g.get_or_insert_with(|| crate::aisessions::Scanner::new(crate::os::home_dir())).history(kind.as_deref(), limit, &running)
                })
                .await?;
                Ok(Some(serde_json::to_value(past)?))
            }
            Location::Machine(m) => hub.harness_history(&m, kind, u32::try_from(limit).unwrap_or(u32::MAX)).await?.map(serde_json::to_value).transpose().map_err(Into::into),
        }
    }

    /// Apply one update from a machine and pass it on to the session's viewers: back once the
    /// session's owner has (`super::copies`), so what reads the copy next finds it. The tests' way in.
    #[cfg(test)]
    pub(crate) async fn ingest(&self, from: &Location, id: &str, u: Update) {
        self.ingest_from(from, id, u, false).await;
        self.sessions.applied(id).await;
    }

    /// Machine `machine` is where session `id` runs (or the server doesn't know the session yet):
    /// what it says of it counts.
    pub fn runs_on(&self, id: &str, machine: &str) -> bool {
        self.sessions.with(id, |v| matches!(&*v.location, Location::Machine(at) if at == machine)).unwrap_or(true)
    }

    /// Session `id` is one kiln knows, and it runs on `machine`.
    pub fn located_on(&self, id: &str, machine: &str) -> bool {
        self.sessions.with(id, |v| matches!(&*v.location, Location::Machine(at) if at == machine)).unwrap_or(false)
    }

    /// A copy of session `id` at `epoch` is from before it went on elsewhere: its machine lets go
    /// of it.
    pub fn stale(&self, id: &str, epoch: u64) -> bool {
        self.epochs.load().get(id).is_some_and(|e| *e > epoch)
    }

    /// The sessions runner `machine` reports holding (`inv`) whose copy there is one left behind:
    /// the session runs elsewhere now, at a later epoch than that copy (docs/ci.md W27-R08). One
    /// just arriving there (its Open on its way) is at the epoch it goes on at, which isn't; a
    /// runner from before says no epochs, so none of its are.
    pub fn left_behind(&self, machine: &str, inv: &super::host::Inventory) -> Vec<String> {
        inv.sessions.iter().filter(|id| !self.runs_on(id, machine) && inv.epochs.get(*id).is_some_and(|e| self.stale(id, *e))).cloned().collect()
    }

    /// Session `id` is at `epoch` now (epochs only rise), kept when it changes.
    pub(crate) fn raise_epoch(&self, id: &str, epoch: u64) {
        // Read off the epochs' copy; a new one swapped in only when this one rises.
        if self.epochs.load().get(id).is_some_and(|e| *e >= epoch) {
            return;
        }
        let raised = self.epochs.update(|g| {
            let e = g.entry(id.to_owned()).or_insert(0);
            let raised = *e < epoch;
            *e = (*e).max(epoch);
            raised
        });
        if !raised {
            return;
        }
        let snapshot = self.epochs.get();
        if let Err(e) = self.keep_json("epochs.json", &*snapshot) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' epochs");
        }
    }

    /// An update of session `id` from `from`, handed to the shard that owns its copy
    /// (`super::copies`), which prepares and applies it after whatever was handed it before
    /// ([`Registry::take_from`]), as it takes a runner's ([`Registry::from_runner`]): back once it's
    /// handed over, waiting only while that owner's inbox is full. `shipped`: its machine keeps it in a
    /// store it sends the server's (`crate::convo::feed`), which keeps it from that, not from here. An
    /// inbox's answer changes nothing of the copy: whoever waits for it is told here, at once. Goal 4,
    /// plan 01a1a59c2a9c step 2; the server's own sessions' updates were prepared here, on the caller's
    /// shard, and handed over prepared, until 2026-11-06 (the legacy audit: one path, the owner's).
    pub async fn ingest_from(&self, from: &Location, id: &str, u: Update, shipped: bool) {
        let u = match u {
            Update::Inbox { said } => {
                self.answered(id, *said);
                return;
            }
            other => other,
        };
        self.room_for_rows(shipped).await;
        self.sessions.packed_within().await;
        let take = super::copies::Take { from: from.clone(), id: id.to_owned(), u, shipped };
        if !self.sessions.send(id, Msg::Take(Box::new(take))).await {
            tracing::debug!(session = id, "an agent session's update came as the server's copies went");
        }
    }

    /// An update from a session's runner, handed to the shard that owns its copy to judge and take
    /// there (`crate::hub::Hub::runner_update`; goal 10, plan 01a1a87b2dae step 4): back once that
    /// owner's inbox has it, waiting only while it's full, or while the copies' texts waiting for the packer are past
    /// their share (`super::copies::Copies::packed_within`; join-burst, item 2), as [`Registry::ingest_from`] does.
    pub async fn from_runner(&self, r: super::copies::RunnerUpdate) {
        let owner = self.sessions.owner_of(&r.session);
        self.room_for_rows(r.shipped).await;
        // Its owner found before it's handed over, so its id isn't copied for that (goal 9, plan 01a1a87b2cec step 3); it waits
        // while the packer is behind (the join burst's bound), so QUIC holds its runner rather than the server its text.
        self.sessions.packed_within().await;
        if !self.sessions.send_to(owner, Msg::Runner(Box::new(r))).await {
            tracing::debug!(owner, "an agent session's update came as the server's copies went");
        }
    }

    /// Waits, before an update that makes rows (one whose machine doesn't keep it in a store it ships, `shipped`) is
    /// handed to its owner, while the conversation store holds more of them than the owners' inboxes may hold
    /// together (`super::copies::Copies::inboxes_bound`): the owner never waits for the store itself (goal 10, plan
    /// 01a1a87b2dae step 5; `crate::convo::store::Store::room_within`), so what hands it updates does, past that.
    async fn room_for_rows(&self, shipped: bool) {
        if !shipped && let Some(store) = &self.convo {
            store.room_within(self.sessions.inboxes_bound()).await;
        }
    }

    /// An update of session `id` from `from`, taken by the shard that owns its copy, `s` (goal 10, plan
    /// 01a1a87b2dae step 4: a runner's update is judged and taken there, not on the hub's loop): what's
    /// left of it when the owner's turn ran out (`over`, [`Registry::apply`]; goal 9), to take first next.
    #[allow(clippy::too_many_arguments)]
    pub fn take_from(&self, s: &mut Shard, hub: &Arc<Hub>, from: &Location, id: &str, u: Update, shipped: bool, over: &dyn Fn(u64) -> bool) -> Option<Ingest> {
        let ingest = self.prepare(hub, from, id, u, shipped)?;
        self.apply(s, hub, ingest, over)
    }

    /// An update of session `id` from `from` as its copy takes it ([`Registry::apply`]): an inbox's
    /// answer is told whoever waits for it, and leaves nothing to take; an Open takes where it runs
    /// and the name given here, and raises its epoch.
    fn prepare(&self, hub: &Hub, from: &Location, id: &str, u: Update, shipped: bool) -> Option<Ingest> {
        // What became of a message: for whoever waits for it, wherever it was taken or refused.
        let u = match u {
            Update::MailFlow { .. } => return None,
            Update::Inbox { said } => {
                self.answered(id, *said);
                return None;
            }
            other => other,
        };
        // Where it runs, and the name given here, are the server's to say: to the copy and to
        // viewers alike (a session announced again — a switch of harness, a reconnect — keeps its
        // machine and its name). The machine's own name is kept aside: it counts once it changes it.
        let mut reported = String::new();
        let u = match u {
            Update::Open { mut info, since } => {
                // The wire announcement omits the local queue index. Until state arrives,
                // absence from it cannot establish that a queued message is gone.
                info.queue_partial |= info.queue.len() != info.queued;
                info.target = from.target();
                own_name(&info.name, &info.kind_name).unwrap_or_default().clone_into(&mut reported);
                if let Some(n) = self.names.load().get(id) {
                    info.name.clone_from(n);
                }
                info.keep_off = self.keep_off.load().contains(id);
                Update::Open { info, since }
            }
            other => other,
        };
        if let Update::Open { info, .. } = &u
            && info.epoch > 0
        {
            // Gone on at a later epoch on a runner from before signed hand-overs: unsigned,
            // recorded saying so (docs/ci.md W27-R08).
            if let Location::Machine(machine) = from
                && info.epoch > self.epoch_of(id)
                && hub.runner_wide_caps(machine) & crate::proto::CAP_CUSTODY == 0
            {
                let why = format!("the process it went to, on {}, runs a kiln from before signed hand-overs: it signed no receipt", hub.shown(machine));
                unsigned(hub, id, info.epoch, &format!("runner {machine}"), why);
            }
            self.raise_epoch(id, info.epoch);
        }
        Some(Ingest { from: from.clone(), id: id.to_owned(), u, shipped, reported, came: super::live::came() })
    }

    /// An update of a session from the machine it runs on, as the shard that owns the session's copy
    /// takes it (`super::copies`): the copy changes and its view goes out, its viewers are sent what
    /// changed, then what the change tells the rest of kiln happens. Goal 4, plan 01a1a59c2a9c
    /// step 2: only this shard writes the copy.
    ///
    /// An update's events are taken while its owner's turn lasts (`over`, asked after each with what the next
    /// is expected to take to write out: whether the next and what ends the update would pass its step
    /// quantum), one at the least, and the copy goes out with those: the rest come back, for the owner to
    /// take first at its next turn, before anything newer (goal 9, plan 01a1a87b2cec step 3: a runner's whole
    /// history in one update held its owner's shard up to 83 ms). One long to write out (a large tool output)
    /// is written by its owner off the shard first (`super::copies::Shard::want_write`).
    pub(super) fn apply(&self, s: &mut Shard, hub: &Arc<Hub>, i: Ingest, over: &dyn Fn(u64) -> bool) -> Option<Ingest> {
        let Ingest { from, id, mut u, shipped, reported, came } = i;
        let id = id.as_str();
        // The frames it makes carry where it came from, at whichever turn it's taken (goal 10, plan 01a1a87b2dae step 5).
        let _came = super::live::carrying(came);
        // Only the machine a session runs on speaks for it: a copy left behind where it went on
        // from says nothing more of it.
        if !matches!(u, Update::Open { .. }) && s.get(id).is_some_and(|m| *m.location != from) {
            return None;
        }
        // A changeset's patch is stored as the whole changeset it makes, once taken in (below); an
        // update's events as they're taken, each written out once for the copy and its row (below).
        let store = self.convo.as_ref().filter(|_| !shipped);
        // A changeset, a state or a meta long to write out (a changeset's files each a JSON tree, then that written out: a
        // runner's up to 512 KiB of hunks; a state compared with its last and written field by field, a meta made a value; and
        // the store's row of each: milliseconds on the shard for a large one): written out off the shard first, nothing of it
        // taken yet, then taken as written there; one its owner tried is taken either way (goal 9, plan 01a1a87b2cec step 3:
        // `super::copies::Shard::want_off`).
        //
        // The same state or meta again as the copy's was made of (a harness says its meta at each turn) has nothing to make, so
        // nothing long to: told apart first, on the shard (a compare), and only one that changed is judged by what its kind takes
        // (`super::copies::Mirror::said`), so a session's first long one goes off the shard too (step 3).
        let same = s.get(id).is_some_and(|m| m.same_again(&u));
        // A meta that changed, one of its harness's catalogs on this owner saying it already (another of its sessions said it):
        // taken as it is, nothing made (goal 9, plan 01a1a87b2cec step 3; goal 11).
        let shared = if same { None } else { s.catalog_for(id, &u) };
        // What every session's states and metas taught their owners, for one's first.
        let said = s.said();
        let tried = s.take_off_written(id);
        if tried.is_none() && !same && shared.is_none() && s.get(id).is_some_and(|m| super::copies::long_off(m, &u, &said)) {
            s.want_off(id, store.is_some());
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported, came });
        }
        // An Open going on from an event inside a compact chunk of its copy (a runner's reconnect after it lost events the copy
        // has): that chunk is read back off the shard first, as one an update changes is, nothing of the Open taken yet (goal 9,
        // plan 01a1a87b2cec step 3: `super::copies::Mirror::keep_through` inflated it and parsed its events on the shard).
        if let Update::Open { info, since: Some(n) } = &u
            && let Some(c) = s.get(id).filter(|m| m.info.epoch == info.epoch).and_then(|m| m.thaw_for_open(*n)).filter(|c| !std::ptr::eq(s.thawed().as_ptr(), Arc::as_ptr(c)))
        {
            s.want_thaw(id, c);
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported, came });
        }
        let (mut written_off, took_off) = match tried {
            Some(w) => {
                let took = w.made.is_some().then_some(w.took);
                (w.made, took)
            }
            None => (None, None),
        };
        // Its row's writing, timed: a state's and a meta's part of what writing them out takes (learned below). What's learned
        // here is timed on the CPU where the shard's thread says, as a turn's hold is (`super::copies::Timer`; goal 9, plan
        // 01a1a87b2cec step 3).
        let row_began = super::copies::Timer::start();
        if let Some(store) = store.filter(|_| !matches!(u, Update::ChangesPatch { .. } | Update::Events { .. })) {
            match written_off.as_mut() {
                Some(made) => store.put(made.take_rows()),
                None => store.send(id, &u),
            }
        }
        let row_took = row_began.took();
        // The event of this update its owner wrote out off the shard, or tried, for that event as it's taken (below; goal 9,
        // plan 01a1a87b2cec step 3).
        let mut written = s.take_written(id);
        // What the copy takes: each event's JSON and its text, written out once for the copy, its
        // live frame, its digest and every snapshot after (`Entry::of_event`, as they're taken, below);
        // a state's and a meta's JSON.
        let mut entries: Vec<Arc<Entry>> = Vec::new();
        // The events' rows for the conversation store, of the same texts.
        let mut rows: Vec<crate::convo::rows::Row> = Vec::new();
        // How many of an update's events its turn took.
        let mut taken = 0;
        // Its info changed as its pages see it: the events socket is told, its frame made of the bytes its
        // view encoded the info as (goal 10, plan 01a1a87b2dae step 5: it was encoded again for the socket).
        let mut changed = false;
        let mut opened_lifecycle = false;
        // What of its info attention reads, before this update (`super::attention::signature`).
        let attention_before = s.get(id).map(|m| super::attention::signature(&m.info));
        // The same meta again (a harness says it at each turn): nothing to keep, encode or send.
        let mut meta_same = false;
        let mut gone = false;
        // The copy's events went another way than its viewers have them: they start over from it.
        let mut resync = false;
        // Finished messages' mentions of files, to keep once the copy is out.
        let mut jobs: Vec<Job> = Vec::new();
        // The plan it proposed (plan mode) in these events, to keep as one of kiln's plans.
        let mut proposed: Option<String> = None;
        let mut save_names = false;
        // The account's other sessions, which pause too (`hold_account`).
        let mut hold: Option<(Command, Vec<(String, Location)>)> = None;
        // Its account, and the window reached and when that starts over.
        let mut hold_for: Option<(String, Option<String>, Option<i64>)> = None;
        // The whole changeset a patch makes, for the store.
        let mut stored_changes: Option<Files> = None;
        // An Open taken: its copy, published once it's taken, is what an attach may wait for as the server starts.
        let opened = matches!(u, Update::Open { .. });
        // A session held closed (its close came before its announcement) gets no copy again (HARNESS-4); what the store keeps
        // of it is kept as before.
        let held = s.closed_open(id, &from, &u);
        if let Update::Open { info, since } = &u
            && !held
        {
            let old = s.get(id);
            opened_lifecycle = old.is_some_and(|m| m.info.status != info.status);
            // A runner that started again goes on from what this copy has of the same epoch:
            // its events through `since` stay; what's after them comes again.
            let keep = since.filter(|_| old.is_some_and(|m| m.info.epoch == info.epoch));
            if let Location::Machine(machine) = &from {
                match keep {
                    Some(n) => tracing::info!(session = id, %machine, after = n, "an agent session's copy goes on from its machine's Open after event"),
                    None => tracing::info!(session = id, %machine, "an agent session's copy starts from its machine's Open"),
                }
            }
            let mut info = (**info).clone();
            // What its goals wait on a person for is the server's to say (`goals::held_stacks`): kept.
            info.held = old.and_then(|m| m.info.held.clone());
            // So is what of its messages couldn't be read (`crate::wire::unread`).
            info.refused = old.and_then(|m| m.info.refused.clone());
            // So is whether kiln started it as its parent's subagent (`Spec::subagent`): a runner
            // from before the word doesn't say it, and it stays one.
            info.subagent |= old.is_some_and(|m| m.info.subagent);
            // What kiln's own words were handed is kept too, and what waits in its queue until
            // the state after this says again (neither goes in an Open): a runner that comes
            // back doesn't make a word handed look lost (CP-T3.2, the fourteenth adversarial check's).
            if let Some(old) = old {
                info.said.clone_from(&old.info.said);
                info.queue.clone_from(&old.info.queue);
            }
            let bus = Arc::clone(s.bus());
            let m = s.entry(id, || Mirror::new(info.clone(), from.clone(), bus));
            // Its machine announced it: what it reports from now on counts.
            m.opened_at = None;
            m.info = Arc::new(info);
            // W20-R01: the owner's Open is newer than the disconnected overlay left on its
            // cached State. Reconcile lifecycle without inventing a State or replacing details.
            if m.state.is_object() && m.state.get::<Status>("status") != Some(m.info.status) {
                opened_lifecycle = true;
                Arc::make_mut(&mut m.state).set("status", &json!(m.info.status));
            }
            m.reported = reported;
            m.location = Arc::new(from.clone());
            match keep {
                Some(n) => {
                    let had = m.events.len();
                    // Its chunks through `n` kept as they are, none read again (goal 9, plan 01a1a87b2cec step 3).
                    m.keep_through(n);
                    // Past its machine's newest it had what the machine no longer has: its
                    // viewers let go of that too.
                    resync = m.events.len() != had;
                }
                None => {
                    m.clear_events();
                    m.started_over_pending = true;
                }
            }
            m.changes = None;
            m.changes_sent = None;
            changed = true;
        }
        // The chunk its owner last read back for a change, or tried: changed where it is, should it still be compact.
        let thawed = s.thawed();
        // The compact chunk this update's next event changes, to be read back off the shard first (goal 9, plan
        // 01a1a87b2cec step 3: `super::copies::Shard::want_thaw`).
        let mut thaw = None;
        // The event of this session its owner last put in among its copy's events off the shard, or tried: one still to be
        // put in is put in where it is. And this update's next event to be put in so: where it goes, and the copy's number
        // (`super::copies::Shard::want_insert`).
        let tried = s.tried_insert(id);
        let mut insert = None;
        // This update's next event long to write out, to be written so by its owner: where its session works, for the files
        // it mentions (`super::copies::Shard::want_write`). And whether this turn took one its owner wrote so: its update is
        // let go of off the shard, as the event's megabytes are freed with it (below).
        let mut write: Option<(String, Location)> = None;
        let mut took_written = false;
        let Some((m, catalogs)) = s.copy_and_catalogs(id) else {
            // Its events are stored all the same, as they were before a copy took them.
            if let Some(store) = store.filter(|_| matches!(u, Update::Events { .. })) {
                store.send(id, &u);
            }
            // A session this server does not know yet (it restarted): ask for all of it.
            if let Location::Machine(machine) = &from
                && !matches!(u, Update::Closed)
            {
                self.request_snapshot(hub, machine, id);
            }
            return None;
        };
        // Its first event changes one in a compact chunk: nothing of it is taken yet, all of it the rest, once that chunk is
        // read back off the shard (goal 9, plan 01a1a87b2cec step 3).
        if let Update::Events { events } = &u
            && let Some(c) = events.first().and_then(|e| m.thaw_for(&e.id, e.seq)).filter(|c| !std::ptr::eq(thawed.as_ptr(), Arc::as_ptr(c)))
        {
            s.want_thaw(id, c);
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported: String::new(), came });
        }
        // A stream into an event of an older compact chunk (a long tool's output, a subagent's text): the same, once that
        // chunk is read back off the shard, before the stream is taken (goal 9, plan 01a1a87b2cec step 3).
        if let Update::Stream { id: event, .. } = &u
            && let Some(c) = m.thaw_for_stream(event).filter(|c| !std::ptr::eq(thawed.as_ptr(), Arc::as_ptr(c)))
        {
            s.want_thaw(id, c);
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported: String::new(), came });
        }
        // Its first event goes in under an older number than the copy's newest: the same, once the copy's events are made
        // again with it in off the shard (goal 9, plan 01a1a87b2cec step 3).
        if let Update::Events { events } = &u
            && let Some((at, e)) = events.first().and_then(|e| m.insert_at(&e.id, e.seq).filter(|_| tried.as_deref() != Some(e.id.as_str())).map(|at| (at, e.clone())))
        {
            let rev = m.rev;
            s.want_insert(id, at, e, rev);
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported: String::new(), came });
        }
        // Its first event is long to write out (a large tool output) and wasn't written yet: the same, once its owner wrote it
        // out off the shard (goal 9, plan 01a1a87b2cec step 3: `super::copies::Shard::want_write`).
        if let Update::Events { events } = &u
            && events.first().is_some_and(|e| written.as_ref().is_none_or(|w| !w.is(id, e)) && super::copies::long_to_write(e, said.sides()))
        {
            let (cwd, location) = (m.info.cwd.clone(), Location::clone(&m.location));
            s.want_write(id, store.is_some(), cwd, location);
            return Some(Ingest { from, id: id.to_owned(), u, shipped, reported: String::new(), came });
        }
        m.rev = m.rev.saturating_add(1);
        // The meta its machine said (goal 9, plan 01a1a87b2cec step 3; goal 11): its copy's catalog already (the same again), else
        // one of its harness's on this owner, taken as it is; else made into one, by its owner off the shard or here (timed, so a
        // session's long ones go off it), and kept among its harness's.
        if let Update::Meta { meta } = &mut u {
            let made = match (written_off.take(), shared) {
                (Some(super::copies::OffMade::Meta { made, .. }), _) => {
                    m.long_to_free = true;
                    Some((made, took_off))
                }
                _ if same => {
                    meta_same = true;
                    None
                }
                (_, Some(c)) => {
                    m.meta = c;
                    None
                }
                _ => {
                    let began = super::copies::Timer::start();
                    let made = super::copies::Catalog::made(std::mem::take(meta));
                    Some((made, Some(row_took.saturating_add(began.took()))))
                }
            };
            match made {
                Some((Ok(made), took)) => {
                    m.meta = catalogs.keep(&m.location, &m.info.kind, made);
                    m.encodings.meta += 1;
                    if let Some(took) = took {
                        m.said_took(&u, took, &said);
                    }
                }
                Some((Err(e), _)) => {
                    tracing::warn!(session = id, error = %e, "a session's meta couldn't be made its catalog: its copy keeps the one it had");
                    meta_same = true;
                }
                None => {}
            }
        }
        match &u {
            // A whole snapshot follows (not one that goes on from what the copy has): its
            // viewers from before start over too.
            Update::Open { .. } => {
                if std::mem::take(&mut m.started_over_pending) {
                    m.started_over();
                }
            }
            // An inbox's answer went to whoever waits for it, before any of this.
            Update::Reply { .. } | Update::Inbox { .. } | Update::MailFlow { .. } => {}
            Update::Closed => {
                gone = true;
                // Its identities with it (docs/ci.md W27-R01).
                let (store, id) = (hub.store.clone(), id.to_owned());
                drop(crate::rt::spawn(async move {
                    if let Err(e) = crate::identities::agent_ended(&store, &id).await {
                        tracing::warn!(session = %id, error = %format!("{e:#}"), "an ended agent's identities weren't revoked");
                    }
                }));
            }
            Update::Reset { .. } => {
                m.clear_events();
                m.started_over();
            }
            Update::State { state } => {
                // The same state as the copy's was made of, the copy's still that one: not made again; another is
                // written once and kept by its fields, its keys the last one's (goal 9, plan 01a1a87b2cec step 5: a
                // whole state made into a value tree was 0.45% of the process in rt7's calm profile, its patch's field
                // compares 0.19%, the last tree let go 0.25%). As its owner wrote it out off the shard, or here, timed, so a
                // session's long states go off it (step 3).
                match written_off.take() {
                    Some(super::copies::OffMade::State { doc, .. }) => {
                        if let Some(doc) = doc {
                            m.state = Arc::new(doc);
                            m.long_to_free = true;
                        }
                        if let Some(took) = took_off {
                            m.said_took(&u, took, &said);
                        }
                    }
                    // Made, and timed, only where it changed: the same one again is a compare, and timed with the rest it taught
                    // that the session's states are short, so the next one that changed was written on the shard however long
                    // (step 3).
                    _ if !same => {
                        let began = super::copies::Timer::start();
                        m.state = Arc::new(StateDoc::made(&**state, &m.state));
                        m.said_took(&u, row_took.saturating_add(began.took()), &said);
                    }
                    _ => {}
                }
                // The info as it was, shared with the copy's views (not copied: its own is, as it changes).
                let before = Arc::clone(&m.info);
                let info = Arc::make_mut(&mut m.info);
                info.apply(state);
                info.last_activity = crate::util::now_ms();
                // Its name: one of its own the machine gives anew; else the one given here. Read off
                // the names' copy, a new one swapped in only as the machine's own is taken up.
                let names = self.names.load();
                match naming(&state.name, &m.reported, &info.kind_name, names.contains_key(id)) {
                    Naming::Adopt(own) => {
                        m.reported = own.to_owned();
                        self.names.update(|n| n.insert(id.to_owned(), own.to_owned()));
                        save_names = true;
                    }
                    Naming::Keep => {
                        if let Some(n) = names.get(id) {
                            info.name.clone_from(n);
                            // Only where the state doesn't say it already: the copy's own when just made (nothing
                            // copied), else the last one's, shared with its views, copied only to change.
                            if m.state.is_object() && m.state.get::<String>("name").as_deref() != Some(n.as_str()) {
                                Arc::make_mut(&mut m.state).set("name", &json!(n));
                            }
                        }
                    }
                    Naming::Theirs => {}
                }
                drop(names);
                if m.info.differs(&before) {
                    changed = true;
                }
                if before.pause.is_none()
                    && let Some(p) = m.info.pause.as_ref().filter(|p| p.reason == super::pause::LIMIT && p.by == "harness")
                    && let Some(account) = m.info.account.clone()
                {
                    let c = Command::LimitHold { window: p.window.clone(), label: p.label.clone(), until: p.until, said: p.said.clone(), account: Some(account.clone()) };
                    hold = Some((c, Vec::new()));
                    // Collected below, once this copy is out.
                    hold_for = Some((account, p.window.clone(), p.until));
                }
            }
            // Taken above.
            Update::Meta { .. } => {}
            // As its owner wrote it out off the shard, or here, timed, so what's long to write is learned (goal 9, plan
            // 01a1a87b2cec step 3).
            Update::Changes { files } => match written_off.take() {
                Some(super::copies::OffMade::Changes { set, .. }) => {
                    m.take_change_entries(set, Vec::new(), true);
                    m.long_to_free = true;
                }
                _ => {
                    let began = super::copies::Timer::start();
                    m.take_changes(files.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), Vec::new(), true);
                    super::copies::wrote_changes(&u, began.took());
                }
            },
            Update::ChangesPatch { set, gone } => {
                match written_off.take() {
                    Some(super::copies::OffMade::Changes { set: made, gone: made_gone, .. }) => {
                        m.take_change_entries(made, made_gone, false);
                        m.long_to_free = true;
                    }
                    _ => {
                        let began = super::copies::Timer::start();
                        m.take_changes(set.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), gone.iter().filter_map(|k| serde_json::to_value(k).ok()).collect(), false);
                        super::copies::wrote_changes(&u, began.took());
                    }
                }
                if self.convo.is_some() && !shipped {
                    stored_changes.clone_from(&m.changes);
                }
            }
            Update::Events { events } => {
                for e in events {
                    // One the copy has in a compact chunk: that chunk is read back off the shard before it's changed, and this
                    // event and those after it are the rest, its owner's next (as when its turn is spent, below).
                    if let Some(c) = m.thaw_for(&e.id, e.seq).filter(|c| !std::ptr::eq(thawed.as_ptr(), Arc::as_ptr(c))) {
                        thaw = Some(c);
                        break;
                    }
                    // One that goes in under an older number than the copy's newest: the copy's events are made again with
                    // it in off the shard, and this event and those after it are the rest, so (goal 9, plan 01a1a87b2cec
                    // step 3: every chunk was read again here).
                    if let Some(at) = m.insert_at(&e.id, e.seq).filter(|_| tried.as_deref() != Some(e.id.as_str())) {
                        insert = Some((at, e.clone(), m.rev));
                        break;
                    }
                    // As its owner wrote it out off the shard, or tried, where it was: else one after the first that's long to write
                    // out is written so, it and those after it the rest (goal 9, plan 01a1a87b2cec step 3: a large tool output's
                    // JSON and row, past the step's quantum on the shard). The first was judged so above, once.
                    let tried_write = written.take().filter(|w| w.is(id, e));
                    if tried_write.is_none() && taken > 0 && super::copies::long_to_write(e, said.sides()) {
                        write = Some((m.info.cwd.clone(), Location::clone(&m.location)));
                        break;
                    }
                    // Written out once, for the copy and for its row alike (goal 9, plan 01a1a87b2cec step 5), and timed, so what's
                    // long to write is learned (step 3).
                    let (entry, mentioned) = match tried_write.and_then(|w| w.made) {
                        Some(made) => {
                            took_written = true;
                            rows.extend(made.row);
                            (made.entry, made.mentions)
                        }
                        None => {
                            let began = super::copies::Timer::start();
                            let entry = Arc::new(if store.is_some() {
                                Entry::of_event(e, Some(&mut |text: String| rows.push(crate::convo::rows::Row::Event(Box::new(crate::convo::rows::event_written(id, e, text))))))
                            } else {
                                Entry::of_event(e, None)
                            });
                            // The files it mentions found with it, as one written out off the shard has them (`made_of`, whose time
                            // counts both), and timed with it: what an event's writing out is expected to take is what an owner's turn
                            // asks of its next event (step 3), and looking through the lab's replies' words for files took about as
                            // long on the CPU as writing them out (42 µs to 36 µs, g9-owners3's measure of the lab's mix in a test
                            // build), so its turns were judged by half of what their events took.
                            let mentioned = mentions(id, &m.info.cwd, &m.location, e);
                            super::copies::wrote(e, began.took());
                            (entry, mentioned)
                        }
                    };
                    if let Some(job) = mentioned {
                        jobs.push(job);
                    }
                    // kiln's own words handed to its harness, with what they said then: what a
                    // goal stack's word or a parent's news held when its agent took it.
                    if let super::model::Body::User { text, client_id: Some(c), .. } = &e.body
                        && super::queued::own(c)
                    {
                        super::queued::note_given(&mut Arc::make_mut(&mut m.info).said, c, super::queued::stable(text));
                    }
                    if let Some(plan) = proposed_in(e) {
                        proposed = Some(plan);
                    }
                    if !entry.is_null() {
                        resync |= m.upsert(Arc::clone(&entry));
                    }
                    entries.push(entry);
                    taken += 1;
                    // Its turn spent: the rest at the owner's next (below), one taken at the least. The next is expected to take
                    // at least what writing it out does.
                    if taken < events.len() && over(events.get(taken).map_or(0, super::copies::write_ns)) {
                        break;
                    }
                }
            }
            Update::Stream { id, text } => m.append(id, text),
        }
        // What of an update's events its turn had no time for (goal 9, plan 01a1a87b2cec step 3): the copy
        // goes out with those taken, and the rest come back to the owner.
        let rest = match &mut u {
            Update::Events { events } if taken < events.len() => Some(Update::Events { events: events.split_off(taken) }),
            _ => None,
        };
        if let Some(store) = store {
            store.put(std::mem::take(&mut rows));
        }
        // Its viewers start over from the copy as it is now (its events went another way than
        // they have them): the feed's resync marker sends each back to a snapshot, and none
        // resumes from before it.
        if resync {
            m.started_over();
        }
        // The copy goes out, then its viewers are sent what changed. A state goes out with the name
        // given here in it (the machine's may lack it). An Open that goes on from what the copy has
        // isn't one to its viewers: they go on with what they have, unless the copy's events went
        // another way than theirs. Its events' frame, where its owner wrote one of them out off the shard
        // (a large tool output's megabytes, which the frame copies in), is written off it too, before
        // anything after it (goal 9, plan 01a1a87b2cec step 3: `super::copies::Mirror::commit_events`).
        let later = if !resync && matches!(u, Update::Events { .. }) {
            m.commit_events(&entries, took_written)
        } else {
            m.commit(|m| match &u {
                _ if resync => Some(super::copies::payload_of(super::live::RESYNC, 0)),
                Update::Open { since: Some(_), .. } if m.state.is_object() => m.state_frame(),
                Update::Open { since: Some(_), .. } if opened_lifecycle => {
                    // A header can precede the first State. Existing resumed carries it without
                    // resetting the viewer's retained timeline or manufacturing default details.
                    let info = serde_json::to_value(&*m.info).unwrap_or(Value::Null);
                    let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::Resumed { info, born: Some(born()) }, rev: Some(m.rev) }.text();
                    Some(super::copies::payload_of(&text, m.rev))
                }
                Update::Open { since: Some(_), .. } => None,
                // A full Open starts the timeline over. Reattach to the coherent copy so an
                // older viewer cannot retain its disconnected State while new details wait.
                Update::Open { since: None, .. } if opened_lifecycle && m.state.is_object() => Some(super::copies::payload_of(super::live::RESYNC, 0)),
                Update::State { .. } => m.state_frame(),
                Update::Changes { .. } | Update::ChangesPatch { .. } => m.changes_frame(),
                Update::Meta { .. } if meta_same => None,
                _ => Some(m.frame_of(&u, &entries)),
            });
            None
        };
        // Attention plan 01a199e1b177752e84dad5dc990e6a15: what changed, for real turn timestamps (a refresh can't make
        // activity), observed below once the copy is let go of.
        let observed = (changed && attention_before != Some(super::attention::signature(&m.info))).then(|| Arc::clone(&m.info));
        // What the copy's state was made of, kept with the value as made (a name given here in it): the same one again finds
        // it so (above).
        match u {
            Update::State { state } => m.state_of = Some((state, Arc::clone(&m.state))),
            // Events among which one its owner wrote out off the shard (a large tool output): let go of off it too, the
            // event's megabytes freed there (goal 9, plan 01a1a87b2cec step 3).
            Update::Events { events } if took_written => super::copies::let_go(events),
            _ => {}
        }
        if gone {
            s.remove(id);
        }
        if let (Some((account, window, until)), Some((_, targets))) = (hold_for.as_ref(), hold.as_mut()) {
            let now = crate::util::now_ms();
            // From every shard's views (another shard's copies are read, never changed, here).
            s.each_view(|other, o| {
                // Not one whose own limits show the limit isn't its own: another account's (an
                // account id gone stale), another model's.
                if other != id
                    && o.info.account.as_deref() == Some(account.as_str())
                    && o.info.pause.is_none()
                    && o.info.status != Status::Exited
                    && super::pause::shares(&o.info.limits, &o.info.model, window.as_deref(), *until, now) != Some(false)
                {
                    targets.push((other.to_owned(), Location::clone(&o.location)));
                }
            });
        }
        // Attention plan 01a199e1b177752e84dad5dc990e6a15: extract real turn
        // timestamps after releasing the mirrors; refreshes can't create activity.
        if let Some(info) = observed {
            super::attention::observe(hub, &info);
        }
        // The whole changeset a patch made, kept in the store as it is when that write goes.
        if let (Some(files), Some(store)) = (stored_changes, self.convo.as_ref()) {
            self.store_changes(id, files, store.clone());
        }
        if let Some((c, targets)) = hold {
            self.hold_account(hub, &c, targets);
        }
        for job in jobs {
            self.keeper.enqueue(job);
        }
        if let Some(plan) = proposed {
            self.proposed.update(|p| p.insert(id.to_owned(), plan.clone()));
        }
        if gone {
            // Copies swapped in only for what they had of it.
            if self.names.load().contains_key(id) {
                save_names = self.names.update(|n| n.remove(id)).is_some() || save_names;
            }
            if self.changes_to_store.load().contains_key(id) {
                self.changes_to_store.update(|c| c.remove(id));
            }
        }
        if save_names {
            self.save_names();
        }
        if gone {
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.forget_holders(&[id.to_owned()]);
            self.opened.forget(id);
            self.asked.forget(id);
            super::archive::closed(hub, id);
            super::attention::forget(id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        } else if changed && let Some(m) = s.get_mut(id) {
            // Its new info's frame in the feed, its text held here; its last one's let go of only now, so a reader
            // passes over that one once this one's in (goal 9, plan 01a1a87b2cec step 5).
            let lent = hub.emit_harness(id, m.info_encoded());
            m.harness_at = lent.seq();
            m.harness = Some(lent);
        }
        if let Some(c) = thaw {
            s.want_thaw(id, c);
        }
        if let Some((at, e, rev)) = insert {
            s.want_insert(id, at, e, rev);
        }
        if let Some((cwd, location)) = write {
            s.want_write(id, store.is_some(), cwd, location);
        }
        if let Some(later) = later {
            s.want_frame(id, later);
        }
        // Its copy on its owner's board now, not as this turn ends: an attach waiting for the session as the server starts
        // finds it once told (`super::returning`; docs/wire.md, "Restarts").
        if opened {
            s.publish_board();
            self.returning.came();
        }
        rest.map(|u| Ingest { from, id: id.to_owned(), u, shipped, reported: String::new(), came })
    }

    /// A session reached its account's usage limit: the account's other sessions, on every machine,
    /// pause until it resets too, rather than each finding out by failing; each one whose own usage
    /// says the limit is its own as well (`pause::account_hold`).
    fn hold_account(&self, hub: &Hub, c: &Command, targets: Vec<(String, Location)>) {
        let Ok(json) = serde_json::to_string(c) else { return };
        for (id, loc) in targets {
            match loc {
                Location::Local => {
                    self.local.command(&id, c.clone());
                }
                Location::Machine(machine) => {
                    hub.command_soon(&machine, &ServerMsg::HarnessCmd { session: id, json: json.clone() }).log_when_answered(crate::hub::LOG_WITHIN);
                }
            }
        }
    }

    fn request_snapshot(&self, hub: &Hub, machine: &str, id: &str) {
        let msg = ServerMsg::HarnessCmd { session: id.to_owned(), json: json!({"type": "snapshot"}).to_string() };
        hub.command_soon(machine, &msg).log_when_answered(crate::hub::LOG_WITHIN);
    }

    /// A machine (re)connected and said what it runs: forget sessions it no longer has. Its
    /// runner process `instance` speaks for the sessions it holds: one another process of the
    /// machine holds ([`Registry::holder`]) stays, and while that process isn't among those
    /// connected (`present`), it is said to be away, as a machine's sessions are when it is.
    pub async fn inventory(&self, hub: &Hub, machine: &str, instance: Option<&str>, present: &[String], inv: Inventory) {
        let keep: std::collections::HashSet<String> = inv.sessions.into_iter().collect();
        // What it holds, for an attach waiting as the server starts (`super::returning`).
        self.returning.reported(machine, keep.iter().cloned());
        let (others, away): (std::collections::HashSet<String>, Vec<String>) = match instance {
            Some(me) => {
                let g = self.holders.load();
                let others: std::collections::HashSet<String> = g.iter().filter(|(_, h)| h.as_str() != me).map(|(id, _)| id.clone()).collect();
                let away = g.iter().filter(|(id, h)| h.as_str() != me && !present.contains(h) && !keep.contains(*id)).map(|(id, _)| id.clone()).collect();
                (others, away)
            }
            None => Default::default(),
        };
        let at = Location::Machine(machine.to_owned());
        let away: Vec<String> = away.into_iter().filter(|id| self.sessions.with(id, |v| *v.location == at && v.info.status != Status::Disconnected).unwrap_or(false)).collect();
        if !away.is_empty() {
            tracing::info!(%machine, sessions = away.len(), "agent sessions another runner process of this machine holds, which isn't connected: kept, away");
            self.sessions_lost(hub, &away).await;
        }
        let (keep, others) = (Arc::new(keep), Arc::new(others));
        // Not one this server opened there that the machine hasn't announced yet: the report was made before the open got
        // there. Its viewers are told, and its feed's topic goes with the last of them. A session at a time, as its owner's
        // turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let gone: Vec<String> = self
            .sessions
            .on_each_session(
                move |id, m| *m.location == at && !keep.contains(id) && !others.contains(id) && m.opened_at.is_none_or(|t| t.elapsed() > OPEN_GRACE),
                |s, id| s.remove(id).map(|_| id.to_owned()),
            )
            .await;
        let kinds = inv.kinds;
        self.kinds.update(|k| k.insert(format!("agent:{machine}"), kinds.clone()));
        self.forget_holders(&gone);
        for id in gone {
            super::archive::closed(hub, &id);
            super::attention::forget(&id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
    }

    /// Of the sessions a runner of `machine` just said it holds (`held`), those this server still
    /// has as disconnected. Its link dropped and came back, and only a session's own update clears
    /// that, so one that's quiet (idle, ended, waiting on a person) stays offline while its machine
    /// is connected. Their snapshots are asked for ([`Command::Snapshot`]).
    pub fn still_disconnected(&self, machine: &str, held: &[String]) -> Vec<String> {
        let at = Location::Machine(machine.to_owned());
        held.iter().filter(|id| self.sessions.with(id, |v| *v.location == at && v.info.status == Status::Disconnected).unwrap_or(false)).cloned().collect()
    }

    /// Session `id` is held by runner process `instance` of its machine, as that process said:
    /// kept (across restarts too) once it changes.
    pub fn held_by(&self, id: &str, instance: &str) {
        // Held so already, as nearly every update finds it: told by the holders' copy, with no copy of the id made to
        // ask (goal 9, plan 01a1a87b2cec step 5: one was made for each update its owner took).
        if self.holders.load().get(id).is_some_and(|h| h == instance) {
            return;
        }
        self.held_by_all(std::slice::from_ref(&id.to_owned()), instance);
    }

    /// [`Registry::held_by`] for each of `ids`: kept once for them all (a runner coming back says
    /// what it holds of every session at once).
    pub fn held_by_all(&self, ids: &[String], instance: &str) {
        // A session's updates say who holds it: read off the holders' copy, a new one swapped in
        // only when one of them changes hands.
        let held = self.holders.load();
        if ids.iter().all(|id| held.get(id).is_some_and(|h| h == instance)) {
            return;
        }
        drop(held);
        let changed = self.holders.update(|g| {
            let mut changed = false;
            for id in ids {
                if g.get(id).is_none_or(|h| h != instance) {
                    g.insert(id.clone(), instance.to_owned());
                    changed = true;
                }
            }
            changed
        });
        if changed {
            self.save_holders();
        }
    }

    /// The runner process of its machine that last said it holds session `id`, if one did.
    pub fn holder(&self, id: &str) -> Option<String> {
        self.holders.load().get(id).cloned()
    }

    fn forget_holders(&self, ids: &[String]) {
        // A copy swapped in only when one of them has a holder.
        if !ids.iter().any(|id| self.holders.load().contains_key(id)) {
            return;
        }
        let changed = self.holders.update(|g| {
            let mut changed = false;
            for id in ids {
                changed |= g.remove(id).is_some();
            }
            changed
        });
        if changed {
            self.save_holders();
        }
    }

    /// Keeps `holders.json`, on the blocking pool and as it is when the write goes: those that
    /// change while one is on its way go with it, or with the next (one a change starts once that
    /// one began). A runner coming back with every session it holds writes it once, never on the
    /// loop that takes its messages.
    fn save_holders(&self) {
        use std::sync::atomic::Ordering;
        // The change just swapped into the holders' copy, then the flag: past the fence a write on
        // its way either reads the change or is seen to have let go (the fence in `write` below).
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.holders_saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let (holders, saving, writing, dir) = (Arc::clone(&self.holders), Arc::clone(&self.holders_saving), Arc::clone(&self.holders_writing), self.dir.clone());
        let write = move || {
            // One at a time, each taking what's newest once it's its turn.
            let _one = writing.lock();
            // Let go first: a change after the copy below starts another write (after this one).
            saving.store(false, Ordering::Release);
            std::sync::atomic::fence(Ordering::SeqCst);
            let now = holders.get();
            if let Err(e) = keep_json_in(&dir, "holders.json", &*now) {
                tracing::warn!(error = %format!("{e:#}"), "couldn't keep which runner process holds each agent session");
            }
        };
        drop(crate::rt::spawn(async move {
            let _ = crate::rt::blocking(write).await;
        }));
    }

    /// Keeps session `id`'s changeset `files` in the conversation store as it is when its turn comes:
    /// written out on the compute pool (most of a megabyte of JSON for a long session's), kept by the
    /// store on the blocking pool (goal 4, plan 01a1a59c2a9c step 3). One writer at a time, at the pace
    /// the store commits: it writes each session's newest, waits for the store to have it, then takes
    /// what came meanwhile, a patch that comes while it works taking the place of the one before (each
    /// is the changeset whole). A burst of patches (a runner sending what it held) writes a session's
    /// changeset as often as the store takes it, not once a patch.
    fn store_changes(&self, id: &str, files: Files, store: crate::convo::store::Store) {
        use std::sync::atomic::Ordering;
        // Into the changesets' copy, then the writer's flag past a fence: a writer letting go as
        // this comes either reads this (past the fence it reads it after) or is seen to have let go.
        self.changes_to_store.update(|c| c.insert(id.to_owned(), Arc::clone(&files)));
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.changes_storing.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, storing) = (Arc::clone(&self.changes_to_store), Arc::clone(&self.changes_storing));
        drop(crate::rt::spawn(async move {
            loop {
                let batch: Vec<(String, Files)> = if pending.load().is_empty() { Vec::new() } else { pending.update(std::mem::take).into_iter().collect() };
                if batch.is_empty() {
                    storing.store(false, Ordering::Release);
                    std::sync::atomic::fence(Ordering::SeqCst);
                    // One that came in between finds no writer at work: this one goes on with it,
                    // unless it started another.
                    if pending.load().is_empty() || storing.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    continue;
                }
                // Each changeset is its files' texts, written out once: a session at a time.
                let rows = crate::rt::compute_map(batch, |(session, files)| {
                    let changes = Some(write::string(write::texts_len(files.iter().map(|f| &**f)), |b| {
                        b.push(b'[');
                        for (i, f) in files.iter().enumerate() {
                            if i > 0 {
                                b.push(b',');
                            }
                            b.extend_from_slice(f.text().as_bytes());
                        }
                        b.push(b']');
                        Ok(())
                    }));
                    crate::convo::rows::Row::Session { session, info: None, meta: None, state: None, changes, epoch: None, open: false, at: crate::util::now_ms() }
                })
                .await;
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "the sessions' changesets couldn't be written out for the conversation store");
                        continue;
                    }
                };
                let store = store.clone();
                let kept = crate::rt::blocking(move || {
                    store.put(rows);
                    store.sync()
                })
                .await;
                if let Err(e) = kept.and_then(|r| r) {
                    tracing::debug!(error = %format!("{e:#}"), "the conversation store didn't say it had the changesets");
                }
            }
        }));
    }

    /// Machine `from` is known as `to` from now on (docs/machines.md): its sessions are there (and
    /// kept so in the conversation store), and what it offers is its. Its runner, coming back as
    /// `to`, finds them its own.
    pub async fn move_machine(&self, hub: &Hub, from: &str, to: &str) -> Vec<String> {
        let (at, now) = (Location::Machine(from.to_owned()), Location::Machine(to.to_owned()));
        let (was, there) = (at.clone(), Arc::new(now.clone()));
        // A session at a time, as its owner's turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let moved: Vec<Info> = self
            .sessions
            .on_each_session(
                move |_, m| *m.location == was,
                move |s, id| {
                    let m = s.get_mut(id)?;
                    m.location = Arc::clone(&there);
                    Arc::make_mut(&mut m.info).target = there.target();
                    m.commit(|_| None);
                    Some(Info::clone(&m.info))
                },
            )
            .await;
        self.kinds.update(|k| {
            if let Some(v) = k.remove(&at.target()) {
                k.entry(now.target()).or_insert(v);
            }
        });
        if let Some(st) = &self.convo {
            st.put(moved.iter().map(|i| crate::convo::rows::Row::Machine { session: i.id.clone(), machine: to.to_owned() }).collect());
        }
        let ids: Vec<String> = moved.iter().map(|i| i.id.clone()).collect();
        for info in moved {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
        ids
    }

    /// Sessions `moved` onto `machine`, connected now (docs/machines.md, "One record a machine"):
    /// those its runner processes hold (`held`) and that read disconnected come back as they are
    /// (their snapshots are asked for, the ids returned); those none of them holds aren't on the
    /// machine, and are gone, as its next inventory would say.
    pub async fn moved_onto_connected(&self, hub: &Hub, machine: &str, moved: &[String], held: &[String]) -> Vec<String> {
        let mine: Vec<String> = moved.iter().filter(|id| held.contains(*id)).cloned().collect();
        let ask = self.still_disconnected(machine, &mine);
        let at = Location::Machine(machine.to_owned());
        let away = Arc::new(moved.iter().filter(|id| !held.contains(*id)).cloned().collect::<Vec<String>>());
        // Its viewers are told, and its feed's topic goes with the last of them.
        // A session at a time, as its owner's turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let gone: Vec<String> = self.sessions.on_each_session(move |id, m| *m.location == at && away.iter().any(|a| a == id), |s, id| s.remove(id).map(|_| id.to_owned())).await;
        self.forget_holders(&gone);
        for id in gone {
            super::archive::closed(hub, &id);
            super::attention::forget(&id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
        ask
    }

    /// The machine went away: its sessions keep running there and come back when it reconnects.
    pub async fn machine_lost(&self, hub: &Hub, machine: &str) {
        self.mail_delivery.changed();
        self.streams.machine_lost(machine, &hub.shown(machine));
        self.ports.machine_lost(machine);
        let at = Location::Machine(machine.to_owned());
        // A session at a time, as its owner's turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let infos: Vec<Info> = self.sessions.on_each_session(move |_, m| *m.location == at, |s, id| s.get_mut(id).map(disconnected)).await;
        self.kinds.update(|k| k.remove(&format!("agent:{machine}")));
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// One runner process of a machine went away while another of the machine stays connected
    /// (see `crate::instances`): the sessions it held keep running there and come back with it.
    pub async fn sessions_lost(&self, hub: &Hub, ids: &[String]) {
        self.mail_delivery.changed();
        let ids: Arc<std::collections::HashSet<String>> = Arc::new(ids.iter().cloned().collect());
        // A session at a time, as its owner's turns take them (goal 9, plan 01a1a87b2cec step 3: `Copies::on_each_session`).
        let infos: Vec<Info> = self.sessions.on_each_session(move |id, _| ids.contains(id), |s, id| s.get_mut(id).map(disconnected)).await;
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What this server's copy of each session on `machine` has, said in digests: what a stream
    /// it starts sends only what the copy lacks of (`ServerMsg::Kept`, `crate::wire::sums`). Read
    /// from the copies' views: taking in updates waits for none of it.
    pub fn kept_on(&self, machine: &str) -> Vec<crate::proto::KeptSession> {
        let at = Location::Machine(machine.to_owned());
        let mut out = Vec::new();
        self.sessions.each(|id, v| {
            if *v.location == at {
                out.push(v.kept(id));
            }
        });
        out
    }

    /// Join a session's stream, as a viewer that takes every frame whole. None when there is no
    /// such session.
    pub fn attach(&self, id: &str) -> Option<Attachment> {
        self.attach_from(id, None, Patches::default(), View::default())
    }

    /// Back once this server can say whether it has session `id`, which it can't while it starts and the session may
    /// still come back with its runner (`super::returning`; docs/wire.md, "Restarts"): what an attach waits for before
    /// it's answered, so one that comes before its runner is back isn't told it's closed. At once in a server up past
    /// its runners' grace.
    pub async fn returned(&self, id: &str) {
        self.returning.wait(id, || self.sessions.slot(id).is_some()).await;
    }

    /// Join a session's stream as a viewer that had it through revision `after` (`(born, rev)`),
    /// and takes the patches `takes` says and its events as `view` says: what it lacks if the copy
    /// can resume it (the same process, nothing started over since), else all of it (or, to a
    /// viewer that takes a window, its window). None when there is no such session.
    pub fn attach_from(&self, id: &str, after: Option<(u64, u64)>, takes: Patches, view: View) -> Option<Attachment> {
        self.attach_window(id, after, takes, view, None)
    }

    pub fn attach_window(&self, id: &str, after: Option<(u64, u64)>, takes: Patches, view: View, limits: Option<super::history::Limits>) -> Option<Attachment> {
        // What's kept is read first: the first time, off disk.
        let files = self.keeper.frame(id);
        let slot = self.sessions.slot(id)?;
        // Counted among its viewers first, without its owner (goal 4, plan 01a1a59c2a9c step 3):
        // from here on every change the owner publishes goes to the feed too, or is in the view
        // read below (`super::copies::Mirror::commit`).
        let viewer = slot.tap().join(takes);
        let refusals = viewer.refusals();
        // Gone already: nothing is made for it.
        if slot.closed() {
            return None;
        }
        // Where the feed ends, then the view: every frame of a change after that view is past it.
        let at = viewer.position();
        let copy = slot.view();
        if copy.closed {
            return None;
        }
        let mut parts = copy.parts_after(after.filter(|(b, _)| *b == born()).map(|(_, rev)| rev), view, self.convo.is_some());
        parts.history = limits.map(|limits| History { limits, token: super::history::cursor(born(), copy.reset_at, id), durable: self.convo.is_some() });
        let rev = parts.rev;
        // Written out by the caller off the runtime's threads: no other session's publish waits on
        // this one, nor any socket on its thread, nor its owner.
        let store = self.convo.clone().map(|s| (s, Arc::from(id)));
        Some(Attachment { snapshot: Snapshot { parts, files, store }, topic: viewer.topic().to_owned(), at, viewer, refusals, rev })
    }

    /// Catch-up for `viewer`, which fell behind having had revision `rev`: what it missed (it begins
    /// with `resumed`), or all of it when that can't be told apart (`open`); where on its topic it
    /// reads on from, taken before the view, so it misses nothing (and skips what the catch-up
    /// held); the feed's refusals then; and the copy's revision they bring it to. None when the
    /// session is gone.
    /// Its events go as `view` says (W21-R15).
    pub fn frames_since(&self, id: &str, viewer: &Viewer, rev: u64, view: View) -> Option<(Snapshot, Option<kiln_seams::bus::Offset>, u64, u64)> {
        self.frames_since_window(id, viewer, rev, view, None)
    }

    pub fn frames_since_window(&self, id: &str, viewer: &Viewer, rev: u64, view: View, limits: Option<super::history::Limits>) -> Option<(Snapshot, Option<kiln_seams::bus::Offset>, u64, u64)> {
        let files = self.keeper.frame(id);
        let slot = self.sessions.slot(id)?;
        let refusals = viewer.refusals();
        let at = viewer.position();
        let copy = slot.view();
        if copy.closed {
            return None;
        }
        let mut parts = copy.parts_after(Some(rev), view, self.convo.is_some());
        parts.history = limits.map(|limits| History { limits, token: super::history::cursor(born(), copy.reset_at, id), durable: self.convo.is_some() });
        let now = parts.rev;
        let store = self.convo.clone().map(|s| (s, Arc::from(id)));
        Some((Snapshot { parts, files, store }, at, refusals, now))
    }

    pub fn history_valid(&self, id: &str, cursor: &str) -> bool {
        self.sessions.view(id).is_some_and(|v| super::history::valid_cursor(cursor, born(), v.reset_at, id))
    }

    /// Whether session `id`'s copy holds every event an older page before number `before` shows
    /// (`super::history::covers`): then [`Registry::history_overlay`] makes the page from the copy alone, and the
    /// conversation store, whose reads wait behind its writes, isn't read (plan 01a1cc09, step 1). Looked at off the
    /// shards, as the overlay is.
    pub async fn history_kept(&self, id: &str, before: u64, limit: usize) -> bool {
        let Some(view) = self.sessions.view(id) else { return false };
        crate::rt::blocking(move || {
            let floor = view.state.get::<u64>("cleared").unwrap_or(1).max(1);
            super::history::covers(view.events.metas().filter_map(|m| m.seq()), floor, before, limit)
        })
        .await
        .unwrap_or(false)
    }

    /// Reconcile a durable page against one exact, latest immutable copy after its IO. Work that
    /// reads compact/spilled entries is on the blocking pool; resets are checked again by the API.
    pub async fn history_overlay(&self, id: &str, cursor: &str, before: u64, limit: usize, events: Vec<Value>) -> anyhow::Result<Option<(Vec<Value>, u64, bool, u64)>> {
        let Some(view) = self.sessions.view(id) else { return Ok(None) };
        if !super::history::valid_cursor(cursor, born(), view.reset_at, id) { return Ok(None); }
        crate::rt::blocking(move || {
            let floor = view.state.get::<u64>("cleared").unwrap_or(1).max(1);
            let held: std::collections::HashSet<_> = view.events.metas().map(|m| m.id()).collect();
            let mut by_seq: std::collections::BTreeMap<u64, Value> = events.into_iter()
                .filter(|e| e.get("id").and_then(Value::as_str).is_none_or(|id| !held.contains(id)))
                .filter_map(|e| super::history::seq(&e).filter(|n| *n >= floor && *n < before).map(|seq| (seq,e))).collect();
            // Only a page plus one proof-of-more row is decoded, even when Copies holds a giant
            // single turn. Metadata still removes a correction's old durable id outside the page.
            let indices: Vec<_> = (0..view.events.len()).rev().filter(|i| view.events.meta(*i).and_then(|m| m.seq()).is_some_and(|n| n >= floor && n < before)).take(limit.saturating_add(1)).collect();
            let mut sending = view.events.sending(false);
            for i in indices {
                let Some(sent) = sending.take(i) else { continue };
                let Ok(value) = serde_json::from_str::<Value>(sent.text(false)) else { continue };
                if let Some(seq) = super::history::seq(&value) { by_seq.insert(seq, value); }
            }
            let more = by_seq.len() > limit;
            Some((by_seq.into_values().collect(), view.rev, more, floor))
        }).await
    }

    /// An older page of session `id`'s events, for a viewer that has them from number `before` on
    /// (docs/wire.md, "Windows"): from its copy, or where the conversation store has them
    /// ([`Paged::Store`]). None when there is no such session. Read on the blocking pool, off the shards
    /// (goal 9, plan 01a1a87b2cec step 3; goal 11, plan 01a1a87b2e56 step 5): a page reads its events as its
    /// viewer takes them, a lean viewer's each made lean (its JSON read whole), and each compact chunk among
    /// them read again, which the request's shard did.
    pub async fn page(&self, id: &str, before: u64, lean: bool) -> anyhow::Result<Option<Paged>> {
        let older = self.convo.is_some();
        let Some(view) = self.sessions.view(id) else { return Ok(None) };
        Ok(Some(crate::rt::blocking(move || view.page(before, lean, older)).await?))
    }

    /// Event `event` of session `id`, whole, as its copy has it: what a lean one held back
    /// (`super::window::lean`). None when the copy doesn't have it (or there's no such session).
    /// Read on the blocking pool, as a page is ([`Registry::page`]): one in a compact chunk is read
    /// again with the rest of its chunk.
    pub async fn event_text(&self, id: &str, event: &str) -> anyhow::Result<Option<String>> {
        let Some(view) = self.sessions.view(id) else { return Ok(None) };
        let event = event.to_owned();
        crate::rt::blocking(move || view.event_text(&event)).await
    }

    /// Files a finished message mentioned, kept now (`super::kept`): its viewers hear of them.
    pub async fn files_added(&self, id: &str, added: &[Kept]) {
        let (sid, added) = (id.to_owned(), added.to_vec());
        self.sessions
            .on(id, move |s| {
                if let Some(m) = s.get_mut(&sid) {
                    // A revision of its own, past what its viewers have: else they'd take it as had.
                    m.rev = m.rev.saturating_add(1);
                    m.commit(|m| {
                        let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::Files { files: added }, rev: Some(m.rev) }.text();
                        Some(super::copies::payload_of(&text, m.rev))
                    });
                }
            })
            .await;
    }

    /// Where a session runs, when one of its messages mentions `path` (a relative mention, in its
    /// folder): a viewer may read that file from its machine, and no other. A file its keeper lists is
    /// said on the request's shard; else its messages are looked through on the blocking pool, newest
    /// first, each one's `text` alone read of its JSON ([`mentions_in`]; goal 11 audit A1, A3): each was
    /// parsed whole on the request's shard, every compact chunk inflated and every spilled one read from
    /// its file on the way, for each file the keeper doesn't list (one too big, past a budget, or from
    /// before kiln kept files).
    pub async fn mentioned_where(&self, id: &str, path: &str) -> Option<Location> {
        let view = self.sessions.view(id)?;
        if self.keeper.lists_path(id, path) {
            return Some(Location::clone(&view.location));
        }
        let path = path.to_owned();
        match crate::rt::blocking(move || mentions_in(&view, &path).then(|| Location::clone(&view.location))).await {
            Ok(found) => found,
            Err(e) => {
                tracing::debug!(session = id, error = %format!("{e:#}"), "a session's messages weren't looked through for a file off the shard");
                None
            }
        }
    }

    /// Start sessions on `target`. Sessions on machines are named here and appear at once
    /// (starting); the machine fills them in.
    pub async fn open(&self, hub: &Hub, target: &str, spec: Spec, count: usize) -> anyhow::Result<Vec<Info>> {
        self.open_as(hub, target, spec, count, &[]).await
    }

    /// [`Registry::open`], the sessions under `ids` in order (made by the client that asks, so its
    /// page can open a session's own page before it has started, W21-R15; those it names no id
    /// for get one made here). One that's a session's already, or a start's under way, is refused
    /// ([`IdTaken`]) before anything starts: asked again under the same ids, as its answer was
    /// lost, a start never starts its sessions twice.
    pub async fn open_as(&self, hub: &Hub, target: &str, mut spec: Spec, count: usize, ids: &[String]) -> anyhow::Result<Vec<Info>> {
        let count = count.clamp(1, 16);
        // An isolated agent's agents are isolated too, however they're started; and one is held so
        // only where its machine starts microVMs: one that can't would run it as a process, unasked
        // (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32).
        if spec.isolation.is_none() {
            spec.isolation = spec.parent.as_deref().and_then(|p| self.info(p)).and_then(|i| i.isolation);
        }
        if spec.isolation.is_some() {
            crate::harness::lineage::isolation_on(hub, target).map_err(|why| anyhow::anyhow!("an agent can't run isolated there: {why}"))?;
        }
        let loc = Location::of_target(target);
        // A machine whose kiln is to be upgraded takes no new agent (`crate::skew`).
        if let Location::Machine(m) = &loc
            && let Some(why) = hub.skew_holds(m)
        {
            anyhow::bail!("{why}");
        }
        let _held = self.claim(ids)?;
        let mut out = Vec::new();
        for i in 0..count {
            let mut spec = spec.clone();
            #[cfg(test)]
            if spec.program.is_none() {
                spec.program.clone_from(&self.test_program.lock());
            }
            if count > 1 {
                spec.name = Some(format!("{} {}", spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)), i + 1));
            }
            match &loc {
                Location::Local => {
                    // Started on one of the server's own shells: in that shell's directory.
                    if let Some(pid) = spec.shell.as_ref().and_then(|t| self.local_shells.load().get(t).copied()) {
                        let (cwd, env) = crate::rt::blocking(move || super::host::shell_context(pid)).await.unwrap_or((None, Vec::new()));
                        if spec.cwd.is_none() {
                            spec.cwd = cwd;
                        }
                        if spec.env.is_empty() {
                            spec.env = env;
                        }
                    }
                    // A workspace of its own first: a volume over its folder, mounted, its folder in it.
                    let id = ids.get(i).cloned().unwrap_or_else(crate::util::new_id);
                    crate::workspace::prepare(&mut spec, &id).await.map_err(|e| e.context("its workspace"))?;
                    let made = spec.workspace.as_ref().and_then(|w| w.made.clone());
                    let s = match self.local.open(Some(id), spec) {
                        Ok(s) => s,
                        Err(e) => {
                            // What was made for it goes with it: asked again under its id, it's made anew.
                            if let Some(m) = made
                                && let Err(gone) = crate::workspace::slates::remove(&m).await
                            {
                                tracing::warn!(volume = %m.volume, error = %format!("{gone:#}"), "the workspace of an agent that couldn't start stayed");
                            }
                            return Err(e);
                        }
                    };
                    let mut info = s.info();
                    info.target = loc.target();
                    // Its copy made so the session lists before its first update arrives (unless
                    // that came first), there once this is answered.
                    let (sid, seed) = (info.id.clone(), Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus)));
                    self.sessions.on(&info.id, move |s| s.insert(sid, seed)).await;
                    out.push(info);
                }
                Location::Machine(machine) => {
                    let id = ids.get(i).cloned().unwrap_or_else(crate::util::new_id);
                    let json = serde_json::to_string(&spec)?;
                    // Its runner answers once it started it (docs/wire.md, "Commands").
                    use crate::wire::commands::Answer;
                    match hub.command(machine, &ServerMsg::HarnessOpen { session: id.clone(), json }, crate::hub::COMMAND_WITHIN).await {
                        Answer::Taken | Answer::Sent | Answer::Parked => {}
                        Answer::Refused { why, .. } => anyhow::bail!("{} refused to start it: {why}", hub.shown(machine)),
                        Answer::Unreachable { .. } => anyhow::bail!("machine {} is not connected", hub.shown(machine)),
                        Answer::Unanswered { why } => anyhow::bail!("{} may or may not have started it: {why}", hub.shown(machine)),
                    }
                    let now = crate::util::now_ms();
                    let kind = super::kinds::Kind::get(&spec.kind);
                    let info = Info {
                        id: id.clone(),
                        deferred: Default::default(), queue_partial: false, queue_revision: None, queue_next: None,
                        admission: None,
                        kind: spec.kind.clone(),
                        kind_name: kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned()),
                        target: loc.target(),
                        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)),
                        shell: spec.shell.clone(),
                        cwd: spec.cwd.clone().unwrap_or_default(),
                        tree: None,
                        created_at: now,
                        status: Status::Starting,
                        title: String::new(),
                        model: spec.model.clone().unwrap_or_default(),
                        mode: spec.mode.clone().unwrap_or_default(),
                        harness_session: spec.resume.clone(),
                        approvals: Vec::new(),
                        queued: 0,
                        queue: Vec::new(),
                        said: Vec::new(),
                        tasks_running: 0,
                        subagents: Vec::new(),
                        cost_usd: 0.0,
                        activity: None,
                        last_activity: now,
                        exit: None,
                        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
                        objective: None,
                        summary: String::new(),
                        doing: None,
                        epoch: 0,
                        todos_done: 0,
                        todos_total: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        context_pct: None,
                        turn_started: None,
                        limits: Vec::new(),
                        account: None,
                        plan: None,
                        left_behind: 0,
                        reach: spec.reach.clone(),
                        pause: None,
                        frozen: None,
                        perms: None,
                        room_wait: None,
                        last_turn: None,
                        keep_off: false,
                        held: None,
                        refused: None,
                        move_on_limit: false,
                        move_plan: Default::default(),
                        pause_at: None,
                        max_subagents: None,
                        parent: spec.parent.clone(),
                        plan_run: spec.plan_run.clone(),
                        helper: spec.helper.clone(),
                        subagent: spec.subagent,
                        variant_of: spec.variant_of.clone(),
                        no_bypass: !spec.allow_bypass,
                        workspace: spec.workspace.clone(),
                        isolation: spec.isolation.clone(),
                    };
                    let mut seed = Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus));
                    seed.opened_at = Some(std::time::Instant::now());
                    self.sessions.on(&id.clone(), move |s| s.insert(id, seed)).await;
                    out.push(info);
                }
            }
        }
        for info in &out {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info.clone()) });
        }
        Ok(out)
    }

    /// Holds `ids` for a start ([`Registry::open_as`]) until what's returned goes: none of them may
    /// be a session's already, or held by another start.
    fn claim(&self, ids: &[String]) -> anyhow::Result<Claim<'_>> {
        if ids.is_empty() {
            return Ok(Claim { held: &self.opening, ids: Vec::new() });
        }
        // Taken on the starts' copy, swapped in whole: of two starts asking for one id at once, the
        // one whose copy goes in first has it, and the other finds it held.
        let taken = self.opening.update(|held| {
            let taken = ids.iter().find(|id| self.sessions.contains(id) || held.contains(id.as_str())).cloned();
            if taken.is_none() {
                held.extend(ids.iter().cloned());
            }
            taken
        });
        if let Some(id) = taken {
            return Err(anyhow::Error::new(IdTaken(id)));
        }
        Ok(Claim { held: &self.opening, ids: ids.to_vec() })
    }

    /// Acknowledges an upload only after immutable bytes and their metadata have both committed.
    /// A partial failure leaves the caller owning its original and never removes stored bytes.
    pub async fn upload(&self, hub: &Hub, name: String, mime: String, data: bytes::Bytes) -> Result<String, super::mail::Error> {
        use kiln_seams::blobs::{Cond, Put};
        let unavailable = || super::mail::Error::Unavailable { reason: "attachment storage authority is unavailable; custody was not accepted".into() };
        let size = u64::try_from(data.len()).map_err(|_| unavailable())?;
        if size == 0 { return Err(super::mail::Error::Invalid { reason: "empty attachment".into() }); }
        if size > super::host::ATTACHMENT_MAX { return Err(super::mail::Error::TooLarge { revision: 0, budget: usize::try_from(super::host::ATTACHMENT_MAX).map_err(|_| unavailable())? }); }
        let (data, sha256, chunks) = crate::rt::blocking(move || {
            let sha256 = crate::util::sha256_hex(&data);
            let chunks = kiln_ci::store::cdc::cuts(&data).len();
            (data, sha256, chunks)
        }).await.map_err(|_| unavailable())?;
        let metadata = Upload { name, mime, size, sha256 };
        let encoded = super::state_view::encode(&metadata, super::state_view::FRAME).map_err(|_| super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME })?;
        let metadata_chunks = kiln_ci::store::cdc::cuts(&encoded).len();
        // LocalBlobs inserts each chunk and one object/key per blob; count both immutable puts.
        let records = ["bytes-object", "bytes-key", "metadata-object", "metadata-key"];
        let rows = chunks.checked_add(metadata_chunks).and_then(|n| n.checked_add(records.len())).and_then(|n| u64::try_from(n).ok()).ok_or_else(unavailable)?;
        let payload = size.checked_add(u64::try_from(encoded.len()).map_err(|_| unavailable())?).ok_or_else(unavailable)?;
        let reserve = hub.store.storage_reserve_tables(payload, vec!["blob_keys".into(), "cas_objects".into(), "cas_chunks".into()], rows).await.map_err(|_| unavailable())?;
        let floor = super::mail_delivery::storage_floor(hub)?;
        let path = hub.cfg.server.data_dir.clone();
        let free = crate::rt::blocking(move || crate::os::disk_available(&path)).await.map_err(|_| unavailable())?.ok_or_else(unavailable)?;
        let required = payload.checked_add(reserve).and_then(|n| n.checked_add(floor)).ok_or_else(unavailable)?;
        if free < required { return Err(super::mail::Error::Capacity { required, authority: free }); }
        let id = crate::util::new_id();
        let kept = hub.blobs.blobs.put(&Upload::bytes(&id), data.to_vec(), Put { cond: Cond::IfNoneMatch, sha256: Some(metadata.sha256.clone()) }).await.map_err(upload_refused)?;
        if kept.size != size || kept.etag != metadata.sha256 { return Err(unavailable()); }
        let hash = crate::util::sha256_hex(&encoded);
        let len = u64::try_from(encoded.len()).map_err(|_| unavailable())?;
        let kept = hub.blobs.blobs.put(&Upload::metadata(&id), encoded, Put { cond: Cond::IfNoneMatch, sha256: Some(hash.clone()) }).await.map_err(upload_refused)?;
        if kept.size != len || kept.etag != hash { return Err(unavailable()); }
        Ok(id)
    }

    async fn upload_of(&self, hub: &Hub, id: &str) -> Result<Upload, super::mail::Error> {
        if !crate::util::is_id(id) { return Err(super::mail::Error::Invalid { reason: "invalid attachment identity".into() }); }
        let (kept, encoded) = crate::blobs::io::get_all(hub.blobs.blobs.as_ref(), &Upload::metadata(id), super::state_view::FRAME as u64).await.map_err(upload_refused)?;
        let invalid = || super::mail::Error::Unavailable { reason: "the original attachment metadata does not match its immutable bytes; communication remains held".into() };
        if crate::util::sha256_hex(&encoded) != kept.etag { return Err(invalid()); }
        let upload: Upload = serde_json::from_slice(&encoded).map_err(|_| invalid())?;
        if upload.size == 0 || upload.size > super::host::ATTACHMENT_MAX { return Err(invalid()); }
        let bytes = hub.blobs.blobs.head(&Upload::bytes(id)).await.map_err(upload_refused)?;
        if bytes.size != upload.size || bytes.etag != upload.sha256 { return Err(invalid()); }
        Ok(upload)
    }

    /// Preview the exact bounded wire command without transferring bytes or rewriting the
    /// immutable original. New custody preflight reserves this shape; retries bypass lookup.
    pub(super) async fn attachment_command(&self, hub: &Hub, recipient: &str, command: Command) -> Result<Command, super::mail::Error> {
        use base64::Engine as _;
        super::state_view::encode(&command, super::state_view::FRAME).map_err(|_| super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME })?;
        let Command::Send { id, text, mode, attachments, overrides, reply_to, task } = command else { return Ok(command) };
        let mut ready = Vec::new();
        for (index, mut attachment) in attachments.into_iter().enumerate() {
            if attachment.path.is_some() { return Err(super::mail::Error::Invalid { reason: "attachments require original bytes or a durable upload identity".into() }); }
            let sha256 = match &attachment.file_id {
                Some(file) => {
                    if !attachment.data_base64.is_empty() { return Err(super::mail::Error::Invalid { reason: "attachment must name one complete original representation".into() }); }
                    let upload = self.upload_of(hub, file).await?;
                    if attachment.name.is_empty() { attachment.name = upload.name; }
                    if attachment.mime.is_empty() { attachment.mime = upload.mime; }
                    upload.sha256
                }
                None => {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(attachment.data_base64.as_bytes()).map_err(|_| super::mail::Error::Invalid { reason: "attachment original is not valid base64".into() })?;
                    if bytes.is_empty() || u64::try_from(bytes.len()).ok().is_none_or(|n| n > super::host::ATTACHMENT_MAX) { return Err(super::mail::Error::Invalid { reason: "attachment original is empty or exceeds its file limit".into() }); }
                    crate::util::sha256_hex(&bytes)
                }
            };
            attachment.file_id = Some(crate::util::derive_id(&["harness-attachment-v1", recipient, &id, &index.to_string(), &sha256]));
            attachment.data_base64.clear();
            ready.push(attachment);
        }
        let command = Command::Send { id, text, mode, attachments: ready, overrides, reply_to, task };
        super::state_view::encode(&command, super::state_view::FRAME).map_err(|_| super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME })?;
        Ok(command)
    }

    /// Transfer bounded verified pieces before dispatch; retry identity is derived from immutable
    /// recipient/message/index/content. Missing or corrupt files never become a name-only send.
    pub(super) async fn route_attachments(&self, hub: &Hub, id: &str, loc: &Location, c: Command) -> Result<Command, Sent> {
        self.route_attachments_on(hub, id, loc, c, crate::proto::LANE_CONTROL).await
    }

    /// [`Registry::route_attachments`], its pieces sent on `lane`: the lane of the command they go
    /// with, which can't overtake them there (an Admit's, the control lane; a message for a
    /// runner's inbox, `LANE_TERM`, which drops a file that hasn't come yet).
    async fn route_attachments_on(&self, hub: &Hub, id: &str, loc: &Location, c: Command, lane: u32) -> Result<Command, Sent> {
        use base64::Engine as _;
        use sha2::Digest as _;
        super::state_view::encode(&c, super::state_view::FRAME).map_err(|_| Sent::PublicationRefused(super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME }))?;
        let Command::Send { id: mid, text, mode, attachments, overrides, reply_to, task } = c else { return Ok(c) };
        let waiting = || Sent::PublicationRefused(super::mail::Error::Unavailable { reason: "the complete original attachment is waiting for verified storage at its owner".into() });
        let mut out = Vec::new();
        for (index, mut a) in attachments.into_iter().enumerate() {
            if a.path.is_some() { return Err(waiting()); }
            let (size, sha256, key, inline) = match &a.file_id {
                Some(file) => {
                    let upload = self.upload_of(hub, file).await.map_err(Sent::PublicationRefused)?;
                    if a.name.is_empty() { a.name = upload.name; }
                    if a.mime.is_empty() { a.mime = upload.mime; }
                    (upload.size, upload.sha256, Some(Upload::bytes(file)), Vec::new())
                }
                None => {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes()).map_err(|_| waiting())?;
                    let size = u64::try_from(bytes.len()).map_err(|_| waiting())?;
                    let sha256 = crate::util::sha256_hex(&bytes);
                    (size, sha256, None, bytes)
                }
            };
            if size == 0 || size > super::host::ATTACHMENT_MAX { return Err(waiting()); }
            let fid = crate::util::derive_id(&["harness-attachment-v1", id, &mid, &index.to_string(), &sha256]);
            let mut hash = sha2::Sha256::new();
            let mut offset = 0u64;
            while offset < size {
                let end = offset.checked_add(FILE_CHUNK as u64).ok_or_else(waiting)?.min(size);
                let data = match &key {
                    Some(key) => {
                        let (meta, bytes) = hub.blobs.blobs.get(key, Some(kiln_seams::blobs::Range { start: offset, end: end.checked_sub(1) })).await.map_err(|e| Sent::PublicationRefused(upload_refused(e)))?;
                        if meta.etag != sha256 || meta.size != size { return Err(waiting()); }
                        bytes
                    }
                    None => inline.get(usize::try_from(offset).map_err(|_| waiting())?..usize::try_from(end).map_err(|_| waiting())?).ok_or_else(waiting)?.to_vec(),
                };
                if u64::try_from(data.len()).ok() != end.checked_sub(offset) { return Err(waiting()); }
                hash.update(&data);
                let done = end == size;
                if done && hex::encode(hash.clone().finalize()) != sha256 { return Err(waiting()); }
                match loc {
                    Location::Local => {
                        let (host, sid, file) = (Arc::clone(&self.local), id.to_owned(), fid.clone());
                        if !matches!(crate::rt::blocking(move || host.receive(&sid, &file, offset, &data, done)).await, Ok(Ok(()))) { return Err(waiting()); }
                    }
                    Location::Machine(machine) => {
                        let msg = ServerMsg::HarnessFile { session: id.to_owned(), file: fid.clone(), offset, data, done };
                        // File pieces and the command after them share a lane; QUIC lane
                        // ordering prevents the command from overtaking its originals.
                        if !hub.send_to_agent(machine, lane, &msg).await { return Err(Sent::Unreachable); }
                    }
                }
                offset = end;
            }
            a.path = None;
            a.data_base64.clear();
            a.file_id = Some(fid);
            out.push(a);
        }
        Ok(Command::Send { id: mid, text, mode, attachments: out, overrides, reply_to, task })
    }

    /// A harness signed in on a machine (kiln's /login): each of that machine's sessions of it is
    /// told, to take up the account (Claude Code starts again with it once its turn ends; Codex reads
    /// it again).
    pub async fn signed_in(&self, hub: &Hub, target: &str, kind: &str, account: Option<&crate::accounts::Account>) {
        // Who it's signed in as now, so each session takes it at once rather than when it next
        // starts (its page would show the old account, and that account's usage, until then).
        let value = account.map(|a| serde_json::json!({"email": a.email, "org": a.org, "plan": a.plan, "method": a.method}));
        let ids: Vec<String> = self.list().into_iter().filter(|i| i.target == target && i.kind == kind).map(|i| i.id).collect();
        for id in ids {
            let _ = self.command(hub, &id, Command::Control { id: "account".into(), value: value.clone() }).await;
        }
    }

    /// The epoch the server last heard session `id` is at.
    pub fn epoch_of(&self, id: &str) -> u64 {
        let seen = self.sessions.with(id, |v| v.info.epoch).unwrap_or(0);
        self.epochs.load().get(id).copied().unwrap_or(0).max(seen)
    }

    /// A real server incarnation and authenticated owner connection, stable across bookkeeping
    /// writes and capacity updates. It changes only when restart/reconnect requires reconciliation.
    pub fn mail_connection(&self, hub: &Hub, id: &str) -> Option<String> {
        match self.location(id)? {
            Location::Local => Some(format!("{}:{}:local", born(), hub.generation)),
            Location::Machine(machine) => {
                let instances = hub.instances.load();
                let peer = instances.owner(&crate::instances::Owned::Agent(id.to_owned()))?;
                let owner = instances.get(peer)?;
                if owner.name != machine || !instances.current_connection(peer) { return None; }
                let connection = hub.tunnels.connection_id(peer)?;
                Some(format!("{}:{}:{peer}:{connection}", born(), hub.generation))
            }
        }
    }

    /// Waits for the answer to message `message` sent to session `id`: none when too many wait
    /// already (it isn't waited for). Its wait reaches the waits' owner before the message goes.
    async fn wait_for(&self, id: &str, message: &str) -> Option<futures::channel::oneshot::Receiver<Said>> {
        self.answers.wait_for(id, message).await
    }

    /// No longer waiting for message `message` sent to session `id`: the waits whose ends were
    /// dropped go (an answer for those still waiting still reaches them).
    fn unwait(&self, id: &str, message: &str) {
        self.answers.unwait(id, message);
    }

    /// Session `id`'s inbox said what became of a message: whoever waits for it longest is told (each
    /// send of it is answered once, in the order they went: two sent under one id at once, one
    /// taken and one refused, are each told their own).
    fn answered(&self, id: &str, said: Said) {
        crate::metrics::sends().said(&said);
        self.answers.answered(id, said);
    }

    /// The answer to message `message` sent to session `id`, once it comes (by [`ANSWER_WAIT`]
    /// on `hub`'s clock): on its way when it doesn't.
    async fn answer(&self, hub: &Hub, id: &str, message: &str, rx: futures::channel::oneshot::Receiver<Said>) -> Sent {
        let late = hub.clock.sleep(ANSWER_WAIT);
        let sent = match futures::future::select(rx, late).await {
            futures::future::Either::Left((Ok(said), _)) => match said.answer {
                Ok(a) => Sent::Accepted(a),
                Err(r) => Sent::Refused(r),
            },
            _ => {
                crate::metrics::sends().unanswered.inc();
                Sent::Ok
            }
        };
        self.unwait(id, message);
        sent
    }

    /// A command for a session, from any device. A message (`Command::Send`, a skill run with an
    /// id) goes for the epoch the server last heard the session is at, and its inbox's answer is
    /// waited for (CP-T3.2): one sent for an epoch the session has since left is sent again, once,
    /// for the epoch its inbox named.
    pub async fn command(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        let pending = self.dispatch(hub, id, c.clone()).await;
        self.resolve(hub, id, c, pending).await
    }

    /// The shared publication path for authenticated producers. Receipt means durable server
    /// custody only; actual source admission and consumption are separate owner events.
    #[allow(clippy::too_many_arguments, reason = "authenticated custody binds the existing recipient, command, proof, principal, sender and explicit status together")]
    pub async fn command_published(&self, hub: &Arc<Hub>, id: &str, command: Command, provenance: Option<Value>, owner: String, sender: String, status: Option<super::mail::StatusKey>) -> Result<super::mail::Receipt, super::mail::Error> {
        self.publish_command(hub, id, command, provenance, owner, sender, status).await
    }

    pub async fn command_status(&self, hub: &Hub, id: &str, command: Command, status: super::mail::StatusKey) -> Sent {
        match self.publish_command(hub, id, command, None, "server".into(), "server".into(), Some(status)).await {
            Ok(receipt) => Sent::Published(receipt),
            Err(error) => Sent::PublicationRefused(error),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "authenticated custody binds the existing recipient, command, proof, principal, sender and explicit status together")]
    async fn publish_command(&self, hub: &Hub, id: &str, command: Command, provenance: Option<Value>, owner: String, sender: String, status: Option<super::mail::StatusKey>) -> Result<super::mail::Receipt, super::mail::Error> {
        let message = message_id(&command).ok_or_else(|| super::mail::Error::Invalid { reason: "durable communications require an immutable nonempty message id".into() })?;
        if !super::session::is_message(&command) { return Err(super::mail::Error::Invalid { reason: "only a communication can transfer durable mail custody".into() }); }
        let command = serde_json::to_value(command).map_err(|_| super::mail::Error::Invalid { reason: "communication cannot be encoded".into() })?;
        super::mail_delivery::publish(hub, super::mail::Publish { owner, sender, recipient: id.to_owned(), id: message, command, provenance, status }).await
    }

    /// Publish a current source event on its existing session feed and control projection.
    /// Source epoch/location are checked again on the owner shard, after the ledger commit.
    pub async fn mail_event(&self, hub: &Arc<Hub>, id: &str, source: Location, event: super::admission::FlowEvent, projection: Option<super::mail::Projection>) -> bool {
        let (id, h) = (id.to_owned(), Arc::clone(hub));
        self.sessions.on(&id.clone(), move |s| {
            let capacity = super::mail_delivery::event_capacity(&event);
            let Some(m) = s.get_mut(&id) else { return false };
            if *m.location != source || m.info.epoch != capacity.epoch { return false; }
            let fault = matches!(&event, super::admission::FlowEvent::Fault { .. });
            let newer = fault || m.info.admission.as_ref().is_none_or(|was| was.epoch != capacity.epoch || was.generation <= capacity.generation);
            let changed = newer && if fault { m.info.admission.is_some() } else { m.info.admission.as_ref() != Some(capacity) };
            if newer {
                Arc::make_mut(&mut m.info).admission = (!fault).then(|| capacity.clone());
                if m.state.is_object() { Arc::make_mut(&mut m.state).set("admission", &if fault { Value::Null } else { json!(capacity) }); }
            }
            m.rev = m.rev.saturating_add(1);
            m.commit(|m| {
                let (publication, mail_scope) = projection.map(|p| (Some(p.publication), Some(p.mail_scope))).unwrap_or_default();
                let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::MailFlow { event: Box::new(event), publication, mail_scope }, rev: Some(m.rev) }.text();
                let mut payload = super::live::Payload::new(m.rev, text.len());
                payload.text().extend_from_slice(text.as_bytes());
                payload.done();
                Some(payload)
            });
            let lent = h.emit_harness(&id, m.info_encoded());
            m.harness_at = lent.seq();
            m.harness = Some(lent);
            if changed {
                h.harness.mail_delivery.changed();
                super::told::wake(&h, &id);
            }
            true
        }).await.unwrap_or(false)
    }

    pub fn mail_receipt(&self, id: &str, receipt: super::mail::Receipt) {
        self.asked.published(id, receipt);
    }

    /// What command `c` to session `id` is, for what its answers mean (`super::asked`): a message,
    /// and whether it goes under its id to an inbox that takes it once.
    pub fn kind_of(&self, hub: &Hub, id: &str, c: &Command) -> super::asked::Kind {
        let message = super::session::is_message(c);
        let inbox = match self.location(id) {
            Some(Location::Local) => self.local.inbox().is_some(),
            Some(Location::Machine(machine)) => hub.instances.load().wide_caps_for(&machine, &crate::instances::Owned::Agent(id.to_owned())) & crate::proto::CAP_INBOX != 0,
            None => false,
        };
        super::asked::Kind { message, deduped: message && inbox && message_id(c).is_some() }
    }

    /// Sends on command `c` to session `id`, new under the id `cmd` its client made, from `scope`
    /// (the UI's sockets', and an API call's under its key: [`super::asked::Asked::take`] took
    /// it). Its answers are kept as their stand says, whether or not who sent it is still there to
    /// hear them: one parked at once, then its runner's once it's back.
    pub async fn send_asked(&self, hub: &Arc<Hub>, id: &str, scope: &str, cmd: &str, c: Command) {
        use std::time::Instant;
        // A message goes into custody (W20), a person's in a queue of its own, unless it's a
        // person's and its session's runner predates mail-flow: then it goes to that runner's
        // inbox, as before, through `dispatch` below ([`super::mail_delivery::legacy_owner`]).
        if super::session::is_message(&c) && (!super::mail::person(scope) || !super::mail_delivery::legacy_owner(hub, id)) {
            let sent = match self.publish_command(hub, id, c, None, scope.to_owned(), scope.to_owned(), None).await {
                Ok(receipt) => Sent::Published(receipt),
                Err(error) => Sent::PublicationRefused(error),
            };
            self.asked.answered(id, scope, cmd, &sent, Instant::now());
            return;
        }
        let pending = self.dispatch(hub, id, c.clone()).await;
        let (h, session, scope, cmd) = (Arc::clone(hub), id.to_owned(), scope.to_owned(), cmd.to_owned());
        crate::rt::spawn(async move {
            let sent = match pending {
                Pending::Command(p) if p.is_away() => {
                    h.harness.asked.answered(&session, &scope, &cmd, &Sent::Parked, Instant::now());
                    Sent::of(p.answer(crate::wire::commands::PARK_FOR).await)
                }
                other => h.harness.resolve(&h, &session, c, other).await,
            };
            h.harness.asked.answered(&session, &scope, &cmd, &sent, Instant::now());
        });
    }

    /// What became of command `c` to session `id`, sent as [`Registry::dispatch`] said (`p`): its
    /// answer, waited for.
    pub async fn resolve(&self, hub: &Hub, id: &str, c: Command, p: Pending) -> Sent {
        match p {
            Pending::Now(sent) => sent,
            Pending::Answer { message, rx } => self.settle(hub, id, c, &message, rx).await,
            Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
        }
    }

    /// The answer to command `c`, message `message`, sent to session `id` ([`Registry::dispatch`]):
    /// sent for an epoch the session has since left, it's sent again, once, for the one its inbox
    /// named.
    pub async fn settle(&self, hub: &Hub, id: &str, c: Command, message: &str, rx: futures::channel::oneshot::Receiver<Said>) -> Sent {
        let sent = self.answer(hub, id, message, rx).await;
        if let Sent::Refused(r) = &sent
            && let Some(current) = again_for(r)
        {
            self.raise_epoch(id, current);
            return self.command_once(hub, id, c).await;
        }
        sent
    }

    async fn command_once(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        match self.dispatch(hub, id, c).await {
            Pending::Now(sent) => sent,
            Pending::Answer { message, rx } => self.answer(hub, id, &message, rx).await,
            Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
        }
    }

    /// Sends command `c` to session `id` now, after whatever was sent before: how it fared, or (a
    /// message) its inbox's answer to wait for ([`Registry::settle`]), so a sender that goes on
    /// meanwhile (a viewer's socket) waits for nothing.
    pub async fn dispatch(&self, hub: &Hub, id: &str, c: Command) -> Pending {
        // A person's pause of a session holds the subagents kiln started under it too, and resuming
        // it lets them go: each still going is sent its own, its answer not waited for.
        if let Command::Pause { on, task: None } = &c {
            let all = self.list();
            for sub in super::lineage::subagents_under(&all, id) {
                if all.iter().any(|i| i.id == sub && i.status != Status::Exited) {
                    let _ = Box::pin(self.dispatch_one(hub, &sub, Command::Pause { on: *on, task: None })).await;
                }
            }
        }
        // What a person lets a session do: never more than its parent's, and the subagents kiln
        // started under it narrowed by it, each sent its own.
        if let Command::Perms { perms, task: None } = &c {
            let all = self.list();
            let mut mine = None;
            for (sid, p) in super::lineage::perms_down(&all, id, perms.as_ref()) {
                if sid == id {
                    mine = Some(p);
                } else if all.iter().any(|i| i.id == sid && i.status != Status::Exited) {
                    let _ = Box::pin(self.dispatch_one(hub, &sid, Command::Perms { perms: p, task: None })).await;
                }
            }
            return Box::pin(self.dispatch_one(hub, id, Command::Perms { perms: mine.flatten(), task: None })).await;
        }
        Box::pin(self.dispatch_one(hub, id, c)).await
    }

    /// Command `c` for session `id` as [`Registry::command`] sends it, with what kiln and the relays
    /// before it record of passing it on (`relayed`: docs/ci.md W27-R08's relays, `crate::relays::words`)
    /// beside it in what goes to the runner, which checks it before the session takes it; sent again
    /// so when its epoch was left meanwhile.
    pub async fn command_relayed(&self, hub: &Hub, id: &str, c: Command, relayed: Value) -> Sent {
        let pending = self.dispatch_relayed(hub, id, c.clone(), Some(&relayed)).await;
        if let Pending::Answer { message, rx } = pending {
            let sent = self.answer(hub, id, &message, rx).await;
            if let Sent::Refused(r) = &sent
                && let Some(current) = again_for(r)
            {
                self.raise_epoch(id, current);
                return match self.dispatch_relayed(hub, id, c, Some(&relayed)).await {
                    Pending::Now(sent) => sent,
                    Pending::Answer { message, rx } => self.answer(hub, id, &message, rx).await,
                    Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
                };
            }
            return sent;
        }
        self.resolve(hub, id, c, pending).await
    }

    /// [`Registry::dispatch`], to session `id` alone.
    async fn dispatch_one(&self, hub: &Hub, id: &str, c: Command) -> Pending {
        self.dispatch_relayed(hub, id, c, None).await
    }

    /// [`Registry::dispatch_one`], with what kiln and the relays before it record of passing it on
    /// (`relayed`, [`Registry::command_relayed`]) beside it in what goes to the runner.
    async fn dispatch_relayed(&self, hub: &Hub, id: &str, c: Command, relayed: Option<&Value>) -> Pending {
        if matches!(&c, Command::Admit { .. } | Command::MailAck { .. } | Command::MailPeers { .. } | Command::MailHandoff { .. }) {
            return Pending::Now(Sent::PublicationRefused(super::mail::Error::Invalid { reason: "mail grants and receipts belong to the authenticated server relay".into() }));
        }
        // A person's message, or kiln's own, to a runner that predates mail-flow goes to its
        // inbox below, as before W20: that runner never pushes the capacity custody waits for.
        // An agent's relayed words stay in custody, bounded, until it's upgraded.
        if super::session::is_message(&c) && (relayed.is_some() || !super::mail_delivery::legacy_owner(hub, id)) {
            let provenance = relayed.map(|proof| json!({"relayed": proof}));
            return Pending::Now(match self.publish_command(hub, id, c, provenance, "server".into(), "server".into(), None).await {
                Ok(receipt) => Sent::Published(receipt),
                Err(error) => Sent::PublicationRefused(error),
            });
        }
        let Some(loc) = self.location(id) else { return Pending::Now(Sent::NoSession) };
        // Kiln's own (`goals::keep`), kept here: its harness has nothing to do with it.
        if let Command::Control { id: control, value } = &c
            && control == "keep_going"
        {
            self.set_keep_going(hub, id, value.as_ref().and_then(Value::as_bool).unwrap_or(true)).await;
            return Pending::Now(Sent::Ok);
        }
        // A name is kept here: the session takes it at once, whatever its machine can do (and
        // its harness names its conversation too, where it does).
        let renamed = matches!(&c, Command::Control { id: control, .. } if control == "rename");
        if let Command::Control { id: control, value } = &c
            && control == "rename"
        {
            self.rename(hub, id, value.as_ref()).await;
        }
        let c = match self.route_attachments_on(hub, id, &loc, c, LANE_TERM).await {
            Ok(c) => c,
            Err(e) => return Pending::Now(e),
        };
        let message = message_id(&c);
        let is_message = super::session::is_message(&c);
        // A message without an id goes on unchecked (counted).
        let unchecked = message.is_none() && is_message;
        let epoch = self.epoch_of(id);
        match loc {
            Location::Local => {
                let wait = match message.as_deref() {
                    Some(m) => self.wait_for(id, m).await,
                    None => None,
                };
                if !self.local.command_at(id, c, Some(epoch)) {
                    // Not waited for after all: its end of the wait goes first, then the wait.
                    drop(wait);
                    if let Some(m) = &message {
                        self.unwait(id, m);
                    }
                    // Its session is here: its lane holds its most (a message is refused so).
                    if message.is_some() && self.local.get(id).is_some() {
                        return Pending::Now(Sent::Refused(inbox::Refusal::Full { session: id.to_owned(), limit: super::host::LANE }));
                    }
                    return Pending::Now(Sent::NoSession);
                }
                if unchecked {
                    crate::metrics::sends().unchecked.inc();
                }
                match (wait, message) {
                    (Some(rx), Some(message)) => Pending::Answer { message, rx },
                    // A command its session here took.
                    _ if !is_message => Pending::Now(Sent::Taken),
                    _ => Pending::Now(Sent::Ok),
                }
            }
            Location::Machine(machine) => {
                let Ok(mut json) = serde_json::to_string(&c) else { return Pending::Now(Sent::NoSession) };
                if let Some(r) = relayed
                    && let Ok(mut v) = serde_json::from_str::<Value>(&json)
                    && let Some(o) = v.as_object_mut()
                {
                    o.insert("relayed".into(), r.clone());
                    json = v.to_string();
                }
                // A runner with inboxes takes a message for an epoch, and answers it.
                let (caps, away) = {
                    let inst = hub.instances.load();
                    (inst.wide_caps_for(&machine, &crate::instances::Owned::Agent(id.to_owned())), inst.newest(&machine).is_none())
                };
                let had_id = message.is_some();
                // A message with an id is refused at once while its runner is away (docs/wire.md's
                // `Send`), as before the command streams: parked on one, it would go with no epoch
                // to the copy there once that runner was back, though its session went on elsewhere
                // meanwhile (resumed, moved) and took it there too, sent again under its id: one
                // message, two harnesses (CP-T3.2, the twenty-second adversarial check's). CP-T3.3
                // parks it under its epoch.
                if had_id && away {
                    return Pending::Now(Sent::Unreachable);
                }
                if let Some(m) = message.filter(|_| caps & crate::proto::CAP_INBOX != 0) {
                    let wait = self.wait_for(id, &m).await;
                    if !hub.send_to_agent(&machine, LANE_TERM, &ServerMsg::HarnessSend { session: id.to_owned(), epoch, json }).await {
                        drop(wait);
                        self.unwait(id, &m);
                        return Pending::Now(Sent::Unreachable);
                    }
                    return match wait {
                        Some(rx) => Pending::Answer { message: m, rx },
                        None => Pending::Now(Sent::Ok),
                    };
                }
                // On the runner's command stream, which answers it (docs/wire.md, "Commands").
                let p = hub.command_start(&machine, &ServerMsg::HarnessCmd { session: id.to_owned(), json }).await;
                let unreachable = matches!(p.at_once(), Some(crate::wire::commands::Answer::Unreachable { .. }));
                if unreachable && !renamed {
                    return Pending::Now(Sent::Unreachable);
                }
                // A message to a runner from before inboxes, or one without an id: unchecked.
                if unchecked || had_id {
                    crate::metrics::sends().unchecked.inc();
                }
                // A name is given even while the machine is away: it's kept here.
                if unreachable { Pending::Now(Sent::Ok) } else { Pending::Command(p) }
            }
        }
    }

    /// Stop a session for good.
    /// Session `id` goes on on machine `to` (in folder `cwd`, else the one it worked in), from the
    /// server's conversation store, because its own machine is gone (`docs/conversations.md`,
    /// "Resuming on another machine"). From here on the copy on the machine it was on doesn't
    /// count: that machine lets go of it if it comes back. Ok once the package is on its way.
    pub async fn resume_on(&self, hub: &Hub, id: &str, to: &str, cwd: Option<String>) -> Result<(), String> {
        let store = self.convo.clone().ok_or_else(|| "this server keeps no conversations (KILN_CONVERSATIONS is off)".to_owned())?;
        let (from, epoch) = self.sessions.with(id, |v| (Location::clone(&v.location), v.state.get::<u64>("epoch").unwrap_or(0))).ok_or_else(|| "no such agent session".to_owned())?;
        let from_name = match &from {
            Location::Local => return Err("it runs on the server itself, which is up: move it instead".into()),
            Location::Machine(m) => m.clone(),
        };
        if hub.agents.load().get(&from_name).is_some_and(|a| a.connected) {
            return Err(format!("{} is online: move the session from there instead", hub.shown(&from_name)));
        }
        if !hub.agents.load().get(to).is_some_and(|a| a.connected) {
            return Err(format!("{} isn't connected", hub.shown(to)));
        }
        if hub.runner_caps(to) & crate::proto::CAP_CONVO == 0 {
            return Err(format!("{} runs a kiln too old to take a session from the server; upgrade it first", hub.shown(to)));
        }
        let (st, i, f) = (store.clone(), id.to_owned(), from_name.clone());
        let (from_shown, to_shown) = (hub.shown(&from_name), hub.shown(to));
        // The package, and its timeline for kiln's take of it (every event's digest), made on the blocking pool (goal 9,
        // plan 01a1a87b2cec step 3).
        let (mut pkg, timeline) = crate::rt::blocking(move || crate::convo::resume::package(&st, &i, &f, cwd.as_deref(), (&from_shown, &to_shown)).map(|p| {
            let t = crate::custody::timeline(&i, p.state.epoch, &p.events);
            (p, t)
        }))
        .await
        .map_err(|e| format!("{e:#}"))?
        .map_err(|e| format!("{e:#}"))?;
        // kiln takes it for the process that held it, which can't hand it on: the package carries
        // the take, which the receipt follows as a hand-on's (docs/ci.md W27-R08).
        let heard = hub.agents.load().get(&from_name).map(|a| a.last_seen).unwrap_or_default();
        let when = time::OffsetDateTime::from_unix_timestamp(heard).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_else(|| "never".to_owned());
        let why = format!("resumed from kiln's copy: {} was last heard from at {when}", hub.shown(&from_name));
        match crate::custody::take_for(&hub.store, &hub.pki, id, pkg.state.epoch, timeline, &why).await {
            Ok(take) => pkg.hand_on = take,
            Err(e) => tracing::warn!(target: "kiln.custody", session = %id, error = %format!("{e:#}"), "kiln couldn't take a session it resumes from its copy: it goes on unsigned"),
        }
        if hub.runner_wide_caps(to) & crate::proto::CAP_CUSTODY == 0 {
            let why = format!("{} runs a kiln from before signed hand-overs: it signs no receipt of kiln's take", hub.shown(to));
            unsigned(hub, id, pkg.state.epoch.saturating_add(1), &format!("runner {to}"), why);
        }
        let bytes = serde_json::to_vec(&pkg).map_err(|e| e.to_string())?;
        // Its copy where it was doesn't count from now on: not here, not in the store (rows that
        // machine still sends are left out).
        let next = epoch.max(pkg.state.epoch) + 1;
        self.raise_epoch(id, next);
        store.put(vec![crate::convo::rows::Row::Session { session: id.to_owned(), info: None, meta: None, state: None, changes: None, epoch: Some(next), open: false, at: crate::util::now_ms() }]);
        let (sid, there) = (id.to_owned(), Location::Machine(to.to_owned()));
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    m.location = Arc::new(there);
                    let info = Arc::make_mut(&mut m.info);
                    info.target = m.location.target();
                    info.status = Status::Starting;
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info.clone()) });
        }
        tracing::info!(session = %id, from = %from_name, %to, bytes = bytes.len(), "agent session goes on on another machine, from the server's store");
        send_package(hub, id, to, &bytes).await
    }

    pub fn never_started(&self, id: &str) -> bool {
        let Some(s) = self.local.get(id) else { return false };
        if s.pid.lock().is_some() {
            return false;
        }
        let core = s.core.lock();
        core.state.status == Status::Exited && core.state.harness_session.is_none() && !core.events().any(|e| matches!(e.body, super::model::Body::User { .. } | super::model::Body::Assistant { .. }))
    }

    pub async fn move_local(&self, hub: &Hub, id: &str, to: &str, cwd: &str) -> Result<(), String> {
        let s = self.local.get(id).filter(|_| self.never_started(id)).ok_or_else(|| "only an agent whose harness never started on the kiln server moves from it".to_owned())?;
        if !hub.agents.load().get(to).is_some_and(|a| a.connected) {
            return Err(format!("{} isn't connected", hub.shown(to)));
        }
        if hub.runner_caps(to) & crate::proto::CAP_CONVO == 0 {
            return Err(format!("{} runs a kiln too old to take a session from the server; upgrade it first", hub.shown(to)));
        }
        let spec = s.resume_spec();
        let (events, mut state) = {
            let core = s.core.lock();
            (core.events().cloned().collect::<Vec<_>>(), core.state.clone())
        };
        let had = self.sessions.with(id, |v| v.state.get::<u64>("epoch")).flatten().unwrap_or(0);
        let next = had.max(state.epoch) + 1;
        state.epoch = next - 1;
        let from_cwd = state.cwd.clone();
        let pkg = super::moving::pack(id, spec, cwd, "the kiln server", &from_cwd, super::moving::Carry::Nothing, events, state);
        let bytes = serde_json::to_vec(&pkg).map_err(|e| e.to_string())?;
        send_package(hub, id, to, &bytes).await?;
        self.raise_epoch(id, next);
        if let Some(store) = &self.convo {
            store.put(vec![crate::convo::rows::Row::Session { session: id.to_owned(), info: None, meta: None, state: None, changes: None, epoch: Some(next), open: false, at: crate::util::now_ms() }]);
        }
        let (sid, there) = (id.to_owned(), Location::Machine(to.to_owned()));
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).filter(|m| *m.location == Location::Local).map(|m| {
                    m.location = Arc::new(there);
                    let info = Arc::make_mut(&mut m.info);
                    info.target = m.location.target();
                    info.status = Status::Starting;
                    m.opened_at = Some(std::time::Instant::now());
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
        self.local.let_go(id);
        tracing::info!(session = %id, %to, bytes = bytes.len(), "an agent whose harness never started on the server goes on on a runner");
        Ok(())
    }

    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let loc = self.location(id);
        let ok = match &loc {
            None => false,
            Some(Location::Local) => match self.local.end(id) {
                super::host::Ended::Closed => true,
                super::host::Ended::NotHere => false,
                // Its archive or restore is under way (attention plan 01a199e1b177752e84dad5dc990e6a15): it goes on,
                // and so does its record here.
                super::host::Ended::Refused(why) => {
                    tracing::info!(session = %id, %why, "an agent wasn't ended");
                    return false;
                }
            },
            // Closed there, or once its machine is back; one its machine doesn't run is gone there.
            Some(Location::Machine(machine)) => {
                use crate::wire::commands::Answer;
                let answer = hub.command(machine, &ServerMsg::HarnessClose { session: id.to_owned() }, crate::hub::COMMAND_WITHIN).await;
                // Its runner keeps it: its archive or restore is under way there.
                if let Answer::Refused { code: crate::wire::channel::Refusal::Refused, why } = &answer {
                    tracing::info!(session = %id, %machine, %why, "an agent wasn't ended");
                    return false;
                }
                // Closed there: its own command streams go, as the runner lets go of them (runner-fan).
                if answer == Answer::Taken {
                    let session = id.to_owned();
                    hub.cmds.tell(move |c| c.forget_session(&session));
                }
                matches!(answer, Answer::Taken | Answer::Sent | Answer::Parked)
            }
        };
        // A machine that is gone cannot confirm: forget it here; it is dropped there on reconnect.
        // Ended in the store too, or it would be listed as away again after a restart.
        if loc.is_some() && (!ok || matches!(loc, Some(Location::Local))) {
            // Its viewers are told, and its feed's topic goes with the last of them. One of the server's own machine,
            // closed here, still says its last as its task ends: until then it's held closed, so what it said before and
            // is still on its way makes no copy of it again (HARNESS-4).
            let (sid, held) = (id.to_owned(), ok && matches!(loc, Some(Location::Local)));
            self.sessions
                .on(id, move |s| {
                    if held {
                        s.close(&sid);
                    } else {
                        s.remove(&sid);
                    }
                })
                .await;
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.streams.forget(hub, id).await;
            self.opened.forget(id);
            self.asked.forget(id);
            if let Some(store) = &self.convo {
                store.put(vec![crate::convo::rows::Row::Closed { session: id.to_owned(), at: crate::util::now_ms() }]);
            }
            super::archive::closed(hub, id);
            super::attention::forget(id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
        ok || loc.is_some()
    }

    /// The parsed state of a session (for checks before acting on it).
    pub fn state(&self, id: &str) -> Option<State> {
        // Read from its view (no copy of it made).
        let state = self.sessions.with(id, |v| Arc::clone(&v.state))?;
        state.read::<State>()
    }

    /// A session as lists show it.
    pub fn info(&self, id: &str) -> Option<Info> {
        self.sessions.with(id, |v| Info::clone(&v.info))
    }

    /// [`Registry::info`] as its owner shares it with its view, not copied (goal 9, plan 01a1a87b2cec step 3: what
    /// reads a few fields of a session found by id, the programs' thresholds and the goal stacks' words said again).
    pub fn info_shared(&self, id: &str) -> Option<Arc<Info>> {
        self.sessions.with(id, |v| Arc::clone(&v.info))
    }

    /// What waits in session `id`'s queue, as its machine last said, the messages whole.
    pub fn queue(&self, id: &str) -> Vec<super::model::QueueItem> {
        let Some(state) = self.sessions.with(id, |v| Arc::clone(&v.state)) else { return Vec::new() };
        state.get::<Vec<super::model::QueueItem>>("queue").unwrap_or_default()
    }

    /// What `agents.get` reads of a session (`harness::ops`): its state as last reported. None of its events is read (goal
    /// 11 audit A2(c): `conclusion` scanned back through them for a reply `agents.get` threw away, on the request's shard,
    /// inflating every compact chunk and reading every spilled one on the way).
    pub fn state_value(&self, id: &str) -> Option<Value> {
        let state = self.sessions.with(id, |v| Arc::clone(&v.state))?;
        Some(state.to_value())
    }

    /// Session `id`'s changeset as JSON: `Null` when it has none, or there's no such session.
    pub fn changes_value(&self, id: &str) -> Value {
        let changes = self.sessions.with(id, |v| v.changes.clone()).flatten();
        changes.map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value()).collect()))
    }

    /// Session `id`'s last reply on its main thread (an `assistant` event of no subagent's), as `agents.result` and an
    /// experiment read it: found by its place, which its copy keeps (`super::copies::Replied`), on the request's shard where
    /// its chunk keeps its events as they are; else read from its chunk, or the copy scanned back for it, on the blocking
    /// pool, where a compact chunk is inflated and a spilled one read from its file (goal 11 audit A2(c), A3: no shard
    /// reads a spill file). Its JSON read where that costs least (`super::copies::reply_value`). None when it has none, or
    /// there's no such session.
    pub async fn reply(&self, id: &str) -> Option<Value> {
        let view = self.sessions.view(id)?;
        let entry = match view.reply_kept() {
            Ok(kept) => kept,
            Err(_) => match crate::rt::blocking(move || view.reply_read()).await {
                Ok(read) => read,
                Err(e) => {
                    tracing::debug!(session = id, error = %format!("{e:#}"), "a session's last reply wasn't read off the shard");
                    None
                }
            },
        }?;
        Some(super::copies::reply_value(entry).await)
    }

    /// Tests: session `id`'s changeset, as its machine would report it.
    #[cfg(test)]
    pub async fn set_changes(&self, id: &str, files: Value) -> bool {
        let files: Option<Files> = match files {
            Value::Array(all) => Some(Arc::new(all.into_iter().map(|f| Arc::new(Entry::new(f))).collect())),
            _ => None,
        };
        let sid = id.to_owned();
        self.sessions
            .on(id, move |s| {
                s.get_mut(&sid)
                    .map(|m| {
                        m.changes = files;
                        m.commit(|_| None);
                    })
                    .is_some()
            })
            .await
            .unwrap_or(false)
    }
}

/// A copy whose machine (or runner process) went away: said disconnected, to its viewers too.
fn disconnected(m: &mut Mirror) -> Info {
    Arc::make_mut(&mut m.info).status = Status::Disconnected;
    if m.state.is_object() {
        Arc::make_mut(&mut m.state).set("status", &json!("disconnected"));
    }
    m.commit(|m| m.state_frame());
    Info::clone(&m.info)
}

/// Whether one of `v`'s messages (what the person or the agent said, newest first) mentions `path`, resolved in its folder
/// ([`Registry::mentioned_where`]): each one's `text` alone read of its JSON, nothing else of it made (goal 11 audit A1).
fn mentions_in(v: &super::copies::View, path: &str) -> bool {
    /// A message's text, borrowed from its JSON where it has no escapes.
    #[derive(serde::Deserialize)]
    struct Said<'a> {
        #[serde(borrow, default)]
        text: Option<std::borrow::Cow<'a, str>>,
    }
    let cwd = &v.info.cwd;
    v.events.iter().rev().any(|e| {
        if !matches!(e.entry.kind(), Some("user" | "assistant")) {
            return false;
        }
        let Ok(said) = serde_json::from_str::<Said<'_>>(e.entry.text()) else { return false };
        said.text.is_some_and(|t| files::mentioned(&t).iter().any(|p| files::resolve(p, cwd).as_deref() == Some(path)))
    })
}

/// A finished message's mentions of files, to keep (`super::kept`): what the person or the agent
/// said, never a tool's input or output.
pub(super) fn mentions(id: &str, cwd: &str, location: &Location, e: &super::model::Event) -> Option<Job> {
    let text = match &e.body {
        super::model::Body::User { text, .. } | super::model::Body::Assistant { text, streaming: false } => text,
        _ => return None,
    };
    let paths: Vec<String> = files::mentioned(text).iter().filter_map(|p| files::resolve(p, cwd)).collect();
    (!paths.is_empty()).then(|| Job { session: id.to_owned(), seq: e.seq, paths, location: location.clone() })
}

fn default_name(kind: &str) -> String {
    super::kinds::Kind::get(kind).map_or_else(|| kind.to_owned(), |k| k.name.to_owned())
}

/// The plan an event proposes (plan mode): its main thread's plan tool, with its words.
fn proposed_in(e: &super::model::Event) -> Option<String> {
    let super::model::Body::Tool(call) = &e.body else { return None };
    if e.parent.is_some() || call.tool != super::model::ToolKind::Plan {
        return None;
    }
    call.input.get("plan").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned)
}


/// Session `id`'s hand-over to `epoch`, taken by `took`, went unsigned because `why`: recorded,
/// with the process kiln last certified it as before then (docs/ci.md W27-R08).
fn unsigned(hub: &Hub, id: &str, epoch: u64, took: &str, why: String) {
    let (store, session, took) = (hub.store.clone(), id.to_owned(), took.to_owned());
    crate::rt::spawn(async move {
        let gave = crate::identities::holder_at(&store, &session, epoch.saturating_sub(1)).await.ok().flatten().unwrap_or_default();
        if let Err(e) = crate::custody::unsigned(&store, &session, epoch, &gave, &took, &why, crate::util::now_unix()).await {
            tracing::warn!(target: "kiln.custody", %session, error = %format!("{e:#}"), "an unsigned hand-over couldn't be recorded");
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A viewer that takes state patches, and one that takes nothing as a patch.
    const STATE: Patches = Patches { state: true, changes: false };
    const WHOLE: Patches = Patches { state: false, changes: false };

    /// W21-R15: a start under ids its client made starts its sessions under them, and one asked
    /// again under any of them (its answer lost on the way back) starts nothing: it's refused as
    /// taken. An id a start under way holds is refused too, until that start is done.
    #[test]
    fn w21_r15_a_start_under_ids_its_client_made_starts_each_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir = tempfile::tempdir()?;
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), ..Spec::default() };
            let (a, b) = (crate::util::new_id(), crate::util::new_id());
            let started = hub.harness.open_as(&hub, "local", spec.clone(), 2, &[a.clone(), b.clone()]).await?;
            assert_eq!(started.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), [a.as_str(), b.as_str()]);
            let listed = hub.harness.list().len();
            let again = hub.harness.open_as(&hub, "local", spec.clone(), 1, std::slice::from_ref(&b)).await;
            assert!(again.as_ref().err().and_then(|e| e.downcast_ref::<IdTaken>()).is_some_and(|t| t.0 == b), "{again:?}");
            assert_eq!(hub.harness.list().len(), listed, "nothing started twice");
            // Held by a start under way: refused until that start is done.
            let c = crate::util::new_id();
            let held = hub.harness.claim(std::slice::from_ref(&c))?;
            assert!(hub.harness.claim(std::slice::from_ref(&c)).is_err());
            drop(held);
            let one = hub.harness.open_as(&hub, "local", spec, 1, std::slice::from_ref(&c)).await?;
            assert_eq!(one.first().map(|i| i.id.as_str()), Some(c.as_str()));
            assert!(crate::util::is_id(&crate::util::new_id()) && !crate::util::is_id("../../etc") && !crate::util::is_id(&a.to_uppercase()));
            Ok(())
        })
    }

    /// HARNESS-4 (CP-M2's AC-M2.2): a session of the server's own machine, closed, is no longer known, and stays so until
    /// its task's last word. Its announcement, said before its close but taken after it (under load the loop that takes
    /// what the server's own sessions say handed it on late, while the close went to its copy's owner directly), makes no
    /// copy of it again; its end lets go of what held it closed. No loop takes what its task says here: each of its words
    /// is handed over by the test, in the order load made.
    #[test]
    fn harness_4_a_closed_agents_announcement_after_its_close_makes_no_copy_of_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir = tempfile::tempdir()?;
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), ..Spec::default() };
            let info = hub.harness.open(&hub, "local", spec, 1).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
            let id = info.id.clone();
            let known = |hub: &Hub| hub.harness.info(&id).is_some() || hub.harness.list().iter().any(|i| i.id == id);
            let held = |hub: &Arc<Hub>| {
                let (hub, key, of) = (Arc::clone(hub), id.clone(), id.clone());
                async move { hub.harness.sessions.on(&key, move |s| s.held_closed(&of)).await }
            };
            anyhow::ensure!(known(&hub), "open, it's known");
            anyhow::ensure!(hub.harness.close(&hub, &id).await, "closed");
            anyhow::ensure!(!known(&hub), "closed, it's still known");
            hub.harness.ingest(&Location::Local, &id, Update::Open { info: Box::new(info.clone()), since: None }).await;
            anyhow::ensure!(!known(&hub), "a closed agent is still known: its announcement after its close made it again");
            hub.harness.ingest(&Location::Local, &id, Update::Closed).await;
            anyhow::ensure!(!known(&hub) && held(&hub).await == Some(false), "its end lets go of what held it closed");
            Ok(())
        })
    }

    /// HARNESS-4: what holds a closed session of the server's own machine closed goes with its task, however that ends.
    /// Here it ends without saying `Closed`: let go of (it went on elsewhere) before its close came, it ends on that. Its task
    /// is held as it starts, waiting for a process it takes over to end, so both reach it before it reads either; then it
    /// goes on. What it hands on is taken as the server's own loop takes it, through its last word, that it's gone: its
    /// announcement makes no copy of it, and nothing of it is held after.
    #[test]
    fn harness_4_a_closes_hold_goes_with_the_sessions_task_however_it_ends() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir = tempfile::tempdir()?;
            let mut taken_over = std::process::Command::new("sleep").arg("600").spawn()?;
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), takeover_pid: Some(taken_over.id()), ..Spec::default() };
            let info = hub.harness.open(&hub, "local", spec, 1).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
            let id = info.id.clone();
            let known = |hub: &Hub| hub.harness.info(&id).is_some() || hub.harness.list().iter().any(|i| i.id == id);
            let held = |hub: &Arc<Hub>| {
                let (hub, key, of) = (Arc::clone(hub), id.clone(), id.clone());
                async move { hub.harness.sessions.on(&key, move |s| s.held_closed(&of)).await }
            };
            anyhow::ensure!(hub.harness.local.let_go(&id), "let go of");
            anyhow::ensure!(hub.harness.close(&hub, &id).await, "closed");
            anyhow::ensure!(!known(&hub) && held(&hub).await == Some(true), "closed, not known, held closed");
            taken_over.kill()?;
            taken_over.wait()?;
            let mut said = Vec::new();
            loop {
                let next = hub.harness.local_rx.recv().await.map_err(|_| anyhow::anyhow!("the server's own sessions' updates ended"))?;
                let last = matches!(&next, super::super::session::Local::Gone(g) if *g == id);
                said.push(match &next {
                    super::super::session::Local::Said(_, Update::Open { .. }) => "open",
                    super::super::session::Local::Said(_, Update::Closed) => "closed",
                    super::super::session::Local::Said(..) => "said",
                    super::super::session::Local::Gone(_) => "gone",
                });
                hub.harness.take_local(next).await;
                if last {
                    break;
                }
            }
            hub.harness.sessions.applied(&id).await;
            anyhow::ensure!(said.contains(&"open") && !said.contains(&"closed"), "its task announced it, and ended without saying Closed: {said:?}");
            anyhow::ensure!(!known(&hub), "a closed agent is still known: {said:?}");
            anyhow::ensure!(held(&hub).await == Some(false), "still held closed after its task is gone: {said:?}");
            Ok(())
        })
    }

    /// CP-T3.2: a message a machine without inboxes took (`seq` 0) is answered as taken, and as
    /// unchecked: sent again, it's another message. One an inbox took says nothing of the kind.
    #[test]
    fn a_message_no_inbox_checked_is_answered_unchecked() {
        let answer = |seq: u64| Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq, repeat: false };
        let unchecked = super::accepted("m1", &answer(0));
        assert!(unchecked.get("unchecked").and_then(Value::as_str).is_some_and(|w| w.contains("another message")), "{unchecked}");
        assert_eq!(unchecked.get("sent"), Some(&json!(true)));
        assert!(super::accepted("m1", &answer(7)).get("unchecked").is_none());
    }

    /// CP-T3.2: a wait for a message's answer that's given up goes (none is left behind to fill the
    /// most there may be); one still waiting stays, and is told the answer when it comes.
    #[test]
    fn waits_given_up_go_and_those_still_waiting_are_told() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let conversations = crate::config::Conversations { off: true, ..Default::default() };
            let r = Registry::with(dir.path().to_path_buf(), &conversations, Arc::new(kiln_seams::clock::SimClock::new()), sim_bus());
            let gone = r.wait_for("s1", "m1").await;
            let kept = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            drop(gone);
            r.unwait("s1", "m1");
            assert_eq!(r.answers.held().await, Some(1), "the one still waiting stays");
            let said = Said { message: "m1".into(), answer: Err(inbox::Refusal::NoSession { session: "s1".into() }) };
            r.answered("s1", said.clone());
            assert_eq!(r.answers.held().await, Some(0));
            assert_eq!(kept.await.ok(), Some(said));
            // Given up before any answer: nothing is left.
            let w = r.wait_for("s2", "m2").await;
            drop(w);
            r.unwait("s2", "m2");
            assert_eq!(r.answers.held().await, Some(0));
            Ok(())
        })
    }

    /// CP-T3.2: each send of a message is answered once, in the order they went: two sent under one
    /// id at once, the first taken and the other refused (another message under its id), are each
    /// told their own.
    #[test]
    fn each_send_under_an_id_is_told_its_own_answer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let conversations = crate::config::Conversations { off: true, ..Default::default() };
            let r = Registry::with(dir.path().to_path_buf(), &conversations, Arc::new(kiln_seams::clock::SimClock::new()), sim_bus());
            let first = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let second = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let taken = Said { message: "m1".into(), answer: Ok(inbox::Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq: 7, repeat: false }) };
            let reused = Said { message: "m1".into(), answer: Err(inbox::Refusal::Reused { session: "s1".into(), message: "m1".into() }) };
            r.answered("s1", taken.clone());
            r.answered("s1", reused.clone());
            assert_eq!(first.await.ok(), Some(taken));
            assert_eq!(second.await.ok(), Some(reused));
            assert_eq!(r.answers.held().await, Some(0));
            Ok(())
        })
    }

    /// One update taken by a copy, as `Registry::ingest_from` takes it: its revision first.
    fn took<R>(m: &mut Mirror, f: impl FnOnce(&mut Mirror) -> R) {
        m.rev += 1;
        f(m);
    }

    fn event(id: &str, text: &str) -> Arc<Entry> {
        Arc::new(Entry::new(json!({"id": id, "kind": "assistant", "text": text})))
    }

    /// A copy's state, set as its runner's would be.
    fn set_state(m: &mut Mirror, v: Value) {
        m.state = Arc::new(StateDoc::of_value(&v));
    }

    /// A copy's changeset as JSON (null before it has one).
    fn changes_of(m: &Mirror) -> Value {
        m.changes.as_ref().map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value()).collect()))
    }

    /// Changeset files as a copy keeps them.
    fn entries(files: Vec<Value>) -> Vec<Arc<Entry>> {
        files.into_iter().map(|f| Arc::new(Entry::new(f))).collect()
    }

    /// The ids of the events in a viewer's frames, in order.
    fn ids(frames: &[String]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .filter(|f| f.get("t").and_then(Value::as_str) == Some("events"))
            .flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    #[test]
    fn a_viewer_resumes_with_only_what_changed() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        for id in ["e1", "e2", "e3"] {
            took(&mut m, |m| m.upsert(event(id, "x")));
        }
        let had = m.rev;
        took(&mut m, |m| m.upsert(event("e4", "new")));
        took(&mut m, |m| m.append("e2", " more"));
        let frames = m.resume(had, None).unwrap_or_default();
        assert!(frames.first().is_some_and(|f| f.contains("\"t\":\"resumed\"")), "it resumes: {frames:?}");
        assert_eq!(ids(&frames), vec!["e2".to_owned(), "e4".to_owned()], "only what changed since, in the timeline's order");
        // Nothing changed since now: only the copy's newest state and its revision.
        assert_eq!(m.resume(m.rev, None).map(|f| ids(&f)), Some(Vec::new()));
        // A revision past what it has is another process's: not resumed.
        assert!(m.resume(m.rev + 1, None).is_none());
    }

    #[test]
    fn a_copy_that_started_over_is_sent_whole() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        took(&mut m, |m| m.upsert(event("e1", "x")));
        let had = m.rev;
        took(&mut m, |m| {
            m.clear_events();
            m.started_over();
        });
        took(&mut m, |m| m.upsert(event("e9", "after")));
        assert!(m.resume(had, None).is_none(), "a viewer from before the reset starts over");
        assert_eq!(m.resume(m.reset_at, None).map(|f| ids(&f)), Some(vec!["e9".to_owned()]));
    }

    /// The frames the copy writes out itself (live, with their revision, and in snapshots, without)
    /// are byte for byte the socket's own (`SessionDown`), whatever they carry: escapes, nesting,
    /// empty patches, a field gone.
    #[test]
    fn frames_are_written_as_the_socket_says_them() -> anyhow::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let odd = json!({"id": "e\"1\u{2028}", "seq": 3, "text": "line\nnext } ] \\ ☃", "n": [1, 2.5, null, true], "o": {"z": 1, "a": {"b": []}}});
        let files = [json!({"path": "src/a.rs", "added": 3, "hunks": [{"lines": ["+x", "-y \"q\""]}]}), json!({"path": "b.rs", "tree": "/w"})];
        let gone = vec![json!({"path": "c.rs"}), json!({"path": "d.rs", "tree": "/w"})];
        let odd_json = serde_json::to_vec(&odd)?;
        for rev in [None, Some(0), Some(42)] {
            let e = Entry::new(odd.clone());
            let fs: Vec<Entry> = files.iter().cloned().map(Entry::new).collect();
            let say = |frame: SessionFrame| SessionDown { frame, rev }.text();
            let written = |f: &dyn Fn(&mut Vec<u8>) -> std::io::Result<()>| write::string(64, |b| f(b));
            assert_eq!(written(&|b| write::events(b, [&e, &e], rev)), say(SessionFrame::Events { events: vec![odd.clone(), odd.clone()] }));
            assert_eq!(written(&|b| write::events(b, std::iter::empty::<&Entry>(), rev)), say(SessionFrame::Events { events: Vec::new() }));
            assert_eq!(written(&|b| write::state(b, &StateDoc::of_value(&odd), rev)), say(SessionFrame::State { state: odd.clone() }));
            assert_eq!(written(&|b| write::state(b, &StateDoc::default(), rev)), say(SessionFrame::State { state: Value::Null }));
            assert_eq!(written(&|b| write::meta(b, &odd_json, rev)), say(SessionFrame::Meta { meta: odd.clone() }));
            assert_eq!(written(&|b| write::changes(b, fs.iter(), rev)), say(SessionFrame::Changes { files: Value::Array(files.to_vec()) }));
            let set: Vec<&Entry> = fs.iter().collect();
            assert_eq!(written(&|b| write::changes_patch(b, &set, &gone, rev)), say(SessionFrame::ChangesPatch { set: files.to_vec(), gone: gone.clone() }));
            assert_eq!(written(&|b| write::changes_patch(b, &[], &[], rev)), say(SessionFrame::ChangesPatch { set: Vec::new(), gone: Vec::new() }));
            assert_eq!(written(&|b| write::changes_patch(b, set.get(..1).unwrap_or_default(), &[], rev)), say(SessionFrame::ChangesPatch { set: files.get(..1).unwrap_or_default().to_vec(), gone: Vec::new() }));
            assert_eq!(written(&|b| write::stream(b, "e\"1", "}\n☃", rev)), say(SessionFrame::Stream { id: "e\"1".into(), text: "}\n☃".into() }));
            let (was, now) = (json!({"a": 1, "b": {"c": 2}, "gone": 3}), json!({"a": 1, "b": {"c": 4}, "new\"": [5]}));
            let (was, now) = (StateDoc::of_value(&was), StateDoc::of_value(&now));
            let (set, gone) = now.patch(&was).ok_or_else(|| anyhow::anyhow!("no patch"))?;
            let set_values = set.iter().map(|(k, v)| Ok(((*k).to_owned(), serde_json::from_slice(v)?))).collect::<anyhow::Result<serde_json::Map<String, Value>>>()?;
            let want = SessionFrame::StatePatch { set: set_values, gone: gone.iter().map(|k| (*k).to_owned()).collect() };
            assert_eq!(written(&|b| write::state_patch(b, &set, &gone, rev)), say(want));
            let (set, gone) = now.patch(&now).ok_or_else(|| anyhow::anyhow!("no patch"))?;
            assert_eq!(written(&|b| write::state_patch(b, &set, &gone, rev)), say(SessionFrame::StatePatch { set: serde_json::Map::new(), gone: Vec::new() }));
            // An entry's text is serde_json's, and its digest the one the runner makes of it.
            assert_eq!(e.text(), serde_json::to_string(&odd)?);
            assert_eq!(crate::wire::sums::event_text(e.text()), crate::wire::sums::event(&odd));
        }
        Ok(())
    }

    /// A long session lets go of its oldest events as it goes: those let go leave its index and the
    /// rest are found where they are, as an index made again would have them.
    #[test]
    fn a_copy_that_lets_go_of_its_oldest_finds_the_rest_where_they_are() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        let n = MAX_EVENTS + MAX_EVENTS / 2;
        for i in 1..=n {
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id": format!("e{i}"), "seq": i, "kind": "assistant", "text": "x"})))));
        }
        assert!(m.events.len() <= MAX_EVENTS + MAX_EVENTS / 4, "{} kept", m.events.len());
        let had = m.index.clone();
        m.reindex();
        assert_eq!(had, m.index, "the index as made again");
        // A reply's words still go to its event, at the revision they came.
        let last = format!("e{n}");
        took(&mut m, |m| m.append(&last, "more"));
        let at = m.index.get(&m.events, &last).unwrap_or(usize::MAX);
        assert_eq!(m.events.get(at).and_then(|e| e.entry.value().get("text").and_then(Value::as_str).map(str::to_owned)).as_deref(), Some("xmore"));
        assert_eq!(m.events.get(at).map(|e| e.entry.text().contains("xmore")), Some(true), "its text written again");
        assert_eq!(m.events.get(at).map(|e| e.at), Some(m.rev), "changed at the revision it came");
    }

    /// With every viewer taking states and changesets as patches, they go as patches alone: a long
    /// session's whole changeset is never put on the bus (it takes 1 MiB at most). A viewer that
    /// takes them whole gets them whole again.
    #[test]
    fn frames_go_as_patches_alone_when_every_viewer_takes_them() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (patching, at) = m.feed.attach(CHANGES);
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        // 400 files of diffs, whole more than the bus takes (each line `"+x",`).
        let lines = u32::try_from(kiln_seams::bus::PAYLOAD_MAX / 400 / r#""+x","#.len() + 1)?;
        let many: Vec<Value> = (0..400).map(|n| file(&format!("src/f{n}.rs"), None, lines)).collect();
        m.take_changes(many, Vec::new(), true);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        // The first would go whole, nothing before it to patch, past what the bus takes: it isn't written, and its viewers
        // start over, sent the resync marker where the bus refused it (goal 9, plan 01a1a87b2cec step 3).
        assert_eq!(m.feed.publish_payload(f), super::super::live::Published::Sent);
        assert_eq!(m.feed.refusals(), 0);
        m.take_changes(vec![file("src/f3.rs", None, lines + 1)], Vec::new(), false);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        assert_eq!(m.feed.publish_payload(f), super::super::live::Published::Sent);
        set_state(&mut m, json!({"status": "running", "activity": "reading"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        set_state(&mut m, json!({"status": "running", "activity": "editing"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        // After the resync marker: the changeset's patch alone, the first state whole, the second its patch alone.
        let [_resync, changes, first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        assert!(changes.text.is_empty() && changes.patch.as_deref().is_some_and(|p| p.len() < 64 * 1024), "{}", changes.text.len());
        assert_eq!(serde_json::from_str::<Value>(changes.for_viewer(CHANGES))?, json!({"t": "changes_patch", "set": [file("src/f3.rs", None, lines + 1)], "rev": changes.rev}));
        assert!(first.patch.is_none() && !first.text.is_empty());
        assert!(second.text.is_empty());
        assert_eq!(serde_json::from_str::<Value>(second.for_viewer(CHANGES))?, json!({"t": "state_patch", "set": {"activity": "editing"}, "rev": second.rev}));
        // A viewer that takes them whole: whole again, its patch beside it for the rest.
        drop(patching);
        let (_whole, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        set_state(&mut m, json!({"status": "idle"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [f] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        assert_eq!(serde_json::from_str::<Value>(f.for_viewer(WHOLE))?, json!({"t": "state", "state": {"status": "idle"}, "rev": f.rev}));
        Ok(())
    }

    #[test]
    fn a_frame_says_its_copys_revision() -> anyhow::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let f = Frame::of(SessionFrame::Events { events: Vec::new() }.into()).stamped(7);
        let v: Value = serde_json::from_str(&f.text)?;
        assert_eq!(v.get("rev").and_then(Value::as_u64), Some(7));
        assert_eq!(f.rev, 7);
        // Stamped, a frame is its type's with its revision.
        for frame in [SessionFrame::Events { events: vec![json!({"id": "e1"})] }, SessionFrame::Closed, SessionFrame::Stream { id: "e1".into(), text: "}".into() }] {
            let stamped = Frame::of(frame.clone().into()).stamped(7);
            assert_eq!(stamped.text.as_ref(), SessionDown { frame, rev: Some(7) }.text());
        }
        Ok(())
    }

    /// A plan its main thread proposes (plan mode) is one to keep; a subagent's, another tool's,
    /// or one without words isn't.
    #[test]
    fn a_plan_proposed_in_plan_mode_is_one_to_keep() -> anyhow::Result<()> {
        let ev = |parent: Value, tool: &str, input: Value| -> anyhow::Result<crate::harness::model::Event> {
            Ok(serde_json::from_value(
                json!({"id": "e1", "seq": 1, "at": 1, "parent": parent, "kind": "tool", "tool_id": "t1", "name": "ExitPlanMode", "tool": tool, "title": "Plan", "input": input, "status": "pending", "started": 1}),
            )?)
        };
        let plan = "# Fix it\n\n1. Read\n2. Fix";
        assert_eq!(proposed_in(&ev(Value::Null, "plan", json!({ "plan": plan }))?).as_deref(), Some(plan));
        assert_eq!(proposed_in(&ev(json!("task1"), "plan", json!({"plan": "1. a"}))?), None, "a subagent's");
        assert_eq!(proposed_in(&ev(Value::Null, "shell", json!({"plan": "1. a"}))?), None, "another tool's");
        assert_eq!(proposed_in(&ev(Value::Null, "plan", json!({"plan": "  "}))?), None, "no words");
        Ok(())
    }

    #[test]
    fn a_name_given_here_holds_whatever_the_machine_knows_of_names() {
        // A runner from before names reports none: the name given here shows.
        assert_eq!(naming("", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("", "", "Claude Code", false), Naming::Theirs);
        // Upgraded since, it reports its harness's name (in any case): still the one given here.
        assert_eq!(naming("Claude Code", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("claude code ", "", "Claude Code", true), Naming::Keep);
        // A machine that names it itself: taken, and kept here too.
        assert_eq!(naming("slates-6d", "", "Claude Code", false), Naming::Adopt("slates-6d"));
        assert_eq!(naming("Mantle", "slates-6d", "Codex", true), Naming::Adopt("Mantle"));
        // Renamed here while the machine still reports its old one: the new one shows until the
        // machine takes it.
        assert_eq!(naming("slates-6d", "slates-6d", "Claude Code", true), Naming::Keep);
    }

    fn sim_bus() -> Arc<dyn kiln_seams::Bus> {
        Arc::new(kiln_seams::bus::SimBus::default())
    }

    /// The frames on a mirror's feed from `from`, as a viewer that takes patches reads them, and
    /// as one that doesn't.
    fn on_feed(bus: &Arc<dyn kiln_seams::Bus>, m: &Mirror, from: u64) -> anyhow::Result<Vec<Frame>> {
        let read = futures::executor::block_on(bus.read(&m.feed.topic(), from, 100))?;
        read.messages.iter().map(|msg| super::super::live::decode(&msg.payload).ok_or_else(|| anyhow::anyhow!("a frame the feed didn't make"))).collect()
    }

    #[test]
    fn a_state_goes_out_whole_and_as_what_changed_in_it() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (_viewer, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let skills = json!([{"name": "deploy", "description": "Ship it. ".repeat(200)}]);
        set_state(&mut m, json!({"status": "running", "activity": "reading", "skills": skills, "pause": {"reason": "limit"}}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        set_state(&mut m, json!({"status": "running", "activity": "editing", "skills": skills}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // The first went whole: there was nothing before it to patch.
        assert!(first.patch.is_none());
        assert_eq!(first.for_viewer(STATE), first.for_viewer(WHOLE));
        let whole: Value = serde_json::from_str(second.for_viewer(WHOLE))?;
        assert_eq!(whole, json!({"t": "state", "state": m.state.to_value(), "rev": second.rev}));
        // As what changed: the activity set, the pause gone, the skills (most of it) left out.
        let patch: Value = serde_json::from_str(second.for_viewer(STATE))?;
        assert_eq!(patch, json!({"t": "state_patch", "set": {"activity": "editing"}, "gone": ["pause"], "rev": second.rev}));
        assert!(second.for_viewer(STATE).len() * 20 < second.for_viewer(WHOLE).len());
        // Applied to what came first, it makes the state now.
        let mut had: Value = serde_json::from_str(first.for_viewer(STATE))?;
        let had = had.get_mut("state").and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("no state"))?;
        for (k, v) in patch.get("set").and_then(Value::as_object).into_iter().flatten() {
            had.insert(k.clone(), v.clone());
        }
        for k in patch.get("gone").and_then(Value::as_array).into_iter().flatten() {
            had.remove(k.as_str().unwrap_or_default());
        }
        assert_eq!(Value::Object(had.clone()), m.state.to_value());
        Ok(())
    }

    #[test]
    fn states_without_viewers_do_not_serialize_or_keep_a_patch_baseline() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (viewer, _) = m.feed.attach(Patches::default());
        set_state(&mut m, json!({"status": "running"}));
        let mut made = false;
        m.commit(|m| {
            made = true;
            m.state_frame()
        });
        assert!(made && !m.sent.is_null());
        drop(viewer);
        set_state(&mut m, json!({"status": "running", "activity": "editing"}));
        let mut made = false;
        m.commit(|m| {
            made = true;
            m.state_frame()
        });
        assert!(!made, "no frame made with none watching");
        assert!(m.sent.is_null());
        let (_viewer, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let snapshot = Snapshot { parts: m.view().parts(), files: None, store: None }.frames();
        assert!(snapshot.iter().any(|f| serde_json::from_str::<Value>(f).ok() == Some(json!({"t": "state", "state": m.state.to_value()}))));
        set_state(&mut m, json!({"status": "idle"}));
        m.commit(|m| m.state_frame());
        let frames = on_feed(&bus, &m, at)?;
        let [frame] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // A field that appeared while nobody watched is absent from the whole state too.
        assert!(frame.patch.is_none());
        assert_eq!(serde_json::from_str::<Value>(frame.for_viewer(STATE))?, json!({"t": "state", "state": m.state.to_value(), "rev": frame.rev}));
        Ok(())
    }

    /// A viewer that takes changesets as patches too.
    const CHANGES: Patches = Patches { state: true, changes: true };

    /// A file of a changeset, as its JSON goes.
    fn file(path: &str, tree: Option<&str>, added: u32) -> Value {
        let mut f = json!({"path": path, "added": added, "removed": 0, "hunks": [{"old_start": 1, "old_lines": 0, "new_start": 1, "new_lines": added, "lines": vec!["+x"; added as usize]}], "at": 1});
        if let (Some(t), Some(o)) = (tree, f.as_object_mut()) {
            o.insert("tree".into(), json!(t));
        }
        f
    }

    /// A changeset's patch is taken in place: a file set replaces the one with its key where it
    /// is (the same path in another working tree is another file), a new one comes after the
    /// rest, and one gone goes.
    #[test]
    fn a_changesets_patch_is_taken_in_place() {
        let mut files = entries(vec![file("a.rs", None, 1), file("b.rs", None, 1), file("a.rs", Some("/w"), 1)]);
        merge_changes(&mut files, vec![file("c.rs", None, 1), file("a.rs", Some("/w"), 5)], &[json!({"path": "b.rs"})]);
        let values: Vec<Value> = files.iter().map(|f| f.value()).collect();
        assert_eq!(values, vec![file("a.rs", None, 1), file("a.rs", Some("/w"), 5), file("c.rs", None, 1)]);
        // Many set at once (a changeset sent whole) go by key the same way.
        let mut files = entries((0..20).map(|n| file(&format!("f{n}.rs"), None, 1)).collect());
        let set: Vec<Value> = (10..30).rev().map(|n| file(&format!("f{n}.rs"), None, 2)).collect();
        merge_changes(&mut files, set, &[]);
        let paths: Vec<String> = files.iter().filter_map(|f| f.file().0.map(str::to_owned)).collect();
        let want: Vec<String> = (0..20).chain((20..30).rev()).map(|n| format!("f{n}.rs")).collect();
        assert_eq!(paths, want);
        assert!(files.iter().skip(10).all(|f| f.value().get("added") == Some(&json!(2))));
    }

    /// A changeset goes to its viewers whole and as what changed in it: the file the agent touched
    /// and the one it put back, not every file's diff again. A viewer that takes only states as
    /// patches (one from before changesets had them) gets it whole.
    #[test]
    fn a_changeset_goes_out_whole_and_as_what_changed_in_it() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        // One viewer from before changesets had patches: changesets go whole for it.
        let (_viewer, at) = m.feed.attach(STATE);
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let many: Vec<Value> = (0..40).map(|n| file(&format!("src/f{n}.rs"), None, 50)).collect();
        m.take_changes(many, Vec::new(), true);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        m.take_changes(vec![file("src/f3.rs", None, 51), file("src/new.rs", None, 2)], vec![json!({"path": "src/f7.rs"})], false);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // The first went whole: there was nothing before it to patch.
        assert!(first.patch.is_none());
        assert_eq!(serde_json::from_str::<Value>(second.for_viewer(STATE))?, json!({"t": "changes", "files": changes_of(&m), "rev": second.rev}));
        let patch: Value = serde_json::from_str(second.for_viewer(CHANGES))?;
        assert_eq!(patch, json!({"t": "changes_patch", "set": [file("src/f3.rs", None, 51), file("src/new.rs", None, 2)], "gone": [{"path": "src/f7.rs"}], "rev": second.rev}));
        assert!(second.for_viewer(CHANGES).len() * 10 < second.for_viewer(STATE).len());
        // Taken into what came first as the copy takes one, it makes the changeset now, in order.
        let mut had = entries(serde_json::from_str::<Value>(first.for_viewer(CHANGES))?.get("files").and_then(Value::as_array).cloned().unwrap_or_default());
        let set = patch.get("set").and_then(Value::as_array).cloned().unwrap_or_default();
        let gone = patch.get("gone").and_then(Value::as_array).cloned().unwrap_or_default();
        merge_changes(&mut had, set, &gone);
        assert_eq!(Value::Array(had.iter().map(|f| f.value()).collect()), changes_of(&m));
        Ok(())
    }

    /// A changeset its viewers couldn't make from theirs by taking a patch (the same files in
    /// another order) goes whole.
    #[test]
    fn a_changeset_in_another_order_goes_whole() {
        let was = json!([file("a.rs", None, 1), file("b.rs", None, 1)]);
        assert!(changes_patch(&was, &json!([file("b.rs", None, 1), file("a.rs", None, 1)])).is_none());
        assert!(changes_patch(&Value::Null, &was).is_none());
        let patch = changes_patch(&was, &json!([file("a.rs", None, 2), file("b.rs", None, 1)])).unwrap_or_default();
        assert_eq!(serde_json::from_str::<Value>(&patch).ok(), Some(json!({"t": "changes_patch", "set": [file("a.rs", None, 2)]})));
    }

    /// The ids of the events a viewer attaching to `id` gets, in order.
    fn copy_of(hub: &Hub, id: &str) -> Vec<String> {
        hub.harness.attach(id).map(|a| ids(&a.frames())).unwrap_or_default()
    }

    /// What a live viewer was sent on its session's feed since it last looked (from `at`, moved past
    /// what it read).
    fn sent(hub: &Hub, topic: &str, at: &mut u64) -> Vec<String> {
        use kiln_seams::Bus as _;
        let Ok(read) = futures::executor::block_on(hub.bus.read(topic, *at, 1000)) else { return Vec::new() };
        *at = read.next;
        read.messages.iter().filter_map(|m| super::super::live::decode(&m.payload)).map(|f| f.text.to_string()).collect()
    }

    /// Goal 10, plan 01a1a87b2dae step 5 (and goal 9 step 3's owner's poll): a state's update encodes its
    /// session's info once, for its view, and the events socket's frame of it is those bytes, as
    /// `ServerEvent::Harness` wrote it before; a state that changes nothing its viewer has goes as no frame;
    /// the same meta again is kept, encoded and sent as nothing, a new one encoded once, its frame as before.
    #[test]
    fn g10_s5_an_update_encodes_once_what_it_changed() -> anyhow::Result<()> {
        use crate::harness::model::{Meta, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Watching, so the feed publishes; and the events socket's frames, as a page takes them.
            let (_watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            let mut events = hub.harness_events.subscribe(16)?;
            let encodings = || async { hub.harness.sessions.on("s", |s| s.get("s").map(|m| m.encodings)).await.flatten().unwrap_or_default() };
            let state = State { summary: "the retry loop reads its deadline".into(), ..State::default() };
            let was = encodings().await;
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state.clone()) }).await;
            let now = encodings().await;
            anyhow::ensure!(now.info == was.info + 1, "the info encoded once: {was:?} then {now:?}");
            let view = hub.harness.sessions.view("s").ok_or_else(|| anyhow::anyhow!("no view"))?;
            let frame = loop {
                let f = events.try_recv().map_err(|e| anyhow::anyhow!("no info frame on the events socket: {e:?}"))?;
                if f.starts_with(r#"{"type":"harness""#) {
                    break f;
                }
            };
            assert_eq!(&*frame, crate::wire::events::ServerEvent::Harness { info: Box::new((*view.info).clone()) }.text(), "the frame as before");
            assert_eq!(frame.as_bytes().get(r#"{"type":"harness","info":"#.len()..frame.len() - 1), Some(view.info_json.as_ref()), "the view's own bytes");
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.iter().filter(|f| f.starts_with(r#"{"t":"state""#)).count() == 1, "one state frame: {heard:?}");
            // The same state again: its info encoded for its view (its last activity moves), no frame.
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state) }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.is_empty(), "no frame for a state that changed nothing its viewer has: {heard:?}");
            // A meta: one frame, as it was written before, encoded once; the same again, nothing.
            let meta = Meta { version: "2.1".into(), agents: vec!["explore".into()], ..Meta::default() };
            let was = encodings().await;
            hub.harness.ingest(&there, "s", Update::Meta { meta: Box::new(meta.clone()) }).await;
            let heard = sent(&hub, &topic, &mut at);
            let want = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::Meta { meta: serde_json::to_value(&meta)? }, rev: hub.harness.rev("s") }.text();
            anyhow::ensure!(heard == vec![want], "the meta's frame as before: {heard:?}");
            anyhow::ensure!(encodings().await.meta == was.meta + 1, "the meta encoded once");
            hub.harness.ingest(&there, "s", Update::Meta { meta: Box::new(meta) }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.is_empty() && encodings().await.meta == was.meta + 1, "the same meta again: nothing kept, encoded or sent: {heard:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: the same state again from its machine isn't made into a value while the
    /// copy's is still the one made of it: the copy keeps that one (its revision moves, no frame goes), and its
    /// view holds what making it again would. Changed here since (a name given here), it's made again, the name in
    /// it, and the same again after that keeps that one; a changed state is made anew. The same meta again
    /// keeps the copy's.
    #[test]
    fn g9_s5_the_same_state_again_isnt_made_into_a_value() -> anyhow::Result<()> {
        use crate::harness::model::{Meta, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let (_watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            // The copy's state and meta values, by where they are, and its revision.
            let held = || async { hub.harness.sessions.on("s", |s| s.get("s").map(|m| (Arc::as_ptr(&m.state) as usize, Arc::as_ptr(&m.meta) as usize, m.rev))).await.flatten().unwrap_or_default() };
            let viewed = || hub.harness.sessions.view("s").map(|v| v.state.to_value()).unwrap_or_default();
            let state = State { summary: "the retry loop reads its deadline".into(), ..State::default() };
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state.clone()) }).await;
            let (made, _, rev) = held().await;
            anyhow::ensure!(sent(&hub, &topic, &mut at).iter().filter(|f| f.starts_with(r#"{"t":"state""#)).count() == 1, "the first state's frame");
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state.clone()) }).await;
            let (again, _, rev_again) = held().await;
            anyhow::ensure!(again == made && rev_again == rev + 1, "the same state again: kept ({made:#x} then {again:#x}), its revision {rev} then {rev_again}");
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.is_empty() && viewed() == serde_json::to_value(&state)?, "no frame, and the view as made again: {heard:?}");
            // A name given here changes the copy's state: the same state again is made again, the name in it.
            hub.harness.rename(&hub, "s", Some(&json!("parser"))).await;
            let (renamed, _, _) = held().await;
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state.clone()) }).await;
            let (remade, _, _) = held().await;
            let mut named = serde_json::to_value(&state)?;
            if let Some(o) = named.as_object_mut() {
                o.insert("name".into(), json!("parser"));
            }
            anyhow::ensure!(remade != renamed && viewed() == named, "made again after the name: {}", viewed());
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(state.clone()) }).await;
            anyhow::ensure!(held().await.0 == remade && viewed() == named, "the same again after that: kept, the name in it");
            // A changed state: made anew.
            let changed = State { doing: Some("Running cargo test".into()), ..state };
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(changed.clone()) }).await;
            let mut want = serde_json::to_value(&changed)?;
            if let Some(o) = want.as_object_mut() {
                o.insert("name".into(), json!("parser"));
            }
            anyhow::ensure!(held().await.0 != remade && viewed() == want, "a changed state made anew: {}", viewed());
            anyhow::ensure!(sent(&hub, &topic, &mut at).iter().any(|f| f.contains("Running cargo test")), "its frame went");
            // The same meta again: the copy's kept.
            let meta = Meta { version: "2.1".into(), ..Meta::default() };
            hub.harness.ingest(&there, "s", Update::Meta { meta: Box::new(meta.clone()) }).await;
            let (_, meta_made, _) = held().await;
            hub.harness.ingest(&there, "s", Update::Meta { meta: Box::new(meta) }).await;
            anyhow::ensure!(held().await.1 == meta_made, "the same meta again: kept");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3; goal 11: the sessions of one harness share its meta. Each owner makes one catalog of a
    /// meta its sessions say, and each of them on it holds that one: taking it allocates no more than the same meta again does
    /// and makes and encodes nothing, its frame is one block written once, and every viewer's frame and snapshot of it is byte
    /// for byte the socket's own, as each session wrote its own before.
    #[test]
    fn g9_s3_a_harnesss_sessions_share_its_meta_made_once() -> anyhow::Result<()> {
        use crate::harness::model::{Meta, Route, SlashCommand};
        use crate::os::counted::{Made, made};
        use crate::wire::ui::{SessionDown, SessionFrame};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // Its owners weigh writing out on their shards against a round trip of their own, a second's: every catalog is made
            // on the shard, where it's counted (consts-audit B1).
            hub.harness.sessions.on_each(|s| s.said().pin_trip(1_000_000_000)).await;
            let there = Location::Machine("box".into());
            // As the lab's harness says it: its commands, each with what it does (15 KB).
            let meta = |version: &str| Meta {
                version: version.into(),
                commands: (0..100)
                    .map(|k| SlashCommand { name: format!("command-{k}"), description: "reads what the agent did and says it in a few words ".repeat(2), hint: String::new(), group: String::new(), args: String::new(), aliases: Vec::new(), route: Route::default() })
                    .collect(),
                ..Meta::default()
            };
            // More sessions than owners.
            let ids: Vec<String> = (0..=2 * crate::rt::shard_count().max(1)).map(|k| format!("s{k}")).collect();
            let owners: std::collections::HashSet<usize> = ids.iter().map(|id| hub.harness.sessions.owner_of(id)).collect();
            for id in &ids {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            let held = || async {
                let mut held = Vec::new();
                for id in &ids {
                    let i = id.clone();
                    let of = hub.harness.sessions.on(id, move |s| s.get(&i).map(|m| (Arc::as_ptr(&m.meta) as usize, m.encodings.meta))).await.flatten();
                    held.push((hub.harness.sessions.owner_of(id), of.unwrap_or_default()));
                }
                held
            };
            // What `u` allocates on its owner's shard, taken as a runner's update is. Off Linux a catalog's making is learned on
            // the clock (`super::super::copies::Timer`), so the session's metas are taught short first: each is made where it's
            // counted, on the shard (as `g9_s5_what_an_update_costs_its_owner` teaches its states).
            let take = |id: &str, u: Update| {
                let (owner, at, id) = (Arc::clone(&hub), there.clone(), id.to_owned());
                async move {
                    let on = id.clone();
                    let blocks = owner.harness.sessions.on(&on, {
                        let owner = Arc::clone(&owner);
                        move |s| {
                            let said = s.said();
                            if let Some(m) = s.get_mut(&id) {
                                while m.said(&u, &said).is_some_and(|c| super::super::copies::said_off(c, said.sides())) {
                                    m.said_took(&u, std::time::Duration::ZERO, &said);
                                }
                            }
                            let before = made();
                            let rest = owner.harness.take_from(s, &owner, &at, &id, u, true, &|_| false);
                            let after = made();
                            rest.is_none().then_some(Made { blocks: after.blocks.saturating_sub(before.blocks), bytes: after.bytes.saturating_sub(before.bytes) })
                        }
                    });
                    blocks.await.flatten()
                }
            };
            // Unwatched: the first of each owner's sessions makes the catalog; each after it takes that one.
            let mut costs = Vec::new();
            for id in &ids {
                costs.push((hub.harness.sessions.owner_of(id), take(id, Update::Meta { meta: Box::new(meta("2.1")) }).await));
            }
            let again = take(ids.first().map_or("", String::as_str), Update::Meta { meta: Box::new(meta("2.1")) }).await;
            // What a session's copy kept of its own before: the meta as its machine said it, and the value made of it.
            let before = made();
            let said = Box::new(meta("2.1"));
            let value = serde_json::to_value(&*said)?;
            let own = made().bytes.saturating_sub(before.bytes);
            drop((said, value));
            let costs: Vec<(usize, Made)> = costs.into_iter().map(|(owner, m)| m.map(|m| (owner, m))).collect::<Option<_>>().ok_or_else(|| anyhow::anyhow!("a meta not taken whole, on the shard"))?;
            let again = again.ok_or_else(|| anyhow::anyhow!("the same again not taken whole, on the shard"))?;
            let mut seen = std::collections::HashSet::new();
            let shared: Vec<Made> = costs.iter().filter(|(owner, _)| !seen.insert(*owner)).map(|(_, m)| *m).collect();
            eprintln!("g9_s3 a harness's meta, taken by each session: {costs:?}; each after its owner's first {shared:?}; the same again {again:?}; a copy's own before, about {own} bytes");
            anyhow::ensure!(!shared.is_empty() && shared.iter().all(|m| m.blocks <= again.blocks), "each session after an owner's first takes its catalog for no more than the same again ({again:?}): {shared:?}");
            let now = held().await;
            let made_on = |owner: usize| now.iter().filter(|(o, _)| *o == owner).map(|(_, (_, n))| *n).sum::<u64>();
            let ptrs = |owner: usize| now.iter().filter(|(o, _)| *o == owner).map(|(_, (p, _))| *p).collect::<std::collections::HashSet<_>>();
            anyhow::ensure!(owners.iter().all(|o| made_on(*o) == 1 && ptrs(*o).len() == 1), "one catalog made on each owner, every session there holding it: {now:?}");
            // Its frame: one block, the frame's own, written once.
            let first = ids.first().cloned().unwrap_or_default();
            let (blocks, framed, rev) = hub
                .harness
                .sessions
                .on(&first.clone(), move |s| {
                    let u = Update::Meta { meta: Box::default() };
                    s.get_mut(&first).map(|m| {
                        let before = made();
                        let mut p = m.frame_of(&u, &[]);
                        let blocks = made().blocks.saturating_sub(before.blocks);
                        (blocks, super::super::live::decode_ref(p.text()).map(|f| f.text.to_owned()), m.rev)
                    })
                })
                .await
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let said = |rev: Option<u64>| -> anyhow::Result<String> { Ok(SessionDown { frame: SessionFrame::Meta { meta: serde_json::to_value(meta("2.1"))? }, rev }.text()) };
            anyhow::ensure!(blocks == 1 && framed == Some(said(Some(rev))?), "its frame one block ({blocks}), as before: {framed:?}");
            // Watched: two metas, said by every other session, so each owner's are said in turn: each made once on each owner it's
            // said on, and each viewer sent its own frame as before.
            let version = |k: usize| if k.is_multiple_of(2) { "2.2" } else { "2.3" };
            let mut viewers = Vec::new();
            for id in &ids {
                let a = hub.harness.attach(id).ok_or_else(|| anyhow::anyhow!("no copy"))?;
                anyhow::ensure!(a.frames().iter().filter(|f| f.starts_with(r#"{"t":"meta""#)).eq([&said(None)?]), "its snapshot's meta as before: {:?}", a.frames());
                viewers.push((id.clone(), a.viewer, a.topic, a.at.unwrap_or_default()));
            }
            let was = held().await;
            for (k, id) in ids.iter().enumerate() {
                anyhow::ensure!(take(id, Update::Meta { meta: Box::new(meta(version(k))) }).await.is_some(), "{id}'s meta taken whole, on the shard");
            }
            for (k, (id, _viewer, topic, at)) in viewers.iter_mut().enumerate() {
                let heard = sent(&hub, topic, at);
                let want = SessionDown { frame: SessionFrame::Meta { meta: serde_json::to_value(meta(version(k)))? }, rev: hub.harness.rev(id) }.text();
                anyhow::ensure!(heard == vec![want], "{id}'s viewer has its meta's frame as before: {heard:?}");
            }
            let now = held().await;
            let said_on: std::collections::HashSet<(usize, &str)> = now.iter().enumerate().map(|(k, (owner, _))| (*owner, version(k))).collect();
            let remade = now.iter().zip(&was).map(|((_, (_, n)), (_, (_, w)))| n.saturating_sub(*w)).sum::<u64>();
            let held_on: std::collections::HashSet<(usize, usize)> = now.iter().map(|(owner, (p, _))| (*owner, *p)).collect();
            anyhow::ensure!(remade == said_on.len() as u64 && held_on.len() == said_on.len(), "each meta made once on each owner it's said on ({}): {remade} made, {} held", said_on.len(), held_on.len());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what a runner's update costs its session's owner on its shard, in allocations
    /// (each block and reallocation) and their bytes, as the lab's agents send them (their machines keep their own
    /// stores, `shipped`): a reply of 1 KB, a state that changes what its info shows, and the same state again, each
    /// unwatched and watched (after one of each, so what the first makes once, a viewer's topic, is made). Said for
    /// the hand-over, and held to what the cuts left: a reply is written into room made for it once (it was copied into
    /// four larger buffers on its way, from serde_json's 128 bytes) and its id isn't copied to find its place.
    #[test]
    fn g9_s5_what_an_update_costs_its_owner() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        use crate::os::counted::{Made, made};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // Its owners weigh writing out on their shards against a round trip of their own, a second's: what's measured is
            // the shard's path, wherever the tests' process learned the pool's round trip to be (consts-audit B1).
            hub.harness.sessions.on_each(|s| s.said().pin_trip(1_000_000_000)).await;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), cwd: "/work".into(), ..Info::default() }), since: None }).await;
            // A reply of 1,020 bytes: past what glibc keeps a thread (1,032 bytes, its JSON with it).
            let reply = |n: u64| Event { id: format!("msg_lab_1_{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: "the retry loop reads its deadline ".repeat(30), streaming: false } };
            // A copy with events already, as the lab's are.
            hub.harness.ingest(&there, "s", Update::Events { events: (1..=40).map(reply).collect() }).await;
            let state = |doing: &str| State { summary: "the retry loop reads its deadline".into(), doing: Some(doing.to_owned()), ..State::default() };
            // What `u` costs its owner, taken as a runner's update is (`Hub::runner_update`'s `take_from`), and whether it
            // was taken whole (none of it left to take later, or written off the shard).
            //
            // Measured where it's taken, on the shard, whatever the host's clock. Off Linux what writing out takes is learned
            // on the clock (`super::super::copies::Timer`; Windows has no finer one), so a state or a reply the host held past
            // a slow poll there was learned as long, and the next went off the shard unmeasured (goal 4, plan 01a1a59c2a9c
            // step 5's Windows and macOS runs, 2026-11-06). So the session's states are taught short first (its own, and its
            // owners' for a first: `Mirror::said_took`), and a reply its owners judge long to write (every session's replies
            // teach them: `super::super::copies::long_to_write`) waits, its shard given up, while replies of another session
            // are taken as any are, until it's judged short.
            hub.harness.ingest(&there, "fill", Update::Open { info: Box::new(Info { id: "fill".into(), kind: "claude".into(), cwd: "/work".into(), ..Info::default() }), since: None }).await;
            let fills = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let cost = |u: Update| {
                let (h, from, fills) = (Arc::clone(&hub), there.clone(), Arc::clone(&fills));
                async move {
                    // Only bounds a hang: past it the test fails rather than waits forever.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let (owner, at, u) = (Arc::clone(&h), from.clone(), u.clone());
                        let measured = h
                            .harness
                            .sessions
                            .on("s", move |s| {
                                let said = s.said();
                                if let Update::Events { events } = &u
                                    && events.first().is_some_and(|e| super::super::copies::long_to_write(e, said.sides()))
                                {
                                    return None;
                                }
                                if let Some(m) = s.get_mut("s") {
                                    while m.said(&u, &said).is_some_and(|c| super::super::copies::said_off(c, said.sides())) {
                                        m.said_took(&u, std::time::Duration::ZERO, &said);
                                    }
                                }
                                let before = made();
                                let rest = owner.harness.take_from(s, &owner, &at, "s", u, true, &|_| false);
                                let after = made();
                                let whole = rest.is_none();
                                drop(rest);
                                Some((Made { blocks: after.blocks.saturating_sub(before.blocks), bytes: after.bytes.saturating_sub(before.bytes) }, whole))
                            })
                            .await
                            .flatten();
                        if let Some(m) = measured {
                            return m;
                        }
                        if std::time::Instant::now() >= deadline {
                            return (Made::default(), false);
                        }
                        let n = fills.fetch_add(1, std::sync::atomic::Ordering::Relaxed).saturating_add(1);
                        h.harness.ingest(&from, "fill", Update::Events { events: vec![reply(n)] }).await;
                    }
                }
            };
            let mut said = Vec::new();
            let mut seq = 41;
            for watched in [false, true] {
                let attached = if watched { Some(hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?) } else { None };
                let doing = |n: u64| format!("Reading part{n}.rs");
                // One of each first.
                cost(Update::Events { events: vec![reply(seq)] }).await;
                cost(Update::State { state: Box::new(state(&doing(seq))) }).await;
                seq += 1;
                let event = cost(Update::Events { events: vec![reply(seq)] }).await;
                let changed = cost(Update::State { state: Box::new(state(&doing(seq))) }).await;
                let again = cost(Update::State { state: Box::new(state(&doing(seq))) }).await;
                seq += 1;
                said.push((watched, event, changed, again));
                drop(attached);
            }
            for (watched, (event, _), (changed, _), (again, _)) in &said {
                eprintln!(
                    "g9_s5 an update's cost to its owner, {}: a reply {} allocations, {} bytes; a changed state {}, {}; the same state again {}, {}",
                    if *watched { "watched" } else { "unwatched" },
                    event.blocks,
                    event.bytes,
                    changed.blocks,
                    changed.bytes,
                    again.blocks,
                    again.bytes
                );
            }
            anyhow::ensure!(said.iter().all(|(_, e, c, a)| e.1 && c.1 && a.1), "each taken whole, on the shard (none made and not whole: a reply judged long to write for 30 s): {said:?}");
            // Held to what's left (measured 2026-11-06 on the owner's thread, where nothing else runs while one is
            // taken): a reply 13 allocations unwatched, 17 watched (its frame, and the bus's copies of it); a changed
            // state 26 and 32; the same state again 13. Before goal 9 step 5's cuts here a reply was five more: its id copied
            // to find its place, and its JSON's 1.1 KB copied into four larger buffers on its way out (serde_json's 128
            // bytes doubled to 2,048), each a reallocation.
            for (watched, (e, _), (c, _), (a, _)) in &said {
                let (reply, changed, again) = if *watched { (17, 32, 13) } else { (13, 26, 13) };
                anyhow::ensure!(e.blocks <= reply && c.blocks <= changed && a.blocks <= again, "watched {watched}: {e:?} {c:?} {a:?}");
            }
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: each change of a session's info lets go of its last `harness` frame in the
    /// events socket's feed (its copy held that frame's text, `crate::wire::events::feed::Feed::push_lent`): its copy
    /// keeps the newest frame's number, and a page reading the feed from before them all is sent the newest alone.
    #[test]
    fn g11_s5_a_sessions_info_frame_lets_go_of_its_last_in_the_feed() -> anyhow::Result<()> {
        use crate::harness::model::State;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let from = hub.feed.head();
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let at = || async { hub.harness.sessions.on("s", |s| s.get("s").map(|m| m.harness_at)).await.flatten().unwrap_or_default() };
            let mut ats = vec![at().await];
            for summary in ["one", "two", "three"] {
                hub.harness.ingest(&there, "s", Update::State { state: Box::new(State { summary: summary.into(), ..State::default() }) }).await;
                ats.push(at().await);
            }
            anyhow::ensure!(ats.windows(2).all(|w| w.first() < w.get(1)) && ats.first().is_some_and(|a| *a > from), "the copy keeps each frame's number: {ats:?} after {from}");
            let crate::wire::events::feed::Since::Frames(frames) = hub.feed.since(from) else { anyhow::bail!("a gap in the feed") };
            let infos: Vec<u64> = frames.iter().filter(|f| f.text.starts_with(r#"{"type":"harness","info":{"id":"s""#)).map(|f| f.seq).collect();
            anyhow::ensure!(infos.len() == 1 && infos.first() == ats.last(), "the session's info frames a page reads: {infos:?}, its newest {:?}", ats.last());
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: `GET /api/harness/sessions`' list is joined once for all the reads
    /// that find the sessions as they were, each answered those same bytes (nothing joined, sorted or copied
    /// for it); a session whose info changed, one that came and one that went each have it joined again once,
    /// saying what `list()` says, oldest first; events that change no session's info leave it as it was.
    #[test]
    fn g10_s5_the_sessions_list_is_joined_once_for_the_reads_that_find_it_as_it_was() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |id: &str, at: i64| Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), created_at: at, ..Info::default() }), since: None };
            for (id, at) in [("b", 2), ("a", 1), ("c", 3)] {
                hub.harness.ingest(&there, id, open(id, at)).await;
            }
            let listed = || anyhow::Ok(serde_json::to_vec(&hub.harness.list())?);
            let joins = || hub.harness.sessions.list_joins();
            let first = hub.harness.list_json();
            let at = joins();
            anyhow::ensure!(first.as_ref() == listed()?.as_slice(), "as the list says, oldest first: {}", String::from_utf8_lossy(&first));
            for _ in 0..3 {
                let again = hub.harness.list_json();
                anyhow::ensure!(again.as_ptr() == first.as_ptr() && again == first, "the same bytes, not joined again");
            }
            anyhow::ensure!(joins() == at, "nothing joined for the reads after the first");
            // A session's info changed: joined again once, for itself and the reads after.
            let state = State { summary: "the retry loop reads its deadline".into(), ..State::default() };
            hub.harness.ingest(&there, "b", Update::State { state: Box::new(state) }).await;
            let changed = hub.harness.list_json();
            anyhow::ensure!(changed.as_ptr() != first.as_ptr() && changed.as_ref() == listed()?.as_slice(), "the change in it");
            anyhow::ensure!(String::from_utf8_lossy(&changed).contains("the retry loop reads its deadline"), "its summary said");
            anyhow::ensure!(hub.harness.list_json().as_ptr() == changed.as_ptr() && joins() == at + 1, "joined once for it");
            // Events whose copy's info stays as it was leave the list as it was.
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the deadline"), streaming: false } };
            let before = hub.harness.info_json("a");
            hub.harness.ingest(&there, "a", Update::Events { events: (1..=3).map(said).collect() }).await;
            if hub.harness.info_json("a") == before {
                anyhow::ensure!(hub.harness.list_json().as_ptr() == changed.as_ptr() && joins() == at + 1, "events that changed no info: the list as it was");
            }
            // One comes, oldest of all; one goes.
            let joined = joins();
            hub.harness.ingest(&there, "d", open("d", 0)).await;
            let more = hub.harness.list_json();
            anyhow::ensure!(more.as_ref() == listed()?.as_slice() && more.starts_with(br#"[{"id":"d""#) && joins() == joined + 1, "one came: {}", String::from_utf8_lossy(&more));
            hub.harness.ingest(&there, "c", Update::Closed).await;
            let fewer = hub.harness.list_json();
            anyhow::ensure!(fewer.as_ref() == listed()?.as_slice() && !String::from_utf8_lossy(&fewer).contains(r#""id":"c""#) && joins() == joined + 2, "one went: {}", String::from_utf8_lossy(&fewer));
            anyhow::ensure!(hub.harness.list_json().as_ptr() == fewer.as_ptr() && joins() == joined + 2, "and then as it was");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: the sessions' list is named by a weak ETag made once for every read that finds
    /// it as it was: the digest of its infos' digests in its order (`crate::http::compress::digest`), each info's the
    /// last join's where it's the same, so the same list is named the same (the very header value) and another list
    /// another, as a digest of the list made from scratch says, through a change, a session that comes and one that
    /// goes; and `GET /api/harness/sessions` is answered with it, a 304 for a request that holds it.
    #[test]
    fn g9_s5_the_sessions_list_is_named_once_for_the_reads_that_find_it_as_it_was() -> anyhow::Result<()> {
        use crate::harness::model::State;
        use crate::http::compress::{digest, weak_etag};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |id: &str, at: i64| Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), created_at: at, ..Info::default() }), since: None };
            for (id, at) in [("b", 2), ("a", 1), ("c", 3)] {
                hub.harness.ingest(&there, id, open(id, at)).await;
            }
            // Its name from scratch: each info as the list says it, digested, the digests digested in order.
            let from_scratch = || -> anyhow::Result<String> {
                let infos: Vec<Vec<u8>> = hub.harness.list().iter().map(serde_json::to_vec).collect::<Result<_, _>>()?;
                let named: Vec<u8> = infos.iter().flat_map(|i| digest(i)).collect();
                Ok(weak_etag(&digest(&named)).and_then(|v| v.to_str().ok().map(str::to_owned)).unwrap_or_default())
            };
            let tag = || hub.harness.list_tagged().1.and_then(|v| v.to_str().ok().map(str::to_owned)).unwrap_or_default();
            let first = tag();
            anyhow::ensure!(first.starts_with("W/\"") && first == from_scratch()?, "{first} against {}", from_scratch()?);
            anyhow::ensure!(tag() == first, "the same list, the same name");
            hub.harness.ingest(&there, "b", Update::State { state: Box::new(State { summary: "a new summary".into(), ..State::default() }) }).await;
            let changed = tag();
            anyhow::ensure!(changed != first && changed == from_scratch()?, "a changed info: {changed}");
            hub.harness.ingest(&there, "d", open("d", 0)).await;
            let more = tag();
            anyhow::ensure!(more != changed && more == from_scratch()?, "one came: {more}");
            hub.harness.ingest(&there, "c", Update::Closed).await;
            let fewer = tag();
            anyhow::ensure!(fewer != more && fewer == from_scratch()?, "one went: {fewer}");
            // The read answers with it, and a request that holds it is answered 304.
            let (json, etag) = hub.harness.list_tagged();
            let etag = etag.ok_or_else(|| anyhow::anyhow!("no name"))?;
            let mut resp = crate::http::server::response(http::StatusCode::OK, "application/json", crate::http::server::full(json));
            resp.headers_mut().insert(http::header::ETAG, etag);
            let req = http::Request::builder().method(http::Method::GET).uri("/api/harness/sessions").header(http::header::IF_NONE_MATCH, fewer.as_str()).body(kiln_http::Incoming::empty())?;
            let answered = crate::http::compress::finish(crate::http::compress::Asked::of(&req), resp).await;
            anyhow::ensure!(answered.status() == http::StatusCode::NOT_MODIFIED, "{}", answered.status());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3 (sessions-read): kiln-lab's realistic list, 32 sessions of about 1.2 KB of info
    /// each, eight of them said anew between two reads as eight agents at work say theirs, is joined again in one join that
    /// allocates no more than a few blocks: its JSON, its record of what it said, room for those said anew, its name and
    /// the list itself, with the answer's two shares of them. The join took every session's view and its info twice in
    /// vectors grown as they filled, sorted them and gathered their digests into one more (21 blocks); a list read held
    /// its shard to 272 µs on the CPU in the Mac lab's realistic profile, 119 µs of it in its route. What it answers is
    /// what the list says, named as a digest of it made from scratch names it, and the read after it finds it as it was.
    #[test]
    fn g9_s3_a_list_with_eight_infos_said_anew_is_joined_in_a_few_blocks() -> anyhow::Result<()> {
        use crate::harness::model::State;
        use crate::http::compress::{digest, weak_etag};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let words = |n: usize, k: usize| (0..n).map(|w| ["the", "retry", "loop", "reads", "its", "deadline", "from", "same", "clock"].get((w + k) % 9).copied().unwrap_or("the")).collect::<Vec<_>>().join(" ");
            let ids: Vec<String> = (0..32).map(|k| format!("01a1b2c3-d4e5-7f60-8a9b-{k:012x}")).collect();
            for (k, id) in ids.iter().enumerate() {
                let info = Info {
                    id: id.clone(),
                    kind: "claude".into(),
                    kind_name: "Claude Code".into(),
                    name: format!("lab session {k}"),
                    cwd: format!("/Users/lab/.cache/kiln-lab/run/w/s{k}"),
                    title: words(12, k),
                    goal: words(60, k),
                    summary: words(40, k + 1),
                    model: "claude-opus-5-5".into(),
                    created_at: 1_000 + i64::try_from(k)?,
                    last_activity: 1_000,
                    ..Info::default()
                };
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(info), since: None }).await;
            }
            let joins = || hub.harness.sessions.list_joins();
            let from_scratch = || -> anyhow::Result<(Vec<u8>, String)> {
                let infos: Vec<Vec<u8>> = hub.harness.list().iter().map(serde_json::to_vec).collect::<Result<_, _>>()?;
                let named: Vec<u8> = infos.iter().flat_map(|i| digest(i)).collect();
                Ok((serde_json::to_vec(&hub.harness.list())?, weak_etag(&digest(&named)).and_then(|v| v.to_str().ok().map(str::to_owned)).unwrap_or_default()))
            };
            let (first, _) = hub.harness.list_tagged();
            anyhow::ensure!(first.len() > 32 * 1_000, "about 1.2 KB of info a session: {} bytes in all", first.len());
            for (k, id) in ids.iter().enumerate().filter(|(k, _)| k % 4 == 1) {
                hub.harness.ingest(&there, id, Update::State { state: Box::new(State { summary: words(40, k + 2), ..State::default() }) }).await;
            }
            let at = joins();
            let before = crate::os::counted::made().blocks;
            let (json, etag) = hub.harness.list_tagged();
            let made = crate::os::counted::made().blocks.saturating_sub(before);
            anyhow::ensure!(joins() == at + 1, "one join: {} then {}", at, joins());
            let (want, named) = from_scratch()?;
            anyhow::ensure!(json.as_ref() == want.as_slice(), "the list as it says, oldest first: {}", String::from_utf8_lossy(&json));
            anyhow::ensure!(etag.as_ref().and_then(|v| v.to_str().ok()) == Some(named.as_str()), "named as from scratch: {etag:?} against {named}");
            anyhow::ensure!(made <= 7, "the join allocated {made} blocks");
            let (again, _) = hub.harness.list_tagged();
            anyhow::ensure!(again.as_ptr() == json.as_ptr() && joins() == at + 1, "the read after finds it as it was");
            Ok(())
        })
    }

    /// CP-T3.2 (the thirteenth and fourteenth adversarial checks'): what kiln's own words (a goal
    /// stack's, a parent's news) said when their harness was handed them is kept on the session's
    /// info (`queued::Kept::look` reads it): from its runner's state (`State::given`), and from its
    /// user events (a runner from before says no `given`), once; another's message isn't. A runner
    /// that comes back (an Open) or started again (its state saying less) takes none of it away.
    #[test]
    fn kiln_s_own_words_handed_to_a_harness_are_noted_with_what_they_said() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, Given, State};
        use crate::harness::queued::stable;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = || Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None };
            hub.harness.ingest(&there, "s", open()).await;
            let user = |id: &str, seq: u64, text: &str| Event {
                id: id.into(),
                seq,
                at: 1,
                parent: None,
                by: None,
                body: Body::User { text: text.into(), images: 0, client_id: Some(id.into()), origin: None, attachments: Vec::new(), reply_to: None },
            };
            hub.harness.ingest(&there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass"), user("m-person", 2, "hello"), user("news-1", 3, "one child settled")] }).await;
            // Upserted again (its delivery's state), with other words: what it was handed stays.
            hub.harness.ingest(&there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass, edited later")] }).await;
            // Its runner's state says another handed.
            let given = |g: &[(&str, &str)]| Update::State { state: Box::new(State { given: g.iter().map(|(m, t)| Given { id: (*m).into(), digest: stable(t) }).collect(), ..State::default() }) };
            hub.harness.ingest(&there, "s", given(&[("goal-2", "/goal docs build")])).await;
            let want = vec![
                Given { id: "goal-1".into(), digest: stable("/goal tests pass") },
                Given { id: "news-1".into(), digest: stable("one child settled") },
                Given { id: "goal-2".into(), digest: stable("/goal docs build") },
            ];
            let said = || hub.harness.info("s").map(|i| i.said).unwrap_or_default();
            assert_eq!(said(), want);
            // Its runner comes back, then says less (started again): all of it stays.
            hub.harness.ingest(&there, "s", open()).await;
            hub.harness.ingest(&there, "s", given(&[])).await;
            assert_eq!(said(), want, "kept across the Open and a state that says less");
            Ok(())
        })
    }

    /// A runner that started again goes on from what the copy has (`crate::wire::sums`): the copy
    /// keeps its events through the Open's `since` and its viewers go on with what they have. Past
    /// the runner's newest the copy lets go, and an event under a number the copy has under
    /// another id takes its place: its viewers start over from the copy then, sent back by the
    /// feed's resync marker to a snapshot, since what they had can't be resumed.
    #[test]
    fn a_copy_goes_on_from_its_machines_open_and_its_viewers_with_it() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |since| Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 3, ..Info::default() }), since };
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            let names = |r: std::ops::RangeInclusive<u64>| r.map(|n| format!("m{n}")).collect::<Vec<_>>();
            hub.harness.ingest(&there, "s", open(None)).await;
            hub.harness.ingest(&there, "s", Update::Events { events: (1..=10).map(said).collect() }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Watching, so the feed publishes; and where on its topic the viewer reads from.
            let (watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            let resync = |heard: &[String]| heard.iter().any(|f| f == crate::harness::live::RESYNC);
            // Its machine again, with all ten: the copy goes on, and its viewer only hears what follows.
            hub.harness.ingest(&there, "s", open(Some(10))).await;
            hub.harness.ingest(&there, "s", Update::Events { events: vec![said(11)] }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.len() == 1 && !heard.iter().any(|f| f.contains(r#""t":"open""#)) && !resync(&heard), "the viewer goes on: {heard:?}");
            let had = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default();
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=11));
            // Again, with only eight of them (its store hadn't kept the rest): the copy lets go of
            // nine and up, and its viewer starts over from the copy: told to by the feed, and what it
            // had can't be resumed, so it's sent the copy whole.
            hub.harness.ingest(&there, "s", open(Some(8))).await;
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=8));
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(resync(&heard), "the viewer is sent back to a snapshot: {heard:?}");
            let again = hub.harness.frames_since("s", &watching, had, View::default()).ok_or_else(|| anyhow::anyhow!("no copy"))?.0.frames();
            anyhow::ensure!(again.first().is_some_and(|f| f.contains(r#""t":"open""#)) && ids(&again) == names(1..=8), "the viewer starts over: {again:?}");
            let kept = hub.harness.kept_on("box");
            anyhow::ensure!(kept.len() == 1 && kept.iter().all(|k| (k.epoch, k.from, k.seq, k.last.len()) == (3, 1, 8, 8)), "{kept:?}");
            // An event sent again under a number the copy has under another id: in its place.
            hub.harness.ingest(&there, "s", Update::Events { events: vec![Event { id: "other".into(), ..said(5) }] }).await;
            let mut want = names(1..=8);
            if let Some(slot) = want.get_mut(4) {
                "other".clone_into(slot);
            }
            anyhow::ensure!(copy_of(&hub, "s") == want, "{:?}", copy_of(&hub, "s"));
            anyhow::ensure!(resync(&sent(&hub, &topic, &mut at)), "the viewer starts over");
            // One it lacks under a number before its newest: in among the others, by number.
            hub.harness.ingest(&there, "s", open(Some(8))).await;
            hub.harness.ingest(&there, "s", Update::Events { events: vec![said(12), said(14)] }).await;
            hub.harness.ingest(&there, "s", Update::Events { events: vec![said(13)] }).await;
            want.extend(names(12..=14));
            anyhow::ensure!(copy_of(&hub, "s") == want, "{:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// A session this server opened on a machine isn't ended by a report the machine made before
    /// the open got there; one it had announced is, once a report leaves it out.
    #[test]
    fn a_report_made_before_an_open_got_there_doesnt_end_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(hub.bus.clone());
            let mut opened = Mirror::new(info("opened"), there.clone(), bus);
            opened.opened_at = Some(std::time::Instant::now());
            hub.harness.sessions.on("opened", move |s| s.insert("opened".into(), opened)).await;
            hub.harness.ingest(&there, "announced", Update::Open { info: Box::new(info("announced")), since: None }).await;
            let att = hub.harness.attach("announced").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let at = att.at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default()).await;
            let left = hub.harness.ids();
            anyhow::ensure!(left == vec!["opened".to_owned()], "{left:?}");
            // Its viewer hears it's gone: its feed's last frame.
            let read = futures::executor::block_on(kiln_seams::Bus::read(&hub.bus, &att.topic, at, 100))?;
            let said: Vec<String> = read.messages.iter().filter_map(|m| super::super::live::decode(&m.payload)).map(|f| f.text.to_string()).collect();
            anyhow::ensure!(said.last().is_some_and(|t| t == super::super::live::closed()), "its viewer heard {said:?}");
            // Its machine announces it: from then on its reports count.
            hub.harness.ingest(&there, "opened", Update::Open { info: Box::new(info("opened")), since: None }).await;
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default()).await;
            anyhow::ensure!(hub.harness.ids().is_empty());
            Ok(())
        })
    }

    /// A runner process's report speaks for the sessions it holds: those another process of its
    /// machine holds stay, listed away while that process isn't connected, never ended (a second
    /// runner on adacachy once ended every agent of the first, which was away).
    #[test]
    fn a_report_never_ends_what_another_process_of_the_machine_holds() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            for id in ["old-1", "old-2", "mine"] {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(info(id)), since: None }).await;
            }
            hub.harness.held_by("old-1", "01a14794");
            hub.harness.held_by("old-2", "01a14794");
            hub.harness.held_by("mine", "main");
            let report = |ids: &[&str]| crate::harness::host::Inventory { sessions: ids.iter().map(|s| (*s).to_owned()).collect(), ..Default::default() };
            let left = |hub: &crate::hub::Hub| hub.harness.ids();
            // `main` says what it holds while the other process is away.
            hub.harness.inventory(&hub, "box", Some("main"), &["main".to_owned()], report(&["mine"])).await;
            anyhow::ensure!(left(&hub) == ["mine", "old-1", "old-2"], "{:?}", left(&hub));
            let status = |id: &str| hub.harness.info(id).map(|i| i.status);
            anyhow::ensure!(status("old-1") == Some(Status::Disconnected) && status("old-2") == Some(Status::Disconnected), "away: {:?} {:?}", status("old-1"), status("old-2"));
            anyhow::ensure!(status("mine") != Some(Status::Disconnected), "{:?}", status("mine"));
            // The process that holds them says it holds one of them: the other ends.
            hub.harness.inventory(&hub, "box", Some("01a14794"), &["main".to_owned(), "01a14794".to_owned()], report(&["old-1"])).await;
            anyhow::ensure!(left(&hub) == ["mine", "old-1"], "{:?}", left(&hub));
            anyhow::ensure!(hub.harness.holder("old-2").is_none(), "its holder goes with it");
            // Who holds what is kept for the next start: written on the blocking pool, soon after.
            let read = || -> Option<HashMap<String, String>> { serde_json::from_slice(&std::fs::read(hub.harness.dir.join("holders.json")).ok()?).ok() };
            let is_now = |k: &HashMap<String, String>| k.get("old-1").map(String::as_str) == Some("01a14794") && !k.contains_key("old-2");
            let mut kept = read().unwrap_or_default();
            for _ in 0..200 {
                if is_now(&kept) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(25)).await;
                kept = read().unwrap_or_default();
            }
            anyhow::ensure!(is_now(&kept), "{kept:?}");
            Ok(())
        })
    }

    /// A burst of a changeset's patches (a runner sending what it held after a cut) is kept in the
    /// conversation store as the changeset they make, as the copy has it: the whole changeset isn't
    /// queued for the store at each patch (most of a megabyte each, for a long session).
    #[test]
    fn a_burst_of_patches_is_stored_as_the_changeset_they_make() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let store = hub.harness.convo.clone().ok_or_else(|| anyhow::anyhow!("no conversation store"))?;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let change = |n: u32| crate::harness::model::FileChange { path: format!("src/f{}.rs", n % 7), tree: None, created: false, deleted: false, added: n, removed: 0, hunks: Vec::new(), truncated: false, at: i64::from(n), by: Vec::new(), place: None };
            for n in 0..200 {
                hub.harness.ingest(&there, "s", Update::ChangesPatch { set: vec![change(n)], gone: Vec::new() }).await;
            }
            // Each file in the place its first patch gave it, as its last patch has it.
            let want = serde_json::to_value([196, 197, 198, 199, 193, 194, 195].map(change))?;
            let mut kept = None;
            for _ in 0..200 {
                let st = store.clone();
                kept = crate::rt::blocking(move || st.kept("s", 0)).await??.and_then(|k| k.changes).and_then(|c| serde_json::from_str::<Value>(&c).ok());
                if kept.as_ref() == Some(&want) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(25)).await;
            }
            anyhow::ensure!(kept.as_ref() == Some(&want), "{kept:?}");
            Ok(())
        })
    }

    /// W27-R08 (the twin): a runner's report names the sessions it holds a copy of that went on
    /// elsewhere at a later epoch, which it's told to let go of; not one just arriving there (its
    /// Open on its way, at the epoch it goes on at), not one it runs, and none from a runner that
    /// says no epochs.
    #[test]
    fn w27_r08_a_report_names_the_copies_left_behind() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let info = |id: &str, epoch: u64| Info { id: id.into(), kind: "claude".into(), epoch, ..Info::default() };
            // "twin" went on on box-b at epoch 1, its copy left on box-a; "arriving" is on its way
            // from box-b to box-a, at epoch 1 there; "here" runs on box-a.
            let (a, b) = (Location::Machine("box-a".into()), Location::Machine("box-b".into()));
            hub.harness.ingest(&b, "twin", Update::Open { info: Box::new(info("twin", 1)), since: None }).await;
            hub.harness.ingest(&b, "arriving", Update::Open { info: Box::new(info("arriving", 0)), since: None }).await;
            hub.harness.ingest(&a, "here", Update::Open { info: Box::new(info("here", 0)), since: None }).await;
            let report = |held: &[(&str, u64)]| crate::harness::host::Inventory { sessions: held.iter().map(|(s, _)| (*s).to_owned()).collect(), epochs: held.iter().map(|(s, e)| ((*s).to_owned(), *e)).collect(), ..Default::default() };
            assert_eq!(hub.harness.left_behind("box-a", &report(&[("twin", 0), ("arriving", 1), ("here", 0)])), ["twin"]);
            let before = crate::harness::host::Inventory { sessions: vec!["twin".into()], ..Default::default() };
            assert!(hub.harness.left_behind("box-a", &before).is_empty(), "a runner from before says no epochs");
            Ok(())
        })
    }

    /// A runner's link dropped and came back to the same server: the sessions it holds that have
    /// said nothing since still read disconnected, offline while their machine is connected (19
    /// agents on three machines, 2026-11-01). Its report finds them, for their snapshots.
    #[test]
    fn a_report_finds_its_sessions_that_still_read_disconnected() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            for id in ["quiet", "busy"] {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(info(id)), since: None }).await;
            }
            hub.harness.ingest(&Location::Machine("elsewhere".into()), "away", Update::Open { info: Box::new(info("away")), since: None }).await;
            hub.harness.machine_lost(&hub, "box").await;
            hub.harness.machine_lost(&hub, "elsewhere").await;
            // Back: one has something to say at once, the other is quiet.
            hub.harness.ingest(&there, "busy", Update::Open { info: Box::new(info("busy")), since: None }).await;
            let held = ["quiet", "busy", "away", "unknown"].map(str::to_owned);
            let stale = hub.harness.still_disconnected("box", &held);
            anyhow::ensure!(stale == ["quiet"], "only its own quiet session: {stale:?}");
            Ok(())
        })
    }

    /// W20-R01: an owner's Open restores lifecycle while its full State is still on the way.
    #[test]
    fn an_owner_open_updates_lifecycle_for_current_and_later_viewers() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |status, since| Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), status, epoch: 3, ..Info::default() }), since };
            hub.harness.ingest(&there, "s", open(Status::Idle, None)).await;
            hub.harness.ingest(&there, "s", Update::State { state: Box::new(State { status: Status::Idle, epoch: 3, summary: "details retained".into(), ..State::default() }) }).await;
            hub.harness.ingest(&there, "s", Update::Events { events: vec![Event { id: "e1".into(), seq: 1, at: 1, parent: None, by: None, body: Body::Assistant { text: "existing event".into(), streaming: false } }] }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let (_watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            for status in [Status::Starting, Status::Idle, Status::Exited, Status::Waiting, Status::Running] {
                hub.harness.machine_lost(&hub, "box").await;
                let _ = sent(&hub, &topic, &mut at);
                hub.harness.ingest(&there, "s", open(status, Some(1))).await;
                anyhow::ensure!(hub.harness.info("s").is_some_and(|i| i.status == status), "the owner's actual lifecycle");
                anyhow::ensure!(hub.harness.state("s").is_some_and(|s| s.status == status && s.summary == "details retained"), "stale offline status is reconciled without replacing details");
                let heard = sent(&hub, &topic, &mut at);
                anyhow::ensure!(heard.iter().filter_map(|f| serde_json::from_str::<Value>(f).ok()).any(|v| v.get("t").and_then(Value::as_str) == Some("state") && v.pointer("/state/status") == Some(&json!(status))), "the existing viewer receives the control change: {heard:?}");
                anyhow::ensure!(!heard.iter().any(|f| f == super::super::live::RESYNC || f.contains(r#""t":"open""#)), "the resumed timeline is not reset: {heard:?}");
                let next = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no later copy"))?;
                let frames = next.frames().iter().map(|f| serde_json::from_str::<Value>(f)).collect::<Result<Vec<_>, _>>()?;
                anyhow::ensure!(frames.iter().any(|v| v.pointer("/info/status") == Some(&json!(status))) && frames.iter().any(|v| v.pointer("/state/status") == Some(&json!(status))), "later attach sees coherent Info and State");
                anyhow::ensure!(copy_of(&hub, "s") == ["e1"], "event history is retained");
            }
            hub.harness.machine_lost(&hub, "box").await;
            let _ = sent(&hub, &topic, &mut at);
            hub.harness.ingest(&there, "s", open(Status::Idle, None)).await;
            anyhow::ensure!(sent(&hub, &topic, &mut at).iter().any(|f| f == super::super::live::RESYNC), "a full Open refreshes older viewers from the coherent copy");
            anyhow::ensure!(hub.harness.state("s").is_some_and(|s| s.status == Status::Idle), "the replacement snapshot has fresh lifecycle");
            // An owner can announce lifecycle before it has supplied any State at all.
            hub.harness.ingest(&there, "header", Update::Open { info: Box::new(Info { id: "header".into(), kind: "claude".into(), status: Status::Exited, ..Info::default() }), since: None }).await;
            let header = hub.harness.attach("header").ok_or_else(|| anyhow::anyhow!("no header copy"))?;
            let (_watching, topic, mut at) = (header.viewer, header.topic, header.at.unwrap_or_default());
            hub.harness.ingest(&there, "header", Update::Open { info: Box::new(Info { id: "header".into(), kind: "claude".into(), status: Status::Starting, ..Info::default() }), since: Some(0) }).await;
            anyhow::ensure!(hub.harness.state("header").is_none(), "a header does not fabricate a State");
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.iter().filter_map(|f| serde_json::from_str::<Value>(f).ok()).any(|v| v.get("t").and_then(Value::as_str) == Some("resumed") && v.pointer("/info/status") == Some(&json!("starting"))), "the header-only viewer receives lifecycle: {heard:?}");
            Ok(())
        })
    }

    /// Session `id`'s events as its copy has them: each one's id and words, in order.
    fn said_in(hub: &Hub, id: &str) -> Vec<(String, String)> {
        hub.harness
            .sessions
            .with(id, |v| v.events.iter().map(|e| (e.entry.id().unwrap_or_default().to_owned(), e.entry.value().get("text").and_then(Value::as_str).unwrap_or_default().to_owned())).collect())
            .unwrap_or_default()
    }

    /// What a viewer's frames make of a session's events, taken into `events` (by number): an
    /// `open` starts it over, `events` put each in its place, `stream` adds to one's words.
    fn take_frames<'a>(events: &mut std::collections::BTreeMap<u64, (String, String)>, frames: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
        for f in frames {
            let v: Value = serde_json::from_str(f)?;
            match v.get("t").and_then(Value::as_str) {
                Some("open") => events.clear(),
                Some("events") => {
                    for e in v.get("events").and_then(Value::as_array).into_iter().flatten() {
                        let seq = e.get("seq").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("an event without its number: {e}"))?;
                        let id = e.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                        events.insert(seq, (id, e.get("text").and_then(Value::as_str).unwrap_or_default().to_owned()));
                    }
                }
                Some("stream") => {
                    let (id, text) = (v.get("id").and_then(Value::as_str).unwrap_or_default(), v.get("text").and_then(Value::as_str).unwrap_or_default());
                    let e = events.values_mut().find(|(e, _)| e == id).ok_or_else(|| anyhow::anyhow!("words for an event the viewer hasn't: {id}"))?;
                    e.1.push_str(text);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A viewer of session `id` that joins after `after` (as a page does, taking states and
    /// changesets as patches) and reads its feed, as the server's viewer loop does, until the
    /// updates stop (`done`) and it has the copy's newest revision: the events it ends with, and
    /// how many live frames it took.
    async fn watch(hub: Arc<Hub>, id: String, after: std::time::Duration, done: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<(String, Vec<(String, String)>, usize)> {
        use kiln_seams::Bus as _;
        crate::rt::sleep(after).await;
        let att = hub.harness.attach_from(&id, None, CHANGES, View::default()).ok_or_else(|| anyhow::anyhow!("no copy of {id}"))?;
        let mut events = std::collections::BTreeMap::new();
        take_frames(&mut events, att.frames().iter().map(String::as_str))?;
        let (mut at, mut have, mut live) = (att.at.ok_or_else(|| anyhow::anyhow!("no feed"))?, att.rev, 0);
        loop {
            let batch = hub.bus.read(&att.topic, at, 256).await?;
            if batch.messages.is_empty() {
                if done.load(std::sync::atomic::Ordering::Acquire) && hub.harness.rev(&id).is_some_and(|r| have >= r) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(2)).await;
                continue;
            }
            at = batch.next;
            for m in batch.messages {
                let f = super::super::live::decode(&m.payload).ok_or_else(|| anyhow::anyhow!("a frame the feed didn't make"))?;
                if &*f.text == super::super::live::RESYNC {
                    // Back to the copy, from what it has, as a viewer that fell behind.
                    let (snapshot, from, _, rev) = hub.harness.frames_since(&id, &att.viewer, have, View::default()).ok_or_else(|| anyhow::anyhow!("{id} gone"))?;
                    take_frames(&mut events, snapshot.frames().iter().map(String::as_str))?;
                    (at, have) = (from.ok_or_else(|| anyhow::anyhow!("no feed"))?, rev);
                    break;
                }
                if f.rev != 0 && f.rev <= have {
                    continue;
                }
                take_frames(&mut events, [f.for_viewer(CHANGES)])?;
                have = have.max(f.rev);
                live += 1;
            }
        }
        Ok((id, events.into_values().collect(), live))
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: sixteen sessions' updates handed to their owners from every
    /// shard at once, every shard kept busy, and viewers joining while they come (others joining
    /// and leaving all the while). Each copy takes every event once, in order, with each reply's
    /// words in the order they came; and each viewer, from its snapshot and the frames after it,
    /// ends with what the copy has: none missed, whenever it joined.
    #[test]
    fn updates_keep_their_order_and_none_is_lost_on_busy_shards() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        use std::sync::atomic::{AtomicBool, Ordering};
        let _busy = crate::hub::tests::shards_kept_busy();
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let ids: Vec<String> = (0..16).map(|n| format!("s{n}")).collect();
            for id in &ids {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            // Every shard kept busy: tasks that hold it a step at a time, giving it up only to
            // what wants it.
            let (busy, done) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
            for _ in 0..crate::rt::shard_count().max(1) * 2 {
                let busy = Arc::clone(&busy);
                crate::rt::spawn_spread(async move {
                    while busy.load(Ordering::Acquire) {
                        let t = std::time::Instant::now();
                        while t.elapsed() < std::time::Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                        crate::rt::maybe_yield().await;
                    }
                });
            }
            let per = 100u64;
            let (sent_tx, sent_rx) = async_channel::unbounded::<()>();
            for id in ids.clone() {
                let (hub, there, sent) = (Arc::clone(&hub), there.clone(), sent_tx.clone());
                crate::rt::spawn_spread(async move {
                    for n in 1..=per {
                        let e = format!("{id}-e{n}");
                        let event = Event { id: e.clone(), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: String::new(), streaming: true } };
                        hub.harness.ingest_from(&there, &id, Update::Events { events: vec![event] }, false).await;
                        for part in ["a", "b"] {
                            hub.harness.ingest_from(&there, &id, Update::Stream { id: e.clone(), text: format!("{n}{part}") }, false).await;
                        }
                        // As a runner's updates come: in bursts, a while apart.
                        if n % 2 == 0 {
                            crate::rt::sleep(std::time::Duration::from_millis(3)).await;
                        }
                    }
                    let _ = sent.send(()).await;
                });
            }
            drop(sent_tx);
            // Three viewers join each session at moments of their own while its updates come, and stay.
            let mut viewers = Vec::new();
            for (k, id) in ids.iter().enumerate() {
                for j in 0..3u64 {
                    let after = std::time::Duration::from_millis((u64::try_from(k).unwrap_or(0) * 7 + j * 41) % 140);
                    viewers.push(crate::rt::spawn(watch(Arc::clone(&hub), id.clone(), after, Arc::clone(&done))));
                }
            }
            // Others come and go: the feeds' topics let go and made again, generation by generation.
            for id in ids.iter().step_by(3) {
                let (hub, id, done) = (Arc::clone(&hub), id.clone(), Arc::clone(&done));
                crate::rt::spawn_spread(async move {
                    while !done.load(Ordering::Acquire) {
                        drop(hub.harness.attach_from(&id, None, CHANGES, View::default()));
                        crate::rt::sleep(std::time::Duration::from_millis(1)).await;
                    }
                });
            }
            for _ in 0..ids.len() {
                sent_rx.recv().await?;
            }
            hub.harness.settled().await;
            done.store(true, Ordering::Release);
            busy.store(false, Ordering::Release);
            for id in &ids {
                let want: Vec<(String, String)> = (1..=per).map(|n| (format!("{id}-e{n}"), format!("{n}a{n}b"))).collect();
                anyhow::ensure!(said_in(&hub, id) == want, "{id}'s copy: {:?}", said_in(&hub, id).iter().take(4).collect::<Vec<_>>());
                anyhow::ensure!(hub.harness.rev(id) == Some(1 + per * 3), "{id}'s revision {:?}", hub.harness.rev(id));
            }
            let (count, mut lively) = (viewers.len(), 0);
            for v in viewers {
                let (id, seen, live) = crate::rt::timeout(std::time::Duration::from_secs(30), v).await.map_err(|_| anyhow::anyhow!("a viewer never caught up"))?.map_err(|_| anyhow::anyhow!("a viewer ended"))??;
                anyhow::ensure!(seen == said_in(&hub, &id), "{id}'s viewer has {} events, the copy {}: {:?}", seen.len(), said_in(&hub, &id).len(), seen.iter().zip(said_in(&hub, &id)).find(|(a, b)| *a != b));
                lively += usize::from(live > 0);
            }
            // Most joined while the updates came, and took the rest of them live.
            anyhow::ensure!(lively * 2 > count, "{lively} of {count} viewers took live frames");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: the same through runners' path, each update judged and taken by
    /// its session's owner (`Registry::from_runner`; goal 10, plan 01a1a87b2dae step 4), an update's events
    /// taken in turns of its owner's (goal 9, plan 01a1a87b2cec step 3). Sixteen sessions' runner updates are
    /// handed over from every shard at once, every shard kept busy, every fourth a run of eight events an
    /// owner's turn may run out in, each event's words streamed after it and a state after each: each copy
    /// takes every event once, in order, with its words in the order they came, and its last state.
    #[test]
    fn g4_s5_runner_updates_keep_their_order_and_none_is_lost_on_busy_shards() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        use std::sync::atomic::{AtomicBool, Ordering};
        let _busy = crate::hub::tests::shards_kept_busy();
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let ids: Vec<String> = (0..16).map(|n| format!("r{n}")).collect();
            for id in &ids {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            // Every shard kept busy, giving itself up only to what wants it.
            let busy = Arc::new(AtomicBool::new(true));
            for _ in 0..crate::rt::shard_count().max(1) * 2 {
                let busy = Arc::clone(&busy);
                crate::rt::spawn_spread(async move {
                    while busy.load(Ordering::Acquire) {
                        let t = std::time::Instant::now();
                        while t.elapsed() < std::time::Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                        crate::rt::maybe_yield().await;
                    }
                });
            }
            let per = 60u64;
            let (sent_tx, sent_rx) = async_channel::unbounded::<()>();
            for id in ids.clone() {
                let (hub, sent) = (Arc::clone(&hub), sent_tx.clone());
                crate::rt::spawn_spread(async move {
                    let runner = |update: Update| crate::harness::copies::RunnerUpdate { peer: 7, agent: "box".into(), session: id.clone(), update, shipped: false, arrived: None, handed: None, taken: None, fill: false };
                    let mut n = 0u64;
                    while n < per {
                        let run = if n.is_multiple_of(4) { 8.min(per - n) } else { 1 };
                        let events: Vec<Event> = (n + 1..=n + run).map(|q| Event { id: format!("{id}-e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: String::new(), streaming: true } }).collect();
                        hub.harness.from_runner(runner(Update::Events { events })).await;
                        for q in n + 1..=n + run {
                            for part in ["a", "b"] {
                                hub.harness.from_runner(runner(Update::Stream { id: format!("{id}-e{q}"), text: format!("{q}{part}") })).await;
                            }
                        }
                        n += run;
                        hub.harness.from_runner(runner(Update::State { state: Box::new(State { summary: format!("through {n}"), ..State::default() }) })).await;
                        if n.is_multiple_of(2) {
                            crate::rt::sleep(std::time::Duration::from_millis(2)).await;
                        }
                    }
                    let _ = sent.send(()).await;
                });
            }
            drop(sent_tx);
            for _ in 0..ids.len() {
                sent_rx.recv().await?;
            }
            hub.harness.settled().await;
            busy.store(false, Ordering::Release);
            for id in &ids {
                let want: Vec<(String, String)> = (1..=per).map(|n| (format!("{id}-e{n}"), format!("{n}a{n}b"))).collect();
                anyhow::ensure!(said_in(&hub, id) == want, "{id}'s copy: {:?}", said_in(&hub, id).iter().take(4).collect::<Vec<_>>());
                let summary = hub.harness.sessions.with(id, |v| v.info.summary.clone());
                anyhow::ensure!(summary.as_deref() == Some(format!("through {per}").as_str()), "{id}'s last state: {summary:?}");
            }
            Ok(())
        })
    }

    /// CP-T3.2: a runner's state too large for its frame comes as a view, its queue left out. The copy keeps the
    /// queue it had (no page sees a field left out, nor the view) and takes the view's own fields; the queue the server
    /// read from the runner comes in at the revision the view said, and one read at another revision doesn't.
    #[test]
    fn cp_t3_2_a_bounded_view_keeps_the_copys_fields_until_read() -> anyhow::Result<()> {
        use crate::harness::model::{QueueItem, State, Status};
        use crate::harness::state_view::{FRAME, Metadata, view};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let id = "big".to_owned();
            hub.harness.ingest(&there, &id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let runner = |update: Update, fill: bool| crate::harness::copies::RunnerUpdate { peer: 7, agent: "box".into(), session: id.clone(), update, shipped: false, arrived: None, handed: None, taken: None, fill };
            let item = |i: usize| QueueItem { id: format!("q{i}"), text: "peer report ".repeat(200), at: 1, attachments: Vec::new(), overrides: None, reply_to: None };
            let copy = || hub.harness.sessions.with(&id, |v| (v.state.to_value(), v.info.queued)).ok_or_else(|| anyhow::anyhow!("no copy"));
            let first = State { status: Status::Running, queue: (0..400).map(item).collect(), ..State::default() };
            hub.harness.from_runner(runner(Update::State { state: Box::new(first.clone()) }, false)).await;
            hub.harness.settled().await;
            let mut now = first.clone();
            now.queue.push(item(400));
            now.status = Status::Waiting;
            let bounded: State = serde_json::from_value(view(&now, FRAME - 4096)?.into_state()?)?;
            anyhow::ensure!(bounded.queue.is_empty(), "the view leaves the queue out");
            hub.harness.from_runner(runner(Update::State { state: Box::new(bounded.clone()) }, false)).await;
            hub.harness.settled().await;
            let (doc, queued) = copy()?;
            anyhow::ensure!(doc.get("view").is_none(), "a page never sees the view");
            anyhow::ensure!(doc.get("status") == Some(&serde_json::json!("waiting")), "the view's own fields count");
            anyhow::ensure!(doc.get("queue").and_then(Value::as_array).map(Vec::len) == Some(400), "the queue left out keeps the copy's");
            anyhow::ensure!(queued == 401, "its count is the runner's: {queued}");
            let revision = bounded.view.as_ref().and_then(|v| v.revisions.get("queue").cloned()).unwrap_or_default();
            let read = |revision: String| {
                let view = Metadata { revision: String::new(), revisions: std::collections::BTreeMap::from([("queue".to_owned(), revision)]), deferred: std::collections::BTreeMap::new(), counts: Default::default(), queue_index: None };
                Update::State { state: Box::new(State { queue: now.queue.clone(), view: Some(view), ..State::default() }) }
            };
            hub.harness.from_runner(runner(read("another".into()), true)).await;
            hub.harness.settled().await;
            anyhow::ensure!(copy()?.0.get("queue").and_then(Value::as_array).map(Vec::len) == Some(400), "a read at another revision isn't taken");
            hub.harness.from_runner(runner(read(revision), true)).await;
            hub.harness.settled().await;
            let (doc, queued) = copy()?;
            anyhow::ensure!(doc.get("queue").and_then(Value::as_array).map(Vec::len) == Some(401) && queued == 401, "the queue read comes in");
            anyhow::ensure!(doc.get("status") == Some(&serde_json::json!("waiting")) && doc.get("view").is_none(), "{:?}", doc.get("status"));
            Ok(())
        })
    }

    #[test]
    fn names_are_kept_across_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Registry::new(dir.path().to_owned());
        r.names.update(|n| n.insert("s1".into(), "Mantle".into()));
        r.save_names();
        // Written off the calling thread, soon after (goal 3, plan 01a1a59c2a8e step 5).
        let kept = || Registry::new(dir.path().to_owned()).names.load().get("s1").cloned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while kept().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(kept().as_deref(), Some("Mantle"));
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae step 2: a start's ids are claimed on the starts' copy, swapped in
    /// whole, with no lock: of threads claiming one id at once (four, as many as `crate::rcu`'s tests
    /// race), one has it and the rest find it held; let go, it's free again.
    #[test]
    fn g5_of_starts_claiming_one_id_at_once_one_has_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Arc::new(Registry::new(dir.path().to_owned()));
        let threads = 4;
        for round in 0..50 {
            let id = format!("s{round}");
            // All claim at once, and each holds what it got until all have tried.
            let (start, tried) = (Arc::new(std::sync::Barrier::new(threads)), Arc::new(std::sync::Barrier::new(threads)));
            let claimers: Vec<_> = (0..threads)
                .map(|_| {
                    let (r, id, start, tried) = (Arc::clone(&r), id.clone(), Arc::clone(&start), Arc::clone(&tried));
                    std::thread::spawn(move || {
                        start.wait();
                        let claim = r.claim(std::slice::from_ref(&id));
                        let had = claim.is_ok();
                        tried.wait();
                        drop(claim);
                        had
                    })
                })
                .collect();
            let mut had = 0;
            for c in claimers {
                had += usize::from(c.join().map_err(|_| anyhow::anyhow!("a claimer panicked"))?);
            }
            anyhow::ensure!(had == 1, "round {round}: {had} had {id}");
            anyhow::ensure!(r.claim(std::slice::from_ref(&id)).is_ok(), "round {round}: {id} is free again once let go");
        }
        Ok(())
    }

    /// Goal 3, plan 01a1a59c2a8e step 5 (perf/api-pass's leftover): an agent's name and its epoch
    /// are kept without the caller waiting on the disk. Here their files' writes wait (each `.part`
    /// a named pipe nobody reads yet): keeping a name and raising an epoch return at once all the
    /// same, where they used to wait on the write; read, each pipe then holds what was kept.
    #[cfg(unix)]
    #[test]
    fn g3_s5_names_and_epochs_are_kept_without_waiting_on_the_disk() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for file in ["names.json.part", "epochs.json.part"] {
            let made = std::process::Command::new("mkfifo").arg(dir.path().join(file)).status()?;
            anyhow::ensure!(made.success(), "mkfifo {file}");
        }
        let r = Arc::new(Registry::new(dir.path().to_owned()));
        let (done, kept) = std::sync::mpsc::channel();
        let keeper = Arc::clone(&r);
        std::thread::spawn(move || {
            keeper.names.update(|n| n.insert("s1".into(), "Mantle".into()));
            keeper.save_names();
            keeper.raise_epoch("s1", 7);
            let _ = done.send(());
        });
        anyhow::ensure!(kept.recv_timeout(std::time::Duration::from_secs(10)).is_ok(), "keeping a name and an epoch waited on the disk");
        // Each pipe read: what the writer, waiting on it, was handed.
        let names: HashMap<String, String> = serde_json::from_slice(&std::fs::read(dir.path().join("names.json.part"))?)?;
        anyhow::ensure!(names.get("s1").map(String::as_str) == Some("Mantle"), "{names:?}");
        let epochs: HashMap<String, u64> = serde_json::from_slice(&std::fs::read(dir.path().join("epochs.json.part"))?)?;
        anyhow::ensure!(epochs.get("s1") == Some(&7), "{epochs:?}");
        Ok(())
    }

    /// Every kind of event, with what each may carry and what it may leave out.
    fn every_kind_of_event() -> Vec<crate::harness::model::Event> {
        use crate::harness::model::{Body, Event, FileDiff, Hunk, Level, Todo, ToolCall, ToolKind, ToolStatus};
        let event = |n: u64, parent: Option<&str>, body: Body| Event { id: format!("e{n}"), seq: n, at: 1_793_800_000_000, parent: parent.map(str::to_owned), by: n.is_multiple_of(2).then(|| "Claude Code".to_owned()), body };
        let tool = ToolCall {
            tool_id: "t1".into(),
            name: "mcp__github__search".into(),
            tool: ToolKind::Edit,
            title: "Edit src/lib.rs".into(),
            detail: Some("a \"quoted\" detail\n".into()),
            // A path and a tree inside it, not of the event's own.
            input: json!({"path": "src/lib.rs", "tree": "t", "nested": {"b": 2, "a": [1, 2.5, null, true]}}),
            status: ToolStatus::Done,
            output: Some("ok \u{2713}".into()),
            truncated: true,
            diffs: vec![FileDiff { path: "src/lib.rs".into(), created: true, hunks: vec![Hunk { old_start: 1, old_lines: 0, new_start: 1, new_lines: 1, lines: vec!["+x".into()] }] }],
            todos: vec![Todo { text: "do it".into(), status: "pending".into() }],
            task: Some("t".into()),
            exit_code: Some(-1),
            started: 5,
            ended: Some(6),
            server: Some("github".into()),
            tool_short: Some("search".into()),
            skill: Some("s".into()),
            runner: Some("linux/amd64".into()),
        };
        let bare = ToolCall { detail: None, output: None, truncated: false, diffs: Vec::new(), todos: Vec::new(), task: None, exit_code: None, ended: None, server: None, tool_short: None, skill: None, runner: None, ..tool.clone() };
        vec![
            event(1, None, Body::User { text: "hello \u{1F600}".into(), images: 2, client_id: Some("c1".into()), origin: Some("human".into()), attachments: vec!["a.png".into()], reply_to: Some("e0".into()) }),
            event(2, None, Body::User { text: String::new(), images: 0, client_id: None, origin: None, attachments: Vec::new(), reply_to: None }),
            event(3, Some("t1"), Body::Assistant { text: "line".into(), streaming: true }),
            event(4, Some(""), Body::Thinking { text: "hmm".into(), streaming: false, redacted: true }),
            event(5, None, Body::Tool(Box::new(tool))),
            event(6, Some("t9"), Body::Tool(Box::new(bare))),
            event(7, None, Body::Notice { level: Level::Warn, text: "retrying".into() }),
        ]
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an event written out once, straight from itself, is the text its
    /// JSON tree wrote (the copy's before, and what a runner's digest of it is of), its store row has that
    /// text, and what's looked up of it is what the tree said, for every kind of event. It holds while
    /// serde_json's maps keep their keys in the order they come (`preserve_order`, which kiln's build has
    /// through hyper-quic's qlog): a build that loses that fails here.
    #[test]
    fn g9_s5_an_event_is_written_out_once_as_its_tree_wrote_it() {
        for e in every_kind_of_event() {
            let n = e.seq;
            let v = serde_json::to_value(&e).unwrap_or(Value::Null);
            let tree = Entry::written(v.clone(), serde_json::to_string(&v).unwrap_or_default());
            let mut kept: Option<String> = None;
            let once = Entry::of_event(&e, Some(&mut |t: String| kept = Some(t)));
            assert_eq!(once.text(), tree.text(), "event {n}");
            assert_eq!(kept.as_deref(), Some(tree.text()), "event {n}: its row's text");
            assert_eq!(Entry::of_event(&e, None).text(), tree.text(), "event {n}, kept by no one else");
            assert_eq!((once.id(), once.seq(), once.kind(), once.on_main_thread()), (tree.id(), tree.seq(), tree.kind(), tree.on_main_thread()), "event {n}");
            assert!(once.head.file.is_none() && tree.head.file.is_none(), "event {n}: no file's path or tree of its own");
            assert_eq!(crate::wire::sums::event_text(once.text()), crate::wire::sums::event(&v), "event {n}: a runner's digest of it");
            // Its row is the one the event written out again makes.
            assert_eq!(kept.map(|t| crate::convo::rows::event_written("s", &e, t)), crate::convo::rows::event("s", &e), "event {n}: its row");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3: each piece streamed into an event goes into its JSON where its text ends, and the
    /// JSON is what writing out its tree with the text grown writes, byte for byte, whatever the pieces hold (quotes,
    /// backslashes, control characters, other scripts, a separator JSON lets be), taken where it is or made anew; one whose
    /// JSON serde_json didn't write (spaces, an escaped key) as its tree writes it; one with no text string takes none.
    #[test]
    fn g9_s3_text_streamed_into_an_event_goes_into_its_json_where_it_is() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        let e = Event { id: "e1".into(), seq: 1, at: 1, parent: None, by: None, body: Body::Assistant { text: "Hel".into(), streaming: true } };
        let pieces = ["lo", ", \"world\"", " \\ back\\", "\n\ttab\u{1}\u{1f}\u{7f}", "é 漢字 🙂 \u{2028}", "", "end"];
        let mut tree = serde_json::to_value(&e)?;
        let (mut own, mut shared) = (Entry::of_event(&e, None), Arc::new(Entry::of_event(&e, None)));
        for piece in pieces {
            if let Some(Value::String(t)) = tree.get_mut("text") {
                t.push_str(piece);
            }
            let want = serde_json::to_string(&tree)?;
            anyhow::ensure!(own.append_text(piece) && own.text() == want, "taken where it is: {} against {want}", own.text());
            shared = Arc::new(shared.appended(piece).ok_or_else(|| anyhow::anyhow!("made anew"))?);
            anyhow::ensure!(shared.text() == want && own.value() == tree, "made anew: {}", shared.text());
            anyhow::ensure!((own.id(), own.seq(), own.kind()) == (Some("e1"), Some(1), Some("assistant")), "its head as it was");
        }
        let mut odd = Entry::from_text(&format!(r#"{{ "id" : "x", "te{}u0078t" : "a" , "seq":2}}"#, '\\'));
        anyhow::ensure!(odd.append_text("b") && odd.text() == r#"{"id":"x","text":"ab","seq":2}"#, "as its tree writes it: {}", odd.text());
        anyhow::ensure!(Entry::from_text(r#"{"id":"y","text":5}"#).appended("z").is_none() && !Entry::from_text(r#"{"id":"y","seq":3}"#).append_text("z"), "none to grow");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an entry's head read from its text alone ([`Head::of_text`], no tree made) is
    /// what its JSON's tree says of it ([`Head::of`]), for any text: every kind of event as it's written out, with a
    /// parent and without, and tool events whose input and output hold an id, a number and a parent of their own; a
    /// changeset's files; and texts where a reading of the text alone could part from the tree's: values that aren't
    /// objects, text that isn't JSON or has more after it, fields of other types or given twice, numbers past u64, i64
    /// and f64, escapes and lone surrogates, nesting to serde_json's limit and past it.
    #[test]
    fn g9_s3_an_entrys_head_read_from_its_text_is_what_its_tree_said() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, FileChange, FilePlace, Hunk, ToolCall};
        let mut events = every_kind_of_event();
        // Each tool event again, its input and its output (some 60 KB of JSON written in JSON) holding an id, a number,
        // a parent, a kind, a path and a tree of their own.
        let inner = json!({"id": "inner", "seq": 99, "parent": "p", "kind": "user", "path": "a", "tree": "t", "deeper": [{"id": "deeper", "seq": 7, "parent": null, "path": null}]});
        let held: Vec<Event> = events
            .iter()
            .filter_map(|e| match &e.body {
                Body::Tool(call) => Some(Event { id: format!("{}-held", e.id), body: Body::Tool(Box::new(ToolCall { input: inner.clone(), output: Some(inner.to_string().repeat(400)), ..(**call).clone() })), ..e.clone() }),
                _ => None,
            })
            .collect();
        assert!(held.iter().any(|e| e.parent.is_some()) && held.iter().any(|e| e.parent.is_none()), "tool events with a parent and without");
        events.extend(held);
        let mut texts = Vec::new();
        for e in &events {
            let text = serde_json::to_string(e)?;
            assert_eq!(Head::of_text(&text), Head::of_event(e), "event {}", e.id);
            texts.push(text);
        }
        let file = FileChange {
            path: "src/a.rs".into(),
            tree: Some("/w/kiln".into()),
            created: true,
            deleted: false,
            added: 2,
            removed: 1,
            hunks: vec![Hunk { old_start: 1, old_lines: 1, new_start: 1, new_lines: 2, lines: vec!["-a".into(), "+b".into(), "+\"c\"".into()] }],
            truncated: false,
            at: 1_793_800_000_000,
            by: vec!["main".into(), "t1".into()],
            place: Some(FilePlace::Project),
        };
        texts.push(serde_json::to_string(&file)?);
        texts.push(serde_json::to_string(&FileChange { tree: None, by: Vec::new(), place: None, ..file })?);
        let edges = [
            // Values that aren't objects: nothing found in them, on the main thread.
            "null", "5", "-1", "1.5", "true", "\"s\"", r#""{\"id\":\"x\"}""#, "[]", r#"[1,{"id":"x"}]"#, r#"[{"id":"x","seq":1,"parent":"p"}]"#, "{}", " {} ",
            // Text that isn't JSON, or has more than whitespace after its value.
            "", " ", "{", "}", "[", "nul", "tru", "-", "01", "1.", ".5", "+1", "1e", r#"{"id":}"#, r#"{"id":"a",}"#, "[1,]", "[,1]", r#"{"id" "a"}"#, r#"{"id":"a" "seq":1}"#,
            "{'id':'a'}", "{id:\"a\"}", "{1:2}", r#"{"id":"a"} x"#, r#"{"id":"a"}}"#, r#"{"id":"a"} {}"#, r#"{"id":"a"},"#, "{\"id\":\"a\"}\u{0}", "\u{feff}{\"id\":\"a\"}",
            r#"{"id":"a","x":01}"#, r#"{"id":"a","x":1.}"#, r#"{"id":"a","x":-}"#, r#"{"id":"a","x":NaN}"#, r#"{"id":"a"/*c*/}"#, r#"{"id":"a}"#, "{\"id\":\"a\tb\"}",
            // Whitespace around and between, and after the value.
            " { \"id\" : \"a\" ,\n\t\"seq\" : 1 ,\r\n \"parent\" : null } ", "{\"id\":\"a\"}\n",
            // Each field of another type than it's read as.
            r#"{"id":5,"seq":"5","kind":7,"parent":false,"path":3,"tree":[]}"#,
            r#"{"id":null,"seq":null,"kind":null,"parent":0,"path":{},"tree":true}"#,
            r#"{"id":["a"],"seq":true,"kind":{"kind":"user"},"parent":"","path":[null]}"#,
            r#"{"id":{"id":"x"},"seq":[1],"parent":[],"tree":"t"}"#,
            r#"{"parent":{}}"#,
            // A field given twice: its last value counts.
            r#"{"id":"a","id":"b"}"#, r#"{"id":"a","id":5}"#, r#"{"id":5,"id":"a"}"#, r#"{"seq":1,"seq":"x"}"#, r#"{"seq":"x","seq":2}"#, r#"{"seq":1,"seq":-1}"#,
            r#"{"kind":"user","kind":"x"}"#, r#"{"kind":"x","kind":"tool"}"#, r#"{"parent":"p","parent":null}"#, r#"{"parent":null,"parent":"p"}"#,
            r#"{"path":"a","path":5}"#, r#"{"tree":null,"tree":"t"}"#,
            // Nulls: a null path or tree is there, a null parent is none.
            r#"{"path":null}"#, r#"{"tree":null}"#, r#"{"path":"a","tree":null}"#, r#"{"parent":null}"#, r#"{"id":"a","parent":null,"path":null,"tree":null}"#,
            // Numbers: only an integer from 0 to u64::MAX is a seq.
            r#"{"seq":0}"#, r#"{"seq":-1}"#, r#"{"seq":-0}"#, r#"{"seq":1.0}"#, r#"{"seq":1e3}"#, r#"{"seq":1E3}"#, r#"{"seq":5e-1}"#, r#"{"seq":0.0}"#,
            r#"{"seq":18446744073709551615}"#, r#"{"seq":18446744073709551616}"#, r#"{"seq":-9223372036854775808}"#, r#"{"seq":-9223372036854775809}"#,
            r#"{"seq":99999999999999999999999}"#, r#"{"seq":1e400}"#,
            // Numbers past f64's range, which the tree refuses wherever they are, and numbers near it.
            r#"{"id":"a","x":1e400}"#, r#"{"id":"a","x":-1e400}"#, r#"{"id":"a","x":1.8e308}"#, r#"{"id":"a","x":1.7976931348623157e308}"#, r#"{"id":"a","x":1e-400}"#,
            r#"{"id":"a","x":0e999999999999}"#, r#"{"id":"a","x":1e999999999999}"#, r#"{"id":"a","x":[1e400]}"#,
            // Escapes, characters past ASCII as they are, and lone surrogates, which the tree refuses wherever they are.
            r#"{"id":"a\"bé"}"#, r#"{"id":"\n\t\\\/\b\f\r"}"#, r#"{"id":"😀","path":"é"}"#, "{\"id\":\"é\u{1F600}\u{7f}\"}",
            r#"{"id":"\ud800"}"#, r#"{"id":"a","x":"\ud800"}"#, r#"{"id":"a","x":"\udc00"}"#, r#"{"id":"a","x":"\ud800A"}"#, r#"{"id":"a","x":"\ud800x"}"#,
            r#"{"id":"a","x":"\ud800\n"}"#, r#"{"id":"a","\ud800":1}"#, r#"{"id":"a","x":"\u12"}"#, r#"{"id":"a","x":"\q"}"#,
            // Head fields inside other values: only the top-level object's count.
            r#"{"x":{"id":"inner","seq":1,"parent":"p","kind":"user","path":"a","tree":"b"}}"#,
            r#"{"a":[{"id":"x","parent":"y"}],"id":"outer","b":{"seq":3,"kind":"tool"}}"#,
        ];
        texts.extend(edges.map(str::to_owned));
        // `\u` escapes, each written here as `~` and its four digits, in values and in keys: undone before a key is
        // compared or a string kept, a pair of surrogates read as one character, two leading surrogates refused.
        let unicode = |t: &str| t.replace('~', concat!("\\", "u"));
        let escaped = [
            r#"{"id":"~00e9~0022","kind":"us~0065r","seq":2}"#,
            r#"{"kind":"t~006fol","id":"t~0031","parent":"~0070"}"#,
            r#"{"i~0064":"x","~0073eq":3,"p~0061rent":"q","p~0061th":"f","tre~0065":"g","kin~0064":"n~006ftice"}"#,
            r#"{"id":"~d83d~de00","path":"~D83D~DE00","p~0061rent":null}"#,
            r#"{"id":"a","x":"~d800~0041"}"#,
            r#"{"id":"a","x":"~d800~d800"}"#,
            r#"{"id":"a","x":"~zzzz"}"#,
        ];
        texts.extend(escaped.map(unicode));
        // Nesting to serde_json's limit (128 levels, the top's among them) and past it: at the top, in a value read
        // through, in a head field.
        for n in [1, 64, 126, 127, 128, 200] {
            let (open, close, objects, ends) = ("[".repeat(n), "]".repeat(n), r#"{"a":"#.repeat(n), "}".repeat(n));
            texts.extend([
                format!("{open}{close}"),
                format!(r#"{{"id":"a","x":{open}{close}}}"#),
                format!(r#"{{"id":"a","parent":{open}{close},"seq":1}}"#),
                format!(r#"{{"id":"a","x":{objects}1{ends}}}"#),
                format!(r#"{objects}{{"id":"a"}}{ends}"#),
            ]);
        }
        texts.push("[".repeat(300));
        // An integer of 400 digits, past f64's range, and a fraction of as many, not.
        let digits = "9".repeat(400);
        texts.extend([format!(r#"{{"id":"a","x":{digits}}}"#), format!(r#"{{"id":"a","x":-{digits}}}"#), format!(r#"{{"seq":{digits}}}"#), format!(r#"{{"id":"a","x":0.{digits}}}"#)]);
        for t in &texts {
            let tree = serde_json::from_str::<Value>(t).map(|v| Head::of(&v)).unwrap_or_default();
            assert_eq!(Head::of_text(t), tree, "{t}");
        }
        // A few said outright, so the comparison isn't only of heads with nothing in them.
        assert_eq!(
            Head::of_text(r#"{"x":{"id":"inner","parent":"p"},"id":"a\"bé","seq":18446744073709551615,"kind":"tool","parent":null,"path":null}"#),
            Head { id: Some("a\"bé".into()), seq: Some(u64::MAX), kind: Some("tool".into()), main: true, file: Some(Box::new((None, None))) }
        );
        assert_eq!(
            Head::of_text(r#"{"id":"b","seq":18446744073709551616,"kind":"other","parent":"","tree":"t"}"#),
            Head { id: Some("b".into()), seq: None, kind: Some("other".into()), main: false, file: Some(Box::new((None, Some("t".into())))) }
        );
        assert_eq!(Head::of_text("[1,2]"), Head { main: true, ..Head::default() });
        assert_eq!(
            Head::of_text(&unicode(r#"{"i~0064":"x~0022","~0073eq":3,"kin~0064":"t~006fol","p~0061rent":"q","p~0061th":"f"}"#)),
            Head { id: Some("x\"".into()), seq: Some(3), kind: Some("tool".into()), main: false, file: Some(Box::new((Some("f".into()), None))) }
        );
        // What `IgnoredAny` reads through, refused as the tree refuses it.
        let deep = |n: usize| format!(r#"{{"id":"a","x":{}{}}}"#, "[".repeat(n), "]".repeat(n));
        assert_eq!(Head::of_text(&deep(1)).id.as_deref(), Some("a"));
        for refused in [r#"{"id":"a","x":1e400}"#.to_owned(), r#"{"id":"a","x":"\ud800"}"#.to_owned(), deep(200)] {
            assert_eq!(Head::of_text(&refused), Head::default(), "{refused}");
        }
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a session's owner never waits for the conversation store. With rows waiting for
    /// its writer, one update after another is taken by the owner and its copy has them (the owner waited
    /// between turns, so the second waited for the writer); only once the store holds as much as the owners' inboxes
    /// may does what hands it updates wait, and goes on as the writer lets rows go.
    #[test]
    fn g10_s5_an_owner_never_waits_for_the_conversation_store() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let Some(store) = hub.harness.convo.clone() else { anyhow::bail!("the scratch hub keeps no conversations") };
            let there = Location::Machine("box".into());
            let said = |n: u64| Update::Events { events: vec![Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } }] };
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            let q = store.queue();
            // Rows wait for the writer, within what the inboxes may hold.
            let past = 1;
            q.pretend_waiting(past);
            let within = std::time::Duration::from_secs(10);
            for n in 1..=2 {
                crate::rt::timeout(within, hub.harness.ingest(&there, "s", said(n))).await.map_err(|_| anyhow::anyhow!("update {n} waited for the store"))?;
            }
            anyhow::ensure!(copy_of(&hub, "s") == ["m1", "m2"], "{:?}", copy_of(&hub, "s"));
            // Past the inboxes' bound too: the hand-off waits, until the writer lets rows go. Their bound follows the server's
            // share of its memory (join-burst, item 1), which another test's runner coming may measure again meanwhile: twice
            // it is past it still.
            let more = hub.harness.sessions.inboxes_bound().saturating_mul(2);
            q.pretend_waiting(more);
            {
                let mut handed = std::pin::pin!(hub.harness.ingest_from(&there, "s", said(3), false));
                anyhow::ensure!(futures::poll!(handed.as_mut()).is_pending(), "past what the inboxes may hold: it waits");
                q.pretend_written(more);
                crate::rt::timeout(within, handed).await.map_err(|_| anyhow::anyhow!("the hand-off never went on"))?;
            }
            q.pretend_written(past);
            hub.harness.sessions.applied("s").await;
            anyhow::ensure!(copy_of(&hub, "s") == ["m1", "m2", "m3"], "{:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// join-burst, item 2: what hands a session's owner its updates waits while the copies' sealed texts waiting for the
    /// packer are past their share of what the server may hold, and goes on as the packer lets them go: none dropped,
    /// each in order. Within the share it looks without a write and allocates nothing. The packer's backlog is made past
    /// its bound by bytes counted as waiting (as a sealed chunk's are), twice the bound past it, should another test's
    /// runner measure the share again meanwhile, and let go of however the test ends.
    #[test]
    fn join_burst_the_intake_waits_while_the_packer_is_behind_and_drops_nothing() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        struct Behind(usize);
        impl Drop for Behind {
            fn drop(&mut self) {
                super::super::copies::pretend_packed(self.0);
            }
        }
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |n: u64| Update::Events { events: vec![Event { id: format!("p{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } }] };
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            let within = std::time::Duration::from_secs(10);
            // The backlog's room made once a process, before what's counted.
            let _ = futures::FutureExt::now_or_never(hub.harness.sessions.packed_within());
            let before = crate::os::counted::made();
            let looked = futures::FutureExt::now_or_never(hub.harness.sessions.packed_within());
            anyhow::ensure!(looked.is_some() && crate::os::counted::made().blocks == before.blocks, "within its share: back at once, nothing allocated");
            crate::rt::timeout(within, hub.harness.ingest(&there, "s", said(1))).await.map_err(|_| anyhow::anyhow!("update 1 waited"))?;
            let behind = Behind(super::super::copies::pretend_behind());
            {
                let mut handed = std::pin::pin!(hub.harness.ingest_from(&there, "s", said(2), true));
                anyhow::ensure!(futures::poll!(handed.as_mut()).is_pending(), "the packer behind: the hand-off waits");
                drop(behind);
                crate::rt::timeout(within, handed).await.map_err(|_| anyhow::anyhow!("the hand-off never went on"))?;
            }
            crate::rt::timeout(within, hub.harness.ingest(&there, "s", said(3))).await.map_err(|_| anyhow::anyhow!("update 3 waited"))?;
            anyhow::ensure!(copy_of(&hub, "s") == ["p1", "p2", "p3"], "{:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an update's events are taken while its owner's turn lasts, one at
    /// the least, the rest coming back to take next. Five events by turns that each run out at once take
    /// five turns, and the copy has each once, in order, as one turn would have left it.
    ///
    /// Each turn is a message of its own to the owner, as the owner's own turns are (`super::super::copies::own`), so its
    /// shard is given up between them. Taken in one message, a turn that handed its event off the shard took nothing (judged
    /// long to write out: `super::super::copies::long_to_write`, by what every owner of the process learned), its hand-off
    /// waited for the message to end, and the loop held shard 0 with every test waiting on it (goal 4, plan 01a1a59c2a9c step
    /// 5's macOS runs, 2026-11-06: the owners learned writing on the clock there, the pacer's stops with it). A turn that
    /// takes nothing waits, its shard given up, while events of another session are taken as any are, until the owners judge
    /// the event short again. The turns that take one are the five.
    #[test]
    fn g9_s3_an_update_is_taken_in_turns() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            for id in ["s", "fill"] {
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            }
            let mut next = Some(Ingest { from: there.clone(), id: "s".into(), u: Update::Events { events: (1..=5).map(said).collect() }, shipped: false, reported: String::new(), came: None });
            let (mut took, mut fills) = (Vec::new(), 0);
            // Only bounds a hang: past it the test fails rather than waits forever.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while let Some(i) = next.take() {
                anyhow::ensure!(std::time::Instant::now() < deadline, "turns took nothing for 30 s: {took:?} taken before");
                let h = Arc::clone(&hub);
                let turn = hub
                    .harness
                    .sessions
                    .on("s", move |s| {
                        let had = s.get("s").map_or(0, |m| m.events.len());
                        let rest = h.harness.apply(s, &h, i, &|_| true);
                        (rest, s.get("s").map_or(0, |m| m.events.len()).saturating_sub(had))
                    })
                    .await;
                let Some((rest, n)) = turn else { anyhow::bail!("no owner took the turn") };
                if n > 0 {
                    took.push(n);
                } else {
                    fills += 1;
                    hub.harness.ingest(&there, "fill", Update::Events { events: vec![said(fills)] }).await;
                }
                next = rest;
            }
            anyhow::ensure!(took == [1; 5], "the turns that took events took {took:?} ({fills} took none)");
            let names: Vec<String> = (1..=5).map(|n| format!("m{n}")).collect();
            anyhow::ensure!(copy_of(&hub, "s") == names, "the copy: {:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the rest of an update its owner's turn ran out in goes on before
    /// anything sent after it. A job sent right behind an update of as many events as a copy keeps
    /// (`MAX_EVENTS`, more than a step quantum takes on any machine) finds every one of them in the copy,
    /// however many turns they took.
    #[test]
    fn g9_s3_the_rest_of_an_update_goes_before_anything_newer() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            let rev = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default();
            let n = u64::try_from(MAX_EVENTS)?;
            hub.harness.ingest_from(&there, "s", Update::Events { events: (1..=n).map(said).collect() }, false).await;
            let found = hub.harness.sessions.on("s", |s| s.get("s").map(|m| m.events.len())).await.flatten();
            anyhow::ensure!(found == Some(MAX_EVENTS), "the job behind it found {found:?} of {MAX_EVENTS}");
            let turns = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default().saturating_sub(rev);
            anyhow::ensure!(turns >= 1, "the update's turns: {turns}");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: snapshots in flight hold at most their room: one past it waits its
    /// turn until one in flight is written out; each holds what it was made at, given back as it's written
    /// and the rest as it goes; one larger than the room goes alone into an empty one. The server's room is
    /// its blocking pool's `fs` places of whole snapshots.
    #[test]
    fn g11_s3_snapshots_past_their_room_wait_their_turn() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            static ROOM: std::sync::OnceLock<crate::memory::queue::Room> = std::sync::OnceLock::new();
            let room = ROOM.get_or_init(|| crate::memory::queue::Room::new(&crate::memory::SNAPSHOTS, 1000));
            let mut first = InFlight::take(room, 800).await;
            anyhow::ensure!(room.held() == 800);
            let went = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let next = {
                let went = Arc::clone(&went);
                crate::rt::spawn(async move {
                    let held = InFlight::take(room, 300).await;
                    went.store(true, std::sync::atomic::Ordering::SeqCst);
                    held
                })
            };
            crate::rt::sleep(std::time::Duration::from_millis(50)).await;
            anyhow::ensure!(!went.load(std::sync::atomic::Ordering::SeqCst), "past the room, it waits");
            first.made(900);
            anyhow::ensure!(room.held() == 900, "made, it holds what it was made at");
            first.written(600);
            let next = next.await.map_err(|_| anyhow::anyhow!("the next never went"))?;
            anyhow::ensure!(went.load(std::sync::atomic::Ordering::SeqCst) && room.held() == 600, "its turn once there was room: {}", room.held());
            drop((first, next));
            anyhow::ensure!(room.held() == 0, "the rest given back as each goes");
            let alone = InFlight::take(room, 5000).await;
            anyhow::ensure!(room.held() == 5000, "larger than the room, alone");
            drop(alone);
            anyhow::ensure!(snapshot_bound(26) == 26 * SNAPSHOT_BYTES && SNAPSHOT_BYTES == 5000 * 647, "the server's: its fs places of whole snapshots");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a snapshot's weight, what it takes of the room before it's made, is
    /// its chunks' segments as they're kept (and a quarter of the texts of one not packed) packed, and their
    /// texts as frames, beside its other frames.
    #[test]
    fn g11_s3_a_snapshots_weight_is_its_segments_packed_and_its_texts_as_frames() -> anyhow::Result<()> {
        let mut m = super::super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
        for q in 1..=300u64 {
            m.rev = q;
            m.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": format!("line {q} of what was said")}))));
        }
        let parts = m.view().parts();
        let segments: usize = parts.events.chunks().iter().filter(|c| c.is_full()).filter_map(|c| c.pack().ok().map(|s| s.deflated.len())).sum();
        let texts: usize = parts.events.chunks().iter().map(|c| c.texts_len()).sum();
        let s = Snapshot { parts: m.view().parts(), files: None, store: None };
        anyhow::ensure!(s.weight(false) == texts, "as frames, its texts: {} of {texts}", s.weight(false));
        anyhow::ensure!(s.weight(true) >= segments && s.weight(true) < texts, "packed, its segments: {} of {segments}", s.weight(true));
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A5: a snapshot's weight, reckoned on its viewer's shard, reads
    /// the metas of a chunk only where it has no segment to say its texts (the newest, one the packer hasn't come to), packed or
    /// as frames, its chunks' segments kept in memory or spilled; every chunk's were read. As frames it's still its texts.
    #[test]
    fn g11_s5_a5_a_snapshots_weight_reads_the_metas_of_chunks_without_a_segment_alone() -> anyhow::Result<()> {
        let from_metas = || super::super::copies::TEXTS_FROM_METAS.with(std::cell::Cell::get);
        let tmp = tempfile::tempdir()?;
        let mut m = super::super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
        for q in 1..=500u64 {
            m.rev = q;
            m.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": format!("line {q}: {}", "of what was said ".repeat(usize::try_from(q % 7)?))}))));
        }
        // Its sealed chunks packed but the newest of them (the packer's yet to come to it), kept compact, the oldest spilled.
        let parts = m.view().parts();
        let sealed: Vec<Arc<super::super::copies::Chunk>> = parts.events.chunks().iter().filter(|c| c.is_full()).cloned().collect();
        for c in sealed.iter().take(sealed.len().saturating_sub(1)) {
            c.pack()?;
        }
        let mut events = parts.events.clone();
        events.compact(None);
        let dir = super::super::spill::Dir::open(&tmp.path().join("spill"))?;
        let chunks = events.spillable(super::super::copies::EVENTS_PAGE);
        let segments: Vec<Arc<super::super::packed::Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(events.spilled_where(&chunks, spilled) > 0, "some spilled");
        let without = events.chunks().iter().filter(|c| c.packed().is_none() && !c.is_spilled()).count();
        anyhow::ensure!(without == 2, "the newest and one not packed: {without}");
        let texts: usize = events.chunks().iter().map(|c| c.texts_len()).sum();
        let s = Snapshot { parts: Parts { events, ..m.view().parts() }, files: None, store: None };
        for packed in [true, false] {
            let was = from_metas();
            let weight = s.weight(packed);
            anyhow::ensure!(from_metas() - was == without as u64, "packed {packed}: {} chunks' metas read, of {without} without a segment", from_metas() - was);
            anyhow::ensure!(packed || weight == texts, "as frames, its texts: {weight} of {texts}");
        }
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a snapshot packed as it goes out is, piece after piece, the message
    /// made at once: each piece but the last at least the size asked for, none once it's all out; whatever its
    /// chunks are (packed, spilled, the open one) and its other frames (kept files among them).
    #[test]
    fn g11_s5_a_snapshot_packed_as_it_goes_is_the_one_made_at_once() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut m = super::super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
        for q in 1..=500u64 {
            m.rev = q;
            m.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": format!("line {q}: {}", "of what was said ".repeat(usize::try_from(q % 7)?))}))));
        }
        // Some chunks packed, the oldest of them spilled, the newest open.
        let parts = m.view().parts();
        for c in parts.events.chunks().iter().filter(|c| c.is_full()).take(5) {
            c.pack()?;
        }
        let dir = super::super::spill::Dir::open(&tmp.path().join("spill"))?;
        let mut events = parts.events.clone();
        events.compact(None);
        let chunks = events.spillable(super::super::copies::EVENTS_PAGE);
        let segments: Vec<Arc<super::super::packed::Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(events.spilled_where(&chunks, spilled) > 0, "some spilled");
        let snapshot = || Snapshot { parts: Parts { events: events.clone(), ..m.view().parts() }, files: Some(r#"{"t":"files","files":[]}"#.into()), store: None };
        let whole = snapshot().packed("synced")?;
        for size in [1, 1024, 64 << 10, usize::MAX] {
            let mut packing = snapshot().packing("synced");
            let mut pieces = Vec::new();
            while let Some(piece) = packing.next_piece(size)? {
                pieces.push(piece);
            }
            anyhow::ensure!(pieces.iter().rev().skip(1).all(|p| p.len() >= size), "each but the last at least {size}");
            anyhow::ensure!(pieces.concat() == whole, "pieces of {size}: the message made at once");
            anyhow::ensure!(packing.next_piece(size)?.is_none(), "none once it's all out");
        }
        Ok(())
    }

    /// A real panic, as a bug would raise it (as `crate::sup`'s tests raise one).
    #[allow(clippy::panic)]
    fn injected(what: &str) {
        panic!("injected: {what}")
    }

    /// Every shard kept busy, as the existing step 5 tests keep them: tasks that hold it 200 µs a step, giving it up only to
    /// what wants it, until the flag they're given goes false.
    fn busy_shards() -> Arc<std::sync::atomic::AtomicBool> {
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
        for _ in 0..crate::rt::shard_count().max(1) * 2 {
            let busy = Arc::clone(&busy);
            crate::rt::spawn_spread(async move {
                while busy.load(std::sync::atomic::Ordering::Acquire) {
                    let t = std::time::Instant::now();
                    while t.elapsed() < std::time::Duration::from_micros(200) {
                        std::hint::spin_loop();
                    }
                    crate::rt::maybe_yield().await;
                }
            });
        }
        busy
    }

    /// An update of session `session` as its runner sends it, for its owner to judge and take (`Registry::from_runner`).
    fn from_box(session: &str, update: Update) -> super::super::copies::RunnerUpdate {
        super::super::copies::RunnerUpdate { peer: 7, agent: "box".into(), session: session.to_owned(), update, shipped: false, arrived: None, handed: None, taken: None, fill: false }
    }

    /// Event `q` of session `id` (named `{id}-e{q}`), saying `text`.
    fn said_as(id: &str, q: u64, text: &str) -> crate::harness::model::Event {
        use crate::harness::model::{Body, Event};
        Event { id: format!("{id}-e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: text.to_owned(), streaming: false } }
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: an owner restarted mid-stream, its work on a copy cut short by a panic (a bug in a
    /// change: its shard catches it, and the owner goes on with its next message, `Shard::take`), loses nothing after it and
    /// leaves no viewer behind. A job changes a session's copy and panics before the change goes out, between its runner's
    /// updates: each update after it is taken, in order, with the change the panic cut short; and a viewer that watched
    /// throughout ends with what the copy has (sent back to a snapshot of it: a change half made goes to no one as a frame).
    #[test]
    fn g4_s5_an_owner_goes_on_after_a_change_that_panicked_and_its_viewers_with_it() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |q: u64| said_as("p", q, &format!("line {q}"));
            hub.harness.ingest(&there, "p", Update::Open { info: Box::new(Info { id: "p".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            hub.harness.ingest(&there, "p", Update::Events { events: (1..=5).map(said).collect() }).await;
            // A viewer watching throughout: joined before the panic.
            let done = Arc::new(AtomicBool::new(false));
            let viewer = crate::rt::spawn(watch(Arc::clone(&hub), "p".into(), std::time::Duration::ZERO, Arc::clone(&done)));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while hub.harness.sessions.on("p", |s| s.get("p").map(|m| m.feed.viewers())).await.flatten().unwrap_or(0) == 0 {
                anyhow::ensure!(std::time::Instant::now() < deadline, "the viewer never joined");
                crate::rt::sleep(std::time::Duration::from_millis(2)).await;
            }
            // A change made to the copy, and a panic before it goes out.
            let cut = Arc::new(Entry::of_event(&said(6), None));
            let panics = Msg::Run(Box::new(move |s: &mut Shard| {
                if let Some(m) = s.get_mut("p") {
                    m.rev += 1;
                    m.upsert(cut);
                    injected("a change to a session's copy, cut short before it went out");
                }
            }));
            anyhow::ensure!(hub.harness.sessions.send("p", panics).await, "its owner took the job");
            // Its runner goes on.
            for q in 7..=12 {
                hub.harness.ingest_from(&there, "p", Update::Events { events: vec![said(q)] }, false).await;
            }
            hub.harness.settled().await;
            done.store(true, Ordering::Release);
            let want: Vec<(String, String)> = (1..=12).map(|q| (format!("p-e{q}"), format!("line {q}"))).collect();
            anyhow::ensure!(said_in(&hub, "p") == want, "the copy: {:?}", said_in(&hub, "p"));
            let (_, seen, _) = crate::rt::timeout(std::time::Duration::from_secs(30), viewer).await.map_err(|_| anyhow::anyhow!("the viewer never caught up"))?.map_err(|_| anyhow::anyhow!("the viewer ended"))??;
            anyhow::ensure!(seen == want, "the viewer ends with {:?}", seen.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>());
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: the server's owners started again mid-stream (the server restarted, its copies gone
    /// with it) make no copy of part of a session from what its runner sends before it announces the session again (the
    /// runner is asked for all of it instead). Its Open and whole history then make the copy whole, and the stream goes on
    /// from there: every event once, in order, with its words.
    #[test]
    fn g4_s5_owners_started_again_mid_stream_make_no_copy_of_part_of_a_session() -> anyhow::Result<()> {
        use crate::harness::model::State;
        crate::rt::block_on(async {
            let open = Update::Open { info: Box::new(Info { id: "r".into(), kind: "claude".into(), ..Info::default() }), since: None };
            let line = |q: u64| said_as("r", q, &format!("line {q}"));
            // The first server takes its first twenty.
            let first = crate::hub::tests::scratch_hub().await?;
            first.harness.from_runner(from_box("r", open.clone())).await;
            first.harness.from_runner(from_box("r", Update::Events { events: (1..=20).map(line).collect() })).await;
            first.harness.settled().await;
            anyhow::ensure!(said_in(&first, "r").len() == 20, "the first server's copy: {} events", said_in(&first, "r").len());
            // Started again: new owners, no copies; the stream goes on before its runner announces the session again.
            let again = crate::hub::tests::scratch_hub().await?;
            let stream = |hub: Arc<Hub>, qs: std::ops::RangeInclusive<u64>| async move {
                for q in qs {
                    hub.harness.from_runner(from_box("r", Update::Events { events: vec![said_as("r", q, "")] })).await;
                    hub.harness.from_runner(from_box("r", Update::Stream { id: format!("r-e{q}"), text: format!("line {q}") })).await;
                }
            };
            stream(Arc::clone(&again), 21..=25).await;
            again.harness.from_runner(from_box("r", Update::State { state: Box::new(State { summary: "mid-stream".into(), ..State::default() }) })).await;
            again.harness.settled().await;
            anyhow::ensure!(again.harness.info("r").is_none() && again.harness.list().is_empty(), "a copy made of part of a session: {:?}", said_in(&again, "r"));
            // Its runner announces it again with all of it, as asked, and goes on.
            again.harness.from_runner(from_box("r", open)).await;
            again.harness.from_runner(from_box("r", Update::Events { events: (1..=25).map(line).collect() })).await;
            stream(Arc::clone(&again), 26..=30).await;
            again.harness.settled().await;
            let want: Vec<(String, String)> = (1..=30).map(|q| (format!("r-e{q}"), format!("line {q}"))).collect();
            anyhow::ensure!(said_in(&again, "r") == want, "{:?}", said_in(&again, "r"));
            Ok(())
        })
    }

    /// A test's draws: xorshift64 from a seed other than zero.
    struct Draws(u64);

    impl Draws {
        /// The next draw below `n` (one at the least).
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
        }
    }

    /// What taking `u` does to a copy's events as `model` has them (by id: each one's number and words), taken alone in
    /// its turn: an event replaces the one under its id, words go onto the event under theirs.
    fn modeled(model: &mut std::collections::BTreeMap<String, (u64, String)>, u: &Update) {
        use crate::harness::model::Body;
        match u {
            Update::Events { events } => {
                for e in events {
                    let text = if let Body::Assistant { text, .. } = &e.body { text.clone() } else { String::new() };
                    model.insert(e.id.clone(), (e.seq, text));
                }
            }
            Update::Stream { id, text } => {
                if let Some((_, words)) = model.get_mut(id) {
                    words.push_str(text);
                }
            }
            _ => {}
        }
    }

    /// Session `id`'s updates from its runner with its events out of their numbers' order, drawn from `draws`, taken into
    /// `model` as they'd be taken. Its new events (`top` + 1 on, `fresh` of them) and those its copy lacks among its older
    /// ones (`late`) come in any order, in batches of one to four, and one in ten of sixteen to thirty-nine (an owner's turn
    /// may run out in one, its rest coming back to it: goal 9, plan 01a1a87b2cec step 3), each new one's words streamed after
    /// it in two parts; now and then a batch goes again as the copy has it (a runner's resend); and every eighth batch one
    /// of its `oldest` is said again in other words, and given more words after.
    fn out_of_order(id: &str, draws: &mut Draws, top: u64, fresh: u64, late: &[u64], oldest: &[u64], model: &mut std::collections::BTreeMap<String, (u64, String)>) -> Vec<Update> {
        let mut due: Vec<u64> = (top + 1..=top + fresh).chain(late.iter().copied()).collect();
        for i in (1..due.len()).rev() {
            due.swap(i, draws.below(i + 1));
        }
        let mut updates = Vec::new();
        let mut take = |model: &mut std::collections::BTreeMap<String, (u64, String)>, u: Update| {
            modeled(model, &u);
            updates.push(u);
        };
        let (mut rest, mut batches) = (due.as_slice(), Vec::<Vec<u64>>::new());
        while !rest.is_empty() {
            let size = if draws.below(10) == 0 { 16 + draws.below(24) } else { 1 + draws.below(4) };
            let (batch, after) = rest.split_at(size.min(rest.len()));
            rest = after;
            let events = batch
                .iter()
                .map(|q| {
                    let text = if *q > top { String::new() } else { format!("late {q}") };
                    said_as(id, *q, &text)
                })
                .collect();
            take(model, Update::Events { events });
            for q in batch.iter().filter(|q| **q > top) {
                for part in ["a", "b"] {
                    take(model, Update::Stream { id: format!("{id}-e{q}"), text: format!("{q}{part}") });
                }
            }
            batches.push(batch.to_vec());
            if draws.below(4) == 0 {
                let again = batches.get(draws.below(batches.len())).cloned().unwrap_or_default();
                let events = again.iter().filter_map(|q| model.get(&format!("{id}-e{q}")).map(|(_, words)| said_as(id, *q, words))).collect();
                take(model, Update::Events { events });
            }
            if batches.len().is_multiple_of(8)
                && let Some(q) = oldest.get(draws.below(oldest.len()))
            {
                take(model, Update::Events { events: vec![said_as(id, *q, &format!("said again at batch {}", batches.len()))] });
                take(model, Update::Stream { id: format!("{id}-e{q}"), text: " and more".into() });
            }
        }
        updates
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a runner's events that reach their session's owner out of their numbers' order end
    /// in it, once each. Eight sessions on busy shards, half of them long (their older chunks kept compact, some spilled),
    /// each sent through the owners' runner path (`Registry::from_runner`): new events in batches in any order, some
    /// batches sent again; on the long ones, events their copies lack among their older ones and new words for their
    /// oldest, each such change read back, or the events made again with it in, off the owner's shard while the updates
    /// behind it wait (goal 9, plan 01a1a87b2cec step 3); and words streamed into events old and new. Each copy ends as
    /// taking its updates one at a time, in the order sent, makes it: every event once, in its number's place, with its
    /// newest words; its index finds each where it is; and a viewer of it ends with what it has.
    #[test]
    fn g4_s5_events_that_come_out_of_order_end_in_their_numbers_order_once_each() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _busy = crate::hub::tests::shards_kept_busy();
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            // A copy's chunk (`super::copies`): the square root of the most it keeps. A long copy has six and a few.
            let chunk = u64::try_from((MAX_EVENTS + MAX_EVENTS / 4).isqrt())?;
            let (old, fresh) = (chunk * 6 + 9, 60u64);
            let late: Vec<u64> = (1..=old).filter(|q| q % 11 == 7).collect();
            let oldest: Vec<u64> = (1..=chunk / 2).filter(|q| !late.contains(q)).collect();
            let ids: Vec<String> = (0..8).map(|n| format!("o{n}")).collect();
            let mut plans = Vec::new();
            for (k, id) in ids.iter().enumerate() {
                let long = k.is_multiple_of(2);
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
                let mut model = std::collections::BTreeMap::new();
                if long {
                    let first = Update::Events { events: (1..=old).filter(|q| !late.contains(q)).map(|q| said_as(id, q, &format!("line {q}"))).collect() };
                    modeled(&mut model, &first);
                    hub.harness.ingest(&there, id, first).await;
                    // Its older chunks packed and kept compact, as its owner keeps them once the packer is done.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    while hub.harness.sessions.with(id, |v| v.events.chunks().iter().filter(|c| c.is_compact()).count()).unwrap_or(0) < 3 {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "{id}'s older chunks were never kept compact");
                        crate::rt::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                let mut draws = Draws(0x2545_f491_4f6c_dd1d ^ u64::try_from(k + 1)?.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                let (top, late, oldest) = if long { (old, late.as_slice(), oldest.as_slice()) } else { (0, &[][..], &[][..]) };
                let updates = out_of_order(id, &mut draws, top, fresh, late, oldest, &mut model);
                plans.push((id.clone(), updates, model));
            }
            let busy = busy_shards();
            let done = Arc::new(AtomicBool::new(false));
            let mut viewers = Vec::new();
            for (k, id) in ids.iter().enumerate() {
                viewers.push(crate::rt::spawn(watch(Arc::clone(&hub), id.clone(), std::time::Duration::from_millis(u64::try_from(k)? * 9), Arc::clone(&done))));
            }
            let (sent_tx, sent_rx) = async_channel::unbounded::<()>();
            let mut models = Vec::new();
            for (id, updates, model) in plans {
                let (hub, sent) = (Arc::clone(&hub), sent_tx.clone());
                let session = id.clone();
                crate::rt::spawn_spread(async move {
                    for u in updates {
                        hub.harness.from_runner(from_box(&session, u)).await;
                    }
                    let _ = sent.send(()).await;
                });
                models.push((id, model));
            }
            drop(sent_tx);
            for _ in 0..ids.len() {
                crate::rt::timeout(std::time::Duration::from_secs(120), sent_rx.recv()).await.map_err(|_| anyhow::anyhow!("a runner's updates were never all handed over"))??;
            }
            hub.harness.settled().await;
            done.store(true, Ordering::Release);
            busy.store(false, Ordering::Release);
            for (id, model) in &models {
                let mut want: Vec<(u64, String, String)> = model.iter().map(|(e, (q, words))| (*q, e.clone(), words.clone())).collect();
                want.sort();
                let want: Vec<(String, String)> = want.into_iter().map(|(_, e, words)| (e, words)).collect();
                let got = said_in(&hub, id);
                anyhow::ensure!(got == want, "{id}: {} events of {}, the first out of place {:?}", got.len(), want.len(), got.iter().zip(&want).find(|(a, b)| a != b));
                let i = id.clone();
                let indexed = hub.harness.sessions.on(id, move |s| s.get(&i).map(|m| m.events.metas().enumerate().all(|(at, meta)| m.index.get(&m.events, meta.id()) == Some(at)))).await.flatten();
                anyhow::ensure!(indexed == Some(true), "{id}'s index doesn't find each event where it is");
            }
            for v in viewers {
                let (id, seen, _) = crate::rt::timeout(std::time::Duration::from_secs(30), v).await.map_err(|_| anyhow::anyhow!("a viewer never caught up"))?.map_err(|_| anyhow::anyhow!("a viewer ended"))??;
                let has = said_in(&hub, &id);
                anyhow::ensure!(seen == has, "{id}'s viewer has {} events, the copy {}: {:?}", seen.len(), has.len(), seen.iter().zip(&has).find(|(a, b)| a != b));
            }
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: many sessions on few shards, their runners' updates handed over by one loop, as the
    /// hub's loop hands them over (each after the one before it, round the sessions), the shards busy: two hundred and
    /// fifty-six sessions on the test runtime's owners (two) each take every event once, in order, with its words in the
    /// order they came and its last state; the viewers of every sixteenth end with what their copies have; and the
    /// sessions' list and what the copies say they keep name every one, each through its newest event.
    #[test]
    fn g4_s5_many_sessions_handed_over_by_one_loop_keep_their_order() -> anyhow::Result<()> {
        use crate::harness::model::State;
        use std::sync::atomic::{AtomicBool, Ordering};
        let _busy = crate::hub::tests::shards_kept_busy();
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (count, per) = (256usize, 12u64);
            let ids: Vec<String> = (0..count).map(|n| format!("l{n}")).collect();
            for id in &ids {
                hub.harness.from_runner(from_box(id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None })).await;
            }
            hub.harness.settled().await;
            let busy = busy_shards();
            let done = Arc::new(AtomicBool::new(false));
            let mut viewers = Vec::new();
            for (j, id) in ids.iter().step_by(16).enumerate() {
                viewers.push(crate::rt::spawn(watch(Arc::clone(&hub), id.clone(), std::time::Duration::from_millis(u64::try_from(j)? * 5), Arc::clone(&done))));
            }
            for q in 1..=per {
                for id in &ids {
                    hub.harness.from_runner(from_box(id, Update::Events { events: vec![said_as(id, q, "")] })).await;
                    for part in ["a", "b"] {
                        hub.harness.from_runner(from_box(id, Update::Stream { id: format!("{id}-e{q}"), text: format!("{q}{part}") })).await;
                    }
                    if q.is_multiple_of(4) {
                        hub.harness.from_runner(from_box(id, Update::State { state: Box::new(State { summary: format!("through {q}"), ..State::default() }) })).await;
                    }
                }
            }
            hub.harness.settled().await;
            done.store(true, Ordering::Release);
            busy.store(false, Ordering::Release);
            for id in &ids {
                let want: Vec<(String, String)> = (1..=per).map(|q| (format!("{id}-e{q}"), format!("{q}a{q}b"))).collect();
                anyhow::ensure!(said_in(&hub, id) == want, "{id}'s copy: {:?}", said_in(&hub, id).iter().take(4).collect::<Vec<_>>());
                let summary = hub.harness.sessions.with(id, |v| v.info.summary.clone());
                anyhow::ensure!(summary.as_deref() == Some(format!("through {per}").as_str()), "{id}'s last state: {summary:?}");
            }
            let listed: Vec<Value> = serde_json::from_slice(&hub.harness.list_json())?;
            let kept = hub.harness.kept_on("box");
            anyhow::ensure!(listed.len() == count && kept.len() == count && kept.iter().all(|k| k.seq == per), "{} listed, {} kept, the least kept through {:?}", listed.len(), kept.len(), kept.iter().map(|k| k.seq).min());
            for v in viewers {
                let (id, seen, _) = crate::rt::timeout(std::time::Duration::from_secs(30), v).await.map_err(|_| anyhow::anyhow!("a viewer never caught up"))?.map_err(|_| anyhow::anyhow!("a viewer ended"))??;
                anyhow::ensure!(seen == said_in(&hub, &id), "{id}'s viewer has {} events, the copy {}", seen.len(), said_in(&hub, &id).len());
            }
            Ok(())
        })
    }

    /// What a held view says of its copy, as a viewer and the API would have it: its snapshot's frames (made on the blocking
    /// pool, as a viewer's are), its newest page of events, its digests, its revision and how many events it holds.
    type ViewSays = (Vec<String>, Option<bytes::Bytes>, crate::proto::KeptSession, u64, usize);

    async fn said_by(view: Arc<super::super::copies::View>) -> anyhow::Result<ViewSays> {
        crate::rt::blocking(move || (Snapshot { parts: view.parts(), files: None, store: None }.frames(), view.events_json(None, 50), view.kept("old"), view.rev, view.events.len())).await
    }

    /// Each view in `held` (with what it was taken after) still says what it said then, now that `now`.
    async fn still_say(held: &[(String, Arc<super::super::copies::View>, ViewSays)], now: &str) -> anyhow::Result<()> {
        for (after, view, was) in held {
            let says = said_by(Arc::clone(view)).await?;
            anyhow::ensure!(&says == was, "the view held after {after} says another thing now that {now}: revision {} and {} events then, {} and {} now", was.3, was.4, says.3, says.4);
        }
        Ok(())
    }

    /// Session `old`'s view held, now that `now`: those held before checked first.
    async fn hold_view(hub: &Hub, held: &mut Vec<(String, Arc<super::super::copies::View>, ViewSays)>, now: &str) -> anyhow::Result<()> {
        still_say(held, now).await?;
        let view = hub.harness.sessions.view("old").ok_or_else(|| anyhow::anyhow!("no copy once {now}"))?;
        let says = said_by(Arc::clone(&view)).await?;
        held.push((now.to_owned(), view, says));
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: readers holding old copies keep what they took. A reader holds its session's view
    /// after each of these, while the copy's owner goes on: events taken in (their chunks then sealed, packed, kept compact
    /// and spilled), an old event said again (its chunk read back off the owner's shard), one put in late among the others
    /// (the events made again off the shard), words streamed into the newest, a long session's oldest let go of (the chunk
    /// the cut fell in made again off the shard), its runner going on from before its newest, a reset, and the session
    /// ending. Each view held makes, at each of those after it and once the session is gone, the snapshot, newest page and
    /// digests it made when it was taken; and once the last is let go of, the copy's spill file goes too.
    #[test]
    fn g4_s5_readers_holding_old_copies_keep_what_they_took() -> anyhow::Result<()> {
        let _busy = crate::hub::tests::shards_kept_busy();
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |since: Option<u64>| Update::Open { info: Box::new(Info { id: "old".into(), kind: "claude".into(), ..Info::default() }), since };
            // Its events numbered `r`, but the seventh (put in late).
            let events = |r: std::ops::RangeInclusive<u64>| Update::Events { events: r.filter(|q| *q != 7).map(|q| said_as("old", q, &format!("line {q}"))).collect() };
            // A copy's chunk (`super::copies`): the square root of the most it keeps.
            let chunk = u64::try_from((MAX_EVENTS + MAX_EVENTS / 4).isqrt())?;
            let n = chunk * 7 + 9;
            let mut held = Vec::new();
            hub.harness.ingest(&there, "old", open(None)).await;
            hub.harness.ingest(&there, "old", events(1..=n)).await;
            hold_view(&hub, &mut held, "events were taken in").await?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while hub.harness.sessions.with("old", |v| v.events.chunks().iter().filter(|c| c.is_spilled()).count()).unwrap_or(0) == 0 {
                anyhow::ensure!(std::time::Instant::now() < deadline, "no chunk was ever spilled");
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            hold_view(&hub, &mut held, "its older chunks were spilled").await?;
            hub.harness.ingest(&there, "old", Update::Events { events: vec![said_as("old", 5, "said again")] }).await;
            hold_view(&hub, &mut held, "an old event was said again").await?;
            hub.harness.ingest(&there, "old", Update::Events { events: vec![said_as("old", 7, "late")] }).await;
            hold_view(&hub, &mut held, "one was put in late").await?;
            hub.harness.ingest(&there, "old", Update::Stream { id: format!("old-e{n}"), text: " and more".into() }).await;
            hold_view(&hub, &mut held, "words were streamed into the newest").await?;
            // Past the most a copy keeps, five hundred at a time: its oldest let go of.
            let past = u64::try_from(MAX_EVENTS + MAX_EVENTS / 4)? + 1;
            let mut from = n + 1;
            while from <= past {
                let to = (from + 499).min(past);
                hub.harness.ingest(&there, "old", events(from..=to)).await;
                from = to + 1;
            }
            let first = hub.harness.sessions.with("old", |v| v.events.meta(0).and_then(|m| m.seq())).flatten();
            anyhow::ensure!(first.is_some_and(|q| q > 1), "its oldest weren't let go of: it begins at {first:?}");
            hold_view(&hub, &mut held, "its oldest were let go of").await?;
            hub.harness.ingest(&there, "old", open(Some(past - chunk))).await;
            hold_view(&hub, &mut held, "its runner went on from before its newest").await?;
            hub.harness.ingest(&there, "old", Update::Reset { from: None }).await;
            hold_view(&hub, &mut held, "it was reset").await?;
            hub.harness.ingest(&there, "old", Update::Closed).await;
            anyhow::ensure!(hub.harness.sessions.view("old").is_none(), "the session never ended");
            still_say(&held, "the session ended").await?;
            // Every view let go of: the copy's spill file goes too.
            drop(held);
            let spill = hub.harness.dir.join("spill");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while std::fs::read_dir(&spill)?.count() > 0 {
                anyhow::ensure!(std::time::Instant::now() < deadline, "the spill file outlived the session and its views");
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /* ------------------------------------------------- windows (docs/wire.md, "Windows") */

    /// Words that read as a coding agent's, from a fixed list: text that packs as such text does
    /// (one character repeated packs a thousandfold, which no conversation does).
    fn words(n: usize, salt: u64) -> String {
        const W: [&str; 48] = [
            "fn", "let", "mut", "self", "impl", "pub", "use", "crate", "match", "Some", "None", "Ok", "Err", "return", "struct", "enum", "error:", "warning:", "src/harness/core.rs", "-->", "|",
            "cargo", "test", "running", "passed", "failed", "the", "a", "of", "to", "in", "is", "that", "it", "for", "on", "with", "as", "when", "what", "4,000", "1024", "0x7f", "=>", "{", "}", "();", "\n",
        ];
        let mut x = salt.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = String::with_capacity(n + 16);
        while out.len() < n {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            out.push_str(W.get(((x >> 33) % W.len() as u64) as usize).copied().unwrap_or("x"));
            out.push(' ');
        }
        out.truncate(n);
        out
    }

    fn tool_event(seq: u64, tool: &str, name: &str, status: &str, input: Value, output: usize, diffs: Value) -> Value {
        let mut v = json!({"id": format!("e{seq}"), "seq": seq, "at": seq, "kind": "tool", "tool_id": format!("t{seq}"), "name": name, "tool": tool,
            "title": words(60, seq), "input": input, "status": status, "started": seq, "ended": seq + 1});
        if let Some(o) = v.as_object_mut() {
            if output > 0 {
                o.insert("output".into(), json!(words(output, seq + 7)));
            }
            if tool == "shell" {
                o.insert("exit_code".into(), json!(0));
            }
            if diffs.as_array().is_some_and(|d| !d.is_empty()) {
                o.insert("diffs".into(), diffs);
            }
        }
        v
    }

    /// A long session as kiln's sessions go: each of a person's turns its prompt, ten tool calls
    /// (seven commands, a read, one of kiln's own tools, an edit), four notes and nine thoughts,
    /// sized as twelve of this repository's longest Claude Code transcripts have them on average
    /// (38,570 calls over 3,910 turns: a command's input 721 bytes and output 1,429; a read's
    /// output 6,869; kiln's tools' 600 and 900; an edit's input 1,345; a note 358, a thought 62).
    fn long_session(events: usize) -> Vec<Value> {
        let mut out = Vec::with_capacity(events);
        let mut seq = 0u64;
        let push = |out: &mut Vec<Value>, v: Value| {
            if out.len() < events {
                out.push(v);
            }
        };
        while out.len() < events {
            let mut next = || {
                seq += 1;
                seq
            };
            let n = next();
            push(&mut out, json!({"id": format!("e{n}"), "seq": n, "at": n, "kind": "user", "text": words(200, n)}));
            for k in 0..10u64 {
                let n = next();
                let e = match k {
                    0..=6 => tool_event(n, "shell", "Bash", "done", json!({"command": words(680, n), "description": "Run it"}), 1429, json!([])),
                    7 => tool_event(n, "read", "Read", "done", json!({"file_path": "/src/harness/core.rs"}), 6869, json!([])),
                    8 => tool_event(n, "mcp", "mcp__kiln__run", "done", json!({"command": words(580, n)}), 900, json!([])),
                    _ => {
                        let lines: Vec<String> = (0..24u64).map(|l| format!("{}{}", [" ", "-", "+"].get((l % 3) as usize).copied().unwrap_or(" "), words(40, n + l))).collect();
                        let diffs = json!([{"path": "src/harness/core.rs", "hunks": [{"old_start": 1, "old_lines": 16, "new_start": 1, "new_lines": 16, "lines": lines}]}]);
                        tool_event(n, "edit", "Edit", "done", json!({"file_path": "src/harness/core.rs", "old_string": words(640, n), "new_string": words(660, n + 1)}), 120, diffs)
                    }
                };
                push(&mut out, e);
                if k % 3 == 0 {
                    let n = next();
                    push(&mut out, json!({"id": format!("e{n}"), "seq": n, "at": n, "kind": "assistant", "text": words(358, n)}));
                }
                let n = next();
                push(&mut out, json!({"id": format!("e{n}"), "seq": n, "at": n, "kind": "thinking", "text": words(62, n)}));
            }
        }
        out
    }

    fn mirror_of(events: &[Value]) -> Mirror {
        // Its id is where a shard keeps it (`super::super::copies::Shard::insert`).
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        set_state(&mut m, json!({"status": "idle", "dropped": 0}));
        for e in events {
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(e.clone()))));
        }
        m
    }

    /// The events a viewer's frames carry, and its `open` frame's window.
    fn sent_events(frames: &[String]) -> (Vec<Value>, Option<Value>) {
        let parsed: Vec<Value> = frames.iter().filter_map(|f| serde_json::from_str(f).ok()).collect();
        let window = parsed.iter().find(|f| f.get("t").and_then(Value::as_str) == Some("open")).and_then(|f| f.get("window").cloned());
        let events = parsed.iter().filter(|f| f.get("t").and_then(Value::as_str) == Some("events")).flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
        (events, window)
    }

    fn seqs(events: &[Value]) -> Vec<u64> {
        events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).collect()
    }

    const WINDOWED: View = View { window: true, lean: false };
    const LEAN: View = View { window: true, lean: true };

    fn bounded_parts(m: &Mirror, after: Option<u64>, lean: bool) -> Parts {
        let mut p = m.view().parts_after(after, View { window: true, lean }, true);
        p.history = Some(History { limits: super::super::history::Limits::new(2, 16 * 1024), token: super::super::history::cursor(born(), m.reset_at, "s"), durable: true });
        p
    }

    #[test]
    fn w21_r02_bounded_copies_keep_lean_hydration_resume_and_incremental_gzip() -> anyhow::Result<()> {
        use std::io::Read as _;
        let all = long_session(480);
        let mut m = mirror_of(&all);
        let legacy = Snapshot { parts: m.view().parts_for(View::default(), false), files: None, store: None }.frames();
        assert_eq!(ids(&legacy).len(), all.len());
        assert!(!legacy.iter().any(|f| f.contains("\"t\":\"history\"")));
        let make = || Snapshot { parts: bounded_parts(&m, None, true), files: None, store: None };
        let frames = make().frames();
        let (events, window) = sent_events(&frames);
        assert!(events.len() < all.len());
        assert!(events.iter().any(|e| e.get("held").is_some()), "main's lazy tool bodies are retained");
        let h: Value = frames.iter().filter_map(|f| serde_json::from_str::<Value>(f).ok()).find(|f| f.get("t").and_then(Value::as_str) == Some("history")).ok_or_else(|| anyhow::anyhow!("no history"))?;
        assert_eq!(h.get("before"), window.as_ref().and_then(|w| w.get("before")));
        assert_eq!(h.get("has_more"), Some(&json!(true)));
        let mut packing = make().packing("{\"t\":\"synced\"}");
        let mut packed = Vec::new();
        while let Some(piece) = packing.next_piece(73)? { packed.extend(piece); }
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(packed.as_slice()).read_to_string(&mut decoded)?;
        assert_eq!(decoded, frames.join("\n") + "\n{\"t\":\"synced\"}");
        let old_cursor = h.get("cursor").and_then(Value::as_str).unwrap_or_default().to_owned();
        let streamed = all.iter().rev().find(|e| e.get("kind").and_then(Value::as_str) == Some("assistant")).and_then(|e| e.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let had = m.rev;
        took(&mut m, |m| m.append(&streamed, " streamed"));
        let resumed = Snapshot { parts: bounded_parts(&m, Some(had), true), files: None, store: None }.frames();
        assert_eq!(ids(&resumed), vec![streamed]);
        assert!(!resumed.iter().any(|f| f.contains("\"t\":\"history\"")));
        took(&mut m, |m| m.started_over());
        assert!(!super::super::history::valid_cursor(&old_cursor, born(), m.reset_at, "s"));
        Ok(())
    }

    #[test]
    fn w21_r02_cursor_pages_overlay_canonical_corrections_and_reject_reset() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut m = mirror_of(&[]);
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id":"replacement","seq":2,"kind":"assistant","text":"corrected"})))));
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id":"moved","seq":9,"kind":"assistant","text":"past this page"})))));
            let token = super::super::history::cursor(born(), m.reset_at, "s");
            hub.harness.sessions.on("s", move |s| s.insert("s".into(), m)).await;
            assert!(hub.harness.history_valid("s", &token));
            assert!(!hub.harness.history_valid("other", &token));
            let durable = vec![json!({"id":"deleted","seq":2,"text":"old"}),json!({"id":"earlier","seq":1}),json!({"id":"moved","seq":3,"text":"stale"})];
            let (page, revision, more, floor) = hub.harness.history_overlay("s", &token, 5, 100, durable).await?.ok_or_else(|| anyhow::anyhow!("no page"))?;
            assert_eq!(revision, 2); assert!(!more); assert_eq!(floor, 1);
            assert_eq!(page.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).collect::<Vec<_>>(), vec!["earlier", "replacement"]);
            hub.harness.sessions.on("s", |s| { if let Some(m) = s.get_mut("s") { took(m, |m| m.started_over()); m.publish(); } }).await;
            assert!(!hub.harness.history_valid("s", &token));
            assert!(hub.harness.history_overlay("s", &token, 5, 100, Vec::new()).await?.is_none());
            Ok(())
        })
    }

    /// W21-R15 (an agent's page opens at once): a viewer that asks for a window is sent the newest
    /// ten turns whole, and told where they begin and that there's more before; one from before
    /// windows is sent every event, its `open` as it was.
    #[test]
    fn w21_r15_a_window_is_the_newest_turns_and_says_where_it_begins() {
        let all = long_session(1_000);
        let m = mirror_of(&all);
        let (events, window) = sent_events(&Snapshot { parts: m.view().parts_for(WINDOWED, false), files: None, store: None }.frames());
        let first = events.first().cloned().unwrap_or_default();
        assert!(super::super::window::begins_turn(&first), "it begins with a turn's prompt: {first}");
        assert_eq!(events.iter().filter(|e| super::super::window::begins_turn(e)).count(), super::super::window::TURNS);
        assert_eq!(seqs(&events), seqs(all.get(all.len() - events.len()..).unwrap_or_default()), "the newest, in order");
        assert_eq!(window, Some(json!({"before": first.get("seq"), "more": true})));
        // An older viewer: all of them, and no window.
        let (whole, none) = sent_events(&Snapshot { parts: m.view().parts_for(View::default(), false), files: None, store: None }.frames());
        assert_eq!(whole.len(), all.len());
        assert_eq!(none, None);
        // A short session goes whole, with nothing before it.
        let short = mirror_of(all.get(..30).unwrap_or_default());
        let (few, w) = sent_events(&Snapshot { parts: short.view().parts_for(WINDOWED, false), files: None, store: None }.frames());
        assert_eq!(few.len(), 30);
        assert_eq!(w, Some(json!({"before": 1, "more": false})));
        // With a conversation store that may keep older ones, a window from the copy's first event
        // still has more before it unless that's where the timeline begins.
        assert!(!short.view().window_at(0, true).more, "the copy holds the timeline from its first");
    }

    /// W21-R15: older pages, asked for one after another from where the window begins, bring every
    /// event before it once, with no gap, oldest first in each, while new events come and old ones
    /// change between them; the last says there's no more.
    #[test]
    fn w21_r15_older_pages_meet_with_no_gap_and_nothing_twice() -> anyhow::Result<()> {
        let all = long_session(3_000);
        let mut m = mirror_of(&all);
        let (window, w) = sent_events(&Snapshot { parts: m.view().parts_for(WINDOWED, false), files: None, store: None }.frames());
        let mut have: Vec<u64> = seqs(&window);
        let mut before = w.as_ref().and_then(|w| w.get("before")).and_then(Value::as_u64).unwrap_or(0);
        let mut more = w.as_ref().and_then(|w| w.get("more")).and_then(Value::as_bool).unwrap_or(false);
        let mut next = all.len() as u64;
        let mut pages = 0;
        while more {
            // Between pages: new events, and an old one changed in place.
            next += 1;
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id": format!("e{next}"), "seq": next, "at": next, "kind": "assistant", "text": "new"})))));
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id": "e2", "seq": 2, "at": 2, "kind": "assistant", "text": "changed"})))));
            let Paged::Page { events, window } = m.view().page(before, false, false) else { anyhow::bail!("not a page from the copy") };
            let page: Vec<Value> = events.iter().filter_map(|t| serde_json::from_str(t).ok()).collect();
            let got = seqs(&page);
            assert!(got.windows(2).all(|p| p.first() < p.get(1)), "oldest first: {got:?}");
            assert_eq!(got.last().copied(), Some(before - 1), "it ends just before the last began");
            assert_eq!(window.before, got.first().copied());
            assert!(page.iter().map(|e| e.to_string().len()).sum::<usize>() <= super::super::window::BYTES || page.len() == 1);
            have.splice(0..0, got);
            before = window.before.unwrap_or(0);
            more = window.more;
            pages += 1;
            assert!(pages < 100, "pages end");
        }
        assert_eq!(have, (1..=all.len() as u64).collect::<Vec<_>>(), "every event once, none missing");
        // Before the first: an empty page, nothing more.
        assert!(matches!(m.view().page(1, false, false), Paged::Page { events, window } if events.is_empty() && !window.more));
        // With a store, before where the timeline begins is nowhere; before what the copy has but
        // after that, the store.
        assert!(matches!(m.view().page(1, false, true), Paged::Page { .. }));
        let mut later = mirror_of(all.get(100..200).unwrap_or_default());
        assert!(matches!(later.view().page(101, false, true), Paged::Store { before: 101, floor: 1 }));
        // Cleared there: the timeline begins at 101, and nothing's before it.
        set_state(&mut later, json!({"cleared": 101}));
        assert!(matches!(later.view().page(101, false, true), Paged::Page { events, window } if events.is_empty() && !window.more));
        Ok(())
    }

    /// W21-R15: lean, a settled command, read and edit go without their bodies (each says what it
    /// held back), what shows open as it comes goes whole, and the whole of one comes on demand.
    #[test]
    fn w21_r15_lean_events_hold_their_bodies_back_and_each_comes_whole_on_demand() -> anyhow::Result<()> {
        let mut all = long_session(48);
        all.push(tool_event(49, "shell", "Bash", "running", json!({"command": "sleep 9"}), 400, json!([])));
        let m = mirror_of(&all);
        let (events, _) = sent_events(&Snapshot { parts: m.view().parts_for(LEAN, false), files: None, store: None }.frames());
        let by = |id: &str| events.iter().find(|e| e.get("id").and_then(Value::as_str) == Some(id)).cloned().unwrap_or_default();
        // e2: a command; e9 (or so): the read; the edit with its counts.
        let cmd = by("e2");
        assert!(cmd.get("output").is_none() && cmd.get("input") == Some(&Value::Null), "{cmd}");
        assert_eq!(cmd.pointer("/held/output"), Some(&json!(1429)));
        let edit = events.iter().find(|e| e.get("tool").and_then(Value::as_str) == Some("edit")).cloned().unwrap_or_default();
        assert_eq!(edit.pointer("/diffs/0/hunks"), Some(&json!([])));
        assert_eq!((edit.pointer("/diffs/0/add"), edit.pointer("/diffs/0/del")), (Some(&json!(8)), Some(&json!(8))));
        let mcp = events.iter().find(|e| e.get("tool").and_then(Value::as_str) == Some("mcp")).cloned().unwrap_or_default();
        assert!(mcp.get("output").is_some() && mcp.get("held").is_none(), "kiln's own tools' answers go whole: {mcp}");
        let running = by("e49");
        assert!(running.get("output").is_some() && running.get("held").is_none(), "a running command goes whole");
        // On demand: the whole one, as the copy has it.
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.harness.sessions.on("s", move |s| s.insert("s".into(), m)).await;
            let r = &hub.harness;
            let whole: Value = serde_json::from_str(&r.event_text("s", "e2").await?.unwrap_or_default())?;
            assert_eq!(whole.get("output").and_then(Value::as_str).map(str::len), Some(1429));
            assert_eq!(Some(&whole), all.get(1));
            assert_eq!(r.event_text("s", "nope").await?, None);
            // A page, lean, holds its bodies back too.
            let Some(Paged::Page { events, .. }) = r.page("s", 40, true).await? else { anyhow::bail!("not a page") };
            assert!(events.iter().any(|t| t.contains("\"held\"")));
            Ok(())
        })
    }

    /// W21-R15: a viewer of a window that resumes gets what changed since, lean as it takes them;
    /// one that missed more than a window holds gets a window again, never every event.
    #[test]
    fn w21_r15_a_window_resumes_with_what_it_lacks_and_never_more_than_a_window() {
        let all = long_session(2_000);
        let mut m = mirror_of(&all);
        let had = m.rev;
        took(&mut m, |m| m.upsert(Arc::new(Entry::new(tool_event(2001, "shell", "Bash", "done", json!({"command": "ls"}), 300, json!([]))))));
        let p = m.view().parts_after(Some(had), LEAN, false);
        let (since, window, _) = p.resolved(&mut p.events.sending(p.lean));
        assert!(since.is_some() && window.is_none());
        let (events, _) = sent_events(&Snapshot { parts: p, files: None, store: None }.frames());
        assert_eq!(seqs(&events), vec![2001]);
        assert!(events.first().is_some_and(|e| e.get("held").is_some()), "lean, as it takes them");
        // Away while more than a window's worth came: a window, not all of it.
        let had = m.rev;
        for k in 0..(super::super::window::ROWS as u64 + 10) {
            let n = 3000 + k;
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id": format!("e{n}"), "seq": n, "at": n, "kind": "assistant", "text": "x"})))));
        }
        let p = m.view().parts_after(Some(had), LEAN, false);
        let (since, window, _) = p.resolved(&mut p.events.sending(p.lean));
        assert!(since.is_none() && window.is_some());
        assert!(sent_events(&Snapshot { parts: p, files: None, store: None }.frames()).0.len() <= super::super::window::ROWS);
        // One from before windows resumes with all of what it lacks, as before.
        let p = m.view().parts_after(Some(had), View::default(), false);
        assert!(p.resolved(&mut p.events.sending(p.lean)).0.is_some());
        assert_eq!(sent_events(&Snapshot { parts: p, files: None, store: None }.frames()).0.len(), super::super::window::ROWS + 10);
    }

    /// Goal 9, plan 01a1a87b2cec step 3, and goal 11, plan 01a1a87b2e56 step 5 (g11-heap): a lean window over a long
    /// session's compact chunks is found where its snapshot is written, not where its parts are taken (the socket's
    /// shard, `Registry::attach_from`, as `api::send_snapshot` takes its packing): taking the parts and the packing
    /// reads no chunk again. Making the frames reads each compact chunk the window reaches again a few times (finding
    /// the window, sizing its frames, writing them), where each was read again once for each of its events. The window
    /// is the one found before: the newest ten turns, lean, beginning with a turn's prompt, more before it.
    #[test]
    fn g11_s5_heap_a_lean_window_is_found_where_its_snapshot_is_written() -> anyhow::Result<()> {
        let inflated = || super::super::packed::INFLATED.with(std::cell::Cell::get);
        let all = long_session(1_000);
        let mut m = mirror_of(&all);
        // Its sealed chunks packed and kept compact, as the server's packer does.
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        m.publish();
        let compact = m.events.chunks().iter().filter(|c| c.is_compact()).count();
        anyhow::ensure!(compact > 3, "compact chunks: {compact}");
        let was = inflated();
        let packing = Snapshot { parts: m.view().parts_for(LEAN, false), files: None, store: None }.packing(r#"{"t":"synced"}"#);
        anyhow::ensure!(inflated() == was, "taking its parts and its packing read no chunk again");
        drop(packing);
        let was = inflated();
        let (events, window) = sent_events(&Snapshot { parts: m.view().parts_for(LEAN, false), files: None, store: None }.frames());
        let read = usize::try_from(inflated() - was)?;
        anyhow::ensure!(read > 0 && read <= 3 * compact, "each compact chunk read again a few times at most: {read} for {compact}");
        anyhow::ensure!(read < events.len() / 10, "not once an event: {read} reads for {} events", events.len());
        let first = events.first().cloned().unwrap_or_default();
        anyhow::ensure!(super::super::window::begins_turn(&first), "it begins with a turn's prompt: {first}");
        anyhow::ensure!(events.iter().filter(|e| super::super::window::begins_turn(e)).count() == super::super::window::TURNS, "ten turns");
        anyhow::ensure!(events.iter().any(|e| e.get("held").is_some()), "lean");
        anyhow::ensure!(window == Some(json!({"before": first.get("seq"), "more": true})), "{window:?}");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5, the memory audit's row A2a: the UI's lean window (`?window=1&lean=1`) over a long
    /// session's compact chunks reads each compact chunk it reaches once an attach (inflated once) and each of its events once,
    /// a tool's JSON read whole once for its lean form and another's not at all: packed as it goes out (as `api::send_snapshot`
    /// sends it), made as frames, and as a resume past what a window holds alike; an older page too, and a resume that fits
    /// reads what changed once. An attach read them four times (finding the window, sizing its frames, each frame's room, its
    /// writing), each time every event of those chunks read whole. The events sent are the window's as before.
    #[test]
    fn g11_s5_a2a_a_lean_window_reads_each_compact_chunk_and_event_once() -> anyhow::Result<()> {
        let inflated = || super::super::packed::INFLATED.with(std::cell::Cell::get);
        let parsed = || LEAN_READS.with(std::cell::Cell::get);
        let all = long_session(1_000);
        let mut m = mirror_of(&all);
        // A revision past which more changed than a window holds (`super::window::ROWS`).
        let early = m.rev / 4;
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        m.publish();
        let view = m.view();
        // Each chunk's events (where they begin, how many) and whether it's compact.
        let mut chunks = Vec::new();
        let mut at = 0usize;
        for c in view.events.chunks() {
            chunks.push((at, c.len(), c.is_compact()));
            at += c.len();
        }
        anyhow::ensure!(chunks.iter().filter(|c| c.2).count() > 3, "compact chunks: {chunks:?}");
        // What events `from..to` hold: the compact chunks they reach, and their tool events in compact chunks.
        let of = |from: usize, to: usize| -> (u64, u64) {
            let reached = chunks.iter().filter(|(start, n, compact)| *compact && *start < to && start + n > from).count();
            let compact = |i: usize| chunks.iter().any(|(start, n, compact)| *compact && (*start..start + n).contains(&i));
            let tools = (from..to).filter(|i| compact(*i) && all.get(*i).and_then(|e| e.get("kind")).and_then(Value::as_str) == Some("tool")).count();
            (reached as u64, tools as u64)
        };
        let snapshot = |parts: Parts| Snapshot { parts, files: None, store: None };
        // The window as before, each event lean where it goes so (and the events kept in memory made lean once, here).
        let (want, window) = sent_events(&snapshot(view.parts_for(LEAN, false)).frames());
        let begin = window.as_ref().and_then(|w| w.get("before")).and_then(Value::as_u64).and_then(|s| usize::try_from(s).ok()).map_or(0, |s| s - 1);
        anyhow::ensure!(want.len() == all.len() - begin && want.iter().any(|e| e.get("held").is_some()), "the newest events from {begin}, lean");
        let (reached, tools) = of(begin, all.len());
        anyhow::ensure!(reached >= 2 && tools > 10, "the window reaches {reached} compact chunks, {tools} tools in them");
        // As it's read: `reads` the chunks inflated and the events read whole, `sent` what it sends.
        let counted = |make: &dyn Fn() -> anyhow::Result<Vec<Value>>| -> anyhow::Result<((u64, u64), Vec<Value>)> {
            let (i, p) = (inflated(), parsed());
            let sent = make()?;
            Ok(((inflated() - i, parsed() - p), sent))
        };
        let packed = || -> anyhow::Result<Vec<Value>> {
            let mut packing = snapshot(view.parts_for(LEAN, false)).packing(r#"{"t":"synced"}"#);
            let mut gz = Vec::new();
            while let Some(piece) = packing.next_piece(16 << 10)? {
                gz.extend(piece);
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(gz.as_slice()), &mut text)?;
            Ok(sent_events(&text.lines().map(str::to_owned).collect::<Vec<_>>()).0)
        };
        let frames = || -> anyhow::Result<Vec<Value>> { Ok(sent_events(&snapshot(view.parts_for(LEAN, false)).frames()).0) };
        let resumed = || -> anyhow::Result<Vec<Value>> { Ok(sent_events(&snapshot(view.parts_after(Some(early), LEAN, false)).frames()).0) };
        for (what, make) in [("packed as it goes", &packed as &dyn Fn() -> anyhow::Result<Vec<Value>>), ("as frames", &frames), ("a resume past a window", &resumed)] {
            let ((chunks_read, events_read), sent) = counted(make)?;
            anyhow::ensure!(sent == want, "{what}: the same window");
            // A window ends at a bound inside a turn at its oldest whole turn's start, having read one event before it.
            anyhow::ensure!(chunks_read >= reached && chunks_read <= reached + 1, "{what}: each compact chunk read once: {chunks_read} reads of {reached}");
            anyhow::ensure!(events_read == tools, "{what}: each tool's JSON read once for its lean form, no other's: {events_read} of {tools}");
        }
        // An older page, lean: its chunks and events read once.
        let ((chunks_read, events_read), page) = counted(&|| -> anyhow::Result<Vec<Value>> { match view.page(u64::try_from(begin + 1)?, true, false) {
            Paged::Page { events, .. } => Ok(events.iter().filter_map(|t| serde_json::from_str(t).ok()).collect()),
            Paged::Store { .. } => anyhow::bail!("a page from the store"),
        } })?;
        let page_begin = begin - page.len();
        let (page_reached, page_tools) = of(page_begin, begin);
        anyhow::ensure!(seqs(&page) == (page_begin as u64 + 1..=begin as u64).collect::<Vec<_>>(), "the page just before the window");
        anyhow::ensure!(chunks_read >= page_reached && chunks_read <= page_reached + 1 && events_read == page_tools, "the page: {chunks_read} chunk reads of {page_reached}, {events_read} of {page_tools} tools");
        // A resume that fits a window: what changed alone, read once.
        let had = m.rev;
        took(&mut m, |m| m.upsert(Arc::new(Entry::new(tool_event(1_001, "shell", "Bash", "done", json!({"command": "ls"}), 300, json!([]))))));
        let view = m.view();
        let ((_, events_read), sent) = counted(&|| Ok(sent_events(&snapshot(view.parts_after(Some(had), LEAN, false)).frames()).0))?;
        anyhow::ensure!(seqs(&sent) == vec![1_001] && events_read == 1, "what changed, read once: {events_read}");
        Ok(())
    }

    /// W21-R15: what a long session's first load costs, before windows and after (4,000 events as
    /// `long_session` makes them): its bytes as written and as packed (`?z=gzip`), and its frames.
    /// Run with `--nocapture` to read them.
    #[test]
    fn w21_r15_window_bounds_measured() -> anyhow::Result<()> {
        let all = long_session(super::super::core::MAX_EVENTS);
        let m = mirror_of(&all);
        let measure = |view: View| -> anyhow::Result<(usize, usize, usize, usize)> {
            let snap = Snapshot { parts: m.view().parts_for(view, true), files: None, store: None };
            let frames = snap.frames();
            let raw: usize = frames.iter().map(String::len).sum();
            let packed = snap.packed(r#"{"t":"synced"}"#)?.len();
            Ok((sent_events(&frames).0.len(), frames.len() + 1, raw, packed))
        };
        let before = measure(View::default())?;
        let window = measure(WINDOWED)?;
        let lean = measure(LEAN)?;
        for (what, (events, frames, raw, packed)) in [("whole (today)", before), ("window", window), ("window + lean", lean)] {
            println!("cold open, {what}: {events} events, {frames} frames, {raw} bytes raw, {packed} bytes gzip");
        }
        assert_eq!(before.0, super::super::core::MAX_EVENTS);
        assert!(lean.2 < before.2 / 10, "a window costs a tenth of today's at most: {lean:?} against {before:?}");
        assert!(lean.2 <= super::super::window::BYTES + 64 * 1024, "about a window's bytes: {lean:?}");
        Ok(())
    }

    /// A person's message to an agent whose runner predates mail-flow goes to that runner's inbox,
    /// as it did before W20, and is answered there: the runner never pushes the capacity custody
    /// waits for, so custody refused it ("waiting for capacity") for as long as the runner was old.
    /// An agent's message to it stays in custody's hands (W20's bound). Once the runner speaks
    /// mail-flow, a person's is queued in custody, in a queue of its own, before the agent's
    /// capacity has come; an agent's and kiln's own wait for that capacity.
    #[test]
    fn w20_a_persons_message_reaches_a_runner_from_before_mail_flow_through_its_inbox() -> anyhow::Result<()> {
        use super::super::asked::Taken;
        use super::super::model::SendMode;
        use crate::proto::{AgentMsg, CAP_INBOX, CAP_UNREAD, CAP_WIRE};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::hub_with_runner(crate::rt::clock(), "old-mac", CAP_UNREAD | CAP_WIRE | CAP_INBOX).await?;
            let open = Update::Open { info: Box::new(Info { id: "s1".into(), kind: "claude".into(), ..Default::default() }), since: None };
            crate::hub::tests::hear_item(&hub, 1, "s:s1#a1", 1, &AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(&open)? }).await?;
            anyhow::ensure!(hub.harness.located_on("s1", "old-mac"), "s1 opened on old-mac");
            anyhow::ensure!(super::super::mail_delivery::legacy_owner(&hub, "s1"), "a runner that names no mail-flow is from before it");
            let send = |id: &str| Command::Send { id: id.into(), text: "what is this section?".into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let asked = |scope: &str, cmd: &str, id: &str| {
                let kind = hub.harness.kind_of(&hub, "s1", &send(id));
                match hub.harness.asked.take("s1", scope, cmd, kind, std::time::Instant::now()) {
                    (_, Taken::New(rx)) => Ok(rx),
                    (_, other) => Err(anyhow::anyhow!("{cmd} came before: {other:?}")),
                }
            };
            // A person's, from the page's socket: on to its inbox, which answers it.
            let person = asked("user:ada", "c1", "m1")?;
            hub.harness.send_asked(&hub, "s1", "user:ada", "c1", send("m1")).await;
            anyhow::ensure!(super::super::mail::receipt(&hub.store, "user:ada".into(), "user:ada".into(), "s1".into(), "m1".into()).await?.is_none(), "not held in custody");
            let took = Update::Inbox { said: Box::new(Said { message: "m1".into(), answer: Ok(Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq: 1, repeat: false }) }) };
            crate::hub::tests::hear_item(&hub, 1, "s:s1#a1", 2, &AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(&took)? }).await?;
            let sent = person.await?;
            anyhow::ensure!(matches!(sent, Sent::Accepted(ref a) if a.message == "m1"), "its inbox took it: {sent:?}");
            // Kiln's own and a person's through the API go there too.
            anyhow::ensure!(matches!(hub.harness.dispatch(&hub, "s1", send("m2")).await, Pending::Answer { ref message, .. } if message == "m2"), "to its inbox");
            // An agent's stays with custody, which takes nothing for a runner that pushes no capacity.
            let agent = asked("agent:a1", "c3", "m3")?;
            hub.harness.send_asked(&hub, "s1", "agent:a1", "c3", send("m3")).await;
            let sent = agent.await?;
            anyhow::ensure!(matches!(sent, Sent::PublicationRefused(super::super::mail::Error::Unavailable { .. })), "an agent's is held to custody: {sent:?}");
            // Its runner upgraded, speaking mail-flow, its capacity not come yet: a person's is
            // queued in custody, in the person's own queue; an agent's and kiln's wait for it.
            hub.instances.update(|i| i.set_features(1, vec![crate::proto::feature::MAIL_FLOW.to_owned()]));
            anyhow::ensure!(!super::super::mail_delivery::legacy_owner(&hub, "s1"), "a runner naming mail-flow isn't from before it");
            let person = asked("user:ada", "c4", "m4")?;
            hub.harness.send_asked(&hub, "s1", "user:ada", "c4", send("m4")).await;
            let sent = person.await?;
            anyhow::ensure!(matches!(sent, Sent::Published(ref r) if r.disposition == super::super::mail::Disposition::Waiting), "queued for the agent: {sent:?}");
            let agent = asked("agent:a1", "c5", "m5")?;
            hub.harness.send_asked(&hub, "s1", "agent:a1", "c5", send("m5")).await;
            let sent = agent.await?;
            anyhow::ensure!(matches!(sent, Sent::PublicationRefused(super::super::mail::Error::Unavailable { .. })), "an agent's waits for its capacity: {sent:?}");
            anyhow::ensure!(matches!(hub.harness.dispatch(&hub, "s1", send("m6")).await, Pending::Now(Sent::PublicationRefused(_))), "and kiln's own");
            Ok(())
        })
    }
}
