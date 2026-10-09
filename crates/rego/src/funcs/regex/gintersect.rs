//! yashtewari/glob-intersection v0.2.0, as regex.globs_match calls it: whether two
//! globs of `.`, `[...]`, `+` and `*` (regex-like, not shell-like) match a common
//! string. Its errors are written by fmt.Errorf with the input inside the format, so
//! a `%` in the input is a verb to Go; `errorf` formats them as Go's fmt does.

/// A token's set of runes, as sorted, disjoint ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Set(Vec<(u32, u32)>);

impl Set {
    fn new(mut runes: Vec<(u32, u32)>) -> Set {
        runes.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(runes.len());
        for (lo, hi) in runes {
            match out.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => out.push((lo, hi)),
            }
        }
        Set(out)
    }

    fn contains(&self, r: u32) -> bool {
        self.0.iter().any(|&(lo, hi)| lo <= r && r <= hi)
    }

    fn intersects(&self, other: &Set) -> bool {
        self.0
            .iter()
            .any(|&(a, b)| other.0.iter().any(|&(c, d)| a <= d && c <= b))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Character(u32),
    Dot,
    Set(Set),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    None,
    Plus,
    Star,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    flag: Flag,
}

/// Token.Equal: the same type and character or set; flags aside.
fn equal(a: &Token, b: &Token) -> bool {
    match (&a.kind, &b.kind) {
        (Kind::Character(x), Kind::Character(y)) => x == y,
        (Kind::Dot, Kind::Dot) => true,
        (Kind::Set(x), Kind::Set(y)) => x == y,
        _ => false,
    }
}

/// Match: whether two tokens have a rune in common.
fn match_tokens(a: &Token, b: &Token) -> bool {
    match (&a.kind, &b.kind) {
        (Kind::Character(x), Kind::Character(y)) => x == y,
        (Kind::Character(_), Kind::Dot) | (Kind::Dot, Kind::Character(_)) => true,
        (Kind::Character(c), Kind::Set(s)) | (Kind::Set(s), Kind::Character(c)) => s.contains(*c),
        (Kind::Dot, _) | (_, Kind::Dot) => true,
        (Kind::Set(x), Kind::Set(y)) => x.intersects(y),
    }
}

const INVALID: &str = "the input provided is invalid";

/// An argument of the error's format: a string, or the wrapped ErrInvalidInput.
#[derive(Debug, Clone, Copy)]
enum Arg<'a> {
    Str(&'a str),
    Invalid,
}

/// invalidInputMessageErrorf.
fn invalid(input: &[u32], index: usize, message: &str, args: &[Arg<'_>]) -> String {
    let format = format!("input:{}, pos:{index}, {message}", runes_string(input));
    errorf(&format, args)
}

fn runes_string(r: &[u32]) -> String {
    r.iter()
        .map(|&c| char::from_u32(c).unwrap_or('\u{FFFD}'))
        .collect()
}

fn one(r: u32) -> String {
    char::from_u32(r).unwrap_or('\u{FFFD}').to_string()
}

/// What nextRune reads: a rune, whether it was escaped, and where the next starts.
enum Next {
    Rune(usize, u32, bool),
    End,
}

fn next_rune(index: usize, input: &[u32]) -> Result<Next, String> {
    let Some(&r) = input.get(index) else {
        return Ok(Next::End);
    };
    if r == u32::from('\\') {
        return match input.get(index + 1) {
            Some(&e) => Ok(Next::Rune(index + 2, e, true)),
            None => Err(invalid(
                input,
                index,
                "input ends with a \\ (escape) character: %w",
                &[Arg::Invalid],
            )),
        };
    }
    Ok(Next::Rune(index + 1, r, false))
}

fn tokenize(input: &[u32]) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut index = 0;
    loop {
        let (newindex, r, escaped) = match next_rune(index, input)? {
            Next::Rune(i, r, e) => (i, r, e),
            Next::End => return Ok(tokens),
        };
        let mut i = newindex;
        let kind = if escaped {
            Kind::Character(r)
        } else {
            match char::from_u32(r) {
                Some('.') => Kind::Dot,
                Some(']') => {
                    return Err(invalid(
                        input,
                        i,
                        "set-close ']' with no preceding '[': %w",
                        &[Arg::Invalid],
                    ));
                }
                Some('[') => {
                    let (next, set) = next_token_set(i, input)?;
                    i = next;
                    Kind::Set(set)
                }
                Some(c @ ('+' | '*')) => {
                    let s = c.to_string();
                    return Err(invalid(
                        input,
                        i,
                        "flag '%s' must be preceded by a non-flag: %w",
                        &[Arg::Str(&s), Arg::Invalid],
                    ));
                }
                _ => Kind::Character(r),
            }
        };
        // nextFlag.
        let mut flag = Flag::None;
        if let Next::Rune(after, f, false) = next_rune(i, input)? {
            if f == u32::from('+') {
                flag = Flag::Plus;
                i = after;
            } else if f == u32::from('*') {
                flag = Flag::Star;
                i = after;
            }
        }
        tokens.push(Token { kind, flag });
        index = i;
    }
}

fn next_token_set(index: usize, input: &[u32]) -> Result<(usize, Set), String> {
    let mut runes: Vec<(u32, u32)> = Vec::new();
    let (mut prev, mut prev_exists) = (0u32, false);
    let mut i = index;
    loop {
        let (newindex, r, escaped) = match next_rune(i, input)? {
            Next::Rune(n, r, e) => (n, r, e),
            Next::End => {
                return Err(invalid(
                    input,
                    i,
                    "found [ without matching ]: %w",
                    &[Arg::Invalid],
                ));
            }
        };
        i = newindex;
        if escaped {
            runes.push((r, r));
            prev = r;
            prev_exists = true;
            continue;
        }
        if r == u32::from('-') {
            if !prev_exists {
                return Err(invalid(
                    input,
                    i,
                    "range character '-' must be preceded by a Unicode character: %w",
                    &[Arg::Invalid],
                ));
            }
            if i + 1 >= input.len() {
                return Err(invalid(
                    input,
                    i,
                    "range character '-' must be followed by a Unicode character: %w",
                    &[Arg::Invalid],
                ));
            }
            let (n, hi, esc) = match next_rune(i, input) {
                Ok(Next::Rune(n, r, e)) => (n, r, e),
                _ => (0, 0, false),
            };
            i = n;
            if !esc && (hi == u32::from(']') || hi == u32::from('-')) {
                return Err(invalid(
                    input,
                    i,
                    "range character '-' cannot be followed by a special symbol: %w",
                    &[Arg::Invalid],
                ));
            }
            if hi < prev {
                let (a, b) = (one(hi), one(prev));
                return Err(invalid(
                    input,
                    i,
                    "range is out of order: '%s' comes before '%s' in Unicode: %w",
                    &[Arg::Str(&a), Arg::Str(&b), Arg::Invalid],
                ));
            }
            runes.push((prev, hi));
            prev_exists = false;
        } else if r == u32::from(']') {
            return Ok((i, Set::new(runes)));
        } else {
            runes.push((r, r));
            prev = r;
            prev_exists = true;
        }
    }
}

/// Simplify: adjacent flagged tokens that are equal become one.
fn simplify(tokens: Vec<Token>) -> Vec<Token> {
    let mut simple: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut latest: Option<usize> = None;
    for t in tokens {
        if let Some(l) = latest
            && let Some(last) = simple.get_mut(l)
            && t.flag != Flag::None
            && last.flag != Flag::None
            && equal(&t, last)
        {
            last.flag = if t.flag == Flag::Plus || last.flag == Flag::Plus {
                Flag::Plus
            } else {
                Flag::Star
            };
            continue;
        }
        simple.push(t);
        latest = Some(simple.len() - 1);
    }
    simple
}

fn new_glob(s: &str) -> Result<Vec<Token>, String> {
    let input: Vec<u32> = s.chars().map(u32::from).collect();
    Ok(simplify(tokenize(&input)?))
}

/// NonEmpty.
pub fn non_empty(lhs: &str, rhs: &str) -> Result<bool, String> {
    let g1 = new_glob(lhs)?;
    let g2 = new_glob(rhs)?;
    let Some((g1, g2)) = trim_globs(&g1, &g2) else {
        return Ok(false);
    };
    Ok(intersect_normal(g1, g2))
}

fn flag(g: &[Token], i: isize) -> Option<Flag> {
    usize::try_from(i).ok().and_then(|i| g.get(i)).map(|t| t.flag)
}

fn trim_globs<'a>(g1: &'a [Token], g2: &'a [Token]) -> Option<(&'a [Token], &'a [Token])> {
    let mut l = 0;
    while let (Some(a), Some(b)) = (g1.get(l), g2.get(l)) {
        if a.flag != Flag::None || b.flag != Flag::None {
            break;
        }
        if !match_tokens(a, b) {
            return None;
        }
        l += 1;
    }
    l = l.saturating_sub(1);
    let li = isize::try_from(l).ok()?;
    let mut r1 = isize::try_from(g1.len()).ok()? - 1;
    let mut r2 = isize::try_from(g2.len()).ok()? - 1;
    while r1 >= 0
        && r1 >= li
        && r2 >= 0
        && r2 >= li
        && flag(g1, r1) == Some(Flag::None)
        && flag(g2, r2) == Some(Flag::None)
    {
        let (a, b) = (
            g1.get(usize::try_from(r1).ok()?)?,
            g2.get(usize::try_from(r2).ok()?)?,
        );
        if !match_tokens(a, b) {
            return None;
        }
        r1 -= 1;
        r2 -= 1;
    }
    if r1 < isize::try_from(g1.len()).ok()? - 1 {
        r1 += 1;
        r2 += 1;
    }
    let e1 = usize::try_from(r1 + 1).ok()?;
    let e2 = usize::try_from(r2 + 1).ok()?;
    Some((g1.get(l..e1)?, g2.get(l..e2)?))
}

fn intersect_normal(g1: &[Token], g2: &[Token]) -> bool {
    let (mut i, mut j) = (0, 0);
    while let (Some(a), Some(b)) = (g1.get(i), g2.get(j)) {
        if a.flag == Flag::None && b.flag == Flag::None {
            if !match_tokens(a, b) {
                return false;
            }
        } else {
            return intersect_special(g1.get(i..).unwrap_or(&[]), g2.get(j..).unwrap_or(&[]));
        }
        i += 1;
        j += 1;
    }
    i == g1.len() && j == g2.len()
}

fn intersect_special(g1: &[Token], g2: &[Token]) -> bool {
    let (Some(a), Some(b)) = (g1.first(), g2.first()) else {
        return false;
    };
    if a.flag != Flag::None {
        match a.flag {
            Flag::Plus => intersect_plus(g1, g2),
            Flag::Star => intersect_star(g1, g2),
            Flag::None => false,
        }
    } else {
        match b.flag {
            Flag::Plus => intersect_plus(g2, g1),
            Flag::Star => intersect_star(g2, g1),
            Flag::None => false,
        }
    }
}

fn intersect_plus(plussed: &[Token], other: &[Token]) -> bool {
    let (Some(p), Some(o)) = (plussed.first(), other.first()) else {
        return false;
    };
    if !match_tokens(p, o) {
        return false;
    }
    if intersect_star(plussed, other.get(1..).unwrap_or(&[])) {
        return true;
    }
    o.flag != Flag::None && intersect_normal(plussed.get(1..).unwrap_or(&[]), other)
}

fn intersect_star(starred: &[Token], other: &[Token]) -> bool {
    let Some(star) = starred.first() else {
        return false;
    };
    let next = starred.get(1);
    for (i, t) in other.iter().enumerate() {
        if let Some(n) = next
            && match_tokens(t, n)
        {
            if intersect_normal(starred.get(1..).unwrap_or(&[]), other.get(i..).unwrap_or(&[])) {
                return true;
            }
            if !match_tokens(t, star) {
                return false;
            }
        } else if !match_tokens(t, star) {
            return false;
        }
    }
    next.is_none()
}

// fmt.Errorf, for the arguments these errors have: strings, and an error made by
// errors.New (a *errors.errorString).

#[derive(Debug, Default, Clone, Copy)]
struct Flags {
    sharp: bool,
    zero: bool,
    plus: bool,
    minus: bool,
    space: bool,
    sharp_v: bool,
    plus_v: bool,
    wid: Option<usize>,
    prec: Option<usize>,
}

fn pad(out: &mut String, f: &Flags, s: &str) {
    let Some(w) = f.wid.filter(|&w| w > 0) else {
        out.push_str(s);
        return;
    };
    let n = w.saturating_sub(s.chars().count());
    let fill = if f.zero && !f.minus { '0' } else { ' ' };
    if !f.minus {
        out.extend(std::iter::repeat_n(fill, n));
        out.push_str(s);
    } else {
        out.push_str(s);
        out.extend(std::iter::repeat_n(fill, n));
    }
}

fn truncate<'a>(f: &Flags, s: &'a str) -> &'a str {
    match f.prec {
        Some(p) => s.char_indices().nth(p).and_then(|(i, _)| s.get(..i)).unwrap_or(s),
        None => s,
    }
}

/// strconv.CanBackquote.
fn can_backquote(s: &str) -> bool {
    s.chars()
        .all(|c| c != '`' && c != '\u{FEFF}' && (c == '\t' || !(c < ' ' || c == '\u{7F}')))
}

fn quote(f: &Flags, s: &str) -> String {
    let mut q = String::new();
    if f.plus {
        // strconv.QuoteToASCII.
        q.push('"');
        for c in s.chars() {
            if c.is_ascii() {
                let mut one = String::new();
                crate::goquote::quote(&mut one, c.encode_utf8(&mut [0u8; 4]));
                q.push_str(one.get(1..one.len().saturating_sub(1)).unwrap_or(""));
            } else if u32::from(c) < 0x10000 {
                q.push_str(&format!("\\u{:04x}", u32::from(c)));
            } else {
                q.push_str(&format!("\\U{:08x}", u32::from(c)));
            }
        }
        q.push('"');
    } else {
        crate::goquote::quote(&mut q, s);
    }
    q
}

/// fmtString for a string's verb; false if the verb is not one for strings.
fn fmt_string(out: &mut String, f: &Flags, s: &str, verb: char) -> bool {
    match verb {
        'v' if f.sharp_v => pad(out, f, &quote(&Flags { plus: false, ..*f }, truncate(f, s))),
        'v' | 's' => pad(out, f, truncate(f, s)),
        'x' | 'X' => fmt_sx(out, f, s, verb == 'X'),
        'q' => {
            let s = truncate(f, s);
            if f.sharp && can_backquote(s) {
                pad(out, f, &format!("`{s}`"));
            } else {
                pad(out, f, &quote(f, s));
            }
        }
        _ => return false,
    }
    true
}

fn fmt_sx(out: &mut String, f: &Flags, s: &str, upper: bool) {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let x = if upper { 'X' } else { 'x' };
    let mut length = s.len();
    if let Some(p) = f.prec
        && p < length
    {
        length = p;
    }
    let mut width = 2 * length;
    if width > 0 {
        if f.space {
            if f.sharp {
                width *= 2;
            }
            width += length - 1;
        } else if f.sharp {
            width += 2;
        }
    } else {
        if let Some(w) = f.wid {
            pad(out, &Flags { wid: Some(w), ..*f }, "");
        }
        return;
    }
    let padding = |out: &mut String| {
        if let Some(w) = f.wid
            && w > width
        {
            let fill = if f.zero && !f.minus { '0' } else { ' ' };
            out.extend(std::iter::repeat_n(fill, w - width));
        }
    };
    if !f.minus {
        padding(out);
    }
    if f.sharp {
        out.push('0');
        out.push(x);
    }
    for (i, &c) in s.as_bytes().iter().take(length).enumerate() {
        if f.space && i > 0 {
            out.push(' ');
            if f.sharp {
                out.push('0');
                out.push(x);
            }
        }
        for d in [c >> 4, c & 0xF] {
            out.push(char::from(digits.get(usize::from(d)).copied().unwrap_or(b'0')));
        }
    }
    if f.minus {
        padding(out);
    }
}

/// printArg.
fn print_arg(out: &mut String, f: &Flags, arg: Arg<'_>, verb: char) {
    match verb {
        'T' => {
            let t = match arg {
                Arg::Str(_) => "string",
                Arg::Invalid => "*errors.errorString",
            };
            pad(out, f, truncate(f, t));
            return;
        }
        'p' => {
            out.push_str("%!p(");
            print_typed(out, f, arg);
            out.push(')');
            return;
        }
        _ => {}
    }
    match arg {
        Arg::Str(s) => {
            if !fmt_string(out, f, s, verb) {
                bad_verb(out, f, arg, verb);
            }
        }
        Arg::Invalid => {
            // handleMethods: %w wraps it; v, s, x, X and q print its Error().
            let verb = if verb == 'w' { 'v' } else { verb };
            if f.sharp_v {
                out.push_str("&errors.errorString{s:");
                fmt_string(out, f, INVALID, 'v');
                out.push('}');
            } else if matches!(verb, 'v' | 's' | 'x' | 'X' | 'q') {
                fmt_string(out, f, INVALID, verb);
            } else {
                // printValue: &{ field }, its field a string the verb does not fit.
                out.push_str("&{%!");
                out.push(verb);
                out.push_str("(string=");
                pad(out, f, truncate(f, INVALID));
                out.push_str(")}");
            }
        }
    }
}

fn print_typed(out: &mut String, f: &Flags, arg: Arg<'_>) {
    match arg {
        Arg::Str(s) => {
            out.push_str("string=");
            fmt_string(out, f, s, 'v');
        }
        Arg::Invalid => {
            out.push_str("*errors.errorString=");
            fmt_string(out, f, INVALID, 'v');
        }
    }
}

/// badVerb.
fn bad_verb(out: &mut String, f: &Flags, arg: Arg<'_>, verb: char) {
    out.push_str("%!");
    out.push(verb);
    out.push('(');
    print_typed(out, f, arg);
    out.push(')');
}

/// parsenum: a decimal number at byte i, or none.
fn parsenum(b: &[u8], start: usize, end: usize) -> (Option<usize>, usize) {
    if start >= end {
        return (None, end);
    }
    let mut num: usize = 0;
    let mut isnum = false;
    let mut i = start;
    while i < end && b.get(i).is_some_and(u8::is_ascii_digit) {
        if num >= 1_000_000 {
            return (None, end);
        }
        num = num * 10 + usize::from(b.get(i).copied().unwrap_or(b'0') - b'0');
        isnum = true;
        i += 1;
    }
    (isnum.then_some(num), i)
}

/// argNumber: a `[n]` at byte i.
fn arg_number(
    b: &[u8],
    arg_num: usize,
    i: usize,
    num_args: usize,
    reordered: &mut bool,
    good: &mut bool,
) -> (usize, usize, bool) {
    if b.get(i) != Some(&b'[') {
        return (arg_num, i, false);
    }
    *reordered = true;
    // parseArgNumber.
    let rest = b.get(i..).unwrap_or(&[]);
    let (index, wid, ok) = 'parse: {
        if rest.len() < 3 {
            break 'parse (None, 1, false);
        }
        for j in 1..rest.len() {
            if rest.get(j) == Some(&b']') {
                let (n, newi) = parsenum(rest, 1, j);
                match n {
                    Some(n) if newi == j => break 'parse (n.checked_sub(1), j + 1, true),
                    _ => break 'parse (None, j + 1, false),
                }
            }
        }
        (None, 1, false)
    };
    if ok && let Some(index) = index.filter(|&x| x < num_args) {
        return (index, i + wid, true);
    }
    *good = false;
    (arg_num, i + wid, ok)
}

/// fmt.Errorf(format, args...).Error().
fn errorf(format: &str, args: &[Arg<'_>]) -> String {
    let b = format.as_bytes();
    let end = b.len();
    let mut out = String::new();
    let mut arg_num = 0;
    let mut after_index;
    let mut reordered = false;
    let mut i = 0;
    'format: while i < end {
        let mut good = true;
        let lasti = i;
        while i < end && b.get(i) != Some(&b'%') {
            i += 1;
        }
        out.push_str(format.get(lasti..i).unwrap_or(""));
        if i >= end {
            break;
        }
        i += 1;
        let mut f = Flags::default();
        while let Some(&c) = b.get(i) {
            match c {
                b'#' => f.sharp = true,
                b'0' => f.zero = true,
                b'+' => f.plus = true,
                b'-' => f.minus = true,
                b' ' => f.space = true,
                _ => {
                    if c.is_ascii_lowercase()
                        && let Some(&a) = args.get(arg_num)
                    {
                        if c == b'v' || c == b'w' {
                            f.sharp_v = f.sharp;
                            f.sharp = false;
                            f.plus_v = f.plus;
                            f.plus = false;
                        }
                        print_arg(&mut out, &f, a, char::from(c));
                        arg_num += 1;
                        i += 1;
                        continue 'format;
                    }
                    break;
                }
            }
            i += 1;
        }
        (arg_num, i, after_index) = arg_number(b, arg_num, i, args.len(), &mut reordered, &mut good);
        if b.get(i) == Some(&b'*') {
            i += 1;
            // No argument here is an int.
            if arg_num < args.len() {
                arg_num += 1;
            }
            out.push_str("%!(BADWIDTH)");
            after_index = false;
        } else {
            let (w, ni) = parsenum(b, i, end);
            f.wid = w;
            i = ni;
            if after_index && f.wid.is_some() {
                good = false;
            }
        }
        if i + 1 < end && b.get(i) == Some(&b'.') {
            i += 1;
            if after_index {
                good = false;
            }
            (arg_num, i, after_index) = arg_number(b, arg_num, i, args.len(), &mut reordered, &mut good);
            if b.get(i) == Some(&b'*') {
                i += 1;
                if arg_num < args.len() {
                    arg_num += 1;
                }
                f.prec = None;
                out.push_str("%!(BADPREC)");
                after_index = false;
            } else {
                let (p, ni) = parsenum(b, i, end);
                f.prec = Some(p.unwrap_or(0));
                i = ni;
            }
        }
        if !after_index {
            (arg_num, i, _) = arg_number(b, arg_num, i, args.len(), &mut reordered, &mut good);
        }
        if i >= end {
            out.push_str("%!(NOVERB)");
            break;
        }
        let verb = format
            .get(i..)
            .and_then(|s| s.chars().next())
            .unwrap_or('\u{FFFD}');
        i += verb.len_utf8();
        if verb == '%' {
            out.push('%');
        } else if !good {
            out.push_str("%!");
            out.push(verb);
            out.push_str("(BADINDEX)");
        } else if let Some(&a) = args.get(arg_num) {
            if verb == 'v' || verb == 'w' {
                f.sharp_v = f.sharp;
                f.sharp = false;
                f.plus_v = f.plus;
                f.plus = false;
            }
            print_arg(&mut out, &f, a, verb);
            arg_num += 1;
        } else {
            out.push_str("%!");
            out.push(verb);
            out.push_str("(MISSING)");
        }
    }
    if !reordered && arg_num < args.len() {
        out.push_str("%!(EXTRA ");
        for (k, a) in args.iter().skip(arg_num).enumerate() {
            if k > 0 {
                out.push_str(", ");
            }
            print_typed(&mut out, &Flags::default(), *a);
        }
        out.push(')');
    }
    out
}
