//! An image's signatures in a policy's input, as buildx v0.37.1 makes them
//! (policy/signatures.go `parseSignatures`, types.go `AttestationSignature`): the
//! attestation chain BuildKit resolved, read back through buildx's acProvider, verified by
//! the policy helpers' VerifyImage against Sigstore's trusted root (D104), which buildx
//! keeps under `~/.docker/buildx/policy/tuf` and shards under its home's `policy/tuf`.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use shards_cmdline::buildflags::LogLevel;
use shards_sigstore::helpers::SignatureInfo;
use shards_sigstore::image::{self as sig, Descriptor, Provider};
use shards_sigstore::trusted_root::TrustedRoot;
use shards_tuf::client::Stage;
use shards_tuf::{Fetch, FetchError};

use super::AttestationChain;
use super::input::Json;
use super::provenance::Provenance;

/// This process's zone at an instant, as Go's `time.Local` has it.
pub fn local_offset(secs: i64) -> i32 {
    #[cfg(unix)]
    {
        i32::try_from(crate::cli::listing::local(secs).offset).unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = secs;
        0
    }
}

/// go-tuf's DefaultFetcher over shards' HTTP client: the platform's roots, the
/// environment's proxies, redirects followed, a 200 or ErrDownloadHTTP, at most `max`
/// octets.
struct Web;

impl Fetch for Web {
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, FetchError> {
        use std::io::Read;
        let other = |e: String| FetchError::Other(e);
        let config =
            shards_registry::tls::client_config(Vec::new(), None).map_err(|e| other(e.to_string()))?;
        let client = shards_registry::http::Client::new(
            Box::new(move |_| Ok(config.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
            std::env::var(k).ok()
        }));
        let parsed = shards_registry::url::Url::parse(url).map_err(|e| other(e.to_string()))?;
        let request = shards_registry::http::Request {
            method: "GET",
            url: &parsed,
            headers: &[],
            body: &[],
            file: None,
        };
        let mut response = client
            .follow(
                &request,
                &|_| Ok(None),
                shards_registry::http::Redirects::Anywhere,
            )
            .map_err(|e| other(e.to_string()))?;
        if response.status != 200 {
            return Err(FetchError::Status {
                url: url.to_string(),
                code: response.status,
            });
        }
        if let Some(length) = response
            .header("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok())
            && length > max
        {
            return Err(FetchError::TooLong {
                url: url.to_string(),
                length,
                max,
            });
        }
        let mut body = Vec::new();
        response
            .by_ref()
            .take(max.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|e| other(e.to_string()))?;
        if body.len() as u64 > max {
            return Err(FetchError::TooLong {
                url: url.to_string(),
                length: body.len() as u64,
                max,
            });
        }
        Ok(body)
    }
}

/// One build's trust provider (buildx's SignatureVerifier, made once per build): Sigstore's
/// trusted root from the cache under the home's `policy/tuf`, refreshed from Sigstore's
/// repository when first needed and kept for the build. A failure is not kept: the next
/// need tries again, as loadTrustProvider does.
///
/// It keeps what each attestation chain read as for the build, too: a source is checked
/// by each policy, for each stage and platform that loads it, and reading its chain costs
/// more than the rest of a check (M127). A chain is known by the SHA-256 of all it holds,
/// so a reading is never another chain's.
#[derive(Default)]
pub struct Trust {
    root: OnceLock<TrustedRoot>,
    read: Mutex<Read>,
}

/// Chains' readings: provenances, with what reading each said, by chain; signatures by
/// chain and platform.
#[derive(Default)]
struct Read {
    provenance: BTreeMap<[u8; 32], ProvenanceReading>,
    signatures: BTreeMap<[u8; 32], Result<Option<Vec<Json>>, String>>,
}

/// A chain's provenance as provenance::parse read it, and what it said reading it.
type ProvenanceReading = (Result<Option<Provenance>, String>, Vec<(LogLevel, String)>);

/// The SHA-256 of all `chain` holds: its digests, and each blob's descriptor and bytes,
/// each length first.
pub fn chain_key(chain: &AttestationChain) -> [u8; 32] {
    let mut h = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut put = |b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    put(chain.root.as_bytes());
    put(chain.attestation_manifest.as_bytes());
    put(&(chain.signature_manifests.len() as u64).to_le_bytes());
    for s in &chain.signature_manifests {
        put(s.as_bytes());
    }
    put(&(chain.blobs.len() as u64).to_le_bytes());
    for (k, (desc, data)) in &chain.blobs {
        put(k.as_bytes());
        put(format!("{desc:?}").as_bytes());
        put(data);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(h.finish().as_ref());
    key
}

impl Trust {
    fn read(&self) -> std::sync::MutexGuard<'_, Read> {
        self.read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The provenance of the chain `key` names (provenance::parse), read once a build:
    /// what reading it said is said again each time.
    pub fn provenance(
        &self,
        key: &[u8; 32],
        chain: &AttestationChain,
        log: &mut super::provenance::Log<'_>,
    ) -> Result<Option<Provenance>, String> {
        let kept = self.read().provenance.get(key).cloned();
        let (read, said) = match kept {
            Some(kept) => kept,
            None => {
                let mut said = Vec::new();
                let read = super::provenance::parse(chain, &mut |l, t| said.push((l, t.to_string())));
                self.read().provenance.insert(*key, (read.clone(), said.clone()));
                (read, said)
            }
        };
        for (l, t) in &said {
            log(*l, t);
        }
        read
    }

    /// The signatures of the chain `key` names for `platform` (parse_signatures), read on
    /// a thread of the stack its documents may need, and kept once the trusted root is in
    /// hand: a failure to load that is tried again, as buildx tries it.
    pub fn signatures(
        &self,
        key: &[u8; 32],
        chain: &AttestationChain,
        platform: &shards_sigstore::platforms::Platform,
    ) -> Result<Option<Vec<Json>>, String> {
        let mut h = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
        h.update(key);
        h.update(format!("{platform:?}").as_bytes());
        let mut at = [0u8; 32];
        at.copy_from_slice(h.finish().as_ref());
        if let Some(kept) = self.read().signatures.get(&at) {
            return kept.clone();
        }
        let read = crate::build::attest::on_json_stack(|| parse_signatures(chain, platform, self))?;
        if self.root.get().is_some() {
            self.read().signatures.insert(at, read.clone());
        }
        read
    }

    /// The verifier's state directory made (getVerifier), as buildx makes it before
    /// each use until one succeeds.
    pub fn verifier(&self) -> Result<(), String> {
        if self.root.get().is_some() {
            return Ok(());
        }
        let dir = shards_ipc::home()?.join("policy").join("tuf");
        std::fs::create_dir_all(&dir).map_err(|e| {
            let why = match e.kind() {
                std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
                std::io::ErrorKind::NotADirectory => "not a directory".to_string(),
                std::io::ErrorKind::ReadOnlyFilesystem => "read-only file system".to_string(),
                _ => e.to_string(),
            };
            format!(
                "failed to create policy verifier config dir: mkdir {}: {why}",
                dir.display()
            )
        })
    }

    pub fn root(&self) -> Result<&TrustedRoot, String> {
        if let Some(r) = self.root.get() {
            return Ok(r);
        }
        let loaded = load().map_err(|(stage, e)| match stage {
            Stage::Provider => format!("loading trust provider: {e}"),
            Stage::Root => format!("getting trusted root: {e}"),
        })?;
        Ok(self.root.get_or_init(|| loaded))
    }
}

fn load() -> Result<TrustedRoot, (Stage, String)> {
    let home = shards_ipc::home().map_err(|e| (Stage::Provider, e))?;
    let cache = home.join("policy").join("tuf");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or((0, 0), |d| {
            (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos())
        });
    let (bytes, _status) = shards_tuf::client::trusted_root(&cache, &Web, now);
    let bytes = bytes.map_err(|(stage, e)| (stage, e.to_string()))?;
    shards_sigstore::trusted_root::parse(&bytes).map_err(|e| (Stage::Root, e.0))
}

/// buildx's acProvider: the chain's signature manifests as the attestation manifest's
/// referrers (their artifact type read from the manifest, cosign's where none), and its
/// blobs.
struct ChainProvider<'a> {
    chain: &'a AttestationChain,
}

impl Provider for ChainProvider<'_> {
    fn referrers(&self, digest: &str, _: &[&str], _: &[(&str, &str)]) -> Result<Vec<Descriptor>, String> {
        if digest != self.chain.attestation_manifest {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for d in &self.chain.signature_manifests {
            let Some((desc, data)) = self.chain.blobs.get(d) else {
                continue;
            };
            let m =
                sig::parse_manifest(data).map_err(|e| format!("unmarshal signature manifest {d}: {e}"))?;
            let mut desc = Descriptor {
                media_type: desc.media_type.clone(),
                digest: desc.digest.clone(),
                size: desc.size,
                ..Descriptor::default()
            };
            desc.artifact_type = if m.artifact_type.is_empty() {
                sig::ARTIFACT_COSIGN_SIGNATURE.to_string()
            } else {
                m.artifact_type
            };
            out.push(desc);
        }
        Ok(out)
    }

    fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String> {
        self.chain
            .blobs
            .get(&desc.digest)
            .map(|(_, data)| data.clone())
            .ok_or_else(|| "not found".to_string())
    }
}

/// parseSignatures: the image's verified signature, if the chain has one; none where it
/// has no signature manifest.
pub fn parse_signatures(
    chain: &AttestationChain,
    platform: &shards_sigstore::platforms::Platform,
    trust: &Trust,
) -> Result<Option<Vec<Json>>, String> {
    if chain.root.is_empty() || chain.attestation_manifest.is_empty() || chain.signature_manifests.is_empty()
    {
        return Ok(None);
    }
    for d in std::iter::once(&chain.root)
        .chain(std::iter::once(&chain.attestation_manifest))
        .chain(chain.signature_manifests.iter())
    {
        shards_image::reference::Digest::parse(d).map_err(|e| e.to_string())?;
    }
    let p = ChainProvider { chain };
    let Some((root_desc, _)) = chain.blobs.get(&chain.root) else {
        return Err(format!("root blob {} not found", chain.root));
    };
    let desc = Descriptor {
        media_type: root_desc.media_type.clone(),
        digest: root_desc.digest.clone(),
        size: root_desc.size,
        ..Descriptor::default()
    };
    if desc.media_type != sig::MEDIA_INDEX {
        return Ok(None);
    }
    let sc = sig::resolve_signature_chain(&p, &desc, platform)
        .map_err(|e| format!("resolving signature chain for image {}: {e}", desc.digest))?;
    let (Some(att), Some(_)) = (&sc.attestation_manifest, &sc.signature_manifest) else {
        return Ok(None);
    };
    if att.digest != chain.attestation_manifest {
        return Err(format!(
            "attestation manifest digest mismatch: expected {}, got {}",
            chain.attestation_manifest, att.digest
        ));
    }
    trust
        .verifier()
        .map_err(|e| format!("getting policy verifier: {e}"))?;
    let si = sig::verify_image(&p, &desc, platform, &|| trust.root(), local_offset)
        .map_err(|e| format!("verifying image signatures: {}", e.0))?;
    Ok(Some(vec![attestation_signature(&si)]))
}

/// AttestationSignature's JSON (toAttestationSignature), each field omitted where empty
/// but the signer's issuer and name.
pub fn attestation_signature(si: &SignatureInfo) -> Json {
    let mut out: Vec<(String, Json)> = Vec::new();
    let put = |k: &str, v: &str, out: &mut Vec<(String, Json)>| {
        if !v.is_empty() {
            out.push((k.into(), Json::Str(v.into())));
        }
    };
    put("kind", si.kind.input_name(), &mut out);
    put("type", si.signature_type.input_name(), &mut out);
    if !si.timestamps.is_empty() {
        out.push((
            "timestamps".into(),
            Json::Arr(
                si.timestamps
                    .iter()
                    .map(|t| {
                        Json::Obj(vec![
                            ("type".into(), Json::Str(t.kind.into())),
                            ("uri".into(), Json::Str(t.uri.clone())),
                            ("timestamp".into(), Json::Str(t.time.rfc3339_nano())),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    put("dockerReference", &si.docker_reference, &mut out);
    if si.is_dhi {
        out.push(("isDHI".into(), Json::Bool(true)));
    }
    if let Some(s) = &si.signer {
        let e = &s.extensions;
        let mut signer = vec![
            (
                "certificateIssuer".to_string(),
                Json::Str(s.certificate_issuer.clone()),
            ),
            (
                "subjectAlternativeName".to_string(),
                Json::Str(s.subject_alternative_name.clone()),
            ),
        ];
        for (k, v) in [
            ("issuer", &e.issuer),
            ("buildSignerURI", &e.build_signer_uri),
            ("buildSignerDigest", &e.build_signer_digest),
            ("runnerEnvironment", &e.runner_environment),
            ("sourceRepositoryURI", &e.source_repository_uri),
            ("sourceRepositoryDigest", &e.source_repository_digest),
            ("sourceRepositoryRef", &e.source_repository_ref),
            ("sourceRepositoryIdentifier", &e.source_repository_identifier),
            ("sourceRepositoryOwnerURI", &e.source_repository_owner_uri),
            (
                "sourceRepositoryOwnerIdentifier",
                &e.source_repository_owner_identifier,
            ),
            ("buildConfigURI", &e.build_config_uri),
            ("buildConfigDigest", &e.build_config_digest),
            ("buildTrigger", &e.build_trigger),
            ("runInvocationURI", &e.run_invocation_uri),
            (
                "sourceRepositoryVisibilityAtSigning",
                &e.source_repository_visibility_at_signing,
            ),
        ] {
            put(k, v, &mut signer);
        }
        out.push(("signer".into(), Json::Obj(signer)));
    }
    Json::Obj(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The GitHub CLI v2.102.0's linux/arm64 archive's attestation, as GitHub serves it
    /// behind its `bundle_url`, verified as buildx v0.37.1 verified it in shards-dind
    /// (`github_attestation(input.http, "cli/cli")`, printed): field for field.
    #[test]
    fn a_github_release_attestation_verifies_as_buildx_verified_it() {
        let bundle = super::super::snappy::decode(include_bytes!(
            "../testdata/gh-2.102.0-linux-arm64.attestation.json.sn"
        ))
        .unwrap();
        assert_eq!(
            bundle,
            include_bytes!("../testdata/gh-2.102.0-linux-arm64.sigstore.json")
        );
        let root = shards_sigstore::trusted_root::parse(include_bytes!(
            "../../../../tuf/roots/sigstore/targets/trusted_root.json"
        ))
        .unwrap();
        let si = shards_sigstore::helpers::verify_artifact(
            "sha256:7862c86c72f43df3a2d93ddde6f473285b4e2af61b494849846827e513ef6484",
            &bundle,
            &|| Ok(&root),
            shards_sigstore::time::utc,
            false,
        )
        .unwrap();
        let got: serde_json::Value = serde_json::from_str(&attestation_signature(&si).indented()).unwrap();
        let want: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/gh-2.102.0-linux-arm64.buildx.json")).unwrap();
        assert_eq!(got, want);
        // Another artifact's digest is not this bundle's subject.
        let other = "sha256:e5cc9fe3bbff5cbc91230981f7860e06076110730a2db997082652199042a1f2";
        assert!(
            shards_sigstore::helpers::verify_artifact(
                other,
                &bundle,
                &|| Ok(&root),
                shards_sigstore::time::utc,
                false
            )
            .is_err()
        );
    }

    /// moby/buildkit v0.28.1's attestation chain for linux/arm64, as Docker Hub served it
    /// (crates/sigstore/testdata/real): its index, attestation manifest, signature
    /// manifest and Sigstore bundle, and its SLSA v1 provenance
    /// (testdata/policy/real).
    pub(crate) fn real_chain() -> AttestationChain {
        let desc = |media_type: &str, digest: &str, data: &[u8], annotations: &[(&str, &str)]| Descriptor {
            media_type: media_type.into(),
            digest: digest.into(),
            size: i64::try_from(data.len()).unwrap(),
            annotations: annotations
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..Descriptor::default()
        };
        let manifest = "application/vnd.oci.image.manifest.v1+json";
        // Each blob's media type, digest, bytes and annotations.
        type Blob = (
            &'static str,
            &'static str,
            &'static [u8],
            &'static [(&'static str, &'static str)],
        );
        let blobs: [Blob; 5] = [
            (
                sig::MEDIA_INDEX,
                "sha256:a82d1ab899cda51aade6fe818d71e4b58c4079e047a0cf29dbb93b2b0465ea69",
                include_bytes!("../../../../sigstore/testdata/real/buildkit-v0.28.1.index.json"),
                &[],
            ),
            (
                manifest,
                "sha256:8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898",
                include_bytes!("../../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.attestation.json"),
                &[],
            ),
            (
                manifest,
                "sha256:64584b03b7c9aff3c8b10a44df9ba7eeb76888382e61f7ffd5ac83d42ff27aac",
                include_bytes!("../../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.sigmanifest.json"),
                &[],
            ),
            (
                "application/vnd.dev.sigstore.bundle.v0.3+json",
                "sha256:3e7b5c6a1e00b8778fc1c881593220acf37fc953a9ffbfbf316cd5858671cdb2",
                include_bytes!("../../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.bundle.json"),
                &[],
            ),
            (
                "application/vnd.in-toto+json",
                "sha256:14c95411788ad54aa780bf35951a7d941ccc0592dc4478e9d399e29462e8c380",
                include_bytes!("../../../testdata/policy/real/buildkit-v0.28.1-arm64.provenance.json"),
                &[("in-toto.io/predicate-type", sig::SLSA_V1)],
            ),
        ];
        AttestationChain {
            root: blobs[0].1.into(),
            attestation_manifest: blobs[1].1.into(),
            signature_manifests: vec![blobs[2].1.into()],
            blobs: blobs
                .iter()
                .map(|(m, d, data, a)| ((*d).to_string(), (desc(m, d, data, a), data.to_vec())))
                .collect(),
        }
    }

    /// The trust provider with the trusted root this binary carries, as a build's holds
    /// it once loaded.
    pub(crate) fn carried_trust() -> Trust {
        let trust = Trust::default();
        let root = shards_sigstore::trusted_root::parse(include_bytes!(
            "../../../../tuf/roots/sigstore/targets/trusted_root.json"
        ))
        .unwrap();
        let _ = trust.root.set(root);
        trust
    }

    #[test]
    fn the_real_chain_verifies_and_reads_as_buildx_reads_it() {
        let chain = real_chain();
        let trust = carried_trust();
        let arm64 = shards_sigstore::platforms::Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
            ..Default::default()
        };
        let sigs = parse_signatures(&chain, &arm64, &trust).unwrap().unwrap();
        assert_eq!(sigs.len(), 1);
        let p = super::super::provenance::parse(&chain, &mut |_, _| {})
            .unwrap()
            .unwrap();
        assert_eq!(p.predicate_type, sig::SLSA_V1);
    }

    fn platform(architecture: &str) -> shards_sigstore::platforms::Platform {
        shards_sigstore::platforms::Platform {
            os: "linux".into(),
            architecture: architecture.into(),
            ..Default::default()
        }
    }

    /// A chain is read once a build: asked again, its provenance comes back as kept, what
    /// reading it said said again, and its signatures as kept; a chain a byte apart, or
    /// another platform, is read anew.
    #[test]
    fn a_chain_is_read_once_a_build() {
        let chain = real_chain();
        let trust = carried_trust();
        let key = chain_key(&chain);
        let mut said = Vec::new();
        let first = trust
            .provenance(&key, &chain, &mut |l, t| said.push((l, t.to_string())))
            .unwrap()
            .unwrap();
        assert_eq!(first.predicate_type, sig::SLSA_V1);
        let signed = trust
            .signatures(&key, &chain, &platform("arm64"))
            .unwrap()
            .unwrap();
        assert_eq!(signed.len(), 1);
        assert_eq!(trust.read().provenance.len(), 1);
        assert_eq!(trust.read().signatures.len(), 1);
        // What is kept is what comes back: a planted reading shows it.
        let planted = super::super::provenance::Provenance {
            predicate_type: "planted".into(),
            ..Default::default()
        };
        trust.read().provenance.insert(
            key,
            (Ok(Some(planted)), vec![(LogLevel::Debug, "said once".into())]),
        );
        let planted = Ok(Some(vec![Json::Str("planted".into())]));
        for v in trust.read().signatures.values_mut() {
            v.clone_from(&planted);
        }
        let mut again = Vec::new();
        let kept = trust
            .provenance(&key, &chain, &mut |l, t| again.push((l, t.to_string())))
            .unwrap()
            .unwrap();
        assert_eq!(kept.predicate_type, "planted");
        assert_eq!(again, [(LogLevel::Debug, "said once".to_string())]);
        assert_eq!(trust.signatures(&key, &chain, &platform("arm64")), planted);
        // Another platform: its own reading (the chain holds no amd64 signature).
        assert_eq!(trust.signatures(&key, &chain, &platform("amd64")), Ok(None));
        assert_eq!(trust.read().signatures.len(), 2);
        // A chain a byte apart: its own key, and its own reading.
        let mut other = chain.clone();
        let (_, data) = other
            .blobs
            .get_mut("sha256:14c95411788ad54aa780bf35951a7d941ccc0592dc4478e9d399e29462e8c380")
            .unwrap();
        *data = String::from_utf8(data.clone())
            .unwrap()
            .replace("slsa-definitions.md", "slsa-definitionz.md")
            .into_bytes();
        let other_key = chain_key(&other);
        assert_ne!(other_key, key);
        let read = trust
            .provenance(&other_key, &other, &mut |_, _| {})
            .unwrap()
            .unwrap();
        assert!(
            read.build_type.ends_with("slsa-definitionz.md"),
            "{}",
            read.build_type
        );
    }

    /// A chain's signatures are read on a stack of their own: a root index as deep as Go's
    /// scanner allows, verified from a thread of a Windows process's 1 MiB main thread.
    #[test]
    fn the_deepest_chain_is_verified_on_its_stack() {
        let index = crate::build::attest::tests::deep("manifests", 10_000);
        let digest = format!(
            "sha256:{}",
            aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &index)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut chain = real_chain();
        let desc = Descriptor {
            media_type: sig::MEDIA_INDEX.into(),
            digest: digest.clone(),
            size: i64::try_from(index.len()).unwrap(),
            ..Descriptor::default()
        };
        chain.blobs.insert(digest.clone(), (desc, index));
        chain.root = digest;
        let trust = carried_trust();
        let key = chain_key(&chain);
        let refused = |e: String| {
            assert!(
                e.ends_with("unmarshaling image index: json: cannot unmarshal array into Go struct field Index.manifests of type v1.Descriptor"),
                "{e}"
            );
        };
        let read =
            crate::build::attest::on_stack(1 << 20, || trust.signatures(&key, &chain, &platform("arm64")))
                .unwrap();
        refused(read.unwrap_err());
        // Verified on a thread of the size the stack is measured with.
        let read = crate::build::attest::on_stack(crate::build::attest::tests::probe_stack(), || {
            parse_signatures(&chain, &platform("arm64"), &trust)
        })
        .unwrap();
        refused(read.unwrap_err());
    }

    /// Before the trusted root is in hand, what is read is not kept: a failure to load it
    /// is tried again, as buildx tries it.
    #[test]
    fn readings_without_the_root_are_not_kept() {
        let mut chain = real_chain();
        chain.signature_manifests.clear();
        let trust = Trust::default();
        let key = chain_key(&chain);
        assert_eq!(trust.signatures(&key, &chain, &platform("arm64")), Ok(None));
        assert!(trust.read().signatures.is_empty());
    }

    /// M127: what each part of a build's policy check costs, offline, on this host:
    /// VerifyImage of moby/buildkit's chain, reading its provenance, the input made of
    /// both, compiling a policy, the thread an evaluation runs on, whole checks, and a
    /// provenance as large as an attacker makes it, its peak memory with it
    /// (docs/research/measurements/policy-path/run.sh).
    #[test]
    #[ignore = "a measurement: docs/research/measurements/policy-path/run.sh"]
    fn policy_path_costs() {
        use std::time::Instant;
        let rounds = |n: usize, f: &mut dyn FnMut()| -> Vec<f64> {
            (0..n)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect()
        };
        let show = |name: &str, mut us: Vec<f64>| {
            us.sort_by(f64::total_cmp);
            let at = |q: f64| us[((us.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "{name:<34} n={:<5} p50={:>10.1}us p90={:>10.1}us p99={:>10.1}us max={:>10.1}us",
                us.len(),
                at(0.5),
                at(0.9),
                at(0.99),
                us[us.len() - 1]
            );
        };
        let cmd = |c: &str, a: &[&str]| {
            std::process::Command::new(c)
                .args(a)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        println!(
            "host: {} {}\nos: {}\nrevision: {}\n",
            cmd("uname", &["-m"]),
            cmd("sysctl", &["-n", "machdep.cpu.brand_string"]),
            cmd("uname", &["-sr"]),
            cmd("git", &["rev-parse", "--short", "HEAD"])
        );
        let chain = real_chain();
        let trust = carried_trust();
        let arm64 = shards_sigstore::platforms::Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
            ..Default::default()
        };
        show(
            "verify image (parse_signatures)",
            rounds(200, &mut || {
                parse_signatures(&chain, &arm64, &trust).unwrap();
            }),
        );
        show(
            "read provenance (80 KiB)",
            rounds(200, &mut || {
                super::super::provenance::parse(&chain, &mut |_, _| {}).unwrap();
            }),
        );
        let source = super::super::Source::new("docker-image://docker.io/moby/buildkit:v0.28.1");
        let meta = super::super::Meta {
            image: Some(super::super::ImageMeta {
                digest: chain.root.clone(),
                config: Some(
                    br#"{"created":"2026-03-25T13:41:00Z","config":{"Env":["PATH=/bin"]}}"#.to_vec(),
                ),
                attestation_chain: Some(chain.clone()),
            }),
            ..super::super::Meta::default()
        };
        let platform = shards_dockerfile::platform::Platform::new("linux", "arm64");
        show(
            "input of an image with its chain",
            rounds(200, &mut || {
                super::super::input::of_source(&source, &meta, Some(&platform), Some(&trust), &mut |_, _| {})
                    .unwrap();
            }),
        );
        let policy = "package docker\n\ndefault allow := false\n\nallow if input.local\n\nallow if {\n\tinput.image.hasProvenance\n\tsome sig in input.image.signatures\n\tdocker_github_builder_signature(sig, \"moby/buildkit\")\n}\n\ndecision := {\"allow\": allow}\n";
        let policies = super::super::Policies::configure(super::super::Setup {
            default: super::super::Opt {
                files: vec![super::super::FileSpec {
                    filename: "Dockerfile.rego".into(),
                    optional: false,
                    data: Some(policy.as_bytes().to_vec()),
                }],
                ..super::super::Opt::default()
            },
            configs: &[],
            env: super::super::Env::default(),
            cwd: std::env::temp_dir(),
            default_platform: platform.clone(),
            debug: false,
            default_policy: false,
        })
        .unwrap()
        .unwrap();
        let _ = policies.trust.root.set(
            shards_sigstore::trusted_root::parse(include_bytes!(
                "../../../../tuf/roots/sigstore/targets/trusted_root.json"
            ))
            .unwrap(),
        );
        show(
            "compile the policy",
            rounds(200, &mut || {
                policies.list[0].compile().unwrap();
            }),
        );
        show(
            "an evaluation's thread (123 MiB)",
            rounds(500, &mut || {
                super::super::with_stack_serving(|_| 1u8, &|_| {}).unwrap();
            }),
        );
        // One run each way, compile included, on this thread: what the checks are made of.
        let (ask, _heard) = std::sync::mpsc::channel();
        let unknown = super::super::input::of_source(
            &source,
            &super::super::Meta::default(),
            Some(&platform),
            Some(&trust),
            &mut |_, _| {},
        )
        .unwrap();
        let known =
            super::super::input::of_source(&source, &meta, Some(&platform), Some(&trust), &mut |_, _| {})
                .unwrap();
        show(
            "run, partial (compile + eval)",
            rounds(200, &mut || {
                policies.list[0].run(&unknown, true, &ask, &trust);
            }),
        );
        show(
            "run, whole (compile + eval)",
            rounds(200, &mut || {
                policies.list[0].run(&known, false, &ask, &trust);
            }),
        );
        struct Quiet;
        impl super::super::Log for Quiet {
            fn line(&self, _: &str) {}
            fn fetch(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<u8>, String> {
                Err("no fetches here".into())
            }
        }
        struct Answers(super::super::Meta);
        impl super::super::Resolve for Answers {
            fn resolve(
                &self,
                _: &super::super::Source,
                _: &super::super::MetaRequest,
            ) -> Result<super::super::Meta, String> {
                Ok(super::super::Meta {
                    image: self.0.image.clone(),
                    ..super::super::Meta::default()
                })
            }
        }
        let answers = Answers(meta.clone());
        let checked = policies.evaluate(&source, Some(&platform), &answers, &Quiet);
        assert!(matches!(checked, Ok(None)), "{checked:?}");
        show(
            "a source checked (signed image)",
            rounds(100, &mut || {
                policies
                    .evaluate(&source, Some(&platform), &answers, &Quiet)
                    .unwrap();
            }),
        );
        // A provenance as an attacker makes it: valid, its bulk in a field Go skips.
        let mut peak_before = 0i64;
        for mib in [1usize, 4, 16, 64] {
            let mut stmt = String::from(
                r#"{"_type":"https://in-toto.io/Statement/v0.1","predicateType":"https://slsa.dev/provenance/v1","subject":[],"predicate":{"buildDefinition":{"buildType":"t","externalParameters":{"x":["#,
            );
            while stmt.len() < mib << 20 {
                stmt.push_str("0,");
            }
            stmt.push_str(r#"0]}}}}"#);
            let mut big = real_chain();
            big.blobs.retain(|_, (d, _)| d.annotations.is_empty());
            let digest = "sha256:5555555555555555555555555555555555555555555555555555555555555555";
            big.blobs.insert(
                digest.into(),
                (
                    Descriptor {
                        media_type: "application/vnd.in-toto+json".into(),
                        digest: digest.into(),
                        size: i64::try_from(stmt.len()).unwrap(),
                        annotations: [("in-toto.io/predicate-type".to_string(), sig::SLSA_V1.to_string())]
                            .into(),
                        ..Descriptor::default()
                    },
                    stmt.into_bytes(),
                ),
            );
            let t = Instant::now();
            let p = super::super::provenance::parse(&big, &mut |_, _| {}).unwrap();
            let took = t.elapsed();
            let peak = peak_rss().unwrap_or(0);
            println!(
                "hostile provenance {mib:>3} MiB: read in {:>8.1} ms, provenance {}, peak RSS {} MiB (+{} MiB)",
                took.as_secs_f64() * 1e3,
                if p.is_some() { "kept" } else { "none" },
                peak >> 20,
                (peak - peak_before).max(0) >> 20
            );
            peak_before = peak;
        }
    }

    /// This process's peak resident memory in bytes, as getrusage(2) has it (`ru_maxrss`,
    /// bytes on macOS, KiB elsewhere); none where there is no getrusage.
    fn peak_rss() -> Option<i64> {
        #[cfg(unix)]
        {
            // SAFETY: rusage holds only integers and timevals, for which zeros are values.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            // SAFETY: getrusage(2) writes the struct it is given, which outlives the call.
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
                return None;
            }
            let max: i64 = usage.ru_maxrss;
            Some(if cfg!(target_os = "macos") {
                max
            } else {
                max * 1024
            })
        }
        #[cfg(not(unix))]
        None
    }
}
