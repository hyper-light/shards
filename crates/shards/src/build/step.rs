//! What a `RUN` step is given besides its tree, as BuildKit gives it
//! (docs/research/buildkit-run.md): its environment, `/etc/hosts`, `/etc/resolv.conf`,
//! and the words of its failure.

use std::net::IpAddr;

use shards_dockerfile::go;
use shards_dockerfile::llb::{HostIp, ProxyEnv};

/// The `PATH` a step gets when its environment has none (util/system/path.go).
pub const DEFAULT_PATH: &[u8] = b"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// BuildKit's default sandbox hostname (executor/oci/hosts.go).
pub const HOSTNAME: &[u8] = b"buildkitsandbox";

/// The step's environment before runc prepares it: the step's own, then the proxy
/// variables, each set one given upper then lower case, then `PATH` if it has none, then
/// secrets as variables (solver/llbsolver/ops/exec.go).
pub fn env(own: &[Vec<u8>], proxy: Option<&ProxyEnv>, secrets: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
    let mut out = own.to_vec();
    if let Some(p) = proxy {
        for (upper, lower, value) in [
            ("HTTP_PROXY", "http_proxy", &p.http),
            ("HTTPS_PROXY", "https_proxy", &p.https),
            ("FTP_PROXY", "ftp_proxy", &p.ftp),
            ("NO_PROXY", "no_proxy", &p.no),
            ("ALL_PROXY", "all_proxy", &p.all),
        ] {
            if value.is_empty() {
                continue;
            }
            for name in [upper, lower] {
                let mut kv = name.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(value);
                out.push(kv);
            }
        }
    }
    if !out.iter().any(|kv| kv.starts_with(b"PATH=")) {
        out.push(DEFAULT_PATH.to_vec());
    }
    for (name, value) in secrets {
        let mut kv = name.clone();
        kv.push(b'=');
        kv.extend_from_slice(value);
        out.push(kv);
    }
    out
}

/// `/etc/hosts` as BuildKit writes it (executor/oci/hosts.go, makeHostsFile).
pub fn hosts(hostname: &[u8], extra: &[HostIp]) -> Vec<u8> {
    let mut out = b"127.0.0.1\tlocalhost ".to_vec();
    out.extend_from_slice(hostname);
    out.extend_from_slice(b"\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    for h in extra {
        out.extend_from_slice(&h.ip);
        out.push(b'\t');
        out.extend_from_slice(&h.host);
        out.push(b'\n');
    }
    out
}

/// The host's resolvers, as BuildKit (executor/oci resolvconfPath) and dockerd
/// (libnetwork resolvconf.Path) read them: /etc/resolv.conf, unless its one nameserver is
/// systemd-resolved's stub, 127.0.0.53, which within a guest is the guest's own address;
/// then the servers systemd-resolved forwards to, which it lists in
/// /run/systemd/resolve/resolv.conf.
pub fn host_resolv() -> Vec<u8> {
    pick_resolv(std::fs::read("/etc/resolv.conf").unwrap_or_default(), || {
        std::fs::read("/run/systemd/resolve/resolv.conf").unwrap_or_default()
    })
}

fn pick_resolv(main: Vec<u8>, systemd: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&main);
    let servers: Vec<IpAddr> = text
        .lines()
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["nameserver", addr, ..] => addr.parse().ok(),
                _ => None,
            },
        )
        .collect();
    if servers == [IpAddr::from([127, 0, 0, 53])] {
        return systemd();
    }
    main
}

/// `/etc/resolv.conf` made from the host's as BuildKit makes it for a step without the
/// host's network (util/resolvconf: Parse, TransformForLegacyNw(true), Generate(false)),
/// and as dockerd makes it for a container on a network without IPv6
/// (TransformForLegacyNw(false)): nameservers less loopback ones, and less IPv6 ones
/// without `ipv6`, or Google's when none remain; the last `search`; every `options`;
/// other lines as they were; no comments, which name the engine that wrote them.
pub fn resolv(host: &[u8], ipv6: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(host);
    let mut nameservers: Vec<IpAddr> = Vec::new();
    let mut search: Vec<&str> = Vec::new();
    let mut options: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for line in text.lines() {
        // bufio.ScanLines drops a line's trailing \r.
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [] => {}
            ["nameserver", addr, ..] => {
                if let Ok(a) = addr.parse::<IpAddr>() {
                    nameservers.push(a);
                }
            }
            ["nameserver"] | ["domain"] | ["search"] | ["options"] => {}
            ["domain" | "search", rest @ ..] => search = rest.to_vec(),
            ["options", rest @ ..] => options.extend_from_slice(rest),
            _ => other.push(line),
        }
    }
    // netip's Is6 holds for an IPv4-mapped address too.
    nameservers.retain(|a| !loopback(a) && (ipv6 || a.is_ipv4()));
    if nameservers.is_empty() {
        nameservers = [
            "8.8.8.8",
            "8.8.4.4",
            "2001:4860:4860::8888",
            "2001:4860:4860::8844",
        ]
        .iter()
        .filter_map(|a| a.parse::<IpAddr>().ok())
        .filter(|a| ipv6 || a.is_ipv4())
        .collect();
    }
    let mut out = String::new();
    for ns in &nameservers {
        out.push_str(&format!("nameserver {ns}\n"));
    }
    if !search.is_empty() {
        out.push_str(&format!("search {}\n", search.join(" ")));
    }
    if !options.is_empty() {
        out.push_str(&format!("options {}\n", options.join(" ")));
    }
    for o in other {
        out.push_str(o);
        out.push('\n');
    }
    out.into_bytes()
}

/// netip.Addr.IsLoopback: 127.0.0.0/8 and ::1, an IPv4 address in IPv6 by its IPv4 form.
fn loopback(a: &IpAddr) -> bool {
    match a {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(v6.is_loopback(), |v4| v4.is_loopback()),
    }
}

/// How a failed step is reported (solver/llbsolver/ops/exec.go): its arguments joined
/// with spaces and Go-quoted, and why.
pub fn failure(args: &[Vec<u8>], why: &str) -> String {
    let joined = args.join(&b' ');
    format!(
        "process {} did not complete successfully: {why}",
        go::quote(&joined)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// systemd-resolved's stub, alone, sends the reader to the servers it forwards to;
    /// any other list, the stub among others included, is read as it is.
    #[test]
    fn systemd_resolveds_stub_is_read_past() {
        let systemd = || b"nameserver 10.0.0.2\n".to_vec();
        for (main, want) in [
            (
                &b"nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n"[..],
                &b"nameserver 10.0.0.2\n"[..],
            ),
            (b"# systemd\nnameserver   127.0.0.53\n", b"nameserver 10.0.0.2\n"),
            (
                b"nameserver 127.0.0.53\nnameserver 1.1.1.1\n",
                b"nameserver 127.0.0.53\nnameserver 1.1.1.1\n",
            ),
            (b"nameserver 192.168.1.1\n", b"nameserver 192.168.1.1\n"),
            (b"", b""),
        ] {
            assert_eq!(
                pick_resolv(main.to_vec(), systemd),
                want,
                "{}",
                String::from_utf8_lossy(main)
            );
        }
    }

    #[test]
    fn a_steps_environment_is_buildkits() {
        let own = vec![b"A=1".to_vec()];
        let proxy = ProxyEnv {
            http: b"http://p:3128".to_vec(),
            no: b"localhost".to_vec(),
            ..ProxyEnv::default()
        };
        assert_eq!(
            env(&own, Some(&proxy), &[(b"TOKEN".to_vec(), b"s".to_vec())]),
            [
                &b"A=1"[..],
                b"HTTP_PROXY=http://p:3128",
                b"http_proxy=http://p:3128",
                b"NO_PROXY=localhost",
                b"no_proxy=localhost",
                DEFAULT_PATH,
                b"TOKEN=s",
            ]
        );
        let own = vec![b"PATH=/x".to_vec()];
        assert_eq!(env(&own, None, &[]), [b"PATH=/x".to_vec()]);
    }

    #[test]
    fn hosts_are_buildkits() {
        let extra = [HostIp {
            host: b"db".to_vec(),
            ip: b"10.0.0.2".to_vec(),
        }];
        assert_eq!(
            hosts(HOSTNAME, &extra),
            b"127.0.0.1\tlocalhost buildkitsandbox\n::1\tlocalhost ip6-localhost ip6-loopback\n10.0.0.2\tdb\n"
        );
    }

    #[test]
    fn resolv_conf_is_the_hosts_less_loopback() {
        let host = b"# mDNSResponder\nnameserver 127.0.0.53\nnameserver ::ffff:127.0.0.1\nnameserver 192.168.1.1\nnameserver fe80:0:0::1\nsearch a b\nsearch home.lan\noptions ndots:2\noptions edns0\nsortlist 10.0.0.0\r\n";
        assert_eq!(
            String::from_utf8(resolv(host, true)).unwrap(),
            "nameserver 192.168.1.1\nnameserver fe80::1\nsearch home.lan\noptions ndots:2 edns0\nsortlist 10.0.0.0\n"
        );
        assert_eq!(
            String::from_utf8(resolv(b"nameserver 127.0.0.1\n", true)).unwrap(),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\nnameserver 2001:4860:4860::8888\nnameserver 2001:4860:4860::8844\n"
        );
        // Without IPv6, as dockerd writes it for its default bridge (moby
        // daemon/libnetwork/internal/resolvconf, docker-v29.3.1).
        let mapped = b"nameserver ::ffff:10.0.0.1\nnameserver 2001:db8::1\nnameserver 192.168.1.1\n";
        assert_eq!(
            String::from_utf8(resolv(mapped, false)).unwrap(),
            "nameserver 192.168.1.1\n"
        );
        assert_eq!(
            String::from_utf8(resolv(b"nameserver 2001:db8::1\n", false)).unwrap(),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
    }

    #[test]
    fn a_failure_is_worded_as_buildkit_words_it() {
        let args = [
            b"/bin/sh".to_vec(),
            b"-c".to_vec(),
            b"echo \"hi\"; exit 3".to_vec(),
        ];
        assert_eq!(
            failure(&args, "exit code: 3"),
            r#"process "/bin/sh -c echo \"hi\"; exit 3" did not complete successfully: exit code: 3"#
        );
    }
}
