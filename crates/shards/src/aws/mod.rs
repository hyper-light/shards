//! AWS, as builds and the daemon speak to it: Signature Version 4 (`sigv4`), for S3's cache
//! (D88) and CloudWatch Logs (D168); the shared config and credentials files (`profile`);
//! credentials as the SDKs' default chain finds them (`credentials`); and where a service
//! is, as aws-sdk-go-v2's endpoint rules say.

pub mod credentials;
pub mod profile;
pub mod sigv4;

use std::io::Read as _;
use std::time::{Duration, SystemTime};

use profile::Profile;
use serde_json::Value;
use shards_registry::http::{Client, Patience, Request};
use shards_registry::url::Url;

/// The variables besides AWS's own (`AWS_*`) that the SDK's configuration, or a
/// credential process it runs, reads: the home its files are in, `PATH`, the proxies.
const OTHERS: [&str; 10] = [
    "HOME",
    "USERPROFILE",
    "PATH",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// Whether the SDK's configuration, or a credential process it runs, may read variable
/// `name`.
pub fn reads(name: &str) -> bool {
    name.starts_with("AWS_") || OTHERS.contains(&name)
}

/// Every variable the chain, and the endpoints and proxies of what it asks, are found by
/// (aws-sdk-go-v2 config/env_config.go's, those it reads elsewhere, the proxies'), for a
/// caller that asks again later to keep what an environment says of them.
const NAMES: [&str; 44] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_ACCESS_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SECRET_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_EC2_METADATA_SERVICE_ENDPOINT",
    "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE",
    "AWS_EC2_METADATA_V1_DISABLED",
    "AWS_CA_BUNDLE",
    "AWS_USE_FIPS_ENDPOINT",
    "AWS_USE_DUALSTACK_ENDPOINT",
    "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
    "AWS_ENDPOINT_URL",
    "AWS_ENDPOINT_URL_STS",
    "AWS_ENDPOINT_URL_SSO",
    "AWS_ENDPOINT_URL_SSO_OIDC",
    "AWS_ENDPOINT_URL_SIGNIN",
    "AWS_ENDPOINT_URL_CLOUDWATCH_LOGS",
    "AWS_ENDPOINT_URL_S3",
    "AWS_LOGIN_CACHE_DIRECTORY",
    "HOME",
    "USERPROFILE",
    "PATH",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// What `env` says of the variables the chain is found by, `NAME=VALUE`.
pub fn snapshot(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    NAMES
        .iter()
        .filter_map(|k| Some(format!("{k}={}", env(k)?)))
        .collect()
}

/// The most of an answer read: AWS's answers to what shards asks are a few kilobytes.
const ANSWER: u64 = 1 << 20;

/// Sends `method` to `url` with `fields` and `body`: the answer's status and body, at
/// most a mebibyte of it.
pub fn exchange(
    client: &Client,
    method: &str,
    url: &str,
    fields: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let parsed = Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    send(client, method, &parsed, fields, body)
}

fn send(
    client: &Client,
    method: &str,
    url: &Url,
    fields: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let mut answer = client
        .send(&Request {
            method,
            url,
            headers: fields,
            body,
            file: None,
        })
        .map_err(|e| format!("{url}: {e}"))?;
    let status = answer.status;
    let mut out = Vec::new();
    (&mut answer)
        .take(ANSWER)
        .read_to_end(&mut out)
        .map_err(|e| format!("{url}: {e}"))?;
    Ok((status, out))
}

/// Posts `body` to `url` with `fields`, signed with `creds` for `scope` (SigV4): the
/// answer's status and body.
pub fn post_signed(
    client: &Client,
    url: &Url,
    fields: &[(&str, &str)],
    body: &[u8],
    creds: &sigv4::Credentials,
    scope: &sigv4::Scope<'_>,
) -> Result<(u16, Vec<u8>), String> {
    let target = url.target();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let added = sigv4::sign(
        &sigv4::Request {
            method: "POST",
            authority: &url.authority(),
            path,
            query,
            headers: fields,
            length: body.len() as u64,
            payload_hash: &sigv4::payload_hash(body),
        },
        creds,
        scope,
        &sigv4::amz_date(SystemTime::now()),
    );
    let mut all = fields.to_vec();
    all.extend(added.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    send(client, "POST", url, &all, body)
}

/// A setting the SDK reads as `true` or `false`, in any case; anything else is an error
/// (setBoolPtrFromEnvVal).
pub fn env_bool(env: &dyn Fn(&str) -> Option<String>, key: &str) -> Result<Option<bool>, String> {
    match env(key).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(v) => Err(format!("{key}={v}: true or false")),
    }
}

/// The HTTP client AWS is asked with: trusting the CA bundle the configuration names
/// (`AWS_CA_BUNDLE`, else the profile's `ca_bundle`) in place of the platform's roots, as
/// the SDK trusts it alone (resolveCustomCABundle), else the platform's; through the
/// environment's proxies; waiting as `patience` says.
pub fn client(
    env: &dyn Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    patience: Patience,
) -> Result<Client, String> {
    let bundle = env("AWS_CA_BUNDLE")
        .filter(|b| !b.is_empty())
        .or_else(|| profile.map(|p| p.ca_bundle.clone()).filter(|b| !b.is_empty()));
    let tls = match bundle {
        Some(path) => bundle_config(&path)?,
        None => shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?,
    };
    Ok(Client::new(
        Box::new(move |_| Ok(tls.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    )
    .with_proxies(shards_registry::proxy::Proxies::from_env(env))
    .with_patience(patience))
}

/// A TLS configuration trusting the certificates of the PEM bundle at `path` alone: one
/// that holds none it can use is refused, as the SDK refuses it (addX509CertsToPool).
fn bundle_config(path: &str) -> Result<std::sync::Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;
    let said = |e: &dyn std::fmt::Display| format!("the AWS CA bundle {path}: {e}");
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| said(&e))?;
    let mut roots = rustls::RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(said(&"no certificate in it"));
    }
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| said(&e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
}

/// `s` escaped as Go's url.QueryEscape escapes it: unreserved bytes kept, a space `+`,
/// every other byte `%XX` — a form's keys and values, as the SDK's query encoder writes
/// them.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A form's body: its pairs sorted by key, each escaped (query.Encoder's Encode).
pub fn form(pairs: &[(&str, &str)]) -> String {
    let mut sorted = pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// An object's field, as Go's encoding/json finds a struct's: by its name, else by one
/// that differs from it in case alone; null where there is none.
pub fn field<'v>(v: &'v Value, name: &str) -> &'v Value {
    v.as_object()
        .and_then(|o| {
            o.get(name).or_else(|| {
                o.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        })
        .unwrap_or(&Value::Null)
}

/// A field's text, where it is a string of some.
pub fn text(v: &Value, name: &str) -> Option<String> {
    field(v, name)
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An RFC 3339 time, as Go's time.Time reads one from JSON.
pub fn rfc3339(s: &str) -> Option<SystemTime> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    let nanos = u64::try_from(t.unix_timestamp_nanos()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos))
}

/// `t` to the second, as Go's time.RFC3339 writes it in UTC.
pub fn rfc3339_of(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    shards_cmdline::format::rfc3339_at(secs, 0)
}

/// A region's partition's DNS suffixes, its own and its dual-stack one, as the SDK's
/// partitions.json says (internal/endpoints/awsrulesfn): by the partitions' region
/// patterns, `aws`'s for any other.
pub fn suffixes(region: &str) -> (&'static str, &'static str) {
    let parts: Vec<&str> = region.split('-').collect();
    let numbered = |n: usize| {
        parts.len() == n
            && parts
                .last()
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            && parts.iter().all(|p| !p.is_empty())
    };
    match parts.as_slice() {
        ["cn", ..] if numbered(3) => ("amazonaws.com.cn", "api.amazonwebservices.com.cn"),
        ["eusc", "de", ..] if numbered(4) => ("amazonaws.eu", "api.amazonwebservices.eu"),
        ["us", "iso", ..] if numbered(4) => ("c2s.ic.gov", "api.aws.ic.gov"),
        ["us", "isob", ..] if numbered(4) => ("sc2s.sgov.gov", "api.aws.scloud"),
        ["eu", "isoe", ..] if numbered(4) => ("cloud.adc-e.uk", "api.cloud-aws.adc-e.uk"),
        ["us", "isof", ..] if numbered(4) => ("csp.hci.ic.gov", "api.aws.hci.ic.gov"),
        _ => ("amazonaws.com", "api.aws"),
    }
}

/// A service's endpoint in `region`, its FIPS or dual-stack one where asked, as the
/// endpoint rules of CloudWatch Logs (`logs`), STS (`sts`), SSO (`portal.sso`) and SSO OIDC
/// (`oidc`) make it (aws-sdk-go-v2 service/*/endpoints.go, their decision diagrams evaluated
/// for each partition): `{prefix}.{region}.{suffix}`; FIPS's `{prefix}-fips`, but in
/// GovCloud, where it is the plain name under `amazonaws.com`; dual-stack's under the
/// partition's dual-stack suffix.
pub fn endpoint(prefix: &str, region: &str, fips: bool, dual_stack: bool) -> String {
    let (dns, dual) = suffixes(region);
    match (fips, dual_stack) {
        (true, true) => format!("https://{prefix}-fips.{region}.{dual}"),
        (true, false) if region.starts_with("us-gov-") => format!("https://{prefix}.{region}.amazonaws.com"),
        (true, false) => format!("https://{prefix}-fips.{region}.{dns}"),
        (false, true) => format!("https://{prefix}.{region}.{dual}"),
        (false, false) => format!("https://{prefix}.{region}.{dns}"),
    }
}

/// Sign-in's endpoint in `region`, for its CreateOAuth2Token (a login session's refresh),
/// as its endpoint rules make it: a host of each partition's own, and FIPS's and
/// dual-stack's.
pub fn signin_endpoint(region: &str, fips: bool, dual_stack: bool) -> String {
    let (dns, dual) = suffixes(region);
    if dual_stack {
        return format!("https://signin.{region}.{dual}");
    }
    let gov = region.starts_with("us-gov-");
    if fips {
        return if gov {
            format!("https://{region}.signin-fips.amazonaws-us-gov.com")
        } else {
            format!("https://signin-fips.{region}.{dns}")
        };
    }
    let host = match dns {
        "amazonaws.com.cn" => "signin.amazonaws.cn",
        "amazonaws.eu" => "signin.amazonaws-eusc.eu",
        "c2s.ic.gov" => "signin.c2shome.ic.gov",
        "sc2s.sgov.gov" => "signin.sc2shome.sgov.gov",
        "cloud.adc-e.uk" => "signin.csphome.adc-e.uk",
        "csp.hci.ic.gov" => "signin.csphome.hci.ic.gov",
        _ if gov => "signin.amazonaws-us-gov.com",
        _ => "signin.aws.amazon.com",
    };
    format!("https://{region}.{host}")
}

/// The endpoint the configuration names for the service whose SDK ID is `id` (`STS`,
/// `CloudWatch Logs`), as aws-sdk-go-v2 resolves a base endpoint: `AWS_ENDPOINT_URL_<ID>`
/// (its ID upper case, spaces `_`), `AWS_ENDPOINT_URL`, the profile's services section's
/// `endpoint_url` for it (its ID lower case, spaces `_`), the profile's `endpoint_url`;
/// none where `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` or the profile says to ignore them.
pub fn configured_endpoint(
    env: &dyn Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    id: &str,
) -> Result<Option<String>, String> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    let ignore = env_bool(env, "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS")?
        .or_else(|| profile.and_then(|p| p.ignore_configured_endpoint_urls))
        .unwrap_or(false);
    if ignore {
        return Ok(None);
    }
    let env_id = id.to_ascii_uppercase().replace(' ', "_");
    let file_id = id.to_ascii_lowercase().replace(' ', "_");
    Ok(var(&format!("AWS_ENDPOINT_URL_{env_id}"))
        .or_else(|| var("AWS_ENDPOINT_URL"))
        .or_else(|| {
            profile
                .and_then(|p| p.services.get(&file_id))
                .and_then(|s| s.get("endpoint_url"))
                .filter(|v| !v.is_empty())
                .cloned()
        })
        .or_else(|| profile.map(|p| p.endpoint_url.clone()).filter(|v| !v.is_empty())))
}

/// An XML element's text: the first `<tag>…</tag>`'s, its entities decoded.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = xml.get(start..)?;
    let end = rest.find(&format!("</{tag}>"))?;
    Some(
        rest.get(..end)?
            .replace("&quot;", "\"")
            .replace("&#34;", "\"")
            .replace("&apos;", "'")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Services' endpoints, as their rules make them in each partition.
    #[test]
    fn endpoints_follow_their_partitions_rules() {
        assert_eq!(
            endpoint("logs", "us-east-1", false, false),
            "https://logs.us-east-1.amazonaws.com"
        );
        assert_eq!(
            endpoint("logs", "cn-north-1", false, false),
            "https://logs.cn-north-1.amazonaws.com.cn"
        );
        assert_eq!(
            endpoint("logs", "us-east-1", true, false),
            "https://logs-fips.us-east-1.amazonaws.com"
        );
        assert_eq!(
            endpoint("logs", "us-gov-west-1", true, false),
            "https://logs.us-gov-west-1.amazonaws.com"
        );
        assert_eq!(
            endpoint("logs", "eu-west-1", false, true),
            "https://logs.eu-west-1.api.aws"
        );
        assert_eq!(
            endpoint("logs", "eu-west-1", true, true),
            "https://logs-fips.eu-west-1.api.aws"
        );
        assert_eq!(
            endpoint("logs", "us-iso-east-1", false, false),
            "https://logs.us-iso-east-1.c2s.ic.gov"
        );
        assert_eq!(
            endpoint("logs", "eusc-de-east-1", false, false),
            "https://logs.eusc-de-east-1.amazonaws.eu"
        );
        assert_eq!(
            endpoint("portal.sso", "us-east-1", true, false),
            "https://portal.sso-fips.us-east-1.amazonaws.com"
        );
        assert_eq!(
            endpoint("sts", "us-gov-east-1", true, true),
            "https://sts-fips.us-gov-east-1.api.aws"
        );
        assert_eq!(
            signin_endpoint("us-east-1", false, false),
            "https://us-east-1.signin.aws.amazon.com"
        );
        assert_eq!(
            signin_endpoint("cn-north-1", false, false),
            "https://cn-north-1.signin.amazonaws.cn"
        );
        assert_eq!(
            signin_endpoint("us-gov-east-1", false, false),
            "https://us-gov-east-1.signin.amazonaws-us-gov.com"
        );
        assert_eq!(
            signin_endpoint("us-gov-east-1", true, false),
            "https://us-gov-east-1.signin-fips.amazonaws-us-gov.com"
        );
        assert_eq!(
            signin_endpoint("us-east-1", true, false),
            "https://signin-fips.us-east-1.amazonaws.com"
        );
    }

    /// A configured endpoint: the service's own variable, then the global one, then the
    /// profile's; none where they are to be ignored.
    #[test]
    fn configured_endpoints_are_found_in_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_string())
        };
        let mut profile = Profile {
            endpoint_url: "http://profile".into(),
            ..Profile::default()
        };
        profile.services.insert(
            "cloudwatch_logs".into(),
            [("endpoint_url".to_string(), "http://service".to_string())].into(),
        );
        let ours = env(&[("AWS_ENDPOINT_URL_CLOUDWATCH_LOGS", "http://var")]);
        assert_eq!(
            configured_endpoint(&ours, Some(&profile), "CloudWatch Logs")
                .unwrap()
                .unwrap(),
            "http://var"
        );
        let global = env(&[("AWS_ENDPOINT_URL", "http://global")]);
        assert_eq!(
            configured_endpoint(&global, Some(&profile), "CloudWatch Logs")
                .unwrap()
                .unwrap(),
            "http://global"
        );
        let none = env(&[]);
        assert_eq!(
            configured_endpoint(&none, Some(&profile), "CloudWatch Logs")
                .unwrap()
                .unwrap(),
            "http://service"
        );
        assert_eq!(
            configured_endpoint(&none, Some(&profile), "STS")
                .unwrap()
                .unwrap(),
            "http://profile"
        );
        let ignored = env(&[
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true"),
            ("AWS_ENDPOINT_URL", "http://g"),
        ]);
        assert_eq!(configured_endpoint(&ignored, Some(&profile), "STS"), Ok(None));
        let typo = env(&[("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "yes")]);
        assert!(configured_endpoint(&typo, Some(&profile), "STS").is_err());
    }

    /// Forms are sorted by key, each side escaped as a query's (Go's QueryEscape).
    #[test]
    fn forms_are_sorted_and_query_escaped() {
        assert_eq!(
            form(&[
                ("Version", "2011-06-15"),
                ("Action", "AssumeRole"),
                ("RoleSessionName", "a b/c+d~")
            ]),
            "Action=AssumeRole&RoleSessionName=a+b%2Fc%2Bd~&Version=2011-06-15"
        );
    }

    /// A field is found by its name first, else by its name in any case, as Go's
    /// encoding/json finds one.
    #[test]
    fn fields_are_found_by_name_then_in_any_case() {
        let v: Value = serde_json::from_str(r#"{"accesskeyid":"folded","Token":"t"}"#).unwrap();
        assert_eq!(text(&v, "AccessKeyId").as_deref(), Some("folded"));
        let v: Value = serde_json::from_str(r#"{"accesskeyid":"folded","AccessKeyId":"exact"}"#).unwrap();
        assert_eq!(text(&v, "AccessKeyId").as_deref(), Some("exact"));
        assert_eq!(text(&v, "Expiration"), None);
        assert_eq!(
            rfc3339_of(rfc3339("2099-01-01T00:00:00.5+01:00").unwrap()),
            "2098-12-31T23:00:00Z"
        );
    }
}
