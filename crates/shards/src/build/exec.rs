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
            tree.compact();
        }
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

    /// A base image's snapshot, from its layers.
    pub fn image(&mut self, layers: Vec<Layer>) -> Result<Ref, String> {
        let mut tree = layer::root();
        let tally = self.apply(&mut tree, &layers)?;
        let fs = Fs::new(tree, now());
        let stack = Stack::layers(tally, fs.tree());
        Ok(Ref {
            fs: Rc::new(fs),
            layers,
            stack,
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
        Ok(Ref {
            fs: Rc::new(fs),
            layers: Vec::new(),
            stack: Stack::unknown("a build context, which no layers make"),
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
        })
    }

    /// Commits a mount: its layer, the diff from its base, written to the store.
    fn commit(&mut self, base: Option<Ref>, mut fs: Fs, description: &str) -> Result<Ref, String> {
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
        fs.begin();
        Ok(Ref {
            fs: Rc::new(fs),
            layers,
            stack,
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
        // stack does not follow.
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
            Err(_) => (None, scratch()),
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
        let user_fs = |slots: &Vec<Slot>,
                       c: &Option<OpChown>,
                       which: bool,
                       own: &Fs|
         -> Result<Option<Rc<Fs>>, String> {
            let Some(c) = c else { return Ok(None) };
            let u = if which { &c.user } else { &c.group };
            match u {
                Some(OpUser::Name { input, .. }) => {
                    if usize::try_from(*input).ok() == usize::try_from(a.input).ok() {
                        return Ok(Some(Rc::new(own.clone())));
                    }
                    match usize::try_from(*input).ok().and_then(|i| slots.get(i)) {
                        Some(Slot::Ref(r)) => Ok(Some(r.fs.clone())),
                        _ => Err(format!("invalid user index: {input}")),
                    }
                }
                _ => Ok(None),
            }
        };
        let chown = match &a.action {
            OpActionKind::Mkdir { owner: o, .. }
            | OpActionKind::Mkfile { owner: o, .. }
            | OpActionKind::Copy { owner: o, .. } => owner(o),
        };
        let ch = match &chown {
            None => Chown::Keep,
            Some(c) => {
                let users = user_fs(slots, &chown, true, &fs)?;
                let groups = user_fs(slots, &chown, false, &fs)?;
                ops::read_user(Some(c), users.as_deref(), groups.as_deref(), &mut self.sources)
                    .map_err(|e| e.0)?
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
            Slot::Ref(self.commit(base, fs, description)?)
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
