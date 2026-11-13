//! The run's CA in its members' trust (D242): for each member whose link reaches a host the
//! run's credential grants name, its image's trust bundle with the CA appended, laid
//! read-only over that bundle as its `/etc/hosts` is laid (domains.rs), and named to it by
//! each variable its TLS stacks read a file of roots from. A member whose link reaches none
//! of them is given nothing: its trust stays its image's.

use std::ffi::CString;

use shards_abi::run::trust as entries;

use crate::netplan::Link;

/// What marks the run's CA in a member's bundle, as BuildKit marks its proxy's
/// ([`crate::proxyca::BEGIN`]), so that a member reading its bundle sees whose CA it is
/// (D242: the change is explicit, never silent).
const BEGIN: &[u8] = b"\n# shards run CA (D242) begin\n";
const END: &[u8] = b"# shards run CA (D242) end\n";

/// The variables a granted member's TLS stacks read a file of roots from, each set to its
/// bundle: Node's and Bun's added certificates (Claude Code), OpenSSL's file, which
/// rustls-native-certs reads too, and Codex's own (plan step 33's research, f57 and f61).
pub const VARIABLES: [&str; 3] = ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "CODEX_CA_CERTIFICATE"];

/// TCP's protocol number (RFC 9293, IANA's assigned internet protocol numbers), as a link's
/// egress names its protocol.
const TCP: u8 = 6;

/// Why a run's CA is not laid in a member's trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Its setup entries are malformed.
    Entries(entries::Error),
    /// Its certificate holds no PEM certificate.
    Certificate,
    /// A host that is not a host's lowered name, as the run host takes one.
    Host(String),
    /// The image holds none of the trust bundles ([`crate::proxyca::BUNDLES`]) to lay it in.
    NoBundle,
    /// The image's bundle could not be read.
    Bundle(String),
    /// The member's environment names a file of roots other than its bundle in one of
    /// [`VARIABLES`]: its own choice, which the run's CA never replaces silently.
    Variable { key: &'static str, value: String },
    /// The member has no scratch, where its bundle is written before it is laid.
    NoScratch,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Entries(e) => write!(f, "the run's CA (D242): {e}"),
            Refused::Certificate => write!(f, "the run's CA (D242): {}", crate::proxyca::NO_CERTIFICATE),
            Refused::Host(h) => write!(f, "the run's CA (D242): {h:?} is not a host's lowered name"),
            Refused::NoBundle => write!(
                f,
                "the run's CA (D242): its image holds no trust bundle to lay it in, none of {}",
                crate::proxyca::BUNDLES
                    .iter()
                    .map(|b| String::from_utf8_lossy(b))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refused::Bundle(e) => write!(f, "the run's CA (D242): its image's trust bundle: {e}"),
            Refused::Variable { key, value } => write!(
                f,
                "its environment sets {key} to {value:?}, a file of roots of its own, where the run's CA (D242) names its trust bundle"
            ),
            Refused::NoScratch => write!(
                f,
                "its image has no /tmp, where its trust bundle with the run's CA (D242) is written to be laid"
            ),
        }
    }
}

/// The run's CA as its setup gives it: its certificate, PEM, and the hosts its grants name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pem: Vec<u8>,
    hosts: Vec<String>,
}

/// What a granted member is given: its image's bundle, where it lies there (resolved), and
/// what is laid over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    pub path: CString,
    pub bundle: Vec<u8>,
}

/// A host as the run host names one (D242, `shards_api::image::request`): a DNS name,
/// lowered, never loopback's.
fn host(h: &str) -> bool {
    shards_dns::host_name(h)
        && !h.bytes().any(|b| b.is_ascii_uppercase())
        && h != "localhost"
        && !h.ends_with(".localhost")
}

impl Run {
    /// The run's CA where `setup` gives one, each entry checked again as the run host made
    /// it (D109): its certificate a PEM certificate, each host a host's lowered name.
    pub fn of(setup: &[Vec<u8>]) -> Result<Option<Run>, Refused> {
        let Some(t) = entries::of(setup).map_err(Refused::Entries)? else {
            return Ok(None);
        };
        if crate::proxyca::first_certificate(t.pem).is_none() {
            return Err(Refused::Certificate);
        }
        let hosts = t
            .hosts
            .iter()
            .map(|h| {
                std::str::from_utf8(h)
                    .ok()
                    .filter(|h| host(h))
                    .map(str::to_string)
                    .ok_or_else(|| Refused::Host(String::from_utf8_lossy(h).into_owned()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(Run {
            pem: t.pem.to_vec(),
            hosts,
        }))
    }

    /// Whether a member with `link` reaches a host the grants name past the microVM: by name,
    /// or on any TCP port, as `shards_net::reaches` holds the whole run (D236), here member by
    /// member. A member with no link reaches nothing past it.
    pub fn reaches(&self, link: Option<&Link>) -> bool {
        link.is_some_and(|l| {
            l.egress.iter().any(|&(proto, _, _)| proto == TCP)
                || self
                    .hosts
                    .iter()
                    .any(|h| l.named.iter().any(|(n, _)| n.eq_ignore_ascii_case(h)))
        })
    }

    /// The bundle `original` with the CA appended between its markers.
    pub fn composed(&self, original: &[u8]) -> Vec<u8> {
        crate::proxyca::appended_between(original, &self.pem, BEGIN, END)
    }

    /// What a granted member is given: the first of `image`'s trust bundles, found and read
    /// as the build finds and reads it (build.rs `find_bundle`, `read_bundle`: in the image,
    /// a regular file, at most [`crate::proxyca::MOST`]), with the CA appended.
    #[cfg(target_os = "linux")]
    pub fn lay(&self, image: &crate::inroot::Root) -> Result<Laid, Refused> {
        let bundle = |e: std::io::Error| Refused::Bundle(e.to_string());
        let path = crate::build::find_bundle(image)
            .map_err(bundle)?
            .ok_or(Refused::NoBundle)?;
        let (original, _) = crate::build::read_bundle(image, &path).map_err(bundle)?;
        let bundle = self.composed(&original);
        let path = CString::new(path).map_err(|_| Refused::Bundle("a path holding a NUL".into()))?;
        Ok(Laid { path, bundle })
    }
}

impl Laid {
    /// `env` with each of [`VARIABLES`] naming the bundle: refused where one already names
    /// another file, a member's own roots, which this never replaces.
    pub fn name_in(&self, env: &mut Vec<CString>) -> Result<(), Refused> {
        let path = self.path.to_bytes();
        for key in VARIABLES {
            let prefix = [key.as_bytes(), b"="].concat();
            let mut named = false;
            for e in env.iter() {
                if let Some(value) = e.to_bytes().strip_prefix(prefix.as_slice()) {
                    if value != path {
                        return Err(Refused::Variable {
                            key,
                            value: String::from_utf8_lossy(value).into_owned(),
                        });
                    }
                    named = true;
                }
            }
            if !named {
                // A path from a CString holds no NUL, nor does a variable's name.
                let variable = CString::new([prefix.as_slice(), path].concat())
                    .map_err(|_| Refused::Bundle("a path holding a NUL".into()))?;
                env.push(variable);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";

    fn setup(hosts: &[&str]) -> Vec<Vec<u8>> {
        let mut s = vec![entries::ca(PEM).unwrap()];
        s.extend(hosts.iter().map(|h| entries::host(h.as_bytes()).unwrap()));
        s
    }

    fn link(egress: Vec<crate::netplan::Egress>, named: &[(&str, u16)]) -> Link {
        Link {
            addresses: Vec::new(),
            addresses6: Vec::new(),
            hosts: Vec::new(),
            egress,
            ingress: Vec::new(),
            dns: false,
            named: named.iter().map(|(h, p)| (h.to_string(), *p)).collect(),
            connects: true,
            accepts: false,
            unix: Vec::new(),
        }
    }

    /// A run with no credential grant has no CA, and nothing is laid; one with grants reads
    /// back its certificate and hosts.
    #[test]
    fn a_run_without_grants_has_no_ca() {
        assert_eq!(Run::of(&[b"caps=0".to_vec()]), Ok(None));
        let run = Run::of(&setup(&["api.anthropic.com", "chatgpt.com"]))
            .unwrap()
            .unwrap();
        assert_eq!(run.hosts, ["api.anthropic.com", "chatgpt.com"]);
    }

    /// Checked again here as the run host made them: a certificate that holds no PEM
    /// certificate, and a host that is not a host's lowered name, are refused.
    #[test]
    fn a_malformed_ca_or_host_is_refused() {
        let no_cert = vec![
            entries::ca(b"not PEM").unwrap(),
            entries::host(b"chatgpt.com").unwrap(),
        ];
        assert_eq!(Run::of(&no_cert), Err(Refused::Certificate));
        for bad in [
            "Chatgpt.com",
            "localhost",
            "a.localhost",
            "10.0.0.1",
            "a..b",
            "-a.com",
        ] {
            assert_eq!(
                Run::of(&setup(&[bad])),
                Err(Refused::Host(bad.to_string())),
                "{bad}"
            );
        }
        assert_eq!(
            Run::of(&setup(&[])),
            Err(Refused::Entries(entries::Error::Unpaired))
        );
    }

    /// A member reaches a granted host by its name (any port, any case), or on any TCP
    /// port; not by another name, not on UDP alone, and not without a link.
    #[test]
    fn a_member_is_granted_where_its_link_reaches_a_granted_host() {
        let run = Run::of(&setup(&["api.anthropic.com"])).unwrap().unwrap();
        assert!(run.reaches(Some(&link(Vec::new(), &[("API.anthropic.com", 443)]))));
        assert!(run.reaches(Some(&link(vec![(TCP, 443, 443)], &[]))));
        assert!(!run.reaches(Some(&link(Vec::new(), &[("chatgpt.com", 443)]))));
        assert!(!run.reaches(Some(&link(vec![(17, 53, 53)], &[]))));
        assert!(!run.reaches(Some(&link(Vec::new(), &[]))));
        assert!(!run.reaches(None));
    }

    /// The bundle given a granted member is its image's, every byte of it, then the CA
    /// between the markers that name it.
    #[test]
    fn the_composed_bundle_holds_the_images_roots_then_the_ca() {
        let run = Run::of(&setup(&["chatgpt.com"])).unwrap().unwrap();
        let image = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let composed = run.composed(image);
        assert!(composed.starts_with(image));
        assert_eq!(
            composed.get(image.len()..).unwrap(),
            [BEGIN, PEM, END].concat().as_slice()
        );
        assert!(crate::proxyca::contains(
            &composed,
            &crate::proxyca::first_certificate(PEM).unwrap()
        ));
        // An image's bundle without its last newline gets one before the marker.
        assert!(run.composed(b"x").starts_with(b"x\n\n# shards run CA"));
    }

    /// Each variable names the bundle; one the member already sets to the bundle stays, set
    /// once; one naming another file refuses the member, never replaced.
    #[test]
    fn the_variables_name_the_bundle_and_a_members_own_roots_are_never_replaced() {
        let laid = Laid {
            path: CString::new("/etc/ssl/certs/ca-certificates.crt").unwrap(),
            bundle: Vec::new(),
        };
        let mut env = vec![
            CString::new("PATH=/usr/bin").unwrap(),
            CString::new("SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt").unwrap(),
        ];
        laid.name_in(&mut env).unwrap();
        let got: Vec<&str> = env.iter().map(|e| e.to_str().unwrap()).collect();
        assert_eq!(
            got,
            [
                "PATH=/usr/bin",
                "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
                "NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt",
                "CODEX_CA_CERTIFICATE=/etc/ssl/certs/ca-certificates.crt",
            ]
        );
        let mut own = vec![CString::new("NODE_EXTRA_CA_CERTS=/home/me/corp.pem").unwrap()];
        assert_eq!(
            laid.name_in(&mut own),
            Err(Refused::Variable {
                key: "NODE_EXTRA_CA_CERTS",
                value: "/home/me/corp.pem".into(),
            })
        );
        assert_eq!(own.len(), 1);
    }

    /// Laid from the image as built: the first of its bundles that is a regular file, an
    /// absolute symlink to it followed in the image, never the host's; an image with no
    /// bundle refuses it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_bundle_is_found_in_the_image_and_an_image_without_one_refuses() {
        let image = shards_testdir::TempDir::new("init-trust").unwrap();
        let at = |p: &str| image.join(p.trim_start_matches('/'));
        let run = Run::of(&setup(&["chatgpt.com"])).unwrap().unwrap();
        let root = crate::inroot::Root::open(&image).unwrap();
        assert_eq!(run.lay(&root), Err(Refused::NoBundle));
        std::fs::create_dir_all(at("/etc/ssl/certs")).unwrap();
        std::fs::create_dir_all(at("/certs")).unwrap();
        std::fs::write(at("/certs/real.pem"), b"roots\n").unwrap();
        std::os::unix::fs::symlink("/certs/real.pem", at("/etc/ssl/certs/ca-certificates.crt")).unwrap();
        let laid = run.lay(&root).unwrap();
        assert_eq!(laid.path.as_bytes(), b"/certs/real.pem");
        assert_eq!(laid.bundle, run.composed(b"roots\n"));
        // The image's own bundle is left as it was: what is laid is a member's alone.
        assert_eq!(std::fs::read(at("/certs/real.pem")).unwrap(), b"roots\n");
    }
}
