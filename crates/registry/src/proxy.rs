//! Proxies, as Go's net/http chooses them from the environment (golang.org/x/net/http/
//! httpproxy, as go1.26 vendors it): `HTTPS_PROXY` for https URLs and `HTTP_PROXY` for
//! http ones, each name uppercase before lowercase, and `HTTP_PROXY` not under CGI
//! (`REQUEST_METHOD` set); none for loopback, or for what `NO_PROXY` names.
//!
//! Unlike Go: a proxy value that is no URL, or names a scheme other than http and https,
//! fails each request it would carry, where Go goes direct (or, for SOCKS, through a dialer
//! shards does not have) without a word. Non-ASCII names in `NO_PROXY` are compared as
//! written, where Go converts them to punycode first.

use std::fmt;
use std::net::IpAddr;

use crate::Error;
use crate::auth::basic;
use crate::url::{Scheme, Url};

/// The environment names Go reads, in the order it reads them.
pub const ENV: [&str; 7] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// A proxy requests go through: its URL, http or https, and the `Proxy-Authorization`
/// its userinfo makes, if it has any.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    pub url: Url,
    pub authorization: Option<String>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("url", &self.url.to_string())
            .field("authorization", &self.authorization.as_ref().map(|_| "…"))
            .finish()
    }
}

/// The proxies a client's requests go through, and the hosts that go direct.
#[derive(Debug, Clone, Default)]
pub struct Proxies {
    http: Option<Result<Proxy, String>>,
    https: Option<Result<Proxy, String>>,
    cgi: bool,
    direct: NoProxy,
}

impl Proxies {
    /// FromEnvironment, then init: what `env` says.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Proxies {
        let any = |names: [&str; 2]| names.iter().find_map(|n| env(n).filter(|v| !v.is_empty()));
        Proxies {
            http: any(["HTTP_PROXY", "http_proxy"]).map(|v| parse(&v)),
            https: any(["HTTPS_PROXY", "https_proxy"]).map(|v| parse(&v)),
            cgi: env("REQUEST_METHOD").is_some_and(|v| !v.is_empty()),
            direct: NoProxy::parse(&any(["NO_PROXY", "no_proxy"]).unwrap_or_default()),
        }
    }

    /// proxyForURL: the proxy `url` goes through, if any.
    pub fn for_url(&self, url: &Url) -> Result<Option<&Proxy>, Error> {
        let proxy = match url.scheme() {
            Scheme::Https => &self.https,
            Scheme::Http => {
                if self.http.is_some() && self.cgi {
                    return Err(Error::new(
                        "refusing to use HTTP_PROXY value in CGI environment; see golang.org/s/cgihttpproxy",
                    ));
                }
                &self.http
            }
        };
        let Some(proxy) = proxy else {
            return Ok(None);
        };
        let host = url.host().trim_start_matches('[').trim_end_matches(']');
        if !self.direct.uses_proxy(host, url.port()) {
            return Ok(None);
        }
        proxy.as_ref().map(Some).map_err(|e| Error::new(e.clone()))
    }
}

/// parseProxy: a proxy URL, with `http://` in front of one that names no scheme or host.
fn parse(value: &str) -> Result<Proxy, String> {
    let given = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let (scheme, rest) = given.split_once("://").unwrap_or(("http", &given));
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "proxy scheme {scheme:?} is not supported: http and https are"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let url =
        Url::parse(&format!("{scheme}://{hostport}/")).map_err(|e| format!("invalid proxy address: {e}"))?;
    let authorization = userinfo.map(|u| {
        let (user, password) = u.split_once(':').unwrap_or((u, ""));
        basic(&unescape(user), &unescape(password))
    });
    Ok(Proxy { url, authorization })
}

/// A userinfo part, its %XX escapes undone, as Go's url.User reads it.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex = |at: usize| b.get(at).and_then(|d| char::from(*d).to_digit(16));
        match (c, hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// NO_PROXY's entries, as init reads them.
#[derive(Debug, Clone, Default)]
struct NoProxy {
    all: bool,
    ips: Vec<IpMatch>,
    domains: Vec<DomainMatch>,
}

#[derive(Debug, Clone)]
enum IpMatch {
    Cidr(IpAddr, u8),
    Ip(IpAddr, Option<String>),
}

#[derive(Debug, Clone)]
struct DomainMatch {
    /// With a leading `.`.
    host: String,
    port: Option<String>,
    /// Whether the name itself matches, not only names under it.
    whole: bool,
}

impl NoProxy {
    fn parse(value: &str) -> NoProxy {
        let mut out = NoProxy::default();
        for entry in value.split(',') {
            let p = entry.trim().to_ascii_lowercase();
            if p.is_empty() {
                continue;
            }
            if p == "*" {
                return NoProxy {
                    all: true,
                    ..NoProxy::default()
                };
            }
            if let Some(cidr) = cidr(&p) {
                out.ips.push(cidr);
                continue;
            }
            let (host, port) = match split_host_port(&p) {
                Some((host, port)) => {
                    if host.is_empty() {
                        continue;
                    }
                    (host, Some(port))
                }
                None => (p.as_str(), None),
            };
            let host = host.trim_start_matches('[').trim_end_matches(']');
            if let Ok(ip) = host.parse::<IpAddr>() {
                out.ips
                    .push(IpMatch::Ip(ip, port.filter(|p| !p.is_empty()).map(String::from)));
                continue;
            }
            if host.is_empty() {
                continue;
            }
            let host = host
                .strip_prefix('*')
                .filter(|h| h.starts_with('.'))
                .unwrap_or(host);
            let (host, whole) = match host.strip_prefix('.') {
                Some(_) => (host.to_string(), false),
                None => (format!(".{host}"), true),
            };
            out.domains.push(DomainMatch {
                host,
                port: port.filter(|p| !p.is_empty()).map(String::from),
                whole,
            });
        }
        out
    }

    /// useProxy: whether `host` (unbracketed) at `port` goes through a proxy.
    fn uses_proxy(&self, host: &str, port: u16) -> bool {
        if self.all {
            return false;
        }
        if host == "localhost" {
            return false;
        }
        let ip = host.parse::<IpAddr>().ok();
        if ip.is_some_and(|ip| ip.is_loopback()) {
            return false;
        }
        let host = host.trim().to_ascii_lowercase();
        let port = port.to_string();
        let port_ok = |wanted: &Option<String>| wanted.as_ref().is_none_or(|w| *w == port);
        if let Some(ip) = ip {
            for m in &self.ips {
                let matched = match m {
                    IpMatch::Cidr(net, bits) => in_cidr(ip, *net, *bits),
                    IpMatch::Ip(want, wanted_port) => *want == ip && port_ok(wanted_port),
                };
                if matched {
                    return false;
                }
            }
            // A domain names no address.
            return true;
        }
        for m in &self.domains {
            let under = host.ends_with(&m.host);
            let itself = m.whole && host == m.host.get(1..).unwrap_or_default();
            if (under || itself) && port_ok(&m.port) {
                return false;
            }
        }
        true
    }
}

/// net.ParseCIDR: an address, `/`, and a prefix no longer than the address.
fn cidr(p: &str) -> Option<IpMatch> {
    let (ip, bits) = p.split_once('/')?;
    let ip: IpAddr = ip.parse().ok()?;
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) || bits.len() > 3 {
        return None;
    }
    let bits: u8 = bits.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (bits <= max).then_some(IpMatch::Cidr(ip, bits))
}

fn in_cidr(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    let (ip, net, len) = match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => (u128::from(a.to_bits()), u128::from(n.to_bits()), 32u32),
        (IpAddr::V6(a), IpAddr::V6(n)) => (a.to_bits(), n.to_bits(), 128u32),
        // An IPv4 address in an IPv6 network, as Go's IPNet.Contains compares them: by the
        // network's family.
        (IpAddr::V4(a), IpAddr::V6(n)) => (a.to_ipv6_mapped().to_bits(), n.to_bits(), 128u32),
        (IpAddr::V6(a), IpAddr::V4(n)) => match a.to_ipv4_mapped() {
            Some(a) => (u128::from(a.to_bits()), u128::from(n.to_bits()), 32u32),
            None => return false,
        },
    };
    let shift = len.saturating_sub(u32::from(bits));
    let keep = |v: u128| v.checked_shr(shift).unwrap_or(0);
    keep(ip) == keep(net)
}

/// net.SplitHostPort: `host:port` or `[host]:port`; `None` where Go's fails (no port, or
/// an unbracketed host with colons of its own).
fn split_host_port(p: &str) -> Option<(&str, &str)> {
    if let Some(rest) = p.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':')?;
        return (!port.contains(':')).then_some((host, port));
    }
    let (host, port) = p.rsplit_once(':')?;
    (!host.contains(':')).then_some((host, port))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn proxies(pairs: &[(&str, &str)]) -> Proxies {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        Proxies::from_env(&move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    fn through(p: &Proxies, url: &str) -> Option<String> {
        p.for_url(&Url::parse(url).unwrap())
            .unwrap()
            .map(|proxy| proxy.url.to_string())
    }

    /// x/net's proxyForURLTests, those of HTTP and HTTPS proxies.
    #[test]
    fn requests_go_through_the_proxy_go_picks() {
        let both = proxies(&[
            ("HTTP_PROXY", "http://http.proxy.tld"),
            ("HTTPS_PROXY", "https://secure.proxy.tld"),
        ]);
        assert_eq!(
            through(&both, "http://example.com").as_deref(),
            Some("http://http.proxy.tld/")
        );
        assert_eq!(
            through(&both, "https://example.com").as_deref(),
            Some("https://secure.proxy.tld/")
        );
        // Uppercase first; lowercase when the uppercase is empty.
        let cased = proxies(&[("HTTP_PROXY", ""), ("http_proxy", "lower.tld:8080")]);
        assert_eq!(
            through(&cased, "http://example.com").as_deref(),
            Some("http://lower.tld:8080/")
        );
        // Loopback goes direct.
        for url in ["http://localhost", "http://127.0.0.1:5000", "http://[::1]/"] {
            assert_eq!(through(&both, url), None, "{url}");
        }
        // No proxy for the scheme: direct.
        let http_only = proxies(&[("HTTP_PROXY", "proxy.tld")]);
        assert_eq!(through(&http_only, "https://example.com"), None);
        // CGI refuses HTTP_PROXY.
        let cgi = proxies(&[("HTTP_PROXY", "proxy.tld"), ("REQUEST_METHOD", "GET")]);
        assert!(cgi.for_url(&Url::parse("http://example.com").unwrap()).is_err());
        assert!(cgi.for_url(&Url::parse("https://example.com").unwrap()).is_ok());
    }

    /// x/net's UseProxyTests, with NO_PROXY as they set it, and `bar.com:80`.
    #[test]
    fn no_proxy_names_what_goes_direct_as_go_reads_it() {
        let p = proxies(&[
            ("HTTP_PROXY", "proxy"),
            (
                "NO_PROXY",
                "foobar.com, .barbaz.net, *.wildcard.io, 192.168.1.1, 192.168.1.2:81, 192.168.1.3:80, 10.0.0.0/30, 2001:db8::52:0:1, [2001:db8::52:0:2]:443, [2001:db8::52:0:3]:80, 2002:db8:a::45/64, bar.com:80",
            ),
        ]);
        let cases: &[(&str, bool)] = &[
            // Never proxy localhost.
            ("localhost", false),
            ("127.0.0.1", false),
            ("127.0.0.2", false),
            ("[::1]", false),
            ("[::2]", true),
            ("barbaz.net", true),
            ("www.barbaz.net", false),
            ("foobar.com", false),
            ("www.foobar.com", false),
            ("foofoobar.com", true),
            ("baz.com", true),
            ("localhost.net", true),
            ("local.localhost", true),
            ("barbarbaz.net", true),
            ("wildcard.io", true),
            ("nested.wildcard.io", false),
            ("awildcard.io", true),
            ("192.168.1.1", false),
            ("192.168.1.2", true),
            ("192.168.1.3", false),
            ("192.168.1.4", true),
            ("10.0.0.2", false),
            ("[2001:db8::52:0:1]", false),
            ("[2001:db8::52:0:2]", true),
            ("[2001:db8::52:0:3]", false),
            ("[2002:db8:a::123]", false),
            ("[fe80::424b:c8be:1643:a1b6]", true),
            ("bar.com", false),
            ("www.bar.com", false),
        ];
        for (host, proxied) in cases {
            let url = format!("http://{host}");
            assert_eq!(through(&p, &url).is_some(), *proxied, "{host}");
        }
        let all = proxies(&[("HTTPS_PROXY", "proxy"), ("no_proxy", "*")]);
        assert_eq!(through(&all, "https://example.com"), None);
        // x/net's proxyForURLTests of NO_PROXY's domains.
        for (no_proxy, url, proxied) in [
            ("example.com", "http://example.com/", false),
            (".example.com", "http://example.com/", true),
            ("ample.com", "http://example.com/", true),
            ("example.com", "http://foo.example.com/", false),
            (".foo.com", "http://example.com/", true),
        ] {
            let p = proxies(&[("HTTP_PROXY", "proxy"), ("NO_PROXY", no_proxy)]);
            assert_eq!(through(&p, url).is_some(), proxied, "{no_proxy} {url}");
        }
    }

    /// A proxy's userinfo makes its Proxy-Authorization, and is never shown.
    #[test]
    fn a_proxys_credentials_authorize_it_and_stay_hidden() {
        let p = proxies(&[("HTTPS_PROXY", "http://us%3Aer:p%40ss@proxy.tld:3128")]);
        let proxy = p
            .for_url(&Url::parse("https://registry.example/v2/").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(proxy.url.to_string(), "http://proxy.tld:3128/");
        assert_eq!(
            proxy.authorization.as_deref(),
            Some(basic(b"us:er", b"p@ss").as_str())
        );
        assert!(!format!("{proxy:?}").contains("p@ss") && !format!("{p:?}").contains("p@ss"));
        let socks = proxies(&[("HTTPS_PROXY", "socks5://proxy.tld:1080")]);
        let e = socks
            .for_url(&Url::parse("https://registry.example/").unwrap())
            .unwrap_err();
        assert!(e.to_string().contains("not supported"), "{e}");
    }
}
