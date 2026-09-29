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

    pub const fn hidden(mut self) -> Flag {
        self.hidden = true;
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
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Int(n)) => n.to_string(),
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::Many(v)) if v.is_empty() => String::new(),
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
        _ => None,
    };
    if let Some((what, verb)) = wrong {
        let bin = path.split(' ').next().unwrap_or_default();
        return Outcome::Fail {
            notices,
            text: format!(
                "{bin}: '{path}' {what}\n\nUsage:  {}\n\n{verb} '{path} --help' for more information",
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
            .map(|f| format!("\"--{}\" is not supported by shards yet", f.name))
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
        (Kind::Many(_), before) => {
            let mut all = match before {
                Some(Value::Many(all)) => all,
                _ => Vec::new(),
            };
            all.push(validate(&flag, value).map_err(invalid)?);
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
    let options = options(command.flags, i64::from(columns) - 1);
    let options = options.trim_end();
    if !options.is_empty() {
        let _ = write!(text, "\n\nOptions:\n{options}");
    }
    text.push('\n');
    text
}

/// pflag's FlagUsagesWrapped: a line per flag `--help` shows, in the order of their
/// names, the usages aligned and wrapped at `columns`.
fn options(flags: &[Flag], columns: i64) -> String {
    let mut shown: Vec<&Flag> = flags.iter().filter(|f| !f.hidden).collect();
    shown.sort_by_key(|f| f.name);
    let mut lines = Vec::new();
    let mut widest = 0;
    for f in shown {
        let mut line = match f.short {
            Some(s) if f.short_deprecated.is_none() => format!("  -{}, --{}", char::from(s), f.name),
            _ => format!("      --{}", f.name),
        };
        let (type_name, usage) = unquote_usage(f);
        if !type_name.is_empty() {
            let _ = write!(line, " {type_name}");
        }
        // pflag counts the separator it puts here.
        widest = widest.max(line.len() + 1);
        let mut usage = usage;
        if !f.default_is_zero() {
            if f.kind == Kind::String {
                let _ = write!(usage, " (default {})", go::quote(f.default));
            } else {
                let _ = write!(usage, " (default {})", f.default);
            }
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
        Kind::Many(t) => t,
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
