//! `shards push` as dockerd's containerd store pushes (moby docker-v29.3.1
//! daemon/containerd/image_push.go, PushImage and pushRef) and docker/cli reports it
//! (cli/command/image/push.go, runPush and handleAux): each tag of the repository, or the
//! one named; what it resolved to, or, when that is an index not all of whose content is
//! here, our platform's manifest alone, with a note saying so; each layer said as it is
//! pushed, found there already, or mounted; then the tag's digest and size.
//!
//! Unlike dockerd: the layers of platforms that are not here are not listed as
//! "Unavailable", once for each time its progress is redrawn.

use shards_image::reference::{Digest, Reference};
use shards_registry::push::Layer;

/// tui's InfoHeader, and PrintNote's layout: a blank line, the header, each further line
/// indented by its width, colours only on a terminal.
fn note(text: &str, color: bool) -> String {
    let header = if color {
        "\x1b[1m\x1b[106m\x1b[30mi\x1b[0m\x1b[0m \x1b[96mInfo → \x1b[0m\x1b[0m"
    } else {
        " Info -> "
    };
    let width = if color { 9 } else { header.len() };
    let mut out = format!("\n{header}");
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push_str(&" ".repeat(width));
        }
        if color {
            out.push_str(&format!("\x1b[3m{line}\x1b[0m"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards push NAME[:TAG]`, `-a` for every tag of the repository.
    pub(super) fn push(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let given = parsed.args.first().map(String::as_str).unwrap_or_default();
        let (all, quiet) = (parsed.bool("all-tags"), parsed.bool("quiet"));
        let mut named = match Reference::parse_normalized(given) {
            Ok(r) => r,
            Err(e) => return refuse(&e.to_string()),
        };
        let name_only = named.tag.is_none() && named.digest.is_none();
        if all && !name_only {
            return refuse("tag can't be used with --all-tags/-a");
        }
        let say = |line: String| {
            if !quiet {
                reply.out(&line);
            }
        };
        if !all && name_only {
            named.tag = Some("latest".into());
            say("Using default tag: latest".into());
        }
        let mut repo = named.clone();
        repo.tag = None;
        repo.digest = None;
        say(format!("The push refers to repository [{}]", repo.name()));
        let store = match crate::pull::store(&self.home) {
            Ok(store) => store,
            Err(e) => return refuse(&e),
        };
        let lease = match store.lease() {
            Ok(lease) => lease,
            Err(e) => return refuse(&e.to_string()),
        };
        let images = match store.named() {
            Ok(images) => images,
            Err(e) => return refuse(&e.to_string()),
        };
        // The tags to push: the one named, or all of the repository's, in name order.
        let wanted: Vec<String> = if all {
            let prefix = format!("{}:", repo.name());
            let mut tags: Vec<String> = images
                .iter()
                .flat_map(|i| i.references.iter())
                .filter(|r| r.starts_with(&prefix) && !r.contains('@'))
                .cloned()
                .collect();
            tags.sort();
            if tags.is_empty() {
                return refuse(&format!(
                    "An image does not exist locally with the tag: {}",
                    repo.familiar()
                ));
            }
            tags
        } else {
            vec![named.to_string()]
        };
        // A shutdown ends it, and so does the client's going: its uploads stop at once.
        let (status, _) = self.cancellable(asker.client, reply.0, |cancel| {
            let mut notes = Vec::new();
            // One registry client for the repository and each repository blobs mount from:
            // its connections and the challenges it answered serve every tag.
            let mut registries = std::collections::HashMap::new();
            for name in &wanted {
                let Some(named) = images.iter().find(|i| i.references.contains(name)) else {
                    let shown = Reference::parse_normalized(name).map_or_else(|_| name.clone(), |r| r.familiar());
                    return refuse(&format!("tag does not exist: {shown}"));
                };
                // Only what is pushed is read.
                let image = match store.image(named) {
                    Ok(image) => image,
                    Err(e) => return refuse(&e.to_string()),
                };
                let reference = match Reference::parse_normalized(name) {
                    Ok(r) => r,
                    Err(e) => return refuse(&e.to_string()),
                };
                let target = image.targets.get(name).unwrap_or(&image.target).clone();
                // A repository of the same registry the image came from: its blobs mount.
                let from = mount_source(&image.sources, &reference);
                let registry = match registries.entry(from.clone()) {
                    std::collections::hash_map::Entry::Occupied(held) => held.into_mut(),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        match crate::pull::registry_for_push(&reference, from.as_deref(), Some(cancel), &|k| {
                            shards_ipc::env_value(&asker.registry_env, k)
                        }) {
                            Ok(r) => slot.insert(r),
                            Err(e) => return refuse(&e),
                        }
                    }
                };
                let tag = reference.tag.clone();
                let report = |digest: &Digest, fate: Layer| {
                    let short = digest.hex().get(..12).unwrap_or_default().to_string();
                    if quiet {
                        return;
                    }
                    reply.out(&match fate {
                        Layer::Pushed => format!("{short}: Pushed"),
                        Layer::Exists => format!("{short}: Layer already exists"),
                        Layer::Mounted(repo) => format!("{short}: Mounted from {repo}"),
                    });
                };
                let mut pushed = target.clone();
                let mut result = shards_registry::push::push(
                    registry,
                    &store,
                    &target,
                    tag.as_deref(),
                    from.as_deref(),
                    &report,
                );
                // An index not all here: our platform's manifest alone (getPushDescriptor).
                if let Err(e) = &result
                    && e.kind() == shards_registry::ErrorKind::Missing
                    && target.digest != image.manifest.to_string()
                {
                    let mut manifest = target.clone();
                    manifest.digest = image.manifest.to_string();
                    manifest.media_type = shards_image::oci::media::OCI_MANIFEST.into();
                    manifest.annotations.clear();
                    manifest.platform = None;
                    if let Ok(meta) = std::fs::metadata(store.blob_path(&image.manifest)) {
                        manifest.size = i64::try_from(meta.len()).unwrap_or(0);
                    }
                    if let Ok(bytes) = std::fs::read(store.blob_path(&image.manifest))
                        && let Some(kind) =
                            serde_json::from_slice::<serde_json::Value>(&bytes)
                                .ok()
                                .and_then(|v| {
                                    v.get("mediaType")
                                        .and_then(serde_json::Value::as_str)
                                        .map(String::from)
                                })
                    {
                        manifest.media_type = kind;
                    }
                    result = shards_registry::push::push(
                        registry,
                        &store,
                        &manifest,
                        tag.as_deref(),
                        from.as_deref(),
                        &report,
                    );
                    if result.is_ok() {
                        let (red, green, reset) = if asker.terminal && asker.color {
                            ("\x1b[31m", "\x1b[32m", "\x1b[0m")
                        } else {
                            ("", "", "")
                        };
                        notes.push(format!(
                            "Not all multiplatform-content is present and only the available single-platform image was pushed\n{red}{}{reset} -> {green}{}{reset}",
                            target.digest, manifest.digest
                        ));
                        pushed = manifest;
                    }
                }
                if let Err(e) = result {
                    for n in &notes {
                        let _ = reply.bytes(
                            crate::spec::LOG_STDOUT,
                            note(n, asker.terminal && asker.color).as_bytes(),
                        );
                    }
                    // A push the daemon cancelled as it stops says so, as a run's prepare does.
                    if cancel.is_cancelled() && self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        return refuse("the daemon is shutting down");
                    }
                    return refuse(&e.to_string());
                }
                if let Some(tag) = &tag {
                    say(format!("{tag}: digest: {} size: {}", pushed.digest, pushed.size));
                }
            }
            for n in &notes {
                let _ = reply.bytes(
                    crate::spec::LOG_STDOUT,
                    note(n, asker.terminal && asker.color).as_bytes(),
                );
            }
            if quiet {
                reply.out(&named.to_string());
            }
            0
        });
        drop(lease);
        status
    }
}

/// The repository blobs are mounted from, as containerd's pusher picks it
/// (selectRepositoryMountCandidate, pusher.go v2.4.1): of the repositories on the same
/// registry the image came from, `sources`, sorted, not the target itself, the one whose
/// path shares the most leading components with the target's, the later on a tie.
fn mount_source(sources: &[String], target: &Reference) -> Option<String> {
    let components: Vec<&str> = target.path.split('/').collect();
    let shared = |path: &str| {
        components
            .iter()
            .zip(path.split('/'))
            .take_while(|(a, b)| *a == b)
            .count()
    };
    sources
        .iter()
        .filter_map(|s| s.split_once('/'))
        .filter(|(host, path)| *host == target.domain && *path != target.path)
        .fold(None, |best: Option<(usize, &str)>, (_, path)| {
            let n = shared(path);
            match best {
                Some((most, _)) if n < most => best,
                _ => Some((n, path)),
            }
        })
        .map(|(_, path)| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// containerd's pick: the most leading components shared, the later on a tie, never
    /// the target itself or another registry's.
    #[test]
    fn blobs_mount_from_the_repository_containerd_picks() {
        let target = Reference::parse_normalized("registry.example/team/app/web").unwrap();
        let sources = |s: &[&str]| s.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let pick = |s: &[&str]| mount_source(&sources(s), &target);
        assert_eq!(
            pick(&[
                "registry.example/other/thing",
                "registry.example/team/app/api",
                "registry.example/team/x"
            ])
            .as_deref(),
            Some("team/app/api")
        );
        assert_eq!(
            pick(&["registry.example/a/one", "registry.example/b/two"]).as_deref(),
            Some("b/two"),
            "a tie: the later"
        );
        assert_eq!(
            pick(&["registry.example/team/app/web"]),
            None,
            "the target itself"
        );
        assert_eq!(
            pick(&["elsewhere.example/team/app/api"]),
            None,
            "another registry"
        );
        assert_eq!(pick(&[]), None);
    }
}
