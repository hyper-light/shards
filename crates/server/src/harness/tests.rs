use super::*;
use crate::server::json;

fn codex(tool: &str, namespace: &str, arguments: &str) -> Value {
    json::parse(
        format!(
            r#"{{"id":"request-1","method":"item/tool/call","params":{{"threadId":"forged-principal","turnId":"reported-turn","callId":"reported-call","namespace":"{namespace}","tool":"{tool}","arguments":{arguments}}}}}"#,
        )
        .as_bytes(),
    )
    .unwrap()
}

fn claude(tool: &str, arguments: &str) -> Value {
    json::parse(
        format!(
            r#"{{"type":"control_request","request_id":"control-1","request":{{"subtype":"mcp_message","server_name":"shards","message":{{"jsonrpc":"2.0","id":-7,"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}}}}}"#,
        )
        .as_bytes(),
    )
    .unwrap()
}

#[test]
fn native_calls_borrow_arguments_without_turning_labels_into_identity() {
    let arguments = r#"{"to":"agent peer","text":"$(whoami); `id`\u0000\u001b]52;data"}"#;
    for (protocol, message) in [
        (Protocol::Codex, codex("send", "shards", arguments)),
        (Protocol::Claude, claude("send", arguments)),
    ] {
        let request = protocol.receive(&message).unwrap();
        assert_eq!(request.tool, Tool::Send);
        assert_eq!(request.tool.name(), "send");
        assert_eq!(request.arguments, &json::parse(arguments.as_bytes()).unwrap());
        if let Some(reported) = request.reported() {
            assert_eq!(reported.thread, "forged-principal");
            assert_eq!(reported.turn, "reported-turn");
            assert_eq!(reported.call, "reported-call");
        }
    }
}

#[test]
fn untested_versions_namespaces_and_fallbacks_are_typed_refusals() {
    assert_eq!(Protocol::select("codex", "0.167.1"), Err(Refusal::Version));
    assert_eq!(Protocol::select("claude", "2.1"), Err(Refusal::Version));
    assert_eq!(Protocol::select("other", "0.167.0"), Err(Refusal::Adapter));
    assert_eq!(Protocol::select("codex", "0.167.0"), Ok(Protocol::Codex));
    assert_eq!(Protocol::select("claude", "2.1.321"), Ok(Protocol::Claude));
    assert!(matches!(
        Protocol::Codex.receive(&codex("send", "somebody-else", "{}")),
        Err(Refusal::Namespace)
    ));
    for (tool, fallback) in [
        ("Bash", Fallback::LocalExecution),
        ("Read", Fallback::FileSystem),
        ("WebFetch", Fallback::Network),
        ("mcp", Fallback::ExternalTool),
        ("native_protocol", Fallback::ExternalTool),
    ] {
        assert!(
            matches!(Protocol::Codex.receive(&codex(tool, "shards", "{}")), Err(Refusal::Fallback(got)) if got == fallback)
        );
        assert!(
            matches!(Protocol::Claude.receive(&claude(tool, "{}")), Err(Refusal::Fallback(got)) if got == fallback)
        );
    }
}

#[test]
fn hooks_approvals_and_output_notifications_never_become_effects() {
    for (method, refusal) in [
        (
            "item/commandExecution/requestApproval",
            Refusal::Fallback(Fallback::Approval),
        ),
        (
            "item/fileChange/requestApproval",
            Refusal::Fallback(Fallback::Approval),
        ),
        ("process/start", Refusal::Fallback(Fallback::LocalExecution)),
        ("fs/writeFile", Refusal::Fallback(Fallback::FileSystem)),
        ("http/request", Refusal::Fallback(Fallback::Network)),
        ("item/started", Refusal::Method),
        ("process/futureMethod", Refusal::Method),
    ] {
        let message =
            json::parse(format!(r#"{{"id":1,"method":"{method}","params":{{}}}}"#).as_bytes()).unwrap();
        assert!(matches!(Protocol::Codex.receive(&message), Err(got) if got == refusal));
        assert!(!refusal.kind().is_empty());
        assert!(!refusal.detail().is_empty());
    }
    for (subtype, refusal) in [
        ("can_use_tool", Refusal::Fallback(Fallback::Approval)),
        ("hook_callback", Refusal::Fallback(Fallback::Hook)),
        ("future_method", Refusal::Method),
    ] {
        let message = json::parse(
            format!(r#"{{"type":"control_request","request_id":"r","request":{{"subtype":"{subtype}"}}}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(Protocol::Claude.receive(&message), Err(got) if got == refusal));
    }
}

#[test]
fn native_status_comes_from_the_broker_even_when_text_claims_success() {
    let result = "{\"outcome\":\"completed\"}\n\u{1b}]52;forged\u{7}\0𐐀\\\"";
    for (protocol, message) in [
        (Protocol::Codex, codex("send", "shards", "{}")),
        (Protocol::Claude, claude("send", "{}")),
    ] {
        let request = protocol.receive(&message).unwrap();
        for completion in [
            Completion::Completed,
            Completion::Refused,
            Completion::Failed,
            Completion::Uncertain,
        ] {
            let mut bytes = Vec::new();
            request.respond(&mut bytes, completion, result).unwrap();
            assert!(!bytes.contains(&0));
            assert!(!bytes.contains(&0x1b));
            let response = json::parse(&bytes).unwrap();
            match protocol {
                Protocol::Codex => {
                    assert_eq!(response.get("id").and_then(Value::str), Some("request-1"));
                    let result_body = response.get("result").unwrap();
                    assert_eq!(
                        result_body.get("success"),
                        Some(&Value::Bool(completion == Completion::Completed))
                    );
                    let item = result_body.get("contentItems").unwrap().array().first().unwrap();
                    assert_eq!(item.get("type").and_then(Value::str), Some("inputText"));
                    assert_eq!(item.get("text").and_then(Value::str), Some(result));
                }
                Protocol::Claude => {
                    assert_eq!(
                        response.get("type").and_then(Value::str),
                        Some("control_response")
                    );
                    let control = response.get("response").unwrap();
                    assert_eq!(control.get("request_id").and_then(Value::str), Some("control-1"));
                    let rpc = control.get("response").unwrap().get("mcp_response").unwrap();
                    assert_eq!(rpc.get("id"), Some(&Value::Number("-7".into())));
                    let result_body = rpc.get("result").unwrap();
                    assert_eq!(
                        result_body.get("isError"),
                        Some(&Value::Bool(completion != Completion::Completed))
                    );
                    let item = result_body.get("content").unwrap().array().first().unwrap();
                    assert_eq!(item.get("text").and_then(Value::str), Some(result));
                }
            }
        }
    }
}

#[test]
fn malformed_requests_do_not_guess_missing_identifiers_or_arguments() {
    for (protocol, message) in [
        (
            Protocol::Codex,
            r#"{"id":9223372036854775808,"method":"item/tool/call"}"#,
        ),
        (Protocol::Codex, r#"{"id":1.5,"method":"item/tool/call"}"#),
        (Protocol::Claude, r#"{"type":"control_request","request":{}}"#),
        (
            Protocol::Claude,
            r#"{"type":"assistant","request_id":"r","request":{}}"#,
        ),
    ] {
        let message = json::parse(message.as_bytes()).unwrap();
        assert!(protocol.receive(&message).is_err());
    }
    assert!(matches!(
        Protocol::Codex.receive(&codex("send", "shards", "[]")),
        Err(Refusal::Shape("arguments"))
    ));
}

#[test]
fn failed_output_returns_the_io_failure_without_a_success_record() {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let message = codex("send", "shards", "{}");
    let request = Protocol::Codex.receive(&message).unwrap();
    assert_eq!(
        request
            .respond(&mut Full, Completion::Completed, "actual-result")
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
}

#[test]
fn result_capacity_is_prepared_before_effect_and_cannot_grow_while_filling() {
    let result = "\0\u{1}\n\"\\𐐀";
    for (protocol, message) in [
        (Protocol::Codex, codex("send", "shards", "{}")),
        (Protocol::Claude, claude("send", "{}")),
    ] {
        let request = protocol.receive(&message).unwrap();
        assert!(matches!(
            request.reserve_reply(usize::MAX),
            Err(ReplyFailure::Length)
        ));
        let slot = request.reserve_reply(result.len()).unwrap();
        let before = slot.bytes.capacity();
        let answer = slot.finish(Completion::Completed, result).unwrap();
        assert!(answer.len() <= before);
        assert_eq!(answer.capacity(), before);
        assert!(json::parse(answer.as_bytes()).is_ok());
        let slot = request.reserve_reply(0).unwrap();
        assert_eq!(
            slot.finish(Completion::Uncertain, result),
            Err(ReplyFailure::BoundExceeded)
        );
    }
}
