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

/// The host port a step's SSH agent socket is relayed to (`RUN --mount=type=ssh`): each
/// connection opens with the step's token for the mount and the agent's id, which the host
/// checks before it reaches the agent, so that no process of the guest's reaches it by
/// the port alone.
pub const SSH_PORT: u32 = 1028;

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
    /// Guest to host, after a step's `CHANGES`: the next bytes of what it changed in its
    /// next output mount (`Step::outputs`, in order), as a stream that `MOUNT_END` ends.
    pub const MOUNT_CHANGES: u8 = 36;
    /// Guest to host: the output mount's changes are all sent.
    pub const MOUNT_END: u8 = 37;
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
    /// `mode` and `uid:gid` the first time. `id` is the host's short name for the cache,
    /// one per cache id the build's steps give, however long theirs is.
    Cache {
        id: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
        readonly: bool,
    },
    /// A socket of mode `mode` owned `uid:gid`, relayed to the client's SSH agent `id` with
    /// `token`, the host's for this mount of this step.
    Ssh {
        id: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
        token: [u8; 16],
    },
}

/// Where a step's process may reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Network {
    /// Its own loopback alone.
    #[default]
    None,
    /// The builder's network, as BuildKit's steps have their host's.
    Default,
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
    /// The seccomp filter it runs under, as a run's `seccomp=` setup entry holds one (its
    /// seccomp(2) flags, little-endian, then its `struct sock_filter`s); empty for none.
    pub seccomp: Vec<u8>,
    /// Its limits, as cgroup v2 files and what each holds, in the order written; where
    /// any, it runs in a cgroup of its own.
    pub cgroup: Vec<(Vec<u8>, Vec<u8>)>,
    /// The writable tree mounts whose changes are outputs of the step's, as LLB's a bind
    /// mount's can be (an SBOM scan's `/run/out`, D81): each its index in `mounts` and the
    /// layer its changes become.
    pub outputs: Vec<(u32, u32)>,
    /// The device nodes its CDI devices bring (D96), made in its `/dev`.
    pub devices: Vec<Device>,
}

/// A device node a step is given: its path in the step, the guest's device it is (`from`,
/// the path itself where empty; never a host's numbers, which name nothing in a guest),
/// the kind it must be (`c`, `b`, a FIFO `p`, or 0 for the guest device's own), and its
/// mode (the guest device's where `None`) and owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: Vec<u8>,
    pub from: Vec<u8>,
    pub kind: u8,
    pub mode: Option<u32>,
    pub uid: u32,
    pub gid: u32,
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
            Network::Default => 1,
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
                Mount::Ssh {
                    id,
                    mode,
                    uid,
                    gid,
                    token,
                } => {
                    out.push(4);
                    put_bytes(&mut out, id);
                    put_u32(&mut out, *mode);
                    put_u32(&mut out, *uid);
                    put_u32(&mut out, *gid);
                    out.extend_from_slice(token);
                }
            }
        }
        put_len(&mut out, self.rlimits.len());
        for &(resource, soft, hard) in &self.rlimits {
            put_u32(&mut out, resource);
            out.extend_from_slice(&soft.to_be_bytes());
            out.extend_from_slice(&hard.to_be_bytes());
        }
        put_bytes(&mut out, &self.seccomp);
        put_len(&mut out, self.cgroup.len());
        for (file, value) in &self.cgroup {
            put_bytes(&mut out, file);
            put_bytes(&mut out, value);
        }
        put_len(&mut out, self.outputs.len());
        for &(mount, layer) in &self.outputs {
            put_u32(&mut out, mount);
            put_u32(&mut out, layer);
        }
        put_len(&mut out, self.devices.len());
        for d in &self.devices {
            put_bytes(&mut out, &d.path);
            put_bytes(&mut out, &d.from);
            out.push(d.kind);
            out.push(u8::from(d.mode.is_some()));
            put_u32(&mut out, d.mode.unwrap_or(0));
            put_u32(&mut out, d.uid);
            put_u32(&mut out, d.gid);
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
            1 => Network::Default,
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
                4 => Mount::Ssh {
                    id: r.bytes()?,
                    mode: r.u32()?,
                    uid: r.u32()?,
                    gid: r.u32()?,
                    token: r.take(16)?.try_into().ok()?,
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
        let seccomp = r.bytes()?;
        let n = r.count(8)?;
        let mut cgroup = Vec::with_capacity(n);
        for _ in 0..n {
            cgroup.push((r.bytes()?, r.bytes()?));
        }
        let n = r.count(8)?;
        let mut outputs = Vec::with_capacity(n);
        for _ in 0..n {
            outputs.push((r.u32()?, r.u32()?));
        }
        let n = r.count(19)?;
        let mut devices = Vec::with_capacity(n);
        for _ in 0..n {
            let path = r.bytes()?;
            let from = r.bytes()?;
            let kind = r.u8()?;
            let has_mode = r.flag()?;
            let mode = r.u32()?;
            devices.push(Device {
                path,
                from,
                kind,
                mode: has_mode.then_some(mode),
                uid: r.u32()?,
                gid: r.u32()?,
            });
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
            seccomp,
            cgroup,
            outputs,
            devices,
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
                    b"/run/buildkit/ssh_agent.0".to_vec(),
                    Mount::Ssh {
                        id: b"default".to_vec(),
                        mode: 0o600,
                        uid: 0,
                        gid: 0,
                        token: [7; 16],
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
            seccomp: vec![0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 255, 127],
            cgroup: vec![(b"memory.max".to_vec(), b"67108864".to_vec())],
            outputs: vec![(1, 9)],
            devices: vec![
                Device {
                    path: b"/dev/fuse".to_vec(),
                    from: Vec::new(),
                    kind: 0,
                    mode: None,
                    uid: 0,
                    gid: 0,
                },
                Device {
                    path: b"/dev/vendor0".to_vec(),
                    from: b"/dev/null".to_vec(),
                    kind: b'c',
                    mode: Some(0o660),
                    uid: 1000,
                    gid: 44,
                },
            ],
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
