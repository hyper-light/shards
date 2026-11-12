//! An Agentfile's directives as the parser reads them (docs/architecture/AGENTFILE_ARCH.md
//! §4 and §12): each form the spec gives, each refused with its reason, and a Dockerfile
//! that reads none of them: to BuildKit they are unknown instructions.
#![allow(clippy::unwrap_used, clippy::panic)]

use shards_agentfile::agentfile::{
    Attach, Connect, Direction, Directive, Domain, Exposure, Mcp, Processes, Scope, SkillSource, TargetKind,
    Volume,
};
use shards_agentfile::instructions::{self, Kind};
use shards_agentfile::lint::Linter;
use shards_agentfile::parser::{self, Dialect};

/// The instructions of `body`, an Agentfile's stage's lines after `FROM scratch`.
fn kinds(body: &str) -> Result<Vec<Kind>, String> {
    let text = format!("FROM scratch\n{body}\n");
    let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile)
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    let ins = instructions::parse(&parsed, &Linter::default())
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    Ok(ins
        .stages
        .into_iter()
        .flat_map(|s| s.commands)
        .map(|c| c.kind)
        .collect())
}

fn one(line: &str) -> Directive {
    match kinds(line).unwrap().as_slice() {
        [Kind::Agentfile(d)] => d.clone(),
        other => panic!("{line}: {other:?}"),
    }
}

fn refused(line: &str) -> String {
    kinds(line).unwrap_err()
}

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[test]
fn agents_and_harnesses_take_a_name_a_source_and_where_they_go() {
    assert_eq!(
        one("AGENT main FROM some.registry.com/claude-opus-5-5"),
        Directive::Agent(Domain {
            name: b("main"),
            source: b("some.registry.com/claude-opus-5-5"),
            to: None,
            processes: Processes::Unbounded,
        })
    );
    assert_eq!(
        one("agent --processes=none Main from ./agent to /opt/main"),
        Directive::Agent(Domain {
            name: b("main"),
            source: b("./agent"),
            to: Some(b("/opt/main")),
            processes: Processes::None,
        })
    );
    assert_eq!(
        one("HARNESS --processes=8 ci FROM https://github.com/org/harness.git#v1:dist"),
        Directive::Harness(Domain {
            name: b("ci"),
            source: b("https://github.com/org/harness.git#v1:dist"),
            to: None,
            processes: Processes::AtMost(8),
        })
    );
    // The name first, never after AS (§12.4), the line rewritten.
    assert_eq!(
        refused("AGENT AS main FROM reg/agent"),
        "dockerfile parse error on line 2: AGENT takes its name first, without AS: AGENT main FROM reg/agent"
    );
    assert!(refused("AGENT 1main FROM reg/agent").contains("invalid name for an agent: \"1main\""));
    assert!(refused("AGENT main reg/agent").contains("AGENT requires a name and FROM <source>"));
    assert!(refused("AGENT main FROM reg/agent TO").contains("AGENT requires a name and FROM <source>"));
    assert!(refused("AGENT --processes=0 main FROM a").contains("--processes is a count above 0 or none"));
    assert!(refused("AGENT --cpus=2 main FROM a").contains("unknown flag: --cpus"));
}

#[test]
fn skills_are_taken_as_add_takes_a_source_for_every_agent_or_those_named() {
    let Directive::Skill(s) = one("SKILL ./review.md") else {
        panic!()
    };
    assert_eq!(
        (s.source, s.dest, s.scope),
        (SkillSource::Path(b("./review.md")), None, Scope::default())
    );
    let Directive::Skill(s) = one("SKILL --chmod=0444 --from=tools /skills/lint /opt/skills FOR main other")
    else {
        panic!()
    };
    assert_eq!(s.source, SkillSource::Path(b("/skills/lint")));
    assert_eq!(s.dest, Some(b("/opt/skills")));
    assert_eq!(
        (s.from.as_slice(), s.chmod.as_slice()),
        (&b"tools"[..], &b"0444"[..])
    );
    assert_eq!(s.scope.names, vec![b("main"), b("other")]);
    let Directive::Skill(s) = one("SKILL --target-kind=harness <<EOF FOR ci\n---\nname: x\n---\nEOF") else {
        panic!()
    };
    let SkillSource::Text(t) = s.source else { panic!() };
    assert_eq!(t.data, b("---\nname: x\n---\n"));
    assert_eq!(
        s.scope,
        Scope {
            kind: Some(TargetKind::Harness),
            names: vec![b("ci")]
        }
    );
    assert!(refused("SKILL a b c").contains("SKILL takes one source"));
    assert!(refused("SKILL ./a FOR").contains("SKILL ... FOR names no one"));
    assert!(refused("SKILL --target-kind=both ./a").contains("--target-kind is agent or harness"));
}

#[test]
fn mcp_servers_are_named_and_offered_to_every_agent_or_those_named() {
    assert_eq!(
        one("MCP files FROM ./servers/files FOR main"),
        Directive::Mcp(Mcp {
            name: b("files"),
            source: b("./servers/files"),
            scope: Scope {
                kind: None,
                names: vec![b("main")]
            },
        })
    );
    assert_eq!(
        one("MCP web FROM https://mcp.example.com/sse"),
        Directive::Mcp(Mcp {
            name: b("web"),
            source: b("https://mcp.example.com/sse"),
            scope: Scope::default()
        })
    );
    assert!(refused("MCP web https://mcp.example.com").contains("MCP requires a name and FROM <source>"));
}

#[test]
fn networks_take_composes_options_and_their_ports() {
    let Directive::Network(n) = one(
        "NETWORK --internal --ipv6 --subnet=10.1.0.0/24 --gateway=10.1.0.1 --label=team=x --expose=8080 --egress=443 --ingress=22 back FOR main other",
    ) else {
        panic!()
    };
    assert_eq!(n.name, b("back"));
    assert!(n.internal && !n.attachable && !n.external);
    assert_eq!((n.ipv4, n.ipv6), (None, Some(true)));
    assert_eq!(n.subnets, vec![b("10.1.0.0/24")]);
    assert_eq!(n.gateways, vec![b("10.1.0.1")]);
    assert_eq!(n.labels, vec![b("team=x")]);
    assert_eq!(
        n.ports,
        vec![
            (b("8080"), Direction::Both),
            (b("22"), Direction::Ingress),
            (b("443"), Direction::Egress)
        ]
    );
    assert_eq!(n.scope.names, vec![b("main"), b("other")]);
    assert!(refused("NETWORK a b").contains("NETWORK requires one name"));
    assert!(refused("NETWORK --scope=swarm a").contains("unknown flag: --scope"));
}

#[test]
fn connections_go_with_or_to_agents_on_networks() {
    assert_eq!(
        one("CONNECT --port=8080 --port=53/udp a b WITH c ON back front"),
        Directive::Connect(Connect {
            kind: None,
            from: vec![b("a"), b("b")],
            both_ways: true,
            to: vec![b("c")],
            on: vec![b("back"), b("front")],
            ports: vec![b("8080"), b("53/udp")],
        })
    );
    assert_eq!(
        one("connect --target-kind=harness --port=9000-9010 ci to main on back"),
        Directive::Connect(Connect {
            kind: Some(TargetKind::Harness),
            from: vec![b("ci")],
            both_ways: false,
            to: vec![b("main")],
            on: vec![b("back")],
            ports: vec![b("9000-9010")],
        })
    );
    // An agent to itself names no flow: it attaches it to the networks, and grants nothing.
    assert!(matches!(one("CONNECT a WITH a ON back"), Directive::Connect(c) if c.ports.is_empty()));
    // Default deny: between agents, no port is no grant, refused rather than read as one.
    assert!(refused("CONNECT a TO b ON back").contains("CONNECT between agents grants no port"));
    assert!(refused("CONNECT --port=http a TO b ON back").contains("no port, range of ports or Unix socket"));
    assert!(refused("CONNECT --port=8080 a WITH b TO c ON n").contains("WITH or TO, not both"));
    assert!(refused("CONNECT a WITH b").contains("CONNECT requires names"));
    assert!(refused("CONNECT WITH b ON n").contains("CONNECT requires names"));
}

#[test]
fn harnesses_are_attached_to_agents_by_name() {
    assert_eq!(
        one("ATTACH main other FOR ci"),
        Directive::Attach(Attach {
            agents: vec![b("main"), b("other")],
            harnesses: vec![b("ci")]
        })
    );
    assert!(refused("ATTACH main").contains("ATTACH requires agents and FOR harnesses"));
    assert!(refused("ATTACH FOR ci").contains("ATTACH requires agents and FOR harnesses"));
}

#[test]
fn expose_takes_a_direction_and_networks_and_stays_plain_without_them() {
    assert_eq!(
        one("EXPOSE 3000 3001/udp AS ingress FOR front back"),
        Directive::Expose(Exposure {
            ports: vec![b("3000"), b("3001/udp")],
            direction: Direction::Ingress,
            networks: vec![b("front"), b("back")],
            agents: vec![],
            harnesses: vec![],
        })
    );
    assert_eq!(
        one("EXPOSE 443 as egress"),
        Directive::Expose(Exposure {
            ports: vec![b("443")],
            direction: Direction::Egress,
            networks: vec![],
            agents: vec![],
            harnesses: vec![],
        })
    );
    assert_eq!(
        kinds("EXPOSE 80 443").unwrap(),
        vec![Kind::Expose(vec![b("443"), b("80")])]
    );
    assert!(refused("EXPOSE AS ingress").contains("at least one port before AS or FOR"));
    assert!(refused("EXPOSE 80 AS sideways").contains("ingress or egress, not \"sideways\""));
    assert!(refused("EXPOSE 80 AS ingress egress").contains("AS takes one word"));
    assert!(refused("EXPOSE 80 FOR").contains("FOR names no network, agent nor harness"));
    // The members it is for (D122): each flag its own kind, a list comma-separated or
    // given again; a port mapped to a member's, ingress's alone, one port to one.
    assert_eq!(
        one("EXPOSE --agents=web,api --agents=db --harnesses=ci 443 AS egress FOR front"),
        Directive::Expose(Exposure {
            ports: vec![b("443")],
            direction: Direction::Egress,
            networks: vec![b("front")],
            agents: vec![b("web"), b("api"), b("db")],
            harnesses: vec![b("ci")],
        })
    );
    assert_eq!(
        one("EXPOSE --agents=api 8081:80/udp AS ingress FOR front"),
        Directive::Expose(Exposure {
            ports: vec![b("8081:80/udp")],
            direction: Direction::Ingress,
            networks: vec![b("front")],
            agents: vec![b("api")],
            harnesses: vec![],
        })
    );
    assert_eq!(
        shards_agentfile::agentfile::outside(b"8081:80/udp"),
        b("8081/udp")
    );
    assert_eq!(shards_agentfile::agentfile::inside(b"8081:80/udp"), b("80/udp"));
    assert!(refused("EXPOSE --agents=web 3000").contains("name members of the networks after FOR"));
    assert!(refused("EXPOSE --mcps=search 3000 FOR front").contains("MCP servers do not join networks yet"));
    assert!(refused("EXPOSE 8081:80 AS egress FOR front").contains("ingress's alone"));
    assert!(refused("EXPOSE 8081:80 FOR front").contains("ingress's alone"));
    assert!(refused("EXPOSE 8081-8082:80 AS ingress FOR front").contains("one port to one port"));
    assert!(refused("EXPOSE --agents=web! 3000 FOR front").contains("an agent"));
}

#[test]
fn volumes_take_a_name_options_and_whom_they_are_for() {
    assert_eq!(
        one("VOLUME data /data FOR main"),
        Directive::Volume(Volume {
            source: Some(b("data")),
            paths: vec![b("/data")],
            chown: vec![],
            chmod: vec![],
            scope: Scope {
                kind: None,
                names: vec![b("main")]
            },
        })
    );
    assert_eq!(
        one("VOLUME --chown=main --target-kind=agent /cache /logs FOR main"),
        Directive::Volume(Volume {
            source: None,
            paths: vec![b("/cache"), b("/logs")],
            chown: b("main"),
            chmod: vec![],
            scope: Scope {
                kind: Some(TargetKind::Agent),
                names: vec![b("main")]
            },
        })
    );
    // Docker's: mount points alone.
    assert_eq!(
        kinds("VOLUME /a /b").unwrap(),
        vec![Kind::Volume(vec![b("/a"), b("/b")])]
    );
    assert_eq!(
        kinds("VOLUME [\"/a\"]").unwrap(),
        vec![Kind::Volume(vec![b("/a")])]
    );
    // A volume over a domain's root or its kernel's file systems would hide what every
    // domain is given there.
    for line in [
        "VOLUME data / FOR main",
        "VOLUME --chown=main /proc/x",
        "VOLUME /sys FOR main",
        "VOLUME data /a/../dev/shm FOR main",
    ] {
        assert!(
            refused(line).contains("VOLUME may not cover a domain's root, /proc, /sys or /dev"),
            "{line}"
        );
    }
    // A domain's /tmp is its own scratch: a volume at it would hide the scratch, and one
    // beneath it would find no mount point there.
    for line in [
        "VOLUME --target-kind=harness /tmp FOR h",
        "VOLUME data /tmp/x FOR main",
        "VOLUME --chown=main /var/../tmp/a/b",
        "VOLUME data tmp/cache",
    ] {
        assert!(
            refused(line).contains("VOLUME may not be at or under /tmp, a domain's own scratch"),
            "{line}"
        );
    }
    // A Dockerfile's VOLUME there is Docker's, the run's own root's alone.
    assert_eq!(
        kinds("VOLUME /tmp /tmp/x").unwrap(),
        vec![Kind::Volume(vec![b("/tmp"), b("/tmp/x")])]
    );
    // Paths that only begin with those names are paths like any other.
    assert!(matches!(
        one("VOLUME data /process FOR main"),
        Directive::Volume(_)
    ));
    assert!(matches!(
        one("VOLUME data /devices FOR main"),
        Directive::Volume(_)
    ));
    assert!(matches!(
        one("VOLUME data /tmpfiles FOR main"),
        Directive::Volume(_)
    ));
}

/// A Dockerfile reads none of it, its meaning BuildKit's: its words are unknown
/// instructions, and `VOLUME`'s options unknown flags.
#[test]
fn a_dockerfile_reads_no_agentfile_directive() {
    for (line, said) in [
        ("AGENT main FROM a", "unknown instruction: AGENT"),
        ("ATTACH a FOR h", "unknown instruction: ATTACH"),
        ("NESTING", "unknown instruction: NESTING"),
        ("VOLUME --chown=a /x", "unknown flag: --chown"),
    ] {
        let text = format!("FROM scratch\n{line}\n");
        let parsed = parser::parse(text.as_bytes()).unwrap();
        let e = instructions::parse(&parsed, &Linter::default()).unwrap_err();
        assert!(String::from_utf8_lossy(&e.message).contains(said), "{line}");
    }
    // `EXPOSE 80 AS ingress` in a Dockerfile: ports Docker would check, not a direction.
    let parsed = parser::parse(b"FROM scratch\nEXPOSE 80 AS ingress\n").unwrap();
    let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
    assert!(matches!(ins.stages[0].commands[0].kind, Kind::Expose(_)));
}

/// An `ONBUILD` trigger runs in another file's build: none of an Agentfile's directives
/// is one.
#[test]
fn no_agentfile_directive_is_an_onbuild_trigger() {
    assert!(refused("ONBUILD AGENT main FROM a").contains("AGENT isn't allowed as an ONBUILD trigger"));
    assert!(refused("ONBUILD network n").contains("NETWORK isn't allowed as an ONBUILD trigger"));
    // A base image's trigger would declare the grant for the file built from it (D234).
    assert!(refused("ONBUILD NESTING").contains("NESTING isn't allowed as an ONBUILD trigger"));
}

/// `NESTING` (D234) takes nothing, read in any case: a word is refused in one message, an
/// option as any unknown flag is.
#[test]
fn nesting_takes_no_arguments() {
    use shards_agentfile::agentfile::NestingRefused;
    assert_eq!(one("NESTING"), Directive::Nesting);
    assert_eq!(one("nesting"), Directive::Nesting);
    assert_eq!(
        NestingRefused::Arguments.to_string(),
        "NESTING takes no arguments"
    );
    for line in ["NESTING true", "NESTING a b", "NESTING [\"x\"]"] {
        assert_eq!(
            refused(line),
            format!("dockerfile parse error on line 2: {}", NestingRefused::Arguments),
            "{line}"
        );
    }
    assert!(refused("NESTING --privileged").contains("unknown flag: --privileged"));
}

/// `text`, an Agentfile, parsed and its names checked: the error's text, if any.
fn checked(text: &str) -> Result<(), String> {
    let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile)
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    let ins = instructions::parse(&parsed, &Linter::default())
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    shards_agentfile::agentfile::check(&ins).map_err(|e| String::from_utf8_lossy(&e.message).into_owned())
}

/// One volume at a mount point in a stage's lineage, however its path is written: a
/// domain is given what is mounted there, so a second would go to the first's domains.
#[test]
fn two_volumes_never_share_a_mount_point() {
    let agents = "FROM scratch AS base\nAGENT a FROM r/a:1\nAGENT b FROM r/b:1\n";
    for (lines, line) in [
        ("VOLUME data /data FOR a\nVOLUME other /data FOR b\n", 5),
        ("VOLUME --chown=a /x /x/ FOR a\n", 4),
        (
            "VOLUME data /data FOR a\nVOLUME --chown=b /srv/../data FOR b\n",
            5,
        ),
        ("VOLUME data /data FOR a\nFROM base\nVOLUME other data FOR b\n", 6),
    ] {
        let e = checked(&format!("{agents}{lines}")).unwrap_err();
        assert!(
            e.contains(&format!("line {line}: VOLUME"))
                && e.contains("a VOLUME mounts a volume there already, at line"),
            "{lines}: {e}"
        );
    }
    // Other stages' lineages, Docker's own VOLUME, and different paths are not one.
    checked(&format!(
        "{agents}VOLUME data /data FOR a\nFROM scratch\nAGENT c FROM r/c:1\nVOLUME other /data FOR c\n"
    ))
    .unwrap();
    checked(&format!(
        "{agents}VOLUME /data\nVOLUME data /data FOR a\nVOLUME data /data2 FOR b\n"
    ))
    .unwrap();
}

/// A volume's name is one the engine makes, at build, not at its first run.
#[test]
fn a_volumes_name_is_one_the_engine_takes() {
    assert!(refused("VOLUME d /data FOR main").contains("volume name is too short"));
    assert!(matches!(
        one("VOLUME dd /data FOR main"),
        Directive::Volume(Volume { source: Some(_), .. })
    ));
}

/// A whole Agentfile of every directive, its names all declared and of their kinds.
#[test]
fn an_agentfile_whose_names_hold_is_checked_whole() {
    checked(
        "FROM alpine:3.22 AS base\n\
         AGENT main FROM reg/agents/main:1\n\
         AGENT other FROM reg/agents/other:1\n\
         HARNESS ci FROM ./harness\n\
         NETWORK --internal back FOR main other\n\
         NETWORK world\n\
         MCP files FROM ./servers/files FOR main\n\
         SKILL ./review.md FOR main other\n\
         VOLUME data /data FOR main\n\
         VOLUME --target-kind=harness /work FOR ci\n\
         CONNECT --port=8080 main WITH other ON back\n\
         CONNECT --target-kind=agent --port=8080 main TO other ON world\n\
         ATTACH main other FOR ci\n\
         EXPOSE 443 AS egress FOR world\n\
         FROM base\n\
         SKILL ./more.md FOR main\n",
    )
    .unwrap();
}

#[test]
fn stages_agents_and_harnesses_share_one_namespace() {
    assert_eq!(
        checked("FROM scratch\nAGENT main FROM a\nHARNESS main FROM b\n").unwrap_err(),
        "dockerfile parse error on line 3: \"main\" names an agent already, declared at line 2: stages, agents and harnesses share one namespace"
    );
    assert!(
        checked("FROM scratch AS main\nAGENT main FROM a\n")
            .unwrap_err()
            .contains("\"main\" names a stage already, declared at line 1")
    );
    // Two stages of one name stay BuildKit's lint.
    checked("FROM scratch AS a\nFROM scratch AS a\n").unwrap();
    assert!(
        checked("FROM scratch\nNETWORK n\nNETWORK n\n")
            .unwrap_err()
            .contains("NETWORK \"n\": declared already, at line 2")
    );
    assert!(
        checked("FROM scratch\nMCP m FROM a\nMCP m FROM b\n")
            .unwrap_err()
            .contains("MCP \"m\": declared already, at line 2")
    );
}

#[test]
fn every_name_a_grant_uses_is_declared_of_its_kind() {
    assert!(
        checked("FROM scratch\nSKILL ./x FOR ghost\n")
            .unwrap_err()
            .contains(
                "SKILL ... FOR names \"ghost\", which no AGENT or HARNESS of this stage's lineage declares"
            )
    );
    assert!(
        checked("FROM scratch\nAGENT main FROM a\nVOLUME --target-kind=harness /d FOR main\n")
            .unwrap_err()
            .contains("VOLUME ... FOR names \"main\" as a harness, but it is an agent")
    );
    assert!(
        checked("FROM scratch\nAGENT main FROM a\nAGENT other FROM b\nATTACH main FOR other\n")
            .unwrap_err()
            .contains("ATTACH ... FOR names \"other\" as a harness, but it is an agent")
    );
    assert!(
        checked("FROM scratch\nEXPOSE 80 FOR nowhere\n")
            .unwrap_err()
            .contains("EXPOSE names network \"nowhere\"")
    );
    // FOR names networks and members alike (2026-11-08), never a name that is both.
    assert!(checked("FROM scratch\nAGENT a FROM x\nNETWORK n\nEXPOSE 80 FOR n a\n").is_ok());
    assert!(
        checked("FROM scratch\nAGENT a FROM x\nNETWORK a\nEXPOSE 80 FOR a\n")
            .unwrap_err()
            .contains("EXPOSE ... FOR names \"a\", which is both a network and an agent")
    );
    assert!(
        checked("FROM scratch\nAGENT a FROM x\nEXPOSE --agents=a 80 FOR a\n")
            .unwrap_err()
            .contains(
                "EXPOSE --agents and --harnesses name members of the networks after FOR, which names none"
            )
    );
}

#[test]
fn connections_join_one_kind_on_networks_that_allow_them() {
    let declared =
        "FROM scratch\nAGENT a FROM x\nAGENT b FROM y\nHARNESS h FROM z\nNETWORK n FOR a\nNETWORK open\n";
    assert!(
        checked(&format!("{declared}CONNECT --port=8080 a WITH h ON open\n"))
            .unwrap_err()
            .contains("CONNECT joins one kind: \"a\" is an agent and \"h\" is not")
    );
    assert!(
        checked(&format!("{declared}CONNECT --port=8080 a WITH b ON n\n"))
            .unwrap_err()
            .contains("CONNECT puts \"b\" on network \"n\", whose FOR at line 5 does not allow it")
    );
    assert!(
        checked(&format!("{declared}CONNECT --port=8080 a WITH b ON gone\n"))
            .unwrap_err()
            .contains("CONNECT ... ON names network \"gone\"")
    );
    checked(&format!("{declared}CONNECT --port=8080 a WITH b ON open\n")).unwrap();
}

/// A stage sees what its lineage declared, and nothing of a stage it is not built from.
#[test]
fn a_stage_sees_its_lineages_names_alone() {
    checked("FROM scratch AS base\nAGENT main FROM a\nFROM base\nSKILL ./s FOR main\n").unwrap();
    assert!(
        checked("FROM scratch AS base\nAGENT main FROM a\nFROM scratch\nSKILL ./s FOR main\n")
            .unwrap_err()
            .contains("which no AGENT or HARNESS of this stage's lineage declares")
    );
}

/// The nesting grant makes the command its microVM's only principal (D234, D115's reason):
/// the build refuses `NESTING` and an agent or harness in one lineage, in either order, at
/// the second, naming the first agent, else the first harness, as shards-init does. Its
/// words are the daemon's and init's (`shards_abi::run::beside::nesting`).
#[test]
fn nesting_is_refused_beside_an_agent_or_harness() {
    use shards_agentfile::agentfile::{NestingRefused, nesting_beside};
    assert_eq!(
        nesting_beside(TargetKind::Agent, b"a"),
        "cannot give the command the nesting grant: the grant makes the command its microVM's only principal, and the image's Agentfile declares agent a"
    );
    assert_eq!(
        nesting_beside(TargetKind::Harness, b"h"),
        "cannot give the command the nesting grant: the grant makes the command its microVM's only principal, and the image's Agentfile declares harness h"
    );
    let beside = |line: usize, kind: TargetKind, name: &str| {
        format!(
            "dockerfile parse error on line {line}: {}",
            NestingRefused::Beside { kind, name: b(name) }
        )
    };
    checked("FROM alpine\nNESTING\n").unwrap();
    for (text, refused) in [
        (
            "FROM alpine\nAGENT a FROM ./a\nNESTING\n",
            beside(3, TargetKind::Agent, "a"),
        ),
        (
            "FROM alpine\nNESTING\nAGENT a FROM ./a\n",
            beside(3, TargetKind::Agent, "a"),
        ),
        (
            "FROM alpine\nNESTING\nHARNESS h FROM ./h\n",
            beside(3, TargetKind::Harness, "h"),
        ),
        (
            "FROM alpine\nHARNESS h FROM ./h\nAGENT a FROM ./a\nNESTING\n",
            beside(4, TargetKind::Agent, "a"),
        ),
        (
            "FROM alpine AS base\nNESTING\nFROM base\nAGENT a FROM ./a\n",
            beside(4, TargetKind::Agent, "a"),
        ),
        (
            "FROM alpine AS base\nHARNESS h FROM ./h\nFROM base\nNESTING\n",
            beside(4, TargetKind::Harness, "h"),
        ),
    ] {
        assert_eq!(checked(text).unwrap_err(), refused, "{text}");
    }
    // Another lineage's domains are not beside it.
    checked("FROM alpine AS agents\nAGENT a FROM ./a\nFROM alpine\nNESTING\n").unwrap();
}

/// An image declares the grant once (D234): a second `NESTING` in its lineage is refused,
/// in one message; stages of other lineages each declare their own.
#[test]
fn nesting_is_declared_once() {
    use shards_agentfile::agentfile::NestingRefused;
    assert_eq!(NestingRefused::Twice.to_string(), "NESTING: declared already");
    let twice = |line: usize| format!("dockerfile parse error on line {line}: {}", NestingRefused::Twice);
    assert_eq!(checked("FROM alpine\nNESTING\nNESTING\n").unwrap_err(), twice(3));
    assert_eq!(
        checked("FROM alpine AS base\nNESTING\nFROM base\nNESTING\n").unwrap_err(),
        twice(4)
    );
    checked("FROM alpine AS one\nNESTING\nFROM alpine AS two\nNESTING\n").unwrap();
}

/// The normalized Agentfile: what was declared, in order, defaults resolved, as JSON whose
/// first field is its schema's version.
#[test]
fn the_normalized_agentfile_holds_what_was_declared() {
    let directives = vec![
        one("AGENT main FROM reg/main:1"),
        one("HARNESS --processes=2 ci FROM ./ci TO /opt/ci"),
        one("NETWORK --egress=443 back FOR main"),
        one("EXPOSE 8080 AS ingress FOR back"),
        one("VOLUME data /data FOR main"),
    ];
    let spec = String::from_utf8(shards_agentfile::agentfile::spec(&directives)).unwrap();
    assert_eq!(
        spec,
        concat!(
            "{\"schemaVersion\":1,",
            "\"agents\":[{\"name\":\"main\",\"source\":\"reg/main:1\",\"to\":\"/agents/main\",\"processes\":null}],",
            "\"harnesses\":[{\"name\":\"ci\",\"source\":\"./ci\",\"to\":\"/opt/ci\",\"processes\":2}],",
            "\"skills\":[],\"mcp\":[],",
            "\"networks\":[{\"name\":\"back\",\"driver\":\"\",\"ipamDriver\":\"\",\"attachable\":false,\"internal\":false,\"external\":false,\"dns\":false,",
            "\"ipv4\":null,\"ipv6\":null,\"driverOpts\":[],\"labels\":[],\"ipamOpts\":[],\"subnets\":[],\"ipRanges\":[],\"gateways\":[],",
            "\"auxAddresses\":[],\"protocols\":[\"tcp\",\"udp\"],\"ports\":[{\"port\":\"443\",\"direction\":\"egress\"}],\"for\":{\"kind\":null,\"names\":[\"main\"]}}],",
            "\"connections\":[],\"attachments\":[],",
            "\"exposures\":[{\"ports\":[\"8080\"],\"direction\":\"ingress\",\"networks\":[\"back\"]}],",
            "\"volumes\":[{\"source\":\"data\",\"paths\":[\"/data\"],\"chown\":\"\",\"chmod\":\"\",\"for\":{\"kind\":null,\"names\":[\"main\"]}}]}"
        )
    );
    let parsed: serde_json::Value = serde_json::from_str(&spec).unwrap();
    assert_eq!(parsed["schemaVersion"], 1);
    assert_eq!(
        shards_agentfile::agentfile::digest(b"").as_slice(),
        &b"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"[..]
    );
}

/// What the last stage's directives reach (§9.5, D58): the error's text, if any.
fn reached(text: &str) -> Result<(), String> {
    let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile)
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    let ins = instructions::parse(&parsed, &Linter::default())
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    let directives: Vec<_> = ins
        .stages
        .last()
        .unwrap()
        .commands
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Agentfile(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    shards_agentfile::agentfile::reach(&directives).map_err(|e| String::from_utf8_lossy(&e).into_owned())
}

/// Reach is transitive (§9.5, §9.8): an internal-only domain, with a path of declared
/// edges to one that reaches the world, fails the build, the path named; a network that
/// opens no port, or is internal, reaches nothing outside; a local MCP server joins no one;
/// a domain on no internal network is not internal-only, so a harness driving an agent
/// that reaches the world is no error.
#[test]
fn reach_through_any_declared_edge_is_reach() {
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    // b on an internal network of its own: internal-only.
    let inner = "NETWORK --internal inner\nCONNECT b WITH b ON inner\n";
    // Through an internal network to one on a network open to the world.
    let e = reached(&format!(
        "{base}NETWORK --internal back\nNETWORK --egress=443 world\nEXPOSE 443 FOR world\n\
         CONNECT --port=8080 b WITH a ON back\nCONNECT a WITH a ON world\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("agent b -> network back -> agent a -> network world"),
        "{e}"
    );
    // A network that opens no port reaches nothing; an internal one neither; nor one whose
    // port only one boundary opens (§12 answer 6: a flow crossing both needs both).
    assert_eq!(
        reached(&format!(
            "{base}NETWORK --internal back\nNETWORK world\nNETWORK --internal --egress=443 shut\n\
             NETWORK --egress=443 half\nEXPOSE 80 FOR half\nEXPOSE 443 FOR world\n\
             CONNECT --port=8080 b WITH a ON back\nCONNECT a WITH a ON world shut half\n"
        )),
        Ok(())
    );
    // EXPOSE ... FOR, with the network's own port, opens it.
    let e = reached(&format!(
        "{base}NETWORK --egress=443 world\nEXPOSE 443 AS egress FOR world\nVOLUME data /data FOR a b\nCONNECT a WITH a ON world\n{inner}"
    ))
    .unwrap_err();
    assert!(
        e.contains("agent b -> volume data -> agent a -> network world"),
        "{e}"
    );
    // A harness attached to both joins them.
    let e = reached(&format!(
        "{base}HARNESS h FROM ./h\nNETWORK --ingress=8080 world\nEXPOSE 8080 AS ingress FOR world\nCONNECT a WITH a ON world\nATTACH a b FOR h\n{inner}"
    ))
    .unwrap_err();
    assert!(
        e.contains("agent b -> ATTACH -> harness h -> ATTACH -> agent a -> network world"),
        "{e}"
    );
    // A remote MCP server is egress; a local one joins no one.
    let e = reached(&format!(
        "{base}MCP web FROM https://mcp.example.com FOR a\nMCP files FROM ./files\nVOLUME shared /s FOR a b\n{inner}"
    ))
    .unwrap_err();
    assert!(
        e.contains("agent b -> volume shared -> agent a -> remote MCP server web"),
        "{e}"
    );
    assert_eq!(reached(&format!("{base}MCP files FROM ./files\n{inner}")), Ok(()));
    // Not internal-only: a harness driving an agent that reaches the world.
    assert_eq!(
        reached(&format!(
            "{base}HARNESS h FROM ./h\nMCP web FROM https://mcp.example.com FOR a\nATTACH a b FOR h\n"
        )),
        Ok(())
    );
    // Every agent reaching the world: nothing reaches it through another.
    assert_eq!(
        reached(&format!(
            "{base}MCP web FROM https://mcp.example.com\nVOLUME ss /s FOR a b\n"
        )),
        Ok(())
    );
}

/// The normalized Agentfile read back (D109), as the daemon reads an image's: `spec` of
/// what it reads is the text it read, and its grants are those of the directives it was
/// written from, which the daemon holds a run to, never the labels.
#[test]
fn a_normalized_agentfile_reads_back_as_its_grants() {
    use shards_agentfile::agentfile::{Grants, from_spec, grants, spec};
    use shards_agentfile::instructions::Kind;
    let directives = |text: &str| -> Vec<Directive> {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
        ins.stages
            .iter()
            .flat_map(|s| &s.commands)
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    };
    let sets = |g: Grants| {
        let set = |v: Vec<Vec<u8>>| v.into_iter().collect::<std::collections::BTreeSet<_>>();
        (
            set(g.egress),
            set(g.mcp),
            g.dns,
            set(g.egress_declared),
            g.nesting,
        )
    };
    for text in [
        "FROM alpine:3.22 AS base\n\
         AGENT main FROM reg/agents/main:1\n\
         AGENT --processes=none other FROM reg/agents/other:1 TO /srv/other\n\
         HARNESS --processes=4 ci FROM ./harness\n\
         NETWORK --internal back FOR main other\n\
         NETWORK --dns --egress=443 --ingress=8080 --expose=53/udp --egress=9000-9010 --protocol=tcp,udp,unix --ingress=unix:sock world\n\
         MCP files FROM ./servers/files FOR main\n\
         MCP web FROM https://mcp.example.com:8443/x FOR main\n\
         SKILL ./review.md FOR main other\n\
         VOLUME data /data FOR main\n\
         VOLUME --chown=main --chmod=700 --target-kind=harness /work /logs FOR ci\n\
         CONNECT --port=8080 main WITH other ON back\n\
         CONNECT --target-kind=agent --port=8080 main TO other ON world\n\
         ATTACH main other FOR ci\n\
         EXPOSE 443 AS egress FOR world\n\
         EXPOSE 9005-9020 53/udp FOR world\n\
         EXPOSE 8080 AS ingress FOR world\n\
         EXPOSE --agents=other 8081:8080 AS ingress FOR world\n\
         EXPOSE --agents=main,other --harnesses=ci 9001 AS egress FOR world\n\
         FROM base\n\
         SKILL ./more.md FOR main\n",
        "FROM scratch\nAGENT a FROM ./a\n",
        "FROM scratch\n",
        "FROM scratch\nNETWORK --egress=443 out\nNESTING\nEXPOSE 443 AS egress FOR out\n",
    ] {
        let d = directives(text);
        let written = spec(&d);
        let read = from_spec(&written).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&spec(&read)),
            String::from_utf8_lossy(&written),
            "{text}"
        );
        assert_eq!(sets(grants(&read)), sets(grants(&d)), "{text}");
    }
    let g = grants(&directives(
        "FROM scratch\nAGENT a FROM ./a\nNETWORK --dns --egress=443 out\nEXPOSE 443 FOR out\n\
         CONNECT a WITH a ON out\nMCP web FROM https://mcp.example.com FOR a\nEXPOSE 25 AS egress FOR out\n",
    ));
    assert_eq!(
        (g.egress, g.mcp, g.dns, g.egress_declared),
        (
            vec![b"443".to_vec()],
            vec![b"mcp.example.com:443".to_vec()],
            true,
            vec![b"25".to_vec()]
        )
    );
    // Not one the build wrote: refused, never guessed at.
    for (bad, said) in [
        (&b"{"[..], "EOF"),
        (b"{\"schemaVersion\":2}", "schema version"),
        (b"{\"schemaVersion\":1}", "no \"agents\""),
        (
            b"{\"schemaVersion\":1,\"agents\":[{\"name\":\"a\",\"source\":\"./a\",\"to\":\"/a\",\"processes\":-1}]}",
            "agents: processes neither none nor a number",
        ),
    ] {
        let e = from_spec(bad).unwrap_err();
        assert!(e.contains(said), "{}: {e}", String::from_utf8_lossy(bad));
    }
}

/// `NESTING` in the normalized Agentfile (D234): `"nesting":true` after its schema's
/// version, only where declared, so every other image's reads as before; read back first,
/// so that of directives in its order, `from_spec` of `spec` is what was written. What the
/// daemon reads of a crafted image is checked as the build checks a file, and [`Grants`]
/// carries the grant only where the build would give it.
#[test]
fn a_normalized_agentfile_records_nesting() {
    use shards_agentfile::agentfile::{NestingRefused, from_spec, grants, nesting, spec};
    let x = vec![Directive::Nesting, one("EXPOSE 443 AS egress FOR out")];
    let written = spec(&x);
    assert!(
        String::from_utf8_lossy(&written).starts_with("{\"schemaVersion\":1,\"nesting\":true,\"agents\":[],"),
        "{}",
        String::from_utf8_lossy(&written)
    );
    assert_eq!(from_spec(&written).unwrap(), x);
    assert_eq!(
        from_spec(&spec(&[Directive::Nesting])).unwrap(),
        vec![Directive::Nesting]
    );
    assert_eq!(nesting(&x), Ok(true));
    assert!(grants(&x).nesting);
    let plain = &x[1..];
    assert!(!String::from_utf8_lossy(&spec(plain)).contains("nesting"));
    assert_eq!(nesting(plain), Ok(false));
    assert!(!grants(plain).nesting);
    // What no build makes: refused in the build's words, and never carried.
    let (agent, harness) = (one("AGENT a FROM ./a"), one("HARNESS h FROM ./h"));
    for (directives, refused) in [
        (
            vec![Directive::Nesting, Directive::Nesting],
            NestingRefused::Twice,
        ),
        (
            vec![Directive::Nesting, harness.clone()],
            NestingRefused::Beside {
                kind: TargetKind::Harness,
                name: b("h"),
            },
        ),
        (
            vec![harness, agent, Directive::Nesting],
            NestingRefused::Beside {
                kind: TargetKind::Agent,
                name: b("a"),
            },
        ),
    ] {
        assert_eq!(nesting(&directives), Err(refused), "{directives:?}");
        assert!(!grants(&directives).nesting, "{directives:?}");
    }
    // A crafted image's normalized Agentfile, the grant beside its domains, reads back as
    // written, and the daemon's check of what it read refuses it as the build does.
    let crafted = spec(&[
        Directive::Nesting,
        one("HARNESS h FROM ./h"),
        one("AGENT a FROM ./a"),
    ]);
    assert_eq!(
        nesting(&from_spec(&crafted).unwrap()),
        Err(NestingRefused::Beside {
            kind: TargetKind::Agent,
            name: b("a"),
        })
    );
    // `"nesting"` written as anything but true: not one the build wrote.
    let lists = "\"agents\":[],\"harnesses\":[],\"skills\":[],\"mcp\":[],\"networks\":[],\
                 \"connections\":[],\"attachments\":[],\"exposures\":[],\"volumes\":[]";
    for value in ["false", "1", "\"true\"", "null"] {
        let text = format!("{{\"schemaVersion\":1,\"nesting\":{value},{lists}}}");
        let e = from_spec(text.as_bytes()).unwrap_err();
        assert_eq!(e, "\"nesting\" is written only as true", "{value}");
    }
}

/// `FROM alpine` then `NESTING` builds (D234): its image holds the normalized Agentfile
/// that records the grant, and its `vnd.osi.agentfile.nesting` label says so for Docker and
/// Kubernetes, which nothing reads to grant (D109). Beside an agent the build refuses it.
#[test]
fn an_agentfile_declaring_nesting_builds() {
    use shards_agentfile::agentfile::{DIGEST_LABEL, NESTING_LABEL, NestingRefused, digest, spec};
    use shards_agentfile::plan::{self, EpochSource, Options, Resolved, Resolver};
    use shards_agentfile::platform::Platform;
    /// Every base image, as one of one layer for linux/amd64.
    struct Bases;
    impl Resolver for Bases {
        fn resolve(&self, name: &[u8], _: Option<&Platform>, _: &[u8]) -> Result<Resolved, Vec<u8>> {
            Ok(Resolved {
                reference: name.to_vec(),
                digest: None,
                config: br#"{"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":["sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d"]}}"#.to_vec(),
            })
        }

        fn epoch(&self, _: &EpochSource) -> Result<Option<plan::Epoch>, Vec<u8>> {
            Ok(None)
        }
    }
    let opts = Options {
        target_platform: Platform::new("linux", "amd64"),
        build_platforms: vec![Platform::new("linux", "amd64")],
        dialect: Dialect::Agentfile,
        ..Default::default()
    };
    let built = plan::plan(b"FROM alpine\nNESTING\n", &opts, &Bases).unwrap();
    let labels = &built.image.config.labels;
    assert_eq!(labels.get(NESTING_LABEL).map(Vec::as_slice), Some(&b"1"[..]));
    assert_eq!(
        labels.get(DIGEST_LABEL),
        Some(&digest(&spec(&[Directive::Nesting])))
    );
    // An image that declares none has no such label.
    let other = plan::plan(b"FROM alpine\nNETWORK n\n", &opts, &Bases).unwrap();
    assert!(other.image.config.labels.contains_key(DIGEST_LABEL));
    assert!(!other.image.config.labels.contains_key(NESTING_LABEL));
    let e = plan::plan(b"FROM alpine\nAGENT a FROM ./a\nNESTING\n", &opts, &Bases).unwrap_err();
    let refused = NestingRefused::Beside {
        kind: TargetKind::Agent,
        name: b("a"),
    };
    assert_eq!(
        String::from_utf8_lossy(&e.message),
        format!("dockerfile parse error on line 3: {refused}")
    );
}

/// An image declaring agents or harnesses gives them its `ENV` (D59), so the build refuses
/// one naming their domain, `SHARDS_DOMAIN`, which shards-init sets: written in the
/// Agentfile or its base image's, as the daemon and init refuse it again. An image of
/// neither keeps it, as BuildKit does.
#[test]
fn an_image_with_members_never_names_their_domain_in_its_env() {
    use shards_agentfile::plan::{self, EpochSource, Options, Resolved, Resolver};
    use shards_agentfile::platform::Platform;
    /// Every base image, as one of one layer for linux/amd64 whose config's `Env` is its own.
    struct Bases(&'static str);
    impl Resolver for Bases {
        fn resolve(&self, name: &[u8], _: Option<&Platform>, _: &[u8]) -> Result<Resolved, Vec<u8>> {
            Ok(Resolved {
                reference: name.to_vec(),
                digest: None,
                config: format!(
                    r#"{{"architecture":"amd64","os":"linux","config":{{"Env":[{}]}},"rootfs":{{"type":"layers","diff_ids":["sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d"]}}}}"#,
                    self.0
                )
                .into_bytes(),
            })
        }

        fn epoch(&self, _: &EpochSource) -> Result<Option<plan::Epoch>, Vec<u8>> {
            Ok(None)
        }
    }
    let opts = Options {
        target_platform: Platform::new("linux", "amd64"),
        build_platforms: vec![Platform::new("linux", "amd64")],
        dialect: Dialect::Agentfile,
        ..Default::default()
    };
    let base = Bases(r#""PATH=/usr/bin""#);
    let refused = |text: &[u8], bases: &Bases| {
        let e = plan::plan(text, &opts, bases).unwrap_err();
        String::from_utf8_lossy(&e.message).into_owned()
    };
    for (text, bases) in [
        (
            &b"FROM alpine\nAGENT a FROM ./a\nENV SHARDS_DOMAIN=agent\n"[..],
            &base,
        ),
        (b"FROM alpine\nHARNESS h FROM ./h\nENV SHARDS_DOMAIN=x\n", &base),
        (
            b"FROM alpine\nAGENT a FROM ./a\n",
            &Bases(r#""SHARDS_DOMAIN=base""#),
        ),
    ] {
        let e = refused(text, bases);
        assert!(
            e.contains("the image's ENV, which its agents and harnesses start with")
                && e.contains("SHARDS_DOMAIN, which init sets"),
            "{e}"
        );
    }
    let built = plan::plan(b"FROM alpine\nAGENT a FROM ./a\nENV LANG=C.UTF-8\n", &opts, &base).unwrap();
    assert_eq!(built.image.config.env, [b("PATH=/usr/bin"), b("LANG=C.UTF-8")]);
    for text in [
        &b"FROM alpine\nENV SHARDS_DOMAIN=x\n"[..],
        b"FROM alpine\nNETWORK n\nENV SHARDS_DOMAIN=x\n",
    ] {
        let kept = plan::plan(text, &opts, &base).unwrap();
        assert!(
            kept.image.config.env.contains(&b("SHARDS_DOMAIN=x")),
            "{:?}",
            kept.image.config.env
        );
    }
}

/// The ports an Agentfile's image may reach past its microVM (D59), its network process's
/// union: of each joined network that is not internal, those both its own grants and the
/// microVM's for it (`EXPOSE ... FOR`) open outward (§12 answer 6); each once.
#[test]
fn egress_is_what_joined_networks_open() {
    use shards_agentfile::instructions::Kind;
    let directives = |text: &str| -> Vec<_> {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
        ins.stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    };
    let ports = |text: &str| -> Vec<String> {
        shards_agentfile::agentfile::egress(&directives(text))
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let base = "FROM alpine\nAGENT a FROM ./a\n";
    assert_eq!(
        ports(&format!(
            "{base}NETWORK --egress=443 --ingress=8080 --expose=53/udp --egress=9000-9010 --egress=25 out\n\
             NETWORK --internal --egress=22 shut\nNETWORK --egress=443 unjoined\n\
             EXPOSE 9005-9020 AS egress FOR out\nEXPOSE 53/udp AS ingress FOR out\nEXPOSE 443 8080 FOR out\n\
             EXPOSE 7000 FOR out\nEXPOSE 22 FOR shut\nEXPOSE 443 FOR unjoined\n\
             CONNECT a WITH a ON out shut\n"
        )),
        // 443 both boundaries open outward; 9005-9010 where their ranges meet. Not 53/udp
        // (the microVM's ingress-only), 8080 (the network's), 25 nor 7000 (one boundary
        // each), 22 (internal), the unjoined network's.
        ["443", "9005-9010"]
    );
    assert!(
        ports(&format!(
            "{base}NETWORK --internal --egress=443 shut\nCONNECT a WITH a ON shut\n"
        ))
        .is_empty()
    );
}

/// A network's ports both boundaries open inward reach the members an `EXPOSE` names, or
/// every member where it names none; of several, the one listening takes each connection
/// (the user's decision, 2026-11-08). `FOR` names networks and members alike: a member it
/// names is given the ports on each network it is on, as `--agents` names it there.
#[test]
fn ingress_reaches_a_networks_one_member() {
    use shards_agentfile::instructions::Kind;
    let receivers = |text: &str| -> Result<Vec<String>, String> {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
        let directives: Vec<_> = ins
            .stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let said = |e: Vec<u8>| String::from_utf8_lossy(&e).into_owned();
        let directives = shards_agentfile::agentfile::resolve_targets(&directives).map_err(said)?;
        let got = shards_agentfile::agentfile::ingress(&directives).map_err(said)?;
        Ok(got
            .iter()
            .map(|r| {
                format!(
                    "{}->{} {} {}",
                    r.at.1,
                    r.to.1,
                    match r.kind {
                        shards_agentfile::agentfile::TargetKind::Agent => "agent",
                        shards_agentfile::agentfile::TargetKind::Harness => "harness",
                    },
                    String::from_utf8_lossy(&r.name)
                )
            })
            .collect())
    };
    let check = |text: &str| receivers(text).map(drop);
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    assert_eq!(
        check(&format!(
            "{base}NETWORK --ingress=8080 front\nEXPOSE 8080 AS ingress FOR front\nCONNECT a WITH a ON front\n"
        )),
        Ok(())
    );
    let several = format!("{base}NETWORK --ingress=8080 front\nCONNECT --port=8080 a WITH b ON front\n");
    // Named none: every member, the one listening taking each connection.
    assert_eq!(
        receivers(&format!("{several}EXPOSE 8080 AS ingress FOR front\n")).unwrap(),
        ["8080->8080 agent a", "8080->8080 agent b"]
    );
    // Named: those alone, one or several.
    assert_eq!(
        receivers(&format!("{several}EXPOSE --agents=a 8080 AS ingress FOR front\n")).unwrap(),
        ["8080->8080 agent a"]
    );
    assert_eq!(
        receivers(&format!(
            "{several}EXPOSE --agents=a 8080 AS ingress FOR front\nEXPOSE --agents=b 8080 FOR front\n"
        ))
        .unwrap(),
        ["8080->8080 agent a", "8080->8080 agent b"]
    );
    assert_eq!(
        receivers(&format!(
            "{several}EXPOSE --agents=a,b 8080 AS ingress FOR front\n"
        ))
        .unwrap(),
        ["8080->8080 agent a", "8080->8080 agent b"]
    );
    // Claims that overlap without being the same ports: which holds the rest, no rule says.
    let e = check(&format!(
        "{base}NETWORK --ingress=8000-8100 front\nCONNECT --port=8080 a WITH b ON front\n\
         EXPOSE --agents=a 8000-8090 AS ingress FOR front\nEXPOSE --agents=b 8080 AS ingress FOR front\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("ports 8000-8090 of agent a and 8080 of agent b overlap"),
        "{e}"
    );
    // Each its own port, the second mapped to its member's 8080 at the network's
    // boundary; egress by both.
    assert_eq!(
        check(&format!(
            "{several}EXPOSE --agents=a 8080 AS ingress FOR front\nEXPOSE --agents=b 9090:8080 AS ingress FOR front\nEXPOSE --agents=a,b 443 AS egress FOR front\n"
        )),
        Ok(())
    );
    // FOR's targets, networks and members mixed: a member on each of its networks, as
    // --agents names it there; a network's every member; the line's --agents its networks'.
    let two = format!(
        "{base}AGENT c FROM ./c\nNETWORK --ingress=8080 front\nNETWORK --ingress=8080 back\n\
         CONNECT --port=8080 a WITH b ON front\nCONNECT c WITH c ON back\n"
    );
    assert_eq!(
        receivers(&format!("{two}EXPOSE 8080 AS ingress FOR c\n")).unwrap(),
        ["8080->8080 agent c"]
    );
    assert_eq!(
        receivers(&format!("{two}EXPOSE 8080 AS ingress FOR front c\n")).unwrap(),
        ["8080->8080 agent a", "8080->8080 agent b", "8080->8080 agent c"]
    );
    assert_eq!(
        receivers(&format!("{two}EXPOSE --agents=b 8080 AS ingress FOR front c\n")).unwrap(),
        ["8080->8080 agent b", "8080->8080 agent c"]
    );
    assert_eq!(
        receivers(&format!("{two}EXPOSE 8080 AS ingress FOR back front\n")).unwrap(),
        ["8080->8080 agent a", "8080->8080 agent b", "8080->8080 agent c"]
    );
    // A member a FOR names on no network: nothing reaches it.
    let e = receivers(&format!("{two}AGENT d FROM ./d\nEXPOSE 8080 AS ingress FOR d\n")).unwrap_err();
    assert!(e.contains("EXPOSE ... FOR agent d: it is on no network"), "{e}");
    // One the network's members do not hold.
    let e = check(&format!(
        "{several}AGENT c FROM ./c\nEXPOSE --agents=c 8080 AS ingress FOR front\n"
    ))
    .unwrap_err();
    assert!(e.contains("agent c is on no network after FOR"), "{e}");
    // One boundary alone lets nothing in: several members are no question then.
    assert_eq!(
        check(&format!(
            "{base}NETWORK --ingress=8080 front\nCONNECT --port=8080 a WITH b ON front\n"
        )),
        Ok(())
    );
    assert_eq!(
        check(&format!(
            "{base}NETWORK --egress=443 out\nEXPOSE 443 FOR out\nCONNECT --port=8080 a WITH b ON out\n"
        )),
        Ok(())
    );
}

/// The ports declared `EXPOSE ... AS egress` alone, which `shards run -p` refuses to publish
/// (§12 answer 6); a port also declared both ways or for ingress is publishable.
#[test]
fn egress_declared_ports_are_named_for_the_run() {
    use shards_agentfile::instructions::Kind;
    let parsed = parser::parse_as(
        b"FROM alpine\nAGENT a FROM ./a\nNETWORK out\nEXPOSE 443 9000-9010/udp AS egress FOR out\n\
          EXPOSE 8443 AS egress FOR out\nEXPOSE 8443 FOR out\nCONNECT a WITH a ON out\n",
        Dialect::Agentfile,
    )
    .unwrap();
    let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
    let directives: Vec<_> = ins
        .stages
        .last()
        .unwrap()
        .commands
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Agentfile(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    let got: Vec<String> = shards_agentfile::agentfile::egress_declared(&directives)
        .into_iter()
        .map(|p| String::from_utf8(p).unwrap())
        .collect();
    assert_eq!(got, ["443", "9000-9010/udp"]);
}

/// An Agentfile's agents have IPv6 where a network that says `--ipv6` is one a `CONNECT`
/// joins (D99): a network declared alone gives no one an address.
#[test]
fn agents_have_ipv6_where_a_joined_network_says_so() {
    use shards_agentfile::instructions::Kind;
    let of = |text: &str| -> bool {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
        let directives: Vec<_> = ins
            .stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        shards_agentfile::agentfile::ipv6(&directives)
    };
    assert!(of(
        "FROM alpine\nAGENT a FROM ./a\nNETWORK --ipv6 six\nCONNECT a WITH a ON six\n"
    ));
    assert!(!of(
        "FROM alpine\nAGENT a FROM ./a\nNETWORK --ipv6 six\nNETWORK four\nCONNECT a WITH a ON four\n"
    ));
    assert!(!of(
        "FROM alpine\nAGENT a FROM ./a\nNETWORK four\nCONNECT a WITH a ON four\n"
    ));
}

/// A remote MCP server's host and port, as its URL says or its scheme's own (§4.4).
#[test]
fn a_remote_mcp_servers_endpoint_is_its_urls() {
    use shards_agentfile::agentfile::mcp_endpoint;
    let e = |u: &str| mcp_endpoint(u.as_bytes()).map(|(h, p)| (String::from_utf8(h).unwrap(), p));
    assert_eq!(
        e("https://MCP.example.com/sse"),
        Some(("mcp.example.com".into(), 443))
    );
    assert_eq!(e("http://mcp.example.com"), Some(("mcp.example.com".into(), 80)));
    assert_eq!(
        e("https://user:pw@10.0.0.7:8443/x?y#z"),
        Some(("10.0.0.7".into(), 8443))
    );
    // An IPv6 address, in its brackets (RFC 3986 §3.2.2), with its port or its scheme's.
    assert_eq!(
        e("http://[2001:DB8::7]:8080/sse"),
        Some(("[2001:db8::7]".into(), 8080))
    );
    assert_eq!(
        e("https://[2001:db8::7]/sse"),
        Some(("[2001:db8::7]".into(), 443))
    );
    for none in [
        "./files",
        "git@github.com:org/x.git",
        "https://",
        "https://h:0/",
        "https://h:99999/",
        "https://[2001:db8::7/",
        "https://[2001:db8::7]x:443/",
    ] {
        assert_eq!(e(none), None, "{none}");
    }
}

/// Networks are default deny (D59): a `CONNECT` grants only ports its networks let in to
/// their members, on networks declared; `--dns` needs a network that is not internal; and
/// membership alone is no way from one agent to another.
#[test]
fn connections_grant_only_what_their_networks_let_in() {
    use shards_agentfile::instructions::Kind;
    let directives = |text: &str| -> Vec<_> {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
        ins.stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    };
    let check = |text: &str| {
        shards_agentfile::agentfile::connections(&directives(text))
            .map_err(|e| String::from_utf8_lossy(&e).into_owned())
    };
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    assert_eq!(
        check(&format!(
            "{base}NETWORK --ingress=8000-8100 back\nCONNECT --port=8080 a TO b ON back\n"
        )),
        Ok(())
    );
    let e = check(&format!(
        "{base}NETWORK --ingress=8000 back\nCONNECT --port=8080 a TO b ON back\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("CONNECT --port=8080 on network back: the network lets no such port in"),
        "{e}"
    );
    // An egress-only port lets nothing in.
    let e = check(&format!(
        "{base}NETWORK --egress=8080 back\nCONNECT --port=8080 a TO b ON back\n"
    ))
    .unwrap_err();
    assert!(e.contains("lets no such port in"), "{e}");
    let e = check(&format!("{base}CONNECT --port=8080 a TO b ON nowhere\n")).unwrap_err();
    assert!(e.contains("ON nowhere: no NETWORK declares it"), "{e}");
    let e = check(&format!(
        "{base}NETWORK --internal --dns back\nCONNECT a WITH a ON back\n"
    ))
    .unwrap_err();
    assert!(e.contains("an internal network has no resolver"), "{e}");
    // Members of one network, never paired: no edge between them for reach to follow.
    assert_eq!(
        shards_agentfile::agentfile::reach(&directives(&format!(
            "{base}NETWORK --internal inner\nNETWORK --egress=443 out\nEXPOSE 443 FOR out\n\
             CONNECT b WITH b ON inner\nCONNECT a WITH a ON inner out\n"
        )))
        .map_err(|e| String::from_utf8_lossy(&e).into_owned()),
        Ok(())
    );
}

/// What a network carries (`NETWORK --protocol`, D59): TCP and UDP where none is said, a
/// port of another refused, and Unix sockets, carried only where named, granted by name.
#[test]
fn networks_carry_only_their_protocols() {
    use shards_agentfile::instructions::Kind;
    let directives = |text: &str| -> Result<Vec<_>, String> {
        let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
        let ins = instructions::parse(&parsed, &Linter::default())
            .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
        Ok(ins
            .stages
            .last()
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::Agentfile(d) => Some(d.clone()),
                _ => None,
            })
            .collect())
    };
    let check = |text: &str| -> Result<(), String> {
        shards_agentfile::agentfile::connections(&directives(text)?)
            .map_err(|e| String::from_utf8_lossy(&e).into_owned())
    };
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    // TCP and UDP by default; Unix only where named.
    assert_eq!(
        check(&format!(
            "{base}NETWORK --ingress=8080 --ingress=53/udp n\nCONNECT --port=8080 --port=53/udp a TO b ON n\n"
        )),
        Ok(())
    );
    let e = check(&format!("{base}NETWORK --ingress=unix:tools n\n")).unwrap_err();
    assert!(
        e.contains("--ingress=unix:tools on network n: the network does not carry unix"),
        "{e}"
    );
    assert_eq!(
        check(&format!(
            "{base}NETWORK --protocol=tcp,unix --ingress=7000 --ingress=unix:tools n\n\
             CONNECT --port=7000 --port=unix:tools a TO b ON n\n"
        )),
        Ok(())
    );
    // A protocol it does not carry, refused wherever it is named.
    let e = check(&format!("{base}NETWORK --protocol=tcp --ingress=53/udp n\n")).unwrap_err();
    assert!(
        e.contains("--ingress=53/udp on network n: the network does not carry udp"),
        "{e}"
    );
    let e = check(&format!(
        "{base}NETWORK --protocol=udp --egress=53/udp n\nEXPOSE 443 FOR n\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("EXPOSE 443 on network n: the network does not carry tcp"),
        "{e}"
    );
    let e = check(&format!(
        "{base}NETWORK --protocol=tcp --ingress=7000 n\nCONNECT --port=unix:tools a TO b ON n\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("CONNECT --port=unix:tools on network n: the network does not carry unix"),
        "{e}"
    );
    // A Unix socket granted by name: one the network does not let in is refused, and one
    // never leaves the microVM.
    let e = check(&format!(
        "{base}NETWORK --protocol=unix --ingress=unix:tools n\nCONNECT --port=unix:other a TO b ON n\n"
    ))
    .unwrap_err();
    assert!(e.contains("lets no such Unix socket in"), "{e}");
    let e = check(&format!("{base}NETWORK --protocol=unix --egress=unix:tools n\n")).unwrap_err();
    assert!(e.contains("a Unix socket never leaves the microVM"), "{e}");
    // An IPv6 subnet, range or gateway on a network that gives its members no IPv6.
    for flag in ["--subnet=fd00:1::/64", "--gateway=fd00:1::1"] {
        let e = check(&format!("{base}NETWORK {flag} n\n")).unwrap_err();
        assert!(
            e.contains(&format!(
                "NETWORK {flag}: an IPv6 address on a network without --ipv6"
            )),
            "{e}"
        );
        assert!(check(&format!("{base}NETWORK --ipv6 {flag} n\n")).is_ok());
    }
    // What is no protocol, and what is no port.
    let e = check(&format!("{base}NETWORK --protocol=sctp n\n")).unwrap_err();
    assert!(
        e.contains("NETWORK --protocol=sctp: no protocol it may carry"),
        "{e}"
    );
    let e = check(&format!("{base}NETWORK --ingress=unix:/etc/x n\n")).unwrap_err();
    assert!(e.contains("no port, range of ports or Unix socket"), "{e}");
    let e = check(&format!(
        "{base}NETWORK --protocol=unix --ingress=unix:t n\nCONNECT --port=unix: a TO b ON n\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("CONNECT --port=unix:: no port, range of ports or Unix socket"),
        "{e}"
    );
}

/// The last stage's directives of `text`, as the build reads them.
fn stage_directives(text: &str) -> Vec<Directive> {
    let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile).unwrap();
    let ins = instructions::parse(&parsed, &Linter::default()).unwrap();
    ins.stages
        .last()
        .unwrap()
        .commands
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Agentfile(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

/// A network's destinations (D236): `--egress=HOST:PORT` of TCP, a host's name, an IPv4
/// address or the host itself, reaching the members a joined network's `EXPOSE` opens
/// their port out for, two boundaries as a port's are, each once; none of an internal or
/// unjoined network's, nor where the microVM's boundary opens no such port. The normalized
/// Agentfile records them as written, read back to the same grants, as the daemon reads
/// them.
#[test]
fn destinations_are_what_joined_networks_name_and_open() {
    use shards_agentfile::agentfile as af;
    let grants = |text: &str| af::grants(&af::resolve_targets(&stage_directives(text)).unwrap());
    let base = "FROM alpine\nHARNESS codex FROM ./codex\nAGENT helper FROM ./helper\n";
    let text = format!(
        "{base}NETWORK --egress=API.OpenAI.com:443 --egress=chatgpt.com:443 --egress=host.docker.internal:7070 \
         --egress=192.0.2.7:8443 --egress=unopened.example:9000 provider FOR codex\n\
         NETWORK --internal --egress=api.anthropic.com:443 shut\nNETWORK --egress=unjoined.example:443 unjoined\n\
         EXPOSE --harnesses=codex 443 7070 8443 AS egress FOR provider\nEXPOSE 443 FOR shut unjoined\n\
         CONNECT codex WITH codex ON provider\nCONNECT helper WITH helper ON shut\n"
    );
    let g = grants(&text);
    let listed: Vec<String> = g
        .destinations
        .iter()
        .map(|d| String::from_utf8(d.clone()).unwrap())
        .collect();
    assert_eq!(
        listed,
        [
            "api.openai.com:443",
            "chatgpt.com:443",
            "host.docker.internal:7070",
            "192.0.2.7:8443"
        ]
    );
    assert!(g.egress.is_empty() && g.mcp.is_empty() && !g.dns, "{g:?}");
    assert!(g.past());
    // Per member: codex alone, helper (on an internal network) nothing.
    let directives = af::resolve_targets(&stage_directives(&text)).unwrap();
    assert_eq!(
        af::destinations_on(
            &directives,
            b"provider",
            Some((TargetKind::Harness, &b"codex"[..]))
        )
        .len(),
        4
    );
    assert!(
        af::destinations_on(
            &directives,
            b"provider",
            Some((TargetKind::Agent, &b"helper"[..]))
        )
        .is_empty()
    );
    assert!(af::destinations_on(&directives, b"shut", None).is_empty());
    af::connections(&directives).unwrap();
    af::held(&directives).unwrap();
    // As the daemon reads the normalized Agentfile back.
    let read = af::from_spec(&af::spec(&directives)).unwrap();
    assert_eq!(af::grants(&read), g);
    assert_eq!(af::spec(&read), af::spec(&directives));
}

/// What no destination of a network's is, refused by name and why: one let in rather than
/// reached, of a network carrying no TCP, an all-digit or patterned host, an IPv6 address,
/// UDP, and the member's own loopback; a port, a range and a Unix socket read as before.
#[test]
fn a_destination_is_a_tcp_host_reached_out() {
    use shards_agentfile::agentfile as af;
    let check = |line: &str| {
        let text = format!("FROM alpine\nAGENT a FROM ./a\n{line}\nCONNECT a WITH a ON n\n");
        af::connections(&stage_directives(&text)).map_err(|e| String::from_utf8_lossy(&e).into_owned())
    };
    for (line, said) in [
        (
            "NETWORK --expose=api.example:443 n",
            "NETWORK --expose=api.example:443 on network n: a destination is reached, never let in",
        ),
        (
            "NETWORK --ingress=api.example:443 n",
            "NETWORK --ingress=api.example:443 on network n: a destination is reached, never let in",
        ),
        (
            "NETWORK --protocol=udp --egress=api.example:443 n",
            "--egress=api.example:443 on network n: the network does not carry tcp",
        ),
        (
            "NETWORK --egress=8081:80 n",
            "NETWORK --egress=8081:80 on network n: 8081:80: 8081 is neither a host's name nor an IPv4 address",
        ),
        (
            "NETWORK --egress=*.example:443 n",
            "*.example is neither a host's name nor an IPv4 address",
        ),
        (
            "NETWORK --egress=[::1]:443 n",
            "[::1] is neither a host's name nor an IPv4 address",
        ),
        ("NETWORK --egress=api.example:443/udp n", "443/udp is no TCP port"),
        (
            "NETWORK --egress=localhost:7070 n",
            "localhost is the asker's own loopback (RFC 6761), never past its microVM; the host is host.docker.internal",
        ),
        (
            "NETWORK --egress=http n",
            "no port, range of ports or Unix socket (8080, 53/udp, 8000-8010/tcp, unix:<name>), nor a destination (HOST:PORT)",
        ),
    ] {
        let e = check(line).unwrap_err();
        assert!(e.contains(said), "{line}: {e}");
    }
    for line in [
        "NETWORK --egress=443 --egress=8000-8010/udp --egress=api.example:443 n",
        "NETWORK --protocol=tcp --egress=host.docker.internal:7070 n",
    ] {
        assert_eq!(check(line), Ok(()), "{line}");
    }
}

/// Members granted one TCP port past the microVM unalike, where any reaches it by name,
/// are refused at build and at run alike (D236), naming both: the network process holds
/// the microVM to the union of its members' names on a port. One member's names beside
/// its own port grant, members granted the same names, and members granted ports alone
/// are held, and pass.
#[test]
fn members_reaching_one_port_by_name_reach_it_alike() {
    use shards_agentfile::agentfile as af;
    let held = |text: &str| {
        let directives = af::resolve_targets(&stage_directives(text)).unwrap();
        af::connections(&directives).unwrap();
        af::held(&directives).map_err(|e| String::from_utf8_lossy(&e).into_owned())
    };
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    let named =
        "NETWORK --egress=api.example:443 named\nEXPOSE 443 AS egress FOR named\nCONNECT a WITH a ON named\n";
    for (directives, said) in [
        (
            format!(
                "{base}{named}NETWORK --egress=443 wide\nEXPOSE 443 AS egress FOR wide\nCONNECT b WITH b ON wide\n"
            ),
            "TCP port 443 past the microVM: agent a reaches api.example and agent b any address there, by a port grant",
        ),
        (
            format!(
                "{base}{named}NETWORK --egress=other.example:443 other\nEXPOSE 443 AS egress FOR other\nCONNECT b WITH b ON other\n"
            ),
            "TCP port 443 past the microVM: agent a reaches api.example and agent b other.example",
        ),
        // A port range covering it is a port grant of it.
        (
            format!(
                "{base}{named}NETWORK --egress=400-500 wide\nEXPOSE 400-500 AS egress FOR wide\nCONNECT b WITH b ON wide\n"
            ),
            "agent b any address there, by a port grant",
        ),
        // A remote MCP server's on the port is a name of it too, as the host itself is.
        (
            format!("{base}{named}MCP web FROM https://mcp.example/sse FOR b\n"),
            "agent a reaches api.example and agent b mcp.example",
        ),
        (
            format!(
                "{base}NETWORK --egress=host.docker.internal:7070 kiln\nEXPOSE 7070 AS egress FOR kiln\nCONNECT a WITH a ON kiln\n\
                 NETWORK --egress=7070 wide\nEXPOSE 7070 AS egress FOR wide\nCONNECT b WITH b ON wide\n"
            ),
            "TCP port 7070 past the microVM: agent a reaches host.docker.internal and agent b any address there",
        ),
    ] {
        let e = held(&directives).unwrap_err();
        assert!(e.contains(said), "{directives}: {e}");
    }
    for directives in [
        // One member, its names beside its own port grant.
        format!(
            "{base}{named}NETWORK --egress=443 wide\nEXPOSE 443 AS egress FOR wide\nCONNECT a WITH a ON wide\n"
        ),
        // Two members, the same names on the one port.
        format!(
            "{base}NETWORK --egress=api.example:443 named\nEXPOSE 443 AS egress FOR named\nCONNECT a WITH a ON named\nCONNECT b WITH b ON named\n"
        ),
        // Names on one port, a port grant of another.
        format!(
            "{base}{named}NETWORK --egress=8443 wide\nEXPOSE 8443 AS egress FOR wide\nCONNECT b WITH b ON wide\n"
        ),
        // Ports alone, unalike: each member held to its own by its init.
        format!(
            "{base}NETWORK --egress=443 one\nNETWORK --egress=443-445 two\nEXPOSE 443-445 AS egress FOR one two\nCONNECT a WITH a ON one\nCONNECT b WITH b ON two\n"
        ),
        // A remote MCP server for every agent: the same name for each.
        format!("{base}MCP web FROM https://mcp.example/sse\n"),
        // The EXPOSE naming one member: the network's other member is granted nothing.
        format!(
            "{base}NETWORK --egress=api.example:443 --egress=443 named\nEXPOSE --agents=a 443 AS egress FOR named\nCONNECT a WITH a ON named\nCONNECT b WITH b ON named\n"
        ),
    ] {
        assert_eq!(held(&directives), Ok(()), "{directives}");
    }
}

/// A destination is a reach past the microVM (§9.5, D236): an internal-only agent with a
/// path to one granted a destination reaches the world through it, refused, the path named.
#[test]
fn a_destination_reaches_the_world() {
    let path = reached(
        "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n\
         NETWORK --internal --ingress=8080 back\nNETWORK --egress=api.example:443 provider\n\
         EXPOSE 443 AS egress FOR provider\n\
         CONNECT --port=8080 b WITH a ON back\nCONNECT a WITH a ON provider\n",
    )
    .unwrap_err();
    assert!(
        path.contains("agent b -> network back -> agent a -> network provider"),
        "{path}"
    );
    // Not where no EXPOSE opens its port: no flow crosses the microVM's boundary.
    assert_eq!(
        reached(
            "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n\
             NETWORK --internal --ingress=8080 back\nNETWORK --egress=api.example:443 provider\n\
             CONNECT --port=8080 b WITH a ON back\nCONNECT a WITH a ON provider\n",
        ),
        Ok(())
    );
}
