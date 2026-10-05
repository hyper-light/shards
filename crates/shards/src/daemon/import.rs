//! `shards import` (docker/cli image/import.go; moby docker-v29.8.1 router image_routes.go
//! postImagesCreate and daemon/containerd/image_import.go ImportImage): a tarball, from the
//! client or a URL, made a one-layer image: gzip and zstd kept as they come, bzip2 and xz
//! decompressed, and a plain tar kept as it is, where dockerd spends a gzip on it; its
//! config dockerd's (BuildFromConfig of `--change`, the platform, the comment).

use std::io::Read;

use shards_cmdline::flags::Parsed;
use shards_image::reference::Reference;

use super::Daemon;
use super::commands::{Asker, Reply};
use crate::containers::Disk;

const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// What the CLI and dockerd say before they import: the reference and the platform read.
fn read_args(parsed: &Parsed) -> Result<(Option<Reference>, shards_dockerfile::platform::Platform), String> {
    use shards_dockerfile::platform;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let host = platform::Platform::new("linux", arch);
    // platforms.Parse in the CLI, before the reference (import.go); none given,
    // platforms.DefaultSpec, whose variant is the CPU's, not normalized (`v8` on arm64).
    let wanted = match parsed.string("platform") {
        "" if arch == "arm64" => platform::Platform {
            variant: b"v8".to_vec(),
            ..host.clone()
        },
        "" => host.clone(),
        given => platform::normalize(
            &platform::parse(given.as_bytes(), &host)
                .map_err(|e| String::from_utf8_lossy(&e).into_owned())?,
        ),
    };
    // The client's ImageImport: a name, never a digest.
    let named = match parsed.args.get(1).filter(|r| !r.is_empty()) {
        None => None,
        Some(r) => {
            let parsed = Reference::parse_normalized(r).map_err(|e| e.to_string())?;
            if parsed.digest.is_some() {
                return Err("cannot import digest reference".into());
            }
            Some(parsed.tag_name_only())
        }
    };
    Ok((named, wanted))
}

impl<D: Disk> Daemon<D> {
    /// `shards import SOURCE [REPOSITORY[:TAG]]`: the image's ID, after `Downloading from
    /// URL` for a URL, as the CLI shows dockerd's stream on a pipe.
    pub(super) fn import(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        let Some(source) = parsed.args.first() else {
            return 1;
        };
        let (named, platform) = match read_args(parsed) {
            Ok(r) => r,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        // The store, made where no image was pulled yet.
        let root = self.home.join("images");
        let store = match std::fs::create_dir_all(&root)
            .map_err(|e| e.to_string())
            .and_then(|()| shards_image::store::Store::open(&root).map_err(|e| e.to_string()))
        {
            Ok(store) => store,
            Err(e) => return refuse(e),
        };
        // BuildFromConfig of an empty config, before anything is read.
        let config = match shards_dockerfile::commit::build_from_config(
            &serde_json::json!({}),
            parsed.many("change"),
            "linux",
        ) {
            Ok(c) => c,
            Err(e) => return refuse(e),
        };
        let url = source.starts_with("http://") || source.starts_with("https://");
        let comment = match parsed.string("message") {
            "" if url => format!("Imported from {source}"),
            "" => "Imported from -".to_string(),
            m => m.to_string(),
        };
        // Held while what it writes is not yet named: no collection takes it meanwhile.
        let _lease = match store.lease() {
            Ok(l) => l,
            Err(e) => return refuse(e.to_string()),
        };
        // What it reads: a URL fetched into the store's ingest, or what the client sent.
        let fetched;
        let mut input: Box<dyn Read> = if url {
            reply.out(&format!("Downloading from {source}"));
            let at = root
                .join("ingest")
                .join(format!("import-{}", crate::containers::now()));
            let limits = match crate::pull::limits() {
                Ok(l) => l,
                Err(e) => return refuse(e),
            };
            match crate::build::http::fetch_now(source, None, at.clone(), &limits) {
                Ok(d) => {
                    fetched = Some(d.path.clone());
                    match std::fs::File::open(&d.path) {
                        Ok(f) => Box::new(f),
                        Err(e) => return refuse(e.to_string()),
                    }
                }
                Err(e) => return refuse(e),
            }
        } else {
            fetched = None;
            match asker.files.first().map(|f| f.try_clone()) {
                Some(Ok(fd)) => Box::new(std::fs::File::from(fd)),
                Some(Err(e)) => return refuse(e.to_string()),
                None => return refuse("the client sent no archive".into()),
            }
        };
        // Kept once, as it came: what it is decides what is made of it.
        let raw = store.writer().map_err(|e| e.to_string()).and_then(|mut w| {
            std::io::copy(&mut input, &mut w).map_err(|e| e.to_string())?;
            w.commit().map_err(|e| e.to_string())
        });
        if let Some(path) = fetched {
            let _ = std::fs::remove_file(path);
        }
        let (raw, _) = match raw {
            Ok(r) => r,
            Err(e) => return refuse(e),
        };
        // A shards microVM, or a container image's archive: taken as `load` takes one,
        // each image made a microVM; a microVM is one already.
        if is_image_archive(&store.blob_path(&raw)) {
            let loaded = std::fs::File::open(store.blob_path(&raw))
                .map_err(|e| e.to_string())
                .and_then(|file| self.load_archive(&store, file));
            self.collect_soon();
            let loaded = match loaded {
                Ok(l) => l,
                Err(e) => return refuse(e),
            };
            let mut status = 0;
            let one = loaded.len() == 1;
            for image in &loaded {
                // The name given names it too, where the archive held one image.
                if let (Some(n), true) = (&named, one)
                    && let Err(e) = store.alias(&n.to_string(), &image.name)
                {
                    reply.err(&format!("Error response from daemon: {e}"));
                    status = 1;
                }
                if let Some(Err(e)) = &image.unpacked {
                    reply.err(&format!("Error response from daemon: {}: {e}", image.shown));
                    status = 1;
                }
                self.image_event(&image.digest, &image.digest, "import");
                reply.out(&image.digest);
            }
            return status;
        }
        // A container's files: an image of one layer, made a microVM at once, as a pull
        // makes one.
        let (id, desc) = match self.import_layer(&store, &raw, config, &platform, &comment, named.as_ref()) {
            Ok(made) => made,
            Err(e) => return refuse(e),
        };
        let shown = named.as_ref().map_or_else(|| id.clone(), Reference::familiar);
        let resolved = match desc.digest() {
            Ok(d) => d,
            Err(e) => return refuse(e.to_string()),
        };
        let made = shards_registry::pull::unpack(
            &store,
            &shown,
            &desc,
            resolved,
            &shards_image::platform::guest(),
            &shards_image::store::Limits::none(),
        );
        self.image_event(&id, &id, "import");
        reply.out(&id);
        match made {
            Ok(pulled) => {
                #[cfg(unix)]
                if let (Some(n), Some(disk)) = (&named, &pulled.rootfs) {
                    crate::pull::publish(&self.home, n, &pulled.id, &pulled.config, disk, &|k| {
                        shards_ipc::env_value(&asker.registry_env, k)
                    });
                }
                0
            }
            // Kept, as a pull keeps an image another platform's guests run.
            Err(e) => {
                reply.err(&format!("Error response from daemon: {shown}: {e}"));
                1
            }
        }
    }

    /// Stored blob `raw` as the image's one layer, and its config and manifest, recorded
    /// under `named` or as a dangling image: its ID (the manifest's digest, as dockerd's
    /// with containerd's store), and its manifest's descriptor.
    fn import_layer(
        &self,
        store: &shards_image::store::Store,
        raw: &shards_image::reference::Digest,
        config: serde_json::Value,
        platform: &shards_dockerfile::platform::Platform,
        comment: &str,
        named: Option<&Reference>,
    ) -> Result<(String, shards_image::oci::Descriptor), String> {
        use shards_build::archive::{Compression, decompressed, detect};
        let open = || std::fs::File::open(store.blob_path(raw)).map_err(|e| e.to_string());
        let mut head = vec![0u8; 512];
        let got = open()?.read(&mut head).map_err(|e| e.to_string())?;
        head.truncate(got);
        let kind = detect(&head);
        let (layer, size, media) = match kind {
            // Kept as they came, as dockerd keeps them; a plain tar too, where dockerd
            // spends a gzip on it.
            Compression::Gzip | Compression::Zstd | Compression::None => {
                let size = std::fs::metadata(store.blob_path(raw))
                    .map_err(|e| e.to_string())?
                    .len();
                let media = match kind {
                    Compression::Gzip => LAYER_GZIP,
                    Compression::Zstd => LAYER_ZSTD,
                    _ => LAYER_TAR,
                };
                (raw.clone(), size, media)
            }
            // Decompressed: neither is a layer's media type.
            Compression::Bzip2 | Compression::Xz => {
                let mut writer = store.writer().map_err(|e| e.to_string())?;
                let mut plain =
                    decompressed(std::io::BufReader::new(open()?), 1 << 16).map_err(|e| e.to_string())?;
                std::io::copy(&mut plain, &mut writer).map_err(|e| e.to_string())?;
                let (layer, size) = writer.commit().map_err(|e| e.to_string())?;
                (layer, size, LAYER_TAR)
            }
        };
        // Its diff ID: the digest of the tar itself.
        let diff_id = if media == LAYER_TAR {
            layer.clone()
        } else {
            let blob = std::fs::File::open(store.blob_path(&layer)).map_err(|e| e.to_string())?;
            let mut plain =
                decompressed(std::io::BufReader::new(blob), 1 << 16).map_err(|e| e.to_string())?;
            let mut hasher = sha2::Sha256::default();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = plain.read(&mut buf).map_err(|e| format!("the archive: {e}"))?;
                if n == 0 {
                    break;
                }
                sha2::Digest::update(&mut hasher, buf.get(..n).unwrap_or_default());
            }
            shards_image::reference::Digest::from_hash(
                shards_image::reference::Algorithm::Sha256,
                &sha2::Digest::finalize(hasher),
            )
        };
        let now = super::commands::rfc3339_nano(u64::try_from(crate::spec::now()).unwrap_or(0));
        let mut image = serde_json::Map::new();
        image.insert("created".into(), now.clone().into());
        image.insert(
            "architecture".into(),
            String::from_utf8_lossy(&platform.architecture)
                .into_owned()
                .into(),
        );
        image.insert(
            "os".into(),
            String::from_utf8_lossy(&platform.os).into_owned().into(),
        );
        if !platform.variant.is_empty() {
            image.insert(
                "variant".into(),
                String::from_utf8_lossy(&platform.variant).into_owned().into(),
            );
        }
        image.insert("config".into(), config);
        image.insert(
            "rootfs".into(),
            serde_json::json!({"type": "layers", "diff_ids": [diff_id.to_string()]}),
        );
        image.insert(
            "history".into(),
            serde_json::json!([{"created": now, "comment": comment}]),
        );
        let config_bytes =
            serde_json::to_vec(&serde_json::Value::Object(image)).map_err(|e| e.to_string())?;
        let config_digest = digest_of(&config_bytes);
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest.to_string(),
                "size": config_bytes.len(),
            },
            "layers": [{"mediaType": media, "digest": layer.to_string(), "size": size}],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        let manifest_digest = digest_of(&manifest_bytes);
        for (digest, bytes) in [
            (&config_digest, &config_bytes),
            (&manifest_digest, &manifest_bytes),
        ] {
            store
                .ingest(digest, bytes.len() as u64, &mut bytes.as_slice())
                .map_err(|e| e.to_string())?;
        }
        let desc = shards_image::oci::Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: manifest_digest.to_string(),
            size: i64::try_from(manifest_bytes.len()).map_err(|e| e.to_string())?,
            platform: None,
            annotations: Default::default(),
        };
        let tag = match named {
            Some(r) => r.to_string(),
            None => format!("{}{manifest_digest}", shards_image::store::DANGLING),
        };
        let contents = [layer, config_digest, manifest_digest.clone()];
        store
            .tag(&tag, &desc, &manifest_digest, &contents)
            .map_err(|e| e.to_string())?;
        Ok((manifest_digest.to_string(), desc))
    }
}

/// Whether the tarball at `path` (plain or compressed) is an image archive: an OCI image
/// layout (`oci-layout`, `index.json`), a shards microVM among them, or a `docker save`
/// archive (`manifest.json`), as containerd's importer recognizes one; not a container's
/// files.
fn is_image_archive(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut plain) = shards_build::archive::decompressed(std::io::BufReader::new(file), 1 << 16) else {
        return false;
    };
    let mut tar = shards_image::tar::Reader::new(&mut plain);
    while let Ok(Some(entry)) = tar.next_entry() {
        if matches!(
            entry.path.as_slice(),
            b"oci-layout" | b"index.json" | b"manifest.json"
        ) {
            return true;
        }
    }
    false
}

/// The SHA-256 digest of `bytes`.
fn digest_of(bytes: &[u8]) -> shards_image::reference::Digest {
    use sha2::Digest as _;
    shards_image::reference::Digest::from_hash(
        shards_image::reference::Algorithm::Sha256,
        &sha2::Sha256::digest(bytes),
    )
}
