//! A host's real vCPU limit, as its own machines show it (D261, ROOT's ruling of
//! 2026-11-17). A hypervisor advertises the most vCPUs it takes (KVM_CAP_MAX_VCPUS, WHP's
//! CPUID 0x40000005, capped by the MADT), and some hosts enter fewer: EC2's nested KVM refused
//! the entry of a machine of 64 vCPUs or more on 4 CPUs (VM-instruction error 7), where Azure's
//! Hyper-V entered 255 (D261's experiment, run 44030858462). No count is chosen here: each
//! count's outcomes are kept per host model, the CPU's model and the hypervisor it runs under,
//! as its machines come to them ([`Limits::record`]). A count that has only been refused on its
//! host model, and every larger one never run there, is refused at once, said as the host's
//! with that evidence ([`Limits::admit`]); a count that has run is tried, its refusal, if it
//! comes, the host's ([`EntryRefused`]). The hypervisor's own most stays the ceiling.
//!
//! A host model's records are one file, read and written through
//! [`open_input`](crate::platform::open_input): in a VM process the one its spawner granted it
//! (D30), as its disks are, which it may neither replace by a rename nor, in App Sandbox, be
//! sure to cut short, so it is written over in place under its own lock; elsewhere the file at
//! its path.

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
    /// within a name's bounds.
    fn key(&self) -> String {
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

/// A host model's records: its file in the records' directory, a line a count, `COUNT RAN
/// REFUSED LAST`, LAST the vCPU the last refusal named or `-`.
#[derive(Debug, Clone)]
pub struct Limits {
    file: PathBuf,
    host: HostModel,
}

impl Limits {
    /// The records of host model `host` kept in `dir`.
    pub fn at(dir: PathBuf, host: HostModel) -> Limits {
        Limits {
            file: dir.join(host.key()),
            host,
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

    /// Each count's outcomes as kept: none where nothing is, a line that says nothing whole
    /// left out. Read under the file's lock, shared, so that a write is read whole.
    pub fn outcomes(&self) -> BTreeMap<u32, Outcomes> {
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
        parse(&String::from_utf8_lossy(&read().unwrap_or_default()))
    }

    /// Whether a machine of `vcpus` is tried: refused at once where it has never run on this
    /// host model and a count no larger has only been refused there ([`OnlyRefused`]). A count
    /// that has run is tried, whatever else it and others came to.
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
        keep(self.opened()?, vcpus, outcome)
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

    /// Holds the records' file open from now on, as granted (`platform::grant_input`): to be
    /// read and written where `write`, made where it is not there; else to be read where it is.
    /// A VM process on Linux, before Landlock confines it (D30), which then gives it no rule
    /// for the file: it reaches its records through this alone, and not once it lets go of
    /// them ([`let_go`](Self::let_go)).
    #[cfg(target_os = "linux")]
    pub fn hold(&self, write: bool) -> io::Result<()> {
        let file = if write {
            crate::platform::open_made(&self.file)?
        } else {
            match std::fs::File::open(&self.file) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    crate::platform::grant_input(self.file.clone(), None);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        };
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
/// (App Sandbox), and a line it shortens leaves no line of its tail behind to be read.
pub fn keep(mut file: std::fs::File, vcpus: u32, outcome: Outcome) -> io::Result<()> {
    file.lock()?;
    let kept = rewrite(&mut file, vcpus, outcome);
    let unlocked = file.unlock();
    kept.and(unlocked)
}

/// [`keep`]'s write, under its lock.
fn rewrite(file: &mut std::fs::File, vcpus: u32, outcome: Outcome) -> io::Result<()> {
    let mut was = Vec::new();
    file.read_to_end(&mut was)?;
    let mut all = parse(&String::from_utf8_lossy(&was));
    let o = all.entry(vcpus).or_default();
    match outcome {
        Outcome::Ran => o.ran = o.ran.saturating_add(1),
        Outcome::Refused { vcpu } => {
            o.refused = o.refused.saturating_add(1);
            o.last_refused_vcpu = Some(vcpu);
        }
    }
    let mut now = text(&all);
    now.extend(std::iter::repeat_n('\n', was.len().saturating_sub(now.len())));
    file.seek(io::SeekFrom::Start(0))?;
    file.write_all(now.as_bytes())
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

/// A machine whose vCPU its host's hypervisor refused to enter (KVM_EXIT_FAIL_ENTRY's hardware
/// reason, VMX's VM-instruction error): the host's, never the guest's (D261).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRefused {
    pub vcpu: usize,
    pub reason: u64,
    pub host: HostModel,
}

impl fmt::Display for EntryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vCPU {}: VM entry failed (hardware reason {:#x}): the host's hypervisor ({}) refused to enter it",
            self.vcpu,
            self.reason,
            hypervisor_under(&self.host)
        )
    }
}

/// A machine refused at once ([`Limits::admit`]): its count, and the least count its host
/// model has only refused, how often, the vCPU the last refusal named, and the file that keeps
/// them, which its user may remove where the host has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlyRefused {
    pub vcpus: u32,
    pub from: u32,
    pub refused: u64,
    pub vcpu: Option<usize>,
    pub host: HostModel,
    pub file: PathBuf,
}

impl fmt::Display for OnlyRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let which = self
            .vcpu
            .map_or_else(|| "a vCPU".to_string(), |v| format!("vCPU {v}"));
        write!(
            f,
            "{} vCPUs: the host's hypervisor ({}) refused to enter {which}; on this host model \
             ({}), counts \u{2265} {} have only been refused ({} times), so it is not tried (kept in \
             {})",
            self.vcpus,
            hypervisor_under(&self.host),
            self.host.cpu,
            self.from,
            self.refused,
            self.file.display()
        )
    }
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
            host: ec2(),
        }
        .to_string();
        assert!(said.starts_with("vCPU 17: VM entry failed (hardware reason 0x7): the host's hypervisor ("));
        assert!(said.ends_with(" under Amazon EC2) refused to enter it"), "{said}");
    }
}
