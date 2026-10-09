//! An image's attestation chain as BuildKit v0.28.1 resolves it for a build policy
//! (source/containerimage/source.go, ResolveImageMetadata with AttestationChain): the
//! signature chain policy-helpers finds from the image's index over the registry's
//! referrers, and the blobs read on the way, with the attestations of the predicate types
//! asked for (addAttestationBlobs).

use std::collections::BTreeMap;

use shards_image::oci;
use shards_image::store::Store;
use shards_registry::registry::Registry;
use shards_sigstore::image::{self as sig, Descriptor, Provider};

use super::policy::AttestationChain;

/// The registry as policy-helpers' ReferrersProvider: referrers by the Referrers API or
/// its tag schema, documents and blobs fetched into the store and read from it.
struct Remote<'a> {
    registry: &'a Registry,
    store: &'a Store,
    limits: &'a shards_image::store::Limits,
}

fn to_oci(d: &Descriptor) -> oci::Descriptor {
    oci::Descriptor {
        media_type: d.media_type.clone(),
        digest: d.digest.clone(),
        size: d.size,
        platform: None,
        annotations: d.annotations.clone(),
    }
}

/// Whether a descriptor names a manifest or an index, which the registry serves at
/// `manifests/`.
fn is_document(media_type: &str) -> bool {
    matches!(
        media_type,
        oci::media::OCI_INDEX
            | oci::media::OCI_MANIFEST
            | oci::media::DOCKER_LIST
            | oci::media::DOCKER_MANIFEST
    )
}

impl Provider for Remote<'_> {
    fn referrers(
        &self,
        digest: &str,
        artifact_types: &[&str],
        filters: &[(&str, &str)],
    ) -> Result<Vec<Descriptor>, String> {
        let bytes = self
            .registry
            .referrers(digest, artifact_types, filters)
            .map_err(|e| e.to_string())?;
        let index = sig::parse_index(&bytes).map_err(|e| format!("failed to decode referrers index: {e}"))?;
        if artifact_types.is_empty() {
            return Ok(index.manifests);
        }
        Ok(index
            .manifests
            .into_iter()
            .filter(|d| artifact_types.contains(&d.artifact_type.as_str()))
            .collect())
    }

    fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String> {
        let d = to_oci(desc);
        if is_document(&desc.media_type) {
            return self
                .registry
                .fetch_document(self.store, &d)
                .map_err(|e| e.to_string());
        }
        self.registry
            .fetch_blob(self.store, &d, self.limits, &|_| {})
            .map_err(|e| e.to_string())?;
        self.store
            .content(&d, self.limits.bytes)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{}: not found", desc.digest))
    }
}

/// The chain of the image `top` resolved to, for `platform`; none where `top` is not an
/// OCI index.
pub fn chain(
    registry: &Registry,
    store: &Store,
    limits: &shards_image::store::Limits,
    top: &oci::Descriptor,
    platform: &shards_sigstore::platforms::Platform,
    resolve_attestations: &[String],
) -> Result<Option<AttestationChain>, String> {
    if top.media_type != sig::MEDIA_INDEX {
        return Ok(None);
    }
    let p = Remote {
        registry,
        store,
        limits,
    };
    let desc = Descriptor {
        media_type: top.media_type.clone(),
        digest: top.digest.clone(),
        size: top.size,
        annotations: top.annotations.clone(),
        ..Descriptor::default()
    };
    let sc = sig::resolve_signature_chain(&p, &desc, platform)?;
    let mut ac = AttestationChain {
        root: desc.digest.clone(),
        ..AttestationChain::default()
    };
    let mut descs = vec![desc];
    if let Some(att) = &sc.attestation_manifest {
        ac.attestation_manifest = att.digest.clone();
        descs.push(att.clone());
    }
    if let Some(s) = &sc.signature_manifest {
        ac.signature_manifests = vec![s.digest.clone()];
        descs.push(s.clone());
        let bytes = sig::read_blob(&p, s)?;
        let m =
            sig::parse_manifest(&bytes).map_err(|e| format!("unmarshaling manifest {}: {e}", s.digest))?;
        descs.extend(m.layers);
    }
    let mut blobs = BTreeMap::new();
    for d in descs {
        let data = sig::read_blob(&p, &d)?;
        blobs.insert(d.digest.clone(), (d, data));
    }
    ac.blobs = blobs;
    if !resolve_attestations.is_empty() && !ac.attestation_manifest.is_empty() {
        add_attestation_blobs(&p, &mut ac, resolve_attestations)?;
    }
    Ok(Some(ac))
}

/// addAttestationBlobs: the attestation manifest's layers of the predicate types asked.
fn add_attestation_blobs(
    p: &dyn Provider,
    ac: &mut AttestationChain,
    types: &[String],
) -> Result<(), String> {
    let Some((_, data)) = ac.blobs.get(&ac.attestation_manifest) else {
        return Ok(());
    };
    if data.is_empty() || types.iter().all(String::is_empty) {
        return Ok(());
    }
    let m = sig::parse_manifest(data).map_err(|e| {
        format!(
            "unmarshaling attestation manifest {}: {e}",
            ac.attestation_manifest
        )
    })?;
    for layer in m.layers {
        let t = layer
            .annotations
            .get("in-toto.io/predicate-type")
            .cloned()
            .unwrap_or_default();
        if !types.contains(&t) || ac.blobs.contains_key(&layer.digest) {
            continue;
        }
        let data = sig::read_blob(p, &layer)?;
        ac.blobs.insert(layer.digest.clone(), (layer, data));
    }
    Ok(())
}
