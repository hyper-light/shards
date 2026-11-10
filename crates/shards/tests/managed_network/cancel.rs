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
    BackendComplete,
    UnexpectedExtra,
}
fn unexpected_backend_work(record: &Value, error: &Value) -> bool {
    record["type"] == "UdpWorkerCompletion"
        && record["role"] == "Backend"
        && record["joined"] == true
        && record["stage"] == "HoldReply"
        && record["terminal"] == "Error"
        && record["arrivals"] == 4
        && record["matched"] == 3
        && record["empty"] == 1
        && record["replies_sent"] == 2
        && error["type"] == "UdpError"
        && error["role"] == "Backend"
        && error["stage"] == "HoldReply"
        && error["cause"] == "UnexpectedDatagram"
        && error["kind"].is_null()
        && error["errno"].is_null()
}
#[test]
fn unexpected_work_record_retains_consumed_extra_and_rejects_expiry_or_cancel() {
    let mut record = json!({"type":"UdpWorkerCompletion","role":"Backend","joined":true,"stage":"HoldReply","terminal":"Error","arrivals":4,"matched":3,"empty":1,"replies_sent":2});
    let mut error = json!({"type":"UdpError","role":"Backend","stage":"HoldReply","cause":"UnexpectedDatagram","kind":null,"errno":null});
    assert!(unexpected_backend_work(&record, &error));
    record["terminal"] = json!("Cancelled");
    assert!(!unexpected_backend_work(&record, &error));
    record["terminal"] = json!("Error");
    error["cause"] = json!("Io");
    error["kind"] = json!("TimedOut");
    assert!(!unexpected_backend_work(&record, &error));
    assert_eq!(record["arrivals"], 4);
    error["cause"] = json!("UnexpectedDatagram");
    error["kind"] = Value::Null;
    record["matched"] = json!(4);
    assert!(!unexpected_backend_work(&record, &error));
}
fn backend_work_returned(record: &Value) -> bool {
    record["type"] == "UdpWorkerCompletion"
        && record["role"] == "Backend"
        && record["joined"] == true
        && record["stage"] == "BackendComplete"
        && record["terminal"] == "BackendComplete"
        && record["arrivals"] == 3
        && record["matched"] == 3
        && record["empty"] == 1
        && record["replies_sent"] == 2
}
#[test]
fn backend_work_record_rejects_cancel_expiry_and_unjoined_or_missing_receipts() {
    let mut record = json!({"type":"UdpWorkerCompletion","role":"Backend","joined":true,"stage":"BackendComplete","terminal":"BackendComplete","arrivals":3,"matched":3,"empty":1,"replies_sent":2});
    assert!(backend_work_returned(&record));
    for terminal in ["Cancelled", "Error"] {
        record["terminal"] = json!(terminal);
        assert!(!backend_work_returned(&record));
        assert_eq!(record["arrivals"], 3);
    }
    record["terminal"] = json!("BackendComplete");
    record["joined"] = json!(false);
    assert!(!backend_work_returned(&record));
    record["joined"] = json!(true);
    record["matched"] = json!(2);
    assert!(!backend_work_returned(&record));
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
fn expired_before_send(record: &Value, error: &Value) -> bool {
    record["type"] == "ForwardCompletion"
        && record["joined"] == true
        && record["terminal"] == "Error"
        && record["step"] == "Binary"
        && record["stage"] == "BeforeUpstreamSend"
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
        && error["type"] == "ForwardError"
        && error["stage"] == "BeforeUpstreamSend"
        && error["progress"] == "NotAttempted"
        && error["cause"] == "Io"
        && error["kind"] == "TimedOut"
}
#[test]
fn notification_expiry_record_cannot_claim_cancellation_or_hide_a_sent_datagram() {
    let mut record = json!({"type":"ForwardCompletion","joined":true,"terminal":"Error","step":"Binary","stage":"BeforeUpstreamSend","upstream_progress":"NotAttempted","ingress_received":1,"upstream_sent":0,"upstream_received":0,"client_sent":0,"empty_ingress":0,"empty_reply":0});
    let mut error = json!({"type":"ForwardError","stage":"BeforeUpstreamSend","progress":"NotAttempted","cause":"Io","kind":"TimedOut"});
    assert!(expired_before_send(&record, &error));
    assert!(!cancelled_before_send(&record));
    error["kind"] = json!("InvalidInput");
    assert!(!expired_before_send(&record, &error));
    error["kind"] = json!("TimedOut");
    record["terminal"] = json!("Cancelled");
    assert!(!expired_before_send(&record, &error));
    record["terminal"] = json!("Error");
    record["upstream_sent"] = json!(1);
    assert!(!expired_before_send(&record, &error));
    assert_eq!(record["upstream_sent"], 1);
}

#[test]
#[ignore = "coordinator alone executes real duplicate/missing notification and original-expiry cleanup"]
fn direct_guest_udp4_notification_failure_retains_ingress_until_original_expiry() {
    let input = Inputs::supplied();
    let mode = required("SHARDS_SOCK_ADDR_MODE");
    assert!(matches!(mode.as_str(), "connected" | "unconnected"));
    let case = required("SHARDS_BOUNDARY_NOTICE");
    let (notice, kind, word, write) = match case.as_str() {
        "duplicate-ready" => ("Duplicate", "InvalidInput", json!(2), "Completed"),
        "missing-ready" => ("Missing", "TimedOut", Value::Null, "Omitted"),
        _ => panic!("supply duplicate-ready or missing-ready"),
    };
    let budget = positive("SHARDS_SOCK_ADDR_BUFFER_BYTES");
    let millis = input.case_ms.to_string();
    let bytes = budget.to_string();
    let run = input.boot(
        &format!("sock-addr-boundary4-{mode}-{case}"),
        &["sock-addr-boundary4", &mode, &millis, &bytes, &case],
    );
    let records = sock_addr_records(&run.stdout);
    let select = |kind: &str| -> Vec<&Value> { records.iter().filter(|r| r["type"] == kind).collect() };
    let one = |kind: &str| {
        let found = select(kind);
        assert_eq!(found.len(), 1, "{kind}: {run}");
        found[0]
    };
    let target = one("Target");
    let flow = one("ForwardTarget");
    assert_eq!(target["operation"], "BoundaryNotification");
    assert_eq!(target["profile"], "FixedOriginalPeer");
    assert_eq!(flow["backend"], target["backend"]);
    assert_eq!(flow["ingress"], target["responder"]);
    assert_eq!(flow["upstream_peer"], target["backend"]);
    let slots = one("PayloadSlots");
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
        assert_eq!(found.len(), 1);
        found[0]
    };
    let baseline = identity("BaselineBackend");
    let client = identity("ForwardedBackend");
    assert_ne!(baseline["pid"], client["pid"]);
    for identity in [baseline, client] {
        assert_eq!(identity["namespace"], target["namespace"]);
        assert_eq!(identity["requested"], target["backend"]);
        let raw = identity["cgroup_hex"].as_str().unwrap();
        let bytes: Vec<_> = raw
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
        assert_eq!(r["flags"].as_i64().unwrap() & 0x28, 0);
        assert_eq!(r["metadata"], "Known");
        assert_eq!(&r["peer"], peer);
        assert_eq!(&r["original"], original);
    };
    let backend = arrivals("Backend");
    let baseline_replies = arrivals("SockAddrBaselineClient");
    assert_eq!(
        backend.len(),
        2,
        "independent actual baseline-only backend receipts: {run}"
    );
    assert_eq!(baseline_replies.len(), 2);
    for (index, step, request, response) in [
        (0, "Binary", "5245515545535400ff", "4241434b454e4400ff"),
        (1, "Empty", "", ""),
    ] {
        check(
            backend[index],
            step,
            request,
            &baseline["local"],
            &target["backend"],
        );
        check(
            baseline_replies[index],
            step,
            response,
            &target["backend"],
            &baseline["local"],
        );
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
    let issued = one("BoundaryNoticeIssued");
    assert_eq!(issued["notice"], notice);
    assert_eq!(issued["word"], word);
    assert_eq!(issued["write"], write);
    let observed = one("BoundaryObservation");
    assert_eq!(observed["notice"], notice);
    assert_eq!(observed["state"], "Rejected");
    assert_eq!(observed["kind"], kind);
    assert_eq!(observed["release"], "WithheldUntilOriginalDeadline");
    let outcome = one("BoundaryOutcome");
    assert_eq!(outcome["notice"], notice);
    assert_eq!(outcome["observed"], "Rejected");
    assert_eq!(outcome["kind"], kind);
    assert_eq!(outcome["worker"], "ExpiredBeforeSend");
    assert_eq!(outcome["deadline"], "Original");
    assert!(
        expired_before_send(one("ForwardCompletion"), one("ForwardError")),
        "{run}"
    );
    let worker = one("UdpWorkerCompletion");
    assert_eq!(worker["role"], "Backend");
    assert_eq!(worker["joined"], true);
    assert_eq!(worker["stage"], "ReceiveDatagram");
    assert_eq!(worker["arrivals"], 2);
    assert_eq!(worker["matched"], 2);
    assert_eq!(worker["empty"], 1);
    assert_eq!(worker["replies_sent"], 2);
    let faults = select("UdpError");
    match worker["terminal"].as_str() {
        Some("Cancelled") => assert!(faults.is_empty()),
        Some("Error") => {
            assert_eq!(faults.len(), 1);
            assert_eq!(faults[0]["role"], "Backend");
            assert_eq!(faults[0]["cause"], "Io");
            assert_eq!(faults[0]["kind"], "TimedOut");
            assert_eq!(faults[0]["stage"], "ReceiveDatagram");
        }
        _ => panic!("backend terminal differs: {run}"),
    }
    let errors = select("Error");
    assert_eq!(
        errors
            .iter()
            .filter(|r| r["stage"] == "AwaitCancellationBoundary" && r["kind"] == kind)
            .count(),
        1
    );
    assert!(errors.iter().all(
        |r| (r["stage"] == "AwaitCancellationBoundary" && r["kind"] == kind)
            || (matches!(r["stage"].as_str(), Some("WaitChild" | "Receive")) && r["kind"] == "TimedOut")
    ));
    let reaped = select("ChildReaped");
    assert_eq!(reaped.len(), 2);
    assert_eq!(reaped[0]["role"], "Backend");
    assert_eq!(reaped[0]["success"], true);
    assert_eq!(reaped[0]["code"], 0);
    assert!(reaped[0]["signal"].is_null());
    assert_eq!(reaped[1]["role"], "ForwardedBackend");
    assert_eq!(reaped[1]["success"], false);
    assert!(
        (reaped[1]["signal"] == 9 && reaped[1]["code"].is_null())
            || (reaped[1]["code"] == 1 && reaped[1]["signal"].is_null())
    );
    assert!(reaped.iter().all(|r| r["reaped"] == true));
    let children = select("ClientCompletion");
    assert!(
        children
            .iter()
            .any(|r| r["role"] == "BaselineBackend" && r["success"] == true && r["exchanges"] == 2)
    );
    if reaped[1]["code"] == 1 {
        assert!(children.iter().any(|r| r["role"] == "ForwardedBackend"
            && r["success"] == false
            && r["stage"] == "Receive"
            && r["exchanges"] == 0));
        assert!(
            errors
                .iter()
                .any(|r| r["stage"] == "Receive" && r["kind"] == "TimedOut")
        );
    }
    assert_eq!(one("ChildCancellation")["role"], "ForwardedBackend");
    assert_eq!(one("ChildCancellation")["requested"], true);
    for absent in [
        "CancelBoundary",
        "PreSendRelease",
        "ForwardExchange",
        "HeldReply",
        "ReceiveStageEntered",
        "HookError",
        "PayloadRefusal",
    ] {
        assert!(select(absent).is_empty(), "{absent}: {run}");
    }
    for absent in [
        "ForwardUpstreamReply",
        "SockAddrForwardedClient",
        "BackendUnexpected",
    ] {
        assert!(arrivals(absent).is_empty());
    }
    assert_sock_addr_hooks_and_cleanup(
        &records,
        target,
        SockAddrPresentation::FixedOriginalPeer.hooks(&mode),
        &run,
    );
    assert_eq!(run.status, Some(0), "{run}");
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
#[test]
#[ignore = "coordinator alone executes controlled backend work return and actual joined terminal"]
fn direct_guest_udp4_backend_completes_without_reply_before_relay_cancel() {
    cancellation_case(CancellationCase::BackendComplete);
}
#[test]
#[ignore = "coordinator alone injects one owned extra datagram and verifies actual backend error/cleanup"]
fn direct_guest_udp4_extra_datagram_is_consumed_before_backend_error_and_relay_cancel() {
    cancellation_case(CancellationCase::UnexpectedExtra);
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
        CancellationCase::BackendComplete => {
            ("sock-addr-backend-end4", "CompleteBackend", 3, "BackendComplete")
        }
        CancellationCase::UnexpectedExtra => ("sock-addr-extra4", "UnexpectedBackend", 3, "HoldReply"),
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
        CancellationCase::UpstreamReceive
        | CancellationCase::BackendComplete
        | CancellationCase::UnexpectedExtra => {
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
            match case {
                CancellationCase::UpstreamReceive | CancellationCase::UnexpectedExtra => {
                    let held = one("HeldReply");
                    assert_eq!(held["stage"], "HoldReply");
                    assert_eq!(held["step"], "Binary");
                    assert_eq!(held["receipt"], "ValidatedBackendDatagram");
                    assert_eq!(held["reply"], "NotIssued");
                    if matches!(case, CancellationCase::UnexpectedExtra) {
                        let extra = arrivals("BackendUnexpected");
                        assert_eq!(extra.len(), 1);
                        assert_eq!(extra[0]["ordinal"], 4);
                        check(
                            extra[0],
                            "Binary",
                            "5245515545535400ff",
                            &flow["upstream"],
                            &target["backend"],
                        );
                        let sent = one("FixtureExtraSend");
                        assert_eq!(sent["origin"], "TrustedFixture");
                        assert_eq!(sent["local"], flow["upstream"]);
                        assert_eq!(sent["peer"], target["backend"]);
                        assert_eq!(sent["sent"], 9);
                        assert_eq!(sent["requested_hex"], "5245515545535400ff");
                        assert_eq!(sent["forward_count"], "SeparateFromForwardCompletion");
                        assert_eq!(sent["delivery"], "RequiresIndependentBackendReceipt");
                        let returned = one("BackendWorkReturned");
                        assert_eq!(returned["terminal"], "Error");
                        assert_eq!(returned["cause"], "UnexpectedDatagram");
                        assert_eq!(returned["receipt"], "ConsumedUnexpectedDatagram");
                        assert_eq!(returned["reply"], "NotIssued");
                        assert_eq!(returned["thread_termination"], "UnobservedUntilJoin");
                        let boundary = one("BackendExtraBoundary");
                        assert_eq!(boundary["state"], "ObservedUnexpectedWorkReturn");
                        assert_eq!(boundary["thread_termination"], "UnobservedUntilJoin");
                    }
                }
                CancellationCase::BackendComplete => {
                    let returned = one("BackendWorkReturned");
                    assert_eq!(returned["terminal"], "BackendComplete");
                    assert_eq!(returned["receipt"], "ValidatedBackendDatagram");
                    assert_eq!(returned["reply"], "NotIssued");
                    assert_eq!(returned["thread_termination"], "UnobservedUntilJoin");
                    let boundary = one("BackendReturnBoundary");
                    assert_eq!(boundary["state"], "ObservedWorkReturn");
                    assert_eq!(boundary["thread_termination"], "UnobservedUntilJoin");
                    assert!(select("HeldReply").is_empty());
                }
                CancellationCase::BeforeUpstreamSend => panic!("pre-send case has no forwarded receipt"),
            }
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
        CancellationCase::BackendComplete => ("UpstreamReceive", "CompletedBackendWork"),
        CancellationCase::UnexpectedExtra => ("UpstreamReceive", "ConsumedUnexpectedBackendDatagram"),
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
        CancellationCase::BackendComplete => {
            assert!(position(backend[2]) < position(one("BackendWorkReturned")));
            assert!(position(one("BackendWorkReturned")) < position(one("BackendReturnBoundary")));
            assert!(position(one("BackendReturnBoundary")) < position(requested));
            assert!(position(one("ReceiveStageEntered")) < position(requested));
            assert!(cancelled_receive(one("ForwardCompletion")), "{run}");
        }
        CancellationCase::UnexpectedExtra => {
            assert!(position(backend[2]) < position(one("HeldReply")));
            assert!(position(one("HeldReply")) < position(one("FixtureExtraSend")));
            assert!(position(backend[2]) < position(arrivals("BackendUnexpected")[0]));
            assert!(position(arrivals("BackendUnexpected")[0]) < position(one("BackendWorkReturned")));
            assert!(position(one("BackendWorkReturned")) < position(one("BackendExtraBoundary")));
            assert!(position(one("BackendExtraBoundary")) < position(requested));
            assert!(position(one("FixtureExtraSend")) < position(requested));
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
    match case {
        CancellationCase::BackendComplete => assert!(backend_work_returned(worker), "{run}"),
        CancellationCase::UnexpectedExtra => {
            assert!(unexpected_backend_work(worker, one("UdpError")), "{run}")
        }
        _ => assert_eq!(worker["terminal"], "Cancelled"),
    }
    assert_eq!(
        worker["arrivals"],
        match case {
            CancellationCase::UnexpectedExtra => 4,
            _ => backend_expected,
        }
    );
    assert_eq!(worker["matched"], backend_expected);
    assert_eq!(worker["empty"], 1);
    assert_eq!(worker["replies_sent"], 2, "only baseline replies issued: {run}");
    if !matches!(case, CancellationCase::UnexpectedExtra) {
        assert!(arrivals("BackendUnexpected").is_empty());
        assert!(select("UdpError").is_empty());
    }
    for absent in ["ForwardUpstreamReply", "SockAddrForwardedClient"] {
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
        Some("Error" | "HookError" | "ForwardError" | "PayloadRefusal")
    )));
    assert_eq!(run.status, Some(0), "{run}");
}
