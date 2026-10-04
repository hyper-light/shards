//! A server's certificate chain, evaluated by the system's trust (Security.framework), as
//! rustls-platform-verifier 0.7.1 evaluates it (src/verification/apple.rs,
//! `verify_certificate`): the same calls, in the same order, with the same arguments, so
//! that what it trusts and refuses is what it did. Only when Security is loaded differs.

use crate::frameworks::{self, CFTypeRef, Owned};

/// Security's codes for the refusals the verifier names (SecBase.h).
const HOST_NAME_MISMATCH: isize = -67602;
const CREATE_CHAIN_FAILED: isize = -25318;
const INVALID_EXTENDED_KEY_USAGE: isize = -67609;
const CERTIFICATE_REVOKED: isize = -67820;

/// Why a chain was not trusted, as the verifier tells rustls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A certificate is not DER Security can read.
    BadEncoding,
    /// The leaf does not name the server.
    NotValidForName,
    /// No chain to a trusted root.
    UnknownIssuer,
    /// The leaf is not for server authentication.
    ExtendedKeyUsage,
    Revoked,
    /// The time is before Apple's epoch.
    FailedToGetCurrentTime,
    /// Another refusal of the chain: Security's words and code, or a step that failed.
    Invalid(String),
    /// The evaluation could not be made at all.
    General(String),
    /// The frameworks could not be loaded.
    Unavailable(String),
}

/// Evaluates `chain` (its leaf first) for `server` at `now_unix` seconds since 1970, with
/// the stapled `ocsp` response if there is one, trusting `extra_roots` beside the system's.
pub fn evaluate(
    chain: &[&[u8]],
    server: &str,
    ocsp: Option<&[u8]>,
    now_unix: u64,
    extra_roots: &[&[u8]],
) -> Result<(), Refusal> {
    let api = frameworks::api().map_err(Refusal::Unavailable)?;
    let certificate = |der: &[u8]| -> Result<Owned<'_>, Refusal> {
        let data = frameworks::data(api, der).ok_or(Refusal::BadEncoding)?;
        // SAFETY: a CFData we hold; the certificate, if made, is ours.
        Owned::new(api, unsafe {
            (api.SecCertificateCreateWithData)(std::ptr::null(), data.ptr)
        })
        .ok_or(Refusal::BadEncoding)
    };
    let certificates = chain
        .iter()
        .map(|c| certificate(c))
        .collect::<Result<Vec<_>, _>>()?;
    let refs: Vec<CFTypeRef> = certificates.iter().map(|c| c.ptr).collect();
    let certificates_array =
        frameworks::array(api, &refs).ok_or_else(|| Refusal::General("no array".into()))?;
    // The server's side, and its name, which the leaf must match.
    let name = frameworks::string(api, server).ok_or_else(|| Refusal::General("no name".into()))?;
    // SAFETY: a live CFString; the policy is ours.
    let policy = Owned::new(api, unsafe { (api.SecPolicyCreateSSL)(1, name.ptr) })
        .ok_or_else(|| Refusal::General("no SSL policy".into()))?;
    let policies =
        frameworks::array(api, &[policy.ptr]).ok_or_else(|| Refusal::General("no array".into()))?;
    let mut trust_ptr: CFTypeRef = std::ptr::null();
    // SAFETY: two live arrays; the trust, if made, is ours.
    let status =
        unsafe { (api.SecTrustCreateWithCertificates)(certificates_array.ptr, policies.ptr, &mut trust_ptr) };
    if status != 0 {
        return Err(Refusal::General(frameworks::status_text(api, status)));
    }
    let trust = Owned::new(api, trust_ptr).ok_or_else(|| Refusal::General("no trust".into()))?;
    // The time rustls gave, in Apple's epoch: whole seconds, as the verifier takes them.
    let since = api.since_1970 as u64;
    let epoch = now_unix
        .checked_sub(since)
        .ok_or(Refusal::FailedToGetCurrentTime)?;
    // SAFETY: a CFDate of a finite time; ours.
    let date = Owned::new(api, unsafe { (api.CFDateCreate)(std::ptr::null(), epoch as f64) })
        .ok_or_else(|| Refusal::General("no date".into()))?;
    let invalid = |status| Refusal::Invalid(frameworks::status_text(api, status));
    // SAFETY: a live trust and date.
    let status = unsafe { (api.SecTrustSetVerifyDate)(trust.ptr, date.ptr) };
    if status != 0 {
        return Err(invalid(status));
    }
    if let Some(ocsp) = ocsp {
        let response = frameworks::data(api, ocsp).ok_or_else(|| Refusal::General("no data".into()))?;
        let responses =
            frameworks::array(api, &[response.ptr]).ok_or_else(|| Refusal::General("no array".into()))?;
        // SAFETY: a live trust and an array of CFData.
        let status = unsafe { (api.SecTrustSetOCSPResponse)(trust.ptr, responses.ptr) };
        if status != 0 {
            return Err(invalid(status));
        }
    }
    if !extra_roots.is_empty() {
        let roots = extra_roots
            .iter()
            .map(|c| certificate(c))
            .collect::<Result<Vec<_>, _>>()?;
        let refs: Vec<CFTypeRef> = roots.iter().map(|c| c.ptr).collect();
        let anchors = frameworks::array(api, &refs).ok_or_else(|| Refusal::General("no array".into()))?;
        // SAFETY: a live trust and an array of certificates.
        let status = unsafe { (api.SecTrustSetAnchorCertificates)(trust.ptr, anchors.ptr) };
        if status != 0 {
            return Err(Refusal::Invalid(frameworks::status_text(api, status)));
        }
        // The system's roots still count: setting anchors alone would trust only them.
        // SAFETY: a live trust.
        let status = unsafe { (api.SecTrustSetAnchorCertificatesOnly)(trust.ptr, 0) };
        if status != 0 {
            return Err(Refusal::Invalid(frameworks::status_text(api, status)));
        }
    }
    let mut error: CFTypeRef = std::ptr::null();
    // SAFETY: a live trust; the error, if made, is ours.
    let trusted = unsafe { (api.SecTrustEvaluateWithError)(trust.ptr, &mut error) };
    let error = Owned::new(api, error);
    if trusted {
        return Ok(());
    }
    let Some(error) = error else {
        return Err(Refusal::Invalid("not trusted".into()));
    };
    // SAFETY: a live CFError.
    let code = unsafe { (api.CFErrorGetCode)(error.ptr) };
    Err(match code {
        HOST_NAME_MISMATCH => Refusal::NotValidForName,
        CREATE_CHAIN_FAILED => Refusal::UnknownIssuer,
        INVALID_EXTENDED_KEY_USAGE => Refusal::ExtendedKeyUsage,
        CERTIFICATE_REVOKED => Refusal::Revoked,
        _ => {
            // SAFETY: a live CFError; its description is ours.
            let description = Owned::new(api, unsafe { (api.CFErrorCopyDescription)(error.ptr) });
            let said = description
                .map(|d| frameworks::text(api, d.ptr))
                .unwrap_or_default();
            Refusal::Invalid(format!("{said}: {code}"))
        }
    })
}
