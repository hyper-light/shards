//! docker/cli's output formatter (cli/command/formatter, v29.8.1), so that shards' commands
//! answer `--format` as the Docker CLI's do: a format is `table`, `json`, `raw`, a Go
//! template, or `table` and a template, whose rows are written through the CLI's
//! tabwriter under a header the template makes of the column names.
//!
//! Each command's module holds its rows as plain data (the fields of moby's API types
//! the CLI reads), its contexts, which a template sees as docker/cli's (same methods, same
//! results, `{{json .}}` as reflect.go's MarshalJSON makes it), its `format` (the CLI's
//! `NewXFormat`) and its `write`:
//! - `container`: `ps` (formatter/container.go, container/list.go);
//! - `image`: `images` (formatter/image.go);
//! - `stats`: `stats` (container/formatter_stats.go);
//! - `history`: `history` (image/formatter_history.go);
//! - `disk`: `system df` and its `-v` (formatter/disk_usage.go, volume.go, buildcache.go).
//!
//! Times are relative to the [`Clock`] given and printed in its zone, where the CLI reads
//! the host's clock and `time.Local`. crates/cmdline/tests/format.rs holds all of it to
//! the CLI's own output (scripts/format/generate). Where shards differs:
//! - Go ranges over maps in random order: a container's networks are sorted here, and an
//!   image's repositories come in the order its names give them;
//! - `{{.}}` and `%v` of a context print `{}`, where Go prints its private fields and
//!   pointers' addresses;
//! - a container without a platform has one that prints `<nil>` but is true in an `if`,
//!   where Go's nil pointer is false; platforms carry no OS version or features, which
//!   only Windows images have, and `stats` has Linux's columns only;
//! - volumes have no cluster volume, so Group, Availability and Status are `N/A`;
//! - a table's header gives a missing column as `<no value>`, as Go does, but a field of
//!   one fails where Go's gives `<no value>` again (tests/format.rs, DEVIATIONS).

mod clock;
pub mod container;
pub mod disk;
pub mod history;
pub mod image;
pub mod network;
mod reference;
pub mod stats;
mod tabwriter;
pub(crate) mod units;
pub mod version;
pub mod volume;

use std::rc::Rc;

use shards_template::{Kind, Object, Template, Value};

pub use clock::{Clock, Zone, rfc3339_at, utc};

/// go-units' BytesSize, as `stats` shows memory.
pub fn units_bytes_size(size: f64) -> String {
    units::bytes_size(size)
}

/// formatter.go's format keys.
pub const TABLE: &str = "table";
pub const RAW: &str = "raw";
pub const JSON: &str = "json";
const DEFAULT_QUIET: &str = "{{.ID}}";
const JSON_FORMAT: &str = "{{json .}}";

/// Format.IsTable: the format starts with `table`.
pub fn is_table(format: &str) -> bool {
    format.starts_with(TABLE)
}

/// Format.IsJSON: the format is `json`.
pub fn is_json(format: &str) -> bool {
    format == JSON
}

/// Format.templateString: `table` taken off, spaces trimmed, and `\t` and `\n` written as
/// two characters made a tab and a newline.
fn template_string(format: &str) -> String {
    match format {
        TABLE => return String::new(),
        JSON => return JSON_FORMAT.into(),
        _ => {}
    }
    let out = format.strip_prefix(TABLE).unwrap_or(format).trim_matches(' ');
    let mut s = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some('t')) => {
                chars.next();
                s.push('\t');
            }
            ('\\', Some('n')) => {
                chars.next();
                s.push('\n');
            }
            _ => s.push(c),
        }
    }
    s
}

/// What formatter.Context holds besides its output: the format (made by a command's
/// `format`), whether to truncate, and how to measure and date what is written.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub format: &'a str,
    pub trunc: bool,
    /// go-runewidth's East Asian width, as the terminal's locale sets it
    /// ([`crate::width::east_asian`]).
    pub east_asian: bool,
    pub clock: &'a Clock<'a>,
}

/// Context.parseFormat.
fn parse(format: &str) -> Result<Template, String> {
    Template::parse("", &template_string(format)).map_err(|e| format!("template parsing error: {e}"))
}

/// Context.Write: each row through the template, then all of them as Context.postFormat
/// writes them. On an error nothing is written, as the CLI writes nothing.
fn write(
    ctx: &Context<'_>,
    header: &'static Header,
    rows: Vec<Value>,
    out: &mut String,
) -> Result<(), String> {
    let tmpl = parse(ctx.format)?;
    let mut buffer = String::new();
    for row in &rows {
        context_format(&tmpl, row, &mut buffer)?;
    }
    post_format(ctx.format, ctx.east_asian, &tmpl, header, &buffer, out);
    Ok(())
}

/// Context.contextFormat.
fn context_format(tmpl: &Template, row: &Value, buffer: &mut String) -> Result<(), String> {
    tmpl.execute_into(row, buffer)
        .map_err(|e| format!("template parsing error: {e}"))?;
    buffer.push('\n');
    Ok(())
}

/// Context.postFormat: a table's header, made by the template with HeaderFunctions (its
/// error ignored, as the CLI ignores it), above the rows, all through the tabwriter
/// (minimum width 10, padding 3); anything else as it is.
fn post_format(
    format: &str,
    east_asian: bool,
    tmpl: &Template,
    header: &'static Header,
    buffer: &str,
    out: &mut String,
) {
    if !is_table(format) {
        out.push_str(buffer);
        return;
    }
    let mut head = String::new();
    let _ = tmpl.execute_header_into(&Value::object(HeaderValue(header)), &mut head);
    let mut tw = tabwriter::Writer::new(10, 3, east_asian);
    tw.write(&head, out);
    tw.write("\n", out);
    tw.write(buffer, out);
    tw.flush(out);
}

/// A context's SubHeaderContext: its methods' names and their columns' titles, sorted by
/// name.
#[derive(Debug)]
struct Header(&'static [(&'static str, &'static str)]);

/// A SubHeaderContext as a template sees it: a `map[string]string` with a `Label` method.
#[derive(Debug, Clone, Copy)]
struct HeaderValue(&'static Header);

impl Object for HeaderValue {
    fn type_name(&self) -> &str {
        "formatter.SubHeaderContext"
    }

    /// A map's key: a missing one is Go's invalid value, which prints `<no value>`.
    fn field(&self, name: &str) -> Option<Value> {
        Some(
            self.0
                .0
                .iter()
                .find(|(k, _)| *k == name)
                .map_or(Value::Nil, |(_, v)| Value::String((*v).into())),
        )
    }

    fn method(&self, name: &str) -> Option<&'static [Kind]> {
        (name == "Label").then_some(&[Kind::String])
    }

    /// SubHeaderContext.Label: the last dotted part of the name, `-` and `_` made spaces.
    fn call(&self, name: &str, args: &[Value]) -> Result<Value, String> {
        match (name, args) {
            ("Label", [Value::String(s)]) => {
                let last = s.rsplit('.').next().unwrap_or_default();
                Ok(Value::String(last.replace(['-', '_'], " ")))
            }
            _ => Err(format!("no method {name}")),
        }
    }

    fn format(&self, out: &mut String) {
        out.push_str("map[");
        for (i, (k, v)) in self.0.0.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(k);
            out.push(':');
            out.push_str(v);
        }
        out.push(']');
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push('{');
        for (i, (k, v)) in self.0.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_string(k, out);
            out.push(':');
            json_string(v, out);
        }
        out.push('}');
        Ok(())
    }
}

/// A formatter context: a Go struct embedding HeaderContext, with exported methods a
/// template calls.
trait Methods: std::fmt::Debug {
    /// Go's name for the pointer to it.
    fn type_name(&self) -> &'static str;
    /// Its exported methods of no arguments but FullHeader, sorted: what MarshalJSON
    /// makes fields of (reflect.go, marshalForMethod).
    const METHODS: &'static [&'static str];
    const HEADER: &'static Header;
    /// Whether it has `Label(name string) string`.
    const LABEL: bool = false;

    /// The value of a method in METHODS.
    fn get(&self, name: &str) -> Option<Value>;

    fn label(&self, name: &str) -> String {
        let _ = name;
        String::new()
    }
}

/// A context as a template sees it. `header` is whether its HeaderContext holds the
/// header, as the one a command makes its header from does.
#[derive(Debug)]
struct Ctx<M> {
    m: M,
    header: bool,
}

impl<M: Methods + 'static> Ctx<M> {
    fn value(m: M) -> Value {
        Value::Object(Rc::new(Ctx { m, header: false }))
    }

    fn header_value(&self) -> Value {
        if self.header {
            Value::object(HeaderValue(M::HEADER))
        } else {
            Value::Nil
        }
    }
}

impl<M: Methods + 'static> Object for Ctx<M> {
    fn type_name(&self) -> &str {
        self.m.type_name()
    }

    /// HeaderContext's field.
    fn field(&self, name: &str) -> Option<Value> {
        (name == "Header").then(|| self.header_value())
    }

    fn method(&self, name: &str) -> Option<&'static [Kind]> {
        if name == "Label" && M::LABEL {
            return Some(&[Kind::String]);
        }
        (name == "FullHeader" || M::METHODS.contains(&name)).then_some(&[])
    }

    fn call(&self, name: &str, args: &[Value]) -> Result<Value, String> {
        match (name, args) {
            ("Label", [Value::String(s)]) => Ok(Value::String(self.m.label(s))),
            ("FullHeader", []) => Ok(self.header_value()),
            (_, []) => self.m.get(name).ok_or_else(|| format!("no method {name}")),
            _ => Err(format!("no method {name}")),
        }
    }

    fn format(&self, out: &mut String) {
        out.push_str("{}");
    }

    /// formatter.MarshalJSON: an object of the methods' values, by json.Marshal, which
    /// escapes `<`, `>` and `&`.
    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push('{');
        for (i, name) in M::METHODS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_string(name, out);
            out.push(':');
            match self.m.get(name) {
                Some(Value::String(s)) => json_string(&s, out),
                Some(Value::Object(o)) => o.json(out)?,
                _ => out.push_str("null"),
            }
        }
        out.push('}');
        Ok(())
    }
}

/// encoding/json's string, as json.Marshal writes it: HTML's characters escaped.
fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            _ if c < ' ' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            _ => out.push(c),
        }
    }
    out.push('"');
}

/// formatter.TruncateID (moby stringid.TruncateID): after the algorithm, 12 bytes.
fn truncate_id(id: &str) -> String {
    let id = id.split_once(':').map_or(id, |(_, rest)| rest);
    let mut end = id.len().min(12);
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    id.get(..end).unwrap_or_default().to_string()
}

/// Labels as `k=v`, sorted, joined by commas.
fn join_labels(labels: &std::collections::BTreeMap<String, String>) -> String {
    let mut all: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    all.sort();
    all.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_become_templates_as_the_cli_makes_them() {
        assert_eq!(
            template_string("table {{.ID}}\\t{{.Names}}"),
            "{{.ID}}\t{{.Names}}"
        );
        assert_eq!(template_string("json"), "{{json .}}");
        assert_eq!(template_string("table"), "");
        assert_eq!(template_string("  a\\\\n "), "a\\\n");
        assert!(is_table("tablefoo"));
        assert_eq!(truncate_id("sha256:0123456789abcdef"), "0123456789ab");
        let mut s = String::new();
        json_string("<none>&\u{1}", &mut s);
        assert_eq!(s, r#""\u003cnone\u003e\u0026\u0001""#);
    }
}
