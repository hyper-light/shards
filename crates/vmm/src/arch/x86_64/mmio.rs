//! MMIO instructions decoded, for a backend whose hypervisor hands the VMM an MMIO exit's
//! instruction bytes and not its effect (WHP: the bytes, the GPA and the direction, and no
//! length, PM M198). Decoded are the instructions Linux decodes for its own guests' MMIO
//! under TDX and SEV-ES, which its accessors compile to (arch/x86/lib/insn-eval.c,
//! `insn_decode_mmio`; readb..writeq, arch/x86/include/asm/io.h): MOV to and from memory
//! (88, 89, 8A, 8B), MOV of an immediate (C6 /0, C7 /0), MOVZX and MOVSX of a byte or a word
//! (0F B6, B7, BE, BF); and shards-init's own control-page stores and loads, which are
//! volatile MOVs (init/src/linux.rs). In 64-bit mode and in 32-bit protected mode with
//! 32-bit addressing. Anything else, MOVS among it, is not decoded: the backend refuses it
//! rather than guess its effect. Ground truth: Intel SDM Vol. 2 §2.1-2.2 (prefixes, REX,
//! ModR/M, SIB, displacements), Vol. 2B MOV, MOVZX, MOVSX.

/// A general-purpose register by its encoding: RAX 0, RCX 1, RDX 2, RBX 3, RSP 4, RBP 5,
/// RSI 6, RDI 7, R8-R15 8-15.
pub type Gpr = u8;

/// How a load's value goes into its register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extend {
    /// As loaded: the access is the register's own size.
    None,
    /// Zero-extended to the register's size (MOVZX).
    Zero,
    /// Sign-extended to the register's size (MOVSX).
    Sign,
}

/// What an instruction does to MMIO, `size` bytes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads `size` bytes into register `reg`, which is `dest` bytes wide: AH, CH, DH or BH
    /// where `high`.
    Load {
        reg: Gpr,
        high: bool,
        size: u8,
        dest: u8,
        extend: Extend,
    },
    /// Writes `size` bytes of register `reg`: AH, CH, DH or BH where `high`.
    Store { reg: Gpr, high: bool, size: u8 },
    /// Writes `size` bytes of `value`.
    StoreImm { value: u64, size: u8 },
}

/// A decoded instruction: what it does, and its length in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    pub access: Access,
    pub len: u8,
}

/// The processor mode an instruction ran in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 64-bit mode (IA32_EFER.LMA and CS.L).
    Long,
    /// 32-bit protected mode (CS.D, not CS.L).
    Protected32,
}

/// The longest instruction there is (Intel SDM Vol. 2 §2.1).
const MAX_LEN: usize = 15;

/// Decodes the instruction at the start of `bytes`; `None` for anything not described in
/// the module's documentation, or bytes that end before it does.
pub fn decode(bytes: &[u8], mode: Mode) -> Option<Decoded> {
    let bytes = bytes.get(..bytes.len().min(MAX_LEN))?;
    let mut at = 0usize;
    let mut operand16 = false;
    // Legacy prefixes, in any order. LOCK makes a MOV undefined (#UD); REP and REPNE change
    // what 0F B6-BF may be on other opcodes, and none of these takes them: all refused.
    loop {
        match *bytes.get(at)? {
            0x66 => operand16 = true,
            // 32-bit addressing in 64-bit mode, whose ModR/M and SIB forms are 64-bit's; in
            // 32-bit mode, 16-bit addressing, another ModR/M table, never Linux's for MMIO.
            0x67 if mode == Mode::Long => {}
            0x67 => return None,
            0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {}
            0xF0 | 0xF2 | 0xF3 => return None,
            _ => break,
        }
        at += 1;
    }
    // REX, in 64-bit mode, immediately before the opcode.
    let mut rex = 0u8;
    if mode == Mode::Long && matches!(bytes.get(at)?, 0x40..=0x4F) {
        rex = *bytes.get(at)?;
        at += 1;
    }
    let rex_w = rex & 0x8 != 0;
    let rex_r = (rex & 0x4) << 1;
    let operand = if rex_w {
        8
    } else if operand16 {
        2
    } else {
        4
    };
    let opcode = *bytes.get(at)?;
    at += 1;
    let two_byte = opcode == 0x0F;
    let second = if two_byte {
        let b = *bytes.get(at)?;
        at += 1;
        Some(b)
    } else {
        None
    };
    let modrm = *bytes.get(at)?;
    at += 1;
    let md = modrm >> 6;
    let reg_field = (modrm >> 3) & 7;
    let rm = modrm & 7;
    // A register operand is not memory, and so no MMIO.
    if md == 3 {
        return None;
    }
    // The SIB byte and the displacement: their lengths alone matter, the GPA being the exit's.
    if rm == 4 {
        let sib = *bytes.get(at)?;
        at += 1;
        if md == 0 && sib & 7 == 5 {
            at += 4;
        }
    } else if md == 0 && rm == 5 {
        // RIP-relative in 64-bit mode, an absolute disp32 in 32-bit mode.
        at += 4;
    }
    at += match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    let reg = reg_field | rex_r;
    // An 8-bit register operand: AH, CH, DH and BH where no REX prefix is present, else
    // SPL, BPL, SIL, DIL and R8B-R15B.
    let byte_reg = |r: Gpr| -> (Gpr, bool) {
        if rex == 0 && (4..8).contains(&r) {
            (r - 4, true)
        } else {
            (r, false)
        }
    };
    let access = match (opcode, second) {
        (0x88, None) => {
            let (reg, high) = byte_reg(reg);
            Access::Store { reg, high, size: 1 }
        }
        (0x89, None) => Access::Store {
            reg,
            high: false,
            size: operand,
        },
        (0x8A, None) => {
            let (reg, high) = byte_reg(reg);
            Access::Load {
                reg,
                high,
                size: 1,
                dest: 1,
                extend: Extend::None,
            }
        }
        (0x8B, None) => Access::Load {
            reg,
            high: false,
            size: operand,
            dest: operand,
            extend: Extend::None,
        },
        (0xC6, None) if reg_field == 0 => {
            let value = u64::from(*bytes.get(at)?);
            at += 1;
            Access::StoreImm { value, size: 1 }
        }
        (0xC7, None) if reg_field == 0 => {
            let value = if operand == 2 {
                let b: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
                at += 2;
                u64::from(u16::from_le_bytes(b))
            } else {
                let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
                at += 4;
                let imm = i32::from_le_bytes(b);
                // Sign-extended to 64 bits under REX.W, as is any imm32 (SDM Vol. 2B MOV).
                if operand == 8 {
                    imm as i64 as u64
                } else {
                    u64::from(imm as u32)
                }
            };
            Access::StoreImm { value, size: operand }
        }
        (0x0F, Some(op @ (0xB6 | 0xB7 | 0xBE | 0xBF))) => Access::Load {
            reg,
            high: false,
            size: if op & 1 == 0 { 1 } else { 2 },
            dest: operand,
            extend: if op < 0xBE { Extend::Zero } else { Extend::Sign },
        },
        _ => return None,
    };
    // Every byte of it within what the exit carried.
    if at > bytes.len() {
        return None;
    }
    Some(Decoded {
        access,
        len: u8::try_from(at).ok()?,
    })
}

/// The `size` bytes a store writes, from its register's value `value`.
pub fn stored(value: u64, high: bool, size: u8) -> u64 {
    let value = if high { value >> 8 } else { value };
    value & mask(size)
}

/// Register `old`'s value once a load has put `data` (its `size` bytes) into it, as a
/// register `dest` bytes wide: a 32-bit destination zeroes the upper half, an 8- or
/// 16-bit one keeps the rest of the register (SDM Vol. 1 §3.4.1.1).
pub fn loaded(old: u64, data: u64, size: u8, dest: u8, high: bool, extend: Extend) -> u64 {
    let data = data & mask(size);
    let value = match extend {
        Extend::Sign if size < 8 => {
            let shift = 64 - 8 * u32::from(size);
            (((data << shift) as i64) >> shift) as u64
        }
        _ => data,
    };
    match dest {
        1 if high => (old & !0xFF00) | ((value & 0xFF) << 8),
        1 => (old & !0xFF) | (value & 0xFF),
        2 => (old & !0xFFFF) | (value & 0xFFFF),
        4 => value & 0xFFFF_FFFF,
        _ => value,
    }
}

/// The low `size` bytes.
fn mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn load(reg: Gpr, high: bool, size: u8, dest: u8, extend: Extend) -> Access {
        Access::Load {
            reg,
            high,
            size,
            dest,
            extend,
        }
    }

    fn store(reg: Gpr, high: bool, size: u8) -> Access {
        Access::Store { reg, high, size }
    }

    /// Linux's MMIO accessors as compilers emit them (gcc and clang, x86-64), each with
    /// its length and effect; and the probe's own (M198).
    #[test]
    fn the_accessors_linux_compiles_decode() {
        let cases: &[(&[u8], Access)] = &[
            // writel: mov %eax,(%rdx); mov %ecx,(%rsi); mov %r8d,(%r9)
            (&[0x89, 0x02], store(0, false, 4)),
            (&[0x89, 0x0E], store(1, false, 4)),
            (&[0x45, 0x89, 0x01], store(8, false, 4)),
            // writeq: mov %rax,(%rdx)
            (&[0x48, 0x89, 0x02], store(0, false, 8)),
            // writew: mov %ax,0x10(%rdi)
            (&[0x66, 0x89, 0x47, 0x10], store(0, false, 2)),
            // writeb: mov %al,(%rdx); mov %ah,(%rdx); mov %sil,(%rdx); mov %r9b,(%rdx)
            (&[0x88, 0x02], store(0, false, 1)),
            (&[0x88, 0x22], store(0, true, 1)),
            (&[0x40, 0x88, 0x32], store(6, false, 1)),
            (&[0x44, 0x88, 0x0A], store(9, false, 1)),
            // readl: mov 0x70(%rbx),%eax; mov 0x12345678(%rbx),%r15d
            (&[0x8B, 0x43, 0x70], load(0, false, 4, 4, Extend::None)),
            (
                &[0x44, 0x8B, 0xBB, 0x78, 0x56, 0x34, 0x12],
                load(15, false, 4, 4, Extend::None),
            ),
            // readq: mov (%rdi),%rax
            (&[0x48, 0x8B, 0x07], load(0, false, 8, 8, Extend::None)),
            // readb into %bh, and readw: movzbl (%rdi),%eax; movzwl 0x4(%rax),%eax
            (&[0x8A, 0x3F], load(3, true, 1, 1, Extend::None)),
            (&[0x0F, 0xB6, 0x07], load(0, false, 1, 4, Extend::Zero)),
            (&[0x0F, 0xB7, 0x40, 0x04], load(0, false, 2, 4, Extend::Zero)),
            // movsbl (%rax),%ecx; movswq (%rax),%rcx
            (&[0x0F, 0xBE, 0x08], load(1, false, 1, 4, Extend::Sign)),
            (&[0x48, 0x0F, 0xBF, 0x08], load(1, false, 2, 8, Extend::Sign)),
            // SIB, with and without a base: mov %eax,0x10(%rax,%rcx,4); mov %eax,0x2000
            (&[0x89, 0x44, 0x88, 0x10], store(0, false, 4)),
            (&[0x89, 0x04, 0x25, 0x00, 0x20, 0x00, 0x00], store(0, false, 4)),
            // RIP-relative: mov 0x1234(%rip),%eax
            (
                &[0x8B, 0x05, 0x34, 0x12, 0x00, 0x00],
                load(0, false, 4, 4, Extend::None),
            ),
            // movl $1,0x50(%rax); movw $1,(%rax); movb $1,(%rax); movq $-1,(%rax)
            (
                &[0xC7, 0x40, 0x50, 0x01, 0x00, 0x00, 0x00],
                Access::StoreImm { value: 1, size: 4 },
            ),
            (
                &[0x66, 0xC7, 0x00, 0x01, 0x00],
                Access::StoreImm { value: 1, size: 2 },
            ),
            (&[0xC6, 0x00, 0x01], Access::StoreImm { value: 1, size: 1 }),
            (
                &[0x48, 0xC7, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
                Access::StoreImm {
                    value: u64::MAX,
                    size: 8,
                },
            ),
            // A segment override, as a per-CPU access has: mov %eax,%gs:(%rdx)
            (&[0x65, 0x89, 0x02], store(0, false, 4)),
        ];
        for (bytes, access) in cases {
            // Trailing bytes, as an exit carries 16, are not the instruction's.
            let mut padded = bytes.to_vec();
            padded.extend([0x90; 16]);
            let d = decode(&padded, Mode::Long).unwrap_or_else(|| panic!("{bytes:02x?}"));
            assert_eq!(d.access, *access, "{bytes:02x?}");
            assert_eq!(usize::from(d.len), bytes.len(), "{bytes:02x?}");
        }
    }

    /// In 32-bit protected mode, 0x40-0x4F are INC and DEC, never REX, and 16-bit
    /// addressing is refused.
    #[test]
    fn protected_mode_has_no_rex_and_no_16_bit_addressing() {
        let d = decode(&[0x89, 0x02, 0x90], Mode::Protected32).unwrap();
        assert_eq!(d.access, store(0, false, 4));
        assert_eq!(d.len, 2);
        // mov %eax,0x2000: an absolute disp32 where 64-bit mode's is RIP-relative.
        let d = decode(&[0x8B, 0x05, 0x00, 0x20, 0x00, 0x00], Mode::Protected32).unwrap();
        assert_eq!(d.len, 6);
        assert_eq!(
            decode(&[0x48, 0x89, 0x02], Mode::Protected32),
            None,
            "DEC EAX, not REX.W"
        );
        assert_eq!(decode(&[0x67, 0x89, 0x02], Mode::Protected32), None);
        // In 64-bit mode, 0x67 is 32-bit addressing, encoded as 64-bit's is.
        assert_eq!(decode(&[0x67, 0x89, 0x02], Mode::Long).unwrap().len, 3);
    }

    /// What is not decoded: MOVS, string and SSE moves, a register operand, LOCK and REP,
    /// C6 and C7 other than /0, other opcodes, and an instruction the bytes end before.
    #[test]
    fn what_is_not_decoded_is_refused() {
        let refused: &[&[u8]] = &[
            &[0xA4],
            &[0xA5],
            &[0xF3, 0xA4],
            &[0x0F, 0x10, 0x00],
            &[0x89, 0xC0],
            &[0xF0, 0x89, 0x02],
            &[0xF3, 0x0F, 0xB8, 0x00],
            &[0xC7, 0x48, 0x10, 0, 0, 0, 0],
            &[0xC6, 0x08, 0x01],
            &[0x01, 0x02],
            &[0xA1, 0, 0, 0, 0],
            &[0x8B, 0x80, 0x00, 0x10],
            &[0xC7, 0x00, 0x01, 0x00],
            &[0x0F],
            &[],
        ];
        for bytes in refused {
            assert_eq!(decode(bytes, Mode::Long), None, "{bytes:02x?}");
        }
        // Fifteen prefixes and more make no instruction.
        let mut long = vec![0x66; 15];
        long.extend([0x89, 0x02]);
        assert_eq!(decode(&long, Mode::Long), None);
    }

    /// Loads merge into their registers as the processor does; stores take their bytes.
    #[test]
    fn loads_and_stores_move_the_right_bytes() {
        let old = 0x1122_3344_5566_7788;
        assert_eq!(
            loaded(old, 0xAB, 1, 1, false, Extend::None),
            0x1122_3344_5566_77AB
        );
        assert_eq!(loaded(old, 0xAB, 1, 1, true, Extend::None), 0x1122_3344_5566_AB88);
        assert_eq!(
            loaded(old, 0xABCD, 2, 2, false, Extend::None),
            0x1122_3344_5566_ABCD
        );
        assert_eq!(loaded(old, 0xDEAD_BEEF, 4, 4, false, Extend::None), 0xDEAD_BEEF);
        assert_eq!(loaded(old, u64::MAX, 8, 8, false, Extend::None), u64::MAX);
        assert_eq!(loaded(old, 0x80, 1, 4, false, Extend::Zero), 0x80);
        assert_eq!(loaded(old, 0x80, 1, 4, false, Extend::Sign), 0xFFFF_FF80);
        assert_eq!(
            loaded(old, 0x8000, 2, 8, false, Extend::Sign),
            0xFFFF_FFFF_FFFF_8000
        );
        assert_eq!(
            loaded(old, 0x80, 1, 2, false, Extend::Sign),
            0x1122_3344_5566_FF80
        );
        // Bytes past the access's size are not the device's.
        assert_eq!(loaded(0, 0xFFFF_FF7F, 1, 4, false, Extend::Sign), 0x7F);
        assert_eq!(stored(old, false, 1), 0x88);
        assert_eq!(stored(old, true, 1), 0x77);
        assert_eq!(stored(old, false, 2), 0x7788);
        assert_eq!(stored(old, false, 4), 0x5566_7788);
        assert_eq!(stored(old, false, 8), old);
    }
}
