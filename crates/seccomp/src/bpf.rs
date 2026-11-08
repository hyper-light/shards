//! Classic BPF over `struct seccomp_data` (linux include/uapi/linux/filter.h, seccomp.h):
//! the instructions a seccomp filter is made of, and an interpreter of them, by which a
//! filter's decisions are read without a kernel (its tests hold shards' programs to the
//! ones runc loads, input by input).

/// One instruction: `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

impl Insn {
    /// Its bytes as the kernel reads `struct sock_filter`, in the host's order.
    pub fn to_ne_bytes(self) -> [u8; 8] {
        let [c0, c1] = self.code.to_ne_bytes();
        let [k0, k1, k2, k3] = self.k.to_ne_bytes();
        [c0, c1, self.jt, self.jf, k0, k1, k2, k3]
    }
}

pub const LD_W_ABS: u16 = 0x20;
pub const ALU_AND_K: u16 = 0x54;
pub const JMP_JA: u16 = 0x05;
pub const JMP_JEQ_K: u16 = 0x15;
pub const JMP_JGT_K: u16 = 0x25;
pub const JMP_JGE_K: u16 = 0x35;
pub const JMP_JSET_K: u16 = 0x45;
pub const RET_K: u16 = 0x06;

/// What a filter is run on: `struct seccomp_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Data {
    pub nr: u32,
    pub arch: u32,
    pub ip: u64,
    pub args: [u64; 6],
}

impl Data {
    /// The 32-bit word at byte `offset`, as `BPF_LD|BPF_W|BPF_ABS` loads it on a
    /// little-endian machine (both guest architectures are).
    fn word(&self, offset: u32) -> Option<u32> {
        let arg = |i: usize| self.args.get(i).copied();
        match offset {
            0 => Some(self.nr),
            4 => Some(self.arch),
            8 => Some(self.ip as u32),
            12 => Some((self.ip >> 32) as u32),
            16..=63 if offset.is_multiple_of(4) => {
                let i = ((offset - 16) / 8) as usize;
                let v = arg(i)?;
                Some(if (offset - 16).is_multiple_of(8) {
                    v as u32
                } else {
                    (v >> 32) as u32
                })
            }
            _ => None,
        }
    }
}

/// What `program` returns for `data`, as the kernel runs it (net/core/filter.c,
/// seccomp_check_filter's subset of instructions); `None` where it is not a program the
/// kernel would take (a jump out of it, an instruction outside the subset, no return).
pub fn run(program: &[Insn], data: &Data) -> Option<u32> {
    run_counted(program, data).map(|(r, _)| r)
}

/// [`run`], and how many instructions it ran.
pub fn run_counted(program: &[Insn], data: &Data) -> Option<(u32, usize)> {
    let mut ran = 0;
    let mut acc: u32 = 0;
    let mut pc: usize = 0;
    // Every jump is forward, so a program runs at most once through.
    for _ in 0..=program.len() {
        let i = program.get(pc)?;
        ran += 1;
        let next = pc.checked_add(1)?;
        let branch =
            |taken: bool| -> Option<usize> { next.checked_add(usize::from(if taken { i.jt } else { i.jf })) };
        pc = match i.code {
            LD_W_ABS => {
                acc = data.word(i.k)?;
                next
            }
            ALU_AND_K => {
                acc &= i.k;
                next
            }
            JMP_JA => next.checked_add(i.k as usize)?,
            JMP_JEQ_K => branch(acc == i.k)?,
            JMP_JGT_K => branch(acc > i.k)?,
            JMP_JGE_K => branch(acc >= i.k)?,
            JMP_JSET_K => branch(acc & i.k != 0)?,
            RET_K => return Some((i.k, ran)),
            _ => return None,
        };
    }
    None
}
