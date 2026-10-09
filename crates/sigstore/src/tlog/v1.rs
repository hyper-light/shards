//! Rekor v1 entry bodies as rekor v1.5.3 reads them for sigstore-go: the proposed entry
//! (models.UnmarshalProposedEntry through go-openapi's JSONConsumer), its kind's model,
//! types.UnmarshalEntry for the three kinds buildx's binary registers (dsse v0.0.1,
//! hashedrekord v0.0.1, intoto v0.0.2), each kind's DecodeEntry, its generated schema's
//! Validate (go-openapi's errors, named as they name them) and its Unmarshal, with the
//! envelope and signature verification those make, and each failure in their words.

use super::gocodec::{STD_ENCODING, URL_ENCODING, corrupt, hex_decode, hex_encode, std_encode};
use super::gojson::{JValue, decode_first, struct_members, type_error, unmarshal};
use super::pem::{RekorKey, new_public_key, verify_cert_chain};
use crate::keys::{self, Load, VerifyWith};
use crate::x509::Hash;

/// A hash as the schemas hold one (algorithm, value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaHash {
    pub algorithm: Option<String>,
    pub value: Option<String>,
}

/// DSSEV001SchemaSignaturesItems0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DsseSignature {
    pub signature: Option<String>,
    pub verifier: Option<Vec<u8>>,
}

/// DSSEV001SchemaProposedContent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposed {
    pub envelope: Option<String>,
    pub verifiers: Option<Vec<Vec<u8>>>,
}

/// DSSEV001Schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dsse {
    pub envelope_hash: Option<SchemaHash>,
    pub payload_hash: Option<SchemaHash>,
    pub proposed: Option<Proposed>,
    pub signatures: Option<Vec<DsseSignature>>,
}

/// HashedrekordV001Schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HashedRekord {
    /// data, and its hash where set.
    pub data: Option<Option<SchemaHash>>,
    /// signature: its content, and its publicKey's content where set.
    pub signature: Option<(Vec<u8>, Option<Vec<u8>>)>,
}

/// IntotoV002SchemaContentEnvelopeSignaturesItems0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntotoSignature {
    pub keyid: String,
    pub public_key: Option<Vec<u8>>,
    pub sig: Option<Vec<u8>>,
}

/// IntotoV002Schema (its content and envelope always present, as DecodeEntry makes them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intoto {
    pub payload: Vec<u8>,
    pub payload_type: Option<String>,
    pub signatures: Option<Vec<IntotoSignature>>,
    pub hash: Option<SchemaHash>,
    pub payload_hash: Option<SchemaHash>,
}

/// A Rekor v1 entry, its schema as Unmarshal leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V1 {
    Dsse(Dsse),
    HashedRekord(HashedRekord),
    Intoto(Intoto),
}

const DSSE_SIGNATURE_PATTERN: &str =
    r"^(?:[A-Za-z0-9+\/]{4})*(?:[A-Za-z0-9+\/]{2}==|[A-Za-z0-9+\/]{3}=|[A-Za-z0-9+\/]{4})$";

/// The kinds models.unmarshalProposedEntry knows.
const KINDS: [&str; 12] = [
    "ProposedEntry",
    "alpine",
    "cose",
    "dsse",
    "hashedrekord",
    "helm",
    "intoto",
    "jar",
    "rekord",
    "rfc3161",
    "rpm",
    "tuf",
];

/// The model's `data` decode (apiVersion and spec), as the model's UnmarshalJSON makes it:
/// apiVersion where set, and spec.
fn model_fields(v: &JValue) -> Result<(Option<String>, JValue), String> {
    let JValue::Object(members) = v else {
        return Ok((None, JValue::Null));
    };
    let mut api: Option<String> = None;
    let mut spec = JValue::Null;
    let mut first_err: Option<String> = None;
    for (i, val) in struct_members(members, &["apiVersion", "spec"]) {
        match (i, val) {
            (0, JValue::Null) => api = None,
            (0, JValue::Str(s)) => api = Some(s.clone()),
            (0, other) => {
                first_err.get_or_insert_with(|| type_error(other.kind(), "", "apiVersion", "string"));
            }
            (_, other) => spec = other.clone(),
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok((api, spec)),
    }
}

/// models.UnmarshalProposedEntry then types.UnmarshalEntry.
pub fn unmarshal_entry(body: &[u8]) -> Result<V1, String> {
    let v = decode_first(body)?;
    // The kind, as struct{ Kind string `json:"kind"` } takes it.
    let mut kind = String::new();
    match &v {
        JValue::Object(members) => {
            let mut first_err: Option<String> = None;
            for (_, val) in struct_members(members, &["kind"]) {
                match val {
                    JValue::Null => {}
                    JValue::Str(s) => kind = s.clone(),
                    other => {
                        first_err.get_or_insert_with(|| type_error(other.kind(), "", "kind", "string"));
                    }
                }
            }
            if let Some(e) = first_err {
                return Err(e);
            }
        }
        JValue::Null => {}
        other => {
            return Err(type_error(
                other.kind(),
                "",
                "",
                "struct { Kind string \"json:\\\"kind\\\"\" }",
            ));
        }
    }
    if kind.is_empty() {
        return Err("kind in body is required".into());
    }
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "invalid kind value: {}",
            shards_dockerfile::go::quote(kind.as_bytes())
        ));
    }
    if kind == "ProposedEntry" {
        return Err("could not unmarshal entry for kind 'ProposedEntry'".into());
    }
    let (api, spec) = model_fields(&v)?;
    let version = match kind.as_str() {
        "dsse" | "hashedrekord" | "intoto" => api.ok_or("api version cannot be nil")?,
        k => return Err(format!("could not unmarshal entry for kind '{k}'")),
    };
    let supported = if kind == "intoto" { "0.0.2" } else { "0.0.1" };
    let parsed = super::semver::parse(&version)
        .map_err(|e| format!("{kind} implementation for version '{version}' not found: {e}"))?;
    if !parsed.equals(supported) {
        return Err(format!(
            "{kind} implementation for version '{version}' not found: unable to locate entry for version {version}"
        ));
    }
    match kind.as_str() {
        "dsse" => dsse_unmarshal(&spec).map(V1::Dsse),
        "hashedrekord" => hashedrekord_unmarshal(&spec).map(V1::HashedRekord),
        _ => intoto_unmarshal(&spec).map(V1::Intoto),
    }
}

/// %T of a decoded `any`.
fn go_type(v: &JValue) -> &'static str {
    match v {
        JValue::Null => "<nil>",
        JValue::Bool(_) => "bool",
        JValue::Number(_) => "json.Number",
        JValue::Str(_) => "string",
        JValue::Array(_) => "[]interface {}",
        JValue::Object(_) => "map[string]interface {}",
    }
}

fn hash_of(v: Option<&JValue>) -> Option<SchemaHash> {
    let h @ JValue::Object(_) = v? else {
        return None;
    };
    Some(SchemaHash {
        algorithm: h.get("algorithm").and_then(JValue::as_str).map(str::to_string),
        value: h.get("value").and_then(JValue::as_str).map(str::to_string),
    })
}

/// A base64 field: StdEncoding.Decode into DecodedLen octets.
fn b64(s: &str, what: &str) -> Result<Vec<u8>, String> {
    STD_ENCODING
        .decode(s.as_bytes())
        .map_err(|at| format!("failed parsing base64 data for {what}: {}", corrupt(at)))
}

/// dsse's DecodeEntry.
fn dsse_decode(input: &JValue) -> Result<Dsse, String> {
    if !matches!(input, JValue::Object(_)) {
        return Err(format!(
            "unsupported input type {} for DecodeEntry",
            go_type(input)
        ));
    }
    let mut m = Dsse::default();
    if let Some(pc @ JValue::Object(_)) = input.get("proposedContent") {
        let mut p = Proposed {
            envelope: pc.get("envelope").and_then(JValue::as_str).map(str::to_string),
            verifiers: None,
        };
        if let Some(JValue::Array(vs)) = pc.get("verifiers") {
            let mut out = Vec::with_capacity(vs.len());
            for it in vs {
                if let JValue::Str(s) = it
                    && !s.is_empty()
                {
                    out.push(b64(s, "verifier")?);
                }
            }
            p.verifiers = Some(out);
        }
        m.proposed = Some(p);
    }
    if let Some(JValue::Array(sigs)) = input.get("signatures") {
        let mut out = Vec::with_capacity(sigs.len());
        for s in sigs {
            if let JValue::Object(_) = s {
                let mut item = DsseSignature {
                    signature: s.get("signature").and_then(JValue::as_str).map(str::to_string),
                    verifier: None,
                };
                if let Some(JValue::Str(vr)) = s.get("verifier")
                    && !vr.is_empty()
                {
                    item.verifier = Some(b64(vr, "signature verifier")?);
                }
                out.push(item);
            }
        }
        m.signatures = Some(out);
    }
    m.envelope_hash = hash_of(input.get("envelopeHash"));
    m.payload_hash = hash_of(input.get("payloadHash"));
    Ok(m)
}

/// A validation failure: go-openapi's Validation, named as its ValidateName renames it.
fn named(prefix: &str, msg: String) -> String {
    format!("{prefix}.{msg}")
}

fn required(path: &str) -> String {
    format!("{path} in body is required")
}

/// A hash's Validate (its algorithm one of `algs`, its value required): its first
/// failure.
fn hash_validate(h: &SchemaHash, path: &str, algs: &[&str]) -> Option<String> {
    let mut res = Vec::new();
    match &h.algorithm {
        None => res.push(required(&format!("{path}.algorithm"))),
        Some(a) if !algs.contains(&a.as_str()) => res.push(format!(
            "{path}.algorithm in body should be one of [{}]",
            algs.join(" ")
        )),
        _ => {}
    }
    if h.value.is_none() {
        res.push(required(&format!("{path}.value")));
    }
    res.into_iter().next()
}

/// "validation failure list" with each failure, as CompositeError prints it.
fn composite(res: Vec<String>) -> Result<(), String> {
    if res.is_empty() {
        return Ok(());
    }
    Err(format!("validation failure list:\n{}", res.join("\n")))
}

/// The DSSE signature pattern's match.
fn dsse_signature_pattern(s: &str) -> bool {
    let b = s.as_bytes();
    let alpha = |c: &u8| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/';
    if b.len() < 4 || !b.len().is_multiple_of(4) {
        return false;
    }
    let (head, last) = b.split_at(b.len() - 4);
    if !head.iter().all(alpha) {
        return false;
    }
    match last {
        [a, b2, b'=', b'='] => alpha(a) && alpha(b2),
        [a, b2, c, b'='] => alpha(a) && alpha(b2) && alpha(c),
        [a, b2, c, d] => alpha(a) && alpha(b2) && alpha(c) && alpha(d),
        _ => false,
    }
}

/// DSSEV001Schema.Validate.
pub fn dsse_validate(m: &Dsse) -> Result<(), String> {
    let mut res = Vec::new();
    if let Some(h) = &m.envelope_hash
        && let Some(e) = hash_validate(h, "envelopeHash", &["sha256"])
    {
        res.push(named("envelopeHash", e));
    }
    if let Some(h) = &m.payload_hash
        && let Some(e) = hash_validate(h, "payloadHash", &["sha256"])
    {
        res.push(named("payloadHash", e));
    }
    if let Some(p) = &m.proposed {
        let mut inner = Vec::new();
        if p.envelope.is_none() {
            inner.push(required("proposedContent.envelope"));
        }
        match &p.verifiers {
            None => inner.push(required("proposedContent.verifiers")),
            Some(v) if v.is_empty() => {
                inner.push("proposedContent.verifiers in body should have at least 1 items".into());
            }
            _ => {}
        }
        if let Some(e) = inner.into_iter().next() {
            res.push(named("proposedContent", e));
        }
    }
    if let Some(sigs) = &m.signatures {
        if sigs.is_empty() {
            res.push("signatures in body should have at least 1 items".into());
        } else {
            for (i, s) in sigs.iter().enumerate() {
                let mut inner = Vec::new();
                match &s.signature {
                    None => inner.push(required("signature")),
                    Some(sig) if !dsse_signature_pattern(sig) => {
                        inner.push(format!(
                            "signature in body should match '{DSSE_SIGNATURE_PATTERN}'"
                        ));
                    }
                    _ => {}
                }
                if s.verifier.is_none() {
                    inner.push(required("verifier"));
                }
                if let Some(e) = inner.into_iter().next() {
                    res.push(named(&format!("signatures.{i}"), e));
                    break;
                }
            }
        }
    }
    composite(res)
}

/// A json.Unmarshal of a dsse.Envelope: (payloadType, payload, [(keyid, sig)]).
type GoEnvelope = (String, String, Vec<(String, String)>);

fn go_envelope(text: &str) -> Result<GoEnvelope, String> {
    let v = unmarshal(text.as_bytes())?;
    let mut env: GoEnvelope = (String::new(), String::new(), Vec::new());
    let members = match &v {
        JValue::Null => return Ok(env),
        JValue::Object(m) => m,
        other => return Err(type_error(other.kind(), "", "", "dsse.Envelope")),
    };
    let mut first_err: Option<String> = None;
    let mut fail = |e: String| {
        first_err.get_or_insert(e);
    };
    for (i, val) in struct_members(members, &["payloadType", "payload", "signatures"]) {
        match (i, val) {
            (_, JValue::Null) if i < 2 => {}
            (0, JValue::Str(s)) => env.0 = s.clone(),
            (1, JValue::Str(s)) => env.1 = s.clone(),
            (0, other) => fail(type_error(other.kind(), "Envelope", "payloadType", "string")),
            (1, other) => fail(type_error(other.kind(), "Envelope", "payload", "string")),
            (_, JValue::Null) => env.2 = Vec::new(),
            (_, JValue::Array(items)) => {
                let mut sigs = Vec::with_capacity(items.len());
                for item in items {
                    let mut sig = (String::new(), String::new());
                    match item {
                        JValue::Null => {}
                        JValue::Object(m) => {
                            for (j, f) in struct_members(m, &["keyid", "sig"]) {
                                match (j, f) {
                                    (_, JValue::Null) => {}
                                    (0, JValue::Str(s)) => sig.0 = s.clone(),
                                    (_, JValue::Str(s)) => sig.1 = s.clone(),
                                    (0, other) => fail(type_error(
                                        other.kind(),
                                        "Signature",
                                        "signatures.keyid",
                                        "string",
                                    )),
                                    (_, other) => fail(type_error(
                                        other.kind(),
                                        "Signature",
                                        "signatures.sig",
                                        "string",
                                    )),
                                }
                            }
                        }
                        other => fail(type_error(
                            other.kind(),
                            "Envelope",
                            "signatures",
                            "dsse.Signature",
                        )),
                    }
                    sigs.push(sig);
                }
                env.2 = sigs;
            }
            (_, other) => fail(type_error(
                other.kind(),
                "Envelope",
                "signatures",
                "[]dsse.Signature",
            )),
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(env),
    }
}

/// dsse's b64Decode: standard, then URL, base64.
pub fn dsse_b64(s: &str) -> Result<Vec<u8>, String> {
    STD_ENCODING
        .decode(s.as_bytes())
        .or_else(|_| URL_ENCODING.decode(s.as_bytes()))
        .map_err(|_| "unable to base64 decode payload (is payload in the right format?)".to_string())
}

/// dsse.PAE.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {payload_type} {} ", payload_type.len(), payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// An EnvelopeVerifier of one key (its key ID unknown) verifying `env`: the signature
/// it accepts, or its failure.
fn envelope_verify(
    v: &keys::Verifier,
    payload_type: &str,
    payload: &str,
    sigs: &[(String, String)],
) -> Result<String, String> {
    if sigs.is_empty() {
        return Err("no signature found".into());
    }
    let body = dsse_b64(payload)?;
    let msg = pae(payload_type, &body);
    // Every signature is decoded, those after the one accepted too.
    let mut accepted: Option<String> = None;
    for (_, s) in sigs {
        let sig = dsse_b64(s)?;
        if accepted.is_none() && v.verify(&sig, &msg, &VerifyWith::default()).is_ok() {
            accepted = Some(s.clone());
        }
    }
    accepted.ok_or_else(|| "accepted signatures do not match threshold, Found: 0, Expected 1".to_string())
}

/// rekor's verifyEnvelope (dsse's, which stops once every signature has a key, or
/// intoto's): each key must verify a signature; every signature must have a key. Each
/// accepted signature with its key.
fn verify_envelope(
    keys_pem: &[Vec<u8>],
    payload_type: &str,
    payload: &str,
    sigs: &[(String, String)],
    stop_early: bool,
) -> Result<Vec<(String, RekorKey)>, String> {
    let mut all: Vec<String> = Vec::new();
    for (_, s) in sigs {
        if !all.contains(s) {
            all.push(s.clone());
        }
    }
    let mut by_sig: Vec<(String, RekorKey)> = Vec::new();
    for pem in keys_pem {
        if stop_early && all.is_empty() {
            break;
        }
        let key = new_public_key(pem).map_err(|e| format!("could not parse public key as x509: {e}"))?;
        let v = keys::load(
            &key.crypto_key(),
            Load {
                hash: Some(Some(Hash::Sha256)),
                ..Load::default()
            },
        )
        .map_err(|e| format!("could not load verifier: {e}"))?;
        let accepted = envelope_verify(&v, payload_type, payload, sigs)
            .map_err(|e| format!("could not verify envelope: {e}"))?;
        all.retain(|s| *s != accepted);
        by_sig.retain(|(s, _)| *s != accepted);
        by_sig.push((accepted, key));
    }
    if !all.is_empty() {
        return Err("all signatures must have a key that verifies it".into());
    }
    Ok(by_sig)
}

/// pem.EncodeToMemory of one block.
fn pem_encode(kind: &str, der: &[u8]) -> Vec<u8> {
    let b64 = std_encode(der);
    let mut out = format!("-----BEGIN {kind}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str(&format!("-----END {kind}-----\n"));
    out.into_bytes()
}

/// A key's CanonicalValue (cryptoutils' PEM of it).
fn canonical_pem(key: &RekorKey) -> Result<Vec<u8>, String> {
    match key {
        RekorKey::Key(k) => keys::marshal_pkix(k)
            .map(|der| pem_encode("PUBLIC KEY", &der))
            .ok_or_else(|| "x509: unsupported public key type".to_string()),
        RekorKey::Certificate(c) => Ok(pem_encode("CERTIFICATE", &c.raw)),
        RekorKey::Chain(cs) => Ok(cs
            .iter()
            .flat_map(|c| pem_encode("CERTIFICATE", &c.raw))
            .collect()),
    }
}

fn sha256(b: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b)
        .as_ref()
        .to_vec()
}

/// dsse v0.0.1's Unmarshal.
fn dsse_unmarshal(spec: &JValue) -> Result<Dsse, String> {
    let mut m = dsse_decode(spec)?;
    dsse_validate(&m)?;
    let Some(proposed) = m.proposed.clone() else {
        if m.envelope_hash.is_none()
            || m.payload_hash.is_none()
            || m.signatures.as_ref().is_none_or(Vec::is_empty)
        {
            return Err(
                "either proposedContent or envelopeHash, payloadHash, and signatures must be present".into(),
            );
        }
        return Ok(m);
    };
    if m.envelope_hash.is_some()
        || m.payload_hash.is_some()
        || m.signatures.as_ref().is_some_and(|s| !s.is_empty())
    {
        return Err(
            "either proposedContent or envelopeHash, payloadHash, and signatures must be present but not both".into(),
        );
    }
    let envelope_text = proposed.envelope.ok_or("proposed content envelope is missing")?;
    let (payload_type, payload, sigs) = go_envelope(&envelope_text)?;
    if sigs.is_empty() {
        return Err("DSSE envelope must contain 1 or more signatures".into());
    }
    let verified = verify_envelope(
        &proposed.verifiers.unwrap_or_default(),
        &payload_type,
        &payload,
        &sigs,
        true,
    )?;
    let mut sorted = verified;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut signatures = m.signatures.take().unwrap_or_default();
    for (sig, key) in &sorted {
        signatures.push(DsseSignature {
            signature: Some(sig.clone()),
            verifier: Some(canonical_pem(key)?),
        });
    }
    let decoded = dsse_b64(&payload)?;
    m.signatures = Some(signatures);
    m.payload_hash = Some(SchemaHash {
        algorithm: Some("sha256".into()),
        value: Some(hex_encode(&sha256(&decoded))),
    });
    m.envelope_hash = Some(SchemaHash {
        algorithm: Some("sha256".into()),
        value: Some(hex_encode(&sha256(envelope_text.as_bytes()))),
    });
    m.proposed = None;
    Ok(m)
}

/// hashedrekord's DecodeEntry.
fn hashedrekord_decode(input: &JValue) -> Result<HashedRekord, String> {
    if !matches!(input, JValue::Object(_)) {
        return Err(format!(
            "unsupported input type {} for DecodeEntry",
            go_type(input)
        ));
    }
    let mut m = HashedRekord::default();
    if let Some(sig @ JValue::Object(_)) = input.get("signature") {
        let mut content = Vec::new();
        if let Some(JValue::Str(c)) = sig.get("content")
            && !c.is_empty()
        {
            content = b64(c, "signature content")?;
        }
        let mut key = None;
        if let Some(pk @ JValue::Object(_)) = sig.get("publicKey") {
            let mut k = Vec::new();
            if let Some(JValue::Str(c)) = pk.get("content")
                && !c.is_empty()
            {
                k = b64(c, "public key content")?;
            }
            key = Some(k);
        }
        m.signature = Some((content, key));
    }
    if let Some(data @ JValue::Object(_)) = input.get("data")
        && let Some(h @ JValue::Object(_)) = data.get("hash")
    {
        m.data = Some(hash_of(Some(h)));
    }
    Ok(m)
}

/// HashedrekordV001Schema.Validate.
pub fn hashedrekord_validate(m: &HashedRekord) -> Result<(), String> {
    let mut res = Vec::new();
    match &m.data {
        None => res.push(required("data")),
        Some(Some(h)) => {
            if let Some(e) = hash_validate(h, "data.hash", &["sha256", "sha384", "sha512"]) {
                res.push(named("data", named("data.hash", e)));
            }
        }
        Some(None) => {}
    }
    if m.signature.is_none() {
        res.push(required("signature"));
    }
    composite(res)
}

/// hashedrekord v0.0.1's Unmarshal: decoded, validated, its signature verified.
fn hashedrekord_unmarshal(spec: &JValue) -> Result<HashedRekord, String> {
    let m = hashedrekord_decode(spec)?;
    hashedrekord_validate(&m)?;
    let Some((sig, key)) = &m.signature else {
        return Err("missing signature".into());
    };
    let Some(key) = key else {
        return Err("missing public key".into());
    };
    let key = new_public_key(key)?;
    let Some(Some(hash)) = &m.data else {
        return Err("missing hash".into());
    };
    let value = hash.value.clone().unwrap_or_default();
    let alg = match hash.algorithm.as_deref() {
        Some("sha384") => Hash::Sha384,
        Some("sha512") => Hash::Sha512,
        _ => Hash::Sha256,
    };
    if value.len() != alg.size() * 2 {
        return Err("invalid value for hash".into());
    }
    let digest = hex_decode(value.as_bytes())?;
    if sig.is_empty() {
        return Err("verifying signature: X509 signature has not been initialized".into());
    }
    let pk = match &key {
        RekorKey::Chain(cs) => {
            verify_cert_chain(cs).map_err(|e| format!("verifying signature: {e}"))?;
            key.crypto_key()
        }
        _ => key.crypto_key(),
    };
    let v = keys::load(
        &pk,
        Load {
            ed25519ph: true,
            ..Load::default()
        },
    )
    .map_err(|e| format!("verifying signature: {e}"))?;
    v.verify(
        sig,
        &[],
        &VerifyWith {
            digest: Some(&digest),
            hash: Some(Some(alg)),
        },
    )
    .map_err(|e| format!("verifying signature: {e}"))?;
    Ok(m)
}

/// intoto's DecodeEntry.
fn intoto_decode(input: &JValue) -> Result<Intoto, String> {
    if !matches!(input, JValue::Object(_)) {
        return Err(format!(
            "unsupported input type {} for DecodeEntry",
            go_type(input)
        ));
    }
    let mut m = Intoto::default();
    let Some(c @ JValue::Object(_)) = input.get("content") else {
        return Ok(m);
    };
    if let Some(env @ JValue::Object(_)) = c.get("envelope") {
        m.payload_type = env
            .get("payloadType")
            .and_then(JValue::as_str)
            .map(str::to_string);
        if let Some(JValue::Str(p)) = env.get("payload")
            && !p.is_empty()
        {
            m.payload = b64(p, "payload")?;
        }
        if let Some(JValue::Array(sigs)) = env.get("signatures") {
            let mut out = Vec::with_capacity(sigs.len());
            for s in sigs {
                if let JValue::Object(_) = s {
                    let mut item = IntotoSignature {
                        keyid: s
                            .get("keyid")
                            .and_then(JValue::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        ..IntotoSignature::default()
                    };
                    if let Some(JValue::Str(sig)) = s.get("sig") {
                        item.sig = Some(b64(sig, "signature")?);
                    }
                    if let Some(JValue::Str(pk)) = s.get("publicKey") {
                        item.public_key = Some(b64(pk, "public key")?);
                    }
                    out.push(item);
                }
            }
            m.signatures = Some(out);
        }
    }
    m.hash = hash_of(c.get("hash"));
    m.payload_hash = hash_of(c.get("payloadHash"));
    Ok(m)
}

/// IntotoV002Schema.Validate.
pub fn intoto_validate(m: &Intoto) -> Result<(), String> {
    let mut content = Vec::new();
    let mut env = Vec::new();
    if m.payload_type.is_none() {
        env.push(required("content.envelope.payloadType"));
    }
    match &m.signatures {
        None => env.push(required("content.envelope.signatures")),
        Some(s) if s.is_empty() => {
            env.push("content.envelope.signatures in body should have at least 1 items".into())
        }
        Some(sigs) => {
            for (i, s) in sigs.iter().enumerate() {
                let mut inner = Vec::new();
                if s.public_key.is_none() {
                    inner.push(required("publicKey"));
                }
                if s.sig.is_none() {
                    inner.push(required("sig"));
                }
                if let Some(e) = inner.into_iter().next() {
                    env.push(named(&format!("content.envelope.signatures.{i}"), e));
                    break;
                }
            }
        }
    }
    if let Some(e) = env.into_iter().next() {
        content.push(named("content.envelope", e));
    }
    if let Some(h) = &m.hash
        && let Some(e) = hash_validate(h, "content.hash", &["sha256"])
    {
        content.push(named("content.hash", e));
    }
    if let Some(h) = &m.payload_hash
        && let Some(e) = hash_validate(h, "content.payloadHash", &["sha256"])
    {
        content.push(named("content.payloadHash", e));
    }
    composite(
        content
            .into_iter()
            .next()
            .map(|e| named("content", e))
            .into_iter()
            .collect(),
    )
}

/// intoto v0.0.2's Unmarshal.
fn intoto_unmarshal(spec: &JValue) -> Result<Intoto, String> {
    let mut m = intoto_decode(spec)?;
    intoto_validate(&m)?;
    if m.payload.is_empty() {
        return Ok(m);
    }
    let payload = String::from_utf8_lossy(&m.payload).into_owned();
    let payload_type = m.payload_type.clone().unwrap_or_default();
    let mut sigs = Vec::new();
    let mut keys_pem = Vec::new();
    for s in m.signatures.as_deref().unwrap_or_default() {
        sigs.push((
            s.keyid.clone(),
            String::from_utf8_lossy(s.sig.as_deref().unwrap_or_default()).into_owned(),
        ));
        keys_pem.push(s.public_key.clone().unwrap_or_default());
    }
    verify_envelope(&keys_pem, &payload_type, &payload, &sigs, false)?;
    let decoded = STD_ENCODING
        .decode(&m.payload)
        .map_err(|at| format!("could not decode envelope payload: {}", corrupt(at)))?;
    m.payload_hash = Some(SchemaHash {
        algorithm: Some("sha256".into()),
        value: Some(hex_encode(&sha256(&decoded))),
    });
    Ok(m)
}

/// The signature an entry carries (Entry.Signature for v1): empty where it cannot be
/// read.
pub fn signature(e: &V1) -> Vec<u8> {
    match e {
        V1::Dsse(d) => d
            .signatures
            .as_ref()
            .and_then(|s| s.first())
            .and_then(|s| s.signature.as_ref())
            .and_then(|s| STD_ENCODING.decode(s.as_bytes()).ok())
            .unwrap_or_default(),
        V1::HashedRekord(h) => h.signature.as_ref().map(|s| s.0.clone()).unwrap_or_default(),
        V1::Intoto(i) => i
            .signatures
            .as_ref()
            .and_then(|s| s.first())
            .and_then(|s| s.sig.as_ref())
            .and_then(|s| STD_ENCODING.decode(s).ok())
            .unwrap_or_default(),
    }
}

/// The PEM an entry's key is in (the first signature's verifier).
pub fn key_pem(e: &V1) -> Option<Vec<u8>> {
    match e {
        V1::Dsse(d) => d.signatures.as_ref()?.first()?.verifier.clone(),
        V1::HashedRekord(h) => h.signature.as_ref()?.1.clone(),
        V1::Intoto(i) => i.signatures.as_ref()?.first()?.public_key.clone(),
    }
}

/// GetHashedRekordDigest for v1: the hash and its algorithm.
pub fn hashed_rekord_digest(e: &V1) -> Option<(Vec<u8>, String)> {
    let V1::HashedRekord(h) = e else {
        return None;
    };
    let hash = h.data.as_ref()?.as_ref()?;
    let value = hash.value.as_ref()?;
    let digest = hex_decode(value.as_bytes()).ok()?;
    Some((digest, hash.algorithm.clone().unwrap_or_default()))
}

/// GetDssePayloadHash for v1.
pub fn dsse_payload_hash(e: &V1) -> Option<Vec<u8>> {
    let value = match e {
        V1::Dsse(d) => d.payload_hash.as_ref()?.value.as_ref()?,
        V1::Intoto(i) => i.payload_hash.as_ref()?.value.as_ref()?,
        V1::HashedRekord(_) => return None,
    };
    hex_decode(value.as_bytes()).ok()
}

/// ValidateEntry for v1: the schema validated again.
pub fn validate(e: &V1) -> Result<(), String> {
    match e {
        V1::Dsse(d) => dsse_validate(d),
        V1::HashedRekord(h) => hashedrekord_validate(h),
        V1::Intoto(i) => intoto_validate(i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_pattern_matches_go_s() {
        assert!(dsse_signature_pattern("YWJj"));
        assert!(dsse_signature_pattern("YWJjZA=="));
        assert!(dsse_signature_pattern("YWJjZGU="));
        assert!(!dsse_signature_pattern(""));
        assert!(!dsse_signature_pattern("YWJ"));
        assert!(dsse_signature_pattern("YW=="));
        assert!(!dsse_signature_pattern("YW==YWJj"));
        assert!(!dsse_signature_pattern("Y==="));
        assert!(!dsse_signature_pattern("YW-j"));
    }

    #[test]
    fn kinds_fail_as_rekor_s() {
        assert_eq!(unmarshal_entry(b"{}").unwrap_err(), "kind in body is required");
        assert_eq!(
            unmarshal_entry(b"{\"kind\":\"x\"}").unwrap_err(),
            "invalid kind value: \"x\""
        );
        assert_eq!(
            unmarshal_entry(b"{\"kind\":\"rekord\",\"apiVersion\":\"0.0.1\"}").unwrap_err(),
            "could not unmarshal entry for kind 'rekord'"
        );
        assert_eq!(
            unmarshal_entry(b"{\"kind\":\"dsse\"}").unwrap_err(),
            "api version cannot be nil"
        );
        assert_eq!(
            unmarshal_entry(b"{\"kind\":\"dsse\",\"apiVersion\":\"0.0.2\",\"spec\":{}}").unwrap_err(),
            "dsse implementation for version '0.0.2' not found: unable to locate entry for version 0.0.2"
        );
    }
}
