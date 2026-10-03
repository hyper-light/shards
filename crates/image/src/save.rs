//! Images as `docker save` writes them (dockerd 29.3.1, its containerd store, measured
//! 2026-10-02): an OCI image layout in a tar, with Docker's `manifest.json` beside it.
//!
//! - `index.json` names each image asked for, in the order asked: what its name resolved
//!   to, annotated with its name and tag and the repository it was pulled from.
//! - `manifest.json` lists each image once, for the Docker tools that read it: our
//!   platform's config and layers, and the names asked for it (`null` for none).
//! - `blobs/<algorithm>/<hex>` holds what is here of each image: its index, and each
//!   manifest of it here with its config and layers, attestations included.
//!
//! The tar is Go's archive/tar's, as containerd's exporter writes it: USTAR headers,
//! every time 0 and owner 0, directories 0755, `index.json` and `manifest.json` 0644,
//! the rest 0444, records in their names' order, and two zero blocks to end.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use crate::oci::{self, Document};
use crate::reference::{Digest, Reference};
use crate::store::{Held, Image, Store};
use crate::{Error, bad};

/// An image asked for, and the name it was asked by, if a name: a reference's record
/// name (`docker.io/library/alpine:3.22`).
#[derive(Debug)]
pub struct Asked<'a> {
    pub image: &'a Image,
    pub name: Option<String>,
}

/// What a record holds.
enum Content {
    Dir,
    Bytes(Vec<u8>),
    File(PathBuf, u64),
}

/// Writes the images `asked` names to `out`.
pub fn save<W: Write>(store: &Store, asked: &[Asked<'_>], out: W) -> Result<(), Error> {
    let mut records: BTreeMap<String, (u32, Content)> = BTreeMap::new();
    let mut index_entries = Vec::with_capacity(asked.len());
    // Each image once, with the names asked for it, in order of first asking.
    let mut listed: Vec<(&Image, Vec<String>)> = Vec::new();
    for a in asked {
        index_entries.push(index_entry(a)?);
        let familiar = a
            .name
            .as_deref()
            .and_then(|n| Reference::parse_normalized(n).ok())
            .map(|r| r.familiar());
        match listed.iter_mut().find(|(i, _)| i.id == a.image.id) {
            Some((_, names)) => names.extend(familiar),
            None => listed.push((a.image, familiar.into_iter().collect())),
        }
    }
    let mut manifests = Vec::with_capacity(listed.len());
    for (image, names) in &listed {
        for digest in present(store, image)? {
            let path = store.blob_path(&digest);
            let len = std::fs::metadata(&path)?.len();
            records.insert(blob_name(&digest), (0o444, Content::File(path, len)));
            records.insert(
                format!("blobs/{}/", digest.algorithm().name()),
                (0o755, Content::Dir),
            );
        }
        manifests.push(docker_manifest(store, image, names)?);
    }
    records.insert("blobs/".into(), (0o755, Content::Dir));
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{}]}}"#,
        oci::media::OCI_INDEX,
        index_entries.join(",")
    );
    records.insert("index.json".into(), (0o644, Content::Bytes(index.into_bytes())));
    records.insert(
        "manifest.json".into(),
        (
            0o644,
            Content::Bytes(format!("[{}]", manifests.join(",")).into_bytes()),
        ),
    );
    records.insert(
        "oci-layout".into(),
        (
            0o444,
            Content::Bytes(br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec()),
        ),
    );
    let mut tar = crate::tar::writer::Writer::new(out);
    for (name, (mode, content)) in &records {
        let (typeflag, size) = match content {
            Content::Dir => (crate::tar::writer::DIR, 0),
            Content::Bytes(b) => (crate::tar::writer::REG, b.len() as u64),
            Content::File(_, len) => (crate::tar::writer::REG, *len),
        };
        tar.header(&crate::tar::writer::Header {
            name: name.clone().into_bytes(),
            typeflag,
            mode: i64::from(*mode),
            size: i64::try_from(size).map_err(|e| Error(e.to_string()))?,
            ..Default::default()
        })?;
        match content {
            Content::Dir => {}
            Content::Bytes(b) => tar.write(b)?,
            Content::File(path, _) => {
                tar.copy(File::open(path)?)?;
            }
        }
    }
    tar.finish()?.flush()?;
    Ok(())
}

fn blob_name(d: &Digest) -> String {
    format!("blobs/{}/{}", d.algorithm().name(), d.hex())
}

/// What is here of `image`, walked from what its references resolved to (containerd's
/// walk of present children): its index, and each manifest here, with its config and
/// layers here.
fn present(store: &Store, image: &Image) -> Result<Vec<Digest>, Error> {
    let mut found = Vec::new();
    let manifests = if image.target.digest == image.manifest.to_string() {
        vec![image.target.clone()]
    } else {
        match store.held(&image.target, oci::MAX_MANIFEST)? {
            Held::Invalid(why) => return bad(why),
            Held::Whole(bytes) => {
                found.push(image.id.clone());
                match serde_json::from_slice::<oci::Index>(&bytes) {
                    Ok(index) => index.manifests,
                    Err(e) => return bad(format!("{}: {e}", image.id)),
                }
            }
            _ => return bad(format!("{}: its index is not here", image.id)),
        }
    };
    for desc in manifests {
        let Held::Whole(bytes) = store.held(&desc, oci::MAX_MANIFEST)? else {
            continue;
        };
        found.push(desc.digest()?);
        if let Document::Manifest(m) = oci::parse_document(&bytes, &desc.media_type)? {
            for part in std::iter::once(&m.config).chain(&m.layers) {
                let d = part.digest()?;
                if store.has(&d) {
                    found.push(d);
                }
            }
        }
    }
    Ok(found)
}

/// `index.json`'s entry for `asked`: what its name resolved to, annotated with the name
/// and its tag, and the repository it was pulled from (containerd's
/// `containerd.io/distribution.source.<registry>`), keys sorted as Go writes a map.
fn index_entry(asked: &Asked<'_>) -> Result<String, Error> {
    let t = &asked.image.target;
    let mut annotations: BTreeMap<String, String> = BTreeMap::new();
    // Each registry's repositories, comma-separated, as containerd labels them.
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in &asked.image.sources {
        if let Some((host, path)) = s.split_once('/') {
            sources
                .entry(host.to_string())
                .or_default()
                .push(path.to_string());
        }
    }
    for (host, paths) in sources {
        annotations.insert(
            format!("containerd.io/distribution.source.{host}"),
            paths.join(","),
        );
    }
    if let Some(name) = &asked.name {
        annotations.insert("io.containerd.image.name".into(), name.clone());
        if let Ok(r) = Reference::parse_normalized(name)
            && let Some(tag) = r.tag
        {
            annotations.insert("org.opencontainers.image.ref.name".into(), tag);
        }
    }
    let json = |s: &str| serde_json::to_string(s).map_err(|e| Error(e.to_string()));
    let mut entry = format!(
        r#"{{"mediaType":{},"digest":{},"size":{}"#,
        json(&t.media_type)?,
        json(&t.digest)?,
        t.size
    );
    if !annotations.is_empty() {
        let pairs: Result<Vec<String>, Error> = annotations
            .iter()
            .map(|(k, v)| Ok(format!("{}:{}", json(k)?, json(v)?)))
            .collect();
        entry.push_str(&format!(r#","annotations":{{{}}}"#, pairs?.join(",")));
    }
    entry.push('}');
    Ok(entry)
}

/// `manifest.json`'s entry for `image`: our platform's config and layers, and `names`.
fn docker_manifest(store: &Store, image: &Image, names: &[String]) -> Result<String, Error> {
    let desc = oci::Descriptor {
        media_type: oci::media::OCI_MANIFEST.into(),
        digest: image.manifest.to_string(),
        size: i64::try_from(std::fs::metadata(store.blob_path(&image.manifest))?.len())
            .map_err(|e| Error(e.to_string()))?,
        platform: None,
        annotations: BTreeMap::new(),
    };
    let bytes = match store.held(&desc, oci::MAX_MANIFEST)? {
        Held::Whole(bytes) => bytes,
        Held::Invalid(why) => return bad(why),
        Held::Missing | Held::Changed(_) => return bad(format!("{}: its manifest is not here", image.id)),
    };
    let manifest = match serde_json::from_slice::<oci::Manifest>(&bytes) {
        Ok(m) => m,
        Err(e) => return bad(format!("{}: {e}", image.manifest)),
    };
    let json = |s: &str| serde_json::to_string(s).map_err(|e| Error(e.to_string()));
    let tags = if names.is_empty() {
        "null".to_string()
    } else {
        let tags: Result<Vec<String>, Error> = names.iter().map(|n| json(n)).collect();
        format!("[{}]", tags?.join(","))
    };
    let layers: Result<Vec<String>, Error> = manifest
        .layers
        .iter()
        .map(|l| json(&blob_name(&l.digest()?)))
        .collect();
    Ok(format!(
        r#"{{"Config":{},"RepoTags":{tags},"Layers":[{}]}}"#,
        json(&blob_name(&manifest.config.digest()?))?,
        layers?.join(",")
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {

    /// A header as Go's archive/tar writes it, by the first one of an archive `docker
    /// save` wrote (dockerd 29.3.1, 2026-10-02): `blobs/`, a directory, mode 0755.
    #[test]
    fn headers_are_written_as_go_writes_them() {
        let mut tar = crate::tar::writer::Writer::new(Vec::new());
        tar.header(&crate::tar::writer::Header {
            name: b"blobs/".to_vec(),
            typeflag: crate::tar::writer::DIR,
            mode: 0o755,
            ..Default::default()
        })
        .unwrap();
        let h = tar.finish().unwrap();
        let mut want = [0u8; 512];
        let mut put = |at: usize, bytes: &[u8]| want[at..at + bytes.len()].copy_from_slice(bytes);
        put(0, b"blobs/");
        put(100, b"0000755\0");
        put(108, b"0000000\0");
        put(116, b"0000000\0");
        put(124, b"00000000000\0");
        put(136, b"00000000000\0");
        put(148, b"010306\0 ");
        put(156, b"5");
        put(257, b"ustar\x0000");
        put(329, b"0000000\0");
        put(337, b"0000000\0");
        assert_eq!(&h[..512], &want[..]);
    }
}
