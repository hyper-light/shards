//! Matching paths as BuildKit matches them, on Linux:
//! - `filepath_match`: Go 1.26's `path/filepath.Match`, which COPY's wildcards use;
//! - `PatternMatcher`: moby/patternmatcher v0.6.1, which `.dockerignore`, `COPY
//!   --exclude` and `--parents` use. It turns each pattern into a Go regular expression,
//!   and `Regexp` reads and matches that text as Go's `regexp` does: the same syntax,
//!   errors and leftmost semantics for what the patterns can produce, rune by rune, in
//!   time linear in the path.
//!
//! One deliberate difference: a Unicode property class (`\p{Greek}`, `\pL`) is refused,
//! where Go would match it from Unicode's tables.

use crate::go;
use crate::lex::{EOF, Scanner};

/// `filepath.ErrBadPattern`'s text.
pub const BAD_PATTERN: &str = "syntax error in pattern";

/// `filepath.ErrBadPattern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPattern;

/// `filepath.Match`, with `/` as the separator.
pub fn filepath_match(mut pattern: &[u8], mut name: &[u8]) -> Result<bool, BadPattern> {
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            return Ok(!name.contains(&b'/'));
        }
        let (t, ok, err) = match_chunk(chunk, name);
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if err {
            return Err(BadPattern);
        }
        if star {
            let mut i = 0;
            while i < name.len() && name.get(i) != Some(&b'/') {
                let (t, ok, err) = match_chunk(chunk, go::tail(name, i + 1));
                if ok {
                    if pattern.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
                if err {
                    return Err(BadPattern);
                }
                i += 1;
            }
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while pattern.first() == Some(&b'*') {
        pattern = go::tail(pattern, 1);
        star = true;
    }
    let mut inrange = false;
    let mut i = 0;
    while let Some(&c) = pattern.get(i) {
        match c {
            b'\\' => {
                if i + 1 < pattern.len() {
                    i += 1;
                }
            }
            b'[' => inrange = true,
            b']' => inrange = false,
            b'*' if !inrange => return (star, go::head(pattern, i), go::tail(pattern, i)),
            _ => {}
        }
        i += 1;
    }
    (star, pattern, b"")
}

/// `matchChunk`: what of `s` is left, whether it matched, whether the pattern is bad.
fn match_chunk<'a>(mut chunk: &[u8], mut s: &'a [u8]) -> (&'a [u8], bool, bool) {
    let mut failed = false;
    while let Some(&c) = chunk.first() {
        failed = failed || s.is_empty();
        match c {
            b'[' => {
                let mut r: u32 = 0;
                if !failed {
                    let (rr, n) = go::decode(s);
                    r = rr;
                    s = go::tail(s, n);
                }
                chunk = go::tail(chunk, 1);
                let mut negated = false;
                if chunk.first() == Some(&b'^') {
                    negated = true;
                    chunk = go::tail(chunk, 1);
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if chunk.first() == Some(&b']') && nrange > 0 {
                        chunk = go::tail(chunk, 1);
                        break;
                    }
                    let Some((lo, rest)) = get_esc(chunk) else {
                        return (b"", false, true);
                    };
                    chunk = rest;
                    let mut hi = lo;
                    if chunk.first() == Some(&b'-') {
                        let Some((h, rest)) = get_esc(go::tail(chunk, 1)) else {
                            return (b"", false, true);
                        };
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
                    failed = s.first() == Some(&b'/');
                    let (_, n) = go::decode(s);
                    s = go::tail(s, n);
                }
                chunk = go::tail(chunk, 1);
            }
            _ => {
                if c == b'\\' {
                    chunk = go::tail(chunk, 1);
                    if chunk.is_empty() {
                        return (b"", false, true);
                    }
                }
                if !failed {
                    failed = chunk.first() != s.first();
                    s = go::tail(s, 1);
                }
                chunk = go::tail(chunk, 1);
            }
        }
    }
    if failed {
        (b"", false, false)
    } else {
        (s, true, false)
    }
}

/// `getEsc`: a class's character, escaped or not; `None` for a bad pattern.
fn get_esc(mut chunk: &[u8]) -> Option<(u32, &[u8])> {
    match chunk.first() {
        None | Some(b'-' | b']') => return None,
        Some(b'\\') => {
            chunk = go::tail(chunk, 1);
            if chunk.is_empty() {
                return None;
            }
        }
        _ => {}
    }
    let (r, n) = go::decode(chunk);
    if r == go::RUNE_ERROR && n == 1 {
        return None;
    }
    let rest = go::tail(chunk, n);
    if rest.is_empty() {
        return None;
    }
    Some((r, rest))
}

/// A set of runes: ranges, sorted or not, and whether it is their complement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    ranges: Vec<(u32, u32)>,
    negated: bool,
}

impl Class {
    fn of(ranges: &[(u32, u32)], negated: bool) -> Class {
        Class {
            ranges: ranges.to_vec(),
            negated,
        }
    }

    fn contains(&self, r: u32) -> bool {
        self.ranges.iter().any(|&(lo, hi)| lo <= r && r <= hi) != self.negated
    }

    /// The runes as plain ranges, the complement taken.
    fn flatten(&self) -> Vec<(u32, u32)> {
        let mut rs = self.ranges.clone();
        rs.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (lo, hi) in rs {
            match merged.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        if !self.negated {
            return merged;
        }
        let mut out = Vec::new();
        let mut next = 0u32;
        for (lo, hi) in merged {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi.saturating_add(1);
        }
        if next <= 0x10FFFF {
            out.push((next, 0x10FFFF));
        }
        out
    }
}

const DIGIT: &[(u32, u32)] = &[(0x30, 0x39)];
const SPACE: &[(u32, u32)] = &[(0x09, 0x0a), (0x0c, 0x0d), (0x20, 0x20)];
const WORD: &[(u32, u32)] = &[(0x30, 0x39), (0x41, 0x5a), (0x5f, 0x5f), (0x61, 0x7a)];

/// `perlGroup`: `\d`, `\s`, `\w` and their complements, ASCII.
fn perl_class(c: u8) -> Option<Class> {
    Some(match c {
        b'd' => Class::of(DIGIT, false),
        b'D' => Class::of(DIGIT, true),
        b's' => Class::of(SPACE, false),
        b'S' => Class::of(SPACE, true),
        b'w' => Class::of(WORD, false),
        b'W' => Class::of(WORD, true),
        _ => return None,
    })
}

/// `posixGroup`: `[:alpha:]` and the rest, `[:^alpha:]` their complements.
fn posix_class(name: &[u8]) -> Option<Class> {
    let (negated, name) = match name.strip_prefix(b"^") {
        Some(n) => (true, n),
        None => (false, name),
    };
    let ranges: &[(u32, u32)] = match name {
        b"alnum" => &[(0x30, 0x39), (0x41, 0x5a), (0x61, 0x7a)],
        b"alpha" => &[(0x41, 0x5a), (0x61, 0x7a)],
        b"ascii" => &[(0x00, 0x7f)],
        b"blank" => &[(0x09, 0x09), (0x20, 0x20)],
        b"cntrl" => &[(0x00, 0x1f), (0x7f, 0x7f)],
        b"digit" => DIGIT,
        b"graph" => &[(0x21, 0x7e)],
        b"lower" => &[(0x61, 0x7a)],
        b"print" => &[(0x20, 0x7e)],
        b"punct" => &[(0x21, 0x2f), (0x3a, 0x40), (0x5b, 0x60), (0x7b, 0x7e)],
        b"space" => &[(0x09, 0x0d), (0x20, 0x20)],
        b"upper" => &[(0x41, 0x5a)],
        b"word" => WORD,
        b"xdigit" => &[(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)],
        _ => return None,
    };
    Some(Class::of(ranges, negated))
}

/// A parsed regular expression.
#[derive(Debug, Clone)]
enum Node {
    Rune(u32),
    Set(Class),
    /// `.`: any rune but a newline.
    Any,
    BeginText,
    EndText,
    WordBoundary(bool),
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Quest(Box<Node>),
    /// The empty expression.
    Empty,
}

/// Why a pattern does not compile; Go's message is not shown, only that it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bad {
    /// Go refuses it.
    Syntax,
    /// Go matches it from Unicode's tables, which shards does not carry.
    Unicode,
}

/// The next rune of `t`: `nextRune`, which refuses invalid UTF-8.
fn next_rune(t: &[u8]) -> Result<(u32, &[u8]), Bad> {
    let (r, n) = go::decode(t);
    if r == go::RUNE_ERROR && n == 1 {
        return Err(Bad::Syntax);
    }
    Ok((r, go::tail(t, n)))
}

fn unhex(c: u32) -> Option<u32> {
    char::from_u32(c)?.to_digit(16)
}

/// `parseEscape`: `\` and what it escapes, as one rune.
fn parse_escape(s: &[u8]) -> Result<(u32, &[u8]), Bad> {
    let t = go::tail(s, 1);
    if t.is_empty() {
        return Err(Bad::Syntax);
    }
    let (c, mut t) = next_rune(t)?;
    let octal = |b: Option<&u8>| b.is_some_and(|c| (b'0'..=b'7').contains(c));
    match char::from_u32(c).unwrap_or('\u{FFFD}') {
        '1'..='7' if !octal(t.first()) => {}
        '0'..='7' => {
            let mut r = c - u32::from(b'0');
            for _ in 1..3 {
                if !octal(t.first()) {
                    break;
                }
                r = r * 8 + u32::from(t.first().copied().unwrap_or(b'0') - b'0');
                t = go::tail(t, 1);
            }
            return Ok((r, t));
        }
        'x' => {
            if t.is_empty() {
                return Err(Bad::Syntax);
            }
            let (c, rest) = next_rune(t)?;
            t = rest;
            if c == u32::from('{') {
                let mut n = 0;
                let mut r: u32 = 0;
                loop {
                    if t.is_empty() {
                        return Err(Bad::Syntax);
                    }
                    let (c, rest) = next_rune(t)?;
                    t = rest;
                    if c == u32::from('}') {
                        break;
                    }
                    let v = unhex(c).ok_or(Bad::Syntax)?;
                    r = r * 16 + v;
                    if r > 0x10FFFF {
                        return Err(Bad::Syntax);
                    }
                    n += 1;
                }
                if n == 0 {
                    return Err(Bad::Syntax);
                }
                return Ok((r, t));
            }
            let x = unhex(c);
            let (c2, rest) = next_rune(t)?;
            t = rest;
            return match (x, unhex(c2)) {
                (Some(x), Some(y)) => Ok((x * 16 + y, t)),
                _ => Err(Bad::Syntax),
            };
        }
        'a' => return Ok((0x07, t)),
        'f' => return Ok((0x0c, t)),
        'n' => return Ok((0x0a, t)),
        'r' => return Ok((0x0d, t)),
        't' => return Ok((0x09, t)),
        'v' => return Ok((0x0b, t)),
        ch if (ch as u32) < 0x80 && !ch.is_ascii_alphanumeric() => return Ok((c, t)),
        _ => {}
    }
    Err(Bad::Syntax)
}

/// `parseClassChar`.
fn class_char(t: &[u8]) -> Result<(u32, &[u8]), Bad> {
    if t.is_empty() {
        return Err(Bad::Syntax);
    }
    if t.first() == Some(&b'\\') {
        parse_escape(t)
    } else {
        next_rune(t)
    }
}

/// `parseClass`: `[...]`, Perl flags on, so a negated class still holds the newline.
fn parse_class(s: &[u8]) -> Result<(Class, &[u8]), Bad> {
    let mut t = go::tail(s, 1);
    let mut negated = false;
    if t.first() == Some(&b'^') {
        negated = true;
        t = go::tail(t, 1);
    }
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut first = true;
    while t.is_empty() || t.first() != Some(&b']') || first {
        first = false;
        if t.is_empty() {
            return Err(Bad::Syntax);
        }
        if t.len() > 2
            && t.starts_with(b"[:")
            && let Some(i) = go::tail(t, 2).windows(2).position(|w| w == b":]")
        {
            let name = go::span(t, 2, 2 + i);
            let class = posix_class(name).ok_or(Bad::Syntax)?;
            ranges.extend(class.flatten());
            t = go::tail(t, 2 + i + 2);
            continue;
        }
        if t.len() >= 2 && t.first() == Some(&b'\\') && matches!(t.get(1), Some(b'p' | b'P')) {
            return Err(Bad::Unicode);
        }
        if t.len() >= 2
            && t.first() == Some(&b'\\')
            && let Some(class) = t.get(1).and_then(|&c| perl_class(c))
        {
            ranges.extend(class.flatten());
            t = go::tail(t, 2);
            continue;
        }
        let (lo, rest) = class_char(t)?;
        t = rest;
        let mut hi = lo;
        if t.len() >= 2 && t.first() == Some(&b'-') && t.get(1) != Some(&b']') {
            let (h, rest) = class_char(go::tail(t, 1))?;
            if h < lo {
                return Err(Bad::Syntax);
            }
            hi = h;
            t = rest;
        }
        ranges.push((lo, hi));
    }
    Ok((Class { ranges, negated }, go::tail(t, 1)))
}

/// What the parse holds on its stack: an expression, or a group's opening.
enum Item {
    Node(Node),
    LeftParen,
    Bar,
}

/// A Go regular expression, as `regexp.Compile` reads it, for what patternmatcher writes.
#[derive(Debug, Clone)]
pub struct Regexp {
    prog: Vec<Inst>,
    start: usize,
}

impl Regexp {
    pub fn compile(s: &[u8]) -> Result<Regexp, Bad> {
        let node = parse(s)?;
        let mut prog = Vec::new();
        let start = emit(&node, &mut prog, usize::MAX);
        // The match follows the expression.
        prog.push(Inst::Match);
        let last = prog.len() - 1;
        for inst in &mut prog {
            inst.patch(usize::MAX, last);
        }
        Ok(Regexp { prog, start })
    }

    /// `MatchString`: whether the expression matches anywhere in `text`.
    pub fn is_match(&self, text: &[u8]) -> bool {
        let runes: Vec<(u32, usize)> = go::runes(text).collect();
        let n = runes.len();
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        let mut on = vec![false; self.prog.len()];
        for pos in 0..=n {
            // Unanchored: a thread starts at every position.
            self.add(&mut current, &mut on, self.start, pos, &runes);
            if current
                .iter()
                .any(|&pc| matches!(self.prog.get(pc), Some(Inst::Match)))
            {
                return true;
            }
            let Some(&(r, _)) = runes.get(pos) else {
                break;
            };
            on.iter_mut().for_each(|o| *o = false);
            for &pc in &current {
                let step = match self.prog.get(pc) {
                    Some(Inst::Rune(c, to)) if *c == r => Some(*to),
                    Some(Inst::Set(class, to)) if class.contains(r) => Some(*to),
                    Some(Inst::Any(to)) if r != 0x0a => Some(*to),
                    _ => None,
                };
                if let Some(to) = step {
                    self.add(&mut next, &mut on, to, pos + 1, &runes);
                }
            }
            std::mem::swap(&mut current, &mut next);
            next.clear();
        }
        current
            .iter()
            .any(|&pc| matches!(self.prog.get(pc), Some(Inst::Match)))
    }

    /// Adds `pc`'s thread at position `pos`, following empty steps.
    fn add(&self, list: &mut Vec<usize>, on: &mut [bool], pc: usize, pos: usize, runes: &[(u32, usize)]) {
        let mut stack = vec![pc];
        while let Some(pc) = stack.pop() {
            match on.get_mut(pc) {
                Some(seen) if !*seen => *seen = true,
                _ => continue,
            }
            let word = |i: usize| {
                runes
                    .get(i)
                    .is_some_and(|&(r, _)| WORD.iter().any(|&(lo, hi)| lo <= r && r <= hi))
            };
            match self.prog.get(pc) {
                Some(Inst::Split(a, b)) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Some(Inst::Jmp(to)) => stack.push(*to),
                Some(Inst::Begin(to)) => {
                    if pos == 0 {
                        stack.push(*to);
                    }
                }
                Some(Inst::End(to)) => {
                    if pos == runes.len() {
                        stack.push(*to);
                    }
                }
                Some(Inst::Boundary(want, to)) => {
                    let at = pos > 0 && word(pos - 1);
                    let is = at != word(pos);
                    if is == *want {
                        stack.push(*to);
                    }
                }
                _ => list.push(pc),
            }
        }
    }
}

/// An instruction: consume a rune, branch, assert, or match; targets are instruction
/// indices, `usize::MAX` meaning "what follows".
#[derive(Debug, Clone)]
enum Inst {
    Rune(u32, usize),
    Set(Class, usize),
    Any(usize),
    Split(usize, usize),
    Jmp(usize),
    Begin(usize),
    End(usize),
    Boundary(bool, usize),
    Match,
}

impl Inst {
    fn patch(&mut self, from: usize, to: usize) {
        let fix = |t: &mut usize| {
            if *t == from {
                *t = to;
            }
        };
        match self {
            Inst::Rune(_, t)
            | Inst::Set(_, t)
            | Inst::Any(t)
            | Inst::Jmp(t)
            | Inst::Begin(t)
            | Inst::End(t)
            | Inst::Boundary(_, t) => fix(t),
            Inst::Split(a, b) => {
                fix(a);
                fix(b);
            }
            Inst::Match => {}
        }
    }
}

/// Emits `node` so that it continues at `next`; returns its first instruction.
fn emit(node: &Node, prog: &mut Vec<Inst>, next: usize) -> usize {
    // Instructions are appended in order; each node's code ends by going to `next`.
    match node {
        Node::Empty => {
            prog.push(Inst::Jmp(next));
            prog.len() - 1
        }
        Node::Rune(c) => {
            prog.push(Inst::Rune(*c, next));
            prog.len() - 1
        }
        Node::Set(class) => {
            prog.push(Inst::Set(class.clone(), next));
            prog.len() - 1
        }
        Node::Any => {
            prog.push(Inst::Any(next));
            prog.len() - 1
        }
        Node::BeginText => {
            prog.push(Inst::Begin(next));
            prog.len() - 1
        }
        Node::EndText => {
            prog.push(Inst::End(next));
            prog.len() - 1
        }
        Node::WordBoundary(want) => {
            prog.push(Inst::Boundary(*want, next));
            prog.len() - 1
        }
        Node::Concat(items) => {
            // Each item continues to the next item's start: emit placeholders, then fix.
            let mut starts = Vec::with_capacity(items.len());
            let mut ends = Vec::with_capacity(items.len());
            for item in items {
                let from = prog.len();
                let start = emit(item, prog, usize::MAX - 1);
                starts.push(start);
                ends.push((from, prog.len()));
            }
            for (i, &(from, to)) in ends.iter().enumerate() {
                let target = starts.get(i + 1).copied().unwrap_or(next);
                for inst in prog.get_mut(from..to).into_iter().flatten() {
                    inst.patch(usize::MAX - 1, target);
                }
            }
            match starts.first() {
                Some(&s) => s,
                None => {
                    prog.push(Inst::Jmp(next));
                    prog.len() - 1
                }
            }
        }
        Node::Alternate(items) => {
            // Split chains: leftmost first.
            let mut entry = None;
            let mut prev_split: Option<usize> = None;
            for (i, item) in items.iter().enumerate() {
                let start = if i + 1 < items.len() {
                    prog.push(Inst::Split(usize::MAX - 2, usize::MAX - 3));
                    let split = prog.len() - 1;
                    let body = emit(item, prog, next);
                    if let Some(Inst::Split(a, _)) = prog.get_mut(split) {
                        *a = body;
                    }
                    split
                } else {
                    emit(item, prog, next)
                };
                if let Some(p) = prev_split
                    && let Some(Inst::Split(_, b)) = prog.get_mut(p)
                {
                    *b = start;
                }
                if i + 1 < items.len() {
                    prev_split = Some(start);
                }
                entry.get_or_insert(start);
            }
            entry.unwrap_or_else(|| {
                prog.push(Inst::Jmp(next));
                prog.len() - 1
            })
        }
        Node::Star(inner) => {
            // L: split(body, next); body -> L
            prog.push(Inst::Split(usize::MAX - 2, next));
            let split = prog.len() - 1;
            let from = prog.len();
            let body = emit(inner, prog, usize::MAX - 1);
            for inst in prog.get_mut(from..).into_iter().flatten() {
                inst.patch(usize::MAX - 1, split);
            }
            if let Some(Inst::Split(a, _)) = prog.get_mut(split) {
                *a = body;
            }
            split
        }
        Node::Plus(inner) => {
            // body -> split(body, next)
            let from = prog.len();
            let body = emit(inner, prog, usize::MAX - 1);
            prog.push(Inst::Split(body, next));
            let split = prog.len() - 1;
            for inst in prog.get_mut(from..split).into_iter().flatten() {
                inst.patch(usize::MAX - 1, split);
            }
            body
        }
        Node::Quest(inner) => {
            prog.push(Inst::Split(usize::MAX - 2, next));
            let split = prog.len() - 1;
            let body = emit(inner, prog, next);
            if let Some(Inst::Split(a, _)) = prog.get_mut(split) {
                *a = body;
            }
            split
        }
    }
}

/// `parse` with Perl flags (`regexp.Compile`'s), for the syntax patterns can produce.
fn parse(s: &[u8]) -> Result<Node, Bad> {
    let mut stack: Vec<Item> = Vec::new();
    let mut t = s;
    let mut last_repeat = false;
    while let Some(&c) = t.first() {
        let mut repeat = false;
        match c {
            b'(' => {
                if t.get(1) == Some(&b'?') {
                    // Flags and named groups: none of patternmatcher's text has them.
                    return Err(Bad::Syntax);
                }
                stack.push(Item::LeftParen);
                t = go::tail(t, 1);
            }
            b'|' => {
                collapse(&mut stack);
                stack.push(Item::Bar);
                t = go::tail(t, 1);
            }
            b')' => {
                collapse(&mut stack);
                let mut alts = Vec::new();
                loop {
                    match stack.pop() {
                        Some(Item::Node(n)) => alts.push(n),
                        Some(Item::Bar) => {}
                        Some(Item::LeftParen) => break,
                        None => return Err(Bad::Syntax),
                    }
                }
                alts.reverse();
                let node = if alts.len() == 1 {
                    alts.pop().unwrap_or(Node::Empty)
                } else {
                    Node::Alternate(alts)
                };
                stack.push(Item::Node(Node::Concat(vec![node])));
                t = go::tail(t, 1);
            }
            b'^' => {
                stack.push(Item::Node(Node::BeginText));
                t = go::tail(t, 1);
            }
            b'$' => {
                stack.push(Item::Node(Node::EndText));
                t = go::tail(t, 1);
            }
            b'.' => {
                stack.push(Item::Node(Node::Any));
                t = go::tail(t, 1);
            }
            b'[' => {
                let (class, rest) = parse_class(t)?;
                stack.push(Item::Node(Node::Set(class)));
                t = rest;
            }
            b'*' | b'+' | b'?' => {
                if last_repeat {
                    return Err(Bad::Syntax);
                }
                let Some(Item::Node(n)) = stack.pop() else {
                    return Err(Bad::Syntax);
                };
                let b = Box::new(n);
                stack.push(Item::Node(match c {
                    b'*' => Node::Star(b),
                    b'+' => Node::Plus(b),
                    _ => Node::Quest(b),
                }));
                repeat = true;
                t = go::tail(t, 1);
            }
            b'\\' => match t.get(1) {
                Some(b'A') => {
                    stack.push(Item::Node(Node::BeginText));
                    t = go::tail(t, 2);
                }
                Some(b'z') => {
                    stack.push(Item::Node(Node::EndText));
                    t = go::tail(t, 2);
                }
                Some(b'b') => {
                    stack.push(Item::Node(Node::WordBoundary(true)));
                    t = go::tail(t, 2);
                }
                Some(b'B') => {
                    stack.push(Item::Node(Node::WordBoundary(false)));
                    t = go::tail(t, 2);
                }
                Some(b'C') => return Err(Bad::Syntax),
                Some(b'Q') => {
                    let body = go::tail(t, 2);
                    let (lit, rest) = match body.windows(2).position(|w| w == b"\\E") {
                        Some(i) => (go::head(body, i), go::tail(body, i + 2)),
                        None => (body, &b""[..]),
                    };
                    let mut lit = lit;
                    while !lit.is_empty() {
                        let (r, rest) = next_rune(lit)?;
                        stack.push(Item::Node(Node::Rune(r)));
                        lit = rest;
                    }
                    t = rest;
                }
                Some(b'p' | b'P') => return Err(Bad::Unicode),
                Some(&p) if perl_class(p).is_some() => {
                    if let Some(class) = perl_class(p) {
                        stack.push(Item::Node(Node::Set(class)));
                    }
                    t = go::tail(t, 2);
                }
                _ => {
                    let (r, rest) = parse_escape(t)?;
                    stack.push(Item::Node(Node::Rune(r)));
                    t = rest;
                }
            },
            _ => {
                let (r, rest) = next_rune(t)?;
                stack.push(Item::Node(Node::Rune(r)));
                t = rest;
            }
        }
        last_repeat = repeat;
    }
    collapse(&mut stack);
    let mut alts = Vec::new();
    while let Some(item) = stack.pop() {
        match item {
            Item::Node(n) => alts.push(n),
            Item::Bar => {}
            Item::LeftParen => return Err(Bad::Syntax),
        }
    }
    alts.reverse();
    Ok(if alts.len() == 1 {
        alts.pop().unwrap_or(Node::Empty)
    } else {
        Node::Alternate(alts)
    })
}

/// Joins the expressions since the last group opening or bar into one concatenation.
fn collapse(stack: &mut Vec<Item>) {
    let mut items = Vec::new();
    while let Some(Item::Node(_)) = stack.last() {
        if let Some(Item::Node(n)) = stack.pop() {
            items.push(n);
        }
    }
    items.reverse();
    stack.push(Item::Node(Node::Concat(items)));
}

/// A patternmatcher pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    cleaned: Vec<u8>,
    dirs: Vec<Vec<u8>>,
    exclusion: bool,
    kind: Option<Kind>,
}

#[derive(Debug, Clone)]
enum Kind {
    Exact,
    Prefix,
    Suffix,
    Regexp(Option<Regexp>),
}

impl Pattern {
    pub fn text(&self) -> &[u8] {
        &self.cleaned
    }

    pub fn exclusion(&self) -> bool {
        self.exclusion
    }

    /// `compile`: the pattern's kind, and for a regexp its text.
    fn compile(&mut self) -> Result<(), Bad> {
        let mut reg: Vec<u8> = b"^".to_vec();
        let mut kind = 0u8; // 0 exact, 1 prefix, 2 suffix, 3 regexp
        let mut scan = Scanner::new(&self.cleaned);
        let mut i = 0;
        let sl = i64::from(b'/');
        while scan.peek() != EOF {
            let ch = scan.next();
            if ch == i64::from(b'*') {
                if scan.peek() == i64::from(b'*') {
                    scan.next();
                    if scan.peek() == sl {
                        scan.next();
                    }
                    if scan.peek() == EOF {
                        if kind == 0 {
                            kind = 1;
                        } else {
                            reg.extend_from_slice(b".*");
                            kind = 3;
                        }
                    } else {
                        reg.extend_from_slice(b"(.*/)?");
                        kind = 3;
                    }
                    if i == 0 {
                        kind = 2;
                    }
                } else {
                    reg.extend_from_slice(b"[^/]*");
                    kind = 3;
                }
            } else if ch == i64::from(b'?') {
                reg.extend_from_slice(b"[^/]");
                kind = 3;
            } else if matches!(
                u8::try_from(ch),
                Ok(b'.' | b'+' | b'(' | b')' | b'|' | b'{' | b'}' | b'$')
            ) {
                reg.push(b'\\');
                push_ch(&mut reg, ch);
            } else if ch == i64::from(b'\\') {
                if scan.peek() != EOF {
                    reg.push(b'\\');
                    push_ch(&mut reg, scan.next());
                    kind = 3;
                } else {
                    reg.push(b'\\');
                }
            } else if ch == i64::from(b'[') || ch == i64::from(b']') {
                push_ch(&mut reg, ch);
                kind = 3;
            } else {
                push_ch(&mut reg, ch);
            }
            i += 1;
        }
        self.kind = Some(match kind {
            0 => Kind::Exact,
            1 => Kind::Prefix,
            2 => Kind::Suffix,
            _ => {
                reg.push(b'$');
                Kind::Regexp(Some(Regexp::compile(&reg)?))
            }
        });
        Ok(())
    }

    /// `match`; `Err` when it does not compile.
    fn matches(&mut self, path: &[u8]) -> Result<bool, Bad> {
        if self.kind.is_none() {
            self.compile()?;
        }
        Ok(match &self.kind {
            Some(Kind::Exact) => path == self.cleaned.as_slice(),
            Some(Kind::Prefix) => {
                path.starts_with(go::head(&self.cleaned, self.cleaned.len().saturating_sub(2)))
            }
            Some(Kind::Suffix) => {
                let suffix = go::tail(&self.cleaned, 2);
                path.ends_with(suffix) || (suffix.first() == Some(&b'/') && path == go::tail(suffix, 1))
            }
            Some(Kind::Regexp(Some(re))) => re.is_match(path),
            _ => return Err(Bad::Syntax),
        })
    }
}

fn push_ch(out: &mut Vec<u8>, ch: i64) {
    go::push(out, u32::try_from(ch).unwrap_or(go::RUNE_ERROR));
}

/// moby/patternmatcher's `PatternMatcher`.
#[derive(Debug, Clone, Default)]
pub struct PatternMatcher {
    patterns: Vec<Pattern>,
    exclusions: bool,
}

/// `MatchInfo`: each pattern's result for the parent.
#[derive(Debug, Clone, Default)]
pub struct MatchInfo(Vec<bool>);

impl PatternMatcher {
    /// `New`: each pattern trimmed, cleaned and checked; `!` negates one.
    pub fn new(patterns: &[Vec<u8>]) -> Result<PatternMatcher, Vec<u8>> {
        let mut pm = PatternMatcher::default();
        for p in patterns {
            let p = go::trim_space(p);
            if p.is_empty() {
                continue;
            }
            let mut p = go::clean(p);
            let mut exclusion = false;
            if p.first() == Some(&b'!') {
                if p.len() == 1 {
                    return Err(b"illegal exclusion pattern: \"!\"".to_vec());
                }
                exclusion = true;
                p = go::tail(&p, 1).to_vec();
                pm.exclusions = true;
            }
            if filepath_match(&p, b".").is_err() {
                return Err(BAD_PATTERN.as_bytes().to_vec());
            }
            let dirs = p.split(|&b| b == b'/').map(<[u8]>::to_vec).collect();
            pm.patterns.push(Pattern {
                cleaned: p,
                dirs,
                exclusion,
                kind: None,
            });
        }
        Ok(pm)
    }

    pub fn exclusions(&self) -> bool {
        self.exclusions
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    fn bad(e: &Bad) -> Vec<u8> {
        match e {
            Bad::Syntax => BAD_PATTERN.as_bytes().to_vec(),
            Bad::Unicode => b"Unicode classes in patterns are not supported by shards".to_vec(),
        }
    }

    /// `Matches`: the file, or its parent's leading directories as deep as the pattern.
    pub fn matches(&mut self, file: &[u8]) -> Result<bool, Vec<u8>> {
        let mut matched = false;
        let parent = dir(file);
        let parent_dirs: Vec<&[u8]> = parent.split(|&b| b == b'/').collect();
        for pattern in &mut self.patterns {
            if pattern.exclusion != matched {
                continue;
            }
            let mut m = pattern.matches(file).map_err(|e| Self::bad(&e))?;
            if !m && parent != b"." && pattern.dirs.len() <= parent_dirs.len() {
                let joined = parent_dirs
                    .get(..pattern.dirs.len())
                    .unwrap_or_default()
                    .join(&b'/');
                m = pattern.matches(&joined).unwrap_or(false);
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// `MatchesOrParentMatches`.
    pub fn matches_or_parent_matches(&mut self, file: &[u8]) -> Result<bool, Vec<u8>> {
        let mut matched = false;
        let parent = dir(file);
        let parent_dirs: Vec<&[u8]> = parent.split(|&b| b == b'/').collect();
        for pattern in &mut self.patterns {
            if pattern.exclusion != matched {
                continue;
            }
            let mut m = pattern.matches(file).map_err(|e| Self::bad(&e))?;
            if !m && parent != b"." {
                for i in 0..parent_dirs.len() {
                    m = pattern
                        .matches(&parent_dirs.get(..=i).unwrap_or_default().join(&b'/'))
                        .unwrap_or(false);
                    if m {
                        break;
                    }
                }
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// `MatchesUsingParentResults`.
    pub fn matches_using_parent_results(
        &mut self,
        file: &[u8],
        parent: &MatchInfo,
    ) -> Result<(bool, MatchInfo), Vec<u8>> {
        if !parent.0.is_empty() && parent.0.len() != self.patterns.len() {
            return Err(b"wrong number of values in parentMatched".to_vec());
        }
        let mut matched = false;
        let mut info = MatchInfo(vec![false; self.patterns.len()]);
        for (i, pattern) in self.patterns.iter_mut().enumerate() {
            let mut m = parent.0.get(i).copied().unwrap_or(false);
            if !m {
                if pattern.exclusion != matched {
                    continue;
                }
                m = pattern.matches(file).map_err(|e| Self::bad(&e))?;
                if !m && parent.0.is_empty() {
                    let p = dir(file);
                    if p != b"." {
                        let dirs: Vec<&[u8]> = p.split(|&b| b == b'/').collect();
                        for j in 0..dirs.len() {
                            m = pattern
                                .matches(&dirs.get(..=j).unwrap_or_default().join(&b'/'))
                                .unwrap_or(false);
                            if m {
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(slot) = info.0.get_mut(i) {
                *slot = m;
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok((matched, info))
    }
}

/// `filepath.Dir`.
fn dir(p: &[u8]) -> Vec<u8> {
    match p.iter().rposition(|&b| b == b'/') {
        Some(i) => go::clean(go::head(p, i + 1)),
        None => b".".to_vec(),
    }
}
