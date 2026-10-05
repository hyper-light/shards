//! docker/cli's fork of Go's text/tabwriter (cli/command/formatter/tabwriter/tabwriter.go),
//! which measures cells with go-runewidth's StringWidth instead of counting runes, as the
//! formatter's tables use it: padded with spaces, no flags.
//!
//! The fork's escapes (0xff, and HTML with FilterHTML) never apply: 0xff is not in UTF-8
//! text, and the formatter sets no flags. Its recursive `format` walks the columns here
//! with a stack of its own, so a row of many cells cannot run out of stack.

use crate::width::string_width;

#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    /// Bytes.
    size: usize,
    /// Columns, as go-runewidth counts them.
    width: usize,
    /// Ended by a tab.
    htab: bool,
}

/// A block of lines being aligned, as one call of tabwriter.go's `format` holds it.
#[derive(Debug, Clone, Copy)]
struct Frame {
    line0: usize,
    line1: usize,
    this: usize,
}

#[derive(Debug)]
pub(super) struct Writer {
    minwidth: usize,
    padding: usize,
    east_asian: bool,
    buf: String,
    pos: usize,
    cell: Cell,
    lines: Vec<Vec<Cell>>,
    widths: Vec<usize>,
}

impl Writer {
    /// `tabwriter.NewWriter(out, minwidth, tabwidth, padding, ' ', 0)`; a tab width only
    /// matters for padding with tabs.
    pub(super) fn new(minwidth: usize, padding: usize, east_asian: bool) -> Writer {
        Writer {
            minwidth,
            padding,
            east_asian,
            buf: String::new(),
            pos: 0,
            cell: Cell::default(),
            lines: vec![Vec::new()],
            widths: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.cell = Cell::default();
        self.lines.clear();
        self.lines.push(Vec::new());
        self.widths.clear();
    }

    fn append(&mut self, text: &str) {
        self.buf.push_str(text);
        self.cell.size += text.len();
    }

    fn update_width(&mut self) {
        let fresh = self.buf.get(self.pos..).unwrap_or_default();
        self.cell.width += string_width(fresh, self.east_asian);
        self.pos = self.buf.len();
    }

    /// Ends the cell; the number of cells now on the line.
    fn terminate_cell(&mut self, htab: bool) -> usize {
        self.cell.htab = htab;
        let cell = std::mem::take(&mut self.cell);
        match self.lines.last_mut() {
            Some(line) => {
                line.push(cell);
                line.len()
            }
            None => {
                self.lines.push(vec![cell]);
                1
            }
        }
    }

    /// Writer.Write: text is taken into cells, and lines out of a block flushed to `out`.
    pub(super) fn write(&mut self, text: &str, out: &mut String) {
        let mut n = 0;
        for (i, ch) in text.bytes().enumerate() {
            if matches!(ch, b'\t' | b'\x0b' | b'\n' | b'\x0c') {
                self.append(text.get(n..i).unwrap_or_default());
                self.update_width();
                n = i + 1;
                let ncells = self.terminate_cell(ch == b'\t');
                if ch == b'\n' || ch == b'\x0c' {
                    self.lines.push(Vec::new());
                    if ch == b'\x0c' || ncells == 1 {
                        // A line of one cell ends the block: nothing after it aligns
                        // with what came before.
                        self.flush_lines(out);
                    }
                }
            }
        }
        self.append(text.get(n..).unwrap_or_default());
    }

    /// Writer.Flush.
    pub(super) fn flush(&mut self, out: &mut String) {
        self.flush_lines(out);
    }

    fn flush_lines(&mut self, out: &mut String) {
        if self.cell.size > 0 {
            self.terminate_cell(false);
        }
        self.format(out);
        self.reset();
    }

    fn padding_to(out: &mut String, textw: usize, cellw: usize) {
        out.extend(std::iter::repeat_n(' ', cellw.saturating_sub(textw)));
    }

    fn write_lines(&self, mut pos: usize, line0: usize, line1: usize, out: &mut String) -> usize {
        for i in line0..line1 {
            let Some(line) = self.lines.get(i) else {
                break;
            };
            for (j, c) in line.iter().enumerate() {
                if c.size > 0 {
                    out.push_str(self.buf.get(pos..pos + c.size).unwrap_or_default());
                    pos += c.size;
                }
                if let Some(&w) = self.widths.get(j) {
                    Self::padding_to(out, c.width, w);
                }
            }
            if i + 1 == self.lines.len() {
                // The last line's unfinished cell.
                out.push_str(self.buf.get(pos..pos + self.cell.size).unwrap_or_default());
                pos += self.cell.size;
            } else {
                out.push('\n');
            }
        }
        pos
    }

    /// Whether the line has a cell in `column` that a tab ends (any but its last).
    fn in_column(&self, line: usize, column: usize) -> bool {
        self.lines.get(line).is_some_and(|l| column + 1 < l.len())
    }

    /// tabwriter.go's format, of all the lines.
    fn format(&mut self, out: &mut String) {
        let mut pos = 0;
        let mut stack = vec![Frame {
            line0: 0,
            line1: self.lines.len(),
            this: 0,
        }];
        while let Some(f) = stack.last().copied() {
            let column = self.widths.len();
            let mut this = f.this;
            while this < f.line1 && !self.in_column(this, column) {
                this += 1;
            }
            if this >= f.line1 {
                pos = self.write_lines(pos, f.line0, f.line1, out);
                stack.pop();
                if let Some(parent) = stack.last_mut() {
                    self.widths.pop();
                    // The caller's loop goes on after the block, and its `this++` skips
                    // the line that ended it, which starts no block.
                    parent.line0 = f.line1;
                    parent.this = f.line1 + 1;
                }
                continue;
            }
            pos = self.write_lines(pos, f.line0, this, out);
            let line0 = this;
            let mut width = self.minwidth;
            while this < f.line1 && self.in_column(this, column) {
                if let Some(c) = self.lines.get(this).and_then(|l| l.get(column)) {
                    width = width.max(c.width + self.padding);
                }
                this += 1;
            }
            if let Some(top) = stack.last_mut() {
                top.line0 = line0;
                top.this = line0;
            }
            self.widths.push(width);
            stack.push(Frame {
                line0,
                line1: this,
                this: line0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> String {
        let mut out = String::new();
        let mut w = Writer::new(10, 3, false);
        w.write(text, &mut out);
        w.flush(&mut out);
        out
    }

    #[test]
    fn cells_align_as_the_cli_aligns_them() {
        assert_eq!(
            table("A\tB\nlonger-than-ten\tx\n"),
            "A                 B\nlonger-than-ten   x\n"
        );
        assert_eq!(table("a\tb\nplain\nc\td\n"), "a         b\nplain\nc         d\n");
        assert_eq!(table("日本\tx\n"), "日本      x\n");
        assert_eq!(table("a\tb\tc\nd\te\n"), "a         b         c\nd         e\n");
        assert_eq!(table("no newline"), "no newline");
    }
}
