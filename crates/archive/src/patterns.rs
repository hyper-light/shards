//! Exclude patterns as moby/patternmatcher v0.6.1 matches them (patternmatcher.go), which
//! go-archive's TarWithOptions applies: a pattern is cleaned, checked by Go's
//! filepath.Match (path/filepath/match.go), then matched exactly, as a prefix, as a
//! suffix, or as the regular expression patternmatcher writes of it.
//!
//! The expressions are Go regexp (RE2) syntax, run here by a small automaton that keeps
//! RE2's guarantee of time linear in the name. It knows what patternmatcher writes and
//! what a pattern's own `[...]` classes and `\` escapes can add: literals, `.`, classes
//! with ranges, negation, POSIX and Perl names, `*`, `+`, `?`, groups, and the `^`, `$`,
//! `\A`, `\z`, `\b`, `\B` assertions. Unicode classes (`\p`), `\Q...\E` and flags, which
//! RE2 also reads, are refused as bad patterns.

use crate::error::{Error, decode_rune};
use crate::gopath::{self, NATIVE, Os};

const ERR_BAD_PATTERN: &str = "syntax error in pattern";

fn bad() -> Error {
    Error::other(ERR_BAD_PATTERN)
}

/// A parsed exclude pattern: Go's Pattern.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    cleaned: Vec<u8>,
    exclusion: bool,
    kind: Result<Kind, ()>,
}

#[derive(Debug, Clone)]
enum Kind {
    Exact,
    Prefix,
    Suffix,
    Regexp(Option<Prog>),
}

impl Pattern {
    /// The pattern, cleaned and without its `!`: Go's String.
    pub(crate) fn text(&self) -> &[u8] {
        &self.cleaned
    }

    pub(crate) fn exclusion(&self) -> bool {
        self.exclusion
    }

    /// Pattern.match.
    fn matches(&self, path: &[u8]) -> Result<bool, Error> {
        let Ok(kind) = &self.kind else {
            return Err(bad());
        };
        let p = &self.cleaned;
        Ok(match kind {
            Kind::Exact => path == p.as_slice(),
            Kind::Prefix => path.starts_with(p.get(..p.len().saturating_sub(2)).unwrap_or_default()),
            Kind::Suffix => {
                let suffix = p.get(2..).unwrap_or_default();
                path.ends_with(suffix)
                    || (suffix.first() == Some(&NATIVE.sep()) && suffix.get(1..) == Some(path))
            }
            Kind::Regexp(None) => return Err(bad()),
            Kind::Regexp(Some(prog)) => prog.matches(path),
        })
    }
}

/// Go's PatternMatcher.
#[derive(Debug, Clone, Default)]
pub(crate) struct PatternMatcher {
    patterns: Vec<Pattern>,
    exclusions: bool,
}

/// What MatchesUsingParentResults learned of a directory, for its children.
#[derive(Debug, Clone, Default)]
pub(crate) struct MatchInfo {
    parent_matched: Vec<bool>,
}

impl PatternMatcher {
    /// New: blank patterns dropped, each cleaned; `!` marks an exception.
    pub(crate) fn new(patterns: &[Vec<u8>]) -> Result<PatternMatcher, Error> {
        let mut pm = PatternMatcher::default();
        for p in patterns {
            let p = trim_space(p);
            if p.is_empty() {
                continue;
            }
            let mut p = gopath::clean(NATIVE, p);
            let mut exclusion = false;
            if p.first() == Some(&b'!') {
                if p.len() == 1 {
                    return Err(Error::other("illegal exclusion pattern: \"!\""));
                }
                exclusion = true;
                p.remove(0);
                pm.exclusions = true;
            }
            go_match(NATIVE, &p, b".")?;
            let kind = compile(NATIVE, &p);
            pm.patterns.push(Pattern {
                cleaned: p,
                exclusion,
                kind,
            });
        }
        Ok(pm)
    }

    pub(crate) fn exclusions(&self) -> bool {
        self.exclusions
    }

    pub(crate) fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// MatchesUsingParentResults: whether `file` is excluded, given what its parent
    /// directory's call returned.
    pub(crate) fn matches_using_parent_results(
        &self,
        file: &[u8],
        parent: &MatchInfo,
    ) -> Result<(bool, MatchInfo), Error> {
        let parent_matched = &parent.parent_matched;
        if !parent_matched.is_empty() && parent_matched.len() != self.patterns.len() {
            return Err(Error::other("wrong number of values in parentMatched"));
        }
        let file = gopath::from_slash(NATIVE, file);
        let mut matched = false;
        let mut info = MatchInfo {
            parent_matched: vec![false; self.patterns.len()],
        };
        for (i, pattern) in self.patterns.iter().enumerate() {
            let mut hit = parent_matched.get(i).copied().unwrap_or(false);
            if !hit {
                if pattern.exclusion != matched {
                    continue;
                }
                hit = pattern.matches(&file)?;
                if !hit && parent_matched.is_empty() {
                    let parent_path = gopath::dir(NATIVE, &file);
                    if parent_path != b"." {
                        let dirs: Vec<&[u8]> = parent_path.split(|&c| c == NATIVE.sep()).collect();
                        for n in 1..=dirs.len() {
                            let prefix = dirs.get(..n).unwrap_or_default().join(&NATIVE.sep());
                            if pattern.matches(&prefix).unwrap_or(false) {
                                hit = true;
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(slot) = info.parent_matched.get_mut(i) {
                *slot = hit;
            }
            if hit {
                matched = !pattern.exclusion;
            }
        }
        Ok((matched, info))
    }
}

/// strings.TrimSpace.
fn trim_space(p: &[u8]) -> &[u8] {
    match std::str::from_utf8(p) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => p.trim_ascii(),
    }
}

/// The runes of `s` as Go reads a string: a byte that starts no rune is U+FFFD.
fn runes(s: &[u8]) -> Vec<char> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let (c, w) = decode_rune(rest);
        out.push(c.unwrap_or('\u{fffd}'));
        rest = rest.get(w.max(1)..).unwrap_or_default();
    }
    out
}

/// Pattern.compile: exact, prefix (`**` at the end), suffix (`**` at the start), or a
/// regular expression.
fn compile(os: Os, pattern: &[u8]) -> Result<Kind, ()> {
    let sl = os.sep() as char;
    let esc_sl = if sl == '\\' {
        "\\\\".to_string()
    } else {
        sl.to_string()
    };
    let mut re = String::from("^");
    let mut kind = 0u8; // 0 exact, 1 prefix, 2 suffix, 3 regexp
    let chars = runes(pattern);
    let mut at = 0;
    let mut i = 0;
    while let Some(&ch) = chars.get(at) {
        at += 1;
        let peek = chars.get(at).copied();
        if ch == '*' {
            if peek == Some('*') {
                at += 1;
                if chars.get(at) == Some(&sl) {
                    at += 1;
                }
                if chars.get(at).is_none() {
                    if kind == 0 {
                        kind = 1;
                    } else {
                        re.push_str(".*");
                        kind = 3;
                    }
                } else {
                    re.push_str(&format!("(.*{esc_sl})?"));
                    kind = 3;
                }
                if i == 0 {
                    kind = 2;
                }
            } else {
                re.push_str(&format!("[^{esc_sl}]*"));
                kind = 3;
            }
        } else if ch == '?' {
            re.push_str(&format!("[^{esc_sl}]"));
            kind = 3;
        } else if ".+()|{}$".contains(ch) {
            re.push('\\');
            re.push(ch);
        } else if ch == '\\' {
            if sl == '\\' {
                re.push_str(&esc_sl);
                i += 1;
                continue;
            }
            match peek {
                Some(next) => {
                    at += 1;
                    re.push('\\');
                    re.push(next);
                    kind = 3;
                }
                None => re.push('\\'),
            }
        } else if ch == '[' || ch == ']' {
            re.push(ch);
            kind = 3;
        } else {
            re.push(ch);
        }
        i += 1;
    }
    Ok(match kind {
        0 => Kind::Exact,
        1 => Kind::Prefix,
        2 => Kind::Suffix,
        _ => {
            re.push('$');
            Kind::Regexp(Prog::compile(&re).ok())
        }
    })
}

/// filepath.Match, for its errors: New checks each pattern against ".".
fn go_match(os: Os, pattern: &[u8], name: &[u8]) -> Result<bool, Error> {
    let sep = os.sep();
    let mut pattern = pattern;
    let mut name = name;
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(os, pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            return Ok(!name.contains(&sep));
        }
        let (t, ok) = match_chunk(os, chunk, name)?;
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if star {
            let mut i = 0;
            while i < name.len() && name.get(i) != Some(&sep) {
                let (t, ok) = match_chunk(os, chunk, name.get(i + 1..).unwrap_or_default())?;
                if ok {
                    if pattern.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
                i += 1;
            }
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

fn scan_chunk(os: Os, pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut p = pattern;
    let mut star = false;
    while p.first() == Some(&b'*') {
        p = p.get(1..).unwrap_or_default();
        star = true;
    }
    let mut inrange = false;
    let mut i = 0;
    while i < p.len() {
        match p.get(i) {
            Some(b'\\') if os != Os::Windows && i + 1 < p.len() => i += 1,
            Some(b'[') => inrange = true,
            Some(b']') => inrange = false,
            Some(b'*') if !inrange => {
                let (chunk, rest) = p.split_at_checked(i).unwrap_or((p, &[]));
                return (star, chunk, rest);
            }
            _ => {}
        }
        i += 1;
    }
    (star, p, &[])
}

fn match_chunk<'a>(os: Os, chunk: &[u8], s: &'a [u8]) -> Result<(&'a [u8], bool), Error> {
    let sep = os.sep();
    let mut chunk = chunk;
    let mut s = s;
    let mut failed = false;
    while let Some(&c) = chunk.first() {
        failed = failed || s.is_empty();
        match c {
            b'[' => {
                let mut r = '\0';
                if !failed {
                    let (rc, n) = decode_rune(s);
                    r = rc.unwrap_or('\u{fffd}');
                    s = s.get(n.max(1)..).unwrap_or_default();
                }
                chunk = chunk.get(1..).unwrap_or_default();
                let mut negated = false;
                if chunk.first() == Some(&b'^') {
                    negated = true;
                    chunk = chunk.get(1..).unwrap_or_default();
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if chunk.first() == Some(&b']') && nrange > 0 {
                        chunk = chunk.get(1..).unwrap_or_default();
                        break;
                    }
                    let (lo, rest) = get_esc(os, chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    if chunk.first() == Some(&b'-') {
                        let (h, rest) = get_esc(os, chunk.get(1..).unwrap_or_default())?;
                        hi = h;
                        chunk = rest;
                    }
                    matched = matched || (lo <= r && r <= hi);
                    nrange += 1;
                }
                failed = failed || matched == negated;
            }
            b'?' => {
                if !failed {
                    failed = s.first() == Some(&sep);
                    let (_, n) = decode_rune(s);
                    s = s.get(n.max(1)..).unwrap_or_default();
                }
                chunk = chunk.get(1..).unwrap_or_default();
            }
            _ => {
                if c == b'\\' && os != Os::Windows {
                    chunk = chunk.get(1..).unwrap_or_default();
                    if chunk.is_empty() {
                        return Err(bad());
                    }
                }
                if !failed {
                    failed = chunk.first() != s.first();
                    s = s.get(1..).unwrap_or_default();
                }
                chunk = chunk.get(1..).unwrap_or_default();
            }
        }
    }
    if failed {
        return Ok((&[], false));
    }
    Ok((s, true))
}

fn get_esc(os: Os, chunk: &[u8]) -> Result<(char, &[u8]), Error> {
    let mut chunk = chunk;
    if chunk.is_empty() || chunk.first() == Some(&b'-') || chunk.first() == Some(&b']') {
        return Err(bad());
    }
    if chunk.first() == Some(&b'\\') && os != Os::Windows {
        chunk = chunk.get(1..).unwrap_or_default();
        if chunk.is_empty() {
            return Err(bad());
        }
    }
    let (r, n) = decode_rune(chunk);
    let Some(r) = r else {
        return Err(bad());
    };
    let rest = chunk.get(n..).unwrap_or_default();
    if rest.is_empty() {
        return Err(bad());
    }
    Ok((r, rest))
}

/// A set of runes: ranges, maybe negated.
#[derive(Debug, Clone)]
struct Class {
    ranges: Vec<(char, char)>,
    negated: bool,
}

impl Class {
    fn one(c: char) -> Class {
        Class {
            ranges: vec![(c, c)],
            negated: false,
        }
    }

    fn has(&self, c: char) -> bool {
        self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != self.negated
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Assert {
    BeginText,
    EndText,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Debug, Clone)]
enum Node {
    Class(Class),
    Assert(Assert),
    Concat(Vec<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Quest(Box<Node>),
}

#[derive(Debug, Clone)]
enum Inst {
    Rune(Class),
    Assert(Assert),
    Split(usize, usize),
    Jmp(usize),
    Match,
}

/// A compiled expression, matched by simulating its automaton over the name's runes.
#[derive(Debug, Clone)]
pub(crate) struct Prog {
    insts: Vec<Inst>,
}

struct Parser {
    s: Vec<char>,
    at: usize,
}

const PERL_D: &[(char, char)] = &[('0', '9')];
const PERL_S: &[(char, char)] = &[('\t', '\n'), ('\x0c', '\r'), (' ', ' ')];
const PERL_W: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];

fn posix_class(name: &str) -> Option<Vec<(char, char)>> {
    let r: &[(char, char)] = match name {
        "alnum" => &[('0', '9'), ('A', 'Z'), ('a', 'z')],
        "alpha" => &[('A', 'Z'), ('a', 'z')],
        "ascii" => &[('\0', '\x7f')],
        "blank" => &[('\t', '\t'), (' ', ' ')],
        "cntrl" => &[('\0', '\x1f'), ('\x7f', '\x7f')],
        "digit" => &[('0', '9')],
        "graph" => &[('!', '~')],
        "lower" => &[('a', 'z')],
        "print" => &[(' ', '~')],
        "punct" => &[('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
        "space" => &[('\t', '\r'), (' ', ' ')],
        "upper" => &[('A', 'Z')],
        "word" => PERL_W,
        "xdigit" => &[('0', '9'), ('A', 'F'), ('a', 'f')],
        _ => return None,
    };
    Some(r.to_vec())
}

/// The complement of sorted, non-overlapping ranges.
fn negate(ranges: &[(char, char)]) -> Vec<(char, char)> {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    let mut out = Vec::new();
    let mut next = 0u32;
    for (lo, hi) in sorted {
        if u32::from(lo) > next
            && let (Some(a), Some(b)) = (char::from_u32(next), char::from_u32(u32::from(lo) - 1))
        {
            out.push((a, b));
        }
        next = next.max(u32::from(hi) + 1);
    }
    if let Some(a) = char::from_u32(next) {
        out.push((a, char::MAX));
    }
    out
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        self.at += 1;
        c
    }

    fn concat(&mut self, depth: usize) -> Result<Node, ()> {
        let mut items: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            let atom = match c {
                ')' => {
                    if depth == 0 {
                        return Err(());
                    }
                    break;
                }
                '|' => return Err(()),
                '(' => {
                    self.next();
                    if self.peek() == Some('?') {
                        return Err(());
                    }
                    let inner = self.concat(depth + 1)?;
                    if self.next() != Some(')') {
                        return Err(());
                    }
                    inner
                }
                '[' => {
                    self.next();
                    Node::Class(self.class()?)
                }
                '.' => {
                    self.next();
                    Node::Class(Class {
                        ranges: vec![('\n', '\n')],
                        negated: true,
                    })
                }
                '^' => {
                    self.next();
                    Node::Assert(Assert::BeginText)
                }
                '$' => {
                    self.next();
                    Node::Assert(Assert::EndText)
                }
                '*' | '+' | '?' => return Err(()),
                '\\' => {
                    self.next();
                    self.escape(false)?
                }
                _ => {
                    self.next();
                    Node::Class(Class::one(c))
                }
            };
            let mut atom = atom;
            let mut repeated = false;
            while let Some(q) = self.peek().filter(|q| matches!(q, '*' | '+' | '?')) {
                if repeated {
                    return Err(());
                }
                self.next();
                if self.peek() == Some('?') {
                    self.next();
                }
                atom = match q {
                    '*' => Node::Star(Box::new(atom)),
                    '+' => Node::Plus(Box::new(atom)),
                    _ => Node::Quest(Box::new(atom)),
                };
                repeated = true;
            }
            items.push(atom);
        }
        Ok(Node::Concat(items))
    }

    /// An escape after `\`: an assertion, a Perl class, or a literal.
    fn escape(&mut self, in_class: bool) -> Result<Node, ()> {
        let c = self.next().ok_or(())?;
        let class = |ranges: &[(char, char)], negated| {
            Ok(Node::Class(Class {
                ranges: ranges.to_vec(),
                negated,
            }))
        };
        match c {
            'A' if !in_class => Ok(Node::Assert(Assert::BeginText)),
            'z' if !in_class => Ok(Node::Assert(Assert::EndText)),
            'b' if !in_class => Ok(Node::Assert(Assert::WordBoundary)),
            'B' if !in_class => Ok(Node::Assert(Assert::NotWordBoundary)),
            'd' => class(PERL_D, false),
            'D' => class(PERL_D, true),
            's' => class(PERL_S, false),
            'S' => class(PERL_S, true),
            'w' => class(PERL_W, false),
            'W' => class(PERL_W, true),
            'a' => Ok(Node::Class(Class::one('\x07'))),
            'f' => Ok(Node::Class(Class::one('\x0c'))),
            't' => Ok(Node::Class(Class::one('\t'))),
            'n' => Ok(Node::Class(Class::one('\n'))),
            'r' => Ok(Node::Class(Class::one('\r'))),
            'v' => Ok(Node::Class(Class::one('\x0b'))),
            '0'..='7' => {
                if c != '0' && !self.peek().is_some_and(|d| d.is_digit(8)) {
                    return Err(());
                }
                let mut v = c.to_digit(8).ok_or(())?;
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            self.next();
                        }
                        None => break,
                    }
                }
                Ok(Node::Class(Class::one(char::from_u32(v).ok_or(())?)))
            }
            'x' => {
                let mut v = 0u32;
                if self.peek() == Some('{') {
                    self.next();
                    let mut digits = 0;
                    loop {
                        let d = self.next().ok_or(())?;
                        if d == '}' && digits > 0 {
                            break;
                        }
                        v = v.checked_mul(16).ok_or(())? + d.to_digit(16).ok_or(())?;
                        digits += 1;
                    }
                } else {
                    for _ in 0..2 {
                        v = v * 16 + self.next().and_then(|d| d.to_digit(16)).ok_or(())?;
                    }
                }
                Ok(Node::Class(Class::one(char::from_u32(v).ok_or(())?)))
            }
            c if c.is_ascii_punctuation() => Ok(Node::Class(Class::one(c))),
            _ => Err(()),
        }
    }

    /// A class after `[`.
    fn class(&mut self) -> Result<Class, ()> {
        let mut negated = false;
        if self.peek() == Some('^') {
            self.next();
            negated = true;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let c = self.peek().ok_or(())?;
            if c == ']' && !first {
                self.next();
                break;
            }
            first = false;
            if c == '[' && self.s.get(self.at + 1) == Some(&':') {
                let rest: String = self.s.get(self.at + 2..).unwrap_or_default().iter().collect();
                if let Some(end) = rest.find(":]") {
                    let name = rest.get(..end).unwrap_or_default();
                    let (neg, name) = match name.strip_prefix('^') {
                        Some(n) => (true, n),
                        None => (false, name),
                    };
                    let r = posix_class(name).ok_or(())?;
                    ranges.extend(if neg { negate(&r) } else { r });
                    self.at += 2 + name.chars().count() + usize::from(neg) + 2;
                    continue;
                }
            }
            let lo = self.class_char(&mut ranges)?;
            let Some(lo) = lo else {
                continue;
            };
            if self.peek() == Some('-') && self.s.get(self.at + 1).is_some_and(|&n| n != ']') {
                self.next();
                let hi = self.class_char(&mut ranges)?.ok_or(())?;
                if hi < lo {
                    return Err(());
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
        Ok(Class { ranges, negated })
    }

    /// A class member: a rune, or a Perl class added to `ranges` (None).
    fn class_char(&mut self, ranges: &mut Vec<(char, char)>) -> Result<Option<char>, ()> {
        let c = self.next().ok_or(())?;
        if c != '\\' {
            return Ok(Some(c));
        }
        match self.escape(true)? {
            Node::Class(cl) if cl.ranges.len() == 1 && !cl.negated => {
                Ok(cl.ranges.first().map(|&(lo, _)| lo))
            }
            Node::Class(cl) => {
                ranges.extend(if cl.negated { negate(&cl.ranges) } else { cl.ranges });
                Ok(None)
            }
            _ => Err(()),
        }
    }
}

impl Prog {
    fn compile(re: &str) -> Result<Prog, ()> {
        let mut p = Parser {
            s: re.chars().collect(),
            at: 0,
        };
        let node = p.concat(0)?;
        if p.at < p.s.len() {
            return Err(());
        }
        let mut insts = Vec::new();
        emit(&node, &mut insts);
        insts.push(Inst::Match);
        Ok(Prog { insts })
    }

    /// Whether the expression matches `s` anywhere, as regexp.MatchString asks;
    /// patternmatcher's begin with `^` and end with `$`.
    fn matches(&self, s: &[u8]) -> bool {
        let text = runes(s);
        let mut clist: Vec<usize> = Vec::new();
        let mut seen = vec![usize::MAX; self.insts.len()];
        for pos in 0..=text.len() {
            // Unanchored: a thread may start at every position.
            self.add(&mut clist, &mut seen, 0, pos, &text);
            if clist
                .iter()
                .any(|&pc| matches!(self.insts.get(pc), Some(Inst::Match)))
            {
                return true;
            }
            let Some(&c) = text.get(pos) else {
                break;
            };
            let mut nlist = Vec::new();
            for &pc in &clist {
                if let Some(Inst::Rune(class)) = self.insts.get(pc)
                    && class.has(c)
                {
                    self.add(&mut nlist, &mut seen, pc + 1, pos + 1, &text);
                }
            }
            clist = nlist;
        }
        false
    }

    /// Adds `pc` and what it reaches without consuming a rune.
    fn add(&self, list: &mut Vec<usize>, seen: &mut [usize], pc: usize, pos: usize, text: &[char]) {
        let mut stack = vec![pc];
        while let Some(pc) = stack.pop() {
            match seen.get_mut(pc) {
                Some(s) if *s == pos => continue,
                Some(s) => *s = pos,
                None => continue,
            }
            match self.insts.get(pc) {
                Some(Inst::Jmp(t)) => stack.push(*t),
                Some(Inst::Split(a, b)) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Some(Inst::Assert(a)) => {
                    if holds(*a, pos, text) {
                        stack.push(pc + 1);
                    }
                }
                Some(_) => list.push(pc),
                None => {}
            }
        }
    }
}

fn is_word(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
}

fn holds(a: Assert, pos: usize, text: &[char]) -> bool {
    let boundary = || is_word(pos.checked_sub(1).and_then(|p| text.get(p))) != is_word(text.get(pos));
    match a {
        Assert::BeginText => pos == 0,
        Assert::EndText => pos == text.len(),
        Assert::WordBoundary => boundary(),
        Assert::NotWordBoundary => !boundary(),
    }
}

fn emit(node: &Node, out: &mut Vec<Inst>) {
    match node {
        Node::Class(c) => out.push(Inst::Rune(c.clone())),
        Node::Assert(a) => out.push(Inst::Assert(*a)),
        Node::Concat(items) => {
            for n in items {
                emit(n, out);
            }
        }
        Node::Star(n) => {
            let split = out.len();
            out.push(Inst::Split(0, 0));
            emit(n, out);
            out.push(Inst::Jmp(split));
            let end = out.len();
            if let Some(s) = out.get_mut(split) {
                *s = Inst::Split(split + 1, end);
            }
        }
        Node::Plus(n) => {
            let start = out.len();
            emit(n, out);
            let end = out.len() + 1;
            out.push(Inst::Split(start, end));
        }
        Node::Quest(n) => {
            let split = out.len();
            out.push(Inst::Split(0, 0));
            emit(n, out);
            let end = out.len();
            if let Some(s) = out.get_mut(split) {
                *s = Inst::Split(split + 1, end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(patterns: &[&str]) -> PatternMatcher {
        let p: Vec<Vec<u8>> = patterns.iter().map(|p| p.as_bytes().to_vec()).collect();
        PatternMatcher::new(&p).unwrap()
    }

    /// The check of patternmatcher_test.go's TestMatches: with the parents' results, and
    /// without.
    fn check(patterns: &[&str], text: &str) -> (bool, bool) {
        let pm = pm(patterns);
        let parent = gopath::dir(Os::Unix, text.as_bytes());
        let mut info = MatchInfo::default();
        if parent != b"." {
            let dirs: Vec<&[u8]> = parent.split(|&c| c == b'/').collect();
            for n in 1..=dirs.len() {
                let prefix = dirs[..n].join(&b'/');
                info = pm.matches_using_parent_results(&prefix, &info).unwrap().1;
            }
        }
        let with = pm.matches_using_parent_results(text.as_bytes(), &info).unwrap().0;
        let without = pm
            .matches_using_parent_results(text.as_bytes(), &MatchInfo::default())
            .unwrap()
            .0;
        (with, without)
    }

    // moby/patternmatcher v0.6.1 patternmatcher_test.go, TestMatches' tables.
    #[test]
    fn matches_as_patternmatcher() {
        let single: &[(&str, &str, bool)] = &[
            ("**", "file", true),
            ("**", "file/", true),
            ("**/", "file", true),
            ("**/", "file/", true),
            ("**", "/", true),
            ("**/", "/", true),
            ("**", "dir/file", true),
            ("**/", "dir/file", true),
            ("**", "dir/file/", true),
            ("**/", "dir/file/", true),
            ("**/**", "dir/file", true),
            ("**/**", "dir/file/", true),
            ("dir/**", "dir/file", true),
            ("dir/**", "dir/file/", true),
            ("dir/**", "dir/dir2/file", true),
            ("dir/**", "dir/dir2/file/", true),
            ("**/dir", "dir", true),
            ("**/dir", "dir/file", true),
            ("**/dir2/*", "dir/dir2/file", true),
            ("**/dir2/*", "dir/dir2/file/", true),
            ("**/dir2/**", "dir/dir2/dir3/file", true),
            ("**/dir2/**", "dir/dir2/dir3/file/", true),
            ("**file", "file", true),
            ("**file", "dir/file", true),
            ("**/file", "dir/file", true),
            ("**file", "dir/dir/file", true),
            ("**/file", "dir/dir/file", true),
            ("**/file*", "dir/dir/file", true),
            ("**/file*", "dir/dir/file.txt", true),
            ("**/file*txt", "dir/dir/file.txt", true),
            ("**/file*.txt", "dir/dir/file.txt", true),
            ("**/file*.txt*", "dir/dir/file.txt", true),
            ("**/**/*.txt", "dir/dir/file.txt", true),
            ("**/**/*.txt2", "dir/dir/file.txt", false),
            ("**/*.txt", "file.txt", true),
            ("**/**/*.txt", "file.txt", true),
            ("a**/*.txt", "a/file.txt", true),
            ("a**/*.txt", "a/dir/file.txt", true),
            ("a**/*.txt", "a/dir/dir/file.txt", true),
            ("a/*.txt", "a/dir/file.txt", false),
            ("a/*.txt", "a/file.txt", true),
            ("a/*.txt**", "a/file.txt", true),
            ("a[b-d]e", "ae", false),
            ("a[b-d]e", "ace", true),
            ("a[b-d]e", "aae", false),
            ("a[^b-d]e", "aze", true),
            (".*", ".foo", true),
            (".*", "foo", false),
            ("abc.def", "abcdef", false),
            ("abc.def", "abc.def", true),
            ("abc.def", "abcZdef", false),
            ("abc?def", "abcZdef", true),
            ("abc?def", "abcdef", false),
            ("a\\\\", "a\\", true),
            ("**/foo/bar", "foo/bar", true),
            ("**/foo/bar", "dir/foo/bar", true),
            ("**/foo/bar", "dir/dir2/foo/bar", true),
            ("abc/**", "abc", false),
            ("abc/**", "abc/def", true),
            ("abc/**", "abc/def/ghi", true),
            ("**/.foo", ".foo", true),
            ("**/.foo", "bar.foo", false),
            ("a(b)c/def", "a(b)c/def", true),
            ("a(b)c/def", "a(b)c/xyz", false),
            ("a.|)$(}+{bc", "a.|)$(}+{bc", true),
            (
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                true,
            ),
            (
                "dist/*.whl",
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                true,
            ),
        ];
        for &(pattern, text, pass) in single {
            assert_eq!(check(&[pattern], text), (pass, pass), "{pattern} {text}");
        }
        let multi: &[(&[&str], &str, bool)] = &[
            (&["**", "!util/docker/web"], "util/docker/web/foo", false),
            (
                &["**", "!util/docker/web", "util/docker/web/foo"],
                "util/docker/web/foo",
                true,
            ),
            (
                &["**", "!dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl"],
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                false,
            ),
            (
                &["**", "!dist/*.whl"],
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                false,
            ),
        ];
        for &(patterns, text, pass) in multi {
            assert_eq!(check(patterns, text), (pass, pass), "{patterns:?} {text}");
        }
        assert!(PatternMatcher::new(&[b"[".to_vec()]).is_err());
        assert!(PatternMatcher::new(&[b"!".to_vec()]).is_err());
        // TestMatchesOrParentMatchesMalformedPatternDoesNotPanicOnRepeatedCall.
        let pm = pm(&["[Local-Only]/"]);
        assert!(
            pm.matches_using_parent_results(b"x", &MatchInfo::default())
                .is_err()
        );
        assert!(
            pm.matches_using_parent_results(b"x", &MatchInfo::default())
                .is_err()
        );
    }
}
