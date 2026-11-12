//! What the controller says on each stream. The same bytes reach the guest in both arms: a
//! QUIC stream that ends at the guest's endpoint (arm g), or one the host's endpoint carries
//! on, header and all, over a vsock stream of its own (arm h).

/// A stream's first bytes: the microVM it is for (4, big-endian), its operation (1) and the
/// operation's count (8, big-endian).
pub const HEADER: usize = 13;

/// Every byte comes back until the stream ends: the round trip.
pub const ECHO: u8 = b'E';
/// The count of bytes follows, then their count comes back (8 bytes): toward the guest.
pub const UPLOAD: u8 = b'U';
/// The count of bytes comes back: from the guest.
pub const DOWNLOAD: u8 = b'D';
/// One line of what the guest's program holds and has used comes back.
pub const STATS: u8 = b'S';

/// The most taken or given in one turn: one vsock packet,
/// `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE` (Linux 6.18 include/linux/virtio_vsock.h), which the guest's
/// transport splits every send into, as managed-engine-relay-throughput-v1 derived for
/// Shards' own relay (r332: throughput flat past it).
pub const CHUNK: usize = 64 * 1024;

/// The guest's vsock port in arm h.
pub const VSOCK_PORT: u32 = 5000;

/// The guest's UDP port in arm g.
pub const QUIC_PORT: u16 = 4433;

/// Zeros, the bytes every upload and download carries.
pub static ZEROS: [u8; CHUNK] = [0; CHUNK];

/// A stream's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub vm: u32,
    pub op: u8,
    pub count: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER] {
        let mut out = [0u8; HEADER];
        let vm = self.vm.to_be_bytes();
        let count = self.count.to_be_bytes();
        for (slot, byte) in out
            .iter_mut()
            .zip(vm.iter().chain(std::iter::once(&self.op)).chain(count.iter()))
        {
            *slot = *byte;
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Header, String> {
        let (vm, rest) = bytes
            .split_first_chunk::<4>()
            .ok_or_else(|| format!("a header of {} bytes", bytes.len()))?;
        let (op, rest) = rest
            .split_first()
            .ok_or_else(|| format!("a header of {} bytes", bytes.len()))?;
        let count = rest
            .first_chunk::<8>()
            .ok_or_else(|| format!("a header of {} bytes", bytes.len()))?;
        if ![ECHO, UPLOAD, DOWNLOAD, STATS].contains(op) {
            return Err(format!("operation {op:#04x}"));
        }
        Ok(Header {
            vm: u32::from_be_bytes(*vm),
            op: *op,
            count: u64::from_be_bytes(*count),
        })
    }
}

/// `bytes` as lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A key's SHA-256 from its hexadecimal.
pub fn fingerprint(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{text:?} is not 64 hexadecimal digits"));
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
        *slot = u8::from_str_radix(pair, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// `--name value` options, every one valued: those in `known`, refused otherwise.
#[derive(Debug, Default)]
pub struct Opts(Vec<(String, String)>);

impl Opts {
    pub fn parse(args: &[String], known: &[&str]) -> Result<Opts, String> {
        let mut out = Vec::new();
        let mut words = args.iter();
        while let Some(word) = words.next() {
            let name = word
                .strip_prefix("--")
                .filter(|n| known.contains(n))
                .ok_or_else(|| format!("unknown option {word:?}; options: --{}", known.join(" --")))?;
            let value = words.next().ok_or_else(|| format!("{word} needs a value"))?;
            out.push((name.to_string(), value.clone()));
        }
        Ok(Opts(out))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find_map(|(n, v)| (n == name).then_some(v.as_str()))
    }

    pub fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("--{name} is needed"))
    }

    pub fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| format!("--{name} {v:?} is not a number")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_header_reads_back_as_written() {
        let h = Header {
            vm: 0x0102_0304,
            op: UPLOAD,
            count: 1 << 40,
        };
        assert_eq!(Header::decode(&h.encode()).unwrap(), h);
        assert!(Header::decode(&h.encode()[..12]).is_err());
        let mut bad = h.encode();
        bad[4] = b'Z';
        assert!(Header::decode(&bad).is_err());
    }

    #[test]
    fn a_fingerprint_reads_back_from_its_hex() {
        let fp: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
        assert_eq!(fingerprint(&hex(&fp)).unwrap(), fp);
        assert!(fingerprint("00").is_err());
    }
}
