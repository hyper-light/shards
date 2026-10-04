//! Errors, in a hairline panel: rounded corners (the site's 7px radius), a rose eyebrow
//! (the prism's rose; the site has no red), the message wrapped to the width there is,
//! and hints after the site's tiny cross.

use crate::layout;
use crate::text;
use crate::tokens::{self, Paint};

/// The panel's lines for `width` columns: each its text with colours, and its visible
/// width. Narrow terminals get a panel without its border, its text still wrapped.
pub fn error(
    paint: &Paint,
    width: usize,
    title: &str,
    message: &str,
    hints: &[&str],
) -> Vec<(String, usize)> {
    let mut lines = Vec::new();
    let boxed = width >= 30;
    let inner = if boxed {
        width.min(84) - 6
    } else {
        width.saturating_sub(2).max(10)
    };
    let heading = format!("{}  {}", text::eyebrow("error"), title);
    let edge = |s: &mut String| {
        if boxed {
            paint.fg(s, tokens::EDGE);
        }
    };
    if boxed {
        let mut s = String::from("  ");
        edge(&mut s);
        s.push('╭');
        s.push_str(&"─".repeat(inner + 2));
        s.push('╮');
        lines.push((s, inner + 6));
    }
    let mut row = |content: &dyn Fn(&mut String), shown: usize| {
        let mut s = String::from("  ");
        if boxed {
            edge(&mut s);
            s.push_str("│ ");
        }
        content(&mut s);
        if boxed {
            s.push_str(&" ".repeat(inner.saturating_sub(shown)));
            edge(&mut s);
            s.push_str(" │");
        }
        lines.push((s, if boxed { inner + 6 } else { shown + 2 }));
    };
    for (i, part) in layout::wrap(&heading, inner).iter().enumerate() {
        let n = part.chars().count();
        row(
            &|s: &mut String| {
                paint.fg(s, tokens::ROSE);
                paint.bold(s, i == 0);
                s.push_str(part);
                paint.bold(s, false);
            },
            n,
        );
    }
    for part in layout::wrap(message, inner) {
        let n = part.chars().count();
        row(
            &|s: &mut String| {
                paint.fg(s, tokens::FOREGROUND);
                s.push_str(&part);
            },
            n,
        );
    }
    for hint in hints {
        for (i, part) in layout::wrap(hint, inner.saturating_sub(2)).iter().enumerate() {
            let n = part.chars().count() + 2;
            row(
                &|s: &mut String| {
                    paint.fg(s, tokens::FAINT);
                    s.push_str(if i == 0 { "+ " } else { "  " });
                    paint.fg(s, tokens::SUBTLE);
                    s.push_str(part);
                },
                n,
            );
        }
    }
    if boxed {
        let mut s = String::from("  ");
        edge(&mut s);
        s.push('╰');
        s.push_str(&"─".repeat(inner + 2));
        s.push('╯');
        lines.push((s, inner + 6));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for ch in s.chars() {
            match (esc, ch) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, ch) => out.push(ch),
            }
        }
        out
    }

    #[test]
    fn a_panel_fits_every_width_and_keeps_every_word() {
        let paint = Paint { truecolor: true };
        let message = "failed to resolve reference \"localhost:5055/x:1\": not found";
        for width in [12usize, 29, 30, 50, 80, 200] {
            let lines = error(
                &paint,
                width,
                "pull",
                message,
                &["check the name", "or the registry"],
            );
            let mut words = String::new();
            for (s, w) in &lines {
                let text = seen(s);
                assert_eq!(text.chars().count(), *w, "{width}: {text:?}");
                assert!(*w <= width.max(12), "{width}: {text:?}");
                words.push_str(&text);
                words.push(' ');
            }
            for word in message.split(' ') {
                // A word longer than a line is broken, so look for its pieces.
                let piece: String = word.chars().take(8).collect();
                assert!(words.contains(&piece), "{width}: {piece} in {words}");
            }
        }
    }
}
