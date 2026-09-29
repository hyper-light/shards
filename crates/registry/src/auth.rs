//! Registry authorization (docs/research/registry-pull.md §3.1), as containerd v2.4.1
//! performs it (`core/remotes/docker/authorizer.go` and `auth/`):
//! - `WWW-Authenticate` is parsed as `auth/parse.go` parses it.
//! - A 401's bearer challenge starts a handler for its host, and its basic challenge does
//!   when there are credentials.
//! - Bearer tokens are fetched as `auth/fetch.go` fetches them: an OAuth2 POST when there
//!   are credentials, falling back to a GET with Basic authentication, else an anonymous
//!   GET. They are cached per host and set of scopes, one fetch at a time.
//! - A registry token (`registrytoken`) is sent to its registry as it is, as dockerd sends
//!   it (`daemon/containerd/resolver.go`).
//!
//! Stricter than containerd:
//! - A realm must be `https`, unless it and the registry are both plain HTTP on loopback.
//!   containerd sends credentials to any realm a registry names.
//! - A token lasts `expires_in`, and at least 60 s, from `issued_at` or its receipt, as
//!   distribution's client counts. containerd keeps a token without `expires_in` forever.
//! - A request refused twice in a row is not retried again.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Read;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use crate::Error;
use crate::http::{Client, Request, Response};
use crate::url::{Scheme as UrlScheme, Url};

/// What shards names itself to token servers (`client_id`; distribution's oauth.md asks
/// for "a meaningful value").
const CLIENT_ID: &str = "shards";
/// A token server's answer is read up to this size: tokens are a few KiB.
const MAX_TOKEN_RESPONSE: u64 = 1 << 20;
/// distribution's `minimumTokenLifetimeSeconds`, and the spec's default.
const MIN_LIFETIME: Duration = Duration::from_secs(60);

/// A challenge's scheme, in containerd's order of preference: bearer, digest, basic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scheme {
    Basic,
    Digest,
    Bearer,
}

/// One `WWW-Authenticate` challenge, with lowercased parameter names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: Scheme,
    pub params: BTreeMap<String, String>,
}

/// The challenges in `values`, one per field line, best first (`ParseAuthHeader`).
pub fn challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    let mut found: Vec<Challenge> = values.into_iter().filter_map(challenge).collect();
    found.sort_by_key(|c| std::cmp::Reverse(c.scheme));
    found
}

/// `parseValueAndParams`: a scheme, then `key=value` pairs separated by commas. Parsing
/// stops, keeping what it has, at the first byte that doesn't fit.
fn challenge(line: &str) -> Option<Challenge> {
    let line = line.trim_matches([' ', '\t']).as_bytes();
    let (scheme, mut rest) = token(line);
    let scheme = match scheme.to_ascii_lowercase().as_slice() {
        b"basic" => Scheme::Basic,
        b"digest" => Scheme::Digest,
        b"bearer" => Scheme::Bearer,
        _ => return None,
    };
    let mut params = BTreeMap::new();
    loop {
        let (key, after) = token(skip_space(rest));
        if key.is_empty() {
            break;
        }
        let Some(after) = after.strip_prefix(b"=") else {
            break;
        };
        let (value, after) = token_or_quoted(after);
        params.insert(
            String::from_utf8_lossy(key).to_ascii_lowercase(),
            String::from_utf8_lossy(&value).into_owned(),
        );
        let Some(after) = skip_space(after).strip_prefix(b",") else {
            break;
        };
        rest = after;
    }
    Some(Challenge { scheme, params })
}

/// An RFC 2616 token character: US-ASCII, not a control, not a separator.
fn is_token(b: u8) -> bool {
    b.is_ascii() && !b.is_ascii_control() && !b" \t\"(),/:;<=>?@[]\\{}".contains(&b)
}

fn skip_space(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|b| b" \t\r\n".contains(b)).count();
    s.get(n..).unwrap_or_default()
}

fn token(s: &[u8]) -> (&[u8], &[u8]) {
    let n = s.iter().take_while(|&&b| is_token(b)).count();
    (s.get(..n).unwrap_or_default(), s.get(n..).unwrap_or_default())
}

/// A token, or a quoted string with `\` escapes. An unterminated quote gives nothing.
fn token_or_quoted(s: &[u8]) -> (Vec<u8>, &[u8]) {
    let Some(quoted) = s.strip_prefix(b"\"") else {
        let (t, rest) = token(s);
        return (t.to_vec(), rest);
    };
    let mut value = Vec::new();
    let mut escape = false;
    for (i, &b) in quoted.iter().enumerate() {
        match b {
            _ if escape => {
                value.push(b);
                escape = false;
            }
            b'\\' => escape = true,
            b'"' => return (value, quoted.get(i + 1..).unwrap_or_default()),
            _ => value.push(b),
        }
    }
    (Vec::new(), &[])
}

/// What `docker login` left for a registry (§3.3).
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Credentials {
    #[default]
    Anonymous,
    Password {
        username: String,
        password: String,
    },
    /// Exchanged for tokens with OAuth2's `refresh_token` grant.
    IdentityToken(String),
    /// Sent to the registry as a bearer token, as it is.
    RegistryToken(String),
}

/// Names the kind of credentials, never their secrets.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credentials::Anonymous => f.write_str("Anonymous"),
            Credentials::Password { username, .. } => write!(f, "Password({username:?})"),
            Credentials::IdentityToken(_) => f.write_str("IdentityToken"),
            Credentials::RegistryToken(_) => f.write_str("RegistryToken"),
        }
    }
}

impl Credentials {
    /// containerd's (username, secret) for them.
    fn pair(&self) -> (&str, &str) {
        match self {
            Credentials::Password { username, password } => (username, password),
            Credentials::IdentityToken(token) => ("", token),
            Credentials::Anonymous | Credentials::RegistryToken(_) => ("", ""),
        }
    }
}

/// Answers registry challenges for one pull: `credentials` belong to `registry`'s host
/// alone, and every other host is answered anonymously.
pub struct Authorizer {
    registry: Url,
    credentials: Credentials,
    hosts: Mutex<HashMap<String, Arc<Handler>>>,
}

impl fmt::Debug for Authorizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorizer")
            .field("registry", &self.registry.authority())
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

enum Handler {
    Basic(String),
    Bearer(Box<Bearer>),
}

struct Bearer {
    realm: Url,
    service: String,
    scopes: Vec<String>,
    username: String,
    secret: String,
    /// One slot per set of scopes; whoever holds a slot's lock fetches for it.
    tokens: Mutex<HashMap<String, Arc<Mutex<Option<Token>>>>>,
}

struct Token {
    value: String,
    expires: SystemTime,
}

impl Authorizer {
    pub fn new(registry: &Url, credentials: Credentials) -> Authorizer {
        Authorizer {
            registry: registry.clone(),
            credentials,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    fn is_registry(&self, url: &Url) -> bool {
        url.scheme() == self.registry.scheme() && url.authority() == self.registry.authority()
    }

    /// The `Authorization` value for a request to `url` that needs `scopes`, fetching a
    /// token first if the cached one is missing or has expired.
    pub fn authorization(
        &self,
        http: &Client,
        url: &Url,
        scopes: &[String],
    ) -> Result<Option<String>, Error> {
        if let Credentials::RegistryToken(token) = &self.credentials {
            return Ok(self.is_registry(url).then(|| format!("Bearer {token}")));
        }
        let handler = {
            let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
            hosts.get(&url.authority()).cloned()
        };
        match handler.as_deref() {
            None => Ok(None),
            Some(Handler::Basic(value)) => Ok(Some(value.clone())),
            Some(Handler::Bearer(bearer)) => bearer.authorization(http, scopes).map(Some),
        }
    }

    /// Takes in a 401 from `url` and says whether to send the request again. `repeated`
    /// says whether the same request was just refused as well.
    pub fn challenged(&self, url: &Url, response: &Response, repeated: bool) -> Result<bool, Error> {
        // dockerd never retries a registry token (`bearerAuthorizer.AddResponses`).
        if matches!(self.credentials, Credentials::RegistryToken(_)) || repeated {
            return Ok(false);
        }
        let host = url.authority();
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        for c in challenges(response.headers("www-authenticate")) {
            match c.scheme {
                Scheme::Bearer => {
                    // `error=` means the token itself was refused: start again.
                    if c.params.contains_key("error") {
                        hosts.remove(&host);
                    }
                    if hosts.contains_key(&host) {
                        return Ok(true);
                    }
                    let (username, secret) = if self.is_registry(url) {
                        self.credentials.pair()
                    } else {
                        ("", "")
                    };
                    let bearer = Bearer::new(url, &c, username, secret)?;
                    hosts.insert(host, Arc::new(Handler::Bearer(Box::new(bearer))));
                    return Ok(true);
                }
                Scheme::Basic => {
                    let (username, secret) = if self.is_registry(url) {
                        self.credentials.pair()
                    } else {
                        ("", "")
                    };
                    if username.is_empty() || secret.is_empty() {
                        return Err(Error::new(format!(
                            "{host} asks for credentials, and there are none"
                        )));
                    }
                    let basic = format!("Basic {}", BASE64.encode(format!("{username}:{secret}")));
                    hosts.insert(host, Arc::new(Handler::Basic(basic)));
                    return Ok(true);
                }
                Scheme::Digest => {}
            }
        }
        Ok(false)
    }
}

impl Bearer {
    /// containerd's `GenerateTokenOptions`, refusing a realm that would take the token
    /// exchange off TLS.
    fn new(registry: &Url, c: &Challenge, username: &str, secret: &str) -> Result<Bearer, Error> {
        let realm = c
            .params
            .get("realm")
            .ok_or_else(|| Error::new(format!("{registry}: a bearer challenge without a realm")))?;
        let realm = Url::parse(realm).map_err(|e| Error::new(format!("{registry}: the token realm: {e}")))?;
        if realm.scheme() == UrlScheme::Http
            && !(loopback(&realm) && registry.scheme() == UrlScheme::Http && loopback(registry))
        {
            return Err(Error::new(format!(
                "{registry} names the token realm {realm}, which is not https"
            )));
        }
        Ok(Bearer {
            realm,
            service: c.params.get("service").cloned().unwrap_or_default(),
            scopes: c
                .params
                .get("scope")
                .map(|s| s.split(' ').map(str::to_string).collect())
                .unwrap_or_default(),
            username: username.to_string(),
            secret: secret.to_string(),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn authorization(&self, http: &Client, request_scopes: &[String]) -> Result<String, Error> {
        // containerd's GetTokenScopes: the request's and the challenge's, sorted, deduplicated.
        let mut scopes: Vec<String> = request_scopes.iter().chain(&self.scopes).cloned().collect();
        scopes.sort();
        scopes.dedup();
        let slot = {
            let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
            tokens.entry(scopes.join(" ")).or_default().clone()
        };
        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = slot.as_ref()
            && SystemTime::now() < token.expires
        {
            return Ok(format!("Bearer {}", token.value));
        }
        let token = self.fetch(http, &scopes)?;
        let value = format!("Bearer {}", token.value);
        *slot = Some(token);
        Ok(value)
    }

    /// containerd's `doBearerAuth`: OAuth2 when there is a secret, falling back to GET
    /// where the server has no OAuth2 endpoint; else an anonymous GET.
    fn fetch(&self, http: &Client, scopes: &[String]) -> Result<Token, Error> {
        let context = |e: Error| Error::new(format!("fetching a token from {}: {e}", self.realm));
        if self.secret.is_empty() {
            return self.get(http, scopes).map_err(context);
        }
        let mut response = self.post(http, scopes).map_err(context)?;
        match response.status {
            // Registries without OAuth2: GCR answers 404, Artifactory 401, ACR 400.
            405 if !self.username.is_empty() => self.get(http, scopes).map_err(context),
            404 | 401 | 400 => self.get(http, scopes).map_err(context),
            _ => read_token(&mut response, true).map_err(context),
        }
    }

    /// `FetchTokenWithOAuth`: a form POST, fields in Go's sorted order.
    fn post(&self, http: &Client, scopes: &[String]) -> Result<Response, Error> {
        let mut form: Vec<(&str, String)> = vec![("client_id", CLIENT_ID.to_string())];
        if self.username.is_empty() {
            form.push(("grant_type", "refresh_token".into()));
            form.push(("refresh_token", self.secret.clone()));
        } else {
            form.push(("grant_type", "password".into()));
            form.push(("password", self.secret.clone()));
            form.push(("username", self.username.clone()));
        }
        if !scopes.is_empty() {
            form.push(("scope", scopes.join(" ")));
        }
        form.push(("service", self.service.clone()));
        form.sort_by(|a, b| a.0.cmp(b.0));
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())))
            .finish();
        let realm = &self.realm;
        http.follow(
            &Request {
                method: "POST",
                url: realm,
                headers: &[("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")],
                body: body.as_bytes(),
            },
            &|_| Ok(None),
        )
    }

    /// `FetchToken`: a GET with `service` and each scope added to the realm's query, and
    /// Basic authentication when there is a secret, sent to the realm's origin only.
    fn get(&self, http: &Client, scopes: &[String]) -> Result<Token, Error> {
        let (base, query) = match self.realm.as_str().split_once('?') {
            Some((base, query)) => (base, query),
            None => (self.realm.as_str(), ""),
        };
        let mut pairs: Vec<(String, String)> =
            form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        if !self.service.is_empty() {
            pairs.push(("service".into(), self.service.clone()));
        }
        pairs.extend(scopes.iter().map(|s| ("scope".to_string(), s.clone())));
        // Go's url.Values.Encode: sorted by key, values in order.
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&pairs)
            .finish();
        let url = if query.is_empty() {
            Url::parse(base)?
        } else {
            Url::parse(&format!("{base}?{query}"))?
        };
        let basic = (!self.secret.is_empty()).then(|| {
            format!(
                "Basic {}",
                BASE64.encode(format!("{}:{}", self.username, self.secret))
            )
        });
        let realm = &self.realm;
        let mut response = http.follow(
            &Request {
                method: "GET",
                url: &url,
                headers: &[],
                body: &[],
            },
            &|hop| Ok(basic.clone().filter(|_| hop.same_origin(realm))),
        )?;
        read_token(&mut response, false)
    }
}

/// The fields of a token server's answer (distribution's token.md and oauth.md).
#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<i64>,
    issued_at: Option<String>,
}

/// Reads a token server's answer. An OAuth2 answer must carry `access_token`; a GET's
/// carries `token` or `access_token`, and `access_token` wins, as containerd has it.
fn read_token(response: &mut Response, oauth: bool) -> Result<Token, Error> {
    if !(200..400).contains(&response.status) {
        return Err(Error::new(format!("unexpected status {}", response.status)));
    }
    let received = SystemTime::now();
    let mut body = Vec::new();
    response
        .take(MAX_TOKEN_RESPONSE + 1)
        .read_to_end(&mut body)
        .map_err(|e| Error::new(format!("reading the token: {e}")))?;
    if body.len() as u64 > MAX_TOKEN_RESPONSE {
        return Err(Error::new("the token response passes 1 MiB"));
    }
    // Go's json.Decoder reads the first value and ignores what follows.
    let parsed: TokenResponse = serde_json::Deserializer::from_slice(&body)
        .into_iter()
        .next()
        .ok_or_else(|| Error::new("an empty token response"))?
        .map_err(|e| Error::new(format!("unable to decode the token response: {e}")))?;
    let value = match (parsed.access_token, parsed.token) {
        (Some(t), _) if !t.is_empty() => t,
        (_, Some(t)) if !t.is_empty() && !oauth => t,
        _ => return Err(Error::new("the token server did not include a token")),
    };
    let issued = match parsed.issued_at {
        None => received,
        Some(at) => {
            let at = time::OffsetDateTime::parse(&at, &time::format_description::well_known::Rfc3339)
                .map_err(|e| Error::new(format!("a bad issued_at {at:?}: {e}")))?;
            SystemTime::from(at)
        }
    };
    let lifetime = parsed
        .expires_in
        .and_then(|s| u64::try_from(s).ok())
        .map_or(MIN_LIFETIME, |s| Duration::from_secs(s).max(MIN_LIFETIME));
    Ok(Token {
        value,
        expires: issued.checked_add(lifetime).unwrap_or(issued),
    })
}

/// Hosts containerd serves over plain HTTP by default: `localhost`, 127.0.0.0/8 and ::1
/// (`core/remotes/docker/registry.go`).
pub(crate) fn loopback(url: &Url) -> bool {
    let host = url.host().trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::testing::{After, Server, serve};

    fn plain() -> Client {
        Client::new(
            Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
            "shards-test",
        )
    }

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn http(status: &str, fields: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\n{fields}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A token server answering with `answers` in turn.
    fn token_server(answers: &[&str]) -> Server {
        serve(
            None,
            answers
                .iter()
                .map(|a| (http("200 OK", "", a), After::Keep))
                .collect(),
        )
    }

    /// A response carrying `challenge`, as a registry's 401 would.
    fn refusal(challenge: &str) -> Response {
        let server = serve(
            None,
            vec![(
                http(
                    "401 Unauthorized",
                    &format!("WWW-Authenticate: {challenge}\r\n"),
                    "",
                ),
                After::Keep,
            )],
        );
        let url = Url::parse(&format!("http://127.0.0.1:{}/v2/", server.port)).unwrap();
        plain()
            .send(&Request {
                method: "GET",
                url: &url,
                headers: &[],
                body: &[],
            })
            .unwrap()
    }

    fn registry() -> Url {
        Url::parse("http://127.0.0.1:5000/v2/").unwrap()
    }

    fn pull(repo: &str) -> Vec<String> {
        vec![format!("repository:{repo}:pull")]
    }

    /// containerd's TestParseAuthHeaderBearer and TestParseAuthHeader.
    #[test]
    fn challenges_parse_as_containerd_parses_them() {
        for scope in [
            "repository:foo/bar:pull,push",
            "repository:foo/bar:pull,push repository:foo/baz:pull repository:foo/foo:push",
        ] {
            let line = format!(
                r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="{scope}""#
            );
            let want = Challenge {
                scheme: Scheme::Bearer,
                params: params(&[
                    ("realm", "https://auth.docker.io/token"),
                    ("service", "registry.docker.io"),
                    ("scope", scope),
                ]),
            };
            assert_eq!(challenges([line.as_str()]), vec![want]);
        }
        let c = challenges([
            r#"Bearer realm="https://auth.example.io/token",empty="",service="registry.example.io",scope="repository:library/hello-world:pull,push""#,
        ]);
        assert_eq!(c[0].params.get("empty").map(String::as_str), Some(""));
        assert_eq!(
            c[0].params.get("service").map(String::as_str),
            Some("registry.example.io")
        );
    }

    #[test]
    fn challenges_come_best_first_and_parsing_stops_at_a_bad_byte() {
        let c = challenges([
            r#"Basic realm="r""#,
            "Negotiate x",
            r#"Digest realm="d""#,
            "BEARER Realm=\"b\", Service=s",
        ]);
        let schemes: Vec<Scheme> = c.iter().map(|c| c.scheme).collect();
        assert_eq!(schemes, [Scheme::Bearer, Scheme::Digest, Scheme::Basic]);
        assert_eq!(c[0].params, params(&[("realm", "b"), ("service", "s")]));
        let c = challenges([r#"Bearer realm="a\"b\\c",service=svc junk=x"#]);
        assert_eq!(c[0].params, params(&[("realm", r#"a"b\c"#), ("service", "svc")]));
        let c = challenges([r#"Bearer realm="unterminated,service=s"#]);
        assert_eq!(c[0].params, params(&[("realm", "")]));
        // Tokens end at separators, so an unquoted URL is cut at its colon.
        let c = challenges(["Bearer realm=https://auth.example/token,service=s"]);
        assert_eq!(c[0].params, params(&[("realm", "https")]));
    }

    #[test]
    fn anonymous_tokens_come_from_a_get_and_are_cached() {
        let tokens = token_server(&[r#"{"token":"t1","expires_in":300}"#]);
        let auth = Authorizer::new(&registry(), Credentials::Anonymous);
        let challenge = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token?x=1",service="svc",scope="repository:a/b:pull""#,
            tokens.port
        );
        assert!(auth.challenged(&registry(), &refusal(&challenge), false).unwrap());
        let client = plain();
        for _ in 0..2 {
            let value = auth
                .authorization(&client, &registry(), &pull("library/alpine"))
                .unwrap();
            assert_eq!(value.as_deref(), Some("Bearer t1"));
        }
        let requests = tokens.requests();
        assert_eq!(requests.len(), 1, "the second request used the cached token");
        // Go's url.Values.Encode: sorted by key, scopes sorted, the realm's own query kept.
        assert!(
            requests[0].starts_with(
                "GET /token?scope=repository%3Aa%2Fb%3Apull&scope=repository%3Alibrary%2Falpine%3Apull&service=svc&x=1 HTTP/1.1\r\n"
            ),
            "{}",
            requests[0]
        );
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    }

    #[test]
    fn passwords_go_to_oauth2_then_to_a_get_with_basic_authentication() {
        let tokens = serve(
            None,
            vec![
                (http("404 Not Found", "", ""), After::Keep),
                (
                    http("200 OK", "", r#"{"token":"t2","expires_in":60}"#),
                    After::Keep,
                ),
            ],
        );
        let credentials = Credentials::Password {
            username: "u".into(),
            password: "p w".into(),
        };
        let auth = Authorizer::new(&registry(), credentials);
        let challenge = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token",service="svc""#,
            tokens.port
        );
        assert!(auth.challenged(&registry(), &refusal(&challenge), false).unwrap());
        let value = auth.authorization(&plain(), &registry(), &pull("a/b")).unwrap();
        assert_eq!(value.as_deref(), Some("Bearer t2"));
        let requests = tokens.requests();
        assert!(
            requests[0].starts_with("POST /token HTTP/1.1\r\n"),
            "{}",
            requests[0]
        );
        assert!(requests[0].ends_with(
            "\r\n\r\nclient_id=shards&grant_type=password&password=p+w&scope=repository%3Aa%2Fb%3Apull&service=svc&username=u"
        ));
        assert!(
            requests[1].starts_with("GET /token?scope=repository%3Aa%2Fb%3Apull&service=svc HTTP/1.1\r\n")
        );
        assert!(
            requests[1].contains("\r\nAuthorization: Basic dTpwIHc=\r\n"),
            "{}",
            requests[1]
        );
    }

    #[test]
    fn identity_tokens_refresh_and_lifetimes_count_from_issued_at() {
        let tokens = token_server(&[
            r#"{"access_token":"old","expires_in":60,"issued_at":"2020-01-01T00:00:00Z"}"#,
            r#"{"access_token":"new","expires_in":600}"#,
        ]);
        let auth = Authorizer::new(&registry(), Credentials::IdentityToken("idt".into()));
        let challenge = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token",service="svc""#,
            tokens.port
        );
        assert!(auth.challenged(&registry(), &refusal(&challenge), false).unwrap());
        let client = plain();
        let get = || auth.authorization(&client, &registry(), &pull("a/b")).unwrap();
        assert_eq!(get().as_deref(), Some("Bearer old"));
        assert_eq!(
            get().as_deref(),
            Some("Bearer new"),
            "issued in 2020, so long expired"
        );
        assert_eq!(get().as_deref(), Some("Bearer new"));
        let requests = tokens.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].contains("grant_type=refresh_token&refresh_token=idt"),
            "{}",
            requests[0]
        );
    }

    #[test]
    fn token_answers_are_checked() {
        for (answer, why) in [
            (r#"{"expires_in":60}"#, "no token"),
            (r#"{"token":"t","issued_at":"yesterday"}"#, "a bad issued_at"),
            (r#"{"token":"t","expires_in":"60"}"#, "a string expires_in"),
            ("not json", "not JSON"),
        ] {
            let tokens = token_server(&[answer]);
            let auth = Authorizer::new(&registry(), Credentials::Anonymous);
            let challenge = format!(r#"Bearer realm="http://127.0.0.1:{}/token""#, tokens.port);
            auth.challenged(&registry(), &refusal(&challenge), false).unwrap();
            assert!(
                auth.authorization(&plain(), &registry(), &pull("a/b")).is_err(),
                "{why}"
            );
        }
        // An OAuth2 answer needs access_token; `token` alone is a GET's.
        let tokens = token_server(&[r#"{"token":"t"}"#]);
        let auth = Authorizer::new(&registry(), Credentials::IdentityToken("idt".into()));
        let challenge = format!(r#"Bearer realm="http://127.0.0.1:{}/token""#, tokens.port);
        auth.challenged(&registry(), &refusal(&challenge), false).unwrap();
        assert!(auth.authorization(&plain(), &registry(), &pull("a/b")).is_err());
    }

    #[test]
    fn realms_must_keep_the_exchange_on_tls() {
        let bearer = |registry: &str, realm: &str| {
            let c = Challenge {
                scheme: Scheme::Bearer,
                params: params(&[("realm", realm)]),
            };
            Bearer::new(&Url::parse(registry).unwrap(), &c, "u", "p").map(|_| ())
        };
        assert!(bearer("https://registry.example/v2/", "https://auth.example/token").is_ok());
        assert!(bearer("https://registry.example/v2/", "http://auth.example/token").is_err());
        assert!(bearer("http://registry.example/v2/", "http://auth.example/token").is_err());
        assert!(bearer("http://localhost:5000/v2/", "http://localhost:5001/token").is_ok());
        assert!(bearer("http://[::1]:5000/v2/", "http://127.0.0.1/token").is_ok());
        // Credentials never leave the machine unencrypted.
        assert!(bearer("http://localhost:5000/v2/", "http://auth.example/token").is_err());
        assert!(bearer("https://registry.example/v2/", "not a url").is_err());
    }

    #[test]
    fn registry_tokens_go_to_their_registry_alone_and_are_not_retried() {
        let auth = Authorizer::new(&registry(), Credentials::RegistryToken("rt".into()));
        let client = plain();
        let other = Url::parse("https://cdn.example/blob").unwrap();
        assert_eq!(
            auth.authorization(&client, &registry(), &[]).unwrap().as_deref(),
            Some("Bearer rt")
        );
        assert_eq!(auth.authorization(&client, &other, &[]).unwrap(), None);
        assert!(
            !auth
                .challenged(&registry(), &refusal(r#"Bearer realm="https://a/t""#), false)
                .unwrap()
        );
    }

    #[test]
    fn basic_challenges_need_credentials() {
        let refused = refusal(r#"Basic realm="registry""#);
        let credentials = Credentials::Password {
            username: "u".into(),
            password: "p".into(),
        };
        let auth = Authorizer::new(&registry(), credentials);
        assert!(auth.challenged(&registry(), &refused, false).unwrap());
        let value = auth.authorization(&plain(), &registry(), &[]).unwrap();
        assert_eq!(value.as_deref(), Some("Basic dTpw"));
        let anonymous = Authorizer::new(&registry(), Credentials::Anonymous);
        assert!(anonymous.challenged(&registry(), &refused, false).is_err());
        // Credentials belong to their registry: another host asking gets none.
        let other = Url::parse("http://127.0.0.1:6000/v2/").unwrap();
        assert!(auth.challenged(&other, &refused, false).is_err());
    }

    #[test]
    fn refused_tokens_start_over_once() {
        let tokens = token_server(&[r#"{"token":"first"}"#, r#"{"token":"second"}"#]);
        let auth = Authorizer::new(&registry(), Credentials::Anonymous);
        let challenge = format!(r#"Bearer realm="http://127.0.0.1:{}/token""#, tokens.port);
        let client = plain();
        assert!(auth.challenged(&registry(), &refusal(&challenge), false).unwrap());
        let get = || auth.authorization(&client, &registry(), &pull("a/b")).unwrap();
        assert_eq!(get().as_deref(), Some("Bearer first"));
        let invalid = format!(r#"{challenge},error="invalid_token""#);
        assert!(auth.challenged(&registry(), &refusal(&invalid), false).unwrap());
        assert_eq!(get().as_deref(), Some("Bearer second"));
        assert!(
            !auth.challenged(&registry(), &refusal(&invalid), true).unwrap(),
            "refused twice"
        );
    }

    #[test]
    fn credentials_never_show_their_secrets() {
        let shown = format!(
            "{:?} {:?} {:?}",
            Credentials::Password {
                username: "u".into(),
                password: "hunter2".into()
            },
            Credentials::IdentityToken("secret-token".into()),
            Credentials::RegistryToken("secret-token".into()),
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("secret-token"),
            "{shown}"
        );
    }
}
