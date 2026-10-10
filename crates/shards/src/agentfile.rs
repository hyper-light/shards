//! An image's Agentfile as the daemon holds a run to it (D109): read in the image's root
//! as its microVM mounts it, checked against the image's digest and as its build checks
//! it, its volumes and grants derived from its directives, never from its labels.

use std::collections::{BTreeMap, BTreeSet};

use shards_cmdline::mounts::clean;

/// A volume an image's Agentfile declares (AGENTFILE_ARCH.md §4.5, §12 answer 8): its
/// name, as Compose names one (none for an anonymous volume at each mount point), its
/// mount points, and whether `FOR` scopes it to some of the image's agents and harnesses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentVolume {
    pub name: Option<String>,
    pub paths: Vec<String>,
    pub scoped: bool,
}

/// The most a normalized Agentfile may be, as shards-init reads one.
const AGENTFILE_MOST: u64 = 1 << 20;

/// An image's Agentfile as the daemon holds a run to it (D109): its volumes, and its
/// grants past the microVM (D59).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Agentfile {
    pub volumes: Vec<AgentVolume>,
    pub grants: shards_dockerfile::agentfile::Grants,
}

/// An image's Agentfile, from its normalized Agentfile, `/.agentfile.json` (D35), as the
/// image's root holds it (`rootfs`, the EROFS image its microVM mounts, where its init
/// reads it), checked against `digest`, the image's `vnd.osi.agentfile.digest`, read back
/// into its directives, and checked again as its build checks them (a crafted image's
/// passed no build): its volumes and grants derived from those, never from its labels,
/// which an image sets as it likes. Read once for each root and digest a process meets.
pub fn agentfile(rootfs: &std::path::Path, digest: &str) -> Result<Agentfile, String> {
    use shards_dockerfile::agentfile::{self as af, Directive};
    use std::sync::{Mutex, OnceLock, PoisonError};
    type Read = BTreeMap<(std::path::PathBuf, String), Agentfile>;
    static READ: OnceLock<Mutex<Read>> = OnceLock::new();
    let read = READ.get_or_init(Mutex::default);
    let key = (rootfs.to_path_buf(), digest.to_string());
    if let Some(a) = read.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Ok(a.clone());
    }
    let fail = |why: String| format!("the image's Agentfile ({digest}): {why}");
    let mut image = std::fs::File::open(rootfs).map_err(|e| fail(format!("{}: {e}", rootfs.display())))?;
    let json = match shards_image::erofs::read_file(&mut image, af::SPEC_PATH, AGENTFILE_MOST)
        .map_err(|e| fail(e.to_string()))?
    {
        shards_image::erofs::Found::File(json) => json,
        shards_image::erofs::Found::Missing => return Err(fail("the image's root holds none".into())),
        shards_image::erofs::Found::Other => {
            return Err(fail("not a regular file in the image's root".into()));
        }
    };
    let actual = String::from_utf8_lossy(&af::digest(&json)).into_owned();
    if actual != digest {
        return Err(fail(format!("its content hashes to {actual}")));
    }
    let directives = af::from_spec(&json).map_err(fail)?;
    let said = |e: Vec<u8>| fail(String::from_utf8_lossy(&e).into_owned());
    af::connections(&directives).map_err(said)?;
    af::ingress(&directives).map_err(said)?;
    af::reach(&directives).map_err(said)?;
    let text = |b: &[u8]| {
        String::from_utf8(b.to_vec()).map_err(|_| fail("a volume's name or path that is not UTF-8".into()))
    };
    let mut volumes = Vec::new();
    for d in &directives {
        if let Directive::Volume(v) = d {
            volumes.push(AgentVolume {
                name: v.source.as_deref().map(text).transpose()?,
                paths: v.paths.iter().map(|p| text(p)).collect::<Result<_, _>>()?,
                scoped: !v.scope.names.is_empty(),
            });
        }
    }
    // As its build refuses them: none over a domain's own mounts, and one at each mount
    // point, whose domains are given what is mounted there.
    let mut mounts = BTreeSet::new();
    for p in volumes.iter().flat_map(|v| &v.paths) {
        if af::covers_domain_mounts(p.as_bytes()) {
            return Err(fail(format!("{}: {p}", af::COVERS)));
        }
        if !mounts.insert(clean(&format!("/{p}"))) {
            return Err(fail(format!("two of its volumes mount {p}")));
        }
    }
    let agentfile = Agentfile {
        volumes,
        grants: af::grants(&directives),
    };
    read.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, agentfile.clone());
    Ok(agentfile)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image's Agentfile read from its root as its microVM mounts it, and checked as
    /// its build checks it, its grants derived from its directives: its digest, no volume
    /// over a domain's own mounts, one at each mount point, an internal-only agent reaching
    /// no further; refused where the root holds none, or something else there.
    #[test]
    fn an_images_agentfile_is_read_from_its_root_and_checked() {
        use shards_dockerfile::agentfile::{
            Connect, Direction, Directive, Domain, Exposure, Grants, Network, Processes, Scope, Volume, spec,
        };
        use shards_image::erofs::{DataRef, Kind, Meta, Node, Source, Tree};
        struct Mem(Vec<u8>);
        impl Source for Mem {
            fn read_at(&mut self, _: DataRef, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
                let at = usize::try_from(at).unwrap();
                buf.copy_from_slice(&self.0[at..at + buf.len()]);
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("shards-agentfile-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root_with = |name: &str, node: Option<(Kind, Vec<u8>)>| {
            let meta = |mode| Meta {
                mode,
                ..Meta::default()
            };
            let mut tree = Tree::new(meta(0o755));
            let mut data = Vec::new();
            if let Some((kind, bytes)) = node {
                data = bytes;
                tree.insert(
                    Tree::ROOT,
                    b".agentfile.json",
                    Node {
                        kind,
                        meta: meta(0o444),
                    },
                )
                .unwrap();
            }
            let path = dir.join(name);
            let mut out = std::fs::File::create(&path).unwrap();
            shards_image::erofs::write(&tree, &mut Mem(data), &mut out).unwrap();
            path
        };
        let file = |json: &[u8]| {
            let data = DataRef { source: 0, offset: 0 };
            Some((
                Kind::File {
                    size: json.len() as u64,
                    data,
                },
                json.to_vec(),
            ))
        };
        let digest = |json: &[u8]| String::from_utf8(shards_dockerfile::agentfile::digest(json)).unwrap();
        let b = |s: &str| s.as_bytes().to_vec();
        let agent = |name: &str| {
            Directive::Agent(Domain {
                name: b(name),
                source: b("./a"),
                to: None,
                processes: Processes::Unbounded,
            })
        };
        let volume = |source: Option<&str>, path: &str, names: &[&str]| {
            Directive::Volume(Volume {
                source: source.map(b),
                paths: vec![b(path)],
                chown: Vec::new(),
                chmod: Vec::new(),
                scope: Scope {
                    kind: None,
                    names: names.iter().map(|n| b(n)).collect(),
                },
            })
        };
        let network = |name: &str, internal: bool| {
            Directive::Network(Network {
                name: b(name),
                internal,
                dns: !internal,
                ports: vec![(b("443"), Direction::Both)],
                ..Network::default()
            })
        };
        let connect = |on: &str| {
            Directive::Connect(Connect {
                kind: None,
                from: vec![b("a")],
                both_ways: true,
                to: vec![b("a")],
                on: vec![b(on)],
                ports: Vec::new(),
            })
        };
        let expose = Directive::Expose(Exposure {
            ports: vec![b("443")],
            direction: Direction::Both,
            networks: vec![b("out")],
        });

        let good = spec(&[
            agent("a"),
            network("out", false),
            connect("out"),
            expose.clone(),
            volume(Some("data"), "/data", &["a"]),
            volume(None, "/x", &[]),
        ]);
        let read = agentfile(&root_with("good", file(&good)), &digest(&good)).unwrap();
        assert_eq!(
            read.volumes,
            vec![
                AgentVolume {
                    name: Some("data".into()),
                    paths: vec!["/data".into()],
                    scoped: true
                },
                AgentVolume {
                    name: None,
                    paths: vec!["/x".into()],
                    scoped: false
                },
            ]
        );
        // From the directives the image's file holds, whatever its labels say.
        assert_eq!(
            read.grants,
            Grants {
                egress: vec![b("443")],
                mcp: Vec::new(),
                dns: true,
                egress_declared: Vec::new(),
            }
        );
        let e = agentfile(&root_with("good-again", file(&good)), "sha256:00").unwrap_err();
        assert!(e.contains("its content hashes to"), "{e}");
        // What a crafted image holds and no build makes: refused as the build refuses it.
        for (name, directives, said) in [
            (
                "dev",
                vec![agent("a"), volume(None, "/dev/shm", &["a"])],
                "VOLUME may not cover a domain's root, /proc, /sys or /dev: /dev/shm",
            ),
            (
                "twice",
                vec![
                    agent("a"),
                    volume(Some("a1"), "/d", &["a"]),
                    volume(Some("b1"), "/d/", &["a"]),
                ],
                "two of its volumes mount /d/",
            ),
            (
                "reach",
                vec![
                    agent("a"),
                    agent("b"),
                    Directive::Network(Network {
                        name: b("inside"),
                        internal: true,
                        ports: vec![(b("8080"), Direction::Both)],
                        ..Network::default()
                    }),
                    network("out", false),
                    expose,
                    Directive::Connect(Connect {
                        kind: None,
                        from: vec![b("a")],
                        both_ways: true,
                        to: vec![b("b")],
                        on: vec![b("inside")],
                        ports: vec![b("8080")],
                    }),
                    connect("out"),
                ],
                "agent b -> network inside -> agent a -> ",
            ),
            (
                "ingress",
                vec![
                    agent("a"),
                    agent("b"),
                    Directive::Network(Network {
                        name: b("pub"),
                        ports: vec![(b("8080"), Direction::Both)],
                        ..Network::default()
                    }),
                    Directive::Expose(Exposure {
                        ports: vec![b("8080")],
                        direction: Direction::Both,
                        networks: vec![b("pub")],
                    }),
                    Directive::Connect(Connect {
                        kind: None,
                        from: vec![b("a")],
                        both_ways: true,
                        to: vec![b("b")],
                        on: vec![b("pub")],
                        ports: vec![b("8080")],
                    }),
                ],
                "lets ports in past the microVM to its members",
            ),
            (
                "port",
                vec![
                    agent("a"),
                    Directive::Network(Network {
                        name: b("shut"),
                        ..Network::default()
                    }),
                    Directive::Connect(Connect {
                        kind: None,
                        from: vec![b("a")],
                        both_ways: true,
                        to: vec![b("a")],
                        on: vec![b("shut")],
                        ports: vec![b("8080")],
                    }),
                ],
                "the network lets no such port in to its members",
            ),
            (
                "resolver",
                vec![
                    agent("a"),
                    Directive::Network(Network {
                        name: b("inside"),
                        internal: true,
                        dns: true,
                        ..Network::default()
                    }),
                    connect("inside"),
                ],
                "an internal network has no resolver past the microVM to ask",
            ),
        ] {
            let json = spec(&directives);
            let got = agentfile(&root_with(name, file(&json)), &digest(&json));
            assert!(got.as_ref().is_err_and(|e| e.contains(said)), "{name}: {got:?}");
        }
        let e = agentfile(&root_with("none", None), "sha256:01").unwrap_err();
        assert!(e.contains("the image's root holds none"), "{e}");
        let link = Kind::Symlink(b"/etc/x".to_vec().into_boxed_slice());
        let e = agentfile(&root_with("link", Some((link, Vec::new()))), "sha256:02").unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
