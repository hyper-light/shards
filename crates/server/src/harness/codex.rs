use super::{
    Completion, Envelope, Fallback, NativeMessage, Refusal, Reported, ReportedError, ReportedId,
    ReportedNotice, ReportedReply, ReportedResult, Request, Tool, id, object, text, wire,
};
use crate::server::json::Value;
use std::io::{self, Write};

pub(super) fn receive_native(message: &Value) -> Result<NativeMessage<'_>, Refusal> {
    object(message, "message")?;
    if message.get("method").is_some() {
        if message.get("id").is_some() {
            return receive(message).map(NativeMessage::Tool);
        }
        if message.get("result").is_some() || message.get("error").is_some() {
            return Err(Refusal::Shape("notification"));
        }
        return Ok(NativeMessage::Notice(ReportedNotice {
            method: text(message, "method")?,
            params: message.get("params"),
        }));
    }
    let id = ReportedId::JsonRpc(id(message, "id")?);
    let result = match (message.get("result"), message.get("error")) {
        (Some(result), None) => ReportedResult::Success(result),
        (None, Some(error)) => {
            object(error, "error")?;
            let code = error
                .get("code")
                .and_then(|value| match value {
                    Value::Number(number) => number.parse::<i64>().ok(),
                    _ => None,
                })
                .ok_or(Refusal::Shape("code"))?;
            ReportedResult::Error(ReportedError::JsonRpc {
                code,
                message: text(error, "message")?,
                data: error.get("data"),
            })
        }
        _ => return Err(Refusal::Shape("response")),
    };
    Ok(NativeMessage::Reply(ReportedReply { id, result }))
}

pub(super) fn receive(message: &Value) -> Result<Request<'_>, Refusal> {
    object(message, "message")?;
    let id = id(message, "id")?;
    match text(message, "method")? {
        "item/tool/call" => {}
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            return Err(Refusal::Fallback(Fallback::Approval));
        }
        "process/start" | "process/read" | "process/write" | "process/signal" | "process/terminate" => {
            return Err(Refusal::Fallback(Fallback::LocalExecution));
        }
        "fs/readFile"
        | "fs/open"
        | "fs/readBlock"
        | "fs/writeBlock"
        | "fs/close"
        | "fs/writeFile"
        | "fs/createDirectory"
        | "fs/getMetadata"
        | "fs/watchMissing"
        | "fs/canonicalize"
        | "fs/readDirectory"
        | "fs/walk"
        | "fs/remove"
        | "fs/copy"
        | "capabilityRoots/discoverV1"
        | "capabilities/discoverV2" => {
            return Err(Refusal::Fallback(Fallback::FileSystem));
        }
        "http/request" => return Err(Refusal::Fallback(Fallback::Network)),
        _ => return Err(Refusal::Method),
    }
    let params = object(message.get("params").ok_or(Refusal::Shape("params"))?, "params")?;
    match params.get("namespace") {
        Some(Value::String(namespace)) if namespace == "shards" => {}
        None | Some(Value::Null | Value::String(_)) => return Err(Refusal::Namespace),
        _ => return Err(Refusal::Shape("namespace")),
    }
    let tool = Tool::receive(text(params, "tool")?)?;
    let arguments = object(
        params.get("arguments").ok_or(Refusal::Shape("arguments"))?,
        "arguments",
    )?;
    let reported = Reported {
        thread: text(params, "threadId")?,
        turn: text(params, "turnId")?,
        call: text(params, "callId")?,
    };
    Ok(Request {
        tool,
        arguments,
        envelope: Envelope::Codex { id, reported },
    })
}

pub(super) fn respond(
    out: &mut impl Write,
    id: &Value,
    completion: Completion,
    result: &str,
) -> io::Result<()> {
    out.write_all(b"{\"id\":")?;
    wire::id(out, id)?;
    out.write_all(b",\"result\":{\"contentItems\":[{\"type\":\"inputText\",\"text\":")?;
    wire::quoted(out, result)?;
    out.write_all(b"}],\"success\":")?;
    out.write_all(if completion == Completion::Completed {
        b"true"
    } else {
        b"false"
    })?;
    out.write_all(b"}}")
}
