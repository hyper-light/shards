//! OPA's scanner (ast/internal/scanner/scanner.go and tokens/tokens.go, v1.14.1).

use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Illegal,
    Eof,
    Whitespace,
    Ident,
    Comment,
    Package,
    Import,
    As,
    Default,
    Else,
    Not,
    Some,
    With,
    Null,
    True,
    False,
    Number,
    String,
    TemplateStringPart,
    TemplateStringEnd,
    RawTemplateStringPart,
    RawTemplateStringEnd,
    LBrack,
    RBrack,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Colon,
    Add,
    Sub,
    Mul,
    Quo,
    Rem,
    And,
    Or,
    Unify,
    Equal,
    Assign,
    In,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    Dot,
    Semicolon,
    Dollar,
    Every,
    Contains,
    If,
}

impl Token {
    /// tokens.Token.String().
    pub fn name(self) -> &'static str {
        match self {
            Token::Illegal => "illegal",
            Token::Eof => "eof",
            Token::Whitespace => "whitespace",
            Token::Comment => "comment",
            Token::Ident => "identifier",
            Token::Package => "package",
            Token::Import => "import",
            Token::As => "as",
            Token::Default => "default",
            Token::Else => "else",
            Token::Not => "not",
            Token::Some => "some",
            Token::With => "with",
            Token::Null => "null",
            Token::True => "true",
            Token::False => "false",
            Token::Number => "number",
            Token::String => "string",
            Token::TemplateStringPart => "template-string-part",
            Token::TemplateStringEnd => "template-string-end",
            Token::RawTemplateStringPart => "raw-template-string-part",
            Token::RawTemplateStringEnd => "raw-template-string-end",
            Token::LBrack => "[",
            Token::RBrack => "]",
            Token::LBrace => "{",
            Token::RBrace => "}",
            Token::LParen => "(",
            Token::RParen => ")",
            Token::Comma => ",",
            Token::Colon => ":",
            Token::Add => "plus",
            Token::Sub => "minus",
            Token::Mul => "mul",
            Token::Quo => "div",
            Token::Rem => "rem",
            Token::And => "and",
            Token::Or => "or",
            Token::Unify => "eq",
            Token::Equal => "equal",
            Token::Assign => "assign",
            Token::In => "in",
            Token::Neq => "neq",
            Token::Gt => "gt",
            Token::Lt => "lt",
            Token::Gte => "gte",
            Token::Lte => "lte",
            Token::Dot => ".",
            Token::Semicolon => ";",
            Token::Dollar => "dollar",
            Token::Every => "every",
            Token::Contains => "contains",
            Token::If => "if",
        }
    }

    /// tokens.IsKeyword: one of the base keywords.
    pub fn is_keyword(self) -> bool {
        matches!(
            self,
            Token::Package
                | Token::Import
                | Token::As
                | Token::Default
                | Token::Else
                | Token::Not
                | Token::Some
                | Token::With
                | Token::Null
                | Token::True
                | Token::False
        )
    }
}

/// The base keywords (tokens.Keywords()).
pub fn base_keywords() -> HashMap<&'static str, Token> {
    HashMap::from([
        ("package", Token::Package),
        ("import", Token::Import),
        ("as", Token::As),
        ("default", Token::Default),
        ("else", Token::Else),
        ("not", Token::Not),
        ("some", Token::Some),
        ("with", Token::With),
        ("null", Token::Null),
        ("true", Token::True),
        ("false", Token::False),
    ])
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Position {
    pub offset: usize,
    pub end: usize,
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub message: &'static str,
    pub pos: Position,
}

/// Go's -1 for the end of input.
const EOF: i32 = -1;
const BOM: i32 = 0xFEFF;

#[derive(Debug, Clone)]
pub struct Scanner<'a> {
    /// Shared by the parser's saved states until one changes it.
    pub keywords: Rc<HashMap<&'static str, Token>>,
    pub bs: &'a [u8],
    errors: Vec<ScanError>,
    offset: usize,
    row: usize,
    col: usize,
    width: usize,
    curr: i32,
}

impl<'a> Scanner<'a> {
    pub fn new(bs: &'a [u8]) -> Scanner<'a> {
        let mut s = Scanner {
            keywords: Rc::new(base_keywords()),
            bs,
            errors: Vec::new(),
            offset: 0,
            row: 1,
            col: 0,
            width: 0,
            curr: EOF,
        };
        s.next();
        if s.curr == BOM {
            s.next();
        }
        s
    }

    pub fn keyword(&self, lit: &str) -> Token {
        self.keywords.get(lit).copied().unwrap_or(Token::Ident)
    }

    pub fn add_keyword(&mut self, kw: &'static str, tok: Token) {
        let keywords = Rc::make_mut(&mut self.keywords);
        keywords.insert(kw, tok);
        if tok == Token::Every {
            keywords.insert("in", Token::In);
        }
    }

    pub fn is_keyword(&self, s: &str) -> bool {
        self.keywords.contains_key(s)
    }

    fn slice(&self, from: usize, to: usize) -> String {
        String::from_utf8_lossy(self.bs.get(from..to).unwrap_or_default()).into_owned()
    }

    /// The next token: its kind, where it is, its text, and what was wrong with it.
    /// `template` continues a template string after its `}` (raw when true).
    pub fn scan(&mut self, template: Option<bool>) -> (Token, Position, String, Vec<ScanError>) {
        let mut pos = Position {
            offset: self.offset.saturating_sub(self.width),
            row: self.row,
            col: self.col,
            end: 0,
        };
        let mut lit = String::new();
        let tok;
        if let Some(raw) = template {
            (lit, tok) = if raw {
                self.scan_raw_template_string()
            } else {
                self.scan_template_string()
            };
        } else if self.is_whitespace() {
            lit = char::from_u32(u32::try_from(self.curr).unwrap_or(0))
                .map(String::from)
                .unwrap_or_default();
            self.next();
            tok = Token::Whitespace;
        } else if is_letter(self.curr) {
            lit = self.scan_identifier();
            tok = self.keyword(&lit);
        } else if is_decimal(self.curr) {
            lit = self.scan_number();
            tok = Token::Number;
        } else {
            let ch = self.curr;
            self.next();
            let c = u32::try_from(ch).ok().and_then(char::from_u32);
            tok = match c {
                None => Token::Eof,
                Some('#') => {
                    lit = self.scan_comment();
                    Token::Comment
                }
                Some('"') => {
                    lit = self.scan_string();
                    Token::String
                }
                Some('`') => {
                    lit = self.scan_raw_string();
                    Token::String
                }
                Some('[') => Token::LBrack,
                Some(']') => Token::RBrack,
                Some('{') => Token::LBrace,
                Some('}') => Token::RBrace,
                Some('(') => Token::LParen,
                Some(')') => Token::RParen,
                Some(',') => Token::Comma,
                Some(':') => {
                    if self.curr == i32::from(b'=') {
                        self.next();
                        Token::Assign
                    } else {
                        Token::Colon
                    }
                }
                Some('+') => Token::Add,
                Some('-') => Token::Sub,
                Some('*') => Token::Mul,
                Some('/') => Token::Quo,
                Some('%') => Token::Rem,
                Some('&') => Token::And,
                Some('|') => Token::Or,
                Some('=') => {
                    if self.curr == i32::from(b'=') {
                        self.next();
                        Token::Equal
                    } else {
                        Token::Unify
                    }
                }
                Some('>') => {
                    if self.curr == i32::from(b'=') {
                        self.next();
                        Token::Gte
                    } else {
                        Token::Gt
                    }
                }
                Some('<') => {
                    if self.curr == i32::from(b'=') {
                        self.next();
                        Token::Lte
                    } else {
                        Token::Lt
                    }
                }
                Some('!') => {
                    if self.curr == i32::from(b'=') {
                        self.next();
                        Token::Neq
                    } else {
                        self.error("illegal ! character");
                        Token::Illegal
                    }
                }
                Some(';') => Token::Semicolon,
                Some('.') => Token::Dot,
                Some('$') => {
                    if self.curr == i32::from(b'`') {
                        self.next();
                        let (l, t) = self.scan_raw_template_string();
                        lit = l;
                        t
                    } else if self.curr == i32::from(b'"') {
                        self.next();
                        let (l, t) = self.scan_template_string();
                        lit = l;
                        t
                    } else {
                        self.error("illegal $ character");
                        Token::Illegal
                    }
                }
                Some(_) => Token::Illegal,
            };
        }
        pos.end = self.offset.saturating_sub(self.width);
        let errs = std::mem::take(&mut self.errors);
        (tok, pos, lit, errs)
    }

    fn scan_identifier(&mut self) -> String {
        let start = self.offset.saturating_sub(1);
        while is_letter(self.curr) || is_digit(self.curr) {
            self.next();
        }
        self.slice(start, self.offset.saturating_sub(1))
    }

    fn scan_number(&mut self) -> String {
        let start = self.offset.saturating_sub(1);
        if self.curr != i32::from(b'.') {
            while is_decimal(self.curr) {
                self.next();
            }
        }
        if self.curr == i32::from(b'.') {
            self.next();
            let mut found = false;
            while is_decimal(self.curr) {
                self.next();
                found = true;
            }
            if !found {
                self.error("expected fraction");
            }
        }
        if lower(self.curr) == i32::from(b'e') {
            self.next();
            if self.curr == i32::from(b'+') || self.curr == i32::from(b'-') {
                self.next();
            }
            let mut found = false;
            while is_decimal(self.curr) {
                self.next();
                found = true;
            }
            if !found {
                self.error("expected exponent");
            }
        }
        if is_letter(self.curr) {
            self.error("illegal number format");
            while is_letter(self.curr) || is_digit(self.curr) {
                self.next();
            }
        }
        self.slice(start, self.offset.saturating_sub(1))
    }

    fn scan_string(&mut self) -> String {
        let start = self.literal_start();
        loop {
            let ch = self.curr;
            if ch == i32::from(b'\n') || ch < 0 {
                self.error("non-terminated string");
                break;
            }
            self.next();
            if ch == i32::from(b'"') {
                break;
            }
            if ch == i32::from(b'\\') {
                match u8::try_from(self.curr).unwrap_or(0) {
                    b'\\' | b'"' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.next(),
                    b'u' => {
                        self.next();
                        self.next();
                        self.next();
                        self.next();
                    }
                    _ => self.error("illegal escape sequence"),
                }
            }
        }
        self.slice(start, self.offset.saturating_sub(1))
    }

    fn scan_raw_string(&mut self) -> String {
        let start = self.literal_start();
        loop {
            let ch = self.curr;
            self.next();
            if ch == i32::from(b'`') {
                break;
            } else if ch < 0 {
                self.error("non-terminated string");
                break;
            }
        }
        self.slice(start, self.offset.saturating_sub(1))
    }

    fn scan_template_string(&mut self) -> (String, Token) {
        let mut tok = Token::TemplateStringPart;
        let start = self.literal_start();
        let mut escapes = Vec::new();
        loop {
            let ch = self.curr;
            if ch == i32::from(b'\n') || ch < 0 {
                self.error("non-terminated string");
                break;
            }
            self.next();
            if ch == i32::from(b'"') {
                tok = Token::TemplateStringEnd;
                break;
            }
            if ch == i32::from(b'{') {
                break;
            }
            if ch == i32::from(b'\\') {
                match u8::try_from(self.curr).unwrap_or(0) {
                    b'\\' | b'"' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.next(),
                    b'{' => {
                        escapes.push(self.offset.saturating_sub(1));
                        self.next();
                    }
                    b'u' => {
                        self.next();
                        self.next();
                        self.next();
                        self.next();
                    }
                    _ => self.error("illegal escape sequence"),
                }
            }
        }
        (self.literal(start, &escapes), tok)
    }

    fn scan_raw_template_string(&mut self) -> (String, Token) {
        let mut tok = Token::RawTemplateStringPart;
        let start = self.literal_start();
        let mut escapes = Vec::new();
        loop {
            let ch = self.curr;
            if ch < 0 {
                self.error("non-terminated string");
                break;
            }
            self.next();
            if ch == i32::from(b'`') {
                tok = Token::RawTemplateStringEnd;
                break;
            }
            if ch == i32::from(b'{') {
                break;
            }
            if ch == i32::from(b'\\') && self.curr == i32::from(b'{') {
                escapes.push(self.offset.saturating_sub(1));
                self.next();
            }
        }
        (self.literal(start, &escapes), tok)
    }

    /// The literal from `start`, each escape's backslash removed (removeEscapes).
    fn literal(&self, start: usize, escapes: &[usize]) -> String {
        let end = self.offset.saturating_sub(1);
        if escapes.is_empty() {
            return self.slice(start, end);
        }
        let mut bs = Vec::new();
        let mut from = start;
        for &escape in escapes {
            if escape > from {
                bs.extend_from_slice(self.bs.get(from..escape.saturating_sub(1)).unwrap_or_default());
            }
            from = escape;
        }
        if from < end {
            bs.extend_from_slice(self.bs.get(from..end).unwrap_or_default());
        }
        String::from_utf8_lossy(&bs).into_owned()
    }

    fn scan_comment(&mut self) -> String {
        let start = self.literal_start();
        while self.curr != i32::from(b'\n') && self.curr != EOF {
            self.next();
        }
        let mut end = self.offset.saturating_sub(1);
        if self.offset > 1 && self.bs.get(self.offset - 2) == Some(&b'\r') {
            end = end.saturating_sub(1);
        }
        self.slice(start, end)
    }

    fn next(&mut self) {
        if self.offset >= self.bs.len() {
            self.curr = EOF;
            self.offset = self.bs.len() + 1;
            return;
        }
        let b = self.bs.get(self.offset).copied().unwrap_or(0);
        self.curr = i32::from(b);
        self.width = 1;
        if b == 0 {
            self.error("illegal null character");
        } else if b >= 0x80 {
            let rest = self.bs.get(self.offset..).unwrap_or_default();
            let (c, w) = decode_rune(rest);
            self.curr = c;
            self.width = w;
            if c == 0xFFFD && w == 1 {
                self.error("illegal utf-8 character");
            } else if c == BOM && self.offset > 0 {
                self.error("illegal byte-order mark");
            }
        }
        self.offset += self.width;
        if self.curr == i32::from(b'\n') {
            self.row += 1;
            self.col = 0;
        } else {
            self.col += 1;
        }
    }

    fn literal_start(&self) -> usize {
        self.offset.saturating_sub(self.width + 1)
    }

    fn is_whitespace(&self) -> bool {
        b" \t\n\r".iter().any(|&c| self.curr == i32::from(c))
    }

    fn error(&mut self, message: &'static str) {
        self.errors.push(ScanError {
            message,
            pos: Position {
                offset: self.offset,
                row: self.row,
                col: self.col,
                end: 0,
            },
        });
    }
}

/// utf8.DecodeRune: the rune and its width, (RuneError, 1) when invalid.
fn decode_rune(b: &[u8]) -> (i32, usize) {
    let first = b.first().copied().unwrap_or(0);
    let len = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (0xFFFD, 1),
    };
    match b.get(..len).map(std::str::from_utf8) {
        Some(Ok(s)) => (
            s.chars()
                .next()
                .map_or(0xFFFD, |c| i32::try_from(u32::from(c)).unwrap_or(0xFFFD)),
            len,
        ),
        _ => (0xFFFD, 1),
    }
}

/// Go's `lower`: ch | 0x20, a lower-case ASCII letter iff ch is one.
fn lower(ch: i32) -> i32 {
    0x20 | ch
}

pub fn is_letter(ch: i32) -> bool {
    (i32::from(b'a') <= lower(ch) && lower(ch) <= i32::from(b'z')) || ch == i32::from(b'_')
}

pub fn is_decimal(ch: i32) -> bool {
    (i32::from(b'0')..=i32::from(b'9')).contains(&ch)
}

/// isDigit: an ASCII digit, or a Unicode decimal digit (Go's `unicode.IsDigit`, Nd).
pub fn is_digit(ch: i32) -> bool {
    is_decimal(ch) || (ch >= 0x80 && is_nd(ch))
}

/// Go 1.26's `unicode._Nd` (Unicode 15.0.0): ranges of ten digits, but for 0x1d7ce.
const ND: [(i32, i32); 64] = [
    (0x0030, 0x0039),
    (0x0660, 0x0669),
    (0x06f0, 0x06f9),
    (0x07c0, 0x07c9),
    (0x0966, 0x096f),
    (0x09e6, 0x09ef),
    (0x0a66, 0x0a6f),
    (0x0ae6, 0x0aef),
    (0x0b66, 0x0b6f),
    (0x0be6, 0x0bef),
    (0x0c66, 0x0c6f),
    (0x0ce6, 0x0cef),
    (0x0d66, 0x0d6f),
    (0x0de6, 0x0def),
    (0x0e50, 0x0e59),
    (0x0ed0, 0x0ed9),
    (0x0f20, 0x0f29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x17e0, 0x17e9),
    (0x1810, 0x1819),
    (0x1946, 0x194f),
    (0x19d0, 0x19d9),
    (0x1a80, 0x1a89),
    (0x1a90, 0x1a99),
    (0x1b50, 0x1b59),
    (0x1bb0, 0x1bb9),
    (0x1c40, 0x1c49),
    (0x1c50, 0x1c59),
    (0xa620, 0xa629),
    (0xa8d0, 0xa8d9),
    (0xa900, 0xa909),
    (0xa9d0, 0xa9d9),
    (0xa9f0, 0xa9f9),
    (0xaa50, 0xaa59),
    (0xabf0, 0xabf9),
    (0xff10, 0xff19),
    (0x104a0, 0x104a9),
    (0x10d30, 0x10d39),
    (0x11066, 0x1106f),
    (0x110f0, 0x110f9),
    (0x11136, 0x1113f),
    (0x111d0, 0x111d9),
    (0x112f0, 0x112f9),
    (0x11450, 0x11459),
    (0x114d0, 0x114d9),
    (0x11650, 0x11659),
    (0x116c0, 0x116c9),
    (0x11730, 0x11739),
    (0x118e0, 0x118e9),
    (0x11950, 0x11959),
    (0x11c50, 0x11c59),
    (0x11d50, 0x11d59),
    (0x11da0, 0x11da9),
    (0x11f50, 0x11f59),
    (0x16a60, 0x16a69),
    (0x16ac0, 0x16ac9),
    (0x16b50, 0x16b59),
    (0x1d7ce, 0x1d7ff),
    (0x1e140, 0x1e149),
    (0x1e2f0, 0x1e2f9),
    (0x1e4f0, 0x1e4f9),
    (0x1e950, 0x1e959),
    (0x1fbf0, 0x1fbf9),
];

fn is_nd(ch: i32) -> bool {
    ND.iter().any(|&(lo, hi)| lo <= ch && ch <= hi)
}
