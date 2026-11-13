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

/// Why a run's grants are refused for their placeholders (D242), typed, as each side that
/// takes them (the orchestrator's, the run's host, its network process) checks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The grant of this reference has a placeholder that is no [`placeholder`].
    NotAToken(String),
    /// The grants of these two references share a placeholder, which could stand for
    /// neither alone.
    Shared(String, String),
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
        }
    }
}

/// Each grant's placeholder a [`placeholder`], and one credential's alone: grants of one
/// reference may share theirs, as each stands for the same credential.
pub fn check(grants: &[CredentialGrant]) -> Result<(), Refused> {
    for (i, grant) in grants.iter().enumerate() {
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
}
