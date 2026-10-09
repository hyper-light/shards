//! URLs as Go 1.26's `net/url` parses and prints them, for the remote sources of `ADD`:
//! `Parse` with its errors, `URL.String`, queries (`ParseQuery`, `Values.Encode`) and
//! escaping. Hosts in brackets are checked as `netip.ParseAddr` checks them. Colons in a
//! host are strict, Go 1.26's default (`urlstrictcolons=1`).

use std::collections::BTreeMap;

use crate::go;

/// Go's `url.Values`: each key's values, in the order given.
pub type Values = BTreeMap<Vec<u8>, Vec<Vec<u8>>>;

/// `url.Userinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Userinfo {
    pub username: Vec<u8>,
    pub password: Option<Vec<u8>>,
}

/// `url.URL`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Url {
    pub scheme: Vec<u8>,
    pub opaque: Vec<u8>,
    pub user: Option<Userinfo>,
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    pub raw_path: Vec<u8>,
    pub omit_host: bool,
    pub force_query: bool,
    pub raw_query: Vec<u8>,
    pub fragment: Vec<u8>,
    pub raw_fragment: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Path,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

/// `shouldEscape`.
fn should_escape(c: u8, mode: Mode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if matches!(mode, Mode::Host | Mode::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => return c == b'?',
            Mode::UserPassword => return matches!(c, b'@' | b'/' | b'?' | b':'),
            Mode::QueryComponent => return true,
            Mode::Fragment => return false,
            Mode::Host | Mode::Zone => {}
        },
        _ => {}
    }
    if mode == Mode::Fragment && matches!(c, b'!' | b'(' | b')' | b'*') {
        return false;
    }
    true
}

fn unhex(c: u8) -> u8 {
    char::from(c)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
        .unwrap_or(0)
}

/// `unescape`, failing with `EscapeError` or `InvalidHostError`'s text.
fn unescape(s: &[u8], mode: Mode) -> Result<Vec<u8>, Vec<u8>> {
    let escape_error = |e: &[u8]| [b"invalid URL escape ".as_slice(), go::quote(e).as_bytes()].concat();
    let mut n = 0;
    let mut has_plus = false;
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        match c {
            b'%' => {
                n += 1;
                let hex = |j: usize| s.get(j).is_some_and(u8::is_ascii_hexdigit);
                if i + 2 >= s.len() || !hex(i + 1) || !hex(i + 2) {
                    let rest = go::tail(s, i);
                    return Err(escape_error(go::head(rest, 3)));
                }
                let (h, l) = (
                    s.get(i + 1).copied().unwrap_or(0),
                    s.get(i + 2).copied().unwrap_or(0),
                );
                let three = go::span(s, i, i + 3);
                if mode == Mode::Host && unhex(h) < 8 && three != b"%25" {
                    return Err(escape_error(three));
                }
                if mode == Mode::Zone {
                    let v = (unhex(h) << 4) | unhex(l);
                    if three != b"%25" && v != b' ' && should_escape(v, Mode::Host) {
                        return Err(escape_error(three));
                    }
                }
                i += 3;
            }
            b'+' => {
                has_plus = mode == Mode::QueryComponent;
                i += 1;
            }
            _ => {
                if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && should_escape(c, mode) {
                    return Err([
                        b"invalid character ".as_slice(),
                        go::quote(&[c]).as_bytes(),
                        b" in host name",
                    ]
                    .concat());
                }
                i += 1;
            }
        }
    }
    if n == 0 && !has_plus {
        return Ok(s.to_vec());
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        match c {
            b'%' => {
                let h = s.get(i + 1).copied().unwrap_or(0);
                let l = s.get(i + 2).copied().unwrap_or(0);
                out.push((unhex(h) << 4) | unhex(l));
                i += 3;
            }
            b'+' => {
                out.push(if mode == Mode::QueryComponent { b' ' } else { b'+' });
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// `escape`.
fn escape(s: &[u8], mode: Mode) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if c == b' ' && mode == Mode::QueryComponent {
            out.push(b'+');
        } else if should_escape(c, mode) {
            out.push(b'%');
            out.push(HEX.get(usize::from(c >> 4)).copied().unwrap_or(b'0'));
            out.push(HEX.get(usize::from(c & 15)).copied().unwrap_or(b'0'));
        } else {
            out.push(c);
        }
    }
    out
}

/// `QueryUnescape`.
pub fn query_unescape(s: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    unescape(s, Mode::QueryComponent)
}

/// `QueryEscape`.
pub fn query_escape(s: &[u8]) -> Vec<u8> {
    escape(s, Mode::QueryComponent)
}

/// `getScheme`.
fn get_scheme(raw: &[u8]) -> Result<(Vec<u8>, &[u8]), Vec<u8>> {
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Ok((Vec::new(), raw));
                }
            }
            b':' => {
                if i == 0 {
                    return Err(b"missing protocol scheme".to_vec());
                }
                return Ok((go::head(raw, i).to_vec(), go::tail(raw, i + 1)));
            }
            _ => return Ok((Vec::new(), raw)),
        }
    }
    Ok((Vec::new(), raw))
}

fn cut(s: &[u8], sep: u8) -> (&[u8], &[u8], bool) {
    match s.iter().position(|&b| b == sep) {
        Some(i) => (go::head(s, i), go::tail(s, i + 1), true),
        None => (s, b"", false),
    }
}

/// `Parse`: with a fragment after `#`. Fails with `url.Error`'s text, `parse "<url>": <why>`.
pub fn parse(raw: &[u8]) -> Result<Url, Vec<u8>> {
    let wrap = |u: &[u8], e: Vec<u8>| [b"parse ".as_slice(), go::quote(u).as_bytes(), b": ", &e].concat();
    let (u, frag, _) = cut(raw, b'#');
    let mut url = parse_inner(u).map_err(|e| wrap(u, e))?;
    if frag.is_empty() {
        return Ok(url);
    }
    let f = unescape(frag, Mode::Fragment).map_err(|e| wrap(raw, e))?;
    url.raw_fragment = if escape(&f, Mode::Fragment) == frag {
        Vec::new()
    } else {
        frag.to_vec()
    };
    url.fragment = f;
    Ok(url)
}

fn parse_inner(raw: &[u8]) -> Result<Url, Vec<u8>> {
    if raw.iter().any(|&b| b < b' ' || b == 0x7f) {
        return Err(b"net/url: invalid control character in URL".to_vec());
    }
    let mut url = Url::default();
    if raw == b"*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }
    let (scheme, mut rest) = get_scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();
    let rest_owned;
    if rest.ends_with(b"?") && rest.iter().filter(|&&b| b == b'?').count() == 1 {
        url.force_query = true;
        rest = go::head(rest, rest.len() - 1);
    } else {
        let (r, q, _) = cut(rest, b'?');
        url.raw_query = q.to_vec();
        rest_owned = r.to_vec();
        rest = &rest_owned;
    }
    if !rest.starts_with(b"/") {
        if !url.scheme.is_empty() {
            url.opaque = rest.to_vec();
            return Ok(url);
        }
        let (segment, _, _) = cut(rest, b'/');
        if segment.contains(&b':') {
            return Err(b"first path segment in URL cannot contain colon".to_vec());
        }
    }
    let mut path = rest;
    if (!url.scheme.is_empty() || !rest.starts_with(b"///")) && rest.starts_with(b"//") {
        let after = go::tail(rest, 2);
        let (authority, p) = match after.iter().position(|&b| b == b'/') {
            Some(i) => (go::head(after, i), go::tail(after, i)),
            None => (after, &b""[..]),
        };
        let (user, host) = parse_authority(&url.scheme, authority)?;
        url.user = user;
        url.host = host;
        path = p;
    } else if !url.scheme.is_empty() && rest.starts_with(b"/") {
        url.omit_host = true;
    }
    let p = unescape(path, Mode::Path)?;
    url.raw_path = if escape(&p, Mode::Path) == path {
        Vec::new()
    } else {
        path.to_vec()
    };
    url.path = p;
    Ok(url)
}

fn valid_userinfo(s: &[u8]) -> bool {
    go::runes(s).all(|(r, _)| {
        char::from_u32(r).is_some_and(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '.'
                        | '_'
                        | ':'
                        | '~'
                        | '!'
                        | '$'
                        | '&'
                        | '\''
                        | '('
                        | ')'
                        | '*'
                        | '+'
                        | ','
                        | ';'
                        | '='
                        | '%'
                        | '@'
                )
        })
    })
}

fn parse_authority(scheme: &[u8], authority: &[u8]) -> Result<(Option<Userinfo>, Vec<u8>), Vec<u8>> {
    let at = authority.iter().rposition(|&b| b == b'@');
    let host = match at {
        None => parse_host(scheme, authority)?,
        Some(i) => parse_host(scheme, go::tail(authority, i + 1))?,
    };
    let Some(i) = at else {
        return Ok((None, host));
    };
    let userinfo = go::head(authority, i);
    if !valid_userinfo(userinfo) {
        return Err(b"net/url: invalid userinfo".to_vec());
    }
    let user = if !userinfo.contains(&b':') {
        Userinfo {
            username: unescape(userinfo, Mode::UserPassword)?,
            password: None,
        }
    } else {
        let (u, p, _) = cut(userinfo, b':');
        Userinfo {
            username: unescape(u, Mode::UserPassword)?,
            password: Some(unescape(p, Mode::UserPassword)?),
        }
    };
    Ok((Some(user), host))
}

fn valid_optional_port(port: &[u8]) -> bool {
    match port.split_first() {
        None => true,
        Some((&b':', digits)) => digits.iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

fn parse_host(_scheme: &[u8], host: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let invalid_port = |p: &[u8]| {
        [
            b"invalid port ".as_slice(),
            go::quote(p).as_bytes(),
            b" after host",
        ]
        .concat()
    };
    match host.iter().rposition(|&b| b == b'[') {
        Some(i) if i > 0 => return Err(b"invalid IP-literal".to_vec()),
        Some(_) => {
            let Some(close) = host.iter().rposition(|&b| b == b']') else {
                return Err(b"missing ']' in host".to_vec());
            };
            let colon_port = go::tail(host, close + 1);
            if !valid_optional_port(colon_port) {
                return Err(invalid_port(colon_port));
            }
            let colon_port = unescape(colon_port, Mode::Host)?;
            let hostname = go::span(host, 1, close);
            let unescaped = match find(hostname, b"%25") {
                Some(z) => {
                    let mut h = unescape(go::head(hostname, z), Mode::Host)?;
                    h.extend(unescape(go::tail(hostname, z), Mode::Zone)?);
                    h
                }
                None => unescape(hostname, Mode::Host)?,
            };
            match parse_addr(&unescaped) {
                Err(e) => return Err([b"invalid host: ".as_slice(), &e].concat()),
                Ok(true) => return Err(b"invalid IP-literal".to_vec()),
                Ok(false) => {}
            }
            return Ok([b"[".as_slice(), &unescaped, b"]", &colon_port].concat());
        }
        None => {
            if let Some(i) = host.iter().position(|&b| b == b':') {
                // Strict: the first colon starts the port, which must be digits alone.
                let colon_port = go::tail(host, i);
                if !valid_optional_port(colon_port) {
                    return Err(invalid_port(colon_port));
                }
            }
        }
    }
    unescape(host, Mode::Host)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Makes `ParseAddr`'s error: the reason, and the text it stopped at.
type AddrFail<'a> = &'a dyn Fn(&str, Option<&[u8]>) -> Vec<u8>;

/// `netip.ParseAddr`: whether the address is IPv4, or `ParseAddr`'s error.
pub fn parse_addr(s: &[u8]) -> Result<bool, Vec<u8>> {
    let fail = |msg: &str, at: Option<&[u8]>| {
        let mut e = [
            b"ParseAddr(".as_slice(),
            go::quote(s).as_bytes(),
            b"): ",
            msg.as_bytes(),
        ]
        .concat();
        if let Some(at) = at.filter(|a| !a.is_empty()) {
            e.extend_from_slice(b" (at ");
            e.extend_from_slice(go::quote(at).as_bytes());
            e.push(b')');
        }
        e
    };
    for &c in s {
        match c {
            b'.' => {
                ipv4_fields(s, s, &fail)?;
                return Ok(true);
            }
            b':' => {
                parse_ipv6(s, &fail)?;
                return Ok(false);
            }
            b'%' => return Err(fail("missing IPv6 address", None)),
            _ => {}
        }
    }
    Err(fail("unable to parse IP", None))
}

/// `parseIPv4Fields` over `part`, a tail of `whole`.
fn ipv4_fields(_whole: &[u8], part: &[u8], fail: AddrFail<'_>) -> Result<(), Vec<u8>> {
    let (mut val, mut pos, mut dig_len) = (0u32, 0, 0);
    for (i, &c) in part.iter().enumerate() {
        if c.is_ascii_digit() {
            if dig_len == 1 && val == 0 {
                return Err(fail("IPv4 field has octet with leading zero", None));
            }
            val = val * 10 + u32::from(c - b'0');
            dig_len += 1;
            if val > 255 {
                return Err(fail("IPv4 field has value >255", None));
            }
        } else if c == b'.' {
            if i == 0 || i == part.len() - 1 || part.get(i - 1) == Some(&b'.') {
                return Err(fail(
                    "IPv4 field must have at least one digit",
                    Some(go::tail(part, i)),
                ));
            }
            if pos == 3 {
                return Err(fail("IPv4 address too long", None));
            }
            pos += 1;
            val = 0;
            dig_len = 0;
        } else {
            return Err(fail("unexpected character", Some(go::tail(part, i))));
        }
    }
    if pos < 3 {
        return Err(fail("IPv4 address too short", None));
    }
    Ok(())
}

/// `parseIPv6`'s checks.
fn parse_ipv6(input: &[u8], fail: AddrFail<'_>) -> Result<(), Vec<u8>> {
    let (mut s, zone) = match input.iter().position(|&b| b == b'%') {
        Some(i) => {
            let zone = go::tail(input, i + 1);
            if zone.is_empty() {
                return Err(fail("zone must be a non-empty string", None));
            }
            (go::head(input, i), zone)
        }
        None => (input, &b""[..]),
    };
    let mut ellipsis: i64 = -1;
    if s.starts_with(b"::") {
        ellipsis = 0;
        s = go::tail(s, 2);
        if s.is_empty() {
            return Ok(());
        }
    }
    let mut i = 0i64;
    while i < 16 {
        let mut off = 0;
        let mut acc: u32 = 0;
        while let Some(&c) = s.get(off) {
            let d = match c {
                b'0'..=b'9' => u32::from(c - b'0'),
                b'a'..=b'f' => u32::from(c - b'a' + 10),
                b'A'..=b'F' => u32::from(c - b'A' + 10),
                _ => break,
            };
            acc = (acc << 4) + d;
            if off > 3 {
                return Err(fail("each group must have 4 or less digits", Some(s)));
            }
            if acc > 0xFFFF {
                return Err(fail("IPv6 field has value >=2^16", Some(s)));
            }
            off += 1;
        }
        if off == 0 {
            return Err(fail(
                "each colon-separated field must have at least one digit",
                Some(s),
            ));
        }
        if s.get(off) == Some(&b'.') {
            if ellipsis < 0 && i != 12 {
                return Err(fail(
                    "embedded IPv4 address must replace the final 2 fields of the address",
                    Some(s),
                ));
            }
            if i + 4 > 16 {
                return Err(fail(
                    "too many hex fields to fit an embedded IPv4 at the end of the address",
                    Some(s),
                ));
            }
            let end = input.len() - if zone.is_empty() { 0 } else { zone.len() + 1 };
            ipv4_fields(input, go::span(input, end - s.len(), end), fail)?;
            s = b"";
            i += 4;
            break;
        }
        i += 2;
        s = go::tail(s, off);
        if s.is_empty() {
            break;
        }
        if s.first() != Some(&b':') {
            return Err(fail("unexpected character, want colon", Some(s)));
        } else if s.len() == 1 {
            return Err(fail("colon must be followed by more characters", Some(s)));
        }
        s = go::tail(s, 1);
        if s.first() == Some(&b':') {
            if ellipsis >= 0 {
                return Err(fail("multiple :: in address", Some(s)));
            }
            ellipsis = i;
            s = go::tail(s, 1);
            if s.is_empty() {
                break;
            }
        }
    }
    if !s.is_empty() {
        return Err(fail("trailing garbage after address", Some(s)));
    }
    if i < 16 {
        if ellipsis < 0 {
            return Err(fail("address string too short", None));
        }
    } else if ellipsis >= 0 {
        return Err(fail("the :: must expand to at least one field of zeros", None));
    }
    Ok(())
}

impl Url {
    /// `EscapedPath`.
    fn escaped_path(&self) -> Vec<u8> {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Mode::Path)
            && unescape(&self.raw_path, Mode::Path).as_deref() == Ok(self.path.as_slice())
        {
            return self.raw_path.clone();
        }
        if self.path == b"*" {
            return b"*".to_vec();
        }
        escape(&self.path, Mode::Path)
    }

    /// `EscapedFragment`.
    fn escaped_fragment(&self) -> Vec<u8> {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Mode::Fragment)
            && unescape(&self.raw_fragment, Mode::Fragment).as_deref() == Ok(self.fragment.as_slice())
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Mode::Fragment)
    }

    /// `URL.String`.
    pub fn string(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        if !self.scheme.is_empty() {
            buf.extend_from_slice(&self.scheme);
            buf.push(b':');
        }
        if !self.opaque.is_empty() {
            buf.extend_from_slice(&self.opaque);
        } else {
            if (!self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some())
                && !(self.omit_host && self.host.is_empty() && self.user.is_none())
            {
                if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                    buf.extend_from_slice(b"//");
                }
                if let Some(u) = &self.user {
                    buf.extend_from_slice(&userinfo_string(u));
                    buf.push(b'@');
                }
                if !self.host.is_empty() {
                    buf.extend_from_slice(&escape(&self.host, Mode::Host));
                }
            }
            let path = self.escaped_path();
            if path.first().is_some_and(|&c| c != b'/') && !self.host.is_empty() {
                buf.push(b'/');
            }
            if buf.is_empty() {
                let (segment, _, _) = cut(&path, b'/');
                if segment.contains(&b':') {
                    buf.extend_from_slice(b"./");
                }
            }
            buf.extend_from_slice(&path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            buf.push(b'#');
            buf.extend_from_slice(&self.escaped_fragment());
        }
        buf
    }

    /// `URL.Query`: `ParseQuery`'s values, its error ignored.
    pub fn query(&self) -> Values {
        parse_query(&self.raw_query).0
    }
}

/// `Userinfo.String`.
pub fn userinfo_string(u: &Userinfo) -> Vec<u8> {
    let mut s = escape(&u.username, Mode::UserPassword);
    if let Some(p) = &u.password {
        s.push(b':');
        s.extend(escape(p, Mode::UserPassword));
    }
    s
}

fn valid_encoded(s: &[u8], mode: Mode) -> bool {
    s.iter().all(|&c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

/// `ParseQuery`: the values, and the first error if any part was malformed.
pub fn parse_query(query: &[u8]) -> (Values, Option<Vec<u8>>) {
    let mut m = Values::new();
    let mut err: Option<Vec<u8>> = None;
    if query.iter().filter(|&&b| b == b'&').count() + 1 > 10_000 {
        return (m, Some(b"number of URL query parameters exceeded limit".to_vec()));
    }
    for part in query.split(|&b| b == b'&') {
        if query.is_empty() {
            break;
        }
        if part.contains(&b';') {
            err = Some(b"invalid semicolon separator in query".to_vec());
            continue;
        }
        if part.is_empty() {
            continue;
        }
        let (k, v, _) = cut(part, b'=');
        let k = match query_unescape(k) {
            Ok(k) => k,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        let v = match query_unescape(v) {
            Ok(v) => v,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        m.entry(k).or_default().push(v);
    }
    (m, err)
}

/// `Values.Encode`: sorted by key.
pub fn encode(v: &Values) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    for (k, vs) in v {
        let key = query_escape(k);
        for val in vs {
            if !buf.is_empty() {
                buf.push(b'&');
            }
            buf.extend_from_slice(&key);
            buf.push(b'=');
            buf.extend(query_escape(val));
        }
    }
    buf
}
