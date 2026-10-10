//! The LLB capabilities a definition needs, as moby/buildkit dockerfile/1.27.1's
//! `client/llb` records them: each op's in its metadata (`addCap`, as the vertex is made
//! and marshalled), the root's as `State.Marshal` adds it, and the words its gateway
//! client refuses one in when the worker lacks it (`apicaps.CapError`), which it checks
//! before it asks for a solve (`grpcClient.Solve`).

use std::collections::BTreeSet;

use crate::llb::{Meta, NetMode, Op, OpKind, OpMountKind, Security};

/// `op`'s capabilities, `meta` its metadata. `sets_default_path`: whether the capabilities
/// the definition is marshalled with (the worker's, `llb.WithCaps`) have
/// `exec.meta.setsdefaultpath`, so that a command leaves PATH's default to the worker and
/// says so (exec.go).
pub fn op(op: &Op, meta: &Meta, sets_default_path: bool) -> BTreeSet<&'static str> {
    let mut caps = BTreeSet::new();
    match &op.kind {
        OpKind::Source { identifier, attrs } => {
            let has = |k: &str| attrs.contains_key(k.as_bytes());
            let mut add = |c: &'static str, on: bool| {
                if on {
                    caps.insert(c);
                }
            };
            if identifier.starts_with(b"docker-image://") {
                add("source.image", true);
                // Only the forced pull needs the worker's support (source.go).
                add(
                    "source.image.resolvemode",
                    attrs
                        .get(b"image.resolvemode".as_slice())
                        .is_some_and(|m| m == b"pull"),
                );
                add("source.image.layerlimit", has("image.layerlimit"));
                add("source.image.checksum", has("image.checksum"));
            } else if identifier.starts_with(b"local://") {
                add("source.local", true);
                add("source.local.sessionid", has("local.session"));
                add("source.local.includepatterns", has("local.includepattern"));
                add("source.local.followpaths", has("local.followpaths"));
                add("source.local.excludepatterns", has("local.excludepatterns"));
                add("source.local.sharedkeyhint", has("local.sharedkeyhint"));
                add("source.local.metadatatransfer", has("local.metadatatransfer"));
                add("source.local.unique", has("local.unique"));
                // `local.differ` needs none: the frontend never makes it required.
            } else if identifier.starts_with(b"git://") {
                add("source.git", true);
                add("source.git.keepgitdir", has("git.keepgitdir"));
                add("source.git.fullurl", has("git.fullurl"));
                // An SSH remote's: its known hosts (scanned or not) and agent socket.
                add("source.git.knownsshhosts", has("git.mountsshsock"));
                add("source.git.mountsshsock", has("git.mountsshsock"));
                add("source.git.checksum", has("git.checksum"));
                add("source.git.skipsubmodules", has("git.skipsubmodules"));
                add("source.git.mtime", has("git.mtime"));
                add("source.git.fetchbycommit", has("git.fetchbycommit"));
                add("source.git.bundle", has("git.bundle"));
                add("source.git.checkoutbundle", has("git.checkoutbundle"));
                // `source.git.httpauth` only for secrets named explicitly, which the
                // frontend never names: its attributes are the defaults.
            } else if identifier.starts_with(b"http://") || identifier.starts_with(b"https://") {
                add("source.http", true);
                add("source.http.checksum", has("http.checksum"));
                add("source.http.perm", has("http.perm"));
                add("soruce.http.uidgid", has("http.uid") || has("http.gid"));
                add("source.http.auth", has("http.authheadersecret"));
                add(
                    "source.http.header",
                    attrs.keys().any(|k| k.starts_with(b"http.header.")),
                );
                add(
                    "source.http.signatureverify",
                    has("http.sig.pubkey") || has("http.sig.signature"),
                );
            } else if identifier.starts_with(b"oci-layout://") {
                add("source.ocilayout", true);
            } else if identifier.starts_with(b"docker-image+blob://")
                || identifier.starts_with(b"oci-layout+blob://")
            {
                // A blob by digest, from a registry or a client's layout (llb.ImageBlob,
                // llb.OCILayoutBlob): the one capability either asks.
                add("source.imageblob", true);
            }
        }
        OpKind::Exec {
            process,
            mounts,
            network,
            security,
            secret_env,
            devices,
        } => {
            if sets_default_path {
                caps.insert("exec.meta.setsdefaultpath");
            }
            if !process.ulimits.is_empty() {
                caps.insert("exec.meta.ulimit");
            }
            if meta.linux_resources.is_some() {
                caps.insert("exec.meta.linux.resources");
            }
            if *network != NetMode::Sandbox {
                caps.insert("exec.meta.network");
            }
            if *security == Security::Insecure {
                caps.insert("exec.meta.security");
            }
            if process.proxy.is_some() {
                caps.insert("exec.meta.proxyenv");
            }
            caps.insert("exec.meta.base");
            // A secret, to a file or into a variable, and an SSH socket are mounts of
            // their own kinds here, but lists of their own in llb's ExecOp.
            let mut secrets = !secret_env.is_empty();
            for m in mounts {
                match &m.kind {
                    OpMountKind::Secret { .. } => {
                        secrets = true;
                        continue;
                    }
                    OpMountKind::Ssh { .. } => {
                        caps.insert("exec.mount.ssh");
                        continue;
                    }
                    _ => {}
                }
                if !m.selector.is_empty() {
                    caps.insert("exec.mount.selector");
                }
                match &m.kind {
                    OpMountKind::Cache { .. } => {
                        caps.insert("exec.mount.cache");
                        caps.insert("exec.mount.cache.sharing");
                    }
                    OpMountKind::Tmpfs { size } => {
                        caps.insert("exec.mount.tmpfs");
                        if *size > 0 {
                            caps.insert("exec.mount.tmpfs.size");
                        }
                    }
                    // A mount of something, not of scratch.
                    OpMountKind::Bind if m.input >= 0 => {
                        caps.insert("exec.mount.bind");
                    }
                    _ => {}
                }
            }
            if secrets {
                caps.insert("exec.mount.secret");
                if !secret_env.is_empty() {
                    caps.insert("exec.secretenv");
                }
            }
            if !devices.is_empty() {
                caps.insert("exec.meta.cdi");
            }
        }
        // `file.base` alone: dockerfile/1.27.1's FileOp.Marshal asks each action for its
        // capabilities from a marshal state that has none yet, so a copy's include and
        // exclude patterns, required paths and mode strings add nothing (fileop.go).
        OpKind::File { .. } => {
            caps.insert("file.base");
        }
        OpKind::Merge => {
            caps.insert("mergeop");
        }
        // shards' own step (D54) needs nothing BuildKit names.
        OpKind::Skills { .. } => {}
    }
    caps
}

/// The root's capabilities (`State.Marshal`): constraints and platforms, and the
/// metadata any op of the definition carries.
pub fn root(metadata: &[Meta]) -> BTreeSet<&'static str> {
    let mut caps = BTreeSet::from(["constraints", "platform"]);
    if metadata.iter().any(|m| m.ignore_cache) {
        caps.insert("meta.ignorecache");
    }
    if metadata.iter().any(|m| !m.description.is_empty()) {
        caps.insert("meta.description");
    }
    caps
}

/// How the worker has a capability it was asked for: not at all, or disabled, with the
/// reason it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lacks<'a> {
    Absent,
    Disabled(&'a str),
}

/// `apicaps.CapError` of LLB capability `id`, which dockerfile/1.27.1 knows (solver/pb's
/// caps.go: every capability experimental but `file.base`, prerelease and hinted, and
/// named only `exporter.sourcedateepoch`), for the product the worker names
/// (BUILDKIT_EXPORTEDPRODUCT).
pub fn refusal(id: &str, lacks: Lacks<'_>, product: &str) -> String {
    let status = if id == "file.base" {
        "prerelease "
    } else {
        "experimental "
    };
    let name = if id == "exporter.sourcedateepoch" {
        "(source date epoch)"
    } else {
        ""
    };
    let mut out = format!("requested {status}feature {id} {name}");
    match lacks {
        Lacks::Absent => {
            out.push_str(" is not supported by build server");
            let hint = match (id, product) {
                ("file.base", "docker") => Some("Docker v19.03"),
                ("file.base", "buildkit") => Some("BuildKit v0.5.0"),
                _ => None,
            };
            if let Some(h) = hint {
                out.push_str(&format!(" (added in {h})"));
            }
            out.push_str(&format!(", please update {product}"));
        }
        Lacks::Disabled(reason) => {
            out.push_str(" has been disabled on the build server");
            if !reason.is_empty() {
                out.push_str(&format!(": {reason}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CapError.Error`'s words (util/apicaps/caps.go), for the product Docker 29.3.1
    /// names (none) and the others.
    #[test]
    fn refusals_are_said_as_apicaps_says_them() {
        assert_eq!(
            refusal("source.git.mtime", Lacks::Absent, ""),
            "requested experimental feature source.git.mtime  is not supported by build server, please update "
        );
        assert_eq!(
            refusal("file.base", Lacks::Absent, "docker"),
            "requested prerelease feature file.base  is not supported by build server (added in Docker v19.03), please update docker"
        );
        assert_eq!(
            refusal("exec.meta.cdi", Lacks::Disabled("no CDI"), "buildkit"),
            "requested experimental feature exec.meta.cdi  has been disabled on the build server: no CDI"
        );
    }

    /// A blob by digest, from a registry or from a client's OCI layout, asks
    /// `source.imageblob` alone, as llb.ImageBlob and llb.OCILayoutBlob add it; a layout's
    /// image asks `source.ocilayout`.
    #[test]
    fn blob_sources_ask_for_image_blobs() {
        let source = |id: &[u8], attrs: &[(&[u8], &[u8])]| Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: id.to_vec(),
                attrs: attrs.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect(),
            },
            platform: None,
        };
        let caps = |o: &Op| op(o, &Meta::default(), false);
        let blob = [
            (&b"http.filename"[..], &b"blob"[..]),
            (b"oci.session", b"s"),
            (b"oci.store", b"x"),
        ];
        assert_eq!(
            caps(&source(b"oci-layout+blob://docker.io/library/x@sha256:00", &blob)),
            BTreeSet::from(["source.imageblob"])
        );
        assert_eq!(
            caps(&source(
                b"docker-image+blob://docker.io/library/x@sha256:00",
                &blob[..1]
            )),
            BTreeSet::from(["source.imageblob"])
        );
        assert_eq!(
            caps(&source(b"oci-layout://x@sha256:00", &[])),
            BTreeSet::from(["source.ocilayout"])
        );
    }
}
