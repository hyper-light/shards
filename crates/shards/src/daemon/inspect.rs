//! `shards image inspect` as dockerd's containerd store answers `docker image inspect`
//! (moby docker-v29.3.1 daemon/containerd/image_inspect.go; api/types/image
//! InspectResponse) and docker/cli prints it (cli/command/inspect/inspector.go,
//! IndentedInspector): each image's document in the API's field order, compact as Go
//! encodes it, then all of them as one array indented by encoding/json's Indent.

use std::time::SystemTime;

use shards_agentfile::image::{Image as Config, json_string};
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
    let mut time = shards_agentfile::go::Time::from_unix(i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
    time.nanosecond = since.subsec_nanos();
    time.rfc3339_nano()
        .map_or_else(|_| "\"1970-01-01T00:00:00Z\"".into(), |s| format!("\"{s}\""))
}

/// `image`'s InspectResponse, compact: fields in the struct's order, those Go leaves out
/// when empty left out. Fails as moby's ImageInspect fails where Go reads its config into
/// no DockerOCIImage.
pub(super) fn document(image: &Image, record: Option<&str>) -> Result<String, String> {
    let (tags, digests) = tags_and_digests(image);
    let config = match image.config.as_deref() {
        Some(b) => Some(Config::from_json(b).map_err(|e| {
            format!(
                "the image's config does not read: {}",
                String::from_utf8_lossy(&e)
            )
        })?),
        None => None,
    };
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
    // `Created.Format(time.RFC3339Nano)`, which fails at no offset.
    if let Some(created) = config
        .as_ref()
        .and_then(|c| c.created.as_ref())
        .map(|t| t.format_rfc3339_nano())
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
    Ok(format!("{{{}}}", o.join(",")))
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
                    reply.err(&format!("shards: {e}"));
                    return 1;
                }
            },
            Ok(None) => (None, Vec::new()),
            Err(e) => {
                reply.err(&format!("shards: {e}"));
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
            match read.and_then(|image| document(&image, record_name(given).as_deref())) {
                Ok(d) => documents.push(super::inspect_doc::image_value(&d)),
                Err(e) => errors.push(format!("shards: {e}")),
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
                return refuse(&format!("shards: {}", super::images::not_found(given)));
            }
            Err(e) => return refuse(&format!("shards: {e}")),
        };
        let lease = store.lease();
        let images = match lease
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|_| store.named().map_err(|e| e.to_string()))
        {
            Ok(images) => images,
            Err(e) => return refuse(&format!("shards: {e}")),
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
                    return refuse(&format!("shards: No such image: {}:latest", repo.familiar()));
                }
                for (name, image) in tagged {
                    asked.push((image, Some(name)));
                }
                continue;
            }
            let image = match resolved {
                Ok(image) => image,
                Err(e) => return refuse(&format!("shards: {e}")),
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
                    Err(e) => return refuse(&format!("shards: {e}")),
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
            Err(e) => refuse(&format!("shards: {e}")),
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
        convert: bool,
    ) -> u8 {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        for given in &parsed.args {
            // shards' grammar (`inspect vm IMAGE`): a name of no microVM that names an
            // image, made one, says the microVM it makes.
            if convert
                && self.resolve(given).is_err()
                && let Ok(doc) = self.image_doc(given)
            {
                documents.push(doc);
                continue;
            }
            let found = self.resolve(given).and_then(|id| {
                super::lock(&self.containers)
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| format!("No such container: {given}"))
            });
            match found {
                Ok(c) => documents.push(self.container_value(&c, parsed.bool("size"))),
                Err(e) if e.starts_with("Error response") => errors.push(e),
                Err(e) => errors.push(format!("shards: {e}")),
            }
        }
        inspected(parsed.string("format"), &documents, errors, styled, reply)
    }

    /// Container `c`'s InspectResponse, from its record, its request, its image and its
    /// VM (inspect_doc.rs).
    /// What container `id` is, as `inspect` says it, for its log's tag and attributes
    /// (D161, D163): its environment, its labels and its command as its image and its run
    /// made them, and this host's name.
    pub(super) fn log_details(&self, id: &str) -> Result<crate::logging::Details, String> {
        // Its record written yet or not: a run's shipper asks as it starts.
        let c = super::lock(&self.containers)
            .made(id)
            .cloned()
            .ok_or_else(|| format!("No such container: {id}"))?;
        let doc: serde_json::Value =
            serde_json::from_str(&super::inspect_doc::json(&self.container_value(&c, false)))
                .map_err(|e| e.to_string())?;
        let at = |path: &str| doc.pointer(path).cloned().unwrap_or_default();
        let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        let strings = |path: &str| -> Vec<String> {
            at(path)
                .as_array()
                .map(|a| a.iter().map(text).collect())
                .unwrap_or_default()
        };
        Ok(crate::logging::Details {
            id: text(&at("/Id")),
            name: text(&at("/Name")),
            image_id: text(&at("/Image")),
            image_name: text(&at("/Config/Image")),
            entrypoint: text(&at("/Path")),
            args: strings("/Args"),
            env: strings("/Config/Env"),
            labels: at("/Config/Labels")
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), text(v))).collect())
                .unwrap_or_default(),
            hostname: super::info::host_name().unwrap_or_default(),
            created: text(&at("/Created")),
        })
    }

    /// The attributes `logs --details` shows before each of container `id`'s lines, as its
    /// run's `--log-opt`s ask (D161): written as dockerd writes them (its log stream's
    /// attrsByteSlice: sorted, each `NAME=VALUE` query-escaped, joined by commas).
    pub(super) fn log_attributes(&self, id: &str, run: &shards_ipc::Run) -> Result<Vec<u8>, String> {
        let details = self.log_details(id)?;
        let attrs = crate::logging::attributes(run, &details.info())?;
        Ok(attrs
            .iter()
            .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
            .collect::<Vec<_>>()
            .join(",")
            .into_bytes())
    }

    /// The bytes of the /dev/shm container `c`'s command sees (D156), as `inspect` says
    /// them: none where it has none (`--ipc none`); where it shares another's (`--ipc
    /// container:NAME`), that one's; its own `--shm-size`; else its microVM's, half all the
    /// memory that microVM may have (`run::shm_bytes`), its own or that of the microVM it
    /// lives in: the one it joins (`--network container:NAME`), or whose own engine made it.
    /// Each followed 64 steps at most, as a run names only those made before it. `request`
    /// is `c`'s as it was asked for.
    fn shm_size(&self, c: &crate::containers::Container, request: &shards_ipc::Run) -> i64 {
        let read = |reference: &str| -> Option<(String, shards_ipc::Run)> {
            let id = self.resolve(reference).ok()?;
            let dir = super::lock(&self.containers).dir(&id);
            let run = std::fs::read(dir.join(super::REQUEST))
                .ok()
                .and_then(|b| shards_ipc::Run::decode(&b))?;
            Some((id, run))
        };
        let (mut id, mut run) = (c.id.clone(), request.clone());
        // Whose /dev/shm it is.
        for _ in 0..64 {
            if run.ipc == "none" {
                return 0;
            }
            match run.ipc.strip_prefix("container:").and_then(read) {
                Some(next) => (id, run) = next,
                None => break,
            }
        }
        if run.shm_size != 0 {
            return run.shm_size;
        }
        // The microVM it is in.
        for _ in 0..64 {
            let engine_of = super::lock(&self.containers)
                .get(&id)
                .and_then(|c| c.engine_made.as_ref().map(|m| m.of.clone()));
            let next = match engine_of {
                Some(of) => read(&of),
                None => run.network.strip_prefix("container:").and_then(read),
            };
            match next {
                Some(next) => (id, run) = next,
                None => break,
            }
        }
        let (_, _, most) = crate::run::sizes(&run.resources, crate::build::host_memory());
        i64::try_from(crate::run::shm_bytes(most)).unwrap_or(i64::MAX)
    }

    pub(super) fn container_value(
        &self,
        c: &crate::containers::Container,
        sized: bool,
    ) -> shards_template::Value {
        use super::inspect_doc::{Facts, Health, Net, document};
        let dir = super::lock(&self.containers).dir(&c.id);
        let mut request = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .unwrap_or_else(|| shards_ipc::Run {
                image: c.image.clone(),
                ..shards_ipc::Run::default()
            });
        let shm = self.shm_size(c, &request);
        // One its microVM's engine made is on that engine's network, or on the microVM's
        // own, its engine's host's (D181), as its engine says it.
        if let Some(m) = &c.engine_made {
            if !request.engine_net.is_empty() {
                request.network.clone_from(&request.engine_net);
            } else if request.network == format!("container:{}", m.of) {
                request.network = "host".into();
            }
        }
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
        let net = match c.engine_made.as_ref().and_then(|m| m.addresses.first()) {
            Some(a) => Some(Net {
                ip: a.ip,
                gateway: a.gateway,
                prefix: a.prefix,
                mac: Some(a.mac),
            }),
            None => self.bridge.as_ref().map(|b| Net {
                ip: b.guest(),
                gateway: b.gateway(),
                prefix: b.subnet().1,
                mac,
            }),
        };
        let paused = super::lock(&self.paused).contains(&c.id);
        // A user network's endpoint (D46): its own while it runs; its network's ID and
        // the names it would have, kept while it is stopped, as dockerd keeps them.
        // None of the host's, for one its microVM's engine made, whose networks are that
        // engine's (D181).
        let user_net = match self.membership(&c.id).filter(|_| c.engine_made.is_none()) {
            Some((n, m)) => Some(super::inspect_doc::UserNet {
                name: n.name.clone(),
                network_id: n.id.clone(),
                endpoint: m.endpoint,
                ip: Some(m.ip),
                gateway: n.pools.first().map(|p| p.gateway),
                prefix: n.pools.first().map_or(0, |p| p.subnet.1),
                ip6: m.ip6,
                gateway6: n.pools6.first().map(|p| p.gateway.clone()).unwrap_or_default(),
                prefix6: n
                    .pools6
                    .first()
                    .and_then(|p| crate::networks::prefix6(&p.subnet))
                    .map_or(0, |(_, bits)| bits),
                mac: m.mac,
                dns_names: m.dns_names,
                engine: None,
            }),
            None if c.engine_made.is_some() => None,
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
                        ip6: None,
                        gateway6: String::new(),
                        prefix6: 0,
                        mac: String::new(),
                        dns_names,
                        engine: None,
                    }
                }),
        };
        let mut user_nets: Vec<super::inspect_doc::UserNet> = user_net.into_iter().collect();
        // One its microVM's engine made: each network of that engine's it is on (D181), its
        // address there while it runs; no name on the default network, which names no peers.
        if let Some(m) = &c.engine_made {
            let networks = super::lock(&self.containers)
                .get(&m.of)
                .map(|p| p.engine_networks.clone())
                .unwrap_or_default();
            for entry in &request.engine_nets {
                let mut parts = entry.split('\0');
                let network = parts.next().unwrap_or_default().to_string();
                let asked_ip = parts.next().unwrap_or_default().to_string();
                let aliases: Vec<String> = parts.map(str::to_string).collect();
                let at = m.addresses.iter().find(|a| a.network == network);
                let network_id = networks
                    .iter()
                    .find(|n| n.name == network)
                    .map_or_else(|| super::engine::own_id(&m.of, &network), |n| n.id.clone());
                let mut dns_names = Vec::new();
                if network != "bridge" {
                    for name in std::iter::once(&m.name).chain(&aliases) {
                        if !dns_names.contains(name) {
                            dns_names.push(name.clone());
                        }
                    }
                }
                user_nets.push(super::inspect_doc::UserNet {
                    name: network.clone(),
                    network_id,
                    endpoint: at
                        .map(|_| super::engine::own_id(&c.id, &network))
                        .unwrap_or_default(),
                    ip: at.map(|a| a.ip),
                    gateway: at.map(|a| a.gateway),
                    prefix: at.map_or(0, |a| a.prefix),
                    ip6: None,
                    gateway6: String::new(),
                    prefix6: 0,
                    mac: at
                        .map(|a| shards_cmdline::network::mac_string(&a.mac))
                        .unwrap_or_default(),
                    dns_names,
                    engine: Some((aliases, asked_ip)),
                });
            }
        }
        document(&Facts {
            container: c,
            request: &request,
            image: config.as_ref(),
            manifest: manifest.as_ref(),
            paused,
            pid,
            health,
            exec_ids,
            // shards' store is its own (D161), no file a reader of Docker's could read, as
            // dockerd names none of its local driver's.
            log_path: String::new(),
            net,
            user_nets,
            size: sized.then(|| self.container_sizes(c)),
            shm,
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
        // Its config as Docker reads it, a DockerOCIImageConfig, written as json.Marshal
        // writes one: each field once, by its own name.
        let config = image
            .config
            .as_deref()
            .and_then(|b| Config::from_json(b).ok())
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c.config.to_json()).ok());
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
                        // docker/cli tries the next kind only past what is not found.
                        Err(Missing::Not(e)) if !any => return Err(daemon_said(&e)),
                        Err(Missing::Not(_)) => {}
                        Err(Missing::Failed(e)) => return Err(daemon_said(&e)),
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
                    "network" => Err(format!("shards: no network is named {given}")),
                    "volume" => Err(format!("shards: no volume is named {given}")),
                    "plugin" => Err(format!("shards: shards has no plugins: {}", go_quote(given))),
                    "node" | "service" | "task" | "secret" | "config" => {
                        Err("shards: shards has no swarm mode".into())
                    }
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
    pub(super) fn image_doc(&self, given: &str) -> Result<shards_template::Value, Missing> {
        let store = self
            .store()
            .map_err(Missing::Not)?
            .ok_or_else(|| Missing::Not(super::images::not_found(given)))?;
        let images = store.named().map_err(|e| Missing::Not(e.to_string()))?;
        let named = super::images::resolve(&images, given).map_err(Missing::Not)?;
        let image = store.image(named).map_err(|e| Missing::Not(e.to_string()))?;
        let doc = document(&image, record_name(given).as_deref()).map_err(Missing::Failed)?;
        Ok(super::inspect_doc::image_value(&doc))
    }
}

/// Why an image has no document: not found, which `inspect` without a `--type` passes
/// over to the next kind of object, or found and failed, which it says.
pub(super) enum Missing {
    Not(String),
    Failed(String),
}

/// `e` as the CLI says what dockerd answered.
fn daemon_said(e: &str) -> String {
    if e.starts_with("Error response") {
        e.to_owned()
    } else {
        format!("shards: {e}")
    }
}

/// `s` as Go's %q writes it.
fn go_quote(s: &str) -> String {
    shards_cmdline::go::quote(s)
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

/// Go's url.QueryEscape: what is not a letter, a digit or one of `-_.~` as `%XX`, and a
/// space as `+`.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
