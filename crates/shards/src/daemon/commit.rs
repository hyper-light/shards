//! `shards commit CONTAINER [REPOSITORY[:TAG]]` (moby daemon/commit.go,
//! CreateImageFromContainer; daemon/containerd/image_commit.go, CommitImage): a new
//! image of the container's image and the layer of what it changed (D37), its config the
//! container's, its history one step longer.

use std::io::{Read as _, Write as _};

use shards_cmdline::flags::Parsed;
use shards_image::reference::Reference;
use shards_ipc::Run;

use super::commands::{Asker, Reply};
use super::{Daemon, LAYER, lock};
use crate::containers::State as Life;

/// An OCI layer's media type, uncompressed, as `shards build` writes its layers.
const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

impl<D: crate::containers::Disk> Daemon<D> {
    pub(super) fn commit_image(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let _ = asker;
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        let Some(given) = parsed.args.first() else {
            return 1;
        };
        if parsed.changed("pause") && parsed.changed("no-pause") {
            reply.err("conflicting options: --no-pause and --pause cannot be used together");
            return 1;
        }
        // The name it is given, read as the CLI reads it before it asks (commit.go).
        let named = match parsed.args.get(1) {
            None => None,
            Some(r) => match Reference::parse_normalized(r) {
                Ok(r) if r.digest.is_some() => {
                    reply.err("refusing to create a tag with a digest reference");
                    return 1;
                }
                Ok(r) => Some(r.tag_name_only()),
                Err(e) => {
                    reply.err(&e.to_string());
                    return 1;
                }
            },
        };
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        if lock(&self.removing).contains(&id) {
            return refuse(format!("You cannot commit container {id} which is being removed"));
        }
        let Some(record) = lock(&self.containers).get(&id).cloned() else {
            return refuse(format!("No such container: {given}"));
        };
        let store = match self.store() {
            Ok(Some(store)) => store,
            Ok(None) => return refuse(format!("container {id}: its image is not here")),
            Err(e) => return refuse(e),
        };
        // Its image: the manifest and config it was made from.
        let base = record.image_id.as_deref().and_then(|want| {
            store
                .images()
                .ok()?
                .into_iter()
                .find(|i| i.id.to_string() == want)
        });
        let Some(base) = base else {
            return refuse(format!("container {id}: the image it was made from is gone"));
        };
        let base_manifest: serde_json::Value = match std::fs::read(store.blob_path(&base.manifest))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            Some(m) => m,
            None => return refuse(format!("container {id}: its image's manifest is not here")),
        };
        // Its image's config as moby's commit reads it (daemon/containerd
        // image_commit.go), a DockerOCIImage, failing as that fails, and written again as
        // json.Marshal writes one: what the new image keeps of it is what Go read.
        let base_config: serde_json::Value = match base.config.as_deref() {
            None => serde_json::Value::Null,
            Some(c) => match shards_dockerfile::image::Image::from_json(c).and_then(|i| i.to_json()) {
                Ok(json) => serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
                Err(e) => return refuse(String::from_utf8_lossy(&e).into_owned()),
            },
        };
        // The request it was made by: what of its config is the run's.
        let dir = lock(&self.containers).dir(&id);
        let request = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| Run::decode(&b));
        let mut config = container_config(base_config.get("config"), request.as_ref());
        let changes = parsed.many("change");
        if !changes.is_empty() {
            // moby's dockerfile.BuildFromConfig, before the config is merged (commit.go).
            config = match shards_dockerfile::commit::build_from_config(&config, changes, "linux") {
                Ok(c) => c,
                Err(e) => return refuse(e),
            };
        }
        // Held while what it writes is not yet named: no collection takes it meanwhile.
        let _lease = match store.lease() {
            Ok(l) => l,
            Err(e) => return refuse(e.to_string()),
        };
        // The layer of what it changed: as it is now, from its microVM, paused unless
        // told otherwise; or as its last run left it.
        let mut writer = match store.writer() {
            Ok(w) => w,
            Err(e) => return refuse(e.to_string()),
        };
        let mut written = 0u64;
        let running = self.running(&id) && record.state == Life::Running;
        let wrote =
            if running {
                if lock(&self.paused).contains(&id) {
                    return refuse(format!(
                        "container {id} is paused: its files can be committed once it is unpaused"
                    ));
                }
                if self.joined_run(&id) {
                    return refuse(format!(
                        "container {id}: a container joining another's network keeps no layer to read yet"
                    ));
                }
                // `--pause`, deprecated, says it where given (commit.go).
                let pause = if parsed.changed("pause") {
                    parsed.bool("pause")
                } else {
                    !parsed.bool("no-pause")
                };
                let spec = shards_abi::run::Spec {
                    builtin: shards_abi::run::builtin::LAYER,
                    argv: if pause {
                        vec![b"pause".to_vec()]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                };
                let mut failed = None;
                let ended = self.exec_streamed(&id, &spec, None, None, &mut |chunk| match writer
                    .write_all(chunk)
                {
                    Ok(()) => {
                        written += chunk.len() as u64;
                        true
                    }
                    Err(e) => {
                        failed = Some(e.to_string());
                        false
                    }
                });
                match (ended, failed) {
                    (_, Some(e)) => Err(e),
                    (Ok(e), None) if e.status == Some(0) => Ok(()),
                    (Ok(_), None) => Err("its microVM could not read its files".into()),
                    (Err(e), None) => Err(e.to_string()),
                }
            } else {
                self.await_settled(&id);
                match std::fs::File::open(dir.join(LAYER)) {
                    Ok(mut layer) => copy_into(&mut layer, &mut writer, &mut written),
                    // Never run: nothing changed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.to_string()),
                }
            };
        if let Err(e) = wrote {
            return refuse(format!("failed to export layer: {e}"));
        }
        // A tar of no entries is its end alone: nothing changed (moby's diff is then none).
        let empty = written <= 1024;
        let (layer, size) = match writer.commit() {
            Ok(done) => done,
            Err(e) => return refuse(format!("failed to export layer: {e}")),
        };
        let (config_digest, manifest_digest, manifest_len, contents) = match self.write_commit(
            &store,
            &base_manifest,
            &base_config,
            config,
            (!empty).then_some((&layer, size)),
            parsed,
        ) {
            Ok(written) => written,
            Err(e) => return refuse(e),
        };
        let desc = shards_image::oci::Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: manifest_digest.to_string(),
            size: manifest_len,
            platform: None,
            annotations: Default::default(),
        };
        let tag = match &named {
            Some(r) => r.to_string(),
            None => format!("{}{manifest_digest}", shards_image::store::DANGLING),
        };
        // Its parent recorded as dockerd labels it (image_builder.go), for `ancestor`.
        if let Err(e) = store.tag_child(&tag, &desc, &manifest_digest, &contents, &base.id) {
            return refuse(e.to_string());
        }
        let _ = config_digest;
        let image_id = manifest_digest.to_string();
        // moby: an image made (image_builder.go), then the container's commit.
        self.image_event(&image_id, &image_id, "create");
        self.container_event(
            &id,
            "commit",
            &[
                ("comment", parsed.string("message").to_string()),
                ("imageID", image_id.clone()),
                (
                    "imageRef",
                    named.as_ref().map_or(String::new(), Reference::familiar),
                ),
            ],
        );
        reply.out(&image_id);
        0
    }

    /// The new image's config and manifest, written to the store: moby's
    /// generateCommitImageConfig, then a manifest of the image's layers and `layer`;
    /// their digests, the manifest's length, and every blob the image holds.
    #[allow(clippy::type_complexity)]
    fn write_commit(
        &self,
        store: &shards_image::store::Store,
        base_manifest: &serde_json::Value,
        base_config: &serde_json::Value,
        config: serde_json::Value,
        layer: Option<(&shards_image::reference::Digest, u64)>,
        parsed: &Parsed,
    ) -> Result<
        (
            shards_image::reference::Digest,
            shards_image::reference::Digest,
            i64,
            Vec<shards_image::reference::Digest>,
        ),
        String,
    > {
        let now = super::commands::rfc3339_nano(u64::try_from(crate::spec::now()).unwrap_or(0));
        let author = match parsed.string("author") {
            "" => base_config
                .get("author")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            a => a.to_string(),
        };
        let mut diff_ids: Vec<serde_json::Value> = base_config
            .pointer("/rootfs/diff_ids")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some((digest, _)) = layer {
            diff_ids.push(digest.to_string().into());
        }
        let mut history: Vec<serde_json::Value> = base_config
            .get("history")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let created_by = config
            .get("Cmd")
            .and_then(serde_json::Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let mut step = serde_json::Map::new();
        step.insert("created".into(), now.clone().into());
        if !created_by.is_empty() {
            step.insert("created_by".into(), created_by.into());
        }
        if !author.is_empty() {
            step.insert("author".into(), author.clone().into());
        }
        let comment = parsed.string("message");
        if !comment.is_empty() {
            step.insert("comment".into(), comment.into());
        }
        if layer.is_none() {
            step.insert("empty_layer".into(), true.into());
        }
        history.push(serde_json::Value::Object(step));
        let text = |k: &str, fallback: &str| {
            base_config
                .get(k)
                .and_then(serde_json::Value::as_str)
                .filter(|v| !v.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        let mut image = serde_json::Map::new();
        image.insert("created".into(), now.into());
        if !author.is_empty() {
            image.insert("author".into(), author.into());
        }
        // The guest's, where the image says none: Go's GOARCH names.
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        image.insert("architecture".into(), text("architecture", arch).into());
        image.insert("os".into(), text("os", "linux").into());
        image.insert("config".into(), config);
        image.insert(
            "rootfs".into(),
            serde_json::json!({"type": "layers", "diff_ids": diff_ids}),
        );
        image.insert("history".into(), history.into());
        let config_bytes =
            serde_json::to_vec(&serde_json::Value::Object(image)).map_err(|e| e.to_string())?;
        let config_digest = digest_of(&config_bytes);
        let mut layers: Vec<serde_json::Value> = base_manifest
            .get("layers")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some((digest, size)) = layer {
            layers.push(
                serde_json::json!({"mediaType": LAYER_TAR, "digest": digest.to_string(), "size": size}),
            );
        }
        let mut contents: Vec<shards_image::reference::Digest> = layers
            .iter()
            .filter_map(|l| l.get("digest")?.as_str())
            .filter_map(|d| shards_image::reference::Digest::parse(d).ok())
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest.to_string(),
                "size": config_bytes.len(),
            },
            "layers": layers,
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
        contents.push(config_digest.clone());
        contents.push(manifest_digest.clone());
        let len = i64::try_from(manifest_bytes.len()).map_err(|e| e.to_string())?;
        Ok((config_digest, manifest_digest, len, contents))
    }
}

/// `src` into the store's `writer`, counting what it wrote.
fn copy_into(
    src: &mut std::fs::File,
    writer: &mut shards_image::store::BlobWriter,
    written: &mut u64,
) -> Result<(), String> {
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = src.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        writer
            .write_all(buf.get(..n).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        *written += n as u64;
    }
}

/// The SHA-256 digest of `bytes`.
fn digest_of(bytes: &[u8]) -> shards_image::reference::Digest {
    use sha2::Digest as _;
    shards_image::reference::Digest::from_hash(
        shards_image::reference::Algorithm::Sha256,
        &sha2::Sha256::digest(bytes),
    )
}

/// The container's config as dockerd keeps it (its image's, with what its run set: moby
/// daemon/create.go, merge), in the OCI image config's form commit writes
/// (containerConfigToDockerOCIImageConfig).
fn container_config(image: Option<&serde_json::Value>, run: Option<&Run>) -> serde_json::Value {
    let mut config = image.cloned().unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = config.as_object_mut() else {
        return serde_json::json!({});
    };
    // The image's exposed ports as the container's config took them (daemon/containerd
    // imagespec.go): those network.ParsePort takes, as it writes them.
    if let Some(ports) = obj.get("ExposedPorts").and_then(serde_json::Value::as_object) {
        let kept: serde_json::Map<String, serde_json::Value> = ports
            .keys()
            .filter_map(|k| shards_image::config::parse_port(k))
            .map(|k| (k, serde_json::json!({})))
            .collect();
        if kept.is_empty() {
            obj.remove("ExposedPorts");
        } else {
            obj.insert("ExposedPorts".into(), kept.into());
        }
    }
    if let Some(run) = run {
        if !run.env.is_empty() {
            let mut env: Vec<String> = obj
                .get("Env")
                .and_then(serde_json::Value::as_array)
                .map(|e| e.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            for kv in &run.env {
                let name = kv.split_once('=').map_or(kv.as_str(), |(n, _)| n);
                env.retain(|e| e.split_once('=').map_or(e.as_str(), |(n, _)| n) != name);
                if kv.contains('=') {
                    env.push(kv.clone());
                }
            }
            obj.insert("Env".into(), env.into());
        }
        if let Some(entrypoint) = &run.entrypoint {
            obj.insert("Entrypoint".into(), entrypoint.clone().into());
            // An entrypoint given takes the image's command away (moby create.go).
            if run.cmd.is_empty() {
                obj.remove("Cmd");
            }
        }
        if !run.cmd.is_empty() {
            obj.insert("Cmd".into(), run.cmd.clone().into());
        }
        if !run.workdir.is_empty() {
            obj.insert("WorkingDir".into(), run.workdir.clone().into());
        }
        if !run.user.is_empty() {
            obj.insert("User".into(), run.user.clone().into());
        }
        if let Some(signal) = &run.stop_signal {
            obj.insert("StopSignal".into(), signal.clone().into());
        }
        // Published ports are exposed (moby daemon/create.go, merge of the host config).
        for p in &run.publish {
            let ports = obj.entry("ExposedPorts").or_insert_with(|| serde_json::json!({}));
            if let Some(ports) = ports.as_object_mut() {
                ports.insert(format!("{}/{}", p.port, p.proto), serde_json::json!({}));
            }
        }
    }
    config
}
