//! ps's output as procps-ng 4.0.2 makes it: each column's text (src/ps/output.c's pr_*
//! functions), the columns laid out (show_one_proc, check_header_width), which
//! processes print (src/ps/select.c want_this_proc) and in what order (display.c
//! simple_spew, fancy_spew and its forest; library/pids.c's sort functions).

use std::cmp::Ordering;

use super::args::{HEAD_MULTI, HEAD_NONE, OUTBUF_SIZE, Opts, SelKind, SortNode};
use super::clock::localtime;
use super::dump::{Proc, System, tty_name};
use super::specs::{BASE_ITEMS, CF_JUST_MASK, Pr, RIGHT, SIGNAL, Sr, UNLIMITED, USER, WCHAN, items};

/// output.c's SPACE_AMOUNT: the most blanks before a column.
const SPACE_AMOUNT: i32 = 144;
/// ps's effective uid: dockerd's, root.
const CACHED_EUID: u32 = 0;

/// What the guest does not send that a column or sort needs, if any.
pub(super) fn missing_column(pr: Pr) -> Option<&'static str> {
    Some(match pr {
        Pr::Oom => "/proc/PID/oom_score",
        Pr::OomAdj => "/proc/PID/oom_score_adj",
        Pr::Pss | Pr::Uss => "/proc/PID/smaps_rollup",
        Pr::Numa => "the NUMA node of each CPU",
        Pr::Agid | Pr::Agnice => "/proc/PID/autogroup",
        Pr::Rbytes | Pr::Rchars | Pr::Rops | Pr::Wbytes | Pr::Wcbytes | Pr::Wchars | Pr::Wops => {
            "/proc/PID/io"
        }
        _ => return None,
    })
}

pub(super) fn missing_sort(sr: Sr) -> Option<&'static str> {
    Some(match sr {
        Sr::OomScore => "/proc/PID/oom_score",
        Sr::OomAdj => "/proc/PID/oom_score_adj",
        Sr::SmapPss | Sr::SmapPrvTotal => "/proc/PID/smaps_rollup",
        Sr::ProcessorNode => "the NUMA node of each CPU",
        Sr::AutogrpId | Sr::AutogrpNice => "/proc/PID/autogroup",
        Sr::MemResPgs | Sr::MemShrPgs => "/proc/PID/statm",
        Sr::IoReadBytes
        | Sr::IoReadChars
        | Sr::IoReadOps
        | Sr::IoWriteBytes
        | Sr::IoWriteCbytes
        | Sr::IoWriteChars
        | Sr::IoWriteOps => "/proc/PID/io",
        _ => return None,
    })
}

/// The printing of one run of ps: its options, the system, and output.c's and
/// display.c's state.
pub(super) struct Out<'a> {
    o: &'a Opts,
    system: &'a System,
    utc_offset: i32,
    active_cols: u32,
    wide_signals: bool,
    max_rightward: u32,
    /// display.c's forest_prefix, up to its NUL.
    forest_prefix: Vec<u8>,
    lines_to_next_header: i32,
    header_gap: i32,
    did_stuff: bool,
    pub text: Vec<u8>,
}

impl<'a> Out<'a> {
    /// init_output (check_header_width) and check_headers.
    pub(super) fn new(o: &'a Opts, system: &'a System, utc_offset: i32) -> Self {
        let fmt = &o.format_list;
        let mut total: u32 = 0;
        let mut was_normal: u32 = 0;
        let mut sigs: u32 = 0;
        for (i, node) in fmt.iter().enumerate() {
            let width = node.width as u32;
            match node.flags & CF_JUST_MASK {
                0 => {
                    total = total.wrapping_add(width);
                    was_normal = 0;
                    continue;
                }
                SIGNAL => {
                    sigs += 1;
                    total = total.wrapping_add(width);
                }
                UNLIMITED => total = total.wrapping_add(if i + 1 < fmt.len() { width } else { 3 }),
                _ => total = total.wrapping_add(width),
            }
            total = total.wrapping_add(was_normal);
            was_normal = 1;
        }
        let screen_cols = o.screen_cols as u32;
        let mut active_cols;
        let mut i: u32 = 0;
        loop {
            i += 1;
            active_cols = screen_cols.wrapping_mul(i);
            if active_cols >= total || screen_cols.wrapping_mul(i) >= OUTBUF_SIZE as u32 / 2 {
                break;
            }
        }
        let wide_signals = total.wrapping_add(sigs.wrapping_mul(7)) <= active_cols;
        let mut out = Out {
            o,
            system,
            utc_offset,
            active_cols,
            wide_signals,
            max_rightward: OUTBUF_SIZE as u32 - 1,
            forest_prefix: Vec::new(),
            lines_to_next_header: 1,
            header_gap: -1,
            did_stuff: false,
            text: Vec::new(),
        };
        if o.header_type == HEAD_MULTI {
            out.header_gap = o.screen_rows.wrapping_sub(1);
        } else if o.header_type == HEAD_NONE || !fmt.iter().any(|n| !n.name.is_empty() && n.pr.is_some()) {
            out.lines_to_next_header = -1;
        }
        out
    }

    /// What ps printed anything; when it did not, it prints the header if due and exits
    /// failing (show_one_proc's end).
    pub(super) fn finish(&mut self) -> bool {
        if self.did_stuff {
            return true;
        }
        self.lines_to_next_header = self.lines_to_next_header.wrapping_sub(1);
        if self.lines_to_next_header == 0 {
            self.show_one_proc(None);
        }
        false
    }

    /// simple_spew and fancy_spew: the processes selected, sorted or as a forest when
    /// asked. Err with ps's message when it fails.
    pub(super) fn spew(&mut self, procs: &[Proc]) -> Result<(), String> {
        let o = self.o;
        if o.forest_type == 0 && o.sort_list.is_empty() {
            match o.selection.last() {
                Some(sel) if sel.kind == SelKind::PidQuick => {
                    // procps_pids_select takes at most FILL_ID_MAX (255) PIDs.
                    if sel.nums.len() > 255 {
                        return Err("fatal library error, reap".into());
                    }
                    for &pid in &sel.nums {
                        if let Some(p) = procs.iter().find(|p| p.tid as u32 == pid)
                            && self.want(p)
                        {
                            self.show_one_proc(Some(p));
                        }
                    }
                }
                _ => {
                    for p in procs {
                        if self.want(p) {
                            self.show_one_proc(Some(p));
                        }
                    }
                }
            }
            return Ok(());
        }
        if procs.is_empty() {
            return Err("fatal library error, reap".into());
        }
        let mut chosen: Vec<&Proc> = procs.iter().filter(|p| self.want(p)).collect();
        if chosen.is_empty() {
            return Ok(());
        }
        let mut sorts: Vec<SortNode> = Vec::new();
        if o.forest_type != 0 {
            // prep_forest_sort: by parent unless sorted otherwise, by start first.
            sorts.push(SortNode {
                sr: Sr::TicsBegan,
                pr: Pr::Stime,
                order: 1,
            });
            if o.sort_list.is_empty() {
                sorts.push(SortNode {
                    sr: Sr::IdPpid,
                    pr: Pr::Ppid,
                    order: 1,
                });
            }
        }
        sorts.extend(o.sort_list.iter().copied());
        // procps_pids_sort sorts only by items in the stack, and only with an order.
        let fetched = |sr: Sr| {
            BASE_ITEMS.contains(&sr)
                || o.format_list
                    .iter()
                    .filter_map(|n| n.pr)
                    .chain(o.sort_list.iter().map(|s| s.pr))
                    .any(|pr| items(pr).contains(&sr))
        };
        for sort in sorts.iter().filter(|s| s.order != 0 && fetched(s.sr)) {
            // glibc's qsort_r is a merge sort: stable.
            chosen.sort_by(|a, b| compare(sort, a, b, self.system));
        }
        if o.forest_type != 0 {
            self.show_forest(&chosen);
        } else {
            for p in chosen {
                self.show_one_proc(Some(p));
            }
        }
        Ok(())
    }

    /// want_this_proc.
    fn want(&self, p: &Proc) -> bool {
        let o = self.o;
        let accepted = o.all_processes
            || ((o.simple_select != 0 || o.selection.is_empty()) && self.table_accept(p))
            || listed(o, p);
        let accepted = accepted && !(o.running_only && !matches!(p.state, b'R' | b'D'));
        accepted != o.negate_selection
    }

    /// table_accept: by the simple options' table of uid, session leader and terminal.
    fn table_accept(&self, p: &Proc) -> bool {
        let index = u32::from(p.euid == CACHED_EUID)
            | u32::from(p.session == p.tgid) << 1
            | u32::from(p.tty == 0) << 2
            | u32::from(p.tty == 0) << 3;
        self.o.select_bits & (1 << index) != 0
    }

    /// show_tree for each process without a parent among them, the last first
    /// (show_forest), without recursion.
    fn show_forest(&mut self, procs: &[&Proc]) {
        for i in (0..procs.len()).rev() {
            let Some(&p) = procs.get(i) else { continue };
            if procs.iter().any(|q| q.tid == p.ppid) {
                continue;
            }
            self.show_tree(procs, i);
        }
    }

    fn set_prefix(&mut self, at: usize, c: u8) {
        self.forest_prefix.truncate(at);
        self.forest_prefix.resize(at, b' ');
        self.forest_prefix.push(c);
    }

    fn show_tree(&mut self, procs: &[&Proc], root: usize) {
        /// A process whose children are being shown: its index, level, and its next
        /// child's.
        struct Frame {
            this: usize,
            level: usize,
            next: Option<usize>,
        }
        let mut stack: Vec<Frame> = Vec::new();
        let enter = |out: &mut Self, stack: &mut Vec<Frame>, this: usize, level: usize, sibling: bool| {
            if level > 0 {
                out.set_prefix(level - 1, if sibling { b'+' } else { b'L' });
            }
            out.forest_prefix.truncate(level);
            let Some(&p) = procs.get(this) else { return };
            out.show_one_proc(Some(p));
            let Some(first) = procs.iter().position(|q| q.ppid == p.tid) else {
                return;
            };
            if level > 0 {
                out.set_prefix(level - 1, if sibling { b'|' } else { b' ' });
            }
            out.forest_prefix.truncate(level);
            stack.push(Frame {
                this,
                level,
                next: Some(first),
            });
        };
        enter(self, &mut stack, root, 0, false);
        while let Some(frame) = stack.last_mut() {
            let Some(i) = frame.next else {
                let level = frame.level;
                stack.pop();
                self.forest_prefix.truncate(level);
                continue;
            };
            let Some(parent) = procs.get(frame.this) else {
                break;
            };
            let more = procs.get(i + 1).is_some_and(|q| q.ppid == parent.tid);
            frame.next = more.then_some(i + 1);
            // init's children are its siblings, except with -H.
            let level = if parent.tid == 1 && self.o.forest_type != b'u' {
                frame.level
            } else {
                frame.level + 1
            };
            enter(self, &mut stack, i, level, more);
        }
    }

    /// show_one_proc: one line, or the header for None, the columns laid out as ps lays
    /// them out: right-justified ones padded to their width, a column that overflows
    /// taking from the blanks of those after it.
    fn show_one_proc(&mut self, p: Option<&Proc>) {
        if p.is_some() {
            self.lines_to_next_header = self.lines_to_next_header.wrapping_sub(1);
            if self.lines_to_next_header == 0 {
                self.lines_to_next_header = self.header_gap;
                self.show_one_proc(None);
            }
        }
        self.did_stuff = true;
        let fmt = &self.o.format_list;
        let (mut correct, mut actual, mut dospace): (i32, i32, i32) = (0, 0, 0);
        for (i, node) in fmt.iter().enumerate() {
            let mut legit = 0;
            let next = fmt.get(i + 1);
            let mut tmpspace = 0;
            let mut max_rightward = if next.is_some() {
                node.width as u32
            } else {
                tmpspace = correct.wrapping_sub(actual);
                if tmpspace < 1 {
                    tmpspace = dospace;
                    self.active_cols
                        .wrapping_sub(actual as u32)
                        .wrapping_sub(tmpspace as u32)
                } else {
                    self.active_cols.wrapping_sub(correct.max(actual) as u32)
                }
            };
            if max_rightward >= OUTBUF_SIZE as u32 {
                max_rightward = OUTBUF_SIZE as u32 - 1;
            }
            self.max_rightward = max_rightward;
            let (mut data, amount) = match (p, node.pr) {
                (Some(p), Some(pr)) => self.render(pr, p),
                _ => (node.name.clone(), node.name.len().min(i32::MAX as usize) as i32),
            };
            let amount = amount.clamp(0, OUTBUF_SIZE - 1);
            data.truncate(OUTBUF_SIZE as usize - 1);
            let room = self
                .active_cols
                .wrapping_sub(actual as u32)
                .wrapping_sub(tmpspace as u32);
            let mut leftpad = 0;
            match node.flags & CF_JUST_MASK {
                RIGHT => leftpad = (node.width.wrapping_sub(amount)).max(0),
                SIGNAL => {
                    if self.wide_signals {
                        leftpad = 16 - amount;
                        legit = 7;
                    } else {
                        leftpad = 9 - amount;
                    }
                    leftpad = leftpad.max(0);
                }
                USER if self.o.user_is_number => leftpad = (node.width.wrapping_sub(amount)).max(0),
                WCHAN if self.o.wchan_is_number => leftpad = (node.width.wrapping_sub(amount)).max(0),
                WCHAN | UNLIMITED if room == 0 => data.truncate(1),
                _ => {}
            }
            let mut space = correct.wrapping_sub(actual).wrapping_add(leftpad);
            if space < 1 {
                space = dospace;
            }
            let space = space.min(SPACE_AMOUNT);
            self.text.extend(std::iter::repeat_n(b' ', space.max(0) as usize));
            self.text.extend_from_slice(&data);
            let Some(next) = next else {
                self.text.push(b'\n');
                break;
            };
            actual = actual.wrapping_add(space).wrapping_add(amount);
            correct = correct.wrapping_add(node.width).wrapping_add(legit);
            if node.pr.is_some() && next.pr.is_some() {
                correct = correct.wrapping_add(1);
                dospace = 1;
            } else {
                dospace = 0;
            }
        }
    }

    /// forest_helper: the tree's prefix for a command.
    fn forest_helper(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rightward = self.max_rightward.min(OUTBUF_SIZE as u32 - 1) as i32;
        let (step, unixy) = if self.o.forest_type == b'u' {
            (2, true)
        } else {
            (4, false)
        };
        for &c in &self.forest_prefix {
            if rightward < step {
                break;
            }
            out.extend_from_slice(match (unixy, c) {
                (true, _) => b"  ",
                (false, b'L' | b'+') => b" \\_ ",
                (false, b'|') => b" |  ",
                _ => b"    ",
            });
            rightward -= step;
        }
        out
    }

    /// A column's text and the cells it takes (pr_*).
    fn render(&self, pr: Pr, p: &Proc) -> (Vec<u8>, i32) {
        let o = self.o;
        let s = self.system;
        let mr = self.max_rightward;
        let text = |t: String| {
            let n = t.len().min(i32::MAX as usize) as i32;
            (t.into_bytes(), n)
        };
        let tics = if o.include_dead_children {
            tics_all_c(p)
        } else {
            tics_all(p)
        };
        let elapsed = self.time_elapsed(p);
        let start = self.start(p);
        let tm = || localtime(start, self.utc_offset);
        match pr {
            Pr::Nop
            | Pr::Oom
            | Pr::OomAdj
            | Pr::Pss
            | Pr::Uss
            | Pr::Numa
            | Pr::Agid
            | Pr::Agnice
            | Pr::Rbytes
            | Pr::Rchars
            | Pr::Rops
            | Pr::Wbytes
            | Pr::Wcbytes
            | Pr::Wchars
            | Pr::Wops => text("-".into()),
            Pr::Args | Pr::Comm => {
                // `c` shows the name for the arguments too: Debian's 4.0.2-3 has upstream's fix
                // (debian/patches/ps_c_option, procps dd3cb089).
                let source = match pr {
                    Pr::Args if !o.bsd_c_option => &p.cmdline,
                    Pr::Comm if o.unix_f_option => &p.cmdline,
                    _ => &p.cmd,
                };
                let mut out = self.forest_helper();
                let fh = out.len() as i64;
                let mut rightward = i64::from(mr) - fh;
                escape(&mut out, source, i64::from(OUTBUF_SIZE) - fh, &mut rightward);
                // `e`'s environment is `-` here, as procps has it when unreadable: none.
                (out, (i64::from(mr) - rightward) as i32)
            }
            Pr::Fname => {
                let mut out = self.forest_helper();
                let fh = out.len() as i64;
                let mut rightward = (i64::from(mr) - fh).min(8);
                escape(&mut out, &p.cmd, i64::from(OUTBUF_SIZE) - fh, &mut rightward);
                (out, (i64::from(mr) - rightward) as i32)
            }
            // Read from files the guest does not send, which procps shows as `-` when it
            // cannot read them: cgroup, exe, the security context, systemd's, lxc.
            Pr::Cgname | Pr::Cgroup | Pr::Exe => {
                let mut out = Vec::new();
                let mut rightward = i64::from(mr);
                escape(&mut out, b"-", i64::from(OUTBUF_SIZE), &mut rightward);
                (out, (i64::from(mr) - rightward) as i32)
            }
            Pr::Context
            | Pr::Lxcname
            | Pr::Luid
            | Pr::SdUnit
            | Pr::SdSession
            | Pr::SdOuid
            | Pr::SdMachine
            | Pr::SdUunit
            | Pr::SdSeat
            | Pr::SdSlice
            | Pr::Cgroupns
            | Pr::Ipcns
            | Pr::Mntns
            | Pr::Netns
            | Pr::Pidns
            | Pr::Timens
            | Pr::Userns
            | Pr::Utsns => text("-".into()),
            Pr::Etime => {
                let mut t = elapsed as u64;
                let ss = t % 60;
                t /= 60;
                let mm = t % 60;
                t /= 60;
                let hh = t % 24;
                let dd = t / 24;
                let mut out = String::new();
                if dd > 0 {
                    out.push_str(&format!("{dd}-"));
                }
                if dd > 0 || hh > 0 {
                    out.push_str(&format!("{hh:02}:"));
                }
                out.push_str(&format!("{mm:02}:{ss:02}"));
                text(out)
            }
            Pr::Etimes => text(format!("{}", elapsed as u32)),
            Pr::C => {
                let jiffies = (elapsed * s.hz as f64) as u64;
                let pcpu = tics.wrapping_mul(100).checked_div(jiffies).unwrap_or(0) as u32;
                text(format!("{:2}", pcpu.min(99)))
            }
            Pr::Pcpu | Pr::Cp => {
                let jiffies = (elapsed * s.hz as f64) as u64;
                let pcpu = tics.wrapping_mul(1000).checked_div(jiffies).unwrap_or(0) as u32;
                if pr == Pr::Cp {
                    text(format!("{:3}", pcpu.min(999)))
                } else if pcpu > 999 {
                    text(format!("{}", pcpu / 10))
                } else {
                    text(format!("{}.{}", pcpu / 10, pcpu % 10))
                }
            }
            Pr::Pgid => text(format!("{}", p.pgrp as u32)),
            Pr::Ppid => text(format!("{}", p.ppid as u32)),
            Pr::Time => {
                let mut t = self.time_all(p) as u64;
                let ss = t % 60;
                t /= 60;
                let mm = t % 60;
                t /= 60;
                let hh = t % 24;
                let dd = t / 24;
                let days = if dd > 0 { format!("{dd}-") } else { String::new() };
                text(format!("{days}{hh:02}:{mm:02}:{ss:02}"))
            }
            Pr::Times => text(format!("{}", self.time_all(p) as u64)),
            Pr::Vsz => text(format!("{}", p.vm_size)),
            Pr::Priority => text(format!("{}", p.priority)),
            Pr::Opri => text(format!("{}", 60_i32.wrapping_add(p.priority))),
            Pr::PriFoo => text(format!("{}", p.priority.wrapping_sub(20))),
            Pr::PriBar => text(format!("{}", p.priority.wrapping_add(1))),
            Pr::PriBaz => text(format!("{}", p.priority.wrapping_add(100))),
            Pr::Pri => text(format!("{}", 39_i32.wrapping_sub(p.priority))),
            Pr::PriApi => text(format!("{}", (-1_i32).wrapping_sub(p.priority))),
            Pr::Nice => {
                if !matches!(p.sched, 0 | 3 | -1) {
                    text("-".into())
                } else {
                    text(format!("{}", p.nice))
                }
            }
            Pr::Class => text(
                match p.sched {
                    -1 => "-",
                    0 => "TS",
                    1 => "FF",
                    2 => "RR",
                    3 => "B",
                    4 => "ISO",
                    5 => "IDL",
                    6 => "DLN",
                    7 => "#7",
                    8 => "#8",
                    9 => "#9",
                    _ => "?",
                }
                .into(),
            ),
            Pr::Rtprio => {
                if matches!(p.sched, 0 | -1) {
                    text("-".into())
                } else {
                    text(format!("{}", p.rtprio))
                }
            }
            Pr::Sched => {
                if p.sched == -1 {
                    text("-".into())
                } else {
                    text(format!("{}", p.sched))
                }
            }
            Pr::Wchan => {
                let w: Vec<u8> = p.wchan.iter().take(mr as usize).copied().collect();
                let n = w.len() as i32;
                (w, n)
            }
            Pr::Tty4 | Pr::Tty8 => {
                let name = tty_name(p.tty, pr == Pr::Tty4);
                let n = name.len() as i32;
                (name, n)
            }
            Pr::Stat => {
                let mut out = vec![p.state];
                if p.nice < 0 {
                    out.push(b'<');
                }
                if p.nice > 0 {
                    out.push(b'N');
                }
                if p.vm_lock != 0 {
                    out.push(b'L');
                }
                if p.session == p.tgid {
                    out.push(b's');
                }
                if p.nlwp > 1 {
                    out.push(b'l');
                }
                if p.pgrp == p.tpgid {
                    out.push(b'+');
                }
                let n = out.len() as i32;
                (out, n)
            }
            Pr::S => (vec![p.state], 1),
            Pr::Flag => text(format!("{:o}", (p.flags >> 6) & 7)),
            Pr::Stackp => text(format!("{:016x}", p.start_stack)),
            Pr::Esp => text(format!("{:016x}", p.kstk_esp)),
            Pr::Eip => text(format!("{:016x}", p.kstk_eip)),
            Pr::Bsdtime => {
                let u = (tics / s.hz) as u32;
                text(format!("{:3}:{:02}", u / 60, u % 60))
            }
            Pr::Bsdstart => {
                let ago = self.system.now.saturating_sub(start).max(0);
                let c = tm().ctime();
                let t: String = if ago > 3600 * 24 {
                    c.chars().skip(4).take(6).collect()
                } else {
                    c.chars().skip(10).take(6).collect()
                };
                (t.into_bytes(), 6)
            }
            Pr::Sz => text(format!("{}", p.vm_size / (s.page_size / 1024))),
            Pr::Dsiz | Pr::Drs => {
                let v = if p.vsize != 0 {
                    p.vsize.wrapping_sub(p.end_code).wrapping_add(p.start_code) >> 10
                } else {
                    0
                };
                text(format!("{}", v as i64))
            }
            Pr::Tsiz | Pr::Trs => {
                let v = if p.vsize != 0 {
                    p.end_code.wrapping_sub(p.start_code) >> 10
                } else {
                    0
                };
                text(format!("{}", v as i64))
            }
            Pr::Swapable => text(format!("{}", p.vm_data.wrapping_add(p.vm_stack))),
            Pr::Size => text(format!("{}", p.vsize)),
            Pr::Minflt => text(format!(
                "{}",
                if o.include_dead_children {
                    p.min_flt.wrapping_add(p.cmin_flt)
                } else {
                    p.min_flt
                }
            )),
            Pr::Majflt => text(format!(
                "{}",
                if o.include_dead_children {
                    p.maj_flt.wrapping_add(p.cmaj_flt)
                } else {
                    p.maj_flt
                }
            )),
            Pr::Lim => {
                if p.rss_rlim == u64::MAX {
                    text("xx".into())
                } else {
                    text(format!("{:5}", p.rss_rlim >> 10))
                }
            }
            Pr::Psr => text(format!("{}", p.processor)),
            Pr::Rss => text(format!("{}", p.vm_rss)),
            Pr::Pmem => {
                let pmem = (p.vm_rss.wrapping_mul(1000) / s.mem_total).min(999);
                text(format!("{:2}.{}", pmem / 10, pmem % 10))
            }
            Pr::Lstart => {
                let c = tm().ctime();
                text(format!("{:>24.24}", c))
            }
            Pr::Stime => {
                let now = localtime(self.system.now, self.utc_offset);
                let t = tm();
                if now.year != t.year {
                    text(format!("{}", t.year))
                } else if now.yday != t.yday {
                    text(format!("{}{:02}", t.month(), t.mday))
                } else {
                    text(format!("{:02}:{:02}", t.hour, t.min))
                }
            }
            Pr::Start => {
                let mut c: Vec<u8> = tm().ctime().into_bytes();
                for i in [8, 11] {
                    if let Some(b) = c.get_mut(i)
                        && *b == b' '
                    {
                        *b = b'0';
                    }
                }
                let c = String::from_utf8_lossy(&c).into_owned();
                if (start as u64).wrapping_add(60 * 60 * 24) > self.system.now as u64 {
                    text(format!("{:>8.8}", c.get(11..).unwrap_or_default()))
                } else {
                    text(format!("  {:>6.6}", c.get(4..).unwrap_or_default()))
                }
            }
            Pr::Tsig => self.signal(&p.sigpnd),
            Pr::Sig => self.signal(&p.signal),
            Pr::Sigmask => self.signal(&p.blocked),
            Pr::Sigignore => self.signal(&p.sigignore),
            Pr::Sigcatch => self.signal(&p.sigcatch),
            Pr::Egid => text(format!("{}", p.egid as i32)),
            Pr::Rgid => text(format!("{}", p.rgid as i32)),
            Pr::Sgid => text(format!("{}", p.sgid as i32)),
            Pr::Fgid => text(format!("{}", p.fgid as i32)),
            Pr::Euid => text(format!("{}", p.euid as i32)),
            Pr::Ruid => text(format!("{}", p.ruid as i32)),
            Pr::Suid => text(format!("{}", p.suid as i32)),
            Pr::Fuid => text(format!("{}", p.fuid as i32)),
            Pr::Ruser => self.name(&s.user(p.ruid), p.ruid),
            Pr::Euser => self.name(&s.user(p.euid), p.euid),
            Pr::Fuser => self.name(&s.user(p.fuid), p.fuid),
            Pr::Suser => self.name(&s.user(p.suid), p.suid),
            Pr::Egroup => self.name(&s.group(p.egid), p.egid),
            Pr::Rgroup => self.name(&s.group(p.rgid), p.rgid),
            Pr::Fgroup => self.name(&s.group(p.fgid), p.fgid),
            Pr::Sgroup => self.name(&s.group(p.sgid), p.sgid),
            Pr::Procs => text(format!("{}", p.tgid)),
            Pr::Tasks => text(format!("{}", p.tid)),
            Pr::Nlwp => text(format!("{}", p.nlwp)),
            Pr::Sess => text(format!("{}", p.session)),
            Pr::Supgid => {
                // escaped_copy: as it is, to the room there is.
                let n = p.supgid.len().min(mr as usize);
                let out = p.supgid.get(..n).unwrap_or_default().to_vec();
                (out, n as i32)
            }
            Pr::Supgrp => {
                let names = supgroups(p, s);
                let mut out = Vec::new();
                let mut rightward = i64::from(mr);
                escape(&mut out, &names, i64::from(OUTBUF_SIZE), &mut rightward);
                (out, (i64::from(mr) - rightward) as i32)
            }
            Pr::Tpgid => text(format!("{}", p.tpgid)),
            Pr::SgiP => {
                if p.state == b'R' {
                    text(format!("{}", p.processor as u32))
                } else {
                    text("*".into())
                }
            }
            Pr::Utilization | Pr::UtilizationC => {
                let tics = if pr == Pr::Utilization {
                    tics_all(p)
                } else {
                    tics_all_c(p)
                };
                let cu = utilization(s, tics, p.start_time);
                text(format!("{:.3}", if cu > 99.0 { 99.999 } else { cu }))
            }
            Pr::TUnlimited | Pr::TUnlimited2 => {
                let vals: &[&str] = if pr == Pr::TUnlimited {
                    &["[123456789-12345] <defunct>", "ps", "123456789-123456"]
                } else {
                    &[
                        "unlimited",
                        "[123456789-12345] <defunct>",
                        "ps",
                        "123456789-123456",
                    ]
                };
                let v = self.test_value(vals);
                let out: Vec<u8> = v.bytes().take(mr as usize).collect();
                let n = out.len() as i32;
                (out, n)
            }
            Pr::TRight => text(
                self.test_value(&["999-23:59:59", "99-23:59:59", "9-23:59:59", "59:59"])
                    .into(),
            ),
            Pr::TRight2 => text(
                self.test_value(&["999-23:59:59", "99-23:59:59", "9-23:59:59"])
                    .into(),
            ),
            Pr::TLeft => text(
                self.test_value(&["tty7", "pts/9999", "iseries/vtty42", "ttySMX0", "3270/tty4"])
                    .into(),
            ),
            Pr::TLeft2 => text(
                self.test_value(&["tty7", "pts/9999", "ttySMX0", "3270/tty4"])
                    .into(),
            ),
        }
    }

    /// The test columns' values, chosen by the header countdown.
    fn test_value<'v>(&self, vals: &[&'v str]) -> &'v str {
        let n = vals.len() as u32;
        vals.get(((self.lines_to_next_header as u32) % n) as usize)
            .copied()
            .unwrap_or_default()
    }

    /// help_pr_sig: 16 hex digits when there is room, else 9.
    fn signal(&self, sig: &[u8]) -> (Vec<u8>, i32) {
        let len = sig.len();
        let out = if self.wide_signals {
            if len > 8 {
                sig.to_vec()
            } else {
                [b"00000000", sig].concat()
            }
        } else if len - sig.iter().take_while(|&&c| c == b'0').count() > 8 {
            [b"<", sig.get(len - 8..).unwrap_or_default()].concat()
        } else if len < 8 {
            [b"00000000".get(len..).unwrap_or_default(), sig].concat()
        } else {
            sig.get(len - 8..).unwrap_or_default().to_vec()
        };
        let n = out.len() as i32;
        (out, n)
    }

    /// do_pr_name: the name, cut to the column with a `+` when it does not fit; the
    /// number with `n`, or when there is no room at all.
    fn name(&self, name: &[u8], id: u32) -> (Vec<u8>, i32) {
        let mr = self.max_rightward as usize;
        if !self.o.user_is_number {
            let mut out = Vec::new();
            let mut cells = i64::from(OUTBUF_SIZE);
            escape(&mut out, name, i64::from(OUTBUF_SIZE), &mut cells);
            if out.len() <= mr {
                let n = out.len() as i32;
                return (out, n);
            }
            if mr >= 1 && out.get(mr - 1).is_some_and(|&c| c < 127) {
                out.truncate(mr - 1);
                out.push(b'+');
                return (out, mr as i32);
            }
        }
        let t = id.to_string();
        let n = t.len() as i32;
        (t.into_bytes(), n)
    }

    /// TIME_ELAPSED: seconds since the process started.
    fn time_elapsed(&self, p: &Proc) -> f64 {
        let t = self.system.boot_tics.wrapping_sub(p.start_time) as f64;
        if t > 0.0 { t / self.system.hz as f64 } else { 0.0 }
    }

    /// TIME_ALL: CPU seconds.
    fn time_all(&self, p: &Proc) -> f64 {
        (p.utime as f64 + p.stime as f64) / self.system.hz as f64
    }

    /// When the process started, in seconds since the epoch.
    fn start(&self, p: &Proc) -> i64 {
        (u64::from(self.system.btime).wrapping_add(p.start_time / self.system.hz)) as i64
    }
}

fn tics_all(p: &Proc) -> u64 {
    p.utime.wrapping_add(p.stime)
}

fn tics_all_c(p: &Proc) -> u64 {
    tics_all(p).wrapping_add(p.cutime).wrapping_add(p.cstime)
}

/// supgrps_from_supgids: the supplementary groups' names.
fn supgroups(p: &Proc, s: &System) -> Vec<u8> {
    if p.supgid.first() == Some(&b'-') {
        return b"-".to_vec();
    }
    let mut out: Vec<u8> = Vec::new();
    for gid in p.supgid.split(|&b| b == b',').filter(|g| !g.is_empty()) {
        let (v, end) = super::dump::strtol(gid, 10);
        if end == 0 {
            break;
        }
        if !out.is_empty() {
            out.push(b',');
        }
        out.extend(s.group(super::dump::as_long(v) as u32));
    }
    if out.is_empty() { b"-".to_vec() } else { out }
}

/// output.c's escape_str in the C locale: control characters as `.`, bytes past ASCII
/// as `?`, at most `maxcells` of them and `bufsize - 1` bytes, up to a NUL; the cells
/// used come off `maxcells`.
fn escape(dst: &mut Vec<u8>, src: &[u8], bufsize: i64, maxcells: &mut i64) {
    if bufsize <= 0 || bufsize >= i64::from(i32::MAX) || *maxcells >= i64::from(i32::MAX) || *maxcells <= 0 {
        return;
    }
    let bufsize = bufsize.min(*maxcells + 1);
    let mut n: i64 = 0;
    for &c in src.iter().take_while(|&&c| c != 0) {
        if n >= *maxcells || n + 1 >= bufsize {
            break;
        }
        dst.push(match c {
            0x20..=0x7e => c,
            0x80..=0xff => b'?',
            _ => b'.',
        });
        n += 1;
    }
    *maxcells -= n;
}

/// proc_was_listed.
fn listed(o: &Opts, p: &Proc) -> bool {
    o.selection.iter().any(|sel| {
        let v = match sel.kind {
            SelKind::Ruid => p.ruid,
            SelKind::Euid => p.euid,
            SelKind::Rgid => p.rgid,
            SelKind::Egid => p.egid,
            SelKind::Pgrp => p.pgrp as u32,
            SelKind::Pid | SelKind::PidQuick => p.tgid as u32,
            SelKind::Ppid => p.ppid as u32,
            SelKind::Tty => p.tty as u32,
            SelKind::Sess => p.session as u32,
            SelKind::Comm => {
                return sel.cmds.iter().any(|c| {
                    let cut = |s: &[u8], n: usize| s.iter().take(n).copied().collect::<Vec<u8>>();
                    // A 15-character name is the kernel's cut of a longer one.
                    (p.cmd.len() == 15 && c.len() >= 15 && cut(&p.cmd, 15) == cut(c, 15))
                        || cut(&p.cmd, 63) == cut(c, 63)
                });
            }
        };
        sel.nums.contains(&v)
    })
}

/// A sort item's value (library/pids.c), with its sort function's kind.
enum Val {
    /// s_int and s_ch: compared by difference.
    Num(i32),
    Unsigned(u64),
    Real(f64),
    /// strcoll, in the C locale.
    Str(Vec<u8>),
    /// strverscmp.
    Vers(Vec<u8>),
    None,
}

/// pids.c's sort functions, in the order asked.
fn compare(sort: &SortNode, a: &Proc, b: &Proc, s: &System) -> Ordering {
    let ord = match (value(sort.sr, a, s), value(sort.sr, b, s)) {
        (Val::Num(x), Val::Num(y)) => x.wrapping_sub(y).cmp(&0),
        (Val::Unsigned(x), Val::Unsigned(y)) => x.cmp(&y),
        (Val::Real(x), Val::Real(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Val::Str(x), Val::Str(y)) => x.cmp(&y),
        (Val::Vers(x), Val::Vers(y)) => strverscmp(&x, &y),
        _ => Ordering::Equal,
    };
    if sort.order < 0 { ord.reverse() } else { ord }
}

fn value(sr: Sr, p: &Proc, s: &System) -> Val {
    let elapsed_tics = s.boot_tics.wrapping_sub(p.start_time) as f64;
    let ns = || Val::Unsigned(0);
    let dash = || Val::Str(b"-".to_vec());
    match sr {
        Sr::AddrCodeEnd => Val::Unsigned(p.end_code),
        Sr::AddrCodeStart => Val::Unsigned(p.start_code),
        Sr::AddrCurrEip => Val::Unsigned(p.kstk_eip),
        Sr::AddrCurrEsp => Val::Unsigned(p.kstk_esp),
        Sr::AddrStackStart => Val::Unsigned(p.start_stack),
        Sr::Cgname | Sr::Cgroup | Sr::Exe | Sr::Lxcname => dash(),
        Sr::SdMach | Sr::SdOuid | Sr::SdSeat | Sr::SdSess | Sr::SdSlice | Sr::SdUnit | Sr::SdUunit => dash(),
        Sr::Cmd => Val::Str(p.cmd.clone()),
        Sr::Cmdline => Val::Str(p.cmdline.clone()),
        Sr::Flags => Val::Unsigned(p.flags),
        Sr::FltMaj => Val::Unsigned(p.maj_flt),
        Sr::FltMin => Val::Unsigned(p.min_flt),
        Sr::IdEgid => Val::Unsigned(p.egid.into()),
        Sr::IdEgroup => Val::Str(s.group(p.egid)),
        Sr::IdEuid => Val::Unsigned(p.euid.into()),
        Sr::IdEuser => Val::Str(s.user(p.euid)),
        Sr::IdFgid => Val::Unsigned(p.fgid.into()),
        Sr::IdFgroup => Val::Str(s.group(p.fgid)),
        Sr::IdFuid => Val::Unsigned(p.fuid.into()),
        Sr::IdFuser => Val::Str(s.user(p.fuid)),
        Sr::IdLogin => Val::Num(-1),
        Sr::IdPgrp => Val::Num(p.pgrp),
        Sr::IdPid => Val::Num(p.tid),
        Sr::IdPpid => Val::Num(p.ppid),
        Sr::IdRgid => Val::Unsigned(p.rgid.into()),
        Sr::IdRgroup => Val::Str(s.group(p.rgid)),
        Sr::IdRuid => Val::Unsigned(p.ruid.into()),
        Sr::IdRuser => Val::Str(s.user(p.ruid)),
        Sr::IdSession => Val::Num(p.session),
        Sr::IdSgid => Val::Unsigned(p.sgid.into()),
        Sr::IdSgroup => Val::Str(s.group(p.sgid)),
        Sr::IdSuid => Val::Unsigned(p.suid.into()),
        Sr::IdSuser => Val::Str(s.user(p.suid)),
        Sr::IdTgid => Val::Num(p.tgid),
        Sr::IdTpgid => Val::Num(p.tpgid),
        Sr::Nice => Val::Num(p.nice),
        Sr::Nlwp => Val::Num(p.nlwp),
        Sr::NsCgroup
        | Sr::NsIpc
        | Sr::NsMnt
        | Sr::NsNet
        | Sr::NsPid
        | Sr::NsTime
        | Sr::NsUser
        | Sr::NsUts => ns(),
        Sr::Priority => Val::Num(p.priority),
        Sr::PriorityRt => Val::Num(p.rtprio),
        Sr::Processor => Val::Num(p.processor),
        Sr::RssRlim => Val::Unsigned(p.rss_rlim),
        Sr::SchedClass => Val::Num(p.sched),
        Sr::Sigblocked => Val::Str(p.blocked.clone()),
        Sr::Sigcatch => Val::Str(p.sigcatch.clone()),
        Sr::Sigignore => Val::Str(p.sigignore.clone()),
        Sr::Signals => Val::Str(p.signal.clone()),
        Sr::Sigpending => Val::Str(p.sigpnd.clone()),
        Sr::State => Val::Num(i32::from(p.state as i8)),
        Sr::Supgids => Val::Str(p.supgid.clone()),
        Sr::Supgroups => Val::Str(supgroups(p, s)),
        Sr::TicsAll => Val::Unsigned(tics_all(p)),
        Sr::TicsBegan => Val::Unsigned(p.start_time),
        Sr::TicsUser => Val::Unsigned(p.utime),
        Sr::TicsUserC => Val::Unsigned(p.utime.wrapping_add(p.cutime)),
        Sr::TimeAll => Val::Real((p.utime as f64 + p.stime as f64) / s.hz as f64),
        Sr::TimeElapsed => Val::Real(if elapsed_tics > 0.0 {
            elapsed_tics / s.hz as f64
        } else {
            0.0
        }),
        Sr::TtyName => Val::Vers(tty_name(p.tty, false)),
        Sr::Utilization => Val::Real(utilization(s, tics_all(p), p.start_time)),
        Sr::UtilizationC => Val::Real(utilization(s, tics_all_c(p), p.start_time)),
        Sr::VmData => Val::Unsigned(p.vm_data),
        Sr::VmExe => Val::Unsigned(p.vm_exe),
        Sr::VmLib => Val::Unsigned(p.vm_lib),
        Sr::VmRss => Val::Unsigned(p.vm_rss),
        Sr::VmRssLocked => Val::Unsigned(p.vm_lock),
        Sr::VmSize => Val::Unsigned(p.vm_size),
        Sr::VmStack => Val::Unsigned(p.vm_stack),
        Sr::VsizeBytes => Val::Unsigned(p.vsize),
        Sr::WchanName => Val::Str(p.wchan.clone()),
        // What the guest does not send: refused before sorting (missing_sort).
        Sr::AutogrpId
        | Sr::AutogrpNice
        | Sr::IoReadBytes
        | Sr::IoReadChars
        | Sr::IoReadOps
        | Sr::IoWriteBytes
        | Sr::IoWriteCbytes
        | Sr::IoWriteChars
        | Sr::IoWriteOps
        | Sr::MemResPgs
        | Sr::MemShrPgs
        | Sr::OomAdj
        | Sr::OomScore
        | Sr::ProcessorNode
        | Sr::SmapPrvTotal
        | Sr::SmapPss
        | Sr::Noop => Val::None,
    }
}

/// UTILIZATION: `((utime + stime) * 100.0f) / t`, the product in single precision as
/// pids.c computes it, over the ticks since the process started.
fn utilization(s: &System, tics: u64, start_time: u64) -> f64 {
    let t = s.boot_tics.wrapping_sub(start_time) as f64;
    if t > 0.0 {
        f64::from(tics as f32 * 100.0_f32) / t
    } else {
        0.0
    }
}

/// glibc's strverscmp: digit runs compared as numbers, those with leading zeros as
/// fractions.
fn strverscmp(a: &[u8], b: &[u8]) -> Ordering {
    // glibc's state machine (string/strverscmp.c).
    const S_N: usize = 0x0;
    const S_I: usize = 0x3;
    const S_F: usize = 0x6;
    const S_Z: usize = 0x9;
    const CMP: i8 = 2;
    const LEN: i8 = 3;
    const NEXT_STATE: [usize; 12] = [S_N, S_I, S_Z, S_N, S_I, S_I, S_N, S_F, S_F, S_N, S_F, S_Z];
    #[rustfmt::skip]
    const RESULT_TYPE: [i8; 36] = [
        CMP, CMP, CMP, CMP, LEN, CMP, CMP, CMP, CMP,
        CMP, -1, -1, 1, LEN, LEN, 1, LEN, LEN,
        CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP,
        CMP, 1, 1, -1, CMP, CMP, -1, CMP, CMP,
    ];
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let class = |c: u8| usize::from(c == b'0') + usize::from(c.is_ascii_digit());
    if a == b {
        return Ordering::Equal;
    }
    let (mut i, mut c1, mut c2) = (0, at(a, 0), at(b, 0));
    let mut state = S_N + class(c1);
    let mut diff = i32::from(c1) - i32::from(c2);
    while diff == 0 {
        if c1 == 0 {
            return Ordering::Equal;
        }
        state = NEXT_STATE.get(state).copied().unwrap_or(S_N);
        i += 1;
        c1 = at(a, i);
        c2 = at(b, i);
        state += class(c1);
        diff = i32::from(c1) - i32::from(c2);
    }
    let kind = RESULT_TYPE.get(state * 3 + class(c2)).copied().unwrap_or(CMP);
    match kind {
        CMP => diff.cmp(&0),
        LEN => {
            let mut j = i + 1;
            while at(a, j).is_ascii_digit() {
                if !at(b, j).is_ascii_digit() {
                    return Ordering::Greater;
                }
                j += 1;
            }
            if at(b, j).is_ascii_digit() {
                Ordering::Less
            } else {
                diff.cmp(&0)
            }
        }
        k => k.cmp(&0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_glibc_compares_them() {
        assert_eq!(strverscmp(b"pts/2", b"pts/10"), Ordering::Less);
        assert_eq!(strverscmp(b"?", b"pts/0"), Ordering::Less);
        assert_eq!(strverscmp(b"a", b"a"), Ordering::Equal);
        assert_eq!(strverscmp(b"000", b"00"), Ordering::Less);
        assert_eq!(strverscmp(b"alpha", b"beta"), Ordering::Less);
        assert_eq!(strverscmp(b"item#99", b"item#100"), Ordering::Less);
        assert_eq!(strverscmp(b"0.9", b"0.10"), Ordering::Less);
    }

    #[test]
    fn escapes_as_the_c_locale_does() {
        let mut out = Vec::new();
        let mut cells = 4;
        escape(&mut out, b"a\tb\xc3\xa9z", 100, &mut cells);
        assert_eq!((out.as_slice(), cells), (&b"a.b?"[..], 0));
    }
}
