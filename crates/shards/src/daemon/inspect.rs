//! `shards image inspect` as dockerd's containerd store answers `docker image inspect`
//! (moby docker-v29.3.1 daemon/containerd/image_inspect.go; api/types/image
//! InspectResponse) and docker/cli prints it (cli/command/inspect/inspector.go,
//! IndentedInspector): each image's document in the API's field order, compact as Go
//! encodes it, then all of them as one array indented by encoding/json's Indent.

use std::time::SystemTime;

use shards_dockerfile::image::{Image as Config, json_string};
use shards_image::reference::Reference;
use shards_image::store::Image;

/// What moby's collectRepoTagsAndDigests makes of an image's references: each name,
/// familiar (a name with a digest among them); and each name's repository with the
/// image's ID. Dangling names give neither.
fn tags_and_digests(image: &Image) -> (Vec<String>, Vec<String>) {
    let (mut tags, mut digests): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let push = |list: &mut Vec<String>, s: String| {
        if !list.contains(&s) {
            list.push(s);
        }
    };
    for name in &image.references {
        if name.starts_with(super::rmi::DANGLING) {
            continue;
        }
        let Ok(r) = Reference::parse_normalized(name) else {
            push(&mut tags, name.clone());
            continue;
        };
        push(&mut tags, r.familiar());
        // A name with a digest names this image's ID: its repository with the ID is it.
        let mut digested = r;
        digested.tag = None;
        digested.digest = Some(image.id.clone());
        push(&mut digests, digested.familiar());
    }
    (tags, digests)
}

/// A JSON array of strings, as Go writes one.
fn strings(list: &[String]) -> String {
    let items: Vec<String> = list.iter().map(|s| json_string(s.as_bytes())).collect();
    format!("[{}]", items.join(","))
}

/// A time as Go's time.Time marshals it, in UTC: RFC 3339 with what fraction it has.
fn time_json(t: SystemTime) -> String {
    let since = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let mut time = shards_dockerfile::go::Time::from_unix(i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
    time.nanosecond = since.subsec_nanos();
    time.rfc3339_nano()
        .map_or_else(|_| "\"1970-01-01T00:00:00Z\"".into(), |s| format!("\"{s}\""))
}

/// `image`'s InspectResponse, compact: fields in the struct's order, those Go leaves out
/// when empty left out.
pub(super) fn document(image: &Image, record: Option<&str>) -> String {
    let (tags, digests) = tags_and_digests(image);
    let config = image.config.as_deref().and_then(|b| Config::from_json(b).ok());
    let mut o = vec![
        format!("\"Id\":{}", json_string(image.id.to_string().as_bytes())),
        format!("\"RepoTags\":{}", strings(&tags)),
        format!("\"RepoDigests\":{}", strings(&digests)),
    ];
    if let Some(comment) = config
        .as_ref()
        .and_then(|c| c.history.last())
        .map(|h| &h.comment)
        .filter(|c| !c.is_empty())
    {
        o.push(format!("\"Comment\":{}", json_string(comment)));
    }
    if let Some(created) = config
        .as_ref()
        .and_then(|c| c.created.as_ref())
        .and_then(|t| t.rfc3339_nano().ok())
    {
        o.push(format!("\"Created\":{}", json_string(created.as_bytes())));
    }
    if let Some(author) = config.as_ref().map(|c| &c.author).filter(|a| !a.is_empty()) {
        o.push(format!("\"Author\":{}", json_string(author)));
    }
    let empty = Vec::new();
    let platform = config.as_ref().map(|c| &c.platform);
    o.push(format!(
        "\"Config\":{}",
        config
            .as_ref()
            .map_or_else(|| "{}".to_string(), |c| c.config.to_json())
    ));
    o.push(format!(
        "\"Architecture\":{}",
        json_string(platform.map_or(&empty, |p| &p.architecture))
    ));
    if let Some(v) = platform.map(|p| &p.variant).filter(|v| !v.is_empty()) {
        o.push(format!("\"Variant\":{}", json_string(v)));
    }
    o.push(format!(
        "\"Os\":{}",
        json_string(platform.map_or(&empty, |p| &p.os))
    ));
    if let Some(v) = platform.map(|p| &p.os_version).filter(|v| !v.is_empty()) {
        o.push(format!("\"OsVersion\":{}", json_string(v)));
    }
    // The content here of our platform's image, walked from what its reference resolved
    // to: the index, if it is one, and our manifest, its config and layers. Its root
    // filesystem is not counted: dockerd 29.3.1 (containerd snapshotter) shows no
    // snapshot here, measured on four images (2026-10-02), where `images` counts it.
    let index = if image.target.digest == image.manifest.to_string() {
        0
    } else {
        u64::try_from(image.target.size).unwrap_or(0)
    };
    let size = image
        .manifests
        .iter()
        .find(|m| m.digest == image.manifest)
        .map_or(0, |m| m.content)
        .saturating_add(index);
    o.push(format!("\"Size\":{size}"));
    let mut rootfs = Vec::new();
    if let Some(c) = &config {
        if !c.rootfs.kind.is_empty() {
            rootfs.push(format!("\"Type\":{}", json_string(&c.rootfs.kind)));
        }
        if let Some(layers) = c.rootfs.diff_ids.as_ref().filter(|l| !l.is_empty()) {
            let items: Vec<String> = layers.iter().map(|l| json_string(l)).collect();
            rootfs.push(format!("\"Layers\":[{}]", items.join(",")));
        }
    }
    o.push(format!("\"RootFS\":{{{}}}", rootfs.join(",")));
    o.push(format!(
        "\"Metadata\":{{\"LastTagTime\":{}}}",
        time_json(image.tagged_at.unwrap_or(SystemTime::UNIX_EPOCH))
    ));
    // The record's own description of what it resolved to, if it keeps one.
    let t = record.and_then(|r| image.targets.get(r)).unwrap_or(&image.target);
    o.push(format!("\"Descriptor\":{}", descriptor(t)));
    let pulls: Vec<String> = image
        .sources
        .iter()
        .map(|s| format!("{{\"Repository\":{}}}", json_string(s.as_bytes())))
        .collect();
    // dockerd leaves out an identity it knows nothing of.
    if !pulls.is_empty() {
        o.push(format!("\"Identity\":{{\"Pull\":[{}]}}", pulls.join(",")));
    }
    format!("{{{}}}", o.join(","))
}

/// An ocispec.Descriptor as Go encodes it: mediaType, digest, size, then the annotations,
/// keys sorted, and the platform, each only if it has any.
fn descriptor(t: &shards_image::oci::Descriptor) -> String {
    let s = |v: &str| json_string(v.as_bytes());
    let mut d = format!(
        "{{\"mediaType\":{},\"digest\":{},\"size\":{}",
        s(&t.media_type),
        s(&t.digest),
        t.size
    );
    if !t.annotations.is_empty() {
        let pairs: Vec<String> = t
            .annotations
            .iter()
            .map(|(k, v)| format!("{}:{}", s(k), s(v)))
            .collect();
        d.push_str(&format!(",\"annotations\":{{{}}}", pairs.join(",")));
    }
    if let Some(p) = &t.platform {
        let mut fields = vec![
            format!("\"architecture\":{}", s(&p.architecture)),
            format!("\"os\":{}", s(&p.os)),
        ];
        if !p.os_features.is_empty() {
            let f: Vec<String> = p.os_features.iter().map(|x| s(x)).collect();
            fields.push(format!("\"os.features\":[{}]", f.join(",")));
        }
        if let Some(v) = p.variant.as_deref().filter(|v| !v.is_empty()) {
            fields.push(format!("\"variant\":{}", s(v)));
        }
        d.push_str(&format!(",\"platform\":{{{}}}", fields.join(",")));
    }
    d.push('}');
    d
}

/// encoding/json's Indent with no prefix and `indent`, of compact JSON: each member and
/// element on its own line, `: ` after keys, empty objects and arrays kept whole.
pub(super) fn indent(compact: &str, indent: &str) -> String {
    let mut out = String::with_capacity(compact.len() * 2);
    let (mut depth, mut need, mut string, mut escaped) = (0usize, false, false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    for c in compact.chars() {
        if string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                string = false;
            }
            continue;
        }
        if need && c != '}' && c != ']' {
            need = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                string = true;
                out.push(c);
            }
            '{' | '[' => {
                need = true;
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '}' | ']' => {
                if need {
                    need = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// The record a name given names: the name, with `latest` if it names no tag; none for
/// an ID or a digest.
fn record_name(given: &str) -> Option<String> {
    match shards_image::reference::AnyReference::parse(given) {
        Ok(shards_image::reference::AnyReference::Named(r)) if r.digest.is_none() => {
            Some(r.tag_name_only().to_string())
        }
        _ => None,
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards image inspect IMAGE...` (docker/cli inspect.Inspect): the documents of the
    /// images found, as one array, then what could not be found.
    pub(super) fn image_inspect(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        styled: bool,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let (store, images) = match self.store() {
            Ok(Some(store)) => match store.named() {
                Ok(images) => (Some(store), images),
                Err(e) => {
                    reply.err(&format!("Error response from daemon: {e}"));
                    return 1;
                }
            },
            Ok(None) => (None, Vec::new()),
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let (mut documents, mut errors) = (Vec::new(), Vec::new());
        for given in &parsed.args {
            // Only what is asked for is read.
            let read = super::images::resolve(&images, given).and_then(|named| {
                store
                    .as_ref()
                    .ok_or_else(|| super::images::not_found(given))?
                    .image(named)
                    .map_err(|e| e.to_string())
            });
            match read {
                Ok(image) => documents.push(super::inspect_doc::image_value(&document(
                    &image,
                    record_name(given).as_deref(),
                ))),
                Err(e) => errors.push(format!("Error response from daemon: {e}")),
            }
        }
        inspected(parsed.string("format"), &documents, errors, styled, reply)
    }

    /// `shards save IMAGE...` (moby ImageExport): every image found first, as dockerd
    /// finds them all before it streams; then the archive, written to what the client
    /// sent (its stdout, or its `-o` file), under a lease, so that no collection takes a
    /// blob meanwhile.
    pub(super) fn save(
        &self,
        args: &[String],
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        use shards_image::reference::AnyReference;
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let Some(out) = asker.files.first() else {
            return refuse("shards: save: the client sent nowhere to write");
        };
        let store = match self.store() {
            Ok(Some(store)) => store,
            Ok(None) => {
                let given = args.first().map(String::as_str).unwrap_or_default();
                return refuse(&format!(
                    "Error response from daemon: {}",
                    super::images::not_found(given)
                ));
            }
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
        let lease = store.lease();
        let images = match lease
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|_| store.named().map_err(|e| e.to_string()))
        {
            Ok(images) => images,
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
        let mut asked: Vec<(&shards_image::store::Named, Option<String>)> = Vec::with_capacity(args.len());
        for given in args {
            let resolved = super::images::resolve(&images, given);
            let parsed = AnyReference::parse(given);
            // dockerd's ExportImage: a name that is no start of the ID it found, nor holds
            // a digest, and names a repository alone, is every tag of the repository,
            // each saved by its name, in name order.
            let by_id = resolved.as_ref().is_ok_and(|i| {
                let algorithm = format!("{}:", i.id.algorithm().name());
                i.id.hex()
                    .starts_with(given.strip_prefix(&algorithm).unwrap_or(given))
            });
            let digested = matches!(&parsed, Ok(AnyReference::Named(r)) if r.digest.is_some())
                || matches!(&parsed, Ok(AnyReference::Digest(_)));
            if !by_id
                && !digested
                && let Ok(AnyReference::Named(repo)) = &parsed
                && repo.tag.is_none()
            {
                let mut tagged: Vec<(String, &shards_image::store::Named)> = images
                    .iter()
                    .flat_map(|i| i.references.iter().map(move |r| (r, i)))
                    .filter(|(r, _)| {
                        shards_image::reference::Reference::parse_normalized(r)
                            .is_ok_and(|r| r.name() == repo.name() && r.tag.is_some() && r.digest.is_none())
                    })
                    .map(|(r, i)| (r.clone(), i))
                    .collect();
                tagged.sort_by(|a, b| a.0.cmp(&b.0));
                if tagged.is_empty() {
                    return refuse(&format!(
                        "Error response from daemon: No such image: {}:latest",
                        repo.familiar()
                    ));
                }
                for (name, image) in tagged {
                    asked.push((image, Some(name)));
                }
                continue;
            }
            let image = match resolved {
                Ok(image) => image,
                Err(e) => return refuse(&format!("Error response from daemon: {e}")),
            };
            // Asked by name, the name it was found by; by ID or digest, none.
            let name = match AnyReference::parse(given) {
                Ok(AnyReference::Named(r)) if r.digest.is_none() => {
                    Some(r.tag_name_only().to_string()).filter(|n| image.references.contains(n))
                }
                _ => None,
            };
            asked.push((image, name));
        }
        // Each image asked for, read once, however often it was asked for.
        let mut read: std::collections::BTreeMap<
            &shards_image::reference::Digest,
            shards_image::store::Image,
        > = std::collections::BTreeMap::new();
        for (named, _) in &asked {
            if !read.contains_key(&named.id) {
                match store.image(named) {
                    Ok(image) => {
                        read.insert(&named.id, image);
                    }
                    Err(e) => return refuse(&format!("Error response from daemon: {e}")),
                }
            }
        }
        let asked: Vec<shards_image::save::Asked<'_>> = asked
            .into_iter()
            .filter_map(|(named, name)| {
                read.get(&named.id)
                    .map(|image| shards_image::save::Asked { image, name })
            })
            .collect();
        let written = out.try_clone().map_err(|e| e.to_string()).and_then(|fd| {
            let w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::from(fd));
            shards_image::save::save(&store, &asked, w).map_err(|e| e.to_string())
        });
        match written {
            Ok(()) => {
                // moby daemon/containerd/image_exporter.go: each image's digest, named so.
                for image in read.values() {
                    self.image_event(&image.target.digest, &image.target.digest, "save");
                }
                0
            }
            Err(e) => refuse(&format!("Error response from daemon: {e}")),
        }
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards inspect vm NAME...` (`docker container inspect`): each microVM's document
    /// (inspect_doc.rs), as docker/cli's inspect.Inspect prints them; then what was not
    /// found.
    pub(super) fn container_inspect(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        styled: bool,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        for given in &parsed.args {
            let found = self.resolve(given).and_then(|id| {
                super::lock(&self.containers)
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| format!("No such container: {given}"))
            });
            match found {
                Ok(c) => documents.push(self.container_value(&c, parsed.bool("size"))),
                Err(e) if e.starts_with("Error response") => errors.push(e),
                Err(e) => errors.push(format!("Error response from daemon: {e}")),
            }
        }
        inspected(parsed.string("format"), &documents, errors, styled, reply)
    }

    /// Container `c`'s InspectResponse, from its record, its request, its image and its
    /// VM (inspect_doc.rs).
    fn container_value(&self, c: &crate::containers::Container, sized: bool) -> shards_template::Value {
        use super::inspect_doc::{Facts, Health, Net, document};
        let dir = super::lock(&self.containers).dir(&c.id);
        let request = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .unwrap_or_else(|| shards_ipc::Run {
                image: c.image.clone(),
                ..shards_ipc::Run::default()
            });
        let (config, manifest) = self.image_facts(c.image_id.as_deref());
        // The run's inbox is locked after `runs` is let go: a run's end holds its inbox
        // while it takes `runs` (take_messages, run_ended).
        let tracked = match super::lock(&self.runs).get(&c.id) {
            Some(super::RunState::Tracked(t)) if !t.visit => Some((t.vm.id(), t.mac, t.inbox.clone())),
            _ => None,
        };
        let (pid, mac, exec_ids) = match tracked {
            Some((pid, mac, inbox)) => {
                let execs = super::lock(&inbox)
                    .exec_ids
                    .iter()
                    .map(|(_, e)| e.clone())
                    .collect();
                (Some(pid), mac, execs)
            }
            None => (None, None, Vec::new()),
        };
        let health = super::lock(&self.health).get(&c.id).map(|h| Health {
            status: match h.status {
                super::health::Status::Starting => "starting",
                super::health::Status::Healthy => "healthy",
                super::health::Status::Unhealthy => "unhealthy",
            },
            failing_streak: h.failing_streak,
            log: h
                .log
                .iter()
                .map(|p| (p.start_ns, p.end_ns, p.exit_code, p.output.clone()))
                .collect(),
        });
        let net = self.bridge.as_ref().map(|b| Net {
            ip: b.guest(),
            gateway: b.gateway(),
            prefix: b.subnet().1,
            mac,
        });
        let paused = super::lock(&self.paused).contains(&c.id);
        // A user network's endpoint (D46): its own while it runs; its network's ID and
        // the names it would have, kept while it is stopped, as dockerd keeps them.
        let user_net = match self.membership(&c.id) {
            Some((n, m)) => Some(super::inspect_doc::UserNet {
                name: n.name.clone(),
                network_id: n.id.clone(),
                endpoint: m.endpoint,
                ip: Some(m.ip),
                gateway: n.pools.first().map(|p| p.gateway),
                prefix: n.pools.first().map_or(0, |p| p.subnet.1),
                mac: m.mac,
                dns_names: m.dns_names,
            }),
            None => self
                .user_network(
                    &self
                        .connected_network(&request)
                        .unwrap_or_else(|| request.network.clone()),
                )
                .map(|n| {
                    let asked = request.endpoints.iter().find(|e| e.network == n.name);
                    let mut dns_names = vec![c.name.clone()];
                    let short = c.id.get(..12).unwrap_or(&c.id).to_string();
                    let hostname = request.hostname.clone().unwrap_or_else(|| short.clone());
                    for name in asked
                        .map(|e| e.aliases.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .chain([short, hostname])
                    {
                        if !dns_names.contains(&name) {
                            dns_names.push(name);
                        }
                    }
                    super::inspect_doc::UserNet {
                        name: n.name,
                        network_id: n.id,
                        endpoint: String::new(),
                        ip: None,
                        gateway: None,
                        prefix: 0,
                        mac: String::new(),
                        dns_names,
                    }
                }),
        };
        document(&Facts {
            container: c,
            request: &request,
            image: config.as_ref(),
            manifest: manifest.as_ref(),
            paused,
            pid,
            health,
            exec_ids,
            log_path: dir.join("log").display().to_string(),
            net,
            user_net,
            size: sized.then(|| self.container_sizes(c)),
        })
    }

    /// Container `c`'s SizeRw and SizeRootFs, as containerd's snapshots count them for
    /// dockerd: the disk its writable layer uses, from its guest while it runs, else as
    /// its last run left it; and that with its image's root filesystem.
    pub(super) fn container_sizes(&self, c: &crate::containers::Container) -> (i64, i64) {
        let running =
            matches!(super::lock(&self.runs).get(&c.id), Some(super::RunState::Tracked(t)) if !t.visit);
        let rw = if running {
            let spec = shards_abi::run::Spec {
                builtin: shards_abi::run::builtin::SIZE,
                ..Default::default()
            };
            self.exec_quietly(&c.id, &spec, None, 64, super::TAKE_TIMEOUT)
                .ok()
                .filter(|q| q.status == Some(0))
                .and_then(|q| String::from_utf8_lossy(&q.output).trim().parse::<u64>().ok())
        } else {
            // As its last run left it, once its VM has saved its layer and said so.
            self.await_settled(&c.id);
            super::lock(&self.containers).get(&c.id).and_then(|c| c.size_rw)
        }
        .unwrap_or(0);
        let image = c
            .image_id
            .as_deref()
            .and_then(|id| {
                let store = self.store().ok().flatten()?;
                let found = store
                    .images()
                    .ok()?
                    .into_iter()
                    .find(|i| i.id.to_string() == id)?;
                Some(found.unpacked)
            })
            .unwrap_or(0);
        let rw = i64::try_from(rw).unwrap_or(i64::MAX);
        (rw, rw.saturating_add(i64::try_from(image).unwrap_or(i64::MAX)))
    }

    /// Image `id`'s config (its `config`), and the descriptor of its manifest for this
    /// platform, as Go encodes one: from its index where it has one.
    fn image_facts(&self, id: Option<&str>) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
        let Some(id) = id else {
            return (None, None);
        };
        let Ok(Some(store)) = self.store() else {
            return (None, None);
        };
        let Some(image) = store
            .images()
            .ok()
            .and_then(|l| l.into_iter().find(|i| i.id.to_string() == id))
        else {
            return (None, None);
        };
        let config = image
            .config
            .as_deref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .and_then(|c| c.get("config").cloned());
        let ours = image.manifest.to_string();
        let manifest = if image.target.digest == ours {
            serde_json::to_value(&image.target).ok()
        } else {
            std::fs::read(store.blob_path(&image.id))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|index| {
                    index
                        .get("manifests")?
                        .as_array()?
                        .iter()
                        .find(|m| m.get("digest").and_then(serde_json::Value::as_str) == Some(&ours))
                        .cloned()
                })
        };
        (config, manifest)
    }
}

/// The object types `inspect --type` names (docker/cli system/inspect.go, allTypes).
const TYPES: [&str; 10] = [
    "config",
    "container",
    "image",
    "network",
    "node",
    "plugin",
    "secret",
    "service",
    "task",
    "volume",
];

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards inspect NAME...` (docker/cli system/inspect.go): each name as the first
    /// kind of object it names, in inspectAll's order (a microVM, an image, a network,
    /// ...), or as `--type` says; then printed as inspect.Inspect prints.
    pub(super) fn inspect_any(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let styled = asker.styled();
        let kind = parsed.string("type");
        let all = TYPES.map(|t| format!("\"{t}\"")).join(", ");
        if parsed.changed("type") && kind.is_empty() {
            reply.err(&format!("type is empty: must be one of {all}"));
            return 1;
        }
        if !kind.is_empty() && !TYPES.contains(&kind) {
            reply.err(&format!("unknown type: {}: must be one of {all}", go_quote(kind)));
            return 1;
        }
        let any = kind.is_empty();
        let (mut documents, mut errors) = (Vec::new(), Vec::new());
        for given in &parsed.args {
            let found = (|| {
                if any || kind == "container" {
                    let c = self.resolve(given).and_then(|id| {
                        super::lock(&self.containers)
                            .get(&id)
                            .cloned()
                            .ok_or_else(|| format!("No such container: {given}"))
                    });
                    match c {
                        Ok(c) => return Ok(self.container_value(&c, parsed.bool("size"))),
                        Err(e) if !any => return Err(daemon_said(&e)),
                        Err(_) => {}
                    }
                }
                if any || kind == "image" {
                    match self.image_doc(given) {
                        Ok(d) => return Ok(d),
                        Err(e) if !any => return Err(daemon_said(&e)),
                        Err(_) => {}
                    }
                }
                if (any || kind == "volume")
                    && let Some(d) = self.volume_doc(given, i64::from(asker.utc_offset))
                {
                    return Ok(d);
                }
                // shards' networks have no documents yet: said so, not that they are not.
                if (any || kind == "network") && matches!(given.as_str(), "bridge" | "none") {
                    return Err(format!(
                        "shards: network {given}: network documents are not served yet"
                    ));
                }
                match kind {
                    "network" => Err(format!("Error response from daemon: network {given} not found")),
                    "volume" => Err(format!("Error response from daemon: get {given}: no such volume")),
                    "plugin" => Err(format!("Error response from daemon: plugin {} not found", go_quote(given))),
                    "node" | "service" | "task" | "secret" | "config" => Err(
                        "Error response from daemon: This node is not a swarm manager: shards has no swarm mode"
                            .into(),
                    ),
                    _ => Err(format!("error: no such object: {given}")),
                }
            })();
            match found {
                Ok(d) => documents.push(d),
                Err(e) => errors.push(e),
            }
        }
        inspected(parsed.string("format"), &documents, errors, styled, reply)
    }

    /// Image `given`'s InspectResponse, or why not.
    fn image_doc(&self, given: &str) -> Result<shards_template::Value, String> {
        let store = self.store()?.ok_or_else(|| super::images::not_found(given))?;
        let images = store.named().map_err(|e| e.to_string())?;
        let named = super::images::resolve(&images, given)?;
        let image = store.image(named).map_err(|e| e.to_string())?;
        Ok(super::inspect_doc::image_value(&document(
            &image,
            record_name(given).as_deref(),
        )))
    }
}

/// `e` as the CLI says what dockerd answered.
fn daemon_said(e: &str) -> String {
    if e.starts_with("Error response") {
        e.to_owned()
    } else {
        format!("Error response from daemon: {e}")
    }
}

/// `s` as Go's %q writes it.
fn go_quote(s: &str) -> String {
    let mut out = String::new();
    json_string_into(s, &mut out);
    out
}

fn json_string_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// What docker/cli's inspect.Inspect writes of `documents`, then `errors`, `format`
/// being `--format`: the documents as an indented JSON array, or compact (`json`), or
/// each through the template (on its Go types, and failing that on its JSON with
/// missingkey=error, as TemplateInspector does), a line each; a template that does not
/// parse exits 64, and anything not found 1.
pub(super) fn inspected(
    format: &str,
    documents: &[shards_template::Value],
    errors: Vec<String>,
    styled: bool,
    reply: &super::commands::Reply<'_>,
) -> u8 {
    let mut errors = errors;
    let json = |v: &shards_template::Value| super::inspect_doc::json(v);
    let text = match format {
        "" => {
            let compact: Vec<String> = documents.iter().map(json).collect();
            if compact.is_empty() {
                "[]".to_string()
            } else {
                indent(&format!("[{}]", compact.join(",")), "    ")
            }
        }
        "json" => {
            let compact: Vec<String> = documents.iter().map(json).collect();
            format!("[{}]", compact.join(","))
        }
        template => {
            let t = match shards_template::Template::parse("", template) {
                Ok(t) => t,
                Err(e) => {
                    reply.err(&format!("template parsing error: {e}"));
                    return 64;
                }
            };
            let mut out = String::new();
            for d in documents {
                match t.execute(d) {
                    Ok(o) => {
                        out.push_str(&o);
                        out.push('\n');
                    }
                    Err(_) => {
                        // tryRawInspectFallback: its JSON, numbers as json.Number.
                        let raw = serde_json::from_str::<serde_json::Value>(&json(d))
                            .map(|r| raw_value(&r))
                            .unwrap_or(shards_template::Value::Nil);
                        match t.missing_key_error().execute(&raw) {
                            Ok(o) => {
                                out.push_str(&o);
                                out.push('\n');
                            }
                            Err(e) => errors.push(format!("template parsing error: {e}")),
                        }
                    }
                }
            }
            // Flush: nothing written is a line of its own.
            if out.is_empty() {
                out.push('\n');
            }
            out.pop();
            out
        }
    };
    if styled && format.is_empty() {
        let mut sheet = shards_ipc::Sheet::new("json");
        sheet.record(&[("text", text)]);
        reply.sheet(&sheet);
    } else {
        reply.out(&text);
    }
    if errors.is_empty() {
        return 0;
    }
    reply.err(&errors.join("\n"));
    1
}

/// A JSON value as json.Decoder with UseNumber decodes it into `any`: numbers as
/// json.Number, which prints as the number's text.
/// encoding/json's Number, which the raw document's numbers decode to (UseNumber): a
/// string to fmt, its digits as they are to encoding/json.
#[derive(Debug)]
struct JsonNumber(String);

impl shards_template::Object for JsonNumber {
    fn type_name(&self) -> &str {
        "json.Number"
    }

    fn format(&self, out: &mut String) {
        out.push_str(&self.0);
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push_str(&self.0);
        Ok(())
    }
}

fn raw_value(v: &serde_json::Value) -> shards_template::Value {
    use serde_json::Value as J;
    use shards_template::Value;
    match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::object(JsonNumber(n.to_string())),
        J::String(s) => Value::String(s.clone()),
        J::Array(l) => Value::list(l.iter().map(raw_value).collect()),
        J::Object(m) => Value::map(m.iter().map(|(k, v)| (k.clone(), raw_value(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As encoding/json's Indent lays out what it is given, by Go's own answers
    /// (docker-images.json, `indents`), strings' punctuation untouched.
    #[test]
    fn json_indents_as_go_indents_it() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("docker-images.json")).unwrap();
        for case in golden["indents"].as_array().unwrap() {
            assert_eq!(
                indent(case["in"].as_str().unwrap(), "    "),
                case["out"].as_str().unwrap(),
                "{case}"
            );
        }
        assert_eq!(indent("[]", "    "), "[]");
        assert_eq!(
            indent(r#"{"a":[],"b":{}}"#, "  "),
            "{\n  \"a\": [],\n  \"b\": {}\n}"
        );
        assert_eq!(
            indent(r#"[{"Id":"x","L":[1,2],"S":"a,b:{c}[\"d\"]"}]"#, "    "),
            "[\n    {\n        \"Id\": \"x\",\n        \"L\": [\n            1,\n            2\n        ],\n        \"S\": \"a,b:{c}[\\\"d\\\"]\"\n    }\n]"
        );
    }
}
