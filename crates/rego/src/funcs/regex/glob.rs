//! gobwas/glob v0.2.3, as glob.match compiles and matches: its lexer and parser
//! (syntax/), its compiler, which turns the tree into the optimised matchers of
//! match/, and those matchers, quirks kept. They measure some lengths in runes and
//! use them as byte offsets, so they run on bytes and may split a rune, which then
//! reads as Go reads a broken one (U+FFFD, one byte). Where Go would panic (a slice
//! out of range), matching fails with Go's runtime error text instead.

/// What matching returns: Go's panic, if it would panic.
type R<T> = Result<T, String>;

const PANIC: &str = "runtime error: slice bounds out of range";

fn sl(s: &[u8], a: usize, b: usize) -> R<&[u8]> {
    s.get(a..b).ok_or_else(|| PANIC.to_string())
}

fn from(s: &[u8], a: usize) -> R<&[u8]> {
    s.get(a..).ok_or_else(|| PANIC.to_string())
}

const RUNE_ERROR: u32 = 0xFFFD;

/// utf8.DecodeRune: the rune at the start of b and its width; RuneError and width 1
/// for a broken one, RuneError and width 0 for none.
fn decode(b: &[u8]) -> (u32, usize) {
    let Some(&lead) = b.first() else {
        return (RUNE_ERROR, 0);
    };
    let w = match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return (RUNE_ERROR, 1),
    };
    match b
        .get(..w)
        .and_then(|p| std::str::from_utf8(p).ok())
        .and_then(|s| s.chars().next())
    {
        Some(c) => (u32::from(c), w),
        None => (RUNE_ERROR, 1),
    }
}

/// `for i, r := range s`.
fn runes(s: &[u8]) -> impl Iterator<Item = (usize, u32)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let rest = s.get(i..)?;
        if rest.is_empty() {
            return None;
        }
        let (r, w) = decode(rest);
        let at = i;
        i += w.max(1);
        Some((at, r))
    })
}

fn rune_count(s: &[u8]) -> usize {
    runes(s).count()
}

/// utf8.RuneLen.
fn rune_len(r: u32) -> usize {
    match r {
        0..=0x7F => 1,
        0x80..=0x7FF => 2,
        0x800..=0xFFFF => 3,
        _ => 4,
    }
}

fn encode(r: u32) -> Vec<u8> {
    char::from_u32(r).unwrap_or('\u{FFFD}').to_string().into_bytes()
}

/// strings.Index.
fn index(s: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    s.windows(needle.len()).position(|w| w == needle)
}

/// strings.LastIndex.
fn last_index(s: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(s.len());
    }
    s.windows(needle.len()).rposition(|w| w == needle)
}

/// strings.IndexRune.
fn index_rune(s: &[u8], r: u32) -> Option<usize> {
    if r < 0x80 {
        return s.iter().position(|&b| u32::from(b) == r);
    }
    if r == RUNE_ERROR {
        return runes(s).find(|&(_, c)| c == RUNE_ERROR).map(|(i, _)| i);
    }
    if char::from_u32(r).is_none() {
        return None;
    }
    index(s, &encode(r))
}

/// util/strings.IndexAnyRunes: the first separator, in their order, found anywhere.
fn index_any_runes(s: &[u8], rs: &[u32]) -> Option<usize> {
    rs.iter().find_map(|&r| index_rune(s, r))
}

/// util/strings.LastIndexAnyRunes, its loop for non-ASCII runes kept.
fn last_index_any_runes(s: &[u8], rs: &[u32]) -> R<Option<usize>> {
    for &r in rs {
        let mut i = None;
        if r < 0x80 {
            i = s.iter().rposition(|&b| u32::from(b) == r);
        } else {
            let mut sub = s;
            while !sub.is_empty() {
                let Some(j) = index_rune(s, r) else { break };
                i = Some(j);
                sub = from(sub, j + 1)?;
            }
        }
        if i.is_some() {
            return Ok(i);
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum M {
    Any(Vec<u32>),
    AnyOf(Vec<M>),
    BTree(Box<BTree>),
    Contains { needle: Vec<u8>, not: bool },
    EveryOf(Vec<M>),
    List { list: Vec<u32>, not: bool },
    Max(usize),
    Min(usize),
    Nothing,
    Prefix(Vec<u8>),
    PrefixAny(Vec<u8>, Vec<u32>),
    PrefixSuffix(Vec<u8>, Vec<u8>),
    Range { lo: u32, hi: u32, not: bool },
    Row { ms: Vec<M>, len: i64 },
    Single(Vec<u32>),
    Suffix(Vec<u8>),
    SuffixAny(Vec<u8>, Vec<u32>),
    Super,
    Text(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BTree {
    value: M,
    left: Option<M>,
    right: Option<M>,
    value_len: i64,
    left_len: i64,
    right_len: i64,
    len: i64,
}

fn new_btree(value: M, left: Option<M>, right: Option<M>) -> M {
    let mut ok = true;
    let value_len = value.len();
    if value_len == -1 {
        ok = false;
    }
    let left_len = left.as_ref().map_or(0, M::len);
    if left_len == -1 {
        ok = false;
    }
    let right_len = right.as_ref().map_or(0, M::len);
    if right_len == -1 {
        ok = false;
    }
    let len = if ok { left_len + value_len + right_len } else { -1 };
    M::BTree(Box::new(BTree {
        value,
        left,
        right,
        value_len,
        left_len,
        right_len,
        len,
    }))
}

fn text(s: Vec<u8>) -> M {
    M::Text(s)
}

/// appendMerge: two sorted lists merged, equal heads once.
fn append_merge(target: &[usize], sub: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(target.len() + sub.len());
    let (mut x, mut y) = (0, 0);
    loop {
        match (target.get(x), sub.get(y)) {
            (None, None) => break,
            (None, Some(_)) => {
                out.extend(sub.get(y..).unwrap_or(&[]));
                break;
            }
            (Some(_), None) => {
                out.extend(target.get(x..).unwrap_or(&[]));
                break;
            }
            (Some(&a), Some(&b)) => {
                if a == b {
                    out.push(a);
                    x += 1;
                    y += 1;
                } else if a < b {
                    out.push(a);
                    x += 1;
                } else {
                    out.push(b);
                    y += 1;
                }
            }
        }
    }
    out
}

type Index = Option<(usize, Vec<usize>)>;

impl M {
    fn len(&self) -> i64 {
        match self {
            M::AnyOf(ms) => {
                let mut l = -1;
                for m in ms {
                    let ml = m.len();
                    if l == -1 {
                        l = ml;
                    } else if ml == -1 || l != ml {
                        return -1;
                    }
                }
                l
            }
            M::BTree(t) => t.len,
            // Its loop returns -1 at its first matcher.
            M::EveryOf(ms) => {
                if ms.is_empty() {
                    0
                } else {
                    -1
                }
            }
            M::List { .. } | M::Range { .. } | M::Single(_) => 1,
            M::Nothing => 0,
            M::Row { len, .. } => *len,
            M::Text(s) => i64::try_from(rune_count(s)).unwrap_or(i64::MAX),
            _ => -1,
        }
    }

    fn matches(&self, s: &[u8]) -> R<bool> {
        Ok(match self {
            M::Any(seps) => index_any_runes(s, seps).is_none(),
            M::AnyOf(ms) => {
                for m in ms {
                    if m.matches(s)? {
                        return Ok(true);
                    }
                }
                false
            }
            M::BTree(t) => t.matches(s)?,
            M::Contains { needle, not } => index(s, needle).is_some() != *not,
            M::EveryOf(ms) => {
                for m in ms {
                    if !m.matches(s)? {
                        return Ok(false);
                    }
                }
                true
            }
            M::List { list, not } => {
                let (r, w) = decode(s);
                if s.len() > w {
                    return Ok(false);
                }
                list.contains(&r) == !*not
            }
            M::Max(limit) => rune_count(s) <= *limit,
            M::Min(limit) => {
                let mut l = 0;
                for _ in runes(s) {
                    l += 1;
                    if l >= *limit {
                        return Ok(true);
                    }
                }
                false
            }
            M::Nothing => s.is_empty(),
            M::Prefix(p) => s.starts_with(p),
            M::PrefixAny(p, seps) => s.starts_with(p) && index_any_runes(from(s, p.len())?, seps).is_none(),
            M::PrefixSuffix(p, x) => s.starts_with(p) && s.ends_with(x),
            M::Range { lo, hi, not } => {
                let (r, w) = decode(s);
                if s.len() > w {
                    return Ok(false);
                }
                (r >= *lo && r <= *hi) == !*not
            }
            M::Row { ms, len } => {
                let n = i64::try_from(rune_count(s)).unwrap_or(i64::MAX);
                n == *len && row_match_all(ms, s)?
            }
            M::Single(seps) => {
                let (r, w) = decode(s);
                if s.len() > w {
                    return Ok(false);
                }
                !seps.contains(&r)
            }
            M::Suffix(x) => s.ends_with(x),
            M::SuffixAny(x, seps) => {
                if !s.ends_with(x) {
                    return Ok(false);
                }
                index_any_runes(sl(s, 0, s.len() - x.len())?, seps).is_none()
            }
            M::Super => true,
            M::Text(t) => s == t.as_slice(),
        })
    }

    fn index(&self, s: &[u8]) -> R<Index> {
        Ok(match self {
            M::Any(seps) => {
                let s = match index_any_runes(s, seps) {
                    Some(0) => return Ok(Some((0, vec![0]))),
                    Some(found) => sl(s, 0, found)?,
                    None => s,
                };
                let mut segs: Vec<usize> = runes(s).map(|(i, _)| i).collect();
                segs.push(s.len());
                Some((0, segs))
            }
            M::AnyOf(ms) => {
                let mut index: Option<usize> = None;
                let mut segments: Vec<usize> = Vec::new();
                for m in ms {
                    let Some((idx, seg)) = m.index(s)? else { continue };
                    match index {
                        None => {
                            index = Some(idx);
                            segments = seg;
                        }
                        Some(i) if idx < i => {
                            index = Some(idx);
                            segments = seg;
                        }
                        Some(i) if idx > i => {}
                        Some(_) => segments = append_merge(&segments, &seg),
                    }
                }
                index.map(|i| (i, segments))
            }
            M::BTree(_) => None,
            M::Contains { needle, not } => {
                let mut offset = 0;
                let idx = index(s, needle);
                let mut s = s;
                if !*not {
                    let Some(idx) = idx else { return Ok(None) };
                    offset = idx + needle.len();
                    if s.len() <= offset {
                        return Ok(Some((0, vec![offset])));
                    }
                    s = from(s, offset)?;
                } else if let Some(idx) = idx {
                    s = sl(s, 0, idx)?;
                }
                let mut segs: Vec<usize> = runes(s).map(|(i, _)| offset + i).collect();
                segs.push(offset + s.len());
                Some((0, segs))
            }
            M::EveryOf(ms) => {
                let (mut index, mut offset) = (0usize, 0usize);
                let mut current: Vec<usize> = Vec::new();
                let mut sub = s;
                for (i, m) in ms.iter().enumerate() {
                    let Some((idx, seg)) = m.index(sub)? else {
                        return Ok(None);
                    };
                    if i == 0 {
                        current.extend(seg);
                    } else {
                        let mut next = Vec::new();
                        // delta := index - (idx + offset), as ints.
                        let delta =
                            i64::try_from(index).unwrap_or(0) - i64::try_from(idx + offset).unwrap_or(0);
                        for &ex in &current {
                            for &n in &seg {
                                if i64::try_from(ex).unwrap_or(0) + delta == i64::try_from(n).unwrap_or(0) {
                                    next.push(n);
                                }
                            }
                        }
                        if next.is_empty() {
                            return Ok(None);
                        }
                        current = next;
                    }
                    index = idx + offset;
                    sub = from(s, index)?;
                    offset += idx;
                }
                Some((index, current))
            }
            M::List { list, not } => runes(s)
                .find(|&(_, r)| *not == !list.contains(&r))
                .map(|(i, r)| (i, vec![rune_len(r)])),
            M::Max(limit) => {
                let mut segs = vec![0];
                let mut count = 0;
                for (i, r) in runes(s) {
                    count += 1;
                    if count > *limit {
                        break;
                    }
                    segs.push(i + rune_len(r));
                }
                Some((0, segs))
            }
            M::Min(limit) => {
                let c = i64::try_from(s.len()).unwrap_or(i64::MAX)
                    - i64::try_from(*limit).unwrap_or(i64::MAX)
                    + 1;
                if c <= 0 {
                    return Ok(None);
                }
                let mut segs = Vec::new();
                let mut count = 0;
                for (i, r) in runes(s) {
                    count += 1;
                    if count >= *limit {
                        segs.push(i + rune_len(r));
                    }
                }
                if segs.is_empty() { None } else { Some((0, segs)) }
            }
            M::Nothing => Some((0, vec![0])),
            M::Prefix(p) => {
                let Some(idx) = index(s, p) else { return Ok(None) };
                let length = p.len();
                let sub = if s.len() > idx + length {
                    from(s, idx + length)?
                } else {
                    &[]
                };
                let mut segs = vec![length];
                segs.extend(runes(sub).map(|(i, r)| length + i + rune_len(r)));
                Some((idx, segs))
            }
            M::PrefixAny(p, seps) => {
                let Some(idx) = index(s, p) else { return Ok(None) };
                let n = p.len();
                let mut sub = from(s, idx + n)?;
                if let Some(i) = index_any_runes(sub, seps) {
                    sub = sl(sub, 0, i)?;
                }
                let mut segs = vec![n];
                segs.extend(runes(sub).map(|(i, r)| n + i + rune_len(r)));
                Some((idx, segs))
            }
            M::PrefixSuffix(p, x) => {
                let Some(prefix_idx) = index(s, p) else {
                    return Ok(None);
                };
                let suffix_len = x.len();
                if suffix_len == 0 {
                    return Ok(Some((prefix_idx, vec![s.len() - prefix_idx])));
                }
                if s.len() <= prefix_idx {
                    return Ok(None);
                }
                let mut segs = Vec::new();
                let mut sub = from(s, prefix_idx)?;
                while let Some(suffix_idx) = last_index(sub, x) {
                    segs.push(suffix_idx + suffix_len);
                    sub = sl(sub, 0, suffix_idx)?;
                }
                if segs.is_empty() {
                    return Ok(None);
                }
                segs.reverse();
                Some((prefix_idx, segs))
            }
            M::Range { lo, hi, not } => runes(s)
                .find(|&(_, r)| *not != (r >= *lo && r <= *hi))
                .map(|(i, r)| (i, vec![rune_len(r)])),
            M::Row { ms, len } => {
                for (i, _) in runes(s) {
                    let rest = from(s, i)?;
                    if i64::try_from(rest.len()).unwrap_or(i64::MAX) < *len {
                        break;
                    }
                    if row_match_all(ms, rest)? {
                        return Ok(Some((i, vec![usize::try_from(*len).unwrap_or(0)])));
                    }
                }
                None
            }
            M::Single(seps) => runes(s)
                .find(|&(_, r)| !seps.contains(&r))
                .map(|(i, r)| (i, vec![rune_len(r)])),
            M::Suffix(x) => index(s, x).map(|idx| (0, vec![idx + x.len()])),
            M::SuffixAny(x, seps) => {
                let Some(idx) = index(s, x) else { return Ok(None) };
                let i = last_index_any_runes(sl(s, 0, idx)?, seps)?.map_or(0, |i| i + 1);
                Some((i, vec![idx + x.len() - i]))
            }
            M::Super => {
                let mut segs: Vec<usize> = runes(s).map(|(i, _)| i).collect();
                segs.push(s.len());
                Some((0, segs))
            }
            M::Text(t) => index(s, t).map(|i| (i, vec![t.len()])),
        })
    }
}

/// Row.matchAll: each matcher on its length's worth of runes, cut one byte past the
/// start of its last rune.
fn row_match_all(ms: &[M], s: &[u8]) -> R<bool> {
    let mut idx = 0;
    for m in ms {
        let length = m.len();
        let (mut next, mut i) = (0usize, 0i64);
        for (at, _) in runes(from(s, idx)?) {
            next = at;
            i += 1;
            if i == length {
                break;
            }
        }
        if i < length || !m.matches(sl(s, idx, idx + next + 1)?)? {
            return Ok(false);
        }
        idx += next + 1;
    }
    Ok(true)
}

impl BTree {
    fn matches(&self, s: &[u8]) -> R<bool> {
        let input_len = i64::try_from(s.len()).unwrap_or(i64::MAX);
        if self.len != -1 && self.len > input_len {
            return Ok(false);
        }
        let mut offset: i64 = if self.left_len >= 0 { self.left_len } else { 0 };
        let limit: i64 = if self.right_len >= 0 {
            input_len - self.right_len
        } else {
            input_len
        };
        let at = |i: i64| usize::try_from(i).map_err(|_| PANIC.to_string());
        while offset < limit {
            let Some((index, segments)) = self.value.index(sl(s, at(offset)?, at(limit)?)?)? else {
                return Ok(false);
            };
            let start = at(offset)? + index;
            let l = sl(s, 0, start)?;
            let left = match &self.left {
                Some(m) => m.matches(l)?,
                None => l.is_empty(),
            };
            if left {
                for &length in segments.iter().rev() {
                    let r: &[u8] = if s.len() <= start + length {
                        &[]
                    } else {
                        from(s, start + length)?
                    };
                    let right = match &self.right {
                        Some(m) => m.matches(r)?,
                        None => r.is_empty(),
                    };
                    if right {
                        return Ok(true);
                    }
                }
            }
            let (_, step) = decode(from(s, start)?);
            offset = i64::try_from(start + step).unwrap_or(i64::MAX);
        }
        Ok(false)
    }
}

// The lexer (syntax/lexer).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Eof,
    Error,
    Text,
    Any,
    Super,
    Single,
    Not,
    Separator,
    RangeOpen,
    RangeClose,
    RangeLo,
    RangeHi,
    RangeBetween,
    TermsOpen,
    TermsClose,
}

impl Tok {
    fn name(self) -> &'static str {
        match self {
            Tok::Eof => "eof",
            Tok::Error => "error",
            Tok::Text => "text",
            Tok::Any => "any",
            Tok::Super => "super",
            Tok::Single => "single",
            Tok::Not => "not",
            Tok::Separator => "separator",
            Tok::RangeOpen => "range_open",
            Tok::RangeClose => "range_close",
            Tok::RangeLo => "range_lo",
            Tok::RangeHi => "range_hi",
            Tok::RangeBetween => "range_between",
            Tok::TermsOpen => "terms_open",
            Tok::TermsClose => "terms_close",
        }
    }
}

/// The lexer's end of input: rune 0, so a NUL ends a pattern where an item starts.
const EOF: u32 = 0;

struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
    err: Option<String>,
    tokens: std::collections::VecDeque<(Tok, String)>,
    terms_level: i64,
    last_rune: u32,
    last_rune_size: usize,
    has_rune: bool,
}

const TEXT_BREAKERS: &[u32] = &[0x3f, 0x2a, 0x5b, 0x7b];
const TERMS_BREAKERS: &[u32] = &[0x3f, 0x2a, 0x5b, 0x7b, 0x7d, 0x2c];

fn rune_string(r: u32) -> String {
    char::from_u32(r).unwrap_or('\u{FFFD}').to_string()
}

impl Lexer<'_> {
    fn next(&mut self) -> (Tok, String) {
        loop {
            if let Some(e) = &self.err {
                return (Tok::Error, e.clone());
            }
            if let Some(t) = self.tokens.pop_front() {
                return t;
            }
            self.fetch_item();
        }
    }

    fn peek(&mut self) -> (u32, usize) {
        if self.pos == self.data.len() {
            return (EOF, 0);
        }
        let (r, w) = decode(self.data.get(self.pos..).unwrap_or(&[]));
        if r == RUNE_ERROR {
            self.err = Some("could not read rune".to_string());
            return (EOF, 0);
        }
        (r, w)
    }

    fn read(&mut self) -> u32 {
        if self.has_rune {
            self.has_rune = false;
            self.pos += self.last_rune_size;
            return self.last_rune;
        }
        let (r, s) = self.peek();
        self.pos += s;
        self.last_rune = r;
        self.last_rune_size = s;
        r
    }

    fn unread(&mut self) {
        if self.has_rune {
            self.err = Some("could not unread rune".to_string());
            return;
        }
        self.pos = self.pos.saturating_sub(self.last_rune_size);
        self.has_rune = true;
    }

    fn push(&mut self, t: Tok, raw: String) {
        self.tokens.push_back((t, raw));
    }

    fn fetch_item(&mut self) {
        let r = self.read();
        let in_terms = self.terms_level > 0;
        match r {
            EOF => self.push(Tok::Eof, String::new()),
            0x7b => {
                self.terms_level += 1;
                self.push(Tok::TermsOpen, "{".into());
            }
            0x2c if in_terms => self.push(Tok::Separator, ",".into()),
            0x7d if in_terms => {
                self.push(Tok::TermsClose, "}".into());
                self.terms_level -= 1;
            }
            0x5b => {
                self.push(Tok::RangeOpen, "[".into());
                self.fetch_range();
            }
            0x3f => self.push(Tok::Single, "?".into()),
            0x2a => {
                if self.read() == 0x2a {
                    self.push(Tok::Super, "**".into());
                } else {
                    self.unread();
                    self.push(Tok::Any, "*".into());
                }
            }
            _ => {
                self.unread();
                self.fetch_text(if in_terms { TERMS_BREAKERS } else { TEXT_BREAKERS });
            }
        }
    }

    fn fetch_range(&mut self) {
        let (mut want_hi, mut want_close, mut seen_not) = (false, false, false);
        loop {
            let r = self.read();
            if r == EOF {
                self.err = Some("unexpected end of input".to_string());
                return;
            }
            if want_close {
                if r != 0x5d {
                    self.err = Some("expected close range character".to_string());
                } else {
                    self.push(Tok::RangeClose, "]".into());
                }
                return;
            }
            if want_hi {
                self.push(Tok::RangeHi, rune_string(r));
                want_close = true;
                continue;
            }
            if !seen_not && r == 0x21 {
                self.push(Tok::Not, "!".into());
                seen_not = true;
                continue;
            }
            let (n, w) = self.peek();
            if n == 0x2d {
                self.pos += w;
                self.push(Tok::RangeLo, rune_string(r));
                self.push(Tok::RangeBetween, "-".into());
                want_hi = true;
                continue;
            }
            self.unread();
            self.fetch_text(&[0x5d]);
            want_close = true;
        }
    }

    fn fetch_text(&mut self, breakers: &[u32]) {
        let mut data = String::new();
        let mut escaped = false;
        loop {
            let r = self.read();
            if r == EOF {
                break;
            }
            if !escaped {
                if r == 0x5c {
                    escaped = true;
                    continue;
                }
                if breakers.contains(&r) {
                    self.unread();
                    break;
                }
            }
            escaped = false;
            data.push_str(&rune_string(r));
        }
        if !data.is_empty() {
            self.push(Tok::Text, data);
        }
    }
}

// The parser (syntax/ast).

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Nothing,
    Pattern,
    List { not: bool, chars: String },
    Range { not: bool, lo: u32, hi: u32 },
    Text(String),
    Any,
    Super,
    Single,
    AnyOf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    kind: Kind,
    children: Vec<Node>,
}

fn node(kind: Kind, children: Vec<Node>) -> Node {
    Node { kind, children }
}

/// The tree under construction: nodes by index, each with its parent's.
struct Arena {
    kinds: Vec<Kind>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}

impl Arena {
    fn insert(&mut self, parent: usize, kind: Kind) -> usize {
        let id = self.kinds.len();
        self.kinds.push(kind);
        self.parents.push(Some(parent));
        self.children.push(Vec::new());
        if let Some(c) = self.children.get_mut(parent) {
            c.push(id);
        }
        id
    }

    fn build(&self, id: usize) -> Node {
        let kind = self.kinds.get(id).cloned().unwrap_or(Kind::Nothing);
        let children = self
            .children
            .get(id)
            .map(|c| c.iter().map(|&c| self.build(c)).collect())
            .unwrap_or_default();
        node(kind, children)
    }

    fn parent(&self, id: usize) -> Option<usize> {
        self.parents.get(id).copied().flatten()
    }
}

/// A nil pointer dereference, as Go would have it.
const NIL: &str = "runtime error: invalid memory address or nil pointer dereference";

fn parse(pattern: &str) -> Result<Node, String> {
    let mut lex = Lexer {
        data: pattern.as_bytes(),
        pos: 0,
        err: None,
        tokens: std::collections::VecDeque::new(),
        terms_level: 0,
        last_rune: 0,
        last_rune_size: 0,
        has_rune: false,
    };
    let mut a = Arena {
        kinds: vec![Kind::Pattern],
        parents: vec![None],
        children: vec![Vec::new()],
    };
    let mut tree = 0;
    'main: loop {
        let (t, raw) = lex.next();
        match t {
            Tok::Eof => break,
            Tok::Error => return Err(raw),
            Tok::Text => {
                a.insert(tree, Kind::Text(raw));
            }
            Tok::Any => {
                a.insert(tree, Kind::Any);
            }
            Tok::Super => {
                a.insert(tree, Kind::Super);
            }
            Tok::Single => {
                a.insert(tree, Kind::Single);
            }
            Tok::RangeOpen => {
                let (mut not, mut lo, mut hi, mut chars) = (false, 0u32, 0u32, String::new());
                loop {
                    let (t, raw) = lex.next();
                    match t {
                        Tok::Eof => return Err("unexpected end".to_string()),
                        Tok::Error => return Err(raw),
                        Tok::Not => not = true,
                        Tok::RangeLo => lo = raw.chars().next().map_or(RUNE_ERROR, u32::from),
                        Tok::RangeHi => {
                            hi = raw.chars().next().map_or(RUNE_ERROR, u32::from);
                            if hi < lo {
                                return Err(format!(
                                    "hi character '{}' should be greater than lo '{}'",
                                    rune_string(hi),
                                    rune_string(lo)
                                ));
                            }
                        }
                        Tok::Text => chars = raw,
                        Tok::RangeClose => {
                            let is_range = lo != 0 && hi != 0;
                            let is_chars = !chars.is_empty();
                            if is_chars == is_range {
                                return Err("could not parse range".to_string());
                            }
                            if is_range {
                                a.insert(tree, Kind::Range { not, lo, hi });
                            } else {
                                a.insert(tree, Kind::List { not, chars });
                            }
                            continue 'main;
                        }
                        _ => {}
                    }
                }
            }
            Tok::TermsOpen => {
                let any_of = a.insert(tree, Kind::AnyOf);
                tree = a.insert(any_of, Kind::Pattern);
            }
            Tok::Separator => {
                let parent = a.parent(tree).ok_or_else(|| NIL.to_string())?;
                tree = a.insert(parent, Kind::Pattern);
            }
            Tok::TermsClose => {
                tree = a
                    .parent(tree)
                    .and_then(|p| a.parent(p))
                    .ok_or_else(|| NIL.to_string())?;
            }
            _ => {
                let mut q = String::new();
                crate::goquote::quote(&mut q, &raw);
                return Err(format!("unexpected token: {}<{q}>", t.name()));
            }
        }
    }
    Ok(a.build(0))
}

// The compiler (compiler/compiler.go).

fn optimize(m: M) -> M {
    match m {
        M::Any(seps) if seps.is_empty() => M::Super,
        M::AnyOf(mut ms) if ms.len() == 1 => ms.pop().unwrap_or(M::Nothing),
        M::List { list, not: false } if list.len() == 1 => {
            text(list.first().map(|&r| encode(r)).unwrap_or_default())
        }
        M::BTree(mut t) => {
            t.left = t.left.map(optimize);
            t.right = t.right.map(optimize);
            let M::Text(r) = &t.value else {
                return M::BTree(t);
            };
            let r = r.clone();
            match (&t.left, &t.right) {
                (None, None) => text(r),
                (Some(M::Super), Some(M::Super)) => M::Contains {
                    needle: r,
                    not: false,
                },
                (Some(M::Super), None) => M::Suffix(r),
                (None, Some(M::Super)) => M::Prefix(r),
                (None, Some(M::Suffix(x))) => M::PrefixSuffix(r, x.clone()),
                (Some(M::Prefix(p)), None) => M::PrefixSuffix(p.clone(), r),
                (Some(M::Any(seps)), None) => M::SuffixAny(r, seps.clone()),
                (None, Some(M::Any(seps))) => M::PrefixAny(r, seps.clone()),
                _ => M::BTree(t),
            }
        }
        m => m,
    }
}

fn compile_matchers(ms: Vec<M>) -> M {
    if ms.len() == 1
        && let Some(m) = ms.first()
    {
        return m.clone();
    }
    if let Some(m) = glue(&ms) {
        return m;
    }
    let mut best: Option<usize> = None;
    let mut max_len = -1;
    for (i, m) in ms.iter().enumerate() {
        let l = m.len();
        if l != -1 && l >= max_len {
            max_len = l;
            best = Some(i);
        }
    }
    let Some(idx) = best else {
        let mut rest = ms;
        if rest.is_empty() {
            return M::Nothing;
        }
        let first = rest.remove(0);
        let r = compile_matchers(rest);
        return new_btree(first, None, Some(r));
    };
    let mut left = ms;
    let mut right = left.split_off(idx);
    let val = if right.is_empty() {
        M::Nothing
    } else {
        right.remove(0)
    };
    let l = if left.is_empty() {
        None
    } else {
        Some(compile_matchers(left))
    };
    let r = if right.is_empty() {
        None
    } else {
        Some(compile_matchers(right))
    };
    new_btree(val, l, r)
}

fn glue(ms: &[M]) -> Option<M> {
    glue_as_every(ms).or_else(|| glue_as_row(ms))
}

fn glue_as_row(ms: &[M]) -> Option<M> {
    if ms.len() <= 1 {
        return None;
    }
    let mut l = 0;
    for m in ms {
        let ml = m.len();
        if ml == -1 {
            return None;
        }
        l += ml;
    }
    Some(M::Row {
        ms: ms.to_vec(),
        len: l,
    })
}

fn glue_as_every(ms: &[M]) -> Option<M> {
    if ms.len() <= 1 {
        return None;
    }
    let (mut has_any, mut has_super, mut has_single) = (false, false, false);
    let mut min = 0;
    let mut separator: &[u32] = &[];
    for (i, m) in ms.iter().enumerate() {
        let sep: &[u32] = match m {
            M::Super => {
                has_super = true;
                &[]
            }
            M::Any(s) => {
                has_any = true;
                s
            }
            M::Single(s) => {
                has_single = true;
                min += 1;
                s
            }
            M::List { list, not: true } => {
                has_single = true;
                min += 1;
                list
            }
            _ => return None,
        };
        if i == 0 {
            separator = sep;
        }
        if sep != separator {
            return None;
        }
    }
    if has_super && !has_any && !has_single {
        return Some(M::Super);
    }
    if has_any && !has_super && !has_single {
        return Some(M::Any(separator.to_vec()));
    }
    if (has_any || has_super) && min > 0 && separator.is_empty() {
        return Some(M::Min(min));
    }
    let mut every = Vec::new();
    if min > 0 {
        every.push(M::Min(min));
        if !has_any && !has_super {
            every.push(M::Max(min));
        }
    }
    if !separator.is_empty() {
        let needle: String = separator.iter().map(|&r| rune_string(r)).collect();
        every.push(M::Contains {
            needle: needle.into_bytes(),
            not: true,
        });
    }
    Some(M::EveryOf(every))
}

fn minimize_matchers(ms: Vec<M>) -> Vec<M> {
    let mut done: Option<M> = None;
    let (mut left, mut right, mut count) = (0, 0, 0);
    for l in 0..ms.len() {
        for r in (l + 1..=ms.len()).rev() {
            let Some(glued) = ms.get(l..r).and_then(glue) else {
                continue;
            };
            let swap = match &done {
                None => true,
                Some(d) => {
                    let (cl, gl) = (d.len(), glued.len());
                    (cl > -1 && gl > -1 && gl > cl) || count < r - l
                }
            };
            if swap {
                done = Some(glued);
                left = l;
                right = r;
                count = r - l;
            }
        }
    }
    let Some(done) = done else {
        return ms;
    };
    let total = ms.len();
    let mut next: Vec<M> = ms.get(..left).unwrap_or(&[]).to_vec();
    next.push(done);
    if right < total {
        next.extend(ms.get(right..).unwrap_or(&[]).iter().cloned());
    }
    if next.len() == total {
        return next;
    }
    minimize_matchers(next)
}

/// commonChildren: the children every node starts and ends with.
fn common_children(nodes: &[Node]) -> (Vec<Node>, Vec<Node>) {
    if nodes.len() <= 1 {
        return (Vec::new(), Vec::new());
    }
    // leastChildren: the first with the fewest.
    let mut idx = 0;
    for (i, n) in nodes.iter().enumerate() {
        if n.children.len() < nodes.get(idx).map_or(0, |m| m.children.len()) {
            idx = i;
        }
    }
    let Some(tree) = nodes.get(idx) else {
        return (Vec::new(), Vec::new());
    };
    let tree_len = tree.children.len();
    let mut common_left = Vec::new();
    let mut common_right: Vec<Option<Node>> = vec![None; tree_len];
    let mut last_right = tree_len;
    let (mut break_left, mut break_right) = (false, false);
    let mut common_total = 0;
    let (mut i, mut j) = (0usize, isize::try_from(tree_len).unwrap_or(0) - 1);
    while common_total < tree_len && j >= 0 && !(break_left && break_right) {
        let ju = usize::try_from(j).unwrap_or(0);
        let (Some(tree_left), Some(tree_right)) = (tree.children.get(i), tree.children.get(ju)) else {
            break;
        };
        for (k, n) in nodes.iter().enumerate() {
            if break_left && break_right {
                break;
            }
            if k == idx {
                continue;
            }
            let rest_left = n.children.get(i);
            let rest_right = n.children.get(ju + n.children.len() - tree_len);
            break_left = break_left || rest_left != Some(tree_left);
            break_right = break_right || (!break_left && ju <= i);
            break_right = break_right || rest_right != Some(tree_right);
        }
        if !break_left {
            common_total += 1;
            common_left.push(tree_left.clone());
        }
        if !break_right {
            common_total += 1;
            last_right = ju;
            if let Some(slot) = common_right.get_mut(ju) {
                *slot = Some(tree_right.clone());
            }
        }
        i += 1;
        j -= 1;
    }
    let right = common_right.into_iter().skip(last_right).flatten().collect();
    (common_left, right)
}

fn minimize_tree_any_of(tree: &Node) -> Option<Node> {
    if !tree.children.iter().all(|c| c.kind == Kind::Pattern) {
        return None;
    }
    let (common_left, common_right) = common_children(&tree.children);
    let (cl, cr) = (common_left.len(), common_right.len());
    if cl == 0 && cr == 0 {
        return None;
    }
    let mut result = Vec::new();
    if cl > 0 {
        result.push(node(Kind::Pattern, common_left));
    }
    let mut any_of: Vec<Node> = Vec::new();
    for child in &tree.children {
        let end = child.children.len().saturating_sub(cr);
        let reuse = child.children.get(cl..end).unwrap_or(&[]);
        let n = if reuse.is_empty() {
            node(Kind::Nothing, Vec::new())
        } else {
            node(Kind::Pattern, reuse.to_vec())
        };
        if !any_of.contains(&n) {
            any_of.push(n);
        }
    }
    if any_of.len() == 1 {
        if let Some(only) = any_of.pop()
            && only.kind != Kind::Nothing
        {
            result.push(only);
        }
    } else if any_of.len() > 1 {
        result.push(node(Kind::AnyOf, any_of));
    }
    if cr > 0 {
        result.push(node(Kind::Pattern, common_right));
    }
    Some(node(Kind::Pattern, result))
}

fn compile_children(tree: &Node, sep: &[u32]) -> Vec<M> {
    tree.children
        .iter()
        .map(|c| optimize(compile_tree(c, sep)))
        .collect()
}

fn compile_tree(tree: &Node, sep: &[u32]) -> M {
    let m = match &tree.kind {
        Kind::AnyOf => {
            if let Some(n) = minimize_tree_any_of(tree) {
                return compile_tree(&n, sep);
            }
            return M::AnyOf(compile_children(tree, sep));
        }
        Kind::Pattern => {
            if tree.children.is_empty() {
                return M::Nothing;
            }
            compile_matchers(minimize_matchers(compile_children(tree, sep)))
        }
        Kind::Any => M::Any(sep.to_vec()),
        Kind::Super => M::Super,
        Kind::Single => M::Single(sep.to_vec()),
        Kind::Nothing => M::Nothing,
        Kind::List { not, chars } => M::List {
            list: chars.chars().map(u32::from).collect(),
            not: *not,
        },
        Kind::Range { not, lo, hi } => M::Range {
            lo: *lo,
            hi: *hi,
            not: *not,
        },
        Kind::Text(t) => text(t.clone().into_bytes()),
    };
    optimize(m)
}

/// A compiled glob.
#[derive(Debug)]
pub struct Glob(M);

/// glob.Compile.
pub fn compile(pattern: &str, separators: &[char]) -> Result<Glob, String> {
    let tree = parse(pattern)?;
    let sep: Vec<u32> = separators.iter().map(|&c| u32::from(c)).collect();
    Ok(Glob(compile_tree(&tree, &sep)))
}

impl Glob {
    /// Glob.Match; an error where Go would panic.
    pub fn matches(&self, s: &[u8]) -> Result<bool, String> {
        self.0.matches(s)
    }
}

/// glob.QuoteMeta.
pub fn quote_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(c, '*' | '?' | '\\' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
