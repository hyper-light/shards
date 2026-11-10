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

#[test]
#[ignore = "coordinator runs isolated IPv4 TCP interception experiment; no production adoption"]
fn experimental_tcp4_interception_has_distinct_backend_and_relay_observations() {
    let inputs = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "real hypervisor required");
    let origin = required("SHARDS_INTERCEPT_ORIGIN");
    assert!(matches!(origin.as_str(), "local" | "forward"));
    let mark = required("SHARDS_INTERCEPT_MARK");
    let table = required("SHARDS_INTERCEPT_TABLE");
    let priority = required("SHARDS_INTERCEPT_RULE_PRIORITY");
    let millis = inputs.case_ms.to_string();
    let run = inputs.boot(
        &format!("intercept-tcp4-{origin}"),
        &["intercept-tcp4", &origin, &millis, &mark, &table, &priority],
    );
    assert_eq!(run.status, Some(0), "{run}");
    assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
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
    let baseline = one("Baseline");
    let intercepted = one("Intercepted");
    assert_eq!(intercepted["original"], baseline["destination"]);
    assert_eq!(intercepted["original"], intercepted["client_peer"]);
    assert_eq!(intercepted["peer"], intercepted["client_local"]);
    let joined = one("Joined");
    assert_eq!(joined["joined"], true);
    assert_eq!(joined["backend_accepted"], 1);
    assert_eq!(joined["relay_accepted"], 1);
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
