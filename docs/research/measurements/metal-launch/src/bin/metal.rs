// Links what MoltenVK links (Metal, QuartzCore, IOSurface, Foundation), as a VM process
// carrying an in-process renderer would, so that dyld loads them at launch.
#[link(name = "Metal", kind = "framework")]
#[link(name = "QuartzCore", kind = "framework")]
#[link(name = "IOSurface", kind = "framework")]
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut core::ffi::c_void;
}
fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("device") {
        // SAFETY: a call with no arguments; the device is leaked.
        let d = unsafe { MTLCreateSystemDefaultDevice() };
        assert!(!d.is_null());
    }
    if arg.as_deref() == Some("hold") || arg.as_deref() == Some("device-hold") {
        if arg.as_deref() == Some("device-hold") {
            // SAFETY: as above.
            let _ = unsafe { MTLCreateSystemDefaultDevice() };
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    // Keep the symbol referenced so the frameworks stay linked.
    std::hint::black_box(MTLCreateSystemDefaultDevice as unsafe extern "C" fn() -> _);
}
