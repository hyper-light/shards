//! `run -p` as the Docker CLI reads it (docker/cli cli/command/container/opts.go, parse:
//! convertToStandardNotation, then go-connections nat.ParsePortSpecs, vendored by
//! docker/cli v29.8.1): each value a port or range of the container, published on a
//! host port, range, or any, at a host address or every one, for TCP, UDP or SCTP.

use crate::network::parse_addr;

/// A container port, as the CLI names one: its number and protocol (`80/tcp`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Port {
    pub number: u16,
    pub proto: String,
}

impl std::fmt::Display for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.number, self.proto)
    }
}

/// Where a container port is published: a host address (none for every one), and a host
/// port, a range of them to take one from, or none for any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub port: Port,
    pub host_ip: String,
    pub host_port: String,
}

/// What `-p`'s `values` publish: the ports they expose, sorted, each once, and their
/// bindings in the order given. Every value is put in standard notation before any is
/// parsed, as the CLI's parse does (review 2.29): one whose notation fails is said before
/// a port before it that would not parse.
pub fn publish(values: &[String]) -> Result<(Vec<Port>, Vec<Binding>), String> {
    let standard: Vec<String> = values.iter().map(|v| standard(v)).collect::<Result<_, _>>()?;
    let mut exposed: Vec<Port> = Vec::new();
    let mut bindings = Vec::new();
    for value in &standard {
        for b in parse_spec(value)? {
            if !exposed.contains(&b.port) {
                exposed.push(b.port.clone());
            }
            bindings.push(b);
        }
    }
    exposed.sort();
    Ok((exposed, bindings))
}

/// convertToStandardNotation: `published=8080,target=80,protocol=udp` as `8080:80/udp`.
fn standard(value: &str) -> Result<String, String> {
    if !value.contains('=') {
        return Ok(value.to_string());
    }
    let (mut published, mut target, mut protocol) = ("", "", "tcp");
    for param in value.split(',') {
        match param.split_once('=') {
            Some((k, v)) if !k.is_empty() => match k {
                "published" => published = v,
                "target" => target = v,
                "protocol" => protocol = v,
                _ => {}
            },
            _ => {
                return Err(format!(
                    "invalid publish opts format (should be name=value but got '{param}')"
                ));
            }
        }
    }
    Ok(format!("{published}:{target}/{protocol}"))
}

/// nat.ParsePortSpec: `[ip:][hostPort:]containerPort[/proto]`.
fn parse_spec(raw: &str) -> Result<Vec<Binding>, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (ip, host_port, container) = match parts.as_slice() {
        [c] => (String::new(), "", *c),
        [h, c] => (String::new(), *h, *c),
        [ip, h, c] => ((*ip).to_string(), *h, *c),
        [ip @ .., h, c] => (ip.join(":"), *h, *c),
        [] => (String::new(), "", ""),
    };
    // SplitProtoPort: no port is no protocol either.
    let (container, proto) = match container.split_once('/') {
        Some(("", _)) => ("", ""),
        Some((port, "")) => (port, "tcp"),
        Some((port, proto)) => (port, proto),
        None if container.is_empty() => ("", ""),
        None => (container, "tcp"),
    };
    if container.is_empty() {
        return Err(format!("no port specified: {raw}<empty>"));
    }
    let proto = proto.to_lowercase();
    if !matches!(proto.as_str(), "tcp" | "udp" | "sctp") {
        return Err(format!("invalid proto: {proto}"));
    }
    let ip = if ip.starts_with('[') {
        split_host(&format!("{ip}:")).map_err(|e| format!("invalid IP address {ip}: {e}"))?
    } else {
        ip
    };
    if !ip.is_empty() && !parse_addr(&ip).is_ok_and(|a| a.zone.is_empty()) {
        return Err(format!("invalid IP address: {ip}"));
    }
    let (start, end) = port_range(container).ok_or_else(|| format!("invalid containerPort: {container}"))?;
    let (mut host_start, mut host_end) = (0, 0);
    if !host_port.is_empty() {
        (host_start, host_end) =
            port_range(host_port).ok_or_else(|| format!("invalid hostPort: {host_port}"))?;
        if end - start != host_end - host_start && end != start {
            return Err(format!(
                "invalid ranges specified for container and host Ports: {container} and {host_port}"
            ));
        }
    }
    let count = end - start + 1;
    Ok((0..count)
        .map(|i| {
            let host_port = if host_port.is_empty() {
                String::new()
            } else if count == 1 && host_start != host_end {
                // A single container port takes one port of the host's range.
                format!("{}-{host_end}", host_start + i)
            } else {
                (host_start + i).to_string()
            };
            Binding {
                port: Port {
                    number: u16::try_from(start + i).unwrap_or(u16::MAX),
                    proto: proto.clone(),
                },
                host_ip: ip.clone(),
                host_port,
            }
        })
        .collect())
}

/// parsePortRange: `start` or `start-end`, each a decimal of 0 to 65535 as
/// strconv.ParseInt reads it (a sign allowed), `end` no less than `start`.
fn port_range(ports: &str) -> Option<(u32, u32)> {
    let number = |s: &str| {
        let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<i64>()
            .ok()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n <= 65535)
    };
    if ports.is_empty() {
        return None;
    }
    let (start, end) = match ports.split_once('-') {
        Some((s, e)) if s != e => (number(s)?, number(e)?),
        Some((s, _)) => (number(s)?, number(s)?),
        None => (number(ports)?, number(ports)?),
    };
    (end >= start).then_some((start, end))
}

/// net.SplitHostPort's host, for `[ip]:`: the address between the brackets, or the
/// AddrError it would return.
fn split_host(hostport: &str) -> Result<String, String> {
    let err = |why: &str| format!("address {hostport}: {why}");
    let Some(last_colon) = hostport.rfind(':') else {
        return Err(err("missing port in address"));
    };
    let Some(end) = hostport.find(']') else {
        return Err(err("missing ']' in address"));
    };
    if end + 1 == hostport.len() {
        return Err(err("missing port in address"));
    }
    if end + 1 != last_colon {
        return Err(if hostport.as_bytes().get(end + 1) == Some(&b':') {
            err("too many colons in address")
        } else {
            err("missing port in address")
        });
    }
    let host = hostport.get(1..end).unwrap_or_default();
    if hostport.get(1..).is_some_and(|s| s.contains('[')) {
        return Err(err("unexpected '[' in address"));
    }
    if hostport.get(end + 1..).is_some_and(|s| s.contains(']')) {
        return Err(err("unexpected ']' in address"));
    }
    Ok(host.to_string())
}

/// What `port CONTAINER PORT` reads PORT as (moby api/types/network ParsePort):
/// `PORT[/PROTO]`, the port a decimal of 0 to 65535, the protocol any, lower-cased, TCP
/// if none.
pub fn parse_port(s: &str) -> Result<Port, String> {
    if s.is_empty() {
        return Err("invalid port: value is empty".into());
    }
    let (port, proto) = s.split_once('/').unwrap_or((s, ""));
    let number = if port.is_empty() {
        Err("value is empty")
    } else if !port.bytes().all(|b| b.is_ascii_digit()) {
        Err("invalid syntax")
    } else {
        // All digits: too many for a u16 is out of range, as strconv.ParseUint says.
        port.parse::<u16>().map_err(|_| "value out of range")
    };
    let number = number.map_err(|e| format!("invalid port '{port}': {e}"))?;
    Ok(Port {
        number,
        proto: if proto.is_empty() {
            "tcp".into()
        } else {
            proto.to_lowercase()
        },
    })
}

/// fvbommel/sortorder's NaturalCompare, by which `docker port` orders its lines: runs of
/// digits compared as numbers (fewer leading zeros first among equals), digits before
/// other bytes, the rest byte by byte.
pub fn natural_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while let (Some(&c1), Some(&c2)) = (a.get(i), b.get(j)) {
        match (c1.is_ascii_digit(), c2.is_ascii_digit()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {
                if c1 != c2 {
                    return c1.cmp(&c2);
                }
                i += 1;
                j += 1;
            }
            (true, true) => {
                let skip = |s: &[u8], mut k: usize| {
                    while s.get(k) == Some(&b'0') {
                        k += 1;
                    }
                    k
                };
                let digits_end = |s: &[u8], mut k: usize| {
                    while s.get(k).is_some_and(u8::is_ascii_digit) {
                        k += 1;
                    }
                    k
                };
                let (nz1, nz2) = (skip(a, i), skip(b, j));
                (i, j) = (digits_end(a, nz1), digits_end(b, nz2));
                let order = (i - nz1)
                    .cmp(&(j - nz2))
                    .then_with(|| a.get(nz1..i).cmp(&b.get(nz2..j)))
                    .then_with(|| nz1.cmp(&nz2));
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
    a.len().cmp(&b.len())
}

/// network.ParsePortRange (moby api/types/network/port.go): `PORT[-PORT][/PROTO]`, its
/// first and last port and its protocol, `tcp` unless named, lowercase.
pub fn parse_port_range(s: &str) -> Result<(u16, u16, String), String> {
    if s.is_empty() {
        return Err("invalid port range: value is empty".into());
    }
    let (range, proto) = s.split_once('/').unwrap_or((s, ""));
    let proto = if proto.is_empty() {
        "tcp".to_string()
    } else {
        proto.to_lowercase()
    };
    let (start, end) = match range.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (range, None),
    };
    let first = port_number(start).map_err(|e| format!("invalid start port '{start}': {e}"))?;
    match end {
        Some(end) if end != start => {
            let last = port_number(end).map_err(|e| format!("invalid end port '{end}': {e}"))?;
            if last < first {
                return Err(format!("invalid port range: {s}"));
            }
            Ok((first, last, proto))
        }
        _ => Ok((first, first, proto)),
    }
}

/// parsePortNumber: strconv.ParseUint(raw, 10, 16)'s answer and words.
fn port_number(raw: &str) -> Result<u16, &'static str> {
    if raw.is_empty() {
        return Err("value is empty");
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid syntax");
    }
    raw.parse().map_err(|_| "value out of range")
}
