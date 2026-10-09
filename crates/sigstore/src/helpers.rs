//! BuildKit's policy helpers (github.com/moby/policy-helpers, as buildx v0.37.1 vendors
//! it): an artifact's bundle verified as VerifyArtifact verifies it, and what it reports
//! (types.SignatureInfo), with the kind of signer DetectKind finds.

use crate::Error;
use crate::summary::Summary;
use crate::time::{Time, Zone};
use crate::trusted_root::TrustedRoot;
use crate::verify::{self, Config, Identity, Material, Policy, VerifiedTimestamp};

/// types.Kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    DockerGithubBuilder,
    DockerHardenedImage,
    SelfSignedGithubRepo,
    SelfSigned,
    Untrusted,
}

impl Kind {
    /// The policy input's name for the kind (buildx's toSignatureKind).
    pub fn input_name(self) -> &'static str {
        match self {
            Kind::DockerGithubBuilder => "docker-github-builder",
            Kind::DockerHardenedImage => "docker-hardened-image",
            Kind::SelfSignedGithubRepo => "self-signed-github-repo",
            Kind::SelfSigned => "self-signed",
            Kind::Untrusted => "untrusted",
        }
    }
}

/// types.SignatureType.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    BundleV03,
    SimpleSigningV1,
}

impl SignatureType {
    /// buildx's toSignatureType.
    pub fn input_name(self) -> &'static str {
        match self {
            SignatureType::BundleV03 => "bundle-v0.3",
            SignatureType::SimpleSigningV1 => "simplesigning-v1",
        }
    }
}

/// types.SignatureInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    pub kind: Kind,
    pub signature_type: SignatureType,
    pub signer: Option<Summary>,
    pub timestamps: Vec<VerifiedTimestamp>,
    pub docker_reference: String,
    pub is_dhi: bool,
}

const GITHUB_PREFIX: &str = "https://github.com/";
const BUILDER: &str = "https://github.com/docker/github-builder/.github/workflows/";
const BUILDER_EXPERIMENTAL: &str = "https://github.com/docker/github-builder-experimental/.github/workflows/";
const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";
const SIGSTORE_ISSUER: &str = "CN=sigstore-intermediate,O=sigstore.dev";

impl SignatureInfo {
    /// DetectKind.
    pub fn detect_kind(&self) -> Kind {
        if self.is_dhi && !self.docker_reference.is_empty() {
            return Kind::DockerHardenedImage;
        }
        let Some(s) = &self.signer else {
            return Kind::Untrusted;
        };
        if s.certificate_issuer != SIGSTORE_ISSUER {
            return Kind::Untrusted;
        }
        let e = &s.extensions;
        let signer_builder =
            e.build_signer_uri.starts_with(BUILDER) || e.build_signer_uri.starts_with(BUILDER_EXPERIMENTAL);
        let san_builder = s.subject_alternative_name.starts_with(BUILDER)
            || s.subject_alternative_name.starts_with(BUILDER_EXPERIMENTAL);
        if signer_builder
            && san_builder
            && e.issuer == GITHUB_ISSUER
            && e.source_repository_uri.starts_with(GITHUB_PREFIX)
            && e.runner_environment == "github-hosted"
            && !self.timestamps.is_empty()
            && self.signature_type == SignatureType::BundleV03
        {
            return Kind::DockerGithubBuilder;
        }
        let workflows = format!("{}/.github/workflows/", e.source_repository_uri);
        if e.issuer == GITHUB_ISSUER
            && e.source_repository_uri.starts_with(GITHUB_PREFIX)
            && e.build_signer_uri.starts_with(&workflows)
            && s.subject_alternative_name.starts_with(&workflows)
            && e.runner_environment == "github-hosted"
        {
            return Kind::SelfSignedGithubRepo;
        }
        Kind::SelfSigned
    }
}

const SLSA_V1: &str = "https://slsa.dev/provenance/v1";
const SLSA_V02: &str = "https://slsa.dev/provenance/v0.2";

/// The options VerifyArtifact verifies with: SCTs, a log entry and an observer
/// timestamp, one each.
pub const ARTIFACT_CONFIG: Config = Config {
    tlog: 1,
    observer: 1,
    sct: 1,
    signed: 0,
    integrated: 0,
    no_observer: false,
};

/// The trusted root, loaded where policy-helpers loads its trust provider: after what
/// is checked without it, so those errors come first. Its error is the whole message.
pub type Root<'r> = dyn Fn() -> Result<&'r TrustedRoot, String> + 'r;

/// VerifyArtifact: the bundle `bundle` over the artifact `digest` (`sha256:…`), against
/// `root`, a SLSA provenance statement required unless `slsa_not_required`.
///
/// A message signature has no statement; Go reads its predicate type from a nil
/// statement and panics, where this refuses it as not SLSA provenance (D105).
pub fn verify_artifact<'r>(
    digest: &str,
    bundle: &[u8],
    root: &Root<'r>,
    zone: Zone,
    slsa_not_required: bool,
) -> Result<SignatureInfo, Error> {
    let (alg, hex) = digest.split_once(':').unwrap_or(("", digest));
    let raw = crate::signature::hex_decode(hex).ok_or_else(|| {
        Error(format!(
            "decoding digest {digest}: {}",
            crate::signature::hex_error(hex)
        ))
    })?;
    let b = crate::bundle::parse(bundle)?;
    let root = root().map_err(Error)?;
    let material = Material {
        root,
        key: None,
        fulcio: true,
    };
    let policy = Policy {
        digest: Some((alg.to_string(), raw)),
        identity: Identity::Any,
    };
    let outcome = verify::verify_entity(
        &verify::Entity::from_bundle(&b),
        &material,
        &ARTIFACT_CONFIG,
        &policy,
        zone,
    )
    .map_err(|e| Error(format!("verifying bundle: {e}")))?;
    let Some(signer) = outcome.certificate else {
        return Err(Error("no valid signatures found".into()));
    };
    let predicate = outcome
        .statement
        .as_ref()
        .map(|s| s.predicate_type.clone())
        .unwrap_or_default();
    if !slsa_not_required && predicate != SLSA_V1 && predicate != SLSA_V02 {
        return Err(Error(format!(
            "unexpected predicate type {}, expecting SLSA provenance",
            shards_dockerfile::go::quote(predicate.as_bytes())
        )));
    }
    let mut si = SignatureInfo {
        kind: Kind::Untrusted,
        signature_type: SignatureType::BundleV03,
        signer: Some(signer),
        timestamps: outcome.timestamps,
        docker_reference: String::new(),
        is_dhi: false,
    };
    si.kind = si.detect_kind();
    Ok(si)
}

impl Time {
    /// time.Time's JSON: RFC 3339 with nanoseconds, trailing zeros trimmed, in its zone.
    pub fn rfc3339_nano(&self) -> String {
        let base = self.rfc3339();
        if self.nanos == 0 {
            return base;
        }
        let frac = format!("{:09}", self.nanos);
        let frac = frac.trim_end_matches('0');
        // Insert the fraction before the zone: after the seconds (19 characters).
        match (base.get(..19), base.get(19..)) {
            (Some(head), Some(zone)) => format!("{head}.{frac}{zone}"),
            _ => base,
        }
    }
}
