//! An Agentfile's directives as the parser reads them (docs/architecture/AGENTFILE_ARCH.md
//! §4 and §12): each form the spec gives, each refused with its reason, and a Dockerfile
//! that reads none of them, as BuildKit reads none.
#![allow(clippy::unwrap_used, clippy::panic)]

use shards_dockerfile::agentfile::{
    Attach, Connect, Direction, Directive, Domain, Exposure, Mcp, Processes, Scope, SkillSource, TargetKind,
    Volume,
};
use shards_dockerfile::instructions::{self, Kind};
use shards_dockerfile::lint::Linter;
use shards_dockerfile::parser::{self, Dialect};

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
fn expose_takes_a_direction_and_networks_and_stays_dockers_without_them() {
    assert_eq!(
        one("EXPOSE 3000 3001/udp AS ingress FOR front back"),
        Directive::Expose(Exposure {
            ports: vec![b("3000"), b("3001/udp")],
            direction: Direction::Ingress,
            networks: vec![b("front"), b("back")],
        })
    );
    assert_eq!(
        one("EXPOSE 443 as egress"),
        Directive::Expose(Exposure {
            ports: vec![b("443")],
            direction: Direction::Egress,
            networks: vec![]
        })
    );
    assert_eq!(
        kinds("EXPOSE 80 443").unwrap(),
        vec![Kind::Expose(vec![b("443"), b("80")])]
    );
    assert!(refused("EXPOSE AS ingress").contains("at least one port before AS or FOR"));
    assert!(refused("EXPOSE 80 AS sideways").contains("ingress or egress, not \"sideways\""));
    assert!(refused("EXPOSE 80 AS ingress egress").contains("AS takes one word"));
    assert!(refused("EXPOSE 80 FOR").contains("FOR names no network"));
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
}

/// A Dockerfile reads none of it, as BuildKit reads none: its words are unknown
/// instructions, and `VOLUME`'s options unknown flags.
#[test]
fn a_dockerfile_reads_no_agentfile_directive() {
    for (line, said) in [
        ("AGENT main FROM a", "unknown instruction: AGENT"),
        ("ATTACH a FOR h", "unknown instruction: ATTACH"),
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
}

/// `text`, an Agentfile, parsed and its names checked: the error's text, if any.
fn checked(text: &str) -> Result<(), String> {
    let parsed = parser::parse_as(text.as_bytes(), Dialect::Agentfile)
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    let ins = instructions::parse(&parsed, &Linter::default())
        .map_err(|e| String::from_utf8_lossy(&e.message).into_owned())?;
    shards_dockerfile::agentfile::check(&ins).map_err(|e| String::from_utf8_lossy(&e.message).into_owned())
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
    let spec = String::from_utf8(shards_dockerfile::agentfile::spec(&directives)).unwrap();
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
        shards_dockerfile::agentfile::digest(b"").as_slice(),
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
    shards_dockerfile::agentfile::reach(&directives).map_err(|e| String::from_utf8_lossy(&e).into_owned())
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
            "{base}MCP web FROM https://mcp.example.com\nVOLUME s /s FOR a b\n"
        )),
        Ok(())
    );
}

/// The ports an Agentfile's image may reach past its microVM (D59), its network process's
/// union: of each joined network that is not internal, those both its own grants and the
/// microVM's for it (`EXPOSE ... FOR`) open outward (§12 answer 6); each once.
#[test]
fn egress_is_what_joined_networks_open() {
    use shards_dockerfile::instructions::Kind;
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
        shards_dockerfile::agentfile::egress(&directives(text))
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

/// A network's ports both boundaries open inward reach its one member; with several, which
/// one a connection is for no directive says, and the build says so rather than guess.
#[test]
fn ingress_reaches_a_networks_one_member() {
    use shards_dockerfile::instructions::Kind;
    let check = |text: &str| -> Result<(), String> {
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
        shards_dockerfile::agentfile::ingress(&directives)
            .map_err(|e| String::from_utf8_lossy(&e).into_owned())
    };
    let base = "FROM alpine\nAGENT a FROM ./a\nAGENT b FROM ./b\n";
    assert_eq!(
        check(&format!(
            "{base}NETWORK --ingress=8080 front\nEXPOSE 8080 AS ingress FOR front\nCONNECT a WITH a ON front\n"
        )),
        Ok(())
    );
    let e = check(&format!(
        "{base}NETWORK --ingress=8080 front\nEXPOSE 8080 AS ingress FOR front\nCONNECT --port=8080 a WITH b ON front\n"
    ))
    .unwrap_err();
    assert!(
        e.contains("network front lets ports in past the microVM to its members a, b"),
        "{e}"
    );
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
    use shards_dockerfile::instructions::Kind;
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
    let got: Vec<String> = shards_dockerfile::agentfile::egress_declared(&directives)
        .into_iter()
        .map(|p| String::from_utf8(p).unwrap())
        .collect();
    assert_eq!(got, ["443", "9000-9010/udp"]);
}

/// A remote MCP server's host and port, as its URL says or its scheme's own (§4.4).
#[test]
fn a_remote_mcp_servers_endpoint_is_its_urls() {
    use shards_dockerfile::agentfile::mcp_endpoint;
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
    for none in [
        "./files",
        "git@github.com:org/x.git",
        "https://",
        "https://h:0/",
        "https://h:99999/",
    ] {
        assert_eq!(e(none), None, "{none}");
    }
}

/// Networks are default deny (D59): a `CONNECT` grants only ports its networks let in to
/// their members, on networks declared; `--dns` needs a network that is not internal; and
/// membership alone is no way from one agent to another.
#[test]
fn connections_grant_only_what_their_networks_let_in() {
    use shards_dockerfile::instructions::Kind;
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
        shards_dockerfile::agentfile::connections(&directives(text))
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
        shards_dockerfile::agentfile::reach(&directives(&format!(
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
    use shards_dockerfile::instructions::Kind;
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
        shards_dockerfile::agentfile::connections(&directives(text)?)
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
