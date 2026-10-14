//! Keyless signing (D126): `shards sign agent NAME` without `--key` signs as cosign
//! v3.1.3 signs without one: an identity token (given, from the environment as cosign's
//! providers find one, or by signing in), Fulcio's certificate for it, the timestamps and
//! transparency log entries the signing config asks for, the bundle checked against the
//! trusted root before it is kept. The services are Sigstore's public instance's, as its
//! TUF repository names them, unless `--signing-config` and `--trusted-root` name others.

use std::io::{self, BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::time::Duration;

use shards_registry::http::{Client, Patience, Redirects, Request};
use shards_registry::url::Url;
use shards_sigstore::keyless::{Answer, Services, Signed};
use shards_sigstore::signing_config::{self, SigningConfig};
use shards_sigstore::trusted_root::TrustedRoot;
use shards_tuf::client::Stage;

/// cosign's ambient token providers, in the order it asks them (Go initializes the
/// packages that register them in import path order).
pub const PROVIDERS: &[&str] = &[
    "buildkite-agent",
    "envvar",
    "filesystem",
    "github-actions",
    "google-workload-identity",
    "google-impersonate",
    "spiffe",
];

/// Where cosign's filesystem provider reads a token.
const FILESYSTEM_TOKEN: &str = "/var/run/sigstore/cosign/oidc-token";

/// The audience cosign asks providers for.
const AUDIENCE: &str = "sigstore";

/// cosign's privacy statement for Sigstore's public instance (privacy.Statement).
const STATEMENT: &str = "\n\tThe sigstore service, hosted by sigstore a Series of LF Projects, LLC, is provided pursuant to the Hosted Project Tools Terms of Use, available at https://lfprojects.org/policies/hosted-project-tools-terms-of-use/.\n\tNote that if your submission includes personal data associated with this signed artifact, it will be part of an immutable record.\n\tThis may include the email address associated with the account with which you authenticate your contractual Agreement.\n\tThis information will be used for signing this artifact and will be stored in public transparency logs and cannot be removed later, and is subject to the Immutable Record notice at https://lfprojects.org/policies/hosted-project-tools-immutable-records/.\n";

const CONFIRMATION: &str = "By typing 'y', you attest that (1) you are not submitting the personal data of any other person; and (2) you understand and agree to the statement and the Agreement terms at the URLs listed above.";

/// The operator Sigstore's public instance names in its signing config.
const PUBLIC_OPERATOR: &str = "sigstore.dev";

/// How a keyless signature is made: cosign's flags for it, which `shards sign` takes.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--identity-token`: a token, or a file holding one.
    pub identity_token: Option<String>,
    /// `--oidc-provider`: the one ambient provider to ask.
    pub oidc_provider: Option<String>,
    /// `--oidc-disable-ambient-providers`.
    pub disable_providers: bool,
    /// `--fulcio-auth-flow`.
    pub flow: Option<String>,
    /// `--oidc-client-id`, `sigstore` where not given.
    pub client_id: Option<String>,
    /// `--oidc-client-secret-file`.
    pub client_secret_file: Option<String>,
    /// `--oidc-redirect-url`.
    pub redirect_url: Option<String>,
    /// `--signing-config`.
    pub signing_config: Option<String>,
    /// `--trusted-root`.
    pub trusted_root: Option<String>,
    /// `--yes`/`-y`: no question before an identity is published.
    pub yes: bool,
}

/// The flags `shards sign` takes for keyless signing, each with its value.
const VALUED: &[&str] = &[
    "--identity-token",
    "--oidc-provider",
    "--fulcio-auth-flow",
    "--oidc-client-id",
    "--oidc-client-secret-file",
    "--oidc-redirect-url",
    "--signing-config",
    "--trusted-root",
];

impl Options {
    /// Takes `arg` (and the value after it from `rest`, where it needs one) if it is one
    /// of these flags: whether it was.
    pub fn take(&mut self, arg: &str, rest: &mut dyn Iterator<Item = &String>) -> Result<bool, String> {
        match arg {
            "--yes" | "-y" => {
                self.yes = true;
                return Ok(true);
            }
            "--oidc-disable-ambient-providers" => {
                self.disable_providers = true;
                return Ok(true);
            }
            _ => {}
        }
        let (flag, value) = match arg.split_once('=') {
            Some((f, v)) if VALUED.contains(&f) => (f, v.to_string()),
            _ if VALUED.contains(&arg) => (arg, rest.next().ok_or(format!("{arg} needs a value"))?.clone()),
            _ => return Ok(false),
        };
        let slot = match flag {
            "--identity-token" => &mut self.identity_token,
            "--oidc-provider" => &mut self.oidc_provider,
            "--fulcio-auth-flow" => &mut self.flow,
            "--oidc-client-id" => &mut self.client_id,
            "--oidc-client-secret-file" => &mut self.client_secret_file,
            "--oidc-redirect-url" => &mut self.redirect_url,
            "--signing-config" => &mut self.signing_config,
            _ => &mut self.trusted_root,
        };
        *slot = Some(value);
        Ok(true)
    }

    /// Whether any flag of keyless signing was given (beside `--key` they are refused).
    pub fn any(&self) -> bool {
        self.identity_token.is_some()
            || self.oidc_provider.is_some()
            || self.disable_providers
            || self.flow.is_some()
            || self.client_id.is_some()
            || self.client_secret_file.is_some()
            || self.redirect_url.is_some()
            || self.signing_config.is_some()
            || self.trusted_root.is_some()
    }
}

fn now() -> (i64, u32) {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or((0, 0), |d| {
            (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos())
        })
}

/// The client the services are asked with: TLS by the system's roots, the environment's
/// proxies, `wait` for an answer to begin and for a stalled transfer.
fn client(wait: Duration) -> Result<Client, String> {
    let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
    Ok(Client::new(
        Box::new(move |_| Ok(config.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    )
    .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
        std::env::var(k).ok()
    }))
    .with_patience(Patience {
        head: Some(wait),
        stall: Some(wait),
        ..Patience::default()
    }))
}

/// An HTTP exchange: the answer's status, ETag and body (at most `max` bytes, as Go's
/// LimitReader takes them). The credentials in `authorization` go to `url`'s origin
/// alone; a redirect elsewhere is refused.
pub fn exchange(
    method: &str,
    url: &str,
    authorization: Option<&str>,
    headers: &[(&str, &str)],
    body: &[u8],
    wait: Duration,
    max: u64,
) -> Result<Answer, String> {
    let parsed = Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    let client = client(wait)?;
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

/// The network keyless signing asks.
struct Net;

impl Services for Net {
    fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        wait: Duration,
        max: u64,
    ) -> Result<Answer, String> {
        let auth = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| *v);
        let rest: Vec<(&str, &str)> = headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("authorization"))
            .copied()
            .collect();
        exchange("POST", url, auth, &rest, body, wait, max)
    }

    fn wait(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// shards_tuf's fetcher over shards' client.
struct Tuf;

impl shards_tuf::Fetch for Tuf {
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, shards_tuf::FetchError> {
        let answer = exchange(
            "GET",
            url,
            None,
            &[],
            &[],
            Duration::from_secs(30),
            max.saturating_add(1),
        )
        .map_err(shards_tuf::FetchError::Other)?;
        if answer.status != 200 {
            return Err(shards_tuf::FetchError::Status {
                url: url.to_string(),
                code: answer.status,
            });
        }
        if u64::try_from(answer.body.len()).unwrap_or(u64::MAX) > max {
            return Err(shards_tuf::FetchError::Other(format!(
                "{url}: more than {max} bytes"
            )));
        }
        Ok(answer.body)
    }
}

/// A target of Sigstore's TUF repository, by way of the cache the build policies use.
fn tuf_target(name: &str) -> Result<Vec<u8>, String> {
    let cache = shards_ipc::home()?.join("policy").join("tuf");
    let (got, _status) = shards_tuf::client::target(&cache, &Tuf, now(), name);
    got.map_err(|(stage, e)| match stage {
        Stage::Provider => format!("loading Sigstore's TUF repository: {e}"),
        Stage::Root => format!("getting {name} from Sigstore's TUF repository: {e}"),
    })
}

/// The signing config: `--signing-config`'s, else the public instance's from TUF.
pub fn signing_config(o: &Options) -> Result<SigningConfig, String> {
    let (bytes, whence) = match &o.signing_config {
        Some(path) => (
            std::fs::read(path).map_err(|e| format!("{path}: {e}"))?,
            path.clone(),
        ),
        None => (
            tuf_target("signing_config.v0.2.json")?,
            "Sigstore's signing config".to_string(),
        ),
    };
    signing_config::parse(&bytes).map_err(|e| format!("{whence}: {e}"))
}

/// The trusted root: `--trusted-root`'s, else the public instance's from TUF.
pub fn trusted_root(o: &Options) -> Result<TrustedRoot, String> {
    let (bytes, whence) = match &o.trusted_root {
        Some(path) => (
            std::fs::read(path).map_err(|e| format!("{path}: {e}"))?,
            path.clone(),
        ),
        None => (
            tuf_target("trusted_root.json")?,
            "Sigstore's trusted root".to_string(),
        ),
    };
    shards_sigstore::trusted_root::parse(&bytes).map_err(|e| format!("{whence}: {}", e.0))
}

/// Whether `s` is a token rather than a file holding one: a JWS in compact form, its
/// header of an algorithm a token is signed with. cosign takes only RS256 as a token and
/// reads any other as a file's name; shards takes each algorithm Fulcio's issuers sign
/// tokens with.
fn is_token(s: &str) -> bool {
    let mut parts = s.split('.');
    let (Some(header), Some(_), Some(_), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Ok(header) = shards_sigstore::gobase64::decode(header.as_bytes(), true, false) else {
        return false;
    };
    let Ok(v) = shards_sigstore::tlog::gojson::unmarshal(&header) else {
        return false;
    };
    matches!(
        v.get("alg").and_then(|a| a.as_str()),
        Some(
            "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" | "ES256" | "ES384" | "ES512" | "EdDSA"
        )
    )
}

/// `--identity-token`'s token: itself, or the file it names.
fn given_token(s: &str) -> Result<String, String> {
    if s.is_empty() || is_token(s) {
        return Ok(s.to_string());
    }
    std::fs::read_to_string(s).map_err(|e| format!("getting id token: {s}: {e}"))
}

/// Whether the provider `name` finds what it needs in this environment (Enabled).
fn enabled(name: &str) -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    match name {
        "buildkite-agent" => set("BUILDKITE_AGENT_ACCESS_TOKEN"),
        "envvar" => std::env::var_os("SIGSTORE_ID_TOKEN").is_some(),
        "filesystem" => std::fs::metadata(FILESYSTEM_TOKEN).is_ok(),
        "github-actions" => set("ACTIONS_ID_TOKEN_REQUEST_TOKEN") && set("ACTIONS_ID_TOKEN_REQUEST_URL"),
        "google-workload-identity" => {
            crate::google::on_gce() && crate::google::workload_identity("garbage").is_ok()
        }
        "google-impersonate" => set("GOOGLE_SERVICE_ACCOUNT_NAME"),
        "spiffe" => std::fs::metadata(crate::spiffe::socket()).is_ok(),
        _ => false,
    }
}

/// The token the provider `name` gives for `audience` (Provide).
fn provide(name: &str, audience: &str) -> Result<String, String> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    match name {
        "envvar" => Ok(var("SIGSTORE_ID_TOKEN")),
        "filesystem" => {
            std::fs::read_to_string(FILESYSTEM_TOKEN).map_err(|e| format!("{FILESYSTEM_TOKEN}: {e}"))
        }
        "github-actions" => github_actions(audience),
        "buildkite-agent" => buildkite(audience),
        "google-workload-identity" => crate::google::workload_identity(audience),
        "google-impersonate" => crate::google::impersonate(&var("GOOGLE_SERVICE_ACCOUNT_NAME"), audience),
        "spiffe" => crate::spiffe::jwt_svid(audience),
        other => Err(format!("{other} is not a valid provider")),
    }
}

/// The GitHub Actions provider: the job's token for `audience`, from the runner's
/// endpoint, asked again twice where it cannot be reached.
fn github_actions(audience: &str) -> Result<String, String> {
    let url = format!(
        "{}&audience={audience}",
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").unwrap_or_default()
    );
    let auth = format!(
        "bearer {}",
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").unwrap_or_default()
    );
    let mut attempt = 0;
    let answer = loop {
        match exchange(
            "GET",
            &url,
            Some(&auth),
            &[],
            &[],
            Duration::from_secs(30),
            1 << 20,
        ) {
            Ok(a) => break a,
            Err(e) if attempt == 2 => return Err(e),
            Err(e) => {
                let _ = writeln!(io::stderr(), "error fetching GitHub OIDC token (will retry): {e}");
                std::thread::sleep(Duration::from_secs(1));
                attempt += 1;
            }
        }
    };
    // cosign decodes whatever the runner answers; shards says where it refused.
    if answer.status != 200 {
        return Err(format!(
            "GitHub's token endpoint answered {}: {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        ));
    }
    field(&answer.body, "value")
}

/// The member `name` of a JSON answer, a string, as encoding/json decodes it into a
/// struct of that one field.
fn field(body: &[u8], name: &str) -> Result<String, String> {
    use shards_sigstore::tlog::gojson::{self, JValue};
    let v = gojson::decode_first(body)?;
    let JValue::Object(members) = &v else {
        return Err(gojson::type_error(v.kind(), "", "", "struct"));
    };
    let mut out = String::new();
    for (_, value) in gojson::struct_members(members, &[name]) {
        match value {
            JValue::Str(s) => out = s.clone(),
            JValue::Null => {}
            other => return Err(gojson::type_error(other.kind(), "", name, "string")),
        }
    }
    Ok(out)
}

/// The Buildkite agent provider: the job's token for `audience`, from the agent API
/// (buildkite/agent v3.130.0 api.OIDCToken).
fn buildkite(audience: &str) -> Result<String, String> {
    let token = std::env::var("BUILDKITE_AGENT_ACCESS_TOKEN").unwrap_or_default();
    let endpoint = std::env::var("BUILDKITE_AGENT_ENDPOINT")
        .ok()
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "https://agent.buildkite.com/v3".into());
    let job = std::env::var("BUILDKITE_JOB_ID").unwrap_or_default();
    // railsPathEscape: url.PathEscape, and dots too.
    let mut escaped = String::new();
    for b in job.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@' => escaped.push(char::from(b)),
            _ => escaped.push_str(&format!("%{b:02X}")),
        }
    }
    let url = format!(
        "{}/{}",
        endpoint.trim_end_matches('/'),
        format!("jobs/{escaped}/oidc/tokens").trim_start_matches('/')
    );
    let body = format!(
        "{{\"audience\":{}}}\n",
        shards_sigstore::fulcio::go_json_string(audience)
    );
    let auth = format!("Token {token}");
    let answer = exchange(
        "POST",
        &url,
        Some(&auth),
        &[("Content-Type", "application/json")],
        body.as_bytes(),
        Duration::from_secs(60),
        1 << 20,
    )?;
    if !(200..300).contains(&answer.status) {
        let message = field(&answer.body, "message").unwrap_or_default();
        let mut s = format!("POST {url}: {}", answer.status);
        if !message.is_empty() {
            s.push_str(&format!(": {message}"));
        }
        return Err(s);
    }
    field(&answer.body, "token").map_err(|e| format!("failed to decode JSON response: {e}"))
}

/// The ambient token (ReadIDToken): `--oidc-provider`'s, else the first enabled
/// provider's that gives one; empty where none is enabled.
fn ambient(o: &Options) -> Result<String, String> {
    if o.disable_providers || !PROVIDERS.iter().any(|p| enabled(p)) {
        return Ok(String::new());
    }
    let fail = |e: String| format!("fetching ambient OIDC credentials: {e}");
    if let Some(name) = &o.oidc_provider {
        if !PROVIDERS.contains(&name.as_str()) {
            return Err(format!("getting provider: {name} is not a valid provider"));
        }
        return provide(name, AUDIENCE).map_err(fail);
    }
    let mut last = None;
    for p in PROVIDERS {
        if !enabled(p) {
            continue;
        }
        match provide(p, AUDIENCE) {
            Ok(t) => return Ok(t),
            Err(e) => last = Some(e),
        }
    }
    Err(fail(last.unwrap_or_else(|| {
        "no providers are enabled, check providers.Enabled() before providers.Provide()".into()
    })))
}

/// The identity token to sign with (RetrieveIDToken): given, else ambient, else signed
/// in for by the flow `--fulcio-auth-flow` names or the terminal suits.
fn token(o: &Options, config: &SigningConfig) -> Result<String, String> {
    let mut token = match &o.identity_token {
        Some(t) => given_token(t)?,
        None => String::new(),
    };
    if token.is_empty() {
        token = ambient(o)?;
    }
    if !token.is_empty() {
        return Ok(token.trim_end_matches(['\n', '\r']).to_string());
    }
    let flow = match o.flow.as_deref() {
        Some(f) => f.to_string(),
        None if !io::stdin().is_terminal() => {
            let _ = writeln!(io::stderr(), "Non-interactive mode detected, using device flow.");
            "device".into()
        }
        None => "normal".into(),
    };
    let issuer = signing_config::select_service(&config.oidc, &[1], now())
        .map_err(|e| format!("selecting OIDC issuer: {e}"))?;
    let client_id = o.client_id.clone().unwrap_or_else(|| "sigstore".into());
    let secret = match &o.client_secret_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("{path}: {e}"))?
            .trim()
            .to_string(),
        None => String::new(),
    };
    crate::oidc::sign_in(&flow, &issuer.url, &client_id, &secret, o.redirect_url.as_deref())
        .map_err(|e| format!("authenticating caller: {e}"))
}

/// Before an identity goes into a log: what will be published, and where. On a terminal
/// it is asked (cosign's question) unless `--yes`; elsewhere it is said.
fn notice(o: &Options, subject: &str, logs: &[String], public: bool) -> Result<(), String> {
    let mut err = io::stderr();
    if public {
        let _ = writeln!(err, "{STATEMENT}");
    }
    let _ = writeln!(
        err,
        "{subject} will be recorded in {}, where it cannot be removed.",
        logs.join(" and ")
    );
    if o.yes || !io::stdin().is_terminal() {
        return Ok(());
    }
    if public {
        let _ = writeln!(err, "{CONFIRMATION}");
    }
    let _ = write!(err, "Are you sure you would like to continue? [y/N] ");
    let _ = err.flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    match line.trim_matches(['\r', '\n']).to_lowercase().as_str() {
        "y" => Ok(()),
        "" | "n" => Err("user declined the prompt".into()),
        other => Err(format!("invalid input {other:?} (allowed values y, n)")),
    }
}

/// `payload`, an in-toto statement, signed keylessly as `o` asks.
pub fn sign(payload: &[u8], o: &Options) -> Result<Signed, String> {
    let config = signing_config(o)?;
    let root = trusted_root(o)?;
    let token = token(o, &config)?;
    let subject = shards_sigstore::fulcio::subject_of_token(&token)?;
    let logs: Vec<String> = config.rekor.iter().map(|s| s.url.clone()).collect();
    if !logs.is_empty() {
        let public = config.rekor.iter().all(|s| s.operator == PUBLIC_OPERATOR);
        notice(o, &subject, &logs, public)?;
    }
    shards_sigstore::keyless::sign(payload, &token, &config, Some(&root), &Net, now())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_cosign_s() {
        let mut o = Options::default();
        let args: Vec<String> = ["tok", "x"].iter().map(|s| s.to_string()).collect();
        let mut rest = args.iter();
        assert!(o.take("--identity-token", &mut rest).unwrap());
        assert!(o.take("--signing-config=sc.json", &mut rest).unwrap());
        assert!(o.take("-y", &mut rest).unwrap());
        assert!(!o.take("--key", &mut rest).unwrap());
        assert_eq!(o.identity_token.as_deref(), Some("tok"));
        assert_eq!(o.signing_config.as_deref(), Some("sc.json"));
        assert!(o.yes && o.any());
        assert_eq!(
            Options::default()
                .take("--trusted-root", &mut [].iter())
                .unwrap_err(),
            "--trusted-root needs a value"
        );
    }

    /// A token is a compact JWS of a token's algorithm; anything else names a file.
    #[test]
    fn a_token_is_told_from_a_file() {
        let b64 = |s: &str| {
            shards_sigstore::tlog::gocodec::std_encode(s.as_bytes())
                .trim_end_matches('=')
                .replace('+', "-")
                .replace('/', "_")
        };
        assert!(is_token(&format!(
            "{}.{}.sig",
            b64(r#"{"alg":"RS256"}"#),
            b64("{}")
        )));
        assert!(is_token(&format!(
            "{}.{}.sig",
            b64(r#"{"alg":"ES256","kid":"k"}"#),
            b64("{}")
        )));
        assert!(!is_token(&format!(
            "{}.{}.sig",
            b64(r#"{"alg":"none"}"#),
            b64("{}")
        )));
        assert!(!is_token("token.txt"));
        assert!(!is_token("/tmp/a.b.c"));
    }

    #[test]
    fn json_fields_are_read_as_go_reads_them() {
        assert_eq!(field(br#"{"Value":"t","other":1}"#, "value").unwrap(), "t");
        assert_eq!(field(br#"{"value":null}"#, "value").unwrap(), "");
        assert!(field(br#"{"value":1}"#, "value").is_err());
    }
}
