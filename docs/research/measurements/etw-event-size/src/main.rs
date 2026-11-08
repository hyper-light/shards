//! The longest string event ETW takes from EventWriteString (PM M492), the call the
//! etwlogs driver writes a run's lines with (daemon/ship/etwlogs.rs, D198).
//!
//! Microsoft's documentation bounds an event at "64KB - header" (ERROR_ARITHMETIC_OVERFLOW)
//! and by the buffers of the sessions that listen (ERROR_MORE_DATA), without saying the
//! header's size. `etw-event-size GUID`: registers a provider of that GUID and, while a
//! session the caller started listens for it (logman), finds by halves the most UTF-16 units,
//! its NUL among them, that EventWriteString takes, then says the answer of each length
//! around it. Without a session listening ETW writes nothing, and says nothing of size.

use windows_sys::Win32::System::Diagnostics::Etw::{EventRegister, EventUnregister, EventWriteString, REGHANDLE};
use windows_sys::core::GUID;

fn guid(s: &str) -> Option<GUID> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    Some(GUID::from_u128(u128::from_str_radix(&hex, 16).ok()?))
}

fn write(handle: REGHANDLE, units: usize) -> u32 {
    let mut s: Vec<u16> = std::iter::repeat_n(u16::from(b'x'), units.saturating_sub(1)).collect();
    s.push(0);
    // SAFETY: a registered provider and a NUL-terminated wide string that outlives the call.
    unsafe { EventWriteString(handle, 0, 0, s.as_ptr()) }
}

fn main() {
    let Some(provider) = std::env::args().nth(1).as_deref().and_then(guid) else {
        eprintln!("usage: etw-event-size GUID");
        std::process::exit(2);
    };
    let mut handle: REGHANDLE = 0;
    // SAFETY: a GUID, no callback, and a handle for the call to fill.
    let registered = unsafe { EventRegister(&provider, None, std::ptr::null(), &raw mut handle) };
    if registered != 0 {
        eprintln!("EventRegister: {registered}");
        std::process::exit(1);
    }
    // The most units taken: 1 is, 64 Ki is not (its bytes alone are 128 KiB).
    let (mut lo, mut hi) = (1usize, 64 << 10);
    assert_eq!(write(handle, lo), 0, "a one-unit event refused");
    let refused = write(handle, hi);
    println!("{hi} units: {refused}");
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if write(handle, mid) == 0 { lo = mid } else { hi = mid }
    }
    println!("most taken: {lo} units, {} bytes of string", lo * 2);
    for units in lo.saturating_sub(2)..=lo + 2 {
        println!("  {units} units ({} bytes): {}", units * 2, write(handle, units));
    }
    // SAFETY: the registration made above, once.
    unsafe { EventUnregister(handle) };
}
