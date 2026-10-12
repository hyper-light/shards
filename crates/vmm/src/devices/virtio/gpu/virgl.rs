//! The renderer: virglrenderer's Venus, in the krunkit fork that renders in this process on
//! macOS, over MoltenVK (D144). Linked static, in `shards-vm-gpu` alone (PM M214).
//!
//! A blob of host memory is held by this process from its making: its pages remapped
//! (`mach_vm_remap`, not copied) into a range of its own, which the window maps and which
//! lives until the blob does. The renderer frees a blob's Vulkan memory when the guest
//! frees it, whether or not the blob lives, and MoltenVK's next allocation takes the freed
//! address (PM M216): mapped from the renderer's address, a blob freed and then mapped
//! would have given the guest whatever took its place. A held range keeps the blob's own
//! pages, which no later allocation is given while any mapping of them lives (PM M216). A
//! blob is mapped whole, in host pages; its pages past its size are its allocation's alone
//! (PM M216), and a blob the guest says is larger than the memory it is of is refused, as
//! the memory's region ends before it.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::{Mutex, PoisonError};
use std::thread::Thread;

use super::protocol::{Refused, resp};
use super::{Blob, Capset, HostMemory, Renderer, Signal};

/// The capability set of Venus (virglrenderer_hw.h, VIRGL_RENDERER_CAPSET_VENUS).
const CAPSET_VENUS: u32 = 4;
/// virglrenderer.h: VIRGL_RENDERER_VENUS, VIRGL_RENDERER_NO_VIRGL.
const VENUS: c_int = 1 << 6;
const NO_VIRGL: c_int = 1 << 7;
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

/// `struct virgl_renderer_resource_create_blob_args`.
#[repr(C)]
struct CreateBlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const libc::iovec,
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
    fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
    fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);
    fn virgl_renderer_resource_create_blob(args: *const CreateBlobArgs) -> c_int;
    fn virgl_renderer_resource_unref(res_handle: u32);
    fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;
    fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
    fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
    fn virgl_renderer_resource_get_map_ptr(res_handle: u32, map_ptr: *mut u64) -> c_int;
}

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
const VM_REGION_BASIC_INFO_64: i32 = 9;
const VM_FLAGS_ANYWHERE: c_int = 1;
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

unsafe extern "C" {
    fn vkCreateInstance(
        info: *const InstanceCreateInfo,
        allocator: *const c_void,
        instance: *mut *mut c_void,
    ) -> i32;
    fn vkDestroyInstance(instance: *mut c_void, allocator: *const c_void);
    fn vkEnumeratePhysicalDevices(instance: *mut c_void, count: *mut u32, devices: *mut *mut c_void) -> i32;
    fn vkGetPhysicalDeviceMemoryProperties(device: *mut c_void, properties: *mut MemoryProperties);
}

/// The bytes of the host GPU's heaps that host-visible memory comes from, as Vulkan says
/// them, which Venus tells the guest: the most memory a guest can have mapped into its
/// window at once.
fn host_visible() -> Result<u64, String> {
    let info = InstanceCreateInfo {
        s_type: STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        next: std::ptr::null(),
        flags: 0,
        application: std::ptr::null(),
        layers: 0,
        layer_names: std::ptr::null(),
        extensions: 0,
        extension_names: std::ptr::null(),
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: an instance of no layers and no extensions, destroyed below.
    let r = unsafe { vkCreateInstance(&raw const info, std::ptr::null(), &raw mut instance) };
    if r != 0 || instance.is_null() {
        return Err(format!("Vulkan made no instance ({r})"));
    }
    let mut count = 1u32;
    let mut device = std::ptr::null_mut();
    // SAFETY: room for one device; VK_INCOMPLETE where there are more, which is fine.
    let r = unsafe { vkEnumeratePhysicalDevices(instance, &raw mut count, &raw mut device) };
    let bytes = if (r == 0 || r == 5) && count > 0 && !device.is_null() {
        let mut props = MemoryProperties {
            type_count: 0,
            types: [MemoryType::default(); 32],
            heap_count: 0,
            heaps: [MemoryHeap::default(); 16],
        };
        // SAFETY: the instance's device, read into a whole properties structure.
        unsafe { vkGetPhysicalDeviceMemoryProperties(device, &raw mut props) };
        let heaps = props.heaps.get(..props.heap_count as usize).unwrap_or_default();
        let visible = props
            .types
            .get(..props.type_count as usize)
            .unwrap_or_default()
            .iter()
            .filter(|t| t.property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0);
        let mut counted = Vec::new();
        let mut total = 0u64;
        for t in visible {
            if !counted.contains(&t.heap_index) {
                counted.push(t.heap_index);
                total = total.saturating_add(heaps.get(t.heap_index as usize).map_or(0, |h| h.size));
            }
        }
        Ok(total)
    } else {
        Err(format!("Vulkan has no GPU ({r})"))
    };
    // SAFETY: the instance made above.
    unsafe { vkDestroyInstance(instance, std::ptr::null()) };
    bytes
}

/// What the renderer's threads tell the device: the fences signalled, and the worker to
/// wake for them. Its address is the renderer's cookie.
#[derive(Default)]
struct Shared {
    signals: Mutex<Vec<Signal>>,
    worker: Mutex<Option<Thread>>,
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

/// A blob's host memory, held: `len` bytes at `at` that map its pages, its own until it
/// goes.
struct Held {
    at: u64,
    len: u64,
    cache: u32,
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: a range this process remapped for the blob and nothing else holds.
        unsafe { mach_vm_deallocate(mach_task_self_, self.at, self.len) };
    }
}

/// The renderer, once a process.
pub struct Venus {
    shared: Box<Shared>,
    capsets: [Capset; 1],
    /// Each blob's guest memory as the renderer holds it, which must live as long as the
    /// blob does: virglrenderer keeps the array's address (virgl_resource.c).
    iovecs: HashMap<u32, Box<[libc::iovec]>>,
    held: HashMap<u32, Held>,
    contexts: Vec<u32>,
    page: u64,
    host_visible: u64,
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
        if STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err("the GPU renderer runs once a process".into());
        }
        let page = crate::platform::page_size().map_err(|e| format!("the host's page size: {e}"))? as u64;
        let host_visible = host_visible()
            .and_then(|b| match b {
                0 => Err("the host's GPU has no memory the host can map, which Venus needs".into()),
                b => Ok(b),
            })
            .inspect_err(|_| STARTED.store(false, std::sync::atomic::Ordering::Release))?;
        let mut shared = Box::new(Shared::default());
        let mut callbacks = Callbacks {
            version: CALLBACKS_VERSION,
            write_fence: Some(write_fence),
            create_gl_context: None,
            destroy_gl_context: None,
            make_current: None,
            get_drm_fd: None,
            write_context_fence: Some(write_context_fence),
            get_server_fd: None,
            get_egl_display: None,
        };
        let cookie = std::ptr::from_mut::<Shared>(&mut shared).cast::<c_void>();
        // SAFETY: the callbacks are copied by the renderer; the cookie lives in `shared`,
        // which this value holds until it cleans the renderer up.
        let r = unsafe { virgl_renderer_init(cookie, VENUS | NO_VIRGL, &raw mut callbacks) };
        if r != 0 {
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
}

/// A negative errno the renderer returned, said.
fn errno(r: c_int) -> std::io::Error {
    std::io::Error::from_raw_os_error(r.saturating_abs())
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
        let nlen = u32::try_from(name.len()).map_err(|_| Refused(resp::ERR_INVALID_PARAMETER))?;
        // SAFETY: the name's bytes, `nlen` of them, which the renderer copies.
        let r = unsafe { virgl_renderer_context_create_with_flags(ctx, capset, nlen, name.as_ptr().cast()) };
        refused(r)?;
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
        let iovecs: Box<[libc::iovec]> = guest
            .iter()
            .map(|&(base, len)| libc::iovec {
                iov_base: base.cast(),
                iov_len: len,
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
        Ok(HostMemory {
            host: held.at as *mut u8,
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
        STARTED.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    unsafe extern "C" {
        /// Metal's GPU, retained, or null where the Mac has none (a virtual one, as CI's).
        fn MTLCreateSystemDefaultDevice() -> *mut c_void;
        fn objc_release(obj: *mut c_void);
    }

    /// Whether this Mac has a GPU for Metal, and so for MoltenVK.
    fn has_gpu() -> bool {
        // SAFETY: Metal's own function, which takes nothing.
        let device = unsafe { MTLCreateSystemDefaultDevice() };
        if device.is_null() {
            return false;
        }
        // SAFETY: the object the call above retained for us, released once.
        unsafe { objc_release(device) };
        true
    }

    /// On this Mac's GPU: the renderer starts once a process, offers Venus's capability set
    /// and fills it, says the host's heaps, makes a context of Venus; a blob of host memory
    /// is held from its making, written through the held range as the renderer sees it,
    /// and refused where the guest says it is larger than its memory.
    #[test]
    fn the_renderer_offers_venus_and_holds_its_blobs() {
        if !has_gpu() {
            println!("SKIP: this Mac has no GPU for Metal");
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
        let mut renderer = 0u64;
        // SAFETY: an output of the renderer's.
        let got = unsafe { virgl_renderer_resource_get_map_ptr(7, &raw mut renderer) };
        assert_eq!(got, 0);
        assert_ne!(
            held.host as u64, renderer,
            "the blob's own range, not the renderer's"
        );
        // SAFETY: the held range and the renderer's, each the blob's pages.
        unsafe {
            held.host.add(100).write(0x5a);
            assert_eq!((renderer as *const u8).add(100).read(), 0x5a);
        }
        // Said to be larger than its memory: its region ends first, and it is refused.
        assert_eq!(venus.hold(7, 1 << 30), Err(Refused(resp::ERR_INVALID_PARAMETER)));
        venus.unref(7);
        assert!(venus.map(7).is_err());
        venus.destroy_context(1);
    }
}
