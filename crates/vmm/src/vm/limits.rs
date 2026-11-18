//! A host's real vCPU limit, as its own machines show it (D261, ROOT's ruling of
//! 2026-11-17). A hypervisor advertises the most vCPUs it takes (KVM_CAP_MAX_VCPUS, WHP's
//! CPUID 0x40000005, capped by the MADT), and some hosts enter fewer: EC2's nested KVM refused
//! the entry of a machine of 64 vCPUs or more on 4 CPUs (VM-instruction error 7), where Azure's
//! Hyper-V entered 255 (D261's experiment, run 44030858462). No count is chosen here: each
//! count's outcomes are kept per host model, the CPU's model and the hypervisor it runs under,
//! as its machines come to them ([`Limits::record`]). Every count is tried, the hypervisor's
//! own most the ceiling: EC2's Intel hosts refused even vCPU 0 of small machines at times, so
//! no refusal, early or not, turns into a lasting refusal (ROOT's decision of 2026-11-17). A
//! refusal is the host's ([`EntryRefused`]), said with what the records hold of its count and
//! the file that keeps them ([`Evidence`]). The tests size themselves by them: a count only
//! refused, and every larger one never run, is not asked of the host again
//! ([`Limits::admit`]), and its known-good count is ([`Limits::known_good`]).
//!
//! A host model's records are one file, read and written through
//! [`open_input`](crate::platform::open_input): in a VM process the one its spawner granted it
//! (D30), as its disks are, which it may neither replace by a rename nor, in App Sandbox, be
//! sure to cut short, so it is written over in place under its own lock; elsewhere the file at
//! its path.
//!
//! A test reads a refusal as its host's, and skips, only where D261's gate takes it for one
//! ([`the_hosts`], ROOT's ruling of 2026-11-17): VMX's VM-instruction error 7, on a host that is
//! itself a VM, with a witness. Either its guest had run, so its host had entered what it then
//! refused; or another VM process, whose guest had run, was refused within its host model's
//! burst width: the largest gap between consecutive refusals of one burst, never a burst's
//! whole span, which a long chain of close refusals would widen without end (ROOT's ruling of
//! 2026-11-17). The records keep each refusal's moment beside the counts, and the largest such
//! gap of the bursts they see ([`keep_refusal`]); the width is that, or the largest D261
//! measured (`limits/bursts.txt`) where that is larger. A burst's refusals each come within the
//! width of the one before, so no burst the records see widens it: only one measured wider
//! does. A machine refused before its guest ran, with no such refusal beside it, is what a
//! VMM's own error looks like, and its failure stands.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

/// The hypervisor a machine here runs under, as a refusal names it.
#[cfg(hv = "kvm")]
const HYPERVISOR: &str = "KVM";
#[cfg(hv = "hvf")]
const HYPERVISOR: &str = "Hypervisor.framework";
#[cfg(hv = "whp")]
const HYPERVISOR: &str = "WHP";
#[cfg(not(hv))]
const HYPERVISOR: &str = "no hypervisor";

/// A host model, which a count's outcomes are kept for: its CPU's model, and the hypervisor or
/// platform it runs under where it is itself a virtual machine (none on bare metal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostModel {
    pub cpu: String,
    pub under: Option<String>,
}

impl HostModel {
    /// This host's model: on x86-64 its CPU's brand string (CPUID 0x80000002-4) and, where
    /// CPUID says it runs under a hypervisor (leaf 1 ECX bit 31), that hypervisor's vendor:
    /// on Linux its platform's as its firmware names it (`/sys/class/dmi/id/sys_vendor`,
    /// "Amazon EC2", "Microsoft Corporation"), else its CPUID signature (leaf 0x40000000).
    /// Elsewhere what its OS names its CPU.
    pub fn here() -> HostModel {
        HostModel {
            cpu: cpu_model(),
            under: under(),
        }
    }

    /// Its records' file name: its words, each byte not a letter, digit, `.` or `-` an `_`,
    /// within a name's bounds. Also the host model D215's kept records of work are keyed by
    /// (the tests' kept ratings, the daemon's teardown C*): work counted on one CPU model is
    /// weighed against its own.
    pub fn key(&self) -> String {
        let whole = match &self.under {
            Some(under) => format!("{} under {under}", self.cpu),
            None => self.cpu.clone(),
        };
        whole
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(KEY_MOST)
            .collect()
    }
}

/// The most characters of a records file's name: within every file system's 255 bytes.
const KEY_MOST: usize = 200;

impl fmt::Display for HostModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.under {
            Some(under) => write!(f, "{} under {under}", self.cpu),
            None => f.write_str(&self.cpu),
        }
    }
}

/// The hypervisor here, under what it runs: "KVM under Amazon EC2".
fn hypervisor_under(host: &HostModel) -> String {
    match &host.under {
        Some(under) => format!("{HYPERVISOR} under {under}"),
        None => HYPERVISOR.to_string(),
    }
}

#[cfg(target_arch = "x86_64")]
fn cpu_model() -> String {
    use std::arch::x86_64::__cpuid;
    let top = __cpuid(0x8000_0000).eax;
    if top < 0x8000_0004 {
        return std::env::consts::ARCH.to_string();
    }
    // Three leaves of four registers of four bytes.
    let mut bytes = Vec::with_capacity(3 * 4 * 4);
    for leaf in 0x8000_0002..=0x8000_0004u32 {
        let r = __cpuid(leaf);
        for word in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    let brand = String::from_utf8_lossy(&bytes)
        .trim_matches(['\0', ' '])
        .to_string();
    if brand.is_empty() {
        std::env::consts::ARCH.to_string()
    } else {
        brand
    }
}

#[cfg(all(not(target_arch = "x86_64"), target_os = "macos"))]
fn cpu_model() -> String {
    sysctl_string(c"machdep.cpu.brand_string").unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

#[cfg(all(not(target_arch = "x86_64"), target_os = "linux"))]
fn cpu_model() -> String {
    // An arm64 Linux names its cores by implementer and part (arch/arm64/kernel/cpuinfo.c).
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |name: &str| {
        info.lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == name))
            .map(|(_, v)| v.trim().to_string())
    };
    match (field("CPU implementer"), field("CPU part")) {
        (Some(implementer), Some(part)) => format!("arm64 implementer {implementer} part {part}"),
        _ => std::env::consts::ARCH.to_string(),
    }
}

#[cfg(all(not(target_arch = "x86_64"), windows))]
fn cpu_model() -> String {
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| std::env::consts::ARCH.to_string())
}

#[cfg(all(
    not(target_arch = "x86_64"),
    not(any(target_os = "macos", target_os = "linux", windows))
))]
fn cpu_model() -> String {
    std::env::consts::ARCH.to_string()
}

#[cfg(target_arch = "x86_64")]
fn under() -> Option<String> {
    use std::arch::x86_64::__cpuid;
    const HYPERVISOR_PRESENT: u32 = 1 << 31;
    if __cpuid(1).ecx & HYPERVISOR_PRESENT == 0 {
        return None;
    }
    platform_vendor().or_else(|| {
        let r = __cpuid(0x4000_0000);
        // The vendor's three registers of four bytes.
        let mut bytes = Vec::with_capacity(3 * 4);
        for word in [r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let signature = String::from_utf8_lossy(&bytes)
            .trim_matches(['\0', ' '])
            .to_string();
        (!signature.is_empty()).then_some(signature)
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn under() -> Option<String> {
    platform_vendor()
}

/// The platform's vendor as its firmware names it, on Linux (SMBIOS through sysfs); none
/// elsewhere, and none where it names none.
fn platform_vendor() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/sys/class/dmi/id/sys_vendor")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(all(not(target_arch = "x86_64"), target_os = "macos"))]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut len: libc::size_t = 0;
    // SAFETY: a NUL-terminated name; no buffer, which asks for the value's length.
    let asked = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if asked != 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: a buffer of the length just read, which the call fills no further.
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buf.truncate(len);
    let text = String::from_utf8_lossy(&buf)
        .trim_matches(['\0', ' '])
        .to_string();
    (!text.is_empty()).then_some(text)
}

/// What machines of one count came to on a host model: how many ran, how many were refused
/// entry, and the vCPU the last refusal named.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcomes {
    pub ran: u64,
    pub refused: u64,
    pub last_refused_vcpu: Option<usize>,
}

/// What a machine came to, for its count's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It ran and ended for any reason but a refused entry.
    Ran,
    /// The host refused to enter vCPU `vcpu` ([`EntryRefused`]).
    Refused { vcpu: usize },
}

/// A refusal as D261's gate reads it ([`the_hosts`]), and as its host model's records keep it
/// beside the counts ([`keep_refusal`]): its moment, its VM process, and whether its guest had
/// run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// When it came, in microseconds since the Unix epoch: the clock every process of its
    /// host reads.
    pub at_us: u64,
    /// The VM process whose vCPU it refused.
    pub machine: u32,
    /// Whether its guest had run: its vCPU, or, where its machine was booted rather than
    /// restored, any vCPU of its machine, had made an exit only a guest's own instructions
    /// make, port or memory-mapped I/O (`vm::runtime`'s `Entered`). The VMM sets a booted
    /// machine's vCPUs' state before any runs, so its host had then entered that state.
    pub entered: bool,
}

impl Refusal {
    /// A refusal of this process's machine, now.
    pub fn now(entered: bool) -> Refusal {
        Refusal {
            at_us: now_us(),
            machine: std::process::id(),
            entered,
        }
    }
}

/// Now, in microseconds since the Unix epoch: the epoch itself where the clock reads before
/// it.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX))
}

/// A host model's records: its file in the records' directory, a line a count, `COUNT RAN
/// REFUSED LAST`, LAST the vCPU the last refusal named or `-`; beside them, a line a refusal a
/// burst may yet join, `refusal AT_US MACHINE ran|first`, and the largest gap between
/// consecutive refusals of one burst its host model has had, `gap GAP_US` (D261's gate,
/// [`keep_refusal`]).
#[derive(Debug, Clone)]
pub struct Limits {
    file: PathBuf,
    host: HostModel,
    /// Whether what D261 measured of its host model is read beside them
    /// ([`measured_at`](Self::measured_at)).
    measured: bool,
}

impl Limits {
    /// The records of host model `host` kept in `dir`.
    pub fn at(dir: PathBuf, host: HostModel) -> Limits {
        Limits {
            file: dir.join(host.key()),
            host,
            measured: false,
        }
    }

    /// [`at`](Self::at), what D261 measured of its host model read beside its records
    /// (`limits/vcpus.txt`): the tests' sizing, which a cold host model
    /// (its records' cache missed, or a save that failed) sizes by what was measured there,
    /// never by a first boot of its most vCPUs. The product's records are its file's alone.
    pub fn measured_at(dir: PathBuf, host: HostModel) -> Limits {
        Limits {
            measured: true,
            ..Limits::at(dir, host)
        }
    }

    /// The records kept in `file`, of host model `host`: a refusal's, as its evidence names
    /// them ([`Evidence::file`]), read back by the tests ([`Said::file`]).
    pub fn kept_in(file: PathBuf, host: HostModel) -> Limits {
        Limits {
            file,
            host,
            measured: false,
        }
    }

    /// The host model they are of.
    pub fn host(&self) -> &HostModel {
        &self.host
    }

    /// The file they are kept in: what a VM process is granted (D30).
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Their text as kept: none where nothing is. Read under the file's lock, shared, so that
    /// a write is read whole.
    fn kept(&self) -> String {
        let read = || -> io::Result<Vec<u8>> {
            let mut file = crate::platform::open_input(&self.file, false)?;
            file.lock_shared()?;
            let mut bytes = Vec::new();
            let read = file.read_to_end(&mut bytes);
            let unlocked = file.unlock();
            read?;
            unlocked?;
            Ok(bytes)
        };
        String::from_utf8_lossy(&read().unwrap_or_default()).into_owned()
    }

    /// Each count's outcomes as kept: none where nothing is, a line that says nothing whole
    /// left out; and, read [`measured_at`](Self::measured_at), what D261 measured of each
    /// count on its host model added to them.
    pub fn outcomes(&self) -> BTreeMap<u32, Outcomes> {
        let mut all = parse(&self.kept());
        if self.measured {
            for (count, seen) in measured_counts(&self.host.key()) {
                let o = all.entry(count).or_default();
                o.ran = o.ran.saturating_add(seen.ran);
                o.refused = o.refused.saturating_add(seen.refused);
                o.last_refused_vcpu = o.last_refused_vcpu.or(seen.last_refused_vcpu);
            }
        }
        all
    }

    /// The refusals kept beside the counts: those a burst may yet join ([`keep_refusal`]).
    pub fn refusals(&self) -> Vec<Refusal> {
        refusals_in(&self.kept())
    }

    /// This host model's burst width, in microseconds: the largest gap between consecutive
    /// refusals of one burst its records keep, or the largest D261 measured where that is
    /// larger ([`measured_width_us`]).
    pub fn burst_width_us(&self) -> u64 {
        width_us(&self.kept())
    }

    /// Whether the tests ask a machine of `vcpus` of this host model: not where it has never run
    /// there and a count no larger has only been refused there ([`OnlyRefused`]). A count that
    /// has run is asked, whatever else it and others came to. The product asks every count.
    pub fn admit(&self, vcpus: u32) -> Result<(), OnlyRefused> {
        let all = self.outcomes();
        if all.get(&vcpus).is_some_and(|o| o.ran > 0) {
            return Ok(());
        }
        match all.into_iter().find(|(_, o)| o.refused > 0 && o.ran == 0) {
            Some((from, o)) if vcpus >= from => Err(OnlyRefused {
                vcpus,
                from,
                refused: o.refused,
                vcpu: o.last_refused_vcpu,
                host: self.host.clone(),
                file: self.file.clone(),
                measured: self.measured,
            }),
            _ => Ok(()),
        }
    }

    /// The largest count this host model has run and never refused: what it is known to
    /// enter. None before one has.
    pub fn known_good(&self) -> Option<u32> {
        self.outcomes()
            .into_iter()
            .rev()
            .find(|(_, o)| o.ran > 0 && o.refused == 0)
            .map(|(count, _)| count)
    }

    /// Keeps a machine of `vcpus` vCPUs' outcome ([`keep`]).
    pub fn record(&self, vcpus: u32, outcome: Outcome) -> io::Result<()> {
        keep(self.opened()?, vcpus, outcome).map(|_| ())
    }

    /// Keeps a machine of `vcpus` vCPUs' refusal at vCPU `vcpu`, and `refusal` beside the
    /// counts for D261's gate ([`keep_refusal`]): a machine the tests boot in their own
    /// process, which keeps its outcome only where they keep it.
    pub fn record_refusal(&self, vcpus: u32, vcpu: usize, refusal: Refusal) -> io::Result<()> {
        keep_refusal(self.opened()?, vcpus, vcpu, refusal).map(|_| ())
    }

    /// What `all`, these records as kept, say of a machine of `vcpus` ([`Evidence`]).
    pub fn evidence(&self, vcpus: u32, all: &BTreeMap<u32, Outcomes>) -> Evidence {
        Evidence {
            vcpus,
            outcomes: all.get(&vcpus).copied().unwrap_or_default(),
            fewest_only_refused: all
                .iter()
                .find(|(_, o)| o.refused > 0 && o.ran == 0)
                .map(|(count, _)| *count),
            cpu: self.host.cpu.clone(),
            file: self.file.clone(),
        }
    }

    /// The records' file, to be read and written: the one granted where it was (a Linux VM
    /// process's own, held), else the file at its path, made with its directory where it is not
    /// there.
    pub fn opened(&self) -> io::Result<std::fs::File> {
        match crate::platform::open_input(&self.file, true) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => crate::platform::open_made(&self.file),
            opened => opened,
        }
    }

    /// Holds the records' file open from now on, to be read and written, made where it is not
    /// there, as granted (`platform::grant_input`): a VM process on Linux, before Landlock
    /// confines it (D30), which then gives it no rule for the file. It reaches its records
    /// through this alone, and not once it lets go of them ([`let_go`](Self::let_go)).
    #[cfg(target_os = "linux")]
    pub fn hold(&self) -> io::Result<()> {
        let file = crate::platform::open_made(&self.file)?;
        crate::platform::grant_input(self.file.clone(), Some(file.into()));
        Ok(())
    }

    /// Lets go of the records' file this process was granted, or held (Linux): from here on it
    /// reaches them only by their path, as a confined VM process may not.
    pub fn let_go(&self) {
        crate::platform::forget_input(&self.file);
    }
}

/// Keeps a machine of `vcpus` vCPUs' `outcome` in `file`, its records' ([`Limits::opened`]):
/// under its lock, what other processes kept since read in and its count's line made or
/// changed, written over the file from its start, padded with empty lines where it is shorter
/// than it was. Its holder may neither rename another file over it nor be sure to cut it short
/// (App Sandbox), and a line it shortens leaves no line of its tail behind to be read. Returns
/// every count's outcomes as kept now.
pub fn keep(file: std::fs::File, vcpus: u32, outcome: Outcome) -> io::Result<BTreeMap<u32, Outcomes>> {
    locked(file, vcpus, outcome, None)
}

/// Keeps a machine of `vcpus` vCPUs' refusal at vCPU `vcpu` as [`keep`] does, and `refusal`
/// beside the counts, for D261's gate ([`the_hosts`]): the refusals a burst may yet join
/// (`joined`), and the largest gap between consecutive refusals of one burst its host model
/// has had.
pub fn keep_refusal(
    file: std::fs::File,
    vcpus: u32,
    vcpu: usize,
    refusal: Refusal,
) -> io::Result<BTreeMap<u32, Outcomes>> {
    locked(file, vcpus, Outcome::Refused { vcpu }, Some(refusal))
}

/// [`keep`] and [`keep_refusal`]: [`rewrite`] under the file's lock.
fn locked(
    mut file: std::fs::File,
    vcpus: u32,
    outcome: Outcome,
    refusal: Option<Refusal>,
) -> io::Result<BTreeMap<u32, Outcomes>> {
    file.lock()?;
    let kept = rewrite(&mut file, vcpus, outcome, refusal);
    let unlocked = file.unlock();
    let all = kept?;
    unlocked?;
    Ok(all)
}

/// [`keep`]'s write, under its lock: the refusals and the largest gap kept beside the counts
/// written back, `refusal` joined to them where there is one.
fn rewrite(
    file: &mut std::fs::File,
    vcpus: u32,
    outcome: Outcome,
    refusal: Option<Refusal>,
) -> io::Result<BTreeMap<u32, Outcomes>> {
    let mut was = Vec::new();
    file.read_to_end(&mut was)?;
    let was_text = String::from_utf8_lossy(&was);
    let mut all = parse(&was_text);
    let mut refusals = refusals_in(&was_text);
    let mut gap = gap_in(&was_text);
    let o = all.entry(vcpus).or_default();
    match outcome {
        Outcome::Ran => o.ran = o.ran.saturating_add(1),
        Outcome::Refused { vcpu } => {
            o.refused = o.refused.saturating_add(1);
            o.last_refused_vcpu = Some(vcpu);
        }
    }
    if let Some(refusal) = refusal {
        let (kept, burst) = joined(&refusals, refusal, width_us(&was_text));
        refusals = kept;
        gap = gap.max(burst);
    }
    let mut now = text(&all);
    now.push_str(&refusals_text(&refusals, gap));
    now.extend(std::iter::repeat_n('\n', was.len().saturating_sub(now.len())));
    file.seek(io::SeekFrom::Start(0))?;
    file.write_all(now.as_bytes())?;
    Ok(all)
}

/// Records' text: a line a count, in order.
fn text(all: &BTreeMap<u32, Outcomes>) -> String {
    all.iter()
        .map(|(count, o)| {
            let last = o
                .last_refused_vcpu
                .map_or_else(|| "-".to_string(), |v| v.to_string());
            format!("{count} {} {} {last}\n", o.ran, o.refused)
        })
        .collect()
}

/// What records' text says: each whole line, a line that says nothing whole left out.
fn parse(text: &str) -> BTreeMap<u32, Outcomes> {
    let mut all = BTreeMap::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(count), Some(ran), Some(refused), Some(last), None) = (
            words.next(),
            words.next(),
            words.next(),
            words.next(),
            words.next(),
        ) else {
            continue;
        };
        let (Ok(count), Ok(ran), Ok(refused)) =
            (count.parse::<u32>(), ran.parse::<u64>(), refused.parse::<u64>())
        else {
            continue;
        };
        let last_refused_vcpu = match last {
            "-" => None,
            n => match n.parse::<usize>() {
                Ok(v) => Some(v),
                Err(_) => continue,
            },
        };
        all.insert(
            count,
            Outcomes {
                ran,
                refused,
                last_refused_vcpu,
            },
        );
    }
    all
}

/// The first word of a refusal's line in records' text, and of a burst's largest gap's.
const REFUSAL: &str = "refusal";
const GAP: &str = "gap";
/// A refusal's last word: its guest had run, or not.
const RAN: &str = "ran";
const FIRST: &str = "first";

/// The refusals records' text keeps: each whole `refusal AT_US MACHINE ran|first` line.
fn refusals_in(text: &str) -> Vec<Refusal> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != REFUSAL {
                return None;
            }
            let at_us = words.next()?.parse::<u64>().ok()?;
            let machine = words.next()?.parse::<u32>().ok()?;
            let entered = match words.next()? {
                RAN => true,
                FIRST => false,
                _ => return None,
            };
            words.next().is_none().then_some(Refusal {
                at_us,
                machine,
                entered,
            })
        })
        .collect()
}

/// The largest gap `text` keeps, in microseconds: its largest `gap GAP_US` line's, words past
/// it (where a measured burst was measured) aside. None where it keeps none.
fn gap_in(text: &str) -> Option<u64> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != GAP {
                return None;
            }
            words.next()?.parse::<u64>().ok()
        })
        .max()
}

/// The refusals and the largest gap kept beside the counts, as records' text.
fn refusals_text(refusals: &[Refusal], gap: Option<u64>) -> String {
    let mut text: String = refusals
        .iter()
        .map(|r| {
            let ran = if r.entered { RAN } else { FIRST };
            format!("{REFUSAL} {} {} {ran}\n", r.at_us, r.machine)
        })
        .collect();
    if let Some(gap) = gap {
        text.push_str(&format!("{GAP} {gap}\n"));
    }
    text
}

/// The bursts of refused entries D261 measured on EC2's Intel runners, as `gap GAP_US` lines,
/// each burst's largest gap between consecutive refusals, followed by where each was measured:
/// every host model's first burst width.
const MEASURED: &str = include_str!("limits/bursts.txt");

/// What D261's experiment measured of each count on a host model (`limits/vcpus.txt`): the
/// tests' sizing reads it beside the host model's records ([`Limits::measured_at`]), as a
/// burst's width reads `limits/bursts.txt`'s.
const MEASURED_COUNTS: &str = include_str!("limits/vcpus.txt");

/// What `limits/vcpus.txt` says of the counts of the host model whose records' file name is
/// `key` ([`HostModel::key`]): each line its key, a count's `COUNT RAN REFUSED LAST` as records
/// keep it, and where it was measured.
fn measured_counts(key: &str) -> BTreeMap<u32, Outcomes> {
    let lines: String = MEASURED_COUNTS
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(key)).then(|| words.take(4).collect::<Vec<_>>().join(" ") + "\n")
        })
        .collect();
    parse(&lines)
}

/// The largest gap between consecutive refused entries of one burst D261 measured, in
/// microseconds (`limits/bursts.txt`): a host model's burst width, unless its records keep a
/// larger one. None measured, none wide.
pub fn measured_width_us() -> u64 {
    gap_in(MEASURED).unwrap_or(0)
}

/// A host model's burst width, as records' `text` keep it: the largest gap of one burst they
/// keep, or the largest D261 measured where that is larger.
fn width_us(text: &str) -> u64 {
    gap_in(text).map_or_else(measured_width_us, |kept| kept.max(measured_width_us()))
}

/// The refusals kept once `new` joins `kept`, and the burst it then belongs to: `kept` and
/// `new` in order of their moments, in chains whose every refusal is within `width_us` of the
/// one before. The chain `new` joined and every later one are kept; those before it are
/// further than the width from every refusal to come, and are let go of. With them, that
/// chain's largest gap between consecutive refusals, where it is a burst ([`burst_gap`]).
fn joined(kept: &[Refusal], new: Refusal, width_us: u64) -> (Vec<Refusal>, Option<u64>) {
    let mut all = kept.to_vec();
    all.push(new);
    all.sort_by_key(|r| r.at_us);
    let mut chains: Vec<Vec<Refusal>> = Vec::new();
    for r in all {
        let joins = chains
            .last()
            .and_then(|chain| chain.last())
            .is_some_and(|last| r.at_us.abs_diff(last.at_us) <= width_us);
        if joins && let Some(chain) = chains.last_mut() {
            chain.push(r);
        } else {
            chains.push(vec![r]);
        }
    }
    let Some(at) = chains.iter().position(|chain| chain.contains(&new)) else {
        return (vec![new], None);
    };
    let gap = chains.get(at).and_then(|chain| burst_gap(chain.as_slice()));
    (chains.into_iter().skip(at).flatten().collect(), gap)
}

/// The largest gap between consecutive refusals of a chain, in order of their moments, where
/// it is a burst of its host's: refusals of two VM processes or more, one of whose guests had
/// run. Never its whole span, which a long chain of close refusals would widen without end
/// (ROOT's ruling of 2026-11-17). None where it is one machine's, or none of its guests had
/// run, as a VMM's own error's would be.
fn burst_gap(chain: &[Refusal]) -> Option<u64> {
    let machines = chain
        .iter()
        .map(|r| r.machine)
        .collect::<std::collections::BTreeSet<_>>();
    if machines.len() < 2 || !chain.iter().any(|r| r.entered) {
        return None;
    }
    chain
        .windows(2)
        .filter_map(|pair| match pair {
            [before, after] => Some(after.at_us.saturating_sub(before.at_us)),
            _ => None,
        })
        .max()
}

/// A machine whose vCPU its host's hypervisor refused to enter (KVM_EXIT_FAIL_ENTRY's hardware
/// reason, VMX's VM-instruction error): the host's, never the guest's (D261). With what its host
/// model's records hold of its count, this refusal kept, where its process keeps them (a VM
/// process's, its user's cache's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRefused {
    pub vcpu: usize,
    pub reason: u64,
    /// Its moment, its VM process, and whether its guest had run: what D261's gate reads of
    /// it ([`the_hosts`]).
    pub refusal: Refusal,
    pub host: HostModel,
    pub evidence: Option<Evidence>,
}

/// What a host model's records hold of a count as its host refuses a machine of it (D261): its
/// outcomes there, the refusal kept; the fewest vCPUs that host model has only refused; and the
/// file that keeps them, which its user may remove where the host has changed. Said with the
/// refusal; nothing is refused for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub vcpus: u32,
    pub outcomes: Outcomes,
    pub fewest_only_refused: Option<u32>,
    pub cpu: String,
    pub file: PathBuf,
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "on this host model ({}), machines of {} vCPUs have run {} times and been refused {} times",
            self.cpu, self.vcpus, self.outcomes.ran, self.outcomes.refused
        )?;
        if let Some(fewest) = self.fewest_only_refused {
            write!(f, ", the fewest vCPUs it has only refused {fewest}")?;
        }
        write!(f, " (kept in {})", self.file.display())
    }
}

impl fmt::Display for EntryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vCPU {}{FAILED}{:#x}{BY}{}{REFUSED}{}, at {} s, {MACHINE}{})",
            self.vcpu,
            self.reason,
            hypervisor_under(&self.host),
            if self.refusal.entered {
                AFTER_IT_RAN
            } else {
                BEFORE_IT_RAN
            },
            seconds(self.refusal.at_us),
            self.refusal.machine
        )?;
        match &self.evidence {
            Some(evidence) => write!(f, "; {evidence}"),
            None => Ok(()),
        }
    }
}

/// [`EntryRefused`]'s words, which [`Said`] reads back:
/// `vCPU N: VM entry failed (hardware reason 0xR): the host's hypervisor (H) refused to enter
/// it (before its guest ran, at S.MICROS s, VM process P)`, then its evidence.
const FAILED: &str = ": VM entry failed (hardware reason ";
const BY: &str = "): the host's hypervisor (";
const REFUSED: &str = ") refused to enter it (";
const AFTER_IT_RAN: &str = "after its guest ran";
const BEFORE_IT_RAN: &str = "before its guest ran";
const MACHINE: &str = "VM process ";
const KEPT_IN: &str = " (kept in ";

/// `us` microseconds as seconds, to the microsecond.
fn seconds(us: u64) -> String {
    format!("{}.{:06}", us / 1_000_000, us % 1_000_000)
}

/// A refusal as a VM process said it ([`EntryRefused`]'s words), read back from what a run
/// printed or a daemon's log: what D261's gate reads of it ([`the_hosts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// Its words, from its `vCPU` on.
    pub line: String,
    pub vcpu: usize,
    pub reason: u64,
    /// Whether its reason is VMX's VM-instruction error: it was said on an x86-64 host, as the
    /// tests read it on the host whose VMs said it ([`VMX_INVALID_CONTROL_FIELD`]).
    pub vmx: bool,
    /// Whether its host is itself a VM: its hypervisor said as running under another.
    pub nested: bool,
    pub refusal: Refusal,
    /// The records its VM process kept it in, as its evidence names them: none where it kept
    /// none (a restore's).
    pub file: Option<PathBuf>,
}

impl Said {
    /// The last refusal `text` tells of in [`EntryRefused`]'s words: none where it tells of
    /// none, or only in words that name no moment (a VM process's of before D261's gate).
    pub fn find(text: &str) -> Option<Said> {
        text.lines().filter_map(Said::read).next_back()
    }

    /// The refusal `line` tells of.
    fn read(line: &str) -> Option<Said> {
        let (head, rest) = line.split_once(FAILED)?;
        let (before, vcpu) = head.rsplit_once("vCPU ")?;
        let vcpu = vcpu.parse::<usize>().ok()?;
        let (reason, rest) = rest.split_once(BY)?;
        let reason = u64::from_str_radix(reason.strip_prefix("0x")?, 16).ok()?;
        let (hypervisor, rest) = rest.split_once(REFUSED)?;
        let (facts, rest) = rest.split_once(')')?;
        let mut facts = facts.split(", ");
        let entered = match facts.next()? {
            AFTER_IT_RAN => true,
            BEFORE_IT_RAN => false,
            _ => return None,
        };
        let (secs, micros) = facts
            .next()?
            .strip_prefix("at ")?
            .strip_suffix(" s")?
            .split_once('.')?;
        let at_us = secs
            .parse::<u64>()
            .ok()?
            .checked_mul(1_000_000)?
            .checked_add(micros.parse::<u64>().ok()?)?;
        let machine = facts.next()?.strip_prefix(MACHINE)?.parse::<u32>().ok()?;
        if facts.next().is_some() {
            return None;
        }
        let file = rest
            .rsplit_once(KEPT_IN)
            .and_then(|(_, file)| file.strip_suffix(')'))
            .map(PathBuf::from);
        Some(Said {
            line: line.get(before.len()..).unwrap_or(line).to_string(),
            vcpu,
            reason,
            vmx: cfg!(target_arch = "x86_64"),
            nested: hypervisor.contains(" under "),
            refusal: Refusal {
                at_us,
                machine,
                entered,
            },
            file,
        })
    }
}

/// VMX's VM-instruction error 7, "VM entry with invalid control field(s)" (Intel SDM Vol. 3C,
/// 31.4; Linux arch/x86/include/asm/vmx.h, VMXERR_ENTRY_INVALID_CONTROL_FIELD): what KVM says a
/// VMX entry's VMfailValid with (vmx.c, `vmx_handle_exit`: VM_INSTRUCTION_ERROR), and what an
/// outer KVM says to the KVM inside it both of controls its checks refuse (nested.c,
/// `nested_vmx_run`) and of its own entry of the nested guest that its hardware refused
/// (`__nested_vmx_vmexit`: "After an early L2 VM-entry failure ... nested_vmx_fail(vcpu,
/// VMXERR_ENTRY_INVALID_CONTROL_FIELD)"; Linux 7.4-rc3). A VMX entry's failure for its guest's
/// state is said with its exit reason, bit 31 set, and SVM's with its exit code, SVM_EXIT_ERR
/// (svm.c, `svm_is_vmrun_failure`): an x86-64 host's 7 is VMX's error 7 alone.
pub const VMX_INVALID_CONTROL_FIELD: u64 = 7;

/// Why D261's gate takes a refusal for its host's ([`the_hosts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Witness {
    /// Its guest had run: its host had entered the state it then refused.
    Ran,
    /// VM process `machine`, whose guest had run, was refused `apart_us` from it, within its
    /// host model's burst width, `width_us`.
    Burst {
        machine: u32,
        apart_us: u64,
        width_us: u64,
    },
}

impl fmt::Display for Witness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ran => f.write_str("its guest had run, so its host had entered the state it refused"),
            Self::Burst {
                machine,
                apart_us,
                width_us,
            } => write!(
                f,
                "{MACHINE}{machine}, whose guest had run, was refused {} s from it, within this host \
                 model's burst width of {} s",
                seconds(*apart_us),
                seconds(*width_us)
            ),
        }
    }
}

/// Why D261's gate does not take a refusal for its host's: its failure stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotTheHosts {
    /// Its reason is not VMX's error 7.
    Reason(u64),
    /// Its host is not itself a VM.
    NotNested,
    /// Its guest had not run, and no VM process whose guest had run was refused within the
    /// burst width, `width_us`, of it: what a VMM's own error looks like.
    Alone { width_us: u64 },
}

impl fmt::Display for NotTheHosts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reason(reason) => write!(
                f,
                "hardware reason {reason:#x} is not VMX's error 7 said on an x86-64 host"
            ),
            Self::NotNested => f.write_str("its host is not itself a VM"),
            Self::Alone { width_us } => write!(
                f,
                "its guest had not run, and no VM process whose guest had run was refused within this \
                 host model's burst width of {} s of it",
                seconds(*width_us)
            ),
        }
    }
}

/// Whether D261's gate takes a refusal for its host's (ROOT's ruling of 2026-11-17), with its
/// witness: VMX's error 7 ([`VMX_INVALID_CONTROL_FIELD`]), on a host that is itself a VM, where
/// its guest had run ([`Refusal::entered`]), or another VM process whose guest had run was
/// refused within `width_us` of it among `kept`, its host model's refusals
/// ([`Limits::refusals`]). A machine refused before its guest ran with no such refusal beside
/// it, as a VMM's own error refuses one, every run, wherever it runs, is not taken. Nor is a
/// refusal witnessed only by others refused before their guests ran: a VMM's error that refuses
/// every machine's first entry would refuse many at once.
pub fn the_hosts(said: &Said, kept: &[Refusal], width_us: u64) -> Result<Witness, NotTheHosts> {
    if !said.vmx || said.reason != VMX_INVALID_CONTROL_FIELD {
        return Err(NotTheHosts::Reason(said.reason));
    }
    if !said.nested {
        return Err(NotTheHosts::NotNested);
    }
    if said.refusal.entered {
        return Ok(Witness::Ran);
    }
    kept.iter()
        .filter(|other| other.machine != said.refusal.machine && other.entered)
        .map(|other| (other.machine, other.at_us.abs_diff(said.refusal.at_us)))
        .filter(|&(_, apart_us)| apart_us <= width_us)
        .min_by_key(|&(_, apart_us)| apart_us)
        .map(|(machine, apart_us)| Witness::Burst {
            machine,
            apart_us,
            width_us,
        })
        .ok_or(NotTheHosts::Alone { width_us })
}

/// Refusals D261's gate took for their host's, every one of a span of time
/// ([`the_hosts_all`]): how many, the first and the last, and the first's witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsRefusals {
    pub count: usize,
    pub first: Said,
    pub last: Said,
    pub witness: Witness,
}

impl fmt::Display for HostsRefusals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} refused, the first at {} s ({MACHINE}{}), the last at {} s ({MACHINE}{}); the first's \
             witness: {}",
            self.count,
            seconds(self.first.refusal.at_us),
            self.first.refusal.machine,
            seconds(self.last.refusal.at_us),
            self.last.refusal.machine,
            self.witness
        )
    }
}

/// Whether D261's gate takes every refusal of `said` for its host's ([`the_hosts`]), each with
/// `kept`, the refusals its witnesses may be among, and `width_us`, its host model's burst
/// width: what a span of time its host refused entries in comes to, a bench's rate's (ROOT's
/// ruling of 2026-11-18, CI 44245219858). A refusal said twice, as its VM process's own line
/// and as its daemon's log says it, counts once. Those it took, together, where it took every
/// one, none where `said` holds none; else each it did not take, with why: their failures
/// stand, and the span is not its host's.
pub fn the_hosts_all(
    said: &[Said],
    kept: &[Refusal],
    width_us: u64,
) -> Result<Option<HostsRefusals>, Vec<(Said, NotTheHosts)>> {
    let each: BTreeMap<(u64, u32), &Said> = said
        .iter()
        .map(|s| ((s.refusal.at_us, s.refusal.machine), s))
        .collect();
    let (mut taken, mut not) = (Vec::new(), Vec::new());
    for s in each.into_values() {
        match the_hosts(s, kept, width_us) {
            Ok(witness) => taken.push((s, witness)),
            Err(why) => not.push((s.clone(), why)),
        }
    }
    if !not.is_empty() {
        return Err(not);
    }
    let (Some(&(first, witness)), Some(&(last, _))) = (taken.first(), taken.last()) else {
        return Ok(None);
    };
    Ok(Some(HostsRefusals {
        count: taken.len(),
        first: first.clone(),
        last: last.clone(),
        witness,
    }))
}

/// A machine the tests do not ask of their host ([`Limits::admit`]): its count, and the least
/// count its host model has only refused, how often, the vCPU the last refusal named, and the
/// file that keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlyRefused {
    pub vcpus: u32,
    pub from: u32,
    pub refused: u64,
    pub vcpu: Option<usize>,
    pub host: HostModel,
    pub file: PathBuf,
    /// Whether D261's measurements of its host model were read beside its file
    /// ([`Limits::measured_at`]).
    pub measured: bool,
}

impl fmt::Display for OnlyRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let which = self
            .vcpu
            .map_or_else(|| "a vCPU".to_string(), |v| format!("vCPU {v}"));
        let measured = if self.measured {
            ", beside D261's measurements of this host model (vm/limits/vcpus.txt)"
        } else {
            ""
        };
        write!(
            f,
            "{} vCPUs: the host's hypervisor ({}) refused to enter {which}; on this host model \
             ({}), counts \u{2265} {} have only been refused ({} times), so it is not tried (kept in \
             {}{measured})",
            self.vcpus,
            hypervisor_under(&self.host),
            self.host.cpu,
            self.from,
            self.refused,
            self.file.display()
        )
    }
}

/// Why a machine's count of vCPUs is refused before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcpusRefused {
    /// None asked.
    None,
    /// More than its host's hypervisor takes in one partition.
    PastPartition { vcpus: u32, partition: u32 },
    /// More than its host has logical processors, where the host's Hyper-V is nested within
    /// another Hyper-V's partition (D261, ROOT's ruling of 2026-11-17): every vCPU past them is
    /// one with no host CPU, which each cross-CPU wait of its kernel waits on (PM M379).
    PastProcessors {
        vcpus: u32,
        processors: u32,
        partition: u32,
    },
}

impl fmt::Display for VcpusRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("at least one vCPU is required"),
            Self::PastPartition { vcpus, partition } => {
                write!(f, "{vcpus} vCPUs requested; this host supports {partition}")
            }
            Self::PastProcessors {
                vcpus,
                processors,
                partition,
            } => write!(
                f,
                "{vcpus} vCPUs: this host's Hyper-V is nested within another Hyper-V's partition, where \
                 a microVM has at most the host's {processors} logical processors (its partitions take \
                 {partition}): each vCPU past them is one with no host CPU, which every cross-CPU wait \
                 of its kernel waits on (D261; PM M379; WHP experiment 44127161851 on 4 logical \
                 processors: 7 CPUs brought online in about 1 s, 14 in 26 s, 29 in 41 min)"
            ),
        }
    }
}

/// The most vCPUs a machine may have on a host whose hypervisor takes `partition` in one, and
/// whose logical processors are `nested`, where its Hyper-V is nested within another Hyper-V's
/// partition (none elsewhere): the fewer of the two there, the partition's elsewhere.
pub fn most_vcpus(partition: u32, nested: Option<u32>) -> u32 {
    nested.map_or(partition, |processors| partition.min(processors))
}

/// Whether a machine of `vcpus` may start on such a host ([`most_vcpus`]), and why not.
pub fn vcpus_within(vcpus: u32, partition: u32, nested: Option<u32>) -> Result<(), VcpusRefused> {
    if vcpus == 0 {
        return Err(VcpusRefused::None);
    }
    if let Some(processors) = nested
        && vcpus > processors
        && processors < partition
    {
        return Err(VcpusRefused::PastProcessors {
            vcpus,
            processors,
            partition,
        });
    }
    if vcpus > partition {
        return Err(VcpusRefused::PastPartition { vcpus, partition });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ec2() -> HostModel {
        HostModel {
            cpu: "Intel(R) Xeon(R) 6975P-C".into(),
            under: Some("Amazon EC2".into()),
        }
    }

    /// D261's outcomes on EC2's Intel runners: what runs is tried, a count only refused, and
    /// every larger one never run, refused at once with the evidence and where it is kept; a
    /// count that ran is tried, refused too or past one only refused; and the known-good count
    /// is the largest never refused (run 44030858462).
    #[test]
    fn a_count_only_refused_and_every_larger_is_refused_at_once() {
        let dir = shards_testdir::TempDir::new("limits").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        assert!(limits.admit(255).is_ok(), "cold: tried");
        assert_eq!(limits.known_good(), None);
        for _ in 0..3 {
            limits.record(32, Outcome::Ran).unwrap();
            limits.record(64, Outcome::Refused { vcpu: 13 }).unwrap();
        }
        limits.record(128, Outcome::Ran).unwrap();
        limits.record(128, Outcome::Refused { vcpu: 57 }).unwrap();
        limits.record(128, Outcome::Refused { vcpu: 75 }).unwrap();
        assert!(limits.admit(32).is_ok());
        assert!(
            limits.admit(63).is_ok(),
            "below the least count only refused: tried"
        );
        let refused = limits.admit(64).unwrap_err();
        assert_eq!((refused.from, refused.refused, refused.vcpu), (64, 3, Some(13)));
        assert_eq!(
            refused.to_string(),
            "64 vCPUs: the host's hypervisor (".to_string()
                + &hypervisor_under(&ec2())
                + ") refused to enter vCPU 13; on this host model (Intel(R) Xeon(R) 6975P-C), \
                   counts \u{2265} 64 have only been refused (3 times), so it is not tried (kept in "
                + &limits.file().display().to_string()
                + ")"
        );
        for larger in [96, 255] {
            assert_eq!(
                limits.admit(larger).unwrap_err().from,
                64,
                "{larger}: larger, never run: refused at once too"
            );
        }
        assert!(
            limits.admit(128).is_ok(),
            "a count that ran is tried, past one only refused"
        );
        assert_eq!(limits.known_good(), Some(32));
        let outcomes = limits.outcomes();
        assert_eq!(
            outcomes.get(&128),
            Some(&Outcomes {
                ran: 1,
                refused: 2,
                last_refused_vcpu: Some(75)
            })
        );
    }

    /// D261's measurements size a cold host model's tests (`Limits::measured_at`,
    /// `limits/vcpus.txt`): on EC2's Intel runners, with no records kept, 32 vCPUs known good and
    /// 255 refused at once, from run 44030858462's arms; a count kept adds to them; the
    /// product's records (`Limits::at`) and a model measured nowhere read their file alone.
    /// Cold, a first boot of 255 vCPUs there ran 2954.6 s, starving the tests beside it, before
    /// its host refused it (CI 44164945050, x86_64 gnu). Red where the measurements are unread.
    #[test]
    fn the_measured_outcomes_size_a_cold_host_models_tests() {
        let dir = shards_testdir::TempDir::new("limits-seed").unwrap();
        let measured = Limits::measured_at(dir.path().to_path_buf(), ec2());
        assert_eq!(measured.known_good(), Some(32), "cold: measured known good");
        let refused = measured.admit(255).unwrap_err();
        assert_eq!((refused.from, refused.refused, refused.vcpu), (64, 3, Some(3)));
        assert!(
            refused
                .to_string()
                .ends_with(", beside D261's measurements of this host model (vm/limits/vcpus.txt))")
        );
        assert!(measured.admit(32).is_ok());
        assert_eq!(
            measured.outcomes().get(&128),
            Some(&Outcomes {
                ran: 1,
                refused: 2,
                last_refused_vcpu: Some(75)
            })
        );
        measured.record(32, Outcome::Ran).unwrap();
        assert_eq!(measured.outcomes().get(&32).map(|o| o.ran), Some(4));
        let product = Limits::at(dir.path().to_path_buf(), ec2());
        assert!(
            product.admit(255).is_ok(),
            "the product's records: its file alone"
        );
        assert_eq!(product.known_good(), Some(32));
        let elsewhere = Limits::measured_at(
            dir.path().to_path_buf(),
            HostModel {
                cpu: "AMD EPYC 7763 64-Core Processor".into(),
                under: Some("Microsoft Corporation".into()),
            },
        );
        assert!(elsewhere.admit(255).is_ok(), "measured nowhere: cold");
        assert_eq!(elsewhere.known_good(), None);
    }

    /// A count that has run is tried though it was refused too; another host model's records
    /// are its own.
    #[test]
    fn a_count_that_ran_is_tried_and_each_host_model_is_its_own() {
        let dir = shards_testdir::TempDir::new("limits-ran").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        limits.record(128, Outcome::Refused { vcpu: 1 }).unwrap();
        limits.record(128, Outcome::Ran).unwrap();
        assert!(limits.admit(128).is_ok());
        assert!(limits.admit(255).is_ok());
        let azure = Limits::at(
            dir.path().to_path_buf(),
            HostModel {
                cpu: "AMD EPYC 9V74 80-Core Processor".into(),
                under: Some("Microsoft Corporation".into()),
            },
        );
        assert!(azure.outcomes().is_empty());
    }

    /// A record that shortens the records' text is written over them in place, the rest of the
    /// file empty lines, none read as a count; one that lengthens them grows the file.
    #[test]
    fn a_shorter_record_is_written_in_place_and_its_tail_read_as_nothing() {
        let dir = shards_testdir::TempDir::new("limits-short").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        limits.record(64, Outcome::Refused { vcpu: 1000 }).unwrap();
        let long = std::fs::metadata(limits.file()).unwrap().len();
        limits.record(64, Outcome::Refused { vcpu: 7 }).unwrap();
        assert_eq!(std::fs::metadata(limits.file()).unwrap().len(), long);
        let want = Outcomes {
            ran: 0,
            refused: 2,
            last_refused_vcpu: Some(7),
        };
        assert_eq!(limits.outcomes().into_iter().collect::<Vec<_>>(), [(64, want)]);
        limits.record(8, Outcome::Ran).unwrap();
        assert!(std::fs::metadata(limits.file()).unwrap().len() > long);
        assert_eq!(limits.outcomes().len(), 2);
    }

    /// A process granted its records (a VM process, D30) reads and keeps them through what it
    /// was granted, not their path, until it lets go of them: here a file elsewhere stands for
    /// the descriptor its spawner opened.
    #[cfg(unix)]
    #[test]
    fn granted_records_are_kept_through_the_grant_until_let_go() {
        let dir = shards_testdir::TempDir::new("limits-grant").unwrap();
        let limits = Limits::at(dir.path().join("records"), ec2());
        let stand_in = dir.path().join("granted");
        std::fs::write(&stand_in, "16 1 0 -\n").unwrap();
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&stand_in)
            .unwrap();
        crate::platform::grant_input(limits.file().to_path_buf(), Some(opened.into()));
        assert_eq!(limits.known_good(), Some(16), "read through the grant");
        limits.record(32, Outcome::Ran).unwrap();
        assert!(!limits.file().exists(), "nothing written at the path");
        assert_eq!(
            std::fs::read_to_string(&stand_in).unwrap(),
            "16 1 0 -\n32 1 0 -\n"
        );
        limits.let_go();
        assert!(limits.outcomes().is_empty(), "the path's own, none yet");
        limits.record(8, Outcome::Ran).unwrap();
        assert_eq!(std::fs::read_to_string(limits.file()).unwrap(), "8 1 0 -\n");
    }

    /// On a host whose Hyper-V is nested within another's partition (CI's Windows runners, 4
    /// logical processors, a partition of 240: PM M379), a machine has at most the host's
    /// logical processors, and one asking more is refused at its start, typed and said with the
    /// evidence; elsewhere the partition's most stands, and so do its refusals' words (ROOT's
    /// ruling of 2026-11-17). Red under the rule it replaces, the partition's most alone, which
    /// admitted 5 to 240 there.
    #[test]
    fn a_nested_hyperv_host_runs_no_more_vcpus_than_its_logical_processors() {
        assert_eq!(most_vcpus(240, None), 240);
        assert_eq!(vcpus_within(240, 240, None), Ok(()));
        assert_eq!(most_vcpus(240, Some(4)), 4);
        assert_eq!(vcpus_within(4, 240, Some(4)), Ok(()));
        let refused = vcpus_within(5, 240, Some(4)).unwrap_err();
        assert_eq!(
            refused,
            VcpusRefused::PastProcessors {
                vcpus: 5,
                processors: 4,
                partition: 240
            }
        );
        let said = refused.to_string();
        for word in [
            "5 vCPUs:",
            "nested within another Hyper-V's partition",
            "4 logical processors",
            "PM M379",
            "44127161851",
        ] {
            assert!(said.contains(word), "{word}: {said}");
        }
        // A nested host of more processors than its partitions take: the partition's most.
        assert_eq!(most_vcpus(240, Some(512)), 240);
        assert_eq!(
            vcpus_within(241, 240, Some(512)),
            Err(VcpusRefused::PastPartition {
                vcpus: 241,
                partition: 240
            })
        );
        assert_eq!(
            vcpus_within(241, 240, None).unwrap_err().to_string(),
            "241 vCPUs requested; this host supports 240"
        );
        assert_eq!(
            vcpus_within(0, 240, Some(4)).unwrap_err().to_string(),
            "at least one vCPU is required"
        );
    }

    /// Records' text reads back as written, and a line that says nothing whole is left out.
    #[test]
    fn records_read_back_and_a_broken_line_is_left_out() {
        let mut all = BTreeMap::new();
        all.insert(
            8,
            Outcomes {
                ran: 3,
                refused: 0,
                last_refused_vcpu: None,
            },
        );
        all.insert(
            64,
            Outcomes {
                ran: 0,
                refused: 2,
                last_refused_vcpu: Some(3),
            },
        );
        assert_eq!(parse(&text(&all)), all);
        assert_eq!(parse("8 3 0 -\nbroken\n9 x 0 -\n64 0 2 3 extra\n"), {
            let mut only = BTreeMap::new();
            only.insert(
                8,
                Outcomes {
                    ran: 3,
                    refused: 0,
                    last_refused_vcpu: None,
                },
            );
            only
        });
    }

    /// An entry refused is said as the host's, naming the vCPU and the hardware's reason, as
    /// D261's experiment reads it (`vCPU N: VM entry failed (hardware reason 0x7)`).
    #[test]
    fn an_entry_refused_is_said_as_the_hosts() {
        let said = EntryRefused {
            vcpu: 17,
            reason: 7,
            refusal: Refusal {
                at_us: 1_794_917_842_377_412,
                machine: 213_829,
                entered: false,
            },
            host: ec2(),
            evidence: None,
        }
        .to_string();
        assert!(said.starts_with("vCPU 17: VM entry failed (hardware reason 0x7): the host's hypervisor ("));
        assert!(
            said.ends_with(
                " under Amazon EC2) refused to enter it (before its guest ran, at 1794917842.377412 s, VM \
                 process 213829)"
            ),
            "{said}"
        );
    }

    /// A refusal kept says what the records now hold of its count, the fewest vCPUs only
    /// refused, and the file that keeps them (ROOT's decision of 2026-11-17: the records shape
    /// the refusal's message alone, the product trying every count).
    #[test]
    fn a_refusal_kept_is_said_with_its_counts_evidence() {
        let dir = shards_testdir::TempDir::new("limits-said").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        limits.record(4, Outcome::Ran).unwrap();
        limits.record(64, Outcome::Refused { vcpu: 13 }).unwrap();
        let all = keep(limits.opened().unwrap(), 4, Outcome::Refused { vcpu: 0 }).unwrap();
        let evidence = limits.evidence(4, &all);
        assert_eq!(
            (
                evidence.outcomes.ran,
                evidence.outcomes.refused,
                evidence.fewest_only_refused
            ),
            (1, 1, Some(64))
        );
        let said = EntryRefused {
            vcpu: 0,
            reason: 7,
            refusal: Refusal {
                at_us: 1_794_917_842_377_412,
                machine: 213_829,
                entered: false,
            },
            host: ec2(),
            evidence: Some(evidence),
        }
        .to_string();
        assert!(
            said.ends_with(
                &("VM process 213829); on this host model (Intel(R) Xeon(R) 6975P-C), machines of 4 \
                   vCPUs have run 1 times and been refused 1 times, the fewest vCPUs it has only \
                   refused 64 (kept in "
                    .to_string()
                    + &limits.file().display().to_string()
                    + ")")
            ),
            "{said}"
        );
        assert!(limits.admit(4).is_ok(), "a count that ran is asked");
    }

    /// A machine of EC2's Intel runners refused at vCPU `vcpu`, at `at_us`, in VM process
    /// `machine`, its guest having run or not.
    fn refused(vcpu: usize, at_us: u64, machine: u32, entered: bool) -> EntryRefused {
        EntryRefused {
            vcpu,
            reason: VMX_INVALID_CONTROL_FIELD,
            refusal: Refusal {
                at_us,
                machine,
                entered,
            },
            host: ec2(),
            evidence: None,
        }
    }

    /// `refused` as the tests read it back from what its VM process said, on an x86-64 host,
    /// where its reason is VMX's, whatever host these tests run on.
    fn said(refused: &EntryRefused) -> Said {
        Said {
            vmx: true,
            ..Said::find(&refused.to_string()).unwrap()
        }
    }

    /// A refusal is read back from what carries its VM process's words, among other lines (a
    /// client's error, a daemon's log), as D261's gate reads it: its vCPU, its reason, whether
    /// its host is nested, its moment, its VM process, whether its guest had run, and the
    /// records that keep it; none where it kept none (a restore's). A VM process's words from
    /// before the gate name no moment, and are read as none.
    #[test]
    fn a_refusal_said_reads_back_as_d261s_gate_reads_it() {
        let dir = shards_testdir::TempDir::new("limits-read").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        let refusal = Refusal {
            at_us: 1_794_927_334_255_000,
            machine: 213_829,
            entered: true,
        };
        let all = keep_refusal(limits.opened().unwrap(), 4, 3, refusal).unwrap();
        let kept = EntryRefused {
            evidence: Some(limits.evidence(4, &all)),
            ..refused(3, refusal.at_us, refusal.machine, refusal.entered)
        };
        let words = kept.to_string();
        let text = format!(
            "shards daemon 213761: serving\nshards exit status: 1: shards: {words}\nshards: the microVM \
             ended before its guest connected\n"
        );
        assert_eq!(
            Said::find(&text),
            Some(Said {
                line: words.clone(),
                vcpu: 3,
                reason: 7,
                vmx: cfg!(target_arch = "x86_64"),
                nested: true,
                refusal,
                file: Some(limits.file().to_path_buf()),
            })
        );
        let restored = EntryRefused {
            evidence: None,
            ..refused(0, refusal.at_us, refusal.machine, false)
        };
        let back = said(&restored);
        assert_eq!((back.vcpu, back.refusal.entered, back.file), (0, false, None));
        assert_eq!(
            Said::find("shards: vCPU 3: VM entry failed (hardware reason 0x7)"),
            None
        );
    }

    /// D261's gate takes the refusals CI's EC2 Intel runners made of small machines beside a
    /// large one for the host's: a machine refused at its first entry, before its guest ran,
    /// within the burst width of another VM process whose guest had run. The moments are their
    /// VMCS dumps' dmesg times, the witness kept before the refusal or after it.
    #[test]
    fn the_gate_takes_a_first_entry_refusal_inside_a_burst() {
        let dir = shards_testdir::TempDir::new("limits-burst").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        let width = measured_width_us();
        // CI 44076016255, x86_64 musl: D261's A/B arm "parallel", 255 vCPUs 82.65 s in, its
        // last dump at 2419.086547; then runs_init_as_pid_1_and_powers_off's 1-vCPU machine,
        // refused at its first VMLAUNCH (RIP at its kernel's entry, no exit before it).
        let arm = refused(13, 2_419_086_547, 4001, true);
        let small = refused(0, 2_419_727_678, 4002, false);
        for machine in [&arm, &small] {
            keep_refusal(limits.opened().unwrap(), 1, machine.vcpu, machine.refusal).unwrap();
        }
        assert_eq!(
            limits.burst_width_us(),
            width,
            "a burst narrower than the measured width"
        );
        assert_eq!(
            the_hosts(&said(&small), &limits.refusals(), limits.burst_width_us()),
            Ok(Witness::Burst {
                machine: 4001,
                apart_us: 641_131,
                width_us: width
            })
        );
        // CI 44076016255, x86_64 gnu: the arm 233.9 s in, its last dump at 2020.355355; then
        // three machines refused at their first entries, the last 0.702252 s after it.
        let arm = refused(40, 2_020_355_355, 4101, true);
        let firsts = [2_020_888_223, 2_020_961_363, 2_021_057_607].map(|at| refused(0, at, 4102, false));
        let kept = [arm.refusal];
        for first in &firsts {
            assert!(
                matches!(
                    the_hosts(&said(first), &kept, width),
                    Ok(Witness::Burst { machine: 4101, .. })
                ),
                "{first}"
            );
        }
        // CI 44077430171, x86_64 musl: a machine refused before its guest ran at 3040.956945,
        // and another VM process 0.095602 s later, after an EPT violation's exit: its witness
        // kept after it, which the tests wait the burst width for.
        let before = refused(3, 3_040_956_945, 213_829, false);
        let later = refused(1, 3_041_052_547, 213_901, true);
        assert_eq!(
            the_hosts(&said(&before), &[before.refusal, later.refusal], width),
            Ok(Witness::Burst {
                machine: 213_901,
                apart_us: 95_602,
                width_us: width
            })
        );
    }

    /// A span of time whose every refusal D261's gate takes for its host's is its host's (a
    /// bench's rate whose host refused entries, ROOT's ruling of 2026-11-18), said with how
    /// many, the first and the last, and the first's witness; a refusal said twice counts once.
    /// A span with any refusal the gate does not take is not: each such refusal is said with
    /// why, its failure standing. A span with none is neither. The shape of CI 44245219858's
    /// firecracker job: a machine whose guest had run refused, and others at their first
    /// entries within the burst width of it.
    #[test]
    fn a_spans_refusals_are_its_hosts_only_where_the_gate_takes_every_one() {
        let width = measured_width_us();
        let ran = refused(3, 1_794_982_514_349_836, 111_001, true);
        let firsts = [
            refused(0, 1_794_982_514_400_000, 111_002, false),
            refused(3, 1_794_982_515_000_000, 111_003, false),
        ];
        let all: Vec<Said> = [&ran, &firsts[0], &firsts[1]].map(said).to_vec();
        let kept: Vec<Refusal> = all.iter().map(|s| s.refusal).collect();
        // Each said twice, as a VM process's own line and its daemon's log say it.
        let twice: Vec<Said> = all.iter().chain(all.iter()).cloned().collect();
        let taken = the_hosts_all(&twice, &kept, width).unwrap().unwrap();
        assert_eq!(
            (
                taken.count,
                taken.first.refusal,
                taken.last.refusal,
                taken.witness
            ),
            (3, ran.refusal, firsts[1].refusal, Witness::Ran)
        );
        // One more, before its guest ran, past the width of every machine whose guest had.
        let alone = refused(0, 1_794_982_600_000_000, 111_004, false);
        let with_alone: Vec<Said> = all.iter().cloned().chain([said(&alone)]).collect();
        let kept: Vec<Refusal> = with_alone.iter().map(|s| s.refusal).collect();
        assert_eq!(
            the_hosts_all(&with_alone, &kept, width),
            Err(vec![(said(&alone), NotTheHosts::Alone { width_us: width })])
        );
        assert_eq!(the_hosts_all(&[], &kept, width), Ok(None));
    }

    /// D261's gate takes a refusal of a machine whose guest had run for its host's, alone or
    /// not: its host had entered the state the VMM gave it, and refused it later (D261's A/B
    /// arm refused at vCPU 13 82.65 s in, CI 44076016255).
    #[test]
    fn the_gate_takes_a_refusal_after_its_guest_ran() {
        let ran = refused(13, 2_419_086_547, 4001, true);
        assert_eq!(the_hosts(&said(&ran), &[], measured_width_us()), Ok(Witness::Ran));
        assert_eq!(the_hosts(&said(&ran), &[], 0), Ok(Witness::Ran));
    }

    /// What a VMM's own error looks like is never taken for the host's, every run: a machine
    /// refused at its first entry, before its guest ran, alone on an idle host. Nor are
    /// machines refused at once, every one before its guest ran, taken as each other's
    /// witnesses, as one VMM error refusing every machine's first entry would refuse them; and
    /// no burst is learned from them. Another reason than VMX's error 7, or a host that is not
    /// itself a VM, is not taken either.
    #[test]
    fn the_gate_fails_a_first_entry_refusal_alone_on_an_idle_host_every_run() {
        let dir = shards_testdir::TempDir::new("limits-alone").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        let width = measured_width_us();
        // A run every ten minutes, each its own machine, each refused before its guest ran.
        for run in 0..5u64 {
            let alone = refused(
                0,
                1_000_000_000 + run * 600_000_000,
                5000 + u32::try_from(run).unwrap(),
                false,
            );
            keep_refusal(limits.opened().unwrap(), 1, alone.vcpu, alone.refusal).unwrap();
            assert_eq!(
                the_hosts(&said(&alone), &limits.refusals(), limits.burst_width_us()),
                Err(NotTheHosts::Alone { width_us: width }),
                "run {run}"
            );
        }
        let at = 9_000_000_000u64;
        let many: Vec<EntryRefused> = (0..4u32)
            .map(|i| refused(0, at + u64::from(i) * 1_000, 6000 + i, false))
            .collect();
        for machine in &many {
            keep_refusal(limits.opened().unwrap(), 1, machine.vcpu, machine.refusal).unwrap();
        }
        for machine in &many {
            assert_eq!(
                the_hosts(&said(machine), &limits.refusals(), limits.burst_width_us()),
                Err(NotTheHosts::Alone { width_us: width }),
                "{machine}"
            );
        }
        assert_eq!(limits.burst_width_us(), width, "no burst learned of them");
        assert_eq!(limits.outcomes().get(&1).map(|o| o.refused), Some(9));
        let ran = refused(0, at, 7000, true);
        let other = Said {
            reason: 0x8000_0021,
            ..said(&ran)
        };
        assert_eq!(
            the_hosts(&other, &[], width),
            Err(NotTheHosts::Reason(0x8000_0021))
        );
        let not_vmx = Said {
            vmx: false,
            ..said(&ran)
        };
        assert_eq!(the_hosts(&not_vmx, &[], width), Err(NotTheHosts::Reason(7)));
        let bare = EntryRefused {
            host: HostModel {
                cpu: "Intel(R) Xeon(R) 6975P-C".into(),
                under: None,
            },
            ..refused(0, at, 7001, true)
        };
        assert_eq!(the_hosts(&said(&bare), &[], width), Err(NotTheHosts::NotNested));
    }

    /// A host model's burst width starts at the largest gap between consecutive refusals of one
    /// burst D261 measured: CI 44077430171's x86_64 musl job, its VMCS dumps at 2382.272392 and
    /// 2383.395391, 1.122999 s apart (that burst's span, 1.132342 s, is no gap). Its records keep
    /// the refusals a burst may yet join, let go of a burst's once a refusal past the width
    /// begins another, and keep each burst's largest gap, which never widens the width: a
    /// burst's refusals each come within the width of the one before. One machine's refusals
    /// are no burst.
    #[test]
    fn the_burst_width_starts_at_the_largest_gap_measured_and_no_burst_widens_it() {
        assert_eq!(measured_width_us(), 1_122_999);
        for run in ["44076016255", "44077430171", "44031797055"] {
            assert!(MEASURED.contains(run), "{run}");
        }
        let dir = shards_testdir::TempDir::new("limits-width").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        let width = measured_width_us();
        assert_eq!(
            limits.burst_width_us(),
            width,
            "the measured width, before any burst"
        );
        let t0 = 5_000_000_000u64;
        for (machine, at_us, entered) in [
            (8000, t0, true),
            (8001, t0 + width, false),
            (8002, t0 + 2 * width, false),
        ] {
            keep_refusal(
                limits.opened().unwrap(),
                4,
                0,
                Refusal {
                    at_us,
                    machine,
                    entered,
                },
            )
            .unwrap();
        }
        assert_eq!(
            limits.burst_width_us(),
            width,
            "a burst whose gaps are the width widened it"
        );
        assert_eq!(limits.refusals().len(), 3, "the burst's kept");
        let later = Refusal {
            at_us: t0 + 10 * width,
            machine: 8003,
            entered: true,
        };
        keep_refusal(limits.opened().unwrap(), 4, 0, later).unwrap();
        assert_eq!(limits.refusals(), [later], "the burst before let go of");
        assert_eq!(limits.burst_width_us(), width, "its width kept");
        for at_us in [t0 + 10 * width + 1, t0 + 13 * width] {
            keep_refusal(
                limits.opened().unwrap(),
                4,
                1,
                Refusal {
                    at_us,
                    machine: 8003,
                    entered: true,
                },
            )
            .unwrap();
        }
        assert_eq!(
            limits.burst_width_us(),
            width,
            "one machine's refusals are no burst"
        );
        assert_eq!(limits.outcomes().get(&4).map(|o| o.refused), Some(6));
    }

    /// A long chain of close refusals leaves a host model's burst width unchanged (ROOT's
    /// ruling of 2026-11-17): a burst teaches the largest gap between its consecutive
    /// refusals, never its whole span. Here a hundred refusals of two VM processes, one of
    /// whose guests had run, each a tenth of the width after the one before: one burst whose
    /// span is nearly ten widths and whose largest gap is a tenth of one. Taught its span, the
    /// width grew to that span, and a refusal anywhere in the next ten widths found a witness.
    #[test]
    fn a_long_chain_of_close_refusals_leaves_the_burst_width_unchanged() {
        let dir = shards_testdir::TempDir::new("limits-chain").unwrap();
        let limits = Limits::at(dir.path().to_path_buf(), ec2());
        let width = measured_width_us();
        let t0 = 7_000_000_000u64;
        for i in 0..100u64 {
            let refusal = Refusal {
                at_us: t0 + i * (width / 10),
                machine: if i % 2 == 0 { 9000 } else { 9001 },
                entered: i == 0,
            };
            keep_refusal(limits.opened().unwrap(), 2, 0, refusal).unwrap();
        }
        assert_eq!(limits.refusals().len(), 100, "one burst, kept whole");
        assert_eq!(
            limits.burst_width_us(),
            width,
            "a chain of close refusals widened the burst width"
        );
    }
}
