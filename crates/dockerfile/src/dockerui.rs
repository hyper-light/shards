//! The frontend's options that shape each `RUN`, as dockerui reads them (moby/buildkit
//! dockerfile/1.27.1 frontend/dockerui/attr.go, config.go): `add-hosts`, `shm-size`,
//! `force-network-mode`, `cgroup-parent` and the resource limits (`memory`, `memswap`,
//! `cpushares`, `cpuperiod`, `cpuquota`, `cpusetcpus`, `cpusetmems`), each in its errors'
//! words.

use std::collections::BTreeMap;

use shards_cmdline::go;

use crate::llb::{HostIp, LinuxResources, NetMode};

/// `parseExtraHosts`: CSV fields `host=ip`, lowercased, each IP as Go's `net.IP` writes
/// it.
pub fn extra_hosts(v: &str) -> Result<Vec<HostIp>, String> {
    if v.is_empty() {
        return Ok(Vec::new());
    }
    let fields = go::csv_fields(v.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let mut out = Vec::new();
    for field in fields {
        let field = String::from_utf8_lossy(&field).to_lowercase();
        let Some((host, ip)) = field.split_once('=') else {
            return Err(format!("invalid key-value pair {field}"));
        };
        let parsed: std::net::IpAddr = ip.parse().map_err(|_| format!("failed to parse IP {ip}"))?;
        // net.IP's String: an IPv4-mapped IPv6 address as IPv4.
        let shown = match parsed {
            std::net::IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
            v4 => v4.to_string(),
        };
        out.push(HostIp {
            host: host.as_bytes().to_vec(),
            ip: shown.into_bytes(),
        });
    }
    Ok(out)
}

/// `parseShmSize`: bytes, as `strconv.ParseInt` reads them; 0 where none.
pub fn shm_size(v: &str) -> Result<i64, String> {
    if v.is_empty() {
        return Ok(0);
    }
    go::parse_int10(v).map_err(|e| e.to_string())
}

/// `parseNetMode`: the network each stage's steps have unless one says otherwise.
/// `parseResolveMode`: the frontend's `image-resolve-mode` as a stage's image source says
/// it (`image.resolvemode`), nothing for the default.
pub fn resolve_mode(v: &str) -> Result<&'static [u8], String> {
    match v {
        "" | "default" => Ok(b""),
        "pull" => Ok(b"pull"),
        "local" => Ok(b"local"),
        _ => Err(format!("invalid image-resolve-mode: {v}")),
    }
}

pub fn net_mode(v: &str) -> Result<NetMode, String> {
    match v {
        "" | "sandbox" => Ok(NetMode::Sandbox),
        "none" => Ok(NetMode::None),
        "host" => Ok(NetMode::Host),
        _ => Err(format!("invalid netmode {v}")),
    }
}

/// `cpuset.Validate`: CSV of CPUs or ranges, none past 8192.
fn cpuset(s: &str) -> Result<(), String> {
    const MAX: i64 = 8192;
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            None => {
                let n = go::parse_int10(part)
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| format!("invalid cpuset element {}", go::quote(part)))?;
                if n > MAX {
                    return Err(format!(
                        "cpuset element {} exceeds maximum {MAX}",
                        go::quote(part)
                    ));
                }
            }
            Some((lo, hi)) => {
                let (lo, hi) = (go::parse_int10(lo.trim()), go::parse_int10(hi.trim()));
                let (Ok(lo), Ok(hi)) = (lo, hi) else {
                    return Err(format!("invalid cpuset range {}", go::quote(part)));
                };
                if lo < 0 || hi < lo {
                    return Err(format!("invalid cpuset range {}", go::quote(part)));
                }
                if hi > MAX {
                    return Err(format!("cpuset range {} exceeds maximum {MAX}", go::quote(part)));
                }
            }
        }
    }
    Ok(())
}

/// `parseLinuxResources`: none where no limit is set.
pub fn linux_resources(opts: &BTreeMap<String, String>) -> Result<Option<LinuxResources>, String> {
    let mut res = LinuxResources::default();
    let get = |k: &str| opts.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let signed = |k: &str, v: &str| go::parse_int10(v).map_err(|e| format!("invalid {k} value: {v}: {e}"));
    let unsigned =
        |k: &str, v: &str| go::parse_uint_bits(v, 64).map_err(|e| format!("invalid {k} value: {v}: {e}"));
    if let Some(v) = get("memory") {
        let n = signed("memory", v)?;
        if n <= 0 {
            return Err(format!("invalid memory value: {v}: must be > 0"));
        }
        res.memory = n;
    }
    if let Some(v) = get("memswap") {
        let n = signed("memswap", v)?;
        if n < -1 || n == 0 {
            return Err(format!(
                "invalid memswap value: {v}: must be -1 (unlimited) or > 0"
            ));
        }
        res.memory_swap = n;
    }
    if let Some(v) = get("cpushares") {
        let n = unsigned("cpushares", v)?;
        if n == 0 {
            return Err(format!("invalid cpushares value: {v}: must be > 0"));
        }
        res.cpu_shares = n;
    }
    if let Some(v) = get("cpuperiod") {
        let n = unsigned("cpuperiod", v)?;
        if n == 0 {
            return Err(format!("invalid cpuperiod value: {v}: must be > 0"));
        }
        res.cpu_period = n;
    }
    if let Some(v) = get("cpuquota") {
        let n = signed("cpuquota", v)?;
        if n <= 0 {
            return Err(format!("invalid cpuquota value: {v}: must be > 0"));
        }
        res.cpu_quota = n;
    }
    for (k, field) in [
        ("cpusetcpus", &mut res.cpuset_cpus),
        ("cpusetmems", &mut res.cpuset_mems),
    ] {
        if let Some(v) = get(k) {
            cpuset(v).map_err(|e| format!("invalid {k} value: {v}: {e}"))?;
            *field = v.as_bytes().to_vec();
        }
    }
    Ok((res != LinuxResources::default()).then_some(res))
}

/// Whether a download's first bytes are an archive's, as dockerui's `isArchive` reads
/// them: bzip2, gzip, xz or zstd's magic, or a zstd skippable frame's (0x184D2A50 to
/// 0x184D2A5F, little-endian), else a tar header Go's `tar.Reader` takes (a block whose
/// checksum, unsigned or signed, is its own).
pub fn is_archive(header: &[u8]) -> bool {
    const MAGIC: [&[u8]; 4] = [
        &[0x42, 0x5A, 0x68],
        &[0x1F, 0x8B, 0x08],
        &[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00],
        &[0x28, 0xB5, 0x2F, 0xFD],
    ];
    if MAGIC.iter().any(|m| header.starts_with(m)) {
        return true;
    }
    if header.len() >= 8
        && let Some(first) = header.get(..4).and_then(|b| <[u8; 4]>::try_from(b).ok())
        && u32::from_le_bytes(first) & 0xFFFF_FFF0 == 0x184D_2A50
    {
        return true;
    }
    let Some(block) = header.get(..512) else {
        return false;
    };
    if block.iter().all(|&b| b == 0) {
        return false;
    }
    // The checksum field, octal, as Go's parseOctal reads it: spaces and NULs around it.
    let Some(field) = block.get(148..156) else {
        return false;
    };
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|&b| b == b' ' || b == 0)
        .take_while(|&b| b != b' ' && b != 0)
        .collect();
    let Some(want) = std::str::from_utf8(&digits)
        .ok()
        .and_then(|d| i64::from_str_radix(d, 8).ok())
    else {
        return false;
    };
    let (mut unsigned, mut signed) = (0i64, 0i64);
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += i64::from(b);
        signed += i64::from(b as i8);
    }
    want == unsigned || want == signed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_read_as_dockerui_reads_them() {
        let hosts = extra_hosts("Db=10.0.0.2,v6=::FFFF:1.2.3.4,x=fe80::1").unwrap();
        let shown: Vec<(String, String)> = hosts
            .iter()
            .map(|h| {
                (
                    String::from_utf8_lossy(&h.host).into(),
                    String::from_utf8_lossy(&h.ip).into(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [("db", "10.0.0.2"), ("v6", "1.2.3.4"), ("x", "fe80::1")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        assert_eq!(extra_hosts("db").unwrap_err(), "invalid key-value pair db");
        assert_eq!(extra_hosts("db=nope").unwrap_err(), "failed to parse IP nope");
        assert_eq!(shm_size("67108864").unwrap(), 67_108_864);
        assert_eq!(net_mode("none").unwrap(), NetMode::None);
        assert_eq!(net_mode("bridge").unwrap_err(), "invalid netmode bridge");
        let opts = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        assert_eq!(linux_resources(&opts(&[])).unwrap(), None);
        let r = linux_resources(&opts(&[("memory", "1073741824"), ("cpusetcpus", "0-3,5")]))
            .unwrap()
            .unwrap();
        assert_eq!(
            (r.memory, r.cpuset_cpus.as_slice()),
            (1_073_741_824, b"0-3,5".as_slice())
        );
        assert_eq!(
            linux_resources(&opts(&[("memswap", "0")])).unwrap_err(),
            "invalid memswap value: 0: must be -1 (unlimited) or > 0"
        );
        assert_eq!(
            linux_resources(&opts(&[("cpusetcpus", "3-1")])).unwrap_err(),
            "invalid cpusetcpus value: 3-1: invalid cpuset range \"3-1\""
        );
    }

    /// dockerfile/1.27.1's isArchive: zstd's frame and skippable frames are archives,
    /// a skippable frame's magic only with the eight bytes of its header.
    #[test]
    fn zstd_downloads_are_archives() {
        assert!(is_archive(&[0x28, 0xB5, 0x2F, 0xFD, 0x00]));
        assert!(is_archive(&[0x5F, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]));
        assert!(is_archive(&[0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, 1]));
        assert!(!is_archive(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0]));
        assert!(!is_archive(&[0x60, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]));
        assert!(!is_archive(b"FROM alpine\n"));
    }
}
