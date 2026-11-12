//! The engine ring (D230): a run's engine connection's bytes through rings in its guest's
//! RAM, which init's relay offers in its hello on [`ENGINE_PORT`](crate::run::ENGINE_PORT)
//! and the VM's vsock device moves to and from the connection's host socket.
//!
//! A slot is one page of positions and waiting flags, then the ring the guest writes for
//! the host, then the ring the host writes for the guest, each `ring` bytes. The relay
//! names the slot's pages by guest-physical address in its offer; the device answers with
//! a [`Reply`] before anything else of the connection. Attached, every later byte either
//! side sends on the connection is a [`DOORBELL`].
//!
//! Every integer is little-endian. Positions are free-running byte counts: a ring holds
//! `head - tail` bytes, at offsets `position % ring`.

use alloc::vec::Vec;

/// The protocol's version; an offer of another is refused.
pub const VERSION: u16 = 1;
/// An offer's first four bytes.
pub const MAGIC: [u8; 4] = *b"RING";
/// The run's setup entry that says its host takes rings: without it the relay offers none.
pub const SPEC: &[u8] = b"engine-ring=1";

/// The unit an offer's extents are in and aligned to: the smallest page of every
/// supported guest (x86_64's, and arm64's 4 KiB granule, which the pinned kernel uses).
pub const PAGE: u64 = 4096;
/// The slot's control page.
pub const CONTROL: u64 = PAGE;
/// The distance between two fields of the control page: Apple silicon's cache line is 128
/// bytes (`hw.cachelinesize`), and x86's spatial prefetcher fetches 64-byte lines in pairs,
/// so no two fields either side writes share one.
pub const STRIDE: u64 = 128;

/// The control page's fields, by their offset. Each position is a u64, each flag a u32.
pub mod field {
    use super::STRIDE;
    /// Bytes the relay has put in the ring for the host.
    pub const TO_HOST_HEAD: u64 = 0;
    /// Bytes the device has taken from it.
    pub const TO_HOST_TAIL: u64 = STRIDE;
    /// Bytes the device has put in the ring for the guest.
    pub const TO_GUEST_HEAD: u64 = 2 * STRIDE;
    /// Bytes the relay has taken from it.
    pub const TO_GUEST_TAIL: u64 = 3 * STRIDE;
    /// The device waits for bytes in the ring for the host.
    pub const HOST_WAITS_DATA: u64 = 4 * STRIDE;
    /// The relay waits for room in the ring for the host.
    pub const GUEST_WAITS_ROOM: u64 = 5 * STRIDE;
    /// The relay waits for bytes in the ring for the guest.
    pub const GUEST_WAITS_DATA: u64 = 6 * STRIDE;
    /// The device waits for room in the ring for the guest.
    pub const HOST_WAITS_ROOM: u64 = 7 * STRIDE;
}

/// The smallest ring: a page.
pub const MIN_RING: u32 = PAGE as u32;
/// The largest ring, and the one the relay offers: the bandwidth-delay product of the
/// engine path's target, 10x the faster relayed baseline (256 MiB in 12.36 ms, 21.7 GB/s)
/// over one way of the relayed 64-byte round trip at p99 (129 / 2 µs), 1.40 MB, rounded up
/// to a power of two (D230; docs/research/engine-ring-channel-decision-v1.md section 7).
pub const MAX_RING: u32 = 2 << 20;

/// The byte either side sends the other on an attached connection: look at the rings.
pub const DOORBELL: u8 = 0x01;
/// The byte the relay sends once its client has ended its writes, after its last bytes are
/// in the ring for the host: the device passes them on, then ends the host socket's
/// writes. A vsock shutdown would end the relay's doorbells with them, and a client that
/// has ended its writes may still be reading a long answer, whose ring the relay must
/// still be able to ring the device about. Any byte but these two is a broken rule.
pub const END: u8 = 0x02;

/// An offer's fixed part: magic, version, a reserved u16, the ring's length and the count
/// of extents.
pub const OFFER_HEADER: usize = 16;
/// An extent: its guest-physical address and its length.
pub const EXTENT: usize = 16;
/// A reply's length.
pub const REPLY: usize = 8;

/// A slot's bytes for rings of `ring` bytes: the control page and both rings.
pub const fn slot_len(ring: u32) -> u64 {
    CONTROL + 2 * ring as u64
}

/// The most extents an offer of rings of `ring` bytes may hold: a slot's pages, none merged.
pub const fn most_extents(ring: u32) -> u64 {
    slot_len(ring) / PAGE
}

/// The longest offer any relay may make: of the largest rings.
pub const MAX_OFFER: usize = OFFER_HEADER + EXTENT * most_extents(MAX_RING) as usize;

/// What a relay offers: its rings' length and the slot's pages, in the slot's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub ring: u32,
    pub extents: Vec<Extent>,
}

/// A run of the slot's pages: where they are in the guest's physical memory, and how many
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub gpa: u64,
    pub len: u64,
}

/// Why an offer cannot be taken as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// Shorter than its fixed part, or than the extents it counts, or longer.
    Length,
    /// Not [`MAGIC`].
    Magic,
    /// A version other than [`VERSION`].
    Version,
    /// A reserved field not zero.
    Reserved,
    /// A ring that is not a power of two from [`MIN_RING`] to [`MAX_RING`].
    Ring,
    /// No extents, or more than a slot has pages.
    Count,
    /// An extent that is not whole pages on a page's boundary, or that runs past the end
    /// of the guest's addresses.
    Extent,
    /// Extents whose lengths do not sum to the slot's.
    Size,
}

impl Offer {
    /// The offer's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(OFFER_HEADER + EXTENT * self.extents.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.ring.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.extents.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        for e in &self.extents {
            out.extend_from_slice(&e.gpa.to_le_bytes());
            out.extend_from_slice(&e.len.to_le_bytes());
        }
        out
    }

    /// The offer `bytes` hold, whole and nothing past it, checked as far as the bytes alone
    /// say: the extents' place in the guest's memory is the device's to check.
    pub fn parse(bytes: &[u8]) -> Result<Offer, Malformed> {
        let (head, rest) = bytes
            .split_first_chunk::<OFFER_HEADER>()
            .ok_or(Malformed::Length)?;
        let [m0, m1, m2, m3, v0, v1, r0, r1, g0, g1, g2, g3, c0, c1, c2, c3] = *head;
        if [m0, m1, m2, m3] != MAGIC {
            return Err(Malformed::Magic);
        }
        if u16::from_le_bytes([v0, v1]) != VERSION {
            return Err(Malformed::Version);
        }
        if u16::from_le_bytes([r0, r1]) != 0 {
            return Err(Malformed::Reserved);
        }
        let ring = u32::from_le_bytes([g0, g1, g2, g3]);
        if !ring.is_power_of_two() || !(MIN_RING..=MAX_RING).contains(&ring) {
            return Err(Malformed::Ring);
        }
        let count = u64::from(u32::from_le_bytes([c0, c1, c2, c3]));
        if count == 0 || count > most_extents(ring) {
            return Err(Malformed::Count);
        }
        let (pairs, []) = rest.as_chunks::<EXTENT>() else {
            return Err(Malformed::Length);
        };
        if pairs.len() as u64 != count {
            return Err(Malformed::Length);
        }
        let mut extents = Vec::with_capacity(pairs.len());
        let mut total = 0u64;
        for pair in pairs {
            let (Some(gpa), Some(len)) = (pair.first_chunk::<8>(), pair.last_chunk::<8>()) else {
                return Err(Malformed::Length);
            };
            let (gpa, len) = (u64::from_le_bytes(*gpa), u64::from_le_bytes(*len));
            if len == 0
                || !gpa.is_multiple_of(PAGE)
                || !len.is_multiple_of(PAGE)
                || gpa.checked_add(len).is_none()
            {
                return Err(Malformed::Extent);
            }
            total = total.checked_add(len).ok_or(Malformed::Size)?;
            extents.push(Extent { gpa, len });
        }
        if total != slot_len(ring) {
            return Err(Malformed::Size);
        }
        Ok(Offer { ring, extents })
    }
}

/// The device's answer to an offer: the connection's first bytes to the guest after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The rings carry the connection's bytes from here on.
    Attached,
    /// The connection goes on over plain vsock, for this reason.
    Refused(Refusal),
}

/// Why the device took no ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The offer's bytes do not say a slot ([`Malformed`]).
    Malformed,
    /// An extent is not the guest's RAM.
    NotRam,
    /// A page of the slot is another connection's ring's still.
    InUse,
}

const REPLY_MAGIC: [u8; 3] = *b"RNG";

impl Reply {
    pub fn encode(self) -> [u8; REPLY] {
        let status = match self {
            Reply::Attached => 0,
            Reply::Refused(Refusal::Malformed) => 1,
            Reply::Refused(Refusal::NotRam) => 2,
            Reply::Refused(Refusal::InUse) => 3,
        };
        let [a, b, c] = REPLY_MAGIC;
        [a, b, c, status, 0, 0, 0, 0]
    }

    /// The reply `bytes` say, or `None` where they are none.
    pub fn parse(bytes: [u8; REPLY]) -> Option<Reply> {
        let [a, b, c, status, 0, 0, 0, 0] = bytes else {
            return None;
        };
        if [a, b, c] != REPLY_MAGIC {
            return None;
        }
        match status {
            0 => Some(Reply::Attached),
            1 => Some(Reply::Refused(Refusal::Malformed)),
            2 => Some(Reply::Refused(Refusal::NotRam)),
            3 => Some(Reply::Refused(Refusal::InUse)),
            _ => None,
        }
    }
}

/// Merges the frame numbers of a slot's pages, in the slot's order, into extents: a page
/// that follows the last in the guest's memory lengthens it. `page` is the guest's page
/// size; frames are its pages' numbers (`/proc/self/pagemap`).
pub fn extents(frames: &[u64], page: u64) -> Option<Vec<Extent>> {
    let mut out: Vec<Extent> = Vec::new();
    for &frame in frames {
        let gpa = frame.checked_mul(page)?;
        match out.last_mut() {
            Some(last) if last.gpa.checked_add(last.len) == Some(gpa) => {
                last.len = last.len.checked_add(page)?;
            }
            _ => out.push(Extent { gpa, len: page }),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn offer(ring: u32) -> Offer {
        // A slot whose pages are two runs, the first half its pages and the rest elsewhere.
        let half = slot_len(ring) / PAGE / 2 * PAGE;
        Offer {
            ring,
            extents: vec![
                Extent {
                    gpa: 0x1000_0000,
                    len: half,
                },
                Extent {
                    gpa: 0x4000_0000,
                    len: slot_len(ring) - half,
                },
            ],
        }
    }

    #[test]
    fn an_offer_reads_back_as_it_was_written() {
        for ring in [MIN_RING, 64 << 10, MAX_RING] {
            let o = offer(ring);
            assert_eq!(Offer::parse(&o.encode()), Ok(o));
        }
    }

    #[test]
    fn an_offer_is_refused_for_each_way_it_can_be_wrong() {
        let good = offer(MAX_RING).encode();
        let with = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            b.splice(at..at + bytes.len(), bytes.iter().copied());
            Offer::parse(&b)
        };
        assert_eq!(Offer::parse(&good[..OFFER_HEADER - 1]), Err(Malformed::Length));
        assert_eq!(Offer::parse(&good[..good.len() - 1]), Err(Malformed::Length));
        let mut longer = good.clone();
        longer.push(0);
        assert_eq!(Offer::parse(&longer), Err(Malformed::Length));
        assert_eq!(with(0, b"GNIR"), Err(Malformed::Magic));
        assert_eq!(with(4, &2u16.to_le_bytes()), Err(Malformed::Version));
        assert_eq!(with(6, &1u16.to_le_bytes()), Err(Malformed::Reserved));
        for ring in [0u32, MIN_RING - 1, MIN_RING + 1, MAX_RING * 2, 3 << 12] {
            assert_eq!(with(8, &ring.to_le_bytes()), Err(Malformed::Ring), "{ring}");
        }
        assert_eq!(with(12, &0u32.to_le_bytes()), Err(Malformed::Count));
        let too_many = most_extents(MAX_RING) as u32 + 1;
        assert_eq!(with(12, &too_many.to_le_bytes()), Err(Malformed::Count));
        // An extent off a page's boundary, not whole pages, empty, or past the addresses.
        assert_eq!(with(16, &0x1000_0001u64.to_le_bytes()), Err(Malformed::Extent));
        assert_eq!(with(24, &4095u64.to_le_bytes()), Err(Malformed::Extent));
        assert_eq!(with(24, &0u64.to_le_bytes()), Err(Malformed::Extent));
        assert_eq!(with(16, &(u64::MAX - 4095).to_le_bytes()), Err(Malformed::Extent));
        // Lengths that sum to more or less than the slot, or past a u64.
        let half = slot_len(MAX_RING) / PAGE / 2 * PAGE;
        assert_eq!(with(24, &(half + PAGE).to_le_bytes()), Err(Malformed::Size));
        let mut huge = good.clone();
        huge.splice(
            16..32,
            [0u8; 8].into_iter().chain((u64::MAX - 4095).to_le_bytes()),
        );
        huge.splice(
            32..48,
            [0u8; 8].into_iter().chain((u64::MAX - 4095).to_le_bytes()),
        );
        assert_eq!(Offer::parse(&huge), Err(Malformed::Size));
    }

    #[test]
    fn the_longest_offer_is_a_slot_of_single_pages() {
        let ring = MAX_RING;
        let pages = most_extents(ring);
        let o = Offer {
            ring,
            extents: (0..pages)
                .map(|i| Extent {
                    gpa: (2 * i) * PAGE,
                    len: PAGE,
                })
                .collect(),
        };
        let bytes = o.encode();
        assert_eq!(bytes.len(), MAX_OFFER);
        assert_eq!(Offer::parse(&bytes), Ok(o));
    }

    #[test]
    fn replies_read_back_and_nothing_else_reads_as_one() {
        for r in [
            Reply::Attached,
            Reply::Refused(Refusal::Malformed),
            Reply::Refused(Refusal::NotRam),
            Reply::Refused(Refusal::InUse),
        ] {
            assert_eq!(Reply::parse(r.encode()), Some(r));
        }
        assert_eq!(Reply::parse(*b"RNG\x04\0\0\0\0"), None);
        assert_eq!(Reply::parse(*b"RNG\0\0\0\0\x01"), None);
        assert_eq!(Reply::parse(*b"XNG\0\0\0\0\0"), None);
        // No doorbell or end is a reply's first byte, so they never read as each other.
        assert!(![DOORBELL, END].contains(&Reply::Attached.encode()[0]));
    }

    #[test]
    fn frames_that_follow_each_other_are_one_extent() {
        assert_eq!(
            extents(&[10, 11, 12, 20, 21, 5], PAGE),
            Some(vec![
                Extent {
                    gpa: 10 * PAGE,
                    len: 3 * PAGE
                },
                Extent {
                    gpa: 20 * PAGE,
                    len: 2 * PAGE
                },
                Extent {
                    gpa: 5 * PAGE,
                    len: PAGE
                },
            ])
        );
        assert_eq!(extents(&[u64::MAX], PAGE), None);
    }

    #[test]
    fn the_control_pages_fields_fit_it_each_on_a_line_of_its_own() {
        let fields = [
            field::TO_HOST_HEAD,
            field::TO_HOST_TAIL,
            field::TO_GUEST_HEAD,
            field::TO_GUEST_TAIL,
            field::HOST_WAITS_DATA,
            field::GUEST_WAITS_ROOM,
            field::GUEST_WAITS_DATA,
            field::HOST_WAITS_ROOM,
        ];
        for (i, f) in fields.iter().enumerate() {
            assert_eq!(*f, i as u64 * STRIDE);
            assert!(f + 8 <= CONTROL);
        }
    }
}
