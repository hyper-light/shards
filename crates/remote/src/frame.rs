//! What a command's QUIC stream carries (D180): frames of a tag, a length (four bytes, big
//! endian) and that many bytes. From the client: the request first, then its stdin and the
//! end of it; from the daemon: the command's stdout and stderr as they come, interleaved as it
//! wrote them, and last its exit status, or why the daemon refused the request. A command's
//! control stream, a stream of its own so that no stdin waiting for room holds it up, carries
//! its terminal's size and the signals its client receives, as SSH's channel requests do
//! beside a channel's data (RFC 4254 §6.7, §6.9).

use crate::Error;

/// The most a frame carries: a pipe's reads come in less, and a request's arguments and
/// variables are a command line's (Linux's ARG_MAX is 2 MiB, execve(2)), within it.
pub const MAX_FRAME: usize = 4 << 20;

/// What a frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    /// Client: run a command ([`Request`]).
    Exec = 1,
    /// Client: join by an invitation (`crate::invite`).
    Join = 2,
    /// Client: what follows steers the command on the stream this names (eight bytes, big
    /// endian): its control stream.
    Controls = 3,
    /// Client: bytes of the command's stdin.
    Stdin = 16,
    /// Client: the end of the command's stdin.
    StdinEnd = 17,
    /// Client, on a control stream: its terminal's size now ([`size`]).
    Resize = 18,
    /// Client, on a control stream: a signal it received, by its Linux number (four bytes,
    /// big endian), the numbering a guest's signals have whatever the client's system.
    Signal = 19,
    /// Daemon: bytes the command wrote to its stdout.
    Stdout = 32,
    /// Daemon: bytes the command wrote to its stderr.
    Stderr = 33,
    /// Daemon: the command ended, with this status (four bytes, big endian).
    Exit = 34,
    /// Daemon: the request is refused, said why (UTF-8).
    Refused = 35,
    /// Daemon: the join is done: the client is enrolled under the invitation's name.
    Joined = 36,
    /// Daemon: the ID of the microVM the command made, for its client's `--cidfile`, which
    /// the client writes as Docker's CLI writes its own (D180).
    Made = 37,
}

impl Tag {
    fn of(byte: u8) -> Option<Tag> {
        Some(match byte {
            1 => Tag::Exec,
            2 => Tag::Join,
            3 => Tag::Controls,
            16 => Tag::Stdin,
            17 => Tag::StdinEnd,
            18 => Tag::Resize,
            19 => Tag::Signal,
            32 => Tag::Stdout,
            33 => Tag::Stderr,
            34 => Tag::Exit,
            35 => Tag::Refused,
            36 => Tag::Joined,
            37 => Tag::Made,
            _ => return None,
        })
    }
}

/// `payload` framed as `tag`, appended to `out`.
pub fn put(out: &mut Vec<u8>, tag: Tag, payload: &[u8]) -> Result<(), Error> {
    if payload.len() > MAX_FRAME {
        return Err(Error::new(format!(
            "a {}-byte frame, over {MAX_FRAME}",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len()).map_err(|_| Error::new("a frame too long"))?;
    out.push(tag as u8);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// What the frame at the front of `buffer` is, and how many bytes it carries, once its header
/// has come: what a frame may be is judged before its bytes are waited for.
pub fn peek(buffer: &[u8]) -> Result<Option<(Tag, usize)>, Error> {
    let Some(&[tag, a, b, c, d]) = buffer.get(..5).and_then(|h| <&[u8; 5]>::try_from(h).ok()) else {
        return Ok(None);
    };
    let tag = Tag::of(tag).ok_or_else(|| Error::new(format!("a frame of no kind ({tag})")))?;
    let len =
        usize::try_from(u32::from_be_bytes([a, b, c, d])).map_err(|_| Error::new("a frame too long"))?;
    if len > MAX_FRAME {
        return Err(Error::new(format!("a {len}-byte frame, over {MAX_FRAME}")));
    }
    Ok(Some((tag, len)))
}

/// The first whole frame at the front of `buffer`, taken from it; none until one has come
/// whole.
pub fn take(buffer: &mut Vec<u8>) -> Result<Option<(Tag, Vec<u8>)>, Error> {
    let Some((tag, len)) = peek(buffer)? else {
        return Ok(None);
    };
    let end = len.checked_add(5).ok_or_else(|| Error::new("a frame too long"))?;
    let Some(payload) = buffer.get(5..end) else {
        return Ok(None);
    };
    let payload = payload.to_vec();
    buffer.drain(..end);
    Ok(Some((tag, payload)))
}

/// A command to run: its arguments, the program's name left out, the variables of the
/// client's environment that shape what it prints, and the client's terminal, where it has
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub args: Vec<Vec<u8>>,
    pub env: Vec<(String, Vec<u8>)>,
    pub terminal: Option<Terminal>,
}

/// A client's terminal: which of its stdin, stdout and stderr it is, which the command's are
/// then of a terminal of its own, and its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub rows: u16,
    pub cols: u16,
}

/// A terminal's size as [`Tag::Resize`] carries it: rows, then columns, two bytes each, big
/// endian.
pub fn size(rows: u16, cols: u16) -> [u8; 4] {
    let ([a, b], [c, d]) = (rows.to_be_bytes(), cols.to_be_bytes());
    [a, b, c, d]
}

/// The size [`size`] put, rows then columns.
pub fn read_size(payload: &[u8]) -> Option<(u16, u16)> {
    let &[a, b, c, d] = <&[u8; 4]>::try_from(payload).ok()?;
    Some((u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d])))
}

/// The variables a request may carry: those of a client's environment that Docker's CLI
/// reads for itself, not its daemon (D180): what shapes how a command prints, the platform
/// it asks for when none is named, and the credentials it sends a registry, which a client
/// gives as `SHARDS_AUTH_CONFIG` for the registries its command reaches alone; and the paths
/// a build of a client's files is of there, which what the machine's build of its copy says
/// names in place of the copy's (`SHARDS_SYNCED_AS`, which no command is given).
pub const ENV: &[&str] = &[
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "COLUMNS",
    "LINES",
    "SHARDS_MOTION",
    "SHARDS_TIMING",
    "SHARDS_DEFAULT_PLATFORM",
    "DOCKER_DEFAULT_PLATFORM",
    "SHARDS_AUTH_CONFIG",
    "SHARDS_SYNCED_AS",
];

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    let len = u32::try_from(bytes.len()).map_err(|_| Error::new("a field too long"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn get_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let short = || Error::new("a request cut short");
    let (len, rest) = input.split_first_chunk::<4>().ok_or_else(short)?;
    let len = usize::try_from(u32::from_be_bytes(*len)).map_err(|_| short())?;
    let field = rest.get(..len).ok_or_else(short)?;
    *input = rest.get(len..).ok_or_else(short)?;
    Ok(field)
}

impl Request {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let count = |n: usize| u32::try_from(n).map_err(|_| Error::new("too many fields"));
        out.extend_from_slice(&count(self.args.len())?.to_be_bytes());
        for arg in &self.args {
            put_bytes(&mut out, arg)?;
        }
        out.extend_from_slice(&count(self.env.len())?.to_be_bytes());
        for (name, value) in &self.env {
            put_bytes(&mut out, name.as_bytes())?;
            put_bytes(&mut out, value)?;
        }
        match self.terminal {
            None => out.push(0),
            Some(t) => {
                out.push(u8::from(t.stdin) | u8::from(t.stdout) << 1 | u8::from(t.stderr) << 2);
                out.extend_from_slice(&size(t.rows, t.cols));
            }
        }
        Ok(out)
    }

    /// A request read from `payload`, its variables those [`ENV`] allows alone: a client that
    /// sends another is refused.
    pub fn decode(mut payload: &[u8]) -> Result<Request, Error> {
        let input = &mut payload;
        let count = |input: &mut &[u8]| -> Result<usize, Error> {
            let short = || Error::new("a request cut short");
            let (n, rest) = input.split_first_chunk::<4>().ok_or_else(short)?;
            *input = rest;
            usize::try_from(u32::from_be_bytes(*n)).map_err(|_| short())
        };
        let n = count(input)?;
        let mut args = Vec::with_capacity(n.min(MAX_FRAME / 4));
        for _ in 0..n {
            args.push(get_bytes(input)?.to_vec());
        }
        let n = count(input)?;
        let mut env = Vec::with_capacity(n.min(ENV.len()));
        for _ in 0..n {
            let name = String::from_utf8(get_bytes(input)?.to_vec())
                .map_err(|_| Error::new("a variable's name is not UTF-8"))?;
            if !ENV.contains(&name.as_str()) {
                return Err(Error::new(format!(
                    "{name}: no variable but {} crosses to another machine",
                    ENV.join(", ")
                )));
            }
            env.push((name, get_bytes(input)?.to_vec()));
        }
        let (&fds, rest) = input
            .split_first()
            .ok_or_else(|| Error::new("a request cut short"))?;
        *input = rest;
        let terminal = match fds {
            0 => None,
            1..=7 => {
                let (sized, rest) = input
                    .split_first_chunk::<4>()
                    .ok_or_else(|| Error::new("a request cut short"))?;
                *input = rest;
                let (rows, cols) = read_size(sized).ok_or_else(|| Error::new("a terminal's size"))?;
                Some(Terminal {
                    stdin: fds & 1 != 0,
                    stdout: fds & 2 != 0,
                    stderr: fds & 4 != 0,
                    rows,
                    cols,
                })
            }
            _ => return Err(Error::new("a terminal for more than stdin, stdout and stderr")),
        };
        if !input.is_empty() {
            return Err(Error::new("a request with bytes past its fields"));
        }
        Ok(Request { args, env, terminal })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Frames come back as they were put, whole or not at all, however the bytes arrive.
    #[test]
    fn frames_come_back_whole_however_they_arrive() {
        let mut wire = Vec::new();
        put(&mut wire, Tag::Stdout, b"out").unwrap();
        put(&mut wire, Tag::Exit, &7i32.to_be_bytes()).unwrap();
        let mut buffer = Vec::new();
        let mut got = Vec::new();
        for byte in wire {
            buffer.push(byte);
            while let Some(frame) = take(&mut buffer).unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(
            got,
            vec![
                (Tag::Stdout, b"out".to_vec()),
                (Tag::Exit, 7i32.to_be_bytes().to_vec())
            ]
        );
        assert!(buffer.is_empty());
        let mut bad = vec![99, 0, 0, 0, 0];
        assert!(take(&mut bad).is_err());
        let mut huge = vec![Tag::Stdout as u8];
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(take(&mut huge).is_err());
    }

    /// A request crosses as it was sent; a variable that would change what the command does
    /// is refused, and so is a request with bytes past its fields.
    #[test]
    fn a_request_carries_its_arguments_and_printing_variables_alone() {
        let request = Request {
            args: vec![b"ps".to_vec(), b"-a".to_vec()],
            env: vec![("COLUMNS".into(), b"120".to_vec())],
            terminal: None,
        };
        assert_eq!(Request::decode(&request.encode().unwrap()).unwrap(), request);
        let on_a_terminal = Request {
            terminal: Some(Terminal {
                stdin: true,
                stdout: false,
                stderr: true,
                rows: 50,
                cols: 300,
            }),
            ..request.clone()
        };
        assert_eq!(
            Request::decode(&on_a_terminal.encode().unwrap()).unwrap(),
            on_a_terminal
        );
        let preload = Request {
            args: vec![b"ps".to_vec()],
            env: vec![("LD_PRELOAD".into(), b"/tmp/x.so".to_vec())],
            terminal: None,
        };
        let e = Request::decode(&preload.encode().unwrap()).unwrap_err();
        assert!(e.to_string().starts_with("LD_PRELOAD: no variable but"), "{e}");
        let mut long = request.encode().unwrap();
        long.push(0);
        assert!(Request::decode(&long).is_err());
        assert!(Request::decode(&[0, 0, 0, 9]).is_err());
    }
}
