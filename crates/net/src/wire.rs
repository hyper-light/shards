//! The headers a guest's frames carry, read and written: virtio-net (virtio 1.3 §5.1.6),
//! Ethernet, ARP (RFC 826), IPv4 (RFC 791), ICMP (RFC 792), IPv6 (RFC 8200), ICMPv6 (RFC
//! 4443) with neighbor discovery (RFC 4861), UDP (RFC 768) and TCP (RFC 9293). A guest is
//! untrusted: every read checks its length first, and what does not parse is dropped, as a
//! NIC drops a malformed frame.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The virtio-net header's length, its `flags`' DATA_VALID bit, and where num_buffers is.
pub const VNET: usize = 12;
/// The checksums are good: the guest need not check them (VIRTIO_NET_HDR_F_DATA_VALID).
const VNET_DATA_VALID: u8 = 2;

pub const ETH: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86dd;
pub const IPV4: usize = 20;
pub const IPV6: usize = 40;
pub const PROTO_ICMPV6: u8 = 58;
/// ICMPv6's types (RFC 4443 §2.1, RFC 4861 §4).
pub const ICMPV6_UNREACHABLE: u8 = 1;
pub const ICMPV6_ECHO_REQUEST: u8 = 128;
pub const ICMPV6_ECHO_REPLY: u8 = 129;
pub const ICMPV6_NEIGHBOR_SOLICIT: u8 = 135;
pub const ICMPV6_NEIGHBOR_ADVERT: u8 = 136;
/// Destination unreachable's code for what a filter refuses: communication with the
/// destination administratively prohibited (RFC 4443 §3.1).
pub const ICMPV6_ADMIN_PROHIBITED: u8 = 1;
/// The hop limit neighbor discovery is sent and taken with alone (RFC 4861 §7.1): what
/// came through no router.
const ND_HOP_LIMIT: u8 = 255;
/// IPv6's minimum MTU (RFC 8200 §5): an ICMPv6 error, its quote included, is no longer
/// (RFC 4443 §2.4 (c)).
const IPV6_MIN_MTU: usize = 1280;
pub const PROTO_ICMP: u8 = 1;
/// ICMP's destination unreachable (RFC 792).
pub const ICMP_UNREACHABLE: u8 = 3;
/// Destination unreachable's code for what a filter refuses: communication
/// administratively prohibited (RFC 1812 §5.2.7.1).
pub const ADMIN_PROHIBITED: u8 = 13;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn ip_at(b: &[u8], at: usize) -> Option<Ipv4Addr> {
    let o: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(Ipv4Addr::from(o))
}

fn ip6_at(b: &[u8], at: usize) -> Option<Ipv6Addr> {
    let o: [u8; 16] = b.get(at..at + 16)?.try_into().ok()?;
    Some(Ipv6Addr::from(o))
}

/// An Ethernet frame's addresses, type and payload.
#[derive(Debug)]
pub struct Eth<'a> {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub kind: u16,
    pub payload: &'a [u8],
}

pub fn eth(b: &[u8]) -> Option<Eth<'_>> {
    Some(Eth {
        dst: b.get(0..6)?.try_into().ok()?,
        src: b.get(6..12)?.try_into().ok()?,
        kind: u16_at(b, 12)?,
        payload: b.get(ETH..)?,
    })
}

/// An ARP request for an IPv4 address: who asks (MAC and address), and for what.
#[derive(Debug)]
pub struct ArpRequest {
    pub sender_mac: [u8; 6],
    pub sender_ip: Ipv4Addr,
    pub target_ip: Ipv4Addr,
}

pub fn arp_request(b: &[u8]) -> Option<ArpRequest> {
    // Ethernet hardware, IPv4 protocol, 6- and 4-byte addresses, a request.
    if u16_at(b, 0)? != 1
        || u16_at(b, 2)? != ETHERTYPE_IPV4
        || *b.get(4)? != 6
        || *b.get(5)? != 4
        || u16_at(b, 6)? != 1
    {
        return None;
    }
    Some(ArpRequest {
        sender_mac: b.get(8..14)?.try_into().ok()?,
        sender_ip: ip_at(b, 14)?,
        target_ip: ip_at(b, 24)?,
    })
}

/// An IP packet, of either version: its addresses, protocol and payload, IPv4's options
/// and IPv6's extension headers skipped. Fragments are not reassembled: the guest's MTU
/// is the device's, and a guest that fragments anyway has them dropped, as a stateless
/// NAT drops them.
#[derive(Debug)]
pub struct Ip<'a> {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub proto: u8,
    /// IPv4's time to live, IPv6's hop limit.
    pub ttl: u8,
    /// The headers as they came, options and extension headers and all: what an ICMP error
    /// quotes of an IPv4 packet.
    pub header: &'a [u8],
    /// The whole packet: what an ICMPv6 error quotes, as much of as fits.
    pub packet: &'a [u8],
    pub payload: &'a [u8],
}

pub fn ipv4(b: &[u8]) -> Option<Ip<'_>> {
    let vihl = *b.first()?;
    if vihl >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(vihl & 0xf) * 4;
    let total = usize::from(u16_at(b, 2)?);
    let frag = u16_at(b, 6)?;
    // More fragments, or an offset: a fragment.
    if ihl < IPV4 || total < ihl || frag & 0x3fff != 0 {
        return None;
    }
    Some(Ip {
        src: IpAddr::V4(ip_at(b, 12)?),
        dst: IpAddr::V4(ip_at(b, 16)?),
        proto: *b.get(9)?,
        ttl: *b.get(8)?,
        header: b.get(..ihl)?,
        packet: b.get(..total)?,
        payload: b.get(ihl..total)?,
    })
}

/// An IPv6 packet, its extension headers skipped: hop-by-hop options, routing and
/// destination options (RFC 8200 §4), at most eight of them, each its own length. A
/// fragment header drops it, as IPv4's fragments are dropped.
pub fn ipv6(b: &[u8]) -> Option<Ip<'_>> {
    if *b.first()? >> 4 != 6 {
        return None;
    }
    let end = IPV6.checked_add(usize::from(u16_at(b, 4)?))?;
    let packet = b.get(..end)?;
    let mut next = *packet.get(6)?;
    let mut at = IPV6;
    let mut skipped = 0;
    while matches!(next, 0 | 43 | 60) {
        if skipped == 8 {
            return None;
        }
        let ext = packet.get(at..at + 2)?;
        let (Some(&following), Some(&len)) = (ext.first(), ext.get(1)) else {
            return None;
        };
        next = following;
        at = at.checked_add((usize::from(len) + 1) * 8)?;
        skipped += 1;
    }
    if next == 44 {
        return None;
    }
    Some(Ip {
        src: IpAddr::V6(ip6_at(b, 8)?),
        dst: IpAddr::V6(ip6_at(b, 24)?),
        proto: next,
        ttl: *packet.get(7)?,
        header: packet.get(..at)?,
        packet,
        payload: packet.get(at..)?,
    })
}

/// A neighbor solicitation (RFC 4861 §4.3): who asks, and for what address. Only one
/// that came through no router (hop limit 255) and asks for an address, from one: a
/// duplicate address check (from `::`) asks of no one.
#[derive(Debug)]
pub struct NeighborSolicit {
    pub sender: Ipv6Addr,
    pub target: Ipv6Addr,
}

pub fn neighbor_solicit(ip: &Ip<'_>) -> Option<NeighborSolicit> {
    let IpAddr::V6(sender) = ip.src else { return None };
    let p = ip.payload;
    if ip.proto != PROTO_ICMPV6
        || ip.ttl != ND_HOP_LIMIT
        || sender.is_unspecified()
        || *p.first()? != ICMPV6_NEIGHBOR_SOLICIT
        || *p.get(1)? != 0
    {
        return None;
    }
    Some(NeighborSolicit {
        sender,
        target: ip6_at(p, 8)?,
    })
}

/// A TCP segment's header fields and payload.
#[derive(Debug)]
pub struct Tcp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// Options: the MSS and window scale the sender offers, in a SYN.
    pub mss: Option<u16>,
    pub wscale: Option<u8>,
    pub payload: &'a [u8],
}

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

pub fn tcp(b: &[u8]) -> Option<Tcp<'_>> {
    let off = usize::from(*b.get(12)? >> 4) * 4;
    if off < 20 {
        return None;
    }
    let (mut mss, mut wscale) = (None, None);
    let opts = b.get(20..off)?;
    let mut i = 0;
    while let Some(&kind) = opts.get(i) {
        match kind {
            0 => break,
            1 => i += 1,
            _ => {
                let len = usize::from(*opts.get(i + 1)?);
                if len < 2 {
                    return None;
                }
                match (kind, len) {
                    (2, 4) => mss = u16_at(opts, i + 2),
                    (3, 3) => wscale = opts.get(i + 2).copied(),
                    _ => {}
                }
                i += len;
            }
        }
    }
    Some(Tcp {
        src_port: u16_at(b, 0)?,
        dst_port: u16_at(b, 2)?,
        seq: u32_at(b, 4)?,
        ack: u32_at(b, 8)?,
        flags: *b.get(13)?,
        window: u16_at(b, 14)?,
        mss,
        wscale,
        payload: b.get(off..)?,
    })
}

/// A UDP datagram's ports and payload.
#[derive(Debug)]
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

pub fn udp(b: &[u8]) -> Option<Udp<'_>> {
    let len = usize::from(u16_at(b, 4)?);
    if len < 8 {
        return None;
    }
    Some(Udp {
        src_port: u16_at(b, 0)?,
        dst_port: u16_at(b, 2)?,
        payload: b.get(8..len)?,
    })
}

/// The one's-complement sum of `b`, folded (RFC 1071).
fn checksum(b: &[u8]) -> u16 {
    let mut sum = 0u32;
    let (pairs, rest) = b.as_chunks::<2>();
    for p in pairs {
        sum += u32::from(u16::from_be_bytes(*p));
    }
    if let [last] = rest {
        sum += u32::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// ICMPv6's checksum of `msg` (RFC 4443 §2.3): over IPv6's pseudo-header (RFC 8200 §8.1)
/// and the message.
fn icmpv6_checksum(src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) -> u16 {
    let mut b = Vec::with_capacity(40 + msg.len());
    b.extend_from_slice(&src.octets());
    b.extend_from_slice(&dst.octets());
    b.extend_from_slice(&u32::try_from(msg.len()).unwrap_or(u32::MAX).to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    b.extend_from_slice(msg);
    checksum(&b)
}

/// The frames this process sends the guest, written into `out` from the start: a
/// virtio-net header saying the checksums need no check (the guest's stack trusts its own
/// device's word, and this link is a pipe between two of shards' processes), then
/// Ethernet from the gateway.
#[derive(Debug)]
pub struct Frames {
    pub gateway_mac: [u8; 6],
    pub guest_mac: [u8; 6],
}

impl Frames {
    fn start(&self, out: &mut Vec<u8>, kind: u16, l4_checked: bool) {
        out.clear();
        out.push(if l4_checked { VNET_DATA_VALID } else { 0 });
        out.extend_from_slice(&[0u8; VNET - 1]);
        out.extend_from_slice(&self.guest_mac);
        out.extend_from_slice(&self.gateway_mac);
        out.extend_from_slice(&kind.to_be_bytes());
    }

    /// The gateway's answer to an ARP request for `ip`: its own MAC, whatever the address,
    /// since every address but the guest's own is reached through it.
    pub fn arp_reply(&self, out: &mut Vec<u8>, req: &ArpRequest) {
        self.start(out, ETHERTYPE_ARP, false);
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        out.extend_from_slice(&[6, 4]);
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&self.gateway_mac);
        out.extend_from_slice(&req.target_ip.octets());
        out.extend_from_slice(&req.sender_mac);
        out.extend_from_slice(&req.sender_ip.octets());
    }

    /// An IPv4 header from `src` to `dst` for `len` bytes of `proto`, its checksum
    /// computed: the guest always checks an IPv4 header's.
    fn ipv4(&self, out: &mut Vec<u8>, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) {
        let at = out.len();
        let total = u16::try_from(IPV4 + len).unwrap_or(u16::MAX);
        out.extend_from_slice(&[0x45, 0]);
        out.extend_from_slice(&total.to_be_bytes());
        // ID 0 and Don't Fragment, as Linux sends what it will not fragment.
        out.extend_from_slice(&[0, 0, 0x40, 0, 64, proto, 0, 0]);
        out.extend_from_slice(&src.octets());
        out.extend_from_slice(&dst.octets());
        let sum = checksum(out.get(at..).unwrap_or_default());
        if let Some(s) = out.get_mut(at + 10..at + 12) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
    }

    /// An IPv6 header from `src` to `dst` for `len` bytes of `proto`, with hop limit
    /// `hops`.
    fn ipv6(&self, out: &mut Vec<u8>, src: Ipv6Addr, dst: Ipv6Addr, proto: u8, len: usize, hops: u8) {
        let len = u16::try_from(len).unwrap_or(u16::MAX);
        out.extend_from_slice(&[0x60, 0, 0, 0]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[proto, hops]);
        out.extend_from_slice(&src.octets());
        out.extend_from_slice(&dst.octets());
    }

    /// An IP header of the addresses' version, from `src` to `dst`, for `len` bytes of
    /// `proto`, after the Ethernet header of that version; `l4_checked` as [`Frames::start`]
    /// takes it.
    fn ip(&self, out: &mut Vec<u8>, src: IpAddr, dst: IpAddr, proto: u8, len: usize, l4_checked: bool) {
        match (src, dst) {
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                self.start(out, ETHERTYPE_IPV6, l4_checked);
                self.ipv6(out, s, d, proto, len, 64);
            }
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                self.start(out, ETHERTYPE_IPV4, l4_checked);
                self.ipv4(out, s, d, proto, len);
            }
            // Never one of each: the stack answers from the version it was asked in.
            _ => {
                self.start(out, ETHERTYPE_IPV4, l4_checked);
                self.ipv4(out, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED, proto, len);
            }
        }
    }

    /// ICMPv6 message `msg` from `src` to `dst`, its checksum filled in, with hop limit
    /// `hops`.
    fn icmpv6(&self, out: &mut Vec<u8>, src: Ipv6Addr, dst: Ipv6Addr, mut msg: Vec<u8>, hops: u8) {
        let sum = icmpv6_checksum(src, dst, &msg);
        if let Some(s) = msg.get_mut(2..4) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
        self.start(out, ETHERTYPE_IPV6, false);
        self.ipv6(out, src, dst, PROTO_ICMPV6, msg.len(), hops);
        out.extend_from_slice(&msg);
    }

    /// An echo reply carrying `data` (identifier and sequence included), ICMP's or
    /// ICMPv6's as the addresses are.
    pub fn icmp_echo_reply(&self, out: &mut Vec<u8>, src: IpAddr, dst: IpAddr, data: &[u8]) {
        if let (IpAddr::V6(s), IpAddr::V6(d)) = (src, dst) {
            let mut msg = vec![ICMPV6_ECHO_REPLY, 0, 0, 0];
            msg.extend_from_slice(data);
            self.icmpv6(out, s, d, msg, 64);
            return;
        }
        self.ip(out, src, dst, PROTO_ICMP, 4 + data.len(), false);
        let at = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(data);
        let sum = checksum(out.get(at..).unwrap_or_default());
        if let Some(s) = out.get_mut(at + 2..at + 4) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
    }

    /// A destination unreachable for what a filter refuses, for datagram `ip`: ICMP's
    /// (RFC 792; code 13, RFC 1812 §5.2.7.1), quoting its header and the first 8 bytes of
    /// its data; or ICMPv6's (RFC 4443 §3.1; code 1), quoting as much of it as fits in
    /// IPv6's minimum MTU (§2.4 (c)).
    pub fn icmp_unreachable(&self, out: &mut Vec<u8>, src: IpAddr, dst: IpAddr, ip: &Ip<'_>) {
        if let (IpAddr::V6(s), IpAddr::V6(d)) = (src, dst) {
            let room = IPV6_MIN_MTU - IPV6 - 8;
            let mut msg = vec![ICMPV6_UNREACHABLE, ICMPV6_ADMIN_PROHIBITED, 0, 0, 0, 0, 0, 0];
            msg.extend_from_slice(ip.packet.get(..ip.packet.len().min(room)).unwrap_or_default());
            self.icmpv6(out, s, d, msg, 64);
            return;
        }
        let data = ip.payload.get(..ip.payload.len().min(8)).unwrap_or_default();
        self.ip(out, src, dst, PROTO_ICMP, 8 + ip.header.len() + data.len(), false);
        let at = out.len();
        out.extend_from_slice(&[ICMP_UNREACHABLE, ADMIN_PROHIBITED, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(ip.header);
        out.extend_from_slice(data);
        let sum = checksum(out.get(at..).unwrap_or_default());
        if let Some(s) = out.get_mut(at + 2..at + 4) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
    }

    /// The gateway's answer to a neighbor solicitation (RFC 4861 §7.2.4): its own MAC for
    /// the address asked, whatever the address, as a router's (its flags router,
    /// solicited and override), since every address but the guest's own is reached
    /// through it, as [`Frames::arp_reply`] answers.
    pub fn neighbor_advert(&self, out: &mut Vec<u8>, ns: &NeighborSolicit) {
        let mut msg = vec![ICMPV6_NEIGHBOR_ADVERT, 0, 0, 0, 0xe0, 0, 0, 0];
        msg.extend_from_slice(&ns.target.octets());
        // Target link-layer address (RFC 4861 §4.6.1): type 2, one 8-byte unit.
        msg.extend_from_slice(&[2, 1]);
        msg.extend_from_slice(&self.gateway_mac);
        self.icmpv6(out, ns.target, ns.sender, msg, ND_HOP_LIMIT);
    }

    /// A UDP datagram's headers, for `len` bytes of payload that follow them: its
    /// checksum is left to the guest's trust in its device, so the headers need nothing of
    /// the bytes, which go to the guest from where they lie.
    pub fn udp_headers(&self, out: &mut Vec<u8>, src: (IpAddr, u16), dst: (IpAddr, u16), len: usize) {
        self.ip(out, src.0, dst.0, PROTO_UDP, 8 + len, true);
        let len = u16::try_from(8 + len).unwrap_or(u16::MAX);
        out.extend_from_slice(&src.1.to_be_bytes());
        out.extend_from_slice(&dst.1.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
    }

    /// A TCP segment's headers, for `len` bytes of payload that follow them, its checksum
    /// left to the guest's trust in its device as a datagram's is; `syn_options` adds the
    /// MSS and window scale a SYN or SYN-ACK offers.
    #[allow(clippy::too_many_arguments)]
    pub fn tcp_headers(
        &self,
        out: &mut Vec<u8>,
        src: (IpAddr, u16),
        dst: (IpAddr, u16),
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        syn_options: Option<(u16, Option<u8>)>,
        len: usize,
    ) {
        let opts = match syn_options {
            Some((_, Some(_))) => 8,
            Some((_, None)) => 4,
            None => 0,
        };
        self.ip(out, src.0, dst.0, PROTO_TCP, 20 + opts + len, true);
        out.extend_from_slice(&src.1.to_be_bytes());
        out.extend_from_slice(&dst.1.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ack.to_be_bytes());
        out.push((((20 + opts) / 4) as u8) << 4);
        out.push(flags);
        out.extend_from_slice(&window.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        if let Some((mss, wscale)) = syn_options {
            out.extend_from_slice(&[2, 4]);
            out.extend_from_slice(&mss.to_be_bytes());
            if let Some(w) = wscale {
                // NOP, then window scale.
                out.extend_from_slice(&[1, 3, 3, w]);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// An IPv4 header's checksum, as written, sums to zero (RFC 1071); a TCP SYN-ACK's
    /// options read back as written.
    #[test]
    fn headers_read_back_as_written() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let mut out = Vec::new();
        let (a, b) = (IpAddr::from([1, 2, 3, 4]), IpAddr::from([172, 17, 0, 2]));
        f.tcp_headers(
            &mut out,
            (a, 80),
            (b, 5000),
            7,
            9,
            SYN | ACK,
            1000,
            Some((65480, Some(7))),
            0,
        );
        let e = eth(&out[VNET..]).unwrap();
        assert_eq!(
            (e.dst, e.src, e.kind),
            (f.guest_mac, f.gateway_mac, ETHERTYPE_IPV4)
        );
        assert_eq!(checksum(&e.payload[..IPV4]), 0);
        let ip = ipv4(e.payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.proto), (a, b, PROTO_TCP));
        let t = tcp(ip.payload).unwrap();
        assert_eq!(
            (t.src_port, t.dst_port, t.seq, t.ack, t.flags),
            (80, 5000, 7, 9, SYN | ACK)
        );
        assert_eq!((t.mss, t.wscale, t.window), (Some(65480), Some(7), 1000));
        // Anything short of its headers is dropped, never read past.
        for cut in 0..out.len() - VNET {
            let _ = eth(&out[VNET..VNET + cut])
                .and_then(|e| ipv4(e.payload))
                .and_then(|i| tcp(i.payload));
        }
    }

    /// A segment's and a datagram's headers count the bytes that follow them, and read
    /// back with them as their payload, to the last.
    #[test]
    fn headers_count_the_bytes_that_follow_them() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let (a, b) = (IpAddr::from([1, 2, 3, 4]), IpAddr::from([172, 17, 0, 2]));
        let bytes: Vec<u8> = (0..1460u32).map(|i| (i * 7) as u8).collect();
        let mut out = Vec::new();
        f.tcp_headers(&mut out, (a, 80), (b, 5000), 7, 9, ACK, 1000, None, bytes.len());
        assert_eq!(out.len(), VNET + 14 + IPV4 + 20);
        out.extend_from_slice(&bytes);
        let ip = ipv4(eth(&out[VNET..]).unwrap().payload).unwrap();
        assert_eq!(checksum(ip.header), 0);
        assert_eq!(tcp(ip.payload).unwrap().payload, bytes);
        out.clear();
        f.udp_headers(&mut out, (a, 53), (b, 5353), bytes.len());
        assert_eq!(out.len(), VNET + 14 + IPV4 + 8);
        out.extend_from_slice(&bytes);
        let ip = ipv4(eth(&out[VNET..]).unwrap().payload).unwrap();
        assert_eq!(checksum(ip.header), 0);
        assert_eq!(udp(ip.payload).unwrap().payload, bytes);
    }

    /// A destination unreachable quotes the datagram's header and the first 8 bytes of its
    /// data, under a checksum that sums to zero (RFC 792).
    #[test]
    fn an_unreachable_quotes_the_datagram() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let (gateway, guest, far) = (
            IpAddr::from([172, 17, 0, 1]),
            IpAddr::from([172, 17, 0, 2]),
            IpAddr::from([8, 8, 8, 8]),
        );
        let question = b"a question longer than eight";
        let mut sent = Vec::new();
        f.udp_headers(&mut sent, (guest, 5353), (far, 53), question.len());
        sent.extend_from_slice(question);
        let sent_ip = ipv4(eth(&sent[VNET..]).unwrap().payload).unwrap();
        let mut out = Vec::new();
        f.icmp_unreachable(&mut out, gateway, guest, &sent_ip);
        let e = eth(&out[VNET..]).unwrap();
        let ip = ipv4(e.payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.proto), (gateway, guest, PROTO_ICMP));
        assert_eq!(checksum(ip.payload), 0);
        assert_eq!(&ip.payload[..2], &[ICMP_UNREACHABLE, ADMIN_PROHIBITED]);
        let quoted = &ip.payload[8..];
        assert_eq!(&quoted[..IPV4], sent_ip.header);
        assert_eq!(&quoted[IPV4..], &sent_ip.payload[..8]);
    }

    /// IPv6's headers read back as written: a segment's and a datagram's, under extension
    /// headers skipped; a fragment is dropped.
    #[test]
    fn ipv6_headers_read_back_as_written() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "fd00:1::2".parse().unwrap();
        let mut out = Vec::new();
        f.tcp_headers(
            &mut out,
            (a, 80),
            (b, 5000),
            7,
            9,
            SYN | ACK,
            1000,
            Some((65460, Some(7))),
            3,
        );
        out.extend_from_slice(b"abc");
        let e = eth(&out[VNET..]).unwrap();
        assert_eq!(e.kind, ETHERTYPE_IPV6);
        let ip = ipv6(e.payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.proto, ip.ttl), (a, b, PROTO_TCP, 64));
        let t = tcp(ip.payload).unwrap();
        assert_eq!(
            (t.src_port, t.dst_port, t.mss, t.payload),
            (80, 5000, Some(65460), &b"abc"[..])
        );
        out.clear();
        f.udp_headers(&mut out, (a, 53), (b, 5353), 2);
        out.extend_from_slice(b"hi");
        let ip = ipv6(eth(&out[VNET..]).unwrap().payload).unwrap();
        assert_eq!(udp(ip.payload).unwrap().payload, b"hi");
        // A destination options header (8 bytes) before UDP is skipped; a fragment header
        // drops the packet.
        let packet = |next: u8, ext: &[u8], rest: &[u8]| {
            let mut p = vec![0x60, 0, 0, 0];
            p.extend_from_slice(&u16::try_from(ext.len() + rest.len()).unwrap().to_be_bytes());
            p.extend_from_slice(&[next, 64]);
            p.extend_from_slice(&[0u8; 32]);
            p.extend_from_slice(ext);
            p.extend_from_slice(rest);
            p
        };
        let datagram = [0, 1, 0, 2, 0, 9, 0, 0, b'x'];
        let opts = packet(60, &[PROTO_UDP, 0, 1, 4, 0, 0, 0, 0], &datagram);
        let ip = ipv6(&opts).unwrap();
        assert_eq!((ip.proto, ip.header.len()), (PROTO_UDP, IPV6 + 8));
        assert_eq!(udp(ip.payload).unwrap().payload, b"x");
        assert!(ipv6(&packet(44, &[PROTO_UDP, 0, 0, 1, 0, 0, 0, 0], &datagram)).is_none());
        // Anything short of its headers is dropped, never read past.
        for cut in 0..opts.len() {
            let _ = ipv6(&opts[..cut]).and_then(|i| udp(i.payload));
        }
    }

    /// The gateway answers a neighbor solicitation as a router, for the address asked, to
    /// the asker; its checksum sums to zero over IPv6's pseudo-header. A solicitation that
    /// came through a router, or checks for a duplicate address, asks no one.
    #[test]
    fn neighbor_solicitations_are_answered_as_a_router_answers() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let guest: Ipv6Addr = "fd00:1::2".parse().unwrap();
        let gateway: Ipv6Addr = "fd00:1::1".parse().unwrap();
        let solicit = |src: Ipv6Addr, hops: u8| {
            let mut msg = vec![ICMPV6_NEIGHBOR_SOLICIT, 0, 0, 0, 0, 0, 0, 0];
            msg.extend_from_slice(&gateway.octets());
            let mut p = vec![0x60, 0, 0, 0];
            p.extend_from_slice(&u16::try_from(msg.len()).unwrap().to_be_bytes());
            p.extend_from_slice(&[PROTO_ICMPV6, hops]);
            p.extend_from_slice(&src.octets());
            p.extend_from_slice(&gateway.octets());
            p.extend_from_slice(&msg);
            p
        };
        let asked = solicit(guest, 255);
        let ns = neighbor_solicit(&ipv6(&asked).unwrap()).unwrap();
        assert_eq!((ns.sender, ns.target), (guest, gateway));
        assert!(neighbor_solicit(&ipv6(&solicit(guest, 254)).unwrap()).is_none());
        assert!(neighbor_solicit(&ipv6(&solicit(Ipv6Addr::UNSPECIFIED, 255)).unwrap()).is_none());
        let mut out = Vec::new();
        f.neighbor_advert(&mut out, &ns);
        let e = eth(&out[VNET..]).unwrap();
        assert_eq!(
            (e.dst, e.src, e.kind),
            (f.guest_mac, f.gateway_mac, ETHERTYPE_IPV6)
        );
        let ip = ipv6(e.payload).unwrap();
        assert_eq!(
            (ip.src, ip.dst, ip.ttl, ip.proto),
            (IpAddr::V6(gateway), IpAddr::V6(guest), 255, PROTO_ICMPV6)
        );
        assert_eq!(icmpv6_checksum(gateway, guest, ip.payload), 0);
        assert_eq!(&ip.payload[..2], &[ICMPV6_NEIGHBOR_ADVERT, 0]);
        assert_eq!(ip.payload[4], 0xe0);
        assert_eq!(&ip.payload[8..24], &gateway.octets());
        assert_eq!(&ip.payload[24..32], &[2, 1, 2, 0, 0, 0, 0, 1]);
    }

    /// An ICMPv6 unreachable quotes as much of the packet as fits in IPv6's minimum MTU,
    /// and an echo reply carries the request's data, each under a checksum that sums to
    /// zero.
    #[test]
    fn icmpv6_errors_quote_what_fits() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let (gateway, guest, far): (IpAddr, IpAddr, IpAddr) = (
            "fd00:1::1".parse().unwrap(),
            "fd00:1::2".parse().unwrap(),
            "2001:db8::9".parse().unwrap(),
        );
        let big = vec![7u8; 4000];
        let mut sent = Vec::new();
        f.udp_headers(&mut sent, (guest, 5353), (far, 53), big.len());
        sent.extend_from_slice(&big);
        let sent_ip = ipv6(eth(&sent[VNET..]).unwrap().payload).unwrap();
        let mut out = Vec::new();
        f.icmp_unreachable(&mut out, gateway, guest, &sent_ip);
        let ip = ipv6(eth(&out[VNET..]).unwrap().payload).unwrap();
        assert_eq!(ip.packet.len(), IPV6_MIN_MTU);
        let (g, u): (Ipv6Addr, Ipv6Addr) = ("fd00:1::1".parse().unwrap(), "fd00:1::2".parse().unwrap());
        assert_eq!(icmpv6_checksum(g, u, ip.payload), 0);
        assert_eq!(&ip.payload[..2], &[ICMPV6_UNREACHABLE, ICMPV6_ADMIN_PROHIBITED]);
        assert_eq!(&ip.payload[8..], &sent_ip.packet[..IPV6_MIN_MTU - IPV6 - 8]);
        out.clear();
        f.icmp_echo_reply(&mut out, gateway, guest, b"\x00\x01\x00\x02ping");
        let ip = ipv6(eth(&out[VNET..]).unwrap().payload).unwrap();
        assert_eq!(icmpv6_checksum(g, u, ip.payload), 0);
        assert_eq!(ip.payload[0], ICMPV6_ECHO_REPLY);
        assert_eq!(&ip.payload[4..], b"\x00\x01\x00\x02ping");
    }
}
