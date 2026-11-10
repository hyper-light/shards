//! Fixture-private Linux v6.18 bpf.h prefixes. No production loader API.
use super::*;
use shards_devcgroup::Insn;

#[repr(C)]
struct SockAddrPrefix {
    user_family: u32,
    user_ip4: u32,
    user_ip6: [u32; 4],
    user_port: u32,
    family: u32,
    socket_type: u32,
    protocol: u32,
}

pub(super) fn program(from: SocketAddrV4, to: SocketAddrV4) -> io::Result<Vec<u8>> {
    // Five exact selectors (two instructions each), two writes, allow, exit.
    // All offsets/widths are the v6.18 bpf_sock_addr UAPI; no flow history/map.
    let selectors = [
        (
            std::mem::offset_of!(SockAddrPrefix, user_family),
            libc::AF_INET as u32,
        ),
        (
            std::mem::offset_of!(SockAddrPrefix, socket_type),
            libc::SOCK_DGRAM as u32,
        ),
        (
            std::mem::offset_of!(SockAddrPrefix, protocol),
            libc::IPPROTO_UDP as u32,
        ),
        (
            std::mem::offset_of!(SockAddrPrefix, user_ip4),
            u32::from_ne_bytes(from.ip().octets()),
        ),
        (
            std::mem::offset_of!(SockAddrPrefix, user_port),
            u32::from(from.port().to_be()),
        ),
    ];
    let writes = [
        (
            std::mem::offset_of!(SockAddrPrefix, user_ip4),
            u32::from_ne_bytes(to.ip().octets()),
        ),
        (
            std::mem::offset_of!(SockAddrPrefix, user_port),
            u32::from(to.port().to_be()),
        ),
    ];
    let allow = selectors
        .len()
        .checked_mul(2)
        .and_then(|n| n.checked_add(writes.len()))
        .ok_or_else(|| invalid("instruction count overflow"))?;
    let count = allow
        .checked_add(2)
        .ok_or_else(|| invalid("instruction count overflow"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(
            count
                .checked_mul(size_of::<u64>())
                .ok_or_else(|| invalid("byte count overflow"))?,
        )
        .map_err(io::Error::other)?;
    let mut emit = |code, dst, src, off, imm| {
        bytes.extend_from_slice(
            &Insn {
                code,
                dst,
                src,
                off,
                imm,
            }
            .to_bytes(),
        )
    };
    for (index, (offset, value)) in selectors.into_iter().enumerate() {
        emit(0x61, 2, 1, i16::try_from(offset).map_err(io::Error::other)?, 0); // LDX W
        let next = index
            .checked_mul(2)
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| invalid("branch offset overflow"))?;
        emit(
            0x56,
            2,
            0,
            i16::try_from(
                allow
                    .checked_sub(next)
                    .ok_or_else(|| invalid("branch underflow"))?,
            )
            .map_err(io::Error::other)?,
            i32::from_ne_bytes(value.to_ne_bytes()),
        ); // JMP32 JNE K: compare the full unsigned UAPI word.
    }
    for (offset, value) in writes {
        emit(
            0x62,
            1,
            0,
            i16::try_from(offset).map_err(io::Error::other)?,
            i32::from_ne_bytes(value.to_ne_bytes()),
        ); // ST W
    }
    emit(0xb7, 0, 0, 0, 1); // MOV64 K: allow selected AND unmatched operations
    emit(0x95, 0, 0, 0, 0);
    Ok(bytes)
}

// Linux v6.18 include/uapi/linux/bpf.h: BPF_PROG_LOAD prefix through
// expected_attach_type, BPF_LINK_CREATE common prefix, BPF_LINK_DETACH prefix.
#[repr(C)]
#[derive(Default)]
struct ProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}
#[repr(C)]
struct LinkCreate {
    prog_fd: u32,
    target_fd: u32,
    attach_type: u32,
    flags: u32,
}
#[repr(C)]
struct LinkDetach {
    link_fd: u32,
}

pub(super) struct Link {
    link: OwnedFd,
    _program: OwnedFd,
}
impl Link {
    pub(super) fn attach(bytes: &[u8], cgroup: &File, mode: Mode) -> io::Result<Self> {
        let attach_type = match mode {
            Mode::Connected => 10,
            Mode::Unconnected => 14,
        };
        let load = ProgLoad {
            prog_type: 18,
            insn_cnt: u32::try_from(
                bytes
                    .len()
                    .checked_div(size_of::<u64>())
                    .ok_or_else(|| invalid("instruction width"))?,
            )
            .map_err(io::Error::other)?,
            insns: bytes.as_ptr() as u64,
            license: c"Apache".as_ptr() as u64,
            expected_attach_type: attach_type,
            ..ProgLoad::default()
        };
        let program = call(5, &load)?;
        let link = call(
            28,
            &LinkCreate {
                prog_fd: u32::try_from(program.as_raw_fd()).map_err(io::Error::other)?,
                target_fd: u32::try_from(cgroup.as_raw_fd()).map_err(io::Error::other)?,
                attach_type,
                flags: 0,
            },
        )?;
        Ok(Self {
            link,
            _program: program,
        })
    }
    pub(super) fn detach(self) -> io::Result<()> {
        let attr = LinkDetach {
            link_fd: u32::try_from(self.link.as_raw_fd()).map_err(io::Error::other)?,
        };
        // SAFETY: initialized v6.18 BPF_LINK_DETACH prefix; held unpinned link.
        if unsafe { libc::syscall(libc::SYS_bpf, 34, &raw const attr, size_of_val(&attr)) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
fn call<T>(command: u32, attr: &T) -> io::Result<OwnedFd> {
    // SAFETY: callers supply initialized UAPI prefixes, no pointer outlives call.
    let fd = unsafe { libc::syscall(libc::SYS_bpf, command, attr, size_of_val(attr)) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(io::Error::other)?;
    // SAFETY: successful load/link commands return a new CLOEXEC descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_selectors_and_network_order_writes() {
        let from = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0x1234);
        let to = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 0xabcd);
        let bytes = program(from, to).unwrap();
        let instructions: Vec<_> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| {
                (
                    b[0],
                    b[1],
                    i16::from_le_bytes([b[2], b[3]]),
                    i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                )
            })
            .collect();
        assert_eq!(instructions.len(), 14);
        for (index, offset) in [0, 32, 36, 4, 24].into_iter().enumerate() {
            assert_eq!(instructions[index * 2].0, 0x61);
            assert_eq!(instructions[index * 2].2, offset);
            assert_eq!(
                index * 2 + 2 + usize::try_from(instructions[index * 2 + 1].2).unwrap(),
                12
            );
        }
        assert_eq!(instructions[10], (0x62, 1, 4, i32::from_ne_bytes([127, 0, 0, 2])));
        assert_eq!(instructions[11], (0x62, 1, 24, i32::from(0xabcdu16.to_be())));
        assert_eq!(instructions[12], (0xb7, 0, 0, 1));
        assert_eq!(instructions[13], (0x95, 0, 0, 0));
        assert_eq!(size_of::<ProgLoad>(), 72);
        assert_eq!(size_of::<LinkCreate>(), 16);
        assert_eq!(size_of::<LinkDetach>(), 4);
    }
}
