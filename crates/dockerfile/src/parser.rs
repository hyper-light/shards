//! A Dockerfile's text into instructions, as BuildKit's `frontend/dockerfile/parser` reads
//! it (moby/buildkit dockerfile/1.27.1): parser directives, comments, line continuations,
//! heredocs, flags and each instruction's arguments, with BuildKit's errors, their lines,
//! and its warnings. Results are BuildKit's byte for byte (tests/oracle.rs), but for these
//! deliberate differences (testdata/deviations.json):
//!
//! - **Flags keep their bytes.** BuildKit reads a flag's bytes as Latin-1 characters, so
//!   `--chown=josé` becomes `--chown=josÃ©`, and the second byte of a UTF-8 no-break space
//!   splits a word. Here flags are read as UTF-8, as every other argument is.
//! - **Lines have no length limit.** BuildKit refuses a line over 64 KiB (`bufio.Scanner`'s
//!   default); here a line is as long as the file.
//! - **Nesting is bounded.** `ONBUILD` inside `ONBUILD`, which no build accepts, and
//!   `${...}` inside `${...}` stop at [`MAX_NESTING`] levels with an error, so that no
//!   input exhausts a thread's stack (Go's stacks grow; a daemon's threads must not).
//!
//! Kept for parity although BuildKit's own source calls it a limitation: an escaped escape
//! token before a line's continuation (`foo \\\`) does not continue the line
//! (`parser.go`, `setEscapeToken`).

use crate::go;
use crate::json::{self, Array};
use crate::lex::{Lex, NoEnv};

/// How deep instructions and expansions may nest.
pub const MAX_NESTING: usize = 64;

/// One parsed instruction, or one of its arguments: BuildKit's `Node`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    /// The instruction as written (its case kept), or an argument.
    pub value: Vec<u8>,
    /// The instruction's arguments, in order. An `ONBUILD`'s single argument holds the
    /// instruction it wraps as its child.
    pub next: Vec<Node>,
    /// Only an `ONBUILD`'s argument has one: its instruction.
    pub children: Vec<Node>,
    pub heredocs: Vec<Heredoc>,
    /// The arguments were a JSON array.
    pub json: bool,
    /// The instruction's line, continuations joined.
    pub original: Vec<u8>,
    /// Its flags (`--name=value`), as written.
    pub flags: Vec<Vec<u8>>,
    /// The lines it spans, from 1.
    pub start_line: usize,
    pub end_line: usize,
    /// The comment lines just before it.
    pub prev_comment: Vec<Vec<u8>>,
}

/// A here-document an instruction takes (`<<NAME`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heredoc {
    pub name: Vec<u8>,
    /// The descriptor it is given on (`3<<NAME`), or 0.
    pub file_descriptor: u64,
    /// Variables expand in its content: its name was not quoted.
    pub expand: bool,
    /// `<<-`: leading tabs are stripped.
    pub chomp: bool,
    pub content: Vec<u8>,
}

/// A parsed Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub instructions: Vec<Node>,
    pub escape: u8,
    pub warnings: Vec<Warning>,
}

/// A warning BuildKit gives: an empty line inside a continued instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub short: Vec<u8>,
    pub url: &'static str,
    pub line: usize,
}

/// A parse error, with the lines it concerns, as BuildKit's `LocationError` has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: Vec<u8>,
    /// Ranges of lines, each `(start, end)`, in lists, as BuildKit nests them.
    pub location: Vec<Vec<(usize, usize)>>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.message))
    }
}

impl std::error::Error for Error {}

fn err(message: impl Into<Vec<u8>>) -> Vec<u8> {
    message.into()
}

/// BuildKit's `toRanges`: each line from `start` to `end`, one range each.
fn ranges(start: usize, end: usize) -> Vec<(usize, usize)> {
    let end = end.max(start);
    (start..=end).map(|l| (l, l)).collect()
}

fn located(message: Vec<u8>, start: usize, end: usize) -> Error {
    Error {
        message,
        location: vec![ranges(start, end)],
    }
}

impl Node {
    /// The parse tree as BuildKit's `Dump` writes it, for comparison with BuildKit.
    pub fn dump(&self) -> Vec<u8> {
        let mut out = go::to_lower(&self.value);
        if !self.flags.is_empty() {
            out.extend_from_slice(b" [");
            for (i, f) in self.flags.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                out.extend_from_slice(go::quote(f).as_bytes());
            }
            out.push(b']');
        }
        for c in &self.children {
            out.push(b'(');
            out.extend_from_slice(&c.dump());
            out.extend_from_slice(b")\n");
        }
        for n in &self.next {
            out.push(b' ');
            if n.children.is_empty() {
                out.extend_from_slice(go::quote(&n.value).as_bytes());
            } else {
                out.extend_from_slice(&n.dump());
            }
        }
        go::trim_space(&out).to_vec()
    }

    fn arg(value: Vec<u8>) -> Node {
        Node {
            value,
            ..Node::default()
        }
    }
}

/// The parse tree of a whole file, as BuildKit dumps its root.
pub fn dump(instructions: &[Node]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in instructions {
        out.push(b'(');
        out.extend_from_slice(&c.dump());
        out.extend_from_slice(b")\n");
    }
    go::trim_space(&out).to_vec()
}

/// What a line's arguments parse to, and whether they were JSON.
type Args = (Vec<Node>, bool);

/// Parser directives (`# escape=`, `# syntax=`, `# check=`), as BuildKit's
/// `DirectiveParser` takes them: only before anything else, each once.
#[derive(Debug)]
struct Directives {
    done: bool,
    seen: Vec<Vec<u8>>,
    escape: u8,
}

/// A directive line's key and value: `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$`, `\s` being
/// Go's ASCII `[\t\n\f\r ]`.
fn directive(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let ws = |b: u8| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ');
    let first = line.first()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let key_end = line
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .unwrap_or(line.len());
    let mut at = key_end;
    while line.get(at).is_some_and(|&b| ws(b)) {
        at += 1;
    }
    if line.get(at) != Some(&b'=') {
        return None;
    }
    let after = go::tail(line, at + 1);
    // The value is lazy and its leading space greedy: what follows the space up to the
    // last character that is not space; or, when only space follows, its last character.
    let lead = after.iter().take_while(|&&b| ws(b)).count();
    let rest = go::tail(after, lead);
    if rest.contains(&b'\n') {
        return None;
    }
    let value = if rest.is_empty() {
        if lead == 0 {
            return None;
        }
        go::span(after, lead - 1, lead)
    } else {
        let end = rest.iter().rposition(|&b| !ws(b)).map_or(0, |e| e + 1);
        go::head(rest, end)
    };
    Some((go::head(line, key_end), value))
}

impl Directives {
    /// Takes `line` as a possible directive; whether it was one.
    fn line(&mut self, line: &[u8]) -> Result<bool, Vec<u8>> {
        if self.done {
            return Ok(false);
        }
        let Some(rest) = line.strip_prefix(b"#") else {
            self.done = true;
            return Ok(false);
        };
        let Some((key, value)) = directive(go::trim_left_space(rest)) else {
            self.done = true;
            return Ok(false);
        };
        let key = key.to_ascii_lowercase();
        if !matches!(key.as_slice(), b"syntax" | b"escape" | b"check") {
            self.done = true;
            return Ok(false);
        }
        if self.seen.contains(&key) {
            let mut m = b"only one ".to_vec();
            m.extend_from_slice(&key);
            m.extend_from_slice(b" parser directive can be used");
            return Err(m);
        }
        self.seen.push(key.clone());
        if key == b"escape" {
            match value {
                [c @ (b'`' | b'\\')] => self.escape = *c,
                _ => {
                    let mut m = b"invalid escape token '".to_vec();
                    m.extend_from_slice(value);
                    m.extend_from_slice(b"' does not match ` or \\");
                    return Err(m);
                }
            }
        }
        Ok(true)
    }
}

/// The first word of directive `key`'s value among a file's leading directives:
/// `ParseDirective` (a byte-order mark and a shebang line skipped, reading stopped at the
/// first line that is no directive, or at a directive given twice).
pub fn directive_value(text: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let (text, line) = directive_text(text);
    parse_all(text, b"#", line)
        .into_iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, value, _)| first_word(&value))
}

/// `DetectSyntax`: the frontend a file names, by a `#` directive, else a `//` one, else as
/// the `syntax` of a file that is one JSON object: the reference (the value's first word),
/// the whole value, and its line.
pub fn detect_syntax(text: &[u8]) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    let (text, line) = directive_text(text);
    for comment in [b"#".as_slice(), b"//"] {
        if let Some((_, value, at)) = parse_all(text, comment, line)
            .into_iter()
            .find(|(k, _, _)| k == b"syntax")
        {
            return Some((first_word(&value), value, at));
        }
    }
    let Ok(json::Value::Object(ref members)) = json::parse(text) else {
        return None;
    };
    // Go's map keeps a key given twice as last given.
    match members.iter().rev().find(|(k, _)| k == b"syntax") {
        Some((_, json::Value::String(v, _))) => Some((v.clone(), v.clone(), line)),
        _ => None,
    }
}

/// A file as its directives are read: its byte-order mark and shebang line dropped, and
/// the line before the first left (`discardBOM`, `discardShebang`).
fn directive_text(text: &[u8]) -> (&[u8], usize) {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    if !text.starts_with(b"#!") {
        return (text, 0);
    }
    match text.iter().position(|&b| b == b'\n') {
        Some(end) => (go::tail(text, end + 1), 1),
        None => (&[], 1),
    }
}

/// `DirectiveParser.ParseAll` with comment prefix `comment`, lines counted on from `line`:
/// the leading directives, each lowercase with its value and line, as far as the first
/// line that is no comment, no directive, none BuildKit knows, or one given again.
fn parse_all(text: &[u8], comment: &[u8], mut line: usize) -> Vec<(Vec<u8>, Vec<u8>, usize)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>, usize)> = Vec::new();
    for raw in text.split(|&b| b == b'\n') {
        line += 1;
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let Some(rest) = raw.strip_prefix(comment) else {
            break;
        };
        let Some((k, value)) = directive(go::trim_left_space(rest)) else {
            break;
        };
        let k = k.to_ascii_lowercase();
        if !matches!(k.as_slice(), b"syntax" | b"escape" | b"check")
            || out.iter().any(|(seen, _, _)| *seen == k)
        {
            break;
        }
        out.push((k, value.to_vec(), line));
    }
    out
}

/// `strings.Cut(value, " ")`'s first part.
pub(crate) fn first_word(value: &[u8]) -> Vec<u8> {
    value.split(|&b| b == b' ').next().unwrap_or_default().to_vec()
}

/// One comment line read as a directive, BuildKit's `DirectiveParser` with no comment
/// prefix: how an instruction's comments carry a `# check=` of their own. The directive's
/// name, lowercase, and value, if the line is one.
pub(crate) fn directive_line(line: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (key, value) = directive(go::trim_left_space(line))?;
    let key = key.to_ascii_lowercase();
    matches!(key.as_slice(), b"syntax" | b"escape" | b"check").then(|| (key, value.to_vec()))
}

fn is_comment(line: &[u8]) -> bool {
    go::trim_left_space(line).first() == Some(&b'#')
}

/// `bytes.TrimRight(b, "\r\n")`.
fn trim_newline(b: &[u8]) -> &[u8] {
    let end = b
        .iter()
        .rposition(|&c| c != b'\r' && c != b'\n')
        .map_or(0, |e| e + 1);
    go::head(b, end)
}

/// Ends a continued line: an escape token, then only spaces and tabs, not preceded by
/// another escape token. Returns the line without it, and whether the instruction ends.
fn trim_continuation(line: &[u8], escape: u8) -> (&[u8], bool) {
    let end = line
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(0, |e| e + 1);
    let body = go::head(line, end);
    if body.last() != Some(&escape) {
        return (line, true);
    }
    let before = go::head(body, body.len() - 1);
    match before.last() {
        // The escape token is ASCII, so no multi-byte rune ends in its byte.
        Some(&b) if b != escape => (before, false),
        Some(_) => (line, true),
        None => (&[], false),
    }
}

/// `[\t\v\f\r ]+`, BuildKit's `reWhitespace`.
fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | 0x0b | 0x0c | b'\r' | b' ')
}

/// `reWhitespace.Split(s, n)` with `n` 2 (`limit`) or all (`None`).
fn split_ws(s: &[u8], limit: Option<usize>) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < s.len() {
        if limit.is_some_and(|n| out.len() + 1 >= n) {
            break;
        }
        if s.get(at).is_some_and(|&b| is_ws(b)) {
            let run_end = go::tail(s, at)
                .iter()
                .position(|&b| !is_ws(b))
                .map_or(s.len(), |e| at + e);
            out.push(go::span(s, start, at));
            start = run_end;
            at = run_end;
        } else {
            at += 1;
        }
    }
    out.push(go::tail(s, start));
    out
}

struct Parser {
    escape: u8,
    depth: usize,
}

impl Parser {
    /// One instruction's line into its node: `newNodeFromLine`.
    fn node(&mut self, line: &[u8], comments: Vec<Vec<u8>>) -> Result<Node, Vec<u8>> {
        let (cmd, flags, args) = self.split_command(line);
        let (next, json) = self.dispatch(&cmd, &args)?;
        Ok(Node {
            value: cmd,
            original: line.to_vec(),
            flags,
            next,
            json,
            prev_comment: comments,
            ..Node::default()
        })
    }

    fn split_command(&self, line: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, Vec<u8>) {
        let parts = split_ws(go::trim_space(line), Some(2));
        let cmd = parts.first().map_or_else(Vec::new, |c| c.to_vec());
        let Some(rest) = parts.get(1) else {
            return (cmd, Vec::new(), Vec::new());
        };
        let (args, flags) = self.builder_flags(rest);
        (cmd, flags, go::trim_space(&args).to_vec())
    }

    /// The leading `--flag` words of `line`, and the rest of it: `extractBuilderFlags`,
    /// reading runes where BuildKit reads bytes (the module's documentation).
    fn builder_flags(&self, line: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        #[derive(PartialEq)]
        enum Phase {
            Spaces,
            Word,
            Quote,
        }
        let mut words = Vec::new();
        let mut phase = Phase::Spaces;
        let mut word: Vec<u8> = Vec::new();
        let mut quote = 0u32;
        let mut blank_ok = false;
        let escape = u32::from(self.escape);
        let mut pos = 0;
        loop {
            let at_end = pos >= line.len();
            let (ch, width) = if at_end {
                (0, 1)
            } else {
                go::decode(go::tail(line, pos))
            };
            if phase == Phase::Spaces {
                if at_end {
                    break;
                }
                if go::is_space(ch) {
                    pos += width;
                    continue;
                }
                if !go::tail(line, pos).starts_with(b"--") {
                    return (go::tail(line, pos).to_vec(), words);
                }
                phase = Phase::Word;
            }
            if at_end {
                if word != b"--" && (blank_ok || !word.is_empty()) {
                    words.push(word);
                }
                break;
            }
            if phase == Phase::Word {
                if go::is_space(ch) {
                    phase = Phase::Spaces;
                    if word == b"--" {
                        return (go::tail(line, pos).to_vec(), words);
                    }
                    if blank_ok || !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    word.clear();
                    blank_ok = false;
                    pos += width;
                    continue;
                }
                if ch == u32::from('\'') || ch == u32::from('"') {
                    quote = ch;
                    blank_ok = true;
                    phase = Phase::Quote;
                    pos += width;
                    continue;
                }
                let (mut c, mut w) = (ch, width);
                if ch == escape {
                    if pos + width == line.len() {
                        pos += width;
                        continue;
                    }
                    pos += width;
                    (c, w) = go::decode(go::tail(line, pos));
                }
                go::push(&mut word, c);
                pos += w;
                continue;
            }
            // In quotes.
            if ch == quote {
                phase = Phase::Word;
                pos += width;
                continue;
            }
            let (mut c, mut w) = (ch, width);
            if ch == escape {
                if pos + width == line.len() {
                    phase = Phase::Word;
                    pos += width;
                    continue;
                }
                pos += width;
                (c, w) = go::decode(go::tail(line, pos));
            }
            go::push(&mut word, c);
            pos += w;
        }
        (Vec::new(), words)
    }

    fn dispatch(&mut self, cmd: &[u8], rest: &[u8]) -> Result<Args, Vec<u8>> {
        match go::to_lower(cmd).as_slice() {
            b"add" | b"copy" | b"volume" => maybe_json_to_list(rest),
            b"arg" => Ok((name_or_name_val(rest, self.escape), false)),
            b"cmd" | b"entrypoint" | b"run" | b"shell" => maybe_json(rest),
            b"env" => Ok((name_val(rest, b"ENV", self.escape)?, false)),
            b"label" => Ok((name_val(rest, b"LABEL", self.escape)?, false)),
            b"expose" | b"from" => Ok((strings_ws(rest), false)),
            b"healthcheck" => health(rest),
            b"maintainer" | b"stopsignal" | b"user" | b"workdir" => Ok((
                if rest.is_empty() {
                    Vec::new()
                } else {
                    vec![Node::arg(rest.to_vec())]
                },
                false,
            )),
            b"onbuild" => self.sub_command(rest),
            // An instruction BuildKit's parser does not know keeps its name, and nothing
            // of its arguments; instructions refuse it later.
            _ => Ok((vec![Node::default()], false)),
        }
    }

    fn sub_command(&mut self, rest: &[u8]) -> Result<Args, Vec<u8>> {
        if rest.is_empty() {
            return Ok((Vec::new(), false));
        }
        if self.depth >= MAX_NESTING {
            return Err(err(format!("ONBUILD nested deeper than {MAX_NESTING} levels")));
        }
        self.depth += 1;
        let child = self.node(rest, Vec::new());
        self.depth -= 1;
        Ok((
            vec![Node {
                children: vec![child?],
                ..Node::default()
            }],
            false,
        ))
    }
}

/// Words separated by spaces, quotes and escapes kept: `parseWords`.
fn words(rest: &[u8], escape: u8) -> Vec<Vec<u8>> {
    let escape = u32::from(escape);
    let mut words = Vec::new();
    let mut word = Vec::new();
    let (mut in_word, mut in_quote, mut quote, mut blank_ok) = (false, false, 0u32, false);
    let mut pos = 0;
    while pos < rest.len() {
        let (ch, width) = go::decode(go::tail(rest, pos));
        if !in_word && !in_quote {
            if go::is_space(ch) {
                pos += width;
                continue;
            }
            in_word = true;
        }
        if in_word {
            if go::is_space(ch) {
                in_word = false;
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                blank_ok = false;
                pos += width;
                continue;
            }
            if ch == u32::from('\'') || ch == u32::from('"') {
                quote = ch;
                blank_ok = true;
                in_word = false;
                in_quote = true;
            }
            let (mut c, mut w) = (ch, width);
            if ch == escape {
                if pos + width == rest.len() {
                    pos += width;
                    continue;
                }
                go::push(&mut word, ch);
                pos += width;
                (c, w) = go::decode(go::tail(rest, pos));
            }
            go::push(&mut word, c);
            pos += w;
            continue;
        }
        // In quotes.
        if ch == quote {
            in_quote = false;
            in_word = true;
        }
        let (mut c, mut w) = (ch, width);
        if ch == escape && quote != u32::from('\'') {
            if pos + width == rest.len() {
                in_quote = false;
                in_word = true;
                pos += width;
                continue;
            }
            pos += width;
            go::push(&mut word, ch);
            (c, w) = go::decode(go::tail(rest, pos));
        }
        go::push(&mut word, c);
        pos += w;
    }
    if (in_word || in_quote) && (blank_ok || !word.is_empty()) {
        words.push(word);
    }
    words
}

/// `KEY=VALUE ...`, or the old `KEY VALUE`: `parseNameVal`.
fn name_val(rest: &[u8], key: &[u8], escape: u8) -> Result<Vec<Node>, Vec<u8>> {
    let ws = words(rest, escape);
    let Some(first) = ws.first() else {
        return Ok(Vec::new());
    };
    if !first.contains(&b'=') {
        let parts = split_ws(rest, Some(2));
        let (Some(k), Some(v)) = (parts.first(), parts.get(1)) else {
            let mut m = key.to_vec();
            m.extend_from_slice(b" must have two arguments");
            return Err(m);
        };
        return Ok(vec![
            Node::arg(k.to_vec()),
            Node::arg(v.to_vec()),
            Node::arg(Vec::new()),
        ]);
    }
    let mut out = Vec::with_capacity(ws.len() * 3);
    for word in ws {
        let Some(at) = word.iter().position(|&b| b == b'=') else {
            let mut m = b"Syntax error - can't find = in ".to_vec();
            m.extend_from_slice(go::quote(&word).as_bytes());
            m.extend_from_slice(b". Must be of the form: name=value");
            return Err(m);
        };
        out.push(Node::arg(go::head(&word, at).to_vec()));
        out.push(Node::arg(go::tail(&word, at + 1).to_vec()));
        out.push(Node::arg(b"=".to_vec()));
    }
    Ok(out)
}

/// `parseNameOrNameVal`: each word as it is.
fn name_or_name_val(rest: &[u8], escape: u8) -> Vec<Node> {
    words(rest, escape).into_iter().map(Node::arg).collect()
}

/// `parseStringsWhitespaceDelimited`.
fn strings_ws(rest: &[u8]) -> Vec<Node> {
    if rest.is_empty() {
        return Vec::new();
    }
    split_ws(rest, None)
        .into_iter()
        .map(|s| Node::arg(s.to_vec()))
        .collect()
}

const NOT_STRINGS: &str = "when using JSON array syntax, arrays must be comprised of strings only";

fn json_array(rest: &[u8]) -> Result<Option<Vec<Node>>, Vec<u8>> {
    match json::array(go::trim_left_space(rest)) {
        Array::Strings(items) => Ok(Some(items.into_iter().map(Node::arg).collect())),
        Array::NotStrings => Err(err(NOT_STRINGS)),
        Array::Not => Ok(None),
    }
}

/// `parseMaybeJSON`: a JSON array, or the whole as one argument.
fn maybe_json(rest: &[u8]) -> Result<Args, Vec<u8>> {
    if rest.is_empty() {
        return Ok((Vec::new(), false));
    }
    Ok(match json_array(rest)? {
        Some(nodes) => (nodes, true),
        None => (vec![Node::arg(rest.to_vec())], false),
    })
}

/// `parseMaybeJSONToList`: a JSON array, or words split by spaces.
fn maybe_json_to_list(rest: &[u8]) -> Result<Args, Vec<u8>> {
    Ok(match json_array(rest)? {
        Some(nodes) => (nodes, true),
        None => (strings_ws(rest), false),
    })
}

/// `parseHealthConfig`: the type, then as `parseMaybeJSON`. BuildKit finds the type's end
/// byte by byte, a byte taken as a Latin-1 character, and so does this, for parity: no
/// UTF-8 sequence's byte is a Latin-1 space but 0x85 and 0xA0, which then end the type.
fn health(rest: &[u8]) -> Result<Args, Vec<u8>> {
    let latin1_space = |b: u8| go::is_space(u32::from(b));
    let sep = rest.iter().position(|&b| latin1_space(b)).unwrap_or(rest.len());
    if sep == 0 {
        return Ok((Vec::new(), false));
    }
    let next = go::tail(rest, sep)
        .iter()
        .position(|&b| !latin1_space(b))
        .map_or(rest.len(), |p| sep + p);
    let (mut cmd, json) = maybe_json(go::tail(rest, next))?;
    let mut nodes = vec![Node::arg(go::head(rest, sep).to_vec())];
    nodes.append(&mut cmd);
    Ok((nodes, json))
}

/// `^(\d*)<<(-?)\s*([^<]*)$` on a word, and the heredoc it names: `ParseHeredoc`.
fn heredoc(word: &[u8]) -> Result<Option<Heredoc>, Vec<u8>> {
    let digits = word.iter().take_while(|b| b.is_ascii_digit()).count();
    let Some(after) = go::tail(word, digits).strip_prefix(b"<<") else {
        return Ok(None);
    };
    let chomp = after.first() == Some(&b'-');
    let after = if chomp { go::tail(after, 1) } else { after };
    let space = after
        .iter()
        .take_while(|&&b| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
        .count();
    let rest = go::tail(after, space);
    if rest.contains(&b'<') || rest.is_empty() {
        return Ok(None);
    }
    // `strconv.ParseUint` with its error ignored: nothing is 0, too large is the most.
    let fd = go::head(word, digits)
        .iter()
        .try_fold(0u64, |n, &d| n.checked_mul(10)?.checked_add(u64::from(d - b'0')));
    let fd = if digits == 0 { 0 } else { fd.unwrap_or(u64::MAX) };
    let mut lex = Lex::new(u32::from('\\'));
    lex.skip_unset_env = true;
    let words = lex.process(rest, &NoEnv).map_err(|e| e.0)?.words;
    if words.len() != 1 {
        return Ok(None);
    }
    lex.raw_quotes = true;
    let raw = lex.process(rest, &NoEnv).map_err(|e| e.0)?.words;
    if raw.len() != words.len() {
        let mut m = b"internal lexing of heredoc produced inconsistent results: ".to_vec();
        m.extend_from_slice(rest);
        return Err(m);
    }
    let quotes = |w: &[u8]| w.iter().filter(|&&b| b == b'\'' || b == b'"').count();
    let name = words.into_iter().next().unwrap_or_default();
    let expand = raw.first().is_some_and(|r| quotes(&name) == quotes(r));
    Ok(Some(Heredoc {
        name,
        file_descriptor: fd,
        expand,
        chomp,
        content: Vec::new(),
    }))
}

/// The heredoc `word` names, if it names one: `MustParseHeredoc`, errors as none.
pub(crate) fn heredoc_word(word: &[u8]) -> Option<Heredoc> {
    heredoc(word).ok().flatten()
}

/// The heredocs a line opens: `heredocsFromLine`.
fn heredocs(line: &[u8]) -> Result<Vec<Heredoc>, Vec<u8>> {
    let mut lex = Lex::new(u32::from('\\'));
    lex.raw_quotes = true;
    lex.raw_escapes = true;
    lex.skip_unset_env = true;
    let words = lex.process(line, &NoEnv).map(|p| p.words).unwrap_or_default();
    let mut docs = Vec::new();
    for w in words {
        if let Some(h) = heredoc(&w)? {
            docs.push(h);
        }
    }
    Ok(docs)
}

/// Whether `node` may take heredocs: `ADD`, `COPY` and `RUN`, also under `ONBUILD`, not in
/// JSON form.
fn takes_heredocs(node: &Node) -> bool {
    let mut n = node;
    if go::to_lower(&n.value) == b"onbuild"
        && let Some(child) = n.next.first().and_then(|a| a.children.first())
    {
        n = child;
    }
    matches!(go::to_lower(&n.value).as_slice(), b"add" | b"copy" | b"run") && !n.json
}

/// Lines with their endings, as BuildKit's scanner splits them.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = go::tail(text, start)
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |e| start + e + 1);
        out.push(go::span(text, start, end));
        start = end;
    }
    out
}

const EMPTY_CONTINUATION: &str = "https://docs.docker.com/go/dockerfile/rule/no-empty-continuation/";

/// Parses a Dockerfile: `parser.Parse`.
pub fn parse(text: &[u8]) -> Result<Parsed, Error> {
    let mut d = Directives {
        done: false,
        seen: Vec::new(),
        escape: b'\\',
    };
    let mut p = Parser {
        escape: b'\\',
        depth: 0,
    };
    let all = lines(text);
    let mut it = all.iter().copied();
    let mut current = 0usize;
    let mut comments: Vec<Vec<u8>> = Vec::new();
    let mut warnings = Vec::new();
    let mut instructions: Vec<Node> = Vec::new();

    while let Some(raw) = it.next() {
        let mut read = raw;
        if current == 0 {
            read = read.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(read);
        }
        if is_comment(read) {
            let c = go::trim_space(go::tail(read, 1));
            if c.is_empty() {
                comments.clear();
            } else {
                comments.push(c.to_vec());
            }
        }
        let (line, directive) = process_line(&mut d, read, true).map_err(|m| located(m, current, 0))?;
        if directive {
            comments.pop();
        }
        p.escape = d.escape;
        current += 1;
        let start = current;
        let (first, mut end_of_line) = trim_continuation(line, d.escape);
        if end_of_line && first.is_empty() {
            continue;
        }
        let mut buf = first.to_vec();
        let mut empty_continuation = false;
        while !end_of_line {
            let Some(raw) = it.next() else { break };
            let (more, _) = process_line(&mut d, raw, false).map_err(|m| located(m, current, 0))?;
            current += 1;
            if is_comment(raw) {
                continue;
            }
            if go::trim_left_space(trim_newline(more)).is_empty() {
                empty_continuation = true;
                continue;
            }
            let (more, eol) = trim_continuation(more, d.escape);
            end_of_line = eol;
            buf.extend_from_slice(more);
        }
        if empty_continuation {
            let mut short = b"Empty continuation line found in: ".to_vec();
            short.extend_from_slice(&buf);
            warnings.push(Warning {
                short,
                url: EMPTY_CONTINUATION,
                line: current,
            });
        }
        let mut child = p
            .node(&buf, std::mem::take(&mut comments))
            .map_err(|m| located(m, start, current))?;
        if takes_heredocs(&child) && buf.windows(2).any(|w| w == b"<<") {
            let docs = heredocs(&buf).map_err(|m| located(m, start, current))?;
            for mut doc in docs {
                let mut terminated = false;
                let mut content = Vec::new();
                for raw in it.by_ref() {
                    current += 1;
                    let mut candidate = trim_newline(raw);
                    if doc.chomp {
                        let tabs = candidate.iter().take_while(|&&b| b == b'\t').count();
                        candidate = go::tail(candidate, tabs);
                    }
                    if candidate == doc.name.as_slice() {
                        terminated = true;
                        break;
                    }
                    content.extend_from_slice(raw);
                }
                if !terminated {
                    return Err(located(err("unterminated heredoc"), start, current));
                }
                doc.content = content;
                child.heredocs.push(doc);
            }
        }
        child.start_line = start;
        child.end_line = current;
        instructions.push(child);
    }
    if instructions.is_empty() {
        return Err(located(err("file with no instructions"), current, 0));
    }
    Ok(Parsed {
        instructions,
        escape: d.escape,
        warnings,
    })
}

/// A line's newline and, with `strip`, its leading space removed, directives taken, and a
/// comment emptied: `processLine`.
fn process_line<'a>(d: &mut Directives, line: &'a [u8], strip: bool) -> Result<(&'a [u8], bool), Vec<u8>> {
    let mut token = trim_newline(line);
    if strip {
        token = go::trim_left_space(token);
    }
    let directive = d.line(token)?;
    Ok((if is_comment(token) { &[] } else { token }, directive))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Nesting stops at its bound with an error, not a stack overflow.
    #[test]
    fn nesting_is_bounded() {
        let mut text = b"FROM a\n".to_vec();
        text.extend(std::iter::repeat_n(b"ONBUILD ".as_slice(), 100_000).flatten());
        text.extend_from_slice(b"RUN x\n");
        let e = parse(&text).unwrap_err();
        assert_eq!(
            e.message,
            format!("ONBUILD nested deeper than {MAX_NESTING} levels").into_bytes()
        );
        let ok = parse(b"FROM a\nONBUILD ONBUILD RUN x\n").unwrap();
        assert_eq!(
            dump(&ok.instructions),
            b"(from \"a\")\n(onbuild (onbuild (run \"x\")))"
        );
    }
}
