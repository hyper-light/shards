// Links what MoltenVK's static library needs (PM M214): Metal, QuartzCore, IOSurface and
// Foundation, as `metal` does, and IOKit, CoreGraphics and AppKit, whose functions and
// classes its surfaces and displays reference, so that dyld loads all seven at launch.
#[link(name = "Metal", kind = "framework")]
#[link(name = "QuartzCore", kind = "framework")]
#[link(name = "IOSurface", kind = "framework")]
#[link(name = "Foundation", kind = "framework")]
#[link(name = "IOKit", kind = "framework")]
#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut core::ffi::c_void;
    fn CACurrentMediaTime() -> f64;
    fn IOSurfaceGetPropertyAlignment(property: *const core::ffi::c_void) -> usize;
    fn NSTemporaryDirectory() -> *mut core::ffi::c_void;
    fn IOServiceMatching(name: *const core::ffi::c_char) -> *mut core::ffi::c_void;
    fn CGMainDisplayID() -> u32;
    fn NSApplicationLoad() -> bool;
}
fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("device") || arg.as_deref() == Some("device-hold") {
        // SAFETY: a call with no arguments; the device is leaked.
        let d = unsafe { MTLCreateSystemDefaultDevice() };
        assert!(!d.is_null());
    }
    if arg.as_deref() == Some("hold") || arg.as_deref() == Some("device-hold") {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    // Each framework's symbol kept referenced, so that each stays linked.
    std::hint::black_box(MTLCreateSystemDefaultDevice as unsafe extern "C" fn() -> _);
    std::hint::black_box(CACurrentMediaTime as unsafe extern "C" fn() -> _);
    std::hint::black_box(IOSurfaceGetPropertyAlignment as unsafe extern "C" fn(_) -> _);
    std::hint::black_box(NSTemporaryDirectory as unsafe extern "C" fn() -> _);
    std::hint::black_box(IOServiceMatching as unsafe extern "C" fn(_) -> _);
    std::hint::black_box(CGMainDisplayID as unsafe extern "C" fn() -> _);
    std::hint::black_box(NSApplicationLoad as unsafe extern "C" fn() -> _);
}
