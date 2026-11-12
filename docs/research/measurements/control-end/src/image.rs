//! Arm g's image: the guest's program alone, as `docker save` writes an archive (an OCI
//! layout with Docker's `manifest.json` beside it, as Docker 25 and later write one), for
//! `shards load`. The E2E tests' test image is made the same way (crates/shards/tests/common).

use sha2::Digest as _;

/// The archive of image `name` (`NAME:TAG`) whose one layer holds `guest` as /control-end,
/// for a microVM of this host's architecture.
pub fn archive(guest: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let layer = tar(&[("control-end", 0o755, Some(guest)), ("tmp", 0o1777, None)])?;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => return Err(format!("no OCI platform for {other}")),
    };
    let config = format!(
        r#"{{"architecture":"{arch}","os":"linux","config":{{"Env":["PATH=/"],"Entrypoint":["/control-end"],"Cmd":["guest","idle"]}},"rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
        digest(&layer)
    )
    .into_bytes();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        digest(&config),
        config.len(),
        digest(&layer),
        layer.len()
    )
    .into_bytes();
    let tag = name.rsplit_once(':').map_or("latest", |(_, tag)| tag);
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"annotations":{{"io.containerd.image.name":"docker.io/library/{name}","org.opencontainers.image.ref.name":"{tag}"}}}}]}}"#,
        digest(&manifest),
        manifest.len()
    );
    let path = |b: &[u8]| format!("blobs/sha256/{}", digest(b).trim_start_matches("sha256:"));
    let docker = format!(
        r#"[{{"Config":"{}","RepoTags":["{name}"],"Layers":["{}"]}}]"#,
        path(&config),
        path(&layer)
    );
    let (manifest_at, config_at, layer_at) = (path(&manifest), path(&config), path(&layer));
    tar(&[
        ("blobs", 0o755, None),
        ("blobs/sha256", 0o755, None),
        (&config_at, 0o644, Some(&config)),
        (&layer_at, 0o644, Some(&layer)),
        (&manifest_at, 0o644, Some(&manifest)),
        ("index.json", 0o644, Some(index.as_bytes())),
        ("manifest.json", 0o644, Some(docker.as_bytes())),
        ("oci-layout", 0o644, Some(br#"{"imageLayoutVersion":"1.0.0"}"#)),
    ])
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::proto::hex(&sha2::Sha256::digest(bytes)))
}

/// A ustar archive (POSIX.1-1988) of `(path, mode, contents)`, owned by root; no contents is a
/// directory.
fn tar(entries: &[(&str, u32, Option<&[u8]>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (path, mode, contents) in entries {
        let name = match contents {
            None => format!("{path}/"),
            Some(_) => (*path).to_string(),
        };
        if name.len() > 100 {
            return Err(format!("{name}: longer than a ustar name"));
        }
        let size = contents.map_or(0, <[u8]>::len);
        let mut h = [0u8; 512];
        put(&mut h, 0, name.as_bytes())?;
        put(&mut h, 100, format!("{mode:07o}\0").as_bytes())?;
        put(&mut h, 108, b"0000000\0")?;
        put(&mut h, 116, b"0000000\0")?;
        put(&mut h, 124, format!("{size:011o}\0").as_bytes())?;
        put(&mut h, 136, b"14500000000\0")?;
        put(&mut h, 148, b"        ")?;
        put(&mut h, 156, if contents.is_none() { b"5" } else { b"0" })?;
        put(&mut h, 257, b"ustar\0")?;
        put(&mut h, 263, b"00")?;
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        put(&mut h, 148, format!("{sum:06o}\0 ").as_bytes())?;
        out.extend_from_slice(&h);
        if let Some(data) = contents {
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512).saturating_mul(512), 0);
        }
    }
    out.resize(out.len().saturating_add(1024), 0);
    Ok(out)
}

fn put(header: &mut [u8; 512], at: usize, bytes: &[u8]) -> Result<(), String> {
    header
        .get_mut(at..at.saturating_add(bytes.len()))
        .ok_or_else(|| format!("a ustar field at {at} of {} bytes", bytes.len()))?
        .copy_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header's checksum is the sum of its bytes with the checksum field as spaces
    /// (POSIX.1-1988 ustar), and its file is padded to whole 512-byte blocks.
    #[test]
    fn a_tar_entry_carries_its_checksum_and_padding() {
        let t = tar(&[("a", 0o644, Some(b"hello"))]).unwrap();
        assert_eq!(t.len(), 512 + 512 + 1024);
        let mut h: [u8; 512] = t[..512].try_into().unwrap();
        let said = std::str::from_utf8(&h[148..154]).unwrap().to_string();
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        assert_eq!(said, format!("{sum:06o}"));
        assert_eq!(&t[512..517], b"hello");
    }
}
