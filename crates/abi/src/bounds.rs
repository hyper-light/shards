//! Bounds the VMM and its clients hold to, each derived from what it bounds, never chosen
//! (D12): pure functions of the readings and records their callers pass, so every process
//! derives a bound the same way from its own readings.

/// How many vsock connections a process may hold, each holding `per` of its descriptors
/// (the VMM's muxer one, its host socket; the simulator two, a pair's ends): what its
/// descriptor limit `limit` leaves past the `held` it has open otherwise. None where it holds
/// its limit or more; any number where each holds none.
pub const fn connections(limit: u64, held: u64, per: u64) -> usize {
    let free = limit.saturating_sub(held);
    let n = match free.checked_div(per) {
        Some(n) => n,
        None => u64::MAX,
    };
    if n > usize::MAX as u64 {
        usize::MAX
    } else {
        n as usize
    }
}

/// The buffer a kernel's panic formats its message into, `vpanic`'s `static char buf[1024]`
/// (Linux 6.18.48, shards' pinned kernel, kernel/panic.c l. 431).
pub const PANIC_MESSAGE: usize = 1024;

/// What a kernel's panic writes its message in: the two lines that carry the buffer,
/// `Kernel panic - not syncing: %s` and `---[ end Kernel panic - not syncing: %s ]---`
/// (kernel/panic.c l. 483 and 596). What a VM keeps of a console no one reads where nothing
/// has been learned yet ([`console_said`]), and the least a report that ran out asks next.
pub const PANIC_FRAME: usize = 2 * PANIC_MESSAGE;

/// How one failure report a VM quoted from the console no one read came out: whole, the
/// console's `N` bytes all kept; or run out, its first bytes lost past the `N` kept. Its
/// record is one line, `report N` or `cut N` ([`ConsoleReport::line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleReport {
    Whole(usize),
    Cut(usize),
}

impl ConsoleReport {
    /// Its record: one whole line, appended at once, so that writers beside each other never
    /// interleave theirs.
    pub fn line(self) -> alloc::string::String {
        match self {
            ConsoleReport::Whole(n) => alloc::format!("report {n}\n"),
            ConsoleReport::Cut(n) => alloc::format!("cut {n}\n"),
        }
    }

    /// The record `line` is, without its newline; none where it is no record.
    pub fn parse(line: &[u8]) -> Option<ConsoleReport> {
        let line = core::str::from_utf8(line).ok()?;
        let (word, n) = line.split_once(' ')?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n = n.parse().ok()?;
        match word {
            "report" => Some(ConsoleReport::Whole(n)),
            "cut" => Some(ConsoleReport::Cut(n)),
            _ => None,
        }
    }
}

/// What a kernel's records say of its failure reports: the longest whole, and the most kept
/// of one that ran out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsoleRecords {
    whole: Option<usize>,
    cut: Option<usize>,
}

impl ConsoleRecords {
    /// `records`' whole lines: a last line with no newline is one a writer has not finished,
    /// and a line that is no record (two writers' torn together) is passed over.
    pub fn read(records: &[u8]) -> ConsoleRecords {
        let finished = match records.iter().rposition(|&b| b == b'\n') {
            Some(at) => records.get(..at).unwrap_or_default(),
            None => return ConsoleRecords::default(),
        };
        finished
            .split(|&b| b == b'\n')
            .filter_map(ConsoleReport::parse)
            .fold(ConsoleRecords::default(), ConsoleRecords::with)
    }

    /// These records and `report`.
    pub fn with(self, report: ConsoleReport) -> ConsoleRecords {
        let most = |a: Option<usize>, b: usize| Some(a.map_or(b, |a| a.max(b)));
        match report {
            ConsoleReport::Whole(n) => ConsoleRecords {
                whole: most(self.whole, n),
                ..self
            },
            ConsoleReport::Cut(n) => ConsoleRecords {
                cut: most(self.cut, n),
                ..self
            },
        }
    }

    /// These records and `other`'s: another kernel's, for what every kernel's say.
    pub fn and(self, other: ConsoleRecords) -> ConsoleRecords {
        let most = |a: Option<usize>, b: Option<usize>| match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        ConsoleRecords {
            whole: most(self.whole, other.whole),
            cut: most(self.cut, other.cut),
        }
    }

    /// What they say a report needs kept: its longest whole one, and twice the most kept of
    /// one that ran out, which needed more, and at least a panic's frame ([`PANIC_FRAME`]),
    /// the report it is kept for: a run-out at nothing kept says nothing of its length. None
    /// where there are none.
    pub fn needs(self) -> Option<usize> {
        let cut = self.cut.map(|n| n.saturating_mul(2).max(PANIC_FRAME));
        match (self.whole, cut) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// How many of its console's last bytes a VM whose console no one reads keeps, to quote as
/// it fails (D12): what its kernel's records need ([`ConsoleRecords::needs`]); with none of
/// its own, what every kernel's need; with none at all, a panic's frame ([`PANIC_FRAME`]).
pub fn console_said(kernel: ConsoleRecords, every: ConsoleRecords) -> usize {
    kernel.needs().or(every.needs()).unwrap_or(PANIC_FRAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process's connections are what its descriptor limit leaves past the descriptors it
    /// holds, shared out by those each connection holds; none where it holds its limit or
    /// more, and any number where each holds none.
    #[test]
    fn connections_are_what_the_limit_leaves() {
        assert_eq!(connections(1024, 24, 1), 1000);
        assert_eq!(connections(1024, 24, 2), 500);
        assert_eq!(connections(1024, 25, 2), 499);
        assert_eq!(connections(1024, 1024, 1), 0);
        assert_eq!(connections(1024, 2048, 1), 0);
        assert_eq!(connections(1024, 24, 0), usize::MAX);
    }

    /// The kept size is learned from a kernel's records: its longest whole report, twice
    /// what ran out (at least a panic's frame); else every kernel's; else a panic's frame,
    /// two lines of `vpanic`'s 1024-byte buffer.
    #[test]
    fn a_consoles_kept_bytes_are_what_its_records_need() {
        let none = ConsoleRecords::default();
        assert_eq!(PANIC_FRAME, 2048);
        assert_eq!(console_said(none, none), PANIC_FRAME);
        let small = ConsoleRecords::read(b"report 1129\nreport 858\n");
        assert_eq!(console_said(small, small), 1129);
        // Below a panic's frame where a whole report says so; never below it once one ran out.
        let ran_out = small.with(ConsoleReport::Cut(0));
        assert_eq!(console_said(ran_out, ran_out), PANIC_FRAME);
        let ran_out = small.with(ConsoleReport::Cut(4096));
        assert_eq!(console_said(ran_out, ran_out), 8192);
        assert_eq!(console_said(ran_out.with(ConsoleReport::Whole(9000)), none), 9000);
        // Another kernel's records speak only for one with none of its own.
        let other = ConsoleRecords::read(b"report 300\n");
        assert_eq!(console_said(none, other.and(none)), 300);
        assert_eq!(console_said(small, other.and(small)), 1129);
        assert_eq!(console_said(none, other.and(ran_out)), 8192);
        let huge = ConsoleRecords::read(b"cut 18446744073709551615\n");
        assert_eq!(console_said(huge, none), usize::MAX);
    }

    /// Records are whole lines: an unfinished last line, and one two writers tore together,
    /// say nothing; each record reads back as it was written.
    #[test]
    fn console_records_are_whole_lines() {
        for r in [
            ConsoleReport::Whole(0),
            ConsoleReport::Whole(1129),
            ConsoleReport::Cut(2048),
        ] {
            let line = r.line();
            assert!(line.ends_with('\n'));
            assert_eq!(ConsoleReport::parse(line.trim_end().as_bytes()), Some(r));
            assert_eq!(
                ConsoleRecords::read(line.as_bytes()),
                ConsoleRecords::default().with(r)
            );
        }
        assert_eq!(ConsoleRecords::read(b"report 5000"), ConsoleRecords::default());
        assert_eq!(
            ConsoleRecords::read(b"report 10\nreport 5000"),
            ConsoleRecords::read(b"report 10\n")
        );
        assert_eq!(
            ConsoleRecords::read(b"cut 4report 9999\nreport 10\n"),
            ConsoleRecords::read(b"report 10\n")
        );
        for torn in [
            &b"report"[..],
            b"report ",
            b"report -1",
            b"report +1",
            b"said 5",
            b"cut 1 2",
            b"\xff 1",
        ] {
            assert_eq!(ConsoleReport::parse(torn), None, "{torn:?}");
        }
    }
}
