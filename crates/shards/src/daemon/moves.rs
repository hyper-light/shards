//! A microVM moved to another machine (D176 part 6): `shards move vm NAME CONTEXT`, whose
//! client asks this daemon and the other machine's, through its context (D180), for its
//! steps one at a time (cli/moves.rs), as `shards copy vm` asks for its own.
//!
//! - `save NAME`: the microVM saved whole and stopped (D176), or the snapshot a save
//!   stopped it as; its [`Plan`], on stdout: what the other machine needs of it.
//! - `ready PLAN`, there: whether the move can be taken, and what it lacks ([`Needs`]): its
//!   image, by the digest it was made of, found there or pulled; the files its snapshot
//!   restores against, each byte for byte as here, else sent.
//! - `export NAME SNAPSHOT [FILE...]`: its directory, its snapshot, its volumes and the
//!   files the other machine lacks, written to the client's descriptor ([`Writer`]).
//! - `import`, there: all of it read from the client's descriptor, put in place, its
//!   snapshot made to restore against that machine's files, and the microVM made there,
//!   stopped, as saved; `restore vm` resumes it.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use shards_vmm::devices::virtio::fs::server::guest;

use super::commands::{Asker, Reply};
use super::snapshots::SNAPSHOTS;
use super::{Daemon, lock};
use crate::containers::{Container, Disk};

/// The stream's first bytes, then its version: 2, each entry as its guest sees it
/// ([`guest::Meta`]), where 1's held the host's own modes and attributes.
const MAGIC: &[u8; 8] = b"SHRDMOVE";
const VERSION: u32 = 2;
/// Kinds of the stream's entries.
const END: u8 = 0;
const DIR: u8 = 1;
const FILE: u8 = 2;
const LINK: u8 = 3;
const PLAN: u8 = 4;
/// A FIFO or a device node.
const NODE: u8 = 5;
/// The most a name in the stream may hold: a path of the guest's, Linux's `PATH_MAX`
/// (include/uapi/linux/limits.h l. 13, Linux 6.18).
const MAX_PATH: usize = 4096;
/// The most a plan's JSON may hold: the plan the other machine was given as a word of its
/// `ready` step's request (cli/moves.rs), a frame of a context's connection at most
/// ([`shards_remote::frame::MAX_FRAME`]); the stream's is that one.
const MAX_PLAN: usize = shards_remote::frame::MAX_FRAME;
/// The most of a file's extended attributes, as a Linux guest's file system holds them:
/// an attribute's name, its value, and the list of its names (`XATTR_NAME_MAX`,
/// `XATTR_SIZE_MAX` and `XATTR_LIST_MAX`, include/uapi/linux/limits.h l. 15-17).
const MAX_XATTR_NAME: usize = 255;
const MAX_XATTR: usize = 64 << 10;
const MAX_XATTR_LIST: usize = 64 << 10;
/// The most attributes a file holds: as many as its list holds of the shortest name, the
/// shortest namespace's prefix (`os2.`, include/uapi/linux/xattr.h l. 33), a byte, and the
/// NUL each name in the list ends with (listxattr(2)).
const MAX_XATTRS: usize = MAX_XATTR_LIST / ("os2.".len() + 1 + 1);
/// How much of a file a read or write moves at once.
const CHUNK: usize = 1 << 20;
/// Where a moved microVM's files that its new machine had none of, or another of, are kept
/// in its directory: what its snapshot restores against there.
const BACKING: &str = "backing";
/// A live move's rounds (D176 part 7): at most so many, Xen's own (xc_domain_save.c,
/// `DEF_MAX_ITERS`, "limit us to 30 times round loop"), the last the first that sends no
/// more than so many bytes, 8 MiB, what a link of 1 Gb/s carries in 67 ms, or no fewer than
/// the round before, or that brings them to three times the first (`live::Stop`).
const LIVE_ROUNDS: u32 = 30;
const LIVE_ENOUGH: u64 = 8 << 20;

/// What a machine needs to take a microVM moved to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    /// Its ID and name, which it keeps.
    pub id: String,
    pub name: String,
    /// Its image as its run named it, and the digest that name resolved to.
    pub image: String,
    pub image_id: String,
    /// Its guest's architecture, and the snapshot format it was saved in.
    pub arch: String,
    pub format: u32,
    /// The hypervisor it ran on (`hv::BACKEND`), whose own its snapshot's vCPUs, interrupt
    /// controller and devices are: none from a daemon of an earlier build, which said none.
    #[serde(default)]
    pub hypervisor: Option<String>,
    /// The snapshot it moves as.
    pub snapshot: String,
    /// This machine's home, resolved: what each file's name in the move is relative to.
    pub home: String,
    /// The files its snapshot restores against, each relative to the home.
    pub files: Vec<Backing>,
    /// The named volumes it mounts, which move with it.
    pub volumes: Vec<String>,
    /// The host directories and files it binds, which the other machine must share with
    /// this one at the same paths: a network file system's, served there as here.
    pub binds: Vec<String>,
    /// This machine's CPU's features, as its OS names them ([`host_cpu`]): the other's must
    /// have each, as its guest may have found and used any.
    #[serde(default)]
    pub cpu: Vec<String>,
    /// Whether its run boots the guest its home stores, no kernel given: the pinned
    /// kernel, which a restore of it prepares its run's request with (run::prepare).
    #[serde(default)]
    pub guest: bool,
    /// The CPU baseline its guest was told (D176 part 6), as `arch::cpu` names it (ID
    /// registers by encoding; CPUID's feature leaves): a machine whose hypervisor shows
    /// every field or bit of it takes it, whatever more the machine it ran on has. Empty
    /// where it was told none, its host's own CPU.
    #[serde(default)]
    pub baseline: shards_vmm::arch::cpu::Id,
    /// The CPU its snapshot was saved on, where its hypervisor restores a snapshot only on
    /// that CPU (WHP: `cpuid::IDENTITY`'s leaves, D245), for the machine moved to to check
    /// before anything moves. Empty elsewhere, and from a daemon of an earlier build.
    #[serde(default)]
    pub cpu_identity: shards_vmm::arch::cpu::Id,
    /// The ports its run publishes on its machine's addresses, as its run asked for them
    /// (`run -p`), which its restore binds again as a start does, and the network whose host
    /// address a binding that names none takes: for the machine moved to to check before
    /// anything moves. None where it bound no host port, and from a daemon of an earlier
    /// build.
    #[serde(default)]
    pub published: Vec<Published>,
    #[serde(default)]
    pub network: String,
    /// The SHA-256 of its run's own key where it ran (D238, plan step 11), as its managed
    /// metadata names it, in lowercase hex: the key the move replaces, as its run makes a key
    /// of its own where it lands (D176 part 6). None where its run has none, and from a
    /// daemon of an earlier build.
    #[serde(default)]
    pub key: Option<String>,
}

/// What a run moved here keeps of the key its run had where it ran (`Plan::key`), which no
/// lease is served under here.
const KEY_REPLACED: &str = "key-replaced";

/// What a run keeps the CPU baseline its guest was told in, in its container's directory
/// (D176 part 6), as its home keeps its own (`cli::moves::BASELINE`): written as the run
/// starts, and moved with it.
pub(super) const RUN_BASELINE: &str = "cpu-baseline";

/// The CPU baseline `home`'s microVMs are told ([`crate::cli::moves::BASELINE`]); none
/// where it keeps none, or none it can read.
pub(super) fn home_baseline(home: &Path) -> shards_vmm::arch::cpu::Id {
    read_baseline(&home.join(crate::cli::moves::BASELINE))
}

/// The CPU on the first line of a baseline's file at `at`; none where there is none.
fn read_baseline(at: &Path) -> shards_vmm::arch::cpu::Id {
    std::fs::read_to_string(at)
        .ok()
        .and_then(|text| shards_vmm::arch::cpu::decode(text.lines().next()?).ok())
        .unwrap_or_default()
}

/// The register of each feature macOS names (`hw.optional.arm.FEAT_*`, [`host_cpu`]) that
/// its field is in (Arm ARM DDI 0487, the ID_AA64* registers' fields): a baseline that
/// names the register masks the feature where it is lower, and a feature whose register
/// the baseline does not name (ISAR2's, which Hypervisor.framework neither says nor lets a
/// VMM set, PM M460) is the guest's whatever the baseline says, and checked by name.
#[cfg(target_arch = "aarch64")]
fn feature_register(feature: &str) -> Option<u16> {
    Some(match feature {
        "FEAT_CSV2" | "FEAT_CSV3" | "FEAT_DIT" | "FEAT_FP16" => 0xc020,
        "FEAT_SSBS" | "FEAT_BTI" => 0xc021,
        "FEAT_CRC32" | "FEAT_FlagM" | "FEAT_FlagM2" | "FEAT_FHM" | "FEAT_DotProd" | "FEAT_SHA3"
        | "FEAT_RDM" | "FEAT_LSE" | "FEAT_SHA256" | "FEAT_SHA512" | "FEAT_SHA1" | "FEAT_AES"
        | "FEAT_PMULL" => 0xc030,
        "FEAT_SPECRES" | "FEAT_SPECRES2" | "FEAT_SB" | "FEAT_FRINTTS" | "FEAT_PACIMP" | "FEAT_LRCPC"
        | "FEAT_LRCPC2" | "FEAT_FCMA" | "FEAT_JSCVT" | "FEAT_PAuth" | "FEAT_PAuth2" | "FEAT_FPAC"
        | "FEAT_FPACCOMBINE" | "FEAT_DPB" | "FEAT_DPB2" | "FEAT_BF16" | "FEAT_EBF16" | "FEAT_I8MM" => 0xc031,
        "FEAT_WFxT" | "FEAT_RPRES" | "FEAT_CSSC" | "FEAT_HBC" => 0xc032,
        "FEAT_ECV" => 0xc038,
        "FEAT_AFP" => 0xc039,
        "FEAT_LSE2" => 0xc03a,
        _ => return None,
    })
}

/// The features of `ran_on` a guest told `baseline` may still have used: those whose
/// register the baseline does not name, and every one where it is none.
#[cfg(target_arch = "aarch64")]
fn unmasked(ran_on: &[String], baseline: &[(u16, u64)]) -> Vec<String> {
    ran_on
        .iter()
        .filter(|f| {
            baseline.is_empty()
                || feature_register(f).is_none_or(|reg| !baseline.iter().any(|(r, _)| *r == reg))
        })
        .cloned()
        .collect()
}

/// The features of `ran_on` a guest told `baseline` may still have used: every one where it
/// is none; else none, as each instruction set an x86-64 guest finds is a bit of the CPUID
/// feature leaves a baseline names whole (`cpuid::FEATURES`), which `covers` checks of it,
/// where the names of /proc/cpuinfo's `flags` are Linux's, kernel features among them.
#[cfg(target_arch = "x86_64")]
fn unmasked(ran_on: &[String], baseline: &[[u32; 7]]) -> Vec<String> {
    if baseline.is_empty() {
        ran_on.to_vec()
    } else {
        Vec::new()
    }
}

/// The features of this machine's CPU a guest may be shown, as its OS names them: on macOS
/// each `hw.optional.arm.FEAT_*` it has, but those of SVE, SME and MTE, which no guest of
/// Hypervisor.framework's is shown (D14); on Linux the first CPU's `flags` (x86) or
/// `Features` (arm64) in /proc/cpuinfo; none elsewhere, which no move checks. Where one
/// machine's guest found a feature the other's CPU has not (ID_AA64ISAR2_EL1's CSSC and HBC,
/// an M5's and not an M4's, which Hypervisor.framework neither says nor lets a VMM set, PM
/// M460), a program that used it would fault there. Not FEAT_WFxT and FEAT_RPRES, of that
/// register too, which the framework shows no guest where macOS names them (an M4 Max's
/// guest finds no `wfxt` and an M3 Ultra's or M4 Max's no `rpres`, PM M499; tests/moved.rs
/// `a_cpu_baseline_is_all_its_guest_finds_and_goes_with_it` holds it on each Mac it runs).
pub(crate) fn host_cpu() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        const FEATURES: &[&str] = &[
            "FEAT_CRC32",
            "FEAT_FlagM",
            "FEAT_FlagM2",
            "FEAT_FHM",
            "FEAT_DotProd",
            "FEAT_SHA3",
            "FEAT_RDM",
            "FEAT_LSE",
            "FEAT_SHA256",
            "FEAT_SHA512",
            "FEAT_SHA1",
            "FEAT_AES",
            "FEAT_PMULL",
            "FEAT_SPECRES",
            "FEAT_SPECRES2",
            "FEAT_SB",
            "FEAT_FRINTTS",
            "FEAT_PACIMP",
            "FEAT_LRCPC",
            "FEAT_LRCPC2",
            "FEAT_FCMA",
            "FEAT_JSCVT",
            "FEAT_PAuth",
            "FEAT_PAuth2",
            "FEAT_FPAC",
            "FEAT_FPACCOMBINE",
            "FEAT_DPB",
            "FEAT_DPB2",
            "FEAT_BF16",
            "FEAT_EBF16",
            "FEAT_I8MM",
            "FEAT_CSSC",
            "FEAT_HBC",
            "FEAT_ECV",
            "FEAT_AFP",
            "FEAT_LSE2",
            "FEAT_CSV2",
            "FEAT_CSV3",
            "FEAT_DIT",
            "FEAT_FP16",
            "FEAT_SSBS",
            "FEAT_BTI",
        ];
        FEATURES
            .iter()
            .filter(|f| {
                let Ok(name) = std::ffi::CString::new(format!("hw.optional.arm.{f}")) else {
                    return false;
                };
                let mut value: libc::c_int = 0;
                let mut len = size_of::<libc::c_int>();
                // SAFETY: sysctlbyname writes at most `len` bytes into `value`.
                let rc = unsafe {
                    libc::sysctlbyname(
                        name.as_ptr(),
                        (&raw mut value).cast(),
                        &mut len,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                rc == 0 && value == 1
            })
            .map(|f| (*f).to_string())
            .collect()
    }
    #[cfg(target_os = "linux")]
    {
        let key = if cfg!(target_arch = "x86_64") {
            "flags"
        } else {
            "Features"
        };
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|l| l.split(':').next().is_some_and(|k| k.trim() == key))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
            })
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// The features of `ran_on`, the CPU of the machine a microVM ran on, that `here`, this
/// machine's, lacks: none where this machine's OS names none ([`host_cpu`]), which no move
/// checks.
fn cpu_lacks<'a>(ran_on: &'a [String], here: &[String]) -> Vec<&'a str> {
    if here.is_empty() {
        return Vec::new();
    }
    ran_on
        .iter()
        .filter(|f| !here.contains(f))
        .map(String::as_str)
        .collect()
}

/// A port a run publishes on its machine's addresses, as `run -p` asked for it (its
/// request's `shards_ipc::Publish`): the guest's port and protocol, and the host's address
/// and port or range, empty where not given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Published {
    pub port: u16,
    pub proto: String,
    pub host_ip: String,
    pub host_port: String,
}

/// A file a snapshot restores against, by its name relative to the home, its length and
/// its bytes' SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Backing {
    /// As its snapshot names it, here.
    pub path: String,
    pub rel: String,
    pub size: u64,
    pub sha256: String,
}

/// What a machine lacks to take a [`Plan`]: its image, which the client loads there, and the
/// files the export is to send; and the packings of a move's stream it unpacks
/// (`crate::packing`), none where a daemon of an earlier build answers, which is sent the
/// stream as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Needs {
    pub image: bool,
    pub files: Vec<String>,
    #[serde(default)]
    pub unpacks: Vec<String>,
}

/// Why a machine refuses a microVM moved to it, before anything of it moves: its `ready`
/// step's answer, as JSON on stdout beside its words on stderr, so that the mover matches
/// what was refused (cli/moves.rs `Refused`) and a client of an earlier build still reads
/// why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refused", rename_all = "kebab-case")]
pub(crate) enum Refused {
    /// Its guest is another architecture's.
    Arch {
        name: String,
        arch: String,
        here: String,
    },
    /// It ran on another hypervisor, whose own its snapshot's vCPUs, interrupt controller
    /// and devices are; `here` is none where this build drives no VMs.
    Hypervisor {
        name: String,
        hypervisor: String,
        here: Option<String>,
    },
    /// It was saved in a snapshot format this build does not read.
    Format { name: String, format: u32, here: u32 },
    /// This machine's hypervisor shows a guest less of each of these fields than the CPU
    /// baseline its guest was told.
    Cpu { name: String, short: Vec<CpuShort> },
    /// The CPU baseline its guest was told is another vendor's than this machine's CPU.
    CpuVendor { name: String },
    /// This machine's CPU is not the one its snapshot was saved on, which its hypervisor
    /// restores a snapshot only on (WHP).
    CpuIdentity { name: String },
    /// This machine's CPU lacks these features, as its OS names them, which its guest may
    /// have used where it ran.
    Features { name: String, lacks: Vec<String> },
    /// A microVM with its ID is here.
    IdTaken { name: String },
    /// A microVM of its name is here.
    NameTaken { name: String },
    /// A volume of the name of one it moves with is here.
    VolumeTaken { name: String, volume: String },
    /// A host path it binds is not on this machine.
    BindAbsent { name: String, path: String },
    /// A port it publishes cannot be bound on this machine, as its restore would bind it:
    /// the binding as its run asked for it, and why.
    PortTaken {
        name: String,
        port: u16,
        proto: String,
        host_ip: String,
        host_port: String,
        why: String,
    },
    /// Its run has a managed scope (its owner's streams, its agent-control endpoint), which a
    /// restore does not take yet: refused where it runs, before it is saved.
    Managed { name: String },
    /// A lease asked under the key its run had before it moved here, which its move replaced
    /// (D176 part 6): the key its run has here, where it has one.
    KeyReplaced {
        name: String,
        asked: String,
        current: Option<String>,
    },
}

/// A field of the CPU a guest was told that this machine's hypervisor shows less of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum CpuShort {
    /// A field of an AArch64 ID register ([`shards_vmm::arch::aarch64::cpu::Short`]).
    Field {
        reg: u16,
        shift: u32,
        guest: i8,
        here: i8,
    },
    /// A feature register of x86-64's CPUID: the bits of it the guest was told that this
    /// machine's hypervisor shows none of ([`shards_vmm::arch::x86_64::cpuid::Short`]).
    Bits {
        function: u32,
        index: u32,
        register: u8,
        missing: u32,
    },
}

/// What a machine whose hypervisor shows a guest `here` refuses of microVM `name`, whose
/// guest was told `baseline` (D176 part 6, D245): each field or register of it `here` has
/// less of, or another vendor's CPU.
fn baseline_refused(
    name: &str,
    here: &shards_vmm::arch::cpu::Id,
    baseline: &shards_vmm::arch::cpu::Id,
) -> Option<Refused> {
    shards_vmm::arch::cpu::covers(here, baseline)
        .err()
        .map(|lacks| cpu_refused(name.to_string(), lacks))
}

/// Whether this machine's hypervisor restores a snapshot only on the CPU it was taken on: WHP
/// on x86-64 (hv::whp's state.rs), which a move checks before anything moves (D245). WHP on
/// arm64 tells a guest its ID registers, as KVM and Hypervisor.framework do.
fn restores_on_its_own_cpu_alone() -> bool {
    cfg!(target_arch = "x86_64") && shards_vmm::hv::BACKEND == Some("whp")
}

/// What a machine whose hypervisor restores a snapshot only on the CPU it was taken on, and
/// whose CPU is `here`, refuses of microVM `name`, saved on `theirs` (D245): any other CPU.
fn identity_refused<T: PartialEq>(name: &str, here: &[T], theirs: &[T]) -> Option<Refused> {
    (here != theirs).then(|| Refused::CpuIdentity {
        name: name.to_string(),
    })
}

/// What a lease asked under fingerprint `asked` for microVM `name` is refused here: the key
/// its move replaced (`replaced`), under which no lease is served once it has moved (D176
/// part 6); `current`, its run's key here, where it has one.
fn key_refused(name: &str, asked: &str, replaced: Option<&str>, current: Option<&str>) -> Option<Refused> {
    (replaced == Some(asked)).then(|| Refused::KeyReplaced {
        name: name.to_string(),
        asked: asked.to_string(),
        current: current.map(str::to_string),
    })
}

/// What `lacks`, of this build's architecture, refuses of microVM `name`'s CPU baseline.
#[cfg(target_arch = "aarch64")]
fn cpu_refused(name: String, lacks: shards_vmm::arch::cpu::Lacks) -> Refused {
    let short = lacks
        .0
        .into_iter()
        .map(|s| CpuShort::Field {
            reg: s.reg,
            shift: s.shift,
            guest: s.guest,
            here: s.host,
        })
        .collect();
    Refused::Cpu { name, short }
}

/// What `lacks`, of this build's architecture, refuses of microVM `name`'s CPU baseline.
#[cfg(target_arch = "x86_64")]
fn cpu_refused(name: String, lacks: shards_vmm::arch::cpu::Lacks) -> Refused {
    use shards_vmm::arch::cpu::Lacks;
    match lacks {
        Lacks::Vendor => Refused::CpuVendor { name },
        Lacks::Bits(bits) => Refused::Cpu {
            name,
            short: bits
                .into_iter()
                .map(|s| CpuShort::Bits {
                    function: s.function,
                    index: s.index,
                    register: s.register,
                    missing: s.missing,
                })
                .collect(),
        },
    }
}

/// A hypervisor as `hv::BACKEND` names it, in a person's words.
fn hypervisor_name(backend: &str) -> &str {
    match backend {
        "kvm" => "KVM",
        "hvf" => "Hypervisor.framework",
        "whp" => "the Windows Hypervisor Platform",
        other => other,
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Arch { name, arch, here } => {
                write!(
                    f,
                    "{name} is a {arch} microVM, and this machine runs {here} microVMs"
                )
            }
            Refused::Hypervisor {
                name,
                hypervisor,
                here,
            } => {
                write!(f, "{name} ran on {}, and ", hypervisor_name(hypervisor))?;
                match here {
                    Some(here) => write!(f, "this machine runs microVMs on {}", hypervisor_name(here))?,
                    None => f.write_str("this machine runs no microVMs")?,
                }
                f.write_str(
                    ": a snapshot holds its vCPUs and interrupt controller as its hypervisor keeps them",
                )
            }
            Refused::Format { name, format, here } => write!(
                f,
                "{name} was saved in snapshot format {format}, and this machine's shards reads {here}"
            ),
            Refused::Cpu { name, short } => {
                // Said as each architecture's `covers` says it: a baseline's shorts are one
                // architecture's.
                let mut fields = Vec::new();
                let mut bits = Vec::new();
                for s in short {
                    match *s {
                        CpuShort::Field {
                            reg,
                            shift,
                            guest,
                            here,
                        } => fields.push(shards_vmm::arch::aarch64::cpu::Short {
                            reg,
                            shift,
                            guest,
                            host: here,
                        }),
                        CpuShort::Bits {
                            function,
                            index,
                            register,
                            missing,
                        } => bits.push(shards_vmm::arch::x86_64::cpuid::Short {
                            function,
                            index,
                            register,
                            missing,
                        }),
                    }
                }
                if !fields.is_empty() {
                    write!(f, "{}, ", shards_vmm::arch::aarch64::cpu::Lacks(fields))?;
                }
                if !bits.is_empty() {
                    write!(f, "{}, ", shards_vmm::arch::x86_64::cpuid::Lacks::Bits(bits))?;
                }
                write!(f, "which {name}'s CPU baseline names")
            }
            Refused::CpuVendor { name } => write!(
                f,
                "{name}'s CPU baseline is another vendor's CPU than this machine's: no CPUID makes one vendor's CPU another's"
            ),
            Refused::CpuIdentity { name } => write!(
                f,
                "this machine's CPU is not the one {name} was saved on, and the Windows Hypervisor Platform restores a snapshot only on that CPU"
            ),
            Refused::Features { name, lacks } => write!(
                f,
                "this machine's CPU lacks {}, which {name}'s guest may have used where it ran",
                lacks.join(", ")
            ),
            Refused::IdTaken { name } => write!(f, "a microVM with {name}'s ID is here already"),
            Refused::NameTaken { name } => write!(f, "a microVM named {name} is here already"),
            Refused::VolumeTaken { name, volume } => write!(
                f,
                "a volume named {volume} is here already, and {name}'s would move here"
            ),
            Refused::BindAbsent { name, path } => write!(
                f,
                "{path}, which {name} binds, is not on this machine: a move takes a bind's host path shared at the same place"
            ),
            Refused::Managed { name } => write!(
                f,
                "{name} has a managed scope, which a restore does not take yet: moved, it could run neither there nor here"
            ),
            Refused::KeyReplaced { name, asked, current } => match current {
                Some(current) => write!(
                    f,
                    "{asked} is the key {name} had before it moved, which no lease is served under: its key is {current}"
                ),
                None => write!(
                    f,
                    "{asked} is the key {name} had before it moved, which no lease is served under: it has no key here yet"
                ),
            },
            Refused::PortTaken {
                name,
                port,
                proto,
                host_ip,
                host_port,
                why,
            } => {
                let at = if host_ip.is_empty() {
                    format!("port {host_port}")
                } else {
                    format!("{host_ip}:{host_port}")
                };
                write!(
                    f,
                    "{name} publishes its port {port}/{proto} at {at}, which this machine cannot bind: {why}"
                )
            }
        }
    }
}

/// What a plan says of the machine its microVM ran on that refuses it on one whose guests
/// are of `arch`, whose VMs `hypervisor` drives and whose shards reads snapshot `format`,
/// before anything else is asked of it: a guest of another architecture, a machine of
/// another hypervisor, a snapshot of another format. A plan of an earlier build names no
/// hypervisor, and is taken as before, its restore refusing a state of another one.
fn platform_refused(plan: &Plan, arch: &str, hypervisor: Option<&str>, format: u32) -> Option<Refused> {
    let name = plan.name.clone();
    if plan.arch != arch {
        return Some(Refused::Arch {
            name,
            arch: plan.arch.clone(),
            here: arch.to_string(),
        });
    }
    if let Some(theirs) = plan.hypervisor.as_deref()
        && Some(theirs) != hypervisor
    {
        return Some(Refused::Hypervisor {
            name,
            hypervisor: theirs.to_string(),
            here: hypervisor.map(str::to_string),
        });
    }
    if plan.format != format {
        return Some(Refused::Format {
            name,
            format: plan.format,
            here: format,
        });
    }
    None
}

/// Why `ready` answers no [`Needs`]: it refuses the move, or it could not tell. A refusal is
/// boxed, as its variants carry what refused it whole and the answer is otherwise a string.
enum Unready {
    Refused(Box<Refused>),
    Failed(String),
}

impl From<String> for Unready {
    fn from(e: String) -> Unready {
        Unready::Failed(e)
    }
}

impl From<Refused> for Unready {
    fn from(refused: Refused) -> Unready {
        Unready::Refused(Box::new(refused))
    }
}

/// `bytes`' SHA-256, in hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of `path`'s bytes, in hex, and its length: read once for each file as it
/// is, by its device, inode, length and time of change, as a live move's last plan reads its
/// files' again as its microVM runs nowhere.
fn digest_of(path: &Path) -> io::Result<(String, u64)> {
    type Seen = std::collections::HashMap<(u64, u64, u64, i64, i64), String>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt as _;
        Some((
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        ))
    };
    #[cfg(not(unix))]
    let key: Option<(u64, u64, u64, i64, i64)> = None;
    if let Some(key) = key
        && let Some(sha) = lock(&SEEN).as_ref().and_then(|s| s.get(&key))
    {
        return Ok((sha.clone(), meta.len()));
    }
    let (sha, len) = digest_read(path)?;
    if let Some(key) = key {
        lock(&SEEN).get_or_insert_with(Seen::new).insert(key, sha.clone());
    }
    Ok((sha, len))
}

fn digest_read(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
        len = len.saturating_add(n as u64);
    }
    Ok((hex(&hasher.finalize()), len))
}

/// `rel` as a name relative to a home: `/`-separated normal components alone, none of them
/// `.`, `..`, empty or holding a NUL, each the guest's name of a host's file (a volume's
/// files' names are its guest's, [`guest::name`]) and named as this host keeps it
/// ([`guest::host_name`]: on Windows a character no Windows name holds escaped as WSL
/// escapes it). Anything else is refused, as it would reach out of the home.
fn relative(rel: &str) -> Result<PathBuf, String> {
    let refuse = || format!("{rel:?} names no file in a home");
    if rel.is_empty() || rel.starts_with('/') || rel.contains(['\0', '\\']) {
        return Err(refuse());
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(refuse());
        }
        out.push(guest::host_name(part.as_bytes()).map_err(|_| refuse())?);
    }
    match out.components().all(|c| matches!(c, Component::Normal(_))) {
        true => Ok(out),
        false => Err(refuse()),
    }
}

/// `path` relative to `home`, `/`-separated, where it is in it.
fn rel_of(home: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(home).ok()?;
    let parts: Option<Vec<&str>> = rest
        .components()
        .map(|c| match c {
            Component::Normal(p) => p.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A stream of a home's files, written by [`Writer`] and read by [`read_plan`] and [`read_entries`]: a magic,
/// a version, the plan, then entries, each its kind, its name relative to the home and what
/// it is, and an end. Each is as a share's guest sees it, through the share server's own
/// reading of its host ([`guest::read`]), whatever the host keeps a guest's owners, modes,
/// kinds and attributes in (D38, D197), so that a volume moved between any two hosts shows
/// its guest there what it showed it here: its kind and mode, its recorded owner, a
/// device's number, and its extended attributes by the guest's names ([`Writer::meta`]). A
/// file is that, its length and the runs of it that hold data, so that a sparse file (a
/// snapshot's memory, a writable layer) stays as sparse; nothing past its length or outside
/// its runs is read.
pub(crate) struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub(crate) fn new(mut out: W, plan: &Plan) -> io::Result<Writer<W>> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        let json = serde_json::to_vec(plan).map_err(io::Error::other)?;
        out.write_all(&[PLAN])?;
        out.write_all(&(json.len() as u64).to_le_bytes())?;
        out.write_all(&json)?;
        Ok(Writer { out })
    }

    fn name(&mut self, kind: u8, rel: &str) -> io::Result<()> {
        self.out.write_all(&[kind])?;
        self.out.write_all(&(rel.len() as u32).to_le_bytes())?;
        self.out.write_all(rel.as_bytes())
    }

    /// A file as its guest sees it: its mode (kind and permissions), its device number,
    /// whether its owner is recorded and the owner and group, then its extended attributes,
    /// each its name and value.
    fn meta(&mut self, meta: &guest::Meta) -> io::Result<()> {
        self.out.write_all(&meta.mode.to_le_bytes())?;
        self.out.write_all(&meta.rdev.to_le_bytes())?;
        let (recorded, (uid, gid)) = (meta.owner.is_some(), meta.owner.unwrap_or_default());
        self.out.write_all(&[u8::from(recorded)])?;
        self.out.write_all(&uid.to_le_bytes())?;
        self.out.write_all(&gid.to_le_bytes())?;
        let list = &meta.xattrs;
        self.out.write_all(&(list.len() as u32).to_le_bytes())?;
        for (name, value) in list {
            self.out.write_all(&(name.len() as u32).to_le_bytes())?;
            self.out.write_all(name)?;
            self.out.write_all(&(value.len() as u32).to_le_bytes())?;
            self.out.write_all(value)?;
        }
        Ok(())
    }

    /// The tree at `path`, named `rel` in the stream: a directory and all it holds, a file,
    /// a symlink, a FIFO or a device node, each named by its guest's name. `skip` leaves out
    /// the entries of a directory it names by `rel`.
    pub(crate) fn tree(&mut self, path: &Path, rel: &str, skip: &dyn Fn(&str) -> bool) -> io::Result<()> {
        let meta = guest::read(path)?;
        match meta.mode & guest::S_IFMT {
            guest::S_IFLNK => {
                let target = String::from_utf8(guest::read_link(path)?)
                    .map_err(|_| io::Error::other(format!("{}: a link that is not UTF-8", path.display())))?;
                self.name(LINK, rel)?;
                self.out.write_all(&(target.len() as u32).to_le_bytes())?;
                self.out.write_all(target.as_bytes())?;
                self.meta(&meta)
            }
            guest::S_IFDIR => {
                self.name(DIR, rel)?;
                self.meta(&meta)?;
                let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(path)?
                    .map(|e| {
                        let e = e?;
                        let name = String::from_utf8(guest::name(&e.file_name())?).map_err(|n| {
                            io::Error::other(format!("{:?}: a name that is not UTF-8", n.as_bytes()))
                        })?;
                        Ok((name, e.path()))
                    })
                    .collect::<io::Result<_>>()?;
                entries.sort();
                for (name, at) in entries {
                    let child = format!("{rel}/{name}");
                    if !skip(&child) {
                        self.tree(&at, &child, skip)?;
                    }
                }
                Ok(())
            }
            guest::S_IFREG => self.file(path, rel),
            guest::S_IFIFO | guest::S_IFCHR | guest::S_IFBLK => {
                self.name(NODE, rel)?;
                self.meta(&meta)
            }
            // A socket is its listener's, which a saved microVM has none of (as tar leaves
            // one out: GNU tar's "socket ignored").
            _ => Ok(()),
        }
    }

    /// The file at `path`, named `rel`: as its guest sees it ([`Writer::meta`]), its length
    /// and the runs of it that hold data.
    pub(crate) fn file(&mut self, path: &Path, rel: &str) -> io::Result<()> {
        let meta = guest::read(path)?;
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let runs = shards_vmm::platform::data_ranges(&file, len)?;
        self.name(FILE, rel)?;
        self.meta(&meta)?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(&(runs.len() as u64).to_le_bytes())?;
        let mut buf = vec![0u8; CHUNK];
        for (start, n) in runs {
            let n = n.min(len.saturating_sub(start));
            self.out.write_all(&start.to_le_bytes())?;
            self.out.write_all(&n.to_le_bytes())?;
            let mut at = start;
            let end = start.saturating_add(n);
            while at < end {
                let want = usize::try_from(end - at).map_or(CHUNK, |rest| rest.min(CHUNK));
                let chunk = buf.get_mut(..want).ok_or(io::ErrorKind::InvalidInput)?;
                shards_vmm::platform::read_exact_at(&file, chunk, at)?;
                self.out.write_all(chunk)?;
                at = at.saturating_add(want as u64);
            }
        }
        Ok(())
    }

    /// The stream's end: what reads it knows it whole.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.out.write_all(&[END])?;
        self.out.flush()
    }
}

/// An entry of a stream, as [`read_entries`] gives it to its taker.
#[expect(
    clippy::type_complexity,
    reason = "a file's runs read as its taker asks, each into what it gives"
)]
pub(crate) enum Entry<'a> {
    Dir {
        rel: String,
        meta: guest::Meta,
    },
    File {
        rel: String,
        meta: guest::Meta,
        len: u64,
        /// Its runs of data, each read into `put` at its offset.
        runs: &'a mut dyn FnMut(&mut dyn FnMut(u64, &[u8]) -> io::Result<()>) -> io::Result<()>,
    },
    Link {
        rel: String,
        target: String,
        meta: guest::Meta,
    },
    /// A FIFO or a device node.
    Node {
        rel: String,
        meta: guest::Meta,
    },
}

fn u32_of(r: &mut dyn Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn u64_of(r: &mut dyn Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// `n` bytes of `r`, at most `max`: a length past it is refused before anything is
/// allocated.
fn bytes_of(r: &mut dyn Read, n: u64, max: usize, what: &str) -> io::Result<Vec<u8>> {
    let n = usize::try_from(n)
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| io::Error::other(format!("{what}: {n} bytes, more than a move holds")))?;
    let mut out = vec![0u8; n];
    r.read_exact(&mut out)?;
    Ok(out)
}

fn text_of(r: &mut dyn Read, max: usize, what: &str) -> io::Result<String> {
    let n = u32_of(r)?;
    String::from_utf8(bytes_of(r, u64::from(n), max, what)?)
        .map_err(|_| io::Error::other(format!("{what}: not UTF-8")))
}

/// A file as its guest sees it, as [`Writer::meta`] wrote it.
fn meta_of(r: &mut dyn Read) -> io::Result<guest::Meta> {
    let mode = u32_of(r)?;
    let rdev = u32_of(r)?;
    let mut recorded = [0u8; 1];
    r.read_exact(&mut recorded)?;
    let (uid, gid) = (u32_of(r)?, u32_of(r)?);
    let owner = match recorded[0] {
        0 => None,
        1 => Some((uid, gid)),
        other => return Err(io::Error::other(format!("an owner's mark of {other}"))),
    };
    Ok(guest::Meta {
        mode,
        rdev,
        owner,
        xattrs: xattrs_of(r)?,
    })
}

fn xattrs_of(r: &mut dyn Read) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let n = u32_of(r)? as usize;
    if n > MAX_XATTRS {
        return Err(io::Error::other(format!("{n} extended attributes")));
    }
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let name_len = u32_of(r)?;
        let name = bytes_of(r, u64::from(name_len), MAX_XATTR_NAME, "an attribute's name")?;
        let value_len = u32_of(r)?;
        let value = bytes_of(r, u64::from(value_len), MAX_XATTR, "an attribute's value")?;
        out.push((name, value));
    }
    Ok(out)
}

/// Reads the head of a stream [`Writer`] wrote from `r`: its magic, version and plan.
pub(crate) fn read_plan(r: &mut dyn Read) -> Result<Plan, String> {
    let io = |e: io::Error| format!("the move's stream: {e}");
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(io)?;
    if &magic != MAGIC {
        return Err("the move's stream is not one".into());
    }
    let version = u32_of(r).map_err(io)?;
    if version != VERSION {
        return Err(format!(
            "the move's stream is of version {version}; this shards reads {VERSION}"
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind).map_err(io)?;
    if kind[0] != PLAN {
        return Err("the move's stream has no plan".into());
    }
    let n = u64_of(r).map_err(io)?;
    serde_json::from_slice(&bytes_of(r, n, MAX_PLAN, "its plan").map_err(io)?)
        .map_err(|e| format!("the move's plan: {e}"))
}

/// Reads the entries of a stream after its plan ([`read_plan`]) from `r`, each given to
/// `take` in order, a file's runs read as `take` asks for them. One that ends before its
/// end, or holds what no writer writes, is refused.
pub(crate) fn read_entries(
    r: &mut dyn Read,
    take: &mut dyn FnMut(Entry<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let io = |e: io::Error| format!("the move's stream: {e}");
    let mut kind = [0u8; 1];
    loop {
        r.read_exact(&mut kind).map_err(io)?;
        match kind[0] {
            END => return Ok(()),
            DIR => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let meta = meta_of(r).map_err(io)?;
                take(Entry::Dir { rel, meta })?;
            }
            LINK => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let target = text_of(r, MAX_PATH, "a link").map_err(io)?;
                let meta = meta_of(r).map_err(io)?;
                take(Entry::Link { rel, target, meta })?;
            }
            NODE => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let meta = meta_of(r).map_err(io)?;
                take(Entry::Node { rel, meta })?;
            }
            FILE => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let meta = meta_of(r).map_err(io)?;
                let len = u64_of(r).map_err(io)?;
                let count = u64_of(r).map_err(io)?;
                let mut read_all = false;
                let mut runs = |put: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>| -> io::Result<()> {
                    let mut buf = vec![0u8; CHUNK];
                    let mut last = 0u64;
                    for _ in 0..count {
                        let start = u64_of(r)?;
                        let n = u64_of(r)?;
                        let end = start
                            .checked_add(n)
                            .filter(|end| *end <= len && start >= last)
                            .ok_or_else(|| io::Error::other("a run of data out of its file"))?;
                        last = end;
                        let mut at = start;
                        while at < end {
                            let want = usize::try_from(end - at).map_or(CHUNK, |rest| rest.min(CHUNK));
                            let chunk = buf.get_mut(..want).ok_or(io::ErrorKind::InvalidInput)?;
                            r.read_exact(chunk)?;
                            put(at, chunk)?;
                            at = at.saturating_add(want as u64);
                        }
                    }
                    read_all = true;
                    Ok(())
                };
                take(Entry::File {
                    rel,
                    meta,
                    len,
                    runs: &mut runs,
                })?;
                if !read_all {
                    return Err("a file's data was left unread".into());
                }
            }
            other => return Err(format!("the move's stream holds an entry of kind {other}")),
        }
    }
}

/// Puts a stream's entries under `root`, a directory of the move's own: each name checked
/// to be one [`relative`] takes, and to be one the plan allows (`allowed`), made new, never
/// over anything there; each made and given what its guest saw ([`guest::set`]) as the
/// share server makes and keeps a guest's, so that this host's server shows it the same.
/// A directory is given its own once all it holds is in it ([`Placer::finish`]), so that a
/// mode that keeps its owner out (a guest's `0555`, kept as such where no owner of it is
/// recorded) is no bar to what goes in it.
struct Placer<'a> {
    root: &'a Path,
    allowed: &'a dyn Fn(&str) -> bool,
    dirs: Vec<(PathBuf, guest::Meta)>,
}

impl<'a> Placer<'a> {
    fn new(root: &'a Path, allowed: &'a dyn Fn(&str) -> bool) -> Placer<'a> {
        Placer {
            root,
            allowed,
            dirs: Vec::new(),
        }
    }

    fn path(&self, rel: &str) -> Result<PathBuf, String> {
        if !(self.allowed)(rel) {
            return Err(format!(
                "the move's stream holds {rel}, which its plan does not name"
            ));
        }
        Ok(self.root.join(relative(rel)?))
    }

    fn take(&mut self, entry: Entry<'_>) -> Result<(), String> {
        let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        match entry {
            Entry::Dir { rel, meta } => {
                let path = self.path(&rel)?;
                make_parent(&path)?;
                guest::make_dir(&path).map_err(|e| at(&path, &e))?;
                self.dirs.push((path, meta));
                Ok(())
            }
            Entry::Link { rel, target, meta } => {
                let path = self.path(&rel)?;
                make_parent(&path)?;
                guest::make_link(&path, target.as_bytes()).map_err(|e| at(&path, &e))?;
                guest::set(&path, &meta).map_err(|e| at(&path, &e))
            }
            Entry::Node { rel, meta } => {
                let path = self.path(&rel)?;
                make_parent(&path)?;
                guest::make_node(&path, &meta).map_err(|e| at(&path, &e))?;
                guest::set(&path, &meta).map_err(|e| at(&path, &e))
            }
            Entry::File { rel, meta, len, runs } => {
                if meta.mode & guest::S_IFMT != guest::S_IFREG {
                    return Err(format!("{rel}: a file of mode {:o}", meta.mode));
                }
                let path = self.path(&rel)?;
                make_parent(&path)?;
                let file = crate::os::owner_only(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true),
                )
                .open(&path)
                .map_err(|e| at(&path, &e))?;
                shards_vmm::platform::make_sparse(&file).map_err(|e| at(&path, &e))?;
                file.set_len(len).map_err(|e| at(&path, &e))?;
                runs(&mut |offset, bytes| {
                    // Only what is not all zeros: the file stays as sparse as it was.
                    if bytes.iter().any(|&b| b != 0) {
                        shards_vmm::platform::write_all_at(&file, bytes, offset)
                    } else {
                        Ok(())
                    }
                })
                .map_err(|e| at(&path, &e))?;
                drop(file);
                guest::set(&path, &meta).map_err(|e| at(&path, &e))
            }
        }
    }

    /// Each directory given what its guest saw, the deepest first, once the stream's
    /// entries are all in place.
    fn finish(self) -> Result<(), String> {
        for (path, meta) in self.dirs.iter().rev() {
            guest::set(path, meta).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }
}

fn make_parent(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display())),
        None => Ok(()),
    }
}

/// Where in a home a move's stream may put what it holds: the microVM's own directory, its
/// volumes', and the files the plan says the other machine lacks.
fn allowed(plan: &Plan, needed: &BTreeSet<String>, rel: &str) -> bool {
    let under = |root: &str| rel == root || rel.starts_with(&format!("{root}/"));
    under(&format!("containers/{}", plan.id))
        || plan.volumes.iter().any(|v| under(&format!("volumes/{v}")))
        || needed.contains(rel)
}

impl<D: Disk> Daemon<D> {
    /// `shards move vm`'s steps (D176 part 6), for its client (cli/moves.rs) or a context's
    /// (remote.rs): `args` after [`shards_ipc::MOVE_STEP`].
    pub(super) fn move_step(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        // A refusal goes typed on stdout, for the mover to match, and in words on stderr, as
        // any step's failure does, for a client of an earlier build.
        let typed = |done: Result<String, Unready>| match done {
            Ok(said) => Ok(said),
            Err(Unready::Failed(e)) => Err(e),
            Err(Unready::Refused(refused)) => {
                if let Ok(json) = serde_json::to_string(&refused) {
                    reply.out(&json);
                }
                Err(refused.to_string())
            }
        };
        let done = match args.first().map(String::as_str) {
            Some("save") => typed(self.move_save(args.get(1).map_or("", String::as_str))),
            Some("plan") => typed(self.move_plan(args.get(1).map_or("", String::as_str))),
            Some("live") => self.move_live(args.get(1..).unwrap_or_default(), asker, reply),
            Some("done") => self.move_done(args.get(1..).unwrap_or_default()),
            Some("ready") => typed(self.move_ready(args.get(1).map_or("", String::as_str), asker, reply)),
            Some("key") => typed(self.move_key(args.get(1..).unwrap_or_default())),
            Some("cpu") if restores_on_its_own_cpu_alone() => {
                Err(shards_vmm::vm::NO_BASELINE_ON_WHP.to_string())
            }
            Some("cpu") => self.host_cpu_id().map(|id| shards_vmm::arch::cpu::encode(&id)),
            Some("export") => self.move_export(args.get(1..).unwrap_or_default(), asker),
            Some("import") => self.move_import(args.get(1..).unwrap_or_default(), asker),
            _ => Err(format!("no step of a move in {args:?}")),
        };
        match done {
            Ok(said) => {
                if !said.is_empty() {
                    reply.out(&said);
                }
                0
            }
            Err(e) => {
                reply.err(&format!("shards: {e}"));
                1
            }
        }
    }

    /// `save NAME`: the microVM saved, stopping it, or the snapshot a save stopped it as;
    /// its plan, as JSON.
    fn move_save(&self, reference: &str) -> Result<String, Unready> {
        let id = self.resolve(reference)?;
        if let Some(refused) = self.unmovable(&id) {
            return Err(refused.into());
        }
        self.await_start(&id);
        // On WHP its plan names this CPU (`exact_cpu`), asked of the VM process once a
        // daemon: asked before the save, not in the time the microVM runs nowhere.
        let _ = self.exact_cpu();
        let snapshot = if self.running(&id) {
            let (_, name) = self
                .save_snapshot(reference, None, false)
                .map_err(|e| e.ours(reference).trim_start_matches("shards: ").to_string())?;
            name
        } else {
            let saved = lock(&self.containers).get(&id).and_then(|c| c.saved.clone());
            saved.ok_or_else(|| {
                format!("{reference} is neither running nor stopped by a save: nothing of it to move")
            })?
        };
        let plan = self.plan(&id, &snapshot)?;
        serde_json::to_string(&plan).map_err(|e| Unready::Failed(e.to_string()))
    }

    /// `plan NAME`: what another machine needs of the running microVM, before a live move
    /// (D176 part 7): its image's root filesystem by its digest, the file a save of it
    /// restores against; any other its last save names is sent as the move ends. The
    /// snapshot a live move saves it as, by the time.
    fn move_plan(&self, reference: &str) -> Result<String, Unready> {
        let id = self.resolve(reference)?;
        if let Some(refused) = self.unmovable(&id) {
            return Err(refused.into());
        }
        self.await_start(&id);
        if !self.running(&id) {
            return Err(format!(
                "{reference} is not running: a live move moves a running microVM"
            ));
        }
        let c = lock(&self.containers)
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no container {id}"))?;
        let home = std::fs::canonicalize(&self.home).map_err(|e| format!("{}: {e}", self.home.display()))?;
        let mut files = Vec::new();
        let cancel = shards_registry::http::Cancel::new();
        let image = c.image_id.clone().unwrap_or_else(|| c.image.clone());
        if let Ok((found, _lease)) = crate::run::find_image(
            &image,
            shards_ipc::Pull::Never,
            &[],
            &[],
            &self.home,
            &|_| {},
            &cancel,
        ) && let Some(rootfs) = found.rootfs
        {
            let resolved = std::fs::canonicalize(&rootfs).unwrap_or(rootfs);
            if let Some(rel) = rel_of(&home, &resolved) {
                let (sha256, size) =
                    digest_of(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))?;
                files.push(Backing {
                    path: resolved.to_string_lossy().into_owned(),
                    rel,
                    size,
                    sha256,
                });
            }
        }
        let mut volumes = Vec::new();
        let mut binds = Vec::new();
        for m in &c.mounts {
            match m.kind.as_str() {
                "volume" if !m.name.is_empty() => volumes.push(m.name.clone()),
                "bind" => binds.push(m.source.clone()),
                _ => {}
            }
        }
        volumes.sort();
        volumes.dedup();
        let now = i64::try_from(crate::containers::now() / 1_000_000_000).unwrap_or(i64::MAX);
        let snapshot = format!(
            "moved-{}",
            shards_cmdline::format::rfc3339_at(now, 0).replace(':', "-")
        );
        let (published, network) = self.published_of(&c);
        let plan = Plan {
            id: c.id.clone(),
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone().unwrap_or_default(),
            arch: std::env::consts::ARCH.into(),
            format: shards_vmm::snapshot::FORMAT,
            hypervisor: shards_vmm::hv::BACKEND.map(str::to_string),
            snapshot,
            home: home.to_string_lossy().into_owned(),
            files,
            volumes,
            binds,
            cpu: host_cpu(),
            guest: self.boots_stored_guest(&c.id),
            baseline: read_baseline(&lock(&self.containers).dir(&c.id).join(RUN_BASELINE)),
            cpu_identity: self.exact_cpu(),
            published,
            network,
            key: self.run_key(&c.id),
        };
        serde_json::to_string(&plan).map_err(|e| Unready::Failed(e.to_string()))
    }

    /// `live NAME SNAPSHOT [--need FILE]... [--checked FILE]...`: the running microVM moved
    /// live (D176 part 7), written to the client's descriptor as it goes: its memory, layer
    /// and swap copied round after round as its guest runs on; then it saved as SNAPSHOT,
    /// held, what its guest wrote of those three since the last round sent as the last;
    /// then its move's stream, as `export`'s, the files the other machine lacks (`--need`)
    /// and any its last save names that the other machine was not asked of (not
    /// `--checked`) with it.
    /// Says on stderr when its save was asked, as the time since the epoch in ns, and what
    /// each round sent.
    fn move_live(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> Result<String, String> {
        let (Some(reference), Some(snapshot)) = (args.first(), args.get(1)) else {
            return Err("live: a microVM and its snapshot".into());
        };
        let (mut needed, mut checked) = (Vec::new(), BTreeSet::new());
        let mut rest = args.get(2..).unwrap_or_default().iter();
        while let Some(flag) = rest.next() {
            match (flag.as_str(), rest.next()) {
                ("--need", Some(rel)) => needed.push(rel.clone()),
                ("--checked", Some(rel)) => {
                    checked.insert(rel.clone());
                }
                _ => return Err(format!("live: {flag:?}")),
            }
        }
        let out = asker
            .files
            .first()
            .ok_or("live: the client sent nowhere to write")?;
        let id = self.resolve(reference)?;
        self.await_start(&id);
        let (inbox, socket) = match lock(&self.runs).get(&id) {
            Some(super::RunState::Tracked(t)) => (t.inbox.clone(), t.socket.clone()),
            _ => return Err(format!("{reference} is not running")),
        };
        if self.joined_run(&id) {
            return Err(format!(
                "{reference} runs in another container's microVM: move that microVM"
            ));
        }
        if lock(&inbox).host_net.is_some() {
            return Err(format!(
                "{reference} is on the host's network, whose sockets of the host's no snapshot keeps"
            ));
        }
        // The rounds, written by its VM straight into the client's descriptor.
        let (told, answer) = std::sync::mpsc::channel();
        {
            let mut inbox = lock(&inbox);
            if inbox.ended {
                return Err(format!("{reference} is not running"));
            }
            if inbox.precopying.is_some() || inbox.saving.is_some() {
                return Err(format!("{reference} is being saved already"));
            }
            inbox.precopying = Some(told);
        }
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&LIVE_ROUNDS.to_be_bytes());
        payload.extend_from_slice(&LIVE_ENOUGH.to_be_bytes());
        use shards_ipc::fd::AsFd as _;
        if let Err(e) = socket.send(shards_ipc::kind::PRECOPY, &payload, &[out.as_fd()]) {
            lock(&inbox).precopying = None;
            return Err(format!("asking its microVM: {e}"));
        }
        // Each round said as it ends, and the bytes the stream carried as it carries them,
        // in the client's answer and in the daemon's log: what one waiting on the move
        // follows, however slowly its host or its link goes (D209), never a clock.
        let rounds = loop {
            match answer.recv() {
                Ok(Rounds::Going(going)) => {
                    let said: Vec<&str> = going.split_whitespace().collect();
                    let line = match said.as_slice() {
                        ["round", n, pages, bytes, us] => {
                            format!("move: round {n}: {pages} pages, {bytes} bytes, {us} us")
                        }
                        ["sent", bytes] => format!("move: carried {bytes} bytes of its rounds"),
                        _ => continue,
                    };
                    super::log(format!("{reference}: {line}"));
                    reply.err(&line);
                }
                Ok(Rounds::Done(rounds)) => break rounds?,
                Err(_) => return Err("its microVM stopped".into()),
            }
        };
        let mut lines = rounds.lines();
        if let Some(paused) = lines.next() {
            reply.err(&format!(
                "move: paused {paused} us to find its layer's and swap's data"
            ));
        }
        let mut sent = Vec::new();
        for line in lines {
            let said: Vec<&str> = line.split_whitespace().collect();
            if let [_pages, bytes, _us] = said.as_slice() {
                sent.push(bytes.parse::<u64>().unwrap_or(0));
            }
        }
        // Why its rounds ended, as its VM judged them.
        let why = shards_vmm::vm::live::Stop::after(&sent, LIVE_ROUNDS, LIVE_ENOUGH)
            .map_or("its VM ended them", shards_vmm::vm::live::Stop::said);
        reply.err(&format!(
            "move: rounds {}, {} bytes, ended: {why}",
            sent.len(),
            sent.iter().fold(0u64, |a, b| a.saturating_add(*b))
        ));
        // Its last save, its guest's writes since the last round sent by its VM as the
        // rounds' last, and their end, before it answers.
        let asked = crate::containers::now();
        reply.err(&format!("move: saved at {asked}"));
        // Held, its vCPUs parked and its devices quiet, until the other machine has it or
        // has not (`done`).
        let t0 = std::time::Instant::now();
        self.save_snapshot_as(reference, Some(snapshot), true, Some(out.as_fd()))
            .map_err(|e| e.ours(reference).trim_start_matches("shards: ").to_string())?;
        reply.err(&format!(
            "move: its last save took {} us",
            t0.elapsed().as_micros()
        ));
        // Then the rest, after the rounds in the same stream.
        let plan = self.plan(&id, snapshot)?;
        let mut send: Vec<String> = needed;
        send.extend(
            plan.files
                .iter()
                .filter(|f| !checked.contains(&f.rel))
                .map(|f| f.rel.clone()),
        );
        let mut export = vec![reference.clone(), snapshot.clone()];
        export.extend(send);
        let t1 = std::time::Instant::now();
        let sent = self.move_export(&export, asker);
        reply.err(&format!(
            "move: its last stream took {} us",
            t1.elapsed().as_micros()
        ));
        sent
    }

    /// `done NAME SNAPSHOT moved|back`: a live move's end (D176 part 7). Moved, the microVM
    /// its last save holds stops as saved; back, the other machine did not take it, and it
    /// goes on here, the snapshot of what its guest wrote since the move's last round, which
    /// nothing restores, removed.
    fn move_done(&self, args: &[String]) -> Result<String, String> {
        let (Some(reference), Some(snapshot), Some(how)) = (args.first(), args.get(1), args.get(2)) else {
            return Err("done: a microVM, its snapshot, and moved or back".into());
        };
        let id = self.resolve(reference)?;
        let moved = match how.as_str() {
            "moved" => true,
            "back" => false,
            other => return Err(format!("done: {other:?}")),
        };
        let socket = match lock(&self.runs).get(&id) {
            Some(super::RunState::Tracked(t)) => t.socket.clone(),
            _ => return Err(format!("{reference} is not running")),
        };
        socket
            .send(shards_ipc::kind::MOVED, &[u8::from(moved)], &[])
            .map_err(|e| format!("telling its microVM: {e}"))?;
        if !moved {
            let at = lock(&self.containers).dir(&id).join(SNAPSHOTS).join(snapshot);
            if at.exists() {
                shards_vmm::snapshot::remove(&at).map_err(|e| format!("{}: {e}", at.display()))?;
            }
        }
        Ok(String::new())
    }

    /// What another machine needs of microVM `id`, saved as `snapshot`, to take it.
    fn plan(&self, id: &str, snapshot: &str) -> Result<Plan, String> {
        let c = lock(&self.containers)
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no container {id}"))?;
        let home = std::fs::canonicalize(&self.home).map_err(|e| format!("{}: {e}", self.home.display()))?;
        let dir = lock(&self.containers).dir(id).join(SNAPSHOTS).join(snapshot);
        let pinned = shards_vmm::snapshot::read(&dir)?;
        let mut files = Vec::new();
        for (path, _) in shards_vmm::snapshot::backing_of(&pinned) {
            // As the home is named, links and all: macOS's /var is /private/var.
            let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let rel = rel_of(&home, &resolved).ok_or_else(|| {
                format!(
                    "{}: its snapshot restores against a file outside this machine's home, which no move takes",
                    path.display()
                )
            })?;
            let (sha256, size) = digest_of(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            files.push(Backing {
                path: path.to_string_lossy().into_owned(),
                rel,
                size,
                sha256,
            });
        }
        drop(pinned);
        let mut volumes = Vec::new();
        let mut binds = Vec::new();
        for m in &c.mounts {
            match m.kind.as_str() {
                "volume" if !m.name.is_empty() => volumes.push(m.name.clone()),
                "bind" => binds.push(m.source.clone()),
                _ => {}
            }
        }
        volumes.sort();
        volumes.dedup();
        let (published, network) = self.published_of(&c);
        Ok(Plan {
            id: c.id.clone(),
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone().unwrap_or_default(),
            arch: std::env::consts::ARCH.into(),
            format: shards_vmm::snapshot::FORMAT,
            hypervisor: shards_vmm::hv::BACKEND.map(str::to_string),
            snapshot: snapshot.to_string(),
            home: home.to_string_lossy().into_owned(),
            files,
            volumes,
            binds,
            cpu: host_cpu(),
            guest: self.boots_stored_guest(id),
            baseline: read_baseline(&lock(&self.containers).dir(id).join(RUN_BASELINE)),
            cpu_identity: self.exact_cpu(),
            published,
            network,
            key: self.run_key(id),
        })
    }

    /// The CPU a snapshot saved here may be restored on alone, for a plan to carry (D245):
    /// this machine's, where its hypervisor restores a snapshot only on the CPU it was taken
    /// on (WHP on x86-64); none elsewhere. Where its VM process cannot say, none: the machine moved to
    /// then finds another CPU at the restore, as with a plan of an earlier build.
    fn exact_cpu(&self) -> shards_vmm::arch::cpu::Id {
        if !restores_on_its_own_cpu_alone() {
            return Default::default();
        }
        self.host_cpu_id().unwrap_or_else(|e| {
            super::log(format!("a move's plan names no CPU: {e}"));
            Default::default()
        })
    }

    /// This machine's CPU as its hypervisor shows a guest (D176 part 6): its VM process asked
    /// (`shards-vm cpu`), once a daemon, until it answers.
    fn host_cpu_id(&self) -> Result<shards_vmm::arch::cpu::Id, String> {
        static HOST: std::sync::OnceLock<shards_vmm::arch::cpu::Id> = std::sync::OnceLock::new();
        if let Some(id) = HOST.get() {
            return Ok(id.clone());
        }
        let asked = (|| {
            use shards_ipc::fd::AsFd as _;
            let (mut reader, writer) = io::pipe().map_err(|e| e.to_string())?;
            let stderr = io::stderr();
            let child = shards_ipc::spawn(
                &self.vm,
                &[std::ffi::OsStr::new("cpu")],
                &[(writer.as_fd(), 1), (stderr.as_fd(), 2)],
                false,
            )
            .map_err(|e| format!("{}: {e}", self.vm.display()))?;
            drop(writer);
            let mut out = String::new();
            let read = reader.read_to_string(&mut out);
            let status = child.wait().map_err(|e| e.to_string())?;
            read.map_err(|e| e.to_string())?;
            if status != 0 {
                return Err(format!(
                    "its VM process could not say this host's CPU (status {status})"
                ));
            }
            shards_vmm::arch::cpu::decode(out.trim())
        })()?;
        Ok(HOST.get_or_init(|| asked).clone())
    }

    /// Why container `id` may not move at all, refused before anything of it moves: a managed
    /// scope, which a restore refuses (the start path's "cannot yet be restored"), so that a
    /// cold move's save would leave it stopped here, its resume refused as well.
    fn unmovable(&self, id: &str) -> Option<Refused> {
        let (dir, name) = {
            let containers = lock(&self.containers);
            let name = containers
                .get(id)
                .map_or_else(|| id.to_string(), |c| c.name.clone());
            (containers.dir(id), name)
        };
        super::managed::has_scope(&dir).then_some(Refused::Managed { name })
    }

    /// Container `id`'s run's own key, as its managed metadata names it, in lowercase hex.
    fn run_key(&self, id: &str) -> Option<String> {
        let dir = lock(&self.containers).dir(id);
        super::managed::vm_key(&dir).map(|k| shards_remote::identity::hex(&k))
    }

    /// `key NAME [ASKED]`, on the machine a microVM moved to (D176 part 6): its run's own key
    /// and the key its move replaced, as JSON (`key`, `replaces`: lowercase hex, or null);
    /// given ASKED, the fingerprint a lease is asked under, refused typed where it is the
    /// replaced one.
    fn move_key(&self, args: &[String]) -> Result<String, Unready> {
        let (name, asked) = match args {
            [name] => (name, None),
            [name, asked] => (name, Some(asked.as_str())),
            other => return Err(format!("key: {other:?}").into()),
        };
        let id = self.resolve(name)?;
        let current = self.run_key(&id);
        let replaced = std::fs::read_to_string(lock(&self.containers).dir(&id).join(KEY_REPLACED))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        if let Some(asked) = asked
            && let Some(refused) = key_refused(name, asked, replaced.as_deref(), current.as_deref())
        {
            return Err(refused.into());
        }
        Ok(serde_json::json!({ "key": current, "replaces": replaced }).to_string())
    }

    /// The ports container `c`'s run publishes on this machine's addresses, as its request
    /// asked for them, and its network: none where its run bound no host port (no `-p`, or a
    /// network whose members' ports are not the host's).
    fn published_of(&self, c: &Container) -> (Vec<Published>, String) {
        if !c.ports.iter().any(|p| p.public != 0) {
            return (Vec::new(), String::new());
        }
        let dir = lock(&self.containers).dir(&c.id);
        let Some(run) = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
        else {
            return (Vec::new(), String::new());
        };
        let published = run
            .publish
            .iter()
            .map(|p| Published {
                port: p.port,
                proto: p.proto.clone(),
                host_ip: p.host_ip.clone(),
                host_port: p.host_port.clone(),
            })
            .collect();
        (published, run.network)
    }

    /// The first port `plan`'s microVM publishes that this machine cannot bind as a start
    /// binds it (`publish::bind`: the address its binding names, else its network's, else
    /// every one; a port of its range; this daemon's own runs' addresses as `in_use` says),
    /// refused by what it is. The sockets go at once: its restore binds them again, and stays
    /// what fails the move should another program take one meanwhile.
    fn ports_refused(&self, plan: &Plan) -> Option<Refused> {
        let default = self.user_network(&plan.network).and_then(|n| n.host_binding());
        plan.published.iter().find_map(|p| {
            let binding = shards_ipc::Publish {
                port: p.port,
                proto: p.proto.clone(),
                host_ip: p.host_ip.clone(),
                host_port: p.host_port.clone(),
            };
            super::publish::bind(
                std::slice::from_ref(&binding),
                &[],
                default,
                super::publish::v6_listenable(),
                |at, proto| self.in_use(at, proto),
            )
            .err()
            .map(|why| Refused::PortTaken {
                name: plan.name.clone(),
                port: p.port,
                proto: p.proto.clone(),
                host_ip: p.host_ip.clone(),
                host_port: p.host_port.clone(),
                why,
            })
        })
    }

    /// Whether container `id`'s run boots the guest its home stores: its request names no
    /// kernel (`--kernel`, `SHARDS_KERNEL`).
    fn boots_stored_guest(&self, id: &str) -> bool {
        let dir = lock(&self.containers).dir(id);
        std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .is_some_and(|r| r.kernel.is_none() && r.init.is_none())
    }

    /// `ready PLAN`, on the machine moved to: whether it can take the plan's microVM, and
    /// what it lacks, as JSON: its image where it has none and none could be pulled, and the
    /// files its snapshot restores against that it has none of, or another of.
    fn move_ready(&self, plan: &str, asker: &Asker, reply: &Reply<'_>) -> Result<String, Unready> {
        let plan: Plan = serde_json::from_str(plan).map_err(|e| format!("the move's plan: {e}"))?;
        let name = || plan.name.clone();
        if let Some(refused) = platform_refused(
            &plan,
            std::env::consts::ARCH,
            shards_vmm::hv::BACKEND,
            shards_vmm::snapshot::FORMAT,
        ) {
            return Err(refused.into());
        }
        // On WHP its CPU first: a restore there takes only the CPU its snapshot was saved on
        // (hv::whp's state.rs), checked here rather than after the whole stream (D245).
        if !plan.cpu_identity.is_empty()
            && restores_on_its_own_cpu_alone()
            && let Some(refused) = identity_refused(&plan.name, &self.host_cpu_id()?, &plan.cpu_identity)
        {
            return Err(refused.into());
        }
        // Its CPU: every feature of the machine it ran on, as its guest may have used any.
        // Told a baseline, each of its fields this machine's hypervisor shows at least; and of
        // the features it ran on, those the baseline cannot mask, by name.
        if !plan.baseline.is_empty()
            && let Some(refused) = baseline_refused(&plan.name, &self.host_cpu_id()?, &plan.baseline)
        {
            return Err(refused.into());
        }
        let here = host_cpu();
        let may_use = unmasked(&plan.cpu, &plan.baseline);
        let lacks = cpu_lacks(&may_use, &here);
        if !lacks.is_empty() {
            let lacks = lacks.into_iter().map(str::to_string).collect();
            return Err(Refused::Features { name: name(), lacks }.into());
        }
        {
            let registry = lock(&self.containers);
            if registry.get(&plan.id).is_some() || registry.dir(&plan.id).exists() {
                return Err(Refused::IdTaken { name: name() }.into());
            }
            if registry.name_taken(&plan.name).is_some() {
                return Err(Refused::NameTaken { name: name() }.into());
            }
        }
        let store = crate::volumes::Store::new(&self.home);
        if let Some(volume) = plan.volumes.iter().find(|v| store.get(v).is_some()) {
            return Err(Refused::VolumeTaken {
                name: name(),
                volume: volume.clone(),
            }
            .into());
        }
        if let Some(bind) = plan.binds.iter().find(|b| std::fs::symlink_metadata(b).is_err()) {
            return Err(Refused::BindAbsent {
                name: name(),
                path: bind.clone(),
            }
            .into());
        }
        // Its published ports, which its restore binds again as a start does: one this
        // machine cannot bind failed the move only after the whole stream.
        if let Some(refused) = self.ports_refused(&plan) {
            return Err(refused.into());
        }
        // Its image, by the digest its name resolved to: here, or pulled by it.
        let image = self.moved_image(&plan, asker, reply);
        // The guest a restore here prepares its run's request with (run::prepare, as a
        // start does), where its run boots the stored one: the pinned kernel, downloaded
        // where this home has none recorded, now rather than in its restore, which a live
        // move's microVM waits for running nowhere. A fresh home's first move waited 17 s
        // in its restore for the kernel's 19 MB from GitHub (PM M462).
        if plan.guest && crate::guest::current(&self.home)?.is_none() {
            let cancel = shards_registry::http::Cancel::new();
            crate::guest::default(&self.home, &|line| reply.err(line), Some(&cancel), &|k| {
                shards_ipc::env_value(&asker.registry_env, k)
            })?;
        }
        let mut needs = Needs {
            image: image.is_err(),
            files: Vec::new(),
            unpacks: vec![crate::packing::CODEC.to_string()],
        };
        for file in &plan.files {
            let here = self.home.join(relative(&file.rel)?);
            let same = match std::fs::metadata(&here) {
                Ok(m) if m.len() == file.size => {
                    digest_of(&here).is_ok_and(|(sha256, _)| sha256 == file.sha256)
                }
                _ => false,
            };
            if !same {
                needs.files.push(file.rel.clone());
            }
        }
        Ok(serde_json::to_string(&needs).map_err(|e| e.to_string())?)
    }

    /// The plan's image here, by its ID, or pulled by the digest its name resolved to.
    fn moved_image(&self, plan: &Plan, asker: &Asker, reply: &Reply<'_>) -> Result<(), String> {
        if plan.image_id.is_empty() {
            return Err("it names no image by digest".into());
        }
        let cancel = shards_registry::http::Cancel::new();
        let say = |line: &str| reply.err(line);
        if crate::run::find_image(
            &plan.image_id,
            shards_ipc::Pull::Never,
            &[],
            &asker.registry_env,
            &self.home,
            &say,
            &cancel,
        )
        .is_ok()
        {
            return Ok(());
        }
        let mut reference =
            shards_image::reference::Reference::parse(&plan.image).map_err(|e| e.to_string())?;
        reference.tag = None;
        reference.digest =
            Some(shards_image::reference::Digest::parse(&plan.image_id).map_err(|e| e.to_string())?);
        crate::run::find_image(
            &reference.to_string(),
            shards_ipc::Pull::Missing,
            &[],
            &asker.registry_env,
            &self.home,
            &say,
            &cancel,
        )
        .map(|_| ())
    }

    /// `export NAME SNAPSHOT [FILE...]`: microVM NAME's directory but its other snapshots,
    /// its named volumes, and the files of the home named, written as a move's stream to the
    /// client's descriptor.
    fn move_export(&self, args: &[String], asker: &Asker) -> Result<String, String> {
        let (Some(reference), Some(snapshot)) = (args.first(), args.get(1)) else {
            return Err("export: a microVM and its snapshot".into());
        };
        let out = asker
            .files
            .first()
            .ok_or("export: the client sent nowhere to write")?
            .try_clone()
            .map_err(|e| e.to_string())?;
        let id = self.resolve(reference)?;
        let plan = self.plan(&id, snapshot)?;
        let home = PathBuf::from(&plan.home);
        let wanted: BTreeSet<&str> = args
            .get(2..)
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect();
        let fail = |e: io::Error| format!("writing the move: {e}");
        let mut w = Writer::new(io::BufWriter::with_capacity(CHUNK, File::from(out)), &plan).map_err(fail)?;
        let dir = lock(&self.containers).dir(&id);
        let own = format!("containers/{id}");
        let snapshots = format!("{own}/{SNAPSHOTS}/");
        let keep = format!("{snapshots}{snapshot}");
        // Its other snapshots stay; so does the writable layer its microVM left it as it
        // stopped (D190), which its snapshot keeps as it was, and its restore takes.
        let kept_layer = format!("{own}/{}", super::layers::KEPT);
        w.tree(&dir, &own, &|rel| {
            rel == kept_layer
                || rel
                    .strip_prefix(&snapshots)
                    .is_some_and(|rest| !rest.contains('/') && rel != keep)
        })
        .map_err(fail)?;
        let store = crate::volumes::Store::new(&self.home);
        for v in &plan.volumes {
            let root = store.data(v);
            let at = root.parent().ok_or("a volume's directory")?;
            w.tree(at, &format!("volumes/{v}"), &|_| false).map_err(fail)?;
        }
        for file in &plan.files {
            if wanted.contains(file.rel.as_str()) {
                w.file(&home.join(relative(&file.rel)?), &file.rel)
                    .map_err(fail)?;
            }
        }
        w.finish().map_err(fail)?;
        Ok(String::new())
    }

    /// `import [--packed zstd]`: a move's stream read from the client's descriptor, unpacked
    /// where it was packed (`crate::packing`), its files put in place, its snapshot made to
    /// restore against this machine's files, and the microVM made here, stopped as saved. Its
    /// name, once it is.
    fn move_import(&self, args: &[String], asker: &Asker) -> Result<String, String> {
        let packed = match args {
            [] => false,
            [flag, codec] if flag == "--packed" && codec == crate::packing::CODEC => true,
            other => return Err(format!("import: {other:?}")),
        };
        let input = asker
            .files
            .first()
            .ok_or("import: the client sent nothing to read")?
            .try_clone()
            .map_err(|e| e.to_string())?;
        let read = io::BufReader::with_capacity(CHUNK, File::from(input));
        let mut input: Box<dyn Read> = if packed {
            Box::new(crate::packing::Unpacked::new(read))
        } else {
            Box::new(read)
        };
        let staging = self.private_staging()?;
        // A live move's rounds first, where it is one (D176 part 7): what its memory, layer
        // and swap held, kept to be laid under its last save's.
        let mut magic = [0u8; 8];
        input
            .read_exact(&mut magic)
            .map_err(|e| format!("the move's stream: {e}"))?;
        let mut rest = io::Cursor::new(magic).chain(&mut input);
        let precopied = if magic == shards_vmm::vm::live::MAGIC {
            Some(Precopy::read(&mut rest, staging.path())?)
        } else {
            None
        };
        let mut rest: Box<dyn Read> = match precopied {
            Some(_) => Box::new(input),
            None => Box::new(rest),
        };
        let plan = read_plan(&mut rest)?;
        let needed: BTreeSet<String> = plan.files.iter().map(|f| f.rel.clone()).collect();
        let allows = |rel: &str| allowed(&plan, &needed, rel);
        let mut placer = Placer::new(staging.path(), &allows);
        read_entries(&mut rest, &mut |entry| placer.take(entry))?;
        placer.finish()?;
        self.adopt(&plan, staging.path(), precopied.as_ref())
    }

    /// A directory of a move's own in the home's private one, removed with all it holds as
    /// it drops.
    fn private_staging(&self) -> Result<Staging, String> {
        let private = self.home.join(super::PRIVATE);
        match crate::os::owner_only_dir(&mut std::fs::DirBuilder::new()).create(&private) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", private.display()));
            }
            _ => {}
        }
        let mut bytes = [0u8; 8];
        shards_vmm::platform::fill_random(&mut bytes).map_err(|e| e.to_string())?;
        let at = private.join(format!("move-{}", hex(&bytes)));
        std::fs::create_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        Ok(Staging(at))
    }

    /// Makes the microVM a move's stream put in `staged` this machine's: its volumes and
    /// the files its snapshot restores against put in place, its snapshot made to restore
    /// against them, its record its own here, and it seen, stopped as saved.
    fn adopt(&self, plan: &Plan, staged: &Path, precopied: Option<&Precopy>) -> Result<String, String> {
        let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        let staged_dir = staged.join("containers").join(&plan.id);
        // A live move's last save holds what its guest wrote since its last round alone:
        // what the rounds sent goes under it (D176 part 7).
        if let Some(precopied) = precopied {
            precopied.lay_under(&staged_dir.join(SNAPSHOTS).join(&plan.snapshot))?;
        }
        // The files this machine lacked, kept in its own directory, each by its place in the
        // plan.
        let backing_dir = staged_dir.join(BACKING);
        let mut shipped = std::collections::BTreeMap::new();
        for (i, file) in plan.files.iter().enumerate() {
            let sent = staged.join(relative(&file.rel)?);
            if !sent.exists() {
                continue;
            }
            std::fs::create_dir_all(&backing_dir).map_err(|e| at(&backing_dir, &e))?;
            let (sha256, size) = digest_of(&sent).map_err(|e| at(&sent, &e))?;
            if sha256 != file.sha256 || size != file.size {
                return Err(format!("{}: its bytes changed on the way", file.rel));
            }
            let kept = backing_dir.join(i.to_string());
            std::fs::rename(&sent, &kept).map_err(|e| at(&sent, &e))?;
            shipped.insert(file.rel.clone(), i);
        }
        let mut c: Container = serde_json::from_slice(
            &std::fs::read(staged_dir.join("config.json")).map_err(|e| at(&staged_dir, &e))?,
        )
        .map_err(|e| format!("its record: {e}"))?;
        if c.id != plan.id {
            return Err("its record is another microVM's".into());
        }
        let store = crate::volumes::Store::new(&self.home);
        for m in &mut c.mounts {
            if m.kind == "volume" && !m.name.is_empty() {
                m.source = store.data(&m.name).to_string_lossy().into_owned();
            }
        }
        c.saved = Some(plan.snapshot.clone());
        c.moved = true;
        // The key its run had where it ran: its run makes one of its own here after its
        // restore (D238), and no lease is served under the old (`key_refused`).
        if let Some(key) = &plan.key {
            let kept = staged_dir.join(KEY_REPLACED);
            std::fs::write(&kept, key).map_err(|e| at(&kept, &e))?;
        }
        c.ports.clear();
        // Its request names its image by the digest it was made of, which this machine
        // found it by: a tag here may name another.
        let request = staged_dir.join(super::REQUEST);
        if let Ok(bytes) = std::fs::read(&request)
            && let Some(mut run) = shards_ipc::Run::decode(&bytes)
            && !plan.image_id.is_empty()
        {
            run.image.clone_from(&plan.image_id);
            std::fs::write(&request, run.encode()).map_err(|e| at(&request, &e))?;
        }
        let home = std::fs::canonicalize(&self.home).map_err(|e| at(&self.home, &e))?;
        let dir = lock(&self.containers).dir(&plan.id);
        // Each file its snapshot names, by the name the plan gave it there.
        let to = |path: &Path| -> Option<PathBuf> {
            let file = plan.files.iter().find(|f| Path::new(&f.path) == path)?;
            match shipped.get(&file.rel) {
                Some(i) => Some(dir.join(BACKING).join(i.to_string())),
                None => Some(home.join(relative(&file.rel).ok()?)),
            }
        };
        let record = serde_json::to_vec(&c).map_err(|e| e.to_string())?;
        std::fs::write(staged_dir.join("config.json"), &record).map_err(|e| at(&staged_dir, &e))?;
        // In place: its volumes, then its directory, which the registry sees as it takes
        // the name.
        let volumes_root = self.home.join("volumes");
        std::fs::create_dir_all(&volumes_root).map_err(|e| at(&volumes_root, &e))?;
        let mut placed: Vec<PathBuf> = Vec::new();
        let undo = |placed: &[PathBuf]| {
            for p in placed {
                let _ = std::fs::remove_dir_all(p);
            }
        };
        {
            let _volumes = crate::volumes::lock();
            for v in &plan.volumes {
                let target = volumes_root.join(v);
                if store.get(v).is_some() || target.exists() {
                    undo(&placed);
                    return Err(format!("a volume named {v} is here already"));
                }
                let sent = staged.join("volumes").join(v);
                if let Err(e) = std::fs::rename(&sent, &target) {
                    undo(&placed);
                    return Err(at(&sent, &e));
                }
                placed.push(target);
            }
        }
        {
            let mut registry = lock(&self.containers);
            if registry.get(&plan.id).is_some() || registry.name_taken(&plan.name).is_some() || dir.exists() {
                drop(registry);
                undo(&placed);
                return Err(format!("a microVM named {} is here already", plan.name));
            }
            if let Err(e) = std::fs::rename(&staged_dir, &dir) {
                drop(registry);
                undo(&placed);
                return Err(at(&staged_dir, &e));
            }
            // Its snapshot, against this machine's files where they are now.
            if let Err(e) = shards_vmm::snapshot::relocate(&dir.join(SNAPSHOTS).join(&plan.snapshot), &to) {
                let _ = std::fs::remove_dir_all(&dir);
                drop(registry);
                undo(&placed);
                return Err(e);
            }
            if let Err(e) = registry.adopt(c) {
                let _ = std::fs::remove_dir_all(&dir);
                drop(registry);
                undo(&placed);
                return Err(e);
            }
        }
        self.container_event(&plan.id, "create", &[]);
        Ok(plan.name.clone())
    }
}

/// What a live move hears of its rounds from its VM (D176 part 7): how they go, as they go
/// (`kind::PRECOPYING`), then their answer (`kind::PRECOPIED`).
pub(super) enum Rounds {
    Going(String),
    Done(Result<String, String>),
}

/// A run's VM's answer to its live move's rounds (`kind::PRECOPIED`), told to the move that
/// waits for it.
pub(super) fn precopied(inbox: &mut super::Inbox, payload: &[u8]) {
    let Some(told) = inbox.precopying.take() else {
        return;
    };
    let answer = match payload.split_first() {
        Some((0, why)) => Err(String::from_utf8_lossy(why).into_owned()),
        _ => Ok(String::from_utf8_lossy(payload).into_owned()),
    };
    let _ = told.send(Rounds::Done(answer));
}

/// How a run's VM's live move's rounds go (`kind::PRECOPYING`), told to the move that waits
/// for them.
pub(super) fn precopying(inbox: &mut super::Inbox, payload: &[u8]) {
    if let Some(told) = &inbox.precopying {
        let _ = told.send(Rounds::Going(String::from_utf8_lossy(payload).into_owned()));
    }
}

/// What a live move's rounds sent, kept on the machine moved to (D176 part 7): its memory's
/// RAM by its offset in a save's file, the memory plugged by its address, its writable
/// layer's file system and its run's swap by their offsets, each a sparse file of the move's
/// own.
struct Precopy {
    ram: PathBuf,
    plugged: PathBuf,
    layer: PathBuf,
    swap: PathBuf,
}

impl Precopy {
    /// Reads the rounds from `r` into files in `dir`, the last save's last, each record
    /// over those before it.
    fn read(r: &mut dyn Read, dir: &Path) -> Result<Precopy, String> {
        use shards_vmm::vm::live;
        let p = Precopy {
            ram: dir.join("pre-ram"),
            plugged: dir.join("pre-plugged"),
            layer: dir.join("pre-layer"),
            swap: dir.join("pre-swap"),
        };
        let open = |at: &Path| -> Result<File, String> {
            let file = crate::os::owner_only(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true),
            )
            .open(at)
            .map_err(|e| format!("{}: {e}", at.display()))?;
            shards_vmm::platform::make_sparse(&file).map_err(|e| format!("{}: {e}", at.display()))?;
            Ok(file)
        };
        let files = [open(&p.ram)?, open(&p.plugged)?, open(&p.layer)?, open(&p.swap)?];
        live::read(r, &mut |record| {
            let file = match record.kind {
                live::RAM => &files[0],
                live::PLUGGED => &files[1],
                live::LAYER => &files[2],
                live::SWAP => &files[3],
                live::ROUND => return Ok(()),
                other => return Err(format!("a live move's record of kind {other}")),
            };
            shards_vmm::platform::write_all_at(file, record.bytes, record.offset)
                .map_err(|e| format!("a live move's record: {e}"))
        })?;
        Ok(p)
    }

    /// Puts what the rounds sent, the last save's last round among them, in place of the
    /// files of the generation the snapshot in `dir` is at, the last save's, which hold none
    /// of their pages, each as long as it: its memory's plugged runs where its state says.
    /// Which pages the rounds sent is never read from these files' holes, as a file system
    /// keeps them in units of its own (NTFS: 64 KiB), the rest of a unit a page was written
    /// into reading as zeros: the rounds' records say, laid in order as they came.
    fn lay_under(&self, dir: &Path) -> Result<(), String> {
        let generation = shards_vmm::snapshot::generation(dir)?;
        let (ram, plugged) = shards_vmm::snapshot::layout(dir)?;
        // The rounds' files become the generation's: what this takes, as its microVM runs
        // nowhere, is the memory plugged, never the whole of its memory.
        let memory = generation.join("memory");
        let saved = memory.with_extension("last");
        let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        std::fs::rename(&memory, &saved).map_err(|e| at(&memory, &e))?;
        std::fs::rename(&self.ram, &memory).map_err(|e| at(&self.ram, &e))?;
        let len = std::fs::metadata(&saved).map_err(|e| at(&saved, &e))?.len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&memory)
            .and_then(|f| f.set_len(len))
            .map_err(|e| at(&memory, &e))?;
        std::fs::remove_file(&saved).map_err(|e| at(&saved, &e))?;
        // Each run plugged, from its address in the rounds' file to its place after the RAM.
        let mut moves = Vec::new();
        let mut offset = ram;
        for (gpa, len) in plugged {
            moves.push((gpa, offset, len));
            offset = offset.saturating_add(len);
        }
        put(&self.plugged, &memory, &moves)?;
        for (pre, name) in [(&self.layer, "layer"), (&self.swap, "swap")] {
            let kept = generation.join(name);
            if !kept.exists() {
                continue;
            }
            let len = std::fs::metadata(&kept).map_err(|e| at(&kept, &e))?.len();
            std::fs::OpenOptions::new()
                .write(true)
                .open(pre)
                .and_then(|f| f.set_len(len))
                .map_err(|e| at(pre, &e))?;
            std::fs::rename(pre, &kept).map_err(|e| at(pre, &e))?;
        }
        Ok(())
    }
}

/// Copies into `to` each page of `from` that holds data, by `moves`, each a run of `from` at
/// its first offset, as long as its third, put at its second in `to`, which holds nothing
/// there: a page of a unit of `from`'s file system that the rounds wrote none of reads as
/// zeros, as the page it stands for is (a round sends every page not all zeros first).
fn put(from: &Path, to: &Path, moves: &[(u64, u64, u64)]) -> Result<(), String> {
    let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
    let source = File::open(from).map_err(|e| at(from, &e))?;
    let target = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(to)
        .map_err(|e| at(to, &e))?;
    let source_len = source.metadata().map_err(|e| at(from, &e))?.len();
    let target_len = target.metadata().map_err(|e| at(to, &e))?.len();
    let theirs = shards_vmm::platform::data_ranges(&source, source_len).map_err(|e| at(from, &e))?;
    let page = shards_vmm::vm::live::PAGE;
    let mut buf = vec![0u8; page as usize];
    for &(src, dst, len) in moves {
        for &(start, n) in &theirs {
            // The part of this data range within the run.
            let lo = start.max(src);
            let hi = start.saturating_add(n).min(src.saturating_add(len));
            let mut p = lo & !(page - 1);
            while p < hi {
                let into = dst + (p - src);
                if p >= src && into + page <= target_len {
                    shards_vmm::platform::read_exact_at(&source, &mut buf, p).map_err(|e| at(from, &e))?;
                    shards_vmm::platform::write_all_at(&target, &buf, into).map_err(|e| at(to, &e))?;
                }
                p += page;
            }
        }
    }
    Ok(())
}

/// A move's staging directory, removed with all it holds as it drops.
struct Staging(PathBuf);

impl Staging {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            id: "a".repeat(64),
            name: "web".into(),
            image: "alpine".into(),
            image_id: format!("sha256:{}", "b".repeat(64)),
            arch: std::env::consts::ARCH.into(),
            format: shards_vmm::snapshot::FORMAT,
            hypervisor: shards_vmm::hv::BACKEND.map(str::to_string),
            snapshot: "s1".into(),
            home: "/h".into(),
            files: vec![Backing {
                path: "/h/images/x.erofs".into(),
                rel: "images/x.erofs".into(),
                size: 3,
                sha256: "c".repeat(64),
            }],
            volumes: vec!["data".into()],
            binds: Vec::new(),
            cpu: Vec::new(),
            guest: false,
            baseline: Vec::new(),
            cpu_identity: Vec::new(),
            published: Vec::new(),
            network: String::new(),
            key: None,
        }
    }

    /// A microVM of another architecture, hypervisor or snapshot format is refused, each by
    /// what differs, before anything else of it is asked: the architecture first, as a
    /// machine of another has another hypervisor's VMs too. A plan of an earlier build names
    /// no hypervisor, and is taken as before.
    #[test]
    fn a_microvm_of_another_hypervisor_is_refused_before_anything_moves() {
        let (arch, format) = (std::env::consts::ARCH, shards_vmm::snapshot::FORMAT);
        let mut kvm = plan();
        kvm.hypervisor = Some("kvm".into());
        assert_eq!(platform_refused(&kvm, arch, Some("kvm"), format), None);
        let refused = platform_refused(&kvm, arch, Some("hvf"), format);
        assert_eq!(
            refused,
            Some(Refused::Hypervisor {
                name: "web".into(),
                hypervisor: "kvm".into(),
                here: Some("hvf".into()),
            })
        );
        assert_eq!(
            refused.map(|r| r.to_string()).as_deref(),
            Some(
                "web ran on KVM, and this machine runs microVMs on Hypervisor.framework: a \
                 snapshot holds its vCPUs and interrupt controller as its hypervisor keeps them"
            )
        );
        assert!(matches!(
            platform_refused(&kvm, arch, None, format),
            Some(Refused::Hypervisor { here: None, .. })
        ));
        let earlier = Plan {
            hypervisor: None,
            ..plan()
        };
        assert_eq!(platform_refused(&earlier, arch, Some("hvf"), format), None);
        let other = if arch == "x86_64" { "aarch64" } else { "x86_64" };
        assert!(matches!(
            platform_refused(&kvm, other, Some("hvf"), format),
            Some(Refused::Arch { .. })
        ));
        assert_eq!(
            platform_refused(&kvm, arch, Some("kvm"), format + 1),
            Some(Refused::Format {
                name: "web".into(),
                format,
                here: format + 1,
            })
        );
    }

    /// A machine whose hypervisor restores a snapshot only on the CPU it was taken on (WHP)
    /// refuses a microVM saved on another, by name, and takes one saved on its own; a plan of
    /// an earlier build, which names no CPU, reads back naming none, which no machine checks
    /// before its restore (D245).
    #[test]
    fn a_microvm_saved_on_another_cpu_is_refused_where_a_restore_takes_only_its_own() {
        let (here, other) = (
            [[1u32, 0, 0, 0x00a6_0f12, 0, 0, 0]],
            [[1u32, 0, 0, 0x00b4_0f40, 0, 0, 0]],
        );
        assert_eq!(identity_refused("web", &here, &here), None);
        assert_eq!(
            identity_refused("web", &here, &other),
            Some(Refused::CpuIdentity { name: "web".into() })
        );
        assert_eq!(
            identity_refused("web", &here, &here[..0]),
            Some(Refused::CpuIdentity { name: "web".into() })
        );
        let mut earlier = serde_json::to_value(plan()).unwrap();
        earlier.as_object_mut().unwrap().remove("cpu_identity");
        let earlier: Plan = serde_json::from_value(earlier).unwrap();
        assert!(earlier.cpu_identity.is_empty());
    }

    /// A moved run's key (D176 part 6, D238): its run's own key is the `vm` line of its managed
    /// metadata, none for a run without one or with none written right; a lease asked under
    /// the key its move replaced is refused typed with its key here, and one under any other
    /// key is no move's to refuse; a plan of an earlier build names none.
    #[test]
    fn a_lease_under_the_key_a_move_replaced_is_refused_and_the_run_names_its_own() {
        let dir = shards_testdir::TempDir::new("mv-key").unwrap();
        assert!(!super::super::managed::has_scope(&dir));
        assert_eq!(super::super::managed::vm_key(&dir), None);
        let key = [0xab_u8; 32];
        let metadata = format!(
            "shards managed output 1\nserver {}\nowner {}\nrun x\ngeneration 1\ncapacity 1\n\
             control 127.0.0.1:1\nvm {}\n",
            "c".repeat(64),
            "d".repeat(64),
            shards_remote::identity::hex(&key)
        );
        std::fs::write(dir.join("managed-output"), &metadata).unwrap();
        assert!(super::super::managed::has_scope(&dir));
        assert_eq!(super::super::managed::vm_key(&dir), Some(key));
        std::fs::write(dir.join("managed-output"), metadata.replace("\nvm ", "\nvm z")).unwrap();
        assert_eq!(super::super::managed::vm_key(&dir), None, "64 hex digits or none");
        std::fs::write(dir.join("managed-output"), "shards managed output 1\nserver x\n").unwrap();
        assert_eq!(
            super::super::managed::vm_key(&dir),
            None,
            "no agent-control endpoint"
        );

        let (old, new) = ("a".repeat(64), "b".repeat(64));
        assert_eq!(
            key_refused("web", &old, Some(&old), Some(&new)),
            Some(Refused::KeyReplaced {
                name: "web".into(),
                asked: old.clone(),
                current: Some(new.clone()),
            })
        );
        assert_eq!(key_refused("web", &new, Some(&old), Some(&new)), None);
        assert_eq!(key_refused("web", &old, None, Some(&new)), None);

        let mut earlier = serde_json::to_value(plan()).unwrap();
        earlier.as_object_mut().unwrap().remove("key");
        let earlier: Plan = serde_json::from_value(earlier).unwrap();
        assert_eq!(earlier.key, None);
    }

    /// A refusal goes as JSON, tagged by what refused it, and reads back as it went; its
    /// words are those each refusal said before it was typed, which a mover of an earlier
    /// build still prints.
    #[test]
    fn a_refusal_is_typed_on_the_wire_and_says_what_it_said() {
        let all = [
            (
                Refused::Arch {
                    name: "web".into(),
                    arch: "aarch64".into(),
                    here: "x86_64".into(),
                },
                "web is a aarch64 microVM, and this machine runs x86_64 microVMs",
            ),
            (
                Refused::Format {
                    name: "web".into(),
                    format: 28,
                    here: 29,
                },
                "web was saved in snapshot format 28, and this machine's shards reads 29",
            ),
            (
                Refused::Cpu {
                    name: "web".into(),
                    short: vec![CpuShort::Field {
                        reg: 0xc032,
                        shift: 52,
                        guest: 1,
                        here: 0,
                    }],
                },
                "this CPU has less of what its guest uses: ID_AA64ISAR2_EL1[55:52] 1 here 0 (the \
                 guest's level, then this host's), which web's CPU baseline names",
            ),
            (
                Refused::Cpu {
                    name: "web".into(),
                    short: vec![
                        CpuShort::Bits {
                            function: 7,
                            index: 0,
                            register: 4,
                            missing: 1 << 16,
                        },
                        CpuShort::Bits {
                            function: 0xd,
                            index: 0,
                            register: 3,
                            missing: 0xe0,
                        },
                    ],
                },
                "this CPU lacks what its guest uses: leaf 0x7.0 ebx bits 0x10000, leaf 0xd.0 eax \
                 bits 0xe0, which web's CPU baseline names",
            ),
            (
                Refused::CpuVendor { name: "web".into() },
                "web's CPU baseline is another vendor's CPU than this machine's: no CPUID makes \
                 one vendor's CPU another's",
            ),
            (
                Refused::PortTaken {
                    name: "web".into(),
                    port: 80,
                    proto: "tcp".into(),
                    host_ip: "127.0.0.1".into(),
                    host_port: "8080".into(),
                    why: "Bind for 127.0.0.1:8080 failed: port is already allocated".into(),
                },
                "web publishes its port 80/tcp at 127.0.0.1:8080, which this machine cannot \
                 bind: Bind for 127.0.0.1:8080 failed: port is already allocated",
            ),
            (
                Refused::Managed { name: "web".into() },
                "web has a managed scope, which a restore does not take yet: moved, it could \
                 run neither there nor here",
            ),
            (
                Refused::KeyReplaced {
                    name: "web".into(),
                    asked: "aa11".into(),
                    current: Some("bb22".into()),
                },
                "aa11 is the key web had before it moved, which no lease is served under: its \
                 key is bb22",
            ),
            (
                Refused::KeyReplaced {
                    name: "web".into(),
                    asked: "aa11".into(),
                    current: None,
                },
                "aa11 is the key web had before it moved, which no lease is served under: it has \
                 no key here yet",
            ),
            (
                Refused::CpuIdentity { name: "web".into() },
                "this machine's CPU is not the one web was saved on, and the Windows Hypervisor \
                 Platform restores a snapshot only on that CPU",
            ),
            (
                Refused::Features {
                    name: "web".into(),
                    lacks: vec!["FEAT_CSSC".into(), "FEAT_HBC".into()],
                },
                "this machine's CPU lacks FEAT_CSSC, FEAT_HBC, which web's guest may have used where it ran",
            ),
            (
                Refused::IdTaken { name: "web".into() },
                "a microVM with web's ID is here already",
            ),
            (
                Refused::NameTaken { name: "web".into() },
                "a microVM named web is here already",
            ),
            (
                Refused::VolumeTaken {
                    name: "web".into(),
                    volume: "data".into(),
                },
                "a volume named data is here already, and web's would move here",
            ),
            (
                Refused::BindAbsent {
                    name: "web".into(),
                    path: "/srv".into(),
                },
                "/srv, which web binds, is not on this machine: a move takes a bind's host path shared at the same place",
            ),
        ];
        for (refused, said) in all {
            let json = serde_json::to_string(&refused).unwrap();
            assert_eq!(serde_json::from_str::<Refused>(&json).unwrap(), refused, "{json}");
            assert_eq!(refused.to_string(), said);
        }
        let json = serde_json::to_string(&Refused::NameTaken { name: "web".into() }).unwrap();
        assert_eq!(json, r#"{"refused":"name-taken","name":"web"}"#);
    }

    /// A baseline masks the features of the registers it names, which the move checks by
    /// their fields; the rest it checks by name.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn a_baseline_masks_the_features_of_the_registers_it_names() {
        let names = |l: &[&str]| l.iter().map(|f| (*f).to_string()).collect::<Vec<String>>();
        // An M3 Ultra on macOS 26.4 and an M4 Max on 26.2 (PM M462).
        let m3 = names(&["FEAT_AES", "FEAT_SSBS"]);
        let m4 = names(&["FEAT_AES", "FEAT_FPACCOMBINE", "FEAT_CSSC"]);
        assert_eq!(cpu_lacks(&m3, &m4), ["FEAT_SSBS"]);
        assert_eq!(cpu_lacks(&m4, &m3), ["FEAT_FPACCOMBINE", "FEAT_CSSC"]);
        // Told a baseline naming PFR1, ISAR0 and ISAR1, as Hypervisor.framework's are: SSBS
        // and FPAC combined are its to mask, and the move checks them by its fields; CSSC,
        // ISAR2's, it cannot, and the move still checks by name.
        let base = [(0xc020, 0), (0xc021, 0), (0xc030, 0), (0xc031, 0)];
        assert!(cpu_lacks(&unmasked(&m3, &base), &m4).is_empty());
        assert_eq!(cpu_lacks(&unmasked(&m4, &base), &m3), ["FEAT_CSSC"]);
        // No baseline: every feature, by name, as before.
        assert_eq!(unmasked(&m4, &[]), m4);
        // A feature of no known register is checked by name.
        assert_eq!(unmasked(&names(&["FEAT_NEW"]), &base), ["FEAT_NEW"]);
    }

    /// An x86-64 baseline names CPUID's feature leaves whole, which the move checks by their
    /// bits ([`shards_vmm::arch::cpu::covers`]): told one, a guest's features are checked
    /// by none of the names Linux gives them; told none, by every one, as before.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_baseline_masks_every_feature_an_x86_guest_finds() {
        let flags = vec![
            "avx512f".to_string(),
            "sha_ni".to_string(),
            "constant_tsc".to_string(),
        ];
        assert_eq!(unmasked(&flags, &[]), flags);
        let base = [[7, 0, 0, 0, 0x019c_91a9, 0, 0]];
        assert!(unmasked(&flags, &base).is_empty());
        assert!(cpu_lacks(&unmasked(&flags, &base), &[]).is_empty());
    }

    /// A machine whose KVM shows a guest less than the baseline a microVM's guest was told
    /// refuses it, each register by the bits it lacks, and one of another vendor by its
    /// vendor; one with all of it takes it (D245).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_machine_short_of_a_baseline_refuses_it_by_each_register() {
        let amd = [0, 0, 0, 0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65];
        let intel = [0, 0, 0, 0x1f, 0x756e_6547, 0x6c65_746e, 0x4965_6e69];
        let baseline = vec![amd, [7, 0, 0, 0, 0x019c_91a9, 0, 0], [0xd, 0, 0, 0xe7, 0, 0, 0]];
        let here = vec![
            amd,
            [7, 0, 0, 0, 0x2000_0000 | 0x019c_91a9, 0, 0],
            [0xd, 0, 0, 0x7, 0, 0, 0],
        ];
        assert_eq!(baseline_refused("web", &baseline, &baseline), None);
        assert_eq!(
            baseline_refused("web", &here, &baseline),
            Some(Refused::Cpu {
                name: "web".into(),
                short: vec![CpuShort::Bits {
                    function: 0xd,
                    index: 0,
                    register: 3,
                    missing: 0xe0,
                }],
            })
        );
        let other = vec![intel, [7, 0, 0, 0, !0, 0, 0], [0xd, 0, 0, !0, 0, 0, 0]];
        assert_eq!(
            baseline_refused("web", &other, &baseline),
            Some(Refused::CpuVendor { name: "web".into() })
        );
    }

    /// A move to a machine whose CPU lacks a feature of the one its microVM ran on is
    /// refused, naming each (an M5 Max's microVM to an M4 Max, PM M460); one whose CPU has
    /// them all, or whose OS names none, is taken. This machine takes its own.
    #[test]
    fn a_cpu_lacking_what_its_guest_may_have_used_is_named() {
        let names = |l: &[&str]| l.iter().map(|f| (*f).to_string()).collect::<Vec<String>>();
        let m5 = names(&["FEAT_AES", "FEAT_CSSC", "FEAT_EBF16", "FEAT_HBC"]);
        let m4 = names(&["FEAT_AES"]);
        assert_eq!(cpu_lacks(&m5, &m4), ["FEAT_CSSC", "FEAT_EBF16", "FEAT_HBC"]);
        assert!(cpu_lacks(&m4, &m5).is_empty());
        assert!(cpu_lacks(&m5, &m5).is_empty());
        assert!(cpu_lacks(&m5, &[]).is_empty());
        let own = host_cpu();
        assert!(cpu_lacks(&own, &own).is_empty());
        if cfg!(any(
            all(target_os = "macos", target_arch = "aarch64"),
            target_os = "linux"
        )) {
            assert!(!own.is_empty(), "this machine's OS names its CPU's features");
        }
    }

    /// A live move's last round, which its last save writes on after the rounds, lays over
    /// them page by page: a page it sends as zeros reads as zeros, and its neighbours as the
    /// rounds sent them, in the file system's unit too. Where the last save's pages were
    /// laid from its files' holes, the rest of each 64 KiB unit NTFS keeps a sparse file's
    /// pages in read as zeros over what the rounds sent (CI's windows-2025: a restored
    /// guest's pmem queue found zeroed, "65535 new buffers on a queue of 64").
    #[test]
    fn a_live_moves_last_round_lays_over_its_rounds_page_by_page() {
        use shards_vmm::vm::live;
        let mut bytes = Vec::new();
        {
            let mut w = live::Writer::new(&mut bytes).unwrap();
            w.record(live::RAM, 0, &[1; 64 << 10]).unwrap();
            w.round(0).unwrap();
            w.record(live::RAM, 0x3000, &[2; 4096]).unwrap();
            w.round(1).unwrap();
        }
        let mut last = live::Writer::going_on(&mut bytes);
        last.record(live::RAM, 0x1000, &[0; 4096]).unwrap();
        last.record(live::RAM, 0x8000, &[3; 4096]).unwrap();
        last.record(live::SWAP, 0x2000, &[4; 4096]).unwrap();
        last.finish().unwrap();
        let dir = shards_testdir::TempDir::new("mv-live").unwrap();
        let p = Precopy::read(&mut bytes.as_slice(), &dir).unwrap();
        let ram = std::fs::read(&p.ram).unwrap();
        assert_eq!(ram.len(), 64 << 10);
        let page = |n: usize| &ram[n * 4096..(n + 1) * 4096];
        for (n, want) in [(0, 1), (1, 0), (2, 1), (3, 2), (4, 1), (8, 3), (9, 1), (15, 1)] {
            assert!(page(n).iter().all(|&b| b == want), "page {n} is not all {want}s");
        }
        let swap = std::fs::read(&p.swap).unwrap();
        assert_eq!(swap.len(), 0x3000);
        assert!(swap[..0x2000].iter().all(|&b| b == 0));
        assert!(swap[0x2000..].iter().all(|&b| b == 4));
        // Cut before its end, as a last save that failed leaves it, it is refused.
        let cut = shards_testdir::TempDir::new("mv-live-cut").unwrap();
        assert!(Precopy::read(&mut &bytes[..bytes.len() - 13], &cut).is_err());
    }

    /// A tree written as a move's stream reads back whole, a sparse file as sparse, its
    /// modes and links with it; and its names are only those its plan allows.
    #[test]
    fn a_tree_moves_whole_and_as_sparse() {
        let from = shards_testdir::TempDir::new("mv-from").unwrap();
        let p = plan();
        let own = from.join("c");
        std::fs::create_dir_all(own.join("snapshots/s1")).unwrap();
        std::fs::create_dir_all(own.join("snapshots/other")).unwrap();
        std::fs::write(own.join("snapshots/other/memory"), b"left").unwrap();
        std::fs::write(own.join("config.json"), b"{}").unwrap();
        // Sparse as a snapshot's memory is made: NTFS takes every cluster up to a file's end
        // as it is extended unless it is sparse (PM M349), and all 64 MiB are its data then.
        let sparse = std::fs::File::create(own.join("snapshots/s1/memory")).unwrap();
        shards_vmm::platform::make_sparse(&sparse).unwrap();
        sparse.set_len(64 << 20).unwrap();
        shards_vmm::platform::write_all_at(&sparse, b"head", 0).unwrap();
        shards_vmm::platform::write_all_at(&sparse, b"tail", (64 << 20) - 4).unwrap();
        drop(sparse);
        guest::make_link(&own.join("link"), b"config.json").unwrap();
        let mut bytes = Vec::new();
        {
            let mut w = Writer::new(&mut bytes, &p).unwrap();
            let rel = format!("containers/{}", p.id);
            let skip_from = format!("{rel}/snapshots/");
            let keep = format!("{skip_from}s1");
            w.tree(&own, &rel, &|r| {
                r.strip_prefix(&skip_from)
                    .is_some_and(|rest| !rest.contains('/') && r != keep)
            })
            .unwrap();
            w.finish().unwrap();
        }
        // Far less than the file's length: its hole is not sent.
        assert!(bytes.len() < 4 << 20, "{}", bytes.len());
        let to = shards_testdir::TempDir::new("mv-to").unwrap();
        let needed: BTreeSet<String> = p.files.iter().map(|f| f.rel.clone()).collect();
        let allows = |rel: &str| allowed(&p, &needed, rel);
        let mut placer = Placer::new(&to, &allows);
        let mut input = bytes.as_slice();
        assert_eq!(read_plan(&mut input).unwrap(), p);
        read_entries(&mut input, &mut |e| placer.take(e)).unwrap();
        placer.finish().unwrap();
        let there = to.join(format!("containers/{}", p.id));
        assert_eq!(std::fs::read(there.join("config.json")).unwrap(), b"{}");
        assert!(!there.join("snapshots/other").exists());
        let memory = std::fs::read(there.join("snapshots/s1/memory")).unwrap();
        assert_eq!(memory.len(), 64 << 20);
        assert_eq!(&memory[..4], b"head");
        assert_eq!(&memory[(64 << 20) - 4..], b"tail");
        assert_eq!(guest::read_link(&there.join("link")).unwrap(), b"config.json");
        // Cut short, it is refused.
        let mut cut = &bytes[..bytes.len() - 1];
        let elsewhere = shards_testdir::TempDir::new("mv-cut").unwrap();
        let anything = |_: &str| true;
        let mut placer = Placer::new(&elsewhere, &anything);
        read_plan(&mut cut).unwrap();
        assert!(read_entries(&mut cut, &mut |e| placer.take(e)).is_err());
    }

    /// A volume's files move as its guest saw them (D176 part 6), whatever this host keeps
    /// them as (D38, D197): each one's kind, mode, recorded owner and group (none where none
    /// was, so that its share's default owner's stays), a device's number, a symlink's
    /// target and its extended attributes, by the guest's names, a name no Windows name
    /// holds among them; and a directory whose mode keeps its owner out takes what it holds
    /// first. Each reads there as it read here, and as it was given.
    #[test]
    fn a_volumes_files_move_as_its_guest_saw_them() {
        let from = shards_testdir::TempDir::new("mv-vfrom").unwrap();
        let p = plan();
        let data = from.join("data");
        std::fs::create_dir(&data).unwrap();
        let root = data.join("_data");
        guest::make_dir(&root).unwrap();
        let meta = |mode: u32, owner: Option<(u32, u32)>| guest::Meta {
            mode,
            owner,
            ..guest::Meta::default()
        };
        let mut f = meta(guest::S_IFREG | 0o640, Some((1234, 5678)));
        f.xattrs = vec![(b"user.k".to_vec(), b"v".to_vec())];
        let files = [
            ("f", f),
            ("a:b", meta(guest::S_IFREG | 0o600, Some((1, 2)))),
            ("zero", meta(guest::S_IFREG, Some((0, 0)))),
            ("host", meta(guest::S_IFREG | 0o644, None)),
        ];
        for (name, m) in &files {
            let at = root.join(guest::host_name(name.as_bytes()).unwrap());
            std::fs::write(&at, name).unwrap();
            guest::set(&at, m).unwrap();
        }
        let null = guest::Meta {
            rdev: (1 << 8) | 3,
            ..meta(guest::S_IFCHR | 0o620, Some((0, 0)))
        };
        guest::make_node(&root.join("null"), &null).unwrap();
        guest::set(&root.join("null"), &null).unwrap();
        guest::make_link(&root.join("l"), b"f").unwrap();
        guest::set(&root.join("l"), &meta(guest::S_IFLNK | 0o777, None)).unwrap();
        guest::set(&root, &meta(guest::S_IFDIR | 0o555, None)).unwrap();
        let names = ["f", "a:b", "zero", "host", "null", "l"];
        let seen = |root: &Path| -> Vec<guest::Meta> {
            let mut all = vec![guest::read(root).unwrap()];
            for name in names {
                let mut m = guest::read(&root.join(guest::host_name(name.as_bytes()).unwrap())).unwrap();
                m.xattrs.sort();
                all.push(m);
            }
            all
        };
        let here = seen(&root);
        for (i, (name, given)) in files.iter().enumerate() {
            assert_eq!(&here[i + 1], given, "{name}");
        }
        assert_eq!((here[5].mode, here[5].rdev), (null.mode, null.rdev));
        assert_eq!(here[0].mode, guest::S_IFDIR | 0o555);

        let mut bytes = Vec::new();
        {
            let mut w = Writer::new(&mut bytes, &p).unwrap();
            w.tree(&data, "volumes/data", &|_| false).unwrap();
            w.finish().unwrap();
        }
        let to = shards_testdir::TempDir::new("mv-vto").unwrap();
        let needed = BTreeSet::new();
        let allows = |rel: &str| allowed(&p, &needed, rel);
        let mut placer = Placer::new(&to, &allows);
        let mut input = bytes.as_slice();
        read_plan(&mut input).unwrap();
        read_entries(&mut input, &mut |e| placer.take(e)).unwrap();
        placer.finish().unwrap();
        let there = to.join("volumes").join("data").join("_data");
        assert_eq!(seen(&there), here);
        assert_eq!(guest::read_link(&there.join("l")).unwrap(), b"f");
        let ab = there.join(guest::host_name(b"a:b").unwrap());
        assert_eq!(std::fs::read(ab).unwrap(), b"a:b");
        // A stream of another version is refused.
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(read_plan(&mut old.as_slice()).is_err());
        // Put back to the host's own access, for the test's directories to go.
        guest::set(&there, &meta(guest::S_IFDIR | 0o755, None)).unwrap();
        guest::set(&root, &meta(guest::S_IFDIR | 0o755, None)).unwrap();
    }

    /// A stream may put nothing past what its plan names: another microVM's directory,
    /// a name reaching out of the home, or a file its plan did not say was needed.
    #[test]
    fn a_stream_puts_only_what_its_plan_names() {
        let p = plan();
        let needed: BTreeSet<String> = p.files.iter().map(|f| f.rel.clone()).collect();
        assert!(allowed(&p, &needed, &format!("containers/{}/config.json", p.id)));
        assert!(allowed(&p, &needed, "volumes/data/_data/f"));
        assert!(allowed(&p, &needed, "images/x.erofs"));
        assert!(!allowed(&p, &needed, "containers/other/config.json"));
        assert!(!allowed(&p, &needed, "volumes/database/_data"));
        assert!(!allowed(&p, &needed, "images/y.erofs"));
        for bad in ["", "/etc/passwd", "a/../b", "a//b", "./a", "a\0b", "..", "a\\b"] {
            assert!(relative(bad).is_err(), "{bad:?}");
        }
        assert_eq!(relative("a/b").unwrap(), Path::new("a").join("b"));
        assert_eq!(
            rel_of(Path::new("/h"), Path::new("/h/images/x")),
            Some("images/x".into())
        );
        assert_eq!(rel_of(Path::new("/h"), Path::new("/other/x")), None);
        assert_eq!(rel_of(Path::new("/h"), Path::new("/h")), None);
    }
}
