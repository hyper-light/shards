//! A GPU's host-visible memory in a WHP partition (PM M383, D201), before Venus is written
//! for Windows: Venus maps each host-visible blob into its guest's window, and on macOS the
//! renderer hands the VMM the address its driver mapped the blob at (D144). On a Windows
//! host, for each memory type of the GPU's that the host can map, the probe allocates 2 MiB
//! of it, maps it (vkMapMemory), and asks:
//! - does WHvMapGpaRange take the driver's mapping, and what does it cost;
//! - does the guest see the host's store through it, and the host the guest's;
//! - does the guest see what the GPU writes into it (vkCmdFillBuffer);
//! and then, for memory of the probe's own (VirtualAlloc) imported as Vulkan memory
//! (VK_EXT_external_memory_host), the same: WHP maps the probe's own pages, and the GPU's
//! writes must reach them.
//!
//! The guest is m341's: one vCPU in long mode running an interpreter, a load or a store at
//! each port exit's word, the window at 1 GiB.

// Windows' and Vulkan's constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use whp_run::{Aligned, Api, bind, check, reg64, segment, summary};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// Each memory's size: a 2 MiB blob.
const SIZE: usize = 2 << 20;
/// Where each memory is mapped in the guest: from 1 GiB, one 2 MiB slot each.
const WINDOW: u64 = 1 << 30;
const PORT: u16 = 0xE0;
const LOAD: u64 = 0;
const STORE: u64 = 1;
const POISON: u64 = 0xDEAD_DEAD_DEAD_DEAD;
const WATCHDOG: Duration = Duration::from_secs(300);
const MOST_FAULTS: u32 = 4;
/// Where each question writes: the host's store, the guest's, and the GPU's fill.
const HOST: usize = 0x100;
const GUEST: usize = 0x200;
const FILLED: usize = 0x400;
const FILL: u32 = 0xA5C3_5A3C;
/// WHvMapGpaRange's cost, timed over this many maps and unmaps of each memory.
const ROUNDS: usize = 100;

/// m341's interpreter: `out`, then RBX's LOAD `mov rax, [rcx]` or STORE `mov [rcx], rsi`.
fn guest_code() -> Vec<u8> {
    let mut c = vec![0xEE]; // out dx, al
    c.extend([0x83, 0xFB, STORE as u8]); // cmp ebx, STORE
    c.extend([0x74, 0]); // je store
    let to_store = c.len() - 1;
    c.extend([0x48, 0x8B, 0x01]); // mov rax, [rcx]
    c.extend([0xEB, 0]); // jmp 0
    let back_load = c.len() - 1;
    let store = c.len();
    c.extend([0x48, 0x89, 0x31]); // mov [rcx], rsi
    c.extend([0xEB, 0]); // jmp 0
    let back_store = c.len() - 1;
    for (at, target) in [(to_store, store), (back_load, 0), (back_store, 0)] {
        c[at] = (target as isize - (at as isize + 1)) as i8 as u8;
    }
    c
}

struct Guest<'a> {
    api: &'a Api,
    partition: WHV_PARTITION_HANDLE,
    rip: u64,
}

impl Guest<'_> {
    fn run(&self) -> Result<WHV_RUN_VP_EXIT_CONTEXT, String> {
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        // SAFETY: an exit context of its own size, on the partition's one vCPU.
        let hr = unsafe {
            (self.api.run)(
                self.partition,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        check("WHvRunVirtualProcessor", hr)?;
        Ok(exit.0)
    }

    fn set(&self, names: &[WHV_REGISTER_NAME], values: &[WHV_REGISTER_VALUE]) -> Result<(), String> {
        // SAFETY: as many values as names, 16-aligned as the caller's `Aligned` holds them.
        check("registers", unsafe {
            (self.api.set_regs)(self.partition, 0, names.as_ptr(), names.len() as u32, values.as_ptr())
        })
    }

    fn start(&mut self) -> Result<(), String> {
        let exit = self.run()?;
        if exit.ExitReason != WHvRunVpExitReasonX64IoPortAccess {
            return Err(format!("first exit {:#x} at rip {:#x}", exit.ExitReason, exit.VpContext.Rip));
        }
        self.rip = exit.VpContext.Rip + 1;
        Ok(())
    }

    /// The operation's answer, and how many memory exits it took.
    fn ask(&mut self, op: u64, gpa: u64, value: u64) -> Result<(u64, u32), String> {
        let values = Aligned([reg64(self.rip), reg64(op), reg64(gpa), reg64(value)]);
        self.set(
            &[WHvX64RegisterRip, WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRsi],
            &values.0,
        )?;
        let mut faults = 0u32;
        loop {
            let exit = self.run()?;
            match exit.ExitReason {
                // SAFETY (each union read): the member the exit reason names.
                r if r == WHvRunVpExitReasonX64IoPortAccess && unsafe { exit.Anonymous.IoPortAccess.PortNumber } == PORT => {
                    self.rip = exit.VpContext.Rip + 1;
                    return Ok((unsafe { exit.Anonymous.IoPortAccess.Rax }, faults));
                }
                r if r == WHvRunVpExitReasonMemoryAccess => {
                    faults += 1;
                    if faults > MOST_FAULTS {
                        return Err(format!("past {MOST_FAULTS} memory exits"));
                    }
                    let next = Aligned([reg64(exit.VpContext.Rip + 3), reg64(POISON)]);
                    let names = [WHvX64RegisterRip, WHvX64RegisterRax];
                    let count = if op == LOAD { 2 } else { 1 };
                    self.set(&names[..count], &next.0[..count])?;
                }
                other => return Err(format!("exit reason {other:#x} at rip {:#x}", exit.VpContext.Rip)),
            }
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn partition(api: &Api) -> Result<WHV_PARTITION_HANDLE, String> {
    let mut partition: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes, and memory it allocates.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut partition))?;
        let count: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(partition, WHvPartitionPropertyCodeProcessorCount, (&raw const count).cast(), 4),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(partition))?;
        let ram = VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        if ram.is_null() {
            return Err(last_error("VirtualAlloc"));
        }
        let mem = std::slice::from_raw_parts_mut(ram, RAM);
        let code = guest_code();
        mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
        // PML4[0] -> PDPT; PDPT[0] and [1]: 1 GiB pages at 0 (the RAM) and 1 GiB (the window).
        mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
        mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
        mem[PDPT as usize + 8..PDPT as usize + 16].copy_from_slice(&(WINDOW | 0x83).to_le_bytes());
        check(
            "WHvMapGpaRange RAM",
            (api.map)(
                partition,
                ram.cast(),
                0,
                RAM as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        check("WHvCreateVirtualProcessor", (api.create_vp)(partition, 0, 0))?;
        let names = [
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterSs,
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
            WHvX64RegisterRsp,
            WHvX64RegisterRdx,
        ];
        let values = Aligned([
            reg64(0x8000_0031),
            reg64(PML4),
            reg64(0x20),
            reg64(0x500),
            segment(0x8, 0xA09B),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            reg64(CODE),
            reg64(0x2),
            reg64(0x8000),
            reg64(u64::from(PORT)),
        ]);
        check(
            "registers",
            (api.set_regs)(partition, 0, names.as_ptr(), names.len() as u32, values.0.as_ptr()),
        )?;
        Ok(partition)
    }
}

// Vulkan, as vulkan_core.h declares what the probe calls: a handle of a dispatchable object
// is a pointer, of a non-dispatchable one a 64-bit value.
type Handle = *mut c_void;
type Object = u64;
type VkResult = i32;

#[repr(C)]
struct ApplicationInfo {
    s_type: i32,
    next: *const c_void,
    name: *const c_char,
    version: u32,
    engine: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct InstanceCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    application: *const ApplicationInfo,
    layers: u32,
    layer_names: *const *const c_char,
    extensions: u32,
    extension_names: *const *const c_char,
}

#[repr(C)]
struct QueueCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    family: u32,
    count: u32,
    priorities: *const f32,
}

#[repr(C)]
struct DeviceCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    queues: u32,
    queue_infos: *const QueueCreateInfo,
    layers: u32,
    layer_names: *const *const c_char,
    extensions: u32,
    extension_names: *const *const c_char,
    features: *const c_void,
}

#[repr(C)]
struct AllocateInfo {
    s_type: i32,
    next: *const c_void,
    size: u64,
    type_index: u32,
}

#[repr(C)]
struct ImportHostPointer {
    s_type: i32,
    next: *const c_void,
    handle_type: u32,
    pointer: *mut c_void,
}

#[repr(C)]
struct HostPointerProperties {
    s_type: i32,
    next: *mut c_void,
    type_bits: u32,
}

#[repr(C)]
struct BufferCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    size: u64,
    usage: u32,
    sharing: u32,
    families: u32,
    family_indices: *const u32,
}

#[repr(C)]
struct ExternalBufferCreateInfo {
    s_type: i32,
    next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct PoolCreateInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    family: u32,
}

#[repr(C)]
struct CommandBufferInfo {
    s_type: i32,
    next: *const c_void,
    pool: Object,
    level: u32,
    count: u32,
}

#[repr(C)]
struct BeginInfo {
    s_type: i32,
    next: *const c_void,
    flags: u32,
    inheritance: *const c_void,
}

#[repr(C)]
struct SubmitInfo {
    s_type: i32,
    next: *const c_void,
    waits: u32,
    wait_semaphores: *const Object,
    wait_stages: *const u32,
    buffers: u32,
    command_buffers: *const Handle,
    signals: u32,
    signal_semaphores: *const Object,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryType {
    flags: u32,
    heap: u32,
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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct QueueFamily {
    flags: u32,
    count: u32,
    timestamp_bits: u32,
    granularity: [u32; 3],
}

const APPLICATION_INFO: i32 = 0;
const INSTANCE_CREATE_INFO: i32 = 1;
const DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const DEVICE_CREATE_INFO: i32 = 3;
const SUBMIT_INFO: i32 = 4;
const MEMORY_ALLOCATE_INFO: i32 = 5;
const BUFFER_CREATE_INFO: i32 = 12;
const COMMAND_POOL_CREATE_INFO: i32 = 39;
const COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const EXTERNAL_MEMORY_BUFFER_CREATE_INFO: i32 = 1000072000;
const IMPORT_MEMORY_HOST_POINTER_INFO_EXT: i32 = 1000178000;
const MEMORY_HOST_POINTER_PROPERTIES_EXT: i32 = 1000178001;
const HOST_VISIBLE: u32 = 0x2;
const HOST_COHERENT: u32 = 0x4;
const HANDLE_TYPE_HOST_ALLOCATION: u32 = 0x80;
const BUFFER_USAGE_TRANSFER_DST: u32 = 0x2;
const QUEUE_TRANSFER_CAPABLE: u32 = 0x1 | 0x2 | 0x4;
const API_1_1: u32 = 1 << 22 | 1 << 12;

/// The loader's functions the probe calls, and the device's.
struct Vk {
    get_instance_proc: unsafe extern "system" fn(Handle, *const c_char) -> *const c_void,
    instance: Handle,
    device: Handle,
    physical: Handle,
    queue: Handle,
    family: u32,
    properties: MemoryProperties,
    host_import: bool,
}

/// Function `name` of the instance or device, as `T`.
unsafe fn proc<T: Copy>(vk: &Vk, device: bool, name: &str) -> Result<T, String> {
    let c: Vec<u8> = name.bytes().chain([0]).collect();
    // SAFETY: the instance's own lookup, for an entry point of type `T`.
    let f = unsafe {
        if device {
            let get: unsafe extern "system" fn(Handle, *const c_char) -> *const c_void =
                std::mem::transmute((vk.get_instance_proc)(vk.instance, c"vkGetDeviceProcAddr".as_ptr()));
            get(vk.device, c.as_ptr().cast())
        } else {
            (vk.get_instance_proc)(vk.instance, c.as_ptr().cast())
        }
    };
    if f.is_null() {
        return Err(format!("no {name}"));
    }
    // SAFETY: the caller names `T` as the entry point's own signature.
    Ok(unsafe { std::mem::transmute_copy(&f) })
}

fn vk_check(what: &str, r: VkResult) -> Result<(), String> {
    if r != 0 {
        return Err(format!("{what}: VkResult {r}"));
    }
    Ok(())
}

/// The first GPU's instance, device and a queue that copies.
fn vulkan() -> Result<Vk, String> {
    let wide: Vec<u16> = "vulkan-1.dll".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated wide string; the loader is a system DLL.
    let lib = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if lib.is_null() {
        return Err(last_error("vulkan-1.dll"));
    }
    // SAFETY: the loader's exported lookup, of its own signature.
    let get_instance_proc: unsafe extern "system" fn(Handle, *const c_char) -> *const c_void = unsafe {
        match GetProcAddress(lib, c"vkGetInstanceProcAddr".as_ptr().cast()) {
            Some(f) => std::mem::transmute(f),
            None => return Err(last_error("vkGetInstanceProcAddr")),
        }
    };
    let mut vk = Vk {
        get_instance_proc,
        instance: std::ptr::null_mut(),
        device: std::ptr::null_mut(),
        physical: std::ptr::null_mut(),
        queue: std::ptr::null_mut(),
        family: 0,
        properties: MemoryProperties {
            type_count: 0,
            types: [MemoryType::default(); 32],
            heap_count: 0,
            heaps: [MemoryHeap::default(); 16],
        },
        host_import: false,
    };
    // SAFETY: Vulkan's calls with their documented arguments, each output local.
    unsafe {
        let create: unsafe extern "system" fn(*const InstanceCreateInfo, *const c_void, *mut Handle) -> VkResult =
            proc(&vk, false, "vkCreateInstance")?;
        let app = ApplicationInfo {
            s_type: APPLICATION_INFO,
            next: std::ptr::null(),
            name: c"m383".as_ptr(),
            version: 1,
            engine: std::ptr::null(),
            engine_version: 0,
            api_version: API_1_1,
        };
        let info = InstanceCreateInfo {
            s_type: INSTANCE_CREATE_INFO,
            next: std::ptr::null(),
            flags: 0,
            application: &raw const app,
            layers: 0,
            layer_names: std::ptr::null(),
            extensions: 0,
            extension_names: std::ptr::null(),
        };
        vk_check("vkCreateInstance", create(&raw const info, std::ptr::null(), &raw mut vk.instance))?;
        let enumerate: unsafe extern "system" fn(Handle, *mut u32, *mut Handle) -> VkResult =
            proc(&vk, false, "vkEnumeratePhysicalDevices")?;
        let mut count = 1u32;
        let r = enumerate(vk.instance, &raw mut count, &raw mut vk.physical);
        if (r != 0 && r != 5) || vk.physical.is_null() {
            return Err(format!("vkEnumeratePhysicalDevices: {r}"));
        }
        let props: unsafe extern "system" fn(Handle, *mut u64) = proc(&vk, false, "vkGetPhysicalDeviceProperties")?;
        let mut buf = [0u64; 128];
        props(vk.physical, buf.as_mut_ptr());
        let bytes = std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), 1024);
        let name = &bytes[20..276];
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        println!("device: {}", String::from_utf8_lossy(&name[..end]));
        let memory: unsafe extern "system" fn(Handle, *mut MemoryProperties) =
            proc(&vk, false, "vkGetPhysicalDeviceMemoryProperties")?;
        memory(vk.physical, &raw mut vk.properties);
        let families: unsafe extern "system" fn(Handle, *mut u32, *mut QueueFamily) =
            proc(&vk, false, "vkGetPhysicalDeviceQueueFamilyProperties")?;
        let mut n = 16u32;
        let mut fams = [QueueFamily::default(); 16];
        families(vk.physical, &raw mut n, fams.as_mut_ptr());
        vk.family = fams[..n as usize]
            .iter()
            .position(|f| f.flags & QUEUE_TRANSFER_CAPABLE != 0)
            .ok_or("no queue family that copies")? as u32;
        let extensions: unsafe extern "system" fn(Handle, *const c_char, *mut u32, *mut [u8; 260]) -> VkResult =
            proc(&vk, false, "vkEnumerateDeviceExtensionProperties")?;
        let mut n = 512u32;
        let mut list = vec![[0u8; 260]; 512];
        extensions(vk.physical, std::ptr::null(), &raw mut n, list.as_mut_ptr());
        vk.host_import = list[..n as usize].iter().any(|e| e.starts_with(b"VK_EXT_external_memory_host\0"));
        let priority = 1.0f32;
        let queue = QueueCreateInfo {
            s_type: DEVICE_QUEUE_CREATE_INFO,
            next: std::ptr::null(),
            flags: 0,
            family: vk.family,
            count: 1,
            priorities: &raw const priority,
        };
        let names = [c"VK_EXT_external_memory_host".as_ptr()];
        let info = DeviceCreateInfo {
            s_type: DEVICE_CREATE_INFO,
            next: std::ptr::null(),
            flags: 0,
            queues: 1,
            queue_infos: &raw const queue,
            layers: 0,
            layer_names: std::ptr::null(),
            extensions: u32::from(vk.host_import),
            extension_names: names.as_ptr(),
            features: std::ptr::null(),
        };
        let create: unsafe extern "system" fn(Handle, *const DeviceCreateInfo, *const c_void, *mut Handle) -> VkResult =
            proc(&vk, false, "vkCreateDevice")?;
        vk_check("vkCreateDevice", create(vk.physical, &raw const info, std::ptr::null(), &raw mut vk.device))?;
        let get_queue: unsafe extern "system" fn(Handle, u32, u32, *mut Handle) = proc(&vk, true, "vkGetDeviceQueue")?;
        get_queue(vk.device, vk.family, 0, &raw mut vk.queue);
    }
    Ok(vk)
}

/// Allocates `SIZE` bytes of memory type `index`, or imports the memory at `host`.
fn allocate(vk: &Vk, index: u32, host: Option<*mut c_void>) -> Result<Object, String> {
    let import = host.map(|pointer| ImportHostPointer {
        s_type: IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
        next: std::ptr::null(),
        handle_type: HANDLE_TYPE_HOST_ALLOCATION,
        pointer,
    });
    let info = AllocateInfo {
        s_type: MEMORY_ALLOCATE_INFO,
        next: import.as_ref().map_or(std::ptr::null(), |i| (i as *const ImportHostPointer).cast()),
        size: SIZE as u64,
        type_index: index,
    };
    let mut memory: Object = 0;
    // SAFETY: Vulkan's allocation, its output local.
    unsafe {
        let f: unsafe extern "system" fn(Handle, *const AllocateInfo, *const c_void, *mut Object) -> VkResult =
            proc(vk, true, "vkAllocateMemory")?;
        vk_check("vkAllocateMemory", f(vk.device, &raw const info, std::ptr::null(), &raw mut memory))?;
    }
    Ok(memory)
}

/// The GPU fills `FILLED..FILLED+256` of `memory` with `FILL`, through a buffer bound to it,
/// and the queue waits for it.
fn gpu_fill(vk: &Vk, memory: Object, external: bool) -> Result<(), String> {
    // SAFETY: Vulkan's calls with their documented arguments, each object destroyed below.
    unsafe {
        let ext = ExternalBufferCreateInfo {
            s_type: EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
            next: std::ptr::null(),
            handle_types: HANDLE_TYPE_HOST_ALLOCATION,
        };
        let info = BufferCreateInfo {
            s_type: BUFFER_CREATE_INFO,
            next: if external { (&raw const ext).cast() } else { std::ptr::null() },
            flags: 0,
            size: SIZE as u64,
            usage: BUFFER_USAGE_TRANSFER_DST,
            sharing: 0,
            families: 0,
            family_indices: std::ptr::null(),
        };
        let mut buffer: Object = 0;
        let create: unsafe extern "system" fn(Handle, *const BufferCreateInfo, *const c_void, *mut Object) -> VkResult =
            proc(vk, true, "vkCreateBuffer")?;
        vk_check("vkCreateBuffer", create(vk.device, &raw const info, std::ptr::null(), &raw mut buffer))?;
        let bind: unsafe extern "system" fn(Handle, Object, Object, u64) -> VkResult = proc(vk, true, "vkBindBufferMemory")?;
        vk_check("vkBindBufferMemory", bind(vk.device, buffer, memory, 0))?;
        let pool_info = PoolCreateInfo {
            s_type: COMMAND_POOL_CREATE_INFO,
            next: std::ptr::null(),
            flags: 0,
            family: vk.family,
        };
        let mut pool: Object = 0;
        let create_pool: unsafe extern "system" fn(Handle, *const PoolCreateInfo, *const c_void, *mut Object) -> VkResult =
            proc(vk, true, "vkCreateCommandPool")?;
        vk_check("vkCreateCommandPool", create_pool(vk.device, &raw const pool_info, std::ptr::null(), &raw mut pool))?;
        let alloc_info = CommandBufferInfo {
            s_type: COMMAND_BUFFER_ALLOCATE_INFO,
            next: std::ptr::null(),
            pool,
            level: 0,
            count: 1,
        };
        let mut cmd: Handle = std::ptr::null_mut();
        let alloc: unsafe extern "system" fn(Handle, *const CommandBufferInfo, *mut Handle) -> VkResult =
            proc(vk, true, "vkAllocateCommandBuffers")?;
        vk_check("vkAllocateCommandBuffers", alloc(vk.device, &raw const alloc_info, &raw mut cmd))?;
        let begin_info = BeginInfo {
            s_type: COMMAND_BUFFER_BEGIN_INFO,
            next: std::ptr::null(),
            flags: 1,
            inheritance: std::ptr::null(),
        };
        let begin: unsafe extern "system" fn(Handle, *const BeginInfo) -> VkResult = proc(vk, true, "vkBeginCommandBuffer")?;
        vk_check("vkBeginCommandBuffer", begin(cmd, &raw const begin_info))?;
        let fill: unsafe extern "system" fn(Handle, Object, u64, u64, u32) = proc(vk, true, "vkCmdFillBuffer")?;
        fill(cmd, buffer, FILLED as u64, 256, FILL);
        let end: unsafe extern "system" fn(Handle) -> VkResult = proc(vk, true, "vkEndCommandBuffer")?;
        vk_check("vkEndCommandBuffer", end(cmd))?;
        let submit_info = SubmitInfo {
            s_type: SUBMIT_INFO,
            next: std::ptr::null(),
            waits: 0,
            wait_semaphores: std::ptr::null(),
            wait_stages: std::ptr::null(),
            buffers: 1,
            command_buffers: &raw const cmd,
            signals: 0,
            signal_semaphores: std::ptr::null(),
        };
        let submit: unsafe extern "system" fn(Handle, u32, *const SubmitInfo, Object) -> VkResult = proc(vk, true, "vkQueueSubmit")?;
        vk_check("vkQueueSubmit", submit(vk.queue, 1, &raw const submit_info, 0))?;
        let wait: unsafe extern "system" fn(Handle) -> VkResult = proc(vk, true, "vkQueueWaitIdle")?;
        vk_check("vkQueueWaitIdle", wait(vk.queue))?;
        let destroy_pool: unsafe extern "system" fn(Handle, Object, *const c_void) = proc(vk, true, "vkDestroyCommandPool")?;
        destroy_pool(vk.device, pool, std::ptr::null());
        let destroy: unsafe extern "system" fn(Handle, Object, *const c_void) = proc(vk, true, "vkDestroyBuffer")?;
        destroy(vk.device, buffer, std::ptr::null());
    }
    Ok(())
}

/// One memory, at `host` in this process and `gpa` in the guest: WHvMapGpaRange's answer and
/// cost, then each side's stores seen by the other and the GPU's fill seen by the guest.
fn questions(api: &Api, guest: &mut Guest<'_>, vk: &Vk, label: &str, host: *mut u8, gpa: u64, memory: Object, external: bool) -> Result<(), String> {
    let flags = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite;
    let started = Instant::now();
    // SAFETY: the memory's mapping, which stays mapped until unmapped below.
    let hr = unsafe { (api.map)(guest.partition, host.cast(), gpa, SIZE as u64, flags) };
    let took = started.elapsed();
    if hr < 0 {
        println!("{label}: WHvMapGpaRange of its mapping refused, HRESULT {:#010x} after {:.1} us", hr as u32, took.as_secs_f64() * 1e6);
        return Ok(());
    }
    println!("{label}: WHvMapGpaRange took it in {:.1} us", took.as_secs_f64() * 1e6);
    // SAFETY (each): an 8-aligned offset within the memory's SIZE bytes, mapped in this
    // process.
    let write = |offset: usize, value: u64| unsafe { std::ptr::write_volatile(host.add(offset).cast::<u64>(), value) };
    let read = |offset: usize| unsafe { std::ptr::read_volatile(host.add(offset).cast::<u64>()) };
    write(HOST, 0x1111_2222_3333_4444);
    let (seen, faults) = guest.ask(LOAD, gpa + HOST as u64, 0)?;
    println!("  the host's store, as the guest loads it: {seen:#x} ({faults} memory exits)");
    guest.ask(STORE, gpa + GUEST as u64, 0x5555_6666_7777_8888)?;
    println!("  the guest's store, as the host reads it: {:#x}", read(GUEST));
    match gpu_fill(vk, memory, external) {
        Ok(()) => {
            let (seen, faults) = guest.ask(LOAD, gpa + FILLED as u64, 0)?;
            println!(
                "  the GPU's fill, as the guest loads it: {seen:#x} (want {:#x}; {faults} memory exits), as the host reads it: {:#x}",
                u64::from(FILL) | u64::from(FILL) << 32,
                read(FILLED)
            );
        }
        Err(e) => println!("  the GPU's fill: {e}"),
    }
    // SAFETY: the range mapped above.
    check("WHvUnmapGpaRange", unsafe { (api.unmap)(guest.partition, gpa, SIZE as u64) })?;
    let mut maps = Vec::with_capacity(ROUNDS);
    let mut unmaps = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        // SAFETY: as above.
        check("WHvMapGpaRange", unsafe { (api.map)(guest.partition, host.cast(), gpa, SIZE as u64, flags) })?;
        maps.push(t.elapsed());
        let t = Instant::now();
        // SAFETY: as above.
        check("WHvUnmapGpaRange", unsafe { (api.unmap)(guest.partition, gpa, SIZE as u64) })?;
        unmaps.push(t.elapsed());
    }
    println!("  map {}; unmap {} (us)", summary(&mut maps), summary(&mut unmaps));
    Ok(())
}

fn probe(api: &Api, guest: &mut Guest<'_>) -> Result<(), String> {
    guest.start()?;
    let vk = vulkan()?;
    let props = &vk.properties;
    let mut slot = 0u64;
    // SAFETY: Vulkan's calls with their documented arguments.
    let (map, unmap, free): (
        unsafe extern "system" fn(Handle, Object, u64, u64, u32, *mut *mut c_void) -> VkResult,
        unsafe extern "system" fn(Handle, Object),
        unsafe extern "system" fn(Handle, Object, *const c_void),
    ) = unsafe { (proc(&vk, true, "vkMapMemory")?, proc(&vk, true, "vkUnmapMemory")?, proc(&vk, true, "vkFreeMemory")?) };
    for (index, t) in props.types[..props.type_count as usize].iter().enumerate() {
        if t.flags & HOST_VISIBLE == 0 {
            continue;
        }
        let label = format!("type {index} (flags {:#x}, heap {})", t.flags, t.heap);
        let memory = match allocate(&vk, index as u32, None) {
            Ok(m) => m,
            Err(e) => {
                println!("{label}: {e}");
                continue;
            }
        };
        let mut host = std::ptr::null_mut();
        // SAFETY: the memory just allocated, mapped whole.
        let r = unsafe { map(vk.device, memory, 0, SIZE as u64, 0, &raw mut host) };
        if r != 0 || host.is_null() {
            println!("{label}: vkMapMemory {r}");
        } else {
            println!("{label}: mapped at {host:p}");
            if let Err(e) = questions(api, guest, &vk, &label, host.cast(), WINDOW + slot * SIZE as u64, memory, false) {
                println!("  stopped: {e}");
            }
            // SAFETY: the mapping made above.
            unsafe { unmap(vk.device, memory) };
        }
        // SAFETY: the memory allocated above, its uses done.
        unsafe { free(vk.device, memory, std::ptr::null()) };
        slot += 1;
    }
    // The probe's own memory, imported.
    if !vk.host_import {
        println!("imported: the device has no VK_EXT_external_memory_host");
        return Ok(());
    }
    // SAFETY: memory of the probe's own, freed below.
    let own = unsafe { VirtualAlloc(std::ptr::null(), SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    if own.is_null() {
        return Err(last_error("VirtualAlloc"));
    }
    let mut bits = HostPointerProperties {
        s_type: MEMORY_HOST_POINTER_PROPERTIES_EXT,
        next: std::ptr::null_mut(),
        type_bits: 0,
    };
    // SAFETY: the extension's call, its output local.
    let r = unsafe {
        let f: unsafe extern "system" fn(Handle, u32, *const c_void, *mut HostPointerProperties) -> VkResult =
            proc(&vk, true, "vkGetMemoryHostPointerPropertiesEXT")?;
        f(vk.device, HANDLE_TYPE_HOST_ALLOCATION, own, &raw mut bits)
    };
    println!("imported: memory types {:#x} take it ({r})", bits.type_bits);
    let index = (0..props.type_count)
        .find(|&i| bits.type_bits & (1 << i) != 0 && props.types[i as usize].flags & (HOST_VISIBLE | HOST_COHERENT) == HOST_VISIBLE | HOST_COHERENT)
        .or_else(|| (0..props.type_count).find(|&i| bits.type_bits & (1 << i) != 0));
    match index {
        Some(index) => match allocate(&vk, index, Some(own)) {
            Ok(memory) => {
                let label = format!("imported into type {index} (flags {:#x})", props.types[index as usize].flags);
                if let Err(e) = questions(api, guest, &vk, &label, own.cast(), WINDOW + slot * SIZE as u64, memory, true) {
                    println!("  stopped: {e}");
                }
                // SAFETY: the memory imported above, its uses done.
                unsafe { free(vk.device, memory, std::ptr::null()) };
            }
            Err(e) => println!("imported: {e}"),
        },
        None => println!("imported: no memory type takes it"),
    }
    // SAFETY: the allocation made above, no longer imported.
    unsafe { VirtualFree(own, 0, MEM_RELEASE) };
    Ok(())
}

fn main() {
    let api = match bind() {
        Ok(api) => api,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    let partition = match partition(&api) {
        Ok(p) => p,
        Err(e) => {
            println!("partition: {e}");
            return;
        }
    };
    let mut guest = Guest {
        api: &api,
        partition,
        rip: CODE,
    };
    let done = AtomicBool::new(false);
    let cancel = api.cancel;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let until = Instant::now() + WATCHDOG;
            while !done.load(Ordering::Relaxed) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !done.load(Ordering::Relaxed) {
                // SAFETY: the partition, alive until this scope ends.
                unsafe { cancel(partition, 0, 0) };
            }
        });
        if let Err(e) = probe(&api, &mut guest) {
            println!("stopped: {e}");
        }
        done.store(true, Ordering::Relaxed);
    });
    // SAFETY: the partition this probe made.
    unsafe {
        (api.delete_vp)(partition, 0);
        (api.delete_partition)(partition);
    }
}
