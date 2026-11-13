//! File System Events (CoreServices' FSEvents.framework, FSEvents.h): what changes beneath
//! directories, each file's change said by its path, the changes this process makes
//! itself left out (`kFSEventStreamCreateFlagIgnoreSelf`). A share process's watch of the
//! directories it shares (D235): one stream for all of them, which the kernel's event log
//! feeds however deep the tree, and no descriptor of its.

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use crate::frameworks::{self, CFTypeRef, Owned};

/// `kFSEventStreamEventIdSinceNow`.
const SINCE_NOW: u64 = u64::MAX;
/// `kFSEventStreamCreateFlagNoDefer`: the first event of a quiet stream told at once.
const NO_DEFER: u32 = 0x02;
/// `kFSEventStreamCreateFlagIgnoreSelf`: no change this process makes.
const IGNORE_SELF: u32 = 0x08;
/// `kFSEventStreamCreateFlagFileEvents`: each file's change, by its own path.
const FILE_EVENTS: u32 = 0x10;

/// `kFSEventStreamEventFlagMustScanSubDirs`: changes beneath the path were not each said.
pub const MUST_SCAN_SUBDIRS: u32 = 0x01;
/// `kFSEventStreamEventFlagRootChanged`: a directory watched was moved or removed.
pub const ROOT_CHANGED: u32 = 0x20;

/// A change: the path of what changed, and its event's flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: Vec<u8>,
    pub flags: u32,
}

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = unsafe extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

/// FSEvents' functions, from CoreServices, and libdispatch's, libSystem's.
#[allow(non_snake_case)]
struct Api {
    FSEventStreamCreate:
        unsafe extern "C" fn(CFTypeRef, Callback, *const Context, CFTypeRef, u64, f64, u32) -> *mut c_void,
    FSEventStreamSetDispatchQueue: unsafe extern "C" fn(*mut c_void, *mut c_void),
    FSEventStreamStart: unsafe extern "C" fn(*mut c_void) -> u8,
    FSEventStreamStop: unsafe extern "C" fn(*mut c_void),
    FSEventStreamInvalidate: unsafe extern "C" fn(*mut c_void),
    FSEventStreamRelease: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: function pointers of a process-wide framework, never written.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

const CORE_SERVICES: &CStr = c"/System/Library/Frameworks/CoreServices.framework/CoreServices";

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let cs = frameworks::open(CORE_SERVICES)?;
        macro_rules! f {
            ($name:literal) => {{
                let at = frameworks::symbol(cs, $name)?;
                // SAFETY: the framework's own function of that name, whose C signature the
                // field it fills declares (FSEvents.h).
                unsafe { frameworks::function(at) }
            }};
        }
        Ok(Api {
            FSEventStreamCreate: f!(c"FSEventStreamCreate"),
            FSEventStreamSetDispatchQueue: f!(c"FSEventStreamSetDispatchQueue"),
            FSEventStreamStart: f!(c"FSEventStreamStart"),
            FSEventStreamStop: f!(c"FSEventStreamStop"),
            FSEventStreamInvalidate: f!(c"FSEventStreamInvalidate"),
            FSEventStreamRelease: f!(c"FSEventStreamRelease"),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// A stream of the changes beneath directories, each sent on its sink as it is told: once
/// dropped, stopped, and its sink let go of after its last callback.
#[derive(Debug)]
pub struct Stream {
    stream: *mut c_void,
    queue: *mut c_void,
    sink: *mut Sender<Change>,
}

// SAFETY: a stream is stopped, invalidated and released from any thread (FSEvents.h).
unsafe impl Send for Stream {}

/// Watches the directories at `paths` for every change beneath them but this process's,
/// each sent on `sink` as FSEvents tells it, with no wait for more (a latency of none): a
/// stream's changes come in batches already, its first at once (`NoDefer`).
pub fn watch(paths: &[&[u8]], sink: Sender<Change>) -> Result<Stream, String> {
    let api = api()?;
    let cf = frameworks::api()?;
    let mut strings = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::str::from_utf8(path).map_err(|_| "a path that is not UTF-8")?;
        strings.push(frameworks::string(cf, text).ok_or("a path no string holds")?);
    }
    let refs: Vec<CFTypeRef> = strings.iter().map(|s| s.ptr).collect();
    let array: Owned<'_> = frameworks::array(cf, &refs).ok_or("no array of the paths")?;
    let sink = Box::into_raw(Box::new(sink));
    let context = Context {
        version: 0,
        info: sink.cast(),
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    // SAFETY: a context and paths that outlive the call, which copies them; the callback
    // reads the sink, freed only once the stream has been invalidated.
    let stream = unsafe {
        (api.FSEventStreamCreate)(
            std::ptr::null(),
            told,
            &context,
            array.ptr,
            SINCE_NOW,
            0.0,
            NO_DEFER | IGNORE_SELF | FILE_EVENTS,
        )
    };
    if stream.is_null() {
        // SAFETY: the sink made above, which no stream holds.
        drop(unsafe { Box::from_raw(sink) });
        return Err("FSEventStreamCreate made no stream".into());
    }
    // SAFETY: a serial queue of its own, for the stream's callbacks in order.
    let queue = unsafe { dispatch_queue_create(c"shards.notices".as_ptr(), std::ptr::null()) };
    let made = Stream { stream, queue, sink };
    if queue.is_null() {
        return Err("dispatch_queue_create made no queue".into());
    }
    // SAFETY: the stream and queue just made, the stream started once it has its queue.
    let started = unsafe {
        (api.FSEventStreamSetDispatchQueue)(stream, queue);
        (api.FSEventStreamStart)(stream)
    };
    if started == 0 {
        return Err("FSEventStreamStart did not start".into());
    }
    Ok(made)
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            // SAFETY: the stream this made: stopped (a stream never started stops as
            // none), invalidated, which ends its callbacks, then released, once each.
            unsafe {
                (api.FSEventStreamStop)(self.stream);
                (api.FSEventStreamInvalidate)(self.stream);
                (api.FSEventStreamRelease)(self.stream);
            }
        }
        if !self.queue.is_null() {
            // SAFETY: the queue this made, released once, its stream gone from it.
            unsafe { dispatch_release(self.queue) };
        }
        // SAFETY: the sink this made, no callback left to read it once invalidated.
        drop(unsafe { Box::from_raw(self.sink) });
    }
}

/// A batch of changes told: each path and its flags sent on the stream's sink.
unsafe extern "C" fn told(
    _stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    if info.is_null() || paths.is_null() || flags.is_null() {
        return;
    }
    // SAFETY: the stream's context, its sink, which outlives every callback.
    let sink = unsafe { &*info.cast::<Sender<Change>>() };
    // SAFETY: without kFSEventStreamCreateFlagUseCFTypes, `count` C strings and as many
    // flags, valid for the callback (FSEvents.h, FSEventStreamCallback).
    let (paths, flags) = unsafe {
        (
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), count),
            std::slice::from_raw_parts(flags, count),
        )
    };
    for (path, flags) in paths.iter().zip(flags) {
        if path.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated path of FSEvents', valid for the callback.
        let path = unsafe { CStr::from_ptr(*path) }.to_bytes().to_vec();
        let _ = sink.send(Change { path, flags: *flags });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A file written beneath a directory watched is told by its path; one this process
    /// writes too, which IgnoreSelf leaves out, is told only through another process.
    #[test]
    fn a_change_beneath_is_told_by_its_path() {
        let dir = shards_testdir::TempDir::new("fsevents").unwrap();
        // Its real path: FSEvents tells paths without the links on the way (/var is
        // /private/var).
        let root = std::fs::canonicalize(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        use std::os::unix::ffi::OsStrExt as _;
        let _stream = watch(&[root.as_os_str().as_bytes()], tx).unwrap();
        let other = root.join("by-another");
        let wrote = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("echo x > \"$1\"")
            .arg("sh")
            .arg(&other)
            .status()
            .unwrap();
        assert!(wrote.success());
        let want = other.as_os_str().as_bytes().to_vec();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let change = rx.recv_timeout(left).expect("no change told");
            if change.path == want {
                break;
            }
        }
    }
}
