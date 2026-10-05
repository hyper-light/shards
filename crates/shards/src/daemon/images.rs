//! `shards images` as the Docker CLI prints `docker images` (docker/cli v29.8.1
//! cli/command/image/list.go, tree.go; formatter/image.go): the tree view by default,
//! folded, or expanded with `--tree`, laid out for the client's terminal, coloured unless
//! it is not one or `NO_COLOR` is set; and the table with `-q`, `--no-trunc` or
//! `--digests`. Held to docker/cli's own output by docker-images.json.
//!
//! Unlike docker/cli: where it walks a Go map (an image tagged in several repositories,
//! in the table), rows come in the order the image names them, where Go's are in random
//! order; and the tree's images of one name keep their order, where Go's unstable sort
//! may swap them past 12 images.

use shards_cmdline::width;

/// A manifest of an image, as dockerd's containerd store lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Manifest {
    pub id: String,
    pub kind: Kind,
    /// `os/arch[/variant]`, as containerd's platforms.Format writes it.
    pub platform: String,
    pub available: bool,
    pub content: i64,
    pub total: i64,
    pub in_use: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Image,
    Attestation,
}

/// An image as dockerd lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Summary {
    pub id: String,
    pub tags: Vec<String>,
    pub digests: Vec<String>,
    /// Seconds since the epoch.
    pub created: i64,
    pub size: i64,
    pub containers: i64,
    pub manifests: Vec<Manifest>,
}

/// Where the listing goes: a terminal of `width` columns, or not one (`width` 0); its
/// colours, unless `NO_COLOR`; and its locale's widths.
#[derive(Clone, Copy, Debug)]
pub(super) struct Out {
    pub terminal: bool,
    pub width: u16,
    pub color: bool,
    pub east_asian: bool,
}

/// An SGR sequence, as morikuni/aec applies one: its codes, the text, then a reset; or,
/// with no colour, the text alone.
#[derive(Clone, Copy)]
struct Sgr(&'static str);

const NONE: Sgr = Sgr("");
const RESET: &str = "\x1b[0m";

impl Sgr {
    fn apply(self, s: &str) -> String {
        if self.0.is_empty() {
            return s.to_string();
        }
        format!("{}{s}{RESET}", self.0)
    }
}

/// go-units' HumanSizeWithPrecision(size, 3): decimal units, three significant digits as
/// Go's `%.3g` writes them.
pub(super) fn human_size(size: i64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = size as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    format!("{}{}", go_g(size, 3), UNITS.get(unit).unwrap_or(&""))
}

/// `v` as Go's `%.{precision}g`: the shorter of `%e` and `%f` for its exponent, at that
/// many significant digits, trailing zeros dropped.
fn go_g(v: f64, precision: usize) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let e = format!("{:.*e}", precision.saturating_sub(1), v);
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim = |s: &str| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if exp < -4 || exp >= i32::try_from(precision).unwrap_or(i32::MAX) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trim(mantissa), exp.unsigned_abs());
    }
    let decimals = usize::try_from(i32::try_from(precision).unwrap_or(0) - 1 - exp).unwrap_or(0);
    trim(&format!("{v:.decimals$}"))
}

/// stringid.TruncateID: what follows the algorithm, its first 12 characters.
pub(super) fn truncate_id(id: &str) -> &str {
    let id = id.split_once(':').map_or(id, |(_, hex)| hex);
    id.get(..12).unwrap_or(id)
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards images [REPOSITORY[:TAG]]` (list.go, runImages): the store's images, newest
    /// first, those `REPOSITORY[:TAG]` names alone if given, in the tree view unless a
    /// flag asks for the table.
    pub(super) fn images(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let (quiet, no_trunc, digests, expanded) = (
            parsed.bool("quiet"),
            parsed.bool("no-trunc"),
            parsed.bool("digests"),
            parsed.bool("tree"),
        );
        // shouldUseTree: the table for what the tree cannot show yet.
        for (asked, flag) in [
            (quiet, "--quiet"),
            (no_trunc, "--no-trunc"),
            (digests, "--show-digest"),
        ] {
            if asked && expanded {
                reply.err(&format!("{flag} is not yet supported with --tree"));
                return 1;
            }
        }
        let images = match self.listed(parsed.args.first().map(String::as_str), parsed.bool("all")) {
            Ok(images) => images,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        // A colour terminal gets shards' page: the images as records.
        if asker.styled() && !quiet && !no_trunc && !digests && !expanded {
            reply.sheet(&self.sheet(&images));
            return 0;
        }
        let text = if quiet || no_trunc || digests {
            let now = asker.now / 1_000_000_000;
            table(&images, now, !no_trunc, quiet, digests)
                .iter()
                .map(|l| format!("{l}\n"))
                .collect()
        } else {
            tree(
                &images,
                expanded,
                Out {
                    terminal: asker.terminal,
                    width: asker.width,
                    color: asker.color,
                    east_asian: asker.east_asian,
                },
            )
        };
        let _ = reply.bytes(crate::spec::LOG_STDOUT, text.as_bytes());
        0
    }

    /// The images as records for a client's page: a head of totals, then one an image,
    /// with what its manifest for our platform holds: its disk, its platform, its layers.
    fn sheet(&self, images: &[Summary]) -> shards_ipc::Sheet {
        let mut sheet = shards_ipc::Sheet::new("images");
        let store = self.store().ok().flatten();
        let stored: Vec<shards_image::store::Image> =
            store.as_ref().and_then(|s| s.images().ok()).unwrap_or_default();
        let (mut content, mut disks) = (0u64, 0u64);
        let mut rows = Vec::new();
        for image in images {
            let found = stored.iter().find(|s| s.id.to_string() == image.id);
            let (packed, disk) = found.map_or((0, 0), |s| (s.content, s.unpacked));
            content = content.saturating_add(packed);
            disks = disks.saturating_add(disk);
            let platform = image
                .manifests
                .iter()
                .find(|m| m.kind == Kind::Image && m.available)
                .map(|m| m.platform.clone())
                .unwrap_or_default();
            let layers = found
                .and_then(|s| s.config.as_ref())
                .and_then(|c| serde_json::from_slice::<shards_image::oci::ImageConfig>(c).ok())
                .map(|c| c.rootfs.diff_ids.len())
                .unwrap_or(0);
            let names: Vec<(String, String)> = if image.tags.is_empty() {
                vec![("<none>".into(), "<none>".into())]
            } else {
                image
                    .tags
                    .iter()
                    .map(
                        |t| match t.rsplit_once(':').filter(|(_, tag)| !tag.contains('/')) {
                            Some((repo, tag)) => (repo.to_string(), tag.to_string()),
                            None => (t.clone(), "<none>".into()),
                        },
                    )
                    .collect()
            };
            // Its microVMs, running and stopped, as `list vm` counts them.
            let (running, stopped) = super::lock(&self.containers)
                .all()
                .filter(|c| c.image_id.as_deref() == Some(image.id.as_str()))
                .fold((0u64, 0u64), |(r, s), c| {
                    if c.state == crate::containers::State::Running {
                        (r + 1, s)
                    } else {
                        (r, s + 1)
                    }
                });
            for (repo, tag) in names {
                rows.push(vec![
                    ("repo", repo),
                    ("tag", tag),
                    ("id", truncate_id(&image.id).to_string()),
                    ("created", image.created.to_string()),
                    ("content", packed.to_string()),
                    ("disk", disk.to_string()),
                    ("platform", platform.clone()),
                    ("layers", layers.to_string()),
                    ("running", running.to_string()),
                    ("stopped", stopped.to_string()),
                ]);
            }
        }
        let free = shards_vmm::platform::disk_space(&self.home)
            .map(|(available, _)| available.to_string())
            .unwrap_or_default();
        sheet.record(&[
            ("kind", "head".into()),
            ("images", images.len().to_string()),
            ("content", content.to_string()),
            ("disk", disks.to_string()),
            ("store", self.home.join("images").display().to_string()),
            ("free", free),
        ]);
        for row in rows {
            sheet.record(&row);
        }
        sheet
    }

    /// The store's images as dockerd lists them, those with a reference `pattern` matches
    /// alone, named by those references alone; dangling ones, kept for a container with
    /// no name of their own, only with `all`.
    fn listed(&self, pattern: Option<&str>, all: bool) -> Result<Vec<Summary>, String> {
        use shards_image::reference::Reference;
        let Some(store) = self.store()? else {
            return Ok(Vec::new());
        };
        let stored = store.images().map_err(|e| e.to_string())?;
        // Each container's image, counted by ID.
        let mut users: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for c in super::lock(&self.containers).all() {
            if let Some(id) = &c.image_id {
                *users.entry(id.clone()).or_default() += 1;
            }
        }
        let mut images = Vec::with_capacity(stored.len());
        for img in stored {
            let (mut tags, mut digests) = (Vec::new(), Vec::new());
            for name in &img.references {
                // A dangling image's name is not one of its names.
                if name.starts_with(super::rmi::DANGLING) {
                    continue;
                }
                let Ok(r) = Reference::parse_normalized(name) else {
                    continue;
                };
                if let Some(p) = pattern
                    && !matches(p, &r)
                {
                    continue;
                }
                // Every name is one of its RepoTags, by digest too, as dockerd lists them
                // (tagsByDigest); the table shows only those with a tag.
                tags.push(r.familiar());
                let mut bare = r;
                bare.tag = None;
                bare.digest = None;
                let digested = format!("{}@{}", bare.familiar(), img.id);
                if !digests.contains(&digested) {
                    digests.push(digested);
                }
            }
            if (pattern.is_some() || !all) && digests.is_empty() {
                continue;
            }
            let id = img.id.to_string();
            let containers = users.get(&id).copied().unwrap_or(0);
            let created = img
                .created
                .as_deref()
                .and_then(|c| shards_dockerfile::go::parse_rfc3339(c.as_bytes()).ok())
                .map_or(0, |t| t.unix().0);
            images.push(Summary {
                manifests: manifests(&img, containers),
                id,
                tags,
                digests,
                created,
                size: size(img.content.saturating_add(img.unpacked)),
                containers,
            });
        }
        // Newest first, as dockerd lists them (byCreated, reversed).
        images.sort_by_key(|i| std::cmp::Reverse(i.created));
        Ok(images)
    }
}

/// Bytes as the API counts them.
fn size(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// An image's manifests as dockerd lists them: one is in use while a container runs it,
/// as a container's ImageManifest names it (moby image_list.go), and a run here uses the
/// image's manifest for our platform.
fn manifests(img: &shards_image::store::Image, containers: i64) -> Vec<Manifest> {
    img.manifests
        .iter()
        .map(|m| Manifest {
            id: m.digest.to_string(),
            kind: if m.attestation {
                Kind::Attestation
            } else {
                Kind::Image
            },
            platform: m.platform.clone().unwrap_or_default(),
            available: m.available,
            content: size(m.content),
            total: size(m.content.saturating_add(m.unpacked)),
            in_use: containers > 0 && m.digest == img.manifest,
        })
        .collect()
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// The image store, if this home has one.
    pub(super) fn store(&self) -> Result<Option<shards_image::store::Store>, String> {
        let root = self.home.join("images");
        if !root.is_dir() {
            return Ok(None);
        }
        shards_image::store::Store::open(&root)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// `shards tag SOURCE TARGET` (moby client ImageTag, then the daemon's postImagesTag):
    /// TARGET, with `latest` if it names no tag, names what SOURCE does.
    pub(super) fn tag(&self, args: &[String], styled: bool, reply: &super::commands::Reply<'_>) -> u8 {
        use shards_image::reference::{AnyReference, Reference};
        let (Some(source), Some(target)) = (args.first(), args.get(1)) else {
            return 1;
        };
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let invalid = |given: &str, e: &dyn std::fmt::Display| {
            format!(
                "error parsing reference: {} is not a valid repository/tag: {e}",
                shards_cmdline::go::quote(given)
            )
        };
        if let Err(e) = AnyReference::parse(source) {
            return refuse(&invalid(source, &e));
        }
        let tagged = match Reference::parse_normalized(target) {
            Ok(r) => r,
            Err(e) => return refuse(&invalid(target, &e)),
        };
        if tagged.digest.is_some() {
            return refuse("refusing to create a tag with a digest reference");
        }
        let tagged = tagged.tag_name_only();
        let mut bare = tagged.clone();
        bare.tag = None;
        if bare.familiar() == "sha256" {
            return refuse(
                "Error response from daemon: refusing to create an ambiguous tag using digest algorithm as name",
            );
        }
        let found = self.store().and_then(|store| {
            let store = store.ok_or_else(|| not_found(source))?;
            let images = store.named().map_err(|e| e.to_string())?;
            let image = resolve(&images, source)?;
            let existing = image.references.first().ok_or_else(|| not_found(source))?;
            store
                .alias(&tagged.to_string(), existing)
                .map_err(|e| e.to_string())?;
            Ok(image.id.to_string())
        });
        match found {
            Ok(id) => {
                if styled {
                    let mut sheet = shards_ipc::Sheet::new("tag");
                    sheet.record(&[
                        ("source", source.clone()),
                        ("target", tagged.familiar()),
                        ("id", truncate_id(&id).to_string()),
                    ]);
                    reply.sheet(&sheet);
                }
                0
            }
            Err(e) => refuse(&format!("Error response from daemon: {e}")),
        }
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards rmi IMAGE...` (docker/cli remove.go, runRemove): each image removed as
    /// dockerd removes it, its lines as it goes, the errors after; with `-f`, those of
    /// images not found forgiven.
    pub(super) fn rmi(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        styled: bool,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        // For a client on a colour terminal: what each name did, and what deleting frees.
        let mut sheet = shards_ipc::Sheet::new("rmi");
        let sizes: Vec<(String, u64)> = if styled {
            self.store()
                .ok()
                .flatten()
                .and_then(|s| s.images().ok())
                .unwrap_or_default()
                .into_iter()
                .map(|i| (i.id.to_string(), i.content.saturating_add(i.unpacked)))
                .collect()
        } else {
            Vec::new()
        };
        use super::rmi::{Record, Records, Removed};
        struct Store<'s>(&'s shards_image::store::Store);
        impl Records for Store<'_> {
            fn untag(&mut self, name: &str) -> Result<(), String> {
                self.0.untag(name).map_err(|e| e.to_string())
            }
            fn alias(&mut self, name: &str, existing: &str) -> Result<(), String> {
                self.0.alias(name, existing).map_err(|e| e.to_string())
            }
        }
        let force = parsed.bool("force");
        let store = match self.store() {
            Ok(store) => store,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let mut errors: Vec<super::rmi::Refused> = Vec::new();
        let with_vms = parsed.bool("vms");
        for given in &parsed.args {
            // `--vms`: the stopped microVMs made from it go first, so that it can; a
            // running one keeps it, and is named.
            if with_vms && let Some(s) = &store {
                for name in self.remove_stopped_of(s, given) {
                    if styled {
                        sheet.record(&[("given", given.clone()), ("removed_vm", name)]);
                    } else {
                        reply.out(&format!("Removed microVM: {name}"));
                    }
                }
            }
            let removed = match &store {
                None => Err(super::rmi::Refused {
                    said: format!("Error response from daemon: {}", not_found(given)),
                    not_found: true,
                }),
                Some(s) => s
                    .references()
                    .map_err(|e| super::rmi::Refused {
                        said: format!("Error response from daemon: {e}"),
                        not_found: false,
                    })
                    .and_then(|records| {
                        let records: Vec<Record> = records
                            .into_iter()
                            .map(|(name, id)| Record { name, id })
                            .collect();
                        let users = super::rmi::users(super::lock(&self.containers).all());
                        super::rmi::delete(&mut Store(s), &records, &users, given, force)
                    }),
            };
            match removed {
                Ok(removed) => {
                    for r in removed {
                        match r {
                            Removed::Untagged(name) if styled => {
                                unpublish(&name);
                                sheet.record(&[("given", given.clone()), ("untagged", name)]);
                            }
                            Removed::Untagged(name) => {
                                unpublish(&name);
                                reply.out(&format!("Untagged: {name}"));
                            }
                            Removed::Deleted(id) => {
                                if styled {
                                    let freed = sizes.iter().find(|(i, _)| *i == id).map_or(0, |(_, n)| *n);
                                    sheet.record(&[
                                        ("given", given.clone()),
                                        ("deleted", truncate_id(&id).to_string()),
                                        ("freed", freed.to_string()),
                                    ]);
                                } else {
                                    reply.out(&format!("Deleted: {id}"));
                                }
                                // What it held goes with the next collection.
                                self.collect_soon();
                            }
                        }
                    }
                }
                Err(e) if styled => {
                    let said = e
                        .said
                        .strip_prefix("Error response from daemon: ")
                        .unwrap_or(&e.said)
                        .to_string();
                    sheet.record(&[("given", given.clone()), ("error", said)]);
                    errors.push(e);
                }
                Err(e) => errors.push(e),
            }
        }
        if styled {
            reply.sheet(&sheet);
            return u8::from(!errors.is_empty() && (!force || errors.iter().any(|e| !e.not_found)));
        }
        if errors.is_empty() {
            return 0;
        }
        let said: Vec<&str> = errors.iter().map(|e| e.said.as_str()).collect();
        reply.err(&said.join("\n"));
        u8::from(!force || errors.iter().any(|e| !e.not_found))
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// Removes the stopped microVMs made from the image `given` names, and returns their
    /// names: what `remove image` takes with the image.
    fn remove_stopped_of(&self, store: &shards_image::store::Store, given: &str) -> Vec<String> {
        let Ok(images) = store.named() else {
            return Vec::new();
        };
        let Ok(image) = resolve(&images, given) else {
            return Vec::new();
        };
        let id = image.id.to_string();
        let stopped: Vec<(String, String)> = super::lock(&self.containers)
            .all()
            .filter(|c| {
                c.image_id.as_deref() == Some(id.as_str()) && c.state != crate::containers::State::Running
            })
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        let mut removed = Vec::new();
        for (cid, name) in stopped {
            if !super::lock(&self.removing).insert(cid.clone()) {
                continue;
            }
            let taken = super::lock(&self.containers).take_out(&cid);
            let gone = match taken {
                Some(removal) => {
                    self.set_aside(&removal).is_ok() && {
                        let _ = self.complete(&removal);
                        true
                    }
                }
                None => false,
            };
            super::lock(&self.removing).remove(&cid);
            if gone {
                removed.push(name);
            }
        }
        removed
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards history IMAGE` (docker/cli history.go): each step of the image's config
    /// history, newest first: the image's ID on the newest, `<missing>` on the rest, as
    /// dockerd's containerd store has it; when, by what, the size of the layer it made, and
    /// its comment. A layer's size is its compressed size, where dockerd's is unpacked.
    pub(super) fn history(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let given = parsed.args.first().map(String::as_str).unwrap_or_default();
        let read = self.store().and_then(|store| {
            let store = store.ok_or_else(|| not_found(given))?;
            let images = store.named().map_err(|e| e.to_string())?;
            let named = resolve(&images, given)?;
            store.image(named).map_err(|e| e.to_string())
        });
        let image = match read {
            Ok(image) => image,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let config: serde_json::Value = image
            .config
            .as_deref()
            .and_then(|c| serde_json::from_slice(c).ok())
            .unwrap_or(serde_json::Value::Null);
        // The layers' sizes, in order, from our platform's manifest.
        let sizes: Vec<i64> = self
            .store()
            .ok()
            .flatten()
            .and_then(|s| std::fs::read(s.blob_path(&image.manifest)).ok())
            .and_then(|b| serde_json::from_slice::<shards_image::oci::Manifest>(&b).ok())
            .map(|m| m.layers.iter().map(|l| l.size).collect())
            .unwrap_or_default();
        let mut layer = 0;
        let mut steps: Vec<(String, String, String, i64, bool)> = Vec::new();
        for h in config
            .get("history")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = |k: &str| {
                h.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let empty = h
                .get("empty_layer")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let size = if empty {
                0
            } else {
                let s = sizes.get(layer).copied().unwrap_or(0);
                layer += 1;
                s
            };
            steps.push((text("created"), text("created_by"), text("comment"), size, empty));
        }
        steps.reverse();
        let now = asker.now / 1_000_000_000;
        let id = truncate_id(&image.id.to_string()).to_string();
        if asker.styled() && !parsed.bool("quiet") {
            let mut sheet = shards_ipc::Sheet::new("history");
            sheet.record(&[
                ("kind", "head".into()),
                ("image", given.to_string()),
                ("id", id.clone()),
            ]);
            for (created, by, comment, size, empty) in &steps {
                let when = shards_dockerfile::go::parse_rfc3339(created.as_bytes()).map_or(0, |t| t.unix().0);
                sheet.record(&[
                    ("created", when.to_string()),
                    ("by", by.clone()),
                    ("size", size.to_string()),
                    ("empty", empty.to_string()),
                    ("comment", comment.clone()),
                ]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        let (human, trunc) = (parsed.bool("human"), !parsed.bool("no-trunc"));
        if parsed.bool("quiet") {
            for (i, _) in steps.iter().enumerate() {
                reply.out(if i == 0 { &id } else { "<missing>" });
            }
            return 0;
        }
        let mut rows = vec![[
            "IMAGE".to_string(),
            "CREATED".into(),
            "CREATED BY".into(),
            "SIZE".into(),
            "COMMENT".into(),
        ]];
        for (i, (created, by, comment, size, _)) in steps.iter().enumerate() {
            let when = shards_dockerfile::go::parse_rfc3339(created.as_bytes()).map_or(0, |t| t.unix().0);
            let created = if human {
                let ago = u128::try_from(now.saturating_sub(when).max(0)).unwrap_or(0) * 1_000_000_000;
                format!("{} ago", super::commands::human_duration(ago))
            } else {
                created.clone()
            };
            let by = by.replace('\t', " ");
            let by = if trunc && by.chars().count() > 45 {
                format!("{}…", by.chars().take(44).collect::<String>())
            } else {
                by
            };
            let size = if human {
                human_size(*size)
            } else {
                size.to_string()
            };
            rows.push([
                if i == 0 { id.clone() } else { "<missing>".into() },
                created,
                by,
                size,
                comment.clone(),
            ]);
        }
        for line in super::commands::tabulate(&rows, asker.east_asian) {
            reply.out(&line);
        }
        0
    }
}

/// The bytes the files under `dir` hold, as `du` counts them: their blocks.
fn on_disk(dir: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.blocks().saturating_mul(512));
            }
        }
    }
    total
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards system df` (docker/cli formatter/disk_usage.go): images, containers, local
    /// volumes and the build cache, each its count, how many are in use, its size, and
    /// what removing the unused would free. A colour terminal also hears of the microVMs'
    /// templates, which only shards has.
    pub(super) fn system_df(&self, asker: &super::commands::Asker, reply: &super::commands::Reply<'_>) -> u8 {
        let images = self.listed(None, false).unwrap_or_default();
        let (img_total, img_active) = (images.len(), images.iter().filter(|i| i.containers > 0).count());
        let img_size: i64 = images.iter().map(|i| i.size).sum();
        let img_free: i64 = images.iter().filter(|i| i.containers == 0).map(|i| i.size).sum();
        let held: Vec<(bool, String)> = super::lock(&self.containers)
            .all()
            .map(|c| (c.state == crate::containers::State::Running, c.id.clone()))
            .collect();
        let containers: Vec<(bool, u64)> = held
            .into_iter()
            .map(|(running, id)| (running, on_disk(&self.home.join("containers").join(id))))
            .collect();
        let c_total = containers.len();
        let c_active = containers.iter().filter(|(r, _)| *r).count();
        let c_size = i64::try_from(containers.iter().map(|(_, s)| s).sum::<u64>()).unwrap_or(i64::MAX);
        let c_free = i64::try_from(
            containers
                .iter()
                .filter(|(r, _)| !*r)
                .map(|(_, s)| s)
                .sum::<u64>(),
        )
        .unwrap_or(i64::MAX);
        let templates = self.home.join("templates");
        let t_total = std::fs::read_dir(&templates)
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        let t_size = i64::try_from(on_disk(&templates)).unwrap_or(i64::MAX);
        if asker.styled() {
            let mut sheet = shards_ipc::Sheet::new("df");
            for (kind, total, active, bytes, freeable) in [
                ("images", img_total, img_active, img_size, img_free),
                ("microVMs", c_total, c_active, c_size, c_free),
                ("templates", t_total, t_total, t_size, 0),
            ] {
                sheet.record(&[
                    ("kind", kind.into()),
                    ("total", total.to_string()),
                    ("active", active.to_string()),
                    ("size", bytes.to_string()),
                    ("reclaimable", freeable.to_string()),
                ]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        let free = |f: i64, all: i64| {
            if all <= 0 {
                human_size(0)
            } else {
                format!("{} ({}%)", human_size(f), f * 100 / all)
            }
        };
        let rows = [
            [
                "TYPE".to_string(),
                "TOTAL".into(),
                "ACTIVE".into(),
                "SIZE".into(),
                "RECLAIMABLE".into(),
            ],
            [
                "Images".into(),
                img_total.to_string(),
                img_active.to_string(),
                human_size(img_size),
                free(img_free, img_size),
            ],
            [
                "Containers".into(),
                c_total.to_string(),
                c_active.to_string(),
                human_size(c_size),
                free(c_free, c_size),
            ],
            [
                "Local Volumes".into(),
                "0".into(),
                "0".into(),
                human_size(0),
                human_size(0),
            ],
            [
                "Build Cache".into(),
                "0".into(),
                "0".into(),
                human_size(0),
                human_size(0),
            ],
        ];
        for line in super::commands::tabulate(&rows, asker.east_asian) {
            reply.out(&line);
        }
        0
    }
}

/// go-units' HumanSize: four significant digits, as prune reports what it reclaimed.
fn human_size4(size: i64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = size as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    format!("{}{}", go_g(size, 4), UNITS.get(unit).unwrap_or(&""))
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards prune vm`, `prune image` and `prune system` (docker/cli container/prune.go,
    /// image/prune.go, system/prune.go; the confirmation is the client's): the stopped
    /// microVMs, with `containers`; the dangling images, or with `all` every image no
    /// microVM was made from, with `images`; then what was reclaimed, as the CLI says it.
    pub(super) fn prune(
        &self,
        images: bool,
        containers: bool,
        all: bool,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let mut reclaimed: i64 = 0;
        let mut removed_vms: Vec<(String, String)> = Vec::new();
        if containers {
            let stopped: Vec<(String, String)> = super::lock(&self.containers)
                .all()
                .filter(|c| c.state != crate::containers::State::Running)
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            for (id, name) in stopped {
                if !super::lock(&self.removing).insert(id.clone()) {
                    continue;
                }
                let bytes = on_disk(&self.home.join("containers").join(&id));
                let taken = super::lock(&self.containers).take_out(&id);
                let gone = match taken {
                    Some(removal) => {
                        self.set_aside(&removal).is_ok() && {
                            let _ = self.complete(&removal);
                            true
                        }
                    }
                    None => false,
                };
                super::lock(&self.removing).remove(&id);
                if gone {
                    reclaimed = reclaimed.saturating_add(i64::try_from(bytes).unwrap_or(i64::MAX));
                    removed_vms.push((id, name));
                }
            }
        }
        let mut removed_images: Vec<super::rmi::Removed> = Vec::new();
        if images && let Ok(Some(store)) = self.store() {
            use super::rmi::{Record, Records};
            struct Store<'s>(&'s shards_image::store::Store);
            impl Records for Store<'_> {
                fn untag(&mut self, name: &str) -> Result<(), String> {
                    self.0.untag(name).map_err(|e| e.to_string())
                }
                fn alias(&mut self, name: &str, existing: &str) -> Result<(), String> {
                    self.0.alias(name, existing).map_err(|e| e.to_string())
                }
            }
            let sizes: Vec<(String, u64)> = store
                .images()
                .unwrap_or_default()
                .into_iter()
                .map(|i| (i.id.to_string(), i.content.saturating_add(i.unpacked)))
                .collect();
            let users = super::rmi::users(super::lock(&self.containers).all());
            let used = |id: &str| users.iter().any(|u| u.image.as_deref() == Some(id));
            // Each image to go, by a name that reaches it: its dangling record, or, with
            // `all`, its ID.
            let mut doomed: Vec<String> = Vec::new();
            for (name, id) in store.references().unwrap_or_default() {
                let id = id.to_string();
                if used(&id) {
                    continue;
                }
                let dangling = name.starts_with(super::rmi::DANGLING);
                if (dangling || all) && !doomed.iter().any(|d| d == &id || d == &name) {
                    doomed.push(if all { id } else { name });
                }
            }
            for given in doomed {
                let records: Vec<Record> = store
                    .references()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, id)| Record { name, id })
                    .collect();
                if let Ok(removed) = super::rmi::delete(&mut Store(&store), &records, &users, &given, true) {
                    for r in removed {
                        if let super::rmi::Removed::Deleted(id) = &r {
                            let freed = sizes.iter().find(|(i, _)| i == id).map_or(0, |(_, n)| *n);
                            reclaimed = reclaimed.saturating_add(i64::try_from(freed).unwrap_or(i64::MAX));
                            self.collect_soon();
                        }
                        removed_images.push(r);
                    }
                }
            }
        }
        if asker.styled() {
            let mut sheet = shards_ipc::Sheet::new("prune");
            sheet.record(&[("kind", "head".into()), ("reclaimed", reclaimed.to_string())]);
            for (id, name) in &removed_vms {
                sheet.record(&[("vm", name.clone()), ("id", truncate_id(id).to_string())]);
            }
            for r in &removed_images {
                match r {
                    super::rmi::Removed::Untagged(n) => sheet.record(&[("untagged", n.clone())]),
                    super::rmi::Removed::Deleted(d) => {
                        sheet.record(&[("deleted", truncate_id(d).to_string())])
                    }
                }
            }
            reply.sheet(&sheet);
            return 0;
        }
        let mut text = String::new();
        if !removed_vms.is_empty() {
            text.push_str("Deleted Containers:\n");
            for (id, _) in &removed_vms {
                text.push_str(id);
                text.push('\n');
            }
            text.push('\n');
        }
        if !removed_images.is_empty() {
            text.push_str("Deleted Images:\n");
            for r in &removed_images {
                match r {
                    super::rmi::Removed::Untagged(n) => text.push_str(&format!("untagged: {n}\n")),
                    super::rmi::Removed::Deleted(d) => text.push_str(&format!("deleted: {d}\n")),
                }
            }
            text.push('\n');
        }
        text.push_str(&format!("Total reclaimed space: {}\n", human_size4(reclaimed)));
        let _ = reply.bytes(crate::spec::LOG_STDOUT, text.as_bytes());
        0
    }
}

/// The microVM a name made, gone from the machine's local image store with the name.
fn unpublish(name: &str) {
    if std::env::var("SHARDS_LOCAL_STORE").is_ok_and(|v| v == "none") {
        return;
    }
    if let Some(socket) = crate::local_store::engine(&|k| std::env::var(k).ok())
        && let Err(e) = crate::local_store::unpublish(&socket, name)
    {
        super::log(e);
    }
}

/// dockerd's words for an image it cannot find (moby daemon/images/image.go,
/// ErrImageDoesNotExist): the reference as given, with `latest` if it names no tag; a
/// digest as it is.
pub(super) fn not_found(given: &str) -> String {
    use shards_image::reference::AnyReference;
    match AnyReference::parse(given) {
        Ok(AnyReference::Digest(d)) => format!("No such image: {d}"),
        Ok(AnyReference::Named(r)) => format!("No such image: {}", r.tag_name_only().familiar()),
        Err(_) => format!("No such image: {given}"),
    }
}

/// The image `given` names, as dockerd's containerd store finds one (moby
/// daemon/containerd/image.go, resolveImage): by digest, its ID, and a name with a digest
/// only in that repository; else by name, with `latest` if it names no tag; else by a
/// prefix of its ID of 4 to 64 hex digits, refused if more than one image has it.
pub(super) fn resolve<'a>(
    images: &'a [shards_image::store::Named],
    given: &str,
) -> Result<&'a shards_image::store::Named, String> {
    use shards_image::reference::{AnyReference, Reference};
    let parsed = AnyReference::parse(given).map_err(|e| e.to_string())?;
    let named = |i: &shards_image::store::Named, name: &str| {
        i.references
            .iter()
            .any(|r| Reference::parse_normalized(r).is_ok_and(|r| r.name() == name))
    };
    let tagged = match parsed {
        AnyReference::Digest(d) => {
            return images.iter().find(|i| i.id == d).ok_or_else(|| not_found(given));
        }
        AnyReference::Named(r) => match r.digest.clone() {
            Some(d) => {
                return images
                    .iter()
                    .find(|i| i.id == d && named(i, &r.name()))
                    .ok_or_else(|| not_found(given));
            }
            None => r.tag_name_only().to_string(),
        },
    };
    if let Some(i) = images.iter().find(|i| i.references.contains(&tagged)) {
        return Ok(i);
    }
    let id = truncated_id(given).ok_or_else(|| not_found(given))?;
    let mut matching = images
        .iter()
        .filter(|i| i.id.algorithm().name() == "sha256" && i.id.hex().starts_with(id));
    match (matching.next(), matching.next()) {
        (None, _) => Err(not_found(given)),
        (Some(i), None) => Ok(i),
        (Some(_), Some(_)) => Err("ambiguous reference".into()),
    }
}

/// checkTruncatedID: `given`, without `sha256:`, if it is 4 to 64 lowercase hex digits.
pub(super) fn truncated_id(given: &str) -> Option<&str> {
    let id = given.strip_prefix("sha256:").unwrap_or(given);
    ((4..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(id)
}

/// The reference filter (moby daemon/containerd/image_list.go, distribution's
/// FamiliarMatch): `pattern`, as Go's path.Match reads it, matches `r` familiar, with its
/// tag or digest or without them, never by its whole name (measured, Docker 29.3.1:
/// `docker.io/library/busybox` matches nothing). A pattern Go cannot read matches nothing;
/// in dockerd it may make the others fail too, as its map orders them, 9 runs in 10.
fn matches(pattern: &str, r: &shards_image::reference::Reference) -> bool {
    let mut bare = r.clone();
    bare.tag = None;
    bare.digest = None;
    [r.familiar(), bare.familiar()]
        .iter()
        .any(|t| shards_dockerfile::glob::filepath_match(pattern.as_bytes(), t.as_bytes()) == Ok(true))
}

// The tree view (tree.go).

const UNTAGGED: &str = "<untagged>";
const SPACING: usize = 3;

struct Details {
    id: String,
    disk_usage: String,
    in_use: bool,
    content_size: String,
}

struct Top {
    names: Vec<String>,
    details: std::rc::Rc<Details>,
    children: std::rc::Rc<Vec<Sub>>,
}

struct Sub {
    platform: String,
    available: bool,
    details: Details,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    Id,
    DiskUsage,
    ContentSize,
    Extra,
}

#[derive(Clone, Copy)]
struct Column {
    title: &'static str,
    width: usize,
    align: Align,
    value: Option<Value>,
    /// Its own colour, whatever the row's (the chips').
    plain: bool,
    no_ellipsis: bool,
}

/// The tree view of `images`, as runTree and printImageTree write it to `out`.
pub(super) fn tree(images: &[Summary], expanded: bool, out: Out) -> String {
    let mut spacing = false;
    let mut view: Vec<Top> = Vec::with_capacity(images.len());
    for img in images {
        let mut in_use = img.containers > 0;
        let mut content = 0i64;
        let mut children = Vec::new();
        for m in &img.manifests {
            content = content.saturating_add(m.content);
            if m.kind != Kind::Image {
                continue;
            }
            in_use |= m.in_use;
            if !expanded {
                continue;
            }
            children.push(Sub {
                platform: m.platform.clone(),
                available: m.available,
                details: Details {
                    id: m.id.clone(),
                    disk_usage: human_size(m.total),
                    in_use: m.in_use,
                    content_size: human_size(m.content),
                },
            });
            spacing = true;
        }
        let details = std::rc::Rc::new(Details {
            id: img.id.clone(),
            disk_usage: human_size(img.size),
            in_use,
            content_size: human_size(content),
        });
        let children = std::rc::Rc::new(children);
        let mut tags = img.tags.clone();
        tags.sort();
        if expanded {
            view.push(Top {
                names: tags,
                details,
                children,
            });
            continue;
        }
        if tags.is_empty() {
            view.push(Top {
                names: Vec::new(),
                details: details.clone(),
                children: children.clone(),
            });
        }
        for tag in tags {
            view.push(Top {
                names: vec![tag],
                details: details.clone(),
                children: children.clone(),
            });
        }
    }
    // By first name, the unnamed last.
    view.sort_by(|a, b| {
        let (x, y) = (a.names.first(), b.names.first());
        match (x, y) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    print_tree(&view, spacing, out)
}

fn print_tree(view: &[Top], spacing: bool, out: Out) -> String {
    let color = out.terminal && out.color;
    let paint = |s: &'static str| if color { Sgr(s) } else { NONE };
    let top_name = paint("\x1b[34m\x1b[1m");
    let normal = paint("\x1b[39m");
    let faint = paint("\x1b[39m\x1b[2m");
    let untagged = paint("\x1b[39m\x1b[2m");
    let title = paint("\x1b[39m\x1b[1m");
    let tw = |s: &str| width::string_width(&clean_ansi(s), out.east_asian);
    let mut width = usize::from(out.width);
    let unlimited = width == 0;
    if out.terminal && width < 20 {
        width = 20;
    }
    // The In Use chip, as tui.Chip draws it, or its letter.
    let chip = if color {
        "\x1b[38;5;0m\x1b[48;5;14m U \x1b[0m"
    } else {
        "U"
    };
    let placeholder = if color { "   " } else { " " };
    let mut text = String::new();
    if !unlimited {
        let header = if color {
            "\x1b[1m\x1b[106m\x1b[30mi\x1b[0m\x1b[0m \x1b[96mInfo → \x1b[0m\x1b[0m"
        } else {
            " Info -> "
        };
        let legend = format!("{header} {chip} In Use");
        text.push_str(&" ".repeat(width.saturating_sub(tw(&legend))));
        text.push_str(&legend);
        text.push('\n');
    }
    let chips = view
        .iter()
        .any(|t| t.details.in_use || t.children.iter().any(|c| c.details.in_use));
    let extra_width = if chips {
        tw(chip).max("Extra".len())
    } else {
        "Extra".len()
    };
    let mut columns = vec![
        Column {
            title: "Image",
            width: 0,
            align: Align::Left,
            value: None,
            plain: false,
            no_ellipsis: true,
        },
        Column {
            title: "ID",
            width: 12,
            align: Align::Left,
            value: Some(Value::Id),
            plain: false,
            no_ellipsis: false,
        },
        Column {
            title: "Disk usage",
            width: 10,
            align: Align::Right,
            value: Some(Value::DiskUsage),
            plain: false,
            no_ellipsis: false,
        },
        Column {
            title: "Content size",
            width: 12,
            align: Align::Right,
            value: Some(Value::ContentSize),
            plain: false,
            no_ellipsis: false,
        },
        Column {
            title: "Extra",
            width: extra_width,
            align: Align::Left,
            value: Some(Value::Extra),
            plain: true,
            no_ellipsis: false,
        },
    ];
    adjust_columns(width, &mut columns, view);
    let print = |c: &Column, clr: Sgr, s: &str| -> String {
        let ln = tw(s);
        let fill = if ln > c.width {
            if !c.no_ellipsis {
                return clr.apply(&ellipsis(s, c.width));
            }
            0
        } else {
            c.width - ln
        };
        match c.align {
            Align::Left => format!("{}{}", clr.apply(s), " ".repeat(fill)),
            Align::Right => format!("{}{}", " ".repeat(fill), clr.apply(s)),
        }
    };
    let value = |v: Value, d: &Details| -> String {
        match v {
            Value::Id => truncate_id(&d.id).to_string(),
            Value::DiskUsage => d.disk_usage.clone(),
            Value::ContentSize => d.content_size.clone(),
            Value::Extra => {
                if !chips {
                    String::new()
                } else if d.in_use {
                    chip.to_string()
                } else {
                    placeholder.to_string()
                }
            }
        }
    };
    let details = |text: &mut String, clr: Sgr, d: &Details| {
        for c in columns.iter().skip(1) {
            let Some(v) = c.value else { continue };
            text.push_str(&" ".repeat(SPACING));
            text.push_str(&print(c, if c.plain { NONE } else { clr }, &value(v, d)));
        }
    };
    for (i, c) in columns.iter().enumerate() {
        if i > 0 {
            text.push_str(&" ".repeat(SPACING));
        }
        text.push_str(&print(c, title, &c.title.to_uppercase()));
    }
    text.push('\n');
    let Some(first) = columns.first().copied() else {
        return text;
    };
    for img in view {
        if img.names.is_empty() {
            text.push_str(&print(&first, untagged, UNTAGGED));
        }
        for (i, name) in img.names.iter().enumerate() {
            let last = i + 1 == img.names.len();
            let multi_line = tw(name) > first.width;
            text.push_str(&print(&first, top_name, name));
            if !last || multi_line {
                text.push('\n');
            }
            if multi_line && last {
                text.push_str(&" ".repeat(first.width));
            }
        }
        details(&mut text, normal, &img.details);
        if !img.children.is_empty() || spacing {
            text.push('\n');
        }
        for (i, sub) in img.children.iter().enumerate() {
            let clr = if sub.available { normal } else { faint };
            let branch = if i + 1 == img.children.len() {
                "└─ "
            } else {
                "├─ "
            };
            text.push_str(&print(&first, clr, &format!("{branch}{}", sub.platform)));
            details(&mut text, clr, &sub.details);
            text.push('\n');
        }
        text.push('\n');
    }
    text
}

/// adjustColumns: the name column as wide as what is left of `width` once the others
/// have theirs, those that would leave it under 12 dropped, and no wider than its widest
/// name; on no terminal, as wide as that.
fn adjust_columns(width: usize, columns: &mut Vec<Column>, view: &[Top]) {
    let mut name_width = width;
    if name_width > 0 {
        let mut keep = columns.len();
        for (idx, c) in columns.iter().enumerate() {
            if c.width == 0 {
                continue;
            }
            let d = c.width + if idx > 0 { SPACING } else { 0 };
            if name_width < d + 12 {
                keep = idx;
                break;
            }
            name_width -= d;
        }
        columns.truncate(keep);
    }
    // The widest name or platform, in bytes, as widestFirstColumnValue counts them.
    let mut widest = "Image".len();
    for img in view {
        if img.names.is_empty() {
            widest = widest.max(UNTAGGED.len());
        }
        for name in &img.names {
            widest = widest.max(name.len());
        }
        for sub in img.children.iter() {
            widest = widest.max(sub.platform.len() + "└─ ".len());
        }
    }
    if width == 0 || name_width > widest {
        name_width = widest;
    }
    if let Some(first) = columns.first_mut() {
        first.width = name_width;
    }
}

/// tui's cleanANSI: each escape, to the next `m`, taken out.
fn clean_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('\x1b') {
        let Some(end) = rest.get(start..).and_then(|r| r.find('m')) else {
            break;
        };
        out.push_str(rest.get(..start).unwrap_or_default());
        rest = rest.get(start + end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// tui.Ellipsis: the first `length - 1` characters and `…`, escapes kept, once there
/// are `length`.
fn ellipsis(s: &str, length: usize) -> String {
    let mut out = String::new();
    let (mut n, mut escape, mut long) = (0usize, false, false);
    for c in s.chars() {
        if c == '\x1b' {
            out.push(c);
            escape = true;
            continue;
        }
        if escape {
            out.push(c);
            if c == 'm' {
                escape = false;
                if long {
                    break;
                }
            }
            continue;
        }
        n += 1;
        if n == length {
            long = true;
        }
        if !long {
            out.push(c);
        }
    }
    if long {
        out.push('…');
    }
    out
}

// The table (formatter/image.go).

/// What the table shows of `images` at `now` (seconds since the epoch): with `quiet`,
/// each row's ID; else its rows under their header.
pub(super) fn table(images: &[Summary], now: i64, trunc: bool, quiet: bool, digests: bool) -> Vec<String> {
    let mut rows: Vec<(String, String, String, &Summary)> = Vec::new();
    for img in images {
        if img.tags.is_empty() && img.digests.is_empty() {
            rows.push(("<none>".into(), "<none>".into(), "<none>".into(), img));
            continue;
        }
        tagged_and_digested(img, digests, &mut rows);
    }
    let id = |img: &Summary| {
        if trunc {
            truncate_id(&img.id).to_string()
        } else {
            img.id.clone()
        }
    };
    if quiet {
        return rows.iter().map(|(_, _, _, img)| id(img)).collect();
    }
    let created = |img: &Summary| {
        let ago = u128::try_from(now.saturating_sub(img.created)).unwrap_or(0) * 1_000_000_000;
        format!("{} ago", super::commands::human_duration(ago))
    };
    if digests {
        let mut table =
            vec![["REPOSITORY", "TAG", "DIGEST", "IMAGE ID", "CREATED", "SIZE"].map(String::from)];
        table.extend(rows.iter().map(|(repo, tag, digest, img)| {
            [
                repo.clone(),
                tag.clone(),
                digest.clone(),
                id(img),
                created(img),
                human_size(img.size),
            ]
        }));
        super::commands::tabulate(&table, false)
    } else {
        let mut table = vec![["REPOSITORY", "TAG", "IMAGE ID", "CREATED", "SIZE"].map(String::from)];
        table.extend(rows.iter().map(|(repo, tag, _, img)| {
            [
                repo.clone(),
                tag.clone(),
                id(img),
                created(img),
                human_size(img.size),
            ]
        }));
        super::commands::tabulate(&table, false)
    }
}

/// imageFormatTaggedAndDigest: a row per tag of each repository, with each of that
/// repository's digests when `digests` are shown; then the repositories with digests
/// alone. Names that are not references are left out.
fn tagged_and_digested<'a>(
    img: &'a Summary,
    digests: bool,
    rows: &mut Vec<(String, String, String, &'a Summary)>,
) {
    use shards_image::reference::Reference;
    let familiar = |mut r: Reference| {
        r.tag = None;
        r.digest = None;
        r.familiar()
    };
    let mut tags: Vec<(String, Vec<String>)> = Vec::new();
    for s in &img.tags {
        let Ok(r) = Reference::parse_normalized(s) else {
            continue;
        };
        let Some(tag) = r.tag.clone() else { continue };
        let repo = familiar(r);
        match tags.iter_mut().find(|(name, _)| *name == repo) {
            Some((_, list)) => list.push(tag),
            None => tags.push((repo, vec![tag])),
        }
    }
    let mut by_digest: Vec<(String, Vec<String>)> = Vec::new();
    for s in &img.digests {
        let Ok(r) = Reference::parse_normalized(s) else {
            continue;
        };
        let Some(digest) = r.digest.as_ref().map(ToString::to_string) else {
            continue;
        };
        let repo = familiar(r);
        match by_digest.iter_mut().find(|(name, _)| *name == repo) {
            Some((_, list)) => list.push(digest),
            None => by_digest.push((repo, vec![digest])),
        }
    }
    for (repo, list) in &tags {
        let pos = by_digest.iter().position(|(name, _)| name == repo);
        let own = pos
            .map(|p| by_digest.remove(p).1)
            .filter(|_| digests)
            .unwrap_or_default();
        for tag in list {
            if own.is_empty() {
                rows.push((repo.clone(), tag.clone(), "<none>".into(), img));
                continue;
            }
            for d in &own {
                rows.push((repo.clone(), tag.clone(), d.clone(), img));
            }
        }
    }
    for (repo, list) in by_digest {
        if digests {
            for d in list {
                rows.push((repo.clone(), "<none>".into(), d, img));
            }
        } else {
            rows.push((repo, "<none>".into(), String::new(), img));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn golden() -> serde_json::Value {
        serde_json::from_str(include_str!("docker-images.json")).unwrap()
    }

    fn summaries(v: &serde_json::Value, now: i64) -> Vec<Summary> {
        let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        let list = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .map(|a| a.iter().map(text).collect())
                .unwrap_or_default()
        };
        v.as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|i| Summary {
                id: text(&i["id"]),
                tags: list(&i["tags"]),
                digests: list(&i["digests"]),
                created: now - i["ago"].as_i64().unwrap(),
                size: i["size"].as_i64().unwrap(),
                containers: i["containers"].as_i64().unwrap(),
                manifests: i["manifests"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|m| Manifest {
                        id: text(&m["id"]),
                        kind: match m["kind"].as_str().unwrap() {
                            "attestation" => Kind::Attestation,
                            _ => Kind::Image,
                        },
                        platform: text(&m["platform"]),
                        available: m["available"].as_bool().unwrap(),
                        content: m["content"].as_i64().unwrap(),
                        total: m["total"].as_i64().unwrap(),
                        in_use: m["in_use"].as_bool().unwrap(),
                    })
                    .collect(),
            })
            .collect()
    }

    fn stored(hex: &str, references: &[&str]) -> shards_image::store::Image {
        shards_image::store::Image {
            id: shards_image::reference::Digest::parse(&format!("sha256:{hex}")).unwrap(),
            references: references.iter().map(|r| (*r).to_string()).collect(),
            target: shards_image::oci::Descriptor {
                media_type: String::new(),
                digest: format!("sha256:{hex}"),
                size: 0,
                platform: None,
                annotations: Default::default(),
            },
            manifest: shards_image::reference::Digest::parse(&format!("sha256:{hex}")).unwrap(),
            config: None,
            tagged_at: None,
            sources: Vec::new(),
            targets: std::collections::BTreeMap::new(),
            created: None,
            manifests: Vec::new(),
            content: 0,
            unpacked: 0,
        }
    }

    fn named(hex: &str, references: &[&str]) -> shards_image::store::Named {
        let image = stored(hex, references);
        shards_image::store::Named {
            id: image.id,
            references: image.references,
            manifest: image.target,
            tagged_at: None,
            sources: Vec::new(),
            targets: std::collections::BTreeMap::new(),
        }
    }

    /// A manifest is in use while a container runs it, and a run uses the image's
    /// manifest for our platform: not every manifest here, as dockerd matches each
    /// container's ImageManifest.
    #[test]
    fn the_manifest_runs_use_is_the_one_in_use() {
        let manifest = |hex: char, attestation: bool| shards_image::store::ImageManifest {
            digest: shards_image::reference::Digest::parse(&format!("sha256:{}", hex.to_string().repeat(64)))
                .unwrap(),
            platform: Some("linux/amd64".into()),
            attestation,
            available: true,
            content: 1,
            unpacked: 0,
        };
        let mut img = stored(&"a".repeat(64), &["docker.io/library/a:1"]);
        img.manifest = manifest('b', false).digest;
        img.manifests = vec![manifest('c', false), manifest('b', false), manifest('d', true)];
        let used: Vec<bool> = manifests(&img, 1).iter().map(|m| m.in_use).collect();
        assert_eq!(used, [false, true, false]);
        assert!(manifests(&img, 0).iter().all(|m| !m.in_use));
    }

    /// Images are found as dockerd's containerd store finds them: by digest or ID, by a
    /// name with a digest only in its repository, by name with `latest` as its tag, and
    /// by an ID's prefix of 4 or more hex digits, refused where it is not one image's.
    #[test]
    fn images_resolve_as_dockerd_resolves_them() {
        let (a, b, c) = (
            "aaaa1".repeat(12) + "aaaa",
            "aaaa2".repeat(12) + "aaaa",
            "c".repeat(64),
        );
        let images = [
            named(
                &a,
                &["docker.io/library/alpine:3.22", "docker.io/library/alpine:latest"],
            ),
            named(&b, &["docker.io/library/busybox:1"]),
            named(&c, &["localhost:5000/team/app:v1"]),
        ];
        let found = |given: &str| resolve(&images, given).map(|i| i.id.hex().to_string());
        assert_eq!(found("alpine"), Ok(a.clone()));
        assert_eq!(found("alpine:3.22"), Ok(a.clone()));
        assert_eq!(found("docker.io/library/busybox:1"), Ok(b.clone()));
        assert_eq!(found("localhost:5000/team/app:v1"), Ok(c.clone()));
        assert_eq!(found(&c), Ok(c.clone()), "an ID");
        assert_eq!(found(&format!("sha256:{c}")), Ok(c.clone()), "a digest");
        assert_eq!(
            found(&format!("localhost:5000/team/app@sha256:{c}")),
            Ok(c.clone())
        );
        assert_eq!(
            found(&format!("busybox@sha256:{c}")),
            Err(format!("No such image: busybox@sha256:{c}")),
            "a digest of another repository"
        );
        assert_eq!(found("cccc"), Ok(c.clone()), "a prefix");
        assert_eq!(found("sha256:cccccc"), Ok(c.clone()));
        assert_eq!(found("ccc"), Err("No such image: ccc:latest".into()), "too short");
        assert_eq!(found("aaaa1"), Ok(a.clone()));
        assert_eq!(found("aaaa"), Err("ambiguous reference".into()));
        assert_eq!(found("nothing:1"), Err("No such image: nothing:1".into()));
        assert_eq!(found("busybox"), Err("No such image: busybox:latest".into()));
        assert_eq!(
            found(&"0".repeat(64)),
            Err(format!("No such image: sha256:{}", "0".repeat(64)))
        );
    }

    #[test]
    fn sizes_read_as_go_units_writes_them() {
        for s in golden()["sizes"].as_array().unwrap() {
            assert_eq!(
                human_size(s["n"].as_i64().unwrap()),
                s["text"].as_str().unwrap(),
                "{s}"
            );
        }
    }

    #[test]
    fn trees_show_as_docker_images_shows_them() {
        let golden = golden();
        let trees = golden["trees"].as_array().unwrap();
        assert!(trees.len() > 60);
        for t in trees {
            let width = u16::try_from(t["width"].as_u64().unwrap()).unwrap();
            let out = Out {
                terminal: width > 0,
                width,
                color: !t["no_color"].as_bool().unwrap(),
                east_asian: false,
            };
            let got = tree(&summaries(&t["images"], 0), t["expanded"].as_bool().unwrap(), out);
            assert_eq!(
                got,
                t["output"].as_str().unwrap(),
                "width {width}, expanded {}, no colour {}",
                t["expanded"],
                t["no_color"]
            );
        }
    }

    #[test]
    fn tables_show_as_docker_images_shows_them() {
        let golden = golden();
        let now = 1_790_673_154;
        for t in golden["tables"].as_array().unwrap() {
            let lines = table(
                &summaries(&t["images"], now),
                now,
                t["trunc"].as_bool().unwrap(),
                t["quiet"].as_bool().unwrap(),
                t["digests"].as_bool().unwrap(),
            );
            let got: String = lines.iter().map(|l| format!("{l}\n")).collect();
            assert_eq!(
                got,
                t["output"].as_str().unwrap(),
                "{} {} {}",
                t["trunc"],
                t["quiet"],
                t["digests"]
            );
        }
    }
}
