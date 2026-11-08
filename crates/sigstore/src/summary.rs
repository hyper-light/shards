//! Fulcio's certificate extensions and sigstore-go's summary of a certificate
//! (fulcio/certificate: extensions.go, summarize.go).

/// certificate.Extensions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub issuer: String,
    pub github_workflow_trigger: String,
    pub github_workflow_sha: String,
    pub github_workflow_name: String,
    pub github_workflow_repository: String,
    pub github_workflow_ref: String,
    pub build_signer_uri: String,
    pub build_signer_digest: String,
    pub runner_environment: String,
    pub source_repository_uri: String,
    pub source_repository_digest: String,
    pub source_repository_ref: String,
    pub source_repository_identifier: String,
    pub source_repository_owner_uri: String,
    pub source_repository_owner_identifier: String,
    pub build_config_uri: String,
    pub build_config_digest: String,
    pub build_trigger: String,
    pub run_invocation_uri: String,
    pub source_repository_visibility_at_signing: String,
}

/// certificate.Summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub certificate_issuer: String,
    pub subject_alternative_name: String,
    pub extensions: Extensions,
}

use crate::asn1::{self, Fields, Kind, Params, Value};
use crate::x509::{Certificate, OID_SAN};

/// 1.3.6.1.4.1.57264.1, under which Fulcio's extensions are numbered.
const FULCIO: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01];

/// ParseDERString.
fn der_string(value: &[u8]) -> Result<String, String> {
    let (v, rest) = asn1::unmarshal(value, Kind::Str, &Params::default().named("string"))
        .map_err(|e| format!("unexpected error unmarshalling DER-encoded string: {e}"))?;
    if !rest.is_empty() {
        return Err("unexpected trailing bytes in DER-encoded string".into());
    }
    match v {
        Value::Str(s) => Ok(s),
        _ => Err("unexpected error unmarshalling DER-encoded string".into()),
    }
}

/// ParseExtensions.
pub fn parse_extensions(cert: &Certificate) -> Result<Extensions, String> {
    let mut out = Extensions::default();
    for e in &cert.extensions {
        let raw = || String::from_utf8_lossy(&e.value).into_owned();
        let n = match e.oid.strip_prefix(FULCIO) {
            Some([n]) => *n,
            _ => continue,
        };
        match n {
            1 => out.issuer = raw(),
            2 => out.github_workflow_trigger = raw(),
            3 => out.github_workflow_sha = raw(),
            4 => out.github_workflow_name = raw(),
            5 => out.github_workflow_repository = raw(),
            6 => out.github_workflow_ref = raw(),
            8 => out.issuer = der_string(&e.value)?,
            9 => out.build_signer_uri = der_string(&e.value)?,
            10 => out.build_signer_digest = der_string(&e.value)?,
            11 => out.runner_environment = der_string(&e.value)?,
            12 => out.source_repository_uri = der_string(&e.value)?,
            13 => out.source_repository_digest = der_string(&e.value)?,
            14 => out.source_repository_ref = der_string(&e.value)?,
            15 => out.source_repository_identifier = der_string(&e.value)?,
            16 => out.source_repository_owner_uri = der_string(&e.value)?,
            17 => out.source_repository_owner_identifier = der_string(&e.value)?,
            18 => out.build_config_uri = der_string(&e.value)?,
            19 => out.build_config_digest = der_string(&e.value)?,
            20 => out.build_trigger = der_string(&e.value)?,
            21 => out.run_invocation_uri = der_string(&e.value)?,
            22 => out.source_repository_visibility_at_signing = der_string(&e.value)?,
            _ => {}
        }
    }
    Ok(out)
}

/// cryptoutils.UnmarshalOtherNameSAN's value, where there is exactly one.
fn other_name_san(cert: &Certificate) -> Option<String> {
    let mut names = Vec::new();
    for e in cert.extensions.iter().filter(|e| e.oid == OID_SAN) {
        let (seq, rest) = asn1::unmarshal(&e.value, Kind::Raw, &Params::default()).ok()?;
        if !rest.is_empty() {
            return None;
        }
        let Value::Raw {
            class,
            tag,
            compound,
            bytes,
            ..
        } = seq
        else {
            return None;
        };
        if !compound || tag != asn1::TAG_SEQUENCE || class != asn1::CLASS_UNIVERSAL {
            return None;
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let (v, r) = asn1::unmarshal(rest, Kind::Raw, &Params::default()).ok()?;
            rest = r;
            let Value::Raw { tag, full, .. } = v else {
                return None;
            };
            if tag != 0 {
                continue;
            }
            // OtherName, read with "tag:0": its OID, then [0] EXPLICIT a string.
            let (on, _) = asn1::unmarshal(full, Kind::Struct, &Params::tagged(0)).ok()?;
            let Value::Struct { inner, .. } = on else {
                return None;
            };
            let mut f = Fields::new(inner);
            let Some(Value::Oid(id)) = f.next(Kind::Oid, &Params::default()).ok()? else {
                return None;
            };
            let Some(Value::Str(value)) = f.next(Kind::Str, &Params::explicit(0)).ok()? else {
                return None;
            };
            if id != [1, 3, 6, 1, 4, 1, 57264, 1, 7] {
                return None;
            }
            names.push(value);
        }
    }
    if names.len() == 1 { names.pop() } else { None }
}

/// SummarizeCertificate.
pub fn summarize(cert: &Certificate) -> Result<Summary, String> {
    let extensions = parse_extensions(cert)?;
    let mut san = if let Some(u) = cert.uris.first() {
        u.clone()
    } else if let Some(e) = cert.emails.first() {
        e.clone()
    } else {
        String::new()
    };
    if san.is_empty() {
        san = other_name_san(cert).unwrap_or_default();
    }
    if san.is_empty() {
        return Err("no Subject Alternative Name found".into());
    }
    Ok(Summary {
        certificate_issuer: cert.issuer_string(),
        subject_alternative_name: san,
        extensions,
    })
}
