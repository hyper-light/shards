//! `--network` as the Docker CLI reads it: each value an attachment, in the short syntax
//! (a network's name) or the long one (`name=…,alias=…,ip=…`), checked as the CLI checks
//! them before it asks the daemon (docker/cli opts/network.go NetworkOpt.Set,
//! cli/command/container/opts.go parseNetworkOpts and parseNetworkAttachmentOpt), with the
//! Go functions those use: `encoding/csv`'s first record, `net/netip.ParseAddr` and
//! `net.ParseMAC` (Go 1.26.1).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::go;

/// An address as `netip.Addr` holds one: IPv4, or IPv6 with its zone, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr {
    pub ip: IpAddr,
    pub zone: String,
}

impl std::fmt::Display for Addr {
    /// As `netip.Addr.String` prints it: dotted IPv4; IPv6 in RFC 5952's form, an
    /// IPv4-mapped one as `::ffff:a.b.c.d`; then `%zone`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip {
            IpAddr::V4(v4) => write!(f, "{v4}"),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => write!(f, "::ffff:{v4}"),
                None => write!(f, "{v6}"),
            }
            .and_then(|()| match self.zone.as_str() {
                "" => Ok(()),
                zone => write!(f, "%{zone}"),
            }),
        }
    }
}

impl Addr {
    /// `Is4`: an IPv4 address, not one mapped into IPv6.
    pub fn is4(&self) -> bool {
        self.ip.is_ipv4()
    }

    /// `Is4In6`: an IPv4-mapped IPv6 address.
    pub fn is4in6(&self) -> bool {
        matches!(self.ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some())
    }

    /// `IsUnspecified`: 0.0.0.0 or ::, and no zone.
    pub fn is_unspecified(&self) -> bool {
        self.ip.is_unspecified() && self.zone.is_empty()
    }

    /// `Unmap`: an IPv4-mapped address as IPv4, its zone dropped; the rest as they are.
    pub fn unmap(&self) -> Addr {
        match self.ip {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Addr {
                    ip: IpAddr::V4(v4),
                    zone: String::new(),
                },
                None => self.clone(),
            },
            IpAddr::V4(_) => self.clone(),
        }
    }
}

/// One `--network` value, as `NetworkOpt.Set` reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachment {
    pub target: String,
    pub aliases: Vec<String>,
    pub driver_opts: Vec<(String, String)>,
    pub ipv4: Option<Addr>,
    pub ipv6: Option<Addr>,
    pub link_local: Vec<Addr>,
    pub mac: String,
    pub gw_priority: i64,
}

impl Attachment {
    /// Whether it asks nothing of its endpoint (`reflect.ValueOf(*ep).IsZero()`).
    pub fn is_bare(&self) -> bool {
        self.aliases.is_empty()
            && self.driver_opts.is_empty()
            && self.ipv4.is_none()
            && self.ipv6.is_none()
            && self.link_local.is_empty()
            && self.mac.is_empty()
            && self.gw_priority == 0
    }
}

/// `value` as `NetworkOpt.Set` reads it: the long syntax wherever `\w+=\w+` matches in it,
/// as its first CSV record of `key=value` fields; otherwise a network's name.
pub fn attachment(value: &str) -> Result<Attachment, String> {
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let bytes = value.as_bytes();
    let long = (0..bytes.len())
        .any(|i| bytes.get(i) == Some(&b'=') && i > 0 && word(bytes.get(i - 1)) && word(bytes.get(i + 1)));
    if !long {
        return Ok(Attachment {
            target: value.to_string(),
            ..Attachment::default()
        });
    }
    let mut a = Attachment::default();
    for field in csv_first_record(value)? {
        // The CLI lowercases the whole field, value too.
        let lower = go_lower(&field);
        let (key, val) = match lower.split_once('=') {
            Some((k, v)) if !k.is_empty() => (k.trim(), v.trim()),
            _ => return Err(format!("invalid field {field}")),
        };
        match key {
            "name" => a.target = val.to_string(),
            "alias" => a.aliases.push(val.to_string()),
            "ip" => a.ipv4 = Some(parse_addr(val)?),
            "ip6" => a.ipv6 = Some(parse_addr(val)?),
            "mac-address" => a.mac = val.to_string(),
            "link-local-ip" => a.link_local.push(parse_addr(val)?),
            "driver-opt" => {
                let (k, v) = match go_lower(val).split_once('=') {
                    Some((k, v)) if !k.is_empty() => (k.trim().to_string(), v.trim().to_string()),
                    _ => return Err("invalid key value pair format in driver options".into()),
                };
                match a.driver_opts.iter_mut().find(|(have, _)| *have == k) {
                    Some(slot) => slot.1 = v,
                    None => a.driver_opts.push((k, v)),
                }
            }
            "gw-priority" => {
                a.gw_priority = atoi(val).map_err(|why| format!("invalid gw-priority ({val}): {why}"))?;
            }
            _ => return Err(format!("invalid field key {key}")),
        }
    }
    if a.target.is_empty() {
        return Err("network name/id is not specified".into());
    }
    Ok(a)
}

/// Whether `mode` names a network users make, not one of the modes every daemon has
/// (moby api/types/container NetworkMode.IsUserDefined).
pub fn is_user_defined(mode: &str) -> bool {
    !matches!(mode, "default" | "bridge" | "host" | "none")
        && !mode.split_once(':').is_some_and(|(k, _)| k == "container")
}

/// The network mode `run` asks for: its first network's, or the default
/// (NetworkOpt.NetworkMode).
pub fn mode(given: &[Attachment]) -> &str {
    given.first().map_or("default", |a| a.target.as_str())
}

/// The endpoints `run` asks the daemon for, by network, in the order given, from the
/// attachments its `--network` flags read (parseNetworkOpts): none given is the default
/// network; one given that asks nothing of its endpoint is left for the daemon to make.
pub fn endpoints(given: &[Attachment]) -> Result<Vec<Attachment>, String> {
    if given.is_empty() {
        return Ok(vec![Attachment {
            target: "default".into(),
            ..Attachment::default()
        }]);
    }
    let mut out: Vec<Attachment> = Vec::with_capacity(given.len());
    let (mut user_defined, mut predefined) = (false, false);
    for (i, a) in given.iter().enumerate() {
        if is_user_defined(&a.target) {
            user_defined = true;
        } else {
            predefined = true;
        }
        if a.target.trim().is_empty() {
            return Err("no name set for network".into());
        }
        if !is_user_defined(&a.target) && !a.aliases.is_empty() {
            return Err("network-scoped aliases are only supported for user-defined networks".into());
        }
        if !a.mac.is_empty() && !parse_mac(a.mac.trim()) {
            return Err(format!("{} is not a valid mac address", a.mac));
        }
        if out.iter().any(|e| e.target == a.target) {
            return Err(format!(
                "network {} is specified multiple times",
                go::quote(&a.target)
            ));
        }
        if i == 0 && given.len() == 1 && a.is_bare() {
            continue;
        }
        out.push(a.clone());
    }
    if user_defined && predefined {
        return Err(
            "conflicting options: cannot attach both user-defined and non-user-defined network-modes".into(),
        );
    }
    Ok(out)
}

/// `strings.ToLower`: each rune's simple lowercase mapping, which differs from Rust's full
/// one only for U+0130 (SpecialCasing.txt's one unconditional lowercase entry).
fn go_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\u{130}' {
            out.push('i');
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// `strconv.Atoi`'s error, as `NumError.Err` says it: the value itself, or why not.
fn atoi(s: &str) -> Result<i64, &'static str> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid syntax");
    }
    s.parse::<i64>().map_err(|_| "value out of range")
}

/// `net.ParseMAC` succeeds: 6, 8 or 20 octets, as `xx:xx…`, `xx-xx…`, `xxxx.xxxx…` or bare
/// hex.
pub fn parse_mac(s: &str) -> bool {
    let b = s.as_bytes();
    // xtoi2 of a two-byte slice: two hex digits.
    let hex2 = |at: usize| {
        b.get(at).is_some_and(u8::is_ascii_hexdigit) && b.get(at + 1).is_some_and(u8::is_ascii_hexdigit)
    };
    // xtoi2 of the rest from `at`: two hex digits, then `sep` unless they end it.
    let hex2_then = |at: usize, sep: u8| hex2(at) && (b.len() <= at + 2 || b.get(at + 2) == Some(&sep));
    let octets = |n: usize| matches!(n, 6 | 8 | 20);
    if b.len() < 12 {
        return false;
    }
    match (b.get(2), b.get(4)) {
        (Some(&sep @ (b':' | b'-')), _) => {
            let n = (b.len() + 1) / 3;
            (b.len() + 1).is_multiple_of(3) && octets(n) && (0..n).all(|i| hex2_then(i * 3, sep))
        }
        (_, Some(&dot @ b'.')) => {
            let n = 2 * (b.len() + 1) / 5;
            (b.len() + 1).is_multiple_of(5)
                && octets(n)
                && (0..n / 2).all(|i| hex2(i * 5) && hex2_then(i * 5 + 2, dot))
        }
        _ => b.len().is_multiple_of(2) && octets(b.len() / 2) && (0..b.len() / 2).all(|i| hex2(i * 2)),
    }
}

/// `netip.ParseAddr(s)`, or its error's text.
pub fn parse_addr(s: &str) -> Result<Addr, String> {
    let fail = |msg: &str, at: &str| {
        let mut e = format!("ParseAddr({}): {msg}", go::quote(s));
        if !at.is_empty() {
            e.push_str(&format!(" (at {})", go::quote(at)));
        }
        e
    };
    for b in s.bytes() {
        match b {
            b'.' => {
                let mut fields = [0u8; 4];
                ipv4_fields(s, &mut fields).map_err(|(m, at)| fail(m, at))?;
                return Ok(Addr {
                    ip: IpAddr::V4(Ipv4Addr::from(fields)),
                    zone: String::new(),
                });
            }
            b':' => return ipv6(s).map_err(|(m, at)| fail(m, at)),
            b'%' => return Err(fail("missing IPv6 address", "")),
            _ => {}
        }
    }
    Err(fail("unable to parse IP", ""))
}

type ParseFail<'a> = (&'static str, &'a str);

/// parseIPv4Fields over `s`, the whole address or its embedded IPv4 end; the error's
/// `at` is a part of it.
fn ipv4_fields<'a>(s: &'a str, fields: &mut [u8; 4]) -> Result<(), ParseFail<'a>> {
    let b = s.as_bytes();
    let (mut val, mut pos, mut dig_len) = (0u32, 0usize, 0u32);
    for (i, &c) in b.iter().enumerate() {
        if c.is_ascii_digit() {
            if dig_len == 1 && val == 0 {
                return Err(("IPv4 field has octet with leading zero", ""));
            }
            val = val * 10 + u32::from(c - b'0');
            dig_len += 1;
            if val > 255 {
                return Err(("IPv4 field has value >255", ""));
            }
        } else if c == b'.' {
            if i == 0 || i == b.len() - 1 || b.get(i - 1) == Some(&b'.') {
                return Err((
                    "IPv4 field must have at least one digit",
                    s.get(i..).unwrap_or_default(),
                ));
            }
            if pos == 3 {
                return Err(("IPv4 address too long", ""));
            }
            if let Some(f) = fields.get_mut(pos) {
                *f = u8::try_from(val).unwrap_or(u8::MAX);
            }
            pos += 1;
            val = 0;
            dig_len = 0;
        } else {
            return Err(("unexpected character", s.get(i..).unwrap_or_default()));
        }
    }
    if pos < 3 {
        return Err(("IPv4 address too short", ""));
    }
    if let Some(f) = fields.get_mut(3) {
        *f = u8::try_from(val).unwrap_or(u8::MAX);
    }
    Ok(())
}

/// parseIPv6.
fn ipv6(input: &str) -> Result<Addr, ParseFail<'_>> {
    let (mut s, zone) = match input.split_once('%') {
        Some((_, "")) => return Err(("zone must be a non-empty string", "")),
        Some((s, zone)) => (s, zone),
        None => (input, ""),
    };
    let addr = |ip: [u8; 16]| Addr {
        ip: IpAddr::V6(Ipv6Addr::from(ip)),
        zone: zone.to_string(),
    };
    let mut ip = [0u8; 16];
    let mut ellipsis: Option<usize> = None;
    if let Some(rest) = s.strip_prefix("::") {
        ellipsis = Some(0);
        s = rest;
        if s.is_empty() {
            return Ok(addr(ip));
        }
    }
    let mut i = 0usize;
    while i < 16 {
        let b = s.as_bytes();
        let mut off = 0usize;
        let mut acc = 0u32;
        while let Some(&c) = b.get(off) {
            let Some(d) = (c as char).to_digit(16) else {
                break;
            };
            acc = (acc << 4) + d;
            if off > 3 {
                return Err(("each group must have 4 or less digits", s));
            }
            if acc > u32::from(u16::MAX) {
                return Err(("IPv6 field has value >=2^16", s));
            }
            off += 1;
        }
        if off == 0 {
            return Err(("each colon-separated field must have at least one digit", s));
        }
        if b.get(off) == Some(&b'.') {
            if ellipsis.is_none() && i != 12 {
                return Err((
                    "embedded IPv4 address must replace the final 2 fields of the address",
                    s,
                ));
            }
            if i + 4 > 16 {
                return Err((
                    "too many hex fields to fit an embedded IPv4 at the end of the address",
                    s,
                ));
            }
            let mut v4 = [0u8; 4];
            ipv4_fields(s, &mut v4)?;
            if let Some(slot) = ip.get_mut(i..i + 4) {
                slot.copy_from_slice(&v4);
            }
            s = "";
            i += 4;
            break;
        }
        let [hi, lo] = u16::try_from(acc).unwrap_or(u16::MAX).to_be_bytes();
        if let Some(slot) = ip.get_mut(i..i + 2) {
            slot.copy_from_slice(&[hi, lo]);
        }
        i += 2;
        s = s.get(off..).unwrap_or_default();
        if s.is_empty() {
            break;
        }
        if !s.starts_with(':') {
            return Err(("unexpected character, want colon", s));
        } else if s.len() == 1 {
            return Err(("colon must be followed by more characters", s));
        }
        s = s.get(1..).unwrap_or_default();
        if s.starts_with(':') {
            if ellipsis.is_some() {
                return Err(("multiple :: in address", s));
            }
            ellipsis = Some(i);
            s = s.get(1..).unwrap_or_default();
            if s.is_empty() {
                break;
            }
        }
    }
    if !s.is_empty() {
        return Err(("trailing garbage after address", s));
    }
    if i < 16 {
        let Some(e) = ellipsis else {
            return Err(("address string too short", ""));
        };
        let n = 16 - i;
        for j in (e..i).rev() {
            if let Some(&v) = ip.get(j)
                && let Some(slot) = ip.get_mut(j + n)
            {
                *slot = v;
            }
        }
        if let Some(gap) = ip.get_mut(e..e + n) {
            gap.fill(0);
        }
    } else if ellipsis.is_some() {
        return Err(("the :: must expand to at least one field of zeros", ""));
    }
    Ok(addr(ip))
}

/// `encoding/csv`'s `Reader.Read` of `input`: its first record, or its error's text.
fn csv_first_record(input: &str) -> Result<Vec<String>, String> {
    // readLine's lines, each with its \n, \r\n made \n, and a last \r before the end
    // dropped.
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut rest = input.as_bytes();
    while !rest.is_empty() {
        let (mut line, after) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let (line, after) = rest.split_at(i + 1);
                (line.to_vec(), after)
            }
            None => {
                let mut line = rest.to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                (line, &[][..])
            }
        };
        rest = after;
        let n = line.len();
        if n >= 2 && line.get(n - 2..) == Some(&b"\r\n"[..]) {
            line.truncate(n - 1);
            if let Some(last) = line.last_mut() {
                *last = b'\n';
            }
        }
        lines.push(line);
    }
    let nl = |b: &[u8]| usize::from(b.last() == Some(&b'\n'));
    // Past empty lines to the record's first; Go counts every line read.
    let Some(first) = lines.iter().position(|l| l.len() != nl(l)) else {
        return Err("EOF".into());
    };
    let (mut index, mut num_line) = (first, first + 1);
    let rec_line = num_line;
    let (mut pos_line, mut pos_col) = (num_line, 1usize);
    let error = |line: usize, col: usize, what: &str| {
        if rec_line == line {
            format!("parse error on line {line}, column {col}: {what}")
        } else {
            format!("record on line {rec_line}; parse error on line {line}, column {col}: {what}")
        }
    };
    const BARE: &str = "bare \" in non-quoted-field";
    const QUOTE: &str = "extraneous or missing \" in quoted-field";
    let mut fields: Vec<Vec<u8>> = Vec::new();
    let mut l: &[u8] = lines.get(index).map(Vec::as_slice).unwrap_or_default();
    'field: loop {
        if l.first() != Some(&b'"') {
            let comma = l.iter().position(|&b| b == b',');
            let field = match comma {
                Some(i) => l.get(..i).unwrap_or_default(),
                None => l.get(..l.len() - nl(l)).unwrap_or_default(),
            };
            if let Some(j) = field.iter().position(|&b| b == b'"') {
                return Err(error(num_line, pos_col + j, BARE));
            }
            fields.push(field.to_vec());
            let Some(i) = comma else {
                break 'field;
            };
            l = l.get(i + 1..).unwrap_or_default();
            pos_col += i + 1;
            continue;
        }
        let mut field = Vec::new();
        l = l.get(1..).unwrap_or_default();
        pos_col += 1;
        loop {
            if let Some(i) = l.iter().position(|&b| b == b'"') {
                field.extend_from_slice(l.get(..i).unwrap_or_default());
                l = l.get(i + 1..).unwrap_or_default();
                pos_col += i + 1;
                match l.first() {
                    Some(b'"') => {
                        field.push(b'"');
                        l = l.get(1..).unwrap_or_default();
                        pos_col += 1;
                    }
                    Some(b',') => {
                        l = l.get(1..).unwrap_or_default();
                        pos_col += 1;
                        fields.push(field);
                        continue 'field;
                    }
                    _ if l.len() == nl(l) => {
                        fields.push(field);
                        break 'field;
                    }
                    _ => return Err(error(num_line, pos_col - 1, QUOTE)),
                }
            } else if !l.is_empty() {
                // The field goes on past its line.
                field.extend_from_slice(l);
                pos_col += l.len();
                index += 1;
                num_line += 1;
                l = lines.get(index).map(Vec::as_slice).unwrap_or_default();
                if !l.is_empty() {
                    pos_line += 1;
                    pos_col = 1;
                }
            } else {
                return Err(error(pos_line, pos_col, QUOTE));
            }
        }
    }
    Ok(fields
        .into_iter()
        .map(|f| String::from_utf8_lossy(&f).into_owned())
        .collect())
}
