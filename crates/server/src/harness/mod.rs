//! Pinned native request codecs. Identity and effects remain owned by the instance.
//!
//! D-HA1 in managed-harness-adapters/README.md records the source/schema pins. These
//! packets are supplied explicitly on the tool channel, never recognized in output.

mod claude;
mod codex;
mod wire;

use crate::server::json::Value;
use std::io::{self, Write};

/// A schema choice, not an attestation of the process which sent a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    Codex,
    Claude,
}

impl Protocol {
    pub fn select(adapter: &str, version: &str) -> Result<Self, Refusal> {
        match (adapter, version) {
            ("codex", "0.167.0") => Ok(Self::Codex),
            ("claude", "2.1.321") => Ok(Self::Claude),
            ("codex" | "claude", _) => Err(Refusal::Version),
            _ => Err(Refusal::Adapter),
        }
    }

    pub fn receive(self, message: &Value) -> Result<Request<'_>, Refusal> {
        match self {
            Self::Codex => codex::receive(message),
            Self::Claude => claude::receive(message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fallback {
    Approval,
    Hook,
    LocalExecution,
    FileSystem,
    Network,
    ExternalTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Adapter,
    Version,
    Method,
    Shape(&'static str),
    Namespace,
    Fallback(Fallback),
}

impl Refusal {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Adapter => "unsupported_adapter",
            Self::Version => "unsupported_version",
            Self::Method => "unsupported_method",
            Self::Shape(_) => "invalid_shape",
            Self::Namespace => "unsupported_namespace",
            Self::Fallback(_) => "unsupported_fallback",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::Adapter | Self::Version | Self::Method | Self::Namespace => self.kind(),
            Self::Shape(field) => field,
            Self::Fallback(fallback) => match fallback {
                Fallback::Approval => "approval_is_not_execution",
                Fallback::Hook => "hook_is_not_execution",
                Fallback::LocalExecution => "local_execution_requires_owned_executor",
                Fallback::FileSystem => "filesystem_requires_owned_executor",
                Fallback::Network => "network_requires_owned_executor",
                Fallback::ExternalTool => "external_tool_requires_registered_executor",
            },
        }
    }
}

/// Only this fixed set may reenter the instance's ordinary tool dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Send,
    Answer,
    Hand,
}

impl Tool {
    fn receive(name: &str) -> Result<Self, Refusal> {
        match name {
            "send" => Ok(Self::Send),
            "answer" => Ok(Self::Answer),
            "hand" => Ok(Self::Hand),
            "run" | "exec_command" | "shell" | "Bash" | "PowerShell" => {
                Err(Refusal::Fallback(Fallback::LocalExecution))
            }
            "Read" | "Write" | "Edit" | "Glob" | "Grep" => Err(Refusal::Fallback(Fallback::FileSystem)),
            "WebFetch" | "WebSearch" => Err(Refusal::Fallback(Fallback::Network)),
            _ => Err(Refusal::Fallback(Fallback::ExternalTool)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Answer => "answer",
            Self::Hand => "hand",
        }
    }
}

pub(crate) struct Request<'a> {
    pub tool: Tool,
    pub arguments: &'a Value,
    envelope: Envelope<'a>,
}

enum Envelope<'a> {
    Codex { id: &'a Value, reported: Reported<'a> },
    Claude { control_id: &'a str, rpc_id: &'a Value },
}

/// These labels never select a principal, grant, process or outstanding broker call.
pub(crate) struct Reported<'a> {
    pub thread: &'a str,
    pub turn: &'a str,
    pub call: &'a str,
}

/// The caller obtains this status from the broker, before rendering its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    Completed,
    Refused,
    Failed,
    Uncertain,
}

impl<'packet> Request<'packet> {
    pub fn reported(&self) -> Option<&Reported<'_>> {
        match &self.envelope {
            Envelope::Codex { reported, .. } => Some(reported),
            Envelope::Claude { .. } => None,
        }
    }

    /// The result stays data, including any control-looking bytes in its text.
    /// Writing borrows both packets and performs no allocation of its own.
    pub fn respond(&self, out: &mut impl Write, completion: Completion, result: &str) -> io::Result<()> {
        match &self.envelope {
            Envelope::Codex { id, .. } => codex::respond(out, id, completion, result),
            Envelope::Claude { control_id, rpc_id } => {
                claude::respond(out, control_id, rpc_id, completion, result)
            }
        }
    }

    /// The bound must come from the canonical broker's reserved result, before
    /// its effect. JSON's longest escape is six ASCII bytes per input byte.
    pub fn reserve_reply(&self, result_bound: usize) -> Result<ReplySlot<'_, 'packet>, ReplyFailure> {
        let mut count = wire::Count(0);
        self.respond(&mut count, Completion::Refused, "")
            .map_err(|_| ReplyFailure::Length)?;
        let capacity = result_bound
            .checked_mul(b"\\u0000".len())
            .and_then(|n| n.checked_add(count.0))
            .ok_or(ReplyFailure::Length)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| ReplyFailure::Capacity)?;
        Ok(ReplySlot {
            bytes,
            result_bound,
            request: self,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyFailure {
    Length,
    Capacity,
    BoundExceeded,
    Encoding,
}

/// A prepared output reservation; filling it cannot grow its buffer.
pub(crate) struct ReplySlot<'request, 'packet> {
    bytes: Vec<u8>,
    result_bound: usize,
    request: &'request Request<'packet>,
}

impl ReplySlot<'_, '_> {
    pub fn finish(mut self, completion: Completion, result: &str) -> Result<String, ReplyFailure> {
        if result.len() > self.result_bound {
            return Err(ReplyFailure::BoundExceeded);
        }
        self.request
            .respond(&mut wire::Fixed(&mut self.bytes), completion, result)
            .map_err(|_| ReplyFailure::Encoding)?;
        String::from_utf8(self.bytes).map_err(|_| ReplyFailure::Encoding)
    }
}

fn object<'a>(value: &'a Value, field: &'static str) -> Result<&'a Value, Refusal> {
    if matches!(value, Value::Object(_)) {
        Ok(value)
    } else {
        Err(Refusal::Shape(field))
    }
}

fn text<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, Refusal> {
    value.get(field).and_then(Value::str).ok_or(Refusal::Shape(field))
}

fn id<'a>(value: &'a Value, field: &'static str) -> Result<&'a Value, Refusal> {
    match value.get(field) {
        Some(id @ Value::String(_)) => Ok(id),
        Some(id @ Value::Number(number)) if number.parse::<i64>().is_ok() => Ok(id),
        _ => Err(Refusal::Shape(field)),
    }
}

#[cfg(test)]
mod tests;
