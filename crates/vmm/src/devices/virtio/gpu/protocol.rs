//! virtio-gpu's control queue as its driver speaks it (virtio 1.3 §5.7.6; Linux
//! include/uapi/linux/virtio_gpu.h): a request is a header and its command's fields, little
//! endian, over any number of the chain's readable buffers; an answer is a header and its
//! fields, over its writable ones. Everything here comes from the guest: lengths and
//! counts are checked against what the request holds before anything is taken from it.

use crate::devices::virtio::queue::Chain;
use crate::memory::Access;

/// Commands (`enum virtio_gpu_ctrl_type`).
pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const RESOURCE_CREATE_BLOB: u32 = 0x010c;
    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const SUBMIT_3D: u32 = 0x0207;
    pub const RESOURCE_MAP_BLOB: u32 = 0x0208;
    pub const RESOURCE_UNMAP_BLOB: u32 = 0x0209;
}

/// Answers.
pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
    pub const OK_MAP_INFO: u32 = 0x1106;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

/// A header's flags.
pub const FLAG_FENCE: u32 = 1 << 0;
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

/// `struct virtio_gpu_ctrl_hdr`'s bytes.
pub const HEADER: usize = 24;

/// What every request and answer begins with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Header {
    pub kind: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl Header {
    fn parse(b: &[u8]) -> Option<Header> {
        Some(Header {
            kind: le32(b, 0)?,
            flags: le32(b, 4)?,
            fence_id: le64(b, 8)?,
            ctx_id: le32(b, 16)?,
            ring_idx: *b.get(20)?,
        })
    }

    fn bytes(&self) -> [u8; HEADER] {
        let mut h = [0u8; HEADER];
        put(&mut h, 0, &self.kind.to_le_bytes());
        put(&mut h, 4, &self.flags.to_le_bytes());
        put(&mut h, 8, &self.fence_id.to_le_bytes());
        put(&mut h, 16, &self.ctx_id.to_le_bytes());
        put(&mut h, 20, &[self.ring_idx]);
        h
    }

    /// Whether the request asks for a fence, and on which of its context's rings.
    pub fn fence(&self) -> Option<(u64, Option<u8>)> {
        (self.flags & FLAG_FENCE != 0).then(|| {
            let ring = (self.flags & FLAG_INFO_RING_IDX != 0).then_some(self.ring_idx);
            (self.fence_id, ring)
        })
    }
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

pub(super) fn put(b: &mut [u8], at: usize, v: &[u8]) {
    if let Some(dst) = at.checked_add(v.len()).and_then(|end| b.get_mut(at..end)) {
        dst.copy_from_slice(v);
    }
}

/// A guest buffer a resource's backing is made of (`struct virtio_gpu_mem_entry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub addr: u64,
    pub len: u32,
}

/// A request's command, its fields taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    GetDisplayInfo,
    GetCapsetInfo {
        index: u32,
    },
    GetCapset {
        id: u32,
        version: u32,
    },
    CtxCreate {
        context_init: u32,
        name: Vec<u8>,
    },
    CtxDestroy,
    CtxAttachResource {
        resource: u32,
    },
    CtxDetachResource {
        resource: u32,
    },
    ResourceCreateBlob {
        resource: u32,
        mem: u32,
        flags: u32,
        id: u64,
        size: u64,
        entries: Vec<Entry>,
    },
    ResourceUnref {
        resource: u32,
    },
    AttachBacking {
        resource: u32,
        entries: Vec<Entry>,
    },
    DetachBacking {
        resource: u32,
    },
    Submit3d {
        commands: Vec<u8>,
    },
    MapBlob {
        resource: u32,
        offset: u64,
    },
    UnmapBlob {
        resource: u32,
    },
    /// A kind this device does not serve: answered ERR_UNSPEC.
    Unsupported(u32),
}

/// Why a request could not be taken: answered with `.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub u32);

/// The `count` entries at `at` of `b`, each 16 bytes: no more than `b` holds.
fn entries(b: &[u8], at: usize, count: u32) -> Result<Vec<Entry>, Refused> {
    let count = usize::try_from(count).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
    let room = b.len().saturating_sub(at) / 16;
    if count > room {
        return Err(Refused(resp::ERR_INVALID_PARAMETER));
    }
    (0..count)
        .map(|i| {
            let e = at + 16 * i;
            Some(Entry {
                addr: le64(b, e)?,
                len: le32(b, e + 8)?,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(Refused(resp::ERR_INVALID_PARAMETER))
}

/// Takes `chain`'s request: its readable bytes, `max` at most, a header and its command.
/// A request too short for its header is not answered (`None`), as its header's fence
/// and context cannot be known; one whose fields do not fit is refused.
pub fn read(chain: &Chain, mem: &Access<'_>, max: usize) -> Option<(Header, Result<Command, Refused>)> {
    let mut b = Vec::new();
    for d in chain.readable() {
        let len = usize::try_from(d.len).ok()?;
        let take = len.min(max.saturating_sub(b.len()));
        if take == 0 {
            break;
        }
        let at = b.len();
        b.resize(at + take, 0);
        mem.read(d.addr, b.get_mut(at..)?).ok()?;
    }
    let header = Header::parse(&b)?;
    let field = |at: usize| le32(&b, HEADER + at).ok_or(Refused(resp::ERR_INVALID_PARAMETER));
    let field64 = |at: usize| le64(&b, HEADER + at).ok_or(Refused(resp::ERR_INVALID_PARAMETER));
    let command = (|| {
        Ok(match header.kind {
            cmd::GET_DISPLAY_INFO => Command::GetDisplayInfo,
            cmd::GET_CAPSET_INFO => Command::GetCapsetInfo { index: field(0)? },
            cmd::GET_CAPSET => Command::GetCapset {
                id: field(0)?,
                version: field(4)?,
            },
            cmd::CTX_CREATE => {
                let nlen = usize::try_from(field(0)?).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
                let context_init = field(4)?;
                // debug_name[64]: its first nlen bytes, 64 at most.
                let name = b
                    .get(HEADER + 8..HEADER + 8 + nlen.min(64))
                    .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?
                    .to_vec();
                Command::CtxCreate { context_init, name }
            }
            cmd::CTX_DESTROY => Command::CtxDestroy,
            cmd::CTX_ATTACH_RESOURCE => Command::CtxAttachResource { resource: field(0)? },
            cmd::CTX_DETACH_RESOURCE => Command::CtxDetachResource { resource: field(0)? },
            cmd::RESOURCE_CREATE_BLOB => {
                let count = field(12)?;
                Command::ResourceCreateBlob {
                    resource: field(0)?,
                    mem: field(4)?,
                    flags: field(8)?,
                    id: field64(16)?,
                    size: field64(24)?,
                    entries: entries(&b, HEADER + 32, count)?,
                }
            }
            cmd::RESOURCE_UNREF => Command::ResourceUnref { resource: field(0)? },
            cmd::RESOURCE_ATTACH_BACKING => {
                let count = field(4)?;
                Command::AttachBacking {
                    resource: field(0)?,
                    entries: entries(&b, HEADER + 8, count)?,
                }
            }
            cmd::RESOURCE_DETACH_BACKING => Command::DetachBacking { resource: field(0)? },
            cmd::SUBMIT_3D => {
                let size = usize::try_from(field(0)?).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
                let commands = b
                    .get(HEADER + 8..)
                    .and_then(|rest| rest.get(..size))
                    .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?
                    .to_vec();
                Command::Submit3d { commands }
            }
            cmd::RESOURCE_MAP_BLOB => Command::MapBlob {
                resource: field(0)?,
                offset: field64(8)?,
            },
            cmd::RESOURCE_UNMAP_BLOB => Command::UnmapBlob { resource: field(0)? },
            other => Command::Unsupported(other),
        })
    })();
    Some((header, command))
}

/// Writes an answer of `kind` to `request`, with `fields` after its header, into `chain`'s
/// writable buffers: its fence and context echoed where the request asked for a fence (§
/// 5.7.6.7). Returns the bytes written; an answer the buffers have no room for is cut short.
pub fn answer(chain: &Chain, mem: &Access<'_>, request: &Header, kind: u32, fields: &[u8]) -> u32 {
    let fenced = request.flags & FLAG_FENCE != 0;
    let header = Header {
        kind,
        flags: request.flags & (FLAG_FENCE | FLAG_INFO_RING_IDX),
        fence_id: if fenced { request.fence_id } else { 0 },
        ctx_id: if fenced { request.ctx_id } else { 0 },
        ring_idx: if fenced { request.ring_idx } else { 0 },
    };
    let head = header.bytes();
    let mut written = 0u32;
    let mut parts = [&head[..], fields].into_iter().flat_map(<[u8]>::iter).copied();
    for d in chain.writable() {
        let len = usize::try_from(d.len).unwrap_or(0);
        let chunk: Vec<u8> = parts.by_ref().take(len).collect();
        if chunk.is_empty() {
            break;
        }
        if mem.write(d.addr, &chunk).is_err() {
            break;
        }
        written = written.saturating_add(u32::try_from(chunk.len()).unwrap_or(u32::MAX));
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::Descriptor;
    use crate::memory::GuestMemory;

    const BASE: u64 = 0x8000_0000;

    fn request(header: &Header, fields: &[u8]) -> Vec<u8> {
        let mut b = header.bytes().to_vec();
        b.extend_from_slice(fields);
        b
    }

    /// A chain of `parts` as readable buffers, then a writable one of `answer` bytes.
    fn chain(a: &Access<'_>, parts: &[&[u8]], answer: u32) -> (Chain, u64) {
        let mut at = BASE;
        let mut descriptors = Vec::new();
        for p in parts {
            a.write(at, p).unwrap();
            descriptors.push(Descriptor {
                addr: at,
                len: u32::try_from(p.len()).unwrap(),
                writable: false,
            });
            at += 0x1000;
        }
        descriptors.push(Descriptor {
            addr: at,
            len: answer,
            writable: true,
        });
        (Chain { head: 0, descriptors }, at)
    }

    /// A request split across buffers, as a driver may give it, reads whole: a context's
    /// creation, its name cut at what `nlen` says.
    #[test]
    fn a_request_reads_across_its_buffers() {
        let mem = GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap();
        let header = Header {
            kind: cmd::CTX_CREATE,
            ctx_id: 7,
            ..Header::default()
        };
        let mut fields = 5u32.to_le_bytes().to_vec();
        fields.extend(4u32.to_le_bytes());
        fields.extend(b"probe-and-more");
        fields.resize(8 + 64, 0);
        let b = request(&header, &fields);
        let (head, rest) = b.split_at(10);
        let access = mem.access().unwrap();
        let (c, _) = chain(&access, &[head, rest], 64);
        let (h, command) = read(&c, &access, 1 << 20).unwrap();
        assert_eq!(h.ctx_id, 7);
        assert_eq!(
            command,
            Ok(Command::CtxCreate {
                context_init: 4,
                name: b"probe".to_vec()
            })
        );
    }

    /// Counts and sizes are held to what the request holds: entries past its end, and a
    /// submission longer than its bytes, are refused; one too short for a header is not
    /// answered.
    #[test]
    fn a_request_cannot_claim_more_than_it_holds() {
        let mem = GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap();
        let access = mem.access().unwrap();
        let blob = Header {
            kind: cmd::RESOURCE_CREATE_BLOB,
            ..Header::default()
        };
        let mut fields = vec![0u8; 32];
        fields[12..16].copy_from_slice(&1000u32.to_le_bytes());
        let (c, _) = chain(&access, &[&request(&blob, &fields)], 64);
        assert_eq!(
            read(&c, &access, 1 << 20).unwrap().1,
            Err(Refused(resp::ERR_INVALID_PARAMETER))
        );
        let submit = Header {
            kind: cmd::SUBMIT_3D,
            ..Header::default()
        };
        let mut fields = 64u32.to_le_bytes().to_vec();
        fields.extend([0u8; 4]);
        fields.extend([1u8; 16]);
        let (c, _) = chain(&access, &[&request(&submit, &fields)], 64);
        assert_eq!(
            read(&c, &access, 1 << 20).unwrap().1,
            Err(Refused(resp::ERR_INVALID_PARAMETER))
        );
        let (c, _) = chain(&access, &[&[0u8; 10]], 64);
        assert_eq!(read(&c, &access, 1 << 20), None);
        // What `max` cuts is not there to claim.
        let mut fields = 16u32.to_le_bytes().to_vec();
        fields.extend([0u8; 4]);
        fields.extend([1u8; 16]);
        let (c, _) = chain(&access, &[&request(&submit, &fields)], 64);
        assert_eq!(
            read(&c, &access, HEADER + 8).unwrap().1,
            Err(Refused(resp::ERR_INVALID_PARAMETER))
        );
        assert_eq!(
            read(&c, &access, 1 << 20).unwrap().1,
            Ok(Command::Submit3d {
                commands: vec![1u8; 16]
            })
        );
    }

    /// An answer echoes its request's fence, context and ring where a fence was asked, and
    /// nothing of them where it was not; it spans the writable buffers it is given.
    #[test]
    fn answers_echo_their_fence() {
        let mem = GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap();
        let access = mem.access().unwrap();
        let fenced = Header {
            kind: cmd::SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 99,
            ctx_id: 3,
            ring_idx: 2,
        };
        let (c, at) = chain(&access, &[&[0u8; 8]], 64);
        assert_eq!(answer(&c, &access, &fenced, resp::OK_NODATA, &[]), HEADER as u32);
        let mut got = [0u8; HEADER];
        access.read(at, &mut got).unwrap();
        assert_eq!(
            Header::parse(&got).unwrap(),
            Header {
                kind: resp::OK_NODATA,
                ..fenced
            }
        );
        assert_eq!(fenced.fence(), Some((99, Some(2))));
        let plain = Header {
            kind: cmd::GET_CAPSET_INFO,
            fence_id: 5,
            ctx_id: 9,
            ..Header::default()
        };
        assert_eq!(plain.fence(), None);
        let fields = [7u8; 16];
        assert_eq!(answer(&c, &access, &plain, resp::OK_CAPSET_INFO, &fields), 40);
        let mut got = [0u8; 40];
        access.read(at, &mut got).unwrap();
        assert_eq!(
            Header::parse(&got).unwrap(),
            Header {
                kind: resp::OK_CAPSET_INFO,
                ..Header::default()
            }
        );
        assert_eq!(&got[HEADER..], &fields);
    }
}
