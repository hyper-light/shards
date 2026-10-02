//! The build protocol: how `shards build` runs `RUN` steps in a builder guest
//! (docs/design/architecture.md D34). The guest dials the host on [`PORT`] once it is up,
//! and the one connection carries everything, in the run protocol's frames
//! ([`crate::run::header`]).
//!
//! The guest holds layers: directories of changes overlayfs stacks, each made once and
//! never changed, named by the host. The host sends a layer as a stream of changes
//! ([`crate::changes`]) in [`kind::LAYER`] frames; each step names its trees as stacks of
//! layers over a base image, so the guest keeps no tree of its own. A step's output is
//! its stdout and stderr ([`crate::run::kind`]), its status, and, if it succeeded, what it
//! changed, which the guest keeps as the layer the step names.

use alloc::vec::Vec;

/// The host port the guest dials.
pub const PORT: u32 = 1026;

/// Frame kinds, beside the run protocol's `STDOUT`, `STDERR`, `SYSTEM_ERR` and `EXIT`.
pub mod kind {
    /// Host to guest: a big-endian u32 layer id, then the next bytes of that layer's
    /// stream. A layer's frames come together, and its stream ends it.
    pub const LAYER: u8 = 32;
    /// Host to guest: a [`Step`](super::Step).
    pub const STEP: u8 = 33;
    /// Guest to host, after a step's `EXIT` of 0: the next bytes of what the step
    /// changed, as a stream that ends it.
    pub const CHANGES: u8 = 34;
    /// Guest to host: the layer whose stream just ended is written.
    pub const LAYERED: u8 = 35;
}

/// A tree: layers stacked over a base image, the last layer on top.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// Which virtio-pmem device holds the base image, as booted (`/dev/pmem<n>`); `None`
    /// for an empty base.
    pub base: Option<u32>,
    pub layers: Vec<u32>,
}

/// What a step mounts where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A tree, or the directory `subpath` of it. Writable only if `writable`, its writes
    /// then discarded with the step.
    Tree {
        tree: Tree,
        subpath: Vec<u8>,
        writable: bool,
    },
    /// A tmpfs of `size` bytes, or the kernel's default with 0.
    Tmpfs { size: u64, readonly: bool },
    /// A file holding `data`, of mode `mode` owned `uid:gid`, read-only.
    Secret {
        data: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    /// A directory the guest keeps by `id` across the steps of one build, made with
    /// `mode` and `uid:gid` the first time.
    Cache {
        id: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
        readonly: bool,
    },
}

/// Where a step's process may reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Network {
    /// Its own loopback alone.
    #[default]
    None,
}

/// A step: what to run, as whom, where, on what.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    pub root: Tree,
    /// The layer the step's changes become, if it succeeds.
    pub upper: u32,
    pub argv: Vec<Vec<u8>>,
    /// `KEY=value` entries, final: the host has applied every default.
    pub env: Vec<Vec<u8>>,
    /// Absolute.
    pub cwd: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups, the primary among them.
    pub groups: Vec<u32>,
    pub hostname: Vec<u8>,
    /// The bytes of `/etc/hosts` and `/etc/resolv.conf`, mounted read-only over the tree's.
    pub hosts: Vec<u8>,
    pub resolv: Vec<u8>,
    pub network: Network,
    /// `--security=insecure`: every capability, nothing masked.
    pub insecure: bool,
    /// Absolute targets, and what is mounted there, in order.
    pub mounts: Vec<(Vec<u8>, Mount)>,
    /// `--ulimit`s: Linux's resource number (the same on every architecture shards runs),
    /// the soft and the hard limit; the rest are inherited.
    pub rlimits: Vec<(u32, u64, u64)>,
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(out, u32::try_from(n).unwrap_or(u32::MAX));
}

fn put_bytes(out: &mut Vec<u8>, s: &[u8]) {
    put_len(out, s.len());
    out.extend_from_slice(s);
}

fn put_tree(out: &mut Vec<u8>, t: &Tree) {
    match t.base {
        Some(n) => {
            out.push(1);
            put_u32(out, n);
        }
        None => out.push(0),
    }
    put_len(out, t.layers.len());
    for &l in &t.layers {
        put_u32(out, l);
    }
}

impl Step {
    /// Big-endian u32s; byte strings and lists are a u32 length, then their items.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_tree(&mut out, &self.root);
        put_u32(&mut out, self.upper);
        for list in [&self.argv, &self.env] {
            put_len(&mut out, list.len());
            for s in list {
                put_bytes(&mut out, s);
            }
        }
        put_bytes(&mut out, &self.cwd);
        put_u32(&mut out, self.uid);
        put_u32(&mut out, self.gid);
        put_len(&mut out, self.groups.len());
        for &g in &self.groups {
            put_u32(&mut out, g);
        }
        for s in [&self.hostname, &self.hosts, &self.resolv] {
            put_bytes(&mut out, s);
        }
        out.push(match self.network {
            Network::None => 0,
        });
        out.push(u8::from(self.insecure));
        put_len(&mut out, self.mounts.len());
        for (target, m) in &self.mounts {
            put_bytes(&mut out, target);
            match m {
                Mount::Tree {
                    tree,
                    subpath,
                    writable,
                } => {
                    out.push(0);
                    put_tree(&mut out, tree);
                    put_bytes(&mut out, subpath);
                    out.push(u8::from(*writable));
                }
                Mount::Tmpfs { size, readonly } => {
                    out.push(1);
                    out.extend_from_slice(&size.to_be_bytes());
                    out.push(u8::from(*readonly));
                }
                Mount::Secret { data, mode, uid, gid } => {
                    out.push(2);
                    put_bytes(&mut out, data);
                    put_u32(&mut out, *mode);
                    put_u32(&mut out, *uid);
                    put_u32(&mut out, *gid);
                }
                Mount::Cache {
                    id,
                    mode,
                    uid,
                    gid,
                    readonly,
                } => {
                    out.push(3);
                    put_bytes(&mut out, id);
                    put_u32(&mut out, *mode);
                    put_u32(&mut out, *uid);
                    put_u32(&mut out, *gid);
                    out.push(u8::from(*readonly));
                }
            }
        }
        put_len(&mut out, self.rlimits.len());
        for &(resource, soft, hard) in &self.rlimits {
            put_u32(&mut out, resource);
            out.extend_from_slice(&soft.to_be_bytes());
            out.extend_from_slice(&hard.to_be_bytes());
        }
        out
    }

    /// The step in `bytes`, or `None` unless they hold exactly one.
    pub fn decode(bytes: &[u8]) -> Option<Step> {
        let mut r = Cursor(bytes);
        let root = r.tree()?;
        let upper = r.u32()?;
        let argv = r.list()?;
        let env = r.list()?;
        let cwd = r.bytes()?;
        let uid = r.u32()?;
        let gid = r.u32()?;
        let n = r.count(4)?;
        let mut groups = Vec::with_capacity(n);
        for _ in 0..n {
            groups.push(r.u32()?);
        }
        let hostname = r.bytes()?;
        let hosts = r.bytes()?;
        let resolv = r.bytes()?;
        let network = match r.u8()? {
            0 => Network::None,
            _ => return None,
        };
        let insecure = r.flag()?;
        let n = r.count(5)?;
        let mut mounts = Vec::with_capacity(n);
        for _ in 0..n {
            let target = r.bytes()?;
            let m = match r.u8()? {
                0 => Mount::Tree {
                    tree: r.tree()?,
                    subpath: r.bytes()?,
                    writable: r.flag()?,
                },
                1 => Mount::Tmpfs {
                    size: u64::from_be_bytes(r.take(8)?.try_into().ok()?),
                    readonly: r.flag()?,
                },
                2 => Mount::Secret {
                    data: r.bytes()?,
                    mode: r.u32()?,
                    uid: r.u32()?,
                    gid: r.u32()?,
                },
                3 => Mount::Cache {
                    id: r.bytes()?,
                    mode: r.u32()?,
                    uid: r.u32()?,
                    gid: r.u32()?,
                    readonly: r.flag()?,
                },
                _ => return None,
            };
            mounts.push((target, m));
        }
        let n = r.count(20)?;
        let mut rlimits = Vec::with_capacity(n);
        for _ in 0..n {
            let resource = r.u32()?;
            let soft = u64::from_be_bytes(r.take(8)?.try_into().ok()?);
            let hard = u64::from_be_bytes(r.take(8)?.try_into().ok()?);
            rlimits.push((resource, soft, hard));
        }
        r.0.is_empty().then_some(Step {
            root,
            upper,
            argv,
            env,
            cwd,
            uid,
            gid,
            groups,
            hostname,
            hosts,
            resolv,
            network,
            insecure,
            mounts,
            rlimits,
        })
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    /// A count of items each at least `min` bytes long: one beyond what is left is a lie.
    fn count(&mut self, min: usize) -> Option<usize> {
        let n = usize::try_from(self.u32()?).ok()?;
        (n <= self.0.len() / min).then_some(n)
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = usize::try_from(self.u32()?).ok()?;
        self.take(n).map(<[u8]>::to_vec)
    }

    fn list(&mut self) -> Option<Vec<Vec<u8>>> {
        let n = self.count(4)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.bytes()?);
        }
        Some(out)
    }

    fn tree(&mut self) -> Option<Tree> {
        let base = match self.u8()? {
            0 => None,
            1 => Some(self.u32()?),
            _ => return None,
        };
        let n = self.count(4)?;
        let mut layers = Vec::with_capacity(n);
        for _ in 0..n {
            layers.push(self.u32()?);
        }
        Some(Tree { base, layers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_step_reads_back_as_written_and_nothing_else_does() {
        let step = Step {
            root: Tree {
                base: Some(0),
                layers: vec![1, 2, 7],
            },
            upper: 8,
            argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo hi".to_vec()],
            env: vec![b"PATH=/bin".to_vec()],
            cwd: b"/src".to_vec(),
            uid: 1000,
            gid: 1000,
            groups: vec![1000, 27],
            hostname: b"buildkitsandbox".to_vec(),
            hosts: b"127.0.0.1\tlocalhost buildkitsandbox\n".to_vec(),
            resolv: b"nameserver 8.8.8.8\n".to_vec(),
            network: Network::None,
            insecure: false,
            mounts: vec![
                (
                    b"/ctx".to_vec(),
                    Mount::Tree {
                        tree: Tree {
                            base: None,
                            layers: vec![3],
                        },
                        subpath: b"/sub".to_vec(),
                        writable: true,
                    },
                ),
                (
                    b"/tmp".to_vec(),
                    Mount::Tmpfs {
                        size: 1 << 20,
                        readonly: false,
                    },
                ),
                (
                    b"/run/secrets/token".to_vec(),
                    Mount::Secret {
                        data: b"s3cr3t".to_vec(),
                        mode: 0o400,
                        uid: 0,
                        gid: 0,
                    },
                ),
                (
                    b"/root/.cache".to_vec(),
                    Mount::Cache {
                        id: b"/root/.cache".to_vec(),
                        mode: 0o755,
                        uid: 0,
                        gid: 0,
                        readonly: false,
                    },
                ),
            ],
            rlimits: vec![(7, 1024, 4096)],
        };
        let bytes = step.encode();
        assert_eq!(Step::decode(&bytes), Some(step));
        for cut in 0..bytes.len() {
            assert_eq!(Step::decode(&bytes[..cut]), None, "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Step::decode(&longer), None);
    }
}
