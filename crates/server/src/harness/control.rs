//! Pinned owner-to-native protocol input. The existing input owner supplies its
//! target-checked initialized region and stages it after encoding. Nothing here
//! opens stdin, issues a right, selects an execution or allocates a pending queue.

use super::{Protocol, wire};
use crate::server::json::Value;
use std::io::{self, Write};

pub(crate) enum Command<'a> {
    CodexTurn {
        id: &'a Value,
        thread: &'a str,
        text: &'a str,
    },
    CodexInterrupt {
        id: &'a Value,
        thread: &'a str,
        turn: &'a str,
    },
    ClaudeTurn {
        session: &'a str,
        text: &'a str,
    },
    ClaudeInterrupt {
        id: &'a str,
    },
    Eof,
    Resize,
    RawBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Turn,
    Interrupt,
    Eof,
    Resize,
    RawBytes,
}

impl Command<'_> {
    /// A semantic request kind, never an input right or a broker effect result.
    pub fn operation(&self) -> Operation {
        match self {
            Self::CodexTurn { .. } | Self::ClaudeTurn { .. } => Operation::Turn,
            Self::CodexInterrupt { .. } | Self::ClaudeInterrupt { .. } => Operation::Interrupt,
            Self::Eof => Operation::Eof,
            Self::Resize => Operation::Resize,
            Self::RawBytes => Operation::RawBytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Protocol,
    Correlation,
    Unsupported(Operation),
    Length,
    Capacity { required: usize, available: usize },
    Encoding,
}

pub(crate) fn encoded_len(protocol: Protocol, command: &Command<'_>) -> Result<usize, Failure> {
    validate(protocol, command)?;
    let mut count = wire::Count(0);
    emit(&mut count, command).map_err(|_| Failure::Length)?;
    Ok(count.0)
}

/// Count the complete JSONL packet before touching the owner's region. A
/// refusal cannot leave a partial method or unescaped prompt on actor stdin.
pub(crate) fn encode_into(
    protocol: Protocol,
    command: &Command<'_>,
    storage: &mut [u8],
) -> Result<usize, Failure> {
    let required = encoded_len(protocol, command)?;
    let available = storage.len();
    let mut region = storage
        .get_mut(..required)
        .ok_or(Failure::Capacity { required, available })?;
    emit(&mut region, command).map_err(|_| Failure::Encoding)?;
    if !region.is_empty() {
        return Err(Failure::Encoding);
    }
    Ok(required)
}

fn validate(protocol: Protocol, command: &Command<'_>) -> Result<(), Failure> {
    let id = match (protocol, command) {
        (_, Command::Eof | Command::Resize | Command::RawBytes) => {
            return Err(Failure::Unsupported(command.operation()));
        }
        (Protocol::Codex, Command::CodexTurn { id, .. } | Command::CodexInterrupt { id, .. }) => id,
        (Protocol::Claude, Command::ClaudeTurn { .. } | Command::ClaudeInterrupt { .. }) => return Ok(()),
        _ => return Err(Failure::Protocol),
    };
    match id {
        Value::String(_) => Ok(()),
        Value::Number(number) if number.parse::<i64>().is_ok() => Ok(()),
        _ => Err(Failure::Correlation),
    }
}

fn emit(out: &mut impl Write, command: &Command<'_>) -> io::Result<()> {
    match command {
        Command::CodexTurn { id, thread, text } => {
            out.write_all(b"{\"id\":")?;
            wire::id(out, id)?;
            out.write_all(b",\"method\":\"turn/start\",\"params\":{\"threadId\":")?;
            wire::quoted(out, thread)?;
            out.write_all(b",\"input\":[{\"type\":\"text\",\"text\":")?;
            wire::quoted(out, text)?;
            out.write_all(b",\"text_elements\":[]}]}}\n")
        }
        Command::CodexInterrupt { id, thread, turn } => {
            out.write_all(b"{\"id\":")?;
            wire::id(out, id)?;
            out.write_all(b",\"method\":\"turn/interrupt\",\"params\":{\"threadId\":")?;
            wire::quoted(out, thread)?;
            out.write_all(b",\"turnId\":")?;
            wire::quoted(out, turn)?;
            out.write_all(b"}}\n")
        }
        Command::ClaudeTurn { session, text } => {
            out.write_all(b"{\"type\":\"user\",\"session_id\":")?;
            wire::quoted(out, session)?;
            out.write_all(b",\"message\":{\"role\":\"user\",\"content\":")?;
            wire::quoted(out, text)?;
            out.write_all(b"},\"parent_tool_use_id\":null}\n")
        }
        Command::ClaudeInterrupt { id } => {
            out.write_all(b"{\"type\":\"control_request\",\"request_id\":")?;
            wire::quoted(out, id)?;
            out.write_all(b",\"request\":{\"subtype\":\"interrupt\"}}\n")
        }
        Command::Eof | Command::Resize | Command::RawBytes => Err(io::ErrorKind::InvalidInput.into()),
    }
}
