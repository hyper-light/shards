//! Shell-like word processing, as BuildKit's `frontend/dockerfile/shell` lexer does it:
//! quotes, escapes, `$name` and `${name...}` expansion with the modifiers `:-`, `-`, `:+`,
//! `+`, `:?`, `?`, `#`, `##`, `%`, `%%`, `/` and `//`, and the splitting of a word into
//! words after expansion. It reads its input as Go's `text/scanner` does (an invalid byte
//! is U+FFFD, a byte-order mark at the start is skipped), and its results, errors
//! included, are BuildKit's byte for byte (tests/oracle.rs), with one deliberate
//! difference:
//!
//! - **Words are split correctly after a nested expansion.** BuildKit's lexer shares one
//!   word buffer between a word and the `${...}` expansions inside it, and each expansion
//!   with a modifier empties it: `ProcessWords("ab${x:-c}d")` gives `["cd"]`, losing `ab`,
//!   and `EXPOSE 80${PROTO:-/tcp}` loses its port. Here each level has its own buffer and
//!   the result is `["abcd"]`. testdata/deviations.json lists each case where this differs.
//!
//! Patterns (`#`, `%`, `/`) are matched as BuildKit's regular expressions match them,
//! leftmost first, `*` greedy for `##`, `%%` and `/` and lazy for `#` and `%`, `?` and `*`
//! never across a newline, but by a matcher over the pattern's few forms in time linear
//! in the pattern times the value, with no regular-expression engine.

use std::collections::{BTreeSet, HashMap};

use crate::go;

/// The environment words expand from.
pub trait Env {
    fn get(&self, key: &[u8]) -> Option<&[u8]>;
}

/// An environment from `KEY=VALUE` entries, the last of a key winning: `EnvsFromSlice`.
#[derive(Debug, Default, Clone)]
pub struct EnvList {
    values: HashMap<Vec<u8>, Vec<u8>>,
}

impl EnvList {
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a [u8]>) -> EnvList {
        let mut values = HashMap::new();
        for e in entries {
            let (k, v) = match e.iter().position(|&b| b == b'=') {
                Some(at) => (go::head(e, at), go::tail(e, at + 1)),
                None => (e, &[][..]),
            };
            values.insert(k.to_vec(), v.to_vec());
        }
        EnvList { values }
    }
}

impl Env for EnvList {
    fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.values.get(key).map(Vec::as_slice)
    }
}

/// An environment with nothing set.
#[derive(Debug, Clone, Copy)]
pub struct NoEnv;

impl Env for NoEnv {
    fn get(&self, _: &[u8]) -> Option<&[u8]> {
        None
    }
}

/// The lexer's settings, as BuildKit's `shell.Lex` fields have them.
#[derive(Debug, Clone, Copy)]
pub struct Lex {
    /// `\` or `` ` ``.
    pub escape: u32,
    /// Keep quotes in the result.
    pub raw_quotes: bool,
    /// Keep escape tokens in the result.
    pub raw_escapes: bool,
    /// Treat quotes as ordinary characters.
    pub skip_process_quotes: bool,
    /// Leave references to unset variables as they are written.
    pub skip_unset_env: bool,
}

/// A word processed: expanded, and split into words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Processed {
    pub word: Vec<u8>,
    pub words: Vec<Vec<u8>>,
    /// The variables referred to that were set, and those that were not.
    pub matched: BTreeSet<Vec<u8>>,
    pub unmatched: BTreeSet<Vec<u8>>,
}

/// An error, its text BuildKit's (which may hold any bytes of the input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub Vec<u8>);

impl Error {
    fn new(s: &str) -> Error {
        Error(s.as_bytes().to_vec())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl std::error::Error for Error {}

const EOF: i64 = -1;

impl Lex {
    pub fn new(escape: u32) -> Lex {
        Lex {
            escape,
            raw_quotes: false,
            raw_escapes: false,
            skip_process_quotes: false,
            skip_unset_env: false,
        }
    }

    /// `word` expanded with `env`, and split into words: `ProcessWordWithMatches` and
    /// `ProcessWords` at once.
    pub fn process(&self, word: &[u8], env: &dyn Env) -> Result<Processed, Error> {
        let mut w = Word {
            lex: *self,
            raw_escapes: self.raw_escapes,
            env,
            scanner: Scanner::new(word),
            depth: 0,
            matched: BTreeSet::new(),
            unmatched: BTreeSet::new(),
        };
        match w.stop_on(None, self.raw_escapes) {
            Ok((result, words)) => Ok(Processed {
                word: result,
                words,
                matched: w.matched,
                unmatched: w.unmatched,
            }),
            Err(Error(inner)) => {
                let mut msg = format!("failed to process {}: ", go::quote(word)).into_bytes();
                msg.extend_from_slice(&inner);
                Err(Error(msg))
            }
        }
    }
}

/// Go's `text/scanner` read rune by rune.
struct Scanner<'a> {
    src: &'a [u8],
    at: usize,
    /// The rune peeked: -2 before the first, EOF at the end.
    ch: i64,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a [u8]) -> Scanner<'a> {
        Scanner { src, at: 0, ch: -2 }
    }

    fn read(&mut self) -> i64 {
        match self.src.get(self.at..).filter(|r| !r.is_empty()) {
            None => EOF,
            Some(rest) => {
                let (r, w) = go::decode(rest);
                self.at += w;
                i64::from(r)
            }
        }
    }

    fn peek(&mut self) -> i64 {
        if self.ch == -2 {
            self.ch = self.read();
            if self.ch == 0xFEFF {
                self.ch = self.read();
            }
        }
        self.ch
    }

    fn next(&mut self) -> i64 {
        let ch = self.peek();
        if ch != EOF {
            self.ch = self.read();
        }
        ch
    }
}

/// `string(ch)` in Go: EOF and what is no rune become U+FFFD.
fn push_ch(out: &mut Vec<u8>, ch: i64) {
    go::push(out, u32::try_from(ch).unwrap_or(go::RUNE_ERROR));
}

fn is_ch(ch: i64, c: char) -> bool {
    ch == i64::from(u32::from(c))
}

/// The words being split, each level of processing its own.
#[derive(Default)]
struct Words {
    buf: Vec<u8>,
    words: Vec<Vec<u8>>,
    in_word: bool,
}

impl Words {
    fn add_char(&mut self, ch: u32) {
        if go::is_space(ch) {
            if self.in_word && !self.buf.is_empty() {
                self.words.push(std::mem::take(&mut self.buf));
                self.in_word = false;
            }
        } else {
            self.add_raw_char(ch);
        }
    }

    fn add_raw_char(&mut self, ch: u32) {
        go::push(&mut self.buf, ch);
        self.in_word = true;
    }

    fn add_string(&mut self, s: &[u8]) {
        for (r, _) in go::runes(s) {
            self.add_char(r);
        }
    }

    fn add_raw_string(&mut self, s: &[u8]) {
        self.buf.extend_from_slice(s);
        self.in_word = true;
    }

    fn finish(mut self) -> Vec<Vec<u8>> {
        if !self.buf.is_empty() {
            self.words.push(self.buf);
        }
        self.words
    }
}

struct Word<'a> {
    lex: Lex,
    raw_escapes: bool,
    env: &'a dyn Env,
    scanner: Scanner<'a>,
    /// How many `${...}` the scanner is inside.
    depth: usize,
    matched: BTreeSet<Vec<u8>>,
    unmatched: BTreeSet<Vec<u8>>,
}

type Stopped = (Vec<u8>, Vec<Vec<u8>>);

impl Word<'_> {
    /// Processes up to `stop` (consumed), or to the end: `processStopOn`.
    fn stop_on(&mut self, stop: Option<u32>, raw_escapes: bool) -> Result<Stopped, Error> {
        if stop.is_some() && self.depth >= crate::parser::MAX_NESTING {
            return Err(Error(
                format!(
                    "${{...}} nested deeper than {} levels",
                    crate::parser::MAX_NESTING
                )
                .into_bytes(),
            ));
        }
        let saved = self.raw_escapes;
        self.raw_escapes = raw_escapes;
        self.depth += usize::from(stop.is_some());
        let done = self.stop_on_inner(stop);
        self.depth -= usize::from(stop.is_some());
        self.raw_escapes = saved;
        done
    }

    fn stop_on_inner(&mut self, stop: Option<u32>) -> Result<Stopped, Error> {
        let mut result = Vec::new();
        let mut words = Words::default();
        let quotes = !self.lex.skip_process_quotes;
        loop {
            let ch = self.scanner.peek();
            if ch == EOF {
                break;
            }
            if let Some(s) = stop
                && ch == i64::from(s)
            {
                self.scanner.next();
                return Ok((result, words.finish()));
            }
            let special = if is_ch(ch, '$') {
                Some(self.dollar()?)
            } else if is_ch(ch, '<') {
                Some(self.possible_heredoc())
            } else if quotes && is_ch(ch, '\'') {
                Some(self.single_quote()?)
            } else if quotes && is_ch(ch, '"') {
                Some(self.double_quote()?)
            } else {
                None
            };
            if let Some(tmp) = special {
                result.extend_from_slice(&tmp);
                if is_ch(ch, '$') {
                    words.add_string(&tmp);
                } else {
                    words.add_raw_string(&tmp);
                }
                continue;
            }
            let mut ch = self.scanner.next();
            if ch == i64::from(self.lex.escape) {
                if self.raw_escapes {
                    words.add_raw_char(self.lex.escape);
                    push_ch(&mut result, ch);
                }
                ch = self.scanner.next();
                if ch == EOF {
                    break;
                }
                words.add_raw_char(u32::try_from(ch).unwrap_or(go::RUNE_ERROR));
            } else {
                words.add_char(u32::try_from(ch).unwrap_or(go::RUNE_ERROR));
            }
            push_ch(&mut result, ch);
        }
        if let Some(s) = stop {
            let mut msg = b"unexpected end of statement while looking for matching ".to_vec();
            go::push(&mut msg, s);
            return Err(Error(msg));
        }
        Ok((result, words.finish()))
    }

    fn single_quote(&mut self) -> Result<Vec<u8>, Error> {
        let mut result = Vec::new();
        let ch = self.scanner.next();
        if self.lex.raw_quotes {
            push_ch(&mut result, ch);
        }
        loop {
            let ch = self.scanner.next();
            if ch == EOF {
                return Err(Error::new(
                    "unexpected end of statement while looking for matching single-quote",
                ));
            }
            if is_ch(ch, '\'') {
                if self.lex.raw_quotes {
                    push_ch(&mut result, ch);
                }
                return Ok(result);
            }
            push_ch(&mut result, ch);
        }
    }

    fn double_quote(&mut self) -> Result<Vec<u8>, Error> {
        let mut result = Vec::new();
        let ch = self.scanner.next();
        if self.lex.raw_quotes {
            push_ch(&mut result, ch);
        }
        loop {
            let peek = self.scanner.peek();
            if peek == EOF {
                return Err(Error::new(
                    "unexpected end of statement while looking for matching double-quote",
                ));
            }
            if is_ch(peek, '"') {
                let ch = self.scanner.next();
                if self.lex.raw_quotes {
                    push_ch(&mut result, ch);
                }
                return Ok(result);
            }
            if is_ch(peek, '$') {
                let value = self.dollar()?;
                result.extend_from_slice(&value);
                continue;
            }
            let mut ch = self.scanner.next();
            if ch == i64::from(self.lex.escape) {
                if self.raw_escapes {
                    push_ch(&mut result, ch);
                }
                let after = self.scanner.peek();
                if after == EOF {
                    // An escape at the end of the word is dropped.
                    continue;
                }
                if is_ch(after, '"') || is_ch(after, '$') || after == i64::from(self.lex.escape) {
                    ch = self.scanner.next();
                }
            }
            push_ch(&mut result, ch);
        }
    }

    fn get_env(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        match self.env.get(name) {
            Some(v) => {
                self.matched.insert(name.to_vec());
                Some(v.to_vec())
            }
            None => {
                self.unmatched.insert(name.to_vec());
                None
            }
        }
    }

    fn dollar(&mut self) -> Result<Vec<u8>, Error> {
        self.scanner.next();
        if !is_ch(self.scanner.peek(), '{') {
            let name = self.name();
            if name.is_empty() {
                return Ok(b"$".to_vec());
            }
            return Ok(match self.get_env(&name) {
                Some(v) => v,
                None if self.lex.skip_unset_env => [b"$".as_slice(), &name].concat(),
                None => Vec::new(),
            });
        }
        self.scanner.next();
        let peek = self.scanner.peek();
        if peek == EOF {
            return Err(Error::new("syntax error: missing '}'"));
        }
        if is_ch(peek, '{') || is_ch(peek, '}') || is_ch(peek, ':') {
            return Err(Error::new("syntax error: bad substitution"));
        }
        let name = self.name();
        let mut ch = self.scanner.next();
        let mut chs = Vec::new();
        push_ch(&mut chs, ch);
        let mut null_is_unset = false;
        if is_ch(ch, '}') {
            return Ok(match self.get_env(&name) {
                Some(v) => v,
                None if self.lex.skip_unset_env => [b"${".as_slice(), &name, b"}"].concat(),
                None => Vec::new(),
            });
        }
        if is_ch(ch, '/') {
            return self.replace(&name);
        }
        if is_ch(ch, ':') {
            null_is_unset = true;
            ch = self.scanner.next();
            push_ch(&mut chs, ch);
        } else if !(is_ch(ch, '+') || is_ch(ch, '-') || is_ch(ch, '?') || is_ch(ch, '#') || is_ch(ch, '%')) {
            return Err(unsupported(&chs));
        }
        let raw = is_ch(ch, '#') || is_ch(ch, '%');
        if null_is_unset && raw {
            return Err(unsupported(&chs));
        }
        let word = match self.stop_on(Some(u32::from('}')), raw) {
            Ok((word, _)) => word,
            Err(e) if self.scanner.peek() == EOF => {
                drop(e);
                return Err(Error::new("syntax error: missing '}'"));
            }
            Err(e) => return Err(e),
        };
        let value = self.get_env(&name);
        if self.lex.skip_unset_env && value.is_none() {
            return Ok([b"${".as_slice(), &name, &chs, &word, b"}"].concat());
        }
        let empty = value.as_ref().is_none_or(Vec::is_empty);
        match u32::try_from(ch).ok().and_then(char::from_u32) {
            Some('-') => Ok(match value {
                Some(v) if !(null_is_unset && v.is_empty()) => v,
                _ => word,
            }),
            Some('+') => Ok(if value.is_none() || (null_is_unset && empty) {
                Vec::new()
            } else {
                word
            }),
            Some('?') => match value {
                None => Err(required(&name, &word, b"is not allowed to be unset")),
                Some(v) if null_is_unset && v.is_empty() => {
                    Err(required(&name, &word, b"is not allowed to be empty"))
                }
                Some(v) => Ok(v),
            },
            Some(c @ ('%' | '#')) => {
                let mut pattern = word.as_slice();
                let greedy = pattern.first() == Some(&(c as u8));
                if greedy {
                    pattern = go::tail(pattern, 1);
                }
                let value = value.unwrap_or_default();
                if c == '%' {
                    trim_suffix(pattern, &value, greedy)
                } else {
                    trim_prefix(pattern, &value, greedy)
                }
            }
            _ => Err(unsupported(&chs)),
        }
    }

    /// `${name/pattern/replacement}` and `${name//pattern/replacement}`.
    fn replace(&mut self, name: &[u8]) -> Result<Vec<u8>, Error> {
        let all = is_ch(self.scanner.peek(), '/');
        if all {
            self.scanner.next();
        }
        let pattern = match self.stop_on(Some(u32::from('/')), true) {
            Ok((p, _)) => p,
            Err(_) if self.scanner.peek() == EOF => {
                return Err(Error::new("syntax error: missing '/' in ${}"));
            }
            Err(e) => return Err(e),
        };
        let replacement = match self.stop_on(Some(u32::from('}')), true) {
            Ok((r, _)) => r,
            Err(_) if self.scanner.peek() == EOF => return Err(Error::new("syntax error: missing '}'")),
            Err(e) => return Err(e),
        };
        let value = match self.get_env(name) {
            Some(v) => v,
            None if self.lex.skip_unset_env => {
                return Ok([b"${".as_slice(), name, b"/", &pattern, b"/", &replacement, b"}"].concat());
            }
            None => Vec::new(),
        };
        let re = Pattern::compile(&pattern, true).map_err(|e| invalid_pattern(&pattern, &e))?;
        if all {
            return Ok(re.replace_all(&value, &replacement));
        }
        Ok(match re.find(&value) {
            Some((a, b)) => [go::head(&value, a), replacement.as_slice(), go::tail(&value, b)].concat(),
            None => value,
        })
    }

    /// A name: digits, or one special parameter, or letters, digits and `_`.
    fn name(&mut self) -> Vec<u8> {
        let mut name = Vec::new();
        loop {
            let ch = self.scanner.peek();
            if ch == EOF {
                break;
            }
            let r = u32::try_from(ch).unwrap_or(go::RUNE_ERROR);
            if name.is_empty() && go::is_digit(r) {
                while self.scanner.peek() != EOF
                    && go::is_digit(u32::try_from(self.scanner.peek()).unwrap_or(go::RUNE_ERROR))
                {
                    let ch = self.scanner.next();
                    push_ch(&mut name, ch);
                }
                return name;
            }
            if name.is_empty()
                && matches!(
                    char::from_u32(r),
                    Some('@' | '*' | '#' | '?' | '-' | '$' | '!' | '0')
                )
            {
                let ch = self.scanner.next();
                push_ch(&mut name, ch);
                return name;
            }
            if !go::is_letter(r) && !go::is_digit(r) && r != u32::from('_') {
                break;
            }
            let ch = self.scanner.next();
            push_ch(&mut name, ch);
        }
        name
    }

    fn possible_heredoc(&mut self) -> Vec<u8> {
        self.scanner.next();
        if !is_ch(self.scanner.peek(), '<') {
            return b"<".to_vec();
        }
        self.scanner.next();
        let mut out = b"<<".to_vec();
        while matches!(self.scanner.peek(), 0x09 | 0x0d | 0x20) {
            let ch = self.scanner.next();
            push_ch(&mut out, ch);
        }
        out
    }
}

fn unsupported(chs: &[u8]) -> Error {
    let mut msg = b"unsupported modifier (".to_vec();
    msg.extend_from_slice(chs);
    msg.extend_from_slice(b") in substitution");
    Error(msg)
}

fn required(name: &[u8], word: &[u8], default: &[u8]) -> Error {
    let message = if word.is_empty() { default } else { word };
    Error([name, b": ", message].concat())
}

fn invalid_pattern(pattern: &[u8], err: &[u8]) -> Error {
    Error(
        [
            b"invalid pattern (".as_slice(),
            pattern,
            b") in substitution: ",
            err,
        ]
        .concat(),
    )
}

/// `${name#pattern}`: the value without the shortest (or longest) prefix matching.
fn trim_prefix(pattern: &[u8], value: &[u8], greedy: bool) -> Result<Vec<u8>, Error> {
    let re = Pattern::compile(pattern, greedy).map_err(|e| invalid_pattern(pattern, &e))?;
    Ok(match re.anchored(value) {
        Some(end) => go::tail(value, end).to_vec(),
        None => value.to_vec(),
    })
}

/// `${name%pattern}`: as BuildKit does it, the prefix trim of the reversed value by the
/// reversed pattern, reversed back, each reversal by runes (an invalid byte becoming
/// U+FFFD).
fn trim_suffix(pattern: &[u8], value: &[u8], greedy: bool) -> Result<Vec<u8>, Error> {
    let pattern = reverse_pattern(pattern);
    let value = reverse(value);
    let trimmed = trim_prefix(&pattern, &value, greedy)?;
    Ok(reverse(&trimmed))
}

fn reverse(b: &[u8]) -> Vec<u8> {
    let runes: Vec<u32> = go::runes(b).map(|(r, _)| r).collect();
    let mut out = Vec::with_capacity(b.len());
    for &r in runes.iter().rev() {
        go::push(&mut out, r);
    }
    out
}

/// The pattern reversed, an escape staying before what it escapes.
fn reverse_pattern(b: &[u8]) -> Vec<u8> {
    // Escapes pair with what follows them; the pairs, in reverse, stay escape first.
    let runes: Vec<u32> = go::runes(b).map(|(r, _)| r).collect();
    let mut units: Vec<&[u32]> = Vec::with_capacity(runes.len());
    let mut rest = runes.as_slice();
    while let Some((&tok, after)) = rest.split_first() {
        let take = if tok == u32::from('\\') && !after.is_empty() {
            2
        } else {
            1
        };
        let Some((unit, more)) = rest.split_at_checked(take) else {
            break;
        };
        units.push(unit);
        rest = more;
    }
    let mut bytes = Vec::with_capacity(b.len());
    for r in units.into_iter().rev().flatten().copied() {
        go::push(&mut bytes, r);
    }
    bytes
}

/// A shell pattern: `?` any one rune, `*` any runes, the rest literal; neither crosses a
/// newline, as `.` in Go's regular expressions does not.
#[derive(Debug)]
struct Pattern {
    toks: Vec<Tok>,
    greedy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Lit(u32),
    Any,
    Star,
}

impl Pattern {
    /// `convertShellPatternToRegex`, its errors included.
    fn compile(pattern: &[u8], greedy: bool) -> Result<Pattern, Vec<u8>> {
        let mut s = Scanner::new(pattern);
        let mut toks = Vec::new();
        loop {
            let tok = s.next();
            if tok == EOF {
                break;
            }
            if is_ch(tok, '*') {
                toks.push(Tok::Star);
            } else if is_ch(tok, '?') {
                toks.push(Tok::Any);
            } else if is_ch(tok, '\\') {
                let peek = s.peek();
                if is_ch(peek, '}') || is_ch(peek, '/') {
                    // The escape of } or / inside ${} is no part of the pattern.
                    continue;
                }
                let tok = s.next();
                if !(is_ch(tok, '*') || is_ch(tok, '?') || is_ch(tok, '\\')) {
                    let mut msg = b"invalid escape '\\".to_vec();
                    push_ch(&mut msg, tok);
                    msg.push(b'\'');
                    return Err(msg);
                }
                toks.push(Tok::Lit(u32::try_from(tok).unwrap_or(go::RUNE_ERROR)));
            } else {
                toks.push(Tok::Lit(u32::try_from(tok).unwrap_or(go::RUNE_ERROR)));
            }
        }
        Ok(Pattern { toks, greedy })
    }

    /// For each token `i` and rune position `j`, whether tokens `i..` match from `j`,
    /// ending anywhere: one table, filled right to left, in time linear in their product.
    fn table(&self, runes: &[(u32, usize)]) -> Table {
        let (m, n) = (self.toks.len(), runes.len());
        let mut can = Table::new(m + 1, n + 1);
        for j in 0..=n {
            can.set(m, j, true);
        }
        for i in (0..m).rev() {
            for j in (0..=n).rev() {
                let here = runes.get(j).map(|&(r, _)| r);
                let v = match self.toks.get(i) {
                    Some(Tok::Lit(c)) => here == Some(*c) && can.get(i + 1, j + 1),
                    Some(Tok::Any) => here.is_some_and(|r| r != 0x0a) && can.get(i + 1, j + 1),
                    Some(Tok::Star) => {
                        can.get(i + 1, j) || (here.is_some_and(|r| r != 0x0a) && can.get(i, j + 1))
                    }
                    None => false,
                };
                can.set(i, j, v);
            }
        }
        can
    }

    /// The rune position the preferred match from `j` ends at, given that one exists:
    /// leftmost-first, each `*` taking the most runes (greedy) or the fewest.
    fn end(&self, runes: &[(u32, usize)], can: &Table, mut j: usize) -> usize {
        for (i, tok) in self.toks.iter().enumerate() {
            match tok {
                Tok::Lit(_) | Tok::Any => j += 1,
                Tok::Star => {
                    let mut lim = j;
                    while runes.get(lim).is_some_and(|&(r, _)| r != 0x0a) {
                        lim += 1;
                    }
                    let found = if self.greedy {
                        (j..=lim).rev().find(|&k| can.get(i + 1, k))
                    } else {
                        (j..=lim).find(|&k| can.get(i + 1, k))
                    };
                    j = found.unwrap_or(j);
                }
            }
        }
        j
    }

    /// The runes of `b`, and each rune's byte offset with `b`'s length last.
    fn runes(b: &[u8]) -> (Vec<(u32, usize)>, Vec<usize>) {
        let mut runes = Vec::new();
        let mut offsets = Vec::new();
        let mut at = 0;
        for (r, w) in go::runes(b) {
            runes.push((r, at));
            offsets.push(at);
            at += w;
        }
        offsets.push(b.len());
        (runes, offsets)
    }

    /// The end of the preferred match at the start of `value`, if any.
    fn anchored(&self, value: &[u8]) -> Option<usize> {
        let (runes, offsets) = Self::runes(value);
        let can = self.table(&runes);
        if !can.get(0, 0) {
            return None;
        }
        offsets.get(self.end(&runes, &can, 0)).copied()
    }

    /// The leftmost match, as byte offsets.
    fn find(&self, value: &[u8]) -> Option<(usize, usize)> {
        let (runes, offsets) = Self::runes(value);
        let can = self.table(&runes);
        let j = (0..=runes.len()).find(|&j| can.get(0, j))?;
        Some((*offsets.get(j)?, *offsets.get(self.end(&runes, &can, j))?))
    }

    /// Every match replaced, as Go's `ReplaceAllString` replaces them: an empty match
    /// right after another is skipped, and `$0`/`${0}` in the replacement is the match.
    fn replace_all(&self, value: &[u8], replacement: &[u8]) -> Vec<u8> {
        let (runes, offsets) = Self::runes(value);
        let can = self.table(&runes);
        let mut out = Vec::new();
        let (mut last_end, mut search) = (0usize, 0usize);
        while search <= value.len() {
            let Some(from) = offsets.iter().position(|&o| o >= search) else {
                break;
            };
            let Some(j) = (from..=runes.len()).find(|&j| can.get(0, j)) else {
                break;
            };
            let (Some(&a), Some(&b)) = (offsets.get(j), offsets.get(self.end(&runes, &can, j))) else {
                break;
            };
            out.extend_from_slice(go::span(value, last_end, a));
            if b > last_end || a == 0 {
                expand(&mut out, replacement, go::span(value, a, b));
            }
            last_end = b;
            let (_, width) = go::decode(go::tail(value, search));
            if search + width > b {
                search += width;
            } else if search + 1 > b {
                search += 1;
            } else {
                search = b;
            }
        }
        out.extend_from_slice(go::tail(value, last_end));
        out
    }
}

/// A table of bits, `rows` by `cols`; reads outside it are false.
struct Table {
    bits: Vec<u64>,
    cols: usize,
}

impl Table {
    fn new(rows: usize, cols: usize) -> Table {
        Table {
            bits: vec![0; (rows * cols).div_ceil(64)],
            cols,
        }
    }

    fn get(&self, row: usize, col: usize) -> bool {
        if col >= self.cols {
            return false;
        }
        let at = row * self.cols + col;
        self.bits.get(at / 64).is_some_and(|w| w >> (at % 64) & 1 == 1)
    }

    fn set(&mut self, row: usize, col: usize, v: bool) {
        let at = row * self.cols + col;
        if let Some(w) = self.bits.get_mut(at / 64) {
            if v {
                *w |= 1 << (at % 64);
            } else {
                *w &= !(1 << (at % 64));
            }
        }
    }
}

/// Go's `Regexp.expand` for a pattern without groups: `$$` is `$`, `$0` and `${0}` the
/// match, any other group or name nothing, a malformed reference a literal `$`.
fn expand(out: &mut Vec<u8>, mut template: &[u8], matched: &[u8]) {
    while let Some(at) = template.iter().position(|&b| b == b'$') {
        out.extend_from_slice(go::head(template, at));
        template = go::tail(template, at + 1);
        if template.first() == Some(&b'$') {
            out.push(b'$');
            template = go::tail(template, 1);
            continue;
        }
        match extract(template) {
            None => out.push(b'$'),
            Some((num, rest)) => {
                if num == Some(0) {
                    out.extend_from_slice(matched);
                }
                template = rest;
            }
        }
    }
    out.extend_from_slice(template);
}

/// Go's `extract`: a reference's group number (none for a name), and what follows it.
fn extract(s: &[u8]) -> Option<(Option<u64>, &[u8])> {
    let brace = s.first() == Some(&b'{');
    let s = if brace { go::tail(s, 1) } else { s };
    let mut i = 0;
    while i < s.len() {
        let (r, w) = go::decode(go::tail(s, i));
        if !go::is_letter(r) && !go::is_digit(r) && r != u32::from('_') {
            break;
        }
        i += w;
    }
    if i == 0 {
        return None;
    }
    let name = go::head(s, i);
    if brace {
        if s.get(i) != Some(&b'}') {
            return None;
        }
        i += 1;
    }
    let mut num: Option<u64> = Some(0);
    for &c in name {
        match num {
            Some(n) if c.is_ascii_digit() && n < 100_000_000 => num = Some(n * 10 + u64::from(c - b'0')),
            _ => {
                num = None;
                break;
            }
        }
    }
    if name.first() == Some(&b'0') && name.len() > 1 {
        num = None;
    }
    Some((num, go::tail(s, i)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A long value against a pattern of many stars takes time linear in their product,
    /// where a backtracking matcher would take exponential time.
    #[test]
    fn patterns_match_in_linear_time() {
        let value = vec![b'a'; 20_000];
        let pattern = b"*a*a*a*a*a*a*a*a*b";
        let started = std::time::Instant::now();
        assert_eq!(trim_prefix(pattern, &value, true).unwrap(), value);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// Nesting stops at its bound with an error, not a stack overflow.
    #[test]
    fn nesting_is_bounded() {
        let deep = [b"${a:-".repeat(100_000), b"}".repeat(100_000)].concat();
        let e = Lex::new(u32::from('\\')).process(&deep, &NoEnv).unwrap_err();
        assert!(
            String::from_utf8_lossy(&e.0).ends_with("nested deeper than 64 levels"),
            "{e}"
        );
    }

    /// Each level of expansion keeps its own words.
    #[test]
    fn words_survive_nested_expansions() {
        let lex = Lex::new(u32::from('\\'));
        let env = EnvList::from_entries([b"A=hello".as_slice()]);
        let p = lex.process(b"ab${x:-c}d ${A:-$B}${C:-${A}}", &env).unwrap();
        assert_eq!(p.words, vec![b"abcd".to_vec(), b"hellohello".to_vec()]);
    }
}
