//! What this client's terminal is, for a command it gives a pty (`Run::terms`): the
//! terminal types it says it is, best first, of which the command is told the first its
//! microVM's root describes where its environment names no TERM.

#[cfg(unix)]
use std::path::{Path, PathBuf};

/// `colors` in a compiled terminfo entry: the 14th of its numbers (term(5); ncurses
/// include/Caps: columns, init_tabs, lines, lines_of_memory, magic_cookie_glitch,
/// padding_baud_rate, virtual_terminal, width_status_line, num_labels, label_height,
/// label_width, max_attributes, maximum_windows, max_colors).
#[cfg(unix)]
const MAX_COLORS: usize = 13;

/// The terminal types this client's terminal is, best first: TERM's own, and then
/// xterm-256color where TERM's description on this host says 256 colours or more, or
/// COLORTERM says 24-bit colour, as a terminal of that many speaks xterm-256color's (one
/// such terminal's type most images describe, where they describe few of theirs: PM M263).
/// None but names a terminfo entry may be filed under (`shards_abi::run::terminal_name`).
pub fn terms() -> Vec<String> {
    let term = std::env::var("TERM")
        .ok()
        .filter(|t| shards_abi::run::terminal_name(t.as_bytes()));
    let truecolor = std::env::var("COLORTERM").is_ok_and(|c| c == "truecolor" || c == "24bit");
    let many = truecolor || term.as_deref().and_then(colors).is_some_and(|c| c >= 256);
    let mut terms: Vec<String> = term.iter().cloned().collect();
    if many && term.as_deref() != Some("xterm-256color") {
        terms.push("xterm-256color".into());
    }
    terms
}

/// The colours `term`'s description on this host says it has, where this host describes
/// it: its entry where ncurses looks (terminfo(5), "Fetching Compiled Descriptions").
#[cfg(unix)]
fn colors(term: &str) -> Option<i32> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dirs = search(
        std::env::var_os("TERMINFO").map(PathBuf::from),
        home.as_deref(),
        std::env::var("TERMINFO_DIRS").ok().as_deref(),
    );
    dirs.iter()
        .find_map(|dir| entry(dir, term).and_then(|bytes| max_colors(&bytes)))
}

/// None on Windows, which keeps no terminfo database: its console's type is said by the
/// client of a context (context.rs).
#[cfg(not(unix))]
fn colors(_: &str) -> Option<i32> {
    None
}

/// Where ncurses looks for a description, in order: TERMINFO, `~/.terminfo`, each of
/// TERMINFO_DIRS (an empty one the system's), then the system's: /etc/terminfo,
/// /lib/terminfo, /usr/share/terminfo, and /usr/lib/terminfo.
#[cfg(unix)]
fn search(terminfo: Option<PathBuf>, home: Option<&Path>, dirs: Option<&str>) -> Vec<PathBuf> {
    let system = [
        "/etc/terminfo",
        "/lib/terminfo",
        "/usr/share/terminfo",
        "/usr/lib/terminfo",
    ]
    .map(PathBuf::from);
    let mut out: Vec<PathBuf> = terminfo.into_iter().collect();
    out.extend(home.map(|h| h.join(".terminfo")));
    if let Some(dirs) = dirs {
        for d in dirs.split(':') {
            if d.is_empty() {
                out.extend(system.iter().cloned());
            } else {
                out.push(PathBuf::from(d));
            }
        }
    }
    out.extend(system);
    out
}

/// `term`'s compiled entry in `dir`: filed under its first character, or that character's
/// hex code where a filesystem's names ignore case (term(5), "Storage").
#[cfg(unix)]
fn entry(dir: &Path, term: &str) -> Option<Vec<u8>> {
    let first = *term.as_bytes().first()?;
    [char::from(first).to_string(), format!("{first:02x}")]
        .iter()
        .find_map(|sub| std::fs::read(dir.join(sub).join(term)).ok())
}

/// `colors` of a compiled terminfo entry (term(5)): six little-endian shorts, the magic
/// (0432, numbers of 16 bits; 01036, of 32), the names' bytes, the booleans' count and the
/// numbers'; then the names and booleans, a byte more where they end on an odd one, and the
/// numbers. None where the entry has none, or is no entry.
#[cfg(unix)]
fn max_colors(entry: &[u8]) -> Option<i32> {
    let short = |at: usize| -> Option<i16> {
        let bytes: [u8; 2] = entry.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(i16::from_le_bytes(bytes))
    };
    let wide = match short(0)? {
        0o432 => false,
        0o1036 => true,
        _ => return None,
    };
    let count = |at: usize| short(at).and_then(|n| usize::try_from(n).ok());
    let (names, bools, numbers) = (count(2)?, count(4)?, count(6)?);
    if numbers <= MAX_COLORS {
        return None;
    }
    let mut at = 12usize.checked_add(names)?.checked_add(bools)?;
    at = at.checked_add(at % 2)?;
    let width = if wide { 4 } else { 2 };
    let field = at.checked_add(MAX_COLORS.checked_mul(width)?)?;
    let value = if wide {
        let bytes: [u8; 4] = entry.get(field..field.checked_add(4)?)?.try_into().ok()?;
        i32::from_le_bytes(bytes)
    } else {
        i32::from(short(field)?)
    };
    (value >= 0).then_some(value)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A compiled entry of `names` bytes of names and `bools` booleans, its numbers each
    /// `width` bytes, `colors` the 14th of 15 (-1 absent).
    fn compiled(names: usize, bools: usize, wide: bool, colors: i32) -> Vec<u8> {
        let magic: i16 = if wide { 0o1036 } else { 0o432 };
        let mut e = Vec::new();
        for v in [magic, names as i16, bools as i16, 15, 0, 0] {
            e.extend(v.to_le_bytes());
        }
        e.extend(vec![b'n'; names]);
        e.extend(vec![1u8; bools]);
        if (names + bools) % 2 == 1 {
            e.push(0);
        }
        for i in 0..15 {
            let v = if i == MAX_COLORS { colors } else { 80 };
            if wide {
                e.extend(v.to_le_bytes());
            } else {
                e.extend((v as i16).to_le_bytes());
            }
        }
        e
    }

    /// An entry's colours are read in either format, past an odd end of its booleans and
    /// an even one; an entry without them, or no entry, has none.
    #[test]
    fn an_entrys_colours_are_read_as_ncurses_writes_them() {
        for (names, bools) in [(10, 3), (10, 4), (1, 0)] {
            assert_eq!(max_colors(&compiled(names, bools, false, 256)), Some(256));
            assert_eq!(
                max_colors(&compiled(names, bools, true, 16_777_216)),
                Some(16_777_216)
            );
            assert_eq!(max_colors(&compiled(names, bools, false, -1)), None);
        }
        let whole = compiled(10, 3, false, 8);
        assert_eq!(max_colors(&whole[..whole.len() - 4]), None, "cut short");
        assert_eq!(max_colors(b"#!/bin/sh\n"), None, "no entry");
        let mut few = compiled(10, 3, false, 8);
        few[6] = 13;
        assert_eq!(max_colors(&few), None, "13 numbers, colors not among them");
    }

    /// Where this host describes them, xterm has 8 colours and xterm-256color 256, read from
    /// its own entries.
    #[test]
    fn this_hosts_xterm_entries_say_their_colours() {
        let dirs = search(None, None, None);
        let found = |term: &str| dirs.iter().find_map(|d| entry(d, term));
        match (found("xterm"), found("xterm-256color")) {
            (Some(xterm), Some(many)) => {
                assert_eq!(max_colors(&xterm), Some(8));
                assert_eq!(max_colors(&many), Some(256));
            }
            _ => eprintln!("SKIP: this host describes no xterm"),
        }
    }

    /// ncurses' order: TERMINFO, ~/.terminfo, TERMINFO_DIRS (an empty one the system's),
    /// then the system's.
    #[test]
    fn descriptions_are_looked_for_where_ncurses_looks() {
        let dirs = search(
            Some(PathBuf::from("/t")),
            Some(Path::new("/home/u")),
            Some("/a::/b"),
        );
        let shown: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
        assert_eq!(
            shown,
            [
                "/t",
                "/home/u/.terminfo",
                "/a",
                "/etc/terminfo",
                "/lib/terminfo",
                "/usr/share/terminfo",
                "/usr/lib/terminfo",
                "/b",
                "/etc/terminfo",
                "/lib/terminfo",
                "/usr/share/terminfo",
                "/usr/lib/terminfo",
            ]
        );
        assert_eq!(search(None, None, None).len(), 4);
    }
}
