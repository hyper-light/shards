//! gRPC's unary call (grpc/grpc PROTOCOL-HTTP2.md) over [`crate::h2`]: one length-prefixed,
//! uncompressed message each way, the call's status from its trailers, or from its
//! headers where they end the stream ("trailers-only"), its message percent-decoded and
//! its details (`grpc-status-details-bin`, a google.rpc.Status) base64-decoded.

use std::io::{Read, Write};

use crate::h2;

/// The most a message may be either way: grpcclient's MaxCallRecvMsgSize and
/// MaxCallSendMsgSize, 16 MiB.
pub const MOST: usize = 16 << 20;

/// A call's status other than OK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: u32,
    pub message: String,
    /// The google.rpc.Status the trailers carried, as protobuf.
    pub details: Vec<u8>,
}

/// Why a call has no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The connection or the stream failed under the call.
    Transport(String),
    /// The call ended with a status other than OK.
    Status(Status),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transport(e) => write!(f, "{e}"),
            // As grpc-go's status.Error says it.
            Error::Status(s) => write!(f, "rpc error: code = {} desc = {}", code_name(s.code), s.message),
        }
    }
}

impl From<h2::Error> for Error {
    fn from(e: h2::Error) -> Error {
        Error::Transport(e.to_string())
    }
}

/// A status code's name, as grpc-go's codes.Code.String says it.
pub fn code_name(code: u32) -> String {
    let name = match code {
        0 => "OK",
        1 => "Canceled",
        2 => "Unknown",
        3 => "InvalidArgument",
        4 => "DeadlineExceeded",
        5 => "NotFound",
        6 => "AlreadyExists",
        7 => "PermissionDenied",
        8 => "ResourceExhausted",
        9 => "FailedPrecondition",
        10 => "Aborted",
        11 => "OutOfRange",
        12 => "Unimplemented",
        13 => "Internal",
        14 => "Unavailable",
        15 => "DataLoss",
        16 => "Unauthenticated",
        _ => return format!("Code({code})"),
    };
    name.to_string()
}

/// `request` to `path` (`/package.Service/Method`), and its response message.
pub fn call<R: Read, W: Write>(
    conn: &mut h2::Conn<R, W>,
    path: &str,
    request: &[u8],
) -> Result<Vec<u8>, Error> {
    if request.len() > MOST {
        return Err(Error::Transport(format!(
            "trying to send message larger than max ({} vs. {MOST})",
            request.len()
        )));
    }
    let mut body = Vec::with_capacity(5 + request.len());
    body.push(0);
    body.extend_from_slice(&u32::try_from(request.len()).unwrap_or(u32::MAX).to_be_bytes());
    body.extend_from_slice(request);
    let response = conn.request(
        &[
            (":method", "POST"),
            (":scheme", "http"),
            (":path", path),
            (":authority", "localhost"),
            ("content-type", "application/grpc"),
            ("user-agent", "shards"),
            ("te", "trailers"),
        ],
        &body,
        MOST + 5,
    )?;
    let header = |fields: &[(Vec<u8>, Vec<u8>)], name: &str| {
        fields
            .iter()
            .find(|(n, _)| n == name.as_bytes())
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    };
    let status = header(&response.headers, ":status");
    if status.as_deref() != Some("200") {
        return Err(Error::Transport(format!(
            "unexpected HTTP status {}",
            status.unwrap_or_default()
        )));
    }
    if !header(&response.headers, "content-type").is_some_and(|c| c.starts_with("application/grpc")) {
        return Err(Error::Transport("a response that is not gRPC".into()));
    }
    // Trailers-only: the headers carry the status.
    let trailers = if response.trailers.is_empty() {
        &response.headers
    } else {
        &response.trailers
    };
    let code = header(trailers, "grpc-status")
        .ok_or_else(|| Error::Transport("a response without its status".into()))?
        .parse::<u32>()
        .map_err(|_| Error::Transport("a status that is no number".into()))?;
    if code != 0 {
        return Err(Error::Status(Status {
            code,
            message: percent_decode(&header(trailers, "grpc-message").unwrap_or_default()),
            details: header(trailers, "grpc-status-details-bin")
                .map(|d| base64_decode(&d))
                .transpose()
                .map_err(Error::Transport)?
                .unwrap_or_default(),
        }));
    }
    let message = response.body;
    let Some((&compressed, rest)) = message.split_first() else {
        return Err(Error::Transport("an OK response without its message".into()));
    };
    if compressed != 0 {
        return Err(Error::Transport(
            "a compressed message, which this side did not ask for".into(),
        ));
    }
    let len = rest
        .get(..4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| Error::Transport("a message shorter than its prefix".into()))?;
    let len = usize::try_from(len).map_err(|_| Error::Transport("a message too large".into()))?;
    let msg = rest.get(4..).unwrap_or_default();
    if msg.len() != len {
        return Err(Error::Transport("a response that is not one message".into()));
    }
    Ok(msg.to_vec())
}

/// grpc-message's percent-encoding undone (PROTOCOL-HTTP2.md): `%XX` a byte, the rest as
/// it is.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%'
            && let Some(hex) = b.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(hex), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Base64 (RFC 4648 §4), its padding optional, as gRPC's binary headers write it.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return Err("base64 of a length no encoding makes".into());
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = value(c).ok_or_else(|| "a byte that is not base64".to_string())?;
            n |= v << (18 - 6 * i);
        }
        let [_, a, b, c] = n.to_be_bytes();
        out.extend_from_slice([a, b, c].get(..chunk.len() - 1).unwrap_or_default());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn binary_headers_and_messages_read_as_grpc_writes_them() {
        assert_eq!(base64_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert!(base64_decode("a").is_err());
        assert!(base64_decode("a$bc").is_err());
        assert_eq!(
            percent_decode("lstat a%25b: no such file%0A"),
            "lstat a%b: no such file\n"
        );
        assert_eq!(percent_decode("100%"), "100%");
    }
}
