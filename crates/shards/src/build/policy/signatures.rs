//! An image's signatures in a policy's input, as buildx v0.37.1 makes them
//! (policy/signatures.go `parseSignatures`, types.go `AttestationSignature`): the
//! attestation chain BuildKit resolved, read back through buildx's acProvider, verified by
//! the policy helpers' VerifyImage against Sigstore's trusted root (D104), which buildx
//! keeps under `~/.docker/buildx/policy/tuf` and shards under its home's `policy/tuf`.

use std::sync::OnceLock;

use shards_sigstore::helpers::SignatureInfo;
use shards_sigstore::image::{self as sig, Descriptor, Provider};
use shards_sigstore::trusted_root::TrustedRoot;
use shards_tuf::client::Stage;
use shards_tuf::{Fetch, FetchError};

use super::AttestationChain;
use super::input::Json;

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
#[derive(Default)]
pub struct Trust {
    root: OnceLock<TrustedRoot>,
}

impl Trust {
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
}
