//! Go's fmt (print.go, format.go) for the values templates hold: Sprint, Sprintln and
//! Sprintf, with the verbs b c d e E f F g G o O q s t T U v x X and the flags, widths,
//! precisions and argument indexes Go reads. `Value::Nil` is a nil interface.
//! Objects print as their `format` says for every verb but %T, with `&` before a pointer
//! at the top level; within lists and maps, where Go prints a pointer's address, they
//! print the same. Floats have no %x.

use crate::strconv;
use crate::value::{Kind, Value};

const LDIGITS: &[u8; 17] = b"0123456789abcdefx";
const UDIGITS: &[u8; 17] = b"0123456789ABCDEFX";

#[derive(Default, Clone, Copy)]
struct Flags {
    wid_present: bool,
    prec_present: bool,
    minus: bool,
    plus: bool,
    sharp: bool,
    space: bool,
    zero: bool,
    plus_v: bool,
    sharp_v: bool,
}

/// print.go's pp with format.go's fmt.
#[derive(Default)]
struct Printer {
    buf: String,
    f: Flags,
    wid: i64,
    prec: i64,
    reordered: bool,
    good_arg_num: bool,
    /// Whether pointers among the arguments print as what they point to, as exec.go's
    /// printableValue hands them to fmt.
    deref: bool,
}

fn digit(digits: &[u8; 17], i: u64) -> char {
    char::from(
        usize::try_from(i)
            .ok()
            .and_then(|i| digits.get(i))
            .copied()
            .unwrap_or(b'?'),
    )
}

/// Width and precision limit, print.go's tooLarge.
fn too_large(x: i64) -> bool {
    !(-1_000_000..=1_000_000).contains(&x)
}

impl Printer {
    fn clear_flags(&mut self) {
        self.f = Flags::default();
        self.wid = 0;
        self.prec = 0;
    }

    fn write_padding(&mut self, n: i64) {
        if n <= 0 {
            return;
        }
        let pad = if self.f.zero && !self.f.minus { '0' } else { ' ' };
        for _ in 0..n {
            self.buf.push(pad);
        }
    }

    fn pad(&mut self, s: &str) {
        if !self.f.wid_present || self.wid == 0 {
            self.buf.push_str(s);
            return;
        }
        let width = self.wid - i64::try_from(s.chars().count()).unwrap_or(0);
        if !self.f.minus {
            self.write_padding(width);
            self.buf.push_str(s);
        } else {
            self.buf.push_str(s);
            self.write_padding(width);
        }
    }

    fn fmt_boolean(&mut self, v: bool) {
        self.pad(if v { "true" } else { "false" });
    }

    fn fmt_unicode(&mut self, u: u64) {
        let mut prec: i64 = 4;
        if self.f.prec_present && self.prec > 4 {
            prec = self.prec;
        }
        let mut hex = String::new();
        let mut v = u;
        loop {
            hex.insert(0, digit(UDIGITS, v & 0xf));
            prec -= 1;
            v >>= 4;
            if v == 0 {
                break;
            }
        }
        while prec > 0 {
            hex.insert(0, '0');
            prec -= 1;
        }
        let mut s = format!("U+{hex}");
        if self.f.sharp
            && let Some(c) = u32::try_from(u).ok().and_then(char::from_u32)
            && strconv::is_print(c)
        {
            s.push_str(" '");
            s.push(c);
            s.push('\'');
        }
        let old = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old;
    }

    fn fmt_integer(&mut self, u: u64, base: u64, signed: bool, verb: char, digits: &[u8; 17]) {
        let negative = signed && (u as i64) < 0;
        let mut u = if negative { u.wrapping_neg() } else { u };
        let mut prec: i64 = 0;
        if self.f.prec_present {
            prec = self.prec;
            if prec == 0 && u == 0 {
                let old = self.f.zero;
                self.f.zero = false;
                self.write_padding(self.wid);
                self.f.zero = old;
                return;
            }
        } else if self.f.zero && !self.f.minus && self.f.wid_present {
            prec = self.wid;
            if negative || self.f.plus || self.f.space {
                prec -= 1;
            }
        }
        // Built backwards, as format.go fills its buffer from the end.
        let mut rev: Vec<char> = Vec::new();
        while u >= base {
            rev.push(digit(digits, u % base));
            u /= base;
        }
        rev.push(digit(digits, u));
        while i64::try_from(rev.len()).unwrap_or(i64::MAX) < prec {
            rev.push('0');
        }
        if self.f.sharp {
            match base {
                2 => rev.extend(['b', '0']),
                8 => {
                    if rev.last() != Some(&'0') {
                        rev.push('0');
                    }
                }
                16 => rev.extend([digit(digits, 16), '0']),
                _ => {}
            }
        }
        if verb == 'O' {
            rev.extend(['o', '0']);
        }
        if negative {
            rev.push('-');
        } else if self.f.plus {
            rev.push('+');
        } else if self.f.space {
            rev.push(' ');
        }
        let s: String = rev.into_iter().rev().collect();
        let old = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old;
    }

    fn truncate<'s>(&self, s: &'s str) -> &'s str {
        if self.f.prec_present {
            let n = usize::try_from(self.prec).unwrap_or(0);
            if let Some((i, _)) = s.char_indices().nth(n) {
                return s.get(..i).unwrap_or(s);
            }
        }
        s
    }

    fn fmt_s(&mut self, s: &str) {
        let s = self.truncate(s);
        self.pad(s);
    }

    /// format.go's fmtSbx for strings.
    fn fmt_sx(&mut self, s: &str, digits: &[u8; 17]) {
        let b = s.as_bytes();
        let mut length = i64::try_from(b.len()).unwrap_or(0);
        if self.f.prec_present && self.prec < length {
            length = self.prec;
        }
        let mut width = 2 * length;
        if width > 0 {
            if self.f.space {
                if self.f.sharp {
                    width *= 2;
                }
                width += length - 1;
            } else if self.f.sharp {
                width += 2;
            }
        } else {
            if self.f.wid_present {
                self.write_padding(self.wid);
            }
            return;
        }
        if self.f.wid_present && self.wid > width && !self.f.minus {
            self.write_padding(self.wid - width);
        }
        if self.f.sharp {
            self.buf.push('0');
            self.buf.push(digit(digits, 16));
        }
        for (i, &c) in b.iter().take(usize::try_from(length).unwrap_or(0)).enumerate() {
            if self.f.space && i > 0 {
                self.buf.push(' ');
                if self.f.sharp {
                    self.buf.push('0');
                    self.buf.push(digit(digits, 16));
                }
            }
            self.buf.push(digit(digits, u64::from(c >> 4)));
            self.buf.push(digit(digits, u64::from(c & 0xf)));
        }
        if self.f.wid_present && self.wid > width && self.f.minus {
            self.write_padding(self.wid - width);
        }
    }

    fn fmt_q(&mut self, s: &str) {
        let s = self.truncate(s);
        if self.f.sharp && strconv::can_backquote(s) {
            self.pad(&format!("`{s}`"));
            return;
        }
        let q = strconv::quote_with(s, self.f.plus);
        self.pad(&q);
    }

    fn rune(c: u64) -> char {
        u32::try_from(c)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{fffd}')
    }

    fn fmt_c(&mut self, c: u64) {
        let mut tmp = [0u8; 4];
        let s = Printer::rune(c).encode_utf8(&mut tmp).to_owned();
        self.pad(&s);
    }

    fn fmt_qc(&mut self, c: u64) {
        let q = strconv::quote_rune(Printer::rune(c), self.f.plus);
        self.pad(&q);
    }

    /// format.go's fmtFloat.
    fn fmt_float_digits(&mut self, v: f64, verb: u8, prec: i64) {
        let prec = if self.f.prec_present { self.prec } else { prec };
        let s = strconv::format_float(v, verb, prec);
        let mut num: Vec<u8> = Vec::with_capacity(s.len() + 1);
        if !(s.starts_with('-') || s.starts_with('+')) {
            num.push(b'+');
        }
        num.extend_from_slice(s.as_bytes());
        if self.f.space
            && !self.f.plus
            && let Some(first @ b'+') = num.first_mut()
        {
            *first = b' ';
        }
        if matches!(num.get(1), Some(b'I' | b'N')) {
            let old = self.f.zero;
            self.f.zero = false;
            if num.get(1) == Some(&b'N') && !self.f.space && !self.f.plus {
                num.remove(0);
            }
            self.pad(&String::from_utf8_lossy(&num));
            self.f.zero = old;
            return;
        }
        if self.f.sharp && verb != b'b' {
            let mut digits: i64 = 0;
            if matches!(verb, b'v' | b'g' | b'G' | b'x') {
                digits = prec;
                if digits == -1 {
                    digits = 6;
                }
            }
            let mut tail: Vec<u8> = Vec::new();
            let mut has_point = false;
            let mut saw_nonzero = false;
            let mut i = 1;
            while let Some(&c) = num.get(i) {
                match c {
                    b'.' => has_point = true,
                    b'p' | b'P' | b'e' | b'E' if c != b'e' && c != b'E' || verb != b'x' => {
                        tail.extend(num.drain(i..));
                        break;
                    }
                    _ => {
                        if c != b'0' {
                            saw_nonzero = true;
                        }
                        if saw_nonzero {
                            digits -= 1;
                        }
                    }
                }
                i += 1;
            }
            if !has_point {
                if num.len() == 2 && num.get(1) == Some(&b'0') {
                    digits -= 1;
                }
                num.push(b'.');
            }
            while digits > 0 {
                num.push(b'0');
                digits -= 1;
            }
            num.extend(tail);
        }
        let len = i64::try_from(num.len()).unwrap_or(0);
        if self.f.plus || num.first() != Some(&b'+') {
            if self.f.zero && !self.f.minus && self.f.wid_present && self.wid > len {
                if let Some(&sign) = num.first() {
                    self.buf.push(char::from(sign));
                }
                self.write_padding(self.wid - len);
                self.buf
                    .push_str(&String::from_utf8_lossy(num.get(1..).unwrap_or(&[])));
                return;
            }
            self.pad(&String::from_utf8_lossy(&num));
            return;
        }
        self.pad(&String::from_utf8_lossy(num.get(1..).unwrap_or(&[])));
    }

    fn fmt_bool(&mut self, v: &Value, b: bool, verb: char) {
        match verb {
            't' | 'v' => self.fmt_boolean(b),
            _ => self.bad_verb(verb, v),
        }
    }

    fn fmt_int(&mut self, v: &Value, u: u64, signed: bool, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v && !signed {
                    let sharp = self.f.sharp;
                    self.f.sharp = true;
                    self.fmt_integer(u, 16, false, 'v', LDIGITS);
                    self.f.sharp = sharp;
                } else {
                    self.fmt_integer(u, 10, signed, verb, LDIGITS);
                }
            }
            'd' => self.fmt_integer(u, 10, signed, verb, LDIGITS),
            'b' => self.fmt_integer(u, 2, signed, verb, LDIGITS),
            'o' | 'O' => self.fmt_integer(u, 8, signed, verb, LDIGITS),
            'x' => self.fmt_integer(u, 16, signed, verb, LDIGITS),
            'X' => self.fmt_integer(u, 16, signed, verb, UDIGITS),
            'c' => self.fmt_c(u),
            'q' => self.fmt_qc(u),
            'U' => self.fmt_unicode(u),
            _ => self.bad_verb(verb, v),
        }
    }

    fn fmt_float(&mut self, v: &Value, f: f64, verb: char) {
        match verb {
            'v' => self.fmt_float_digits(f, b'g', -1),
            'b' | 'g' | 'G' => self.fmt_float_digits(f, verb as u8, -1),
            'f' | 'e' | 'E' => self.fmt_float_digits(f, verb as u8, 6),
            'F' => self.fmt_float_digits(f, b'f', 6),
            _ => self.bad_verb(verb, v),
        }
    }

    fn fmt_string(&mut self, v: &Value, s: &str, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v {
                    self.fmt_q(s);
                } else {
                    self.fmt_s(s);
                }
            }
            's' => self.fmt_s(s),
            'x' => self.fmt_sx(s, LDIGITS),
            'X' => self.fmt_sx(s, UDIGITS),
            'q' => self.fmt_q(s),
            _ => self.bad_verb(verb, v),
        }
    }

    fn bad_verb(&mut self, verb: char, v: &Value) {
        self.buf.push_str("%!");
        self.buf.push(verb);
        self.buf.push('(');
        if v.is_nil() {
            self.buf.push_str("<nil>");
        } else {
            self.buf.push_str(&v.type_name());
            self.buf.push('=');
            self.print_arg(v, 'v');
        }
        self.buf.push(')');
    }

    fn print_arg(&mut self, arg: &Value, verb: char) {
        if arg.is_nil() {
            match verb {
                'T' | 'v' => self.pad("<nil>"),
                _ => self.bad_verb(verb, arg),
            }
            return;
        }
        match verb {
            'T' => {
                self.fmt_s(&arg.type_name());
                return;
            }
            'p' => {
                self.bad_verb(verb, arg);
                return;
            }
            _ => {}
        }
        // A pointer to a struct prints as &{...} at the top (print.go's printValue).
        if let Value::Object(o) = arg
            && !self.deref
            && o.type_name().starts_with('*')
        {
            self.buf.push('&');
        }
        self.print_value(arg, verb);
    }

    fn print_value(&mut self, v: &Value, verb: char) {
        match v {
            Value::Nil => {
                if self.f.sharp_v {
                    self.buf.push_str("interface {}(nil)");
                } else {
                    self.buf.push_str("<nil>");
                }
            }
            Value::Bool(b) => self.fmt_bool(v, *b, verb),
            Value::Int(i) => self.fmt_int(v, *i as u64, true, verb),
            Value::Uint(u) => self.fmt_int(v, *u, false, verb),
            Value::Float(f) => self.fmt_float(v, *f, verb),
            Value::String(s) => self.fmt_string(v, s, verb),
            Value::Object(o) => {
                let mut s = String::new();
                o.format(&mut s);
                self.buf.push_str(&s);
            }
            Value::Map(kind, entries) => {
                if self.f.sharp_v {
                    self.buf.push_str(&v.type_name());
                    self.buf.push('{');
                } else {
                    self.buf.push_str("map[");
                }
                for (i, (k, e)) in entries.iter().enumerate() {
                    if i > 0 {
                        if self.f.sharp_v {
                            self.buf.push_str(", ");
                        } else {
                            self.buf.push(' ');
                        }
                    }
                    self.print_value(&Value::String(k.clone()), verb);
                    self.buf.push(':');
                    self.print_elem(*kind, e, verb);
                }
                self.buf.push(if self.f.sharp_v { '}' } else { ']' });
            }
            Value::List(kind, items) => {
                if self.f.sharp_v {
                    self.buf.push_str(&v.type_name());
                    self.buf.push('{');
                    for (i, e) in items.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.print_elem(*kind, e, verb);
                    }
                    self.buf.push('}');
                } else {
                    self.buf.push('[');
                    for (i, e) in items.iter().enumerate() {
                        if i > 0 {
                            self.buf.push(' ');
                        }
                        self.print_elem(*kind, e, verb);
                    }
                    self.buf.push(']');
                }
            }
        }
    }

    /// An element of a list or map: an interface value when the kind is Any.
    fn print_elem(&mut self, kind: Kind, e: &Value, verb: char) {
        if kind == Kind::Any && e.is_nil() {
            if self.f.sharp_v {
                self.buf.push_str("interface {}(nil)");
            } else {
                self.buf.push_str("<nil>");
            }
            return;
        }
        self.print_value(e, verb);
    }

    fn do_print(&mut self, args: &[Value]) {
        let mut prev_string = false;
        for (i, arg) in args.iter().enumerate() {
            let is_string = matches!(arg, Value::String(_));
            if i > 0 && !is_string && !prev_string {
                self.buf.push(' ');
            }
            self.print_arg(arg, 'v');
            prev_string = is_string;
        }
    }

    fn do_println(&mut self, args: &[Value]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.buf.push(' ');
            }
            self.print_arg(arg, 'v');
        }
        self.buf.push('\n');
    }

    /// print.go's argNumber.
    fn arg_number(
        &mut self,
        arg_num: usize,
        format: &[u8],
        i: usize,
        num_args: usize,
    ) -> (usize, usize, bool) {
        if format.get(i) != Some(&b'[') {
            return (arg_num, i, false);
        }
        self.reordered = true;
        let (index, wid, ok) = parse_arg_number(format.get(i..).unwrap_or(&[]));
        if ok && index >= 0 && (index as usize) < num_args {
            return (index as usize, i + wid, true);
        }
        self.good_arg_num = false;
        (arg_num, i + wid, ok)
    }

    fn do_printf(&mut self, format: &str, a: &[Value]) {
        let fb = format.as_bytes();
        let end = fb.len();
        let mut arg_num: usize = 0;
        let mut after_index;
        self.reordered = false;
        let mut i = 0;
        'format: while i < end {
            self.good_arg_num = true;
            let lasti = i;
            while i < end && fb.get(i) != Some(&b'%') {
                i += 1;
            }
            if i > lasti {
                self.buf.push_str(format.get(lasti..i).unwrap_or(""));
            }
            if i >= end {
                break;
            }
            i += 1;
            self.clear_flags();
            while let Some(&c) = fb.get(i) {
                match c {
                    b'#' => self.f.sharp = true,
                    b'0' => self.f.zero = true,
                    b'+' => self.f.plus = true,
                    b'-' => self.f.minus = true,
                    b' ' => self.f.space = true,
                    _ => {
                        if c.is_ascii_lowercase() && arg_num < a.len() {
                            if c == b'v' {
                                self.f.sharp_v = self.f.sharp;
                                self.f.sharp = false;
                                self.f.plus_v = self.f.plus;
                                self.f.plus = false;
                            }
                            if let Some(arg) = a.get(arg_num) {
                                self.print_verb(arg, char::from(c));
                            }
                            arg_num += 1;
                            i += 1;
                            continue 'format;
                        }
                        break;
                    }
                }
                i += 1;
            }
            (arg_num, i, after_index) = self.arg_number(arg_num, fb, i, a.len());
            if fb.get(i) == Some(&b'*') {
                i += 1;
                let (n, ok, next) = int_from_arg(a, arg_num);
                self.wid = n;
                self.f.wid_present = ok;
                arg_num = next;
                if !ok {
                    self.buf.push_str("%!(BADWIDTH)");
                }
                if self.wid < 0 {
                    self.wid = -self.wid;
                    self.f.minus = true;
                    self.f.zero = false;
                }
                after_index = false;
            } else {
                let (n, ok, next) = parse_num(fb, i, end);
                self.wid = n;
                self.f.wid_present = ok;
                i = next;
                if after_index && self.f.wid_present {
                    self.good_arg_num = false;
                }
            }
            if i + 1 < end && fb.get(i) == Some(&b'.') {
                i += 1;
                if after_index {
                    self.good_arg_num = false;
                }
                (arg_num, i, after_index) = self.arg_number(arg_num, fb, i, a.len());
                if fb.get(i) == Some(&b'*') {
                    i += 1;
                    let (n, ok, next) = int_from_arg(a, arg_num);
                    self.prec = n;
                    self.f.prec_present = ok;
                    arg_num = next;
                    if self.prec < 0 {
                        self.prec = 0;
                        self.f.prec_present = false;
                    }
                    if !self.f.prec_present {
                        self.buf.push_str("%!(BADPREC)");
                    }
                    after_index = false;
                } else {
                    let (n, ok, next) = parse_num(fb, i, end);
                    self.prec = n;
                    self.f.prec_present = ok;
                    i = next;
                    if !self.f.prec_present {
                        self.prec = 0;
                        self.f.prec_present = true;
                    }
                }
            }
            if !after_index {
                (arg_num, i, _) = self.arg_number(arg_num, fb, i, a.len());
            }
            if i >= end {
                self.buf.push_str("%!(NOVERB)");
                break;
            }
            let verb = format
                .get(i..)
                .and_then(|s| s.chars().next())
                .unwrap_or('\u{fffd}');
            i += verb.len_utf8();
            match verb {
                '%' => self.buf.push('%'),
                _ if !self.good_arg_num => {
                    self.buf.push_str("%!");
                    self.buf.push(verb);
                    self.buf.push_str("(BADINDEX)");
                }
                _ if arg_num >= a.len() => {
                    self.buf.push_str("%!");
                    self.buf.push(verb);
                    self.buf.push_str("(MISSING)");
                }
                _ => {
                    if verb == 'v' {
                        self.f.sharp_v = self.f.sharp;
                        self.f.sharp = false;
                        self.f.plus_v = self.f.plus;
                        self.f.plus = false;
                    }
                    if let Some(arg) = a.get(arg_num) {
                        self.print_verb(arg, verb);
                    }
                    arg_num += 1;
                }
            }
        }
        if !self.reordered && arg_num < a.len() {
            self.clear_flags();
            self.buf.push_str("%!(EXTRA ");
            for (i, arg) in a.iter().skip(arg_num).enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                if arg.is_nil() {
                    self.buf.push_str("<nil>");
                } else {
                    self.buf.push_str(&arg.type_name());
                    self.buf.push('=');
                    self.print_arg(arg, 'v');
                }
            }
            self.buf.push(')');
        }
    }

    /// printArg, with `%w` as Sprintf reads it: a bad verb.
    fn print_verb(&mut self, arg: &Value, verb: char) {
        if verb == 'w' {
            self.bad_verb(verb, arg);
        } else {
            self.print_arg(arg, verb);
        }
    }
}

/// print.go's parsenum.
fn parse_num(s: &[u8], start: usize, end: usize) -> (i64, bool, usize) {
    if start >= end {
        return (0, false, end);
    }
    let mut num: i64 = 0;
    let mut isnum = false;
    let mut i = start;
    while i < end {
        let Some(&c) = s.get(i) else { break };
        if !c.is_ascii_digit() {
            break;
        }
        if too_large(num) {
            return (0, false, end);
        }
        num = num * 10 + i64::from(c - b'0');
        isnum = true;
        i += 1;
    }
    (num, isnum, i)
}

/// print.go's parseArgNumber: the zero-based index, the bytes read, and whether it is
/// well formed.
fn parse_arg_number(format: &[u8]) -> (i64, usize, bool) {
    if format.len() < 3 {
        return (0, 1, false);
    }
    for i in 1..format.len() {
        if format.get(i) == Some(&b']') {
            let (width, ok, newi) = parse_num(format, 1, i);
            if !ok || newi != i {
                return (0, i + 1, false);
            }
            return (width - 1, i + 1, true);
        }
    }
    (0, 1, false)
}

/// print.go's intFromArg.
fn int_from_arg(a: &[Value], arg_num: usize) -> (i64, bool, usize) {
    let Some(arg) = a.get(arg_num) else {
        return (0, false, arg_num);
    };
    let (mut num, mut is_int) = match arg {
        Value::Int(i) => (*i, true),
        Value::Uint(u) => match i64::try_from(*u) {
            Ok(n) => (n, true),
            Err(_) => (0, false),
        },
        _ => (0, false),
    };
    if too_large(num) {
        num = 0;
        is_int = false;
    }
    (num, is_int, arg_num + 1)
}

/// `fmt.Sprint`.
pub(crate) fn sprint(args: &[Value]) -> String {
    let mut p = Printer::default();
    p.do_print(args);
    p.buf
}

/// `fmt.Sprint` of values exec.go's printableValue made printable: pointers followed.
pub(crate) fn sprint_printable(args: &[Value]) -> String {
    let mut p = Printer {
        deref: true,
        ..Printer::default()
    };
    p.do_print(args);
    p.buf
}

/// `fmt.Sprintln`.
pub(crate) fn sprintln(args: &[Value]) -> String {
    let mut p = Printer::default();
    p.do_println(args);
    p.buf
}

/// `fmt.Sprintf`.
pub(crate) fn sprintf(format: &str, args: &[Value]) -> String {
    let mut p = Printer::default();
    p.do_printf(format, args);
    p.buf
}

/// `fmt.Sprintf("%#U", r)`, as the lexer's errors print characters.
pub(crate) fn sharp_u(c: char) -> String {
    sprintf("%#U", &[Value::Int(i64::from(u32::from(c)))])
}

/// `fmt.Sprintf("%.10q", s)`.
pub(crate) fn quote_prefix(s: &str, runes: usize) -> String {
    let end = s.char_indices().nth(runes).map_or(s.len(), |(i, _)| i);
    strconv::quote(s.get(..end).unwrap_or(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_formats_as_go_formats() {
        let s = |v: &str| Value::String(v.into());
        assert_eq!(
            sprintf("%-5s|%5s|%.2s", &[s("ab"), s("cd"), s("xyz")]),
            "ab   |   cd|xy"
        );
        assert_eq!(
            sprintf(
                "%05d %x %X %o %b",
                &[
                    Value::Int(-42),
                    Value::Int(255),
                    Value::Int(255),
                    Value::Int(8),
                    Value::Int(5)
                ]
            ),
            "-0042 ff FF 10 101"
        );
        assert_eq!(sprintf("%d", &[s("a")]), "%!d(string=a)");
        assert_eq!(sprintf("%d %d", &[Value::Int(1)]), "1 %!d(MISSING)");
        assert_eq!(sprintf("%d", &[Value::Int(1), Value::Int(2)]), "1%!(EXTRA int=2)");
        assert_eq!(
            sprintf(
                "%8.3f|%e|%g",
                &[Value::Float(3.14259), Value::Float(1234.5678), Value::Float(1e21)]
            ),
            "   3.143|1.234568e+03|1e+21"
        );
        assert_eq!(
            sprint(&[Value::Int(1), Value::Int(2), s("a"), Value::Int(3)]),
            "1 2a3"
        );
        assert_eq!(sharp_u('€'), "U+20AC '€'");
    }
}
