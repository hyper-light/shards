//! Flags and arguments as docker/cli v29.8.1 reads them. spf13/pflag v1.0.10 parses the
//! flags (flag.go, errors.go); spf13/cobra v1.10.2 then checks, in this order, `--help`,
//! the count of arguments, and the flags the daemon cannot serve (command.go execute;
//! docker/cli cmd/docker/docker.go areFlagsSupported). The CLI words the mistakes
//! (cli/cobra.go FlagErrorFunc, cli/required.go) and `--help` (cli/cobra.go
//! usageTemplate, with pflag's FlagUsagesWrapped).

use std::fmt::Write as _;

use crate::go;

/// What a flag holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// True or false; given alone, true.
    Bool,
    /// A number, read as Go's `strconv.ParseInt(s, 0, 64)` reads it.
    Int,
    /// Text; the last given counts.
    String,
    /// Text given any number of times, each kept; the name is its type in `--help`.
    Many(&'static str),
    /// A time, read as Go's `time.ParseDuration` reads it, in nanoseconds; shown as
    /// `Duration.String` shows it.
    Duration,
    /// One value of a type of docker/cli's own (a pflag `Var`); the name is its type in
    /// `--help`: `bytes` (MemBytes, kept as the bytes) or `decimal` (NanoCPUs, kept as
    /// billionths).
    Value(&'static str),
}

/// A command's flag.
#[derive(Clone, Copy, Debug)]
pub struct Flag {
    pub name: &'static str,
    pub short: Option<u8>,
    pub kind: Kind,
    /// Its value when not given, as pflag prints it.
    pub default: &'static str,
    pub usage: &'static str,
    /// Why it is deprecated, which using it prints; deprecated flags are hidden.
    pub deprecated: Option<&'static str>,
    /// Why its shorthand is deprecated, which using the shorthand prints.
    pub short_deprecated: Option<&'static str>,
    /// Left out of `--help`.
    pub hidden: bool,
    /// Whether shards does what it asks. The rest parse as `docker`'s do, are left out of
    /// `--help` as dockerd's unsupported ones are, and refuse any value but their default.
    pub supported: bool,
    /// The flag whose value this one sets too: the CLI binds both to one variable
    /// (`stop --time` and `--timeout`).
    pub shares: Option<&'static str>,
    /// One of shards' own, which `docker` does not have: `--help` lists it apart, after
    /// `docker`'s, whose text stays the CLI's.
    pub extension: bool,
    /// Its value shows as nothing, whatever it holds (a pflag Value whose `String` is
    /// empty: buildx's `--check`).
    pub unshown: bool,
}

impl Flag {
    const fn new(
        name: &'static str,
        short: Option<u8>,
        kind: Kind,
        default: &'static str,
        usage: &'static str,
    ) -> Flag {
        Flag {
            name,
            short,
            kind,
            default,
            usage,
            deprecated: None,
            short_deprecated: None,
            hidden: false,
            supported: true,
            shares: None,
            extension: false,
            unshown: false,
        }
    }

    pub const fn bool(name: &'static str, short: Option<u8>, usage: &'static str) -> Flag {
        Flag::new(name, short, Kind::Bool, "false", usage)
    }

    pub const fn int(
        name: &'static str,
        short: Option<u8>,
        default: &'static str,
        usage: &'static str,
    ) -> Flag {
        Flag::new(name, short, Kind::Int, default, usage)
    }

    pub const fn string(
        name: &'static str,
        short: Option<u8>,
        default: &'static str,
        usage: &'static str,
    ) -> Flag {
        Flag::new(name, short, Kind::String, default, usage)
    }

    pub const fn duration(name: &'static str, short: Option<u8>, usage: &'static str) -> Flag {
        Flag::new(name, short, Kind::Duration, "0s", usage)
    }

    pub const fn value(
        name: &'static str,
        short: Option<u8>,
        type_name: &'static str,
        usage: &'static str,
    ) -> Flag {
        Flag::new(name, short, Kind::Value(type_name), "", usage)
    }

    pub const fn many(
        name: &'static str,
        short: Option<u8>,
        type_name: &'static str,
        usage: &'static str,
    ) -> Flag {
        Flag::new(name, short, Kind::Many(type_name), "", usage)
    }

    /// The flag with default `default`.
    pub const fn defaulting(mut self, default: &'static str) -> Flag {
        self.default = default;
        self
    }

    /// The flag deprecated for `why`.
    pub const fn deprecated(mut self, why: &'static str) -> Flag {
        self.deprecated = Some(why);
        self.hidden = true;
        self
    }

    /// The flag with its shorthand deprecated for `why`.
    pub const fn short_deprecated(mut self, why: &'static str) -> Flag {
        self.short_deprecated = Some(why);
        self
    }

    /// The flag as one of shards' own (`Flag::extension`).
    pub const fn extension(mut self) -> Flag {
        self.extension = true;
        self
    }

    pub const fn hidden(mut self) -> Flag {
        self.hidden = true;
        self
    }

    /// The flag with its value shown as nothing ([`Flag::unshown`]).
    pub const fn unshown(mut self) -> Flag {
        self.unshown = true;
        self
    }

    /// The flag bound to the same value as flag `other`.
    pub const fn sharing(mut self, other: &'static str) -> Flag {
        self.shares = Some(other);
        self
    }

    /// The flag as one shards does not serve yet.
    pub const fn unsupported(mut self) -> Flag {
        self.supported = false;
        self.hidden = true;
        self
    }

    /// Its names as pflag's messages show them: `-s, --signal`, or `--time`.
    fn names(&self) -> String {
        match self.short {
            Some(s) if self.short_deprecated.is_none() => {
                format!("-{}, --{}", char::from(s), self.name)
            }
            _ => format!("--{}", self.name),
        }
    }

    /// Whether pflag leaves the default out of `--help` (flag.go defaultIsZeroValue).
    fn default_is_zero(&self) -> bool {
        match self.kind {
            Kind::Bool => matches!(self.default, "false" | ""),
            Kind::Int => self.default == "0",
            Kind::String => self.default.is_empty(),
            Kind::Many(_) => matches!(self.default, "false" | "<nil>" | "" | "0"),
            Kind::Duration => matches!(self.default, "0" | "0s"),
            Kind::Value(_) => matches!(self.default, "false" | "<nil>" | "" | "0"),
        }
    }
}

/// How many arguments a command takes (docker/cli cli/required.go).
#[derive(Clone, Copy, Debug)]
pub enum Args {
    Any,
    None,
    AtLeast(usize),
    Exactly(usize),
    /// At least the first, at most the second (docker/cli RequiresRangeArgs).
    Range(usize, usize),
    /// At most (docker/cli RequiresMaxArgs).
    AtMost(usize),
}

/// A command: what `--help` and the mistakes it answers say of it, and what it takes.
#[derive(Clone, Copy, Debug)]
pub struct Command {
    /// What follows its name in its usage line: `[OPTIONS] CONTAINER [CONTAINER...]`.
    pub usage: &'static str,
    pub about: &'static str,
    /// The command lines that name it, as `--help` lists them.
    pub aliases: &'static str,
    pub args: Args,
    /// The flags shards serves.
    pub flags: &'static [Flag],
    /// The flags `docker` takes that shards does not serve yet, which parse all the same:
    /// a line each of `NAME SHORT KIND DEFAULT SHARES`, `-` for no shorthand, an empty
    /// default or no flag shared, and the kind `b`, `i`, `s` or `m` (`Kind`). One string
    /// rather than a table of `Flag`s, whose `&str`s filled a page of fixed-up pointers
    /// that every parse faulted in: 7 µs of the thin client's start (docs/research/
    /// platform-measurements.md M29).
    pub unserved: &'static str,
    /// Whether flags may follow its arguments: pflag's `interspersed`, off for commands
    /// whose arguments are another command's (`run`).
    pub interspersed: bool,
    /// What its errors start with, other than a flag's: `ERROR: ` for buildx's, which
    /// its main prints so (cmd/buildx/main.go), nothing for the CLI's own.
    pub error_prefix: &'static str,
}

/// `v` as Go's `encoding/csv` writes one record, without its line end (pflag's
/// `writeAsCSV`): a field is quoted, its quotes doubled, if it holds a comma, a quote or a
/// line break, or starts with a space.
/// A CSV record's fields, as encoding/csv reads one line (pflag's readAsCSV): commas
/// part them, a quoted field keeps its commas, and a doubled quote in it is one.
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (false, ',') => fields.push(std::mem::take(&mut field)),
            (false, '"') if field.is_empty() => quoted = true,
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (_, c) => field.push(c),
        }
    }
    fields.push(field);
    fields
}

fn csv_record(v: &[String]) -> String {
    let fields: Vec<String> = v
        .iter()
        .map(|f| {
            let quote = f == r"\."
                || f.contains([',', '"', '\r', '\n'])
                || f.chars().next().is_some_and(char::is_whitespace);
            if quote {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect();
    fields.join(",")
}

/// The flag an `unserved` line describes.
fn unserved_flag(line: &'static str) -> Option<Flag> {
    let mut fields = line.split(' ');
    let (name, short, kind, default, shares) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    let none = |field: &'static str| (field != "-").then_some(field);
    let kind = match kind {
        "b" => Kind::Bool,
        "i" => Kind::Int,
        "s" => Kind::String,
        _ => Kind::Many("list"),
    };
    let flag = Flag::new(
        name,
        none(short).and_then(|s| s.bytes().next()),
        kind,
        none(default).unwrap_or_default(),
        "",
    );
    let flag = match none(shares) {
        Some(owner) => flag.sharing(owner),
        None => flag,
    };
    Some(flag.unsupported())
}

/// A value given for a flag.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Bool(bool),
    Int(i64),
    Text(String),
    Many(Vec<String>),
}

/// A command line that asks for its command: the flags it set, and its arguments.
#[derive(Clone, Debug)]
pub struct Parsed {
    command: &'static Command,
    /// The command's flags, then the ones it does not serve that the command line named.
    flags: Vec<Flag>,
    /// What each flag holds, by the flag that owns the value (`Flag::shares`).
    values: Vec<Option<Value>>,
    /// Whether the command line set each flag.
    set: Vec<bool>,
    pub args: Vec<String>,
    /// What pflag said of the deprecated flags used. Cobra prints it on the command's
    /// output, which the CLI makes stdout (spf13/cobra command.go ParseFlags and Print).
    pub notices: String,
}

impl Parsed {
    fn index(&self, name: &str) -> Option<usize> {
        self.flags.iter().position(|f| f.name == name)
    }

    /// The index of flag `find`, a served one or one of `unserved`, which joins `flags`.
    fn find(&mut self, find: impl Fn(&Flag) -> bool) -> Option<usize> {
        if let Some(i) = self.flags.iter().position(&find) {
            return Some(i);
        }
        let flag = self
            .command
            .unserved
            .lines()
            .filter_map(unserved_flag)
            .find(|f| find(f))?;
        self.flags.push(flag);
        self.values.push(None);
        self.set.push(false);
        Some(self.flags.len() - 1)
    }

    /// The index of the flag that owns flag `index`'s value.
    fn owner(&self, index: usize) -> usize {
        self.flags
            .get(index)
            .and_then(|f| f.shares)
            .and_then(|name| self.index(name))
            .unwrap_or(index)
    }

    fn value(&self, name: &str) -> Option<&Value> {
        let index = self.index(name)?;
        self.values.get(self.owner(index)).and_then(Option::as_ref)
    }

    fn default(&self, name: &str) -> &'static str {
        self.index(name)
            .and_then(|i| self.flags.get(i))
            .map_or("", |f| f.default)
    }

    /// Whether the command line set flag `name`, to anything.
    pub fn changed(&self, name: &str) -> bool {
        self.index(name)
            .and_then(|i| self.set.get(i))
            .copied()
            .unwrap_or(false)
    }

    /// The flags the command line set, in name order, each with its value as pflag's
    /// `Value.String` shows it.
    pub fn given(&self) -> Vec<(&'static str, String)> {
        let mut given: Vec<(&'static str, String)> = self
            .flags
            .iter()
            .filter(|f| self.changed(f.name))
            .map(|f| {
                let shown = match self.value(f.name) {
                    _ if f.unshown => String::new(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Int(n)) if f.kind == Kind::Duration => crate::gotime::format_duration(*n),
                    Some(Value::Int(n)) => n.to_string(),
                    Some(Value::Text(s)) if f.kind == Kind::Value("bytes") => {
                        crate::resources::mem_bytes_string(s.parse().unwrap_or(0))
                    }
                    Some(Value::Text(s)) if f.kind == Kind::Value("decimal") => {
                        crate::resources::nano_cpus_string(s.parse().unwrap_or(0))
                    }
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::Many(v)) if v.is_empty() => String::new(),
                    // NetworkOpt prints as nothing, whatever it holds.
                    Some(Value::Many(_)) if matches!(f.kind, Kind::Many("network")) => String::new(),
                    // pflag's IP and IP network slices print joined by commas.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("ipSlice" | "ipNetSlice")) => {
                        format!("[{}]", v.join(","))
                    }
                    // pflag's string slices and arrays print as one CSV record.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("stringArray" | "strings")) => {
                        format!("[{}]", csv_record(v))
                    }
                    // UlimitOpt keeps the last of each resource, and prints them sorted.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("ulimit")) => {
                        let mut by_name = std::collections::BTreeMap::new();
                        // Each as go-units' Ulimit.String writes it: `name=soft:hard`.
                        for u in v {
                            let shown = crate::buildflags::parse_ulimit(u)
                                .map_or_else(|_| u.clone(), |u| u.to_string());
                            by_name.insert(u.split_once('=').map_or(u.as_str(), |(n, _)| n), shown);
                        }
                        let mut shown: Vec<String> = by_name.into_values().collect();
                        shown.sort_unstable();
                        format!("[{}]", shown.join(" "))
                    }
                    // MountOpt prints each mount's type, source and target.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("mount")) => {
                        crate::mounts::mounts_string(v)
                    }
                    // MapOpts prints its map as fmt's %v does: keys sorted, the last of each.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("map")) => {
                        let mut by_key = std::collections::BTreeMap::new();
                        for kv in v {
                            let (k, val) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
                            by_key.insert(k, val);
                        }
                        let shown: Vec<String> = by_key.iter().map(|(k, v)| format!("{k}:{v}")).collect();
                        format!("map[{}]", shown.join(" "))
                    }
                    // FilterOpt prints its map as JSON, its names and values sorted.
                    Some(Value::Many(v)) if matches!(f.kind, Kind::Many("filter")) => filter_json(v),
                    Some(Value::Many(v)) => format!("[{}]", v.join(" ")),
                    None => String::new(),
                };
                (f.name, shown)
            })
            .collect();
        given.sort_by_key(|(name, _)| *name);
        given
    }

    /// The value of bool flag `name`.
    pub fn bool(&self, name: &str) -> bool {
        match self.value(name) {
            Some(Value::Bool(b)) => *b,
            _ => go::parse_bool(self.default(name)).unwrap_or(false),
        }
    }

    /// The value of int flag `name`.
    pub fn int(&self, name: &str) -> i64 {
        match self.value(name) {
            Some(Value::Int(n)) => *n,
            _ => go::parse_int(self.default(name)).unwrap_or(0),
        }
    }

    /// The value of text flag `name`.
    pub fn string(&self, name: &str) -> &str {
        match self.value(name) {
            Some(Value::Text(s)) => s,
            _ => self.default(name),
        }
    }

    /// The values given for flag `name`, in order.
    pub fn many(&self, name: &str) -> &[String] {
        match self.value(name) {
            Some(Value::Many(v)) => v,
            _ => &[],
        }
    }
}

/// What a command line asks for.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Its command.
    Run(Parsed),
    /// Help (`help`), for stdout after `notices`; the status is 0.
    Help { notices: String },
    /// Nothing it can have: `notices` for stdout, then `text` and a newline for stderr,
    /// and the status to exit with.
    Fail {
        notices: String,
        text: String,
        status: u8,
    },
}

/// A flag error, which the CLI answers with 125 and the usage (cli/cobra.go FlagErrorFunc).
const FLAG_ERROR: u8 = 125;

/// A command's usage line, `path` being its name with its parents' (`shards container ls`):
/// cobra's UseLine.
fn use_line(command: &Command, path: &str) -> String {
    format!("{path} {}", command.usage).trim_end().to_string()
}

/// Reads `argv`, the words after the command's name, for `command`, which `path` names
/// with its parents' (`shards stop`, `shards container ls`). `validate` may reject or
/// rewrite a value given for a `Many` flag, as the CLI's `ListOpts` validators do (`-e`).
pub fn parse(
    command: &'static Command,
    path: &str,
    argv: &[String],
    validate: &dyn Fn(&Flag, &str) -> Result<String, String>,
) -> Outcome {
    let mut parsed = Parsed {
        command,
        flags: command.flags.to_vec(),
        values: vec![None; command.flags.len()],
        set: vec![false; command.flags.len()],
        args: Vec::new(),
        notices: String::new(),
    };
    if let Err(e) = read(&mut parsed, argv, validate) {
        return Outcome::Fail {
            notices: String::new(),
            text: format!(
                "{e}\n\nUsage:  {}\n\nRun '{path} --help' for more information",
                use_line(command, path)
            ),
            status: FLAG_ERROR,
        };
    }
    let notices = std::mem::take(&mut parsed.notices);
    if parsed.bool("help") {
        return Outcome::Help { notices };
    }
    let n = parsed.args.len();
    let plural = |k: usize| if k == 1 { "argument" } else { "arguments" };
    let wrong = match command.args {
        Args::None if n > 0 => Some(("accepts no arguments".to_string(), "Run")),
        Args::AtLeast(k) if n < k => Some((format!("requires at least {k} {}", plural(k)), "See")),
        Args::Exactly(k) if n != k => Some((format!("requires {k} {}", plural(k)), "Run")),
        Args::AtMost(k) if n > k => Some((format!("requires at most {k} {}", plural(k)), "Run")),
        Args::Range(min, max) if n < min || n > max => Some((
            format!("requires at least {min} and at most {max} {}", plural(max)),
            "Run",
        )),
        _ => None,
    };
    if let Some((what, verb)) = wrong {
        let bin = path.split(' ').next().unwrap_or_default();
        return Outcome::Fail {
            notices,
            text: format!(
                "{}{bin}: '{path}' {what}\n\nUsage:  {}\n\n{verb} '{path} --help' for more information",
                command.error_prefix,
                use_line(command, path)
            ),
            status: 1,
        };
    }
    // Every flag asked of shards that it cannot serve, in the order `--help` lists them.
    let mut unserved: Vec<&Flag> = parsed
        .flags
        .iter()
        .enumerate()
        .filter(|&(i, f)| {
            !f.supported
                && parsed.set.get(i).copied().unwrap_or(false)
                && parsed
                    .values
                    .get(parsed.owner(i))
                    .and_then(Option::as_ref)
                    .is_some_and(|v| !is_default(f, v))
        })
        .map(|(_, f)| f)
        .collect();
    unserved.sort_by_key(|f| f.name);
    if !unserved.is_empty() {
        let text: Vec<String> = unserved
            .iter()
            .map(|f| {
                format!(
                    "{}\"--{}\" is not supported by shards yet",
                    command.error_prefix, f.name
                )
            })
            .collect();
        return Outcome::Fail {
            notices,
            text: text.join("\n"),
            status: 1,
        };
    }
    parsed.notices = notices;
    Outcome::Run(parsed)
}

/// Whether `value` is what flag `f` holds when not given.
fn is_default(f: &Flag, value: &Value) -> bool {
    match value {
        Value::Bool(b) => go::parse_bool(f.default).is_ok_and(|d| d == *b),
        Value::Int(n) if f.kind == Kind::Duration => {
            crate::gotime::duration(f.default).is_ok_and(|d| d == *n)
        }
        Value::Int(n) => go::parse_int(f.default).is_ok_and(|d| d == *n),
        Value::Text(s) => s == f.default,
        Value::Many(v) => v.is_empty(),
    }
}

/// pflag's parseArgs: flags and arguments, until `--`, or with `interspersed` off, the
/// first argument.
fn read(
    parsed: &mut Parsed,
    argv: &[String],
    validate: &dyn Fn(&Flag, &str) -> Result<String, String>,
) -> Result<(), String> {
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        let bytes = word.as_bytes();
        if bytes.len() < 2 || bytes.first() != Some(&b'-') {
            parsed.args.push(word.clone());
            if !parsed.command.interspersed {
                parsed.args.extend(words.cloned());
                return Ok(());
            }
            continue;
        }
        if bytes.get(1) == Some(&b'-') {
            if bytes.len() == 2 {
                parsed.args.extend(words.cloned());
                return Ok(());
            }
            read_long(parsed, word, &mut words, validate)?;
        } else {
            read_short(parsed, word, &mut words, validate)?;
        }
    }
    Ok(())
}

/// FilterOpt.String: the filters `given` (`name=value` each, as [`value`] keeps them)
/// as `json.Marshal` writes their `map[string]map[string]bool`; nothing for none.
fn filter_json(given: &[String]) -> String {
    let mut by_name: std::collections::BTreeMap<&str, std::collections::BTreeSet<&str>> = Default::default();
    for f in given {
        if let Some((name, value)) = f.split_once('=') {
            by_name.entry(name).or_default().insert(value);
        }
    }
    if by_name.is_empty() {
        return String::new();
    }
    let fields: Vec<String> = by_name
        .iter()
        .map(|(name, values)| {
            let values: Vec<String> = values.iter().map(|v| format!("{}:true", go_json(v))).collect();
            format!("{}:{{{}}}", go_json(name), values.join(","))
        })
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// `s` as Go's encoding/json writes a string: HTML's `<`, `>` and `&` escaped, and the
/// line and paragraph separators.
fn go_json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value of a flag that takes many, as docker/cli's option type for it takes it: a
/// filter (opts.FilterOpt.Set) is `name=value`, its name lowered and both trimmed, or
/// empty; a run's `--label`, `--dns`, `--dns-search` and `--add-host` as their ListOpts
/// validate them (opts/opts.go); others are as given.
pub fn value(flag: &Flag, value: &str) -> Result<String, String> {
    match (flag.name, flag.kind) {
        (_, Kind::Many("filter")) => filter(value),
        // MemSwapBytes takes -1 as itself (docker/cli opts/opts.go).
        ("memory-swap", Kind::Value("bytes")) if value == "-1" => Ok(value.to_string()),
        (_, Kind::Value("bytes")) => crate::resources::ram_in_bytes(value).map(|n| n.to_string()),
        (_, Kind::Value("decimal")) => crate::resources::parse_cpus(value).map(|n| n.to_string()),
        (_, Kind::Value("uint16")) => crate::go::parse_uint_bits(value, 16)
            .map(|n| n.to_string())
            .map_err(|e| e.to_string()),
        ("label", Kind::Many("list")) => validate_label(value),
        ("dns", Kind::Many("list")) => validate_ip(value),
        ("dns-search", Kind::Many("list")) => validate_dns_search(value),
        ("add-host", Kind::Many("list")) => validate_extra_host(value),
        ("device-cgroup-rule", Kind::Many("list")) => validate_device_cgroup_rule(value),
        // pflag's IP values: an address, as Go's net.IP prints it.
        (_, Kind::Many("ipSlice")) => go_ip(value.trim())
            .ok_or_else(|| format!("invalid string being converted to IP address: {value}")),
        (_, Kind::Many("ipNetSlice")) => {
            go_cidr(value.trim()).ok_or_else(|| format!("invalid string being converted to CIDR: {value}"))
        }
        (_, Kind::Value("ip")) => {
            go_ip(value.trim()).ok_or_else(|| format!("failed to parse IP: {}", crate::go::quote(value)))
        }
        ("link", Kind::Many("list")) => validate_link(value),
        ("device-read-bps" | "device-write-bps", Kind::Many("list")) => validate_throttle_bps(value),
        ("device-read-iops" | "device-write-iops", Kind::Many("list")) => validate_throttle_iops(value),
        ("blkio-weight-device", Kind::Many("list")) => validate_weight_device(value),
        ("sysctl", Kind::Many("map")) => validate_sysctl(value),
        // opts.MountOpt.Set.
        (_, Kind::Many("mount")) => crate::mounts::parse_mount(value, None).map(|_| value.to_string()),
        // UlimitOpt, through go-units' ParseUlimit, whose names leave out `as`, which
        // shards' build alone takes (buildflags::validate).
        (_, Kind::Many("ulimit")) => match crate::buildflags::parse_ulimit(value)? {
            u if u.name == "as" => Err("invalid ulimit type: as".into()),
            _ => Ok(value.to_string()),
        },
        _ => Ok(value.to_string()),
    }
}

/// An address as Go's net.ParseIP takes it, as its String prints it: an IPv4-mapped one in
/// dotted form.
fn go_ip(s: &str) -> Option<String> {
    let ip: std::net::IpAddr = s.parse().ok()?;
    Some(match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        v4 => v4.to_string(),
    })
}

/// A CIDR as Go's net.ParseCIDR takes it, as the network it names prints: its address
/// masked by its length.
fn go_cidr(s: &str) -> Option<String> {
    let (addr, bits) = s.split_once('/')?;
    let bits: u8 = bits.parse().ok().filter(|_| !bits.starts_with('+'))?;
    match addr.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v4) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            Some(format!(
                "{}/{bits}",
                std::net::Ipv4Addr::from(u32::from(v4) & mask)
            ))
        }
        std::net::IpAddr::V6(v6) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            Some(format!(
                "{}/{bits}",
                std::net::Ipv6Addr::from(u128::from(v6) & mask)
            ))
        }
        _ => None,
    }
}

/// opts.ValidateLink: `NAME[:ALIAS]`.
fn validate_link(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("empty string specified for links".into());
    }
    if value.split(':').count() > 2 {
        return Err(format!("bad format for links: {value}"));
    }
    Ok(value.to_string())
}

/// A device's path and the rest (opts/throttledevice.go and weightdevice.go): `PATH:REST`,
/// PATH under `/dev/`.
fn device_and(value: &str) -> Result<(&str, &str), String> {
    match value.split_once(':') {
        Some((k, v)) if !k.is_empty() => {
            if k.starts_with("/dev/") {
                Ok((k, v))
            } else {
                Err(format!("bad format for device path: {value}"))
            }
        }
        _ => Err(format!("bad format: {value}")),
    }
}

/// ValidateThrottleBpsDevice: `PATH:RATE`, the rate as go-units' RAMInBytes reads it; kept
/// as `PATH:BYTES`.
fn validate_throttle_bps(value: &str) -> Result<String, String> {
    let (path, rate) = device_and(value)?;
    match crate::resources::ram_in_bytes(rate) {
        Ok(n) if n >= 0 => Ok(format!("{path}:{n}")),
        _ => Err(format!(
            "invalid rate for device: {value}. The correct format is <device-path>:<number>[<unit>]. Number must be a positive integer. Unit is optional and can be kb, mb, or gb"
        )),
    }
}

/// ValidateThrottleIOpsDevice: `PATH:RATE`, a whole number.
fn validate_throttle_iops(value: &str) -> Result<String, String> {
    let (path, rate) = device_and(value)?;
    let n = crate::go::parse_uint_bits(rate, 64).map_err(|_| {
        format!("invalid rate for device: {value}. The correct format is <device-path>:<number>. Number must be a positive integer")
    })?;
    Ok(format!("{path}:{n}"))
}

/// ValidateWeightDevice: `PATH:WEIGHT`, 0 or 10 to 1000.
fn validate_weight_device(value: &str) -> Result<String, String> {
    let (path, weight) = device_and(value)?;
    match crate::go::parse_uint_bits(weight, 16) {
        Ok(w) if w == 0 || (10..=1000).contains(&w) => Ok(format!("{path}:{w}")),
        _ => Err(format!("invalid weight for device: {value}")),
    }
}

/// validateDeviceCgroupRule (cli/command/container/opts.go): `^[acb] ([0-9]+|\*):([0-9]+|\*) [rwm]{1,3}$`.
fn validate_device_cgroup_rule(value: &str) -> Result<String, String> {
    let number = |s: &str| s == "*" || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let ok = (|| {
        let (kind, rest) = value.split_once(' ')?;
        let (numbers, access) = rest.split_once(' ')?;
        let (major, minor) = numbers.split_once(':')?;
        Some(
            matches!(kind, "a" | "c" | "b")
                && number(major)
                && number(minor)
                && (1..=3).contains(&access.len())
                && access.bytes().all(|b| matches!(b, b'r' | b'w' | b'm')),
        )
    })()
    .unwrap_or(false);
    if ok {
        Ok(value.to_string())
    } else {
        Err(format!("invalid device cgroup format '{value}'"))
    }
}

fn filter(value: &str) -> Result<String, String> {
    // An empty filter is no filter (opts.FilterOpt.Set): kept empty, and skipped.
    if value.is_empty() {
        return Ok(String::new());
    }
    let Some((name, val)) = value.split_once('=') else {
        return Err("bad format of filter (expected name=value)".into());
    };
    Ok(format!("{}={}", name.trim().to_lowercase(), val.trim()))
}

/// opts.ValidateLabel.
fn validate_label(value: &str) -> Result<String, String> {
    let key = value.split_once('=').map_or(value, |(k, _)| k);
    let key = key.trim_start_matches([' ', '\t']);
    if key.is_empty() {
        return Err(format!("invalid label '{value}': empty name"));
    }
    if key.contains([' ', '\t']) {
        return Err(format!("label '{key}' contains whitespaces"));
    }
    Ok(value.to_string())
}

/// opts.ValidateIPAddress: net.ParseIP of the trimmed value, as IP.String writes it (an
/// IPv4-mapped address as IPv4).
pub fn validate_ip(value: &str) -> Result<String, String> {
    match value.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_some() => {
            Ok(v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_default())
        }
        Ok(ip) => Ok(ip.to_string()),
        Err(_) => Err(format!("IP address is not correctly formatted: {value}")),
    }
}

/// opts.ValidateDNSSearch: `.`, or a domain as validateDomain reads one.
fn validate_dns_search(value: &str) -> Result<String, String> {
    let value = value.trim_matches(' ');
    if value == "." {
        return Ok(value.to_string());
    }
    // alphaRegexp, then domainRegexp's first group, under 255 bytes. Its `(:?` are
    // groups that may start with a colon, as Go reads them.
    let invalid = || format!("{value} is not a valid domain");
    if !value.bytes().any(|b| b.is_ascii_alphabetic()) {
        return Err(invalid());
    }
    let domain = regex::Regex::new(
        r"^(:?(:?[a-zA-Z0-9]|(:?[a-zA-Z0-9][a-zA-Z0-9\-]*[a-zA-Z0-9]))(:?\.(:?[a-zA-Z0-9]|(:?[a-zA-Z0-9][a-zA-Z0-9\-]*[a-zA-Z0-9])))*)\.?[\t\n\x0c\r ]*$",
    )
    .map_err(|e| e.to_string())?;
    match domain.captures(value).and_then(|c| c.get(1)) {
        Some(m) if m.as_str().len() < 255 => Ok(m.as_str().to_string()),
        _ => Err(invalid()),
    }
}

/// opts.ValidateExtraHost: `host=ip` or `host:ip`, the address bracketed or not, or
/// `host-gateway`; given to dockerd as `host:ip`.
fn validate_extra_host(value: &str) -> Result<String, String> {
    let split = value.split_once('=').or_else(|| value.split_once(':'));
    let (k, v) = match split {
        Some((k, v)) if !k.is_empty() && !k.contains(':') => (k, v),
        _ => return Err(format!("bad format for add-host: {}", crate::go::quote(value))),
    };
    if v != "host-gateway" {
        let bare = if v.len() > 2 && v.starts_with('[') && v.ends_with(']') {
            v.get(1..v.len() - 1).unwrap_or(v)
        } else {
            v
        };
        if validate_ip(bare).is_err() {
            return Err(format!(
                "invalid IP address in add-host: {}",
                crate::go::quote(bare)
            ));
        }
        return Ok(format!("{k}:{bare}"));
    }
    Ok(format!("{k}:{v}"))
}

/// docker/cli's opts.ValidateSysctl: the sysctls a container's namespaces hold, the IPC
/// ones by name, the network and message queue ones by prefix.
fn validate_sysctl(value: &str) -> Result<String, String> {
    const NAMED: [&str; 8] = [
        "kernel.msgmax",
        "kernel.msgmnb",
        "kernel.msgmni",
        "kernel.sem",
        "kernel.shmall",
        "kernel.shmmax",
        "kernel.shmmni",
        "kernel.shm_rmid_forced",
    ];
    match value.split_once('=') {
        Some((k, _))
            if !k.is_empty()
                && (NAMED.contains(&k) || k.starts_with("net.") || k.starts_with("fs.mqueue.")) =>
        {
            Ok(value.to_string())
        }
        _ => Err(format!("sysctl '{value}' is not allowed")),
    }
}

/// pflag's parseLongArg: `--name`, `--name=value`, or `--name value`.
fn read_long(
    parsed: &mut Parsed,
    word: &str,
    words: &mut std::slice::Iter<'_, String>,
    validate: &dyn Fn(&Flag, &str) -> Result<String, String>,
) -> Result<(), String> {
    let name = word.get(2..).unwrap_or_default();
    if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
        return Err(format!("bad flag syntax: {word}"));
    }
    let (name, inline) = match name.split_once('=') {
        Some((n, v)) => (n, Some(v.to_string())),
        None => (name, None),
    };
    let Some(index) = parsed.find(|f| f.name == name) else {
        return Err(format!("unknown flag: --{name}"));
    };
    let flag = parsed.flags.get(index).copied().ok_or("no flag")?;
    let value = match inline {
        Some(v) => v,
        None if flag.kind == Kind::Bool => "true".to_string(),
        None => words
            .next()
            .cloned()
            .ok_or_else(|| format!("flag needs an argument: --{name}"))?,
    };
    set(parsed, index, &value, validate)
}

/// pflag's parseShortArg: shorthands run together (`-aq`), the last of them perhaps
/// taking a value (`-n3`, `-n=3`, `-n 3`).
fn read_short(
    parsed: &mut Parsed,
    word: &str,
    words: &mut std::slice::Iter<'_, String>,
    validate: &dyn Fn(&Flag, &str) -> Result<String, String>,
) -> Result<(), String> {
    let mut shorts = word.get(1..).unwrap_or_default();
    while let Some(&c) = shorts.as_bytes().first() {
        // Go test binaries' own flags pass untouched (golangflag.go).
        if shorts.starts_with("test.") {
            return Ok(());
        }
        let rest = shorts.get(1..).unwrap_or_default();
        let Some(index) = parsed.find(|f| f.short == Some(c)) else {
            return Err(format!(
                "unknown shorthand flag: {} in -{shorts}",
                go::quote_rune(latin1(c))
            ));
        };
        let flag = parsed.flags.get(index).copied().ok_or("no flag")?;
        let value;
        if let Some(inline) = rest.strip_prefix('=').filter(|_| shorts.len() > 2) {
            value = inline.to_string();
            shorts = "";
        } else if flag.kind == Kind::Bool {
            value = "true".to_string();
            shorts = rest;
        } else if !rest.is_empty() {
            value = rest.to_string();
            shorts = "";
        } else if let Some(next) = words.next() {
            value = next.clone();
            shorts = "";
        } else if c == b'h'
            && let Some(help) = parsed.find(|f| f.name == "help")
        {
            // `-h` with nothing after it is help, whatever else it is short for
            // (`run -h`, where `-h HOST` is `--hostname`).
            set(parsed, help, "true", validate)?;
            shorts = "";
            continue;
        } else {
            return Err(format!(
                "flag needs an argument: {} in -{shorts}",
                go::quote_rune(latin1(c))
            ));
        }
        if let Some(why) = flag.short_deprecated {
            let _ = writeln!(
                parsed.notices,
                "Flag shorthand -{} has been deprecated, {why}",
                char::from(c)
            );
        }
        set(parsed, index, &value, validate)?;
    }
    Ok(())
}

/// The rune Go's `string(byte)` makes of a shorthand's first byte, whose first byte pflag
/// then shows: the byte itself if ASCII, else the lead byte of its two-byte encoding.
fn latin1(b: u8) -> char {
    match b {
        0..=0x7f => char::from(b),
        0x80..=0xbf => '\u{c2}',
        _ => '\u{c3}',
    }
}

/// pflag's Set: `value` read into the flag at `index`, or why not.
fn set(
    parsed: &mut Parsed,
    index: usize,
    value: &str,
    validate: &dyn Fn(&Flag, &str) -> Result<String, String>,
) -> Result<(), String> {
    let flag = parsed.flags.get(index).copied().ok_or("no flag")?;
    let invalid = |cause: String| {
        format!(
            "invalid argument {} for {} flag: {cause}",
            go::quote(value),
            go::quote(&flag.names())
        )
    };
    // The flag that owns the value joins the rest first, if it is one not served.
    if let Some(owner) = flag.shares {
        parsed.find(|f| f.name == owner);
    }
    let owner = parsed.owner(index);
    if let Some(set) = parsed.set.get_mut(index) {
        *set = true;
    }
    let slot = parsed.values.get_mut(owner).ok_or("no flag")?;
    *slot = Some(match (flag.kind, slot.take()) {
        (Kind::Bool, _) => Value::Bool(go::parse_bool(value).map_err(|e| invalid(e.to_string()))?),
        (Kind::Int, _) => Value::Int(go::parse_int(value).map_err(|e| invalid(e.to_string()))?),
        (Kind::String, _) => Value::Text(value.to_string()),
        (Kind::Duration, _) => Value::Int(crate::gotime::duration(value).map_err(invalid)?),
        (Kind::Value(_), _) => Value::Text(validate(&flag, value).map_err(invalid)?),
        (Kind::Many(kind), before) => {
            let mut all = match before {
                Some(Value::Many(all)) => all,
                _ => Vec::new(),
            };
            // pflag's slices take each value's pieces (StringSlice's as a CSV record,
            // IPSlice's and IPNetSlice's at each comma, quotes dropped), each checked.
            match kind {
                "strings" => {
                    for piece in csv_fields(value) {
                        all.push(validate(&flag, &piece).map_err(invalid)?);
                    }
                }
                "ipSlice" | "ipNetSlice" => {
                    for piece in value.replace('"', "").split(',') {
                        all.push(validate(&flag, piece).map_err(invalid)?);
                    }
                }
                _ => all.push(validate(&flag, value).map_err(invalid)?),
            }
            Value::Many(all)
        }
    });
    if let Some(why) = flag.deprecated {
        let _ = writeln!(parsed.notices, "Flag --{} has been deprecated, {why}", flag.name);
    }
    Ok(())
}

/// What `--help` prints for `command`, which `path` names, on a terminal `columns` wide:
/// the CLI's usage template (docker/cli cli/cobra.go usageTemplate, as its help template
/// trims it). The CLI takes the width of its stdin's terminal, or 80 (wrappedFlagUsages).
pub fn help(command: &Command, path: &str, columns: u16) -> String {
    let mut text = format!("Usage:  {}\n\n{}", use_line(command, path), command.about.trim());
    if !command.aliases.is_empty() {
        let _ = write!(text, "\n\nAliases:\n  {}", command.aliases);
    }
    for (heading, extension) in [("Options", false), ("Shards options", true)] {
        let options = options(command.flags, extension, i64::from(columns) - 1);
        let options = options.trim_end();
        if !options.is_empty() {
            let _ = write!(text, "\n\n{heading}:\n{options}");
        }
    }
    text.push('\n');
    text
}

/// A flag as `--help` shows it.
#[derive(Debug, Clone)]
pub struct Shown {
    /// Its shorthand, where `--help` shows one.
    pub short: Option<char>,
    pub name: &'static str,
    /// The name of its value: `string`, or what its usage marks with backquotes.
    pub value: String,
    /// What it does, its backquotes taken out.
    pub usage: String,
    /// Its default, as pflag prints it, where it is not its type's zero.
    pub default: Option<String>,
    pub deprecated: Option<&'static str>,
}

/// The flags `--help` shows, in the order of their names: `docker`'s, or shards' own.
pub fn shown(flags: &[Flag], extension: bool) -> Vec<Shown> {
    let mut shown: Vec<&Flag> = flags
        .iter()
        .filter(|f| !f.hidden && f.extension == extension)
        .collect();
    shown.sort_by_key(|f| f.name);
    shown
        .into_iter()
        .map(|f| {
            let (value, usage) = unquote_usage(f);
            Shown {
                short: f.short.filter(|_| f.short_deprecated.is_none()).map(char::from),
                name: f.name,
                value,
                usage,
                default: (!f.default_is_zero()).then(|| {
                    if f.kind == Kind::String {
                        go::quote(f.default)
                    } else {
                        f.default.to_string()
                    }
                }),
                deprecated: f.deprecated,
            }
        })
        .collect()
}

/// pflag's FlagUsagesWrapped: a line per flag `--help` shows, in the order of their
/// names, the usages aligned and wrapped at `columns`: `docker`'s flags, or shards' own.
fn options(flags: &[Flag], extension: bool, columns: i64) -> String {
    let mut lines = Vec::new();
    let mut widest = 0;
    for f in shown(flags, extension) {
        let mut line = match f.short {
            Some(s) => format!("  -{s}, --{}", f.name),
            None => format!("      --{}", f.name),
        };
        if !f.value.is_empty() {
            let _ = write!(line, " {}", f.value);
        }
        // pflag counts the separator it puts here.
        widest = widest.max(line.len() + 1);
        let mut usage = f.usage;
        if let Some(default) = f.default {
            let _ = write!(usage, " (default {default})");
        }
        if let Some(why) = f.deprecated {
            let _ = write!(usage, " (DEPRECATED: {why})");
        }
        lines.push((line, usage));
    }
    let mut out = String::new();
    for (line, usage) in lines {
        let spacing = " ".repeat(widest - line.len());
        let indent = i64::try_from(widest + 2).unwrap_or(i64::MAX);
        let _ = writeln!(out, "{line} {spacing} {}", wrap(indent, columns, &usage));
    }
    out
}

/// pflag's UnquoteUsage: the name a usage marks with backquotes, or the type's.
fn unquote_usage(f: &Flag) -> (String, String) {
    let usage = f.usage;
    if let Some((before, after)) = usage.split_once('`')
        && let Some((name, rest)) = after.split_once('`')
    {
        return (name.to_string(), format!("{before}{name}{rest}"));
    }
    let type_name = match f.kind {
        Kind::Bool => "",
        Kind::Int => "int",
        Kind::String => "string",
        Kind::Many(t) | Kind::Value(t) => t,
        Kind::Duration => "duration",
    };
    (type_name.to_string(), usage.to_string())
}

/// pflag's wrap: `s` wrapped to `width` columns (none if 0), lines after the first
/// indented by `indent`.
fn wrap(indent: i64, width: i64, s: &str) -> String {
    let spaces = |n: i64| " ".repeat(usize::try_from(n).unwrap_or(0));
    if width == 0 {
        return s.replace('\n', &format!("\n{}", spaces(indent)));
    }
    let mut indent = indent;
    let mut room = width - indent;
    let mut out = String::new();
    // Too little room beside the names: the usage goes below them.
    if room < 24 {
        indent = 16;
        room = width - indent;
        out.push('\n');
        out.push_str(&spaces(indent));
    }
    if room < 24 {
        return s.replace('\n', &out);
    }
    // The last line may run this far over, rather than leave a word alone on the next.
    let slop = 5;
    let room = usize::try_from(room - 5).unwrap_or(0);
    let pad = format!("\n{}", spaces(indent));
    let (first, mut rest) = wrap_line(room, slop, s);
    out.push_str(&first.replace('\n', &pad));
    while !rest.is_empty() {
        let (line, after) = wrap_line(room, slop, rest);
        out.push_str(&pad);
        out.push_str(&line.replace('\n', &pad));
        rest = after;
    }
    out
}

/// pflag's wrapN: the start of `s` up to the last space or newline within `i` bytes, and
/// the rest; all of `s` if it fits in `i + slop`.
fn wrap_line(i: usize, slop: usize, s: &str) -> (&str, &str) {
    if i + slop > s.len() {
        return (s, "");
    }
    let head = s.get(..i).unwrap_or(s);
    let Some(space) = head.rfind([' ', '\t', '\n']).filter(|&w| w > 0) else {
        return (s, "");
    };
    let split = match head.rfind('\n') {
        Some(newline) if newline > 0 && newline < space => newline,
        _ => space,
    };
    (
        s.get(..split).unwrap_or(s),
        s.get(split + 1..).unwrap_or_default(),
    )
}
