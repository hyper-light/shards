//! What a guest's console said last (D12): as many of its last bytes as its spawner
//! learned its kernel's failure reports need (`shards_abi::bounds::console_said`), quoted
//! in whole lines as it fails.

use std::collections::VecDeque;

pub use shards_abi::bounds::ConsoleReport;

/// What a console said last ([`Kept::said`]): its whole lines, and whether they are all it
/// said or its first were lost past what it kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    pub lines: Vec<u8>,
    pub report: ConsoleReport,
}

/// A console's last bytes, at most `keep` of them.
#[derive(Debug)]
pub struct Kept {
    said: VecDeque<u8>,
    keep: usize,
    /// Whether bytes said were lost past `keep`, and whether the last lost ended a line,
    /// so that the first kept is a line's first.
    lost: Option<bool>,
}

impl Kept {
    /// Keeps the last `keep` bytes said; none where none is quoted.
    pub fn new(keep: usize) -> Kept {
        Kept {
            said: VecDeque::new(),
            keep,
            lost: None,
        }
    }

    /// Adds what the console said.
    pub fn push(&mut self, bytes: &[u8]) {
        for &v in bytes {
            if self.said.len() >= self.keep {
                let lost = self.said.pop_front().unwrap_or(v);
                self.lost = Some(lost == b'\n');
            }
            if self.keep > 0 {
                self.said.push_back(v);
            }
        }
    }

    /// The whole lines of the last bytes kept: what the console said last, as a kernel
    /// writes why it panics there. A report whole where it kept every byte said; else cut
    /// at what it keeps, a line it lost the start of dropped.
    pub fn said(&self) -> Said {
        let mut lines: Vec<u8> = self.said.iter().copied().collect();
        let report = match self.lost {
            None => ConsoleReport::Whole(lines.len()),
            Some(at_line_end) => {
                if !at_line_end {
                    let cut = lines
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(lines.len(), |at| at.saturating_add(1));
                    lines.drain(..cut);
                }
                ConsoleReport::Cut(self.keep)
            }
        };
        Said { lines, report }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the console said last is kept, as many of its last bytes as it was asked to
    /// keep, in whole lines. Whether its first bytes were lost is said with it: a report
    /// whole, of every byte said, or cut at what was kept.
    #[test]
    fn the_last_bytes_said_are_kept_in_whole_lines() {
        let mut k = Kept::new(10);
        k.push(b"one\ntwo\n");
        assert_eq!(
            k.said(),
            Said {
                lines: b"one\ntwo\n".to_vec(),
                report: ConsoleReport::Whole(8)
            }
        );
        k.push(b"three\n");
        // "one\n" lost, a whole line: what is kept starts a line.
        assert_eq!(
            k.said(),
            Said {
                lines: b"two\nthree\n".to_vec(),
                report: ConsoleReport::Cut(10)
            }
        );
        // "ab" lost, the start of a line: the rest of that line is dropped.
        let mut mid = Kept::new(6);
        mid.push(b"ab\nfour\n");
        assert_eq!(mid.said().lines, b"four\n");
        let mut none = Kept::new(0);
        none.push(b"x\n");
        assert_eq!(
            none.said(),
            Said {
                lines: Vec::new(),
                report: ConsoleReport::Cut(0)
            }
        );
    }
}
