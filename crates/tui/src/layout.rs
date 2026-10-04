//! Rows that fit their width by changing shape, not by being cut: each part of a row
//! has forms from its richest to its most compact (an empty form leaves it out), and the
//! parts that matter least give way first, as the site's components collapse at narrow
//! widths (a post row drops its end text; a header stacks) rather than clip. One part may
//! be elastic, a bar: it takes what width is left, between its least and its most.

/// A part of a row: its forms, richest first, each `(cells wide, text)`; and how much it
/// matters (higher stays longer).
#[derive(Debug, Clone)]
pub struct Part {
    pub forms: Vec<(usize, String)>,
    pub keep: u8,
}

impl Part {
    /// A part whose forms are plain text, measured by characters.
    pub fn text(keep: u8, forms: &[&str]) -> Part {
        Part {
            forms: forms
                .iter()
                .map(|f| (f.chars().count(), (*f).to_string()))
                .collect(),
            keep,
        }
    }
}

/// What a row became: which form each part took, and the elastic part's width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fit {
    pub chosen: Vec<usize>,
    pub elastic: usize,
}

/// Fits `parts`, separated by `gap` cells, into `width`, with an elastic part of
/// `elastic.0` to `elastic.1` cells (`(0, 0)` for none). Every part starts at its
/// richest form; while the row is too wide the elastic part narrows to its least, then
/// the part that matters least (the later of equals) takes its next form. A row that
/// still does not fit keeps its most compact forms: the caller clips that last resort.
pub fn fit(parts: &[Part], gap: usize, width: usize, elastic: (usize, usize)) -> Fit {
    let mut chosen = vec![0usize; parts.len()];
    let used = |chosen: &[usize]| -> usize {
        let mut shown = 0;
        let mut cells = 0;
        for (p, &c) in parts.iter().zip(chosen) {
            let w = p.forms.get(c).map_or(0, |f| f.0);
            if w > 0 {
                cells += w;
                shown += 1;
            }
        }
        let pieces = shown + usize::from(elastic.1 > 0);
        cells + gap * pieces.saturating_sub(1)
    };
    loop {
        let fixed = used(&chosen);
        if fixed + elastic.0 <= width {
            let room = width - fixed;
            return Fit {
                chosen,
                elastic: room.min(elastic.1),
            };
        }
        // The least kept part that has a smaller form left.
        let next = parts
            .iter()
            .enumerate()
            .filter(|(i, p)| chosen.get(*i).is_some_and(|&c| c + 1 < p.forms.len()))
            .min_by_key(|(i, p)| (p.keep, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match next.and_then(|i| chosen.get_mut(i)) {
            Some(c) => *c += 1,
            None => {
                return Fit {
                    chosen,
                    elastic: elastic.0.min(width.saturating_sub(fixed)),
                };
            }
        }
    }
}

/// A reference as short as it can say itself: Docker Hub's library and domain left out,
/// as `docker` shows familiar names.
pub fn familiar(reference: &str) -> &str {
    reference
        .strip_prefix("docker.io/library/")
        .or_else(|| reference.strip_prefix("docker.io/"))
        .unwrap_or(reference)
}

/// `s` in at most `width` characters, its end replaced by `…` if it must be cut: the last
/// resort.
pub fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        out.push('…');
    }
    out
}

/// `text` wrapped at word boundaries into lines of at most `width` characters; a word
/// longer than a line is broken.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let mut word = word.to_string();
            loop {
                let have = line.chars().count();
                let need = word.chars().count() + usize::from(have > 0);
                if have + need <= width {
                    if have > 0 {
                        line.push(' ');
                    }
                    line.push_str(&word);
                    break;
                }
                if have > 0 {
                    lines.push(std::mem::take(&mut line));
                    continue;
                }
                // A word longer than a line: as much as fits, the rest on the next.
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                lines.push(head);
                if word.is_empty() {
                    break;
                }
            }
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn layer() -> Vec<Part> {
        vec![
            Part::text(9, &["●"]),
            Part::text(3, &["8f2c5a1b9d3e", "8f2c5a", ""]),
            Part::text(5, &["12.3 MB / 48.1 MB", "12/48M", ""]),
            Part::text(2, &["verified", "✓", ""]),
        ]
    }

    #[test]
    fn a_wide_row_keeps_everything_and_its_bar_grows() {
        let f = fit(&layer(), 2, 120, (10, 40));
        assert_eq!(f.chosen, vec![0, 0, 0, 0]);
        assert_eq!(f.elastic, 40);
    }

    #[test]
    fn a_narrowing_row_gives_way_least_first() {
        // The bar narrows first.
        let f = fit(&layer(), 2, 60, (10, 40));
        assert_eq!(f.chosen, vec![0, 0, 0, 0]);
        assert!(f.elastic < 40 && f.elastic >= 10);
        // Then the status, the least kept, shortens; then the id; the size goes last.
        // 1 + 12 + 17 + 1, four gaps of 2, and the bar's least, 10: 49 cells.
        let f = fit(&layer(), 2, 49, (10, 40));
        assert_eq!(f.chosen, vec![0, 0, 0, 1], "{f:?}");
        let f = fit(&layer(), 2, 48, (10, 40));
        assert_eq!(f.chosen, vec![0, 0, 0, 2], "{f:?}");
        let f = fit(&layer(), 2, 30, (10, 40));
        assert_eq!(f.chosen[0], 0, "the dot stays");
        assert!(f.chosen[3] >= 1 && f.chosen[1] >= 1, "{f:?}");
        let f = fit(&layer(), 2, 14, (10, 40));
        assert_eq!(f.chosen, vec![0, 2, 2, 2], "{f:?}");
    }

    #[test]
    fn every_width_is_filled_and_never_overrun_while_it_can_be() {
        for width in 24..160 {
            let parts = layer();
            let f = fit(&parts, 2, width, (10, 40));
            let mut cells = f.elastic;
            let mut shown = usize::from(f.elastic > 0);
            for (p, &c) in parts.iter().zip(&f.chosen) {
                if p.forms[c].0 > 0 {
                    cells += p.forms[c].0;
                    shown += 1;
                }
            }
            cells += 2 * shown.saturating_sub(1);
            assert!(cells <= width, "{width}: {cells}");
        }
    }

    #[test]
    fn names_shorten_by_meaning_before_they_are_cut() {
        assert_eq!(familiar("docker.io/library/python:3.13"), "python:3.13");
        assert_eq!(familiar("docker.io/rocm/pytorch:latest"), "rocm/pytorch:latest");
        assert_eq!(familiar("ghcr.io/a/b:1"), "ghcr.io/a/b:1");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("ab", 4), "ab");
    }

    #[test]
    fn text_wraps_at_words() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }
}
