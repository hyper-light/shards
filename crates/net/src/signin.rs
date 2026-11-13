//! Sign-in profiles (D242; kiln plan 01a1d2b88be57304b972761e2a5c6781 step 8): what Claude Code's
//! and Codex's own sign-ins say to their providers' token endpoints, as their pinned builds say
//! it, so that the injector (inject.rs) can take a token response's tokens outside the microVM,
//! give the CLI placeholders in their place, and answer its refreshes and revocations outside.
//! The profiles are pure: they hold no socket, clock or lease, and their caller holds what they
//! read, within its connection's window. [`SignIns`] is the run's sign-ins as the network
//! process holds them, and its asks of the run host (`NET_SIGN_IN`), which never wait.
//!
//! Each shape is cited to the CLI's own build, read and never run: Claude Code 2.1.311's bundle
//! (linux-x64, sha256 86dc1fcc…8ada; offsets into that file) and Codex rust-v0.167.2's sources
//! (commit ce21072c9; paths under codex-rs). The reads are docs/research/s33-veil-creds/STEP8.md's.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use zeroize::{Zeroize, Zeroizing};

pub use crate::dns::credential::{Captured, Cli, SignInGrant};

use crate::dns::credential;
use crate::inject::{self, Field};
use crate::lease::Waiter;

/// `cli`'s profile.
pub fn profile(cli: Cli) -> &'static Profile {
    match cli {
        Cli::ClaudeCode => &CLAUDE_CODE,
        Cli::Codex => &CODEX,
    }
}

/// An endpoint a profile names: its host, lowered, and its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub host: &'static str,
    pub path: &'static str,
}

/// A CLI's sign-in, as its pinned build makes it.
#[derive(Debug)]
pub struct Profile {
    pub cli: Cli,
    /// Where its code is exchanged and its tokens refreshed (RFC 6749 §3.2, §6).
    pub token: Endpoint,
    /// Where it revokes a token (RFC 7009 §2.1), on its token endpoint's host.
    pub revoke: Endpoint,
    /// Where its access token mints a credential that the response carries in the clear: its
    /// access token is never filled in toward these, as what came back would be a credential
    /// in the microVM.
    pub mints: &'static [Endpoint],
}

/// Claude Code 2.1.311. Its OAuth config (`on()`, offset 238938203) names
/// `TOKEN_URL:"https://platform.claude.com/v1/oauth/token"`, where it exchanges its pasted code
/// (`c9o`, 242102662: JSON `grant_type` `authorization_code`, `code`, `redirect_uri`,
/// `client_id`, `code_verifier`, `state`) and refreshes (242103577: JSON `grant_type`
/// `refresh_token`, `refresh_token`, `client_id`, `scope`), reading `access_token`,
/// `refresh_token` (its old one kept where none comes), `expires_in`, `refresh_token_expires_in`
/// and `scope` (243341777, 242103577); its revocation is `${TOKEN_URL}/revoke` (`i3r`,
/// 242106079: JSON `token`, `token_type_hint` `refresh_token`, `client_id`). It asks no `openid`
/// scope (`deo()`, 238937878) and reads no `id_token`. `API_KEY_URL`
/// (`https://api.anthropic.com/api/oauth/claude_cli/create_api_key`) answers its bearer token's
/// POST with `raw_key`, an API key (`u9o`, 242107148), and its default scopes ask
/// `org:create_api_key` (238937512).
pub static CLAUDE_CODE: Profile = Profile {
    cli: Cli::ClaudeCode,
    token: Endpoint {
        host: "platform.claude.com",
        path: "/v1/oauth/token",
    },
    revoke: Endpoint {
        host: "platform.claude.com",
        path: "/v1/oauth/token/revoke",
    },
    mints: &[Endpoint {
        host: "api.anthropic.com",
        path: "/api/oauth/claude_cli/create_api_key",
    }],
};

/// Codex rust-v0.167.2. `REFRESH_TOKEN_URL` is `https://auth.openai.com/oauth/token`
/// (login/src/auth/manager.rs:230), where it exchanges a code in form encoding (the device
/// flow's too: login/src/oauth/client.rs:79-95, device_code_auth.rs:206-217) and refreshes in
/// JSON (`grant_type`, `client_id`, `refresh_token`, and `resource` and `refresh_reason` where
/// set: client.rs:97-113, 132-134), reading `id_token`, `access_token` and `refresh_token`
/// (server.rs:701-707, manager.rs:1855-1860). `REVOKE_TOKEN_URL` is
/// `https://auth.openai.com/oauth/revoke` (manager.rs:231), its body JSON `token`,
/// `token_type_hint` and, for a refresh token, `client_id` (auth/revoke.rs:47-53, 104-117): its
/// refresh token where it holds one, else its access token (revoke.rs:68-77).
pub static CODEX: Profile = Profile {
    cli: Cli::Codex,
    token: Endpoint {
        host: "auth.openai.com",
        path: "/oauth/token",
    },
    revoke: Endpoint {
        host: "auth.openai.com",
        path: "/oauth/revoke",
    },
    mints: &[],
};

impl Profile {
    /// Whether a request to `host` for `target` reaches an endpoint that mints a credential
    /// (`mints`). A server reads its path in ways this side cannot know, so the path is read
    /// widely: percent-decoded, its empty and dot segments gone (RFC 3986 §5.2.4), lowered, and
    /// anything under a minting path counts as it.
    pub fn mints(&self, host: &str, target: &str) -> bool {
        let path = loose(path(target));
        self.mints.iter().any(|e| {
            e.host.eq_ignore_ascii_case(host)
                && (path == e.path
                    || path
                        .strip_prefix(e.path)
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    }
}

/// A request target's path (RFC 9112 §3.2): an origin-form target's before its query, an
/// absolute-form target's after its authority.
pub(crate) fn path(target: &str) -> &str {
    let rest = match target.split_once("://") {
        Some((_, rest)) => rest
            .find(['/', '?', '#'])
            .and_then(|i| rest.get(i..))
            .unwrap_or(""),
        None => target,
    };
    rest.split(['?', '#']).next().unwrap_or(rest)
}

/// `path` read as widely as a server may read it: each `%XX` decoded, empty and `.` segments
/// dropped, `..` taking the segment before it, then lowered.
fn loose(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b, hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                i += 3;
            }
            _ => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    let decoded = String::from_utf8_lossy(&decoded).to_ascii_lowercase();
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    format!("/{}", segments.join("/"))
}

/// A sign-in's placeholders, as its capture gave them: what its CLI holds in its tokens'
/// places, none for a refresh token its token response did not give. Holding one is the
/// capability, so none prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Placeholders {
    pub access: String,
    pub refresh: Option<String>,
}

impl fmt::Debug for Placeholders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Placeholders")
            .field("access", &format_args!("{} bytes", self.access.len()))
            .field(
                "refresh",
                &self.refresh.as_ref().map(|r| format!("{} bytes", r.len())),
            )
            .finish()
    }
}

/// A request to a sign-in's token host, as the injector read it: its head's parts and its body,
/// whole. Its body may carry a placeholder, so it never prints.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub host: &'a str,
    pub method: &'a str,
    pub target: &'a str,
    pub content_type: Option<&'a [u8]>,
    pub body: &'a [u8],
}

impl fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("host", &self.host)
            .field("method", &self.method)
            .field("path", &path(self.target))
            .field("body", &format_args!("{} bytes", self.body.len()))
            .finish()
    }
}

/// What a request to a sign-in's token host asks (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// A refresh (RFC 6749 §6) whose refresh token is the placeholder of the sign-in at this
    /// index: answered outside, never sent on, as the one holder of the sign-in's refresh token
    /// refreshes it for every run (RFC 6749 §6, f47).
    Refresh(usize),
    /// A revocation (RFC 7009 §2.1) of a placeholder of the sign-in at this index: answered
    /// outside, its stored tokens untouched, so the VM's logout never ends the sign-in it was
    /// given (f55).
    Revoke(usize),
    /// Anything else: sent on as it came, its response read for tokens ([`read`]).
    Pass,
}

/// What `request` asks of `profile`'s token host, `signins` being the run's sign-ins'
/// placeholders. A request whose fields this side cannot read, or which names one twice,
/// passes: it carries no value of the run's, and its response is read whatever it asked.
pub fn ask(profile: &Profile, request: &Request<'_>, signins: &[&Placeholders]) -> Ask {
    if !request.host.eq_ignore_ascii_case(profile.token.host) || !request.method.eq_ignore_ascii_case("POST")
    {
        return Ask::Pass;
    }
    let at = path(request.target);
    if at == profile.token.path {
        let Some([Some(grant), Some(token)]) =
            fields_of(request, [&b"grant_type"[..], &b"refresh_token"[..]])
        else {
            return Ask::Pass;
        };
        if grant.as_slice() != b"refresh_token" {
            return Ask::Pass;
        }
        return signins
            .iter()
            .position(|s| s.refresh.as_ref().is_some_and(|r| same(r.as_bytes(), &token)))
            .map_or(Ask::Pass, Ask::Refresh);
    }
    if at == profile.revoke.path {
        let Some([Some(token)]) = fields_of(request, [&b"token"[..]]) else {
            return Ask::Pass;
        };
        return signins
            .iter()
            .position(|s| {
                s.refresh.as_ref().is_some_and(|r| same(r.as_bytes(), &token))
                    || same(s.access.as_bytes(), &token)
            })
            .map_or(Ask::Pass, Ask::Revoke);
    }
    Ask::Pass
}

/// Whether `a` and `b` are the same bytes, every byte of equal lengths compared, as
/// `credential::decide` compares a placeholder: what stands there is its sender's to choose.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |differ, (x, y)| differ | (x ^ y)) == 0
}

/// A request body's top-level fields named `names`, read by its Content-Type: a JSON object's
/// string members (RFC 8259) or a form's pairs (`application/x-www-form-urlencoded`, WHATWG URL
/// §5.1), each wiped as it goes. None where the body is neither, or names one of them twice.
fn fields_of<const N: usize>(
    request: &Request<'_>,
    names: [&[u8]; N],
) -> Option<[Option<Zeroizing<Vec<u8>>>; N]> {
    let media = request
        .content_type?
        .split(|&b| b == b';')
        .next()
        .map(<[u8]>::trim_ascii)?;
    let mut found: [Option<Zeroizing<Vec<u8>>>; N] = std::array::from_fn(|_| None);
    let mut take = |name: &[u8], value: Zeroizing<Vec<u8>>| -> Option<()> {
        if let Some(slot) = names
            .iter()
            .position(|n| *n == name)
            .and_then(|i| found.get_mut(i))
        {
            if slot.is_some() {
                return None;
            }
            *slot = Some(value);
        }
        Some(())
    };
    if media.eq_ignore_ascii_case(b"application/json") {
        let json = json(request.body).ok()?;
        for m in json.members.iter().filter(|m| m.depth == 1) {
            let value = if m.string {
                unescape(request.body, &m.value).ok()?
            } else {
                Zeroizing::new(Vec::new())
            };
            take(m.name.as_slice(), value)?;
        }
    } else if media.eq_ignore_ascii_case(b"application/x-www-form-urlencoded") {
        for pair in request.body.split(|&b| b == b'&').filter(|p| !p.is_empty()) {
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(i) => (pair.get(..i)?, pair.get(i + 1..)?),
                None => (pair, &b""[..]),
            };
            take(form_decode(name).as_slice(), form_decode(value))?;
        }
    } else {
        return None;
    }
    Some(found)
}

/// A form's name or value decoded (WHATWG URL §5.1): `+` a space, each `%XX` its byte.
fn form_decode(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b, hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Why a sign-in's token response is not passed on (R04): the injector answers the guest 502
/// for each, as a response it cannot read or write could carry a token into the microVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its head is no HTTP/1.1 response's this side reads.
    Head,
    /// Its body is coded (a content coding other than `identity`, or a transfer coding other
    /// than `chunked`), so its tokens could not be found.
    Coded,
    /// Its body is no JSON text (RFC 8259), so it could not be read for tokens.
    NotJson,
    /// A token's name where its CLI reads none: in a nested object, twice, or not a string.
    Misplaced,
    /// A token's value elsewhere in the body than its own place.
    Echoed,
    /// An ID token that is no JWS compact serialization (RFC 7515 §7.1).
    NotAJwt,
    /// A placeholder that is no token a JSON string holds as it is (RFC 6750 §2.1's
    /// `b64token`).
    NoPlaceholder,
    /// A token response its sign-in needs an access token from that carries none.
    NoToken,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::Head => "a sign-in's response whose head this side cannot read",
            Refusal::Coded => "a sign-in's response whose body is coded, so its tokens cannot be found",
            Refusal::NotJson => "a sign-in's response that is no JSON text, so its tokens cannot be found",
            Refusal::Misplaced => "a sign-in's response with a token where its CLI reads none",
            Refusal::Echoed => "a sign-in's response that carries a token's value outside its place",
            Refusal::NotAJwt => "a sign-in's response whose ID token is no JWT",
            Refusal::NoPlaceholder => "a sign-in's placeholder that is no token",
            Refusal::NoToken => "a sign-in's token response that carries no access token",
        })
    }
}

/// Whether a response's body is as its sender wrote it, so that its tokens can be read where they
/// lie: no content coding but `identity` (RFC 9110 §8.4), no transfer coding but `chunked`
/// (RFC 9112 §6.1), whose framing the injector reads.
pub fn readable(head: &[u8]) -> Result<(), Refusal> {
    let fields = response_fields(head).ok_or(Refusal::Head)?;
    readable_fields(
        fields
            .iter()
            .map(|f| (f.name.as_bytes(), head.get(f.value.clone()).unwrap_or_default())),
    )
}

/// `readable`, of a response's fields as names and values (an HTTP/2 response's, decoded).
pub fn readable_fields<'f>(fields: impl IntoIterator<Item = (&'f [u8], &'f [u8])>) -> Result<(), Refusal> {
    for (name, value) in fields {
        let only = |coding: &[u8]| inject::list(value).all(|c| c.eq_ignore_ascii_case(coding));
        if (name.eq_ignore_ascii_case(b"content-encoding") && !only(b"identity"))
            || (name.eq_ignore_ascii_case(b"transfer-encoding") && !only(b"chunked"))
        {
            return Err(Refusal::Coded);
        }
    }
    Ok(())
}

/// A response head for a body of `len` bytes (RFC 9112 §6.3, f43): its Content-Length and
/// Transfer-Encoding fields cut and one Content-Length of `len` written, the rest as it came,
/// as a sender must not forward a Content-Length it knows to be wrong (RFC 9110 §8.6).
pub fn frame(head: &[u8], len: usize) -> Result<Vec<u8>, Refusal> {
    let fields = response_fields(head).ok_or(Refusal::Head)?;
    let end = head.len().checked_sub(2).ok_or(Refusal::Head)?;
    let mut out = Vec::with_capacity(head.len().saturating_add(40));
    let mut at = 0usize;
    for f in fields
        .iter()
        .filter(|f| f.name == "content-length" || f.name == "transfer-encoding")
    {
        out.extend_from_slice(head.get(at..f.line.start).ok_or(Refusal::Head)?);
        at = f.line.end;
    }
    out.extend_from_slice(head.get(at..end).ok_or(Refusal::Head)?);
    out.extend_from_slice(format!("Content-Length: {len}\r\n\r\n").as_bytes());
    Ok(out)
}

/// A whole response head's fields, where each lies.
fn response_fields(head: &[u8]) -> Option<Vec<Field>> {
    let lines = head.iter().filter(|&&b| b == b'\n').count();
    let mut read = vec![httparse::EMPTY_HEADER; lines];
    let mut response = httparse::Response::new(&mut read);
    match response.parse(head) {
        Ok(httparse::Status::Complete(n)) if n == head.len() => inject::fields(head, response.headers),
        _ => None,
    }
}

/// A token response's tokens (RFC 6749 §5.1; OpenID Connect Core §3.1.3.3's `id_token`), each
/// held wiped where it was read, the access token's life, and the scope granted.
#[derive(Default)]
pub struct Tokens {
    pub access: Option<Token>,
    pub refresh: Option<Token>,
    pub id: Option<Token>,
    /// Its `expires_in`: the access token's life in seconds (RFC 6749 §5.1), where it is one
    /// whole number.
    pub expires_in: Option<u64>,
    /// Its `scope`: the scope its access token was granted (RFC 6749 §5.1, §3.3), where it is
    /// one string. No credential: what a CLI's own file records beside its tokens (Claude
    /// Code's `scopes`), which its orchestrator stages into a later run's.
    pub scope: Option<String>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access", &self.access.is_some())
            .field("refresh", &self.refresh.is_some())
            .field("id", &self.id.is_some())
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// A token's value, and where its JSON string lies in its body.
pub struct Token {
    pub value: Zeroizing<Vec<u8>>,
    at: Range<usize>,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token({} bytes)", self.value.len())
    }
}

const ACCESS: &[u8] = b"access_token";
const REFRESH: &[u8] = b"refresh_token";
const ID: &[u8] = b"id_token";
const EXPIRES_IN: &[u8] = b"expires_in";
const SCOPE: &[u8] = b"scope";

/// What a response from a sign-in's token host carries, read from its whole body, as it came
/// (identity-coded, its chunks joined): none where it names no token (an error, a device
/// flow's code: passed on as it came); its tokens where its outermost object names them, each
/// a string once. Refused: a body that is no JSON text, a token's name anywhere else, and a
/// token's value in any other string, its escapes decoded, where only its own place is
/// replaced.
pub fn read(body: &[u8]) -> Result<Option<Tokens>, Refusal> {
    if body.trim_ascii().is_empty() {
        return Ok(None);
    }
    let json = json(body).map_err(|NotJson| Refusal::NotJson)?;
    let mut tokens = Tokens::default();
    for m in &json.members {
        if m.depth == 1 && m.name.as_slice() == EXPIRES_IN {
            tokens.expires_in = tokens
                .expires_in
                .or_else(|| seconds(body.get(m.value.clone()).unwrap_or_default()));
            continue;
        }
        if m.depth == 1 && m.string && m.name.as_slice() == SCOPE && tokens.scope.is_none() {
            let scope = unescape(body, &m.value).map_err(|NotJson| Refusal::NotJson)?;
            tokens.scope = String::from_utf8(scope.to_vec()).ok();
            continue;
        }
        let slot = match m.name.as_slice() {
            ACCESS => &mut tokens.access,
            REFRESH => &mut tokens.refresh,
            ID => &mut tokens.id,
            _ => continue,
        };
        if m.depth != 1 || !m.string || slot.is_some() {
            return Err(Refusal::Misplaced);
        }
        let value = unescape(body, &m.value).map_err(|NotJson| Refusal::NotJson)?;
        *slot = Some(Token {
            value,
            at: m.value.clone(),
        });
    }
    {
        let read: Vec<&Token> = [&tokens.access, &tokens.refresh, &tokens.id]
            .into_iter()
            .flatten()
            .collect();
        if read.is_empty() {
            return Ok(None);
        }
        for r in json.strings.iter().filter(|r| !read.iter().any(|t| t.at == **r)) {
            let s = unescape(body, r).map_err(|NotJson| Refusal::NotJson)?;
            if read.iter().any(|t| contains(&s, &t.value)) {
                return Err(Refusal::Echoed);
            }
        }
    }
    Ok(Some(tokens))
}

/// A whole number of seconds, as `expires_in` is (RFC 6749 §5.1).
fn seconds(v: &[u8]) -> Option<u64> {
    if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(v).ok()?.parse().ok()
}

/// Whether `needle` lies in `hay` (Knuth–Morris–Pratt, as the injector's echo mask seeks).
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let table = inject::failure(needle);
    let mut m = 0usize;
    hay.iter().any(|&b| {
        m = inject::step(needle, &table, m, b);
        m == needle.len()
    })
}

/// `body` with its tokens' places given `with`'s placeholders and its ID token made inert
/// ([`inert`]); every other byte as it came. Made in one buffer sized at once and wiped as it
/// drops, so that no buffer let go of as it grows holds a part of a token.
pub fn write(body: &[u8], tokens: &Tokens, with: &Placeholders) -> Result<Zeroizing<Vec<u8>>, Refusal> {
    // A b64token holds no quote, backslash or control, so a JSON string holds it as it is.
    if !credential::placeholder(&with.access)
        || with.refresh.as_ref().is_some_and(|r| !credential::placeholder(r))
    {
        return Err(Refusal::NoPlaceholder);
    }
    let mut edits: Vec<(Range<usize>, Zeroizing<Vec<u8>>)> = Vec::new();
    if let Some(t) = &tokens.access {
        edits.push((t.at.clone(), quoted(with.access.as_bytes())));
    }
    if let Some(t) = &tokens.refresh {
        // A refresh token no placeholder stands for never reaches the guest.
        let refresh = with.refresh.as_ref().ok_or(Refusal::NoPlaceholder)?;
        edits.push((t.at.clone(), quoted(refresh.as_bytes())));
    }
    if let Some(t) = &tokens.id {
        edits.push((t.at.clone(), quoted(&inert(&t.value)?)));
    }
    edits.sort_by_key(|(r, _)| r.start);
    let len = edits.iter().fold(body.len(), |len, (r, v)| {
        len.saturating_sub(r.len()).saturating_add(v.len())
    });
    let mut out = Zeroizing::new(Vec::with_capacity(len));
    let mut at = 0usize;
    for (r, v) in &edits {
        out.extend_from_slice(body.get(at..r.start).ok_or(Refusal::Misplaced)?);
        out.extend_from_slice(v);
        at = r.end;
    }
    out.extend_from_slice(body.get(at..).unwrap_or_default());
    Ok(out)
}

/// Appends `bytes` to `buf`, which holds a token response as it comes: where it has no room, a
/// buffer of twice its size takes its bytes and the old one is wiped as it drops, so that no
/// buffer let go of holds a part of a token.
pub fn append(buf: &mut Zeroizing<Vec<u8>>, bytes: &[u8]) {
    let need = buf.len().saturating_add(bytes.len());
    if need > buf.capacity() {
        let mut grown = Zeroizing::new(Vec::with_capacity(need.max(buf.capacity().saturating_mul(2))));
        grown.extend_from_slice(buf);
        *buf = grown;
    }
    buf.extend_from_slice(bytes);
}

/// `v` as a JSON string, which it holds as it is.
fn quoted(v: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(v.len().saturating_add(2)));
    out.push(b'"');
    out.extend_from_slice(v);
    out.push(b'"');
    out
}

/// What stands in an ID token's signature's place (D242, step 8's question 4): never a signature,
/// so the token it ends is no credential, and filled in nowhere.
const NO_SIGNATURE: &[u8] = b"shards-placeholder";

/// An ID token made inert (D242, step 8's question 4): its header and payload as they came, and
/// [`NO_SIGNATURE`] in its signature's place. Codex reads an ID token's payload alone
/// (login/src/token_data.rs:144-155: three parts, none empty, the payload decoded, the signature
/// never), and its browser login exchanges a real one at its token endpoint for an API key
/// (server.rs:446-448, 1036-1057), which any holder of one could ask; one whose signature is not
/// its issuer's is no credential there. Refused: a token that is no JWS compact serialization
/// (RFC 7515 §7.1: three base64url parts).
fn inert(jwt: &[u8]) -> Result<Zeroizing<Vec<u8>>, Refusal> {
    let mut parts = jwt.split(|&b| b == b'.');
    let (Some(header), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::NotAJwt);
    };
    let base64url = |p: &[u8]| {
        !p.is_empty()
            && p.iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !base64url(header) || !base64url(payload) {
        return Err(Refusal::NotAJwt);
    }
    let mut out = Zeroizing::new(Vec::with_capacity(
        header
            .len()
            .saturating_add(payload.len())
            .saturating_add(NO_SIGNATURE.len())
            .saturating_add(2),
    ));
    out.extend_from_slice(header);
    out.push(b'.');
    out.extend_from_slice(payload);
    out.push(b'.');
    out.extend_from_slice(NO_SIGNATURE);
    Ok(out)
}

/// The answer to a refresh answered outside: `body`, a token response whose tokens are
/// placeholders, kept from every cache as a response carrying tokens is (RFC 6749 §5.1).
pub fn refreshed(body: &[u8]) -> Zeroizing<Vec<u8>> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nPragma: no-cache\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut out = Zeroizing::new(Vec::with_capacity(head.len().saturating_add(body.len())));
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
    out
}

/// The answer to a revocation answered outside: 200, which a server answers to a token it
/// revoked or never knew, and whose body its client ignores (RFC 7009 §2.2).
pub const REVOKED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nCache-Control: no-store\r\n\r\n";

/// A JSON text's strings and its objects' members at every depth, where each lies (RFC 8259).
#[derive(Default)]
struct Json {
    members: Vec<Member>,
    /// Every string, names too, its quotes included.
    strings: Vec<Range<usize>>,
}

/// An object's member: its depth (1 for the outermost object's), its name, and its value's place.
struct Member {
    depth: usize,
    name: Zeroizing<Vec<u8>>,
    value: Range<usize>,
    string: bool,
}

/// A member's name, read before its value.
struct Named {
    depth: usize,
    name: Zeroizing<Vec<u8>>,
}

/// An array or object begun and not yet ended.
struct Open {
    object: bool,
    start: usize,
    member: Option<Named>,
}

/// Bytes that are not one JSON text (RFC 8259 §2).
#[derive(Debug)]
struct NotJson;

impl Json {
    fn member(&mut self, named: Option<Named>, value: Range<usize>, string: bool) {
        if let Some(Named { depth, name }) = named {
            self.members.push(Member {
                depth,
                name,
                value,
                string,
            });
        }
    }
}

/// `text` read as one JSON text (RFC 8259 §2: one value between whitespace), its arrays and
/// objects followed with a stack of its own rather than by recursion, so that no depth of
/// nesting reaches the thread's stack.
fn json(text: &[u8]) -> Result<Json, NotJson> {
    let mut s = Scan { text, at: 0 };
    let mut out = Json::default();
    let mut open: Vec<Open> = Vec::new();
    let mut named: Option<Named> = None;
    loop {
        s.ws();
        let start = s.at;
        match s.peek().ok_or(NotJson)? {
            b @ (b'{' | b'[') => {
                let object = b == b'{';
                s.at += 1;
                open.push(Open {
                    object,
                    start,
                    member: named.take(),
                });
                s.ws();
                if !s.close(object) {
                    if object {
                        named = Some(s.name(open.len(), &mut out)?);
                    }
                    continue;
                }
                let o = open.pop().ok_or(NotJson)?;
                out.member(o.member, o.start..s.at, false);
            }
            b'"' => {
                let r = s.string()?;
                out.strings.push(r.clone());
                out.member(named.take(), r, true);
            }
            b'-' | b'0'..=b'9' => {
                s.number()?;
                out.member(named.take(), start..s.at, false);
            }
            _ => {
                s.literal()?;
                out.member(named.take(), start..s.at, false);
            }
        }
        // After a value: the next member or element, or its container's end.
        loop {
            s.ws();
            let Some(object) = open.last().map(|o| o.object) else {
                return if s.at == text.len() { Ok(out) } else { Err(NotJson) };
            };
            if s.eat(b',') {
                if object {
                    s.ws();
                    named = Some(s.name(open.len(), &mut out)?);
                }
                break;
            }
            if !s.close(object) {
                return Err(NotJson);
            }
            let o = open.pop().ok_or(NotJson)?;
            out.member(o.member, o.start..s.at, false);
        }
    }
}

/// A JSON text, read from `at`.
struct Scan<'a> {
    text: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        let hit = self.peek() == Some(b);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn close(&mut self, object: bool) -> bool {
        self.eat(if object { b'}' } else { b']' })
    }

    /// RFC 8259 §2's whitespace: space, tab, LF and CR alone.
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn digits(&mut self) -> bool {
        let from = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        self.at > from
    }

    /// A string (RFC 8259 §7), where it lies with its quotes: its escapes one of §7's, no control
    /// character unescaped.
    fn string(&mut self) -> Result<Range<usize>, NotJson> {
        let start = self.at;
        if !self.eat(b'"') {
            return Err(NotJson);
        }
        loop {
            match self.peek().ok_or(NotJson)? {
                b'"' => {
                    self.at += 1;
                    return Ok(start..self.at);
                }
                b'\\' => {
                    self.at += 1;
                    match self.peek().ok_or(NotJson)? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.at += 1,
                        b'u' => {
                            self.at += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                                    return Err(NotJson);
                                }
                                self.at += 1;
                            }
                        }
                        _ => return Err(NotJson),
                    }
                }
                0x00..=0x1f => return Err(NotJson),
                _ => self.at += 1,
            }
        }
    }

    /// A number (RFC 8259 §6): no leading zero, digits after a point and an exponent.
    fn number(&mut self) -> Result<(), NotJson> {
        self.eat(b'-');
        if !self.eat(b'0') && !self.digits() {
            return Err(NotJson);
        }
        if self.eat(b'.') && !self.digits() {
            return Err(NotJson);
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _sign = self.eat(b'+') || self.eat(b'-');
            if !self.digits() {
                return Err(NotJson);
            }
        }
        Ok(())
    }

    fn literal(&mut self) -> Result<(), NotJson> {
        let rest = self.text.get(self.at..).unwrap_or_default();
        let word = [&b"true"[..], b"false", b"null"]
            .into_iter()
            .find(|w| rest.starts_with(w))
            .ok_or(NotJson)?;
        self.at += word.len();
        Ok(())
    }

    /// A member's name and its colon.
    fn name(&mut self, depth: usize, out: &mut Json) -> Result<Named, NotJson> {
        let r = self.string()?;
        out.strings.push(r.clone());
        let name = unescape(self.text, &r)?;
        self.ws();
        if !self.eat(b':') {
            return Err(NotJson);
        }
        Ok(Named { depth, name })
    }
}

/// A string's value (RFC 8259 §7), its escapes decoded into a buffer sized at once (no escape
/// decodes longer than it is written) and wiped as it drops. A lone surrogate is refused.
fn unescape(text: &[u8], string: &Range<usize>) -> Result<Zeroizing<Vec<u8>>, NotJson> {
    let inner = text
        .get(string.start.saturating_add(1)..string.end.saturating_sub(1))
        .ok_or(NotJson)?;
    let mut out = Zeroizing::new(Vec::with_capacity(inner.len()));
    let mut i = 0usize;
    while let Some(&b) = inner.get(i) {
        i += 1;
        if b != b'\\' {
            out.push(b);
            continue;
        }
        let e = inner.get(i).copied().ok_or(NotJson)?;
        i += 1;
        let byte = match e {
            b'"' | b'\\' | b'/' => e,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let (c, used) = code_point(inner.get(i..).unwrap_or_default())?;
                i += used;
                let mut utf8 = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                utf8.zeroize();
                continue;
            }
            _ => return Err(NotJson),
        };
        out.push(byte);
    }
    Ok(out)
}

/// The character a `\u` escape's hex begins (RFC 8259 §7: a character past the Basic
/// Multilingual Plane as its surrogate pair), and how many bytes it took.
fn code_point(rest: &[u8]) -> Result<(char, usize), NotJson> {
    let hex = |at: usize| -> Option<u32> {
        rest.get(at..at + 4)?
            .iter()
            .try_fold(0u32, |n, &d| Some(n * 16 + char::from(d).to_digit(16)?))
    };
    let high = hex(0).ok_or(NotJson)?;
    let (c, used) = match high {
        0xD800..=0xDBFF => {
            let low = match rest.get(4..6) {
                Some(b"\\u") => hex(6).ok_or(NotJson)?,
                _ => return Err(NotJson),
            };
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(NotJson);
            }
            (0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00), 10)
        }
        0xDC00..=0xDFFF => return Err(NotJson),
        c => (c, 4),
    };
    Ok((char::from_u32(c).ok_or(NotJson)?, used))
}

/// A length of `n` bytes in a field of `W` bytes, network order; none past it.
fn length<const W: usize>(n: usize) -> Option<[u8; W]> {
    let n = u64::try_from(n).ok()?;
    let bytes = n.to_be_bytes();
    let (high, low) = bytes.split_at_checked(8usize.checked_sub(W)?)?;
    if high.iter().any(|&b| b != 0) {
        return None;
    }
    low.try_into().ok()
}

/// Appends `bytes` after their length in a field of `W` bytes.
fn put<const W: usize>(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    out.extend_from_slice(&length::<W>(bytes.len())?);
    out.extend_from_slice(bytes);
    Some(())
}

/// Bytes read in order, a read past their end refused.
struct Read<'a>(&'a [u8]);

impl<'a> Read<'a> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(taken)
    }

    fn number<const W: usize>(&mut self) -> Option<u64> {
        let mut be = [0u8; 8];
        be.get_mut(8usize.checked_sub(W)?..)?
            .copy_from_slice(self.take(W)?);
        Some(u64::from_be_bytes(be))
    }

    fn sized<const W: usize>(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.number::<W>()?).ok()?;
        self.take(n)
    }

    fn text(&mut self) -> Option<String> {
        String::from_utf8(self.sized::<2>()?.to_vec()).ok()
    }

    /// What [`put_captured`] wrote.
    fn captured(&mut self) -> Option<Captured> {
        let access = self.text()?;
        let access_placeholder = self.text()?;
        let refresh_placeholder = match self.byte()? {
            0 => None,
            1 => Some(self.text()?),
            _ => return None,
        };
        Some(Captured {
            access,
            access_placeholder,
            refresh_placeholder,
        })
    }

    /// A value, copied once into a buffer that wipes it as it drops.
    fn secret(&mut self) -> Option<Zeroizing<Vec<u8>>> {
        Some(Zeroizing::new(self.sized::<4>()?.to_vec()))
    }

    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

/// `NET_SIGN_INS`'s payload: each grant as its sign-in's name (a two-byte length, network order,
/// and the name), its CLI's code (a byte), its hosts' count (two bytes) and each host (its
/// length's byte and the name), then 1 where an earlier capture gave it placeholders, which
/// follow (its access token's reference and that token's placeholder, each a two-byte length and
/// its bytes, then 1 and its refresh token's placeholder alike, or 0 where it has none), else 0.
/// None where a length passes its field, which the bounds `encode_grants` (lib.rs) cites leave
/// room for.
pub fn encode_grants(grants: &[SignInGrant]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for g in grants {
        put::<2>(&mut out, g.sign_in.as_bytes())?;
        out.push(g.cli.code());
        out.extend_from_slice(&length::<2>(g.toward.len())?);
        for host in &g.toward {
            put::<1>(&mut out, host.as_bytes())?;
        }
        match &g.captured {
            Some(c) => {
                out.push(1);
                put_captured(&mut out, c)?;
            }
            None => out.push(0),
        }
    }
    Some(out)
}

/// A capture's reference and placeholders, as [`encode_grants`] and [`Answered`] write them.
fn put_captured(out: &mut Vec<u8>, c: &Captured) -> Option<()> {
    put::<2>(out, c.access.as_bytes())?;
    put::<2>(out, c.access_placeholder.as_bytes())?;
    match &c.refresh_placeholder {
        Some(refresh) => {
            out.push(1);
            put::<2>(out, refresh.as_bytes())
        }
        None => {
            out.push(0);
            Some(())
        }
    }
}

/// What [`encode_grants`] wrote; none for a malformed payload, a CLI no profile serves, a grant
/// of no name or toward no host, or two of one sign-in or one CLI: a capture names no sign-in,
/// so a run's sign-in of a CLI is one.
pub fn decode_grants(bytes: &[u8]) -> Option<Vec<SignInGrant>> {
    let mut r = Read(bytes);
    let mut grants: Vec<SignInGrant> = Vec::new();
    while !r.done() {
        let sign_in = r.text()?;
        let cli = Cli::from_code(r.byte()?)?;
        let mut toward = Vec::new();
        for _ in 0..r.number::<2>()? {
            toward.push(String::from_utf8(r.sized::<1>()?.to_vec()).ok()?);
        }
        let captured = match r.byte()? {
            0 => None,
            1 => Some(r.captured()?),
            _ => return None,
        };
        if sign_in.is_empty()
            || toward.is_empty()
            || grants.iter().any(|g| g.cli == cli || g.sign_in == sign_in)
        {
            return None;
        }
        grants.push(SignInGrant {
            sign_in,
            cli,
            toward,
            captured,
        });
    }
    Some(grants)
}

/// A sign-in's tokens as its capture took them (D242, step 8), each wiped as it drops: what the
/// network process tells its run host for the orchestrator's broker to keep. Its ID token rides
/// inert alone ([`inert`], as its CLI is given it), no credential: the identity its CLI's own file
/// records (Codex's `auth.json` parses it for its account), which its orchestrator stages into a
/// later run's; the real one stays nowhere.
pub struct Taken {
    pub access: Zeroizing<Vec<u8>>,
    pub refresh: Option<Zeroizing<Vec<u8>>>,
    /// The access token's life in seconds, where its response said (RFC 6749 §5.1).
    pub expires_in: Option<u64>,
    /// Its ID token made inert, where its response had one.
    pub id_token: Option<Zeroizing<Vec<u8>>>,
    /// The scope its access token was granted, where its response said (RFC 6749 §5.1).
    pub scope: Option<String>,
}

impl fmt::Debug for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Taken")
            .field("access", &format_args!("{} bytes", self.access.len()))
            .field("refresh", &self.refresh.as_ref().map(|r| r.len()))
            .field("expires_in", &self.expires_in)
            .field("id_token", &self.id_token.as_ref().map(|t| t.len()))
            .field("scope", &self.scope)
            .finish()
    }
}

impl Taken {
    /// The tokens [`read`] found, copied once each, its ID token made inert: refused where it
    /// has no access token, or an ID token that is no JWT.
    pub fn of(tokens: &Tokens) -> Result<Taken, Refusal> {
        Ok(Taken {
            access: Zeroizing::new(tokens.access.as_ref().ok_or(Refusal::NoToken)?.value.to_vec()),
            refresh: tokens.refresh.as_ref().map(|t| Zeroizing::new(t.value.to_vec())),
            expires_in: tokens.expires_in,
            id_token: tokens.id.as_ref().map(|t| inert(&t.value)).transpose()?,
            scope: tokens.scope.clone(),
        })
    }
}

/// Why an ask of a sign-in was not answered (R04), as its run host said it, each cause typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declined {
    /// The orchestrator's broker refused it, in its words: a sign-in its store does not hold, or
    /// a refresh its provider refused (its refresh token revoked or expired).
    Denied(String),
    /// Its broker could not be asked, or failed, in its words: a later ask may be answered.
    Unavailable(String),
    /// The run has ended: nothing is asked after.
    Ended,
    /// The run's orchestrator answers no sign-in.
    Unanswered,
}

impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Declined::Denied(why) => write!(f, "its broker refused it: {why}"),
            Declined::Unavailable(why) => write!(f, "its broker could not answer it: {why}"),
            Declined::Ended => f.write_str("the run has ended"),
            Declined::Unanswered => f.write_str("the run's orchestrator answers no sign-in"),
        }
    }
}

impl Declined {
    fn encode(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            Declined::Denied(why) => {
                out.push(1);
                put::<2>(out, why.as_bytes())
            }
            Declined::Unavailable(why) => {
                out.push(2);
                put::<2>(out, why.as_bytes())
            }
            Declined::Ended => {
                out.push(3);
                Some(())
            }
            Declined::Unanswered => {
                out.push(4);
                Some(())
            }
        }
    }

    fn decode(r: &mut Read<'_>) -> Option<Declined> {
        Some(match r.byte()? {
            1 => Declined::Denied(r.text()?),
            2 => Declined::Unavailable(r.text()?),
            3 => Declined::Ended,
            4 => Declined::Unanswered,
            _ => return None,
        })
    }
}

/// The words of `NET_SIGN_IN`, its first byte.
const WORD_CAPTURE: u8 = 1;
const WORD_REFRESH: u8 = 2;
const WORD_REVOKE: u8 = 3;

/// What the network process tells its run host of a sign-in (`NET_SIGN_IN`, R04).
#[derive(Debug)]
pub enum Told {
    /// Keep a capture's tokens and give their placeholders, asked by `id`.
    Capture { id: u32, sign_in: String, taken: Taken },
    /// Refresh the sign-in once for every flow of the run that asked, asked by `id`.
    Refresh { id: u32, sign_in: String },
    /// The VM revoked the sign-in: answered here, its stored tokens untouched.
    Revoke { sign_in: String },
}

impl Told {
    /// Its payload, in one buffer sized at once and wiped as it drops: no buffer let go of as one
    /// grows holds a part of a token.
    pub fn encode(&self) -> Option<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(Vec::new());
        match self {
            Told::Capture { id, sign_in, taken } => {
                let refresh = taken.refresh.as_ref().map_or(0, |r| r.len().saturating_add(4));
                let id_token = taken.id_token.as_ref().map_or(0, |t| t.len().saturating_add(4));
                let scope = taken.scope.as_ref().map_or(0, |s| s.len().saturating_add(2));
                out.reserve_exact(
                    sign_in
                        .len()
                        .saturating_add(taken.access.len())
                        .saturating_add(refresh)
                        .saturating_add(id_token)
                        .saturating_add(scope)
                        .saturating_add(27),
                );
                out.push(WORD_CAPTURE);
                out.extend_from_slice(&id.to_be_bytes());
                put::<2>(&mut out, sign_in.as_bytes())?;
                put::<4>(&mut out, &taken.access)?;
                match &taken.refresh {
                    Some(r) => {
                        out.push(1);
                        put::<4>(&mut out, r)?;
                    }
                    None => out.push(0),
                }
                match taken.expires_in {
                    Some(s) => {
                        out.push(1);
                        out.extend_from_slice(&s.to_be_bytes());
                    }
                    None => out.push(0),
                }
                match &taken.id_token {
                    Some(t) => {
                        out.push(1);
                        put::<4>(&mut out, t)?;
                    }
                    None => out.push(0),
                }
                match &taken.scope {
                    Some(s) => {
                        out.push(1);
                        put::<2>(&mut out, s.as_bytes())?;
                    }
                    None => out.push(0),
                }
            }
            Told::Refresh { id, sign_in } => {
                out.push(WORD_REFRESH);
                out.extend_from_slice(&id.to_be_bytes());
                put::<2>(&mut out, sign_in.as_bytes())?;
            }
            Told::Revoke { sign_in } => {
                out.push(WORD_REVOKE);
                put::<2>(&mut out, sign_in.as_bytes())?;
            }
        }
        Some(out)
    }

    /// What [`encode`](Self::encode) wrote; none for a payload that is no such word.
    pub fn decode(payload: &[u8]) -> Option<Told> {
        let mut r = Read(payload);
        let told = match r.byte()? {
            WORD_CAPTURE => {
                let id = u32::try_from(r.number::<4>()?).ok()?;
                let sign_in = r.text()?;
                let access = r.secret()?;
                let refresh = match r.byte()? {
                    0 => None,
                    1 => Some(r.secret()?),
                    _ => return None,
                };
                let expires_in = match r.byte()? {
                    0 => None,
                    1 => Some(r.number::<8>()?),
                    _ => return None,
                };
                let id_token = match r.byte()? {
                    0 => None,
                    1 => Some(r.secret()?),
                    _ => return None,
                };
                let scope = match r.byte()? {
                    0 => None,
                    1 => Some(r.text()?),
                    _ => return None,
                };
                Told::Capture {
                    id,
                    sign_in,
                    taken: Taken {
                        access,
                        refresh,
                        expires_in,
                        id_token,
                        scope,
                    },
                }
            }
            WORD_REFRESH => Told::Refresh {
                id: u32::try_from(r.number::<4>()?).ok()?,
                sign_in: r.text()?,
            },
            WORD_REVOKE => Told::Revoke { sign_in: r.text()? },
            _ => return None,
        };
        r.done().then_some(told)
    }
}

/// The run host's answer to an ask of a sign-in, by its id (`NET_SIGN_IN`).
pub enum Answered {
    Captured {
        id: u32,
        result: Result<Captured, Declined>,
    },
    /// The token response the sign-in's CLI is to have, wiped as it drops.
    Refreshed {
        id: u32,
        result: Result<Zeroizing<Vec<u8>>, Declined>,
    },
}

/// Shows each answer, of a refresh's token response its length alone.
impl fmt::Debug for Answered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answered::Captured { id, result } => f
                .debug_struct("Captured")
                .field("id", id)
                .field("result", result)
                .finish(),
            Answered::Refreshed { id, result } => f
                .debug_struct("Refreshed")
                .field("id", id)
                .field("result", &result.as_ref().map(|body| body.len()))
                .finish(),
        }
    }
}

impl Answered {
    pub fn encode(&self) -> Option<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(Vec::new());
        match self {
            Answered::Captured { id, result } => {
                out.push(WORD_CAPTURE);
                out.extend_from_slice(&id.to_be_bytes());
                match result {
                    Ok(c) => {
                        out.push(0);
                        put_captured(&mut out, c)?;
                    }
                    Err(declined) => {
                        out.push(1);
                        declined.encode(&mut out)?;
                    }
                }
            }
            Answered::Refreshed { id, result } => {
                out.reserve_exact(result.as_ref().map_or(0, |b| b.len()).saturating_add(10));
                out.push(WORD_REFRESH);
                out.extend_from_slice(&id.to_be_bytes());
                match result {
                    Ok(body) => {
                        out.push(0);
                        put::<4>(&mut out, body)?;
                    }
                    Err(declined) => {
                        out.push(1);
                        declined.encode(&mut out)?;
                    }
                }
            }
        }
        Some(out)
    }

    pub fn decode(payload: &[u8]) -> Option<Answered> {
        let mut r = Read(payload);
        let tag = r.byte()?;
        let id = u32::try_from(r.number::<4>()?).ok()?;
        let ok = match r.byte()? {
            0 => true,
            1 => false,
            _ => return None,
        };
        let answered = match (tag, ok) {
            (WORD_CAPTURE, true) => Answered::Captured {
                id,
                result: Ok(r.captured()?),
            },
            (WORD_REFRESH, true) => Answered::Refreshed {
                id,
                result: Ok(r.secret()?),
            },
            (WORD_CAPTURE, false) => Answered::Captured {
                id,
                result: Err(Declined::decode(&mut r)?),
            },
            (WORD_REFRESH, false) => Answered::Refreshed {
                id,
                result: Err(Declined::decode(&mut r)?),
            },
            _ => return None,
        };
        r.done().then_some(answered)
    }
}

/// An ask of the run host under way (R04): a capture, its flow waiting; or a refresh, every flow
/// of the sign-in that asked one waiting on it, so that one rotation serves them all (RFC 6749
/// §6, f47).
#[derive(Debug)]
enum Asking {
    /// Its asker gone (an HTTP/2 stream let go of) where none: its answer is kept all the same.
    Capture {
        index: usize,
        asker: Option<Asker>,
    },
    Refresh {
        index: usize,
        waiting: Vec<Asker>,
    },
}

/// Who waits on an ask: a flow, and its request's HTTP/2 stream, 0 for an HTTP/1.1 flow's, whose
/// requests go one at a time (no HTTP/2 request is on stream 0, RFC 9113 §5.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Asker {
    pub flow: Waiter,
    pub stream: u32,
}

impl From<Waiter> for Asker {
    fn from(flow: Waiter) -> Asker {
        Asker { flow, stream: 0 }
    }
}

/// An ask's answer, as its flow takes it.
pub enum Answer {
    /// The sign-in's tokens are kept outside: its placeholders are its grant's `captured` now.
    Captured,
    /// The sign-in refreshed outside: the token response its CLI is to have.
    Refreshed(Zeroizing<Vec<u8>>),
    Declined(Declined),
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answer::Captured => f.write_str("Captured"),
            Answer::Refreshed(body) => write!(f, "Refreshed({} bytes)", body.len()),
            Answer::Declined(why) => f.debug_tuple("Declined").field(why).finish(),
        }
    }
}

/// What an answer changed of the run's sign-ins, for its credentials to follow.
#[derive(Debug, PartialEq, Eq)]
pub enum Changed {
    /// The sign-in at `index` was captured: its access grant is new, the one it replaced gone,
    /// and a lease of its reference held before is stale.
    Captured {
        index: usize,
        replaced: Option<Captured>,
    },
    /// The sign-in at `index` refreshed: its access token's held lease is stale.
    Refreshed { index: usize },
    /// Nothing of its credentials: a refusal.
    Nothing,
}

/// The run's sign-ins as its network process holds them (D242, step 8; R04): each grant, with
/// what its capture gave; each ask of the run host by its id until its answer; each answer kept
/// for its flow until that flow takes it. An ask never holds up the loop that makes it: it goes
/// out on the loop's next pass ([`asks`](Self::asks)) and its answer comes as an event naming the
/// flows to resume ([`resumed`](Self::resumed)), as a run's leases do.
pub struct SignIns {
    pub grants: Vec<SignInGrant>,
    asking: HashMap<u32, Asking>,
    next: u32,
    unsent: Vec<Zeroizing<Vec<u8>>>,
    answers: HashMap<Asker, Answer>,
    resumed: Vec<Waiter>,
    ended: bool,
}

/// Shows its grants and where its asks stand, never a token: a capture's unsent ask holds one.
impl fmt::Debug for SignIns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignIns")
            .field("grants", &self.grants)
            .field("asking", &self.asking)
            .field("unsent", &self.unsent.len())
            .field("answers", &self.answers)
            .field("ended", &self.ended)
            .finish()
    }
}

impl SignIns {
    pub fn new(grants: Vec<SignInGrant>) -> SignIns {
        SignIns {
            grants,
            asking: HashMap::new(),
            next: 0,
            unsent: Vec::new(),
            answers: HashMap::new(),
            resumed: Vec::new(),
            ended: false,
        }
    }

    /// The run's sign-in of `cli`, which is one at most.
    pub fn of(&self, cli: Cli) -> Option<usize> {
        self.grants.iter().position(|g| g.cli == cli)
    }

    /// The sign-ins whose token host `host` is.
    pub fn at_token_host(&self, host: &str) -> Option<Cli> {
        self.grants
            .iter()
            .map(|g| g.cli)
            .find(|&cli| profile(cli).token.host.eq_ignore_ascii_case(host))
    }

    /// An id no ask under way has: at most one more try than there are asks.
    fn fresh(&mut self) -> u32 {
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            if !self.asking.contains_key(&id) {
                return id;
            }
        }
    }

    fn sign_in(&self, index: usize) -> Result<String, Declined> {
        if self.ended {
            return Err(Declined::Ended);
        }
        self.grants
            .get(index)
            .map(|g| g.sign_in.clone())
            .ok_or_else(|| Declined::Denied("no sign-in of the run's".into()))
    }

    /// Asks the run host to keep sign-in `index`'s `taken` tokens, `asker` waiting.
    pub fn capture(&mut self, index: usize, taken: Taken, asker: impl Into<Asker>) -> Result<(), Declined> {
        let sign_in = self.sign_in(index)?;
        let id = self.fresh();
        let told = Told::Capture { id, sign_in, taken }
            .encode()
            .ok_or_else(|| Declined::Unavailable("a capture past what its lengths count".into()))?;
        self.unsent.push(told);
        self.asking.insert(
            id,
            Asking::Capture {
                index,
                asker: Some(asker.into()),
            },
        );
        Ok(())
    }

    /// Asks the run host to refresh sign-in `index`, `asker` waiting: joined to the ask under way
    /// where there is one.
    pub fn refresh(&mut self, index: usize, asker: impl Into<Asker>) -> Result<(), Declined> {
        let asker = asker.into();
        let sign_in = self.sign_in(index)?;
        let under_way = self.asking.values_mut().find_map(|a| match a {
            Asking::Refresh { index: i, waiting } if *i == index => Some(waiting),
            _ => None,
        });
        if let Some(waiting) = under_way {
            waiting.push(asker);
            return Ok(());
        }
        let id = self.fresh();
        let told = Told::Refresh { id, sign_in }
            .encode()
            .ok_or_else(|| Declined::Unavailable("a sign-in's name past what its length counts".into()))?;
        self.unsent.push(told);
        self.asking.insert(
            id,
            Asking::Refresh {
                index,
                waiting: vec![asker],
            },
        );
        Ok(())
    }

    /// Tells the run host the VM revoked sign-in `index`, which is answered here.
    pub fn revoke(&mut self, index: usize) {
        if let Ok(sign_in) = self.sign_in(index)
            && let Some(told) = (Told::Revoke { sign_in }).encode()
        {
            self.unsent.push(told);
        }
    }

    /// The words to the run host since the last call, each a `NET_SIGN_IN` payload.
    pub fn asks(&mut self) -> Vec<Zeroizing<Vec<u8>>> {
        std::mem::take(&mut self.unsent)
    }

    /// The run host's answer `payload` (`NET_SIGN_IN`), the run's credential grants being
    /// `grants`: what it changed, its flows resumed. None for a payload that is no answer, or one
    /// to no ask of this process's, which breaks the run host's contract. A capture's placeholders
    /// that are no tokens, or another credential's, are declined, never taken. After the run's
    /// end, an answer is let go of: nothing waits on it.
    pub fn answered(&mut self, payload: &[u8], grants: &[credential::CredentialGrant]) -> Option<Changed> {
        let answered = Answered::decode(payload)?;
        if self.ended {
            return Some(Changed::Nothing);
        }
        let (id, asking) = match &answered {
            Answered::Captured { id, .. } | Answered::Refreshed { id, .. } => (*id, self.asking.remove(id)?),
        };
        match (asking, answered) {
            (Asking::Capture { index, asker }, Answered::Captured { result, .. }) => {
                let result = result.and_then(|c| self.fits(index, c, grants));
                Some(match result {
                    Ok(c) => {
                        let replaced = self.grants.get_mut(index)?.captured.replace(c);
                        self.tell(asker, Answer::Captured);
                        Changed::Captured { index, replaced }
                    }
                    Err(declined) => {
                        self.tell(asker, Answer::Declined(declined));
                        Changed::Nothing
                    }
                })
            }
            (Asking::Refresh { index, waiting }, Answered::Refreshed { result, .. }) => {
                let changed = match &result {
                    Ok(_) => Changed::Refreshed { index },
                    Err(_) => Changed::Nothing,
                };
                for asker in waiting {
                    let answer = match &result {
                        Ok(body) => Answer::Refreshed(body.clone()),
                        Err(declined) => Answer::Declined(declined.clone()),
                    };
                    self.tell(Some(asker), answer);
                }
                Some(changed)
            }
            (asking, _) => {
                // An answer of another ask's kind: the ask is put back, the contract broken.
                self.asking.insert(id, asking);
                None
            }
        }
    }

    /// Captured placeholders `c` for sign-in `index`, where each is a token and the run's
    /// credentials' alone: none another grant's placeholder (the access grant they replace
    /// aside), and the two apart.
    fn fits(
        &self,
        index: usize,
        c: Captured,
        grants: &[credential::CredentialGrant],
    ) -> Result<Captured, Declined> {
        let old = self.grants.get(index).and_then(|g| g.captured.as_ref());
        let others: Vec<&str> = grants
            .iter()
            .filter(|g| old.is_none_or(|o| g.placeholder != o.access_placeholder))
            .map(|g| g.placeholder.as_str())
            .chain(
                self.grants
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .filter_map(|(_, g)| g.captured.as_ref())
                    .filter_map(|o| o.refresh_placeholder.as_deref()),
            )
            .collect();
        let token = |p: &str| credential::placeholder(p) && !others.contains(&p);
        if token(&c.access_placeholder)
            && c.refresh_placeholder
                .as_deref()
                .is_none_or(|r| token(r) && r != c.access_placeholder)
        {
            Ok(c)
        } else {
            Err(Declined::Denied(
                "its placeholders are no tokens, or stand for another credential".into(),
            ))
        }
    }

    fn tell(&mut self, asker: Option<Asker>, answer: Answer) {
        if let Some(asker) = asker {
            self.answers.insert(asker, answer);
            self.resumed.push(asker.flow);
        }
    }

    /// `asker`'s answer, where one came for it.
    pub fn take(&mut self, asker: impl Into<Asker>) -> Option<Answer> {
        self.answers.remove(&asker.into())
    }

    /// `asker` gone (an HTTP/2 stream let go of before its answer was taken): its answer dropped,
    /// and none kept for it after. Its ask stays under way, as other askers or the sign-in's
    /// grants may still take its answer.
    pub fn forget(&mut self, asker: Asker) {
        self.answers.remove(&asker);
        for asking in self.asking.values_mut() {
            match asking {
                Asking::Capture { asker: a, .. } if *a == Some(asker) => *a = None,
                Asking::Refresh { waiting, .. } => waiting.retain(|a| *a != asker),
                Asking::Capture { .. } => {}
            }
        }
    }

    /// The flows whose asks were answered or ended since the last call.
    pub fn resumed(&mut self) -> Vec<Waiter> {
        std::mem::take(&mut self.resumed)
    }

    /// How many flows wait on asks under way: a test's view of their single flight.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.asking
            .values()
            .map(|a| match a {
                Asking::Capture { asker, .. } => usize::from(asker.is_some()),
                Asking::Refresh { waiting, .. } => waiting.len(),
            })
            .sum()
    }

    /// The run's end, or its host's: each ask under way declined, its flows told the run has
    /// ended, and nothing asked after.
    pub fn end(&mut self) {
        self.ended = true;
        self.unsent.clear();
        for (_, asking) in std::mem::take(&mut self.asking) {
            match asking {
                Asking::Capture { asker, .. } => self.tell(asker, Answer::Declined(Declined::Ended)),
                Asking::Refresh { waiting, .. } => {
                    for asker in waiting {
                        self.tell(Some(asker), Answer::Declined(Declined::Ended));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS_PLACEHOLDER: &str =
        "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REFRESH_PLACEHOLDER: &str =
        "kiln-placeholder-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    /// Values shaped as Anthropic's (`sk-ant-oat`, `sk-ant-ort`), none real.
    const ACCESS: &str = "sk-ant-oat01-not-a-real-access-token-0123456789";
    const REFRESH: &str = "sk-ant-ort01-not-a-real-refresh-token-9876543210";
    /// A JWT shaped as OpenAI's ID token: a header, a payload of claims Codex reads, a
    /// signature; none real.
    const ID_HEADER: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9";
    const ID_PAYLOAD: &str = "eyJlbWFpbCI6ImFAZXhhbXBsZS50ZXN0IiwiZXhwIjoxOTAwMDAwMDAwfQ";
    const ID_SIGNATURE: &str = "c2lnbmF0dXJlLW5vdC1yZWFs";

    fn placeholders() -> Placeholders {
        Placeholders {
            access: ACCESS_PLACEHOLDER.into(),
            refresh: Some(REFRESH_PLACEHOLDER.into()),
        }
    }

    fn id_token() -> String {
        format!("{ID_HEADER}.{ID_PAYLOAD}.{ID_SIGNATURE}")
    }

    /// Claude Code's exchange answer, as `formatTokens` reads it (243341777).
    fn claude_body() -> String {
        format!(
            r#"{{"token_type":"Bearer","access_token":"{ACCESS}","expires_in":28800,"refresh_token":"{REFRESH}","scope":"user:inference user:profile","account":{{"uuid":"u","email_address":"a@example.test"}}}}"#
        )
    }

    /// Codex's exchange answer, as `ExchangedTokens` reads it (server.rs:701-707).
    fn codex_body() -> String {
        format!(
            r#"{{"id_token": "{}", "access_token": "{ACCESS}", "refresh_token": "{REFRESH}"}}"#,
            id_token()
        )
    }

    fn holds(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn a_claude_code_token_response_is_given_placeholders_and_nothing_else_changes() {
        let body = claude_body();
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        assert_eq!(
            tokens.access.as_ref().unwrap().value.as_slice(),
            ACCESS.as_bytes()
        );
        assert_eq!(
            tokens.refresh.as_ref().unwrap().value.as_slice(),
            REFRESH.as_bytes()
        );
        assert!(tokens.id.is_none());
        assert_eq!(tokens.expires_in, Some(28800));
        let out = write(body.as_bytes(), &tokens, &placeholders()).unwrap();
        let want = body
            .replace(ACCESS, ACCESS_PLACEHOLDER)
            .replace(REFRESH, REFRESH_PLACEHOLDER);
        assert_eq!(String::from_utf8(out.to_vec()).unwrap(), want);
        assert!(!holds(&out, ACCESS) && !holds(&out, REFRESH));
    }

    #[test]
    fn a_codex_token_response_keeps_its_id_tokens_claims_and_loses_its_signature() {
        let body = codex_body();
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        let out = write(body.as_bytes(), &tokens, &placeholders()).unwrap();
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(!holds(&out, ACCESS) && !holds(&out, REFRESH) && !holds(&out, &id_token()));
        assert!(!holds(&out, ID_SIGNATURE));
        // As Codex reads it (token_data.rs:144-155): three parts, none empty, the payload as
        // the issuer wrote it.
        let id = text
            .split(r#""id_token": ""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let parts: Vec<&str> = id.split('.').collect();
        assert_eq!(parts, [ID_HEADER, ID_PAYLOAD, "shards-placeholder"]);
        assert!(text.contains(&format!(r#""access_token": "{ACCESS_PLACEHOLDER}""#)));
        assert!(text.contains(&format!(r#""refresh_token": "{REFRESH_PLACEHOLDER}""#)));
    }

    #[test]
    fn an_id_token_that_is_no_jwt_is_refused() {
        for id in ["opaque", "a.b", "a.b.c.d", "a..c", ".b.c", r#"a\"x.b.c"#] {
            let body = format!(r#"{{"id_token":"{id}","access_token":"{ACCESS}"}}"#);
            let tokens = read(body.as_bytes());
            let written =
                tokens.and_then(|t| write(body.as_bytes(), &t.unwrap(), &placeholders()).map(|_| ()));
            assert_eq!(written, Err(Refusal::NotAJwt), "{id}");
        }
    }

    #[test]
    fn a_response_without_tokens_passes_and_one_that_cannot_be_read_is_refused() {
        for body in [
            "",
            "  \r\n",
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
            "[1,2]",
            r#"{"device_auth_id":"d","user_code":"ABCD-1234","interval":"5"}"#,
        ] {
            assert!(read(body.as_bytes()).unwrap().is_none(), "{body}");
        }
        for body in [
            "<html>no</html>",
            "{",
            r#"{"a":1}x"#,
            "\u{feff}{}",
            r#"{"a":01}"#,
            r#"{"a":1,}"#,
            r#"[1,]"#,
            "{\"a\":\"\u{1}\"}",
            r#"{"access_token":"x","a":"\ud800"}"#,
            r#"{"a":"\x"}"#,
        ] {
            assert_eq!(read(body.as_bytes()).err(), Some(Refusal::NotJson), "{body}");
        }
    }

    #[test]
    fn a_token_anywhere_but_its_place_is_refused() {
        let nested = format!(r#"{{"data":{{"access_token":"{ACCESS}"}}}}"#);
        let twice = format!(r#"{{"access_token":"{ACCESS}","access_token":"{ACCESS}"}}"#);
        let number = r#"{"access_token":12345}"#.to_string();
        let in_array = format!(r#"{{"x":[{{"refresh_token":"{REFRESH}"}}]}}"#);
        for body in [nested, twice, number, in_array] {
            assert_eq!(read(body.as_bytes()).err(), Some(Refusal::Misplaced), "{body}");
        }
    }

    #[test]
    fn a_tokens_value_echoed_elsewhere_is_refused_escaped_or_not() {
        let plain = format!(r#"{{"access_token":"{ACCESS}","note":"was {ACCESS}"}}"#);
        // `\u0073` is `s`: the echo decoded is the value.
        let escaped = format!(
            r#"{{"refresh_token":"{REFRESH}","x":["\u0073{}"]}}"#,
            REFRESH.strip_prefix('s').unwrap()
        );
        let as_a_name = format!(r#"{{"access_token":"{ACCESS}","{ACCESS}":1}}"#);
        for body in [plain, escaped, as_a_name] {
            assert_eq!(read(body.as_bytes()).err(), Some(Refusal::Echoed), "{body}");
        }
    }

    #[test]
    fn a_tokens_escapes_are_decoded_and_its_whole_string_replaced() {
        let body = r#"{"access_token":"a\/b\u00e9\ud83d\ude00c","refresh_token":"R1"}"#;
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        assert_eq!(
            tokens.access.as_ref().unwrap().value.as_slice(),
            "a/bé😀c".as_bytes()
        );
        let out = write(body.as_bytes(), &tokens, &placeholders()).unwrap();
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            format!(r#"{{"access_token":"{ACCESS_PLACEHOLDER}","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#)
        );
    }

    #[test]
    fn a_placeholder_a_json_string_cannot_hold_as_it_is_is_refused() {
        let body = claude_body();
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        for bad in ["", "a\"b", "a\\b", "a b"] {
            let with = Placeholders {
                access: bad.into(),
                refresh: Some(REFRESH_PLACEHOLDER.into()),
            };
            assert_eq!(
                write(body.as_bytes(), &tokens, &with).err(),
                Some(Refusal::NoPlaceholder)
            );
        }
    }

    #[test]
    fn a_rewritten_responses_framing_is_recomputed() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nContent-Length: 9\r\n\r\n";
        let framed = frame(head, 321).unwrap();
        assert_eq!(
            String::from_utf8(framed).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: 321\r\n\r\n"
        );
        assert_eq!(frame(b"HTTP/1.1 200 OK\r\n", 1), Err(Refusal::Head));
        // A whole exchange: the body's new length, the head saying it.
        let body = claude_body();
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        let out = write(body.as_bytes(), &tokens, &placeholders()).unwrap();
        let framed = frame(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n", out.len()).unwrap();
        assert!(holds(&framed, &format!("Content-Length: {}\r\n", out.len())));
        assert_ne!(out.len(), body.len());
    }

    #[test]
    fn a_coded_body_is_not_read() {
        let ok = [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n"[..],
            b"HTTP/1.1 200 OK\r\nContent-Encoding: identity\r\nTransfer-Encoding: chunked\r\n\r\n",
        ];
        for head in ok {
            assert_eq!(readable(head), Ok(()));
        }
        let coded = [
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\n"[..],
            b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        ];
        for head in coded {
            assert_eq!(readable(head), Err(Refusal::Coded));
        }
    }

    fn request<'a>(host: &'a str, target: &'a str, content_type: &'a str, body: &'a str) -> Request<'a> {
        Request {
            host,
            method: "POST",
            target,
            content_type: Some(content_type.as_bytes()),
            body: body.as_bytes(),
        }
    }

    const JSON: &str = "application/json";
    const FORM: &str = "application/x-www-form-urlencoded";

    #[test]
    fn a_refresh_carrying_a_sign_ins_placeholder_is_answered_outside() {
        let other = Placeholders {
            access: "kiln-placeholder-aa".into(),
            refresh: Some("kiln-placeholder-bb".into()),
        };
        let mine = placeholders();
        let signins = [&other, &mine];
        // Claude Code's refresh (242103577).
        let claude = format!(
            r#"{{"grant_type":"refresh_token","refresh_token":"{REFRESH_PLACEHOLDER}","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","scope":"user:inference"}}"#
        );
        let r = request("platform.claude.com", "/v1/oauth/token", JSON, &claude);
        assert_eq!(ask(&CLAUDE_CODE, &r, &signins), Ask::Refresh(1));
        // Codex's (client.rs:97-113, 132-134: a BTreeMap's keys in order).
        let codex = format!(
            r#"{{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_reason":"proactive_expiry","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#
        );
        let r = request(
            "auth.openai.com",
            "/oauth/token",
            "application/json; charset=utf-8",
            &codex,
        );
        assert_eq!(ask(&CODEX, &r, &signins), Ask::Refresh(1));
        // In form encoding, as RFC 6749 §6 writes it.
        let form = format!("grant_type=refresh_token&refresh_token={REFRESH_PLACEHOLDER}");
        let r = request(
            "auth.openai.com",
            "https://auth.openai.com/oauth/token?x=1",
            FORM,
            &form,
        );
        assert_eq!(ask(&CODEX, &r, &signins), Ask::Refresh(1));
    }

    #[test]
    fn a_request_that_is_no_refresh_of_a_placeholder_passes() {
        let mine = placeholders();
        let signins = [&mine];
        let refresh = format!(r#"{{"grant_type":"refresh_token","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#);
        let access_as_refresh =
            format!(r#"{{"grant_type":"refresh_token","refresh_token":"{ACCESS_PLACEHOLDER}"}}"#);
        let off_by_one = refresh.replace("3210\"", "3211\"");
        let twice = format!(
            r#"{{"grant_type":"refresh_token","refresh_token":"x","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#
        );
        let passes = [
            // Its own token, or another sign-in's access placeholder, as the refresh token.
            request(
                "platform.claude.com",
                "/v1/oauth/token",
                JSON,
                r#"{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-its-own"}"#,
            ),
            request("platform.claude.com", "/v1/oauth/token", JSON, &access_as_refresh),
            // A placeholder that differs in its last byte alone.
            request("platform.claude.com", "/v1/oauth/token", JSON, &off_by_one),
            // The code exchanged, whose response is read for its tokens.
            request(
                "platform.claude.com",
                "/v1/oauth/token",
                JSON,
                r#"{"grant_type":"authorization_code","code":"c","code_verifier":"v"}"#,
            ),
            // Another host, path or content type.
            request("api.anthropic.com", "/v1/oauth/token", JSON, &refresh),
            request("platform.claude.com", "/v1/oauth/token2", JSON, &refresh),
            request("platform.claude.com", "/v1/oauth/token", "text/plain", &refresh),
            // A field named twice.
            request("platform.claude.com", "/v1/oauth/token", JSON, &twice),
        ];
        for r in passes {
            assert_eq!(ask(&CLAUDE_CODE, &r, &signins), Ask::Pass, "{r:?}");
        }
        let mut get = request("platform.claude.com", "/v1/oauth/token", JSON, &refresh);
        get.method = "GET";
        assert_eq!(ask(&CLAUDE_CODE, &get, &signins), Ask::Pass);
    }

    #[test]
    fn a_revocation_of_a_placeholder_is_answered_outside_and_another_passes() {
        let mine = placeholders();
        let signins = [&mine];
        // Claude Code's (`i3r`, 242106079).
        let claude = format!(
            r#"{{"token":"{REFRESH_PLACEHOLDER}","token_type_hint":"refresh_token","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e"}}"#
        );
        let r = request("platform.claude.com", "/v1/oauth/token/revoke", JSON, &claude);
        assert_eq!(ask(&CLAUDE_CODE, &r, &signins), Ask::Revoke(0));
        // Codex's of its access token, where it holds no refresh token (revoke.rs:68-77).
        let codex = format!(r#"{{"token":"{ACCESS_PLACEHOLDER}","token_type_hint":"access_token"}}"#);
        let r = request("auth.openai.com", "/oauth/revoke", JSON, &codex);
        assert_eq!(ask(&CODEX, &r, &signins), Ask::Revoke(0));
        let r = request("auth.openai.com", "/oauth/revoke", JSON, r#"{"token":"its-own"}"#);
        assert_eq!(ask(&CODEX, &r, &signins), Ask::Pass);
    }

    #[test]
    fn a_sign_ins_token_is_never_filled_toward_an_endpoint_that_mints_a_key() {
        for target in [
            "/api/oauth/claude_cli/create_api_key",
            "/api/oauth/claude_cli/create_api_key?x=1",
            "https://api.anthropic.com/api/oauth/claude_cli/create_api_key",
            "//api//oauth/claude_cli/create_api_key/",
            "/api/oauth/claude_cli/./create_api_key",
            "/api/oauth/x/../claude_cli/create_api_key",
            "/api/oauth/claude_cli/%63reate_api_key",
            "/API/OAuth/Claude_CLI/Create_API_Key",
            "/api/oauth/claude_cli/create_api_key/more",
        ] {
            assert!(CLAUDE_CODE.mints("api.anthropic.com", target), "{target}");
            assert!(CLAUDE_CODE.mints("API.anthropic.com", target), "{target}");
        }
        for target in [
            "/v1/messages",
            "/api/oauth/claude_cli/roles",
            "/api/oauth/claude_cli/create_api_keys",
        ] {
            assert!(!CLAUDE_CODE.mints("api.anthropic.com", target), "{target}");
        }
        assert!(!CLAUDE_CODE.mints("platform.claude.com", "/api/oauth/claude_cli/create_api_key"));
        assert!(!CODEX.mints("chatgpt.com", "/backend-api/codex/responses"));
    }

    #[test]
    fn the_answers_given_outside_are_whole_messages() {
        let body = br#"{"access_token":"p","refresh_token":"q","expires_in":60}"#;
        let answer = refreshed(body);
        let text = String::from_utf8(answer.to_vec()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Cache-Control: no-store\r\n"));
        assert!(text.ends_with(&format!(
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )));
        assert!(response_fields(REVOKED).is_some());
        assert!(holds(REVOKED, "Content-Length: 0\r\n"));
    }

    #[test]
    fn no_token_or_placeholder_prints() {
        let body = codex_body();
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        let shown = format!("{tokens:?} {:?} {:?}", tokens.access, placeholders());
        for secret in [
            ACCESS,
            REFRESH,
            ID_SIGNATURE,
            ACCESS_PLACEHOLDER,
            REFRESH_PLACEHOLDER,
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
        let refresh = format!(r#"{{"grant_type":"refresh_token","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#);
        let r = request("platform.claude.com", "/v1/oauth/token", JSON, &refresh);
        assert!(!format!("{r:?}").contains(REFRESH_PLACEHOLDER));
    }

    #[test]
    fn json_nesting_is_followed_without_recursion() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(read(deep.as_bytes()).unwrap().is_none());
        let body = format!(
            r#"{{"a":[1,-2.5e+3,true,false,null,{{"b":"c"}},[]],"access_token":"{ACCESS}","e":{{}},"f":0}}"#
        );
        let tokens = read(body.as_bytes()).unwrap().unwrap();
        assert_eq!(tokens.access.unwrap().value.as_slice(), ACCESS.as_bytes());
        assert_eq!(
            read(br#"{"expires_in":3600.5,"access_token":"T1"}"#)
                .unwrap()
                .unwrap()
                .expires_in,
            None
        );
    }

    fn sign_in(captured: bool) -> SignInGrant {
        SignInGrant {
            sign_in: "claude/acct-1".into(),
            cli: Cli::ClaudeCode,
            toward: vec!["api.anthropic.com".into()],
            captured: captured.then(|| Captured {
                access: "sign-ins/claude/acct-1/access".into(),
                access_placeholder: ACCESS_PLACEHOLDER.into(),
                refresh_placeholder: Some(REFRESH_PLACEHOLDER.into()),
            }),
        }
    }

    fn taken() -> Taken {
        Taken {
            access: Zeroizing::new(ACCESS.as_bytes().to_vec()),
            refresh: Some(Zeroizing::new(REFRESH.as_bytes().to_vec())),
            expires_in: Some(28800),
            id_token: Some(Zeroizing::new(b"eyJh.eyJw.shards-placeholder".to_vec())),
            scope: Some("user:inference user:profile".into()),
        }
    }

    #[test]
    fn sign_in_grants_ride_their_message_and_a_cli_has_one() {
        let grants = vec![
            sign_in(true),
            SignInGrant {
                sign_in: "codex/acct-2".into(),
                cli: Cli::Codex,
                toward: vec!["chatgpt.com".into()],
                captured: None,
            },
        ];
        let bytes = encode_grants(&grants).unwrap();
        assert_eq!(decode_grants(&bytes), Some(grants.clone()));
        assert_eq!(decode_grants(&bytes[..bytes.len() - 1]), None);
        let two = encode_grants(&[
            sign_in(false),
            SignInGrant {
                sign_in: "claude/acct-3".into(),
                ..sign_in(false)
            },
        ])
        .unwrap();
        assert_eq!(decode_grants(&two), None, "two sign-ins of one CLI");
        let mut no_cli = encode_grants(&[sign_in(false)]).unwrap();
        no_cli[2 + "claude/acct-1".len()] = 9;
        assert_eq!(decode_grants(&no_cli), None);
        let nowhere = SignInGrant {
            toward: Vec::new(),
            ..sign_in(false)
        };
        assert_eq!(decode_grants(&encode_grants(&[nowhere]).unwrap()), None);
        let shown = format!("{grants:?}");
        assert!(
            !shown.contains(ACCESS_PLACEHOLDER) && !shown.contains(REFRESH_PLACEHOLDER),
            "{shown}"
        );
    }

    #[test]
    fn a_sign_ins_words_round_trip_and_print_no_token() {
        let capture = Told::Capture {
            id: 7,
            sign_in: "claude/acct-1".into(),
            taken: taken(),
        };
        let bytes = capture.encode().unwrap();
        match Told::decode(&bytes).unwrap() {
            Told::Capture { id, sign_in, taken } => {
                assert_eq!(
                    (id, sign_in.as_str(), taken.expires_in),
                    (7, "claude/acct-1", Some(28800))
                );
                assert_eq!(taken.access.as_slice(), ACCESS.as_bytes());
                assert_eq!(taken.refresh.unwrap().as_slice(), REFRESH.as_bytes());
            }
            other => panic!("{other:?}"),
        }
        assert!(Told::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Told::decode(&[bytes.as_slice(), &[0]].concat()).is_none());
        let shown = format!("{capture:?}");
        assert!(!shown.contains(ACCESS) && !shown.contains(REFRESH), "{shown}");
        for told in [
            Told::Refresh {
                id: 1,
                sign_in: "s".into(),
            },
            Told::Revoke { sign_in: "s".into() },
        ] {
            let back = Told::decode(&told.encode().unwrap()).unwrap();
            assert_eq!(format!("{back:?}"), format!("{told:?}"));
        }
        let body = Zeroizing::new(token_body(ACCESS, REFRESH).into_bytes());
        let refreshed = Answered::Refreshed {
            id: 4,
            result: Ok(body.clone()),
        };
        match Answered::decode(&refreshed.encode().unwrap()).unwrap() {
            Answered::Refreshed {
                id: 4,
                result: Ok(back),
            } => assert_eq!(back, body),
            other => panic!("{other:?}"),
        }
        assert!(!format!("{refreshed:?}").contains(ACCESS));
        for a in [
            Answered::Captured {
                id: 3,
                result: Ok(sign_in(true).captured.unwrap()),
            },
            Answered::Captured {
                id: 3,
                result: Err(Declined::Denied("no such sign-in".into())),
            },
            Answered::Refreshed {
                id: 4,
                result: Err(Declined::Unanswered),
            },
            Answered::Refreshed {
                id: 4,
                result: Err(Declined::Unavailable("down".into())),
            },
            Answered::Refreshed {
                id: 4,
                result: Err(Declined::Ended),
            },
        ] {
            let back = Answered::decode(&a.encode().unwrap()).unwrap();
            assert_eq!(format!("{back:?}"), format!("{a:?}"));
        }
    }

    fn token_body(access: &str, refresh: &str) -> String {
        format!(r#"{{"access_token":"{access}","refresh_token":"{refresh}"}}"#)
    }

    #[test]
    fn a_sign_ins_refreshes_ask_once_and_its_asks_end_with_the_run() {
        let mut s = SignIns::new(vec![sign_in(false)]);
        assert_eq!((s.of(Cli::ClaudeCode), s.of(Cli::Codex)), (Some(0), None));
        assert_eq!(s.at_token_host("PLATFORM.claude.com"), Some(Cli::ClaudeCode));
        assert_eq!(s.at_token_host("auth.openai.com"), None);
        s.capture(0, taken(), 1).unwrap();
        let asks = s.asks();
        let Some(Told::Capture { id, .. }) = Told::decode(&asks[0]) else {
            panic!("no capture asked")
        };
        let captured = sign_in(true).captured.unwrap();
        let answer = Answered::Captured {
            id,
            result: Ok(captured.clone()),
        };
        assert_eq!(
            s.answered(&answer.encode().unwrap(), &[]),
            Some(Changed::Captured {
                index: 0,
                replaced: None
            })
        );
        assert_eq!(s.resumed(), [1]);
        assert!(matches!(s.take(1), Some(Answer::Captured)));
        assert_eq!(s.grants[0].captured, Some(captured));
        assert!(
            s.answered(&answer.encode().unwrap(), &[]).is_none(),
            "an answer to no ask taken"
        );
        s.refresh(0, 1).unwrap();
        s.refresh(0, 2).unwrap();
        let asks = s.asks();
        assert_eq!((asks.len(), s.waiting()), (1, 2), "one refresh asked twice");
        let Some(Told::Refresh { id, .. }) = Told::decode(&asks[0]) else {
            panic!("no refresh asked")
        };
        let body = Zeroizing::new(token_body(ACCESS, REFRESH).into_bytes());
        let answer = Answered::Refreshed {
            id,
            result: Ok(body.clone()),
        };
        assert_eq!(
            s.answered(&answer.encode().unwrap(), &[]),
            Some(Changed::Refreshed { index: 0 })
        );
        let mut resumed = s.resumed();
        resumed.sort_unstable();
        assert_eq!(resumed, [1, 2]);
        for waiter in [1, 2] {
            assert!(matches!(s.take(waiter), Some(Answer::Refreshed(b)) if b == body));
        }
        s.revoke(0);
        assert!(matches!(Told::decode(&s.asks()[0]), Some(Told::Revoke { .. })));
        s.refresh(0, 3).unwrap();
        s.end();
        assert_eq!(s.resumed(), [3]);
        assert!(matches!(s.take(3), Some(Answer::Declined(Declined::Ended))));
        assert_eq!(s.refresh(0, 4), Err(Declined::Ended));
        assert!(s.asks().is_empty());
    }

    /// Asks and answers are kept per flow and HTTP/2 stream (D242, steps 7 and 8). Do: three streams
    /// of one flow ask its sign-in's refresh at once, the third let go of before the answer; then a
    /// stream asks a capture and is let go of before its answer. Expect: one refresh asked; each of
    /// the first two its own answer, none for the flow's HTTP/1.1 stream 0 or the third; the
    /// capture kept as the sign-in's all the same, no answer kept for its stream.
    #[test]
    fn each_stream_of_a_flow_takes_its_own_answer_and_one_let_go_of_none() {
        let mut s = SignIns::new(vec![sign_in(false)]);
        let on = |stream| Asker { flow: 7, stream };
        for stream in [1, 3, 5] {
            s.refresh(0, on(stream)).unwrap();
        }
        s.forget(on(5));
        let asks = s.asks();
        assert_eq!(
            (asks.len(), s.waiting()),
            (1, 2),
            "one refresh, two streams waiting"
        );
        let Some(Told::Refresh { id, .. }) = Told::decode(&asks[0]) else {
            panic!("no refresh asked")
        };
        let body = Zeroizing::new(token_body(ACCESS, REFRESH).into_bytes());
        let answer = Answered::Refreshed {
            id,
            result: Ok(body.clone()),
        };
        assert_eq!(
            s.answered(&answer.encode().unwrap(), &[]),
            Some(Changed::Refreshed { index: 0 })
        );
        assert_eq!(s.resumed(), [7, 7]);
        assert!(s.take(7).is_none(), "stream 0 took a stream's answer");
        for stream in [1, 3] {
            assert!(matches!(s.take(on(stream)), Some(Answer::Refreshed(b)) if b == body));
        }
        assert!(s.take(on(5)).is_none(), "an answer kept for a stream let go of");
        s.capture(0, taken(), on(9)).unwrap();
        let Some(Told::Capture { id, .. }) = Told::decode(&s.asks()[0]) else {
            panic!("no capture asked")
        };
        s.forget(on(9));
        let captured = sign_in(true).captured.unwrap();
        let answer = Answered::Captured {
            id,
            result: Ok(captured.clone()),
        };
        assert_eq!(
            s.answered(&answer.encode().unwrap(), &[]),
            Some(Changed::Captured {
                index: 0,
                replaced: None
            })
        );
        assert!(s.take(on(9)).is_none() && s.resumed().is_empty());
        assert_eq!(s.grants[0].captured, Some(captured));
    }

    #[test]
    fn a_captures_placeholders_that_are_no_tokens_or_anothers_are_declined() {
        let other = credential::CredentialGrant {
            reference: "openai/key".into(),
            toward: vec!["api.openai.com".into()],
            placeholder: ACCESS_PLACEHOLDER.into(),
        };
        let mut s = SignIns::new(vec![sign_in(false)]);
        for (access, refresh) in [
            (ACCESS_PLACEHOLDER, REFRESH_PLACEHOLDER),
            ("not a token", REFRESH_PLACEHOLDER),
            (REFRESH_PLACEHOLDER, REFRESH_PLACEHOLDER),
        ] {
            s.capture(0, taken(), 1).unwrap();
            let Some(Told::Capture { id, .. }) = Told::decode(&s.asks()[0]) else {
                panic!("no capture asked")
            };
            let c = Captured {
                access: "a".into(),
                access_placeholder: access.into(),
                refresh_placeholder: Some(refresh.into()),
            };
            let answer = Answered::Captured { id, result: Ok(c) };
            assert_eq!(
                s.answered(&answer.encode().unwrap(), std::slice::from_ref(&other)),
                Some(Changed::Nothing)
            );
            assert!(matches!(s.take(1), Some(Answer::Declined(Declined::Denied(_)))));
            assert!(s.grants[0].captured.is_none());
        }
    }

    /// A capture carries what its CLI's own file records beside its tokens, none a credential,
    /// and a sign-in whose response gave no refresh token holds no placeholder for one (D242,
    /// step 8, ROOT's asks). Do: take Codex's response's tokens, and Claude Code's; send a
    /// capture's word; write a response with a refresh token for a sign-in with no refresh
    /// placeholder, and one without; ask a refresh of such a sign-in. Expect: the ID token taken
    /// inert alone, its signature gone, and the scope as it came, both through the word; the
    /// refresh token with nothing to stand for it refused, never passed; the response without one
    /// written; the refresh passed, nothing of the run's in it.
    #[test]
    fn a_capture_carries_its_inert_id_token_and_scope_and_a_sign_in_may_hold_no_refresh_placeholder() {
        let codex = codex_body();
        let taken = Taken::of(&read(codex.as_bytes()).unwrap().unwrap()).unwrap();
        let inert = format!("{ID_HEADER}.{ID_PAYLOAD}.shards-placeholder");
        assert_eq!(
            taken.id_token.as_deref().map(Vec::as_slice),
            Some(inert.as_bytes())
        );
        assert!(!holds(taken.id_token.as_ref().unwrap(), ID_SIGNATURE));
        let claude = claude_body();
        let taken = Taken::of(&read(claude.as_bytes()).unwrap().unwrap()).unwrap();
        assert_eq!(taken.scope.as_deref(), Some("user:inference user:profile"));
        assert!(taken.id_token.is_none());
        let told = Told::Capture {
            id: 3,
            sign_in: "codex/acct-2".into(),
            taken: Taken::of(&read(codex.as_bytes()).unwrap().unwrap()).unwrap(),
        };
        let Some(Told::Capture { taken, .. }) = Told::decode(&told.encode().unwrap()) else {
            panic!("no capture read back")
        };
        assert_eq!(
            taken.id_token.as_deref().map(Vec::as_slice),
            Some(inert.as_bytes())
        );
        assert_eq!(taken.scope, None);
        let unrefreshed = Placeholders {
            access: ACCESS_PLACEHOLDER.into(),
            refresh: None,
        };
        let tokens = read(claude.as_bytes()).unwrap().unwrap();
        assert!(matches!(
            write(claude.as_bytes(), &tokens, &unrefreshed),
            Err(Refusal::NoPlaceholder)
        ));
        let bare = format!(r#"{{"access_token":"{ACCESS}","expires_in":28800}}"#);
        let tokens = read(bare.as_bytes()).unwrap().unwrap();
        let written = write(bare.as_bytes(), &tokens, &unrefreshed).unwrap();
        assert!(holds(&written, ACCESS_PLACEHOLDER) && !holds(&written, ACCESS));
        let body = format!(r#"{{"grant_type":"refresh_token","refresh_token":"{REFRESH_PLACEHOLDER}"}}"#);
        let asked = request("platform.claude.com", "/v1/oauth/token", JSON, &body);
        assert_eq!(ask(&CLAUDE_CODE, &asked, &[&unrefreshed]), Ask::Pass);
    }
}
