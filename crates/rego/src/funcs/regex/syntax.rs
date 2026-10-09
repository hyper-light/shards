//! Go's regexp/syntax parser (go1.26.1 src/regexp/syntax/parse.go) with the flags
//! regexp.Compile gives it (`syntax.Perl`): what it accepts, what it rejects and in
//! which words, and the tree it builds, written out again as a pattern the `regex`
//! crate reads as Go reads the original.
//!
//! The tree is Go's without its optimisations that do not change what matches
//! (single-rune classes made literals, alternations factored, classes merged), so the
//! size and depth limits (`expression too large`, `expression nests too deeply`) are
//! measured on a tree a little larger than Go's for some patterns near those limits.
//! Unicode classes are the `regex` crate's tables (Unicode 16) where Go has its own
//! (Unicode 15.0.0): they differ only on code points Unicode 16 assigned.

use std::fmt::Write as _;

// Parse flags (syntax.Flags).
const FOLD_CASE: u16 = 1;
const CLASS_NL: u16 = 1 << 2;
const DOT_NL: u16 = 1 << 3;
const ONE_LINE: u16 = 1 << 4;
const NON_GREEDY: u16 = 1 << 5;
const PERL_X: u16 = 1 << 6;
const UNICODE_GROUPS: u16 = 1 << 7;
/// syntax.Perl.
const PERL: u16 = CLASS_NL | ONE_LINE | PERL_X | UNICODE_GROUPS;

const MAX_HEIGHT: usize = 1000;
const MAX_SIZE: i64 = (128 << 20) / 40;
const MAX_RUNE: u32 = 0x10FFFF;

// Error codes (syntax.ErrorCode).
const ERR_INVALID_CHAR_RANGE: &str = "invalid character class range";
const ERR_INVALID_ESCAPE: &str = "invalid escape sequence";
const ERR_INVALID_NAMED_CAPTURE: &str = "invalid named capture";
const ERR_INVALID_PERL_OP: &str = "invalid or unsupported Perl syntax";
const ERR_INVALID_REPEAT_OP: &str = "invalid nested repetition operator";
const ERR_INVALID_REPEAT_SIZE: &str = "invalid repeat count";
const ERR_MISSING_BRACKET: &str = "missing closing ]";
const ERR_MISSING_PAREN: &str = "missing closing )";
const ERR_MISSING_REPEAT_ARGUMENT: &str = "missing argument to repetition operator";
const ERR_TRAILING_BACKSLASH: &str = "trailing backslash at end of expression";
const ERR_UNEXPECTED_PAREN: &str = "unexpected )";
const ERR_NESTING_DEPTH: &str = "expression nests too deeply";
const ERR_LARGE: &str = "expression too large";

/// syntax.Error's text.
fn error(code: &str, expr: &str) -> String {
    format!("error parsing regexp: {code}: `{expr}`")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    NoMatch,
    EmptyMatch,
    Literal,
    CharClass,
    AnyCharNotNL,
    AnyChar,
    BeginLine,
    EndLine,
    BeginText,
    EndText,
    WordBoundary,
    NoWordBoundary,
    Capture,
    Star,
    Plus,
    Quest,
    Repeat,
    Concat,
    Alternate,
    // Pseudo-ops for the parse stack.
    LeftParen,
    VerticalBar,
}

impl Op {
    fn pseudo(self) -> bool {
        matches!(self, Op::LeftParen | Op::VerticalBar)
    }
}

/// A Unicode table a `\p` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    /// A general category, by Go's key.
    Category(&'static str),
    /// A script, by Go's key.
    Script(&'static str),
    Any,
    Ascii,
}

/// One item of a character class, as Go appends it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Range(u32, u32),
    /// A Perl or POSIX group: ASCII ranges.
    Group {
        ranges: &'static [(u32, u32)],
        negated: bool,
    },
    Table {
        table: Table,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    items: Vec<Item>,
    negated: bool,
    fold: bool,
}

#[derive(Debug, Clone)]
struct Node {
    op: Op,
    flags: u16,
    runes: Vec<u32>,
    class: Option<Class>,
    min: i32,
    max: i32,
    cap: usize,
    name: String,
    subs: Vec<Node>,
    height: usize,
    size: i64,
}

impl Node {
    fn new(op: Op, flags: u16) -> Node {
        Node {
            op,
            flags,
            runes: Vec::new(),
            class: None,
            min: 0,
            max: 0,
            cap: 0,
            name: String::new(),
            subs: Vec::new(),
            height: 1,
            size: 1,
        }
    }

    /// Go's calcHeight and calcSize, from the children's.
    fn measure(&mut self) {
        self.height = 1 + self.subs.iter().map(|s| s.height).max().unwrap_or(0);
        let sub = self.subs.first().map_or(0, |s| s.size);
        let size: i64 = match self.op {
            Op::Literal => i64::try_from(self.runes.len()).unwrap_or(i64::MAX),
            Op::Capture | Op::Star => sub.saturating_add(2),
            Op::Plus | Op::Quest => sub.saturating_add(1),
            Op::Concat => self.subs.iter().fold(0i64, |a, s| a.saturating_add(s.size)),
            Op::Alternate => {
                let n = i64::try_from(self.subs.len()).unwrap_or(i64::MAX);
                let sum = self.subs.iter().fold(0i64, |a, s| a.saturating_add(s.size));
                if n > 1 { sum.saturating_add(n - 1) } else { sum }
            }
            Op::Repeat => {
                if self.max == -1 {
                    if self.min == 0 {
                        sub.saturating_add(2)
                    } else {
                        i64::from(self.min).saturating_mul(sub).saturating_add(1)
                    }
                } else {
                    i64::from(self.max)
                        .saturating_mul(sub)
                        .saturating_add(i64::from(self.max - self.min))
                }
            }
            _ => 0,
        };
        self.size = size.max(1);
    }
}

/// A parse failure: Go's error text.
type Error = String;

struct Parser<'a> {
    flags: u16,
    stack: Vec<Node>,
    num_cap: usize,
    whole: &'a str,
    num_regexp: usize,
    repeats: i64,
    track_size: bool,
}

/// What `\p` and Go's POSIX and Perl groups are made of.
const DIGIT: &[(u32, u32)] = &[(0x30, 0x39)];
const SPACE_PERL: &[(u32, u32)] = &[(0x9, 0xa), (0xc, 0xd), (0x20, 0x20)];
const WORD: &[(u32, u32)] = &[(0x30, 0x39), (0x41, 0x5a), (0x5f, 0x5f), (0x61, 0x7a)];

fn perl_group(c: u8) -> Option<(&'static [(u32, u32)], bool)> {
    Some(match c {
        b'd' => (DIGIT, false),
        b'D' => (DIGIT, true),
        b's' => (SPACE_PERL, false),
        b'S' => (SPACE_PERL, true),
        b'w' => (WORD, false),
        b'W' => (WORD, true),
        _ => return None,
    })
}

fn posix_group(name: &str) -> Option<(&'static [(u32, u32)], bool)> {
    let (negated, body) = match name.strip_prefix("[:^") {
        Some(rest) => (true, rest),
        None => (false, name.strip_prefix("[:")?),
    };
    let ranges: &'static [(u32, u32)] = match body {
        "alnum:]" => &[(0x30, 0x39), (0x41, 0x5a), (0x61, 0x7a)],
        "alpha:]" => &[(0x41, 0x5a), (0x61, 0x7a)],
        "ascii:]" => &[(0x0, 0x7f)],
        "blank:]" => &[(0x9, 0x9), (0x20, 0x20)],
        "cntrl:]" => &[(0x0, 0x1f), (0x7f, 0x7f)],
        "digit:]" => DIGIT,
        "graph:]" => &[(0x21, 0x7e)],
        "lower:]" => &[(0x61, 0x7a)],
        "print:]" => &[(0x20, 0x7e)],
        "punct:]" => &[(0x21, 0x2f), (0x3a, 0x40), (0x5b, 0x60), (0x7b, 0x7e)],
        "space:]" => &[(0x9, 0xd), (0x20, 0x20)],
        "upper:]" => &[(0x41, 0x5a)],
        "word:]" => WORD,
        "xdigit:]" => &[(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)],
        _ => return None,
    };
    Some((ranges, negated))
}

/// unicode.Categories' keys in Go 1.26.
const CATEGORIES: &[&str] = &[
    "C", "Cc", "Cf", "Cn", "Co", "Cs", "L", "LC", "Ll", "Lm", "Lo", "Lt", "Lu", "M", "Mc", "Me", "Mn", "N",
    "Nd", "Nl", "No", "P", "Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps", "S", "Sc", "Sk", "Sm", "So", "Z", "Zl",
    "Zp", "Zs",
];

/// unicode.CategoryAliases in Go 1.26.
const CATEGORY_ALIASES: &[(&str, &str)] = &[
    ("Cased_Letter", "LC"),
    ("Close_Punctuation", "Pe"),
    ("Combining_Mark", "M"),
    ("Connector_Punctuation", "Pc"),
    ("Control", "Cc"),
    ("Currency_Symbol", "Sc"),
    ("Dash_Punctuation", "Pd"),
    ("Decimal_Number", "Nd"),
    ("Enclosing_Mark", "Me"),
    ("Final_Punctuation", "Pf"),
    ("Format", "Cf"),
    ("Initial_Punctuation", "Pi"),
    ("Letter", "L"),
    ("Letter_Number", "Nl"),
    ("Line_Separator", "Zl"),
    ("Lowercase_Letter", "Ll"),
    ("Mark", "M"),
    ("Math_Symbol", "Sm"),
    ("Modifier_Letter", "Lm"),
    ("Modifier_Symbol", "Sk"),
    ("Nonspacing_Mark", "Mn"),
    ("Number", "N"),
    ("Open_Punctuation", "Ps"),
    ("Other", "C"),
    ("Other_Letter", "Lo"),
    ("Other_Number", "No"),
    ("Other_Punctuation", "Po"),
    ("Other_Symbol", "So"),
    ("Paragraph_Separator", "Zp"),
    ("Private_Use", "Co"),
    ("Punctuation", "P"),
    ("Separator", "Z"),
    ("Space_Separator", "Zs"),
    ("Spacing_Mark", "Mc"),
    ("Surrogate", "Cs"),
    ("Symbol", "S"),
    ("Titlecase_Letter", "Lt"),
    ("Unassigned", "Cn"),
    ("Uppercase_Letter", "Lu"),
    ("cntrl", "Cc"),
    ("digit", "Nd"),
    ("punct", "P"),
];

/// unicode.Scripts' keys in Go 1.26 that Go's lookup can reach: it looks a name up
/// by its canonical form (one capital, then lower case, no `_`), which only these
/// keys already have.
const SCRIPTS: &[&str] = &[
    "Adlam",
    "Ahom",
    "Arabic",
    "Armenian",
    "Avestan",
    "Balinese",
    "Bamum",
    "Batak",
    "Bengali",
    "Bhaiksuki",
    "Bopomofo",
    "Brahmi",
    "Braille",
    "Buginese",
    "Buhid",
    "Carian",
    "Chakma",
    "Cham",
    "Cherokee",
    "Chorasmian",
    "Common",
    "Coptic",
    "Cuneiform",
    "Cypriot",
    "Cyrillic",
    "Deseret",
    "Devanagari",
    "Dogra",
    "Duployan",
    "Elbasan",
    "Elymaic",
    "Ethiopic",
    "Georgian",
    "Glagolitic",
    "Gothic",
    "Grantha",
    "Greek",
    "Gujarati",
    "Gurmukhi",
    "Han",
    "Hangul",
    "Hanunoo",
    "Hatran",
    "Hebrew",
    "Hiragana",
    "Inherited",
    "Javanese",
    "Kaithi",
    "Kannada",
    "Katakana",
    "Kawi",
    "Kharoshthi",
    "Khmer",
    "Khojki",
    "Khudawadi",
    "Lao",
    "Latin",
    "Lepcha",
    "Limbu",
    "Lisu",
    "Lycian",
    "Lydian",
    "Mahajani",
    "Makasar",
    "Malayalam",
    "Mandaic",
    "Manichaean",
    "Marchen",
    "Medefaidrin",
    "Miao",
    "Modi",
    "Mongolian",
    "Mro",
    "Multani",
    "Myanmar",
    "Nabataean",
    "Nandinagari",
    "Newa",
    "Nko",
    "Nushu",
    "Ogham",
    "Oriya",
    "Osage",
    "Osmanya",
    "Palmyrene",
    "Phoenician",
    "Rejang",
    "Runic",
    "Samaritan",
    "Saurashtra",
    "Sharada",
    "Shavian",
    "Siddham",
    "Sinhala",
    "Sogdian",
    "Soyombo",
    "Sundanese",
    "Syriac",
    "Tagalog",
    "Tagbanwa",
    "Takri",
    "Tamil",
    "Tangsa",
    "Tangut",
    "Telugu",
    "Thaana",
    "Thai",
    "Tibetan",
    "Tifinagh",
    "Tirhuta",
    "Toto",
    "Ugaritic",
    "Vai",
    "Vithkuqi",
    "Wancho",
    "Yezidi",
    "Yi",
];

/// canonicalName: a leading capital, then lower case, without `_`, `-` and spaces.
fn canonical_name(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    let mut first = true;
    for &c in name.as_bytes() {
        if matches!(c, b'_' | b'-' | b' ') {
            continue;
        }
        if first {
            out.push(c.to_ascii_uppercase());
            first = false;
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// unicodeTable: the table a name names, and whether it is inverted.
fn unicode_table(name: &str) -> Option<(Table, bool)> {
    let canon = canonical_name(name);
    match canon.as_slice() {
        b"Any" => return Some((Table::Any, false)),
        b"Assigned" => return Some((Table::Category("Cn"), true)),
        b"Ascii" => return Some((Table::Ascii, false)),
        b"Lc" => return Some((Table::Category("LC"), false)),
        _ => {}
    }
    if let Some(c) = CATEGORIES.iter().find(|c| c.as_bytes() == canon.as_slice()) {
        return Some((Table::Category(c), false));
    }
    if let Some(s) = SCRIPTS.iter().find(|s| s.as_bytes() == canon.as_slice()) {
        return Some((Table::Script(s), false));
    }
    CATEGORY_ALIASES
        .iter()
        .find(|(alias, _)| canonical_name(alias) == canon)
        .map(|(_, actual)| (Table::Category(actual), false))
}

fn is_alnum(c: u32) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a)
}

fn unhex(c: u32) -> Option<u32> {
    match c {
        0x30..=0x39 => Some(c - 0x30),
        0x61..=0x66 => Some(c - 0x61 + 10),
        0x41..=0x46 => Some(c - 0x41 + 10),
        _ => None,
    }
}

/// The rune at byte `i` of `s` and the position after it; at the end, Go's
/// `utf8.DecodeRuneInString("")`: RuneError and no width.
fn next_rune(s: &str, i: usize) -> (u32, usize) {
    match s.get(i..).and_then(|t| t.chars().next()) {
        Some(c) => (u32::from(c), i + c.len_utf8()),
        None => (0xFFFD, i),
    }
}

fn byte(s: &str, i: usize) -> Option<u8> {
    s.as_bytes().get(i).copied()
}

fn sub(s: &str, a: usize, b: usize) -> &str {
    s.get(a..b).unwrap_or("")
}

impl Parser<'_> {
    fn new_node(&mut self, op: Op) -> Node {
        self.num_regexp = self.num_regexp.saturating_add(1);
        Node::new(op, self.flags)
    }

    /// checkLimits on a node about to go on the stack.
    fn check_limits(&mut self, re: &Node) -> Result<(), Error> {
        self.check_size(re)?;
        if self.num_regexp >= MAX_HEIGHT && re.height > MAX_HEIGHT {
            return Err(error(ERR_NESTING_DEPTH, self.whole));
        }
        Ok(())
    }

    fn check_size(&mut self, re: &Node) -> Result<(), Error> {
        if !self.track_size {
            if self.repeats == 0 {
                self.repeats = 1;
            }
            if re.op == Op::Repeat {
                let mut n = if re.max == -1 { re.min } else { re.max };
                if n <= 0 {
                    n = 1;
                }
                let n = i64::from(n);
                if n > MAX_SIZE / self.repeats {
                    self.repeats = MAX_SIZE;
                } else {
                    self.repeats = self.repeats.saturating_mul(n);
                }
            }
            if i64::try_from(self.num_regexp).unwrap_or(i64::MAX) < MAX_SIZE / self.repeats {
                return Ok(());
            }
            self.track_size = true;
            if self.stack.iter().any(|s| s.size > MAX_SIZE) {
                return Err(error(ERR_LARGE, self.whole));
            }
        }
        if re.size > MAX_SIZE {
            return Err(error(ERR_LARGE, self.whole));
        }
        Ok(())
    }

    fn push(&mut self, mut re: Node) -> Result<(), Error> {
        re.measure();
        self.maybe_concat();
        self.check_limits(&re)?;
        self.stack.push(re);
        Ok(())
    }

    /// maybeConcat: merges the top two literals of the stack if their case folding
    /// agrees.
    fn maybe_concat(&mut self) {
        let n = self.stack.len();
        if n < 2 {
            return;
        }
        let (Some(re1), Some(re2)) = (self.stack.get(n - 1), self.stack.get(n - 2)) else {
            return;
        };
        if re1.op != Op::Literal || re2.op != Op::Literal || re1.flags & FOLD_CASE != re2.flags & FOLD_CASE {
            return;
        }
        if let Some(re1) = self.stack.pop()
            && let Some(re2) = self.stack.last_mut()
        {
            re2.runes.extend(re1.runes);
            re2.measure();
        }
    }

    fn literal(&mut self, r: u32) -> Result<(), Error> {
        let mut re = self.new_node(Op::Literal);
        re.runes.push(r);
        self.push(re)
    }

    fn op(&mut self, op: Op) -> Result<(), Error> {
        let re = self.new_node(op);
        self.push(re)
    }

    /// repeat: the top of the stack repeated. `before` is where the operator starts,
    /// `after` where it ends; returns the new `after`.
    fn repeat(
        &mut self,
        op: Op,
        min: i32,
        max: i32,
        before: usize,
        mut after: usize,
        last_repeat: Option<usize>,
    ) -> Result<usize, Error> {
        let mut flags = self.flags;
        if self.flags & PERL_X != 0 {
            if byte(self.whole, after) == Some(b'?') {
                after += 1;
                flags ^= NON_GREEDY;
            }
            if let Some(last) = last_repeat {
                return Err(error(ERR_INVALID_REPEAT_OP, sub(self.whole, last, after)));
            }
        }
        let missing = || error(ERR_MISSING_REPEAT_ARGUMENT, sub(self.whole, before, after));
        match self.stack.last() {
            None => return Err(missing()),
            Some(top) if top.op.pseudo() => return Err(missing()),
            Some(_) => {}
        }
        let Some(top) = self.stack.pop() else {
            return Err(missing());
        };
        self.num_regexp = self.num_regexp.saturating_add(1);
        let mut re = Node::new(op, flags);
        re.min = min;
        re.max = max;
        re.subs.push(top);
        re.measure();
        let checked = self.check_limits(&re);
        let valid = !(op == Op::Repeat && (min >= 2 || max >= 2) && !repeat_is_valid(&re, 1000));
        self.stack.push(re);
        checked?;
        if !valid {
            return Err(error(ERR_INVALID_REPEAT_SIZE, sub(self.whole, before, after)));
        }
        Ok(after)
    }

    /// concat: the top of the stack, above the topmost `|` or `(`, concatenated.
    fn concat(&mut self) -> Result<(), Error> {
        self.maybe_concat();
        let i = self
            .stack
            .iter()
            .rposition(|n| n.op.pseudo())
            .map_or(0, |i| i + 1);
        let subs = self.stack.split_off(i);
        if subs.is_empty() {
            let re = self.new_node(Op::EmptyMatch);
            return self.push(re);
        }
        let re = self.collapse(subs, Op::Concat);
        self.push(re)
    }

    /// alternate: the top of the stack, above the topmost `(`, as alternatives.
    fn alternate(&mut self) -> Result<(), Error> {
        let i = self
            .stack
            .iter()
            .rposition(|n| n.op.pseudo())
            .map_or(0, |i| i + 1);
        let subs = self.stack.split_off(i);
        if subs.is_empty() {
            let re = self.new_node(Op::NoMatch);
            return self.push(re);
        }
        let re = self.collapse(subs, Op::Alternate);
        self.push(re)
    }

    /// collapse: `op` applied to subs, hoisting any of the same op.
    fn collapse(&mut self, mut subs: Vec<Node>, op: Op) -> Node {
        if subs.len() == 1
            && let Some(one) = subs.pop()
        {
            return one;
        }
        let mut re = self.new_node(op);
        for s in subs {
            if s.op == op {
                re.subs.extend(s.subs);
            } else {
                re.subs.push(s);
            }
        }
        re.measure();
        re
    }

    /// swapVerticalBar: if the top of the stack is an element above a `|`, swaps them.
    fn swap_vertical_bar(&mut self) -> bool {
        let n = self.stack.len();
        if n >= 2 && self.stack.get(n - 2).is_some_and(|re| re.op == Op::VerticalBar) {
            self.stack.swap(n - 2, n - 1);
            return true;
        }
        false
    }

    fn parse_vertical_bar(&mut self) -> Result<(), Error> {
        self.concat()?;
        if !self.swap_vertical_bar() {
            self.op(Op::VerticalBar)?;
        }
        Ok(())
    }

    fn parse_right_paren(&mut self) -> Result<(), Error> {
        self.concat()?;
        if self.swap_vertical_bar() {
            self.stack.pop();
        }
        self.alternate()?;
        let unexpected = || error(ERR_UNEXPECTED_PAREN, self.whole);
        if self.stack.len() < 2 {
            return Err(unexpected());
        }
        let (Some(re1), Some(mut re2)) = (self.stack.pop(), self.stack.pop()) else {
            return Err(unexpected());
        };
        if re2.op != Op::LeftParen {
            return Err(unexpected());
        }
        self.flags = re2.flags;
        if re2.cap == 0 {
            self.push(re1)
        } else {
            re2.op = Op::Capture;
            re2.subs.push(re1);
            self.push(re2)
        }
    }

    /// parsePerlFlags: `(?` flags and non-capturing and named groups, at byte `at`.
    fn parse_perl_flags(&mut self, at: usize) -> Result<usize, Error> {
        let s = self.whole.get(at..).unwrap_or("");
        let b = s.as_bytes();
        let starts_with_p = b.len() > 4 && b.get(2) == Some(&b'P') && b.get(3) == Some(&b'<');
        let starts_with_name = b.len() > 3 && b.get(2) == Some(&b'<');
        if starts_with_p || starts_with_name {
            let expr_start = if starts_with_name { 3 } else { 4 };
            let Some(end) = s.find('>') else {
                return Err(error(ERR_INVALID_NAMED_CAPTURE, s));
            };
            let capture = sub(s, 0, end + 1);
            let name = sub(s, expr_start, end);
            if name.is_empty() || !name.bytes().all(|c| c == b'_' || is_alnum(u32::from(c))) {
                return Err(error(ERR_INVALID_NAMED_CAPTURE, capture));
            }
            self.num_cap += 1;
            let mut re = self.new_node(Op::LeftParen);
            re.cap = self.num_cap;
            re.name = name.to_string();
            self.push(re)?;
            return Ok(at + end + 1);
        }

        let mut t = 2;
        let mut flags = self.flags;
        let mut negative = false;
        let mut saw_flag = false;
        while t < s.len() {
            let (c, next) = next_rune(s, t);
            t = next;
            match c {
                0x69 /* i */ => {
                    flags = if negative { flags & !FOLD_CASE } else { flags | FOLD_CASE };
                    saw_flag = true;
                }
                0x6d /* m */ => {
                    flags = if negative { flags | ONE_LINE } else { flags & !ONE_LINE };
                    saw_flag = true;
                }
                0x73 /* s */ => {
                    flags = if negative { flags & !DOT_NL } else { flags | DOT_NL };
                    saw_flag = true;
                }
                0x55 /* U */ => {
                    flags = if negative { flags & !NON_GREEDY } else { flags | NON_GREEDY };
                    saw_flag = true;
                }
                0x2d /* - */ => {
                    if negative {
                        break;
                    }
                    negative = true;
                    saw_flag = false;
                }
                0x3a | 0x29 /* : ) */ => {
                    if negative && !saw_flag {
                        break;
                    }
                    if c == 0x3a {
                        self.op(Op::LeftParen)?;
                    }
                    self.flags = flags;
                    return Ok(at + t);
                }
                _ => break,
            }
        }
        Err(error(ERR_INVALID_PERL_OP, sub(s, 0, t)))
    }

    /// parseEscape: an escape at byte `at` (a `\`), as one rune.
    fn parse_escape(&self, at: usize) -> Result<(u32, usize), Error> {
        let s = self.whole;
        let mut t = at + 1;
        if t >= s.len() {
            return Err(error(ERR_TRAILING_BACKSLASH, ""));
        }
        let (c, next) = next_rune(s, t);
        t = next;
        let invalid = |t: usize| error(ERR_INVALID_ESCAPE, sub(s, at, t));
        let octal = |i: usize| byte(s, i).filter(|b| (b'0'..=b'7').contains(b));
        match c {
            0x31..=0x37 | 0x30 => {
                if c != 0x30 && octal(t).is_none() {
                    // A single non-zero digit is a backreference; not supported.
                    return Err(invalid(t));
                }
                let mut r = c - 0x30;
                for _ in 1..3 {
                    let Some(d) = octal(t) else { break };
                    r = r * 8 + u32::from(d - b'0');
                    t += 1;
                }
                Ok((r, t))
            }
            0x78 /* x */ => {
                if t >= s.len() {
                    return Err(invalid(t));
                }
                let (c, next) = next_rune(s, t);
                t = next;
                if c == 0x7b {
                    let mut nhex = 0;
                    let mut r: u32 = 0;
                    loop {
                        if t >= s.len() {
                            return Err(invalid(t));
                        }
                        let (c, next) = next_rune(s, t);
                        t = next;
                        if c == 0x7d {
                            break;
                        }
                        let Some(v) = unhex(c) else {
                            return Err(invalid(t));
                        };
                        r = r * 16 + v;
                        if r > MAX_RUNE {
                            return Err(invalid(t));
                        }
                        nhex += 1;
                    }
                    if nhex == 0 {
                        return Err(invalid(t));
                    }
                    return Ok((r, t));
                }
                let x = unhex(c);
                let (c, next) = next_rune(s, t);
                t = next;
                match (x, unhex(c)) {
                    (Some(x), Some(y)) => Ok((x * 16 + y, t)),
                    _ => Err(invalid(t)),
                }
            }
            0x61 => Ok((0x7, t)),
            0x66 => Ok((0xc, t)),
            0x6e => Ok((0xa, t)),
            0x72 => Ok((0xd, t)),
            0x74 => Ok((0x9, t)),
            0x76 => Ok((0xb, t)),
            _ if c < 0x80 && !is_alnum(c) => Ok((c, t)),
            _ => Err(invalid(t)),
        }
    }

    /// parseUnicodeClass: a `\p` or `\P` at byte `at`, if one is there.
    fn parse_unicode_class(&self, at: usize) -> Result<Option<(Item, usize)>, Error> {
        let s = self.whole.get(at..).unwrap_or("");
        let b = s.as_bytes();
        if self.flags & UNICODE_GROUPS == 0 || b.len() < 2 || b.first() != Some(&b'\\') {
            return Ok(None);
        }
        let mut negated = match b.get(1) {
            Some(b'p') => false,
            Some(b'P') => true,
            _ => return Ok(None),
        };
        let (c, t) = next_rune(s, 2);
        let (seq, mut name, rest) = if c != 0x7b {
            (sub(s, 0, t), sub(s, 2, t), t)
        } else {
            let Some(end) = s.find('}') else {
                return Err(error(ERR_INVALID_CHAR_RANGE, s));
            };
            (sub(s, 0, end + 1), sub(s, 3, end), end + 1)
        };
        if let Some(n) = name.strip_prefix('^') {
            negated = !negated;
            name = n;
        }
        let Some((table, inverted)) = unicode_table(name) else {
            return Err(error(ERR_INVALID_CHAR_RANGE, seq));
        };
        Ok(Some((
            Item::Table {
                table,
                negated: negated != inverted,
            },
            at + rest,
        )))
    }

    /// parsePerlClassEscape: `\d` and the like at byte `at`.
    fn parse_perl_class_escape(&self, at: usize) -> Option<(Item, usize)> {
        if self.flags & PERL_X == 0 || byte(self.whole, at) != Some(b'\\') {
            return None;
        }
        let (ranges, negated) = perl_group(byte(self.whole, at + 1)?)?;
        Some((Item::Group { ranges, negated }, at + 2))
    }

    fn class_node(&mut self, items: Vec<Item>, negated: bool) -> Node {
        let mut re = self.new_node(Op::CharClass);
        re.class = Some(Class {
            items,
            negated,
            fold: self.flags & FOLD_CASE != 0,
        });
        re
    }

    /// parseClass: a `[...]` at byte `at`.
    fn parse_class(&mut self, at: usize) -> Result<usize, Error> {
        let s = self.whole;
        let whole_class = sub(s, at, s.len());
        let mut t = at + 1;
        let mut negated = false;
        if byte(s, t) == Some(b'^') {
            negated = true;
            t += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        while t >= s.len() || byte(s, t) != Some(b']') || first {
            first = false;
            // POSIX [:alnum:] and the like.
            if s.len() > t + 2 && byte(s, t) == Some(b'[') && byte(s, t + 1) == Some(b':') {
                let rest = sub(s, t + 2, s.len());
                if let Some(i) = rest.find(":]") {
                    let name = sub(s, t, t + 2 + i + 2);
                    let Some((ranges, negated)) = posix_group(name) else {
                        return Err(error(ERR_INVALID_CHAR_RANGE, name));
                    };
                    items.push(Item::Group { ranges, negated });
                    t += i + 4;
                    continue;
                }
            }
            if let Some((item, next)) = self.parse_unicode_class(t)? {
                items.push(item);
                t = next;
                continue;
            }
            if let Some((item, next)) = self.parse_perl_class_escape(t) {
                items.push(item);
                t = next;
                continue;
            }
            // A single character or a range.
            let rng = t;
            let (lo, next) = self.parse_class_char(t, whole_class)?;
            t = next;
            let mut hi = lo;
            if byte(s, t) == Some(b'-') && byte(s, t + 1).is_some_and(|c| c != b']') {
                t += 1;
                let (h, next) = self.parse_class_char(t, whole_class)?;
                t = next;
                hi = h;
                if hi < lo {
                    return Err(error(ERR_INVALID_CHAR_RANGE, sub(s, rng, t)));
                }
            }
            items.push(Item::Range(lo, hi));
        }
        let re = self.class_node(items, negated);
        self.push(re)?;
        Ok(t + 1)
    }

    fn parse_class_char(&self, at: usize, whole_class: &str) -> Result<(u32, usize), Error> {
        if at >= self.whole.len() {
            return Err(error(ERR_MISSING_BRACKET, whole_class));
        }
        if byte(self.whole, at) == Some(b'\\') {
            return self.parse_escape(at);
        }
        Ok(next_rune(self.whole, at))
    }

    /// parseRepeat: `{min}`, `{min,}` or `{min,max}` at byte `at`. `None` if it is not
    /// one; min -1 if a number is too big.
    fn parse_repeat(&self, at: usize) -> Option<(i32, i32, usize)> {
        let s = self.whole;
        let mut t = at + 1;
        let (min, next) = parse_int(s, t)?;
        t = next;
        let mut min = min;
        let max;
        match byte(s, t)? {
            b',' => {
                t += 1;
                match byte(s, t)? {
                    b'}' => max = -1,
                    _ => {
                        let (m, next) = parse_int(s, t)?;
                        t = next;
                        max = m;
                        if max < 0 {
                            min = -1;
                        }
                    }
                }
            }
            _ => max = min,
        }
        if byte(s, t)? != b'}' {
            return None;
        }
        Some((min, max, t + 1))
    }
}

/// parseInt: decimal digits without a leading zero; -1 when too big.
fn parse_int(s: &str, at: usize) -> Option<(i32, usize)> {
    let b = s.as_bytes();
    let first = *b.get(at)?;
    if !first.is_ascii_digit() {
        return None;
    }
    if first == b'0' && b.get(at + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut end = at;
    while b.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let mut n: i32 = 0;
    for &d in b.get(at..end).unwrap_or(&[]) {
        if n >= 100_000_000 {
            n = -1;
            break;
        }
        n = n * 10 + i32::from(d - b'0');
    }
    Some((n, end))
}

/// repeatIsValid: no more than n copies of the innermost thing.
fn repeat_is_valid(re: &Node, mut n: i32) -> bool {
    if re.op == Op::Repeat {
        let mut m = re.max;
        if m == 0 {
            return true;
        }
        if m < 0 {
            m = re.min;
        }
        if m > n {
            return false;
        }
        if m > 0 {
            n /= m;
        }
    }
    re.subs.iter().all(|s| repeat_is_valid(s, n))
}

/// A parsed regexp: its tree, and its capture groups' names.
#[derive(Debug)]
pub struct Parsed {
    root: Node,
    /// Each group's name, the whole match's (index 0) and unnamed ones empty.
    pub names: Vec<String>,
}

/// syntax.Parse(s, syntax.Perl).
pub fn parse(s: &str) -> Result<Parsed, Error> {
    let mut p = Parser {
        flags: PERL,
        stack: Vec::new(),
        num_cap: 0,
        whole: s,
        num_regexp: 0,
        repeats: 0,
        track_size: false,
    };
    let mut t = 0;
    let mut last_repeat: Option<usize> = None;
    while t < s.len() {
        let mut repeat = None;
        let Some(c) = byte(s, t) else { break };
        match c {
            b'(' => {
                if p.flags & PERL_X != 0 && byte(s, t + 1) == Some(b'?') {
                    t = p.parse_perl_flags(t)?;
                } else {
                    p.num_cap += 1;
                    let mut re = p.new_node(Op::LeftParen);
                    re.cap = p.num_cap;
                    p.push(re)?;
                    t += 1;
                }
            }
            b'|' => {
                p.parse_vertical_bar()?;
                t += 1;
            }
            b')' => {
                p.parse_right_paren()?;
                t += 1;
            }
            b'^' => {
                p.op(if p.flags & ONE_LINE != 0 {
                    Op::BeginText
                } else {
                    Op::BeginLine
                })?;
                t += 1;
            }
            b'$' => {
                p.op(if p.flags & ONE_LINE != 0 {
                    Op::EndText
                } else {
                    Op::EndLine
                })?;
                t += 1;
            }
            b'.' => {
                p.op(if p.flags & DOT_NL != 0 {
                    Op::AnyChar
                } else {
                    Op::AnyCharNotNL
                })?;
                t += 1;
            }
            b'[' => t = p.parse_class(t)?,
            b'*' | b'+' | b'?' => {
                let op = match c {
                    b'*' => Op::Star,
                    b'+' => Op::Plus,
                    _ => Op::Quest,
                };
                let after = p.repeat(op, 0, 0, t, t + 1, last_repeat)?;
                repeat = Some(t);
                t = after;
            }
            b'{' => match p.parse_repeat(t) {
                None => {
                    p.literal(0x7b)?;
                    t += 1;
                }
                Some((min, max, after)) => {
                    if min < 0 || min > 1000 || max > 1000 || (max >= 0 && min > max) {
                        return Err(error(ERR_INVALID_REPEAT_SIZE, sub(s, t, after)));
                    }
                    let after = p.repeat(Op::Repeat, min, max, t, after, last_repeat)?;
                    repeat = Some(t);
                    t = after;
                }
            },
            b'\\' => t = parse_backslash(&mut p, t)?,
            _ => {
                let (r, next) = next_rune(s, t);
                p.literal(r)?;
                t = next;
            }
        }
        last_repeat = repeat;
    }
    p.concat()?;
    if p.swap_vertical_bar() {
        p.stack.pop();
    }
    p.alternate()?;
    if p.stack.len() != 1 {
        return Err(error(ERR_MISSING_PAREN, s));
    }
    let Some(root) = p.stack.pop() else {
        return Err(error(ERR_MISSING_PAREN, s));
    };
    let mut names = vec![String::new(); p.num_cap + 1];
    collect_names(&root, &mut names);
    Ok(Parsed { root, names })
}

fn parse_backslash(p: &mut Parser<'_>, t: usize) -> Result<usize, Error> {
    let s = p.whole;
    if p.flags & PERL_X != 0
        && let Some(c) = byte(s, t + 1)
    {
        match c {
            b'A' => {
                p.op(Op::BeginText)?;
                return Ok(t + 2);
            }
            b'b' => {
                p.op(Op::WordBoundary)?;
                return Ok(t + 2);
            }
            b'B' => {
                p.op(Op::NoWordBoundary)?;
                return Ok(t + 2);
            }
            b'C' => return Err(error(ERR_INVALID_ESCAPE, sub(s, t, t + 2))),
            b'Q' => {
                let rest = sub(s, t + 2, s.len());
                let (lit, next) = match rest.find("\\E") {
                    Some(i) => (sub(rest, 0, i), t + 2 + i + 2),
                    None => (rest, s.len()),
                };
                for c in lit.chars() {
                    p.literal(u32::from(c))?;
                }
                return Ok(next);
            }
            b'z' => {
                p.op(Op::EndText)?;
                return Ok(t + 2);
            }
            _ => {}
        }
    }
    if matches!(byte(s, t + 1), Some(b'p' | b'P'))
        && let Some((item, next)) = p.parse_unicode_class(t)?
    {
        let re = p.class_node(vec![item], false);
        p.push(re)?;
        return Ok(next);
    }
    if let Some((item, next)) = p.parse_perl_class_escape(t) {
        let re = p.class_node(vec![item], false);
        p.push(re)?;
        return Ok(next);
    }
    let (r, next) = p.parse_escape(t)?;
    p.literal(r)?;
    Ok(next)
}

fn collect_names(re: &Node, names: &mut [String]) {
    if re.op == Op::Capture
        && let Some(slot) = names.get_mut(re.cap)
    {
        slot.clone_from(&re.name);
    }
    for s in &re.subs {
        collect_names(s, names);
    }
}

// Writing the tree as a pattern of the `regex` crate.

/// A class that matches nothing, and one that matches every character.
const NOTHING: &str = r"[^\x{0}-\x{10FFFF}]";
const EVERYTHING: &str = r"[\x{0}-\x{10FFFF}]";

fn is_surrogate(r: u32) -> bool {
    (0xD800..=0xDFFF).contains(&r)
}

fn push_rune(out: &mut String, r: u32) {
    if r < 0x80 && is_alnum(r) {
        if let Some(c) = char::from_u32(r) {
            out.push(c);
        }
    } else {
        let _ = write!(out, "\\x{{{r:X}}}");
    }
}

/// A range, without the surrogates no string holds; empty if nothing is left.
fn push_range(out: &mut String, lo: u32, hi: u32) {
    let mut part = |lo: u32, hi: u32| {
        if lo <= hi {
            push_rune(out, lo);
            out.push('-');
            push_rune(out, hi);
        }
    };
    if hi < 0xD800 || lo > 0xDFFF {
        part(lo, hi);
    } else {
        if lo < 0xD800 {
            part(lo, 0xD7FF);
        }
        if hi > 0xDFFF {
            part(0xE000, hi);
        }
    }
}

fn push_table(out: &mut String, table: Table, negated: bool) {
    let p = if negated { 'P' } else { 'p' };
    match table {
        // The `regex` crate has no surrogates; no string holds one.
        Table::Category("Cs") => out.push_str(if negated { EVERYTHING } else { NOTHING }),
        Table::Category(c) => {
            let _ = write!(out, "\\{p}{{gc={c}}}");
        }
        Table::Script(s) => {
            let _ = write!(out, "\\{p}{{sc={s}}}");
        }
        Table::Any => out.push_str(if negated { NOTHING } else { EVERYTHING }),
        Table::Ascii => out.push_str(if negated {
            r"[^\x{0}-\x{7F}]"
        } else {
            r"[\x{0}-\x{7F}]"
        }),
    }
}

fn push_item(out: &mut String, item: &Item) {
    match item {
        Item::Range(lo, hi) => push_range(out, *lo, *hi),
        Item::Group { ranges, negated } => {
            out.push('[');
            if *negated {
                out.push('^');
            }
            for (lo, hi) in *ranges {
                push_range(out, *lo, *hi);
            }
            out.push(']');
        }
        Item::Table { table, negated } => push_table(out, *table, *negated),
    }
}

/// A class's items between brackets; `None` when nothing is in them.
fn bracket(items: &[&Item], negated: bool) -> Option<String> {
    let mut body = String::new();
    for item in items {
        push_item(&mut body, item);
    }
    if body.is_empty() {
        return None;
    }
    Some(format!("[{}{body}]", if negated { "^" } else { "" }))
}

/// The class Go's case folding makes of LC (Cased_Letter) leaves out U+0345, which
/// folds to letters in it: Go has no fold table for LC. These are that orbit.
const LC_ORBIT: [u32; 4] = [0x345, 0x399, 0x3B9, 0x1FBE];

fn push_class(out: &mut String, class: &Class) {
    let all: Vec<&Item> = class.items.iter().collect();
    let plain = match bracket(&all, class.negated) {
        Some(b) => b,
        None => (if class.negated { EVERYTHING } else { NOTHING }).to_string(),
    };
    let lc = |i: &&Item| {
        matches!(
            i,
            Item::Table {
                table: Table::Category("LC"),
                ..
            }
        )
    };
    if !class.fold {
        out.push_str(&plain);
        return;
    }
    if !class.items.iter().any(|i| lc(&i)) {
        let _ = write!(out, "(?i:{plain})");
        return;
    }
    // Go folds every item but LC; the `regex` crate folds the whole class. They
    // differ only on LC's orbit, which is written out as Go has it.
    let folded: Vec<&Item> = class.items.iter().filter(|i| !lc(i)).collect();
    let unfolded: Vec<&Item> = class.items.iter().filter(lc).collect();
    let test = |items: &[&Item], fold: bool| -> Option<regex::Regex> {
        let b = bracket(items, false)?;
        regex::Regex::new(&if fold { format!("(?i:{b})") } else { b }).ok()
    };
    let (f, u) = (test(&folded, true), test(&unfolded, false));
    let mut members = String::new();
    for r in LC_ORBIT {
        let Some(c) = char::from_u32(r) else { continue };
        let mut buf = [0u8; 4];
        let cs = c.encode_utf8(&mut buf);
        let inside =
            f.as_ref().is_some_and(|re| re.is_match(cs)) || u.as_ref().is_some_and(|re| re.is_match(cs));
        if inside != class.negated {
            push_rune(&mut members, r);
        }
    }
    let _ = write!(out, "(?:(?i:[{plain}&&[^\\x{{345}}]])");
    if !members.is_empty() {
        let _ = write!(out, "|[{members}]");
    }
    out.push(')');
}

fn emit(out: &mut String, re: &Node) {
    match re.op {
        Op::NoMatch => out.push_str(NOTHING),
        Op::EmptyMatch => out.push_str("(?:)"),
        Op::Literal => {
            if re.runes.iter().any(|&r| is_surrogate(r)) {
                out.push_str(NOTHING);
                return;
            }
            let fold = re.flags & FOLD_CASE != 0;
            out.push_str(if fold { "(?i:" } else { "(?:" });
            for &r in &re.runes {
                push_rune(out, r);
            }
            out.push(')');
        }
        Op::CharClass => {
            if let Some(c) = &re.class {
                push_class(out, c);
            }
        }
        Op::AnyCharNotNL => out.push_str(r"[^\n]"),
        Op::AnyChar => out.push_str("(?s:.)"),
        Op::BeginLine => out.push_str("(?m:^)"),
        Op::EndLine => out.push_str("(?m:$)"),
        Op::BeginText => out.push_str(r"\A"),
        Op::EndText => out.push_str(r"\z"),
        Op::WordBoundary => out.push_str(r"(?-u:\b)"),
        Op::NoWordBoundary => out.push_str(r"(?-u:\B)"),
        Op::Capture => {
            out.push('(');
            for s in &re.subs {
                emit(out, s);
            }
            out.push(')');
        }
        Op::Star | Op::Plus | Op::Quest | Op::Repeat => {
            out.push_str("(?:");
            for s in &re.subs {
                emit(out, s);
            }
            out.push(')');
            match re.op {
                Op::Star => out.push('*'),
                Op::Plus => out.push('+'),
                Op::Quest => out.push('?'),
                _ => {
                    if re.max == -1 {
                        let _ = write!(out, "{{{},}}", re.min);
                    } else if re.max == re.min {
                        let _ = write!(out, "{{{}}}", re.min);
                    } else {
                        let _ = write!(out, "{{{},{}}}", re.min, re.max);
                    }
                }
            }
            if re.flags & NON_GREEDY != 0 {
                out.push('?');
            }
        }
        Op::Concat => {
            for s in &re.subs {
                emit(out, s);
            }
        }
        Op::Alternate => {
            out.push_str("(?:");
            for (i, s) in re.subs.iter().enumerate() {
                if i > 0 {
                    out.push('|');
                }
                emit(out, s);
            }
            out.push(')');
        }
        Op::LeftParen | Op::VerticalBar => {}
    }
}

impl Parsed {
    /// The pattern for the `regex` crate.
    pub fn pattern(&self) -> String {
        let mut out = String::new();
        emit(&mut out, &self.root);
        out
    }
}
