//! Checked native startup inputs. Values bind images/configuration, never a process,
//! physical grant or permission to launch a worker. Native owners retain that authority.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::time::Instant;

use sha2::{Digest as _, Sha256};

use crate::{GuestSpec, LaunchError, PreparedBoot, Refusal, RuntimePaths, Share};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageIdentity {
    bytes: u64,
    sha256: [u8; 32],
}

impl ImageIdentity {
    pub fn new(bytes: u64, sha256: [u8; 32]) -> Result<Self, Refusal> {
        if bytes == 0 {
            return Err(Refusal::Launch(LaunchError::ImageSize));
        }
        Ok(Self { bytes, sha256 })
    }
    pub fn bytes(self) -> u64 {
        self.bytes
    }
    pub fn sha256(self) -> [u8; 32] {
        self.sha256
    }
}

/// Declarations in the SAME cost evidence, already hashed as RuntimeCost.id by
/// the caller. A declaration is checked against actual bytes before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeImages {
    evidence: [u8; 32],
    cli: ImageIdentity,
    kernel: ImageIdentity,
    cpu_vmm: Option<ImageIdentity>,
    gpu_vmm: Option<ImageIdentity>,
}

impl NativeImages {
    pub fn new(
        evidence: [u8; 32],
        cli: ImageIdentity,
        kernel: ImageIdentity,
        cpu_vmm: Option<ImageIdentity>,
        gpu_vmm: Option<ImageIdentity>,
    ) -> Result<Self, Refusal> {
        if evidence == [0; 32] || (cpu_vmm.is_none() && gpu_vmm.is_none()) {
            return Err(Refusal::Launch(LaunchError::ImageMissing));
        }
        Ok(Self {
            evidence,
            cli,
            kernel,
            cpu_vmm,
            gpu_vmm,
        })
    }
    pub fn evidence(self) -> [u8; 32] {
        self.evidence
    }
    pub fn cli(self) -> ImageIdentity {
        self.cli
    }
    pub fn kernel(self) -> ImageIdentity {
        self.kernel
    }
    pub fn vmm(self, gpu: bool) -> Result<ImageIdentity, Refusal> {
        (if gpu { self.gpu_vmm } else { self.cpu_vmm }).ok_or(Refusal::Launch(LaunchError::ImageMissing))
    }
}

/// Borrowed inputs from the same initialized runtime and original console bound.
/// The existing standalone preparation does not require these declarations.
#[derive(Debug, Clone, Copy)]
pub struct NativeBoot<'a> {
    pub paths: &'a RuntimePaths,
    pub images: NativeImages,
    pub console_kept: usize,
}

#[derive(Debug)]
pub(crate) struct Inputs {
    kernel: std::path::PathBuf,
    shares: Vec<Share>,
    console_kept: usize,
    gpu: bool,
    images: Option<NativeImages>,
}

impl Inputs {
    pub(crate) fn new(
        spec: &GuestSpec,
        console_kept: usize,
        native: Option<&NativeBoot<'_>>,
    ) -> Result<Self, Refusal> {
        spec.shares
            .len()
            .checked_mul(size_of::<Share>())
            .ok_or(Refusal::Launch(LaunchError::Count))?;
        let mut shares = Vec::new();
        shares
            .try_reserve_exact(spec.shares.len())
            .map_err(|_| Refusal::Launch(LaunchError::Allocation))?;
        let path = |path: &Path| -> Result<std::path::PathBuf, Refusal> {
            if native.is_some() && (!path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0)) {
                return Err(Refusal::Launch(LaunchError::NativeInputs));
            }
            Ok(crate::client::argument_copy(path.as_os_str())?.into())
        };
        for share in &spec.shares {
            shares.push(Share {
                dir: path(&share.dir)?,
                read_only: share.read_only,
            });
        }
        let gpu = spec.gpu || !spec.gpu_devices.is_empty();
        if let Some(native) = native {
            native.images.vmm(gpu)?;
        }
        Ok(Self {
            kernel: path(&spec.kernel)?,
            shares,
            console_kept,
            gpu,
            images: native.map(|native| native.images),
        })
    }
    pub(crate) fn is_bound(&self) -> bool {
        self.images.is_some()
    }
}

/// A view of this exact checked boot. No opaque argv parsing or new owner/copy.
#[derive(Debug, Clone, Copy)]
pub struct NativeStartup<'a> {
    binary: &'a Path,
    kernel: &'a Path,
    shares: &'a [Share],
    paths: &'a RuntimePaths,
    images: NativeImages,
    gpu: bool,
    console_kept: usize,
}

impl PreparedBoot {
    pub fn native_startup(&self) -> Result<NativeStartup<'_>, Refusal> {
        let images = self
            .startup
            .images
            .ok_or(Refusal::Launch(LaunchError::ImageMissing))?;
        let paths = self
            .runtime_paths
            .as_ref()
            .ok_or(Refusal::Launch(LaunchError::NativeInputs))?;
        images.vmm(self.startup.gpu)?;
        Ok(NativeStartup {
            binary: &self.binary,
            kernel: &self.startup.kernel,
            shares: &self.startup.shares,
            paths,
            images,
            gpu: self.startup.gpu,
            console_kept: self.startup.console_kept,
        })
    }
}

impl NativeStartup<'_> {
    pub fn binary(&self) -> &Path {
        self.binary
    }
    pub fn kernel(&self) -> &Path {
        self.kernel
    }
    pub fn shares(&self) -> &[Share] {
        self.shares
    }
    pub fn paths(&self) -> &RuntimePaths {
        self.paths
    }
    pub fn images(&self) -> NativeImages {
        self.images
    }
    pub fn gpu(&self) -> bool {
        self.gpu
    }
    pub fn console_kept(&self) -> usize {
        self.console_kept
    }
    pub fn console_pin(&self) -> [u8; 32] {
        self.images.kernel.sha256
    }
    pub fn verify_cli(&self, deadline: Instant) -> Result<File, Refusal> {
        verify_native_image(self.binary, self.images.cli, deadline)
    }
    pub fn verify_kernel(&self, deadline: Instant) -> Result<File, Refusal> {
        verify_native_image(self.kernel, self.images.kernel, deadline)
    }
}

fn io(operation: &'static str, why: std::io::Error) -> Refusal {
    Refusal::ManagedIo {
        operation,
        kind: why.kind(),
        code: why.raw_os_error(),
    }
}

fn remaining(deadline: Instant) -> Result<(), Refusal> {
    if Instant::now() >= deadline {
        return Err(Refusal::Launch(LaunchError::ImageDeadline));
    }
    Ok(())
}

/// SHA-256's block, 64 bytes (FIPS 180-4 §5.1.1): the least the digest takes in at once, and
/// what an image is read in where its file system names no size of its own.
const SHA256_BLOCK: u64 = 64;

/// How much of an image one read takes: the size its file system prefers for I/O, stat(2)'s
/// `st_blksize` ("the preferred block size for efficient file system I/O") or, on Windows,
/// the volume's `PhysicalBytesPerSectorForPerformance` (FILE_STORAGE_INFO). A launch hashes
/// the whole `shards` binary and kernel, tens of MiB, which SHA-256's 64-byte blocks read in
/// about a million reads. Where the file system says nothing, SHA-256's block.
fn read_size(file: &File, metadata: &Metadata) -> u64 {
    let preferred = preferred_io(file, metadata).unwrap_or(0);
    if preferred == 0 { SHA256_BLOCK } else { preferred }
}

#[cfg(unix)]
fn preferred_io(_file: &File, metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(metadata.blksize())
}

#[cfg(windows)]
fn preferred_io(file: &File, _metadata: &Metadata) -> Option<u64> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STORAGE_INFO, FileStorageInfo, GetFileInformationByHandleEx,
    };
    let mut info = FILE_STORAGE_INFO::default();
    let size = u32::try_from(std::mem::size_of::<FILE_STORAGE_INFO>()).ok()?;
    // SAFETY: an open file's handle, and a FILE_STORAGE_INFO of `size` bytes for the call to
    // fill; nothing is kept past it.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStorageInfo,
            (&raw mut info).cast(),
            size,
        )
    };
    (ok != 0).then_some(u64::from(info.PhysicalBytesPerSectorForPerformance))
}

#[cfg(not(any(unix, windows)))]
fn preferred_io(_file: &File, _metadata: &Metadata) -> Option<u64> {
    None
}

fn same(before: &Metadata, after: &Metadata) -> bool {
    if !after.is_file() || before.len() != after.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        before.creation_time() == after.creation_time()
            && before.last_write_time() == after.last_write_time()
            && before.file_attributes() == after.file_attributes()
    }
    #[cfg(not(any(unix, windows)))]
    false
}

/// Hash exactly the declared bytes from a regular, non-symlink/reparse file and
/// retain that SAME open file. This is a file-content witness, not an exec token:
/// the native owner must join its final held image and retain the handle until
/// consumption. Executing a separately reopened path would reintroduce TOCTOU.
pub fn verify_native_image(path: &Path, expected: ImageIdentity, deadline: Instant) -> Result<File, Refusal> {
    remaining(deadline)?;
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(Refusal::Launch(LaunchError::NativeInputs));
    }
    let located = std::fs::symlink_metadata(path).map_err(|why| io("locating native image", why))?;
    if !located.is_file() {
        return Err(Refusal::Launch(LaunchError::ImageChanged));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|why| io("opening native image", why))?;
    let before = file.metadata().map_err(|why| io("sizing native image", why))?;
    if !before.is_file() || before.len() != expected.bytes || !same(&located, &before) {
        return Err(Refusal::Launch(LaunchError::ImageChanged));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if before.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(Refusal::Launch(LaunchError::ImageChanged));
        }
    }
    let mut digest = Sha256::new();
    let mut block = Vec::new();
    let piece = usize::try_from(read_size(&file, &before).min(expected.bytes))
        .map_err(|_| Refusal::Launch(LaunchError::Count))?;
    block
        .try_reserve_exact(piece)
        .map_err(|_| Refusal::Launch(LaunchError::Allocation))?;
    block.resize(piece, 0);
    let mut left = expected.bytes;
    while left != 0 {
        remaining(deadline)?;
        let count = usize::try_from(
            left.min(u64::try_from(block.len()).map_err(|_| Refusal::Launch(LaunchError::Count))?),
        )
        .map_err(|_| Refusal::Launch(LaunchError::Count))?;
        let room = block
            .get_mut(..count)
            .ok_or(Refusal::Launch(LaunchError::Count))?;
        #[cfg(test)]
        READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        let got = match file.read(room) {
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            read => read.map_err(|why| io("reading native image", why))?,
        };
        if got == 0 {
            return Err(Refusal::Launch(LaunchError::ImageChanged));
        }
        digest.update(room.get(..got).ok_or(Refusal::Launch(LaunchError::Count))?);
        left = left
            .checked_sub(u64::try_from(got).map_err(|_| Refusal::Launch(LaunchError::Count))?)
            .ok_or(Refusal::Launch(LaunchError::Count))?;
    }
    remaining(deadline)?;
    let mut extra = [0u8; 1];
    if file
        .read(&mut extra)
        .map_err(|why| io("checking native image end", why))?
        != 0
    {
        return Err(Refusal::Launch(LaunchError::ImageChanged));
    }
    let after = file
        .metadata()
        .map_err(|why| io("checking native image identity", why))?;
    let at_path = std::fs::symlink_metadata(path).map_err(|why| io("checking native image path", why))?;
    if !same(&before, &after) || !same(&before, &at_path) {
        return Err(Refusal::Launch(LaunchError::ImageChanged));
    }
    if <[u8; 32]>::from(digest.finalize()) != expected.sha256 {
        return Err(Refusal::Launch(LaunchError::ImageDigest));
    }
    file.rewind()
        .map_err(|why| io("rewinding verified native image", why))?;
    remaining(deadline)?;
    Ok(file)
}

#[cfg(test)]
thread_local! {
    /// The reads of an image's bytes this thread's verifications made.
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;
