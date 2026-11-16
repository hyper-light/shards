//! The I/O this host's block storage has completed, as its drivers count it: each IOKit
//! `IOBlockStorageDriver`'s `Statistics`, whose `Operations (Read)` and `Operations (Write)`
//! (IOKit/storage/IOBlockStorageDriver.h, `kIOBlockStorageDriverStatisticsReadsKey` and
//! `kIOBlockStorageDriverStatisticsWritesKey`) are what iostat(8) reads. D215's judge reads
//! their growth to tell a wait in a kernel that its host's disks are working through from one
//! they are not (shards-api progress.rs `Hold::disks`).

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

use crate::frameworks::{self, CFTypeRef, Owned};

/// IOKit's functions (IOKitLib.h): its ports and objects are `mach_port_t`s (u32), its
/// returns `kern_return_t`s.
#[allow(non_snake_case)]
struct Api {
    IOServiceMatching: unsafe extern "C" fn(*const c_char) -> CFTypeRef,
    IOServiceGetMatchingServices: unsafe extern "C" fn(u32, CFTypeRef, *mut u32) -> i32,
    IOIteratorNext: unsafe extern "C" fn(u32) -> u32,
    IORegistryEntryCreateCFProperty: unsafe extern "C" fn(u32, CFTypeRef, CFTypeRef, u32) -> CFTypeRef,
    IOObjectRelease: unsafe extern "C" fn(u32) -> i32,
}

// SAFETY: function pointers of a process-wide framework, never written.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

const IOKIT: &CStr = c"/System/Library/Frameworks/IOKit.framework/IOKit";

/// The main port IOKit's calls take: IOKitLib.h's `kIOMainPortDefault`, for which
/// `MACH_PORT_NULL` stands.
const MAIN_PORT_DEFAULT: u32 = 0;

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let io = frameworks::open(IOKIT)?;
        macro_rules! f {
            ($name:literal) => {{
                let at = frameworks::symbol(io, $name)?;
                // SAFETY: the framework's own function of that name, whose C signature the
                // field it fills declares (IOKitLib.h).
                unsafe { frameworks::function(at) }
            }};
        }
        Ok(Api {
            IOServiceMatching: f!(c"IOServiceMatching"),
            IOServiceGetMatchingServices: f!(c"IOServiceGetMatchingServices"),
            IOIteratorNext: f!(c"IOIteratorNext"),
            IORegistryEntryCreateCFProperty: f!(c"IORegistryEntryCreateCFProperty"),
            IOObjectRelease: f!(c"IOObjectRelease"),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The reads and writes every block storage driver of this host has completed so far,
/// summed; none where they cannot be read. A driver whose statistics cannot be read adds
/// nothing: only the sum's growth is read, which any other shows.
pub fn completed() -> Option<u64> {
    let io = api().ok()?;
    let cf = frameworks::api().ok()?;
    let statistics = frameworks::string(cf, "Statistics")?;
    let keys = [
        frameworks::string(cf, "Operations (Read)")?,
        frameworks::string(cf, "Operations (Write)")?,
    ];
    // SAFETY: a NUL-terminated class name; the dictionary made is consumed below.
    let matching = unsafe { (io.IOServiceMatching)(c"IOBlockStorageDriver".as_ptr()) };
    if matching.is_null() {
        return None;
    }
    let mut drivers: u32 = 0;
    // SAFETY: the default main port, the matching dictionary, one reference of which the
    // call consumes whatever it returns, and an iterator for it to fill.
    if unsafe { (io.IOServiceGetMatchingServices)(MAIN_PORT_DEFAULT, matching, &raw mut drivers) } != 0 {
        return None;
    }
    let mut sum = 0u64;
    loop {
        // SAFETY: the iterator made above, released after the loop.
        let driver = unsafe { (io.IOIteratorNext)(drivers) };
        if driver == 0 {
            break;
        }
        // SAFETY: a registry entry the iterator gave, and a CFString key; the property is a
        // copy of ours, released as it drops.
        let property = Owned::new(cf, unsafe {
            (io.IORegistryEntryCreateCFProperty)(driver, statistics.ptr, std::ptr::null(), 0)
        });
        // SAFETY: the entry IOIteratorNext returned, released once.
        unsafe { (io.IOObjectRelease)(driver) };
        if let Some(property) = property {
            for key in &keys {
                sum = sum.saturating_add(count(cf, property.ptr, key.ptr).unwrap_or(0));
            }
        }
    }
    // SAFETY: the iterator made above, released once.
    unsafe { (io.IOObjectRelease)(drivers) };
    Some(sum)
}

/// The count `dictionary` holds under `key`: none where it is no dictionary, holds no
/// number there, or one that is not a non-negative 64-bit integer.
fn count(cf: &frameworks::Api, dictionary: CFTypeRef, key: CFTypeRef) -> Option<u64> {
    // SAFETY: CFGetTypeID of a live object, and the type IDs CoreFoundation keeps.
    let is_dictionary = unsafe { (cf.CFGetTypeID)(dictionary) == (cf.CFDictionaryGetTypeID)() };
    if !is_dictionary {
        return None;
    }
    // SAFETY: a live dictionary and a live key; the value, if any, is the dictionary's, not
    // ours to release.
    let value = unsafe { (cf.CFDictionaryGetValue)(dictionary, key) };
    // SAFETY: CFGetTypeID of a live object the dictionary holds.
    if value.is_null() || unsafe { (cf.CFGetTypeID)(value) != (cf.CFNumberGetTypeID)() } {
        return None;
    }
    let mut n: i64 = 0;
    // SAFETY: a live CFNumber, read as an SInt64 into a local of that type.
    let exact = unsafe { (cf.CFNumberGetValue)(value, frameworks::SINT64, (&raw mut n).cast::<c_void>()) };
    if exact == 0 {
        return None;
    }
    u64::try_from(n).ok()
}

#[cfg(test)]
mod tests {
    /// This host's block storage drivers are read, and their completed I/O only grows: a
    /// write flushed to the disk's media (F_FULLFSYNC) is one more of their writes, so the
    /// sum is read again until it shows that, a wait for that fact, with no clock.
    #[test]
    fn a_hosts_completed_io_is_read_and_grows_with_a_flush() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;
        let first = super::completed().expect("the host's block storage statistics");
        let dir = shards_testdir::TempDir::new("storage").unwrap();
        let mut file = std::fs::File::create(dir.join("flushed")).unwrap();
        loop {
            file.write_all(&[0u8; 4096]).unwrap();
            // SAFETY: fcntl(2) F_FULLFSYNC of a file this test holds open.
            assert_ne!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) }, -1);
            if super::completed().unwrap() > first {
                break;
            }
        }
    }
}
