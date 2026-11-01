//! The SPIFFE provider of keyless signing, as cosign v3.1.3's gets a token
//! (pkg/providers/spiffe over go-spiffe v2.7.0): a JWT-SVID for the audience, from the
//! Workload API on the agent's socket, or on Windows its named pipe (gRPC, `FetchJWTSVID`,
//! the `workload.spiffe.io` header), its first SVID checked as jwtsvid.ParseInsecure checks
//! it. cosign dials `unix://` and its socket's path alone, which go-spiffe refuses on
//! Windows (workloadapi/addr_windows.go: `npipe:` and `tcp:` there), so that no cosign there
//! reaches a Workload API; shards reaches SPIRE's agent's pipe.

use std::io::{Read, Write};

use shards_gateway::wire::{Reader, Value, Writer};
use shards_sigstore::oidc;

/// The socket cosign asks where SPIFFE_ENDPOINT_SOCKET names none.
#[cfg(unix)]
const DEFAULT: &str = "/tmp/spire-agent/public/api.sock";

/// The pipe SPIRE's agent serves its Workload API on where SPIFFE_ENDPOINT_SOCKET names
/// none, on Windows: `\spire-agent\public\api` (spire v1.15.5,
/// cmd/spire-agent/cli/common/defaults_windows.go, DefaultNamedPipeName), on this machine
/// (pkg/common/namedpipe, AddrFromName).
#[cfg(windows)]
const DEFAULT: &str = r"\\.\pipe\spire-agent\public\api";

/// The Workload API's socket: SPIFFE_ENDPOINT_SOCKET's, as a path or (as the SPIFFE
/// Workload Endpoint spec writes it, which cosign leaves unread) a `unix://` URI.
#[cfg(unix)]
fn endpoint() -> Result<String, String> {
    Ok(match std::env::var("SPIFFE_ENDPOINT_SOCKET") {
        Ok(s) if !s.is_empty() => s.strip_prefix("unix://").unwrap_or(&s).to_string(),
        _ => DEFAULT.into(),
    })
}

/// The Workload API's named pipe on Windows: SPIFFE_ENDPOINT_SOCKET's ([`pipe_of`]), else
/// SPIRE's agent's.
#[cfg(windows)]
fn endpoint() -> Result<String, String> {
    match std::env::var("SPIFFE_ENDPOINT_SOCKET") {
        Ok(s) if !s.is_empty() => pipe_of(&s),
        _ => Ok(DEFAULT.into()),
    }
}

/// The pipe SPIFFE_ENDPOINT_SOCKET's `value` names on Windows: `npipe:NAME`, the pipe
/// `\\.\pipe\NAME`, as go-spiffe reads it there (workloadapi/addr_windows.go,
/// namedPipeTarget: its name joined to `pipe`, either slash a separator), or the pipe's own
/// path, which go-spiffe refuses. A name of `.` or `..`, which a join would move out of
/// `pipe`, is refused.
#[cfg(windows)]
fn pipe_of(value: &str) -> Result<String, String> {
    let name = value
        .strip_prefix("npipe:")
        .or_else(|| {
            value
                .replace('/', "\\")
                .get(..9)
                .filter(|p| p.eq_ignore_ascii_case(r"\\.\pipe\"))
                .and_then(|_| value.get(9..))
        })
        .ok_or_else(|| {
            format!(
                "SPIFFE_ENDPOINT_SOCKET {value}: the Workload API is reached on Windows by its named pipe, npipe:NAME or \\\\.\\pipe\\NAME"
            )
        })?;
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "." || *p == "..") {
        return Err(format!("SPIFFE_ENDPOINT_SOCKET {value}: no pipe's name"));
    }
    Ok(format!(r"\\.\pipe\{}", parts.join("\\")))
}

/// Whether the Workload API is there to ask, as cosign's provider is enabled where its
/// socket is: the socket, or on Windows the pipe ([`there`]).
pub fn reachable() -> bool {
    endpoint().is_ok_and(|at| there(&at))
}

/// Whether the socket at `path` is there.
#[cfg(unix)]
fn there(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// Whether the named pipe at `path` is there, asked without connecting to it, which would
/// take one of its instances: WaitNamedPipeW for a millisecond, which says at once where
/// no instance of it is (ERROR_FILE_NOT_FOUND), and times out (ERROR_SEM_TIMEOUT) where
/// each is in use (Microsoft's WaitNamedPipeW).
#[cfg(windows)]
fn there(path: &str) -> bool {
    use windows_sys::Win32::Foundation::ERROR_SEM_TIMEOUT;
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name.
    let free = unsafe { WaitNamedPipeW(wide.as_ptr(), 1) } != 0;
    free || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32)
}

/// A connection to the Workload API at `path`, its two halves.
#[cfg(unix)]
fn connect(path: &str) -> Result<(std::os::unix::net::UnixStream, std::os::unix::net::UnixStream), String> {
    let stream = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("{path}: {e}"))?;
    let reader = stream.try_clone().map_err(|e| e.to_string())?;
    Ok((reader, stream))
}

/// A connection to the Workload API's named pipe at `path`, its two halves, as go-winio
/// v0.6.2 dials one for go-spiffe (pipe.go, tryDialPipe): read and write; anonymous to the
/// pipe's server (SECURITY_SQOS_PRESENT, SECURITY_ANONYMOUS), which cannot impersonate this
/// process, as SPIRE's agent needs not, attesting a workload by its process (spire v1.15.5
/// pkg/common/peertracker/npipe_windows.go, GetNamedPipeClientProcessId); and, where every
/// instance is in use (ERROR_PIPE_BUSY), once one is free, as go-winio tries again until
/// its dial's context ends.
#[cfg(windows)]
fn connect(path: &str) -> Result<(std::fs::File, std::fs::File), String> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_ANONYMOUS;
    use windows_sys::Win32::System::Pipes::{NMPWAIT_WAIT_FOREVER, WaitNamedPipeW};
    let pipe = loop {
        match std::fs::File::options()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_ANONYMOUS)
            .open(path)
        {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
                // SAFETY: a NUL-terminated name.
                if unsafe { WaitNamedPipeW(wide.as_ptr(), NMPWAIT_WAIT_FOREVER) } == 0 {
                    return Err(format!("{path}: {}", std::io::Error::last_os_error()));
                }
            }
            opened => break opened.map_err(|e| format!("{path}: {e}"))?,
        }
    };
    let reader = pipe.try_clone().map_err(|e| e.to_string())?;
    Ok((reader, pipe))
}

/// JWTSVIDRequest: the audiences, no SPIFFE ID asked for.
fn request(audience: &str) -> Vec<u8> {
    let mut w = Writer::default();
    w.string(1, audience);
    w.0
}

/// The SVIDs of a JWTSVIDResponse: each one's token and hint.
fn svids(response: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for field in Reader::new(response) {
        let (number, value) = field.map_err(|e| e.0)?;
        if number != 1 {
            continue;
        }
        let Value::Bytes(svid) = value else {
            return Err("a JWT-SVID that is not a message".into());
        };
        let (mut token, mut hint) = (String::new(), String::new());
        for f in Reader::new(svid) {
            match f.map_err(|e| e.0)? {
                (2, v) => token = v.string().map_err(|e| e.0)?,
                (3, v) => hint = v.string().map_err(|e| e.0)?,
                _ => {}
            }
        }
        out.push((token, hint));
    }
    Ok(out)
}

/// spiffeid.FromString's checks of a SPIFFE ID: `spiffe://`, a trust domain of lower
/// case letters, digits, `.`, `-` and `_`, and a path of segments of letters, digits,
/// `.`, `-` and `_`, none empty, `.` or `..`.
fn spiffe_id(s: &str) -> Result<(), String> {
    let rest = s
        .strip_prefix("spiffe://")
        .ok_or("scheme is missing or invalid")?;
    let (td, path) = match rest.find('/') {
        Some(i) => (
            rest.get(..i).unwrap_or_default(),
            rest.get(i..).unwrap_or_default(),
        ),
        None => (rest, ""),
    };
    if td.is_empty() {
        return Err("trust domain is missing".into());
    }
    if !td
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err("trust domain characters are limited to lowercase letters, numbers, dots, dashes, and underscores".into());
    }
    if path.is_empty() {
        return Ok(());
    }
    for seg in path.split('/').skip(1) {
        match seg {
            "" => return Err("path cannot contain empty segments".into()),
            "." | ".." => return Err("path cannot contain dot segments".into()),
            _ => {}
        }
        if !seg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        {
            return Err(
                "path segment characters are limited to letters, numbers, dots, dashes, and underscores"
                    .into(),
            );
        }
    }
    Ok(())
}

/// jwtsvid.ParseInsecure: a JWS of an algorithm SPIFFE allows, a `typ` of JWT or JOSE
/// where it has one, a SPIFFE ID its subject, an expiry, `audience` among its audiences,
/// not expired, not before its time (go-jose's minute of leeway).
fn check(token: &str, audience: &str, now: i64) -> Result<(), String> {
    use shards_sigstore::tlog::gojson::{self, JValue};
    let fail = |e: &str| format!("jwtsvid: {e}");
    let allowed = [
        "RS256", "RS384", "RS512", "ES256", "ES384", "ES512", "PS256", "PS384", "PS512",
    ];
    let jws = oidc::parse_jws(token, &allowed).map_err(|_| fail("unable to parse JWT token"))?;
    let header = token.split('.').next().unwrap_or_default();
    let header = shards_sigstore::gobase64::decode(header.as_bytes(), true, false).unwrap_or_default();
    let header = gojson::unmarshal(&header).map_err(|_| fail("unable to parse JWT token"))?;
    if let Some(typ) = header.get("typ").and_then(JValue::as_str)
        && typ != "JWT"
        && typ != "JOSE"
    {
        return Err(fail("token header type not equal to either JWT or JOSE"));
    }
    let claims = gojson::unmarshal(&jws.payload)
        .map_err(|e| fail(&format!("unable to get claims from token: {e}")))?;
    let subject = claims.get("sub").and_then(JValue::as_str).unwrap_or_default();
    if subject.is_empty() {
        return Err(fail("token missing subject claim"));
    }
    let number = |k: &str| match claims.get(k) {
        Some(JValue::Number(n)) => n.parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    };
    let expiry = number("exp").ok_or_else(|| fail("token missing exp claim"))?;
    spiffe_id(subject).map_err(|e| fail(&format!("token has an invalid subject claim: {e}")))?;
    let audiences: Vec<String> = match claims.get("aud") {
        Some(JValue::Str(a)) => vec![a.clone()],
        Some(JValue::Array(list)) => list
            .iter()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if !audiences.iter().any(|a| a == audience) {
        let have: Vec<String> = audiences.iter().map(|a| oidc::go_quote(a)).collect();
        return Err(fail(&format!(
            "expected audience in [{}] (audience=[{}])",
            oidc::go_quote(audience),
            have.join(" ")
        )));
    }
    if now > expiry + 60 {
        return Err(fail("token has expired"));
    }
    if number("nbf").is_some_and(|nbf| now + 60 < nbf) {
        return Err("go-jose/go-jose/jwt: validation failed, token not valid yet (nbf)".into());
    }
    if number("iat").is_some_and(|iat| now + 60 < iat) {
        return Err("go-jose/go-jose/jwt: validation field, token issued in the future (iat)".into());
    }
    Ok(())
}

/// The first JWT-SVID the Workload API gives for `audience`.
pub fn jwt_svid(audience: &str) -> Result<String, String> {
    let (reader, writer) = connect(&endpoint()?)?;
    fetch(reader, writer, audience)
}

/// The first JWT-SVID the Workload API on a connection's two halves gives for `audience`.
fn fetch(reader: impl Read, writer: impl Write, audience: &str) -> Result<String, String> {
    let mut conn = shards_gateway::h2::Conn::new(reader, writer).map_err(|e| e.0)?;
    let response = shards_gateway::grpc::call_with(
        &mut conn,
        "/SpiffeWorkloadAPI/FetchJWTSVID",
        &request(audience),
        &[("workload.spiffe.io", "true")],
    )
    .map_err(|e| e.to_string())?;
    let svids = svids(&response)?;
    let (token, _) = svids.first().ok_or("there were no SVIDs in the response")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    check(token, audience, now)?;
    Ok(token.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A JWT-SVID of `claims`, signed by no one, as jwtsvid.ParseInsecure reads one.
    fn token(claims: &str) -> String {
        let b = |s: &str| oidc::base64url(s.as_bytes());
        format!(
            "{}.{}.{}",
            b(r#"{"alg":"ES256","typ":"JWT"}"#),
            b(claims),
            b("sig")
        )
    }

    /// The Workload API on a connection's two halves, answering one FetchJWTSVID with an
    /// SVID of `token` as a gRPC server does: once the call's message has come (its DATA
    /// frame's END_STREAM), its settings, the response's headers, its message and its
    /// status; then whatever the client says, to its end, so that nothing it reads is cut.
    fn workload_api(mut r: impl Read, mut w: impl Write, token: &str) {
        let mut preface = [0u8; 24];
        r.read_exact(&mut preface).unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        loop {
            let mut head = [0u8; 9];
            r.read_exact(&mut head).unwrap();
            let len = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
            r.read_exact(&mut vec![0; len]).unwrap();
            if head[3] == 0 && head[4] & 1 != 0 {
                break;
            }
        }
        let mut svid = Writer::default();
        svid.string(1, "spiffe://example.org/signer");
        svid.string(2, token);
        let mut response = Writer::default();
        response.message(1, &svid.0);
        let mut message = vec![0];
        message.extend_from_slice(&u32::try_from(response.0.len()).unwrap().to_be_bytes());
        message.extend_from_slice(&response.0);
        let mut out = Vec::new();
        let mut frame = |kind: u8, flags: u8, stream: u32, payload: &[u8]| {
            out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes()[1..]);
            out.extend_from_slice(&[kind, flags]);
            out.extend_from_slice(&stream.to_be_bytes());
            out.extend_from_slice(payload);
        };
        let encode = shards_gateway::hpack::encode;
        frame(4, 0, 0, &[]);
        frame(
            1,
            4,
            1,
            &encode(&[(":status", "200"), ("content-type", "application/grpc")]),
        );
        frame(0, 0, 1, &message);
        frame(1, 4 | 1, 1, &encode(&[("grpc-status", "0")]));
        w.write_all(&out).unwrap();
        w.flush().unwrap();
        let _ = r.read_to_end(&mut Vec::new());
    }

    /// Do: a Workload API on a socket asked for a JWT-SVID. Expect its SVID's token.
    #[cfg(unix)]
    #[test]
    fn a_jwt_svid_is_asked_of_the_workload_api_on_its_socket() {
        let dir = shards_testdir::TempDir::new("spiffe").unwrap();
        let path = dir.join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let svid = token(r#"{"sub":"spiffe://example.org/signer","aud":["sigstore"],"exp":4000000000}"#);
        let given = svid.clone();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            workload_api(s.try_clone().unwrap(), s, &given);
        });
        let path = path.to_str().unwrap();
        assert!(there(path));
        let (r, w) = connect(path).unwrap();
        assert_eq!(fetch(r, w, "sigstore").unwrap(), svid);
        server.join().unwrap();
    }

    /// Do: a Workload API on a named pipe of this machine, as SPIRE's agent serves it on
    /// Windows, asked whether it is there and for a JWT-SVID; then a pipe no one serves.
    /// Expect it there, as it was before it was asked and with no connection of the ask's,
    /// its SVID's token, and the other pipe not there.
    #[cfg(windows)]
    #[test]
    fn a_jwt_svid_is_asked_of_the_workload_api_on_its_named_pipe() {
        use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_WAIT,
        };
        let path = format!(r"\\.\pipe\shards-tests-spiffe-{}", std::process::id());
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated name; one instance, of bytes, waiting; no attributes.
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(pipe, INVALID_HANDLE_VALUE);
        // SAFETY: the pipe just made, owned here alone.
        let pipe = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(pipe) });
        assert!(there(&path));
        let svid = token(r#"{"sub":"spiffe://example.org/signer","aud":["sigstore"],"exp":4000000000}"#);
        let given = svid.clone();
        let server = std::thread::spawn(move || {
            use std::os::windows::io::AsRawHandle as _;
            // SAFETY: the pipe's handle, waited on for its one client, no overlap.
            if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } == 0 {
                // Its client came first.
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(ERROR_PIPE_CONNECTED as i32)
                );
            }
            workload_api(pipe.try_clone().unwrap(), pipe, &given);
        });
        let (r, w) = connect(&path).unwrap();
        assert_eq!(fetch(r, w, "sigstore").unwrap(), svid);
        server.join().unwrap();
        assert!(!there(&format!(r"{path}-none")));
    }

    #[cfg(windows)]
    #[test]
    fn a_named_pipe_is_named_as_go_spiffe_names_it() {
        let pipe = r"\\.\pipe\spire-agent\public\api";
        for value in [
            r"npipe:spire-agent\public\api",
            r"npipe:\spire-agent\public\api",
            "npipe:spire-agent/public/api",
            r"\\.\pipe\spire-agent\public\api",
            r"\\.\PIPE\spire-agent\public\api",
            "//./pipe/spire-agent/public/api",
        ] {
            assert_eq!(pipe_of(value).unwrap(), pipe, "{value}");
        }
        for value in [
            "unix:///tmp/spire-agent/public/api.sock",
            "/tmp/spire-agent/public/api.sock",
            "npipe:",
            r"npipe:..\x",
            r"\\.\pipe\a\.\b",
        ] {
            assert!(pipe_of(value).is_err(), "{value}");
        }
    }

    #[test]
    fn spiffe_ids_are_checked_as_go_spiffe_checks_them() {
        spiffe_id("spiffe://example.org/ns/default/sa/signer").unwrap();
        spiffe_id("spiffe://example.org").unwrap();
        assert_eq!(
            spiffe_id("https://example.org/x").unwrap_err(),
            "scheme is missing or invalid"
        );
        assert!(spiffe_id("spiffe://Example.org/x").is_err());
        assert_eq!(
            spiffe_id("spiffe://example.org//x").unwrap_err(),
            "path cannot contain empty segments"
        );
        assert_eq!(
            spiffe_id("spiffe://example.org/x/..").unwrap_err(),
            "path cannot contain dot segments"
        );
    }

    #[test]
    fn the_request_and_response_are_the_workload_api_s() {
        assert_eq!(request("sigstore"), b"\x0a\x08sigstore");
        let mut svid = Writer::default();
        svid.string(1, "spiffe://example.org/x");
        svid.string(2, "a.b.c");
        svid.string(3, "internal");
        let mut resp = Writer::default();
        resp.message(1, &svid.0);
        assert_eq!(
            svids(&resp.0).unwrap(),
            [("a.b.c".to_string(), "internal".to_string())]
        );
    }

    #[test]
    fn a_jwt_svid_names_a_spiffe_id_and_the_audience() {
        let ok = token(r#"{"sub":"spiffe://example.org/signer","aud":["sigstore"],"exp":2000000000}"#);
        check(&ok, "sigstore", 1_900_000_000).unwrap();
        assert_eq!(
            check(&ok, "other", 1_900_000_000).unwrap_err(),
            "jwtsvid: expected audience in [\"other\"] (audience=[\"sigstore\"])"
        );
        assert_eq!(
            check(&ok, "sigstore", 2_000_000_100).unwrap_err(),
            "jwtsvid: token has expired"
        );
        let no_exp = token(r#"{"sub":"spiffe://example.org/signer","aud":"sigstore"}"#);
        assert_eq!(
            check(&no_exp, "sigstore", 0).unwrap_err(),
            "jwtsvid: token missing exp claim"
        );
        let bad_sub = token(r#"{"sub":"user@example.org","aud":"sigstore","exp":2000000000}"#);
        assert!(
            check(&bad_sub, "sigstore", 0)
                .unwrap_err()
                .starts_with("jwtsvid: token has an invalid subject claim")
        );
    }
}
