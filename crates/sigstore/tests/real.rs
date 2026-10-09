//! Sigstore's own trusted root and a real bundle (moby/buildkit v0.28.1's arm64
//! attestation signature, as Docker's GitHub builder signed it) read and checked piece
//! by piece.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use shards_sigstore::time::Time;
use shards_sigstore::x509::{Certificate, Eku, Options, Pool};

fn root() -> shards_sigstore::trusted_root::TrustedRoot {
    shards_sigstore::trusted_root::parse(include_bytes!(
        "../../tuf/roots/sigstore/targets/trusted_root.json"
    ))
    .unwrap()
}

fn bundle() -> serde_json::Value {
    serde_json::from_slice(include_bytes!(
        "../testdata/real/buildkit-v0.28.1-arm64.bundle.json"
    ))
    .unwrap()
}

#[test]
fn sigstore_s_trusted_root_reads() {
    let r = root();
    assert_eq!(r.tlogs.len(), 2);
    assert_eq!(r.certificate_authorities.len(), 2);
    assert_eq!(r.ctlogs.len(), 2);
    assert_eq!(r.timestamp_authorities.len(), 1);
    let rekor = &r.tlogs["c0d23d6ad406973f9559f3ba2d1ca01f84147d8ffc5b8445c224f98b9591801d"];
    assert_eq!(rekor.base_url, "https://rekor.sigstore.dev");
    assert!(r.timestamp_authorities[0].root.is_some());
}

#[test]
fn the_bundle_s_certificate_chains_to_fulcio_when_logged() {
    let r = root();
    let b = bundle();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(
            b["verificationMaterial"]["certificate"]["rawBytes"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    let leaf = Certificate::parse(&raw).unwrap();
    let integrated: i64 = b["verificationMaterial"]["tlogEntries"][0]["integratedTime"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let ca = r
        .certificate_authorities
        .iter()
        .find(|ca| {
            ca.start.is_some_and(|s| s.secs <= integrated) && ca.end.is_none_or(|e| e.secs >= integrated)
        })
        .unwrap();
    let mut roots = Pool::default();
    roots.add(ca.root.clone());
    let mut inter = Pool::default();
    for c in &ca.intermediates {
        inter.add(c.clone());
    }
    let opts = Options {
        roots: &roots,
        intermediates: &inter,
        now: Time::utc(integrated, 0),
        key_usages: vec![Eku::CodeSigning],
    };
    let chains = leaf.verify(&opts).unwrap();
    assert_eq!(chains.len(), 1);
    assert_eq!(chains[0].len(), 3);
    assert_eq!(leaf.issuer_string(), "CN=sigstore-intermediate,O=sigstore.dev");
    // Outside its ten minutes the leaf is not valid.
    let late = Options {
        now: Time::utc(integrated + 3600, 0),
        ..opts
    };
    assert!(
        leaf.verify(&late)
            .unwrap_err()
            .0
            .starts_with("x509: certificate has expired or is not yet valid: current time")
    );
}

#[test]
fn the_bundle_reads_and_its_envelope_verifies_by_the_artifact_s_digest() {
    use shards_sigstore::bundle::{self, Material, SignatureContent};
    use shards_sigstore::signature::{compat_verifier, verify_with_digest};
    let b = bundle::parse(include_bytes!(
        "../testdata/real/buildkit-v0.28.1-arm64.bundle.json"
    ))
    .unwrap();
    assert_eq!(b.version, "v0.3");
    let Material::Certificate(Some(raw)) = &b.material else {
        panic!("not a certificate");
    };
    let leaf = Certificate::parse(raw).unwrap();
    let v = compat_verifier(&leaf.public_key, true, true).unwrap();
    let content = b.signature_content().unwrap();
    assert!(matches!(content, SignatureContent::Envelope(_)));
    let hex = b"8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898";
    let digest: Vec<u8> = hex
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    verify_with_digest(&v, &content, "sha256", &digest).unwrap();
    let mut wrong = digest.clone();
    wrong[0] ^= 1;
    assert_eq!(
        verify_with_digest(&v, &content, "sha256", &wrong).unwrap_err(),
        "provided artifact digest does not match any digest in statement"
    );
    let summary = shards_sigstore::summary::summarize(&leaf).unwrap();
    assert_eq!(
        summary.certificate_issuer,
        "CN=sigstore-intermediate,O=sigstore.dev"
    );
    assert_eq!(
        summary.extensions.issuer,
        "https://token.actions.githubusercontent.com"
    );
    assert!(
        summary
            .subject_alternative_name
            .starts_with("https://github.com/")
    );
}

/// The registry's answers for moby/buildkit v0.28.1, as fetched: its blobs by digest, and
/// the referrers of its arm64 attestation manifest.
struct Hub(Vec<(&'static str, &'static [u8])>);

impl shards_sigstore::image::Provider for Hub {
    fn referrers(
        &self,
        digest: &str,
        _: &[&str],
        _: &[(&str, &str)],
    ) -> Result<Vec<shards_sigstore::image::Descriptor>, String> {
        if digest != "sha256:8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898" {
            return Ok(Vec::new());
        }
        Ok(shards_sigstore::image::parse_index(include_bytes!(
            "../testdata/real/buildkit-v0.28.1.referrers.json"
        ))?
        .manifests)
    }

    fn read(&self, desc: &shards_sigstore::image::Descriptor) -> Result<Vec<u8>, String> {
        self.0
            .iter()
            .find(|(d, _)| *d == desc.digest)
            .map(|(_, b)| b.to_vec())
            .ok_or_else(|| format!("{}: not found", desc.digest))
    }
}

fn hub() -> Hub {
    Hub(vec![
        (
            "sha256:a82d1ab899cda51aade6fe818d71e4b58c4079e047a0cf29dbb93b2b0465ea69",
            include_bytes!("../testdata/real/buildkit-v0.28.1.index.json"),
        ),
        (
            "sha256:8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898",
            include_bytes!("../testdata/real/buildkit-v0.28.1-arm64.attestation.json"),
        ),
        (
            "sha256:64584b03b7c9aff3c8b10a44df9ba7eeb76888382e61f7ffd5ac83d42ff27aac",
            include_bytes!("../testdata/real/buildkit-v0.28.1-arm64.sigmanifest.json"),
        ),
        (
            "sha256:3e7b5c6a1e00b8778fc1c881593220acf37fc953a9ffbfbf316cd5858671cdb2",
            include_bytes!("../testdata/real/buildkit-v0.28.1-arm64.bundle.json"),
        ),
    ])
}

fn index_desc() -> shards_sigstore::image::Descriptor {
    shards_sigstore::image::Descriptor {
        media_type: shards_sigstore::image::MEDIA_INDEX.into(),
        digest: "sha256:a82d1ab899cda51aade6fe818d71e4b58c4079e047a0cf29dbb93b2b0465ea69".into(),
        size: include_bytes!("../testdata/real/buildkit-v0.28.1.index.json").len() as i64,
        ..Default::default()
    }
}

#[test]
fn the_image_s_signature_is_docker_s_github_builder_s() {
    use shards_sigstore::helpers::{Kind, SignatureType};
    use shards_sigstore::platforms::Platform;
    let arm64 = Platform {
        os: "linux".into(),
        architecture: "arm64".into(),
        ..Platform::default()
    };
    let trusted = root();
    let si = shards_sigstore::image::verify_image(
        &hub(),
        &index_desc(),
        &arm64,
        &|| Ok(&trusted),
        shards_sigstore::time::utc,
    )
    .unwrap();
    assert_eq!(si.kind, Kind::DockerGithubBuilder);
    assert_eq!(si.signature_type, SignatureType::BundleV03);
    let kinds: Vec<&str> = si.timestamps.iter().map(|t| t.kind).collect();
    assert_eq!(kinds, ["Tlog", "TimestampAuthority"]);
    let signer = si.signer.unwrap();
    assert_eq!(
        signer.extensions.source_repository_uri,
        "https://github.com/moby/buildkit"
    );
    // Another platform's attestation has no signature here.
    let amd64 = Platform {
        architecture: "amd64".into(),
        ..arm64
    };
    assert_eq!(
        shards_sigstore::image::verify_image(
            &hub(),
            &index_desc(),
            &amd64,
            &|| Ok(&trusted),
            shards_sigstore::time::utc
        )
        .unwrap_err()
        .0,
        "no signature found for image sha256:a82d1ab899cda51aade6fe818d71e4b58c4079e047a0cf29dbb93b2b0465ea69"
    );
}
