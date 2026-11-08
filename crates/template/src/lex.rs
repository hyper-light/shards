//! text/template/parse/lex.go: the scanner, with its trim markers, comments and
//! keywords. Delimiters are Go's defaults, `{{` and `}}`.

use std::fmt;

use crate::fmt::{quote_prefix, sharp_u};
use crate::strconv;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum T {
    Error,
    Bool,
    Char,
    CharConstant,
    Comment,
    Complex,
    Assign,
    Declare,
    Eof,
    Field,
    Identifier,
    LeftDelim,
    LeftParen,
    Number,
    Pipe,
    RawString,
    RightDelim,
    RightParen,
    Space,
    String,
    Text,
    Variable,
    // Keywords.
    Block,
    Break,
    Continue,
    Dot,
    Define,
    Else,
    End,
    If,
    Nil,
    Range,
    Template,
    With,
}

impl T {
    fn is_keyword(self) -> bool {
        matches!(
            self,
            T::Block
                | T::Break
                | T::Continue
                | T::Dot
                | T::Define
                | T::Else
                | T::End
                | T::If
                | T::Nil
                | T::Range
                | T::Template
                | T::With
        )
    }
}

fn keyword(word: &str) -> Option<T> {
    Some(match word {
        "." => T::Dot,
        "block" => T::Block,
        "break" => T::Break,
        "continue" => T::Continue,
        "define" => T::Define,
        "else" => T::Else,
        "end" => T::End,
        "if" => T::If,
        "range" => T::Range,
        "nil" => T::Nil,
        "template" => T::Template,
        "with" => T::With,
        _ => return None,
    })
}

/// A token: its type, where it starts, its text and its line.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub typ: T,
    pub pos: usize,
    pub val: String,
    pub line: usize,
}

impl Item {
    pub(crate) fn eof() -> Item {
        Item {
            typ: T::Eof,
            pos: 0,
            val: "EOF".into(),
            line: 0,
        }
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.typ {
            T::Eof => f.write_str("EOF"),
            T::Error => f.write_str(&self.val),
            t if t.is_keyword() => write!(f, "<{}>", self.val),
            _ if self.val.len() > 10 => write!(f, "{}...", quote_prefix(&self.val, 10)),
            _ => f.write_str(&strconv::quote(&self.val)),
        }
    }
}

const LEFT_DELIM: &str = "{{";
const RIGHT_DELIM: &str = "}}";
const LEFT_COMMENT: &str = "/*";
const RIGHT_COMMENT: &str = "*/";
const SPACE_CHARS: [char; 4] = [' ', '\t', '\r', '\n'];
/// The trim marker and the space beside it.
const TRIM_MARKER_LEN: usize = 2;

#[derive(Clone, Copy)]
enum State {
    Text,
    LeftDelim,
    Comment,
    RightDelim,
    InsideAction,
    Space,
    Identifier,
    Field,
    Variable,
    Char,
    Number,
    Quote,
    RawQuote,
}

pub(crate) struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    start: usize,
    at_eof: bool,
    paren_depth: i64,
    line: usize,
    start_line: usize,
    item: Item,
    inside_action: bool,
}

fn is_space(r: Option<char>) -> bool {
    matches!(r, Some(' ' | '\t' | '\r' | '\n'))
}

/// unicode.IsLetter and IsDigit, as std's Alphabetic and Numeric properties approximate
/// them beyond ASCII.
fn is_alpha_numeric(r: Option<char>) -> bool {
    match r {
        Some(c) => c == '_' || c.is_alphabetic() || c.is_numeric(),
        None => false,
    }
}

fn has_left_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    b.first() == Some(&b'-') && matches!(b.get(1), Some(b' ' | b'\t' | b'\r' | b'\n'))
}

fn has_right_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b' ' | b'\t' | b'\r' | b'\n')) && b.get(1) == Some(&b'-')
}

fn newlines(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(input: &'a str) -> Lexer<'a> {
        Lexer {
            input,
            pos: 0,
            start: 0,
            at_eof: false,
            paren_depth: 0,
            line: 1,
            start_line: 1,
            item: Item::eof(),
            inside_action: false,
        }
    }

    fn from(&self, i: usize) -> &'a str {
        self.input.get(i..).unwrap_or("")
    }

    fn span(&self, a: usize, b: usize) -> &'a str {
        self.input.get(a..b).unwrap_or("")
    }

    fn next(&mut self) -> Option<char> {
        let Some(r) = self.from(self.pos).chars().next() else {
            self.at_eof = true;
            return None;
        };
        self.pos += r.len_utf8();
        if r == '\n' {
            self.line += 1;
        }
        Some(r)
    }

    fn peek(&mut self) -> Option<char> {
        let r = self.next();
        self.backup();
        r
    }

    fn backup(&mut self) {
        if !self.at_eof
            && self.pos > 0
            && let Some(r) = self.span(0, self.pos).chars().next_back()
        {
            self.pos -= r.len_utf8();
            if r == '\n' {
                self.line -= 1;
            }
        }
    }

    fn this_item(&mut self, t: T) -> Item {
        let i = Item {
            typ: t,
            pos: self.start,
            val: self.span(self.start, self.pos).to_owned(),
            line: self.start_line,
        };
        self.start = self.pos;
        self.start_line = self.line;
        i
    }

    fn emit(&mut self, t: T) -> Option<State> {
        let i = self.this_item(t);
        self.emit_item(i)
    }

    fn emit_item(&mut self, i: Item) -> Option<State> {
        self.item = i;
        None
    }

    fn ignore(&mut self) {
        self.line += newlines(self.span(self.start, self.pos));
        self.start = self.pos;
        self.start_line = self.line;
    }

    fn accept(&mut self, valid: &str) -> bool {
        if let Some(c) = self.next()
            && valid.contains(c)
        {
            return true;
        }
        self.backup();
        false
    }

    fn accept_run(&mut self, valid: &str) {
        while let Some(c) = self.next() {
            if !valid.contains(c) {
                break;
            }
        }
        self.backup();
    }

    fn errorf(&mut self, msg: String) -> Option<State> {
        self.item = Item {
            typ: T::Error,
            pos: self.start,
            val: msg,
            line: self.start_line,
        };
        self.start = 0;
        self.pos = 0;
        self.input = "";
        None
    }

    /// The next token.
    pub(crate) fn next_item(&mut self) -> Item {
        self.item = Item {
            typ: T::Eof,
            pos: self.pos,
            val: "EOF".into(),
            line: self.start_line,
        };
        let mut state = if self.inside_action {
            State::InsideAction
        } else {
            State::Text
        };
        loop {
            let next = match state {
                State::Text => self.lex_text(),
                State::LeftDelim => self.lex_left_delim(),
                State::Comment => self.lex_comment(),
                State::RightDelim => self.lex_right_delim(),
                State::InsideAction => self.lex_inside_action(),
                State::Space => self.lex_space(),
                State::Identifier => self.lex_identifier(),
                State::Field => self.lex_field_or_variable(T::Field),
                State::Variable => self.lex_variable(),
                State::Char => self.lex_char(),
                State::Number => self.lex_number(),
                State::Quote => self.lex_quote(),
                State::RawQuote => self.lex_raw_quote(),
            };
            match next {
                Some(s) => state = s,
                None => return self.item.clone(),
            }
        }
    }

    fn lex_text(&mut self) -> Option<State> {
        if let Some(x) = self.from(self.pos).find(LEFT_DELIM) {
            if x > 0 {
                self.pos += x;
                let mut trim = 0;
                let delim_end = self.pos + LEFT_DELIM.len();
                if has_left_trim_marker(self.from(delim_end)) {
                    let text = self.span(self.start, self.pos);
                    trim = text.len() - text.trim_end_matches(SPACE_CHARS).len();
                }
                self.pos -= trim;
                self.line += newlines(self.span(self.start, self.pos));
                let i = self.this_item(T::Text);
                self.pos += trim;
                self.ignore();
                if !i.val.is_empty() {
                    return self.emit_item(i);
                }
            }
            return Some(State::LeftDelim);
        }
        self.pos = self.input.len();
        if self.pos > self.start {
            self.line += newlines(self.span(self.start, self.pos));
            return self.emit(T::Text);
        }
        self.emit(T::Eof)
    }

    fn at_right_delim(&self) -> (bool, bool) {
        let rest = self.from(self.pos);
        if has_right_trim_marker(rest) && self.from(self.pos + TRIM_MARKER_LEN).starts_with(RIGHT_DELIM) {
            return (true, true);
        }
        if rest.starts_with(RIGHT_DELIM) {
            return (true, false);
        }
        (false, false)
    }

    fn lex_left_delim(&mut self) -> Option<State> {
        self.pos += LEFT_DELIM.len();
        let trim = has_left_trim_marker(self.from(self.pos));
        let after_marker = if trim { TRIM_MARKER_LEN } else { 0 };
        if self.from(self.pos + after_marker).starts_with(LEFT_COMMENT) {
            self.pos += after_marker;
            self.ignore();
            return Some(State::Comment);
        }
        let i = self.this_item(T::LeftDelim);
        self.inside_action = true;
        self.pos += after_marker;
        self.ignore();
        self.paren_depth = 0;
        self.emit_item(i)
    }

    fn lex_comment(&mut self) -> Option<State> {
        self.pos += LEFT_COMMENT.len();
        let Some(x) = self.from(self.pos).find(RIGHT_COMMENT) else {
            return self.errorf("unclosed comment".into());
        };
        self.pos += x + RIGHT_COMMENT.len();
        let (delim, trim) = self.at_right_delim();
        if !delim {
            return self.errorf("comment ends before closing delimiter".into());
        }
        self.line += newlines(self.span(self.start, self.pos));
        let _comment = self.this_item(T::Comment);
        if trim {
            self.pos += TRIM_MARKER_LEN;
        }
        self.pos += RIGHT_DELIM.len();
        if trim {
            let rest = self.from(self.pos);
            self.pos += rest.len() - rest.trim_start_matches(SPACE_CHARS).len();
        }
        self.ignore();
        Some(State::Text)
    }

    fn lex_right_delim(&mut self) -> Option<State> {
        let (_, trim) = self.at_right_delim();
        if trim {
            self.pos += TRIM_MARKER_LEN;
            self.ignore();
        }
        self.pos += RIGHT_DELIM.len();
        let i = self.this_item(T::RightDelim);
        if trim {
            let rest = self.from(self.pos);
            self.pos += rest.len() - rest.trim_start_matches(SPACE_CHARS).len();
            self.ignore();
        }
        self.inside_action = false;
        self.emit_item(i)
    }

    fn lex_inside_action(&mut self) -> Option<State> {
        let (delim, _) = self.at_right_delim();
        if delim {
            if self.paren_depth == 0 {
                return Some(State::RightDelim);
            }
            return self.errorf("unclosed left paren".into());
        }
        let r = self.next();
        let Some(c) = r else {
            return self.errorf("unclosed action".into());
        };
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                self.backup();
                Some(State::Space)
            }
            '=' => self.emit(T::Assign),
            ':' => {
                if self.next() != Some('=') {
                    return self.errorf("expected :=".into());
                }
                self.emit(T::Declare)
            }
            '|' => self.emit(T::Pipe),
            '"' => Some(State::Quote),
            '`' => Some(State::RawQuote),
            '$' => Some(State::Variable),
            '\'' => Some(State::Char),
            '.' if !matches!(self.input.as_bytes().get(self.pos), None | Some(b'0'..=b'9')) => {
                Some(State::Field)
            }
            '.' | '+' | '-' | '0'..='9' => {
                self.backup();
                Some(State::Number)
            }
            _ if is_alpha_numeric(r) => {
                self.backup();
                Some(State::Identifier)
            }
            '(' => {
                self.paren_depth += 1;
                self.emit(T::LeftParen)
            }
            ')' => {
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return self.errorf("unexpected right paren".into());
                }
                self.emit(T::RightParen)
            }
            _ if c.is_ascii() && !c.is_ascii_control() => self.emit(T::Char),
            _ => self.errorf(format!("unrecognized character in action: {}", sharp_u(c))),
        }
    }

    fn lex_space(&mut self) -> Option<State> {
        let mut num_spaces = 0;
        while is_space(self.peek()) {
            self.next();
            num_spaces += 1;
        }
        let before = self.pos.saturating_sub(1);
        if has_right_trim_marker(self.from(before))
            && self.from(before + TRIM_MARKER_LEN).starts_with(RIGHT_DELIM)
        {
            self.backup();
            if num_spaces == 1 {
                return Some(State::RightDelim);
            }
        }
        self.emit(T::Space)
    }

    fn lex_identifier(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if is_alpha_numeric(r) {
                continue;
            }
            self.backup();
            let word = self.span(self.start, self.pos);
            if !self.at_terminator() {
                return self.bad_character(r);
            }
            return match keyword(word) {
                Some(k) if k.is_keyword() => self.emit(k),
                _ if word.starts_with('.') => self.emit(T::Field),
                _ if word == "true" || word == "false" => self.emit(T::Bool),
                _ => self.emit(T::Identifier),
            };
        }
    }

    fn bad_character(&mut self, r: Option<char>) -> Option<State> {
        let shown = match r {
            Some(c) => sharp_u(c),
            None => "%!U(int32=-1)".into(),
        };
        self.errorf(format!("bad character {shown}"))
    }

    fn lex_variable(&mut self) -> Option<State> {
        if self.at_terminator() {
            return self.emit(T::Variable);
        }
        self.lex_field_or_variable(T::Variable)
    }

    fn lex_field_or_variable(&mut self, typ: T) -> Option<State> {
        if self.at_terminator() {
            if typ == T::Variable {
                return self.emit(T::Variable);
            }
            return self.emit(T::Dot);
        }
        let mut r;
        loop {
            r = self.next();
            if !is_alpha_numeric(r) {
                self.backup();
                break;
            }
        }
        if !self.at_terminator() {
            return self.bad_character(r);
        }
        self.emit(typ)
    }

    fn at_terminator(&mut self) -> bool {
        let r = self.peek();
        if is_space(r) {
            return true;
        }
        match r {
            None | Some('.' | ',' | '|' | ':' | ')' | '(') => true,
            _ => self.from(self.pos).starts_with(RIGHT_DELIM),
        }
    }

    fn lex_char(&mut self) -> Option<State> {
        loop {
            match self.next() {
                Some('\\') => {
                    let r = self.next();
                    if r.is_some() && r != Some('\n') {
                        continue;
                    }
                    return self.errorf("unterminated character constant".into());
                }
                None | Some('\n') => return self.errorf("unterminated character constant".into()),
                Some('\'') => break,
                Some(_) => {}
            }
        }
        self.emit(T::CharConstant)
    }

    fn lex_number(&mut self) -> Option<State> {
        if !self.scan_number() {
            let text = self.span(self.start, self.pos).to_owned();
            return self.errorf(format!("bad number syntax: {}", strconv::quote(&text)));
        }
        let sign = self.peek();
        if sign == Some('+') || sign == Some('-') {
            if !self.scan_number() || self.input.as_bytes().get(self.pos.wrapping_sub(1)) != Some(&b'i') {
                let text = self.span(self.start, self.pos).to_owned();
                return self.errorf(format!("bad number syntax: {}", strconv::quote(&text)));
            }
            return self.emit(T::Complex);
        }
        self.emit(T::Number)
    }

    fn scan_number(&mut self) -> bool {
        self.accept("+-");
        let mut digits = "0123456789_";
        if self.accept("0") {
            if self.accept("xX") {
                digits = "0123456789abcdefABCDEF_";
            } else if self.accept("oO") {
                digits = "01234567_";
            } else if self.accept("bB") {
                digits = "01_";
            }
        }
        self.accept_run(digits);
        if self.accept(".") {
            self.accept_run(digits);
        }
        if digits.len() == 10 + 1 && self.accept("eE") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        if digits.len() == 16 + 6 + 1 && self.accept("pP") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        self.accept("i");
        if is_alpha_numeric(self.peek()) {
            self.next();
            return false;
        }
        true
    }

    fn lex_quote(&mut self) -> Option<State> {
        loop {
            match self.next() {
                Some('\\') => {
                    let r = self.next();
                    if r.is_some() && r != Some('\n') {
                        continue;
                    }
                    return self.errorf("unterminated quoted string".into());
                }
                None | Some('\n') => return self.errorf("unterminated quoted string".into()),
                Some('"') => break,
                Some(_) => {}
            }
        }
        self.emit(T::String)
    }

    fn lex_raw_quote(&mut self) -> Option<State> {
        loop {
            match self.next() {
                None => return self.errorf("unterminated raw quoted string".into()),
                Some('`') => break,
                Some(_) => {}
            }
        }
        self.emit(T::RawString)
    }
}
