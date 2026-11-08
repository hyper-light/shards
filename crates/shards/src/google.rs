//! Google's tokens, as Google's Go libraries get them (google.golang.org/api v0.284.0,
//! golang.org/x/oauth2/google, cloud.google.com/go/compute/metadata v0.9.0): on Google's
//! machines, from the metadata server; from Application Default Credentials (a service
//! account's key, a user's refresh token, an impersonated service account, or workload
//! identity federation from a file or URL). Identity tokens for keyless signing, as
//! cosign v3.1.3's Google providers get them (and a service account impersonated through
//! IAM's generateIdToken); access tokens for a scope, and when they expire, for Cloud
//! Logging (D169).
//!
//! Every lookup is in an environment given ([`Env`]): the process's for keyless signing; a
//! run's client's, for a log the daemon sends (D169).

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use shards_registry::http::{Client, Patience, Redirects, Request};
use shards_registry::url::Url;
use shards_sigstore::keyless::Answer;
use shards_sigstore::tlog::gojson::{self, JValue};

/// An environment's variables, by name.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// This process's environment.
pub fn own_env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn var(env: Env<'_>, k: &str) -> Option<String> {
    env(k).filter(|v| !v.is_empty())
}

/// An access token, and when it expires, where its issuer says.
pub struct Token {
    pub value: zeroize::Zeroizing<String>,
    // Read by the daemon's log shipper, which is Unix's alone as yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub expires: Option<SystemTime>,
}

impl Token {
    fn of(value: String, lasts: Option<u64>) -> Token {
        Token {
            value: zeroize::Zeroizing::new(value),
            expires: lasts.and_then(|s| SystemTime::now().checked_add(Duration::from_secs(s))),
        }
    }

    /// Whether it has expired, or will within `window`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn expiring(&self, window: Duration) -> bool {
        self.expires
            .is_some_and(|t| SystemTime::now().checked_add(window).is_none_or(|soon| t <= soon))
    }
}

/// An HTTP exchange in `env`: through its proxies, unless `direct`, as the metadata server
/// is reached (Google's metadata client has no proxy, so its tokens reach no proxy); the
/// answer's status, ETag and body (at most `max` bytes). The credentials in
/// `authorization` go to `url`'s origin alone; a redirect elsewhere is refused.
#[allow(clippy::too_many_arguments)]
fn exchange(
    env: Env<'_>,
    direct: bool,
    method: &str,
    url: &str,
    authorization: Option<&str>,
    headers: &[(&str, &str)],
    body: &[u8],
    wait: Duration,
    max: u64,
) -> Result<Answer, String> {
    let parsed = Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
    let mut client = Client::new(
        Box::new(move |_| Ok(config.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    )
    .with_patience(Patience {
        head: Some(wait),
        stall: Some(wait),
        ..Patience::default()
    });
    if !direct {
        client = client.with_proxies(shards_registry::proxy::Proxies::from_env(env));
    }
    let request = Request {
        method,
        url: &parsed,
        headers,
        body,
        file: None,
    };
    let auth = authorization.map(str::to_string);
    let origin = parsed.clone();
    let mut response = client
        .follow(
            &request,
            &|u| Ok(auth.clone().filter(|_| u.same_origin(&origin))),
            Redirects::SameOrigin,
        )
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    (&mut response)
        .take(max)
        .read_to_end(&mut out)
        .map_err(|e| format!("{method} {url}: {e}"))?;
    Ok(Answer {
        status: response.status,
        etag: response.header("etag").unwrap_or_default().to_string(),
        body: out,
    })
}

/// The metadata server's documented address (metadataIP), unless GCE_METADATA_HOST names
/// another.
const METADATA_IP: &str = "169.254.169.254";
const IAM_CREDENTIALS: &str = "https://iamcredentials.googleapis.com";
const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

fn metadata_host(env: Env<'_>) -> String {
    var(env, "GCE_METADATA_HOST").unwrap_or_else(|| METADATA_IP.into())
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

/// metadata.OnGCE: GCE_METADATA_HOST set in `env`, or the metadata server answering or
/// resolving, whichever probe answers first; where the machine says it is Google's, a
/// probe saying no is given 5 s for the other to say yes. The machine is asked once.
pub fn on_gce(env: Env<'_>) -> bool {
    if var(env, "GCE_METADATA_HOST").is_some() {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
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
pub fn metadata_get(env: Env<'_>, suffix: &str) -> Result<String, String> {
    let url = format!(
        "http://{}/computeMetadata/v1/{}",
        metadata_host(env),
        suffix.trim_start_matches('/')
    );
    let mut attempt = 0;
    loop {
        let got = exchange(
            env,
            true,
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
fn adc_json(env: Env<'_>) -> Result<Option<Vec<u8>>, String> {
    if let Some(path) = var(env, "GOOGLE_APPLICATION_CREDENTIALS") {
        return std::fs::read(&path)
            .map(Some)
            .map_err(|e| format!("google: error getting credentials using GOOGLE_APPLICATION_CREDENTIALS environment variable: {e}"));
    }
    let dir = match var(env, "CLOUDSDK_CONFIG") {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            #[cfg(windows)]
            let base = var(env, "APPDATA").map(std::path::PathBuf::from);
            #[cfg(not(windows))]
            let base = var(env, "HOME").map(|h| std::path::PathBuf::from(h).join(".config"));
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

/// How long a token lasts, as an answer says it: `expires_in`'s seconds.
fn lasts(v: &JValue) -> Option<u64> {
    match v.get("expires_in") {
        Some(JValue::Number(n)) => n.split('.').next().and_then(|s| s.parse().ok()),
        Some(JValue::Str(n)) => n.parse().ok(),
        _ => None,
    }
}

/// A form posted to a token endpoint: the field it answers with, and how long it lasts.
fn token_from(
    env: Env<'_>,
    url: &str,
    form: &[(&str, &str)],
    field: &str,
) -> Result<(String, Option<u64>), String> {
    let a = exchange(
        env,
        false,
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
    Ok((t, lasts(&v)))
}

/// A service account key's assertion exchanged at its token URI: an ID token for
/// `audience` (target_audience), or an access token for `scope`.
fn service_account(
    env: Env<'_>,
    account: &JValue,
    audience: Option<&str>,
    scope: &str,
) -> Result<(String, Option<u64>), String> {
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
            q(scope),
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
        env,
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

/// IAM's generateAccessToken for `account` and `scope`, as `bearer`: the token, and its
/// `expireTime`.
fn generate_access_token(
    env: Env<'_>,
    account: &str,
    bearer: &str,
    delegates: &[String],
    scope: &str,
) -> Result<Token, String> {
    let url = format!("{IAM_CREDENTIALS}/v1/projects/-/serviceAccounts/{account}:generateAccessToken");
    let v = iam(
        env,
        &url,
        bearer,
        &format!(r#""scope":[{}]"#, shards_sigstore::fulcio::go_json_string(scope)),
        delegates,
    )?;
    let expires = v.get("expireTime").and_then(JValue::as_str).and_then(|t| {
        let t = time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()?;
        let secs = u64::try_from(t.unix_timestamp()).ok()?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    });
    Ok(Token {
        value: zeroize::Zeroizing::new(text(&v, "accessToken")),
        expires,
    })
}

/// IAM's generateIdToken for `account` and `audience`, as `bearer` (the impersonate
/// package's request: the audience, the email included).
fn generate_id_token(
    env: Env<'_>,
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
    Ok(text(&iam(env, &url, bearer, &body, delegates)?, "token"))
}

fn iam(env: Env<'_>, url: &str, bearer: &str, fields: &str, delegates: &[String]) -> Result<JValue, String> {
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
        env,
        false,
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
    gojson::unmarshal(&a.body).map_err(|e| format!("impersonate: unable to parse response: {e}"))
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
fn subject_token(env: Env<'_>, source: &JValue) -> Result<String, String> {
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
        let a = exchange(
            env,
            false,
            "GET",
            url,
            None,
            &refs,
            &[],
            Duration::from_secs(30),
            1 << 20,
        )?;
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

/// Workload identity federation's access token for `scope`: the subject token exchanged
/// at Google's STS.
fn external_access_token(env: Env<'_>, account: &JValue, scope: &str) -> Result<Token, String> {
    let source = account
        .get("credential_source")
        .ok_or("oauth2/google/externalaccount: missing credential_source")?;
    let subject = subject_token(env, source)?;
    let token_url = Some(text(account, "token_url"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://sts.googleapis.com/v1/token".into());
    let (value, lasts) = token_from(
        env,
        &token_url,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("audience", &text(account, "audience")),
            ("scope", scope),
            (
                "requested_token_type",
                "urn:ietf:params:oauth:token-type:access_token",
            ),
            ("subject_token", &subject),
            ("subject_token_type", &text(account, "subject_token_type")),
        ],
        "access_token",
    )?;
    Ok(Token::of(value, lasts))
}

/// An access token for `scope` from credentials of any type ADC knows.
fn access_token(env: Env<'_>, account: &JValue, scope: &str) -> Result<Token, String> {
    match text(account, "type").as_str() {
        "service_account" => {
            let (value, lasts) = service_account(env, account, None, scope)?;
            Ok(Token::of(value, lasts))
        }
        "authorized_user" => {
            let (value, lasts) = token_from(
                env,
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
            )?;
            Ok(Token::of(value, lasts))
        }
        "impersonated_service_account" => {
            let source = account
                .get("source_credentials")
                .ok_or("google: missing source_credentials")?;
            let bearer = access_token(env, source, CLOUD_PLATFORM)?;
            let target = impersonated_account(&text(account, "service_account_impersonation_url"))?;
            generate_access_token(env, &target, &bearer.value, &delegates(account), scope)
        }
        "external_account" => match text(account, "service_account_impersonation_url").as_str() {
            "" => external_access_token(env, account, scope),
            url => {
                let token = external_access_token(env, account, CLOUD_PLATFORM)?;
                generate_access_token(env, &impersonated_account(url)?, &token.value, &[], scope)
            }
        },
        other => Err(format!(
            "google: unknown credential type: {}",
            shards_sigstore::oidc::go_quote(other)
        )),
    }
}

/// The default credentials' access token for `scope` (FindDefaultCredentials): their
/// JSON's, else the metadata server's, asked for that scope.
pub fn access(env: Env<'_>, scope: &str) -> Result<Token, String> {
    if let Some(json) = adc_json(env)? {
        let v = gojson::unmarshal(&json).map_err(|e| format!("google: {e}"))?;
        return access_token(env, &v, scope);
    }
    if on_gce(env) {
        let query = shards_sigstore::oidc::form(&[("scopes", scope)]);
        let body = metadata_get(env, &format!("instance/service-accounts/default/token?{query}"))?;
        let v = gojson::unmarshal(body.as_bytes()).map_err(|e| format!("google: {e}"))?;
        return Ok(Token::of(text(&v, "access_token"), lasts(&v)));
    }
    Err("google: could not find default credentials. See https://cloud.google.com/docs/authentication/external/set-up-adc for more information".into())
}

/// The project the default credentials' JSON names (`project_id`, a service account's), as
/// FindDefaultCredentials gives it.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn credentials_project(env: Env<'_>) -> Option<String> {
    let json = adc_json(env).ok().flatten()?;
    let v = gojson::unmarshal(&json).ok()?;
    Some(text(&v, "project_id")).filter(|p| !p.is_empty())
}

/// The project a request is billed and counted to, where not the resource's:
/// GOOGLE_CLOUD_QUOTA_PROJECT, else the credentials' `quota_project_id` (a user's), sent as
/// `x-goog-user-project`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn quota_project(env: Env<'_>) -> Option<String> {
    var(env, "GOOGLE_CLOUD_QUOTA_PROJECT").or_else(|| {
        let json = adc_json(env).ok().flatten()?;
        let v = gojson::unmarshal(&json).ok()?;
        Some(text(&v, "quota_project_id")).filter(|p| !p.is_empty())
    })
}

/// Whether Google's credentials, projects or metadata server are found by variable
/// `name`: a run's client gives these with the run (D169).
pub fn reads(name: &str) -> bool {
    name.starts_with("GOOGLE_")
        || name.starts_with("CLOUDSDK_")
        || name.starts_with("GCE_METADATA_")
        || name == "APPDATA"
}

/// The google-workload-identity provider: idtoken.NewTokenSource's token for `audience`,
/// from the default credentials' JSON (a service account, an impersonated one, or
/// federation through one) or, on Google's machines, the metadata server's identity.
pub fn workload_identity(audience: &str) -> Result<String, String> {
    let env: Env<'_> = &own_env;
    if audience.is_empty() {
        return Err("idtoken: must supply a non-empty audience".into());
    }
    if let Some(json) = adc_json(env)? {
        let v = gojson::unmarshal(&json).map_err(|e| format!("idtoken: {e}"))?;
        return match text(&v, "type").as_str() {
            "service_account" => service_account(env, &v, Some(audience), CLOUD_PLATFORM).map(|(t, _)| t),
            "impersonated_service_account" => {
                let source = v
                    .get("source_credentials")
                    .ok_or("idtoken: missing source_credentials")?;
                let bearer = access_token(env, source, CLOUD_PLATFORM)?;
                let target = impersonated_account(&text(&v, "service_account_impersonation_url"))?;
                generate_id_token(env, &target, audience, &bearer.value, &delegates(&v))
            }
            "external_account" => {
                let url = text(&v, "service_account_impersonation_url");
                if url.is_empty() {
                    return Err("idtoken: unsupported credentials type".into());
                }
                let token = external_access_token(env, &v, CLOUD_PLATFORM)?;
                generate_id_token(env, &impersonated_account(&url)?, audience, &token.value, &[])
            }
            other => Err(format!(
                "idtoken: credential type {} not supported",
                shards_sigstore::oidc::go_quote(other)
            )),
        };
    }
    if on_gce(env) {
        let query = shards_sigstore::oidc::form(&[("audience", audience), ("format", "full")]);
        let token = metadata_get(
            env,
            &format!("instance/service-accounts/default/identity?{query}"),
        )?;
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
    let env: Env<'_> = &own_env;
    if audience.is_empty() {
        return Err("impersonate: an audience must be provided".into());
    }
    if account.is_empty() {
        return Err("impersonate: a target service account must be provided".into());
    }
    let bearer = access(env, CLOUD_PLATFORM)?;
    generate_id_token(env, account, audience, &bearer.value, &[])
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
            subject_token(&own_env, &src(format!(r#"{{"file":{}}}"#, q(&plain)))).unwrap(),
            "tok"
        );
        assert_eq!(
            subject_token(
                &own_env,
                &src(format!(
                    r#"{{"file":{},"format":{{"type":"json","subject_token_field_name":"value"}}}}"#,
                    q(&json)
                ))
            )
            .unwrap(),
            "jtok"
        );
        assert!(subject_token(&own_env, &src(r#"{"executable":{"command":"x"}}"#.into())).is_err());
    }
}
