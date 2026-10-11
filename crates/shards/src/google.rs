//! Google's identity tokens for keyless signing, as cosign v3.1.3's Google providers get
//! them (pkg/providers/google over google.golang.org/api v0.284.0 and
//! cloud.google.com/go/compute/metadata v0.9.0): on Google's machines, from the metadata
//! server; from Application Default Credentials (a service account's key, an impersonated
//! service account, or workload identity federation from a file or URL); and a service
//! account impersonated through IAM's generateIdToken.

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use shards_sigstore::tlog::gojson::{self, JValue};

use crate::keyless::exchange;

/// The metadata server's documented address (metadataIP), unless GCE_METADATA_HOST names
/// another.
const METADATA_IP: &str = "169.254.169.254";
const IAM_CREDENTIALS: &str = "https://iamcredentials.googleapis.com";
const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

fn metadata_host() -> String {
    std::env::var("GCE_METADATA_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| METADATA_IP.into())
}

/// Whether the machine's own description says it is Google's (systemInfoSuggestsGCE).
fn system_suggests_gce() -> bool {
    #[cfg(target_os = "linux")]
    {
        let name = std::fs::read_to_string("/sys/class/dmi/id/product_name").unwrap_or_default();
        matches!(name.trim(), "Google" | "Google Compute Engine")
    }
    #[cfg(windows)]
    {
        windows_product_name().is_some_and(|n| n.trim().starts_with("Google"))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}

/// HKLM\SYSTEM\HardwareConfig\Current's SystemProductName.
#[cfg(windows)]
fn windows_product_name() -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let key = wide("SYSTEM\\HardwareConfig\\Current");
    let value = wide("SystemProductName");
    let mut buf = vec![0u16; 256];
    let mut len = u32::try_from(buf.len() * 2).ok()?;
    // SAFETY: RegGetValueW writes at most `len` bytes into `buf` and the bytes it wrote
    // into `len`; the key and value names are NUL-terminated.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    let chars = usize::try_from(len).ok()? / 2;
    let s = String::from_utf16_lossy(buf.get(..chars)?);
    Some(s.trim_end_matches('\0').to_string())
}

/// The metadata server answers as Google's: a GET of its root, its Metadata-Flavor
/// Google (metadataRequestStrategy).
fn metadata_answers() -> bool {
    let Ok(mut addrs) = format!("{METADATA_IP}:80").to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut conn) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {METADATA_IP}\r\nUser-Agent: gcloud-golang/0.1\r\nConnection: close\r\n\r\n"
    );
    if conn.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = Vec::new();
    let _ = conn.take(64 << 10).read_to_end(&mut head);
    let head = String::from_utf8_lossy(&head);
    let head = head.split("\r\n\r\n").next().unwrap_or_default();
    head.lines().skip(1).any(|l| {
        l.split_once(':')
            .is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case("metadata-flavor") && v.trim() == "Google")
    })
}

/// `metadata.google.internal.` resolves to the metadata server (dnsRequestStrategy).
fn metadata_resolves() -> bool {
    "metadata.google.internal.:80"
        .to_socket_addrs()
        .is_ok_and(|mut a| a.any(|s| s.ip().to_string() == METADATA_IP))
}

/// metadata.OnGCE: GCE_METADATA_HOST set, or the metadata server answering or resolving,
/// whichever probe answers first; where the machine says it is Google's, a probe saying
/// no is given 5 s for the other to say yes. Asked once.
pub fn on_gce() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if std::env::var_os("GCE_METADATA_HOST").is_some_and(|h| !h.is_empty()) {
            return true;
        }
        let (tx, rx) = mpsc::channel();
        let probes: [fn() -> bool; 2] = [metadata_answers, metadata_resolves];
        for probe in probes {
            let tx = tx.clone();
            // Detached: the answer that comes second is not waited for.
            let _ = std::thread::Builder::new()
                .name("gce-probe".into())
                .spawn(move || {
                    let _ = tx.send(probe());
                });
        }
        drop(tx);
        let first = rx.recv().unwrap_or(false);
        if first || !system_suggests_gce() {
            return first;
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    })
}

/// metadata.Get of `suffix`: its value, or the server's refusal in the client's words.
fn metadata_get(suffix: &str) -> Result<String, String> {
    let url = format!(
        "http://{}/computeMetadata/v1/{}",
        metadata_host(),
        suffix.trim_start_matches('/')
    );
    let mut attempt = 0;
    loop {
        let got = exchange(
            "GET",
            &url,
            None,
            &[("Metadata-Flavor", "Google"), ("User-Agent", "gcloud-golang/0.1")],
            &[],
            Duration::from_secs(5),
            1 << 20,
        );
        // The client's retryer: transport failures and 5xx, backing off from 100 ms.
        let again = match &got {
            Ok(a) => a.status >= 500,
            Err(_) => true,
        };
        if again && attempt < 4 {
            std::thread::sleep(Duration::from_millis(100 << attempt));
            attempt += 1;
            continue;
        }
        let a = got?;
        return match a.status {
            200 => Ok(String::from_utf8_lossy(&a.body).into_owned()),
            404 => Err(format!(
                "metadata: GCE metadata {} not defined",
                shards_sigstore::oidc::go_quote(suffix)
            )),
            code => Err(format!(
                "metadata: GCE metadata {} returned status code {code}: {}",
                shards_sigstore::oidc::go_quote(suffix),
                String::from_utf8_lossy(&a.body)
            )),
        };
    }
}

/// Application Default Credentials' JSON: GOOGLE_APPLICATION_CREDENTIALS's file, else
/// gcloud's well-known file, if either is there.
fn adc_json() -> Result<Option<Vec<u8>>, String> {
    if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
        return std::fs::read(&path)
            .map(Some)
            .map_err(|e| format!("google: error getting credentials using GOOGLE_APPLICATION_CREDENTIALS environment variable: {e}"));
    }
    let dir = match std::env::var_os("CLOUDSDK_CONFIG").filter(|p| !p.is_empty()) {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            #[cfg(windows)]
            let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
            #[cfg(not(windows))]
            let base = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"));
            match base {
                Some(b) => b.join("gcloud"),
                None => return Ok(None),
            }
        }
    };
    match std::fs::read(dir.join("application_default_credentials.json")) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "google: error getting credentials using well-known file: {e}"
        )),
    }
}

fn text(v: &JValue, k: &str) -> String {
    v.get(k).and_then(JValue::as_str).unwrap_or_default().to_string()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A JWT signed RS256 with a service account's key (its PKCS #8 PEM), its key ID the
/// account's.
fn signed_jwt(account: &JValue, claims: &str) -> Result<String, String> {
    let pem = text(account, "private_key");
    let (block, _) =
        shards_sigstore::tlog::pem::decode(pem.as_bytes()).ok_or("google: private key is not PEM")?;
    let key = aws_lc_rs::signature::RsaKeyPair::from_pkcs8(&block.bytes)
        .map_err(|e| format!("google: private key: {e}"))?;
    let header = format!(
        r#"{{"alg":"RS256","typ":"JWT","kid":{}}}"#,
        shards_sigstore::fulcio::go_json_string(&text(account, "private_key_id"))
    );
    let input = format!(
        "{}.{}",
        shards_sigstore::oidc::base64url(header.as_bytes()),
        shards_sigstore::oidc::base64url(claims.as_bytes())
    );
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(
        &aws_lc_rs::signature::RSA_PKCS1_SHA256,
        &aws_lc_rs::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| "google: signing the assertion failed")?;
    Ok(format!("{input}.{}", shards_sigstore::oidc::base64url(&sig)))
}

/// A form posted to a token endpoint, and the field it answers with.
fn token_from(url: &str, form: &[(&str, &str)], field: &str) -> Result<String, String> {
    let a = exchange(
        "POST",
        url,
        None,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        shards_sigstore::oidc::form(form).as_bytes(),
        Duration::from_secs(30),
        1 << 20,
    )?;
    let v = gojson::unmarshal(&a.body).map_err(|e| format!("oauth2: cannot parse json: {e}"))?;
    if !(200..300).contains(&a.status) {
        return Err(format!(
            "oauth2: cannot fetch token: {}\nResponse: {}",
            a.status,
            String::from_utf8_lossy(&a.body)
        ));
    }
    let t = text(&v, field);
    if t.is_empty() {
        return Err(format!("oauth2: server response missing {field}"));
    }
    Ok(t)
}

/// A service account key's assertion exchanged at its token URI: an ID token for
/// `audience` (target_audience), or an access token for cloud-platform.
fn service_account(account: &JValue, audience: Option<&str>) -> Result<String, String> {
    let token_uri = Some(text(account, "token_uri"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
    let iat = now_secs();
    let q = shards_sigstore::fulcio::go_json_string;
    let claims = match audience {
        Some(aud) => format!(
            r#"{{"iss":{},"aud":{},"exp":{},"iat":{iat},"target_audience":{}}}"#,
            q(&text(account, "client_email")),
            q(&token_uri),
            iat + 3600,
            q(aud)
        ),
        None => format!(
            r#"{{"iss":{},"scope":{},"aud":{},"exp":{},"iat":{iat}}}"#,
            q(&text(account, "client_email")),
            q(CLOUD_PLATFORM),
            q(&token_uri),
            iat + 3600
        ),
    };
    let assertion = signed_jwt(account, &claims)?;
    let field = if audience.is_some() {
        "id_token"
    } else {
        "access_token"
    };
    token_from(
        &token_uri,
        &[("grant_type", JWT_BEARER), ("assertion", &assertion)],
        field,
    )
}

/// The service account an IAM impersonation URL names (`…/serviceAccounts/SA:generate…`).
fn impersonated_account(url: &str) -> Result<String, String> {
    let rest = url
        .rsplit_once("/serviceAccounts/")
        .map(|(_, r)| r)
        .ok_or_else(|| format!("google: no service account in {url}"))?;
    Ok(rest.split(':').next().unwrap_or(rest).to_string())
}

/// IAM's generateAccessToken for `account`, as `bearer`.
fn generate_access_token(account: &str, bearer: &str, delegates: &[String]) -> Result<String, String> {
    let url = format!("{IAM_CREDENTIALS}/v1/projects/-/serviceAccounts/{account}:generateAccessToken");
    iam(
        &url,
        bearer,
        &format!(
            r#""scope":[{}]"#,
            shards_sigstore::fulcio::go_json_string(CLOUD_PLATFORM)
        ),
        delegates,
        "accessToken",
    )
}

/// IAM's generateIdToken for `account` and `audience`, as `bearer` (the impersonate
/// package's request: the audience, the email included).
fn generate_id_token(
    account: &str,
    audience: &str,
    bearer: &str,
    delegates: &[String],
) -> Result<String, String> {
    let url = format!("{IAM_CREDENTIALS}/v1/projects/-/serviceAccounts/{account}:generateIdToken");
    let body = format!(
        r#""audience":{},"includeEmail":true"#,
        shards_sigstore::fulcio::go_json_string(audience)
    );
    iam(&url, bearer, &body, delegates, "token")
}

fn iam(url: &str, bearer: &str, fields: &str, delegates: &[String], answer: &str) -> Result<String, String> {
    let mut body = format!("{{{fields}");
    if !delegates.is_empty() {
        let list: Vec<String> = delegates
            .iter()
            .map(|d| shards_sigstore::fulcio::go_json_string(&format!("projects/-/serviceAccounts/{d}")))
            .collect();
        body.push_str(&format!(r#","delegates":[{}]"#, list.join(",")));
    }
    body.push('}');
    let auth = format!("Bearer {bearer}");
    let a = exchange(
        "POST",
        url,
        Some(&auth),
        &[("Content-Type", "application/json")],
        body.as_bytes(),
        Duration::from_secs(30),
        1 << 20,
    )
    .map_err(|e| format!("impersonate: unable to generate ID token: {e}"))?;
    if !(200..300).contains(&a.status) {
        return Err(format!(
            "impersonate: status code {}: {}",
            a.status,
            String::from_utf8_lossy(&a.body)
        ));
    }
    let v = gojson::unmarshal(&a.body).map_err(|e| format!("impersonate: unable to parse response: {e}"))?;
    Ok(text(&v, answer))
}

fn delegates(v: &JValue) -> Vec<String> {
    match v.get("delegates") {
        Some(JValue::Array(list)) => list
            .iter()
            .filter_map(|d| d.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Workload identity federation's subject token: its credential source's file or URL,
/// as text or as a JSON field.
fn subject_token(source: &JValue) -> Result<String, String> {
    let raw = if let Some(file) = source.get("file").and_then(JValue::as_str) {
        std::fs::read(file).map_err(|e| {
            format!("oauth2/google/externalaccount: failed to load credentials from file: {e}")
        })?
    } else if let Some(url) = source.get("url").and_then(JValue::as_str) {
        let headers: Vec<(String, String)> = match source.get("headers") {
            Some(JValue::Object(h)) => h
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect(),
            _ => Vec::new(),
        };
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let a = exchange("GET", url, None, &refs, &[], Duration::from_secs(30), 1 << 20)?;
        if !(200..300).contains(&a.status) {
            return Err(format!(
                "oauth2/google/externalaccount: status code {}: {}",
                a.status,
                String::from_utf8_lossy(&a.body)
            ));
        }
        a.body
    } else if source.get("executable").is_some() || source.get("environment_id").is_some() {
        return Err(
            "workload identity federation from an executable or AWS: give the token itself with --identity-token"
                .into(),
        );
    } else {
        return Err("oauth2/google/externalaccount: unable to determine credential source".into());
    };
    let format = source.get("format");
    match format.and_then(|f| f.get("type")).and_then(JValue::as_str) {
        Some("json") => {
            let field = format
                .and_then(|f| f.get("subject_token_field_name"))
                .and_then(JValue::as_str)
                .unwrap_or_default();
            let v = gojson::unmarshal(&raw).map_err(|e| {
                format!("oauth2/google/externalaccount: failed to unmarshal subject token file: {e}")
            })?;
            v.get(field)
                .and_then(JValue::as_str)
                .map(str::to_string)
                .ok_or_else(|| "oauth2/google/externalaccount: provided subject_token_field_name not found in credentials".into())
        }
        _ => Ok(String::from_utf8_lossy(&raw).trim().to_string()),
    }
}

/// Workload identity federation's access token: the subject token exchanged at Google's
/// STS.
fn external_access_token(account: &JValue) -> Result<String, String> {
    let source = account
        .get("credential_source")
        .ok_or("oauth2/google/externalaccount: missing credential_source")?;
    let subject = subject_token(source)?;
    let token_url = Some(text(account, "token_url"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://sts.googleapis.com/v1/token".into());
    token_from(
        &token_url,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("audience", &text(account, "audience")),
            ("scope", CLOUD_PLATFORM),
            (
                "requested_token_type",
                "urn:ietf:params:oauth:token-type:access_token",
            ),
            ("subject_token", &subject),
            ("subject_token_type", &text(account, "subject_token_type")),
        ],
        "access_token",
    )
}

/// An access token from credentials of any type ADC knows.
fn access_token(account: &JValue) -> Result<String, String> {
    match text(account, "type").as_str() {
        "service_account" => service_account(account, None),
        "authorized_user" => token_from(
            &Some(text(account, "token_uri"))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into()),
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &text(account, "client_id")),
                ("client_secret", &text(account, "client_secret")),
                ("refresh_token", &text(account, "refresh_token")),
            ],
            "access_token",
        ),
        "impersonated_service_account" => {
            let source = account
                .get("source_credentials")
                .ok_or("google: missing source_credentials")?;
            let bearer = access_token(source)?;
            let target = impersonated_account(&text(account, "service_account_impersonation_url"))?;
            generate_access_token(&target, &bearer, &delegates(account))
        }
        "external_account" => {
            let token = external_access_token(account)?;
            match text(account, "service_account_impersonation_url").as_str() {
                "" => Ok(token),
                url => generate_access_token(&impersonated_account(url)?, &token, &[]),
            }
        }
        other => Err(format!(
            "google: unknown credential type: {}",
            shards_sigstore::oidc::go_quote(other)
        )),
    }
}

/// The default credentials' access token: their JSON's, else the metadata server's.
fn default_access_token() -> Result<String, String> {
    if let Some(json) = adc_json()? {
        let v = gojson::unmarshal(&json).map_err(|e| format!("google: {e}"))?;
        return access_token(&v);
    }
    if on_gce() {
        let body = metadata_get("instance/service-accounts/default/token")?;
        let v = gojson::unmarshal(body.as_bytes()).map_err(|e| format!("google: {e}"))?;
        return Ok(text(&v, "access_token"));
    }
    Err("google: could not find default credentials. See https://cloud.google.com/docs/authentication/external/set-up-adc for more information".into())
}

/// The google-workload-identity provider: idtoken.NewTokenSource's token for `audience`,
/// from the default credentials' JSON (a service account, an impersonated one, or
/// federation through one) or, on Google's machines, the metadata server's identity.
pub fn workload_identity(audience: &str) -> Result<String, String> {
    if audience.is_empty() {
        return Err("idtoken: must supply a non-empty audience".into());
    }
    if let Some(json) = adc_json()? {
        let v = gojson::unmarshal(&json).map_err(|e| format!("idtoken: {e}"))?;
        return match text(&v, "type").as_str() {
            "service_account" => service_account(&v, Some(audience)),
            "impersonated_service_account" => {
                let source = v
                    .get("source_credentials")
                    .ok_or("idtoken: missing source_credentials")?;
                let bearer = access_token(source)?;
                let target = impersonated_account(&text(&v, "service_account_impersonation_url"))?;
                generate_id_token(&target, audience, &bearer, &delegates(&v))
            }
            "external_account" => {
                let url = text(&v, "service_account_impersonation_url");
                if url.is_empty() {
                    return Err("idtoken: unsupported credentials type".into());
                }
                let token = external_access_token(&v)?;
                generate_id_token(&impersonated_account(&url)?, audience, &token, &[])
            }
            other => Err(format!(
                "idtoken: credential type {} not supported",
                shards_sigstore::oidc::go_quote(other)
            )),
        };
    }
    if on_gce() {
        let query = shards_sigstore::oidc::form(&[("audience", audience), ("format", "full")]);
        let token = metadata_get(&format!("instance/service-accounts/default/identity?{query}"))?;
        if token.is_empty() {
            return Err("idtoken: invalid response from metadata service".into());
        }
        return Ok(token);
    }
    Err("idtoken: couldn't find any credentials".into())
}

/// The google-impersonate provider: `account`'s ID token for `audience`, IAM's
/// generateIdToken as the default credentials.
pub fn impersonate(account: &str, audience: &str) -> Result<String, String> {
    if audience.is_empty() {
        return Err("impersonate: an audience must be provided".into());
    }
    if account.is_empty() {
        return Err("impersonate: a target service account must be provided".into());
    }
    let bearer = default_access_token()?;
    generate_id_token(account, audience, &bearer, &[])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn impersonation_urls_name_their_account() {
        assert_eq!(
            impersonated_account(
                "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/signer@p.iam.gserviceaccount.com:generateAccessToken"
            )
            .unwrap(),
            "signer@p.iam.gserviceaccount.com"
        );
        assert!(impersonated_account("https://example.com/").is_err());
    }

    /// A federated subject token from a file, as text or as a JSON field.
    #[test]
    fn subject_tokens_come_from_their_source() {
        let dir = shards_testdir::TempDir::new("gsubj").unwrap();
        let plain = dir.join("plain");
        std::fs::write(&plain, "tok\n").unwrap();
        let json = dir.join("json");
        std::fs::write(&json, r#"{"value":"jtok"}"#).unwrap();
        let src = |s: String| gojson::unmarshal(s.as_bytes()).unwrap();
        let q = |p: &std::path::Path| shards_sigstore::fulcio::go_json_string(&p.display().to_string());
        assert_eq!(
            subject_token(&src(format!(r#"{{"file":{}}}"#, q(&plain)))).unwrap(),
            "tok"
        );
        assert_eq!(
            subject_token(&src(format!(
                r#"{{"file":{},"format":{{"type":"json","subject_token_field_name":"value"}}}}"#,
                q(&json)
            )))
            .unwrap(),
            "jtok"
        );
        assert!(subject_token(&src(r#"{"executable":{"command":"x"}}"#.into())).is_err());
    }
}
