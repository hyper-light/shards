//! Git's signatures as buildx v0.37.1 reads them for build policies (D102): OpenPGP as
//! go-crypto v1.4.1 reads it (ASCII armor, openpgp/armor; packets, packet.go and
//! reader.go; signature packets, signature.go), each error of the kind go-crypto gives,
//! since what buildx makes of a signature depends on which errors it skips; and SSH
//! signatures as hiddeco/sshsig and x/crypto/ssh read them. And verified as buildx's
//! policy builtins verify them (D103): key rings, keys and detached signatures as
//! go-crypto reads and checks them, BuildKit's pgpsign and gitsign rules over that, and
//! sshsig's. Held to them by `tests/oracle.rs` and `tests/verify.rs` against
//! `scripts/gitsign/generate`.

pub mod arith;
pub mod armor;
pub mod key;
pub mod keyring;
pub mod pem;
pub mod pgpsign;
pub mod signature;
pub mod ssh;
pub mod verify;

/// go-crypto's errors, by kind: the kind decides whether a reader skips a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// errors.StructuralError: the data is malformed.
    Structural(String),
    /// errors.UnsupportedError: well formed, but not something go-crypto reads.
    Unsupported(String),
    /// errors.UnknownPacketTypeError.
    UnknownPacketType(u8),
    /// errors.CriticalUnknownPacketTypeError.
    CriticalUnknownPacketType(u8),
    /// errors.InvalidArgumentError.
    InvalidArgument(String),
    /// errors.SignatureError: a signature that does not verify.
    Signature(String),
    /// A key, a signature or an identity revoked, expired, or unknown, in go-crypto's
    /// words (ErrKeyRevoked, ErrKeyExpired, ErrSignatureExpired, ErrUnknownIssuer).
    Fixed(&'static str),
    /// io.EOF: no more input.
    Eof,
    /// io.ErrUnexpectedEOF.
    UnexpectedEof,
    /// A packet of a message (encrypted, compressed or literal data), which go-crypto
    /// reads past in a key ring as far as its header and shards refuses (D103).
    NotPorted(u8),
    /// Base64 or another error of the armor's body.
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Structural(s) => write!(f, "openpgp: invalid data: {s}"),
            Error::Unsupported(s) => write!(f, "openpgp: unsupported feature: {s}"),
            Error::UnknownPacketType(t) => write!(f, "openpgp: unknown packet type: {t}"),
            Error::CriticalUnknownPacketType(t) => {
                write!(f, "openpgp: unknown critical packet type: {t}")
            }
            Error::InvalidArgument(s) => write!(f, "openpgp: invalid argument: {s}"),
            Error::Signature(s) => write!(f, "openpgp: invalid signature: {s}"),
            Error::Fixed(s) => f.write_str(s),
            Error::Eof => f.write_str("EOF"),
            Error::UnexpectedEof => f.write_str("unexpected EOF"),
            Error::NotPorted(t) => write!(
                f,
                "openpgp: a message's packet (type {t}) where keys or signatures belong"
            ),
            Error::Other(s) => f.write_str(s),
        }
    }
}

/// A packet's body, read from the stream it came in (packet.go's readers): `Span` its
/// remaining length; `Partial` the rest of the current chunk and whether more chunks
/// follow (partialLengthReader); `Rest` to the stream's end (an old-format packet of
/// indeterminate length).
#[derive(Debug, Clone, Copy)]
enum Body {
    Span(u64),
    Partial { remaining: u64, partial: bool },
    Rest,
}

/// Bytes read in order, as an io.Reader gives them.
#[derive(Debug)]
pub struct Stream<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Stream<'a> {
    pub fn new(data: &'a [u8]) -> Stream<'a> {
        Stream { data, at: 0 }
    }

    fn rest(&self) -> &'a [u8] {
        self.data.get(self.at..).unwrap_or_default()
    }

    /// io.ReadFull of `n` bytes: EOF where none are left, UnexpectedEof where some are.
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let rest = self.rest();
        match rest.get(..n) {
            Some(b) => {
                self.at += n;
                Ok(b)
            }
            None if rest.is_empty() && n > 0 => Err(Error::Eof),
            None => {
                self.at = self.data.len();
                Err(Error::UnexpectedEof)
            }
        }
    }
}

/// A packet's contents: the stream, and how much of it is the packet's.
#[derive(Debug)]
pub struct Contents<'s, 'a> {
    stream: &'s mut Stream<'a>,
    body: Body,
}

impl Contents<'_, '_> {
    /// packet.go's readFull from the packet's reader: `n` bytes, EOF read as
    /// UnexpectedEof.
    pub fn read_full(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::with_capacity(n.min(1 << 16));
        while out.len() < n {
            let want = n - out.len();
            let got = match &mut self.body {
                Body::Rest => self.stream.take(want.min(self.stream.rest().len()))?,
                Body::Span(left) => {
                    if *left == 0 {
                        return Err(Error::UnexpectedEof);
                    }
                    let k = want.min(usize::try_from(*left).unwrap_or(usize::MAX));
                    let avail = self.stream.rest().len();
                    if avail == 0 {
                        return Err(Error::UnexpectedEof);
                    }
                    let b = self.stream.take(k.min(avail))?;
                    *left -= b.len() as u64;
                    b
                }
                Body::Partial { remaining, partial } => {
                    if *remaining == 0 {
                        if !*partial {
                            return Err(Error::UnexpectedEof);
                        }
                        let (len, more) = read_length(self.stream).map_err(|e| match e {
                            Error::Eof => Error::UnexpectedEof,
                            e => e,
                        })?;
                        *remaining = len;
                        *partial = more;
                        continue;
                    }
                    let k = want.min(usize::try_from(*remaining).unwrap_or(usize::MAX));
                    let avail = self.stream.rest().len();
                    if avail == 0 {
                        return Err(Error::UnexpectedEof);
                    }
                    let b = self.stream.take(k.min(avail))?;
                    *remaining -= b.len() as u64;
                    b
                }
            };
            if got.is_empty() {
                return Err(Error::UnexpectedEof);
            }
            out.extend_from_slice(got);
        }
        Ok(out)
    }

    /// io.ReadFull of `n` bytes from the packet's reader: EOF where it gives none.
    pub(crate) fn read_full_io(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        let at = self.stream.at;
        match self.read_full(n) {
            Err(Error::UnexpectedEof) if self.stream.at == at => Err(Error::Eof),
            r => r,
        }
    }

    /// io.ReadAll of the packet's reader: to its end, a definite length cut short
    /// UnexpectedEof.
    pub fn read_all(&mut self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        loop {
            match &mut self.body {
                Body::Rest => {
                    out.extend_from_slice(self.stream.rest());
                    self.stream.at = self.stream.data.len();
                    return Ok(out);
                }
                Body::Span(left) => {
                    let want = usize::try_from(*left).unwrap_or(usize::MAX);
                    let rest = self.stream.rest();
                    let k = want.min(rest.len());
                    out.extend_from_slice(rest.get(..k).unwrap_or_default());
                    self.stream.at += k;
                    *left -= k as u64;
                    return if *left > 0 {
                        Err(Error::UnexpectedEof)
                    } else {
                        Ok(out)
                    };
                }
                Body::Partial { remaining, partial } => {
                    let want = usize::try_from(*remaining).unwrap_or(usize::MAX);
                    let rest = self.stream.rest();
                    let k = want.min(rest.len());
                    out.extend_from_slice(rest.get(..k).unwrap_or_default());
                    self.stream.at += k;
                    *remaining -= k as u64;
                    if *remaining > 0 {
                        return Err(Error::UnexpectedEof);
                    }
                    if !*partial {
                        return Ok(out);
                    }
                    let (len, more) = read_length(self.stream).map_err(|e| match e {
                        Error::Eof => Error::UnexpectedEof,
                        e => e,
                    })?;
                    *remaining = len;
                    *partial = more;
                }
            }
        }
    }

    /// The packet's length as its header gives it, as packet.Read passes it to a padding
    /// packet: none for a partial or indeterminate length.
    pub(crate) fn declared(&self) -> Option<u64> {
        match self.body {
            Body::Span(n) => Some(n),
            _ => None,
        }
    }

    /// consumeAll: the rest of the packet, read and dropped.
    fn consume_all(&mut self) {
        loop {
            match &mut self.body {
                Body::Rest => {
                    self.stream.at = self.stream.data.len();
                    return;
                }
                Body::Span(left) => {
                    let k = usize::try_from(*left)
                        .unwrap_or(usize::MAX)
                        .min(self.stream.rest().len());
                    self.stream.at += k;
                    *left = 0;
                    return;
                }
                Body::Partial { remaining, partial } => {
                    let k = usize::try_from(*remaining)
                        .unwrap_or(usize::MAX)
                        .min(self.stream.rest().len());
                    self.stream.at += k;
                    *remaining = 0;
                    if !*partial {
                        return;
                    }
                    match read_length(self.stream) {
                        Ok((len, more)) => {
                            *remaining = len;
                            *partial = more;
                        }
                        Err(_) => return,
                    }
                }
            }
        }
    }
}

/// readLength (RFC 4880 §4.2.2): a new-format length, and whether it is partial.
fn read_length(s: &mut Stream<'_>) -> Result<(u64, bool), Error> {
    let first = *s.take(1)?.first().ok_or(Error::UnexpectedEof)?;
    Ok(match first {
        0..=191 => (u64::from(first), false),
        192..=223 => {
            let second = *s.take(1)?.first().ok_or(Error::UnexpectedEof)?;
            ((u64::from(first - 192) << 8) + u64::from(second) + 192, false)
        }
        224..=254 => (1u64 << (first & 0x1f), true),
        255 => {
            let b = s.take(4)?;
            let mut v = 0u64;
            for x in b {
                v = (v << 8) | u64::from(*x);
            }
            (v, false)
        }
    })
}

/// readHeader: a packet's tag and its contents.
fn read_header<'s, 'a>(s: &'s mut Stream<'a>) -> Result<(u8, Contents<'s, 'a>), Error> {
    let first = *s.take(1)?.first().ok_or(Error::UnexpectedEof)?;
    if first & 0x80 == 0 {
        return Err(Error::Structural("tag byte does not have MSB set".into()));
    }
    if first & 0x40 == 0 {
        let tag = (first & 0x3f) >> 2;
        let length_type = first & 3;
        if length_type == 3 {
            return Ok((
                tag,
                Contents {
                    stream: s,
                    body: Body::Rest,
                },
            ));
        }
        let n = 1usize << length_type;
        let b = s.take(n).map_err(|e| match e {
            Error::Eof => Error::UnexpectedEof,
            e => e,
        })?;
        let mut len = 0u64;
        for x in b {
            len = (len << 8) | u64::from(*x);
        }
        return Ok((
            tag,
            Contents {
                stream: s,
                body: Body::Span(len),
            },
        ));
    }
    let tag = first & 0x3f;
    let (len, partial) = read_length(s)?;
    let body = if partial {
        Body::Partial {
            remaining: len,
            partial: true,
        }
    } else {
        Body::Span(len)
    };
    Ok((tag, Contents { stream: s, body }))
}

/// A packet as this crate reads it.
#[derive(Debug, Clone)]
pub enum Packet {
    Signature(Box<signature::Signature>),
    /// A public key or subkey, or a secret one's public part.
    PublicKey(Box<key::PublicKey>),
    /// A user ID's octets, as Go's string holds them.
    UserId(Vec<u8>),
    UserAttribute,
    Padding,
}

/// packet.Read: the next packet; a packet that fails to parse is consumed whole, one
/// parsed is consumed only as far as its parse read, as go-crypto leaves the rest.
fn read(s: &mut Stream<'_>) -> Result<Option<Packet>, Error> {
    let (tag, mut contents) = read_header(s)?;
    let r = match tag {
        2 => signature::Signature::parse(&mut contents).map(|sig| Some(Packet::Signature(Box::new(sig)))),
        5 | 7 => key::PublicKey::parse_secret(&mut contents, tag == 7)
            .map(|k| Some(Packet::PublicKey(Box::new(k)))),
        6 | 14 => {
            key::PublicKey::parse(&mut contents, tag == 14).map(|k| Some(Packet::PublicKey(Box::new(k))))
        }
        13 => contents.read_all().map(|id| Some(Packet::UserId(id))),
        17 => contents
            .read_all()
            .and_then(|b| key::user_attribute(&b))
            .map(|()| Some(Packet::UserAttribute)),
        // Marker.parse: "PGP", read with io.ReadFull; Reader.Next drops it.
        10 => match contents.read_full_io(3) {
            Ok(b) if b == b"PGP" => Ok(None),
            Ok(_) => Err(Error::Structural("invalid marker packet".into())),
            Err(e) => Err(e),
        },
        // Padding.parse: its declared length, copied out.
        21 => {
            let n = contents.declared().unwrap_or(0);
            contents
                .read_full(usize::try_from(n).unwrap_or(usize::MAX))
                .map(|_| Some(Packet::Padding))
        }
        12 => Err(Error::UnknownPacketType(tag)),
        1 | 3 | 4 | 8 | 9 | 11 | 18 | 20 => Err(Error::NotPorted(tag)),
        t if t < 40 => Err(Error::CriticalUnknownPacketType(t)),
        t => Err(Error::UnknownPacketType(t)),
    };
    if r.is_err() {
        contents.consume_all();
    }
    r
}

/// packet.Reader over `data`.
#[derive(Debug)]
pub struct Reader<'a> {
    stream: Stream<'a>,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader {
            stream: Stream::new(data),
        }
    }

    /// Reader.Next: the next packet, skipping packets of unknown types, markers, and
    /// those unsupported (but for kinds that carry data). `Ok(None)` at the end.
    pub fn next_packet(&mut self) -> Result<Option<Packet>, Error> {
        loop {
            match read(&mut self.stream) {
                Ok(Some(p)) => return Ok(Some(p)),
                Ok(None) => continue,
                Err(Error::Eof) => return Ok(None),
                Err(Error::UnknownPacketType(_) | Error::Unsupported(_)) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// BuildKit's pgpsign.ParseArmoredDetachedSignature: the first signature packet of an
/// armored block, and the block's body, each error in its words.
pub fn parse_armored_detached_signature(data: &[u8]) -> Result<(signature::Signature, Vec<u8>), String> {
    let block = armor::decode(data).map_err(|e| match e {
        Error::Structural(_) | Error::Other(_) => format!("failed to read armored signature body: {e}"),
        e => format!("failed to decode armored signature: {e}"),
    })?;
    let mut reader = Reader::new(&block.body);
    loop {
        match reader.next_packet() {
            Ok(Some(Packet::Signature(sig))) => return Ok((*sig, block.body.clone())),
            Ok(Some(_)) => continue,
            Ok(None) => return Err("no signature packet found".into()),
            Err(e) => return Err(format!("failed to read next packet: {e}")),
        }
    }
}

/// What buildx tells a policy of a Git object's signature (policy/git.go
/// parseGitSignature, through BuildKit's gitsign.ParseSignature): an OpenPGP signature's
/// version and issuer, or an SSH signature's version and its key's fingerprint; none for
/// a signature it cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summary {
    None,
    Pgp { version: u8, key_id: Option<u64> },
    Ssh { version: u32, fingerprint: String },
}

pub fn summary(signature: &[u8]) -> Summary {
    if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        return match parse_armored_detached_signature(signature) {
            Ok((sig, _)) => Summary::Pgp {
                version: sig.version,
                key_id: sig.issuer_key_id,
            },
            Err(_) => Summary::None,
        };
    }
    if signature.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
        let Some(block) = pem::decode(signature).filter(|b| b.kind == "SSH SIGNATURE") else {
            return Summary::None;
        };
        return match ssh::parse_signature(&block.bytes) {
            Ok(s) => Summary::Ssh {
                version: s.version,
                fingerprint: s.public_key.fingerprint(),
            },
            Err(_) => Summary::None,
        };
    }
    Summary::None
}
