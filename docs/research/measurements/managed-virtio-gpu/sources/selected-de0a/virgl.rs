//! The renderer: virglrenderer's Venus (D144, D201). Linked static, in `shards-vm-gpu`
//! alone (PM M214).
//!
//! On macOS, the krunkit fork, which renders in this process over MoltenVK. A blob of host
//! memory is held by this process from its making: its pages remapped (`mach_vm_remap`, not
//! copied) into a range of its own, which the window maps and which lives until the blob
//! does. The renderer frees a blob's Vulkan memory when the guest frees it, whether or not
//! the blob lives, and MoltenVK's next allocation takes the freed address (PM M216): mapped
//! from the renderer's address, a blob freed and then mapped would have given the guest
//! whatever took its place. A held range keeps the blob's own pages, which no later
//! allocation is given while any mapping of them lives (PM M216). A blob is mapped whole, in
//! host pages; its pages past its size are its allocation's alone (PM M216), and a blob the
//! guest says is larger than the memory it is of is refused, as the memory's region ends
//! before it.
//!
//! On Linux, virglrenderer 1.3.0, whose Venus contexts its render server serves: here a
//! thread of this process, given one end of a socket pair as its client's connection
//! (`get_server_fd`), each context a thread of it (D201). A blob of host memory is held by
//! the descriptor it is exported as, a dma-buf of the host's driver or shared memory, which
//! keeps its pages while this process holds it, the driver's free of it notwithstanding
//! (PM M381); the window maps it by that descriptor, whole host pages, and a blob the guest
//! says is larger than its memory is refused.
//!
//! On Windows, the krunkit fork as on macOS, rendering in this process, built for Windows
//! with shards' patch (scripts/gpu-libs, D202), over the host's Vulkan loader. A blob of
//! host memory is held by the renderer: every allocation its guest may map is of pages the
//! renderer allocates and its device imports (VK_EXT_external_memory_host), which the blob
//! holds until it goes, whatever the guest frees first; the window maps them where they
//! are, whole host pages, and a blob the guest says is larger than its pages is refused.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::{Mutex, PoisonError};
use std::thread::Thread;

use super::protocol::{Refused, resp};
use super::{Blob, Capset, HostMemory, Pages, Renderer, Signal};
use crate::vm::gpu_selection::{self as selection, PhysicalGpu};

/// The capability set of Venus (virglrenderer_hw.h, VIRGL_RENDERER_CAPSET_VENUS).
const CAPSET_VENUS: u32 = 4;
/// virglrenderer.h: VIRGL_RENDERER_VENUS, VIRGL_RENDERER_NO_VIRGL.
const VENUS: c_int = 1 << 6;
const NO_VIRGL: c_int = 1 << 7;
/// How the renderer starts: Venus alone, in this process (the krunkit fork).
#[cfg(any(target_os = "macos", windows))]
const FLAGS: c_int = VENUS | NO_VIRGL;
/// How the renderer starts: Venus alone, its contexts served by its render server
/// (VIRGL_RENDERER_RENDER_SERVER), each context's fences waited for on a thread of its own
/// (VIRGL_RENDERER_THREAD_SYNC) and said through `write_context_fence` from there
/// (VIRGL_RENDERER_ASYNC_FENCE_CB), as QEMU starts it for Venus.
#[cfg(target_os = "linux")]
const FLAGS: c_int = VENUS | NO_VIRGL | (1 << 9) | 2 | (1 << 8);
/// The callbacks' version that has `write_context_fence` (virglrenderer.h).
const CALLBACKS_VERSION: c_int = 3;
const FENCE_FLAG_MERGEABLE: u32 = 1 << 0;
/// VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE.
const BLOB_MAPPABLE: u32 = 1 << 0;
/// VIRTIO_GPU_BLOB_MEM_GUEST, which has no host memory.
const BLOB_MEM_GUEST: u32 = 1;
/// VIRGL_RENDERER_MAP_CACHE_MASK.
const MAP_CACHE_MASK: u32 = 0x0f;

/// `struct virgl_renderer_callbacks` (virglrenderer.h), to its version 4.
#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    create_gl_context: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> *mut c_void>,
    destroy_gl_context: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    make_current: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int>,
    get_drm_fd: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    write_context_fence: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<unsafe extern "C" fn(*mut c_void, u32) -> c_int>,
    get_egl_display: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
}

/// `struct iovec`, POSIX's, which virglrenderer declares itself where the host has none
/// (vrend_iov.h).
#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

/// `struct virgl_renderer_resource_create_blob_args`.
#[repr(C)]
struct CreateBlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const IoVec,
    num_iovs: u32,
}

unsafe extern "C" {
    fn virgl_renderer_init(cookie: *mut c_void, flags: c_int, cb: *mut Callbacks) -> c_int;
    fn virgl_renderer_cleanup(cookie: *mut c_void);
    fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
    fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);
    fn virgl_renderer_context_create_with_flags(
        ctx_id: u32,
        flags: u32,
        nlen: u32,
        name: *const c_char,
    ) -> c_int;
    fn virgl_renderer_context_destroy(handle: u32);
    fn virgl_renderer_context_set_physical_device_uuids(
        ctx: u32,
        uuids: *const *const c_char,
        count: u32,
    ) -> i32;
    fn shards_gpu_get_properties(
        properties: GetPhysicalDeviceProperties2,
        extensions: EnumerateDeviceExtensionProperties,
        device: *mut c_void,
        output: *mut selection::Properties,
    ) -> i32;
    fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
    fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);
    fn virgl_renderer_resource_create_blob(args: *const CreateBlobArgs) -> c_int;
    fn virgl_renderer_resource_unref(res_handle: u32);
    fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;
    fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
    fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
    #[cfg(any(target_os = "macos", windows))]
    fn virgl_renderer_resource_get_map_ptr(res_handle: u32, map_ptr: *mut u64) -> c_int;
    #[cfg(target_os = "linux")]
    fn virgl_renderer_resource_export_blob(res_id: u32, fd_type: *mut u32, fd: *mut c_int) -> c_int;
}

/// virglrenderer.h: VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF and _SHM, the descriptors a blob of
/// host memory is mapped by.
#[cfg(target_os = "linux")]
const FD_DMABUF: u32 = 1;
#[cfg(target_os = "linux")]
const FD_SHM: u32 = 3;

/// `struct render_context_args` (server/render_context.h): what `render_server_main` says
/// to a context's process, which a server whose contexts are threads never fills.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct ContextArgs {
    valid: bool,
    init_flags: u32,
    ctx_id: u32,
    ctx_name: [c_char; 32],
    ctx_fd: c_int,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    /// The render server (server/render_server.c), until its client's connection ends.
    fn render_server_main(argc: c_int, argv: *mut *mut c_char, ctx_args: *mut ContextArgs) -> bool;
    /// getopt(3)'s next argument, which a server's options are scanned from.
    static mut optind: c_int;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// This task's port, which the SDK's `mach_task_self()` reads (mach/mach_init.h).
    static mach_task_self_: libc::mach_port_t;
    fn mach_vm_region(
        target_task: libc::vm_map_t,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object_name: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn mach_vm_remap(
        target_task: libc::vm_map_t,
        target_address: *mut u64,
        size: u64,
        mask: u64,
        flags: c_int,
        src_task: libc::vm_map_t,
        src_address: u64,
        copy: libc::boolean_t,
        cur_protection: *mut libc::vm_prot_t,
        max_protection: *mut libc::vm_prot_t,
        inheritance: u32,
    ) -> libc::kern_return_t;
    fn mach_vm_deallocate(target: libc::vm_map_t, address: u64, size: u64) -> libc::kern_return_t;
}

/// `struct vm_region_basic_info_64` (mach/vm_region.h), flavor VM_REGION_BASIC_INFO_64.
#[cfg(target_os = "macos")]
#[repr(C, packed(4))]
#[derive(Default)]
struct BasicInfo {
    protection: i32,
    max_protection: i32,
    inheritance: u32,
    shared: u32,
    reserved: u32,
    offset: u64,
    behavior: i32,
    user_wired_count: u16,
}

/// mach/vm_region.h, mach/vm_statistics.h, mach/vm_inherit.h.
#[cfg(target_os = "macos")]
const VM_REGION_BASIC_INFO_64: i32 = 9;
#[cfg(target_os = "macos")]
const VM_FLAGS_ANYWHERE: c_int = 1;
#[cfg(target_os = "macos")]
const VM_INHERIT_NONE: u32 = 2;

/// Vulkan's, as MoltenVK exports it (vulkan_core.h): enough to read the host's memory heaps.
#[repr(C)]
struct InstanceCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    application: *const c_void,
    layers: u32,
    layer_names: *const *const c_char,
    extensions: u32,
    extension_names: *const *const c_char,
}

#[repr(C)]
struct ApplicationInfo {
    s_type: i32,
    next: *const c_void,
    application_name: *const c_char,
    application_version: u32,
    engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryType {
    property_flags: u32,
    heap_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryHeap {
    size: u64,
    flags: u32,
}

#[repr(C)]
struct MemoryProperties {
    type_count: u32,
    types: [MemoryType; 32],
    heap_count: u32,
    heaps: [MemoryHeap; 16],
}

const STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const MEMORY_PROPERTY_HOST_VISIBLE: u32 = 1 << 1;
const MEMORY_PROPERTY_DEVICE_LOCAL: u32 = 1 << 0;

/// `VkExtensionProperties` (vulkan_core.h).
#[repr(C)]
struct ExtensionProperties {
    name: [c_char; 256],
    spec_version: u32,
}

type CreateInstance = unsafe extern "C" fn(*const InstanceCreateInfo, *const c_void, *mut *mut c_void) -> i32;
type DestroyInstance = unsafe extern "C" fn(*mut c_void, *const c_void);
type EnumeratePhysicalDevices = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type GetPhysicalDeviceMemoryProperties = unsafe extern "C" fn(*mut c_void, *mut MemoryProperties);
type EnumerateDeviceExtensionProperties =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut u32, *mut ExtensionProperties) -> i32;
type GetPhysicalDeviceProperties2 = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn vkCreateInstance(
        info: *const InstanceCreateInfo,
        allocator: *const c_void,
        instance: *mut *mut c_void,
    ) -> i32;
    fn vkDestroyInstance(instance: *mut c_void, allocator: *const c_void);
    fn vkEnumeratePhysicalDevices(instance: *mut c_void, count: *mut u32, devices: *mut *mut c_void) -> i32;
    fn vkGetPhysicalDeviceMemoryProperties(device: *mut c_void, properties: *mut MemoryProperties);
    fn vkGetPhysicalDeviceProperties2(device: *mut c_void, properties: *mut c_void);
    fn vkEnumerateDeviceExtensionProperties(
        device: *mut c_void,
        layer: *const c_char,
        count: *mut u32,
        properties: *mut ExtensionProperties,
    ) -> i32;
}

/// The Vulkan entry points [`host_visible`] calls: MoltenVK's, linked, on macOS; on Linux
/// the host's loader's, opened by its soname (libvulkan.so.1), as virglrenderer opens it,
/// and on Windows from System32 (vulkan-1.dll), as the renderer opens it there; closed as
/// this goes.
struct Vulkan {
    create_instance: CreateInstance,
    destroy_instance: DestroyInstance,
    enumerate_physical_devices: EnumeratePhysicalDevices,
    get_physical_device_memory_properties: GetPhysicalDeviceMemoryProperties,
    enumerate_device_extension_properties: EnumerateDeviceExtensionProperties,
    get_physical_device_properties2: GetPhysicalDeviceProperties2,
    #[cfg(target_os = "linux")]
    loader: *mut c_void,
    #[cfg(windows)]
    loader: windows_sys::Win32::Foundation::HMODULE,
}

impl Vulkan {
    #[cfg(target_os = "macos")]
    fn open() -> Result<Vulkan, String> {
        Ok(Vulkan {
            create_instance: vkCreateInstance,
            destroy_instance: vkDestroyInstance,
            enumerate_physical_devices: vkEnumeratePhysicalDevices,
            get_physical_device_memory_properties: vkGetPhysicalDeviceMemoryProperties,
            enumerate_device_extension_properties: vkEnumerateDeviceExtensionProperties,
            get_physical_device_properties2: vkGetPhysicalDeviceProperties2,
        })
    }

    #[cfg(target_os = "linux")]
    fn open() -> Result<Vulkan, String> {
        let said = || {
            // SAFETY: dlerror(3) of this thread's last dl call, copied before the next.
            let e = unsafe { libc::dlerror() };
            if e.is_null() {
                String::new()
            } else {
                // SAFETY: a NUL-terminated message of the dynamic linker's.
                unsafe { std::ffi::CStr::from_ptr(e) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        // SAFETY: dlopen(3) of the loader by its soname.
        let loader = unsafe { libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if loader.is_null() {
            return Err(format!(
                "this host has no Vulkan loader, which Venus renders through: {}",
                said()
            ));
        }
        let entry = |name: &std::ffi::CStr| {
            // SAFETY: dlsym(3) of the loader just opened.
            let p = unsafe { libc::dlsym(loader, name.as_ptr()) };
            if p.is_null() {
                Err(format!(
                    "the Vulkan loader has no {}: {}",
                    name.to_string_lossy(),
                    said()
                ))
            } else {
                Ok(p)
            }
        };
        let found = (|| {
            Ok::<_, String>((
                entry(c"vkCreateInstance")?,
                entry(c"vkDestroyInstance")?,
                entry(c"vkEnumeratePhysicalDevices")?,
                entry(c"vkGetPhysicalDeviceMemoryProperties")?,
                entry(c"vkEnumerateDeviceExtensionProperties")?,
                entry(c"vkGetPhysicalDeviceProperties2")?,
            ))
        })();
        let (create, destroy, enumerate, properties, extensions, properties2) = match found {
            Ok(found) => found,
            Err(e) => {
                // SAFETY: the handle opened above, used no more.
                unsafe { libc::dlclose(loader) };
                return Err(e);
            }
        };
        // SAFETY: each the loader's own entry point of that name, which Vulkan declares of
        // the type it is given (vulkan_core.h).
        unsafe {
            Ok(Vulkan {
                create_instance: std::mem::transmute::<*mut c_void, CreateInstance>(create),
                destroy_instance: std::mem::transmute::<*mut c_void, DestroyInstance>(destroy),
                enumerate_physical_devices: std::mem::transmute::<*mut c_void, EnumeratePhysicalDevices>(
                    enumerate,
                ),
                get_physical_device_memory_properties: std::mem::transmute::<
                    *mut c_void,
                    GetPhysicalDeviceMemoryProperties,
                >(properties),
                enumerate_device_extension_properties: std::mem::transmute::<
                    *mut c_void,
                    EnumerateDeviceExtensionProperties,
                >(extensions),
                get_physical_device_properties2: std::mem::transmute::<
                    *mut c_void,
                    GetPhysicalDeviceProperties2,
                >(properties2),
                loader,
            })
        }
    }
}

#[cfg(windows)]
impl Vulkan {
    fn open() -> Result<Vulkan, String> {
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        };
        let name: Vec<u16> = "vulkan-1.dll".encode_utf16().chain([0]).collect();
        // SAFETY: the loader by its name, from System32 alone.
        let loader =
            unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if loader.is_null() {
            return Err(format!(
                "this host has no Vulkan loader (vulkan-1.dll in System32), which Venus renders through: {}",
                std::io::Error::last_os_error()
            ));
        }
        let entry = |name: &std::ffi::CStr| {
            // SAFETY: an entry point of the loader just opened, by its NUL-terminated name.
            unsafe { GetProcAddress(loader, name.as_ptr().cast()) }.ok_or_else(|| {
                format!(
                    "the Vulkan loader has no {}: {}",
                    name.to_string_lossy(),
                    std::io::Error::last_os_error()
                )
            })
        };
        let found = (|| {
            Ok::<_, String>((
                entry(c"vkCreateInstance")?,
                entry(c"vkDestroyInstance")?,
                entry(c"vkEnumeratePhysicalDevices")?,
                entry(c"vkGetPhysicalDeviceMemoryProperties")?,
                entry(c"vkEnumerateDeviceExtensionProperties")?,
                entry(c"vkGetPhysicalDeviceProperties2")?,
            ))
        })();
        let (create, destroy, enumerate, properties, extensions, properties2) = match found {
            Ok(found) => found,
            Err(e) => {
                // SAFETY: the handle opened above, used no more.
                unsafe { FreeLibrary(loader) };
                return Err(e);
            }
        };
        type Proc = unsafe extern "system" fn() -> isize;
        // SAFETY: each the loader's own entry point of that name, which Vulkan declares of
        // the type it is given (vulkan_core.h; VKAPI_CALL is the platform's convention).
        unsafe {
            Ok(Vulkan {
                create_instance: std::mem::transmute::<Proc, CreateInstance>(create),
                destroy_instance: std::mem::transmute::<Proc, DestroyInstance>(destroy),
                enumerate_physical_devices: std::mem::transmute::<Proc, EnumeratePhysicalDevices>(enumerate),
                get_physical_device_memory_properties: std::mem::transmute::<
                    Proc,
                    GetPhysicalDeviceMemoryProperties,
                >(properties),
                enumerate_device_extension_properties: std::mem::transmute::<
                    Proc,
                    EnumerateDeviceExtensionProperties,
                >(extensions),
                get_physical_device_properties2: std::mem::transmute::<Proc, GetPhysicalDeviceProperties2>(
                    properties2,
                ),
                loader,
            })
        }
    }

    /// Whether `device` imports memory of the host's own (VK_EXT_external_memory_host),
    /// which every allocation a guest maps is of on Windows (D202).
    fn imports_host_memory(&self, device: *mut c_void) -> bool {
        let mut count = 0u32;
        // SAFETY: the count alone asked, of the device's own extensions.
        let r = unsafe {
            (self.enumerate_device_extension_properties)(
                device,
                std::ptr::null(),
                &raw mut count,
                std::ptr::null_mut(),
            )
        };
        if r != 0 {
            return false;
        }
        let mut found: Vec<ExtensionProperties> = (0..count)
            .map(|_| ExtensionProperties {
                name: [0; 256],
                spec_version: 0,
            })
            .collect();
        let mut count = found.len() as u32;
        // SAFETY: room for `count` properties; VK_INCOMPLETE where there are more, which
        // reads those it has.
        let r = unsafe {
            (self.enumerate_device_extension_properties)(
                device,
                std::ptr::null(),
                &raw mut count,
                found.as_mut_ptr(),
            )
        };
        (r == 0 || r == 5)
            && found.iter().take(count as usize).any(|e| {
                // SAFETY: a NUL-terminated name within its array (vulkan_core.h).
                unsafe { std::ffi::CStr::from_ptr(e.name.as_ptr()) }.to_bytes()
                    == b"VK_EXT_external_memory_host"
            })
    }
}

#[cfg(target_os = "linux")]
impl Drop for Vulkan {
    fn drop(&mut self) {
        // SAFETY: the loader this value opened, its entry points used no more.
        unsafe { libc::dlclose(self.loader) };
    }
}

#[cfg(windows)]
impl Drop for Vulkan {
    fn drop(&mut self) {
        // SAFETY: the loader this value opened, its entry points used no more.
        unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.loader) };
    }
}

/// The bytes of the host GPUs' heaps that host-visible memory comes from, as Vulkan says
/// them, which Venus tells the guest, every GPU's counted: the most memory a guest can
/// have mapped into its window at once. On Windows those of the memory the renderer shows
/// its guest as host-visible: a device's that imports host memory, its types not
/// device-local (D202).
type UuidText = [c_char; 33]; // Vulkan UUID16 -> hex32 plus the C terminator.

fn uuid_text(uuid: &[u8; 16]) -> Result<UuidText, String> {
    let mut text = [0; 33];
    let digits = b"0123456789abcdef";
    for (slot, nibble) in text
        .iter_mut()
        .zip(uuid.iter().flat_map(|byte| [byte >> 4, byte & 0xf]))
    {
        let digit = digits.get(usize::from(nibble)).ok_or("invalid UUID hex nibble")?;
        *slot = c_char::try_from(*digit).map_err(|_| "UUID ASCII is not representable")?;
    }
    Ok(text)
}

fn host_visible(selected: &[PhysicalGpu]) -> Result<(u64, Vec<UuidText>), String> {
    selection::check_selection(selected).map_err(|e| e.to_string())?;
    let vk = Vulkan::open()?;
    // Vulkan 1.1 defines ID properties and Properties2. Venus itself requires
    // Vulkan's device/driver capabilities; no older identity fallback is allowed.
    let application = ApplicationInfo {
        s_type: 0,
        next: std::ptr::null(),
        application_name: std::ptr::null(),
        application_version: 0,
        engine_name: std::ptr::null(),
        engine_version: 0,
        api_version: (1 << 22) | (1 << 12),
    };
    let info = InstanceCreateInfo {
        s_type: STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        next: std::ptr::null(),
        flags: 0,
        application: (&raw const application).cast(),
        layers: 0,
        layer_names: std::ptr::null(),
        extensions: 0,
        extension_names: std::ptr::null(),
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: complete ABI structures and a live output, destroyed after every path below.
    let result = unsafe { (vk.create_instance)(&raw const info, std::ptr::null(), &raw mut instance) };
    if result != 0 || instance.is_null() {
        return Err(format!("Vulkan made no instance ({result})"));
    }
    let checked = (|| {
        let mut count = 0u32;
        // SAFETY: count-only enumeration of this instance.
        let result =
            unsafe { (vk.enumerate_physical_devices)(instance, &raw mut count, std::ptr::null_mut()) };
        if result != 0 || count == 0 {
            return Err(format!("Vulkan has no complete device list ({result})"));
        }
        let count_usize = usize::try_from(count).map_err(|_| "Vulkan device count overflows")?;
        if count_usize > selection::MAX_PHYSICAL_GPUS {
            return Err("Vulkan exceeds the physical device-index space".into());
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(count_usize)
            .map_err(|_| "Vulkan device storage is unavailable")?;
        devices.resize(count_usize, std::ptr::null_mut());
        // SAFETY: exactly the driver's advertised capacity; an incomplete/changing catalog refuses.
        let result =
            unsafe { (vk.enumerate_physical_devices)(instance, &raw mut count, devices.as_mut_ptr()) };
        let written = usize::try_from(count).map_err(|_| "Vulkan device count overflows")?;
        if result != 0 || written > devices.len() {
            return Err(format!("Vulkan device enumeration changed ({result})"));
        }
        devices.truncate(written);
        if devices.iter().any(|device| device.is_null()) {
            return Err("Vulkan returned a null physical device".into());
        }
        let mut properties = Vec::new();
        if !selected.is_empty() {
            properties
                .try_reserve_exact(devices.len())
                .map_err(|_| "Vulkan identity storage is unavailable")?;
            for device in &devices {
                let mut props = selection::Properties::default();
                // SAFETY: the helper uses the pinned C header's complete Properties2 ABI,
                // loader entry points and a live instance's physical device; output is repr(C).
                let result = unsafe {
                    shards_gpu_get_properties(
                        vk.get_physical_device_properties2,
                        vk.enumerate_device_extension_properties,
                        *device,
                        &raw mut props,
                    )
                };
                if result != 0 {
                    return Err(format!("Vulkan native GPU identity query failed ({result})"));
                }
                properties.push(props);
            }
        }
        let indices = if selected.is_empty() {
            let mut indices = Vec::new();
            indices
                .try_reserve_exact(devices.len())
                .map_err(|_| "Vulkan memory selection storage is unavailable")?;
            indices.extend(0..devices.len());
            indices
        } else {
            selection::resolve(selected, &properties, cfg!(target_os = "macos")).map_err(|e| e.to_string())?
        };
        let mut uuids = Vec::new();
        uuids
            .try_reserve_exact(selected.len())
            .map_err(|_| "Vulkan UUID selection storage is unavailable")?;
        let mut total = 0u64;
        for index in indices {
            let device = *devices.get(index).ok_or("Vulkan selection index is invalid")?;
            if !selected.is_empty() {
                uuids.push(uuid_text(
                    &properties
                        .get(index)
                        .ok_or("Vulkan selection identity is missing")?
                        .uuid,
                )?);
            }
            #[cfg(windows)]
            if if selected.is_empty() {
                !vk.imports_host_memory(device)
            } else {
                properties
                    .get(index)
                    .is_none_or(|props| props.host_memory_valid == 0)
            } {
                continue;
            }
            let mut props = MemoryProperties {
                type_count: 0,
                types: [MemoryType::default(); 32],
                heap_count: 0,
                heaps: [MemoryHeap::default(); 16],
            };
            // SAFETY: a live physical device and complete memory-properties structure.
            unsafe { (vk.get_physical_device_memory_properties)(device, &raw mut props) };
            let types = props
                .types
                .get(..usize::try_from(props.type_count).map_err(|_| "Vulkan memory-type count overflows")?)
                .ok_or("Vulkan memory-type count is invalid")?;
            let heaps = props
                .heaps
                .get(..usize::try_from(props.heap_count).map_err(|_| "Vulkan heap count overflows")?)
                .ok_or("Vulkan heap count is invalid")?;
            let mut counted = [false; 16]; // VK_MAX_MEMORY_HEAPS.
            for kind in types
                .iter()
                .filter(|kind| kind.property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0)
                .filter(|kind| cfg!(not(windows)) || kind.property_flags & MEMORY_PROPERTY_DEVICE_LOCAL == 0)
            {
                let index = usize::try_from(kind.heap_index).map_err(|_| "Vulkan heap index overflows")?;
                let seen = counted.get_mut(index).ok_or("Vulkan heap index is invalid")?;
                if !*seen {
                    let heap = heaps
                        .get(index)
                        .ok_or("Vulkan memory type references a missing heap")?;
                    total = total
                        .checked_add(heap.size)
                        .ok_or("selected host-visible GPU memory overflows")?;
                    *seen = true;
                }
            }
        }
        Ok((total, uuids))
    })();
    // SAFETY: the instance made above, no physical handle is retained beyond this call.
    unsafe { (vk.destroy_instance)(instance, std::ptr::null()) };
    checked
}

/// What the renderer's threads tell the device: the fences signalled, and the worker to
/// wake for them. Its address is the renderer's cookie.
#[derive(Default)]
struct Shared {
    signals: Mutex<Vec<Signal>>,
    worker: Mutex<Option<Thread>>,
    /// The client's end of the render server's connection, until the renderer takes it.
    #[cfg(target_os = "linux")]
    server: std::sync::atomic::AtomicI32,
}

/// A fence of context `ctx`'s `ring` signalled, on whichever thread the renderer signals
/// it (its rings' sync threads, or the caller's for ring 0).
unsafe extern "C" fn write_context_fence(cookie: *mut c_void, ctx: u32, ring: u32, fence: u64) {
    // SAFETY: the cookie is the renderer's `Shared`, which outlives the renderer
    // (`Drop for Venus` cleans the renderer up before it is freed).
    let shared = unsafe { &*cookie.cast::<Shared>() };
    let Ok(ring) = u8::try_from(ring) else { return };
    shared
        .signals
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Signal { ctx, ring, fence });
    if let Some(w) = shared
        .worker
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        w.unpark();
    }
}

/// The device timeline's fences: Venus creates none (`virgl_renderer_create_fence` is
/// virgl's), and the device answers them itself.
unsafe extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {}

/// The render server's connection, its client's end, given the renderer once: it owns it
/// then (proxy_server.c, `proxy_server_init_fd`).
#[cfg(target_os = "linux")]
unsafe extern "C" fn get_server_fd(cookie: *mut c_void, _version: u32) -> c_int {
    // SAFETY: the cookie is the renderer's `Shared`, as for `write_context_fence`.
    let shared = unsafe { &*cookie.cast::<Shared>() };
    shared.server.swap(-1, std::sync::atomic::Ordering::AcqRel)
}

/// A blob's host memory, held: `len` bytes at `at` that map its pages, its own until it
/// goes.
#[cfg(target_os = "macos")]
struct Held {
    at: u64,
    len: u64,
    cache: u32,
}

#[cfg(target_os = "macos")]
impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: a range this process remapped for the blob and nothing else holds.
        unsafe { mach_vm_deallocate(mach_task_self_, self.at, self.len) };
    }
}

/// A blob's host memory, held: the descriptor it was exported as, a dma-buf or shared
/// memory, `len` bytes of which map it, which keeps its pages until it closes.
#[cfg(target_os = "linux")]
struct Held {
    fd: OwnedFd,
    len: u64,
    cache: u32,
}

/// A blob's host memory: `len` bytes at `at`, its pages, which the renderer holds until
/// the blob goes (D202).
#[cfg(windows)]
struct Held {
    at: u64,
    len: u64,
    cache: u32,
}

/// The renderer, once a process.
pub struct Venus {
    shared: Box<Shared>,
    capsets: [Capset; 1],
    /// Each blob's guest memory as the renderer holds it, which must live as long as the
    /// blob does: virglrenderer keeps the array's address (virgl_resource.c).
    iovecs: HashMap<u32, Box<[IoVec]>>,
    held: HashMap<u32, Held>,
    contexts: Vec<u32>,
    page: u64,
    host_visible: u64,
    selected: Vec<UuidText>,
    /// The render server's thread, which ends as the renderer lets its connection go.
    #[cfg(target_os = "linux")]
    server: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: the renderer's state is process-global and every call is made on the device's
// worker, one at a time; the iovecs' pointers are guest RAM's, which outlives the renderer.
unsafe impl Send for Venus {}

impl std::fmt::Debug for Venus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Venus")
            .field("contexts", &self.contexts)
            .field("blobs", &self.iovecs.len())
            .finish_non_exhaustive()
    }
}

static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Venus {
    /// Starts the renderer, Venus alone: once a process.
    pub fn new() -> Result<Venus, String> {
        Self::new_selected(&[])
    }

    pub fn new_selected(selected: &[PhysicalGpu]) -> Result<Venus, String> {
        if STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err("the GPU renderer runs once a process".into());
        }
        let page = crate::platform::page_size().map_err(|e| format!("the host's page size: {e}"))? as u64;
        let (host_visible, selected) = host_visible(selected)
            .and_then(|b| match b {
                (0, _) => Err("the selected GPU has no memory the host can map, which Venus needs".into()),
                b => Ok(b),
            })
            .inspect_err(|_| STARTED.store(false, std::sync::atomic::Ordering::Release))?;
        let mut shared = Box::new(Shared::default());
        #[cfg(target_os = "linux")]
        let server = match render_server() {
            Ok((client, server)) => {
                shared
                    .server
                    .store(client.into_raw_fd(), std::sync::atomic::Ordering::Release);
                Some(server)
            }
            Err(e) => {
                STARTED.store(false, std::sync::atomic::Ordering::Release);
                return Err(e);
            }
        };
        let mut callbacks = Callbacks {
            version: CALLBACKS_VERSION,
            write_fence: Some(write_fence),
            create_gl_context: None,
            destroy_gl_context: None,
            make_current: None,
            get_drm_fd: None,
            write_context_fence: Some(write_context_fence),
            #[cfg(not(target_os = "linux"))]
            get_server_fd: None,
            #[cfg(target_os = "linux")]
            get_server_fd: Some(get_server_fd),
            get_egl_display: None,
        };
        let cookie = std::ptr::from_mut::<Shared>(&mut shared).cast::<c_void>();
        // SAFETY: the callbacks are copied by the renderer; the cookie lives in `shared`,
        // which this value holds until it cleans the renderer up.
        let r = unsafe { virgl_renderer_init(cookie, FLAGS, &raw mut callbacks) };
        if r != 0 {
            // The server's connection, if the renderer never took it, and so its thread.
            #[cfg(target_os = "linux")]
            {
                let fd = shared.server.swap(-1, std::sync::atomic::Ordering::AcqRel);
                if fd >= 0 {
                    // SAFETY: the client's end, which nothing else owns now.
                    drop(unsafe { OwnedFd::from_raw_fd(fd) });
                }
                // SAFETY: the renderer's own state, cleaned up as a failed init leaves it.
                unsafe { virgl_renderer_cleanup(cookie) };
                if let Some(t) = server {
                    let _ = t.join();
                }
            }
            STARTED.store(false, std::sync::atomic::Ordering::Release);
            return Err(format!("the GPU renderer did not start: {}", errno(r)));
        }
        let (mut version, mut size) = (0u32, 0u32);
        // SAFETY: two outputs of the renderer's.
        unsafe { virgl_renderer_get_cap_set(CAPSET_VENUS, &raw mut version, &raw mut size) };
        let venus = Venus {
            shared,
            capsets: [Capset {
                id: CAPSET_VENUS,
                max_version: version,
                max_size: size,
            }],
            iovecs: HashMap::new(),
            held: HashMap::new(),
            contexts: Vec::new(),
            page,
            host_visible,
            selected,
            #[cfg(target_os = "linux")]
            server,
        };
        if size == 0 {
            return Err("the GPU renderer offers no Venus capability set: no Vulkan device".into());
        }
        Ok(venus)
    }

    /// The bytes of the host GPU's heaps of host-visible memory: the window's length.
    pub fn host_visible(&self) -> u64 {
        self.host_visible
    }

    /// Holds `resource`'s host memory, `size` bytes of it whole pages, where the guest may
    /// map it: refused if its region ends before its size does.
    #[cfg(target_os = "macos")]
    fn hold(&mut self, resource: u32, size: u64) -> Result<(), Refused> {
        let mut ptr = 0u64;
        // SAFETY: an output of the renderer's.
        if unsafe { virgl_renderer_resource_get_map_ptr(resource, &raw mut ptr) } != 0 || ptr == 0 {
            return Ok(());
        }
        let len = size
            .checked_next_multiple_of(self.page)
            .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?;
        let (mut start, mut region) = (ptr, 0u64);
        let mut info = BasicInfo::default();
        let mut count = (std::mem::size_of::<BasicInfo>() / 4) as u32;
        let mut object = 0;
        // SAFETY: mach_vm_region(2) of this task, filling `info` for the region at or past
        // `ptr`.
        let kr = unsafe {
            mach_vm_region(
                mach_task_self_,
                &raw mut start,
                &raw mut region,
                VM_REGION_BASIC_INFO_64,
                (&raw mut info).cast(),
                &raw mut count,
                &raw mut object,
            )
        };
        let within = start <= ptr
            && ptr
                .checked_add(len)
                .is_some_and(|end| end <= start.saturating_add(region));
        if kr != 0 || !within || !ptr.is_multiple_of(self.page) {
            return Err(Refused(resp::ERR_INVALID_PARAMETER));
        }
        let mut cache = 0;
        // SAFETY: an output of the renderer's.
        unsafe { virgl_renderer_resource_get_map_info(resource, &raw mut cache) };
        let (mut at, mut cur, mut max) = (0u64, 0, 0);
        // SAFETY: a remap of this task's own pages, shared (not copied), at an address the
        // kernel chooses.
        let kr = unsafe {
            mach_vm_remap(
                mach_task_self_,
                &raw mut at,
                len,
                0,
                VM_FLAGS_ANYWHERE,
                mach_task_self_,
                ptr,
                0,
                &raw mut cur,
                &raw mut max,
                VM_INHERIT_NONE,
            )
        };
        if kr != 0 {
            return Err(Refused(resp::ERR_OUT_OF_MEMORY));
        }
        self.held.insert(
            resource,
            Held {
                at,
                len,
                cache: cache & MAP_CACHE_MASK,
            },
        );
        Ok(())
    }

    /// Holds `resource`'s host memory, `size` bytes of it whole pages, where the guest may
    /// map it: the descriptor the renderer exports it as, a dma-buf or shared memory;
    /// refused if the memory ends before its size does, or is of a descriptor that maps
    /// nowhere (an opaque one, PM M381).
    #[cfg(target_os = "linux")]
    fn hold(&mut self, resource: u32, size: u64) -> Result<(), Refused> {
        let (mut kind, mut fd) = (0u32, -1);
        // SAFETY: two outputs of the renderer's; the descriptor is a new one, this
        // process's to close.
        if unsafe { virgl_renderer_resource_export_blob(resource, &raw mut kind, &raw mut fd) } != 0 || fd < 0
        {
            return Ok(());
        }
        // SAFETY: the descriptor just exported, which nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let len = size
            .checked_next_multiple_of(self.page)
            .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?;
        // A dma-buf's size and a shared memory file's are where their ends are
        // (dma_buf_llseek takes SEEK_END alone), its pages to the last one its end is in.
        // SAFETY: lseek(2) of the descriptor held.
        let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        let fits = u64::try_from(end)
            .ok()
            .and_then(|end| end.checked_next_multiple_of(self.page))
            .is_some_and(|end| len <= end);
        if !matches!(kind, FD_DMABUF | FD_SHM) || !fits {
            return Err(Refused(resp::ERR_INVALID_PARAMETER));
        }
        let mut cache = 0;
        // SAFETY: an output of the renderer's.
        unsafe { virgl_renderer_resource_get_map_info(resource, &raw mut cache) };
        self.held.insert(
            resource,
            Held {
                fd,
                len,
                cache: cache & MAP_CACHE_MASK,
            },
        );
        Ok(())
    }

    /// Holds `resource`'s host memory, `size` bytes of it whole pages, where the guest may
    /// map it: the pages the renderer made it of, which it holds until the blob goes
    /// (D202); refused if they end before its size does, or are not this process's
    /// committed, writable memory.
    #[cfg(windows)]
    fn hold(&mut self, resource: u32, size: u64) -> Result<(), Refused> {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
        };
        let mut ptr = 0u64;
        // SAFETY: an output of the renderer's.
        if unsafe { virgl_renderer_resource_get_map_ptr(resource, &raw mut ptr) } != 0 || ptr == 0 {
            return Ok(());
        }
        let len = size
            .checked_next_multiple_of(self.page)
            .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?;
        // SAFETY: an all-zero MEMORY_BASIC_INFORMATION is a valid one, filled below.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: VirtualQuery of this process's address, into a whole structure.
        let got = unsafe {
            VirtualQuery(
                ptr as *const c_void,
                &raw mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        // The region of pages alike from the blob's first, which its last must be within.
        let start = info.BaseAddress as u64;
        let within = got != 0
            && info.State == MEM_COMMIT
            && info.Protect == PAGE_READWRITE
            && start <= ptr
            && ptr
                .checked_add(len)
                .is_some_and(|end| end <= start.saturating_add(info.RegionSize as u64));
        if !within || !ptr.is_multiple_of(self.page) {
            return Err(Refused(resp::ERR_INVALID_PARAMETER));
        }
        let mut cache = 0;
        // SAFETY: an output of the renderer's.
        unsafe { virgl_renderer_resource_get_map_info(resource, &raw mut cache) };
        self.held.insert(
            resource,
            Held {
                at: ptr,
                len,
                cache: cache & MAP_CACHE_MASK,
            },
        );
        Ok(())
    }
}

/// The render server, on a thread of this process, and its client's end of its connection,
/// a socket pair of SOCK_SEQPACKET as the server takes it (`--socket-fd`): a context the
/// client makes the server serves on a thread of its own (render-server-worker=thread).
#[cfg(target_os = "linux")]
fn render_server() -> Result<(OwnedFd, std::thread::JoinHandle<()>), String> {
    let mut fds = [-1; 2];
    // SAFETY: socketpair(2) into a two-descriptor array.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(format!(
            "the GPU's render server's connection: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the pair just made, each end owned once.
    let (client, server) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let socket = std::ffi::CString::new(server.as_raw_fd().to_string()).map_err(|e| e.to_string())?;
    let thread = std::thread::Builder::new()
        .name("venus-server".into())
        .spawn(move || {
            // The server owns its end from here, and closes it as it ends
            // (render_server.c, `render_server_init`).
            let _ = server.into_raw_fd();
            let mut argv = [
                c"virgl_render_server".as_ptr().cast_mut(),
                c"--socket-fd".as_ptr().cast_mut(),
                socket.as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ];
            let mut args = ContextArgs::default();
            // SAFETY: getopt(3)'s scan started afresh, as only this thread scans options
            // (glibc and musl reset theirs at 0); then the server, with its arguments, which
            // live until it returns.
            let served = unsafe {
                optind = 0;
                render_server_main(3, argv.as_mut_ptr(), &raw mut args)
            };
            // It ends as its connection does, which it takes for a failure
            // (render_server.c, `render_server_run`).
            crate::debug!("virtio-gpu: the render server ended ({served})");
        })
        .map_err(|e| format!("the GPU's render server: {e}"))?;
    Ok((client, thread))
}

/// A negative errno the renderer returned, said.
#[cfg(unix)]
fn errno(r: c_int) -> std::io::Error {
    std::io::Error::from_raw_os_error(r.saturating_abs())
}

/// A negative errno the renderer returned, its C library's (the UCRT's), which Windows'
/// own codes are not.
#[cfg(windows)]
fn errno(r: c_int) -> std::io::Error {
    std::io::Error::other(format!("errno {}", r.saturating_abs()))
}

/// Refused as the renderer refuses: what it did not take.
fn refused(r: c_int) -> Result<(), Refused> {
    if r == 0 {
        Ok(())
    } else {
        Err(Refused(resp::ERR_INVALID_PARAMETER))
    }
}

impl Renderer for Venus {
    fn capsets(&self) -> &[Capset] {
        &self.capsets
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, Refused> {
        let c = self
            .capsets
            .iter()
            .find(|c| c.id == id)
            .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?;
        let mut caps = vec![0u8; c.max_size as usize];
        // SAFETY: a buffer of the capset's size, as the renderer said it.
        unsafe { virgl_renderer_fill_caps(id, version, caps.as_mut_ptr().cast()) };
        Ok(caps)
    }

    fn create_context(&mut self, ctx: u32, capset: u32, name: &[u8]) -> Result<(), Refused> {
        if self.contexts.contains(&ctx) {
            return Err(Refused(resp::ERR_INVALID_PARAMETER));
        }
        self.contexts
            .try_reserve(1)
            .map_err(|_| Refused(resp::ERR_OUT_OF_MEMORY))?;
        let mut pointers = Vec::new();
        pointers
            .try_reserve_exact(self.selected.len())
            .map_err(|_| Refused(resp::ERR_OUT_OF_MEMORY))?;
        pointers.extend(self.selected.iter().map(|uuid| uuid.as_ptr()));
        let count = u32::try_from(pointers.len()).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
        let nlen = u32::try_from(name.len()).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
        // SAFETY: the name's bytes, `nlen` of them, which the renderer copies.
        let r = unsafe { virgl_renderer_context_create_with_flags(ctx, capset, nlen, name.as_ptr().cast()) };
        refused(r)?;
        if !pointers.is_empty() {
            // SAFETY: a newly created context, serialized on this device's worker;
            // immutable NUL-terminated UUID arrays live through the synchronous,
            // typed ACK. The C owner copies them before accepting guest commands.
            let result =
                unsafe { virgl_renderer_context_set_physical_device_uuids(ctx, pointers.as_ptr(), count) };
            if result != 0 {
                // SAFETY: no command/resource has escaped for this owned context.
                unsafe { virgl_renderer_context_destroy(ctx) };
                return Err(Refused(if result == -1 {
                    resp::ERR_OUT_OF_MEMORY
                } else {
                    resp::ERR_INVALID_PARAMETER
                }));
            }
        }
        self.contexts.push(ctx);
        Ok(())
    }

    fn destroy_context(&mut self, ctx: u32) {
        // SAFETY: a context of the device's, which it destroys once.
        unsafe { virgl_renderer_context_destroy(ctx) };
        self.contexts.retain(|&c| c != ctx);
    }

    fn attach(&mut self, ctx: u32, resource: u32) {
        let (Ok(ctx), Ok(resource)) = (c_int::try_from(ctx), c_int::try_from(resource)) else {
            return;
        };
        // SAFETY: plain arguments; the renderer checks both.
        unsafe { virgl_renderer_ctx_attach_resource(ctx, resource) };
    }

    fn detach(&mut self, ctx: u32, resource: u32) {
        let (Ok(ctx), Ok(resource)) = (c_int::try_from(ctx), c_int::try_from(resource)) else {
            return;
        };
        // SAFETY: plain arguments; the renderer checks both.
        unsafe { virgl_renderer_ctx_detach_resource(ctx, resource) };
    }

    fn create_blob(&mut self, ctx: u32, blob: &Blob, guest: &[(*mut u8, usize)]) -> Result<(), Refused> {
        let iovecs: Box<[IoVec]> = guest
            .iter()
            .map(|&(base, len)| IoVec {
                base: base.cast(),
                len,
            })
            .collect();
        let args = CreateBlobArgs {
            res_handle: blob.resource,
            ctx_id: ctx,
            blob_mem: blob.mem,
            blob_flags: blob.flags,
            blob_id: blob.id,
            size: blob.size,
            iovecs: if iovecs.is_empty() {
                std::ptr::null()
            } else {
                iovecs.as_ptr()
            },
            num_iovs: u32::try_from(iovecs.len()).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?,
        };
        // SAFETY: arguments of the blob's, its iovecs held below for as long as it lives.
        refused(unsafe { virgl_renderer_resource_create_blob(&raw const args) })?;
        self.iovecs.insert(blob.resource, iovecs);
        if blob.mem != BLOB_MEM_GUEST
            && blob.flags & BLOB_MAPPABLE != 0
            && let Err(e) = self.hold(blob.resource, blob.size)
        {
            self.unref(blob.resource);
            return Err(e);
        }
        Ok(())
    }

    fn unref(&mut self, resource: u32) {
        // SAFETY: a resource of the device's, which it unreferences once.
        unsafe { virgl_renderer_resource_unref(resource) };
        self.iovecs.remove(&resource);
        self.held.remove(&resource);
    }

    fn submit(&mut self, ctx: u32, commands: &mut [u8]) -> Result<(), Refused> {
        let ctx = c_int::try_from(ctx).map_err(|_| Refused(resp::ERR_INVALID_CONTEXT_ID))?;
        let words = c_int::try_from(commands.len() / 4).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
        // SAFETY: the device's copy of the guest's commands, `words` 32-bit words.
        refused(unsafe { virgl_renderer_submit_cmd(commands.as_mut_ptr().cast(), ctx, words) })
    }

    fn fence(&mut self, ctx: u32, ring: u8, fence: u64) -> Result<(), Refused> {
        // SAFETY: plain arguments; the renderer checks the context and its ring.
        refused(unsafe {
            virgl_renderer_context_create_fence(ctx, FENCE_FLAG_MERGEABLE, u32::from(ring), fence)
        })
    }

    fn signals(&mut self, into: &mut Vec<Signal>) {
        into.append(&mut self.shared.signals.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn wake(&mut self, worker: Thread) {
        *self.shared.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
    }

    fn map(&mut self, resource: u32) -> Result<HostMemory, Refused> {
        let held = self
            .held
            .get(&resource)
            .ok_or(Refused(resp::ERR_INVALID_PARAMETER))?;
        #[cfg(any(target_os = "macos", windows))]
        let pages = Pages::At(held.at as *mut u8);
        #[cfg(target_os = "linux")]
        let pages = Pages::Fd(held.fd.as_raw_fd());
        Ok(HostMemory {
            pages,
            len: usize::try_from(held.len).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?,
            cache: held.cache,
        })
    }

    /// The window lets go of its mapping; the blob keeps its pages until it goes.
    fn unmap(&mut self, _resource: u32) {}

    fn reset(&mut self) {
        let resources: Vec<u32> = self.iovecs.keys().copied().collect();
        for r in resources {
            self.unref(r);
        }
        for ctx in std::mem::take(&mut self.contexts) {
            // SAFETY: a context of the device's, destroyed once.
            unsafe { virgl_renderer_context_destroy(ctx) };
        }
        self.shared
            .signals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

impl Drop for Venus {
    fn drop(&mut self) {
        self.reset();
        let cookie = std::ptr::from_mut::<Shared>(&mut self.shared).cast::<c_void>();
        // SAFETY: the renderer this value started, cleaned up before its cookie goes.
        unsafe { virgl_renderer_cleanup(cookie) };
        // Its connection let go, the render server ends.
        #[cfg(target_os = "linux")]
        if let Some(server) = self.server.take()
            && server.join().is_err()
        {
            crate::warn!("virtio-gpu: the render server's thread ended abnormally");
        }
        STARTED.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        /// Metal's GPU, retained, or null where the Mac has none (a virtual one, as CI's).
        fn MTLCreateSystemDefaultDevice() -> *mut c_void;
        fn objc_release(obj: *mut c_void);
    }

    /// Whether this Mac has a GPU for Metal, and so for MoltenVK.
    #[cfg(target_os = "macos")]
    fn has_gpu() -> Result<(), String> {
        // SAFETY: Metal's own function, which takes nothing.
        let device = unsafe { MTLCreateSystemDefaultDevice() };
        if device.is_null() {
            return Err("this Mac has no GPU for Metal".into());
        }
        // SAFETY: the object the call above retained for us, released once.
        unsafe { objc_release(device) };
        Ok(())
    }

    /// Whether this host has a GPU for Vulkan: its loader, and a device whose memory the
    /// host can map.
    #[cfg(not(target_os = "macos"))]
    fn has_gpu() -> Result<(), String> {
        match host_visible(&[])?.0 {
            0 => Err("this host's Vulkan devices have no memory the host can map".into()),
            _ => Ok(()),
        }
    }

    /// The bytes at `offset` of `pages`, read through a mapping of their own: the held
    /// range on macOS, the held descriptor mapped afresh on Linux, the pages themselves on
    /// Windows.
    #[cfg_attr(windows, allow(unused_variables))]
    fn read(pages: Pages, len: usize, offset: usize) -> u8 {
        match pages {
            // SAFETY: the held range, the blob's pages.
            Pages::At(at) => unsafe { at.add(offset).read() },
            #[cfg(unix)]
            Pages::Fd(fd) => {
                // SAFETY: a mapping of the held descriptor of its own, unmapped below.
                let at = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                assert_ne!(at, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
                // SAFETY: as just mapped.
                let b = unsafe { at.cast::<u8>().add(offset).read() };
                // SAFETY: the mapping made above.
                unsafe { libc::munmap(at, len) };
                b
            }
        }
    }

    /// The renderer's own mapping of `resource`, which it writes: MoltenVK's on macOS, its
    /// pages on Windows, virglrenderer's of the blob's descriptor on Linux.
    fn renderers(resource: u32) -> *mut u8 {
        #[cfg(any(target_os = "macos", windows))]
        {
            let mut renderer = 0u64;
            // SAFETY: an output of the renderer's.
            let got = unsafe { virgl_renderer_resource_get_map_ptr(resource, &raw mut renderer) };
            assert_eq!(got, 0);
            renderer as *mut u8
        }
        #[cfg(target_os = "linux")]
        {
            unsafe extern "C" {
                fn virgl_renderer_resource_map(
                    res_handle: u32,
                    map: *mut *mut c_void,
                    size: *mut u64,
                ) -> c_int;
            }
            let (mut map, mut size) = (std::ptr::null_mut(), 0u64);
            // SAFETY: two outputs of the renderer's; its mapping lives until the blob's unref.
            let got = unsafe { virgl_renderer_resource_map(resource, &raw mut map, &raw mut size) };
            assert_eq!(got, 0);
            map.cast()
        }
    }

    /// On this host's GPU: the renderer starts once a process, offers Venus's capability
    /// set and fills it, says the host's heaps, makes a context of Venus; a blob of host
    /// memory is held from its making, what the renderer writes seen through what it holds,
    /// and refused where the guest says it is larger than its memory.
    #[test]
    fn the_renderer_offers_venus_and_holds_its_blobs() {
        if let Err(why) = has_gpu() {
            println!("SKIP: {why}");
            return;
        }
        let mut venus = Venus::new().unwrap();
        assert_eq!(Venus::new().unwrap_err(), "the GPU renderer runs once a process");
        let c = venus.capsets()[0];
        assert_eq!(c.id, CAPSET_VENUS);
        let caps = venus.capset(CAPSET_VENUS, c.max_version).unwrap();
        assert_eq!(caps.len(), c.max_size as usize);
        assert!(caps.iter().any(|&b| b != 0), "an empty capability set");
        assert!(venus.host_visible() > 0);
        venus.create_context(1, CAPSET_VENUS, b"test").unwrap();
        // A blob of shared memory (blob_id 0), mappable: the renderer's own.
        let blob = Blob {
            resource: 7,
            mem: 2,
            flags: BLOB_MAPPABLE,
            id: 0,
            size: 20000,
        };
        venus.create_blob(1, &blob, &[]).unwrap();
        let held = venus.map(7).unwrap();
        assert_eq!(held.len as u64, 20000u64.next_multiple_of(venus.page));
        let renderer = renderers(7);
        #[cfg(target_os = "macos")]
        if let Pages::At(at) = held.pages {
            assert_ne!(at, renderer, "the blob's own range, not the renderer's");
        }
        // SAFETY: the renderer's mapping of the blob's pages.
        unsafe { renderer.add(100).write(0x5a) };
        assert_eq!(read(held.pages, held.len, 100), 0x5a);
        // Said to be larger than its memory: its region ends first, and it is refused.
        assert_eq!(venus.hold(7, 1 << 30), Err(Refused(resp::ERR_INVALID_PARAMETER)));
        venus.unref(7);
        assert!(venus.map(7).is_err());
        venus.destroy_context(1);
    }
}
