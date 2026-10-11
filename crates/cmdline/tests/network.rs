//! shards_cmdline::network makes of `--network` values what the Docker CLI makes of them:
//! every answer in go-network.json (scripts/docker-cli/network_test.go, run in docker/cli
//! v29.8.1 built by Go 1.26.1), byte for byte.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use serde_json::Value;
use shards_cmdline::network::{self, Attachment};

fn golden() -> Value {
    serde_json::from_str(include_str!("go-network.json")).unwrap()
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| text(s).to_string()).collect())
        .unwrap_or_default()
}

#[test]
fn addresses_parse_and_fail_as_netip_parses_them() {
    let golden = golden();
    for case in golden["addrs"].as_array().unwrap() {
        let input = text(&case["in"]);
        let ours = match network::parse_addr(input) {
            Ok(a) => (a.to_string(), String::new()),
            Err(e) => (String::new(), e),
        };
        assert_eq!(
            ours,
            (text(&case["out"]).to_string(), text(&case["err"]).to_string()),
            "{input:?}"
        );
    }
}

#[test]
fn macs_parse_as_net_parses_them() {
    let golden = golden();
    for case in golden["macs"].as_array().unwrap() {
        let input = text(&case["in"]);
        assert_eq!(
            network::parse_mac(input),
            case["ok"].as_bool().unwrap(),
            "{input:?}"
        );
        assert_eq!(
            network::mac_octets(input)
                .map(|o| network::mac_string(&o))
                .unwrap_or_default(),
            text(&case["out"]),
            "{input:?}"
        );
    }
}

#[test]
fn values_read_as_network_opt_reads_them() {
    let golden = golden();
    for case in golden["attachments"].as_array().unwrap() {
        let input = text(&case["in"]);
        match network::attachment(input) {
            Err(e) => assert_eq!(e, text(&case["err"]), "{input:?}"),
            Ok(a) => {
                assert_eq!(text(&case["err"]), "", "{input:?} read as {a:?}");
                let addr =
                    |a: &Option<network::Addr>| a.as_ref().map(ToString::to_string).unwrap_or_default();
                let opts: BTreeMap<String, String> = a.driver_opts.iter().cloned().collect();
                let want_opts: BTreeMap<String, String> = case["driver_opts"]
                    .as_object()
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), text(v).to_string())).collect())
                    .unwrap_or_default();
                assert_eq!(
                    (
                        a.target.as_str(),
                        a.aliases.clone(),
                        addr(&a.ipv4),
                        addr(&a.ipv6),
                        a.link_local.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        a.mac.as_str(),
                        opts,
                        a.gw_priority,
                    ),
                    (
                        text(&case["target"]),
                        list(&case["aliases"]),
                        text(&case["ipv4"]).to_string(),
                        text(&case["ipv6"]).to_string(),
                        list(&case["link_local"]),
                        text(&case["mac"]),
                        want_opts,
                        case["gw_priority"].as_i64().unwrap(),
                    ),
                    "{input:?}"
                );
            }
        }
    }
}

/// run's top-level network flags land on its endpoints as applyContainerOptions puts them:
/// `--link-local-ip` counted as given for the conflict and what netip does not read
/// dropped, a MAC as Go writes a HardwareAddr.
#[test]
fn top_level_flags_land_on_endpoints_as_run_puts_them() {
    let golden = golden();
    for case in golden["applied"].as_array().unwrap() {
        let args = list(&case["args"]);
        let mut top = network::TopLevel::default();
        let mut given = Vec::new();
        for pair in args.chunks(2) {
            let [flag, value] = pair else { panic!("{args:?}") };
            match flag.as_str() {
                "--network" => given.push(network::attachment(value).unwrap()),
                "--network-alias" => top.aliases.push(value.clone()),
                "--ip" => top.ipv4 = Some(network::parse_addr(value).unwrap()),
                "--ip6" => top.ipv6 = Some(network::parse_addr(value).unwrap()),
                "--mac-address" => top.mac.clone_from(value),
                "--link-local-ip" => top.link_local.push(value.clone()),
                other => panic!("{other}"),
            }
        }
        match network::endpoints(&given, &top) {
            Err(e) => assert_eq!(e, text(&case["err"]), "{args:?}"),
            Ok(endpoints) => {
                assert_eq!(text(&case["err"]), "", "{args:?}");
                let ours: BTreeMap<String, Value> = endpoints
                    .into_iter()
                    .map(|a| {
                        let addr = |a: &Option<network::Addr>| {
                            a.as_ref().map(ToString::to_string).unwrap_or_default()
                        };
                        let mac = network::mac_octets(a.mac.trim())
                            .map(|o| network::mac_string(&o))
                            .unwrap_or_default();
                        let e = serde_json::json!({
                            "aliases": a.aliases,
                            "ipv4": addr(&a.ipv4),
                            "ipv6": addr(&a.ipv6),
                            "link_local": a.link_local.iter().map(ToString::to_string).collect::<Vec<_>>(),
                            "mac": mac,
                        });
                        (a.target, e)
                    })
                    .collect();
                let want: BTreeMap<String, Value> = case["endpoints"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                assert_eq!(ours, want, "{args:?}");
            }
        }
    }
}

#[test]
fn runs_ask_for_the_networks_run_asks_for() {
    let golden = golden();
    for case in golden["runs"].as_array().unwrap() {
        let networks = list(&case["networks"]);
        // pflag's objection to a value comes first, as the flag is read.
        let read: Result<Vec<Attachment>, String> = networks
            .iter()
            .map(|n| {
                network::attachment(n).map_err(|e| {
                    format!(
                        "invalid argument {} for \"--network\" flag: {e}",
                        shards_cmdline::go::quote(n)
                    )
                })
            })
            .collect();
        let ours = read.and_then(|given| {
            let mut endpoints: Vec<String> = network::endpoints(&given, &network::TopLevel::default())?
                .into_iter()
                .map(|a| a.target)
                .collect();
            endpoints.sort();
            Ok((network::mode(&given).to_string(), endpoints))
        });
        match ours {
            Err(e) => assert_eq!(e, text(&case["err"]), "{networks:?}"),
            Ok((mode, endpoints)) => {
                assert_eq!(text(&case["err"]), "", "{networks:?}");
                assert_eq!(
                    (mode.as_str(), endpoints),
                    (text(&case["mode"]), list(&case["endpoints"])),
                    "{networks:?}"
                );
            }
        }
    }
}

#[test]
fn published_ports_read_as_run_reads_them() {
    let golden = golden();
    let cases = golden["ports"].as_array().unwrap();
    assert!(cases.len() > 40);
    for case in cases {
        let publish = list(&case["publish"]);
        match shards_cmdline::ports::publish(&publish) {
            Err(e) => assert_eq!(e, text(&case["err"]), "{publish:?}"),
            Ok((exposed, bindings)) => {
                assert_eq!(text(&case["err"]), "", "{publish:?}");
                let shown = |a: &str| match shards_cmdline::network::parse_addr(a) {
                    Ok(addr) => addr.to_string(),
                    Err(_) => String::new(),
                };
                let mut got: Vec<String> = bindings
                    .iter()
                    .map(|b| format!("{} {} {}", b.port, shown(&b.host_ip), b.host_port))
                    .collect();
                got.sort();
                let exposed: Vec<String> = exposed.iter().map(ToString::to_string).collect();
                assert_eq!(
                    (exposed, got),
                    (list(&case["exposed"]), list(&case["bindings"])),
                    "{publish:?}"
                );
            }
        }
    }
}

#[test]
fn port_arguments_read_as_docker_port_reads_them() {
    let golden = golden();
    for case in golden["port_args"].as_array().unwrap() {
        let got = shards_cmdline::ports::parse_port(text(&case["arg"]));
        match got {
            Ok(port) => assert_eq!(port.to_string(), text(&case["port"]), "{case}"),
            Err(e) => assert_eq!(e, text(&case["err"]), "{case}"),
        }
    }
}

#[test]
fn port_lines_sort_as_docker_port_sorts_them() {
    let golden = golden();
    let sorted = list(&golden["natural"]);
    let mut lines = sorted.clone();
    lines.reverse();
    lines.sort_by(|a, b| shards_cmdline::ports::natural_compare(a, b));
    assert_eq!(lines, sorted);
}
