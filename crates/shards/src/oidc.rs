//! Signing in for an identity token, as cosign v3.1.3 does where it is given none and the
//! environment has none (sigstore v1.10.8 pkg/oauthflow): in a browser, the code it
//! brings back to a listener on this host (PKCE); by a device code, typed in elsewhere;
//! or by the client's own credentials. Each token is checked as go-oidc checks it before
//! it is used. Messages go to stderr, which shards keeps for what it says to people.

use std::io::{self, BufRead as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use shards_sigstore::oidc::{self, Provider};

use crate::keyless::exchange;

/// How long the browser has to bring the code back (getCode's timer).
const BROWSER_WAIT: Duration = Duration::from_secs(120);

/// The redirect for a code typed in by hand (oobRedirectURI).
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";

/// The page the browser shows once it has brought the code back.
const SIGNED_IN: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>shards: signed in</title><style>body{font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem;padding:0 1rem;color:#1b1f24;background:#fff}@media (prefers-color-scheme:dark){body{color:#e6e6e6;background:#111}}</style></head><body><h1>Signed in</h1><p>shards has your identity token and is signing. You can close this tab.</p></body></html>";

fn get(url: &str, wait: Duration) -> Result<(u16, Vec<u8>), String> {
    let a = exchange("GET", url, None, &[], &[], wait, 1 << 20)?;
    Ok((a.status, a.body))
}

fn post_form(url: &str, authorization: Option<&str>, body: &str) -> Result<(u16, Vec<u8>), String> {
    let a = exchange(
        "POST",
        url,
        authorization,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body.as_bytes(),
        Duration::from_secs(30),
        1 << 20,
    )?;
    Ok((a.status, a.body))
}

/// Go's `resp.Status`: the code and its reason.
fn status_text(code: u16) -> String {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    };
    if reason.is_empty() {
        code.to_string()
    } else {
        format!("{code} {reason}")
    }
}

/// The provider at `issuer` (oidc.NewProvider), from its discovery document.
fn discover(issuer: &str, wait: Duration) -> Result<Provider, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let (status, body) = get(&url, wait)?;
    if status != 200 {
        return Err(format!(
            "{}: {}",
            status_text(status),
            String::from_utf8_lossy(&body)
        ));
    }
    oidc::provider(&body, issuer)
}

/// The ID token `raw` verified as the provider's verifier verifies it: its signature by
/// one of the provider's keys (that of its key ID, if it names one), by an algorithm the
/// provider signs with (RS256 where it says none), then its claims.
fn verify(p: &Provider, client_id: &str, raw: &str) -> Result<oidc::IdToken, String> {
    let algs: Vec<&str> = if p.algorithms.is_empty() {
        vec!["RS256"]
    } else {
        p.algorithms.iter().map(String::as_str).collect()
    };
    let jws = oidc::parse_jws(raw, &algs).map_err(|e| format!("oidc: malformed jwt: {e}"))?;
    let (status, body) = get(&p.jwks_uri, Duration::from_secs(30))
        .map_err(|e| format!("fetching keys oidc: get keys failed {e}"))?;
    if status != 200 {
        return Err(format!(
            "fetching keys oidc: get keys failed: {} {}",
            status_text(status),
            String::from_utf8_lossy(&body)
        ));
    }
    let keys = oidc::parse_jwks(&body).map_err(|e| format!("fetching keys {e}"))?;
    let verified = keys
        .iter()
        .filter(|k| jws.kid.is_empty() || k.kid == jws.kid)
        .any(|k| oidc::verify_jws(&jws, &k.key));
    if !verified {
        return Err("failed to verify signature: failed to verify id token signature".into());
    }
    let token = oidc::id_token(&jws.payload, &jws.alg)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    oidc::check(&token, &p.issuer, client_id, now)?;
    Ok(token)
}

/// The subject a verified token's claims name (SubjectFromToken).
fn subject(t: &oidc::IdToken) -> Result<String, String> {
    shards_sigstore::fulcio::subject_from_claims(&t.claims)
}

/// An identity token from `issuer` by `flow`: `normal` (a browser), `device`, or
/// `client_credentials` (OIDConnect).
pub fn sign_in(
    flow: &str,
    issuer: &str,
    client_id: &str,
    secret: &str,
    redirect: Option<&str>,
) -> Result<String, String> {
    match flow {
        // StaticTokenGetter of the empty token: what go-jose says of it.
        "token" => return Err("go-jose/go-jose: compact JWS format must have three parts".into()),
        "normal" | "device" | "client_credentials" => {}
        other => return Err(format!("unsupported oauth flow: {other}")),
    }
    let p = discover(issuer, Duration::from_secs(30))?;
    match flow {
        "normal" => browser(&p, client_id, secret, redirect),
        "device" => device(&p, issuer, client_id, redirect),
        _ => client_credentials(&p, issuer, client_id, secret, redirect),
    }
}

/// The token endpoint's answer (oauth2's doTokenRoundTrip): the token's fields, or the
/// provider's refusal in oauth2's words.
fn token_answer(status: u16, body: &[u8]) -> Result<(String, String), String> {
    use shards_sigstore::tlog::gojson::{self, JValue};
    let failure = !(200..300).contains(&status);
    let refused = || {
        format!(
            "oauth2: cannot fetch token: {}\nResponse: {}",
            status_text(status),
            String::from_utf8_lossy(body)
        )
    };
    let v = match gojson::unmarshal(body) {
        Ok(v) => v,
        Err(_) if failure => return Err(refused()),
        Err(e) => return Err(format!("oauth2: cannot parse json: {e}")),
    };
    let text = |k: &str| v.get(k).and_then(JValue::as_str).unwrap_or_default().to_string();
    let code = text("error");
    if !code.is_empty() {
        let mut s = format!("oauth2: {}", oidc::go_quote(&code));
        for k in ["error_description", "error_uri"] {
            let t = text(k);
            if !t.is_empty() {
                s.push_str(&format!(" {}", oidc::go_quote(&t)));
            }
        }
        return Err(s);
    }
    if failure {
        return Err(refused());
    }
    let access = text("access_token");
    if access.is_empty() {
        return Err("oauth2: server response missing access_token".into());
    }
    let id = text("id_token");
    if id.is_empty() {
        return Err("id_token not present".into());
    }
    Ok((access, id))
}

/// Config.Exchange of `code`, the client authenticated in the header first and, where
/// that is refused, in the body (oauth2's AuthStyle probe).
fn exchange_code(
    p: &Provider,
    client_id: &str,
    secret: &str,
    pairs: &[(&str, &str)],
) -> Result<(String, String), String> {
    let basic = format!(
        "Basic {}",
        shards_sigstore::tlog::gocodec::std_encode(
            format!("{}:{}", oidc::query_escape(client_id), oidc::query_escape(secret)).as_bytes()
        )
    );
    let (status, body) = post_form(&p.token_endpoint, Some(&basic), &oidc::form(pairs))?;
    match token_answer(status, &body) {
        Ok(t) => Ok(t),
        Err(_) => {
            let mut with_client = pairs.to_vec();
            if !client_id.is_empty() {
                with_client.push(("client_id", client_id));
            }
            if !secret.is_empty() {
                with_client.push(("client_secret", secret));
            }
            let (status, body) = post_form(&p.token_endpoint, None, &oidc::form(&with_client))?;
            token_answer(status, &body)
        }
    }
}

/// Opens `url` in the browser as pkg/browser does: `open` on macOS, the first of
/// xdg-open, x-www-browser and www-browser on Linux and the BSDs, ShellExecute on Windows.
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        run("open", url)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let providers = ["xdg-open", "x-www-browser", "www-browser"];
        let path = std::env::var_os("PATH").unwrap_or_default();
        for p in providers {
            if std::env::split_paths(&path).any(|d| d.join(p).is_file()) {
                return run(p, url);
            }
        }
        Err(format!(
            "exec: \"{}\": executable file not found in $PATH",
            providers.join(",")
        ))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: ShellExecuteW reads the NUL-terminated URL; the other pointers are null,
        // which it takes as their defaults.
        let r = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                std::ptr::null(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // A value above 32 is success (ShellExecuteW's documentation).
        if r as usize > 32 {
            Ok(())
        } else {
            Err(format!("ShellExecute failed: {}", r as usize))
        }
    }
}

#[cfg(unix)]
fn run(program: &str, url: &str) -> Result<(), String> {
    let status = std::process::Command::new(program)
        .arg(url)
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program}: {status}"))
    }
}

/// The request line's target of an HTTP request read from `conn`, its head consumed.
fn request_target(conn: &mut TcpStream) -> Option<String> {
    conn.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut reader = io::BufReader::new(&*conn);
    let mut line = String::new();
    (&mut reader).take(16 << 10).read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (_, target) = (parts.next()?, parts.next()?);
    let target = target.to_string();
    loop {
        let mut header = String::new();
        let n = (&mut reader).take(16 << 10).read_line(&mut header).ok()?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }
    Some(target)
}

fn respond(conn: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        conn,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.flush();
}

/// The code a browser brings back to `path` on `listener` with `state` (the redirect
/// listener and getCode): a wrong state is refused; the wait is two minutes.
fn await_code(listener: &TcpListener, path: &str, state: &str) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    std::thread::scope(|scope| {
        let serving = std::thread::Builder::new()
            .name("oidc-redirect".into())
            .spawn_scoped(scope, || {
                while !stop.load(Ordering::Relaxed) {
                    let mut conn = match listener.accept() {
                        Ok((c, _)) => c,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    let _ = conn.set_nonblocking(false);
                    let Some(target) = request_target(&mut conn) else {
                        continue;
                    };
                    let (p, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
                    if p != path {
                        respond(&mut conn, "404 Not Found", "404 page not found");
                        continue;
                    }
                    let pairs = oidc::parse_form(query);
                    if oidc::form_value(&pairs, "state") != state {
                        respond(&mut conn, "400 Bad Request", "invalid state token");
                        let _ = tx.send(Err("invalid state token".into()));
                        return;
                    }
                    respond(&mut conn, "200 OK", SIGNED_IN);
                    let _ = tx.send(Ok(oidc::form_value(&pairs, "code").to_string()));
                    return;
                }
            });
        let got = match serving {
            Ok(_) => rx
                .recv_timeout(BROWSER_WAIT)
                .unwrap_or_else(|_| Err("timeout".into())),
            Err(e) => Err(e.to_string()),
        };
        stop.store(true, Ordering::Relaxed);
        got
    })
}

/// A code typed in by hand, the redirect out of band (doOobFlow).
fn oob_code(p: &Provider, client_id: &str, state: &str, extra: &[(&str, &str)]) -> Result<String, String> {
    let url = oidc::auth_code_url(&p.authorization_endpoint, client_id, OOB, state, extra);
    let mut err = io::stderr();
    let _ = writeln!(err, "Go to the following link in a browser:\n\n\t {url}");
    let _ = write!(err, "Enter verification code: ");
    let _ = err.flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let _ = writeln!(err);
    Ok(line.split_whitespace().next().unwrap_or_default().to_string())
}

/// The browser's flow (InteractiveIDTokenGetter): the code brought back to a listener on
/// this host, or typed in where no browser opens or none brings it back.
fn browser(p: &Provider, client_id: &str, secret: &str, redirect: Option<&str>) -> Result<String, String> {
    let state = oidc::random_url_safe(128)?;
    let nonce = oidc::random_url_safe(128)?;
    let (listener, redirect_url, path) = match redirect {
        Some(r) => {
            let u = shards_registry::url::Url::parse(r)
                .map_err(|e| format!("starting redirect listener: {e}"))?;
            let l =
                TcpListener::bind(u.authority()).map_err(|e| format!("starting redirect listener: {e}"))?;
            let path = u.target().split('?').next().unwrap_or("/").to_string();
            (l, r.to_string(), path)
        }
        None => {
            let l =
                TcpListener::bind("localhost:0").map_err(|e| format!("starting redirect listener: {e}"))?;
            let port = l.local_addr().map_err(|e| e.to_string())?.port();
            (
                l,
                format!("http://localhost:{port}/auth/callback"),
                "/auth/callback".to_string(),
            )
        }
    };
    let pkce = oidc::pkce(p)?;
    let extra = [
        ("code_challenge_method", pkce.method.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("access_type", "online"),
        ("nonce", nonce.as_str()),
    ];
    let url = oidc::auth_code_url(
        &p.authorization_endpoint,
        client_id,
        &redirect_url,
        &state,
        &extra,
    );
    let mut used_redirect = redirect_url.clone();
    let code = match open_browser(&url) {
        Err(e) => {
            let _ = writeln!(io::stderr(), "error opening browser: {e}");
            used_redirect = OOB.into();
            oob_code(p, client_id, &state, &extra)?
        }
        Ok(()) => {
            let _ = writeln!(io::stderr(), "Your browser will now be opened to:\n{url}");
            match await_code(&listener, &path, &state) {
                Ok(code) => code,
                Err(e) => {
                    let _ = writeln!(io::stderr(), "error getting code from local server: {e}");
                    used_redirect = OOB.into();
                    oob_code(p, client_id, &state, &extra)?
                }
            }
        }
    };
    let pairs = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", used_redirect.as_str()),
        ("code_verifier", pkce.verifier.as_str()),
        ("nonce", nonce.as_str()),
    ];
    let (access, raw) = exchange_code(p, client_id, secret, &pairs)?;
    let token = verify(p, client_id, &raw)?;
    if token.nonce != nonce {
        return Err("nonce does not match value sent".into());
    }
    if !token.at_hash.is_empty() {
        oidc::verify_access_token(&token, &access)?;
    }
    subject(&token)?;
    Ok(raw)
}

/// The endpoint `field` of the provider's discovery document, read again as the device
/// and client credentials getters read it (CodeURL), its issuer `issuer`.
fn code_url(issuer: &str, field: &str, missing: &str) -> Result<String, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let (status, body) = get(&url, Duration::from_secs(3))?;
    if status != 200 {
        return Err(format!(
            "{}: {}",
            status_text(status),
            String::from_utf8_lossy(&body)
        ));
    }
    let v = shards_sigstore::tlog::gojson::unmarshal(&body)
        .map_err(|e| format!("oidc: failed to decode provider discovery object: {e}"))?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    if text("issuer") != issuer {
        return Err(format!(
            "oidc: issuer did not match the issuer returned by provider, expected {} got {}",
            oidc::go_quote(issuer),
            oidc::go_quote(&text("issuer"))
        ));
    }
    let endpoint = text(field);
    if endpoint.is_empty() {
        return Err(missing.into());
    }
    Ok(endpoint)
}

/// The device flow (DeviceFlowTokenGetter): a code to type in elsewhere, the token
/// endpoint asked until it answers. shards waits RFC 8628's 5 s between asks where the
/// provider gives no interval; oauthflow asks again at once.
fn device(p: &Provider, issuer: &str, client_id: &str, redirect: Option<&str>) -> Result<String, String> {
    use shards_sigstore::tlog::gojson::JValue;
    let pkce = oidc::pkce(p)?;
    let mut pairs = vec![
        ("client_id", client_id),
        ("scope", "openid email"),
        ("code_challenge_method", pkce.method.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
    ];
    if let Some(r) = redirect {
        pairs.push(("redirect_uri", r));
    }
    let endpoint = code_url(
        issuer,
        "device_authorization_endpoint",
        "oidc: device authorization endpoint not returned by provider",
    )?;
    let (status, body) = post_form(&endpoint, None, &oidc::form(&pairs))?;
    if status != 200 {
        return Err(format!(
            "{}: {}",
            status_text(status),
            String::from_utf8_lossy(&body)
        ));
    }
    let v = shards_sigstore::tlog::gojson::unmarshal(&body)?;
    let text = |k: &str| v.get(k).and_then(JValue::as_str).unwrap_or_default().to_string();
    let number = |k: &str| match v.get(k) {
        Some(JValue::Number(n)) => n.parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    let device_code = text("device_code");
    let uri = Some(text("verification_uri_complete"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| text("verification_uri"));
    let mut err = io::stderr();
    let _ = writeln!(
        err,
        "Enter the verification code {} in your browser at: {uri}",
        text("user_code")
    );
    let _ = writeln!(err, "Code will be valid for {} seconds", number("expires_in"));
    let mut interval = Duration::from_secs(match number("interval") {
        0 => 5,
        n => n,
    });
    let poll = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("client_id", client_id),
        ("device_code", device_code.as_str()),
        ("scope", "openid email"),
        ("code_verifier", pkce.verifier.as_str()),
    ];
    let raw = loop {
        let (_, body) = post_form(&p.token_endpoint, None, &oidc::form(&poll))?;
        let t = shards_sigstore::tlog::gojson::unmarshal(&body)?;
        let field = |k: &str| t.get(k).and_then(JValue::as_str).unwrap_or_default().to_string();
        let id = field("id_token");
        if !id.is_empty() {
            let _ = writeln!(err, "Token received!");
            break id;
        }
        match field("error").as_str() {
            e @ ("access_denied" | "expired_token") => return Err(format!("error obtaining token: {e}")),
            "authorization_pending" => std::thread::sleep(interval),
            "slow_down" => {
                // RFC 8628 §3.5: the interval grows by 5 s for the rest of the flow;
                // oauthflow waits 10 s more once.
                interval += Duration::from_secs(5);
                std::thread::sleep(interval);
            }
            other => return Err(format!("unexpected error in device flow: {other}")),
        }
    };
    let token = verify(p, client_id, &raw)?;
    subject(&token)?;
    Ok(raw)
}

/// The client credentials flow (DefaultFlowClientCredentials): the client's own ID and
/// secret for a token.
fn client_credentials(
    p: &Provider,
    issuer: &str,
    client_id: &str,
    secret: &str,
    redirect: Option<&str>,
) -> Result<String, String> {
    use shards_sigstore::tlog::gojson::JValue;
    let mut pairs = vec![
        ("client_id", client_id),
        ("client_secret", secret),
        ("scope", "openid email"),
        ("grant_type", "client_credentials"),
    ];
    if let Some(r) = redirect {
        pairs.push(("redirect_uri", r));
    }
    let endpoint = code_url(
        issuer,
        "token_endpoint",
        "oidc: client credentials token authorization endpoint not returned by provider",
    )?;
    let (status, body) = post_form(&endpoint, None, &oidc::form(&pairs))?;
    if status != 200 {
        return Err(format!(
            "{}: {}",
            status_text(status),
            String::from_utf8_lossy(&body)
        ));
    }
    let v = shards_sigstore::tlog::gojson::unmarshal(&body)?;
    let field = |k: &str| v.get(k).and_then(JValue::as_str).unwrap_or_default().to_string();
    let raw = field("id_token");
    if raw.is_empty() {
        return Err(format!("unexpected error in client flow: {}", field("error")));
    }
    let _ = writeln!(io::stderr(), "Token received!");
    let token = verify(p, client_id, &raw)?;
    subject(&token)?;
    Ok(raw)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The token endpoint's answers read as oauth2 reads them.
    #[test]
    fn token_answers_read_as_oauth2_s() {
        assert_eq!(
            token_answer(200, br#"{"access_token":"a","id_token":"i"}"#).unwrap(),
            ("a".to_string(), "i".to_string())
        );
        assert_eq!(
            token_answer(
                400,
                br#"{"error":"invalid_grant","error_description":"bad code"}"#
            )
            .unwrap_err(),
            "oauth2: \"invalid_grant\" \"bad code\""
        );
        assert_eq!(
            token_answer(500, b"<html>").unwrap_err(),
            "oauth2: cannot fetch token: 500 Internal Server Error\nResponse: <html>"
        );
        assert_eq!(
            token_answer(200, br#"{"id_token":"i"}"#).unwrap_err(),
            "oauth2: server response missing access_token"
        );
        assert_eq!(
            token_answer(200, br#"{"access_token":"a"}"#).unwrap_err(),
            "id_token not present"
        );
    }

    /// The listener takes the code its state comes with, and refuses another state.
    #[test]
    fn the_browser_brings_the_code_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
                c.write_all(b"GET /elsewhere HTTP/1.1\r\nHost: x\r\n\r\n")
                    .unwrap();
                let mut answer = String::new();
                c.read_to_string(&mut answer).unwrap();
                assert!(answer.starts_with("HTTP/1.1 404"));
                let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
                c.write_all(b"GET /auth/callback?code=the%2Bcode&state=st HTTP/1.1\r\nHost: x\r\n\r\n")
                    .unwrap();
                let mut answer = String::new();
                c.read_to_string(&mut answer).unwrap();
                assert!(answer.starts_with("HTTP/1.1 200 OK"));
            });
            assert_eq!(await_code(&listener, "/auth/callback", "st").unwrap(), "the+code");
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
                c.write_all(b"GET /auth/callback?code=c&state=forged HTTP/1.1\r\n\r\n")
                    .unwrap();
            });
            assert_eq!(
                await_code(&listener, "/auth/callback", "st").unwrap_err(),
                "invalid state token"
            );
        });
    }
}
