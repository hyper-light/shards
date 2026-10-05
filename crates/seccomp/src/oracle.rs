//! This crate held to Docker's toolchain (testdata/oracle-*.json, scripts/seccomp/generate):
//! each case's program as runc v1.3.4 with libseccomp 2.5.4 would load it, or its refusal.
//!
//! Docker mode decides as that program does on every input of a grid (each architecture,
//! each syscall number to past the last any table knows, x32's and ARM's own ranges, -1,
//! and each argument's compared values and their neighbours), and refuses in its words.
//! Shards mode's program decides as its own graph does, and differs from Docker's only
//! where compiler.rs says it does better.

use crate::bpf::{self, Data, Insn};
use crate::compiler::{self, Mode};
use crate::profile::{self, Arch, Container, Kernel};
use crate::tables::{self, Abi};

fn json(path: &str) -> serde_json::Value {
    let full = format!("{}/testdata/{path}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(&full).unwrap()).unwrap()
}

fn program(v: &serde_json::Value) -> Vec<Insn> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|i| Insn {
                    code: i[0].as_u64().unwrap() as u16,
                    jt: i[1].as_u64().unwrap() as u8,
                    jf: i[2].as_u64().unwrap() as u8,
                    k: i[3].as_u64().unwrap() as u32,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn caps(case: &serde_json::Value) -> Vec<String> {
    case["caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect()
}

fn profile_bytes(case: &serde_json::Value) -> Vec<u8> {
    match case.get("profile") {
        Some(p) => serde_json::to_vec(p).unwrap(),
        None => profile::DEFAULT.to_vec(),
    }
}

/// The guest kernels the oracle ran on and shards boots are both past every minKernel the
/// cases name but "9.0".
const KERNEL: Kernel = Kernel(6, 18);

fn build(case: &serde_json::Value, arch: Arch, mode: Mode) -> Result<Option<compiler::Compiled>, String> {
    let caps = caps(case);
    let c = Container {
        arch,
        caps: &caps,
        kernel: KERNEL,
    };
    let decoded = profile::decode(&profile_bytes(case))?;
    let Some(spec) = profile::resolve(&decoded, &c)? else {
        return Ok(None);
    };
    let Some(cfg) = profile::convert(&spec)? else {
        return Ok(None);
    };
    compiler::compile(&cfg, arch, mode).map(Some)
}

/// The values worth comparing an argument with: the case's own, their neighbours, and
/// those that split a word.
fn values(case: &serde_json::Value) -> Vec<u64> {
    let mut v = vec![
        0,
        1,
        2,
        u64::MAX,
        0xffff_ffff,
        0x1_0000_0000,
        0x1_0000_0001,
        0x7fff_ffff,
        0x8000_0000,
    ];
    let p: serde_json::Value = serde_json::from_slice(&profile_bytes(case)).unwrap();
    for s in p["syscalls"].as_array().into_iter().flatten() {
        for a in s["args"].as_array().into_iter().flatten() {
            for key in ["value", "valueTwo"] {
                if let Some(x) = a[key].as_u64() {
                    v.extend([
                        x,
                        x.wrapping_sub(1),
                        x.wrapping_add(1),
                        x | 0x1_0000_0000,
                        x & 0xffff_ffff,
                    ]);
                }
            }
            if let (Some(m), Some(d)) = (a["value"].as_u64(), a["valueTwo"].as_u64()) {
                v.extend([d | !m, d & m, d ^ 1]);
            }
        }
    }
    // ipc's calls with a version in their upper half.
    v.extend([0x1_0017, 0x2_0001]);
    v.sort_unstable();
    v.dedup();
    v
}

fn arg_vectors(vals: &[u64]) -> Vec<[u64; 6]> {
    let mut out = vec![[0; 6]];
    for &v in vals {
        out.push([v; 6]);
        for i in 0..6 {
            let mut a = [0; 6];
            a[i] = v;
            out.push(a);
        }
    }
    out
}

/// Each syscall number worth asking about, under an architecture.
fn numbers() -> Vec<u32> {
    let x32 = tables::X32_SYSCALL_BIT as u32;
    let mut n: Vec<u32> = (0..=1100).collect();
    n.extend(x32..=x32 + 1100);
    n.extend(983_040..=983_050);
    n.extend([x32 - 1, 0x7fff_ffff, 0xffff_fffe, u32::MAX]);
    n
}

fn arches(arch: Arch) -> Vec<u32> {
    let foreign = 21 | 0x8000_0000 | 0x4000_0000; // AUDIT_ARCH_PPC64LE
    match arch {
        Arch::Amd64 => vec![Abi::X86_64.audit_arch(), Abi::X86.audit_arch(), foreign],
        Arch::Arm64 => vec![Abi::Aarch64.audit_arch(), Abi::Arm.audit_arch(), foreign],
    }
}

/// Each input of the grid: every number with no arguments, and the numbers of syscalls a
/// case compares arguments of (and the multiplexers) with every vector.
fn inputs(case: &serde_json::Value, arch: Arch) -> Vec<Data> {
    let vectors = arg_vectors(&values(case));
    let compared: Vec<String> = {
        let p: serde_json::Value = serde_json::from_slice(&profile_bytes(case)).unwrap();
        p["syscalls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["args"].as_array().is_some_and(|a| !a.is_empty()))
            .flat_map(|s| {
                let mut names: Vec<String> = s["names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|n| n.as_str().unwrap().to_string())
                    .collect();
                if let Some(n) = s["name"].as_str() {
                    names.push(n.to_string());
                }
                names
            })
            .collect()
    };
    let mut out = Vec::new();
    for a in arches(arch) {
        let abis: &[Abi] = match arch {
            Arch::Amd64 => &[Abi::X86_64, Abi::X86, Abi::X32],
            Arch::Arm64 => &[Abi::Aarch64, Abi::Arm],
        };
        let mut special: Vec<u32> = vec![tables::X86_SOCKETCALL as u32, tables::X86_IPC as u32];
        for abi in abis {
            for name in &compared {
                for n in [
                    tables::libseccomp::resolve_name(*abi, name),
                    tables::libseccomp::resolve_name_raw(*abi, name),
                ] {
                    if n >= 0 {
                        special.push(n as u32);
                    }
                }
                if let Some(n) = tables::kernel_nr(*abi, name) {
                    special.push(n as u32);
                }
            }
        }
        special.sort_unstable();
        special.dedup();
        for nr in numbers() {
            let vs: &[[u64; 6]] = if special.contains(&nr) {
                &vectors
            } else {
                &vectors[..1]
            };
            for args in vs {
                out.push(Data {
                    nr,
                    arch: a,
                    ip: 0,
                    args: *args,
                });
            }
        }
    }
    out
}

fn cases() -> Vec<serde_json::Value> {
    json("cases.json").as_array().unwrap().clone()
}

/// Docker mode decides as runc's program does, and refuses as runc and dockerd do.
#[test]
fn docker_mode_decides_as_runcs_program() {
    for (key, arch) in [("amd64", Arch::Amd64), ("arm64", Arch::Arm64)] {
        let oracle = json(&format!("oracle-{key}.json"));
        for (case, want) in cases().iter().zip(oracle["cases"].as_array().unwrap()) {
            let name = case["name"].as_str().unwrap();
            assert_eq!(want["name"], case["name"]);
            let got = build(case, arch, Mode::Docker);
            if let Some(e) = want["error"].as_str() {
                assert_eq!(got.err().as_deref(), Some(e), "{key} {name}");
                continue;
            }
            let prog = program(&want["program"]);
            let got = got.unwrap_or_else(|e| panic!("{key} {name}: {e}"));
            let Some(got) = got else {
                assert!(prog.is_empty(), "{key} {name}: unconfined, runc filters");
                continue;
            };
            assert_eq!(
                u64::from(got.flags),
                want["flags"].as_u64().unwrap(),
                "{key} {name} flags"
            );
            let mut diffs = Vec::new();
            for d in inputs(case, arch) {
                let (ours, theirs) = (got.decide(&d), bpf::run(&prog, &d));
                if ours != theirs {
                    diffs.push((d, ours, theirs));
                }
            }
            assert!(
                diffs.is_empty(),
                "{key} {name}: {} inputs decided otherwise, e.g. {:x?}",
                diffs.len(),
                &diffs[..diffs.len().min(5)]
            );
        }
    }
}

/// Each program shards loads decides as the graph it was compiled from, in both modes,
/// and is one the kernel takes.
#[test]
fn programs_decide_as_their_graphs() {
    for arch in [Arch::Amd64, Arch::Arm64] {
        for case in cases() {
            for mode in [Mode::Shards, Mode::Docker] {
                let Ok(Some(c)) = build(&case, arch, mode) else {
                    continue;
                };
                let p = c.program().unwrap();
                assert!(p.insns.len() <= 4096);
                for d in inputs(&case, arch) {
                    assert_eq!(
                        bpf::run(&p.insns, &d),
                        c.decide(&d),
                        "{arch:?} {mode:?} {} {d:?}",
                        case["name"]
                    );
                }
            }
        }
    }
}

const ENOSYS: u32 = 0x0005_0026;

/// Shards decides as Docker does but where it means to do better: a syscall Docker's
/// tables cannot name but the guest kernel's can, either side's ENOSYS (runc's stub's
/// line is drawn by Docker's tables, shards' by the kernel's, and shards' answers too for
/// numbers that are no syscall), and x86's multiplexers.
#[test]
fn shards_differs_from_docker_only_where_it_does_better() {
    for (key, arch) in [("amd64", Arch::Amd64), ("arm64", Arch::Arm64)] {
        let oracle = json(&format!("oracle-{key}.json"));
        for (case, want) in cases().iter().zip(oracle["cases"].as_array().unwrap()) {
            let name = case["name"].as_str().unwrap();
            let prog = program(&want["program"]);
            let ours = build(case, arch, Mode::Shards);
            if let Some(e) = want["error"].as_str() {
                assert_eq!(ours.err().as_deref(), Some(e), "{key} {name}");
                continue;
            }
            let Some(ours) = ours.unwrap() else {
                assert!(prog.is_empty());
                continue;
            };
            let p: serde_json::Value = serde_json::from_slice(&profile_bytes(case)).unwrap();
            let listed: Vec<String> = p["syscalls"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|s| {
                    s["names"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .chain(s.get("name").cloned())
                })
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect();
            for d in inputs(case, arch) {
                let (a, b) = (ours.decide(&d), bpf::run(&prog, &d));
                if a == b {
                    continue;
                }
                let abi = [Abi::X86_64, Abi::X86, Abi::X32, Abi::Aarch64, Abi::Arm]
                    .into_iter()
                    .find(|x| {
                        x.audit_arch() == d.arch
                            && ((d.nr & 0x4000_0000 != 0) == (*x == Abi::X32)
                                || *x == Abi::X86
                                || *x == Abi::Arm
                                || *x == Abi::Aarch64)
                    });
                let new_name = abi.is_some_and(|abi| {
                    listed.iter().any(|n| {
                        tables::kernel_nr(abi, n) == Some(d.nr as i32)
                            && tables::libseccomp::resolve_name(abi, n) != d.nr as i32
                    })
                });
                let multiplexed = abi == Some(Abi::X86) && (d.nr == 102 || d.nr == 117);
                let enosys = a == Some(ENOSYS) || b == Some(ENOSYS);
                assert!(
                    new_name || multiplexed || enosys,
                    "{key} {name}: {d:x?} shards {a:x?}, Docker {b:x?}"
                );
            }
        }
    }
}

/// What shards does better, case by case.
#[test]
fn shards_allows_what_dockers_default_profile_allows() {
    let none: Vec<String> = Vec::new();
    let default_caps: Vec<String> = cases()[0]["caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    let _ = none;
    let decide = |arch: Arch, abi: Abi, nr: u32, args: [u64; 6]| {
        let c = Container {
            arch,
            caps: &default_caps,
            kernel: KERNEL,
        };
        let cfg = profile::convert(
            &profile::resolve(&profile::decode(profile::DEFAULT).unwrap(), &c)
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        compiler::compile(&cfg, arch, Mode::Shards)
            .unwrap()
            .decide(&Data {
                nr,
                arch: abi.audit_arch(),
                ip: 0,
                args,
            })
    };
    let allow = Some(compiler::ACT_ALLOW);
    let eperm = Some(compiler::act_errno(1));
    // mseal, listmount, statmount: allowed by Docker's own profile, ENOSYS under Docker
    // Desktop's runc (PM M119).
    for name in ["mseal", "listmount", "statmount", "fchmodat2", "cachestat"] {
        for (arch, abi) in [(Arch::Arm64, Abi::Aarch64), (Arch::Amd64, Abi::X86_64)] {
            let nr = tables::kernel_nr(abi, name).unwrap() as u32;
            assert_eq!(decide(arch, abi, nr, [0; 6]), allow, "{name} {abi:?}");
        }
    }
    // socket(2) is allowed but for AF_ALG (38) and AF_VSOCK (40), directly; the default
    // profile allows socketcall(2) outright, which both keep.
    let socket = tables::kernel_nr(Abi::X86_64, "socket").unwrap() as u32;
    assert_eq!(
        decide(Arch::Amd64, Abi::X86_64, socket, [2, 0, 0, 0, 0, 0]),
        allow
    );
    assert_eq!(
        decide(Arch::Amd64, Abi::X86_64, socket, [40, 0, 0, 0, 0, 0]),
        eperm
    );
    let x86_socket = tables::kernel_nr(Abi::X86, "socket").unwrap() as u32;
    assert_eq!(
        decide(Arch::Amd64, Abi::X86, x86_socket, [40, 0, 0, 0, 0, 0]),
        eperm
    );
    assert_eq!(
        decide(Arch::Amd64, Abi::X86, 102, [1, 0, 0, 0, 0, 0]),
        allow,
        "socketcall, listed"
    );
}

/// A profile that allows socket(2) only for AF_INET, and not socketcall(2): an i386
/// process cannot make another family's socket through socketcall, which cannot be told
/// the family, where Docker's runc lets it (libseccomp writes the call over the family's
/// comparison); socketcall's calls the profile does not compare still go through it, and
/// ipc's are read by their low 16 bits, as the kernel reads them.
#[test]
fn x86_multiplexers_keep_what_a_profile_compares() {
    let profile = br#"{"defaultAction":"SCMP_ACT_ERRNO","architectures":["SCMP_ARCH_X86_64","SCMP_ARCH_X86"],"syscalls":[
        {"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]},
        {"names":["connect","shmget"],"action":"SCMP_ACT_ALLOW"}]}"#;
    let caps: Vec<String> = Vec::new();
    let c = Container {
        arch: Arch::Amd64,
        caps: &caps,
        kernel: KERNEL,
    };
    let cfg = profile::convert(
        &profile::resolve(&profile::decode(profile).unwrap(), &c)
            .unwrap()
            .unwrap(),
    )
    .unwrap()
    .unwrap();
    let decide = |mode: Mode, nr: u32, arg0: u64| {
        compiler::compile(&cfg, Arch::Amd64, mode).unwrap().decide(&Data {
            nr,
            arch: Abi::X86.audit_arch(),
            ip: 0,
            args: [arg0, 0, 0, 0, 0, 0],
        })
    };
    let (allow, eperm) = (Some(compiler::ACT_ALLOW), Some(compiler::act_errno(1)));
    assert_eq!(
        decide(Mode::Docker, 102, 1),
        allow,
        "Docker: socketcall(SYS_SOCKET), any family"
    );
    assert_eq!(decide(Mode::Shards, 102, 1), eperm);
    assert_eq!(decide(Mode::Shards, 102, 3), allow, "socketcall(SYS_CONNECT)");
    let x86_socket = tables::kernel_nr(Abi::X86, "socket").unwrap() as u32;
    assert_eq!(decide(Mode::Shards, x86_socket, 2), allow);
    assert_eq!(decide(Mode::Shards, x86_socket, 40), eperm);
    // shmget is ipc's 23; a version in the upper half is ignored by the kernel.
    assert_eq!(decide(Mode::Shards, 117, 0x1_0017), allow);
    assert_eq!(
        decide(Mode::Docker, 117, 0x1_0017),
        eperm,
        "Docker: the whole word"
    );
}

/// A number that is no syscall of the guest kernel, below the profile's last: ENOSYS, as
/// the kernel's own answer, where runc's stub returns the default action.
#[test]
fn a_number_that_is_no_syscall_is_enosys() {
    let caps: Vec<String> = Vec::new();
    let c = Container {
        arch: Arch::Amd64,
        caps: &caps,
        kernel: KERNEL,
    };
    let cfg = profile::convert(
        &profile::resolve(&profile::decode(profile::DEFAULT).unwrap(), &c)
            .unwrap()
            .unwrap(),
    )
    .unwrap()
    .unwrap();
    assert!(!tables::kernel_has(Abi::X86_64, 400));
    let d = Data {
        nr: 400,
        arch: Abi::X86_64.audit_arch(),
        ip: 0,
        args: [0; 6],
    };
    assert_eq!(
        compiler::compile(&cfg, Arch::Amd64, Mode::Shards)
            .unwrap()
            .decide(&d),
        Some(ENOSYS)
    );
    assert_eq!(
        compiler::compile(&cfg, Arch::Amd64, Mode::Docker)
            .unwrap()
            .decide(&d),
        Some(compiler::act_errno(1))
    );
}

/// What a filter costs (platform-measurements.md M120): each program's length, and the
/// instructions it runs for each syscall of the guest kernel's tables (no arguments), for
/// Docker's default profile and capabilities, shards' program against runc's.
///
///   cargo test -p shards-seccomp --release -- --ignored --nocapture measure_programs
#[test]
#[ignore]
fn measure_programs() {
    let pct = |v: &mut Vec<usize>, p: usize| {
        v.sort_unstable();
        v[(v.len() - 1) * p / 100]
    };
    for (key, arch, abis) in [
        ("amd64", Arch::Amd64, vec![Abi::X86_64, Abi::X86]),
        ("arm64", Arch::Arm64, vec![Abi::Aarch64, Abi::Arm]),
    ] {
        let oracle = json(&format!("oracle-{key}.json"));
        let theirs = program(&oracle["cases"][0]["program"]);
        let ours = build(&cases()[0], arch, Mode::Shards)
            .unwrap()
            .unwrap()
            .program()
            .unwrap()
            .insns;
        eprintln!(
            "{key}: program length: shards {}, runc {}",
            ours.len(),
            theirs.len()
        );
        for abi in abis {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for nr in 0..1100u32 {
                if !tables::kernel_has(abi, nr as i32) {
                    continue;
                }
                let d = Data {
                    nr,
                    arch: abi.audit_arch(),
                    ip: 0,
                    args: [0; 6],
                };
                a.push(bpf::run_counted(&ours, &d).unwrap().1);
                b.push(bpf::run_counted(&theirs, &d).unwrap().1);
            }
            let n = a.len();
            let mean = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len() as f64;
            let (ma, mb) = (mean(&a), mean(&b));
            eprintln!(
                "  {abi:?} n={n}: shards mean {ma:.1} p50 {} p90 {} p99 {} max {}; runc mean {mb:.1} p50 {} p90 {} p99 {} max {}",
                pct(&mut a, 50),
                pct(&mut a, 90),
                pct(&mut a, 99),
                pct(&mut a, 100),
                pct(&mut b, 50),
                pct(&mut b, 90),
                pct(&mut b, 99),
                pct(&mut b, 100)
            );
        }
    }
}
