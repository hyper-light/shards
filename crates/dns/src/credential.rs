//! A run's credential grants and what the injector decides with them (D242): shared by the
//! orchestrator's side (shards-api, which checks a grant and carries it to the run's host)
//! and the network process (shards-net, whose injector decides with it), as both name a
//! grant's hosts by this crate's rules ([`crate::host_name`], [`crate::HOST`]).

use core::fmt;

/// A credential grant by reference (D242): the credential named `reference` (a secret's path
/// in the orchestrator's store, never a value) may be filled in at the VM's egress toward the
/// hosts in `toward`, and no others, where its `placeholder` stands in a request.
///
/// Each host in `toward` is a DNS name lowered, as a network's `--egress=HOST:PORT` names a
/// destination's host by name (D236), or [`HOST`](crate::HOST), the host itself; never an
/// address (TLS names a server by its DNS name alone, RFC 6066 §3), never with a port, and
/// never `localhost`. The orchestrator's side checks that where it builds a run's request; a
/// side reading a wire takes them as bytes and checks them again (D109).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct CredentialGrant {
    pub reference: String,
    pub toward: Vec<String>,
    /// What stands for the credential in a request (D242): minted by the orchestrator where
    /// it gives the credential to a member, a token ([`placeholder`]) its member sends in a
    /// credential field as it would the value. Holding it is the capability, so it never
    /// prints.
    pub placeholder: String,
}

/// Shows a grant's reference and hosts, and of its placeholder its length alone.
impl fmt::Debug for CredentialGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialGrant")
            .field("reference", &self.reference)
            .field("toward", &self.toward)
            .field("placeholder", &format_args!("{} bytes", self.placeholder.len()))
            .finish()
    }
}

/// What the credential injector (a gateway, not a proxy: RFC 9110 §3.7, so it may set the
/// `Authorization` field a proxy may not, §11.6.2) does with a request it has terminated,
/// decided by [`decide`] (D242). The injector renders the outcome; this enum is the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fill {
    /// Fill the credential named `reference` in where its placeholder stands, then forward.
    With(String),
    /// No grant's placeholder toward this host stands there: forward the request as it is,
    /// with no credential (default deny).
    Pass,
    /// The request's host disagrees with the TLS name the flow arrived on, so the flow is
    /// misdirected (RFC 9110 §7.4): refuse it 421, never guessing which host was meant.
    Misdirected,
}

/// What the injector fills into a request to a credential-granted host (D242). `sni` is the
/// TLS server name the flow was terminated on, `host` the request's host (its HTTP/1.1 `Host`
/// or HTTP/2 `:authority`, the authority alone, without a port), `token` what stands in one of
/// its credential fields (none for a field that holds none), and `grants` the VM's.
///
/// A value is filled only where the SNI, the host and a grant's host all agree and the token
/// is that grant's placeholder. The SNI alone is not enough (a client picks its `Host` freely:
/// domain fronting), so a `Host` that differs from the SNI is
/// [`Misdirected`](Fill::Misdirected), whatever it carries. A placeholder toward a host its
/// grant does not name is [`Pass`](Fill::Pass): the request goes out with the placeholder,
/// carrying no credential. Hosts match ASCII case-insensitively, as DNS names compare (RFC
/// 9549 §2.3); a placeholder matches exactly, compared in time that does not depend on where
/// it first differs, as what a request carries there is its sender's to choose.
pub fn decide(grants: &[CredentialGrant], sni: &str, host: &str, token: Option<&str>) -> Fill {
    if !sni.eq_ignore_ascii_case(host) {
        return Fill::Misdirected;
    }
    let Some(token) = token else {
        return Fill::Pass;
    };
    for grant in grants {
        if same(grant.placeholder.as_bytes(), token.as_bytes())
            && grant.toward.iter().any(|h| h.eq_ignore_ascii_case(host))
        {
            return Fill::With(grant.reference.clone());
        }
    }
    Fill::Pass
}

/// Whether `a` and `b` are the same bytes, every byte of equal lengths compared.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |differ, (x, y)| differ | (x ^ y)) == 0
}

/// Whether `s` is a placeholder a member can send where its credential goes: a token as a
/// bearer credential is one, RFC 6750 §2.1's `b64token` (`1*( ALPHA / DIGIT / "-" / "." / "_"
/// / "~" / "+" / "/" ) *"="`), which an `x-api-key` field's value may be too (RFC 9110 §5.5's
/// field-vchar holds each). kiln's broker mints them so: a visible marker, then 256 random bits
/// in hex.
pub fn placeholder(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/'))
}

/// The most a grant's [`Field`] holds: bytes of a reference, a placeholder or a sign-in's name,
/// or hosts of a grant. The network process's wire carries each behind a two-byte length,
/// network order (shards-net's `encode_grants`, `NET_CREDENTIALS`, and `signin::encode_grants`,
/// `NET_SIGN_INS`), so one past it could never reach the injector: the wire's own bound, which
/// [`check`] and [`check_sign_ins`] hold on every side that takes a grant.
pub const WIRE_MOST: usize = u16::MAX as usize;

/// A grant's field that the network process's wire carries behind a two-byte length
/// ([`WIRE_MOST`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Reference,
    Placeholder,
    Hosts,
    SignIn,
    SignInHosts,
    Access,
    AccessPlaceholder,
    RefreshPlaceholder,
    /// A run's upstream root, its DER's bytes (`NET_CREDENTIALS`, ahead of the grants).
    UpstreamRoot,
    /// A run's upstream roots, their count.
    UpstreamRoots,
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Field::Reference => "credential grant's reference",
            Field::Placeholder => "credential grant's placeholder",
            Field::Hosts => "credential grant's hosts",
            Field::SignIn => "sign-in grant's name",
            Field::SignInHosts => "sign-in grant's hosts",
            Field::Access => "sign-in grant's access token's reference",
            Field::AccessPlaceholder => "sign-in grant's access token's placeholder",
            Field::RefreshPlaceholder => "sign-in grant's refresh token's placeholder",
            Field::UpstreamRoot => "upstream root",
            Field::UpstreamRoots => "run's upstream roots",
        })
    }
}

/// Why a run's grants are refused for their placeholders or their fields' lengths (D242),
/// typed, as each side that takes them (the orchestrator's, the run's host, its network
/// process) checks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The grant of this reference has a placeholder that is no [`placeholder`].
    NotAToken(String),
    /// The grants of these two references share a placeholder, which could stand for
    /// neither alone.
    Shared(String, String),
    /// The grant at `place` among the run's credential grants or its sign-in grants (as
    /// `field` names) holds `len` in `field`, past [`WIRE_MOST`]. Told by its place and length,
    /// never its bytes: a placeholder is a capability, and the field may be any size.
    TooLong { field: Field, place: usize, len: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotAToken(reference) => write!(
                f,
                "{reference}: its placeholder is not a token a request carries a credential as \
                 (RFC 6750 §2.1's b64token)"
            ),
            Refused::Shared(a, b) => write!(f, "{a} and {b}: one placeholder stands for two credentials"),
            Refused::TooLong { field, place, len } => {
                let unit = match field {
                    Field::Hosts | Field::SignInHosts => "hosts",
                    Field::UpstreamRoots => "roots",
                    _ => "bytes",
                };
                write!(
                    f,
                    "the {field} at {place}: {len} {unit}, past the {WIRE_MOST} the network process's \
                     wire carries (a two-byte length)"
                )
            }
        }
    }
}

/// `len` of `field` within [`WIRE_MOST`], else refused.
fn within(field: Field, place: usize, len: usize) -> Result<(), Refused> {
    if len > WIRE_MOST {
        return Err(Refused::TooLong { field, place, len });
    }
    Ok(())
}

/// A run's upstream roots (D242), each a certificate's DER, and their count, within
/// [`WIRE_MOST`]: `NET_CREDENTIALS` carries each behind a two-byte length, and their count behind
/// two more (shards-net's `encode_credentials`). Their place in `TooLong` is 0 for their count.
pub fn check_roots(roots: &[Vec<u8>]) -> Result<(), Refused> {
    within(Field::UpstreamRoots, 0, roots.len())?;
    for (i, root) in roots.iter().enumerate() {
        within(Field::UpstreamRoot, i, root.len())?;
    }
    Ok(())
}

/// Each grant's reference, placeholder and hosts within [`WIRE_MOST`], its placeholder a
/// [`placeholder`], and one credential's alone: grants of one reference may share theirs, as
/// each stands for the same credential.
pub fn check(grants: &[CredentialGrant]) -> Result<(), Refused> {
    for (i, grant) in grants.iter().enumerate() {
        within(Field::Reference, i, grant.reference.len())?;
        within(Field::Placeholder, i, grant.placeholder.len())?;
        within(Field::Hosts, i, grant.toward.len())?;
        if !placeholder(&grant.placeholder) {
            return Err(Refused::NotAToken(grant.reference.clone()));
        }
        if let Some(other) = grants
            .iter()
            .take(i)
            .find(|g| g.placeholder == grant.placeholder && g.reference != grant.reference)
        {
            return Err(Refused::Shared(other.reference.clone(), grant.reference.clone()));
        }
    }
    Ok(())
}

/// A CLI whose own sign-in a run may capture outside its microVM (D242, step 8); its token
/// endpoint's profile is the network process's (shards-net's `signin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    ClaudeCode,
    Codex,
}

impl Cli {
    /// Its number on the wires that carry it.
    pub fn code(self) -> u8 {
        match self {
            Cli::ClaudeCode => 1,
            Cli::Codex => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Cli> {
        match code {
            1 => Some(Cli::ClaudeCode),
            2 => Some(Cli::Codex),
            _ => None,
        }
    }
}

/// A sign-in grant (D242, step 8): the run may sign `cli` in as the sign-in named `sign_in` (a
/// name in the orchestrator's store, never a value), its token response taken outside the
/// microVM and its CLI given placeholders, its refreshes and revocations answered outside; its
/// access token is filled in toward the hosts in `toward` alone, by the rules a
/// [`CredentialGrant`]'s hosts keep. `captured` is what an earlier capture of the sign-in gave,
/// where there was one, so that a later run's CLI holds the same placeholders.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInGrant {
    pub sign_in: String,
    pub cli: Cli,
    pub toward: Vec<String>,
    pub captured: Option<Captured>,
}

/// What a sign-in's capture gave (D242, step 8): its access token's reference in the
/// orchestrator's store and the placeholder that stands for it, and its refresh token's
/// placeholder, which a refresh or a revocation names and nothing fills: none where its token
/// response gave no refresh token, so that no placeholder stands for a token that is not, and its
/// CLI, holding none, asks no refresh. Holding a placeholder is the capability, so none prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Captured {
    pub access: String,
    pub access_placeholder: String,
    pub refresh_placeholder: Option<String>,
}

impl fmt::Debug for SignInGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignInGrant")
            .field("sign_in", &self.sign_in)
            .field("cli", &self.cli)
            .field("toward", &self.toward)
            .field("captured", &self.captured)
            .finish()
    }
}

/// Shows its reference, and of its placeholders their lengths alone.
impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Captured")
            .field("access", &self.access)
            .field(
                "access_placeholder",
                &format_args!("{} bytes", self.access_placeholder.len()),
            )
            .field(
                "refresh_placeholder",
                &self
                    .refresh_placeholder
                    .as_ref()
                    .map(|p| format!("{} bytes", p.len())),
            )
            .finish()
    }
}

impl Captured {
    /// Its access token as a credential grant toward `toward`, which the injector fills where
    /// its placeholder stands.
    pub fn grant(&self, toward: &[String]) -> CredentialGrant {
        CredentialGrant {
            reference: self.access.clone(),
            toward: toward.to_vec(),
            placeholder: self.access_placeholder.clone(),
        }
    }
}

/// A run's grants and its sign-ins' captures checked as one (D242): each sign-in's name, hosts
/// and capture within [`WIRE_MOST`], each placeholder a token ([`placeholder`]) and one
/// credential's alone, a sign-in's refresh placeholder too, which no grant or other sign-in may
/// share.
pub fn check_sign_ins(grants: &[CredentialGrant], sign_ins: &[SignInGrant]) -> Result<(), Refused> {
    for (i, s) in sign_ins.iter().enumerate() {
        within(Field::SignIn, i, s.sign_in.len())?;
        within(Field::SignInHosts, i, s.toward.len())?;
        if let Some(c) = &s.captured {
            within(Field::Access, i, c.access.len())?;
            within(Field::AccessPlaceholder, i, c.access_placeholder.len())?;
            within(
                Field::RefreshPlaceholder,
                i,
                c.refresh_placeholder.as_ref().map_or(0, String::len),
            )?;
        }
    }
    let mut all = grants.to_vec();
    all.extend(
        sign_ins
            .iter()
            .filter_map(|s| s.captured.as_ref().map(|c| c.grant(&s.toward))),
    );
    check(&all)?;
    for (i, s) in sign_ins.iter().enumerate() {
        let Some(refresh) = s.captured.as_ref().and_then(|c| c.refresh_placeholder.as_ref()) else {
            continue;
        };
        if !placeholder(refresh) {
            return Err(Refused::NotAToken(s.sign_in.clone()));
        }
        let shared = all
            .iter()
            .find(|g| g.placeholder == *refresh)
            .map(|g| g.reference.clone())
            .or_else(|| {
                sign_ins
                    .iter()
                    .take(i)
                    .filter_map(|o| o.captured.as_ref().map(|oc| (o, oc)))
                    .find(|(_, oc)| oc.refresh_placeholder.as_ref() == Some(refresh))
                    .map(|(o, _)| o.sign_in.clone())
            });
        if let Some(other) = shared {
            return Err(Refused::Shared(other, s.sign_in.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CODEX: &str = "kiln-placeholder-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn grants() -> Vec<CredentialGrant> {
        vec![
            CredentialGrant {
                reference: "anthropic/claude-max".into(),
                toward: vec!["api.anthropic.com".into(), "platform.claude.com".into()],
                placeholder: CLAUDE.into(),
            },
            CredentialGrant {
                reference: "openai/chatgpt".into(),
                toward: vec!["chatgpt.com".into()],
                placeholder: CODEX.into(),
            },
        ]
    }

    /// The injector fills a credential only where the TLS name, the request's host and a
    /// grant's host all agree and the grant's own placeholder stands in the request (D242):
    /// a grant toward one host fills no request to another, a placeholder toward a host its
    /// grant does not name goes out bare, a host that differs from the SNI is misdirected
    /// whatever it carries, and hosts match whatever their ASCII case (R10: without the
    /// host's check, or the placeholder's, this fails).
    #[test]
    fn decide_fills_only_a_grants_placeholder_on_an_agreed_granted_host() {
        let grants = grants();
        let with = |r: &str| Fill::With(r.into());
        assert_eq!(
            decide(&grants, "api.anthropic.com", "api.anthropic.com", Some(CLAUDE)),
            with("anthropic/claude-max")
        );
        assert_eq!(
            decide(
                &grants,
                "platform.claude.com",
                "platform.claude.com",
                Some(CLAUDE)
            ),
            with("anthropic/claude-max")
        );
        assert_eq!(
            decide(&grants, "chatgpt.com", "chatgpt.com", Some(CODEX)),
            with("openai/chatgpt")
        );
        // Case folds, as DNS names compare (RFC 9549 §2.3).
        assert_eq!(
            decide(&grants, "API.Anthropic.Com", "api.anthropic.COM", Some(CLAUDE)),
            with("anthropic/claude-max")
        );
        // A placeholder toward a host its grant does not name: no credential (f32, f33),
        // though another grant's host it is.
        assert_eq!(
            decide(&grants, "chatgpt.com", "chatgpt.com", Some(CLAUDE)),
            Fill::Pass
        );
        assert_eq!(
            decide(&grants, "evil.example", "evil.example", Some(CLAUDE)),
            Fill::Pass
        );
        // No placeholder, another token, one a byte short or of another case: nothing.
        assert_eq!(
            decide(&grants, "api.anthropic.com", "api.anthropic.com", None),
            Fill::Pass
        );
        let short = CLAUDE.strip_suffix('f').unwrap();
        for token in ["sk-ant-real-looking", short, &CLAUDE.to_uppercase()] {
            assert_eq!(
                decide(&grants, "api.anthropic.com", "api.anthropic.com", Some(token)),
                Fill::Pass,
                "{token}"
            );
        }
        assert_eq!(
            decide(&[], "api.anthropic.com", "api.anthropic.com", Some(CLAUDE)),
            Fill::Pass
        );
        // The request's host disagrees with the TLS name it arrived on: misdirected, never
        // filled, though the SNI is a granted host (the SNI alone is not enough).
        assert_eq!(
            decide(&grants, "api.anthropic.com", "evil.example", Some(CLAUDE)),
            Fill::Misdirected
        );
        assert_eq!(
            decide(&grants, "chatgpt.com", "api.anthropic.com", None),
            Fill::Misdirected
        );
    }

    /// A placeholder is a bearer token's `b64token` (RFC 6750 §2.1): kiln's, and one ending
    /// in padding, are; an empty one, padding alone, and one holding a space, a quote, a
    /// colon, a comma or a control are not.
    #[test]
    fn a_placeholder_is_a_bearer_token() {
        for ok in [CLAUDE, "a", "A.b_c~d+e/f-9", "abc==", "x="] {
            assert!(placeholder(ok), "{ok:?} refused");
        }
        for bad in [
            "", "=", "==", "a b", "a\"b", "a:b", "a,b", "a\tb", "a\r\nb", "é", "a=b",
        ] {
            assert!(!placeholder(bad), "{bad:?} taken");
        }
    }

    /// Grants are taken whose placeholders are tokens, each one credential's (one credential
    /// granted twice may give both one); refused, naming the grant, where one is no token or
    /// two credentials share one (R10: without either check, its case is taken).
    #[test]
    fn each_placeholder_is_a_token_and_one_credentials_alone() {
        let mut g = grants();
        assert_eq!(check(&g), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        g.push(CredentialGrant {
            toward: vec!["claude.ai".into()],
            ..g[0].clone()
        });
        assert_eq!(
            check(&g),
            Ok(()),
            "one credential granted twice with one placeholder"
        );
        g[2].reference = "anthropic/console".into();
        assert_eq!(
            check(&g),
            Err(Refused::Shared(
                "anthropic/claude-max".into(),
                "anthropic/console".into()
            ))
        );
        let mut g = grants();
        g[1].placeholder = "two words".into();
        assert_eq!(check(&g), Err(Refused::NotAToken("openai/chatgpt".into())));
        g[1].placeholder = String::new();
        assert_eq!(check(&g), Err(Refused::NotAToken("openai/chatgpt".into())));
    }

    /// A grant prints its reference and hosts, which name it, and of its placeholder, the
    /// capability its member holds, its length alone.
    #[test]
    fn a_grants_placeholder_never_prints() {
        let shown = format!("{:?}", grants()[0]);
        assert!(shown.contains("anthropic/claude-max"), "{shown}");
        assert!(shown.contains("api.anthropic.com"), "{shown}");
        assert!(!shown.contains(CLAUDE), "{shown}");
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
        assert!(shown.contains(&format!("{} bytes", CLAUDE.len())), "{shown}");
    }

    #[test]
    fn a_sign_ins_placeholders_are_tokens_and_one_credentials_alone() {
        let captured = Captured {
            access: "sign-ins/c/access".into(),
            access_placeholder: CLAUDE.into(),
            refresh_placeholder: Some(CODEX.into()),
        };
        let sign_in = SignInGrant {
            sign_in: "c".into(),
            cli: Cli::ClaudeCode,
            toward: vec!["api.anthropic.com".into()],
            captured: Some(captured.clone()),
        };
        assert_eq!(check_sign_ins(&[], std::slice::from_ref(&sign_in)), Ok(()));
        // A capture that gave no refresh token holds no placeholder for one.
        let unrefreshed = SignInGrant {
            captured: Some(Captured {
                refresh_placeholder: None,
                ..captured.clone()
            }),
            ..sign_in.clone()
        };
        assert_eq!(check_sign_ins(&[], &[unrefreshed]), Ok(()));
        for placeholder in [CLAUDE, CODEX] {
            let grant = CredentialGrant {
                reference: "openai/chatgpt".into(),
                toward: vec!["chatgpt.com".into()],
                placeholder: placeholder.into(),
            };
            assert!(
                matches!(
                    check_sign_ins(&[grant], std::slice::from_ref(&sign_in)),
                    Err(Refused::Shared(..))
                ),
                "a sign-in's placeholder shared with a grant"
            );
        }
        let bad = SignInGrant {
            captured: Some(Captured {
                refresh_placeholder: Some("a b".into()),
                ..captured.clone()
            }),
            ..sign_in.clone()
        };
        assert_eq!(check_sign_ins(&[], &[bad]), Err(Refused::NotAToken("c".into())));
        let shown = format!("{sign_in:?}");
        assert!(!shown.contains(CLAUDE) && !shown.contains(CODEX), "{shown}");
        assert_eq!(Cli::from_code(Cli::Codex.code()), Some(Cli::Codex));
        assert_eq!(Cli::from_code(Cli::ClaudeCode.code()), Some(Cli::ClaudeCode));
        assert_eq!(Cli::from_code(0), None);
    }
}
