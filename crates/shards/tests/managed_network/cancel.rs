//! One finite coordinator-owned cancellation case; actual VM execution is ignored.
use super::*;
use serde_json::Value;

fn cancelled_receive(record: &Value) -> bool {
    record["type"] == "ForwardCompletion"
        && record["joined"] == true
        && record["terminal"] == "Cancelled"
        && record["step"] == "Binary"
        && record["stage"] == "UpstreamReceive"
        && record["upstream_progress"] == "KernelAccepted"
        && ["ingress_received", "upstream_sent"]
            .iter()
            .all(|field| record[*field] == 1)
        && ["upstream_received", "client_sent", "empty_ingress", "empty_reply"]
            .iter()
            .all(|field| record[*field] == 0)
}

#[test]
fn cancellation_record_rejects_work_expiry_and_preserves_sent_fact() {
    let mut record = json!({"type":"ForwardCompletion","joined":true,"terminal":"Cancelled","step":"Binary","stage":"UpstreamReceive","upstream_progress":"KernelAccepted","ingress_received":1,"upstream_sent":1,"upstream_received":0,"client_sent":0,"empty_ingress":0,"empty_reply":0});
    assert!(cancelled_receive(&record));
    record["terminal"] = json!("Error");
    assert!(!cancelled_receive(&record));
    assert_eq!(record["upstream_sent"], 1);
    record["terminal"] = json!("Cancelled");
    record["stage"] = json!("IngressReceive");
    assert!(!cancelled_receive(&record));
    record["stage"] = json!("UpstreamReceive");
    record["client_sent"] = json!(1);
    assert!(!cancelled_receive(&record));
}

#[derive(Clone, Copy)]
enum CancellationCase {
    UpstreamReceive,
    BeforeUpstreamSend,
}
fn cancelled_before_send(record: &Value) -> bool {
    record["type"] == "ForwardCompletion"
        && record["joined"] == true
        && record["terminal"] == "Cancelled"
        && record["step"] == "Binary"
        && matches!(
            record["stage"].as_str(),
            Some("BeforeUpstreamSend" | "UpstreamSend")
        )
        && record["upstream_progress"] == "NotAttempted"
        && record["ingress_received"] == 1
        && [
            "upstream_sent",
            "upstream_received",
            "client_sent",
            "empty_ingress",
            "empty_reply",
        ]
        .iter()
        .all(|field| record[*field] == 0)
}
#[test]
fn precommit_record_rejects_an_actual_upstream_send() {
    let mut record = json!({"type":"ForwardCompletion","joined":true,"terminal":"Cancelled","step":"Binary","stage":"UpstreamSend","upstream_progress":"NotAttempted","ingress_received":1,"upstream_sent":0,"upstream_received":0,"client_sent":0,"empty_ingress":0,"empty_reply":0});
    assert!(cancelled_before_send(&record));
    record["upstream_progress"] = json!("Attempted");
    assert!(!cancelled_before_send(&record));
    record["upstream_progress"] = json!("NotAttempted");
    record["upstream_sent"] = json!(1);
    assert!(!cancelled_before_send(&record));
    assert_eq!(record["ingress_received"], 1);
}
#[test]
#[ignore = "coordinator alone executes real upstream-receive cancellation and cleanup"]
fn direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline() {
    cancellation_case(CancellationCase::UpstreamReceive);
}
#[test]
#[ignore = "coordinator alone checks pending cancellation before actual next upstream send"]
fn direct_guest_udp4_precommit_cancel_requires_no_backend_effect() {
    cancellation_case(CancellationCase::BeforeUpstreamSend);
}
fn cancellation_case(case: CancellationCase) {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let budget = positive("SHARDS_SOCK_ADDR_BUFFER_BYTES");
    let millis = input.case_ms.to_string();
    let bytes = budget.to_string();
    let (command, operation, backend_expected, backend_stage) = match case {
        CancellationCase::UpstreamReceive => ("sock-addr-cancel4", "CancelUpstreamReceive", 3, "HoldReply"),
        CancellationCase::BeforeUpstreamSend => (
            "sock-addr-precommit4",
            "CancelBeforeUpstreamSend",
            2,
            "ReceiveDatagram",
        ),
    };
    let name = format!("{command}-{mode}");
    let run = input.boot(&name, &[command, &mode, &millis, &bytes]);
    let records = sock_addr_records(&run.stdout);
    let select = |kind: &str| -> Vec<&Value> { records.iter().filter(|r| r["type"] == kind).collect() };
    let one = |kind: &str| {
        let found = select(kind);
        assert_eq!(found.len(), 1, "{kind}: {run}");
        found[0]
    };
    let target = one("Target");
    let flow = one("ForwardTarget");
    let slots = one("PayloadSlots");
    assert_eq!(target["operation"], operation);
    assert_eq!(target["profile"], "FixedOriginalPeer");
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
    let identities = select("ClientIdentity");
    assert_eq!(identities.len(), 2);
    let identity = |role: &str| {
        let found: Vec<_> = identities.iter().copied().filter(|r| r["role"] == role).collect();
        assert_eq!(found.len(), 1, "{role}: {run}");
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
            assert_eq!(identity["peer"], target["backend"]);
        } else {
            assert!(identity["peer"].is_null());
            assert_eq!(identity["peer_error"], "NotConnected");
        }
    }
    let arrivals = |role: &str| -> Vec<&Value> {
        records
            .iter()
            .filter(|r| r["type"] == "UdpArrival" && r["role"] == role)
            .collect()
    };
    let check = |r: &Value, step: &str, hex: &str, peer: &Value, original: &Value| {
        assert_eq!(r["step"], step);
        assert_eq!(r["copied_hex"], hex);
        assert_eq!(r["reported"], hex.len() / 2);
        // Linux guest MSG_TRUNC | MSG_CTRUNC, independent of the host ABI.
        assert_eq!(r["flags"].as_i64().unwrap() & 0x28, 0);
        assert_eq!(r["metadata"], "Known");
        assert_eq!(&r["peer"], peer);
        assert_eq!(&r["original"], original);
    };
    let baseline_arrivals = arrivals("SockAddrBaselineClient");
    assert_eq!(baseline_arrivals.len(), 2);
    for (r, (step, hex)) in baseline_arrivals
        .into_iter()
        .zip([("Binary", "4241434b454e4400ff"), ("Empty", "")])
    {
        check(r, step, hex, &target["backend"], &baseline["local"]);
    }
    let ingress = arrivals("ForwardIngress");
    assert_eq!(ingress.len(), 1);
    check(
        ingress[0],
        "Binary",
        "5245515545535400ff",
        &client["local"],
        &flow["ingress"],
    );
    let backend = arrivals("Backend");
    assert_eq!(
        backend.len(),
        backend_expected,
        "exact independent baseline/forwarded receipts: {run}"
    );
    for (r, step, hex) in [
        (backend[0], "Binary", "5245515545535400ff"),
        (backend[1], "Empty", ""),
    ] {
        check(r, step, hex, &baseline["local"], &target["backend"]);
    }
    match case {
        CancellationCase::UpstreamReceive => {
            check(
                backend[2],
                "Binary",
                "5245515545535400ff",
                &flow["upstream"],
                &target["backend"],
            );
            let at = |v: &Value| v.as_str().unwrap().parse::<std::net::SocketAddr>().unwrap();
            assert_eq!(
                backend_source(
                    at(&backend[2]["peer"]),
                    at(&flow["upstream"]),
                    at(&client["local"])
                ),
                BackendSource::OwnedUpstream
            );
            let held = one("HeldReply");
            assert_eq!(held["stage"], "HoldReply");
            assert_eq!(held["step"], "Binary");
            assert_eq!(held["receipt"], "ValidatedBackendDatagram");
            assert_eq!(held["reply"], "NotIssued");
            let entered = one("ReceiveStageEntered");
            assert_eq!(entered["stage"], "UpstreamReceive");
            assert_eq!(entered["step"], "Binary");
            assert_eq!(entered["upstream_progress"], "KernelAccepted");
            assert_eq!(entered["kernel_sleep"], "Unobserved");
        }
        CancellationCase::BeforeUpstreamSend => {
            let ready = one("IngressValidated");
            assert_eq!(ready["stage"], "BeforeUpstreamSend");
            assert_eq!(ready["step"], "Binary");
            assert_eq!(ready["upstream_progress"], "NotAttempted");
            assert_eq!(ready["ingress_received"], 1);
            assert!(select("HeldReply").is_empty());
            assert!(select("ReceiveStageEntered").is_empty());
            assert_eq!(one("PreSendRelease")["state"], "ReleasedAfterCancelRequest");
        }
    }
    let boundaries = select("CancelBoundary");
    assert_eq!(boundaries.len(), 2);
    assert_eq!(boundaries[0]["state"], "Requested");
    let (stage, receipt) = match case {
        CancellationCase::UpstreamReceive => ("UpstreamReceive", "ValidatedBackendDatagram"),
        CancellationCase::BeforeUpstreamSend => ("BeforeUpstreamSend", "ValidatedOwnedIngress"),
    };
    assert_eq!(boundaries[0]["stage"], stage);
    assert_eq!(boundaries[0]["receipt"], receipt);
    assert_eq!(boundaries[0]["kernel_sleep"], "Unobserved");
    assert_eq!(boundaries[1]["state"], "JoinedWithinWorkDeadline");
    for boundary in boundaries {
        assert!(boundary["work_remaining_ns"].as_u64().unwrap() > 0);
    }
    let position = |r: &Value| records.iter().position(|value| std::ptr::eq(value, r)).unwrap();
    let requested = select("CancelBoundary")[0];
    match case {
        CancellationCase::UpstreamReceive => {
            assert!(position(backend[2]) < position(one("HeldReply")));
            assert!(position(one("HeldReply")) < position(requested));
            assert!(position(one("ReceiveStageEntered")) < position(requested));
            assert!(cancelled_receive(one("ForwardCompletion")), "{run}");
        }
        CancellationCase::BeforeUpstreamSend => {
            assert!(position(ingress[0]) < position(one("IngressValidated")));
            assert!(position(one("IngressValidated")) < position(requested));
            assert!(position(requested) < position(one("PreSendRelease")));
            assert!(cancelled_before_send(one("ForwardCompletion")), "{run}");
        }
    }
    let worker = one("UdpWorkerCompletion");
    assert_eq!(worker["role"], "Backend");
    assert_eq!(worker["joined"], true);
    assert_eq!(worker["stage"], backend_stage);
    assert_eq!(worker["terminal"], "Cancelled");
    assert_eq!(worker["arrivals"], backend_expected);
    assert_eq!(worker["matched"], backend_expected);
    assert_eq!(worker["empty"], 1);
    assert_eq!(worker["replies_sent"], 2, "only baseline replies issued: {run}");
    for absent in [
        "ForwardUpstreamReply",
        "SockAddrForwardedClient",
        "BackendUnexpected",
    ] {
        assert!(arrivals(absent).is_empty());
    }
    assert!(select("ForwardExchange").is_empty());
    let completed = one("ClientCompletion");
    assert_eq!(completed["role"], "BaselineBackend");
    assert_eq!(completed["success"], true);
    assert_eq!(completed["compatibility"], "Exact");
    assert_eq!(completed["exchanges"], 2);
    let child_cancel = one("ChildCancellation");
    assert_eq!(child_cancel["role"], "ForwardedBackend");
    assert_eq!(child_cancel["requested"], true);
    let reaped = select("ChildReaped");
    assert_eq!(reaped.len(), 2);
    for (r, role, success, code, signal) in [
        (reaped[0], "Backend", true, json!(0), Value::Null),
        (reaped[1], "ForwardedBackend", false, Value::Null, json!(9)),
    ] {
        assert_eq!(r["role"], role);
        assert_eq!(r["reaped"], true);
        assert_eq!(r["success"], success);
        assert_eq!(r["code"], code);
        assert_eq!(r["signal"], signal);
    }
    let hooks = SockAddrPresentation::FixedOriginalPeer.hooks(&mode);
    let observed: Vec<_> = select("Cleanup")
        .into_iter()
        .filter(|r| r["state"] == "Observed")
        .collect();
    assert_eq!(observed.len(), 1);
    assert_sock_addr_hooks_and_cleanup(&records, target, hooks, &run);
    assert!(!records.iter().any(|r| matches!(
        r["type"].as_str(),
        Some("Error" | "HookError" | "ForwardError" | "UdpError" | "PayloadRefusal")
    )));
    assert_guest_passed(&run, &name);
}
