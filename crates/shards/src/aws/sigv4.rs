//! AWS Signature Version 4 (D88, D168), as aws-sdk-go-v2's v4 signer signs a request: its
//! canonical request, the string to sign, and the key derived from the secret for its date,
//! region and service, HMAC-SHA256 throughout. Which headers it signs is one of the
//! signer's two rules: every header but those it ignores (IgnoredHeaders), as it signs a
//! request it sends, CloudWatch Logs' among them; or an allow list and every `x-amz-*`
//! (RequiredSignedHeaders), as S3's client signs for BuildKit, its payload's hash a header
//! of its own.

use aws_lc_rs::hmac;
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

/// The headers the signer signs beside `host` and every `x-amz-*` (v4 `RequiredSigned
/// Headers`), lowercase; `content-length` where the body is not empty.
const SIGNED: &[&str] = &[
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-md5",
    "content-type",
    "expires",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "range",
];

/// The headers the signer leaves unsigned under [`Rule::Ignored`] (v4 IgnoredHeaders),
/// lowercase.
const IGNORED: &[&str] = &[
    "authorization",
    "user-agent",
    "x-amzn-trace-id",
    "expect",
    "transfer-encoding",
];

/// Which of a request's headers its signature covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Every header but those of [`IGNORED`]: a request the SDK sends.
    Ignored,
    /// Those of [`SIGNED`] and every `x-amz-*`, its payload's hash `x-amz-content-sha256`:
    /// S3's, as BuildKit's client signs them.
    Required,
}

/// What a signature is for: its region, its service (`logs`, `s3`, `sts`) and its rule.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
    pub rule: Rule,
}

/// A body not hashed (S3's `UNSIGNED-PAYLOAD`), and the hash of an empty one.
pub const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

/// Who signs: an access key, its secret, and a session's token if any.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret: Zeroizing<String>,
    pub token: Option<Zeroizing<String>>,
}

/// Neither the secret nor the token is shown.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({})", self.access_key)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// HMAC-SHA256 (RFC 2104), AWS-LC's.
fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg)
        .as_ref()
        .to_vec()
}

/// RFC 3986's encoding of a query's key or value, as the signer writes them: unreserved
/// characters as they are, every other byte `%XX`.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query string's pairs, decoded.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    let decode = |s: &str| -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            let hexval = |c: u8| char::from(c).to_digit(16);
            match (
                c,
                b.get(i + 1).copied().and_then(hexval),
                b.get(i + 2).copied().and_then(hexval),
            ) {
                (b'%', Some(h), Some(l)) => {
                    out.push(u8::try_from(h * 16 + l).unwrap_or(0));
                    i += 3;
                }
                (b'+', _, _) => {
                    out.push(b' ');
                    i += 1;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// A request to sign: its method, its URL's authority (host, and port unless the
/// scheme's own), its escaped path and its query, its headers, the length of its body
/// and its payload's hash (or [`UNSIGNED`]).
pub struct Request<'a> {
    pub method: &'a str,
    pub authority: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub length: u64,
    pub payload_hash: &'a str,
}

/// The headers SigV4 adds to `r` for `creds` in `scope`, at `when` (UTC, as
/// `YYYYMMDDTHHMMSSZ`): `x-amz-date`, S3's `x-amz-content-sha256`, `x-amz-security-token`
/// where there is a token, and `authorization`.
pub fn sign(r: &Request<'_>, creds: &Credentials, scope: &Scope<'_>, when: &str) -> Vec<(String, String)> {
    let date = when.get(..8).unwrap_or(when);
    let mut added = vec![("x-amz-date".to_string(), when.to_string())];
    if scope.rule == Rule::Required {
        added.push(("x-amz-content-sha256".to_string(), r.payload_hash.to_string()));
    }
    if let Some(t) = &creds.token {
        added.push(("x-amz-security-token".to_string(), t.to_string()));
    }
    // The headers signed: host, content-length if any, and those the rule takes.
    let mut signed: Vec<(String, String)> = vec![("host".to_string(), r.authority.to_string())];
    if r.length > 0 {
        signed.push(("content-length".to_string(), r.length.to_string()));
    }
    for (k, v) in r
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_string()))
        .chain(added.clone())
    {
        let taken = match scope.rule {
            Rule::Ignored => !IGNORED.contains(&k.as_str()),
            Rule::Required => SIGNED.contains(&k.as_str()) || k.starts_with("x-amz-"),
        };
        if taken && !signed.iter().any(|(n, _)| *n == k) {
            signed.push((k, v));
        }
    }
    signed.sort();
    let names: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
    let canonical_headers: String = signed
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    let mut pairs: Vec<(String, String)> = query_pairs(r.query)
        .into_iter()
        .map(|(k, v)| (uri_encode(&k), uri_encode(&v)))
        .collect();
    pairs.sort();
    let canonical_query: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let path = if r.path.is_empty() { "/" } else { r.path };
    let canonical = format!(
        "{}\n{path}\n{}\n{canonical_headers}\n{}\n{}",
        r.method,
        canonical_query.join("&"),
        names.join(";"),
        r.payload_hash
    );
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{when}\n{credential_scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let key = [scope.region, scope.service, "aws4_request"].iter().fold(
        mac(
            Zeroizing::new(format!("AWS4{}", creds.secret.as_str())).as_bytes(),
            date.as_bytes(),
        ),
        |k, part| mac(&k, part.as_bytes()),
    );
    added.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={}, Signature={}",
            creds.access_key,
            names.join(";"),
            hex(&mac(&key, to_sign.as_bytes()))
        ),
    ));
    added
}

/// `when` as SigV4 dates a request: `YYYYMMDDTHHMMSSZ`, UTC.
pub fn amz_date(when: std::time::SystemTime) -> String {
    let t = time::OffsetDateTime::from(when);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// The SHA-256 of `body`, in hex: a payload's hash.
pub fn payload_hash(body: &[u8]) -> String {
    hex(&Sha256::digest(body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Every case of build/testdata/sigv4.json (`scripts/s3/generate`): the headers the signer
    /// added, those it signs and the signature, as aws-sdk-go-v2's v4 signer set them.
    #[test]
    fn signatures_match_the_recorded_vectors() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../build/testdata/sigv4.json")).unwrap();
        assert_eq!(cases.len(), 7);
        for c in cases {
            let url = c["url"].as_str().unwrap();
            let rest = url.split_once("://").unwrap().1;
            let (authority, target) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let given: Vec<(String, String)> = c["headers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|h| {
                            (
                                h["name"].as_str().unwrap().to_string(),
                                h["value"].as_str().unwrap().to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let headers: Vec<(&str, &str)> = given.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let token = c["token"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
            let creds = Credentials {
                access_key: "AKIDEXAMPLE".into(),
                secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
                token: token.map(Zeroizing::new),
            };
            let added = sign(
                &Request {
                    method: c["method"].as_str().unwrap(),
                    authority,
                    path,
                    query,
                    headers: &headers,
                    length: c["length"].as_u64().unwrap(),
                    payload_hash: c["payload_hash"].as_str().unwrap(),
                },
                &creds,
                &Scope {
                    region: c["region"].as_str().unwrap(),
                    service: "s3",
                    rule: Rule::Required,
                },
                "20261008T123456Z",
            );
            for want in c["signed"].as_array().unwrap() {
                let name = want["name"].as_str().unwrap();
                if given.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                    continue;
                }
                let got = added.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
                assert_eq!(got, want["value"].as_str(), "{} {url}: {name}", c["method"]);
            }
        }
    }
    /// Every request the recording made of CloudWatch Logs (logging/testdata/awslogs.json,
    /// `scripts/logdrivers/generate`), signed again at its time with the credentials it was
    /// given: the same signature, every header signed but those the signer ignores.
    #[test]
    fn recorded_cloudwatch_requests_are_signed_alike() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../logging/testdata/awslogs.json")).unwrap();
        let creds = Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
            token: Some(Zeroizing::new("example-session-token".into())),
        };
        let mut checked = 0;
        for case in oracle["cases"].as_array().unwrap() {
            for r in case["requests"].as_array().unwrap() {
                let fields = r["fields"].as_object().unwrap();
                let field = |k: &str| fields[k].as_str().unwrap();
                let authorization = field("authorization");
                let signed = authorization
                    .split("SignedHeaders=")
                    .nth(1)
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap();
                // Those it signed, but those the signer adds or makes itself.
                let headers: Vec<(&str, &str)> = signed
                    .split(';')
                    .filter(|k| !["host", "content-length", "x-amz-date", "x-amz-security-token"].contains(k))
                    .map(|k| (k, field(k)))
                    .collect();
                let body = r["body"].as_str().unwrap().as_bytes();
                let added = sign(
                    &Request {
                        method: r["method"].as_str().unwrap(),
                        authority: field("host"),
                        path: r["path"].as_str().unwrap(),
                        query: r["query"].as_str().unwrap(),
                        headers: &headers,
                        length: body.len() as u64,
                        payload_hash: &payload_hash(body),
                    },
                    &creds,
                    &Scope {
                        region: "us-east-1",
                        service: "logs",
                        rule: Rule::Ignored,
                    },
                    field("x-amz-date"),
                );
                let ours = added.iter().find(|(k, _)| k == "authorization").unwrap();
                assert_eq!(ours.1, authorization, "{}", field("x-amz-target"));
                checked += 1;
            }
        }
        assert!(checked >= 14, "{checked} requests");
    }
}
