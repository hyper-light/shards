//! Keyless signing as cosign v3.1.3 signs (pkg/cosign/bundle sign.go SignData over
//! sigstore-go v1.2.2's sign.Bundle): an ephemeral ECDSA P-256 key signs the DSSE
//! envelope of an in-toto statement; Fulcio certifies the key for the identity token's
//! subject; each timestamp authority the signing config selects stamps the signature;
//! each transparency log it selects logs it; the bundle then verified against the
//! trusted root, as sign.Bundle verifies what it made, before anyone is given it.

use std::time::Duration;

use crate::bundle::TlogEntry;
use crate::image::{ARTIFACT_IN_TOTO, ARTIFACT_SIGSTORE_BUNDLE};
use crate::rekor::proto_string;
use crate::sign::{Signer, pae};
use crate::signing_config::{self, Service, SigningConfig};
use crate::tlog::gocodec;
use crate::tlog::gojson::{self, JValue};
use crate::trusted_root::TrustedRoot;

/// An answer to a request: its status, its `ETag`, and its body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub etag: String,
    pub body: Vec<u8>,
}

/// What keyless signing asks of the network: posts, each with a deadline and at most so
/// many bytes of answer, and waits between attempts.
pub trait Services {
    fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        timeout: Duration,
        max: u64,
    ) -> Result<Answer, String>;

    fn wait(&self, d: Duration);
}

/// What a signature was made with, beside its bundle: the certificate, and the services
/// that took part.
#[derive(Debug, Clone)]
pub struct Signed {
    pub bundle: Vec<u8>,
    /// The leaf certificate Fulcio issued (DER).
    pub certificate: Vec<u8>,
    pub fulcio: String,
    pub timestamps: Vec<String>,
    /// Each log, and the entry it gave.
    pub entries: Vec<(String, TlogEntry)>,
}

/// The retries sigstore-go makes of Fulcio and timestamp authorities (cosign sets one):
/// an answer of 5xx or 429 asked again after 1 s, then 2 s. An error to connect is not.
/// sigstore-go waits after the last attempt too, then reads the closed answer ("http:
/// read on closed response body"); shards gives the last answer itself.
fn retrying(
    services: &dyn Services,
    retries: u32,
    ask: &dyn Fn() -> Result<Answer, String>,
) -> Result<Answer, String> {
    let mut attempt = 0u32;
    loop {
        let answer = ask()?;
        let again = (500..600).contains(&answer.status) || answer.status == 429;
        if !again || attempt >= retries {
            return Ok(answer);
        }
        services.wait(Duration::from_secs(1u64 << attempt.min(16)));
        attempt += 1;
    }
}

/// Fulcio's certificate for `signer`'s key, for the subject of `token`.
fn certificate(services: &dyn Services, base: &str, token: &str, signer: &Signer) -> Result<Vec<u8>, String> {
    let subject = crate::fulcio::subject_of_token(token)?;
    let proof = signer.sign(subject.as_bytes())?;
    let body = crate::fulcio::request(signer.algorithm(), &signer.public_key_pem(), &proof);
    let url = format!("{base}{}", crate::fulcio::SIGNING_CERT_PATH);
    let auth = format!("Bearer {token}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let answer = retrying(services, 1, &|| {
        services.post(&url, &headers, &body, Duration::from_secs(30), 1 << 20)
    })?;
    if answer.status != 200 {
        return Err(format!(
            "Fulcio returned {}: {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        ));
    }
    crate::fulcio::certificate(&answer.body)
}

/// A timestamp authority's stamp of `signature`: its whole answer, as the bundle keeps it.
fn timestamp(services: &dyn Services, url: &str, signature: &[u8]) -> Result<Vec<u8>, String> {
    let body = crate::tsa::request(signature);
    let headers = [("Content-Type", "application/timestamp-query")];
    let answer = retrying(services, 1, &|| {
        services.post(url, &headers, &body, Duration::from_secs(30), 1 << 20)
    })?;
    if answer.status != 200 && answer.status != 201 {
        return Err(format!(
            "timestamp authority returned {}: {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        ));
    }
    crate::tsa::check_response(&answer.body)?;
    Ok(answer.body)
}

/// models.Error as the Rekor client marshals it into its errors: `{"code":…,"message":…}`.
fn rekor_error(body: &[u8]) -> String {
    let decoded = gojson::decode_first(body);
    let Ok(JValue::Object(m)) = &decoded else {
        return "null".into();
    };
    let mut code = None;
    let mut message = None;
    for (i, v) in gojson::struct_members(m, &["code", "message"]) {
        match (i, v) {
            (0, JValue::Number(n)) => code = Some(n.clone()),
            (1, JValue::Str(s)) => message = Some(s.clone()),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if let Some(c) = code.filter(|c| c != "0") {
        parts.push(format!(r#""code":{c}"#));
    }
    if let Some(m) = message.filter(|m| !m.is_empty()) {
        parts.push(format!(r#""message":{}"#, crate::fulcio::go_json_string(&m)));
    }
    format!("{{{}}}", parts.join(","))
}

/// Rekor v1's entry for the envelope, verified by `cert` (PEM): the go-openapi client's
/// request, retried once on an error to connect, a 5xx other than 501, or a 429.
fn entry_v1(services: &dyn Services, base: &str, envelope: &str, cert: &[u8]) -> Result<TlogEntry, String> {
    let body = crate::rekor::proposed_dsse(envelope, cert);
    let url = format!("{base}{}", crate::rekor::V1_ENTRIES_PATH);
    let headers = [
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
    ];
    let ask = || services.post(&url, &headers, &body, Duration::from_secs(90), 32 << 20);
    let answer = match ask() {
        Ok(a) if a.status == 429 || (a.status >= 500 && a.status != 501) => {
            services.wait(Duration::from_secs(1));
            ask()?
        }
        Ok(a) => a,
        Err(_) => {
            services.wait(Duration::from_secs(1));
            ask()?
        }
    };
    let path = format!("[POST {}]", crate::rekor::V1_ENTRIES_PATH);
    match answer.status {
        201 => crate::rekor::entry_v1(&answer.body, &answer.etag),
        400 => Err(format!(
            "{path}[400] createLogEntryBadRequest {}",
            rekor_error(&answer.body)
        )),
        409 => Err(format!(
            "{path}[409] createLogEntryConflict {}",
            rekor_error(&answer.body)
        )),
        s => Err(format!(
            "{path}[{s}] createLogEntry default {}",
            rekor_error(&answer.body)
        )),
    }
}

/// Rekor v2's entry: a hashedrekord of the envelope's PAE digest.
fn entry_v2(
    services: &dyn Services,
    base: &str,
    digest: &[u8],
    signature: &[u8],
    cert: &[u8],
) -> Result<TlogEntry, String> {
    let body = crate::rekor::request_v2(digest, signature, Some(cert), &[], "PKIX_ECDSA_P256_SHA_256");
    let url = format!("{}{}", base.trim_end_matches('/'), crate::rekor::V2_ENTRIES_PATH);
    let headers = [("Content-Type", "application/json")];
    let answer = services
        .post(&url, &headers, &body, Duration::from_secs(30), 10 << 20)
        .map_err(|e| format!("adding rekor v2 entry: getting response: {e}"))?;
    if answer.status != 201 {
        return Err(format!(
            "adding rekor v2 entry: unexpected response: {} {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        ));
    }
    crate::rekor::entry_v2(&answer.body).map_err(|e| format!("adding rekor v2 entry: {e}"))
}

fn bytes(b: &[u8]) -> String {
    format!("\"{}\"", gocodec::std_encode(b))
}

/// The bundle as protojson writes it: v0.3, the leaf certificate, the log entries and
/// timestamps where there are any, the envelope.
pub fn bundle_json(
    cert: &[u8],
    entries: &[TlogEntry],
    stamps: &[Vec<u8>],
    payload: &[u8],
    sig: &[u8],
) -> Vec<u8> {
    let mut material = format!(r#""certificate":{{"rawBytes":{}}}"#, bytes(cert));
    if !entries.is_empty() {
        let list: Vec<String> = entries.iter().map(crate::rekor::entry_json).collect();
        material.push_str(&format!(r#","tlogEntries":[{}]"#, list.join(",")));
    }
    if !stamps.is_empty() {
        let list: Vec<String> = stamps
            .iter()
            .map(|s| format!(r#"{{"signedTimestamp":{}}}"#, bytes(s)))
            .collect();
        material.push_str(&format!(
            r#","timestampVerificationData":{{"rfc3161Timestamps":[{}]}}"#,
            list.join(",")
        ));
    }
    format!(
        r#"{{"mediaType":{},"verificationMaterial":{{{material}}},"dsseEnvelope":{{"payload":{},"payloadType":{},"signatures":[{{"sig":{}}}]}}}}"#,
        proto_string(ARTIFACT_SIGSTORE_BUNDLE),
        bytes(payload),
        proto_string(ARTIFACT_IN_TOTO),
        bytes(sig)
    )
    .into_bytes()
}

/// The services of a kind the config selects, as SignData selects them.
fn selected(
    list: &[Service],
    config: signing_config::ServiceConfiguration,
    versions: &[u32],
    now: (i64, u32),
) -> Result<Vec<Service>, String> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    signing_config::select_services(list, config, versions, now, &mut signing_config::random_below)
}

/// `payload`, an in-toto statement, signed keylessly with the identity `token` names, by
/// the services `config` selects at `now`; verified against `root`, where given, as
/// sign.Bundle verifies (identities and artifact not checked: what was signed is known).
pub fn sign(
    payload: &[u8],
    token: &str,
    config: &SigningConfig,
    root: Option<&TrustedRoot>,
    services: &dyn Services,
    now: (i64, u32),
) -> Result<Signed, String> {
    if config.ca.is_empty() {
        return Err("no fulcio URLs provided in signing config".into());
    }
    let fulcio = signing_config::select_service(&config.ca, crate::fulcio::API_VERSIONS, now)?;
    let tsas = selected(&config.tsa, config.tsa_config, &[1], now)?;
    let logs = selected(
        &config.rekor,
        config.rekor_config,
        crate::rekor::API_VERSIONS,
        now,
    )?;
    if logs.iter().any(|l| l.major_api_version == 2) && tsas.is_empty() {
        return Err(
            "a timestamp authority must be provided to request a short-lived certificate that will be logged to Rekor"
                .into(),
        );
    }
    let signer = Signer::generate_p256()?;
    let message = pae(ARTIFACT_IN_TOTO, payload);
    let signature = signer.sign(&message)?;
    let cert = certificate(services, &fulcio.url, token, &signer)?;
    let cert_pem = crate::tlog::pem::encode("CERTIFICATE", &cert);
    let mut stamps = Vec::new();
    for tsa in &tsas {
        stamps.push(timestamp(services, &tsa.url, &signature)?);
    }
    let envelope = crate::rekor::envelope_json(payload, ARTIFACT_IN_TOTO, &signature);
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &message);
    let mut entries = Vec::new();
    for log in &logs {
        let e = match log.major_api_version {
            2 => entry_v2(services, &log.url, digest.as_ref(), &signature, &cert)?,
            _ => entry_v1(services, &log.url, &envelope, &cert_pem)?,
        };
        entries.push((log.url.clone(), e));
    }
    let list: Vec<TlogEntry> = entries.iter().map(|(_, e)| e.clone()).collect();
    let bundle = bundle_json(&cert, &list, &stamps, payload, &signature);
    if let Some(root) = root {
        verify(&bundle, root, tsas.len(), logs.len(), now)?;
    }
    Ok(Signed {
        bundle,
        certificate: cert,
        fulcio: fulcio.url,
        timestamps: tsas.into_iter().map(|t| t.url).collect(),
        entries,
    })
}

/// sign.Bundle's check of what it made: each timestamp and log entry, the certificate's
/// chain to Fulcio at those times, the signature.
fn verify(
    bundle: &[u8],
    root: &TrustedRoot,
    stamps: usize,
    logs: usize,
    now: (i64, u32),
) -> Result<(), String> {
    use crate::verify::{Config, Identity, Material, Policy};
    let mut config = Config::default();
    if stamps > 0 {
        config.signed = stamps;
    }
    if logs > 0 {
        config.tlog = logs;
        if stamps == 0 {
            config.integrated = logs;
        }
    }
    if stamps == 0 && logs == 0 {
        config.current = Some(crate::time::Time::utc(now.0, now.1));
    }
    let material = Material {
        root,
        key: None,
        fulcio: true,
    };
    let policy = Policy {
        digest: None,
        identity: Identity::Unsafe,
    };
    crate::verify::verify(
        bundle,
        &material,
        &config,
        &policy,
        crate::time::utc,
        crate::time::Time::utc(now.0, now.1),
    )
    .map(|_| ())
    .map_err(|e| e.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use std::cell::RefCell;

    /// Services that answer from a script, and keep what they were asked.
    struct Script {
        answers: RefCell<Vec<Result<Answer, String>>>,
        asked: RefCell<Vec<(String, Vec<u8>)>>,
        waited: RefCell<Vec<Duration>>,
    }

    impl Services for Script {
        fn post(
            &self,
            url: &str,
            _: &[(&str, &str)],
            body: &[u8],
            _: Duration,
            _: u64,
        ) -> Result<Answer, String> {
            self.asked.borrow_mut().push((url.to_string(), body.to_vec()));
            self.answers.borrow_mut().remove(0)
        }

        fn wait(&self, d: Duration) {
            self.waited.borrow_mut().push(d);
        }
    }

    fn answer(status: u16, body: &str) -> Result<Answer, String> {
        Ok(Answer {
            status,
            etag: String::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    /// Fulcio and timestamp authorities are asked again on 5xx and 429, after 1 s and
    /// 2 s, and the last answer is the one given; nothing else is asked again.
    #[test]
    fn fulcio_and_timestamps_are_asked_again_as_sigstore_go_asks() {
        let s = Script {
            answers: RefCell::new(vec![answer(503, "busy"), answer(429, "slow")]),
            asked: RefCell::default(),
            waited: RefCell::default(),
        };
        let got = retrying(&s, 1, &|| s.post("u", &[], b"", Duration::ZERO, 0)).unwrap();
        assert_eq!(got.status, 429);
        assert_eq!(*s.waited.borrow(), [Duration::from_secs(1)]);
        let s = Script {
            answers: RefCell::new(vec![answer(400, "no")]),
            asked: RefCell::default(),
            waited: RefCell::default(),
        };
        assert_eq!(
            retrying(&s, 1, &|| s.post("u", &[], b"", Duration::ZERO, 0))
                .unwrap()
                .status,
            400
        );
        assert!(s.waited.borrow().is_empty());
    }

    #[test]
    fn rekor_s_refusals_read_as_its_client_words_them() {
        assert_eq!(
            rekor_error(br#"{"message":"bad","code":400}"#),
            r#"{"code":400,"message":"bad"}"#
        );
        assert_eq!(rekor_error(b"oops"), "null");
    }

    /// Rekor v2 without a timestamp authority is refused before anything is asked, as
    /// cosign refuses it.
    #[test]
    fn rekor_v2_needs_a_timestamp_authority() {
        let config = SigningConfig {
            ca: vec![Service {
                url: "http://fulcio".into(),
                major_api_version: 1,
                start: None,
                end: None,
                operator: "a".into(),
            }],
            oidc: vec![],
            rekor: vec![Service {
                url: "http://rekor".into(),
                major_api_version: 2,
                start: None,
                end: None,
                operator: "a".into(),
            }],
            rekor_config: signing_config::ServiceConfiguration {
                selector: signing_config::Selector::Any,
                count: 0,
            },
            tsa: vec![],
            tsa_config: signing_config::ServiceConfiguration {
                selector: signing_config::Selector::Any,
                count: 0,
            },
        };
        let s = Script {
            answers: RefCell::new(vec![]),
            asked: RefCell::default(),
            waited: RefCell::default(),
        };
        assert_eq!(
            sign(b"{}", "a.b.c", &config, None, &s, (1_760_000_000, 0)).unwrap_err(),
            "a timestamp authority must be provided to request a short-lived certificate that will be logged to Rekor"
        );
        assert!(s.asked.borrow().is_empty());
        let empty = SigningConfig { ca: vec![], ..config };
        assert_eq!(
            sign(b"{}", "a.b.c", &empty, None, &s, (1_760_000_000, 0)).unwrap_err(),
            "no fulcio URLs provided in signing config"
        );
    }
}
