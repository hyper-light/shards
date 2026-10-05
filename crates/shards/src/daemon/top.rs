//! `shards top`: a microVM's processes laid out as procps-ng 4.0.2's `ps` lays them out,
//! from what the guest's init read of them in `/proc` (init procs.rs), then taken apart
//! as dockerd takes `ps`'s output apart (moby daemon/top_unix.go, parsePSOutput).
//!
//! dockerd runs `ps ARGS -qPIDS` and, when that fails (`-q` refuses sorting, forests,
//! negation and other selections), `ps ARGS` (ContainerTop); `ps` here does the same
//! over the guest's processes, which are all the container's. procps's parser, formats
//! and layout are ported in `top/`; what the guest does not send (threads, `/proc/PID/io`
//! and the like) is refused rather than shown wrong.

mod args;
mod clock;
mod dump;
mod output;
mod specs;

use args::{Parsed, TF_SHOW_PROC};

/// What `top` shows: ps's titles, and each process's fields under them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Table {
    pub titles: Vec<String>,
    pub processes: Vec<Vec<String>>,
}

/// What `ps ARGS` (ARGS as dockerd splits them, on spaces, after its validatePSArgs)
/// prints of the processes in `dump`, in a zone `utc_offset` seconds east of UTC; or what ps says is
/// wrong with ARGS, as dockerd says it (`ps: ` and the first line ps wrote to stderr).
pub(super) fn ps(dump: &[u8], args: &[&str], utc_offset: i32) -> Result<String, String> {
    let args = args.to_vec();
    let (system, procs) = dump::parse(dump)?;
    // exec refuses a NUL in an argument.
    if args.iter().any(|a| a.contains('\0')) {
        return Err("ps: fork/exec /usr/bin/ps: invalid argument".into());
    }
    let pids: Vec<String> = procs.iter().map(|p| p.tid.to_string()).collect();
    let quick = format!("-q{}", pids.join(","));
    let mut with_quick = args.clone();
    with_quick.push(&quick);
    match run(&system, &procs, &with_quick, utc_offset) {
        Ran::Printed(text) => return Ok(text),
        Ran::Unsupported(msg) => return Err(msg),
        Ran::Failed(_) => {}
    }
    match run(&system, &procs, &args, utc_offset) {
        Ran::Printed(text) => Ok(text),
        Ran::Unsupported(msg) => Err(msg),
        Ran::Failed(why) => Err(format!("ps: {why}")),
    }
}

/// How one run of ps ends.
enum Ran {
    /// Successfully, with this on stdout.
    Printed(String),
    /// Failing: the first line of its stderr, or the exit status when it wrote none.
    Failed(String),
    /// Asking for what the guest does not send.
    Unsupported(String),
}

/// One run of ps (display.c main) over the dump's processes.
fn run(system: &dump::System, procs: &[dump::Proc], args: &[&str], utc_offset: i32) -> Ran {
    let argv: Vec<Vec<u8>> = std::iter::once("ps")
        .chain(args.iter().copied())
        .map(|a| a.as_bytes().to_vec())
        .collect();
    let o = match args::parse(&argv, system, procs) {
        Parsed::Run(o) => o,
        Parsed::Print(text) => return Ran::Printed(String::from_utf8_lossy(&text).into_owned()),
        Parsed::Fail(line) => return Ran::Failed(line),
    };
    if let Some(msg) = conflicts(&o) {
        return Ran::Failed(msg.into());
    }
    if o.thread_flags != TF_SHOW_PROC {
        let option = o.thread_option.as_deref().unwrap_or("threads");
        return Ran::Unsupported(format!(
            "ps: shards top cannot list threads ({option}): the guest sends each process, not its threads"
        ));
    }
    for node in &o.format_list {
        if let Some(what) = node.pr.and_then(output::missing_column) {
            return Ran::Unsupported(format!(
                "ps: shards top cannot show {}: the guest does not send {what}",
                String::from_utf8_lossy(&node.name)
            ));
        }
    }
    if let Some(what) = o.sort_list.iter().find_map(|s| output::missing_sort(s.sr)) {
        return Ran::Unsupported(format!(
            "ps: shards top cannot sort by that: the guest does not send {what}"
        ));
    }
    let mut out = output::Out::new(&o, system, utc_offset);
    if let Err(line) = out.spew(procs) {
        return Ran::Failed(line);
    }
    if !out.finish() {
        return Ran::Failed("exit status 1".into());
    }
    Ran::Printed(String::from_utf8_lossy(&out.text).into_owned())
}

/// arg_check_conflicts (display.c): what `-q` cannot be used with.
fn conflicts(o: &args::Opts) -> Option<&'static str> {
    let quick = o
        .selection
        .iter()
        .filter(|s| s.kind == args::SelKind::PidQuick)
        .count();
    if quick > 1 {
        return Some("q/-q/--quick-pid can only be used once.");
    }
    if quick == 0 {
        return None;
    }
    if o.selection.len() > quick {
        return Some("q/-q/--quick-pid cannot be combined with other selection options.");
    }
    if o.forest_type != 0 {
        return Some("q/-q/--quick-pid cannot be used together with forest type listings.");
    }
    if !o.sort_list.is_empty() {
        return Some("q/-q,--quick-pid cannot be used together with sort options.");
    }
    if o.negate_selection {
        return Some("q/-q/--quick-pid cannot be used together with negation switches.");
    }
    None
}

/// fieldsASCII: fields between ASCII blanks.
fn fields(s: &str) -> Vec<&str> {
    s.split(['\t', '\n', '\x0c', '\r', ' '])
        .filter(|f| !f.is_empty())
        .collect()
}

/// `ps`'s output taken apart as dockerd takes it (parsePSOutput): the titles are the
/// header's fields; each line's fields under them, all past the last title joined by
/// spaces. A thread's line (PID `-`) follows its process's.
pub(super) fn table(output: &str) -> Result<Table, String> {
    let mut lines = output.split('\n');
    let titles: Vec<String> = fields(lines.next().unwrap_or_default())
        .into_iter()
        .map(str::to_string)
        .collect();
    let pid = titles
        .iter()
        .position(|t| t == "PID")
        .ok_or("Couldn't find PID field in ps output")?;
    let mut processes = Vec::new();
    let mut after_process = false;
    for line in lines.filter(|l| !l.is_empty()) {
        let f = fields(line);
        // Go indexes past the end with a panic; dockerd's ps never prints such a line.
        let field = f
            .get(pid)
            .ok_or_else(|| format!("Unexpected line in ps output: {line:?}"))?;
        if *field == "-" {
            if after_process {
                processes.push(row(&f, titles.len()));
            }
            continue;
        }
        field
            .parse::<i64>()
            .map_err(|e| format!("Unexpected pid '{field}': {e}"))?;
        // Every process the guest lists is the container's.
        after_process = true;
        processes.push(row(&f, titles.len()));
    }
    Ok(Table { titles, processes })
}

/// appendProcess2ProcList: the fields under all titles but the last, the rest joined.
fn row(fields: &[&str], titles: usize) -> Vec<String> {
    let cut = titles.saturating_sub(1).min(fields.len());
    let (head, tail) = fields.split_at(cut);
    let mut row: Vec<String> = head.iter().map(|s| s.to_string()).collect();
    row.push(tail.join(" "));
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &[u8] = include_bytes!("testdata/procps/dump");
    const CASES: &str = include_str!("testdata/procps/cases");
    const PIDS: &str = include_str!("testdata/procps/pids");

    /// The dump without what ps was not asked for (the driver's shell).
    fn dump() -> Vec<u8> {
        let pids: Vec<&str> = PIDS.trim().split(',').collect();
        let mut records = DUMP.split(|&b| b == 0x1e);
        let mut out = records.next().unwrap().to_vec();
        for r in records {
            let stat = String::from_utf8_lossy(r.split(|&b| b == 0x1f).next().unwrap()).into_owned();
            if pids.contains(&stat.split(' ').next().unwrap()) {
                out.push(0x1e);
                out.extend_from_slice(r);
            }
        }
        out
    }

    fn out(n: usize) -> String {
        let path = format!(
            "{}/src/daemon/testdata/procps/{n:02}.out",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn ps_prints_what_procps_prints() {
        let dump = dump();
        let mut failed = Vec::new();
        for (i, case) in CASES.lines().enumerate() {
            let args: Vec<&str> = case.split(' ').collect();
            let want = out(i + 1);
            let got = match ps(&dump, &args, 0) {
                Ok(text) => text,
                Err(e) => format!("Err: {e}"),
            };
            if got != want {
                failed.push(format!(
                    "case {} `ps {case}`:\n--- want\n{want}--- got\n{got}",
                    i + 1
                ));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[test]
    fn tables_are_taken_apart_as_dockerd_takes_them() {
        let dump = dump();
        let t = table(&ps(&dump, &["-ef"], 0).unwrap()).unwrap();
        assert_eq!(
            t.titles,
            ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]
        );
        assert_eq!(t.processes.len(), 12);
        assert_eq!(
            t.processes[7],
            [
                "root",
                "204",
                "1",
                "0",
                "06:49",
                "?",
                "00:00:00",
                "sh -c sleep 1007; : an arg with spaces tab.here"
            ]
        );
        // No PID title: dockerd's error.
        let e = table(&ps(&dump, &["-o", "pid=PROCESS,args=WHAT"], 0).unwrap()).unwrap_err();
        assert_eq!(e, "Couldn't find PID field in ps output");
        // A thread line follows its process; an orphan one is dropped.
        let t = table("PID CMD\n-  orphan\n1 a b\n- thread\n").unwrap();
        assert_eq!(t.processes, [vec!["1", "a b"], vec!["-", "thread"]]);
        assert!(table("PID\nx\n").unwrap_err().starts_with("Unexpected pid 'x'"));
    }

    #[test]
    fn failures_read_as_dockerd_reports_them() {
        let dump = dump();
        assert_eq!(
            ps(&dump, &["-Q"], 0).unwrap_err(),
            "ps: error: unsupported SysV option"
        );
        // The second pass fails too ("way bad" at `aux`): the first pass's error.
        assert_eq!(
            ps(&dump, &["aux", "-x"], 0).unwrap_err(),
            "ps: error: must set personality to get -x option"
        );
        assert_eq!(
            ps(&dump, &["-o", "pid=PID"], 0).unwrap().lines().next().unwrap(),
            "  PID"
        );
        assert!(ps(&dump, &["-L"], 0).unwrap_err().contains("(-L)"));
        assert!(
            ps(&dump, &["-o", "pid,rbytes"], 0)
                .unwrap_err()
                .contains("/proc/PID/io")
        );
        // Nothing selected: ps exits 1 with nothing on stderr.
        assert_eq!(ps(&dump, &["-p", "1"], 0).unwrap_err(), "ps: exit status 1");
        assert_eq!(ps(&dump, &["V"], 0).unwrap(), "ps from procps-ng 4.0.2\n");
    }

    #[test]
    fn options_ps_refuses_with_q_run_without_it() {
        let dump = dump();
        // Sorted: the busy process first.
        let sorted = ps(&dump, &["-eo", "pid,pcpu", "--sort=-pcpu"], 0).unwrap();
        assert_eq!(sorted.lines().nth(1).unwrap(), "  203 64.8");
        // Selected by user, which -q refuses to combine with.
        let mine = ps(&dump, &["-u", "averyveryverylongusername", "-o", "pid"], 0).unwrap();
        assert_eq!(mine, "  PID\n  199\n");
        // A forest: the terminal's shell under script.
        let forest = ps(&dump, &["-e", "--forest", "-o", "pid,args"], 0).unwrap();
        assert!(forest.contains("  202 script -qfc sleep 1005 /dev/null\n  209  \\_ sh -c sleep 1005\n  210      \\_ sleep 1005\n"), "{forest}");
    }

    /// The dump as if read `later` seconds after it was.
    fn later(dump: &[u8], later: i64) -> Vec<u8> {
        let head_end = dump.iter().position(|&b| b == 0x1e).unwrap();
        let mut fields: Vec<Vec<u8>> = dump[..head_end]
            .split(|&b| b == 0x1f)
            .map(<[u8]>::to_vec)
            .collect();
        let uptime: f64 = String::from_utf8_lossy(&fields[5]).parse().unwrap();
        fields[5] = format!("{:.2}", uptime + later as f64).into_bytes();
        let now: i64 = String::from_utf8_lossy(&fields[6]).parse().unwrap();
        fields[6] = (now + later).to_string().into_bytes();
        let mut out = fields.join(&0x1f);
        out.extend_from_slice(&dump[head_end..]);
        out
    }

    #[test]
    fn start_times_follow_the_zone_and_now() {
        let dump = dump();
        let line = |dump: &[u8], offset: i32| {
            ps(dump, &["-o", "pid,stime,start,bsdstart,lstart"], offset)
                .unwrap()
                .lines()
                .nth(1)
                .unwrap()
                .to_string()
        };
        assert_eq!(
            line(&dump, 0),
            "  197 06:49 06:49:11  06:49 Mon Oct  5 06:49:11 2026"
        );
        // East of UTC: the next day, started today.
        assert_eq!(
            line(&dump, 18 * 3600),
            "  197 00:49 00:49:11  00:49 Tue Oct  6 00:49:11 2026"
        );
        // Started four seconds before a midnight now just past: yesterday, but within a
        // day.
        assert_eq!(
            line(&dump, 61_845),
            "  197 Oct05 23:59:56  23:59 Mon Oct  5 23:59:56 2026"
        );
        // Two days on, and a year on.
        assert_eq!(
            line(&later(&dump, 2 * 86_400), 0),
            "  197 Oct05   Oct 05 Oct  5 Mon Oct  5 06:49:11 2026"
        );
        assert_eq!(
            line(&later(&dump, 365 * 86_400), 0),
            "  197  2026   Oct 05 Oct  5 Mon Oct  5 06:49:11 2026"
        );
        // ELAPSED across days.
        let e = ps(&later(&dump, 2 * 86_400 + 3600), &["-o", "pid,etime,etimes"], 0).unwrap();
        assert_eq!(e.lines().nth(1).unwrap(), "  197  2-01:00:03  176403");
    }

    #[test]
    fn long_names_are_cut_with_a_plus() {
        let dump = dump();
        let t = ps(&dump, &["-o", "pid,user:4,user:1,user"], 0).unwrap();
        assert_eq!(
            t.lines().nth(3).unwrap(),
            "  199 ave+ + averyveryverylongusername"
        );
    }
}
