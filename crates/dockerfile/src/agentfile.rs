//! An Agentfile's directives (docs/architecture/AGENTFILE_ARCH.md §4 and §12, architecture.md
//! D35): a Dockerfile's instructions, and these, which declare the agents a microVM runs,
//! their skills and MCP servers, the harnesses that drive them, their volumes, and the
//! networks between them. Each line parses here into what it declares; the plan resolves
//! its names, and every grant is checked there.
//!
//! Keywords inside a directive (`FROM`, `TO`, `FOR`, `AS`, `WITH`, `ON`) are read in any
//! case, as `FROM … AS` is. Names of agents, harnesses and MCP servers are stage names
//! (`^[a-z][a-z0-9-_.]*$`, read lowercase): agents and harnesses share one namespace with a
//! file's stages, so that `COPY --from=main` names one thing (§7 Q19.2).

use crate::go;
use crate::instructions::{FlagType, Req, SourceContent, errf, sources, valid_stage_name};

/// What a name in a directive that takes either names (§4.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Agent,
    Harness,
}

/// How many processes a domain may start (§12.15, `--processes=<n|none>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Processes {
    /// The microVM's own limit.
    #[default]
    Unbounded,
    /// None: its seccomp filter refuses every way to start one (§9.9).
    None,
    AtMost(u32),
}

/// `AGENT` and `HARNESS`: a domain, where it comes from, and where it unpacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub name: Vec<u8>,
    /// As written: an OCI reference, a git URL, an http(s) URL, or a path (§12.2).
    pub source: Vec<u8>,
    /// `TO`'s path, or none for `/agents/<name>` or `/harness/<name>` (§12.1).
    pub to: Option<Vec<u8>>,
    pub processes: Processes,
}

/// Whom a grant is for: `FOR`'s names, of the kind `--target-kind` says or each name's
/// own. None named is every one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    pub kind: Option<TargetKind>,
    pub names: Vec<Vec<u8>>,
}

/// What a skill is taken from: a source as `ADD` reads one, or a heredoc's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    Path(Vec<u8>),
    Text(SourceContent),
}

/// `SKILL`: a skill taken as `ADD` takes a source, for every agent or those named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub source: SkillSource,
    /// Where it goes, or none for each agent's skills directory (§12.1).
    pub dest: Option<Vec<u8>>,
    pub from: Vec<u8>,
    pub chown: Vec<u8>,
    pub chmod: Vec<u8>,
    pub link: bool,
    pub exclude: Vec<Vec<u8>>,
    pub keep_git_dir: Option<bool>,
    pub checksum: Vec<u8>,
    pub scope: Scope,
}

/// `MCP`: a server, local or remote, offered to every agent or those named (§4.4, §12.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mcp {
    pub name: Vec<u8>,
    /// As written: a URL is a remote server; a path, git URL or OCI reference, one spoken
    /// to over stdio, `[:<version>]` naming what is fetched (§12.3).
    pub source: Vec<u8>,
    pub scope: Scope,
}

/// The way a port is open: both, in or out (§4.1, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Both,
    Ingress,
    Egress,
}

/// `NETWORK`: a network as Compose makes one, the ports open on it, and who may join it.
/// Declaring it joins no one (§4.6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Network {
    pub name: Vec<u8>,
    pub driver: Vec<u8>,
    pub driver_opts: Vec<Vec<u8>>,
    pub attachable: bool,
    pub internal: bool,
    pub external: bool,
    pub ipv4: Option<bool>,
    pub ipv6: Option<bool>,
    pub labels: Vec<Vec<u8>>,
    pub ipam_driver: Vec<u8>,
    pub ipam_opts: Vec<Vec<u8>>,
    pub subnets: Vec<Vec<u8>>,
    pub ip_ranges: Vec<Vec<u8>>,
    pub gateways: Vec<Vec<u8>>,
    pub aux_addresses: Vec<Vec<u8>>,
    /// `--expose`, `--ingress` and `--egress`, in the order given.
    /// `--dns`: its members may ask the microVM's resolver for names past the microVM,
    /// which no other grant implies (D59).
    pub dns: bool,
    pub ports: Vec<(Vec<u8>, Direction)>,
    /// `--protocol`: what it carries at all, a bit for each of [`PROTOCOLS`]; none for its
    /// default, TCP and UDP. A port of another is refused (D59).
    pub protocols: u8,
    pub scope: Scope,
}

/// The protocols a network may carry (`NETWORK --protocol`).
pub const PROTOCOLS: [&[u8]; 3] = [b"tcp", b"udp", b"unix"];

/// What a network carries: its `--protocol`s, else TCP and UDP.
pub fn carries(n: &Network, protocol: &[u8]) -> bool {
    let bits = if n.protocols == 0 { 0b011 } else { n.protocols };
    PROTOCOLS
        .iter()
        .position(|p| *p == protocol)
        .is_some_and(|i| bits & (1 << i) != 0)
}

/// A Unix socket a network carries, `unix:<name>`: its name, a stage-style name, which is
/// the directory its socket lies in, `/run/networks/<network>/<name>/` (D59).
pub fn unix_endpoint(s: &[u8]) -> Option<&[u8]> {
    s.strip_prefix(b"unix:")
        .filter(|n| valid_stage_name(n) && go::to_lower(n) == *n)
}

/// The protocol a port names: `unix` for a Unix socket, else its IP protocol's name.
fn protocol_of(p: &[u8]) -> Option<&'static [u8]> {
    if unix_endpoint(p).is_some() {
        return Some(b"unix");
    }
    match port_range(p)?.0 {
        6 => Some(b"tcp"),
        _ => Some(b"udp"),
    }
}

/// `CONNECT`: who may send to whom, on which ports, on which networks (§4.7). Networks are
/// default deny: a flow it does not name does not exist, so its `--port`s are what the
/// receiving side accepts, and a `CONNECT` of agents to themselves alone, which names no
/// flow, attaches them to its networks and grants nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub kind: Option<TargetKind>,
    pub from: Vec<Vec<u8>>,
    /// `WITH`: both ways. `TO`: requests go from `from` to `to` alone.
    pub both_ways: bool,
    pub to: Vec<Vec<u8>>,
    pub on: Vec<Vec<u8>>,
    /// `--port`: each a port or range as Docker writes one (`8080`, `53/udp`,
    /// `8000-8010/tcp`), TCP where none is said.
    pub ports: Vec<Vec<u8>>,
}

/// `ATTACH`: the harnesses that may drive the agents named (§4.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    pub agents: Vec<Vec<u8>>,
    pub harnesses: Vec<Vec<u8>>,
}

/// `EXPOSE` with `AS` or `FOR`: ports open at the microVM's boundary, one way or both, for
/// the networks named, or for none until a grant gives them (§4.1, §8 Q7, §12.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    /// Each as written: the microVM's port, or for ingress `<port>:<member_port>`, the
    /// microVM's then its member's (D122).
    pub ports: Vec<Vec<u8>>,
    pub direction: Direction,
    pub networks: Vec<Vec<u8>>,
    /// The members it is for (`--agents`, `--harnesses`, D122): every member of the
    /// networks where none is named.
    pub agents: Vec<Vec<u8>>,
    pub harnesses: Vec<Vec<u8>>,
}

/// An `EXPOSE`'s port as the microVM has it: of `<port>:<member_port>[/proto]` (D122) the
/// first, with its protocol; any other as written.
pub fn outside(p: &[u8]) -> Vec<u8> {
    mapping(p).0
}

/// An `EXPOSE`'s port as its member has it: of `<port>:<member_port>[/proto]` the second,
/// with its protocol; any other as written.
pub fn inside(p: &[u8]) -> Vec<u8> {
    mapping(p).1
}

/// `<port>:<member_port>[/proto]`'s two ports, each with the protocol; a port as written,
/// twice, where there is no `:`.
fn mapping(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (ports, proto) = match p.iter().rposition(|&b| b == b'/') {
        Some(at) => p.split_at(at),
        None => (p, &b""[..]),
    };
    match ports.iter().position(|&b| b == b':') {
        Some(at) => {
            let (a, b) = ports.split_at(at);
            (
                [a, proto].concat(),
                [b.get(1..).unwrap_or_default(), proto].concat(),
            )
        }
        None => (p.to_vec(), p.to_vec()),
    }
}

/// `VOLUME` with options, a name or `FOR` (§4.5, §12.8): mount points, the named volume
/// mounted at the one there is, and whom it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// A volume's name, as Compose names one; none for anonymous volumes.
    pub source: Option<Vec<u8>>,
    pub paths: Vec<Vec<u8>>,
    pub chown: Vec<u8>,
    pub chmod: Vec<u8>,
    pub scope: Scope,
}

/// An Agentfile's directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    Agent(Domain),
    Harness(Domain),
    Skill(Skill),
    Mcp(Mcp),
    Network(Network),
    Connect(Connect),
    Attach(Attach),
    Expose(Exposure),
    Volume(Volume),
}

/// What an `AGENT`, `HARNESS` or `MCP` `FROM` names (§12.2), told apart as written: a Git
/// URL (as BuildKit's git contexts read one), an http(s) URL, a path from the build
/// context (`.`, `./`, `../` or `/` first), or else an OCI reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git(crate::git::GitRef),
    Http(Vec<u8>),
    Path(Vec<u8>),
    Oci(Vec<u8>),
}

/// `src` told apart (§12.2). A path or URL takes no version: a final `:<tag>` on a path
/// is refused, for it would read as an OCI reference's.
pub fn source_of(src: &[u8]) -> Result<Source, Vec<u8>> {
    match crate::git::parse_git_ref(src) {
        crate::git::Parsed::Git(g) if !g.indistinguishable_from_local => return Ok(Source::Git(g)),
        crate::git::Parsed::BadGit(e) => return Err(e),
        _ => {}
    }
    if src.starts_with(b"http://") || src.starts_with(b"https://") {
        return Ok(Source::Http(src.to_vec()));
    }
    if src == b"." || src.starts_with(b"./") || src.starts_with(b"../") || src.starts_with(b"/") {
        let last = src.rsplit(|&b| b == b'/').next().unwrap_or_default();
        if let Some(i) = last.iter().rposition(|&b| b == b':') {
            let tag = last.get(i + 1..).unwrap_or_default();
            let tagged = !tag.is_empty()
                && tag.len() <= 128
                && tag
                    .first()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                && tag
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
            if tagged {
                return Err(errf(&[
                    go::quote(src).as_bytes(),
                    b" is a path, which takes no version: a :tag is an OCI reference's",
                ]));
            }
        }
        return Ok(Source::Path(src.to_vec()));
    }
    std::str::from_utf8(src)
        .ok()
        .and_then(|s| shards_image::reference::Reference::parse_normalized(s).ok())
        .map(|_| Source::Oci(src.to_vec()))
        .ok_or_else(|| {
            errf(&[
                go::quote(src).as_bytes(),
                b" is no path (./, ../ or /), Git or http(s) URL, or OCI reference",
            ])
        })
}

fn is(word: &[u8], keyword: &[u8]) -> bool {
    go::to_lower(word) == keyword
}

/// A name of an agent, harness, MCP server or network, lowercase: as a stage's.
fn name(word: &[u8], what: &str) -> Result<Vec<u8>, Vec<u8>> {
    let lower = go::to_lower(word);
    if !valid_stage_name(&lower) {
        return Err(errf(&[
            b"invalid name for ",
            what.as_bytes(),
            b": ",
            go::quote(word).as_bytes(),
            b", name can't start with a number or contain symbols",
        ]));
    }
    Ok(lower)
}

fn names(words: &[Vec<u8>], what: &str) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    words.iter().map(|w| name(w, what)).collect()
}

/// `--target-kind`'s value.
fn target_kind(req: &Req<'_>) -> Result<Option<TargetKind>, Vec<u8>> {
    match req.flags.value("target-kind").as_slice() {
        b"" => Ok(None),
        b"agent" => Ok(Some(TargetKind::Agent)),
        b"harness" => Ok(Some(TargetKind::Harness)),
        other => Err(errf(&[
            b"--target-kind is agent or harness, not ",
            go::quote(other).as_bytes(),
        ])),
    }
}

/// `args` split at the first word that is `keyword`: before it, and after it if it is there.
fn split_at<'a>(args: &'a [Vec<u8>], keyword: &[u8]) -> (&'a [Vec<u8>], Option<&'a [Vec<u8>]>) {
    match args.iter().position(|w| is(w, keyword)) {
        Some(at) => (args.get(..at).unwrap_or_default(), args.get(at + 1..)),
        None => (args, None),
    }
}

/// `FOR`'s names, which there must be if `FOR` is written.
fn for_names(after: Option<&[Vec<u8>]>, directive: &str) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    match after {
        None => Ok(Vec::new()),
        Some([]) => Err(format!("{directive} ... FOR names no one").into_bytes()),
        Some(words) => names(words, "an agent or harness"),
    }
}

/// `AGENT [--processes=<n|none>] <name> FROM <source> [TO <path>]`, and `HARNESS`'s, alike.
fn domain(req: &mut Req<'_>, directive: &str) -> Result<Domain, Vec<u8>> {
    req.flags.add("processes", FlagType::String, b"");
    req.flags.parse()?;
    let processes = match req.flags.value("processes").as_slice() {
        b"" => Processes::Unbounded,
        b"none" => Processes::None,
        n => match std::str::from_utf8(n).ok().and_then(|n| n.parse::<u32>().ok()) {
            Some(n) if n > 0 => Processes::AtMost(n),
            _ => {
                return Err(errf(&[
                    b"--processes is a count above 0 or none, not ",
                    go::quote(n).as_bytes(),
                ]));
            }
        },
    };
    let args = req.args.clone();
    // `AGENT AS main FROM …`: the name comes first, as `ARG` and `ENV` take theirs (§12.4).
    if let [first, n, ..] = args.as_slice()
        && is(first, b"as")
    {
        let rest = args.get(2..).unwrap_or_default();
        let mut fixed = format!("{directive} {}", String::from_utf8_lossy(n));
        for w in rest {
            fixed.push(' ');
            fixed.push_str(&String::from_utf8_lossy(w));
        }
        return Err(format!("{directive} takes its name first, without AS: {fixed}").into_bytes());
    }
    let usage = || {
        format!("{directive} requires a name and FROM <source>: {directive} <name> FROM <source> [TO <path>]")
            .into_bytes()
    };
    let [n, from, source, rest @ ..] = args.as_slice() else {
        return Err(usage());
    };
    if !is(from, b"from") {
        return Err(usage());
    }
    let to = match rest {
        [] => None,
        [to, path] if is(to, b"to") => Some(path.clone()),
        _ => return Err(usage()),
    };
    let what = if directive == "AGENT" {
        "an agent"
    } else {
        "a harness"
    };
    Ok(Domain {
        name: name(n, what)?,
        source: source.clone(),
        to,
        processes,
    })
}

/// `SKILL [ADD's options] [--from=…] [--target-kind=…] <source> [<dest>] [FOR <name>…]`.
fn skill(req: &mut Req<'_>) -> Result<Skill, Vec<u8>> {
    req.flags.add("chown", FlagType::String, b"");
    req.flags.add("chmod", FlagType::String, b"");
    req.flags.add("link", FlagType::Bool, b"false");
    req.flags.add("keep-git-dir", FlagType::Bool, b"false");
    req.flags.add("checksum", FlagType::String, b"");
    req.flags.add("exclude", FlagType::Strings, b"");
    req.flags.add("from", FlagType::String, b"");
    req.flags.add("target-kind", FlagType::String, b"");
    req.flags.parse()?;
    let all = req.args.clone();
    let (taken, after) = split_at(&all, b"for");
    let usage =
        b"SKILL takes one source and a destination if any: SKILL <source> [<dest>] [FOR <name> ...]".to_vec();
    if taken.is_empty() || taken.len() > 2 {
        return Err(usage);
    }
    // A destination, when given, is what `sources` splits off last; else one is made up
    // and set aside, as `sources` takes a list of sources and their destination.
    let dest = taken.get(1).cloned();
    req.args = [
        taken.first().cloned().unwrap_or_default(),
        dest.clone().unwrap_or_else(|| b"/".to_vec()),
    ]
    .to_vec();
    let mut found = sources(req, "SKILL")?;
    let source = match (found.paths.pop(), found.contents.pop()) {
        (Some(path), None) => SkillSource::Path(path),
        (None, Some(text)) => SkillSource::Text(text),
        _ => return Err(usage),
    };
    let f = &req.flags;
    Ok(Skill {
        source,
        dest,
        from: f.value("from"),
        chown: f.value("chown"),
        chmod: f.value("chmod"),
        link: f.is_true("link"),
        exclude: f.values("exclude"),
        keep_git_dir: f.used("keep-git-dir").then(|| f.is_true("keep-git-dir")),
        checksum: f.value("checksum"),
        scope: Scope {
            kind: target_kind(req)?,
            names: for_names(after, "SKILL")?,
        },
    })
}

/// `MCP [--target-kind=…] <name> FROM <source> [FOR <name>…]`.
fn mcp(req: &mut Req<'_>) -> Result<Mcp, Vec<u8>> {
    req.flags.add("target-kind", FlagType::String, b"");
    req.flags.parse()?;
    let all = req.args.clone();
    let (declared, after) = split_at(&all, b"for");
    let [n, from, source] = declared else {
        return Err(
            b"MCP requires a name and FROM <source>: MCP <name> FROM <source> [FOR <name> ...]".to_vec(),
        );
    };
    if !is(from, b"from") {
        return Err(
            b"MCP requires a name and FROM <source>: MCP <name> FROM <source> [FOR <name> ...]".to_vec(),
        );
    }
    Ok(Mcp {
        name: name(n, "an MCP server")?,
        source: source.clone(),
        scope: Scope {
            kind: target_kind(req)?,
            names: for_names(after, "MCP")?,
        },
    })
}

/// `NETWORK [options] <name> [FOR <name>…]`, its options Compose's network's, as
/// `docker network create` names them, and `--expose`, `--ingress` and `--egress`.
fn network(req: &mut Req<'_>) -> Result<Network, Vec<u8>> {
    for (flag, kind) in [
        ("driver", FlagType::String),
        ("opt", FlagType::Strings),
        ("attachable", FlagType::Bool),
        ("internal", FlagType::Bool),
        ("external", FlagType::Bool),
        ("ipv4", FlagType::Bool),
        ("ipv6", FlagType::Bool),
        ("label", FlagType::Strings),
        ("ipam-driver", FlagType::String),
        ("ipam-opt", FlagType::Strings),
        ("subnet", FlagType::Strings),
        ("ip-range", FlagType::Strings),
        ("gateway", FlagType::Strings),
        ("aux-address", FlagType::Strings),
        ("expose", FlagType::Strings),
        ("ingress", FlagType::Strings),
        ("egress", FlagType::Strings),
        ("dns", FlagType::Bool),
        ("protocol", FlagType::Strings),
        ("target-kind", FlagType::String),
    ] {
        req.flags
            .add(flag, kind, if kind == FlagType::Bool { b"false" } else { b"" });
    }
    req.flags.parse()?;
    let all = req.args.clone();
    let (declared, after) = split_at(&all, b"for");
    let [n] = declared else {
        return Err(b"NETWORK requires one name: NETWORK [options] <name> [FOR <name> ...]".to_vec());
    };
    let f = &req.flags;
    let bool_used = |flag: &str| f.used(flag).then(|| f.is_true(flag));
    // The ports in the order given, each flag's in turn.
    let mut ports = Vec::new();
    for (flag, direction) in [
        ("expose", Direction::Both),
        ("ingress", Direction::Ingress),
        ("egress", Direction::Egress),
    ] {
        ports.extend(f.values(flag).into_iter().map(|p| (p, direction)));
    }
    // What it carries: each `--protocol`, a comma-separated list.
    let mut protocols = 0u8;
    for v in f.values("protocol") {
        for p in v.split(|&b| b == b',') {
            let p = go::to_lower(p.trim_ascii());
            let Some(i) = PROTOCOLS.iter().position(|q| *q == p.as_slice()) else {
                return Err(errf(&[
                    b"NETWORK --protocol=",
                    &v,
                    b": no protocol it may carry (tcp, udp, unix)",
                ]));
            };
            protocols |= 1 << i;
        }
    }
    Ok(Network {
        name: name(n, "a network")?,
        driver: f.value("driver"),
        driver_opts: f.values("opt"),
        attachable: f.is_true("attachable"),
        internal: f.is_true("internal"),
        external: f.is_true("external"),
        ipv4: bool_used("ipv4"),
        ipv6: bool_used("ipv6"),
        labels: f.values("label"),
        ipam_driver: f.value("ipam-driver"),
        ipam_opts: f.values("ipam-opt"),
        subnets: f.values("subnet"),
        ip_ranges: f.values("ip-range"),
        gateways: f.values("gateway"),
        aux_addresses: f.values("aux-address"),
        dns: f.is_true("dns"),
        ports,
        protocols,
        scope: Scope {
            kind: target_kind(req)?,
            names: for_names(after, "NETWORK")?,
        },
    })
}

/// `CONNECT [--target-kind=…] <a>… (WITH|TO) <b>… ON <network>…`.
fn connect(req: &mut Req<'_>) -> Result<Connect, Vec<u8>> {
    req.flags.add("target-kind", FlagType::String, b"");
    req.flags.add("port", FlagType::Strings, b"");
    req.flags.parse()?;
    let usage = || {
        b"CONNECT requires names, WITH or TO, names, and ON networks: CONNECT <a> ... (WITH|TO) <b> ... ON <network> ...".to_vec()
    };
    let all = req.args.clone();
    let (ends, on) = split_at(&all, b"on");
    let on = match on {
        Some(nets) if !nets.is_empty() => names(nets, "a network")?,
        _ => return Err(usage()),
    };
    let with = ends.iter().position(|w| is(w, b"with"));
    let to = ends.iter().position(|w| is(w, b"to"));
    let (at, both_ways) = match (with, to) {
        (Some(at), None) => (at, true),
        (None, Some(at)) => (at, false),
        (Some(_), Some(_)) => return Err(b"CONNECT takes WITH or TO, not both".to_vec()),
        (None, None) => return Err(usage()),
    };
    let from = ends.get(..at).unwrap_or_default();
    let peers = ends.get(at + 1..).unwrap_or_default();
    if from.is_empty() || peers.is_empty() {
        return Err(usage());
    }
    let ports = req.flags.values("port");
    for p in &ports {
        if protocol_of(p).is_none() {
            return Err(errf(&[
                b"CONNECT --port=",
                p,
                b": no port, range of ports or Unix socket (8080, 53/udp, 8000-8010/tcp, unix:<name>)",
            ]));
        }
    }
    let from = names(from, "an agent or harness")?;
    let to = names(peers, "an agent or harness")?;
    // Networks are default deny: a CONNECT between agents that names no port grants
    // nothing, and is refused rather than read as a grant.
    let alone = from.iter().chain(&to).all(|n| Some(n) == from.first());
    if ports.is_empty() && !alone {
        return Err(b"CONNECT between agents grants no port: name what the receiving side accepts with --port=<port>[/tcp|/udp] or --port=unix:<name> (networks are default deny)".to_vec());
    }
    Ok(Connect {
        kind: target_kind(req)?,
        from,
        both_ways,
        to,
        on,
        ports,
    })
}

/// `ATTACH <agent>… FOR <harness>…`.
fn attach(req: &mut Req<'_>) -> Result<Attach, Vec<u8>> {
    req.flags.parse()?;
    let all = req.args.clone();
    let (agents, after) = split_at(&all, b"for");
    match after {
        Some(harnesses) if !agents.is_empty() && !harnesses.is_empty() => Ok(Attach {
            agents: names(agents, "an agent")?,
            harnesses: names(harnesses, "a harness")?,
        }),
        _ => Err(b"ATTACH requires agents and FOR harnesses: ATTACH <agent> ... FOR <harness> ...".to_vec()),
    }
}

/// `EXPOSE [--agents=…] [--harnesses=…] [--mcps=…] <port>… [AS <ingress|egress>]
/// [FOR <network>…]`, where it is written with either keyword: `None` for a Dockerfile's
/// `EXPOSE`. `members` are the three flags' names, each list given comma-separated.
pub(crate) fn exposure(args: &[Vec<u8>], members: [Vec<Vec<u8>>; 3]) -> Result<Option<Exposure>, Vec<u8>> {
    let as_at = args.iter().position(|w| is(w, b"as"));
    let for_at = args.iter().position(|w| is(w, b"for"));
    let [agents, harnesses, mcps] = members;
    if as_at.is_none() && for_at.is_none() {
        if !agents.is_empty() || !harnesses.is_empty() || !mcps.is_empty() {
            return Err(b"EXPOSE --agents, --harnesses and --mcps name members of the networks after FOR: EXPOSE --agents=<agent> <port> AS ingress FOR <network>".to_vec());
        }
        return Ok(None);
    }
    if !mcps.is_empty() {
        return Err(b"EXPOSE --mcps: MCP servers do not join networks yet (architecture.md D122); name agents with --agents and harnesses with --harnesses".to_vec());
    }
    let ports_end = as_at.or(for_at).unwrap_or(args.len());
    let ports = args.get(..ports_end).unwrap_or_default().to_vec();
    if ports.is_empty() {
        return Err(b"EXPOSE requires at least one port before AS or FOR".to_vec());
    }
    let direction = match as_at {
        None => Direction::Both,
        Some(at) => {
            let dir = args
                .get(at + 1)
                .ok_or(b"EXPOSE ... AS requires ingress or egress".to_vec())?;
            if for_at.is_some_and(|f| f != at + 2) || (for_at.is_none() && args.len() != at + 2) {
                return Err(b"EXPOSE ... AS takes one word, ingress or egress, then FOR if any".to_vec());
            }
            match go::to_lower(dir).as_slice() {
                b"ingress" => Direction::Ingress,
                b"egress" => Direction::Egress,
                _ => {
                    return Err(errf(&[
                        b"EXPOSE ... AS is ingress or egress, not ",
                        go::quote(dir).as_bytes(),
                    ]));
                }
            }
        }
    };
    let networks = match for_at {
        None => Vec::new(),
        Some(at) => match args.get(at + 1..) {
            Some(nets) if !nets.is_empty() => names(nets, "a network")?,
            _ => return Err(b"EXPOSE ... FOR names no network".to_vec()),
        },
    };
    // A mapping, the microVM's port then its member's, is ingress's alone, and one port
    // each side (D122).
    for p in ports.iter().filter(|p| p.contains(&b':')) {
        if direction != Direction::Ingress {
            return Err(errf(&[
                b"EXPOSE ",
                p,
                b": a port mapped to a member's, <port>:<member_port>, is ingress's alone; write AS ingress",
            ]));
        }
        let one = |q: &[u8]| port_range(q).is_some_and(|(_, lo, hi)| lo == hi);
        if !one(&outside(p)) || !one(&inside(p)) {
            return Err(errf(&[
                b"EXPOSE ",
                p,
                b": a mapping is one port to one port, <port>:<member_port>[/tcp|/udp]",
            ]));
        }
    }
    let split = |list: Vec<Vec<u8>>, what: &str| -> Result<Vec<Vec<u8>>, Vec<u8>> {
        let words: Vec<Vec<u8>> = list
            .iter()
            .flat_map(|l| l.split(|&b| b == b','))
            .map(<[u8]>::to_vec)
            .collect();
        names(&words, what)
    };
    Ok(Some(Exposure {
        ports,
        direction,
        networks,
        agents: split(agents, "an agent")?,
        harnesses: split(harnesses, "a harness")?,
    }))
}

/// A Compose volume's name: `[a-zA-Z0-9][a-zA-Z0-9_.-]*` (compose-spec, `volumes`).
fn volume_name(w: &[u8]) -> bool {
    w.first().is_some_and(u8::is_ascii_alphanumeric)
        && w.iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Why an Agentfile's volume may not be where [`covers_domain_mounts`] says.
pub const COVERS: &str = "VOLUME may not cover a domain's root, /proc, /sys or /dev";

/// Whether a mount point, rooted at `/`, covers a domain's root or the file systems its
/// init mounts there, `/proc`, `/sys` and `/dev` (D109): an Agentfile's volumes may not,
/// and Docker's `VOLUME`s there stay the run's own root's.
pub fn covers_domain_mounts(path: &[u8]) -> bool {
    let at = crate::go::clean(&[b"/".as_slice(), path].concat());
    let covers = |dir: &[u8]| {
        at.strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
    };
    at == b"/" || [&b"/proc"[..], b"/sys", b"/dev"].iter().any(|d| covers(d))
}

/// `VOLUME [--chown=…] [--chmod=…] [--target-kind=…] <path>… [FOR <name>…]`, or a named
/// volume, `<name> <path>`, where the line takes an option, a name or `FOR`: `None` for a
/// Dockerfile's `VOLUME`. Flags are already declared on `req`.
pub(crate) fn volume(req: &mut Req<'_>) -> Result<Option<Volume>, Vec<u8>> {
    let all = req.args.clone();
    let (paths, after) = split_at(&all, b"for");
    let named = !req.flags_given() && after.is_none();
    let named_volume = matches!(paths, [n, _] if volume_name(n));
    if named && !named_volume {
        return Ok(None);
    }
    if paths.is_empty() {
        return Err(b"VOLUME requires at least one path before FOR".to_vec());
    }
    let (source, paths) = match paths {
        // As the engine refuses it when the run makes the volume (moby volume/local).
        [n, _] if volume_name(n) && n.len() < 2 => {
            return Err(
                b"volume name is too short, names should be at least two alphanumeric characters".to_vec(),
            );
        }
        [n, dest] if volume_name(n) => (Some(n.clone()), vec![dest.clone()]),
        _ => (None, paths.to_vec()),
    };
    // Each mount point is given to agents and harnesses (§4.5, D109): their root and their
    // system's mounts are their own.
    if let Some(p) = paths.iter().find(|p| covers_domain_mounts(p)) {
        return Err([COVERS.as_bytes(), b": ", p].concat());
    }
    let f = &req.flags;
    Ok(Some(Volume {
        source,
        paths,
        chown: f.value("chown"),
        chmod: f.value("chmod"),
        scope: Scope {
            kind: target_kind(req)?,
            names: for_names(after, "VOLUME")?,
        },
    }))
}

/// An Agentfile directive's line, by its lowercase name: `None` where `name` names none.
pub(crate) fn directive(name: &[u8], req: &mut Req<'_>) -> Option<Result<Directive, Vec<u8>>> {
    Some(match name {
        b"agent" => domain(req, "AGENT").map(Directive::Agent),
        b"harness" => domain(req, "HARNESS").map(Directive::Harness),
        b"skill" => skill(req).map(Directive::Skill),
        b"mcp" => mcp(req).map(Directive::Mcp),
        b"network" => network(req).map(Directive::Network),
        b"connect" => connect(req).map(Directive::Connect),
        b"attach" => attach(req).map(Directive::Attach),
        _ => return None,
    })
}

/// What a name of the namespace stages, agents and harnesses share was declared as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Declared {
    Stage,
    Agent,
    Harness,
}

impl Declared {
    fn word(self) -> &'static str {
        match self {
            Declared::Stage => "a stage",
            Declared::Agent => "an agent",
            Declared::Harness => "a harness",
        }
    }
}

/// A file's names, as the stage at hand sees them: its lineage's agents, harnesses, MCP
/// servers and networks (§7 Q19.5), every stage's alias.
#[derive(Default, Clone)]
struct Seen {
    domains: std::collections::BTreeMap<Vec<u8>, (Declared, usize)>,
    networks: std::collections::BTreeMap<Vec<u8>, (Vec<Vec<u8>>, usize)>,
    /// Where its `VOLUME`s mount, rooted as Docker roots them, and on which line.
    mounts: std::collections::BTreeMap<Vec<u8>, usize>,
}

fn at(line: usize, message: String) -> crate::instructions::Error {
    crate::instructions::Error {
        message: format!("dockerfile parse error on line {line}: {message}").into_bytes(),
        location: vec![vec![(line, line)]],
    }
}

fn shown(name: &[u8]) -> String {
    go::quote(name)
}

/// Checks an Agentfile's names (§4.10, §7 Q19, §12.10, §12.11): stages, agents and
/// harnesses share one namespace, each name declared once; MCP servers and networks are
/// declared once each; every name a grant uses is declared in the stage's lineage, of the
/// kind `--target-kind` says; `ATTACH` names agents, then harnesses; `CONNECT`'s names are
/// of one kind, on networks declared, and allowed by each network's `FOR`.
pub fn check(ins: &crate::instructions::Instructions) -> Result<(), crate::instructions::Error> {
    use crate::instructions::Kind;
    let mut file: std::collections::BTreeMap<Vec<u8>, (Declared, usize)> = std::collections::BTreeMap::new();
    let mut mcps: std::collections::BTreeMap<Vec<u8>, usize> = std::collections::BTreeMap::new();
    let mut networks_anywhere: std::collections::BTreeMap<Vec<u8>, usize> = std::collections::BTreeMap::new();
    let mut lineages: Vec<Seen> = Vec::with_capacity(ins.stages.len());
    let mut declare = |name: &[u8], what: Declared, line: usize| -> Result<(), crate::instructions::Error> {
        match file.get(name) {
            // Two stages of one name are BuildKit's lint, not an error.
            Some((Declared::Stage, _)) if what == Declared::Stage => Ok(()),
            Some(&(was, first)) => Err(at(
                line,
                format!(
                    "{} names {} already, declared at line {first}: stages, agents and harnesses share one namespace",
                    shown(name),
                    was.word()
                ),
            )),
            None => {
                file.insert(name.to_vec(), (what, line));
                Ok(())
            }
        }
    };
    for (i, stage) in ins.stages.iter().enumerate() {
        let line = stage.location.first().map_or(0, |l| l.0);
        if !stage.name.is_empty() {
            declare(&stage.name, Declared::Stage, line)?;
        }
        // Its lineage: the stage it is built from, if one of the file's.
        let base = go::to_lower(&stage.base_name);
        let mut seen = ins
            .stages
            .iter()
            .take(i)
            .rposition(|s| !s.name.is_empty() && s.name == base)
            .and_then(|j| lineages.get(j).cloned())
            .unwrap_or_default();
        for c in &stage.commands {
            let Kind::Agentfile(d) = &c.kind else { continue };
            let line = c.location.first().map_or(0, |l| l.0);
            match d {
                Directive::Agent(a) => {
                    declare(&a.name, Declared::Agent, line)?;
                    seen.domains.insert(a.name.clone(), (Declared::Agent, line));
                }
                Directive::Harness(h) => {
                    declare(&h.name, Declared::Harness, line)?;
                    seen.domains.insert(h.name.clone(), (Declared::Harness, line));
                }
                Directive::Mcp(m) => {
                    if let Some(first) = mcps.insert(m.name.clone(), line) {
                        return Err(at(
                            line,
                            format!("MCP {}: declared already, at line {first}", shown(&m.name)),
                        ));
                    }
                    scope(&seen, &m.scope, line, "MCP")?;
                }
                Directive::Network(n) => {
                    if let Some(first) = networks_anywhere.insert(n.name.clone(), line) {
                        return Err(at(
                            line,
                            format!("NETWORK {}: declared already, at line {first}", shown(&n.name)),
                        ));
                    }
                    scope(&seen, &n.scope, line, "NETWORK")?;
                    seen.networks
                        .insert(n.name.clone(), (n.scope.names.clone(), line));
                }
                Directive::Skill(s) => scope(&seen, &s.scope, line, "SKILL")?,
                Directive::Volume(v) => {
                    scope(&seen, &v.scope, line, "VOLUME")?;
                    // One volume at a mount point: a domain is given what is mounted
                    // there, so a second would be given to the first's domains.
                    for p in &v.paths {
                        let rooted = go::clean(&[b"/".as_slice(), p].concat());
                        if let Some(first) = seen.mounts.insert(rooted, line) {
                            return Err(at(
                                line,
                                format!(
                                    "VOLUME {}: a VOLUME mounts a volume there already, at line {first}",
                                    shown(p)
                                ),
                            ));
                        }
                    }
                }
                Directive::Expose(e) => {
                    for n in &e.networks {
                        declared_network(&seen, n, line, "EXPOSE")?;
                    }
                    for n in &e.agents {
                        kind_of(&seen, n, Some(TargetKind::Agent), line, "EXPOSE --agents")?;
                    }
                    for n in &e.harnesses {
                        kind_of(&seen, n, Some(TargetKind::Harness), line, "EXPOSE --harnesses")?;
                    }
                }
                Directive::Attach(a) => {
                    for n in &a.agents {
                        kind_of(&seen, n, Some(TargetKind::Agent), line, "ATTACH")?;
                    }
                    for n in &a.harnesses {
                        kind_of(&seen, n, Some(TargetKind::Harness), line, "ATTACH ... FOR")?;
                    }
                }
                Directive::Connect(cn) => {
                    let mut kinds = Vec::new();
                    for n in cn.from.iter().chain(&cn.to) {
                        kinds.push((n, kind_of(&seen, n, cn.kind, line, "CONNECT")?));
                    }
                    if let Some((first, k)) = kinds.first()
                        && let Some((other, _)) = kinds.iter().find(|(_, o)| o != k)
                    {
                        return Err(at(
                            line,
                            format!(
                                "CONNECT joins one kind: {} is {} and {} is not; write a CONNECT for each",
                                shown(first),
                                if *k == TargetKind::Agent {
                                    "an agent"
                                } else {
                                    "a harness"
                                },
                                shown(other)
                            ),
                        ));
                    }
                    for net in &cn.on {
                        let allowed = declared_network(&seen, net, line, "CONNECT ... ON")?;
                        if allowed.is_empty() {
                            continue;
                        }
                        for n in cn.from.iter().chain(&cn.to) {
                            if !allowed.contains(n) {
                                let declared_at = seen.networks.get(net).map_or(0, |n| n.1);
                                return Err(at(
                                    line,
                                    format!(
                                        "CONNECT puts {} on network {}, whose FOR at line {declared_at} does not allow it",
                                        shown(n),
                                        shown(net)
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
        lineages.push(seen);
    }
    Ok(())
}

/// The kind of the domain `name` names in `seen`, which must be `want` where it is given.
fn kind_of(
    seen: &Seen,
    name: &[u8],
    want: Option<TargetKind>,
    line: usize,
    directive: &str,
) -> Result<TargetKind, crate::instructions::Error> {
    let kind = match seen.domains.get(name) {
        Some((Declared::Agent, _)) => TargetKind::Agent,
        Some((Declared::Harness, _)) => TargetKind::Harness,
        _ => {
            return Err(at(
                line,
                format!(
                    "{directive} names {}, which no AGENT or HARNESS of this stage's lineage declares",
                    shown(name)
                ),
            ));
        }
    };
    match want {
        Some(w) if w != kind => {
            let (is, said) = match kind {
                TargetKind::Agent => ("an agent", "harness"),
                TargetKind::Harness => ("a harness", "agent"),
            };
            Err(at(
                line,
                format!("{directive} names {} as a {said}, but it is {is}", shown(name)),
            ))
        }
        _ => Ok(kind),
    }
}

/// Each name `scope` grants to, checked as [`kind_of`] checks one.
fn scope(seen: &Seen, scope: &Scope, line: usize, directive: &str) -> Result<(), crate::instructions::Error> {
    for n in &scope.names {
        kind_of(seen, n, scope.kind, line, &format!("{directive} ... FOR"))?;
    }
    Ok(())
}

/// The names network `name` allows, none for every one, if the lineage declares it.
fn declared_network<'s>(
    seen: &'s Seen,
    name: &[u8],
    line: usize,
    directive: &str,
) -> Result<&'s [Vec<u8>], crate::instructions::Error> {
    seen.networks.get(name).map(|n| n.0.as_slice()).ok_or_else(|| {
        at(
            line,
            format!(
                "{directive} names network {}, which no NETWORK of this stage's lineage declares",
                shown(name)
            ),
        )
    })
}

/// Where a built image keeps its normalized Agentfile, outside every domain (§8, D35).
pub const SPEC_PATH: &[u8] = b"/.agentfile.json";

/// The label namespace shards' build owns (§8, D59): every label the build writes of an
/// Agentfile begins with it (the digest, the egress, DNS and MCP grants). A `--label` on a
/// `build` or a `run` may not set one: they are the build's record of the Agentfile, which
/// the daemon and the guest trust, not the user's to forge (both build- and run-time, as
/// the user mutates a run through the CLI). Docker reserves no label, so this is shards'
/// own (recorded as a deviation).
pub const LABEL_PREFIX: &[u8] = b"vnd.osi.agentfile.";

/// Whether `key` is in the namespace shards' build owns ([`LABEL_PREFIX`]).
pub fn reserved_label(key: &[u8]) -> bool {
    key.starts_with(LABEL_PREFIX)
}

/// The config label that carries the normalized Agentfile's digest (§8): `vnd.osi`, as the
/// OSI's media types (`application/vnd.osi.agent.v1`).
pub const DIGEST_LABEL: &[u8] = b"vnd.osi.agentfile.digest";

/// The manifest annotations of an Agentfile's image (D57), by which a registry's
/// listing finds it without fetching its config: the normalized Agentfile's digest, and
/// its agents' and harnesses' names, each list comma-separated in name order.
pub const AGENTS_ANNOTATION: &[u8] = b"vnd.osi.agentfile.agents";
pub const HARNESSES_ANNOTATION: &[u8] = b"vnd.osi.agentfile.harnesses";

/// The label of an Agentfile's egress grants (D59), the ports [`egress`] finds,
/// comma-separated: the union its microVM's network process allows, each agent held to
/// its own by its microVM's switch.
pub const EGRESS_LABEL: &[u8] = b"vnd.osi.agentfile.egress";

/// A port as Docker writes one (`443`, `53/udp`, `8000-8010/tcp`): its protocol's IP
/// number (6 TCP, 17 UDP, TCP where none is said) and its range's ends.
pub fn port_range(s: &[u8]) -> Option<(u8, u16, u16)> {
    let s = std::str::from_utf8(s).ok()?;
    let (range, proto) = match s.rsplit_once('/') {
        Some((r, "tcp")) => (r, 6),
        Some((r, "udp")) => (r, 17),
        Some(_) => return None,
        None => (s, 6),
    };
    let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p > 0);
    let (lo, hi) = match range.split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => (port(range)?, port(range)?),
    };
    (lo <= hi).then_some((proto, lo, hi))
}

/// The ports network `net` lets cross the microVM's boundary, outward (egress) or inward
/// (ingress): two boundaries, each its own grant (§12 answer 6), and a flow crossing both
/// needs both. The network's own (`NETWORK --expose`, and `--egress` or `--ingress`) and
/// the microVM's for it (`EXPOSE ... FOR` it, both ways or `AS` that direction), as
/// ranges of each protocol that both open; none for an internal network.
pub fn boundary(directives: &[Directive], net: &[u8], outward: bool) -> Vec<(u8, u16, u16)> {
    let away = if outward {
        Direction::Ingress
    } else {
        Direction::Egress
    };
    let mut ours = Vec::new();
    for d in directives {
        if let Directive::Network(n) = d
            && n.name == net
        {
            if n.internal {
                return Vec::new();
            }
            ours.extend(
                n.ports
                    .iter()
                    .filter(|(_, d)| *d != away)
                    .filter_map(|(p, _)| port_range(p)),
            );
        }
    }
    let mut vms = Vec::new();
    // A port mapped to a member's (D122): the microVM's, where the network lets the
    // member's in.
    let mut mapped = Vec::new();
    for d in directives {
        if let Directive::Expose(e) = d
            && e.direction != away
            && e.networks.iter().any(|n| n == net)
        {
            for p in &e.ports {
                let (o, i) = (outside(p), inside(p));
                match (port_range(&o), port_range(&i)) {
                    (Some(o), Some(i)) if o != i => mapped.push((o, i)),
                    (Some(o), _) => vms.push(o),
                    _ => {}
                }
            }
        }
    }
    let mut out: Vec<(u8, u16, u16)> = Vec::new();
    for &(p, lo, hi) in &ours {
        for &(q, a, b) in &vms {
            let (lo, hi) = (lo.max(a), hi.min(b));
            if p == q && lo <= hi && !out.contains(&(p, lo, hi)) {
                out.push((p, lo, hi));
            }
        }
    }
    for (o, (q, a, b)) in mapped {
        if ours.iter().any(|&(p, lo, hi)| p == q && lo <= a && b <= hi) && !out.contains(&o) {
            out.push(o);
        }
    }
    out
}

/// The label of an Agentfile whose networks grant names past the microVM (`NETWORK --dns`
/// on one that is not internal, which a `CONNECT` joins): its microVM's network process
/// then asks the host's resolvers for any name; else only remote MCP servers' own.
pub const DNS_LABEL: &[u8] = b"vnd.osi.agentfile.dns";

/// Whether an Agentfile grants names past the microVM ([`DNS_LABEL`]).
pub fn dns(directives: &[Directive]) -> bool {
    directives.iter().any(|d| {
        matches!(d, Directive::Network(n) if n.dns && !n.internal
            && directives.iter().any(|c| matches!(c, Directive::Connect(c) if c.on.contains(&n.name))))
    })
}

/// The label of an Agentfile's remote MCP servers (§4.4, §9.6), each `host:port`,
/// comma-separated: its microVM's network process lets a flow to that port reach only the
/// addresses the host resolves to (or the address it is), for the agents granted it.
pub const MCP_LABEL: &[u8] = b"vnd.osi.agentfile.mcp";

/// A remote MCP server's host and port, from its URL: the port written, or else its
/// scheme's own (443 for `https`, 80 for `http`; §4.4, decided 2026-10-02). None for a
/// source that is no http(s) URL, or one with no host.
pub fn mcp_endpoint(url: &[u8]) -> Option<(Vec<u8>, u16)> {
    let (rest, default) = match url.strip_prefix(b"https://") {
        Some(r) => (r, 443),
        None => (url.strip_prefix(b"http://")?, 80),
    };
    let authority = rest.split(|&b| b == b'/' || b == b'?' || b == b'#').next()?;
    let hostport = authority.rsplit(|&b| b == b'@').next()?;
    let (host, port) = match hostport.iter().rposition(|&b| b == b':') {
        Some(i) => {
            let port = std::str::from_utf8(hostport.get(i + 1..)?)
                .ok()?
                .parse::<u16>()
                .ok()?;
            (hostport.get(..i)?, port)
        }
        None => (hostport, default),
    };
    (!host.is_empty() && port > 0).then(|| (host.to_ascii_lowercase(), port))
}

/// The remote MCP servers an Agentfile declares, each `host:port` once.
pub fn remote_mcp(directives: &[Directive]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for d in directives {
        if let Directive::Mcp(m) = d
            && let Some((host, port)) = mcp_endpoint(&m.source)
        {
            let e = [host.as_slice(), b":", port.to_string().as_bytes()].concat();
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    out
}

/// The label of the ports an Agentfile declares `EXPOSE ... AS egress` alone (§12 answer
/// 6): `shards run -p` refuses to publish them, as an egress port is a destination, not a
/// listener.
pub const EGRESS_DECLARED_LABEL: &[u8] = b"vnd.osi.agentfile.egress-declared";

/// The ports an Agentfile declares `EXPOSE ... AS egress` and nowhere both ways or for
/// ingress, as Docker writes them, each once.
pub fn egress_declared(directives: &[Directive]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let inward: Vec<Vec<u8>> = directives
        .iter()
        .filter_map(|d| match d {
            Directive::Expose(e) if e.direction != Direction::Egress => {
                Some(e.ports.iter().map(|p| outside(p)))
            }
            _ => None,
        })
        .flatten()
        .collect();
    for d in directives {
        if let Directive::Expose(e) = d
            && e.direction == Direction::Egress
        {
            for p in &e.ports {
                if !inward.contains(p) && !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
    }
    out
}

/// Each `CONNECT`'s flows held to its networks (default deny): every port it grants must be
/// one each network it is on lets in to its members (`NETWORK --ingress` or `--expose`),
/// a network it is on must be declared, and `--dns` names a resolver past the microVM,
/// A port of a protocol network `net` does not carry, refused.
fn carried(net: &[u8], what: &[u8], p: &[u8], protocol: &[u8]) -> Vec<u8> {
    [
        what,
        p,
        b" on network ",
        net,
        b": the network does not carry ",
        protocol,
        b"; name it with NETWORK --protocol (TCP and UDP where none is said; networks are default deny)",
    ]
    .concat()
}

/// which an internal network has none of.
pub fn connections(directives: &[Directive]) -> Result<(), Vec<u8>> {
    // Each port a network opens is of a protocol it carries, and a Unix socket never
    // leaves the microVM.
    for d in directives {
        let Directive::Network(n) = d else { continue };
        for (p, direction) in &n.ports {
            let flag: &[u8] = match direction {
                Direction::Both => b"--expose=",
                Direction::Ingress => b"--ingress=",
                Direction::Egress => b"--egress=",
            };
            let Some(protocol) = protocol_of(p) else {
                return Err([
                    b"NETWORK ".as_slice(),
                    flag,
                    p,
                    b" on network ",
                    &n.name,
                    b": no port, range of ports or Unix socket (8080, 53/udp, 8000-8010/tcp, unix:<name>)",
                ]
                .concat());
            };
            if protocol == b"unix" && *direction == Direction::Egress {
                return Err([
                    b"NETWORK --egress=".as_slice(),
                    p,
                    b" on network ",
                    &n.name,
                    b": a Unix socket never leaves the microVM; let it in to the network's members with --ingress",
                ]
                .concat());
            }
            if !carries(n, protocol) {
                return Err(carried(&n.name, flag, p, protocol));
            }
        }
    }
    for d in directives {
        let Directive::Expose(e) = d else { continue };
        for net in &e.networks {
            let Some(n) = directives.iter().find_map(|d| match d {
                Directive::Network(n) if n.name == *net => Some(n),
                _ => None,
            }) else {
                continue;
            };
            for p in &e.ports {
                if let Some(protocol) = protocol_of(p)
                    && !carries(n, protocol)
                {
                    return Err(carried(net, b"EXPOSE ", p, protocol));
                }
            }
        }
    }
    for d in directives {
        if let Directive::Network(n) = d
            && n.dns
            && n.internal
        {
            return Err([
                b"NETWORK --dns --internal ".as_slice(),
                &n.name,
                b": an internal network has no resolver past the microVM to ask",
            ]
            .concat());
        }
    }
    for d in directives {
        let Directive::Connect(c) = d else { continue };
        for net in &c.on {
            let Some(n) = directives.iter().find_map(|d| match d {
                Directive::Network(n) if n.name == *net => Some(n),
                _ => None,
            }) else {
                return Err([b"CONNECT ... ON ".as_slice(), net, b": no NETWORK declares it"].concat());
            };
            let inward: Vec<(u8, u16, u16)> = n
                .ports
                .iter()
                .filter(|(_, d)| *d != Direction::Egress)
                .filter_map(|(p, _)| port_range(p))
                .collect();
            for p in &c.ports {
                if let Some(name) = unix_endpoint(p) {
                    if !carries(n, b"unix") {
                        return Err(carried(net, b"CONNECT --port=", p, b"unix"));
                    }
                    let opened = n
                        .ports
                        .iter()
                        .any(|(q, d)| *d != Direction::Egress && unix_endpoint(q) == Some(name));
                    if !opened {
                        return Err([
                            b"CONNECT --port=".as_slice(),
                            p,
                            b" on network ",
                            net,
                            b": the network lets no such Unix socket in to its members; open it with NETWORK --ingress=",
                            p,
                            b" (networks are default deny)",
                        ]
                        .concat());
                    }
                    continue;
                }
                let Some((proto, lo, hi)) = port_range(p) else {
                    continue;
                };
                if !inward.iter().any(|&(q, a, b)| q == proto && a <= lo && hi <= b) {
                    return Err([
                        b"CONNECT --port=".as_slice(),
                        p,
                        b" on network ",
                        net,
                        b": the network lets no such port in to its members; open it with NETWORK --ingress or --expose (networks are default deny)",
                    ]
                    .concat());
                }
            }
        }
    }
    Ok(())
}

/// Who a port coming in past the microVM reaches (D122): the microVM's range, the
/// member's (the same unless an `EXPOSE` maps one to the other), and the member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    pub at: (u8, u16, u16),
    pub to: (u8, u16, u16),
    pub kind: TargetKind,
    pub name: Vec<u8>,
}

/// What an Agentfile lets in past its microVM (§4.1, §4.6, §12 answer 6, D122): a
/// network's ports both boundaries open inward reach one member each, the one an `EXPOSE`
/// names for them (`--agents`, `--harnesses`), else the network's one member. A member
/// named on no network its `EXPOSE` is for, a port two members claim, and a port of a
/// network of several that names none are refused, naming them, rather than guessed at;
/// else each port's receiver.
pub fn ingress(directives: &[Directive]) -> Result<Vec<Receiver>, Vec<u8>> {
    // Each name's kind: as a CONNECT says it, else the one kind it is declared as.
    let declared = |name: &[u8], kind: TargetKind| {
        directives.iter().any(|d| match (d, kind) {
            (Directive::Agent(a), TargetKind::Agent) => a.name == name,
            (Directive::Harness(h), TargetKind::Harness) => h.name == name,
            _ => false,
        })
    };
    let kind = |name: &[u8], said: Option<TargetKind>| {
        said.or_else(|| {
            match (
                declared(name, TargetKind::Agent),
                declared(name, TargetKind::Harness),
            ) {
                (true, false) => Some(TargetKind::Agent),
                (false, true) => Some(TargetKind::Harness),
                _ => None,
            }
        })
    };
    type Member<'a> = (Option<TargetKind>, &'a [u8]);
    let mut nets: Vec<(&[u8], Vec<Member<'_>>)> = Vec::new();
    for d in directives {
        if let Directive::Connect(c) = d {
            for n in &c.on {
                let at = match nets.iter().position(|(x, _)| *x == n.as_slice()) {
                    Some(at) => at,
                    None => {
                        nets.push((n, Vec::new()));
                        nets.len() - 1
                    }
                };
                if let Some((_, members)) = nets.get_mut(at) {
                    for m in c.from.iter().chain(&c.to) {
                        let member = (kind(m, c.kind), m.as_slice());
                        if !members.contains(&member) {
                            members.push(member);
                        }
                    }
                }
            }
        }
    }
    let shown = |(k, name): Member<'_>| -> Vec<u8> {
        let what: &[u8] = match k {
            Some(TargetKind::Harness) => b"harness ",
            _ => b"agent ",
        };
        [what, name].concat()
    };
    let named = |e: &'_ Exposure| -> Vec<(Option<TargetKind>, Vec<u8>)> {
        e.agents
            .iter()
            .map(|a| (Some(TargetKind::Agent), a.clone()))
            .chain(e.harnesses.iter().map(|h| (Some(TargetKind::Harness), h.clone())))
            .collect()
    };
    // Each member named is on a network its EXPOSE is for.
    for d in directives {
        let Directive::Expose(e) = d else { continue };
        for (k, m) in named(e) {
            let on = e.networks.iter().any(|n| {
                nets.iter()
                    .any(|(x, ms)| *x == n.as_slice() && ms.contains(&(k, m.as_slice())))
            });
            if !on {
                return Err([
                    b"EXPOSE: ".as_slice(),
                    &shown((k, &m)),
                    b" is on no network after FOR; attach it with CONNECT ... ON one of them",
                ]
                .concat());
            }
        }
    }
    let mut out: Vec<Receiver> = Vec::new();
    for (net, members) in &nets {
        // What the network itself lets in to its members (`NETWORK --ingress`,
        // `--expose`): none of an internal one.
        let own: Vec<(u8, u16, u16)> = directives
            .iter()
            .filter_map(|d| match d {
                Directive::Network(n) if n.name == *net && !n.internal => Some(&n.ports),
                _ => None,
            })
            .flatten()
            .filter(|(_, d)| *d != Direction::Egress)
            .filter_map(|(p, _)| port_range(p))
            .collect();
        // Who each microVM port coming in is claimed for: the member an EXPOSE names,
        // else every member. One mapped to a member's port crosses the network's
        // boundary at the member's.
        type Claim = (
            (u8, u16, u16),
            (u8, u16, u16),
            Option<(Option<TargetKind>, Vec<u8>)>,
        );
        let mut claims: Vec<Claim> = Vec::new();
        for d in directives {
            let Directive::Expose(e) = d else { continue };
            if e.direction == Direction::Egress || !e.networks.iter().any(|n| n == net) {
                continue;
            }
            let receivers = named(e);
            if receivers.len() > 1 {
                return Err([
                    b"EXPOSE for network ".as_slice(),
                    net,
                    b" names ",
                    &receivers
                        .iter()
                        .map(|(k, m)| shown((*k, m)))
                        .collect::<Vec<_>>()
                        .join(&b", "[..]),
                    b": a port coming in goes to one member; write an EXPOSE for each, each its own port",
                ]
                .concat());
            }
            for p in &e.ports {
                let (o, i) = (outside(p), inside(p));
                let (Some((proto, lo, hi)), Some((iproto, ilo, ihi))) = (port_range(&o), port_range(&i))
                else {
                    continue;
                };
                if o != i {
                    if own.iter().any(|&(q, a, b)| q == iproto && a <= ilo && ihi <= b) {
                        claims.push(((proto, lo, hi), (iproto, ilo, ihi), receivers.first().cloned()));
                    }
                    continue;
                }
                for &(q, a, b) in &own {
                    let (lo, hi) = (lo.max(a), hi.min(b));
                    if proto == q && lo <= hi {
                        claims.push(((proto, lo, hi), (proto, lo, hi), receivers.first().cloned()));
                    }
                }
            }
        }
        let port = |(proto, lo, hi): (u8, u16, u16)| -> Vec<u8> {
            let range = if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}-{hi}")
            };
            if proto == 17 {
                format!("{range}/udp")
            } else {
                range
            }
            .into_bytes()
        };
        for (i, (range, to, who)) in claims.iter().enumerate() {
            match who {
                None if members.len() > 1 => {
                    return Err([
                        b"network ".as_slice(),
                        net,
                        b" lets port ",
                        &port(*range),
                        b" in past the microVM to its members ",
                        &members.iter().map(|m| shown(*m)).collect::<Vec<_>>().join(&b", "[..]),
                        b": name the one it is for, EXPOSE --agents=<agent> (or --harnesses=<harness>) <port> AS ingress FOR ",
                        net,
                        b" (architecture.md D122)",
                    ]
                    .concat());
                }
                None => {
                    if let Some(&(k, m)) = members.first() {
                        out.push(Receiver {
                            at: *range,
                            to: *to,
                            kind: k.unwrap_or(TargetKind::Agent),
                            name: m.to_vec(),
                        });
                    }
                }
                Some((k, m)) => {
                    let overlaps = |(p, a, b): (u8, u16, u16)| p == range.0 && a <= range.2 && range.1 <= b;
                    if let Some((_, _, Some((k2, m2)))) = claims.iter().skip(i + 1).find(|(r, _, other)| {
                        overlaps(*r) && other.as_ref().is_some_and(|o| o != &(*k, m.clone()))
                    }) {
                        return Err([
                            b"port ".as_slice(),
                            &port(*range),
                            b" on network ",
                            net,
                            b" comes in to two members, ",
                            &shown((*k, m)),
                            b" and ",
                            &shown((*k2, m2)),
                            b": a port coming in goes to one member",
                        ]
                        .concat());
                    }
                    out.push(Receiver {
                        at: *range,
                        to: *to,
                        kind: k.unwrap_or(TargetKind::Agent),
                        name: m.clone(),
                    });
                }
            }
        }
    }
    out.dedup();
    Ok(out)
}

/// The ports an Agentfile lets some domain open flows to past its microVM: of each network
/// a `CONNECT` joins, those it lets cross both boundaries outward ([`boundary`]), each
/// once, as Docker writes them.
pub fn egress(directives: &[Directive]) -> Vec<Vec<u8>> {
    let mut nets: Vec<&[u8]> = Vec::new();
    for d in directives {
        if let Directive::Connect(c) = d {
            for n in &c.on {
                if !nets.contains(&n.as_slice()) {
                    nets.push(n);
                }
            }
        }
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for net in nets {
        for (proto, lo, hi) in boundary(directives, net, true) {
            let range = if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}-{hi}")
            };
            let port = if proto == 17 {
                format!("{range}/udp")
            } else {
                range
            }
            .into_bytes();
            if !out.contains(&port) {
                out.push(port);
            }
        }
    }
    out
}

/// What a domain reaches, through every edge an Agentfile declares (§9.5, §9.8, D58): a
/// network both join (by `CONNECT ... ON`), a named volume granted to both, `ATTACH`.
/// A domain reaches the world when it joins a network that is not internal and lets a port
/// cross both its boundary and the microVM's ([`boundary`]), either way: an ingress-only
/// port answers what reaches it, so its replies carry data out too; or an external one,
/// whose reach the build cannot see; or when a remote MCP
/// server is granted to it, or to every agent. An internal-only domain, one that joins an
/// internal network and does not reach the world, with a path of edges to one that does,
/// reaches the world through it: a build error naming the path, as relays and
/// declassifiers are not designed yet (§7 Q20). A local MCP server is no
/// edge: each caller has its own instance, in its own confinement (§9.6).
pub fn reach(directives: &[Directive]) -> Result<(), Vec<u8>> {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    // A domain: (is a harness, name).
    type Node = (bool, Vec<u8>);
    let mut domains: BTreeSet<Node> = BTreeSet::new();
    for d in directives {
        match d {
            Directive::Agent(a) => {
                domains.insert((false, a.name.clone()));
            }
            Directive::Harness(h) => {
                domains.insert((true, h.name.clone()));
            }
            _ => {}
        }
    }
    let kind_of = |name: &[u8], said: Option<TargetKind>| -> Node {
        let harness = match said {
            Some(TargetKind::Harness) => true,
            Some(TargetKind::Agent) => false,
            None => !domains.contains(&(false, name.to_vec())) && domains.contains(&(true, name.to_vec())),
        };
        (harness, name.to_vec())
    };
    let shown = |d: &Node| -> String {
        format!(
            "{} {}",
            if d.0 { "harness" } else { "agent" },
            String::from_utf8_lossy(&d.1)
        )
    };
    // Each network: whether it is internal, and whether some port crosses both
    // boundaries, either way ([`boundary`]): an ingress-only port answers what reaches it,
    // so its replies carry data out too.
    let mut networks: BTreeMap<Vec<u8>, (bool, bool)> = BTreeMap::new();
    for d in directives {
        if let Directive::Network(n) = d {
            let open = !boundary(directives, &n.name, true).is_empty()
                || !boundary(directives, &n.name, false).is_empty();
            let e = networks.entry(n.name.clone()).or_default();
            e.0 = n.internal;
            // An external network is the host's: what it reaches, the build cannot see. A
            // resolver past the microVM carries what its questions name out.
            e.1 |= open || n.external || n.dns;
        }
    }
    // The edges, each labelled as the path says it.
    let mut edges: BTreeMap<Node, Vec<(Node, String)>> = BTreeMap::new();
    let mut joined: BTreeMap<Vec<u8>, BTreeSet<Node>> = BTreeMap::new();
    let mut volumes: BTreeMap<Vec<u8>, BTreeSet<Node>> = BTreeMap::new();
    let mut world: BTreeMap<Node, String> = BTreeMap::new();
    let mut link = |a: &Node, b: &Node, why: String| {
        edges.entry(a.clone()).or_default().push((b.clone(), why.clone()));
        edges.entry(b.clone()).or_default().push((a.clone(), why));
    };
    for d in directives {
        match d {
            Directive::Connect(c) => {
                for net in &c.on {
                    for n in c.from.iter().chain(&c.to) {
                        joined.entry(net.clone()).or_default().insert(kind_of(n, c.kind));
                    }
                }
                // An edge only where a flow is granted: membership grants none. Either way,
                // as answers carry data back.
                if !c.ports.is_empty() {
                    let why = format!(
                        "network {}",
                        String::from_utf8_lossy(c.on.first().map_or(&[][..], |n| n.as_slice()))
                    );
                    for x in &c.from {
                        for y in &c.to {
                            if x != y {
                                link(&kind_of(x, c.kind), &kind_of(y, c.kind), why.clone());
                            }
                        }
                    }
                }
            }
            Directive::Volume(v) => {
                if let Some(src) = &v.source {
                    for n in &v.scope.names {
                        volumes
                            .entry(src.clone())
                            .or_default()
                            .insert(kind_of(n, v.scope.kind));
                    }
                }
            }
            Directive::Attach(a) => {
                for h in &a.harnesses {
                    for ag in &a.agents {
                        link(&(true, h.clone()), &(false, ag.clone()), "ATTACH".into());
                    }
                }
            }
            Directive::Mcp(m) if m.source.starts_with(b"http://") || m.source.starts_with(b"https://") => {
                let why = format!("remote MCP server {}", String::from_utf8_lossy(&m.name));
                if m.scope.names.is_empty() {
                    for dom in domains.iter().filter(|d| !d.0) {
                        world.entry(dom.clone()).or_insert_with(|| why.clone());
                    }
                } else {
                    for n in &m.scope.names {
                        world
                            .entry(kind_of(n, m.scope.kind))
                            .or_insert_with(|| why.clone());
                    }
                }
            }
            _ => {}
        }
    }
    for (net, members) in &joined {
        let (internal, open) = networks.get(net).copied().unwrap_or_default();
        let why = format!("network {}", String::from_utf8_lossy(net));
        if !internal && open {
            for m in members {
                world.entry(m.clone()).or_insert_with(|| why.clone());
            }
        }
    }
    for (vol, members) in &volumes {
        let list: Vec<&Node> = members.iter().collect();
        for (i, a) in list.iter().enumerate() {
            for b in list.iter().skip(i + 1) {
                link(a, b, format!("volume {}", String::from_utf8_lossy(vol)));
            }
        }
    }
    // The internal-only domains, and from each the shortest path to one reaching the world.
    let internal_only: BTreeSet<&Node> = joined
        .iter()
        .filter(|(net, _)| networks.get(*net).is_some_and(|n| n.0))
        .flat_map(|(_, members)| members)
        .filter(|d| !world.contains_key(*d))
        .collect();
    let mut found = Vec::new();
    for start in internal_only {
        let mut prev: BTreeMap<Node, (Node, String)> = BTreeMap::new();
        let mut queue = VecDeque::from([start.clone()]);
        let mut seen = BTreeSet::from([start.clone()]);
        while let Some(at) = queue.pop_front() {
            if let Some(why) = world.get(&at) {
                // Read from the domain that does not reach the world, outward.
                let mut path = vec![shown(&at)];
                let mut cur = at.clone();
                while let Some((p, edge)) = prev.get(&cur) {
                    path.push(edge.clone());
                    path.push(shown(p));
                    cur = p.clone();
                }
                path.reverse();
                path.push(why.clone());
                found.push(path.join(" -> "));
                break;
            }
            for (next, why) in edges.get(&at).map(Vec::as_slice).unwrap_or_default() {
                if seen.insert(next.clone()) {
                    prev.insert(next.clone(), (at.clone(), why.clone()));
                    queue.push_back(next.clone());
                }
            }
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    let mut text = b"an internal-only domain reaches the world through another (AGENTFILE_ARCH.md \xc2\xa79.5), which needs a relay the Agentfile cannot name yet:".to_vec();
    for f in found {
        text.extend_from_slice(b"\n  ");
        text.extend_from_slice(f.as_bytes());
    }
    Err(text)
}

/// The normalized Agentfile (§8, D35): what a target stage's lineage declared, in the
/// order declared, defaults resolved, as JSON shards' runtime reads. Its first field is
/// its schema's version, which a reader refuses past what it knows.
pub fn spec(directives: &[Directive]) -> Vec<u8> {
    use crate::json::{write_string, write_strings};
    let mut out = String::from("{\"schemaVersion\":1");
    let mut lists: [(&str, Vec<String>); 9] = [
        ("agents", Vec::new()),
        ("harnesses", Vec::new()),
        ("skills", Vec::new()),
        ("mcp", Vec::new()),
        ("networks", Vec::new()),
        ("connections", Vec::new()),
        ("attachments", Vec::new()),
        ("exposures", Vec::new()),
        ("volumes", Vec::new()),
    ];
    let field = |o: &mut String, name: &str| {
        o.push_str(",\"");
        o.push_str(name);
        o.push_str("\":");
    };
    let scope = |o: &mut String, s: &Scope| {
        field(o, "for");
        o.push_str("{\"kind\":");
        match s.kind {
            None => o.push_str("null"),
            Some(TargetKind::Agent) => o.push_str("\"agent\""),
            Some(TargetKind::Harness) => o.push_str("\"harness\""),
        }
        o.push_str(",\"names\":");
        write_strings(o, &s.names);
        o.push('}');
    };
    let direction = |d: Direction| match d {
        Direction::Both => "\"both\"",
        Direction::Ingress => "\"ingress\"",
        Direction::Egress => "\"egress\"",
    };
    for d in directives {
        let mut o = String::from("{");
        let at = match d {
            Directive::Agent(a) | Directive::Harness(a) => {
                let (at, home) = match d {
                    Directive::Agent(_) => (0, "/agents/"),
                    _ => (1, "/harness/"),
                };
                o.push_str("\"name\":");
                write_string(&mut o, &a.name);
                field(&mut o, "source");
                write_string(&mut o, &a.source);
                field(&mut o, "to");
                let to =
                    a.to.clone()
                        .unwrap_or_else(|| [home.as_bytes(), &a.name].concat());
                write_string(&mut o, &to);
                field(&mut o, "processes");
                match a.processes {
                    Processes::Unbounded => o.push_str("null"),
                    Processes::None => o.push_str("\"none\""),
                    Processes::AtMost(n) => o.push_str(&n.to_string()),
                }
                at
            }
            Directive::Skill(s) => {
                match &s.source {
                    SkillSource::Path(p) => {
                        o.push_str("\"source\":");
                        write_string(&mut o, p);
                    }
                    SkillSource::Text(t) => {
                        o.push_str("\"text\":");
                        write_string(&mut o, &t.data);
                    }
                }
                field(&mut o, "dest");
                match &s.dest {
                    Some(dest) => write_string(&mut o, dest),
                    None => o.push_str("null"),
                }
                field(&mut o, "from");
                write_string(&mut o, &s.from);
                scope(&mut o, &s.scope);
                2
            }
            Directive::Mcp(m) => {
                o.push_str("\"name\":");
                write_string(&mut o, &m.name);
                field(&mut o, "source");
                write_string(&mut o, &m.source);
                scope(&mut o, &m.scope);
                3
            }
            Directive::Network(n) => {
                o.push_str("\"name\":");
                write_string(&mut o, &n.name);
                for (name, value) in [("driver", &n.driver), ("ipamDriver", &n.ipam_driver)] {
                    field(&mut o, name);
                    write_string(&mut o, value);
                }
                for (name, value) in [
                    ("attachable", n.attachable),
                    ("internal", n.internal),
                    ("external", n.external),
                    ("dns", n.dns),
                ] {
                    field(&mut o, name);
                    o.push_str(if value { "true" } else { "false" });
                }
                for (name, value) in [("ipv4", n.ipv4), ("ipv6", n.ipv6)] {
                    field(&mut o, name);
                    o.push_str(match value {
                        None => "null",
                        Some(true) => "true",
                        Some(false) => "false",
                    });
                }
                for (name, values) in [
                    ("driverOpts", &n.driver_opts),
                    ("labels", &n.labels),
                    ("ipamOpts", &n.ipam_opts),
                    ("subnets", &n.subnets),
                    ("ipRanges", &n.ip_ranges),
                    ("gateways", &n.gateways),
                    ("auxAddresses", &n.aux_addresses),
                ] {
                    field(&mut o, name);
                    write_strings(&mut o, values);
                }
                field(&mut o, "protocols");
                let carried: Vec<Vec<u8>> = PROTOCOLS
                    .iter()
                    .filter(|p| carries(n, p))
                    .map(|p| p.to_vec())
                    .collect();
                write_strings(&mut o, &carried);
                field(&mut o, "ports");
                o.push('[');
                for (i, (port, d)) in n.ports.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    o.push_str("{\"port\":");
                    write_string(&mut o, port);
                    o.push_str(",\"direction\":");
                    o.push_str(direction(*d));
                    o.push('}');
                }
                o.push(']');
                scope(&mut o, &n.scope);
                4
            }
            Directive::Connect(c) => {
                o.push_str("\"kind\":");
                o.push_str(match c.kind {
                    None => "null",
                    Some(TargetKind::Agent) => "\"agent\"",
                    Some(TargetKind::Harness) => "\"harness\"",
                });
                field(&mut o, "from");
                write_strings(&mut o, &c.from);
                field(&mut o, "bothWays");
                o.push_str(if c.both_ways { "true" } else { "false" });
                field(&mut o, "to");
                write_strings(&mut o, &c.to);
                field(&mut o, "on");
                write_strings(&mut o, &c.on);
                field(&mut o, "ports");
                write_strings(&mut o, &c.ports);
                5
            }
            Directive::Attach(a) => {
                o.push_str("\"agents\":");
                write_strings(&mut o, &a.agents);
                field(&mut o, "harnesses");
                write_strings(&mut o, &a.harnesses);
                6
            }
            Directive::Expose(e) => {
                o.push_str("\"ports\":");
                write_strings(&mut o, &e.ports);
                field(&mut o, "direction");
                o.push_str(direction(e.direction));
                field(&mut o, "networks");
                write_strings(&mut o, &e.networks);
                // The members it is for (D122), where it names any: an image built before
                // them reads the same.
                if !e.agents.is_empty() {
                    field(&mut o, "agents");
                    write_strings(&mut o, &e.agents);
                }
                if !e.harnesses.is_empty() {
                    field(&mut o, "harnesses");
                    write_strings(&mut o, &e.harnesses);
                }
                7
            }
            Directive::Volume(v) => {
                o.push_str("\"source\":");
                match &v.source {
                    Some(s) => write_string(&mut o, s),
                    None => o.push_str("null"),
                }
                field(&mut o, "paths");
                write_strings(&mut o, &v.paths);
                for (name, value) in [("chown", &v.chown), ("chmod", &v.chmod)] {
                    field(&mut o, name);
                    write_string(&mut o, value);
                }
                scope(&mut o, &v.scope);
                8
            }
        };
        o.push('}');
        if let Some((_, list)) = lists.get_mut(at) {
            list.push(o);
        }
    }
    for (name, list) in &lists {
        field(&mut out, name);
        out.push('[');
        out.push_str(&list.join(","));
        out.push(']');
    }
    out.push('}');
    out.into_bytes()
}

/// `sha256:<hex>` of `bytes`, as a label names a digest.
pub fn digest(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest as _;
    let sum = sha2::Sha256::digest(bytes);
    let mut out = b"sha256:".to_vec();
    for b in sum {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    out
}

/// What an Agentfile grants past its microVM (D59): the build's labels of it, and what the
/// daemon holds a run to, each derived from the directives alone; the daemon's from the
/// normalized Agentfile it read in the image's root and checked against the image's digest
/// ([`from_spec`], D109), never from labels, which a crafted image sets as it likes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// [`egress`]: the ports its microVM's network process lets out.
    pub egress: Vec<Vec<u8>>,
    /// [`remote_mcp`]: each remote MCP server's `host:port`.
    pub mcp: Vec<Vec<u8>>,
    /// [`dns`]: any name past the microVM.
    pub dns: bool,
    /// [`egress_declared`]: the ports a run may not publish.
    pub egress_declared: Vec<Vec<u8>>,
}

/// An Agentfile's [`Grants`].
pub fn grants(directives: &[Directive]) -> Grants {
    Grants {
        egress: egress(directives),
        mcp: remote_mcp(directives),
        dns: dns(directives),
        egress_declared: egress_declared(directives),
    }
}

/// The directives a normalized Agentfile ([`spec`]) records, read back: each grant as it
/// is written there, and what only the build uses (a skill's copy options), which it does
/// not record, at its default. Of any text `spec` wrote, `spec` of what this reads is that
/// text again.
pub fn from_spec(text: &[u8]) -> Result<Vec<Directive>, String> {
    use serde_json::Value;
    let doc: Value = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    if doc.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return Err("a schema version this shards does not know".into());
    }
    fn get<'v>(v: &'v Value, key: &str) -> Result<&'v Value, String> {
        v.get(key).ok_or_else(|| format!("no {key:?}"))
    }
    fn bytes(v: &Value, key: &str) -> Result<Vec<u8>, String> {
        get(v, key)?
            .as_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| format!("{key:?} is no string"))
    }
    fn maybe(v: &Value, key: &str) -> Result<Option<Vec<u8>>, String> {
        match get(v, key)? {
            Value::Null => Ok(None),
            _ => bytes(v, key).map(Some),
        }
    }
    fn strings(v: &Value, key: &str) -> Result<Vec<Vec<u8>>, String> {
        get(v, key)?
            .as_array()
            .ok_or_else(|| format!("{key:?} is no list"))?
            .iter()
            .map(|s| s.as_str().map(|s| s.as_bytes().to_vec()))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{key:?} holds more than strings"))
    }
    fn boolean(v: &Value, key: &str) -> Result<bool, String> {
        get(v, key)?
            .as_bool()
            .ok_or_else(|| format!("{key:?} is no boolean"))
    }
    fn maybe_bool(v: &Value, key: &str) -> Result<Option<bool>, String> {
        match get(v, key)? {
            Value::Null => Ok(None),
            b => b
                .as_bool()
                .map(Some)
                .ok_or_else(|| format!("{key:?} is no boolean")),
        }
    }
    fn kind(v: &Value) -> Result<Option<TargetKind>, String> {
        match v {
            Value::Null => Ok(None),
            k if k == "agent" => Ok(Some(TargetKind::Agent)),
            k if k == "harness" => Ok(Some(TargetKind::Harness)),
            _ => Err(format!("{v} is no kind of domain")),
        }
    }
    fn scope(v: &Value) -> Result<Scope, String> {
        let s = get(v, "for")?;
        Ok(Scope {
            kind: kind(get(s, "kind")?)?,
            names: strings(s, "names")?,
        })
    }
    fn direction(v: &Value) -> Result<Direction, String> {
        match get(v, "direction")?.as_str() {
            Some("both") => Ok(Direction::Both),
            Some("ingress") => Ok(Direction::Ingress),
            Some("egress") => Ok(Direction::Egress),
            _ => Err("a direction neither both, ingress nor egress".into()),
        }
    }
    let mut out = Vec::new();
    for (list, at) in [
        ("agents", 0),
        ("harnesses", 1),
        ("skills", 2),
        ("mcp", 3),
        ("networks", 4),
        ("connections", 5),
        ("attachments", 6),
        ("exposures", 7),
        ("volumes", 8),
    ] {
        let items = get(&doc, list)?
            .as_array()
            .ok_or_else(|| format!("{list:?} is no list"))?;
        for v in items {
            let read = || -> Result<Directive, String> {
                Ok(match at {
                    0 | 1 => {
                        let domain = Domain {
                            name: bytes(v, "name")?,
                            source: bytes(v, "source")?,
                            to: Some(bytes(v, "to")?),
                            processes: match get(v, "processes")? {
                                Value::Null => Processes::Unbounded,
                                n if n == "none" => Processes::None,
                                n => Processes::AtMost(
                                    n.as_u64()
                                        .and_then(|n| u32::try_from(n).ok())
                                        .ok_or("processes neither none nor a number")?,
                                ),
                            },
                        };
                        if at == 0 {
                            Directive::Agent(domain)
                        } else {
                            Directive::Harness(domain)
                        }
                    }
                    2 => Directive::Skill(Skill {
                        source: match v.get("text") {
                            Some(_) => SkillSource::Text(SourceContent {
                                path: Vec::new(),
                                data: bytes(v, "text")?,
                                expand: false,
                            }),
                            None => SkillSource::Path(bytes(v, "source")?),
                        },
                        dest: maybe(v, "dest")?,
                        from: bytes(v, "from")?,
                        chown: Vec::new(),
                        chmod: Vec::new(),
                        link: false,
                        exclude: Vec::new(),
                        keep_git_dir: None,
                        checksum: Vec::new(),
                        scope: scope(v)?,
                    }),
                    3 => Directive::Mcp(Mcp {
                        name: bytes(v, "name")?,
                        source: bytes(v, "source")?,
                        scope: scope(v)?,
                    }),
                    4 => {
                        let mut protocols = 0u8;
                        for p in strings(v, "protocols")? {
                            let i = PROTOCOLS
                                .iter()
                                .position(|q| *q == p)
                                .ok_or("a protocol neither tcp, udp nor unix")?;
                            protocols |= 1 << i;
                        }
                        let ports = get(v, "ports")?
                            .as_array()
                            .ok_or("\"ports\" is no list")?
                            .iter()
                            .map(|p| Ok((bytes(p, "port")?, direction(p)?)))
                            .collect::<Result<_, String>>()?;
                        Directive::Network(Network {
                            name: bytes(v, "name")?,
                            driver: bytes(v, "driver")?,
                            driver_opts: strings(v, "driverOpts")?,
                            attachable: boolean(v, "attachable")?,
                            internal: boolean(v, "internal")?,
                            external: boolean(v, "external")?,
                            ipv4: maybe_bool(v, "ipv4")?,
                            ipv6: maybe_bool(v, "ipv6")?,
                            labels: strings(v, "labels")?,
                            ipam_driver: bytes(v, "ipamDriver")?,
                            ipam_opts: strings(v, "ipamOpts")?,
                            subnets: strings(v, "subnets")?,
                            ip_ranges: strings(v, "ipRanges")?,
                            gateways: strings(v, "gateways")?,
                            aux_addresses: strings(v, "auxAddresses")?,
                            dns: boolean(v, "dns")?,
                            ports,
                            protocols,
                            scope: scope(v)?,
                        })
                    }
                    5 => Directive::Connect(Connect {
                        kind: kind(get(v, "kind")?)?,
                        from: strings(v, "from")?,
                        both_ways: boolean(v, "bothWays")?,
                        to: strings(v, "to")?,
                        on: strings(v, "on")?,
                        ports: strings(v, "ports")?,
                    }),
                    6 => Directive::Attach(Attach {
                        agents: strings(v, "agents")?,
                        harnesses: strings(v, "harnesses")?,
                    }),
                    7 => Directive::Expose(Exposure {
                        ports: strings(v, "ports")?,
                        direction: direction(v)?,
                        networks: strings(v, "networks")?,
                        agents: if v.get("agents").is_some() {
                            strings(v, "agents")?
                        } else {
                            Vec::new()
                        },
                        harnesses: if v.get("harnesses").is_some() {
                            strings(v, "harnesses")?
                        } else {
                            Vec::new()
                        },
                    }),
                    _ => Directive::Volume(Volume {
                        source: maybe(v, "source")?,
                        paths: strings(v, "paths")?,
                        chown: bytes(v, "chown")?,
                        chmod: bytes(v, "chmod")?,
                        scope: scope(v)?,
                    }),
                })
            };
            out.push(read().map_err(|e| format!("{list}: {e}"))?);
        }
    }
    Ok(out)
}
