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
