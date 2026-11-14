//! Coordinator-only direct-guest capability/socket fixture; no interception claim.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]
mod common;

#[path = "managed_network/cancel.rs"]
mod cancel;

#[path = "managed_network/tcp_forward.rs"]
mod tcp_forward;

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
        // A new directory: no earlier run's records are read as this run's.
        std::fs::create_dir(&records)
            .unwrap_or_else(|e| panic!("record directory must be new: {}: {e}", records.display()));
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
        let record = json!({"case":name,"fixture_revision":option_env!("SHARDS_NETWORK_SOURCE_REVISION").unwrap_or("not-embedded-retain-source-sha256"),"kernel":{"path":self.kernel,"sha256":digest(&self.kernel)},"config":{"path":self.config,"sha256":digest(&self.config),"provenance":self.provenance,"authority":"coordinator-supplied evidence; association with booted kernel not independently verified","symbols":values},"guest":{"path":common::test_guest(),"sha256":digest(common::test_guest())},"cli":{"path":common::shards(),"sha256":digest(common::shards())},"boot_ms":self.boot.as_millis(),"case_ms":self.case_ms,"memory_mib":self.memory_mib,"vcpus":self.cpus,"interception":"not installed or verified"});
        std::fs::write(
            self.records.join(format!("{name}.identity.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        record
    }
    #[track_caller]
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
        );
        std::fs::write(self.records.join(format!("{name}.stdout")), &run.stdout).unwrap();
        std::fs::write(self.records.join(format!("{name}.stderr")), &run.stderr).unwrap();
        std::fs::write(
            self.records.join(format!("{name}.status.json")),
            serde_json::to_vec(&json!({"status":run.status})).unwrap(),
        )
        .unwrap();
        // Written last: this case's run ended and each of its records above is whole. A case
        // with an identity record and no marker was interrupted.
        let lines = |verdict: &str| run.stdout.lines().filter(|l| l.trim_end() == verdict).count();
        let failed = run
            .stdout
            .lines()
            .filter(|l| l.contains("SHARDS-TEST FAIL"))
            .count();
        std::fs::write(
            self.records.join(format!("{name}.complete.json")),
            serde_json::to_vec(&json!({"status":run.status,"signal":run.signal,"stdout_bytes":run.stdout.len(),"stderr_bytes":run.stderr.len(),"guest_pass_lines":lines("SHARDS-TEST PASS"),"guest_fail_lines":failed})).unwrap(),
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

/// A direct-boot guest's verdict, as boot.rs reads it: exactly one `SHARDS-TEST PASS` line and
/// no `SHARDS-TEST FAIL`. The test guest powers off after either verdict
/// (crates/testguest/src/linux.rs), and the VMM exits 0 on every clean power-off, so the
/// status alone accepts a failed guest.
fn assert_guest_passed(run: &common::Run, what: &str) {
    assert_eq!(run.status, Some(0), "{what}: {run}");
    let lines = || run.stdout.lines().map(str::trim_end);
    assert!(
        !lines().any(|line| line.contains("SHARDS-TEST FAIL")),
        "{what}: the guest's verdict is FAIL: {run}"
    );
    assert_eq!(
        lines().filter(|line| *line == "SHARDS-TEST PASS").count(),
        1,
        "{what}: exactly one guest PASS line: {run}"
    );
}

#[test]
fn a_guest_verdict_is_its_pass_line_not_the_exit_status() {
    let passed = |stdout: &str| {
        let run = common::Run {
            status: Some(0),
            signal: None,
            stdout: stdout.into(),
            stderr: String::new(),
            elapsed: Duration::ZERO,
        };
        std::panic::catch_unwind(|| assert_guest_passed(&run, "case")).is_ok()
    };
    // A retained real run's ending, its status 0 (managed-coordinator-boundaries/boundary0456/
    // precommit-connected/sock-addr-precommit4-connected.stdout).
    assert!(!passed(
        "SHARDS-TEST FAIL managed-network probe failed; see its errno/state record\r\n"
    ));
    assert!(passed(
        "managed-network capabilities kernel=6.18\r\nSHARDS-TEST PASS\r\n"
    ));
    assert!(!passed("managed-network capabilities kernel=6.18\r\n"));
    assert!(!passed(
        "SHARDS-TEST PASS\r\nSHARDS-TEST FAIL reporting the verdict: broken pipe\r\n"
    ));
    assert!(!passed("SHARDS-TEST PASS\r\nSHARDS-TEST PASS\r\n"));
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
    // UDP refuses local-loopback and local-self, here as in the guest: their backend binds the
    // destination in the router's own namespace, where the reply reservation must bind that
    // same tuple, and fails with EADDRINUSE (errno 98) in all eight retained runs
    // (docs/research/measurements/managed-network-udp-local/coordinator-6e3d). Untested by
    // this fixture, the requirement stays open; it is not passed.
    assert!(
        matches!(origin.as_str(), "local" | "forward"),
        "UDP origin {origin:?}: local or forward; local-loopback and local-self cannot reserve the reply tuple their backend holds"
    );
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
    assert_guest_passed(&run, "actual UDP guest");
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
    assert_eq!(target["backend_inode"], backend);
    assert_eq!(target["relay_inode"], router);
    assert_eq!(
        target["client_inode"],
        if origin == "forward" { client } else { router }
    );
    let destination: std::net::SocketAddr = target["destination"].as_str().unwrap().parse().unwrap();
    let expected_ip: std::net::IpAddr = match family {
        InterceptionFamily::Ipv4 => "192.0.2.2",
        InterceptionFamily::Ipv6 => "2001:db8:1::2",
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
            (InterceptionFamily::Ipv4, "local") => "192.0.2.1",
            (InterceptionFamily::Ipv4, _) => "198.51.100.2",
            (InterceptionFamily::Ipv6, "local") => "2001:db8:1::1",
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
        assert_guest_passed(&run, "relay-absent guest");
        return;
    }
    assert_guest_passed(&run, "TCP interception guest");
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
    assert_guest_passed(&run, "capabilities guest");
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
                assert_guest_passed(&run, &name);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SockAddrPresentation {
    DestinationOnly,
    FixedOriginalPeer,
}
impl SockAddrPresentation {
    fn parse(word: &str) -> Result<Self, &'static str> {
        match word {
            "destination-only" => Ok(Self::DestinationOnly),
            "fixed-original-peer" => Ok(Self::FixedOriginalPeer),
            _ => Err("explicit socket-address presentation profile required"),
        }
    }
    fn argument(self) -> &'static str {
        match self {
            Self::DestinationOnly => "destination-only",
            Self::FixedOriginalPeer => "fixed-original-peer",
        }
    }
    fn hooks(self, mode: &str) -> &'static [&'static str] {
        match (self, mode) {
            (Self::DestinationOnly, "connected") => &["Connect4"],
            (Self::DestinationOnly, "unconnected") => &["Sendmsg4"],
            (Self::FixedOriginalPeer, "connected") => &["Connect4", "Recvmsg4", "Getpeername4"],
            (Self::FixedOriginalPeer, "unconnected") => &["Sendmsg4", "Recvmsg4"],
            _ => panic!("UDP mode must be explicit"),
        }
    }
}

#[test]
fn socket_address_presentation_profile_has_no_default() {
    for word in ["", "fixed", "none", "unknown"] {
        assert!(SockAddrPresentation::parse(word).is_err());
    }
    let only = SockAddrPresentation::parse("destination-only").unwrap();
    let fixed = SockAddrPresentation::parse("fixed-original-peer").unwrap();
    assert_eq!(only.hooks("connected"), ["Connect4"]);
    assert_eq!(only.hooks("unconnected"), ["Sendmsg4"]);
    assert_eq!(fixed.hooks("connected"), ["Connect4", "Recvmsg4", "Getpeername4"]);
    assert_eq!(fixed.hooks("unconnected"), ["Sendmsg4", "Recvmsg4"]);
    assert_eq!(fixed.argument(), "fixed-original-peer");
}

fn assert_sock_addr_hooks_and_cleanup(
    records: &[serde_json::Value],
    target: &serde_json::Value,
    hooks: &[&str],
    run: &common::Run,
) {
    for hook in hooks {
        let programs: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "Program" && r["hook"] == *hook)
            .collect();
        assert_eq!(programs.len(), 1, "one program for each selected hook: {run}");
        let reverse = matches!(*hook, "Recvmsg4" | "Getpeername4");
        assert_eq!(
            programs[0]["from"],
            target[if reverse { "responder" } else { "backend" }]
        );
        assert_eq!(
            programs[0]["to"],
            target[if reverse { "backend" } else { "responder" }]
        );
        for (kind, fact) in [("LinkAttached", None), ("LinkDetached", Some("detached"))] {
            let selected: Vec<_> = records
                .iter()
                .filter(|r| r["type"] == kind && r["hook"] == *hook)
                .collect();
            assert_eq!(selected.len(), 1, "actual {kind}/{hook}: {run}");
            if let Some(fact) = fact {
                assert_eq!(selected[0][fact], true, "actual detach: {run}");
            }
        }
    }
    for kind in ["Program", "LinkAttached", "LinkDetached"] {
        assert_eq!(
            records.iter().filter(|r| r["type"] == kind).count(),
            hooks.len(),
            "no unnamed extra hook: {run}"
        );
    }
    let cleanup = records
        .iter()
        .find(|r| r["type"] == "Cleanup" && r["state"] == "Observed")
        .expect("joined/detached/removed facts");
    assert_eq!(cleanup["hook_setup"], "Ready");
    assert_eq!(cleanup["links_required"], hooks.len());
    assert_eq!(cleanup["links_acquired"], hooks.len());
    for fact in [
        "backend_joined",
        "relay_joined",
        "link_detached",
        "cgroup_removed",
    ] {
        assert_eq!(cleanup[fact], true, "{fact}: {run}");
    }
}

#[test]
#[ignore = "coordinator alone loads hooks and runs actual guest compatibility comparison"]
fn direct_guest_socket_address_udp4_requires_original_native_tuple() {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let presentation = SockAddrPresentation::parse(&required("SHARDS_SOCK_ADDR_PRESENTATION")).unwrap();
    let profile = presentation.argument();
    let name = format!("sock-addr-udp4-{mode}-{profile}");
    let millis = input.case_ms.to_string();
    let run = input.boot(&name, &["sock-addr-udp4", &mode, &millis, profile]);
    let records = sock_addr_records(&run.stdout);
    // Raw boot/identity/status are durable before any compatibility assertion.
    let one = |kind: &str| {
        let selected: Vec<_> = records.iter().filter(|r| r["type"] == kind).collect();
        assert_eq!(selected.len(), 1, "{kind}: {run}");
        selected[0]
    };
    let target = one("Target");
    assert_ne!(target["backend"], target["responder"]);
    assert_eq!(target["profile"], format!("{presentation:?}"));
    assert_sock_addr_hooks_and_cleanup(&records, target, presentation.hooks(&mode), &run);
    let identities: Vec<_> = records.iter().filter(|r| r["type"] == "ClientIdentity").collect();
    assert_eq!(identities.len(), 2, "both fresh exec clients must report: {run}");
    assert_ne!(identities[0]["pid"], identities[1]["pid"]);
    for identity in &identities {
        assert_eq!(identity["namespace"], target["namespace"]);
        assert_eq!(identity["requested"], target["backend"]);
        let cgroup = identity["cgroup_hex"].as_str().expect("actual cgroup bytes");
        let bytes: Vec<_> = cgroup
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
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
            .any(|r| r["type"] == "Error" || r["type"] == "UdpError" || r["type"] == "HookError")
    );
    assert_guest_passed(&run, "destination rewrite alone is not compatibility success");
}

#[derive(Debug, PartialEq, Eq)]
enum BackendSource {
    OwnedUpstream,
    OriginalClient,
    Other,
    Ambiguous,
}
fn backend_source(
    actual: std::net::SocketAddr,
    upstream: std::net::SocketAddr,
    client: std::net::SocketAddr,
) -> BackendSource {
    if upstream == client {
        BackendSource::Ambiguous
    } else if actual == upstream {
        BackendSource::OwnedUpstream
    } else if actual == client {
        BackendSource::OriginalClient
    } else {
        BackendSource::Other
    }
}
#[test]
fn buffered_relay_source_witness_rejects_client_correct_direct_bypass() {
    let client = "127.0.0.1:41".parse().unwrap();
    let upstream = "127.0.0.1:42".parse().unwrap();
    assert_eq!(
        backend_source(upstream, upstream, client),
        BackendSource::OwnedUpstream
    );
    assert_eq!(
        backend_source(client, upstream, client),
        BackendSource::OriginalClient
    );
    assert_ne!(
        backend_source(client, upstream, client),
        BackendSource::OwnedUpstream
    );
    assert_eq!(backend_source(client, client, client), BackendSource::Ambiguous);
}

#[test]
#[ignore = "coordinator alone executes real two-leg buffered guest comparison"]
fn direct_guest_two_leg_udp4_records_buffers_and_backend_source_change() {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let presentation = SockAddrPresentation::parse(&required("SHARDS_SOCK_ADDR_PRESENTATION")).unwrap();
    let budget = positive("SHARDS_SOCK_ADDR_BUFFER_BYTES");
    let profile = presentation.argument();
    let millis = input.case_ms.to_string();
    let bytes = budget.to_string();
    let name = format!("sock-addr-relay4-{mode}-{profile}");
    std::fs::write(input.records.join(format!("{name}.payload-input.json")),serde_json::to_vec(&json!({"buffer_bytes":budget,"scope":"retained payload slots only; allocator/kernel/console not accounted"})).unwrap()).unwrap();
    let run = input.boot(&name, &["sock-addr-relay4", &mode, &millis, profile, &bytes]);
    let records = sock_addr_records(&run.stdout);
    let one = |kind: &str| {
        let found: Vec<_> = records.iter().filter(|r| r["type"] == kind).collect();
        assert_eq!(found.len(), 1, "{kind}: {run}");
        found[0]
    };
    let at = |value: &serde_json::Value| -> std::net::SocketAddr { value.as_str().unwrap().parse().unwrap() };
    let target = one("Target");
    let flow = one("ForwardTarget");
    let slots = one("PayloadSlots");
    assert_eq!(target["operation"], "BufferedRelay");
    assert_eq!(target["profile"], format!("{presentation:?}"));
    assert_eq!(flow["backend"], target["backend"]);
    assert_eq!(flow["ingress"], target["responder"]);
    assert_eq!(flow["upstream_peer"], target["backend"]);
    assert_eq!(slots["provided"], budget);
    assert!(
        slots["request_capacity"]
            .as_u64()
            .unwrap()
            .checked_add(slots["reply_capacity"].as_u64().unwrap())
            .unwrap()
            <= budget
    );
    assert_eq!(slots["charge_scope"], "PayloadCapacityOnly");
    assert_eq!(slots["allocator_kernel_console"], "Unaccounted");
    let identity = |role: &str| {
        let found: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "ClientIdentity" && r["role"] == role)
            .collect();
        assert_eq!(found.len(), 1, "client role {role}: {run}");
        found[0]
    };
    let baseline = identity("BaselineBackend");
    let client = identity("ForwardedBackend");
    assert_ne!(baseline["pid"], client["pid"]);
    for identity in [baseline, client] {
        assert_eq!(identity["namespace"], target["namespace"]);
        assert_eq!(identity["requested"], target["backend"]);
        let cgroup = identity["cgroup_hex"].as_str().unwrap();
        let bytes: Vec<_> = cgroup
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert!(String::from_utf8(bytes).unwrap().contains("/sock-addr-client\n"));
        if mode == "connected" {
            assert_eq!(
                identity["peer"], target["backend"],
                "client-visible original endpoint: {run}"
            );
        } else {
            assert!(identity["peer"].is_null());
            assert_eq!(identity["peer_error"], "NotConnected");
        }
    }
    let upstream = at(&flow["upstream"]);
    let actor = at(&client["local"]);
    assert_ne!(upstream, actor, "held upstream must be a distinct actual socket");
    let script = [
        ("Binary", "5245515545535400ff", "4241434b454e4400ff"),
        ("Empty", "", ""),
    ];
    let actual_arrivals = |role: &str| -> Vec<&serde_json::Value> {
        records
            .iter()
            .filter(|r| r["type"] == "UdpArrival" && r["role"] == role)
            .collect()
    };
    let check = |record: &serde_json::Value,
                 step: &str,
                 hex: &str,
                 peer: &serde_json::Value,
                 original: &serde_json::Value| {
        assert_eq!(record["step"], step);
        assert_eq!(record["copied_hex"], hex);
        assert_eq!(record["reported"], hex.len() / 2);
        assert_eq!(record["flags"].as_i64().unwrap() & 0x28, 0);
        assert_eq!(record["metadata"], "Known");
        assert_eq!(&record["peer"], peer);
        assert_eq!(&record["original"], original);
    };
    for (role, owner) in [
        ("SockAddrBaselineClient", baseline),
        ("SockAddrForwardedClient", client),
    ] {
        let arrivals = actual_arrivals(role);
        assert_eq!(arrivals.len(), script.len(), "both client datagrams: {run}");
        for (record, (step, _, reply)) in arrivals.into_iter().zip(script) {
            check(record, step, reply, &target["backend"], &owner["local"]);
        }
    }
    for (role, peer, original, reply) in [
        ("ForwardIngress", &client["local"], &flow["ingress"], false),
        (
            "ForwardUpstreamReply",
            &target["backend"],
            &flow["upstream"],
            true,
        ),
    ] {
        let arrivals = actual_arrivals(role);
        assert_eq!(arrivals.len(), script.len(), "owned custody {role}: {run}");
        for (record, (step, request, response)) in arrivals.into_iter().zip(script) {
            check(
                record,
                step,
                if reply { response } else { request },
                peer,
                original,
            );
        }
    }
    let backend = actual_arrivals("Backend");
    let real_scripts = [baseline, client];
    assert_eq!(
        backend.len(),
        script.len() * real_scripts.len(),
        "baseline AND actual forwarded receipts: {run}"
    );
    for (phase, owner) in real_scripts.into_iter().enumerate() {
        for (index, (step, request, _)) in script.into_iter().enumerate() {
            let record = backend[phase * script.len() + index];
            let peer = if phase == 0 {
                &owner["local"]
            } else {
                &flow["upstream"]
            };
            check(record, step, request, peer, &target["backend"]);
            if phase != 0 {
                assert_eq!(
                    backend_source(at(&record["peer"]), upstream, actor),
                    BackendSource::OwnedUpstream,
                    "client-correct direct backend bypass is NOT a relay: {run}"
                );
            }
        }
    }
    let exchanges: Vec<_> = records
        .iter()
        .filter(|r| r["type"] == "ForwardExchange")
        .collect();
    assert_eq!(exchanges.len(), script.len());
    for (record, (step, _, _)) in exchanges.into_iter().zip(script) {
        assert_eq!(record["step"], step);
        assert_eq!(record["client"], client["local"]);
        assert_eq!(record["upstream"], flow["upstream"]);
        assert_eq!(record["ingress"], flow["ingress"]);
        assert_eq!(record["backend"], target["backend"]);
        assert_eq!(record["source_preservation"], "Changed");
        assert_eq!(record["stage"], "ExchangeComplete");
    }
    let completed = one("ForwardCompletion");
    assert_eq!(completed["joined"], true);
    assert_eq!(completed["terminal"], "ScriptComplete");
    for field in [
        "ingress_received",
        "upstream_sent",
        "upstream_received",
        "client_sent",
    ] {
        assert_eq!(completed[field], script.len());
    }
    for field in ["empty_ingress", "empty_reply"] {
        assert_eq!(
            completed[field],
            script.iter().filter(|(step, _, _)| *step == "Empty").count()
        );
    }
    let backend_done: Vec<_> = records
        .iter()
        .filter(|r| r["type"] == "UdpWorkerCompletion" && r["role"] == "Backend")
        .collect();
    assert_eq!(backend_done.len(), 1);
    assert_eq!(backend_done[0]["joined"], true);
    assert_eq!(backend_done[0]["terminal"], "Cancelled");
    assert_eq!(backend_done[0]["arrivals"], script.len() * real_scripts.len());
    assert_eq!(backend_done[0]["matched"], script.len() * real_scripts.len());
    assert_eq!(
        backend_done[0]["empty"],
        script.iter().filter(|(step, _, _)| *step == "Empty").count() * real_scripts.len()
    );
    for role in ["BaselineBackend", "ForwardedBackend"] {
        let terminals: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "ClientCompletion" && r["role"] == role)
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0]["success"], true);
        assert_eq!(terminals[0]["compatibility"], "Exact");
        assert_eq!(terminals[0]["exchanges"], script.len());
    }
    for role in ["Backend", "ForwardedBackend"] {
        let reaped: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "ChildReaped" && r["role"] == role)
            .collect();
        assert_eq!(reaped.len(), 1, "owned {role} reap: {run}");
        assert_eq!(reaped[0]["reaped"], true);
        assert_eq!(reaped[0]["success"], true);
        assert_eq!(reaped[0]["code"], 0);
    }
    assert_sock_addr_hooks_and_cleanup(&records, target, presentation.hooks(&mode), &run);
    assert!(!records.iter().any(|r| matches!(
        r["type"].as_str(),
        Some("Error" | "HookError" | "ForwardError" | "UdpError" | "PayloadRefusal")
    )));
    assert_guest_passed(
        &run,
        "finite transfer with OBSERVED upstream source change; not full native transparency",
    );
}
