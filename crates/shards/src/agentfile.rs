//! An image's Agentfile as the daemon holds a run to it (D109): read in the image's root
//! as its microVM mounts it, checked against the image's digest and as its build checks
//! it, its volumes and grants derived from its directives, never from its labels.
// Only Unix has the daemon, so far.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::BTreeSet;

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

/// The most a normalized Agentfile may be: what shards-init, which reads it in the guest,
/// takes (crates/init src/json.rs `MAX_LEN`, l. 7), so that the host refuses as the guest
/// would, at build and at run alike.
const AGENTFILE_MOST: u64 = 1 << 20;

/// An image's Agentfile as the daemon holds a run to it (D109): its volumes, and its
/// grants past the microVM (D59).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Agentfile {
    pub volumes: Vec<AgentVolume>,
    pub grants: shards_agentfile::agentfile::Grants,
    /// How many agents and harnesses it declares, the domains its init numbers (D59) and
    /// whose IDs no other process takes (D115).
    pub domains: u32,
    pub managed_labels: Vec<String>,
    pub active_members: Vec<u32>,
    /// Whether their flows past the microVM, out or in, or their names, cross init's
    /// network namespace, the command's, on their way to eth0 (D59, D115).
    pub uplink: bool,
    /// Whether its agents have IPv6 (`NETWORK --ipv6`, D99): with grants past the microVM,
    /// its run's eth0 takes an IPv6 address for their flows where its network gives none.
    pub ipv6: bool,
    /// Who each port coming in past the microVM reaches (D122), for `shards read ports`.
    pub receivers: Vec<shards_agentfile::agentfile::Receiver>,
}

/// What a run's command is given of the nesting grant (D234), of what its image declares and
/// what the run asks (`--security-opt nesting=true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nesting {
    /// The run does not ask for it, and its image does not declare it.
    Neither,
    /// Its image declares it, and the run does not ask for it: the run goes on without it,
    /// and says so ([`NESTING_UNASKED`]), as D109 drops nothing unsaid.
    Unasked,
    /// The run asks for it, and its image declares it beside no agent or harness: its
    /// command has it, with the run's deadline and GPUs.
    Granted(shards_abi::run::nesting::Grant),
}

impl Nesting {
    /// The grant, where the command has it.
    pub fn grant(&self) -> Option<&shards_abi::run::nesting::Grant> {
        match self {
            Nesting::Granted(grant) => Some(grant),
            Nesting::Neither | Nesting::Unasked => None,
        }
    }
}

/// What a run of an image declaring `NESTING` that does not ask for it is told as it is made.
pub const NESTING_UNASKED: &str = "the nesting grant withheld from the command: the image's Agentfile declares NESTING, and the run does not ask for it (--security-opt nesting=true)";

/// Why the daemon refuses a run the nesting grant (D234), in the words shards-init refuses it
/// with again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestingRefused {
    /// The run asks for it, and its image's Agentfile, or its lack of one, declares no
    /// `NESTING`.
    Undeclared,
    /// With `--cgroupns host` (D156).
    CgroupnsHost,
    /// With `--pid host` (D115).
    PidHost,
    /// A run joining the network of one given it (D119).
    Joiner,
    /// A run joining another's network that asks for it.
    Joined,
    /// A deadline (`ready_by`) in a run that does not ask for it, whose deadline it is.
    Ungranted,
}

impl std::fmt::Display for NestingRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use shards_abi::run::nesting;
        match self {
            NestingRefused::Undeclared => f.write_str(&nesting::undeclared()),
            NestingRefused::CgroupnsHost => f.write_str(&nesting::cgroupns_host()),
            NestingRefused::PidHost => f.write_str(&nesting::pid_host()),
            NestingRefused::Joiner => f.write_str(&nesting::joiner()),
            NestingRefused::Joined => f.write_str(&nesting::joined()),
            NestingRefused::Ungranted => write!(f, "{}", nesting::Error::Ungranted("ready-by=")),
        }
    }
}

/// Whether `run` asks for the nesting grant, as [`crate::setup::security`] reads its
/// `--security-opt`: one it cannot read, which refuses the run, asks for nothing.
fn asks_nesting(run: &shards_ipc::Run) -> bool {
    crate::setup::security(run).is_ok_and(|s| s.nesting)
}

/// What the command of `run` is given of the nesting grant (D234), of an image whose
/// Agentfile, as the daemon read it, is `agentfile` (none for an image without one); or why
/// not, as shards-init refuses it again: its image must declare it, and `--cgroupns host`
/// and `--pid host` undo it. A deadline is the grant's alone.
pub fn nesting(agentfile: Option<&Agentfile>, run: &shards_ipc::Run) -> Result<Nesting, NestingRefused> {
    let declared = agentfile.is_some_and(|a| a.grants.nesting);
    if !asks_nesting(run) {
        return match (run.ready_by, declared) {
            (Some(_), _) => Err(NestingRefused::Ungranted),
            (None, true) => Ok(Nesting::Unasked),
            (None, false) => Ok(Nesting::Neither),
        };
    }
    if !declared {
        return Err(NestingRefused::Undeclared);
    }
    if run.cgroupns == "host" {
        return Err(NestingRefused::CgroupnsHost);
    }
    if run.pid == "host" {
        return Err(NestingRefused::PidHost);
    }
    Ok(Nesting::Granted(shards_abi::run::nesting::Grant {
        ready_by: run.ready_by,
        // A run names no GPUs by their identities (D232) yet: `--gpus` asks for its
        // microVM's one, which names none.
        gpus: Vec::new(),
    }))
}

/// A run joining another's microVM (D119), and the nesting grant (D234): refused where that
/// microVM has it (`joins_granted`), whose command is its only principal, and where the run
/// asks for it, never a joiner's; in that order, as shards-init refuses each again.
pub fn joining(run: &shards_ipc::Run, joins_granted: bool) -> Result<(), NestingRefused> {
    if joins_granted {
        return Err(NestingRefused::Joiner);
    }
    if asks_nesting(run) {
        return Err(NestingRefused::Joined);
    }
    Ok(())
}

impl Agentfile {
    /// Whether its agents' flows past the microVM go by IPv6 too (D99): they have IPv6, and
    /// are granted ports or remote MCP servers past it. Their run's eth0 then needs an IPv6
    /// address, which a network without IPv6 does not give it.
    pub fn egress6(&self) -> bool {
        self.ipv6 && (!self.grants.egress.is_empty() || !self.grants.mcp.is_empty())
    }

    /// What of `run` reaches the image's domains (D115), in the words its init refuses
    /// it with again (`shards_abi::run::beside`): a privileged command, `/proc` unmasked,
    /// the microVM's PID or cgroup namespace (D156) or the host's network (D124), a
    /// capability that reaches
    /// them, a net.* sysctl where their flows cross the command's network namespace, a uid
    /// or gid of theirs, and no network where they are granted what lies past the microVM.
    /// None where it declares no domain.
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
        if run.cgroupns == "host" {
            return Err(beside::cgroupns_host());
        }
        if run.network == "host" {
            return Err(beside::network_host());
        }
        if let Some((cap, reach)) = beside::crossing(self.caps(run), self.uplink) {
            let name = shards_abi::run::CAP_NAMES
                .get(cap as usize)
                .copied()
                .unwrap_or("a capability");
            return Err(beside::capability(name, reach));
        }
        // A microVM's own and its endpoint's (`driver-opt=…endpoint.sysctls`), each net.*.
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

    /// The capabilities `run` gives its command: the default set images are made for (D115),
    /// less Docker's default `CAP_NET_RAW` where the domains' flows cross the command's network
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

    /// The default capabilities of `run`'s command that [`Agentfile::caps`] withholds,
    /// each by number and what it reaches of the domains: CAP_NET_RAW, where their flows
    /// past the microVM cross the command's network namespace. The run warns of each as it
    /// is made.
    pub fn withheld(&self, run: &shards_ipc::Run) -> Vec<(u32, &'static str)> {
        let (defaults, kept) = (crate::setup::capabilities(run), self.caps(run));
        shards_abi::run::beside::UPLINK
            .iter()
            .copied()
            .filter(|&(c, _)| c < 64 && defaults & (1 << c) != 0 && kept & (1 << c) == 0)
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
/// which an image sets as it likes. Each lookup verifies the root it opens.
pub fn agentfile(rootfs: &std::path::Path, digest: &str) -> Result<Agentfile, String> {
    use shards_agentfile::agentfile::{self as af, Directive};
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
    let said = |e: Vec<u8>| fail(String::from_utf8_lossy(&e).into_owned());
    let directives = af::resolve_targets(&af::from_spec(&json).map_err(fail)?).map_err(said)?;
    af::connections(&directives).map_err(said)?;
    let receivers = af::ingress(&directives).map_err(said)?;
    af::reach(&directives).map_err(said)?;
    // `NESTING` as its build checks it (D234): once, and beside no agent or harness.
    af::nesting(&directives).map_err(|e| fail(e.to_string()))?;
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
        if let Some(m) = af::domain_mount(p.as_bytes()) {
            return Err(fail(format!("{m}: {p}")));
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
    let (managed_labels, active_members) = managed_plan(&mut image, &directives).map_err(fail)?;
    let agentfile = Agentfile {
        managed_labels,
        active_members,
        volumes,
        uplink: !grants.egress.is_empty() || !grants.mcp.is_empty() || grants.dns || ingress,
        ipv6: af::ipv6(&directives),
        grants,
        domains: u32::try_from(domains).map_err(|_| fail("more agents and harnesses than uids".into()))?,
        receivers,
    };
    Ok(agentfile)
}

/// Read the image's runnable fact with the same root-confined Linux path semantics
/// as init. EROFS read_file deliberately does not follow links; vfs already does.
fn managed_plan(
    image: &mut std::fs::File,
    directives: &[shards_agentfile::agentfile::Directive],
) -> Result<(Vec<String>, Vec<u32>), String> {
    use shards_agentfile::agentfile::Directive;
    use shards_build::vfs::{Errno, Fs};
    use shards_image::erofs::Kind;
    use std::io::{Read as _, Seek as _};
    if !directives
        .iter()
        .any(|d| matches!(d, Directive::Agent(_) | Directive::Harness(_)))
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let tree = shards_image::erofs::read_tree(image, 0).map_err(|e| e.to_string())?;
    // The clock is unused: this is a read-only resolution, not a file action.
    let root = Fs::new(tree, (0, 0));
    let mut labels = Vec::new();
    let mut active = Vec::new();
    for harness in [false, true] {
        for directive in directives {
            let (kind, domain) = match (harness, directive) {
                (false, Directive::Agent(domain)) => ("agent", domain),
                (true, Directive::Harness(domain)) => ("harness", domain),
                _ => continue,
            };
            let member = u32::try_from(labels.len()).map_err(|e| e.to_string())?;
            let name = std::str::from_utf8(&domain.name).map_err(|e| e.to_string())?;
            labels.try_reserve(1).map_err(|e| e.to_string())?;
            labels.push(format!("{kind} {name}"));
            let home = if harness { "/harness" } else { "/agents" };
            let to = domain
                .to
                .as_ref()
                .map(|to| std::str::from_utf8(to))
                .transpose()
                .map_err(|e| e.to_string())?
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{home}/{name}"));
            let path = format!("{to}.d/osi.json");
            let id = match root.stat(path.as_bytes()) {
                Ok(id) => id,
                Err(e) if e.errno == Errno::NoEnt => continue,
                Err(e) => return Err(e.to_string()),
            };
            let Some(node) = root.node(id) else {
                return Err("missing managed config inode".into());
            };
            let Kind::File { size, data } = node.kind else {
                return Err(format!("{path}: not a regular config file"));
            };
            if size > AGENTFILE_MOST || data.source != 0 {
                return Err(format!(
                    "{path}: config exceeds the guest reader or names foreign data"
                ));
            }
            let len = usize::try_from(size).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(len).map_err(|e| e.to_string())?;
            bytes.resize(len, 0);
            image
                .seek(std::io::SeekFrom::Start(data.offset))
                .map_err(|e| e.to_string())?;
            image.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            let config: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))?;
            if config.get("run").is_some() {
                active.try_reserve(1).map_err(|e| e.to_string())?;
                active.push(member);
            }
        }
    }
    Ok((labels, active))
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
            grants: shards_agentfile::agentfile::Grants {
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
        let defaults = crate::setup::capabilities(&Run::default());
        assert_ne!(defaults & net_raw, 0);
        assert_eq!(beside(true).caps(&Run::default()), defaults & !net_raw);
        assert_eq!(beside(false).caps(&Run::default()), defaults);
        let booted = caps(&["CAP_SYS_BOOT"]);
        assert_eq!(
            beside(true).caps(&booted),
            crate::setup::capabilities(&booted) & !net_raw
        );
        let raw = caps(&["CAP_NET_RAW"]);
        assert_ne!(beside(true).caps(&raw) & net_raw, 0);
    }

    #[test]
    fn runnable_members_are_read_again_when_the_image_at_a_path_changes() {
        use shards_agentfile::agentfile::{Directive, Domain, Processes, digest, spec};
        use shards_build::vfs::Fs;
        use shards_image::erofs::{DataRef, Meta, Source, Tree};
        struct Contents(Vec<Vec<u8>>);
        impl Source for Contents {
            fn read_at(&mut self, data: DataRef, at: u64, out: &mut [u8]) -> std::io::Result<()> {
                let bytes = self.0.get(data.source as usize).unwrap();
                out.copy_from_slice(bytes.get(at as usize..at as usize + out.len()).unwrap());
                Ok(())
            }
        }
        let declaration = Directive::Agent(Domain {
            name: b"a".to_vec(),
            source: b"./artifact".to_vec(),
            to: None,
            processes: Processes::Unbounded,
        });
        let json = spec(&[declaration]);
        let expected = String::from_utf8(digest(&json)).unwrap();
        let dir = shards_testdir::TempDir::new("managed-plan-replace").unwrap();
        let path = dir.join("root.erofs");
        let write = |config: &[u8]| {
            let mut fs = Fs::new(
                Tree::new(Meta {
                    mode: 0o755,
                    ..Meta::default()
                }),
                (0, 0),
            );
            for directory in [b"/agents".as_slice(), b"/agents/a.d"] {
                fs.mkdir(directory, 0o755).unwrap();
            }
            let mut contents = Contents(vec![json.clone(), config.to_vec()]);
            for (index, name) in [b"/.agentfile.json".as_slice(), b"/agents/a.d/osi.json"]
                .into_iter()
                .enumerate()
            {
                let id = fs.create(name, 0o444).unwrap();
                fs.set_data(
                    id,
                    contents.0[index].len() as u64,
                    DataRef {
                        source: index as u32,
                        offset: 0,
                    },
                );
            }
            let mut file = std::fs::File::create(&path).unwrap();
            shards_image::erofs::write(fs.tree(), &mut contents, &mut file).unwrap();
            file.sync_all().unwrap();
        };
        write(br#"{"run":{"command":["true"]}}"#);
        assert_eq!(agentfile(&path, &expected).unwrap().active_members, [0]);
        write(b"{}");
        assert!(agentfile(&path, &expected).unwrap().active_members.is_empty());
    }

    #[test]
    fn runnable_members_keep_declared_ids_and_resolve_config_links_inside_the_image() {
        use shards_agentfile::agentfile::{Directive, Domain, Processes};
        use shards_build::vfs::Fs;
        use shards_image::erofs::{DataRef, Meta, Source, Tree};
        struct Contents(Vec<Vec<u8>>);
        impl Source for Contents {
            fn read_at(&mut self, data: DataRef, at: u64, out: &mut [u8]) -> std::io::Result<()> {
                let bytes = self.0.get(data.source as usize).unwrap();
                out.copy_from_slice(bytes.get(at as usize..at as usize + out.len()).unwrap());
                Ok(())
            }
        }
        let mut fs = Fs::new(
            Tree::new(Meta {
                mode: 0o755,
                ..Meta::default()
            }),
            (0, 0),
        );
        for path in ["/agents", "/agents/files.d", "/agents/a.d", "/harness", "/shared"] {
            fs.mkdir(path.as_bytes(), 0o755).unwrap();
        }
        let mut contents = Contents(vec![b"{}".to_vec(), br#"{"run":{"command":["true"]}}"#.to_vec()]);
        for (index, path) in ["/agents/files.d/osi.json", "/shared/osi.json"]
            .into_iter()
            .enumerate()
        {
            let id = fs.create(path.as_bytes(), 0o444).unwrap();
            fs.set_data(
                id,
                contents.0.get(index).unwrap().len() as u64,
                DataRef {
                    source: index as u32,
                    offset: 0,
                },
            );
        }
        fs.symlink(b"/shared/osi.json", b"/agents/a.d/osi.json").unwrap();
        fs.symlink(b"/shared", b"/harness/h.d").unwrap();
        let domain = |name: &str| Domain {
            name: name.as_bytes().to_vec(),
            source: b"./artifact".to_vec(),
            to: None,
            processes: Processes::Unbounded,
        };
        let declarations = [
            Directive::Harness(domain("h")),
            Directive::Agent(domain("files")),
            Directive::Agent(domain("a")),
        ];
        let dir = shards_testdir::TempDir::new("managed-plan").unwrap();
        let path = dir.join("root.erofs");
        let mut file = std::fs::File::create(&path).unwrap();
        shards_image::erofs::write(fs.tree(), &mut contents, &mut file).unwrap();
        drop(file);
        let (labels, active) = managed_plan(&mut std::fs::File::open(path).unwrap(), &declarations).unwrap();
        assert_eq!(labels, ["agent files", "agent a", "harness h"]);
        assert_eq!(active, [1, 2]);
    }

    /// An image's Agentfile read from its root as its microVM mounts it, and checked as
    /// its build checks it, its grants derived from its directives: its digest, no volume
    /// over a domain's own mounts, one at each mount point, an internal-only agent reaching
    /// no further; refused where the root holds none, or something else there.
    #[test]
    fn an_images_agentfile_is_read_from_its_root_and_checked() {
        use shards_agentfile::agentfile::{
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
        let digest = |json: &[u8]| String::from_utf8(shards_agentfile::agentfile::digest(json)).unwrap();
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
                nesting: false,
            }
        );
        let e = agentfile(&root_with("good-again", file(&good)), "sha256:00").unwrap_err();
        assert!(e.contains("its content hashes to"), "{e}");
        // What a crafted image holds and no build makes: refused as the build refuses it.
        let nesting_beside = shards_abi::run::beside::nesting("agent", "a");
        for (name, directives, said) in [
            (
                "dev",
                vec![agent("a"), volume(None, "/dev/shm", &["a"])],
                "VOLUME may not cover a domain's root, /proc, /sys or /dev: /dev/shm",
            ),
            (
                "tmp",
                vec![agent("a"), volume(None, "/tmp", &["a"])],
                "VOLUME may not be at or under /tmp, a domain's own scratch: /tmp",
            ),
            (
                "under-tmp",
                vec![agent("a"), volume(Some("cache"), "/tmp/cache", &[])],
                "VOLUME may not be at or under /tmp, a domain's own scratch: /tmp/cache",
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
            // The nesting grant beside an agent (D234), in the build's words and init's.
            (
                "nesting-beside",
                vec![agent("a"), Directive::Nesting],
                nesting_beside.as_str(),
            ),
        ] {
            let json = spec(&directives);
            let got = agentfile(&root_with(name, file(&json)), &digest(&json));
            assert!(got.as_ref().is_err_and(|e| e.contains(said)), "{name}: {got:?}");
        }
        // A port let in to several members goes to the one listening on it (D122, the
        // user's decision of 2026-11-08): each is a receiver of it, none refused.
        let json = spec(&[
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
        ]);
        let got = agentfile(&root_with("ingress", file(&json)), &digest(&json)).unwrap();
        let names: Vec<&[u8]> = got
            .receivers
            .iter()
            .filter(|r| r.at == (6, 8080, 8080))
            .map(|r| r.name.as_slice())
            .collect();
        assert_eq!(names, [b"a".as_slice(), b"b".as_slice()], "{got:?}");
        // An image declaring the nesting grant alone (D234): declared, its grant the run's
        // to ask for.
        let json = spec(&[Directive::Nesting]);
        let got = agentfile(&root_with("nesting", file(&json)), &digest(&json)).unwrap();
        assert!(got.grants.nesting, "{got:?}");
        assert_eq!(got.domains, 0);
        let e = agentfile(&root_with("none", None), "sha256:01").unwrap_err();
        assert!(e.contains("the image's root holds none"), "{e}");
        let link = Kind::Symlink(b"/etc/x".to_vec().into_boxed_slice());
        let e = agentfile(&root_with("link", Some((link, Vec::new()))), "sha256:02").unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The nesting grant (D234) is given only where the run asks for it and its image's
    /// Agentfile declares it: refused where the run asks and the image does not, or has no
    /// Agentfile; with the microVM's cgroup or PID namespace; and for a deadline without it.
    /// An image's grant the run does not ask for is withheld, to be said. Each refusal is in
    /// init's words.
    #[test]
    fn the_nesting_grant_is_given_only_as_its_image_declares_and_its_run_asks() {
        use shards_abi::run::nesting::{self, Grant};
        use shards_ipc::Run;
        let declaring = Agentfile {
            grants: shards_agentfile::agentfile::Grants {
                nesting: true,
                ..Default::default()
            },
            ..Agentfile::default()
        };
        let silent = Agentfile::default();
        let asking = |f: &dyn Fn(&mut Run)| {
            let mut r = Run {
                security_opt: vec!["nesting=true".into()],
                ..Run::default()
            };
            f(&mut r);
            r
        };
        let plain = Run::default();
        let declined = Run {
            security_opt: vec!["nesting=false".into()],
            ..Run::default()
        };
        assert_eq!(nesting(None, &plain), Ok(Nesting::Neither));
        assert_eq!(nesting(Some(&silent), &plain), Ok(Nesting::Neither));
        assert_eq!(nesting(Some(&declaring), &plain), Ok(Nesting::Unasked));
        assert_eq!(nesting(Some(&declaring), &declined), Ok(Nesting::Unasked));
        assert_eq!(
            nesting(Some(&declaring), &asking(&|_| {})),
            Ok(Nesting::Granted(Grant::default()))
        );
        assert_eq!(
            nesting(Some(&declaring), &asking(&|r| r.ready_by = Some(7))),
            Ok(Nesting::Granted(Grant {
                ready_by: Some(7),
                gpus: Vec::new()
            }))
        );
        let late = Run {
            ready_by: Some(7),
            ..Run::default()
        };
        for (agentfile, run, refused) in [
            (None, asking(&|_| {}), NestingRefused::Undeclared),
            (Some(&silent), asking(&|_| {}), NestingRefused::Undeclared),
            (
                Some(&declaring),
                asking(&|r| r.cgroupns = "host".into()),
                NestingRefused::CgroupnsHost,
            ),
            (
                Some(&declaring),
                asking(&|r| r.pid = "host".into()),
                NestingRefused::PidHost,
            ),
            (Some(&declaring), late.clone(), NestingRefused::Ungranted),
            (None, late.clone(), NestingRefused::Ungranted),
        ] {
            assert_eq!(nesting(agentfile, &run), Err(refused), "{refused:?}");
        }
        for (refused, said) in [
            (NestingRefused::Undeclared, nesting::undeclared()),
            (NestingRefused::CgroupnsHost, nesting::cgroupns_host()),
            (NestingRefused::PidHost, nesting::pid_host()),
            (NestingRefused::Joiner, nesting::joiner()),
            (NestingRefused::Joined, nesting::joined()),
            (
                NestingRefused::Ungranted,
                nesting::Error::Ungranted("ready-by=").to_string(),
            ),
        ] {
            assert_eq!(refused.to_string(), said);
        }
        assert_eq!(
            Nesting::Granted(Grant::default()).grant(),
            Some(&Grant::default())
        );
        assert_eq!(Nesting::Unasked.grant(), None);
        assert_eq!(Nesting::Neither.grant(), None);
    }

    /// No run joining another's microVM (D119) takes the nesting grant, nor joins a microVM
    /// whose command has it (D234): refused for the microVM it joins first, as init refuses
    /// it; a joiner that asks for nothing, of a microVM without it, goes on.
    #[test]
    fn no_joiner_takes_or_joins_the_nesting_grant() {
        use shards_ipc::Run;
        let asks = Run {
            security_opt: vec!["nesting=true".into()],
            ..Run::default()
        };
        let plain = Run::default();
        assert_eq!(joining(&plain, false), Ok(()));
        assert_eq!(joining(&plain, true), Err(NestingRefused::Joiner));
        assert_eq!(joining(&asks, false), Err(NestingRefused::Joined));
        assert_eq!(joining(&asks, true), Err(NestingRefused::Joiner));
    }

    /// The words refusing the nesting grant beside an image's agent or harness are one set
    /// (D234): the build's (shards_agentfile), the daemon's read of the image, and init's.
    #[test]
    fn the_nesting_grant_beside_a_domain_is_refused_in_one_set_of_words() {
        use shards_agentfile::agentfile::{TargetKind, nesting_beside};
        assert_eq!(
            nesting_beside(TargetKind::Agent, b"a"),
            shards_abi::run::beside::nesting("agent", "a")
        );
        assert_eq!(
            nesting_beside(TargetKind::Harness, b"h"),
            shards_abi::run::beside::nesting("harness", "h")
        );
    }
}
