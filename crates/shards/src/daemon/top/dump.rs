//! The guest's process list (init procs.rs) read as procps's library reads `/proc`
//! (procps-ng 4.0.2 library/readproc.c: stat2proc, status2proc, fill_cmdline_cvt;
//! library/pids.c's items; library/pwcache.c; library/wchan.c; library/devname.c).

/// Separators of the dump (init procs.rs).
const RS: u8 = 0x1e;
const US: u8 = 0x1f;
/// readproc.c's MAX_BUFSZ, the most of a command line read.
const MAX_BUFSZ: usize = 1024 * 64 * 2;
/// pwcache.h's P_G_SZ: names this long or longer show as numbers.
const P_G_SZ: usize = 33;

/// What ps reads of the system as a whole.
#[derive(Debug)]
pub(super) struct System {
    /// `/proc/stat`'s btime, as output.c's boot_time() keeps it: an unsigned int.
    pub btime: u32,
    /// The clock tick (procps_hertz_get).
    pub hz: u64,
    /// getpagesize().
    pub page_size: u64,
    /// `/proc/meminfo`'s MemTotal, in KiB.
    pub mem_total: u64,
    /// The uptime in ticks, as pids.c computes it: `up_secs * hertz`.
    pub boot_tics: u64,
    /// time(NULL) as ps starts (output.c's seconds_since_1970).
    pub now: i64,
    passwd: Vec<u8>,
    group: Vec<u8>,
}

/// One process, as readproc.c fills a proc_t.
#[derive(Debug, Clone, Default)]
pub(super) struct Proc {
    pub tid: i32,
    pub tgid: i32,
    /// The command name, escaped (stat2proc).
    pub cmd: Vec<u8>,
    pub state: u8,
    pub ppid: i32,
    pub pgrp: i32,
    pub session: i32,
    pub tty: i32,
    pub tpgid: i32,
    pub flags: u64,
    pub min_flt: u64,
    pub cmin_flt: u64,
    pub maj_flt: u64,
    pub cmaj_flt: u64,
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
    pub priority: i32,
    pub nice: i32,
    pub nlwp: i32,
    pub start_time: u64,
    pub vsize: u64,
    pub rss_rlim: u64,
    pub start_code: u64,
    pub end_code: u64,
    pub start_stack: u64,
    pub kstk_esp: u64,
    pub kstk_eip: u64,
    pub processor: i32,
    pub rtprio: i32,
    pub sched: i32,
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fgid: u32,
    pub vm_data: u64,
    pub vm_exe: u64,
    pub vm_lock: u64,
    pub vm_lib: u64,
    pub vm_rss: u64,
    pub vm_size: u64,
    pub vm_stack: u64,
    /// Pending signals of the process (ShdPnd, or SigPnd), blocked, caught, ignored,
    /// and the thread's pending (SigPnd).
    pub signal: Vec<u8>,
    pub blocked: Vec<u8>,
    pub sigcatch: Vec<u8>,
    pub sigignore: Vec<u8>,
    pub sigpnd: Vec<u8>,
    /// The supplementary groups, comma-separated, or `-`.
    pub supgid: Vec<u8>,
    /// The command line as ps shows it (fill_cmdline_cvt).
    pub cmdline: Vec<u8>,
    /// What the process waits in (lookup_wchan).
    pub wchan: Vec<u8>,
}

/// A malformed dump.
fn malformed(what: &str) -> String {
    format!("the guest's process list is malformed ({what})")
}

/// The system and its processes, in the dump's order.
pub(super) fn parse(dump: &[u8]) -> Result<(System, Vec<Proc>), String> {
    let mut records = dump.split(|&b| b == RS);
    let head = records.next().ok_or_else(|| malformed("empty"))?;
    let mut f = head.split(|&b| b == US);
    if f.next() != Some(&b"shards-processes/1"[..]) {
        return Err(malformed("no version"));
    }
    let mut number = |what: &str| -> Result<String, String> {
        let field = f.next().ok_or_else(|| malformed(what))?;
        Ok(String::from_utf8_lossy(field).trim().to_string())
    };
    let btime: u64 = number("btime")?.parse().map_err(|_| malformed("btime"))?;
    let hz: u64 = number("clock tick")?
        .parse()
        .map_err(|_| malformed("clock tick"))?;
    let page_size: u64 = number("page size")?.parse().map_err(|_| malformed("page size"))?;
    let mem_total: u64 = number("MemTotal")?.parse().map_err(|_| malformed("MemTotal"))?;
    let uptime: f64 = number("uptime")?.parse().map_err(|_| malformed("uptime"))?;
    let now: i64 = number("time")?.parse().map_err(|_| malformed("time"))?;
    // ps divides by each of these (output.c pr_bsdtime, pr_sz, pr_pmem).
    if hz == 0 || page_size < 1024 || mem_total == 0 {
        return Err(malformed("clock tick, page size or MemTotal"));
    }
    let passwd = f.next().unwrap_or_default().to_vec();
    let group = f.next().unwrap_or_default().to_vec();
    let system = System {
        // boot_time() keeps an unsigned int (output.c).
        btime: btime as u32,
        hz,
        page_size,
        mem_total,
        // pids.c: `info->boot_tics = up_secs * info->hertz`, a double made integral.
        boot_tics: (uptime * hz as f64) as u64,
        now,
        passwd,
        group,
    };
    let mut procs = Vec::new();
    for record in records.filter(|r| !r.is_empty()) {
        let mut f = record.split(|&b| b == US);
        let stat = f.next().unwrap_or_default();
        let status = f.next().unwrap_or_default();
        let cmdline = f.next().unwrap_or_default();
        let wchan = f.next().unwrap_or_default();
        let mut p = Proc::default();
        stat2proc(stat, &mut p);
        status2proc(status, &mut p);
        p.cmdline = fill_cmdline(cmdline, &p);
        p.wchan = lookup_wchan(wchan);
        procs.push(p);
    }
    Ok((system, procs))
}

impl System {
    /// getpwuid's name for `uid` as pwcache_get_user keeps it: the number when there is
    /// none, or when it is too long.
    pub(super) fn user(&self, uid: u32) -> Vec<u8> {
        name_of(&self.passwd, uid).unwrap_or_else(|| uid.to_string().into_bytes())
    }

    /// getgrgid's name for `gid`, as pwcache_get_group keeps it.
    pub(super) fn group(&self, gid: u32) -> Vec<u8> {
        name_of(&self.group, gid).unwrap_or_else(|| gid.to_string().into_bytes())
    }

    /// getpwnam's uid for `name`.
    pub(super) fn uid_named(&self, name: &[u8]) -> Option<u32> {
        id_named(&self.passwd, name)
    }

    /// getgrnam's gid for `name`.
    pub(super) fn gid_named(&self, name: &[u8]) -> Option<u32> {
        id_named(&self.group, name)
    }
}

/// The entries of a passwd or group file: name and id (the first and third fields),
/// in order, as glibc's files backend reads them.
fn entries(file: &[u8]) -> impl Iterator<Item = (&[u8], u32)> {
    file.split(|&b| b == b'\n').filter_map(|line| {
        let mut fields = line.split(|&b| b == b':');
        let name = fields.next()?;
        let id = std::str::from_utf8(fields.nth(1)?).ok()?.parse().ok()?;
        (!name.is_empty() && !matches!(name.first(), Some(b'#' | b'+' | b'-'))).then_some((name, id))
    })
}

fn name_of(file: &[u8], id: u32) -> Option<Vec<u8>> {
    let (name, _) = entries(file).find(|&(_, i)| i == id)?;
    (name.len() < P_G_SZ).then(|| name.to_vec())
}

fn id_named(file: &[u8], name: &[u8]) -> Option<u32> {
    entries(file).find(|&(n, _)| n == name).map(|(_, id)| id)
}

/// strtol and strtoul as the C library reads a number: leading white space, a sign,
/// then digits in `base` (0: by prefix); the value (saturated, as glibc saturates) and
/// where the digits end, which is the start when there are none.
pub(super) fn strtol(s: &[u8], base: u32) -> (i128, usize) {
    let mut i = s.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let mut negative = false;
    if let Some(&c @ (b'+' | b'-')) = s.get(i) {
        negative = c == b'-';
        i += 1;
    }
    let mut base = base;
    let hex_prefix = matches!(s.get(i), Some(b'0'))
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && s.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
    if (base == 0 || base == 16) && hex_prefix {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if s.get(i) == Some(&b'0') { 8 } else { 10 };
    }
    let start = i;
    let mut value: i128 = 0;
    while let Some(d) = s.get(i).and_then(|&c| (c as char).to_digit(base)) {
        value = value
            .saturating_mul(i128::from(base))
            .saturating_add(i128::from(d));
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    // Far past any 64-bit value: glibc saturates there.
    let value = value.min(i128::from(u64::MAX) + 1);
    (if negative { -value } else { value }, i)
}

/// strtol's value as a C long.
pub(super) fn as_long(v: i128) -> i64 {
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// strtoul's value as a C unsigned long: out of range is ULONG_MAX, and a negative
/// value is negated in unsigned arithmetic.
pub(super) fn as_ulong(v: i128) -> u64 {
    if v > i128::from(u64::MAX) || v < -i128::from(u64::MAX) {
        u64::MAX
    } else if v < 0 {
        (v.unsigned_abs() as u64).wrapping_neg()
    } else {
        v as u64
    }
}

/// library/escape.c's escape_str in the C locale: at most `bufsize - 1` bytes, up to a
/// NUL, with control characters as `.` and bytes past ASCII as `?` (esc_all).
fn lib_escape(src: &[u8], bufsize: usize) -> Vec<u8> {
    src.iter()
        .take_while(|&&c| c != 0)
        .take(bufsize.saturating_sub(1))
        .map(|&c| match c {
            0x20..=0x7e => c,
            0x80..=0xff => b'?',
            _ => b'.',
        })
        .collect()
}

/// stat2proc: the command name between the first `(` and the last `)`, then the
/// fields after it as its sscanf reads them, stopping at the first that is not a number.
fn stat2proc(s: &[u8], p: &mut Proc) {
    p.processor = 0;
    p.rtprio = -1;
    p.sched = -1;
    p.nlwp = 0;
    let s = s.split(|&b| b == 0).next().unwrap_or_default();
    let Some(open) = s.iter().position(|&b| b == b'(') else {
        return;
    };
    let after = s.get(open + 1..).unwrap_or_default();
    let Some(close) = after.iter().rposition(|&b| b == b')') else {
        return;
    };
    if after.get(close + 1).is_none() {
        return;
    }
    let raw = after.get(..close.min(63)).unwrap_or_default();
    p.cmd = lib_escape(raw, 64);
    let rest = after.get(close + 2..).unwrap_or_default();
    let Some(&state) = rest.first() else {
        return;
    };
    p.state = state;
    let tokens: Vec<&[u8]> = rest
        .get(1..)
        .unwrap_or_default()
        .split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    let mut tokens = tokens.into_iter();
    let mut stopped = false;
    // Each conversion in turn; a number that does not end its token is assigned and is
    // the last, as with sscanf.
    let mut next = |any: bool| -> Option<i128> {
        if stopped {
            return None;
        }
        let t = tokens.next()?;
        if any {
            return Some(0);
        }
        let (v, end) = strtol(t, 10);
        if end == 0 {
            stopped = true;
            return None;
        }
        stopped = end != t.len();
        Some(v)
    };
    let int = |v: i128| as_long(v) as i32;
    let first: Vec<i128> = std::iter::from_fn(|| next(false)).take(27).collect();
    for (i, v) in first.iter().enumerate() {
        match i {
            0 => p.ppid = int(*v),
            1 => p.pgrp = int(*v),
            2 => p.session = int(*v),
            3 => p.tty = int(*v),
            4 => p.tpgid = int(*v),
            5 => p.flags = as_ulong(*v),
            6 => p.min_flt = as_ulong(*v),
            7 => p.cmin_flt = as_ulong(*v),
            8 => p.maj_flt = as_ulong(*v),
            9 => p.cmaj_flt = as_ulong(*v),
            10 => p.utime = as_ulong(*v),
            11 => p.stime = as_ulong(*v),
            12 => p.cutime = as_ulong(*v),
            13 => p.cstime = as_ulong(*v),
            14 => p.priority = int(*v),
            15 => p.nice = int(*v),
            16 => p.nlwp = int(*v),
            18 => p.start_time = as_ulong(*v),
            19 => p.vsize = as_ulong(*v),
            21 => p.rss_rlim = as_ulong(*v),
            22 => p.start_code = as_ulong(*v),
            23 => p.end_code = as_ulong(*v),
            24 => p.start_stack = as_ulong(*v),
            25 => p.kstk_esp = as_ulong(*v),
            26 => p.kstk_eip = as_ulong(*v),
            _ => {}
        }
    }
    // Then the four signal masks, whatever they hold (%*s), and the former wchan,
    // nswap, cnswap, exit_signal before the processor, priority and policy.
    if first.len() == 27 && (0..4).all(|_| next(true).is_some()) {
        let last: Vec<i128> = std::iter::from_fn(|| next(false)).take(7).collect();
        if let Some(&v) = last.get(4) {
            p.processor = int(v);
        }
        if let Some(&v) = last.get(5) {
            p.rtprio = int(v);
        }
        if let Some(&v) = last.get(6) {
            p.sched = int(v);
        }
    }
    if p.nlwp == 0 {
        p.nlwp = 1;
    }
}

/// status2proc: the lines it knows, by name; it stops at a line without a colon and
/// tab after its name, as procps does.
fn status2proc(s: &[u8], p: &mut Proc) {
    let s = s.split(|&b| b == 0).next().unwrap_or_default();
    let (mut threads, mut tgid, mut pid) = (0_i64, 0_i64, 0_i64);
    let mut shdpnd: Option<Vec<u8>> = None;
    let mut supgid: Option<Vec<u8>> = None;
    let mut at = 0;
    loop {
        let line = s.get(at..).unwrap_or_default();
        if line.len() < 4 {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            break;
        };
        if line.get(colon + 1) != Some(&b'\t') {
            break;
        }
        let name = line.get(..colon).unwrap_or_default();
        let value = line.get(colon + 2..).unwrap_or_default();
        let ids = |v: &[u8]| -> [u32; 4] {
            let mut out = [0_u32; 4];
            let mut at = 0;
            for slot in &mut out {
                let (n, end) = strtol(v.get(at..).unwrap_or_default(), 10);
                *slot = as_long(n) as u32;
                at += end;
            }
            out
        };
        let kb = |v: &[u8]| as_long(strtol(v, 10).0) as u64;
        let mask = |v: &[u8]| -> Vec<u8> { v.iter().take(16).take_while(|&&b| b != 0).copied().collect() };
        match name {
            b"Name" if p.cmd.is_empty() => {
                // Unescape the kernel's `\n` and `\\` (status2proc case_Name).
                let mut raw = Vec::new();
                let mut it = value.iter();
                while raw.len() < 63 {
                    let Some(&c) = it.next() else { break };
                    let c = match c {
                        b'\n' => break,
                        b'\\' => match it.next() {
                            None | Some(b'\n') => break,
                            Some(b'n') => b'\n',
                            Some(&c) => c,
                        },
                        c => c,
                    };
                    raw.push(c);
                }
                p.cmd = lib_escape(&raw, 64);
            }
            b"ShdPnd" => shdpnd = Some(mask(value)),
            b"SigBlk" => p.blocked = mask(value),
            b"SigCgt" => p.sigcatch = mask(value),
            b"SigIgn" => p.sigignore = mask(value),
            b"SigPnd" => p.sigpnd = mask(value),
            b"State" => p.state = value.first().copied().unwrap_or(0),
            b"Tgid" => tgid = as_long(strtol(value, 10).0),
            b"Pid" => pid = as_long(strtol(value, 10).0),
            b"PPid" => p.ppid = as_long(strtol(value, 10).0) as i32,
            b"Threads" => threads = as_long(strtol(value, 10).0),
            b"Uid" => [p.ruid, p.euid, p.suid, p.fuid] = ids(value),
            b"Gid" => [p.rgid, p.egid, p.sgid, p.fgid] = ids(value),
            b"VmData" => p.vm_data = kb(value),
            b"VmExe" => p.vm_exe = kb(value),
            b"VmLck" => p.vm_lock = kb(value),
            b"VmLib" => p.vm_lib = kb(value),
            b"VmRSS" => p.vm_rss = kb(value),
            b"VmSize" => p.vm_size = kb(value),
            b"VmStk" => p.vm_stack = kb(value),
            b"Groups" => {
                // Blank-separated, with a trailing blank, made comma-separated.
                let list = value.split(|&b| b == b'\n').next().unwrap_or_default();
                let list: Vec<u8> = list
                    .iter()
                    .copied()
                    .skip_while(|&b| b == b' ' || b == b'\t')
                    .collect();
                if !list.is_empty() {
                    let list = list.strip_suffix(b" ").unwrap_or(&list);
                    supgid = Some(list.iter().map(|&b| if b == b' ' { b',' } else { b }).collect());
                }
            }
            _ => {}
        }
        match line.iter().position(|&b| b == b'\n') {
            Some(nl) => at += nl + 1,
            None => break,
        }
    }
    // Recent kernels give the process's pending signals as ShdPnd.
    p.signal = match shdpnd {
        Some(s) if !s.is_empty() => s,
        _ => p.sigpnd.clone(),
    };
    if threads != 0 {
        p.nlwp = threads as i32;
        p.tgid = tgid as i32;
        p.tid = pid as i32;
    } else {
        p.nlwp = 1;
        p.tgid = pid as i32;
        p.tid = pid as i32;
    }
    p.supgid = supgid.unwrap_or_else(|| b"-".to_vec());
}

/// fill_cmdline_cvt: the arguments joined by blanks (read_unvectored), escaped; or, for
/// a process without them, its name in brackets, `<defunct>` after a zombie's.
fn fill_cmdline(raw: &[u8], p: &Proc) -> Vec<u8> {
    let n = raw.len().min(MAX_BUFSZ - 1);
    let mut dst = raw.get(..n).unwrap_or_default().to_vec();
    let out = if n > 0 {
        let kept = dst.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        for b in dst.iter_mut().take(kept) {
            if *b == b'\n' || *b == 0 {
                *b = b' ';
            }
        }
        if let Some(last) = dst.last_mut()
            && *last == b' '
        {
            *last = 0;
        }
        lib_escape(&dst, MAX_BUFSZ)
    } else {
        let mut out = vec![b'['];
        out.extend(lib_escape(&p.cmd, MAX_BUFSZ - 2));
        out.push(b']');
        if p.state == b'Z' {
            out.extend_from_slice(b" <defunct>");
        }
        out
    };
    if out.is_empty() { b"?".to_vec() } else { out }
}

/// lookup_wchan: `-` for a running process (the kernel's `0`), `?` for nothing read,
/// else the symbol without a leading `.` and underscores.
fn lookup_wchan(raw: &[u8]) -> Vec<u8> {
    let buf = raw.get(..raw.len().min(63)).unwrap_or_default();
    if buf.is_empty() {
        return b"?".to_vec();
    }
    let buf = buf.split(|&b| b == 0).next().unwrap_or_default();
    if buf == b"0" {
        return b"-".to_vec();
    }
    let buf = buf.strip_prefix(b".").unwrap_or(buf);
    buf.iter().copied().skip_while(|&b| b == b'_').collect()
}

/// glibc's major and minor of a device number.
fn major(dev: u32) -> u32 {
    (dev >> 8) & 0xfff
}

fn minor(dev: u32) -> u32 {
    (dev & 0xff) | ((dev >> 12) & 0xfff00)
}

/// The device file of a terminal, as dev_to_tty finds it on a standard Linux `/dev`:
/// /proc/tty/drivers' pseudo-terminals, consoles and virtio consoles, then guess_name's
/// table (devname.c). None for none.
fn tty_path(dev: i32) -> Option<String> {
    let dev = dev as u32;
    if dev == 0 {
        return None;
    }
    let (maj, min) = (major(dev), minor(dev));
    let path = match maj {
        5 => match min {
            0 => "/dev/tty".to_string(),
            1 => "/dev/console".to_string(),
            2 => "/dev/ptmx".to_string(),
            _ => return None,
        },
        229 => format!("/dev/hvc{min}"),
        204 if min >= 64 => format!("/dev/ttyAMA{}", min - 64),
        3 => {
            let t0 = *b"pqrstuvwxyzabcde".get((min >> 4) as usize)?;
            let t1 = *b"0123456789abcdef".get((min & 0x0f) as usize)?;
            format!("/dev/tty{}{}", t0 as char, t1 as char)
        }
        4 if min < 64 => format!("/dev/tty{min}"),
        4 => format!("/dev/ttyS{}", min - 64),
        136..=143 => format!("/dev/pts/{}", min + (maj - 136) * 256),
        _ => {
            let prefix = match maj {
                11 => "ttyB",
                17 => "ttyH",
                19 => "ttyC",
                22 | 23 => "ttyD",
                24 => "ttyE",
                32 => "ttyX",
                43 => "ttyI",
                46 => "ttyR",
                48 => "ttyL",
                57 => "ttyP",
                71 => "ttyF",
                75 => "ttyW",
                78 | 112 => "ttyM",
                105 => "ttyV",
                148 => "ttyT",
                154 => "ttySR",
                156 => return Some(format!("/dev/ttySR{}", min + 256)),
                164 => "ttyCH",
                166 => "ttyACM",
                172 => "ttyMX",
                174 => "ttySI",
                188 => "ttyUSB",
                208 => "ttyU",
                216 => "ttyUB",
                224 => "ttyY",
                227 => "3270/tty",
                256 => "ttyEQ",
                _ => return None,
            };
            format!("/dev/{prefix}{min}")
        }
    };
    Some(path)
}

/// dev_to_tty with `ABBREV_DEV` (TTY_NAME), and also `ABBREV_TTY | ABBREV_PTS`
/// (`numbers`, TTY_NUMBER): `?` for none, characters outside ASCII's printable as `?`.
pub(super) fn tty_name(dev: i32, numbers: bool) -> Vec<u8> {
    let Some(path) = tty_path(dev) else {
        return b"?".to_vec();
    };
    let mut name = path.as_bytes();
    for (prefix, wanted) in [(&b"/dev/"[..], true), (b"tty", numbers), (b"pts/", numbers)] {
        if wanted
            && let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            name = rest;
        }
    }
    name.iter()
        .take(64)
        .map(|&c| if c <= b' ' || c > 126 { b'?' } else { c })
        .collect()
}

/// The terminals a `/dev` lookup finds (parser.c parse_tty): those of the processes, and
/// the console and controlling-terminal devices every system has.
pub(super) fn tty_device(path: &[u8], procs: &[Proc]) -> Option<i32> {
    let fixed = [(5 << 8), (5 << 8) | 1, (5 << 8) | 2];
    procs
        .iter()
        .map(|p| p.tty)
        .chain(fixed)
        .find(|&dev| tty_path(dev).is_some_and(|p| p.as_bytes() == path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_are_named_as_devname_names_them() {
        assert_eq!(tty_name(34816, false), b"pts/0");
        assert_eq!(tty_name(34816, true), b"0");
        assert_eq!(tty_name((4 << 8) | 65, false), b"ttyS1");
        assert_eq!(tty_name((4 << 8) | 65, true), b"S1");
        assert_eq!(tty_name(229 << 8, false), b"hvc0");
        assert_eq!(tty_name((5 << 8) | 1, true), b"console");
        assert_eq!(tty_name(0, false), b"?");
        assert_eq!(tty_name((250 << 8) | 3, false), b"?");
        // A pseudo-terminal past 255 (minor's high bits).
        assert_eq!(tty_name((136 << 8) | (1 << 20) | 4, false), b"pts/260");
    }

    #[test]
    fn numbers_read_as_strtol_reads_them() {
        assert_eq!(strtol(b"  42x", 10), (42, 4));
        assert_eq!(strtol(b"0x1f", 0), (31, 4));
        assert_eq!(strtol(b"010", 0), (8, 3));
        assert_eq!(strtol(b"-3", 0), (-3, 2));
        assert_eq!(strtol(b"x", 10), (0, 0));
        assert_eq!(as_ulong(-1), u64::MAX);
        assert_eq!(as_ulong(strtol(b"99999999999999999999999", 10).0), u64::MAX);
    }

    #[test]
    fn command_lines_are_joined_and_escaped() {
        let p = Proc {
            cmd: b"sleep".to_vec(),
            state: b'Z',
            ..Proc::default()
        };
        assert_eq!(fill_cmdline(b"", &p), b"[sleep] <defunct>");
        assert_eq!(fill_cmdline(b"a\0b\tc\0\0", &p), b"a b.c");
        assert_eq!(fill_cmdline(b"\0\0", &p), b"?");
        assert_eq!(fill_cmdline(b"a\n", &p), b"a");
        assert_eq!(lookup_wchan(b"0"), b"-");
        assert_eq!(lookup_wchan(b""), b"?");
        assert_eq!(lookup_wchan(b"__x"), b"x");
    }
}
