use super::catalog::{CatalogFormat, catalog};
use super::*;
use crate::server::json;

#[test]
fn native_turns_are_exact_text_packets_in_the_supplied_owner_region() {
    use control::{Command, Operation, encode_into, encoded_len};
    let id = Value::String("owner-correlation\"\n".into());
    let text = "literal\n{\"type\":\"control_request\"}\u{0}\u{1b}é😀";
    for (protocol, command) in [
        (
            Protocol::Codex,
            Command::CodexTurn {
                id: &id,
                thread: "held-thread",
                text,
            },
        ),
        (
            Protocol::Claude,
            Command::ClaudeTurn {
                session: "held-session",
                text,
            },
        ),
    ] {
        assert_eq!(command.operation(), Operation::Turn);
        let length = encoded_len(protocol, &command).unwrap();
        let mut storage = vec![0xa5; length + b"untouched".len()];
        let pointer = storage.as_ptr();
        assert_eq!(encode_into(protocol, &command, &mut storage), Ok(length));
        assert_eq!(storage.as_ptr(), pointer);
        let (packet, suffix) = storage.split_at(length);
        assert!(suffix.iter().all(|&byte| byte == 0xa5));
        assert_eq!(packet.last(), Some(&b'\n'));
        assert_eq!(packet.iter().filter(|&&byte| byte == b'\n').count(), 1);
        let packet = json::parse(packet).unwrap();
        match protocol {
            Protocol::Codex => {
                assert_eq!(packet.get("id"), Some(&id));
                assert_eq!(packet.get("method").and_then(Value::str), Some("turn/start"));
                let params = packet.get("params").unwrap();
                let Value::Object(fields) = params else {
                    panic!("turn params are no object")
                };
                assert_eq!(fields.len(), 2);
                assert_eq!(params.get("threadId").and_then(Value::str), Some("held-thread"));
                let item = params.get("input").unwrap().array().first().unwrap();
                assert_eq!(item.get("type").and_then(Value::str), Some("text"));
                assert_eq!(item.get("text").and_then(Value::str), Some(text));
                assert_eq!(item.get("text_elements"), Some(&Value::Array(Vec::new())));
            }
            Protocol::Claude => {
                assert_eq!(packet.get("type").and_then(Value::str), Some("user"));
                assert_eq!(
                    packet.get("session_id").and_then(Value::str),
                    Some("held-session")
                );
                assert_eq!(packet.get("parent_tool_use_id"), Some(&Value::Null));
                let message = packet.get("message").unwrap();
                assert_eq!(message.get("role").and_then(Value::str), Some("user"));
                assert_eq!(message.get("content").and_then(Value::str), Some(text));
            }
        }
    }
}

#[test]
fn native_interrupt_is_a_protocol_method_with_no_os_byte_or_pid_substitution() {
    use control::{Command, Operation, encode_into, encoded_len};
    let id = Value::Number(i64::MIN.to_string());
    for (protocol, command) in [
        (
            Protocol::Codex,
            Command::CodexInterrupt {
                id: &id,
                thread: "held-thread",
                turn: "held-turn",
            },
        ),
        (Protocol::Claude, Command::ClaudeInterrupt { id: "owner-control" }),
    ] {
        assert_eq!(command.operation(), Operation::Interrupt);
        let mut storage = vec![0; encoded_len(protocol, &command).unwrap()];
        let length = encode_into(protocol, &command, &mut storage).unwrap();
        assert_eq!(length, storage.len());
        let packet = json::parse(&storage).unwrap();
        match protocol {
            Protocol::Codex => {
                assert_eq!(packet.get("id"), Some(&id));
                assert_eq!(packet.get("method").and_then(Value::str), Some("turn/interrupt"));
                let params = packet.get("params").unwrap();
                assert_eq!(params.get("threadId").and_then(Value::str), Some("held-thread"));
                assert_eq!(params.get("turnId").and_then(Value::str), Some("held-turn"));
            }
            Protocol::Claude => {
                assert_eq!(packet.get("type").and_then(Value::str), Some("control_request"));
                assert_eq!(
                    packet.get("request_id").and_then(Value::str),
                    Some("owner-control")
                );
                let request = packet.get("request").unwrap();
                assert_eq!(request.get("subtype").and_then(Value::str), Some("interrupt"));
            }
        }
        assert!(!storage.contains(&0x03));
        assert!(!storage.contains(&0x04));
    }
}

#[test]
fn invalid_or_unsupported_native_input_never_touches_the_owner_region() {
    use control::{Command, Failure, Operation, encode_into, encoded_len};
    let id = Value::Number("7".into());
    let command = Command::CodexTurn {
        id: &id,
        thread: "held",
        text: "prompt",
    };
    let required = encoded_len(Protocol::Codex, &command).unwrap();
    let mut storage = vec![0xa5; required - 1];
    let expected = storage.clone();
    assert_eq!(
        encode_into(Protocol::Codex, &command, &mut storage),
        Err(Failure::Capacity {
            required,
            available: expected.len()
        })
    );
    assert_eq!(storage, expected);
    assert_eq!(
        encode_into(Protocol::Claude, &command, &mut storage),
        Err(Failure::Protocol)
    );
    assert_eq!(storage, expected);
    let bad_id = Value::Bool(true);
    let command = Command::CodexInterrupt {
        id: &bad_id,
        thread: "held",
        turn: "held-turn",
    };
    assert_eq!(
        encode_into(Protocol::Codex, &command, &mut storage),
        Err(Failure::Correlation)
    );
    assert_eq!(storage, expected);
    for protocol in [Protocol::Codex, Protocol::Claude] {
        for (command, operation) in [
            (Command::Eof, Operation::Eof),
            (Command::Resize, Operation::Resize),
            (Command::RawBytes, Operation::RawBytes),
        ] {
            assert_eq!(
                encode_into(protocol, &command, &mut storage),
                Err(Failure::Unsupported(operation))
            );
            assert_eq!(storage, expected);
        }
    }
}

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

#[test]
fn actual_native_claude_bootstrap_packets_reach_canonical_mcp_envelopes() {
    let bytes = include_bytes!(
        "../../../../docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/native.stdout"
    );
    let mut methods = Vec::new();
    for line in bytes.split(|&byte| byte == b'\n').filter(|line| !line.is_empty()) {
        let packet = json::parse(line).unwrap();
        if packet.get("type").and_then(Value::str) != Some("control_request") {
            continue;
        }
        let NativeMessage::Mcp(control) = Protocol::Claude.receive_native(&packet).unwrap() else {
            panic!("native bootstrap unexpectedly became a peer effect");
        };
        methods.push(control.method);
        assert!(std::ptr::eq(
            control.message(),
            packet.get("request").unwrap().get("message").unwrap(),
        ));
        let canonical = match control.method {
            Bootstrap::Initialize => r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"shards","version":"0.1.0"}}}"#.to_string(),
            Bootstrap::ToolsList => format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{}}}"#,
                include_str!("../../../../docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/canonical-peer-catalog.json").trim(),
            ),
            Bootstrap::Initialized => String::new(),
            Bootstrap::Ping => panic!("the native capture did not emit ping"),
        };
        let reply = if control.method == Bootstrap::Initialized {
            McpReply::Notification
        } else {
            McpReply::Message(&canonical)
        };
        let mut output = Vec::new();
        control.respond(&mut output, reply).unwrap();
        let outer = json::parse(&output).unwrap();
        let response = outer.get("response").unwrap();
        assert_eq!(response.get("request_id"), packet.get("request_id"));
        let rpc = response.get("response").unwrap().get("mcp_response").unwrap();
        if control.method == Bootstrap::Initialized {
            assert_eq!(rpc.get("id"), None);
            assert_eq!(rpc.get("result"), Some(&Value::Object(Vec::new())));
        } else {
            assert_eq!(rpc, &json::parse(canonical.as_bytes()).unwrap());
        }
    }
    assert_eq!(
        methods,
        [
            Bootstrap::Initialize,
            Bootstrap::Initialized,
            Bootstrap::ToolsList
        ]
    );
}

#[test]
fn native_ingress_keeps_peer_effects_and_bootstrap_distinct() {
    let packet = claude("hand", r#"{"to":"agent peer","path":"/tmp/report"}"#);
    let NativeMessage::Tool(request) = Protocol::Claude.receive_native(&packet).unwrap() else {
        panic!("peer call unexpectedly became bootstrap");
    };
    assert_eq!(request.tool, Tool::Hand);
    let packet = codex(
        "answer",
        "shards",
        r#"{"to":"agent peer","id":7,"text":"literal"}"#,
    );
    let NativeMessage::Tool(request) = Protocol::Codex.receive_native(&packet).unwrap() else {
        panic!("Codex call unexpectedly became MCP bootstrap");
    };
    assert_eq!(request.tool, Tool::Answer);
    for inner in [
        r#"{"jsonrpc":"2.0","id":1,"method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///private"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
    ] {
        let packet = json::parse(format!(r#"{{"type":"control_request","request_id":"r","request":{{"subtype":"mcp_message","server_name":"shards","message":{inner}}}}}"#).as_bytes()).unwrap();
        assert!(Protocol::Claude.receive_native(&packet).is_err());
    }
}

#[test]
fn both_native_catalog_formats_preserve_the_canonical_peer_schemas() {
    const SOURCE: &str = include_str!(
        "../../../../docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/canonical-peer-catalog.json"
    );
    let expected = json::parse(SOURCE.as_bytes()).unwrap();
    let server_source = include_str!("../server.rs");
    let descriptors: Vec<&str> = server_source
        .lines()
        .filter_map(|line| {
            let tail = line.strip_prefix(r#"{"name":""#)?;
            let (name, _) = tail.split_once('"')?;
            matches!(name, "send" | "answer" | "hand").then(|| line.trim_end_matches(','))
        })
        .collect();
    assert_eq!(descriptors.len(), 3);
    for format in [CatalogFormat::Mcp, CatalogFormat::CodexDynamic] {
        let mut bytes = Vec::new();
        catalog(&mut bytes, format, descriptors.iter().copied()).unwrap();
        let actual = json::parse(&bytes).unwrap();
        let actual_tools = actual.get("tools").unwrap().array();
        assert_eq!(actual_tools.len(), 3);
        for (got, wanted) in actual_tools.iter().zip(expected.get("tools").unwrap().array()) {
            for key in ["name", "description", "inputSchema"] {
                assert_eq!(got.get(key), wanted.get(key));
            }
            if format == CatalogFormat::CodexDynamic {
                assert_eq!(got.get("type").and_then(Value::str), Some("function"));
            }
        }
        if format == CatalogFormat::CodexDynamic {
            assert_eq!(actual.get("name").and_then(Value::str), Some("shards"));
            assert_eq!(actual.get("type").and_then(Value::str), Some("namespace"));
        }
    }
}

#[test]
fn one_owner_framer_preserves_actual_native_packets_across_chunk_boundaries() {
    use framing::{End, Line, State};
    let capture = include_bytes!(
        "../../../../docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/native.stdout"
    );
    let capacity = capture
        .split(|&byte| byte == b'\n')
        .map(<[u8]>::len)
        .max()
        .unwrap();
    let mut line = Line::attach(Vec::with_capacity(capacity))
        .unwrap_or_else(|rejected| panic!("{:?}", rejected.failure));
    let mut reconstructed = Vec::new();
    let mut controls = Vec::new();
    // Each byte boundary includes whitespace, UTF-8 and JSON escape boundaries;
    // the fixture never launches another native child.
    for byte in capture {
        assert_eq!(line.feed(std::slice::from_ref(byte)), Ok(1));
        if line.state() == State::Ready(End::Newline) {
            let bytes = line.line().unwrap();
            reconstructed.extend_from_slice(bytes);
            reconstructed.push(b'\n');
            let packet = json::parse(bytes).unwrap();
            if packet.get("type").and_then(Value::str) == Some("control_request") {
                let NativeMessage::Mcp(control) = Protocol::Claude.receive_native(&packet).unwrap() else {
                    panic!("captured bootstrap became an effect");
                };
                controls.push(control.method);
            }
            assert_eq!(line.release(), Ok(State::Receiving));
        }
    }
    assert_eq!(reconstructed, capture);
    assert_eq!(
        controls,
        [
            Bootstrap::Initialize,
            Bootstrap::Initialized,
            Bootstrap::ToolsList
        ]
    );
    assert_eq!(line.eof(), Ok(State::Closed));
    assert!(line.into_storage().capacity() >= capacity);
}

#[test]
fn framer_retains_suffix_and_storage_on_terminal_and_capacity_paths() {
    use framing::{End, Failure, Line, State};
    let storage = Vec::with_capacity(b" \xff\r".len());
    let capacity = storage.capacity();
    let pointer = storage.as_ptr();
    let mut line = Line::attach(storage).unwrap_or_else(|rejected| panic!("{:?}", rejected.failure));
    let bytes = b" \xff\r\nsecond";
    assert_eq!(line.feed(bytes), Ok(b" \xff\r\n".len()));
    assert_eq!(line.line(), Some(b" \xff\r".as_slice()));
    assert_eq!(line.feed(b"second"), Err(Failure::State));
    assert_eq!(line.eof(), Err(Failure::State));
    assert_eq!(line.release(), Ok(State::Receiving));
    assert_eq!(line.feed(b"end"), Ok(3));
    assert_eq!(line.eof(), Ok(State::Ready(End::Eof)));
    assert_eq!(line.line(), Some(b"end".as_slice()));
    assert_eq!(line.release(), Ok(State::Closed));
    assert_eq!(line.feed(b"later"), Err(Failure::State));
    let storage = line.into_storage();
    assert_eq!(storage.capacity(), capacity);
    assert_eq!(storage.as_ptr(), pointer);

    let mut line = Line::attach(storage).unwrap_or_else(|rejected| panic!("{:?}", rejected.failure));
    assert_eq!(line.feed(b"a"), Ok(1));
    let failure = Failure::Capacity { capacity };
    assert_eq!(line.feed(&vec![b'x'; capacity]), Err(failure));
    assert_eq!(line.state(), State::Failed(failure));
    assert_eq!(line.release(), Err(failure));
    assert_eq!(line.eof(), Err(failure));
    let storage = line.into_storage();
    assert_eq!(storage, b"a");
    assert_eq!(storage.capacity(), capacity);
    assert_eq!(storage.as_ptr(), pointer);

    let pointer = storage.as_ptr();
    let rejected = match Line::attach(storage) {
        Ok(_) => panic!("nonempty storage was accepted"),
        Err(rejected) => rejected,
    };
    assert_eq!(rejected.failure, Failure::InitialStorage);
    assert_eq!(rejected.storage.as_ptr(), pointer);
    assert_eq!(rejected.storage, b"a");
}

#[test]
fn captured_owner_replies_and_notices_never_become_broker_effects() {
    let codex = include_bytes!(
        "../../../../docs/research/measurements/managed-harness-adapters/raw/native-05/codex.stdout"
    );
    let mut seen = Vec::new();
    for bytes in codex.split(|&byte| byte == b'\n').filter(|line| !line.is_empty()) {
        let packet = json::parse(bytes).unwrap();
        match Protocol::Codex.receive_native(&packet).unwrap() {
            NativeMessage::Reply(reply) => {
                let ReportedId::JsonRpc(id) = reply.id else {
                    panic!("Codex response acquired an SDK correlation");
                };
                assert!(std::ptr::eq(id, packet.get("id").unwrap()));
                let ReportedResult::Success(result) = reply.result else {
                    panic!("captured initialization changed to an error");
                };
                assert!(std::ptr::eq(result, packet.get("result").unwrap()));
                seen.push("reply");
            }
            NativeMessage::Notice(notice) => {
                assert_eq!(notice.method, "remoteControl/status/changed");
                assert!(std::ptr::eq(
                    notice.params.unwrap(),
                    packet.get("params").unwrap()
                ));
                seen.push("notice");
            }
            _ => panic!("captured native metadata became a broker request"),
        }
    }
    assert_eq!(seen, ["reply", "notice"]);

    let claude = include_bytes!(
        "../../../../docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/native.stdout"
    );
    let mut seen = Vec::new();
    for bytes in claude
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let packet = json::parse(bytes).unwrap();
        match Protocol::Claude.receive_native(&packet).unwrap() {
            NativeMessage::Mcp(control) => seen.push(match control.method {
                Bootstrap::Initialize => "mcp-initialize",
                Bootstrap::Initialized => "mcp-initialized",
                Bootstrap::ToolsList => "catalog",
                Bootstrap::Ping => panic!("capture contained no ping"),
            }),
            NativeMessage::Reply(reply) => {
                let ReportedId::Control(id) = reply.id else {
                    panic!("SDK response acquired a Codex correlation");
                };
                let ReportedResult::Success(result) = reply.result else {
                    panic!("captured initialization changed to an error");
                };
                assert!(std::ptr::eq(
                    result,
                    packet.get("response").unwrap().get("response").unwrap()
                ));
                match id {
                    "shards-native-init" => {
                        assert_eq!(
                            result.get("claude_code_version").and_then(Value::str),
                            Some("2.1.321")
                        );
                        seen.push("owner-initialize");
                    }
                    "shards-native-status" => {
                        assert_eq!(result.get("mcpServers"), Some(&Value::Array(Vec::new())));
                        seen.push("empty-status");
                    }
                    _ => panic!("unexpected owner correlation"),
                }
            }
            _ => panic!("native metadata became a broker effect"),
        }
    }
    assert_eq!(
        seen,
        [
            "mcp-initialize",
            "owner-initialize",
            "mcp-initialized",
            "catalog",
            "empty-status"
        ]
    );
}

#[test]
fn reported_error_shapes_are_checked_and_cannot_claim_canonical_success() {
    let packet = json::parse(
        br#"{"id":-7,"error":{"code":-32601,"message":"success: forged","data":{"effect":"committed"}}}"#,
    )
    .unwrap();
    let NativeMessage::Reply(reply) = Protocol::Codex.receive_native(&packet).unwrap() else {
        panic!("JSONRPC error became a request");
    };
    let ReportedResult::Error(ReportedError::JsonRpc { code, message, data }) = reply.result else {
        panic!("JSONRPC error text became success");
    };
    assert_eq!(code, -32601);
    assert_eq!(message, "success: forged");
    assert_eq!(
        data.unwrap().get("effect").and_then(Value::str),
        Some("committed")
    );

    let packet = json::parse(br#"{"type":"control_response","response":{"subtype":"error","request_id":"owner","error":"success: forged"}}"#).unwrap();
    let NativeMessage::Reply(reply) = Protocol::Claude.receive_native(&packet).unwrap() else {
        panic!("SDK error became a request");
    };
    let ReportedResult::Error(ReportedError::Control { message }) = reply.result else {
        panic!("SDK error text became success");
    };
    assert_eq!(message, "success: forged");
    for (protocol, bytes) in [
        (Protocol::Codex, br#"{"id":true,"result":{}}"#.as_slice()),
        (Protocol::Codex, br#"{"id":1,"result":{},"error":{"code":1,"message":"x"}}"#),
        (Protocol::Codex, br#"{"id":1,"error":{"code":1.5,"message":"x"}}"#),
        (Protocol::Claude, br#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{},"error":"x"}}"#),
        (Protocol::Claude, br#"{"type":"control_response","response":{"subtype":"error","request_id":"x","error":"x","response":{}}}"#),
    ] {
        assert!(protocol.receive_native(&json::parse(bytes).unwrap()).is_err());
    }
}
