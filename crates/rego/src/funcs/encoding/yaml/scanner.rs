//! go-yaml v2.4.2's reader and scanner (readerc.go, scannerc.go), libyaml's as Go
//! ported it: input decoded in the same 512-byte windows (so a bad character is found
//! when go-yaml finds it), tokens queued and simple keys tracked as there, and every
//! error with go-yaml's problem and marks.

use std::collections::HashMap;

/// A position: characters read, its line and column, all from 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mark {
    pub index: i64,
    pub line: i64,
    pub column: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Reader,
    Scanner,
    Parser,
}

/// A reader, scanner or parser error, as go-yaml's parser keeps it.
#[derive(Debug, Clone)]
pub struct YamlError {
    pub kind: ErrorKind,
    pub problem: String,
    pub problem_mark: Mark,
    pub context_mark: Mark,
}

impl YamlError {
    /// `parser.fail()`'s text, without the `yaml: ` prefix.
    pub fn text(&self) -> String {
        let mut line = 0;
        if self.problem_mark.line != 0 {
            line = self.problem_mark.line;
            if self.kind == ErrorKind::Scanner {
                line += 1;
            }
        } else if self.context_mark.line != 0 {
            line = self.context_mark.line;
        }
        let msg = if self.problem.is_empty() {
            "unknown problem parsing YAML content"
        } else {
            &self.problem
        };
        if line != 0 { format!("line {line}: {msg}") } else { msg.to_string() }
    }
}

pub type Result<T> = std::result::Result<T, YamlError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    NoToken,
    StreamStart,
    StreamEnd,
    VersionDirective,
    TagDirective,
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowSequenceEnd,
    FlowMappingStart,
    FlowMappingEnd,
    BlockEntry,
    FlowEntry,
    Key,
    Value,
    Alias,
    Anchor,
    Tag,
    Scalar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarStyle {
    Any,
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub typ: TokenType,
    pub start_mark: Mark,
    pub end_mark: Mark,
    /// Alias and anchor names, scalar values, tag handles.
    pub value: Vec<u8>,
    /// Tag suffixes.
    pub suffix: Vec<u8>,
    /// %TAG prefixes.
    pub prefix: Vec<u8>,
    pub style: ScalarStyle,
    pub major: i8,
    pub minor: i8,
}

impl Token {
    fn new(typ: TokenType, start_mark: Mark, end_mark: Mark) -> Token {
        Token {
            typ,
            start_mark,
            end_mark,
            value: Vec::new(),
            suffix: Vec::new(),
            prefix: Vec::new(),
            style: ScalarStyle::Any,
            major: 0,
            minor: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SimpleKey {
    possible: bool,
    required: bool,
    token_number: usize,
    mark: Mark,
}

const INPUT_RAW_BUFFER_SIZE: usize = 512;
const MAX_FLOW_LEVEL: usize = 10000;
const MAX_INDENTS: usize = 10000;
const MAX_NUMBER_LENGTH: i8 = 2;

// Character classes (yamlprivateh.go), reading past the end as NUL.

pub fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

pub fn is_alpha(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn is_digit(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_digit()
}

fn as_digit(b: &[u8], i: usize) -> i8 {
    i8::try_from(at(b, i).wrapping_sub(b'0')).unwrap_or(0)
}

fn is_hex(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_hexdigit()
}

fn as_hex(b: &[u8], i: usize) -> u32 {
    let c = at(b, i);
    match c {
        b'A'..=b'F' => u32::from(c - b'A') + 10,
        b'a'..=b'f' => u32::from(c - b'a') + 10,
        _ => u32::from(c.wrapping_sub(b'0')),
    }
}

pub fn is_ascii(b: &[u8], i: usize) -> bool {
    at(b, i) <= 0x7F
}

pub fn is_printable(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    let c1 = at(b, i + 1);
    let c2 = at(b, i + 2);
    c == 0x0A
        || (0x20..=0x7E).contains(&c)
        || (c == 0xC2 && c1 >= 0xA0)
        || (c > 0xC2 && c < 0xED)
        || (c == 0xED && c1 < 0xA0)
        || c == 0xEE
        || (c == 0xEF && !(c1 == 0xBB && c2 == 0xBF) && !(c1 == 0xBF && (c2 == 0xBE || c2 == 0xBF)))
}

pub fn is_z(b: &[u8], i: usize) -> bool {
    at(b, i) == 0
}

/// go-yaml's is_bom, which looks at the start of `b` whatever `i` is.
pub fn is_bom(b: &[u8], _i: usize) -> bool {
    at(b, 0) == 0xEF && at(b, 1) == 0xBB && at(b, 2) == 0xBF
}

pub fn is_space(b: &[u8], i: usize) -> bool {
    at(b, i) == b' '
}

fn is_tab(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\t'
}

pub fn is_blank(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b' ' || c == b'\t'
}

pub fn is_break(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b'\r'
        || c == b'\n'
        || (c == 0xC2 && at(b, i + 1) == 0x85)
        || (c == 0xE2 && at(b, i + 1) == 0x80 && (at(b, i + 2) == 0xA8 || at(b, i + 2) == 0xA9))
}

fn is_crlf(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\r' && at(b, i + 1) == b'\n'
}

pub fn is_breakz(b: &[u8], i: usize) -> bool {
    is_break(b, i) || at(b, i) == 0
}

pub fn is_blankz(b: &[u8], i: usize) -> bool {
    is_blank(b, i) || is_breakz(b, i)
}

/// The length of the UTF-8 sequence `c` starts, 0 when it starts none.
pub fn width(c: u8) -> usize {
    if c & 0x80 == 0x00 {
        1
    } else if c & 0xE0 == 0xC0 {
        2
    } else if c & 0xF0 == 0xE0 {
        3
    } else if c & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

/// go-yaml's scanner over one input.
#[derive(Debug)]
pub struct Scanner<'a> {
    input: &'a [u8],
    input_pos: usize,
    raw: Vec<u8>,
    raw_pos: usize,
    eof: bool,
    encoding_known: bool,
    buf: Vec<u8>,
    pos: usize,
    unread: usize,
    offset: usize,
    mark: Mark,

    stream_start_produced: bool,
    pub stream_end_produced: bool,
    flow_level: usize,
    tokens: Vec<Token>,
    head: usize,
    pub tokens_parsed: usize,
    pub token_available: bool,
    indent: i64,
    indents: Vec<i64>,
    simple_key_allowed: bool,
    simple_keys: Vec<SimpleKey>,
    simple_keys_by_tok: HashMap<usize, usize>,
}

impl<'a> Scanner<'a> {
    pub fn new(input: &'a [u8]) -> Scanner<'a> {
        Scanner {
            input,
            input_pos: 0,
            raw: Vec::with_capacity(INPUT_RAW_BUFFER_SIZE),
            raw_pos: 0,
            eof: false,
            encoding_known: false,
            buf: Vec::new(),
            pos: 0,
            unread: 0,
            offset: 0,
            mark: Mark::default(),
            stream_start_produced: false,
            stream_end_produced: false,
            flow_level: 0,
            tokens: Vec::new(),
            head: 0,
            tokens_parsed: 0,
            token_available: false,
            indent: 0,
            indents: Vec::new(),
            simple_key_allowed: false,
            simple_keys: Vec::new(),
            simple_keys_by_tok: HashMap::new(),
        }
    }

    // ---- reader (readerc.go) ----

    fn reader_error(&self, problem: &str) -> YamlError {
        YamlError {
            kind: ErrorKind::Reader,
            problem: problem.to_string(),
            problem_mark: Mark::default(),
            context_mark: Mark::default(),
        }
    }

    /// yaml_parser_update_raw_buffer with yaml_string_read_handler.
    fn update_raw_buffer(&mut self) {
        if self.raw_pos == 0 && self.raw.len() == INPUT_RAW_BUFFER_SIZE {
            return;
        }
        if self.eof {
            return;
        }
        if self.raw_pos > 0 {
            let keep = self.raw.get(self.raw_pos..).map(<[u8]>::to_vec).unwrap_or_default();
            self.raw = keep;
        }
        self.raw_pos = 0;
        if self.input_pos == self.input.len() {
            self.eof = true;
            return;
        }
        let room = INPUT_RAW_BUFFER_SIZE.saturating_sub(self.raw.len());
        let end = self.input.len().min(self.input_pos + room);
        if let Some(chunk) = self.input.get(self.input_pos..end) {
            self.raw.extend_from_slice(chunk);
        }
        self.input_pos = end;
    }

    fn determine_encoding(&mut self) {
        while !self.eof && self.raw.len() - self.raw_pos < 3 {
            self.update_raw_buffer();
        }
        let r = self.raw.get(self.raw_pos..).unwrap_or_default();
        if r.len() >= 3 && at(r, 0) == 0xEF && at(r, 1) == 0xBB && at(r, 2) == 0xBF {
            self.raw_pos += 3;
            self.offset += 3;
        }
        self.encoding_known = true;
    }

    /// yaml_parser_update_buffer: at least `length` characters decoded and unread.
    fn update_buffer(&mut self, length: usize) -> Result<()> {
        if self.unread >= length {
            return Ok(());
        }
        if !self.encoding_known {
            self.determine_encoding();
        }
        if self.pos > 0 && self.pos < self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        } else if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        let mut first = true;
        while self.unread < length {
            if !first || self.raw_pos == self.raw.len() {
                self.update_raw_buffer();
            }
            first = false;
            while self.raw_pos != self.raw.len() {
                let raw_unread = self.raw.len() - self.raw_pos;
                let octet = at(&self.raw, self.raw_pos);
                let w = width(octet);
                if w == 0 {
                    return Err(self.reader_error("invalid leading UTF-8 octet"));
                }
                if w > raw_unread {
                    if self.eof {
                        return Err(self.reader_error("incomplete UTF-8 octet sequence"));
                    }
                    break;
                }
                let mut value: u32 = match w {
                    1 => u32::from(octet & 0x7F),
                    2 => u32::from(octet & 0x1F),
                    3 => u32::from(octet & 0x0F),
                    _ => u32::from(octet & 0x07),
                };
                for k in 1..w {
                    let o = at(&self.raw, self.raw_pos + k);
                    if o & 0xC0 != 0x80 {
                        return Err(self.reader_error("invalid trailing UTF-8 octet"));
                    }
                    value = (value << 6) + u32::from(o & 0x3F);
                }
                let ok_len = match w {
                    1 => true,
                    2 => value >= 0x80,
                    3 => value >= 0x800,
                    _ => value >= 0x10000,
                };
                if !ok_len {
                    return Err(self.reader_error("invalid length of a UTF-8 sequence"));
                }
                if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
                    return Err(self.reader_error("invalid Unicode character"));
                }
                let allowed = value == 0x09
                    || value == 0x0A
                    || value == 0x0D
                    || (0x20..=0x7E).contains(&value)
                    || value == 0x85
                    || (0xA0..=0xD7FF).contains(&value)
                    || (0xE000..=0xFFFD).contains(&value)
                    || (0x10000..=0x10FFFF).contains(&value);
                if !allowed {
                    return Err(self.reader_error("control characters are not allowed"));
                }
                if let Some(bytes) = self.raw.get(self.raw_pos..self.raw_pos + w) {
                    self.buf.extend_from_slice(bytes);
                }
                self.raw_pos += w;
                self.offset += w;
                self.unread += 1;
            }
            if self.eof {
                self.buf.push(0);
                self.unread += 1;
                break;
            }
        }
        Ok(())
    }

    fn cache(&mut self, length: usize) -> Result<()> {
        if self.unread >= length { Ok(()) } else { self.update_buffer(length) }
    }

    // ---- low-level reading (scannerc.go) ----

    fn b(&self) -> &[u8] {
        self.buf.get(self.pos..).unwrap_or_default()
    }

    fn c(&self, k: usize) -> u8 {
        at(&self.buf, self.pos + k)
    }

    fn skip(&mut self) {
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread = self.unread.saturating_sub(1);
        self.pos += width(self.c(0)).max(1);
    }

    fn skip_line(&mut self) {
        if is_crlf(self.b(), 0) {
            self.mark.index += 2;
            self.mark.column = 0;
            self.mark.line += 1;
            self.unread = self.unread.saturating_sub(2);
            self.pos += 2;
        } else if is_break(self.b(), 0) {
            self.mark.index += 1;
            self.mark.column = 0;
            self.mark.line += 1;
            self.unread = self.unread.saturating_sub(1);
            self.pos += width(self.c(0)).max(1);
        }
    }

    fn read(&mut self, s: &mut Vec<u8>) {
        let w = width(self.c(0)).max(1);
        if let Some(bytes) = self.buf.get(self.pos..self.pos + w) {
            s.extend_from_slice(bytes);
        }
        self.pos += w;
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread = self.unread.saturating_sub(1);
    }

    fn read_line(&mut self, s: &mut Vec<u8>) {
        let (c0, c1, c2) = (self.c(0), self.c(1), self.c(2));
        if c0 == b'\r' && c1 == b'\n' {
            s.push(b'\n');
            self.pos += 2;
            self.mark.index += 1;
            self.unread = self.unread.saturating_sub(1);
        } else if c0 == b'\r' || c0 == b'\n' {
            s.push(b'\n');
            self.pos += 1;
        } else if c0 == 0xC2 && c1 == 0x85 {
            s.push(b'\n');
            self.pos += 2;
        } else if c0 == 0xE2 && c1 == 0x80 && (c2 == 0xA8 || c2 == 0xA9) {
            s.extend_from_slice(&[c0, c1, c2]);
            self.pos += 3;
        } else {
            return;
        }
        self.mark.index += 1;
        self.mark.column = 0;
        self.mark.line += 1;
        self.unread = self.unread.saturating_sub(1);
    }

    fn error(&self, context_mark: Mark, problem: &str) -> YamlError {
        YamlError {
            kind: ErrorKind::Scanner,
            problem: problem.to_string(),
            problem_mark: self.mark,
            context_mark,
        }
    }

    // ---- the token queue ----

    pub fn peek(&mut self) -> Result<&Token> {
        if !self.token_available {
            self.fetch_more_tokens()?;
        }
        let t = self.tokens.get(self.head);
        t.ok_or_else(|| self.error(self.mark, "no token available"))
    }

    pub fn skip_token(&mut self) {
        self.token_available = false;
        self.tokens_parsed += 1;
        self.stream_end_produced = self.tokens.get(self.head).is_some_and(|t| t.typ == TokenType::StreamEnd);
        self.head += 1;
        if self.head == self.tokens.len() {
            self.tokens.clear();
            self.head = 0;
        }
    }

    fn insert_token(&mut self, pos: Option<usize>, token: Token) {
        match pos {
            None => self.tokens.push(token),
            Some(p) => {
                let at = (self.head + p).min(self.tokens.len());
                self.tokens.insert(at, token);
            }
        }
    }

    fn fetch_more_tokens(&mut self) -> Result<()> {
        loop {
            if self.head != self.tokens.len() {
                let Some(&idx) = self.simple_keys_by_tok.get(&self.tokens_parsed) else {
                    break;
                };
                if !self.simple_key_is_valid(idx)? {
                    break;
                }
            }
            self.fetch_next_token()?;
        }
        self.token_available = true;
        Ok(())
    }

    fn fetch_next_token(&mut self) -> Result<()> {
        self.cache(1)?;
        if !self.stream_start_produced {
            self.fetch_stream_start();
            return Ok(());
        }
        self.scan_to_next_token()?;
        self.unroll_indent(self.mark.column);
        self.cache(4)?;
        let (c0, c1, c2) = (self.c(0), self.c(1), self.c(2));
        if is_z(self.b(), 0) {
            return self.fetch_stream_end();
        }
        if self.mark.column == 0 && c0 == b'%' {
            return self.fetch_directive();
        }
        if self.mark.column == 0 && c0 == b'-' && c1 == b'-' && c2 == b'-' && is_blankz(self.b(), 3) {
            return self.fetch_document_indicator(TokenType::DocumentStart);
        }
        if self.mark.column == 0 && c0 == b'.' && c1 == b'.' && c2 == b'.' && is_blankz(self.b(), 3) {
            return self.fetch_document_indicator(TokenType::DocumentEnd);
        }
        match c0 {
            b'[' => return self.fetch_flow_collection_start(TokenType::FlowSequenceStart),
            b'{' => return self.fetch_flow_collection_start(TokenType::FlowMappingStart),
            b']' => return self.fetch_flow_collection_end(TokenType::FlowSequenceEnd),
            b'}' => return self.fetch_flow_collection_end(TokenType::FlowMappingEnd),
            b',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if c0 == b'-' && is_blankz(self.b(), 1) {
            return self.fetch_block_entry();
        }
        if c0 == b'?' && (self.flow_level > 0 || is_blankz(self.b(), 1)) {
            return self.fetch_key();
        }
        if c0 == b':' && (self.flow_level > 0 || is_blankz(self.b(), 1)) {
            return self.fetch_value();
        }
        match c0 {
            b'*' => return self.fetch_anchor(TokenType::Alias),
            b'&' => return self.fetch_anchor(TokenType::Anchor),
            b'!' => return self.fetch_tag(),
            b'|' if self.flow_level == 0 => return self.fetch_block_scalar(true),
            b'>' if self.flow_level == 0 => return self.fetch_block_scalar(false),
            b'\'' => return self.fetch_flow_scalar(true),
            b'"' => return self.fetch_flow_scalar(false),
            _ => {}
        }
        let indicator = is_blankz(self.b(), 0)
            || matches!(
                c0,
                b'-' | b'?'
                    | b':'
                    | b','
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'#'
                    | b'&'
                    | b'*'
                    | b'!'
                    | b'|'
                    | b'>'
                    | b'\''
                    | b'"'
                    | b'%'
                    | b'@'
                    | b'`'
            );
        if !indicator
            || (c0 == b'-' && !is_blank(self.b(), 1))
            || (self.flow_level == 0 && (c0 == b'?' || c0 == b':') && !is_blankz(self.b(), 1))
        {
            return self.fetch_plain_scalar();
        }
        Err(self.error(self.mark, "found character that cannot start any token"))
    }

    fn simple_key_is_valid(&mut self, idx: usize) -> Result<bool> {
        let mark = self.mark;
        let Some(key) = self.simple_keys.get_mut(idx) else {
            return Ok(false);
        };
        if !key.possible {
            return Ok(false);
        }
        if key.mark.line < mark.line || key.mark.index + 1024 < mark.index {
            if key.required {
                let ctx = key.mark;
                return Err(self.error(ctx, "could not find expected ':'"));
            }
            key.possible = false;
            return Ok(false);
        }
        Ok(true)
    }

    fn save_simple_key(&mut self) -> Result<()> {
        let required = self.flow_level == 0 && self.indent == self.mark.column;
        if self.simple_key_allowed {
            let key = SimpleKey {
                possible: true,
                required,
                token_number: self.tokens_parsed + (self.tokens.len() - self.head),
                mark: self.mark,
            };
            self.remove_simple_key()?;
            let last = self.simple_keys.len().saturating_sub(1);
            if let Some(slot) = self.simple_keys.get_mut(last) {
                *slot = key;
            }
            self.simple_keys_by_tok.insert(key.token_number, last);
        }
        Ok(())
    }

    fn remove_simple_key(&mut self) -> Result<()> {
        let i = self.simple_keys.len().saturating_sub(1);
        let Some(key) = self.simple_keys.get_mut(i) else {
            return Ok(());
        };
        if key.possible {
            if key.required {
                let ctx = key.mark;
                return Err(self.error(ctx, "could not find expected ':'"));
            }
            key.possible = false;
            let n = key.token_number;
            self.simple_keys_by_tok.remove(&n);
        }
        Ok(())
    }

    fn increase_flow_level(&mut self) -> Result<()> {
        self.simple_keys.push(SimpleKey {
            possible: false,
            required: false,
            token_number: self.tokens_parsed + (self.tokens.len() - self.head),
            mark: self.mark,
        });
        self.flow_level += 1;
        if self.flow_level > MAX_FLOW_LEVEL {
            let ctx = self.simple_keys.last().map(|k| k.mark).unwrap_or_default();
            return Err(self.error(ctx, &format!("exceeded max depth of {MAX_FLOW_LEVEL}")));
        }
        Ok(())
    }

    fn decrease_flow_level(&mut self) {
        if self.flow_level > 0 {
            self.flow_level -= 1;
            if let Some(k) = self.simple_keys.pop() {
                self.simple_keys_by_tok.remove(&k.token_number);
            }
        }
    }

    fn roll_indent(&mut self, column: i64, number: Option<usize>, typ: TokenType, mark: Mark) -> Result<()> {
        if self.flow_level > 0 {
            return Ok(());
        }
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            if self.indents.len() > MAX_INDENTS {
                let ctx = self.simple_keys.last().map(|k| k.mark).unwrap_or_default();
                return Err(self.error(ctx, &format!("exceeded max depth of {MAX_INDENTS}")));
            }
            let pos = number.map(|n| n.saturating_sub(self.tokens_parsed));
            self.insert_token(pos, Token::new(typ, mark, mark));
        }
        Ok(())
    }

    fn unroll_indent(&mut self, column: i64) {
        if self.flow_level > 0 {
            return;
        }
        while self.indent > column {
            self.insert_token(None, Token::new(TokenType::BlockEnd, self.mark, self.mark));
            self.indent = self.indents.pop().unwrap_or(-1);
        }
    }

    fn fetch_stream_start(&mut self) {
        self.indent = -1;
        self.simple_keys.push(SimpleKey::default());
        self.simple_keys_by_tok = HashMap::new();
        self.simple_key_allowed = true;
        self.stream_start_produced = true;
        self.insert_token(None, Token::new(TokenType::StreamStart, self.mark, self.mark));
    }

    fn fetch_stream_end(&mut self) -> Result<()> {
        if self.mark.column != 0 {
            self.mark.column = 0;
            self.mark.line += 1;
        }
        self.unroll_indent(-1);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        self.insert_token(None, Token::new(TokenType::StreamEnd, self.mark, self.mark));
        Ok(())
    }

    fn fetch_directive(&mut self) -> Result<()> {
        self.unroll_indent(-1);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_directive()?;
        self.insert_token(None, token);
        Ok(())
    }

    fn fetch_document_indicator(&mut self, typ: TokenType) -> Result<()> {
        self.unroll_indent(-1);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let start = self.mark;
        self.skip();
        self.skip();
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(typ, start, end));
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, typ: TokenType) -> Result<()> {
        self.save_simple_key()?;
        self.increase_flow_level()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(typ, start, end));
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, typ: TokenType) -> Result<()> {
        self.remove_simple_key()?;
        self.decrease_flow_level();
        self.simple_key_allowed = false;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(typ, start, end));
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> Result<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(TokenType::FlowEntry, start, end));
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> Result<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(self.error(self.mark, "block sequence entries are not allowed in this context"));
            }
            self.roll_indent(self.mark.column, None, TokenType::BlockSequenceStart, self.mark)?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(TokenType::BlockEntry, start, end));
        Ok(())
    }

    fn fetch_key(&mut self) -> Result<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(self.error(self.mark, "mapping keys are not allowed in this context"));
            }
            self.roll_indent(self.mark.column, None, TokenType::BlockMappingStart, self.mark)?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = self.flow_level == 0;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(TokenType::Key, start, end));
        Ok(())
    }

    fn fetch_value(&mut self) -> Result<()> {
        let idx = self.simple_keys.len().saturating_sub(1);
        if self.simple_key_is_valid(idx)? {
            let key = self.simple_keys.get(idx).copied().unwrap_or_default();
            let pos = key.token_number.saturating_sub(self.tokens_parsed);
            self.insert_token(Some(pos), Token::new(TokenType::Key, key.mark, key.mark));
            self.roll_indent(key.mark.column, Some(key.token_number), TokenType::BlockMappingStart, key.mark)?;
            if let Some(k) = self.simple_keys.get_mut(idx) {
                k.possible = false;
            }
            self.simple_keys_by_tok.remove(&key.token_number);
            self.simple_key_allowed = false;
        } else {
            if self.flow_level == 0 {
                if !self.simple_key_allowed {
                    return Err(self.error(self.mark, "mapping values are not allowed in this context"));
                }
                self.roll_indent(self.mark.column, None, TokenType::BlockMappingStart, self.mark)?;
            }
            self.simple_key_allowed = self.flow_level == 0;
        }
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(None, Token::new(TokenType::Value, start, end));
        Ok(())
    }

    fn fetch_anchor(&mut self, typ: TokenType) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_anchor(typ)?;
        self.insert_token(None, token);
        Ok(())
    }

    fn fetch_tag(&mut self) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_tag()?;
        self.insert_token(None, token);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, literal: bool) -> Result<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let token = self.scan_block_scalar(literal)?;
        self.insert_token(None, token);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, single: bool) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_flow_scalar(single)?;
        self.insert_token(None, token);
        Ok(())
    }

    fn fetch_plain_scalar(&mut self) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_plain_scalar()?;
        self.insert_token(None, token);
        Ok(())
    }

    fn scan_to_next_token(&mut self) -> Result<()> {
        loop {
            self.cache(1)?;
            if self.mark.column == 0 && is_bom(&self.buf, self.pos) {
                self.skip();
            }
            self.cache(1)?;
            while self.c(0) == b' ' || ((self.flow_level > 0 || !self.simple_key_allowed) && self.c(0) == b'\t') {
                self.skip();
                self.cache(1)?;
            }
            if self.c(0) == b'#' {
                while !is_breakz(self.b(), 0) {
                    self.skip();
                    self.cache(1)?;
                }
            }
            if is_break(self.b(), 0) {
                self.cache(2)?;
                self.skip_line();
                if self.flow_level == 0 {
                    self.simple_key_allowed = true;
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    fn scan_directive(&mut self) -> Result<Token> {
        let start = self.mark;
        self.skip();
        let name = self.scan_directive_name(start)?;
        let mut token;
        if name == b"YAML" {
            let (major, minor) = self.scan_version_directive_value(start)?;
            token = Token::new(TokenType::VersionDirective, start, self.mark);
            token.major = major;
            token.minor = minor;
        } else if name == b"TAG" {
            let (handle, prefix) = self.scan_tag_directive_value(start)?;
            token = Token::new(TokenType::TagDirective, start, self.mark);
            token.value = handle;
            token.prefix = prefix;
        } else {
            return Err(self.error(start, "found unknown directive name"));
        }
        self.cache(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.cache(1)?;
        }
        if self.c(0) == b'#' {
            while !is_breakz(self.b(), 0) {
                self.skip();
                self.cache(1)?;
            }
        }
        if !is_breakz(self.b(), 0) {
            return Err(self.error(start, "did not find expected comment or line break"));
        }
        if is_break(self.b(), 0) {
            self.cache(2)?;
            self.skip_line();
        }
        Ok(token)
    }

    fn scan_directive_name(&mut self, start: Mark) -> Result<Vec<u8>> {
        self.cache(1)?;
        let mut s = Vec::new();
        while is_alpha(self.b(), 0) {
            self.read(&mut s);
            self.cache(1)?;
        }
        if s.is_empty() {
            return Err(self.error(start, "could not find expected directive name"));
        }
        if !is_blankz(self.b(), 0) {
            return Err(self.error(start, "found unexpected non-alphabetical character"));
        }
        Ok(s)
    }

    fn scan_version_directive_value(&mut self, start: Mark) -> Result<(i8, i8)> {
        self.cache(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.cache(1)?;
        }
        let major = self.scan_version_directive_number(start)?;
        if self.c(0) != b'.' {
            return Err(self.error(start, "did not find expected digit or '.' character"));
        }
        self.skip();
        let minor = self.scan_version_directive_number(start)?;
        Ok((major, minor))
    }

    fn scan_version_directive_number(&mut self, start: Mark) -> Result<i8> {
        self.cache(1)?;
        let mut value: i8 = 0;
        let mut length: i8 = 0;
        while is_digit(self.b(), 0) {
            length += 1;
            if length > MAX_NUMBER_LENGTH {
                return Err(self.error(start, "found extremely long version number"));
            }
            value = value.wrapping_mul(10).wrapping_add(as_digit(self.b(), 0));
            self.skip();
            self.cache(1)?;
        }
        if length == 0 {
            return Err(self.error(start, "did not find expected version number"));
        }
        Ok(value)
    }

    fn scan_tag_directive_value(&mut self, start: Mark) -> Result<(Vec<u8>, Vec<u8>)> {
        self.cache(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.cache(1)?;
        }
        let handle = self.scan_tag_handle(true, start)?;
        self.cache(1)?;
        if !is_blank(self.b(), 0) {
            return Err(self.error(start, "did not find expected whitespace"));
        }
        while is_blank(self.b(), 0) {
            self.skip();
            self.cache(1)?;
        }
        let prefix = self.scan_tag_uri(true, &[], start)?;
        self.cache(1)?;
        if !is_blankz(self.b(), 0) {
            return Err(self.error(start, "did not find expected whitespace or line break"));
        }
        Ok((handle, prefix))
    }

    fn scan_anchor(&mut self, typ: TokenType) -> Result<Token> {
        let mut s = Vec::new();
        let start = self.mark;
        self.skip();
        self.cache(1)?;
        while is_alpha(self.b(), 0) {
            self.read(&mut s);
            self.cache(1)?;
        }
        let end = self.mark;
        let c = self.c(0);
        if s.is_empty()
            || !(is_blankz(self.b(), 0) || matches!(c, b'?' | b':' | b',' | b']' | b'}' | b'%' | b'@' | b'`'))
        {
            return Err(self.error(start, "did not find expected alphabetic or numeric character"));
        }
        let mut token = Token::new(typ, start, end);
        token.value = s;
        Ok(token)
    }

    fn scan_tag(&mut self) -> Result<Token> {
        let start = self.mark;
        self.cache(2)?;
        let mut handle;
        let mut suffix;
        if self.c(1) == b'<' {
            handle = Vec::new();
            self.skip();
            self.skip();
            suffix = self.scan_tag_uri(false, &[], start)?;
            if self.c(0) != b'>' {
                return Err(self.error(start, "did not find the expected '>'"));
            }
            self.skip();
        } else {
            handle = self.scan_tag_handle(false, start)?;
            if handle.first() == Some(&b'!') && handle.len() > 1 && handle.last() == Some(&b'!') {
                suffix = self.scan_tag_uri(false, &[], start)?;
            } else {
                suffix = self.scan_tag_uri(false, &handle, start)?;
                handle = vec![b'!'];
                if suffix.is_empty() {
                    std::mem::swap(&mut handle, &mut suffix);
                }
            }
        }
        self.cache(1)?;
        if !is_blankz(self.b(), 0) {
            return Err(self.error(start, "did not find expected whitespace or line break"));
        }
        let mut token = Token::new(TokenType::Tag, start, self.mark);
        token.value = handle;
        token.suffix = suffix;
        Ok(token)
    }

    fn scan_tag_handle(&mut self, _directive: bool, start: Mark) -> Result<Vec<u8>> {
        self.cache(1)?;
        if self.c(0) != b'!' {
            return Err(self.tag_error(start, "did not find expected '!'"));
        }
        let mut s = Vec::new();
        self.read(&mut s);
        self.cache(1)?;
        while is_alpha(self.b(), 0) {
            self.read(&mut s);
            self.cache(1)?;
        }
        if self.c(0) == b'!' {
            self.read(&mut s);
        } else if _directive && s != b"!" {
            return Err(self.tag_error(start, "did not find expected '!'"));
        }
        Ok(s)
    }

    fn tag_error(&self, start: Mark, problem: &str) -> YamlError {
        self.error(start, problem)
    }

    fn scan_tag_uri(&mut self, directive: bool, head: &[u8], start: Mark) -> Result<Vec<u8>> {
        let mut s = Vec::new();
        let mut has_tag = !head.is_empty();
        if head.len() > 1 {
            s.extend_from_slice(head.get(1..).unwrap_or_default());
        }
        self.cache(1)?;
        loop {
            let c = self.c(0);
            let ok = is_alpha(self.b(), 0)
                || matches!(
                    c,
                    b';' | b'/'
                        | b'?'
                        | b':'
                        | b'@'
                        | b'&'
                        | b'='
                        | b'+'
                        | b'$'
                        | b','
                        | b'.'
                        | b'!'
                        | b'~'
                        | b'*'
                        | b'\''
                        | b'('
                        | b')'
                        | b'['
                        | b']'
                        | b'%'
                );
            if !ok {
                break;
            }
            if c == b'%' {
                self.scan_uri_escapes(directive, start, &mut s)?;
            } else {
                self.read(&mut s);
            }
            self.cache(1)?;
            has_tag = true;
        }
        if !has_tag {
            return Err(self.tag_error(start, "did not find expected tag URI"));
        }
        Ok(s)
    }

    fn scan_uri_escapes(&mut self, _directive: bool, start: Mark, s: &mut Vec<u8>) -> Result<()> {
        let mut w: usize = 1024;
        while w > 0 {
            self.cache(3)?;
            if !(self.c(0) == b'%' && is_hex(self.b(), 1) && is_hex(self.b(), 2)) {
                return Err(self.tag_error(start, "did not find URI escaped octet"));
            }
            let octet = u8::try_from((as_hex(self.b(), 1) << 4) + as_hex(self.b(), 2)).unwrap_or(0);
            if w == 1024 {
                w = width(octet);
                if w == 0 {
                    return Err(self.tag_error(start, "found an incorrect leading UTF-8 octet"));
                }
            } else if octet & 0xC0 != 0x80 {
                return Err(self.tag_error(start, "found an incorrect trailing UTF-8 octet"));
            }
            s.push(octet);
            self.skip();
            self.skip();
            self.skip();
            w -= 1;
        }
        Ok(())
    }

    fn scan_block_scalar(&mut self, literal: bool) -> Result<Token> {
        let start = self.mark;
        self.skip();
        self.cache(1)?;
        let mut chomping = 0;
        let mut increment: i64 = 0;
        if self.c(0) == b'+' || self.c(0) == b'-' {
            chomping = if self.c(0) == b'+' { 1 } else { -1 };
            self.skip();
            self.cache(1)?;
            if is_digit(self.b(), 0) {
                if self.c(0) == b'0' {
                    return Err(self.error(start, "found an indentation indicator equal to 0"));
                }
                increment = i64::from(as_digit(self.b(), 0));
                self.skip();
            }
        } else if is_digit(self.b(), 0) {
            if self.c(0) == b'0' {
                return Err(self.error(start, "found an indentation indicator equal to 0"));
            }
            increment = i64::from(as_digit(self.b(), 0));
            self.skip();
            self.cache(1)?;
            if self.c(0) == b'+' || self.c(0) == b'-' {
                chomping = if self.c(0) == b'+' { 1 } else { -1 };
                self.skip();
            }
        }
        self.cache(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.cache(1)?;
        }
        if self.c(0) == b'#' {
            while !is_breakz(self.b(), 0) {
                self.skip();
                self.cache(1)?;
            }
        }
        if !is_breakz(self.b(), 0) {
            return Err(self.error(start, "did not find expected comment or line break"));
        }
        if is_break(self.b(), 0) {
            self.cache(2)?;
            self.skip_line();
        }
        let mut end = self.mark;
        let mut indent: i64 = 0;
        if increment > 0 {
            indent = if self.indent >= 0 { self.indent + increment } else { increment };
        }
        let mut s = Vec::new();
        let mut leading_break = Vec::new();
        let mut trailing_breaks = Vec::new();
        self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start, &mut end)?;
        self.cache(1)?;
        let mut leading_blank = false;
        while self.mark.column == indent && !is_z(self.b(), 0) {
            let trailing_blank = is_blank(self.b(), 0);
            if !literal && !leading_blank && !trailing_blank && leading_break.first() == Some(&b'\n') {
                if trailing_breaks.is_empty() {
                    s.push(b' ');
                }
            } else {
                s.extend_from_slice(&leading_break);
            }
            leading_break.clear();
            s.extend_from_slice(&trailing_breaks);
            trailing_breaks.clear();
            leading_blank = is_blank(self.b(), 0);
            while !is_breakz(self.b(), 0) {
                self.read(&mut s);
                self.cache(1)?;
            }
            self.cache(2)?;
            self.read_line(&mut leading_break);
            self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start, &mut end)?;
        }
        if chomping != -1 {
            s.extend_from_slice(&leading_break);
        }
        if chomping == 1 {
            s.extend_from_slice(&trailing_breaks);
        }
        let mut token = Token::new(TokenType::Scalar, start, end);
        token.value = s;
        token.style = if literal { ScalarStyle::Literal } else { ScalarStyle::Folded };
        Ok(token)
    }

    fn scan_block_scalar_breaks(
        &mut self,
        indent: &mut i64,
        breaks: &mut Vec<u8>,
        start: Mark,
        end: &mut Mark,
    ) -> Result<()> {
        *end = self.mark;
        let mut max_indent: i64 = 0;
        loop {
            self.cache(1)?;
            while (*indent == 0 || self.mark.column < *indent) && is_space(self.b(), 0) {
                self.skip();
                self.cache(1)?;
            }
            if self.mark.column > max_indent {
                max_indent = self.mark.column;
            }
            if (*indent == 0 || self.mark.column < *indent) && is_tab(self.b(), 0) {
                return Err(self.error(start, "found a tab character where an indentation space is expected"));
            }
            if !is_break(self.b(), 0) {
                break;
            }
            self.cache(2)?;
            self.read_line(breaks);
            *end = self.mark;
        }
        if *indent == 0 {
            *indent = max_indent;
            if *indent < self.indent + 1 {
                *indent = self.indent + 1;
            }
            if *indent < 1 {
                *indent = 1;
            }
        }
        Ok(())
    }

    fn scan_flow_scalar(&mut self, single: bool) -> Result<Token> {
        let start = self.mark;
        self.skip();
        let mut s = Vec::new();
        let mut leading_break = Vec::new();
        let mut trailing_breaks = Vec::new();
        let mut whitespaces = Vec::new();
        loop {
            self.cache(4)?;
            let (c0, c1, c2) = (self.c(0), self.c(1), self.c(2));
            if self.mark.column == 0
                && ((c0 == b'-' && c1 == b'-' && c2 == b'-') || (c0 == b'.' && c1 == b'.' && c2 == b'.'))
                && is_blankz(self.b(), 3)
            {
                return Err(self.error(start, "found unexpected document indicator"));
            }
            if is_z(self.b(), 0) {
                return Err(self.error(start, "found unexpected end of stream"));
            }
            let mut leading_blanks = false;
            while !is_blankz(self.b(), 0) {
                let c0 = self.c(0);
                if single && c0 == b'\'' && self.c(1) == b'\'' {
                    s.push(b'\'');
                    self.skip();
                    self.skip();
                } else if single && c0 == b'\'' {
                    break;
                } else if !single && c0 == b'"' {
                    break;
                } else if !single && c0 == b'\\' && is_break(self.b(), 1) {
                    self.cache(3)?;
                    self.skip();
                    self.skip_line();
                    leading_blanks = true;
                    break;
                } else if !single && c0 == b'\\' {
                    let mut code_length = 0;
                    match self.c(1) {
                        b'0' => s.push(0),
                        b'a' => s.push(0x07),
                        b'b' => s.push(0x08),
                        b't' | b'\t' => s.push(0x09),
                        b'n' => s.push(0x0A),
                        b'v' => s.push(0x0B),
                        b'f' => s.push(0x0C),
                        b'r' => s.push(0x0D),
                        b'e' => s.push(0x1B),
                        b' ' => s.push(0x20),
                        b'"' => s.push(b'"'),
                        b'\'' => s.push(b'\''),
                        b'\\' => s.push(b'\\'),
                        b'N' => s.extend_from_slice(&[0xC2, 0x85]),
                        b'_' => s.extend_from_slice(&[0xC2, 0xA0]),
                        b'L' => s.extend_from_slice(&[0xE2, 0x80, 0xA8]),
                        b'P' => s.extend_from_slice(&[0xE2, 0x80, 0xA9]),
                        b'x' => code_length = 2,
                        b'u' => code_length = 4,
                        b'U' => code_length = 8,
                        _ => return Err(self.error(start, "found unknown escape character")),
                    }
                    self.skip();
                    self.skip();
                    if code_length > 0 {
                        self.cache(code_length)?;
                        let mut value: u32 = 0;
                        for k in 0..code_length {
                            if !is_hex(self.b(), k) {
                                return Err(self.error(start, "did not find expected hexdecimal number"));
                            }
                            value = (value << 4) + as_hex(self.b(), k);
                        }
                        if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
                            return Err(self.error(start, "found invalid Unicode character escape code"));
                        }
                        push_utf8(&mut s, value);
                        for _ in 0..code_length {
                            self.skip();
                        }
                    }
                } else {
                    self.read(&mut s);
                }
                self.cache(2)?;
            }
            self.cache(1)?;
            let c0 = self.c(0);
            if (single && c0 == b'\'') || (!single && c0 == b'"') {
                break;
            }
            while is_blank(self.b(), 0) || is_break(self.b(), 0) {
                if is_blank(self.b(), 0) {
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.cache(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.cache(1)?;
            }
            if leading_blanks {
                if leading_break.first() == Some(&b'\n') {
                    if trailing_breaks.is_empty() {
                        s.push(b' ');
                    } else {
                        s.extend_from_slice(&trailing_breaks);
                    }
                } else {
                    s.extend_from_slice(&leading_break);
                    s.extend_from_slice(&trailing_breaks);
                }
                trailing_breaks.clear();
                leading_break.clear();
            } else {
                s.extend_from_slice(&whitespaces);
                whitespaces.clear();
            }
        }
        self.skip();
        let mut token = Token::new(TokenType::Scalar, start, self.mark);
        token.value = s;
        token.style = if single { ScalarStyle::SingleQuoted } else { ScalarStyle::DoubleQuoted };
        Ok(token)
    }

    fn scan_plain_scalar(&mut self) -> Result<Token> {
        let mut s = Vec::new();
        let mut leading_break = Vec::new();
        let mut trailing_breaks = Vec::new();
        let mut whitespaces = Vec::new();
        let mut leading_blanks = false;
        let indent = self.indent + 1;
        let start = self.mark;
        let mut end = self.mark;
        loop {
            self.cache(4)?;
            let (c0, c1, c2) = (self.c(0), self.c(1), self.c(2));
            if self.mark.column == 0
                && ((c0 == b'-' && c1 == b'-' && c2 == b'-') || (c0 == b'.' && c1 == b'.' && c2 == b'.'))
                && is_blankz(self.b(), 3)
            {
                break;
            }
            if c0 == b'#' {
                break;
            }
            while !is_blankz(self.b(), 0) {
                let c = self.c(0);
                if (c == b':' && is_blankz(self.b(), 1))
                    || (self.flow_level > 0 && matches!(c, b',' | b'?' | b'[' | b']' | b'{' | b'}'))
                {
                    break;
                }
                if leading_blanks || !whitespaces.is_empty() {
                    if leading_blanks {
                        if leading_break.first() == Some(&b'\n') {
                            if trailing_breaks.is_empty() {
                                s.push(b' ');
                            } else {
                                s.extend_from_slice(&trailing_breaks);
                            }
                        } else {
                            s.extend_from_slice(&leading_break);
                            s.extend_from_slice(&trailing_breaks);
                        }
                        trailing_breaks.clear();
                        leading_break.clear();
                        leading_blanks = false;
                    } else {
                        s.extend_from_slice(&whitespaces);
                        whitespaces.clear();
                    }
                }
                self.read(&mut s);
                end = self.mark;
                self.cache(2)?;
            }
            if !(is_blank(self.b(), 0) || is_break(self.b(), 0)) {
                break;
            }
            self.cache(1)?;
            while is_blank(self.b(), 0) || is_break(self.b(), 0) {
                if is_blank(self.b(), 0) {
                    if leading_blanks && self.mark.column < indent && is_tab(self.b(), 0) {
                        return Err(self.error(start, "found a tab character that violates indentation"));
                    }
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.cache(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.cache(1)?;
            }
            if self.flow_level == 0 && self.mark.column < indent {
                break;
            }
        }
        let mut token = Token::new(TokenType::Scalar, start, end);
        token.value = s;
        token.style = ScalarStyle::Plain;
        if leading_blanks {
            self.simple_key_allowed = true;
        }
        Ok(token)
    }
}

/// Appends a code point as UTF-8, as go-yaml writes escapes.
fn push_utf8(s: &mut Vec<u8>, value: u32) {
    let b = |x: u32| u8::try_from(x & 0xFF).unwrap_or(0);
    if value <= 0x7F {
        s.push(b(value));
    } else if value <= 0x7FF {
        s.push(b(0xC0 + (value >> 6)));
        s.push(b(0x80 + (value & 0x3F)));
    } else if value <= 0xFFFF {
        s.push(b(0xE0 + (value >> 12)));
        s.push(b(0x80 + ((value >> 6) & 0x3F)));
        s.push(b(0x80 + (value & 0x3F)));
    } else {
        s.push(b(0xF0 + (value >> 18)));
        s.push(b(0x80 + ((value >> 12) & 0x3F)));
        s.push(b(0x80 + ((value >> 6) & 0x3F)));
        s.push(b(0x80 + (value & 0x3F)));
    }
}
