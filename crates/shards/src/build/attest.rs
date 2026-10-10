//! An image's attestation chain as BuildKit v0.28.1 resolves it for a build policy
//! (source/containerimage/source.go, ResolveImageMetadata with AttestationChain): the
//! signature chain policy-helpers finds from the image's index over the registry's
//! referrers, and the blobs read on the way, with the attestations of the predicate types
//! asked for (addAttestationBlobs).
//!
//! Its blobs come to at most [`CHAIN_MAX`] bytes: each blob's size is checked against
//! what is left of that before it is fetched, so no image makes a build fetch or hold more
//! of it.

use std::cell::Cell;
use std::collections::BTreeMap;

use shards_image::oci;
use shards_image::reference::{Algorithm, Digest};
use shards_image::store::Store;
use shards_registry::registry::Registry;
use shards_sigstore::image::{self as sig, Descriptor, Provider};

use super::policy::AttestationChain;

/// The most bytes a chain's blobs come to: what buildx's policy session takes in the one
/// CheckPolicy request that carries them. Its gRPC server keeps grpc-go's default receive
/// limit (`defaultServerMaxReceiveMessageSize`, 4 MiB; buildkit session.NewSession sets
/// none), so a larger chain fails Docker's policy check. shards refuses it before
/// fetching it: nothing Docker accepts is refused, and no image can make a build hold more.
pub const CHAIN_MAX: u64 = 4 << 20;

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
        let max = blob_max(desc, self.limits.bytes)?;
        self.registry
            .fetch_blob(self.store, &d, self.limits, &|_| {})
            .map_err(|e| e.to_string())?;
        self.store
            .content(&d, max)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{}: not found", desc.digest))
    }
}

/// The most of a blob that is read for a chain, `limit` or the chain's whole: a blob
/// larger than that is refused before it is fetched, whatever the chain reads it for.
fn blob_max(desc: &Descriptor, limit: u64) -> Result<u64, String> {
    let max = limit.min(CHAIN_MAX);
    match u64::try_from(desc.size) {
        Ok(size) if size <= max => Ok(max),
        _ => Err(too_large(&desc.digest, desc.size, max)),
    }
}

/// A provider read as content.ReadBlob reads one: what a descriptor embeds first.
struct Embedding<'a>(&'a dyn Provider);

impl Provider for Embedding<'_> {
    fn referrers(
        &self,
        digest: &str,
        artifact_types: &[&str],
        filters: &[(&str, &str)],
    ) -> Result<Vec<Descriptor>, String> {
        self.0.referrers(digest, artifact_types, filters)
    }

    fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String> {
        match embedded(desc)? {
            Some(data) => Ok(data),
            None => self.0.read(desc),
        }
    }
}

/// content.ReadBlob's useDescriptorData: the blob a descriptor carries in its `data`,
/// where that is as long as the descriptor says and of its digest. It is never fetched.
fn embedded(desc: &Descriptor) -> Result<Option<Vec<u8>>, String> {
    if i64::try_from(desc.data.len()).ok() != Some(desc.size) {
        return Ok(None);
    }
    let digest = Digest::parse(&desc.digest).map_err(|e| format!("invalid descriptor digest: {e}"))?;
    let algorithm = match digest.algorithm() {
        Algorithm::Sha256 => &aws_lc_rs::digest::SHA256,
        Algorithm::Sha384 => &aws_lc_rs::digest::SHA384,
        Algorithm::Sha512 => &aws_lc_rs::digest::SHA512,
    };
    let of_data = Digest::from_hash(
        digest.algorithm(),
        aws_lc_rs::digest::digest(algorithm, &desc.data).as_ref(),
    );
    Ok((of_data == digest).then(|| desc.data.clone()))
}

fn too_large(digest: &str, size: i64, max: u64) -> String {
    format!(
        "attestation chain blob {digest} of {size} bytes: an image's attestation chain may come to {max} bytes, as much as buildx's policy session takes"
    )
}

/// The stack a chain's documents are read on. The policy helpers' JSON (shards_sigstore's
/// gojson, shards_tuf's) is read by recursive descent, and a document as deep as Go's
/// scanner allows (10000 levels) took 3616 KiB to read where a build reads it on
/// aarch64-apple-darwin, and 3136 KiB to parse and drop on x86_64-apple-darwin (M127):
/// more than a spawned thread's 2 MiB or a Windows process's 1 MiB main thread, whose
/// overflow ends the process. The most measured, in whole MiB;
/// `the_deepest_chain_is_read_on_its_stack` and `the_deepest_chain_is_verified_on_its_stack`
/// hold every target to it.
pub(super) const JSON_STACK: usize = 4 << 20;

/// `f` on a thread of [`JSON_STACK`], this one waiting for it.
pub(super) fn on_json_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, String> {
    on_stack(JSON_STACK, f)
}

pub(super) fn on_stack<T: Send>(size: usize, f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("chain".into())
            .stack_size(size)
            .spawn_scoped(s, f)
            .map_err(|e| format!("starting the attestation chain's reading: {e}"))?
            .join()
            .map_err(|_| "the attestation chain's reading failed".to_string())
    })
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
    let desc = Descriptor {
        media_type: top.media_type.clone(),
        digest: top.digest.clone(),
        size: top.size,
        annotations: top.annotations.clone(),
        ..Descriptor::default()
    };
    let remote = Remote {
        registry,
        store,
        limits,
    };
    on_json_stack(|| chain_from(&remote, &desc, platform, resolve_attestations))?
}

/// What is left of a chain's [`CHAIN_MAX`] bytes, each blob taken from it before it is
/// read.
struct Budget(Cell<u64>);

impl Budget {
    fn take(&self, d: &Descriptor) -> Result<(), String> {
        let left = self.0.get();
        match u64::try_from(d.size) {
            Ok(size) if size <= left => {
                self.0.set(left - size);
                Ok(())
            }
            _ => Err(too_large(&d.digest, d.size, CHAIN_MAX)),
        }
    }
}

/// [`chain`] over any provider.
fn chain_from(
    p: &dyn Provider,
    desc: &Descriptor,
    platform: &shards_sigstore::platforms::Platform,
    resolve_attestations: &[String],
) -> Result<Option<AttestationChain>, String> {
    if desc.media_type != sig::MEDIA_INDEX {
        return Ok(None);
    }
    let p = &Embedding(p);
    let sc = sig::resolve_signature_chain(p, desc, platform)?;
    let mut ac = AttestationChain {
        root: desc.digest.clone(),
        ..AttestationChain::default()
    };
    let mut descs = vec![desc.clone()];
    if let Some(att) = &sc.attestation_manifest {
        ac.attestation_manifest = att.digest.clone();
        descs.push(att.clone());
    }
    if let Some(s) = &sc.signature_manifest {
        ac.signature_manifests = vec![s.digest.clone()];
        descs.push(s.clone());
        let bytes = sig::read_blob(p, s)?;
        let m =
            sig::parse_manifest(&bytes).map_err(|e| format!("unmarshaling manifest {}: {e}", s.digest))?;
        descs.extend(m.layers);
    }
    let budget = Budget(Cell::new(CHAIN_MAX));
    let mut blobs: BTreeMap<String, (Descriptor, Vec<u8>)> = BTreeMap::new();
    for d in descs {
        // BuildKit reads a digest named again from what it fetched the first time, and
        // keeps the later descriptor: the same bytes, neither fetched nor counted again.
        if let Some(blob) = blobs.get_mut(&d.digest) {
            blob.0 = d;
            continue;
        }
        budget.take(&d)?;
        let data = sig::read_blob(p, &d)?;
        blobs.insert(d.digest.clone(), (d, data));
    }
    ac.blobs = blobs;
    if !resolve_attestations.is_empty() && !ac.attestation_manifest.is_empty() {
        add_attestation_blobs(p, &mut ac, resolve_attestations, &budget)?;
    }
    Ok(Some(ac))
}

/// addAttestationBlobs: the attestation manifest's layers of the predicate types asked.
fn add_attestation_blobs(
    p: &dyn Provider,
    ac: &mut AttestationChain,
    types: &[String],
    budget: &Budget,
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
        budget.take(&layer)?;
        let data = sig::read_blob(p, &layer)?;
        ac.blobs.insert(layer.digest.clone(), (layer, data));
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use std::cell::RefCell;

    use base64::Engine as _;
    use serde_json::json;

    use super::*;

    const INDEX: &str = "sha256:a82d1ab899cda51aade6fe818d71e4b58c4079e047a0cf29dbb93b2b0465ea69";
    const ATTESTATION: &str = "sha256:8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898";
    const SIGNATURE: &str = "sha256:64584b03b7c9aff3c8b10a44df9ba7eeb76888382e61f7ffd5ac83d42ff27aac";
    const BUNDLE: &str = "sha256:3e7b5c6a1e00b8778fc1c881593220acf37fc953a9ffbfbf316cd5858671cdb2";
    const PROVENANCE: &str = "sha256:14c95411788ad54aa780bf35951a7d941ccc0592dc4478e9d399e29462e8c380";
    const BUNDLE_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";

    const INDEX_DATA: &[u8] = include_bytes!("../../../sigstore/testdata/real/buildkit-v0.28.1.index.json");
    const ATTESTATION_DATA: &[u8] =
        include_bytes!("../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.attestation.json");
    const BUNDLE_DATA: &[u8] =
        include_bytes!("../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.bundle.json");

    /// moby/buildkit v0.28.1's blobs as Docker Hub served them, the referrers of its arm64
    /// attestation manifest, and each read asked of it.
    struct Hub {
        blobs: Vec<(String, Vec<u8>)>,
        referrers: Vec<u8>,
        read: RefCell<Vec<String>>,
    }

    impl Provider for Hub {
        fn referrers(&self, digest: &str, _: &[&str], _: &[(&str, &str)]) -> Result<Vec<Descriptor>, String> {
            if digest != ATTESTATION {
                return Ok(Vec::new());
            }
            Ok(sig::parse_index(&self.referrers)?.manifests)
        }

        fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String> {
            self.read.borrow_mut().push(desc.digest.clone());
            self.blobs
                .iter()
                .find(|(d, _)| *d == desc.digest)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| format!("{}: not found", desc.digest))
        }
    }

    fn hub() -> Hub {
        let blobs: [(&str, &[u8]); 5] = [
            (INDEX, INDEX_DATA),
            (ATTESTATION, ATTESTATION_DATA),
            (
                SIGNATURE,
                include_bytes!("../../../sigstore/testdata/real/buildkit-v0.28.1-arm64.sigmanifest.json"),
            ),
            (BUNDLE, BUNDLE_DATA),
            (
                PROVENANCE,
                include_bytes!("../../testdata/policy/real/buildkit-v0.28.1-arm64.provenance.json"),
            ),
        ];
        Hub {
            blobs: blobs
                .iter()
                .map(|(d, b)| ((*d).to_string(), b.to_vec()))
                .collect(),
            referrers: include_bytes!("../../../sigstore/testdata/real/buildkit-v0.28.1.referrers.json")
                .to_vec(),
            read: RefCell::new(Vec::new()),
        }
    }

    fn sha256(b: &[u8]) -> String {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b);
        Digest::from_hash(Algorithm::Sha256, d.as_ref()).to_string()
    }

    /// The hub with the signature manifest's layers replaced by `layers`, its referrers
    /// naming the edited manifest by its digest and size; and that manifest's length.
    fn signed_with(layers: serde_json::Value) -> (Hub, usize) {
        let original = hub()
            .blobs
            .iter()
            .find(|(d, _)| d == SIGNATURE)
            .unwrap()
            .1
            .clone();
        let mut m: serde_json::Value = serde_json::from_slice(&original).unwrap();
        m["layers"] = layers;
        signed_by(serde_json::to_vec(&m).unwrap())
    }

    /// The hub with `edited` for its signature manifest.
    fn signed_by(edited: Vec<u8>) -> (Hub, usize) {
        let mut h = hub();
        let original = h.blobs.iter().find(|(d, _)| d == SIGNATURE).unwrap().1.clone();
        let (digest, len) = (sha256(&edited), edited.len());
        h.referrers = String::from_utf8(h.referrers)
            .unwrap()
            .replace(SIGNATURE, &digest)
            .replace(
                &format!("\"size\":{}", original.len()),
                &format!("\"size\":{len}"),
            )
            .into_bytes();
        h.blobs.push((digest, edited));
        (h, len)
    }

    /// The hub with one signature layer of `size` bytes, and what the rest of the chain
    /// comes to.
    fn filled(size: usize) -> (Hub, usize) {
        let layer = vec![7u8; size];
        let (mut h, signature) =
            signed_with(json!([{ "mediaType": BUNDLE_TYPE, "digest": sha256(&layer), "size": size }]));
        h.blobs.push((sha256(&layer), layer));
        (h, INDEX_DATA.len() + ATTESTATION_DATA.len() + signature)
    }

    fn index() -> Descriptor {
        Descriptor {
            media_type: sig::MEDIA_INDEX.into(),
            digest: INDEX.into(),
            size: i64::try_from(INDEX_DATA.len()).unwrap(),
            ..Descriptor::default()
        }
    }

    fn arm64() -> shards_sigstore::platforms::Platform {
        shards_sigstore::platforms::Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
            ..Default::default()
        }
    }

    fn reads(h: &Hub, digest: &str) -> usize {
        h.read.borrow().iter().filter(|d| *d == digest).count()
    }

    fn slsa1() -> Vec<String> {
        vec![sig::SLSA_V1.to_string()]
    }

    /// The real chain: the blobs BuildKit sends, its provenance only when asked for.
    #[test]
    fn the_real_chain_is_read_as_buildkit_reads_it() {
        let ac = chain_from(&hub(), &index(), &arm64(), &slsa1()).unwrap().unwrap();
        assert_eq!(ac.root, INDEX);
        assert_eq!(ac.attestation_manifest, ATTESTATION);
        assert_eq!(ac.signature_manifests, [SIGNATURE]);
        let mut want = vec![INDEX, ATTESTATION, SIGNATURE, BUNDLE, PROVENANCE];
        want.sort_unstable();
        assert_eq!(ac.blobs.keys().collect::<Vec<_>>(), want);
        assert!(
            ac.blobs
                .iter()
                .all(|(d, (desc, data))| *d == desc.digest && sha256(data) == *d)
        );
        let ac = chain_from(&hub(), &index(), &arm64(), &[]).unwrap().unwrap();
        assert!(!ac.blobs.contains_key(PROVENANCE));
        // An image, not an index: no chain, nothing read.
        let h = hub();
        let manifest = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            ..index()
        };
        assert!(chain_from(&h, &manifest, &arm64(), &[]).unwrap().is_none());
        assert!(h.read.borrow().is_empty());
    }

    /// A signature manifest naming a layer larger than a chain may be: refused, the layer
    /// never fetched. Layers each smaller, together larger: refused at the first past the
    /// limit, nothing from it on fetched. A chain of exactly the limit is read whole.
    #[test]
    fn a_chain_past_its_limit_is_refused_before_it_is_fetched() {
        let big = sha256(b"big");
        let (h, _) = signed_with(json!([{ "mediaType": BUNDLE_TYPE, "digest": big, "size": CHAIN_MAX + 1 }]));
        assert_eq!(
            chain_from(&h, &index(), &arm64(), &[]).unwrap_err(),
            format!(
                "attestation chain blob {big} of {} bytes: an image's attestation chain may come to 4194304 bytes, as much as buildx's policy session takes",
                CHAIN_MAX + 1
            )
        );
        assert_eq!(reads(&h, &big), 0, "{:?}", h.read.borrow());

        let quarter = usize::try_from(CHAIN_MAX / 4).unwrap();
        let layers: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; quarter]).collect();
        let (mut h, _) = signed_with(
            layers
                .iter()
                .map(|b| json!({ "mediaType": BUNDLE_TYPE, "digest": sha256(b), "size": b.len() }))
                .collect(),
        );
        h.blobs.extend(layers.iter().map(|b| (sha256(b), b.clone())));
        let err = chain_from(&h, &index(), &arm64(), &[]).unwrap_err();
        assert!(
            err.starts_with(&format!("attestation chain blob {} ", sha256(&layers[3]))),
            "{err}"
        );
        assert_eq!(reads(&h, &sha256(&layers[2])), 1);
        assert_eq!(reads(&h, &sha256(&layers[3])), 0);
        assert_eq!(reads(&h, &sha256(&layers[4])), 0);

        // One layer that fills the chain to its last byte; then one byte more.
        let (_, rest) = filled(1_000_000);
        let size = usize::try_from(CHAIN_MAX).unwrap() - rest;
        let (h, same) = filled(size);
        assert_eq!(same, rest, "the signature manifest keeps its length");
        let ac = chain_from(&h, &index(), &arm64(), &[]).unwrap().unwrap();
        assert_eq!(
            ac.blobs.values().map(|(_, b)| b.len()).sum::<usize>(),
            usize::try_from(CHAIN_MAX).unwrap()
        );
        assert!(chain_from(&filled(size + 1).0, &index(), &arm64(), &[]).is_err());
    }

    /// The attestations asked for come out of what is left of the chain's limit: with all
    /// but a kilobyte of it taken, the 80 KiB provenance is refused, never fetched.
    #[test]
    fn attestations_asked_for_count_toward_the_limit() {
        let (_, rest) = filled(1_000_000);
        let (h, _) = filled(usize::try_from(CHAIN_MAX).unwrap() - rest - 1024);
        chain_from(&h, &index(), &arm64(), &[]).unwrap().unwrap();
        let err = chain_from(&h, &index(), &arm64(), &slsa1()).unwrap_err();
        assert!(
            err.starts_with(&format!("attestation chain blob {PROVENANCE} of 80376 bytes")),
            "{err}"
        );
        assert_eq!(reads(&h, PROVENANCE), 0);
    }

    /// A digest named twice is read once, counted once, and kept with the descriptor named
    /// last: as BuildKit keeps it, its second read served from what it buffered.
    #[test]
    fn a_digest_named_again_is_read_once_and_keeps_its_later_descriptor() {
        let bundle = json!({ "mediaType": BUNDLE_TYPE, "digest": BUNDLE, "size": BUNDLE_DATA.len() });
        let (once, _) = signed_with(json!([bundle]));
        chain_from(&once, &index(), &arm64(), &[]).unwrap().unwrap();
        let mut later = bundle.clone();
        later["annotations"] = json!({ "x": "later" });
        let (h, _) = signed_with(json!([
            bundle,
            later,
            { "mediaType": BUNDLE_TYPE, "digest": INDEX, "size": CHAIN_MAX },
        ]));
        let ac = chain_from(&h, &index(), &arm64(), &[]).unwrap().unwrap();
        assert_eq!(reads(&h, BUNDLE), reads(&once, BUNDLE));
        assert_eq!(reads(&h, INDEX), reads(&once, INDEX));
        let (desc, data) = &ac.blobs[BUNDLE];
        assert_eq!(desc.annotations.get("x").map(String::as_str), Some("later"));
        assert_eq!(data, BUNDLE_DATA);
        assert_eq!(ac.blobs[INDEX].0.media_type, BUNDLE_TYPE);
        assert_eq!(ac.blobs[INDEX].1, INDEX_DATA);
    }

    /// A layer whose descriptor carries its blob is read from there, never fetched, as
    /// containerd's content.ReadBlob reads it.
    #[test]
    fn a_blob_its_descriptor_carries_is_not_fetched() {
        let data = base64::engine::general_purpose::STANDARD.encode(BUNDLE_DATA);
        let (mut h, _) = signed_with(json!([
            { "mediaType": BUNDLE_TYPE, "digest": BUNDLE, "size": BUNDLE_DATA.len(), "data": data },
        ]));
        h.blobs.retain(|(d, _)| d != BUNDLE);
        let ac = chain_from(&h, &index(), &arm64(), &[]).unwrap().unwrap();
        assert_eq!(ac.blobs[BUNDLE].1, BUNDLE_DATA);
        assert_eq!(reads(&h, BUNDLE), 0);

        let d = Descriptor {
            digest: sha256(b"{}"),
            size: 2,
            data: b"{}".to_vec(),
            ..Descriptor::default()
        };
        assert_eq!(embedded(&d).unwrap().unwrap(), b"{}");
        // Not as long as its descriptor says, or not of its digest: fetched.
        assert_eq!(embedded(&Descriptor { size: 3, ..d.clone() }).unwrap(), None);
        assert_eq!(
            embedded(&Descriptor {
                digest: sha256(b"[]"),
                ..d.clone()
            })
            .unwrap(),
            None
        );
        // A digest go-digest refuses, where the data is as long as said.
        assert_eq!(
            embedded(&Descriptor {
                digest: "sha256:abc".into(),
                ..d.clone()
            })
            .unwrap_err(),
            "invalid descriptor digest: invalid checksum digest length"
        );
        // sha384 and sha512 too, and the empty blob, named with no data.
        for (algorithm, of) in [
            (Algorithm::Sha384, &aws_lc_rs::digest::SHA384),
            (Algorithm::Sha512, &aws_lc_rs::digest::SHA512),
        ] {
            let digest =
                Digest::from_hash(algorithm, aws_lc_rs::digest::digest(of, b"{}").as_ref()).to_string();
            assert_eq!(
                embedded(&Descriptor { digest, ..d.clone() }).unwrap().unwrap(),
                b"{}"
            );
        }
        let empty = Descriptor {
            digest: sha256(b""),
            ..Descriptor::default()
        };
        assert_eq!(embedded(&empty).unwrap(), Some(Vec::new()));
    }

    /// What the registry is asked for is bounded by the chain's limit and the pull's,
    /// whichever is smaller.
    #[test]
    fn a_blob_larger_than_a_chain_may_be_is_never_fetched() {
        let d = |size: i64| Descriptor {
            digest: sha256(b"x"),
            size,
            ..Descriptor::default()
        };
        assert_eq!(blob_max(&d(0), u64::MAX).unwrap(), CHAIN_MAX);
        assert_eq!(blob_max(&d(4 << 20), u64::MAX).unwrap(), CHAIN_MAX);
        assert!(blob_max(&d((4 << 20) + 1), u64::MAX).is_err());
        assert!(blob_max(&d(-1), u64::MAX).is_err());
        assert_eq!(blob_max(&d(10), 10).unwrap(), 10);
        assert!(blob_max(&d(11), 10).is_err());
    }

    /// A stack to read a chain on: [`JSON_STACK`], or `SHARDS_JSON_STACK_PROBE` KiB to
    /// measure with (docs/research/measurements/policy-path/stack.py).
    pub(in crate::build) fn probe_stack() -> usize {
        std::env::var("SHARDS_JSON_STACK_PROBE")
            .ok()
            .and_then(|k| k.parse::<usize>().ok())
            .map_or(JSON_STACK, |k| k << 10)
    }

    /// A document `levels` deep in all: an object holding `key`, its value arrays inside
    /// arrays.
    pub(in crate::build) fn deep(key: &str, levels: usize) -> Vec<u8> {
        let n = levels - 1;
        format!("{{\"{key}\":{}{}}}", "[".repeat(n), "]".repeat(n)).into_bytes()
    }

    /// The deepest documents a chain may hold, read where a build reads them, on a thread
    /// of [`JSON_STACK`]: an index and a signature manifest as deep as Go's scanner allows
    /// (10000 levels), each refused for its types once read, and one level deeper,
    /// refused by the scanner.
    #[test]
    fn the_deepest_chain_is_read_on_its_stack() {
        let (by_index, by_signature, deeper) = on_stack(probe_stack(), || {
            let read_index = |levels: usize| {
                let index = deep("manifests", levels);
                let desc = Descriptor {
                    media_type: sig::MEDIA_INDEX.into(),
                    digest: sha256(&index),
                    size: i64::try_from(index.len()).unwrap(),
                    ..Descriptor::default()
                };
                let mut h = hub();
                h.blobs.push((desc.digest.clone(), index));
                chain_from(&h, &desc, &arm64(), &[])
            };
            let (h, _) = signed_by(deep("layers", 10_000));
            (
                read_index(10_000),
                chain_from(&h, &index(), &arm64(), &[]),
                read_index(10_001),
            )
        })
        .unwrap();
        assert_eq!(
            by_index.unwrap_err(),
            "unmarshaling image index: json: cannot unmarshal array into Go struct field Index.manifests of type v1.Descriptor"
        );
        let e = by_signature.unwrap_err();
        assert!(
            e.ends_with(
                "json: cannot unmarshal array into Go struct field Manifest.layers of type v1.Descriptor"
            ),
            "{e}"
        );
        assert_eq!(
            deeper.unwrap_err(),
            "unmarshaling image index: invalid character '[' exceeded max depth"
        );
    }
}
