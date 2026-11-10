use super::{Completion, Envelope, Fallback, Refusal, Request, Tool, id, object, text, wire};
use crate::server::json::Value;
use std::io::{self, Write};

pub(super) fn receive(message: &Value) -> Result<Request<'_>, Refusal> {
    object(message, "message")?;
    if text(message, "type")? != "control_request" {
        return Err(Refusal::Method);
    }
    let control_id = text(message, "request_id")?;
    let request = object(
        message.get("request").ok_or(Refusal::Shape("request"))?,
        "request",
    )?;
    match text(request, "subtype")? {
        "mcp_message" => {}
        "can_use_tool" => return Err(Refusal::Fallback(Fallback::Approval)),
        "hook_callback" => return Err(Refusal::Fallback(Fallback::Hook)),
        _ => return Err(Refusal::Method),
    }
    if text(request, "server_name")? != "shards" {
        return Err(Refusal::Namespace);
    }
    let rpc = object(
        request.get("message").ok_or(Refusal::Shape("message"))?,
        "message",
    )?;
    let rpc_id = id(rpc, "id")?;
    if text(rpc, "jsonrpc")? != "2.0" || text(rpc, "method")? != "tools/call" {
        return Err(Refusal::Method);
    }
    let params = object(rpc.get("params").ok_or(Refusal::Shape("params"))?, "params")?;
    let tool = Tool::receive(text(params, "name")?)?;
    let arguments = object(
        params.get("arguments").ok_or(Refusal::Shape("arguments"))?,
        "arguments",
    )?;
    Ok(Request {
        tool,
        arguments,
        envelope: Envelope::Claude { control_id, rpc_id },
    })
}

pub(super) fn respond(
    out: &mut impl Write,
    control_id: &str,
    rpc_id: &Value,
    completion: Completion,
    result: &str,
) -> io::Result<()> {
    out.write_all(b"{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":")?;
    wire::quoted(out, control_id)?;
    out.write_all(b",\"response\":{\"mcp_response\":{\"jsonrpc\":\"2.0\",\"id\":")?;
    wire::id(out, rpc_id)?;
    out.write_all(b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":")?;
    wire::quoted(out, result)?;
    out.write_all(b"}],\"isError\":")?;
    out.write_all(if completion == Completion::Completed {
        b"false"
    } else {
        b"true"
    })?;
    out.write_all(b"}}}}}")
}
