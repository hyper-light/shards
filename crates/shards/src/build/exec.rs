//! Runs what a build's definition asks of the host (docs/design/architecture.md D33):
//! base images and the context as snapshots, file operations as BuildKit's
//! FileOpSolver runs them (dockerfile/1.27.1 solver/llbsolver/ops/file.go), and merges.
//! Each committed result is a snapshot and the chain of layers that makes it; a layer
//! is written to the store as BuildKit's differ writes it (`shards_build::diff`),
//! uncompressed. Each result follows the tree its layers stack to too
//! (`shards_build::stack`), so the image's root filesystem is written from the last
//! snapshot, not stacked again from the layers.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use shards_build::copy::Chown;
use shards_build::data::Sources;
use shards_build::ops::{self, CopyAction};
use shards_build::stack::{Stack, Tally};
use shards_build::vfs::Fs;
use shards_build::{Error as BuildError, diff};
use shards_dockerfile::export::Layer;
use shards_dockerfile::go::Time;
use shards_dockerfile::llb::{OpAction, OpActionKind, OpChown, OpUser};
use shards_image::erofs::{self, DataRef, Kind, Meta, Node, Tree};
use shards_image::layer;
use shards_image::reference::Digest;
use shards_image::store::{self, Limits, Store, Unpacked};

use super::builder::{Origin, origin_id};

/// The media type of the layers shards writes: uncompressed, as the store keeps them for
/// `shards run` to unpack at no cost.
pub const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

/// A committed result: its snapshot, the layers that make it, and how the tree they
/// stack to differs from the snapshot.
#[derive(Debug, Clone)]
pub struct Ref {
    pub fs: Rc<Fs>,
    pub layers: Vec<Layer>,
    pub stack: Stack,
    /// Where its tree comes from, as a builder guest would hold it.
    pub origin: Rc<Origin>,
}

/// A target's snapshot and its stack, to write its image's root filesystem from.
#[derive(Debug)]
pub struct Flat {
    fs: Fs,
    stack: Stack,
}

/// The time now, as the kernel would stamp a change.
pub fn now() -> (i64, u32) {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

/// An empty snapshot: scratch, its root a directory BuildKit just made.
fn scratch() -> Fs {
    let t = now();
    Fs::new(
        Tree::new(Meta {
            mode: 0o755,
            mtime: t.0,
            mtime_nsec: t.1,
            ..Meta::default()
        }),
        t,
    )
}

/// Scratch as a snapshot: empty, and the tree its no layers stack to.
fn scratch_ref() -> Ref {
    let fs = scratch();
    let stack = Stack::layers(Tally::default(), fs.tree());
    Ref {
        fs: Rc::new(fs),
        layers: Vec::new(),
        stack,
        origin: Rc::new(Origin::Scratch),
    }
}

/// What the build reads files from, and keeps until it is done.
#[derive(Debug)]
pub struct Exec<'a> {
    pub store: &'a Store,
    pub limits: &'a Limits,
    pub sources: Sources,
    /// Layers unpacked for snapshots, their files read until the build ends.
    unpacked: Vec<Unpacked>,
    /// Where the context's files are snapshotted; dropped after `sources`, which reads them.
    stages: Vec<store::Stage>,
    /// Where ADD's archives are decompressed, dropped after `sources` too.
    unpack: Option<store::Stage>,
    /// What the build's ADDs may unpack, together.
    budget: shards_build::archive::Budget,
    /// Where `RUN` steps' changes are kept, the source they are read as, and how much it
    /// holds.
    run_staging: Option<(std::fs::File, u32, u64)>,
    /// The build context's snapshots: what BuildKit takes content checksums of before an
    /// operation reads them.
    contexts: Vec<Rc<Fs>>,
    /// The cache ids the build's steps mount, each named in the builder by its index: an
    /// id is any string, and as a name of its own it could pass a file name's limit.
    caches: Vec<Vec<u8>>,
}

/// One slot of a file operation: an input, an action's mount, or its committed result.
enum Slot {
    Ref(Ref),
    Mount { base: Option<Ref>, fs: Box<Fs> },
    Taken,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl<'a> Exec<'a> {
    pub fn new(store: &'a Store, limits: &'a Limits) -> Exec<'a> {
        Exec {
            store,
            limits,
            sources: Sources::default(),
            unpacked: Vec::new(),
            stages: Vec::new(),
            unpack: None,
            budget: shards_build::archive::Budget::new(*limits),
            run_staging: None,
            contexts: Vec::new(),
            caches: Vec::new(),
        }
    }

    /// The builder's name for cache `id`: the same for every mount of it in this build.
    fn cache_name(&mut self, id: &[u8]) -> Vec<u8> {
        let index = match self.caches.iter().position(|c| c == id) {
            Some(i) => i,
            None => {
                self.caches.push(id.to_vec());
                self.caches.len() - 1
            }
        };
        index.to_string().into_bytes()
    }

    /// What BuildKit's solver finds before an operation reads `path` of `fs`, if `fs` is
    /// the build context: a content checksum, which fails for a path that names nothing,
    /// its symlinks followed; a wildcard may match nothing (solver/llbsolver
    /// NewContentHashFunc, cache/contenthash). Other inputs are not checksummed first, and
    /// fail as the operation reads them (measured against BuildKit, 2026-10-02). It names
    /// the context where BuildKit names its internal reference, and where several paths
    /// are missing, the first, where BuildKit's parallel checksums name any of them.
    fn checksummed(&self, fs: &Rc<Fs>, path: &[u8], wildcard: bool) -> Result<(), String> {
        if !self.contexts.iter().any(|c| Rc::ptr_eq(c, fs)) {
            return Ok(());
        }
        if wildcard && path.iter().any(|b| matches!(b, b'*' | b'?' | b'[')) {
            return Ok(());
        }
        let shown = if path.starts_with(b"/") {
            String::from_utf8_lossy(path).into_owned()
        } else {
            format!("/{}", String::from_utf8_lossy(path))
        };
        match fs.stat(path) {
            Ok(_) => Ok(()),
            Err(_) => Err(format!(
                "failed to compute cache key: failed to calculate checksum of ref context: {}: not found",
                shards_cmdline::go::quote(&shown)
            )),
        }
    }

    /// `layers` applied in order on `fs`, as the store stacks an image's layers; what
    /// they hold against the store's limits.
    fn apply(&mut self, tree: &mut Tree, layers: &[Layer]) -> Result<Tally, String> {
        let store_layers = layers
            .iter()
            .map(|l| {
                Ok(store::Layer {
                    blob: Digest::parse(&String::from_utf8_lossy(&l.digest)).map_err(err)?,
                    media_type: String::from_utf8_lossy(&l.media_type).into_owned(),
                    diff_id: Digest::parse(&String::from_utf8_lossy(&l.diff_id)).map_err(err)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let tars = self
            .store
            .unpack_layers(&store_layers, self.limits)
            .map_err(err)?;
        let (mut entries, mut metadata, mut bytes) = (0u64, 0u64, 0u64);
        let limits = *self.limits;
        for t in &tars {
            bytes = bytes.saturating_add(t.size().map_err(err)?);
            let id = self.sources.archive(t.open().map_err(err)?).map_err(err)?;
            let mut count = |e: &shards_image::tar::Entry| {
                entries += 1;
                let held = e
                    .xattrs
                    .iter()
                    .fold(e.path.len() + e.link.len(), |n, (k, v)| n + k.len() + v.len());
                metadata = metadata.saturating_add(held as u64);
                if entries > limits.entries || metadata > limits.metadata {
                    return Err(shards_image::Error::from(std::io::Error::other(
                        "the image passes its limits of entries or metadata (SHARDS_MAX_IMAGE_*)",
                    )));
                }
                Ok(())
            };
            layer::apply(tree, id, BufReader::new(t.open().map_err(err)?), &mut count).map_err(err)?;
            tree.compact_if_worth_it();
        }
        tree.compact();
        self.unpacked.extend(tars);
        Ok(Tally {
            entries,
            metadata,
            bytes,
        })
    }

    /// The stage ADD decompresses archives into, made at the first.
    fn unpack_stage(&mut self) -> Result<std::path::PathBuf, String> {
        if self.unpack.is_none() {
            self.unpack = Some(self.store.stage().map_err(err)?);
        }
        self.unpack
            .as_ref()
            .map(|s| s.path().to_path_buf())
            .ok_or_else(|| "no stage".to_string())
    }

    /// A base image's snapshot, from its layers: in a builder guest, its root filesystem
    /// on pmem device `base`, or else the whole tree, sent.
    pub fn image(&mut self, layers: Vec<Layer>, base: Option<u32>) -> Result<Ref, String> {
        let mut tree = layer::root();
        let tally = self.apply(&mut tree, &layers)?;
        let fs = Rc::new(Fs::new(tree, now()));
        let stack = Stack::layers(tally, fs.tree());
        let origin = match base {
            Some(n) => Origin::Image(n),
            None => Origin::Whole {
                id: origin_id(),
                fs: fs.clone(),
            },
        };
        Ok(Ref {
            fs,
            layers,
            stack,
            origin: Rc::new(origin),
        })
    }

    /// The build context's snapshot: what BuildKit would receive, read in place.
    pub fn context(
        &mut self,
        dir: &std::path::Path,
        filters: &shards_build::context::Filters,
    ) -> Result<Ref, String> {
        let stage = self.store.stage().map_err(err)?;
        let fs = shards_build::context::load(dir, filters, &mut self.sources, now(), stage.path())
            .map_err(|e| e.0)?;
        self.stages.push(stage);
        let fs = Rc::new(fs);
        self.contexts.push(fs.clone());
        Ok(Ref {
            fs: fs.clone(),
            layers: Vec::new(),
            stack: Stack::unknown("a build context, which no layers make"),
            origin: Rc::new(Origin::Whole { id: origin_id(), fs }),
        })
    }

    /// A directory that lasts as long as the build, for its downloads.
    pub fn stage(&mut self) -> Result<PathBuf, String> {
        let stage = self.store.stage().map_err(err)?;
        let path = stage.path().to_path_buf();
        self.stages.push(stage);
        Ok(path)
    }

    /// What an HTTP source makes of its download (BuildKit dockerfile/1.27.1 source/http,
    /// save): one file named as `name` is made safe, mode 0600, owned by root, with the
    /// download's mtime. Its bytes count against the build's budget for what ADD writes.
    pub fn downloaded(&mut self, d: super::http::Download, name: &[u8]) -> Result<Ref, String> {
        self.budget.fetched(d.size).map_err(|e| e.0)?;
        let name = super::http::safe_file_name(name);
        let data = self.sources.host(d.path, d.size).map_err(err)?;
        let mut fs = scratch();
        fs.put(
            &name,
            Node {
                kind: Kind::File { size: d.size, data },
                meta: Meta {
                    mode: 0o600,
                    // Without a Last-Modified, the time is 1970's, as BuildKit's save
                    // leaves it.
                    mtime: d.last_modified.map_or(0, |t| t.0),
                    mtime_nsec: d.last_modified.map_or(0, |t| t.1),
                    ..Meta::default()
                },
            },
        )
        .map_err(|e| e.to_string())?;
        fs.begin();
        let fs = Rc::new(fs);
        Ok(Ref {
            fs: fs.clone(),
            layers: Vec::new(),
            stack: Stack::unknown("a download, which no layers make"),
            origin: Rc::new(Origin::Whole { id: origin_id(), fs }),
        })
    }

    /// A source's tree as one snapshot (a Git checkout's): `root`'s directory holding
    /// `entries`, each path's parent before it, every directory's meta set again once what
    /// it holds is in. Its `bytes` count against the build's budget for what ADD writes.
    pub fn tree_of(&mut self, root: Meta, entries: Vec<(Vec<u8>, Node)>, bytes: u64) -> Result<Ref, String> {
        self.budget.fetched(bytes).map_err(|e| e.0)?;
        // Its clock is its root's time: what its entries change is stamped as the source
        // says, not as the build's clock would (the root itself, once it holds them).
        let clock = (root.mtime, root.mtime_nsec);
        let mut fs = Fs::new(Tree::new(root), clock);
        let mut dirs = Vec::new();
        for (path, node) in entries {
            if matches!(node.kind, Kind::Dir(_)) {
                fs.put_dir(&path, node.meta.clone(), false)
                    .map_err(|e| e.to_string())?;
                dirs.push((path, node.meta));
            } else {
                fs.put(&path, node).map_err(|e| e.to_string())?;
            }
        }
        for (path, meta) in dirs.into_iter().rev() {
            fs.set_meta(&path, meta).map_err(|e| e.to_string())?;
        }
        fs.begin();
        let fs = Rc::new(fs);
        Ok(Ref {
            fs: fs.clone(),
            layers: Vec::new(),
            stack: Stack::unknown("a source's tree, which no layers make"),
            origin: Rc::new(Origin::Whole { id: origin_id(), fs }),
        })
    }

    /// A merge (COPY --link's): the inputs' layers one after another, and the snapshot
    /// they stack to.
    pub fn merge(&mut self, inputs: &[Ref]) -> Result<Ref, String> {
        let Some((first, rest)) = inputs.split_first() else {
            let fs = scratch();
            let stack = Stack::layers(Tally::default(), fs.tree());
            return Ok(Ref {
                fs: Rc::new(fs),
                layers: Vec::new(),
                stack,
                origin: Rc::new(Origin::Scratch),
            });
        };
        let mut fs = (*first.fs).clone();
        let mut layers = first.layers.clone();
        let mut applied = Tally::default();
        for r in rest {
            applied = applied.plus(self.apply(&mut *fs.unrecorded_tree(), &r.layers)?);
            layers.extend(r.layers.iter().cloned());
        }
        fs.begin();
        let stack = first.stack.merge(applied, fs.tree());
        Ok(Ref {
            fs: Rc::new(fs),
            layers,
            stack,
            origin: Rc::new(Origin::Merge(inputs.iter().map(|r| r.origin.clone()).collect())),
        })
    }

    /// Commits a mount: its layer, the diff from its base, written to the store. Its
    /// origin is `origin`, or the step that made it from its base's.
    pub fn commit(
        &mut self,
        base: Option<Ref>,
        mut fs: Fs,
        description: &str,
        origin: Option<Origin>,
    ) -> Result<Ref, String> {
        let empty = scratch();
        let lower = base.as_ref().map_or(&empty, |b| &*b.fs);
        let mut w = self.store.writer().map_err(err)?;
        crate::phase("step");
        let mut record = diff::write_layer(lower, &fs, &mut self.sources, &mut w).map_err(|e| e.0)?;
        crate::phase("layer");
        let (digest, size) = w.commit().map_err(err)?;
        let data = std::mem::take(&mut record.data);
        crate::phase("blob");
        let stack = base
            .as_ref()
            .map_or_else(
                || Stack::layers(Tally::default(), lower.tree()),
                |b| b.stack.clone(),
            )
            .commit(lower, &fs, record, size, &mut self.sources)
            .map_err(|e| e.0)?;
        // The files the layer holds are read from it from now on: the stored bytes its
        // digest covers, not the build's copies, so the image has the layer's bytes. The
        // same bytes, read from elsewhere: no change of the step's.
        if !data.is_empty() {
            let blob = std::fs::File::open(self.store.blob_path(&digest)).map_err(err)?;
            let source = self.sources.archive(blob).map_err(err)?;
            for (id, offset) in data {
                if let Some(Node {
                    kind: Kind::File { data, .. },
                    ..
                }) = fs.unrecorded_tree().node_mut(id)
                {
                    *data = DataRef { source, offset };
                }
            }
        }
        let t = now();
        let mut created = Time::from_unix(t.0);
        created.nanosecond = t.1;
        let base_origin = base.as_ref().map(|b| b.origin.clone());
        let mut layers = base.map(|b| b.layers).unwrap_or_default();
        layers.push(Layer {
            media_type: LAYER_TAR.as_bytes().to_vec(),
            digest: digest.to_string().into_bytes(),
            size,
            diff_id: digest.to_string().into_bytes(),
            annotations: BTreeMap::new(),
            created: Some(created),
            description: description.as_bytes().to_vec(),
        });
        let step = fs.tree().step();
        fs.begin();
        let fs = Rc::new(fs);
        let origin = origin.unwrap_or_else(|| Origin::Step {
            id: origin_id(),
            parent: base_origin.unwrap_or_else(|| Rc::new(Origin::Scratch)),
            fs: fs.clone(),
            step,
        });
        Ok(Ref {
            fs,
            layers,
            stack,
            origin: Rc::new(origin),
        })
    }

    /// `r`'s snapshot, to write its image's root filesystem from, or why the export must
    /// stack its layers again instead: a stack not followed, or one past the store's
    /// limits, which `Store::rootfs` words.
    pub fn flat(&self, r: Ref) -> Result<Flat, String> {
        let tally = match &r.stack {
            Stack::Unknown(why) => return Err(why.to_string()),
            Stack::Known(_) => r.stack.tally().unwrap_or_default(),
        };
        let l = self.limits;
        if tally.entries > l.entries || tally.metadata > l.metadata || tally.bytes > l.bytes {
            return Err("an image at its limits".into());
        }
        // The snapshot itself, not a copy: a copy is another tree, whose node ids the
        // stack does not follow. Its origin goes first, since it may hold it too.
        drop(r.origin);
        let fs = Rc::try_unwrap(r.fs).map_err(|_| "a snapshot still shared".to_string())?;
        if !r.stack.follows(fs.tree()) {
            return Err("a snapshot other than the one followed".into());
        }
        Ok(Flat { fs, stack: r.stack })
    }

    /// The root filesystem of `layers`, the image of `flat`, as `Store::rootfs` builds
    /// it: the snapshot put in its layers' form, its files read where the build has them.
    pub fn rootfs(&mut self, flat: Flat, layers: &[store::Layer]) -> Result<PathBuf, String> {
        let Flat { mut fs, stack } = flat;
        let sources = &mut self.sources;
        self.store
            .rootfs_written(layers, self.limits, shards_build::stack::PRODUCER, |out| {
                stack
                    .finish(&mut fs)
                    .map_err(|why| shards_image::Error::from(std::io::Error::other(why.to_string())))?;
                fs.unrecorded_tree().drop_index();
                erofs::write(fs.tree(), sources, out)?;
                Ok(())
            })
            .map_err(err)
    }

    /// A file operation's outputs, its actions run as FileOpSolver runs them: each on a
    /// mount of its input, committed when it is an output or read twice.
    pub fn file(
        &mut self,
        inputs: &[Ref],
        actions: &[OpAction],
        description: &str,
    ) -> Result<Vec<Ref>, String> {
        let n = inputs.len();
        let total = n + actions.len();
        let mut uses = vec![0usize; total];
        let mut outputs: BTreeMap<i64, usize> = BTreeMap::new();
        for (i, a) in actions.iter().enumerate() {
            for idx in [a.input, a.secondary_input] {
                if let Ok(idx) = usize::try_from(idx) {
                    if idx >= total {
                        return Err(format!("invalid input index {idx}, {total} provided"));
                    }
                    if let Some(u) = uses.get_mut(idx) {
                        *u += 1;
                    }
                }
            }
            if a.output != -1 && outputs.insert(a.output, n + i).is_some() {
                return Err(format!("duplicate output {}", a.output));
            }
        }
        if outputs.is_empty() {
            return Err("no outputs specified".into());
        }
        let commit: Vec<bool> = (0..total)
            .map(|i| uses.get(i).copied().unwrap_or(0) > 1 || outputs.values().any(|&o| o == i))
            .collect();
        let mut slots: Vec<Slot> = inputs.iter().cloned().map(Slot::Ref).collect();
        slots.extend((0..actions.len()).map(|_| Slot::Taken));
        let mut done = vec![false; actions.len()];
        let mut out = Vec::new();
        for (expected, (&o, &idx)) in outputs.iter().enumerate() {
            if o != expected as i64 {
                return Err(format!("missing output index {expected}"));
            }
            self.run_action(
                idx,
                inputs.len(),
                actions,
                &commit,
                &mut slots,
                &mut done,
                description,
                &mut Vec::new(),
            )?;
            match slots.get(idx) {
                Some(Slot::Ref(r)) => out.push(r.clone()),
                _ => return Err(format!("output {o} was not committed")),
            }
        }
        Ok(out)
    }

    /// FileOpSolver's getInput for action slot `idx`: its input's mount, the action run on
    /// it, and the result committed or kept.
    #[allow(clippy::too_many_arguments)]
    fn run_action(
        &mut self,
        idx: usize,
        n: usize,
        actions: &[OpAction],
        commit: &[bool],
        slots: &mut Vec<Slot>,
        done: &mut [bool],
        description: &str,
        loading: &mut Vec<usize>,
    ) -> Result<(), String> {
        if idx < n || done.get(idx - n).copied().unwrap_or(true) {
            return Ok(());
        }
        if loading.contains(&idx) {
            return Err(format!("loop from index {idx}"));
        }
        loading.push(idx);
        let Some(a) = actions.get(idx - n) else {
            return Err(format!("invalid input index {idx}"));
        };
        for dep in [a.input, a.secondary_input] {
            if let Ok(dep) = usize::try_from(dep) {
                self.run_action(dep, n, actions, commit, slots, done, description, loading)?;
            }
        }
        loading.pop();
        // The mount to change: a fresh one over a committed input, or the input action's.
        let (base, mut fs) = match usize::try_from(a.input) {
            // Scratch, as any input is: its snapshot is what the mount copies and what the
            // commit takes the step's changes against, one tree.
            Err(_) => {
                let base = scratch_ref();
                let mut fs = (*base.fs).clone();
                fs.begin();
                (Some(base), fs)
            }
            Ok(i) => match slots.get_mut(i).map(|s| std::mem::replace(s, Slot::Taken)) {
                Some(Slot::Ref(r)) => {
                    if let Some(s) = slots.get_mut(i) {
                        *s = Slot::Ref(r.clone());
                    }
                    let mut fs = (*r.fs).clone();
                    fs.begin();
                    (Some(r), fs)
                }
                Some(Slot::Mount { base, fs }) => (base, *fs),
                _ => return Err(format!("input {i} is used twice uncommitted")),
            },
        };
        let owner = |c: &Option<OpChown>| c.clone();
        // The tree a user or group name is read from, borrowed: the action's own, as it
        // is so far, or another input's.
        fn user_fs<'s>(
            slots: &'s [Slot],
            c: &Option<OpChown>,
            which: bool,
            own: &'s Fs,
            own_input: i64,
        ) -> Result<Option<&'s Fs>, String> {
            let Some(c) = c else { return Ok(None) };
            let u = if which { &c.user } else { &c.group };
            match u {
                Some(OpUser::Name { input, .. }) => {
                    if usize::try_from(*input).ok() == usize::try_from(own_input).ok() {
                        return Ok(Some(own));
                    }
                    match usize::try_from(*input).ok().and_then(|i| slots.get(i)) {
                        Some(Slot::Ref(r)) => Ok(Some(&*r.fs)),
                        _ => Err(format!("invalid user index: {input}")),
                    }
                }
                _ => Ok(None),
            }
        }
        let chown = match &a.action {
            OpActionKind::Mkdir { owner: o, .. }
            | OpActionKind::Mkfile { owner: o, .. }
            | OpActionKind::Copy { owner: o, .. } => owner(o),
        };
        let ch = match &chown {
            None => Chown::Keep,
            Some(c) => {
                let users = user_fs(slots, &chown, true, &fs, a.input)?;
                let groups = user_fs(slots, &chown, false, &fs, a.input)?;
                ops::read_user(Some(c), users, groups, &mut self.sources).map_err(|e| e.0)?
            }
        };
        let r: Result<(), BuildError> = match &a.action {
            OpActionKind::Mkdir {
                path,
                mode,
                make_parents,
                timestamp,
                ..
            } => ops::mkdir(&mut fs, path, *mode, *make_parents, ch, *timestamp),
            OpActionKind::Mkfile {
                path,
                mode,
                data,
                timestamp,
                ..
            } => {
                let at = self.sources.bytes(data.clone()).map_err(err)?;
                ops::mkfile(&mut fs, path, *mode, (data.len() as u64, at), ch, *timestamp)
            }
            OpActionKind::Copy {
                src,
                dest,
                mode,
                mode_str,
                follow_symlink,
                dir_copy_contents,
                attempt_unpack,
                create_dest_path,
                allow_wildcard,
                allow_empty_wildcard,
                timestamp,
                include_patterns,
                exclude_patterns,
                ..
            } => {
                let from: Rc<Fs> = match usize::try_from(a.secondary_input) {
                    Err(_) => Rc::new(scratch()),
                    Ok(i) => match slots.get_mut(i).map(|s| std::mem::replace(s, Slot::Taken)) {
                        Some(Slot::Ref(r)) => {
                            let fs = r.fs.clone();
                            if let Some(s) = slots.get_mut(i) {
                                *s = Slot::Ref(r);
                            }
                            fs
                        }
                        Some(Slot::Mount { fs, .. }) => Rc::new(*fs),
                        _ => return Err(format!("input {i} is used twice uncommitted")),
                    },
                };
                self.checksummed(&from, src, *allow_wildcard)?;
                let action = CopyAction {
                    src: src.clone(),
                    dest: dest.clone(),
                    mode: *mode,
                    mode_str: mode_str.clone(),
                    follow_symlink: *follow_symlink,
                    dir_copy_contents: *dir_copy_contents,
                    attempt_unpack: *attempt_unpack,
                    create_dest_path: *create_dest_path,
                    allow_wildcard: *allow_wildcard,
                    allow_empty_wildcard: *allow_empty_wildcard,
                    timestamp: *timestamp,
                    include_patterns: include_patterns.clone(),
                    exclude_patterns: exclude_patterns.clone(),
                };
                let stage = self.unpack_stage()?;
                let mut io = shards_build::archive::Unpack {
                    sources: &mut self.sources,
                    stage: &stage,
                    budget: &mut self.budget,
                };
                ops::copy(&from, &mut fs, &action, ch, &mut io)
            }
        };
        r.map_err(|e| e.0)?;
        if let Some(d) = done.get_mut(idx - n) {
            *d = true;
        }
        let slot = if commit.get(idx).copied().unwrap_or(false) {
            Slot::Ref(self.commit(base, fs, description, None)?)
        } else {
            Slot::Mount {
                base,
                fs: Box::new(fs),
            }
        };
        if let Some(s) = slots.get_mut(idx) {
            *s = slot;
        }
        Ok(())
    }
}

/// A `RUN`'s operation, as the definition has it, and what the build allows it.
#[derive(Debug)]
pub struct RunOp<'o> {
    pub process: &'o shards_dockerfile::llb::Process,
    pub mounts: &'o [shards_dockerfile::llb::OpMount],
    pub network: shards_dockerfile::llb::NetMode,
    pub security: shards_dockerfile::llb::Security,
    /// Secrets as variables: secret id, the variable's name, whether it may be missing (as
    /// the plan gives them, `pb.SecretEnv`'s order).
    pub secret_env: &'o [(Vec<u8>, Vec<u8>, bool)],
    /// The build's secrets, by id, read as it began (`--secret`).
    pub secrets: &'o std::collections::BTreeMap<String, shards_cmdline::buildflags::SecretBytes>,
    /// `--allow security.insecure` and `--allow network.host`.
    pub insecure: bool,
    pub network_host: bool,
    /// The seccomp filter every step but an insecure one runs under
    /// (crate::setup::step_seccomp).
    pub seccomp: &'o [u8],
    /// The SSH agents `--ssh` forwards, by id.
    pub agents: &'o super::Agents,
    /// Its limits, its op's (`--memory`, `--cpu-shares` and the like).
    pub resources: Option<&'o shards_dockerfile::llb::LinuxResources>,
}

/// Cgroup v2 files and what each holds, in the order written.
pub type CgroupFiles = Vec<(Vec<u8>, Vec<u8>)>;

/// runc's `ConvertCPUSharesToCgroupV2Value`, as the runc Docker 29.3.1 ships converts
/// `--cpu-shares` (runc v1.3.4, opencontainers/cgroups v0.0.4 utils.go): 1 to 10000 on a
/// quadratic of the shares' logarithm, 100 for the default 1024; 0 unset.
pub fn cpu_weight(shares: u64) -> u64 {
    match shares {
        0 => 0,
        1..=2 => 1,
        262_144.. => 10_000,
        _ => {
            #[allow(clippy::cast_precision_loss)]
            let l = (shares as f64).log2();
            let exponent = (l * l + 125.0 * l) / 612.0 - 7.0 / 34.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let w = 10f64.powf(exponent).ceil() as u64;
            w
        }
    }
}

/// The cgroup v2 files a step's limits are, as runc writes them (opencontainers/cgroups
/// v0.0.4 fs2/memory.go `setMemory`, fs2/cpu.go `setCPU`, fs2/cpuset.go): the swap first,
/// memory, the CPU's weight and maximum, the CPUs and memory nodes. A swap past the
/// memory alone is refused as runc refuses it.
pub fn cgroup_files(r: &shards_dockerfile::llb::LinuxResources) -> Result<CgroupFiles, String> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.as_bytes().to_vec(), v.into_bytes()));
    // ConvertMemorySwapToCgroupV2Value, memory never -1 here (dockerui takes it > 0).
    let swap = match (r.memory_swap, r.memory) {
        (-1 | 0, _) => r.memory_swap,
        (_, 0) => return Err("unable to set swap limit without memory limit".into()),
        (s, m) if s < m => return Err("memory+swap limit should be >= memory limit".into()),
        (s, m) => s - m,
    };
    match swap {
        -1 => put("memory.swap.max", "max".into()),
        0 if r.memory_swap > 0 => put("memory.swap.max", "0".into()),
        0 => {}
        n => put("memory.swap.max", n.to_string()),
    }
    if r.memory > 0 {
        put("memory.max", r.memory.to_string());
    }
    let weight = cpu_weight(r.cpu_shares);
    if weight != 0 {
        put("cpu.weight", weight.to_string());
    }
    if r.cpu_quota != 0 || r.cpu_period != 0 {
        let quota = if r.cpu_quota > 0 {
            r.cpu_quota.to_string()
        } else {
            "max".into()
        };
        let period = if r.cpu_period == 0 { 100_000 } else { r.cpu_period };
        put("cpu.max", format!("{quota} {period}"));
    }
    if !r.cpuset_cpus.is_empty() {
        put(
            "cpuset.cpus",
            String::from_utf8_lossy(&r.cpuset_cpus).into_owned(),
        );
    }
    if !r.cpuset_mems.is_empty() {
        put(
            "cpuset.mems",
            String::from_utf8_lossy(&r.cpuset_mems).into_owned(),
        );
    }
    Ok(out)
}

/// An SSH mount's agent id: `default` where it names none, as BuildKit's sshforward
/// reads an empty one (DefaultID).
fn ssh_id(id: &[u8]) -> String {
    if id.is_empty() {
        "default".into()
    } else {
        String::from_utf8_lossy(id).into_owned()
    }
}

/// A token of an SSH mount's own, from the kernel's random source, which its relay
/// presents to the host.
#[cfg(unix)]
fn ssh_token() -> Result<[u8; 16], String> {
    let mut token = [0u8; 16];
    shards_net::entropy(&mut token).map_err(|e| format!("an SSH mount's token: {e}"))?;
    Ok(token)
}

/// No builder runs on Windows yet (builder.rs), so no step there reaches an agent.
#[cfg(not(unix))]
fn ssh_token() -> Result<[u8; 16], String> {
    Err("RUN steps run in a builder microVM, which shards starts on Linux and macOS hosts only so far".into())
}

/// Secret `id`'s bytes, if the build was given it.
fn secret<'s>(op: &'s RunOp<'_>, id: &[u8]) -> Option<&'s [u8]> {
    let id = std::str::from_utf8(id).ok()?;
    op.secrets.get(id).map(|s| s.bytes())
}

/// Linux's resource numbers, the same on every architecture shards runs, by the names
/// `--ulimit` takes (client/llb/exec.go).
const RLIMITS: [(&str, u32); 16] = [
    ("cpu", 0),
    ("fsize", 1),
    ("data", 2),
    ("stack", 3),
    ("core", 4),
    ("rss", 5),
    ("nproc", 6),
    ("nofile", 7),
    ("memlock", 8),
    ("as", 9),
    ("locks", 10),
    ("sigpending", 11),
    ("msgqueue", 12),
    ("nice", 13),
    ("rtprio", 14),
    ("rttime", 15),
];

impl Exec<'_> {
    /// A user file of `fs` as BuildKit opens one (executor/oci/user.go): resolved within
    /// the snapshot, a regular file, at most 10 MiB; `None` if it cannot be opened.
    fn user_file(&mut self, fs: &Fs, path: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let Ok(id) = fs.stat(path) else { return Ok(None) };
        let Some(Node {
            kind: Kind::File { size, data },
            ..
        }) = fs.node(id)
        else {
            return Ok(None);
        };
        if *size > shards_user::BUILDKIT_USER_FILE as u64 {
            return Err(format!(
                "{:?} exceeds {} bytes",
                String::from_utf8_lossy(path),
                shards_user::BUILDKIT_USER_FILE
            ));
        }
        let mut buf = vec![0u8; *size as usize];
        if *size > 0 {
            erofs::Source::read_at(&mut self.sources, *data, 0, &mut buf).map_err(err)?;
        }
        Ok(Some(buf))
    }

    /// Where a step's changes are kept: one file of the build's, read as a source.
    fn staging(&mut self) -> Result<(std::fs::File, u32, u64), String> {
        if let Some((f, source, at)) = &self.run_staging {
            return Ok((f.try_clone().map_err(err)?, *source, *at));
        }
        let stage = self.store.stage().map_err(err)?;
        let path = stage.path().join("run-changes");
        let f = std::fs::File::options()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.stages.push(stage);
        let source = self.sources.archive(f.try_clone().map_err(err)?).map_err(err)?;
        self.run_staging = Some((f.try_clone().map_err(err)?, source, 0));
        Ok((f, source, 0))
    }

    /// Runs a `RUN` in `builder` as BuildKit runs it (docs/research/buildkit-run.md), its
    /// output to `out` as it comes; its result is its root mount's.
    pub fn run(
        &mut self,
        builder: &mut super::builder::Builder,
        inputs: &[Ref],
        op: &RunOp<'_>,
        description: &str,
        out: &mut dyn FnMut(u8, &[u8]),
    ) -> Result<Vec<Ref>, String> {
        use shards_abi::build::{Mount, Network, Step};
        use shards_dockerfile::llb::{NetMode, OpMountKind, Security};

        let p = op.process;
        let input = |i: i64| -> Result<Ref, String> {
            match usize::try_from(i) {
                Ok(i) => inputs
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("invalid input index {i}")),
                Err(_) => Ok(scratch_ref()),
            }
        };
        let root_mount = op
            .mounts
            .iter()
            .find(|m| m.dest == b"/")
            .ok_or("a command with no root mount")?;
        // Only a bind mount has an output: a secret's, a cache's or a tmpfs's index is
        // protobuf's default 0, which BuildKit's solver never reads (exec.go, Marshal).
        if op
            .mounts
            .iter()
            .any(|m| m.dest != b"/" && m.output >= 0 && matches!(m.kind, OpMountKind::Bind))
        {
            return Err("a command's output other than its root".into());
        }
        let root = input(root_mount.input)?;
        let fail = |why: &str| super::step::failure(&p.args, why);
        if op.security == Security::Insecure && !op.insecure {
            return Err("security.insecure is not allowed".into());
        }
        let network = match op.network {
            NetMode::Host if !op.network_host => return Err("network.host is not allowed".into()),
            NetMode::None => Network::None,
            // The builder's own network: BuildKit's default gives a step its host's, and
            // the builder VM is the host it has; host networking is the same, granted.
            NetMode::Sandbox | NetMode::Host => Network::Default,
        };
        let passwd = self.user_file(&root.fs, b"/etc/passwd");
        let group = self.user_file(&root.fs, b"/etc/group");
        let user = match (&passwd, &group) {
            (Ok(pw), Ok(gr)) => shards_user::buildkit(&p.user, pw.as_deref(), gr.as_deref()),
            (Err(e), _) | (_, Err(e)) if !p.user.is_empty() => Err(e.clone()),
            _ => shards_user::buildkit(b"", None, None),
        }
        .map_err(|e| fail(&e))?;
        // Secrets as variables: the build's, and a missing one empty or, when required, an
        // error (solver/llbsolver/ops/exec.go, loadSecretEnv).
        let mut secrets = Vec::new();
        for (id, name, optional) in op.secret_env {
            match secret(op, id) {
                Some(value) => secrets.push((name.clone(), value.to_vec())),
                None if *optional => secrets.push((name.clone(), Vec::new())),
                None => return Err(format!("secret {}: not found", String::from_utf8_lossy(id))),
            }
        }
        let env = super::step::env(&p.env, p.proxy.as_ref(), &secrets);
        let env = shards_user::prepare_env(&env, user.uid, passwd.ok().flatten().as_deref())
            .map_err(|e| fail(&e))?;
        let hostname = if p.hostname.is_empty() {
            super::step::HOSTNAME.to_vec()
        } else {
            p.hostname.clone()
        };
        let resolv = super::step::resolv(&super::step::host_resolv(), true);
        let mut mounts = Vec::new();
        for m in op.mounts.iter().filter(|m| m.dest != b"/") {
            let mount = match &m.kind {
                OpMountKind::Bind => {
                    let r = input(m.input)?;
                    self.checksummed(&r.fs, &m.selector, false)?;
                    Mount::Tree {
                        tree: builder.tree(&r.origin, &mut self.sources)?,
                        subpath: m.selector.clone(),
                        writable: !m.readonly,
                    }
                }
                OpMountKind::Cache { id, .. } => {
                    // A cache's first content and owner: its source's directory, if any.
                    let (mode, uid, gid) = match usize::try_from(m.input) {
                        Ok(_) => {
                            let r = input(m.input)?;
                            let sel = if m.selector.is_empty() {
                                b"/".to_vec()
                            } else {
                                m.selector.clone()
                            };
                            let meta =
                                r.fs.stat(&sel)
                                    .ok()
                                    .and_then(|id| r.fs.node(id))
                                    .map(|n| n.meta.clone())
                                    .unwrap_or_default();
                            (u32::from(meta.mode), meta.uid, meta.gid)
                        }
                        Err(_) => (0o755, 0, 0),
                    };
                    Mount::Cache {
                        id: self.cache_name(id),
                        mode,
                        uid,
                        gid,
                        readonly: m.readonly,
                    }
                }
                OpMountKind::Tmpfs { size } => Mount::Tmpfs {
                    size: u64::try_from(*size).unwrap_or(0),
                    readonly: m.readonly,
                },
                OpMountKind::Secret {
                    id,
                    uid,
                    gid,
                    mode,
                    optional,
                } => match secret(op, id) {
                    Some(data) => Mount::Secret {
                        data: data.to_vec(),
                        mode: *mode,
                        uid: *uid,
                        gid: *gid,
                    },
                    None if *optional => continue,
                    None => return Err(format!("secret {}: not found", String::from_utf8_lossy(id))),
                },
                OpMountKind::Ssh {
                    id, uid, gid, mode, ..
                } if op.agents.contains_key(&ssh_id(id)) => {
                    // A token of this mount's own, which its relay presents to the host.
                    let token = ssh_token()?;
                    Mount::Ssh {
                        id: ssh_id(id).into_bytes(),
                        mode: *mode,
                        uid: *uid,
                        gid: *gid,
                        token,
                    }
                }
                OpMountKind::Ssh { id, optional, .. } => {
                    if *optional {
                        continue;
                    }
                    return Err(format!("no SSH key {:?} forwarded from the client", ssh_id(id)));
                }
            };
            mounts.push((m.dest.clone(), mount));
        }
        let mut rlimits = Vec::new();
        for u in &p.ulimits {
            let name = String::from_utf8_lossy(&u.name).to_lowercase();
            let resource = RLIMITS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| *r)
                .ok_or_else(|| format!("invalid ulimit name {name}"))?;
            let limit = |v: i64| u64::try_from(v).unwrap_or(u64::MAX);
            rlimits.push((resource, limit(u.soft), limit(u.hard)));
        }
        let step = Step {
            root: builder.tree(&root.origin, &mut self.sources)?,
            upper: 0,
            argv: p.args.clone(),
            env,
            cwd: if p.cwd.is_empty() {
                b"/".to_vec()
            } else {
                p.cwd.clone()
            },
            uid: user.uid,
            gid: user.gid,
            groups: user.groups,
            hosts: super::step::hosts(&hostname, &p.extra_hosts),
            hostname,
            resolv,
            network,
            insecure: op.security == Security::Insecure,
            mounts,
            rlimits,
            cgroup: match op.resources {
                Some(r) => cgroup_files(r)?,
                None => Vec::new(),
            },
            // BuildKit leaves an insecure step unconfined, as runc does a privileged
            // container (oci/spec.go: WithDefaultSeccomp unless insecure).
            seccomp: if op.security == Security::Insecure {
                Vec::new()
            } else {
                op.seccomp.to_vec()
            },
        };
        let mut fs = (*root.fs).clone();
        fs.begin();
        fs.now = now();
        let (mut staging, source, at) = self.staging()?;
        let mut applier = shards_build::upper::Applier::new(&mut fs, &mut staging, source, at);
        let (ended, layer) = builder.run(step, out, &mut applier, op.agents)?;
        match ended {
            super::builder::Ended::Status(0) => {}
            super::builder::Ended::Status(n) => return Err(fail(&format!("exit code: {n}"))),
            super::builder::Ended::NotRun(why) => return Err(fail(&why)),
        }
        let at = applier.finish().map_err(|e| e.0)?;
        if let Some((_, _, end)) = &mut self.run_staging {
            *end = at;
        }
        let origin = Origin::Run {
            parent: root.origin.clone(),
            layer,
        };
        Ok(vec![self.commit(Some(root), fs, description, Some(origin))?])
    }
}

#[cfg(test)]
mod tests {
    /// runc's cpu.weight for every --cpu-shares value, on this platform's floating point:
    /// Go's math is its own, this one's the platform's.
    #[test]
    fn cpu_weights_are_runcs() {
        let table = include_str!("testdata/cpu-weight.txt");
        let points: Vec<(u64, u64)> = table
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let (s, w) = l.split_once(' ').unwrap();
                (s.parse().unwrap(), w.parse().unwrap())
            })
            .collect();
        assert_eq!(cpu_weight(0), 0);
        for (i, &(from, weight)) in points.iter().enumerate() {
            let to = points.get(i + 1).map_or(262_145, |p| p.0 - 1);
            for shares in from..=to {
                assert_eq!(cpu_weight(shares), weight, "shares {shares}");
            }
        }
    }

    #[test]
    fn limits_are_written_as_runc_writes_them() {
        use shards_dockerfile::llb::LinuxResources;
        let files = |r: LinuxResources| {
            cgroup_files(&r).map(|f| {
                f.into_iter()
                    .map(|(k, v)| format!("{}={}", String::from_utf8_lossy(&k), String::from_utf8_lossy(&v)))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            files(LinuxResources {
                memory: 64 << 20,
                memory_swap: 96 << 20,
                cpu_shares: 512,
                cpu_quota: 50_000,
                cpuset_cpus: b"0-1".to_vec(),
                ..Default::default()
            })
            .unwrap(),
            [
                "memory.swap.max=33554432",
                "memory.max=67108864",
                "cpu.weight=59",
                "cpu.max=50000 100000",
                "cpuset.cpus=0-1"
            ]
        );
        assert_eq!(
            files(LinuxResources {
                memory: 64 << 20,
                memory_swap: -1,
                ..Default::default()
            })
            .unwrap(),
            ["memory.swap.max=max", "memory.max=67108864"]
        );
        assert_eq!(
            files(LinuxResources {
                memory: 64 << 20,
                memory_swap: 64 << 20,
                ..Default::default()
            })
            .unwrap(),
            ["memory.swap.max=0", "memory.max=67108864"]
        );
        assert_eq!(
            files(LinuxResources {
                cpu_period: 50_000,
                ..Default::default()
            })
            .unwrap(),
            ["cpu.max=max 50000"]
        );
        assert_eq!(
            files(LinuxResources {
                memory: 64 << 20,
                memory_swap: 32 << 20,
                ..Default::default()
            })
            .unwrap_err(),
            "memory+swap limit should be >= memory limit"
        );
        assert_eq!(
            files(LinuxResources {
                memory_swap: 32 << 20,
                ..Default::default()
            })
            .unwrap_err(),
            "unable to set swap limit without memory limit"
        );
    }

    use super::*;

    /// Every resource `--ulimit` takes is one the builder can set, by Linux's number for
    /// it: go-units' names, and `as` (shards_cmdline::buildflags::ULIMITS).
    #[test]
    fn every_ulimit_names_a_resource() {
        for name in shards_cmdline::buildflags::ULIMITS {
            assert!(RLIMITS.iter().any(|&(n, _)| n == name), "{name}");
        }
        let mut numbers: Vec<u32> = RLIMITS.iter().map(|&(_, n)| n).collect();
        numbers.sort_unstable();
        assert_eq!(
            numbers,
            (0..16).collect::<Vec<_>>(),
            "RLIMIT_CPU to RLIMIT_RTTIME, once each"
        );
    }
}
