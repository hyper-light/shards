//! The headers a guest's frames carry, read and written: virtio-net (virtio 1.3 §5.1.6),
//! Ethernet, ARP (RFC 826), IPv4 (RFC 791), ICMP (RFC 792), UDP (RFC 768) and TCP (RFC
//! 9293). A guest is untrusted: every read checks its length first, and what does not
//! parse is dropped, as a NIC drops a malformed frame.

use std::net::Ipv4Addr;

/// The virtio-net header's length, its `flags`' DATA_VALID bit, and where num_buffers is.
pub const VNET: usize = 12;
/// The checksums are good: the guest need not check them (VIRTIO_NET_HDR_F_DATA_VALID).
const VNET_DATA_VALID: u8 = 2;

pub const ETH: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const IPV4: usize = 20;
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

/// An IPv4 packet: its addresses, protocol and payload, options skipped. Fragments are
/// not reassembled: the guest's MTU is the device's, and a guest that fragments anyway
/// has them dropped, as a stateless NAT drops them.
#[derive(Debug)]
pub struct Ip<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub ttl: u8,
    /// The header as it came, options and all: what an ICMP error quotes.
    pub header: &'a [u8],
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
        src: ip_at(b, 12)?,
        dst: ip_at(b, 16)?,
        proto: *b.get(9)?,
        ttl: *b.get(8)?,
        header: b.get(..ihl)?,
        payload: b.get(ihl..total)?,
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

    /// An ICMP echo reply carrying `data` (identifier and sequence included).
    pub fn icmp_echo_reply(&self, out: &mut Vec<u8>, src: Ipv4Addr, dst: Ipv4Addr, data: &[u8]) {
        self.start(out, ETHERTYPE_IPV4, false);
        self.ipv4(out, src, dst, PROTO_ICMP, 4 + data.len());
        let at = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(data);
        let sum = checksum(out.get(at..).unwrap_or_default());
        if let Some(s) = out.get_mut(at + 2..at + 4) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
    }

    /// An ICMP destination unreachable of `code` (RFC 792) for datagram `ip`, quoting its
    /// header and the first 8 bytes of its data, as RFC 792 has one quote them.
    pub fn icmp_unreachable(&self, out: &mut Vec<u8>, src: Ipv4Addr, dst: Ipv4Addr, code: u8, ip: &Ip<'_>) {
        let data = ip.payload.get(..ip.payload.len().min(8)).unwrap_or_default();
        self.start(out, ETHERTYPE_IPV4, false);
        self.ipv4(out, src, dst, PROTO_ICMP, 8 + ip.header.len() + data.len());
        let at = out.len();
        out.extend_from_slice(&[ICMP_UNREACHABLE, code, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(ip.header);
        out.extend_from_slice(data);
        let sum = checksum(out.get(at..).unwrap_or_default());
        if let Some(s) = out.get_mut(at + 2..at + 4) {
            s.copy_from_slice(&sum.to_be_bytes());
        }
    }

    /// A UDP datagram, its checksum left to the guest's trust in its device.
    pub fn udp(&self, out: &mut Vec<u8>, src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), payload: &[u8]) {
        self.start(out, ETHERTYPE_IPV4, true);
        self.ipv4(out, src.0, dst.0, PROTO_UDP, 8 + payload.len());
        let len = u16::try_from(8 + payload.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&src.1.to_be_bytes());
        out.extend_from_slice(&dst.1.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(payload);
    }

    /// A TCP segment, its checksum left to the guest's trust in its device; `syn_options`
    /// adds the MSS and window scale a SYN-ACK offers.
    #[allow(clippy::too_many_arguments)]
    pub fn tcp(
        &self,
        out: &mut Vec<u8>,
        src: (Ipv4Addr, u16),
        dst: (Ipv4Addr, u16),
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        syn_options: Option<(u16, Option<u8>)>,
        payload: &[u8],
    ) {
        let opts = match syn_options {
            Some((_, Some(_))) => 8,
            Some((_, None)) => 4,
            None => 0,
        };
        self.start(out, ETHERTYPE_IPV4, true);
        self.ipv4(out, src.0, dst.0, PROTO_TCP, 20 + opts + payload.len());
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
        out.extend_from_slice(payload);
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
        let (a, b) = (Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(172, 17, 0, 2));
        f.tcp(
            &mut out,
            (a, 80),
            (b, 5000),
            7,
            9,
            SYN | ACK,
            1000,
            Some((65480, Some(7))),
            b"",
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

    /// A destination unreachable quotes the datagram's header and the first 8 bytes of its
    /// data, under a checksum that sums to zero (RFC 792).
    #[test]
    fn an_unreachable_quotes_the_datagram() {
        let f = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let (gateway, guest, far) = (
            Ipv4Addr::new(172, 17, 0, 1),
            Ipv4Addr::new(172, 17, 0, 2),
            Ipv4Addr::new(8, 8, 8, 8),
        );
        let mut sent = Vec::new();
        f.udp(
            &mut sent,
            (guest, 5353),
            (far, 53),
            b"a question longer than eight",
        );
        let sent_ip = ipv4(eth(&sent[VNET..]).unwrap().payload).unwrap();
        let mut out = Vec::new();
        f.icmp_unreachable(&mut out, gateway, guest, ADMIN_PROHIBITED, &sent_ip);
        let e = eth(&out[VNET..]).unwrap();
        let ip = ipv4(e.payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.proto), (gateway, guest, PROTO_ICMP));
        assert_eq!(checksum(ip.payload), 0);
        assert_eq!(&ip.payload[..2], &[ICMP_UNREACHABLE, ADMIN_PROHIBITED]);
        let quoted = &ip.payload[8..];
        assert_eq!(&quoted[..IPV4], sent_ip.header);
        assert_eq!(&quoted[IPV4..], &sent_ip.payload[..8]);
    }
}
