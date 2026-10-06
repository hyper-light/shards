//! A container's device rules, as runc v1.3.4 keeps them (opencontainers/cgroups v0.0.4,
//! devices/devices_emulator.go: cgroup v1's devices.allow and devices.deny replayed in
//! order) and compiles them for cgroup v2 (devices/devicefilter.go): an eBPF program of
//! type BPF_PROG_TYPE_CGROUP_DEVICE, which allows each access to a device that one of the
//! rules allows, in the order runc writes them, and else the default. No dependencies, so
//! that shards-init, which loads the program, builds it too.

/// A rule's device type (`a` all, `b` block, `c` char).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    All,
    Block,
    Char,
}

impl Kind {
    /// Its letter, by which runc sorts rules (a < b < c).
    pub const fn letter(self) -> char {
        match self {
            Kind::All => 'a',
            Kind::Block => 'b',
            Kind::Char => 'c',
        }
    }
}

/// Access bits: read, write, mknod (BPF_DEVCG_ACC_*).
pub const READ: u8 = 2;
pub const WRITE: u8 = 4;
pub const MKNOD: u8 = 1;
pub const RWM: u8 = READ | WRITE | MKNOD;

/// `r`, `w` and `m`, as bits; none for a letter that is none of them.
pub fn perms(s: &str) -> Option<u8> {
    s.chars().try_fold(0u8, |acc, c| {
        Some(
            acc | match c {
                'r' => READ,
                'w' => WRITE,
                'm' => MKNOD,
                _ => return None,
            },
        )
    })
}

/// A rule (devices/config Rule): a type, a major and minor (-1 for any), what may be done,
/// and whether it allows or denies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub kind: Kind,
    pub major: i64,
    pub minor: i64,
    pub perms: u8,
    pub allow: bool,
}

const ANY: i64 = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Meta {
    kind: Kind,
    major: i64,
    minor: i64,
}

/// The emulator (devices_emulator.go): the default, and the exceptions to it.
#[derive(Debug, Clone, Default)]
struct Emulator {
    default_allow: bool,
    rules: Vec<(Meta, u8)>,
}

impl Emulator {
    fn get(&self, m: &Meta) -> u8 {
        self.rules.iter().find(|(k, _)| k == m).map_or(0, |(_, p)| *p)
    }

    fn set(&mut self, m: Meta, p: u8) {
        self.rules.retain(|(k, _)| *k != m);
        if p != 0 {
            self.rules.push((m, p));
        }
    }

    fn add(&mut self, m: Meta, p: u8) {
        let now = self.get(&m) | p;
        self.set(m, now);
    }

    /// rmRule: an exception narrowed, refused where a wildcard exception holds it too.
    fn remove(&mut self, m: Meta, p: u8) -> Result<(), String> {
        for partial in [
            Meta { major: ANY, ..m },
            Meta { minor: ANY, ..m },
            Meta {
                major: ANY,
                minor: ANY,
                ..m
            },
        ] {
            if partial == m {
                continue;
            }
            let held = self.get(&partial);
            if held & p != 0 {
                return Err(format!(
                    "requested rule [{} {}] not supported by devices cgroupv1 (cannot punch hole in existing wildcard rule [{} {}])",
                    show(m),
                    show_perms(p),
                    show(partial),
                    show_perms(held)
                ));
            }
        }
        let left = self.get(&m) & !p;
        self.set(m, left);
        Ok(())
    }

    fn apply(&mut self, r: &Rule) -> Result<(), String> {
        let m = Meta {
            kind: r.kind,
            major: r.major,
            minor: r.minor,
        };
        if r.kind == Kind::All {
            *self = Emulator {
                default_allow: r.allow,
                rules: Vec::new(),
            };
            return Ok(());
        }
        let wrap = |e: String, what: &str| format!("{what}: {e}");
        match (r.allow, self.default_allow) {
            (true, true) => self
                .remove(m, r.perms)
                .map_err(|e| wrap(e, "unable to remove 'deny' exception")),
            (true, false) => {
                self.add(m, r.perms);
                Ok(())
            }
            (false, true) => {
                self.add(m, r.perms);
                Ok(())
            }
            (false, false) => self
                .remove(m, r.perms)
                .map_err(|e| wrap(e, "unable to remove 'allow' exception")),
        }
    }

    /// Rules(): the transition from a default cgroup (deny all) to this one, in runc's
    /// order (major, then minor, then type).
    fn rules(&self) -> Vec<Rule> {
        let mut out = Vec::new();
        if self.default_allow {
            out.push(Rule {
                kind: Kind::All,
                major: ANY,
                minor: ANY,
                perms: RWM,
                allow: true,
            });
        }
        let mut ordered = self.rules.clone();
        ordered.sort_by_key(|(m, _)| (m.major, m.minor, m.kind));
        for (m, p) in ordered {
            out.push(Rule {
                kind: m.kind,
                major: m.major,
                minor: m.minor,
                perms: p,
                allow: !self.default_allow,
            });
        }
        out
    }
}

/// A rule's device as Go's `%v` prints runc's deviceMeta: its type a rune's number.
fn show(m: Meta) -> String {
    format!("{{{} {} {}}}", u32::from(m.kind.letter()), m.major, m.minor)
}

fn show_perms(p: u8) -> String {
    let mut s = String::new();
    for (bit, c) in [(READ, 'r'), (WRITE, 'w'), (MKNOD, 'm')] {
        if p & bit != 0 {
            s.push(c);
        }
    }
    s
}

/// What the rules, applied in order, come to: whether what no rule names is allowed, and
/// the rules that make the exceptions, in the program's order.
pub fn reduce(rules: &[Rule]) -> Result<(bool, Vec<Rule>), String> {
    let mut emu = Emulator::default();
    for r in rules {
        emu.apply(r)?;
    }
    let clean: Vec<Rule> = emu.rules().into_iter().filter(|r| r.kind != Kind::All).collect();
    Ok((emu.default_allow, clean))
}

/// One eBPF instruction (`struct bpf_insn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u8,
    pub dst: u8,
    pub src: u8,
    pub off: i16,
    pub imm: i32,
}

impl Insn {
    /// Its bytes as the kernel reads `struct bpf_insn`, little-endian.
    pub fn to_bytes(self) -> [u8; 8] {
        let [o0, o1] = self.off.to_le_bytes();
        let [i0, i1, i2, i3] = self.imm.to_le_bytes();
        [
            self.code,
            (self.dst & 0xf) | (self.src << 4),
            o0,
            o1,
            i0,
            i1,
            i2,
            i3,
        ]
    }
}

const LDX_MEM_W: u8 = 0x61;
const ALU32_AND_K: u8 = 0x54;
const ALU32_RSH_K: u8 = 0x74;
const ALU32_MOV_X: u8 = 0xbc;
const ALU32_MOV_K: u8 = 0xb4;
const JMP_JNE_K: u8 = 0x55;
const JMP_JNE_X: u8 = 0x5d;
const EXIT: u8 = 0x95;

/// BPF_DEVCG_DEV_BLOCK and _CHAR.
const DEV_BLOCK: i32 = 1;
const DEV_CHAR: i32 = 2;

/// The license runc loads the program with.
pub const LICENSE: &str = "Apache";

/// devicefilter.go's program for `rules` (each a rule `reduce` made): the access type
/// in R2, the access in R3, major and minor in R4 and R5; each rule a block that goes on
/// to the next unless it matches, and then returns its verdict; the default at the end.
pub fn program(default_allow: bool, rules: &[Rule]) -> Result<Vec<Insn>, String> {
    let i = |code, dst, src, off, imm| Insn {
        code,
        dst,
        src,
        off,
        imm,
    };
    let mut out = vec![
        i(LDX_MEM_W, 2, 1, 0, 0),
        i(ALU32_AND_K, 2, 0, 0, 0xFFFF),
        i(LDX_MEM_W, 3, 1, 0, 0),
        i(ALU32_RSH_K, 3, 0, 0, 16),
        i(LDX_MEM_W, 4, 1, 4, 0),
        i(LDX_MEM_W, 5, 1, 8, 0),
    ];
    for r in rules {
        let kind = match r.kind {
            Kind::Char => DEV_CHAR,
            Kind::Block => DEV_BLOCK,
            Kind::All => return Err("[internal error] a wildcard rule past the first".into()),
        };
        if r.allow == default_allow {
            return Err("[internal error] a rule that changes nothing".into());
        }
        // Up to u32's most, then int32 as Go converts it; the minor's refusal names the
        // major, as runc's does.
        if r.major > i64::from(u32::MAX) {
            return Err(format!("invalid major {}", r.major));
        }
        if r.minor > i64::from(u32::MAX) {
            return Err(format!("invalid minor {}", r.major));
        }
        #[allow(clippy::cast_possible_truncation)]
        let (major, minor) = (r.major as i32, r.minor as i32);
        let has_access = r.perms != RWM;
        // The block's tests, each jumping past it, to the next block's first instruction.
        let mut tests: Vec<Insn> = vec![i(JMP_JNE_K, 2, 0, 0, kind)];
        if has_access {
            tests.push(i(ALU32_MOV_X, 1, 3, 0, 0));
            tests.push(i(ALU32_AND_K, 1, 0, 0, i32::from(r.perms)));
            tests.push(i(JMP_JNE_X, 1, 3, 0, 0));
        }
        if r.major >= 0 {
            tests.push(i(JMP_JNE_K, 4, 0, 0, major));
        }
        if r.minor >= 0 {
            tests.push(i(JMP_JNE_K, 5, 0, 0, minor));
        }
        let verdict = [i(ALU32_MOV_K, 0, 0, 0, i32::from(r.allow)), i(EXIT, 0, 0, 0, 0)];
        let len = tests.len() + verdict.len();
        for (n, t) in tests.iter_mut().enumerate() {
            if t.code == JMP_JNE_K || t.code == JMP_JNE_X {
                t.off = i16::try_from(len - n - 1).map_err(|_| "a block too long")?;
            }
        }
        out.extend(tests);
        out.extend(verdict);
    }
    out.push(i(ALU32_MOV_K, 0, 0, 0, i32::from(default_allow)));
    out.push(i(EXIT, 0, 0, 0, 0));
    Ok(out)
}

/// The program for `rules` applied in order, as its bytes.
pub fn compile(rules: &[Rule]) -> Result<Vec<u8>, String> {
    let (default_allow, clean) = reduce(rules)?;
    Ok(program(default_allow, &clean)?
        .iter()
        .flat_map(|i| i.to_bytes())
        .collect())
}

/// A device given to a container (`--device`), as runc's rules name it: its path in
/// the container, which drops runc's own rule for that path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Given {
    pub path: String,
    pub rule: Rule,
}

/// A container's rules as dockerd and runc make them (moby daemon/oci_linux.go
/// WithDevices; runc specconv createDevices and CreateCgroupConfig): moby's defaults,
/// then each device given and each `--device-cgroup-rule`, then runc's own, less those
/// whose path a device given takes; a privileged one's, everything, then runc's own.
///
/// Where runc reads any `a` rule as every device, all of read, write and mknod, whatever
/// its numbers and access say (`a 1:2 m` and `a *:* r` allow everything), shards reads
/// one that is not `a *:* rwm` as what it says: a block and a char rule of its numbers
/// and access.
pub fn container(given: &[Given], rules: &[Rule], privileged: bool) -> Vec<Rule> {
    let mut out = if privileged {
        vec![Rule {
            kind: Kind::All,
            major: ANY,
            minor: ANY,
            perms: RWM,
            allow: true,
        }]
    } else {
        let mut out = moby_defaults();
        out.extend(given.iter().map(|g| g.rule));
        for r in rules {
            if r.kind == Kind::All && (r.major != ANY || r.minor != ANY || r.perms != RWM) {
                out.push(Rule {
                    kind: Kind::Block,
                    ..*r
                });
                out.push(Rule {
                    kind: Kind::Char,
                    ..*r
                });
            } else {
                out.push(*r);
            }
        }
        out
    };
    out.extend(
        RUNC_ALLOWED
            .iter()
            .filter(|(path, _)| path.is_empty() || given.iter().all(|g| g.path != *path))
            .map(|(_, r)| *r),
    );
    out
}

/// moby's default rules for a container (daemon/pkg/oci/defaults.go): none but
/// /dev/null, zero, urandom, random, tty and console, and /dev/fuse denied.
pub fn moby_defaults() -> Vec<Rule> {
    let c = |major, minor, allow| Rule {
        kind: Kind::Char,
        major,
        minor,
        perms: RWM,
        allow,
    };
    vec![
        Rule {
            kind: Kind::All,
            major: ANY,
            minor: ANY,
            perms: RWM,
            allow: false,
        },
        c(1, 5, true),
        c(1, 3, true),
        c(1, 9, true),
        c(1, 8, true),
        c(5, 0, true),
        c(5, 1, true),
        c(10, 229, false),
    ]
}

const fn allowed(kind: Kind, major: i64, minor: i64, perms: u8) -> Rule {
    Rule {
        kind,
        major,
        minor,
        perms,
        allow: true,
    }
}

/// The paths runc's own devices are at, which a device given at one takes the place of.
pub const RUNC_PATHS: [&str; 6] = [
    "/dev/null",
    "/dev/random",
    "/dev/full",
    "/dev/tty",
    "/dev/zero",
    "/dev/urandom",
];

/// runc's AllowedDevices (libcontainer/specconv), appended after a container's own, each
/// with its path: mknod of any device, /dev/null, random, full, tty, zero, urandom, the
/// ptys and ptmx, and /dev/net/tun.
const RUNC_ALLOWED: [(&str, Rule); 11] = [
    ("", allowed(Kind::Char, ANY, ANY, MKNOD)),
    ("", allowed(Kind::Block, ANY, ANY, MKNOD)),
    ("/dev/null", allowed(Kind::Char, 1, 3, RWM)),
    ("/dev/random", allowed(Kind::Char, 1, 8, RWM)),
    ("/dev/full", allowed(Kind::Char, 1, 7, RWM)),
    ("/dev/tty", allowed(Kind::Char, 5, 0, RWM)),
    ("/dev/zero", allowed(Kind::Char, 1, 5, RWM)),
    ("/dev/urandom", allowed(Kind::Char, 1, 9, RWM)),
    ("", allowed(Kind::Char, 136, ANY, RWM)),
    ("", allowed(Kind::Char, 5, 2, RWM)),
    ("", allowed(Kind::Char, 10, 200, RWM)),
];

/// Whether `program` allows an access of `access` to a `kind` device `major:minor`,
/// as the kernel runs it: for tests.
pub fn run(program: &[Insn], kind: Kind, access: u8, major: u32, minor: u32) -> Option<bool> {
    let kind = match kind {
        Kind::Char => DEV_CHAR as u32,
        Kind::Block => DEV_BLOCK as u32,
        Kind::All => return None,
    };
    let ctx = [(u32::from(access) << 16) | kind, major, minor];
    let mut regs = [0u64; 11];
    let mut pc = 0usize;
    for _ in 0..=program.len() {
        let i = program.get(pc)?;
        let (d, s) = (usize::from(i.dst), usize::from(i.src));
        let imm = i64::from(i.imm) as u64;
        let mut next = pc + 1;
        match i.code {
            LDX_MEM_W => {
                let at = usize::try_from(i.off).ok()? / 4;
                *regs.get_mut(d)? = u64::from(*ctx.get(at)?);
            }
            ALU32_AND_K => *regs.get_mut(d)? = (*regs.get(d)? as u32 & i.imm as u32).into(),
            ALU32_RSH_K => *regs.get_mut(d)? = ((*regs.get(d)? as u32) >> i.imm).into(),
            ALU32_MOV_X => *regs.get_mut(d)? = u64::from(*regs.get(s)? as u32),
            ALU32_MOV_K => *regs.get_mut(d)? = u64::from(i.imm as u32),
            JMP_JNE_K | JMP_JNE_X => {
                let b = if i.code == JMP_JNE_K { imm } else { *regs.get(s)? };
                if *regs.get(d)? != b {
                    next = pc.checked_add_signed(isize::from(i.off))?.checked_add(1)?;
                }
            }
            EXIT => return Some(regs[0] != 0),
            _ => return None,
        }
        pc = next;
    }
    None
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn docker(extra: &[Rule]) -> Vec<Rule> {
        container(&[], extra, false)
    }

    /// A default container's: its few devices read and written, any device made, no
    /// other read: /dev/pmem0 (259:0), which shards' guests mount their image from, is not.
    #[test]
    fn a_default_container_reaches_dockers_devices_alone() {
        let (allow, clean) = reduce(&docker(&[])).unwrap();
        assert!(!allow);
        let p = program(allow, &clean).unwrap();
        for (major, minor) in [
            (1, 3),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (5, 0),
            (5, 2),
            (136, 4),
            (10, 200),
        ] {
            assert_eq!(
                run(&p, Kind::Char, READ | WRITE, major, minor),
                Some(true),
                "{major}:{minor}"
            );
        }
        assert_eq!(run(&p, Kind::Block, READ, 259, 0), Some(false));
        assert_eq!(run(&p, Kind::Block, MKNOD, 259, 0), Some(true));
        assert_eq!(
            run(&p, Kind::Char, READ, 10, 229),
            Some(false),
            "fuse, denied unless asked"
        );
        assert_eq!(run(&p, Kind::Char, READ, 5, 1), Some(true), "console, moby's own");
        assert_eq!(
            run(&p, Kind::Char, READ, 4, 0),
            Some(false),
            "a tty runc does not name"
        );
    }

    /// `--device /dev/fuse`: its rule after moby's denial of it allows it.
    #[test]
    fn a_device_given_is_reached() {
        let fuse = allowed(Kind::Char, 10, 229, RWM);
        let (allow, clean) = reduce(&docker(&[fuse])).unwrap();
        let p = program(allow, &clean).unwrap();
        assert_eq!(run(&p, Kind::Char, READ | WRITE, 10, 229), Some(true));
        let read_only = Rule { perms: READ, ..fuse };
        let (allow, clean) = reduce(&docker(&[read_only])).unwrap();
        let p = program(allow, &clean).unwrap();
        assert_eq!(run(&p, Kind::Char, READ, 10, 229), Some(true));
        assert_eq!(run(&p, Kind::Char, WRITE, 10, 229), Some(false));
    }

    /// A privileged container's: everything.
    #[test]
    fn a_privileged_container_reaches_every_device() {
        let (allow, clean) = reduce(&container(&[], &[], true)).unwrap();
        assert!(allow && clean.is_empty());
        assert_eq!(
            run(&program(allow, &clean).unwrap(), Kind::Block, READ, 259, 0),
            Some(true)
        );
    }

    /// `a` rules that are not every device's every access: what they say, where runc
    /// allows everything.
    #[test]
    fn a_narrow_all_rule_allows_what_it_says() {
        let r = allowed(Kind::All, ANY, ANY, READ);
        let (allow, clean) = reduce(&docker(&[r])).unwrap();
        let p = program(allow, &clean).unwrap();
        assert_eq!(run(&p, Kind::Block, READ, 259, 0), Some(true));
        assert_eq!(run(&p, Kind::Block, WRITE, 259, 0), Some(false));
        let r = allowed(Kind::All, 1, 2, MKNOD);
        let (allow, clean) = reduce(&docker(&[r])).unwrap();
        let p = program(allow, &clean).unwrap();
        assert_eq!(run(&p, Kind::Block, READ, 259, 0), Some(false));
        let all = allowed(Kind::All, ANY, ANY, RWM);
        assert_eq!(reduce(&docker(&[all])).unwrap(), (true, Vec::new()));
    }

    /// A device given at a path runc names drops runc's rule for it.
    #[test]
    fn a_device_at_runcs_path_takes_its_rule() {
        let named: Vec<&str> = RUNC_ALLOWED
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| !p.is_empty())
            .collect();
        assert_eq!(named, RUNC_PATHS);
        let fuse = Given {
            path: "/dev/full".into(),
            rule: allowed(Kind::Char, 10, 229, RWM),
        };
        let (allow, clean) = reduce(&container(&[fuse], &[], false)).unwrap();
        let p = program(allow, &clean).unwrap();
        assert_eq!(
            run(&p, Kind::Char, READ, 1, 7),
            Some(false),
            "runc's alone named it"
        );
        assert_eq!(run(&p, Kind::Char, READ, 10, 229), Some(true));
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod oracle;
