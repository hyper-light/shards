//! An image's Agentfile as the daemon holds a run to it (D109): read in the image's root
//! as its microVM mounts it, checked against the image's digest and as its build checks
//! it, its volumes and grants derived from its directives, never from its labels.
// Only Unix has the daemon, so far.
#![cfg_attr(not(unix), allow(dead_code))]

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
    /// How many agents and harnesses it declares, the domains its init numbers (D59) and
    /// whose IDs no other process takes (D115).
    pub domains: u32,
    /// Whether their flows past the microVM, out or in, or their names, cross init's
    /// network namespace, the command's, on their way to eth0 (D59, D115).
    pub uplink: bool,
    /// Who each port coming in past the microVM reaches (D122), for `shards read ports`.
    pub receivers: Vec<shards_dockerfile::agentfile::Receiver>,
}

impl Agentfile {
    /// What of `run` reaches the image's domains (D115), in the words its init refuses
    /// it with again (`shards_abi::run::beside`): a privileged command, `/proc` unmasked,
    /// a capability that reaches them, a net.* sysctl where their flows cross the
    /// command's network namespace, a uid or gid of theirs, and no network where they are
    /// granted what lies past the microVM. None where it declares no domain.
    pub fn refuse(&self, run: &shards_ipc::Run) -> Result<(), String> {
        use shards_abi::run::beside;
        if self.domains == 0 {
            return Ok(());
        }
        if run.privileged {
            return Err(beside::privileged("run"));
        }
        if run.system_paths {
            return Err(beside::unmasked());
        }
        if run.pid == "host" {
            return Err(beside::pid_host());
        }
        if let Some((cap, reach)) = beside::crossing(self.caps(run), self.uplink) {
            let name = shards_abi::run::CAP_NAMES
                .get(cap as usize)
                .copied()
                .unwrap_or("a capability");
            return Err(beside::capability(name, reach));
        }
        // A container's own and its endpoint's (`driver-opt=…endpoint.sysctls`), each net.*.
        if self.uplink
            && let Some(s) = run
                .sysctls
                .iter()
                .find(|s| s.starts_with("net."))
                .cloned()
                .or_else(|| crate::setup::endpoint_sysctls(run).into_iter().next())
        {
            return Err(beside::sysctl(s.split_once('=').map_or(s.as_str(), |(k, _)| k)));
        }
        // A link-local address of its own in the link to the domains' switch (PM M175).
        if self.uplink
            && let Some(ip) = crate::setup::own_endpoint(run)
                .into_iter()
                .flat_map(|e| e.link_local.iter())
                .filter_map(|s| shards_cmdline::network::parse_addr(s).ok())
                .map(|a| a.unmap().ip)
                .find(|ip| matches!(ip, std::net::IpAddr::V4(v4) if beside::in_uplink(v4.octets())))
        {
            return Err(beside::link_local(&ip.to_string()));
        }
        self.ids(&run.user, &run.group_add)?;
        let granted = !self.grants.egress.is_empty() || !self.grants.mcp.is_empty() || self.grants.dns;
        if run.network == "none" && (granted || self.uplink) {
            return Err(
                "cannot run without a network: the image's Agentfile grants its agents what lies past the microVM, and --network none gives the microVM none".into(),
            );
        }
        Ok(())
    }

    /// The capabilities `run` gives its command: Docker's, as moby tweaks them, less
    /// Docker's default `CAP_NET_RAW` where the domains' flows cross the command's network
    /// namespace; one the run adds itself stays, for [`Agentfile::refuse`] to refuse.
    pub fn caps(&self, run: &shards_ipc::Run) -> u64 {
        let caps = crate::setup::capabilities(run);
        if self.domains == 0 || !self.uplink {
            return caps;
        }
        let added = |name: &str| run.privileged || run.cap_add.iter().any(|c| c == name || c == "ALL");
        shards_abi::run::beside::UPLINK
            .iter()
            .filter(|&&(c, _)| {
                !shards_abi::run::CAP_NAMES
                    .get(c as usize)
                    .is_some_and(|n| added(n))
            })
            .fold(caps, |m, &(c, _)| m & !(1 << c))
    }

    /// The capabilities Docker would give `run`'s command that [`Agentfile::caps`]
    /// withholds, each by number and what it reaches of the domains: CAP_NET_RAW, one of
    /// Docker's defaults, where their flows past the microVM cross the command's network
    /// namespace. The run says so as it is made, as dockerd's warnings are said.
    pub fn withheld(&self, run: &shards_ipc::Run) -> Vec<(u32, &'static str)> {
        let (docker, kept) = (crate::setup::capabilities(run), self.caps(run));
        shards_abi::run::beside::UPLINK
            .iter()
            .copied()
            .filter(|&(c, _)| c < 64 && docker & (1 << c) != 0 && kept & (1 << c) == 0)
            .collect()
    }

    /// Refuses a numeric `user` (`uid[:gid]`) or `--group-add` gid of the domains' IDs; a
    /// name, which the image's own databases resolve, its init refuses as it resolves it.
    pub fn ids(&self, user: &str, groups: &[String]) -> Result<(), String> {
        use shards_abi::run::beside;
        if self.domains == 0 {
            return Ok(());
        }
        let (u, g) = user.split_once(':').unwrap_or((user, ""));
        for (which, id) in [("uid", u), ("gid", g)]
            .into_iter()
            .chain(groups.iter().map(|g| ("gid", g.as_str())))
        {
            if let Ok(n) = id.parse::<u32>()
                && beside::taken(n, self.domains)
            {
                return Err(beside::id(which, n, self.domains));
            }
        }
        Ok(())
    }
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
    let receivers = af::ingress(&directives).map_err(said)?;
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
    let grants = af::grants(&directives);
    let domains = directives
        .iter()
        .filter(|d| matches!(d, Directive::Agent(_) | Directive::Harness(_)))
        .count();
    // Ports let in past the microVM, of a network a `CONNECT` joins, as its init lets
    // them in (D59).
    let ingress = directives.iter().any(|d| match d {
        Directive::Connect(c) => {
            c.on.iter()
                .any(|n| !af::boundary(&directives, n, false).is_empty())
        }
        _ => false,
    });
    let agentfile = Agentfile {
        volumes,
        uplink: !grants.egress.is_empty() || !grants.mcp.is_empty() || grants.dns || ingress,
        grants,
        domains: u32::try_from(domains).map_err(|_| fail("more agents and harnesses than uids".into()))?,
        receivers,
    };
    read.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, agentfile.clone());
    Ok(agentfile)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run may give its command beside an image's agents and harnesses (D115): it is
    /// refused a privileged command, `/proc` unmasked, each capability that reaches them
    /// (CAP_NET_RAW and CAP_NET_ADMIN where their flows cross its network namespace), a
    /// net.* sysctl there, a numeric uid or gid of theirs, and no network where they are
    /// granted what lies past the microVM; Docker's default CAP_NET_RAW is not its own
    /// there. An image that declares no domain is refused none of it.
    #[test]
    fn a_run_gives_its_command_nothing_that_reaches_its_domains() {
        use shards_ipc::Run;
        let beside = |uplink: bool| Agentfile {
            domains: 2,
            uplink,
            grants: shards_dockerfile::agentfile::Grants {
                egress: if uplink { vec![b"443".to_vec()] } else { Vec::new() },
                ..Default::default()
            },
            ..Agentfile::default()
        };
        let run = |f: &dyn Fn(&mut Run)| {
            let mut r = Run::default();
            f(&mut r);
            r
        };
        let caps = |list: &[&str]| run(&|r| r.cap_add = list.iter().map(|s| (*s).to_string()).collect());
        // A link-local address of its own (PM M175), on the default bridge.
        let link_local = |a: &str| {
            run(&|r| {
                r.network = "default".into();
                r.endpoints = vec![shards_ipc::Endpoint {
                    network: "default".into(),
                    link_local: vec![a.to_string()],
                    ..Default::default()
                }];
            })
        };
        for (r, uplink, said) in [
            (run(&|r| r.privileged = true), false, "cannot run privileged"),
            (
                run(&|r| r.system_paths = true),
                false,
                "cannot leave /proc unmasked",
            ),
            (
                caps(&["CAP_SYS_ADMIN"]),
                false,
                "cannot give the command CAP_SYS_ADMIN",
            ),
            (
                caps(&["CAP_SYS_PTRACE"]),
                false,
                "cannot give the command CAP_SYS_PTRACE",
            ),
            (caps(&["ALL"]), false, "cannot give the command CAP_SYS_MODULE"),
            (
                caps(&["CAP_NET_RAW"]),
                true,
                "cannot give the command CAP_NET_RAW",
            ),
            (
                caps(&["CAP_NET_ADMIN"]),
                true,
                "cannot give the command CAP_NET_ADMIN",
            ),
            (
                run(&|r| r.sysctls = vec!["net.ipv4.ip_forward=0".into()]),
                true,
                "cannot set sysctl net.ipv4.ip_forward",
            ),
            (
                run(&|r| r.user = "200001".into()),
                false,
                "cannot run as uid 200001",
            ),
            (
                run(&|r| r.user = "app:4394305".into()),
                false,
                "cannot run as gid 4394305",
            ),
            (
                run(&|r| r.group_add = vec!["200000".into()]),
                false,
                "cannot run as gid 200000",
            ),
            (
                run(&|r| r.network = "none".into()),
                true,
                "cannot run without a network",
            ),
            (
                link_local("169.254.77.2"),
                true,
                "cannot give eth0 link-local address 169.254.77.2",
            ),
            (
                link_local("::ffff:169.254.77.1"),
                true,
                "cannot give eth0 link-local address 169.254.77.1",
            ),
        ] {
            let got = beside(uplink).refuse(&r);
            assert!(
                got.as_ref().is_err_and(|e| e.starts_with(said)),
                "{said}: {got:?}"
            );
            let none = Agentfile {
                domains: 0,
                ..beside(uplink)
            };
            assert_eq!(none.refuse(&r), Ok(()), "{said}");
        }
        // What reaches nothing of theirs, given as asked.
        for r in [
            caps(&["CAP_NET_RAW", "CAP_NET_ADMIN", "CAP_KILL", "CAP_SYS_BOOT"]),
            run(&|r| r.sysctls = vec!["net.ipv4.ip_forward=0".into(), "kernel.shmmax=1".into()]),
            run(&|r| r.user = "1000:200002".into()),
            run(&|r| r.security_opt = vec!["seccomp=unconfined".into()]),
            link_local("169.254.77.2"),
        ] {
            assert_eq!(beside(false).refuse(&r), Ok(()));
        }
        assert_eq!(beside(true).refuse(&link_local("169.254.78.1")), Ok(()));
        assert_eq!(
            beside(true).refuse(&run(&|r| r.sysctls = vec!["kernel.shmmax=1".into()])),
            Ok(())
        );
        let net_raw = 1u64 << 13;
        let docker = crate::setup::capabilities(&Run::default());
        assert_ne!(docker & net_raw, 0);
        assert_eq!(beside(true).caps(&Run::default()), docker & !net_raw);
        assert_eq!(beside(false).caps(&Run::default()), docker);
        let booted = caps(&["CAP_SYS_BOOT"]);
        assert_eq!(
            beside(true).caps(&booted),
            crate::setup::capabilities(&booted) & !net_raw
        );
        let raw = caps(&["CAP_NET_RAW"]);
        assert_ne!(beside(true).caps(&raw) & net_raw, 0);
    }

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
        let dir = shards_testdir::TempDir::new("agentfile-root").unwrap();
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
            agents: Vec::new(),
            harnesses: Vec::new(),
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
                        agents: Vec::new(),
                        harnesses: Vec::new(),
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
                "lets port 8080 in past the microVM to its members agent a, agent b: name the one it is for",
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
