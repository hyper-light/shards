//! OpenID Connect as cosign v3.1.3 signs in with it (sigstore v1.10.8 pkg/oauthflow over
//! golang.org/x/oauth2 v0.36.0 and coreos/go-oidc v3.18.0): the provider's discovery
//! document, PKCE, the authorization URL, form bodies, and the ID token checked as
//! go-oidc's verifier checks it (its JWS against the provider's keys, issuer, audience,
//! expiry, not-before), with the nonce and access token hash oauthflow checks. What goes
//! over the network is the caller's.

use crate::tlog::gocodec;
use crate::tlog::gojson::{self, JValue};

/// The algorithms go-oidc takes ID tokens signed with (supportedAlgorithms).
pub const SUPPORTED: &[&str] = &[
    "RS256", "RS384", "RS512", "ES256", "ES384", "ES512", "PS256", "PS384", "PS512", "EdDSA",
];

/// A provider, as its discovery document describes it (providerJSON).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provider {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    pub jwks_uri: String,
    /// The algorithms it signs ID tokens with that go-oidc supports.
    pub algorithms: Vec<String>,
    /// code_challenge_methods_supported, which oauthflow's PKCE reads.
    pub code_challenge_methods: Vec<String>,
}

fn strings(v: &JValue, strukt: &str, field: &str) -> Result<Vec<String>, String> {
    match v {
        JValue::Null => Ok(Vec::new()),
        JValue::Array(items) => items
            .iter()
            .map(|i| match i {
                JValue::Str(s) => Ok(s.clone()),
                JValue::Null => Ok(String::new()),
                other => Err(gojson::type_error(other.kind(), strukt, field, "string")),
            })
            .collect(),
        other => Err(gojson::type_error(other.kind(), strukt, field, "[]string")),
    }
}

/// The discovery document `body` of the provider at `issuer` (NewProvider): its issuer
/// must be `issuer`.
pub fn provider(body: &[u8], issuer: &str) -> Result<Provider, String> {
    let fail = |e: String| format!("oidc: failed to decode provider discovery object: {e}");
    let v = gojson::unmarshal(body).map_err(fail)?;
    let members = match &v {
        JValue::Object(m) => m.as_slice(),
        JValue::Null => &[],
        other => {
            return Err(fail(gojson::type_error(
                other.kind(),
                "",
                "",
                "oidc.providerJSON",
            )));
        }
    };
    let names = [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "device_authorization_endpoint",
        "jwks_uri",
        "userinfo_endpoint",
        "id_token_signing_alg_values_supported",
        "code_challenge_methods_supported",
    ];
    let mut p = Provider::default();
    let mut algorithms = Vec::new();
    for (i, value) in gojson::struct_members(members, &names) {
        let name = names.get(i).copied().unwrap_or_default();
        match i {
            6 => algorithms = strings(value, "providerJSON", name).map_err(fail)?,
            7 => p.code_challenge_methods = strings(value, "providerJSON", name).unwrap_or_default(),
            _ => {
                let s = match value {
                    JValue::Str(s) => s.clone(),
                    JValue::Null => continue,
                    other => {
                        return Err(fail(gojson::type_error(
                            other.kind(),
                            "providerJSON",
                            name,
                            "string",
                        )));
                    }
                };
                match i {
                    0 => p.issuer = s,
                    1 => p.authorization_endpoint = s,
                    2 => p.token_endpoint = s,
                    3 => p.device_authorization_endpoint = s,
                    4 => p.jwks_uri = s,
                    _ => {}
                }
            }
        }
    }
    if p.issuer != issuer {
        return Err(format!(
            "oidc: issuer URL provided to client ({}) did not match the issuer URL returned by provider ({})",
            go_quote(issuer),
            go_quote(&p.issuer)
        ));
    }
    p.algorithms = algorithms
        .into_iter()
        .filter(|a| SUPPORTED.contains(&a.as_str()))
        .collect();
    Ok(p)
}

/// Go's `%q` of a string.
pub fn go_quote(s: &str) -> String {
    shards_agentfile::go::quote(s.as_bytes())
}

/// url.QueryEscape: unreserved bytes kept, space as `+`, the rest `%XX`.
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

/// url.Values.Encode: `key=value` pairs, keys sorted (a key's values in their order),
/// each escaped.
pub fn form(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The pairs of a query or form body (url.ParseQuery's, the first value of a key kept by
/// [`form_value`]).
pub fn parse_form(s: &str) -> Vec<(String, String)> {
    let unescape = |t: &str| -> String {
        let b = t.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            match c {
                b'+' => out.push(b' '),
                b'%' => {
                    let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
                    match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                        Some(v) => {
                            out.push(v);
                            i += 2;
                        }
                        None => out.push(b'%'),
                    }
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (unescape(k), unescape(v)),
            None => (unescape(p), String::new()),
        })
        .collect()
}

/// The first value of `key` in `pairs` (Values.Get, FormValue).
pub fn form_value<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map_or("", |(_, v)| v.as_str())
}

/// cryptoutils.GenerateRandomURLSafeString: so many bits of randomness, unpadded
/// base64url.
pub fn random_url_safe(bits: usize) -> Result<String, String> {
    let mut b = vec![0u8; bits.div_ceil(8)];
    aws_lc_rs::rand::fill(&mut b).map_err(|_| "reading random bytes")?;
    Ok(base64url(&b))
}

/// base64.RawURLEncoding.
pub fn base64url(b: &[u8]) -> String {
    gocodec::std_encode(b)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// PKCE as oauthflow makes it (NewPKCE): S256 where the provider lists it (or is
/// Azure's, which lists none), a verifier of 384 bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub method: String,
    pub challenge: String,
    pub verifier: String,
}

pub fn pkce(p: &Provider) -> Result<Pkce, String> {
    let s256 = p.code_challenge_methods.iter().any(|m| m == "S256");
    let azure = [
        "https://login.microsoftonline.com/",
        "https://login.microsoftonline.us/",
    ]
    .iter()
    .any(|prefix| p.authorization_endpoint.starts_with(prefix));
    if !s256 && !azure {
        return Err(format!(
            "PKCE is not supported by OIDC provider '{}'",
            p.authorization_endpoint
        ));
    }
    let verifier = random_url_safe(384)?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
    Ok(Pkce {
        method: "S256".into(),
        challenge: base64url(digest.as_ref()),
        verifier,
    })
}

/// Config.AuthCodeURL: the provider's authorization endpoint with the request's
/// parameters, `response_type` and `client_id` first among them only as sorting puts them.
pub fn auth_code_url(
    auth_url: &str,
    client_id: &str,
    redirect: &str,
    state: &str,
    extra: &[(&str, &str)],
) -> String {
    let mut pairs: Vec<(&str, &str)> = vec![("response_type", "code"), ("client_id", client_id)];
    if !redirect.is_empty() {
        pairs.push(("redirect_uri", redirect));
    }
    pairs.push(("scope", "openid email"));
    if !state.is_empty() {
        pairs.push(("state", state));
    }
    // Each option sets its key (Values.Set): a later one replaces an earlier.
    for (k, v) in extra {
        pairs.retain(|(pk, _)| pk != k);
        pairs.push((k, v));
    }
    let sep = if auth_url.contains('?') { '&' } else { '?' };
    format!("{auth_url}{sep}{}", form(&pairs))
}

/// A JWS in compact form, as go-jose parses one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jws {
    pub alg: String,
    pub kid: String,
    /// `header.payload`, which the signature is over.
    pub signing_input: Vec<u8>,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
}

/// ParseSigned of a compact JWS, its algorithm among `allowed`.
pub fn parse_jws(token: &str, allowed: &[&str]) -> Result<Jws, String> {
    let token: String = token.chars().filter(|c| !c.is_whitespace()).collect();
    let three = || "go-jose/go-jose: compact JWS format must have three parts".to_string();
    let (protected, rest) = token.split_once('.').ok_or_else(three)?;
    let (claims, sig) = rest.split_once('.').ok_or_else(three)?;
    if sig.contains('.') {
        return Err(three());
    }
    let decode =
        |s: &str| crate::gobase64::decode(s.as_bytes(), true, false).map_err(crate::gobase64::error_text);
    let header = decode(protected)?;
    let payload = decode(claims)?;
    let signature = decode(sig)?;
    let h = gojson::unmarshal(&header)?;
    let alg = h
        .get("alg")
        .and_then(JValue::as_str)
        .unwrap_or_default()
        .to_string();
    let kid = h
        .get("kid")
        .and_then(JValue::as_str)
        .unwrap_or_default()
        .to_string();
    if h.get("crit").is_some() {
        return Err("go-jose/go-jose: unsupported crit header".into());
    }
    if !allowed.contains(&alg.as_str()) {
        let list: Vec<String> = allowed.iter().map(|a| go_quote(a)).collect();
        return Err(format!(
            "unexpected signature algorithm {}; expected [{}]",
            go_quote(&alg),
            list.join(" ")
        ));
    }
    Ok(Jws {
        alg,
        kid,
        signing_input: format!("{protected}.{claims}").into_bytes(),
        payload,
        signature,
    })
}

/// A public key of a JWK set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// The curve's name and the uncompressed point.
    Ec {
        crv: String,
        point: Vec<u8>,
    },
    Ed25519(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jwk {
    pub kid: String,
    pub key: Key,
}

/// The keys of a JWK set (JSONWebKeySet) that are keys go-jose can verify with; others
/// are left out.
pub fn parse_jwks(body: &[u8]) -> Result<Vec<Jwk>, String> {
    let v = gojson::unmarshal(body).map_err(|e| format!("oidc: failed to decode keys: {e}"))?;
    let keys = match v.get("keys") {
        Some(JValue::Array(k)) => k.as_slice(),
        _ => &[],
    };
    let b64 = |o: &JValue, k: &str| -> Option<Vec<u8>> {
        crate::gobase64::decode(o.get(k)?.as_str()?.as_bytes(), true, false).ok()
    };
    let mut out = Vec::new();
    for k in keys {
        let kid = k
            .get("kid")
            .and_then(JValue::as_str)
            .unwrap_or_default()
            .to_string();
        let key = match k.get("kty").and_then(JValue::as_str) {
            Some("RSA") => match (b64(k, "n"), b64(k, "e")) {
                (Some(n), Some(e)) => Key::Rsa { n, e },
                _ => continue,
            },
            Some("EC") => {
                let crv = k
                    .get("crv")
                    .and_then(JValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                let size = match crv.as_str() {
                    "P-256" => 32,
                    "P-384" => 48,
                    "P-521" => 66,
                    _ => continue,
                };
                let (Some(x), Some(y)) = (b64(k, "x"), b64(k, "y")) else {
                    continue;
                };
                if x.len() != size || y.len() != size {
                    continue;
                }
                let mut point = vec![4u8];
                point.extend(x);
                point.extend(y);
                Key::Ec { crv, point }
            }
            Some("OKP") if k.get("crv").and_then(JValue::as_str) == Some("Ed25519") => match b64(k, "x") {
                Some(x) if x.len() == 32 => Key::Ed25519(x),
                _ => continue,
            },
            _ => continue,
        };
        out.push(Jwk { kid, key });
    }
    Ok(out)
}

/// Whether `key` verifies `jws`'s signature by its algorithm.
pub fn verify_jws(jws: &Jws, key: &Key) -> bool {
    use aws_lc_rs::signature as s;
    let msg = &jws.signing_input;
    let sig = &jws.signature;
    match (jws.alg.as_str(), key) {
        (alg @ ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512"), Key::Rsa { n, e }) => {
            let params: &s::RsaParameters = match alg {
                "RS256" => &s::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &s::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &s::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &s::RSA_PSS_2048_8192_SHA256,
                "PS384" => &s::RSA_PSS_2048_8192_SHA384,
                _ => &s::RSA_PSS_2048_8192_SHA512,
            };
            s::RsaPublicKeyComponents {
                n: n.as_slice(),
                e: e.as_slice(),
            }
            .verify(params, msg, sig)
            .is_ok()
        }
        (alg @ ("ES256" | "ES384" | "ES512"), Key::Ec { crv, point }) => {
            let algorithm: &s::EcdsaVerificationAlgorithm = match (alg, crv.as_str()) {
                ("ES256", "P-256") => &s::ECDSA_P256_SHA256_FIXED,
                ("ES384", "P-384") => &s::ECDSA_P384_SHA384_FIXED,
                ("ES512", "P-521") => &s::ECDSA_P521_SHA512_FIXED,
                _ => return false,
            };
            s::UnparsedPublicKey::new(algorithm, point)
                .verify(msg, sig)
                .is_ok()
        }
        ("EdDSA", Key::Ed25519(x)) => s::UnparsedPublicKey::new(&s::ED25519, x).verify(msg, sig).is_ok(),
        _ => false,
    }
}

/// An ID token's claims as go-oidc reads them (idToken), and the algorithm it was signed
/// with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdToken {
    pub issuer: String,
    pub subject: String,
    pub audience: Vec<String>,
    pub expiry: i64,
    pub not_before: Option<i64>,
    pub nonce: String,
    pub at_hash: String,
    pub alg: String,
    /// The claims, as given.
    pub claims: Vec<u8>,
}

/// jsonTime: a number of seconds, an integer or not.
fn json_time(v: &JValue) -> Result<i64, String> {
    match v {
        JValue::Number(n) => n
            .parse::<i64>()
            .or_else(|_| n.parse::<f64>().map(|f| f as i64))
            .map_err(|e| e.to_string()),
        other => Err(gojson::type_error(other.kind(), "", "", "json.Number")),
    }
}

/// The claims of a verified token's payload.
pub fn id_token(payload: &[u8], alg: &str) -> Result<IdToken, String> {
    let fail = |e: String| format!("oidc: failed to unmarshal claims: {e}");
    let v = gojson::unmarshal(payload).map_err(fail)?;
    let mut t = IdToken {
        alg: alg.to_string(),
        claims: payload.to_vec(),
        ..IdToken::default()
    };
    let JValue::Object(members) = &v else {
        return Err(fail(gojson::type_error(v.kind(), "", "", "oidc.idToken")));
    };
    let names = ["iss", "sub", "aud", "exp", "nbf", "nonce", "at_hash"];
    for (i, value) in gojson::struct_members(members, &names) {
        let name = names.get(i).copied().unwrap_or_default();
        let text = || match value {
            JValue::Str(s) => Ok(s.clone()),
            JValue::Null => Ok(String::new()),
            other => Err(fail(gojson::type_error(other.kind(), "idToken", name, "string"))),
        };
        match i {
            0 => t.issuer = text()?,
            1 => t.subject = text()?,
            2 => {
                t.audience = match value {
                    JValue::Str(s) => vec![s.clone()],
                    other => strings(other, "", "").map_err(fail)?,
                }
            }
            3 => t.expiry = json_time(value).map_err(fail)?,
            4 if !matches!(value, JValue::Null) => t.not_before = Some(json_time(value).map_err(fail)?),
            5 => t.nonce = text()?,
            6 => t.at_hash = text()?,
            _ => {}
        }
    }
    Ok(t)
}

/// go-oidc's checks of a token's claims: issuer, audience, expiry, and not-before with
/// five minutes' leeway, at `now` (seconds).
pub fn check(t: &IdToken, issuer: &str, client_id: &str, now: i64) -> Result<(), String> {
    if t.issuer != issuer {
        return Err(format!(
            "oidc: id token issued by a different provider, expected {} got {}",
            go_quote(issuer),
            go_quote(&t.issuer)
        ));
    }
    if client_id.is_empty() {
        return Err(
            "oidc: invalid configuration, clientID must be provided or SkipClientIDCheck must be set".into(),
        );
    }
    if !t.audience.iter().any(|a| a == client_id) {
        let list: Vec<String> = t.audience.iter().map(|a| go_quote(a)).collect();
        return Err(format!(
            "oidc: expected audience {} got [{}]",
            go_quote(client_id),
            list.join(" ")
        ));
    }
    if t.expiry < now {
        return Err(format!(
            "oidc: token is expired (Token Expiry: {})",
            crate::time::Time::utc(t.expiry, 0).rfc3339()
        ));
    }
    if let Some(nbf) = t.not_before
        && now + 300 < nbf
    {
        return Err(format!(
            "oidc: current time {} before the nbf (not before) time: {}",
            crate::time::Time::utc(now, 0).rfc3339(),
            crate::time::Time::utc(nbf, 0).rfc3339()
        ));
    }
    Ok(())
}

/// IDToken.VerifyAccessToken: the left half of the access token's hash by the token's
/// algorithm, base64url, its `at_hash`.
pub fn verify_access_token(t: &IdToken, access_token: &str) -> Result<(), String> {
    use aws_lc_rs::digest;
    if t.at_hash.is_empty() {
        return Err("id token did not have an access token hash".into());
    }
    let alg = match t.alg.as_str() {
        "RS256" | "ES256" | "PS256" => &digest::SHA256,
        "RS384" | "ES384" | "PS384" => &digest::SHA384,
        "RS512" | "ES512" | "PS512" | "EdDSA" => &digest::SHA512,
        other => return Err(format!("oidc: unsupported signing algorithm {}", go_quote(other))),
    };
    let d = digest::digest(alg, access_token.as_bytes());
    let half = d.as_ref().get(..d.as_ref().len() / 2).unwrap_or_default();
    if base64url(half) != t.at_hash {
        return Err("access token hash does not match value in ID token".into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use aws_lc_rs::signature::{self as s, KeyPair as _};

    #[test]
    fn forms_and_urls_are_go_s() {
        assert_eq!(query_escape("openid email/+&é"), "openid+email%2F%2B%26%C3%A9");
        assert_eq!(
            form(&[("scope", "openid email"), ("client_id", "sigstore")]),
            "client_id=sigstore&scope=openid+email"
        );
        let url = auth_code_url(
            "https://oauth2.sigstore.dev/auth/auth",
            "sigstore",
            "http://localhost:1/auth/callback",
            "st",
            &[
                ("code_challenge_method", "S256"),
                ("code_challenge", "ch"),
                ("access_type", "online"),
                ("nonce", "no"),
            ],
        );
        assert_eq!(
            url,
            "https://oauth2.sigstore.dev/auth/auth?access_type=online&client_id=sigstore&code_challenge=ch&code_challenge_method=S256&nonce=no&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fauth%2Fcallback&response_type=code&scope=openid+email&state=st"
        );
        let pairs = parse_form("code=a%2Bb&state=x+y&empty");
        assert_eq!(form_value(&pairs, "code"), "a+b");
        assert_eq!(form_value(&pairs, "state"), "x y");
        assert_eq!(form_value(&pairs, "missing"), "");
        assert_eq!(random_url_safe(128).unwrap().len(), 22);
        assert_eq!(random_url_safe(384).unwrap().len(), 64);
    }

    #[test]
    fn a_provider_is_its_issuer_s() {
        let doc = br#"{"issuer":"https://oauth2.sigstore.dev/auth","authorization_endpoint":"https://oauth2.sigstore.dev/auth/auth","token_endpoint":"https://oauth2.sigstore.dev/auth/token","jwks_uri":"https://oauth2.sigstore.dev/auth/keys","device_authorization_endpoint":"https://oauth2.sigstore.dev/auth/device/code","id_token_signing_alg_values_supported":["RS256","HS256"],"code_challenge_methods_supported":["S256","plain"]}"#;
        let p = provider(doc, "https://oauth2.sigstore.dev/auth").unwrap();
        assert_eq!(p.algorithms, ["RS256"]);
        assert_eq!(
            p.device_authorization_endpoint,
            "https://oauth2.sigstore.dev/auth/device/code"
        );
        assert_eq!(pkce(&p).unwrap().method, "S256");
        assert_eq!(
            provider(doc, "https://oauth2.sigstore.dev").unwrap_err(),
            "oidc: issuer URL provided to client (\"https://oauth2.sigstore.dev\") did not match the issuer URL returned by provider (\"https://oauth2.sigstore.dev/auth\")"
        );
        let no_pkce = Provider {
            authorization_endpoint: "https://idp/auth".into(),
            ..Provider::default()
        };
        assert_eq!(
            pkce(&no_pkce).unwrap_err(),
            "PKCE is not supported by OIDC provider 'https://idp/auth'"
        );
    }

    /// A token an ES256 key signed verifies with that key's JWK, and with no other; its
    /// claims are checked as go-oidc checks them.
    #[test]
    fn tokens_verify_against_their_provider_s_keys() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = s::EcdsaKeyPair::generate_pkcs8(&s::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let k = s::EcdsaKeyPair::from_pkcs8(&s::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
        let point = k.public_key().as_ref().to_vec();
        let header = base64url(br#"{"alg":"ES256","kid":"one"}"#);
        let claims = base64url(
            br#"{"iss":"https://idp","sub":"s","aud":["sigstore"],"exp":2000000000,"nonce":"n","email":"a@b.c","email_verified":true}"#,
        );
        let input = format!("{header}.{claims}");
        let sig = k.sign(&rng, input.as_bytes()).unwrap();
        let token = format!("{input}.{}", base64url(sig.as_ref()));
        let jwks = format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","kid":"one","x":"{}","y":"{}"}},{{"kty":"oct","k":"x"}}]}}"#,
            base64url(&point[1..33]),
            base64url(&point[33..])
        );
        let keys = parse_jwks(jwks.as_bytes()).unwrap();
        assert_eq!(keys.len(), 1);
        let jws = parse_jws(&token, &["ES256"]).unwrap();
        assert!(verify_jws(&jws, &keys[0].key));
        let mut tampered = jws.clone();
        tampered.signing_input.push(b'x');
        assert!(!verify_jws(&tampered, &keys[0].key));
        assert_eq!(
            parse_jws(&token, &["RS256"]).unwrap_err(),
            "unexpected signature algorithm \"ES256\"; expected [\"RS256\"]"
        );
        let t = id_token(&jws.payload, &jws.alg).unwrap();
        assert_eq!(t.audience, ["sigstore"]);
        check(&t, "https://idp", "sigstore", 1_900_000_000).unwrap();
        assert!(
            check(&t, "https://idp", "other", 1_900_000_000)
                .unwrap_err()
                .contains("expected audience")
        );
        assert!(
            check(&t, "https://idp", "sigstore", 2_100_000_000)
                .unwrap_err()
                .starts_with("oidc: token is expired")
        );
        assert!(
            check(&t, "https://other", "sigstore", 1_900_000_000)
                .unwrap_err()
                .contains("different provider")
        );
    }

    #[test]
    fn the_access_token_hash_is_the_left_half() {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b"access");
        let t = IdToken {
            alg: "RS256".into(),
            at_hash: base64url(&d.as_ref()[..16]),
            ..IdToken::default()
        };
        verify_access_token(&t, "access").unwrap();
        assert_eq!(
            verify_access_token(&t, "other").unwrap_err(),
            "access token hash does not match value in ID token"
        );
    }
}
