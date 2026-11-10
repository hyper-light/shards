//! Fixed fixture messages, source-linked to init/src/links.rs and Linux v6.18
//! netlink.h, rtnetlink.h, fib_rules.h and netfilter/{nfnetlink,nf_tables}.h.
//! Deliberately not a production netlink API (see the experiment's evidence).

use super::*;

const REQUEST: u16 = 1;
const ACK: u16 = 4;
const CREATE_EXCL: u16 = 0x600;
const NESTED: u16 = 0x8000;
const TABLE_NAME: &[u8] = b"managed-intercept\0";

fn put(out: &mut Vec<u8>, data: &[u8]) -> io::Result<()> {
    out.try_reserve(data.len()).map_err(io::Error::other)?;
    out.extend_from_slice(data);
    Ok(())
}

fn attr(out: &mut Vec<u8>, kind: u16, value: &[u8]) -> io::Result<()> {
    let len = value
        .len()
        .checked_add(4)
        .ok_or_else(|| invalid("attribute overflow"))?;
    let len16 = u16::try_from(len).map_err(|_| invalid("attribute exceeds ABI width"))?;
    put(out, &len16.to_ne_bytes())?;
    put(out, &kind.to_ne_bytes())?;
    put(out, value)?;
    let padding = (4 - len % 4) % 4;
    put(
        out,
        [0u8; 4]
            .get(..padding)
            .ok_or_else(|| invalid("attribute padding"))?,
    )
}

fn be(out: &mut Vec<u8>, kind: u16, value: u32) -> io::Result<()> {
    attr(out, kind, &value.to_be_bytes())
}

pub(super) struct Netlink(OwnedFd);

impl Netlink {
    pub(super) fn open(protocol: i32) -> io::Result<Self> {
        // SAFETY: socket creates a new owned descriptor; no host namespace is changed.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is the uniquely owned descriptor just created.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn exchange(&self, messages: &[Message], deadline: &Deadline) -> io::Result<()> {
        let mut out = Vec::new();
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(messages.len())
            .map_err(io::Error::other)?;
        for (index, m) in messages.iter().enumerate() {
            let sequence = index
                .checked_add(1)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("netlink sequence overflow"))?;
            let length = m
                .body
                .len()
                .checked_add(16)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("netlink message overflow"))?;
            put(&mut out, &length.to_ne_bytes())?;
            put(&mut out, &m.kind.to_ne_bytes())?;
            put(&mut out, &m.flags.to_ne_bytes())?;
            put(&mut out, &sequence.to_ne_bytes())?;
            put(&mut out, &0u32.to_ne_bytes())?;
            put(&mut out, &m.body)?;
            if m.flags & ACK != 0 {
                pending.push(sequence);
            }
        }
        deadline.io(self.0.as_raw_fd(), libc::POLLOUT, || {
            // SAFETY: the complete initialized netlink message buffer is readable.
            let n = unsafe { libc::send(self.0.as_raw_fd(), out.as_ptr().cast(), out.len(), 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if usize::try_from(n).ok() != Some(out.len()) {
                return Err(invalid("partial netlink send"));
            }
            Ok(())
        })?;
        while !pending.is_empty() {
            let mut probe = 0u8;
            let length = deadline.io(self.0.as_raw_fd(), libc::POLLIN, || {
                // SAFETY: MSG_TRUNC learns the datagram length without consuming it.
                let n = unsafe {
                    libc::recv(
                        self.0.as_raw_fd(),
                        (&raw mut probe).cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_TRUNC,
                    )
                };
                usize::try_from(n).map_err(|_| io::Error::last_os_error())
            })?;
            if length == 0 {
                return Err(invalid("empty netlink acknowledgment"));
            }
            let mut reply = buffer(length)?;
            deadline.io(self.0.as_raw_fd(), libc::POLLIN, || {
                // SAFETY: reply is writable for the learned datagram length.
                let n = unsafe { libc::recv(self.0.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len(), 0) };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                if usize::try_from(n).ok() != Some(reply.len()) {
                    return Err(invalid("netlink datagram changed"));
                }
                Ok(())
            })?;
            acknowledge(&reply, &mut pending)?;
        }
        Ok(())
    }

    pub(super) fn link_up(&self, index: u32, deadline: &Deadline) -> io::Result<()> {
        let body = ifinfo(index, libc::IFF_UP as u32)?;
        self.exchange(&[Message::route(16, 0, body)], deadline)
    }

    pub(super) fn veth(
        &self,
        here: &[u8],
        peer: &[u8],
        peer_ns: RawFd,
        deadline: &Deadline,
    ) -> io::Result<()> {
        let mut body = ifinfo(0, 0)?;
        attr(&mut body, 3, here)?; // IFLA_IFNAME
        let mut other = ifinfo(0, 0)?;
        attr(&mut other, 3, peer)?;
        attr(&mut other, 28, &peer_ns.to_ne_bytes())?; // IFLA_NET_NS_FD
        let mut data = Vec::new();
        attr(&mut data, 1 | NESTED, &other)?; // VETH_INFO_PEER
        let mut info = Vec::new();
        attr(&mut info, 1, b"veth\0")?; // IFLA_INFO_KIND
        attr(&mut info, 2 | NESTED, &data)?;
        attr(&mut body, 18 | NESTED, &info)?;
        self.exchange(&[Message::route(16, CREATE_EXCL, body)], deadline)
    }

    pub(super) fn address(&self, index: u32, ip: Ipv4Addr, deadline: &Deadline) -> io::Result<()> {
        let mut body = Vec::new();
        put(&mut body, &[libc::AF_INET as u8, 30, 0, 0])?; // fixture's RFC5737 /30 links
        put(&mut body, &index.to_ne_bytes())?;
        attr(&mut body, 1, &ip.octets())?;
        attr(&mut body, 2, &ip.octets())?;
        self.exchange(&[Message::route(20, CREATE_EXCL, body)], deadline)
    }

    pub(super) fn default_route(&self, index: u32, gateway: Ipv4Addr, deadline: &Deadline) -> io::Result<()> {
        let body = route(index, Ipv4Addr::UNSPECIFIED, 0, Some(gateway), 254, 1, 0)?;
        self.exchange(&[Message::route(24, CREATE_EXCL, body)], deadline)
    }

    pub(super) fn policy(
        &self,
        lo: u32,
        input: &Inputs,
        destination: SocketAddr,
        deadline: &Deadline,
    ) -> io::Result<()> {
        let SocketAddr::V4(destination) = destination else {
            return Err(invalid("policy experiment is IPv4 only"));
        };
        // The actual bound listener address is authoritative, including loopback
        // and own-address cases where the old remote BACKEND is not the target.
        let body = route(lo, *destination.ip(), 32, None, input.table, 2, 254)?; // RTN_LOCAL / RT_SCOPE_HOST
        self.exchange(&[Message::route(24, CREATE_EXCL, body)], deadline)?;
        let mut body = Vec::new();
        put(&mut body, &[libc::AF_INET as u8, 0, 0, 0, 0, 0, 0, 1])?; // fib_rule_hdr, FR_ACT_TO_TBL
        put(&mut body, &0u32.to_ne_bytes())?;
        for (kind, value) in [
            (6, input.priority),
            (10, input.mark),
            (16, u32::MAX),
            (15, input.table),
        ] {
            attr(&mut body, kind, &value.to_ne_bytes())?;
        }
        self.exchange(&[Message::route(32, CREATE_EXCL, body)], deadline)
    }

    pub(super) fn interception(
        &self,
        input: &Inputs,
        destination: SocketAddr,
        port: u16,
        deadline: &Deadline,
    ) -> io::Result<()> {
        self.exchange(&interception_messages(input, destination, port)?, deadline)
    }
}

fn interception_messages(input: &Inputs, destination: SocketAddr, port: u16) -> io::Result<Vec<Message>> {
    let mut table = Vec::new();
    attr(&mut table, 1, TABLE_NAME)?;
    let begin = Message {
        kind: 0x10,
        flags: REQUEST,
        body: bytes(&[0, 0, 0, 10])?,
    };
    let end = Message {
        kind: 0x11,
        flags: REQUEST,
        body: bytes(&[0, 0, 0, 10])?,
    };
    let pre = chain(b"pre\0", b"filter\0", 0)?;
    let mut expressions = selector(destination)?;
    load(&mut expressions, &port.to_be_bytes())?;
    let mut tproxy = Vec::new();
    be(&mut tproxy, 1, libc::AF_INET as u32)?; // NFTA_TPROXY_FAMILY
    be(&mut tproxy, 3, 1)?; // NFTA_TPROXY_REG_PORT, NFT_REG_1
    expression(&mut expressions, b"tproxy\0", &tproxy)?;
    mark_accept(&mut expressions, input.mark)?;
    let intercept = rule(b"pre\0", expressions)?;
    // nft_do_chain advances to the NEXT rule after NFT_BREAK (v6.18,
    // nf_tables_core.c:255–290). A drop appended to the TPROXY rule is skipped.
    let mut expressions = selector(destination)?;
    verdict(&mut expressions, Verdict::Drop)?;
    let initial = [
        begin,
        Message::nft(0, CREATE_EXCL, table)?,
        pre,
        intercept,
        rule(b"pre\0", expressions)?,
    ];
    let output = if input.origin != Origin::Forward {
        let mut expressions = selector(destination)?;
        mark_accept(&mut expressions, input.mark)?;
        Some([chain(b"out\0", b"route\0", 3)?, rule(b"out\0", expressions)?])
    } else {
        None
    };
    let ending = [end];
    let count = initial
        .len()
        .checked_add(output.as_ref().map_or(0, |messages| messages.len()))
        .and_then(|count| count.checked_add(ending.len()))
        .ok_or_else(|| invalid("interception message count overflow"))?;
    let mut messages = Vec::new();
    messages.try_reserve_exact(count).map_err(io::Error::other)?;
    messages.extend(initial);
    if let Some(output) = output {
        messages.extend(output);
    }
    messages.extend(ending);
    Ok(messages)
}

fn bytes(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    put(&mut out, input)?;
    Ok(out)
}

struct Message {
    kind: u16,
    flags: u16,
    body: Vec<u8>,
}
impl Message {
    fn route(kind: u16, flags: u16, body: Vec<u8>) -> Self {
        Self {
            kind,
            flags: flags | REQUEST | ACK,
            body,
        }
    }
    fn nft(kind: u16, flags: u16, attributes: Vec<u8>) -> io::Result<Self> {
        let mut body = bytes(&[libc::AF_INET as u8, 0, 0, 0])?;
        put(&mut body, &attributes)?;
        Ok(Self::route((10 << 8) | kind, flags, body))
    }
}

fn ifinfo(index: u32, flags: u32) -> io::Result<Vec<u8>> {
    let index = i32::try_from(index).map_err(|_| invalid("interface index exceeds ABI width"))?;
    let mut out = bytes(&[0; 4])?;
    put(&mut out, &index.to_ne_bytes())?;
    put(&mut out, &flags.to_ne_bytes())?;
    put(&mut out, &(libc::IFF_UP as u32).to_ne_bytes())?;
    Ok(out)
}

fn route(
    index: u32,
    destination: Ipv4Addr,
    prefix: u8,
    gateway: Option<Ipv4Addr>,
    table: u32,
    kind: u8,
    scope: u8,
) -> io::Result<Vec<u8>> {
    let mut out = bytes(&[libc::AF_INET as u8, prefix, 0, 0, 0, 3, scope, kind])?;
    put(&mut out, &0u32.to_ne_bytes())?;
    attr(&mut out, 15, &table.to_ne_bytes())?; // RTA_TABLE
    if prefix != 0 {
        attr(&mut out, 1, &destination.octets())?;
    }
    if let Some(ip) = gateway {
        attr(&mut out, 5, &ip.octets())?;
    }
    attr(&mut out, 4, &index.to_ne_bytes())?;
    Ok(out)
}

fn expression(out: &mut Vec<u8>, name: &[u8], data: &[u8]) -> io::Result<()> {
    let mut body = Vec::new();
    attr(&mut body, 1, name)?;
    attr(&mut body, 2 | NESTED, data)?;
    attr(out, 1 | NESTED, &body)
}

fn load(out: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let mut data = Vec::new();
    attr(&mut data, 1, value)?;
    let mut body = Vec::new();
    be(&mut body, 1, 1)?;
    attr(&mut body, 2 | NESTED, &data)?;
    expression(out, b"immediate\0", &body)
}

fn compare(out: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let mut data = Vec::new();
    attr(&mut data, 1, value)?;
    let mut body = Vec::new();
    be(&mut body, 1, 1)?;
    be(&mut body, 2, 0)?;
    attr(&mut body, 3 | NESTED, &data)?;
    expression(out, b"cmp\0", &body)
}

fn selector(at: SocketAddr) -> io::Result<Vec<u8>> {
    let SocketAddr::V4(at) = at else {
        return Err(invalid("TCP experiment is IPv4 only"));
    };
    let mut out = Vec::new();
    let mut meta = Vec::new();
    be(&mut meta, 1, 1)?;
    be(&mut meta, 2, 16)?;
    expression(&mut out, b"meta\0", &meta)?; // NFT_META_L4PROTO
    compare(&mut out, &[libc::IPPROTO_TCP as u8])?;
    for (base, offset, value) in [
        (1, 16, at.ip().octets().as_slice()),
        (2, 2, at.port().to_be_bytes().as_slice()),
    ] {
        let mut payload = Vec::new();
        be(&mut payload, 1, 1)?;
        be(&mut payload, 2, base)?;
        be(&mut payload, 3, offset)?;
        be(
            &mut payload,
            4,
            u32::try_from(value.len()).map_err(|_| invalid("payload ABI width"))?,
        )?;
        expression(&mut out, b"payload\0", &payload)?;
        compare(&mut out, value)?;
    }
    Ok(out)
}

fn mark_accept(out: &mut Vec<u8>, mark: u32) -> io::Result<()> {
    load(out, &mark.to_ne_bytes())?;
    let mut meta = Vec::new();
    be(&mut meta, 2, 3)?;
    be(&mut meta, 3, 1)?;
    expression(out, b"meta\0", &meta)?; // NFT_META_MARK, NFTA_META_SREG
    verdict(out, Verdict::Accept)
}

// Linux v6.18 include/uapi/linux/netfilter.h: NF_DROP=0, NF_ACCEPT=1.
#[repr(u32)]
enum Verdict {
    Drop = 0,
    Accept = 1,
}

fn verdict(out: &mut Vec<u8>, verdict: Verdict) -> io::Result<()> {
    let mut code = Vec::new();
    be(&mut code, 1, verdict as u32)?; // NFTA_VERDICT_CODE
    let mut data = Vec::new();
    attr(&mut data, 2 | NESTED, &code)?;
    let mut body = Vec::new();
    be(&mut body, 1, 0)?;
    attr(&mut body, 2 | NESTED, &data)?;
    expression(out, b"immediate\0", &body)
}

fn chain(name: &[u8], kind: &[u8], hook: u32) -> io::Result<Message> {
    let mut body = Vec::new();
    attr(&mut body, 1, TABLE_NAME)?;
    attr(&mut body, 3, name)?;
    let mut data = Vec::new();
    be(&mut data, 1, hook)?;
    be(&mut data, 2, 0)?;
    attr(&mut body, 4 | NESTED, &data)?;
    be(&mut body, 5, 1)?;
    attr(&mut body, 7, kind)?;
    Message::nft(3, CREATE_EXCL, body)
}

fn rule(chain: &[u8], expressions: Vec<u8>) -> io::Result<Message> {
    let mut body = Vec::new();
    attr(&mut body, 1, TABLE_NAME)?;
    attr(&mut body, 2, chain)?;
    attr(&mut body, 4 | NESTED, &expressions)?;
    Message::nft(6, 0x400 | 0x800, body)
}

fn field<const N: usize>(input: &[u8], at: usize) -> io::Result<[u8; N]> {
    let end = at
        .checked_add(N)
        .ok_or_else(|| invalid("acknowledgment overflow"))?;
    input
        .get(at..end)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("short acknowledgment"))
}

fn acknowledge(mut input: &[u8], pending: &mut Vec<u32>) -> io::Result<()> {
    while !input.is_empty() {
        let len =
            usize::try_from(u32::from_ne_bytes(field(input, 0)?)).map_err(|_| invalid("ack length width"))?;
        // nlmsghdr + nlmsgerr (errno and the echoed request header).
        if len < 36 || len > input.len() {
            return Err(invalid("invalid acknowledgment length"));
        }
        if u16::from_ne_bytes(field(input, 4)?) != 2 {
            return Err(invalid("unexpected netlink response kind"));
        }
        let seq = u32::from_ne_bytes(field(input, 8)?);
        if !pending.contains(&seq) {
            return Err(invalid("unknown or duplicate acknowledgment"));
        }
        let errno = i32::from_ne_bytes(field(input, 16)?);
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(
                errno
                    .checked_neg()
                    .ok_or_else(|| invalid("invalid netlink errno"))?,
            ));
        }
        pending.retain(|value| *value != seq);
        let aligned = len
            .checked_add(3)
            .ok_or_else(|| invalid("ack alignment overflow"))?
            & !3;
        input = input
            .get(aligned..)
            .ok_or_else(|| invalid("short acknowledgment padding"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acks_reject_wrong_identity_duplicate_truncation_and_errno() {
        let make = |seq: u32, code: i32| {
            let mut out = Vec::new();
            for b in [
                36u32.to_ne_bytes(),
                2u32.to_ne_bytes(),
                seq.to_ne_bytes(),
                0u32.to_ne_bytes(),
                code.to_ne_bytes(),
            ] {
                out.extend(b);
            }
            out.extend([0; 16]);
            out
        };
        acknowledge(&make(1, 0), &mut vec![1]).unwrap();
        assert!(acknowledge(&make(2, 0), &mut vec![1]).is_err());
        assert!(acknowledge(&make(1, 0), &mut vec![]).is_err());
        assert_eq!(
            acknowledge(&make(1, -libc::EPERM), &mut vec![1])
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert!(acknowledge(&[0; 19], &mut vec![1]).is_err());
    }
    #[test]
    fn route_and_selector_keep_exact_ipv4_and_tcp_port() {
        let at = SocketAddr::from((BACKEND, 43210));
        let encoded = selector(at).unwrap();
        assert!(encoded.windows(4).any(|b| b == BACKEND.octets()));
        assert!(encoded.windows(2).any(|b| b == 43210u16.to_be_bytes()));
        let local = route(7, BACKEND, 32, None, 1000, 2, 254).unwrap();
        assert_eq!(local.get(..8).unwrap(), [2, 32, 0, 0, 0, 3, 254, 2]);
        assert!(local.windows(4).any(|b| b == 1000u32.to_ne_bytes()));
        let mut out = Vec::new();
        attr(&mut out, 3, b"x\0").unwrap();
        assert_eq!(out, [6, 0, 3, 0, b'x', 0, 0, 0]);
    }

    fn attributes(mut input: &[u8]) -> Vec<(u16, &[u8])> {
        let mut attrs = Vec::new();
        while !input.is_empty() {
            let length = usize::from(u16::from_ne_bytes(input[..2].try_into().unwrap()));
            let kind = u16::from_ne_bytes(input[2..4].try_into().unwrap());
            assert!((4..=input.len()).contains(&length));
            attrs.push((kind & !NESTED, &input[4..length]));
            input = &input[(length + 3) & !3..];
        }
        attrs
    }

    fn attribute(input: &[u8], kind: u16) -> &[u8] {
        attributes(input).into_iter().find(|(k, _)| *k == kind).unwrap().1
    }

    fn rule_expressions(message: &Message) -> &[u8] {
        assert_eq!(message.kind, (10 << 8) | 6);
        assert_ne!(message.flags & 0x800, 0); // NLM_F_APPEND preserves rule order.
        attribute(&message.body[4..], 4)
    }

    fn immediate_code(expression: &[u8]) -> u32 {
        assert_eq!(attribute(expression, 1), b"immediate\0");
        let data = attribute(expression, 2);
        assert_eq!(attribute(data, 1), 0u32.to_be_bytes()); // NFT_REG_VERDICT
        let verdict = attribute(attribute(data, 2), 2); // NFT_DATA_VERDICT
        u32::from_be_bytes(attribute(verdict, 1).try_into().unwrap())
    }

    #[test]
    fn fallback_is_a_separate_appended_exact_selector_drop() {
        for (origin, ip) in [
            ("forward", BACKEND),
            ("forward-relay-absent", BACKEND),
            ("local", BACKEND),
            ("local-loopback", Ipv4Addr::LOCALHOST),
            ("local-self", ROUTER_BACKEND),
        ] {
            let input = Inputs::parse(origin, "1000", "1", "1000", "100").unwrap();
            let at = SocketAddr::from((ip, 43210));
            let messages = interception_messages(&input, at, 43211).unwrap();
            assert_eq!(messages.first().unwrap().kind, 0x10);
            assert_eq!(messages.last().unwrap().kind, 0x11);
            let rules: Vec<_> = messages
                .iter()
                .filter(|message| message.kind == (10 << 8) | 6)
                .collect();
            assert_eq!(rules.len(), if input.origin == Origin::Forward { 2 } else { 3 });
            let intercept = rules[0];
            let fallback = rules[1];
            for message in [intercept, fallback] {
                assert_eq!(attribute(&message.body[4..], 2), b"pre\0");
            }
            let selected = selector(at).unwrap();
            let intercepted = rule_expressions(intercept);
            let dropped = rule_expressions(fallback);
            assert_eq!(&intercepted[..selected.len()], selected);
            assert_eq!(&dropped[..selected.len()], selected);
            let selectors = attributes(&selected);
            let comparisons: Vec<_> = selectors
                .iter()
                .filter(|(_, expr)| attribute(expr, 1) == b"cmp\0")
                .map(|(_, expr)| attribute(attribute(attribute(expr, 2), 3), 1))
                .collect();
            assert_eq!(
                comparisons,
                [
                    &[libc::IPPROTO_TCP as u8][..],
                    &ip.octets(),
                    &43210u16.to_be_bytes()
                ]
            );
            let intercept_tail = attributes(&intercepted[selected.len()..]);
            let names: Vec<_> = intercept_tail
                .iter()
                .map(|(_, expr)| attribute(expr, 1))
                .collect();
            assert_eq!(
                names,
                [
                    b"immediate\0".as_slice(),
                    b"tproxy\0",
                    b"immediate\0",
                    b"meta\0",
                    b"immediate\0"
                ]
            );
            assert_eq!(immediate_code(intercept_tail.last().unwrap().1), 1); // NF_ACCEPT
            let drop_tail = attributes(&dropped[selected.len()..]);
            assert_eq!(drop_tail.len(), 1);
            assert_eq!(immediate_code(drop_tail[0].1), 0); // NF_DROP
            // Unmatched packets retain the original base-chain ACCEPT policy.
            for message in messages.iter().filter(|message| message.kind == (10 << 8) | 3) {
                assert_eq!(attribute(&message.body[4..], 5), 1u32.to_be_bytes());
            }
            if input.origin != Origin::Forward {
                let output = rules[2];
                assert_eq!(attribute(&output.body[4..], 2), b"out\0");
                let expressions = attributes(rule_expressions(output));
                assert_eq!(immediate_code(expressions.last().unwrap().1), 1);
                assert_eq!(messages.len(), 8);
            } else {
                assert_eq!(messages.len(), 6);
            }
        }
    }

    #[test]
    fn same_namespace_targets_keep_their_actual_destination_in_both_messages() {
        for ip in [Ipv4Addr::LOCALHOST, ROUTER_BACKEND] {
            let at = SocketAddr::from((ip, 43210));
            let encoded = selector(at).unwrap();
            assert!(encoded.windows(4).any(|bytes| bytes == ip.octets()));
            assert!(!encoded.windows(4).any(|bytes| bytes == BACKEND.octets()));
            let local = route(7, ip, 32, None, 1000, 2, 254).unwrap();
            assert!(local.windows(4).any(|bytes| bytes == ip.octets()));
            assert!(!local.windows(4).any(|bytes| bytes == BACKEND.octets()));
        }
    }
}
