//! Independent native endpoint/tuple/half-close assertions for the owned pump.
use super::*;

fn tcp_records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("managed-tcp-forward "))
        .map(|line| serde_json::from_str(line).expect("typed TCP fixture record"))
        .collect()
}

fn supplied_payload(key: &str) -> (String, String) {
    let word = required(key).to_ascii_lowercase();
    let (length, seed) = if let Some(repeated) = word.strip_prefix("repeat:") {
        let (length, seed) = repeated.split_once(':').expect("repeat byte-count:hex-seed");
        (Some(length.parse::<usize>().unwrap()), seed)
    } else {
        (None, word.as_str())
    };
    assert!(seed.len().is_multiple_of(2) && seed.bytes().all(|b| b.is_ascii_hexdigit()));
    let hex = if let Some(bytes) = length {
        assert!(!seed.is_empty());
        let chars = bytes.checked_mul(2).unwrap();
        let (count, tail) = (chars / seed.len(), chars % seed.len());
        let mut expanded = seed.repeat(count);
        expanded.push_str(seed.get(..tail).expect("validated hex prefix"));
        expanded
    } else {
        seed.to_owned()
    };
    (word, hex)
}

#[test]
#[ignore = "caller supplies finite KVM, artifact, byte and time envelopes"]
fn managed_tcp_forward_preserves_both_half_closes() {
    let input = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "actual hypervisor required");
    let scenario = required("SHARDS_TCP_SCENARIO");
    assert!(matches!(scenario.as_str(), "server-first" | "backend-fin-first"));
    let (request_arg, request) = supplied_payload("SHARDS_TCP_REQUEST_HEX");
    let (greeting_arg, greeting) = supplied_payload("SHARDS_TCP_GREETING_HEX");
    let (tail_arg, tail) = supplied_payload("SHARDS_TCP_TAIL_HEX");
    assert!(!request.is_empty() && !greeting.is_empty());
    let up = positive("SHARDS_TCP_UP_BYTES").to_string();
    let down = positive("SHARDS_TCP_DOWN_BYTES").to_string();
    let mark = required("SHARDS_INTERCEPT_MARK");
    let table = required("SHARDS_INTERCEPT_TABLE");
    let priority = required("SHARDS_INTERCEPT_RULE_PRIORITY");
    let millis = input.case_ms.to_string();
    let run = input.boot(
        "tcp-forward",
        &[
            "intercept-tcp-forward4",
            &scenario,
            &millis,
            &mark,
            &table,
            &priority,
            &up,
            &down,
            &request_arg,
            &greeting_arg,
            &tail_arg,
        ],
    );
    let rows = tcp_records(&run.stdout);
    std::fs::write(
        input.records.join("tcp-forward.observed.json"),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
    assert_guest_passed(&run, "TCP forward guest");
    let one = |kind| {
        let got: Vec<_> = rows.iter().filter(|row| row["type"] == kind).collect();
        assert_eq!(got.len(), 1, "one {kind}: {rows:?}");
        got[0]
    };
    let endpoint = |role, ordinal| {
        let got: Vec<_> = rows
            .iter()
            .filter(|r| r["type"] == "Endpoint" && r["role"] == role && r["ordinal"] == ordinal)
            .collect();
        assert_eq!(got.len(), 1, "one native endpoint: {rows:?}");
        got[0]
    };
    assert_eq!(rows.iter().filter(|r| r["type"] == "Endpoint").count(), 4);
    let response = format!("{greeting}{tail}");
    for ordinal in [0, 1] {
        let backend = endpoint("Backend", ordinal);
        let client = endpoint("Client", ordinal);
        for row in [backend, client] {
            assert_eq!(row["ok"], true, "{row:?}");
            assert_eq!(row["read_end"], "Closed");
            assert_eq!(row["write_end"], "Closed");
            assert_eq!(row["error_kind"], "None");
            assert!(row["errno"].is_null());
            assert!(row["extra_hex"].is_null());
        }
        assert_eq!(backend["read_hex"], request);
        assert_eq!(client["read_hex"], response);
        assert_eq!(backend["read"].as_u64(), Some((request.len() / 2) as u64));
        assert_eq!(client["read"].as_u64(), Some((response.len() / 2) as u64));
        assert_eq!(backend["written"], client["read"]);
        assert_eq!(client["written"], backend["read"]);
    }
    let joined = one("Joined");
    assert_eq!(joined["relay_joined"], true);
    assert_eq!(joined["backend_joined"], true);
    let done = one("Completion");
    assert_eq!(done["connect"], "Connected");
    assert_eq!(done["accepted"], 1);
    assert_eq!(done["backend_accepted"], 2);
    assert_eq!(done["ok"], true);
    assert!(done["fault"].is_null());
    for (direction, bytes) in [
        ("to_backend", request.len() / 2),
        ("to_client", response.len() / 2),
    ] {
        assert_eq!(done[direction]["half"], "WriteClosed");
        assert_eq!(done[direction]["source_end"], "Eof");
        assert_eq!(done[direction]["read"].as_u64(), Some(bytes as u64));
        assert_eq!(done[direction]["written"].as_u64(), Some(bytes as u64));
        assert_eq!(done[direction]["pending"], 0);
        assert_eq!(done[direction]["extra_consumed"], 0);
    }
    let exact = one("Exact");
    let client = endpoint("Client", 1);
    let backend = endpoint("Backend", 1);
    assert_eq!(client["local"], exact["client"]);
    assert_eq!(client["peer"], exact["destination"]);
    assert_eq!(backend["local"], exact["destination"]);
    assert_eq!(backend["peer"], exact["upstream"]);
    assert_ne!(backend["peer"], client["local"], "actual direct-backend bypass");
    assert_eq!(endpoint("Backend", 0)["peer"], endpoint("Client", 0)["local"]);
    let cleanup = one("Cleanup");
    assert_eq!(cleanup["fd_before"], cleanup["fd_after"]);
    assert_eq!(cleanup["kernel_return"], "Unknown");
}

#[test]
#[ignore = "finite one-VM old/new init engine compatibility measurement"]
fn managed_engine_half_close_records_actual_host_finish() {
    let input = Inputs::supplied();
    assert!(!common::cannot_run_vms(), "actual hypervisor required");
    let mode = required("SHARDS_ENGINE_HALF_MODE");
    assert!(matches!(mode.as_str(), "client-fin-first" | "host-fin-first"));
    let (request_arg, request) = supplied_payload("SHARDS_ENGINE_REQUEST_HEX");
    let (suffix_arg, suffix) = supplied_payload("SHARDS_ENGINE_SUFFIX_HEX");
    let (late_arg, _) = supplied_payload("SHARDS_ENGINE_LATE_HEX");
    let maximum = positive("SHARDS_ENGINE_RESPONSE_BYTES").to_string();
    input.identity("engine-half-close");
    let home = common::TempDir::new("relay-eng");
    let init = common::guest_init();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", input.kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    // The first command starts the home's daemon: from here on it is stopped however the
    // test ends.
    let mut daemon = Daemon {
        input: &input,
        env: &env,
        stopped: false,
    };
    let (image, _server) = common::served();
    let pulled = common::run_shards_env(&[], &["pull", "-q", &image], &env, input.boot);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let memory = format!("{}m", input.memory_mib);
    let cpus = input.cpus.to_string();
    let millis = input.case_ms.to_string();
    let run = common::run_shards_env(
        &[],
        &[
            "run",
            "--rm",
            "--use-api-socket",
            "--memory",
            &memory,
            "--memory-swap",
            &memory,
            "--cpus",
            &cpus,
            &image,
            "managed-network",
            "engine-halfclose",
            &mode,
            &millis,
            shards_abi::run::ENGINE_SOCKET,
            &maximum,
            &request_arg,
            &suffix_arg,
            &late_arg,
        ],
        &env,
        input.boot,
    );
    std::fs::write(input.records.join("engine.stdout"), &run.stdout).unwrap();
    std::fs::write(input.records.join("engine.stderr"), &run.stderr).unwrap();
    let rows = tcp_records(&run.stdout);
    std::fs::write(
        input.records.join("engine.observed.json"),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
    assert_eq!(run.status, Some(0), "{}\n{}", run.stdout, run.stderr);
    let got: Vec<_> = rows
        .iter()
        .filter(|row| row["type"] == "EngineHalfClose")
        .collect();
    assert_eq!(got.len(), 1, "{rows:?}");
    let row = got[0];
    assert_eq!(row["ok"], true, "{row:?}");
    assert_eq!(row["phase"], "ResponseEof");
    assert_eq!(row["read_end"], "Closed");
    assert_eq!(row["written"].as_u64(), Some((request.len() / 2) as u64));
    assert!(row["read_hex"].as_str().unwrap().ends_with(&suffix));
    assert_eq!(row["fd_before"], row["fd_after"]);
    assert_eq!(row["worker_joined"], true);
    assert_eq!(row["kernel_return"], "Unknown");
    if mode == "client-fin-first" {
        assert_eq!(row["write_end"], "Closed");
        assert_eq!(row["late_write"]["kind"], "NotAttempted");
    } else {
        assert_eq!(row["write_end"], "Open");
        // The engine closes the client's two halves before its EOF (managed-engine-relay-root:
        // "actual late X fails BrokenPipe/errno32" at every source compared), so the late byte
        // has one answer. 32 is the guest's Linux EPIPE (asm-generic/errno-base.h).
        assert_eq!(
            row["late_write"],
            json!({"kind":"Failed","error_kind":"BrokenPipe","errno":32}),
            "{row:?}"
        );
    }
    let stop = daemon.stop();
    assert_eq!(stop.status, Some(0), "{stop}");
}

/// The engine test's daemon, stopped once: by the test as it ends, or as it unwinds. Either
/// way `engine.daemon-stop.json` records the stop.
struct Daemon<'a> {
    input: &'a Inputs,
    env: &'a [(&'a str, &'a std::ffi::OsStr)],
    stopped: bool,
}

impl Daemon<'_> {
    fn stop(&mut self) -> common::Run {
        self.stopped = true;
        let stop = common::run_shards_env(&[], &["stop", "daemon"], self.env, self.input.boot);
        let record = json!({"status":stop.status,"stdout":stop.stdout,"stderr":stop.stderr,"test_unwinding":std::thread::panicking()});
        if let Err(error) = std::fs::write(
            self.input.records.join("engine.daemon-stop.json"),
            serde_json::to_vec(&record).unwrap_or_default(),
        ) {
            let _ = writeln!(std::io::stderr(), "engine.daemon-stop.json: {error}");
        }
        stop
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}
