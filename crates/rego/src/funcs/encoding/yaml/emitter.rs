//! go-yaml v2.4.2's encoder (encode.go, sorter.go) and libyaml's emitter as Go ported
//! it (emitterc.go), for the values go-yaml decodes from JSON: block collections (flow
//! when empty), scalars plain, quoted or literal as go-yaml picks, lines folded at 80
//! columns, keys in go-yaml's natural order.

use std::sync::OnceLock;

use super::scanner::{
    ScalarStyle, at, is_blank, is_blankz, is_bom, is_break, is_printable, is_space, width,
};
use super::{GoVal, gofmt, needs_quotes};

#[derive(Debug, Default, Clone)]
struct ScalarData {
    multiline: bool,
    flow_plain_allowed: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
}

#[derive(Debug)]
struct Emitter {
    out: Vec<u8>,
    best_indent: i64,
    best_width: i64,
    indent: i64,
    indents: Vec<i64>,
    flow_level: i64,
    root_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: i64,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
}

/// yaml_emitter_analyze_scalar.
fn analyze_scalar(value: &[u8]) -> ScalarData {
    if value.is_empty() {
        return ScalarData {
            multiline: false,
            flow_plain_allowed: false,
            block_plain_allowed: true,
            single_quoted_allowed: true,
            block_allowed: false,
        };
    }
    let mut block_indicators = false;
    let mut flow_indicators = false;
    let mut line_breaks = false;
    let mut special_characters = false;
    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;
    let mut previous_space = false;
    let mut previous_break = false;
    let c0 = at(value, 0);
    let c1 = at(value, 1);
    let c2 = at(value, 2);
    if value.len() >= 3 && ((c0 == b'-' && c1 == b'-' && c2 == b'-') || (c0 == b'.' && c1 == b'.' && c2 == b'.')) {
        block_indicators = true;
        flow_indicators = true;
    }
    let mut preceded_by_whitespace = true;
    let mut i = 0;
    while i < value.len() {
        let w = width(at(value, i)).max(1);
        let followed_by_whitespace = i + w >= value.len() || is_blank(value, i + w);
        let c = at(value, i);
        if i == 0 {
            match c {
                b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>' | b'\'' | b'"' | b'%'
                | b'@' | b'`' => {
                    flow_indicators = true;
                    block_indicators = true;
                }
                b'?' | b':' => {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                b'-' => {
                    if followed_by_whitespace {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                }
                _ => {}
            }
        } else {
            match c {
                b',' | b'?' | b'[' | b']' | b'{' | b'}' => flow_indicators = true,
                b':' => {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                b'#' => {
                    if preceded_by_whitespace {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                }
                _ => {}
            }
        }
        if !is_printable(value, i) {
            special_characters = true;
        }
        if is_space(value, i) {
            if i == 0 {
                leading_space = true;
            }
            if i + w == value.len() {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(value, i) {
            line_breaks = true;
            if i == 0 {
                leading_break = true;
            }
            if i + w == value.len() {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded_by_whitespace = is_blankz(value, i);
        i += w;
    }
    let mut d = ScalarData {
        multiline: line_breaks,
        flow_plain_allowed: true,
        block_plain_allowed: true,
        single_quoted_allowed: true,
        block_allowed: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        d.flow_plain_allowed = false;
        d.block_plain_allowed = false;
    }
    if trailing_space {
        d.block_allowed = false;
    }
    if break_space {
        d.flow_plain_allowed = false;
        d.block_plain_allowed = false;
        d.single_quoted_allowed = false;
    }
    if space_break || special_characters {
        d.flow_plain_allowed = false;
        d.block_plain_allowed = false;
        d.single_quoted_allowed = false;
        d.block_allowed = false;
    }
    if line_breaks {
        d.flow_plain_allowed = false;
        d.block_plain_allowed = false;
    }
    if flow_indicators {
        d.flow_plain_allowed = false;
    }
    if block_indicators {
        d.block_plain_allowed = false;
    }
    d
}

/// A scalar as the encoder hands it to the emitter: its text and requested style.
struct Scalar {
    value: Vec<u8>,
    style: ScalarStyle,
}

fn scalar_of(v: &GoVal) -> Option<Scalar> {
    let plain = |s: String| Scalar {
        value: s.into_bytes(),
        style: ScalarStyle::Plain,
    };
    Some(match v {
        GoVal::Nil => plain("null".into()),
        GoVal::Bool(b) => plain(b.to_string()),
        GoVal::Int(i) => plain(i.to_string()),
        GoVal::Uint(u) => plain(u.to_string()),
        GoVal::Float(f) => {
            let s = gofmt::format_float(*f, b'g', false);
            plain(match s.as_str() {
                "+Inf" => ".inf".into(),
                "-Inf" => "-.inf".into(),
                "NaN" => ".nan".into(),
                _ => s,
            })
        }
        GoVal::Str(s) => {
            // stringv: strings that would resolve to another type are quoted.
            let style = if s.contains(&b'\n') {
                ScalarStyle::Literal
            } else if !needs_quotes(s) {
                ScalarStyle::Plain
            } else {
                ScalarStyle::DoubleQuoted
            };
            Scalar { value: s.clone(), style }
        }
        GoVal::Seq(_) | GoVal::Map(_) => return None,
    })
}

impl Emitter {
    fn put(&mut self, c: u8) {
        self.out.push(c);
        self.column += 1;
    }

    fn put_break(&mut self) {
        self.out.push(b'\n');
        self.column = 0;
    }

    fn write(&mut self, s: &[u8], i: &mut usize) {
        let w = width(at(s, *i)).max(1);
        if let Some(bytes) = s.get(*i..*i + w) {
            self.out.extend_from_slice(bytes);
        }
        self.column += 1;
        *i += w;
    }

    fn write_all(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() {
            self.write(s, &mut i);
        }
    }

    fn write_break(&mut self, s: &[u8], i: &mut usize) {
        if at(s, *i) == b'\n' {
            self.put_break();
            *i += 1;
        } else {
            self.write(s, i);
            self.column = 0;
        }
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if !indentless {
            self.indent += self.best_indent;
        }
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().unwrap_or(-1);
    }

    fn write_indent(&mut self) {
        let indent = self.indent.max(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break();
        }
        while self.column < indent {
            self.put(b' ');
        }
        self.whitespace = true;
        self.indention = true;
    }

    fn write_indicator(&mut self, indicator: &[u8], need_whitespace: bool, is_whitespace: bool, is_indention: bool) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        self.write_all(indicator);
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
        self.open_ended = false;
    }

    /// yaml_emitter_check_simple_key, for the node about to be emitted.
    fn check_simple_key(v: &GoVal) -> bool {
        match v {
            GoVal::Seq(items) => items.is_empty(),
            GoVal::Map(m) => m.entries.is_empty(),
            _ => match scalar_of(v) {
                Some(s) => {
                    let d = analyze_scalar(&s.value);
                    !d.multiline && s.value.len() <= 128
                }
                None => false,
            },
        }
    }

    fn emit_node(&mut self, v: &GoVal, root: bool, mapping: bool, simple_key: bool) {
        self.root_context = root;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match v {
            GoVal::Seq(items) => self.emit_sequence(items),
            GoVal::Map(m) => {
                let keys = sorted_keys(&m.entries);
                let entries: Vec<&(GoVal, GoVal)> = keys.iter().filter_map(|&i| m.entries.get(i)).collect();
                self.emit_mapping(&entries);
            }
            _ => {
                if let Some(s) = scalar_of(v) {
                    self.emit_scalar(&s);
                }
            }
        }
    }

    fn emit_sequence(&mut self, items: &[GoVal]) {
        if self.flow_level > 0 || items.is_empty() {
            self.write_indicator(b"[", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    self.write_indicator(b",", false, false, false);
                }
                if self.column > self.best_width {
                    self.write_indent();
                }
                self.emit_node(item, false, false, false);
            }
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"]", false, false, false);
            return;
        }
        let indentless = self.mapping_context && !self.indention;
        self.increase_indent(false, indentless);
        for item in items {
            self.write_indent();
            self.write_indicator(b"-", true, false, true);
            self.emit_node(item, false, false, false);
        }
        self.pop_indent();
    }

    fn emit_mapping(&mut self, entries: &[&(GoVal, GoVal)]) {
        if self.flow_level > 0 || entries.is_empty() {
            self.write_indicator(b"{", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
            for (n, (k, v)) in entries.iter().map(|e| (&e.0, &e.1)).enumerate() {
                if n > 0 {
                    self.write_indicator(b",", false, false, false);
                }
                if self.column > self.best_width {
                    self.write_indent();
                }
                if Self::check_simple_key(k) {
                    self.emit_node(k, false, true, true);
                    self.write_indicator(b":", false, false, false);
                } else {
                    self.write_indicator(b"?", true, false, false);
                    self.emit_node(k, false, true, false);
                    if self.column > self.best_width {
                        self.write_indent();
                    }
                    self.write_indicator(b":", true, false, false);
                }
                self.emit_node(v, false, true, false);
            }
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"}", false, false, false);
            return;
        }
        self.increase_indent(false, false);
        for (k, v) in entries.iter().map(|e| (&e.0, &e.1)) {
            self.write_indent();
            if Self::check_simple_key(k) {
                self.emit_node(k, false, true, true);
                self.write_indicator(b":", false, false, false);
            } else {
                self.write_indicator(b"?", true, false, true);
                self.emit_node(k, false, true, false);
                self.write_indent();
                self.write_indicator(b":", true, false, true);
            }
            self.emit_node(v, false, true, false);
        }
        self.pop_indent();
    }

    fn emit_scalar(&mut self, s: &Scalar) {
        let d = analyze_scalar(&s.value);
        // yaml_emitter_select_scalar_style (implicit, untagged scalars).
        let mut style = s.style;
        if style == ScalarStyle::Any {
            style = ScalarStyle::Plain;
        }
        if self.simple_key_context && d.multiline {
            style = ScalarStyle::DoubleQuoted;
        }
        if style == ScalarStyle::Plain {
            if (self.flow_level > 0 && !d.flow_plain_allowed) || (self.flow_level == 0 && !d.block_plain_allowed) {
                style = ScalarStyle::SingleQuoted;
            }
            if s.value.is_empty() && (self.flow_level > 0 || self.simple_key_context) {
                style = ScalarStyle::SingleQuoted;
            }
        }
        if style == ScalarStyle::SingleQuoted && !d.single_quoted_allowed {
            style = ScalarStyle::DoubleQuoted;
        }
        if (style == ScalarStyle::Literal || style == ScalarStyle::Folded)
            && (!d.block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = ScalarStyle::DoubleQuoted;
        }
        self.increase_indent(true, false);
        let allow_breaks = !self.simple_key_context;
        match style {
            ScalarStyle::SingleQuoted => self.write_single_quoted(&s.value, allow_breaks),
            ScalarStyle::DoubleQuoted => self.write_double_quoted(&s.value, allow_breaks),
            ScalarStyle::Literal | ScalarStyle::Folded => self.write_literal(&s.value),
            ScalarStyle::Plain | ScalarStyle::Any => self.write_plain(&s.value, allow_breaks),
        }
        self.pop_indent();
    }

    fn write_plain(&mut self, value: &[u8], allow_breaks: bool) {
        if !self.whitespace {
            self.put(b' ');
        }
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks && !spaces && self.column > self.best_width && !is_space(value, i + 1) {
                    self.write_indent();
                    i += width(at(value, i)).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && at(value, i) == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut i);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.whitespace = false;
        self.indention = false;
        if self.root_context {
            self.open_ended = true;
        }
    }

    fn write_single_quoted(&mut self, value: &[u8], allow_breaks: bool) {
        self.write_indicator(b"'", true, false, false);
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len() - 1
                    && !is_space(value, i + 1)
                {
                    self.write_indent();
                    i += width(at(value, i)).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && at(value, i) == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut i);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if at(value, i) == b'\'' {
                    self.put(b'\'');
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.write_indicator(b"'", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_double_quoted(&mut self, value: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false);
        let mut i = 0;
        while i < value.len() {
            let c = at(value, i);
            if !is_printable(value, i)
                || is_bom(value, i)
                || is_break(value, i)
                || c == b'"'
                || c == b'\\'
            {
                let (mut w, mut v): (usize, u32) = if c & 0x80 == 0 {
                    (1, u32::from(c & 0x7F))
                } else if c & 0xE0 == 0xC0 {
                    (2, u32::from(c & 0x1F))
                } else if c & 0xF0 == 0xE0 {
                    (3, u32::from(c & 0x0F))
                } else if c & 0xF8 == 0xF0 {
                    (4, u32::from(c & 0x07))
                } else {
                    (1, 0)
                };
                for k in 1..w {
                    v = (v << 6) + (u32::from(at(value, i + k)) & 0x3F);
                }
                i += w;
                self.put(b'\\');
                match v {
                    0x00 => self.put(b'0'),
                    0x07 => self.put(b'a'),
                    0x08 => self.put(b'b'),
                    0x09 => self.put(b't'),
                    0x0A => self.put(b'n'),
                    0x0B => self.put(b'v'),
                    0x0C => self.put(b'f'),
                    0x0D => self.put(b'r'),
                    0x1B => self.put(b'e'),
                    0x22 => self.put(b'"'),
                    0x5C => self.put(b'\\'),
                    0x85 => self.put(b'N'),
                    0xA0 => self.put(b'_'),
                    0x2028 => self.put(b'L'),
                    0x2029 => self.put(b'P'),
                    _ => {
                        if v <= 0xFF {
                            self.put(b'x');
                            w = 2;
                        } else if v <= 0xFFFF {
                            self.put(b'u');
                            w = 4;
                        } else {
                            self.put(b'U');
                            w = 8;
                        }
                        let mut k = i64::try_from((w - 1) * 4).unwrap_or(0);
                        while k >= 0 {
                            let digit = u8::try_from((v >> k) & 0x0F).unwrap_or(0);
                            self.put(if digit < 10 { digit + b'0' } else { digit + b'A' - 10 });
                            k -= 4;
                        }
                    }
                }
                spaces = false;
            } else if is_space(value, i) {
                if allow_breaks && !spaces && self.column > self.best_width && i > 0 && i < value.len() - 1 {
                    self.write_indent();
                    if is_space(value, i + 1) {
                        self.put(b'\\');
                    }
                    i += width(at(value, i)).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else {
                self.write(value, &mut i);
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_block_scalar_hints(&mut self, value: &[u8]) {
        if is_space(value, 0) || is_break(value, 0) {
            let hint = [b'0' + u8::try_from(self.best_indent).unwrap_or(2)];
            self.write_indicator(&hint, false, false, false);
        }
        self.open_ended = false;
        let mut chomp: Option<u8> = None;
        if value.is_empty() {
            chomp = Some(b'-');
        } else {
            let mut i = value.len() - 1;
            while i > 0 && at(value, i) & 0xC0 == 0x80 {
                i -= 1;
            }
            if !is_break(value, i) {
                chomp = Some(b'-');
            } else if i == 0 {
                chomp = Some(b'+');
                self.open_ended = true;
            } else {
                i -= 1;
                while i > 0 && at(value, i) & 0xC0 == 0x80 {
                    i -= 1;
                }
                if is_break(value, i) {
                    chomp = Some(b'+');
                    self.open_ended = true;
                }
            }
        }
        if let Some(c) = chomp {
            self.write_indicator(&[c], false, false, false);
        }
    }

    fn write_literal(&mut self, value: &[u8]) {
        self.write_indicator(b"|", true, false, false);
        self.write_block_scalar_hints(value);
        self.put_break();
        self.indention = true;
        self.whitespace = true;
        let mut breaks = true;
        let mut i = 0;
        while i < value.len() {
            if is_break(value, i) {
                self.write_break(value, &mut i);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                breaks = false;
            }
        }
    }
}

// ---- keyList's order (sorter.go) ----

fn class_re(pat: &str) -> Option<regex::Regex> {
    regex::Regex::new(pat).ok()
}

fn is_letter(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    static RE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    RE.get_or_init(|| class_re(r"^\p{L}$"))
        .as_ref()
        .is_some_and(|re| re.is_match(c.encode_utf8(&mut [0; 4])))
}

fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    static RE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    RE.get_or_init(|| class_re(r"^\p{Nd}$"))
        .as_ref()
        .is_some_and(|re| re.is_match(c.encode_utf8(&mut [0; 4])))
}

fn key_float(v: &GoVal) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    match v {
        GoVal::Int(i) => Some(*i as f64),
        GoVal::Uint(u) => Some(*u as f64),
        GoVal::Float(f) => Some(*f),
        GoVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// reflect.Kind's order, for keys of different kinds.
fn kind(v: &GoVal) -> u8 {
    match v {
        GoVal::Bool(_) => 1,
        GoVal::Int(_) => 2,
        GoVal::Uint(_) => 11,
        GoVal::Float(_) => 14,
        GoVal::Map(_) => 21,
        GoVal::Seq(_) => 23,
        GoVal::Str(_) => 24,
        GoVal::Nil => 20,
    }
}

fn digit_value(c: char) -> i64 {
    i64::from(u32::from(c)).wrapping_sub(i64::from(u32::from('0')))
}

/// keyList.Less.
fn key_less(a: &GoVal, b: &GoVal) -> bool {
    let (ak, bk) = (kind(a), kind(b));
    if let (Some(af), Some(bf)) = (key_float(a), key_float(b)) {
        if af != bf {
            return af < bf;
        }
        if ak != bk {
            return ak < bk;
        }
        return match (a, b) {
            (GoVal::Int(x), GoVal::Int(y)) => x < y,
            (GoVal::Uint(x), GoVal::Uint(y)) => x < y,
            (GoVal::Float(x), GoVal::Float(y)) => x < y,
            (GoVal::Bool(x), GoVal::Bool(y)) => !x && *y,
            _ => false,
        };
    }
    let (GoVal::Str(sa), GoVal::Str(sb)) = (a, b) else {
        return ak < bk;
    };
    let ar: Vec<char> = gofmt::lossy(sa).chars().collect();
    let br: Vec<char> = gofmt::lossy(sb).chars().collect();
    let n = ar.len().min(br.len());
    for i in 0..n {
        let (Some(&ca), Some(&cb)) = (ar.get(i), br.get(i)) else {
            break;
        };
        if ca == cb {
            continue;
        }
        let al = is_letter(ca);
        let bl = is_letter(cb);
        if al && bl {
            return ca < cb;
        }
        if al || bl {
            return bl;
        }
        let mut an: i64 = 0;
        let mut bn: i64 = 0;
        if ca == '0' || cb == '0' {
            let mut j = i;
            while j > 0 {
                j -= 1;
                let Some(&cj) = ar.get(j) else { break };
                if !is_digit(cj) {
                    break;
                }
                if cj != '0' {
                    an = 1;
                    bn = 1;
                    break;
                }
            }
        }
        let mut ai = i;
        while let Some(&c) = ar.get(ai).filter(|c| is_digit(**c)) {
            an = an.wrapping_mul(10).wrapping_add(digit_value(c));
            ai += 1;
        }
        let mut bi = i;
        while let Some(&c) = br.get(bi).filter(|c| is_digit(**c)) {
            bn = bn.wrapping_mul(10).wrapping_add(digit_value(c));
            bi += 1;
        }
        if an != bn {
            return an < bn;
        }
        if ai != bi {
            return ai < bi;
        }
        return ca < cb;
    }
    ar.len() < br.len()
}

/// The entries' indexes in keyList order (a merge sort: no comparator can make it fail).
fn sorted_keys(entries: &[(GoVal, GoVal)]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    let less = |x: usize, y: usize| match (entries.get(x), entries.get(y)) {
        (Some(a), Some(b)) => key_less(&a.0, &b.0),
        _ => false,
    };
    let mut buf = idx.clone();
    let mut width = 1;
    let n = idx.len();
    while width < n {
        let mut lo = 0;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while k < hi {
                let take_right = j < hi
                    && (i >= mid || less(idx.get(j).copied().unwrap_or(0), idx.get(i).copied().unwrap_or(0)));
                let src = if take_right {
                    j += 1;
                    j - 1
                } else {
                    i += 1;
                    i - 1
                };
                if let (Some(slot), Some(&v)) = (buf.get_mut(k), idx.get(src)) {
                    *slot = v;
                }
                k += 1;
            }
            lo = hi;
        }
        std::mem::swap(&mut idx, &mut buf);
        width *= 2;
    }
    idx
}

/// yaml.Marshal of a decoded value: one implicit document.
pub fn encode(v: &GoVal) -> Result<String, String> {
    let mut e = Emitter {
        out: Vec::new(),
        best_indent: 2,
        best_width: 80,
        indent: -1,
        indents: Vec::new(),
        flow_level: 0,
        root_context: false,
        mapping_context: false,
        simple_key_context: false,
        column: 0,
        whitespace: true,
        indention: true,
        open_ended: false,
    };
    e.emit_node(v, true, false, false);
    // DOCUMENT-END, implicit.
    e.write_indent();
    String::from_utf8(e.out).map_err(|_| "yaml: invalid UTF-8 output".to_string())
}
