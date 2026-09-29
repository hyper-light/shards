//! The virtio-vsock packet header (virtio 1.3 §5.10.6; Linux
//! include/uapi/linux/virtio_vsock.h, `struct virtio_vsock_hdr`, packed).

/// Bytes of header before every packet's payload.
pub const HEADER_LEN: usize = 44;
/// The host's address (VMADDR_CID_HOST).
pub const HOST_CID: u64 = 2;
/// The only socket type offered: stream (VIRTIO_VSOCK_TYPE_STREAM).
pub const TYPE_STREAM: u16 = 1;
/// The largest payload a driver sends in one packet (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE).
pub const MAX_PAYLOAD: u32 = 64 * 1024;

/// Packet operations (enum virtio_vsock_op).
pub mod op {
    pub const REQUEST: u16 = 1;
    pub const RESPONSE: u16 = 2;
    pub const RST: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const RW: u16 = 5;
    pub const CREDIT_UPDATE: u16 = 6;
    pub const CREDIT_REQUEST: u16 = 7;
}

/// SHUTDOWN flags (enum virtio_vsock_shutdown).
pub mod shutdown {
    /// The sender will receive no more data.
    pub const RCV: u32 = 1;
    /// The sender will send no more data.
    pub const SEND: u32 = 2;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Header {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    /// Payload bytes after the header.
    pub len: u32,
    /// Socket type (`type` on the wire).
    pub kind: u16,
    pub op: u16,
    pub flags: u32,
    /// The sender's receive buffer for this connection, in bytes.
    pub buf_alloc: u32,
    /// Free-running count of bytes the sender has consumed from this connection.
    pub fwd_cnt: u32,
}

impl Header {
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Header {
        let mut f = Fields(bytes);
        Header {
            src_cid: u64::from_le_bytes(f.take()),
            dst_cid: u64::from_le_bytes(f.take()),
            src_port: u32::from_le_bytes(f.take()),
            dst_port: u32::from_le_bytes(f.take()),
            len: u32::from_le_bytes(f.take()),
            kind: u16::from_le_bytes(f.take()),
            op: u16::from_le_bytes(f.take()),
            flags: u32::from_le_bytes(f.take()),
            buf_alloc: u32::from_le_bytes(f.take()),
            fwd_cnt: u32::from_le_bytes(f.take()),
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        let fields: [&[u8]; 10] = [
            &self.src_cid.to_le_bytes(),
            &self.dst_cid.to_le_bytes(),
            &self.src_port.to_le_bytes(),
            &self.dst_port.to_le_bytes(),
            &self.len.to_le_bytes(),
            &self.kind.to_le_bytes(),
            &self.op.to_le_bytes(),
            &self.flags.to_le_bytes(),
            &self.buf_alloc.to_le_bytes(),
            &self.fwd_cnt.to_le_bytes(),
        ];
        for (dst, src) in out.iter_mut().zip(fields.into_iter().flatten()) {
            *dst = *src;
        }
        out
    }
}

/// Consecutive fixed-size fields of a header.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    /// The next `N` bytes. `decode` takes exactly `HEADER_LEN` bytes, so these never run
    /// short; if they did, the field would read as zero.
    fn take<const N: usize>(&mut self) -> [u8; N] {
        match self.0.split_first_chunk::<N>() {
            Some((head, rest)) => {
                self.0 = rest;
                *head
            }
            None => [0; N],
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_field_at_its_uapi_offset() {
        let h = Header {
            src_cid: 0x0102_0304_0506_0708,
            dst_cid: 2,
            src_port: 0x1122_3344,
            dst_port: 1234,
            len: 4000,
            kind: TYPE_STREAM,
            op: op::RW,
            flags: shutdown::RCV | shutdown::SEND,
            buf_alloc: 262_144,
            fwd_cnt: 0xdead_beef,
        };
        let b = h.encode();
        // Offsets of struct virtio_vsock_hdr (packed).
        assert_eq!(b[0..8], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(b[8..16], 2u64.to_le_bytes());
        assert_eq!(b[16..20], 0x1122_3344u32.to_le_bytes());
        assert_eq!(b[20..24], 1234u32.to_le_bytes());
        assert_eq!(b[24..28], 4000u32.to_le_bytes());
        assert_eq!(b[28..30], 1u16.to_le_bytes());
        assert_eq!(b[30..32], 5u16.to_le_bytes());
        assert_eq!(b[32..36], 3u32.to_le_bytes());
        assert_eq!(b[36..40], 262_144u32.to_le_bytes());
        assert_eq!(b[40..44], 0xdead_beefu32.to_le_bytes());
        assert_eq!(Header::decode(&b), h);
    }
}
