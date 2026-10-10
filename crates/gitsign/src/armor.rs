//! ASCII armor as go-crypto's armor.Decode reads it: the first `-----BEGIN TYPE-----`
//! line, its headers up to a blank line, then its body as io.ReadAll reads it
//! (pgpsign.ParseArmoredDetachedSignature): armor's lineReader gives the base64 lines,
//! each trimmed as bytes.TrimSpace trims it and at most 96 bytes, to `-----END` or a
//! checksum line (`=XXXX`, not checked); encoding/base64's stream decoder decodes them in
//! the chunks Go 1.26's io.ReadAll asks for. Where a chunk ends decides what the decoder
//! takes (padding may end a chunk, and decoding begins again after it) and where its
//! errors point, so the chunks are Go's.
//!
//! A key ring's body is read whole the same way, where go-crypto's ReadKeyRing reads it
//! as its packets ask, in their chunks. For a body whose base64 is corrupt, both refuse
//! it, but the error is the body's at io.ReadAll's offsets where go-crypto may report a
//! packet's error first or count from another chunk; and padding inside a line, which
//! go-crypto takes where its packet reader happens to end a chunk there, is refused.
//!
//! A private key block's body is a secret key's, so every buffer it passes through is
//! wiped when dropped, and its body ([`Body`]) grows without leaving a copy behind; other
//! blocks' bodies are public, and are not.

use zeroize::{Zeroize as _, Zeroizing};

use crate::{Error, go};

/// A block's body: a private key block's wiped when dropped, and grown without leaving a
/// copy behind (`secret`).
#[derive(Debug)]
pub struct Body {
    bytes: Vec<u8>,
    secret: bool,
}

impl std::ops::Deref for Body {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if self.secret {
            self.bytes.zeroize();
        }
    }
}

impl Body {
    /// The bytes, which drop then does not wipe: a public block's.
    pub fn into_vec(mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    /// `bytes` appended; where the body is secret and must grow, it is copied into a
    /// larger one and the old wiped.
    fn append(&mut self, bytes: &[u8]) {
        let free = self.bytes.capacity() - self.bytes.len();
        if self.secret && free < bytes.len() {
            let want = self.bytes.len().saturating_add(bytes.len());
            let mut grown = Vec::with_capacity(want.max(self.bytes.capacity().saturating_mul(2)));
            grown.extend_from_slice(&self.bytes);
            std::mem::replace(&mut self.bytes, grown).zeroize();
        }
        self.bytes.extend_from_slice(bytes);
    }
}

/// A block's type (where it is not UTF-8, with U+FFFD for what is not, so that it is
/// never a type the callers ask for, as in Go), its headers (bytes, as Go's strings;
/// each name's last value, as Go's map keeps them), and the reader its body comes from.
#[derive(Debug)]
pub struct Block<'a> {
    pub kind: String,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    lines: Bufio<'a>,
}

/// bufio.NewReaderSize(in, 100) over `data`: its buffer, what of it is read (`r`) and
/// filled (`w`), and how much of `data` it has taken (`taken`), which a block read
/// leaves taken past its end (armor.Decode: "an arbitrary amount of data may have been
/// read past the end of the block").
#[derive(Debug, Clone)]
struct Bufio<'a> {
    data: &'a [u8],
    taken: usize,
    buf: [u8; BUFFER],
    r: usize,
    w: usize,
    eof: bool,
}

const BUFFER: usize = 100;

impl Drop for Bufio<'_> {
    fn drop(&mut self) {
        self.buf.zeroize();
    }
}

impl<'a> Bufio<'a> {
    fn new(data: &'a [u8]) -> Bufio<'a> {
        Bufio {
            data,
            taken: 0,
            buf: [0; BUFFER],
            r: 0,
            w: 0,
            eof: false,
        }
    }

    /// fill: the unread bytes slid to the front, then one read of what fits.
    fn fill(&mut self) {
        if self.r > 0 {
            self.buf.copy_within(self.r..self.w, 0);
            self.w -= self.r;
            self.r = 0;
        }
        let rest = self.data.get(self.taken..).unwrap_or_default();
        if rest.is_empty() {
            self.eof = true;
            return;
        }
        let n = rest.len().min(BUFFER - self.w);
        if let (Some(dst), Some(src)) = (self.buf.get_mut(self.w..self.w + n), rest.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.w += n;
        self.taken += n;
    }

    /// ReadLine: a line without its end, and whether it filled the buffer (its rest then
    /// the next line's); None at the end of the data.
    fn read_line(&mut self) -> Option<(&[u8], bool)> {
        let mut s = 0;
        let (start, mut end, full) = loop {
            let unread = self.buf.get(self.r + s..self.w).unwrap_or_default();
            if let Some(i) = unread.iter().position(|&b| b == b'\n') {
                let start = self.r;
                self.r += s + i + 1;
                break (start, self.r, false);
            }
            if self.eof {
                let start = self.r;
                self.r = self.w;
                if start == self.w {
                    return None;
                }
                break (start, self.w, false);
            }
            if self.w - self.r >= BUFFER {
                self.r = self.w;
                break (0, BUFFER, true);
            }
            s = self.w - self.r;
            self.fill();
        };
        let ends = |end: usize, b: u8| end > start && self.buf.get(end - 1) == Some(&b);
        if full {
            // A '\r' ending a full buffer is left for the next line.
            if ends(end, b'\r') {
                end -= 1;
                self.r -= 1;
            }
        } else if ends(end, b'\n') {
            end -= 1;
            if ends(end, b'\r') {
                end -= 1;
            }
        }
        Some((self.buf.get(start..end).unwrap_or_default(), full))
    }
}

const START: &[u8] = b"-----BEGIN ";
const END: &[u8] = b"-----END ";
const EOL: &[u8] = b"-----";

fn corrupt() -> Error {
    Error::Structural("armor invalid".into())
}

/// armor.Decode: the first block's type and headers, past leading garbage; EOF where
/// there is none, or its headers are cut short.
pub fn decode(data: &[u8]) -> Result<Block<'_>, Error> {
    let mut lines = Bufio::new(data);
    'next_block: loop {
        let mut ignore_next = false;
        let kind = loop {
            let ignore_this = ignore_next;
            let (line, prefix) = lines.read_line().ok_or(Error::Eof)?;
            ignore_next = prefix;
            if ignore_next || ignore_this {
                continue;
            }
            let line = go::trim_space(line);
            if line.len() > START.len() + EOL.len() && line.starts_with(START) {
                break String::from_utf8_lossy(
                    line.get(START.len()..line.len() - EOL.len()).unwrap_or_default(),
                )
                .into_owned();
            }
        };
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut index: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
        // lastKey: the header a line too long for the buffer goes on in.
        let mut last = 0;
        let mut next_is_continuation = false;
        loop {
            let is_continuation = next_is_continuation;
            let (line, prefix) = lines.read_line().ok_or(Error::Eof)?;
            next_is_continuation = prefix;
            if is_continuation {
                if let Some(h) = headers.get_mut(last) {
                    h.1.extend_from_slice(line);
                }
                continue;
            }
            let line = go::trim_space(line);
            if line.is_empty() {
                break;
            }
            let Some(i) = line.iter().position(|&b| b == b':') else {
                continue 'next_block;
            };
            let key = line.get(..i).unwrap_or_default().to_vec();
            let value = line.get(i + 2..).unwrap_or_default().to_vec();
            last = match index.get(&key) {
                Some(&at) => {
                    if let Some(h) = headers.get_mut(at) {
                        h.1 = value;
                    }
                    at
                }
                None => {
                    index.insert(key.clone(), headers.len());
                    headers.push((key, value));
                    headers.len() - 1
                }
            };
        }
        return Ok(Block { kind, headers, lines });
    }
}

/// The capacity Go's append gives a new byte slice of `n` bytes (growslice's
/// roundupsize): the smallest size class that holds it (Go 1.26's
/// internal/runtime/gc/sizeclasses.go), or whole 8 KiB pages past them.
fn go_capacity(n: usize) -> usize {
    const CLASSES: [usize; 68] = [
        0, 8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 288, 320, 352,
        384, 416, 448, 480, 512, 576, 640, 704, 768, 896, 1024, 1152, 1280, 1408, 1536, 1792, 2048, 2304,
        2688, 3072, 3200, 3456, 4096, 4864, 5376, 6144, 6528, 6784, 6912, 8192, 9472, 9728, 10240, 10880,
        12288, 13568, 14336, 16384, 18432, 19072, 20480, 21760, 24576, 27264, 28672, 32768,
    ];
    const PAGE: usize = 8192;
    if n <= 32768 - 8 {
        return CLASSES.iter().copied().find(|&c| c >= n).unwrap_or(n);
    }
    n.div_ceil(PAGE).saturating_mul(PAGE)
}

impl Block<'_> {
    /// io.ReadAll(block.Body): the body, and how much of the data the reads took, where
    /// the next armor.Decode over the same reader begins. Go 1.26's io.ReadAll reads into
    /// a slice of 512 bytes, then, each time fewer than a sixteenth of one is left, into a
    /// new one that append makes of 256 bytes and half again each time; each read asks
    /// the decoder for what is left of the slice. A body of whole lines of symbols only
    /// is read at once ([`whole`]); any other is read in Go's chunks from its start.
    pub fn read_body(self) -> (Result<Body, Error>, usize) {
        let secret = self.kind == "PGP PRIVATE KEY BLOCK";
        let start = self.lines.clone();
        let mut lines = self.lines;
        if let Some(body) = whole(&mut lines, secret) {
            return (Ok(body), lines.taken);
        }
        let mut decoder = Decoder {
            lines: LineReader {
                input: start,
                rest: Zeroizing::new(Vec::with_capacity(96)),
                eof: false,
            },
            err: None,
            read_err: None,
            buf: [0; 1024],
            nbuf: 0,
            out: Zeroizing::new(Vec::with_capacity(1024 / 4 * 3)),
        };
        let mut body = Body {
            bytes: Vec::new(),
            secret,
        };
        let (mut cap, mut len, mut next) = (512usize, 0usize, 256usize);
        let result = loop {
            let (n, err) = decoder.read(cap.saturating_sub(len), &mut body);
            len += n;
            match err {
                Some(Error::Eof) => break Ok(body),
                Some(e) => break Err(e),
                None => {}
            }
            if cap.saturating_sub(len) < cap / 16 {
                (cap, len) = (go_capacity(next), 0);
                next = next.saturating_add(next / 2);
            }
        };
        (result, decoder.lines.input.taken)
    }
}

/// The body where its lines, to the end line or a checksum line, are no longer than
/// armor allows and hold symbols only, in whole quanta, but for padding that ends the
/// last: then each chunk of io.ReadAll's reads decodes whole, the padded quantum ends the
/// last, none is left at the end, and the body is the lines' symbols decoded in order, a
/// line's last quantum finished by the next (`carry`). None where a line holds anything
/// else (padding before the end, a carriage return, a byte that is no symbol) or is too
/// long, or a quantum is cut short: their chunks decide what Go makes of them.
fn whole(lines: &mut Bufio<'_>, secret: bool) -> Option<Body> {
    let mut body = Body {
        bytes: Vec::new(),
        secret,
    };
    let mut symbols = Zeroizing::new([0u8; 3 + 96]);
    let mut decoded = Zeroizing::new(Vec::with_capacity(symbols.len() / 4 * 3));
    let mut carry = 0;
    let mut padded = false;
    while let Some((line, prefix)) = lines.read_line() {
        if prefix {
            return None;
        }
        let line = go::trim_space(line);
        if line.starts_with(END) || (line.len() == 5 && line.first() == Some(&b'=')) {
            break;
        }
        let pads = line.iter().rev().take_while(|&&c| c == b'=').count();
        if padded && !line.is_empty() || pads > 2 || line.len() > 96 {
            return None;
        }
        if !line.get(..line.len() - pads)?.iter().all(|&c| go::is_symbol(c)) {
            return None;
        }
        padded = pads > 0;
        let held = carry + line.len();
        symbols.get_mut(carry..held)?.copy_from_slice(line);
        let quanta = held / 4 * 4;
        decoded.clear();
        go::base64_decode(symbols.get(..quanta)?, &mut decoded).ok()?;
        body.append(&decoded);
        symbols.copy_within(quanta..held, 0);
        carry = held - quanta;
    }
    (carry == 0).then_some(body)
}

/// armor's lineReader: the body's lines, each trimmed, to the end line or a checksum line;
/// what of a line did not fit the read is kept for the next (`rest`).
struct LineReader<'a> {
    input: Bufio<'a>,
    rest: Zeroizing<Vec<u8>>,
    eof: bool,
}

impl LineReader<'_> {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if self.eof {
            return (0, Some(Error::Eof));
        }
        if !self.rest.is_empty() {
            let n = p.len().min(self.rest.len());
            if let (Some(dst), Some(src)) = (p.get_mut(..n), self.rest.get(..n)) {
                dst.copy_from_slice(src);
            }
            self.rest.drain(..n);
            return (n, None);
        }
        let Some((line, prefix)) = self.input.read_line() else {
            return (0, Some(Error::Eof));
        };
        if prefix {
            return (0, Some(corrupt()));
        }
        let line = go::trim_space(line);
        if line.starts_with(END) || (line.len() == 5 && line.first() == Some(&b'=')) {
            self.eof = true;
            return (0, Some(Error::Eof));
        }
        if line.len() > 96 {
            return (0, Some(corrupt()));
        }
        let n = p.len().min(line.len());
        if let (Some(dst), Some(src)) = (p.get_mut(..n), line.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.rest.clear();
        self.rest.extend_from_slice(line.get(n..).unwrap_or_default());
        (n, None)
    }

    /// base64's newlineFilteringReader over the lines: '\r' and '\n' dropped, reading
    /// again where a read was all of them.
    fn read_filtered(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        let (mut n, mut err) = self.read(p);
        while n > 0 {
            let read = p.get_mut(..n).unwrap_or_default();
            // Most reads have no line break to drop.
            let Some(first) = read.iter().position(|&b| b == b'\r' || b == b'\n') else {
                return (n, err);
            };
            let mut kept = first;
            for i in first..read.len() {
                let Some(&b) = read.get(i) else { break };
                if b != b'\r' && b != b'\n' {
                    if let Some(slot) = read.get_mut(kept) {
                        *slot = b;
                    }
                    kept += 1;
                }
            }
            if kept > 0 {
                return (kept, err);
            }
            (n, err) = self.read(p);
        }
        (n, err)
    }
}

/// encoding/base64's stream decoder (Go 1.26): symbols in `buf` (`nbuf` of them), the
/// quanta of a read decoded into `out`, and its sticky errors. io.ReadAll's reads ask for
/// 16 bytes or more, so the symbols a read takes (`nn`, four for every three bytes it
/// asks for) decode to no more than it asks for, and none is left for the next
/// (decoder.out).
struct Decoder<'a> {
    lines: LineReader<'a>,
    err: Option<Error>,
    read_err: Option<Error>,
    buf: [u8; 1024],
    nbuf: usize,
    out: Zeroizing<Vec<u8>>,
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        self.buf.zeroize();
    }
}

impl Decoder<'_> {
    /// decoder.Read into a slice of `p` bytes, appended to `body`.
    fn read(&mut self, p: usize, body: &mut Body) -> (usize, Option<Error>) {
        if let Some(e) = &self.err {
            return (0, Some(e.clone()));
        }
        while self.nbuf < 4 && self.read_err.is_none() {
            let nn = (p / 3 * 4).min(self.buf.len());
            let Some(into) = self.buf.get_mut(self.nbuf..nn) else {
                break;
            };
            let (n, err) = self.lines.read_filtered(into);
            self.nbuf += n;
            self.read_err = err;
        }
        if self.nbuf < 4 {
            // StdEncoding pads, so a final fragment is no quantum.
            let err = match self.read_err.clone() {
                Some(Error::Eof) if self.nbuf > 0 => Some(Error::UnexpectedEof),
                e => e,
            };
            self.err.clone_from(&err);
            return (0, err);
        }
        let nr = self.nbuf / 4 * 4;
        self.out.clear();
        let decoded = go::base64_decode(self.buf.get(..nr).unwrap_or_default(), &mut self.out);
        body.append(&self.out);
        self.err = decoded
            .err()
            .map(|at| Error::Other(format!("illegal base64 data at input byte {at}")));
        self.nbuf -= nr;
        self.buf.copy_within(nr..nr + self.nbuf, 0);
        (self.out.len(), self.err.clone())
    }
}
