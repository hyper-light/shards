//! Coordinator-only direct-guest capability/socket fixture; no interception claim.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]
mod common;

use serde_json::json;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Inputs {
    kernel: PathBuf,
    config: PathBuf,
    provenance: String,
    records: PathBuf,
    boot: Duration,
    case_ms: u64,
    memory_mib: u64,
    cpus: u64,
}
fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("coordinator must supply {name}; fixture has no production-pin/default fallback")
    })
}
fn absolute_file(name: &str) -> PathBuf {
    let p = PathBuf::from(required(name));
    assert!(
        p.is_absolute() && p.is_file(),
        "{name} must name an absolute existing file: {}",
        p.display()
    );
    p
}
fn positive(name: &str) -> u64 {
    let value = required(name)
        .parse::<u64>()
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(value > 0, "{name} must be positive");
    value
}
impl Inputs {
    fn supplied() -> Self {
        let records = PathBuf::from(required("SHARDS_NETWORK_RECORDS"));
        assert!(records.is_absolute(), "record directory must be absolute");
        std::fs::create_dir_all(&records).unwrap();
        let result = Self {
            kernel: absolute_file("SHARDS_KERNEL"),
            config: absolute_file("SHARDS_NETWORK_EFFECTIVE_CONFIG"),
            provenance: required("SHARDS_NETWORK_CONFIG_PROVENANCE"),
            records,
            boot: Duration::from_millis(positive("SHARDS_NETWORK_BOOT_MS")),
            case_ms: positive("SHARDS_NETWORK_CASE_MS"),
            memory_mib: positive("SHARDS_NETWORK_MEMORY_MIB"),
            cpus: positive("SHARDS_NETWORK_CPUS"),
        };
        assert!(
            !result.provenance.trim().is_empty(),
            "config provenance must be explicit"
        );
        result
    }
    fn identity(&self, name: &str) -> serde_json::Value {
        let digest = |p: &Path| common::sha256_digest(&std::fs::read(p).unwrap());
        let symbols = [
            "CONFIG_NFT_SOCKET",
            "CONFIG_NFT_TPROXY",
            "CONFIG_NF_SOCKET_IPV4",
            "CONFIG_NF_SOCKET_IPV6",
            "CONFIG_NF_TPROXY_IPV4",
            "CONFIG_NF_TPROXY_IPV6",
            "CONFIG_IP_MULTIPLE_TABLES",
            "CONFIG_IPV6_MULTIPLE_TABLES",
            "CONFIG_BPF_SYSCALL",
            "CONFIG_CGROUP_BPF",
        ];
        let config = std::fs::read_to_string(&self.config).expect("supply the uncompressed effective config");
        let values: Vec<_> = symbols
            .iter()
            .map(|symbol| json!({"symbol":symbol,"supplied_value":config_value(&config,symbol)}))
            .collect();
        let record = json!({"case":name,"fixture_revision":option_env!("SHARDS_NETWORK_SOURCE_REVISION").unwrap_or("not-embedded-retain-source-sha256"),"kernel":{"path":self.kernel,"sha256":digest(&self.kernel)},"config":{"path":self.config,"sha256":digest(&self.config),"provenance":self.provenance,"authority":"coordinator-supplied evidence; association with booted kernel not independently verified","symbols":values},"guest":{"path":common::test_guest(),"sha256":digest(common::test_guest())},"cli":{"path":common::shards()},"boot_ms":self.boot.as_millis(),"case_ms":self.case_ms,"memory_mib":self.memory_mib,"vcpus":self.cpus,"interception":"not installed or verified"});
        std::fs::write(
            self.records.join(format!("{name}.identity.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        record
    }
    fn boot(&self, name: &str, probe: &[&str]) -> common::Run {
        let identity = self.identity(name);
        let _ = writeln!(std::io::stdout(), "NETWORK_GUEST_IDENTITY {identity}");
        let home = common::TempDir::new("network-direct");
        let cmdline = command_line(probe);
        std::fs::write(self.records.join(format!("{name}.cmdline")), &cmdline).unwrap();
        let memory = self.memory_mib.to_string();
        let cpus = self.cpus.to_string();
        let run = common::run_shards_env(
            &["run"],
            &[
                "--kernel",
                self.kernel.to_str().unwrap(),
                "--init",
                common::test_guest().to_str().unwrap(),
                "--memory",
                &memory,
                "--cpus",
                &cpus,
                "--cmdline",
                &cmdline,
            ],
            &[("SHARDS_HOME", home.as_os_str())],
            self.boot,
        );
        std::fs::write(self.records.join(format!("{name}.stdout")), &run.stdout).unwrap();
        std::fs::write(self.records.join(format!("{name}.stderr")), &run.stderr).unwrap();
        std::fs::write(
            self.records.join(format!("{name}.status.json")),
            serde_json::to_vec(&json!({"status":run.status})).unwrap(),
        )
        .unwrap();
        run
    }
}
fn config_value<'a>(config: &'a str, symbol: &str) -> &'a str {
    for line in config.lines() {
        if let Some(value) = line.strip_prefix(symbol).and_then(|line| line.strip_prefix('=')) {
            return value;
        }
        if line.strip_prefix("# ").and_then(|line| line.strip_prefix(symbol)) == Some(" is not set") {
            return "n";
        }
    }
    "absent"
}
fn command_line(probe: &[&str]) -> String {
    // Linux v6.18 init/main.c set_init_arg takes argv after --; next_arg in
    // lib/cmdline.c preserves double-quoted empty arguments. This invokes no shell.
    let mut line =
        String::from("console=ttyS0 quiet panic=-1 shards_test=managed-network -- managed-network");
    for word in probe {
        assert!(
            !word.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\0')),
            "unsafe kernel argv word"
        );
        line.push(' ');
        line.push_str(if word.is_empty() { "\"\"" } else { word });
    }
    line
}

#[test]
fn fixture_cmdline_and_config_preserve_empty_and_missing_evidence() {
    let line = command_line(&["pair", "tcp", "127.0.0.1:0", "", "00ff", "1000", "native"]);
    assert!(line.ends_with("pair tcp 127.0.0.1:0 \"\" 00ff 1000 native"));
    assert_eq!(
        config_value("# CONFIG_NFT_TPROXY is not set\n", "CONFIG_NFT_TPROXY"),
        "n"
    );
    assert_eq!(
        config_value("CONFIG_NFT_TPROXY=y\n", "CONFIG_NFT_SOCKET"),
        "absent"
    );
}

fn intercept_records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("managed-intercept "))
        .map(|record| serde_json::from_str(record).expect("typed interception record"))
        .collect()
}

#[test]
fn interception_records_keep_distinct_bytes_and_typed_counts() {
    let records = intercept_records(
        "ignored\nmanaged-intercept {\"type\":\"Joined\",\"backend_accepted\":1,\"relay_accepted\":1,\"joined\":true}\n",
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["joined"], true);
    assert_eq!(records[0]["backend_accepted"].as_u64(), Some(1));
    assert!(std::panic::catch_unwind(|| intercept_records("managed-intercept {broken}")).is_err());
}

#[derive(Clone, Copy, Debug)]
enum InterceptionFamily {
    Ipv4,
    Ipv6,
}

#[test]
#[ignore = "coordinator runs isolated IPv4 TCP interception experiment; no production adoption"]
fn experimental_tcp4_interception_has_distinct_backend_and_relay_observations() {
    experimental_tcp_interception(InterceptionFamily::Ipv4);
}

#[test]
#[ignore = "coordinator runs isolated IPv6 TCP interception experiment; no production adoption"]
fn experimental_tcp6_interception_has_distinct_backend_and_relay_observations() {
    experimental_tcp_interception(InterceptionFamily::Ipv6);
}

#[test]
#[ignore = "coordinator runs isolated UDP4 compatibility experiment; no production adoption"]
fn experimental_udp4_interception_has_datagram_and_original_tuple_observations() {
    experimental_udp_interception(InterceptionFamily::Ipv4);
}

#[test]
#[ignore = "coordinator runs isolated UDP6 compatibility experiment; no production adoption"]
fn experimental_udp6_interception_has_datagram_and_original_tuple_observations() {
    experimental_udp_interception(InterceptionFamily::Ipv6);
}

fn experimental_udp_interception(family: InterceptionFamily) {
    let protocol = match family {
        InterceptionFamily::Ipv4 => "intercept-udp4",
        InterceptionFamily::Ipv6 => "intercept-udp6",
    };
    let inputs = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "real hypervisor required");
    let origin = required("SHARDS_INTERCEPT_ORIGIN");
    assert!(matches!(
        origin.as_str(),
        "local" | "forward" | "local-loopback" | "local-self"
    ));
    let mode = required("SHARDS_INTERCEPT_UDP_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let mark = required("SHARDS_INTERCEPT_MARK");
    let table = required("SHARDS_INTERCEPT_TABLE");
    let priority = required("SHARDS_INTERCEPT_RULE_PRIORITY");
    let millis = inputs.case_ms.to_string();
    let run = inputs.boot(
        &format!("{protocol}-{origin}-{mode}"),
        &[protocol, &origin, &mode, &millis, &mark, &table, &priority],
    );
    let records = intercept_records(&run.stdout);
    assert_udp_records(&records, family, &origin, &mode);
    assert_eq!(run.status, Some(0), "actual UDP guest failed: {run}");
    assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
}

fn assert_udp_records(records: &[serde_json::Value], family: InterceptionFamily, origin: &str, mode: &str) {
    let one = |kind| {
        let matching: Vec<_> = records.iter().filter(|r| r["type"] == kind).collect();
        assert_eq!(matching.len(), 1, "{kind}: {records:?}");
        matching[0]
    };
    let namespaces = one("Namespaces");
    let router = namespaces["router_inode"].as_u64().unwrap();
    let backend = namespaces["backend_inode"].as_u64().unwrap();
    let client = namespaces["client_inode"].as_u64().unwrap();
    assert!(router != backend && router != client && backend != client);
    let target = one("UdpSetup");
    assert_eq!(target["stage"], "ReserveReplyTuple");
    for field in ["baseline", "backend_worker", "relay_worker"] {
        assert_eq!(target[field], "NotStarted");
    }
    assert_eq!(
        target["family"],
        match family {
            InterceptionFamily::Ipv4 => "Ipv4",
            InterceptionFamily::Ipv6 => "Ipv6",
        }
    );
    assert_eq!(
        target["origin"],
        match origin {
            "local" => "Local",
            "forward" => "Forward",
            "local-loopback" => "LocalLoopback",
            "local-self" => "LocalSelf",
            other => panic!("unknown UDP origin {other}"),
        }
    );
    assert_eq!(
        target["mode"],
        if mode == "connected" {
            "Connected"
        } else {
            "Unconnected"
        }
    );
    assert_eq!(
        target["backend_inode"],
        if matches!(origin, "local-loopback" | "local-self") {
            router
        } else {
            backend
        }
    );
    assert_eq!(target["relay_inode"], router);
    assert_eq!(
        target["client_inode"],
        if origin == "forward" { client } else { router }
    );
    let destination: std::net::SocketAddr = target["destination"].as_str().unwrap().parse().unwrap();
    let expected_ip: std::net::IpAddr = match (family, origin) {
        (InterceptionFamily::Ipv4, "local-loopback") => "127.0.0.1",
        (InterceptionFamily::Ipv4, "local-self") => "192.0.2.1",
        (InterceptionFamily::Ipv4, _) => "192.0.2.2",
        (InterceptionFamily::Ipv6, "local-loopback") => "::1",
        (InterceptionFamily::Ipv6, "local-self") => "2001:db8:1::1",
        (InterceptionFamily::Ipv6, _) => "2001:db8:1::2",
    }
    .parse()
    .unwrap();
    assert_eq!(destination.ip(), expected_ip);
    assert_ne!(destination.port(), 0);
    let reservation_errors: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "UdpError" && record["role"] == "ReplySocket")
        .collect();
    assert!(
        reservation_errors.is_empty(),
        "native reply reservation failed; interception was not tested: setup={target}, actual_errors={reservation_errors:?}"
    );
    let ready = one("UdpTarget");
    for field in [
        "family",
        "origin",
        "mode",
        "destination",
        "client_inode",
        "backend_inode",
        "relay_inode",
    ] {
        assert_eq!(ready[field], target[field]);
    }
    let target = ready;
    assert_eq!(target["reply_local"], destination.to_string());
    let relay_port = target["relay_port"].as_u64().unwrap();
    assert!((1..=u16::MAX as u64).contains(&relay_port));
    assert_ne!(relay_port, u64::from(destination.port()));
    let arrivals = |role| {
        records
            .iter()
            .filter(|r| r["type"] == "UdpArrival" && r["role"] == role)
            .collect::<Vec<_>>()
    };
    let baseline = arrivals("BaselineClient");
    let intercepted = arrivals("InterceptedClient");
    let backend_packets = arrivals("Backend");
    let relay_packets = arrivals("Relay");
    for packets in [&baseline, &intercepted, &backend_packets, &relay_packets] {
        assert_eq!(packets.len(), 2, "{records:?}");
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(packet["ordinal"], index + 1);
            assert_eq!(packet["step"], if index == 0 { "Binary" } else { "Empty" });
            assert_eq!(packet["metadata"], "Known");
            assert_eq!(packet["flags"], 0);
            let bytes = packet["copied_hex"].as_str().unwrap();
            assert!(bytes.len().is_multiple_of(2) && bytes.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(packet["reported"], bytes.len() / 2);
            if index == 1 {
                assert!(bytes.is_empty());
            }
        }
        assert_eq!(packets[0]["peer"], packets[1]["peer"]);
        assert_eq!(packets[0]["local"], packets[1]["local"]);
        assert_eq!(packets[0]["original"], packets[1]["original"]);
    }
    assert_eq!(backend_packets[0]["copied_hex"], "5245515545535400ff");
    assert_eq!(relay_packets[0]["copied_hex"], "5245515545535400ff");
    assert_eq!(baseline[0]["copied_hex"], "4241434b454e4400ff");
    assert_eq!(intercepted[0]["copied_hex"], "52454c41590080");
    assert_ne!(baseline[0]["local"], intercepted[0]["local"]);
    for (packets, server) in [(&baseline, &backend_packets), (&intercepted, &relay_packets)] {
        let local: std::net::SocketAddr = packets[0]["local"].as_str().unwrap().parse().unwrap();
        let expected_source: std::net::IpAddr = match (family, origin) {
            (InterceptionFamily::Ipv4, "local-loopback") => "127.0.0.1",
            (InterceptionFamily::Ipv4, "local" | "local-self") => "192.0.2.1",
            (InterceptionFamily::Ipv4, _) => "198.51.100.2",
            (InterceptionFamily::Ipv6, "local-loopback") => "::1",
            (InterceptionFamily::Ipv6, "local" | "local-self") => "2001:db8:1::1",
            (InterceptionFamily::Ipv6, _) => "2001:db8:2::2",
        }
        .parse()
        .unwrap();
        assert_eq!(local.ip(), expected_source);
        assert_ne!(local.port(), 0);
        assert_eq!(packets[0]["peer"], destination.to_string());
        assert_eq!(packets[0]["original"], local.to_string());
        assert_eq!(server[0]["peer"], local.to_string());
        assert_eq!(server[0]["original"], destination.to_string());
    }
    assert_eq!(backend_packets[0]["local"], destination.to_string());
    let relay_local: std::net::SocketAddr = relay_packets[0]["local"].as_str().unwrap().parse().unwrap();
    assert!(relay_local.ip().is_unspecified());
    assert_eq!(u64::from(relay_local.port()), relay_port);
    for role in ["Backend", "Relay"] {
        let completed = records
            .iter()
            .find(|r| r["type"] == "UdpWorkerCompletion" && r["role"] == role)
            .unwrap();
        assert_eq!(completed["joined"], true);
        assert_eq!(completed["arrivals"], 2);
        assert_eq!(completed["matched"], 2);
        assert_eq!(completed["empty"], 1);
        assert_eq!(
            completed["terminal"],
            if role == "Backend" {
                "Cancelled"
            } else {
                "ScriptComplete"
            }
        );
    }
    for role in ["BaselineClient", "InterceptedClient"] {
        let outcome = records
            .iter()
            .find(|r| r["type"] == "UdpClientOutcome" && r["role"] == role)
            .unwrap();
        assert_eq!(outcome["outcome"], "ScriptComplete");
    }
    assert!(!records.iter().any(|r| r["type"] == "UdpError"));
    assert_eq!(one("UdpIntercepted")["joined"], true);
    assert_eq!(one("UdpIntercepted")["after_window"], "Unverified");
}

fn experimental_tcp_interception(family: InterceptionFamily) {
    let protocol = match family {
        InterceptionFamily::Ipv4 => "intercept-tcp4",
        InterceptionFamily::Ipv6 => "intercept-tcp6",
    };
    let inputs = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "real hypervisor required");
    let origin = required("SHARDS_INTERCEPT_ORIGIN");
    assert!(matches!(
        origin.as_str(),
        "local" | "forward" | "local-loopback" | "local-self" | "forward-relay-absent"
    ));
    let mark = required("SHARDS_INTERCEPT_MARK");
    let table = required("SHARDS_INTERCEPT_TABLE");
    let priority = required("SHARDS_INTERCEPT_RULE_PRIORITY");
    let millis = inputs.case_ms.to_string();
    let run = inputs.boot(
        &format!("{protocol}-{origin}"),
        &[protocol, &origin, &millis, &mark, &table, &priority],
    );
    let records = intercept_records(&run.stdout);
    let one = |kind| {
        let matching: Vec<_> = records.iter().filter(|r| r["type"] == kind).collect();
        assert_eq!(matching.len(), 1, "{kind}: {run}");
        matching[0]
    };
    let namespaces = one("Namespaces");
    let router = namespaces["router_inode"].as_u64().unwrap();
    let backend = namespaces["backend_inode"].as_u64().unwrap();
    let client = namespaces["client_inode"].as_u64().unwrap();
    assert!(router != backend && router != client && backend != client);
    let target = one("Target");
    assert_eq!(
        target["relay_mode"],
        if origin == "forward-relay-absent" {
            "Absent"
        } else {
            "Present"
        }
    );
    let expected_ip = match (family, origin.as_str()) {
        (InterceptionFamily::Ipv4, "local-loopback") => "127.0.0.1",
        (InterceptionFamily::Ipv4, "local-self") => "192.0.2.1",
        (InterceptionFamily::Ipv4, _) => "192.0.2.2",
        (InterceptionFamily::Ipv6, "local-loopback") => "::1",
        (InterceptionFamily::Ipv6, "local-self") => "2001:db8:1::1",
        (InterceptionFamily::Ipv6, _) => "2001:db8:1::2",
    }
    .parse::<std::net::IpAddr>()
    .unwrap();
    assert_eq!(target["family"], format!("{family:?}"));
    let actual_destination = target["destination"]
        .as_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(actual_destination.ip(), expected_ip);
    assert_eq!(target["relay_inode"].as_u64(), Some(router));
    assert_eq!(
        target["client_inode"].as_u64(),
        Some(if matches!(origin.as_str(), "forward" | "forward-relay-absent") {
            client
        } else {
            router
        })
    );
    assert_eq!(
        target["backend_inode"].as_u64(),
        Some(if matches!(origin.as_str(), "local-loopback" | "local-self") {
            router
        } else {
            backend
        })
    );
    let baseline = one("Baseline");
    assert_eq!(target["destination"], baseline["destination"]);
    if origin == "forward-relay-absent" {
        let closed = one("RelayListenerClosed");
        assert_eq!(closed["relay_port"], baseline["relay_port"]);
        assert_eq!(closed["relay_worker"], "NotStarted");
        let outcome = one("ClientOutcome");
        let joined = one("Joined");
        assert_eq!(joined["joined"], true);
        assert_eq!(joined["relay_worker"], "NotStarted");
        // Accept and exact request receipt are independent actual effects.
        assert_eq!(
            joined["backend_accepted"], 1,
            "absent relay admitted a fresh backend connection: {run}"
        );
        assert_eq!(
            joined["backend_receipts"], 1,
            "absent relay delivered another exact request: {run}"
        );
        assert_eq!(joined["relay_accepted"], 0);
        assert_eq!(joined["relay_receipts"], 0);
        assert_eq!(
            outcome["stage"], "ConnectCall",
            "setup failure is inconclusive: {run}"
        );
        let verdict = one("AbsenceVerdict")["verdict"].as_str().unwrap();
        let worker_errors: Vec<_> = records.iter().filter(|r| r["type"] == "WorkerError").collect();
        let window = match verdict {
            "ContainedRefusal" => {
                assert_eq!(outcome["outcome"], "Refused");
                assert_eq!(outcome["kind"], "ConnectionRefused");
                assert_eq!(joined["backend_terminal"], "Stopped");
                assert!(worker_errors.is_empty());
                "AttemptUntilJoin"
            }
            "NoNewBackendEffectBeforeDeadline" => {
                assert_eq!(outcome["outcome"], "Deadline");
                assert_eq!(outcome["kind"], "TimedOut");
                match joined["backend_terminal"].as_str().unwrap() {
                    "Stopped" => assert!(worker_errors.is_empty()),
                    "Deadline" => {
                        assert_eq!(worker_errors.len(), 1);
                        assert_eq!(worker_errors[0]["worker"], "Backend");
                        assert_eq!(worker_errors[0]["kind"], "TimedOut");
                    }
                    other => panic!("unknown/failed backend completion {other}: {run}"),
                }
                "OriginalAbsoluteDeadline"
            }
            other => panic!("absence has no checked verdict ({other}): {run}"),
        };
        let backends: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "ServerObservation" && r["role"] == "Backend")
            .collect();
        assert_eq!(backends.len(), 1, "actual backend receipt count differs: {run}");
        let clients: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "ClientObservation")
            .collect();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0]["reply"], "Backend");
        assert_eq!(clients[0]["hex"], "4241434b454e4400ff");
        assert_eq!(backends[0]["peer"], clients[0]["local"]);
        assert_eq!(backends[0]["local"], baseline["destination"]);
        assert!(
            records
                .iter()
                .all(|r| r["type"] != "ServerObservation" || r["role"] != "Relay")
        );
        let checked = one("RelayAbsentChecked");
        assert_eq!(checked["verdict"], verdict);
        assert_eq!(checked["window"], window);
        assert_eq!(checked["after_window"], "Unverified");
        assert_eq!(checked["no_second_backend_accept_or_receipt"], true);
        assert_eq!(run.status, Some(0), "{run}");
        assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
        return;
    }
    assert_eq!(run.status, Some(0), "{run}");
    assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
    let intercepted = one("Intercepted");
    assert_eq!(intercepted["original"], baseline["destination"]);
    assert_eq!(intercepted["original"], intercepted["client_peer"]);
    assert_eq!(intercepted["peer"], intercepted["client_local"]);
    let joined = one("Joined");
    assert_eq!(joined["joined"], true);
    assert_eq!(joined["backend_accepted"], 1);
    assert_eq!(joined["relay_accepted"], 1);
    assert_eq!(joined["backend_receipts"], 1);
    assert_eq!(joined["relay_receipts"], 1);
    let observations: Vec<_> = records
        .iter()
        .filter(|r| r["type"] == "ClientObservation")
        .collect();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0]["reply"], "Backend");
    assert_eq!(observations[0]["hex"], "4241434b454e4400ff");
    assert_eq!(observations[1]["reply"], "Relay");
    assert_eq!(observations[1]["hex"], "52454c41590080");
    let servers: Vec<_> = records
        .iter()
        .filter(|r| r["type"] == "ServerObservation")
        .collect();
    assert_eq!(servers.len(), 2);
    let actual_backend = servers.iter().find(|r| r["role"] == "Backend").unwrap();
    let actual_relay = servers.iter().find(|r| r["role"] == "Relay").unwrap();
    assert_eq!(actual_backend["local"], baseline["destination"]);
    assert_eq!(actual_backend["peer"], observations[0]["local"]);
    assert_eq!(actual_relay["local"], intercepted["original"]);
    assert_eq!(actual_relay["peer"], intercepted["client_local"]);
}

#[test]
#[ignore = "coordinator runs direct experimental-kernel VMs"]
fn direct_guest_capabilities_are_separate_from_payload_results() {
    let inputs = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "real hypervisor required");
    let run = inputs.boot("capabilities", &["capabilities"]);
    assert_eq!(run.status, Some(0), "{run}");
    assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
    assert!(
        run.stdout.contains("managed-network capabilities kernel="),
        "{run}"
    );
    for family in ["ipv4", "ipv6"] {
        assert!(
            run.stdout
                .contains(&format!("capability {family}-transparent-bind=accepted"))
                || run
                    .stdout
                    .contains(&format!("capability {family}-transparent-bind=refused")),
            "missing capability result: {run}"
        );
    }
    assert!(
        run.stdout
            .contains("config=unverified provide-exact-built-config"),
        "guest must not pretend host-supplied config was observed in guest: {run}"
    );
    assert!(
        !run.stdout.contains("managed-network Matched"),
        "capability observation is not a payload result: {run}"
    );
    println!(
        "NETWORK_GUEST_CAPABILITIES observed=true nft_hook=unverified supplied_config_association=unverified payload_tested=false"
    );
}

#[test]
#[ignore = "coordinator runs direct experimental-kernel VMs"]
fn direct_guest_native_pairs_preserve_binary_empty_and_half_close() {
    let inputs = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "real hypervisor required");
    let deadline = inputs.case_ms.to_string();
    for (family, address) in [("v4", "127.0.0.1:0"), ("v6", "[::1]:0")] {
        for transport in ["tcp", "udp", "udp-connected"] {
            for (payload, sent, reply) in [("binary", "00ff0a804100", "ff000d0a8001"), ("empty", "", "")] {
                let name = format!("{family}-{transport}-{payload}");
                let run = inputs.boot(
                    &name,
                    &["pair", transport, address, sent, reply, &deadline, "native"],
                );
                assert_eq!(run.status, Some(0), "{name}: {run}");
                assert!(run.stdout.contains("SHARDS-TEST PASS"), "{name}: {run}");
                assert_eq!(
                    run.stdout
                        .lines()
                        .filter(|line| line.starts_with("managed-network Matched "))
                        .count(),
                    2,
                    "both native endpoints must match: {name}: {run}"
                );
                for expected in [sent, reply] {
                    assert!(
                        run.stdout
                            .lines()
                            .any(|line| line.starts_with("managed-network Received ")
                                && line.ends_with(&format!("hex={expected}"))),
                        "missing exact bytes: {name}: {run}"
                    );
                }
                if transport == "tcp" {
                    for event in ["WriteClosed", "PeerClosed"] {
                        assert_eq!(
                            run.stdout
                                .lines()
                                .filter(|line| line.starts_with(&format!("managed-network {event} ")))
                                .count(),
                            2,
                            "TCP must half-close and receive EOF on each endpoint: {name}: {run}"
                        );
                    }
                }
                println!("NETWORK_GUEST_PAIR case={name} endpoint_matches=2 interception=unverified");
            }
        }
    }
}

fn sock_addr_records(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| {
            line.strip_prefix("managed-sock-addr ")
                .or_else(|| line.strip_prefix("managed-intercept "))
        })
        .map(|line| serde_json::from_str(line).expect("fixture record must be valid JSON"))
        .collect()
}

#[test]
fn socket_address_record_parser_keeps_actual_tuple_and_failed_verdict() {
    let records = sock_addr_records(concat!(
        "console noise\n",
        "managed-sock-addr {\"type\":\"ClientIdentity\",\"requested\":\"127.0.0.1:41\",\"peer\":\"127.0.0.1:42\"}\n",
        "managed-intercept {\"type\":\"UdpArrival\",\"peer\":\"127.0.0.1:42\",\"reported\":0,\"copied_hex\":\"\"}\n",
        "managed-sock-addr {\"type\":\"ClientCompletion\",\"compatibility\":\"Mismatch\",\"success\":false}\n"
    ));
    assert_eq!(records.len(), 3);
    assert_ne!(records[0]["requested"], records[0]["peer"]);
    assert_eq!(records[1]["reported"], 0);
    assert_eq!(records[2]["success"], false);
}

#[test]
#[ignore = "coordinator alone loads hooks and runs actual guest compatibility comparison"]
fn direct_guest_socket_address_udp4_requires_original_native_tuple() {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let name = format!("sock-addr-udp4-{mode}");
    let millis = input.case_ms.to_string();
    let run = input.boot(&name, &["sock-addr-udp4", &mode, &millis]);
    let records = sock_addr_records(&run.stdout);
    // Raw boot/identity/status are durable before any compatibility assertion.
    let one = |kind: &str| {
        let selected: Vec<_> = records.iter().filter(|r| r["type"] == kind).collect();
        assert_eq!(selected.len(), 1, "{kind}: {run}");
        selected[0]
    };
    let target = one("Target");
    assert_ne!(target["backend"], target["responder"]);
    let identities: Vec<_> = records.iter().filter(|r| r["type"] == "ClientIdentity").collect();
    assert_eq!(identities.len(), 2, "both fresh exec clients must report: {run}");
    assert_ne!(identities[0]["pid"], identities[1]["pid"]);
    for identity in &identities {
        assert_eq!(identity["namespace"], target["namespace"]);
        assert_eq!(identity["requested"], target["backend"]);
        let cgroup = identity["cgroup_hex"].as_str().expect("actual cgroup bytes");
        let bytes: Vec<_> = cgroup
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert!(String::from_utf8(bytes).unwrap().contains("/sock-addr-client\n"));
        if mode == "connected" {
            assert_eq!(
                identity["peer"], target["backend"],
                "changed getpeername is incompatible: {run}"
            );
        } else {
            assert!(identity["peer"].is_null());
            assert_eq!(identity["peer_error"], "NotConnected");
        }
    }
    for (role, identity, binary) in [
        ("SockAddrBaselineClient", identities[0], "4241434b454e4400ff"),
        ("SockAddrHookClient", identities[1], "52454c41590080"),
    ] {
        let arrivals: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "UdpArrival" && r["role"] == role)
            .collect();
        assert_eq!(arrivals.len(), 2, "binary and empty observations required: {run}");
        for (index, arrival) in arrivals.into_iter().enumerate() {
            assert_eq!(
                arrival["peer"], target["backend"],
                "IP AND port must remain original: {run}"
            );
            assert_eq!(arrival["original"], identity["local"]);
            assert_eq!(arrival["metadata"], "Known");
            assert_eq!(
                // Linux guest MSG_TRUNC (0x20) | MSG_CTRUNC (0x08), not host ABI.
                arrival["flags"].as_i64().unwrap() & 0x28,
                0
            );
            assert_eq!(arrival["copied_hex"], if index == 0 { binary } else { "" });
            assert_eq!(arrival["reported"], if index == 0 { binary.len() / 2 } else { 0 });
        }
    }
    let cleanup = records
        .iter()
        .find(|r| r["type"] == "Cleanup" && r["state"] == "Observed")
        .expect("joined/detached/removed facts");
    for fact in [
        "backend_joined",
        "relay_joined",
        "link_detached",
        "cgroup_removed",
    ] {
        assert_eq!(cleanup[fact], true, "{fact}: {run}");
    }
    for role in ["Backend", "Relay"] {
        let worker = records
            .iter()
            .find(|r| r["type"] == "UdpWorkerCompletion" && r["role"] == role)
            .expect("worker joined");
        assert_eq!(worker["joined"], true);
        assert_eq!(worker["arrivals"], 2);
        assert_eq!(worker["matched"], 2);
        assert_eq!(worker["empty"], 1);
        assert_eq!(
            worker["terminal"],
            if role == "Backend" {
                "Cancelled"
            } else {
                "ScriptComplete"
            }
        );
        let client = records
            .iter()
            .find(|r| r["type"] == "ClientCompletion" && r["expected"] == role)
            .expect("client terminal");
        assert_eq!(client["script"], "Complete");
        assert_eq!(client["exchanges"], 2);
        assert_eq!(client["compatibility"], "Exact");
        assert_eq!(client["success"], true);
    }
    assert!(
        !records
            .iter()
            .any(|r| r["type"] == "Error" || r["type"] == "UdpError")
    );
    assert_eq!(
        run.status, 0,
        "destination rewrite alone is not compatibility success: {run}"
    );
}
