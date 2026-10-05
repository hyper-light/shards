//! Drawing a display over its last frame, in place, writing only what changed.
//!
//! A frame is a list of lines. Drawn at the same size as the last, only the lines that
//! differ are written: the cursor goes up to the frame's top, steps over each run of
//! unchanged lines in one move, and rewrites the rest; a frame that changed nothing
//! writes nothing. When the size or the number of lines changed, the frame is drawn
//! whole: back up over the rows the last one takes at the terminal's width now (a
//! terminal narrowed under it wrapped its lines onto more rows), everything below
//! cleared, every line written.

use std::io::Write;

/// What a frame's write begins and ends with: synchronized output on, the cursor hidden;
/// then both undone.
const BEGIN: &str = "\x1b[?2026h\x1b[?25l";
const END: &str = "\x1b[?25h\x1b[?2026l";

#[derive(Debug, Default)]
pub struct Frame {
    /// The last frame's lines, as written, and each one's visible width.
    drawn: Vec<(String, usize)>,
    /// The columns the last frame was drawn at.
    cols: usize,
    /// The frame being made; its strings kept between frames.
    next: Vec<(String, usize)>,
    buf: String,
    /// Lines made so far this frame.
    made: usize,
}

impl Frame {
    pub fn new() -> Frame {
        Frame::default()
    }

    /// Begins a frame.
    pub fn begin(&mut self) {
        self.made = 0;
    }

    /// The frame's next line: its text, with its colours, and its visible width. The
    /// line's storage is kept from frame to frame.
    pub fn put(&mut self, text: &str, width: usize) {
        match self.next.get_mut(self.made) {
            Some(slot) => {
                slot.0.clear();
                slot.0.push_str(text);
                slot.1 = width;
            }
            None => self.next.push((text.to_string(), width)),
        }
        self.made += 1;
    }

    /// Writes what changed since the last frame, for a terminal `cols` wide, to `out`;
    /// returns the bytes written (none when nothing changed).
    pub fn end(&mut self, cols: usize, out: &mut impl Write) -> usize {
        let cols = cols.max(1);
        self.next.truncate(self.made);
        self.buf.clear();
        let whole = cols != self.cols || self.next.len() != self.drawn.len();
        if whole {
            let rows: usize = self
                .drawn
                .iter()
                .map(|(_, w)| w.div_ceil(self.cols.max(1)).max(1))
                .sum();
            // At the width now: lines the terminal rewrapped take more rows.
            let rows_now: usize = self.drawn.iter().map(|(_, w)| w.div_ceil(cols).max(1)).sum();
            let up = rows.max(rows_now);
            if up > 0 {
                self.buf.push_str(&format!("\r\x1b[{up}A"));
            }
            self.buf.push_str("\x1b[J");
            for (text, _) in &self.next {
                self.buf.push_str(text);
                self.buf.push_str("\x1b[0m\n");
            }
        } else {
            let first = self.next.iter().zip(&self.drawn).position(|(a, b)| a.0 != b.0);
            if let Some(first) = first {
                let up = self.drawn.len() - first;
                self.buf.push_str(&format!("\r\x1b[{up}A"));
                let mut skip = 0;
                for (a, b) in self.next.iter().zip(&self.drawn).skip(first) {
                    if a.0 == b.0 {
                        skip += 1;
                        continue;
                    }
                    if skip > 0 {
                        self.buf.push_str(&format!("\x1b[{skip}B"));
                        skip = 0;
                    }
                    self.buf.push_str("\r\x1b[2K");
                    self.buf.push_str(&a.0);
                    self.buf.push_str("\x1b[0m\n");
                }
                if skip > 0 {
                    self.buf.push_str(&format!("\x1b[{skip}B"));
                }
                self.buf.push('\r');
            }
        }
        let written = self.buf.len();
        if written > 0 {
            // One update: the terminal shows the frame whole (synchronized output, DEC
            // mode 2026, which terminals without it ignore), and the cursor, moved over
            // the lines redrawn, is hidden while it moves, so that it does not flash
            // across them; shown again in the same write, so nothing is left hidden.
            let _ = out.write_all(BEGIN.as_bytes());
            let _ = out.write_all(self.buf.as_bytes());
            let _ = out.write_all(END.as_bytes());
            let _ = out.flush();
        }
        std::mem::swap(&mut self.drawn, &mut self.next);
        self.cols = cols;
        written
    }

    /// What follows is below the frame: nothing draws over it again.
    pub fn leave(&mut self) {
        self.drawn.clear();
        self.next.clear();
        self.cols = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn frame(f: &mut Frame, cols: usize, lines: &[&str]) -> String {
        f.begin();
        for l in lines {
            f.put(l, l.chars().count());
        }
        let mut out = Vec::new();
        f.end(cols, &mut out);
        let out = String::from_utf8(out).unwrap();
        if out.is_empty() {
            return out;
        }
        // Each write is one update, the cursor hidden through it.
        out.strip_prefix(BEGIN)
            .and_then(|o| o.strip_suffix(END))
            .unwrap_or_else(|| panic!("{out:?}"))
            .to_string()
    }

    #[test]
    fn the_first_frame_is_drawn_whole() {
        let mut f = Frame::new();
        let out = frame(&mut f, 80, &["a", "b"]);
        assert_eq!(out, "\x1b[Ja\x1b[0m\nb\x1b[0m\n");
    }

    #[test]
    fn an_unchanged_frame_writes_nothing() {
        let mut f = Frame::new();
        frame(&mut f, 80, &["a", "b", "c"]);
        assert_eq!(frame(&mut f, 80, &["a", "b", "c"]), "");
    }

    #[test]
    fn only_the_changed_lines_are_written() {
        let mut f = Frame::new();
        frame(&mut f, 80, &["head", "one", "two", "three"]);
        let out = frame(&mut f, 80, &["head", "one", "TWO", "three"]);
        // Up to "two", rewrite it, step over "three".
        assert_eq!(out, "\r\x1b[2A\r\x1b[2KTWO\x1b[0m\n\x1b[1B\r");
        assert!(!out.contains("head") && !out.contains("one") && !out.contains("three"));
        let out = frame(&mut f, 80, &["HEAD", "one", "TWO", "THREE"]);
        assert_eq!(
            out,
            "\r\x1b[4A\r\x1b[2KHEAD\x1b[0m\n\x1b[2B\r\x1b[2KTHREE\x1b[0m\n\r"
        );
    }

    #[test]
    fn a_resize_or_a_new_line_count_draws_it_whole() {
        let mut f = Frame::new();
        frame(&mut f, 80, &[&"x".repeat(60), "y"]);
        // Narrowed to 40, the 60-wide line wraps onto 2 rows: back over 3.
        let out = frame(&mut f, 40, &["x", "y"]);
        assert!(out.starts_with("\r\x1b[3A\x1b[J"), "{out:?}");
        let out = frame(&mut f, 40, &["x", "y", "z"]);
        assert!(out.starts_with("\r\x1b[2A\x1b[J"), "{out:?}");
        f.leave();
        assert_eq!(frame(&mut f, 40, &["x"]), "\x1b[Jx\x1b[0m\n");
    }
}
