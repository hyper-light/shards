//! A Sigstore signing config, as sigstore-go v1.2.2 reads it and picks the services to
//! sign with (pkg/root/signing_config.go): protojson of protobuf-specs v0.5.1's
//! SigningConfig, of media type `application/vnd.dev.sigstore.signingconfig.v0.2+json`;
//! each service by its API version and the time it is valid for, the newest first.

use crate::proto::{self, Msg, Val};
use crate::schemas::{self, SERVICE_SELECTOR};

pub const MEDIA_TYPE: &str = "application/vnd.dev.sigstore.signingconfig.v0.2+json";

/// A service: its URL, the major version of its API, when it is valid (seconds and
/// nanoseconds since 1970, where given), and who runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub url: String,
    pub major_api_version: u32,
    pub start: Option<(i64, u32)>,
    pub end: Option<(i64, u32)>,
    pub operator: String,
}

impl Service {
    /// ValidAtTime: not before its start, not after its end.
    pub fn valid_at(&self, now: (i64, u32)) -> bool {
        if self.start.is_some_and(|start| now < start) {
            return false;
        }
        !self.end.is_some_and(|end| now > end)
    }
}

/// How many of a kind of service to use: every one, any one, or so many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    Undefined,
    All,
    Any,
    Exact,
    /// A value no name stands for, which protojson keeps.
    Other(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceConfiguration {
    pub selector: Selector,
    pub count: u32,
}

/// The services a signature may use: certificate authorities (Fulcio), OIDC providers,
/// transparency logs (Rekor) and timestamp authorities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningConfig {
    pub ca: Vec<Service>,
    pub oidc: Vec<Service>,
    pub rekor: Vec<Service>,
    pub rekor_config: ServiceConfiguration,
    pub tsa: Vec<Service>,
    pub tsa_config: ServiceConfiguration,
}

fn service(m: &Msg) -> Service {
    let range = m.msg(3);
    Service {
        url: m.string(1),
        major_api_version: u32::try_from(m.uint(2)).unwrap_or(0),
        start: range.and_then(|r| r.time(1)),
        end: range.and_then(|r| r.time(2)),
        operator: m.string(4),
    }
}

fn services(m: &Msg, number: u32) -> Vec<Service> {
    m.list(number)
        .iter()
        .filter_map(|v| match v {
            Val::Msg(s) => Some(service(s)),
            _ => None,
        })
        .collect()
}

fn configuration(m: &Msg, number: u32) -> ServiceConfiguration {
    let c = m.msg(number);
    let selector = match c.map_or(0, |c| c.enumeration(1)) {
        0 => Selector::Undefined,
        1 => Selector::All,
        2 => Selector::Any,
        3 => Selector::Exact,
        n => Selector::Other(n),
    };
    ServiceConfiguration {
        selector,
        count: c.map_or(0, |c| u32::try_from(c.uint(2)).unwrap_or(0)),
    }
}

/// NewSigningConfigFromJSON: the config protojson reads, of version 0.2's media type.
pub fn parse(json: &[u8]) -> Result<SigningConfig, String> {
    let m = proto::unmarshal(json, &schemas::SIGNING_CONFIG).map_err(|e| e.0)?;
    let media_type = m.string(5);
    if media_type != MEDIA_TYPE {
        return Err(format!("unsupported SigningConfig media type: {media_type}"));
    }
    Ok(SigningConfig {
        ca: services(&m, 6),
        oidc: services(&m, 7),
        rekor: services(&m, 8),
        rekor_config: configuration(&m, 9),
        tsa: services(&m, 10),
        tsa_config: configuration(&m, 11),
    })
}

/// The selector's name, as protojson names it.
pub fn selector_name(s: Selector) -> String {
    let n = match s {
        Selector::Undefined => 0,
        Selector::All => 1,
        Selector::Any => 2,
        Selector::Exact => 3,
        Selector::Other(n) => n,
    };
    SERVICE_SELECTOR
        .iter()
        .find(|(_, v)| *v == n)
        .map_or_else(|| n.to_string(), |(name, _)| (*name).to_string())
}

/// Go's `%v` of a `[]uint32`: `[1 2]`.
fn versions_text(v: &[u32]) -> String {
    let parts: Vec<String> = v.iter().map(u32::to_string).collect();
    format!("[{}]", parts.join(" "))
}

/// Go's `%v` of a time in UTC (Time.String, no monotonic reading): `2006-01-02
/// 15:04:05.999999999 +0000 UTC`.
pub fn time_text(now: (i64, u32)) -> String {
    let t = crate::time::Time::utc(now.0, 0).rfc3339();
    // rfc3339 is `YYYY-MM-DDTHH:MM:SSZ`; the fraction's trailing zeros go, and with none
    // left, its point.
    let body = t.trim_end_matches('Z').replacen('T', " ", 1);
    let frac = format!("{:09}", now.1);
    let frac = frac.trim_end_matches('0');
    let frac = if frac.is_empty() {
        String::new()
    } else {
        format!(".{frac}")
    };
    format!("{body}{frac} +0000 UTC")
}

/// The services, newest start first: sigstore-go sorts them by start (stable at the
/// sizes signing configs have: Go's pdqsort takes insertion sort below 13) and
/// reverses them.
fn newest_first(services: &[Service]) -> Vec<Service> {
    let mut sorted = services.to_vec();
    sorted.sort_by(|a, b| {
        a.start
            .unwrap_or((i64::MIN, 0))
            .cmp(&b.start.unwrap_or((i64::MIN, 0)))
    });
    sorted.reverse();
    sorted
}

fn versions_newest_first(supported: &[u32]) -> Vec<u32> {
    let mut v = supported.to_vec();
    v.sort_unstable();
    v.reverse();
    v
}

/// SelectService: the newest service valid at `now` of the highest API version the
/// client supports.
pub fn select_service(services: &[Service], supported: &[u32], now: (i64, u32)) -> Result<Service, String> {
    if supported.is_empty() {
        return Err("no supported API versions".into());
    }
    let sorted = newest_first(services);
    for version in versions_newest_first(supported) {
        if let Some(s) = sorted
            .iter()
            .find(|s| s.major_api_version == version && s.valid_at(now))
        {
            return Ok(s.clone());
        }
    }
    Err(format!(
        "no matching service found for API versions {} and current time {}",
        versions_text(supported),
        time_text(now)
    ))
}

/// SelectServices: of the highest API version any valid service has, one service per
/// operator, newest first; then every one (ALL), one at random (ANY), or `count` at
/// random (EXACT). `below(n)` is a random number below n.
pub fn select_services(
    services: &[Service],
    config: ServiceConfiguration,
    supported: &[u32],
    now: (i64, u32),
    below: &mut dyn FnMut(usize) -> Result<usize, String>,
) -> Result<Vec<Service>, String> {
    if supported.is_empty() {
        return Err("no supported API versions".into());
    }
    let sorted = newest_first(services);
    let mut operators: Vec<&str> = Vec::new();
    let mut selected: Vec<Service> = Vec::new();
    for version in versions_newest_first(supported) {
        for s in &sorted {
            if s.major_api_version == version && s.valid_at(now) && !operators.contains(&s.operator.as_str())
            {
                operators.push(&s.operator);
                selected.push(s.clone());
            }
        }
        if !selected.is_empty() {
            break;
        }
    }
    if selected.is_empty() {
        return Err(format!(
            "no matching services found for API versions {} and current time {}",
            versions_text(supported),
            time_text(now)
        ));
    }
    match config.selector {
        Selector::All => Ok(selected),
        Selector::Any => {
            let i = below(selected.len())?;
            Ok(selected.get(i).cloned().into_iter().collect())
        }
        Selector::Exact => {
            if config.count == 0 {
                return Err("service selector count must be greater than 0".into());
            }
            let count = usize::try_from(config.count).unwrap_or(usize::MAX);
            if count > selected.len() {
                return Err(format!(
                    "service selector count {} must be less than or equal to the slice length {}",
                    config.count,
                    selected.len()
                ));
            }
            // selectExact: each pick swapped with the last and the last dropped.
            let mut left = selected;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let i = below(left.len())?;
                let last = left.len().saturating_sub(1);
                left.swap(i.min(last), last);
                if let Some(s) = left.pop() {
                    out.push(s);
                }
            }
            Ok(out)
        }
        Selector::Undefined | Selector::Other(_) => Err("invalid service selector".into()),
    }
}

/// A uniformly random number below `n` (math/rand's Intn, from the system's generator).
pub fn random_below(n: usize) -> Result<usize, String> {
    if n == 0 {
        return Err("invalid argument to Intn".into());
    }
    let n = u64::try_from(n).map_err(|_| "invalid argument to Intn")?;
    // The largest multiple of n that fits, below which a draw is unbiased.
    let limit = u64::MAX - (u64::MAX % n);
    loop {
        let mut b = [0u8; 8];
        aws_lc_rs::rand::fill(&mut b).map_err(|_| "reading random bytes")?;
        let v = u64::from_le_bytes(b);
        if v < limit {
            return usize::try_from(v % n).map_err(|_| "a random index past usize".into());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> (i64, u32) {
        shards_dockerfile::go::parse_rfc3339(rfc3339.as_bytes())
            .unwrap()
            .unix()
    }

    fn svc(url: &str, version: u32, start: &str, operator: &str) -> Service {
        Service {
            url: url.into(),
            major_api_version: version,
            start: Some(at(start)),
            end: None,
            operator: operator.into(),
        }
    }

    /// The public instance's config as Sigstore's TUF repository serves it
    /// (root-signing targets/signing_config.v0.2.json, 2026-10-11).
    const PUBLIC: &str = r#"{
  "mediaType": "application/vnd.dev.sigstore.signingconfig.v0.2+json",
  "caUrls": [{"url": "https://fulcio.sigstore.dev", "majorApiVersion": 1,
    "validFor": {"start": "2022-04-13T20:06:15.000Z"}, "operator": "sigstore.dev"}],
  "oidcUrls": [{"url": "https://oauth2.sigstore.dev/auth", "majorApiVersion": 1,
    "validFor": {"start": "2022-04-13T20:06:15.000Z"}, "operator": "sigstore.dev"}],
  "rekorTlogUrls": [{"url": "https://rekor.sigstore.dev", "majorApiVersion": 1,
    "validFor": {"start": "2021-01-12T11:53:27.000Z"}, "operator": "sigstore.dev"}],
  "tsaUrls": [{"url": "https://timestamp.sigstore.dev/api/v1/timestamp", "majorApiVersion": 1,
    "validFor": {"start": "2025-07-04T00:00:00Z"}, "operator": "sigstore.dev"}],
  "rekorTlogConfig": {"selector": "ANY"},
  "tsaConfig": {"selector": "ANY"}
}"#;

    #[test]
    fn the_public_config_reads_as_sigstore_go_reads_it() {
        let c = parse(PUBLIC.as_bytes()).unwrap();
        assert_eq!(
            c.ca,
            vec![svc(
                "https://fulcio.sigstore.dev",
                1,
                "2022-04-13T20:06:15Z",
                "sigstore.dev"
            )]
        );
        assert_eq!(c.rekor[0].url, "https://rekor.sigstore.dev");
        assert_eq!(c.tsa[0].url, "https://timestamp.sigstore.dev/api/v1/timestamp");
        assert_eq!(
            c.rekor_config,
            ServiceConfiguration {
                selector: Selector::Any,
                count: 0
            }
        );
        assert_eq!(c.tsa_config.selector, Selector::Any);
        let now = at("2026-10-11T00:00:00Z");
        let fulcio = select_service(&c.ca, &[1], now).unwrap();
        assert_eq!(fulcio.url, "https://fulcio.sigstore.dev");
        let logs = select_services(&c.rekor, c.rekor_config, &[1, 2], now, &mut random_below).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].url, "https://rekor.sigstore.dev");
    }

    #[test]
    fn other_media_types_and_fields_are_refused_in_sigstore_go_s_words() {
        assert_eq!(
            parse(br#"{"mediaType": "application/vnd.dev.sigstore.signingconfig.v0.1+json"}"#).unwrap_err(),
            "unsupported SigningConfig media type: application/vnd.dev.sigstore.signingconfig.v0.1+json"
        );
        assert!(
            parse(br#"{"mediaType": "x", "caUrl": []}"#)
                .unwrap_err()
                .contains("unknown field \"caUrl\"")
        );
    }

    /// The newest valid service of the highest version supported; past the end, before
    /// the start, and of versions not supported, none.
    #[test]
    fn a_service_is_the_newest_valid_one_of_the_highest_version() {
        let mut old = svc("https://old", 1, "2020-01-01T00:00:00Z", "a");
        let new = svc("https://new", 1, "2024-01-01T00:00:00Z", "a");
        let v2 = svc("https://v2", 2, "2025-01-01T00:00:00Z", "a");
        let later = svc("https://later", 2, "2030-01-01T00:00:00Z", "a");
        let all = [old.clone(), new.clone(), v2.clone(), later];
        let now = at("2026-10-11T00:00:00Z");
        assert_eq!(select_service(&all, &[1], now).unwrap(), new);
        assert_eq!(select_service(&all, &[1, 2], now).unwrap(), v2);
        old.end = Some(at("2021-01-01T00:00:00Z"));
        assert_eq!(
            select_service(&[old.clone()], &[1], now).unwrap_err(),
            "no matching service found for API versions [1] and current time 2026-10-11 00:00:00 +0000 UTC"
        );
        assert_eq!(
            select_service(&[old], &[], now).unwrap_err(),
            "no supported API versions"
        );
        assert_eq!(
            time_text((1_760_140_800, 462_364_000)),
            "2025-10-11 00:00:00.462364 +0000 UTC"
        );
    }

    /// One service per operator (the first, newest, of each; services with no operator
    /// are one operator's), of the highest version any has; ALL keeps them in order,
    /// EXACT picks so many, each once.
    #[test]
    fn services_are_one_per_operator_then_selected() {
        let now = at("2026-10-11T00:00:00Z");
        let list = [
            svc("https://a1", 1, "2024-01-01T00:00:00Z", "a"),
            svc("https://a2", 1, "2025-01-01T00:00:00Z", "a"),
            svc("https://b", 1, "2023-01-01T00:00:00Z", "b"),
            svc("https://none1", 1, "2022-01-01T00:00:00Z", ""),
            svc("https://none2", 1, "2021-01-01T00:00:00Z", ""),
        ];
        let all = ServiceConfiguration {
            selector: Selector::All,
            count: 0,
        };
        let got = select_services(&list, all, &[1], now, &mut random_below).unwrap();
        let urls: Vec<&str> = got.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["https://a2", "https://b", "https://none1"]);
        let exact = ServiceConfiguration {
            selector: Selector::Exact,
            count: 2,
        };
        let mut draws = vec![0usize, 0].into_iter();
        let got = select_services(&list, exact, &[1], now, &mut |_| Ok(draws.next().unwrap())).unwrap();
        let urls: Vec<&str> = got.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["https://a2", "https://none1"]);
        let too_many = ServiceConfiguration {
            selector: Selector::Exact,
            count: 4,
        };
        assert_eq!(
            select_services(&list, too_many, &[1], now, &mut random_below).unwrap_err(),
            "service selector count 4 must be less than or equal to the slice length 3"
        );
        let none = ServiceConfiguration {
            selector: Selector::Exact,
            count: 0,
        };
        assert_eq!(
            select_services(&list, none, &[1], now, &mut random_below).unwrap_err(),
            "service selector count must be greater than 0"
        );
        let undefined = ServiceConfiguration {
            selector: Selector::Undefined,
            count: 0,
        };
        assert_eq!(
            select_services(&list, undefined, &[1], now, &mut random_below).unwrap_err(),
            "invalid service selector"
        );
        assert_eq!(
            select_services(&list, all, &[2], now, &mut random_below).unwrap_err(),
            "no matching services found for API versions [2] and current time 2026-10-11 00:00:00 +0000 UTC"
        );
    }
}
