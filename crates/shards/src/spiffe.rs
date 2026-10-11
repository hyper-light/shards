//! The SPIFFE provider of keyless signing, as cosign v3.1.3's gets a token
//! (pkg/providers/spiffe over go-spiffe v2.7.0): a JWT-SVID for the audience, from the
//! Workload API on the agent's socket (gRPC, `FetchJWTSVID`, the `workload.spiffe.io`
//! header), its first SVID checked as jwtsvid.ParseInsecure checks it.

#[cfg(any(unix, test))]
use shards_gateway::wire::{Reader, Value, Writer};
#[cfg(any(unix, test))]
use shards_sigstore::oidc;

/// The socket cosign asks where SPIFFE_ENDPOINT_SOCKET names none.
const DEFAULT_SOCKET: &str = "/tmp/spire-agent/public/api.sock";

/// The Workload API's socket: SPIFFE_ENDPOINT_SOCKET's, as a path or (as the SPIFFE
/// Workload Endpoint spec writes it, which cosign leaves unread) a `unix://` URI.
pub fn socket() -> String {
    match std::env::var("SPIFFE_ENDPOINT_SOCKET") {
        Ok(s) if !s.is_empty() => s.strip_prefix("unix://").unwrap_or(&s).to_string(),
        _ => DEFAULT_SOCKET.into(),
    }
}

/// JWTSVIDRequest: the audiences, no SPIFFE ID asked for.
#[cfg(any(unix, test))]
fn request(audience: &str) -> Vec<u8> {
    let mut w = Writer::default();
    w.string(1, audience);
    w.0
}

/// The SVIDs of a JWTSVIDResponse: each one's token and hint.
#[cfg(any(unix, test))]
fn svids(response: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for field in Reader::new(response) {
        let (number, value) = field.map_err(|e| e.0)?;
        if number != 1 {
            continue;
        }
        let Value::Bytes(svid) = value else {
            return Err("a JWT-SVID that is not a message".into());
        };
        let (mut token, mut hint) = (String::new(), String::new());
        for f in Reader::new(svid) {
            match f.map_err(|e| e.0)? {
                (2, v) => token = v.string().map_err(|e| e.0)?,
                (3, v) => hint = v.string().map_err(|e| e.0)?,
                _ => {}
            }
        }
        out.push((token, hint));
    }
    Ok(out)
}

/// spiffeid.FromString's checks of a SPIFFE ID: `spiffe://`, a trust domain of lower
/// case letters, digits, `.`, `-` and `_`, and a path of segments of letters, digits,
/// `.`, `-` and `_`, none empty, `.` or `..`.
#[cfg(any(unix, test))]
fn spiffe_id(s: &str) -> Result<(), String> {
    let rest = s
        .strip_prefix("spiffe://")
        .ok_or("scheme is missing or invalid")?;
    let (td, path) = match rest.find('/') {
        Some(i) => (
            rest.get(..i).unwrap_or_default(),
            rest.get(i..).unwrap_or_default(),
        ),
        None => (rest, ""),
    };
    if td.is_empty() {
        return Err("trust domain is missing".into());
    }
    if !td
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err("trust domain characters are limited to lowercase letters, numbers, dots, dashes, and underscores".into());
    }
    if path.is_empty() {
        return Ok(());
    }
    for seg in path.split('/').skip(1) {
        match seg {
            "" => return Err("path cannot contain empty segments".into()),
            "." | ".." => return Err("path cannot contain dot segments".into()),
            _ => {}
        }
        if !seg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        {
            return Err(
                "path segment characters are limited to letters, numbers, dots, dashes, and underscores"
                    .into(),
            );
        }
    }
    Ok(())
}

/// jwtsvid.ParseInsecure: a JWS of an algorithm SPIFFE allows, a `typ` of JWT or JOSE
/// where it has one, a SPIFFE ID its subject, an expiry, `audience` among its audiences,
/// not expired, not before its time (go-jose's minute of leeway).
#[cfg(any(unix, test))]
fn check(token: &str, audience: &str, now: i64) -> Result<(), String> {
    use shards_sigstore::tlog::gojson::{self, JValue};
    let fail = |e: &str| format!("jwtsvid: {e}");
    let allowed = [
        "RS256", "RS384", "RS512", "ES256", "ES384", "ES512", "PS256", "PS384", "PS512",
    ];
    let jws = oidc::parse_jws(token, &allowed).map_err(|_| fail("unable to parse JWT token"))?;
    let header = token.split('.').next().unwrap_or_default();
    let header = shards_sigstore::gobase64::decode(header.as_bytes(), true, false).unwrap_or_default();
    let header = gojson::unmarshal(&header).map_err(|_| fail("unable to parse JWT token"))?;
    if let Some(typ) = header.get("typ").and_then(JValue::as_str)
        && typ != "JWT"
        && typ != "JOSE"
    {
        return Err(fail("token header type not equal to either JWT or JOSE"));
    }
    let claims = gojson::unmarshal(&jws.payload)
        .map_err(|e| fail(&format!("unable to get claims from token: {e}")))?;
    let subject = claims.get("sub").and_then(JValue::as_str).unwrap_or_default();
    if subject.is_empty() {
        return Err(fail("token missing subject claim"));
    }
    let number = |k: &str| match claims.get(k) {
        Some(JValue::Number(n)) => n.parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    };
    let expiry = number("exp").ok_or_else(|| fail("token missing exp claim"))?;
    spiffe_id(subject).map_err(|e| fail(&format!("token has an invalid subject claim: {e}")))?;
    let audiences: Vec<String> = match claims.get("aud") {
        Some(JValue::Str(a)) => vec![a.clone()],
        Some(JValue::Array(list)) => list
            .iter()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if !audiences.iter().any(|a| a == audience) {
        let have: Vec<String> = audiences.iter().map(|a| oidc::go_quote(a)).collect();
        return Err(fail(&format!(
            "expected audience in [{}] (audience=[{}])",
            oidc::go_quote(audience),
            have.join(" ")
        )));
    }
    if now > expiry + 60 {
        return Err(fail("token has expired"));
    }
    if number("nbf").is_some_and(|nbf| now + 60 < nbf) {
        return Err("go-jose/go-jose/jwt: validation failed, token not valid yet (nbf)".into());
    }
    if number("iat").is_some_and(|iat| now + 60 < iat) {
        return Err("go-jose/go-jose/jwt: validation field, token issued in the future (iat)".into());
    }
    Ok(())
}

/// The first JWT-SVID the Workload API gives for `audience`.
#[cfg(unix)]
pub fn jwt_svid(audience: &str) -> Result<String, String> {
    let path = socket();
    let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    let reader = stream.try_clone().map_err(|e| e.to_string())?;
    let mut conn = shards_gateway::h2::Conn::new(reader, stream).map_err(|e| e.0)?;
    let response = shards_gateway::grpc::call_with(
        &mut conn,
        "/SpiffeWorkloadAPI/FetchJWTSVID",
        &request(audience),
        &[("workload.spiffe.io", "true")],
    )
    .map_err(|e| e.to_string())?;
    let svids = svids(&response)?;
    let (token, _) = svids.first().ok_or("there were no SVIDs in the response")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    check(token, audience, now)?;
    Ok(token.clone())
}

/// The Workload API is a Unix socket, or on Windows a named pipe, which shards does not
/// reach.
#[cfg(not(unix))]
pub fn jwt_svid(_audience: &str) -> Result<String, String> {
    Err("the SPIFFE Workload API's named pipe: give the token itself with --identity-token".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn spiffe_ids_are_checked_as_go_spiffe_checks_them() {
        spiffe_id("spiffe://example.org/ns/default/sa/signer").unwrap();
        spiffe_id("spiffe://example.org").unwrap();
        assert_eq!(
            spiffe_id("https://example.org/x").unwrap_err(),
            "scheme is missing or invalid"
        );
        assert!(spiffe_id("spiffe://Example.org/x").is_err());
        assert_eq!(
            spiffe_id("spiffe://example.org//x").unwrap_err(),
            "path cannot contain empty segments"
        );
        assert_eq!(
            spiffe_id("spiffe://example.org/x/..").unwrap_err(),
            "path cannot contain dot segments"
        );
    }

    #[test]
    fn the_request_and_response_are_the_workload_api_s() {
        assert_eq!(request("sigstore"), b"\x0a\x08sigstore");
        let mut svid = Writer::default();
        svid.string(1, "spiffe://example.org/x");
        svid.string(2, "a.b.c");
        svid.string(3, "internal");
        let mut resp = Writer::default();
        resp.message(1, &svid.0);
        assert_eq!(
            svids(&resp.0).unwrap(),
            [("a.b.c".to_string(), "internal".to_string())]
        );
    }

    #[test]
    fn a_jwt_svid_names_a_spiffe_id_and_the_audience() {
        let b = |s: &str| oidc::base64url(s.as_bytes());
        let token = |claims: &str| {
            format!(
                "{}.{}.{}",
                b(r#"{"alg":"ES256","typ":"JWT"}"#),
                b(claims),
                b("sig")
            )
        };
        let ok = token(r#"{"sub":"spiffe://example.org/signer","aud":["sigstore"],"exp":2000000000}"#);
        check(&ok, "sigstore", 1_900_000_000).unwrap();
        assert_eq!(
            check(&ok, "other", 1_900_000_000).unwrap_err(),
            "jwtsvid: expected audience in [\"other\"] (audience=[\"sigstore\"])"
        );
        assert_eq!(
            check(&ok, "sigstore", 2_000_000_100).unwrap_err(),
            "jwtsvid: token has expired"
        );
        let no_exp = token(r#"{"sub":"spiffe://example.org/signer","aud":"sigstore"}"#);
        assert_eq!(
            check(&no_exp, "sigstore", 0).unwrap_err(),
            "jwtsvid: token missing exp claim"
        );
        let bad_sub = token(r#"{"sub":"user@example.org","aud":"sigstore","exp":2000000000}"#);
        assert!(
            check(&bad_sub, "sigstore", 0)
                .unwrap_err()
                .starts_with("jwtsvid: token has an invalid subject claim")
        );
    }
}
