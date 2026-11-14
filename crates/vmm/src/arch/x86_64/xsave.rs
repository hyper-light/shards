//! A vCPU's XSAVE area as KVM's KVM_GET_XSAVE2 writes it and KVM_SET_XSAVE reads it: as many
//! bytes as KVM_CHECK_EXTENSION(KVM_CAP_XSAVE2) says, and "the offsets of the state save
//! areas in struct kvm_xsave follow the contents of CPUID leaf 0xD on the host" (Linux
//! Documentation/virt/kvm/api.rst §4.43, §4.134): the standard format's legacy region and
//! header first, then each state component at the offset, and of the size, its host's leaf
//! 0xD subleaf names (Intel SDM Vol. 1 §13.4; Vol. 2A CPUID leaf 0DH, ECX >= 2: EAX its size,
//! EBX its offset). So a saved area is laid into this host's as each component the guest
//! enabled or holds is laid out on both (migrate-07).

/// The legacy region's bytes, then the header's (SDM Vol. 1 §13.4.1-13.4.2): x87 and SSE
/// state live in the first, XSTATE_BV and XCOMP_BV in the second.
const LEGACY: usize = 512;
const HEADER: usize = 64;
/// XCOMP_BV's bit 63: the area is in the compacted format (SDM Vol. 1 §13.4.2), which KVM's
/// ABI never writes.
const COMPACTED: u64 = 1 << 63;
/// The first component leaf 0xD lays out: 0 (x87) and 1 (SSE) are the legacy region's.
const FIRST_LAID: u32 = 2;

/// Why a saved XSAVE area is not laid into this host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XsaveRefused {
    /// Shorter than its legacy region and header.
    Short { len: usize },
    /// In the compacted format.
    Compacted,
    /// A component its guest enabled or holds that a host's leaf 0xD does not lay out:
    /// the saved vCPU's (`here: false`) or this host's.
    Unlaid { component: u32, here: bool },
    /// A component at another offset, or of another size, here than where it was saved,
    /// each `(offset, size)`.
    Moved {
        component: u32,
        saved: (u32, u32),
        here: (u32, u32),
    },
    /// A component that ends at `end`, past the saved area (`here: false`) or this host's.
    Past { component: u32, end: u64, here: bool },
    /// This host's area could not be allocated.
    Unallocated { size: usize },
}

impl std::fmt::Display for XsaveRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let whose = |here: bool| if here { "this CPU's" } else { "the snapshot's" };
        match self {
            XsaveRefused::Short { len } => write!(f, "the snapshot's XSAVE area is {len} bytes, no area's"),
            XsaveRefused::Compacted => f.write_str("the snapshot's XSAVE area is in the compacted format"),
            XsaveRefused::Unlaid { component, here } => {
                write!(
                    f,
                    "XSAVE component {component} is not in {} XSAVE layout",
                    whose(*here)
                )
            }
            XsaveRefused::Moved {
                component,
                saved,
                here,
            } => write!(
                f,
                "XSAVE component {component} is {} bytes at {} in the snapshot, {} at {} on this CPU",
                saved.1, saved.0, here.1, here.0
            ),
            XsaveRefused::Past { component, end, here } => write!(
                f,
                "XSAVE component {component} ends at {end}, past {} XSAVE area",
                whose(*here)
            ),
            XsaveRefused::Unallocated { size } => write!(f, "no memory for an XSAVE area of {size} bytes"),
        }
    }
}

impl std::error::Error for XsaveRefused {}

/// Component `component`'s `(offset, size)` in the XSAVE layout of the CPU whose CPUID is
/// `cpuid`: leaf 0xD subleaf `component`'s EBX and EAX.
fn laid(cpuid: &[[u32; 7]], component: u32) -> Option<(u32, u32)> {
    cpuid
        .iter()
        .find(|l| l[0] == 0xd && l[1] == component)
        .map(|l| (l[4], l[3]))
        .filter(|&(_, size)| size > 0)
}

/// `saved`, the XSAVE area of a vCPU whose CPUID is `saved_cpuid` (its host's leaf 0xD, as
/// KVM offered it) and whose XCR0 is `xcr0`, laid into an area of `size` bytes for this host,
/// whose CPUID is `here_cpuid`: the legacy region and header as they are, each component the
/// guest enabled (XCR0) or holds (XSTATE_BV) at its offset, and zeros elsewhere, which a
/// component outside XSTATE_BV is restored from as its initial state (SDM Vol. 1 §13.8.1).
/// Refused where any such component is laid out otherwise, or nowhere, on either host.
pub fn lay_into(
    saved: &[u8],
    saved_cpuid: &[[u32; 7]],
    xcr0: u64,
    here_cpuid: &[[u32; 7]],
    size: usize,
) -> Result<Vec<u8>, XsaveRefused> {
    let head = LEGACY + HEADER;
    let word = |at: usize| {
        saved
            .get(at..at + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
    };
    let (Some(xstate_bv), Some(xcomp_bv)) = (word(LEGACY), word(LEGACY + 8)) else {
        return Err(XsaveRefused::Short { len: saved.len() });
    };
    if saved.len() < head || size < head {
        return Err(XsaveRefused::Short {
            len: saved.len().min(size),
        });
    }
    if xcomp_bv & COMPACTED != 0 {
        return Err(XsaveRefused::Compacted);
    }
    let mut ranges = Vec::new();
    let wanted = xcr0 | xstate_bv;
    for component in (FIRST_LAID..u64::BITS).filter(|&c| wanted & (1 << c) != 0) {
        let there = laid(saved_cpuid, component).ok_or(XsaveRefused::Unlaid {
            component,
            here: false,
        })?;
        let here = laid(here_cpuid, component).ok_or(XsaveRefused::Unlaid {
            component,
            here: true,
        })?;
        if there != here {
            return Err(XsaveRefused::Moved {
                component,
                saved: there,
                here,
            });
        }
        let (at, len) = (u64::from(here.0), u64::from(here.1));
        let end = at + len;
        for (area, is_here) in [(saved.len(), false), (size, true)] {
            if end > area as u64 {
                return Err(XsaveRefused::Past {
                    component,
                    end,
                    here: is_here,
                });
            }
        }
        // Within both areas, whose lengths are usizes.
        ranges.push((at as usize, end as usize));
    }
    let mut area = Vec::new();
    area.try_reserve_exact(size)
        .map_err(|_| XsaveRefused::Unallocated { size })?;
    area.resize(size, 0);
    for (from, to) in std::iter::once((0, head)).chain(ranges) {
        if let (Some(into), Some(bytes)) = (area.get_mut(from..to), saved.get(from..to)) {
            into.copy_from_slice(bytes);
        }
    }
    Ok(area)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Leaf 0xD's subleaves, `[function, index, flags, size, offset, ..]`, of an AVX-512 host
    /// with PKRU: AVX, the opmask, ZMM_Hi256, Hi16_ZMM, PKRU, in that order (a fixture,
    /// no host's recorded table); with AMX's TILECFG and TILEDATA where `amx`, the area then
    /// 11008 bytes, as an AMX host's KVM_CAP_XSAVE2 says (hv/kvm/state.rs's fixture).
    fn layout(amx: bool) -> Vec<[u32; 7]> {
        let mut leaves = vec![
            [0xd, 2, 1, 256, 576, 0, 0],
            [0xd, 5, 1, 64, 1088, 0, 0],
            [0xd, 6, 1, 512, 1152, 0, 0],
            [0xd, 7, 1, 1024, 1664, 0, 0],
            [0xd, 9, 1, 8, 2688, 0, 0],
        ];
        if amx {
            leaves.extend([[0xd, 17, 1, 64, 2752, 0, 0], [0xd, 18, 1, 8192, 2816, 0, 0]]);
        }
        leaves
    }

    /// x87, SSE, AVX, the three AVX-512 components and PKRU.
    const XCR0: u64 = 0x2e7;

    /// A non-AMX host's 4096-byte area, each component's bytes its number, XSTATE_BV its
    /// XCR0's: what such a host's KVM_GET_XSAVE writes of a guest that used them all.
    fn saved() -> Vec<u8> {
        let mut area = vec![0xee; 4096];
        area[..LEGACY].fill(1);
        area[LEGACY..LEGACY + 8].copy_from_slice(&XCR0.to_le_bytes());
        area[LEGACY + 8..LEGACY + HEADER].fill(0);
        for l in layout(false) {
            let (at, len) = (l[4] as usize, l[3] as usize);
            area[at..at + len].fill(l[1] as u8);
        }
        area
    }

    /// migrate-07: a 4096-byte area saved on a non-AMX host is laid into an AMX host's
    /// 11008-byte one, each component the guest enabled where both hosts lay it, the legacy
    /// region and header as saved, and AMX's components, which the guest never enabled,
    /// zeros: their initial state.
    #[test]
    fn a_smaller_hosts_xsave_area_is_laid_into_a_larger_ones() {
        let saved = saved();
        let area = lay_into(&saved, &layout(false), XCR0, &layout(true), 11008).unwrap();
        assert_eq!(area.len(), 11008);
        assert_eq!(&area[..LEGACY + HEADER], &saved[..LEGACY + HEADER]);
        for l in layout(false) {
            let (at, len) = (l[4] as usize, l[3] as usize);
            assert!(
                area[at..at + len].iter().all(|&b| b == l[1] as u8),
                "component {}",
                l[1]
            );
        }
        assert!(
            area[2752..].iter().all(|&b| b == 0),
            "AMX's components are their initial state"
        );
        // What lies between components in the saved area is no component's, and not kept.
        assert!(area[2696..2752].iter().all(|&b| b == 0));
    }

    /// A component laid out elsewhere, or nowhere, on either host, and areas that are none
    /// of the standard format's, are refused typed.
    #[test]
    fn an_xsave_area_laid_out_otherwise_is_refused() {
        let saved = saved();
        let mut moved = layout(true);
        moved[4][4] = 2696;
        assert_eq!(
            lay_into(&saved, &layout(false), XCR0, &moved, 11008),
            Err(XsaveRefused::Moved {
                component: 9,
                saved: (2688, 8),
                here: (2696, 8)
            })
        );
        let mut without = layout(true);
        without.retain(|l| l[1] != 9);
        assert_eq!(
            lay_into(&saved, &layout(false), XCR0, &without, 11008),
            Err(XsaveRefused::Unlaid {
                component: 9,
                here: true
            })
        );
        // A component held (XSTATE_BV) that its own host does not lay out.
        let mut tiles = saved.clone();
        tiles[LEGACY..LEGACY + 8].copy_from_slice(&(XCR0 | 1 << 18).to_le_bytes());
        assert_eq!(
            lay_into(&tiles, &layout(false), XCR0, &layout(true), 11008),
            Err(XsaveRefused::Unlaid {
                component: 18,
                here: false
            })
        );
        // One the saved area is too short for.
        assert_eq!(
            lay_into(&saved[..2048], &layout(false), XCR0, &layout(true), 11008),
            Err(XsaveRefused::Past {
                component: 7,
                end: 2688,
                here: false
            })
        );
        let mut compacted = saved.clone();
        compacted[LEGACY + 8..LEGACY + 16].copy_from_slice(&COMPACTED.to_le_bytes());
        assert_eq!(
            lay_into(&compacted, &layout(false), XCR0, &layout(true), 11008),
            Err(XsaveRefused::Compacted)
        );
        assert_eq!(
            lay_into(&saved[..LEGACY], &layout(false), XCR0, &layout(true), 11008),
            Err(XsaveRefused::Short { len: LEGACY })
        );
        assert_eq!(
            lay_into(&saved[..LEGACY + 16], &layout(false), 0, &layout(true), 11008),
            Err(XsaveRefused::Short { len: LEGACY + 16 })
        );
    }
}
