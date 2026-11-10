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

#[test]
#[ignore = "coordinator alone executes real upstream-receive cancellation and cleanup"]
fn direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline() {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let budget = positive("SHARDS_SOCK_ADDR_BUFFER_BYTES");
    let millis = input.case_ms.to_string();
    let bytes = budget.to_string();
    let name = format!("sock-addr-cancel4-{mode}");
    let run = input.boot(&name, &["sock-addr-cancel4", &mode, &millis, &bytes]);
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
    assert_eq!(target["operation"], "CancelUpstreamReceive");
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
        3,
        "baseline Binary/Empty plus actual held receipt: {run}"
    );
    for (r, step, hex, peer) in [
        (backend[0], "Binary", "5245515545535400ff", &baseline["local"]),
        (backend[1], "Empty", "", &baseline["local"]),
        (backend[2], "Binary", "5245515545535400ff", &flow["upstream"]),
    ] {
        check(r, step, hex, peer, &target["backend"]);
    }
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
    let boundaries = select("CancelBoundary");
    assert_eq!(boundaries.len(), 2);
    assert_eq!(boundaries[0]["state"], "Requested");
    assert_eq!(boundaries[0]["receipt"], "ValidatedBackendDatagram");
    assert_eq!(boundaries[0]["kernel_sleep"], "Unobserved");
    assert_eq!(boundaries[1]["state"], "JoinedWithinWorkDeadline");
    for boundary in boundaries {
        assert!(boundary["work_remaining_ns"].as_u64().unwrap() > 0);
    }
    let position = |r: &Value| records.iter().position(|value| std::ptr::eq(value, r)).unwrap();
    let requested = select("CancelBoundary")[0];
    assert!(position(backend[2]) < position(held));
    assert!(position(held) < position(requested));
    assert!(position(entered) < position(requested));
    assert!(cancelled_receive(one("ForwardCompletion")), "{run}");
    let worker = one("UdpWorkerCompletion");
    assert_eq!(worker["role"], "Backend");
    assert_eq!(worker["joined"], true);
    assert_eq!(worker["stage"], "HoldReply");
    assert_eq!(worker["terminal"], "Cancelled");
    assert_eq!(worker["arrivals"], 3);
    assert_eq!(worker["matched"], 3);
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
    assert_eq!(run.status, Some(0), "{run}");
}
