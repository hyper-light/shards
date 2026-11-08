//! OCI 1.1 referrers (distribution-spec v1.1.1, image-spec v1.1.1): manifests whose
//! `subject` names another. A registry with the referrers API lists them itself
//! (spec.md:600-665); one without keeps, under the referrers tag schema, an index that
//! the clients pushing and deleting them update (spec.md:497-511, 688-699, 717-737).
//! Measured against distribution v3.1.2, which has no API, and zot v2.1.22, which has
//! it, with cosign v3.1.3 as the client (PM M135).

use std::collections::BTreeMap;
use std::io::Read;

use serde::{Deserialize, Serialize};
use shards_image::oci::{Descriptor, MAX_MANIFEST, media};
use shards_image::reference::Digest;
use shards_image::store::{Held, Store};

use crate::registry::{ENCODINGS, Registry, decoded, not_fetched};
use crate::{Error, ErrorKind};

/// A referrer as a list names it (image-spec descriptor.md; spec.md:505-509): its
/// manifest's type, size and digest, its artifact type, and its manifest's annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Referrer {
    pub media_type: String,
    pub size: i64,
    pub digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_type: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// A list of referrers: an image index.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct List {
    schema_version: u32,
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    manifests: Option<Vec<Referrer>>,
}

/// What of a manifest its entry in a list is made of.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Written {
    #[serde(default)]
    artifact_type: Option<String>,
    config: Option<ConfigDescriptor>,
    #[serde(default)]
    annotations: Option<BTreeMap<String, String>>,
    subject: Option<SubjectDescriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigDescriptor {
    #[serde(default)]
    media_type: String,
}

#[derive(Deserialize)]
struct SubjectDescriptor {
    digest: String,
}

/// The tag the referrers tag schema lists `subject`'s referrers under (spec.md:722-731):
/// `<alg>-<ref>`, the reference cut to 64 characters.
pub fn schema_tag(subject: &str) -> Result<String, Error> {
    let d = Digest::parse(subject).map_err(|e| Error::new(format!("{subject}: {e}")))?;
    let hex: String = d.hex().chars().take(64).collect();
    Ok(format!("{}-{hex}", d.algorithm().name()))
}

/// The manifest `desc` describes, its bytes `bytes`, as a list names it (spec.md:505-509):
/// its `artifactType`, else its config's type, and its annotations; and its subject's
/// digest, which it must have.
pub fn entry(desc: &Descriptor, bytes: &[u8]) -> Result<(Referrer, String), Error> {
    let w: Written =
        serde_json::from_slice(bytes).map_err(|e| Error::new(format!("{}: {e}", desc.digest)))?;
    let subject = w.subject.ok_or_else(|| {
        Error::new(format!(
            "{}: a manifest with no subject refers to nothing",
            desc.digest
        ))
    })?;
    let artifact_type = match w.artifact_type.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => w.config.map(|c| c.media_type).unwrap_or_default(),
    };
    Ok((
        Referrer {
            media_type: desc.media_type.clone(),
            size: desc.size,
            digest: desc.digest.clone(),
            artifact_type,
            annotations: w.annotations.unwrap_or_default(),
        },
        subject.digest,
    ))
}

/// The referrers of `subject` the registry lists: every page of the API's
/// (spec.md:600-621, `Link` to the next), or, where it has none (a 404), the index the
/// tag schema names, none where that is not one (spec.md:663-665); those of
/// `artifact_types` alone, where any are given, filtered here where the registry did not
/// say it filtered them. All the pages together are at most a manifest's size.
pub fn list(registry: &Registry, subject: &str, artifact_types: &[&str]) -> Result<Vec<Referrer>, Error> {
    let mut found = match api(registry, subject, artifact_types)? {
        Some(found) => found,
        None => match registry.manifest_at(&schema_tag(subject)?)? {
            Some((kind, bytes)) => parse_list(&bytes, &kind).unwrap_or_default(),
            None => Vec::new(),
        },
    };
    if !artifact_types.is_empty() {
        found.retain(|r| artifact_types.contains(&r.artifact_type.as_str()));
    }
    Ok(found)
}

/// An index's referrers, if it is one: of the index's type, as served or as written.
fn parse_list(bytes: &[u8], served_as: &str) -> Option<Vec<Referrer>> {
    let list: List = serde_json::from_slice(bytes).ok()?;
    let kind = served_as.split(';').next().unwrap_or_default().trim();
    let index =
        list.media_type == media::OCI_INDEX || (list.media_type.is_empty() && kind == media::OCI_INDEX);
    (list.schema_version == 2 && index).then(|| list.manifests.unwrap_or_default())
}

/// The referrers API's pages for `subject`, all of them; none where it answers 404.
fn api(registry: &Registry, subject: &str, artifact_types: &[&str]) -> Result<Option<Vec<Referrer>>, Error> {
    let mut url = registry.base().join(&format!("referrers/{subject}"))?;
    for t in artifact_types {
        url = url.with_query_pair("artifactType", t)?;
    }
    let mut found = Vec::new();
    let mut read: u64 = 0;
    let mut first = true;
    loop {
        let accept = [("Accept", media::OCI_INDEX), ("Accept-Encoding", ENCODINGS)];
        let (mut response, _) = registry.request("GET", &url, &accept)?;
        match response.status {
            200..=299 => {}
            404 if first => return Ok(None),
            _ => return Err(not_fetched(response, &url)),
        }
        first = false;
        let next = match response.header("link").and_then(next_link) {
            Some(link) => Some(response.url().join(link)?),
            None => None,
        };
        let kind = response.header("content-type").unwrap_or_default().to_string();
        let encoding = response
            .header("content-encoding")
            .unwrap_or_default()
            .to_string();
        let left = MAX_MANIFEST.saturating_sub(read);
        let mut body = decoded(&mut response, &encoding)?;
        let mut bytes = Vec::new();
        body.by_ref()
            .take(left.saturating_add(1))
            .read_to_end(&mut bytes)?;
        read = read.saturating_add(bytes.len() as u64);
        if read > MAX_MANIFEST {
            return Err(Error::new(format!(
                "the referrers of {subject}: more than {MAX_MANIFEST} bytes of lists"
            )));
        }
        let page = parse_list(&bytes, &kind)
            .ok_or_else(|| Error::new(format!("the referrers of {subject}: not an image index")))?;
        found.extend(page);
        match next {
            Some(link) => url = link,
            None => return Ok(Some(found)),
        }
    }
}

/// The target of a `Link` header's `rel="next"` (RFC 8288 §3): `<URI>; rel="next"`.
fn next_link(header: &str) -> Option<&str> {
    header.split(',').find_map(|link| {
        let (target, params) = link.trim().split_once(';')?;
        let next = params.split(';').any(|p| {
            let p = p.trim();
            p == "rel=\"next\"" || p == "rel=next"
        });
        next.then(|| target.trim().strip_prefix('<')?.strip_suffix('>'))
            .flatten()
    })
}

/// Pushes the referrer `desc` describes from `store`: what it names first, then it by its
/// digest; then, unless the registry answered that it took the subject (`OCI-Subject`,
/// spec.md:499) or lists it by the referrers API, the subject's list under the tag schema
/// with it (spec.md:501-511, 717-720). Returns how the registry lists it.
pub fn push(registry: &Registry, store: &Store, desc: &Descriptor) -> Result<Listed, Error> {
    let bytes = match store.held(desc, MAX_MANIFEST)? {
        Held::Whole(b) => b,
        Held::Invalid(why) => return Err(Error::new(why)),
        Held::Missing | Held::Changed(_) => {
            return Err(Error::of(
                ErrorKind::Missing,
                format!("content digest {}: not found", desc.digest),
            ));
        }
    };
    let (entry, subject) = entry(desc, &bytes)?;
    let answered = crate::push::push_manifest_answered(registry, store, desc)?;
    if answered.as_deref() == Some(subject.as_str()) {
        return Ok(Listed::ByApi);
    }
    // Put before, or answered without the header: the API asked whether it lists them.
    if api(registry, &subject, &[])?.is_some() {
        return Ok(Listed::ByApi);
    }
    update_schema(registry, &subject, |list| {
        // Duplicates are not made (spec.md:504).
        if list.iter().any(|r| r.digest == entry.digest) {
            return false;
        }
        list.push(entry.clone());
        true
    })?;
    Ok(Listed::ByTagSchema)
}

/// How a registry lists a referrer pushed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    /// By the referrers API: the registry keeps the list.
    ByApi,
    /// In the index the referrers tag schema names, which the push updated.
    ByTagSchema,
}

/// Deletes the referrer `referrer` of `subject`, then, where the registry has no referrers
/// API, its entry in the subject's list under the tag schema (spec.md:695-699).
pub fn delete(registry: &Registry, subject: &str, referrer: &str) -> Result<(), Error> {
    registry.delete_manifest(referrer)?;
    if api(registry, subject, &[])?.is_some() {
        return Ok(());
    }
    update_schema(registry, subject, |list| {
        let before = list.len();
        list.retain(|r| r.digest != referrer);
        list.len() != before
    })
}

/// The list the tag schema names for `subject`, changed by `change` and put back if it
/// says it changed it: an empty one where the tag names none (spec.md:503); a failure,
/// with nothing put, where it names something else than an index (spec.md:502).
fn update_schema(
    registry: &Registry,
    subject: &str,
    change: impl FnOnce(&mut Vec<Referrer>) -> bool,
) -> Result<(), Error> {
    let tag = schema_tag(subject)?;
    let mut list = match registry.manifest_at(&tag)? {
        Some((kind, bytes)) => parse_list(&bytes, &kind).ok_or_else(|| {
            Error::new(format!(
                "{tag}: the referrers tag schema names something else than an image index"
            ))
        })?,
        None => Vec::new(),
    };
    if !change(&mut list) {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&List {
        schema_version: 2,
        media_type: media::OCI_INDEX.to_string(),
        manifests: Some(list),
    })
    .map_err(|e| Error::new(e.to_string()))?;
    registry.put_manifest(&tag, media::OCI_INDEX, &bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_schema_names_a_subject_by_its_digest() {
        let hex = "da41a93758ed80c4185f3e8ae4bd3630ff53abd33813412d8c73d15ec231b61c";
        assert_eq!(
            schema_tag(&format!("sha256:{hex}")).unwrap(),
            format!("sha256-{hex}")
        );
        // A longer reference is cut to 64 characters (spec.md:727).
        let long = "a".repeat(128);
        assert_eq!(
            schema_tag(&format!("sha512:{long}")).unwrap(),
            format!("sha512-{}", "a".repeat(64))
        );
        assert!(schema_tag("nope").is_err());
    }

    #[test]
    fn an_entry_takes_the_artifact_type_else_the_configs_and_the_annotations() {
        let d = |bytes: &[u8]| Descriptor {
            media_type: media::OCI_MANIFEST.into(),
            digest: "sha256:aa".into(),
            size: i64::try_from(bytes.len()).unwrap(),
            platform: None,
            annotations: BTreeMap::new(),
        };
        let typed = br#"{"schemaVersion":2,"artifactType":"application/x.sig","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:44","size":2},"layers":[],"annotations":{"a":"b"},"subject":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:bb","size":1}}"#;
        let (r, subject) = entry(&d(typed), typed).unwrap();
        assert_eq!(
            (r.artifact_type.as_str(), subject.as_str()),
            ("application/x.sig", "sha256:bb")
        );
        assert_eq!(r.annotations.get("a").map(String::as_str), Some("b"));
        let untyped = br#"{"schemaVersion":2,"config":{"mediaType":"application/x.cfg","digest":"sha256:44","size":2},"layers":[],"subject":{"digest":"sha256:bb"}}"#;
        assert_eq!(
            entry(&d(untyped), untyped).unwrap().0.artifact_type,
            "application/x.cfg"
        );
        let none = br#"{"schemaVersion":2,"config":{"mediaType":"application/x.cfg","digest":"sha256:44","size":2},"layers":[]}"#;
        assert!(
            entry(&d(none), none)
                .unwrap_err()
                .to_string()
                .contains("no subject")
        );
    }

    #[test]
    fn a_link_names_the_next_page() {
        assert_eq!(
            next_link(r#"</v2/r/referrers/sha256:aa?n=1&last=x>; rel="next""#),
            Some("/v2/r/referrers/sha256:aa?n=1&last=x")
        );
        assert_eq!(next_link(r#"<a>; rel="prev", <b>; rel="next""#), Some("b"));
        assert_eq!(next_link(r#"<a>; rel="prev""#), None);
    }

    #[test]
    fn only_an_index_is_a_list() {
        let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"m","size":1,"digest":"sha256:aa","artifactType":"t"}]}"#;
        assert_eq!(parse_list(index, "").unwrap().len(), 1);
        let untyped = br#"{"schemaVersion":2,"manifests":null}"#;
        assert_eq!(parse_list(untyped, media::OCI_INDEX).unwrap().len(), 0);
        assert!(parse_list(untyped, media::OCI_MANIFEST).is_none());
        let manifest =
            br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{}}"#;
        assert!(parse_list(manifest, "").is_none());
    }
}
