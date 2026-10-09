//! ASCII armor as go-crypto's armor.Decode reads it: the first `-----BEGIN TYPE-----`
//! line, its headers up to a blank line, then base64 lines until `-----END` or a
//! checksum line (`=XXXX`, not checked), each line trimmed and at most 96 bytes.

use crate::Error;

/// A block: its type and headers, and its body decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// bufio.NewReaderSize(in, 100) over `data`: its buffer, what of it is read (`r`) and
/// filled (`w`), and how much of `data` it has taken (`taken`), which a block read
/// leaves taken past its end (armor.Decode: "an arbitrary amount of data may have been
/// read past the end of the block").
struct Bufio<'a> {
    data: &'a [u8],
    taken: usize,
    buf: [u8; BUFFER],
    r: usize,
    w: usize,
    eof: bool,
}

const BUFFER: usize = 100;

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
    fn read_line(&mut self) -> Option<(Vec<u8>, bool)> {
        let mut s = 0;
        let (mut line, full) = loop {
            let unread = self.buf.get(self.r + s..self.w).unwrap_or_default();
            if let Some(i) = unread.iter().position(|&b| b == b'\n') {
                let end = self.r + s + i + 1;
                let line = self.buf.get(self.r..end).unwrap_or_default().to_vec();
                self.r = end;
                break (line, false);
            }
            if self.eof {
                let line = self.buf.get(self.r..self.w).unwrap_or_default().to_vec();
                self.r = self.w;
                if line.is_empty() {
                    return None;
                }
                break (line, false);
            }
            if self.w - self.r >= BUFFER {
                let line = self.buf.to_vec();
                self.r = self.w;
                break (line, true);
            }
            s = self.w - self.r;
            self.fill();
        };
        if full {
            // A '\r' ending a full buffer is left for the next line.
            if line.last() == Some(&b'\r') {
                line.pop();
                self.r -= 1;
            }
            return Some((line, true));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        Some((line, false))
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    b.get(start..end).unwrap_or_default()
}

const START: &[u8] = b"-----BEGIN ";
const END: &[u8] = b"-----END ";
const EOL: &[u8] = b"-----";

/// armor.Decode, and the block's body read whole.
pub fn decode(data: &[u8]) -> Result<Block, Error> {
    decode_from(data).map(|(block, _)| block)
}

/// armor.Decode with its body read to its end, and how much of `data` the read took:
/// where the next Decode over the same reader begins.
pub fn decode_from(data: &[u8]) -> Result<(Block, usize), Error> {
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
            let line = trim(&line);
            if line.len() > START.len() + EOL.len() && line.starts_with(START) {
                break String::from_utf8_lossy(
                    line.get(START.len()..line.len() - EOL.len()).unwrap_or_default(),
                )
                .into_owned();
            }
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut next_is_continuation = false;
        loop {
            let is_continuation = next_is_continuation;
            let (line, prefix) = lines.read_line().ok_or(Error::Eof)?;
            next_is_continuation = prefix;
            if is_continuation {
                if let Some(last) = headers.last_mut() {
                    last.1.push_str(&String::from_utf8_lossy(&line));
                }
                continue;
            }
            let line = trim(&line);
            if line.is_empty() {
                break;
            }
            let Some(i) = line.iter().position(|&b| b == b':') else {
                continue 'next_block;
            };
            let key = String::from_utf8_lossy(line.get(..i).unwrap_or_default()).into_owned();
            let value = if line.len() > i + 2 {
                String::from_utf8_lossy(line.get(i + 2..).unwrap_or_default()).into_owned()
            } else {
                String::new()
            };
            match headers.iter_mut().find(|(k, _)| *k == key) {
                Some(h) => h.1 = value,
                None => headers.push((key, value)),
            }
        }
        // The body: each line trimmed, to the end line or the checksum line.
        let mut b64 = Vec::new();
        while let Some((line, prefix)) = lines.read_line() {
            if prefix {
                return Err(Error::Structural("armor invalid".into()));
            }
            let line = trim(&line);
            if line.starts_with(END) {
                break;
            }
            if line.len() == 5 && line.first() == Some(&b'=') {
                break;
            }
            if line.len() > 96 {
                return Err(Error::Structural("armor invalid".into()));
            }
            b64.extend_from_slice(line);
        }
        let body = base64(&b64)?;
        return Ok((Block { kind, headers, body }, lines.taken));
    }
}

/// encoding/base64's StdEncoding: padded, line breaks skipped; its error the offset of
/// the first bad byte.
pub(crate) fn base64(input: &[u8]) -> Result<Vec<u8>, Error> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bad = |at: usize| Error::Other(format!("illegal base64 data at input byte {at}"));
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut i = 0;
    while i < input.len() {
        let chunk = input.get(i..).unwrap_or_default();
        let mut vals = [0u32; 4];
        let mut n = 0;
        let mut j = 0;
        while n < 4 {
            let Some(&c) = chunk.get(j) else {
                if n == 0 {
                    return Ok(out);
                }
                // decodeQuantum at the end of a short quantum: its start.
                return Err(bad(i));
            };
            if c == b'=' {
                // Padding: only after two or three symbols, and nothing after it.
                if n < 2 {
                    return Err(bad(i + j));
                }
                let need = 4 - n;
                let pads = chunk.get(j..j + need).unwrap_or_default();
                if pads.len() != need || pads.iter().any(|&p| p != b'=') {
                    return Err(bad(i + j + pads.iter().take_while(|&&p| p == b'=').count()));
                }
                if i + j + need != input.len() {
                    return Err(bad(i + j + need));
                }
                let v = vals.iter().take(n).fold(0u32, |a, &x| (a << 6) | x) << (6 * need as u32);
                let bytes = v.to_be_bytes();
                out.extend_from_slice(bytes.get(1..1 + (n - 1)).unwrap_or_default());
                return Ok(out);
            }
            let Some(v) = value(c) else {
                return Err(bad(i + j));
            };
            if let Some(slot) = vals.get_mut(n) {
                *slot = v;
            }
            n += 1;
            j += 1;
        }
        let v = vals.iter().fold(0u32, |a, &x| (a << 6) | x);
        out.extend_from_slice(v.to_be_bytes().get(1..).unwrap_or_default());
        i += j;
    }
    Ok(out)
}
