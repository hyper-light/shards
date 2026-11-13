//! Bounded receipt from the carried GPU process after its exact context filter ACK.

use super::{JoinError, MAX_PHYSICAL_GPUS, PhysicalGpu, PhysicalGpuError, check_selection};
use alloc::{string::String, vec::Vec};
use core::fmt::{self, Write};

const HEADER: &str = "shards-gpu-selection 3";
const IDENTITY_HEADER: &str = "shards-gpu-selection 2";
const IDENTITY_LINE_BYTES: usize =
    "metal:ffffffffffffffff\tffffffffffffffffffffffffffffffff\tpci:ffffffff:ff:1f.7\tffffffffffffffff\n"
        .len();
/// The receipt's lines but its devices', each at its longest: the host-visible bytes a u64's
/// digits, the device count [`MAX_PHYSICAL_GPUS`]'s.
const FIXED_BYTES: usize = HEADER.len()
    + "\nhost-visible ".len()
    + decimal_digits(u64::MAX as u128)
    + "\ndevices ".len()
    + decimal_digits(MAX_PHYSICAL_GPUS as u128)
    + "\nend\n".len();
/// The fewest bytes a device line takes: its Vulkan UUID's hex digits, the one field every
/// line has at a fixed width, and its end. A receipt holds no more devices than its own
/// bytes hold lines of these, which bounds a count before anything is reserved for it.
const DEVICE_LINE_LEAST: usize = 2 * core::mem::size_of::<[u8; 16]>() + "\n".len();

/// The decimal digits `n` is written with.
const fn decimal_digits(mut n: u128) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// Vulkan's VK_MAX_PHYSICAL_DEVICE_NAME_SIZE includes the UTF-8 string's NUL.
pub const GPU_NAME_BYTES: usize = 256;

/// A name a Vulkan driver wrote into one of its structures' fixed `char` arrays (a device's
/// `deviceName`, an extension's `extensionName`): its bytes before the array's first NUL,
/// taken within the array; none where the array holds no NUL, past which C's string
/// functions read (resources/gpu/native-identity.c checks each with `memchr` first).
pub fn fixed_name(array: &[u8]) -> Option<&core::ffi::CStr> {
    core::ffi::CStr::from_bytes_until_nul(array).ok()
}

/// The renderer driver's reported name, never a selector or a catalog/model guess.
/// Fixed storage keeps ownership transfer fallible only where text is requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuName {
    bytes: [u8; GPU_NAME_BYTES],
}

impl GpuName {
    pub fn new(text: &str) -> Result<Self, GpuProbeError> {
        if text.len() >= GPU_NAME_BYTES || text.contains('\0') {
            return Err(GpuProbeError::Properties);
        }
        let mut bytes = [0; GPU_NAME_BYTES];
        bytes
            .get_mut(..text.len())
            .ok_or(GpuProbeError::Properties)?
            .copy_from_slice(text.as_bytes());
        Ok(Self { bytes })
    }

    pub fn from_buffer(bytes: &[u8; GPU_NAME_BYTES]) -> Result<Self, GpuProbeError> {
        Self::new(Self::text(bytes)?)
    }

    pub fn as_str(&self) -> Result<&str, GpuProbeError> {
        Self::text(&self.bytes)
    }

    fn text(bytes: &[u8; GPU_NAME_BYTES]) -> Result<&str, GpuProbeError> {
        fixed_name(bytes)
            .ok_or(GpuProbeError::Properties)?
            .to_str()
            .map_err(|_| GpuProbeError::Properties)
    }

    pub fn try_text(&self) -> Result<String, GpuProbeError> {
        let name = self.as_str()?;
        let mut text = String::new();
        text.try_reserve_exact(name.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        text.push_str(name);
        Ok(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedGpu {
    pub physical: PhysicalGpu,
    pub uuid: [u8; 16],
    /// Complete per-device PCI properties, never an ADL assumed segment.
    pub pci: Option<PhysicalGpu>,
    /// Exact native byte order from VkPhysicalDeviceIDProperties when valid.
    pub luid: Option<[u8; 8]>,
    /// Actual VkPhysicalDeviceProperties::deviceName when reported. None stays unknown.
    pub name: Option<GpuName>,
    /// Checked sum of this selected device's DEVICE_LOCAL heaps, in bytes. This is
    /// neither host RAM, free VRAM, a working-set budget nor a utilization reading.
    pub device_local_bytes: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GpuProbe {
    pub devices: Vec<ResolvedGpu>,
    pub host_visible_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuProbeError {
    Selection(PhysicalGpuError),
    Empty,
    Syntax,
    Count,
    Identity,
    Memory,
    Allocation,
    Unavailable,
    Pending,
    Properties,
    Scope,
    ReportTaken,
    /// The VM process refused its selection, as it said in place of a receipt: certain,
    /// where a receipt its client could not read leaves its selection unknown.
    Refused(GpuRefusal),
    /// The VM process's report writer closed with nothing written: it ended before its
    /// renderer said anything of its selection (its own end says why).
    Unreported,
}

/// Why a VM process's renderer serves none of its selection (D232), which it says in place of
/// a receipt: as a probe's answer, or as a managed boot's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuRefusal {
    /// The selection's identities, joined to the host's live Vulkan devices.
    Join(JoinError),
    /// The selected GPUs have no memory the host can map, which Venus needs.
    NoHostMemory,
    /// The renderer refused the selection's exact filter, with the renderer's code.
    Acknowledgment(u32),
}

/// A refusal's record: this line, then its reason's.
const REFUSAL_HEADER: &str = "shards-gpu-refusal 1";

impl GpuRefusal {
    /// Its record, which a probe prints in place of its receipt, no longer than the receipt of
    /// the fewest GPUs a selection names.
    pub fn encode(&self) -> Result<String, GpuProbeError> {
        rendered(probe_bytes(1)?, |out| self.render(out))
    }

    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        writeln!(out, "{REFUSAL_HEADER}")?;
        match self {
            Self::Join(join) => {
                write!(out, "join {}", join_token(*join))?;
                if let JoinError::Selection(why) = join {
                    write!(out, " {}", selection_token(*why))?;
                }
            }
            Self::NoHostMemory => out.write_str("no-host-memory")?,
            Self::Acknowledgment(code) => write!(out, "acknowledgment {code}")?,
        }
        out.write_char('\n')
    }

    /// The refusal a record's lines after its header say: one reason, and nothing after it.
    fn parse<'a>(mut lines: impl Iterator<Item = &'a str>) -> Result<Self, GpuProbeError> {
        let reason = lines.next().ok_or(GpuProbeError::Syntax)?;
        if lines.next().is_some() {
            return Err(GpuProbeError::Syntax);
        }
        if reason == "no-host-memory" {
            return Ok(Self::NoHostMemory);
        }
        if let Some(code) = reason.strip_prefix("acknowledgment ") {
            return Ok(Self::Acknowledgment(canonical_u32(code)?));
        }
        let join = reason.strip_prefix("join ").ok_or(GpuProbeError::Syntax)?;
        Ok(Self::Join(match join.split_once(' ') {
            Some(("selection", why)) => JoinError::Selection(selection_from(why)?),
            Some(_) => return Err(GpuProbeError::Syntax),
            None => join_from(join)?,
        }))
    }
}

fn join_token(join: JoinError) -> &'static str {
    match join {
        JoinError::Selection(_) => "selection",
        JoinError::Count => "count",
        JoinError::Allocation => "allocation",
        JoinError::Missing => "missing",
        JoinError::Ambiguous => "ambiguous",
        JoinError::Aliased => "aliased",
        JoinError::InvalidUuid => "invalid-uuid",
        JoinError::InvalidNative => "invalid-native",
        JoinError::Memory => "memory",
    }
}

/// A join's reason by its token; `selection`, whose reason follows it, is none alone.
fn join_from(token: &str) -> Result<JoinError, GpuProbeError> {
    Ok(match token {
        "count" => JoinError::Count,
        "allocation" => JoinError::Allocation,
        "missing" => JoinError::Missing,
        "ambiguous" => JoinError::Ambiguous,
        "aliased" => JoinError::Aliased,
        "invalid-uuid" => JoinError::InvalidUuid,
        "invalid-native" => JoinError::InvalidNative,
        "memory" => JoinError::Memory,
        _ => return Err(GpuProbeError::Syntax),
    })
}

fn selection_token(why: PhysicalGpuError) -> &'static str {
    match why {
        PhysicalGpuError::Syntax => "syntax",
        PhysicalGpuError::PciAddress => "pci-address",
        PhysicalGpuError::MetalRegistry => "metal-registry",
        PhysicalGpuError::Count => "count",
        PhysicalGpuError::Duplicate => "duplicate",
        PhysicalGpuError::Allocation => "allocation",
    }
}

fn selection_from(token: &str) -> Result<PhysicalGpuError, GpuProbeError> {
    Ok(match token {
        "syntax" => PhysicalGpuError::Syntax,
        "pci-address" => PhysicalGpuError::PciAddress,
        "metal-registry" => PhysicalGpuError::MetalRegistry,
        "count" => PhysicalGpuError::Count,
        "duplicate" => PhysicalGpuError::Duplicate,
        "allocation" => PhysicalGpuError::Allocation,
        _ => return Err(GpuProbeError::Syntax),
    })
}

/// A `u32` as a record writes it: decimal digits alone, no leading zero but zero's own.
fn canonical_u32(text: &str) -> Result<u32, GpuProbeError> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return Err(GpuProbeError::Syntax);
    }
    text.parse().map_err(|_| GpuProbeError::Syntax)
}

impl fmt::Display for GpuRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Join(why) => why.fmt(out),
            Self::NoHostMemory => {
                out.write_str("the selected GPU has no memory the host can map, which Venus needs")
            }
            Self::Acknowledgment(code) => {
                write!(out, "the renderer refused the exact GPU filter ACK ({code})")
            }
        }
    }
}
impl core::error::Error for GpuRefusal {}

/// The envelope for a VMM-private pipe report, bound to the native scope prepared
/// before effects. The consumer must also verify the private native writer; this
/// scope value alone confers no observation or physical attachment authority.
#[derive(Debug, PartialEq, Eq)]
pub struct GpuReport {
    pub scope: [u8; 16],
    pub probe: GpuProbe,
}

const REPORT_PREFIX: &str = "shards-vmm-gpu 1 ";

pub fn report_bytes(count: usize) -> Result<usize, GpuProbeError> {
    probe_bytes(count)?
        .checked_add(REPORT_PREFIX.len())
        .and_then(|bytes| bytes.checked_add(core::mem::size_of::<[u8; 16]>().checked_mul(2)?))
        .and_then(|bytes| bytes.checked_add("\n".len()))
        .ok_or(GpuProbeError::Count)
}

/// The text `render` writes, measured first, no more than `bound` bytes, so that its storage
/// is reserved once, fallibly, for exactly that.
fn rendered(bound: usize, render: impl Fn(&mut dyn Write) -> fmt::Result) -> Result<String, GpuProbeError> {
    struct Count {
        bytes: usize,
        bound: usize,
    }
    impl Write for Count {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.bytes = self
                .bytes
                .checked_add(text.len())
                .filter(|bytes| *bytes <= self.bound)
                .ok_or(fmt::Error)?;
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, bound };
    render(&mut count).map_err(|_| GpuProbeError::Count)?;
    let mut text = String::new();
    text.try_reserve_exact(count.bytes)
        .map_err(|_| GpuProbeError::Allocation)?;
    render(&mut text).map_err(|_| GpuProbeError::Count)?;
    Ok(text)
}

/// A report's first line: the envelope, bound to its launch's scope.
fn render_scope(scope: [u8; 16], out: &mut dyn Write) -> fmt::Result {
    out.write_str(REPORT_PREFIX)?;
    for byte in scope {
        write!(out, "{byte:02x}")?;
    }
    out.write_char('\n')
}

impl GpuReport {
    pub fn check(&self, scope: [u8; 16], selected: &[PhysicalGpu]) -> Result<(), GpuProbeError> {
        if scope == [0; 16] || self.scope != scope {
            return Err(GpuProbeError::Scope);
        }
        self.probe.check(selected)
    }

    pub fn encode(&self) -> Result<String, GpuProbeError> {
        self.check(self.scope, &self.probe.selectors()?)?;
        rendered(report_bytes(self.probe.devices.len())?, |out| {
            render_scope(self.scope, out)?;
            self.probe.render(out)
        })
    }

    /// A refusal's frame (D232), scoped as a receipt's is: what a VM process writes to its
    /// client's private pipe in place of its receipt where its renderer serves none of its
    /// selection, no longer than the report of the fewest GPUs a selection names.
    pub fn refusal(scope: [u8; 16], refusal: GpuRefusal) -> Result<String, GpuProbeError> {
        if scope == [0; 16] {
            return Err(GpuProbeError::Scope);
        }
        rendered(report_bytes(1)?, |out| {
            render_scope(scope, out)?;
            refusal.render(out)
        })
    }

    /// The report a VM process wrote, its scope `scope`'s and its GPUs those `selected`
    /// names; or, where it wrote its refusal instead, that refusal
    /// ([`GpuProbeError::Refused`]); or where its writer closed with nothing written,
    /// [`GpuProbeError::Unreported`].
    pub fn parse(text: &str, scope: [u8; 16], selected: &[PhysicalGpu]) -> Result<Self, GpuProbeError> {
        if text.is_empty() {
            return Err(GpuProbeError::Unreported);
        }
        if text.len() > report_bytes(selected.len())? {
            return Err(GpuProbeError::Count);
        }
        let (header, probe) = text.split_once('\n').ok_or(GpuProbeError::Syntax)?;
        let reported = header.strip_prefix(REPORT_PREFIX).ok_or(GpuProbeError::Syntax)?;
        let reported = fixed_hex(reported)?;
        if scope == [0; 16] || reported != scope {
            return Err(GpuProbeError::Scope);
        }
        let report = Self {
            scope: reported,
            probe: GpuProbe::parse_count(probe, Some(selected.len()))?,
        };
        report.check(scope, selected)?;
        Ok(report)
    }
}

pub fn probe_bytes(count: usize) -> Result<usize, GpuProbeError> {
    if count == 0 || count > MAX_PHYSICAL_GPUS {
        return Err(GpuProbeError::Count);
    }
    let line_bytes = GPU_NAME_BYTES
        .checked_mul(2)
        .and_then(|name| name.checked_add("\t\t18446744073709551615".len()))
        .and_then(|facts| facts.checked_add(IDENTITY_LINE_BYTES))
        .ok_or(GpuProbeError::Count)?;
    count
        .checked_mul(line_bytes)
        .and_then(|n| n.checked_add(FIXED_BYTES))
        .ok_or(GpuProbeError::Count)
}

pub fn uuid_from_hex(text: &str) -> Result<[u8; 16], GpuProbeError> {
    fixed_hex(text)
}

fn fixed_hex<const N: usize>(text: &str) -> Result<[u8; N], GpuProbeError> {
    if N.checked_mul(2) != Some(text.len()) {
        return Err(GpuProbeError::Syntax);
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => byte.checked_sub(b'0'),
        b'a'..=b'f' => byte.checked_sub(b'a').and_then(|b| b.checked_add(10)),
        b'A'..=b'F' => byte.checked_sub(b'A').and_then(|b| b.checked_add(10)),
        _ => None,
    };
    let mut uuid = [0; N];
    // The checked exact 2*N length above rules out a trailing byte.
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (slot, pair) in uuid.iter_mut().zip(pairs) {
        let high = pair
            .first()
            .copied()
            .and_then(nibble)
            .ok_or(GpuProbeError::Syntax)?;
        let low = pair
            .get(1)
            .copied()
            .and_then(nibble)
            .ok_or(GpuProbeError::Syntax)?;
        *slot = (high << 4) | low;
    }
    Ok(uuid)
}

impl GpuProbe {
    pub fn check(&self, selected: &[PhysicalGpu]) -> Result<(), GpuProbeError> {
        check_selection(selected).map_err(GpuProbeError::Selection)?;
        if selected.is_empty() || self.devices.len() != selected.len() {
            return Err(GpuProbeError::Empty);
        }
        if self.host_visible_bytes == 0 {
            return Err(GpuProbeError::Memory);
        }
        let mut uuids = Vec::new();
        uuids
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        for (resolved, selected) in self.devices.iter().zip(selected) {
            if resolved.physical != *selected || resolved.uuid == [0; 16] {
                return Err(GpuProbeError::Identity);
            }
            if let Some(pci) = resolved.pci
                && (!matches!(pci, PhysicalGpu::Pci { .. }) || pci.check().is_err())
            {
                return Err(GpuProbeError::Identity);
            }
            let matches = match resolved.physical {
                PhysicalGpu::Pci { .. } => resolved.pci == Some(resolved.physical),
                PhysicalGpu::MetalRegistry(id) => {
                    resolved.luid.is_some_and(|raw| u64::from_be_bytes(raw) == id)
                }
                PhysicalGpu::Luid(id) => resolved.luid.is_some_and(|raw| u64::from_le_bytes(raw) == id),
            };
            if !matches {
                return Err(GpuProbeError::Identity);
            }
            if let Some(name) = &resolved.name {
                name.as_str()?;
            }
            uuids.push(resolved.uuid);
        }
        uuids.sort_unstable();
        if uuids.windows(2).any(|pair| pair.first() == pair.last()) {
            return Err(GpuProbeError::Identity);
        }
        Ok(())
    }

    pub fn selectors(&self) -> Result<Vec<PhysicalGpu>, GpuProbeError> {
        probe_bytes(self.devices.len())?;
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        selected.extend(self.devices.iter().map(|device| device.physical));
        self.check(&selected)?;
        Ok(selected)
    }

    pub fn try_clone(&self) -> Result<Self, GpuProbeError> {
        self.selectors()?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        devices.extend_from_slice(&self.devices);
        Ok(Self {
            devices,
            host_visible_bytes: self.host_visible_bytes,
        })
    }

    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        writeln!(out, "{HEADER}")?;
        writeln!(out, "host-visible {}", self.host_visible_bytes)?;
        writeln!(out, "devices {}", self.devices.len())?;
        for device in &self.devices {
            write!(out, "{}\t", device.physical)?;
            for byte in device.uuid {
                write!(out, "{byte:02x}")?;
            }
            out.write_char('\t')?;
            match device.pci {
                Some(pci) => write!(out, "{pci}")?,
                None => out.write_char('-')?,
            };
            out.write_char('\t')?;
            match device.luid {
                Some(luid) => {
                    for byte in luid {
                        write!(out, "{byte:02x}")?;
                    }
                }
                None => out.write_char('-')?,
            };
            out.write_char('\t')?;
            match device.name {
                Some(name) => {
                    for byte in name.bytes {
                        write!(out, "{byte:02x}")?;
                    }
                }
                None => out.write_char('-')?,
            }
            out.write_char('\t')?;
            match device.device_local_bytes {
                Some(bytes) => write!(out, "{bytes}")?,
                None => out.write_char('-')?,
            }
            out.write_char('\n')?;
        }
        writeln!(out, "end")
    }

    pub fn encode(&self) -> Result<String, GpuProbeError> {
        self.selectors()?;
        rendered(probe_bytes(self.devices.len())?, |out| self.render(out))
    }

    /// The receipt a probe printed; or, where it printed its refusal instead, that refusal
    /// ([`GpuProbeError::Refused`]).
    pub fn parse(text: &str) -> Result<Self, GpuProbeError> {
        Self::parse_count(text, None)
    }

    fn parse_count(text: &str, expected: Option<usize>) -> Result<Self, GpuProbeError> {
        if text.len() > probe_bytes(expected.unwrap_or(MAX_PHYSICAL_GPUS))? || !text.ends_with('\n') {
            return Err(GpuProbeError::Syntax);
        }
        let mut lines = text.lines();
        let facts = match lines.next() {
            Some(HEADER) => true,
            Some(IDENTITY_HEADER) => false,
            Some(REFUSAL_HEADER) => {
                return Err(GpuRefusal::parse(lines).map_or_else(|why| why, GpuProbeError::Refused));
            }
            _ => return Err(GpuProbeError::Syntax),
        };
        fn field<'a>(line: Option<&'a str>, prefix: &str) -> Result<&'a str, GpuProbeError> {
            line.and_then(|line| line.strip_prefix(prefix))
                .ok_or(GpuProbeError::Syntax)
        }
        let number = |text: &str| {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(GpuProbeError::Syntax);
            }
            text.parse::<u64>().map_err(|_| GpuProbeError::Count)
        };
        let host_visible_bytes = number(field(lines.next(), "host-visible ")?)?;
        let count =
            usize::try_from(number(field(lines.next(), "devices ")?)?).map_err(|_| GpuProbeError::Count)?;
        probe_bytes(count)?;
        if expected.is_some_and(|expected| count != expected) {
            return Err(GpuProbeError::Count);
        }
        if count > text.len() / DEVICE_LINE_LEAST {
            return Err(GpuProbeError::Syntax);
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(count)
            .map_err(|_| GpuProbeError::Allocation)?;
        for _ in 0..count {
            let mut fields = lines.next().ok_or(GpuProbeError::Syntax)?.split('\t');
            let native = fields.next().ok_or(GpuProbeError::Syntax)?;
            let uuid = fields.next().ok_or(GpuProbeError::Syntax)?;
            let pci = fields.next().ok_or(GpuProbeError::Syntax)?;
            let luid = fields.next().ok_or(GpuProbeError::Syntax)?;
            let (name, device_local_bytes) = if facts {
                let name = fields.next().ok_or(GpuProbeError::Syntax)?;
                let size = fields.next().ok_or(GpuProbeError::Syntax)?;
                let name = if name == "-" {
                    None
                } else {
                    let bytes = fixed_hex(name)?;
                    let name = GpuName::from_buffer(&bytes)?;
                    if name.bytes != bytes {
                        return Err(GpuProbeError::Properties);
                    }
                    Some(name)
                };
                let size = if size == "-" { None } else { Some(number(size)?) };
                (name, size)
            } else {
                (None, None)
            };
            if fields.next().is_some() {
                return Err(GpuProbeError::Syntax);
            }
            devices.push(ResolvedGpu {
                physical: native.parse().map_err(GpuProbeError::Selection)?,
                uuid: uuid_from_hex(uuid)?,
                pci: if pci == "-" {
                    None
                } else {
                    Some(pci.parse().map_err(GpuProbeError::Selection)?)
                },
                luid: if luid == "-" { None } else { Some(fixed_hex(luid)?) },
                name,
                device_local_bytes,
            });
        }
        if lines.next() != Some("end") || lines.next().is_some() {
            return Err(GpuProbeError::Syntax);
        }
        let probe = Self {
            devices,
            host_visible_bytes,
        };
        probe.selectors()?;
        Ok(probe)
    }
}

impl fmt::Display for GpuProbeError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection(why) => why.fmt(out),
            Self::Empty => out.write_str("GPU probe does not contain the exact requested nonempty set"),
            Self::Syntax => out.write_str("GPU probe receipt is malformed or truncated"),
            Self::Count => {
                out.write_str("GPU probe count or byte budget overflows its physical address space")
            }
            Self::Identity => {
                out.write_str("GPU probe native identities or UUIDs disagree, repeat or are unavailable")
            }
            Self::Memory => out.write_str("selected GPU has no host-visible memory for Venus"),
            Self::Allocation => out.write_str("GPU probe storage is unavailable"),
            Self::Unavailable => out.write_str("GPU probe capability has not been initialized"),
            Self::Pending => {
                out.write_str("GPU probe has no completed native result within the caller deadline")
            }
            Self::Properties => out.write_str("renderer GPU properties are malformed or unavailable"),
            Self::Scope => out.write_str("VMM GPU report differs from its prepared native scope"),
            Self::ReportTaken => out.write_str("VMM GPU report has already been transferred"),
            Self::Refused(why) => why.fmt(out),
            Self::Unreported => out.write_str("VMM GPU report writer closed with no report written"),
        }
    }
}
impl core::error::Error for GpuProbeError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    #[test]
    fn managed_report_binds_native_scope_and_exact_set_without_console_authority() {
        let physical = PhysicalGpu::MetalRegistry(7);
        let report = GpuReport {
            scope: [3; 16],
            probe: GpuProbe {
                devices: vec![ResolvedGpu {
                    physical,
                    uuid: [1; 16],
                    pci: None,
                    luid: Some(7u64.to_be_bytes()),
                    name: Some(GpuName::new("native renderer").unwrap()),
                    device_local_bytes: None,
                }],
                host_visible_bytes: 4096,
            },
        };
        let text = report.encode().unwrap();
        assert_eq!(GpuReport::parse(&text, [3; 16], &[physical]).unwrap(), report);
        assert_eq!(
            GpuReport::parse(&text, [4; 16], &[physical]),
            Err(GpuProbeError::Scope)
        );
        assert!(GpuReport::parse(&text, [0; 16], &[physical]).is_err());
        assert!(GpuReport::parse(&text, [3; 16], &[PhysicalGpu::MetalRegistry(8)]).is_err());
        assert!(GpuReport::parse(&report.probe.encode().unwrap(), [3; 16], &[physical]).is_err());
        assert!(text.len() <= report_bytes(1).unwrap());
    }

    #[test]
    fn managed_report_refuses_torn_hostile_or_appended_scope_frames() {
        let physical = PhysicalGpu::Pci {
            domain: 7,
            bus: 2,
            device: 1,
            function: 0,
        };
        let report = GpuReport {
            scope: [3; 16],
            probe: GpuProbe {
                devices: vec![ResolvedGpu {
                    physical,
                    uuid: [1; 16],
                    pci: Some(physical),
                    luid: None,
                    name: None,
                    device_local_bytes: Some(0),
                }],
                host_visible_bytes: 4096,
            },
        };
        let text = report.encode().unwrap();
        for end in 0..text.len() {
            assert!(GpuReport::parse(text.get(..end).unwrap(), [3; 16], &[physical]).is_err());
        }
        for bad in [
            text.replace("shards-vmm-gpu 1", "shards-vmm-gpu 2"),
            text.replace(&"03".repeat(16), &format!("+{}", "3".repeat(31))),
            text.replace(&"03".repeat(16), &"00".repeat(16)),
            text.replace("devices 1", &format!("devices {MAX_PHYSICAL_GPUS}")),
            format!("{text}{text}"),
            format!("{text}trailing\n"),
        ] {
            assert!(GpuReport::parse(&bad, [3; 16], &[physical]).is_err());
        }
        assert!(report_bytes(0).is_err());
        assert!(report_bytes(MAX_PHYSICAL_GPUS + 1).is_err());
    }

    #[test]
    fn completed_receipt_preserves_exact_identity_and_refuses_forgery_or_torn_output() {
        let native = PhysicalGpu::Pci {
            domain: u32::MAX,
            bus: 196,
            device: 0,
            function: 0,
        };
        let probe = GpuProbe {
            devices: vec![ResolvedGpu {
                physical: native,
                uuid: [7; 16],
                pci: Some(native),
                luid: Some(9u64.to_le_bytes()),
                name: None,
                device_local_bytes: None,
            }],
            host_visible_bytes: 4096,
        };
        let text = probe.encode().unwrap();
        assert_eq!(GpuProbe::parse(&text).unwrap(), probe);
        assert_eq!(
            probe.check(&[PhysicalGpu::Luid(196)]),
            Err(GpuProbeError::Identity)
        );
        for end in 0..text.len() {
            assert!(GpuProbe::parse(text.get(..end).unwrap()).is_err());
        }
        for bad in [
            text.replace("devices 1", "devices +1"),
            text.replace("devices 1", &format!("devices {}", MAX_PHYSICAL_GPUS + 1)),
            text.replace("host-visible 4096", "host-visible 0"),
            text.replace(
                "07070707070707070707070707070707",
                "00000000000000000000000000000000",
            ),
        ] {
            assert!(GpuProbe::parse(&bad).is_err());
        }
    }

    #[test]
    fn renderer_properties_keep_known_zero_and_unknown_distinct_and_legacy_unknown() {
        let physical = PhysicalGpu::Pci {
            domain: 7,
            bus: 2,
            device: 1,
            function: 0,
        };
        let mut probe = GpuProbe {
            devices: vec![ResolvedGpu {
                physical,
                uuid: [1; 16],
                pci: Some(physical),
                luid: None,
                name: Some(GpuName::new("actual renderer\tGPU\nα").unwrap()),
                device_local_bytes: Some(0),
            }],
            host_visible_bytes: 4096,
        };
        let encoded = probe.encode().unwrap();
        let parsed = GpuProbe::parse(&encoded).unwrap();
        assert_eq!(parsed, probe);
        assert_eq!(
            parsed.devices[0].name.unwrap().try_text().unwrap(),
            "actual renderer\tGPU\nα"
        );
        assert_eq!(parsed.devices[0].device_local_bytes, Some(0));
        probe.devices[0].name = None;
        probe.devices[0].device_local_bytes = None;
        let unknown = probe.encode().unwrap();
        assert_eq!(GpuProbe::parse(&unknown).unwrap(), probe);
        let old = unknown.replace(HEADER, IDENTITY_HEADER).replace("\t-\t-\n", "\n");
        assert_eq!(GpuProbe::parse(&old).unwrap(), probe);
        assert!(encoded.len() <= probe_bytes(1).unwrap());
    }

    /// D232: a receipt's byte bound is its longest encoding exactly: each device line with
    /// every field at its widest, and the device count as wide as `MAX_PHYSICAL_GPUS`, the
    /// widest count the bound allows for, which a smaller count's narrower digits fall short of.
    #[test]
    fn the_byte_bound_is_the_longest_encoding_of_its_count() {
        let widest = |i: u8| {
            let id = u64::MAX - u64::from(i);
            ResolvedGpu {
                physical: PhysicalGpu::MetalRegistry(id),
                uuid: [0xff - i; 16],
                pci: Some(PhysicalGpu::Pci {
                    domain: u32::MAX,
                    bus: u8::MAX,
                    device: 31,
                    function: 7,
                }),
                luid: Some(id.to_be_bytes()),
                name: Some(GpuName::new("widest").unwrap()),
                device_local_bytes: Some(u64::MAX),
            }
        };
        for count in [1u8, 10] {
            let probe = GpuProbe {
                devices: (0..count).map(widest).collect(),
                host_visible_bytes: u64::MAX,
            };
            let text = probe.encode().unwrap();
            let narrower = MAX_PHYSICAL_GPUS.to_string().len() - count.to_string().len();
            assert_eq!(text.len() + narrower, probe_bytes(usize::from(count)).unwrap());
        }
    }

    /// D202, D232: a driver's name is taken within its fixed array, as native-identity.c's
    /// `memchr` takes one before comparing it: up to its first NUL, the array's last byte
    /// included, and none where its driver left it unterminated, never the bytes past it.
    #[test]
    fn a_name_is_taken_within_its_fixed_array() {
        let wanted = b"VK_EXT_external_memory_host";
        let mut array = [0u8; GPU_NAME_BYTES];
        array[..wanted.len()].copy_from_slice(wanted);
        assert_eq!(
            fixed_name(&array).map(core::ffi::CStr::to_bytes),
            Some(&wanted[..])
        );
        let mut full = [b'x'; GPU_NAME_BYTES];
        full[GPU_NAME_BYTES - 1] = 0;
        assert_eq!(
            fixed_name(&full).map(|name| name.to_bytes().len()),
            Some(GPU_NAME_BYTES - 1)
        );
        // Unterminated, a NUL right past it in the same memory, where a read past it ends.
        let mut memory = [0u8; 2 * GPU_NAME_BYTES];
        memory[..GPU_NAME_BYTES].fill(b'x');
        assert_eq!(fixed_name(&memory[..GPU_NAME_BYTES]), None);
    }

    /// Every refusal a record says: each join reason, each selection reason among them.
    fn refusals() -> Vec<GpuRefusal> {
        let mut all = vec![
            GpuRefusal::NoHostMemory,
            GpuRefusal::Acknowledgment(0),
            GpuRefusal::Acknowledgment(u32::MAX),
        ];
        all.extend(
            [
                JoinError::Count,
                JoinError::Allocation,
                JoinError::Missing,
                JoinError::Ambiguous,
                JoinError::Aliased,
                JoinError::InvalidUuid,
                JoinError::InvalidNative,
                JoinError::Memory,
            ]
            .map(GpuRefusal::Join),
        );
        all.extend(
            [
                PhysicalGpuError::Syntax,
                PhysicalGpuError::PciAddress,
                PhysicalGpuError::MetalRegistry,
                PhysicalGpuError::Count,
                PhysicalGpuError::Duplicate,
                PhysicalGpuError::Allocation,
            ]
            .map(|why| GpuRefusal::Join(JoinError::Selection(why))),
        );
        all
    }

    /// D232: a VM process's refusal reaches its client as itself, as a probe's record or a
    /// report's frame, each reason its own, and a report's scope binds it as it binds a
    /// receipt; a report whose writer closed unwritten is no report, not a malformed one.
    #[test]
    fn a_refusal_reaches_its_client_typed() {
        assert_eq!(
            GpuRefusal::Join(JoinError::Missing).encode().unwrap(),
            "shards-gpu-refusal 1\njoin missing\n"
        );
        let selected = [PhysicalGpu::MetalRegistry(7)];
        for refusal in refusals() {
            let record = refusal.encode().unwrap();
            assert_eq!(GpuProbe::parse(&record), Err(GpuProbeError::Refused(refusal)));
            let frame = GpuReport::refusal([3; 16], refusal).unwrap();
            assert_eq!(frame, format!("shards-vmm-gpu 1 {}\n{record}", "03".repeat(16)));
            assert!(frame.len() <= report_bytes(1).unwrap());
            assert_eq!(
                GpuReport::parse(&frame, [3; 16], &selected),
                Err(GpuProbeError::Refused(refusal))
            );
            assert_eq!(
                GpuReport::parse(&frame, [4; 16], &selected),
                Err(GpuProbeError::Scope)
            );
        }
        assert_eq!(
            GpuReport::refusal([0; 16], GpuRefusal::NoHostMemory),
            Err(GpuProbeError::Scope)
        );
        assert_eq!(
            GpuReport::parse("", [3; 16], &selected),
            Err(GpuProbeError::Unreported)
        );
    }

    /// D232: a refusal torn, appended to or worded otherwise than its record is no refusal:
    /// malformed, as a receipt so changed is.
    #[test]
    fn a_torn_or_hostile_refusal_is_malformed() {
        let record = GpuRefusal::Join(JoinError::Selection(PhysicalGpuError::Duplicate))
            .encode()
            .unwrap();
        for end in 0..record.len() {
            assert_eq!(
                GpuProbe::parse(record.get(..end).unwrap()),
                Err(GpuProbeError::Syntax),
                "{end}"
            );
        }
        for reason in [
            "join",
            "join selection",
            "join missing extra",
            "join  missing",
            "join selection unknown",
            "join selection count extra",
            "Join missing",
            "unknown",
            "no-host-memory ",
            "acknowledgment",
            "acknowledgment 007",
            "acknowledgment +7",
            "acknowledgment -1",
            "acknowledgment 4294967296",
        ] {
            let text = format!("shards-gpu-refusal 1\n{reason}\n");
            assert_eq!(GpuProbe::parse(&text), Err(GpuProbeError::Syntax), "{reason}");
        }
        for text in [
            format!("{record}{record}"),
            format!("{record}\n"),
            record.replace("refusal 1", "refusal 2"),
        ] {
            assert_eq!(GpuProbe::parse(&text), Err(GpuProbeError::Syntax), "{text:?}");
        }
    }

    #[test]
    fn renderer_name_and_heap_receipt_reject_overflow_noncanonical_and_hostile_text() {
        assert!(GpuName::new(&"x".repeat(GPU_NAME_BYTES)).is_err());
        assert!(GpuName::new("a\0b").is_err());
        assert!(GpuName::from_buffer(&[b'x'; GPU_NAME_BYTES]).is_err());
        let mut bytes = [0; GPU_NAME_BYTES];
        bytes[0] = 0xff;
        assert!(GpuName::from_buffer(&bytes).is_err());
        let physical = PhysicalGpu::MetalRegistry(7);
        let probe = GpuProbe {
            devices: vec![ResolvedGpu {
                physical,
                uuid: [1; 16],
                pci: None,
                luid: Some(7u64.to_be_bytes()),
                name: Some(GpuName::new("actual").unwrap()),
                device_local_bytes: Some(u64::MAX),
            }],
            host_visible_bytes: 4096,
        };
        let encoded = probe.encode().unwrap();
        for bad in [
            encoded.replace("\t18446744073709551615\n", "\t18446744073709551616\n"),
            encoded.replace("\t18446744073709551615\n", "\t+1\n"),
            encoded.replace(&"00".repeat(GPU_NAME_BYTES - 6), &"01".repeat(GPU_NAME_BYTES - 6)),
            encoded.replace("61637475616c", "ff637475616c"),
        ] {
            assert!(GpuProbe::parse(&bad).is_err());
        }
        assert!(GpuName::new(&"x".repeat(GPU_NAME_BYTES - 1)).is_ok());
    }
}
