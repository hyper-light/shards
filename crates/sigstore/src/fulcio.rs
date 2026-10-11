//! A signing certificate from Fulcio, as sigstore-go v1.2.2 asks for one (pkg/sign
//! certificate.go Fulcio.GetCertificate): the identity token's subject, signed by the
//! ephemeral key as proof that the caller holds it, sent with the key to
//! `/api/v2/signingCert`; the first certificate of the chain answered.

use crate::tlog::gojson::{self, JValue};

/// The path sigstore-go asks Fulcio's API at.
pub const SIGNING_CERT_PATH: &str = "/api/v2/signingCert";

/// The API versions sigstore-go signs with (FulcioAPIVersions).
pub const API_VERSIONS: &[u32] = &[1];

/// SubjectFromUnverifiedToken of a token's claims (oauthflow, sigstore v1.10.8): its
/// email where it has one, if verified, else its subject.
pub fn subject_from_claims(claims: &[u8]) -> Result<String, String> {
    let v = gojson::unmarshal(claims)?;
    let mut email = String::new();
    let mut verified = false;
    let mut subject = String::new();
    let mut type_error: Option<String> = None;
    match &v {
        JValue::Null => {}
        JValue::Object(members) => {
            for (field, value) in gojson::struct_members(members, &["email", "email_verified", "sub"]) {
                match field {
                    // stringAsBool.UnmarshalJSON, given the value's text, whatever it is.
                    1 => {
                        verified = match value {
                            JValue::Bool(b) => *b,
                            JValue::Str(s) if s == "true" || s == "True" => true,
                            JValue::Str(s) if s == "false" || s == "False" => false,
                            _ => return Err("invalid value for boolean".into()),
                        }
                    }
                    _ => {
                        let name = if field == 0 { "email" } else { "sub" };
                        match value {
                            JValue::Str(s) if field == 0 => email = s.clone(),
                            JValue::Str(s) => subject = s.clone(),
                            JValue::Null => {}
                            other => {
                                // encoding/json goes on past a type's mismatch and
                                // returns the first.
                                type_error.get_or_insert_with(|| {
                                    gojson::type_error(other.kind(), "claims", name, "string")
                                });
                            }
                        }
                    }
                }
            }
        }
        other => {
            return Err(gojson::type_error(other.kind(), "", "", "oauthflow.claims"));
        }
    }
    if let Some(e) = type_error {
        return Err(e);
    }
    if !email.is_empty() {
        if !verified {
            return Err("not verified by identity provider".into());
        }
        return Ok(email);
    }
    if subject.is_empty() {
        return Err("no subject found in claims".into());
    }
    Ok(subject)
}

/// The subject of a token as GetCertificate reads it, unverified: its second part's
/// claims (base64url, unpadded).
pub fn subject_of_token(token: &str) -> Result<String, String> {
    let mut parts = token.split('.');
    let (Some(_), Some(claims)) = (parts.next(), parts.next()) else {
        return Err("unable to get subject from identity token".into());
    };
    let claims =
        crate::gobase64::decode(claims.as_bytes(), true, false).map_err(crate::gobase64::error_text)?;
    subject_from_claims(&claims)
}

/// encoding/json's string of `s`: `"`, `\` and control characters escaped (`\n`, `\r`,
/// `\t` short, the rest `\u00XX`), and `<`, `>`, `&`, U+2028 and U+2029 as `\u` escapes.
pub fn go_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The request's body: json.Marshal of fulcioCertRequest, the key as a PEM block of its
/// PKIX encoding and the proof of possession base64.
pub fn request(algorithm: &str, public_key_pem: &str, proof: &[u8]) -> Vec<u8> {
    format!(
        r#"{{"publicKeyRequest":{{"publicKey":{{"algorithm":{},"content":{}}},"proofOfPossession":{}}}}}"#,
        go_json_string(algorithm),
        go_json_string(public_key_pem),
        go_json_string(&crate::tlog::gocodec::std_encode(proof))
    )
    .into_bytes()
}

/// The certificates of `chain` (fulcioResponse's chain.certificates), in Go's words
/// where they are not strings.
fn certificates(chain: Option<&JValue>, path: &str) -> Result<Vec<String>, String> {
    let Some(JValue::Object(members)) = chain else {
        return match chain {
            None | Some(JValue::Null) => Ok(Vec::new()),
            // The innermost struct decoding names the error (addErrorContext).
            Some(other) => Err(gojson::type_error(
                other.kind(),
                path.split('.').next().unwrap_or_default(),
                path,
                "sign.chain",
            )),
        };
    };
    let mut out = Vec::new();
    for (_, list) in gojson::struct_members(members, &["certificates"]) {
        out.clear();
        match list {
            JValue::Null => {}
            JValue::Array(items) => {
                for item in items {
                    match item {
                        JValue::Str(s) => out.push(s.clone()),
                        JValue::Null => out.push(String::new()),
                        other => {
                            return Err(gojson::type_error(
                                other.kind(),
                                "chain",
                                &format!("{path}.certificates"),
                                "string",
                            ));
                        }
                    }
                }
            }
            other => {
                return Err(gojson::type_error(
                    other.kind(),
                    "chain",
                    &format!("{path}.certificates"),
                    "[]string",
                ));
            }
        }
    }
    Ok(out)
}

/// The leaf certificate (DER) of Fulcio's answer `body` to a request that it took (200):
/// the first of the chain with the SCT embedded, else of the one with it detached.
pub fn certificate(body: &[u8]) -> Result<Vec<u8>, String> {
    let v = gojson::unmarshal(body)?;
    let members = match &v {
        JValue::Object(m) => m.as_slice(),
        JValue::Null => &[],
        other => return Err(gojson::type_error(other.kind(), "", "", "sign.fulcioResponse")),
    };
    let mut embedded = Vec::new();
    let mut detached = Vec::new();
    for (field, value) in gojson::struct_members(
        members,
        &["signedCertificateEmbeddedSct", "signedCertificateDetachedSct"],
    ) {
        let name = if field == 0 {
            "signedCertificateEmbeddedSct"
        } else {
            "signedCertificateDetachedSct"
        };
        let chain = match value {
            JValue::Object(m) => gojson::struct_members(m, &["chain"]).last().map(|(_, c)| *c),
            JValue::Null => None,
            other => {
                return Err(gojson::type_error(
                    other.kind(),
                    "fulcioResponse",
                    name,
                    &format!("sign.{name}"),
                ));
            }
        };
        let certs = certificates(chain, &format!("{name}.chain"))?;
        if field == 0 {
            embedded = certs;
        } else {
            detached = certs;
        }
    }
    let first = embedded
        .first()
        .or_else(|| detached.first())
        .ok_or("Fulcio returned no certificates")?;
    let (block, _) =
        crate::tlog::pem::decode(first.as_bytes()).ok_or("unable to parse Fulcio certificate")?;
    Ok(block.bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn token(claims: &str) -> String {
        let b64 = |s: &[u8]| {
            crate::tlog::gocodec::std_encode(s)
                .trim_end_matches('=')
                .replace('+', "-")
                .replace('/', "_")
        };
        format!(
            "{}.{}.{}",
            b64(b"{\"alg\":\"RS256\"}"),
            b64(claims.as_bytes()),
            b64(b"sig")
        )
    }

    /// oauthflow's subjectFromClaims: a verified email, else the subject; its
    /// stringAsBool takes the words it takes; Go's matching of field names by case.
    #[test]
    fn a_tokens_subject_is_as_oauthflow_reads_it() {
        let ok = |c: &str| subject_of_token(&token(c));
        assert_eq!(
            ok(r#"{"email":"a@b.c","email_verified":true,"sub":"x"}"#).unwrap(),
            "a@b.c"
        );
        assert_eq!(
            ok(r#"{"email":"a@b.c","email_verified":"True"}"#).unwrap(),
            "a@b.c"
        );
        assert_eq!(
            ok(r#"{"EMAIL":"a@b.c","Email_Verified":"true"}"#).unwrap(),
            "a@b.c"
        );
        assert_eq!(
            ok(r#"{"sub":"repo:hyper-light/shards:ref:refs/heads/dev"}"#).unwrap(),
            "repo:hyper-light/shards:ref:refs/heads/dev"
        );
        assert_eq!(
            ok(r#"{"email":"a@b.c","email_verified":false,"sub":"x"}"#).unwrap_err(),
            "not verified by identity provider"
        );
        assert_eq!(
            ok(r#"{"email":"a@b.c"}"#).unwrap_err(),
            "not verified by identity provider"
        );
        assert_eq!(
            ok(r#"{"email_verified":"yes"}"#).unwrap_err(),
            "invalid value for boolean"
        );
        assert_eq!(
            ok(r#"{"email_verified":null}"#).unwrap_err(),
            "invalid value for boolean"
        );
        assert_eq!(
            ok(r#"{"aud":"sigstore"}"#).unwrap_err(),
            "no subject found in claims"
        );
        assert_eq!(ok("null").unwrap_err(), "no subject found in claims");
        assert_eq!(
            ok(r#"{"sub":5}"#).unwrap_err(),
            "json: cannot unmarshal number into Go struct field claims.sub of type string"
        );
        assert_eq!(
            ok("[1]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type oauthflow.claims"
        );
        assert_eq!(
            subject_of_token("one").unwrap_err(),
            "unable to get subject from identity token"
        );
        assert_eq!(
            subject_of_token("a.b=c.d").unwrap_err(),
            "illegal base64 data at input byte 1"
        );
    }

    #[test]
    fn the_request_is_go_s_json_of_it() {
        let body = request(
            "ECDSA",
            "-----BEGIN PUBLIC KEY-----\nMFk=\n-----END PUBLIC KEY-----\n",
            b"\x01\x02",
        );
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"publicKeyRequest":{"publicKey":{"algorithm":"ECDSA","content":"-----BEGIN PUBLIC KEY-----\nMFk=\n-----END PUBLIC KEY-----\n"},"proofOfPossession":"AQI="}}"#
        );
        let bs = '\\';
        assert_eq!(
            go_json_string("<&>\u{2028}\x01"),
            format!("\"{bs}u003c{bs}u0026{bs}u003e{bs}u2028{bs}u0001\"")
        );
    }

    #[test]
    fn the_leaf_is_the_first_certificate_of_the_chain_answered() {
        let pem = String::from_utf8(crate::tlog::pem::encode("CERTIFICATE", b"leaf")).unwrap();
        let other = String::from_utf8(crate::tlog::pem::encode("CERTIFICATE", b"ca")).unwrap();
        let body = format!(
            r#"{{"signedCertificateEmbeddedSct":{{"chain":{{"certificates":[{},{}]}}}}}}"#,
            go_json_string(&pem),
            go_json_string(&other)
        );
        assert_eq!(certificate(body.as_bytes()).unwrap(), b"leaf");
        let detached = format!(
            r#"{{"signedCertificateDetachedSct":{{"chain":{{"certificates":[{}]}},"signedCertificateTimestamp":"x"}}}}"#,
            go_json_string(&other)
        );
        assert_eq!(certificate(detached.as_bytes()).unwrap(), b"ca");
        assert_eq!(certificate(b"{}").unwrap_err(), "Fulcio returned no certificates");
        assert_eq!(
            certificate(br#"{"signedCertificateEmbeddedSct":{"chain":{"certificates":["x"]}}}"#).unwrap_err(),
            "unable to parse Fulcio certificate"
        );
        assert_eq!(
            certificate(br#"{"signedCertificateEmbeddedSct":{"chain":{"certificates":[5]}}}"#).unwrap_err(),
            "json: cannot unmarshal number into Go struct field chain.signedCertificateEmbeddedSct.chain.certificates of type string"
        );
        assert_eq!(
            certificate(br#"{"signedCertificateDetachedSct":5}"#).unwrap_err(),
            "json: cannot unmarshal number into Go struct field fulcioResponse.signedCertificateDetachedSct of type sign.signedCertificateDetachedSct"
        );
        assert_eq!(
            certificate(br#"{"signedCertificateEmbeddedSct":{"chain":"x"}}"#).unwrap_err(),
            "json: cannot unmarshal string into Go struct field signedCertificateEmbeddedSct.signedCertificateEmbeddedSct.chain of type sign.chain"
        );
    }
}
