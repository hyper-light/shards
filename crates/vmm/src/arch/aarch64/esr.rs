//! Exception Syndrome Register decoding for the exits a VMM handles
//! (Arm ARM D19.2.40; field layouts in docs/research/hvf-arm64-kvm-ground-truth.md §2).

pub const EC_WFX: u32 = 0x01;
pub const EC_HVC64: u32 = 0x16;
pub const EC_SMC64: u32 = 0x17;
pub const EC_SYS64: u32 = 0x18;
pub const EC_IABT_LOW: u32 = 0x20;
pub const EC_DABT_LOW: u32 = 0x24;

pub fn ec(esr: u64) -> u32 {
    ((esr >> 26) & 0x3f) as u32
}

/// Whether a data abort was a write (WnR), for an instruction syndrome or not. A stage 1
/// table walk's abort is a write when the walk would update a descriptor. A cache
/// maintenance instruction's abort always reports WnR set ([`cache_maintenance`]), and is
/// no write.
pub fn writes(esr: u64) -> bool {
    esr & (1 << 6) != 0
}

/// Whether a data abort came from a cache maintenance or address translation instruction
/// (ISS bit 8, CM), such as the `DC CVAU` a guest kernel runs over a page before it
/// executes from it. Its WnR is always set, and it carries no instruction syndrome: it
/// moves no data, so it is neither emulated as an access nor a write. KVM skips one that
/// reaches no guest memory (arch/arm64/kvm/mmu.c, `kvm_handle_guest_abort`).
pub fn cache_maintenance(esr: u64) -> bool {
    esr & (1 << 8) != 0
}

/// Instruction length: 4 bytes when IL is set, else 2 (only for T32, never here).
pub fn instr_len(esr: u64) -> u64 {
    if esr & (1 << 25) != 0 { 4 } else { 2 }
}

/// A decoded data abort with a valid instruction syndrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAbort {
    /// Access size in bytes (1, 2, 4, 8).
    pub size: usize,
    pub write: bool,
    /// Transfer register; 31 means XZR/WZR.
    pub reg: u8,
    /// Loads sign-extend to the register width.
    pub sign_extend: bool,
    /// 64-bit register (`X`) rather than 32-bit (`W`).
    pub sixty_four: bool,
}

/// Returns `None` when ISV = 0 (no syndrome; the access cannot be emulated from ESR).
pub fn data_abort(esr: u64) -> Option<DataAbort> {
    if esr & (1 << 24) == 0 {
        return None;
    }
    Some(DataAbort {
        size: 1 << ((esr >> 22) & 3),
        write: esr & (1 << 6) != 0,
        reg: ((esr >> 16) & 0x1f) as u8,
        sign_extend: esr & (1 << 21) != 0,
        sixty_four: esr & (1 << 15) != 0,
    })
}

impl DataAbort {
    /// Converts `size` bytes read from a device into the register value the load
    /// instruction would produce (sign extension and W-register truncation).
    pub fn load_value(&self, raw: u64) -> u64 {
        let bits = (self.size.clamp(1, 8) * 8) as u32;
        let mut v = if bits == 64 {
            raw
        } else {
            raw & ((1u64 << bits) - 1)
        };
        if self.sign_extend && bits < 64 && v & (1 << (bits - 1)) != 0 {
            v |= !0u64 << bits;
        }
        if !self.sixty_four {
            v &= 0xffff_ffff;
        }
        v
    }
}

/// A trapped MSR/MRS (EC 0x18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysRegAccess {
    /// op0:op1:CRn:CRm:op2 packed like `arch::aarch64::sysreg::enc`.
    pub encoding: u16,
    pub reg: u8,
    pub read: bool,
}

pub fn sysreg_access(esr: u64) -> SysRegAccess {
    let iss = esr & 0x1ff_ffff;
    let op0 = ((iss >> 20) & 3) as u16;
    let op2 = ((iss >> 17) & 7) as u16;
    let op1 = ((iss >> 14) & 7) as u16;
    let crn = ((iss >> 10) & 0xf) as u16;
    let crm = ((iss >> 1) & 0xf) as u16;
    SysRegAccess {
        encoding: super::sysreg::enc(op0, op1, crn, crm, op2),
        reg: ((iss >> 5) & 0x1f) as u8,
        read: iss & 1 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_measured_syndromes() {
        // Syndromes observed on the M5 Max (platform-measurements.md M4):
        // `str w2, [x1]` -> 0x93820046 and `ldr w2, [x1]` -> 0x93820006.
        let w = data_abort(0x9382_0046).unwrap();
        assert_eq!(
            (w.size, w.write, w.reg, w.sign_extend, w.sixty_four),
            (4, true, 2, false, false)
        );
        assert_eq!(ec(0x9382_0046), EC_DABT_LOW);
        assert_eq!(instr_len(0x9382_0046), 4);
        let r = data_abort(0x9382_0006).unwrap();
        assert!(!r.write);
        assert_eq!(data_abort(0x9200_0046), None); // ISV clear
    }

    /// A guest kernel's `DC CVAU` on a page taken away at stage 2, on the M5 Max: a level 3
    /// translation fault with CM and WnR set and no instruction syndrome.
    #[test]
    fn cache_maintenance_is_told_from_a_write() {
        let dc = 0x9200_0147;
        assert_eq!(ec(dc), EC_DABT_LOW);
        assert!(cache_maintenance(dc) && writes(dc));
        assert_eq!(data_abort(dc), None);
        assert!(!cache_maintenance(0x9382_0046), "a store");
    }

    #[test]
    fn load_value_extends_like_hardware() {
        let ldrsb_w = DataAbort {
            size: 1,
            write: false,
            reg: 0,
            sign_extend: true,
            sixty_four: false,
        };
        assert_eq!(ldrsb_w.load_value(0x80), 0xffff_ff80);
        let ldrsb_x = DataAbort {
            sixty_four: true,
            ..ldrsb_w
        };
        assert_eq!(ldrsb_x.load_value(0x80), 0xffff_ffff_ffff_ff80);
        let ldrh = DataAbort {
            size: 2,
            write: false,
            reg: 0,
            sign_extend: false,
            sixty_four: false,
        };
        assert_eq!(ldrh.load_value(0xdead_beef), 0xbeef);
        let ldr_x = DataAbort {
            size: 8,
            write: false,
            reg: 0,
            sign_extend: false,
            sixty_four: true,
        };
        assert_eq!(ldr_x.load_value(u64::MAX), u64::MAX);
    }

    #[test]
    fn decodes_sysreg_trap() {
        // mrs x3, MDCCINT_EL1 (op0=2 op1=0 CRn=0 CRm=2 op2=0): ISS = op0<<20|op2<<17|op1<<14|CRn<<10|Rt<<5|CRm<<1|dir
        let iss = (2 << 20) | (3 << 5) | (2 << 1) | 1;
        let a = sysreg_access(((EC_SYS64 as u64) << 26) | (1 << 25) | iss);
        assert_eq!(
            a,
            SysRegAccess {
                encoding: super::super::sysreg::enc(2, 0, 0, 2, 0),
                reg: 3,
                read: true
            }
        );
    }
}
