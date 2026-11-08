//! A signature's entry in a Rekor transparency log, as sigstore-go v1.2.2 writes one
//! (pkg/sign transparency.go): to Rekor v1, a `dsse` v0.0.1 entry proposed as rekor
//! v1.5.3's client proposes it, the log's answer turned into the bundle's entry as
//! rekor's tle.GenerateTransparencyLogEntry turns it; to Rekor v2 (rekor-tiles v2.3.0), a
//! `hashedrekord` v0.0.2 request over the envelope's PAE digest, the answer the entry.

use crate::bundle::{InclusionProof, TlogEntry};
use crate::fulcio::go_json_string;
use crate::tlog::gocodec;
use crate::tlog::gojson::{self, JValue};

/// Where each version's API takes entries.
pub const V1_ENTRIES_PATH: &str = "/api/v1/log/entries";
pub const V2_ENTRIES_PATH: &str = "/api/v2/log/entries";

/// The API versions sigstore-go writes to (RekorAPIVersions).
pub const API_VERSIONS: &[u32] = &[1, 2];

/// The envelope as json.Marshal writes protobuf-specs' Envelope struct: payload and
/// signature in standard base64, no key ID.
pub fn envelope_json(payload: &[u8], payload_type: &str, signature: &[u8]) -> String {
    format!(
        r#"{{"payload":{},"payloadType":{},"signatures":[{{"sig":{}}}]}}"#,
        go_json_string(&gocodec::std_encode(payload)),
        go_json_string(payload_type),
        go_json_string(&gocodec::std_encode(signature))
    )
}

/// The `dsse` v0.0.1 entry proposed for `envelope` (its JSON) and the certificate or key
/// that verifies it (PEM), as the request's body: go-openapi's polymorphic MarshalJSON of
/// models.DSSE, the spec's `signatures` nil, ended by the newline its JSON producer's
/// json.Encoder writes.
pub fn proposed_dsse(envelope: &str, verifier_pem: &[u8]) -> Vec<u8> {
    format!(
        "{{\"apiVersion\":\"0.0.1\",\"spec\":{{\"proposedContent\":{{\"envelope\":{},\"verifiers\":[{}]}},\"signatures\":null}},\"kind\":\"dsse\"}}\n",
        go_json_string(envelope),
        go_json_string(&gocodec::std_encode(verifier_pem))
    )
    .into_bytes()
}

/// The members of `v` (an object) that set `fields`, the last of each.
fn fields<'v>(v: &'v JValue, names: &[&str]) -> Vec<Option<&'v JValue>> {
    let mut out = vec![None; names.len()];
    if let JValue::Object(m) = v {
        for (i, value) in gojson::struct_members(m, names) {
            if let Some(slot) = out.get_mut(i) {
                *slot = Some(value);
            }
        }
    }
    out
}

/// A required int64 of the answer: a JSON integer in range, else Go's words.
fn int64(v: Option<&JValue>, strukt: &str, field: &str) -> Result<i64, String> {
    match v {
        Some(JValue::Number(n)) => n
            .parse::<i64>()
            .map_err(|_| gojson::type_error(&format!("number {n}"), strukt, field, "int64")),
        Some(JValue::Null) | None => Err(format!("Rekor's answer has no {field}")),
        Some(other) => Err(gojson::type_error(other.kind(), strukt, field, "int64")),
    }
}

fn string<'v>(v: Option<&'v JValue>, strukt: &str, field: &str) -> Result<&'v str, String> {
    match v {
        Some(JValue::Str(s)) => Ok(s),
        Some(JValue::Null) | None => Err(format!("Rekor's answer has no {field}")),
        Some(other) => Err(gojson::type_error(other.kind(), strukt, field, "string")),
    }
}

/// The entry Rekor v1 answered a proposed entry with (201): the member of its
/// `LogEntry` map the answer's ETag names, as GenerateTransparencyLogEntry makes it a
/// bundle's entry: IDs and hashes from hex, the body from base64, its kind and version
/// read from it as rekor's types read it.
pub fn entry_v1(answer: &[u8], etag: &str) -> Result<TlogEntry, String> {
    let v = gojson::decode_first(answer)?;
    let JValue::Object(members) = &v else {
        return Err(gojson::type_error(v.kind(), "", "", "models.LogEntry"));
    };
    let anon = members
        .iter()
        .rev()
        .find(|(k, _)| k == etag)
        .map(|(_, e)| e)
        .ok_or_else(|| format!("Rekor's answer holds no entry {etag}, which its ETag names"))?;
    let s = "LogEntryAnon";
    let [_, body, time, id, index, verification] = fields(
        anon,
        &[
            "attestation",
            "body",
            "integratedTime",
            "logID",
            "logIndex",
            "verification",
        ],
    )[..] else {
        return Err("Rekor's answer is not an entry".into());
    };
    let log_id = gocodec::hex_decode(string(id, s, "logID")?.as_bytes())
        .map_err(|e| format!("decoding logID string: {e}"))?;
    let verification = verification.ok_or("Rekor's answer has no verification")?;
    let [proof, set] = fields(verification, &["inclusionProof", "signedEntryTimestamp"])[..] else {
        return Err("Rekor's answer has no verification".into());
    };
    let proof = proof.ok_or("Rekor's answer has no inclusionProof")?;
    let p = "LogEntryAnonVerificationInclusionProof";
    let [checkpoint, hashes, proof_index, root, size] = fields(
        proof,
        &["checkpoint", "hashes", "logIndex", "rootHash", "treeSize"],
    )[..] else {
        return Err("Rekor's answer has no inclusionProof".into());
    };
    let root_hash = gocodec::hex_decode(string(root, p, "rootHash")?.as_bytes())
        .map_err(|e| format!("decoding inclusion proof root hash: {e}"))?;
    let mut proof_hashes = Vec::new();
    if let Some(JValue::Array(items)) = hashes {
        for h in items {
            let h = h
                .as_str()
                .ok_or_else(|| gojson::type_error(h.kind(), p, "hashes", "string"))?;
            proof_hashes.push(
                gocodec::hex_decode(h.as_bytes())
                    .map_err(|e| format!("decoding inclusion proof hash: {e}"))?,
            );
        }
    }
    let set = match set {
        Some(JValue::Str(b)) => {
            crate::gobase64::decode(b.as_bytes(), false, true).map_err(crate::gobase64::error_text)?
        }
        Some(JValue::Null) | None => Vec::new(),
        Some(other) => {
            return Err(gojson::type_error(
                other.kind(),
                "LogEntryAnonVerification",
                "signedEntryTimestamp",
                "strfmt.Base64",
            ));
        }
    };
    let body = match body {
        Some(JValue::Str(b)) => crate::gobase64::decode(b.as_bytes(), false, true)
            .map_err(|e| format!("base64 decoding body: {}", crate::gobase64::error_text(e)))?,
        Some(other) => {
            let ty = match other {
                JValue::Null => "<nil>",
                JValue::Bool(_) => "bool",
                JValue::Number(_) => "json.Number",
                JValue::Array(_) => "[]interface {}",
                _ => "map[string]interface {}",
            };
            return Err(format!("body is not string or []byte: ({ty})"));
        }
        None => return Err("body is not string or []byte: (<nil>)<nil>".into()),
    };
    let entry = crate::tlog::v1::unmarshal_entry(&body)?;
    let (kind, version) = match entry {
        crate::tlog::v1::V1::Dsse(_) => ("dsse", "0.0.1"),
        crate::tlog::v1::V1::HashedRekord(_) => ("hashedrekord", "0.0.1"),
        crate::tlog::v1::V1::Intoto(_) => ("intoto", "0.0.2"),
    };
    Ok(TlogEntry {
        log_index: int64(index, s, "logIndex")?,
        log_id: Some((!log_id.is_empty()).then_some(log_id)),
        kind_version: Some((kind.to_string(), version.to_string())),
        integrated_time: int64(time, s, "integratedTime")?,
        inclusion_promise: Some((!set.is_empty()).then_some(set)),
        inclusion_proof: Some(InclusionProof {
            log_index: int64(proof_index, p, "logIndex")?,
            root_hash,
            tree_size: int64(size, p, "treeSize")?,
            hashes: proof_hashes,
            checkpoint: Some(string(checkpoint, p, "checkpoint")?.to_string()),
        }),
        canonicalized_body: (!body.is_empty()).then_some(body),
    })
}

/// The request Rekor v2 takes for a signature over `digest` by the holder of `cert`
/// (DER) or `key` (PKIX DER): protojson of rekor-tiles' CreateEntryRequest of a
/// HashedRekordRequestV002, the verifier's key details by name.
pub fn request_v2(
    digest: &[u8],
    signature: &[u8],
    cert: Option<&[u8]>,
    key: &[u8],
    key_details: &str,
) -> Vec<u8> {
    let verifier = match cert {
        Some(der) => format!(r#"{{"x509Certificate":{{"rawBytes":{}}}"#, json_bytes(der)),
        None => format!(r#"{{"publicKey":{{"rawBytes":{}}}"#, json_bytes(key)),
    };
    format!(
        r#"{{"hashedRekordRequestV002":{{"digest":{},"signature":{{"content":{},"verifier":{verifier},"keyDetails":{}}}}}}}}}"#,
        json_bytes(digest),
        json_bytes(signature),
        proto_string(key_details)
    )
    .into_bytes()
}

/// The entry Rekor v2 answered (201): protojson of a TransparencyLogEntry.
pub fn entry_v2(answer: &[u8]) -> Result<TlogEntry, String> {
    let m = crate::proto::unmarshal(answer, &crate::schemas::TRANSPARENCY_LOG_ENTRY)
        .map_err(|e| format!("unmarshaling response body: {}", e.0))?;
    Ok(crate::bundle::tlog_entry(&m))
}

/// protojson's string: `"`, `\` and control characters escaped, `\b`, `\f`, `\n`, `\r`
/// and `\t` short, the rest as four lowercase hex digits; nothing else.
pub fn proto_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// protojson's bytes: standard base64, padded.
fn json_bytes(b: &[u8]) -> String {
    format!("\"{}\"", gocodec::std_encode(b))
}

/// The entry as protojson writes a TransparencyLogEntry: fields in their order, those
/// unset or zero left out, int64s as strings.
pub fn entry_json(e: &TlogEntry) -> String {
    let mut parts = Vec::new();
    if e.log_index != 0 {
        parts.push(format!(r#""logIndex":"{}""#, e.log_index));
    }
    if let Some(id) = &e.log_id {
        let inner = id
            .as_ref()
            .map_or(String::new(), |k| format!(r#""keyId":{}"#, json_bytes(k)));
        parts.push(format!(r#""logId":{{{inner}}}"#));
    }
    if let Some((kind, version)) = &e.kind_version {
        let mut kv = Vec::new();
        if !kind.is_empty() {
            kv.push(format!(r#""kind":{}"#, proto_string(kind)));
        }
        if !version.is_empty() {
            kv.push(format!(r#""version":{}"#, proto_string(version)));
        }
        parts.push(format!(r#""kindVersion":{{{}}}"#, kv.join(",")));
    }
    if e.integrated_time != 0 {
        parts.push(format!(r#""integratedTime":"{}""#, e.integrated_time));
    }
    if let Some(promise) = &e.inclusion_promise {
        let inner = promise.as_ref().map_or(String::new(), |s| {
            format!(r#""signedEntryTimestamp":{}"#, json_bytes(s))
        });
        parts.push(format!(r#""inclusionPromise":{{{inner}}}"#));
    }
    if let Some(p) = &e.inclusion_proof {
        let mut q = Vec::new();
        if p.log_index != 0 {
            q.push(format!(r#""logIndex":"{}""#, p.log_index));
        }
        if !p.root_hash.is_empty() {
            q.push(format!(r#""rootHash":{}"#, json_bytes(&p.root_hash)));
        }
        if p.tree_size != 0 {
            q.push(format!(r#""treeSize":"{}""#, p.tree_size));
        }
        if !p.hashes.is_empty() {
            let hashes: Vec<String> = p.hashes.iter().map(|h| json_bytes(h)).collect();
            q.push(format!(r#""hashes":[{}]"#, hashes.join(",")));
        }
        if let Some(c) = &p.checkpoint {
            let inner = if c.is_empty() {
                String::new()
            } else {
                format!(r#""envelope":{}"#, proto_string(c))
            };
            q.push(format!(r#""checkpoint":{{{inner}}}"#));
        }
        parts.push(format!(r#""inclusionProof":{{{}}}"#, q.join(",")));
    }
    if let Some(body) = &e.canonicalized_body {
        parts.push(format!(r#""canonicalizedBody":{}"#, json_bytes(body)));
    }
    format!("{{{}}}", parts.join(","))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_proposed_entry_is_go_openapi_s() {
        let env = envelope_json(b"{}", "application/vnd.in-toto+json", b"\x01");
        assert_eq!(
            env,
            r#"{"payload":"e30=","payloadType":"application/vnd.in-toto+json","signatures":[{"sig":"AQ=="}]}"#
        );
        let entry = String::from_utf8(proposed_dsse(&env, b"PEM\n")).unwrap();
        assert_eq!(
            entry,
            concat!(
                r#"{"apiVersion":"0.0.1","spec":{"proposedContent":{"envelope":"{\"payload\":\"e30=\",\"payloadType\":\"application/vnd.in-toto+json\",\"signatures\":[{\"sig\":\"AQ==\"}]}","verifiers":["UEVNCg=="]},"signatures":null},"kind":"dsse"}"#,
                "\n"
            )
        );
    }

    /// An entry written back as protojson writes it reads as the bundle reads entries.
    #[test]
    fn an_entry_round_trips_through_protojson() {
        let e = TlogEntry {
            log_index: 7,
            log_id: Some(Some(vec![1, 2])),
            kind_version: Some(("dsse".into(), "0.0.1".into())),
            integrated_time: 1_760_000_000,
            inclusion_promise: Some(Some(vec![3])),
            inclusion_proof: Some(InclusionProof {
                log_index: 6,
                root_hash: vec![4],
                tree_size: 8,
                hashes: vec![vec![5], vec![6]],
                checkpoint: Some("rekor.local - 1\n8\nBA==\n\n\u{2014} rekor.local x\n".into()),
            }),
            canonicalized_body: Some(b"{}".to_vec()),
        };
        let json = entry_json(&e);
        assert!(json.starts_with(r#"{"logIndex":"7","logId":{"keyId":"AQI="},"kindVersion":{"kind":"dsse","version":"0.0.1"},"integratedTime":"1760000000""#));
        assert!(json.contains(r#""envelope":"rekor.local - 1\n8\nBA==\n\n"#));
        assert_eq!(entry_v2(json.as_bytes()).unwrap(), e);
        // Zero fields are left out, as protojson leaves them.
        let zero = TlogEntry {
            log_index: 0,
            integrated_time: 0,
            ..e
        };
        assert!(!entry_json(&zero).contains("logIndex\":\"0"));
    }

    #[test]
    fn the_v2_request_is_protojson_of_it() {
        let r = String::from_utf8(request_v2(
            b"\x01",
            b"\x02",
            Some(b"\x03"),
            b"",
            "PKIX_ECDSA_P256_SHA_256",
        ))
        .unwrap();
        assert_eq!(
            r,
            r#"{"hashedRekordRequestV002":{"digest":"AQ==","signature":{"content":"Ag==","verifier":{"x509Certificate":{"rawBytes":"Aw=="},"keyDetails":"PKIX_ECDSA_P256_SHA_256"}}}}"#
        );
    }
}
