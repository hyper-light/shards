//! Copies between guest memory and the host's own, as D29 has the VMM's threads make them:
//! by accesses the compiler may neither elide, merge, split nor invent (volatile ones, or
//! `asm!`), laid out as the host's own memcpy lays its copies out, so that they take its
//! time (PM M137).
//!
//! A copy of 64 bytes or more goes much as macOS's memcpy (`_platform_memmove`, in
//! libsystem_platform) goes: its first 64 bytes, then 64 at a time from its destination's
//! next cache line, whose stores then never straddle one, then its last 32. The bytes
//! between are written twice, the same. memcpy aligns its stores to 32 bytes, which left
//! frames of 9000 bytes and more slower than these to deliver. Its accesses:
//!
//! - aarch64: pairs of 16-byte registers, as glibc's memcpy (sysdeps/aarch64/memcpy.S,
//!   `L(loop64)`: LDP and STP of Q registers). On macOS as its own: loads non-temporal
//!   (LDNP), and stores too (STNP) in copies of 16 KiB or more, which temporal ones made
//!   slower from memory the caches do not hold.
//! - x86_64: SSE2's 16-byte moves, the widest every x86_64 has, by `asm!`, since a volatile
//!   u128 is split into two 8-byte moves. Unmeasured against glibc's memcpy (AVX, `rep
//!   movsb`).
//!
//! A shorter copy, and every copy under Miri, which runs no `asm!`, or on other
//! architectures: single bytes up to the guest side's 16-byte boundary, then volatile
//! u128s, an 8-byte word and single bytes, each byte once; a guest's aligned fields are read
//! and written whole.

/// The width of a short copy's accesses.
const WIDE: usize = 16;
/// The width of a short copy's last whole word.
const WORD: usize = 8;
/// From how long a copy goes as memcpy's long ones go.
#[cfg(all(not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
const LONG: usize = 64;
/// From how many bytes macOS's memcpy stores past the caches (`_platform_memmove`:
/// `cmp x2, #0x4, lsl #12`, then STNP).
#[cfg(all(not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
const STREAM: usize = 16 << 10;

#[cfg(test)]
thread_local! {
    /// How many bytes this thread has copied to and from guest memory: how a test sees a
    /// device's bytes cross by these copies.
    pub static COPIED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Copies `len` bytes of guest memory at `src` to host memory at `dst`.
///
/// # Safety
/// `src` must be valid for reads of `len` bytes of guest memory, and `dst` for writes of
/// `len` bytes of host memory that nothing else accesses meanwhile; the two never overlap.
pub unsafe fn copy_in(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(test)]
    COPIED.with(|c| c.set(c.get() + len as u64));
    // SAFETY: as the caller promises.
    unsafe { copy::<true>(src, dst, len) }
}

/// Copies `len` bytes of host memory at `src` to guest memory at `dst`.
///
/// # Safety
/// `src` must be valid for reads of `len` bytes of host memory, and `dst` for writes of
/// `len` bytes of guest memory; the two never overlap.
pub unsafe fn copy_out(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(test)]
    COPIED.with(|c| c.set(c.get() + len as u64));
    // SAFETY: as the caller promises.
    unsafe { copy::<false>(src, dst, len) }
}

/// Copies `len` bytes from `src` to `dst`, guest memory being `src` if `IN`, else `dst`.
///
/// # Safety
/// As [`copy_in`]'s or [`copy_out`]'s.
#[inline(always)]
unsafe fn copy<const IN: bool>(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: as the caller promises.
    if unsafe { long(src, dst, len) } {
        return;
    }
    let head = if IN {
        src.align_offset(WIDE)
    } else {
        dst.align_offset(WIDE)
    }
    .min(len);
    let mut i = 0;
    // SAFETY: every access lies within the `len` bytes at `src` and `dst`; the guest side's
    // wide and word accesses start `head` and a multiple of WIDE in, so are aligned.
    unsafe {
        while i < head {
            dst.add(i).write_volatile(src.add(i).read_volatile());
            i += 1;
        }
        while len - i >= WIDE {
            if IN {
                let w = src.add(i).cast::<u128>().read_volatile();
                dst.add(i).cast::<u128>().write_unaligned(w);
            } else {
                let w = src.add(i).cast::<u128>().read_unaligned();
                dst.add(i).cast::<u128>().write_volatile(w);
            }
            i += WIDE;
        }
        if len - i >= WORD {
            if IN {
                let w = src.add(i).cast::<u64>().read_volatile();
                dst.add(i).cast::<u64>().write_unaligned(w);
            } else {
                let w = src.add(i).cast::<u64>().read_unaligned();
                dst.add(i).cast::<u64>().write_volatile(w);
            }
            i += WORD;
        }
        while i < len {
            dst.add(i).write_volatile(src.add(i).read_volatile());
            i += 1;
        }
    }
}

/// Copies `len` bytes much as memcpy copies 64 or more, if `len` is that long: its first
/// 64 bytes, 64 at a time from the destination's next 64-byte boundary, then its last 32.
/// Whether it copied.
///
/// # Safety
/// `src` valid for reads and `dst` for writes of `len` bytes, the two apart.
#[cfg(all(not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(always)]
unsafe fn long(src: *const u8, dst: *mut u8, len: usize) -> bool {
    if len < LONG {
        return false;
    }
    // SAFETY: as the caller promises, `len` being 64 or more.
    unsafe {
        if len >= STREAM {
            pieces::<true>(src, dst, len);
        } else {
            pieces::<false>(src, dst, len);
        }
    }
    true
}

/// [`long`]'s pieces, stored past the caches if `STREAM` (on macOS).
///
/// # Safety
/// As [`long`]'s, `len` being 64 or more.
#[cfg(all(not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(always)]
unsafe fn pieces<const STREAM: bool>(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: as the caller promises. Each piece lies within the `len` bytes: the first
    // block's 64, `i` from 1 to 64 on, blocks while more than 64 bytes are left, and the
    // last pair from `len - 32`, which is no further in than `i + 32` then.
    unsafe {
        block::<STREAM>(src, dst);
        let mut i = 64 - dst.addr() % 64;
        while len - i > 64 {
            block::<STREAM>(src.add(i), dst.add(i));
            i += 64;
        }
        if len - i > 32 {
            pair::<STREAM>(src.add(i), dst.add(i));
        }
        pair::<STREAM>(src.add(len - 32), dst.add(len - 32));
    }
}

/// The instructions an aarch64 copy loads and stores Q-register pairs with: as macOS's
/// memcpy, non-temporal loads, and non-temporal stores if `STREAM`; elsewhere as glibc's.
#[cfg(all(not(miri), target_arch = "aarch64"))]
macro_rules! by_host {
    ($stream:ident, $pairs:ident) => {
        if !cfg!(target_os = "macos") {
            $pairs!("ldp", "stp");
        } else if $stream {
            $pairs!("ldnp", "stnp");
        } else {
            $pairs!("ldnp", "stp");
        }
    };
}

/// Copies 32 bytes by a pair of Q registers.
///
/// # Safety
/// `src` valid for reads and `dst` for writes of 32 bytes.
#[cfg(all(not(miri), target_arch = "aarch64"))]
#[inline(always)]
unsafe fn pair<const STREAM: bool>(src: *const u8, dst: *mut u8) {
    macro_rules! pairs {
        ($load:literal, $store:literal) => {
            // SAFETY: as the caller promises: 32 bytes read at `src`, written at `dst`, and
            // nothing else.
            unsafe {
                std::arch::asm!(
                    concat!($load, " {a:q}, {b:q}, [{s}]"),
                    concat!($store, " {a:q}, {b:q}, [{t}]"),
                    s = in(reg) src,
                    t = in(reg) dst,
                    a = out(vreg) _,
                    b = out(vreg) _,
                    options(nostack, preserves_flags),
                )
            }
        };
    }
    by_host!(STREAM, pairs);
}

/// Copies 64 bytes by two pairs of Q registers.
///
/// # Safety
/// `src` valid for reads and `dst` for writes of 64 bytes.
#[cfg(all(not(miri), target_arch = "aarch64"))]
#[inline(always)]
unsafe fn block<const STREAM: bool>(src: *const u8, dst: *mut u8) {
    macro_rules! pairs {
        ($load:literal, $store:literal) => {
            // SAFETY: as the caller promises: 64 bytes read at `src`, written at `dst`, and
            // nothing else.
            unsafe {
                std::arch::asm!(
                    concat!($load, " {a:q}, {b:q}, [{s}]"),
                    concat!($load, " {c:q}, {d:q}, [{s}, #32]"),
                    concat!($store, " {a:q}, {b:q}, [{t}]"),
                    concat!($store, " {c:q}, {d:q}, [{t}, #32]"),
                    s = in(reg) src,
                    t = in(reg) dst,
                    a = out(vreg) _,
                    b = out(vreg) _,
                    c = out(vreg) _,
                    d = out(vreg) _,
                    options(nostack, preserves_flags),
                )
            }
        };
    }
    by_host!(STREAM, pairs);
}

/// Copies 32 bytes by two SSE2 moves each way.
///
/// # Safety
/// `src` valid for reads and `dst` for writes of 32 bytes.
#[cfg(all(not(miri), target_arch = "x86_64"))]
#[inline(always)]
unsafe fn pair<const STREAM: bool>(src: *const u8, dst: *mut u8) {
    // SAFETY: as the caller promises: 32 bytes read at `src`, written at `dst`, and nothing
    // else.
    unsafe {
        std::arch::asm!(
            "movdqu {a}, xmmword ptr [{s}]",
            "movdqu {b}, xmmword ptr [{s} + 16]",
            "movdqu xmmword ptr [{t}], {a}",
            "movdqu xmmword ptr [{t} + 16], {b}",
            s = in(reg) src,
            t = in(reg) dst,
            a = out(xmm_reg) _,
            b = out(xmm_reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Copies 64 bytes by four SSE2 moves each way.
///
/// # Safety
/// `src` valid for reads and `dst` for writes of 64 bytes.
#[cfg(all(not(miri), target_arch = "x86_64"))]
#[inline(always)]
unsafe fn block<const STREAM: bool>(src: *const u8, dst: *mut u8) {
    // SAFETY: as the caller promises: 64 bytes read at `src`, written at `dst`, and nothing
    // else.
    unsafe {
        std::arch::asm!(
            "movdqu {a}, xmmword ptr [{s}]",
            "movdqu {b}, xmmword ptr [{s} + 16]",
            "movdqu {c}, xmmword ptr [{s} + 32]",
            "movdqu {d}, xmmword ptr [{s} + 48]",
            "movdqu xmmword ptr [{t}], {a}",
            "movdqu xmmword ptr [{t} + 16], {b}",
            "movdqu xmmword ptr [{t} + 32], {c}",
            "movdqu xmmword ptr [{t} + 48], {d}",
            s = in(reg) src,
            t = in(reg) dst,
            a = out(xmm_reg) _,
            b = out(xmm_reg) _,
            c = out(xmm_reg) _,
            d = out(xmm_reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Under Miri, and on other architectures, no copy goes as memcpy's long ones.
///
/// # Safety
/// None needed: it copies nothing.
#[cfg(any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
#[inline(always)]
unsafe fn long(_src: *const u8, _dst: *mut u8, _len: usize) -> bool {
    false
}
