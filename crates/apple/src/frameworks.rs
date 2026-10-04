//! The frameworks' functions, looked up once.

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

pub type CFTypeRef = *const c_void;
pub type CFIndex = isize;
pub type OSStatus = i32;

/// `kCFStringEncodingUTF8`.
pub const UTF8: u32 = 0x0800_0100;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CFRange {
    pub location: CFIndex,
    pub length: CFIndex,
}

/// Every function and constant shards calls, from Security and CoreFoundation.
#[allow(non_snake_case, missing_debug_implementations)]
pub struct Api {
    pub CFRelease: unsafe extern "C" fn(CFTypeRef),
    pub CFDataCreate: unsafe extern "C" fn(CFTypeRef, *const u8, CFIndex) -> CFTypeRef,
    pub CFDataGetBytePtr: unsafe extern "C" fn(CFTypeRef) -> *const u8,
    pub CFDataGetLength: unsafe extern "C" fn(CFTypeRef) -> CFIndex,
    pub CFArrayCreate: unsafe extern "C" fn(CFTypeRef, *const CFTypeRef, CFIndex, *const c_void) -> CFTypeRef,
    pub CFStringCreateWithBytes: unsafe extern "C" fn(CFTypeRef, *const u8, CFIndex, u32, u8) -> CFTypeRef,
    pub CFStringGetLength: unsafe extern "C" fn(CFTypeRef) -> CFIndex,
    pub CFStringGetBytes:
        unsafe extern "C" fn(CFTypeRef, CFRange, u32, u8, u8, *mut u8, CFIndex, *mut CFIndex) -> CFIndex,
    pub CFDateCreate: unsafe extern "C" fn(CFTypeRef, f64) -> CFTypeRef,
    pub CFErrorGetCode: unsafe extern "C" fn(CFTypeRef) -> CFIndex,
    pub CFErrorCopyDescription: unsafe extern "C" fn(CFTypeRef) -> CFTypeRef,
    pub CFURLCreateFromFileSystemRepresentation:
        unsafe extern "C" fn(CFTypeRef, *const u8, CFIndex, u8) -> CFTypeRef,
    pub CFURLCreateBookmarkData:
        unsafe extern "C" fn(CFTypeRef, CFTypeRef, usize, CFTypeRef, CFTypeRef, *mut CFTypeRef) -> CFTypeRef,
    /// `kCFTypeArrayCallBacks`: an address, the callbacks' struct.
    pub kCFTypeArrayCallBacks: *const c_void,
    /// `kCFAbsoluteTimeIntervalSince1970`.
    pub since_1970: f64,
    pub SecCertificateCreateWithData: unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> CFTypeRef,
    pub SecPolicyCreateSSL: unsafe extern "C" fn(u8, CFTypeRef) -> CFTypeRef,
    pub SecTrustCreateWithCertificates:
        unsafe extern "C" fn(CFTypeRef, CFTypeRef, *mut CFTypeRef) -> OSStatus,
    pub SecTrustSetVerifyDate: unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> OSStatus,
    pub SecTrustSetOCSPResponse: unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> OSStatus,
    pub SecTrustSetAnchorCertificates: unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> OSStatus,
    pub SecTrustSetAnchorCertificatesOnly: unsafe extern "C" fn(CFTypeRef, u8) -> OSStatus,
    pub SecTrustEvaluateWithError: unsafe extern "C" fn(CFTypeRef, *mut CFTypeRef) -> bool,
    pub SecCopyErrorMessageString: unsafe extern "C" fn(OSStatus, *const c_void) -> CFTypeRef,
}

// SAFETY: function pointers and constants of process-wide frameworks, never written.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

const SECURITY: &CStr = c"/System/Library/Frameworks/Security.framework/Security";
const CORE_FOUNDATION: &CStr = c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

/// The frameworks, loaded the first time anything asks, or why they could not be.
pub fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn open(path: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: a NUL-terminated path; the handle is kept for the process's life.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        // SAFETY: dlerror's message, if any, valid until the next dl call on this thread.
        let why = unsafe {
            let e = libc::dlerror();
            if e.is_null() {
                String::from("not loaded")
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        };
        return Err(format!("{}: {why}", path.to_string_lossy()));
    }
    Ok(handle)
}

fn symbol(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: a live handle and a NUL-terminated name.
    let at = unsafe { libc::dlsym(handle, name.as_ptr() as *const c_char) };
    if at.is_null() {
        return Err(format!("{} is missing", name.to_string_lossy()));
    }
    Ok(at)
}

/// A symbol's address as the function pointer type `F`, a pointer's size.
///
/// # Safety
///
/// `at` is a function whose C signature is `F`'s.
unsafe fn function<F: Copy>(at: *mut c_void) -> F {
    const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
    // SAFETY: as the caller promises, and of a pointer's size, checked above.
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&at) }
}

fn load() -> Result<Api, String> {
    let cf = open(CORE_FOUNDATION)?;
    let sec = open(SECURITY)?;
    macro_rules! f {
        ($h:expr, $name:literal) => {{
            let at = symbol($h, $name)?;
            // SAFETY: the framework's own function of that name, whose C signature the
            // field it fills declares (Apple's headers: CFBase.h, CFData.h, CFArray.h,
            // CFString.h, CFDate.h, CFError.h, CFURL.h, SecCertificate.h, SecPolicy.h,
            // SecTrust.h, SecBase.h).
            unsafe { function(at) }
        }};
    }
    let callbacks = symbol(cf, c"kCFTypeArrayCallBacks")?;
    let since = symbol(cf, c"kCFAbsoluteTimeIntervalSince1970")?;
    Ok(Api {
        CFRelease: f!(cf, c"CFRelease"),
        CFDataCreate: f!(cf, c"CFDataCreate"),
        CFDataGetBytePtr: f!(cf, c"CFDataGetBytePtr"),
        CFDataGetLength: f!(cf, c"CFDataGetLength"),
        CFArrayCreate: f!(cf, c"CFArrayCreate"),
        CFStringCreateWithBytes: f!(cf, c"CFStringCreateWithBytes"),
        CFStringGetLength: f!(cf, c"CFStringGetLength"),
        CFStringGetBytes: f!(cf, c"CFStringGetBytes"),
        CFDateCreate: f!(cf, c"CFDateCreate"),
        CFErrorGetCode: f!(cf, c"CFErrorGetCode"),
        CFErrorCopyDescription: f!(cf, c"CFErrorCopyDescription"),
        CFURLCreateFromFileSystemRepresentation: f!(cf, c"CFURLCreateFromFileSystemRepresentation"),
        CFURLCreateBookmarkData: f!(cf, c"CFURLCreateBookmarkData"),
        kCFTypeArrayCallBacks: callbacks,
        // SAFETY: a `const CFTimeInterval` (a double) the framework exports.
        since_1970: unsafe { *(since as *const f64) },
        SecCertificateCreateWithData: f!(sec, c"SecCertificateCreateWithData"),
        SecPolicyCreateSSL: f!(sec, c"SecPolicyCreateSSL"),
        SecTrustCreateWithCertificates: f!(sec, c"SecTrustCreateWithCertificates"),
        SecTrustSetVerifyDate: f!(sec, c"SecTrustSetVerifyDate"),
        SecTrustSetOCSPResponse: f!(sec, c"SecTrustSetOCSPResponse"),
        SecTrustSetAnchorCertificates: f!(sec, c"SecTrustSetAnchorCertificates"),
        SecTrustSetAnchorCertificatesOnly: f!(sec, c"SecTrustSetAnchorCertificatesOnly"),
        SecTrustEvaluateWithError: f!(sec, c"SecTrustEvaluateWithError"),
        SecCopyErrorMessageString: f!(sec, c"SecCopyErrorMessageString"),
    })
}

/// A CoreFoundation object this code made, released once, when dropped.
pub struct Owned<'a> {
    api: &'a Api,
    pub ptr: CFTypeRef,
}

impl<'a> Owned<'a> {
    /// `ptr`, made by a Create or Copy call, or `None` if it made nothing.
    pub fn new(api: &'a Api, ptr: CFTypeRef) -> Option<Owned<'a>> {
        // Made only for a reference: an `Owned` of NULL, even dropped unused, would
        // release NULL, which CoreFoundation halts on.
        (!ptr.is_null()).then(|| Owned { api, ptr })
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        // SAFETY: a reference made for this value alone, released once.
        unsafe { (self.api.CFRelease)(self.ptr) }
    }
}

impl std::fmt::Debug for Owned<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Owned({:p})", self.ptr)
    }
}

/// A CFData of `bytes`.
pub fn data<'a>(api: &'a Api, bytes: &[u8]) -> Option<Owned<'a>> {
    let len = CFIndex::try_from(bytes.len()).ok()?;
    // SAFETY: a buffer of `len` bytes, copied by the call.
    Owned::new(api, unsafe {
        (api.CFDataCreate)(std::ptr::null(), bytes.as_ptr(), len)
    })
}

/// A CFString of `s`.
pub fn string<'a>(api: &'a Api, s: &str) -> Option<Owned<'a>> {
    let len = CFIndex::try_from(s.len()).ok()?;
    // SAFETY: UTF-8 bytes of `len`, copied by the call.
    Owned::new(api, unsafe {
        (api.CFStringCreateWithBytes)(std::ptr::null(), s.as_ptr(), len, UTF8, 0)
    })
}

/// A CFArray of `items`, each retained by it.
pub fn array<'a>(api: &'a Api, items: &[CFTypeRef]) -> Option<Owned<'a>> {
    let len = CFIndex::try_from(items.len()).ok()?;
    // SAFETY: `len` live references, retained by the array through the standard callbacks.
    Owned::new(api, unsafe {
        (api.CFArrayCreate)(std::ptr::null(), items.as_ptr(), len, api.kCFTypeArrayCallBacks)
    })
}

/// A CFString's text.
pub fn text(api: &Api, s: CFTypeRef) -> String {
    // SAFETY: a live CFString; the bytes are copied into a buffer of the size asked for.
    unsafe {
        let len = (api.CFStringGetLength)(s);
        let range = CFRange {
            location: 0,
            length: len,
        };
        let mut need: CFIndex = 0;
        (api.CFStringGetBytes)(s, range, UTF8, 0, 0, std::ptr::null_mut(), 0, &mut need);
        let mut buf = vec![0u8; usize::try_from(need).unwrap_or(0)];
        let mut used: CFIndex = 0;
        (api.CFStringGetBytes)(s, range, UTF8, 0, 0, buf.as_mut_ptr(), need, &mut used);
        buf.truncate(usize::try_from(used).unwrap_or(0));
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// What Security says of `status`, as security-framework's `Error` says it.
pub fn status_text(api: &Api, status: OSStatus) -> String {
    // SAFETY: SecCopyErrorMessageString makes a string we release.
    let message = Owned::new(api, unsafe {
        (api.SecCopyErrorMessageString)(status, std::ptr::null())
    });
    match message {
        Some(m) => text(api, m.ptr),
        None => format!("error code {status}"),
    }
}
