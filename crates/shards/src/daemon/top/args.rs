//! ps's command line as procps-ng 4.0.2 reads it: src/ps/parser.c (SysV, BSD and GNU
//! options, a second BSD-style pass when the first fails), src/ps/sortformat.c (format
//! and sort lists, the default formats) and src/ps/select.c's select_bits_setup.

use super::dump::{Proc, System, as_long, as_ulong, strtol};
use super::specs::{self, AIX, PIDMAX, Pr, SHORTSORT, SPECS, Sr};

/// common.h's OUTBUF_SIZE, also ps's width when its output is not a terminal.
pub(super) const OUTBUF_SIZE: i32 = 2 * 64 * 1024;
/// The digits of the largest PID (procps_pid_length, from /proc/sys/kernel/pid_max,
/// which the dump does not carry): the kernel's default pid_max, 32768, for up to 32
/// CPUs, or 1024 a CPU up to 97 CPUs.
const PID_LENGTH: i32 = 5;

// Selection (common.h SS_*), format flags (FF_*), modifiers (FM_*), thread flags
// (TF_*), personality (PER_*) and headers (HEAD_*).
pub(super) const SS_B_X: u32 = 0x01;
const SS_B_G: u32 = 0x02;
const SS_U_D: u32 = 0x04;
const SS_U_A: u32 = 0x08;
const SS_B_A: u32 = 0x10;
const FF_UF: u32 = 0x0001;
const FF_UL: u32 = 0x0004;
const FF_BJ: u32 = 0x0008;
const FF_BL: u32 = 0x0010;
const FF_BS: u32 = 0x0020;
const FF_BU: u32 = 0x0040;
const FF_BV: u32 = 0x0080;
const FF_LX: u32 = 0x0100;
const FF_LM: u32 = 0x0200;
const FF_FC: u32 = 0x0400;
const FM_C: u32 = 0x0001;
const FM_J: u32 = 0x0002;
const FM_Y: u32 = 0x0004;
const FM_P: u32 = 0x0010;
const FM_M: u32 = 0x0020;
const FM_F: u32 = 0x0080;
const TF_B_H: u32 = 0x0001;
const TF_B_M: u32 = 0x0002;
const TF_U_M: u32 = 0x0004;
const TF_U_T: u32 = 0x0008;
const TF_U_L: u32 = 0x0010;
pub(super) const TF_SHOW_PROC: u32 = 0x0100;
const TF_SHOW_TASK: u32 = 0x0200;
const TF_SHOW_BOTH: u32 = 0x0400;
const TF_LOOSE_TASKS: u32 = 0x0800;
const TF_MUST_USE: u32 = 0x4000;
const PER_OLD_M: u32 = 0x0040;
pub(super) const HEAD_SINGLE: u8 = 0;
pub(super) const HEAD_NONE: u8 = 1;
pub(super) const HEAD_MULTI: u8 = 2;

/// A column (format_node): its header, print function (none for AIX's literal text),
/// sort item, width and flags.
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub name: Vec<u8>,
    pub pr: Option<Pr>,
    pub width: i32,
    pub flags: u32,
}

/// A sort key (sort_node): the item, the column's print function (whose items the stack
/// fetches) and the order, +1, -1, or 0 for none.
#[derive(Debug, Clone, Copy)]
pub(super) struct SortNode {
    pub sr: Sr,
    pub pr: Pr,
    pub order: i32,
}

/// What a selection list matches (common.h SEL_*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelKind {
    Ruid,
    Euid,
    Rgid,
    Egid,
    Pgrp,
    Pid,
    PidQuick,
    Tty,
    Sess,
    Comm,
    Ppid,
}

/// A selection list (selection_node): numbers compared as unsigned ints, or command
/// names.
#[derive(Debug, Clone)]
pub(super) struct Sel {
    pub kind: SelKind,
    pub nums: Vec<u32>,
    pub cmds: Vec<Vec<u8>>,
}

/// ps's options once read (global.c's globals).
#[derive(Debug, Clone, Default)]
pub(super) struct Opts {
    pub all_processes: bool,
    pub bsd_c_option: bool,
    pub bsd_e_option: bool,
    /// 0, or `u` (-H), `b` (f), `g` (--forest).
    pub forest_type: u8,
    format_flags: u32,
    format_modifiers: u32,
    pub header_type: u8,
    pub include_dead_children: bool,
    pub negate_selection: bool,
    pub running_only: bool,
    personality: u32,
    prefer_bsd_defaults: bool,
    pub screen_cols: i32,
    pub screen_rows: i32,
    /// The selection lists, the latest last (selection_list's head).
    pub selection: Vec<Sel>,
    pub simple_select: u32,
    pub thread_flags: u32,
    /// The option that asked for threads, for shards' message.
    pub thread_option: Option<String>,
    pub unix_f_option: bool,
    pub user_is_number: bool,
    pub wchan_is_number: bool,
    w_count: u32,
    pub format_list: Vec<Node>,
    pub sort_list: Vec<SortNode>,
    pub select_bits: u32,
}

/// What reading the command line comes to.
#[derive(Debug)]
pub(super) enum Parsed {
    /// Options to list processes with.
    Run(Box<Opts>),
    /// Text ps prints to stdout before exiting successfully (-V, L, --help, --info).
    Print(Vec<u8>),
    /// ps exits failing, with this first line on stderr.
    Fail(String),
}

/// How a parsing step stops: an error, which the second pass may get past, or an
/// exit.
enum Stop {
    Error(String),
    Exit(Parsed),
}

type Step = Result<(), Stop>;

fn err<T>(msg: &str) -> Result<T, Stop> {
    Err(Stop::Error(msg.to_string()))
}

/// sortformat.c's deferred -o, -O, o, O, --format, --sort and k lists (sf_node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SfCode {
    UO,
    Uo,
    BO,
    Bo,
    GSort,
    GFormat,
}

#[derive(Debug)]
struct SfNode {
    sf: Vec<u8>,
    code: SfCode,
    /// In order.
    f_cooked: Vec<Node>,
    /// In order.
    s_cooked: Vec<SortNode>,
}

/// The arguments' kinds (parser.c arg_type).
#[derive(Debug, PartialEq, Eq)]
enum ArgType {
    Gnu,
    End,
    Pgrp,
    Sysv,
    Pid,
    Bsd,
    Fail,
    Sess,
}

struct Parser<'a> {
    argv: &'a [Vec<u8>],
    thisarg: usize,
    force_bsd: bool,
    o: Opts,
    sf_list: Vec<SfNode>,
    have_gnu_sort: bool,
    already_parsed_sort: bool,
    already_parsed_format: bool,
    /// format_parse's static errbuf: kept for the life of the ps process.
    errbuf: Option<String>,
    system: &'a System,
    procs: &'a [Proc],
}

/// arg_parse: `argv` (argv[0] the program) read as ps reads it.
pub(super) fn parse(argv: &[Vec<u8>], system: &System, procs: &[Proc]) -> Parsed {
    let mut p = Parser {
        argv,
        thisarg: 0,
        force_bsd: false,
        o: Opts::default(),
        sf_list: Vec::new(),
        have_gnu_sort: false,
        already_parsed_sort: false,
        already_parsed_format: false,
        errbuf: None,
        system,
        procs,
    };
    p.reset_global();
    let first = match p.pass() {
        Ok(()) => return p.done(),
        Err(Stop::Exit(parsed)) => return parsed,
        Err(Stop::Error(e)) => e,
    };
    // try_bsd: everything again, as BSD where it was SysV.
    p.reset_global();
    p.o.w_count = 0;
    p.reset_sortformat();
    p.thisarg = 0;
    p.force_bsd = true;
    p.o.prefer_bsd_defaults = true;
    p.o.personality |= PER_OLD_M;
    match p.pass() {
        Ok(()) => p.done(),
        Err(Stop::Exit(parsed)) => parsed,
        Err(Stop::Error(_)) => Parsed::Fail(format!("error: {first}")),
    }
}

impl Parser<'_> {
    fn pass(&mut self) -> Step {
        self.parse_all_options()?;
        self.thread_option_check()?;
        self.process_sf_options()?;
        self.select_bits_setup()
    }

    /// choose_dimensions, then the options.
    fn done(mut self) -> Parsed {
        if self.o.w_count > 0 && self.o.screen_cols < 132 {
            self.o.screen_cols = 132;
        }
        if self.o.w_count > 1 {
            self.o.screen_cols = OUTBUF_SIZE;
        }
        Parsed::Run(Box::new(self.o))
    }

    /// reset_global: set_screen_size (not a terminal: OUTBUF_SIZE columns, the 24 rows
    /// of its fallback) and set_personality ("unknown": none).
    fn reset_global(&mut self) {
        self.o = Opts {
            screen_cols: OUTBUF_SIZE,
            screen_rows: 24,
            header_type: HEAD_SINGLE,
            w_count: self.o.w_count,
            ..Opts::default()
        };
    }

    fn reset_sortformat(&mut self) {
        self.sf_list.clear();
        self.have_gnu_sort = false;
        self.already_parsed_sort = false;
        self.already_parsed_format = false;
    }

    fn arg(&self, i: usize) -> &[u8] {
        self.argv.get(i).map_or(&[][..], Vec::as_slice)
    }

    fn parse_all_options(&mut self) -> Step {
        while self.thisarg + 1 < self.argv.len() {
            self.thisarg += 1;
            match arg_type(self.arg(self.thisarg)) {
                ArgType::Gnu => self.parse_gnu_option()?,
                ArgType::Sysv if !self.force_bsd => self.parse_sysv_option()?,
                ArgType::Sysv => {
                    self.o.prefer_bsd_defaults = true;
                    self.parse_bsd_option()?;
                }
                ArgType::Bsd => {
                    // No personality forces BSD, so the second pass stops here.
                    if self.force_bsd {
                        return err("way bad");
                    }
                    self.o.prefer_bsd_defaults = true;
                    self.parse_bsd_option()?;
                }
                ArgType::Pgrp | ArgType::Sess | ArgType::Pid => {
                    self.o.prefer_bsd_defaults = true;
                    self.parse_trailing_pids()?;
                }
                ArgType::End | ArgType::Fail => return err("garbage option"),
            }
        }
        Ok(())
    }

    /// get_opt_arg: the rest of this argument after the option at `at`, or the next
    /// argument when it is not empty.
    fn get_opt_arg(&mut self, at: usize) -> Option<Vec<u8>> {
        let this = self.arg(self.thisarg);
        if let Some(rest) = this.get(at + 1..)
            && !rest.is_empty()
        {
            return Some(rest.to_vec());
        }
        if self.thisarg + 2 > self.argv.len() {
            return None;
        }
        let next = self.arg(self.thisarg + 1).to_vec();
        if next.is_empty() {
            return None;
        }
        self.thisarg += 1;
        Some(next)
    }

    /// `exclusive(x)`: the option must be the only argument.
    fn exclusive(&self, option: &str) -> Step {
        if self.argv.len() != 2 || self.arg(1) != option.as_bytes() {
            return err(&format!("the option is exclusive: {option}"));
        }
        Ok(())
    }

    fn thread(&mut self, flag: u32, option: &str) {
        self.o.thread_flags |= flag;
        self.o.thread_option.get_or_insert_with(|| option.to_string());
    }

    fn add_list(&mut self, arg: &[u8], kind: SelKind, parse: ListKind) -> Step {
        let sel = self
            .parse_list(arg, kind, parse)
            .map_err(|e| Stop::Error(e.to_string()))?;
        self.o.selection.push(sel);
        Ok(())
    }

    fn parse_sysv_option(&mut self) -> Step {
        let mut at = 0;
        loop {
            at += 1;
            let Some(&c) = self.arg(self.thisarg).get(at) else {
                return Ok(());
            };
            match c {
                b'A' | b'e' => self.o.all_processes = true,
                b'C' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of command names must follow -C".into()))?;
                    return self.add_list(&arg, SelKind::Comm, ListKind::Cmd);
                }
                b'F' => {
                    self.o.format_modifiers |= FM_F;
                    self.o.format_flags |= FF_UF;
                    self.o.unix_f_option = true;
                }
                b'G' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of real groups must follow -G".into()))?;
                    return self.add_list(&arg, SelKind::Rgid, ListKind::Gid);
                }
                b'H' => self.o.forest_type = b'u',
                b'L' => self.thread(TF_U_L, "-L"),
                b'M' | b'Z' => self.o.format_modifiers |= FM_M,
                b'N' => self.o.negate_selection = true,
                b'O' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("format or sort specification must follow -O".into()))?;
                    return self.defer_sf_option(arg, SfCode::UO);
                }
                b'P' => self.o.format_modifiers |= FM_P,
                b'T' => self.thread(TF_U_T, "-T"),
                b'U' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of real users must follow -U".into()))?;
                    return self.add_list(&arg, SelKind::Ruid, ListKind::Uid);
                }
                b'V' => {
                    self.exclusive("-V")?;
                    return Err(Stop::Exit(Parsed::Print(VERSION.into())));
                }
                b'a' => self.o.simple_select |= SS_U_A,
                b'c' => self.o.format_modifiers |= FM_C,
                b'd' => self.o.simple_select |= SS_U_D,
                b'f' => {
                    self.o.format_flags |= FF_UF;
                    self.o.unix_f_option = true;
                }
                b'g' => {
                    let arg = self.get_opt_arg(at).ok_or_else(|| {
                        Stop::Error("list of session leaders OR effective group names must follow -g".into())
                    })?;
                    if self.add_list(&arg, SelKind::Sess, ListKind::Pid).is_ok()
                        || self.add_list(&arg, SelKind::Egid, ListKind::Gid).is_ok()
                    {
                        return Ok(());
                    }
                    return err("list of session leaders OR effective group IDs was invalid");
                }
                // sysv_j_format is NULL without a personality.
                b'j' => self.o.format_modifiers |= FM_J,
                b'l' => self.o.format_flags |= FF_UL,
                b'm' => self.thread(TF_U_M, "-m"),
                b'o' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("format specification must follow -o".into()))?;
                    return self.defer_sf_option(arg, SfCode::Uo);
                }
                b'p' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of process IDs must follow -p".into()))?;
                    return self.add_list(&arg, SelKind::Pid, ListKind::Pid);
                }
                b'q' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("List of process IDs must follow -q.".into()))?;
                    return self.add_list(&arg, SelKind::PidQuick, ListKind::Pid);
                }
                b's' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of session IDs must follow -s".into()))?;
                    return self.add_list(&arg, SelKind::Sess, ListKind::Pid);
                }
                b't' => {
                    let arg = self.get_opt_arg(at).ok_or_else(|| {
                        Stop::Error("list of terminals (pty, tty...) must follow -t".into())
                    })?;
                    return self.add_list(&arg, SelKind::Tty, ListKind::Tty);
                }
                b'u' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of users must follow -u".into()))?;
                    return self.add_list(&arg, SelKind::Euid, ListKind::Uid);
                }
                b'w' => self.o.w_count += 1,
                // Behind a personality ps does not have by default.
                b'x' => return err("must set personality to get -x option"),
                b'y' => self.o.format_modifiers |= FM_Y,
                b'-' => return err("embedded '-' among SysV options makes no sense"),
                _ => return err("unsupported SysV option"),
            }
        }
    }

    fn parse_bsd_option(&mut self) -> Step {
        let this = self.arg(self.thisarg);
        // `at` is the index before the first option letter.
        let mut at: usize = if this.first() == Some(&b'-') {
            if !self.force_bsd {
                return err("cannot happen - problem #1");
            }
            0
        } else {
            if self.force_bsd {
                return err("second chance parse failed, not BSD or SysV");
            }
            usize::MAX
        };
        loop {
            at = at.wrapping_add(1);
            let Some(&c) = self.arg(self.thisarg).get(at) else {
                return Ok(());
            };
            match c {
                b'0'..=b'9' => {
                    let arg = self.arg(self.thisarg).get(at..).unwrap_or_default().to_vec();
                    return self.add_list(&arg, SelKind::Pid, ListKind::Pid);
                }
                b'H' => self.thread(TF_B_H, "H"),
                b'L' => {
                    self.exclusive("L")?;
                    return Err(Stop::Exit(Parsed::Print(format_specifiers())));
                }
                b'M' => self.thread(TF_B_M, "M"),
                b'O' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("format or sort specification must follow O".into()))?;
                    return self.defer_sf_option(arg, SfCode::BO);
                }
                b'S' => self.o.include_dead_children = true,
                b'T' => self.o.selection.push(Sel {
                    kind: SelKind::Tty,
                    nums: vec![CACHED_TTY],
                    cmds: Vec::new(),
                }),
                b'U' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of users must follow U".into()))?;
                    return self.add_list(&arg, SelKind::Euid, ListKind::Uid);
                }
                b'V' => {
                    self.exclusive("V")?;
                    return Err(Stop::Exit(Parsed::Print(VERSION.into())));
                }
                b'W' => return err("obsolete W option not supported (you have a /dev/drum?)"),
                b'X' => self.o.format_flags |= FF_LX,
                b'Z' => self.o.format_modifiers |= FM_M,
                b'a' => self.o.simple_select |= SS_B_A,
                b'c' => self.o.bsd_c_option = true,
                b'e' => self.o.bsd_e_option = true,
                b'f' => self.o.forest_type = b'b',
                b'g' => self.o.simple_select |= SS_B_G,
                b'h' => {
                    if self.o.header_type != HEAD_SINGLE {
                        return err("only one heading option may be specified");
                    }
                    self.o.header_type = HEAD_NONE;
                }
                b'j' => self.o.format_flags |= FF_BJ,
                b'k' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("long sort specification must follow 'k'".into()))?;
                    return self.defer_sf_option(arg, SfCode::GSort);
                }
                b'l' => self.o.format_flags |= FF_BL,
                b'm' => {
                    if self.o.personality & PER_OLD_M != 0 {
                        self.o.format_flags |= FF_LM;
                    } else {
                        self.thread(TF_B_M, "m");
                    }
                }
                b'n' => {
                    self.o.wchan_is_number = true;
                    self.o.user_is_number = true;
                }
                b'o' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("format specification must follow o".into()))?;
                    return self.defer_sf_option(arg, SfCode::Bo);
                }
                b'p' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("list of process IDs must follow p".into()))?;
                    return self.add_list(&arg, SelKind::Pid, ListKind::Pid);
                }
                b'q' => {
                    let arg = self
                        .get_opt_arg(at)
                        .ok_or_else(|| Stop::Error("List of process IDs must follow q.".into()))?;
                    return self.add_list(&arg, SelKind::PidQuick, ListKind::Pid);
                }
                b'r' => self.o.running_only = true,
                b's' => self.o.format_flags |= FF_BS,
                b't' => {
                    let Some(arg) = self.get_opt_arg(at) else {
                        // Obsolete BSD: this terminal.
                        self.o.selection.push(Sel {
                            kind: SelKind::Tty,
                            nums: vec![CACHED_TTY],
                            cmds: Vec::new(),
                        });
                        return Ok(());
                    };
                    return self.add_list(&arg, SelKind::Tty, ListKind::Tty);
                }
                b'u' => self.o.format_flags |= FF_BU,
                b'v' => self.o.format_flags |= FF_BV,
                b'w' => self.o.w_count += 1,
                b'x' => self.o.simple_select |= SS_B_X,
                b'-' => return err("embedded '-' among BSD options makes no sense"),
                _ => return err("unsupported option (BSD syntax)"),
            }
        }
    }

    /// grab_gnu_arg: after `=` or `:`, or the next argument when it is not empty.
    fn grab_gnu_arg(&mut self, rest: &[u8]) -> Option<Vec<u8>> {
        match rest.first() {
            Some(b'=' | b':') => {
                let arg = rest.get(1..).unwrap_or_default();
                (!arg.is_empty()).then(|| arg.to_vec())
            }
            Some(_) => None,
            None => {
                if self.thisarg + 2 > self.argv.len() {
                    return None;
                }
                let next = self.arg(self.thisarg + 1).to_vec();
                if next.is_empty() {
                    return None;
                }
                self.thisarg += 1;
                Some(next)
            }
        }
    }

    fn parse_gnu_option(&mut self) -> Step {
        let s = self.arg(self.thisarg).get(2..).unwrap_or_default().to_vec();
        let sl = s.iter().position(|&b| b == b':' || b == b'=').unwrap_or(s.len());
        if sl > 15 {
            return err("unknown gnu long option");
        }
        let name = s.get(..sl).unwrap_or_default();
        let rest = s.get(sl..).unwrap_or_default();
        let no_arg = |what: &str| -> Step {
            if rest.is_empty() {
                Ok(())
            } else {
                err(&format!("option --{what} does not take an argument"))
            }
        };
        let list = |p: &mut Self, msg: &str, kind: SelKind, parse: ListKind| -> Step {
            let arg = p.grab_gnu_arg(rest).ok_or_else(|| Stop::Error(msg.into()))?;
            p.add_list(&arg, kind, parse)
        };
        let number = |arg: Option<Vec<u8>>| -> Option<i32> {
            let arg = arg.filter(|a| !a.is_empty())?;
            let (v, end) = strtol(&arg, 0);
            let v = as_long(v);
            (end == arg.len() && v > 0 && v < 2_000_000_000).then_some(v as i32)
        };
        match name {
            b"Group" => list(
                self,
                "list of real groups must follow --Group",
                SelKind::Rgid,
                ListKind::Gid,
            ),
            b"User" => list(
                self,
                "list of real users must follow --User",
                SelKind::Ruid,
                ListKind::Uid,
            ),
            b"cols" | b"width" | b"columns" => {
                let arg = self.grab_gnu_arg(rest);
                self.o.screen_cols = number(arg).ok_or_else(|| {
                    Stop::Error("number of columns must follow --cols, --width, or --columns".into())
                })?;
                Ok(())
            }
            b"cumulative" => {
                no_arg("cumulative")?;
                self.o.include_dead_children = true;
                Ok(())
            }
            b"deselect" => {
                no_arg("deselect")?;
                self.o.negate_selection = true;
                Ok(())
            }
            b"no-header" | b"no-headers" | b"no-heading" | b"no-headings" | b"noheader" | b"noheaders"
            | b"noheading" | b"noheadings" => {
                no_arg("no-heading")?;
                if self.o.header_type != HEAD_SINGLE {
                    return err("only one heading option may be specified");
                }
                self.o.header_type = HEAD_NONE;
                Ok(())
            }
            b"header" | b"headers" | b"heading" | b"headings" => {
                no_arg("heading")?;
                if self.o.header_type != HEAD_SINGLE {
                    return err("only one heading option may be specified");
                }
                self.o.header_type = HEAD_MULTI;
                Ok(())
            }
            b"forest" => {
                no_arg("forest")?;
                self.o.forest_type = b'g';
                Ok(())
            }
            b"format" => {
                let arg = self
                    .grab_gnu_arg(rest)
                    .ok_or_else(|| Stop::Error("format specification must follow --format".into()))?;
                self.defer_sf_option(arg, SfCode::GFormat)
            }
            b"group" => list(
                self,
                "list of effective groups must follow --group",
                SelKind::Egid,
                ListKind::Gid,
            ),
            b"help" => {
                let arg = self.grab_gnu_arg(rest);
                Err(Stop::Exit(Parsed::Print(help(arg.as_deref()))))
            }
            b"info" => {
                self.exclusive("--info")?;
                // Its text goes to stderr.
                Err(Stop::Exit(Parsed::Print(Vec::new())))
            }
            b"lines" | b"rows" => {
                let arg = self.grab_gnu_arg(rest);
                self.o.screen_rows = number(arg)
                    .ok_or_else(|| Stop::Error("number of rows must follow --rows or --lines".into()))?;
                Ok(())
            }
            b"pid" => list(
                self,
                "list of process IDs must follow --pid",
                SelKind::Pid,
                ListKind::Pid,
            ),
            b"ppid" => list(
                self,
                "list of process IDs must follow --ppid",
                SelKind::Ppid,
                ListKind::Pid,
            ),
            b"quick-pid" => list(
                self,
                "List of process IDs must follow --quick-pid.",
                SelKind::PidQuick,
                ListKind::Pid,
            ),
            b"sid" => list(
                self,
                "some sid thing(s) must follow --sid",
                SelKind::Sess,
                ListKind::Pid,
            ),
            b"sort" => {
                let arg = self
                    .grab_gnu_arg(rest)
                    .ok_or_else(|| Stop::Error("long sort specification must follow --sort".into()))?;
                self.defer_sf_option(arg, SfCode::GSort)
            }
            b"tty" => list(
                self,
                "list of ttys must follow --tty",
                SelKind::Tty,
                ListKind::Tty,
            ),
            b"user" => list(
                self,
                "list of effective users must follow --user",
                SelKind::Euid,
                ListKind::Uid,
            ),
            b"version" => {
                self.exclusive("--version")?;
                Err(Stop::Exit(Parsed::Print(VERSION.into())))
            }
            b"context" => {
                self.o.format_flags |= FF_FC;
                Ok(())
            }
            _ => err("unknown gnu long option"),
        }
    }

    /// parse_trailing_pids: every argument left, PIDs, `-`process groups and
    /// `+`sessions.
    fn parse_trailing_pids(&mut self) -> Step {
        let mut lists = [
            (SelKind::Pid, Vec::new()),
            (SelKind::Pgrp, Vec::new()),
            (SelKind::Sess, Vec::new()),
        ];
        for i in self.thisarg..self.argv.len() {
            let data = self.arg(i);
            let (list, data) = match data.first() {
                Some(b'-') => (1, data.get(1..).unwrap_or_default()),
                Some(b'+') => (2, data.get(1..).unwrap_or_default()),
                _ => (0, data),
            };
            let pid = parse_pid(data).map_err(|e| Stop::Error(e.into()))?;
            if let Some((_, nums)) = lists.get_mut(list) {
                nums.push(pid);
            }
        }
        self.thisarg = self.argv.len().saturating_sub(1);
        for (kind, nums) in lists {
            if !nums.is_empty() {
                self.o.selection.push(Sel {
                    kind,
                    nums,
                    cmds: Vec::new(),
                });
            }
        }
        Ok(())
    }

    /// parse_list: items separated by blanks, tabs or commas, none empty.
    fn parse_list(&self, arg: &[u8], kind: SelKind, parse: ListKind) -> Result<Sel, &'static str> {
        if !well_formed_list(arg) {
            return Err("improper list");
        }
        let mut sel = Sel {
            kind,
            nums: Vec::new(),
            cmds: Vec::new(),
        };
        for item in arg.split(|&b| b == b' ' || b == b',' || b == b'\t') {
            match parse {
                ListKind::Pid => sel.nums.push(parse_pid(item)?),
                ListKind::Uid => sel.nums.push(self.parse_id(item, true)?),
                ListKind::Gid => sel.nums.push(self.parse_id(item, false)?),
                ListKind::Cmd => sel.cmds.push(item.iter().take(63).copied().collect()),
                ListKind::Tty => sel.nums.push(self.parse_tty(item)?),
            }
        }
        Ok(sel)
    }

    /// parse_uid and parse_gid: a number, or a name.
    fn parse_id(&self, s: &[u8], user: bool) -> Result<u32, &'static str> {
        let (v, end) = strtol(s, 0);
        let mut num = as_ulong(v);
        if end != s.len() {
            let id = if user {
                self.system.uid_named(s)
            } else {
                self.system.gid_named(s)
            };
            match id {
                Some(id) => num = u64::from(id),
                None if !self.o.negate_selection => {
                    return Err(if user {
                        "user name does not exist"
                    } else {
                        "group name does not exist"
                    });
                }
                None => num = u64::MAX,
            }
        }
        if !self.o.negate_selection && num > 0xffff_fffe {
            return Err(if user {
                "user ID out of range"
            } else {
                "group ID out of range"
            });
        }
        Ok(num as u32)
    }

    /// parse_tty: the device of a terminal named as /dev, /dev/pts or /dev/tty names
    /// it, or none for `-` and `?`.
    fn parse_tty(&self, s: &[u8]) -> Result<u32, &'static str> {
        let candidates: Vec<Vec<u8>> = if s.first() == Some(&b'/') {
            vec![s.to_vec()]
        } else {
            ["/dev/pts/", "/dev/", "/dev/tty", "/dev/pty"]
                .iter()
                .map(|p| [p.as_bytes(), s].concat())
                .chain([[b"/dev/", s, b"nsole"].concat()])
                .collect()
        };
        if let Some(dev) = candidates
            .iter()
            .find_map(|c| super::dump::tty_device(c, self.procs))
        {
            return Ok(dev as u32);
        }
        if s.first() != Some(&b'/') && (s == b"-" || s == b"?") {
            return Ok(0);
        }
        Err("TTY could not be found")
    }

    /// defer_sf_option.
    fn defer_sf_option(&mut self, sf: Vec<u8>, code: SfCode) -> Step {
        if code == SfCode::GSort {
            self.have_gnu_sort = true;
        }
        self.sf_list.push(SfNode {
            sf,
            code,
            f_cooked: Vec::new(),
            s_cooked: Vec::new(),
        });
        Ok(())
    }

    fn thread_option_check(&mut self) -> Step {
        let t = &mut self.o.thread_flags;
        if *t == 0 {
            *t = TF_SHOW_PROC;
            return Ok(());
        }
        if self.o.forest_type != 0 {
            return err("thread display conflicts with forest display");
        }
        if *t & TF_B_H != 0 && *t & (TF_B_M | TF_U_M) != 0 {
            return err("thread flags conflict; can't use H with m or -m");
        }
        if *t & TF_B_M != 0 && *t & TF_U_M != 0 {
            return err("thread flags conflict; can't use both m and -m");
        }
        if *t & TF_U_L != 0 && *t & TF_U_T != 0 {
            return err("thread flags conflict; can't use both -L and -T");
        }
        if *t & TF_B_H != 0 {
            *t |= TF_SHOW_PROC | TF_LOOSE_TASKS;
        }
        if *t & (TF_B_M | TF_U_M) != 0 {
            *t |= TF_SHOW_PROC | TF_SHOW_TASK | TF_SHOW_BOTH;
        }
        if *t & (TF_U_T | TF_U_L) != 0 {
            if *t & (TF_B_M | TF_U_M | TF_B_H) != 0 {
                *t |= TF_MUST_USE;
            } else {
                *t |= TF_SHOW_TASK;
            }
        }
        Ok(())
    }

    fn select_bits_setup(&mut self) -> Step {
        let o = &mut self.o;
        if o.simple_select == 0 && !o.prefer_bsd_defaults {
            o.select_bits = 0xaa00;
            return Ok(());
        }
        // Without SunOS's personality, g is always on.
        let switch = if o.simple_select & (SS_U_A | SS_U_D) == 0 {
            o.simple_select | SS_B_G
        } else {
            o.simple_select
        };
        o.select_bits = match switch {
            x if x == SS_U_A | SS_U_D => 0x3f3f,
            SS_U_A => 0x0303,
            SS_U_D => 0x3333,
            0 => 0x0202,
            SS_B_A => 0x0303,
            SS_B_X => 0x2222,
            x if x == SS_B_X | SS_B_A => 0x3333,
            SS_B_G => 0x0a0a,
            x if x == SS_B_G | SS_B_A => 0x0f0f,
            x if x == SS_B_G | SS_B_X => 0xaaaa,
            x if x == SS_B_G | SS_B_X | SS_B_A => {
                o.all_processes = true;
                o.simple_select = 0;
                o.select_bits
            }
            _ => return err("process selection options conflict"),
        };
        Ok(())
    }

    /// parse_O_option over every deferred list, the first given first.
    fn parse_o_options(&mut self) -> Step {
        let mut list = std::mem::take(&mut self.sf_list);
        let result = list.iter_mut().try_for_each(|sfn| self.parse_o_option(sfn));
        self.sf_list = list;
        result
    }

    fn parse_o_option(&mut self, sfn: &mut SfNode) -> Step {
        match sfn.code {
            SfCode::Bo | SfCode::GFormat | SfCode::Uo => {
                self.format_parse(sfn)?;
                self.already_parsed_format = true;
            }
            SfCode::UO => {
                if self.already_parsed_format {
                    return err("option -O can not follow other format options");
                }
                self.format_parse(sfn)?;
                self.already_parsed_format = true;
                o_wrap(sfn, b'u');
            }
            SfCode::BO => {
                let sort_err = if self.have_gnu_sort || self.already_parsed_sort {
                    Some("multiple sort options")
                } else {
                    self.verify_short_sort(&sfn.sf)
                };
                let Some(sort_err) = sort_err else {
                    short_sort_parse(sfn);
                    self.already_parsed_sort = true;
                    return Ok(());
                };
                if self.already_parsed_format {
                    return err("option O is neither first format nor sort order");
                }
                if self.format_parse(sfn).is_ok() {
                    self.already_parsed_format = true;
                    o_wrap(sfn, b'b');
                    return Ok(());
                }
                return err(sort_err);
            }
            SfCode::GSort => {
                let result = if self.already_parsed_sort {
                    err("multiple sort options")
                } else {
                    long_sort_parse(sfn)
                };
                self.already_parsed_sort = true;
                result?;
            }
        }
        Ok(())
    }

    /// verify_short_sort.
    fn verify_short_sort(&self, arg: &[u8]) -> Option<&'static str> {
        const ALL: &[u8] = b"CGJKMNPRSTUcfgjkmnoprstuvy+-";
        if !arg.iter().all(|c| ALL.contains(c)) {
            return Some("bad sorting code");
        }
        let mut seen = [false; 256];
        for (i, &c) in arg.iter().enumerate() {
            match c {
                b'+' | b'-' => {
                    if matches!(arg.get(i + 1), None | Some(b'+' | b'-')) {
                        return Some("bad sorting code");
                    }
                }
                _ => {
                    if c == b'P' && self.o.forest_type != 0 {
                        return Some("PPID sort and forest output conflict");
                    }
                    let slot = seen.get_mut(usize::from(c))?;
                    if *slot {
                        return Some("bad sorting code");
                    }
                    *slot = true;
                }
            }
        }
        None
    }

    /// format_parse: specifiers separated by blanks, commas, tabs or newlines, each
    /// with an optional `:width` and `=header` (the last header takes the rest of the
    /// list); failing that, AIX's `%` descriptors when there is a `%`.
    fn format_parse(&mut self, sfn: &mut SfNode) -> Step {
        match self.format_items(sfn) {
            Ok(()) => {
                self.already_parsed_format = true;
                Ok(())
            }
            Err(e) => {
                if sfn.sf.contains(&b'%') {
                    aix_format_parse(sfn).map_err(|e| Stop::Error(e.into()))?;
                    self.already_parsed_format = true;
                    return Ok(());
                }
                Err(Stop::Error(e))
            }
        }
    }

    fn format_items(&mut self, sfn: &mut SfNode) -> Result<(), String> {
        let mut buf = sfn.sf.clone();
        let delim = |c: u8| matches!(c, b' ' | b',' | b'\t' | b'\n');
        let (items, trailing) = count_items(&buf, delim).ok_or("improper format list")?;
        if items == 0 {
            return Err("empty format list".into());
        }
        if trailing {
            buf.pop();
        }
        let mut walk = Some(0_usize);
        for left in (0..items).rev() {
            let start = walk.ok_or("please report this bug")?;
            let rest = buf.get(start..).unwrap_or_default();
            let sep = rest.iter().position(|&c| delim(c));
            let item = match sep {
                Some(sep) if left > 0 => rest.get(..sep).unwrap_or_default(),
                _ => rest,
            };
            let (spec, header) = match item.iter().position(|&c| c == b'=') {
                Some(eq) => (
                    item.get(..eq).unwrap_or_default(),
                    Some(item.get(eq + 1..).unwrap_or_default()),
                ),
                None => (item, None),
            };
            let (spec, width) = match spec.iter().position(|&c| c == b':') {
                Some(colon) => {
                    let w = spec.get(colon + 1..).unwrap_or_default();
                    let n = atoi(w);
                    if w.is_empty() || !w.iter().all(u8::is_ascii_digit) || w.first() == Some(&b'0') || n <= 0
                    {
                        return Err("column widths must be unsigned decimal numbers".into());
                    }
                    (spec.get(..colon).unwrap_or_default(), Some(n))
                }
                None => (spec, None),
            };
            let Some(mut nodes) = do_one_spec(spec, header) else {
                let msg = self.errbuf.get_or_insert_with(|| {
                    let mut msg = format!(
                        "unknown user-defined format specifier \"{}\"",
                        String::from_utf8_lossy(spec)
                    );
                    // errbuf is 80 bytes.
                    while msg.len() > 79 {
                        msg.pop();
                    }
                    msg
                });
                return Err(msg.clone());
            };
            if let Some(width) = width {
                if nodes.len() > 1 {
                    return Err("can not set width for a macro (multi-column) format specifier".into());
                }
                for n in &mut nodes {
                    n.width = width;
                }
            }
            sfn.f_cooked.extend(nodes);
            walk = sep.map(|s| start + s + 1);
        }
        Ok(())
    }

    /// process_sf_options: the deferred lists parsed and merged, else the format the
    /// flags name (generate_sysv_list for SysV's), then the modifiers' changes to it.
    fn process_sf_options(&mut self) -> Step {
        self.parse_o_options()?;
        for sfn in &mut self.sf_list {
            self.o.format_list.append(&mut sfn.f_cooked);
            self.o.sort_list.extend(sfn.s_cooked.drain(..).rev());
        }
        let o = &mut self.o;
        if !o.format_list.is_empty() {
            if o.format_flags != 0 {
                return err("conflicting format options");
            }
            if o.format_modifiers != 0 {
                return err("can not use output modifiers with user-defined output");
            }
            if o.thread_flags & TF_MUST_USE != 0 {
                return err("-L/-T with H/m/-m and -o/-O/o/O is nonsense");
            }
            return Ok(());
        }
        let spec = match o.format_flags {
            0 | FF_UF | FF_UL => None,
            x if x == FF_UF | FF_UL => None,
            FF_BJ => Some("OL_j"),
            FF_BL => Some("OL_l"),
            FF_BS => Some("OL_s"),
            FF_BU => Some("OL_u"),
            FF_BV => Some("OL_v"),
            FF_LX => Some("OL_X"),
            FF_LM => Some("OL_m"),
            FF_FC => Some("FLASK_context"),
            // FF_Uj is set only with a personality.
            _ => return err("conflicting format options"),
        };
        let Some(spec) = spec else {
            return self.generate_sysv_list();
        };
        o.format_list = do_one_spec(spec.as_bytes(), None).unwrap_or_default();
        let one = |name: &str| do_one_spec(name.as_bytes(), None).unwrap_or_default();
        if o.format_modifiers & FM_J != 0 {
            if !add_after(&mut o.format_list, "PPID", one("pgid"))
                && !add_after(&mut o.format_list, "PID", one("pgid"))
            {
                return catastrophic(885, "internal error: no PID or PPID for -j option");
            }
            if !add_after(&mut o.format_list, "PGID", one("sid")) {
                return err("lost my PGID");
            }
        }
        if o.format_modifiers & FM_Y != 0 {
            delete(&mut o.format_list, "F");
            if add_after(&mut o.format_list, "ADDR", one("rss")) {
                delete(&mut o.format_list, "ADDR");
            }
        }
        if o.format_modifiers & FM_C != 0 {
            for name in ["%CPU", "CPU", "CP", "C", "NI"] {
                delete(&mut o.format_list, name);
            }
            if !add_after(&mut o.format_list, "PRI", one("class")) {
                return catastrophic(900, "internal error: no PRI for -c option");
            }
            delete(&mut o.format_list, "PRI");
            if !add_after(&mut o.format_list, "CLS", one("pri")) {
                return err("lost my CLS");
            }
        }
        if o.thread_flags & TF_U_T != 0
            && !add_after(&mut o.format_list, "PID", one("spid"))
            && o.thread_flags & TF_MUST_USE != 0
        {
            return err("-T with H/-m/m but no PID for SPID to follow");
        }
        if o.thread_flags & TF_U_L != 0 {
            let placed = ["SID", "SESS", "PGID", "PGRP", "PPID", "PID"]
                .iter()
                .any(|after| add_after(&mut o.format_list, after, one("lwp")));
            if !placed && o.thread_flags & TF_MUST_USE != 0 {
                return err("-L with H/-m/m but no PID/PGID/SID/SESS for NLWP to follow");
            }
            add_after(&mut o.format_list, "%CPU", one("nlwp"));
        }
        if o.format_modifiers & FM_M != 0 {
            let mut list = one("label");
            list.append(&mut o.format_list);
            o.format_list = list;
        }
        Ok(())
    }

    /// generate_sysv_list: the SysV format for -f, -l, -F, -j, -y, -c, -P, -M, -L and
    /// -T, built backwards.
    fn generate_sysv_list(&mut self) -> Step {
        let o = &mut self.o;
        let (ff, fm, tf) = (o.format_flags, o.format_modifiers, o.thread_flags);
        if fm & FM_Y != 0 && ff & FF_UL == 0 {
            return err("modifier -y without format -l makes no sense");
        }
        let mut list: Vec<&str> = Vec::new();
        if o.prefer_bsd_defaults {
            list.push(if ff != 0 { "cmd" } else { "args" });
            list.push("bsdtime");
            if ff & FF_UL == 0 {
                list.push("stat");
            }
        } else {
            list.push(if ff & FF_UF != 0 { "cmd" } else { "ucmd" });
            list.push("time");
        }
        list.push("tname");
        if ff & FF_UF != 0 {
            list.push("stime");
        }
        if fm & FM_F != 0 {
            if fm & FM_P == 0 {
                list.push("psr");
            }
            if !(ff & FF_UL != 0 && fm & FM_Y != 0) {
                list.push("rss");
            }
        }
        if ff & FF_UL != 0 {
            list.push("wchan");
        }
        if fm & FM_F != 0 || ff & FF_UL != 0 {
            list.push("sz");
        }
        if ff & FF_UL != 0 {
            list.push(if fm & FM_Y != 0 { "rss" } else { "addr_1" });
        }
        if fm & FM_C != 0 {
            list.push("pri");
            list.push("class");
        } else if ff & FF_UL != 0 {
            list.push("ni");
            list.push("opri");
        }
        if tf & TF_U_L != 0 && ff & FF_UF != 0 {
            list.push("nlwp");
        }
        if ff & (FF_UF | FF_UL) != 0 && fm & FM_C == 0 {
            list.push("c");
        }
        if fm & FM_P != 0 {
            list.push("psr");
        }
        if tf & TF_U_L != 0 {
            list.push("lwp");
        }
        if fm & FM_J != 0 {
            list.push("sid");
            list.push("pgid");
        }
        if ff & (FF_UF | FF_UL) != 0 {
            list.push("ppid");
        }
        if tf & TF_U_T != 0 {
            list.push("spid");
        }
        list.push("pid");
        if ff & FF_UF != 0 {
            list.push("uid_hack");
        } else if ff & FF_UL != 0 {
            list.push("uid");
        }
        if ff & FF_UL != 0 {
            list.push("s");
            if fm & FM_Y == 0 {
                list.push("f");
            }
        }
        if fm & FM_M != 0 {
            list.push("label");
        }
        o.format_list = list
            .iter()
            .rev()
            .flat_map(|s| do_one_spec(s.as_bytes(), None).unwrap_or_default())
            .collect();
        Ok(())
    }
}

/// How parse_list reads each item.
#[derive(Debug, Clone, Copy)]
enum ListKind {
    Pid,
    Uid,
    Gid,
    Cmd,
    Tty,
}

/// ps's own terminal: none, as dockerd's child has none.
const CACHED_TTY: u32 = 0;

/// display_ps_version.
const VERSION: &str = "ps from procps-ng 4.0.2\n";

/// catastrophic_failure: error_at_line's message, and exit.
fn catastrophic(line: u32, msg: &str) -> Step {
    Err(Stop::Exit(Parsed::Fail(format!(
        "ps:src/ps/sortformat.c:{line}: {msg}"
    ))))
}

fn arg_type(s: &[u8]) -> ArgType {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    match at(0) {
        b'a'..=b'z' | b'A'..=b'Z' => return ArgType::Bsd,
        b'0'..=b'9' => return ArgType::Pid,
        b'+' => return ArgType::Sess,
        b'-' => {}
        _ => return ArgType::Fail,
    }
    match at(1) {
        b'a'..=b'z' | b'A'..=b'Z' => return ArgType::Sysv,
        b'0'..=b'9' => return ArgType::Pgrp,
        b'-' => {}
        _ => return ArgType::Fail,
    }
    match at(2) {
        b'a'..=b'z' | b'A'..=b'Z' => ArgType::Gnu,
        0 => ArgType::End,
        _ => ArgType::Fail,
    }
}

/// parse_pid.
fn parse_pid(s: &[u8]) -> Result<u32, &'static str> {
    let (v, end) = strtol(s, 0);
    // An empty one ("+") passes this, and is 0.
    if end != s.len() {
        return Err("process ID list syntax error");
    }
    let num = as_ulong(v);
    if !(1..=0x7fff_ffff).contains(&num) {
        return Err("process ID out of range");
    }
    Ok(num as u32)
}

/// atoi: strtol's long made an int.
fn atoi(s: &[u8]) -> i32 {
    as_long(strtol(s, 10).0) as i32
}

/// parse_list's check: no empty item, and no delimiter at the end.
fn well_formed_list(arg: &[u8]) -> bool {
    let delim = |c: u8| matches!(c, b' ' | b',' | b'\t');
    matches!(count_items(arg, delim), Some((n, false)) if n > 0)
}

/// The items of a list, and whether a delimiter ends it; None if a delimiter starts it
/// or follows another.
fn count_items(s: &[u8], delim: impl Fn(u8) -> bool) -> Option<(usize, bool)> {
    if s.is_empty() {
        return None;
    }
    let mut need_item = true;
    let mut items = 0;
    for &c in s {
        if delim(c) {
            if need_item {
                return None;
            }
            need_item = true;
        } else {
            if need_item {
                items += 1;
            }
            need_item = false;
        }
    }
    Some((items, need_item))
}

/// do_one_spec: a specifier's column, or a macro's columns, with the header given.
fn do_one_spec(spec: &[u8], header: Option<&[u8]>) -> Option<Vec<Node>> {
    if let Some(fs) = specs::spec(spec) {
        let w1 = if fs.flags & PIDMAX != 0 {
            PID_LENGTH.max(fs.head.len() as i32)
        } else {
            fs.width
        };
        let (name, width) = match header {
            Some(h) => (h.to_vec(), w1.max(h.len().min(i32::MAX as usize) as i32)),
            None => (fs.head.as_bytes().to_vec(), w1),
        };
        return Some(vec![Node {
            name,
            pr: Some(fs.pr),
            width,
            flags: fs.flags,
        }]);
    }
    let list = specs::macro_list(spec)?;
    Some(
        list.split([',', ' '])
            .filter(|s| !s.is_empty())
            .flat_map(|s| do_one_spec(s.as_bytes(), header).unwrap_or_default())
            .collect(),
    )
}

/// O_wrap: PID first, the trailer after.
fn o_wrap(sfn: &mut SfNode, otype: u8) {
    let trailer = if otype == b'b' { "END_BSD" } else { "END_SYS5" };
    let mut list = do_one_spec(b"pid", None).unwrap_or_default();
    list.append(&mut sfn.f_cooked);
    list.extend(do_one_spec(trailer.as_bytes(), None).unwrap_or_default());
    sfn.f_cooked = list;
}

/// aix_format_parse: `%` descriptors and the blanks between them.
fn aix_format_parse(sfn: &mut SfNode) -> Result<(), &'static str> {
    let s = &sfn.sf;
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    // The C state machine, counting items.
    let mut items = 0;
    let mut i = 0;
    let mut c = at(i);
    i += 1;
    'initial: loop {
        if c != b'%' {
            if c == 0 {
                break 'initial;
            }
            items += 1;
            loop {
                c = at(i);
                i += 1;
                if c == b'%' {
                    break;
                }
                if c == b' ' {
                    continue;
                }
                if c != 0 {
                    return Err("improper AIX field descriptor");
                }
                break 'initial;
            }
        }
        // get_desc
        items += 1;
        c = at(i);
        i += 1;
        if c == 0 || c == b' ' {
            return Err("missing AIX field descriptor");
        }
    }
    let mut walk = 0;
    for _ in 0..items {
        if at(walk) == b'%' {
            walk += 1;
            if at(walk) == b'%' {
                return Err("missing AIX field descriptor");
            }
            let desc = at(walk);
            walk += 1;
            let (_, spec, head) = AIX
                .iter()
                .find(|(d, _, _)| *d == desc)
                .ok_or("unknown AIX field descriptor")?;
            let nodes = do_one_spec(spec.as_bytes(), Some(head.as_bytes()))
                .ok_or("AIX field descriptor processing bug")?;
            sfn.f_cooked.extend(nodes);
        } else {
            let text: Vec<u8> = s
                .get(walk..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&c| c != b'%')
                .copied()
                .collect();
            walk += text.len();
            sfn.f_cooked.push(Node {
                width: text.len().min(i32::MAX as usize) as i32,
                name: text,
                pr: None,
                flags: 0,
            });
        }
    }
    Ok(())
}

/// do_one_sort_spec: a specifier with an optional `+` or `-`.
fn do_one_sort_spec(spec: &[u8]) -> Option<SortNode> {
    let (order, spec) = match spec.first() {
        Some(b'-') => (-1, spec.get(1..).unwrap_or_default()),
        Some(b'+') => (1, spec.get(1..).unwrap_or_default()),
        _ => (1, spec),
    };
    specs::spec(spec).map(|fs| SortNode {
        sr: fs.sr,
        pr: fs.pr,
        order,
    })
}

/// long_sort_parse.
fn long_sort_parse(sfn: &mut SfNode) -> Step {
    let delim = |c: u8| matches!(c, b' ' | b',' | b'\t' | b'\n');
    let (items, trailing) =
        count_items(&sfn.sf, delim).ok_or_else(|| Stop::Error("improper sort list".into()))?;
    if items == 0 {
        return err("empty sort list");
    }
    let sf = if trailing {
        sfn.sf.get(..sfn.sf.len() - 1).unwrap_or_default()
    } else {
        &sfn.sf[..]
    };
    let nodes: Option<Vec<SortNode>> = sf.split(|&c| delim(c)).map(do_one_sort_spec).collect();
    sfn.s_cooked = nodes.ok_or_else(|| Stop::Error("unknown sort specifier".into()))?;
    Ok(())
}

/// short_sort_parse: its error is not looked at, so the keys before an unknown one
/// stay. Only the first key has an order unless one is given: the others' is none.
fn short_sort_parse(sfn: &mut SfNode) {
    let mut order = 1;
    for &c in &sfn.sf {
        match c {
            b'+' => order = 1,
            b'-' => order = -1,
            _ => {
                let Some(node) = SHORTSORT
                    .iter()
                    .find(|(d, _)| *d == c)
                    .and_then(|(_, s)| do_one_sort_spec(s.as_bytes()))
                else {
                    return;
                };
                sfn.s_cooked.push(SortNode { order, ..node });
                order = 0;
            }
        }
    }
}

/// fmt_add_after: `put` after the first column headed `find`.
fn add_after(list: &mut Vec<Node>, find: &str, put: Vec<Node>) -> bool {
    let Some(at) = list.iter().position(|n| n.name == find.as_bytes()) else {
        return false;
    };
    let tail = list.split_off(at + 1);
    list.extend(put);
    list.extend(tail);
    true
}

/// fmt_delete: the first column headed `find`.
fn delete(list: &mut Vec<Node>, find: &str) {
    if let Some(at) = list.iter().position(|n| n.name == find.as_bytes()) {
        list.remove(at);
    }
}

/// print_format_specifiers (`ps L`).
fn format_specifiers() -> Vec<u8> {
    let mut out = String::new();
    for s in SPECS
        .iter()
        .take_while(|s| s.spec != "~")
        .filter(|s| s.pr != Pr::Nop)
    {
        let clip = |t: &str, n: usize| t.chars().take(n).collect::<String>();
        out.push_str(&format!("{:<12} {:<8}\n", clip(s.spec, 12), clip(s.head, 8)));
    }
    out.into_bytes()
}

/// do_help to stdout (`--help [section]`).
fn help(opt: Option<&[u8]>) -> Vec<u8> {
    const SECTIONS: [(&str, &str); 6] = [
        ("simple", "s"),
        ("list", "l"),
        ("output", "o"),
        ("threads", "t"),
        ("misc", "m"),
        ("all", "a"),
    ];
    let section = opt.and_then(|o| {
        SECTIONS
            .iter()
            .position(|(w, a)| o == w.as_bytes() || o == a.as_bytes())
    });
    let shows = |i: usize| section == Some(i) || section == Some(5);
    let mut out = String::from("\nUsage:\n ps [options]\n");
    if shows(0) {
        out.push_str(HELP_SIMPLE);
    }
    if shows(1) {
        out.push_str(HELP_LIST);
    }
    if shows(2) {
        out.push_str(HELP_OUTPUT);
    }
    if shows(3) {
        out.push_str(HELP_THREADS);
    }
    if shows(4) {
        out.push_str(HELP_MISC);
    }
    if section.is_none() {
        out.push_str(
            "\n Try 'ps --help <simple|list|output|threads|misc|all>'\n  or 'ps --help <s|l|o|t|m|a>'\n for additional help text.\n",
        );
    }
    out.push_str("\nFor more details see ps(1).\n");
    out.into_bytes()
}

const HELP_SIMPLE: &str = "
Basic options:
 -A, -e               all processes
 -a                   all with tty, except session leaders
  a                   all with tty, including other users
 -d                   all except session leaders
 -N, --deselect       negate selection
  r                   only running processes
  T                   all processes on this terminal
  x                   processes without controlling ttys
";

const HELP_LIST: &str = "
Selection by list:
 -C <command>         command name
 -G, --Group <GID>    real group id or name
 -g, --group <group>  session or effective group name
 -p, p, --pid <PID>   process id
        --ppid <PID>  parent process id
 -q, q, --quick-pid <PID>
                      process id (quick mode)
 -s, --sid <session>  session id
 -t, t, --tty <tty>   terminal
 -u, U, --user <UID>  effective user id or name
 -U, --User <UID>     real user id or name

  The selection options take as their argument either:
    a comma-separated list e.g. '-u root,nobody' or
    a blank-separated list e.g. '-p 123 4567'
";

const HELP_OUTPUT: &str = "
Output formats:
 -F                   extra full
 -f                   full-format, including command lines
  f, --forest         ascii art process tree
 -H                   show process hierarchy
 -j                   jobs format
  j                   BSD job control format
 -l                   long format
  l                   BSD long format
 -M, Z                add security data (for SELinux)
 -O <format>          preloaded with default columns
  O <format>          as -O, with BSD personality
 -o, o, --format <format>
                      user-defined format
  -P                  add psr column
  s                   signal format
  u                   user-oriented format
  v                   virtual memory format
  X                   register format
 -y                   do not show flags, show rss vs. addr (used with -l)
     --context        display security context (for SELinux)
     --headers        repeat header lines, one per page
     --no-headers     do not print header at all
     --cols, --columns, --width <num>
                      set screen width
     --rows, --lines <num>
                      set screen height
";

const HELP_THREADS: &str = "
Show threads:
  H                   as if they were processes
 -L                   possibly with LWP and NLWP columns
 -m, m                after processes
 -T                   possibly with SPID column
";

const HELP_MISC: &str = "
Miscellaneous options:
 -c                   show scheduling class with -l option
  c                   show true command name
  e                   show the environment after command
  k,    --sort        specify sort order as: [+|-]key[,[+|-]key[,...]]
  L                   show format specifiers
  n                   display numeric uid and wchan
  S,    --cumulative  include some dead child process data
 -y                   do not show flags, show rss (only with -l)
 -V, V, --version     display version information and exit
 -w, w                unlimited output width

        --help <simple|list|output|threads|misc|all>
                      display help and exit
";

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[Node]) -> Vec<String> {
        list.iter()
            .map(|n| String::from_utf8_lossy(&n.name).into_owned())
            .collect()
    }

    fn opts(args: &[&str]) -> Box<Opts> {
        let (system, procs) = super::super::dump::parse(include_bytes!("../testdata/procps/dump")).unwrap();
        let argv: Vec<Vec<u8>> = std::iter::once("ps")
            .chain(args.iter().copied())
            .map(|a| a.as_bytes().to_vec())
            .collect();
        match parse(&argv, &system, &procs) {
            Parsed::Run(o) => o,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn fail(args: &[&str]) -> String {
        let (system, procs) = super::super::dump::parse(include_bytes!("../testdata/procps/dump")).unwrap();
        let argv: Vec<Vec<u8>> = std::iter::once("ps")
            .chain(args.iter().copied())
            .map(|a| a.as_bytes().to_vec())
            .collect();
        match parse(&argv, &system, &procs) {
            Parsed::Fail(e) => e,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn formats_are_built_as_sortformat_builds_them() {
        assert_eq!(
            names(&opts(&["-c", "-l"]).format_list),
            [
                "F", "S", "UID", "PID", "PPID", "CLS", "PRI", "ADDR", "SZ", "WCHAN", "TTY", "TIME", "CMD"
            ]
        );
        assert_eq!(
            names(&opts(&["-O", "user"]).format_list),
            ["PID", "USER", "S", "TTY", "TIME", "COMMAND"]
        );
        assert_eq!(
            names(&opts(&["O", "user"]).format_list),
            ["PID", "USER", "S", "TTY", "TIME", "COMMAND"]
        );
        assert_eq!(
            names(&opts(&["j", "-j"]).format_list).get(..5).unwrap(),
            ["PPID", "PGID", "SID", "PID", "PGID"]
        );
        assert_eq!(
            names(&opts(&["-o", "%p %c"]).format_list),
            ["PID", " ", "COMMAND", ""]
        );
        assert_eq!(
            names(&opts(&["-o", "pid,args=WHAT"]).format_list),
            ["PID", "WHAT"]
        );
        // The header takes no delimiters: what follows one is another specifier.
        assert_eq!(
            fail(&["-o", "pid,args=A,B"]),
            "error: unknown user-defined format specifier \"B\""
        );
        assert_eq!(opts(&["-o", "pid,user:3"]).format_list.last().unwrap().width, 3);
        let o = opts(&["k", "-pid,user"]);
        assert_eq!(
            o.sort_list.iter().map(|s| (s.sr, s.order)).collect::<Vec<_>>(),
            [(Sr::IdEuser, 1), (Sr::IdTgid, -1)]
        );
        // Short keys after the first have no order.
        let o = opts(&["O", "Pp"]);
        assert_eq!(o.sort_list.iter().map(|s| s.order).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn errors_are_the_first_pass_errors() {
        // The second pass stops at `aux` ("way bad"): the first pass's error is shown.
        assert_eq!(
            fail(&["-x", "aux"]),
            "error: must set personality to get -x option"
        );
        assert_eq!(
            fail(&["-o", "pid,nope"]),
            "error: unknown user-defined format specifier \"nope\""
        );
        assert_eq!(
            fail(&["-o", "user:0"]),
            "error: column widths must be unsigned decimal numbers"
        );
        assert_eq!(
            fail(&["-c", "u"]),
            "ps:src/ps/sortformat.c:900: internal error: no PRI for -c option"
        );
        assert_eq!(fail(&["-f", "u"]), "error: conflicting format options");
        assert_eq!(fail(&["--nope"]), "error: unknown gnu long option");
        assert_eq!(fail(&["-u", "nobody-here"]), "error: user name does not exist");
        // -ax fails as SysV, and the second pass reads it as BSD.
        assert_eq!(opts(&["-ax"]).simple_select, 0);
        assert!(opts(&["-ax"]).all_processes);
    }
}
