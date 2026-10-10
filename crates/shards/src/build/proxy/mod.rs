//! A build's proxy (D110): buildx v0.37.1's `exec.proxy` policy cap, served as BuildKit
//! v0.33.0 serves it (util/network/proxyprovider). When a build's policies ask for the cap,
//! every `RUN` whose network is the builder's (BuildKit's default and host modes) reaches
//! this proxy alone: the builder's network process carries the gateway's [`PORT`] to its
//! Unix socket and nothing else anywhere. Each request through it is checked by the build's
//! policies as an HTTP source, on the build's thread, before it goes on:
//! - plain HTTP, as an absolute URL or a path with its `Host` (BuildKit's handler);
//! - HTTPS through CONNECT, answered with `200 Connection Established`, then TLS with a
//!   certificate the build's CA signs for the tunnel's host, each request in it checked
//!   as `https://` and the tunnel's host.
//!
//! A refusal is `403 Forbidden` as BuildKit sends it; each request let through goes on
//! with the hop-by-hop fields and `Accept-Encoding` taken off, as the client asked it,
//! through the proxies the build's environment names, trusting what the host trusts
//! (`tls::client_config`), and its response comes back as it was; a failure is a `502`.
//! What the step's requests come to is recorded (capture.rs): its log's lines and its
//! provenance's materials.
//!
//! The proxy serves a step's connections only while that step runs, on threads of the
//! builder's step (`Session::serve`); as the step ends, its connections are shut and its
//! requests upstream cancelled.

#[cfg(unix)]
pub mod ca;
#[cfg_attr(not(unix), allow(dead_code))]
pub mod capture;
#[cfg(unix)]
pub mod head;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod sniff;

#[cfg(unix)]
pub use self::server::{PORT, Proxy, Questions, Session};

/// Where shards starts no builder (Windows), there is no proxy to start either.
#[cfg(not(unix))]
#[derive(Debug)]
pub enum Proxy {}

#[cfg(not(unix))]
impl Proxy {
    pub fn ca(&self) -> &[u8] {
        match *self {}
    }
}

use shards_dockerfile::url as gourl;

/// What NO_PROXY says: the step's own loopback, never proxied.
const NO_PROXY: &str = "127.0.0.1,localhost,::1";

/// `network.ProxyEnv`: the eight variables a step under the proxy is given, `proxy` its URL,
/// in BuildKit's order.
pub fn env(proxy: &str) -> Vec<Vec<u8>> {
    [
        ("HTTP_PROXY", proxy),
        ("HTTPS_PROXY", proxy),
        ("ALL_PROXY", proxy),
        ("http_proxy", proxy),
        ("https_proxy", proxy),
        ("all_proxy", proxy),
        ("NO_PROXY", NO_PROXY),
        ("no_proxy", NO_PROXY),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={v}").into_bytes())
    .collect()
}

/// `executor.ReplaceEnv`: `env` without the variables `replacement` names, then
/// `replacement`.
pub fn replace_env(env: &[Vec<u8>], replacement: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let name = |kv: &[u8]| -> Vec<u8> { kv.split(|&b| b == b'=').next().unwrap_or_default().to_vec() };
    let names: Vec<Vec<u8>> = replacement.iter().map(|kv| name(kv)).collect();
    env.iter()
        .filter(|kv| !names.contains(&name(kv)))
        .chain(replacement)
        .cloned()
        .collect()
}

/// `upstreamProxyEnvironment`: the proxies the build's environment names for HTTP and
/// HTTPS, each a URL or `host[:port]` of http, https, socks5 or socks5h with a host, or
/// the step fails; their values are never said, as they may hold credentials.
pub fn check_upstream(env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    for [upper, lower] in [["HTTP_PROXY", "http_proxy"], ["HTTPS_PROXY", "https_proxy"]] {
        let Some((name, value)) = [upper, lower]
            .iter()
            .find_map(|n| env(n).filter(|v| !v.is_empty()).map(|v| (*n, v)))
        else {
            continue;
        };
        let parsed = match gourl::parse(value.as_bytes()) {
            Ok(u) if !u.scheme.is_empty() && !u.host.is_empty() => Ok(u),
            _ => gourl::parse(format!("http://{value}").as_bytes()),
        };
        let valid = parsed.is_ok_and(|u| {
            !u.host_port().0.is_empty()
                && matches!(u.scheme.as_slice(), b"http" | b"https" | b"socks5" | b"socks5h")
        });
        if !valid {
            return Err(format!("invalid {name} in the build's environment"));
        }
    }
    Ok(())
}
